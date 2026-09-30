use std::{
    error::Error,
    fs::OpenOptions,
    io::{Read, Write},
    net::SocketAddr,
    os::unix::fs::{MetadataExt, OpenOptionsExt},
    path::Path,
    sync::{Arc, atomic::Ordering},
    time::Duration,
};

use puffinbox::{AppState, Config, api, auth, db};
use sqlx::postgres::PgPoolOptions;
use tracing_subscriber::EnvFilter;
use uuid::Uuid;

fn main() {
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
        .unwrap_or_else(|error| {
            eprintln!("could not start the async runtime: {error}");
            std::process::exit(1);
        });
    let result = runtime.block_on(run());
    if let Err(error) = &result {
        tracing::error!(error = %error, "server initialization or runtime failed");
    }
    // A stalled network filesystem syscall in spawn_blocking cannot be
    // cancelled by Tokio. Bound runtime teardown so an unresponsive mount
    // cannot keep the process alive indefinitely.
    runtime.shutdown_timeout(Duration::from_secs(5));
    if result.is_err() {
        std::process::exit(1);
    }
}

async fn run() -> Result<(), Box<dyn Error>> {
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_target(false)
        .init();

    let config = Arc::new(
        Config::from_env().map_err(|message| format!("invalid configuration: {message}"))?,
    );
    tokio::fs::create_dir_all(&config.data_dir).await?;
    let pool = PgPoolOptions::new()
        .max_connections(32)
        .acquire_timeout(Duration::from_secs(5))
        .after_connect(|connection, _metadata| {
            Box::pin(async move {
                sqlx::query("SET statement_timeout = '15s'")
                    .execute(connection)
                    .await?;
                Ok(())
            })
        })
        .connect(&config.database_url)
        .await?;
    let mut instance_lock = db::try_server_instance_lock(&pool)
        .await?
        .ok_or("another PuffinBox server instance already owns this database")?;
    let run_id = Uuid::new_v4();
    // Claim the database before applying any migration so a competing newer
    // process cannot alter the schema under a server that failed its lock.
    // Running migrations on the dedicated lock connection also makes a lost
    // backend abort its in-flight migration session.
    db::set_active_run_marker_on_connection(&mut instance_lock, run_id).await?;
    sqlx::migrate!("./migrations")
        .run(&mut instance_lock)
        .await?;
    let (ended_stale, interrupted) = db::activate_run(&pool, run_id).await?;
    db::reconcile_login_throttle_capacity(&pool, run_id).await?;
    let server_id = db::persisted_server_id(&pool, run_id).await?;
    if interrupted > 0 {
        tracing::warn!(
            count = interrupted,
            "marked interrupted library scans for retry"
        );
    }
    if ended_stale > 0 {
        tracing::info!(
            count = ended_stale,
            "closed playback sessions left by a previous server run"
        );
    }

    if db::user_count(&pool).await? == 0
        && let (Some(username), Some(password)) = (
            &config.bootstrap_admin_username,
            &config.bootstrap_admin_password,
        )
    {
        auth::validate_username(username)
            .map_err(|_| "bootstrap administrator username is invalid")?;
        auth::validate_password(password)
            .map_err(|_| "bootstrap administrator password must be 12–1024 bytes")?;
        let temporary_state = AppState::new(pool.clone(), config.clone(), server_id, None);
        let hash = auth::hash_password(&temporary_state, password.clone())
            .await
            .map_err(|_| std::io::Error::other("bootstrap password hashing failed"))?;
        let user = db::bootstrap_first_admin(&pool, run_id, username, &hash).await?;
        if user.is_some() {
            tracing::info!(username, "created the configured initial administrator");
        }
    }

    let setup_token = if db::user_count(&pool).await? == 0 {
        Some(resolve_setup_token(&config.setup_token, &config.data_dir.join("setup-token")).await?)
    } else {
        let _ = tokio::fs::remove_file(config.data_dir.join("setup-token")).await;
        None
    };
    if setup_token.is_some() {
        tracing::warn!(path = %config.data_dir.join("setup-token").display(), "initial administrator setup is enabled; read the protected setup-token file");
    }

    let state = AppState::new_for_run(pool.clone(), config.clone(), server_id, run_id, setup_token);
    let recovered_live_jobs = puffinbox::media_features::recover_livetv(&state)
        .await
        .map_err(|error| format!("could not recover Live TV scratch jobs: {error:?}"))?;
    if recovered_live_jobs > 0 {
        tracing::info!(
            count = recovered_live_jobs,
            "reconciled stale Live TV playback and recording jobs from a prior server run"
        );
    }
    puffinbox::media_features::start_livetv_recorder(state.clone()).await;
    puffinbox::media_features::start_dlna(state.clone())
        .await
        .map_err(|error| format!("could not start opt-in DLNA service: {error}"))?;
    puffinbox::offline::start_worker(state.clone());
    puffinbox::metadata::start_worker(state.clone());
    let shutdown_requested = state.shutdown_requested.clone();
    let scan_slots = state.scan_slots.clone();
    let scan_worker_count = config.max_scan_workers;
    let address: SocketAddr = config.bind;
    let listener = tokio::net::TcpListener::bind(address).await?;
    tracing::info!(address = %listener.local_addr()?, product = "PuffinBox", "server listening");

    let prune_pool = pool.clone();
    let prune_shutdown_requested = shutdown_requested.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(600));
        loop {
            interval.tick().await;
            if prune_shutdown_requested.load(Ordering::Acquire) {
                return;
            }
            if let Err(error) = db::prune_login_throttles(&prune_pool, run_id).await {
                tracing::warn!(error = %error, "could not prune expired login throttle entries");
            }
        }
    });

    let (shutdown_tx, mut shutdown_rx) = tokio::sync::watch::channel(false);
    let mut serve = Box::pin(async move {
        axum::serve(
            listener,
            api::router(state).into_make_service_with_connect_info::<SocketAddr>(),
        )
        .with_graceful_shutdown(async move {
            let _ = shutdown_rx.changed().await;
        })
        .await
    });
    let mut lock_monitor = Box::pin(async {
        let mut interval = tokio::time::interval(Duration::from_secs(3));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        interval.tick().await;
        loop {
            interval.tick().await;
            match tokio::time::timeout(
                Duration::from_secs(2),
                sqlx::query("SELECT 1").execute(&mut instance_lock),
            )
            .await
            {
                Ok(Ok(_)) => {}
                Ok(Err(error)) => {
                    tracing::error!(error = %error, "database instance-lock connection was lost");
                    return;
                }
                Err(_) => {
                    tracing::error!("database instance-lock heartbeat timed out");
                    return;
                }
            }
        }
    });
    let mut force_stop = false;
    let mut lock_lost = false;
    let server_result = tokio::select! {
        result = &mut serve => result,
        _ = shutdown_signal() => {
            shutdown_requested.store(true, Ordering::Release);
            shutdown_tx.send_replace(true);
            let drain = tokio::time::timeout(Duration::from_secs(20), &mut serve);
            tokio::pin!(drain);
            tokio::select! {
                result = &mut drain => match result {
                    Ok(result) => result,
                    Err(_) => {
                        tracing::warn!("server shutdown exceeded 20 seconds; active requests were dropped");
                        force_stop = true;
                        Ok(())
                    }
                },
                _ = &mut lock_monitor => {
                    lock_lost = true;
                    force_stop = true;
                    Err(std::io::Error::other("database instance-lock connection was lost during shutdown"))
                }
            }
        },
        _ = &mut lock_monitor => {
            lock_lost = true;
            shutdown_requested.store(true, Ordering::Release);
            shutdown_tx.send_replace(true);
            force_stop = true;
            Err(std::io::Error::other("database instance-lock connection was lost; server stopped"))
        },
    };
    shutdown_requested.store(true, Ordering::Release);
    drop(lock_monitor);
    if force_stop {
        drop(serve);
    }
    match tokio::time::timeout(
        Duration::from_secs(5),
        scan_slots.acquire_many_owned(scan_worker_count as u32),
    )
    .await
    {
        Ok(Ok(permits)) => drop(permits),
        Ok(Err(_)) | Err(_) => {
            tracing::warn!(
                "library scanner shutdown exceeded 5 seconds; stale run fencing will reject further writes"
            );
        }
    }
    match tokio::time::timeout(
        Duration::from_secs(30),
        puffinbox::media_features::shutdown(),
    )
    .await
    {
        Ok(true) => tracing::info!("all media children drained during shutdown"),
        Ok(false) | Err(_) => {
            tracing::warn!(
                "media shutdown exceeded its drain deadline; remaining child jobs will be terminated with the process"
            );
        }
    }
    match tokio::time::timeout(
        Duration::from_secs(2),
        db::end_run_playback_sessions(&pool, run_id),
    )
    .await
    {
        Ok(Ok(ended)) if ended > 0 => {
            tracing::info!(
                count = ended,
                "closed playback sessions for this server run"
            );
        }
        Ok(Err(error)) => {
            tracing::warn!(error = %error, "could not close playback sessions for this server run")
        }
        Err(_) => tracing::warn!("playback session shutdown exceeded 2 seconds"),
        _ => {}
    }
    if !lock_lost {
        match tokio::time::timeout(
            Duration::from_secs(17),
            db::set_active_run_marker_on_connection(&mut instance_lock, Uuid::new_v4()),
        )
        .await
        {
            Ok(Ok(true)) => {}
            Ok(Ok(false)) => tracing::warn!("run shutdown fence found no metadata table"),
            Ok(Err(error)) => {
                tracing::warn!(error = %error, "could not fence this run before releasing the database lock")
            }
            Err(_) => tracing::warn!("run shutdown fence exceeded 17 seconds"),
        }
    }
    if tokio::time::timeout(Duration::from_secs(3), pool.close())
        .await
        .is_err()
    {
        tracing::warn!(
            "database pool shutdown exceeded 3 seconds; remaining work will be terminated"
        );
    }
    if tokio::time::timeout(
        Duration::from_secs(1),
        sqlx::query_scalar::<_, bool>("SELECT pg_advisory_unlock($1)")
            .bind(db::SERVER_INSTANCE_LOCK_KEY)
            .fetch_one(&mut instance_lock),
    )
    .await
    .is_err()
    {
        tracing::warn!("database advisory unlock timed out; dropping the lock connection");
    }
    drop(instance_lock);
    server_result?;
    Ok(())
}

async fn resolve_setup_token(
    configured: &Option<String>,
    path: &Path,
) -> Result<String, Box<dyn Error>> {
    if let Some(token) = configured {
        validate_setup_token(token)?;
        return Ok(token.clone());
    }
    match OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(path)
    {
        Ok(mut file) => {
            let metadata = file.metadata()?;
            if !metadata.is_file() || metadata.mode() & 0o077 != 0 || metadata.len() > 256 {
                return Err("setup-token file must be a regular file, protected to mode 0600, and at most 256 bytes".into());
            }
            let mut token = String::new();
            Read::by_ref(&mut file)
                .take(257)
                .read_to_string(&mut token)?;
            let token = token.trim().to_owned();
            validate_setup_token(&token)?;
            return Ok(token);
        }
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => {}
        Err(error) => return Err(error.into()),
    }
    let token = Uuid::new_v4().to_string();
    let bytes = format!("{token}\n");
    let mut file = OpenOptions::new()
        .write(true)
        .create_new(true)
        .mode(0o600)
        .open(path)?;
    file.write_all(bytes.as_bytes())?;
    file.sync_all()?;
    Ok(token)
}

fn validate_setup_token(token: &str) -> Result<(), Box<dyn Error>> {
    if !(24..=256).contains(&token.len()) || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err("setup token must contain 24–256 printable ASCII bytes".into());
    }
    Ok(())
}

async fn shutdown_signal() {
    let ctrl_c = async {
        let _ = tokio::signal::ctrl_c().await;
    };
    #[cfg(unix)]
    let terminate = async {
        use tokio::signal::unix::{SignalKind, signal};
        if let Ok(mut signal) = signal(SignalKind::terminate()) {
            signal.recv().await;
        }
    };
    #[cfg(not(unix))]
    let terminate = std::future::pending::<()>();
    tokio::select! { _ = ctrl_c => {}, _ = terminate => {} }
}
