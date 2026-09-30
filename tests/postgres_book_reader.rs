use std::{env, fs, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use http_body_util::BodyExt;
use ipnet::IpNet;
use puffinbox::{AppState, Config, api, auth, db, library};
use sqlx::{postgres::PgPoolOptions, types::Json};
use tower::ServiceExt;
use uuid::Uuid;

mod common;

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via PUFFINBOX_TEST_DATABASE_URL"]
async fn book_reader_serves_pdf_epub_and_enforces_user_library_parental_and_download_policy() {
    let database_url = env::var("PUFFINBOX_TEST_DATABASE_URL")
        .expect("set PUFFINBOX_TEST_DATABASE_URL to a disposable PostgreSQL database");
    let admin_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url)
        .await
        .unwrap();
    let schema = format!("puffinbox_books_test_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
        .execute(&admin_pool)
        .await
        .unwrap();

    let connection_schema = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(6)
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

    let media_root = env::temp_dir().join(format!("puffinbox-books-{}", Uuid::new_v4()));
    fs::create_dir_all(&media_root).unwrap();
    fs::write(media_root.join("Sample.PDF"), b"%PDF-1.7\nfixture").unwrap();
    fs::write(media_root.join("Sample.epub"), b"PK\x03\x04fixture").unwrap();
    fs::write(media_root.join("Rated.epub"), b"PK\x03\x04rated").unwrap();
    let media_root = fs::canonicalize(media_root).unwrap();
    let root_text = media_root.to_str().unwrap().to_owned();
    let (device_id, inode) = library::inspect_library_root_identity(media_root.clone())
        .await
        .unwrap();

    let library_id = Uuid::new_v4();
    sqlx::query("INSERT INTO libraries(id,name,collection_type,locations) VALUES ($1,'Book reader test','books',$2)")
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

    let pdf_id = insert_book(
        &pool,
        library_id,
        &media_root.join("Sample.PDF"),
        "Book",
        40,
    )
    .await;
    let epub_id = insert_book(
        &pool,
        library_id,
        &media_root.join("Sample.epub"),
        "EBook",
        40,
    )
    .await;
    let rated_epub_id = insert_book(
        &pool,
        library_id,
        &media_root.join("Rated.epub"),
        "Book",
        75,
    )
    .await;
    set_local_policy_rating(&pool, rated_epub_id, "R", 75).await;
    assert_eq!(
        db::get_item(&pool, rated_epub_id)
            .await
            .unwrap()
            .unwrap()
            .rating,
        Some(75),
        "the restricted-book fixture must carry a recognized policy rating"
    );
    let admin_id = insert_user(&pool, "books-admin", true, true, true).await;
    let download_disabled_id = insert_user(&pool, "books-no-download", false, true, false).await;
    let library_limited_id = insert_user(&pool, "books-library-limited", false, true, true).await;
    let parental_limited_id = insert_user(&pool, "books-parental-limited", false, true, true).await;
    sqlx::query("UPDATE users SET restrict_libraries=TRUE WHERE id=$1")
        .bind(library_limited_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET restrict_libraries=TRUE,max_parental_rating=50 WHERE id=$1")
        .bind(parental_limited_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO user_library_access(user_id,library_id) VALUES ($1,$2)")
        .bind(parental_limited_id)
        .bind(library_id)
        .execute(&pool)
        .await
        .unwrap();

    let data_dir = env::temp_dir().join(format!("puffinbox-books-data-{}", Uuid::new_v4()));
    fs::create_dir_all(&data_dir).unwrap();
    let config = Config {
        bind: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        public_base_url: None,
        database_url: database_url.clone(),
        server_name: "Book reader test".to_owned(),
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
    let router = api::router(state.clone());
    let admin_token = issue_test_token(&state, admin_id, "books-admin").await;
    let download_disabled_token =
        issue_test_token(&state, download_disabled_id, "books-no-download").await;
    let library_limited_token =
        issue_test_token(&state, library_limited_id, "books-library-limited").await;
    let parental_limited_token =
        issue_test_token(&state, parental_limited_id, "books-parental-limited").await;

    let reader = call(
        &router,
        &format!("/Books/{pdf_id}/Reader"),
        &admin_token,
        None,
    )
    .await;
    assert_eq!(reader.status(), StatusCode::OK);
    assert!(
        reader.headers()["content-security-policy"]
            .to_str()
            .unwrap()
            .contains("worker-src 'self'")
    );
    assert!(
        reader.headers()["cache-control"]
            .to_str()
            .unwrap()
            .contains("no-store")
    );
    let html = reader.into_body().collect().await.unwrap().to_bytes();
    let html = String::from_utf8(html.to_vec()).unwrap();
    assert!(html.contains("data-format=\"pdf\""));
    assert!(html.contains("data-book-title=\"Sample.PDF\""));

    let mut pdf_request = Request::builder()
        .uri(format!("/Books/{pdf_id}/Document"))
        .header("X-Emby-Token", &admin_token)
        .header("Range", "bytes=0-4")
        .body(Body::empty())
        .unwrap();
    *pdf_request.method_mut() = axum::http::Method::GET;
    let pdf = router.clone().oneshot(pdf_request).await.unwrap();
    assert_eq!(pdf.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(pdf.headers()["content-type"], "application/pdf");
    assert!(!pdf.headers().contains_key("content-disposition"));
    assert_eq!(
        pdf.headers()["content-range"],
        format!("bytes 0-4/{}", b"%PDF-1.7\nfixture".len())
    );
    let pdf_bytes = pdf.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(pdf_bytes.as_ref(), &b"%PDF-"[..]);

    let epub = call(
        &router,
        &format!("/Books/{epub_id}/Epub"),
        &admin_token,
        None,
    )
    .await;
    assert_eq!(epub.status(), StatusCode::OK);
    assert_eq!(epub.headers()["content-type"], "application/epub+zip");
    assert!(!epub.headers().contains_key("content-disposition"));
    let epub_bytes = epub.into_body().collect().await.unwrap().to_bytes();
    assert_eq!(epub_bytes.as_ref(), &b"PK\x03\x04fixture"[..]);

    let reader_without_download = call(
        &router,
        &format!("/Books/{epub_id}/Reader"),
        &download_disabled_token,
        None,
    )
    .await;
    assert_eq!(reader_without_download.status(), StatusCode::OK);
    let html = reader_without_download
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    assert!(
        String::from_utf8_lossy(&html)
            .contains("id=\"download-link\" class=\"download-link\" href=\"/Books/")
            && String::from_utf8_lossy(&html).contains("/Download\" hidden")
    );
    let denied_download = call(
        &router,
        &format!("/Books/{epub_id}/Download"),
        &download_disabled_token,
        None,
    )
    .await;
    assert_eq!(denied_download.status(), StatusCode::FORBIDDEN);
    assert!(
        !denied_download
            .headers()
            .contains_key("content-disposition")
    );
    let allowed_download = call(
        &router,
        &format!("/Books/{epub_id}/Download"),
        &admin_token,
        None,
    )
    .await;
    assert_eq!(allowed_download.status(), StatusCode::OK);
    assert!(
        allowed_download.headers()["content-disposition"]
            .to_str()
            .unwrap()
            .starts_with("attachment;")
    );
    let denied_jellyfin_download = call(
        &router,
        &format!("/Items/{epub_id}/Download"),
        &download_disabled_token,
        None,
    )
    .await;
    assert_eq!(denied_jellyfin_download.status(), StatusCode::FORBIDDEN);
    let denied_jellyfin_download_head = call_method(
        &router,
        axum::http::Method::HEAD,
        &format!("/Items/{epub_id}/Download"),
        &download_disabled_token,
        None,
    )
    .await;
    assert_eq!(
        denied_jellyfin_download_head.status(),
        StatusCode::FORBIDDEN
    );
    let allowed_jellyfin_download = call(
        &router,
        &format!("/Items/{epub_id}/Download"),
        &admin_token,
        None,
    )
    .await;
    assert_eq!(allowed_jellyfin_download.status(), StatusCode::OK);
    assert_eq!(
        allowed_jellyfin_download.headers()["content-type"],
        "application/epub+zip"
    );
    assert!(
        allowed_jellyfin_download.headers()["content-disposition"]
            .to_str()
            .unwrap()
            .contains("Sample.epub")
    );
    assert_eq!(
        allowed_jellyfin_download
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .as_ref(),
        &b"PK\x03\x04fixture"[..]
    );
    let allowed_pdf_head = call_method(
        &router,
        axum::http::Method::HEAD,
        &format!("/Items/{pdf_id}/Download"),
        &admin_token,
        None,
    )
    .await;
    assert_eq!(allowed_pdf_head.status(), StatusCode::OK);
    assert_eq!(
        allowed_pdf_head.headers()["content-type"],
        "application/pdf"
    );
    assert_eq!(
        allowed_pdf_head.headers()["content-length"],
        b"%PDF-1.7\nfixture".len().to_string()
    );
    assert!(
        allowed_pdf_head
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .is_empty()
    );
    let library_denied = call(
        &router,
        &format!("/Books/{epub_id}/Reader"),
        &library_limited_token,
        None,
    )
    .await;
    assert_eq!(library_denied.status(), StatusCode::NOT_FOUND);
    let parental_allowed = call(
        &router,
        &format!("/Books/{epub_id}/Reader"),
        &parental_limited_token,
        None,
    )
    .await;
    assert_eq!(parental_allowed.status(), StatusCode::OK);
    let parental_denied = call(
        &router,
        &format!("/Books/{rated_epub_id}/Reader"),
        &parental_limited_token,
        None,
    )
    .await;
    assert_eq!(parental_denied.status(), StatusCode::NOT_FOUND);

    state
        .shutdown_requested
        .store(true, std::sync::atomic::Ordering::Release);
    drop(router);
    drop(state);
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .execute(&admin_pool)
        .await
        .unwrap();
    admin_pool.close().await;
    let _ = fs::remove_dir_all(data_dir);
    let _ = fs::remove_dir_all(media_root);
}

async fn insert_book(
    pool: &sqlx::PgPool,
    library_id: Uuid,
    path: &std::path::Path,
    item_type: &str,
    rating: i16,
) -> Uuid {
    let id = Uuid::new_v4();
    let metadata = fs::metadata(path).unwrap();
    let modified = chrono::DateTime::<chrono::Utc>::from(metadata.modified().unwrap());
    let path = path.to_str().unwrap().to_owned();
    let name = PathBuf::from(&path)
        .file_name()
        .unwrap()
        .to_string_lossy()
        .into_owned();
    sqlx::query("INSERT INTO items(id,library_id,name,sort_name,item_type,path,path_hash,size_bytes,date_modified,rating) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10)")
        .bind(id)
        .bind(library_id)
        .bind(&name)
        .bind(name.to_ascii_lowercase())
        .bind(item_type)
        .bind(&path)
        .bind(db::path_hash(&path))
        .bind(metadata.len() as i64)
        .bind(modified)
        .bind(rating)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn set_local_policy_rating(pool: &sqlx::PgPool, item_id: Uuid, label: &str, value: i16) {
    sqlx::query("INSERT INTO item_metadata(item_id,provider_key,content_rating,policy_rating_scale,policy_rating_value) VALUES ($1,'local-nfo',$2,'US-MPAA-v1',$3)")
        .bind(item_id)
        .bind(label)
        .bind(value)
        .execute(pool)
        .await
        .unwrap();
}

async fn insert_user(
    pool: &sqlx::PgPool,
    username: &str,
    is_admin: bool,
    enable_remote_access: bool,
    enable_content_downloading: bool,
) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,username,username_norm,password_hash,is_admin,enable_remote_access,enable_content_downloading) VALUES ($1,$2,$2,'unused',$3,$4,$5)")
        .bind(id)
        .bind(username)
        .bind(is_admin)
        .bind(enable_remote_access)
        .bind(enable_content_downloading)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn issue_test_token(state: &AppState, user_id: Uuid, device_id: &str) -> String {
    let user = db::get_user(&state.db, user_id).await.unwrap().unwrap();
    auth::issue_token(
        state,
        &user,
        "book-reader-test",
        "book-reader-test",
        device_id,
    )
    .await
    .unwrap()
    .token
}

async fn call(
    router: &axum::Router,
    uri: &str,
    token: &str,
    range: Option<&str>,
) -> axum::response::Response {
    call_method(router, axum::http::Method::GET, uri, token, range).await
}

async fn call_method(
    router: &axum::Router,
    method: axum::http::Method,
    uri: &str,
    token: &str,
    range: Option<&str>,
) -> axum::response::Response {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("X-Emby-Token", token);
    if let Some(range) = range {
        request = request.header("Range", range);
    }
    router
        .clone()
        .oneshot(request.body(Body::empty()).unwrap())
        .await
        .unwrap()
}
