use std::{
    env, fs,
    net::{Ipv4Addr, SocketAddr},
    path::PathBuf,
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::mpsc,
};

use axum::{
    body::Body,
    extract::connect_info::ConnectInfo,
    http::{Method, Request, StatusCode},
    response::Response,
};
use http_body_util::BodyExt;
use ipnet::IpNet;
use puffinbox::{AppState, Config, api, auth, db, library, media_features};
use serde_json::{Value, json};
use sqlx::postgres::PgPoolOptions;
use tower::ServiceExt;
use uuid::Uuid;

mod common;

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database via PUFFINBOX_TEST_DATABASE_URL"]
async fn dlna_pairing_addresses_are_exact_bounded_ipv4_hosts() {
    let database_url = env::var("PUFFINBOX_TEST_DATABASE_URL")
        .expect("set PUFFINBOX_TEST_DATABASE_URL to a disposable PostgreSQL database");
    let admin_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url)
        .await
        .unwrap();
    let schema = format!("puffinbox_dlna_test_{}", Uuid::new_v4().simple());
    sqlx::query(&format!("CREATE SCHEMA \"{schema}\""))
        .execute(&admin_pool)
        .await
        .unwrap();

    let connection_schema = schema.clone();
    let pool = PgPoolOptions::new()
        .max_connections(2)
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

    let user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,username,username_norm,password_hash) VALUES ($1,'dlna-test','dlna-test','unused')")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();

    insert_pairing(&pool, user_id, "192.168.40.18/32")
        .await
        .expect("an exact IPv4 host address must be accepted");

    for invalid_address in [
        ("::1/128", "IPv6 address"),
        ("192.168.40.19/24", "non-host subnet mask"),
        ("0.0.0.0/32", "unspecified address"),
        ("255.255.255.255/32", "limited broadcast"),
        ("239.1.2.3/32", "multicast address"),
    ] {
        let result = insert_pairing(&pool, user_id, invalid_address.0).await;
        let error = result.expect_err(invalid_address.1);
        assert_eq!(
            sqlstate(&error).as_deref(),
            Some("23514"),
            "{} rejected by the wrong constraint",
            invalid_address.1
        );
    }

    let duplicate = insert_pairing(&pool, user_id, "192.168.40.18/32").await;
    let error = duplicate.expect_err("a renderer address may be paired only once");
    assert_eq!(sqlstate(&error).as_deref(), Some("23505"));

    let pair_count: i64 = sqlx::query_scalar("SELECT count(*) FROM dlna_pairings")
        .fetch_one(&pool)
        .await
        .unwrap();
    assert_eq!(
        pair_count, 1,
        "invalid or duplicate pairings must not persist"
    );

    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .execute(&admin_pool)
        .await
        .unwrap();
}

#[tokio::test]
#[ignore = "requires a disposable PostgreSQL database and free UDP port 1900"]
async fn dlna_http_and_ssdp_are_bound_to_the_direct_peer_and_current_user_policy() {
    let database_url = env::var("PUFFINBOX_TEST_DATABASE_URL")
        .expect("set PUFFINBOX_TEST_DATABASE_URL to a disposable PostgreSQL database");
    let admin_pool = PgPoolOptions::new()
        .max_connections(1)
        .acquire_timeout(Duration::from_secs(5))
        .connect(&database_url)
        .await
        .unwrap();
    let schema = format!("puffinbox_dlna_http_test_{}", Uuid::new_v4().simple());
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
    let user_id = Uuid::new_v4();
    sqlx::query("INSERT INTO users(id,username,username_norm,password_hash,is_admin,allow_media_playback,restrict_libraries) VALUES ($1,'dlna-http-test','dlna-http-test','unused',FALSE,TRUE,TRUE)")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();

    let root_dir = env::temp_dir().join(format!("puffinbox-dlna-media-{}", Uuid::new_v4()));
    fs::create_dir_all(&root_dir).unwrap();
    let media_path = root_dir.join("peer-bound-fixture.mp4");
    let fixture_bytes = b"original synthetic DLNA range fixture";
    fs::write(&media_path, fixture_bytes).unwrap();
    let root_dir = fs::canonicalize(root_dir).unwrap();
    let media_path = fs::canonicalize(media_path).unwrap();
    let root_text = root_dir.to_str().unwrap().to_owned();
    let media_text = media_path.to_str().unwrap().to_owned();
    let (device_id, inode) = library::inspect_library_root_identity(root_dir.clone())
        .await
        .unwrap();
    let library_id = Uuid::new_v4();
    db::insert_library(
        &pool,
        run_id,
        library_id,
        "DLNA fixture",
        "movies",
        std::slice::from_ref(&root_dir),
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
    sqlx::query("INSERT INTO items(id,library_id,name,sort_name,item_type,path,path_hash,size_bytes,container) VALUES ($1,$2,'Peer-bound fixture','peer-bound fixture','Movie',$3,$4,$5,'mp4')")
        .bind(item_id)
        .bind(library_id)
        .bind(&media_text)
        .bind(db::path_hash(&media_text))
        .bind(fixture_bytes.len() as i64)
        .execute(&pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO user_library_access(user_id,library_id) VALUES ($1,$2)")
        .bind(user_id)
        .bind(library_id)
        .execute(&pool)
        .await
        .unwrap();

    let config = Config {
        bind: "127.0.0.1:8096".parse::<SocketAddr>().unwrap(),
        public_base_url: None,
        database_url: database_url.clone(),
        server_name: "DLNA synthetic client test".to_owned(),
        web_root: PathBuf::from("web"),
        data_dir: env::temp_dir().join(format!("puffinbox-dlna-data-{}", Uuid::new_v4())),
        ffmpeg_path: None,
        max_scan_workers: 1,
        max_page_size: 100,
        access_token_lifetime_hours: 24,
        cookie_secure: false,
        cors_origins: Vec::new(),
        trusted_proxies: Vec::new(),
        local_networks: vec!["127.0.0.0/8".parse::<IpNet>().unwrap()],
        dlna: puffinbox::config::DlnaConfig {
            enabled: "true".to_owned(),
            interface_address: Some("127.0.0.1".to_owned()),
            advertised_origin: Some("http://127.0.0.1:8096".to_owned()),
        },
        setup_token: None,
        bootstrap_admin_username: None,
        bootstrap_admin_password: None,
    };
    let server_id = db::persisted_server_id(&pool, run_id).await.unwrap();
    let state = AppState::new_for_run(pool.clone(), Arc::new(config), server_id, run_id, None);
    let user = db::get_user(&pool, user_id).await.unwrap().unwrap();
    let token = auth::issue_token(
        &state,
        &user,
        "dlna-test",
        "synthetic client",
        "dlna-http-test",
    )
    .await
    .unwrap()
    .token;

    let direct_peer = SocketAddr::from(([127, 0, 0, 1], 54001));
    let spoofed_peer = SocketAddr::from(([127, 0, 0, 2], 54002));
    let router = api::router(state.clone());

    let unauthenticated = call(
        &router,
        Request::builder()
            .method(Method::GET)
            .uri("/Puffinbox/Dlna/Pairings")
            .extension(ConnectInfo(direct_peer))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(unauthenticated.status(), StatusCode::UNAUTHORIZED);

    let pair_response = call(
        &router,
        Request::builder()
            .method(Method::POST)
            .uri("/Puffinbox/Dlna/Pairings")
            .header("authorization", format!("Bearer {token}"))
            .header("content-type", "application/json")
            .extension(ConnectInfo(direct_peer))
            .body(Body::from(
                json!({"DeviceName":"Synthetic renderer", "ClientAddress":"127.0.0.1"}).to_string(),
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(pair_response.status(), StatusCode::CREATED);
    let pairing: Value = serde_json::from_slice(
        &pair_response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes(),
    )
    .unwrap();
    let pairing_id = pairing["PairingId"].as_str().unwrap();
    assert_eq!(pairing["ClientAddress"], "127.0.0.1");
    assert!(
        pairing["DescriptionUrl"]
            .as_str()
            .unwrap()
            .ends_with(&format!("/Puffinbox/Dlna/{pairing_id}/description.xml"))
    );
    media_features::start_dlna(state.clone()).await.unwrap();

    let description_path = format!("/Puffinbox/Dlna/{pairing_id}/description.xml");
    let description = call(
        &router,
        Request::builder()
            .method(Method::GET)
            .uri(&description_path)
            .header("x-forwarded-for", "127.0.0.2")
            .extension(ConnectInfo(direct_peer))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(description.status(), StatusCode::OK);
    let description_text = String::from_utf8(
        description
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(description_text.contains("urn:schemas-upnp-org:service:ContentDirectory:1"));
    assert!(!description_text.contains("Bearer"));
    assert!(description_text.contains("<eventSubURL>"));
    assert!(description_text.contains(&format!("<UDN>uuid:{server_id}</UDN>")));

    let generic_description = call(
        &router,
        Request::builder()
            .method(Method::GET)
            .uri("/Puffinbox/Dlna/description.xml")
            .extension(ConnectInfo(direct_peer))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(generic_description.status(), StatusCode::OK);
    let generic_description_text = String::from_utf8(
        generic_description
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(generic_description_text.contains(&format!("<UDN>uuid:{server_id}</UDN>")));
    assert!(generic_description_text.contains(&format!(
        "/Puffinbox/Dlna/{pairing_id}/ContentDirectory/control"
    )));
    let unpaired_generic_description = call(
        &router,
        Request::builder()
            .method(Method::GET)
            .uri("/Puffinbox/Dlna/description.xml")
            .header("x-forwarded-for", "127.0.0.1")
            .extension(ConnectInfo(spoofed_peer))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(unpaired_generic_description.status(), StatusCode::NOT_FOUND);

    let event_path = format!("/Puffinbox/Dlna/{pairing_id}/ContentDirectory/event");

    let callback_listener = tokio::net::TcpListener::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let callback_address = callback_listener.local_addr().unwrap();
    let (notification_tx, mut notification_rx) = mpsc::channel(8);
    let callback_server = tokio::spawn(async move {
        for _ in 0..8 {
            let request = receive_gena_notification(&callback_listener).await;
            if notification_tx.send(request).await.is_err() {
                return;
            }
        }
    });

    let spoofed_subscription = call(
        &router,
        Request::builder()
            .method(Method::from_bytes(b"SUBSCRIBE").unwrap())
            .uri(&event_path)
            .header(
                "callback",
                format!("<http://127.0.0.1:{}/notify>", callback_address.port()),
            )
            .header("nt", "upnp:event")
            .header("timeout", "Second-120")
            .extension(ConnectInfo(spoofed_peer))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(spoofed_subscription.status(), StatusCode::NOT_FOUND);

    let subscription_response = call(
        &router,
        Request::builder()
            .method(Method::from_bytes(b"SUBSCRIBE").unwrap())
            .uri(&event_path)
            .header(
                "callback",
                format!("<http://127.0.0.1:{}/notify>", callback_address.port()),
            )
            .header("nt", "upnp:event")
            .header("timeout", "Second-120")
            .extension(ConnectInfo(direct_peer))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(subscription_response.status(), StatusCode::OK);
    let subscription_id = subscription_response
        .headers()
        .get("sid")
        .unwrap()
        .to_str()
        .unwrap()
        .to_owned();
    assert!(subscription_id.starts_with("uuid:"));
    assert_eq!(subscription_response.headers()["timeout"], "Second-120");
    let initial_notification = tokio::time::timeout(Duration::from_secs(4), notification_rx.recv())
        .await
        .expect("initial GENA notification arrives")
        .expect("callback server remains active");
    assert_eq!(
        gena_request_header(&initial_notification, "nt"),
        Some("upnp:event")
    );
    assert_eq!(
        gena_request_header(&initial_notification, "nts"),
        Some("upnp:propchange")
    );
    assert!(initial_notification.starts_with("NOTIFY /notify HTTP/1.1"));
    assert_eq!(
        gena_request_header(&initial_notification, "sid"),
        Some(subscription_id.as_str())
    );
    assert_eq!(gena_request_header(&initial_notification, "seq"), Some("0"));
    assert!(initial_notification.contains("<SystemUpdateID>"));
    assert!(initial_notification.contains("<ContainerUpdateIDs>0,"));

    let invalid_renewal = call(
        &router,
        Request::builder()
            .method(Method::from_bytes(b"SUBSCRIBE").unwrap())
            .uri(&event_path)
            .header("sid", &subscription_id)
            .header("timeout", "Second-60")
            .extension(ConnectInfo(spoofed_peer))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(invalid_renewal.status(), StatusCode::NOT_FOUND);

    let renewal = call(
        &router,
        Request::builder()
            .method(Method::from_bytes(b"SUBSCRIBE").unwrap())
            .uri(&event_path)
            .header("sid", &subscription_id)
            .header("timeout", "Second-60")
            .extension(ConnectInfo(direct_peer))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(renewal.status(), StatusCode::OK);
    assert_eq!(
        renewal.headers().get("sid").unwrap().to_str().unwrap(),
        subscription_id
    );
    assert_eq!(renewal.headers()["timeout"], "Second-60");

    let initial_update_id = get_system_update_id(&router, pairing_id, direct_peer).await;
    sqlx::query("UPDATE items SET name='Updated Peer-bound fixture' WHERE id=$1")
        .bind(item_id)
        .execute(&pool)
        .await
        .unwrap();
    let changed_update_id = get_system_update_id(&router, pairing_id, direct_peer).await;
    assert_ne!(changed_update_id, initial_update_id);
    sqlx::query("INSERT INTO item_metadata(item_id,provider_key,content_rating,policy_rating_scale,policy_rating_value) VALUES ($1,'local-nfo','PG','US-MPAA-v1',50)")
        .bind(item_id)
        .execute(&pool)
        .await
        .unwrap();
    let metadata_update_id = get_system_update_id(&router, pairing_id, direct_peer).await;
    assert_ne!(metadata_update_id, changed_update_id);

    let connection_ids = soap_control(
        &router,
        pairing_id,
        direct_peer,
        "ConnectionManager",
        "GetCurrentConnectionIDs",
        "<s:Envelope><s:Body><u:GetCurrentConnectionIDs/></s:Body></s:Envelope>",
    )
    .await;
    assert_eq!(connection_ids.status(), StatusCode::OK);
    let connection_ids_body = String::from_utf8(
        connection_ids
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(connection_ids_body.contains("<ConnectionIDs></ConnectionIDs>"));

    let valid_unknown_connection = soap_control(
        &router,
        pairing_id,
        direct_peer,
        "ConnectionManager",
        "GetCurrentConnectionInfo",
        "<s:Envelope><s:Body><u:GetCurrentConnectionInfo><ConnectionID>0</ConnectionID></u:GetCurrentConnectionInfo></s:Body></s:Envelope>",
    )
    .await;
    assert_eq!(
        valid_unknown_connection.status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    let valid_unknown_connection_body = String::from_utf8(
        valid_unknown_connection
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(valid_unknown_connection_body.contains("<errorCode>706</errorCode>"));

    let invalid_connection_id = soap_control(
        &router,
        pairing_id,
        direct_peer,
        "ConnectionManager",
        "GetCurrentConnectionInfo",
        "<s:Envelope><s:Body><u:GetCurrentConnectionInfo><ConnectionID>not-an-integer</ConnectionID></u:GetCurrentConnectionInfo></s:Body></s:Envelope>",
    )
    .await;
    assert_eq!(
        invalid_connection_id.status(),
        StatusCode::INTERNAL_SERVER_ERROR
    );
    let invalid_connection_body = String::from_utf8(
        invalid_connection_id
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(invalid_connection_body.contains("<errorCode>402</errorCode>"));

    let spoofed_description = call(
        &router,
        Request::builder()
            .method(Method::GET)
            .uri(&description_path)
            .header("x-forwarded-for", "127.0.0.1")
            .extension(ConnectInfo(spoofed_peer))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(spoofed_description.status(), StatusCode::NOT_FOUND);

    let protocol_info = call(
        &router,
        Request::builder()
            .method(Method::POST)
            .uri(format!(
                "/Puffinbox/Dlna/{pairing_id}/ConnectionManager/control"
            ))
            .header(
                "soapaction",
                "\"urn:schemas-upnp-org:service:ConnectionManager:1#GetProtocolInfo\"",
            )
            .header("content-type", "text/xml; charset=utf-8")
            .extension(ConnectInfo(direct_peer))
            .body(Body::from(
                "<s:Envelope><s:Body><u:GetProtocolInfo/></s:Body></s:Envelope>",
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(protocol_info.status(), StatusCode::OK);
    let protocol_body = protocol_info
        .into_body()
        .collect()
        .await
        .unwrap()
        .to_bytes();
    let protocol_body = String::from_utf8(protocol_body.to_vec()).unwrap();
    assert!(protocol_body.contains("video/mp4"));
    assert!(!protocol_body.contains("http-get:*:*:"));

    let browse_request_body = format!(
        "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:Browse xmlns:u=\"urn:schemas-upnp-org:service:ContentDirectory:1\"><ObjectID>{library_id}</ObjectID><BrowseFlag>BrowseDirectChildren</BrowseFlag><Filter>*</Filter><StartingIndex>0</StartingIndex><RequestedCount>25</RequestedCount><SortCriteria></SortCriteria></u:Browse></s:Body></s:Envelope>"
    );
    let browse_response = call(
        &router,
        Request::builder()
            .method(Method::POST)
            .uri(format!(
                "/Puffinbox/Dlna/{pairing_id}/ContentDirectory/control"
            ))
            .header(
                "soapaction",
                "\"urn:schemas-upnp-org:service:ContentDirectory:1#Browse\"",
            )
            .header("content-type", "text/xml; charset=utf-8")
            .extension(ConnectInfo(direct_peer))
            .body(Body::from(browse_request_body.clone()))
            .unwrap(),
    )
    .await;
    assert_eq!(browse_response.status(), StatusCode::OK);
    let browse_body = String::from_utf8(
        browse_response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(browse_body.contains("Peer-bound fixture"));
    assert!(browse_body.contains(&format!("/Puffinbox/Dlna/{pairing_id}/media/{item_id}")));
    assert!(browse_body.contains(&format!("<UpdateID>{metadata_update_id}</UpdateID>")));

    let unsupported_sort_body = browse_request_body.replace(
        "<SortCriteria></SortCriteria>",
        "<SortCriteria>+dc:title</SortCriteria>",
    );
    let unsupported_sort = soap_control(
        &router,
        pairing_id,
        direct_peer,
        "ContentDirectory",
        "Browse",
        &unsupported_sort_body,
    )
    .await;
    assert_eq!(unsupported_sort.status(), StatusCode::INTERNAL_SERVER_ERROR);
    let unsupported_sort_body = String::from_utf8(
        unsupported_sort
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    assert!(unsupported_sort_body.contains("<errorCode>709</errorCode>"));

    let media_path = format!("/Puffinbox/Dlna/{pairing_id}/media/{item_id}");
    let range_response = call(
        &router,
        Request::builder()
            .method(Method::GET)
            .uri(&media_path)
            .header("range", "bytes=1-5")
            .extension(ConnectInfo(direct_peer))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(range_response.status(), StatusCode::PARTIAL_CONTENT);
    assert_eq!(
        &range_response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()[..],
        &fixture_bytes[1..6]
    );
    let spoofed_media = call(
        &router,
        Request::builder()
            .method(Method::GET)
            .uri(&media_path)
            .header("x-forwarded-for", "127.0.0.1")
            .header("range", "bytes=1-5")
            .extension(ConnectInfo(spoofed_peer))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(spoofed_media.status(), StatusCode::NOT_FOUND);

    let forbidden_soap = call(
        &router,
        Request::builder()
            .method(Method::POST)
            .uri(format!("/Puffinbox/Dlna/{pairing_id}/ContentDirectory/control"))
            .header(
                "soapaction",
                "\"urn:schemas-upnp-org:service:ContentDirectory:1#Browse\"",
            )
            .header("content-type", "text/xml; charset=utf-8")
            .extension(ConnectInfo(direct_peer))
            .body(Body::from(
                "<!DOCTYPE x [<!ENTITY secret SYSTEM 'file:///etc/passwd'>]><s:Envelope><s:Body><u:Browse><ObjectID>&secret;</ObjectID></u:Browse></s:Body></s:Envelope>",
            ))
            .unwrap(),
    )
    .await;
    assert_eq!(forbidden_soap.status(), StatusCode::BAD_REQUEST);

    let allowed_search_socket = tokio::net::UdpSocket::bind((Ipv4Addr::LOCALHOST, 0))
        .await
        .unwrap();
    let unpaired_search_socket = tokio::net::UdpSocket::bind((Ipv4Addr::new(127, 0, 0, 2), 0))
        .await
        .unwrap();
    unpaired_search_socket
        .send_to(
            b"M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: upnp:rootdevice\r\n\r\n",
            (Ipv4Addr::LOCALHOST, 1900),
        )
        .await
        .unwrap();
    let mut unpaired_response = vec![0; 2048];
    assert!(
        tokio::time::timeout(
            Duration::from_millis(350),
            unpaired_search_socket.recv_from(&mut unpaired_response),
        )
        .await
        .is_err()
    );
    allowed_search_socket
        .send_to(
            b"M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: upnp:rootdevice\r\n\r\n",
            (Ipv4Addr::LOCALHOST, 1900),
        )
        .await
        .unwrap();
    let mut discovery_response = vec![0; 2048];
    let (length, _) = tokio::time::timeout(
        Duration::from_secs(2),
        allowed_search_socket.recv_from(&mut discovery_response),
    )
    .await
    .expect("paired synthetic client receives SSDP response")
    .unwrap();
    let discovery_response = std::str::from_utf8(&discovery_response[..length]).unwrap();
    assert!(discovery_response.contains(&server_id.to_string()));
    assert!(discovery_response.contains("/Puffinbox/Dlna/description.xml"));
    assert!(!discovery_response.contains(&pairing_id.to_string()));
    assert!(!discovery_response.contains(&description_path));

    tokio::time::sleep(Duration::from_millis(275)).await;
    sqlx::query("DELETE FROM user_library_access WHERE user_id=$1 AND library_id=$2")
        .bind(user_id)
        .bind(library_id)
        .execute(&pool)
        .await
        .unwrap();
    let access_update_id = get_system_update_id(&router, pairing_id, direct_peer).await;
    let changed_notification = tokio::time::timeout(Duration::from_secs(10), async {
        loop {
            let notification = notification_rx
                .recv()
                .await
                .expect("callback server remains active");
            if notification.contains(&format!(
                "<SystemUpdateID>{access_update_id}</SystemUpdateID>"
            )) {
                break notification;
            }
        }
    })
    .await
    .expect("catalog revision change produces a GENA notification");
    assert_eq!(
        gena_request_header(&changed_notification, "sid"),
        Some(subscription_id.as_str())
    );
    assert!(gena_request_header(&changed_notification, "seq").is_some());

    let unsubscribe = call(
        &router,
        Request::builder()
            .method(Method::from_bytes(b"UNSUBSCRIBE").unwrap())
            .uri(&event_path)
            .header("sid", &subscription_id)
            .extension(ConnectInfo(direct_peer))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(unsubscribe.status(), StatusCode::OK);
    callback_server.abort();
    let _ = callback_server.await;
    assert_ne!(
        get_system_update_id(&router, pairing_id, direct_peer).await,
        metadata_update_id,
        "library visibility changes must advance the ContentDirectory update ID"
    );
    let revoked_media = call(
        &router,
        Request::builder()
            .method(Method::GET)
            .uri(&media_path)
            .extension(ConnectInfo(direct_peer))
            .body(Body::empty())
            .unwrap(),
    )
    .await;
    assert_eq!(revoked_media.status(), StatusCode::NOT_FOUND);
    sqlx::query("UPDATE users SET allow_media_playback=FALSE WHERE id=$1")
        .bind(user_id)
        .execute(&pool)
        .await
        .unwrap();
    allowed_search_socket
        .send_to(
            b"M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: upnp:rootdevice\r\n\r\n",
            (Ipv4Addr::LOCALHOST, 1900),
        )
        .await
        .unwrap();
    let mut revoked_response = vec![0; 2048];
    assert!(
        tokio::time::timeout(
            Duration::from_millis(400),
            allowed_search_socket.recv_from(&mut revoked_response),
        )
        .await
        .is_err()
    );

    assert!(media_features::shutdown().await);
    allowed_search_socket
        .send_to(
            b"M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: upnp:rootdevice\r\n\r\n",
            (Ipv4Addr::LOCALHOST, 1900),
        )
        .await
        .unwrap();
    let mut stopped_response = vec![0; 2048];
    assert!(
        tokio::time::timeout(
            Duration::from_millis(400),
            allowed_search_socket.recv_from(&mut stopped_response),
        )
        .await
        .is_err()
    );

    drop(router);
    drop(state);
    pool.close().await;
    sqlx::query(&format!("DROP SCHEMA \"{schema}\" CASCADE"))
        .execute(&admin_pool)
        .await
        .unwrap();
    admin_pool.close().await;
    let _ = fs::remove_dir_all(root_dir);
}

async fn insert_pairing(
    pool: &sqlx::PgPool,
    user_id: Uuid,
    client_address: &str,
) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO dlna_pairings(id,user_id,client_address,device_name) VALUES ($1,$2,$3::inet,'Test renderer')")
        .bind(Uuid::new_v4())
        .bind(user_id)
        .bind(client_address)
        .execute(pool)
        .await?;
    Ok(())
}

async fn get_system_update_id(router: &axum::Router, pairing_id: &str, peer: SocketAddr) -> u32 {
    let response = soap_control(
        router,
        pairing_id,
        peer,
        "ContentDirectory",
        "GetSystemUpdateID",
        "<s:Envelope><s:Body><u:GetSystemUpdateID/></s:Body></s:Envelope>",
    )
    .await;
    assert_eq!(response.status(), StatusCode::OK);
    let body = String::from_utf8(
        response
            .into_body()
            .collect()
            .await
            .unwrap()
            .to_bytes()
            .to_vec(),
    )
    .unwrap();
    body.split_once("<Id>")
        .and_then(|(_, rest)| rest.split_once("</Id>"))
        .and_then(|(id, _)| id.parse::<u32>().ok())
        .expect("GetSystemUpdateID returns a ui4 ID")
}

async fn soap_control(
    router: &axum::Router,
    pairing_id: &str,
    peer: SocketAddr,
    service: &str,
    action: &str,
    body: &str,
) -> Response {
    let (path, service_type) = match service {
        "ContentDirectory" => (
            "ContentDirectory",
            "urn:schemas-upnp-org:service:ContentDirectory:1",
        ),
        "ConnectionManager" => (
            "ConnectionManager",
            "urn:schemas-upnp-org:service:ConnectionManager:1",
        ),
        _ => panic!("unsupported DLNA SOAP test service"),
    };
    call(
        router,
        Request::builder()
            .method(Method::POST)
            .uri(format!("/Puffinbox/Dlna/{pairing_id}/{path}/control"))
            .header("soapaction", format!("\"{service_type}#{action}\""))
            .header("content-type", "text/xml; charset=utf-8")
            .extension(ConnectInfo(peer))
            .body(Body::from(body.to_owned()))
            .unwrap(),
    )
    .await
}

async fn call(router: &axum::Router, request: Request<Body>) -> Response {
    router.clone().oneshot(request).await.unwrap()
}

async fn receive_gena_notification(listener: &tokio::net::TcpListener) -> String {
    let (mut stream, _) = tokio::time::timeout(Duration::from_secs(8), listener.accept())
        .await
        .expect("DLNA server connects to the paired callback address")
        .unwrap();
    let mut request = Vec::new();
    let mut buffer = [0u8; 2048];
    loop {
        let read = tokio::time::timeout(Duration::from_secs(3), stream.read(&mut buffer))
            .await
            .expect("DLNA callback request completes promptly")
            .unwrap();
        if read == 0 {
            break;
        }
        request.extend_from_slice(&buffer[..read]);
        let Some(header_end) = request.windows(4).position(|bytes| bytes == b"\r\n\r\n") else {
            assert!(request.len() <= 8192, "GENA request headers remain bounded");
            continue;
        };
        let headers = std::str::from_utf8(&request[..header_end]).unwrap();
        let content_length = headers
            .lines()
            .filter_map(|line| line.split_once(':'))
            .find(|(name, _)| name.eq_ignore_ascii_case("content-length"))
            .and_then(|(_, value)| value.trim().parse::<usize>().ok())
            .unwrap_or(0);
        if request.len() >= header_end + 4 + content_length {
            break;
        }
        assert!(request.len() <= 8192, "GENA request remains bounded");
    }
    stream
        .write_all(b"HTTP/1.1 200 OK\r\nContent-Length: 0\r\nConnection: close\r\n\r\n")
        .await
        .unwrap();
    String::from_utf8(request).unwrap()
}

fn gena_request_header<'a>(request: &'a str, name: &str) -> Option<&'a str> {
    request.lines().find_map(|line| {
        let (header_name, value) = line.split_once(':')?;
        header_name
            .eq_ignore_ascii_case(name)
            .then_some(value.trim())
    })
}

fn sqlstate(error: &sqlx::Error) -> Option<String> {
    error
        .as_database_error()
        .and_then(|database| database.code())
        .map(|code| code.into_owned())
}
