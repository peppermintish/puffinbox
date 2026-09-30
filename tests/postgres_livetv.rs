use std::{env, fs, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use axum::{
    body::Body,
    http::{Request, StatusCode},
};
use chrono::{DateTime, Duration as ChronoDuration, SecondsFormat, Utc};
use ipnet::IpNet;
use puffinbox::{AppState, Config, api, auth, db};
use serde_json::{Value, json};
use sqlx::{Row, postgres::PgPoolOptions, types::Json};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    net::TcpListener,
    task::JoinHandle,
    time::{sleep, timeout},
};
use tower::ServiceExt;
use uuid::Uuid;

mod common;

type DeletedSourceRow = (
    Option<String>,
    Option<String>,
    Option<String>,
    bool,
    Option<DateTime<Utc>>,
);

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via PUFFINBOX_TEST_DATABASE_URL"]
async fn pinned_iptv_refresh_applies_guide_parental_policy_and_timer_filters() {
    let database_url = env::var("PUFFINBOX_TEST_DATABASE_URL")
        .expect("set PUFFINBOX_TEST_DATABASE_URL to a disposable PostgreSQL database");
    let admin_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url)
        .await
        .unwrap();
    let schema = format!("puffinbox_livetv_test_{}", Uuid::new_v4().simple());
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
    let library_root = env::temp_dir().join(format!("puffinbox-livetv-{}", Uuid::new_v4()));
    fs::create_dir_all(&library_root).unwrap();
    let library_root = fs::canonicalize(library_root).unwrap();
    let library_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO libraries(id,name,collection_type,locations) VALUES($1,'IPTV fixture','mixed',$2)",
    )
    .bind(library_id)
    .bind(Json(vec![library_root.to_string_lossy().into_owned()]))
    .execute(&pool)
    .await
    .unwrap();

    let admin_id = insert_user(&pool, "livetv-admin", true, false, None, &[]).await;
    let viewer_id = insert_user(
        &pool,
        "livetv-viewer",
        false,
        true,
        Some(50),
        &["LiveTvProgram"],
    )
    .await;
    sqlx::query("INSERT INTO user_library_access(user_id,library_id) VALUES($1,$2)")
        .bind(viewer_id)
        .bind(library_id)
        .execute(&pool)
        .await
        .unwrap();

    let now = Utc::now();
    let starts = xmltv_time(now - ChronoDuration::minutes(1));
    let ends = xmltv_time(now + ChronoDuration::minutes(44));
    let repeat_starts = xmltv_time(now + ChronoDuration::hours(1));
    let repeat_ends = xmltv_time(now + ChronoDuration::hours(1) + ChronoDuration::minutes(45));
    let encore_starts = xmltv_time(now + ChronoDuration::hours(2));
    let encore_ends = xmltv_time(now + ChronoDuration::hours(2) + ChronoDuration::minutes(45));
    let playlist = b"#EXTM3U\n#EXTINF:-1 tvg-id=\"adult\",Adult Channel\n/live/adult.ts\n#EXTINF:-1 tvg-id=\"family\",Family Channel\n/live/family.ts\n#EXTINF:-1 tvg-id=\"unrated\",Unrated Channel\n/live/unrated.ts\n".to_vec();
    let refreshed_playlist = b"#EXTM3U\n#EXTINF:-1 tvg-id=\"adult\" tvg-logo=\"/logos/adult.png\",Adult Channel\n/live/adult.ts\n#EXTINF:-1 tvg-id=\"family\" tvg-logo=\"/logos/family.png\",Family Channel\n/live/family.ts\n#EXTINF:-1 tvg-id=\"unrated\" tvg-logo=\"/logos/unrated.png\",Unrated Channel\n/live/unrated.ts\n".to_vec();
    let omitted_channel_playlist = b"#EXTM3U\n#EXTINF:-1 tvg-id=\"adult\" tvg-logo=\"/logos/adult.png\",Adult Channel\n/live/adult.ts\n#EXTINF:-1 tvg-id=\"family\" tvg-logo=\"/logos/family.png\",Family Channel\n/live/family.ts\n".to_vec();
    let guide = format!(
        "<tv>\
         <channel id=\"adult\"><display-name>Adult Channel</display-name></channel>\
         <channel id=\"family\"><display-name>Family Channel</display-name></channel>\
         <channel id=\"unrated\"><display-name>Unrated Channel</display-name></channel>\
         <programme channel=\"adult\" start=\"{starts}\" stop=\"{ends}\"><title>Late News</title><rating system=\"VCHIP\"><value>TV-MA</value></rating></programme>\
         <programme channel=\"family\" start=\"{starts}\" stop=\"{ends}\"><title>Family Hour</title><rating system=\"VCHIP\"><value>TV-Y</value></rating></programme>\
         <programme channel=\"family\" start=\"{repeat_starts}\" stop=\"{repeat_ends}\"><title>Family Hour</title><rating system=\"VCHIP\"><value>TV-Y</value></rating></programme>\
         <programme channel=\"family\" start=\"{encore_starts}\" stop=\"{encore_ends}\"><title>Family Encore</title><rating system=\"VCHIP\"><value>TV-Y</value></rating></programme>\
         <programme channel=\"unrated\" start=\"{starts}\" stop=\"{ends}\"><title>Unknown Rating</title><rating system=\"example\"><value>TV-MA</value></rating></programme>\
         </tv>"
    )
    .into_bytes();
    let (feed_origin, fixture_task) = serve_feeds(
        playlist,
        refreshed_playlist,
        omitted_channel_playlist,
        guide,
    )
    .await;

    let config = Config {
        bind: "127.0.0.1:0".parse::<SocketAddr>().unwrap(),
        public_base_url: None,
        database_url: database_url.clone(),
        server_name: "Live TV test".to_owned(),
        web_root: PathBuf::from("web"),
        data_dir: library_root.join("data"),
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
    assert!(!admin.enable_live_tv_access);
    assert!(!admin.enable_live_tv_management);
    let viewer = db::get_user(&pool, viewer_id).await.unwrap().unwrap();
    let admin_token =
        auth::issue_token(&state, &admin, "livetv-test", "test-client", "livetv-admin")
            .await
            .unwrap()
            .token;
    let viewer_token = auth::issue_token(
        &state,
        &viewer,
        "livetv-test",
        "test-client",
        "livetv-viewer",
    )
    .await
    .unwrap()
    .token;
    let router = api::router(state.clone());

    let (viewer_policy_status, viewer_policy) = call_json(
        &router,
        "POST",
        &format!("/Users/{viewer_id}/Policy"),
        &admin_token,
        Some(json!({
            "EnableLiveTvAccess": true,
            "EnableLiveTvManagement": true
        })),
    )
    .await;
    assert_eq!(viewer_policy_status, StatusCode::OK, "{viewer_policy}");
    assert_eq!(viewer_policy["EnableLiveTvAccess"], true);
    assert_eq!(viewer_policy["EnableLiveTvManagement"], true);
    let (viewer_policy_get_status, viewer_policy_get) = call_json(
        &router,
        "GET",
        &format!("/Users/{viewer_id}/Policy"),
        &admin_token,
        None,
    )
    .await;
    assert_eq!(
        viewer_policy_get_status,
        StatusCode::OK,
        "{viewer_policy_get}"
    );
    assert_eq!(viewer_policy_get["EnableLiveTvAccess"], true);
    assert_eq!(viewer_policy_get["EnableLiveTvManagement"], true);

    let no_tv_access_id = insert_user(&pool, "livetv-no-access", false, false, None, &[]).await;
    let no_tv_access = db::get_user(&pool, no_tv_access_id).await.unwrap().unwrap();
    assert!(!no_tv_access.enable_live_tv_access);
    assert!(!no_tv_access.enable_live_tv_management);
    let no_tv_access_token = auth::issue_token(
        &state,
        &no_tv_access,
        "livetv-test",
        "test-client",
        "livetv-no-access",
    )
    .await
    .unwrap()
    .token;

    let view_only_id = insert_user(&pool, "livetv-view-only", false, false, None, &[]).await;
    let (view_only_policy_status, view_only_policy) = call_json(
        &router,
        "POST",
        &format!("/Users/{view_only_id}/Policy"),
        &admin_token,
        Some(json!({
            "EnableLiveTvAccess": true,
            "EnableLiveTvManagement": false
        })),
    )
    .await;
    assert_eq!(
        view_only_policy_status,
        StatusCode::OK,
        "{view_only_policy}"
    );
    assert_eq!(view_only_policy["EnableLiveTvAccess"], true);
    assert_eq!(view_only_policy["EnableLiveTvManagement"], false);
    let view_only = db::get_user(&pool, view_only_id).await.unwrap().unwrap();
    let view_only_token = auth::issue_token(
        &state,
        &view_only,
        "livetv-test",
        "test-client",
        "livetv-view-only",
    )
    .await
    .unwrap()
    .token;

    let unpinned_source = json!({
        "LibraryId": library_id,
        "Name": "Unpinned fixture",
        "PlaylistUrl": "http://example.invalid/channels.m3u",
        "OriginPins": [{ "origin": feed_origin, "addresses": ["127.0.0.1"] }]
    });
    let (status, _) = call_json(
        &router,
        "POST",
        "/Admin/LiveTv/Sources",
        &admin_token,
        Some(unpinned_source),
    )
    .await;
    assert_eq!(status, StatusCode::BAD_REQUEST);

    let source_request = json!({
        "LibraryId": library_id,
        "Name": "Pinned loopback fixture",
        "PlaylistUrl": format!("{feed_origin}channels.m3u"),
        "GuideUrl": format!("{feed_origin}guide.xml"),
        "OriginPins": [{ "origin": feed_origin, "addresses": ["127.0.0.1"] }]
    });
    let (status, source_body) = call_json(
        &router,
        "POST",
        "/Admin/LiveTv/Sources",
        &admin_token,
        Some(source_request.clone()),
    )
    .await;
    assert_eq!(status, StatusCode::CREATED, "{source_body}");
    let source_id = Uuid::parse_str(source_body["Id"].as_str().unwrap()).unwrap();
    for secret_key in ["PlaylistUrl", "GuideUrl", "OriginPins"] {
        assert!(
            source_body.get(secret_key).is_none(),
            "create response exposed {secret_key}"
        );
    }
    let (duplicate_create_status, _) = call_json(
        &router,
        "POST",
        "/Admin/LiveTv/Sources",
        &admin_token,
        Some(source_request.clone()),
    )
    .await;
    assert_eq!(duplicate_create_status, StatusCode::CONFLICT);
    let (source_list_status, source_list) =
        call_json(&router, "GET", "/Admin/LiveTv/Sources", &admin_token, None).await;
    assert_eq!(source_list_status, StatusCode::OK, "{source_list}");
    let source_list_text = source_list.to_string();
    assert!(!source_list_text.contains(&feed_origin));
    assert!(!source_list_text.contains("127.0.0.1"));
    for secret_key in ["PlaylistUrl", "GuideUrl", "OriginPins"] {
        assert!(
            source_list_text.find(secret_key).is_none(),
            "source list exposed {secret_key}"
        );
    }
    for (method, body) in [
        ("POST", Some(json!({ "Name": "Viewer edit attempt" }))),
        ("DELETE", None),
    ] {
        let (status, _) = call_json(
            &router,
            method,
            &format!("/Admin/LiveTv/Sources/{source_id}"),
            &view_only_token,
            body,
        )
        .await;
        assert_eq!(
            status,
            StatusCode::FORBIDDEN,
            "{method} source without management"
        );
    }
    let (rename_status, renamed_source) = call_json(
        &router,
        "POST",
        &format!("/Admin/LiveTv/Sources/{source_id}"),
        &admin_token,
        Some(json!({ "Name": "Pinned IPTV renamed" })),
    )
    .await;
    assert_eq!(rename_status, StatusCode::OK, "{renamed_source}");
    assert_eq!(renamed_source["Name"], "Pinned IPTV renamed");
    for secret_key in ["PlaylistUrl", "GuideUrl", "OriginPins"] {
        assert!(
            renamed_source.get(secret_key).is_none(),
            "update response exposed {secret_key}"
        );
    }
    let stored_source_config: (String, Option<String>, String) = sqlx::query_as(
        "SELECT playlist_url,guide_url,origin_pins::text FROM live_tv_sources WHERE id=$1",
    )
    .bind(source_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stored_source_config.0, format!("{feed_origin}channels.m3u"));
    assert_eq!(
        stored_source_config.1,
        Some(format!("{feed_origin}guide.xml"))
    );
    assert!(stored_source_config.2.contains("127.0.0.1"));
    let (unsafe_update_status, _) = call_json(
        &router,
        "POST",
        &format!("/Admin/LiveTv/Sources/{source_id}"),
        &admin_token,
        Some(json!({ "PlaylistUrl": "http://example.invalid/channels.m3u" })),
    )
    .await;
    assert_eq!(unsafe_update_status, StatusCode::BAD_REQUEST);
    let (refresh_status, refresh_body) = call_json(
        &router,
        "POST",
        &format!("/Admin/LiveTv/Sources/{source_id}/Refresh"),
        &admin_token,
        None,
    )
    .await;
    assert_eq!(refresh_status, StatusCode::OK, "{refresh_body}");
    let (duplicate_candidate_status, duplicate_candidate_body) = call_json(
        &router,
        "POST",
        "/Admin/LiveTv/Sources",
        &admin_token,
        Some(json!({
            "LibraryId": library_id,
            "Name": "Duplicate candidate",
            "PlaylistUrl": format!("{feed_origin}channels.m3u"),
            "GuideUrl": format!("{feed_origin}guide.xml"),
            "OriginPins": [{ "origin": feed_origin, "addresses": ["127.0.0.1"] }]
        })),
    )
    .await;
    assert_eq!(
        duplicate_candidate_status,
        StatusCode::CREATED,
        "{duplicate_candidate_body}"
    );
    let duplicate_candidate_id = duplicate_candidate_body["Id"].as_str().unwrap();
    let (duplicate_rename_status, _) = call_json(
        &router,
        "POST",
        &format!("/Admin/LiveTv/Sources/{duplicate_candidate_id}"),
        &admin_token,
        Some(json!({ "Name": "Pinned IPTV renamed" })),
    )
    .await;
    assert_eq!(duplicate_rename_status, StatusCode::CONFLICT);
    let (duplicate_candidate_delete_status, _) = call_json(
        &router,
        "DELETE",
        &format!("/Admin/LiveTv/Sources/{duplicate_candidate_id}"),
        &admin_token,
        None,
    )
    .await;
    assert_eq!(duplicate_candidate_delete_status, StatusCode::NO_CONTENT);
    let (deleted_candidate_refresh_status, _) = call_json(
        &router,
        "POST",
        &format!("/Admin/LiveTv/Sources/{duplicate_candidate_id}/Refresh"),
        &admin_token,
        None,
    )
    .await;
    assert_eq!(deleted_candidate_refresh_status, StatusCode::NOT_FOUND);

    let (channels_status, channels) = call_json(
        &router,
        "GET",
        "/LiveTv/Channels?Limit=20",
        &admin_token,
        None,
    )
    .await;
    assert_eq!(channels_status, StatusCode::OK, "{channels}");
    assert_eq!(channels["TotalRecordCount"], 3);
    assert_eq!(channels["Items"][0]["Type"], "LiveTvChannel");

    let minimum = (now - ChronoDuration::minutes(2)).to_rfc3339_opts(SecondsFormat::Secs, true);
    let maximum = (now + ChronoDuration::hours(3)).to_rfc3339_opts(SecondsFormat::Secs, true);
    let guide_uri =
        format!("/LiveTv/Programs?MinStartDate={minimum}&MaxStartDate={maximum}&Limit=20");
    let (programs_status, programs) =
        call_json(&router, "GET", &guide_uri, &admin_token, None).await;
    assert_eq!(programs_status, StatusCode::OK, "{programs}");
    assert_eq!(programs["TotalRecordCount"], 5);
    assert_eq!(
        sqlx::query_scalar::<_, Option<i16>>(
            "SELECT policy_rating_value FROM live_tv_programs WHERE title='Late News'",
        )
        .fetch_one(&pool)
        .await
        .unwrap(),
        Some(100)
    );

    let (viewer_channels_status, viewer_channels) = call_json(
        &router,
        "GET",
        "/LiveTv/Channels?Limit=20",
        &viewer_token,
        None,
    )
    .await;
    assert_eq!(viewer_channels_status, StatusCode::OK, "{viewer_channels}");
    assert_eq!(viewer_channels["TotalRecordCount"], 1);
    assert_eq!(viewer_channels["Items"][0]["Name"], "Family Channel");
    let (viewer_programs_status, viewer_programs) =
        call_json(&router, "GET", &guide_uri, &viewer_token, None).await;
    assert_eq!(viewer_programs_status, StatusCode::OK, "{viewer_programs}");
    assert_eq!(viewer_programs["TotalRecordCount"], 3);
    assert_eq!(viewer_programs["Items"][0]["Name"], "Family Hour");

    let family_channel_id =
        Uuid::parse_str(viewer_channels["Items"][0]["Id"].as_str().unwrap()).unwrap();
    let family_program_id =
        Uuid::parse_str(viewer_programs["Items"][0]["Id"].as_str().unwrap()).unwrap();

    let (no_access_channels_status, no_access_channels) = call_json(
        &router,
        "GET",
        "/LiveTv/Channels?Limit=20",
        &no_tv_access_token,
        None,
    )
    .await;
    assert_eq!(
        no_access_channels_status,
        StatusCode::FORBIDDEN,
        "{no_access_channels}"
    );
    for uri in [
        "/LiveTv/Programs",
        "/LiveTv/Timers",
        "/LiveTv/SeriesTimers",
        "/LiveTv/Recordings",
        "/LiveTv/TunerHosts",
    ] {
        let (status, body) = call_json(&router, "GET", uri, &no_tv_access_token, None).await;
        assert_eq!(status, StatusCode::FORBIDDEN, "GET {uri}: {body}");
    }
    let (no_access_items_status, no_access_items) = call_json(
        &router,
        "GET",
        "/Items?IncludeItemTypes=LiveTvChannel&EnableTotalRecordCount=true",
        &no_tv_access_token,
        None,
    )
    .await;
    assert_eq!(no_access_items_status, StatusCode::OK, "{no_access_items}");
    assert_eq!(no_access_items["TotalRecordCount"], 0);
    let (no_access_playback_status, no_access_playback) = call_json(
        &router,
        "POST",
        &format!("/Items/{family_channel_id}/PlaybackInfo"),
        &no_tv_access_token,
        Some(json!({})),
    )
    .await;
    assert_eq!(
        no_access_playback_status,
        StatusCode::FORBIDDEN,
        "{no_access_playback}"
    );
    let (no_access_hls_status, no_access_hls) = call_json(
        &router,
        "GET",
        &format!(
            "/LiveTv/Channels/{family_channel_id}/master.m3u8?PlaySessionId={}",
            Uuid::new_v4()
        ),
        &no_tv_access_token,
        None,
    )
    .await;
    assert_eq!(
        no_access_hls_status,
        StatusCode::FORBIDDEN,
        "{no_access_hls}"
    );

    let (view_only_channels_status, view_only_channels) = call_json(
        &router,
        "GET",
        "/LiveTv/Channels?Limit=20",
        &view_only_token,
        None,
    )
    .await;
    assert_eq!(
        view_only_channels_status,
        StatusCode::OK,
        "{view_only_channels}"
    );
    assert_eq!(view_only_channels["TotalRecordCount"], 3);
    let view_only_start = Utc::now() + ChronoDuration::minutes(10);
    let (view_only_timer_status, view_only_timer) = call_json(
        &router,
        "POST",
        "/LiveTv/Timers",
        &view_only_token,
        Some(json!({
            "ChannelId": family_channel_id,
            "StartDate": view_only_start.to_rfc3339(),
            "EndDate": (view_only_start + ChronoDuration::minutes(30)).to_rfc3339()
        })),
    )
    .await;
    assert_eq!(
        view_only_timer_status,
        StatusCode::FORBIDDEN,
        "{view_only_timer}"
    );

    let (defaults_status, defaults) = call_json(
        &router,
        "GET",
        &format!("/LiveTv/Timers/Defaults?programId={family_program_id}"),
        &viewer_token,
        None,
    )
    .await;
    assert_eq!(defaults_status, StatusCode::OK, "{defaults}");
    assert_eq!(defaults["Type"], "SeriesTimerInfoDto");
    assert_eq!(defaults["ProgramId"], family_program_id.to_string());
    assert_eq!(defaults["ChannelId"], family_channel_id.to_string());
    assert_eq!(defaults["ChannelName"], "Family Channel");
    assert_eq!(defaults["Name"], "Family Hour");
    assert_eq!(defaults["RecordAnyTime"], true);
    assert!(defaults["StartDate"].as_str().is_some());
    assert!(defaults["EndDate"].as_str().is_some());
    assert!(defaults.get("OutputLibraryId").is_none());
    let (bare_defaults_status, bare_defaults) = call_json(
        &router,
        "GET",
        "/LiveTv/Timers/Defaults",
        &admin_token,
        None,
    )
    .await;
    assert_eq!(bare_defaults_status, StatusCode::OK, "{bare_defaults}");
    assert_eq!(bare_defaults["Type"], "SeriesTimerInfoDto");
    assert!(bare_defaults.get("ChannelId").is_none());
    assert!(bare_defaults.get("OutputLibraryId").is_none());

    let (timer_status, _) = call_json(
        &router,
        "POST",
        "/LiveTv/Timers",
        &viewer_token,
        Some(json!({
            "ChannelId": family_channel_id,
            "ProgramId": family_program_id,
            "PrePaddingSeconds": 60,
            "PostPaddingSeconds": 120,
            "Name": "Standard timer payload"
        })),
    )
    .await;
    assert_eq!(timer_status, StatusCode::NO_CONTENT);
    let (created_timers_status, created_timers) = call_json(
        &router,
        "GET",
        &format!("/LiveTv/Timers?ChannelId={family_channel_id}&IsScheduled=true"),
        &viewer_token,
        None,
    )
    .await;
    assert_eq!(created_timers_status, StatusCode::OK, "{created_timers}");
    let timer = created_timers["Items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["ProgramId"] == family_program_id.to_string())
        .unwrap();
    assert_eq!(timer["OutputLibraryId"], library_id.to_string());
    assert_eq!(timer["Status"], "New");
    assert_eq!(timer["Name"], "Family Hour");
    let timer_id = timer["Id"].as_str().unwrap();
    let (timer_get_status, timer_get) = call_json(
        &router,
        "GET",
        &format!("/LiveTv/Timers/{timer_id}"),
        &viewer_token,
        None,
    )
    .await;
    assert_eq!(timer_get_status, StatusCode::OK, "{timer_get}");
    assert_eq!(timer_get["Status"], "New");

    let mut series_timer_payload = defaults.clone();
    series_timer_payload["DayPattern"] = json!("Daily");
    series_timer_payload["Name"] = json!("Family Hour rule");
    let (series_create_status, _) = call_json(
        &router,
        "POST",
        "/LiveTv/SeriesTimers",
        &viewer_token,
        Some(series_timer_payload),
    )
    .await;
    assert_eq!(series_create_status, StatusCode::NO_CONTENT);
    let (admin_series_create_status, admin_series_create_body) = call_json(
        &router,
        "POST",
        "/LiveTv/SeriesTimers",
        &admin_token,
        Some(json!({
            "ChannelId": family_channel_id,
            "ProgramId": family_program_id,
            "Name": "Admin Family Hour rule",
            "DayPattern": "Daily"
        })),
    )
    .await;
    assert_eq!(
        admin_series_create_status,
        StatusCode::NO_CONTENT,
        "{admin_series_create_body}"
    );
    let admin_scheduled_airings: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM live_tv_timers t \
         JOIN live_tv_series_timers s ON s.id=t.series_timer_id \
         WHERE s.owner_user_id=$1 AND s.name='Admin Family Hour rule' \
           AND t.status='scheduled'",
    )
    .bind(admin_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert!(
        admin_scheduled_airings > 0,
        "an admin with default-false stored Live TV flags must still materialize series airings"
    );
    let (series_list_status, series_list) =
        call_json(&router, "GET", "/LiveTv/SeriesTimers", &viewer_token, None).await;
    assert_eq!(series_list_status, StatusCode::OK, "{series_list}");
    assert_eq!(series_list["TotalRecordCount"], 1);
    assert_eq!(series_list["Items"][0]["Type"], "SeriesTimerInfoDto");
    assert_eq!(series_list["Items"][0]["Name"], "Family Hour rule");
    assert_eq!(series_list["Items"][0]["DayPattern"], "Daily");
    assert_eq!(
        series_list["Items"][0]["OutputLibraryId"],
        library_id.to_string()
    );
    let series_timer_id = series_list["Items"][0]["Id"].as_str().unwrap().to_owned();
    let series_start_snapshot = series_list["Items"][0]["StartDate"].clone();
    let series_end_snapshot = series_list["Items"][0]["EndDate"].clone();
    let (view_only_series_status, view_only_series) = call_json(
        &router,
        "POST",
        "/LiveTv/SeriesTimers",
        &view_only_token,
        Some(json!({
            "ChannelId": family_channel_id,
            "ProgramId": family_program_id,
            "Name": "Forbidden view-only series rule",
            "DayPattern": "Daily"
        })),
    )
    .await;
    assert_eq!(
        view_only_series_status,
        StatusCode::FORBIDDEN,
        "{view_only_series}"
    );

    // A guide refresh deletes the guide rows and can null the child's
    // program_id FK. The stable series rule plus its airing interval must
    // still prevent a duplicate timer after the same guide is re-imported.
    let refresh_children_before: Vec<(Uuid, DateTime<Utc>, DateTime<Utc>)> = sqlx::query_as(
        "SELECT id,start_at,end_at FROM live_tv_timers WHERE series_timer_id=$1 \
         AND status='scheduled' ORDER BY start_at,id",
    )
    .bind(Uuid::parse_str(&series_timer_id).unwrap())
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(refresh_children_before.len(), 1);
    let (stable_airing_id, stable_airing_start, stable_airing_end) = refresh_children_before[0];
    sqlx::query("UPDATE live_tv_timers SET program_id=NULL WHERE id=$1")
        .bind(stable_airing_id)
        .execute(&pool)
        .await
        .unwrap();
    // Refresh must lock users before its first guide mutation. Holding the
    // viewer row forces the early source-wide lock query to wait; a guide
    // DELETE query here would recreate the user↔program-FK lock cycle.
    let mut held_refresh_user = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(viewer_id)
        .fetch_one(&mut *held_refresh_user)
        .await
        .unwrap();
    let dedup_refresh_router = router.clone();
    let dedup_refresh_token = admin_token.clone();
    let dedup_refresh_uri = format!("/Admin/LiveTv/Sources/{source_id}/Refresh");
    let mut dedup_refresh_task = tokio::spawn(async move {
        call_json(
            &dedup_refresh_router,
            "POST",
            &dedup_refresh_uri,
            &dedup_refresh_token,
            None,
        )
        .await
    });
    let waiting_query = wait_for_refresh_user_lock_waiter(&pool, &mut dedup_refresh_task).await;
    assert!(waiting_query.contains("SELECT u.id FROM users u WHERE TRUE"));
    assert!(waiting_query.contains("ORDER BY u.id FOR UPDATE OF u"));
    assert!(!waiting_query.contains("DELETE FROM live_tv_programs"));

    // A newly inserted non-admin must participate in the same advisory lock:
    // otherwise it could be omitted from the refresh's user-row snapshot and
    // create a timer while guide rows are being replaced.
    let create_pool = pool.clone();
    let create_run_id = run_id;
    let mut concurrent_user_create = tokio::spawn(async move {
        db::create_user(
            &create_pool,
            create_run_id,
            &db::NewUser {
                username: "refresh-race-non-admin".to_owned(),
                password_hash: "unused-test-hash".to_owned(),
                is_admin: false,
                disabled: false,
                enable_remote_access: false,
                allow_media_playback: true,
                enable_content_downloading: false,
                enable_live_tv_access: true,
                enable_live_tv_management: false,
                max_parental_rating: None,
                block_unrated_items: Vec::new(),
                allowed_library_ids: None,
            },
        )
        .await
    });
    wait_for_user_creation_advisory_waiter(&pool, &mut concurrent_user_create).await;
    held_refresh_user.commit().await.unwrap();
    let (dedup_refresh_status, dedup_refresh_body) = dedup_refresh_task.await.unwrap();
    assert_eq!(dedup_refresh_status, StatusCode::OK, "{dedup_refresh_body}");
    let created_during_refresh = concurrent_user_create.await.unwrap().unwrap();
    assert_eq!(created_during_refresh.username, "refresh-race-non-admin");
    let dedup_rows: Vec<(Uuid, Option<Uuid>)> = sqlx::query_as(
        "SELECT id,program_id FROM live_tv_timers WHERE series_timer_id=$1 \
         AND start_at=$2 AND end_at=$3 AND status='scheduled' ORDER BY id",
    )
    .bind(Uuid::parse_str(&series_timer_id).unwrap())
    .bind(stable_airing_start)
    .bind(stable_airing_end)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(dedup_rows.len(), 1, "refresh duplicated the same airing");
    assert_eq!(dedup_rows[0].0, stable_airing_id);
    assert_eq!(
        dedup_rows[0].1, None,
        "guide refresh should null the old program FK"
    );

    let (paged_status, paged) = call_json(
        &router,
        "GET",
        "/LiveTv/SeriesTimers?StartIndex=0&Limit=1&EnableTotalRecordCount=false",
        &viewer_token,
        None,
    )
    .await;
    assert_eq!(paged_status, StatusCode::OK, "{paged}");
    assert_eq!(paged["StartIndex"], 0);
    assert_eq!(paged["TotalRecordCount"], 0);
    assert_eq!(paged["Items"].as_array().unwrap().len(), 1);
    let (next_page_status, next_page) = call_json(
        &router,
        "GET",
        "/LiveTv/SeriesTimers?StartIndex=1&Limit=1",
        &viewer_token,
        None,
    )
    .await;
    assert_eq!(next_page_status, StatusCode::OK, "{next_page}");
    assert_eq!(next_page["StartIndex"], 1);
    assert_eq!(next_page["TotalRecordCount"], 1);
    assert!(next_page["Items"].as_array().unwrap().is_empty());
    let (invalid_page_status, _) = call_json(
        &router,
        "GET",
        "/LiveTv/SeriesTimers?StartIndex=-1&Limit=1",
        &viewer_token,
        None,
    )
    .await;
    assert_eq!(invalid_page_status, StatusCode::BAD_REQUEST);
    let (invalid_limit_status, _) = call_json(
        &router,
        "GET",
        "/LiveTv/SeriesTimers?Limit=65",
        &viewer_token,
        None,
    )
    .await;
    assert_eq!(invalid_limit_status, StatusCode::BAD_REQUEST);
    let (series_get_status, series_get) = call_json(
        &router,
        "GET",
        &format!("/LiveTv/SeriesTimers/{series_timer_id}"),
        &viewer_token,
        None,
    )
    .await;
    assert_eq!(series_get_status, StatusCode::OK, "{series_get}");
    assert_eq!(series_get["Id"], series_timer_id);
    let (other_owner_status, _) = call_json(
        &router,
        "GET",
        &format!("/LiveTv/SeriesTimers/{series_timer_id}"),
        &admin_token,
        None,
    )
    .await;
    assert_eq!(other_owner_status, StatusCode::NOT_FOUND);
    let (unsupported_rule_status, _) = call_json(
        &router,
        "POST",
        "/LiveTv/SeriesTimers",
        &viewer_token,
        Some(json!({
            "ChannelId": family_channel_id,
            "ProgramId": family_program_id,
            "RecordAnyChannel": true
        })),
    )
    .await;
    assert_eq!(unsupported_rule_status, StatusCode::BAD_REQUEST);

    let encore_program_id = program_id_by_name(&viewer_programs, "Family Encore");
    let (manual_encore_status, manual_encore_body) = call_json(
        &router,
        "POST",
        "/LiveTv/Timers",
        &viewer_token,
        Some(json!({
            "ChannelId": family_channel_id,
            "ProgramId": encore_program_id
        })),
    )
    .await;
    assert_eq!(
        manual_encore_status,
        StatusCode::NO_CONTENT,
        "{manual_encore_body}"
    );
    let (manual_encore_timer_id, manual_encore_start, manual_encore_end): (
        Uuid,
        DateTime<Utc>,
        DateTime<Utc>,
    ) = sqlx::query_as(
        "SELECT id,start_at,end_at FROM live_tv_timers \
             WHERE owner_user_id=$1 AND program_id=$2 AND status='scheduled'",
    )
    .bind(viewer_id)
    .bind(encore_program_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    let (encore_create_status, _) = call_json(
        &router,
        "POST",
        "/LiveTv/SeriesTimers",
        &viewer_token,
        Some(json!({
            "ChannelId": family_channel_id,
            "ProgramId": encore_program_id,
            "Name": "Family Encore rule",
            "DayPattern": "Daily"
        })),
    )
    .await;
    assert_eq!(encore_create_status, StatusCode::NO_CONTENT);
    let (series_rules_status, series_rules) =
        call_json(&router, "GET", "/LiveTv/SeriesTimers", &viewer_token, None).await;
    assert_eq!(series_rules_status, StatusCode::OK, "{series_rules}");
    let encore_rule_id = Uuid::parse_str(
        series_rules["Items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["Name"] == "Family Encore rule")
            .unwrap()["Id"]
            .as_str()
            .unwrap(),
    )
    .unwrap();

    // Guide replacement clears the manual timer's program FK. The active
    // series rule must still recognize the same channel airing by its stable
    // interval instead of queueing a duplicate DVR capture.
    let (manual_refresh_status, manual_refresh_body) = call_json(
        &router,
        "POST",
        &format!("/Admin/LiveTv/Sources/{source_id}/Refresh"),
        &admin_token,
        None,
    )
    .await;
    assert_eq!(
        manual_refresh_status,
        StatusCode::OK,
        "{manual_refresh_body}"
    );
    let same_manual_airing: Vec<(Uuid, Option<Uuid>, Option<Uuid>)> = sqlx::query_as(
        "SELECT id,program_id,series_timer_id FROM live_tv_timers \
         WHERE owner_user_id=$1 AND channel_item_id=$2 AND start_at=$3 AND end_at=$4 \
           AND status IN ('scheduled','recording') ORDER BY id",
    )
    .bind(viewer_id)
    .bind(family_channel_id)
    .bind(manual_encore_start)
    .bind(manual_encore_end)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(
        same_manual_airing.len(),
        1,
        "guide refresh duplicated a manual airing"
    );
    assert_eq!(same_manual_airing[0].0, manual_encore_timer_id);
    assert_eq!(
        same_manual_airing[0].1, None,
        "refresh should null the old guide FK"
    );
    assert_eq!(
        same_manual_airing[0].2, None,
        "the surviving timer is manual"
    );

    // The source refresh first removes the previous guide rows. Clearing this
    // anchor FK keeps that earlier ON DELETE SET NULL cascade out of the race;
    // the rule's title, rating, day, and channel snapshots remain authoritative.
    sqlx::query("UPDATE live_tv_series_timers SET program_id=NULL WHERE id=$1")
        .bind(encore_rule_id)
        .execute(&pool)
        .await
        .unwrap();

    let mut held_rule = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM live_tv_series_timers WHERE id=$1 FOR UPDATE")
        .bind(encore_rule_id)
        .fetch_one(&mut *held_rule)
        .await
        .unwrap();
    let refresh_router = router.clone();
    let refresh_token = admin_token.clone();
    let refresh_uri = format!("/Admin/LiveTv/Sources/{source_id}/Refresh");
    let refresh_task = tokio::spawn(async move {
        call_json(&refresh_router, "POST", &refresh_uri, &refresh_token, None).await
    });
    wait_for_series_rule_lock_waiters(&pool, 1, None).await;

    let cancel_router = router.clone();
    let cancel_token = viewer_token.clone();
    let cancel_uri = format!("/LiveTv/SeriesTimers/{encore_rule_id}");
    let mut cancel_task = tokio::spawn(async move {
        call_json(&cancel_router, "DELETE", &cancel_uri, &cancel_token, None).await
    });
    wait_for_series_rule_lock_waiters(&pool, 2, Some(&mut cancel_task)).await;
    held_rule.commit().await.unwrap();

    let (race_refresh_status, race_refresh_body) = refresh_task.await.unwrap();
    assert_eq!(race_refresh_status, StatusCode::OK, "{race_refresh_body}");
    let (race_cancel_status, race_cancel_body) = cancel_task.await.unwrap();
    assert_eq!(
        race_cancel_status,
        StatusCode::NO_CONTENT,
        "{race_cancel_body}"
    );
    let active_encore_timers: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM live_tv_timers WHERE series_timer_id=$1 \
         AND status IN ('scheduled','recording','publishing')",
    )
    .bind(encore_rule_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(active_encore_timers, 0);

    sqlx::query("DELETE FROM live_tv_programs WHERE id=$1")
        .bind(family_program_id)
        .execute(&pool)
        .await
        .unwrap();
    let (snapshot_status, snapshot_timer) = call_json(
        &router,
        "GET",
        &format!("/LiveTv/Timers/{timer_id}"),
        &viewer_token,
        None,
    )
    .await;
    assert_eq!(snapshot_status, StatusCode::OK, "{snapshot_timer}");
    assert_eq!(snapshot_timer["Name"], "Family Hour");

    let (series_after_delete_status, series_after_delete) = call_json(
        &router,
        "GET",
        &format!("/LiveTv/SeriesTimers/{series_timer_id}"),
        &viewer_token,
        None,
    )
    .await;
    assert_eq!(
        series_after_delete_status,
        StatusCode::OK,
        "{series_after_delete}"
    );
    assert_eq!(series_after_delete["ProgramId"], Value::Null);
    assert_eq!(series_after_delete["Name"], "Family Hour rule");
    let (series_update_status, _) = call_json(
        &router,
        "POST",
        &format!("/LiveTv/SeriesTimers/{series_timer_id}"),
        &viewer_token,
        Some(json!({ "Id": series_timer_id, "PrePaddingSeconds": 30 })),
    )
    .await;
    assert_eq!(series_update_status, StatusCode::NO_CONTENT);
    let (series_updated_status, series_updated) = call_json(
        &router,
        "GET",
        &format!("/LiveTv/SeriesTimers/{series_timer_id}"),
        &viewer_token,
        None,
    )
    .await;
    assert_eq!(series_updated_status, StatusCode::OK, "{series_updated}");
    assert_eq!(series_updated["Name"], "Family Hour rule");
    assert_eq!(series_updated["StartDate"], series_start_snapshot);
    assert_eq!(series_updated["EndDate"], series_end_snapshot);
    let (series_child_status, series_child_list) = call_json(
        &router,
        "GET",
        &format!("/LiveTv/Timers?SeriesTimerId={series_timer_id}"),
        &viewer_token,
        None,
    )
    .await;
    assert_eq!(series_child_status, StatusCode::OK, "{series_child_list}");
    assert_eq!(series_child_list["TotalRecordCount"], 1);
    assert_eq!(
        series_child_list["Items"][0]["SeriesTimerId"],
        series_timer_id
    );
    assert_eq!(series_child_list["Items"][0]["PrePaddingSeconds"], 30);
    let (series_cancel_status, _) = call_json(
        &router,
        "DELETE",
        &format!("/LiveTv/SeriesTimers/{series_timer_id}"),
        &viewer_token,
        None,
    )
    .await;
    assert_eq!(series_cancel_status, StatusCode::NO_CONTENT);
    let (series_cancelled_list_status, series_cancelled_list) =
        call_json(&router, "GET", "/LiveTv/SeriesTimers", &viewer_token, None).await;
    assert_eq!(series_cancelled_list_status, StatusCode::OK);
    assert_eq!(series_cancelled_list["TotalRecordCount"], 0);

    let manual_start = Utc::now() + ChronoDuration::minutes(10);
    let manual_end = manual_start + ChronoDuration::minutes(30);
    let manual_request = json!({
        "ChannelId": family_channel_id,
        "StartDate": manual_start.to_rfc3339(),
        "EndDate": manual_end.to_rfc3339(),
        "Name": "  Manual family recording  "
    });
    let (manual_status, _) = call_json(
        &router,
        "POST",
        "/LiveTv/Timers",
        &admin_token,
        Some(manual_request),
    )
    .await;
    assert_eq!(manual_status, StatusCode::NO_CONTENT);
    let (manual_list_status, manual_list) = call_json(
        &router,
        "GET",
        &format!("/LiveTv/Timers?ChannelId={family_channel_id}&IsScheduled=true"),
        &admin_token,
        None,
    )
    .await;
    assert_eq!(manual_list_status, StatusCode::OK, "{manual_list}");
    let manual_timer = manual_list["Items"]
        .as_array()
        .unwrap()
        .iter()
        .find(|item| item["Name"] == "Manual family recording")
        .unwrap();
    assert_eq!(manual_timer["OutputLibraryId"], library_id.to_string());
    let manual_id = manual_timer["Id"].as_str().unwrap().to_owned();
    let (manual_get_status, manual_get) = call_json(
        &router,
        "GET",
        &format!("/LiveTv/Timers/{manual_id}"),
        &admin_token,
        None,
    )
    .await;
    assert_eq!(manual_get_status, StatusCode::OK, "{manual_get}");
    assert_eq!(manual_get["Name"], "Manual family recording");
    let updated_start = manual_start + ChronoDuration::minutes(5);
    let updated_end = manual_end + ChronoDuration::minutes(5);
    let (update_status, _) = call_json(
        &router,
        "POST",
        &format!("/LiveTv/Timers/{manual_id}"),
        &admin_token,
        Some(json!({
            "Id": manual_id,
            "Name": "  Updated manual family recording  ",
            "StartDate": updated_start.to_rfc3339(),
            "EndDate": updated_end.to_rfc3339(),
            "PrePaddingSeconds": 15,
            "PostPaddingSeconds": 30,
            "Status": "New",
            "KeepUntil": "UntilDeleted"
        })),
    )
    .await;
    assert_eq!(update_status, StatusCode::NO_CONTENT);
    let (updated_get_status, updated_get) = call_json(
        &router,
        "GET",
        &format!("/LiveTv/Timers/{manual_id}"),
        &admin_token,
        None,
    )
    .await;
    assert_eq!(updated_get_status, StatusCode::OK, "{updated_get}");
    assert_eq!(updated_get["Name"], "Updated manual family recording");
    assert_eq!(updated_get["PrePaddingSeconds"], 15);
    assert_eq!(updated_get["PostPaddingSeconds"], 30);
    assert_eq!(
        DateTime::parse_from_rfc3339(updated_get["StartDate"].as_str().unwrap())
            .unwrap()
            .timestamp_micros(),
        updated_start.timestamp_micros()
    );
    let (wrong_owner_update_status, _) = call_json(
        &router,
        "POST",
        &format!("/LiveTv/Timers/{manual_id}"),
        &viewer_token,
        Some(json!({ "Id": manual_id })),
    )
    .await;
    assert_eq!(wrong_owner_update_status, StatusCode::NOT_FOUND);
    let (wrong_id_update_status, _) = call_json(
        &router,
        "POST",
        &format!("/LiveTv/Timers/{manual_id}"),
        &admin_token,
        Some(json!({ "Id": Uuid::new_v4() })),
    )
    .await;
    assert_eq!(wrong_id_update_status, StatusCode::BAD_REQUEST);
    let (admin_timers_status, admin_timers) = call_json(
        &router,
        "GET",
        &format!("/LiveTv/Timers?ChannelId={family_channel_id}&IsScheduled=true"),
        &admin_token,
        None,
    )
    .await;
    assert_eq!(admin_timers_status, StatusCode::OK, "{admin_timers}");
    let admin_timer_items = admin_timers["Items"].as_array().unwrap();
    assert_eq!(admin_timers["TotalRecordCount"], 3);
    assert_eq!(admin_timer_items.len(), 3);
    let manual_timer_matches = admin_timer_items
        .iter()
        .filter(|item| item["Id"] == manual_id)
        .collect::<Vec<_>>();
    assert_eq!(manual_timer_matches.len(), 1);
    assert_eq!(
        manual_timer_matches[0]["Name"],
        "Updated manual family recording"
    );
    assert_eq!(
        admin_timer_items
            .iter()
            .filter(|item| !item["SeriesTimerId"].is_null())
            .count(),
        2,
        "the remaining rows are the two scheduled series airings"
    );
    let (viewer_timers_status, viewer_timers) = call_json(
        &router,
        "GET",
        &format!("/LiveTv/Timers?ChannelId={family_channel_id}&IsScheduled=true"),
        &viewer_token,
        None,
    )
    .await;
    assert_eq!(viewer_timers_status, StatusCode::OK, "{viewer_timers}");
    assert_eq!(viewer_timers["TotalRecordCount"], 2);
    let viewer_timer_items = viewer_timers["Items"].as_array().unwrap();
    assert_eq!(viewer_timer_items.len(), 2);
    assert_eq!(viewer_timer_items[0]["Id"], timer["Id"]);
    assert_eq!(viewer_timer_items[0]["Name"], "Family Hour");
    assert_eq!(
        viewer_timer_items[1]["Id"],
        manual_encore_timer_id.to_string()
    );
    assert_eq!(viewer_timer_items[1]["Name"], "Family Encore");
    let overlong_name = "x".repeat(513);
    let (overlong_status, _) = call_json(
        &router,
        "POST",
        "/LiveTv/Timers",
        &admin_token,
        Some(json!({
            "ChannelId": family_channel_id,
            "StartDate": manual_start.to_rfc3339(),
            "EndDate": manual_end.to_rfc3339(),
            "Name": overlong_name
        })),
    )
    .await;
    assert_eq!(overlong_status, StatusCode::BAD_REQUEST);

    let (_, active_adult_channel) = call_json(
        &router,
        "GET",
        "/LiveTv/Channels?Limit=20",
        &admin_token,
        None,
    )
    .await;
    let adult_channel_id = channel_id_by_name(&active_adult_channel, "Adult Channel");
    let adult_program_id = program_id_by_name(&programs, "Late News");
    let (adult_defaults_status, _) = call_json(
        &router,
        "GET",
        &format!("/LiveTv/Timers/Defaults?ProgramId={adult_program_id}"),
        &viewer_token,
        None,
    )
    .await;
    assert_eq!(adult_defaults_status, StatusCode::NOT_FOUND);
    let (adult_timer_status, _) = call_json(
        &router,
        "POST",
        "/LiveTv/Timers",
        &viewer_token,
        Some(json!({ "ChannelId": adult_channel_id, "ProgramId": adult_program_id })),
    )
    .await;
    assert_eq!(adult_timer_status, StatusCode::NOT_FOUND);

    let channel_filter =
        format!("/LiveTv/Timers?ChannelId={family_channel_id}&IsScheduled=true&IsActive=false");
    let (timers_status, timers) =
        call_json(&router, "GET", &channel_filter, &viewer_token, None).await;
    assert_eq!(timers_status, StatusCode::OK, "{timers}");
    assert_eq!(timers["TotalRecordCount"], 2);
    assert_eq!(timers["Items"][0]["Id"], timer["Id"]);
    assert_eq!(timers["Items"][1]["Id"], manual_encore_timer_id.to_string());

    let series_filter = format!("/LiveTv/Timers?SeriesTimerId={}", Uuid::new_v4());
    for query in [
        "/LiveTv/Timers?IsActive=true",
        "/LiveTv/Timers?IsScheduled=false",
        series_filter.as_str(),
    ] {
        let (status, body) = call_json(&router, "GET", query, &viewer_token, None).await;
        assert_eq!(status, StatusCode::OK, "{body}");
        assert_eq!(body["TotalRecordCount"], 0);
    }

    let recorded_rows: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM live_tv_recordings")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        recorded_rows, 0,
        "timer creation does not claim a DVR capture"
    );

    // A request authenticated before revocation must not create a timer from
    // its stale CurrentUser snapshot. Hold the locked-user query, revoke both
    // Live TV grants, then let the create path perform its fresh READ COMMITTED
    // reload after the row lock is released.
    let mut held_viewer = pool.begin().await.unwrap();
    sqlx::query("SELECT id FROM users WHERE id=$1 FOR UPDATE")
        .bind(viewer_id)
        .fetch_one(&mut *held_viewer)
        .await
        .unwrap();
    let create_router = router.clone();
    let create_token = viewer_token.clone();
    let create_body = json!({
        "ChannelId": family_channel_id,
        "ProgramId": encore_program_id
    });
    let mut create_task = tokio::spawn(async move {
        call_json(
            &create_router,
            "POST",
            "/LiveTv/Timers",
            &create_token,
            Some(create_body),
        )
        .await
    });
    wait_for_timer_user_lock_waiter(&pool, &mut create_task).await;
    sqlx::query(
        "UPDATE users SET enable_live_tv_access=FALSE,enable_live_tv_management=FALSE WHERE id=$1",
    )
    .bind(viewer_id)
    .execute(&mut *held_viewer)
    .await
    .unwrap();
    held_viewer.commit().await.unwrap();
    let (revoked_create_status, revoked_create_body) = create_task.await.unwrap();
    assert_eq!(
        revoked_create_status,
        StatusCode::FORBIDDEN,
        "fresh policy reload must reject the waiting request: {revoked_create_body}"
    );
    let stale_timer_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM live_tv_timers WHERE owner_user_id=$1 AND program_id=$2 \
         AND status IN ('scheduled','recording')",
    )
    .bind(viewer_id)
    .bind(encore_program_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(stale_timer_count, 0);

    // Source edits and deletion must not interrupt an active capture. Once
    // the synthetic recording reaches a terminal state, the lifecycle update
    // below can proceed normally.
    let lifecycle_timer_id = Uuid::new_v4();
    let lifecycle_recording_id = Uuid::new_v4();
    let lifecycle_run_id = Uuid::new_v4();
    let capture_start = Utc::now() - ChronoDuration::minutes(1);
    let capture_end = Utc::now() + ChronoDuration::minutes(29);
    sqlx::query(
        "INSERT INTO live_tv_timers(id,owner_user_id,channel_item_id,start_at,end_at,output_library_id, \
         status,claimed_run_id,started_at) VALUES($1,$2,$3,$4,$5,$6,'recording',$7,NOW())",
    )
    .bind(lifecycle_timer_id)
    .bind(viewer_id)
    .bind(family_channel_id)
    .bind(capture_start)
    .bind(capture_end)
    .bind(library_id)
    .bind(lifecycle_run_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO live_tv_recordings(id,timer_id,channel_item_id,library_id,channel_name,title, \
         relative_path,status,claimed_run_id,started_at) \
         VALUES($1,$2,$3,$4,'Lifecycle test channel','Lifecycle test capture', \
         'livetv-source-lifecycle-test.ts','recording',$5,NOW())",
    )
    .bind(lifecycle_recording_id)
    .bind(lifecycle_timer_id)
    .bind(family_channel_id)
    .bind(library_id)
    .bind(lifecycle_run_id)
    .execute(&pool)
    .await
    .unwrap();
    let blocked_playlist = format!("{feed_origin}channels.m3u?recording-blocked=1");
    let (blocked_update_status, _) = call_json(
        &router,
        "POST",
        &format!("/Admin/LiveTv/Sources/{source_id}"),
        &admin_token,
        Some(json!({ "PlaylistUrl": blocked_playlist })),
    )
    .await;
    assert_eq!(blocked_update_status, StatusCode::CONFLICT);
    let (blocked_delete_status, _) = call_json(
        &router,
        "DELETE",
        &format!("/Admin/LiveTv/Sources/{source_id}"),
        &admin_token,
        None,
    )
    .await;
    assert_eq!(blocked_delete_status, StatusCode::CONFLICT);
    sqlx::query(
        "UPDATE live_tv_recordings SET status='failed',claimed_run_id=NULL,finished_at=NOW(), \
         last_error_code='source-unavailable',updated_at=NOW() WHERE id=$1",
    )
    .bind(lifecycle_recording_id)
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query(
        "UPDATE live_tv_timers SET status='failed',claimed_run_id=NULL,finished_at=NOW(), \
         last_error_code='source-unavailable',updated_at=NOW() WHERE id=$1",
    )
    .bind(lifecycle_timer_id)
    .execute(&pool)
    .await
    .unwrap();

    let changed_playlist = format!("{feed_origin}channels.m3u?revision=2");
    let (config_update_status, config_update_body) = call_json(
        &router,
        "POST",
        &format!("/Admin/LiveTv/Sources/{source_id}"),
        &admin_token,
        Some(json!({ "PlaylistUrl": changed_playlist })),
    )
    .await;
    assert_eq!(config_update_status, StatusCode::OK, "{config_update_body}");
    assert_eq!(config_update_body["RefreshStatus"], "queued");
    assert!(config_update_body["LastRefreshedAt"].is_null());
    for secret_key in ["PlaylistUrl", "GuideUrl", "OriginPins"] {
        assert!(
            config_update_body.get(secret_key).is_none(),
            "update response exposed {secret_key}"
        );
    }
    let imported_channel_statuses: Vec<bool> = sqlx::query_scalar(
        "SELECT enabled FROM live_tv_channels WHERE source_id=$1 ORDER BY item_id",
    )
    .bind(source_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(imported_channel_statuses, vec![false, false, false]);
    let retired_channel_url_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM live_tv_channels WHERE source_id=$1 \
         AND (stream_url IS NOT NULL OR logo_url IS NOT NULL)",
    )
    .bind(source_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(retired_channel_url_count, 0);
    let guide_after_playlist_update: Option<String> =
        sqlx::query_scalar("SELECT guide_url FROM live_tv_sources WHERE id=$1")
            .bind(source_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        guide_after_playlist_update,
        Some(format!("{feed_origin}guide.xml"))
    );
    let (reimport_status, reimport_body) = call_json(
        &router,
        "POST",
        &format!("/Admin/LiveTv/Sources/{source_id}/Refresh"),
        &admin_token,
        None,
    )
    .await;
    assert_eq!(reimport_status, StatusCode::OK, "{reimport_body}");
    let (reimported_channels_status, reimported_channels) = call_json(
        &router,
        "GET",
        "/LiveTv/Channels?Limit=20",
        &admin_token,
        None,
    )
    .await;
    assert_eq!(
        reimported_channels_status,
        StatusCode::OK,
        "{reimported_channels}"
    );
    assert_eq!(reimported_channels["TotalRecordCount"], 3);

    let refreshed_logo: Option<String> = sqlx::query_scalar(
        "SELECT logo_url FROM live_tv_channels WHERE source_id=$1 AND source_channel_id='unrated'",
    )
    .bind(source_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(
        refreshed_logo,
        Some(format!("{feed_origin}logos/unrated.png"))
    );

    let (omission_refresh_status, omission_refresh_body) = call_json(
        &router,
        "POST",
        &format!("/Admin/LiveTv/Sources/{source_id}/Refresh"),
        &admin_token,
        None,
    )
    .await;
    assert_eq!(
        omission_refresh_status,
        StatusCode::OK,
        "{omission_refresh_body}"
    );
    let omitted_channel: (bool, Option<String>, Option<String>) = sqlx::query_as(
        "SELECT enabled,stream_url,logo_url FROM live_tv_channels \
         WHERE source_id=$1 AND source_channel_id='unrated'",
    )
    .bind(source_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(omitted_channel, (false, None, None));

    let (clear_guide_status, clear_guide_body) = call_json(
        &router,
        "POST",
        &format!("/Admin/LiveTv/Sources/{source_id}"),
        &admin_token,
        Some(json!({ "GuideUrl": null })),
    )
    .await;
    assert_eq!(clear_guide_status, StatusCode::OK, "{clear_guide_body}");
    let guide_after_clear: Option<String> =
        sqlx::query_scalar("SELECT guide_url FROM live_tv_sources WHERE id=$1")
            .bind(source_id)
            .fetch_one(&pool)
            .await
            .unwrap();
    assert_eq!(
        guide_after_clear, None,
        "explicit null should clear the guide URL"
    );

    let (source_delete_status, _) = call_json(
        &router,
        "DELETE",
        &format!("/Admin/LiveTv/Sources/{source_id}"),
        &admin_token,
        None,
    )
    .await;
    assert_eq!(source_delete_status, StatusCode::NO_CONTENT);
    let (source_list_after_delete_status, sources_after_delete) =
        call_json(&router, "GET", "/Admin/LiveTv/Sources", &admin_token, None).await;
    assert_eq!(source_list_after_delete_status, StatusCode::OK);
    assert_eq!(sources_after_delete, json!([]));
    let deleted_source: DeletedSourceRow = sqlx::query_as(
        "SELECT playlist_url,guide_url,origin_pins::text,enabled,deleted_at \
         FROM live_tv_sources WHERE id=$1",
    )
    .bind(source_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(deleted_source.0, None);
    assert_eq!(deleted_source.1, None);
    assert_eq!(deleted_source.2, None);
    assert!(!deleted_source.3);
    assert!(deleted_source.4.is_some());
    let retained_disabled_channels: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM live_tv_channels WHERE source_id=$1 AND enabled=FALSE",
    )
    .bind(source_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(retained_disabled_channels, 3);
    let retained_channel_url_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM live_tv_channels WHERE source_id=$1 \
         AND (stream_url IS NOT NULL OR logo_url IS NOT NULL)",
    )
    .bind(source_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(retained_channel_url_count, 0);
    let remaining_scheduled_timers: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM live_tv_timers t JOIN live_tv_channels c ON c.item_id=t.channel_item_id \
         WHERE c.source_id=$1 AND t.status='scheduled'",
    )
    .bind(source_id)
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(remaining_scheduled_timers, 0);
    let (deleted_source_refresh_status, _) = call_json(
        &router,
        "POST",
        &format!("/Admin/LiveTv/Sources/{source_id}/Refresh"),
        &admin_token,
        None,
    )
    .await;
    assert_eq!(deleted_source_refresh_status, StatusCode::NOT_FOUND);
    let (deleted_channels_status, deleted_channels) = call_json(
        &router,
        "GET",
        "/LiveTv/Channels?Limit=20",
        &admin_token,
        None,
    )
    .await;
    assert_eq!(
        deleted_channels_status,
        StatusCode::OK,
        "{deleted_channels}"
    );
    assert_eq!(deleted_channels["TotalRecordCount"], 0);
    fixture_task.await.unwrap();

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
    let _ = fs::remove_dir_all(library_root);
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via PUFFINBOX_TEST_DATABASE_URL"]
async fn source_lifecycle_migration_preserves_active_capture_urls() {
    let database_url = env::var("PUFFINBOX_TEST_DATABASE_URL")
        .expect("set PUFFINBOX_TEST_DATABASE_URL to a disposable PostgreSQL database");
    let admin_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url)
        .await
        .unwrap();
    let schema = format!(
        "puffinbox_livetv_source_migration_{}",
        Uuid::new_v4().simple()
    );
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
    common::apply_migrations_through_0018(&pool).await.unwrap();

    let library_id = Uuid::new_v4();
    sqlx::query("INSERT INTO libraries(id,name,locations) VALUES($1,'Lifecycle migration fixture','[]'::jsonb)")
        .bind(library_id)
        .execute(&pool)
        .await
        .unwrap();
    let owner_id = insert_user(&pool, "lifecycle-migration-owner", false, false, None, &[]).await;
    let source_id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO live_tv_sources(id,library_id,name,playlist_url,guide_url,origin_pins) \
         VALUES($1,$2,'Lifecycle migration source','https://feed.example/channels.m3u', \
         'https://feed.example/guide.xml','[]'::jsonb)",
    )
    .bind(source_id)
    .bind(library_id)
    .execute(&pool)
    .await
    .unwrap();

    let channel_fixtures = [
        (
            Uuid::new_v4(),
            "recording",
            "https://feed.example/live.ts?token=recording",
            "https://feed.example/logo.png?token=recording",
        ),
        (
            Uuid::new_v4(),
            "publishing",
            "https://feed.example/publishing.ts?token=publishing",
            "https://feed.example/publishing.png?token=publishing",
        ),
        (
            Uuid::new_v4(),
            "inactive",
            "https://feed.example/retired.ts?token=stale",
            "https://feed.example/retired.png?token=stale",
        ),
    ];
    for (index, (item_id, source_channel_id, stream_url, logo_url)) in
        channel_fixtures.iter().enumerate()
    {
        let path = format!("livetv://migration/{source_channel_id}");
        let path_hash = format!("{:064x}", index + 1);
        sqlx::query(
            "INSERT INTO items(id,library_id,name,sort_name,item_type,path,path_hash) \
             VALUES($1,$2,$3,$3,'LiveTvChannel',$4,$5)",
        )
        .bind(item_id)
        .bind(library_id)
        .bind(source_channel_id)
        .bind(path)
        .bind(path_hash)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO live_tv_channels(item_id,library_id,source_id,source_channel_id,name, \
             stream_url,logo_url,enabled) VALUES($1,$2,$3,$4,$4,$5,$6,FALSE)",
        )
        .bind(item_id)
        .bind(library_id)
        .bind(source_id)
        .bind(source_channel_id)
        .bind(stream_url)
        .bind(logo_url)
        .execute(&pool)
        .await
        .unwrap();
    }

    for (index, (channel_id, recording_status, _, _)) in channel_fixtures[..2].iter().enumerate() {
        let timer_id = Uuid::new_v4();
        let recording_id = Uuid::new_v4();
        let claimed_run_id = Uuid::new_v4();
        sqlx::query(
            "INSERT INTO live_tv_timers(id,owner_user_id,channel_item_id,start_at,end_at, \
             output_library_id,status,claimed_run_id,started_at) \
             VALUES($1,$2,$3,NOW()-INTERVAL '1 minute',NOW()+INTERVAL '29 minutes',$4, \
             'recording',$5,NOW())",
        )
        .bind(timer_id)
        .bind(owner_id)
        .bind(channel_id)
        .bind(library_id)
        .bind(claimed_run_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO live_tv_recordings(id,timer_id,channel_item_id,library_id,channel_name, \
             title,relative_path,status,claimed_run_id,started_at) \
             VALUES($1,$2,$3,$4,'Lifecycle channel',$5,$6,$7,$8,NOW())",
        )
        .bind(recording_id)
        .bind(timer_id)
        .bind(channel_id)
        .bind(library_id)
        .bind(format!("{recording_status} capture"))
        .bind(format!("livetv-{index}.ts"))
        .bind(recording_status)
        .bind(claimed_run_id)
        .execute(&pool)
        .await
        .unwrap();
    }

    sqlx::raw_sql(include_str!(
        "../migrations/0019_live_tv_source_lifecycle.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();

    let migrated_channels: Vec<(String, Option<String>, Option<String>)> = sqlx::query_as(
        "SELECT source_channel_id,stream_url,logo_url FROM live_tv_channels \
         WHERE source_id=$1 ORDER BY source_channel_id",
    )
    .bind(source_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    let active_recording = migrated_channels
        .iter()
        .find(|(channel, _, _)| channel == "recording")
        .unwrap();
    assert_eq!(
        active_recording.1.as_deref(),
        Some("https://feed.example/live.ts?token=recording")
    );
    assert_eq!(
        active_recording.2.as_deref(),
        Some("https://feed.example/logo.png?token=recording")
    );
    let active_publishing = migrated_channels
        .iter()
        .find(|(channel, _, _)| channel == "publishing")
        .unwrap();
    assert_eq!(
        active_publishing.1.as_deref(),
        Some("https://feed.example/publishing.ts?token=publishing")
    );
    assert_eq!(
        active_publishing.2.as_deref(),
        Some("https://feed.example/publishing.png?token=publishing")
    );
    let inactive = migrated_channels
        .iter()
        .find(|(channel, _, _)| channel == "inactive")
        .unwrap();
    assert_eq!((inactive.1.as_deref(), inactive.2.as_deref()), (None, None));

    for (channel_id, _, _, _) in &channel_fixtures[..2] {
        sqlx::query(
            "UPDATE live_tv_timers SET status='failed',claimed_run_id=NULL,finished_at=NOW(), \
             last_error_code='source-unavailable',updated_at=NOW() \
             WHERE channel_item_id=$1 AND status='recording'",
        )
        .bind(channel_id)
        .execute(&pool)
        .await
        .unwrap();
        sqlx::query(
            "UPDATE live_tv_recordings SET status='failed',claimed_run_id=NULL,finished_at=NOW(), \
             last_error_code='source-unavailable',updated_at=NOW() \
             WHERE channel_item_id=$1 AND status IN ('recording','publishing')",
        )
        .bind(channel_id)
        .execute(&pool)
        .await
        .unwrap();
        let retired_urls: (Option<String>, Option<String>) =
            sqlx::query_as("SELECT stream_url,logo_url FROM live_tv_channels WHERE item_id=$1")
                .bind(channel_id)
                .fetch_one(&pool)
                .await
                .unwrap();
        assert_eq!(retired_urls, (None, None));
    }

    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .execute(&admin_pool)
        .await
        .unwrap();
    admin_pool.close().await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via PUFFINBOX_TEST_DATABASE_URL"]
async fn series_dedup_upgrade_keeps_recordings_and_cancels_scheduled_duplicates() {
    let database_url = env::var("PUFFINBOX_TEST_DATABASE_URL")
        .expect("set PUFFINBOX_TEST_DATABASE_URL to a disposable PostgreSQL database");
    let admin_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url)
        .await
        .unwrap();
    let schema = format!("puffinbox_livetv_upgrade_{}", Uuid::new_v4().simple());
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
    common::apply_migrations_through_0014(&pool).await.unwrap();

    let suffix = Uuid::new_v4().simple().to_string();
    let library_id = Uuid::new_v4();
    let owner_id = Uuid::new_v4();
    let source_id = Uuid::new_v4();
    let channel_id = Uuid::new_v4();
    let rule_id = Uuid::new_v4();
    let recording_timer_ids = [Uuid::new_v4(), Uuid::new_v4()];
    let scheduled_timer_id = Uuid::new_v4();
    let claimed_run_id = Uuid::new_v4();
    let opaque_path = format!("puffinbox://livetv/{source_id}/{channel_id}");
    let now = Utc::now();
    let start_at = now - ChronoDuration::minutes(15);
    let end_at = now + ChronoDuration::minutes(45);

    sqlx::query("INSERT INTO libraries(id,name,collection_type,locations) VALUES($1,$2,'mixed','[]'::jsonb)")
        .bind(library_id)
        .bind(format!("Series migration {suffix}"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO users(id,username,username_norm,password_hash) VALUES($1,$2,$2,'unused')",
    )
    .bind(owner_id)
    .bind(format!("upgrade-{suffix}"))
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO live_tv_sources(id,library_id,name,playlist_url,origin_pins) VALUES($1,$2,$3,'http://127.0.0.1/list.m3u','[]'::jsonb)")
        .bind(source_id)
        .bind(library_id)
        .bind(format!("Upgrade source {suffix}"))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO items(id,library_id,name,sort_name,item_type,path,path_hash) VALUES($1,$2,'Upgrade channel','upgrade channel','LiveTvChannel',$3,$4)")
        .bind(channel_id)
        .bind(library_id)
        .bind(&opaque_path)
        .bind(db::path_hash(&opaque_path))
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO live_tv_channels(item_id,library_id,source_id,source_channel_id,name,stream_url) VALUES($1,$2,$3,'upgrade','Upgrade channel','http://127.0.0.1/live.ts')")
        .bind(channel_id)
        .bind(library_id)
        .bind(source_id)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO live_tv_series_timers(id,owner_user_id,channel_item_id,name,match_title,match_title_key,start_at,end_at,output_library_id) VALUES($1,$2,$3,'Upgrade rule','Upgrade episode','upgrade episode',$4,$5,$6)")
        .bind(rule_id)
        .bind(owner_id)
        .bind(channel_id)
        .bind(start_at)
        .bind(end_at)
        .bind(library_id)
        .execute(&pool)
        .await
        .unwrap();

    for (index, timer_id) in recording_timer_ids.iter().enumerate() {
        sqlx::query("INSERT INTO live_tv_timers(id,owner_user_id,channel_item_id,series_timer_id,start_at,end_at,output_library_id,status,claimed_run_id,started_at) VALUES($1,$2,$3,$4,$5,$6,$7,'recording',$8,$9)")
            .bind(timer_id)
            .bind(owner_id)
            .bind(channel_id)
            .bind(rule_id)
            .bind(start_at)
            .bind(end_at)
            .bind(library_id)
            .bind(claimed_run_id)
            .bind(start_at)
            .execute(&pool)
            .await
            .unwrap();
        sqlx::query("INSERT INTO live_tv_recordings(id,timer_id,channel_item_id,library_id,channel_name,title,relative_path,status,claimed_run_id,started_at) VALUES($1,$2,$3,$4,'Upgrade channel','Upgrade episode',$5,'recording',$6,$7)")
            .bind(Uuid::new_v4())
            .bind(timer_id)
            .bind(channel_id)
            .bind(library_id)
            .bind(format!("upgrade/{index}.ts"))
            .bind(claimed_run_id)
            .bind(start_at)
            .execute(&pool)
            .await
            .unwrap();
    }
    sqlx::query("INSERT INTO live_tv_timers(id,owner_user_id,channel_item_id,series_timer_id,start_at,end_at,output_library_id,status) VALUES($1,$2,$3,$4,$5,$6,$7,'scheduled')")
        .bind(scheduled_timer_id)
        .bind(owner_id)
        .bind(channel_id)
        .bind(rule_id)
        .bind(start_at)
        .bind(end_at)
        .bind(library_id)
        .execute(&pool)
        .await
        .unwrap();

    sqlx::raw_sql(include_str!(
        "../migrations/0015_live_tv_series_airing_dedup.sql"
    ))
    .execute(&pool)
    .await
    .unwrap();

    let statuses: Vec<(Uuid, String)> = sqlx::query_as(
        "SELECT id,status FROM live_tv_timers WHERE series_timer_id=$1 ORDER BY status,id",
    )
    .bind(rule_id)
    .fetch_all(&pool)
    .await
    .unwrap();
    assert_eq!(statuses.len(), 3);
    assert_eq!(
        statuses
            .iter()
            .filter(|(_, status)| status == "recording")
            .count(),
        2,
        "migration must retain every already-claimed recording"
    );
    assert_eq!(
        statuses
            .iter()
            .filter(|(id, status)| *id == scheduled_timer_id && status == "cancelled")
            .count(),
        1,
        "queued duplicate must be cancelled when a recording already exists"
    );
    let recording_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM live_tv_recordings WHERE timer_id=ANY($1) AND status='recording'",
    )
    .bind(recording_timer_ids.as_slice())
    .fetch_one(&pool)
    .await
    .unwrap();
    assert_eq!(recording_count, 2, "recording references remain intact");

    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .execute(&admin_pool)
        .await
        .unwrap();
    admin_pool.close().await;
}

async fn insert_user(
    pool: &sqlx::PgPool,
    username: &str,
    is_admin: bool,
    restrict_libraries: bool,
    max_rating: Option<i16>,
    block_unrated: &[&str],
) -> Uuid {
    let id = Uuid::new_v4();
    let block_unrated = block_unrated
        .iter()
        .map(|value| (*value).to_owned())
        .collect::<Vec<_>>();
    sqlx::query(
        "INSERT INTO users(id,username,username_norm,password_hash,is_admin,enable_remote_access, \
         allow_media_playback,restrict_libraries,max_parental_rating,block_unrated_items) \
         VALUES($1,$2,$2,'unused',$3,TRUE,TRUE,$4,$5,$6)",
    )
    .bind(id)
    .bind(username)
    .bind(is_admin)
    .bind(restrict_libraries)
    .bind(max_rating)
    .bind(block_unrated)
    .execute(pool)
    .await
    .unwrap();
    id
}

fn xmltv_time(value: chrono::DateTime<Utc>) -> String {
    value.format("%Y%m%d%H%M%S +0000").to_string()
}

async fn serve_feeds(
    playlist: Vec<u8>,
    refreshed_playlist: Vec<u8>,
    omitted_channel_playlist: Vec<u8>,
    guide: Vec<u8>,
) -> (String, JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let address = listener.local_addr().unwrap();
    let origin = format!("http://{address}/");
    let task = tokio::spawn(async move {
        let mut revision_refresh_count = 0;
        for _ in 0..12 {
            let (mut socket, _) = listener.accept().await.unwrap();
            let mut request = Vec::new();
            let mut chunk = [0_u8; 2048];
            loop {
                let count = socket.read(&mut chunk).await.unwrap();
                if count == 0 {
                    break;
                }
                request.extend_from_slice(&chunk[..count]);
                if request.windows(4).any(|window| window == b"\r\n\r\n") {
                    break;
                }
                assert!(request.len() <= 8192, "fixture request headers are bounded");
            }
            let request_line = String::from_utf8_lossy(&request)
                .lines()
                .next()
                .unwrap_or_default()
                .to_owned();
            let body = if request_line.contains("/channels.m3u?revision=2") {
                let body = if revision_refresh_count == 0 {
                    &refreshed_playlist
                } else {
                    &omitted_channel_playlist
                };
                revision_refresh_count += 1;
                body
            } else if request_line.contains("/channels.m3u") {
                &playlist
            } else if request_line.contains("/guide.xml") {
                &guide
            } else {
                panic!("unexpected fixture request path: {request_line}");
            };
            let content_type = if request_line.contains("/channels.m3u") {
                "audio/x-mpegurl"
            } else {
                "application/xml"
            };
            let response = format!(
                "HTTP/1.1 200 OK\r\nContent-Type: {content_type}\r\nContent-Length: {}\r\nConnection: close\r\n\r\n",
                body.len()
            );
            socket.write_all(response.as_bytes()).await.unwrap();
            socket.write_all(body).await.unwrap();
        }
    });
    (origin, task)
}

fn channel_id_by_name(body: &Value, name: &str) -> Uuid {
    Uuid::parse_str(
        body["Items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["Name"] == name)
            .unwrap()["Id"]
            .as_str()
            .unwrap(),
    )
    .unwrap()
}

fn program_id_by_name(body: &Value, name: &str) -> Uuid {
    Uuid::parse_str(
        body["Items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| item["Name"] == name)
            .unwrap()["Id"]
            .as_str()
            .unwrap(),
    )
    .unwrap()
}

async fn wait_for_series_rule_lock_waiters(
    pool: &sqlx::PgPool,
    minimum: i64,
    task: Option<&mut JoinHandle<(StatusCode, Value)>>,
) {
    if timeout(Duration::from_secs(10), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pg_stat_activity \
                 WHERE datname=current_database() AND pid<>pg_backend_pid() \
                   AND wait_event_type='Lock' AND query LIKE '%FOR UPDATE%' \
                   AND (query LIKE '%live_tv_series_timers%' OR \
                        query LIKE '%FROM users u WHERE u.id=$1 FOR UPDATE OF u%')",
            )
            .fetch_one(pool)
            .await
            .unwrap();
            if waiting >= minimum {
                return;
            }
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok()
    {
        return;
    }

    let waiting = sqlx::query(
        "SELECT pid,wait_event_type,wait_event,state,query FROM pg_stat_activity \
         WHERE datname=current_database() AND pid<>pg_backend_pid() \
         ORDER BY pid",
    )
    .fetch_all(pool)
    .await
    .unwrap()
    .into_iter()
    .map(|row| {
        format!(
            "pid={} wait={:?}/{:?} state={:?} query={:?}",
            row.try_get::<i32, _>("pid").unwrap(),
            row.try_get::<Option<String>, _>("wait_event_type").unwrap(),
            row.try_get::<Option<String>, _>("wait_event").unwrap(),
            row.try_get::<Option<String>, _>("state").unwrap(),
            row.try_get::<Option<String>, _>("query").unwrap(),
        )
    })
    .collect::<Vec<_>>();
    let task_result = if let Some(task) = task {
        if task.is_finished() {
            format!("finished with {:?}", (&mut *task).await)
        } else {
            "still running".to_owned()
        }
    } else {
        "not provided".to_owned()
    };
    panic!(
        "expected at least {minimum} series-rule lock waiters; task={task_result}; pg_stat_activity={waiting:#?}"
    );
}

async fn wait_for_refresh_user_lock_waiter(
    pool: &sqlx::PgPool,
    task: &mut JoinHandle<(StatusCode, Value)>,
) -> String {
    let query = timeout(Duration::from_secs(10), async {
        loop {
            if let Some(query) = sqlx::query_scalar::<_, String>(
                "SELECT query FROM pg_stat_activity \
                 WHERE datname=current_database() AND pid<>pg_backend_pid() \
                   AND wait_event_type='Lock' \
                   AND query LIKE '%SELECT u.id FROM users u WHERE TRUE%' \
                   AND query LIKE '%ORDER BY u.id FOR UPDATE OF u%' LIMIT 1",
            )
            .fetch_optional(pool)
            .await
            .unwrap()
            {
                return query;
            }
            assert!(
                !task.is_finished(),
                "source refresh finished without waiting for the all-user lock"
            );
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await;
    match query {
        Ok(query) => query,
        Err(_) => {
            let waiting = sqlx::query(
                "SELECT pid,wait_event_type,wait_event,state,query FROM pg_stat_activity \
                 WHERE datname=current_database() AND pid<>pg_backend_pid() ORDER BY pid",
            )
            .fetch_all(pool)
            .await
            .unwrap()
            .into_iter()
            .map(|row| {
                format!(
                    "pid={} wait={:?}/{:?} state={:?} query={:?}",
                    row.try_get::<i32, _>("pid").unwrap(),
                    row.try_get::<Option<String>, _>("wait_event_type").unwrap(),
                    row.try_get::<Option<String>, _>("wait_event").unwrap(),
                    row.try_get::<Option<String>, _>("state").unwrap(),
                    row.try_get::<Option<String>, _>("query").unwrap(),
                )
            })
            .collect::<Vec<_>>();
            panic!("source refresh did not wait for the all-user lock: {waiting:#?}");
        }
    }
}

async fn wait_for_user_creation_advisory_waiter(
    pool: &sqlx::PgPool,
    task: &mut JoinHandle<Result<auth::UserRecord, sqlx::Error>>,
) {
    let found = timeout(Duration::from_secs(10), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pg_stat_activity \
                 WHERE datname=current_database() AND pid<>pg_backend_pid() \
                   AND wait_event_type='Lock' \
                   AND query LIKE '%pg_advisory_xact_lock(82473011)%'",
            )
            .fetch_one(pool)
            .await
            .unwrap();
            if waiting > 0 {
                return true;
            }
            assert!(
                !task.is_finished(),
                "non-admin user creation completed instead of waiting for refresh serialization"
            );
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();
    assert!(
        found,
        "non-admin user creation did not wait for the source-refresh advisory lock"
    );
}

async fn wait_for_timer_user_lock_waiter(
    pool: &sqlx::PgPool,
    task: &mut JoinHandle<(StatusCode, Value)>,
) {
    let found = timeout(Duration::from_secs(10), async {
        loop {
            let waiting: i64 = sqlx::query_scalar(
                "SELECT COUNT(*) FROM pg_stat_activity \
                 WHERE datname=current_database() AND pid<>pg_backend_pid() \
                   AND wait_event_type='Lock' AND query LIKE '%FROM users u WHERE u.id=$1 FOR UPDATE OF u%'",
            )
            .fetch_one(pool)
            .await
            .unwrap();
            if waiting > 0 {
                return true;
            }
            assert!(
                !task.is_finished(),
                "timer create finished without waiting for the user policy lock"
            );
            sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .is_ok();
    assert!(found, "timer create did not wait for the held user row");
}

async fn call_json(
    router: &axum::Router,
    method: &str,
    uri: &str,
    token: &str,
    body: Option<Value>,
) -> (StatusCode, Value) {
    let mut request = Request::builder()
        .method(method)
        .uri(uri)
        .header("X-Emby-Token", token);
    if body.is_some() {
        request = request.header("Content-Type", "application/json");
    }
    let body = body
        .map(|value| Body::from(serde_json::to_vec(&value).unwrap()))
        .unwrap_or_else(Body::empty);
    let response = router
        .clone()
        .oneshot(request.body(body).unwrap())
        .await
        .unwrap();
    let status = response.status();
    let bytes = axum::body::to_bytes(response.into_body(), 1024 * 1024)
        .await
        .unwrap();
    let value = if bytes.is_empty() {
        Value::Null
    } else {
        serde_json::from_slice(&bytes)
            .unwrap_or_else(|_| json!({ "body": String::from_utf8_lossy(&bytes) }))
    };
    (status, value)
}
