use std::sync::{Arc, atomic::AtomicBool};

use sqlx::PgPool;
use tokio::sync::Semaphore;
use uuid::Uuid;

use crate::config::Config;

#[derive(Clone)]
pub struct AppState {
    pub db: PgPool,
    pub config: Arc<Config>,
    pub server_id: Uuid,
    pub run_id: Uuid,
    pub setup_token: Option<Arc<str>>,
    pub dummy_password_hash: Arc<str>,
    pub password_hash_slots: Arc<Semaphore>,
    pub scan_slots: Arc<Semaphore>,
    pub shutdown_requested: Arc<AtomicBool>,
    pub(crate) user_events: Arc<crate::websocket::UserEvents>,
    pub(crate) syncplay: Arc<crate::syncplay::SyncGroups>,
}

impl AppState {
    /// Close upgraded sockets before releasing this run's database resources.
    pub async fn drain_user_sockets(&self) -> bool {
        self.shutdown_requested
            .store(true, std::sync::atomic::Ordering::Release);
        self.user_events.drain().await
    }

    pub fn new(
        db: PgPool,
        config: Arc<Config>,
        server_id: Uuid,
        setup_token: Option<String>,
    ) -> Self {
        Self::new_for_run(db, config, server_id, Uuid::new_v4(), setup_token)
    }

    pub fn new_for_run(
        db: PgPool,
        config: Arc<Config>,
        server_id: Uuid,
        run_id: Uuid,
        setup_token: Option<String>,
    ) -> Self {
        let scan_workers = config.max_scan_workers;
        Self {
            db,
            config,
            server_id,
            run_id,
            setup_token: setup_token.map(Arc::<str>::from),
            dummy_password_hash: Arc::from(crate::auth::dummy_password_hash()),
            password_hash_slots: Arc::new(Semaphore::new(2)),
            scan_slots: Arc::new(Semaphore::new(scan_workers)),
            shutdown_requested: Arc::new(AtomicBool::new(false)),
            user_events: Arc::new(crate::websocket::UserEvents::new()),
            syncplay: Arc::new(crate::syncplay::SyncGroups::new()),
        }
    }
}
