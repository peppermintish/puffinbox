use std::{env, path::PathBuf, sync::Arc, time::Duration};

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::connect_info::ConnectInfo,
    http::{Request, StatusCode},
    response::Response,
};
use puffinbox::{AppState, Config, api, auth, db};
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions, types::Json};
use tower::ServiceExt;
use uuid::Uuid;

mod common;

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via PUFFINBOX_TEST_DATABASE_URL"]
async fn facets_and_selections_share_metadata_and_current_user_boundaries() {
    let database_url = env::var("PUFFINBOX_TEST_DATABASE_URL")
        .expect("set PUFFINBOX_TEST_DATABASE_URL to a disposable PostgreSQL database");
    let admin_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url)
        .await
        .unwrap();
    let schema = format!("puffinbox_filters_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
        .execute(&admin_pool)
        .await
        .unwrap();
    let selected_schema = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(5))
        .after_connect(move |connection, _| {
            let schema = selected_schema.clone();
            Box::pin(async move {
                sqlx::query(&format!("SET search_path TO \"{schema}\""))
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect(&database_url)
        .await
        .unwrap();
    common::apply_migrations(&pool).await.unwrap();
    let run = Uuid::new_v4();
    db::activate_run(&pool, run).await.unwrap();
    let owner = Uuid::new_v4();
    let peer = Uuid::new_v4();
    let admin = Uuid::new_v4();
    for (id, name, administrator) in [
        (owner, "facets-owner", false),
        (peer, "facets-peer", false),
        (admin, "facets-admin", true),
    ] {
        sqlx::query("INSERT INTO users(id,username,username_norm,password_hash,is_admin,enable_remote_access,restrict_libraries,max_parental_rating,enable_live_tv_access) VALUES ($1,$2,$2,'unused-synthetic-hash',$3,TRUE,TRUE,50,FALSE)")
            .bind(id).bind(name).bind(administrator).execute(&pool).await.unwrap();
    }
    let library = Uuid::new_v4();
    let private = Uuid::new_v4();
    let disabled = Uuid::new_v4();
    for (id, name) in [
        (library, "Visible facets"),
        (private, "Private facets"),
        (disabled, "Disabled facets"),
    ] {
        db::insert_library(
            &pool,
            run,
            id,
            name,
            "movies",
            &[PathBuf::from("/media")],
            true,
        )
        .await
        .unwrap();
    }
    sqlx::query("UPDATE libraries SET enabled=FALSE WHERE id=$1")
        .bind(disabled)
        .execute(&pool)
        .await
        .unwrap();
    for (user, id) in [(owner, library), (peer, private)] {
        sqlx::query("INSERT INTO user_library_access(user_id,library_id) VALUES ($1,$2)")
            .bind(user)
            .bind(id)
            .execute(&pool)
            .await
            .unwrap();
    }
    let folder = item(&pool, library, None, "Folder", "/media/films").await;
    let nested = item(
        &pool,
        library,
        Some(folder),
        "Folder",
        "/media/films/nested",
    )
    .await;
    let first = item(
        &pool,
        library,
        Some(folder),
        "Movie",
        "/media/films/first.mkv",
    )
    .await;
    let second = item(
        &pool,
        library,
        Some(folder),
        "Movie",
        "/media/films/second.mkv",
    )
    .await;
    let comedy = item(
        &pool,
        library,
        Some(nested),
        "Movie",
        "/media/films/nested/comedy.mkv",
    )
    .await;
    let audio = item(
        &pool,
        library,
        Some(nested),
        "Audio",
        "/media/films/nested/song.flac",
    )
    .await;
    for id in [first, second] {
        metadata(
            &pool,
            id,
            "local-nfo",
            json!(["Drama", "Français"]),
            json!({"year":2020,"tags":["Weekend"]}),
            Some("PG-13"),
            Some(50),
        )
        .await;
        // A lower-priority result must not supply extra filter choices.
        metadata(
            &pool,
            id,
            "tvmaze",
            json!(["LowerPriority"]),
            json!({}),
            None,
            None,
        )
        .await;
    }
    metadata(
        &pool,
        comedy,
        "local-nfo",
        json!(["Comedy"]),
        json!({"year":2010,"tags":["Weekend","Short"]}),
        Some("G"),
        Some(0),
    )
    .await;
    metadata(
        &pool,
        audio,
        "local-nfo",
        json!(["Jazz"]),
        json!({"year":2022}),
        None,
        None,
    )
    .await;
    sqlx::query("UPDATE item_metadata SET premiere_date='2021-04-02' WHERE item_id=$1 AND provider_key='local-nfo'")
        .bind(second).execute(&pool).await.unwrap();
    let fallback = item(
        &pool,
        library,
        Some(folder),
        "Movie",
        "/media/films/fallback.mkv",
    )
    .await;
    metadata(
        &pool,
        fallback,
        "local-nfo",
        json!([]),
        json!({"year":"bad","tags":"malformed"}),
        Some("PG"),
        Some(25),
    )
    .await;
    metadata(
        &pool,
        fallback,
        "tvmaze",
        json!(["Culture"]),
        json!({}),
        None,
        None,
    )
    .await;
    sqlx::query("UPDATE item_metadata SET premiere_date='2004-03-02' WHERE item_id=$1 AND provider_key='tvmaze'")
        .bind(fallback).execute(&pool).await.unwrap();
    sqlx::query("INSERT INTO trusted_plugins(plugin_id,name,version,api_version,manifest_sha256,binary_sha256,declared_license,declared_provenance,enabled,status) VALUES ('facet-test','Facet test','1',1,$1,$2,'MIT','Synthetic fixture',TRUE,'enabled')")
        .bind("a".repeat(64)).bind("b".repeat(64)).execute(&pool).await.unwrap();
    metadata(
        &pool,
        fallback,
        "plugin:facet-test",
        json!(["PluginGenre"]),
        json!({"manifestSha256":"a".repeat(64),"moduleSha256":"b".repeat(64)}),
        None,
        None,
    )
    .await;

    let rated = item(
        &pool,
        library,
        Some(folder),
        "Movie",
        "/media/films/adult.mkv",
    )
    .await;
    metadata(
        &pool,
        rated,
        "local-nfo",
        json!(["Restricted"]),
        json!({"year":1999,"tags":["RestrictedTag"]}),
        Some("R"),
        Some(75),
    )
    .await;
    let private_item = item(&pool, private, None, "Movie", "/media/private.mkv").await;
    metadata(
        &pool,
        private_item,
        "local-nfo",
        json!(["PrivateGenre"]),
        json!({"year":2001,"tags":["PrivateTag"]}),
        Some("G"),
        Some(0),
    )
    .await;
    for (lib, path, kind, label) in [
        (disabled, "/media/disabled.mkv", "Movie", "DisabledGenre"),
        (library, "/media/.hidden/movie.mkv", "Movie", "HiddenGenre"),
        (library, "/media/opaque.bin", "File", "FileGenre"),
    ] {
        let id = item(&pool, lib, None, kind, path).await;
        metadata(
            &pool,
            id,
            "local-nfo",
            json!([label]),
            json!({}),
            Some("G"),
            Some(0),
        )
        .await;
    }
    let recorded = item(&pool, library, None, "Movie", "/media/recorded.mkv").await;
    metadata(
        &pool,
        recorded,
        "local-nfo",
        json!(["RecordingGenre"]),
        json!({}),
        Some("G"),
        Some(0),
    )
    .await;
    sqlx::query("UPDATE items SET metadata_json='{\"LiveTvRecording\":true}' WHERE id=$1")
        .bind(recorded)
        .execute(&pool)
        .await
        .unwrap();
    let config = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        public_base_url: None,
        database_url,
        server_name: "Facet fixture".to_owned(),
        web_root: PathBuf::from("web"),
        data_dir: env::temp_dir().join(format!("puffinbox-facets-{schema}")),
        ffmpeg_path: None,
        max_scan_workers: 1,
        max_page_size: 100,
        access_token_lifetime_hours: 24,
        cookie_secure: false,
        cors_origins: Vec::new(),
        trusted_proxies: Vec::new(),
        local_networks: vec!["127.0.0.0/8".parse().unwrap()],
        setup_token: None,
        bootstrap_admin_username: None,
        bootstrap_admin_password: None,
    };
    let state = AppState::new_for_run(pool.clone(), Arc::new(config), Uuid::new_v4(), run, None);
    let mut tokens = Vec::new();
    for user in [owner, peer, admin] {
        tokens.push(
            auth::issue_token(
                &state,
                &db::get_user(&pool, user).await.unwrap().unwrap(),
                "facet-fixture",
                "fixture",
                "facet-client",
            )
            .await
            .unwrap()
            .token,
        );
    }
    let router = api::router(state);
    let owner_token = &tokens[0];
    let legacy_response = call(&router, "/Items/Filters", Some(owner_token)).await;
    assert_eq!(legacy_response.headers()["cache-control"], "no-store");
    assert_eq!(legacy_response.headers()["pragma"], "no-cache");
    let legacy = json_body(legacy_response).await;
    assert_eq!(
        legacy["Genres"],
        json!(["Comedy", "Drama", "Français", "Jazz", "PluginGenre"])
    );
    assert_eq!(legacy["Tags"], json!(["Short", "Weekend"]));
    assert_eq!(legacy["OfficialRatings"], json!(["G", "PG", "PG-13"]));
    assert_eq!(legacy["Years"], json!([2004, 2010, 2020, 2021, 2022]));
    let modern_response = call(&router, "/Items/Filters2", Some(owner_token)).await;
    assert_eq!(modern_response.headers()["cache-control"], "no-store");
    let modern = json_body(modern_response).await;
    assert_eq!(modern["Tags"], legacy["Tags"]);
    assert_eq!(modern["AudioLanguages"], json!([]));
    assert_eq!(modern["SubtitleLanguages"], json!([]));
    let genres = modern["Genres"].as_array().unwrap();
    let id_for = |name: &str| {
        genres.iter().find(|row| row["Name"] == name).unwrap()["Id"]
            .as_str()
            .unwrap()
    };
    for row in genres {
        Uuid::parse_str(row["Id"].as_str().unwrap()).unwrap();
    }
    let repeated = json_body(call(&router, "/Items/Filters2", Some(owner_token)).await).await;
    assert_eq!(repeated, modern);
    let drama = json_body(
        call(
            &router,
            &format!("/Items?Recursive=true&GenreIds={}", id_for("Drama")),
            Some(owner_token),
        )
        .await,
    )
    .await;
    assert_eq!(drama["TotalRecordCount"], 2);
    let combined = json_body(call(&router,&format!("/Items?Recursive=true&GenreIds={}|{}&Tags=Weekend&Years=2010,2020&OfficialRatings=G|PG-13&Limit=1&StartIndex=1",id_for("Drama"),id_for("Comedy")),Some(owner_token)).await).await;
    assert_eq!(combined["TotalRecordCount"], 2);
    assert_eq!(combined["StartIndex"], 1);
    assert_eq!(combined["Items"].as_array().unwrap().len(), 1);
    let named = json_body(
        call(
            &router,
            "/Items?Recursive=true&Genres=drama&Tags=weekend&Years=2021&OfficialRatings=pg-13",
            Some(owner_token),
        )
        .await,
    )
    .await;
    assert_eq!(named["TotalRecordCount"], 1);
    assert_eq!(named["Items"][0]["Id"], second.to_string());
    assert_eq!(named["Items"][0]["ProductionYear"], 2021);
    assert_eq!(named["Items"][0]["Tags"], json!(["Weekend"]));
    verify_album_ordering(
        &router,
        &pool,
        library,
        private,
        disabled,
        owner,
        owner_token,
    )
    .await;
    verify_music_artist_and_track_ordering(&router, &pool, library, owner, owner_token).await;
    let unknown = json_body(
        call(
            &router,
            &format!("/Items?Recursive=true&GenreIds={}", Uuid::new_v4()),
            Some(owner_token),
        )
        .await,
    )
    .await;
    assert_eq!(unknown["TotalRecordCount"], 0);
    let direct = json_body(
        call(
            &router,
            &format!("/Items/Filters?parentId={folder}&recursive=false&includeItemTypes=Movie"),
            Some(owner_token),
        )
        .await,
    )
    .await;
    assert_eq!(
        direct["Genres"],
        json!(["Drama", "Français", "PluginGenre"])
    );
    let recursive = json_body(
        call(
            &router,
            &format!("/Items/Filters?ParentId={folder}&Recursive=true&IncludeItemTypes=Movie"),
            Some(owner_token),
        )
        .await,
    )
    .await;
    assert_eq!(
        recursive["Genres"],
        json!(["Comedy", "Drama", "Français", "PluginGenre"])
    );
    let music = json_body(
        call(
            &router,
            "/Items/Filters?mediaTypes=Audio",
            Some(owner_token),
        )
        .await,
    )
    .await;
    assert_eq!(music["Genres"], json!(["Jazz"]));
    let peer_facets = json_body(call(&router, "/Items/Filters", Some(&tokens[1])).await).await;
    assert_eq!(peer_facets["Genres"], json!(["PrivateGenre"]));
    let selected = json_body(
        call(
            &router,
            &format!("/Items/Filters?userId={owner}"),
            Some(&tokens[2]),
        )
        .await,
    )
    .await;
    assert_eq!(selected, legacy);
    let admin_facets = json_body(call(&router, "/Items/Filters", Some(&tokens[2])).await).await;
    assert!(
        admin_facets["Genres"]
            .as_array()
            .unwrap()
            .contains(&json!("Restricted"))
    );
    assert!(
        admin_facets["Genres"]
            .as_array()
            .unwrap()
            .contains(&json!("PrivateGenre"))
    );
    assert!(
        !admin_facets["Genres"]
            .as_array()
            .unwrap()
            .contains(&json!("DisabledGenre"))
    );
    for path in [
        format!("/Items/Filters?userId={peer}"),
        format!("/Items/Filters2?userId={peer}"),
    ] {
        assert_eq!(
            call(&router, &path, Some(owner_token)).await.status(),
            StatusCode::FORBIDDEN
        );
    }
    for path in [
        format!("/Items/Filters?parentId={private}"),
        format!("/Items/Filters2?parentId={rated}"),
        format!("/Items/Filters?parentId={disabled}"),
        format!("/Items/Filters2?parentId={}", Uuid::new_v4()),
    ] {
        assert_eq!(
            call(&router, &path, Some(owner_token)).await.status(),
            StatusCode::NOT_FOUND
        );
    }
    for path in ["/Items/Filters", "/Items/Filters2"] {
        let response = call(&router, path, None).await;
        assert_eq!(response.status(), StatusCode::UNAUTHORIZED);
        assert_eq!(response.headers()["cache-control"], "no-store");
    }
    for path in [
        "/Items?GenreIds=bad",
        "/Items?Years=1799",
        "/Items?Years=99999999999999999999",
        "/Items/Filters?mediaTypes=invalid",
        "/Items/Filters2?recursive=invalid",
        "/Items/Filters2?isAiring=false",
        "/Items/Filters2?isSports=true",
    ] {
        assert_eq!(
            call(&router, path, Some(owner_token)).await.status(),
            StatusCode::BAD_REQUEST,
            "{path}"
        );
    }
    // Disabling a trusted provider changes both the visible facet and its item selection.
    sqlx::query(
        "UPDATE trusted_plugins SET enabled=FALSE,status='disabled' WHERE plugin_id='facet-test'",
    )
    .execute(&pool)
    .await
    .unwrap();
    let after = json_body(call(&router, "/Items/Filters", Some(owner_token)).await).await;
    assert!(
        after["Genres"]
            .as_array()
            .unwrap()
            .contains(&json!("Culture"))
    );
    assert!(
        !after["Genres"]
            .as_array()
            .unwrap()
            .contains(&json!("PluginGenre"))
    );
    let old_id = json_body(
        call(
            &router,
            &format!("/Items?Recursive=true&GenreIds={}", id_for("PluginGenre")),
            Some(owner_token),
        )
        .await,
    )
    .await;
    assert_eq!(old_id["TotalRecordCount"], 0);
    verify_embedded_audio_metadata(&router, &pool, library, private, owner, owner_token).await;
    verify_audio_sort_names(&router, &pool, library, owner, owner_token).await;
    sqlx::query("UPDATE users SET block_unrated_items=ARRAY['Music'] WHERE id=$1")
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    let blocked = json_body(call(&router, "/Items/Filters", Some(owner_token)).await).await;
    assert!(
        !blocked["Genres"]
            .as_array()
            .unwrap()
            .contains(&json!("Jazz"))
    );
    sqlx::query("DELETE FROM user_library_access WHERE user_id=$1")
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    let revoked = json_body(call(&router, "/Items/Filters2", Some(owner_token)).await).await;
    assert_eq!(revoked["Genres"], json!([]));
    assert_eq!(revoked["Tags"], json!([]));
    assert_eq!(
        call(
            &router,
            &format!("/Items/Filters?parentId={library}"),
            Some(owner_token)
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );

    // More distinct choices than the response bound must fail rather than return
    // a misleading partial menu. This uses 33 rows of bounded metadata arrays.
    let capped = Uuid::new_v4();
    db::insert_library(
        &pool,
        run,
        capped,
        "Facet bound",
        "movies",
        &[PathBuf::from("/facet-bound")],
        true,
    )
    .await
    .unwrap();
    sqlx::query("INSERT INTO user_library_access(user_id,library_id) VALUES ($1,$2)")
        .bind(owner)
        .bind(capped)
        .execute(&pool)
        .await
        .unwrap();
    for chunk in 0..33 {
        let id = item(
            &pool,
            capped,
            None,
            "Movie",
            &format!("/facet-bound/{chunk}.mkv"),
        )
        .await;
        let count = if chunk == 32 { 1 } else { 128 };
        let labels = (0..count)
            .map(|offset| format!("Genre{:04}", chunk * 128 + offset))
            .collect::<Vec<_>>();
        metadata(
            &pool,
            id,
            "local-nfo",
            json!(labels),
            json!({}),
            Some("G"),
            Some(0),
        )
        .await;
    }
    assert_eq!(
        call(&router, "/Items/Filters2", Some(owner_token))
            .await
            .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    let filtered = json_body(
        call(
            &router,
            "/Items?Recursive=true&Genres=Genre4096",
            Some(owner_token),
        )
        .await,
    )
    .await;
    assert_eq!(filtered["TotalRecordCount"], 1);
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .execute(&admin_pool)
        .await
        .unwrap();
}

async fn item(pool: &PgPool, library: Uuid, parent: Option<Uuid>, kind: &str, path: &str) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO items(id,library_id,parent_id,name,sort_name,item_type,path,path_hash) VALUES ($1,$2,$3,$4,$4,$5,$4,$6)")
        .bind(id).bind(library).bind(parent).bind(path).bind(kind).bind(db::path_hash(path))
        .execute(pool).await.unwrap();
    id
}

async fn verify_album_ordering(
    router: &Router,
    pool: &PgPool,
    library: Uuid,
    private: Uuid,
    disabled: Uuid,
    owner: Uuid,
    token: &str,
) {
    let mut albums = Vec::new();
    for (index, name, sort_name, premiere, year) in [
        (0, "Zebra", "alpha", Some("2022-04-02"), Some(1900)),
        (1, "Alpha", "beta", Some("2022-04-02"), Some(1900)),
        (2, "Older", "gamma", Some("2010-01-01"), Some(2040)),
        (3, "Recent", "delta", None, Some(2021)),
        (4, "Past", "epsilon", None, Some(1999)),
        (5, "Unknown", "omega", None, None),
    ] {
        let album = item(
            pool,
            library,
            None,
            "MusicAlbum",
            &format!("/media/order/{index}"),
        )
        .await;
        item(
            pool,
            library,
            Some(album),
            "Audio",
            &format!("/media/order/{index}/song.flac"),
        )
        .await;
        sqlx::query("UPDATE items SET name=$2,sort_name=$3 WHERE id=$1")
            .bind(album)
            .bind(name)
            .bind(sort_name)
            .execute(pool)
            .await
            .unwrap();
        metadata(
            pool,
            album,
            "local-nfo",
            json!([]),
            json!({"year":year}),
            None,
            None,
        )
        .await;
        sqlx::query("UPDATE item_metadata SET premiere_date=$2::text::date WHERE item_id=$1 AND provider_key='local-nfo'")
            .bind(album).bind(premiere).execute(pool).await.unwrap();
        albums.push(album);
    }
    // Lower-priority and untrusted dates must not replace the displayed value.
    for (id, provider) in [(albums[2], "tvmaze"), (albums[4], "plugin:untrusted")] {
        metadata(pool, id, provider, json!([]), json!({}), None, None).await;
        sqlx::query("UPDATE item_metadata SET premiere_date='2099-01-01' WHERE item_id=$1 AND provider_key=$2")
            .bind(id).bind(provider).execute(pool).await.unwrap();
    }
    for (library_id, path, rating) in [
        (private, "/media/order/private", None),
        (disabled, "/media/order/disabled", None),
        (library, "/media/.hidden-order", None),
        (library, "/media/order/restricted", Some(90)),
    ] {
        let album = item(pool, library_id, None, "MusicAlbum", path).await;
        item(
            pool,
            library_id,
            Some(album),
            "Audio",
            &format!("{path}/song.flac"),
        )
        .await;
        metadata(
            pool,
            album,
            "local-nfo",
            json!([]),
            json!({"year":2099}),
            None,
            rating,
        )
        .await;
        sqlx::query("UPDATE item_metadata SET premiere_date='2099-01-01' WHERE item_id=$1")
            .bind(album)
            .execute(pool)
            .await
            .unwrap();
    }
    let prefix = "/Items?Recursive=true&IncludeItemTypes=MusicAlbum";
    let ordered = json_body(call(router, &format!("{prefix}&SortBy=PremiereDate,ProductionYear,SortName&SortOrder=Descending,Descending,Ascending"), Some(token)).await).await;
    assert_eq!(ordered["TotalRecordCount"], 6);
    assert_eq!(item_ids(&ordered), albums);
    assert_eq!(ordered["Items"][0]["ProductionYear"], 2022);
    assert_eq!(ordered["Items"][2]["ProductionYear"], 2010);
    assert_eq!(ordered["Items"][4]["ProductionYear"], 1999);
    let descending = json_body(
        call(
            router,
            &format!("{prefix}&sortBy=PremiereDate,ProductionYear,SortName&sortOrder=Descending"),
            Some(token),
        )
        .await,
    )
    .await;
    assert_eq!(
        item_ids(&descending),
        vec![
            albums[1], albums[0], albums[2], albums[3], albums[4], albums[5]
        ]
    );
    let mixed = json_body(call(router, &format!("{prefix}&SortBy=PremiereDate,ProductionYear,SortName&SortOrder=Descending,Ascending,Descending"), Some(token)).await).await;
    assert_eq!(
        item_ids(&mixed),
        vec![
            albums[1], albums[0], albums[2], albums[4], albums[3], albums[5]
        ]
    );
    let year = json_body(
        call(
            router,
            &format!("{prefix}&SortBy=ProductionYear,SortName&SortOrder=Descending,Ascending"),
            Some(token),
        )
        .await,
    )
    .await;
    assert_eq!(
        item_ids(&year),
        vec![
            albums[0], albums[1], albums[3], albums[2], albums[4], albums[5]
        ]
    );
    let name = json_body(call(router, &format!("{prefix}&SortBy=Name"), Some(token)).await).await;
    assert_eq!(
        item_ids(&name),
        vec![
            albums[1], albums[2], albums[4], albums[3], albums[5], albums[0]
        ]
    );
    let filtered = json_body(call(router, &format!("{prefix}&SortBy=PremiereDate,ProductionYear,SortName&SortOrder=Descending,Descending,Ascending&ExcludeItemIds={},{},{}&StartIndex=1&Limit=2",albums[0],albums[2],albums[0]), Some(token)).await).await;
    assert_eq!(filtered["TotalRecordCount"], 4);
    assert_eq!(filtered["StartIndex"], 1);
    assert_eq!(item_ids(&filtered), vec![albums[3], albums[4]]);
    let legacy = json_body(
        call(
            router,
            &format!(
                "/Users/{owner}/Items?Ids={},{},{}&excludeItemIds={}",
                albums[5], albums[0], albums[1], albums[0]
            ),
            Some(token),
        )
        .await,
    )
    .await;
    assert_eq!(legacy["TotalRecordCount"], 2);
    assert_eq!(item_ids(&legacy), vec![albums[5], albums[1]]);
    let unknown = json_body(
        call(
            router,
            &format!("{prefix}&excludeItemIds={}", Uuid::new_v4()),
            Some(token),
        )
        .await,
    )
    .await;
    assert_eq!(unknown["TotalRecordCount"], 6);
    for (id, date, ticks) in [(albums[0], "2020-01-01", 10), (albums[1], "2021-01-01", 20)] {
        sqlx::query("INSERT INTO user_item_data(user_id,item_id,playback_position_ticks,last_played_at) VALUES ($1,$2,$3,$4::text::timestamptz)")
            .bind(owner).bind(id).bind(ticks as i64).bind(date).execute(pool).await.unwrap();
    }
    let played = json_body(call(router, &format!("{prefix}&SortBy=ProductionYear,LastPlayedDate,SortName&SortOrder=Descending,Descending,Ascending"), Some(token)).await).await;
    assert_eq!(
        item_ids(&played),
        vec![
            albums[1], albums[0], albums[3], albums[2], albums[4], albums[5]
        ]
    );
    let resume = json_body(call(router, &format!("{prefix}&Filters=IsResumable&SortBy=ProductionYear,LastPlayedDate&SortOrder=Descending"), Some(token)).await).await;
    assert_eq!(resume["TotalRecordCount"], 2);
    assert_eq!(item_ids(&resume), vec![albums[1], albums[0]]);
    sqlx::query("INSERT INTO user_item_data(user_id,item_id,is_favorite) VALUES ($1,$2,TRUE)")
        .bind(owner)
        .bind(albums[5])
        .execute(pool)
        .await
        .unwrap();
    let date_played = json_body(
        call(
            router,
            &format!(
                "{prefix}&Ids={},{},{}&SortBy=DatePlayed&SortOrder=Descending",
                albums[5], albums[0], albums[1]
            ),
            Some(token),
        )
        .await,
    )
    .await;
    assert_eq!(
        item_ids(&date_played),
        vec![albums[1], albums[0], albums[5]]
    );
    let fewer_orders = json_body(call(router, &format!("{prefix}&SortBy=PremiereDate,ProductionYear,SortName&SortOrder=Descending,Ascending"), Some(token)).await).await;
    assert_eq!(item_ids(&fewer_orders), item_ids(&mixed));
    let too_many_ids = std::iter::repeat_n(albums[0].to_string(), 1001)
        .collect::<Vec<_>>()
        .join(",");
    assert_eq!(
        call(
            router,
            &format!("{prefix}&ExcludeItemIds={too_many_ids}"),
            Some(token)
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    for suffix in [
        "SortBy=SortName,Unsupported",
        "SortBy=PremiereDate,SortName&SortOrder=Ascending,Invalid",
        "SortBy=SortName&SortOrder=Ascending,Descending",
        "SortBy=SortName,,Name",
        "SortBy=SortName,SortName,SortName,SortName,SortName,SortName,SortName,SortName,SortName",
        "ExcludeItemIds=not-an-id",
    ] {
        assert_eq!(
            call(router, &format!("{prefix}&{suffix}"), Some(token))
                .await
                .status(),
            StatusCode::BAD_REQUEST,
            "{suffix}"
        );
    }
    // Keep the existing facet fixture's expected menus unchanged.
    sqlx::query(
        "DELETE FROM items WHERE path LIKE '/media/order/%' OR path LIKE '/media/.hidden-order%'",
    )
    .execute(pool)
    .await
    .unwrap();
}

fn item_ids(result: &Value) -> Vec<Uuid> {
    result["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| Uuid::parse_str(item["Id"].as_str().unwrap()).unwrap())
        .collect()
}

async fn verify_music_artist_and_track_ordering(
    router: &Router,
    pool: &PgPool,
    library: Uuid,
    owner: Uuid,
    token: &str,
) {
    let artist = item(
        pool,
        library,
        None,
        "MusicArtist",
        "/media/ordering-music/artist-a",
    )
    .await;
    let other = item(
        pool,
        library,
        None,
        "MusicArtist",
        "/media/ordering-music/artist-b",
    )
    .await;
    let album = item(
        pool,
        library,
        Some(artist),
        "MusicAlbum",
        "/media/ordering-music/artist-a/album",
    )
    .await;
    let unrelated = item(
        pool,
        library,
        Some(other),
        "MusicAlbum",
        "/media/ordering-music/artist-b/album",
    )
    .await;
    let unrelated_track = item(
        pool,
        library,
        Some(unrelated),
        "Audio",
        "/media/ordering-music/artist-b/album/1.flac",
    )
    .await;
    let mut tracks = Vec::new();
    // Filenames deliberately disagree with the local NFO disc/track order.
    for (name, disc, track) in [
        ("Zebra.flac", Some(1), Some(1)),
        ("Alpha.flac", Some(1), Some(2)),
        ("Beta.flac", Some(2), Some(1)),
        ("Unknown.flac", None, None),
    ] {
        let id = item(
            pool,
            library,
            Some(album),
            "Audio",
            &format!("/media/ordering-music/artist-a/album/{name}"),
        )
        .await;
        metadata(
            pool,
            id,
            "local-nfo",
            json!([]),
            json!({"discNumber":disc,"trackNumber":track}),
            None,
            None,
        )
        .await;
        tracks.push(id);
    }
    metadata(
        pool,
        tracks[0],
        "plugin:untrusted",
        json!([]),
        json!({"discNumber":99,"trackNumber":99}),
        None,
        None,
    )
    .await;
    let ordered = json_body(call(router, &format!("/Users/{owner}/Items?ParentId={album}&SortBy=ParentIndexNumber,IndexNumber,SortName"), Some(token)).await).await;
    assert_eq!(ordered["TotalRecordCount"], 4);
    assert_eq!(item_ids(&ordered), tracks);
    let album_queue = json_body(call(router, &format!("/Users/{owner}/Items?ParentId={album}&Filters=IsNotFolder&Recursive=true&SortBy=Album,ParentIndexNumber,IndexNumber,SortName&MediaTypes=Audio,Video&Limit=300&Fields=Chapters,MediaSources,Trickplay&ExcludeLocationTypes=Virtual&EnableTotalRecordCount=false&CollapseBoxSetItems=false"), Some(token)).await).await;
    assert_eq!(item_ids(&album_queue), tracks);
    assert!(album_queue.get("TotalRecordCount").is_none());
    for (id, name) in [(album, "Zebra album"), (unrelated, "Alpha album")] {
        sqlx::query("UPDATE items SET name=$2 WHERE id=$1")
            .bind(id)
            .bind(name)
            .execute(pool)
            .await
            .unwrap();
    }
    let explicit_ids = std::iter::once(unrelated_track)
        .chain(tracks.iter().copied())
        .map(|id| id.to_string())
        .collect::<Vec<_>>()
        .join(",");
    let grouped = json_body(
        call(
            router,
            &format!(
                "/Items?Ids={explicit_ids}&sortBy=Album,ParentIndexNumber,IndexNumber,SortName"
            ),
            Some(token),
        )
        .await,
    )
    .await;
    let expected = std::iter::once(unrelated_track)
        .chain(tracks.iter().copied())
        .collect::<Vec<_>>();
    assert_eq!(item_ids(&grouped), expected);
    assert_eq!(grouped["Items"][0]["Album"], "Alpha album");
    assert_eq!(grouped["Items"][1]["Album"], "Zebra album");
    let grouped_descending = json_body(call(router, &format!("/Items?Ids={explicit_ids}&SortBy=Album,ParentIndexNumber,IndexNumber,SortName&SortOrder=Descending,Ascending,Ascending,Ascending"), Some(token)).await).await;
    let expected = tracks
        .iter()
        .copied()
        .chain(std::iter::once(unrelated_track))
        .collect::<Vec<_>>();
    assert_eq!(item_ids(&grouped_descending), expected);
    for (row, disc, track) in [(0, 1, 1), (1, 1, 2), (2, 2, 1)] {
        assert_eq!(ordered["Items"][row]["ParentIndexNumber"], disc);
        assert_eq!(ordered["Items"][row]["IndexNumber"], track);
    }
    let descending = json_body(call(router, &format!("/Items?ParentId={album}&sortBy=ParentIndexNumber,IndexNumber,SortName&sortOrder=Descending,Ascending"), Some(token)).await).await;
    assert_eq!(
        item_ids(&descending),
        vec![tracks[2], tracks[0], tracks[1], tracks[3]]
    );
    for key in ["ArtistIds", "AlbumArtistIds", "artistIds", "albumArtistIds"] {
        let selected = json_body(
            call(
                router,
                &format!("/Items?Recursive=true&IncludeItemTypes=MusicAlbum&{key}={artist}"),
                Some(token),
            )
            .await,
        )
        .await;
        assert_eq!(selected["TotalRecordCount"], 1);
        assert_eq!(item_ids(&selected), vec![album]);
    }
    let selected = json_body(call(router, &format!("/Items?ParentId={artist}&Recursive=true&IncludeItemTypes=Audio&AlbumArtistIds={artist}&SortBy=ParentIndexNumber,IndexNumber,SortName&Limit=2&StartIndex=1"), Some(token)).await).await;
    assert_eq!(selected["TotalRecordCount"], 4);
    assert_eq!(item_ids(&selected), vec![tracks[1], tracks[2]]);
    let disjoint = json_body(call(router, &format!("/Items?Recursive=true&IncludeItemTypes=MusicAlbum&ArtistIds={artist}&AlbumArtistIds={other}"), Some(token)).await).await;
    assert_eq!(disjoint["TotalRecordCount"], 0);
    let excluded = json_body(call(router, &format!("/Items?Recursive=true&IncludeItemTypes=MusicAlbum&AlbumArtistIds={artist}&ExcludeItemIds={album}"), Some(token)).await).await;
    assert_eq!(excluded["TotalRecordCount"], 0);
    let hidden_artist = item(
        pool,
        library,
        None,
        "MusicArtist",
        "/media/.hidden-ordering-artist",
    )
    .await;
    let hidden_album = item(
        pool,
        library,
        Some(hidden_artist),
        "MusicAlbum",
        "/media/ordering-music/visible-child",
    )
    .await;
    item(
        pool,
        library,
        Some(hidden_album),
        "Audio",
        "/media/ordering-music/visible-child/song.flac",
    )
    .await;
    let hidden_parent = json_body(
        call(
            router,
            &format!(
                "/Items?Recursive=true&IncludeItemTypes=MusicAlbum&AlbumArtistIds={hidden_artist}"
            ),
            Some(token),
        )
        .await,
    )
    .await;
    assert_eq!(hidden_parent["TotalRecordCount"], 0);
    let private_album = item(
        pool,
        library,
        Some(artist),
        "MusicAlbum",
        "/media/.hidden-ordering-album",
    )
    .await;
    let child = item(
        pool,
        library,
        Some(private_album),
        "Audio",
        "/media/ordering-music/visible-track.flac",
    )
    .await;
    let hidden_album_match = json_body(
        call(
            router,
            &format!("/Items?Ids={child}&AlbumArtistIds={artist}"),
            Some(token),
        )
        .await,
    )
    .await;
    assert_eq!(hidden_album_match["TotalRecordCount"], 0);
    let private_album_sort = json_body(
        call(
            router,
            &format!(
                "/Items?Ids={},{}&SortBy=Album&SortOrder=Descending",
                child, tracks[0]
            ),
            Some(token),
        )
        .await,
    )
    .await;
    assert_eq!(item_ids(&private_album_sort), vec![tracks[0], child]);
    assert!(private_album_sort["Items"][1].get("Album").is_none());
    metadata(
        pool,
        artist,
        "local-nfo",
        json!([]),
        json!({}),
        None,
        Some(90),
    )
    .await;
    let restricted_artist = json_body(
        call(
            router,
            &format!("/Items?Recursive=true&IncludeItemTypes=Audio&ArtistIds={artist}"),
            Some(token),
        )
        .await,
    )
    .await;
    assert_eq!(restricted_artist["TotalRecordCount"], 0);
    sqlx::query("UPDATE item_metadata SET policy_rating_scale=NULL,policy_rating_value=NULL WHERE item_id=$1 AND provider_key='local-nfo'")
        .bind(artist).execute(pool).await.unwrap();
    for invalid in [
        json!(-1),
        json!(2147483648_i64),
        json!("NaN"),
        json!([1]),
        json!("999999999999999999999999999999"),
    ] {
        sqlx::query("UPDATE item_metadata SET metadata_json=$2 WHERE item_id=$1 AND provider_key='local-nfo'")
            .bind(tracks[3]).bind(Json(json!({"discNumber":invalid,"trackNumber":invalid}))).execute(pool).await.unwrap();
        let result = json_body(
            call(
                router,
                &format!("/Items?ParentId={album}&SortBy=ParentIndexNumber,IndexNumber,SortName"),
                Some(token),
            )
            .await,
        )
        .await;
        assert_eq!(item_ids(&result), tracks);
        assert!(result["Items"][3].get("IndexNumber").is_none());
        assert!(result["Items"][3].get("ParentIndexNumber").is_none());
    }
    let unnumbered = json_body(
        call(
            router,
            &format!("/Items?Ids={}&SortBy=IndexNumber", tracks[3]),
            Some(token),
        )
        .await,
    )
    .await;
    assert_eq!(unnumbered["TotalRecordCount"], 1);
    let series = item(
        pool,
        library,
        None,
        "Series",
        "/media/ordering-music/series",
    )
    .await;
    let mut episodes = Vec::new();
    for (season_index, episode_index) in [(2, 1), (1, 2), (1, 1)] {
        let season = item(
            pool,
            library,
            Some(series),
            "Season",
            &format!("/media/ordering-music/season-{season_index}-{episode_index}/{season_index}"),
        )
        .await;
        let episode = item(pool, library, Some(season), "Episode", &format!("/media/ordering-music/season-{season_index}-{episode_index}/S{season_index:02}E{episode_index:02}.mkv")).await;
        episodes.push(episode);
    }
    let hidden_season = item(
        pool,
        library,
        Some(series),
        "Season",
        "/media/.hidden-ordering-season0",
    )
    .await;
    let visible_episode = item(
        pool,
        library,
        Some(hidden_season),
        "Episode",
        "/media/ordering-music/visible-S00E01.mkv",
    )
    .await;
    let episode_page = json_body(call(router, &format!("/Items?ParentId={series}&Recursive=true&IncludeItemTypes=Episode&SortBy=ParentIndexNumber,IndexNumber,SortName"), Some(token)).await).await;
    assert_eq!(
        item_ids(&episode_page),
        vec![episodes[2], episodes[1], episodes[0], visible_episode]
    );
    assert_eq!(episode_page["Items"][0]["ParentIndexNumber"], 1);
    assert_eq!(episode_page["Items"][0]["IndexNumber"], 1);
    assert!(episode_page["Items"][3].get("ParentIndexNumber").is_none());
    assert_eq!(
        call(router, "/Items?ArtistIds=invalid", Some(token))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    sqlx::query("DELETE FROM items WHERE path LIKE '/media/ordering-music/%' OR path IN ('/media/.hidden-ordering-artist','/media/.hidden-ordering-album','/media/.hidden-ordering-season0')")
        .execute(pool).await.unwrap();
}

async fn metadata(
    pool: &PgPool,
    id: Uuid,
    provider: &str,
    genres: Value,
    metadata: Value,
    label: Option<&str>,
    policy: Option<i16>,
) {
    sqlx::query("INSERT INTO item_metadata(item_id,provider_key,genres,metadata_json,content_rating,policy_rating_scale,policy_rating_value) VALUES ($1,$2,$3,$4,$5,CASE WHEN $6::smallint IS NOT NULL THEN 'US-MPAA-v1' END,$6)")
        .bind(id).bind(provider).bind(Json(genres)).bind(Json(metadata)).bind(label).bind(policy)
        .execute(pool).await.unwrap();
}

async fn verify_audio_sort_names(
    router: &Router,
    pool: &PgPool,
    library: Uuid,
    owner: Uuid,
    token: &str,
) {
    // Public Jellyfin 12 FLAC observations: no prefix for a missing number,
    // zero is meaningful, and values wider than four digits are not truncated.
    let cases = [
        ("Sort Plain", None, None, "Sort Plain"),
        ("Sort Track Only", None, Some(2), "0002 - Sort Track Only"),
        ("Sort Disc Only", Some(2), None, "0002 - Sort Disc Only"),
        ("Sort Both", Some(1), Some(2), "0001 - 0002 - Sort Both"),
        (
            "Sort Large",
            Some(10001),
            Some(10002),
            "10001 - 10002 - Sort Large",
        ),
        ("Sort Zero", Some(0), Some(0), "0000 - 0000 - Sort Zero"),
        (
            "Sort Zero Disc",
            Some(0),
            Some(2),
            "0000 - 0002 - Sort Zero Disc",
        ),
        ("The Zebra", Some(1), Some(1), "0001 - 0001 - The Zebra"),
        (
            "A, Small: Test!",
            Some(1),
            Some(3),
            "0001 - 0003 - A, Small: Test!",
        ),
    ];
    let album = item(pool, library, None, "MusicAlbum", "/media/sort-contract").await;
    let mut tracks = Vec::new();
    for (index, (title, disc, track, _)) in cases.iter().enumerate() {
        let id = item(
            pool,
            library,
            Some(album),
            "Audio",
            &format!("/media/sort-contract/case-{index:02}.flac"),
        )
        .await;
        sqlx::query("UPDATE items SET name=$2,sort_name=lower($2),size_bytes=100,date_modified='2026-10-03T00:00:00Z' WHERE id=$1")
            .bind(id).bind(format!("case-{index:02}")).execute(pool).await.unwrap();
        sqlx::query("INSERT INTO item_metadata(item_id,provider_key,title,metadata_json,source_library_id,source_path_hash,source_size_bytes,source_date_modified) SELECT id,'embedded-audio',$2,$3,library_id,path_hash,size_bytes,date_modified FROM items WHERE id=$1")
            .bind(id).bind(title).bind(Json(json!({"discNumber":disc,"trackNumber":track}))).execute(pool).await.unwrap();
        tracks.push(id);
    }
    let prefix =
        format!("/Users/{owner}/Items?ParentId={album}&IncludeItemTypes=Audio&Fields=SortName");
    let ascending_indices = [5, 6, 7, 3, 8, 2, 1, 4, 0];
    let ascending_ids: Vec<_> = ascending_indices
        .iter()
        .map(|index| tracks[*index])
        .collect();
    let ascending =
        json_body(call(router, &format!("{prefix}&SortBy=SortName"), Some(token)).await).await;
    assert_eq!(ascending["TotalRecordCount"], 9);
    assert_eq!(item_ids(&ascending), ascending_ids);
    for (row, index) in ascending["Items"]
        .as_array()
        .unwrap()
        .iter()
        .zip(ascending_indices)
    {
        assert_eq!(row["Name"], cases[index].0);
        assert_eq!(row["SortName"], cases[index].3);
        assert_eq!(row["ParentIndexNumber"], json!(cases[index].1));
        assert_eq!(row["IndexNumber"], json!(cases[index].2));
    }
    let descending = json_body(
        call(
            router,
            &format!("{prefix}&sortBy=SortName&sortOrder=Descending"),
            Some(token),
        )
        .await,
    )
    .await;
    assert_eq!(
        item_ids(&descending),
        ascending_ids.iter().rev().copied().collect::<Vec<_>>()
    );
    let names = json_body(call(router, &format!("{prefix}&SortBy=Name"), Some(token)).await).await;
    assert_eq!(
        item_ids(&names),
        [8, 3, 2, 4, 0, 1, 5, 6, 7].map(|index| tracks[index])
    );
    let page = json_body(
        call(
            router,
            &format!("{prefix}&SortBy=SortName&StartIndex=3&Limit=2"),
            Some(token),
        )
        .await,
    )
    .await;
    assert_eq!(page["TotalRecordCount"], 9);
    assert_eq!(item_ids(&page), [tracks[3], tracks[8]]);

    // Filtering must precede the same ordering and paging expressions.
    for id in &tracks[..4] {
        sqlx::query("INSERT INTO user_item_data(user_id,item_id,rating) VALUES ($1,$2,10)")
            .bind(owner)
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
    }
    let likes = json_body(
        call(
            router,
            &format!("{prefix}&Filters=Likes&SortBy=SortName&StartIndex=1&Limit=2"),
            Some(token),
        )
        .await,
    )
    .await;
    assert_eq!(likes["TotalRecordCount"], 4);
    assert_eq!(item_ids(&likes), [tracks[2], tracks[1]]);

    // A changed file identity invalidates both the returned prefix and SQL
    // order. The stale metadata row stays available for diagnosis.
    sqlx::query("UPDATE items SET size_bytes=101 WHERE id=$1")
        .bind(tracks[5])
        .execute(pool)
        .await
        .unwrap();
    let stale = json_body(call(router, &format!("/Items/{}", tracks[5]), Some(token)).await).await;
    assert_eq!(stale["Name"], "case-05");
    assert_eq!(stale["SortName"], "case-05");
    let changed =
        json_body(call(router, &format!("{prefix}&SortBy=SortName"), Some(token)).await).await;
    assert_eq!(
        item_ids(&changed),
        [6, 7, 3, 8, 2, 1, 4, 5, 0].map(|index| tracks[index])
    );
}

async fn verify_embedded_audio_metadata(
    router: &Router,
    pool: &PgPool,
    library: Uuid,
    private: Uuid,
    owner: Uuid,
    token: &str,
) {
    let artist = item(
        pool,
        library,
        None,
        "MusicArtist",
        "/media/embedded/Embedded Lead",
    )
    .await;
    let album = item(
        pool,
        library,
        Some(artist),
        "MusicAlbum",
        "/media/embedded/Folder Album",
    )
    .await;
    let first = item(
        pool,
        library,
        Some(album),
        "Audio",
        "/media/embedded/blob-a.flac",
    )
    .await;
    let second = item(
        pool,
        library,
        Some(album),
        "Audio",
        "/media/embedded/blob-b.flac",
    )
    .await;
    let hidden = item(
        pool,
        private,
        None,
        "Audio",
        "/media/private/embedded-hidden.flac",
    )
    .await;
    for (id, name) in [
        (artist, "Embedded Lead"),
        (album, "Folder Album"),
        (first, "blob-a"),
        (second, "blob-b"),
    ] {
        sqlx::query("UPDATE items SET name=$2,sort_name=lower($2) WHERE id=$1")
            .bind(id)
            .bind(name)
            .execute(pool)
            .await
            .unwrap();
    }
    for (id, title, album_name, number, genre, credit) in [
        (
            first,
            "Embedded Alpha Title",
            "Zebra tagged album",
            3,
            "Rock; Jazz",
            "Embedded Lead",
        ),
        (
            second,
            "Embedded Beta Title",
            "Alpha tagged album",
            4,
            "Jazz",
            "Embedded Lead",
        ),
        (
            hidden,
            "Private tagged title",
            "Private tagged album",
            1,
            "PrivateTagGenre",
            "Private tagged artist",
        ),
    ] {
        sqlx::query(
            "UPDATE items SET size_bytes=100,date_modified='2021-04-05T00:00:00Z' WHERE id=$1",
        )
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO item_metadata(item_id,provider_key,title,premiere_date,genres,metadata_json,source_library_id,source_path_hash,source_size_bytes,source_date_modified) SELECT id,'embedded-audio',$2,'2021-04-05',$3,$4,library_id,path_hash,size_bytes,date_modified FROM items WHERE id=$1")
            .bind(id).bind(title).bind(Json(json!([genre])))
            .bind(Json(json!({"album":album_name,"artists":[credit],"albumArtists":[credit],"trackNumber":number,"discNumber":2})))
            .execute(pool).await.unwrap();
    }
    let prefix = format!("/Users/{owner}/Items?ParentId={album}&IncludeItemTypes=Audio");
    let ordered = json_body(
        call(
            router,
            &format!("{prefix}&SortBy=Album,IndexNumber"),
            Some(token),
        )
        .await,
    )
    .await;
    assert_eq!(item_ids(&ordered), [second, first]);
    let tagged = &ordered["Items"][1];
    assert_eq!(tagged["Name"], "Embedded Alpha Title");
    assert_eq!(tagged["Album"], "Zebra tagged album");
    assert_eq!(tagged["IndexNumber"], 3);
    assert_eq!(tagged["ParentIndexNumber"], 2);
    assert_eq!(tagged["ProductionYear"], 2021);
    assert_eq!(tagged["Artists"], json!(["Embedded Lead"]));
    assert_eq!(tagged["ArtistItems"][0]["Id"], artist.to_string());
    let search = json_body(
        call(
            router,
            &format!("{prefix}&SearchTerm=Alpha%20Title"),
            Some(token),
        )
        .await,
    )
    .await;
    assert_eq!(item_ids(&search), [first]);
    let by_artist = json_body(
        call(
            router,
            &format!("{prefix}&ArtistIds={artist}&SortBy=IndexNumber"),
            Some(token),
        )
        .await,
    )
    .await;
    assert_eq!(item_ids(&by_artist), [first, second]);
    let private_facet =
        json_body(call(router, "/Items/Filters?mediaTypes=Audio", Some(token)).await).await;
    assert!(
        !private_facet["Genres"]
            .as_array()
            .unwrap()
            .contains(&json!("PrivateTagGenre"))
    );
    assert_eq!(
        call(router, &format!("/Items/{hidden}"), Some(token))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );

    // A filename fallback is lower priority than actual provider titles,
    // without demoting the embedded genres and other fields on that row.
    sqlx::query("INSERT INTO trusted_plugins(plugin_id,name,version,api_version,manifest_sha256,binary_sha256,declared_license,declared_provenance,enabled,status) VALUES ('embedded-title-test','Embedded title test','1',1,$1,$2,'MIT','Synthetic fixture',TRUE,'enabled')")
        .bind("c".repeat(64)).bind("d".repeat(64)).execute(pool).await.unwrap();
    metadata(
        pool,
        first,
        "plugin:embedded-title-test",
        json!(["Plugin genre"]),
        json!({"manifestSha256":"c".repeat(64),"moduleSha256":"d".repeat(64)}),
        None,
        None,
    )
    .await;
    sqlx::query("UPDATE item_metadata SET title='Plugin audio title' WHERE item_id=$1 AND provider_key='plugin:embedded-title-test'")
        .bind(first).execute(pool).await.unwrap();
    let actual_tag = json_body(call(router, &format!("/Items/{first}"), Some(token)).await).await;
    assert_eq!(actual_tag["Name"], "Embedded Alpha Title");
    sqlx::query("UPDATE item_metadata SET title='File fallback',metadata_json=metadata_json || '{\"titleIsFileFallback\":true}'::jsonb WHERE item_id=$1 AND provider_key='embedded-audio'")
        .bind(first).execute(pool).await.unwrap();
    let plugin_title = json_body(call(router, &format!("/Items/{first}"), Some(token)).await).await;
    assert_eq!(plugin_title["Name"], "Plugin audio title");
    assert_eq!(plugin_title["Genres"], json!(["Rock; Jazz"]));
    let plugin_search = json_body(
        call(
            router,
            &format!("{prefix}&SearchTerm=Plugin%20audio%20title"),
            Some(token),
        )
        .await,
    )
    .await;
    assert_eq!(item_ids(&plugin_search), [first]);
    let fallback_search = json_body(
        call(
            router,
            &format!("{prefix}&SearchTerm=File%20fallback"),
            Some(token),
        )
        .await,
    )
    .await;
    assert_eq!(fallback_search["TotalRecordCount"], 0);
    sqlx::query("UPDATE trusted_plugins SET enabled=FALSE,status='disabled' WHERE plugin_id='embedded-title-test'")
        .execute(pool).await.unwrap();
    let file_title = json_body(call(router, &format!("/Items/{first}"), Some(token)).await).await;
    assert_eq!(file_title["Name"], "File fallback");
    sqlx::query(
        "DELETE FROM item_metadata WHERE item_id=$1 AND provider_key='plugin:embedded-title-test'",
    )
    .bind(first)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("DELETE FROM trusted_plugins WHERE plugin_id='embedded-title-test'")
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE item_metadata SET title='Embedded Alpha Title',metadata_json=metadata_json - 'titleIsFileFallback' WHERE item_id=$1 AND provider_key='embedded-audio'")
        .bind(first).execute(pool).await.unwrap();

    metadata(
        pool,
        first,
        "local-nfo",
        json!(["Local genre"]),
        json!({"trackNumber":9,"artists":["Hidden explicit credit"]}),
        None,
        None,
    )
    .await;
    sqlx::query("UPDATE item_metadata SET title='Local title' WHERE item_id=$1 AND provider_key='local-nfo'")
        .bind(first).execute(pool).await.unwrap();
    let local = json_body(call(router, &format!("/Items/{first}"), Some(token)).await).await;
    assert_eq!(local["Name"], "Local title");
    assert_eq!(local["IndexNumber"], 9);
    assert_eq!(local["Genres"], json!(["Local genre"]));
    assert_eq!(local["Artists"], json!([]));
    let overridden = json_body(
        call(
            router,
            &format!("{prefix}&SearchTerm=Alpha%20Title"),
            Some(token),
        )
        .await,
    )
    .await;
    assert_eq!(overridden["TotalRecordCount"], 0);
    sqlx::query("DELETE FROM item_metadata WHERE item_id=$1 AND provider_key='local-nfo'")
        .bind(first)
        .execute(pool)
        .await
        .unwrap();

    // The old embedded row remains for diagnosis, but none of its fields may
    // survive a changed catalog snapshot in display, sorting or credits.
    sqlx::query("UPDATE items SET size_bytes=101 WHERE id=$1")
        .bind(first)
        .execute(pool)
        .await
        .unwrap();
    let changed = json_body(call(router, &format!("/Items/{first}"), Some(token)).await).await;
    assert_eq!(changed["Name"], "blob-a");
    assert_eq!(changed["Album"], "Folder Album");
    assert!(changed["IndexNumber"].is_null());
    assert!(changed["ProductionYear"].is_null());
    assert_eq!(changed["Genres"], json!([]));
    let no_stale_genre = json_body(
        call(
            router,
            &format!("{prefix}&Genres=Rock%3B%20Jazz"),
            Some(token),
        )
        .await,
    )
    .await;
    assert_eq!(no_stale_genre["TotalRecordCount"], 0);
    let no_stale_title = json_body(
        call(
            router,
            &format!("{prefix}&SearchTerm=Alpha%20Title"),
            Some(token),
        )
        .await,
    )
    .await;
    assert_eq!(no_stale_title["TotalRecordCount"], 0);
    // Restore this fixture's snapshot so later broad facet checks can inspect
    // the current embedded genre; no playback state is created or reset here.
    sqlx::query("UPDATE items SET size_bytes=100 WHERE id=$1")
        .bind(first)
        .execute(pool)
        .await
        .unwrap();
}

async fn call(router: &Router, path: &str, token: Option<&str>) -> Response {
    let mut request = Request::builder().uri(path).extension(ConnectInfo(
        "127.0.0.1:30000".parse::<std::net::SocketAddr>().unwrap(),
    ));
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}

async fn json_body(response: Response) -> Value {
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 1024 * 1024).await.unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    serde_json::from_slice(&bytes).unwrap()
}
