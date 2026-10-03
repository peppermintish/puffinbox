use std::{env, fs, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use axum::{
    body::Body,
    http::{Request, StatusCode},
    response::Response,
};
use base64::Engine;
use http_body_util::BodyExt;
use ipnet::IpNet;
use puffinbox::{AppState, Config, api, auth, db, library, metadata};
use serde_json::{Value, json};
use sqlx::{Row, postgres::PgPoolOptions, types::Json};
use tower::ServiceExt;
use uuid::Uuid;

mod common;

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via PUFFINBOX_TEST_DATABASE_URL"]
async fn local_nfo_worker_updates_catalog_dto_and_primary_artwork_route() {
    let database_url = env::var("PUFFINBOX_TEST_DATABASE_URL")
        .expect("set PUFFINBOX_TEST_DATABASE_URL to a disposable PostgreSQL database");
    let admin_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url)
        .await
        .unwrap();
    let schema = format!("puffinbox_metadata_catalog_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
        .execute(&admin_pool)
        .await
        .unwrap();

    let connection_schema = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(5)
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
    let root_dir = env::temp_dir().join(format!("puffinbox-metadata-catalog-{}", Uuid::new_v4()));
    fs::create_dir_all(&root_dir).unwrap();
    let poster_only_path = root_dir.join("Poster Only.mkv");
    let classified_path = root_dir.join("Classified Movie.mkv");
    fs::write(&poster_only_path, b"movie fixture").unwrap();
    fs::write(&classified_path, b"movie fixture").unwrap();
    fs::write(
        root_dir.join("Classified Movie.nfo"),
        b"<movie><title>Classified Provider Title</title><plot>Policy description</plot><mpaa>PG-13</mpaa><genre>Drama</genre><tag>Sidecar tag</tag><year>2020</year></movie>",
    )
    .unwrap();
    // A generated, valid 1x1 RGBA PNG keeps the transport/ETag assertion
    // grounded in decodable image bytes rather than a signature-only stub.
    let poster_bytes = base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==")
        .unwrap();
    fs::write(root_dir.join("Poster Only-poster.png"), &poster_bytes).unwrap();
    fs::write(root_dir.join("Classified Movie-poster.png"), &poster_bytes).unwrap();
    let root_dir = fs::canonicalize(root_dir).unwrap();
    let root_text = root_dir.to_str().unwrap().to_owned();
    let (device_id, inode) = library::inspect_library_root_identity(root_dir.clone())
        .await
        .unwrap();

    let library_id = Uuid::new_v4();
    sqlx::query("INSERT INTO libraries(id,name,collection_type,locations) VALUES ($1,'Metadata catalog fixture','movies',$2)")
        .bind(library_id)
        .bind(Json(vec![root_text.clone()]))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO library_root_identities(library_id,root_path_hash,root_path,device_id,inode) VALUES ($1,$2,$3,$4,$5)")
        .bind(library_id)
        .bind(db::path_hash(&root_text))
        .bind(&root_text)
        .bind(device_id.to_string())
        .bind(inode.to_string())
        .execute(&pool)
        .await
        .unwrap();

    let admin_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,username,username_norm,password_hash,is_admin,enable_remote_access) VALUES ($1,'metadata-catalog-admin','metadata-catalog-admin','unused',TRUE,TRUE)")
        .bind(admin_id)
        .execute(&pool)
        .await
        .unwrap();
    let poster_item_id = Uuid::new_v4();
    insert_item(&pool, poster_item_id, library_id, &poster_only_path).await;
    let classified_item_id = Uuid::new_v4();
    insert_item(&pool, classified_item_id, library_id, &classified_path).await;

    let data_dir = env::temp_dir().join(format!("puffinbox-metadata-data-{}", Uuid::new_v4()));
    fs::create_dir_all(&data_dir).unwrap();
    let config = Config {
        bind: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        public_base_url: None,
        database_url: database_url.clone(),
        server_name: "Metadata catalog test".to_owned(),
        web_root: PathBuf::from("web"),
        data_dir: data_dir.clone(),
        ffmpeg_path: None,
        max_scan_workers: 1,
        max_page_size: 10_000,
        access_token_lifetime_hours: 24,
        cookie_secure: false,
        cors_origins: Vec::new(),
        trusted_proxies: Vec::new(),
        local_networks: vec!["127.0.0.0/8".parse::<IpNet>().unwrap()],
        setup_token: None,
        bootstrap_admin_username: None,
        bootstrap_admin_password: None,
    };
    let state = AppState::new_for_run(pool.clone(), Arc::new(config), Uuid::new_v4(), run_id, None);
    let admin = db::get_user(&pool, admin_id).await.unwrap().unwrap();
    let token = auth::issue_token(
        &state,
        &admin,
        "metadata-test",
        "test-client",
        "metadata-catalog",
    )
    .await
    .unwrap()
    .token;
    let router = api::router(state.clone());
    metadata::start_worker(state.clone());

    let static_response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/web/app.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        static_response.headers()["cache-control"],
        "no-cache, must-revalidate"
    );
    let served_app = static_response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let served_app = String::from_utf8(served_app.to_vec()).unwrap();
    assert!(served_app.contains("window.PuffinboxClientCompat.replaceChildren"));
    assert!(!served_app.contains("grid.replaceChildren"));
    let index_response = router
        .clone()
        .oneshot(Request::builder().uri("/web/").body(Body::empty()).unwrap())
        .await
        .unwrap();
    let index = index_response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let index = String::from_utf8(index.to_vec()).unwrap();
    assert!(index.find("client-compat.js").unwrap() < index.find("app.js").unwrap());

    let offline_worker = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/web/offline-sw.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(offline_worker.status(), StatusCode::OK);
    assert_eq!(
        offline_worker.headers()["cache-control"],
        "no-cache, must-revalidate"
    );
    let worker_body = offline_worker
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let worker_body = String::from_utf8(worker_body.to_vec()).unwrap();
    assert!(worker_body.contains("self.addEventListener('fetch'"));

    let offline_cache = router
        .clone()
        .oneshot(
            Request::builder()
                .uri("/web/offline-cache.js")
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(offline_cache.status(), StatusCode::OK);

    let poster_job = enqueue_local_refresh(&router, &token, poster_item_id).await;
    let policy_job = enqueue_local_refresh(&router, &token, classified_item_id).await;
    wait_for_completed_job(&pool, poster_job).await;
    wait_for_completed_job(&pool, policy_job).await;

    let (item_status, item_body) = call_json(
        &router,
        "GET",
        &format!("/Items/{poster_item_id}"),
        &token,
        None,
        None,
    )
    .await;
    assert_eq!(item_status, StatusCode::OK, "{item_body}");
    assert!(item_body["OfficialRating"].is_null());
    assert_eq!(
        item_body["ImageTags"]["Primary"].as_str().unwrap().len(),
        64
    );

    let (browse_status, browse_body) = call_json(
        &router,
        "GET",
        "/Items?IncludeItemTypes=Movie&Limit=100&Recursive=true",
        &token,
        None,
        None,
    )
    .await;
    assert_eq!(browse_status, StatusCode::OK, "{browse_body}");
    let listed = browse_body["Items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["Id"] == poster_item_id.to_string())
        .expect("poster-only item appears in ordinary catalog browse");
    assert_eq!(
        listed["ImageTags"]["Primary"],
        item_body["ImageTags"]["Primary"]
    );

    let image_uri = format!("/Items/{poster_item_id}/Images/Primary");
    let image_response = call_raw(&router, "GET", &image_uri, &token, None, None).await;
    assert_eq!(image_response.status(), StatusCode::OK);
    assert_eq!(image_response.headers()["content-type"], "image/png");
    assert_eq!(
        image_response.headers()["cache-control"],
        "private, max-age=0, must-revalidate"
    );
    let etag = image_response.headers()["etag"]
        .to_str()
        .unwrap()
        .to_owned();
    let image_bytes = image_response
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    assert_eq!(&image_bytes[..], &poster_bytes);
    assert!(image_bytes.starts_with(b"\x89PNG\r\n\x1a\n"));
    assert_eq!(&image_bytes[12..16], b"IHDR");
    assert_eq!(
        u32::from_be_bytes(image_bytes[16..20].try_into().unwrap()),
        1
    );
    assert_eq!(
        u32::from_be_bytes(image_bytes[20..24].try_into().unwrap()),
        1
    );
    assert!(image_bytes.windows(4).any(|chunk| chunk == b"IDAT"));
    assert!(image_bytes.ends_with(b"IEND\xaeB`\x82"));
    assert_eq!(etag, format!("\"{}\"", sha256_hex(&image_bytes)));
    let cached = call_raw(&router, "GET", &image_uri, &token, None, Some(&etag)).await;
    assert_eq!(cached.status(), StatusCode::NOT_MODIFIED);
    let unauthenticated = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(&image_uri)
                .header("If-None-Match", &etag)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

    let denied_user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,username,username_norm,password_hash,is_admin,enable_remote_access,restrict_libraries) VALUES ($1,'metadata-denied-user','metadata-denied-user','unused',FALSE,TRUE,TRUE)")
        .bind(denied_user_id)
        .execute(&pool)
        .await
        .unwrap();
    let denied_user = db::get_user(&pool, denied_user_id).await.unwrap().unwrap();
    let denied_token = auth::issue_token(
        &state,
        &denied_user,
        "metadata-test",
        "test-client",
        "metadata-denied",
    )
    .await
    .unwrap()
    .token;
    let denied_image = call_raw(&router, "GET", &image_uri, &denied_token, None, None).await;
    assert_eq!(denied_image.status(), StatusCode::NOT_FOUND);

    let (classified_status, classified_body) = call_json(
        &router,
        "GET",
        &format!("/Items/{classified_item_id}"),
        &token,
        None,
        None,
    )
    .await;
    assert_eq!(classified_status, StatusCode::OK, "{classified_body}");
    assert_eq!(classified_body["Name"], "Classified Provider Title");
    assert_eq!(classified_body["Overview"], "Policy description");
    assert_eq!(classified_body["Genres"][0], "Drama");
    assert_eq!(classified_body["Tags"], json!(["Sidecar tag"]));
    assert_eq!(classified_body["ProductionYear"], 2020);
    assert_eq!(classified_body["OfficialRating"], "PG-13");
    let (filters_status, filters_body) =
        call_json(&router, "GET", "/Items/Filters", &token, None, None).await;
    assert_eq!(filters_status, StatusCode::OK);
    assert_eq!(filters_body["Tags"], json!(["Sidecar tag"]));
    assert!(
        filters_body["Years"]
            .as_array()
            .unwrap()
            .contains(&json!(2020))
    );
    let (hints_status, hints_body) = call_json(
        &router,
        "GET",
        "/Search/Hints?SearchTerm=Classified%20Movie.mkv",
        &token,
        None,
        None,
    )
    .await;
    assert_eq!(hints_status, StatusCode::OK, "{hints_body}");
    let hint = hints_body["SearchHints"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["Id"] == classified_item_id.to_string())
        .expect("classified item appears in search hints");
    assert_eq!(hint["Name"], "Classified Provider Title");
    assert_eq!(hint["OfficialRating"], "PG-13");
    let (classified_row, classified_rating): (String, Option<i16>) = sqlx::query_as(
        "SELECT content_rating,policy_rating_value FROM item_metadata WHERE item_id=$1 AND provider_key='local-nfo'",
    )
    .bind(classified_item_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(classified_row, "PG-13");
    assert_eq!(classified_rating, Some(50));

    let policy_user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,username,username_norm,password_hash,is_admin,enable_remote_access,restrict_libraries,max_parental_rating,block_unrated_items) VALUES ($1,'metadata-policy-user','metadata-policy-user','unused',FALSE,TRUE,TRUE,100,ARRAY['Movie']::TEXT[])")
        .bind(policy_user_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO user_library_access(user_id,library_id) VALUES ($1,$2)")
        .bind(policy_user_id)
        .bind(library_id)
        .execute(&pool)
        .await
        .unwrap();
    let policy_user = db::get_user(&pool, policy_user_id).await.unwrap().unwrap();
    let policy_token = auth::issue_token(
        &state,
        &policy_user,
        "metadata-test",
        "test-client",
        "metadata-policy",
    )
    .await
    .unwrap()
    .token;
    let (rated_status, rated_body) = call_json(
        &router,
        "GET",
        &format!("/Items/{classified_item_id}"),
        &policy_token,
        None,
        None,
    )
    .await;
    assert_eq!(rated_status, StatusCode::OK, "{rated_body}");

    // Removing the NFO clears its stale text and parental label even if the
    // replacement poster is unsafe; the previously stored artwork is retained.
    fs::remove_file(root_dir.join("Classified Movie.nfo")).unwrap();
    fs::write(
        root_dir.join("Classified Movie-poster.jpg"),
        vec![0_u8; 4 * 1024 * 1024 + 1],
    )
    .unwrap();
    let removed_nfo_job = enqueue_local_refresh(&router, &token, classified_item_id).await;
    wait_for_completed_job(&pool, removed_nfo_job).await;
    let cleared_row = sqlx::query("SELECT content_rating,policy_rating_value,title,artwork_bytes,artwork_sha256 FROM item_metadata WHERE item_id=$1 AND provider_key='local-nfo'")
        .bind(classified_item_id)
        .fetch_one(&pool)
        .await
        .unwrap();
    let content_rating: Option<String> = cleared_row.try_get("content_rating").unwrap();
    let policy_rating: Option<i16> = cleared_row.try_get("policy_rating_value").unwrap();
    let title: Option<String> = cleared_row.try_get("title").unwrap();
    let artwork_bytes: Option<Vec<u8>> = cleared_row.try_get("artwork_bytes").unwrap();
    let artwork_sha: Option<String> = cleared_row.try_get("artwork_sha256").unwrap();
    assert_eq!(content_rating, None);
    assert_eq!(policy_rating, None);
    assert_eq!(title, None);
    assert_eq!(artwork_bytes.as_deref(), Some(poster_bytes.as_slice()));
    let expected_artwork_sha = sha256_hex(&poster_bytes);
    assert_eq!(artwork_sha.as_deref(), Some(expected_artwork_sha.as_str()));
    let (unrated_status, unrated_body) = call_json(
        &router,
        "GET",
        &format!("/Items/{classified_item_id}"),
        &policy_token,
        None,
        None,
    )
    .await;
    assert_eq!(unrated_status, StatusCode::NOT_FOUND, "{unrated_body}");

    // Standard movie.nfo precedes the basename sidecar, as observed through
    // the opaque reference API. A rejected selected file cannot borrow a rating.
    let standard_dir = root_dir.join("Standard movie");
    fs::create_dir(&standard_dir).unwrap();
    let standard_path = standard_dir.join("Feature.mkv");
    let basename_nfo = standard_dir.join("Feature.nfo");
    let standard_nfo = standard_dir.join("movie.nfo");
    fs::write(&standard_path, b"movie fixture").unwrap();
    fs::write(
        &standard_nfo,
        b"<movie><title>Standard Movie Title</title><mpaa>G</mpaa><studio>Alpha Film Studio</studio><studio>Beta Film Studio</studio></movie>",
    )
    .unwrap();
    let standard_item_id = Uuid::new_v4();
    insert_item(&pool, standard_item_id, library_id, &standard_path).await;
    let job = enqueue_local_refresh(&router, &token, standard_item_id).await;
    wait_for_completed_job(&pool, job).await;
    let (status, standard_body) = call_json(
        &router,
        "GET",
        &format!("/Items/{standard_item_id}"),
        &policy_token,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{standard_body}");
    assert_eq!(standard_body["Name"], "Standard Movie Title");
    assert_eq!(standard_body["OfficialRating"], "G");
    let studios = standard_body["Studios"].as_array().unwrap();
    assert_eq!(studios.len(), 2);
    assert_eq!(studios[0]["Name"], "Alpha Film Studio");
    assert_eq!(studios[1]["Name"], "Beta Film Studio");
    let studio_id = studios[0]["Id"].as_str().unwrap();
    let (status, selected) = call_json(
        &router,
        "GET",
        &format!("/Items?Recursive=true&IncludeItemTypes=Movie&StudioIds={studio_id}"),
        &policy_token,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK, "{selected}");
    assert_eq!(selected["TotalRecordCount"], 1);
    assert_eq!(selected["Items"][0]["Id"], standard_item_id.to_string());

    fs::write(
        &basename_nfo,
        b"<movie><title>Preferred Basename Title</title><mpaa>PG-13</mpaa><studio>Preferred Studio</studio></movie>",
    )
    .unwrap();
    let job = enqueue_local_refresh(&router, &token, standard_item_id).await;
    wait_for_completed_job(&pool, job).await;
    let (status, preferred) = call_json(
        &router,
        "GET",
        &format!("/Items/{standard_item_id}"),
        &token,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(preferred["Name"], "Standard Movie Title");
    assert_eq!(preferred["OfficialRating"], "G");
    assert_eq!(preferred["Studios"][0]["Name"], "Alpha Film Studio");

    fs::remove_file(&standard_nfo).unwrap();
    let job = enqueue_local_refresh(&router, &token, standard_item_id).await;
    wait_for_completed_job(&pool, job).await;
    let (status, basename_only) = call_json(
        &router,
        "GET",
        &format!("/Items/{standard_item_id}"),
        &token,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(basename_only["Name"], "Preferred Basename Title");
    assert_eq!(basename_only["OfficialRating"], "PG-13");
    assert_eq!(basename_only["Studios"][0]["Name"], "Preferred Studio");

    fs::write(&standard_nfo, b"<movie><title>Invalid").unwrap();
    let job = enqueue_local_refresh(&router, &token, standard_item_id).await;
    wait_for_job(&pool, job, Some("nfo-invalid-document")).await;
    let (status, denied) = call_json(
        &router,
        "GET",
        &format!("/Items/{standard_item_id}"),
        &policy_token,
        None,
        None,
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND, "{denied}");

    fs::remove_file(&standard_nfo).unwrap();
    let job = enqueue_local_refresh(&router, &token, standard_item_id).await;
    wait_for_completed_job(&pool, job).await;
    #[cfg(unix)]
    {
        std::os::unix::fs::symlink(&basename_nfo, &standard_nfo).unwrap();
        let job = enqueue_local_refresh(&router, &token, standard_item_id).await;
        wait_for_job(&pool, job, Some("nfo-sidecar-unsafe")).await;
        let (status, denied) = call_json(
            &router,
            "GET",
            &format!("/Items/{standard_item_id}"),
            &policy_token,
            None,
            None,
        )
        .await;
        assert_eq!(status, StatusCode::NOT_FOUND, "{denied}");
        fs::remove_file(&standard_nfo).unwrap();
    }
    fs::remove_file(&basename_nfo).unwrap();
    let job = enqueue_local_refresh(&router, &token, standard_item_id).await;
    wait_for_completed_job(&pool, job).await;
    let remaining_metadata: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM item_metadata WHERE item_id=$1 AND provider_key='local-nfo'",
    )
    .bind(standard_item_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(remaining_metadata, 0);

    state
        .shutdown_requested
        .store(true, std::sync::atomic::Ordering::Release);
    tokio::time::sleep(Duration::from_millis(1100)).await;
    drop(router);
    drop(state);
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .execute(&admin_pool)
        .await
        .unwrap();
    admin_pool.close().await;
    let _ = fs::remove_dir_all(data_dir);
    let _ = fs::remove_dir_all(root_dir);
}

async fn insert_item(pool: &sqlx::PgPool, id: Uuid, library_id: Uuid, path: &std::path::Path) {
    let name = path.file_name().unwrap().to_string_lossy().into_owned();
    let path = path.to_str().unwrap().to_owned();
    sqlx::query("INSERT INTO items(id,library_id,name,sort_name,item_type,path,path_hash) VALUES ($1,$2,$3,$4,'Movie',$5,$6)")
        .bind(id)
        .bind(library_id)
        .bind(&name)
        .bind(name.to_lowercase())
        .bind(&path)
        .bind(db::path_hash(&path))
        .execute(pool)
        .await
        .unwrap();
}

fn sha256_hex(bytes: &[u8]) -> String {
    use sha2::{Digest, Sha256};

    Sha256::digest(bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

async fn enqueue_local_refresh(router: &axum::Router, token: &str, item_id: Uuid) -> Uuid {
    let (status, body) = call_json(
        router,
        "POST",
        "/Puffinbox/Metadata/Refreshes",
        token,
        Some(json!({ "ItemId": item_id, "Providers": ["local-nfo"] })),
        None,
    )
    .await;
    assert_eq!(status, StatusCode::ACCEPTED, "{body}");
    Uuid::parse_str(body["Jobs"][0]["Id"].as_str().unwrap()).unwrap()
}

async fn wait_for_completed_job(pool: &sqlx::PgPool, id: Uuid) {
    wait_for_job(pool, id, None).await;
}

async fn wait_for_job(pool: &sqlx::PgPool, id: Uuid, expected_error: Option<&str>) {
    for _ in 0..200 {
        let row = sqlx::query("SELECT status,items_succeeded,items_errors,last_error_code FROM metadata_refresh_runs WHERE id=$1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap();
        let status: String = row.try_get("status").unwrap();
        if matches!(
            status.as_str(),
            "completed" | "completed_with_errors" | "failed"
        ) {
            let succeeded: i64 = row.try_get("items_succeeded").unwrap();
            let errors: i64 = row.try_get("items_errors").unwrap();
            let error: Option<String> = row.try_get("last_error_code").unwrap();
            let (expected_status, expected_counts) = if expected_error.is_some() {
                ("completed_with_errors", (0, 1))
            } else {
                ("completed", (1, 0))
            };
            assert_eq!(status, expected_status, "metadata job error {error:?}");
            assert_eq!(
                (succeeded, errors),
                expected_counts,
                "metadata job error {error:?}"
            );
            assert_eq!(error.as_deref(), expected_error);
            return;
        }
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    panic!("metadata job {id} did not complete in time");
}

async fn call_json(
    router: &axum::Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<Value>,
    if_none_match: Option<&str>,
) -> (StatusCode, Value) {
    let response = call_raw(router, method, uri, token, body, if_none_match).await;
    let status = response.status();
    let bytes = response.into_body().collect().await.unwrap().to_bytes();
    let json = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({ "body": String::from_utf8_lossy(&bytes) }))
    };
    (status, json)
}

async fn call_raw(
    router: &axum::Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<Value>,
    if_none_match: Option<&str>,
) -> Response {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("X-Emby-Token", token);
    if body.is_some() {
        builder = builder.header("Content-Type", "application/json");
    }
    if let Some(value) = if_none_match {
        builder = builder.header("If-None-Match", value);
    }
    let body = body
        .map(|value| Body::from(serde_json::to_vec(&value).unwrap()))
        .unwrap_or_else(Body::empty);
    router
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap()
}
