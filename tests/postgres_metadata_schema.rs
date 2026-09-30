use std::{env, time::Duration};

use sqlx::{PgPool, postgres::PgPoolOptions};
use uuid::Uuid;

mod common;

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via PUFFINBOX_TEST_DATABASE_URL"]
async fn metadata_jobs_and_provider_rows_are_bounded_and_isolated() {
    let database_url = env::var("PUFFINBOX_TEST_DATABASE_URL")
        .expect("set PUFFINBOX_TEST_DATABASE_URL to a disposable PostgreSQL database");
    let admin_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url)
        .await
        .unwrap();
    let schema = format!("puffinbox_metadata_test_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
        .execute(&admin_pool)
        .await
        .unwrap();

    let connection_schema = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(2)
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

    let owner_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,username,username_norm,password_hash) VALUES ($1,'metadata-test','metadata-test','unused')")
        .bind(owner_id)
        .execute(&pool)
        .await
        .unwrap();
    let library_id = Uuid::new_v4();
    sqlx::query("INSERT INTO libraries(id,name,collection_type,locations) VALUES ($1,'Metadata test','mixed','[]'::jsonb)")
        .bind(library_id)
        .execute(&pool)
        .await
        .unwrap();
    let item_id = Uuid::new_v4();
    sqlx::query("INSERT INTO items(id,library_id,name,sort_name,item_type,path,path_hash) VALUES ($1,$2,'fixture.mkv','fixture.mkv','Movie','/fixture.mkv',$3)")
        .bind(item_id)
        .bind(library_id)
        .bind(Uuid::new_v4().to_string())
        .execute(&pool)
        .await
        .unwrap();

    let first_run = Uuid::new_v4();
    insert_library_job(&pool, first_run, library_id, owner_id, "queued")
        .await
        .unwrap();
    for active_status in ["queued", "running", "retry_wait"] {
        sqlx::query("UPDATE metadata_refresh_runs SET status=$2,claimed_run_id=CASE WHEN $2='running' THEN $3 ELSE NULL END WHERE id=$1")
            .bind(first_run)
            .bind(active_status)
            .bind(first_run)
            .execute(&pool)
            .await
            .unwrap();
        let duplicate =
            insert_library_job(&pool, Uuid::new_v4(), library_id, owner_id, "queued").await;
        let error = duplicate.expect_err("duplicate active library/provider job was admitted");
        assert_eq!(
            sqlstate(&error).as_deref(),
            Some("23505"),
            "duplicate job rejected for the wrong reason"
        );
    }
    sqlx::query("UPDATE metadata_refresh_runs SET status='completed',claimed_run_id=NULL,finished_at=NOW() WHERE id=$1")
        .bind(first_run)
        .execute(&pool)
        .await
        .unwrap();
    let replacement_run = Uuid::new_v4();
    insert_library_job(&pool, replacement_run, library_id, owner_id, "queued")
        .await
        .expect("a completed job must permit a later refresh");

    let item_run = Uuid::new_v4();
    insert_item_job(&pool, item_run, item_id, owner_id, "queued")
        .await
        .unwrap();
    let duplicate_item =
        insert_item_job(&pool, Uuid::new_v4(), item_id, owner_id, "retry_wait").await;
    let duplicate_item_error =
        duplicate_item.expect_err("duplicate active item/provider job was admitted");
    assert_eq!(
        sqlstate(&duplicate_item_error).as_deref(),
        Some("23505"),
        "duplicate item job rejected for the wrong reason"
    );

    sqlx::query("INSERT INTO item_metadata(item_id,provider_key,title) VALUES ($1,'local-nfo','catalog-owned sidecar row')")
        .bind(item_id)
        .execute(&pool)
        .await
        .unwrap();

    // Library deletion leaves immutable refresh scope/history intact, while
    // catalog-owned provider rows still cascade with their item.
    sqlx::query("DELETE FROM libraries WHERE id=$1")
        .bind(library_id)
        .execute(&pool)
        .await
        .unwrap();
    let historical =
        sqlx::query_scalar::<_, Uuid>("SELECT id FROM metadata_refresh_runs WHERE id=$1")
            .bind(replacement_run)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(historical, replacement_run);
    let removed_metadata: i64 =
        sqlx::query_scalar("SELECT count(*) FROM item_metadata WHERE item_id=$1")
            .bind(item_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        removed_metadata, 0,
        "provider rows must cascade with deleted catalog items"
    );

    let second_library_id = Uuid::new_v4();
    sqlx::query("INSERT INTO libraries(id,name,collection_type,locations) VALUES ($1,'Metadata rows','mixed','[]'::jsonb)")
        .bind(second_library_id)
        .execute(&pool)
        .await
        .unwrap();
    let second_item_id = Uuid::new_v4();
    sqlx::query("INSERT INTO items(id,library_id,name,sort_name,item_type,path,path_hash) VALUES ($1,$2,'metadata.mkv','metadata.mkv','Movie','/metadata.mkv',$3)")
        .bind(second_item_id)
        .bind(second_library_id)
        .bind(Uuid::new_v4().to_string())
        .execute(&pool)
        .await
        .unwrap();
    for provider in ["local-nfo", "tvmaze"] {
        sqlx::query("INSERT INTO item_metadata(item_id,provider_key,title) VALUES ($1,$2,'provider-specific title')")
            .bind(second_item_id)
            .bind(provider)
            .execute(&pool)
            .await
            .unwrap();
    }
    let provider_count: i64 =
        sqlx::query_scalar("SELECT count(*) FROM item_metadata WHERE item_id=$1")
            .bind(second_item_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        provider_count, 2,
        "one provider must not erase another provider's row"
    );

    let plugin_key = format!("plugin:{}", "a".repeat(64));
    sqlx::query(
        "INSERT INTO item_metadata(item_id,provider_key,title) VALUES ($1,$2,'bounded plugin key')",
    )
    .bind(second_item_id)
    .bind(&plugin_key)
    .execute(&pool)
    .await
    .expect("64-character plugin IDs should use the same bound everywhere");
    sqlx::query("INSERT INTO trusted_plugins(plugin_id,name,version,api_version,manifest_sha256,binary_sha256,declared_license,declared_provenance) VALUES ($1,'Fixture plugin','1.0.0',1,repeat('a',64),repeat('b',64),'MIT OR Apache-2.0','local original source')")
        .bind("a".repeat(64))
        .execute(&pool)
        .await
        .expect("64-character plugin IDs should use the same bound everywhere");
    let too_long_plugin_key = format!("plugin:{}", "a".repeat(65));
    let plugin_key_rejected =
        sqlx::query("INSERT INTO item_metadata(item_id,provider_key) VALUES ($1,$2)")
            .bind(second_item_id)
            .bind(too_long_plugin_key)
            .execute(&pool)
            .await;
    let key_error = plugin_key_rejected.expect_err("65-character plugin ID was accepted");
    assert_eq!(
        sqlstate(&key_error).as_deref(),
        Some("23514"),
        "long provider key rejected for the wrong reason"
    );
    let too_long_plugin_id = sqlx::query("INSERT INTO trusted_plugins(plugin_id,name,version,api_version,manifest_sha256,binary_sha256,declared_license,declared_provenance) VALUES ($1,'Long fixture plugin','1.0.0',1,repeat('a',64),repeat('b',64),'MIT OR Apache-2.0','local original source')")
        .bind("a".repeat(65))
        .execute(&pool)
        .await;
    let id_error = too_long_plugin_id.expect_err("65-character plugin ID was accepted");
    assert_eq!(
        sqlstate(&id_error).as_deref(),
        Some("23514"),
        "long plugin ID rejected for the wrong reason"
    );

    let artwork_mismatch = sqlx::query("UPDATE item_metadata SET artwork_mime='image/png',artwork_size=2,artwork_sha256=repeat('a',64),artwork_bytes=decode('00','hex') WHERE item_id=$1 AND provider_key='tvmaze'")
        .bind(second_item_id)
        .execute(&pool)
        .await;
    let artwork_error = artwork_mismatch.expect_err("artwork byte size mismatch was accepted");
    assert_eq!(
        sqlstate(&artwork_error).as_deref(),
        Some("23514"),
        "artwork size rejected for the wrong reason"
    );
    let null_classification = sqlx::query("UPDATE item_metadata SET policy_rating_scale='US-MPAA-v1',policy_rating_value=NULL WHERE item_id=$1 AND provider_key='local-nfo'")
        .bind(second_item_id)
        .execute(&pool)
        .await;
    let rating_error =
        null_classification.expect_err("incomplete classification tuple was accepted");
    assert_eq!(
        sqlstate(&rating_error).as_deref(),
        Some("23514"),
        "incomplete rating rejected for the wrong reason"
    );

    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .execute(&admin_pool)
        .await
        .unwrap();
}

fn sqlstate(error: &sqlx::Error) -> Option<String> {
    error
        .as_database_error()
        .and_then(|database| database.code())
        .map(|code| code.into_owned())
}

async fn insert_library_job(
    pool: &PgPool,
    id: Uuid,
    library_id: Uuid,
    requested_by: Uuid,
    status: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO metadata_refresh_runs(id,scope_kind,scope_library_id,scope_library_name,requested_by,provider_key,status) VALUES ($1,'library',$2,'Metadata test',$3,'local-nfo',$4)")
        .bind(id)
        .bind(library_id)
        .bind(requested_by)
        .bind(status)
        .execute(pool)
        .await?;
    Ok(())
}

async fn insert_item_job(
    pool: &PgPool,
    id: Uuid,
    item_id: Uuid,
    requested_by: Uuid,
    status: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO metadata_refresh_runs(id,scope_kind,scope_item_id,requested_by,provider_key,status) VALUES ($1,'item',$2,$3,'local-nfo',$4)")
        .bind(id)
        .bind(item_id)
        .bind(requested_by)
        .bind(status)
        .execute(pool)
        .await?;
    Ok(())
}
