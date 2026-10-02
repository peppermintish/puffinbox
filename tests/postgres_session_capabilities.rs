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
    let library_id = Uuid::new_v4();
    let track_id = Uuid::new_v4();
    sqlx::query("INSERT INTO libraries(id,name,collection_type,locations,enabled) VALUES ($1,'Playback event music','music','[\"/music\"]',TRUE)")
        .bind(library_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO items(id,library_id,name,sort_name,item_type,path,path_hash,runtime_ticks) VALUES ($1,$2,'Event track','Event track','Audio','/music/event.flac',$3,1000000000)")
        .bind(track_id)
        .bind(library_id)
        .bind(db::path_hash("/music/event.flac"))
        .execute(&pool)
        .await
        .unwrap();
    let early_progress = json!({"ItemId":track_id,"PlaySessionId":"","PositionTicks":120000000});
    let early_selector = db::PlaybackSessionSelector {
        run_id,
        user_id,
        device_id: first_device.clone(),
        id: None,
        item_id: Some(track_id),
    };
    let pending_progress = db::wait_for_playback_start(&pool, &early_selector);
    tokio::pin!(pending_progress);
    assert!(
        tokio::time::timeout(Duration::from_millis(30), &mut pending_progress)
            .await
            .is_err(),
        "native progress can arrive before start acquires its transaction lock"
    );
    let empty_start = json!({"ItemId":track_id,"PlaySessionId":"","PositionTicks":0});
    assert_eq!(
        post(
            &router,
            Some(&first.token),
            "/Sessions/Playing",
            &empty_start
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        pending_progress.await.unwrap().unwrap().item_id,
        Some(track_id)
    );
    assert_eq!(
        post(
            &router,
            Some(&first.token),
            "/Sessions/Playing/Progress",
            &early_progress
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    let early_data = db::item_user_data(&pool, user_id, &[track_id])
        .await
        .unwrap();
    assert_eq!(early_data[&track_id].playback_position_ticks, 120000000);
    assert_eq!(
        post(
            &router,
            Some(&first.token),
            "/Sessions/Playing/Stopped",
            &early_progress
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    for invalid in [" ".to_owned(), "x\ny".to_owned(), "x".repeat(257)] {
        let body = json!({"ItemId":track_id,"PlaySessionId":invalid,"PositionTicks":0});
        assert_eq!(
            post(&router, Some(&first.token), "/Sessions/Playing", &body)
                .await
                .status(),
            StatusCode::BAD_REQUEST,
        );
    }
    for opaque_id in ["1790913876274", ""] {
        let event = json!({"ItemId":track_id,"PlaySessionId":opaque_id,"PositionTicks":0,"PlayMethod":"DirectPlay"});
        for token in [&first.token, &second.token] {
            assert_eq!(
                post(&router, Some(token), "/Sessions/Playing", &event)
                    .await
                    .status(),
                StatusCode::NO_CONTENT,
                "an opaque client playback ID is scoped to its authenticated device"
            );
        }
        let playback_ids: Vec<Uuid> = sqlx::query_scalar(
            "SELECT id FROM playback_sessions WHERE user_id=$1 AND ended_at IS NULL",
        )
        .bind(user_id)
        .fetch_all(&pool)
        .await
        .unwrap();
        assert_eq!(playback_ids.len(), 2);
        assert_ne!(playback_ids[0], playback_ids[1]);
        assert_eq!(
            post(
                &router,
                Some(&peer_session.token),
                "/Sessions/Playing/Progress",
                &event
            )
            .await
            .status(),
            StatusCode::NOT_FOUND,
            "copying a client ID cannot update another user's playback"
        );
        let progress =
            json!({"ItemId":track_id,"PlaySessionId":opaque_id,"PositionTicks":120000000});
        assert_eq!(
            post(
                &router,
                Some(&first.token),
                "/Sessions/Playing/Progress",
                &progress
            )
            .await
            .status(),
            StatusCode::NO_CONTENT
        );
        let stopped =
            json!({"ItemId":track_id,"PlaySessionId":opaque_id,"PositionTicks":200000000});
        let (stop, repeated_stop) = tokio::join!(
            post(
                &router,
                Some(&first.token),
                "/Sessions/Playing/Stopped",
                &stopped
            ),
            post(
                &router,
                Some(&first.token),
                "/Sessions/Playing/Stopped",
                &stopped
            )
        );
        for response in [stop, repeated_stop] {
            assert_eq!(response.status(), StatusCode::NO_CONTENT);
        }
        let data = db::item_user_data(&pool, user_id, &[track_id])
            .await
            .unwrap();
        assert_eq!(data[&track_id].playback_position_ticks, 200000000);
        let duplicate_stop = json!({"ItemId":track_id,"PlaySessionId":opaque_id,"PositionTicks":0});
        assert_eq!(
            post(
                &router,
                Some(&first.token),
                "/Sessions/Playing/Stopped",
                &duplicate_stop
            )
            .await
            .status(),
            StatusCode::NO_CONTENT,
            "a duplicate stop is acknowledged without replacing the saved position"
        );
        let retained = db::item_user_data(&pool, user_id, &[track_id])
            .await
            .unwrap();
        assert_eq!(retained[&track_id].playback_position_ticks, 200000000);
        assert_eq!(
            post(
                &router,
                Some(&peer_session.token),
                "/Sessions/Playing/Stopped",
                &duplicate_stop
            )
            .await
            .status(),
            StatusCode::NOT_FOUND,
            "a duplicate stop cannot acknowledge another user's ended playback"
        );
        assert_eq!(
            sqlx::query_scalar::<_, i64>(
                "SELECT COUNT(*) FROM playback_sessions WHERE user_id=$1 AND ended_at IS NULL"
            )
            .bind(user_id)
            .fetch_one(&pool)
            .await
            .unwrap(),
            1,
            "stopping one device leaves the other device's session active"
        );
        assert_eq!(
            post(
                &router,
                Some(&second.token),
                "/Sessions/Playing/Stopped",
                &stopped
            )
            .await
            .status(),
            StatusCode::NO_CONTENT
        );
        assert_eq!(
            post(
                &router,
                Some(&second.token),
                "/Sessions/Playing/Progress",
                &progress
            )
            .await
            .status(),
            StatusCode::NOT_FOUND,
            "an empty or opaque ID cannot revive stopped playback"
        );
    }
    let server_id = Uuid::new_v4();
    let uuid_event = json!({"ItemId":track_id,"PlaySessionId":server_id,"PositionTicks":0});
    assert_eq!(
        post(
            &router,
            Some(&first.token),
            "/Sessions/Playing",
            &uuid_event
        )
        .await
        .status(),
        StatusCode::NO_CONTENT,
    );
    assert_eq!(
        sqlx::query_scalar::<_, Uuid>(
            "SELECT id FROM playback_sessions WHERE user_id=$1 AND ended_at IS NULL"
        )
        .bind(user_id)
        .fetch_one(&pool)
        .await
        .unwrap(),
        server_id,
        "server-issued UUIDs retain their identity"
    );
    let completed = json!({"ItemId":track_id,"PlaySessionId":server_id,"PositionTicks":980000000});
    assert_eq!(
        post(
            &router,
            Some(&first.token),
            "/Sessions/Playing/Stopped",
            &completed
        )
        .await
        .status(),
        StatusCode::NO_CONTENT,
    );
    let completed_data = db::item_user_data(&pool, user_id, &[track_id])
        .await
        .unwrap();
    assert!(completed_data[&track_id].played);
    assert_eq!(completed_data[&track_id].playback_position_ticks, 0);
    assert_eq!(
        post(
            &router,
            Some(&first.token),
            "/Sessions/Playing/Stopped",
            &uuid_event
        )
        .await
        .status(),
        StatusCode::NO_CONTENT,
        "a duplicate zero-position stop cannot erase completion"
    );
    let retained_data = db::item_user_data(&pool, user_id, &[track_id])
        .await
        .unwrap();
    assert!(retained_data[&track_id].played);
    assert_eq!(retained_data[&track_id].playback_position_ticks, 0);
    assert_eq!(
        retained_data[&track_id].play_count,
        completed_data[&track_id].play_count
    );
    assert_eq!(
        post(
            &router,
            Some(&second.token),
            "/Sessions/Playing/Stopped",
            &uuid_event
        )
        .await
        .status(),
        StatusCode::NOT_FOUND,
        "an ended UUID is still restricted to its authenticated device"
    );
    let next_track = Uuid::new_v4();
    sqlx::query("INSERT INTO items(id,library_id,name,sort_name,item_type,path,path_hash,runtime_ticks) VALUES ($1,$2,'Next event track','Next event track','Audio','/music/next.flac',$3,1000000000)")
        .bind(next_track).bind(library_id).bind(db::path_hash("/music/next.flac"))
        .execute(&pool).await.unwrap();
    let next_session = Uuid::new_v4();
    let next_event =
        json!({"ItemId":next_track,"PlaySessionId":next_session,"PositionTicks":10000000});
    assert_eq!(
        post(
            &router,
            Some(&first.token),
            "/Sessions/Playing",
            &next_event
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    for duplicate in [
        &uuid_event,
        &json!({"ItemId":track_id,"PlaySessionId":"","PositionTicks":0}),
    ] {
        assert_eq!(
            post(
                &router,
                Some(&first.token),
                "/Sessions/Playing/Stopped",
                duplicate
            )
            .await
            .status(),
            StatusCode::NO_CONTENT
        );
    }
    assert_eq!(
        db::active_playback_session(
            &pool,
            run_id,
            user_id,
            &first_device,
            Some(next_session),
            Some(next_track)
        )
        .await
        .unwrap()
        .unwrap()
        .id,
        next_session,
        "a duplicate old-track stop cannot close the newer queue item"
    );
    assert_eq!(
        post(
            &router,
            Some(&first.token),
            "/Sessions/Playing/Stopped",
            &next_event
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    sqlx::query("UPDATE libraries SET enabled=FALSE WHERE id=$1")
        .bind(library_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        post(
            &router,
            Some(&first.token),
            "/Sessions/Playing/Stopped",
            &uuid_event
        )
        .await
        .status(),
        StatusCode::NOT_FOUND,
        "an ended session cannot bypass a changed library policy"
    );
    sqlx::query("UPDATE libraries SET enabled=TRUE WHERE id=$1")
        .bind(library_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET allow_media_playback=FALSE WHERE id=$1")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        post(
            &router,
            Some(&first.token),
            "/Sessions/Playing/Stopped",
            &uuid_event
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    sqlx::query("UPDATE users SET allow_media_playback=TRUE WHERE id=$1")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
    for unknown in [
        json!({}),
        json!({"ItemId":track_id,"PlaySessionId":Uuid::new_v4()}),
    ] {
        assert_eq!(
            post(
                &router,
                Some(&first.token),
                "/Sessions/Playing/Stopped",
                &unknown
            )
            .await
            .status(),
            StatusCode::NOT_FOUND
        );
    }
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
