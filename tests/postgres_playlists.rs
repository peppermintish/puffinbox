use std::{env, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use axum::{body::Body, http::Request};
use http_body_util::BodyExt;
use ipnet::IpNet;
use puffinbox::{AppState, Config, api, auth, db};
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;
use uuid::Uuid;

mod common;

struct RemoveDirectoryOnDrop(PathBuf);

impl Drop for RemoveDirectoryOnDrop {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via PUFFINBOX_TEST_DATABASE_URL"]
async fn playlists_preserve_order_and_enforce_owner_library_and_playback_policy() {
    let database_url = env::var("PUFFINBOX_TEST_DATABASE_URL")
        .expect("set PUFFINBOX_TEST_DATABASE_URL to a disposable PostgreSQL database");
    let admin_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url)
        .await
        .unwrap();
    let schema = format!("puffinbox_playlists_test_{}", Uuid::new_v4().simple());
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

    let library_id = Uuid::new_v4();
    sqlx::query("INSERT INTO libraries(id,name,collection_type,locations,enabled) VALUES ($1,'Playlist music','music',$2,TRUE)")
        .bind(library_id)
        .bind(sqlx::types::Json(vec!["/music".to_owned()]))
        .execute(&pool)
        .await
        .unwrap();
    let first = insert_audio(&pool, library_id, "First song", "/music/first.flac").await;
    let second = insert_audio(&pool, library_id, "Second song", "/music/second.flac").await;
    let third = insert_audio(&pool, library_id, "Third song", "/music/third.flac").await;
    let hidden = insert_audio(
        &pool,
        library_id,
        "Hidden song",
        "/music/.private/hidden.flac",
    )
    .await;

    let owner_id = insert_user(&pool, "playlist-owner", true).await;
    let peer_id = insert_user(&pool, "playlist-peer", true).await;
    let playback_disabled_id = insert_user(&pool, "playlist-no-play", false).await;
    let data_dir = env::temp_dir().join(format!("puffinbox-playlists-{}", Uuid::new_v4()));
    std::fs::create_dir_all(&data_dir).unwrap();
    let _data_dir_guard = RemoveDirectoryOnDrop(data_dir.clone());
    let config = Config {
        bind: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        public_base_url: None,
        database_url: database_url.clone(),
        server_name: "Playlist integration test".to_owned(),
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
        setup_token: None,
        bootstrap_admin_username: None,
        bootstrap_admin_password: None,
    };
    let state = AppState::new_for_run(pool.clone(), Arc::new(config), Uuid::new_v4(), run_id, None);
    let router = api::router(state.clone());
    let owner = db::get_user(&pool, owner_id).await.unwrap().unwrap();
    let peer = db::get_user(&pool, peer_id).await.unwrap().unwrap();
    let playback_disabled = db::get_user(&pool, playback_disabled_id)
        .await
        .unwrap()
        .unwrap();
    let owner_token = auth::issue_token(&state, &owner, "test", "owner", "owner-device")
        .await
        .unwrap()
        .token;
    let peer_token = auth::issue_token(&state, &peer, "test", "peer", "peer-device")
        .await
        .unwrap()
        .token;
    let playback_disabled_token = auth::issue_token(
        &state,
        &playback_disabled,
        "test",
        "restricted",
        "restricted-device",
    )
    .await
    .unwrap()
    .token;

    let no_auth = send(&router, "GET", "/Playlists", None, None).await;
    assert_eq!(no_auth.status(), axum::http::StatusCode::UNAUTHORIZED);
    let no_auth_items = send(
        &router,
        "GET",
        &format!("/Playlists/{}/Items", Uuid::new_v4()),
        None,
        None,
    )
    .await;
    assert_eq!(no_auth_items.status(), axum::http::StatusCode::UNAUTHORIZED);

    let public_create = send(
        &router,
        "POST",
        "/Playlists",
        Some(&owner_token),
        Some(json!({ "Name": "Public", "IsPublic": true })),
    )
    .await;
    assert_eq!(public_create.status(), axum::http::StatusCode::BAD_REQUEST);
    let video_create = send(
        &router,
        "POST",
        "/Playlists",
        Some(&owner_token),
        Some(json!({ "Name": "Video", "MediaType": "Video" })),
    )
    .await;
    assert_eq!(video_create.status(), axum::http::StatusCode::BAD_REQUEST);

    let created = send(
        &router,
        "POST",
        "/Playlists",
        Some(&owner_token),
        Some(json!({
            "Name": "Road trip",
            "Ids": [first, second],
            "MediaType": "Audio",
            "IsPublic": false
        })),
    )
    .await;
    assert_eq!(created.status(), axum::http::StatusCode::OK);
    let created: Value = response_json(created).await;
    let playlist_id = Uuid::parse_str(created["Id"].as_str().unwrap()).unwrap();

    let list = send(&router, "GET", "/Playlists", Some(&owner_token), None).await;
    assert_eq!(list.status(), axum::http::StatusCode::OK);
    let list: Value = response_json(list).await;
    assert_eq!(list["TotalRecordCount"], 1);
    assert_eq!(list["Items"][0]["Name"], "Road trip");
    assert_eq!(list["Items"][0]["Type"], "Playlist");
    assert_eq!(list["Items"][0]["MediaType"], "Audio");

    let metadata = send(
        &router,
        "GET",
        &format!("/Playlists/{playlist_id}"),
        Some(&owner_token),
        None,
    )
    .await;
    assert_eq!(metadata.status(), axum::http::StatusCode::OK);
    let metadata: Value = response_json(metadata).await;
    assert_eq!(metadata["OpenAccess"], false);
    assert_eq!(metadata["Shares"], json!([]));
    assert_eq!(metadata["ItemIds"], json!([first, second]));

    let page = send(
        &router,
        "GET",
        &format!("/Playlists/{playlist_id}/Items?StartIndex=1&Limit=1"),
        Some(&owner_token),
        None,
    )
    .await;
    assert_eq!(page.status(), axum::http::StatusCode::OK);
    let page: Value = response_json(page).await;
    assert_eq!(page["TotalRecordCount"], 2);
    assert_eq!(page["StartIndex"], 1);
    assert_eq!(page["Items"][0]["Id"], second.to_string());
    let second_entry_id = page["Items"][0]["PlaylistItemId"].as_str().unwrap();

    let add = send(
        &router,
        "POST",
        &format!("/Playlists/{playlist_id}/Items?Ids={third}&Position=1&UserId={owner_id}"),
        Some(&owner_token),
        None,
    )
    .await;
    assert_eq!(add.status(), axum::http::StatusCode::NO_CONTENT);
    let after_add = playlist_items(&router, playlist_id, &owner_token).await;
    assert_eq!(item_ids(&after_add), vec![first, third, second]);
    let third_entry_id = after_add["Items"][1]["PlaylistItemId"]
        .as_str()
        .unwrap()
        .to_owned();

    let remove = send(
        &router,
        "DELETE",
        &format!("/Playlists/{playlist_id}/Items?EntryIds={third_entry_id}"),
        Some(&owner_token),
        None,
    )
    .await;
    assert_eq!(remove.status(), axum::http::StatusCode::NO_CONTENT);
    assert_eq!(
        item_ids(&playlist_items(&router, playlist_id, &owner_token).await),
        vec![first, second]
    );

    let move_item = send(
        &router,
        "POST",
        &format!("/Playlists/{playlist_id}/Items/{second_entry_id}/Move/0"),
        Some(&owner_token),
        None,
    )
    .await;
    assert_eq!(move_item.status(), axum::http::StatusCode::NO_CONTENT);
    assert_eq!(
        item_ids(&playlist_items(&router, playlist_id, &owner_token).await),
        vec![second, first]
    );

    let update = send(
        &router,
        "POST",
        &format!("/Playlists/{playlist_id}"),
        Some(&owner_token),
        Some(json!({ "Name": "Updated mix", "Ids": [first, second] })),
    )
    .await;
    assert_eq!(update.status(), axum::http::StatusCode::NO_CONTENT);
    assert_eq!(
        item_ids(&playlist_items(&router, playlist_id, &owner_token).await),
        vec![first, second]
    );
    let list =
        response_json(send(&router, "GET", "/Playlists", Some(&owner_token), None).await).await;
    assert_eq!(list["Items"][0]["Name"], "Updated mix");

    let hidden_add = send(
        &router,
        "POST",
        &format!("/Playlists/{playlist_id}/Items?Ids={hidden}"),
        Some(&owner_token),
        None,
    )
    .await;
    assert_eq!(hidden_add.status(), axum::http::StatusCode::NOT_FOUND);

    sqlx::query("UPDATE users SET restrict_libraries=TRUE WHERE id=$1")
        .bind(owner_id)
        .execute(&pool)
        .await
        .unwrap();
    let filtered = playlist_items(&router, playlist_id, &owner_token).await;
    assert_eq!(filtered["Items"], json!([]));
    assert_eq!(filtered["TotalRecordCount"], 0);
    let filtered_metadata = response_json(
        send(
            &router,
            "GET",
            &format!("/Playlists/{playlist_id}"),
            Some(&owner_token),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(filtered_metadata["ItemIds"], json!([]));

    let peer_read = send(
        &router,
        "GET",
        &format!("/Playlists/{playlist_id}"),
        Some(&peer_token),
        None,
    )
    .await;
    assert_eq!(peer_read.status(), axum::http::StatusCode::NOT_FOUND);
    let peer_update = send(
        &router,
        "POST",
        &format!("/Playlists/{playlist_id}"),
        Some(&peer_token),
        Some(json!({ "Name": "Not yours" })),
    )
    .await;
    assert_eq!(peer_update.status(), axum::http::StatusCode::NOT_FOUND);
    let peer_delete = send(
        &router,
        "DELETE",
        &format!("/Playlists/{playlist_id}"),
        Some(&peer_token),
        None,
    )
    .await;
    assert_eq!(peer_delete.status(), axum::http::StatusCode::NOT_FOUND);
    let cross_user = send(
        &router,
        "GET",
        &format!("/Playlists?UserId={owner_id}"),
        Some(&peer_token),
        None,
    )
    .await;
    assert_eq!(cross_user.status(), axum::http::StatusCode::FORBIDDEN);

    let no_play_playlist = send(
        &router,
        "POST",
        "/Playlists",
        Some(&playback_disabled_token),
        Some(json!({ "Name": "Unavailable playback" })),
    )
    .await;
    assert_eq!(no_play_playlist.status(), axum::http::StatusCode::OK);
    let no_play_playlist: Value = response_json(no_play_playlist).await;
    let no_play_playlist_id = Uuid::parse_str(no_play_playlist["Id"].as_str().unwrap()).unwrap();
    let no_play_items = send(
        &router,
        "GET",
        &format!("/Playlists/{no_play_playlist_id}/Items"),
        Some(&playback_disabled_token),
        None,
    )
    .await;
    assert_eq!(no_play_items.status(), axum::http::StatusCode::FORBIDDEN);
    let no_play_add = send(
        &router,
        "POST",
        &format!("/Playlists/{no_play_playlist_id}/Items?Ids={first}"),
        Some(&playback_disabled_token),
        None,
    )
    .await;
    assert_eq!(no_play_add.status(), axum::http::StatusCode::FORBIDDEN);
    let no_playback = send(
        &router,
        "POST",
        "/Sessions/Playing",
        Some(&playback_disabled_token),
        Some(json!({ "ItemId": first })),
    )
    .await;
    assert_eq!(no_playback.status(), axum::http::StatusCode::FORBIDDEN);

    let delete = send(
        &router,
        "DELETE",
        &format!("/Playlists/{playlist_id}"),
        Some(&owner_token),
        None,
    )
    .await;
    assert_eq!(delete.status(), axum::http::StatusCode::NO_CONTENT);
    let deleted = send(
        &router,
        "GET",
        &format!("/Playlists/{playlist_id}"),
        Some(&owner_token),
        None,
    )
    .await;
    assert_eq!(deleted.status(), axum::http::StatusCode::NOT_FOUND);

    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .execute(&admin_pool)
        .await
        .unwrap();
}

async fn insert_audio(pool: &sqlx::PgPool, library_id: Uuid, name: &str, path: &str) -> Uuid {
    let item_id = Uuid::new_v4();
    sqlx::query("INSERT INTO items(id,library_id,name,sort_name,item_type,path,path_hash,container,runtime_ticks) VALUES ($1,$2,$3,$3,'Audio',$4,$5,'flac',1800000000)")
        .bind(item_id)
        .bind(library_id)
        .bind(name)
        .bind(path)
        .bind(db::path_hash(path))
        .execute(pool)
        .await
        .unwrap();
    item_id
}

async fn insert_user(pool: &sqlx::PgPool, username: &str, allow_media_playback: bool) -> Uuid {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,username,username_norm,password_hash,allow_media_playback,enable_remote_access) VALUES ($1,$2,$2,'unused',$3,TRUE)")
        .bind(id)
        .bind(username)
        .bind(allow_media_playback)
        .execute(pool)
        .await
        .unwrap();
    id
}

async fn send(
    router: &axum::Router,
    method: &str,
    uri: &str,
    token: Option<&str>,
    body: Option<Value>,
) -> axum::response::Response {
    let mut builder = Request::builder().method(method).uri(uri);
    if let Some(token) = token {
        builder = builder.header("Authorization", format!("MediaBrowser Token=\"{token}\""));
    }
    let request_body = if let Some(body) = body {
        builder = builder.header("content-type", "application/json");
        Body::from(body.to_string())
    } else {
        Body::empty()
    };
    router
        .clone()
        .oneshot(builder.body(request_body).unwrap())
        .await
        .unwrap()
}

async fn response_json(response: axum::response::Response) -> Value {
    let body = response.into_body().collect().await.unwrap().to_bytes();
    serde_json::from_slice(&body).unwrap()
}

async fn playlist_items(router: &axum::Router, playlist_id: Uuid, token: &str) -> Value {
    response_json(
        send(
            router,
            "GET",
            &format!("/Playlists/{playlist_id}/Items"),
            Some(token),
            None,
        )
        .await,
    )
    .await
}

fn item_ids(items: &Value) -> Vec<Uuid> {
    items["Items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|item| Uuid::parse_str(item["Id"].as_str().unwrap()).unwrap())
        .collect()
}
