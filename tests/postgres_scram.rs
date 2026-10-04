use std::{env, net::IpAddr, time::Duration};

use sqlx::{
    Connection, PgConnection,
    postgres::{PgConnectOptions, PgSslMode},
};
use tokio::{
    io::{AsyncRead, AsyncReadExt, AsyncWriteExt},
    net::{TcpListener, TcpStream},
    task::JoinHandle,
};
use uuid::Uuid;

// Abort owned proxy tasks on a failed assertion or timeout as well as on success.
struct OwnedTask<T>(JoinHandle<T>);

impl<T> Drop for OwnedTask<T> {
    fn drop(&mut self) {
        self.0.abort();
    }
}

async fn frame(stream: &mut (impl AsyncRead + Unpin)) -> std::io::Result<Vec<u8>> {
    let tag = stream.read_u8().await?;
    let len = stream.read_u32().await?;
    assert!((4..=1_048_576).contains(&len));
    let mut data = vec![tag];
    data.extend_from_slice(&len.to_be_bytes());
    let offset = data.len();
    data.resize(offset + len as usize - 4, 0);
    stream.read_exact(&mut data[offset..]).await?;
    Ok(data)
}

// This test-only wire proxy forwards real server messages and can corrupt its proof.
// Loopback and disabled TLS make the packet assertions observable in the fixture.
async fn attempt(
    base: &PgConnectOptions,
    role: &str,
    password: &str,
    tamper: bool,
) -> Result<String, sqlx::Error> {
    let listener = TcpListener::bind(("127.0.0.1", 0)).await.unwrap();
    let port = listener.local_addr().unwrap().port();
    let backend = (base.get_host().to_owned(), base.get_port());
    let expected_role = role.to_owned();
    let mut proxy = OwnedTask(tokio::spawn(async move {
        let (mut client, _) = listener.accept().await.unwrap();
        let mut server = TcpStream::connect(backend).await.unwrap();
        let length = client.read_u32().await.unwrap();
        assert!((8..=8192).contains(&length));
        let mut startup = vec![0; length as usize - 4];
        client.read_exact(&mut startup).await.unwrap();
        assert_eq!(&startup[..4], &196608_u32.to_be_bytes());
        let fields: Vec<_> = startup[4..].split(|byte| *byte == 0).collect();
        let selected = fields.chunks(2).find(|pair| pair[0] == b"user").unwrap();
        assert_eq!(selected[1], expected_role.as_bytes());
        server.write_all(&length.to_be_bytes()).await.unwrap();
        server.write_all(&startup).await.unwrap();
        let authentication = frame(&mut server).await.unwrap();
        assert_eq!(authentication[0], b'R');
        assert_eq!(&authentication[5..9], &10_u32.to_be_bytes());
        client.write_all(&authentication).await.unwrap();
        let initial = frame(&mut client).await.unwrap();
        assert_eq!(initial[0], b'p');
        let end = initial[5..].iter().position(|byte| *byte == 0).unwrap() + 5;
        assert_eq!(&initial[5..end], b"SCRAM-SHA-256");
        let first = std::str::from_utf8(&initial[end + 5..]).unwrap();
        assert!(first.starts_with("n,,n=") && first.rsplit_once(",r=").is_some());
        server.write_all(&initial).await.unwrap();
        let (mut client_read, mut client_write) = client.into_split();
        let (mut server_read, mut server_write) = server.into_split();
        let mut forward = OwnedTask(tokio::spawn(async move {
            let mut changed = false;
            loop {
                let mut packet = match frame(&mut server_read).await {
                    Ok(value) => value,
                    Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => break,
                    Err(error) => panic!("server framing error: {:?}", error.kind()),
                };
                if tamper && packet[0] == b'R' && packet[5..9] == 12_u32.to_be_bytes() {
                    assert_eq!(&packet[9..11], b"v=");
                    packet[11] = if packet[11] == b'A' { b'B' } else { b'A' };
                    changed = true;
                }
                if client_write.write_all(&packet).await.is_err() {
                    break;
                }
            }
            let _ = client_write.shutdown().await;
            changed
        }));
        let _ = tokio::io::copy(&mut client_read, &mut server_write).await;
        let _ = server_write.shutdown().await;
        let changed = (&mut forward.0).await.unwrap();
        assert_eq!(changed, tamper, "server signature alteration must occur");
    }));
    let options = base
        .clone()
        .host("127.0.0.1")
        .port(port)
        .username(role)
        .password(password)
        .ssl_mode(PgSslMode::Disable);
    let result = match PgConnection::connect_with(&options).await {
        Ok(mut connection) => {
            let value = sqlx::query_scalar::<_, String>("SELECT current_user")
                .fetch_one(&mut connection)
                .await;
            connection.close().await.unwrap();
            value
        }
        Err(error) => Err(error),
    };
    tokio::time::timeout(Duration::from_secs(10), &mut proxy.0)
        .await
        .expect("the owned proxy must finish")
        .unwrap();
    result
}

fn assert_authentication_rejected(result: Result<String, sqlx::Error>) {
    let error = result.unwrap_err();
    assert_eq!(error.as_database_error().unwrap().code().unwrap(), "28P01");
}

#[tokio::test]
#[ignore = "requires a disposable loopback PostgreSQL database with CREATE ROLE permission via PUFFINBOX_TEST_DATABASE_URL"]
async fn scram_preserves_startup_roles_and_rejects_invalid_passwords_and_server_proofs() {
    let base: PgConnectOptions = env::var("PUFFINBOX_TEST_DATABASE_URL")
        .expect("set PUFFINBOX_TEST_DATABASE_URL to a disposable PostgreSQL database")
        .parse()
        .unwrap();
    assert!(
        base.get_host()
            .parse::<IpAddr>()
            .expect("this wire fixture requires a loopback IP")
            .is_loopback()
    );
    let mut admin = PgConnection::connect_with(&base).await.unwrap();
    let encryption: String = sqlx::query_scalar("SHOW password_encryption")
        .fetch_one(&mut admin)
        .await
        .unwrap();
    assert_eq!(encryption, "scram-sha-256");
    let suffix = Uuid::new_v4().simple().to_string();
    let password = Uuid::new_v4().simple().to_string();
    let mut roles = vec![];
    let mut creation_error = None;
    for (prefix, role_password) in [
        ("ascii", password.clone()),
        ("Unicode🧪", password.clone()),
        ("punct,=", password.clone()),
        ("normalized", format!("synthetic\u{00a0}{suffix}")),
        ("raw", format!("synthetic\u{0007}{suffix}")),
    ] {
        let role = format!("puffinbox_{prefix}_{suffix}");
        let quoted = role.replace('"', "\"\"");
        match sqlx::query(&format!(
            "CREATE ROLE \"{quoted}\" LOGIN PASSWORD '{role_password}'"
        ))
        .execute(&mut admin)
        .await
        {
            Ok(_) => roles.push((role, role_password)),
            Err(error) => {
                creation_error = Some(error);
                break;
            }
        }
    }

    // Keep cleanup outside the task so a protocol assertion cannot strand owned roles.
    let probe_roles = roles.clone();
    let mut probes = OwnedTask(tokio::spawn(async move {
        assert_eq!(probe_roles.len(), 5);
        for (role, password) in probe_roles {
            assert_eq!(attempt(&base, &role, &password, false).await.unwrap(), role);
            if password.contains('\u{00a0}') {
                assert_eq!(
                    attempt(&base, &role, &password.replace('\u{00a0}', " "), false)
                        .await
                        .unwrap(),
                    role
                );
            }
            assert_authentication_rejected(
                attempt(&base, &role, "wrong-synthetic-password", false).await,
            );
            assert!(matches!(
                attempt(&base, &role, &password, true).await,
                Err(sqlx::Error::Protocol(_))
            ));
        }
        assert_authentication_rejected(
            attempt(
                &base,
                &format!("puffinbox_missing_{suffix}"),
                &password,
                false,
            )
            .await,
        );
    }));
    let outcome = tokio::time::timeout(Duration::from_secs(60), &mut probes.0).await;
    if outcome.is_err() {
        probes.0.abort();
        let _ = (&mut probes.0).await;
    }
    let mut cleanup_errors = vec![];
    for (role, _) in roles {
        let quoted = role.replace('"', "\"\"");
        if let Err(error) = sqlx::query(&format!("DROP ROLE \"{quoted}\""))
            .execute(&mut admin)
            .await
        {
            cleanup_errors.push(error);
        }
    }
    admin.close().await.unwrap();
    assert!(cleanup_errors.is_empty(), "owned role cleanup failed");
    assert!(creation_error.is_none(), "owned role creation failed");
    outcome.expect("SCRAM cases must finish").unwrap();
}
