use std::{env, fs, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use axum::{
    body::Body,
    http::{HeaderMap, Request, StatusCode},
};
use chrono::Utc;
use http_body_util::BodyExt;
use ipnet::IpNet;
use puffinbox::{AppState, Config, api, auth, db, library};
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;
use uuid::Uuid;

mod common;

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via PUFFINBOX_TEST_DATABASE_URL"]
async fn offline_quota_recovery_and_source_removal_are_transactional() {
    let database_url = env::var("PUFFINBOX_TEST_DATABASE_URL")
        .expect("set PUFFINBOX_TEST_DATABASE_URL to a disposable PostgreSQL database");
    let admin_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url)
        .await
        .unwrap();
    let schema = format!("puffinbox_offline_test_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
        .execute(&admin_pool)
        .await
        .unwrap();
    let connection_schema = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(4)
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
    let run_a = Uuid::new_v4();
    db::activate_run(&pool, run_a).await.unwrap();

    let owner = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,username,username_norm,password_hash) VALUES ($1,'offline-test','offline-test','unused')")
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    let library = Uuid::new_v4();
    db::insert_library(
        &pool,
        run_a,
        library,
        "Offline test",
        "movies",
        &[std::path::PathBuf::from("/media")],
        true,
    )
    .await
    .unwrap();
    let item = Uuid::new_v4();
    sqlx::query("INSERT INTO items(id,library_id,name,sort_name,item_type,path,path_hash,size_bytes,date_modified) VALUES ($1,$2,'fixture.mkv','fixture.mkv','Movie','/media/fixture.mkv',$3,100,NOW())")
        .bind(item)
        .bind(library)
        .bind(Uuid::new_v4().to_string())
        .execute(&pool)
        .await
        .unwrap();

    sqlx::query(
        "INSERT INTO offline_user_quotas(user_id,quota_bytes,used_bytes) VALUES ($1,1000,100)",
    )
    .bind(owner)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE offline_global_quota SET used_bytes=100 WHERE singleton=TRUE")
        .execute(&pool)
        .await
        .unwrap();
    let ready_id = Uuid::new_v4();
    let source_run = Uuid::new_v4();
    sqlx::query("INSERT INTO offline_packages(id,user_id,item_id,library_id,item_name,item_type,source_size,source_modified_at,reservation_bytes,bytes_copied,actual_size,sha256,relative_path,status,created_at,finished_at) VALUES ($1,$2,$3,$4,'fixture.mkv','Movie',100,NOW(),0,100,100,repeat('a',64),$5,'ready',NOW(),NOW())")
        .bind(ready_id)
        .bind(owner)
        .bind(item)
        .bind(library)
        .bind(format!("{source_run}/{ready_id}.media"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO offline_package_chunks(package_id,chunk_index,byte_length,sha256) VALUES ($1,0,100,repeat('a',64))")
        .bind(ready_id)
        .execute(&pool)
        .await
        .unwrap();

    sqlx::query("DELETE FROM items WHERE id=$1")
        .bind(item)
        .execute(&pool)
        .await
        .unwrap();
    let deleted_item_package = sqlx::query("SELECT status,reservation_bytes,actual_size,sha256,relative_path FROM offline_packages WHERE id=$1")
        .bind(ready_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        sqlx::Row::try_get::<String, _>(&deleted_item_package, "status").unwrap(),
        "cancelled"
    );
    assert_eq!(
        sqlx::Row::try_get::<i64, _>(&deleted_item_package, "reservation_bytes").unwrap(),
        0
    );
    assert!(
        sqlx::Row::try_get::<Option<i64>, _>(&deleted_item_package, "actual_size")
            .unwrap()
            .is_none()
    );
    assert!(
        sqlx::Row::try_get::<Option<String>, _>(&deleted_item_package, "sha256")
            .unwrap()
            .is_none()
    );
    assert!(
        sqlx::Row::try_get::<Option<String>, _>(&deleted_item_package, "relative_path")
            .unwrap()
            .is_none()
    );
    let chunks: i64 = sqlx::query_scalar(
        "SELECT count(*)::BIGINT FROM offline_package_chunks WHERE package_id=$1",
    )
    .bind(ready_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(chunks, 0);
    let quota_used: i64 =
        sqlx::query_scalar("SELECT used_bytes FROM offline_user_quotas WHERE user_id=$1")
            .bind(owner)
            .fetch_one(&pool)
            .await
            .unwrap();
    let global_used: i64 =
        sqlx::query_scalar("SELECT used_bytes FROM offline_global_quota WHERE singleton=TRUE")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((quota_used, global_used), (0, 0));
    let user_cleanup: i64 =
        sqlx::query_scalar("SELECT cleanup_bytes FROM offline_user_quotas WHERE user_id=$1")
            .bind(owner)
            .fetch_one(&pool)
            .await
            .unwrap();
    let global_cleanup: i64 =
        sqlx::query_scalar("SELECT cleanup_bytes FROM offline_global_quota WHERE singleton=TRUE")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((user_cleanup, global_cleanup), (100, 100));
    let orphan_count: i64 = sqlx::query_scalar("SELECT count(*)::BIGINT FROM offline_orphan_files")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(orphan_count, 1);

    let reservation_items = [Uuid::new_v4(), Uuid::new_v4()];
    for (index, reservation_item) in reservation_items.iter().enumerate() {
        sqlx::query("INSERT INTO items(id,library_id,name,sort_name,item_type,path,path_hash,size_bytes,date_modified) VALUES ($1,$2,$3,$3,'Movie',$4,$5,100,NOW())")
            .bind(reservation_item)
            .bind(library)
            .bind(format!("reservation-{index}.mkv"))
            .bind(format!("/media/reservation-{index}.mkv"))
            .bind(Uuid::new_v4().to_string())
            .execute(&pool)
            .await
            .unwrap();
    }
    let failure_package = Uuid::new_v4();
    let surviving_package = Uuid::new_v4();
    sqlx::query("UPDATE offline_user_quotas SET reserved_bytes=175 WHERE user_id=$1")
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE offline_global_quota SET reserved_bytes=175 WHERE singleton=TRUE")
        .execute(&pool)
        .await
        .unwrap();
    for (package_id, item_id, size) in [
        (failure_package, reservation_items[0], 100_i64),
        (surviving_package, reservation_items[1], 75_i64),
    ] {
        sqlx::query("INSERT INTO offline_packages(id,user_id,item_id,library_id,item_name,item_type,source_size,source_modified_at,reservation_bytes,status) VALUES ($1,$2,$3,$4,'reserved.mkv','Movie',$5,NOW(),$5,'queued')")
            .bind(package_id).bind(owner).bind(item_id).bind(library).bind(size)
            .execute(&pool).await.unwrap();
    }
    sqlx::query("UPDATE offline_packages SET status='failed',reservation_bytes=0,error_code='copy-failed',finished_at=NOW() WHERE id=$1")
        .bind(failure_package).execute(&pool).await.unwrap();
    let remaining_reservation: i64 =
        sqlx::query_scalar("SELECT reserved_bytes FROM offline_user_quotas WHERE user_id=$1")
            .bind(owner)
            .fetch_one(&pool)
            .await
            .unwrap();
    let remaining_global_reservation: i64 =
        sqlx::query_scalar("SELECT reserved_bytes FROM offline_global_quota WHERE singleton=TRUE")
            .fetch_one(&pool)
            .await
            .unwrap();
    let survivor_status: String =
        sqlx::query_scalar("SELECT status FROM offline_packages WHERE id=$1")
            .bind(surviving_package)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        (
            remaining_reservation,
            remaining_global_reservation,
            survivor_status.as_str()
        ),
        (75, 75, "queued")
    );

    let resumable_id = Uuid::new_v4();
    sqlx::query("INSERT INTO offline_user_quotas(user_id,quota_bytes,reserved_bytes) VALUES ($1,1000,175) ON CONFLICT(user_id) DO UPDATE SET reserved_bytes=175")
        .bind(owner)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE offline_global_quota SET reserved_bytes=175 WHERE singleton=TRUE")
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO offline_packages(id,user_id,item_id,library_id,item_name,item_type,source_size,source_modified_at,reservation_bytes,bytes_copied,status,claimed_run_id,relative_path,started_at) VALUES ($1,$2,$3,$4,'fixture.mkv','Movie',100,NOW(),100,50,'running',$5,$6,NOW())")
        .bind(resumable_id)
        .bind(owner)
        .bind(Uuid::new_v4())
        .bind(library)
        .bind(run_a)
        .bind(format!("{run_a}/{resumable_id}.partial"))
        .execute(&pool)
        .await
        .unwrap();
    let run_b = Uuid::new_v4();
    db::set_active_run_marker(&pool, run_b).await.unwrap();
    db::activate_run(&pool, run_b).await.unwrap();
    let recovered = sqlx::query("SELECT status,claimed_run_id,bytes_copied,reservation_bytes,relative_path FROM offline_packages WHERE id=$1")
        .bind(resumable_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        sqlx::Row::try_get::<String, _>(&recovered, "status").unwrap(),
        "queued"
    );
    assert!(
        sqlx::Row::try_get::<Option<Uuid>, _>(&recovered, "claimed_run_id")
            .unwrap()
            .is_none()
    );
    assert_eq!(
        sqlx::Row::try_get::<i64, _>(&recovered, "bytes_copied").unwrap(),
        0
    );
    assert_eq!(
        sqlx::Row::try_get::<i64, _>(&recovered, "reservation_bytes").unwrap(),
        100
    );
    let stale_partial = format!("{run_a}/{resumable_id}.partial");
    assert_eq!(
        sqlx::Row::try_get::<Option<String>, _>(&recovered, "relative_path").unwrap(),
        Some(stale_partial.clone())
    );
    let stale_orphan_charge: bool = sqlx::query_scalar(
        "SELECT charge_accounted FROM offline_orphan_files WHERE relative_path=$1",
    )
    .bind(&stale_partial)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(!stale_orphan_charge);
    sqlx::query("UPDATE offline_packages SET status='cancelled',claimed_run_id=NULL,reservation_bytes=0,relative_path=NULL,error_code='cancelled',finished_at=NOW() WHERE id=$1")
        .bind(resumable_id)
        .execute(&pool)
        .await
        .unwrap();
    let stale_orphan_charge: bool = sqlx::query_scalar(
        "SELECT charge_accounted FROM offline_orphan_files WHERE relative_path=$1",
    )
    .bind(&stale_partial)
    .fetch_one(&pool)
    .await
    .unwrap();
    let (owner_reserved, owner_cleanup): (i64, i64) = sqlx::query_as(
        "SELECT reserved_bytes,cleanup_bytes FROM offline_user_quotas WHERE user_id=$1",
    )
    .bind(owner)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(stale_orphan_charge);
    assert_eq!((owner_reserved, owner_cleanup), (75, 200));

    let reserved_owner = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,username,username_norm,password_hash) VALUES ($1,'offline-delete','offline-delete','unused')")
        .bind(reserved_owner)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO offline_user_quotas(user_id,quota_bytes,reserved_bytes) VALUES ($1,1000,75)",
    )
    .bind(reserved_owner)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE offline_global_quota SET reserved_bytes=150 WHERE singleton=TRUE")
        .execute(&pool)
        .await
        .unwrap();
    let queued_id = Uuid::new_v4();
    sqlx::query("INSERT INTO offline_packages(id,user_id,item_id,library_id,item_name,item_type,source_size,source_modified_at,reservation_bytes,status) VALUES ($1,$2,$3,$4,'queued.mkv','Movie',75,NOW(),75,'queued')")
        .bind(queued_id)
        .bind(reserved_owner)
        .bind(Uuid::new_v4())
        .bind(library)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM users WHERE id=$1")
        .bind(reserved_owner)
        .execute(&pool)
        .await
        .unwrap();
    let global_reserved: i64 =
        sqlx::query_scalar("SELECT reserved_bytes FROM offline_global_quota WHERE singleton=TRUE")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(global_reserved, 75);
    let user_jobs: i64 =
        sqlx::query_scalar("SELECT count(*)::BIGINT FROM offline_packages WHERE user_id=$1")
            .bind(reserved_owner)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(user_jobs, 0);

    // An item-wide cancellation and a user deletion can touch several package
    // rows and quota rows at once. Running them together catches trigger lock
    // inversions that a single-package test cannot expose.
    let race_users = [Uuid::new_v4(), Uuid::new_v4()];
    let race_item = Uuid::new_v4();
    for (index, user_id) in race_users.iter().enumerate() {
        sqlx::query(
            "INSERT INTO users(id,username,username_norm,password_hash) VALUES ($1,$2,$2,'unused')",
        )
        .bind(user_id)
        .bind(format!("offline-race-{index}"))
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query("INSERT INTO offline_user_quotas(user_id,quota_bytes,reserved_bytes) VALUES ($1,1000,25)")
            .bind(user_id)
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO items(id,library_id,name,sort_name,item_type,path,path_hash,size_bytes,date_modified) VALUES ($1,$2,'race.mkv','race.mkv','Movie','/media/race.mkv',$3,25,NOW())")
        .bind(race_item)
        .bind(library)
        .bind(Uuid::new_v4().to_string())
        .execute(&pool)
        .await
        .unwrap();
    for user_id in &race_users {
        sqlx::query("INSERT INTO offline_packages(id,user_id,item_id,library_id,item_name,item_type,source_size,source_modified_at,reservation_bytes,status) VALUES ($1,$2,$3,$4,'race.mkv','Movie',25,NOW(),25,'queued')")
            .bind(Uuid::new_v4())
            .bind(user_id)
            .bind(race_item)
            .bind(library)
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query(
        "UPDATE offline_global_quota SET reserved_bytes=reserved_bytes+50 WHERE singleton=TRUE",
    )
    .execute(&pool)
    .await
    .unwrap();
    let barrier = Arc::new(tokio::sync::Barrier::new(2));
    let delete_item_pool = pool.clone();
    let delete_item_barrier = barrier.clone();
    let delete_item = tokio::spawn(async move {
        let mut tx = delete_item_pool.begin().await.unwrap();
        delete_item_barrier.wait().await;
        sqlx::query("DELETE FROM items WHERE id=$1")
            .bind(race_item)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    });
    let delete_user_pool = pool.clone();
    let delete_user_barrier = barrier.clone();
    let delete_user_id = race_users[0];
    let delete_user = tokio::spawn(async move {
        let mut tx = delete_user_pool.begin().await.unwrap();
        delete_user_barrier.wait().await;
        sqlx::query("DELETE FROM users WHERE id=$1")
            .bind(delete_user_id)
            .execute(&mut *tx)
            .await
            .unwrap();
        tx.commit().await.unwrap();
    });
    tokio::time::timeout(Duration::from_secs(10), async {
        delete_item.await.unwrap();
        delete_user.await.unwrap();
    })
    .await
    .expect("overlapping item cancellation and user deletion must not deadlock");
    let survivor_state = sqlx::query_as::<_, (String, i64)>(
        "SELECT status,reservation_bytes FROM offline_packages WHERE user_id=$1 AND item_id=$2",
    )
    .bind(race_users[1])
    .bind(race_item)
    .fetch_optional(&pool)
    .await
    .unwrap();
    let (survivor_status, survivor_reservation) =
        survivor_state.expect("cancelled package history remains auditable");
    assert_eq!(survivor_status, "cancelled");
    assert_eq!(survivor_reservation, 0);
    let survivor_reserved: i64 =
        sqlx::query_scalar("SELECT reserved_bytes FROM offline_user_quotas WHERE user_id=$1")
            .bind(race_users[1])
            .fetch_one(&pool)
            .await
            .unwrap();
    let global_reserved_after_race: i64 =
        sqlx::query_scalar("SELECT reserved_bytes FROM offline_global_quota WHERE singleton=TRUE")
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!((survivor_reserved, global_reserved_after_race), (0, 75));

    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .execute(&admin_pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via PUFFINBOX_TEST_DATABASE_URL"]
async fn offline_http_copy_ranges_policy_and_cancel_follow_the_real_workflow() {
    let database_url = env::var("PUFFINBOX_TEST_DATABASE_URL")
        .expect("set PUFFINBOX_TEST_DATABASE_URL to a disposable PostgreSQL database");
    let admin_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url)
        .await
        .unwrap();
    let schema = format!("puffinbox_offline_http_test_{}", Uuid::new_v4().simple());
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
    let run_id = Uuid::new_v4();
    db::activate_run(&pool, run_id).await.unwrap();

    let temp_root = env::temp_dir().join(format!("puffinbox-offline-http-{}", Uuid::new_v4()));
    let media_root = temp_root.join("library");
    let data_dir = temp_root.join("data");
    fs::create_dir_all(&media_root).unwrap();
    fs::create_dir_all(&data_dir).unwrap();
    let source_path = media_root.join("offline-http-fixture.mkv");
    let content: Vec<u8> = (0..(1024 * 1024 + 37))
        .map(|index| (index % 251) as u8)
        .collect();
    fs::write(&source_path, &content).unwrap();
    let source_metadata = fs::metadata(&source_path).unwrap();
    let modified = chrono::DateTime::<Utc>::from(source_metadata.modified().unwrap());
    let modified = chrono::DateTime::from_timestamp_micros(modified.timestamp_micros()).unwrap();

    let library_id = Uuid::new_v4();
    db::insert_library(
        &pool,
        run_id,
        library_id,
        "Offline HTTP fixture",
        "movies",
        std::slice::from_ref(&media_root),
        true,
    )
    .await
    .unwrap();
    let (device, inode) = library::inspect_library_root_identity(media_root.clone())
        .await
        .unwrap();
    let root_path = media_root.to_str().unwrap();
    sqlx::query("INSERT INTO library_root_identities(library_id,root_path_hash,root_path,device_id,inode) VALUES ($1,$2,$3,$4,$5)")
        .bind(library_id)
        .bind(db::path_hash(root_path))
        .bind(root_path)
        .bind(device.to_string())
        .bind(inode.to_string())
        .execute(&pool)
        .await
        .unwrap();
    let user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,username,username_norm,password_hash,enable_remote_access) VALUES ($1,'offline-http','offline-http','unused',TRUE)")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET enable_live_tv_access=TRUE,block_unrated_items=ARRAY['LiveTvProgram']::TEXT[] WHERE id=$1")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
    let item_id = Uuid::new_v4();
    let source_text = source_path.to_str().unwrap();
    sqlx::query("INSERT INTO items(id,library_id,name,sort_name,item_type,path,path_hash,container,size_bytes,date_modified) VALUES ($1,$2,'offline-http-fixture.mkv','offline-http-fixture.mkv','Movie',$3,$4,'mkv',$5,$6)")
        .bind(item_id)
        .bind(library_id)
        .bind(source_text)
        .bind(db::path_hash(source_text))
        .bind(i64::try_from(content.len()).unwrap())
        .bind(modified)
        .execute(&pool)
        .await
        .unwrap();
    let recording_path = media_root.join("offline-http-recording.mkv");
    let recording_bytes = vec![0x47_u8; 188 * 4];
    fs::write(&recording_path, &recording_bytes).unwrap();
    let recording_modified =
        chrono::DateTime::<Utc>::from(fs::metadata(&recording_path).unwrap().modified().unwrap());
    let recording_modified =
        chrono::DateTime::from_timestamp_micros(recording_modified.timestamp_micros()).unwrap();
    let recording_item_id = Uuid::new_v4();
    let recording_text = recording_path.to_str().unwrap();
    sqlx::query("INSERT INTO items(id,library_id,name,sort_name,item_type,path,path_hash,container,size_bytes,date_modified,metadata_json) VALUES ($1,$2,'offline-http-recording.mkv','offline-http-recording.mkv','Movie',$3,$4,'mkv',$5,$6,jsonb_build_object('LiveTvRecording',TRUE))")
        .bind(recording_item_id)
        .bind(library_id)
        .bind(recording_text)
        .bind(db::path_hash(recording_text))
        .bind(i64::try_from(recording_bytes.len()).unwrap())
        .bind(recording_modified)
        .execute(&pool)
        .await
        .unwrap();
    let recording_source_id = Uuid::new_v4();
    let recording_channel_id = Uuid::new_v4();
    let recording_channel_path =
        format!("puffinbox://livetv/{recording_source_id}/{recording_channel_id}");
    sqlx::query("INSERT INTO live_tv_sources(id,library_id,name,playlist_url,origin_pins) VALUES($1,$2,'Offline rating fixture','https://example.invalid/list.m3u','[]'::jsonb)")
        .bind(recording_source_id)
        .bind(library_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO items(id,library_id,name,sort_name,item_type,path,path_hash,metadata_json) VALUES($1,$2,'Offline rating channel','offline rating channel','LiveTvChannel',$3,$4,jsonb_build_object('SourceId',$5))")
        .bind(recording_channel_id)
        .bind(library_id)
        .bind(&recording_channel_path)
        .bind(db::path_hash(&recording_channel_path))
        .bind(recording_source_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO live_tv_channels(item_id,library_id,source_id,source_channel_id,name,stream_url) VALUES($1,$2,$3,'offline-rating','Offline rating channel','https://example.invalid/live.ts')")
        .bind(recording_channel_id)
        .bind(library_id)
        .bind(recording_source_id)
        .execute(&pool)
        .await
        .unwrap();
    let recording_timer_id = Uuid::new_v4();
    sqlx::query("INSERT INTO live_tv_timers(id,owner_user_id,channel_item_id,start_at,end_at,output_library_id,status,started_at,finished_at,policy_rating_scale,policy_rating_value) VALUES($1,$2,$3,NOW()-INTERVAL '1 hour',NOW()-INTERVAL '1 second',$4,'completed',NOW()-INTERVAL '1 hour',NOW(),'US-PARENTAL-v1',50)")
        .bind(recording_timer_id)
        .bind(user_id)
        .bind(recording_channel_id)
        .bind(library_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO live_tv_recordings(id,timer_id,channel_item_id,library_id,channel_name,title,relative_path,item_id,status,byte_count,sha256,started_at,finished_at,policy_rating_scale,policy_rating_value) VALUES($1,$2,$3,$4,'Offline rating channel','Offline recording','offline-rating.mkv',$5,'completed',$6,repeat('a',64),NOW()-INTERVAL '1 hour',NOW(),'US-PARENTAL-v1',50)")
        .bind(Uuid::new_v4())
        .bind(recording_timer_id)
        .bind(recording_channel_id)
        .bind(library_id)
        .bind(recording_item_id)
        .bind(i64::try_from(recording_bytes.len()).unwrap())
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        db::get_item(&pool, recording_item_id)
            .await
            .unwrap()
            .unwrap()
            .rating,
        Some(50)
    );
    // A raw catalog score on an otherwise unrated DVR item is not trusted
    // parental provenance and must remain blocked for this user.
    let untrusted_recording_path = media_root.join("offline-http-untrusted-recording.mkv");
    let untrusted_recording_bytes = vec![0x47_u8; 188 * 4];
    fs::write(&untrusted_recording_path, &untrusted_recording_bytes).unwrap();
    let untrusted_recording_modified = chrono::DateTime::<Utc>::from(
        fs::metadata(&untrusted_recording_path)
            .unwrap()
            .modified()
            .unwrap(),
    );
    let untrusted_recording_modified =
        chrono::DateTime::from_timestamp_micros(untrusted_recording_modified.timestamp_micros())
            .unwrap();
    let untrusted_recording_item_id = Uuid::new_v4();
    let untrusted_recording_text = untrusted_recording_path.to_str().unwrap();
    sqlx::query("INSERT INTO items(id,library_id,name,sort_name,item_type,path,path_hash,container,size_bytes,date_modified,rating,metadata_json) VALUES ($1,$2,'offline-http-untrusted-recording.mkv','offline-http-untrusted-recording.mkv','Movie',$3,$4,'mkv',$5,$6,50,jsonb_build_object('LiveTvRecording',TRUE))")
        .bind(untrusted_recording_item_id)
        .bind(library_id)
        .bind(untrusted_recording_text)
        .bind(db::path_hash(untrusted_recording_text))
        .bind(i64::try_from(untrusted_recording_bytes.len()).unwrap())
        .bind(untrusted_recording_modified)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        db::get_item(&pool, untrusted_recording_item_id)
            .await
            .unwrap()
            .unwrap()
            .rating,
        None
    );
    let token = "offline-http-token";
    db::create_auth_token(
        &pool,
        run_id,
        db::NewAuthToken {
            token_id: Uuid::new_v4(),
            user_id,
            token_hash: auth::token_digest(token),
            expires_at: Utc::now() + chrono::Duration::hours(1),
            client: "offline-http-test".to_owned(),
            device_name: "offline-http-test".to_owned(),
            device_id: "offline-http-test".to_owned(),
        },
    )
    .await
    .unwrap();
    let config = Config {
        bind: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        public_base_url: None,
        database_url: database_url.clone(),
        server_name: "Offline HTTP test".to_owned(),
        web_root: PathBuf::from("web"),
        data_dir: data_dir.clone(),
        ffmpeg_path: None,
        max_scan_workers: 1,
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
    let router = api::router(state.clone());
    assert_eq!(
        call_api(&router, "GET", "/Puffinbox/Offline/Packages", None)
            .await
            .0,
        StatusCode::UNAUTHORIZED
    );
    let (status, queued_json) = call_api_json(
        &router,
        "POST",
        "/Puffinbox/Offline/Packages",
        Some(token),
        serde_json::json!({"ItemId": item_id}),
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{queued_json}");
    let package_id = Uuid::parse_str(queued_json["Id"].as_str().unwrap()).unwrap();
    let (list_status, listed) =
        call_api(&router, "GET", "/Puffinbox/Offline/Packages", Some(token)).await;
    assert_eq!(list_status, StatusCode::OK);
    assert_eq!(listed.as_array().unwrap().len(), 1);
    let (duplicate_status, duplicate) = call_api_json(
        &router,
        "POST",
        "/Puffinbox/Offline/Packages",
        Some(token),
        serde_json::json!({"ItemId": item_id}),
    )
    .await;
    assert_eq!(duplicate_status, StatusCode::OK);
    assert_eq!(duplicate["Id"], package_id.to_string());
    let (untrusted_recording_status, untrusted_recording_body) = call_api_json(
        &router,
        "POST",
        "/Puffinbox/Offline/Packages",
        Some(token),
        serde_json::json!({"ItemId": untrusted_recording_item_id}),
    )
    .await;
    assert_eq!(
        untrusted_recording_status,
        StatusCode::NOT_FOUND,
        "raw catalog ratings do not bypass a LiveTvProgram unrated block: {untrusted_recording_body}"
    );
    let (rated_recording_status, rated_recording_package) = call_api_json(
        &router,
        "POST",
        "/Puffinbox/Offline/Packages",
        Some(token),
        serde_json::json!({"ItemId": recording_item_id}),
    )
    .await;
    assert_eq!(
        rated_recording_status,
        StatusCode::ACCEPTED,
        "a completed DVR rating snapshot must satisfy a LiveTvProgram unrated block: {rated_recording_package}"
    );
    let rated_recording_package_id =
        Uuid::parse_str(rated_recording_package["Id"].as_str().unwrap()).unwrap();
    assert_eq!(
        call_api(
            &router,
            "DELETE",
            &format!("/Puffinbox/Offline/Packages/{rated_recording_package_id}"),
            Some(token),
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    puffinbox::offline::start_worker(state.clone());

    let mut ready = false;
    for _ in 0..200 {
        let (status, body) = call_api(
            &router,
            "GET",
            &format!("/Puffinbox/Offline/Packages/{package_id}"),
            Some(token),
        )
        .await;
        assert_eq!(status, StatusCode::OK, "{body}");
        match body["Status"].as_str() {
            Some("ready") => {
                ready = true;
                break;
            }
            Some("failed") => panic!("offline copy failed: {body}"),
            _ => tokio::time::sleep(Duration::from_millis(50)).await,
        }
    }
    assert!(
        ready,
        "offline worker did not finish within the test deadline"
    );

    let first = call_api_response(
        &router,
        "GET",
        &format!("/Puffinbox/Offline/Packages/{package_id}/Content"),
        Some(token),
        Some("bytes=0-1048575"),
    )
    .await;
    assert_eq!(first.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(first.headers["content-range"], "bytes 0-1048575/1048613");
    assert_eq!(
        first.headers["x-chunk-sha256"],
        sha256_hex(&content[..1024 * 1024])
    );
    assert_eq!(first.body, content[..1024 * 1024]);
    let tail = call_api_response(
        &router,
        "GET",
        &format!("/Puffinbox/Offline/Packages/{package_id}/Content"),
        Some(token),
        Some("bytes=1048576-1048612"),
    )
    .await;
    assert_eq!(tail.status, StatusCode::PARTIAL_CONTENT);
    assert_eq!(tail.body, content[1024 * 1024..]);

    sqlx::query("UPDATE users SET enable_content_downloading=FALSE WHERE id=$1")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
    for uri in [format!("/Puffinbox/Offline/Packages/{package_id}")] {
        let response = call_api(&router, "GET", &uri, Some(token)).await;
        assert_eq!(response.0, StatusCode::NOT_FOUND, "{}: {}", uri, response.1);
    }
    let revoked_content = call_api_response(
        &router,
        "GET",
        &format!("/Puffinbox/Offline/Packages/{package_id}/Content"),
        Some(token),
        Some("bytes=0-1048575"),
    )
    .await;
    assert_eq!(revoked_content.status, StatusCode::FORBIDDEN);
    let (revoked_list_status, revoked_list) =
        call_api(&router, "GET", "/Puffinbox/Offline/Packages", Some(token)).await;
    assert_eq!(revoked_list_status, StatusCode::OK);
    assert!(revoked_list.as_array().unwrap().is_empty());
    sqlx::query("UPDATE users SET enable_content_downloading=TRUE WHERE id=$1")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        call_api(
            &router,
            "DELETE",
            &format!("/Puffinbox/Offline/Packages/{package_id}"),
            Some(token),
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let cleanup_bytes: i64 =
        sqlx::query_scalar("SELECT cleanup_bytes FROM offline_user_quotas WHERE user_id=$1")
            .bind(user_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(cleanup_bytes, content.len() as i64);

    state
        .shutdown_requested
        .store(true, std::sync::atomic::Ordering::Release);
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .execute(&admin_pool)
        .await
        .unwrap();
    let _ = fs::remove_dir_all(temp_root);
}

struct ApiResponse {
    status: StatusCode,
    headers: HeaderMap,
    body: Vec<u8>,
}

async fn call_api(
    router: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
) -> (StatusCode, serde_json::Value) {
    let response = call_api_response(router, method, uri, token, None).await;
    let value = if response.body.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&response.body).unwrap()
    };
    (response.status, value)
}

async fn call_api_json(
    router: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    json: serde_json::Value,
) -> (StatusCode, serde_json::Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("Content-Type", "application/json");
    if let Some(token) = token {
        builder = builder.header("X-Emby-Token", token);
    }
    let response = router
        .clone()
        .oneshot(
            builder
                .body(Body::from(serde_json::to_vec(&json).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let value = if body.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    (status, value)
}

async fn call_api_response(
    router: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    range: Option<&str>,
) -> ApiResponse {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header("X-Emby-Token", token);
    }
    if let Some(range) = range {
        builder = builder.header("Range", range);
    }
    let response = router
        .clone()
        .oneshot(builder.body(Body::empty()).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let headers = response.headers().clone();
    let body = response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes()
        .to_vec();
    ApiResponse {
        status,
        headers,
        body,
    }
}

fn sha256_hex(data: &[u8]) -> String {
    use sha2::{Digest, Sha256};
    Sha256::digest(data)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
