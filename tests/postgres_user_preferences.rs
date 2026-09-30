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
async fn user_preferences_are_private_persistent_and_scoped_to_each_client() {
    let database_url = env::var("PUFFINBOX_TEST_DATABASE_URL")
        .expect("set PUFFINBOX_TEST_DATABASE_URL to a disposable PostgreSQL database");
    let admin_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url)
        .await
        .unwrap();
    let schema = format!("puffinbox_preferences_test_{}", Uuid::new_v4().simple());
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
    let config = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        public_base_url: None,
        database_url,
        server_name: "Preferences test".to_owned(),
        web_root: PathBuf::from("web"),
        data_dir: env::temp_dir().join(format!("puffinbox-preferences-{schema}")),
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
    let state = AppState::new_for_run(pool.clone(), Arc::new(config), Uuid::new_v4(), run_id, None);
    let password = "Synthetic-preferences-test!";
    let password_hash = auth::hash_password(&state, password.to_owned())
        .await
        .unwrap();
    let owner = Uuid::new_v4();
    let peer = Uuid::new_v4();
    for (id, name) in [(owner, "preferences-user"), (peer, "preferences-peer")] {
        sqlx::query("INSERT INTO users(id,username,username_norm,password_hash,is_admin,enable_remote_access) VALUES ($1,$2,$2,$3,FALSE,TRUE)")
            .bind(id).bind(name).bind(&password_hash).execute(&pool).await.unwrap();
    }
    let router = api::router(state.clone());
    let login = call(
        &router,
        "POST",
        "/Users/authenticatebyname",
        None,
        Some(
            json!({"Username":"preferences-user", "Pw":password, "DeviceId":"preferences-browser"}),
        ),
    )
    .await;
    assert_eq!(login.status(), StatusCode::OK);
    let login = body_json(login).await;
    let token = login["AccessToken"].as_str().unwrap().to_owned();
    assert_eq!(login["User"]["ServerId"], state.server_id.to_string());
    assert_eq!(
        login["SessionInfo"]["ServerId"],
        state.server_id.to_string()
    );
    assert_eq!(login["SessionInfo"]["UserId"], owner.to_string());
    assert_eq!(login["SessionInfo"]["DeviceId"], "preferences-browser");
    assert!(login["User"]["Configuration"]["OrderedViews"].is_array());
    let configuration = json!({"SubtitleLanguagePreference":"eng", "SubtitleMode":"OnlyForced", "EnableNextEpisodeAutoPlay":false, "Policy":{"IsAdministrator":true}});
    assert_eq!(
        call(
            &router,
            "POST",
            "/Users/Configuration",
            Some(&token),
            Some(configuration)
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    let user = body_json(call(&router, "GET", "/Users/Me", Some(&token), None).await).await;
    assert_eq!(user["Configuration"]["SubtitleLanguagePreference"], "eng");
    assert_eq!(user["Configuration"]["SubtitleMode"], "OnlyForced");
    assert_eq!(user["Configuration"]["EnableNextEpisodeAutoPlay"], false);
    assert_eq!(
        user["Policy"]["IsAdministrator"], false,
        "configuration cannot change access policy"
    );
    assert_eq!(
        call(
            &router,
            "POST",
            &format!("/Users/Configuration?userId={peer}"),
            Some(&token),
            Some(json!({}))
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let invalid = json!({"EnableLocalPassword":true});
    assert_eq!(
        call(
            &router,
            "POST",
            "/Users/Configuration",
            Some(&token),
            Some(invalid)
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    let prefs_path = "/DisplayPreferences/usersettings?client=emby";
    let prefs = json!({"Id":"usersettings", "Client":"emby", "CustomPrefs":{"homesection0":"resume", "nullable":null}, "ShowSidebar":true});
    assert_eq!(
        call(&router, "POST", prefs_path, Some(&token), Some(prefs))
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    let stored = body_json(call(&router, "GET", prefs_path, Some(&token), None).await).await;
    assert_eq!(stored["CustomPrefs"]["homesection0"], "resume");
    assert_eq!(stored["ShowSidebar"], true);
    assert!(stored["CustomPrefs"]["nullable"].is_null());
    let other_client = body_json(
        call(
            &router,
            "GET",
            "/DisplayPreferences/usersettings?client=other",
            Some(&token),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(other_client["CustomPrefs"], json!({}));
    assert_eq!(
        call(
            &router,
            "GET",
            &format!("/DisplayPreferences/usersettings?client=emby&userId={peer}"),
            Some(&token),
            None
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        call(
            &router,
            "POST",
            &format!("/DisplayPreferences/usersettings?client=emby&userId={peer}"),
            Some(&token),
            Some(json!({}))
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    for invalid in [
        json!({"Id":"another"}),
        json!({"Client":"other"}),
        json!({"CustomPrefs":{"x":"a".repeat(4097)}}),
    ] {
        assert_eq!(
            call(&router, "POST", prefs_path, Some(&token), Some(invalid))
                .await
                .status(),
            StatusCode::BAD_REQUEST
        );
    }
    let owner_record = db::get_user(&pool, owner).await.unwrap().unwrap();
    let other_session = auth::issue_token(&state, &owner_record, "test", "phone", "phone-device")
        .await
        .unwrap();
    let other_user = body_json(
        call(
            &router,
            "GET",
            "/Users/Me",
            Some(&other_session.token),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(
        other_user["Configuration"], user["Configuration"],
        "settings survive across sessions"
    );
    let other_prefs =
        body_json(call(&router, "GET", prefs_path, Some(&other_session.token), None).await).await;
    assert_eq!(other_prefs, stored);
    let peer_record = db::get_user(&pool, peer).await.unwrap().unwrap();
    let peer_session = auth::issue_token(&state, &peer_record, "test", "phone", "peer-device")
        .await
        .unwrap();
    let peer_prefs =
        body_json(call(&router, "GET", prefs_path, Some(&peer_session.token), None).await).await;
    assert_eq!(peer_prefs["CustomPrefs"], json!({}));
    assert_eq!(
        call(&router, "GET", prefs_path, None, None).await.status(),
        StatusCode::UNAUTHORIZED
    );
    let endpoint =
        body_json(call(&router, "GET", "/System/Endpoint", Some(&token), None).await).await;
    assert_eq!(endpoint, json!({"IsLocal":true, "IsInNetwork":true}));
    let bitrate = call(
        &router,
        "GET",
        "/Playback/BitrateTest?size=70001",
        Some(&token),
        None,
    )
    .await;
    assert_eq!(bitrate.status(), StatusCode::OK);
    assert_eq!(bitrate.headers()["content-length"], "70001");
    assert_eq!(
        to_bytes(bitrate.into_body(), 100000).await.unwrap().len(),
        70001
    );
    for size in [0, 100000001] {
        assert_eq!(
            call(
                &router,
                "GET",
                &format!("/Playback/BitrateTest?size={size}"),
                Some(&token),
                None
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
    }
    db::revoke_auth_token(&pool, run_id, &auth::token_digest(&token), owner)
        .await
        .unwrap();
    assert_eq!(
        call(&router, "GET", prefs_path, Some(&token), None)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .execute(&admin_pool)
        .await
        .unwrap();
    admin_pool.close().await;
}

async fn call(
    router: &Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> Response {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("host", "media.test")
        .header("origin", "http://media.test")
        .extension(ConnectInfo(
            "127.0.0.1:20000".parse::<std::net::SocketAddr>().unwrap(),
        ));
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    let body = if let Some(body) = body {
        request = request.header("content-type", "application/json");
        Body::from(body.to_string())
    } else {
        Body::empty()
    };
    router
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap()
}

async fn body_json(response: Response) -> Value {
    assert_eq!(response.status(), StatusCode::OK);
    serde_json::from_slice(&to_bytes(response.into_body(), 128 * 1024).await.unwrap()).unwrap()
}
