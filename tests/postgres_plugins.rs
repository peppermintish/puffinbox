use std::{env, fs, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use chrono::Utc;
use http_body_util::BodyExt;
use ipnet::IpNet;
use puffinbox::{AppState, Config, api, auth, db, metadata};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;
use uuid::Uuid;

mod common;

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via PUFFINBOX_TEST_DATABASE_URL"]
async fn staged_plugin_trust_enable_refresh_and_disable_are_hash_bound() {
    let database_url = env::var("PUFFINBOX_TEST_DATABASE_URL")
        .expect("set PUFFINBOX_TEST_DATABASE_URL to a disposable PostgreSQL database");
    let admin_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url)
        .await
        .unwrap();
    let schema = format!("puffinbox_plugins_test_{}", Uuid::new_v4().simple());
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

    let admin_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,username,username_norm,password_hash,is_admin,enable_remote_access) VALUES ($1,'plugin-admin','plugin-admin','unused',TRUE,TRUE)")
        .bind(admin_id)
        .execute(&pool)
        .await
        .unwrap();
    let library_id = Uuid::new_v4();
    sqlx::query("INSERT INTO libraries(id,name,collection_type,locations) VALUES ($1,'Plugin fixture library','mixed','[]'::jsonb)")
        .bind(library_id)
        .execute(&pool)
        .await
        .unwrap();
    let item_id = Uuid::new_v4();
    let path = "/fixture/plugin-validation.mkv";
    sqlx::query("INSERT INTO items(id,library_id,name,sort_name,item_type,path,path_hash,overview) VALUES ($1,$2,'Plugin fixture','plugin fixture','Movie',$3,$4,'Catalog description')")
        .bind(item_id)
        .bind(library_id)
        .bind(path)
        .bind(db::path_hash(path))
        .execute(&pool)
        .await
        .unwrap();

    let token = "plugin-lifecycle-test-token";
    db::create_auth_token(
        &pool,
        run_id,
        db::NewAuthToken {
            token_id: Uuid::new_v4(),
            user_id: admin_id,
            token_hash: auth::token_digest(token),
            expires_at: Utc::now() + chrono::Duration::hours(1),
            client: "plugin-lifecycle-test".to_owned(),
            device_name: "plugin-lifecycle-test".to_owned(),
            device_id: "plugin-lifecycle-test".to_owned(),
        },
    )
    .await
    .unwrap();

    let data_dir = env::temp_dir().join(format!("puffinbox-plugin-test-{}", Uuid::new_v4()));
    let plugin_id = "lifecycle-fixture";
    let plugin_dir = data_dir.join("plugins").join(plugin_id);
    fs::create_dir_all(&plugin_dir).unwrap();
    let module = include_bytes!("../examples/plugins/metadata-enricher.wasm");
    let manifest = json!({
        "id": plugin_id,
        "name": "Lifecycle fixture",
        "version": "1.0.0",
        "apiVersion": 1,
        "hook": "metadata.enrich.v1",
        "module": "metadata-enricher.wasm",
        "moduleSha256": sha256_hex(module),
        "license": "MIT OR Apache-2.0",
        "provenance": "Original repository test fixture"
    });
    fs::write(
        plugin_dir.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();
    let module_path = plugin_dir.join("metadata-enricher.wasm");
    fs::write(&module_path, module).unwrap();

    let config = Config {
        bind: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        public_base_url: None,
        database_url: database_url.clone(),
        server_name: "Plugin lifecycle test".to_owned(),
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
        call_json(
            &router,
            "POST",
            "/Puffinbox/Plugins/TrustStaged",
            None,
            json!({"PluginId": plugin_id}),
        )
        .await
        .0,
        StatusCode::UNAUTHORIZED
    );

    // Text cannot be trusted by renaming it or by supplying a matching hash.
    let text = include_bytes!("../examples/plugins/metadata-enricher.wat");
    let mut text_manifest = manifest.clone();
    text_manifest["moduleSha256"] = json!(sha256_hex(text));
    fs::write(&module_path, text).unwrap();
    fs::write(
        plugin_dir.join("manifest.json"),
        serde_json::to_vec(&text_manifest).unwrap(),
    )
    .unwrap();
    let (text_status, text_body) = call_json(
        &router,
        "POST",
        "/Puffinbox/Plugins/TrustStaged",
        Some(token),
        json!({"PluginId": plugin_id}),
    )
    .await;
    assert_eq!(text_status, StatusCode::BAD_REQUEST, "{text_body}");
    assert!(text_body.to_string().contains("module-invalid"));

    text_manifest["module"] = json!("metadata-enricher.wat");
    fs::write(plugin_dir.join("metadata-enricher.wat"), text).unwrap();
    fs::write(
        plugin_dir.join("manifest.json"),
        serde_json::to_vec(&text_manifest).unwrap(),
    )
    .unwrap();
    assert_eq!(
        call_json(
            &router,
            "POST",
            "/Puffinbox/Plugins/TrustStaged",
            Some(token),
            json!({"PluginId": plugin_id}),
        )
        .await
        .0,
        StatusCode::BAD_REQUEST
    );
    let trusted: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM trusted_plugins WHERE plugin_id=$1)")
            .bind(plugin_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert!(!trusted);
    fs::write(&module_path, module).unwrap();
    fs::write(
        plugin_dir.join("manifest.json"),
        serde_json::to_vec(&manifest).unwrap(),
    )
    .unwrap();

    let (trust_status, trust_body) = call_json(
        &router,
        "POST",
        "/Puffinbox/Plugins/TrustStaged",
        Some(token),
        json!({"PluginId": plugin_id}),
    )
    .await;
    assert_eq!(trust_status, StatusCode::CREATED, "{trust_body}");
    assert_eq!(trust_body["PluginId"], plugin_id);
    assert_eq!(trust_body["Enabled"], false);

    // A changed module cannot inherit the previously recorded trust. The
    // digest is restored before the positive enable/refresh path.
    fs::write(&module_path, b"(module)").unwrap();
    let (changed_hash_status, changed_hash_body) = call_json(
        &router,
        "POST",
        &format!("/Puffinbox/Plugins/{plugin_id}/Enable"),
        Some(token),
        Value::Null,
    )
    .await;
    assert_eq!(
        changed_hash_status,
        StatusCode::CONFLICT,
        "{changed_hash_body}"
    );
    fs::write(&module_path, module).unwrap();

    assert_eq!(
        call_json(
            &router,
            "POST",
            &format!("/Puffinbox/Plugins/{plugin_id}/Enable"),
            Some(token),
            Value::Null,
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let enabled: bool = sqlx::query_scalar(
        "SELECT enabled FROM trusted_plugins WHERE plugin_id=$1 AND status='enabled'",
    )
    .bind(plugin_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(enabled);

    metadata::start_worker(state.clone());
    let (refresh_status, refresh_body) = call_json(
        &router,
        "POST",
        "/Puffinbox/Metadata/Refreshes",
        Some(token),
        json!({"ItemId": item_id, "Providers": [format!("plugin:{plugin_id}")]}),
    )
    .await;
    assert_eq!(refresh_status, StatusCode::ACCEPTED, "{refresh_body}");
    let job_id = refresh_body["Jobs"][0]["Id"]
        .as_str()
        .expect("accepted refresh includes its job ID");
    let job_id = Uuid::parse_str(job_id).unwrap();
    let mut completed = false;
    for _ in 0..240 {
        let row = sqlx::query(
            "SELECT status,items_succeeded,items_errors,last_error_code FROM metadata_refresh_runs WHERE id=$1",
        )
        .bind(job_id)
        .fetch_one(&pool)
        .await
        .unwrap();
        let status: String = sqlx::Row::try_get(&row, "status").unwrap();
        if matches!(
            status.as_str(),
            "completed" | "completed_with_errors" | "failed"
        ) {
            let succeeded: i64 = sqlx::Row::try_get(&row, "items_succeeded").unwrap();
            let errors: i64 = sqlx::Row::try_get(&row, "items_errors").unwrap();
            let error_code: Option<String> = sqlx::Row::try_get(&row, "last_error_code").unwrap();
            assert_eq!(status, "completed", "refresh error {error_code:?}");
            assert_eq!((succeeded, errors), (1, 0));
            completed = true;
            break;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    assert!(completed, "plugin metadata refresh did not finish in time");

    let (metadata_status, metadata_body) = call_json(
        &router,
        "GET",
        &format!("/Puffinbox/Metadata/Items/{item_id}"),
        Some(token),
        Value::Null,
    )
    .await;
    assert_eq!(metadata_status, StatusCode::OK, "{metadata_body}");
    assert_eq!(
        metadata_body["Preferred"]["Overview"],
        "Applied the original example metadata hook."
    );
    assert_eq!(
        metadata_body["Providers"][0]["ProviderKey"],
        format!("plugin:{plugin_id}")
    );

    assert_eq!(
        call_json(
            &router,
            "POST",
            &format!("/Puffinbox/Plugins/{plugin_id}/Disable"),
            Some(token),
            Value::Null,
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let (hidden_status, hidden_body) = call_json(
        &router,
        "GET",
        &format!("/Puffinbox/Metadata/Items/{item_id}"),
        Some(token),
        Value::Null,
    )
    .await;
    assert_eq!(hidden_status, StatusCode::OK, "{hidden_body}");
    assert!(hidden_body["Preferred"]["Overview"].is_null());
    assert_eq!(hidden_body["Providers"], json!([]));

    assert_eq!(
        call_json(
            &router,
            "POST",
            &format!("/Puffinbox/Plugins/{plugin_id}/Enable"),
            Some(token),
            Value::Null,
        )
        .await
        .0,
        StatusCode::NO_CONTENT
    );
    let (reenabled_status, reenabled_body) = call_json(
        &router,
        "GET",
        &format!("/Puffinbox/Metadata/Items/{item_id}"),
        Some(token),
        Value::Null,
    )
    .await;
    assert_eq!(reenabled_status, StatusCode::OK, "{reenabled_body}");
    assert!(
        reenabled_body["Preferred"]["Overview"].is_null(),
        "disabling and re-enabling must not revive stale plug-in output"
    );
    assert_eq!(reenabled_body["Providers"], json!([]));

    state
        .shutdown_requested
        .store(true, std::sync::atomic::Ordering::Release);
    tokio::time::sleep(Duration::from_millis(20)).await;
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .execute(&admin_pool)
        .await
        .unwrap();
    admin_pool.close().await;
    let _ = fs::remove_dir_all(data_dir);
}

async fn call_json(
    router: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    json_body: Value,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("Content-Type", "application/json");
    if let Some(token) = token {
        request = request.header("X-Emby-Token", token);
    }
    let response = router
        .clone()
        .oneshot(
            request
                .body(Body::from(serde_json::to_vec(&json_body).unwrap()))
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let body = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes).unwrap()
    };
    (status, body)
}

fn sha256_hex(bytes: &[u8]) -> String {
    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}
