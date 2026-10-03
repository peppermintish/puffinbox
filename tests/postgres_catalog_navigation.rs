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
    // Instant Mix includes the seed before genuinely related music. Private,
    // hidden and rating-restricted tracks must not influence this queue.
    let genre_track = item(
        &pool,
        library,
        None,
        "Genre neighbour",
        "Audio",
        "/media/genre.flac",
        None,
    )
    .await;
    let blocked_track = item(
        &pool,
        library,
        None,
        "Restricted mix track",
        "Audio",
        "/media/blocked.flac",
        Some(100),
    )
    .await;
    let hidden_track = item(
        &pool,
        library,
        None,
        "Hidden mix track",
        "Audio",
        "/media/.private/track.flac",
        None,
    )
    .await;
    let private_track = item(
        &pool,
        private_library,
        None,
        "Private mix track",
        "Audio",
        "/media/private-mix.flac",
        None,
    )
    .await;
    for id in [
        first_track,
        genre_track,
        blocked_track,
        hidden_track,
        private_track,
    ] {
        sqlx::query("INSERT INTO item_metadata(item_id,provider_key,genres) VALUES ($1,'local-nfo',$2) ON CONFLICT(item_id,provider_key) DO UPDATE SET genres=EXCLUDED.genres")
            .bind(id).bind(json!([" Mix genre ", "", 42])).execute(&pool).await.unwrap();
    }
    let mix_path = format!("/Items/{first_track}/InstantMix");
    for path in [
        mix_path.clone(),
        format!("/Songs/{first_track}/InstantMix"),
        format!("/Albums/{first_album}/InstantMix"),
        format!("/Artists/{artist}/InstantMix"),
    ] {
        let response = call(&router, &path, Some(&owner_token)).await;
        assert_eq!(response.headers()["cache-control"], "private, no-store");
        let mix = body_json(response).await;
        assert_eq!(mix["TotalRecordCount"], 3, "{path}");
        assert_eq!(mix["StartIndex"], 0);
        assert_eq!(mix["Items"][0]["Id"], first_track.to_string());
        let ids = mix["Items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|track| track["Id"].as_str().unwrap())
            .collect::<Vec<_>>();
        for expected in [first_track, second_track, genre_track] {
            assert!(ids.contains(&expected.to_string().as_str()));
        }
        assert!(
            mix["Items"]
                .as_array()
                .unwrap()
                .iter()
                .all(|track| track["Type"] == "Audio" && track.get("Path").is_none())
        );
    }
    let genre_id: Uuid = sqlx::query_scalar("SELECT md5('puffinbox/genre/v1:Mix genre')::uuid")
        .fetch_one(&pool)
        .await
        .unwrap();
    for path in [
        "/MusicGenres/mix%20genre/InstantMix".to_owned(),
        format!("/MusicGenres/InstantMix?id={genre_id}"),
    ] {
        let response = call(&router, &path, Some(&owner_token)).await;
        assert_eq!(response.headers()["cache-control"], "private, no-store");
        let mix = body_json(response).await;
        assert_eq!(mix["TotalRecordCount"], 2);
        let ids = mix["Items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|track| track["Id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert!(ids.contains(&first_track.to_string().as_str()));
        assert!(ids.contains(&genre_track.to_string().as_str()));
    }
    for (suffix, expected) in [
        ("?limit=-1", StatusCode::BAD_REQUEST),
        ("?limit=2147483648", StatusCode::BAD_REQUEST),
        ("?imageTypeLimit=-1", StatusCode::BAD_REQUEST),
        ("?enableImageTypes=Unknown", StatusCode::BAD_REQUEST),
        ("?userId=invalid", StatusCode::BAD_REQUEST),
    ] {
        assert_eq!(
            call(&router, &format!("{mix_path}{suffix}"), Some(&owner_token))
                .await
                .status(),
            expected
        );
    }
    let options = body_json(call(&router, &format!("{mix_path}?Limit=300&Fields=Genres,Path&EnableImages=false&EnableUserData=false&EnableImageTypes=Chapter"), Some(&owner_token)).await).await;
    assert_eq!(options["TotalRecordCount"], 3);
    assert!(
        options["Items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|track| track.get("ImageTags").is_none() && track.get("UserData").is_none())
    );
    let no_page =
        body_json(call(&router, &format!("{mix_path}?limit=0"), Some(&owner_token)).await).await;
    assert_eq!(no_page["Items"], json!([]));
    assert_eq!(no_page["TotalRecordCount"], 3);
    let one =
        body_json(call(&router, &format!("{mix_path}?limit=1"), Some(&owner_token)).await).await;
    assert_eq!(one["Items"].as_array().unwrap().len(), 1);
    assert_eq!(one["TotalRecordCount"], 3);
    assert_eq!(
        call(&router, &mix_path, None).await.status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        call(&router, &mix_path, Some(&peer_token)).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            &router,
            &format!("{mix_path}?userId={peer}"),
            Some(&owner_token)
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    for path in [
        format!("/Items/{private_track}/InstantMix"),
        format!("/Items/{blocked_track}/InstantMix"),
        format!("/Items/{hidden_track}/InstantMix"),
        format!("/Songs/{first_album}/InstantMix"),
        "/MusicGenres/missing/InstantMix".to_owned(),
        format!("/MusicGenres/InstantMix?id={}", Uuid::new_v4()),
    ] {
        assert_eq!(
            call(&router, &path, Some(&owner_token)).await.status(),
            StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        call(&router, "/MusicGenres/InstantMix", Some(&owner_token))
            .await
            .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        call(
            &router,
            &format!("/Items/{movie}/InstantMix"),
            Some(&owner_token)
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    let head = router
        .clone()
        .oneshot(
            Request::builder()
                .method("HEAD")
                .uri(&mix_path)
                .header("authorization", format!("Bearer {owner_token}"))
                .extension(ConnectInfo(
                    "127.0.0.1:30000".parse::<std::net::SocketAddr>().unwrap(),
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(head.status(), StatusCode::OK);
    assert_eq!(head.headers()["cache-control"], "private, no-store");
    assert!(to_bytes(head.into_body(), 1024).await.unwrap().is_empty());

    let playlist = Uuid::new_v4();
    let empty_playlist = Uuid::new_v4();
    for id in [playlist, empty_playlist] {
        sqlx::query("INSERT INTO playlists(id,owner_user_id,name) VALUES ($1,$2,'Mix fixture')")
            .bind(id)
            .bind(peer)
            .execute(&pool)
            .await
            .unwrap();
    }
    for (position, id) in [
        second_track,
        first_track,
        first_track,
        private_track,
        blocked_track,
        hidden_track,
    ]
    .into_iter()
    .enumerate()
    {
        sqlx::query(
            "INSERT INTO playlist_items(id,playlist_id,item_id,position) VALUES($1,$2,$3,$4)",
        )
        .bind(Uuid::new_v4())
        .bind(playlist)
        .bind(id)
        .bind(position as i32)
        .execute(&pool)
        .await
        .unwrap();
    }
    let playlist_path = format!("/Playlists/{playlist}/InstantMix");
    assert_eq!(
        call(&router, &playlist_path, Some(&owner_token))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    for id in [playlist, empty_playlist] {
        sqlx::query("INSERT INTO playlist_users(playlist_id,user_id,can_edit) VALUES($1,$2,FALSE)")
            .bind(id)
            .bind(owner)
            .execute(&pool)
            .await
            .unwrap();
    }
    for path in [
        playlist_path.clone(),
        format!("/Items/{playlist}/InstantMix"),
    ] {
        let mix = body_json(call(&router, &path, Some(&owner_token)).await).await;
        assert_eq!(mix["TotalRecordCount"], 3);
        assert_eq!(mix["Items"][0]["Id"], second_track.to_string());
        assert_eq!(mix["Items"][1]["Id"], first_track.to_string());
    }
    let empty = body_json(
        call(
            &router,
            &format!("/Playlists/{empty_playlist}/InstantMix"),
            Some(&owner_token),
        )
        .await,
    )
    .await;
    assert_eq!(empty["TotalRecordCount"], 0);
    assert_eq!(empty["Items"], json!([]));
    // An administrator selecting someone else's private queue cannot borrow it.
    assert_eq!(
        call(
            &router,
            &format!("{playlist_path}?userId={owner}"),
            Some(&admin_token)
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    sqlx::query("DELETE FROM playlist_users WHERE playlist_id=$1 AND user_id=$2")
        .bind(playlist)
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&router, &playlist_path, Some(&owner_token))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    sqlx::query("UPDATE libraries SET enabled=FALSE WHERE id=$1")
        .bind(library)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&router, &mix_path, Some(&owner_token)).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            &router,
            "/MusicGenres/mix%20genre/InstantMix",
            Some(&owner_token)
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    sqlx::query("UPDATE libraries SET enabled=TRUE WHERE id=$1")
        .bind(library)
        .execute(&pool)
        .await
        .unwrap();

    sqlx::query("UPDATE items SET path='/media/.hidden-artist' WHERE id=$1")
        .bind(artist)
        .execute(&pool)
        .await
        .unwrap();
    let private_parent_mix = body_json(call(&router, &mix_path, Some(&owner_token)).await).await;
    assert_eq!(private_parent_mix["TotalRecordCount"], 2);
    assert_eq!(
        private_parent_mix["Items"][0]["ArtistItems"],
        json!([]),
        "a visible track must not disclose its hidden artist"
    );
    assert_eq!(private_parent_mix["Items"][0]["AlbumArtists"], json!([]));
    let detail = body_json(
        call(
            &router,
            &format!("/Items/{first_track}"),
            Some(&owner_token),
        )
        .await,
    )
    .await;
    assert_eq!(detail["ArtistItems"], json!([]));
    let list = body_json(
        call(
            &router,
            &format!("/Items?Ids={first_track}"),
            Some(&owner_token),
        )
        .await,
    )
    .await;
    assert_eq!(list["Items"][0]["ArtistItems"], json!([]));
    sqlx::query("UPDATE items SET path='/media/artist' WHERE id=$1")
        .bind(artist)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE items SET path='/media/.hidden-album' WHERE id=$1")
        .bind(first_album)
        .execute(&pool)
        .await
        .unwrap();
    let detail = body_json(
        call(
            &router,
            &format!("/Items/{first_track}"),
            Some(&owner_token),
        )
        .await,
    )
    .await;
    assert!(detail.get("AlbumId").is_none());
    assert!(detail.get("Album").is_none());
    assert_eq!(detail["ArtistItems"], json!([]));
    assert_eq!(
        call(
            &router,
            &format!("/Albums/{first_album}/InstantMix"),
            Some(&owner_token)
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    sqlx::query("UPDATE items SET path='/media/artist/first' WHERE id=$1")
        .bind(first_album)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO item_metadata(item_id,provider_key,policy_rating_scale,policy_rating_value) VALUES($1,'local-nfo','US-MPAA-v1',100)").bind(artist).execute(&pool).await.unwrap();
    let detail = body_json(
        call(
            &router,
            &format!("/Items/{first_track}"),
            Some(&owner_token),
        )
        .await,
    )
    .await;
    assert_eq!(detail["ArtistItems"], json!([]));
    let restricted_parent_mix = body_json(call(&router, &mix_path, Some(&owner_token)).await).await;
    assert_eq!(restricted_parent_mix["TotalRecordCount"], 2);
    sqlx::query("DELETE FROM item_metadata WHERE item_id=$1")
        .bind(artist)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET block_unrated_items=ARRAY['Music'] WHERE id=$1")
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call(&router, &mix_path, Some(&owner_token)).await.status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        call(
            &router,
            "/MusicGenres/mix%20genre/InstantMix",
            Some(&owner_token)
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    sqlx::query("UPDATE users SET block_unrated_items=ARRAY[]::text[] WHERE id=$1")
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    for ordinal in 0..101 {
        let extra = item(
            &pool,
            library,
            None,
            &format!("Extra mix {ordinal}"),
            "Audio",
            &format!("/media/extra-mix-{ordinal}.flac"),
            None,
        )
        .await;
        sqlx::query(
            "INSERT INTO item_metadata(item_id,provider_key,genres) VALUES($1,'local-nfo',$2)",
        )
        .bind(extra)
        .bind(json!(["Mix genre"]))
        .execute(&pool)
        .await
        .unwrap();
    }
    let capped = body_json(
        call(
            &router,
            &format!("{mix_path}?limit=2147483647"),
            Some(&owner_token),
        )
        .await,
    )
    .await;
    assert_eq!(capped["TotalRecordCount"], 104);
    assert_eq!(capped["Items"].as_array().unwrap().len(), 100);
    assert_eq!(capped["Items"][0]["Id"], first_track.to_string());

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

    verify_music_credit_queries(&router, &pool, owner, &owner_token, &peer_token).await;
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .execute(&admin_pool)
        .await
        .unwrap();
    admin_pool.close().await;
}

async fn verify_music_credit_queries(
    router: &Router,
    pool: &sqlx::PgPool,
    user_id: Uuid,
    token: &str,
    denied_token: &str,
) {
    let library = Uuid::new_v4();
    sqlx::query("INSERT INTO libraries(id,name,collection_type,locations) VALUES ($1,'Credit fixture','music','[\"/credits\"]')").bind(library).execute(pool).await.unwrap();
    sqlx::query("INSERT INTO user_library_access(user_id,library_id) VALUES ($1,$2)")
        .bind(user_id)
        .bind(library)
        .execute(pool)
        .await
        .unwrap();
    let folder = item(pool, library, None, "Music", "Folder", "/credits", None).await;
    let lead = item(
        pool,
        library,
        Some(folder),
        "Reference Lead Artist",
        "MusicArtist",
        "/credits/lead",
        None,
    )
    .await;
    let guest = item(
        pool,
        library,
        Some(folder),
        "Reference Guest Artist",
        "MusicArtist",
        "/credits/guest",
        None,
    )
    .await;
    let shared = item(
        pool,
        library,
        Some(lead),
        "Shared album",
        "MusicAlbum",
        "/credits/lead/shared",
        None,
    )
    .await;
    let solo = item(
        pool,
        library,
        Some(guest),
        "Guest album",
        "MusicAlbum",
        "/credits/guest/solo",
        None,
    )
    .await;
    let lead_track = item(
        pool,
        library,
        Some(shared),
        "Lead track",
        "Audio",
        "/credits/lead/shared/lead.flac",
        None,
    )
    .await;
    let guest_track = item(
        pool,
        library,
        Some(shared),
        "Guest track",
        "Audio",
        "/credits/lead/shared/guest.flac",
        None,
    )
    .await;
    let solo_track = item(
        pool,
        library,
        Some(solo),
        "Solo track",
        "Audio",
        "/credits/guest/solo/solo.flac",
        None,
    )
    .await;
    for id in [lead_track, guest_track, solo_track] {
        sqlx::query("UPDATE items SET runtime_ticks=50000000 WHERE id=$1")
            .bind(id)
            .execute(pool)
            .await
            .unwrap();
    }
    sqlx::query(
        "INSERT INTO item_metadata(item_id,provider_key,metadata_json) VALUES ($1,'local-nfo',$2)",
    )
    .bind(guest_track)
    .bind(json!({"artists":["Reference Guest Artist"],"albumArtists":["Reference Lead Artist"]}))
    .execute(pool)
    .await
    .unwrap();
    // Expectations come from the opaque Jellyfin 12 public-API fixture:
    // a guest belongs to the shared album but is not its album artist.
    for (kind, selector, artist, expected) in [
        ("Audio", "ArtistIds", lead, vec![lead_track, guest_track]),
        (
            "Audio",
            "AlbumArtistIds",
            lead,
            vec![lead_track, guest_track],
        ),
        ("Audio", "ContributingArtistIds", lead, vec![]),
        ("Audio", "ArtistIds", guest, vec![guest_track, solo_track]),
        ("Audio", "AlbumArtistIds", guest, vec![solo_track]),
        ("Audio", "ContributingArtistIds", guest, vec![guest_track]),
        ("Audio", "ExcludeArtistIds", lead, vec![solo_track]),
        ("Audio", "ExcludeArtistIds", guest, vec![lead_track]),
        ("MusicAlbum", "ArtistIds", lead, vec![shared]),
        ("MusicAlbum", "ArtistIds", guest, vec![shared, solo]),
        ("MusicAlbum", "AlbumArtistIds", guest, vec![solo]),
        ("MusicAlbum", "ContributingArtistIds", guest, vec![shared]),
        ("MusicAlbum", "ContributingArtistIds", lead, vec![]),
        ("MusicAlbum", "ExcludeArtistIds", lead, vec![solo]),
        ("MusicAlbum", "ExcludeArtistIds", guest, vec![]),
        ("MusicArtist", "ArtistIds", guest, vec![]),
        ("MusicArtist", "AlbumArtistIds", guest, vec![]),
        ("MusicArtist", "ContributingArtistIds", guest, vec![]),
        ("MusicArtist", "ExcludeArtistIds", guest, vec![lead, guest]),
    ] {
        for endpoint in ["/Items".to_owned(), format!("/Users/{user_id}/Items")] {
            let path = format!(
                "{endpoint}?ParentId={library}&Recursive=true&IncludeItemTypes={kind}&{selector}={artist}"
            );
            let result = body_json(call(router, &path, Some(token)).await).await;
            assert_eq!(
                result["TotalRecordCount"],
                expected.len(),
                "{path}: {result}"
            );
            let mut actual = result["Items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|item| Uuid::parse_str(item["Id"].as_str().unwrap()).unwrap())
                .collect::<Vec<_>>();
            actual.sort();
            let mut expected = expected.clone();
            expected.sort();
            assert_eq!(actual, expected, "{path}");
        }
    }
    let details =
        body_json(call(router, &format!("/Items/{guest_track}"), Some(token)).await).await;
    assert_eq!(details["ArtistItems"][0]["Id"], guest.to_string());
    assert_eq!(details["Artists"], json!(["Reference Guest Artist"]));
    assert_eq!(details["AlbumArtists"][0]["Id"], lead.to_string());
    assert_eq!(details["ArtistItems"].as_array().unwrap().len(), 1);
    verify_artist_role_lists(
        router,
        [library, shared, solo, lead, guest],
        token,
        denied_token,
    )
    .await;
    for path in [
        format!("/Items/{guest}/InstantMix"),
        format!("/Artists/{guest}/InstantMix"),
        format!("/Items/{guest_track}/Similar"),
    ] {
        assert_eq!(
            call(router, &path, Some(denied_token)).await.status(),
            StatusCode::NOT_FOUND,
            "{path}"
        );
    }
    // Puffinbox's recommendation rule must consume the same visible credits
    // as browsing. The guest contribution must not disappear from its seeds.
    for route in ["Items", "Artists"] {
        let mix = body_json(
            call(
                router,
                &format!("/{route}/{guest}/InstantMix?Limit=100"),
                Some(token),
            )
            .await,
        )
        .await;
        let ids = mix["Items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| item["Id"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(ids.len(), 3, "{route}: {mix}");
        assert_eq!(ids[..2], [guest_track.to_string(), solo_track.to_string()]);
        assert_eq!(ids[2], lead_track.to_string());
    }
    for (source, related, excluded) in [
        (solo_track, guest_track, vec![]),
        (guest_track, solo_track, vec![guest]),
        (solo, shared, vec![guest]),
    ] {
        let path = format!("/Items/{source}/Similar?Limit=100");
        let similar = body_json(call(router, &path, Some(token)).await).await;
        assert!(
            similar["Items"]
                .as_array()
                .unwrap()
                .iter()
                .any(|item| item["Id"] == related.to_string()),
            "{path}: {similar}"
        );
        for artist in excluded {
            let filtered = body_json(
                call(
                    router,
                    &format!("{path}&ExcludeArtistIds={artist}"),
                    Some(token),
                )
                .await,
            )
            .await;
            assert!(
                filtered["Items"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .all(|item| item["Id"] != related.to_string()),
                "{path}: {filtered}"
            );
        }
    }
    for (artist, songs, albums) in [(lead, 2, 1), (guest, 2, 2)] {
        let details = body_json(
            call(
                router,
                &format!("/Items/{artist}?Fields=ItemCounts"),
                Some(token),
            )
            .await,
        )
        .await;
        assert_eq!(details["SongCount"], songs);
        assert_eq!(details["AlbumCount"], albums);
        assert_eq!(details["ChildCount"], songs + albums);
        assert_eq!(details["RunTimeTicks"], 100000000);
    }
    let paged = body_json(call(router,&format!("/Items?ParentId={library}&Recursive=true&IncludeItemTypes=MusicAlbum&ArtistIds={guest}&StartIndex=1&Limit=1"),Some(token)).await).await;
    assert_eq!(paged["TotalRecordCount"], 2);
    assert_eq!(paged["Items"].as_array().unwrap().len(), 1);
    let combined = body_json(call(router,&format!("/Items?ParentId={library}&Recursive=true&IncludeItemTypes=MusicAlbum&AlbumArtistIds={lead}&ContributingArtistIds={guest}"),Some(token)).await).await;
    assert_eq!(combined["TotalRecordCount"], 1);
    assert_eq!(combined["Items"][0]["Id"], shared.to_string());
    for selector in [
        "ArtistIds",
        "AlbumArtistIds",
        "ContributingArtistIds",
        "ExcludeArtistIds",
    ] {
        let path = format!(
            "/Items?ParentId={library}&Recursive=true&IncludeItemTypes=MusicAlbum&{selector}={}",
            Uuid::new_v4()
        );
        let data = body_json(call(router, &path, Some(token)).await).await;
        assert_eq!(
            data["TotalRecordCount"],
            if selector == "ExcludeArtistIds" { 2 } else { 0 }
        );
        assert_eq!(
            call(router, &format!("/Items?{selector}=invalid"), Some(token))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        call(router, &format!("/Items/{guest}"), Some(denied_token))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    let denied = body_json(
        call(
            router,
            &format!("/Items?Recursive=true&ArtistIds={guest}"),
            Some(denied_token),
        )
        .await,
    )
    .await;
    assert_eq!(denied["TotalRecordCount"], 0);
    // An explicit hidden credit must not become a fallback credit or disclose
    // its name through another artist's visible album.
    sqlx::query("UPDATE items SET path='/credits/.guest' WHERE id=$1")
        .bind(guest)
        .execute(pool)
        .await
        .unwrap();
    let hidden = body_json(
        call(
            router,
            &format!("/Items?Recursive=true&ContributingArtistIds={guest}"),
            Some(token),
        )
        .await,
    )
    .await;
    assert_eq!(hidden["TotalRecordCount"], 0);
    let details =
        body_json(call(router, &format!("/Items/{guest_track}"), Some(token)).await).await;
    assert_eq!(details["Artists"], json!([]));
    assert_eq!(details["ArtistItems"], json!([]));
    assert_eq!(details["AlbumArtists"][0]["Id"], lead.to_string());
    for endpoint in ["/Artists", "/Artists/AlbumArtists"] {
        let list = body_json(
            call(
                router,
                &format!("{endpoint}?ParentId={library}"),
                Some(token),
            )
            .await,
        )
        .await;
        assert_eq!(list["TotalRecordCount"], 1);
        assert_eq!(list["Items"][0]["Id"], lead.to_string());
        assert!(!list.to_string().contains("Reference Guest Artist"));
    }
    let album = body_json(call(router, &format!("/Items/{shared}"), Some(token)).await).await;
    assert!(!album.to_string().contains("Reference Guest Artist"));
    let similar =
        body_json(call(router, &format!("/Items/{solo_track}/Similar"), Some(token)).await).await;
    assert_eq!(similar["TotalRecordCount"], 0, "{similar}");
    assert_eq!(
        call(router, &format!("/Artists/{guest}/InstantMix"), Some(token))
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    sqlx::query("UPDATE items SET path='/credits/guest' WHERE id=$1")
        .bind(guest)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO item_metadata(item_id,provider_key,policy_rating_scale,policy_rating_value) VALUES ($1,'local-nfo','US-MPAA-v1',100)").bind(guest).execute(pool).await.unwrap();
    let hidden = body_json(
        call(
            router,
            &format!("/Items?Recursive=true&ContributingArtistIds={guest}"),
            Some(token),
        )
        .await,
    )
    .await;
    assert_eq!(hidden["TotalRecordCount"], 0);
    for endpoint in ["/Artists", "/Artists/AlbumArtists"] {
        let list = body_json(
            call(
                router,
                &format!("{endpoint}?ParentId={library}&SearchTerm=Guest"),
                Some(token),
            )
            .await,
        )
        .await;
        assert_eq!(list["TotalRecordCount"], 0);
        assert_eq!(list["Items"], json!([]));
    }
    sqlx::query("DELETE FROM item_metadata WHERE item_id=$1")
        .bind(guest)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE items SET path='/credits/lead/shared/.guest.flac' WHERE id=$1")
        .bind(guest_track)
        .execute(pool)
        .await
        .unwrap();
    let details = body_json(call(router, &format!("/Items/{guest}"), Some(token)).await).await;
    assert_eq!(details["SongCount"], 1);
    assert_eq!(details["AlbumCount"], 1);
    assert_eq!(details["RunTimeTicks"], 50000000);
    for endpoint in ["/Artists", "/Artists/AlbumArtists"] {
        let list = body_json(
            call(
                router,
                &format!("{endpoint}?ParentId={library}&SearchTerm=Guest"),
                Some(token),
            )
            .await,
        )
        .await;
        assert_eq!(list["TotalRecordCount"], 1);
        assert_eq!(list["Items"][0]["Id"], guest.to_string());
        assert_eq!(list["Items"][0]["SongCount"], 1);
        assert_eq!(list["Items"][0]["AlbumCount"], 1);
        assert_eq!(list["Items"][0]["ChildCount"], 2);
    }
    let mix =
        body_json(call(router, &format!("/Artists/{guest}/InstantMix"), Some(token)).await).await;
    assert_eq!(mix["TotalRecordCount"], 1);
    assert_eq!(mix["Items"][0]["Id"], solo_track.to_string());
    let hidden = body_json(
        call(
            router,
            &format!("/Items?Recursive=true&ContributingArtistIds={guest}"),
            Some(token),
        )
        .await,
    )
    .await;
    assert_eq!(hidden["TotalRecordCount"], 0);
    sqlx::query("UPDATE items SET path='/credits/lead/shared/guest.flac' WHERE id=$1")
        .bind(guest_track)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE libraries SET enabled=FALSE WHERE id=$1")
        .bind(library)
        .execute(pool)
        .await
        .unwrap();
    let disabled = body_json(
        call(
            router,
            &format!("/Items?Recursive=true&ArtistIds={guest}"),
            Some(token),
        )
        .await,
    )
    .await;
    assert_eq!(disabled["TotalRecordCount"], 0);
    for endpoint in ["/Artists", "/Artists/AlbumArtists"] {
        let list = body_json(
            call(
                router,
                &format!("{endpoint}?SearchTerm=Reference"),
                Some(token),
            )
            .await,
        )
        .await;
        assert_eq!(list["TotalRecordCount"], 0);
        assert_eq!(list["Items"], json!([]));
    }
}

async fn verify_artist_role_lists(
    router: &Router,
    targets: [Uuid; 5],
    token: &str,
    denied_token: &str,
) {
    let [library, shared, solo, lead, guest] = targets;
    // Frozen public queries distinguish role-specific counts from the
    // artist detail's aggregate runtime. A parent album is outside its own
    // descendant selection, so its artist list has zero album counts.
    for (endpoint, library_counts, shared_counts) in [
        (
            "/Artists",
            vec![(guest, 2, 2), (lead, 1, 1)],
            vec![(guest, 1, 0), (lead, 1, 0)],
        ),
        (
            "/Artists/AlbumArtists",
            vec![(guest, 1, 1), (lead, 2, 1)],
            vec![(lead, 2, 0)],
        ),
    ] {
        for (parent, expected) in [
            (library, library_counts.clone()),
            (shared, shared_counts),
            (solo, vec![(guest, 1, 0)]),
        ] {
            let path = format!("{endpoint}?ParentId={parent}&Fields=ItemCounts");
            let result = body_json(call(router, &path, Some(token)).await).await;
            assert_eq!(
                result["TotalRecordCount"],
                expected.len(),
                "{path}: {result}"
            );
            assert_eq!(result["StartIndex"], 0);
            for (row, (id, songs, albums)) in
                result["Items"].as_array().unwrap().iter().zip(expected)
            {
                assert_eq!(row["Id"], id.to_string(), "{path}");
                assert_eq!(row["SongCount"], songs, "{path}");
                assert_eq!(row["AlbumCount"], albums, "{path}");
                assert_eq!(row["ChildCount"], songs + albums, "{path}");
                assert_eq!(row["RunTimeTicks"], 100000000, "{path}");
                assert!(row["ParentId"].is_null(), "{path}: {row}");
                assert!(row.get("Path").is_none());
            }
        }
        let search = body_json(
            call(
                router,
                &format!("{endpoint}?ParentId={library}&SearchTerm=reference%20guest"),
                Some(token),
            )
            .await,
        )
        .await;
        assert_eq!(search["TotalRecordCount"], 1);
        assert_eq!(search["Items"][0]["Id"], guest.to_string());
        assert_eq!(search["Items"][0]["SongCount"], library_counts[0].1);
        assert_eq!(search["Items"][0]["AlbumCount"], library_counts[0].2);
        for start in [0, 1, 2] {
            let page = body_json(
                call(
                    router,
                    &format!("{endpoint}?ParentId={library}&StartIndex={start}&Limit=1"),
                    Some(token),
                )
                .await,
            )
            .await;
            assert_eq!(page["TotalRecordCount"], 2);
            assert_eq!(page["StartIndex"], start);
            assert_eq!(
                page["Items"].as_array().unwrap().len(),
                usize::from(start < 2)
            );
            if start < 2 {
                assert_eq!(
                    page["Items"][0]["Id"],
                    library_counts[start as usize].0.to_string()
                );
            }
        }
        let empty = body_json(
            call(
                router,
                &format!("{endpoint}?ParentId={library}&Limit=0"),
                Some(token),
            )
            .await,
        )
        .await;
        assert_eq!(empty["TotalRecordCount"], 2);
        assert_eq!(empty["Items"], json!([]));
        let uncounted = body_json(
            call(
                router,
                &format!("{endpoint}?ParentId={library}&EnableTotalRecordCount=false"),
                Some(token),
            )
            .await,
        )
        .await;
        assert_eq!(uncounted["TotalRecordCount"], 0);
        assert_eq!(uncounted["Items"].as_array().unwrap().len(), 2);
        let literal_search = body_json(
            call(
                router,
                &format!("{endpoint}?ParentId={library}&SearchTerm=%25"),
                Some(token),
            )
            .await,
        )
        .await;
        assert_eq!(literal_search["TotalRecordCount"], 0);
        assert_eq!(
            call(
                router,
                &format!("{endpoint}?ParentId={library}"),
                Some(denied_token)
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
        let denied = body_json(
            call(
                router,
                &format!("{endpoint}?SearchTerm=Reference"),
                Some(denied_token),
            )
            .await,
        )
        .await;
        assert_eq!(denied["TotalRecordCount"], 0);
        for options in [
            "StartIndex=-1".to_owned(),
            "Limit=-1".to_owned(),
            format!("SearchTerm={}", "x".repeat(201)),
        ] {
            assert_eq!(
                call(router, &format!("{endpoint}?{options}"), Some(token))
                    .await
                    .status(),
                StatusCode::BAD_REQUEST
            );
        }
    }
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
