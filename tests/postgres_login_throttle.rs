use std::{env, time::Duration};

use puffinbox::db;
use sqlx::postgres::PgPoolOptions;
use uuid::Uuid;

mod common;

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via PUFFINBOX_TEST_DATABASE_URL"]
async fn concurrent_new_buckets_fail_closed_at_capacity_and_pruning_reopens_admission() {
    let database_url = env::var("PUFFINBOX_TEST_DATABASE_URL")
        .expect("set PUFFINBOX_TEST_DATABASE_URL to a disposable PostgreSQL database");
    let admin_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url)
        .await
        .unwrap();
    let schema = format!("puffinbox_throttle_test_{}", Uuid::new_v4().simple());
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
    let run_id = Uuid::new_v4();
    db::activate_run(&pool, run_id).await.unwrap();
    sqlx::query("INSERT INTO login_throttles(bucket_hash,failures,window_started_at) SELECT lpad(n::TEXT,64,'0'),0,NOW() FROM generate_series(1,99998) AS source(n)")
        .execute(&pool)
        .await
        .unwrap();

    let first_bucket = "a".repeat(64);
    let second_bucket = "b".repeat(64);
    let (first, second) = tokio::join!(
        db::issue_login_failure(&pool, run_id, &first_bucket, 8),
        db::issue_login_failure(&pool, run_id, &second_bucket, 8),
    );
    let first = first.unwrap();
    let second = second.unwrap();
    assert_ne!(
        first, second,
        "one insertion must hit the capacity boundary"
    );
    assert!(db::login_bucket_locked(&pool, &first_bucket).await.unwrap());
    assert!(
        db::login_bucket_locked(&pool, &second_bucket)
            .await
            .unwrap()
    );

    let third_bucket = "c".repeat(64);
    assert!(
        db::issue_login_failure(&pool, run_id, &third_bucket, 8)
            .await
            .unwrap()
    );
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*)::BIGINT FROM login_throttles")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(count, 100_000, "the capacity marker is the only extra row");

    sqlx::query("UPDATE login_throttles SET window_started_at=NOW() - INTERVAL '2 days' WHERE bucket_hash <> $1")
        .bind("xxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxxx")
        .execute(&pool)
        .await
        .unwrap();
    db::prune_login_throttles(&pool, run_id).await.unwrap();
    assert!(!db::login_bucket_locked(&pool, &first_bucket).await.unwrap());

    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .execute(&admin_pool)
        .await
        .unwrap();
    admin_pool.close().await;
}
