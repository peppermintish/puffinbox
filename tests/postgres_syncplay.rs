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
