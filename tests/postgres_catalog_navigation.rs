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
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;
use uuid::Uuid;

mod common;

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via PUFFINBOX_TEST_DATABASE_URL"]
async fn navigation_and_theme_media_keep_library_rating_and_user_boundaries() {
    let database_url = env::var("PUFFINBOX_TEST_DATABASE_URL")
        .expect("set PUFFINBOX_TEST_DATABASE_URL to a disposable PostgreSQL database");
    let admin_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url)
        .await
        .unwrap();
    let schema = format!("puffinbox_navigation_test_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
        .execute(&admin_pool)
        .await
        .unwrap();
    let connection_schema = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(5))
        .after_connect(move |connection, _| {
            let schema = connection_schema.clone();
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
    let run_id = Uuid::new_v4();
    db::activate_run(&pool, run_id).await.unwrap();
    let owner = Uuid::new_v4();
    let peer = Uuid::new_v4();
    let administrator = Uuid::new_v4();
    for (id, name, is_admin) in [
        (owner, "navigation-owner", false),
        (peer, "navigation-peer", false),
        (administrator, "navigation-admin", true),
    ] {
        sqlx::query("INSERT INTO users(id,username,username_norm,password_hash,is_admin,enable_remote_access,restrict_libraries,max_parental_rating) VALUES ($1,$2,$2,'unused-synthetic-hash',$3,TRUE,TRUE,50)")
            .bind(id).bind(name).bind(is_admin).execute(&pool).await.unwrap();
    }
    let library = Uuid::new_v4();
    let private_library = Uuid::new_v4();
    for (id, name) in [
        (library, "Navigation fixture"),
        (private_library, "Private navigation fixture"),
    ] {
        db::insert_library(
            &pool,
            run_id,
            id,
            name,
            "movies",
            &[PathBuf::from("/media")],
            true,
        )
        .await
        .unwrap();
    }
    sqlx::query("INSERT INTO user_library_access(user_id,library_id) VALUES ($1,$2)")
        .bind(owner)
        .bind(library)
        .execute(&pool)
        .await
        .unwrap();
    let folder = item(
        &pool,
        library,
        None,
        "Film folder",
        "Folder",
        "/media/film",
        None,
    )
    .await;
    let movie = item(
        &pool,
        library,
        Some(folder),
        "Fixture movie",
        "Movie",
        "/media/film/movie.mp4",
        None,
    )
    .await;
    let song = item(
        &pool,
        library,
        Some(folder),
        "Opening theme",
        "Audio",
        "/media/film/theme.mp3",
        None,
    )
    .await;
    let second_song = item(
        &pool,
        library,
        Some(folder),
        "Zed theme",
        "Audio",
        "/media/film/theme-music/extra.flac",
        None,
    )
    .await;
    let video = item(
        &pool,
        library,
        Some(folder),
        "Theme video",
        "Movie",
        "/media/film/backdrops/theme.mp4",
        None,
    )
    .await;
    let soundtrack = item(
        &pool,
        library,
        Some(folder),
        "Soundtrack",
        "Audio",
        "/media/film/soundtracks/track.mp3",
        None,
    )
    .await;
    let blocked_song = item(
        &pool,
        library,
        Some(folder),
        "Restricted theme",
        "Audio",
        "/media/film/theme-music/restricted.mp3",
        Some(100),
    )
    .await;
    item(
        &pool,
        library,
        Some(folder),
        "Hidden theme",
        "Audio",
        "/media/film/theme-music/.hidden.mp3",
        None,
    )
    .await;
    item(
        &pool,
        library,
        None,
        "Unrelated theme",
        "Audio",
        "/media/other/theme.mp3",
        None,
    )
    .await;
    let private_movie = item(
        &pool,
        private_library,
        None,
        "Private movie",
        "Movie",
        "/media/film/movie.mp4",
        None,
    )
    .await;
    item(
        &pool,
        private_library,
        None,
        "Private theme",
        "Audio",
        "/media/film/theme.mp3",
        None,
    )
    .await;
    let series = item(
        &pool,
        library,
        None,
        "Series",
        "Series",
        "/media/series",
        None,
    )
    .await;
    let season = item(
        &pool,
        library,
        Some(series),
        "Season 1",
        "Season",
        "/media/series/Season 1",
        None,
    )
    .await;
    let episode = item(
        &pool,
        library,
        Some(season),
        "Episode",
        "Episode",
        "/media/series/Season 1/episode.mkv",
        None,
    )
    .await;
    let inherited_song = item(
        &pool,
        library,
        Some(series),
        "Series theme",
        "Audio",
        "/media/series/theme.mp3",
        None,
    )
    .await;
    let config = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        public_base_url: None,
        database_url,
        server_name: "Navigation test".to_owned(),
        web_root: PathBuf::from("web"),
        data_dir: env::temp_dir().join(format!("puffinbox-navigation-{schema}")),
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
    let server_id = Uuid::new_v4();
    let state = AppState::new_for_run(pool.clone(), Arc::new(config), server_id, run_id, None);
    let owner_record = db::get_user(&pool, owner).await.unwrap().unwrap();
    let owner_token =
        auth::issue_token(&state, &owner_record, "navigation-test", "fixture", "owner")
            .await
            .unwrap()
            .token;
    let peer_record = db::get_user(&pool, peer).await.unwrap().unwrap();
    let peer_token = auth::issue_token(&state, &peer_record, "navigation-test", "fixture", "peer")
        .await
        .unwrap()
        .token;
    let admin_record = db::get_user(&pool, administrator).await.unwrap().unwrap();
    let admin_token =
        auth::issue_token(&state, &admin_record, "navigation-test", "fixture", "admin")
            .await
            .unwrap()
            .token;
    let router = api::router(state);

    let legacy_path = format!("/Users/{owner}/Items/{movie}");
    let legacy = body_json(call(&router, &legacy_path, Some(&owner_token)).await).await;
    assert_eq!(legacy["Id"], movie.to_string());
    assert_eq!(legacy["ServerId"], server_id.to_string());
    assert!(legacy.get("Path").is_none());
    assert_eq!(
        call(&router, &legacy_path, Some(&peer_token))
            .await
            .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &router,
            &format!("{legacy_path}?userId={peer}"),
            Some(&owner_token)
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    let admin_as_owner = body_json(call(&router, &legacy_path, Some(&admin_token)).await).await;
    assert!(admin_as_owner.get("Path").is_none());
    for path in [
        format!("/Items/{private_movie}/Ancestors"),
        format!("/Items/{private_movie}/ThemeMedia"),
        format!("/Users/{owner}/Items/{private_movie}"),
    ] {
        assert_eq!(
            call(&router, &path, Some(&owner_token)).await.status(),
            StatusCode::NOT_FOUND
        );
    }
    let ancestors_path = format!("/Items/{episode}/Ancestors");
    let ancestors = body_json(call(&router, &ancestors_path, Some(&owner_token)).await).await;
    let ancestor_ids = ancestors
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["Id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(
        ancestor_ids,
        vec![season.to_string(), series.to_string(), library.to_string()]
    );
    assert!(
        ancestors
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["ServerId"] == server_id.to_string() && item.get("Path").is_none())
    );
    assert_eq!(
        call(
            &router,
            &format!("{ancestors_path}?userId={peer}"),
            Some(&owner_token)
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        body_json(
            call(
                &router,
                &format!("/Items/{library}/Ancestors"),
                Some(&owner_token)
            )
            .await
        )
        .await,
        json!([])
    );

    let theme_path = format!("/Items/{movie}/ThemeMedia");
    let themes = body_json(call(&router, &theme_path, Some(&owner_token)).await).await;
    assert_eq!(themes["ThemeSongsResult"]["TotalRecordCount"], 2);
    assert_eq!(themes["ThemeSongsResult"]["OwnerId"], movie.to_string());
    let song_ids = themes["ThemeSongsResult"]["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["Id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    assert_eq!(song_ids, vec![song.to_string(), second_song.to_string()]);
    assert!(!song_ids.contains(&blocked_song.to_string()));
    assert_eq!(
        themes["ThemeVideosResult"]["Items"][0]["Id"],
        video.to_string()
    );
    assert_eq!(
        themes["SoundtrackSongsResult"]["Items"][0]["Id"],
        soundtrack.to_string()
    );
    assert!(
        themes["ThemeSongsResult"]["Items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["ServerId"] == server_id.to_string() && item.get("Path").is_none())
    );
    let sorted = body_json(
        call(
            &router,
            &format!("{theme_path}?sortBy=Name&sortOrder=Descending"),
            Some(&owner_token),
        )
        .await,
    )
    .await;
    assert_eq!(
        sorted["ThemeSongsResult"]["Items"][0]["Id"],
        second_song.to_string()
    );
    let random = body_json(
        call(
            &router,
            &format!("{theme_path}?sortBy=Random&inheritFromParent=true"),
            Some(&owner_token),
        )
        .await,
    )
    .await;
    let mut random_ids = random["ThemeSongsResult"]["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| item["Id"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    random_ids.sort();
    let mut expected_ids = song_ids.clone();
    expected_ids.sort();
    assert_eq!(random_ids, expected_ids);
    assert_eq!(
        call(
            &router,
            &format!("{theme_path}?sortBy=Budget"),
            Some(&owner_token)
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            &router,
            &format!("{theme_path}?userId={peer}"),
            Some(&owner_token)
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let own_only = body_json(
        call(
            &router,
            &format!("/Items/{episode}/ThemeMedia"),
            Some(&owner_token),
        )
        .await,
    )
    .await;
    assert_eq!(own_only["ThemeSongsResult"]["TotalRecordCount"], 0);
    let inherited = body_json(
        call(
            &router,
            &format!("/Items/{episode}/ThemeMedia?inheritFromParent=true"),
            Some(&owner_token),
        )
        .await,
    )
    .await;
    assert_eq!(
        inherited["ThemeSongsResult"]["Items"][0]["Id"],
        inherited_song.to_string()
    );
    assert_eq!(inherited["ThemeSongsResult"]["OwnerId"], series.to_string());
    let similar_path = format!("/Items/{movie}/Similar");
    let collections_path = format!("/Items/{movie}/Collections");
    for path in [&similar_path, &collections_path] {
        assert_eq!(
            call(&router, path, None).await.status(),
            StatusCode::UNAUTHORIZED
        );
        assert_eq!(
            call(&router, path, Some(&peer_token)).await.status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(
                &router,
                &format!("{path}?userId={peer}"),
                Some(&owner_token)
            )
            .await
            .status(),
            StatusCode::FORBIDDEN
        );
        assert_eq!(
            call(&router, &format!("{path}?limit=-1"), Some(&owner_token))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            call(&router, &format!("{path}?limit=101"), Some(&owner_token))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            call(
                &router,
                &path.replace(&movie.to_string(), &private_movie.to_string()),
                Some(&owner_token)
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
        assert_eq!(
            call(
                &router,
                &path.replace(&movie.to_string(), &Uuid::new_v4().to_string()),
                Some(&owner_token)
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
    }
    let related_a = item(
        &pool,
        library,
        None,
        "A related film",
        "Movie",
        "/media/related-a.mp4",
        None,
    )
    .await;
    let related_b = item(
        &pool,
        library,
        None,
        "B related film",
        "Movie",
        "/media/related-b.mp4",
        None,
    )
    .await;
    let restricted_related = item(
        &pool,
        library,
        None,
        "A restricted film",
        "Movie",
        "/media/restricted-related.mp4",
        Some(100),
    )
    .await;
    let hidden_related = item(
        &pool,
        library,
        None,
        "A hidden film",
        "Movie",
        "/media/.hidden/related.mp4",
        None,
    )
    .await;
    for id in [
        movie,
        related_a,
        related_b,
        restricted_related,
        hidden_related,
        private_movie,
        song,
    ] {
        sqlx::query("INSERT INTO item_metadata(item_id,provider_key,genres) VALUES ($1,'local-nfo',$2) ON CONFLICT (item_id,provider_key) DO UPDATE SET genres=EXCLUDED.genres")
            .bind(id).bind(json!(["Synthetic genre"])).execute(&pool).await.unwrap();
    }
    let related = call(
        &router,
        &format!("{similar_path}?limit=1&fields=Path,Genres"),
        Some(&owner_token),
    )
    .await;
    assert_eq!(related.headers()["cache-control"], "private, no-store");
    let related = body_json(related).await;
    assert_eq!(related["TotalRecordCount"], 2);
    assert_eq!(related["StartIndex"], 0);
    assert_eq!(related["Items"].as_array().unwrap().len(), 1);
    assert_eq!(related["Items"][0]["Id"], related_a.to_string());
    assert_eq!(related["Items"][0]["ServerId"], server_id.to_string());
    assert!(related["Items"][0].get("Path").is_none());
    let no_page = body_json(
        call(
            &router,
            &format!("{similar_path}?limit=0"),
            Some(&owner_token),
        )
        .await,
    )
    .await;
    assert_eq!(no_page["TotalRecordCount"], 2);
    assert_eq!(no_page["Items"], json!([]));
    let admin_related = body_json(
        call(
            &router,
            &format!("{similar_path}?userId={owner}"),
            Some(&admin_token),
        )
        .await,
    )
    .await;
    assert_eq!(admin_related["TotalRecordCount"], 2);
    assert!(
        admin_related["Items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item.get("Path").is_none())
    );
    sqlx::query("UPDATE libraries SET enabled=FALSE WHERE id=$1")
        .bind(library)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&router, &similar_path, Some(&owner_token))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    sqlx::query("UPDATE libraries SET enabled=TRUE WHERE id=$1")
        .bind(library)
        .execute(&pool)
        .await
        .unwrap();
    let artist = item(
        &pool,
        library,
        None,
        "Synthetic artist",
        "MusicArtist",
        "/media/artist",
        None,
    )
    .await;
    let first_album = item(
        &pool,
        library,
        Some(artist),
        "First album",
        "MusicAlbum",
        "/media/artist/first",
        None,
    )
    .await;
    let second_album = item(
        &pool,
        library,
        Some(artist),
        "Second album",
        "MusicAlbum",
        "/media/artist/second",
        None,
    )
    .await;
    let first_track = item(
        &pool,
        library,
        Some(first_album),
        "First track",
        "Audio",
        "/media/artist/first/track.flac",
        None,
    )
    .await;
    let second_track = item(
        &pool,
        library,
        Some(second_album),
        "Second track",
        "Audio",
        "/media/artist/second/track.flac",
        None,
    )
    .await;
    let artist_related = body_json(
        call(
            &router,
            &format!("/Items/{first_track}/Similar"),
            Some(&owner_token),
        )
        .await,
    )
    .await;
    assert_eq!(artist_related["TotalRecordCount"], 1);
    assert_eq!(artist_related["Items"][0]["Id"], second_track.to_string());
    let excluded = body_json(
        call(
            &router,
            &format!("/Items/{first_track}/Similar?excludeArtistIds={artist}"),
            Some(&owner_token),
        )
        .await,
    )
    .await;
    assert_eq!(excluded["Items"], json!([]));
    assert_eq!(excluded["TotalRecordCount"], 0);
    assert_eq!(
        call(
            &router,
            &format!("/Items/{first_track}/Similar?excludeArtistIds=invalid"),
            Some(&owner_token)
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    let outer_set = item(
        &pool,
        library,
        None,
        "A collection",
        "BoxSet",
        "/media/sets",
        None,
    )
    .await;
    let inner_set = item(
        &pool,
        library,
        Some(outer_set),
        "B collection",
        "BoxSet",
        "/media/sets/inner",
        None,
    )
    .await;
    let contained = item(
        &pool,
        library,
        Some(inner_set),
        "Contained film",
        "Movie",
        "/media/sets/inner/film.mp4",
        None,
    )
    .await;
    let containing_path = format!("/Items/{contained}/Collections");
    let collections = call(
        &router,
        &format!("{containing_path}?startIndex=1&limit=1"),
        Some(&owner_token),
    )
    .await;
    assert_eq!(collections.headers()["cache-control"], "private, no-store");
    let collections = body_json(collections).await;
    assert_eq!(collections["TotalRecordCount"], 2);
    assert_eq!(collections["StartIndex"], 1);
    assert_eq!(collections["Items"].as_array().unwrap().len(), 1);
    assert_eq!(collections["Items"][0]["Id"], inner_set.to_string());
    assert!(collections["Items"][0].get("Path").is_none());
    let empty_collections =
        body_json(call(&router, &collections_path, Some(&owner_token)).await).await;
    assert_eq!(empty_collections["TotalRecordCount"], 0);
    assert_eq!(empty_collections["Items"], json!([]));
    assert_eq!(
        call(
            &router,
            &format!("{containing_path}?startIndex=-1"),
            Some(&owner_token)
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    sqlx::query("UPDATE items SET parent_id=id WHERE id=$1")
        .bind(outer_set)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&router, &containing_path, Some(&owner_token))
            .await
            .status(),
        StatusCode::CONFLICT
    );
    for path in [&ancestors_path, &theme_path, &legacy_path] {
        assert_eq!(
            call(&router, path, None).await.status(),
            StatusCode::UNAUTHORIZED
        );
    }
    sqlx::query("UPDATE items SET parent_id=id WHERE id=$1")
        .bind(folder)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(
            &router,
            &format!("/Items/{movie}/Ancestors"),
            Some(&owner_token)
        )
        .await
        .status(),
        StatusCode::CONFLICT
    );

    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .execute(&admin_pool)
        .await
        .unwrap();
    admin_pool.close().await;
}

async fn item(
    pool: &sqlx::PgPool,
    library: Uuid,
    parent: Option<Uuid>,
    name: &str,
    kind: &str,
    path: &str,
    rating: Option<i16>,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO items(id,library_id,parent_id,name,sort_name,item_type,path,path_hash) VALUES ($1,$2,$3,$4,$5,$6,$7,$8)")
        .bind(id).bind(library).bind(parent).bind(name).bind(name.to_ascii_lowercase()).bind(kind).bind(path).bind(db::path_hash(path)).execute(pool).await.unwrap();
    if let Some(rating) = rating {
        sqlx::query("INSERT INTO item_metadata(item_id,provider_key,policy_rating_scale,policy_rating_value) VALUES ($1,'local-nfo','US-MPAA-v1',$2)")
            .bind(id).bind(rating).execute(pool).await.unwrap();
    }
    id
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

async fn body_json(response: Response) -> Value {
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(&to_bytes(response.into_body(), 1024 * 1024).await.unwrap()).unwrap()
}
