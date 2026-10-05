use std::{env, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use futures_util::StreamExt;
use puffinbox::{AppState, Config, api, auth, db};
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use tokio::{net::TcpStream, time::timeout};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Message, client::IntoClientRequest, http::StatusCode},
};
use uuid::Uuid;

mod common;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via PUFFINBOX_TEST_DATABASE_URL"]
async fn groups_isolate_sessions_and_enforce_current_policy_tokens_and_server_run() {
    let database_url = env::var("PUFFINBOX_TEST_DATABASE_URL").unwrap();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .unwrap();
    let schema = format!("puffinbox_syncplay_test_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
        .execute(&admin)
        .await
        .unwrap();
    let connection_schema = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .after_connect(move |connection, _| {
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
    let creator_id = Uuid::new_v4();
    let peer_id = Uuid::new_v4();
    let admin_id = Uuid::new_v4();
    for (id, name, access, is_admin) in [
        (creator_id, "group-creator", "CreateAndJoinGroups", false),
        (peer_id, "group-peer", "JoinGroups", false),
        (admin_id, "group-admin", "CreateAndJoinGroups", true),
    ] {
        sqlx::query("INSERT INTO users(id,username,username_norm,password_hash,enable_remote_access,sync_play_access,is_admin) VALUES($1,$2,$2,'unused',TRUE,$3,$4)")
            .bind(id).bind(name).bind(access).bind(is_admin).execute(&pool).await.unwrap();
    }
    let config = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        public_base_url: None,
        database_url,
        server_name: "SyncPlay test".to_owned(),
        web_root: PathBuf::from("web"),
        data_dir: env::temp_dir().join(format!("puffinbox-{schema}")),
        ffmpeg_path: None,
        max_scan_workers: 1,
        max_page_size: 100,
        access_token_lifetime_hours: 24,
        cookie_secure: false,
        cors_origins: Vec::new(),
        trusted_proxies: Vec::new(),
        local_networks: Vec::new(),
        dlna: Default::default(),
        setup_token: None,
        bootstrap_admin_username: None,
        bootstrap_admin_password: None,
    };
    let state = AppState::new_for_run(pool.clone(), Arc::new(config), Uuid::new_v4(), run_id, None);
    let creator = db::get_user(&pool, creator_id).await.unwrap().unwrap();
    let peer = db::get_user(&pool, peer_id).await.unwrap().unwrap();
    let administrator = db::get_user(&pool, admin_id).await.unwrap().unwrap();
    // Both tokens deliberately claim the same device. Group membership is
    // bound to the authenticated token session, never the claimed device ID.
    let first = auth::issue_token(&state, &creator, "test", "browser", "shared-device")
        .await
        .unwrap();
    let second = auth::issue_token(&state, &creator, "test", "browser", "shared-device")
        .await
        .unwrap();
    let peer_token = auth::issue_token(&state, &peer, "test", "browser", "peer-device")
        .await
        .unwrap();
    let admin_token = auth::issue_token(&state, &administrator, "test", "browser", "admin-device")
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind(state.config.bind)
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let base = format!("http://{address}");
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let router = api::router(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = stop_rx.await;
        })
        .await
        .unwrap();
    });
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let mut own = open(address, &first.token).await;
    let mut same_user = open(address, &second.token).await;
    let mut other = open(address, &peer_token.token).await;
    assert_eq!(
        http.get(format!("{base}/SyncPlay/List"))
            .send()
            .await
            .unwrap()
            .status(),
        StatusCode::UNAUTHORIZED
    );
    status(
        &http,
        &base,
        &first.token,
        "POST",
        "/SyncPlay/Leave",
        None,
        StatusCode::NO_CONTENT,
    )
    .await;
    update(&mut own, "NotInGroup", Uuid::nil(), json!("")).await;
    quiet(&mut same_user).await;
    quiet(&mut other).await;
    let missing = Uuid::new_v4();
    for body in [json!({}), json!({"GroupId":null})] {
        status(
            &http,
            &base,
            &first.token,
            "POST",
            "/SyncPlay/Join",
            Some(body),
            StatusCode::NO_CONTENT,
        )
        .await;
        update(&mut own, "GroupDoesNotExist", Uuid::nil(), json!("")).await;
    }
    status(
        &http,
        &base,
        &first.token,
        "POST",
        "/SyncPlay/Join",
        Some(json!({"GroupId":"invalid"})),
        StatusCode::BAD_REQUEST,
    )
    .await;
    status(
        &http,
        &base,
        &first.token,
        "GET",
        &format!("/SyncPlay/{missing}"),
        None,
        StatusCode::NOT_FOUND,
    )
    .await;
    status(
        &http,
        &base,
        &first.token,
        "POST",
        "/SyncPlay/Join",
        Some(json!({"GroupId":missing})),
        StatusCode::NO_CONTENT,
    )
    .await;
    update(&mut own, "GroupDoesNotExist", Uuid::nil(), json!("")).await;
    let created = request(
        &http,
        &base,
        &first.token,
        "POST",
        "/SyncPlay/New",
        Some(json!({"GroupName":"  Original group  "})),
    )
    .await;
    assert_eq!(created.status(), StatusCode::OK);
    let group: Value = created.json().await.unwrap();
    let id = Uuid::parse_str(group["GroupId"].as_str().unwrap()).unwrap();
    assert_eq!(group["GroupId"].as_str().unwrap().len(), 32);
    assert_eq!(group["GroupName"], "Original group");
    assert_eq!(group["State"], "Idle");
    assert_eq!(group["Participants"], json!(["group-creator"]));
    joined(&mut own, id, &["group-creator"]).await;
    quiet(&mut same_user).await;
    quiet(&mut other).await;
    status(
        &http,
        &base,
        &first.token,
        "POST",
        "/SyncPlay/New",
        Some(json!({"GroupName":"x".repeat(201)})),
        StatusCode::BAD_REQUEST,
    )
    .await;
    status(
        &http,
        &base,
        &peer_token.token,
        "POST",
        "/SyncPlay/New",
        Some(json!({})),
        StatusCode::FORBIDDEN,
    )
    .await;
    for (count, expected) in [(100, StatusCode::OK), (101, StatusCode::BAD_REQUEST)] {
        status(
            &http,
            &base,
            &admin_token.token,
            "POST",
            "/SyncPlay/New",
            Some(json!({"GroupName":"🐧".repeat(count)})),
            expected,
        )
        .await;
        if expected == StatusCode::OK {
            status(
                &http,
                &base,
                &admin_token.token,
                "POST",
                "/SyncPlay/Leave",
                None,
                StatusCode::NO_CONTENT,
            )
            .await;
        }
    }
    for _ in 0..2 {
        status(
            &http,
            &base,
            &second.token,
            "POST",
            "/SyncPlay/Join",
            Some(json!({"GroupId":id})),
            StatusCode::NO_CONTENT,
        )
        .await;
        update(&mut own, "UserJoined", id, json!("group-creator")).await;
        joined(&mut same_user, id, &["group-creator"]).await;
        quiet(&mut other).await;
    }
    status(
        &http,
        &base,
        &peer_token.token,
        "POST",
        "/SyncPlay/Join",
        Some(json!({"GroupId":id})),
        StatusCode::NO_CONTENT,
    )
    .await;
    update(&mut own, "UserJoined", id, json!("group-peer")).await;
    update(&mut same_user, "UserJoined", id, json!("group-peer")).await;
    joined(&mut other, id, &["group-creator", "group-peer"]).await;
    let policy_path = format!("/Users/{peer_id}/Policy");
    status(
        &http,
        &base,
        &peer_token.token,
        "POST",
        &policy_path,
        Some(json!({"SyncPlayAccess":"CreateAndJoinGroups"})),
        StatusCode::FORBIDDEN,
    )
    .await;
    status(
        &http,
        &base,
        &admin_token.token,
        "POST",
        &policy_path,
        Some(json!({"SyncPlayAccess":"None"})),
        StatusCode::NO_CONTENT,
    )
    .await;
    let peer_dto: Value = request(
        &http,
        &base,
        &admin_token.token,
        "GET",
        &format!("/Users/{peer_id}"),
        None,
    )
    .await
    .json()
    .await
    .unwrap();
    assert_eq!(peer_dto["Policy"]["SyncPlayAccess"], "None");
    let groups: Value = request(&http, &base, &first.token, "GET", "/SyncPlay/List", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(groups[0]["Participants"], json!(["group-creator"]));
    update(&mut own, "UserLeft", id, json!("group-peer")).await;
    update(&mut same_user, "UserLeft", id, json!("group-peer")).await;
    update(&mut other, "GroupLeft", id, json!(id.to_string())).await;
    for (method, path, body) in [
        ("GET", "/SyncPlay/List".to_owned(), None),
        ("GET", format!("/SyncPlay/{id}"), None),
        (
            "POST",
            "/SyncPlay/Join".to_owned(),
            Some(json!({"GroupId":id})),
        ),
        ("POST", "/SyncPlay/New".to_owned(), Some(json!({}))),
    ] {
        status(
            &http,
            &base,
            &peer_token.token,
            method,
            &path,
            body,
            StatusCode::FORBIDDEN,
        )
        .await;
    }
    quiet(&mut other).await;
    status(
        &http,
        &base,
        &admin_token.token,
        "POST",
        &policy_path,
        Some(json!({"SyncPlayAccess":"JoinGroups"})),
        StatusCode::NO_CONTENT,
    )
    .await;
    status(
        &http,
        &base,
        &peer_token.token,
        "POST",
        "/SyncPlay/Join",
        Some(json!({"GroupId":id})),
        StatusCode::NO_CONTENT,
    )
    .await;
    update(&mut own, "UserJoined", id, json!("group-peer")).await;
    update(&mut same_user, "UserJoined", id, json!("group-peer")).await;
    joined(&mut other, id, &["group-creator", "group-peer"]).await;
    sqlx::query("UPDATE users SET disabled=TRUE WHERE id=$1")
        .bind(peer_id)
        .execute(&pool)
        .await
        .unwrap();
    status(
        &http,
        &base,
        &first.token,
        "GET",
        "/SyncPlay/List",
        None,
        StatusCode::OK,
    )
    .await;
    update(&mut own, "UserLeft", id, json!("group-peer")).await;
    update(&mut same_user, "UserLeft", id, json!("group-peer")).await;
    closed(&mut other).await;
    status(
        &http,
        &base,
        &peer_token.token,
        "GET",
        "/SyncPlay/List",
        None,
        StatusCode::UNAUTHORIZED,
    )
    .await;
    db::revoke_auth_token(
        &pool,
        run_id,
        &auth::token_digest(&second.token),
        creator_id,
    )
    .await
    .unwrap();
    status(
        &http,
        &base,
        &first.token,
        "GET",
        "/SyncPlay/List",
        None,
        StatusCode::OK,
    )
    .await;
    update(&mut own, "UserLeft", id, json!("group-creator")).await;
    closed(&mut same_user).await;
    status(
        &http,
        &base,
        &first.token,
        "POST",
        "/SyncPlay/Leave",
        None,
        StatusCode::NO_CONTENT,
    )
    .await;
    update(&mut own, "GroupLeft", id, json!(id.to_string())).await;
    let empty: Value = request(&http, &base, &first.token, "GET", "/SyncPlay/List", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(empty, json!([]));

    let mut policy_change = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(82473011)")
        .execute(&mut *policy_change)
        .await
        .unwrap();
    let waiting_http = http.clone();
    let waiting_base = base.clone();
    let waiting_token = first.token.clone();
    let mut waiting = tokio::spawn(async move {
        request(
            &waiting_http,
            &waiting_base,
            &waiting_token,
            "GET",
            "/SyncPlay/List",
            None,
        )
        .await
        .status()
    });
    assert!(
        timeout(Duration::from_millis(30), &mut waiting)
            .await
            .is_err()
    );
    sqlx::query("UPDATE users SET sync_play_access='None' WHERE id=$1")
        .bind(creator_id)
        .execute(&mut *policy_change)
        .await
        .unwrap();
    policy_change.commit().await.unwrap();
    assert_eq!(
        timeout(Duration::from_secs(3), waiting)
            .await
            .unwrap()
            .unwrap(),
        StatusCode::FORBIDDEN
    );
    status(
        &http,
        &base,
        &admin_token.token,
        "POST",
        &format!("/Users/{creator_id}/Policy"),
        Some(json!({"SyncPlayAccess":"CreateAndJoinGroups"})),
        StatusCode::NO_CONTENT,
    )
    .await;

    own.close(None).await.unwrap();
    timeout(Duration::from_secs(3), async {
        loop {
            match own.next().await {
                Some(Ok(Message::Close(_))) | Some(Err(_)) | None => break,
                _ => {}
            }
        }
    })
    .await
    .expect("socket did not finish closing");
    status(
        &http,
        &base,
        &first.token,
        "GET",
        "/SyncPlay/List",
        None,
        StatusCode::OK,
    )
    .await;
    let expiring_wait = auth::issue_token(&state, &creator, "test", "browser", "expiring-wait")
        .await
        .unwrap();
    let mut expiration_guard = pool.begin().await.unwrap();
    sqlx::query("SELECT pg_advisory_xact_lock(82473011)")
        .execute(&mut *expiration_guard)
        .await
        .unwrap();
    let waiting_http = http.clone();
    let waiting_base = base.clone();
    let waiting_token = expiring_wait.token.clone();
    let waiting = tokio::spawn(async move {
        request(
            &waiting_http,
            &waiting_base,
            &waiting_token,
            "POST",
            "/SyncPlay/New",
            Some(json!({"GroupName":"Expired wait"})),
        )
        .await
        .status()
    });
    timeout(Duration::from_secs(3), async {
        loop {
            let blocked: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM pg_stat_activity WHERE datname=current_database() AND query='SELECT pg_advisory_xact_lock_shared(82473011)' AND wait_event_type='Lock')").fetch_one(&pool).await.unwrap();
            if blocked { break; }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    }).await.expect("group request did not reach the policy guard");
    sqlx::query("UPDATE auth_tokens SET expires_at=clock_timestamp() WHERE id=$1")
        .bind(expiring_wait.token_id)
        .execute(&pool)
        .await
        .unwrap();
    expiration_guard.commit().await.unwrap();
    assert_eq!(
        timeout(Duration::from_secs(3), waiting)
            .await
            .unwrap()
            .unwrap(),
        StatusCode::UNAUTHORIZED
    );
    let empty: Value = request(&http, &base, &first.token, "GET", "/SyncPlay/List", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(empty, json!([]));
    // Expired sessions disappear on the next authorized read, even without
    // an open socket or an explicit leave request.
    let expiring = auth::issue_token(&state, &creator, "test", "browser", "expiring")
        .await
        .unwrap();
    status(
        &http,
        &base,
        &expiring.token,
        "POST",
        "/SyncPlay/New",
        Some(json!({})),
        StatusCode::OK,
    )
    .await;
    sqlx::query("UPDATE auth_tokens SET expires_at=NOW()-INTERVAL '1 second' WHERE id=$1")
        .bind(expiring.token_id)
        .execute(&pool)
        .await
        .unwrap();
    let empty: Value = request(&http, &base, &first.token, "GET", "/SyncPlay/List", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(empty, json!([]));
    status(
        &http,
        &base,
        &expiring.token,
        "GET",
        "/SyncPlay/List",
        None,
        StatusCode::UNAUTHORIZED,
    )
    .await;

    // A per-user limit cannot be bypassed using new sessions or device IDs.
    let mut bounded = Vec::new();
    for index in 0..4 {
        let token = auth::issue_token(
            &state,
            &creator,
            "test",
            "browser",
            &format!("bounded-{index}"),
        )
        .await
        .unwrap();
        status(
            &http,
            &base,
            &token.token,
            "POST",
            "/SyncPlay/New",
            Some(json!({})),
            StatusCode::OK,
        )
        .await;
        bounded.push(token);
    }
    status(
        &http,
        &base,
        &first.token,
        "POST",
        "/SyncPlay/New",
        Some(json!({})),
        StatusCode::TOO_MANY_REQUESTS,
    )
    .await;
    let before: Value = request(&http, &base, &first.token, "GET", "/SyncPlay/List", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(before.as_array().unwrap().len(), 4);
    assert!(
        db::set_active_run_marker(&pool, Uuid::new_v4())
            .await
            .unwrap()
    );
    status(
        &http,
        &base,
        &first.token,
        "POST",
        "/SyncPlay/Join",
        Some(json!({"GroupId":before[0]["GroupId"]})),
        StatusCode::SERVICE_UNAVAILABLE,
    )
    .await;
    assert!(db::set_active_run_marker(&pool, run_id).await.unwrap());
    let after: Value = request(&http, &base, &first.token, "GET", "/SyncPlay/List", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(
        after.as_array().unwrap().len(),
        before.as_array().unwrap().len()
    );
    for (old, current) in before
        .as_array()
        .unwrap()
        .iter()
        .zip(after.as_array().unwrap())
    {
        for field in ["GroupId", "GroupName", "State", "Participants"] {
            assert_eq!(current[field], old[field]);
        }
        let previous_time =
            chrono::DateTime::parse_from_rfc3339(old["LastUpdatedAt"].as_str().unwrap()).unwrap();
        let current_time =
            chrono::DateTime::parse_from_rfc3339(current["LastUpdatedAt"].as_str().unwrap())
                .unwrap();
        assert!(
            current_time > previous_time,
            "group DTO timestamps describe each response's creation"
        );
    }
    for token in bounded {
        status(
            &http,
            &base,
            &token.token,
            "POST",
            "/SyncPlay/Leave",
            None,
            StatusCode::NO_CONTENT,
        )
        .await;
    }
    for body in [
        json!({"SyncPlayAccess":"None"}),
        json!({"SyncPlayAccess":"CreateAndJoinGroups","EnableMediaPlayback":false}),
    ] {
        status(
            &http,
            &base,
            &admin_token.token,
            "POST",
            &format!("/Users/{admin_id}/Policy"),
            Some(body),
            StatusCode::NO_CONTENT,
        )
        .await;
        status(
            &http,
            &base,
            &admin_token.token,
            "POST",
            "/SyncPlay/New",
            Some(json!({})),
            StatusCode::FORBIDDEN,
        )
        .await;
    }
    assert!(state.drain_user_sockets().await);
    stop_tx.send(()).unwrap();
    timeout(Duration::from_secs(3), server)
        .await
        .unwrap()
        .unwrap();
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}

async fn request(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
) -> reqwest::Response {
    let mut request = http
        .request(method.parse().unwrap(), format!("{base}{path}"))
        .header("x-emby-token", token);
    if let Some(body) = body {
        request = request.json(&body);
    }
    request.send().await.expect("fixture HTTP request failed")
}

async fn status(
    http: &reqwest::Client,
    base: &str,
    token: &str,
    method: &str,
    path: &str,
    body: Option<Value>,
    expected: StatusCode,
) {
    assert_eq!(
        request(http, base, token, method, path, body)
            .await
            .status(),
        expected,
        "{method} {path}"
    );
}

async fn open(address: SocketAddr, token: &str) -> Socket {
    let mut request = format!("ws://{address}/socket")
        .into_client_request()
        .unwrap();
    request
        .headers_mut()
        .insert("x-emby-token", token.parse().unwrap());
    let (mut socket, response) = connect_async(request).await.expect("fixture socket failed");
    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
    assert_eq!(message(&mut socket).await["MessageType"], "ForceKeepAlive");
    socket
}

async fn message(socket: &mut Socket) -> Value {
    let next = timeout(Duration::from_secs(3), socket.next())
        .await
        .expect("SyncPlay event timed out")
        .unwrap()
        .unwrap();
    let Message::Text(text) = next else {
        panic!("expected text event");
    };
    let value: Value = serde_json::from_str(&text).unwrap();
    assert!(Uuid::parse_str(value["MessageId"].as_str().unwrap()).is_ok());
    value
}

async fn update(socket: &mut Socket, kind: &str, id: Uuid, data: Value) {
    let event = message(socket).await;
    assert_eq!(event["MessageType"], "SyncPlayGroupUpdate");
    assert_eq!(
        event["Data"],
        json!({"GroupId":id.simple().to_string(),"Type":kind,"Data":data})
    );
}

async fn joined(socket: &mut Socket, id: Uuid, participants: &[&str]) {
    let event = message(socket).await;
    assert_eq!(event["MessageType"], "SyncPlayGroupUpdate");
    assert_eq!(event["Data"]["Type"], "GroupJoined");
    assert_eq!(event["Data"]["GroupId"], id.simple().to_string());
    assert_eq!(event["Data"]["Data"]["Participants"], json!(participants));
    assert_eq!(event["Data"]["Data"]["State"], "Idle");
    let command = message(socket).await;
    assert_eq!(command["MessageType"], "SyncPlayCommand");
    assert_eq!(command["Data"]["GroupId"], id.simple().to_string());
    assert_eq!(command["Data"]["Command"], "Stop");
    assert_eq!(
        command["Data"]["PlaylistItemId"],
        Uuid::nil().simple().to_string()
    );
    assert_eq!(command["Data"]["PositionTicks"], 0);
    for field in ["When", "EmittedAt"] {
        assert!(
            chrono::DateTime::parse_from_rfc3339(command["Data"][field].as_str().unwrap()).is_ok()
        );
    }
}

async fn quiet(socket: &mut Socket) {
    assert!(
        timeout(Duration::from_millis(100), socket.next())
            .await
            .is_err(),
        "unrelated session received a group event"
    );
}

async fn closed(socket: &mut Socket) {
    timeout(Duration::from_secs(7), async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Close(Some(frame)))) => {
                    assert_eq!(u16::from(frame.code), 1008);
                    break;
                }
                Some(Ok(Message::Text(text))) => {
                    assert_ne!(
                        serde_json::from_str::<Value>(&text).unwrap()["MessageType"],
                        "SyncPlayCommand"
                    );
                }
                _ => panic!("session ended without expected close"),
            }
        }
    })
    .await
    .expect("unavailable session stayed open");
    let _ = socket.close(None).await;
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via PUFFINBOX_TEST_DATABASE_URL"]
async fn queues_coordinate_playback_and_recheck_every_participants_media_access() {
    let database_url = env::var("PUFFINBOX_TEST_DATABASE_URL").unwrap();
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .connect(&database_url)
        .await
        .unwrap();
    let schema = format!("puffinbox_syncplay_queue_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
        .execute(&admin)
        .await
        .unwrap();
    let connection_schema = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(8)
        .after_connect(move |connection, _| {
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
    let creator = Uuid::new_v4();
    let peer = Uuid::new_v4();
    let outsider = Uuid::new_v4();
    for (id, name) in [
        (creator, "queue-owner"),
        (peer, "queue-peer"),
        (outsider, "queue-outsider"),
    ] {
        sqlx::query("INSERT INTO users(id,username,username_norm,password_hash,enable_remote_access,max_parental_rating) VALUES($1,$2,$2,'unused',TRUE,50)").bind(id).bind(name).execute(&pool).await.unwrap();
    }
    let allowed_library = Uuid::new_v4();
    let private_library = Uuid::new_v4();
    for id in [allowed_library, private_library] {
        sqlx::query("INSERT INTO libraries(id,name,collection_type,locations) VALUES($1,$2,'tvshows','[\"/synthetic-media\"]')").bind(id).bind(format!("Queue library {id}")).execute(&pool).await.unwrap();
    }
    sqlx::query("UPDATE users SET restrict_libraries=TRUE WHERE id IN ($1,$2)")
        .bind(peer)
        .bind(outsider)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO user_library_access(user_id,library_id) VALUES($1,$3),($2,$4)")
        .bind(peer)
        .bind(outsider)
        .bind(allowed_library)
        .bind(private_library)
        .execute(&pool)
        .await
        .unwrap();
    let root = Uuid::new_v4();
    let first_item = Uuid::new_v4();
    let second_item = Uuid::new_v4();
    let private_item = Uuid::new_v4();
    let adult_parent = Uuid::new_v4();
    let adult_episode = Uuid::new_v4();
    let hidden_item = Uuid::new_v4();
    for (id, library, parent, kind, name, path) in [
        (
            root,
            allowed_library,
            None,
            "Series",
            "Queue series",
            "/synthetic-media/Queue",
        ),
        (
            first_item,
            allowed_library,
            Some(root),
            "Episode",
            "First",
            "/synthetic-media/Queue/first.mp4",
        ),
        (
            second_item,
            allowed_library,
            Some(root),
            "Episode",
            "Second",
            "/synthetic-media/Queue/second.mp4",
        ),
        (
            private_item,
            private_library,
            None,
            "Movie",
            "Private",
            "/synthetic-media/private.mp4",
        ),
        (
            adult_parent,
            allowed_library,
            None,
            "Series",
            "Adult",
            "/synthetic-media/Adult",
        ),
        (
            adult_episode,
            allowed_library,
            Some(adult_parent),
            "Episode",
            "Adult child",
            "/synthetic-media/Adult/child.mp4",
        ),
        (
            hidden_item,
            allowed_library,
            None,
            "Movie",
            "Hidden",
            "/synthetic-media/.hidden.mp4",
        ),
    ] {
        sqlx::query("INSERT INTO items(id,library_id,parent_id,name,sort_name,item_type,path,path_hash,runtime_ticks) VALUES($1,$2,$3,$4,$4,$5,$6,$7,3000000000)").bind(id).bind(library).bind(parent).bind(name).bind(kind).bind(path).bind(id.simple().to_string()).execute(&pool).await.unwrap();
        sqlx::query("INSERT INTO item_metadata(item_id,provider_key,policy_rating_scale,policy_rating_value) VALUES($1,'local-nfo','US-MPAA-v1',$2)").bind(id).bind(if id == adult_parent { 90i16 } else { 20i16 }).execute(&pool).await.unwrap();
    }
    let config = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        public_base_url: None,
        database_url,
        server_name: "SyncPlay queue test".to_owned(),
        web_root: PathBuf::from("web"),
        data_dir: env::temp_dir().join(format!("puffinbox-{schema}")),
        ffmpeg_path: None,
        max_scan_workers: 1,
        max_page_size: 100,
        access_token_lifetime_hours: 24,
        cookie_secure: false,
        cors_origins: Vec::new(),
        trusted_proxies: Vec::new(),
        local_networks: Vec::new(),
        dlna: Default::default(),
        setup_token: None,
        bootstrap_admin_username: None,
        bootstrap_admin_password: None,
    };
    let state = AppState::new_for_run(pool.clone(), Arc::new(config), Uuid::new_v4(), run_id, None);
    let mut tokens = Vec::new();
    for user in [creator, peer, outsider] {
        let user = db::get_user(&pool, user).await.unwrap().unwrap();
        tokens.push(
            auth::issue_token(&state, &user, "test", "browser", "same-device")
                .await
                .unwrap()
                .token,
        );
    }
    let listener = tokio::net::TcpListener::bind(state.config.bind)
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let base = format!("http://{address}");
    let (stop_tx, stop_rx) = tokio::sync::oneshot::channel();
    let router = api::router(state.clone());
    let server = tokio::spawn(async move {
        axum::serve(
            listener,
            router.into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async {
            let _ = stop_rx.await;
        })
        .await
        .unwrap();
    });
    let http = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()
        .unwrap();
    let mut own = open(address, &tokens[0]).await;
    let mut other = open(address, &tokens[1]).await;
    let mut unrelated = open(address, &tokens[2]).await;
    for path in ["/GetUtcTime", "/GetUTCTime"] {
        let before = chrono::Utc::now();
        let response = http.get(format!("{base}{path}")).send().await.unwrap();
        assert_eq!(response.status(), StatusCode::OK);
        let clock: Value = response.json().await.unwrap();
        let received =
            chrono::DateTime::parse_from_rfc3339(clock["RequestReceptionTime"].as_str().unwrap())
                .unwrap();
        let sent = chrono::DateTime::parse_from_rfc3339(
            clock["ResponseTransmissionTime"].as_str().unwrap(),
        )
        .unwrap();
        assert!(received >= before && received <= sent && sent <= chrono::Utc::now());
    }
    for (path, body) in [
        ("/SyncPlay/SetRepeatMode", json!({"Mode":"Unknown"})),
        ("/SyncPlay/SetShuffleMode", json!({"Mode":"Unknown"})),
        ("/SyncPlay/Ping", json!({"Ping":-1})),
        ("/SyncPlay/Ping", json!({"Ping":60001})),
        ("/SyncPlay/Seek", json!({"PositionTicks":-1})),
        ("/SyncPlay/Ready", json!({"PositionTicks":-1})),
        (
            "/SyncPlay/SetPlaylistItem",
            json!({"PlaylistItemId":"malformed"}),
        ),
    ] {
        status(
            &http,
            &base,
            &tokens[0],
            "POST",
            path,
            Some(body),
            StatusCode::BAD_REQUEST,
        )
        .await;
    }
    let group: Value = request(
        &http,
        &base,
        &tokens[0],
        "POST",
        "/SyncPlay/New",
        Some(json!({"GroupName":"Queue review"})),
    )
    .await
    .json()
    .await
    .unwrap();
    let id = Uuid::parse_str(group["GroupId"].as_str().unwrap()).unwrap();
    joined(&mut own, id, &["queue-owner"]).await;
    status(
        &http,
        &base,
        &tokens[1],
        "POST",
        "/SyncPlay/Join",
        Some(json!({"GroupId":id})),
        StatusCode::NO_CONTENT,
    )
    .await;
    update(&mut own, "UserJoined", id, json!("queue-peer")).await;
    joined(&mut other, id, &["queue-owner", "queue-peer"]).await;
    quiet(&mut unrelated).await;
    let mut duplicate = open(address, &tokens[0]).await;
    duplicate.close(None).await.unwrap();
    status(
        &http,
        &base,
        &tokens[0],
        "GET",
        &format!("/SyncPlay/{id}"),
        None,
        StatusCode::OK,
    )
    .await;
    quiet(&mut own).await;
    quiet(&mut other).await;

    for item in [private_item, adult_episode, hidden_item, Uuid::new_v4()] {
        status(
            &http,
            &base,
            &tokens[0],
            "POST",
            "/SyncPlay/SetNewQueue",
            Some(json!({"PlayingQueue":[item],"PlayingItemPosition":0})),
            StatusCode::FORBIDDEN,
        )
        .await;
        quiet(&mut own).await;
        quiet(&mut other).await;
    }
    for (body, expected) in [
        (
            json!({"PlayingQueue":[first_item],"StartPositionTicks":-1}),
            StatusCode::BAD_REQUEST,
        ),
        (
            json!({"PlayingQueue":[first_item],"PlayingItemPosition":99}),
            StatusCode::NO_CONTENT,
        ),
        (
            json!({"PlayingQueue":[first_item],"PlayingItemPosition":-1}),
            StatusCode::NO_CONTENT,
        ),
        (json!({"PlayingQueue":[]}), StatusCode::NO_CONTENT),
        (
            json!({"PlayingQueue":vec![first_item;513]}),
            StatusCode::BAD_REQUEST,
        ),
    ] {
        status(
            &http,
            &base,
            &tokens[0],
            "POST",
            "/SyncPlay/SetNewQueue",
            Some(body),
            expected,
        )
        .await;
    }
    quiet(&mut own).await;
    quiet(&mut other).await;
    status(&http, &base, &tokens[0], "POST", "/SyncPlay/SetNewQueue", Some(json!({"playingQueue":[first_item,second_item,first_item],"playingItemPosition":0,"startPositionTicks":50000000})), StatusCode::NO_CONTENT).await;
    let mut queue = same_queue(&mut own, &mut other, id, "NewPlaylist").await;
    assert_eq!(queue["PlayingItemIndex"], 0);
    assert_eq!(queue["StartPositionTicks"], 50000000);
    assert_eq!(queue["IsPlaying"], false);
    assert_eq!(queue["RepeatMode"], "RepeatNone");
    assert_eq!(queue["ShuffleMode"], "Sorted");
    let playlist = queue["Playlist"].as_array().unwrap();
    assert_eq!(playlist.len(), 3);
    assert_eq!(playlist[0]["ItemId"], first_item.simple().to_string());
    assert_eq!(playlist[0]["ItemId"], playlist[2]["ItemId"]);
    assert_ne!(playlist[0]["PlaylistItemId"], playlist[2]["PlaylistItemId"]);
    let playing = playlist[0]["PlaylistItemId"].clone();
    let ready = json!({"When":chrono::Utc::now(),"PositionTicks":50000000,"IsPlaying":false,"PlaylistItemId":playing});
    status(
        &http,
        &base,
        &tokens[0],
        "POST",
        "/SyncPlay/Ready",
        Some(ready.clone()),
        StatusCode::NO_CONTENT,
    )
    .await;
    command(&mut own, id, "Pause").await;
    quiet(&mut other).await;
    status(
        &http,
        &base,
        &tokens[1],
        "POST",
        "/SyncPlay/Ready",
        Some(ready.clone()),
        StatusCode::NO_CONTENT,
    )
    .await;
    let scheduled = command(&mut own, id, "Unpause").await;
    let peer_scheduled = command(&mut other, id, "Unpause").await;
    assert_eq!(scheduled, peer_scheduled);
    let when = chrono::DateTime::parse_from_rfc3339(scheduled["When"].as_str().unwrap()).unwrap();
    let emitted =
        chrono::DateTime::parse_from_rfc3339(scheduled["EmittedAt"].as_str().unwrap()).unwrap();
    assert!((900..=1000).contains(&when.signed_duration_since(emitted).num_milliseconds()));
    for socket in [&mut own, &mut other] {
        update(
            socket,
            "StateUpdate",
            id,
            json!({"State":"Playing","Reason":"Ready"}),
        )
        .await;
    }
    status(
        &http,
        &base,
        &tokens[0],
        "POST",
        "/SyncPlay/Ready",
        Some(ready),
        StatusCode::NO_CONTENT,
    )
    .await;
    command(&mut own, id, "Unpause").await;
    quiet(&mut other).await;
    status(
        &http,
        &base,
        &tokens[0],
        "POST",
        "/SyncPlay/Pause",
        None,
        StatusCode::NO_CONTENT,
    )
    .await;
    for socket in [&mut own, &mut other] {
        let paused = command(socket, id, "Pause").await;
        assert!(paused["PositionTicks"].as_i64().unwrap() >= 50000000);
        update(
            socket,
            "StateUpdate",
            id,
            json!({"State":"Paused","Reason":"Pause"}),
        )
        .await;
    }
    status(
        &http,
        &base,
        &tokens[0],
        "POST",
        "/SyncPlay/Ping",
        Some(json!({"Ping":750})),
        StatusCode::NO_CONTENT,
    )
    .await;
    status(
        &http,
        &base,
        &tokens[0],
        "POST",
        "/SyncPlay/Unpause",
        None,
        StatusCode::NO_CONTENT,
    )
    .await;
    for socket in [&mut own, &mut other] {
        let unpause = command(socket, id, "Unpause").await;
        let when = chrono::DateTime::parse_from_rfc3339(unpause["When"].as_str().unwrap()).unwrap();
        let emitted =
            chrono::DateTime::parse_from_rfc3339(unpause["EmittedAt"].as_str().unwrap()).unwrap();
        assert!(when.signed_duration_since(emitted).num_milliseconds() >= 1400);
        update(
            socket,
            "StateUpdate",
            id,
            json!({"State":"Playing","Reason":"Unpause"}),
        )
        .await;
    }
    status(&http, &base, &tokens[0], "POST", "/SyncPlay/Buffering", Some(json!({"When":chrono::Utc::now(),"PositionTicks":50000000,"IsPlaying":false,"PlaylistItemId":playing})), StatusCode::NO_CONTENT).await;
    update(
        &mut own,
        "StateUpdate",
        id,
        json!({"State":"Waiting","Reason":"Buffer"}),
    )
    .await;
    let paused = command(&mut other, id, "Pause").await;
    update(
        &mut other,
        "StateUpdate",
        id,
        json!({"State":"Waiting","Reason":"Buffer"}),
    )
    .await;
    status(&http, &base, &tokens[0], "POST", "/SyncPlay/Ready", Some(json!({"When":chrono::Utc::now(),"PositionTicks":paused["PositionTicks"],"IsPlaying":false,"PlaylistItemId":playing})), StatusCode::NO_CONTENT).await;
    for socket in [&mut own, &mut other] {
        command(socket, id, "Unpause").await;
        update(
            socket,
            "StateUpdate",
            id,
            json!({"State":"Playing","Reason":"Ready"}),
        )
        .await;
    }
    status(
        &http,
        &base,
        &tokens[0],
        "POST",
        "/SyncPlay/Seek",
        Some(json!({"PositionTicks":100000000})),
        StatusCode::NO_CONTENT,
    )
    .await;
    for socket in [&mut own, &mut other] {
        let seek = command(socket, id, "Seek").await;
        assert_eq!(seek["PositionTicks"], 100000000);
        update(
            socket,
            "StateUpdate",
            id,
            json!({"State":"Waiting","Reason":"Seek"}),
        )
        .await;
    }
    status(
        &http,
        &base,
        &tokens[0],
        "POST",
        "/SyncPlay/Ready",
        Some(json!({"PositionTicks":107000000,"PlaylistItemId":playing})),
        StatusCode::NO_CONTENT,
    )
    .await;
    command(&mut own, id, "Seek").await;
    for socket in [&mut own, &mut other] {
        update(
            socket,
            "StateUpdate",
            id,
            json!({"State":"Waiting","Reason":"Ready"}),
        )
        .await;
    }
    status(
        &http,
        &base,
        &tokens[0],
        "POST",
        "/SyncPlay/NextItem",
        Some(json!({"PlaylistItemId":Uuid::new_v4()})),
        StatusCode::NO_CONTENT,
    )
    .await;
    quiet(&mut own).await;
    quiet(&mut other).await;
    status(
        &http,
        &base,
        &tokens[0],
        "POST",
        "/SyncPlay/NextItem",
        Some(json!({"PlaylistItemId":playing})),
        StatusCode::NO_CONTENT,
    )
    .await;
    queue = same_queue(&mut own, &mut other, id, "NextItem").await;
    assert_eq!(queue["PlayingItemIndex"], 1);
    assert_eq!(queue["StartPositionTicks"], 0);
    let playing = queue["Playlist"][1]["PlaylistItemId"].clone();
    status(
        &http,
        &base,
        &tokens[0],
        "POST",
        "/SyncPlay/PreviousItem",
        Some(json!({"PlaylistItemId":playing})),
        StatusCode::NO_CONTENT,
    )
    .await;
    queue = same_queue(&mut own, &mut other, id, "PreviousItem").await;
    assert_eq!(queue["PlayingItemIndex"], 0);
    let last = queue["Playlist"][2]["PlaylistItemId"].clone();
    status(
        &http,
        &base,
        &tokens[0],
        "POST",
        "/SyncPlay/SetPlaylistItem",
        Some(json!({"PlaylistItemId":last})),
        StatusCode::NO_CONTENT,
    )
    .await;
    queue = same_queue(&mut own, &mut other, id, "SetCurrentItem").await;
    assert_eq!(queue["PlayingItemIndex"], 2);
    for (mode, reason) in [("Queue", "Queue"), ("QueueNext", "QueueNext")] {
        status(
            &http,
            &base,
            &tokens[0],
            "POST",
            "/SyncPlay/Queue",
            Some(json!({"ItemIds":[second_item],"Mode":mode})),
            StatusCode::NO_CONTENT,
        )
        .await;
        queue = same_queue(&mut own, &mut other, id, reason).await;
    }
    assert_eq!(queue["Playlist"].as_array().unwrap().len(), 5);
    assert_eq!(
        queue["Playlist"][3]["ItemId"],
        second_item.simple().to_string()
    );
    let move_id = queue["Playlist"][4]["PlaylistItemId"].clone();
    status(
        &http,
        &base,
        &tokens[0],
        "POST",
        "/SyncPlay/MovePlaylistItem",
        Some(json!({"PlaylistItemId":move_id,"NewIndex":0})),
        StatusCode::NO_CONTENT,
    )
    .await;
    queue = same_queue(&mut own, &mut other, id, "MoveItem").await;
    assert_eq!(queue["PlayingItemIndex"], 3);
    let selected = queue["Playlist"][3]["PlaylistItemId"].clone();
    let custom_order = queue["Playlist"].clone();
    status(
        &http,
        &base,
        &tokens[0],
        "POST",
        "/SyncPlay/SetShuffleMode",
        Some(json!({"Mode":"Shuffle"})),
        StatusCode::NO_CONTENT,
    )
    .await;
    queue = same_queue(&mut own, &mut other, id, "ShuffleMode").await;
    assert_eq!(queue["PlayingItemIndex"], 0);
    assert_eq!(
        queue["Playlist"][queue["PlayingItemIndex"].as_u64().unwrap() as usize]["PlaylistItemId"],
        selected
    );
    status(
        &http,
        &base,
        &tokens[0],
        "POST",
        "/SyncPlay/SetShuffleMode",
        Some(json!({"Mode":"Sorted"})),
        StatusCode::NO_CONTENT,
    )
    .await;
    queue = same_queue(&mut own, &mut other, id, "ShuffleMode").await;
    assert_eq!(queue["Playlist"], custom_order);
    assert_eq!(
        queue["Playlist"][queue["PlayingItemIndex"].as_u64().unwrap() as usize]["PlaylistItemId"],
        selected
    );
    status(
        &http,
        &base,
        &tokens[0],
        "POST",
        "/SyncPlay/SetRepeatMode",
        Some(json!({"Mode":"RepeatOne"})),
        StatusCode::NO_CONTENT,
    )
    .await;
    same_queue(&mut own, &mut other, id, "RepeatMode").await;
    status(
        &http,
        &base,
        &tokens[0],
        "POST",
        "/SyncPlay/NextItem",
        Some(json!({"PlaylistItemId":selected})),
        StatusCode::NO_CONTENT,
    )
    .await;
    queue = same_queue(&mut own, &mut other, id, "NextItem").await;
    assert_eq!(
        queue["Playlist"][queue["PlayingItemIndex"].as_u64().unwrap() as usize]["PlaylistItemId"],
        selected
    );
    status(
        &http,
        &base,
        &tokens[0],
        "POST",
        "/SyncPlay/PreviousItem",
        Some(json!({"PlaylistItemId":selected})),
        StatusCode::NO_CONTENT,
    )
    .await;
    queue = same_queue(&mut own, &mut other, id, "PreviousItem").await;
    assert_eq!(
        queue["Playlist"][queue["PlayingItemIndex"].as_u64().unwrap() as usize]["PlaylistItemId"],
        selected
    );
    for (index, expected) in [(-1, 0), (99, 4)] {
        status(
            &http,
            &base,
            &tokens[0],
            "POST",
            "/SyncPlay/MovePlaylistItem",
            Some(json!({"PlaylistItemId":selected,"NewIndex":index})),
            StatusCode::NO_CONTENT,
        )
        .await;
        queue = same_queue(&mut own, &mut other, id, "MoveItem").await;
        assert_eq!(queue["PlayingItemIndex"], expected);
    }
    status(
        &http,
        &base,
        &tokens[0],
        "POST",
        "/SyncPlay/RemoveFromPlaylist",
        Some(json!({"ClearPlaylist":true,"ClearPlayingItem":false})),
        StatusCode::NO_CONTENT,
    )
    .await;
    queue = same_queue(&mut own, &mut other, id, "RemoveItems").await;
    assert_eq!(queue["Playlist"].as_array().unwrap().len(), 1);
    assert_eq!(queue["PlayingItemIndex"], 0);
    status(
        &http,
        &base,
        &tokens[0],
        "POST",
        "/SyncPlay/RemoveFromPlaylist",
        Some(json!({"ClearPlaylist":true,"ClearPlayingItem":true})),
        StatusCode::NO_CONTENT,
    )
    .await;
    queue = same_queue(&mut own, &mut other, id, "RemoveItems").await;
    assert_eq!(queue["Playlist"], json!([]));
    assert_eq!(queue["PlayingItemIndex"], -1);
    for socket in [&mut own, &mut other] {
        assert_eq!(
            command(socket, id, "Stop").await["PlaylistItemId"],
            Uuid::nil().simple().to_string()
        );
    }

    status(
        &http,
        &base,
        &tokens[0],
        "POST",
        "/SyncPlay/SetNewQueue",
        Some(json!({"PlayingQueue":[first_item]})),
        StatusCode::NO_CONTENT,
    )
    .await;
    same_queue(&mut own, &mut other, id, "NewPlaylist").await;
    let list: Value = request(&http, &base, &tokens[2], "GET", "/SyncPlay/List", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(list, json!([]));
    status(
        &http,
        &base,
        &tokens[2],
        "GET",
        &format!("/SyncPlay/{id}"),
        None,
        StatusCode::NOT_FOUND,
    )
    .await;
    status(
        &http,
        &base,
        &tokens[2],
        "POST",
        "/SyncPlay/Join",
        Some(json!({"GroupId":id})),
        StatusCode::NO_CONTENT,
    )
    .await;
    update(&mut unrelated, "GroupDoesNotExist", Uuid::nil(), json!("")).await;
    quiet(&mut own).await;
    quiet(&mut other).await;
    sqlx::query("DELETE FROM user_library_access WHERE user_id=$1")
        .bind(peer)
        .execute(&pool)
        .await
        .unwrap();
    status(
        &http,
        &base,
        &tokens[0],
        "POST",
        "/SyncPlay/SetRepeatMode",
        Some(json!({"Mode":"RepeatAll"})),
        StatusCode::NO_CONTENT,
    )
    .await;
    update(&mut own, "UserLeft", id, json!("queue-peer")).await;
    update(&mut other, "GroupLeft", id, json!(id.to_string())).await;
    let event = message(&mut own).await;
    assert_eq!(event["Data"]["Type"], "PlayQueue");
    quiet(&mut other).await;
    quiet(&mut unrelated).await;
    sqlx::query("INSERT INTO user_library_access(user_id,library_id) VALUES($1,$2)")
        .bind(peer)
        .bind(allowed_library)
        .execute(&pool)
        .await
        .unwrap();
    status(
        &http,
        &base,
        &tokens[1],
        "POST",
        "/SyncPlay/Join",
        Some(json!({"GroupId":id})),
        StatusCode::NO_CONTENT,
    )
    .await;
    update(&mut own, "UserJoined", id, json!("queue-peer")).await;
    command(&mut own, id, "Pause").await;
    let event = message(&mut other).await;
    assert_eq!(event["Data"]["Type"], "GroupJoined");
    assert_eq!(message(&mut other).await["Data"]["Type"], "PlayQueue");
    sqlx::query("UPDATE item_metadata SET policy_rating_value=90 WHERE item_id=$1")
        .bind(root)
        .execute(&pool)
        .await
        .unwrap();
    let list: Value = request(&http, &base, &tokens[0], "GET", "/SyncPlay/List", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(list, json!([]));
    // Both participants lost access through a parent rating. Buffered queue
    // events are discarded; only the membership-removal notice may arrive.
    update(&mut own, "GroupLeft", id, json!(id.to_string())).await;
    update(&mut other, "GroupLeft", id, json!(id.to_string())).await;
    quiet(&mut own).await;
    quiet(&mut other).await;
    quiet(&mut unrelated).await;
    let after: Value = request(&http, &base, &tokens[0], "GET", "/SyncPlay/List", None)
        .await
        .json()
        .await
        .unwrap();
    assert_eq!(after, json!([]));
    sqlx::query("UPDATE item_metadata SET policy_rating_value=20 WHERE item_id=$1")
        .bind(root)
        .execute(&pool)
        .await
        .unwrap();
    let last_group: Value = request(
        &http,
        &base,
        &tokens[0],
        "POST",
        "/SyncPlay/New",
        Some(json!({"GroupName":"Disconnect review"})),
    )
    .await
    .json()
    .await
    .unwrap();
    let last_id = Uuid::parse_str(last_group["GroupId"].as_str().unwrap()).unwrap();
    joined(&mut own, last_id, &["queue-owner"]).await;
    own.close(None).await.unwrap();
    timeout(Duration::from_secs(3), async {
        loop {
            let groups: Value = request(&http, &base, &tokens[0], "GET", "/SyncPlay/List", None)
                .await
                .json()
                .await
                .unwrap();
            if groups == json!([]) {
                break;
            }
            tokio::time::sleep(Duration::from_millis(10)).await;
        }
    })
    .await
    .expect("last socket left a group behind");
    status(
        &http,
        &base,
        &tokens[0],
        "GET",
        "/Users/Me",
        None,
        StatusCode::OK,
    )
    .await;
    for socket in [&mut other, &mut unrelated] {
        socket.close(None).await.unwrap();
    }
    state
        .shutdown_requested
        .store(true, std::sync::atomic::Ordering::Release);
    assert!(state.drain_user_sockets().await);
    stop_tx.send(()).unwrap();
    timeout(Duration::from_secs(5), server)
        .await
        .unwrap()
        .unwrap();
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .execute(&admin)
        .await
        .unwrap();
    admin.close().await;
}

async fn command(socket: &mut Socket, id: Uuid, kind: &str) -> Value {
    let event = message(socket).await;
    assert_eq!(event["MessageType"], "SyncPlayCommand");
    assert_eq!(event["Data"]["GroupId"], id.simple().to_string());
    assert_eq!(event["Data"]["Command"], kind);
    event["Data"].clone()
}

async fn same_queue(first: &mut Socket, second: &mut Socket, id: Uuid, reason: &str) -> Value {
    let own = message(first).await;
    let other = message(second).await;
    for event in [&own, &other] {
        assert_eq!(event["MessageType"], "SyncPlayGroupUpdate");
        assert_eq!(event["Data"]["Type"], "PlayQueue");
        assert_eq!(event["Data"]["GroupId"], id.simple().to_string());
        assert_eq!(event["Data"]["Data"]["Reason"], reason);
    }
    assert_eq!(own["Data"], other["Data"]);
    own["Data"]["Data"].clone()
}
