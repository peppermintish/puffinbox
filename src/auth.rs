use std::{
    net::{IpAddr, SocketAddr},
    sync::OnceLock,
};

use argon2::{
    Argon2,
    password_hash::{PasswordHasher, PasswordVerifier, phc::PasswordHash},
};
use axum::{
    extract::{FromRequestParts, connect_info::ConnectInfo},
    http::{HeaderMap, HeaderValue, Method, header},
};
use base64::{Engine, engine::general_purpose::URL_SAFE_NO_PAD};
use chrono::{Duration, Utc};
use ipnet::IpNet;
use rand::{RngCore, rngs::OsRng};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use uuid::Uuid;

use crate::{config::Config, db, error::ApiError, state::AppState};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct UserRecord {
    pub id: Uuid,
    pub username: String,
    pub is_admin: bool,
    pub disabled: bool,
    pub enable_remote_access: bool,
    pub allow_media_playback: bool,
    pub enable_content_downloading: bool,
    pub enable_live_tv_access: bool,
    pub enable_live_tv_management: bool,
    pub restrict_libraries: bool,
    pub max_parental_rating: Option<i32>,
    pub block_unrated_items: Vec<String>,
    pub allowed_library_ids: Vec<Uuid>,
    #[serde(default)]
    pub configuration: crate::user_settings::UserConfiguration,
}

#[derive(Clone, Debug)]
pub struct CurrentUser(pub UserRecord);

#[derive(Clone, Debug)]
pub struct MediaUser(pub UserRecord);

pub(crate) struct SocketIdentity {
    pub user: UserRecord,
    pub token_hash: String,
    pub local_client: bool,
}

impl FromRequestParts<AppState> for SocketIdentity {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let (token, from_cookie) = socket_token(&parts.headers, parts.uri.query())?;
        // Browser upgrades can carry cookies from another origin. Apply the
        // HTTP origin policy even though an upgrade uses GET.
        if (from_cookie || parts.headers.contains_key(header::ORIGIN))
            && !origin_is_same_site_origin(parts, &state.config)
        {
            return Err(ApiError::Forbidden);
        }
        let token_hash = token_digest(&token);
        let local_client = is_local_client(parts, &state.config);
        let CurrentUser(user) = authenticate_parts(parts, state, (token, from_cookie)).await?;
        Ok(Self {
            user,
            token_hash,
            local_client,
        })
    }
}

fn socket_token(headers: &HeaderMap, query: Option<&str>) -> Result<(String, bool), ApiError> {
    let mut candidate = extract_token(headers)?;
    if let Some(query) = query {
        if query.len() > 2048 {
            return Err(ApiError::Unauthorized);
        }
        let mut query_token_seen = false;
        for (name, value) in url::form_urlencoded::parse(query.as_bytes()) {
            if matches!(name.as_ref(), "api_key" | "ApiKey") {
                if query_token_seen {
                    return Err(ApiError::Unauthorized);
                }
                query_token_seen = true;
                add_auth_candidate(&mut candidate, value.into_owned(), false)?;
            }
        }
    }
    candidate.ok_or(ApiError::Unauthorized)
}

#[derive(Clone, Debug)]
pub struct AdminUser(pub UserRecord);

impl FromRequestParts<AppState> for CurrentUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        authenticate_parts(
            parts,
            state,
            extract_token(&parts.headers)?.ok_or(ApiError::Unauthorized)?,
        )
        .await
    }
}

impl FromRequestParts<AppState> for MediaUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let header_or_cookie_token = extract_token(&parts.headers)?;
        let api_key = extract_media_query_token(&parts.method, parts.uri.query())?;
        if let (Some((token, from_cookie)), Some(query_token)) = (&header_or_cookie_token, &api_key)
        {
            if token == query_token {
                let CurrentUser(user) =
                    authenticate_parts(parts, state, (token.clone(), *from_cookie)).await?;
                return Ok(Self(user));
            }
            if !scoped_media_route(parts) {
                return Err(ApiError::Unauthorized);
            }
            let (media_parent_id, media_user) =
                active_scoped_media_identity(parts, state, query_token).await?;
            let (parent_id, _) = db::active_auth_identity(&state.db, &token_digest(token))
                .await?
                .ok_or(ApiError::Unauthorized)?;
            let CurrentUser(parent_user) =
                authenticate_parts(parts, state, (token.clone(), *from_cookie)).await?;
            if parent_id != media_parent_id || parent_user.id != media_user.id {
                return Err(ApiError::Unauthorized);
            }
            return Ok(Self(media_user));
        }
        if let Some((token, from_cookie)) = header_or_cookie_token {
            let CurrentUser(user) = authenticate_parts(parts, state, (token, from_cookie)).await?;
            return Ok(Self(user));
        }
        let token = api_key.ok_or(ApiError::Unauthorized)?;
        if db::active_auth_identity(&state.db, &token_digest(&token))
            .await?
            .is_some()
        {
            let CurrentUser(user) = authenticate_parts(parts, state, (token, false)).await?;
            return Ok(Self(user));
        }
        let (_, user) = active_scoped_media_identity(parts, state, &token).await?;
        Ok(Self(user))
    }
}

async fn active_scoped_media_identity(
    parts: &axum::http::request::Parts,
    state: &AppState,
    token: &str,
) -> Result<(Uuid, UserRecord), ApiError> {
    if !scoped_media_route(parts) {
        return Err(ApiError::Unauthorized);
    }
    let (parent_token_id, user) = db::active_media_access_identity(&state.db, &token_digest(token))
        .await?
        .ok_or(ApiError::Unauthorized)?;
    if !user.allow_media_playback {
        return Err(ApiError::Forbidden);
    }
    if !user.enable_remote_access && !is_local_client(parts, &state.config) {
        return Err(ApiError::Forbidden);
    }
    Ok((parent_token_id, user))
}

fn scoped_media_route(parts: &axum::http::request::Parts) -> bool {
    if !matches!(parts.method, Method::GET | Method::HEAD) {
        return false;
    }
    let segments = parts
        .uri
        .path()
        .split('/')
        .filter(|segment| !segment.is_empty())
        .collect::<Vec<_>>();
    let valid_id = |value: &&str| Uuid::parse_str(value).is_ok();
    match segments.as_slice() {
        ["Items", item_id, "File"] => valid_id(item_id),
        ["Videos", item_id, "stream"] | ["Audio", item_id, "stream"] => valid_id(item_id),
        ["Videos", item_id, "master.m3u8"] | ["Audio", item_id, "master.m3u8"] => valid_id(item_id),
        ["LiveTv", "Channels", item_id, "master.m3u8"] => valid_id(item_id),
        [
            "LiveTv",
            "Channels",
            item_id,
            "hls",
            session_id,
            "playlist.m3u8",
        ]
        | ["LiveTv", "Channels", item_id, "hls", session_id, _] => {
            valid_id(item_id) && Uuid::parse_str(session_id).is_ok()
        }
        ["Videos", item_id, "hls", session_id, _] | ["Audio", item_id, "hls", session_id, _] => {
            valid_id(item_id) && Uuid::parse_str(session_id).is_ok()
        }
        [
            "Videos",
            item_id,
            source_id,
            "Subtitles",
            route_index,
            extension,
        ] if matches!(*extension, "Stream.vtt" | "Stream.srt") => {
            valid_id(item_id) && valid_id(source_id) && route_index.parse::<u32>().is_ok()
        }
        [
            "Videos",
            item_id,
            source_id,
            "Subtitles",
            route_index,
            start_ticks,
            extension,
        ] if matches!(*extension, "Stream.vtt" | "Stream.srt") => {
            valid_id(item_id)
                && valid_id(source_id)
                && route_index.parse::<u32>().is_ok()
                && start_ticks.parse::<u64>().is_ok()
        }
        _ => false,
    }
}

fn extract_media_query_token(
    method: &Method,
    query: Option<&str>,
) -> Result<Option<String>, ApiError> {
    let query_allowed = *method == Method::GET || *method == Method::HEAD;
    let mut api_key: Option<String> = None;
    if let Some(raw) = query {
        for (name, value) in url::form_urlencoded::parse(raw.as_bytes()) {
            if name == "ApiKey" {
                if !query_allowed || api_key.is_some() {
                    return Err(ApiError::Unauthorized);
                }
                api_key = Some(value.into_owned());
            }
        }
    }
    Ok(api_key)
}

async fn authenticate_parts(
    parts: &axum::http::request::Parts,
    state: &AppState,
    (token, from_cookie): (String, bool),
) -> Result<CurrentUser, ApiError> {
    if token.is_empty() || token.len() > 256 || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(ApiError::Unauthorized);
    }
    if from_cookie
        && method_needs_csrf_protection(&parts.method)
        && !origin_is_same_site_origin(parts, &state.config)
    {
        return Err(ApiError::Forbidden);
    }
    let token_hash = token_digest(&token);
    let (token_id, user) = db::active_auth_identity(&state.db, &token_hash)
        .await?
        .ok_or(ApiError::Unauthorized)?;
    if !user.enable_remote_access && !is_local_client(parts, &state.config) {
        return Err(ApiError::Forbidden);
    }
    if let Err(error) = db::touch_auth_token(&state.db, state.run_id, token_id).await {
        if matches!(&error, sqlx::Error::Protocol(message) if message.contains(db::STALE_SERVER_RUN_ERROR))
        {
            return Err(ApiError::Unavailable);
        }
        return Err(error.into());
    }
    Ok(CurrentUser(user))
}

impl FromRequestParts<AppState> for AdminUser {
    type Rejection = ApiError;

    async fn from_request_parts(
        parts: &mut axum::http::request::Parts,
        state: &AppState,
    ) -> Result<Self, Self::Rejection> {
        let current = CurrentUser::from_request_parts(parts, state).await?;
        if !current.0.is_admin {
            return Err(ApiError::Forbidden);
        }
        Ok(Self(current.0))
    }
}

pub async fn hash_password(state: &AppState, password: String) -> Result<String, ApiError> {
    validate_password(&password)?;
    let permit = state
        .password_hash_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::RateLimited)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        hash_password_sync(&password)
    })
    .await
    .map_err(|_| ApiError::Internal("password hashing worker failed".to_owned()))?
    .map_err(|_| ApiError::Internal("password hashing failed".to_owned()))
}

pub async fn verify_password(
    state: &AppState,
    password: String,
    encoded_hash: String,
) -> Result<bool, ApiError> {
    if password.len() > 1024 {
        return Err(ApiError::BadRequest(
            "Password exceeds the 1024-byte limit".to_owned(),
        ));
    }
    let permit = state
        .password_hash_slots
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::RateLimited)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let parsed = PasswordHash::new(&encoded_hash).map_err(|_| ())?;
        Ok::<bool, ()>(
            Argon2::default()
                .verify_password(password.as_bytes(), &parsed)
                .is_ok(),
        )
    })
    .await
    .map_err(|_| ApiError::Internal("password verification worker failed".to_owned()))?
    .map_err(|_| ApiError::Unauthorized)
}

fn hash_password_sync(password: &str) -> Result<String, argon2::password_hash::Error> {
    Argon2::default()
        .hash_password(password.as_bytes())
        .map(|hash| hash.to_string())
}

pub fn dummy_password_hash() -> &'static str {
    static HASH: OnceLock<String> = OnceLock::new();
    HASH.get_or_init(|| {
        hash_password_sync("puffinbox-invalid-account-password")
            .expect("fixed dummy password must hash")
    })
}

pub fn validate_password(password: &str) -> Result<(), ApiError> {
    if password.len() < 12 {
        return Err(ApiError::BadRequest(
            "Password must contain at least 12 characters".to_owned(),
        ));
    }
    if password.len() > 1024 {
        return Err(ApiError::BadRequest(
            "Password exceeds the 1024-byte limit".to_owned(),
        ));
    }
    Ok(())
}

pub fn token_digest(token: &str) -> String {
    let digest = Sha256::digest(token.as_bytes());
    digest.iter().map(|byte| format!("{byte:02x}")).collect()
}

#[derive(Clone, Debug)]
pub struct IssuedToken {
    pub token: String,
    pub token_id: Uuid,
    pub expires_at: chrono::DateTime<Utc>,
}

#[derive(Clone, Debug)]
pub struct IssuedMediaToken {
    pub token: String,
    pub expires_at: chrono::DateTime<Utc>,
}

pub async fn issue_token(
    state: &AppState,
    user: &UserRecord,
    client: &str,
    device_name: &str,
    device_id: &str,
) -> Result<IssuedToken, ApiError> {
    let mut bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let token = URL_SAFE_NO_PAD.encode(bytes);
    let expires_at = Utc::now() + Duration::hours(state.config.access_token_lifetime_hours);
    let token_id = Uuid::new_v4();
    db::create_auth_token(
        &state.db,
        state.run_id,
        db::NewAuthToken {
            token_id,
            user_id: user.id,
            token_hash: token_digest(&token),
            expires_at,
            client: sanitize_header(client),
            device_name: sanitize_header(device_name),
            device_id: sanitize_identity(device_id, MAX_DEVICE_ID_BYTES),
        },
    )
    .await?;
    Ok(IssuedToken {
        token,
        token_id,
        expires_at,
    })
}

pub async fn issue_media_access_token(
    state: &AppState,
    parent_token_id: Uuid,
) -> Result<IssuedMediaToken, ApiError> {
    let mut bytes = [0_u8; 32];
    OsRng.fill_bytes(&mut bytes);
    let token = URL_SAFE_NO_PAD.encode(bytes);
    let requested_expiry = Utc::now() + Duration::hours(4);
    let expires_at = db::create_media_access_token(
        &state.db,
        state.run_id,
        parent_token_id,
        &token_digest(&token),
        requested_expiry,
    )
    .await?
    .ok_or(ApiError::Unauthorized)?;
    Ok(IssuedMediaToken { token, expires_at })
}

fn sanitize_header(value: &str) -> String {
    sanitize_identity(value, 128)
}

pub const MAX_DEVICE_ID_BYTES: usize = 1024;

fn sanitize_identity(value: &str, max_bytes: usize) -> String {
    let mut cleaned = String::new();
    for character in value.chars().filter(|c| !c.is_control()) {
        if cleaned.len() + character.len_utf8() > max_bytes {
            break;
        }
        cleaned.push(character);
    }
    if cleaned.trim().is_empty() {
        "unknown".to_owned()
    } else {
        cleaned
    }
}

fn add_auth_candidate(
    candidate: &mut Option<(String, bool)>,
    token: String,
    from_cookie: bool,
) -> Result<(), ApiError> {
    let token = token.trim().to_owned();
    if token.is_empty() || token.len() > 256 || !token.bytes().all(|byte| byte.is_ascii_graphic()) {
        return Err(ApiError::Unauthorized);
    }
    match candidate {
        Some((current, _)) if current != &token => Err(ApiError::Unauthorized),
        Some((_, cookie)) => {
            if !from_cookie {
                *cookie = false;
            }
            Ok(())
        }
        None => {
            *candidate = Some((token, from_cookie));
            Ok(())
        }
    }
}

fn extract_token(headers: &HeaderMap) -> Result<Option<(String, bool)>, ApiError> {
    let mut candidate = None;
    for name in ["x-emby-token", "x-mediabrowser-token"] {
        for value in headers.get_all(name).iter() {
            let token = value.to_str().map_err(|_| ApiError::Unauthorized)?;
            add_auth_candidate(&mut candidate, token.to_owned(), false)?;
        }
    }
    for name in [header::AUTHORIZATION.as_str(), "x-emby-authorization"] {
        for value in headers.get_all(name).iter() {
            let value = value.to_str().map_err(|_| ApiError::Unauthorized)?;
            if let Some(token) = parse_media_browser_token(value) {
                add_auth_candidate(&mut candidate, token, false)?;
            } else if !(is_media_browser_header(value)
                && parse_media_browser_parameters(value).is_some())
            {
                return Err(ApiError::Unauthorized);
            }
        }
    }
    let mut cookie_count = 0;
    for value in headers.get_all(header::COOKIE).iter() {
        let cookie = value.to_str().map_err(|_| ApiError::Unauthorized)?;
        for part in cookie.split(';') {
            let Some((name, value)) = part.trim().split_once('=') else {
                continue;
            };
            if name == "puffinbox_session" {
                cookie_count += 1;
                if cookie_count > 1 {
                    return Err(ApiError::Unauthorized);
                }
                add_auth_candidate(&mut candidate, value.to_owned(), true)?;
            }
        }
    }
    Ok(candidate)
}

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct ClientIdentity {
    pub client: Option<String>,
    pub device_name: Option<String>,
    pub device_id: Option<String>,
    pub version: Option<String>,
}

pub fn client_identity_from_headers(headers: &HeaderMap) -> Result<ClientIdentity, ApiError> {
    let mut identity = ClientIdentity::default();
    for name in [header::AUTHORIZATION.as_str(), "x-emby-authorization"] {
        for value in headers.get_all(name).iter() {
            let value = value.to_str().map_err(|_| {
                ApiError::BadRequest("Client authorization header is invalid".to_owned())
            })?;
            if !is_media_browser_header(value) {
                continue;
            }
            let parameters = parse_media_browser_parameters(value).ok_or_else(|| {
                ApiError::BadRequest("Client authorization header is invalid".to_owned())
            })?;
            for (name, value) in parameters {
                let target = if name.eq_ignore_ascii_case("Client") {
                    Some(&mut identity.client)
                } else if name.eq_ignore_ascii_case("Device")
                    || name.eq_ignore_ascii_case("DeviceName")
                {
                    Some(&mut identity.device_name)
                } else if name.eq_ignore_ascii_case("DeviceId") {
                    Some(&mut identity.device_id)
                } else if name.eq_ignore_ascii_case("Version") {
                    Some(&mut identity.version)
                } else {
                    None
                };
                if let Some(target) = target {
                    let max_bytes = if name.eq_ignore_ascii_case("DeviceId") {
                        MAX_DEVICE_ID_BYTES
                    } else {
                        128
                    };
                    if value.is_empty()
                        || value.len() > max_bytes
                        || !value
                            .bytes()
                            .all(|byte| byte.is_ascii_graphic() || byte == b' ')
                    {
                        return Err(ApiError::BadRequest(
                            "Client authorization metadata exceeds supported limits".to_owned(),
                        ));
                    }
                    if target.as_ref().is_some_and(|previous| previous != &value) {
                        return Err(ApiError::BadRequest(
                            "Client authorization headers contain conflicting identity values"
                                .to_owned(),
                        ));
                    }
                    *target = Some(value);
                }
            }
        }
    }
    Ok(identity)
}

fn parse_media_browser_token(value: &str) -> Option<String> {
    let trimmed = value.trim();
    if trimmed
        .get(..7)
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("Bearer "))
    {
        return Some(trimmed[7..].trim().to_owned());
    }
    parse_media_browser_parameters(trimmed)?
        .into_iter()
        .find(|(name, _)| name.eq_ignore_ascii_case("Token"))
        .map(|(_, value)| value)
}

fn is_media_browser_header(value: &str) -> bool {
    value
        .split_whitespace()
        .next()
        .is_some_and(|scheme| scheme.eq_ignore_ascii_case("MediaBrowser"))
}

fn parse_media_browser_parameters(value: &str) -> Option<Vec<(String, String)>> {
    let value = value.trim();
    let (scheme, mut rest) = value.split_once(char::is_whitespace)?;
    if !scheme.eq_ignore_ascii_case("MediaBrowser") {
        return None;
    }
    let mut parameters = Vec::new();
    loop {
        rest = rest.trim_start();
        if rest.is_empty() {
            return Some(parameters);
        }
        let after_comma = rest.strip_prefix(',');
        if let Some(next) = after_comma {
            rest = next;
            continue;
        }
        let equals = rest.find('=')?;
        let name = rest[..equals].trim();
        if name.is_empty()
            || !name
                .bytes()
                .all(|byte| byte.is_ascii_alphanumeric() || byte == b'-')
        {
            return None;
        }
        rest = rest[equals + 1..].trim_start();
        let parsed;
        if let Some(quoted) = rest.strip_prefix('"') {
            let mut output = String::new();
            let mut chars = quoted.chars();
            let mut terminated = false;
            while let Some(character) = chars.next() {
                match character {
                    '"' => {
                        terminated = true;
                        break;
                    }
                    '\\' => output.push(chars.next()?),
                    character if !character.is_control() => output.push(character),
                    _ => return None,
                }
            }
            if !terminated {
                return None;
            }
            let consumed = quoted.len() - chars.as_str().len();
            parsed = output;
            rest = quoted.get(consumed..)?;
            let after_value = rest.trim_start();
            if !after_value.is_empty() && !after_value.starts_with(',') {
                return None;
            }
            rest = after_value;
        } else {
            let (parameter, tail) = rest.split_once(',').unwrap_or((rest, ""));
            parsed = parameter.trim().to_owned();
            if parsed.is_empty() {
                return None;
            }
            rest = if tail.is_empty() {
                ""
            } else {
                &rest[rest.len() - tail.len()..]
            };
        }
        if parameters
            .iter()
            .any(|(existing, _): &(String, String)| existing.eq_ignore_ascii_case(name))
        {
            return None;
        }
        parameters.push((name.to_owned(), parsed));
    }
}

pub fn cookie_header(token: &str, config: &Config) -> Result<HeaderValue, ApiError> {
    let max_age = config.access_token_lifetime_hours.saturating_mul(3600);
    let secure = if config.cookie_secure { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "puffinbox_session={token}; Path=/; Max-Age={max_age}; HttpOnly; SameSite=Strict{secure}"
    ))
    .map_err(|_| ApiError::Internal("could not construct session cookie".to_owned()))
}

pub fn expired_cookie_header(config: &Config) -> Result<HeaderValue, ApiError> {
    let secure = if config.cookie_secure { "; Secure" } else { "" };
    HeaderValue::from_str(&format!(
        "puffinbox_session=; Path=/; Max-Age=0; HttpOnly; SameSite=Strict{secure}"
    ))
    .map_err(|_| ApiError::Internal("could not construct session cookie".to_owned()))
}

pub fn client_address(parts: &axum::http::request::Parts, trusted: &[IpNet]) -> Option<IpAddr> {
    let peer = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip())?;
    if !trusted.iter().any(|net| net.contains(&peer)) {
        return Some(peer);
    }
    let mut forwarded_values = parts.headers.get_all("x-forwarded-for").iter();
    let forwarded = forwarded_values.next()?;
    if forwarded_values.next().is_some() {
        return None;
    }
    let forwarded = forwarded.to_str().ok()?;
    let mut hops = Vec::new();
    for raw in forwarded.split(',') {
        if raw.trim().is_empty() {
            return None;
        }
        let Ok(address) = raw.trim().parse::<IpAddr>() else {
            return None;
        };
        hops.push(address);
    }
    hops.into_iter()
        .rev()
        .find(|address| !trusted.iter().any(|net| net.contains(address)))
}

fn is_local_client(parts: &axum::http::request::Parts, config: &Config) -> bool {
    client_address(parts, &config.trusted_proxies).is_some_and(|address| {
        config
            .local_networks
            .iter()
            .any(|net| net.contains(&address))
    })
}

pub fn client_is_local(parts: &axum::http::request::Parts, config: &Config) -> bool {
    is_local_client(parts, config)
}

fn method_needs_csrf_protection(method: &Method) -> bool {
    !matches!(*method, Method::GET | Method::HEAD | Method::OPTIONS)
}

pub fn origin_is_same_site_origin(parts: &axum::http::request::Parts, config: &Config) -> bool {
    let mut origin_values = parts.headers.get_all(header::ORIGIN).iter();
    let Some(origin_header) = origin_values.next() else {
        return origin_is_exact_same_origin(parts, config);
    };
    if origin_values.next().is_some() {
        return false;
    }
    let Ok(raw_origin) = origin_header.to_str() else {
        return false;
    };
    let fetch_site = match fetch_site_value(parts) {
        Ok(fetch_site) => fetch_site,
        Err(()) => return false,
    };
    let origin_matches_request = origin_header_matches_request(parts, config, raw_origin);
    if origin_matches_request
        && fetch_site.is_some_and(|value| value.eq_ignore_ascii_case("cross-site"))
    {
        return false;
    }
    if config
        .cors_origins
        .iter()
        .any(|allowed| allowed == raw_origin)
    {
        return true;
    }
    origin_matches_request
}

pub fn origin_is_exact_same_origin(parts: &axum::http::request::Parts, config: &Config) -> bool {
    let mut origin_values = parts.headers.get_all(header::ORIGIN).iter();
    let Some(origin_header) = origin_values.next() else {
        return fetch_metadata_confirms_same_origin(parts);
    };
    if origin_values.next().is_some() {
        return false;
    }
    let Ok(raw_origin) = origin_header.to_str() else {
        return false;
    };
    let fetch_site = match fetch_site_value(parts) {
        Ok(fetch_site) => fetch_site,
        Err(()) => return false,
    };
    let origin_matches_request = origin_header_matches_request(parts, config, raw_origin);
    if origin_matches_request
        && fetch_site.is_some_and(|value| value.eq_ignore_ascii_case("cross-site"))
    {
        return false;
    }
    origin_matches_request
}

fn origin_header_matches_request(
    parts: &axum::http::request::Parts,
    config: &Config,
    raw_origin: &str,
) -> bool {
    let Ok(origin) = url::Url::parse(raw_origin) else {
        return false;
    };
    if origin.username() != ""
        || origin.password().is_some()
        || origin.path() != "/"
        || origin.query().is_some()
        || origin.fragment().is_some()
    {
        return false;
    }
    let Some(host) = request_host(parts) else {
        return false;
    };
    let scheme = request_scheme(parts, config).unwrap_or_else(|| "http".to_owned());
    let Ok(expected) = url::Url::parse(&format!("{scheme}://{host}/")) else {
        return false;
    };
    origin.origin() == expected.origin()
}

pub(crate) fn fetch_metadata_confirms_same_origin(parts: &axum::http::request::Parts) -> bool {
    if request_host(parts).is_none() {
        return false;
    }
    let Ok(Some(fetch_site)) = fetch_site_value(parts) else {
        return false;
    };
    fetch_site.eq_ignore_ascii_case("same-origin")
}

fn request_host(parts: &axum::http::request::Parts) -> Option<&str> {
    let mut host_values = parts.headers.get_all(header::HOST).iter();
    let host = host_values.next()?;
    if host_values.next().is_some() {
        return None;
    }
    host.to_str().ok()
}

fn fetch_site_value(parts: &axum::http::request::Parts) -> Result<Option<&str>, ()> {
    let mut fetch_site_values = parts.headers.get_all("sec-fetch-site").iter();
    let Some(fetch_site) = fetch_site_values.next() else {
        return Ok(None);
    };
    if fetch_site_values.next().is_some() {
        return Err(());
    }
    fetch_site.to_str().map(Some).map_err(|_| ())
}

pub fn validate_username(username: &str) -> Result<(), ApiError> {
    let name = username.trim();
    if name.is_empty()
        || name.len() > 64
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
    {
        return Err(ApiError::BadRequest(
            "Username must be 1–64 ASCII letters, numbers, dots, underscores, or hyphens"
                .to_owned(),
        ));
    }
    Ok(())
}

fn request_scheme(parts: &axum::http::request::Parts, config: &Config) -> Option<String> {
    let peer = parts
        .extensions
        .get::<ConnectInfo<SocketAddr>>()
        .map(|ci| ci.0.ip());
    let proxy_is_trusted =
        peer.is_some_and(|ip| config.trusted_proxies.iter().any(|net| net.contains(&ip)));
    let mut forwarded_proto_values = parts.headers.get_all("x-forwarded-proto").iter();
    let forwarded_proto = forwarded_proto_values.next();
    let forwarded_proto_is_unique = forwarded_proto_values.next().is_none();
    if proxy_is_trusted
        && forwarded_proto_is_unique
        && let Some(proto) = forwarded_proto.and_then(|value| value.to_str().ok())
    {
        let candidate = proto.trim().to_ascii_lowercase();
        if candidate == "http" || candidate == "https" {
            return Some(candidate);
        }
    }
    parts
        .uri
        .scheme_str()
        .map(str::to_ascii_lowercase)
        .or_else(|| Some("http".to_owned()))
}

pub fn extract_raw_token(headers: &HeaderMap) -> Result<Option<(String, bool)>, ApiError> {
    extract_token(headers)
}

pub fn cookie_only_session_token(
    headers: &HeaderMap,
    raw_query: Option<&str>,
) -> Result<String, ApiError> {
    if raw_query.is_some()
        || [
            header::AUTHORIZATION.as_str(),
            "x-emby-authorization",
            "x-emby-token",
            "x-mediabrowser-token",
        ]
        .iter()
        .any(|name| headers.contains_key(*name))
    {
        return Err(ApiError::Unauthorized);
    }
    match extract_token(headers)? {
        Some((token, true)) => Ok(token),
        _ => Err(ApiError::Unauthorized),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::{net::SocketAddr, path::PathBuf};

    fn test_parts(peer: &str, uri: &str) -> axum::http::request::Parts {
        let mut parts = axum::http::Request::builder()
            .uri(uri)
            .body(())
            .unwrap()
            .into_parts()
            .0;
        let peer = peer.parse::<SocketAddr>().unwrap();
        parts.extensions.insert(ConnectInfo(peer));
        parts
    }

    fn test_config(trusted_proxies: Vec<IpNet>) -> Config {
        Config {
            bind: "0.0.0.0:8096".parse().unwrap(),
            public_base_url: None,
            database_url: "postgres://127.0.0.1/test".to_owned(),
            server_name: "Auth test".to_owned(),
            web_root: PathBuf::from("web"),
            data_dir: PathBuf::from("data"),
            ffmpeg_path: None,
            max_scan_workers: 1,
            max_page_size: 100,
            access_token_lifetime_hours: 24,
            cookie_secure: false,
            cors_origins: Vec::new(),
            trusted_proxies,
            local_networks: Vec::new(),
            setup_token: None,
            bootstrap_admin_username: None,
            bootstrap_admin_password: None,
        }
    }

    #[test]
    fn parses_native_media_browser_login_identity_and_token() {
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, HeaderValue::from_static(
            "MediaBrowser Client=\"Puffin Test Player\", Device=\"Living Room\", DeviceId=\"device-001\", Version=\"12.0.0\", Token=\"opaque-token-1234567890\"",
        ));
        assert_eq!(
            client_identity_from_headers(&headers).unwrap(),
            ClientIdentity {
                client: Some("Puffin Test Player".to_owned()),
                device_name: Some("Living Room".to_owned()),
                device_id: Some("device-001".to_owned()),
                version: Some("12.0.0".to_owned()),
            }
        );
        assert_eq!(
            extract_raw_token(&headers).unwrap(),
            Some(("opaque-token-1234567890".to_owned(), false))
        );
    }

    #[test]
    fn long_device_ids_remain_distinct_and_other_metadata_stays_bounded() {
        let prefix = "opaque-browser-id-".repeat(24);
        let first = format!("{prefix}first");
        let second = format!("{prefix}second");
        let mut headers = HeaderMap::new();
        headers.insert(header::AUTHORIZATION, HeaderValue::from_str(&format!(
            "MediaBrowser Client=\"Jellyfin Web\", Device=\"Chrome\", DeviceId=\"{first}\", Version=\"12.0.0\""
        )).unwrap());
        let parsed = client_identity_from_headers(&headers).unwrap();
        assert_eq!(parsed.device_id.as_deref(), Some(first.as_str()));
        assert_eq!(sanitize_identity(&first, MAX_DEVICE_ID_BYTES), first);
        assert_ne!(
            sanitize_identity(&first, MAX_DEVICE_ID_BYTES),
            sanitize_identity(&second, MAX_DEVICE_ID_BYTES)
        );
        for (field, value) in [
            ("DeviceId", "x".repeat(MAX_DEVICE_ID_BYTES + 1)),
            ("Client", "x".repeat(129)),
            ("Device", "x".repeat(129)),
        ] {
            headers.insert(
                header::AUTHORIZATION,
                HeaderValue::from_str(&format!("MediaBrowser {field}=\"{value}\"")).unwrap(),
            );
            assert!(client_identity_from_headers(&headers).is_err(), "{field}");
        }
    }

    #[test]
    fn rejects_conflicting_native_client_identity_headers() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("MediaBrowser Client=\"Player A\""),
        );
        headers.insert(
            "x-emby-authorization",
            HeaderValue::from_static("MediaBrowser Client=\"Player B\""),
        );
        assert!(client_identity_from_headers(&headers).is_err());
    }

    #[test]
    fn metadata_only_authorization_does_not_override_explicit_token_header() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("MediaBrowser Client=\"Player\", DeviceId=\"device-001\""),
        );
        headers.insert(
            "x-emby-token",
            HeaderValue::from_static("opaque-token-1234567890"),
        );
        assert_eq!(
            extract_raw_token(&headers).unwrap(),
            Some(("opaque-token-1234567890".to_owned(), false))
        );
    }

    #[test]
    fn explicit_token_remains_header_auth_when_same_cookie_is_also_present() {
        let mut headers = HeaderMap::new();
        headers.insert(
            "x-emby-token",
            HeaderValue::from_static("opaque-token-1234567890"),
        );
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("puffinbox_session=opaque-token-1234567890"),
        );
        assert_eq!(
            extract_raw_token(&headers).unwrap(),
            Some(("opaque-token-1234567890".to_owned(), false))
        );
    }

    #[test]
    fn query_media_auth_is_limited_to_read_only_methods() {
        assert_eq!(
            extract_media_query_token(&Method::GET, Some("PlaySessionId=one&ApiKey=media-token"))
                .unwrap()
                .as_deref(),
            Some("media-token")
        );
        assert_eq!(
            extract_media_query_token(&Method::HEAD, Some("ApiKey=media-token"))
                .unwrap()
                .as_deref(),
            Some("media-token")
        );
        for method in [Method::POST, Method::PUT, Method::PATCH, Method::DELETE] {
            assert!(
                extract_media_query_token(&method, Some("ApiKey=media-token")).is_err(),
                "{method} must reject media query credentials"
            );
        }
        assert!(extract_media_query_token(&Method::GET, Some("ApiKey=one&ApiKey=two")).is_err());
    }

    #[test]
    fn socket_query_tokens_are_bounded_unique_and_consistent_with_headers() {
        let mut headers = HeaderMap::new();
        assert_eq!(
            socket_token(&headers, Some("deviceId=test&api_key=session-token")).unwrap(),
            ("session-token".to_owned(), false)
        );
        assert_eq!(
            socket_token(&headers, Some("ApiKey=session-token")).unwrap(),
            ("session-token".to_owned(), false)
        );
        for query in [
            "api_key=one&ApiKey=one",
            "api_key=one&api_key=two",
            "api_key=",
            "api_key=bad%0Atoken",
        ] {
            assert!(socket_token(&headers, Some(query)).is_err());
        }
        assert!(socket_token(&headers, Some(&"x".repeat(2049))).is_err());
        headers.insert("x-emby-token", HeaderValue::from_static("session-token"));
        assert!(socket_token(&headers, Some("api_key=another-token")).is_err());
        assert_eq!(
            socket_token(&headers, Some("api_key=session-token")).unwrap(),
            ("session-token".to_owned(), false)
        );
        headers.remove("x-emby-token");
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("puffinbox_session=session-token"),
        );
        assert_eq!(
            socket_token(&headers, None).unwrap(),
            ("session-token".to_owned(), true)
        );
        assert!(socket_token(&headers, Some("api_key=another-token")).is_err());
    }

    #[test]
    fn media_access_token_exchange_requires_cookie_only_authentication() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::COOKIE,
            HeaderValue::from_static("puffinbox_session=cookie-session"),
        );
        assert_eq!(
            cookie_only_session_token(&headers, None).unwrap(),
            "cookie-session"
        );
        assert!(cookie_only_session_token(&headers, Some("ApiKey=query-token")).is_err());
        assert!(cookie_only_session_token(&headers, Some("unrelated=value")).is_err());

        headers.insert("x-emby-token", HeaderValue::from_static("cookie-session"));
        assert!(cookie_only_session_token(&headers, None).is_err());
        headers.remove("x-emby-token");
        headers.insert(
            header::AUTHORIZATION,
            HeaderValue::from_static("Bearer different-session"),
        );
        assert!(cookie_only_session_token(&headers, None).is_err());
    }

    #[test]
    fn token_exchange_origin_guard_does_not_trust_configured_cross_origin_clients() {
        let config = test_config(Vec::new());
        let mut same_origin = test_parts("127.0.0.1:8000", "/Users/Me/MediaAccessToken");
        same_origin
            .headers
            .insert(header::HOST, HeaderValue::from_static("media.example"));
        same_origin.headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://media.example"),
        );
        assert!(origin_is_exact_same_origin(&same_origin, &config));

        let mut duplicate_host = same_origin.clone();
        duplicate_host
            .headers
            .append(header::HOST, HeaderValue::from_static("other.example"));
        assert!(!origin_is_exact_same_origin(&duplicate_host, &config));

        duplicate_host.headers.remove(header::ORIGIN);
        duplicate_host
            .headers
            .insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
        assert!(!origin_is_exact_same_origin(&duplicate_host, &config));

        let cors_config = Config {
            cors_origins: vec!["https://desktop.example".to_owned()],
            ..config.clone()
        };
        let mut cross_origin = same_origin.clone();
        cross_origin.headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://desktop.example"),
        );
        assert!(origin_is_same_site_origin(&cross_origin, &cors_config));
        assert!(!origin_is_exact_same_origin(&cross_origin, &cors_config));

        let mut duplicate_cors_origin = cross_origin.clone();
        duplicate_cors_origin.headers.append(
            header::ORIGIN,
            HeaderValue::from_static("https://desktop.example"),
        );
        assert!(!origin_is_same_site_origin(
            &duplicate_cors_origin,
            &cors_config
        ));

        let mut contradictory_fetch_site = same_origin;
        contradictory_fetch_site
            .headers
            .insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
        assert!(!origin_is_same_site_origin(
            &contradictory_fetch_site,
            &config
        ));
        assert!(!origin_is_exact_same_origin(
            &contradictory_fetch_site,
            &config
        ));
    }

    #[test]
    fn missing_origin_requires_same_origin_fetch_metadata() {
        let config = test_config(Vec::new());
        let mut same_origin = test_parts("127.0.0.1:8000", "/Users/Me/MediaAccessToken");
        same_origin
            .headers
            .insert(header::HOST, HeaderValue::from_static("media.example"));
        same_origin
            .headers
            .insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
        assert!(origin_is_exact_same_origin(&same_origin, &config));
        assert!(origin_is_same_site_origin(&same_origin, &config));

        let mut cross_site = same_origin.clone();
        cross_site
            .headers
            .insert("sec-fetch-site", HeaderValue::from_static("cross-site"));
        assert!(!origin_is_exact_same_origin(&cross_site, &config));
        assert!(!origin_is_same_site_origin(&cross_site, &config));

        let mut same_site = same_origin.clone();
        same_site
            .headers
            .insert("sec-fetch-site", HeaderValue::from_static("same-site"));
        assert!(!origin_is_exact_same_origin(&same_site, &config));
        assert!(!origin_is_same_site_origin(&same_site, &config));

        let mut duplicated_fetch_site = same_origin.clone();
        duplicated_fetch_site
            .headers
            .append("sec-fetch-site", HeaderValue::from_static("same-origin"));
        assert!(!origin_is_exact_same_origin(
            &duplicated_fetch_site,
            &config
        ));

        same_origin.headers.remove("sec-fetch-site");
        assert!(!origin_is_exact_same_origin(&same_origin, &config));
        assert!(!origin_is_same_site_origin(&same_origin, &config));
        same_origin
            .headers
            .insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
        same_origin
            .headers
            .insert(header::ORIGIN, HeaderValue::from_static("null"));
        assert!(!origin_is_exact_same_origin(&same_origin, &config));

        same_origin.headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://other.example"),
        );
        same_origin
            .headers
            .insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
        assert!(!origin_is_exact_same_origin(&same_origin, &config));

        let mut duplicated_origin = test_parts("127.0.0.1:8000", "/Users/Me/MediaAccessToken");
        duplicated_origin
            .headers
            .insert(header::HOST, HeaderValue::from_static("media.example"));
        duplicated_origin.headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("http://media.example"),
        );
        duplicated_origin.headers.append(
            header::ORIGIN,
            HeaderValue::from_static("http://media.example"),
        );
        duplicated_origin
            .headers
            .insert("sec-fetch-site", HeaderValue::from_static("same-origin"));
        assert!(!origin_is_exact_same_origin(&duplicated_origin, &config));
    }

    #[test]
    fn scoped_media_credentials_are_limited_to_registered_read_only_media_routes() {
        let item_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let mut file = test_parts(
            "127.0.0.1:8000",
            &format!("/Items/{item_id}/File?ApiKey=scoped"),
        );
        assert!(scoped_media_route(&file));
        file.uri = format!("/Videos/{item_id}/hls/{session_id}/playlist.m3u8")
            .parse()
            .unwrap();
        assert!(scoped_media_route(&file));
        file.uri = format!("/LiveTv/Channels/{item_id}/master.m3u8")
            .parse()
            .unwrap();
        assert!(scoped_media_route(&file));
        file.uri = format!("/LiveTv/Channels/{item_id}/hls/{session_id}/playlist.m3u8")
            .parse()
            .unwrap();
        assert!(scoped_media_route(&file));
        file.uri = format!("/LiveTv/Channels/{item_id}/hls/{session_id}/segment000001.ts")
            .parse()
            .unwrap();
        assert!(scoped_media_route(&file));
        file.uri = format!("/Videos/{item_id}/{item_id}/Subtitles/3/Stream.vtt")
            .parse()
            .unwrap();
        assert!(scoped_media_route(&file));
        file.uri = format!("/Videos/{item_id}/{item_id}/Subtitles/3/9000/Stream.srt")
            .parse()
            .unwrap();
        assert!(scoped_media_route(&file));
        file.method = Method::POST;
        file.uri = format!("/LiveTv/Channels/{item_id}/hls/{session_id}/keepalive")
            .parse()
            .unwrap();
        assert!(!scoped_media_route(&file));
        file.method = Method::GET;
        file.uri = "/Users/Me?ApiKey=scoped".parse().unwrap();
        assert!(!scoped_media_route(&file));
        file.uri = format!("/Items/{item_id}/Download?ApiKey=scoped")
            .parse()
            .unwrap();
        assert!(!scoped_media_route(&file));
        file.method = Method::POST;
        file.uri = format!("/Items/{item_id}/File?ApiKey=scoped")
            .parse()
            .unwrap();
        assert!(!scoped_media_route(&file));
    }

    #[test]
    fn forwarded_client_address_trusts_only_valid_single_header_from_trusted_peer() {
        let trusted = [
            "127.0.0.0/8".parse::<IpNet>().unwrap(),
            "10.0.0.0/8".parse().unwrap(),
        ];
        let mut valid = test_parts("127.0.0.1:8000", "/");
        valid.headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.8, 10.1.2.3"),
        );
        assert_eq!(
            client_address(&valid, &trusted),
            Some("203.0.113.8".parse().unwrap())
        );

        let mut spoofed_untrusted = test_parts("203.0.113.9:8000", "/");
        spoofed_untrusted
            .headers
            .insert("x-forwarded-for", HeaderValue::from_static("10.2.3.4"));
        assert_eq!(
            client_address(&spoofed_untrusted, &trusted),
            Some("203.0.113.9".parse().unwrap())
        );

        let mut malformed = test_parts("127.0.0.1:8000", "/");
        malformed.headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("203.0.113.8, not-an-address"),
        );
        assert_eq!(client_address(&malformed, &trusted), None);

        let mut duplicated = test_parts("127.0.0.1:8000", "/");
        duplicated
            .headers
            .append("x-forwarded-for", HeaderValue::from_static("203.0.113.8"));
        duplicated
            .headers
            .append("x-forwarded-for", HeaderValue::from_static("10.1.2.3"));
        assert_eq!(client_address(&duplicated, &trusted), None);

        let mut all_trusted = test_parts("127.0.0.1:8000", "/");
        all_trusted.headers.insert(
            "x-forwarded-for",
            HeaderValue::from_static("10.1.2.3, 127.0.0.2"),
        );
        assert_eq!(client_address(&all_trusted, &trusted), None);

        let missing = test_parts("127.0.0.1:8000", "/");
        assert_eq!(client_address(&missing, &trusted), None);
    }

    #[test]
    fn forwarded_proto_is_trusted_only_from_one_configured_proxy_header() {
        let trusted_proxy = "127.0.0.0/8".parse::<IpNet>().unwrap();
        let config = test_config(vec![trusted_proxy]);
        let mut trusted = test_parts("127.0.0.1:8000", "http://internal/");
        trusted
            .headers
            .insert("x-forwarded-proto", HeaderValue::from_static("https"));
        assert_eq!(request_scheme(&trusted, &config).as_deref(), Some("https"));
        trusted
            .headers
            .append("x-forwarded-proto", HeaderValue::from_static("http"));
        assert_eq!(request_scheme(&trusted, &config).as_deref(), Some("http"));

        let mut untrusted = test_parts("203.0.113.8:8000", "http://internal/");
        untrusted
            .headers
            .insert("x-forwarded-proto", HeaderValue::from_static("https"));
        assert_eq!(request_scheme(&untrusted, &config).as_deref(), Some("http"));
    }

    #[test]
    fn cookie_csrf_origin_uses_trusted_forwarded_scheme() {
        let config = test_config(vec!["127.0.0.0/8".parse().unwrap()]);
        let mut trusted = test_parts("127.0.0.1:8000", "http://internal/");
        trusted
            .headers
            .insert(header::HOST, HeaderValue::from_static("media.example"));
        trusted.headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://media.example"),
        );
        trusted
            .headers
            .insert("x-forwarded-proto", HeaderValue::from_static("https"));
        assert!(origin_is_same_site_origin(&trusted, &config));

        let mut untrusted = test_parts("203.0.113.8:8000", "http://internal/");
        untrusted
            .headers
            .insert(header::HOST, HeaderValue::from_static("media.example"));
        untrusted.headers.insert(
            header::ORIGIN,
            HeaderValue::from_static("https://media.example"),
        );
        untrusted
            .headers
            .insert("x-forwarded-proto", HeaderValue::from_static("https"));
        assert!(!origin_is_same_site_origin(&untrusted, &config));
    }
}
