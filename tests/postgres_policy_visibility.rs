use std::{env, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use axum::{body::Body, http::Request};
use chrono::{Duration as ChronoDuration, Utc};
use http_body_util::BodyExt;
use ipnet::IpNet;
use puffinbox::{
    AppState, Config, api, auth, db,
    library::{ItemQuery, ItemRecord},
};
use sqlx::{PgPool, postgres::PgPoolOptions};
use tower::ServiceExt;
use uuid::Uuid;

mod common;

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via PUFFINBOX_TEST_DATABASE_URL"]
async fn database_and_direct_item_api_hide_restricted_or_legacy_catalog_rows() {
    let database_url = env::var("PUFFINBOX_TEST_DATABASE_URL")
        .expect("set PUFFINBOX_TEST_DATABASE_URL to a disposable PostgreSQL database");
    let admin_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url)
        .await
        .unwrap();
    let schema = format!("puffinbox_policy_test_{}", Uuid::new_v4().simple());
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
    let policy_index_exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM pg_indexes WHERE schemaname=current_schema() AND indexname='live_tv_recordings_item_policy_idx')",
    )
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(policy_index_exists);
    let run_id = Uuid::new_v4();
    db::activate_run(&pool, run_id).await.unwrap();

    let library_id = Uuid::new_v4();
    db::insert_library(
        &pool,
        run_id,
        library_id,
        "Policy test",
        "movies",
        &[PathBuf::from("/media")],
        true,
    )
    .await
    .unwrap();
    let user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,username,username_norm,password_hash,restrict_libraries,max_parental_rating,block_unrated_items,enable_remote_access) VALUES ($1,'restricted','restricted','unused',TRUE,50,ARRAY['Movie']::TEXT[],TRUE)")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();

    let admin_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,username,username_norm,password_hash,is_admin,enable_remote_access) VALUES ($1,'admin','admin','unused',TRUE,TRUE)")
        .bind(admin_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO user_library_access(user_id,library_id) VALUES ($1,$2)")
        .bind(user_id)
        .bind(library_id)
        .execute(&pool)
        .await
        .unwrap();

    let allowed_folder = insert_item(
        &pool,
        library_id,
        None,
        "Allowed",
        "Folder",
        "/media/Allowed",
        None,
    )
    .await;
    let allowed_movie = insert_item(
        &pool,
        library_id,
        Some(allowed_folder.id),
        "allowed.mkv",
        "Movie",
        "/media/Allowed/allowed.mkv",
        Some(40),
    )
    .await;
    set_local_policy_rating(&pool, allowed_movie.id, "PG-13", 50).await;
    let allowed_movie = db::get_item(&pool, allowed_movie.id)
        .await
        .unwrap()
        .unwrap();
    let blocked_folder = insert_item(
        &pool,
        library_id,
        None,
        "Blocked",
        "Folder",
        "/media/Blocked",
        None,
    )
    .await;
    let blocked_movie = insert_item(
        &pool,
        library_id,
        Some(blocked_folder.id),
        "blocked.mkv",
        "Movie",
        "/media/Blocked/blocked.mkv",
        Some(80),
    )
    .await;
    set_local_policy_rating(&pool, blocked_movie.id, "R", 75).await;
    let blocked_movie = db::get_item(&pool, blocked_movie.id)
        .await
        .unwrap()
        .unwrap();
    let unrated_folder = insert_item(
        &pool,
        library_id,
        None,
        "Unrated",
        "Folder",
        "/media/Unrated",
        None,
    )
    .await;
    let unrated_movie = insert_item(
        &pool,
        library_id,
        Some(unrated_folder.id),
        "unrated.mkv",
        "Movie",
        "/media/Unrated/unrated.mkv",
        Some(10),
    )
    .await;
    // An explicit local-NFO "Not Rated" row keeps the item unrated even when
    // a legacy catalog score is present.
    sqlx::query("INSERT INTO item_metadata(item_id,provider_key,content_rating) VALUES ($1,'local-nfo','Not Rated')")
        .bind(unrated_movie.id)
        .execute(&pool)
        .await
        .unwrap();
    let unrated_movie = db::get_item(&pool, unrated_movie.id)
        .await
        .unwrap()
        .unwrap();
    let catalog_rated_movie = insert_item(
        &pool,
        library_id,
        Some(allowed_folder.id),
        "catalog-rated.mkv",
        "Movie",
        "/media/Allowed/catalog-rated.mkv",
        Some(45),
    )
    .await;
    assert_eq!(allowed_movie.rating, Some(50));
    assert_eq!(blocked_movie.rating, Some(75));
    let raw_unrated_score: Option<i16> = sqlx::query_scalar("SELECT rating FROM items WHERE id=$1")
        .bind(unrated_movie.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(raw_unrated_score, Some(10));
    assert_eq!(unrated_movie.rating, None);
    let raw_catalog_score: Option<i16> = sqlx::query_scalar("SELECT rating FROM items WHERE id=$1")
        .bind(catalog_rated_movie.id)
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(raw_catalog_score, Some(45));
    assert_eq!(catalog_rated_movie.rating, None);
    let hidden_movie = insert_item(
        &pool,
        library_id,
        None,
        "secret.mkv",
        "Movie",
        "/media/.private/secret.mkv",
        Some(10),
    )
    .await;
    let legacy_file = insert_item(
        &pool,
        library_id,
        None,
        "credentials.env",
        "File",
        "/media/credentials.env",
        None,
    )
    .await;

    let restricted = auth::UserRecord {
        id: user_id,
        username: "restricted".to_owned(),
        is_admin: false,
        disabled: false,
        enable_remote_access: true,
        allow_media_playback: true,
        enable_content_downloading: true,
        enable_live_tv_access: false,
        enable_live_tv_management: false,
        restrict_libraries: true,
        max_parental_rating: Some(50),
        block_unrated_items: vec!["Movie".to_owned()],
        allowed_library_ids: vec![library_id],
        configuration: Default::default(),
    };
    assert!(
        !db::item_visible_to_user(&pool, &restricted, &catalog_rated_movie)
            .await
            .unwrap(),
        "an unclassified catalog score is not parental-rating provenance"
    );
    let mut stricter_user = restricted.clone();
    stricter_user.max_parental_rating = Some(40);
    assert!(
        !db::item_visible_to_user(&pool, &stricter_user, &catalog_rated_movie)
            .await
            .unwrap(),
        "an unclassified catalog score remains unrated under stricter policy"
    );
    assert!(
        db::item_visible_to_user(&pool, &restricted, &allowed_folder)
            .await
            .unwrap()
    );
    // The unrated folder has no visible descendants; this exercises the
    // recursive CTE's category lookup through each child's metadata_json.
    for blocked in [
        &blocked_folder,
        &blocked_movie,
        &unrated_folder,
        &unrated_movie,
        &hidden_movie,
        &legacy_file,
    ] {
        assert!(
            !db::item_visible_to_user(&pool, &restricted, blocked)
                .await
                .unwrap()
        );
    }

    // A completed DVR recording has an explicit server-owned US-PARENTAL-v1
    // snapshot. It remains policy-rated after the source guide can change.
    let recording_library_id = Uuid::new_v4();
    db::insert_library(
        &pool,
        run_id,
        recording_library_id,
        "Recording policy fixture",
        "movies",
        &[PathBuf::from("/media/recording-policy")],
        true,
    )
    .await
    .unwrap();
    let recording_user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,username,username_norm,password_hash,restrict_libraries,max_parental_rating,block_unrated_items,enable_live_tv_access) VALUES ($1,'recording-policy','recording-policy','unused',TRUE,50,ARRAY['LiveTvProgram']::TEXT[],TRUE)")
        .bind(recording_user_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO user_library_access(user_id,library_id) VALUES($1,$2)")
        .bind(recording_user_id)
        .bind(recording_library_id)
        .execute(&pool)
        .await
        .unwrap();
    let recording_user = db::get_user(&pool, recording_user_id)
        .await
        .unwrap()
        .unwrap();
    let source_id = Uuid::new_v4();
    sqlx::query("INSERT INTO live_tv_sources(id,library_id,name,playlist_url,origin_pins) VALUES($1,$2,'Policy fixture','https://example.invalid/list.m3u','[]'::jsonb)")
        .bind(source_id)
        .bind(recording_library_id)
        .execute(&pool)
        .await
        .unwrap();
    let channel = insert_item(
        &pool,
        recording_library_id,
        None,
        "Policy fixture channel",
        "LiveTvChannel",
        "/media/recording-policy/policy-fixture-channel",
        None,
    )
    .await;
    sqlx::query("INSERT INTO live_tv_channels(item_id,library_id,source_id,source_channel_id,name,stream_url) VALUES($1,$2,$3,'policy-fixture','Policy fixture channel','https://example.invalid/live.m3u8')")
        .bind(channel.id)
        .bind(recording_library_id)
        .bind(source_id)
        .execute(&pool)
        .await
        .unwrap();
    let recorded_item = insert_item(
        &pool,
        recording_library_id,
        None,
        "Recorded policy fixture.mkv",
        "Movie",
        "/media/recording-policy/Recorded policy fixture.mkv",
        None,
    )
    .await;
    sqlx::query(
        "UPDATE items SET metadata_json=jsonb_build_object('LiveTvRecording',TRUE) WHERE id=$1",
    )
    .bind(recorded_item.id)
    .execute(&pool)
    .await
    .unwrap();
    let timer_id = Uuid::new_v4();
    sqlx::query("INSERT INTO live_tv_timers(id,owner_user_id,channel_item_id,start_at,end_at,output_library_id,status,started_at,finished_at,policy_rating_scale,policy_rating_value) VALUES($1,$2,$3,NOW()-INTERVAL '1 hour',NOW()-INTERVAL '1 second',$4,'completed',NOW()-INTERVAL '1 hour',NOW(),'US-PARENTAL-v1',50)")
        .bind(timer_id)
        .bind(recording_user_id)
        .bind(channel.id)
        .bind(recording_library_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO live_tv_recordings(id,timer_id,channel_item_id,library_id,channel_name,title,relative_path,item_id,status,byte_count,sha256,started_at,finished_at,policy_rating_scale,policy_rating_value) VALUES($1,$2,$3,$4,'Policy fixture channel','Recorded policy fixture','recordings/policy-fixture.mkv',$5,'completed',1,repeat('a',64),NOW()-INTERVAL '1 hour',NOW(),'US-PARENTAL-v1',50)")
        .bind(Uuid::new_v4())
        .bind(timer_id)
        .bind(channel.id)
        .bind(recording_library_id)
        .bind(recorded_item.id)
        .execute(&pool)
        .await
        .unwrap();
    let recorded_item = db::get_item(&pool, recorded_item.id)
        .await
        .unwrap()
        .unwrap();
    assert_eq!(recorded_item.rating, Some(50));
    assert!(
        db::item_visible_to_user(&pool, &recording_user, &recorded_item)
            .await
            .unwrap()
    );
    assert!(
        db::library_visible_to_user(&pool, &recording_user, recording_library_id)
            .await
            .unwrap()
    );
    let (recording_movies, recording_movie_count) = db::browse_items(
        &pool,
        &recording_user,
        ItemQuery {
            parent_id: Some(recording_library_id),
            include_item_types: vec!["Movie".to_owned()],
            limit: 10,
            enable_total_record_count: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(recording_movie_count, Some(1));
    assert_eq!(
        recording_movies
            .iter()
            .map(|item| item.id)
            .collect::<Vec<_>>(),
        [recorded_item.id]
    );
    let mut plan_transaction = pool.begin().await.unwrap();
    sqlx::query("SET LOCAL enable_seqscan=off")
        .execute(&mut *plan_transaction)
        .await
        .unwrap();
    let policy_lookup_plan = sqlx::query_scalar::<_, String>(
        "EXPLAIN (COSTS OFF) SELECT r.policy_rating_value FROM live_tv_recordings r WHERE r.item_id=$1 AND r.status='completed' AND r.policy_rating_scale='US-PARENTAL-v1'",
    )
    .bind(recorded_item.id)
    .fetch_all(&mut *plan_transaction)
    .await
    .unwrap()
    .join("\n");
    plan_transaction.rollback().await.unwrap();
    assert!(policy_lookup_plan.contains("live_tv_recordings_item_policy_idx"));

    let (top_level, total) = db::browse_items(
        &pool,
        &restricted,
        ItemQuery {
            parent_id: Some(library_id),
            limit: 100,
            enable_total_record_count: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert_eq!(total, Some(1));
    assert_eq!(
        top_level.iter().map(|item| item.id).collect::<Vec<_>>(),
        [allowed_folder.id]
    );
    let (blocked_children, blocked_total) = db::browse_items(
        &pool,
        &restricted,
        ItemQuery {
            parent_id: Some(blocked_folder.id),
            limit: 100,
            enable_total_record_count: true,
            ..Default::default()
        },
    )
    .await
    .unwrap();
    assert!(blocked_children.is_empty());
    assert_eq!(blocked_total, Some(0));
    let counts = db::item_counts(&pool, &restricted).await.unwrap();
    assert_eq!(counts.movie_count, 1);
    assert!(
        db::library_visible_to_user(&pool, &restricted, library_id)
            .await
            .unwrap()
    );

    let token = "policy-test-token";
    create_token(&pool, run_id, user_id, token).await;
    let admin_token = "policy-admin-test-token";
    create_token(&pool, run_id, admin_id, admin_token).await;
    let server_id = Uuid::new_v4();
    let router = api::router(AppState::new_for_run(
        pool.clone(),
        Arc::new(test_config(database_url.clone())),
        server_id,
        run_id,
        None,
    ));
    assert_eq!(
        get_status(&router, &format!("/Items/{}", allowed_folder.id), token).await,
        axum::http::StatusCode::OK
    );
    let (library_status, library_json) =
        get_json(&router, &format!("/Items/{library_id}"), token).await;
    assert_eq!(library_status, axum::http::StatusCode::OK);
    assert_eq!(library_json["Id"], library_id.to_string());
    assert_eq!(library_json["ServerId"], server_id.to_string());
    assert_eq!(library_json["IsFolder"], true);
    assert_eq!(library_json["Type"], "CollectionFolder");
    assert_eq!(
        get_status(
            &router,
            &format!("/Items/{library_id}?userId={admin_id}"),
            token
        )
        .await,
        axum::http::StatusCode::FORBIDDEN
    );
    let (views_status, views_json) = get_json(&router, "/UserViews", token).await;
    assert_eq!(views_status, axum::http::StatusCode::OK);
    assert_eq!(views_json["StartIndex"], 0);
    assert!(views_json["Items"].as_array().unwrap().iter().any(|view| {
        view["Id"] == library_id.to_string()
            && view["ServerId"] == server_id.to_string()
            && view["IsFolder"] == true
    }));
    for id in [
        blocked_folder.id,
        blocked_movie.id,
        unrated_folder.id,
        unrated_movie.id,
        hidden_movie.id,
        legacy_file.id,
    ] {
        assert_eq!(
            get_status(&router, &format!("/Items/{id}"), token).await,
            axum::http::StatusCode::NOT_FOUND
        );
    }
    assert_eq!(
        get_status(
            &router,
            &format!("/Items?ParentId={}", blocked_folder.id),
            token
        )
        .await,
        axum::http::StatusCode::NOT_FOUND
    );
    let (browse_status, browse_json) = get_json(
        &router,
        &format!("/Items?ParentId={library_id}&Limit=100"),
        token,
    )
    .await;
    assert_eq!(browse_status, axum::http::StatusCode::OK);
    assert_eq!(browse_json["TotalRecordCount"], 1);
    assert_eq!(browse_json["Items"].as_array().unwrap().len(), 1);
    assert_eq!(browse_json["Items"][0]["Id"], allowed_folder.id.to_string());
    assert_eq!(browse_json["Items"][0]["ServerId"], server_id.to_string());

    let (count_status, counts_json) = get_json(&router, "/Items/Counts", token).await;
    assert_eq!(count_status, axum::http::StatusCode::OK);
    assert_eq!(counts_json["MovieCount"], 1);
    for id in [hidden_movie.id, legacy_file.id] {
        assert_eq!(
            get_status(&router, &format!("/Items/{id}"), admin_token).await,
            axum::http::StatusCode::NOT_FOUND
        );
    }
    let (admin_count_status, admin_counts_json) =
        get_json(&router, "/Items/Counts", admin_token).await;
    assert_eq!(admin_count_status, axum::http::StatusCode::OK);
    assert_eq!(admin_counts_json["MovieCount"], 5);
    let (latest_status, latest_json) = get_json(
        &router,
        "/Items/Latest?IncludeItemTypes=Movie&Limit=10",
        token,
    )
    .await;
    assert_eq!(latest_status, axum::http::StatusCode::OK);
    assert!(latest_json.is_array());
    assert!(
        latest_json
            .as_array()
            .unwrap()
            .iter()
            .any(|item| item["Id"] == allowed_movie.id.to_string())
    );

    sqlx::query("UPDATE items SET runtime_ticks=1000 WHERE id=$1")
        .bind(allowed_movie.id)
        .execute(&pool)
        .await
        .unwrap();
    let (save_status, saved_data) = post_json(
        &router,
        &format!("/UserItems/{}/UserData", allowed_movie.id),
        token,
        serde_json::json!({
            "ItemId": allowed_movie.id,
            "Played": false,
            "PlaybackPositionTicks": 400
        }),
    )
    .await;
    assert_eq!(save_status, axum::http::StatusCode::OK);
    assert_eq!(saved_data["ItemId"], allowed_movie.id.to_string());
    assert_eq!(saved_data["PlaybackPositionTicks"], 400);
    assert_eq!(saved_data["PlayedPercentage"].as_f64(), Some(40.0));
    let (resume_status, resume_json) = get_json(&router, "/UserItems/Resume?Limit=10", token).await;
    assert_eq!(resume_status, axum::http::StatusCode::OK);
    assert_eq!(resume_json["TotalRecordCount"], 1);
    assert_eq!(resume_json["Items"][0]["Id"], allowed_movie.id.to_string());

    let mut category_items = Vec::new();
    for (name, kind, path) in [
        ("song.mp3", "Audio", "/media/Allowed/song.mp3"),
        ("novel.epub", "Book", "/media/Allowed/novel.epub"),
        ("picture.jpg", "Photo", "/media/Allowed/picture.jpg"),
        ("hidden.mp3", "Audio", "/media/Allowed/.hidden.mp3"),
    ] {
        let item = insert_item(
            &pool,
            library_id,
            Some(allowed_folder.id),
            name,
            kind,
            path,
            Some(40),
        )
        .await;
        sqlx::query("INSERT INTO user_item_data(user_id,item_id,playback_position_ticks,played,last_played_at) VALUES ($1,$2,400,FALSE,now())")
            .bind(user_id).bind(item.id).execute(&pool).await.unwrap();
        category_items.push(item);
    }
    // A saved position predating a policy change must not reveal a restricted movie.
    sqlx::query("INSERT INTO user_item_data(user_id,item_id,playback_position_ticks,played,last_played_at) VALUES ($1,$2,400,FALSE,now())")
        .bind(user_id).bind(blocked_movie.id).execute(&pool).await.unwrap();

    for (category, expected_id) in [
        ("Video", allowed_movie.id),
        ("Audio", category_items[0].id),
        ("Book", category_items[1].id),
        ("Photo", category_items[2].id),
    ] {
        let (status, result) = get_json(
            &router,
            &format!("/UserItems/Resume?MediaTypes={category}&Limit=1"),
            token,
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(result["TotalRecordCount"], 1);
        assert_eq!(result["Items"].as_array().unwrap().len(), 1);
        assert_eq!(result["Items"][0]["Id"], expected_id.to_string());
        assert_eq!(result["Items"][0]["MediaType"], category);
    }
    let (status, result) = get_json(
        &router,
        "/UserItems/Resume?mediaTypes=Audio,Video&Limit=1&StartIndex=1",
        token,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(result["TotalRecordCount"], 2);
    assert_eq!(result["StartIndex"], 1);
    assert_eq!(result["Items"].as_array().unwrap().len(), 1);
    for endpoint in ["/UserItems/Resume", "/Items?Recursive=true"] {
        let separator = if endpoint.contains('?') { '&' } else { '?' };
        let (status, result) = get_json(
            &router,
            &format!("{endpoint}{separator}MediaTypes=Audio&IncludeItemTypes=Movie"),
            token,
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(result["TotalRecordCount"], 0);
        assert!(result["Items"].as_array().unwrap().is_empty());
    }
    let (status, result) = get_json(&router, "/Items/Latest?MediaTypes=Audio&Limit=1", token).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(result.as_array().unwrap().len(), 1);
    assert_eq!(result[0]["Id"], category_items[0].id.to_string());
    let (status, result) =
        get_json(&router, "/Items?MediaTypes=Unknown&Recursive=true", token).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(result["TotalRecordCount"], 1);
    assert_eq!(result["Items"][0]["Id"], allowed_folder.id.to_string());
    assert!(
        result["Items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["MediaType"].is_null())
    );
    for endpoint in ["/UserItems/Resume", "/Items", "/Items/Latest"] {
        assert_eq!(
            get_status(&router, &format!("{endpoint}?MediaTypes=Invalid"), token).await,
            axum::http::StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        get_status(
            &router,
            &format!("/UserItems/Resume?MediaTypes=Audio&UserId={admin_id}"),
            token
        )
        .await,
        axum::http::StatusCode::FORBIDDEN
    );
    for item in category_items {
        sqlx::query("DELETE FROM items WHERE id=$1")
            .bind(item.id)
            .execute(&pool)
            .await
            .unwrap();
    }

    let (favorite_status, favorite_data) = post_json(
        &router,
        &format!("/UserFavoriteItems/{}", allowed_movie.id),
        token,
        serde_json::json!({}),
    )
    .await;
    assert_eq!(favorite_status, axum::http::StatusCode::OK);
    assert_eq!(favorite_data["IsFavorite"], true);
    let (played_status, played_data) = post_json(
        &router,
        &format!("/UserPlayedItems/{}", allowed_movie.id),
        token,
        serde_json::json!({}),
    )
    .await;
    assert_eq!(played_status, axum::http::StatusCode::OK);
    assert_eq!(played_data["Played"], true);
    assert_eq!(played_data["PlaybackPositionTicks"], 0);
    let (unplayed_status, unplayed_data) = delete_json(
        &router,
        &format!("/UserPlayedItems/{}", allowed_movie.id),
        token,
    )
    .await;
    assert_eq!(unplayed_status, axum::http::StatusCode::OK);
    assert_eq!(unplayed_data["Played"], false);
    let (favorite_get_status, favorite_get_json) =
        get_json(&router, "/Items?Filters=IsFavorite&Limit=10", token).await;
    assert_eq!(favorite_get_status, axum::http::StatusCode::OK);
    assert_eq!(favorite_get_json["TotalRecordCount"], 1);
    assert_eq!(
        favorite_get_json["Items"][0]["Id"],
        allowed_movie.id.to_string()
    );

    sqlx::query("DELETE FROM items WHERE id=$1")
        .bind(allowed_movie.id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("DELETE FROM items WHERE id=$1")
        .bind(catalog_rated_movie.id)
        .execute(&pool)
        .await
        .unwrap();
    assert!(
        !db::library_visible_to_user(&pool, &restricted, library_id)
            .await
            .unwrap()
    );
    assert!(
        db::list_libraries(&pool, &restricted)
            .await
            .unwrap()
            .is_empty()
    );
    let (views_status, views_json) = get_json(&router, "/UserViews", token).await;
    assert_eq!(views_status, axum::http::StatusCode::OK);
    assert_eq!(views_json["TotalRecordCount"], 0);
    assert!(views_json["Items"].as_array().unwrap().is_empty());
    assert_eq!(
        get_status(&router, &format!("/Items/{library_id}"), token).await,
        axum::http::StatusCode::NOT_FOUND
    );

    let tv_library_id = Uuid::new_v4();
    db::insert_library(
        &pool,
        run_id,
        tv_library_id,
        "TV navigation test",
        "tvshows",
        &[PathBuf::from("/tv")],
        true,
    )
    .await
    .unwrap();
    let series = insert_item(
        &pool,
        tv_library_id,
        None,
        "Example Series",
        "Series",
        "/tv/Example Series",
        None,
    )
    .await;
    let season = insert_item(
        &pool,
        tv_library_id,
        Some(series.id),
        "Season 1",
        "Season",
        "/tv/Example Series/Season 1",
        None,
    )
    .await;
    let episode = insert_item(
        &pool,
        tv_library_id,
        Some(season.id),
        "S01E02 - Pilot.mkv",
        "Episode",
        "/tv/Example Series/Season 1/S01E02 - Pilot.mkv",
        None,
    )
    .await;
    let (seasons_status, seasons_json) = get_json(
        &router,
        &format!("/Shows/{}/Seasons", series.id),
        admin_token,
    )
    .await;
    assert_eq!(seasons_status, axum::http::StatusCode::OK);
    assert_eq!(seasons_json["Items"][0]["Id"], season.id.to_string());
    let (episodes_status, episodes_json) = get_json(
        &router,
        &format!("/Shows/{}/Episodes?SeasonId={}", series.id, season.id),
        admin_token,
    )
    .await;
    assert_eq!(episodes_status, axum::http::StatusCode::OK);
    assert_eq!(episodes_json["Items"][0]["Id"], episode.id.to_string());
    assert_eq!(episodes_json["Items"][0]["SeriesId"], series.id.to_string());
    assert_eq!(episodes_json["Items"][0]["SeasonId"], season.id.to_string());
    assert_eq!(episodes_json["Items"][0]["IndexNumber"], 2);
    assert_eq!(episodes_json["Items"][0]["ParentIndexNumber"], 1);

    let music_library_id = Uuid::new_v4();
    db::insert_library(
        &pool,
        run_id,
        music_library_id,
        "Music navigation test",
        "music",
        &[PathBuf::from("/music")],
        true,
    )
    .await
    .unwrap();
    let artist = insert_item(
        &pool,
        music_library_id,
        None,
        "Example Artist",
        "MusicArtist",
        "/music/Example Artist",
        None,
    )
    .await;
    let album = insert_item(
        &pool,
        music_library_id,
        Some(artist.id),
        "Example Album",
        "MusicAlbum",
        "/music/Example Artist/Example Album",
        None,
    )
    .await;
    let song = insert_item(
        &pool,
        music_library_id,
        Some(album.id),
        "01 - Opening.flac",
        "Audio",
        "/music/Example Artist/Example Album/01 - Opening.flac",
        None,
    )
    .await;
    let (artists_status, artists_json) = get_json(
        &router,
        &format!("/Artists?ParentId={music_library_id}"),
        admin_token,
    )
    .await;
    assert_eq!(artists_status, axum::http::StatusCode::OK);
    assert_eq!(artists_json["Items"][0]["Id"], artist.id.to_string());
    let (persons_status, persons_json) = get_json(
        &router,
        &format!("/Persons?ParentId={music_library_id}&PersonTypes=Actor%2CArtist&Limit=10"),
        admin_token,
    )
    .await;
    assert_eq!(persons_status, axum::http::StatusCode::OK);
    assert_eq!(persons_json["TotalRecordCount"], 1);
    assert_eq!(persons_json["Items"][0]["Id"], artist.id.to_string());
    assert_eq!(persons_json["Items"][0]["Type"], "Person");
    let (repeated_person_types_status, repeated_person_types_json) = get_json(
        &router,
        &format!(
            "/Persons?ParentId={music_library_id}&PersonTypes=Actor&PersonTypes=AlbumArtist&Limit=10"
        ),
        admin_token,
    )
    .await;
    assert_eq!(repeated_person_types_status, axum::http::StatusCode::OK);
    assert_eq!(repeated_person_types_json["TotalRecordCount"], 1);
    let (person_by_name_status, person_by_name_json) =
        get_json(&router, "/Persons/Example%20Artist", admin_token).await;
    assert_eq!(person_by_name_status, axum::http::StatusCode::OK);
    assert_eq!(person_by_name_json["Id"], artist.id.to_string());
    assert_eq!(person_by_name_json["Type"], "Person");
    let (appears_status, appears_json) = get_json(
        &router,
        &format!(
            "/Persons?AppearsInItemId={}&PersonTypes=AlbumArtist",
            song.id
        ),
        admin_token,
    )
    .await;
    assert_eq!(appears_status, axum::http::StatusCode::OK);
    assert_eq!(appears_json["TotalRecordCount"], 1);
    assert_eq!(appears_json["Items"][0]["Id"], artist.id.to_string());
    let (appears_page_status, appears_page_json) = get_json(
        &router,
        &format!(
            "/Persons?AppearsInItemId={}&PersonTypes=Artist&StartIndex=1&Limit=1",
            song.id
        ),
        admin_token,
    )
    .await;
    assert_eq!(appears_page_status, axum::http::StatusCode::OK);
    assert_eq!(appears_page_json["TotalRecordCount"], 1);
    assert_eq!(appears_page_json["StartIndex"], 1);
    assert!(appears_page_json["Items"].as_array().unwrap().is_empty());
    let (actor_status, actor_json) = get_json(
        &router,
        &format!("/Persons?ParentId={music_library_id}&PersonTypes=Actor"),
        admin_token,
    )
    .await;
    assert_eq!(actor_status, axum::http::StatusCode::OK);
    assert_eq!(actor_json["TotalRecordCount"], 0);
    assert!(actor_json["Items"].as_array().unwrap().is_empty());
    assert_eq!(
        get_status(
            &router,
            &format!("/Persons?ParentId={music_library_id}"),
            token,
        )
        .await,
        axum::http::StatusCode::NOT_FOUND
    );
    assert_eq!(
        get_status(&router, "/Persons/Example%20Artist", token).await,
        axum::http::StatusCode::NOT_FOUND
    );

    // Exact name resolution must not depend on whether that item fits into the
    // bounded full-text substring page ahead of many prefix matches.
    for index in 0..120 {
        let name = format!("A Zeta Artist Target {index:03}");
        let path = format!("/music/{name}");
        insert_item(
            &pool,
            music_library_id,
            None,
            &name,
            "MusicArtist",
            &path,
            None,
        )
        .await;
    }
    let exact_artist = insert_item(
        &pool,
        music_library_id,
        None,
        "Zeta Artist Target",
        "MusicArtist",
        "/music/Zeta Artist Target",
        None,
    )
    .await;
    let (late_person_status, late_person_json) =
        get_json(&router, "/Persons/Zeta%20Artist%20Target", admin_token).await;
    assert_eq!(late_person_status, axum::http::StatusCode::OK);
    assert_eq!(late_person_json["Id"], exact_artist.id.to_string());
    assert_eq!(late_person_json["Type"], "Person");
    let stored_title_artist = insert_item(
        &pool,
        music_library_id,
        None,
        "Stored Artist Name",
        "MusicArtist",
        "/music/Stored Artist Name",
        None,
    )
    .await;
    sqlx::query("INSERT INTO item_metadata(item_id,provider_key,title) VALUES ($1,'local-nfo','Visible Artist Name')")
        .bind(stored_title_artist.id)
        .execute(&pool)
        .await
        .unwrap();
    let (visible_person_status, visible_person_json) =
        get_json(&router, "/Persons/Visible%20Artist%20Name", admin_token).await;
    assert_eq!(visible_person_status, axum::http::StatusCode::OK);
    assert_eq!(
        visible_person_json["Id"],
        stored_title_artist.id.to_string()
    );
    assert_eq!(visible_person_json["Name"], "Visible Artist Name");
    assert_eq!(
        get_status(&router, "/Persons/Stored%20Artist%20Name", admin_token).await,
        axum::http::StatusCode::NOT_FOUND
    );
    let (song_status, song_json) =
        get_json(&router, &format!("/Items/{}", song.id), admin_token).await;
    assert_eq!(song_status, axum::http::StatusCode::OK);
    assert_eq!(song_json["AlbumId"], album.id.to_string());
    assert_eq!(song_json["Album"], "Example Album");
    assert_eq!(song_json["ArtistItems"][0]["Id"], artist.id.to_string());
    assert_eq!(song_json["IndexNumber"], 1);

    verify_folder_filters(&router, &pool, run_id, user_id, token).await;
    verify_administrator_policy(&router, &pool, run_id, admin_id, admin_token).await;
    drop(router);
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .execute(&admin_pool)
        .await
        .unwrap();
    admin_pool.close().await;
}

async fn verify_administrator_policy(
    router: &axum::Router,
    pool: &PgPool,
    run_id: Uuid,
    admin_id: Uuid,
    admin_token: &str,
) {
    use axum::http::StatusCode;
    use serde_json::json;

    let target_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,username,username_norm,password_hash,enable_remote_access) VALUES ($1,'policy-role-target','policy-role-target','unused',TRUE)")
        .bind(target_id).execute(pool).await.unwrap();
    let target_token = "policy-role-target-token";
    create_token(pool, run_id, target_id, target_token).await;
    let target_uri = format!("/Users/{target_id}/Policy");
    let admin_uri = format!("/Users/{admin_id}/Policy");
    let (status, _) = post_json(
        router,
        &target_uri,
        target_token,
        json!({"IsAdministrator":true}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::FORBIDDEN,
        "users cannot promote themselves"
    );
    assert!(
        !db::get_user(pool, target_id)
            .await
            .unwrap()
            .unwrap()
            .is_admin
    );

    for body in [json!({"IsAdministrator":false}), json!({"IsDisabled":true})] {
        let (status, _) = post_json(router, &admin_uri, admin_token, body).await;
        assert_eq!(
            status,
            StatusCode::BAD_REQUEST,
            "the last enabled administrator is retained"
        );
        let retained = db::get_user(pool, admin_id).await.unwrap().unwrap();
        assert!(retained.is_admin && !retained.disabled);
    }
    for body in [
        json!({"IsAdministrator":"true"}),
        json!({"IsAdministrator":true,"Name":"cannot-rename"}),
        json!({"IsAdministrator":true,"Password":"cannot-change"}),
    ] {
        let (status, _) = post_json(router, &target_uri, admin_token, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let retained = db::get_user(pool, target_id).await.unwrap().unwrap();
        assert!(!retained.is_admin);
        assert_eq!(retained.username, "policy-role-target");
    }
    let (status, body) = post_json(
        router,
        &target_uri,
        admin_token,
        json!({"IsAdministrator":true}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "administrator is a documented policy field: {body}"
    );
    assert!(body.is_null());
    let (status, policy) = get_json(router, &target_uri, target_token).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(policy["IsAdministrator"], true);
    assert!(
        db::get_user(pool, target_id)
            .await
            .unwrap()
            .unwrap()
            .is_admin
    );
    assert_eq!(
        get_status(router, "/Users", target_token).await,
        StatusCode::OK
    );

    let (status, _) = post_json(
        router,
        &format!("/Users/{}/Policy", Uuid::new_v4()),
        admin_token,
        json!({"IsAdministrator":true}),
    )
    .await;
    assert_eq!(status, StatusCode::NOT_FOUND);
    let (status, body) = post_json(
        router,
        &admin_uri,
        admin_token,
        json!({"IsAdministrator":false}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "another enabled administrator remains: {body}"
    );
    assert!(body.is_null());
    assert_eq!(
        get_status(router, "/Users", admin_token).await,
        StatusCode::FORBIDDEN,
        "existing sessions lose administrative access immediately"
    );
    for body in [json!({"IsAdministrator":false}), json!({"IsDisabled":true})] {
        let (status, _) = post_json(router, &target_uri, target_token, body).await;
        assert_eq!(status, StatusCode::BAD_REQUEST);
        let retained = db::get_user(pool, target_id).await.unwrap().unwrap();
        assert!(retained.is_admin && !retained.disabled);
    }
    let (status, body) = post_json(
        router,
        &admin_uri,
        target_token,
        json!({"IsAdministrator":true}),
    )
    .await;
    assert_eq!(
        status,
        StatusCode::NO_CONTENT,
        "the remaining administrator can restore a role: {body}"
    );
    assert!(body.is_null());
    assert_eq!(
        get_status(router, "/Users", admin_token).await,
        StatusCode::OK
    );
    let (status, body) = post_json(
        router,
        &target_uri,
        admin_token,
        json!({"IsAdministrator":false,"EnableAllFolders":false}),
    )
    .await;
    assert_eq!(status, StatusCode::NO_CONTENT, "{body}");
    assert!(body.is_null());
    let retained = db::get_user(pool, target_id).await.unwrap().unwrap();
    assert!(!retained.is_admin && retained.restrict_libraries);
    assert_eq!(
        get_status(router, "/Users", target_token).await,
        StatusCode::FORBIDDEN
    );
    let (status, policy) = get_json(router, &target_uri, target_token).await;
    assert_eq!(status, StatusCode::OK);
    assert_eq!(policy["IsAdministrator"], false);
    assert_eq!(policy["EnableAllFolders"], false);
}

async fn verify_folder_filters(
    router: &axum::Router,
    pool: &PgPool,
    run_id: Uuid,
    user_id: Uuid,
    token: &str,
) {
    let library_id = Uuid::new_v4();
    db::insert_library(
        pool,
        run_id,
        library_id,
        "Folder filter fixture",
        "photos",
        &[PathBuf::from("/photos")],
        true,
    )
    .await
    .unwrap();
    sqlx::query("INSERT INTO user_library_access(user_id,library_id) VALUES ($1,$2)")
        .bind(user_id)
        .bind(library_id)
        .execute(pool)
        .await
        .unwrap();
    let mut folders = Vec::new();
    for kind in [
        "Folder",
        "CollectionFolder",
        "Season",
        "BoxSet",
        "Series",
        "MusicArtist",
        "MusicAlbum",
    ] {
        let folder = insert_item(
            pool,
            library_id,
            None,
            &format!("00 {kind}"),
            kind,
            &format!("/photos/{kind}"),
            None,
        )
        .await;
        insert_item(
            pool,
            library_id,
            Some(folder.id),
            "Nested photo",
            "Photo",
            &format!("/photos/{kind}/nested.png"),
            None,
        )
        .await;
        folders.push(folder.id.to_string());
    }
    let first = insert_item(
        pool,
        library_id,
        None,
        "01 Photo",
        "Photo",
        "/photos/first.png",
        None,
    )
    .await;
    let second = insert_item(
        pool,
        library_id,
        None,
        "02 Photo",
        "Photo",
        "/photos/second.png",
        None,
    )
    .await;
    let video = insert_item(
        pool,
        library_id,
        None,
        "03 Video",
        "Movie",
        "/photos/video.mp4",
        Some(40),
    )
    .await;
    set_local_policy_rating(pool, video.id, "PG", 40).await;
    let blocked = insert_item(
        pool,
        library_id,
        None,
        "00 Blocked",
        "Photo",
        "/photos/blocked.png",
        Some(100),
    )
    .await;
    set_local_policy_rating(pool, blocked.id, "R", 100).await;
    let hidden = insert_item(
        pool,
        library_id,
        None,
        "00 Hidden",
        "Photo",
        "/photos/.hidden.png",
        None,
    )
    .await;
    let blocked_folder = insert_item(
        pool,
        library_id,
        None,
        "00 Blocked folder",
        "Folder",
        "/photos/blocked",
        None,
    )
    .await;
    let blocked_child = insert_item(
        pool,
        library_id,
        Some(blocked_folder.id),
        "Blocked child",
        "Photo",
        "/photos/blocked/child.png",
        Some(100),
    )
    .await;
    set_local_policy_rating(pool, blocked_child.id, "R", 100).await;

    // Match the official photo viewer's request, including filters before paging.
    let (status, page) = get_json(router, &format!("/Users/{user_id}/Items?ParentId={library_id}&Filters=IsNotFolder&Recursive=false&SortBy=SortName&MediaTypes=Photo,Video&SortOrder=Ascending&Fields=Chapters,MediaSources,Trickplay&ExcludeLocationTypes=Virtual&CollapseBoxSetItems=false&StartIndex=1&Limit=1"), token).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(page["TotalRecordCount"], 3);
    assert_eq!(page["StartIndex"], 1);
    assert_eq!(page["Items"].as_array().unwrap().len(), 1);
    assert_eq!(page["Items"][0]["Id"], second.id.to_string());
    assert_eq!(page["Items"][0]["IsFolder"], false);
    for selection in [
        "Filters=IsNotFolder",
        "IsFolder=false",
        "isFolder=false&filters=isnotfolder",
    ] {
        let (status, result) = get_json(router, &format!("/Items?ParentId={library_id}&{selection}&MediaTypes=Photo&Recursive=false&SortBy=SortName"), token).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(result["TotalRecordCount"], 2);
        assert_eq!(result["Items"][0]["Id"], first.id.to_string());
        assert_eq!(result["Items"][1]["Id"], second.id.to_string());
    }
    let (status, result) = get_json(router, &format!("/Items?ParentId={library_id}&Filters=IsNotFolder&MediaTypes=Photo&Recursive=true&EnableTotalRecordCount=false"), token).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(result.get("TotalRecordCount").is_none());
    assert_eq!(result["Items"].as_array().unwrap().len(), 9);
    assert!(
        result["Items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|item| item["IsFolder"] == false)
    );
    for selection in ["Filters=IsFolder", "IsFolder=true&Filters=isfolder"] {
        let (status, result) = get_json(
            router,
            &format!("/Items?ParentId={library_id}&{selection}&Recursive=true"),
            token,
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(result["TotalRecordCount"], 7);
        let mut actual = result["Items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|item| {
                assert_eq!(item["IsFolder"], true);
                item["Id"].as_str().unwrap().to_owned()
            })
            .collect::<Vec<_>>();
        actual.sort();
        folders.sort();
        assert_eq!(actual, folders);
    }
    let (status, result) = get_json(
        router,
        &format!("/Items?ParentId={library_id}&Filters=IsFolder&MediaTypes=Photo&Recursive=true"),
        token,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(result["TotalRecordCount"], 0);
    assert!(result["Items"].as_array().unwrap().is_empty());
    for selection in [
        "Filters=IsFolder,IsNotFolder",
        "Filters=IsNotFolder,IsFolder",
        "IsFolder=true&Filters=IsNotFolder",
        "isFolder=false&filters=isfolder",
        "Filters=UnknownFilter",
        "IsFolder=invalid",
    ] {
        assert_eq!(
            get_status(
                router,
                &format!("/Items?ParentId={library_id}&{selection}"),
                token
            )
            .await,
            axum::http::StatusCode::BAD_REQUEST
        );
    }
    let private_library = Uuid::new_v4();
    db::insert_library(
        pool,
        run_id,
        private_library,
        "Private photos",
        "photos",
        &[PathBuf::from("/private-photos")],
        true,
    )
    .await
    .unwrap();
    let private_photo = insert_item(
        pool,
        private_library,
        None,
        "Private photo",
        "Photo",
        "/private-photos/private.png",
        None,
    )
    .await;
    let ids = format!(
        "{},{},{},{},{},{},{}",
        second.id,
        hidden.id,
        blocked.id,
        private_photo.id,
        Uuid::new_v4(),
        first.id,
        first.id
    );
    for endpoint in ["/Items".to_owned(), format!("/Users/{user_id}/Items")] {
        let (status, result) = get_json(router, &format!("{endpoint}?Ids={ids}&Limit=300&Fields=Chapters,MediaSources,Trickplay&ExcludeLocationTypes=Virtual&EnableTotalRecordCount=true&CollapseBoxSetItems=false"), token).await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(result["TotalRecordCount"], 2);
        assert_eq!(result["Items"].as_array().unwrap().len(), 2);
        assert_eq!(result["Items"][0]["Id"], second.id.to_string());
        assert_eq!(result["Items"][1]["Id"], first.id.to_string());
        let (status, page) = get_json(
            router,
            &format!("{endpoint}?ids={ids}&StartIndex=1&Limit=1"),
            token,
        )
        .await;
        assert_eq!(status, axum::http::StatusCode::OK);
        assert_eq!(page["TotalRecordCount"], 2);
        assert_eq!(page["Items"].as_array().unwrap().len(), 1);
        assert_eq!(page["Items"][0]["Id"], first.id.to_string());
    }
    let (status, sorted) = get_json(
        router,
        &format!(
            "/Items?Ids={ids}&ParentId={library_id}&Recursive=true&MediaTypes=Photo&SortBy=SortName"
        ),
        token,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(sorted["TotalRecordCount"], 2);
    assert_eq!(sorted["Items"][0]["Id"], first.id.to_string());
    assert_eq!(sorted["Items"][1]["Id"], second.id.to_string());
    let (status, omitted_count) = get_json(
        router,
        &format!(
            "/Items?Ids={}&MediaTypes=Video&EnableTotalRecordCount=false",
            first.id
        ),
        token,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert!(omitted_count.get("TotalRecordCount").is_none());
    assert!(omitted_count["Items"].as_array().unwrap().is_empty());
    for selection in [
        String::new(),
        "invalid".to_owned(),
        format!("{},invalid", first.id),
        format!("{},", first.id),
        vec![first.id.to_string(); 1001].join(","),
    ] {
        assert_eq!(
            get_status(router, &format!("/Items?Ids={selection}"), token).await,
            axum::http::StatusCode::BAD_REQUEST
        );
    }
    assert_eq!(
        get_status(
            router,
            &format!("/Items?ParentId={private_library}&Filters=IsNotFolder"),
            token
        )
        .await,
        axum::http::StatusCode::NOT_FOUND
    );
    sqlx::query("UPDATE libraries SET enabled=FALSE WHERE id=$1")
        .bind(library_id)
        .execute(pool)
        .await
        .unwrap();
    let (status, disabled) = get_json(router, &format!("/Items?Ids={ids}"), token).await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(disabled["TotalRecordCount"], 0);
    assert!(disabled["Items"].as_array().unwrap().is_empty());
    assert_eq!(
        get_status(
            router,
            &format!("/Items?ParentId={library_id}&Filters=IsNotFolder"),
            token
        )
        .await,
        axum::http::StatusCode::NOT_FOUND
    );
    let (status, result) = get_json(
        router,
        "/Items?Filters=IsNotFolder&MediaTypes=Photo&Recursive=true",
        token,
    )
    .await;
    assert_eq!(status, axum::http::StatusCode::OK);
    assert_eq!(result["TotalRecordCount"], 0);
}

async fn insert_item(
    pool: &PgPool,
    library_id: Uuid,
    parent_id: Option<Uuid>,
    name: &str,
    item_type: &str,
    path: &str,
    rating: Option<i16>,
) -> ItemRecord {
    let id = Uuid::new_v4();
    sqlx::query("INSERT INTO items(id,library_id,parent_id,name,sort_name,item_type,path,path_hash,rating) VALUES ($1,$2,$3,$4,$4,$5,$6,$7,$8)")
        .bind(id)
        .bind(library_id)
        .bind(parent_id)
        .bind(name)
        .bind(item_type)
        .bind(path)
        .bind(db::path_hash(path))
        .bind(rating)
        .execute(pool)
        .await
        .unwrap();
    db::get_item(pool, id).await.unwrap().unwrap()
}

async fn set_local_policy_rating(pool: &PgPool, item_id: Uuid, label: &str, value: i16) {
    sqlx::query("INSERT INTO item_metadata(item_id,provider_key,content_rating,policy_rating_scale,policy_rating_value) VALUES ($1,'local-nfo',$2,'US-MPAA-v1',$3)")
        .bind(item_id)
        .bind(label)
        .bind(value)
        .execute(pool)
        .await
        .unwrap();
}

async fn get_status(router: &axum::Router, uri: &str, token: &str) -> axum::http::StatusCode {
    router
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .header("X-Emby-Token", token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap()
        .status()
}

async fn get_json(
    router: &axum::Router,
    uri: &str,
    token: &str,
) -> (axum::http::StatusCode, serde_json::Value) {
    let response = router
        .clone()
        .oneshot(
            Request::builder()
                .uri(uri)
                .header("X-Emby-Token", token)
                .body(Body::empty())
                .unwrap(),
        )
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let body_len = body.len();
    let value = serde_json::from_slice(&body).unwrap_or_else(|error| {
        panic!("GET {uri} returned {status} with a {body_len}-byte body; JSON parse error: {error}")
    });
    (status, value)
}

async fn post_json(
    router: &axum::Router,
    uri: &str,
    token: &str,
    value: serde_json::Value,
) -> (axum::http::StatusCode, serde_json::Value) {
    request_json(router, axum::http::Method::POST, uri, token, Some(value)).await
}

async fn delete_json(
    router: &axum::Router,
    uri: &str,
    token: &str,
) -> (axum::http::StatusCode, serde_json::Value) {
    request_json(router, axum::http::Method::DELETE, uri, token, None).await
}

async fn request_json(
    router: &axum::Router,
    method: axum::http::Method,
    uri: &str,
    token: &str,
    value: Option<serde_json::Value>,
) -> (axum::http::StatusCode, serde_json::Value) {
    let mut builder = Request::builder()
        .method(method)
        .uri(uri)
        .header("X-Emby-Token", token);
    let body = if let Some(value) = value {
        builder = builder.header("Content-Type", "application/json");
        Body::from(serde_json::to_vec(&value).unwrap())
    } else {
        Body::empty()
    };
    let response = router
        .clone()
        .oneshot(builder.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let body = response.into_body().collect().await.unwrap().to_bytes();
    let json = if body.is_empty() {
        serde_json::Value::Null
    } else {
        serde_json::from_slice(&body).unwrap()
    };
    (status, json)
}

async fn create_token(pool: &PgPool, run_id: Uuid, user_id: Uuid, token: &str) {
    db::create_auth_token(
        pool,
        run_id,
        db::NewAuthToken {
            token_id: Uuid::new_v4(),
            user_id,
            token_hash: auth::token_digest(token),
            expires_at: Utc::now() + ChronoDuration::hours(1),
            client: "test".to_owned(),
            device_name: "test".to_owned(),
            device_id: "policy-test".to_owned(),
        },
    )
    .await
    .unwrap();
}

fn test_config(database_url: String) -> Config {
    Config {
        bind: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        public_base_url: None,
        database_url,
        server_name: "Test".to_owned(),
        web_root: env::temp_dir(),
        data_dir: env::temp_dir(),
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
    }
}
