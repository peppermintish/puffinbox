use std::{env, net::SocketAddr, path::PathBuf, sync::Arc, time::Duration};

use futures_util::{SinkExt, StreamExt};
use puffinbox::{AppState, Config, api, auth, db};
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use tokio::{net::TcpStream, time::timeout};
use tokio_tungstenite::{
    MaybeTlsStream, WebSocketStream, connect_async,
    tungstenite::{Error, Message, client::IntoClientRequest, http::StatusCode},
};
use uuid::Uuid;

mod common;

type Socket = WebSocketStream<MaybeTlsStream<TcpStream>>;

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via PUFFINBOX_TEST_DATABASE_URL"]
async fn sockets_publish_committed_own_user_data_and_enforce_live_authorization() {
    let database_url = env::var("PUFFINBOX_TEST_DATABASE_URL")
        .expect("set PUFFINBOX_TEST_DATABASE_URL to a disposable PostgreSQL database");
    let admin = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url)
        .await
        .unwrap();
    let schema = format!("puffinbox_socket_test_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
        .execute(&admin)
        .await
        .unwrap();
    let connection_schema = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(6)
        .acquire_timeout(Duration::from_secs(5))
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
    let user_id = Uuid::new_v4();
    let peer_id = Uuid::new_v4();
    for (id, name) in [(user_id, "socket-user"), (peer_id, "socket-peer")] {
        sqlx::query("INSERT INTO users(id,username,username_norm,password_hash,enable_remote_access) VALUES($1,$2,$2,'unused',TRUE)")
            .bind(id).bind(name).execute(&pool).await.unwrap();
    }
    let library_id = Uuid::new_v4();
    db::insert_library(
        &pool,
        run_id,
        library_id,
        "Socket fixture",
        "movies",
        &[PathBuf::from("/media")],
        true,
    )
    .await
    .unwrap();
    let item_id = Uuid::new_v4();
    sqlx::query("INSERT INTO items(id,library_id,name,sort_name,item_type,path,path_hash,runtime_ticks) VALUES($1,$2,'Socket movie','socket movie','Movie','/media/socket.mkv',$3,3000000000)")
        .bind(item_id).bind(library_id).bind(auth::token_digest("socket-fixture"))
        .execute(&pool).await.unwrap();
    let config = Config {
        bind: "127.0.0.1:0".parse().unwrap(),
        public_base_url: None,
        database_url,
        server_name: "Socket test".to_owned(),
        web_root: PathBuf::from("web"),
        data_dir: env::temp_dir().join(format!("puffinbox-{schema}")),
        ffmpeg_path: None,
        max_scan_workers: 1,
        max_page_size: 100,
        access_token_lifetime_hours: 24,
        cookie_secure: false,
        cors_origins: vec!["https://allowed.example".to_owned()],
        trusted_proxies: vec!["127.0.0.1/32".parse().unwrap()],
        local_networks: vec!["10.0.0.0/8".parse().unwrap()],
        setup_token: None,
        bootstrap_admin_username: None,
        bootstrap_admin_password: None,
    };
    let state = AppState::new_for_run(pool.clone(), Arc::new(config), Uuid::new_v4(), run_id, None);
    let user = db::get_user(&pool, user_id).await.unwrap().unwrap();
    let peer = db::get_user(&pool, peer_id).await.unwrap().unwrap();
    let token = auth::issue_token(&state, &user, "Socket test", "browser", "socket-main")
        .await
        .unwrap();
    let peer_token = auth::issue_token(&state, &peer, "Socket test", "browser", "socket-peer")
        .await
        .unwrap();
    let listener = tokio::net::TcpListener::bind(state.config.bind)
        .await
        .unwrap();
    let address = listener.local_addr().unwrap();
    let base = format!("http://{address}");
    let socket_base = format!("ws://{address}/socket");
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
    let query = format!("?api_key={}&deviceId=socket-main", token.token);
    for invalid in [
        String::new(),
        "?api_key=invalid".to_owned(),
        format!("{query}&api_key={}", token.token),
        format!("{query}&ApiKey={}", token.token),
        "?api_key=bad%0Atoken".to_owned(),
    ] {
        reject(&socket_base, &invalid, &[], StatusCode::UNAUTHORIZED).await;
    }
    reject(
        &socket_base,
        &query,
        &[("x-emby-token", &peer_token.token)],
        StatusCode::UNAUTHORIZED,
    )
    .await;
    reject(
        &socket_base,
        &query,
        &[("origin", "https://untrusted.example")],
        StatusCode::FORBIDDEN,
    )
    .await;
    let cookie = format!("puffinbox_session={}", token.token);
    reject(
        &socket_base,
        "",
        &[("cookie", &cookie)],
        StatusCode::FORBIDDEN,
    )
    .await;
    reject(
        &socket_base,
        "",
        &[("cookie", &cookie), ("origin", "https://untrusted.example")],
        StatusCode::FORBIDDEN,
    )
    .await;
    let mut cookie_socket = open(&socket_base, "", &[("cookie", &cookie), ("origin", &base)]).await;
    cookie_socket.close(None).await.unwrap();
    let scoped = auth::issue_media_access_token(&state, token.token_id)
        .await
        .unwrap();
    reject(
        &socket_base,
        &format!("?api_key={}", scoped.token),
        &[],
        StatusCode::UNAUTHORIZED,
    )
    .await;

    let mut own = open(&socket_base, &query, &[]).await;
    let mut other = open(
        &socket_base,
        "",
        &[
            ("x-emby-token", &peer_token.token),
            ("origin", "https://allowed.example"),
        ],
    )
    .await;
    own.send(Message::Text(
        json!({"MessageType":"KeepAlive"}).to_string().into(),
    ))
    .await
    .unwrap();
    assert_eq!(message(&mut own).await["MessageType"], "KeepAlive");
    let data_path = format!("/Items/{item_id}/UserData");
    let response = request(
        &http,
        &base,
        &token.token,
        "POST",
        &data_path,
        Some(json!({"PlaybackPositionTicks":1200000000,"Played":false,"PlayCount":42,"LastPlayedDate":"2024-06-07T08:09:10Z","Rating":8.5})),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let committed: Value = response.json().await.unwrap();
    assert_eq!(committed["PlayCount"], 42);
    assert_eq!(committed["LastPlayedDate"], "2024-06-07T08:09:10Z");
    assert_eq!(committed["Rating"], 8.5);
    assert_eq!(committed["Likes"], true);
    expect_data(&mut own, user_id, committed).await;
    quiet(&mut other).await;
    for (method, path, field, expected) in [
        (
            "POST",
            format!("/UserFavoriteItems/{item_id}"),
            "IsFavorite",
            true,
        ),
        (
            "DELETE",
            format!("/UserFavoriteItems/{item_id}"),
            "IsFavorite",
            false,
        ),
        (
            "POST",
            format!("/UserPlayedItems/{item_id}"),
            "Played",
            true,
        ),
        (
            "DELETE",
            format!("/UserPlayedItems/{item_id}"),
            "Played",
            false,
        ),
    ] {
        let response = request(&http, &base, &token.token, method, &path, None).await;
        assert_eq!(response.status(), StatusCode::OK);
        let data: Value = response.json().await.unwrap();
        assert_eq!(data[field], expected);
        expect_data(&mut own, user_id, data).await;
    }
    let play_id = Uuid::new_v4();
    for (path, position) in [
        ("/Sessions/Playing", 800000000i64),
        ("/Sessions/Playing/Progress", 1400000000),
        ("/Sessions/Playing/Stopped", 1600000000),
    ] {
        let response = request(&http, &base, &token.token, "POST", path,
            Some(json!({"ItemId":item_id,"PlaySessionId":play_id,"PositionTicks":position,"PlayedToCompletion":false}))).await;
        assert_eq!(response.status(), StatusCode::NO_CONTENT);
        let data = request(&http, &base, &token.token, "GET", &data_path, None)
            .await
            .json()
            .await
            .unwrap();
        expect_data(&mut own, user_id, data).await;
    }
    quiet(&mut other).await;
    let response = request(
        &http,
        &base,
        &peer_token.token,
        "POST",
        &data_path,
        Some(json!({"PlaybackPositionTicks":990000000})),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    expect_data(&mut other, peer_id, response.json().await.unwrap()).await;
    quiet(&mut own).await;

    // A policy change committed with an update must be reloaded at delivery.
    sqlx::raw_sql("CREATE FUNCTION hide_socket_item() RETURNS trigger LANGUAGE plpgsql AS $$ BEGIN IF NEW.playback_position_ticks=1310000000 THEN UPDATE users SET restrict_libraries=TRUE WHERE id=NEW.user_id; END IF; RETURN NEW; END $$; CREATE TRIGGER hide_socket_item AFTER INSERT OR UPDATE ON user_item_data FOR EACH ROW EXECUTE FUNCTION hide_socket_item();")
        .execute(&pool).await.unwrap();
    let response = request(
        &http,
        &base,
        &token.token,
        "POST",
        &data_path,
        Some(json!({"PlaybackPositionTicks":1310000000})),
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    quiet(&mut own).await;
    assert_eq!(
        request(&http, &base, &token.token, "GET", &data_path, None)
            .await
            .status(),
        StatusCode::NOT_FOUND
    );
    sqlx::raw_sql(
        "DROP TRIGGER hide_socket_item ON user_item_data; DROP FUNCTION hide_socket_item();",
    )
    .execute(&pool)
    .await
    .unwrap();
    sqlx::query("UPDATE users SET restrict_libraries=FALSE WHERE id=$1")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
    let response = request(
        &http,
        &base,
        &token.token,
        "POST",
        &data_path,
        Some(json!({"PlaybackPositionTicks":1600000000})),
    )
    .await;
    expect_data(&mut own, user_id, response.json().await.unwrap()).await;

    let mut extra = Vec::new();
    for _ in 0..7 {
        extra.push(open(&socket_base, &query, &[]).await);
    }
    reject(&socket_base, &query, &[], StatusCode::TOO_MANY_REQUESTS).await;
    for mut socket in extra {
        socket.close(None).await.unwrap();
    }
    let mut released = timeout(Duration::from_secs(3), async {
        loop {
            match handshake(&socket_base, &query, &[]).await {
                Ok((mut socket, _)) => {
                    initial(&mut socket).await;
                    break socket;
                }
                Err(Error::Http(response))
                    if response.status() == StatusCode::TOO_MANY_REQUESTS =>
                {
                    tokio::time::sleep(Duration::from_millis(20)).await;
                }
                _ => panic!("connection slots did not become reusable"),
            }
        }
    })
    .await
    .unwrap();
    released.close(None).await.unwrap();
    let mut binary = open(&socket_base, &query, &[]).await;
    binary
        .send(Message::Binary(vec![0, 1].into()))
        .await
        .unwrap();
    expect_close(&mut binary, 1003).await;
    let mut malformed = open(&socket_base, &query, &[]).await;
    malformed.send(Message::Text("{".into())).await.unwrap();
    expect_close(&mut malformed, 1007).await;
    let mut oversized = open(&socket_base, &query, &[]).await;
    oversized
        .send(Message::Text("x".repeat(16 * 1024 + 1).into()))
        .await
        .unwrap();
    let ended = timeout(Duration::from_secs(3), oversized.next())
        .await
        .unwrap();
    assert!(
        matches!(ended, None | Some(Err(_)) | Some(Ok(Message::Close(_)))),
        "oversized socket message was accepted"
    );
    let mut flooding = open(&socket_base, &query, &[]).await;
    for _ in 0..33 {
        let _ = flooding
            .send(Message::Text(
                json!({"MessageType":"KeepAlive"}).to_string().into(),
            ))
            .await;
    }
    expect_close(&mut flooding, 1008).await;

    // Live sessions must observe expiry, disabling, policy changes and logout.
    let expired = auth::issue_token(&state, &user, "Socket test", "browser", "expiring")
        .await
        .unwrap();
    let expiry_query = format!("?api_key={}", expired.token);
    let mut expiring = open(&socket_base, &expiry_query, &[]).await;
    sqlx::query("UPDATE auth_tokens SET expires_at=NOW()-INTERVAL '1 second' WHERE id=$1")
        .bind(expired.token_id)
        .execute(&pool)
        .await
        .unwrap();
    expect_close(&mut expiring, 1008).await;
    reject(&socket_base, &expiry_query, &[], StatusCode::UNAUTHORIZED).await;
    let mut disabled = open(&socket_base, &query, &[]).await;
    sqlx::query("UPDATE users SET disabled=TRUE WHERE id=$1")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
    expect_close(&mut disabled, 1008).await;
    expect_close(&mut own, 1008).await;
    reject(&socket_base, &query, &[], StatusCode::UNAUTHORIZED).await;
    sqlx::query("UPDATE users SET disabled=FALSE,enable_remote_access=FALSE WHERE id=$1")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
    let remote_headers = [("x-forwarded-for", "203.0.113.20")];
    reject(&socket_base, &query, &remote_headers, StatusCode::FORBIDDEN).await;
    sqlx::query("UPDATE users SET enable_remote_access=TRUE WHERE id=$1")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
    let mut remote = open(&socket_base, &query, &remote_headers).await;
    sqlx::query("UPDATE users SET enable_remote_access=FALSE WHERE id=$1")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
    expect_close(&mut remote, 1008).await;
    let mut revoked = open(&socket_base, &query, &[("x-forwarded-for", "10.0.0.10")]).await;
    db::revoke_auth_token(&pool, run_id, &auth::token_digest(&token.token), user_id)
        .await
        .unwrap();
    expect_close(&mut revoked, 1008).await;
    reject(&socket_base, &query, &[], StatusCode::UNAUTHORIZED).await;
    assert!(
        db::set_active_run_marker(&pool, Uuid::new_v4())
            .await
            .unwrap()
    );
    expect_close(&mut other, 1008).await;
    reject(
        &socket_base,
        "",
        &[("x-emby-token", &peer_token.token)],
        StatusCode::SERVICE_UNAVAILABLE,
    )
    .await;
    assert!(db::set_active_run_marker(&pool, run_id).await.unwrap());
    let mut closing = open(&socket_base, "", &[("x-emby-token", &peer_token.token)]).await;
    assert!(state.drain_user_sockets().await);
    expect_close(&mut closing, 1001).await;
    reject(
        &socket_base,
        "",
        &[("x-emby-token", &peer_token.token)],
        StatusCode::SERVICE_UNAVAILABLE,
    )
    .await;
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

async fn handshake(
    base: &str,
    query: &str,
    headers: &[(&str, &str)],
) -> Result<
    (
        Socket,
        tokio_tungstenite::tungstenite::handshake::client::Response,
    ),
    Error,
> {
    let mut request = format!("{base}{query}").into_client_request().unwrap();
    for (name, value) in headers {
        request.headers_mut().append(
            tokio_tungstenite::tungstenite::http::HeaderName::from_bytes(name.as_bytes()).unwrap(),
            value.parse().unwrap(),
        );
    }
    timeout(Duration::from_secs(3), connect_async(request))
        .await
        .expect("socket handshake timed out")
}

async fn open(base: &str, query: &str, headers: &[(&str, &str)]) -> Socket {
    let (mut socket, response) = match handshake(base, query, headers).await {
        Ok(result) => result,
        Err(Error::Http(response)) => {
            panic!("socket handshake rejected with {}", response.status())
        }
        _ => panic!("socket handshake failed"),
    };
    assert_eq!(response.status(), StatusCode::SWITCHING_PROTOCOLS);
    initial(&mut socket).await;
    socket
}

async fn reject(base: &str, query: &str, headers: &[(&str, &str)], status: StatusCode) {
    match handshake(base, query, headers).await {
        Err(Error::Http(response)) => assert_eq!(response.status(), status),
        _ => panic!("invalid socket handshake was not rejected with its expected HTTP status"),
    }
}

async fn initial(socket: &mut Socket) {
    let initial = message(socket).await;
    assert_eq!(initial["MessageType"], "ForceKeepAlive");
    assert_eq!(initial["Data"], 90);
    assert!(Uuid::parse_str(initial["MessageId"].as_str().unwrap()).is_ok());
}

async fn message(socket: &mut Socket) -> Value {
    let message = timeout(Duration::from_secs(3), socket.next())
        .await
        .expect("socket event timed out")
        .expect("socket ended")
        .expect("socket protocol error");
    let Message::Text(text) = message else {
        panic!("expected a text socket event");
    };
    serde_json::from_str(&text).unwrap()
}

async fn expect_data(socket: &mut Socket, user_id: Uuid, committed: Value) {
    let event = message(socket).await;
    assert_eq!(event["MessageType"], "UserDataChanged");
    assert_eq!(event["Data"]["UserId"], user_id.to_string());
    assert_eq!(event["Data"]["UserDataList"], json!([committed]));
}

async fn quiet(socket: &mut Socket) {
    assert!(
        timeout(Duration::from_millis(300), socket.next())
            .await
            .is_err(),
        "socket received data belonging to another user or a hidden item"
    );
}

async fn expect_close(socket: &mut Socket, code: u16) {
    timeout(Duration::from_secs(7), async {
        loop {
            match socket.next().await {
                Some(Ok(Message::Close(Some(frame)))) => {
                    assert_eq!(u16::from(frame.code), code);
                    break;
                }
                Some(Ok(Message::Text(text))) => {
                    let event: Value = serde_json::from_str(&text).unwrap();
                    assert_ne!(
                        event["MessageType"], "UserDataChanged",
                        "closed session received private data"
                    );
                }
                _ => panic!("socket ended without the expected close frame"),
            }
        }
    })
    .await
    .expect("socket remained active after its session became unavailable");
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
