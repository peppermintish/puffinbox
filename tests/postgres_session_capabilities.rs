use std::{env, path::PathBuf, sync::Arc, time::Duration};

use axum::{
    Router,
    body::{Body, to_bytes},
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
async fn capabilities_persist_per_session_and_enforce_ownership_and_revocation() {
    let database_url = env::var("PUFFINBOX_TEST_DATABASE_URL")
        .expect("set PUFFINBOX_TEST_DATABASE_URL to a disposable PostgreSQL database");
    let admin_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url)
        .await
        .unwrap();
    let schema = format!("puffinbox_capabilities_test_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
        .execute(&admin_pool)
        .await
        .unwrap();
    let connection_schema = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(4)
        .acquire_timeout(Duration::from_secs(5))
        .after_connect(move |connection, _metadata| {
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
    let user_id = Uuid::new_v4();
    let peer_id = Uuid::new_v4();
    for (id, name) in [
        (user_id, "capabilities-user"),
        (peer_id, "capabilities-peer"),
    ] {
        sqlx::query("INSERT INTO users(id,username,username_norm,password_hash,is_admin,enable_remote_access) VALUES ($1,$2,$2,'unused',FALSE,TRUE)")
            .bind(id)
            .bind(name)
            .execute(&pool)
            .await
            .unwrap();
    }
    let config = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        public_base_url: None,
        database_url,
        server_name: "Capabilities test".to_owned(),
        web_root: PathBuf::from("web"),
        data_dir: env::temp_dir().join(format!("puffinbox-capabilities-{schema}")),
        ffmpeg_path: None,
        max_scan_workers: 1,
        max_page_size: 100,
        access_token_lifetime_hours: 24,
        cookie_secure: false,
        cors_origins: Vec::new(),
        trusted_proxies: Vec::new(),
        local_networks: Vec::new(),
        setup_token: None,
        bootstrap_admin_username: None,
        bootstrap_admin_password: None,
    };
    let state = AppState::new_for_run(pool.clone(), Arc::new(config), Uuid::new_v4(), run_id, None);
    let router = api::router(state.clone());
    let user = db::get_user(&pool, user_id).await.unwrap().unwrap();
    let peer = db::get_user(&pool, peer_id).await.unwrap().unwrap();
    let prefix = "opaque-device-".repeat(32);
    let first_device = format!("{prefix}one");
    let second_device = format!("{prefix}two");
    let first = auth::issue_token(&state, &user, "test", "browser", &first_device)
        .await
        .unwrap();
    let second = auth::issue_token(&state, &user, "test", "browser", &second_device)
        .await
        .unwrap();
    let peer_session = auth::issue_token(&state, &peer, "test", "peer", "peer-device")
        .await
        .unwrap();
    let capabilities = json!({
        "PlayableMediaTypes": ["Video", "Audio"],
        "SupportedCommands": ["SetVolume", "Mute"],
        "SupportsMediaControl": true,
        "SupportsPersistentIdentifier": true,
        "DeviceProfile": {"Name": "Synthetic browser", "DirectPlayProfiles": []},
        "IconUrl": "https://example.invalid/client.png"
    });
    assert_eq!(
        post(&router, None, "/Sessions/Capabilities/Full", &capabilities)
            .await
            .status(),
        StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        post(
            &router,
            Some(&first.token),
            "/Sessions/Capabilities/Full",
            &capabilities
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    let rows = db::list_auth_sessions(&pool, user_id, false).await.unwrap();
    assert_eq!(rows.len(), 2, "session listing cannot reveal another user");
    let stored = rows.iter().find(|row| row.id == first.token_id).unwrap();
    assert_eq!(stored.device_id, first_device);
    assert_eq!(
        stored.capabilities["DeviceProfile"],
        capabilities["DeviceProfile"]
    );
    assert_eq!(
        rows.iter()
            .find(|row| row.id == second.token_id)
            .unwrap()
            .device_id,
        second_device,
        "opaque identifiers sharing their first 128 bytes remain distinct"
    );
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/Sessions")
                .header("authorization", format!("Bearer {}", first.token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), StatusCode::OK);
    let body: Value =
        serde_json::from_slice(&to_bytes(response.into_body(), 128 * 1024).await.unwrap()).unwrap();
    let session = body
        .as_array()
        .unwrap()
        .iter()
        .find(|row| row["Id"] == first.token_id.to_string())
        .unwrap();
    assert_eq!(session["Capabilities"]["SupportsMediaControl"], true);
    assert_eq!(
        session["SupportsRemoteControl"], false,
        "client declarations do not enable unsupported server commands"
    );
    assert_eq!(
        post(
            &router,
            Some(&first.token),
            &format!("/Sessions/Capabilities/Full?id={}", peer_session.token_id),
            &capabilities
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        db::auth_session_by_token(&pool, &auth::token_digest(&peer_session.token))
            .await
            .unwrap()
            .unwrap()
            .capabilities,
        json!({})
    );
    let own_target = format!("/Sessions/Capabilities/Full?id={}", second.token_id);
    assert_eq!(
        post(&router, Some(&first.token), &own_target, &capabilities)
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    for invalid in [
        json!({"SupportedCommands": ["bad\ncommand"]}),
        json!({"SupportedCommands": vec!["Mute"; 129]}),
        json!({"DeviceProfile": []}),
    ] {
        assert_eq!(
            post(
                &router,
                Some(&first.token),
                "/Sessions/Capabilities/Full",
                &invalid
            )
            .await
            .status(),
            StatusCode::BAD_REQUEST
        );
    }
    db::revoke_auth_token(&pool, run_id, &auth::token_digest(&second.token), user_id)
        .await
        .unwrap();
    assert_eq!(
        post(&router, Some(&first.token), &own_target, &capabilities)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        post(
            &router,
            Some(&second.token),
            "/Sessions/Capabilities/Full",
            &capabilities
        )
        .await
        .status(),
        StatusCode::UNAUTHORIZED
    );
    let next_run = Uuid::new_v4();
    assert!(db::set_active_run_marker(&pool, next_run).await.unwrap());
    db::activate_run(&pool, next_run).await.unwrap();
    assert_eq!(
        post(
            &router,
            Some(&first.token),
            "/Sessions/Capabilities/Full",
            &capabilities
        )
        .await
        .status(),
        StatusCode::SERVICE_UNAVAILABLE
    );
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .execute(&admin_pool)
        .await
        .unwrap();
    admin_pool.close().await;
}

async fn post(router: &Router, token: Option<&str>, uri: &str, body: &Value) -> Response {
    let mut request = Request::builder()
        .method("POST")
        .uri(uri)
        .header("content-type", "application/json");
    if let Some(token) = token {
        request = request.header("authorization", format!("Bearer {token}"));
    }
    router
        .clone()
        .oneshot(request.body(Body::from(body.to_string())).unwrap())
        .await
        .unwrap()
}
