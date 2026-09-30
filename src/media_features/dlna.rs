//! Policy-scoped UPnP MediaServer endpoints.
//!
//! UPnP discovery and SOAP requests do not provide account authentication.
//! PuffinBox therefore advertises a device only after an authenticated user
//! pairs one exact private-network IPv4 address. The pairing UUID is an
//! identifier, not a bearer credential: every request also has to originate
//! from the paired address, and the current user and item policy are loaded
//! again before returning a catalog entry or media bytes.

use std::{
    collections::HashMap,
    env,
    net::{IpAddr, Ipv4Addr, SocketAddr, SocketAddrV4, UdpSocket as StdUdpSocket},
    os::fd::AsRawFd,
    str::FromStr,
    sync::{Arc, Mutex, OnceLock},
    time::{Duration, Instant},
};

use axum::{
    Json, Router,
    body::{Body, to_bytes},
    extract::{Path, State, connect_info::ConnectInfo as PeerInfo},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::{IntoResponse, Response},
    routing::{any, delete, get, post},
};
use chrono::{DateTime, Utc};
use quick_xml::{Reader, escape::unescape, events::Event};
use serde::{Deserialize, Serialize};
use sqlx::Row;
use tokio::{
    net::UdpSocket,
    sync::{OwnedSemaphorePermit, Semaphore, oneshot},
    task::JoinHandle,
    time::{self},
};
use url::Url;
use uuid::Uuid;

use crate::{
    ApiError,
    auth::{CurrentUser, UserRecord},
    db,
    library::{ItemQuery, ItemRecord, LibraryRecord},
    state::AppState,
};

const CONTENT_DIRECTORY: &str = "urn:schemas-upnp-org:service:ContentDirectory:1";
const CONNECTION_MANAGER: &str = "urn:schemas-upnp-org:service:ConnectionManager:1";
const MEDIA_SERVER: &str = "urn:schemas-upnp-org:device:MediaServer:1";
const SSDP_GROUP: Ipv4Addr = Ipv4Addr::new(239, 255, 255, 250);
const SSDP_PORT: u16 = 1900;
const SSDP_MAX_AGE_SECONDS: u64 = 180;
const SSDP_ADVERTISEMENT_REFRESH: Duration = Duration::from_secs(90);
const MAX_PAIRINGS_PER_USER: i64 = 32;
const MAX_PAIRINGS: i64 = 256;
const MAX_BROWSE_RESULTS: i64 = 200;
const MAX_BROWSE_OFFSET: i64 = 10_000_000;
const MAX_SOAP_BYTES: usize = 64 * 1024;
const MAX_SOAP_EVENTS: usize = 1024;
const MAX_SOAP_DEPTH: usize = 24;
const MAX_SOAP_TEXT_BYTES: usize = 4096;
const MAX_GENA_SUBSCRIPTIONS: usize = 64;
const MAX_GENA_SUBSCRIPTIONS_PER_PAIRING: usize = 8;
const MAX_GENA_TIMEOUT_SECONDS: u64 = 1800;
const MAX_GENA_REQUEST_BYTES: usize = 1024;
const MAX_GENA_CALLBACK_URL_BYTES: usize = 2048;
const GENA_POLL_INTERVAL: Duration = Duration::from_secs(5);
const GENA_INITIAL_NOTIFY_DELAY: Duration = Duration::from_millis(100);
const GENA_NOTIFY_TIMEOUT: Duration = Duration::from_secs(2);
const MAX_PARALLEL_GENA_NOTIFICATIONS: usize = 8;
const PAIRING_ADVISORY_LOCK: i64 = 82_473_015;
const SAFE_MEDIA_TYPES: &[&str] = &[
    "video/mp4",
    "video/x-matroska",
    "video/webm",
    "video/x-msvideo",
    "video/mpeg",
    "video/mp2t",
    "audio/mpeg",
    "audio/mp4",
    "audio/aac",
    "audio/flac",
    "audio/ogg",
    "audio/ogg; codecs=opus",
    "audio/wav",
    "audio/aiff",
    "image/jpeg",
    "image/png",
    "image/gif",
    "image/webp",
    "image/avif",
    "image/bmp",
];

#[derive(Clone, Debug)]
struct DlnaSettings {
    interface: Ipv4Addr,
    origin: Url,
}

struct ServiceHandle {
    stop: oneshot::Sender<()>,
    join: JoinHandle<bool>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct PairingRecord {
    id: Uuid,
    user_id: Uuid,
    client_address: Ipv4Addr,
    device_name: String,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct PairingDto {
    pairing_id: Uuid,
    device_name: String,
    client_address: String,
    description_url: String,
    created_at: DateTime<Utc>,
    last_seen_at: Option<DateTime<Utc>>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PairRequest {
    device_name: String,
    client_address: String,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum SearchTarget {
    All,
    RootDevice,
    Device,
    DeviceUuid(Uuid),
    ContentDirectory,
    ConnectionManager,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ContentAction {
    Browse,
    GetSystemUpdateId,
    GetSearchCapabilities,
    GetSortCapabilities,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
enum ConnectionAction {
    ProtocolInfo,
    CurrentConnectionIds,
    CurrentConnectionInfo,
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
struct SoapArguments {
    values: HashMap<String, String>,
}

#[derive(Clone, Debug, PartialEq, Eq)]
struct BrowseRequest {
    object_id: String,
    browse_metadata: bool,
    starting_index: i64,
    requested_count: i64,
}

#[derive(Clone, Debug)]
struct GenaSubscription {
    id: Uuid,
    pairing_id: Uuid,
    client_address: Ipv4Addr,
    callback: Url,
    expires_at: Instant,
    next_sequence: u32,
    last_revision: Option<u32>,
    notification_in_flight: bool,
}

static SERVICE: OnceLock<Mutex<Option<ServiceHandle>>> = OnceLock::new();
static SEARCH_LIMITER: OnceLock<Mutex<HashMap<Ipv4Addr, Instant>>> = OnceLock::new();
static GENA_SUBSCRIPTIONS: OnceLock<Mutex<HashMap<Uuid, GenaSubscription>>> = OnceLock::new();
static GENA_HTTP_CLIENT: OnceLock<Result<reqwest::Client, String>> = OnceLock::new();
static GENA_NOTIFY_LIMITER: OnceLock<Arc<Semaphore>> = OnceLock::new();

fn service_slot() -> &'static Mutex<Option<ServiceHandle>> {
    SERVICE.get_or_init(|| Mutex::new(None))
}

/// Add the authenticated pairing API and the IP-bound UPnP endpoints.
pub(super) fn router(state: AppState) -> Router<()> {
    Router::new()
        .route(
            "/Puffinbox/Dlna/Pairings",
            get(list_pairings).post(pair_device),
        )
        .route(
            "/Puffinbox/Dlna/Pairings/{pairing_id}",
            delete(unpair_device),
        )
        .route(
            "/Puffinbox/Dlna/description.xml",
            get(discovered_device_description),
        )
        .route(
            "/Puffinbox/Dlna/{pairing_id}/description.xml",
            get(device_description),
        )
        .route(
            "/Puffinbox/Dlna/{pairing_id}/ContentDirectory/scpd.xml",
            get(content_directory_scpd),
        )
        .route(
            "/Puffinbox/Dlna/{pairing_id}/ContentDirectory/control",
            post(content_directory_control),
        )
        .route(
            "/Puffinbox/Dlna/{pairing_id}/ContentDirectory/event",
            any(content_directory_event),
        )
        .route(
            "/Puffinbox/Dlna/{pairing_id}/ConnectionManager/scpd.xml",
            get(connection_manager_scpd),
        )
        .route(
            "/Puffinbox/Dlna/{pairing_id}/ConnectionManager/control",
            post(connection_manager_control),
        )
        .route(
            "/Puffinbox/Dlna/{pairing_id}/media/{item_id}",
            get(stream_item).head(stream_item),
        )
        .with_state(state)
}

/// Start the SSDP listener only when the operator explicitly enables DLNA.
/// A configured interface and a matching HTTP origin are required so a
/// renderer cannot receive a description URL for an unrelated address.
pub(super) async fn start(state: AppState) -> Result<(), String> {
    let Some(settings) = settings_from_env(&state)? else {
        tracing::info!("DLNA discovery is disabled");
        return Ok(());
    };

    let socket = StdUdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, SSDP_PORT))
        .map_err(|error| format!("could not bind the DLNA SSDP port {SSDP_PORT}: {error}"))?;
    set_multicast_interface(&socket, settings.interface).map_err(|error| {
        format!(
            "could not select the DLNA SSDP multicast interface {}: {error}",
            settings.interface
        )
    })?;
    socket
        .join_multicast_v4(&SSDP_GROUP, &settings.interface)
        .map_err(|error| format!("could not join the DLNA SSDP multicast group: {error}"))?;
    socket
        .set_multicast_ttl_v4(2)
        .map_err(|error| format!("could not set the DLNA multicast hop limit: {error}"))?;
    socket
        .set_nonblocking(true)
        .map_err(|error| format!("could not configure the DLNA SSDP socket: {error}"))?;
    let socket = UdpSocket::from_std(socket)
        .map_err(|error| format!("could not attach the DLNA SSDP socket: {error}"))?;

    let (stop, stopped) = oneshot::channel();
    let join = tokio::spawn(run_ssdp(state, socket, settings, stopped));
    let mut service = service_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    if service.is_some() {
        let _ = stop.send(());
        return Err("DLNA SSDP service was already started".to_owned());
    }
    *service = Some(ServiceHandle { stop, join });
    Ok(())
}

/// Stop the paired-address SSDP listener before returning.
pub(super) async fn shutdown() -> bool {
    let service = service_slot()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .take();
    let Some(service) = service else {
        gena_subscriptions()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner)
            .clear();
        return true;
    };
    let _ = service.stop.send(());
    let stopped = matches!(
        time::timeout(Duration::from_secs(4), service.join).await,
        Ok(Ok(true))
    );
    gena_subscriptions()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .clear();
    stopped
}

fn settings_from_env(state: &AppState) -> Result<Option<DlnaSettings>, String> {
    let enabled = env::var("PUFFINBOX_DLNA_ENABLED").unwrap_or_else(|_| "false".to_owned());
    let enabled = match enabled.trim().to_ascii_lowercase().as_str() {
        "true" | "1" | "yes" => true,
        "false" | "0" | "no" | "" => false,
        _ => return Err("PUFFINBOX_DLNA_ENABLED must be true or false".to_owned()),
    };
    if !enabled {
        return Ok(None);
    }
    let interface = env::var("PUFFINBOX_DLNA_INTERFACE_ADDRESS")
        .map_err(|_| "PUFFINBOX_DLNA_INTERFACE_ADDRESS is required when DLNA is enabled")?
        .parse::<Ipv4Addr>()
        .map_err(|_| "PUFFINBOX_DLNA_INTERFACE_ADDRESS must be an IPv4 address")?;
    let origin_raw = env::var("PUFFINBOX_DLNA_ADVERTISED_ORIGIN")
        .map_err(|_| "PUFFINBOX_DLNA_ADVERTISED_ORIGIN is required when DLNA is enabled")?;
    let origin = Url::parse(&origin_raw)
        .map_err(|_| "PUFFINBOX_DLNA_ADVERTISED_ORIGIN must be a valid HTTP origin")?;
    let origin_ip = origin.host().and_then(|host| match host {
        url::Host::Ipv4(address) => Some(address),
        _ => None,
    });
    let server_bind_matches = match state.config.bind.ip() {
        IpAddr::V4(address) => address.is_unspecified() || address == interface,
        IpAddr::V6(_) => false,
    };
    if origin.scheme() != "http"
        || origin_ip != Some(interface)
        || is_reserved_target(interface)
        || is_network_boundary(interface, &state.config.local_networks)
        || !origin.username().is_empty()
        || origin.password().is_some()
        || origin.query().is_some()
        || origin.fragment().is_some()
        || !matches!(origin.path(), "/" | "")
        || origin.port_or_known_default() != Some(8096)
        || !server_bind_matches
        || state.config.bind.port() != 8096
        || !is_local_address(&state.config.local_networks, IpAddr::V4(interface))
    {
        return Err(
            "DLNA advertised origin must be an HTTP origin on the configured local IPv4 interface"
                .to_owned(),
        );
    }
    Ok(Some(DlnaSettings { interface, origin }))
}

fn set_multicast_interface(socket: &StdUdpSocket, interface: Ipv4Addr) -> std::io::Result<()> {
    let interface = libc::in_addr {
        s_addr: u32::from_ne_bytes(interface.octets()),
    };
    // SAFETY: `interface` points to a valid `in_addr` for the duration of the
    // call, and the size matches that value's representation.
    let result = unsafe {
        libc::setsockopt(
            socket.as_raw_fd(),
            libc::IPPROTO_IP,
            libc::IP_MULTICAST_IF,
            (&interface as *const libc::in_addr).cast(),
            std::mem::size_of::<libc::in_addr>() as libc::socklen_t,
        )
    };
    if result == -1 {
        Err(std::io::Error::last_os_error())
    } else {
        Ok(())
    }
}

async fn pair_device(
    State(app): State<AppState>,
    CurrentUser(user): CurrentUser,
    PeerInfo(peer): PeerInfo<SocketAddr>,
    Json(request): Json<PairRequest>,
) -> Result<(StatusCode, Json<PairingDto>), ApiError> {
    ensure_dlna_enabled(&app)?;
    if user.disabled || !user.allow_media_playback {
        return Err(ApiError::Forbidden);
    }
    let peer = normalized_ipv4(peer.ip()).ok_or(ApiError::Forbidden)?;
    let Some(settings) = settings_from_env(&app).map_err(|_| ApiError::Unavailable)? else {
        return Err(ApiError::NotFound);
    };
    if !is_local_address(&app.config.local_networks, IpAddr::V4(peer))
        || !is_local_address(&app.config.local_networks, IpAddr::V4(settings.interface))
    {
        return Err(ApiError::Forbidden);
    }
    let target = Ipv4Addr::from_str(request.client_address.trim())
        .map_err(|_| ApiError::BadRequest("ClientAddress must be an IPv4 address".to_owned()))?;
    if !is_local_address(&app.config.local_networks, IpAddr::V4(target))
        || is_reserved_target(target)
        || is_network_boundary(target, &app.config.local_networks)
    {
        return Err(ApiError::BadRequest(
            "ClientAddress must be a usable address in a configured local network".to_owned(),
        ));
    }
    let device_name = normalize_device_name(&request.device_name)?;
    let pairing = create_pairing(&app, user.id, target, &device_name).await?;
    let dto = pairing_dto(&settings.origin, pairing, Utc::now(), None)?;
    Ok((StatusCode::CREATED, Json(dto)))
}

async fn list_pairings(
    State(app): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> Result<Json<Vec<PairingDto>>, ApiError> {
    ensure_dlna_enabled(&app)?;
    let settings = settings_from_env(&app)
        .map_err(|_| ApiError::Unavailable)?
        .ok_or(ApiError::NotFound)?;
    let rows = sqlx::query(
        "SELECT id,user_id,host(client_address) AS client_address,device_name,created_at,last_seen_at FROM dlna_pairings WHERE user_id=$1 ORDER BY created_at DESC,id LIMIT $2",
    )
    .bind(user.id)
    .bind(MAX_PAIRINGS_PER_USER)
    .fetch_all(&app.db)
    .await?;
    rows.iter()
        .map(|row| {
            let pairing = PairingRecord {
                id: row.try_get("id")?,
                user_id: row.try_get("user_id")?,
                client_address: row
                    .try_get::<String, _>("client_address")?
                    .parse()
                    .map_err(|_| sqlx::Error::Protocol("invalid stored DLNA address".to_owned()))?,
                device_name: row.try_get("device_name")?,
            };
            pairing_dto(
                &settings.origin,
                pairing,
                row.try_get("created_at")?,
                row.try_get("last_seen_at")?,
            )
            .map_err(|_| sqlx::Error::Protocol("invalid DLNA description URL".to_owned()))
        })
        .collect::<Result<Vec<_>, _>>()
        .map(Json)
        .map_err(ApiError::from)
}

async fn unpair_device(
    State(app): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(pairing_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    ensure_dlna_enabled(&app)?;
    let mut tx = app.db.begin().await?;
    db::require_active_run(&mut tx, app.run_id).await?;
    let owner: Option<Uuid> =
        sqlx::query_scalar("SELECT user_id FROM dlna_pairings WHERE id=$1 FOR UPDATE")
            .bind(pairing_id)
            .fetch_optional(&mut *tx)
            .await?;
    let Some(owner) = owner else {
        tx.rollback().await?;
        return Err(ApiError::NotFound);
    };
    if owner != user.id && !user.is_admin {
        tx.rollback().await?;
        return Err(ApiError::NotFound);
    }
    sqlx::query("DELETE FROM dlna_pairings WHERE id=$1")
        .bind(pairing_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    remove_gena_subscriptions_for_pairing(pairing_id);
    Ok(StatusCode::NO_CONTENT)
}

async fn create_pairing(
    app: &AppState,
    user_id: Uuid,
    client_address: Ipv4Addr,
    device_name: &str,
) -> Result<PairingRecord, ApiError> {
    let mut tx = app.db.begin().await?;
    db::require_active_run(&mut tx, app.run_id).await?;
    sqlx::query("SELECT pg_advisory_xact_lock($1)")
        .bind(PAIRING_ADVISORY_LOCK)
        .execute(&mut *tx)
        .await?;
    let user_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dlna_pairings WHERE user_id=$1")
        .bind(user_id)
        .fetch_one(&mut *tx)
        .await?;
    let total_count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM dlna_pairings")
        .fetch_one(&mut *tx)
        .await?;
    let existing_owner: Option<Uuid> =
        sqlx::query_scalar("SELECT user_id FROM dlna_pairings WHERE client_address=$1::inet")
            .bind(client_address.to_string())
            .fetch_optional(&mut *tx)
            .await?;
    if existing_owner.is_some_and(|owner| owner != user_id) {
        tx.rollback().await?;
        return Err(ApiError::Conflict(
            "This renderer address is already paired to another account".to_owned(),
        ));
    }
    if existing_owner.is_none()
        && (user_count >= MAX_PAIRINGS_PER_USER || total_count >= MAX_PAIRINGS)
    {
        tx.rollback().await?;
        return Err(ApiError::RateLimited);
    }

    let id = Uuid::new_v4();
    let row = sqlx::query(
        "INSERT INTO dlna_pairings(id,user_id,client_address,device_name,enabled,created_at) VALUES ($1,$2,$3::inet,$4,TRUE,NOW()) ON CONFLICT(client_address) DO UPDATE SET id=EXCLUDED.id,device_name=EXCLUDED.device_name,enabled=TRUE,created_at=NOW(),last_seen_at=NULL WHERE dlna_pairings.user_id=EXCLUDED.user_id RETURNING id,user_id,host(client_address) AS client_address,device_name",
    )
    .bind(id)
    .bind(user_id)
    .bind(client_address.to_string())
    .bind(device_name)
    .fetch_optional(&mut *tx)
    .await?;
    let Some(row) = row else {
        tx.rollback().await?;
        return Err(ApiError::Conflict(
            "This renderer address is already paired to another account".to_owned(),
        ));
    };
    let pairing = PairingRecord {
        id: row.try_get("id")?,
        user_id: row.try_get("user_id")?,
        client_address: row
            .try_get::<String, _>("client_address")?
            .parse()
            .map_err(|_| ApiError::Internal("stored renderer address is invalid".to_owned()))?,
        device_name: row.try_get("device_name")?,
    };
    tx.commit().await?;
    Ok(pairing)
}

async fn device_description(
    State(app): State<AppState>,
    PeerInfo(peer): PeerInfo<SocketAddr>,
    Path(pairing_id): Path<Uuid>,
) -> Result<Response, ApiError> {
    ensure_dlna_enabled(&app)?;
    let (pairing, user) = authorize_pairing(&app, pairing_id, peer.ip()).await?;
    let settings = settings_from_env(&app)
        .map_err(|_| ApiError::Unavailable)?
        .ok_or(ApiError::NotFound)?;
    let xml = device_description_xml(
        &app.config.server_name,
        app.server_id,
        &pairing,
        &settings.origin,
    );
    let _ = user;
    xml_response(StatusCode::OK, xml)
}

async fn discovered_device_description(
    State(app): State<AppState>,
    PeerInfo(peer): PeerInfo<SocketAddr>,
) -> Result<Response, ApiError> {
    ensure_dlna_enabled(&app)?;
    let peer = normalized_ipv4(peer.ip()).ok_or(ApiError::NotFound)?;
    if !is_local_address(&app.config.local_networks, IpAddr::V4(peer)) {
        return Err(ApiError::NotFound);
    }
    let pairing = pairing_for_address(&app, peer)
        .await?
        .ok_or(ApiError::NotFound)?;
    let (pairing, _) = authorize_pairing(&app, pairing.id, IpAddr::V4(peer)).await?;
    let settings = settings_from_env(&app)
        .map_err(|_| ApiError::Unavailable)?
        .ok_or(ApiError::NotFound)?;
    let xml = device_description_xml(
        &app.config.server_name,
        app.server_id,
        &pairing,
        &settings.origin,
    );
    xml_response(StatusCode::OK, xml)
}

async fn content_directory_scpd(
    State(app): State<AppState>,
    PeerInfo(peer): PeerInfo<SocketAddr>,
    Path(pairing_id): Path<Uuid>,
) -> Result<Response, ApiError> {
    ensure_dlna_enabled(&app)?;
    authorize_pairing(&app, pairing_id, peer.ip()).await?;
    xml_response(StatusCode::OK, CONTENT_DIRECTORY_SCPD.to_owned())
}

async fn connection_manager_scpd(
    State(app): State<AppState>,
    PeerInfo(peer): PeerInfo<SocketAddr>,
    Path(pairing_id): Path<Uuid>,
) -> Result<Response, ApiError> {
    ensure_dlna_enabled(&app)?;
    authorize_pairing(&app, pairing_id, peer.ip()).await?;
    xml_response(StatusCode::OK, CONNECTION_MANAGER_SCPD.to_owned())
}

async fn content_directory_event(
    State(app): State<AppState>,
    PeerInfo(peer): PeerInfo<SocketAddr>,
    Path(pairing_id): Path<Uuid>,
    method: Method,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    ensure_dlna_enabled(&app)?;
    let (pairing, _) = authorize_pairing(&app, pairing_id, peer.ip()).await?;
    if ["callback", "sid", "nt", "timeout"]
        .iter()
        .any(|name| headers.get_all(*name).iter().count() > 1)
    {
        return Ok(gena_precondition_failed());
    }
    let bytes = to_bytes(body, MAX_GENA_REQUEST_BYTES)
        .await
        .map_err(|_| ApiError::BadRequest("GENA request body is too large".to_owned()))?;
    if !bytes.is_empty() {
        return Ok(gena_precondition_failed());
    }
    match method.as_str() {
        "SUBSCRIBE" => {
            let callback_header = single_header(&headers, "callback");
            let sid_header = single_header(&headers, "sid");
            let nt_header = single_header(&headers, "nt");
            let timeout_header = single_header(&headers, "timeout");
            let Some(timeout_seconds) = parse_gena_timeout(timeout_header.as_deref()) else {
                return Ok(gena_precondition_failed());
            };
            match (callback_header, sid_header, nt_header) {
                (Some(callback), None, Some(nt)) if nt.eq_ignore_ascii_case("upnp:event") => {
                    let Some(callback) = parse_gena_callback(
                        &callback,
                        pairing.client_address,
                        &app.config.local_networks,
                    ) else {
                        return Ok(gena_precondition_failed());
                    };
                    let revision = catalog_update_id(&app).await?;
                    let now = Instant::now();
                    let id = Uuid::new_v4();
                    let subscription = GenaSubscription {
                        id,
                        pairing_id,
                        client_address: pairing.client_address,
                        callback,
                        expires_at: now + Duration::from_secs(timeout_seconds),
                        next_sequence: 0,
                        last_revision: None,
                        notification_in_flight: true,
                    };
                    if !insert_gena_subscription(subscription.clone()) {
                        return Ok(gena_service_unavailable());
                    }
                    let state = app.clone();
                    tokio::spawn(async move {
                        time::sleep(GENA_INITIAL_NOTIFY_DELAY).await;
                        deliver_gena_notification(state, subscription, revision, 0).await;
                    });
                    Ok(gena_subscription_response(id, timeout_seconds))
                }
                (None, Some(sid), None)
                    if !headers.contains_key("callback") && !headers.contains_key("nt") =>
                {
                    let Some(id) = parse_gena_sid(&sid) else {
                        return Ok(gena_precondition_failed());
                    };
                    let mut subscriptions = gena_subscriptions()
                        .lock()
                        .unwrap_or_else(std::sync::PoisonError::into_inner);
                    let Some(subscription) = subscriptions.get_mut(&id) else {
                        return Ok(gena_precondition_failed());
                    };
                    if subscription.pairing_id != pairing_id
                        || subscription.client_address != pairing.client_address
                        || subscription.expires_at <= Instant::now()
                    {
                        subscriptions.remove(&id);
                        return Ok(gena_precondition_failed());
                    }
                    subscription.expires_at = Instant::now() + Duration::from_secs(timeout_seconds);
                    Ok(gena_subscription_response(id, timeout_seconds))
                }
                _ => Ok(gena_precondition_failed()),
            }
        }
        "UNSUBSCRIBE" => {
            if headers.contains_key("callback")
                || headers.contains_key("nt")
                || headers.contains_key("timeout")
            {
                return Ok(gena_precondition_failed());
            }
            let Some(sid) = single_header(&headers, "sid") else {
                return Ok(gena_precondition_failed());
            };
            let Some(id) = parse_gena_sid(&sid) else {
                return Ok(gena_precondition_failed());
            };
            let mut subscriptions = gena_subscriptions()
                .lock()
                .unwrap_or_else(std::sync::PoisonError::into_inner);
            let Some(subscription) = subscriptions.get(&id) else {
                return Ok(gena_precondition_failed());
            };
            if subscription.pairing_id != pairing_id
                || subscription.client_address != pairing.client_address
                || subscription.expires_at <= Instant::now()
            {
                subscriptions.remove(&id);
                return Ok(gena_precondition_failed());
            }
            subscriptions.remove(&id);
            let mut response = Response::new(Body::empty());
            *response.status_mut() = StatusCode::OK;
            Ok(response)
        }
        _ => {
            let mut response = Response::new(Body::empty());
            *response.status_mut() = StatusCode::METHOD_NOT_ALLOWED;
            response.headers_mut().insert(
                header::ALLOW,
                HeaderValue::from_static("SUBSCRIBE, UNSUBSCRIBE"),
            );
            Ok(response)
        }
    }
}

fn gena_subscriptions() -> &'static Mutex<HashMap<Uuid, GenaSubscription>> {
    GENA_SUBSCRIPTIONS.get_or_init(|| Mutex::new(HashMap::new()))
}

fn insert_gena_subscription(subscription: GenaSubscription) -> bool {
    let mut subscriptions = gena_subscriptions()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = Instant::now();
    subscriptions.retain(|_, existing| existing.expires_at > now);
    let per_pairing = subscriptions
        .values()
        .filter(|existing| existing.pairing_id == subscription.pairing_id)
        .count();
    if subscriptions.len() >= MAX_GENA_SUBSCRIPTIONS
        || per_pairing >= MAX_GENA_SUBSCRIPTIONS_PER_PAIRING
    {
        return false;
    }
    subscriptions.insert(subscription.id, subscription);
    true
}

fn single_header(headers: &HeaderMap, name: &str) -> Option<String> {
    let mut values = headers.get_all(name).iter();
    let value = values.next()?.to_str().ok()?.trim();
    if values.next().is_some() || value.is_empty() || value.len() > MAX_GENA_CALLBACK_URL_BYTES {
        return None;
    }
    Some(value.to_owned())
}

fn parse_gena_timeout(raw: Option<&str>) -> Option<u64> {
    let Some(raw) = raw else {
        return Some(MAX_GENA_TIMEOUT_SECONDS);
    };
    let value = raw.trim();
    let seconds = if value.eq_ignore_ascii_case("second-infinite") {
        MAX_GENA_TIMEOUT_SECONDS
    } else {
        value
            .get(..7)
            .filter(|prefix| prefix.eq_ignore_ascii_case("second-"))?;
        let count = value.get(7..)?.parse::<u64>().ok()?;
        if count == 0 {
            return None;
        }
        count.min(MAX_GENA_TIMEOUT_SECONDS)
    };
    Some(seconds)
}

fn parse_gena_callback(
    raw: &str,
    paired_address: Ipv4Addr,
    networks: &[ipnet::IpNet],
) -> Option<Url> {
    let raw = raw.trim();
    let enclosed = raw.strip_prefix('<')?.strip_suffix('>')?;
    if enclosed.is_empty()
        || enclosed.len() > MAX_GENA_CALLBACK_URL_BYTES
        || enclosed.bytes().any(|byte| {
            byte.is_ascii_control() || byte.is_ascii_whitespace() || matches!(byte, b'<' | b'>')
        })
    {
        return None;
    }
    let callback = Url::parse(enclosed).ok()?;
    let address = match callback.host()? {
        url::Host::Ipv4(address) => address,
        _ => return None,
    };
    if callback.scheme() != "http"
        || address != paired_address
        || !is_local_address(networks, IpAddr::V4(address))
        || is_reserved_target(address)
        || is_network_boundary(address, networks)
        || callback
            .port_or_known_default()
            .is_none_or(|port| port == 0)
        || !callback.username().is_empty()
        || callback.password().is_some()
        || callback.fragment().is_some()
    {
        return None;
    }
    Some(callback)
}

fn parse_gena_sid(raw: &str) -> Option<Uuid> {
    Uuid::parse_str(raw.strip_prefix("uuid:")?).ok()
}

fn gena_subscription_response(id: Uuid, timeout_seconds: u64) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::HeaderName::from_static("sid"),
        HeaderValue::from_str(&format!("uuid:{id}")).expect("UUID is a valid header value"),
    );
    response.headers_mut().insert(
        header::HeaderName::from_static("timeout"),
        HeaderValue::from_str(&format!("Second-{timeout_seconds}"))
            .expect("bounded timeout is a valid header value"),
    );
    response
}

fn gena_precondition_failed() -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::PRECONDITION_FAILED;
    response
}

fn gena_service_unavailable() -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::SERVICE_UNAVAILABLE;
    response
}

async fn deliver_gena_notification(
    state: AppState,
    subscription: GenaSubscription,
    revision: u32,
    sequence: u32,
) {
    let permit = match gena_notify_limiter().acquire_owned().await {
        Ok(permit) => permit,
        Err(_) => {
            remove_gena_subscription(subscription.id);
            return;
        }
    };
    let still_current = {
        let subscriptions = gena_subscriptions()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        subscriptions.get(&subscription.id).is_some_and(|current| {
            current.notification_in_flight
                && current.next_sequence == sequence
                && current.expires_at > Instant::now()
        })
    };
    if !still_current {
        return;
    }
    if authorize_pairing(
        &state,
        subscription.pairing_id,
        IpAddr::V4(subscription.client_address),
    )
    .await
    .is_err()
    {
        remove_gena_subscription(subscription.id);
        return;
    }
    let success = send_gena_notification(&subscription, revision, sequence, permit).await;
    let mut subscriptions = gena_subscriptions()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let Some(current) = subscriptions.get_mut(&subscription.id) else {
        return;
    };
    if current.next_sequence != sequence {
        return;
    }
    if success {
        current.last_revision = Some(revision);
        current.next_sequence = sequence.wrapping_add(1);
        current.notification_in_flight = false;
    } else {
        subscriptions.remove(&subscription.id);
    }
}

async fn send_gena_notification(
    subscription: &GenaSubscription,
    revision: u32,
    sequence: u32,
    _permit: OwnedSemaphorePermit,
) -> bool {
    let client = GENA_HTTP_CLIENT.get_or_init(|| {
        reqwest::Client::builder()
            .no_proxy()
            .redirect(reqwest::redirect::Policy::none())
            .connect_timeout(GENA_NOTIFY_TIMEOUT)
            .timeout(GENA_NOTIFY_TIMEOUT)
            .build()
            .map_err(|error| error.to_string())
    });
    let Ok(client) = client else {
        tracing::warn!("DLNA event callback client could not be initialized");
        return false;
    };
    let body = format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><e:propertyset xmlns:e=\"urn:schemas-upnp-org:event-1-0\"><e:property><SystemUpdateID>{revision}</SystemUpdateID></e:property><e:property><ContainerUpdateIDs>0,{revision}</ContainerUpdateIDs></e:property></e:propertyset>"
    );
    match client
        .request(
            reqwest::Method::from_bytes(b"NOTIFY").expect("NOTIFY is a valid HTTP method"),
            subscription.callback.clone(),
        )
        .header("NT", "upnp:event")
        .header("NTS", "upnp:propchange")
        .header("SID", format!("uuid:{}", subscription.id))
        .header("SEQ", sequence.to_string())
        .header("Content-Type", "text/xml; charset=\"utf-8\"")
        .body(body)
        .send()
        .await
    {
        Ok(response) if response.status().is_success() => true,
        Ok(response) => {
            tracing::debug!(status = %response.status(), "DLNA event callback rejected notification");
            false
        }
        Err(error) => {
            tracing::debug!(%error, "DLNA event callback failed");
            false
        }
    }
}

fn gena_notify_limiter() -> Arc<Semaphore> {
    GENA_NOTIFY_LIMITER
        .get_or_init(|| Arc::new(Semaphore::new(MAX_PARALLEL_GENA_NOTIFICATIONS)))
        .clone()
}

fn remove_gena_subscription(id: Uuid) {
    gena_subscriptions()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .remove(&id);
}

fn remove_gena_subscriptions_for_pairing(pairing_id: Uuid) {
    gena_subscriptions()
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner)
        .retain(|_, subscription| subscription.pairing_id != pairing_id);
}

async fn poll_gena_subscriptions(state: AppState) {
    let now = Instant::now();
    let should_read_revision = {
        let mut subscriptions = gena_subscriptions()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        subscriptions.retain(|_, subscription| subscription.expires_at > now);
        subscriptions.values().any(|subscription| {
            !subscription.notification_in_flight && subscription.last_revision.is_some()
        })
    };
    if !should_read_revision {
        return;
    }
    let Ok(revision) = catalog_update_id(&state).await else {
        return;
    };
    let changed = {
        let mut subscriptions = gena_subscriptions()
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        subscriptions
            .values_mut()
            .filter(|subscription| {
                !subscription.notification_in_flight
                    && subscription
                        .last_revision
                        .is_some_and(|seen| seen != revision)
            })
            .map(|subscription| {
                subscription.notification_in_flight = true;
                (subscription.clone(), subscription.next_sequence)
            })
            .collect::<Vec<_>>()
    };
    for (subscription, sequence) in changed {
        let state = state.clone();
        tokio::spawn(async move {
            deliver_gena_notification(state, subscription, revision, sequence).await;
        });
    }
}

async fn content_directory_control(
    State(app): State<AppState>,
    PeerInfo(peer): PeerInfo<SocketAddr>,
    Path(pairing_id): Path<Uuid>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    ensure_dlna_enabled(&app)?;
    let (_pairing, user) = authorize_pairing(&app, pairing_id, peer.ip()).await?;
    let settings = settings_from_env(&app)
        .map_err(|_| ApiError::Unavailable)?
        .ok_or(ApiError::NotFound)?;
    let action = parse_content_action(&headers)?;
    let bytes = to_bytes(body, MAX_SOAP_BYTES)
        .await
        .map_err(|_| ApiError::BadRequest("SOAP request exceeds the supported size".to_owned()))?;
    let xml = std::str::from_utf8(&bytes)
        .map_err(|_| ApiError::BadRequest("SOAP request must be UTF-8 XML".to_owned()))?;
    let args = parse_soap_arguments(xml, action.name())?;
    let response_fields = match action {
        ContentAction::Browse => {
            if args
                .values
                .get("SortCriteria")
                .is_some_and(|criteria| !criteria.is_empty())
            {
                return Ok(soap_fault(709, "Unsupported or invalid sort criteria"));
            }
            let request = parse_browse_request(&args)?;
            let (didl, returned, total, update_id) =
                browse(&app, &user, pairing_id, &settings.origin, &request).await?;
            format!(
                "<Result>{}</Result><NumberReturned>{returned}</NumberReturned><TotalMatches>{total}</TotalMatches><UpdateID>{update_id}</UpdateID>",
                xml_escape(&didl)
            )
        }
        ContentAction::GetSystemUpdateId => {
            let update_id = catalog_update_id(&app).await?;
            format!("<Id>{update_id}</Id>")
        }
        ContentAction::GetSearchCapabilities => "<SearchCaps></SearchCaps>".to_owned(),
        ContentAction::GetSortCapabilities => "<SortCaps></SortCaps>".to_owned(),
    };
    Ok(soap_success(
        CONTENT_DIRECTORY,
        action.name(),
        &response_fields,
    ))
}

async fn connection_manager_control(
    State(app): State<AppState>,
    PeerInfo(peer): PeerInfo<SocketAddr>,
    Path(pairing_id): Path<Uuid>,
    headers: HeaderMap,
    body: Body,
) -> Result<Response, ApiError> {
    ensure_dlna_enabled(&app)?;
    authorize_pairing(&app, pairing_id, peer.ip()).await?;
    let action = parse_connection_action(&headers)?;
    let bytes = to_bytes(body, MAX_SOAP_BYTES)
        .await
        .map_err(|_| ApiError::BadRequest("SOAP request exceeds the supported size".to_owned()))?;
    let xml = std::str::from_utf8(&bytes)
        .map_err(|_| ApiError::BadRequest("SOAP request must be UTF-8 XML".to_owned()))?;
    let args = parse_soap_arguments(xml, action.name())?;
    let fields = match action {
        ConnectionAction::ProtocolInfo if args.values.is_empty() => format!(
            "<Source>{}</Source><Sink></Sink>",
            xml_escape(&source_protocol_info())
        ),
        ConnectionAction::CurrentConnectionIds if args.values.is_empty() => {
            "<ConnectionIDs></ConnectionIDs>".to_owned()
        }
        ConnectionAction::CurrentConnectionInfo => {
            if args.values.len() != 1
                || args
                    .values
                    .get("ConnectionID")
                    .and_then(|value| value.parse::<i32>().ok())
                    .is_none()
            {
                return Ok(soap_fault(402, "Invalid Args"));
            }
            return Ok(soap_fault(706, "Invalid Connection Reference"));
        }
        _ => return Ok(soap_fault(402, "Invalid Args")),
    };
    Ok(soap_success(CONNECTION_MANAGER, action.name(), &fields))
}

async fn stream_item(
    State(app): State<AppState>,
    PeerInfo(peer): PeerInfo<SocketAddr>,
    Path((pairing_id, item_id)): Path<(Uuid, Uuid)>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    ensure_dlna_enabled(&app)?;
    let (_pairing, user) = authorize_pairing(&app, pairing_id, peer.ip()).await?;
    if user.disabled || !user.allow_media_playback {
        return Err(ApiError::NotFound);
    }
    let item = db::get_item(&app.db, item_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !db::item_visible_to_user(&app.db, &user, &item).await? {
        return Err(ApiError::NotFound);
    }
    let media_type = super::content_type_for(&item.path);
    if !supports_media_type(&media_type) {
        return Err(ApiError::NotFound);
    }
    let media = super::authorized_media(&app, &user, item_id).await?;
    super::stream_resolved(media, &headers, &method, false).await
}

async fn browse(
    app: &AppState,
    user: &UserRecord,
    pairing_id: Uuid,
    origin: &Url,
    request: &BrowseRequest,
) -> Result<(String, i64, i64, u32), ApiError> {
    let update_id = catalog_update_id(app).await?;
    if request.browse_metadata {
        if request.object_id == "0" {
            let didl = didl_root();
            return Ok((didl, 1, 1, update_id));
        }
        let id = Uuid::parse_str(&request.object_id)
            .map_err(|_| ApiError::BadRequest("ObjectID is not a valid identifier".to_owned()))?;
        if let Some(library) = db::get_library(&app.db, id).await? {
            if !db::library_visible_to_user(&app.db, user, library.id).await? {
                return Err(ApiError::NotFound);
            }
            return Ok((didl_document(&[didl_library(&library)]), 1, 1, update_id));
        }
        let item = db::get_item(&app.db, id).await?.ok_or(ApiError::NotFound)?;
        if !db::item_visible_to_user(&app.db, user, &item).await? {
            return Err(ApiError::NotFound);
        }
        return Ok((
            didl_document(&[didl_item(&item, item.parent_id, pairing_id, origin)]),
            1,
            1,
            update_id,
        ));
    }

    if request.object_id == "0" {
        let libraries = db::list_libraries(&app.db, user).await?;
        let total = libraries.len() as i64;
        let page = libraries
            .iter()
            .skip(request.starting_index as usize)
            .take(request.requested_count as usize)
            .map(didl_library)
            .collect::<Vec<_>>();
        return Ok((didl_document(&page), page.len() as i64, total, update_id));
    }

    let object_id = Uuid::parse_str(&request.object_id)
        .map_err(|_| ApiError::BadRequest("ObjectID is not a valid identifier".to_owned()))?;
    if let Some(library) = db::get_library(&app.db, object_id).await? {
        if !db::library_visible_to_user(&app.db, user, library.id).await? {
            return Err(ApiError::NotFound);
        }
        let query = ItemQuery {
            parent_id: Some(library.id),
            recursive: false,
            start_index: request.starting_index,
            limit: request.requested_count,
            enable_total_record_count: true,
            sort_by: "SortName".to_owned(),
            sort_order: "Ascending".to_owned(),
            ..ItemQuery::default()
        };
        let (items, total) = db::browse_items(&app.db, user, query).await?;
        let page = items
            .iter()
            .map(|item| didl_item(item, Some(library.id), pairing_id, origin))
            .collect::<Vec<_>>();
        return Ok((
            didl_document(&page),
            page.len() as i64,
            total.unwrap_or(0),
            update_id,
        ));
    }

    let parent = db::get_item(&app.db, object_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !db::item_visible_to_user(&app.db, user, &parent).await? {
        return Err(ApiError::NotFound);
    }
    if !is_container_type(&parent.item_type) {
        return Err(ApiError::BadRequest(
            "ObjectID does not identify a container".to_owned(),
        ));
    }
    let query = ItemQuery {
        parent_id: Some(parent.id),
        recursive: false,
        start_index: request.starting_index,
        limit: request.requested_count,
        enable_total_record_count: true,
        sort_by: "SortName".to_owned(),
        sort_order: "Ascending".to_owned(),
        ..ItemQuery::default()
    };
    let (items, total) = db::browse_items(&app.db, user, query).await?;
    let page = items
        .iter()
        .map(|item| didl_item(item, Some(parent.id), pairing_id, origin))
        .collect::<Vec<_>>();
    Ok((
        didl_document(&page),
        page.len() as i64,
        total.unwrap_or(0),
        update_id,
    ))
}

async fn catalog_update_id(app: &AppState) -> Result<u32, ApiError> {
    let revision: i64 =
        sqlx::query_scalar("SELECT revision FROM dlna_catalog_revision WHERE singleton=TRUE")
            .fetch_one(&app.db)
            .await?;
    u32::try_from(revision)
        .map_err(|_| ApiError::Internal("DLNA catalog revision is out of range".to_owned()))
}

async fn authorize_pairing(
    app: &AppState,
    pairing_id: Uuid,
    peer: IpAddr,
) -> Result<(PairingRecord, UserRecord), ApiError> {
    let peer = normalized_ipv4(peer).ok_or(ApiError::NotFound)?;
    if !is_local_address(&app.config.local_networks, IpAddr::V4(peer)) {
        return Err(ApiError::NotFound);
    }
    let row = sqlx::query(
        "SELECT id,user_id,host(client_address) AS client_address,device_name FROM dlna_pairings WHERE id=$1 AND client_address=$2::inet AND enabled=TRUE",
    )
    .bind(pairing_id)
    .bind(peer.to_string())
    .fetch_optional(&app.db)
    .await?
    .ok_or(ApiError::NotFound)?;
    let pairing = PairingRecord {
        id: row.try_get("id")?,
        user_id: row.try_get("user_id")?,
        client_address: row
            .try_get::<String, _>("client_address")?
            .parse()
            .map_err(|_| ApiError::Internal("stored renderer address is invalid".to_owned()))?,
        device_name: row.try_get("device_name")?,
    };
    let user = db::get_user(&app.db, pairing.user_id)
        .await?
        .filter(|user| !user.disabled && user.allow_media_playback)
        .ok_or(ApiError::NotFound)?;
    Ok((pairing, user))
}

fn is_local_address(networks: &[ipnet::IpNet], address: IpAddr) -> bool {
    networks.iter().any(|network| network.contains(&address))
}

fn normalized_ipv4(address: IpAddr) -> Option<Ipv4Addr> {
    match address {
        IpAddr::V4(value) => Some(value),
        IpAddr::V6(_) => None,
    }
}

fn normalize_device_name(raw: &str) -> Result<String, ApiError> {
    let value: String = raw
        .chars()
        .filter(|character| !character.is_control())
        .collect::<String>()
        .trim()
        .to_owned();
    if value.is_empty() || value.chars().count() > 128 {
        return Err(ApiError::BadRequest(
            "DeviceName must contain 1 to 128 characters".to_owned(),
        ));
    }
    Ok(value)
}

fn is_reserved_target(address: Ipv4Addr) -> bool {
    address.is_unspecified() || address.is_multicast() || address == Ipv4Addr::BROADCAST
}

fn is_network_boundary(address: Ipv4Addr, networks: &[ipnet::IpNet]) -> bool {
    networks.iter().any(|network| {
        let ipnet::IpNet::V4(network) = network else {
            return false;
        };
        network.prefix_len() <= 30
            && (address == network.network() || address == network.broadcast())
    })
}

fn ensure_dlna_enabled(app: &AppState) -> Result<(), ApiError> {
    match settings_from_env(app) {
        Ok(Some(_)) => Ok(()),
        Ok(None) => Err(ApiError::NotFound),
        Err(_) => Err(ApiError::Unavailable),
    }
}

fn pairing_dto(
    origin: &Url,
    pairing: PairingRecord,
    created_at: DateTime<Utc>,
    last_seen_at: Option<DateTime<Utc>>,
) -> Result<PairingDto, ApiError> {
    let description_url = origin
        .join(&format!("Puffinbox/Dlna/{}/description.xml", pairing.id))
        .map_err(|_| ApiError::Unavailable)?
        .to_string();
    Ok(PairingDto {
        pairing_id: pairing.id,
        device_name: pairing.device_name,
        client_address: pairing.client_address.to_string(),
        description_url,
        created_at,
        last_seen_at,
    })
}

async fn run_ssdp(
    state: AppState,
    socket: UdpSocket,
    settings: DlnaSettings,
    mut stopped: oneshot::Receiver<()>,
) -> bool {
    let mut buffer = [0u8; 2048];
    let mut event_poll = time::interval(GENA_POLL_INTERVAL);
    event_poll.set_missed_tick_behavior(time::MissedTickBehavior::Skip);
    let mut advertisements_active = false;
    let mut last_alive = None;
    loop {
        tokio::select! {
            _ = &mut stopped => {
                if advertisements_active {
                    let messages = ssdp_announcement_messages(&settings.origin, state.server_id, false);
                    for attempt in 0..2 {
                        if send_multicast_messages(&socket, &messages).await {
                            break;
                        }
                        if attempt == 0 {
                            time::sleep(Duration::from_millis(100)).await;
                        }
                    }
                }
                return true;
            }
            _ = event_poll.tick() => {
                poll_gena_subscriptions(state.clone()).await;
                refresh_ssdp_advertisements(
                    &state,
                    &socket,
                    &settings,
                    &mut advertisements_active,
                    &mut last_alive,
                ).await;
            }
            received = socket.recv_from(&mut buffer) => {
                let Ok((length, remote)) = received else {
                    tracing::warn!("DLNA SSDP receive failed; stopping discovery service");
                    return false;
                };
                let SocketAddr::V4(remote) = remote else { continue; };
                if !is_local_address(&state.config.local_networks, IpAddr::V4(*remote.ip())) {
                    continue;
                }
                let Some(target) = parse_msearch(&buffer[..length]) else { continue; };
                if !allow_search(*remote.ip()) { continue; }
                let Ok(Some(pairing)) = pairing_for_address(&state, *remote.ip()).await else { continue; };
                let Ok(user) = db::get_user(&state.db, pairing.user_id).await else { continue; };
                if user.is_none_or(|user| user.disabled || !user.allow_media_playback) { continue; }
                send_search_responses(
                    &socket,
                    &settings,
                    state.server_id,
                    target,
                    remote,
                ).await;
            }
        }
    }
}

async fn refresh_ssdp_advertisements(
    state: &AppState,
    socket: &UdpSocket,
    settings: &DlnaSettings,
    advertisements_active: &mut bool,
    last_alive: &mut Option<Instant>,
) {
    let active_addresses: Result<Vec<String>, sqlx::Error> = sqlx::query_scalar(
        "SELECT host(p.client_address) FROM dlna_pairings p JOIN users u ON u.id=p.user_id WHERE p.enabled=TRUE AND u.disabled=FALSE AND u.allow_media_playback=TRUE ORDER BY p.id LIMIT $1",
    )
    .bind(MAX_PAIRINGS)
    .fetch_all(&state.db)
    .await;
    let Ok(active_addresses) = active_addresses else {
        tracing::debug!("could not refresh DLNA SSDP advertisements from pairing policy");
        return;
    };
    let has_active_pairing = active_addresses.iter().any(|address| {
        address.parse::<Ipv4Addr>().ok().is_some_and(|address| {
            is_local_address(&state.config.local_networks, IpAddr::V4(address))
        })
    });

    if has_active_pairing {
        let refresh_due =
            last_alive.is_none_or(|last| last.elapsed() >= SSDP_ADVERTISEMENT_REFRESH);
        if !*advertisements_active || refresh_due {
            let messages = ssdp_announcement_messages(&settings.origin, state.server_id, true);
            if send_multicast_messages(socket, &messages).await {
                *advertisements_active = true;
                *last_alive = Some(Instant::now());
            }
        }
    } else if *advertisements_active {
        let messages = ssdp_announcement_messages(&settings.origin, state.server_id, false);
        if send_multicast_messages(socket, &messages).await {
            *advertisements_active = false;
            *last_alive = None;
        }
    }
}

async fn send_multicast_messages(socket: &UdpSocket, messages: &[String]) -> bool {
    let destination = SocketAddrV4::new(SSDP_GROUP, SSDP_PORT);
    let mut complete = true;
    for message in messages {
        if let Err(error) = socket.send_to(message.as_bytes(), destination).await {
            tracing::debug!(%error, "DLNA SSDP multicast send failed");
            complete = false;
        }
    }
    complete
}

async fn pairing_for_address(
    state: &AppState,
    address: Ipv4Addr,
) -> Result<Option<PairingRecord>, sqlx::Error> {
    let row = sqlx::query(
        "SELECT id,user_id,host(client_address) AS client_address,device_name FROM dlna_pairings WHERE client_address=$1::inet AND enabled=TRUE",
    )
    .bind(address.to_string())
    .fetch_optional(&state.db)
    .await?;
    let Some(row) = row else {
        return Ok(None);
    };
    Ok(Some(PairingRecord {
        id: row.try_get("id")?,
        user_id: row.try_get("user_id")?,
        client_address: row
            .try_get::<String, _>("client_address")?
            .parse()
            .map_err(|_| sqlx::Error::Protocol("invalid stored DLNA address".to_owned()))?,
        device_name: row.try_get("device_name")?,
    }))
}

async fn send_search_responses(
    socket: &UdpSocket,
    settings: &DlnaSettings,
    server_id: Uuid,
    target: SearchTarget,
    remote: SocketAddrV4,
) {
    let delay_millis = (rand::random::<u16>() % 250) as u64;
    time::sleep(Duration::from_millis(delay_millis)).await;
    for message in search_responses(&settings.origin, server_id, target) {
        let _ = socket.send_to(message.as_bytes(), remote).await;
    }
}

fn search_responses(origin: &Url, server_id: Uuid, target: SearchTarget) -> Vec<String> {
    let targets: Vec<String> = match target {
        SearchTarget::All => vec![
            "upnp:rootdevice".to_owned(),
            format!("uuid:{server_id}"),
            MEDIA_SERVER.to_owned(),
            CONTENT_DIRECTORY.to_owned(),
            CONNECTION_MANAGER.to_owned(),
        ],
        SearchTarget::RootDevice => vec!["upnp:rootdevice".to_owned()],
        SearchTarget::DeviceUuid(requested) if requested == server_id => {
            vec![format!("uuid:{server_id}")]
        }
        SearchTarget::DeviceUuid(_) => Vec::new(),
        SearchTarget::Device => vec![MEDIA_SERVER.to_owned()],
        SearchTarget::ContentDirectory => vec![CONTENT_DIRECTORY.to_owned()],
        SearchTarget::ConnectionManager => vec![CONNECTION_MANAGER.to_owned()],
    };
    targets
        .into_iter()
        .map(|target| {
            let concrete = target;
            let usn = if concrete == "upnp:rootdevice" {
                format!("uuid:{server_id}::upnp:rootdevice")
            } else if concrete.starts_with("uuid:") {
                concrete.clone()
            } else {
                format!("uuid:{server_id}::{concrete}")
            };
            let location = origin
                .join("Puffinbox/Dlna/description.xml")
                .map(|url| url.to_string())
                .unwrap_or_default();
            format!(
                "HTTP/1.1 200 OK\r\nCACHE-CONTROL: max-age={SSDP_MAX_AGE_SECONDS}\r\nEXT:\r\nLOCATION: {location}\r\nSERVER: Linux/1 UPnP/1.1 Puffinbox/0.1\r\nST: {concrete}\r\nUSN: {usn}\r\n\r\n"
            )
        })
        .collect()
}

fn ssdp_announcement_messages(origin: &Url, server_id: Uuid, alive: bool) -> Vec<String> {
    let targets = [
        "upnp:rootdevice".to_owned(),
        format!("uuid:{server_id}"),
        MEDIA_SERVER.to_owned(),
        CONTENT_DIRECTORY.to_owned(),
        CONNECTION_MANAGER.to_owned(),
    ];
    let location = origin
        .join("Puffinbox/Dlna/description.xml")
        .map(|url| url.to_string())
        .unwrap_or_default();
    targets
        .into_iter()
        .map(|target| {
            let usn = if target == "upnp:rootdevice" {
                format!("uuid:{server_id}::upnp:rootdevice")
            } else if target.starts_with("uuid:") {
                target.clone()
            } else {
                format!("uuid:{server_id}::{target}")
            };
            if alive {
                format!(
                    "NOTIFY * HTTP/1.1\r\nHOST: {SSDP_GROUP}:{SSDP_PORT}\r\nCACHE-CONTROL: max-age={SSDP_MAX_AGE_SECONDS}\r\nLOCATION: {location}\r\nNT: {target}\r\nNTS: ssdp:alive\r\nSERVER: Linux/1 UPnP/1.1 Puffinbox/0.1\r\nUSN: {usn}\r\n\r\n"
                )
            } else {
                format!(
                    "NOTIFY * HTTP/1.1\r\nHOST: {SSDP_GROUP}:{SSDP_PORT}\r\nNT: {target}\r\nNTS: ssdp:byebye\r\nUSN: {usn}\r\n\r\n"
                )
            }
        })
        .collect()
}

fn source_protocol_info() -> String {
    SAFE_MEDIA_TYPES
        .iter()
        .map(|media_type| {
            format!(
                "http-get:*:{media_type}:DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000"
            )
        })
        .collect::<Vec<_>>()
        .join(",")
}

fn supports_media_type(media_type: &str) -> bool {
    SAFE_MEDIA_TYPES.contains(&media_type)
}

fn parse_msearch(datagram: &[u8]) -> Option<SearchTarget> {
    if datagram.len() > 2048 || datagram.contains(&0) || !datagram.ends_with(b"\r\n\r\n") {
        return None;
    }
    let text = std::str::from_utf8(datagram).ok()?;
    let text = text.strip_suffix("\r\n\r\n")?;
    let mut lines = text.split("\r\n");
    if lines.next()? != "M-SEARCH * HTTP/1.1" {
        return None;
    }
    let mut fields = HashMap::<String, String>::new();
    for line in lines {
        if line.is_empty() {
            return None;
        }
        let (raw_name, raw_value) = line.split_once(':')?;
        if raw_name.trim() != raw_name
            || raw_name.is_empty()
            || !raw_name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return None;
        }
        let name = raw_name.to_ascii_lowercase();
        let value = raw_value.trim();
        if !matches!(name.as_str(), "host" | "man" | "mx" | "st" | "user-agent")
            || value.len() > 512
            || value.bytes().any(|byte| byte.is_ascii_control())
            || fields.insert(name, value.to_owned()).is_some()
        {
            return None;
        }
    }
    if fields.get("man")?.trim() != "\"ssdp:discover\""
        || fields.get("host")?.trim() != "239.255.255.250:1900"
    {
        return None;
    }
    let mx = fields.get("mx")?.parse::<u8>().ok()?;
    if !(1..=5).contains(&mx) {
        return None;
    }
    match fields.get("st")?.as_str() {
        "ssdp:all" => Some(SearchTarget::All),
        "upnp:rootdevice" => Some(SearchTarget::RootDevice),
        MEDIA_SERVER => Some(SearchTarget::Device),
        CONTENT_DIRECTORY => Some(SearchTarget::ContentDirectory),
        CONNECTION_MANAGER => Some(SearchTarget::ConnectionManager),
        value if value.starts_with("uuid:") => Uuid::parse_str(&value[5..])
            .ok()
            .map(SearchTarget::DeviceUuid),
        _ => None,
    }
}

fn allow_search(address: Ipv4Addr) -> bool {
    let limiter = SEARCH_LIMITER.get_or_init(|| Mutex::new(HashMap::new()));
    let mut limiter = limiter
        .lock()
        .unwrap_or_else(std::sync::PoisonError::into_inner);
    let now = Instant::now();
    limiter.retain(|_, seen| now.duration_since(*seen) < Duration::from_secs(60));
    if limiter
        .get(&address)
        .is_some_and(|seen| now.duration_since(*seen) < Duration::from_millis(250))
    {
        return false;
    }
    if limiter.len() >= 4096 && !limiter.contains_key(&address) {
        return false;
    }
    limiter.insert(address, now);
    true
}

fn parse_content_action(headers: &HeaderMap) -> Result<ContentAction, ApiError> {
    let action = parse_soap_action(headers)?;
    let prefix = format!("{CONTENT_DIRECTORY}#");
    let name = action.strip_prefix(&prefix).ok_or(ApiError::BadRequest(
        "SOAPAction service does not match ContentDirectory".to_owned(),
    ))?;
    match name {
        "Browse" => Ok(ContentAction::Browse),
        "GetSystemUpdateID" => Ok(ContentAction::GetSystemUpdateId),
        "GetSearchCapabilities" => Ok(ContentAction::GetSearchCapabilities),
        "GetSortCapabilities" => Ok(ContentAction::GetSortCapabilities),
        _ => Err(ApiError::BadRequest(
            "Unsupported ContentDirectory action".to_owned(),
        )),
    }
}

fn parse_connection_action(headers: &HeaderMap) -> Result<ConnectionAction, ApiError> {
    let action = parse_soap_action(headers)?;
    let prefix = format!("{CONNECTION_MANAGER}#");
    let name = action.strip_prefix(&prefix).ok_or(ApiError::BadRequest(
        "SOAPAction service does not match ConnectionManager".to_owned(),
    ))?;
    match name {
        "GetProtocolInfo" => Ok(ConnectionAction::ProtocolInfo),
        "GetCurrentConnectionIDs" => Ok(ConnectionAction::CurrentConnectionIds),
        "GetCurrentConnectionInfo" => Ok(ConnectionAction::CurrentConnectionInfo),
        _ => Err(ApiError::BadRequest(
            "Unsupported ConnectionManager action".to_owned(),
        )),
    }
}

fn parse_soap_action(headers: &HeaderMap) -> Result<String, ApiError> {
    let mut values = headers.get_all("soapaction").iter();
    let Some(value) = values.next() else {
        return Err(ApiError::BadRequest(
            "SOAPAction header is required".to_owned(),
        ));
    };
    if values.next().is_some() {
        return Err(ApiError::BadRequest(
            "SOAPAction header must appear once".to_owned(),
        ));
    }
    let value = value
        .to_str()
        .map_err(|_| ApiError::BadRequest("SOAPAction header is invalid".to_owned()))?
        .trim();
    let action = value
        .strip_prefix('"')
        .and_then(|v| v.strip_suffix('"'))
        .unwrap_or(value);
    if action.len() > 256 || action.bytes().any(|byte| byte.is_ascii_control()) {
        return Err(ApiError::BadRequest(
            "SOAPAction header is invalid".to_owned(),
        ));
    }
    Ok(action.to_owned())
}

fn parse_soap_arguments(xml: &str, expected_action: &str) -> Result<SoapArguments, ApiError> {
    if xml.len() > MAX_SOAP_BYTES {
        return Err(ApiError::BadRequest(
            "SOAP request exceeds the supported size".to_owned(),
        ));
    }
    let mut reader = Reader::from_str(xml);
    reader.config_mut().trim_text(false);
    let mut stack = Vec::<String>::with_capacity(8);
    let mut args = SoapArguments::default();
    let mut root_seen = false;
    let mut root_closed = false;
    let mut body_seen = false;
    let mut action_seen = false;
    let mut current_arg: Option<String> = None;
    let mut current_text = String::new();
    let mut event_count = 0usize;
    loop {
        match reader.read_event() {
            Ok(Event::Start(element)) => {
                bump_soap_event(&mut event_count)?;
                check_soap_attributes(&element)?;
                if stack.is_empty() {
                    if root_seen
                        || root_closed
                        || local_name(element.local_name().as_ref())? != "Envelope"
                    {
                        return Err(bad_soap("invalid SOAP envelope"));
                    }
                    root_seen = true;
                }
                if stack.len() >= MAX_SOAP_DEPTH {
                    return Err(bad_soap("SOAP nesting is too deep"));
                }
                let name = local_name(element.local_name().as_ref())?;
                match stack.len() {
                    1 if name == "Body" => {
                        if body_seen {
                            return Err(bad_soap("SOAP body is duplicated"));
                        }
                        body_seen = true;
                    }
                    1 => return Err(bad_soap("SOAP envelope contains an unsupported element")),
                    2 if name == expected_action && body_seen => {
                        if action_seen {
                            return Err(bad_soap("SOAP action is duplicated"));
                        }
                        action_seen = true;
                    }
                    2 if body_seen => {
                        return Err(bad_soap("SOAP body action does not match SOAPAction"));
                    }
                    3 if action_seen => {
                        if !args.values.contains_key(&name) {
                            args.values.insert(name.clone(), String::new());
                            current_arg = Some(name.clone());
                            current_text.clear();
                        } else {
                            return Err(bad_soap("SOAP argument is duplicated"));
                        }
                    }
                    0 | 3 => {}
                    _ => return Err(bad_soap("nested SOAP argument is unsupported")),
                }
                stack.push(name);
            }
            Ok(Event::Empty(element)) => {
                bump_soap_event(&mut event_count)?;
                check_soap_attributes(&element)?;
                let name = local_name(element.local_name().as_ref())?;
                if stack.len() == 2 && body_seen && name == expected_action {
                    if action_seen {
                        return Err(bad_soap("SOAP action is duplicated"));
                    }
                    action_seen = true;
                } else if stack.len() == 3 && action_seen {
                    if args.values.insert(name, String::new()).is_some() {
                        return Err(bad_soap("SOAP argument is duplicated"));
                    }
                } else if stack.len() > 3 && current_arg.is_some() {
                    return Err(bad_soap("nested SOAP argument is unsupported"));
                } else if stack.is_empty() {
                    return Err(bad_soap("SOAP envelope cannot be empty"));
                }
            }
            Ok(Event::Text(text)) => {
                bump_soap_event(&mut event_count)?;
                let decoded = text
                    .decode()
                    .map_err(|_| bad_soap("SOAP text encoding is invalid"))?;
                if stack.is_empty() {
                    if !decoded.trim().is_empty() {
                        return Err(bad_soap("text appears outside SOAP envelope"));
                    }
                } else if current_arg.is_some() {
                    let value =
                        unescape(&decoded).map_err(|_| bad_soap("SOAP entity is invalid"))?;
                    append_soap_text(&mut current_text, &value)?;
                } else if !decoded.trim().is_empty() {
                    return Err(bad_soap("unexpected text in SOAP envelope"));
                }
            }
            Ok(Event::CData(text)) => {
                bump_soap_event(&mut event_count)?;
                if current_arg.is_some() {
                    let decoded = text
                        .decode()
                        .map_err(|_| bad_soap("SOAP text encoding is invalid"))?;
                    append_soap_text(&mut current_text, &decoded)?;
                } else {
                    return Err(bad_soap("unexpected CDATA in SOAP envelope"));
                }
            }
            Ok(Event::GeneralRef(reference)) => {
                bump_soap_event(&mut event_count)?;
                let character = if let Some(character) = reference
                    .resolve_char_ref()
                    .map_err(|_| bad_soap("SOAP entity is invalid"))?
                {
                    character
                } else {
                    let entity = std::str::from_utf8(reference.as_ref())
                        .map_err(|_| bad_soap("SOAP entity is invalid"))?;
                    let resolved = quick_xml::escape::resolve_predefined_entity(entity)
                        .ok_or(bad_soap("SOAP entity is not supported"))?;
                    let mut chars = resolved.chars();
                    let character = chars.next().ok_or(bad_soap("SOAP entity is invalid"))?;
                    if chars.next().is_some() {
                        return Err(bad_soap("SOAP entity is invalid"));
                    }
                    character
                };
                if current_arg.is_some() {
                    append_soap_text(&mut current_text, &character.to_string())?;
                } else {
                    return Err(bad_soap("unexpected entity in SOAP envelope"));
                }
            }
            Ok(Event::End(element)) => {
                bump_soap_event(&mut event_count)?;
                let name = local_name(element.local_name().as_ref())?;
                let Some(open) = stack.pop() else {
                    return Err(bad_soap("SOAP close tag has no matching open tag"));
                };
                if open != name {
                    return Err(bad_soap("SOAP tags are not balanced"));
                }
                if stack.len() == 3 && current_arg.as_deref() == Some(name.as_str()) {
                    args.values.insert(name, current_text.trim().to_owned());
                    current_arg = None;
                    current_text.clear();
                }
                if stack.is_empty() {
                    root_closed = true;
                }
            }
            Ok(Event::DocType(_)) => return Err(bad_soap("SOAP document types are not allowed")),
            Ok(Event::Eof) => break,
            Ok(_) => bump_soap_event(&mut event_count)?,
            Err(_) => return Err(bad_soap("SOAP XML is malformed")),
        }
    }
    if !root_seen
        || !root_closed
        || !body_seen
        || !action_seen
        || !stack.is_empty()
        || current_arg.is_some()
    {
        return Err(bad_soap("SOAP envelope or action is incomplete"));
    }
    Ok(args)
}

fn check_soap_attributes(element: &quick_xml::events::BytesStart<'_>) -> Result<(), ApiError> {
    let mut count = 0usize;
    for attribute in element.attributes().with_checks(true) {
        let attribute = attribute.map_err(|_| bad_soap("SOAP attribute is malformed"))?;
        count += 1;
        if count > 32 || attribute.key.as_ref().len() > 128 || attribute.value.len() > 1024 {
            return Err(bad_soap("SOAP attribute limit exceeded"));
        }
    }
    Ok(())
}

fn append_soap_text(target: &mut String, text: &str) -> Result<(), ApiError> {
    if target.len().saturating_add(text.len()) > MAX_SOAP_TEXT_BYTES {
        return Err(bad_soap("SOAP argument text is too large"));
    }
    target.push_str(text);
    Ok(())
}

fn bump_soap_event(count: &mut usize) -> Result<(), ApiError> {
    *count = count.saturating_add(1);
    if *count > MAX_SOAP_EVENTS {
        Err(bad_soap("SOAP event limit exceeded"))
    } else {
        Ok(())
    }
}

fn local_name(raw: &[u8]) -> Result<String, ApiError> {
    let raw = std::str::from_utf8(raw).map_err(|_| bad_soap("SOAP element name is invalid"))?;
    let name = raw.rsplit(':').next().unwrap_or(raw);
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
    {
        return Err(bad_soap("SOAP element name is invalid"));
    }
    Ok(name.to_owned())
}

fn parse_browse_request(args: &SoapArguments) -> Result<BrowseRequest, ApiError> {
    let object_id = required_argument(args, "ObjectID")?;
    let browse_flag = args
        .values
        .get("BrowseFlag")
        .map(String::as_str)
        .unwrap_or("BrowseDirectChildren");
    let browse_metadata = match browse_flag {
        "BrowseMetadata" => true,
        "BrowseDirectChildren" => false,
        _ => return Err(bad_soap("BrowseFlag is invalid")),
    };
    let starting_index = optional_nonnegative(args, "StartingIndex", 0)?;
    let requested_count = optional_nonnegative(args, "RequestedCount", MAX_BROWSE_RESULTS)?;
    if starting_index > MAX_BROWSE_OFFSET {
        return Err(bad_soap("StartingIndex exceeds the supported offset"));
    }
    let requested_count = if requested_count == 0 {
        MAX_BROWSE_RESULTS
    } else {
        requested_count.min(MAX_BROWSE_RESULTS)
    };
    Ok(BrowseRequest {
        object_id,
        browse_metadata,
        starting_index,
        requested_count,
    })
}

fn required_argument(args: &SoapArguments, name: &str) -> Result<String, ApiError> {
    let value = args.values.get(name).cloned().unwrap_or_default();
    if value.is_empty() || value.len() > 128 {
        return Err(bad_soap("A required SOAP argument is missing or too long"));
    }
    Ok(value)
}

fn optional_nonnegative(args: &SoapArguments, name: &str, default: i64) -> Result<i64, ApiError> {
    let Some(raw) = args.values.get(name).filter(|value| !value.is_empty()) else {
        return Ok(default);
    };
    let value = raw
        .parse::<i64>()
        .map_err(|_| bad_soap("SOAP count argument is invalid"))?;
    if value < 0 {
        return Err(bad_soap("SOAP count argument cannot be negative"));
    }
    Ok(value)
}

fn bad_soap(message: &str) -> ApiError {
    ApiError::BadRequest(message.to_owned())
}

fn is_container_type(item_type: &str) -> bool {
    matches!(
        item_type.to_ascii_lowercase().as_str(),
        "folder"
            | "collectionfolder"
            | "season"
            | "boxset"
            | "series"
            | "musicartist"
            | "musicalbum"
    )
}

fn didl_root() -> String {
    didl_document(&["<container id=\"0\" parentID=\"-1\" restricted=\"1\"><dc:title>Puffinbox</dc:title><upnp:class>object.container</upnp:class></container>".to_owned()])
}

fn didl_library(library: &LibraryRecord) -> String {
    format!(
        "<container id=\"{}\" parentID=\"0\" restricted=\"1\"><dc:title>{}</dc:title><upnp:class>object.container.storageFolder</upnp:class></container>",
        library.id,
        xml_escape(&library.name)
    )
}

fn didl_item(
    item: &ItemRecord,
    parent_override: Option<Uuid>,
    pairing_id: Uuid,
    origin: &Url,
) -> String {
    if is_container_type(&item.item_type) {
        return format!(
            "<container id=\"{}\" parentID=\"{}\" restricted=\"1\"><dc:title>{}</dc:title><upnp:class>object.container.storageFolder</upnp:class></container>",
            item.id,
            parent_override
                .or(item.parent_id)
                .map(|id| id.to_string())
                .unwrap_or_else(|| "0".to_owned()),
            xml_escape(&item.name)
        );
    }
    let content_type = super::content_type_for(&item.path);
    let class = match content_type.split('/').next().unwrap_or_default() {
        "video" => "object.item.videoItem",
        "audio" => "object.item.audioItem.musicTrack",
        "image" => "object.item.imageItem.photo",
        _ => "object.item",
    };
    let parent_id = parent_override
        .or(item.parent_id)
        .map(|id| id.to_string())
        .unwrap_or_else(|| "0".to_owned());
    let resource = origin
        .join(&format!("Puffinbox/Dlna/{pairing_id}/media/{}", item.id))
        .map(|url| url.to_string())
        .unwrap_or_default();
    let size = item
        .size_bytes
        .map(|value| format!(" size=\"{value}\""))
        .unwrap_or_default();
    let duration = item
        .runtime_ticks
        .and_then(format_duration)
        .map(|value| format!(" duration=\"{value}\""))
        .unwrap_or_default();
    if !supports_media_type(&content_type) {
        return format!(
            "<item id=\"{}\" parentID=\"{}\" restricted=\"1\"><dc:title>{}</dc:title><upnp:class>{class}</upnp:class></item>",
            item.id,
            xml_escape(&parent_id),
            xml_escape(&item.name),
        );
    }
    format!(
        "<item id=\"{}\" parentID=\"{}\" restricted=\"1\"><dc:title>{}</dc:title><upnp:class>{class}</upnp:class><res protocolInfo=\"http-get:*:{}:DLNA.ORG_OP=01;DLNA.ORG_CI=0;DLNA.ORG_FLAGS=01700000000000000000000000000000\"{size}{duration}>{}</res></item>",
        item.id,
        xml_escape(&parent_id),
        xml_escape(&item.name),
        xml_escape(&content_type),
        xml_escape(&resource),
    )
}

fn didl_document(nodes: &[String]) -> String {
    format!(
        "<DIDL-Lite xmlns=\"urn:schemas-upnp-org:metadata-1-0/DIDL-Lite/\" xmlns:dc=\"http://purl.org/dc/elements/1.1/\" xmlns:upnp=\"urn:schemas-upnp-org:metadata-1-0/upnp/\">{}</DIDL-Lite>",
        nodes.join("")
    )
}

fn format_duration(ticks: i64) -> Option<String> {
    if ticks < 0 {
        return None;
    }
    let millis = ticks / 10_000;
    let hours = millis / 3_600_000;
    let minutes = (millis / 60_000) % 60;
    let seconds = (millis / 1_000) % 60;
    let fraction = millis % 1_000;
    Some(format!(
        "{hours:02}:{minutes:02}:{seconds:02}.{fraction:03}"
    ))
}

fn xml_escape(raw: &str) -> String {
    let mut escaped = String::with_capacity(raw.len());
    for mut character in raw.chars() {
        let codepoint = character as u32;
        let xml_character = matches!(character, '\t' | '\n' | '\r')
            || ((0x20..=0x10_FFFF).contains(&codepoint)
                && codepoint % 0x1_0000 != 0xFFFE
                && codepoint % 0x1_0000 != 0xFFFF);
        if !xml_character {
            character = '\u{FFFD}';
        }
        match character {
            '&' => escaped.push_str("&amp;"),
            '<' => escaped.push_str("&lt;"),
            '>' => escaped.push_str("&gt;"),
            '"' => escaped.push_str("&quot;"),
            '\'' => escaped.push_str("&apos;"),
            character => escaped.push(character),
        }
    }
    escaped
}

fn soap_success(service: &str, action: &str, fields: &str) -> Response {
    xml_response(
        StatusCode::OK,
        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><u:{action}Response xmlns:u=\"{service}\">{fields}</u:{action}Response></s:Body></s:Envelope>"
        ),
    )
    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn soap_fault(code: u16, description: &str) -> Response {
    xml_response(
        StatusCode::INTERNAL_SERVER_ERROR,
        format!(
            "<?xml version=\"1.0\" encoding=\"utf-8\"?><s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\" s:encodingStyle=\"http://schemas.xmlsoap.org/soap/encoding/\"><s:Body><s:Fault><faultcode>s:Client</faultcode><faultstring>UPnPError</faultstring><detail><UPnPError xmlns=\"urn:schemas-upnp-org:control-1-0\"><errorCode>{code}</errorCode><errorDescription>{}</errorDescription></UPnPError></detail></s:Fault></s:Body></s:Envelope>",
            xml_escape(description)
        ),
    )
    .unwrap_or_else(|_| StatusCode::INTERNAL_SERVER_ERROR.into_response())
}

fn xml_response(status: StatusCode, xml: String) -> Result<Response, ApiError> {
    if xml.len() > MAX_SOAP_BYTES * 8 {
        return Err(ApiError::Unavailable);
    }
    let mut response = Response::new(Body::from(xml));
    *response.status_mut() = status;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_static("text/xml; charset=utf-8"),
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}

fn device_description_xml(
    server_name: &str,
    server_id: Uuid,
    pairing: &PairingRecord,
    origin: &Url,
) -> String {
    let base = origin
        .join(&format!("Puffinbox/Dlna/{}/", pairing.id))
        .map(|url| url.to_string())
        .unwrap_or_default();
    let udn = format!("uuid:{server_id}");
    format!(
        "<?xml version=\"1.0\" encoding=\"utf-8\"?><root xmlns=\"urn:schemas-upnp-org:device-1-0\"><specVersion><major>1</major><minor>0</minor></specVersion><device><deviceType>{MEDIA_SERVER}</deviceType><friendlyName>{}</friendlyName><manufacturer>Puffinbox</manufacturer><modelName>Puffinbox Media Server</modelName><UDN>{udn}</UDN><serviceList><service><serviceType>{CONTENT_DIRECTORY}</serviceType><serviceId>urn:upnp-org:serviceId:ContentDirectory</serviceId><SCPDURL>{base}ContentDirectory/scpd.xml</SCPDURL><controlURL>{base}ContentDirectory/control</controlURL><eventSubURL>{base}ContentDirectory/event</eventSubURL></service><service><serviceType>{CONNECTION_MANAGER}</serviceType><serviceId>urn:upnp-org:serviceId:ConnectionManager</serviceId><SCPDURL>{base}ConnectionManager/scpd.xml</SCPDURL><controlURL>{base}ConnectionManager/control</controlURL></service></serviceList><presentationURL>{}</presentationURL></device></root>",
        xml_escape(server_name),
        xml_escape(origin.as_str())
    )
}

const CONTENT_DIRECTORY_SCPD: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?><scpd xmlns=\"urn:schemas-upnp-org:service-1-0\"><specVersion><major>1</major><minor>0</minor></specVersion><actionList><action><name>Browse</name><argumentList><argument><name>ObjectID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_ObjectID</relatedStateVariable></argument><argument><name>BrowseFlag</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_BrowseFlag</relatedStateVariable></argument><argument><name>Filter</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Filter</relatedStateVariable></argument><argument><name>StartingIndex</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Index</relatedStateVariable></argument><argument><name>RequestedCount</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_Count</relatedStateVariable></argument><argument><name>SortCriteria</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_SortCriteria</relatedStateVariable></argument><argument><name>Result</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Result</relatedStateVariable></argument><argument><name>NumberReturned</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Count</relatedStateVariable></argument><argument><name>TotalMatches</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Count</relatedStateVariable></argument><argument><name>UpdateID</name><direction>out</direction><relatedStateVariable>SystemUpdateID</relatedStateVariable></argument></argumentList></action><action><name>GetSystemUpdateID</name><argumentList><argument><name>Id</name><direction>out</direction><relatedStateVariable>SystemUpdateID</relatedStateVariable></argument></argumentList></action><action><name>GetSearchCapabilities</name><argumentList><argument><name>SearchCaps</name><direction>out</direction><relatedStateVariable>SearchCapabilities</relatedStateVariable></argument></argumentList></action><action><name>GetSortCapabilities</name><argumentList><argument><name>SortCaps</name><direction>out</direction><relatedStateVariable>SortCapabilities</relatedStateVariable></argument></argumentList></action></actionList><serviceStateTable><stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_ObjectID</name><dataType>string</dataType></stateVariable><stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_BrowseFlag</name><dataType>string</dataType></stateVariable><stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_Filter</name><dataType>string</dataType></stateVariable><stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_Index</name><dataType>ui4</dataType></stateVariable><stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_Count</name><dataType>ui4</dataType></stateVariable><stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_SortCriteria</name><dataType>string</dataType></stateVariable><stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_Result</name><dataType>string</dataType></stateVariable><stateVariable sendEvents=\"yes\"><name>SystemUpdateID</name><dataType>ui4</dataType></stateVariable><stateVariable sendEvents=\"yes\"><name>ContainerUpdateIDs</name><dataType>string</dataType></stateVariable><stateVariable sendEvents=\"no\"><name>SearchCapabilities</name><dataType>string</dataType></stateVariable><stateVariable sendEvents=\"no\"><name>SortCapabilities</name><dataType>string</dataType></stateVariable></serviceStateTable></scpd>";

const CONNECTION_MANAGER_SCPD: &str = "<?xml version=\"1.0\" encoding=\"utf-8\"?><scpd xmlns=\"urn:schemas-upnp-org:service-1-0\"><specVersion><major>1</major><minor>0</minor></specVersion><actionList><action><name>GetProtocolInfo</name><argumentList><argument><name>Source</name><direction>out</direction><relatedStateVariable>SourceProtocolInfo</relatedStateVariable></argument><argument><name>Sink</name><direction>out</direction><relatedStateVariable>SinkProtocolInfo</relatedStateVariable></argument></argumentList></action><action><name>GetCurrentConnectionIDs</name><argumentList><argument><name>ConnectionIDs</name><direction>out</direction><relatedStateVariable>CurrentConnectionIDs</relatedStateVariable></argument></argumentList></action><action><name>GetCurrentConnectionInfo</name><argumentList><argument><name>ConnectionID</name><direction>in</direction><relatedStateVariable>A_ARG_TYPE_ConnectionID</relatedStateVariable></argument><argument><name>RcsID</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_RcsID</relatedStateVariable></argument><argument><name>AVTransportID</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_AVTransportID</relatedStateVariable></argument><argument><name>ProtocolInfo</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_ProtocolInfo</relatedStateVariable></argument><argument><name>PeerConnectionManager</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_ConnectionManager</relatedStateVariable></argument><argument><name>PeerConnectionID</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_ConnectionID</relatedStateVariable></argument><argument><name>Direction</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_Direction</relatedStateVariable></argument><argument><name>Status</name><direction>out</direction><relatedStateVariable>A_ARG_TYPE_ConnectionStatus</relatedStateVariable></argument></argumentList></action></actionList><serviceStateTable><stateVariable sendEvents=\"no\"><name>SourceProtocolInfo</name><dataType>string</dataType></stateVariable><stateVariable sendEvents=\"no\"><name>SinkProtocolInfo</name><dataType>string</dataType></stateVariable><stateVariable sendEvents=\"no\"><name>CurrentConnectionIDs</name><dataType>string</dataType></stateVariable><stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_ConnectionID</name><dataType>i4</dataType></stateVariable><stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_RcsID</name><dataType>i4</dataType></stateVariable><stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_AVTransportID</name><dataType>i4</dataType></stateVariable><stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_ProtocolInfo</name><dataType>string</dataType></stateVariable><stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_ConnectionManager</name><dataType>string</dataType></stateVariable><stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_Direction</name><dataType>string</dataType></stateVariable><stateVariable sendEvents=\"no\"><name>A_ARG_TYPE_ConnectionStatus</name><dataType>string</dataType></stateVariable></serviceStateTable></scpd>";

impl ContentAction {
    fn name(self) -> &'static str {
        match self {
            Self::Browse => "Browse",
            Self::GetSystemUpdateId => "GetSystemUpdateID",
            Self::GetSearchCapabilities => "GetSearchCapabilities",
            Self::GetSortCapabilities => "GetSortCapabilities",
        }
    }
}

impl ConnectionAction {
    fn name(self) -> &'static str {
        match self {
            Self::ProtocolInfo => "GetProtocolInfo",
            Self::CurrentConnectionIds => "GetCurrentConnectionIDs",
            Self::CurrentConnectionInfo => "GetCurrentConnectionInfo",
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn msearch(st: &str) -> Vec<u8> {
        format!(
            "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 2\r\nST: {st}\r\n\r\n"
        )
        .into_bytes()
    }

    #[test]
    fn accepts_only_supported_ssdp_searches() {
        assert_eq!(
            parse_msearch(&msearch("upnp:rootdevice")),
            Some(SearchTarget::RootDevice)
        );
        assert_eq!(parse_msearch(&msearch("ssdp:all")), Some(SearchTarget::All));
        assert_eq!(
            parse_msearch(&msearch(CONTENT_DIRECTORY)),
            Some(SearchTarget::ContentDirectory)
        );
        assert_eq!(
            parse_msearch(&msearch("urn:attacker:service:Anything:1")),
            None
        );
        let mut duplicate = msearch("ssdp:all");
        let insertion = b"MX: 2\r\n";
        let end = duplicate
            .windows(2)
            .position(|window| window == b"\r\n")
            .unwrap()
            + 2;
        duplicate.splice(end..end, insertion.iter().copied());
        assert_eq!(parse_msearch(&duplicate), None);
    }

    #[test]
    fn rejects_ssdp_smuggling_and_unbounded_values() {
        for request in [
            "M-SEARCH * HTTP/1.0\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: ssdp:all\r\n\r\n",
            "M-SEARCH * HTTP/1.1\r\nHOST: attacker:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: ssdp:all\r\n\r\n",
            "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: ssdp:discover\r\nMX: 1\r\nST: ssdp:all\r\n\r\n",
            "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 9\r\nST: ssdp:all\r\n\r\n",
            "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: ssdp:all\r\nX: attacker\r\n\r\nbody",
            "M-SEARCH * HTTP/1.1\r\nHOST: 239.255.255.250:1900\r\nMAN: \"ssdp:discover\"\r\nMX: 1\r\nST: ssdp:all\r\nX: attacker\r\n\r\n",
        ] {
            assert_eq!(parse_msearch(request.as_bytes()), None);
        }
        let unknown_header = msearch("ssdp:all");
        let unknown_header = String::from_utf8(unknown_header)
            .unwrap()
            .replace("\r\n\r\n", "\r\nX-Forwarded-For: 192.168.1.20\r\n\r\n");
        assert_eq!(parse_msearch(unknown_header.as_bytes()), None);
        assert_eq!(parse_msearch(&vec![b'A'; 2049]), None);
    }

    #[test]
    fn search_response_uses_server_identity_and_generic_description_url() {
        let pairing = PairingRecord {
            id: Uuid::parse_str("a88485b6-fec2-4aa8-a10d-80257f5faf40").unwrap(),
            user_id: Uuid::new_v4(),
            client_address: Ipv4Addr::new(192, 168, 1, 20),
            device_name: "Living room display".to_owned(),
        };
        let server_id = Uuid::parse_str("507679b4-39a0-4566-b9d4-25dff2e0640a").unwrap();
        let origin = Url::parse("http://192.168.1.10:8096/").unwrap();
        let response = search_responses(&origin, server_id, SearchTarget::RootDevice).remove(0);
        assert!(response.contains(&format!("uuid:{server_id}::upnp:rootdevice")));
        assert!(response.contains("Puffinbox/Dlna/description.xml"));
        assert!(!response.contains(&pairing.id.to_string()));
        assert!(!response.to_ascii_lowercase().contains("token"));
        assert!(
            search_responses(&origin, server_id, SearchTarget::DeviceUuid(Uuid::new_v4()))
                .is_empty()
        );
        assert_eq!(
            search_responses(&origin, server_id, SearchTarget::DeviceUuid(server_id)).len(),
            1
        );
    }

    #[test]
    fn ssdp_announcements_have_standard_fields_without_pairing_urls() {
        let pairing_id = Uuid::parse_str("a88485b6-fec2-4aa8-a10d-80257f5faf40").unwrap();
        let server_id = Uuid::parse_str("507679b4-39a0-4566-b9d4-25dff2e0640a").unwrap();
        let origin = Url::parse("http://192.168.1.10:8096/").unwrap();
        let alive = ssdp_announcement_messages(&origin, server_id, true);
        let byebye = ssdp_announcement_messages(&origin, server_id, false);

        assert_eq!(alive.len(), 5);
        assert_eq!(byebye.len(), 5);
        for message in alive {
            assert!(message.starts_with("NOTIFY * HTTP/1.1\r\n"));
            assert!(message.contains("HOST: 239.255.255.250:1900\r\n"));
            assert!(message.contains("CACHE-CONTROL: max-age=180\r\n"));
            assert!(
                message.contains(
                    "LOCATION: http://192.168.1.10:8096/Puffinbox/Dlna/description.xml\r\n"
                )
            );
            assert!(message.contains("NTS: ssdp:alive\r\n"));
            assert!(message.contains(&server_id.to_string()));
            assert!(!message.contains(&pairing_id.to_string()));
            assert!(!message.contains(&format!("/Puffinbox/Dlna/{pairing_id}/")));
        }
        for message in byebye {
            assert!(message.contains("NTS: ssdp:byebye\r\n"));
            assert!(!message.contains("LOCATION:"));
            assert!(message.contains(&server_id.to_string()));
            assert!(!message.contains(&pairing_id.to_string()));
        }
    }

    #[test]
    fn ssdp_multicast_egress_is_selected_from_the_configured_interface() {
        let socket = StdUdpSocket::bind(SocketAddrV4::new(Ipv4Addr::UNSPECIFIED, 0)).unwrap();
        set_multicast_interface(&socket, Ipv4Addr::LOCALHOST).unwrap();
    }

    #[test]
    fn device_description_advertises_supported_content_directory_eventing() {
        let pairing = PairingRecord {
            id: Uuid::parse_str("a88485b6-fec2-4aa8-a10d-80257f5faf40").unwrap(),
            user_id: Uuid::new_v4(),
            client_address: Ipv4Addr::new(192, 168, 1, 20),
            device_name: "Living room display".to_owned(),
        };
        let server_id = Uuid::parse_str("507679b4-39a0-4566-b9d4-25dff2e0640a").unwrap();
        let origin = Url::parse("http://192.168.1.10:8096/").unwrap();
        let description = device_description_xml("Puffinbox", server_id, &pairing, &origin);

        assert!(description.contains(&format!("<UDN>uuid:{server_id}</UDN>")));
        assert!(description.contains(&format!("/Puffinbox/Dlna/{}/", pairing.id)));
        assert!(description.contains("ContentDirectory/scpd.xml"));
        assert!(description.contains("ContentDirectory/control"));
        assert!(description.contains("ContentDirectory/event"));
        assert!(description.contains("<eventSubURL>"));
        assert!(description.contains("ConnectionManager/scpd.xml"));
        assert!(description.contains("ConnectionManager/control"));
        assert!(
            CONTENT_DIRECTORY_SCPD
                .contains("<stateVariable sendEvents=\"yes\"><name>SystemUpdateID</name>")
        );
        assert!(
            CONTENT_DIRECTORY_SCPD
                .contains("<stateVariable sendEvents=\"yes\"><name>ContainerUpdateIDs</name>")
        );
        assert!(CONNECTION_MANAGER_SCPD.contains("<stateVariable sendEvents=\"no\">"));
    }

    #[test]
    fn gena_subscription_inputs_are_bounded_and_peer_scoped() {
        assert_eq!(parse_gena_timeout(None), Some(MAX_GENA_TIMEOUT_SECONDS));
        assert_eq!(parse_gena_timeout(Some("Second-infinite")), Some(1800));
        assert_eq!(parse_gena_timeout(Some("second-7200")), Some(1800));
        assert_eq!(parse_gena_timeout(Some("Second-60")), Some(60));
        for timeout in ["Second-0", "Second--1", "Second-abc", "Other-20"] {
            assert_eq!(parse_gena_timeout(Some(timeout)), None, "{timeout}");
        }

        let peer = Ipv4Addr::new(192, 168, 10, 42);
        let networks = vec!["192.168.10.0/24".parse().unwrap()];
        assert!(
            parse_gena_callback("<http://192.168.10.42:12345/notify?sid=x>", peer, &networks)
                .is_some()
        );
        for callback in [
            "<https://192.168.10.42:12345/notify>",
            "<http://192.168.10.43:12345/notify>",
            "<http://localhost:12345/notify>",
            "<http://user@192.168.10.42:12345/notify>",
            "<http://192.168.10.42:12345/notify#fragment>",
            "<http://192.168.11.42:12345/notify>",
            "<http://192.168.10.42:12345/notify><http://192.168.10.42:12346/other>",
        ] {
            assert!(
                parse_gena_callback(callback, peer, &networks).is_none(),
                "accepted unsafe callback {callback}"
            );
        }
    }

    #[test]
    fn parses_bounded_browse_action_and_rejects_invalid_xml() {
        let xml = soap_browse_body("0", "BrowseDirectChildren", "0", "25");
        let args = parse_soap_arguments(&xml, "Browse").unwrap();
        let request = parse_browse_request(&args).unwrap();
        assert_eq!(request.object_id, "0");
        assert_eq!(request.requested_count, 25);
        assert!(
            parse_soap_arguments(
                "<!DOCTYPE x [<!ENTITY x SYSTEM 'file:///etc/passwd'>]><s:Envelope/>",
                "Browse"
            )
            .is_err()
        );
        assert!(
            parse_soap_arguments(
                "<s:Envelope><s:Body><u:Browse><ObjectID>0</u:Browse></s:Body></s:Envelope>",
                "Browse"
            )
            .is_err()
        );
        assert!(parse_soap_arguments("<s:Envelope/><s:Envelope/>", "Browse").is_err());
        assert!(parse_soap_arguments("<s:Envelope><s:Body><u:GetSystemUpdateID/><u:GetSystemUpdateID/></s:Body></s:Envelope>", "GetSystemUpdateID").is_err());
        assert!(
            parse_soap_arguments(
                "<s:Envelope><s:Body></s:Body><s:Body></s:Body></s:Envelope>",
                "Browse"
            )
            .is_err()
        );
        assert!(parse_soap_arguments("<s:Envelope><s:Body><u:Browse><ObjectID>0</ObjectID><Nested><Value>x</Value></Nested></u:Browse></s:Body></s:Envelope>", "Browse").is_err());
    }

    #[test]
    fn rejects_duplicate_and_oversized_soap_arguments() {
        let duplicate = "<s:Envelope><s:Body><u:Browse><ObjectID>0</ObjectID><ObjectID>1</ObjectID></u:Browse></s:Body></s:Envelope>";
        assert!(parse_soap_arguments(duplicate, "Browse").is_err());
        let too_much = format!(
            "<s:Envelope><s:Body><u:Browse><ObjectID>{}</ObjectID></u:Browse></s:Body></s:Envelope>",
            "a".repeat(MAX_SOAP_TEXT_BYTES + 1)
        );
        assert!(parse_soap_arguments(&too_much, "Browse").is_err());
        assert!(
            parse_browse_request(&SoapArguments {
                values: HashMap::from([
                    ("ObjectID".to_owned(), "0".to_owned()),
                    ("StartingIndex".to_owned(), "-1".to_owned())
                ])
            })
            .is_err()
        );
    }

    #[test]
    fn escaping_keeps_catalog_metadata_inside_xml_text_and_attributes() {
        assert_eq!(xml_escape("A < B & \"C\""), "A &lt; B &amp; &quot;C&quot;");
        let id = Uuid::new_v4();
        let item = ItemRecord {
            id,
            library_id: Uuid::new_v4(),
            parent_id: None,
            name: "</dc:title><attack>".to_owned(),
            sort_name: "bad".to_owned(),
            item_type: "Movie".to_owned(),
            path: "movie.mp4".into(),
            container: Some("mp4".to_owned()),
            size_bytes: Some(10),
            runtime_ticks: None,
            date_added: Utc::now(),
            date_modified: None,
            rating: None,
            overview: None,
            metadata_json: serde_json::Value::Object(Default::default()),
        };
        let origin = Url::parse("http://192.168.1.1:8096/").unwrap();
        let node = didl_item(&item, None, Uuid::new_v4(), &origin);
        assert!(!node.contains("</dc:title><attack>"));
        assert!(node.contains("&lt;/dc:title&gt;&lt;attack&gt;"));
    }

    #[test]
    fn unsupported_files_have_no_http_resource() {
        let item = ItemRecord {
            id: Uuid::new_v4(),
            library_id: Uuid::new_v4(),
            parent_id: None,
            name: "book.epub".to_owned(),
            sort_name: "book.epub".to_owned(),
            item_type: "Book".to_owned(),
            path: "book.epub".into(),
            container: Some("epub".to_owned()),
            size_bytes: Some(10),
            runtime_ticks: None,
            date_added: Utc::now(),
            date_modified: None,
            rating: None,
            overview: None,
            metadata_json: serde_json::Value::Object(Default::default()),
        };
        let origin = Url::parse("http://192.168.1.1:8096/").unwrap();
        let node = didl_item(&item, None, Uuid::new_v4(), &origin);
        assert!(node.contains("<item"));
        assert!(!node.contains("<res "));
        assert!(!supports_media_type("application/epub+zip"));
    }

    #[test]
    fn pairing_target_rejects_multicast_and_unconfigured_addresses() {
        assert!(is_reserved_target(Ipv4Addr::BROADCAST));
        assert!(is_reserved_target(Ipv4Addr::UNSPECIFIED));
        assert!(is_reserved_target(Ipv4Addr::new(239, 1, 2, 3)));
        let local = vec!["192.168.10.0/24".parse().unwrap()];
        assert!(is_local_address(
            &local,
            IpAddr::V4(Ipv4Addr::new(192, 168, 10, 9))
        ));
        assert!(!is_local_address(
            &local,
            IpAddr::V4(Ipv4Addr::new(192, 168, 11, 9))
        ));
        assert!(is_network_boundary(Ipv4Addr::new(192, 168, 10, 0), &local));
        assert!(is_network_boundary(
            Ipv4Addr::new(192, 168, 10, 255),
            &local
        ));
        assert!(!is_network_boundary(Ipv4Addr::new(192, 168, 10, 9), &local));
        assert_eq!(
            normalized_ipv4("::ffff:192.168.10.3".parse().unwrap()),
            None
        );
        assert_eq!(normalized_ipv4("2001:db8::2".parse().unwrap()), None);
        assert!(normalize_device_name(&"a".repeat(129)).is_err());
        assert_eq!(
            normalize_device_name(&"é".repeat(128))
                .unwrap()
                .chars()
                .count(),
            128
        );
    }

    #[test]
    fn protocol_info_lists_only_the_content_types_with_resources() {
        let source = source_protocol_info();
        assert!(source.contains("http-get:*:video/mp4:"));
        assert!(source.contains("http-get:*:audio/flac:"));
        assert!(!source.contains("http-get:*:*:"));
        assert!(
            SAFE_MEDIA_TYPES
                .iter()
                .all(|mime| supports_media_type(mime))
        );
    }

    #[test]
    fn xml_escape_replaces_forbidden_xml_controls() {
        assert_eq!(xml_escape("a\u{0001}b"), "a\u{FFFD}b");
    }

    #[test]
    fn browse_page_is_capped_and_rejects_huge_offsets() {
        let request = parse_browse_request(&SoapArguments {
            values: HashMap::from([
                ("ObjectID".to_owned(), "0".to_owned()),
                ("RequestedCount".to_owned(), "99999".to_owned()),
            ]),
        })
        .unwrap();
        assert_eq!(request.requested_count, MAX_BROWSE_RESULTS);
        let huge = SoapArguments {
            values: HashMap::from([
                ("ObjectID".to_owned(), "0".to_owned()),
                (
                    "StartingIndex".to_owned(),
                    (MAX_BROWSE_OFFSET + 1).to_string(),
                ),
            ]),
        };
        assert!(parse_browse_request(&huge).is_err());
    }

    fn soap_browse_body(object_id: &str, flag: &str, start: &str, count: &str) -> String {
        format!(
            "<s:Envelope xmlns:s=\"http://schemas.xmlsoap.org/soap/envelope/\"><s:Body><u:Browse xmlns:u=\"{CONTENT_DIRECTORY}\"><ObjectID>{object_id}</ObjectID><BrowseFlag>{flag}</BrowseFlag><Filter>*</Filter><StartingIndex>{start}</StartingIndex><RequestedCount>{count}</RequestedCount><SortCriteria></SortCriteria></u:Browse></s:Body></s:Envelope>"
        )
    }
}
