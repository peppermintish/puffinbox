use std::{env, fs, net::SocketAddr, path::PathBuf, str::FromStr, sync::Arc, time::Duration};

use axum::{
    body::{Body, to_bytes},
    http::{Request, StatusCode},
};
use ipnet::IpNet;
use puffinbox::{AppState, Config, api, auth, db, library};
use sqlx::{
    PgPool,
    postgres::{PgConnectOptions, PgPoolOptions},
};
use tower::ServiceExt;
use uuid::Uuid;

mod common;

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via PUFFINBOX_TEST_DATABASE_URL"]
async fn server_run_fences_playback_and_scans_and_scan_reconciliation_fails_safe() {
    let database_url = env::var("PUFFINBOX_TEST_DATABASE_URL")
        .expect("set PUFFINBOX_TEST_DATABASE_URL to a disposable PostgreSQL database");
    let admin_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url)
        .await
        .unwrap();
    let lock_database = format!("puffinbox_lock_test_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE DATABASE \"{lock_database}\""))
        .execute(&admin_pool)
        .await
        .expect("the disposable PostgreSQL test user must be allowed to create databases");
    let instance_lock_pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(
            PgConnectOptions::from_str(&database_url)
                .unwrap()
                .database(&lock_database),
        )
        .await
        .unwrap();
    assert!(
        !db::set_active_run_marker(&instance_lock_pool, Uuid::new_v4())
            .await
            .unwrap(),
        "first startup skips run fencing when the metadata table is absent"
    );
    let competing_lock_pool = PgPoolOptions::new()
        .max_connections(2)
        .acquire_timeout(Duration::from_secs(5))
        .connect_with(
            PgConnectOptions::from_str(&database_url)
                .unwrap()
                .database(&lock_database),
        )
        .await
        .unwrap();
    let schema = format!("puffinbox_server_guard_test_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
        .execute(&admin_pool)
        .await
        .unwrap();

    let connection_schema = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(8)
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
    let old_run = Uuid::new_v4();
    db::activate_run(&pool, old_run).await.unwrap();
    let mut lock_owner = db::try_server_instance_lock(&instance_lock_pool)
        .await
        .unwrap()
        .expect("first server process may claim the instance lock");
    let owner_backend_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut lock_owner)
        .await
        .unwrap();
    assert!(
        db::try_server_instance_lock(&competing_lock_pool)
            .await
            .unwrap()
            .is_none(),
        "a competing process must fail before it can run migrations"
    );
    let competing_backend_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&competing_lock_pool)
        .await
        .unwrap();
    assert_ne!(owner_backend_pid, competing_backend_pid);
    // The owner is a detached connection. Dropping it closes that PostgreSQL
    // backend and releases the session lock instead of returning a locked
    // backend to the first pool.
    drop(lock_owner);
    let mut reacquired_lock = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if let Some(lock) = db::try_server_instance_lock(&competing_lock_pool)
                .await
                .unwrap()
            {
                break lock;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
    })
    .await
    .expect("the dropped owner connection should release the lock");
    let reacquired_backend_pid: i32 = sqlx::query_scalar("SELECT pg_backend_pid()")
        .fetch_one(&mut reacquired_lock)
        .await
        .unwrap();
    assert_ne!(owner_backend_pid, reacquired_backend_pid);
    let reacquired_unlocked: bool = sqlx::query_scalar("SELECT pg_advisory_unlock($1)")
        .bind(db::SERVER_INSTANCE_LOCK_KEY)
        .fetch_one(&mut reacquired_lock)
        .await
        .unwrap();
    assert!(reacquired_unlocked);
    sqlx::Connection::close(reacquired_lock).await.unwrap();
    let mut explicit_release_lock = db::try_server_instance_lock(&instance_lock_pool)
        .await
        .unwrap()
        .expect("a different pool can acquire after explicit unlock and close");
    let explicit_release_unlocked: bool = sqlx::query_scalar("SELECT pg_advisory_unlock($1)")
        .bind(db::SERVER_INSTANCE_LOCK_KEY)
        .fetch_one(&mut explicit_release_lock)
        .await
        .unwrap();
    assert!(explicit_release_unlocked);
    sqlx::Connection::close(explicit_release_lock)
        .await
        .unwrap();

    let paths = TestPaths::new();
    let media_root = paths.path.join("media");
    fs::create_dir_all(&media_root).unwrap();
    let media_file = media_root.join("fixture.mkv");
    fs::write(&media_file, b"scan fixture").unwrap();
    let raced_root = paths.path.join("raced");
    fs::create_dir_all(&raced_root).unwrap();

    let library_id = Uuid::new_v4();
    db::insert_library(
        &pool,
        old_run,
        library_id,
        "Scan fixture",
        "movies",
        std::slice::from_ref(&media_root),
        true,
    )
    .await
    .unwrap();
    let fence_library_id = Uuid::new_v4();
    db::insert_library(
        &pool,
        old_run,
        fence_library_id,
        "Run fence fixture",
        "movies",
        &[PathBuf::from("/nonexistent/puffinbox-scan-fence")],
        true,
    )
    .await
    .unwrap();
    let raced_library_id = Uuid::new_v4();
    db::insert_library(
        &pool,
        old_run,
        raced_library_id,
        "Concurrent scan fence fixture",
        "movies",
        std::slice::from_ref(&raced_root),
        true,
    )
    .await
    .unwrap();
    let user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,username,username_norm,password_hash,is_admin,enable_remote_access) VALUES ($1,'guard-test','guard-test','unused',TRUE,TRUE)")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
    let viewer_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,username,username_norm,password_hash,enable_remote_access) VALUES ($1,'guard-viewer','guard-viewer','unused',TRUE)")
        .bind(viewer_id)
        .execute(&pool)
        .await
        .unwrap();

    let old_scan_id = Uuid::new_v4();
    assert_eq!(
        db::claim_library_scan(&pool, old_run, fence_library_id, old_scan_id)
            .await
            .unwrap(),
        db::LibraryScanClaim::Claimed
    );

    let state = AppState::new_for_run(
        pool.clone(),
        Arc::new(test_config(database_url.clone())),
        Uuid::new_v4(),
        old_run,
        None,
    );
    let first_scan = run_scan(&state, library_id).await;
    assert_eq!(first_scan.status, "completed");
    assert_eq!(first_scan.errors, 0);
    let movie_id: Uuid =
        sqlx::query_scalar("SELECT id FROM items WHERE path=$1 AND item_type='Movie'")
            .bind(media_file.to_str().unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();

    let live_source_id = Uuid::new_v4();
    let live_item_id = Uuid::new_v4();
    let live_path = format!("puffinbox://livetv/{live_source_id}/synthetic-channel");
    sqlx::query("INSERT INTO live_tv_sources(id,library_id,name,playlist_url,origin_pins) VALUES ($1,$2,'Synthetic Live Source','https://example.invalid/guide.m3u','[]'::JSONB)")
        .bind(live_source_id)
        .bind(library_id)
        .execute(&pool)
        .await
        .unwrap();
    let mut virtual_item_tx = pool.begin().await.unwrap();
    db::upsert_virtual_item(
        &mut virtual_item_tx,
        old_run,
        db::VirtualItemInput {
            library_id,
            item_id: live_item_id,
            name: "Synthetic Channel",
            item_type: "LiveTvChannel",
            opaque_path: &live_path,
            overview: Some("A synthetic guide entry"),
            metadata_json: &serde_json::json!({"sourceId": live_source_id}),
        },
    )
    .await
    .unwrap();
    let unsafe_path_item_id = Uuid::new_v4();
    let mut unsafe_path_tx = pool.begin().await.unwrap();
    assert!(
        db::upsert_virtual_item(
            &mut unsafe_path_tx,
            old_run,
            db::VirtualItemInput {
                library_id,
                item_id: unsafe_path_item_id,
                name: "Invalid file-backed channel",
                item_type: "LiveTvChannel",
                opaque_path: "/etc/passwd",
                overview: None,
                metadata_json: &serde_json::json!({}),
            },
        )
        .await
        .is_err()
    );
    unsafe_path_tx.rollback().await.unwrap();
    let unsafe_path_inserted: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM items WHERE id=$1)")
            .bind(unsafe_path_item_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        !unsafe_path_inserted,
        "opaque items cannot insert filesystem paths"
    );
    sqlx::query("INSERT INTO live_tv_channels(item_id,library_id,source_id,source_channel_id,name,stream_url,enabled) VALUES ($1,$2,$3,'synthetic-channel','Synthetic Channel','https://example.invalid/live.m3u8',FALSE)")
        .bind(live_item_id)
        .bind(library_id)
        .bind(live_source_id)
        .execute(&mut *virtual_item_tx)
        .await
        .unwrap();
    virtual_item_tx.commit().await.unwrap();

    let admin_record = db::get_user(&pool, user_id).await.unwrap().unwrap();
    let live_query = library::ItemQuery {
        include_item_types: vec!["LiveTvChannel".to_owned()],
        recursive: true,
        limit: 100,
        enable_total_record_count: true,
        ..Default::default()
    };
    let (disabled_channels, disabled_channel_count) =
        db::browse_items(&pool, &admin_record, live_query.clone())
            .await
            .unwrap();
    assert!(disabled_channels.is_empty());
    assert_eq!(disabled_channel_count, Some(0));
    let virtual_item = db::get_item(&pool, live_item_id).await.unwrap().unwrap();
    assert!(
        !db::item_visible_to_user(&pool, &admin_record, &virtual_item)
            .await
            .unwrap()
    );
    sqlx::query("UPDATE live_tv_channels SET enabled=TRUE WHERE item_id=$1")
        .bind(live_item_id)
        .execute(&pool)
        .await
        .unwrap();
    let (enabled_channels, enabled_channel_count) =
        db::browse_items(&pool, &admin_record, live_query)
            .await
            .unwrap();
    assert_eq!(enabled_channels.len(), 1);
    assert_eq!(enabled_channels[0].id, live_item_id);
    assert_eq!(enabled_channel_count, Some(1));

    fs::remove_file(&media_file).unwrap();
    let deletion_scan = run_scan(&state, library_id).await;
    assert_eq!(deletion_scan.status, "completed");
    let deleted_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM items WHERE id=$1")
        .bind(movie_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(deleted_rows, 0, "successful scans reconcile deleted media");
    let live_row_survived_scan: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM items WHERE id=$1 AND item_type='LiveTvChannel' AND path=$2)",
    )
    .bind(live_item_id)
    .bind(&live_path)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        live_row_survived_scan,
        "filesystem reconciliation must retain virtual channel identities"
    );

    fs::write(&media_file, b"restored fixture").unwrap();
    let restored_scan = run_scan(&state, library_id).await;
    assert_eq!(restored_scan.status, "completed");
    let old_session_id = Uuid::new_v4();
    assert_eq!(
        db::start_playback_session(
            &pool,
            playback_request(old_session_id, old_run, user_id, movie_id),
        )
        .await
        .unwrap(),
        db::PlaybackStartResult::Started
    );

    let offline_root = paths.path.join("offline");
    fs::rename(&media_root, &offline_root).unwrap();
    let missing_root_scan = run_scan(&state, library_id).await;
    assert_eq!(missing_root_scan.status, "completed_with_errors");
    assert!(missing_root_scan.errors > 0);
    assert_catalog_item_exists(&pool, movie_id).await;

    fs::create_dir(&media_root).unwrap();
    let replacement_root_scan = run_scan(&state, library_id).await;
    assert_eq!(replacement_root_scan.status, "completed_with_errors");
    assert!(replacement_root_scan.errors > 0);
    assert_catalog_item_exists(&pool, movie_id).await;

    let (old_device, old_inode) = db::library_root_identity(&pool, library_id, &media_root)
        .await
        .unwrap()
        .expect("the successful initial scan records the root identity");
    let old_device = old_device.to_string();
    let old_inode = old_inode.to_string();
    let rebound_file = media_root.join("rebound.mkv");
    fs::write(&rebound_file, b"authorized root recovery fixture").unwrap();
    // Restore the original path under the newly rebound root too. The earlier
    // successful scan intentionally reconciled the deleted catalog row, so
    // the later HTTP mutation must use an item that is present and visible.
    fs::write(&media_file, b"restored after authorized root rebind").unwrap();
    let admin_record = db::get_user(&pool, user_id).await.unwrap().unwrap();
    let admin_token = auth::issue_token(&state, &admin_record, "test", "admin", "admin-device")
        .await
        .unwrap()
        .token;
    let viewer_record = db::get_user(&pool, viewer_id).await.unwrap().unwrap();
    let viewer_token = auth::issue_token(&state, &viewer_record, "test", "viewer", "viewer-device")
        .await
        .unwrap()
        .token;
    let app = api::router(state.clone());

    // The official web client uses these legacy spellings during sign-in.
    // They must not fall through to the authenticated user-update route.
    for path in ["/Users/AuthenticateByName", "/Users/authenticatebyname"] {
        let response = app
            .clone()
            .oneshot(
                Request::builder()
                    .method("POST")
                    .uri(path)
                    .header("content-type", "application/json")
                    .body(Body::from("{"))
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::BAD_REQUEST, "{path}");
    }
    for path in ["/Users/Public", "/users/public"] {
        let response = app
            .clone()
            .oneshot(Request::builder().uri(path).body(Body::empty()).unwrap())
            .await
            .unwrap();
        assert_eq!(response.status(), StatusCode::OK, "{path}");
        let body = to_bytes(response.into_body(), 4096).await.unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&body).unwrap(),
            serde_json::json!([]),
            "anonymous login screens must not expose the database's accounts"
        );
    }
    let identities_path = "/Puffinbox/Libraries/RootIdentities";
    let unauthenticated = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(identities_path)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);
    let forbidden = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(identities_path)
                .header("x-emby-token", viewer_token.as_str())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(forbidden.status(), StatusCode::FORBIDDEN);
    let listed = app
        .clone()
        .oneshot(
            Request::builder()
                .uri(identities_path)
                .header("x-emby-token", admin_token.as_str())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(listed.status(), StatusCode::OK);

    let rebind_path = "/Puffinbox/Libraries/Roots/Rebind";
    let rebind_body = serde_json::json!({
        "LibraryId": library_id,
        "RootPath": media_root,
        "ExpectedDeviceId": old_device,
        "ExpectedInode": old_inode,
    })
    .to_string();
    let unauthorized_rebind = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(rebind_path)
                .header("content-type", "application/json")
                .body(Body::from(rebind_body.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthorized_rebind.status(), StatusCode::UNAUTHORIZED);
    let forbidden_rebind = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(rebind_path)
                .header("content-type", "application/json")
                .header("x-emby-token", viewer_token.as_str())
                .body(Body::from(rebind_body.clone()))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(forbidden_rebind.status(), StatusCode::FORBIDDEN);
    let rebound = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(rebind_path)
                .header("content-type", "application/json")
                .header("x-emby-token", admin_token.as_str())
                .body(Body::from(rebind_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(rebound.status(), StatusCode::ACCEPTED);
    let rebound_status = wait_for_scan(&state, library_id).await;
    assert_eq!(rebound_status.status, "completed");
    assert_eq!(rebound_status.errors, 0);
    let rebound_item_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM items WHERE library_id=$1 AND path=$2 AND item_type='Movie')",
    )
    .bind(library_id)
    .bind(rebound_file.to_str().unwrap())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        rebound_item_exists,
        "authorized root rebind starts a successful rescan"
    );
    let stale_rebind_body = serde_json::json!({
        "LibraryId": library_id,
        "RootPath": media_root,
        "ExpectedDeviceId": old_device,
        "ExpectedInode": old_inode,
    })
    .to_string();
    let stale_rebind = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(rebind_path)
                .header("content-type", "application/json")
                .header("x-emby-token", admin_token.as_str())
                .body(Body::from(stale_rebind_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stale_rebind.status(), StatusCode::CONFLICT);
    let persisted_root_identity = db::library_root_identity(&pool, library_id, &media_root)
        .await
        .unwrap()
        .unwrap();
    let opened_root_identity = library::inspect_library_root_identity(media_root.clone())
        .await
        .unwrap();
    assert_eq!(
        persisted_root_identity,
        (opened_root_identity.0, opened_root_identity.1)
    );
    let current_device = persisted_root_identity.0.to_string();
    let current_inode = persisted_root_identity.1.to_string();
    sqlx::query("UPDATE libraries SET enabled=FALSE WHERE id=$1")
        .bind(library_id)
        .execute(&pool)
        .await
        .unwrap();
    let disabled_rebind_body = serde_json::json!({
        "LibraryId": library_id,
        "RootPath": media_root,
        "ExpectedDeviceId": current_device,
        "ExpectedInode": current_inode,
    })
    .to_string();
    let disabled_rebind = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(rebind_path)
                .header("content-type", "application/json")
                .header("x-emby-token", admin_token.as_str())
                .body(Body::from(disabled_rebind_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(disabled_rebind.status(), StatusCode::ACCEPTED);
    let disabled_json: serde_json::Value = serde_json::from_slice(
        &to_bytes(disabled_rebind.into_body(), 64 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(disabled_json["ScanStatus"], "deferred-disabled");

    sqlx::query("UPDATE libraries SET enabled=TRUE WHERE id=$1")
        .bind(library_id)
        .execute(&pool)
        .await
        .unwrap();
    let occupied_scan_slot = state.scan_slots.clone().acquire_owned().await.unwrap();
    let capacity_rebind_body = serde_json::json!({
        "LibraryId": library_id,
        "RootPath": media_root,
        "ExpectedDeviceId": current_device,
        "ExpectedInode": current_inode,
    })
    .to_string();
    let capacity_rebind = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(rebind_path)
                .header("content-type", "application/json")
                .header("x-emby-token", admin_token.as_str())
                .body(Body::from(capacity_rebind_body))
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(capacity_rebind.status(), StatusCode::ACCEPTED);
    let capacity_json: serde_json::Value = serde_json::from_slice(
        &to_bytes(capacity_rebind.into_body(), 64 * 1024)
            .await
            .unwrap(),
    )
    .unwrap();
    assert_eq!(capacity_json["RootStatus"], "already-current");
    assert_eq!(capacity_json["ScanStatus"], "capacity-reached");
    drop(occupied_scan_slot);

    let old_scan_files = (0..260)
        .map(|index| raced_root.join(format!("old-{index:04}.mkv")))
        .collect::<Vec<_>>();
    for path in &old_scan_files {
        fs::write(path, b"previously indexed media").unwrap();
    }
    let baseline_scan = run_scan(&state, raced_library_id).await;
    assert_eq!(baseline_scan.status, "completed");
    let (raced_device, raced_inode) =
        db::library_root_identity(&pool, raced_library_id, &raced_root)
            .await
            .unwrap()
            .unwrap();
    let opened_raced_identity = library::inspect_library_root_identity(raced_root.clone())
        .await
        .unwrap();
    let opened_raced_device = opened_raced_identity.0.to_string();
    let opened_raced_inode = opened_raced_identity.1.to_string();
    let rebind_probe_scan_id = Uuid::new_v4();
    assert_eq!(
        db::claim_library_scan(&pool, old_run, raced_library_id, rebind_probe_scan_id)
            .await
            .unwrap(),
        db::LibraryScanClaim::Claimed
    );
    assert_eq!(
        db::rebind_library_root(
            &pool,
            old_run,
            &db::RootRebindRequest {
                library_id: raced_library_id,
                root_path: raced_root.clone(),
                expected_device_id: raced_device.to_string(),
                expected_inode: raced_inode.to_string(),
                current_device_id: opened_raced_identity.0.to_string(),
                current_inode: opened_raced_identity.1.to_string(),
            },
        )
        .await
        .unwrap(),
        db::RootRebindResult::ScanRunning,
        "root identity cannot change while the persisted scan status is running"
    );
    sqlx::query("UPDATE library_scan_state SET status='interrupted',finished_at=NOW() WHERE library_id=$1 AND scan_id=$2")
        .bind(raced_library_id)
        .bind(rebind_probe_scan_id)
        .execute(&pool)
        .await
        .unwrap();
    for path in &old_scan_files {
        fs::remove_file(path).unwrap();
    }
    for index in 0..600 {
        fs::write(
            raced_root.join(format!("raced-{index:04}.mkv")),
            b"current scan media",
        )
        .unwrap();
    }
    let scan_gate_key = i64::from_le_bytes(
        Uuid::new_v4().as_bytes()[..8]
            .try_into()
            .expect("a UUID has at least eight bytes"),
    );
    let mut scan_gate = pool.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(scan_gate_key)
        .execute(&mut *scan_gate)
        .await
        .unwrap();
    sqlx::raw_sql(&format!(
        "CREATE FUNCTION hold_raced_item_write() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN IF NEW.name LIKE 'raced-%' THEN PERFORM pg_advisory_xact_lock({scan_gate_key}); END IF; RETURN NEW; END; $$; CREATE TRIGGER hold_raced_items BEFORE INSERT OR UPDATE ON items FOR EACH ROW EXECUTE FUNCTION hold_raced_item_write()"
    ))
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        library::spawn_scan(state.clone(), raced_library_id)
            .await
            .unwrap(),
        library::ScanStart::Started
    );
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            let scan_is_waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND pid<>pg_backend_pid() AND query LIKE 'INSERT INTO items%' AND wait_event_type='Lock' AND wait_event='advisory')",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            if scan_is_waiting {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the scan transaction should wait at the advisory barrier");

    let mutation_gate_key = i64::from_le_bytes(
        Uuid::new_v4().as_bytes()[..8]
            .try_into()
            .expect("a UUID has at least eight bytes"),
    );
    let mut mutation_gate = admin_pool.acquire().await.unwrap();
    sqlx::query("SELECT pg_advisory_lock($1)")
        .bind(mutation_gate_key)
        .execute(&mut *mutation_gate)
        .await
        .unwrap();
    sqlx::raw_sql(&format!(
        "CREATE FUNCTION delay_user_data_write() RETURNS TRIGGER LANGUAGE plpgsql AS $$ BEGIN PERFORM pg_advisory_xact_lock({mutation_gate_key}); RETURN NEW; END; $$; CREATE TRIGGER delay_user_data_before_write BEFORE INSERT OR UPDATE ON user_item_data FOR EACH ROW EXECUTE FUNCTION delay_user_data_write()"
    ))
    .execute(&pool)
    .await
    .unwrap();
    let favorite_request = Request::builder()
        .method("POST")
        .uri(format!("/UserFavoriteItems/{movie_id}"))
        .header("x-emby-token", admin_token.as_str())
        .body(Body::empty())
        .unwrap();
    let current_movie = db::get_item(&pool, movie_id)
        .await
        .unwrap()
        .expect("the restored scan should preserve the deterministic item id");
    assert!(
        db::item_visible_to_user(&pool, &admin_record, &current_movie)
            .await
            .unwrap(),
        "the administrator should be able to favorite the restored movie"
    );
    let restored_item_visible: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM items i JOIN libraries l ON l.id=i.library_id WHERE i.id=$1 AND l.enabled=TRUE)",
    )
    .bind(movie_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        restored_item_visible,
        "the target row must be in an enabled library"
    );
    let favorite_request_app = app.clone();
    let mut in_flight_favorite = tokio::spawn(async move {
        favorite_request_app
            .oneshot(favorite_request)
            .await
            .unwrap()
    });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let trigger_is_running: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND pid<>pg_backend_pid() AND query LIKE 'INSERT INTO user_item_data%' AND wait_event_type='Lock' AND wait_event='advisory')",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            if trigger_is_running {
                break;
            }
            if in_flight_favorite.is_finished() {
                let response = (&mut in_flight_favorite)
                    .await
                    .expect("the early HTTP response task should not panic");
                let status = response.status();
                let body = to_bytes(response.into_body(), 64 * 1024).await.unwrap();
                panic!(
                    "the HTTP favorite mutation returned {status} before reaching the trigger gate: {}",
                    String::from_utf8_lossy(&body)
                );
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the ordinary HTTP mutation reached its delayed database write");

    let new_run = Uuid::new_v4();
    let marker_pool = pool.clone();
    let marker_update =
        tokio::spawn(async move { db::set_active_run_marker(&marker_pool, new_run).await });
    tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let takeover_is_waiting: bool = sqlx::query_scalar(
                "SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND pid<>pg_backend_pid() AND query LIKE 'INSERT INTO instance_meta%' AND wait_event_type='Lock')",
            )
            .fetch_one(&pool)
            .await
            .unwrap();
            if takeover_is_waiting {
                break;
            }
            assert!(
                !marker_update.is_finished(),
                "the run marker update should wait behind both held transactions"
            );
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await
    .expect("the new run reached the database fence and is blocked");
    assert!(
        !marker_update.is_finished(),
        "run takeover must wait for the overlapping HTTP mutation and scanner transactions"
    );
    let released: bool = sqlx::query_scalar("SELECT pg_advisory_unlock($1)")
        .bind(mutation_gate_key)
        .fetch_one(&mut *mutation_gate)
        .await
        .unwrap();
    assert!(released);
    drop(mutation_gate);
    let favorite_response = tokio::time::timeout(Duration::from_secs(5), &mut in_flight_favorite)
        .await
        .expect("the favorite transaction should finish after its barrier opens")
        .unwrap();
    assert_eq!(favorite_response.status(), StatusCode::OK);
    assert!(
        !marker_update.is_finished(),
        "the run takeover remains blocked by the still-held scan transaction"
    );
    let scan_gate_released: bool = sqlx::query_scalar("SELECT pg_advisory_unlock($1)")
        .bind(scan_gate_key)
        .fetch_one(&mut *scan_gate)
        .await
        .unwrap();
    assert!(scan_gate_released);
    drop(scan_gate);
    assert!(marker_update.await.unwrap().unwrap());
    let stale_http_write = app
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!("/UserFavoriteItems/{movie_id}"))
                .header("x-emby-token", admin_token.as_str())
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(stale_http_write.status(), StatusCode::SERVICE_UNAVAILABLE);
    assert!(
        db::set_item_favorite(&pool, old_run, user_id, movie_id, false)
            .await
            .is_err(),
        "a stale request that passed HTTP authentication earlier cannot mutate afterward"
    );
    let old_run_favorite: bool = sqlx::query_scalar(
        "SELECT is_favorite FROM user_item_data WHERE user_id=$1 AND item_id=$2",
    )
    .bind(user_id)
    .bind(movie_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(old_run_favorite);
    assert_eq!(
        db::claim_library_scan(&pool, old_run, fence_library_id, Uuid::new_v4())
            .await
            .unwrap(),
        db::LibraryScanClaim::StaleRun,
        "the takeover marker fences old scan claims before migration recovery"
    );
    assert_eq!(
        db::start_playback_session(
            &pool,
            playback_request(Uuid::new_v4(), old_run, user_id, movie_id),
        )
        .await
        .unwrap(),
        db::PlaybackStartResult::StaleRun,
        "the takeover marker fences old playback writes before migration recovery"
    );
    let mut stale_virtual_tx = pool.begin().await.unwrap();
    assert!(
        db::upsert_virtual_item(
            &mut stale_virtual_tx,
            old_run,
            db::VirtualItemInput {
                library_id,
                item_id: Uuid::new_v4(),
                name: "Stale channel",
                item_type: "LiveTvChannel",
                opaque_path: "puffinbox://livetv/stale-run/channel",
                overview: None,
                metadata_json: &serde_json::json!({}),
            },
        )
        .await
        .is_err()
    );
    stale_virtual_tx.rollback().await.unwrap();
    let rows_at_pre_migration_fence =
        movie_count_with_prefix(&pool, raced_library_id, "raced-").await;
    tokio::time::timeout(Duration::from_secs(15), async {
        while state.scan_slots.available_permits() != 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("pre-migration fencing should stop the stale scanner");
    assert_eq!(
        movie_count_with_prefix(&pool, raced_library_id, "raced-").await,
        rows_at_pre_migration_fence,
        "the old scanner cannot write after the takeover marker is committed"
    );
    let (ended_sessions, interrupted_scans) = db::activate_run(&pool, new_run).await.unwrap();
    assert_eq!(ended_sessions, 1);
    assert_eq!(interrupted_scans, 2, "both in-flight scans are interrupted");
    let interrupted_states = library::scan_status(&pool).await.unwrap();
    for interrupted_library_id in [fence_library_id, raced_library_id] {
        let interrupted_state = interrupted_states
            .iter()
            .find(|status| status.library_id == interrupted_library_id)
            .unwrap();
        assert_eq!(interrupted_state.status, "interrupted");
    }
    let rows_at_activation = movie_count_with_prefix(&pool, raced_library_id, "raced-").await;
    tokio::time::timeout(Duration::from_secs(15), async {
        while state.scan_slots.available_permits() != 1 {
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("the stale scan worker should stop after its generation check");
    let rows_after_stale_worker = movie_count_with_prefix(&pool, raced_library_id, "raced-").await;
    assert_eq!(rows_after_stale_worker, rows_at_activation);
    let previous_item_survives: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM items WHERE library_id=$1 AND path=$2)")
            .bind(raced_library_id)
            .bind(old_scan_files[0].to_str().unwrap())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(
        previous_item_survives,
        "an interrupted generation cannot reconcile away previously indexed rows"
    );
    assert_eq!(
        db::claim_library_scan(&pool, old_run, fence_library_id, Uuid::new_v4())
            .await
            .unwrap(),
        db::LibraryScanClaim::StaleRun
    );
    let old_scan_state = sqlx::query_as::<_, (Uuid, String)>(
        "SELECT scan_id,status FROM library_scan_state WHERE library_id=$1",
    )
    .bind(fence_library_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(old_scan_state, (old_scan_id, "interrupted".to_owned()));
    assert_eq!(
        db::rebind_library_root(
            &pool,
            old_run,
            &db::RootRebindRequest {
                library_id: raced_library_id,
                root_path: raced_root.clone(),
                expected_device_id: raced_device.to_string(),
                expected_inode: raced_inode.to_string(),
                current_device_id: opened_raced_device.clone(),
                current_inode: opened_raced_inode.clone(),
            },
        )
        .await
        .unwrap(),
        db::RootRebindResult::StaleRun,
        "a server process from an old run cannot rebind roots"
    );

    let stale_session_id = Uuid::new_v4();
    assert_eq!(
        db::start_playback_session(
            &pool,
            playback_request(stale_session_id, old_run, user_id, movie_id),
        )
        .await
        .unwrap(),
        db::PlaybackStartResult::StaleRun
    );
    let old_selector = playback_selector(old_run, user_id, old_session_id, movie_id);
    assert!(
        db::update_playback_session(&pool, old_selector.clone(), Some(900), None)
            .await
            .unwrap()
            .is_none()
    );
    assert!(
        db::finish_playback_session(&pool, old_selector, Some(900), None, true)
            .await
            .unwrap()
            .is_none()
    );

    let new_session_id = Uuid::new_v4();
    assert_eq!(
        db::start_playback_session(
            &pool,
            playback_request(new_session_id, new_run, user_id, movie_id),
        )
        .await
        .unwrap(),
        db::PlaybackStartResult::Started
    );
    assert_eq!(
        db::start_playback_session(
            &pool,
            playback_request(Uuid::new_v4(), old_run, user_id, movie_id),
        )
        .await
        .unwrap(),
        db::PlaybackStartResult::StaleRun,
        "a stale process cannot end the new run's active playback session"
    );
    let active =
        db::active_playback_session(&pool, new_run, user_id, "test-device", None, Some(movie_id))
            .await
            .unwrap()
            .unwrap();
    assert_eq!(active.id, new_session_id);

    for _ in 0..32 {
        let session_id = Uuid::new_v4();
        assert_eq!(
            db::start_playback_session(
                &pool,
                playback_request(session_id, new_run, user_id, movie_id),
            )
            .await
            .unwrap(),
            db::PlaybackStartResult::Started
        );
        let progress_pool = pool.clone();
        let progress_selector = playback_selector(new_run, user_id, session_id, movie_id);
        let progress = tokio::spawn(async move {
            db::update_playback_session(&progress_pool, progress_selector, Some(100), None).await
        });
        let stop_pool = pool.clone();
        let stop_selector = playback_selector(new_run, user_id, session_id, movie_id);
        let stop = tokio::spawn(async move {
            db::finish_playback_session(&stop_pool, stop_selector, Some(200), Some(false), true)
                .await
        });
        let (progress_result, stop_result) = tokio::join!(progress, stop);
        let _ = progress_result.unwrap().unwrap();
        assert!(stop_result.unwrap().unwrap().is_some());
        let final_session = sqlx::query_as::<_, (i64, bool)>(
            "SELECT position_ticks,ended_at IS NOT NULL FROM playback_sessions WHERE id=$1",
        )
        .bind(session_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        assert_eq!(final_session, (200, true));
        let mut user_data = db::item_user_data(&pool, user_id, &[movie_id])
            .await
            .unwrap();
        let user_data = user_data.remove(&movie_id).unwrap();
        assert_eq!(user_data.playback_position_ticks, 200);
        assert!(!user_data.played);
    }

    drop(state);
    pool.close().await;
    competing_lock_pool.close().await;
    instance_lock_pool.close().await;
    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .execute(&admin_pool)
        .await
        .unwrap();
    sqlx::query(&format!("DROP DATABASE \"{lock_database}\""))
        .execute(&admin_pool)
        .await
        .unwrap();
    admin_pool.close().await;
}

async fn run_scan(state: &AppState, library_id: Uuid) -> library::ScanStatus {
    assert_eq!(
        library::spawn_scan(state.clone(), library_id)
            .await
            .unwrap(),
        library::ScanStart::Started
    );
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(status) = library::scan_status(&state.db)
                .await
                .unwrap()
                .into_iter()
                .find(|status| status.library_id == library_id)
                && status.status != "running"
                && status.finished_at.is_some()
            {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("library scan should finish within 15 seconds")
}

async fn wait_for_scan(state: &AppState, library_id: Uuid) -> library::ScanStatus {
    tokio::time::timeout(Duration::from_secs(15), async {
        loop {
            if let Some(status) = library::scan_status(&state.db)
                .await
                .unwrap()
                .into_iter()
                .find(|status| status.library_id == library_id)
                && status.status != "running"
                && status.finished_at.is_some()
            {
                return status;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
    })
    .await
    .expect("root recovery scan should finish within 15 seconds")
}

async fn assert_catalog_item_exists(pool: &PgPool, item_id: Uuid) {
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM items WHERE id=$1)")
        .bind(item_id)
        .fetch_one(pool)
        .await
        .unwrap();
    assert!(
        exists,
        "failed or unsafe scans preserve prior catalogue rows"
    );
}

async fn movie_count_with_prefix(pool: &PgPool, library_id: Uuid, prefix: &str) -> i64 {
    sqlx::query_scalar(
        "SELECT COUNT(*)::BIGINT FROM items WHERE library_id=$1 AND item_type='Movie' AND name LIKE ($2 || '%')",
    )
    .bind(library_id)
    .bind(prefix)
    .fetch_one(pool)
    .await
    .unwrap()
}

fn playback_request(
    id: Uuid,
    run_id: Uuid,
    user_id: Uuid,
    item_id: Uuid,
) -> db::PlaybackStartRequest {
    db::PlaybackStartRequest {
        id,
        run_id,
        user_id,
        item_id,
        device_id: "test-device".to_owned(),
        device_name: "Test Device".to_owned(),
        client: "Guard Test".to_owned(),
        play_method: Some("DirectPlay".to_owned()),
        position_ticks: Some(0),
    }
}

fn playback_selector(
    run_id: Uuid,
    user_id: Uuid,
    id: Uuid,
    item_id: Uuid,
) -> db::PlaybackSessionSelector {
    db::PlaybackSessionSelector {
        run_id,
        user_id,
        device_id: "test-device".to_owned(),
        id: Some(id),
        item_id: Some(item_id),
    }
}

fn test_config(database_url: String) -> Config {
    Config {
        bind: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        public_base_url: None,
        database_url,
        server_name: "PostgreSQL guard test".to_owned(),
        web_root: env::temp_dir(),
        data_dir: env::temp_dir(),
        ffmpeg_path: None,
        max_scan_workers: 1,
        max_page_size: 100,
        access_token_lifetime_hours: 24,
        cookie_secure: false,
        cors_origins: Vec::new(),
        trusted_proxies: Vec::new(),
        local_networks: vec!["127.0.0.0/8".parse::<IpNet>().unwrap()],
        setup_token: None,
        bootstrap_admin_username: None,
        bootstrap_admin_password: None,
    }
}

struct TestPaths {
    path: PathBuf,
}

impl TestPaths {
    fn new() -> Self {
        let path = env::temp_dir().join(format!("puffinbox-server-guard-{}", Uuid::new_v4()));
        fs::create_dir_all(&path).unwrap();
        Self { path }
    }
}

impl Drop for TestPaths {
    fn drop(&mut self) {
        let _ = fs::remove_dir_all(&self.path);
    }
}
