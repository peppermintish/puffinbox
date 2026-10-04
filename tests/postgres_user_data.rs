use std::{env, path::PathBuf, sync::Arc};

use axum::{
    Router,
    body::{Body, to_bytes},
    extract::connect_info::ConnectInfo,
    http::{Request, StatusCode},
    response::Response,
};
use puffinbox::{AppState, Config, api, auth, db};
use serde_json::{Value, json};
use sqlx::{PgPool, postgres::PgPoolOptions};
use tower::ServiceExt;
use uuid::Uuid;

mod common;

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via PUFFINBOX_TEST_DATABASE_URL"]
async fn user_data_edits_preserve_omitted_fields_and_follow_user_and_media_permissions() {
    let database_url = env::var("PUFFINBOX_TEST_DATABASE_URL").unwrap();
    let admin_pool = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .unwrap();
    let schema = format!("puffinbox_user_data_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
        .execute(&admin_pool)
        .await
        .unwrap();
    let selected_schema = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(6)
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
    let administrator = Uuid::new_v4();
    for (id, name, admin) in [
        (owner, "data-owner", false),
        (peer, "data-peer", false),
        (administrator, "data-admin", true),
    ] {
        sqlx::query("INSERT INTO users(id,username,username_norm,password_hash,is_admin,enable_remote_access,restrict_libraries,max_parental_rating) VALUES ($1,$2,$2,'synthetic-unused',$3,TRUE,TRUE,50)")
            .bind(id).bind(name).bind(admin).execute(&pool).await.unwrap();
    }
    let library = Uuid::new_v4();
    let private = Uuid::new_v4();
    let disabled = Uuid::new_v4();
    for (id, name) in [
        (library, "Music"),
        (private, "Private"),
        (disabled, "Disabled"),
    ] {
        db::insert_library(
            &pool,
            run,
            id,
            name,
            "music",
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
    for user in [owner, peer] {
        sqlx::query("INSERT INTO user_library_access(user_id,library_id) VALUES ($1,$2)")
            .bind(user)
            .bind(library)
            .execute(&pool)
            .await
            .unwrap();
    }
    let track = item(
        &pool,
        library,
        "Audio",
        "/media/track.flac",
        Some(50_000_000),
    )
    .await;
    let movie = item(
        &pool,
        library,
        "Movie",
        "/media/movie.mp4",
        Some(50_000_000),
    )
    .await;
    let unknown = item(&pool, library, "Audio", "/media/unknown.flac", None).await;
    let hidden = item(
        &pool,
        library,
        "Audio",
        "/media/.hidden.flac",
        Some(50_000_000),
    )
    .await;
    let denied = item(
        &pool,
        private,
        "Audio",
        "/media/private.flac",
        Some(50_000_000),
    )
    .await;
    let disabled_item = item(
        &pool,
        disabled,
        "Audio",
        "/media/disabled.flac",
        Some(50_000_000),
    )
    .await;
    let restricted = item(
        &pool,
        library,
        "Audio",
        "/media/restricted.flac",
        Some(50_000_000),
    )
    .await;
    sqlx::query("INSERT INTO item_metadata(item_id,provider_key,metadata_json,policy_rating_scale,policy_rating_value) VALUES ($1,'local-nfo','{}','US-MPAA-v1',90)")
        .bind(restricted).execute(&pool).await.unwrap();
    let config = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        public_base_url: None,
        database_url,
        server_name: "User data test".to_owned(),
        web_root: PathBuf::from("web"),
        data_dir: env::temp_dir().join(format!("puffinbox-user-data-{schema}")),
        ffmpeg_path: None,
        max_scan_workers: 1,
        max_page_size: 100,
        access_token_lifetime_hours: 24,
        cookie_secure: false,
        cors_origins: Vec::new(),
        trusted_proxies: Vec::new(),
        local_networks: vec!["127.0.0.0/8".parse().unwrap()],
        dlna: Default::default(),
        setup_token: None,
        bootstrap_admin_username: None,
        bootstrap_admin_password: None,
    };
    let state = AppState::new_for_run(pool.clone(), Arc::new(config), Uuid::new_v4(), run, None);
    let mut tokens = Vec::new();
    for id in [owner, peer, administrator] {
        let user = db::get_user(&pool, id).await.unwrap().unwrap();
        tokens.push(
            auth::issue_token(&state, &user, "test", "test", &id.to_string())
                .await
                .unwrap()
                .token,
        );
    }
    let router = api::router(state.clone());
    let path = format!("/UserItems/{track}/UserData");
    let data = body(
        call(
            &router,
            "POST",
            &path,
            Some(&tokens[0]),
            Some(json!({"Played":true})),
        )
        .await,
    )
    .await;
    assert_eq!(data["Played"], true);
    assert_eq!(
        data["PlayCount"], 0,
        "an explicit played flag is not a playback event"
    );
    assert!(data["LastPlayedDate"].is_null());
    assert!(data["PlayedPercentage"].is_null());
    let values = json!({"Played":false,"IsFavorite":true,"PlaybackPositionTicks":12345678,"PlayCount":7,"LastPlayedDate":"2024-06-07T10:09:10+02:00","Likes":true});
    let saved = body(call(&router, "POST", &path, Some(&tokens[0]), Some(values)).await).await;
    assert_eq!(saved["PlayCount"], 7);
    assert_eq!(saved["LastPlayedDate"], "2024-06-07T08:09:10Z");
    assert_eq!(saved["Rating"], 10.0);
    assert_eq!(saved["Likes"], true);
    assert!((saved["PlayedPercentage"].as_f64().unwrap() - 24.691356).abs() < 1e-9);
    for alias in [
        format!("/Items/{track}/UserData"),
        format!("/UserItems/{track}"),
        path.clone(),
    ] {
        assert_eq!(
            body(call(&router, "GET", &alias, Some(&tokens[0]), None).await).await,
            saved
        );
    }
    for payload in [
        json!({}),
        json!({"Played":null,"IsFavorite":null,"PlaybackPositionTicks":null,"PlayCount":null,"LastPlayedDate":null,"Rating":null,"Likes":null}),
        json!({"PlayedPercentage":85,"UnplayedItemCount":12,"Key":"not-a-selector","ItemId":"not-a-uuid"}),
    ] {
        assert_eq!(
            body(call(&router, "POST", &path, Some(&tokens[0]), Some(payload)).await).await,
            saved
        );
    }
    let changed = body(
        call(
            &router,
            "POST",
            &path,
            Some(&tokens[0]),
            Some(json!({"playCount":3,"lastPlayedDate":"2023-02-03T04:05:06Z"})),
        )
        .await,
    )
    .await;
    assert_eq!(changed["PlayCount"], 3);
    assert_eq!(changed["LastPlayedDate"], "2023-02-03T04:05:06Z");
    for key in [
        "Played",
        "IsFavorite",
        "PlaybackPositionTicks",
        "Rating",
        "Likes",
    ] {
        assert_eq!(changed[key], saved[key]);
    }
    for (payload, rating, likes) in [
        (json!({"Likes":false}), 1.0, false),
        (json!({"Rating":6.4999}), 6.4999, false),
        (json!({"Rating":6.5}), 6.5, true),
        (json!({"Rating":8.5,"Likes":false}), 8.5, true),
        (json!({"Rating":0}), 0.0, false),
    ] {
        let data = body(call(&router, "POST", &path, Some(&tokens[0]), Some(payload)).await).await;
        assert_eq!(data["Rating"].as_f64(), Some(rating));
        assert_eq!(data["Likes"], likes);
        assert_eq!(data["PlayCount"], 3);
    }
    for (position, percent) in [
        (0, None),
        (10_000_000, Some(20.0)),
        (80_000_000, Some(160.0)),
    ] {
        let data = body(
            call(
                &router,
                "POST",
                &path,
                Some(&tokens[0]),
                Some(json!({"Played":true,"PlaybackPositionTicks":position})),
            )
            .await,
        )
        .await;
        assert_eq!(data["PlayedPercentage"].as_f64(), percent);
        assert_eq!(data["PlayCount"], 3);
    }
    let unknown_data = body(
        call(
            &router,
            "POST",
            &format!("/UserItems/{unknown}/UserData"),
            Some(&tokens[0]),
            Some(json!({"PlayedPercentage":85})),
        )
        .await,
    )
    .await;
    assert_eq!(unknown_data["PlaybackPositionTicks"], 0);
    assert!(unknown_data["PlayedPercentage"].is_null());

    let before = body(call(&router, "GET", &path, Some(&tokens[0]), None).await).await;
    for payload in [
        json!({"PlayCount":-1}),
        json!({"PlaybackPositionTicks":-1}),
        json!({"PlaybackPositionTicks":3155760000000001_i64}),
        json!({"Rating":-0.0001}),
        json!({"Rating":10.0001}),
        json!({"Rating":1e100}),
    ] {
        assert_eq!(
            call(&router, "POST", &path, Some(&tokens[0]), Some(payload))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
        assert_eq!(
            body(call(&router, "GET", &path, Some(&tokens[0]), None).await).await,
            before
        );
    }
    assert_eq!(
        call(&router, "POST", &path, None, Some(json!({"PlayCount":4})))
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    for id in [hidden, denied, disabled_item, restricted, Uuid::new_v4()] {
        let denied_path = format!("/UserItems/{id}/UserData");
        for method in ["GET", "POST"] {
            assert_eq!(
                call(
                    &router,
                    method,
                    &denied_path,
                    Some(&tokens[0]),
                    (method == "POST").then(|| json!({"Rating":9,"PlayCount":4}))
                )
                .await
                .status(),
                StatusCode::NOT_FOUND
            );
        }
    }
    assert_eq!(
        call(
            &router,
            "POST",
            &format!("{path}?userId={peer}"),
            Some(&tokens[0]),
            Some(json!({"PlayCount":4}))
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let peer_data = body(call(&router, "GET", &path, Some(&tokens[1]), None).await).await;
    assert_eq!(peer_data["PlayCount"], 0);
    assert!(peer_data["Rating"].is_null());
    let administered = body(
        call(
            &router,
            "POST",
            &format!("{path}?userId={peer}"),
            Some(&tokens[2]),
            Some(json!({"PlayCount":4,"Rating":9})),
        )
        .await,
    )
    .await;
    assert_eq!(administered["PlayCount"], 4);
    assert_eq!(
        body(call(&router, "GET", &path, Some(&tokens[1]), None).await).await,
        administered
    );
    assert_eq!(
        body(call(&router, "GET", &path, Some(&tokens[0]), None).await).await,
        before
    );

    personal_catalog_filters(
        &pool,
        &router,
        library,
        owner,
        peer,
        &tokens,
        &[hidden, denied, disabled_item, restricted],
    )
    .await;

    let count_edit = call(
        &router,
        "POST",
        &path,
        Some(&tokens[0]),
        Some(json!({"PlayCount":11})),
    );
    let rating_edit = call(
        &router,
        "POST",
        &path,
        Some(&tokens[0]),
        Some(json!({"Rating":8.5})),
    );
    let (count_response, rating_response) = tokio::join!(count_edit, rating_edit);
    assert_eq!(count_response.status(), StatusCode::OK);
    assert_eq!(rating_response.status(), StatusCode::OK);
    let combined = body(call(&router, "GET", &path, Some(&tokens[0]), None).await).await;
    assert_eq!(combined["PlayCount"], 11);
    assert_eq!(
        combined["Rating"], 8.5,
        "concurrent partial edits retain each other's fields"
    );
    assert_eq!(combined["LastPlayedDate"], before["LastPlayedDate"]);
    for id in [track, movie] {
        let data_path = format!("/UserItems/{id}/UserData");
        body(
            call(
                &router,
                "POST",
                &data_path,
                Some(&tokens[0]),
                Some(json!({"Played":false,"PlayCount":2147483647,"Rating":8.5})),
            )
            .await,
        )
        .await;
        let play_id = Uuid::new_v4();
        for (endpoint, ticks) in [
            ("/Sessions/Playing", 0),
            ("/Sessions/Playing/Stopped", 50_000_000),
        ] {
            assert_eq!(call(&router, "POST", endpoint, Some(&tokens[0]), Some(json!({"ItemId":id,"PlaySessionId":play_id,"PositionTicks":ticks,"PlayedToCompletion":true}))).await.status(), StatusCode::NO_CONTENT);
        }
        let data = body(call(&router, "GET", &data_path, Some(&tokens[0]), None).await).await;
        assert_eq!(
            data["PlayCount"], 2147483647,
            "playback cannot overflow an edited maximum count"
        );
        assert_eq!(data["Rating"], 8.5);
        body(
            call(
                &router,
                "POST",
                &data_path,
                Some(&tokens[0]),
                Some(json!({"Played":false})),
            )
            .await,
        )
        .await;
        assert_eq!(
            body(
                call(
                    &router,
                    "POST",
                    &format!("/UserPlayedItems/{id}"),
                    Some(&tokens[0]),
                    None
                )
                .await
            )
            .await["PlayCount"],
            2147483647
        );
    }
    let new_run = Uuid::new_v4();
    let persisted = body(call(&router, "GET", &path, Some(&tokens[0]), None).await).await;
    db::set_active_run_marker(&pool, new_run).await.unwrap();
    db::activate_run(&pool, new_run).await.unwrap();
    let restarted = AppState::new_for_run(
        pool.clone(),
        state.config.clone(),
        state.server_id,
        new_run,
        None,
    );
    let restarted_router = api::router(restarted);
    assert_eq!(
        body(call(&restarted_router, "GET", &path, Some(&tokens[0]), None).await).await,
        persisted
    );
    assert_eq!(
        call(
            &router,
            "POST",
            &path,
            Some(&tokens[0]),
            Some(json!({"PlayCount":1}))
        )
        .await
        .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    assert_eq!(
        body(call(&restarted_router, "GET", &path, Some(&tokens[0]), None).await).await,
        persisted
    );
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .execute(&admin_pool)
        .await
        .unwrap();
}

async fn personal_catalog_filters(
    pool: &PgPool,
    router: &Router,
    library: Uuid,
    owner: Uuid,
    peer: Uuid,
    tokens: &[String],
    invisible: &[Uuid],
) {
    let album = item(pool, library, "MusicAlbum", "/media/Filter Album", None).await;
    let mut tracks = Vec::new();
    for (name, title) in [
        ("a", "Filter Alpha"),
        ("b", "Filter Beta"),
        ("c", "Filter Charlie"),
        ("d", "Filter Delta"),
        ("e", "Filter Empty"),
    ] {
        let track = item(
            pool,
            library,
            "Audio",
            &format!("/media/Filter Album/{name}.flac"),
            Some(50_000_000),
        )
        .await;
        sqlx::query("UPDATE items SET parent_id=$1,name=$3 WHERE id=$2")
            .bind(album)
            .bind(track)
            .bind(title)
            .execute(pool)
            .await
            .unwrap();
        tracks.push(track);
    }
    let prefix =
        format!("/Items?ParentId={album}&Recursive=true&IncludeItemTypes=Audio&SortBy=SortName");
    selection(
        router,
        &format!("{prefix}&Filters=Likes"),
        &tokens[0],
        &[],
        0,
    )
    .await;
    selection(
        router,
        &format!("{prefix}&Filters=Dislikes"),
        &tokens[0],
        &tracks,
        5,
    )
    .await;
    for (id, payload) in [
        (
            tracks[0],
            json!({"Rating":6.5,"IsFavorite":false,"Played":true}),
        ),
        (tracks[1], json!({"Rating":6.4999,"IsFavorite":true})),
        (
            tracks[2],
            json!({"Rating":0,"IsFavorite":false,"Played":true}),
        ),
        (tracks[3], json!({"IsFavorite":true})),
    ] {
        body(
            call(
                router,
                "POST",
                &format!("/UserItems/{id}/UserData"),
                Some(&tokens[0]),
                Some(payload),
            )
            .await,
        )
        .await;
    }
    for (filters, expected) in [
        ("Likes", vec![tracks[0]]),
        ("likes,Likes", vec![tracks[0]]),
        ("Dislikes", tracks[1..].to_vec()),
        ("IsFavoriteOrLikes", vec![tracks[1], tracks[3]]),
        ("Likes,IsFavorite", vec![]),
        ("IsFavorite,Likes", vec![]),
        ("Dislikes,IsFavorite", vec![tracks[1], tracks[3]]),
        ("IsFavoriteOrLikes,Dislikes", vec![tracks[1], tracks[3]]),
        ("IsFavoriteOrLikes,Likes", vec![]),
        ("IsFavorite,IsFavoriteOrLikes", vec![tracks[1], tracks[3]]),
        ("IsPlayed,Likes", vec![tracks[0]]),
        ("IsUnplayed,IsFavoriteOrLikes", vec![tracks[1], tracks[3]]),
        ("Dislikes,IsResumable", vec![]),
    ] {
        selection(
            router,
            &format!("{prefix}&Filters={filters}"),
            &tokens[0],
            &expected,
            expected.len(),
        )
        .await;
    }
    selection(
        router,
        &format!("{prefix}&Filters=Dislikes&StartIndex=1&Limit=1"),
        &tokens[0],
        &[tracks[2]],
        4,
    )
    .await;
    selection(
        router,
        &format!("{prefix}&Filters=Dislikes&ExcludeItemIds={}", tracks[1]),
        &tokens[0],
        &tracks[2..],
        3,
    )
    .await;
    selection(
        router,
        &format!("{prefix}&filters=Likes&searchTerm=Alpha"),
        &tokens[0],
        &[tracks[0]],
        1,
    )
    .await;
    selection(
        router,
        &format!("{prefix}&Filters=Likes"),
        &tokens[1],
        &[],
        0,
    )
    .await;
    selection(
        router,
        &format!("{prefix}&Filters=Likes"),
        &tokens[2],
        &[],
        0,
    )
    .await;
    selection(
        router,
        &format!("{prefix}&Filters=Likes&userId={owner}"),
        &tokens[2],
        &[tracks[0]],
        1,
    )
    .await;
    for filters in ["Likes,Dislikes", "Dislikes,Likes"] {
        assert_eq!(
            call(
                router,
                "GET",
                &format!("{prefix}&Filters={filters}"),
                Some(&tokens[0]),
                None
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
    }
    body(
        call(
            router,
            "POST",
            &format!("/UserItems/{}/UserData", tracks[0]),
            Some(&tokens[0]),
            Some(json!({"Rating":10})),
        )
        .await,
    )
    .await;
    selection(
        router,
        &format!("{prefix}&Filters=IsFavoriteOrLikes"),
        &tokens[0],
        &[tracks[1], tracks[3]],
        2,
    )
    .await;
    for id in invisible {
        sqlx::query(
            "INSERT INTO user_item_data(user_id,item_id,rating,is_favorite) VALUES ($1,$2,10,TRUE)",
        )
        .bind(owner)
        .bind(id)
        .execute(pool)
        .await
        .unwrap();
    }
    let ids = std::iter::once(&tracks[0])
        .chain(invisible)
        .map(Uuid::to_string)
        .collect::<Vec<_>>()
        .join(",");
    selection(
        router,
        &format!("/Items?Ids={ids}&Recursive=true&Filters=Likes"),
        &tokens[0],
        &[tracks[0]],
        1,
    )
    .await;
    let legacy = prefix.replacen("/Items?", &format!("/Users/{owner}/Items?"), 1);
    selection(
        router,
        &format!("{legacy}&Filters=Dislikes"),
        &tokens[0],
        &tracks[1..],
        4,
    )
    .await;
    for (path, expected) in [
        (
            "/Items?IncludeItemTypes=Playlist&Filters=Likes".to_owned(),
            StatusCode::BAD_REQUEST,
        ),
        (
            format!("{prefix}&Filters=IsFavoriteOrLikes&userId={peer}"),
            StatusCode::FORBIDDEN,
        ),
    ] {
        assert_eq!(
            call(router, "GET", &path, Some(&tokens[0]), None)
                .await
                .status(),
            expected
        );
    }
}

async fn selection(router: &Router, path: &str, token: &str, expected: &[Uuid], total: usize) {
    let result = body(call(router, "GET", path, Some(token), None).await).await;
    let ids = result["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["Id"].as_str().unwrap().parse::<Uuid>().unwrap())
        .collect::<Vec<_>>();
    assert_eq!(ids, expected, "{path}");
    assert_eq!(result["TotalRecordCount"], total, "{path}");
}

async fn item(pool: &PgPool, library: Uuid, kind: &str, path: &str, runtime: Option<i64>) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO items(id,library_id,name,sort_name,item_type,path,path_hash,runtime_ticks) VALUES ($1,$2,$3,$3,$4,$3,$5,$6)")
        .bind(id).bind(library).bind(path).bind(kind).bind(db::path_hash(path)).bind(runtime).execute(pool).await.unwrap();
    id
}

async fn call(
    router: &Router,
    method: &str,
    path: &str,
    token: Option<&str>,
    payload: Option<Value>,
) -> Response {
    let mut request = Request::builder()
        .method(method)
        .uri(path)
        .extension(ConnectInfo(
            "127.0.0.1:30000".parse::<std::net::SocketAddr>().unwrap(),
        ));
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let body = if let Some(payload) = payload {
        request = request.header("content-type", "application/json");
        Body::from(payload.to_string())
    } else {
        Body::empty()
    };
    router
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap()
}

async fn body(response: Response) -> Value {
    let status = response.status();
    let bytes = to_bytes(response.into_body(), 128 * 1024).await.unwrap();
    assert_eq!(
        status,
        StatusCode::OK,
        "{}",
        String::from_utf8_lossy(&bytes)
    );
    serde_json::from_slice(&bytes).unwrap()
}
