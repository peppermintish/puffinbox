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
        dlna: Default::default(),
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

    let catalog = send(
        &router,
        "GET",
        "/Items?includeItemTypes=Playlist&recursive=true&searchTerm=road&mediaTypes=Audio",
        Some(&owner_token),
        None,
    )
    .await;
    assert_eq!(catalog.status(), axum::http::StatusCode::OK);
    assert_eq!(catalog.headers()["cache-control"], "private, no-store");
    let catalog = response_json(catalog).await;
    assert_eq!(catalog["TotalRecordCount"], 1);
    assert_eq!(catalog["Items"][0]["Id"], playlist_id.to_string());
    assert_eq!(catalog["Items"][0]["Type"], "Playlist");
    assert_eq!(catalog["Items"][0]["IsFolder"], true);
    assert_eq!(catalog["Items"][0]["CanDelete"], true);
    assert!(catalog["Items"][0].get("Path").is_none());

    let detail = send(
        &router,
        "GET",
        &format!("/Items/{playlist_id}?userId={owner_id}"),
        Some(&owner_token),
        None,
    )
    .await;
    assert_eq!(detail.status(), axum::http::StatusCode::OK);
    assert_eq!(detail.headers()["cache-control"], "private, no-store");
    let detail = response_json(detail).await;
    assert_eq!(detail["Name"], "Road trip");
    assert_eq!(detail["DateCreated"], catalog["Items"][0]["DateCreated"]);
    assert!(detail["DateModified"].is_string());
    let permission = send(
        &router,
        "GET",
        &format!("/Playlists/{playlist_id}/Users/{owner_id}"),
        Some(&owner_token),
        None,
    )
    .await;
    assert_eq!(permission.status(), axum::http::StatusCode::OK);
    assert_eq!(permission.headers()["cache-control"], "private, no-store");
    assert_eq!(
        response_json(permission).await,
        json!({"UserId":owner_id,"CanEdit":true})
    );
    for (requested_user, token) in [(peer_id, &owner_token), (owner_id, &peer_token)] {
        assert_eq!(
            send(
                &router,
                "GET",
                &format!("/Playlists/{playlist_id}/Users/{requested_user}"),
                Some(token),
                None
            )
            .await
            .status(),
            axum::http::StatusCode::NOT_FOUND
        );
    }
    let legacy = send(
        &router,
        "GET",
        &format!("/Users/{owner_id}/Items?IncludeItemTypes=Playlist"),
        Some(&owner_token),
        None,
    )
    .await;
    assert_eq!(legacy.status(), axum::http::StatusCode::OK);
    assert_eq!(item_ids(&response_json(legacy).await), vec![playlist_id]);
    let conflict = send(
        &router,
        "GET",
        &format!("/Users/{owner_id}/Items?UserId={peer_id}"),
        Some(&owner_token),
        None,
    )
    .await;
    assert_eq!(conflict.status(), axum::http::StatusCode::BAD_REQUEST);
    let peer_alias = send(
        &router,
        "GET",
        &format!("/Users/{owner_id}/Items?IncludeItemTypes=Playlist"),
        Some(&peer_token),
        None,
    )
    .await;
    assert_eq!(peer_alias.status(), axum::http::StatusCode::FORBIDDEN);
    for uri in [
        format!("/Items?ParentId={playlist_id}&Limit=300&Fields=Chapters,MediaSources,Trickplay"),
        format!(
            "/Users/{owner_id}/Items?ParentId={playlist_id}&Limit=300&Fields=Chapters,MediaSources,Trickplay"
        ),
        format!(
            "/Users/{owner_id}/Items?ParentId={playlist_id}&Limit=300&Fields=Chapters,MediaSources,Trickplay&ExcludeLocationTypes=Virtual&CollapseBoxSetItems=false"
        ),
    ] {
        let entries = send(&router, "GET", &uri, Some(&owner_token), None).await;
        assert_eq!(entries.status(), axum::http::StatusCode::OK);
        assert_eq!(entries.headers()["cache-control"], "private, no-store");
        let entries = response_json(entries).await;
        assert_eq!(entries["TotalRecordCount"], 2);
        assert_eq!(item_ids(&entries), vec![first, second]);
        assert_ne!(
            entries["Items"][0]["PlaylistItemId"],
            entries["Items"][1]["PlaylistItemId"]
        );
    }
    let entry_page = response_json(send(&router, "GET", &format!("/Users/{owner_id}/Items?ParentId={playlist_id}&StartIndex=1&Limit=1&EnableTotalRecordCount=false"), Some(&owner_token), None).await).await;
    assert_eq!(item_ids(&entry_page), vec![second]);
    assert_eq!(entry_page["StartIndex"], 1);
    assert!(entry_page.get("TotalRecordCount").is_none());
    for query in [
        "SortBy=Name",
        "Filters=IsFavorite",
        "Filters=IsFolder",
        "Filters=IsNotFolder",
        "SearchTerm=First",
        "Ids=invalid",
        "ExcludeLocationTypes=FileSystem",
        "ExcludeLocationTypes=Virtual,FileSystem",
        "CollapseBoxSetItems=true",
    ] {
        assert_eq!(
            send(
                &router,
                "GET",
                &format!("/Items?ParentId={playlist_id}&{query}"),
                Some(&owner_token),
                None
            )
            .await
            .status(),
            axum::http::StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        send(
            &router,
            "GET",
            &format!("/Items?ParentId={playlist_id}"),
            Some(&peer_token),
            None
        )
        .await
        .status(),
        axum::http::StatusCode::NOT_FOUND
    );
    for suffix in ["Ancestors", "ThemeMedia"] {
        let response = send(
            &router,
            "GET",
            &format!("/Items/{playlist_id}/{suffix}"),
            Some(&owner_token),
            None,
        )
        .await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert_eq!(response.headers()["cache-control"], "private, no-store");
        let response = response_json(response).await;
        if suffix == "Ancestors" {
            assert_eq!(response, json!([]));
        } else {
            for key in [
                "ThemeSongsResult",
                "ThemeVideosResult",
                "SoundtrackSongsResult",
            ] {
                assert_eq!(response[key]["Items"], json!([]));
                assert_eq!(response[key]["TotalRecordCount"], 0);
            }
        }
        assert_eq!(
            send(
                &router,
                "GET",
                &format!("/Items/{playlist_id}/{suffix}"),
                Some(&peer_token),
                None
            )
            .await
            .status(),
            axum::http::StatusCode::NOT_FOUND
        );
    }

    let second_playlist = response_json(
        send(
            &router,
            "POST",
            "/Playlists",
            Some(&owner_token),
            Some(json!({ "Name": "Road 100%_mix", "MediaType": "Audio" })),
        )
        .await,
    )
    .await;
    let second_playlist_id = Uuid::parse_str(second_playlist["Id"].as_str().unwrap()).unwrap();
    sqlx::query("UPDATE playlists SET created_at='2020-01-01T00:00:00Z',updated_at='2023-01-01T00:00:00Z' WHERE id=$1")
        .bind(playlist_id).execute(&pool).await.unwrap();
    sqlx::query("UPDATE playlists SET created_at='2021-01-01T00:00:00Z',updated_at='2022-01-01T00:00:00Z' WHERE id=$1")
        .bind(second_playlist_id).execute(&pool).await.unwrap();
    for (sort, order, expected) in [
        ("DateCreated", "Ascending", playlist_id),
        ("DateAdded", "Descending", second_playlist_id),
        ("DateModified", "Ascending", second_playlist_id),
        ("DateModified", "Descending", playlist_id),
    ] {
        let response = send(
            &router,
            "GET",
            &format!("/Items?IncludeItemTypes=Playlist&SortBy={sort}&SortOrder={order}&Limit=1"),
            Some(&owner_token),
            None,
        )
        .await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        assert_eq!(item_ids(&response_json(response).await), vec![expected]);
    }
    for (query, expected_id) in [
        (
            "SortBy=Name&SortOrder=Descending&StartIndex=1&Limit=1",
            second_playlist_id,
        ),
        (
            "SearchTerm=100%25_mix&EnableTotalRecordCount=false",
            second_playlist_id,
        ),
        (
            "SortBy=DateCreated&SortOrder=Ascending&Limit=1&CollapseBoxSetItems=false&ExcludeLocationTypes=Virtual&EnableTotalRecordCount=false",
            playlist_id,
        ),
    ] {
        let response = send(
            &router,
            "GET",
            &format!("/Items?IncludeItemTypes=Playlist&{query}"),
            Some(&owner_token),
            None,
        )
        .await;
        assert_eq!(response.status(), axum::http::StatusCode::OK);
        let response = response_json(response).await;
        assert_eq!(item_ids(&response), vec![expected_id]);
        if query.contains("false") {
            assert!(response.get("TotalRecordCount").is_none());
        } else {
            assert_eq!(response["TotalRecordCount"], 2);
            assert_eq!(response["StartIndex"], 1);
        }
    }
    for query in ["MediaTypes=Video", "SearchTerm=absent", "StartIndex=100"] {
        let response = response_json(
            send(
                &router,
                "GET",
                &format!("/Items?IncludeItemTypes=Playlist&{query}"),
                Some(&owner_token),
                None,
            )
            .await,
        )
        .await;
        assert_eq!(response["Items"], json!([]));
    }
    for query in [
        "Filters=IsFavorite",
        "IsPlayed=false",
        "Filters=IsFolder",
        "Filters=IsNotFolder",
        "Genres=Rock",
        "Tags=Road",
        "SortBy=LastPlayedDate",
        "SortBy=Name,DateCreated",
        "StartIndex=-1",
    ] {
        let response = send(
            &router,
            "GET",
            &format!("/Items?IncludeItemTypes=Playlist&{query}"),
            Some(&owner_token),
            None,
        )
        .await;
        assert_eq!(
            response.status(),
            axum::http::StatusCode::BAD_REQUEST,
            "{query}"
        );
    }
    let mixed = send(
        &router,
        "GET",
        "/Items?IncludeItemTypes=Playlist,Audio",
        Some(&owner_token),
        None,
    )
    .await;
    assert_eq!(mixed.status(), axum::http::StatusCode::BAD_REQUEST);
    let parent_scope = send(
        &router,
        "GET",
        &format!("/Items?IncludeItemTypes=Playlist&ParentId={library_id}"),
        Some(&owner_token),
        None,
    )
    .await;
    assert_eq!(parent_scope.status(), axum::http::StatusCode::OK);
    let parent_scope = response_json(parent_scope).await;
    assert_eq!(parent_scope["TotalRecordCount"], 1);
    assert_eq!(item_ids(&parent_scope), vec![playlist_id]);
    sqlx::query("UPDATE items SET path='/music/.private/'||name WHERE id=ANY($1)")
        .bind(vec![first, second])
        .execute(&pool)
        .await
        .unwrap();
    let hidden_tracks = response_json(
        send(
            &router,
            "GET",
            &format!("/Items?IncludeItemTypes=Playlist&ParentId={library_id}"),
            Some(&owner_token),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(hidden_tracks["TotalRecordCount"], 0);
    assert_eq!(hidden_tracks["Items"], json!([]));
    sqlx::query("UPDATE items SET path='/music/first.flac' WHERE id=$1")
        .bind(first)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("UPDATE items SET path='/music/second.flac' WHERE id=$1")
        .bind(second)
        .execute(&pool)
        .await
        .unwrap();
    let unknown_parent = send(
        &router,
        "GET",
        &format!(
            "/Items?IncludeItemTypes=Playlist&ParentId={}",
            Uuid::new_v4()
        ),
        Some(&owner_token),
        None,
    )
    .await;
    assert_eq!(unknown_parent.status(), axum::http::StatusCode::NOT_FOUND);
    sqlx::query("UPDATE libraries SET enabled=FALSE WHERE id=$1")
        .bind(library_id)
        .execute(&pool)
        .await
        .unwrap();
    let disabled_parent = send(
        &router,
        "GET",
        &format!("/Items?IncludeItemTypes=Playlist&ParentId={library_id}"),
        Some(&owner_token),
        None,
    )
    .await;
    assert_eq!(disabled_parent.status(), axum::http::StatusCode::NOT_FOUND);
    sqlx::query("UPDATE libraries SET enabled=TRUE WHERE id=$1")
        .bind(library_id)
        .execute(&pool)
        .await
        .unwrap();
    let peer_catalog = response_json(
        send(
            &router,
            "GET",
            "/Items?IncludeItemTypes=Playlist",
            Some(&peer_token),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(peer_catalog["TotalRecordCount"], 0);
    for method in ["GET", "DELETE"] {
        let response = send(
            &router,
            method,
            &format!("/Items/{playlist_id}"),
            Some(&peer_token),
            None,
        )
        .await;
        assert_eq!(response.status(), axum::http::StatusCode::NOT_FOUND);
    }
    let cross_catalog = send(
        &router,
        "GET",
        &format!("/Items?IncludeItemTypes=Playlist&UserId={owner_id}"),
        Some(&peer_token),
        None,
    )
    .await;
    assert_eq!(cross_catalog.status(), axum::http::StatusCode::FORBIDDEN);
    let atomic_delete = send(
        &router,
        "DELETE",
        &format!("/Items?ids={second_playlist_id},{first}"),
        Some(&owner_token),
        None,
    )
    .await;
    assert_eq!(atomic_delete.status(), axum::http::StatusCode::NOT_FOUND);
    assert_eq!(
        send(
            &router,
            "GET",
            &format!("/Items/{second_playlist_id}"),
            Some(&owner_token),
            None
        )
        .await
        .status(),
        axum::http::StatusCode::OK
    );
    let bulk_delete = send(
        &router,
        "DELETE",
        &format!("/Items?ids={second_playlist_id},{second_playlist_id}"),
        Some(&owner_token),
        None,
    )
    .await;
    assert_eq!(bulk_delete.status(), axum::http::StatusCode::NO_CONTENT);
    assert!(db::get_item(&pool, first).await.unwrap().is_some());

    for (method, uri) in [
        ("GET", format!("/Items/{playlist_id}")),
        ("DELETE", format!("/Items/{playlist_id}")),
        ("DELETE", format!("/Items?Ids={playlist_id}")),
    ] {
        assert_eq!(
            send(&router, method, &uri, None, None).await.status(),
            axum::http::StatusCode::UNAUTHORIZED
        );
    }
    for query in [
        "Ids=invalid",
        "ExcludeItemTypes=Playlist",
        "IsFavorite=true",
        "UnknownOption=true",
    ] {
        assert_eq!(
            send(
                &router,
                "GET",
                &format!("/Items?IncludeItemTypes=Playlist&{query}"),
                Some(&owner_token),
                None
            )
            .await
            .status(),
            axum::http::StatusCode::BAD_REQUEST
        );
    }
    for uri in [
        "/Items".to_owned(),
        "/Items?Ids=invalid".to_owned(),
        format!("/Items?Ids={}&UserId={peer_id}", playlist_id),
    ] {
        let expected = if uri.contains("UserId") {
            axum::http::StatusCode::FORBIDDEN
        } else {
            axum::http::StatusCode::BAD_REQUEST
        };
        assert_eq!(
            send(&router, "DELETE", &uri, Some(&owner_token), None)
                .await
                .status(),
            expected
        );
    }
    sqlx::query("UPDATE users SET is_admin=TRUE WHERE id=$1")
        .bind(peer_id)
        .execute(&pool)
        .await
        .unwrap();
    assert_eq!(
        send(
            &router,
            "GET",
            &format!("/Items?IncludeItemTypes=Playlist&UserId={owner_id}"),
            Some(&peer_token),
            None
        )
        .await
        .status(),
        axum::http::StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            &router,
            "GET",
            &format!("/Items/{playlist_id}?UserId={owner_id}"),
            Some(&peer_token),
            None
        )
        .await
        .status(),
        axum::http::StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            &router,
            "DELETE",
            &format!("/Items/{playlist_id}"),
            Some(&peer_token),
            None
        )
        .await
        .status(),
        axum::http::StatusCode::NOT_FOUND
    );
    sqlx::query("UPDATE users SET is_admin=FALSE WHERE id=$1")
        .bind(peer_id)
        .execute(&pool)
        .await
        .unwrap();

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
    let hidden_parent = send(
        &router,
        "GET",
        &format!("/Items?IncludeItemTypes=Playlist&ParentId={library_id}"),
        Some(&owner_token),
        None,
    )
    .await;
    assert_eq!(hidden_parent.status(), axum::http::StatusCode::NOT_FOUND);
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

    shared_playlists_keep_permissions_and_media_visibility_separate(
        &router,
        &pool,
        owner_id,
        peer_id,
        &owner_token,
        &peer_token,
        first,
        second,
        third,
    )
    .await;

    let delete = send(
        &router,
        "DELETE",
        &format!("/Items/{playlist_id}"),
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

#[allow(clippy::too_many_arguments)]
async fn shared_playlists_keep_permissions_and_media_visibility_separate(
    router: &axum::Router,
    pool: &sqlx::PgPool,
    owner_id: Uuid,
    peer_id: Uuid,
    owner_token: &str,
    peer_token: &str,
    first: Uuid,
    second: Uuid,
    third: Uuid,
) {
    use axum::http::StatusCode;
    sqlx::query("UPDATE users SET restrict_libraries=FALSE WHERE id=$1")
        .bind(owner_id)
        .execute(pool)
        .await
        .unwrap();
    let created = send(
        router,
        "POST",
        "/Playlists",
        Some(owner_token),
        Some(json!({
            "Name":"Shared music", "Ids":[first,second],
            "Users":[{"UserId":peer_id,"CanEdit":false}]
        })),
    )
    .await;
    assert_eq!(
        created.status(),
        StatusCode::OK,
        "explicit playlist sharing must be accepted"
    );
    let id = Uuid::parse_str(response_json(created).await["Id"].as_str().unwrap()).unwrap();
    let uri = format!("/Playlists/{id}");
    let permission = format!("{uri}/Users/{peer_id}");
    let entries_uri = format!("{uri}/Items");
    let detail = response_json(
        send(
            router,
            "GET",
            &format!("/Items/{id}"),
            Some(peer_token),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(detail["CanDelete"], false);
    assert_eq!(detail["Name"], "Shared music");
    assert_eq!(
        item_ids(&playlist_items(router, id, peer_token).await),
        vec![first, second]
    );
    let catalog = response_json(
        send(
            router,
            "GET",
            "/Items?IncludeItemTypes=Playlist",
            Some(peer_token),
            None,
        )
        .await,
    )
    .await;
    assert!(item_ids(&catalog).contains(&id));
    assert_eq!(
        response_json(send(router, "GET", &permission, Some(peer_token), None).await).await,
        json!({"UserId":peer_id,"CanEdit":false})
    );
    let shares = send(
        router,
        "GET",
        &format!("{uri}/Users"),
        Some(owner_token),
        None,
    )
    .await;
    assert_eq!(shares.status(), StatusCode::OK);
    assert_eq!(shares.headers()["cache-control"], "private, no-store");
    assert_eq!(
        response_json(shares).await,
        json!([{"UserId":peer_id,"CanEdit":false}])
    );
    assert_eq!(
        send(
            router,
            "GET",
            &format!("{uri}/Users"),
            Some(peer_token),
            None
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    for suffix in ["Ancestors", "ThemeMedia"] {
        assert_eq!(
            send(
                router,
                "GET",
                &format!("/Items/{id}/{suffix}"),
                Some(peer_token),
                None
            )
            .await
            .status(),
            StatusCode::OK
        );
    }
    assert_eq!(
        item_ids(
            &response_json(
                send(
                    router,
                    "GET",
                    &format!("/Items?ParentId={id}"),
                    Some(peer_token),
                    None
                )
                .await
            )
            .await
        ),
        vec![first, second]
    );
    for (method, path, body) in [
        (
            "POST",
            uri.clone(),
            Some(json!({"Name":"Reader cannot rename"})),
        ),
        ("POST", format!("{entries_uri}?Ids={third}"), None),
        (
            "DELETE",
            format!("{entries_uri}?EntryIds={}", Uuid::new_v4()),
            None,
        ),
        ("POST", permission.clone(), Some(json!({"CanEdit":true}))),
    ] {
        assert_eq!(
            send(router, method, &path, Some(peer_token), body)
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        send(router, "DELETE", &uri, Some(peer_token), None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            router,
            "POST",
            &permission,
            Some(owner_token),
            Some(json!({"CanEdit":true}))
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        send(
            router,
            "POST",
            &uri,
            Some(peer_token),
            Some(json!({"Name":"Edited by peer"}))
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        send(
            router,
            "POST",
            &format!("{entries_uri}?Ids={third}"),
            Some(peer_token),
            None
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        item_ids(&playlist_items(router, id, owner_token).await),
        vec![first, second, third]
    );
    assert_eq!(
        send(
            router,
            "POST",
            &uri,
            Some(peer_token),
            Some(json!({"Users":[]}))
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    assert_eq!(
        send(
            router,
            "POST",
            &format!("{uri}/Users/{owner_id}"),
            Some(owner_token),
            Some(json!({"CanEdit":false}))
        )
        .await
        .status(),
        StatusCode::BAD_REQUEST
    );
    assert_eq!(
        send(
            router,
            "DELETE",
            &format!("{uri}/Users/{owner_id}"),
            Some(owner_token),
            None
        )
        .await
        .status(),
        StatusCode::FORBIDDEN
    );
    let invalid = send(
        router,
        "POST",
        &uri,
        Some(owner_token),
        Some(json!({"Name":"Must roll back","Users":[{"UserId":Uuid::new_v4(),"CanEdit":true}]})),
    )
    .await;
    assert_eq!(invalid.status(), StatusCode::BAD_REQUEST);
    let detail = response_json(
        send(
            router,
            "GET",
            &format!("/Items/{id}"),
            Some(owner_token),
            None,
        )
        .await,
    )
    .await;
    assert_eq!(detail["Name"], "Edited by peer");
    assert_eq!(
        send(
            router,
            "POST",
            &permission,
            Some(owner_token),
            Some(json!({"CanEdit":null}))
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        send(
            router,
            "POST",
            &permission,
            Some(owner_token),
            Some(json!({"CanEdit":"false"}))
        )
        .await
        .status(),
        StatusCode::UNPROCESSABLE_ENTITY
    );
    sqlx::query("UPDATE items SET path='/music/.hidden/second.flac' WHERE id=$1")
        .bind(second)
        .execute(pool)
        .await
        .unwrap();
    assert_eq!(
        item_ids(&playlist_items(router, id, peer_token).await),
        vec![first, third]
    );
    let hidden_replacement = send(
        router,
        "POST",
        &uri,
        Some(peer_token),
        Some(json!({"Ids":[first]})),
    )
    .await;
    assert_eq!(hidden_replacement.status(), StatusCode::FORBIDDEN);
    let visible_entries = playlist_items(router, id, peer_token).await;
    let entry_id = visible_entries["Items"][0]["PlaylistItemId"]
        .as_str()
        .unwrap();
    for (method, path) in [
        ("POST", format!("{entries_uri}?Ids={third}")),
        ("DELETE", format!("{entries_uri}?EntryIds={entry_id}")),
        ("POST", format!("{entries_uri}/{entry_id}/Move/0")),
    ] {
        assert_eq!(
            send(router, method, &path, Some(peer_token), None)
                .await
                .status(),
            StatusCode::FORBIDDEN
        );
    }
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM playlist_items WHERE playlist_id=$1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap(),
        3
    );
    sqlx::query("UPDATE items SET path='/music/second.flac' WHERE id=$1")
        .bind(second)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("UPDATE users SET restrict_libraries=TRUE WHERE id=$1")
        .bind(peer_id)
        .execute(pool)
        .await
        .unwrap();
    assert!(item_ids(&playlist_items(router, id, peer_token).await).is_empty());
    assert_eq!(
        send(
            router,
            "POST",
            &format!("{entries_uri}?Ids={first}"),
            Some(peer_token),
            None
        )
        .await
        .status(),
        StatusCode::NOT_FOUND
    );
    sqlx::query("UPDATE users SET restrict_libraries=FALSE WHERE id=$1")
        .bind(peer_id)
        .execute(pool)
        .await
        .unwrap();
    assert_eq!(
        send(router, "DELETE", &permission, Some(owner_token), None)
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    for path in [
        &uri,
        &entries_uri,
        &format!("/Items/{id}"),
        &format!("/Items?ParentId={id}"),
    ] {
        assert_eq!(
            send(router, "GET", path, Some(peer_token), None)
                .await
                .status(),
            StatusCode::NOT_FOUND
        );
    }
    assert!(
        !item_ids(
            &response_json(
                send(
                    router,
                    "GET",
                    "/Items?IncludeItemTypes=Playlist",
                    Some(peer_token),
                    None
                )
                .await
            )
            .await
        )
        .contains(&id)
    );
    assert_eq!(send(router,"POST", &uri,Some(owner_token),Some(json!({"Users":[{"UserId":peer_id,"CanEdit":false},{"UserId":peer_id,"CanEdit":true}]}))).await.status(),StatusCode::BAD_REQUEST);
    assert_eq!(
        send(
            router,
            "POST",
            &uri,
            Some(owner_token),
            Some(json!({"Users":[{"UserId":peer_id,"CanEdit":false}]}))
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        send(
            router,
            "POST",
            &uri,
            Some(owner_token),
            Some(json!({"Users":null}))
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        send(router, "GET", &uri, Some(peer_token), None)
            .await
            .status(),
        StatusCode::OK
    );
    assert_eq!(
        send(
            router,
            "POST",
            &uri,
            Some(owner_token),
            Some(json!({"Users":[]}))
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        send(router, "GET", &uri, Some(peer_token), None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        send(
            router,
            "POST",
            &permission,
            Some(owner_token),
            Some(json!({"CanEdit":true}))
        )
        .await
        .status(),
        StatusCode::NO_CONTENT
    );
    let mut revocation = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM playlists WHERE id=$1 FOR UPDATE")
        .bind(id)
        .execute(&mut *revocation)
        .await
        .unwrap();
    let waiting_router = router.clone();
    let waiting_token = peer_token.to_owned();
    let waiting_uri = uri.clone();
    let editor = tokio::spawn(async move {
        send(
            &waiting_router,
            "POST",
            &waiting_uri,
            Some(&waiting_token),
            Some(json!({"Name":"Revoked writer"})),
        )
        .await
        .status()
    });
    tokio::time::timeout(Duration::from_secs(5),async {
        loop {
            let waiting = sqlx::query_scalar::<_,bool>("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND wait_event_type='Lock' AND query='SELECT owner_user_id FROM playlists WHERE id=$1 FOR UPDATE')")
                .fetch_one(pool).await.unwrap();
            if waiting { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("editor must wait behind the playlist row lock");
    sqlx::query("DELETE FROM playlist_users WHERE playlist_id=$1 AND user_id=$2")
        .bind(id)
        .bind(peer_id)
        .execute(&mut *revocation)
        .await
        .unwrap();
    revocation.commit().await.unwrap();
    assert_eq!(
        tokio::time::timeout(Duration::from_secs(5), editor)
            .await
            .unwrap()
            .unwrap(),
        StatusCode::NOT_FOUND
    );
    assert_eq!(
        sqlx::query_scalar::<_, String>("SELECT name FROM playlists WHERE id=$1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap(),
        "Edited by peer"
    );
    assert_eq!(
        send(router, "DELETE", &uri, Some(owner_token), None)
            .await
            .status(),
        StatusCode::NO_CONTENT
    );
    assert_eq!(
        sqlx::query_scalar::<_, i64>("SELECT COUNT(*) FROM playlist_users WHERE playlist_id=$1")
            .bind(id)
            .fetch_one(pool)
            .await
            .unwrap(),
        0
    );
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
