use std::{
    env, fs,
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};

use axum::{body::Body, http::Request};
use chrono::{Duration as ChronoDuration, Utc};
use ipnet::IpNet;
use puffinbox::{AppState, Config, api, auth, db, library, media_features};
use serde_json::{Value, json};
use sha2::Digest;
use sqlx::{Row, postgres::PgPoolOptions, types::Json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
    time::{sleep, timeout},
};
use tower::ServiceExt;
use uuid::Uuid;

mod common;

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via PUFFINBOX_TEST_DATABASE_URL"]
async fn scheduled_iptv_capture_publishes_and_recovers_atomically() {
    let database_url = env::var("PUFFINBOX_TEST_DATABASE_URL")
        .expect("set PUFFINBOX_TEST_DATABASE_URL to a disposable PostgreSQL database");
    let admin_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url)
        .await
        .unwrap();
    let schema = format!("puffinbox_livetv_dvr_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
        .execute(&admin_pool)
        .await
        .unwrap();
    let connection_schema = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(12)
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
    let root = env::temp_dir().join(format!("puffinbox-livetv-dvr-{}", Uuid::new_v4()));
    fs::create_dir_all(&root).unwrap();
    let root = fs::canonicalize(root).unwrap();
    let data_dir = root.join("data");
    fs::create_dir_all(&data_dir).unwrap();
    let library_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO libraries(id,name,collection_type,locations) VALUES($1,'IPTV DVR fixture','mixed',$2)",
    )
    .bind(library_id)
    .bind(Json(vec![root.to_string_lossy().into_owned()]))
    .execute(&pool)
    .await
    .unwrap();
    let owner_id = insert_user(&pool, "recorder-owner", true, None, &[]).await;
    let access_owner_id = insert_user(&pool, "revocable-recording-owner", false, None, &[]).await;
    sqlx::query(
        "UPDATE users SET enable_live_tv_access=TRUE,enable_live_tv_management=TRUE WHERE id=$1",
    )
    .bind(access_owner_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO user_library_access(user_id,library_id) VALUES($1,$2)")
        .bind(access_owner_id)
        .bind(library_id)
        .execute(&pool)
        .await
        .unwrap();
    let restrictive_id = insert_user(&pool, "recording-child", false, Some(40), &[]).await;
    let unrated_block_id = insert_user(
        &pool,
        "recording-unrated-child",
        false,
        None,
        &["LiveTvProgram"],
    )
    .await;
    for user_id in [restrictive_id, unrated_block_id] {
        sqlx::query("UPDATE users SET enable_live_tv_access=TRUE WHERE id=$1")
            .bind(user_id)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO user_library_access(user_id,library_id) VALUES($1,$2)")
            .bind(user_id)
            .bind(library_id)
            .execute(&pool)
            .await
            .unwrap();
    }

    let (stream_url, fixture_task) = serve_chunked_transport_stream().await;
    let stream = url::Url::parse(&stream_url).unwrap();
    let origin = stream.origin().ascii_serialization();
    let source_id = Uuid::new_v4();
    let channel_id = stable_channel_item_id(source_id, "fixture-channel");
    let origin_pins = json!([{
        "origin": origin,
        "addresses": [Ipv4Addr::LOCALHOST.to_string()]
    }]);
    sqlx::query(
        "INSERT INTO live_tv_sources(id,library_id,name,playlist_url,origin_pins,enabled,refresh_status,last_refreshed_at) \
         VALUES($1,$2,'Pinned DVR fixture',$3,$4,TRUE,'ready',NOW())",
    )
    .bind(source_id)
    .bind(library_id)
    .bind(format!("{}/channels.m3u", stream.origin().ascii_serialization()))
    .bind(Json(origin_pins))
    .execute(&pool)
    .await
    .unwrap();
    let channel_path = format!("puffinbox://livetv/{source_id}/fixture-channel");
    let channel_metadata = json!({ "SourceId": source_id, "ChannelId": "fixture-channel" });
    let mut tx = pool.begin().await.unwrap();
    db::upsert_virtual_item(
        &mut tx,
        run_id,
        db::VirtualItemInput {
            library_id,
            item_id: channel_id,
            name: "Fixture Channel",
            item_type: "LiveTvChannel",
            opaque_path: &channel_path,
            overview: None,
            metadata_json: &channel_metadata,
        },
    )
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO live_tv_channels(item_id,library_id,source_id,source_channel_id,name,stream_url) \
         VALUES($1,$2,$3,'fixture-channel','Fixture Channel',$4)",
    )
    .bind(channel_id)
    .bind(library_id)
    .bind(source_id)
    .bind(&stream_url)
    .execute(&mut *tx)
    .await
    .unwrap();
    tx.commit().await.unwrap();

    let config = Config {
        bind: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        public_base_url: None,
        database_url: database_url.clone(),
        server_name: "Live TV DVR test".to_owned(),
        web_root: PathBuf::from("web"),
        data_dir,
        ffmpeg_path: None,
        max_scan_workers: 2,
        max_page_size: 100,
        access_token_lifetime_hours: 24,
        cookie_secure: false,
        cors_origins: Vec::new(),
        trusted_proxies: Vec::new(),
        local_networks: vec!["127.0.0.0/8".parse::<IpNet>().unwrap()],
        dlna: Default::default(),
        setup_token: None,
        bootstrap_admin_username: None,
        bootstrap_admin_password: None,
    };
    let state = AppState::new_for_run(pool.clone(), Arc::new(config), Uuid::new_v4(), run_id, None);
    let owner = db::get_user(&pool, owner_id).await.unwrap().unwrap();
    let owner_token =
        auth::issue_token(&state, &owner, "dvr-test", "test-client", "recorder-owner")
            .await
            .unwrap()
            .token;
    let router = api::router(state.clone());

    // A scan that sees a final-looking file while its recording row is still
    // active must not add an unrated catalog row for it.
    let pending_id = Uuid::new_v4();
    let pending_path = root.join(recording_name(pending_id));
    fs::write(&pending_path, valid_ts_bytes(2)).unwrap();
    insert_active_recording(&pool, owner_id, channel_id, library_id, pending_id).await;
    sqlx::query(
        "UPDATE live_tv_recordings SET status='publishing',byte_count=376,sha256=$2 WHERE timer_id=$1",
    )
    .bind(pending_id)
    .bind(sha256_hex(&valid_ts_bytes(2)))
    .execute(&pool)
    .await
    .unwrap();
    assert_eq!(
        library::spawn_scan(state.clone(), library_id)
            .await
            .unwrap(),
        library::ScanStart::Started
    );
    wait_for_scan(&pool, library_id, &root).await;
    let pending_item_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM items WHERE library_id=$1 AND path=$2")
            .bind(library_id)
            .bind(pending_path.to_string_lossy().as_ref())
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        pending_item_count, 0,
        "active recording files are scan-hidden"
    );
    sqlx::query("DELETE FROM live_tv_recordings WHERE timer_id=$1")
        .bind(pending_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM live_tv_timers WHERE id=$1")
        .bind(pending_id)
        .execute(&pool)
        .await
        .unwrap();
    fs::remove_file(pending_path).unwrap();

    let now = Utc::now();
    let timer_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO live_tv_timers(id,owner_user_id,channel_item_id,start_at,end_at,output_library_id, \
         rating_system,content_rating,policy_rating_scale,policy_rating_value,display_name) \
         VALUES($1,$2,$3,$4,$5,$6,'VCHIP','TV-PG','US-PARENTAL-v1',50,'Manual DVR title')",
    )
    .bind(timer_id)
    .bind(owner_id)
    .bind(channel_id)
    .bind(now - ChronoDuration::seconds(5))
    .bind(now + ChronoDuration::seconds(3))
    .bind(library_id)
    .execute(&pool)
    .await
    .unwrap();
    let expired_id = Uuid::new_v4();
    insert_timer(
        &pool,
        owner_id,
        channel_id,
        library_id,
        expired_id,
        now - ChronoDuration::minutes(10),
        now - ChronoDuration::minutes(1),
    )
    .await;
    let missed_id = Uuid::new_v4();
    insert_timer(
        &pool,
        owner_id,
        channel_id,
        library_id,
        missed_id,
        now - ChronoDuration::minutes(3),
        now + ChronoDuration::minutes(3),
    )
    .await;

    media_features::start_livetv_recorder(state.clone()).await;
    wait_for_recording(&pool, timer_id).await;
    let recording = sqlx::query(
        "SELECT r.status,r.item_id,r.byte_count,r.sha256,r.relative_path,i.path,i.name,i.rating,i.runtime_ticks,i.metadata_json \
         FROM live_tv_recordings r JOIN items i ON i.id=r.item_id WHERE r.timer_id=$1",
    )
    .bind(timer_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        recording.try_get::<String, _>("status").unwrap(),
        "completed"
    );
    let byte_count: i64 = recording.try_get("byte_count").unwrap();
    assert!(byte_count >= 188 && byte_count % 188 == 0);
    let final_path: String = recording.try_get("path").unwrap();
    assert_eq!(
        recording.try_get::<String, _>("relative_path").unwrap(),
        PathBuf::from(&final_path)
            .file_name()
            .unwrap()
            .to_string_lossy()
    );
    assert!(PathBuf::from(&final_path).is_file());
    assert_eq!(
        recording.try_get::<String, _>("name").unwrap(),
        "Manual DVR title"
    );
    assert_eq!(
        recording.try_get::<Option<i16>, _>("rating").unwrap(),
        Some(50)
    );
    assert!(
        recording
            .try_get::<Option<i64>, _>("runtime_ticks")
            .unwrap()
            .unwrap_or_default()
            > 0
    );
    assert_eq!(
        recording.try_get::<Value, _>("metadata_json").unwrap()["LiveTvRecording"],
        true
    );
    let item_id: Uuid = recording.try_get("item_id").unwrap();
    let visible_item = db::get_item(&pool, item_id).await.unwrap().unwrap();
    let too_restrictive = db::get_user(&pool, restrictive_id).await.unwrap().unwrap();
    let block_unrated = db::get_user(&pool, unrated_block_id)
        .await
        .unwrap()
        .unwrap();
    assert!(
        !db::item_visible_to_user(&pool, &too_restrictive, &visible_item)
            .await
            .unwrap()
    );
    assert!(
        db::item_visible_to_user(&pool, &block_unrated, &visible_item)
            .await
            .unwrap()
    );

    let fake_unrated_id = Uuid::new_v4();
    let fake_path = root.join(format!("recording-{fake_unrated_id}.ts"));
    let fake_path_text = fake_path.to_string_lossy().into_owned();
    sqlx::query(
        "INSERT INTO items(id,library_id,name,sort_name,item_type,path,path_hash,container,metadata_json) \
         VALUES($1,$2,'Unrated recording','unrated recording','Movie',$3,$4,'ts',$5)",
    )
    .bind(fake_unrated_id)
    .bind(library_id)
    .bind(&fake_path_text)
    .bind(db::path_hash(&fake_path_text))
    .bind(Json(json!({ "LiveTvRecording": true })))
    .execute(&pool)
    .await
    .unwrap();
    let fake_item = db::get_item(&pool, fake_unrated_id).await.unwrap().unwrap();
    assert!(
        !db::item_visible_to_user(&pool, &block_unrated, &fake_item)
            .await
            .unwrap()
    );

    // The same parental result must hold through the generic media endpoint.
    let child = db::get_user(&pool, restrictive_id).await.unwrap().unwrap();
    let child_token = auth::issue_token(&state, &child, "dvr-test", "test-client", "restricted")
        .await
        .unwrap()
        .token;
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("GET")
                .uri(format!("/Items/{item_id}/PlaybackInfo"))
                .header("X-Emby-Token", &child_token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);

    for missed in [expired_id, missed_id] {
        let status: String = sqlx::query_scalar("SELECT status FROM live_tv_timers WHERE id=$1")
            .bind(missed)
            .fetch_one(&pool)
            .await
            .unwrap();
        assert_eq!(
            status, "failed",
            "expired and overly-late windows fail closed"
        );
        let rows: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM live_tv_recordings WHERE timer_id=$1")
                .bind(missed)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(rows, 0, "missed windows do not create empty recordings");
    }

    // A non-admin owner can lose Live TV access after a timer was queued.
    // The capture may finish its bounded input window, but a fresh policy read
    // immediately before publication must prevent a catalog-visible file.
    let revoked_capture_id = Uuid::new_v4();
    let revoke_now = Utc::now();
    insert_timer(
        &pool,
        access_owner_id,
        channel_id,
        library_id,
        revoked_capture_id,
        revoke_now - ChronoDuration::seconds(1),
        revoke_now + ChronoDuration::seconds(2),
    )
    .await;
    wait_for_active_recording(&pool, revoked_capture_id).await;
    wait_for_partial_file(&root, revoked_capture_id).await;
    let root_path = root.to_string_lossy().into_owned();
    let root_identity = sqlx::query(
        "SELECT root_path_hash,device_id,inode FROM library_root_identities WHERE library_id=$1 AND root_path=$2",
    )
    .bind(library_id)
    .bind(&root_path)
    .fetch_one(&pool)
    .await
    .unwrap();
    let root_path_hash: String = root_identity.try_get("root_path_hash").unwrap();
    let root_device: String = root_identity.try_get("device_id").unwrap();
    let root_inode: String = root_identity.try_get("inode").unwrap();
    sqlx::query("DELETE FROM library_root_identities WHERE library_id=$1 AND root_path_hash=$2")
        .bind(library_id)
        .bind(&root_path_hash)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET enable_live_tv_access=FALSE WHERE id=$1")
        .bind(access_owner_id)
        .execute(&pool)
        .await
        .unwrap();
    sleep(Duration::from_secs(3)).await;
    let blocked_cleanup =
        sqlx::query("SELECT status,claimed_run_id FROM live_tv_recordings WHERE timer_id=$1")
            .bind(revoked_capture_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        blocked_cleanup.try_get::<String, _>("status").unwrap(),
        "recording",
        "missing root identity keeps terminal failure hidden"
    );
    assert_eq!(
        blocked_cleanup
            .try_get::<Option<Uuid>, _>("claimed_run_id")
            .unwrap(),
        Some(run_id)
    );
    assert!(root.join(partial_name(revoked_capture_id)).is_file());
    sqlx::query(
        "INSERT INTO library_root_identities(library_id,root_path_hash,root_path,device_id,inode) VALUES($1,$2,$3,$4,$5)",
    )
    .bind(library_id)
    .bind(root_path_hash)
    .bind(root_path)
    .bind(root_device)
    .bind(root_inode)
    .execute(&pool)
    .await
    .unwrap();
    wait_for_recording_status(&pool, revoked_capture_id, "failed").await;
    assert!(!root.join(recording_name(revoked_capture_id)).exists());
    assert!(
        !root.join(partial_name(revoked_capture_id)).exists(),
        "terminal failed status must not become visible before partial-file cleanup"
    );
    sqlx::query("UPDATE users SET enable_live_tv_access=TRUE WHERE id=$1")
        .bind(access_owner_id)
        .execute(&pool)
        .await
        .unwrap();

    // A partial-path unlink failure must keep the recording active and retry
    // finalization after the filesystem obstruction is removed.
    let file_retry_id = Uuid::new_v4();
    let file_retry_partial = root.join(partial_name(file_retry_id));
    let file_retry_final = root.join(recording_name(file_retry_id));
    fs::create_dir(&file_retry_partial).unwrap();
    fs::write(&file_retry_final, valid_ts_bytes(2)).unwrap();
    let file_retry_now = Utc::now();
    insert_timer(
        &pool,
        owner_id,
        channel_id,
        library_id,
        file_retry_id,
        file_retry_now - ChronoDuration::seconds(1),
        file_retry_now + ChronoDuration::seconds(30),
    )
    .await;
    wait_for_active_recording(&pool, file_retry_id).await;
    sleep(Duration::from_secs(1)).await;
    let file_retry_status: String =
        sqlx::query_scalar("SELECT status FROM live_tv_recordings WHERE timer_id=$1")
            .bind(file_retry_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(file_retry_status, "recording");
    assert!(file_retry_partial.is_dir());
    assert!(file_retry_final.is_file());
    fs::remove_dir(&file_retry_partial).unwrap();
    wait_for_recording_status(&pool, file_retry_id, "interrupted").await;
    assert!(!file_retry_partial.exists());
    assert!(!file_retry_final.exists());

    // A terminal-status SQL failure happens after cleanup. The row stays
    // active while the idempotent finalizer retries, then commits once the
    // temporary constraint is removed.
    let db_retry_id = Uuid::new_v4();
    let constraint_name = format!("livetv_retry_gate_{}", db_retry_id.simple());
    sqlx::query(&format!(
        "ALTER TABLE live_tv_recordings ADD CONSTRAINT {constraint_name} \
         CHECK (id <> '{db_retry_id}'::uuid OR status <> 'failed')"
    ))
    .execute(&pool)
    .await
    .unwrap();
    let db_retry_now = Utc::now();
    insert_timer(
        &pool,
        access_owner_id,
        channel_id,
        library_id,
        db_retry_id,
        db_retry_now - ChronoDuration::seconds(1),
        db_retry_now + ChronoDuration::seconds(6),
    )
    .await;
    wait_for_active_recording(&pool, db_retry_id).await;
    wait_for_partial_file(&root, db_retry_id).await;
    sqlx::query("UPDATE users SET enable_live_tv_access=FALSE WHERE id=$1")
        .bind(access_owner_id)
        .execute(&pool)
        .await
        .unwrap();
    wait_for_cleaned_active_recording(&pool, &root, db_retry_id).await;
    sqlx::query(&format!(
        "ALTER TABLE live_tv_recordings DROP CONSTRAINT {constraint_name}"
    ))
    .execute(&pool)
    .await
    .unwrap();
    wait_for_recording_status(&pool, db_retry_id, "failed").await;
    assert!(!root.join(partial_name(db_retry_id)).exists());
    assert!(!root.join(recording_name(db_retry_id)).exists());
    sqlx::query("UPDATE users SET enable_live_tv_access=TRUE WHERE id=$1")
        .bind(access_owner_id)
        .execute(&pool)
        .await
        .unwrap();

    // The output link can succeed while the terminal database transition
    // fails. The tracked finalizer must verify the already published file and
    // complete the same claim in this process after the constraint is lifted.
    let publish_retry_id = Uuid::new_v4();
    let publish_retry_constraint = format!("livetv_publish_retry_{}", publish_retry_id.simple());
    sqlx::query(&format!(
        "ALTER TABLE live_tv_recordings ADD CONSTRAINT {publish_retry_constraint} \
         CHECK (id <> '{publish_retry_id}'::uuid OR status <> 'completed')"
    ))
    .execute(&pool)
    .await
    .unwrap();
    let publish_retry_now = Utc::now();
    insert_timer(
        &pool,
        owner_id,
        channel_id,
        library_id,
        publish_retry_id,
        publish_retry_now - ChronoDuration::seconds(1),
        publish_retry_now + ChronoDuration::seconds(5),
    )
    .await;
    wait_for_recording_status(&pool, publish_retry_id, "publishing").await;
    let publish_retry_final = root.join(recording_name(publish_retry_id));
    let publish_retry_partial = root.join(partial_name(publish_retry_id));
    timeout(Duration::from_secs(5), async {
        while !publish_retry_final.is_file() {
            sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("publication links the verified final file before its database transition");
    sleep(Duration::from_millis(500)).await;
    let still_publishing: String =
        sqlx::query_scalar("SELECT status FROM live_tv_recordings WHERE timer_id=$1")
            .bind(publish_retry_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(still_publishing, "publishing");
    assert!(!publish_retry_partial.exists());
    sqlx::query(&format!(
        "ALTER TABLE live_tv_recordings DROP CONSTRAINT {publish_retry_constraint}"
    ))
    .execute(&pool)
    .await
    .unwrap();
    wait_for_recording_status(&pool, publish_retry_id, "completed").await;
    assert!(publish_retry_final.is_file());
    assert!(!publish_retry_partial.exists());

    // Hold the timer row while a cancel request queues before the recorder's
    // publication transition. Once released, cancellation must win and no
    // final file may be linked. This exercises the same row-lock ordering as
    // the inverse, point-of-no-return publishing check below.
    let cancel_race_id = Uuid::new_v4();
    let cancel_now = Utc::now();
    insert_timer(
        &pool,
        owner_id,
        channel_id,
        library_id,
        cancel_race_id,
        cancel_now - ChronoDuration::seconds(1),
        cancel_now + ChronoDuration::seconds(3),
    )
    .await;
    wait_for_active_recording(&pool, cancel_race_id).await;
    let mut timer_lock = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM live_tv_timers WHERE id=$1 FOR UPDATE")
        .bind(cancel_race_id)
        .fetch_one(&mut *timer_lock)
        .await
        .unwrap();
    let cancel_router = router.clone();
    let cancel_token = owner_token.clone();
    let cancel_task = tokio::spawn(async move {
        let response = cancel_router
            .oneshot(
                Request::builder()
                    .method("DELETE")
                    .uri(format!("/LiveTv/Timers/{cancel_race_id}"))
                    .header("X-Emby-Token", cancel_token)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        response.status()
    });
    // The scheduled end passes while both operations wait on our row lock;
    // the cancellation query was sent first and owns the next lock turn.
    sleep(Duration::from_secs(5)).await;
    timer_lock.commit().await.unwrap();
    assert_eq!(
        timeout(Duration::from_secs(5), cancel_task)
            .await
            .expect("cancellation completes after lock release")
            .unwrap(),
        axum::http::StatusCode::NO_CONTENT
    );
    wait_for_recording_status(&pool, cancel_race_id, "failed").await;
    let cancel_timer_status: String =
        sqlx::query_scalar("SELECT status FROM live_tv_timers WHERE id=$1")
            .bind(cancel_race_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(cancel_timer_status, "cancelled");
    timeout(Duration::from_secs(5), async {
        while root.join(recording_name(cancel_race_id)).exists()
            || root.join(partial_name(cancel_race_id)).exists()
        {
            sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("cancelled worker removes private and public output names");

    // A durable publishing row is the point of no return: the user cannot
    // cancel it after that transition. A valid final plus leftover partial
    // represents a crash just after linkat and is finalized/cleaned on restart.
    let recovered_id = Uuid::new_v4();
    let recovery_final = root.join(recording_name(recovered_id));
    let recovery_partial = root.join(partial_name(recovered_id));
    let recovered_bytes = valid_ts_bytes(4);
    fs::write(&recovery_final, &recovered_bytes).unwrap();
    fs::write(&recovery_partial, &recovered_bytes).unwrap();
    let digest = sha256_hex(&recovered_bytes);
    insert_stale_recovery_rows(
        &pool,
        owner_id,
        channel_id,
        library_id,
        RecoveryFixture {
            id: recovered_id,
            status: "publishing",
            checksum: &digest,
            size: recovered_bytes.len() as i64,
        },
    )
    .await;
    let locked_cancel_router = router.clone();
    let locked_cancel_token = owner_token.clone();
    let publishing_cancel = locked_cancel_router
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/LiveTv/Timers/{recovered_id}"))
                .header("X-Emby-Token", locked_cancel_token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(publishing_cancel.status(), axum::http::StatusCode::CONFLICT);
    assert!(recovery_final.is_file());
    assert!(recovery_partial.is_file());

    // Replacing the configured root makes cleanup unverifiable. Cancellation
    // must leave the recording claimed through shutdown so a later run can
    // recover it after the original directory is restored.
    let obstructed_id = Uuid::new_v4();
    let obstructed_now = Utc::now();
    insert_timer(
        &pool,
        owner_id,
        channel_id,
        library_id,
        obstructed_id,
        obstructed_now - ChronoDuration::seconds(1),
        obstructed_now + ChronoDuration::seconds(60),
    )
    .await;
    wait_for_active_recording(&pool, obstructed_id).await;
    wait_for_partial_file(&root, obstructed_id).await;
    let displaced_root = root.with_file_name(format!(
        "{}.displaced",
        root.file_name().unwrap().to_string_lossy()
    ));
    fs::rename(&root, &displaced_root).unwrap();
    fs::create_dir(&root).unwrap();
    let cancel_response = router
        .clone()
        .oneshot(
            Request::builder()
                .method("DELETE")
                .uri(format!("/LiveTv/Timers/{obstructed_id}"))
                .header("X-Emby-Token", &owner_token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cancel_response.status(), axum::http::StatusCode::NO_CONTENT);

    state
        .shutdown_requested
        .store(true, std::sync::atomic::Ordering::Release);
    assert!(media_features::shutdown().await);
    timeout(Duration::from_secs(2), fixture_task)
        .await
        .expect("fixture connection closes when capture stops")
        .unwrap();

    let obstructed_state = sqlx::query(
        "SELECT r.status,r.claimed_run_id,t.status AS timer_status FROM live_tv_recordings r \
         JOIN live_tv_timers t ON t.id=r.timer_id WHERE r.timer_id=$1",
    )
    .bind(obstructed_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        obstructed_state.try_get::<String, _>("status").unwrap(),
        "recording",
        "shutdown must not terminalize a row while its partial remains"
    );
    assert_eq!(
        obstructed_state
            .try_get::<Option<Uuid>, _>("claimed_run_id")
            .unwrap(),
        Some(run_id)
    );
    assert_eq!(
        obstructed_state
            .try_get::<String, _>("timer_status")
            .unwrap(),
        "cancelled"
    );
    assert!(displaced_root.join(partial_name(obstructed_id)).is_file());

    let recovery_run_id = Uuid::new_v4();
    sqlx::query("UPDATE instance_meta SET value=$1 WHERE key='active_run_id'")
        .bind(recovery_run_id.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let state_for_recovery = AppState::new_for_run(
        pool.clone(),
        state.config.clone(),
        Uuid::new_v4(),
        recovery_run_id,
        None,
    );
    assert!(
        media_features::recover_livetv(&state_for_recovery)
            .await
            .is_err(),
        "recovery must fail closed while the configured root identity is replaced"
    );
    let still_active: String =
        sqlx::query_scalar("SELECT status FROM live_tv_recordings WHERE timer_id=$1")
            .bind(obstructed_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(still_active, "recording");
    assert!(displaced_root.join(partial_name(obstructed_id)).is_file());
    fs::remove_dir(&root).unwrap();
    fs::rename(&displaced_root, &root).unwrap();

    // A recording row with only a partial is interrupted and cleaned.
    let interrupted_id = Uuid::new_v4();
    let interrupted_partial = root.join(partial_name(interrupted_id));
    fs::write(&interrupted_partial, valid_ts_bytes(1)).unwrap();
    insert_stale_recovery_rows(
        &pool,
        owner_id,
        channel_id,
        library_id,
        RecoveryFixture {
            id: interrupted_id,
            status: "recording",
            checksum: "",
            size: 0,
        },
    )
    .await;
    let recovered_count = media_features::recover_livetv(&state_for_recovery)
        .await
        .unwrap();
    assert_eq!(recovered_count, 3);
    let published_status: String =
        sqlx::query_scalar("SELECT status FROM live_tv_recordings WHERE timer_id=$1")
            .bind(recovered_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(published_status, "completed");
    assert!(
        !recovery_partial.exists(),
        "leftover partial is removed after final verification"
    );
    let interrupted_status: String =
        sqlx::query_scalar("SELECT status FROM live_tv_recordings WHERE timer_id=$1")
            .bind(interrupted_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(interrupted_status, "interrupted");
    assert!(!interrupted_partial.exists());
    let obstructed_status: String =
        sqlx::query_scalar("SELECT status FROM live_tv_recordings WHERE timer_id=$1")
            .bind(obstructed_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(obstructed_status, "interrupted");
    let obstructed_timer_status: String =
        sqlx::query_scalar("SELECT status FROM live_tv_timers WHERE id=$1")
            .bind(obstructed_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(obstructed_timer_status, "cancelled");
    assert!(!root.join(partial_name(obstructed_id)).exists());

    // Startup recovery follows the same fresh access policy. A valid durable
    // publication belonging to a user whose Live TV access was revoked is
    // discarded and marked interrupted rather than completed into the catalog.
    let revoked_recovery_id = Uuid::new_v4();
    let revoked_recovery_final = root.join(recording_name(revoked_recovery_id));
    let revoked_recovery_partial = root.join(partial_name(revoked_recovery_id));
    let revoked_bytes = valid_ts_bytes(3);
    let revoked_checksum = sha256_hex(&revoked_bytes);
    fs::write(&revoked_recovery_final, &revoked_bytes).unwrap();
    fs::write(&revoked_recovery_partial, &revoked_bytes).unwrap();
    insert_stale_recovery_rows(
        &pool,
        access_owner_id,
        channel_id,
        library_id,
        RecoveryFixture {
            id: revoked_recovery_id,
            status: "publishing",
            checksum: &revoked_checksum,
            size: revoked_bytes.len() as i64,
        },
    )
    .await;
    sqlx::query("UPDATE users SET enable_live_tv_access=FALSE WHERE id=$1")
        .bind(access_owner_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        media_features::recover_livetv(&state_for_recovery)
            .await
            .unwrap(),
        1
    );
    let revoked_recovery_status: String =
        sqlx::query_scalar("SELECT status FROM live_tv_recordings WHERE timer_id=$1")
            .bind(revoked_recovery_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(revoked_recovery_status, "interrupted");
    assert!(!revoked_recovery_final.exists());
    assert!(!revoked_recovery_partial.exists());

    drop(state_for_recovery);
    drop(state);
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .execute(&admin_pool)
        .await
        .unwrap();
    admin_pool.close().await;
    fs::remove_dir_all(root).unwrap();
}

async fn insert_user(
    pool: &sqlx::PgPool,
    username: &str,
    admin: bool,
    max_rating: Option<i16>,
    blocked_categories: &[&str],
) -> Uuid {
    let id = Uuid::new_v4();
    let blocked_categories = blocked_categories
        .iter()
        .map(|value| (*value).to_owned())
        .collect::<Vec<_>>();
    sqlx::query(
        "INSERT INTO users(id,username,username_norm,password_hash,is_admin,enable_remote_access,allow_media_playback,restrict_libraries,max_parental_rating,block_unrated_items) \
         VALUES($1,$2,$2,'unused',$3,TRUE,TRUE,NOT $3,$4,$5)",
    )
    .bind(id)
    .bind(username)
    .bind(admin)
    .bind(max_rating)
    .bind(blocked_categories)
    .execute(pool)
    .await
    .unwrap();
    id
}

async fn serve_chunked_transport_stream() -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let url = format!("http://{address}/live.ts");
    let task = tokio::spawn(async move {
        // Main capture, revoked-owner capture, SQL-retry capture, publication
        // recovery, cancel-vs-publish, and cancel-during-shutdown capture.
        for _ in 0..6 {
            let Ok((mut socket, _)) = listener.accept().await else {
                return;
            };
            let mut request = Vec::new();
            let mut buffer = [0_u8; 1024];
            loop {
                let Ok(count) = socket.read(&mut buffer).await else {
                    return;
                };
                if count == 0 {
                    return;
                }
                request.extend_from_slice(&buffer[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
                if request.len() > 8192 {
                    return;
                }
            }
            if socket
                .write_all(b"HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n")
                .await
                .is_err()
            {
                continue;
            }
            let chunk = valid_ts_bytes(8);
            let prefix = format!("{:x}\r\n", chunk.len());
            for _ in 0..1200 {
                if socket.write_all(prefix.as_bytes()).await.is_err()
                    || socket.write_all(&chunk).await.is_err()
                    || socket.write_all(b"\r\n").await.is_err()
                {
                    break;
                }
                sleep(Duration::from_millis(50)).await;
            }
        }
    });
    (url, task)
}

async fn insert_timer(
    pool: &sqlx::PgPool,
    owner_id: Uuid,
    channel_id: Uuid,
    library_id: Uuid,
    id: Uuid,
    start: chrono::DateTime<Utc>,
    end: chrono::DateTime<Utc>,
) {
    sqlx::query(
        "INSERT INTO live_tv_timers(id,owner_user_id,channel_item_id,start_at,end_at,output_library_id) VALUES($1,$2,$3,$4,$5,$6)",
    )
    .bind(id)
    .bind(owner_id)
    .bind(channel_id)
    .bind(start)
    .bind(end)
    .bind(library_id)
    .execute(pool)
    .await
    .unwrap();
}

async fn insert_active_recording(
    pool: &sqlx::PgPool,
    owner_id: Uuid,
    channel_id: Uuid,
    library_id: Uuid,
    id: Uuid,
) {
    sqlx::query(
        "INSERT INTO live_tv_timers(id,owner_user_id,channel_item_id,start_at,end_at,output_library_id,status,claimed_run_id,started_at) \
         VALUES($1,$2,$3,NOW()-INTERVAL '1 second',NOW()+INTERVAL '1 hour',$4,'recording',$5,NOW())",
    )
    .bind(id)
    .bind(owner_id)
    .bind(channel_id)
    .bind(library_id)
    .bind(Uuid::new_v4())
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO live_tv_recordings(id,timer_id,channel_item_id,library_id,channel_name,title,relative_path,status,claimed_run_id) \
         VALUES($1,$1,$2,$3,'Fixture Channel','Pending','recording-'||$1::text||'.ts','recording',$4)",
    )
    .bind(id)
    .bind(channel_id)
    .bind(library_id)
    .bind(Uuid::new_v4())
    .execute(pool)
    .await
    .unwrap();
}

struct RecoveryFixture<'a> {
    id: Uuid,
    status: &'a str,
    checksum: &'a str,
    size: i64,
}

async fn insert_stale_recovery_rows(
    pool: &sqlx::PgPool,
    owner_id: Uuid,
    channel_id: Uuid,
    library_id: Uuid,
    fixture: RecoveryFixture<'_>,
) {
    let old_run = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO live_tv_timers(id,owner_user_id,channel_item_id,start_at,end_at,output_library_id,status,claimed_run_id,started_at) \
         VALUES($1,$2,$3,NOW()-INTERVAL '1 minute',NOW()+INTERVAL '1 minute',$4,'recording',$5,NOW()-INTERVAL '30 seconds')",
    )
    .bind(fixture.id)
    .bind(owner_id)
    .bind(channel_id)
    .bind(library_id)
    .bind(old_run)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO live_tv_recordings(id,timer_id,channel_item_id,library_id,channel_name,title,relative_path,status,claimed_run_id,byte_count,sha256) \
         VALUES($1,$1,$2,$3,'Fixture Channel','Recovery Fixture',$4,$5,$6,$7,NULLIF($8,''))",
    )
    .bind(fixture.id)
    .bind(channel_id)
    .bind(library_id)
    .bind(recording_name(fixture.id))
    .bind(fixture.status)
    .bind(old_run)
    .bind(fixture.size)
    .bind(fixture.checksum)
    .execute(pool)
    .await
    .unwrap();
}

async fn wait_for_scan(pool: &sqlx::PgPool, library_id: Uuid, root: &std::path::Path) {
    timeout(Duration::from_secs(10), async {
        loop {
            if library::scan_status(pool)
                .await
                .unwrap()
                .iter()
                .any(|status| status.library_id == library_id && status.status != "running")
            {
                return;
            }
            sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("initial root scan finishes");
    assert!(
        db::library_root_identity(pool, library_id, root)
            .await
            .unwrap()
            .is_some(),
        "successful scan stores the registered root identity"
    );
}

async fn wait_for_active_recording(pool: &sqlx::PgPool, timer_id: Uuid) {
    timeout(Duration::from_secs(10), async {
        loop {
            let status: Option<String> =
                sqlx::query_scalar("SELECT status FROM live_tv_recordings WHERE timer_id=$1")
                    .bind(timer_id)
                    .fetch_optional(pool)
                    .await
                    .unwrap();
            if status.as_deref() == Some("recording") {
                return;
            }
            assert_ne!(
                status.as_deref(),
                Some("failed"),
                "recording worker failed before active state"
            );
            sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("recorder claims the due timer");
}

async fn wait_for_recording_status(pool: &sqlx::PgPool, timer_id: Uuid, expected: &str) {
    timeout(Duration::from_secs(10), async {
        loop {
            let status: Option<String> =
                sqlx::query_scalar("SELECT status FROM live_tv_recordings WHERE timer_id=$1")
                    .bind(timer_id)
                    .fetch_optional(pool)
                    .await
                    .unwrap();
            if status.as_deref() == Some(expected) {
                return;
            }
            sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("recording reaches expected final state");
}

async fn wait_for_partial_file(root: &std::path::Path, timer_id: Uuid) {
    let partial = root.join(partial_name(timer_id));
    timeout(Duration::from_secs(10), async {
        loop {
            if partial.is_file() {
                return;
            }
            sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("recording creates its partial output");
}

async fn wait_for_cleaned_active_recording(
    pool: &sqlx::PgPool,
    root: &std::path::Path,
    timer_id: Uuid,
) {
    let partial = root.join(partial_name(timer_id));
    let final_file = root.join(recording_name(timer_id));
    timeout(Duration::from_secs(10), async {
        loop {
            let status: Option<String> =
                sqlx::query_scalar("SELECT status FROM live_tv_recordings WHERE timer_id=$1")
                    .bind(timer_id)
                    .fetch_optional(pool)
                    .await
                    .unwrap();
            assert_ne!(
                status.as_deref(),
                Some("failed"),
                "failed status must wait until partial and final output cleanup completes"
            );
            assert_ne!(
                status.as_deref(),
                Some("interrupted"),
                "interrupted status must wait until partial and final output cleanup completes"
            );
            if status.as_deref() == Some("recording") && !partial.exists() && !final_file.exists() {
                return;
            }
            sleep(Duration::from_millis(30)).await;
        }
    })
    .await
    .expect("failure finalizer retries after cleanup and leaves the row active until SQL succeeds");
}

async fn wait_for_recording(pool: &sqlx::PgPool, timer_id: Uuid) {
    timeout(Duration::from_secs(10), async {
        loop {
            let status = sqlx::query_scalar::<_, Option<String>>(
                "SELECT status FROM live_tv_recordings WHERE timer_id=$1",
            )
            .bind(timer_id)
            .fetch_optional(pool)
            .await
            .unwrap()
            .flatten();
            if status.as_deref() == Some("completed") {
                return;
            }
            assert_ne!(status.as_deref(), Some("failed"), "recording worker failed");
            assert_ne!(
                status.as_deref(),
                Some("interrupted"),
                "recording was interrupted"
            );
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .expect("scheduled capture completes");
}

fn valid_ts_bytes(packet_count: usize) -> Vec<u8> {
    let mut data = vec![0_u8; packet_count * 188];
    for packet in data.as_chunks_mut::<188>().0 {
        packet[0] = 0x47;
    }
    data
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    let hash = Sha256::digest(data);
    hash.iter().map(|byte| format!("{byte:02x}")).collect()
}

fn recording_name(id: Uuid) -> String {
    format!("recording-{id}.ts")
}

fn partial_name(id: Uuid) -> String {
    format!(".puffinbox-recording-{id}.partial")
}

fn stable_channel_item_id(source_id: Uuid, external_id: &str) -> Uuid {
    let digest = sha2::Sha256::digest(format!("{source_id}:{external_id}").as_bytes());
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}
