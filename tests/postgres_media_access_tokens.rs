use std::{env, fs, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use axum::{body::Body, http::Request};
use base64::Engine;
use chrono::{DateTime, Utc};
use http_body_util::BodyExt;
use ipnet::IpNet;
use puffinbox::{AppState, Config, api, auth, db, library};
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;
use uuid::Uuid;

mod common;

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via PUFFINBOX_TEST_DATABASE_URL"]
async fn media_access_token_restores_cookie_session_media_and_stays_read_only_and_session_bound() {
    let database_url = env::var("PUFFINBOX_TEST_DATABASE_URL")
        .expect("set PUFFINBOX_TEST_DATABASE_URL to a disposable PostgreSQL database");
    let admin_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url)
        .await
        .unwrap();
    let schema = format!("puffinbox_media_access_test_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
        .execute(&admin_pool)
        .await
        .unwrap();

    let connection_schema = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(6)
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

    let media_root = env::temp_dir().join(format!("puffinbox-media-access-{}", Uuid::new_v4()));
    fs::create_dir_all(&media_root).unwrap();
    let media_path = media_root.join("fixture.png");
    let media_bytes = base64::engine::general_purpose::STANDARD
        .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR4nGP4z8DwHwAFAAH/iZk9HQAAAABJRU5ErkJggg==")
        .unwrap();
    fs::write(&media_path, &media_bytes).unwrap();
    let media_metadata = fs::metadata(&media_path).unwrap();
    let modified = DateTime::<Utc>::from(media_metadata.modified().unwrap());
    let media_root = fs::canonicalize(media_root).unwrap();
    let root_text = media_root.to_str().unwrap().to_owned();
    let (device_id, inode) = library::inspect_library_root_identity(media_root.clone())
        .await
        .unwrap();
    let library_id = Uuid::new_v4();
    db::insert_library(
        &pool,
        run_id,
        library_id,
        "Media access test",
        "photos",
        std::slice::from_ref(&media_root),
        true,
    )
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
    let item_id = Uuid::new_v4();
    sqlx::query("INSERT INTO items(id,library_id,name,sort_name,item_type,path,path_hash,size_bytes,date_modified) VALUES ($1,$2,'fixture.png','fixture.png','Photo',$3,$4,$5,$6)")
        .bind(item_id)
        .bind(library_id)
        .bind(media_path.to_str().unwrap())
        .bind(db::path_hash(media_path.to_str().unwrap()))
        .bind(media_metadata.len() as i64)
        .bind(modified)
        .execute(&pool)
        .await
        .unwrap();

    let user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,username,username_norm,password_hash,is_admin,enable_remote_access,allow_media_playback) VALUES ($1,'media-admin','media-admin','unused',TRUE,TRUE,TRUE)")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
    let data_dir = env::temp_dir().join(format!("puffinbox-media-access-data-{}", Uuid::new_v4()));
    fs::create_dir_all(&data_dir).unwrap();
    let config = Config {
        bind: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        public_base_url: None,
        database_url: database_url.clone(),
        server_name: "Media access test".to_owned(),
        web_root: PathBuf::from("web"),
        data_dir: data_dir.clone(),
        ffmpeg_path: None,
        max_scan_workers: 1,
        max_page_size: 100,
        access_token_lifetime_hours: 24,
        cookie_secure: false,
        cors_origins: vec!["https://desktop.example".to_owned()],
        trusted_proxies: Vec::new(),
        local_networks: vec!["127.0.0.0/8".parse::<IpNet>().unwrap()],
        setup_token: None,
        bootstrap_admin_username: None,
        bootstrap_admin_password: None,
    };
    let state = AppState::new_for_run(
        pool.clone(),
        Arc::new(config.clone()),
        Uuid::new_v4(),
        run_id,
        None,
    );
    let router = api::router(state.clone());
    let user = db::get_user(&pool, user_id).await.unwrap().unwrap();
    let parent_session = auth::issue_token(&state, &user, "test", "browser", "browser-device")
        .await
        .unwrap();
    let peer_user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,username,username_norm,password_hash,is_admin,enable_remote_access) VALUES ($1,'media-peer','media-peer','unused',TRUE,TRUE)")
        .bind(peer_user_id)
        .execute(&pool)
        .await
        .unwrap();
    let peer_user = db::get_user(&pool, peer_user_id).await.unwrap().unwrap();
    let peer_session = auth::issue_token(&state, &peer_user, "test", "peer", "peer-device")
        .await
        .unwrap();
    let set_cookie = auth::cookie_header(&parent_session.token, &config).unwrap();
    assert!(set_cookie.to_str().unwrap().contains("HttpOnly"));
    let cookie = set_cookie
        .to_str()
        .unwrap()
        .split(';')
        .next()
        .unwrap()
        .to_owned();

    let cookie_photo = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/Items/{item_id}/File"))
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cookie_photo.status(), axum::http::StatusCode::OK);
    assert_eq!(cookie_photo.headers()["content-type"], "image/png");
    assert_eq!(cookie_photo.headers()["x-content-type-options"], "nosniff");
    assert_eq!(
        cookie_photo.into_body().collect().await.unwrap().to_bytes(),
        media_bytes.as_slice(),
        "the existing HttpOnly session cookie must authorize media-file delivery"
    );

    let cookie_primary = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/Items/{item_id}/Images/Primary"))
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cookie_primary.status(), axum::http::StatusCode::OK);
    assert_eq!(cookie_primary.headers()["content-type"], "image/png");
    assert_eq!(
        cookie_primary.headers()["cache-control"],
        "private, no-store"
    );
    assert_eq!(
        cookie_primary.headers()["x-content-type-options"],
        "nosniff"
    );
    assert_eq!(
        cookie_primary
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes(),
        media_bytes.as_slice()
    );

    let origin = "http://media.example";
    let minted = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Users/Me/MediaAccessToken")
                .header("host", "media.example")
                .header("origin", origin)
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(minted.status(), axum::http::StatusCode::OK);
    assert_eq!(minted.headers()["cache-control"], "no-store");
    assert_eq!(minted.headers()["pragma"], "no-cache");
    let minted_body = minted.into_body().collect().await.unwrap().to_bytes();
    let minted_body: serde_json::Value = serde_json::from_slice(&minted_body).unwrap();
    let media_token = minted_body["AccessToken"].as_str().unwrap().to_owned();
    for credential in [&parent_session.token, &media_token] {
        assert_eq!(
            db::media_auth_device_id(&pool, user_id, &auth::token_digest(credential))
                .await
                .unwrap()
                .as_deref(),
            Some("browser-device")
        );
        assert!(
            db::media_auth_device_id(&pool, Uuid::new_v4(), &auth::token_digest(credential))
                .await
                .unwrap()
                .is_none()
        );
    }
    for credential in [&parent_session.token, &media_token] {
        for method in ["GET", "HEAD"] {
            let image = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(format!(
                            "/Items/{item_id}/Images/Primary?ApiKey={credential}"
                        ))
                        .header("range", "bytes=1-5")
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(image.status(), axum::http::StatusCode::PARTIAL_CONTENT);
            assert_eq!(image.headers()["content-type"], "image/png");
            assert_eq!(image.headers()["content-length"], "5");
            assert_eq!(image.headers()["referrer-policy"], "no-referrer");
            let bytes = image.into_body().collect().await.unwrap().to_bytes();
            if method == "GET" {
                assert_eq!(bytes, &media_bytes[1..6]);
            } else {
                assert!(bytes.is_empty());
            }
        }
    }
    let anonymous_image = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/Items/{item_id}/Images/Primary"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        anonymous_image.status(),
        axum::http::StatusCode::UNAUTHORIZED
    );
    assert_eq!(
        anonymous_image.headers()["cache-control"],
        "private, no-store"
    );
    let conflicting_image = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/Items/{item_id}/Images/Primary?ApiKey={media_token}"
                ))
                .header("x-emby-token", &peer_session.token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        conflicting_image.status(),
        axum::http::StatusCode::UNAUTHORIZED
    );
    let unsafe_id = Uuid::new_v4();
    let unsafe_path = media_root.join("legacy.svg");
    fs::write(
        &unsafe_path,
        b"<svg xmlns=\"http://www.w3.org/2000/svg\"></svg>",
    )
    .unwrap();
    sqlx::query("INSERT INTO items(id,library_id,name,sort_name,item_type,path,path_hash) VALUES ($1,$2,'legacy.svg','legacy.svg','Photo',$3,$4)")
        .bind(unsafe_id).bind(library_id).bind(unsafe_path.to_str().unwrap())
        .bind(db::path_hash(unsafe_path.to_str().unwrap())).execute(&pool).await.unwrap();
    let unsafe_image = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/Items/{unsafe_id}/Images/Primary?ApiKey={media_token}"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(unsafe_image.status(), axum::http::StatusCode::NOT_FOUND);
    assert_eq!(unsafe_image.headers()["cache-control"], "private, no-store");
    sqlx::query("UPDATE users SET allow_media_playback=FALSE WHERE id=$1")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
    let blocked_image = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/Items/{item_id}/Images/Primary?ApiKey={media_token}"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(blocked_image.status(), axum::http::StatusCode::FORBIDDEN);
    sqlx::query("UPDATE users SET allow_media_playback=TRUE WHERE id=$1")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
    let restricted_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,username,username_norm,password_hash,enable_remote_access,restrict_libraries) VALUES ($1,'photo-viewer','photo-viewer','unused',TRUE,TRUE)")
        .bind(restricted_id).execute(&pool).await.unwrap();
    let restricted_user = db::get_user(&pool, restricted_id).await.unwrap().unwrap();
    let restricted_session = auth::issue_token(
        &state,
        &restricted_user,
        "test",
        "photo-viewer",
        "photo-device",
    )
    .await
    .unwrap();
    for (allowed, expected) in [
        (false, axum::http::StatusCode::NOT_FOUND),
        (true, axum::http::StatusCode::OK),
        (false, axum::http::StatusCode::NOT_FOUND),
    ] {
        if allowed {
            sqlx::query("INSERT INTO user_library_access(user_id,library_id) VALUES ($1,$2)")
                .bind(restricted_id)
                .bind(library_id)
                .execute(&pool)
                .await
                .unwrap();
        } else {
            sqlx::query("DELETE FROM user_library_access WHERE user_id=$1 AND library_id=$2")
                .bind(restricted_id)
                .bind(library_id)
                .execute(&pool)
                .await
                .unwrap();
        }
        let image = router
            .clone()
            .oneshot(
                Request::builder()
                    .uri(format!("/Items/{item_id}/Images/Primary"))
                    .header("x-emby-token", &restricted_session.token)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(image.status(), expected);
    }
    for method in ["GET", "HEAD"] {
        let audio_uri = format!(
            "/Audio/{item_id}/universal?Container=flac&MaxAudioSampleRate=44100&MaxAudioBitDepth=16&ApiKey={media_token}"
        );
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(&audio_uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            axum::http::StatusCode::NOT_FOUND,
            "a scoped credential reaches the authorized audio route but cannot play a photo"
        );
        let wrong_parent = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(&audio_uri)
                    .header("x-emby-token", &peer_session.token)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(wrong_parent.status(), axum::http::StatusCode::UNAUTHORIZED);
        let wrong_user = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(format!("{audio_uri}&UserId={peer_user_id}"))
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(wrong_user.status(), axum::http::StatusCode::FORBIDDEN);
    }
    // Opaque reader bytes exercise authorization and exact delivery here;
    // decoding a real FLAC is covered by the separate client fixture.
    let track_id = Uuid::new_v4();
    let track_path = media_root.join("reader.flac");
    let track_bytes = b"fLaC original reader fixture";
    fs::write(&track_path, track_bytes).unwrap();
    let track_metadata = fs::metadata(&track_path).unwrap();
    sqlx::query("INSERT INTO items(id,library_id,name,sort_name,item_type,path,path_hash,size_bytes,date_modified) VALUES ($1,$2,'reader.flac','reader.flac','Audio',$3,$4,$5,$6)")
        .bind(track_id).bind(library_id).bind(track_path.to_str().unwrap())
        .bind(db::path_hash(track_path.to_str().unwrap())).bind(track_metadata.len() as i64)
        .bind(DateTime::<Utc>::from(track_metadata.modified().unwrap())).execute(&pool).await.unwrap();
    for method in ["GET", "HEAD"] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(format!(
                        "/Audio/{track_id}/stream.flac?Static=true&ApiKey={media_token}"
                    ))
                    .header("range", "bytes=1-5")
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(response.status(), axum::http::StatusCode::PARTIAL_CONTENT);
        assert_eq!(response.headers()["content-type"], "audio/flac");
        assert_eq!(response.headers()["cache-control"], "private, no-store");
        assert_eq!(
            response.headers()["content-range"],
            format!("bytes 1-5/{}", track_bytes.len())
        );
        let body = response.into_body().collect().await.unwrap().to_bytes();
        if method == "HEAD" {
            assert!(body.is_empty());
        } else {
            assert_eq!(body.as_ref(), &track_bytes[1..6]);
        }
        for resource in [
            format!("/Audio/{track_id}/stream.mp3"),
            format!("/Videos/{track_id}/stream.flac"),
            format!("/Audio/{item_id}/stream.png"),
        ] {
            let response = router
                .clone()
                .oneshot(
                    Request::builder()
                        .method(method)
                        .uri(format!("{resource}?ApiKey={media_token}"))
                        .body(Body::empty())
                        .unwrap(),
                )
                .await
                .unwrap();
            assert_eq!(
                response.status(),
                axum::http::StatusCode::NOT_FOUND,
                "a suffix cannot convert original bytes or select another media type"
            );
        }
    }
    sqlx::query("UPDATE libraries SET enabled=FALSE WHERE id=$1")
        .bind(library_id)
        .execute(&pool)
        .await
        .unwrap();
    let disabled_image = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/Items/{item_id}/Images/Primary?ApiKey={media_token}"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(disabled_image.status(), axum::http::StatusCode::NOT_FOUND);
    let denied = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/Audio/{track_id}/stream.flac?ApiKey={media_token}"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(denied.status(), axum::http::StatusCode::NOT_FOUND);
    sqlx::query("UPDATE libraries SET enabled=TRUE WHERE id=$1")
        .bind(library_id)
        .execute(&pool)
        .await
        .unwrap();
    let expires_at: chrono::DateTime<Utc> =
        serde_json::from_value(minted_body["ExpiresAt"].clone()).unwrap();
    assert!(expires_at > Utc::now());
    assert!(expires_at <= Utc::now() + chrono::Duration::hours(4));

    let native_media = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/Items/{item_id}/File?ApiKey={media_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(native_media.status(), axum::http::StatusCode::OK);
    assert_eq!(
        native_media.into_body().collect().await.unwrap().to_bytes(),
        media_bytes.as_slice()
    );

    let cookie_bound_media = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/Items/{item_id}/File?ApiKey={media_token}"))
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(cookie_bound_media.status(), axum::http::StatusCode::OK);

    let same_parent_header_media = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/Items/{item_id}/File?ApiKey={media_token}"))
                .header("x-emby-token", &parent_session.token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        same_parent_header_media.status(),
        axum::http::StatusCode::OK
    );

    let other_parent = auth::issue_token(&state, &user, "test", "other-browser", "other-device")
        .await
        .unwrap();
    let other_cookie = auth::cookie_header(&other_parent.token, &config).unwrap();
    let other_cookie = other_cookie.to_str().unwrap().split(';').next().unwrap();
    let wrong_parent_media = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/Items/{item_id}/File?ApiKey={media_token}"))
                .header("cookie", other_cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        wrong_parent_media.status(),
        axum::http::StatusCode::UNAUTHORIZED,
        "the scoped token must be bound to its exact parent auth session"
    );

    let wrong_parent_header_media = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/Items/{item_id}/File?ApiKey={media_token}"))
                .header("x-emby-token", &peer_session.token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        wrong_parent_header_media.status(),
        axum::http::StatusCode::UNAUTHORIZED,
        "a different token header must not be combined with the scoped media token"
    );

    for (method, uri) in [
        (
            "GET",
            format!("/Items/{item_id}/Download?ApiKey={media_token}"),
        ),
        (
            "GET",
            format!("/Items/{item_id}/PlaybackInfo?ApiKey={media_token}"),
        ),
        ("GET", format!("/Users/Me?ApiKey={media_token}")),
        ("POST", format!("/Sessions/Playing?ApiKey={media_token}")),
        (
            "DELETE",
            format!(
                "/Videos/{item_id}/hls/{}?ApiKey={media_token}",
                Uuid::new_v4()
            ),
        ),
    ] {
        let response = router
            .clone()
            .oneshot(
                Request::builder()
                    .method(method)
                    .uri(&uri)
                    .body(Body::empty())
                    .unwrap(),
            )
            .await
            .unwrap();
        assert_eq!(
            response.status(),
            axum::http::StatusCode::UNAUTHORIZED,
            "scoped media tokens must not authorize {method} {uri}"
        );
    }

    let child_header = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/Items/{item_id}/File"))
                .header("x-emby-token", &media_token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        child_header.status(),
        axum::http::StatusCode::UNAUTHORIZED,
        "scoped media tokens cannot authenticate through a normal token header"
    );

    let same_identity_header = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Users/Me/MediaAccessToken")
                .header("host", "media.example")
                .header("origin", origin)
                .header("cookie", &cookie)
                .header("x-emby-token", &parent_session.token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        same_identity_header.status(),
        axum::http::StatusCode::UNAUTHORIZED
    );

    let mismatched_identity_header = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Users/Me/MediaAccessToken")
                .header("host", "media.example")
                .header("origin", origin)
                .header("cookie", &cookie)
                .header("authorization", format!("Bearer {}", peer_session.token))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        mismatched_identity_header.status(),
        axum::http::StatusCode::UNAUTHORIZED
    );

    let query_credential = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri(format!(
                    "/Users/Me/MediaAccessToken?ApiKey={}",
                    parent_session.token
                ))
                .header("host", "media.example")
                .header("origin", origin)
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        query_credential.status(),
        axum::http::StatusCode::UNAUTHORIZED
    );

    let cors_origin_exchange = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Users/Me/MediaAccessToken")
                .header("host", "media.example")
                .header("origin", "https://desktop.example")
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        cors_origin_exchange.status(),
        axum::http::StatusCode::FORBIDDEN
    );
    assert!(
        !cors_origin_exchange
            .headers()
            .contains_key("access-control-allow-origin")
    );
    let cors_body = cors_origin_exchange
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    assert!(!String::from_utf8_lossy(&cors_body).contains("AccessToken"));

    let logout = router
        .clone()
        .oneshot(
            Request::builder()
                .method("POST")
                .uri("/Users/Me/Logout")
                .header("host", "media.example")
                .header("origin", origin)
                .header("cookie", &cookie)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(logout.status(), axum::http::StatusCode::NO_CONTENT);
    for credential in [&parent_session.token, &media_token] {
        assert!(
            db::media_auth_device_id(&pool, user_id, &auth::token_digest(credential))
                .await
                .unwrap()
                .is_none()
        );
    }
    let after_logout = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!("/Items/{item_id}/File?ApiKey={media_token}"))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(after_logout.status(), axum::http::StatusCode::UNAUTHORIZED);
    let after_logout_image = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/Items/{item_id}/Images/Primary?ApiKey={media_token}"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        after_logout_image.status(),
        axum::http::StatusCode::UNAUTHORIZED
    );
    let after_logout_audio = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(format!(
                    "/Audio/{track_id}/stream.flac?ApiKey={media_token}"
                ))
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    assert_eq!(
        after_logout_audio.status(),
        axum::http::StatusCode::UNAUTHORIZED
    );

    pool.close().await;
    sqlx::query(format!("DROP SCHEMA \"{schema}\" CASCADE").as_str())
        .execute(&admin_pool)
        .await
        .unwrap();
    admin_pool.close().await;
    let _ = fs::remove_dir_all(data_dir);
    let _ = fs::remove_dir_all(media_root);
}
