use std::{collections::HashSet, path::PathBuf};

use axum::{
    Json, Router,
    body::to_bytes,
    extract::{Path, Query, RawQuery, Request, State},
    http::{HeaderMap, HeaderName, HeaderValue, StatusCode, header},
    response::{IntoResponse, Redirect, Response},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use ctutils::CtEq;
use serde::{Deserialize, Serialize};
use sqlx::error::DatabaseError;
use tower_http::{
    cors::{AllowHeaders, AllowMethods, AllowOrigin, CorsLayer},
    limit::RequestBodyLimitLayer,
    services::ServeDir,
    trace::TraceLayer,
};
use uuid::Uuid;

const MAX_CATALOG_PAGE_SIZE: i64 = 100;

use crate::{
    auth::{self, AdminUser, CurrentUser, UserRecord},
    db::{self, NewUser, UserPatch},
    error::ApiError,
    library::{ItemQuery, ItemRecord, LibraryRecord},
    state::AppState,
};

pub fn router(state: AppState) -> Router {
    let cors = cors_layer(&state);
    let web_root = state.config.web_root.clone();
    let core = Router::new()
        .route("/", get(root_redirect))
        .route("/health", get(health))
        .route("/health/ready", get(ready))
        .route("/System/Info/Public", get(public_system_info))
        .route("/Branding/Configuration", get(branding_configuration))
        .route("/QuickConnect/Enabled", get(quick_connect_enabled))
        .route("/Users/Public", get(public_users))
        .route("/users/public", get(public_users))
        .route("/System/Info", get(system_info))
        .route("/Localization/ParentalRatings", get(parental_ratings))
        .route("/Startup/Configuration", get(startup_configuration))
        .route("/Startup/User", post(startup_create_user))
        .route("/Users/AuthenticateByName", post(authenticate_by_name))
        .route("/Users/authenticatebyname", post(authenticate_by_name))
        .route("/Users/Me", get(get_current_user))
        .route(
            "/Users/Me/MediaAccessToken",
            post(restore_media_access_token),
        )
        .route("/Users/Me/Logout", post(logout))
        .route("/Users", get(list_users).post(create_user))
        .route(
            "/Users/{user_id}",
            get(get_user).post(update_user).delete(remove_user),
        )
        .route(
            "/Users/{user_id}/Policy",
            get(get_user_policy).post(update_user_policy),
        )
        .route(
            "/Library/VirtualFolders",
            get(list_libraries)
                .post(create_library)
                .delete(remove_library),
        )
        .route("/Library/MediaFolders", get(list_libraries))
        .route("/Library/Refresh", post(refresh_library_scan))
        .route(
            "/Library/VirtualFolders/Refresh",
            post(refresh_library_scan),
        )
        .route("/Library/ScanStatus", get(library_scan_status))
        .route(
            "/Puffinbox/Libraries/RootIdentities",
            get(list_library_root_identities),
        )
        .route(
            "/Puffinbox/Libraries/Roots/Rebind",
            post(rebind_library_root),
        )
        .route("/UserViews", get(user_views))
        .route("/Items/Latest", get(latest_items))
        .route("/UserItems/Resume", get(resume_items))
        .route("/Shows/{series_id}/Seasons", get(show_seasons))
        .route("/Shows/{series_id}/Episodes", get(show_episodes))
        .route("/Persons", get(list_music_persons))
        .route("/Persons/{name}", get(get_music_person))
        .route("/Artists", get(list_music_artists))
        .route("/Artists/AlbumArtists", get(list_music_artists))
        .route("/Items", get(browse_items))
        .route("/Items/Counts", get(item_counts))
        .route("/Items/{item_id}", get(get_item))
        .route("/Users/{user_id}/Items/{item_id}", get(get_user_item))
        .route(
            "/Items/{item_id}/UserData",
            get(get_user_item_data).post(update_user_item_data),
        )
        .route(
            "/UserItems/{item_id}",
            get(get_user_item_data).post(update_user_item_data),
        )
        .route(
            "/UserItems/{item_id}/UserData",
            get(get_user_item_data).post(update_user_item_data),
        )
        .route(
            "/UserPlayedItems/{item_id}",
            post(mark_item_played).delete(mark_item_unplayed),
        )
        .route(
            "/UserFavoriteItems/{item_id}",
            post(mark_item_favorite).delete(unmark_item_favorite),
        )
        .route("/Search/Hints", get(search_hints))
        .route("/Sessions", get(list_sessions))
        .route(
            "/Sessions/Capabilities/Full",
            post(update_session_capabilities),
        )
        .route("/Sessions/Logout", post(logout))
        .route("/Sessions/Playing", post(start_playback))
        .route("/Sessions/Playing/Progress", post(progress_playback))
        .route("/Sessions/Playing/Stopped", post(stop_playback))
        .with_state(state.clone());
    let trace = TraceLayer::new_for_http().make_span_with(|request: &Request| {
        tracing::info_span!("http_request", method = %request.method(), path = %request.uri().path())
    });
    core.merge(crate::media_features::router(state.clone()))
        .merge(crate::catalog_navigation::router(state.clone()))
        .merge(crate::client_connection::router(state.clone()))
        .merge(crate::user_settings::router(state.clone()))
        .merge(crate::playlists::router(state.clone()))
        .merge(crate::offline::router(state.clone()))
        .merge(crate::metadata::router(state.clone()))
        .merge(crate::plugins::router(state.clone()))
        .nest_service(
            "/web",
            ServeDir::new(web_root).append_index_html_on_directories(true),
        )
        .layer(RequestBodyLimitLayer::new(64 * 1024))
        .layer(trace)
        .layer(cors)
        .layer(axum::middleware::from_fn_with_state(
            state.clone(),
            add_response_security_headers,
        ))
}

#[derive(Debug, PartialEq, Eq)]
struct MediaAccessTokenRequestFacts {
    origin_present: bool,
    host_present: bool,
    cookie_present: bool,
    query_present: bool,
    extra_auth_header_present: bool,
    fetch_site_same_origin: bool,
    exact_origin_match: bool,
}

impl MediaAccessTokenRequestFacts {
    fn from_parts(parts: &axum::http::request::Parts, config: &crate::config::Config) -> Self {
        Self {
            origin_present: parts.headers.contains_key(header::ORIGIN),
            host_present: parts.headers.contains_key(header::HOST),
            cookie_present: parts.headers.contains_key(header::COOKIE),
            query_present: parts.uri.query().is_some(),
            extra_auth_header_present: [
                header::AUTHORIZATION.as_str(),
                "x-emby-authorization",
                "x-emby-token",
                "x-mediabrowser-token",
            ]
            .iter()
            .any(|name| parts.headers.contains_key(*name)),
            fetch_site_same_origin: auth::fetch_metadata_confirms_same_origin(parts),
            exact_origin_match: auth::origin_is_exact_same_origin(parts, config),
        }
    }
}

async fn add_response_security_headers(
    State(state): State<AppState>,
    request: Request,
    next: axum::middleware::Next,
) -> Response {
    let is_web_asset = request.uri().path() == "/web" || request.uri().path().starts_with("/web/");
    let is_media_token_exchange = request.uri().path() == "/Users/Me/MediaAccessToken";
    let is_media_token_exchange_post =
        is_media_token_exchange && request.method() == axum::http::Method::POST;
    let (parts, body) = request.into_parts();
    let media_token_request_facts = is_media_token_exchange_post
        .then(|| MediaAccessTokenRequestFacts::from_parts(&parts, &state.config));
    let mut response = next.run(Request::from_parts(parts, body)).await;
    if let Some(facts) = media_token_request_facts {
        tracing::info!(
            status = response.status().as_u16(),
            origin_present = facts.origin_present,
            host_present = facts.host_present,
            cookie_present = facts.cookie_present,
            query_present = facts.query_present,
            extra_auth_header_present = facts.extra_auth_header_present,
            fetch_site_same_origin = facts.fetch_site_same_origin,
            exact_origin_match = facts.exact_origin_match,
            "media access token exchange"
        );
    }
    response.headers_mut().insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    if is_web_asset {
        // Embedded clients can retain web assets independently from the
        // server process. Require revalidation so deployments do not leave
        // those clients running stale JavaScript after an update.
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("no-cache, must-revalidate"),
        );
    }
    if is_media_token_exchange {
        response
            .headers_mut()
            .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
        response
            .headers_mut()
            .insert(header::PRAGMA, HeaderValue::from_static("no-cache"));
        response
            .headers_mut()
            .remove(header::ACCESS_CONTROL_ALLOW_ORIGIN);
        response
            .headers_mut()
            .remove(header::ACCESS_CONTROL_ALLOW_CREDENTIALS);
        response
            .headers_mut()
            .remove(header::ACCESS_CONTROL_EXPOSE_HEADERS);
    }
    response
}

#[cfg(test)]
mod media_access_token_log_tests {
    use super::*;
    use axum::body::Body;

    #[test]
    fn exchange_log_facts_contain_presence_flags_without_request_values() {
        let config = crate::config::Config {
            bind: "127.0.0.1:8096".parse().unwrap(),
            public_base_url: None,
            database_url: "postgres://127.0.0.1/test".to_owned(),
            server_name: "test".to_owned(),
            web_root: PathBuf::from("web"),
            data_dir: PathBuf::from("data"),
            ffmpeg_path: None,
            max_scan_workers: 1,
            max_page_size: 100,
            access_token_lifetime_hours: 24,
            cookie_secure: false,
            cors_origins: Vec::new(),
            trusted_proxies: Vec::new(),
            local_networks: Vec::new(),
            setup_token: None,
            bootstrap_admin_username: None,
            bootstrap_admin_password: None,
        };
        let request = Request::builder()
            .method(axum::http::Method::POST)
            .uri("/Users/Me/MediaAccessToken?ApiKey=query-secret")
            .header(header::HOST, "host-secret")
            .header(header::ORIGIN, "https://origin-secret")
            .header("sec-fetch-site", "same-origin")
            .header(header::COOKIE, "puffinbox_session=cookie-secret")
            .header(header::AUTHORIZATION, "Bearer header-secret")
            .body(Body::empty())
            .unwrap();
        let (parts, _) = request.into_parts();
        let facts = MediaAccessTokenRequestFacts::from_parts(&parts, &config);
        let logged_fields = format!("{facts:?}");

        assert!(facts.origin_present);
        assert!(facts.host_present);
        assert!(facts.cookie_present);
        assert!(facts.query_present);
        assert!(facts.extra_auth_header_present);
        assert!(facts.fetch_site_same_origin);
        assert!(!facts.exact_origin_match);
        for secret in [
            "host-secret",
            "origin-secret",
            "cookie-secret",
            "header-secret",
            "query-secret",
        ] {
            assert!(!logged_fields.contains(secret));
        }
    }
}

fn cors_layer(state: &AppState) -> CorsLayer {
    cors_layer_for_origins(&state.config.cors_origins)
}

fn cors_layer_for_origins(configured_origins: &[String]) -> CorsLayer {
    if configured_origins.is_empty() {
        return CorsLayer::new();
    }
    let origins: Vec<HeaderValue> = configured_origins
        .iter()
        .filter_map(|origin| HeaderValue::from_str(origin).ok())
        .collect();
    CorsLayer::new()
        .allow_origin(AllowOrigin::list(origins))
        .allow_credentials(true)
        .allow_methods(AllowMethods::list(
            ["GET", "HEAD", "POST", "PUT", "DELETE", "OPTIONS"]
                .into_iter()
                .filter_map(|v| v.parse().ok()),
        ))
        .allow_headers(AllowHeaders::list(
            [
                "authorization",
                "content-type",
                "if-none-match",
                "range",
                "x-emby-token",
                "x-emby-authorization",
                "x-mediabrowser-token",
            ]
            .into_iter()
            .filter_map(|v| v.parse().ok()),
        ))
        .expose_headers([
            header::CONTENT_RANGE,
            header::ACCEPT_RANGES,
            header::CONTENT_LENGTH,
            header::ETAG,
            HeaderName::from_static("x-chunk-sha256"),
        ])
}

#[cfg(test)]
mod cors_tests {
    use axum::{
        body::Body,
        http::{Method, Request, StatusCode},
        routing::get,
    };
    use tower::ServiceExt;

    use super::cors_layer_for_origins;

    #[tokio::test]
    async fn configured_origin_can_revalidate_private_artwork() {
        let app = axum::Router::new()
            .route("/image", get(|| async { "image" }))
            .layer(cors_layer_for_origins(&[
                "https://client.example".to_owned()
            ]));
        let preflight = Request::builder()
            .method(Method::OPTIONS)
            .uri("/image")
            .header("Origin", "https://client.example")
            .header("Access-Control-Request-Method", "GET")
            .header("Access-Control-Request-Headers", "if-none-match")
            .body(Body::empty())
            .unwrap();
        let preflight_response = app.clone().oneshot(preflight).await.unwrap();
        assert_eq!(preflight_response.status(), StatusCode::OK);
        let allowed_headers = preflight_response.headers()["access-control-allow-headers"]
            .to_str()
            .unwrap()
            .to_ascii_lowercase();
        assert!(allowed_headers.contains("if-none-match"));

        let request = Request::builder()
            .method(Method::GET)
            .uri("/image")
            .header("Origin", "https://client.example")
            .body(Body::empty())
            .unwrap();
        let response = app.oneshot(request).await.unwrap();
        let exposed = response.headers()["access-control-expose-headers"]
            .to_str()
            .unwrap()
            .to_ascii_lowercase();
        assert!(exposed.contains("etag"));
    }
}

async fn root_redirect() -> Redirect {
    Redirect::temporary("/web/")
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct BrandingOptionsDto {
    login_disclaimer: Option<&'static str>,
    custom_css: Option<&'static str>,
    splashscreen_enabled: bool,
}

async fn branding_configuration() -> Json<BrandingOptionsDto> {
    Json(BrandingOptionsDto {
        login_disclaimer: None,
        custom_css: None,
        splashscreen_enabled: false,
    })
}

async fn quick_connect_enabled() -> Json<bool> {
    Json(false)
}

async fn public_users() -> Json<Vec<UserDto>> {
    // Accounts use manual sign-in; do not publish their names anonymously.
    Json(Vec::new())
}

#[derive(Serialize)]
#[serde(rename_all = "camelCase")]
struct HealthResponse {
    status: &'static str,
}

async fn health() -> Json<HealthResponse> {
    Json(HealthResponse { status: "ok" })
}

async fn ready(State(state): State<AppState>) -> Result<Json<HealthResponse>, ApiError> {
    sqlx::query("SELECT 1").execute(&state.db).await?;
    Ok(Json(HealthResponse { status: "ready" }))
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct SystemInfoDto {
    local_address: Option<String>,
    server_name: String,
    version: &'static str,
    id: Uuid,
    product_name: &'static str,
    operating_system: &'static str,
    startup_wizard_completed: bool,
    puffinbox_version: &'static str,
}

async fn system_info_inner(state: &AppState) -> Result<SystemInfoDto, ApiError> {
    Ok(SystemInfoDto {
        local_address: state
            .config
            .public_base_url
            .as_ref()
            .map(|url| url.origin().ascii_serialization())
            .or_else(|| {
                (!state.config.bind.ip().is_unspecified() && state.config.bind.port() != 0)
                    .then(|| format!("http://{}", state.config.bind))
            }),
        server_name: state.config.server_name.clone(),
        // Clients use Version as their server compatibility floor; the product
        // and its own release remain separately identified below.
        version: "12.0.0",
        id: state.server_id,
        product_name: "PuffinBox",
        operating_system: std::env::consts::OS,
        startup_wizard_completed: db::user_count(&state.db).await? > 0,
        puffinbox_version: env!("CARGO_PKG_VERSION"),
    })
}

async fn public_system_info(
    State(state): State<AppState>,
) -> Result<Json<SystemInfoDto>, ApiError> {
    Ok(Json(system_info_inner(&state).await?))
}

async fn system_info(
    State(state): State<AppState>,
    _user: CurrentUser,
) -> Result<Json<SystemInfoDto>, ApiError> {
    Ok(Json(system_info_inner(&state).await?))
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct ParentalRatingDto {
    name: String,
    value: i32,
    rating_score: ParentalRatingScoreDto,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct ParentalRatingScoreDto {
    score: i32,
    sub_score: Option<i32>,
}

async fn parental_ratings() -> Json<Vec<ParentalRatingDto>> {
    Json(
        [
            ("G", 0),
            ("PG", 25),
            ("PG-13", 50),
            ("R", 75),
            ("NC-17", 100),
        ]
        .into_iter()
        .map(|(label, ordinal)| ParentalRatingDto {
            name: format!("US-MPAA-v1: {label}"),
            value: ordinal,
            rating_score: ParentalRatingScoreDto {
                score: ordinal,
                sub_score: None,
            },
        })
        .collect(),
    )
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct StartupConfigurationDto {
    server_name: String,
    is_startup_wizard_completed: bool,
    setup_token_required: bool,
}

async fn startup_configuration(
    State(state): State<AppState>,
) -> Result<Json<StartupConfigurationDto>, ApiError> {
    let count = db::user_count(&state.db).await?;
    Ok(Json(StartupConfigurationDto {
        server_name: state.config.server_name.clone(),
        is_startup_wizard_completed: count > 0,
        setup_token_required: count == 0 && state.setup_token.is_some(),
    }))
}

#[derive(Deserialize)]
struct StartupUserRequest {
    #[serde(rename = "Username", alias = "username")]
    username: String,
    #[serde(rename = "Password", alias = "password")]
    password: String,
    #[serde(rename = "SetupToken", alias = "setupToken")]
    setup_token: String,
}

#[derive(Deserialize)]
struct AuthenticateRequest {
    #[serde(rename = "Username", alias = "username")]
    username: String,
    #[serde(rename = "Pw", alias = "pw", alias = "Password", alias = "password")]
    password: String,
    #[serde(default, rename = "DeviceId", alias = "deviceId")]
    device_id: Option<String>,
    #[serde(default, rename = "DeviceName", alias = "deviceName")]
    device_name: Option<String>,
    #[serde(default, rename = "Client", alias = "client")]
    client: Option<String>,
    #[serde(default, rename = "Version", alias = "version")]
    client_version: Option<String>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct UserPolicyDto {
    is_administrator: bool,
    is_disabled: bool,
    enable_remote_access: bool,
    enable_media_playback: bool,
    enable_content_downloading: bool,
    enable_live_tv_access: bool,
    enable_live_tv_management: bool,
    enable_all_folders: bool,
    enabled_folders: Vec<Uuid>,
    max_parental_rating: Option<i32>,
    block_unrated_items: Vec<String>,
    authentication_provider_id: &'static str,
    password_reset_provider_id: &'static str,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct UserDto {
    id: Uuid,
    server_id: Uuid,
    name: String,
    has_password: bool,
    has_configured_password: bool,
    is_administrator: bool,
    policy: UserPolicyDto,
    configuration: crate::user_settings::UserConfiguration,
}

fn user_dto(user: &UserRecord, server_id: Uuid) -> UserDto {
    UserDto {
        id: user.id,
        server_id,
        name: user.username.clone(),
        has_password: true,
        has_configured_password: true,
        is_administrator: user.is_admin,
        configuration: user.configuration.clone(),
        policy: UserPolicyDto {
            is_administrator: user.is_admin,
            is_disabled: user.disabled,
            enable_remote_access: user.enable_remote_access,
            enable_media_playback: user.allow_media_playback,
            enable_content_downloading: user.enable_content_downloading,
            enable_live_tv_access: user.is_admin || user.enable_live_tv_access,
            enable_live_tv_management: user.is_admin || user.enable_live_tv_management,
            enable_all_folders: user.is_admin || !user.restrict_libraries,
            enabled_folders: user.allowed_library_ids.clone(),
            max_parental_rating: user.max_parental_rating,
            block_unrated_items: user.block_unrated_items.clone(),
            authentication_provider_id: "PuffinBox.LocalAuthentication",
            password_reset_provider_id: "PuffinBox.LocalPasswordReset",
        },
    }
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct AuthenticationResponseDto {
    user: UserDto,
    session_info: SessionDto,
    access_token: String,
    server_id: Uuid,
}

async fn authentication_response(
    state: &AppState,
    user: &UserRecord,
    token: String,
) -> Result<Response, ApiError> {
    let session = db::auth_session_by_token(&state.db, &auth::token_digest(&token))
        .await?
        .ok_or(ApiError::Unauthorized)?;
    let mut response = Json(AuthenticationResponseDto {
        user: user_dto(user, state.server_id),
        session_info: session_dto(session, state.server_id),
        access_token: token.clone(),
        server_id: state.server_id,
    })
    .into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        auth::cookie_header(&token, &state.config)?,
    );
    Ok(response)
}

async fn startup_create_user(
    State(state): State<AppState>,
    request: Request,
) -> Result<Response, ApiError> {
    let (parts, body) = request.into_parts();
    require_json_content_type(&parts.headers)?;
    validate_login_origin(&parts, &state.config)?;
    if !auth::client_is_local(&parts, &state.config) {
        return Err(ApiError::Forbidden);
    }
    let body = to_bytes(body, 64 * 1024)
        .await
        .map_err(|_| ApiError::BadRequest("Request body is too large or invalid".to_owned()))?;
    let body: StartupUserRequest = serde_json::from_slice(&body)
        .map_err(|_| ApiError::BadRequest("Setup request is invalid".to_owned()))?;
    if db::user_count(&state.db).await? > 0 {
        return Err(ApiError::Conflict(
            "Initial account setup has already completed".to_owned(),
        ));
    }
    let expected = state
        .setup_token
        .as_deref()
        .ok_or_else(|| ApiError::Conflict("Initial account setup is disabled".to_owned()))?;
    if expected.len() != body.setup_token.len()
        || !bool::from(expected.as_bytes().ct_eq(body.setup_token.as_bytes()))
    {
        return Err(ApiError::Unauthorized);
    }
    auth::validate_username(&body.username)?;
    let password_hash = auth::hash_password(&state, body.password).await?;
    let user = db::bootstrap_first_admin(&state.db, state.run_id, &body.username, &password_hash)
        .await?
        .ok_or_else(|| {
            ApiError::Conflict("Initial account setup has already completed".to_owned())
        })?;
    let _ = tokio::fs::remove_file(state.config.data_dir.join("setup-token")).await;
    let issued =
        auth::issue_token(&state, &user, "PuffinBox", "Initial setup", "bootstrap").await?;
    authentication_response(&state, &user, issued.token).await
}

async fn authenticate_by_name(
    State(state): State<AppState>,
    request: Request,
) -> Result<Response, ApiError> {
    let (parts, body) = request.into_parts();
    require_json_content_type(&parts.headers)?;
    validate_login_origin(&parts, &state.config)?;
    let body = to_bytes(body, 64 * 1024)
        .await
        .map_err(|_| ApiError::BadRequest("Request body is too large or invalid".to_owned()))?;
    let input: AuthenticateRequest = serde_json::from_slice(&body)
        .map_err(|_| ApiError::BadRequest("Authentication request is invalid".to_owned()))?;
    if input.username.len() > 64 || input.password.len() > 1024 {
        return Err(ApiError::BadRequest(
            "Authentication request exceeds field limits".to_owned(),
        ));
    }
    let header_identity = auth::client_identity_from_headers(&parts.headers)?;
    let client = login_identity(header_identity.client.as_deref(), input.client.as_deref())?
        .or(login_identity(
            header_identity.version.as_deref(),
            input.client_version.as_deref(),
        )?)
        .unwrap_or_else(|| "unknown".to_owned());
    let device_name = login_identity(
        header_identity.device_name.as_deref(),
        input.device_name.as_deref(),
    )?
    .unwrap_or_else(|| "unknown".to_owned());
    let device_id = login_identity_with_limit(
        header_identity.device_id.as_deref(),
        input.device_id.as_deref(),
        auth::MAX_DEVICE_ID_BYTES,
    )?
    .unwrap_or_else(|| "unknown".to_owned());
    let username = input.username.trim().to_ascii_lowercase();
    let remote = auth::client_address(&parts, &state.config.trusted_proxies)
        .map(|value| value.to_string())
        .unwrap_or_else(|| "unresolved".to_owned());
    let global_bucket = auth::token_digest(&format!("ip:{remote}"));
    let bucket_hash = auth::token_digest(&format!("user:{remote}:{username}"));
    if db::login_bucket_locked(&state.db, &global_bucket).await? {
        return Err(ApiError::RateLimited);
    }
    if db::login_bucket_locked(&state.db, &bucket_hash).await? {
        return Err(ApiError::RateLimited);
    }
    let record = db::find_user_by_name(&state.db, &username).await?;
    let Some((user, password_hash)) = record else {
        let _ = auth::verify_password(
            &state,
            input.password.clone(),
            state.dummy_password_hash.to_string(),
        )
        .await?;
        record_login_failure(&state, &global_bucket, &bucket_hash).await?;
        return Err(ApiError::Unauthorized);
    };
    let verified = auth::verify_password(&state, input.password, password_hash).await?;
    if !verified || user.disabled {
        record_login_failure(&state, &global_bucket, &bucket_hash).await?;
        return Err(ApiError::Unauthorized);
    }
    if !user.enable_remote_access && !auth::client_is_local(&parts, &state.config) {
        record_login_failure(&state, &global_bucket, &bucket_hash).await?;
        return Err(ApiError::Unauthorized);
    }
    db::clear_login_bucket(&state.db, state.run_id, &bucket_hash).await?;
    let issued = auth::issue_token(&state, &user, &client, &device_name, &device_id).await?;
    authentication_response(&state, &user, issued.token).await
}

async fn record_login_failure(
    state: &AppState,
    global_bucket: &str,
    user_bucket: &str,
) -> Result<(), ApiError> {
    let _ = db::issue_login_failure(&state.db, state.run_id, global_bucket, 60).await?;
    let _ = db::issue_login_failure(&state.db, state.run_id, user_bucket, 8).await?;
    Ok(())
}

fn require_json_content_type(headers: &HeaderMap) -> Result<(), ApiError> {
    let content_type = headers
        .get(header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .unwrap_or("");
    if content_type
        .split(';')
        .next()
        .is_some_and(|value| value.trim().eq_ignore_ascii_case("application/json"))
    {
        Ok(())
    } else {
        Err(ApiError::BadRequest(
            "Content-Type must be application/json".to_owned(),
        ))
    }
}

fn validate_login_origin(
    parts: &axum::http::request::Parts,
    config: &crate::Config,
) -> Result<(), ApiError> {
    if parts.headers.contains_key(header::ORIGIN)
        && !auth::origin_is_same_site_origin(parts, config)
    {
        return Err(ApiError::Forbidden);
    }
    Ok(())
}

fn login_identity(
    header_value: Option<&str>,
    body_value: Option<&str>,
) -> Result<Option<String>, ApiError> {
    login_identity_with_limit(header_value, body_value, 128)
}

fn login_identity_with_limit(
    header_value: Option<&str>,
    body_value: Option<&str>,
    max_bytes: usize,
) -> Result<Option<String>, ApiError> {
    let validate = |value: Option<&str>| -> Result<Option<String>, ApiError> {
        let Some(value) = value.map(str::trim).filter(|value| !value.is_empty()) else {
            return Ok(None);
        };
        if value.len() > max_bytes || value.chars().any(char::is_control) {
            return Err(ApiError::BadRequest(format!(
                "Client identity fields must be at most {max_bytes} bytes without control characters"
            )));
        }
        Ok(Some(value.to_owned()))
    };
    let header_value = validate(header_value)?;
    let body_value = validate(body_value)?;
    if header_value.is_some() && body_value.is_some() && header_value != body_value {
        return Err(ApiError::BadRequest(
            "Client identity in the body conflicts with the authorization header".to_owned(),
        ));
    }
    Ok(header_value.or(body_value))
}

#[derive(Default)]
enum PatchField<T> {
    #[default]
    Missing,
    Present(T),
}

impl<'de, T: Deserialize<'de>> Deserialize<'de> for PatchField<T> {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        T::deserialize(deserializer).map(Self::Present)
    }
}

#[derive(Deserialize, Default)]
#[serde(deny_unknown_fields)]
struct UserWriteRequest {
    #[serde(
        default,
        rename = "Name",
        alias = "name",
        alias = "Username",
        alias = "username"
    )]
    name: Option<String>,
    #[serde(default, rename = "Password", alias = "password")]
    password: Option<String>,
    #[serde(
        default,
        rename = "IsAdministrator",
        alias = "isAdministrator",
        alias = "is_admin"
    )]
    is_administrator: Option<bool>,
    #[serde(
        default,
        rename = "IsDisabled",
        alias = "isDisabled",
        alias = "disabled"
    )]
    is_disabled: Option<bool>,
    #[serde(default, rename = "EnableRemoteAccess", alias = "enableRemoteAccess")]
    enable_remote_access: Option<bool>,
    #[serde(default, rename = "EnableMediaPlayback", alias = "enableMediaPlayback")]
    enable_media_playback: Option<bool>,
    #[serde(
        default,
        rename = "EnableContentDownloading",
        alias = "enableContentDownloading"
    )]
    enable_content_downloading: Option<bool>,
    #[serde(default, rename = "EnableLiveTvAccess", alias = "enableLiveTvAccess")]
    enable_live_tv_access: Option<bool>,
    #[serde(
        default,
        rename = "EnableLiveTvManagement",
        alias = "enableLiveTvManagement"
    )]
    enable_live_tv_management: Option<bool>,
    #[serde(default, rename = "EnableAllFolders", alias = "enableAllFolders")]
    enable_all_folders: Option<bool>,
    #[serde(default, rename = "EnabledFolders", alias = "enabledFolders")]
    enabled_folders: Option<Vec<Uuid>>,
    #[serde(default, rename = "AllowedLibraryIds", alias = "allowedLibraryIds")]
    allowed_library_ids: Option<Vec<Uuid>>,
    #[serde(default, rename = "MaxParentalRating", alias = "maxParentalRating")]
    max_parental_rating: PatchField<Option<i32>>,
    #[serde(default, rename = "BlockUnratedItems", alias = "blockUnratedItems")]
    block_unrated_items: PatchField<Option<Vec<String>>>,
}

impl UserWriteRequest {
    fn remote_access_for_new_user(&self) -> bool {
        self.enable_remote_access.unwrap_or(false)
    }
}

fn requested_libraries(
    body: &UserWriteRequest,
) -> Result<(Option<bool>, Option<Vec<Uuid>>), ApiError> {
    if body.enabled_folders.is_some() && body.allowed_library_ids.is_some() {
        return Err(ApiError::BadRequest(
            "Send either EnabledFolders or AllowedLibraryIds, not both".to_owned(),
        ));
    }
    let ids = body
        .enabled_folders
        .clone()
        .or_else(|| body.allowed_library_ids.clone());
    if ids.as_ref().is_some_and(|ids| {
        ids.len() > 10_000 || ids.iter().copied().collect::<HashSet<_>>().len() != ids.len()
    }) {
        return Err(ApiError::BadRequest(
            "Library folder list is too large or contains duplicates".to_owned(),
        ));
    }
    match body.enable_all_folders {
        Some(true) => Ok((Some(true), None)),
        Some(false) => Ok((Some(false), Some(ids.unwrap_or_default()))),
        None => Ok(ids
            .map(|ids| (Some(false), Some(ids)))
            .unwrap_or((None, None))),
    }
}

fn validate_user_policy(body: &UserWriteRequest) -> Result<Option<Option<i32>>, ApiError> {
    if let PatchField::Present(value) = &body.max_parental_rating
        && value.is_some_and(|rating| !(0..=100).contains(&rating))
    {
        return Err(ApiError::BadRequest(
            "MaxParentalRating must be between 0 and 100".to_owned(),
        ));
    }
    if let PatchField::Present(Some(values)) = &body.block_unrated_items {
        const ALLOWED: [&str; 9] = [
            "Movie",
            "Trailer",
            "Series",
            "Music",
            "Book",
            "LiveTvChannel",
            "LiveTvProgram",
            "ChannelContent",
            "Other",
        ];
        if values.len() > ALLOWED.len()
            || values
                .iter()
                .any(|value| !ALLOWED.contains(&value.as_str()))
        {
            return Err(ApiError::BadRequest(
                "BlockUnratedItems contains an unsupported category".to_owned(),
            ));
        }
    }
    Ok(match &body.max_parental_rating {
        PatchField::Missing => None,
        PatchField::Present(value) => Some(*value),
    })
}

fn block_unrated_patch(body: &UserWriteRequest) -> Option<Vec<String>> {
    match &body.block_unrated_items {
        PatchField::Missing => None,
        PatchField::Present(Some(values)) => Some(values.clone()),
        PatchField::Present(None) => Some(Vec::new()),
    }
}

async fn get_current_user(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> Json<UserDto> {
    Json(user_dto(&user, state.server_id))
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct MediaAccessTokenDto {
    access_token: String,
    expires_at: DateTime<Utc>,
}

async fn restore_media_access_token(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    request: Request,
) -> Result<Response, ApiError> {
    let (parts, _) = request.into_parts();
    if !auth::origin_is_exact_same_origin(&parts, &state.config) {
        return Err(ApiError::Forbidden);
    }
    let cookie_token = auth::cookie_only_session_token(&parts.headers, parts.uri.query())?;
    let (parent_token_id, session_user) =
        db::active_auth_identity(&state.db, &auth::token_digest(&cookie_token))
            .await?
            .ok_or(ApiError::Unauthorized)?;
    if session_user.id != user.id {
        return Err(ApiError::Unauthorized);
    }
    if !user.allow_media_playback {
        return Err(ApiError::Forbidden);
    }
    let token = auth::issue_media_access_token(&state, parent_token_id).await?;
    let response = Json(MediaAccessTokenDto {
        access_token: token.token,
        expires_at: token.expires_at,
    })
    .into_response();
    Ok(response)
}

async fn logout(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if let Some((token, _from_cookie)) = auth::extract_raw_token(&headers)? {
        db::revoke_auth_token(
            &state.db,
            state.run_id,
            &auth::token_digest(&token),
            user.id,
        )
        .await?;
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    response.headers_mut().insert(
        header::SET_COOKIE,
        auth::expired_cookie_header(&state.config)?,
    );
    Ok(response)
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct SessionDto {
    id: Uuid,
    server_id: Uuid,
    user_id: Uuid,
    user_name: String,
    client: String,
    device_name: String,
    device_id: String,
    date_created: DateTime<Utc>,
    last_activity_date: DateTime<Utc>,
    is_active: bool,
    supports_remote_control: bool,
    supports_media_control: bool,
    playable_media_types: Vec<String>,
    supported_commands: Vec<String>,
    play_state: serde_json::Value,
    additional_users: Vec<serde_json::Value>,
    now_playing_queue: Vec<serde_json::Value>,
    capabilities: serde_json::Value,
}

fn session_dto(row: db::AuthSessionRecord, server_id: Uuid) -> SessionDto {
    SessionDto {
        id: row.id,
        server_id,
        user_id: row.user_id,
        user_name: row.username,
        client: row.client,
        device_name: row.device_name,
        device_id: row.device_id,
        date_created: row.created_at,
        last_activity_date: row.last_seen_at,
        is_active: Utc::now()
            .signed_duration_since(row.last_seen_at)
            .num_minutes()
            <= 30,
        supports_remote_control: false,
        supports_media_control: false,
        playable_media_types: serde_json::from_value(
            row.capabilities["PlayableMediaTypes"].clone(),
        )
        .unwrap_or_default(),
        supported_commands: serde_json::from_value(row.capabilities["SupportedCommands"].clone())
            .unwrap_or_default(),
        play_state: serde_json::json!({"IsPaused": false, "CanSeek": false}),
        additional_users: Vec::new(),
        now_playing_queue: Vec::new(),
        capabilities: row.capabilities,
    }
}

async fn list_sessions(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> Result<Json<Vec<SessionDto>>, ApiError> {
    let rows = db::list_auth_sessions(&state.db, user.id, user.is_admin).await?;
    Ok(Json(
        rows.into_iter()
            .map(|row| session_dto(row, state.server_id))
            .collect(),
    ))
}

#[derive(Default, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
struct ClientCapabilitiesDto {
    playable_media_types: Vec<String>,
    supported_commands: Vec<String>,
    supports_media_control: bool,
    supports_persistent_identifier: bool,
    device_profile: Option<serde_json::Value>,
    app_store_url: Option<String>,
    icon_url: Option<String>,
}

impl ClientCapabilitiesDto {
    fn validate(&self) -> Result<(), ApiError> {
        let valid_text = |value: &str, limit: usize| {
            value.len() <= limit && !value.chars().any(char::is_control)
        };
        if self.playable_media_types.len() > 16
            || self.supported_commands.len() > 128
            || self
                .playable_media_types
                .iter()
                .any(|value| !valid_text(value, 128))
            || self
                .supported_commands
                .iter()
                .any(|value| !valid_text(value, 128))
            || self
                .app_store_url
                .iter()
                .chain(self.icon_url.iter())
                .any(|value| !valid_text(value, 2048))
            || self
                .device_profile
                .as_ref()
                .is_some_and(|value| !value.is_object())
        {
            return Err(ApiError::BadRequest(
                "Invalid client capabilities".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Default, Deserialize)]
struct CapabilitiesQuery {
    #[serde(alias = "Id")]
    id: Option<Uuid>,
}

async fn update_session_capabilities(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    headers: HeaderMap,
    Query(query): Query<CapabilitiesQuery>,
    Json(capabilities): Json<ClientCapabilitiesDto>,
) -> Result<StatusCode, ApiError> {
    capabilities.validate()?;
    let (token, _) = auth::extract_raw_token(&headers)?.ok_or(ApiError::Unauthorized)?;
    let session = db::auth_session_by_token(&state.db, &auth::token_digest(&token))
        .await?
        .ok_or(ApiError::Unauthorized)?;
    let capabilities = serde_json::to_value(capabilities)
        .map_err(|_| ApiError::Internal("client capabilities serialization failed".to_owned()))?;
    let updated = db::update_auth_session_capabilities(
        &state.db,
        state.run_id,
        query.id.unwrap_or(session.id),
        user.id,
        user.is_admin,
        &capabilities,
    )
    .await?;
    if !updated {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
struct PlaybackEventRequest {
    item_id: Option<Uuid>,
    play_session_id: Option<String>,
    position_ticks: Option<i64>,
    play_method: Option<String>,
    played_to_completion: Option<bool>,
}

struct DeviceContext {
    id: String,
    name: String,
    client: String,
}

async fn device_context(
    state: &AppState,
    user: &UserRecord,
    headers: &HeaderMap,
) -> Result<DeviceContext, ApiError> {
    let (token, _) = auth::extract_raw_token(headers)?.ok_or(ApiError::Unauthorized)?;
    let record = db::auth_session_by_token(&state.db, &auth::token_digest(&token))
        .await?
        .ok_or(ApiError::Unauthorized)?;
    if record.user_id != user.id {
        return Err(ApiError::Unauthorized);
    }
    Ok(DeviceContext {
        id: record.device_id,
        name: record.device_name,
        client: record.client,
    })
}

fn parse_play_session_id(raw: Option<&str>) -> Result<Option<Uuid>, ApiError> {
    raw.map(|value| {
        Uuid::parse_str(value)
            .map_err(|_| ApiError::BadRequest("PlaySessionId must be a UUID".to_owned()))
    })
    .transpose()
}

fn validate_playback_position(value: Option<i64>) -> Result<(), ApiError> {
    if value.is_some_and(|ticks| !(0..=3_155_760_000_000_000).contains(&ticks)) {
        return Err(ApiError::BadRequest(
            "PositionTicks is outside the supported range".to_owned(),
        ));
    }
    Ok(())
}

async fn start_playback(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    headers: HeaderMap,
    Json(body): Json<PlaybackEventRequest>,
) -> Result<StatusCode, ApiError> {
    if !user.allow_media_playback {
        return Err(ApiError::Forbidden);
    }
    let item_id = body
        .item_id
        .ok_or_else(|| ApiError::BadRequest("ItemId is required".to_owned()))?;
    let item = db::get_item(&state.db, item_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !db::item_visible_to_user(&state.db, &user, &item).await? {
        return Err(ApiError::NotFound);
    }
    validate_playback_position(body.position_ticks)?;
    let device = device_context(&state, &user, &headers).await?;
    let play_session_id =
        parse_play_session_id(body.play_session_id.as_deref())?.unwrap_or_else(Uuid::new_v4);
    match db::start_playback_session(
        &state.db,
        db::PlaybackStartRequest {
            id: play_session_id,
            run_id: state.run_id,
            user_id: user.id,
            item_id,
            device_id: device.id,
            device_name: device.name,
            client: device.client,
            play_method: body.play_method,
            position_ticks: body.position_ticks,
        },
    )
    .await?
    {
        db::PlaybackStartResult::Started => {}
        db::PlaybackStartResult::AlreadyActive => {}
        db::PlaybackStartResult::Conflict => {
            return Err(ApiError::Conflict(
                "PlaySessionId is already associated with another playback session".to_owned(),
            ));
        }
        db::PlaybackStartResult::StaleRun => return Err(ApiError::Unavailable),
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn progress_playback(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    headers: HeaderMap,
    Json(body): Json<PlaybackEventRequest>,
) -> Result<StatusCode, ApiError> {
    if !user.allow_media_playback {
        return Err(ApiError::Forbidden);
    }
    validate_playback_position(body.position_ticks)?;
    let device = device_context(&state, &user, &headers).await?;
    let session_id = parse_play_session_id(body.play_session_id.as_deref())?;
    let active = db::active_playback_session(
        &state.db,
        state.run_id,
        user.id,
        &device.id,
        session_id,
        body.item_id,
    )
    .await?
    .ok_or(ApiError::NotFound)?;
    let item_id = active.item_id.ok_or(ApiError::NotFound)?;
    visible_playback_item(&state, &user, item_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    db::update_playback_session(
        &state.db,
        db::PlaybackSessionSelector {
            run_id: state.run_id,
            user_id: user.id,
            device_id: device.id,
            id: Some(active.id),
            item_id: Some(item_id),
        },
        body.position_ticks,
        body.play_method.as_deref(),
    )
    .await?
    .ok_or(ApiError::NotFound)?;
    let _ = crate::media_features::touch_playback_session(user.id, item_id, active.id).await;
    Ok(StatusCode::NO_CONTENT)
}

async fn stop_playback(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    headers: HeaderMap,
    Json(body): Json<PlaybackEventRequest>,
) -> Result<StatusCode, ApiError> {
    validate_playback_position(body.position_ticks)?;
    let device = device_context(&state, &user, &headers).await?;
    let session_id = parse_play_session_id(body.play_session_id.as_deref())?;
    let active = db::active_playback_session(
        &state.db,
        state.run_id,
        user.id,
        &device.id,
        session_id,
        body.item_id,
    )
    .await?
    .ok_or(ApiError::NotFound)?;
    let item_id = active.item_id.ok_or(ApiError::NotFound)?;
    let item = visible_playback_item(&state, &user, item_id).await?;
    let session = db::finish_playback_session(
        &state.db,
        db::PlaybackSessionSelector {
            run_id: state.run_id,
            user_id: user.id,
            device_id: device.id,
            id: Some(active.id),
            item_id: Some(item_id),
        },
        body.position_ticks,
        body.played_to_completion,
        user.allow_media_playback && item.is_some(),
    )
    .await?;
    crate::media_features::cancel_playback_session(user.id, item_id, active.id).await;
    let _session = session.ok_or(ApiError::NotFound)?;
    if !user.allow_media_playback {
        return Err(ApiError::Forbidden);
    }
    let _item = item.ok_or(ApiError::NotFound)?;
    Ok(StatusCode::NO_CONTENT)
}

async fn visible_playback_item(
    state: &AppState,
    user: &UserRecord,
    item_id: Uuid,
) -> Result<Option<ItemRecord>, ApiError> {
    let Some(item) = db::get_item(&state.db, item_id).await? else {
        return Ok(None);
    };
    if !db::item_visible_to_user(&state.db, user, &item).await? {
        return Ok(None);
    }
    Ok(Some(item))
}

async fn list_users(
    State(state): State<AppState>,
    _admin: AdminUser,
) -> Result<Json<Vec<UserDto>>, ApiError> {
    Ok(Json(
        db::list_users(&state.db)
            .await?
            .iter()
            .map(|user| user_dto(user, state.server_id))
            .collect(),
    ))
}

async fn get_user(
    State(state): State<AppState>,
    CurrentUser(current): CurrentUser,
    Path(user_id): Path<Uuid>,
) -> Result<Json<UserDto>, ApiError> {
    if !current.is_admin && current.id != user_id {
        return Err(ApiError::Forbidden);
    }
    let user = db::get_user(&state.db, user_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(user_dto(&user, state.server_id)))
}

async fn create_user(
    State(state): State<AppState>,
    _admin: AdminUser,
    Json(body): Json<UserWriteRequest>,
) -> Result<Response, ApiError> {
    let username = body
        .name
        .as_deref()
        .ok_or_else(|| ApiError::BadRequest("Name is required".to_owned()))?
        .trim();
    auth::validate_username(username)?;
    let password = body
        .password
        .clone()
        .ok_or_else(|| ApiError::BadRequest("Password is required".to_owned()))?;
    let max_rating = validate_user_policy(&body)?.flatten();
    let categories = block_unrated_patch(&body).unwrap_or_default();
    let (all_folders, ids) = requested_libraries(&body)?;
    let allowed_library_ids = match all_folders {
        Some(true) => None,
        Some(false) => Some(ids.unwrap_or_default()),
        None => ids,
    };
    let password_hash = auth::hash_password(&state, password).await?;
    let is_admin = body.is_administrator.unwrap_or(false);
    let user = db::create_user(
        &state.db,
        state.run_id,
        &NewUser {
            username: username.to_owned(),
            password_hash,
            is_admin,
            disabled: body.is_disabled.unwrap_or(false),
            enable_remote_access: body.remote_access_for_new_user(),
            allow_media_playback: body.enable_media_playback.unwrap_or(true),
            enable_content_downloading: body.enable_content_downloading.unwrap_or(true),
            enable_live_tv_access: body.enable_live_tv_access.unwrap_or(is_admin),
            enable_live_tv_management: body.enable_live_tv_management.unwrap_or(is_admin),
            max_parental_rating: max_rating,
            block_unrated_items: categories,
            allowed_library_ids,
        },
    )
    .await
    .map_err(map_user_write_error)?;
    Ok(Json(user_dto(&user, state.server_id)).into_response())
}

async fn update_user(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(user_id): Path<Uuid>,
    Json(body): Json<UserWriteRequest>,
) -> Result<Json<UserDto>, ApiError> {
    if let Some(name) = body.name.as_deref() {
        auth::validate_username(name)?;
    }
    let max_parental_rating = validate_user_policy(&body)?;
    let (enable_all_folders, allowed_library_ids) = requested_libraries(&body)?;
    let password_hash = match body.password.clone() {
        Some(password) => Some(auth::hash_password(&state, password).await?),
        None => None,
    };
    let block_unrated_items = block_unrated_patch(&body);
    let patch = UserPatch {
        username: body.name,
        password_hash,
        is_admin: body.is_administrator,
        disabled: body.is_disabled,
        enable_remote_access: body.enable_remote_access,
        allow_media_playback: body.enable_media_playback,
        enable_content_downloading: body.enable_content_downloading,
        enable_live_tv_access: body.enable_live_tv_access,
        enable_live_tv_management: body.enable_live_tv_management,
        max_parental_rating,
        block_unrated_items,
        allowed_library_ids,
        enable_all_folders,
    };
    let user = db::update_user(&state.db, state.run_id, user_id, &patch)
        .await
        .map_err(map_user_write_error)?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(user_dto(&user, state.server_id)))
}

async fn remove_user(
    State(state): State<AppState>,
    CurrentUser(current): CurrentUser,
    Path(user_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    if !current.is_admin {
        return Err(ApiError::Forbidden);
    }
    if !db::delete_user(&state.db, state.run_id, user_id)
        .await
        .map_err(map_user_write_error)?
    {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn get_user_policy(
    State(state): State<AppState>,
    CurrentUser(current): CurrentUser,
    Path(user_id): Path<Uuid>,
) -> Result<Json<UserPolicyDto>, ApiError> {
    if !current.is_admin && current.id != user_id {
        return Err(ApiError::Forbidden);
    }
    let user = db::get_user(&state.db, user_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(user_dto(&user, state.server_id).policy))
}

async fn update_user_policy(
    State(state): State<AppState>,
    _admin: AdminUser,
    Path(user_id): Path<Uuid>,
    Json(body): Json<UserWriteRequest>,
) -> Result<Json<UserPolicyDto>, ApiError> {
    if body.name.is_some() || body.password.is_some() || body.is_administrator.is_some() {
        return Err(ApiError::BadRequest(
            "The policy endpoint only accepts policy fields".to_owned(),
        ));
    }
    let max_parental_rating = validate_user_policy(&body)?;
    let (enable_all_folders, allowed_library_ids) = requested_libraries(&body)?;
    let block_unrated_items = block_unrated_patch(&body);
    let patch = UserPatch {
        username: None,
        password_hash: None,
        is_admin: body.is_administrator,
        disabled: body.is_disabled,
        enable_remote_access: body.enable_remote_access,
        allow_media_playback: body.enable_media_playback,
        enable_content_downloading: body.enable_content_downloading,
        enable_live_tv_access: body.enable_live_tv_access,
        enable_live_tv_management: body.enable_live_tv_management,
        max_parental_rating,
        block_unrated_items,
        allowed_library_ids,
        enable_all_folders,
    };
    let user = db::update_user(&state.db, state.run_id, user_id, &patch)
        .await
        .map_err(map_user_write_error)?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(user_dto(&user, state.server_id).policy))
}

fn map_user_write_error(error: sqlx::Error) -> ApiError {
    if error
        .as_database_error()
        .is_some_and(DatabaseError::is_unique_violation)
    {
        ApiError::Conflict("A user with that name already exists".to_owned())
    } else if matches!(&error, sqlx::Error::Protocol(message) if message.contains("last enabled administrator"))
    {
        ApiError::Conflict("At least one enabled administrator must remain".to_owned())
    } else if matches!(&error, sqlx::Error::Protocol(message) if message.contains("library IDs do not exist"))
    {
        ApiError::BadRequest("One or more library folders do not exist".to_owned())
    } else {
        ApiError::from(error)
    }
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct LibraryDto {
    id: Uuid,
    item_id: Uuid,
    name: String,
    collection_type: String,
    locations: Option<Vec<String>>,
}

fn library_dto(library: &LibraryRecord, include_paths: bool) -> LibraryDto {
    LibraryDto {
        id: library.id,
        item_id: library.id,
        name: library.name.clone(),
        collection_type: library.collection_type.clone(),
        locations: include_paths.then(|| {
            library
                .locations
                .iter()
                .map(|path| path.to_string_lossy().into_owned())
                .collect()
        }),
    }
}

async fn list_libraries(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> Result<Json<Vec<LibraryDto>>, ApiError> {
    let libraries = db::list_libraries(&state.db, &user).await?;
    Ok(Json(
        libraries
            .iter()
            .map(|library| library_dto(library, user.is_admin))
            .collect(),
    ))
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct ScanStartResultDto {
    started: usize,
    already_running: usize,
    library_missing: usize,
    capacity_reached: usize,
    shutting_down: usize,
}

async fn refresh_library_scan(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
) -> Result<Response, ApiError> {
    let libraries = db::list_libraries(&state.db, &admin).await?;
    let mut result = ScanStartResultDto {
        started: 0,
        already_running: 0,
        library_missing: 0,
        capacity_reached: 0,
        shutting_down: 0,
    };
    for library in libraries {
        match crate::library::spawn_scan(state.clone(), library.id).await? {
            crate::library::ScanStart::Started => result.started += 1,
            crate::library::ScanStart::AlreadyRunning => result.already_running += 1,
            crate::library::ScanStart::LibraryMissing => result.library_missing += 1,
            crate::library::ScanStart::CapacityReached => result.capacity_reached += 1,
            crate::library::ScanStart::ShuttingDown => result.shutting_down += 1,
            crate::library::ScanStart::StaleRun => result.shutting_down += 1,
        }
    }
    let status = if (result.capacity_reached > 0 || result.shutting_down > 0) && result.started == 0
    {
        StatusCode::SERVICE_UNAVAILABLE
    } else {
        StatusCode::ACCEPTED
    };
    Ok((status, Json(result)).into_response())
}

async fn library_scan_status(
    State(state): State<AppState>,
    _admin: AdminUser,
) -> Result<Json<Vec<crate::library::ScanStatus>>, ApiError> {
    Ok(Json(crate::library::scan_status(&state.db).await?))
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct LibraryRootIdentityDto {
    library_id: Uuid,
    library_name: String,
    root_path: String,
    device_id: String,
    inode: String,
    last_verified_at: DateTime<Utc>,
}

async fn list_library_root_identities(
    State(state): State<AppState>,
    _admin: AdminUser,
) -> Result<Json<Vec<LibraryRootIdentityDto>>, ApiError> {
    let identities = db::list_library_root_identities(&state.db).await?;
    Ok(Json(
        identities
            .into_iter()
            .map(|identity| LibraryRootIdentityDto {
                library_id: identity.library_id,
                library_name: identity.library_name,
                root_path: identity.root_path,
                device_id: identity.device_id,
                inode: identity.inode,
                last_verified_at: identity.last_verified_at,
            })
            .collect(),
    ))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RebindLibraryRootRequest {
    #[serde(rename = "LibraryId", alias = "libraryId")]
    library_id: Uuid,
    #[serde(rename = "RootPath", alias = "rootPath")]
    root_path: PathBuf,
    #[serde(rename = "ExpectedDeviceId", alias = "expectedDeviceId")]
    expected_device_id: String,
    #[serde(rename = "ExpectedInode", alias = "expectedInode")]
    expected_inode: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct RebindLibraryRootResponse {
    root_status: &'static str,
    scan_status: &'static str,
    device_id: String,
    inode: String,
}

async fn rebind_library_root(
    State(state): State<AppState>,
    _admin: AdminUser,
    Json(request): Json<RebindLibraryRootRequest>,
) -> Result<Response, ApiError> {
    let expected_device_id = normalized_identity(&request.expected_device_id, "ExpectedDeviceId")?;
    let expected_inode = normalized_identity(&request.expected_inode, "ExpectedInode")?;
    let library = db::get_library_including_disabled(&state.db, request.library_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !library
        .locations
        .iter()
        .any(|root| root == &request.root_path)
    {
        return Err(ApiError::BadRequest(
            "RootPath must exactly match a configured library location".to_owned(),
        ));
    }
    let (device_id, inode) =
        crate::library::inspect_library_root_identity(request.root_path.clone())
            .await
            .map_err(|_| {
                ApiError::BadRequest(
            "RootPath cannot be opened as a normalized directory without symlink components"
                .to_owned(),
        )
            })?;
    let device_id = device_id.to_string();
    let inode = inode.to_string();
    let result = db::rebind_library_root(
        &state.db,
        state.run_id,
        &db::RootRebindRequest {
            library_id: request.library_id,
            root_path: request.root_path.clone(),
            expected_device_id,
            expected_inode,
            current_device_id: device_id.clone(),
            current_inode: inode.clone(),
        },
    )
    .await?;
    let root_status = match result {
        db::RootRebindResult::Rebound => "rebound",
        db::RootRebindResult::AlreadyCurrent => "already-current",
        db::RootRebindResult::LibraryMissing => return Err(ApiError::NotFound),
        db::RootRebindResult::RootNotConfigured => {
            return Err(ApiError::BadRequest(
                "RootPath must exactly match a configured library location".to_owned(),
            ));
        }
        db::RootRebindResult::IdentityMissing => {
            return Err(ApiError::Conflict(
                "No previous root identity exists; run an initial scan first".to_owned(),
            ));
        }
        db::RootRebindResult::IdentityChanged => {
            return Err(ApiError::Conflict(
                "The saved root identity changed during this request; reload root identities and retry".to_owned(),
            ));
        }
        db::RootRebindResult::ScanRunning => {
            return Err(ApiError::Conflict(
                "The library cannot be rebound while a scan is running".to_owned(),
            ));
        }
        db::RootRebindResult::StaleRun => return Err(ApiError::Unavailable),
    };
    let enabled = db::library_is_enabled(&state.db, request.library_id)
        .await?
        .unwrap_or(false);
    let scan_status = if !enabled {
        "deferred-disabled"
    } else {
        match crate::library::spawn_scan(state, request.library_id).await {
            Ok(crate::library::ScanStart::Started) => "started",
            Ok(crate::library::ScanStart::AlreadyRunning) => "already-running",
            Ok(crate::library::ScanStart::LibraryMissing) => "library-missing",
            Ok(crate::library::ScanStart::CapacityReached) => "capacity-reached",
            Ok(crate::library::ScanStart::ShuttingDown) => "shutting-down",
            Ok(crate::library::ScanStart::StaleRun) => "stale-run",
            Err(error) => {
                tracing::warn!(library_id = %request.library_id, error = %error, "root identity was updated but its scan could not be queued");
                "queue-error"
            }
        }
    };
    Ok((
        StatusCode::ACCEPTED,
        Json(RebindLibraryRootResponse {
            root_status,
            scan_status,
            device_id,
            inode,
        }),
    )
        .into_response())
}

fn normalized_identity(raw: &str, name: &str) -> Result<String, ApiError> {
    if raw.is_empty() || raw.len() > 20 || !raw.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(ApiError::BadRequest(format!(
            "{name} must be an unsigned decimal filesystem identity"
        )));
    }
    let value = raw
        .parse::<u64>()
        .map_err(|_| ApiError::BadRequest(format!("{name} is outside the supported range")))?;
    Ok(value.to_string())
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct CreateLibraryRequest {
    #[serde(default, rename = "Name", alias = "name")]
    name: Option<String>,
    #[serde(default, rename = "Locations", alias = "locations")]
    locations: Option<Vec<PathBuf>>,
    #[serde(default, rename = "CollectionType", alias = "collectionType")]
    collection_type: Option<String>,
    #[serde(default, rename = "RefreshLibrary", alias = "refreshLibrary")]
    refresh_library: Option<bool>,
    #[serde(default, rename = "LibraryOptions", alias = "libraryOptions")]
    library_options: Option<serde_json::Value>,
}

struct ResolvedCreateLibraryRequest {
    name: String,
    locations: Vec<PathBuf>,
    collection_type: String,
    refresh_library: bool,
    enabled: bool,
}

fn default_collection_type() -> String {
    "mixed".to_owned()
}

fn set_query_once<T>(slot: &mut Option<T>, value: T, field: &str) -> Result<(), ApiError> {
    if slot.is_some() {
        return Err(ApiError::BadRequest(format!(
            "{field} may appear only once"
        )));
    }
    *slot = Some(value);
    Ok(())
}

fn merge_library_field<T: PartialEq>(
    query: Option<T>,
    body: Option<T>,
    field: &str,
) -> Result<Option<T>, ApiError> {
    match (query, body) {
        (Some(query), Some(body)) if query != body => Err(ApiError::BadRequest(format!(
            "query and body {field} values conflict"
        ))),
        (Some(query), _) => Ok(Some(query)),
        (_, Some(body)) => Ok(Some(body)),
        (None, None) => Ok(None),
    }
}

fn resolve_create_library_request(
    query: Option<&str>,
    body_bytes: &[u8],
    headers: &HeaderMap,
) -> Result<ResolvedCreateLibraryRequest, ApiError> {
    let mut query_name = None;
    let mut query_collection = None;
    let mut query_refresh = None;
    let mut query_locations = Vec::new();
    if let Some(query) = query {
        for (key, value) in url::form_urlencoded::parse(query.as_bytes()) {
            match key.to_ascii_lowercase().as_str() {
                "name" => set_query_once(&mut query_name, value.into_owned(), "Name")?,
                "collectiontype" => {
                    set_query_once(&mut query_collection, value.into_owned(), "CollectionType")?
                }
                "refreshlibrary" => set_query_once(
                    &mut query_refresh,
                    value.parse::<bool>().map_err(|_| {
                        ApiError::BadRequest("RefreshLibrary must be true or false".to_owned())
                    })?,
                    "RefreshLibrary",
                )?,
                "path" | "paths" | "paths[]" => {
                    query_locations.push(PathBuf::from(value.into_owned()))
                }
                _ => {}
            }
        }
    }
    let body = if body_bytes.is_empty() {
        CreateLibraryRequest {
            name: None,
            locations: None,
            collection_type: None,
            refresh_library: None,
            library_options: None,
        }
    } else {
        require_json_content_type(headers)?;
        serde_json::from_slice::<CreateLibraryRequest>(body_bytes)
            .map_err(|_| ApiError::BadRequest("Library request is invalid".to_owned()))?
    };
    let name = merge_library_field(query_name, body.name, "Name")?
        .ok_or_else(|| ApiError::BadRequest("Name is required".to_owned()))?;
    let body_locations = body.locations;
    if !query_locations.is_empty()
        && body_locations
            .as_ref()
            .is_some_and(|values| values != &query_locations)
    {
        return Err(ApiError::BadRequest(
            "query and body location values conflict".to_owned(),
        ));
    }
    let locations = if query_locations.is_empty() {
        body_locations.unwrap_or_default()
    } else {
        query_locations
    };
    let collection_type =
        merge_library_field(query_collection, body.collection_type, "CollectionType")?
            .unwrap_or_else(default_collection_type);
    let refresh_library =
        merge_library_field(query_refresh, body.refresh_library, "RefreshLibrary")?
            .unwrap_or(false);
    let enabled = match body.library_options {
        None | Some(serde_json::Value::Null) => true,
        Some(serde_json::Value::Object(options)) => {
            let mut enabled = true;
            let mut seen_enabled = false;
            for (name, value) in options {
                if name.eq_ignore_ascii_case("Enabled") {
                    if seen_enabled {
                        return Err(ApiError::BadRequest(
                            "LibraryOptions contains duplicate Enabled values".to_owned(),
                        ));
                    }
                    enabled = value.as_bool().ok_or_else(|| {
                        ApiError::BadRequest("LibraryOptions.Enabled must be a boolean".to_owned())
                    })?;
                    seen_enabled = true;
                } else {
                    return Err(ApiError::BadRequest(format!(
                        "LibraryOptions.{name} is not implemented"
                    )));
                }
            }
            enabled
        }
        Some(_) => {
            return Err(ApiError::BadRequest(
                "LibraryOptions must be an object or null".to_owned(),
            ));
        }
    };
    Ok(ResolvedCreateLibraryRequest {
        name,
        locations,
        collection_type,
        refresh_library,
        enabled,
    })
}

async fn create_library(
    State(state): State<AppState>,
    _admin: AdminUser,
    request: Request,
) -> Result<Response, ApiError> {
    let (parts, request_body) = request.into_parts();
    let request_body = to_bytes(request_body, 64 * 1024)
        .await
        .map_err(|_| ApiError::BadRequest("Request body is too large or invalid".to_owned()))?;
    let body = resolve_create_library_request(parts.uri.query(), &request_body, &parts.headers)?;
    let name = body.name.trim();
    if name.is_empty() || name.len() > 128 {
        return Err(ApiError::BadRequest(
            "Library name must be 1–128 characters".to_owned(),
        ));
    }
    if body.locations.is_empty() || body.locations.len() > 64 {
        return Err(ApiError::BadRequest(
            "A library requires 1–64 directory locations".to_owned(),
        ));
    }
    let collection_type = body.collection_type.trim();
    if ![
        "mixed",
        "movies",
        "tvshows",
        "music",
        "musicvideos",
        "books",
        "photos",
        "homevideos",
        "boxsets",
    ]
    .contains(&collection_type.to_ascii_lowercase().as_str())
    {
        return Err(ApiError::BadRequest(
            "Unsupported collection type".to_owned(),
        ));
    }
    let mut locations = Vec::with_capacity(body.locations.len());
    let mut seen = Vec::new();
    for requested in body.locations {
        if !requested.is_absolute() {
            return Err(ApiError::BadRequest(
                "Library paths must be absolute".to_owned(),
            ));
        }
        let canonical = tokio::fs::canonicalize(&requested).await.map_err(|_| {
            ApiError::BadRequest("A library location does not exist or cannot be read".to_owned())
        })?;
        if canonical.to_str().is_none() {
            return Err(ApiError::BadRequest(
                "Library locations must use UTF-8 filesystem paths".to_owned(),
            ));
        }
        if !tokio::fs::metadata(&canonical)
            .await
            .map_err(|_| ApiError::BadRequest("A library location cannot be read".to_owned()))?
            .is_dir()
        {
            return Err(ApiError::BadRequest(
                "Every library location must be a directory".to_owned(),
            ));
        }
        if seen.iter().any(|existing: &PathBuf| {
            canonical.starts_with(existing) || existing.starts_with(&canonical)
        }) {
            return Err(ApiError::BadRequest(
                "Library locations must not overlap".to_owned(),
            ));
        }
        seen.push(canonical.clone());
        locations.push(canonical);
    }
    if locations.is_empty() {
        return Err(ApiError::BadRequest(
            "Library locations must be unique".to_owned(),
        ));
    }
    let id = Uuid::new_v4();
    db::insert_library(
        &state.db,
        state.run_id,
        id,
        name,
        &collection_type.to_ascii_lowercase(),
        &locations,
        body.enabled,
    )
    .await
    .map_err(|error| {
        if error
            .as_database_error()
            .is_some_and(DatabaseError::is_unique_violation)
        {
            ApiError::Conflict("A library with that name already exists".to_owned())
        } else {
            error.into()
        }
    })?;
    let scan_start = if body.enabled && body.refresh_library {
        Some(crate::library::spawn_scan(state.clone(), id).await?)
    } else {
        None
    };
    if matches!(
        scan_start,
        Some(
            crate::library::ScanStart::CapacityReached
                | crate::library::ScanStart::ShuttingDown
                | crate::library::ScanStart::StaleRun,
        )
    ) {
        tracing::warn!(library_id = %id, "new library is saved but no scan worker is currently available");
    }
    let mut response = StatusCode::NO_CONTENT.into_response();
    let scan_header = match scan_start {
        Some(crate::library::ScanStart::Started) => "started",
        Some(crate::library::ScanStart::AlreadyRunning) => "already-running",
        Some(crate::library::ScanStart::LibraryMissing) => "library-missing",
        Some(crate::library::ScanStart::CapacityReached) => "capacity-reached",
        Some(crate::library::ScanStart::ShuttingDown) => "shutting-down",
        Some(crate::library::ScanStart::StaleRun) => "stale-run",
        None => "not-requested",
    };
    response.headers_mut().insert(
        HeaderName::from_static("x-puffinbox-scan-status"),
        HeaderValue::from_static(scan_header),
    );
    Ok(response)
}

#[derive(Deserialize)]
struct RemoveLibraryQuery {
    #[serde(rename = "Name", alias = "name")]
    name: String,
}

async fn remove_library(
    State(state): State<AppState>,
    _admin: AdminUser,
    Query(query): Query<RemoveLibraryQuery>,
) -> Result<StatusCode, ApiError> {
    if !db::remove_library_by_name(&state.db, state.run_id, query.name.trim()).await? {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

#[cfg(test)]
mod library_request_tests {
    use super::*;

    #[test]
    fn accepts_public_virtual_folder_query_and_library_options_body() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let parsed = resolve_create_library_request(
            Some("name=Movies&collectionType=movies&paths=%2Fmnt%2Fmedia&refreshLibrary=false"),
            br#"{"LibraryOptions":{}}"#,
            &headers,
        )
        .unwrap();
        assert_eq!(parsed.name, "Movies");
        assert_eq!(parsed.collection_type, "movies");
        assert_eq!(parsed.locations, vec![PathBuf::from("/mnt/media")]);
        assert!(!parsed.refresh_library);
        assert!(parsed.enabled);
    }

    #[test]
    fn accepts_json_extension_and_supported_enabled_option() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let parsed = resolve_create_library_request(
            None,
            br#"{"Name":"Disabled","Locations":["/mnt/media"],"LibraryOptions":{"Enabled":false}}"#,
            &headers,
        )
        .unwrap();
        assert_eq!(parsed.name, "Disabled");
        assert!(!parsed.enabled);
    }

    #[test]
    fn rejects_unimplemented_library_options_instead_of_ignoring_them() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::CONTENT_TYPE,
            HeaderValue::from_static("application/json"),
        );
        let parsed = resolve_create_library_request(
            Some("name=Movies&collectionType=movies&paths=%2Fmnt%2Fmedia"),
            br#"{"LibraryOptions":{"EnablePhotos":true}}"#,
            &headers,
        );
        assert!(parsed.is_err());
    }
}

#[derive(Deserialize, Default)]
struct ItemsQueryParams {
    #[serde(default, rename = "ParentId", alias = "parentId")]
    parent_id: Option<Uuid>,
    #[serde(default, rename = "SearchTerm", alias = "searchTerm")]
    search_term: Option<String>,
    #[serde(default, rename = "IncludeItemTypes", alias = "includeItemTypes")]
    include_item_types: Option<String>,
    #[serde(default, rename = "Recursive", alias = "recursive")]
    recursive: Option<bool>,
    #[serde(default, rename = "StartIndex", alias = "startIndex")]
    start_index: Option<i64>,
    #[serde(default, rename = "Limit", alias = "limit")]
    limit: Option<i64>,
    #[serde(
        default = "default_true",
        rename = "EnableTotalRecordCount",
        alias = "enableTotalRecordCount"
    )]
    enable_total_record_count: bool,
    #[serde(default, rename = "SortBy", alias = "sortBy")]
    sort_by: Option<String>,
    #[serde(default, rename = "SortOrder", alias = "sortOrder")]
    sort_order: Option<String>,
    #[serde(default, rename = "IsPlayed", alias = "isPlayed")]
    is_played: Option<bool>,
    #[serde(default, rename = "Filters", alias = "filters")]
    filters: Option<String>,
    #[serde(default, rename = "UserId", alias = "userId")]
    user_id: Option<Uuid>,
}

fn default_true() -> bool {
    true
}

#[derive(Default, Serialize)]
#[serde(rename_all = "PascalCase")]
struct UserDataDto {
    played: bool,
    play_count: i32,
    is_favorite: bool,
    playback_position_ticks: i64,
    last_played_date: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    item_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    played_percentage: Option<f64>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct BaseItemDto {
    id: Uuid,
    server_id: Uuid,
    name: String,
    #[serde(rename = "Type")]
    item_type: String,
    is_folder: bool,
    media_type: Option<&'static str>,
    parent_id: Option<Uuid>,
    container: Option<String>,
    run_time_ticks: Option<i64>,
    date_created: DateTime<Utc>,
    date_modified: Option<DateTime<Utc>>,
    overview: Option<String>,
    #[serde(skip_serializing_if = "Vec::is_empty")]
    genres: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    official_rating: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    community_rating: Option<f64>,
    #[serde(skip_serializing_if = "Option::is_none")]
    premiere_date: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image_tags: Option<ItemImageTagsDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    series_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    season_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    index_number: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    parent_index_number: Option<i32>,
    #[serde(skip_serializing_if = "Option::is_none")]
    album: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    album_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    artist_items: Option<Vec<BaseItemPersonDto>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_data: Option<UserDataDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct ItemImageTagsDto {
    primary: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct BaseItemPersonDto {
    name: String,
    id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    role: Option<String>,
    #[serde(rename = "Type")]
    person_type: &'static str,
}

fn item_dto(
    item: &ItemRecord,
    server_id: Uuid,
    user_data: Option<UserDataDto>,
    navigation: Option<&db::ItemNavigationLinks>,
    metadata: Option<&crate::metadata::DisplayMetadata>,
    include_path: bool,
) -> BaseItemDto {
    BaseItemDto {
        id: item.id,
        server_id,
        name: metadata
            .and_then(|metadata| metadata.name.clone())
            .unwrap_or_else(|| item.name.clone()),
        item_type: item.item_type.clone(),
        is_folder: matches!(
            item.item_type.as_str(),
            "Folder"
                | "CollectionFolder"
                | "Season"
                | "BoxSet"
                | "Series"
                | "MusicArtist"
                | "MusicAlbum"
        ),
        media_type: match item.item_type.as_str() {
            "Movie" | "Series" | "Episode" | "Video" | "Trailer" | "MusicVideo" => Some("Video"),
            "Audio" | "MusicAlbum" | "MusicArtist" => Some("Audio"),
            "Photo" => Some("Photo"),
            "Book" | "AudioBook" | "EBook" => Some("Book"),
            _ => None,
        },
        parent_id: item.parent_id,
        container: item.container.clone(),
        run_time_ticks: item.runtime_ticks,
        date_created: item.date_added,
        date_modified: item.date_modified,
        overview: metadata
            .and_then(|metadata| metadata.overview.clone())
            .or_else(|| item.overview.clone()),
        genres: metadata
            .map(|metadata| metadata.genres.clone())
            .unwrap_or_default(),
        official_rating: metadata.and_then(|metadata| metadata.official_rating.clone()),
        community_rating: metadata.and_then(|metadata| metadata.community_score),
        premiere_date: metadata
            .and_then(|metadata| metadata.premiere_date)
            .and_then(|date| date.and_hms_opt(0, 0, 0))
            .map(|date_time| DateTime::<Utc>::from_naive_utc_and_offset(date_time, Utc)),
        image_tags: metadata
            .and_then(|metadata| metadata.primary_image_tag.clone())
            .map(|primary| ItemImageTagsDto { primary }),
        series_id: navigation.and_then(|links| links.series_id),
        season_id: navigation.and_then(|links| links.season_id),
        index_number: navigation.and_then(|links| links.index_number),
        parent_index_number: navigation.and_then(|links| links.parent_index_number),
        album: navigation.and_then(|links| links.album.clone()),
        album_id: navigation.and_then(|links| links.album_id),
        artist_items: navigation.and_then(|links| {
            links.artist_id.map(|id| {
                vec![BaseItemPersonDto {
                    name: links.artist.clone().unwrap_or_default(),
                    id,
                    role: None,
                    person_type: "Artist",
                }]
            })
        }),
        path: include_path.then(|| item.path.to_string_lossy().into_owned()),
        user_data,
    }
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct ItemsResultDto {
    items: Vec<BaseItemDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_record_count: Option<i64>,
    start_index: i64,
}

fn item_query(params: ItemsQueryParams, state: &AppState) -> Result<ItemQuery, ApiError> {
    let start_index = params.start_index.unwrap_or(0);
    if start_index < 0 {
        return Err(ApiError::BadRequest(
            "StartIndex cannot be negative".to_owned(),
        ));
    }
    let limit = params
        .limit
        .unwrap_or(100)
        .clamp(1, state.config.max_page_size.min(MAX_CATALOG_PAGE_SIZE));
    let search_term = match params.search_term {
        Some(value) if value.len() > 200 => {
            return Err(ApiError::BadRequest(
                "SearchTerm exceeds the 200-character limit".to_owned(),
            ));
        }
        Some(value) => value.trim().to_owned(),
        None => String::new(),
    };
    let search_term = (!search_term.is_empty()).then_some(search_term);
    let mut include_item_types = Vec::new();
    if let Some(raw) = params.include_item_types {
        include_item_types = raw
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
            .map(str::to_owned)
            .collect();
        if include_item_types.len() > 32 {
            return Err(ApiError::BadRequest(
                "IncludeItemTypes contains more than 32 values".to_owned(),
            ));
        }
        if include_item_types.iter().any(|value| value.len() > 64) {
            return Err(ApiError::BadRequest(
                "IncludeItemTypes contains an invalid value".to_owned(),
            ));
        }
    }
    let sort_by = params
        .sort_by
        .as_deref()
        .unwrap_or("SortName")
        .split(',')
        .next()
        .unwrap_or("SortName")
        .trim()
        .to_owned();
    if ![
        "SortName",
        "Name",
        "DateCreated",
        "DateAdded",
        "DateModified",
        "LastPlayedDate",
    ]
    .iter()
    .any(|allowed| sort_by.eq_ignore_ascii_case(allowed))
    {
        return Err(ApiError::BadRequest("Unsupported SortBy field".to_owned()));
    }
    let sort_order = params.sort_order.unwrap_or_else(|| "Ascending".to_owned());
    if !sort_order.eq_ignore_ascii_case("Ascending")
        && !sort_order.eq_ignore_ascii_case("Descending")
        && !sort_order.eq_ignore_ascii_case("Asc")
        && !sort_order.eq_ignore_ascii_case("Desc")
    {
        return Err(ApiError::BadRequest(
            "SortOrder must be Ascending or Descending".to_owned(),
        ));
    }
    let mut is_played = params.is_played;
    let mut is_favorite = false;
    let mut is_resumable = false;
    if let Some(filters) = params.filters {
        for filter in filters
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            match filter.to_ascii_lowercase().as_str() {
                "isplayed" => {
                    if is_played == Some(false) {
                        return Err(ApiError::BadRequest(
                            "Filters conflicts with IsPlayed".to_owned(),
                        ));
                    }
                    is_played = Some(true);
                }
                "isunplayed" => {
                    if is_played == Some(true) {
                        return Err(ApiError::BadRequest(
                            "Filters conflicts with IsPlayed".to_owned(),
                        ));
                    }
                    is_played = Some(false);
                }
                "isfavorite" => is_favorite = true,
                "isresumable" => is_resumable = true,
                _ => {
                    return Err(ApiError::BadRequest(
                        "Filters contains an unsupported item filter".to_owned(),
                    ));
                }
            }
        }
    }
    Ok(ItemQuery {
        parent_id: params.parent_id,
        search_term,
        exact_name: None,
        include_item_types,
        recursive: params.recursive.unwrap_or(false),
        start_index,
        limit,
        enable_total_record_count: params.enable_total_record_count,
        sort_by,
        sort_order,
        is_played,
        is_favorite,
        is_resumable,
    })
}

#[derive(Deserialize, Default)]
struct UserViewsParams {
    #[serde(default, rename = "UserId", alias = "userId")]
    user_id: Option<Uuid>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct LibraryViewDto {
    id: Uuid,
    server_id: Uuid,
    name: String,
    #[serde(rename = "Type")]
    item_type: &'static str,
    collection_type: String,
    is_folder: bool,
    display_preferences_id: String,
    user_data: UserDataDto,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct LibraryViewsResultDto {
    items: Vec<LibraryViewDto>,
    total_record_count: i64,
    start_index: i64,
}

pub(crate) fn library_view_dto(library: &LibraryRecord, server_id: Uuid) -> LibraryViewDto {
    LibraryViewDto {
        id: library.id,
        server_id,
        name: library.name.clone(),
        item_type: "CollectionFolder",
        collection_type: library.collection_type.clone(),
        is_folder: true,
        display_preferences_id: library.id.to_string(),
        user_data: empty_user_data(library.id),
    }
}

async fn user_views(
    State(state): State<AppState>,
    CurrentUser(current): CurrentUser,
    Query(params): Query<UserViewsParams>,
) -> Result<Json<LibraryViewsResultDto>, ApiError> {
    let selected = match params.user_id {
        Some(id) if id != current.id && !current.is_admin => return Err(ApiError::Forbidden),
        Some(id) if id != current.id => db::get_user(&state.db, id)
            .await?
            .ok_or(ApiError::NotFound)?,
        _ => current.clone(),
    };
    let libraries = db::list_libraries(&state.db, &selected).await?;
    let items = libraries
        .iter()
        .map(|library| library_view_dto(library, state.server_id))
        .collect::<Vec<_>>();
    Ok(Json(LibraryViewsResultDto {
        total_record_count: items.len() as i64,
        items,
        start_index: 0,
    }))
}

async fn browse_items(
    State(state): State<AppState>,
    CurrentUser(current): CurrentUser,
    Query(params): Query<ItemsQueryParams>,
) -> Result<Json<ItemsResultDto>, ApiError> {
    let selected = selected_user(&state, &current, params.user_id).await?;
    let query = item_query(params, &state)?;
    ensure_parent_visible(&state, &selected, query.parent_id).await?;
    Ok(Json(item_query_result(&state, &selected, query).await?))
}

pub(crate) async fn selected_user(
    state: &AppState,
    current: &UserRecord,
    selected_id: Option<Uuid>,
) -> Result<UserRecord, ApiError> {
    match selected_id {
        Some(id) if id != current.id && !current.is_admin => Err(ApiError::Forbidden),
        Some(id) if id != current.id => {
            db::get_user(&state.db, id).await?.ok_or(ApiError::NotFound)
        }
        _ => Ok(current.clone()),
    }
}

async fn ensure_parent_visible(
    state: &AppState,
    user: &UserRecord,
    parent_id: Option<Uuid>,
) -> Result<(), ApiError> {
    let Some(parent_id) = parent_id else {
        return Ok(());
    };
    if db::get_library(&state.db, parent_id).await?.is_some() {
        if !db::library_visible_to_user(&state.db, user, parent_id).await? {
            return Err(ApiError::NotFound);
        }
    } else {
        let parent = db::get_item(&state.db, parent_id)
            .await?
            .ok_or(ApiError::NotFound)?;
        if !db::item_visible_to_user(&state.db, user, &parent).await? {
            return Err(ApiError::NotFound);
        }
    }
    Ok(())
}

async fn item_query_result(
    state: &AppState,
    user: &UserRecord,
    query: ItemQuery,
) -> Result<ItemsResultDto, ApiError> {
    let start_index = query.start_index;
    let (items, total_record_count) = db::browse_items(&state.db, user, query).await?;
    let item_ids = items.iter().map(|item| item.id).collect::<Vec<_>>();
    let user_data = db::item_user_data(&state.db, user.id, &item_ids).await?;
    let navigation = db::item_navigation_links(&state.db, &item_ids).await?;
    let display_metadata = crate::metadata::load_display_metadata(&state.db, &item_ids).await?;
    let items = items
        .iter()
        .map(|item| {
            let data = user_data
                .get(&item.id)
                .map(|data| user_data_dto(data, item.id, item.runtime_ticks))
                .unwrap_or_else(|| empty_user_data(item.id));
            item_dto(
                item,
                state.server_id,
                Some(data),
                navigation.get(&item.id),
                display_metadata.get(&item.id),
                user.is_admin,
            )
        })
        .collect();
    Ok(ItemsResultDto {
        items,
        total_record_count,
        start_index,
    })
}

fn user_data_dto(
    data: &db::UserItemData,
    item_id: Uuid,
    runtime_ticks: Option<i64>,
) -> UserDataDto {
    UserDataDto {
        played: data.played,
        play_count: data.play_count,
        is_favorite: data.is_favorite,
        playback_position_ticks: data.playback_position_ticks,
        last_played_date: data.last_played_at,
        item_id: Some(item_id),
        played_percentage: runtime_ticks
            .filter(|duration| *duration > 0)
            .map(|duration| {
                if data.played {
                    100.0
                } else {
                    ((data.playback_position_ticks as f64 * 100.0) / duration as f64)
                        .clamp(0.0, 100.0)
                }
            }),
    }
}

fn empty_user_data(item_id: Uuid) -> UserDataDto {
    UserDataDto {
        item_id: Some(item_id),
        ..UserDataDto::default()
    }
}

#[derive(Deserialize, Default)]
struct LatestItemsParams {
    #[serde(default, rename = "ParentId", alias = "parentId")]
    parent_id: Option<Uuid>,
    #[serde(default, rename = "IncludeItemTypes", alias = "includeItemTypes")]
    include_item_types: Option<String>,
    #[serde(default, rename = "IsPlayed", alias = "isPlayed")]
    is_played: Option<bool>,
    #[serde(default, rename = "StartIndex", alias = "startIndex")]
    start_index: Option<i64>,
    #[serde(default, rename = "Limit", alias = "limit")]
    limit: Option<i64>,
    #[serde(default, rename = "UserId", alias = "userId")]
    user_id: Option<Uuid>,
}

async fn latest_items(
    State(state): State<AppState>,
    CurrentUser(current): CurrentUser,
    Query(params): Query<LatestItemsParams>,
) -> Result<Json<Vec<BaseItemDto>>, ApiError> {
    let user = selected_user(&state, &current, params.user_id).await?;
    ensure_parent_visible(&state, &user, params.parent_id).await?;
    let (start_index, limit) = page_values(params.start_index, params.limit, &state)?;
    let query = ItemQuery {
        parent_id: params.parent_id,
        search_term: None,
        exact_name: None,
        include_item_types: parse_item_types(params.include_item_types.as_deref())?,
        recursive: true,
        start_index,
        limit,
        enable_total_record_count: false,
        sort_by: "DateCreated".to_owned(),
        sort_order: "Descending".to_owned(),
        is_played: params.is_played,
        is_favorite: false,
        is_resumable: false,
    };
    let result = item_query_result(&state, &user, query).await?;
    Ok(Json(result.items))
}

#[derive(Deserialize, Default)]
struct ResumeItemsParams {
    #[serde(default, rename = "ParentId", alias = "parentId")]
    parent_id: Option<Uuid>,
    #[serde(default, rename = "IncludeItemTypes", alias = "includeItemTypes")]
    include_item_types: Option<String>,
    #[serde(default, rename = "SearchTerm", alias = "searchTerm")]
    search_term: Option<String>,
    #[serde(default, rename = "StartIndex", alias = "startIndex")]
    start_index: Option<i64>,
    #[serde(default, rename = "Limit", alias = "limit")]
    limit: Option<i64>,
    #[serde(default, rename = "UserId", alias = "userId")]
    user_id: Option<Uuid>,
}

async fn resume_items(
    State(state): State<AppState>,
    CurrentUser(current): CurrentUser,
    Query(params): Query<ResumeItemsParams>,
) -> Result<Json<ItemsResultDto>, ApiError> {
    let user = selected_user(&state, &current, params.user_id).await?;
    ensure_parent_visible(&state, &user, params.parent_id).await?;
    let (start_index, limit) = page_values(params.start_index, params.limit, &state)?;
    let search_term = normalized_search_term(params.search_term)?;
    let query = ItemQuery {
        parent_id: params.parent_id,
        search_term,
        exact_name: None,
        include_item_types: parse_item_types(params.include_item_types.as_deref())?,
        recursive: true,
        start_index,
        limit,
        enable_total_record_count: true,
        sort_by: "LastPlayedDate".to_owned(),
        sort_order: "Descending".to_owned(),
        is_played: None,
        is_favorite: false,
        is_resumable: true,
    };
    Ok(Json(item_query_result(&state, &user, query).await?))
}

#[derive(Deserialize, Default)]
struct ShowItemsParams {
    #[serde(default, rename = "UserId", alias = "userId")]
    user_id: Option<Uuid>,
    #[serde(default, rename = "SeasonId", alias = "seasonId")]
    season_id: Option<Uuid>,
    #[serde(default, rename = "StartIndex", alias = "startIndex")]
    start_index: Option<i64>,
    #[serde(default, rename = "Limit", alias = "limit")]
    limit: Option<i64>,
    #[serde(
        default = "default_true",
        rename = "EnableTotalRecordCount",
        alias = "enableTotalRecordCount"
    )]
    enable_total_record_count: bool,
}

async fn show_seasons(
    State(state): State<AppState>,
    CurrentUser(current): CurrentUser,
    Path(series_id): Path<Uuid>,
    Query(params): Query<ShowItemsParams>,
) -> Result<Json<ItemsResultDto>, ApiError> {
    let user = selected_user(&state, &current, params.user_id).await?;
    visible_item_of_type(&state, &user, series_id, "Series").await?;
    let (start_index, limit) = page_values(params.start_index, params.limit, &state)?;
    let query = ItemQuery {
        parent_id: Some(series_id),
        search_term: None,
        exact_name: None,
        include_item_types: vec!["Season".to_owned()],
        recursive: false,
        start_index,
        limit,
        enable_total_record_count: params.enable_total_record_count,
        sort_by: "SortName".to_owned(),
        sort_order: "Ascending".to_owned(),
        is_played: None,
        is_favorite: false,
        is_resumable: false,
    };
    Ok(Json(item_query_result(&state, &user, query).await?))
}

async fn show_episodes(
    State(state): State<AppState>,
    CurrentUser(current): CurrentUser,
    Path(series_id): Path<Uuid>,
    Query(params): Query<ShowItemsParams>,
) -> Result<Json<ItemsResultDto>, ApiError> {
    let user = selected_user(&state, &current, params.user_id).await?;
    visible_item_of_type(&state, &user, series_id, "Series").await?;
    let parent_id = if let Some(season_id) = params.season_id {
        let season = visible_item_of_type(&state, &user, season_id, "Season").await?;
        if season.parent_id != Some(series_id) {
            return Err(ApiError::NotFound);
        }
        season_id
    } else {
        series_id
    };
    let (start_index, limit) = page_values(params.start_index, params.limit, &state)?;
    let query = ItemQuery {
        parent_id: Some(parent_id),
        search_term: None,
        exact_name: None,
        include_item_types: vec!["Episode".to_owned()],
        recursive: params.season_id.is_none(),
        start_index,
        limit,
        enable_total_record_count: params.enable_total_record_count,
        sort_by: "SortName".to_owned(),
        sort_order: "Ascending".to_owned(),
        is_played: None,
        is_favorite: false,
        is_resumable: false,
    };
    Ok(Json(item_query_result(&state, &user, query).await?))
}

async fn visible_item_of_type(
    state: &AppState,
    user: &UserRecord,
    item_id: Uuid,
    expected_type: &str,
) -> Result<ItemRecord, ApiError> {
    let item = db::get_item(&state.db, item_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if item.item_type != expected_type || !db::item_visible_to_user(&state.db, user, &item).await? {
        return Err(ApiError::NotFound);
    }
    Ok(item)
}

#[derive(Deserialize, Default)]
struct ArtistListParams {
    #[serde(default, rename = "ParentId", alias = "parentId")]
    parent_id: Option<Uuid>,
    #[serde(default, rename = "SearchTerm", alias = "searchTerm")]
    search_term: Option<String>,
    #[serde(default, rename = "StartIndex", alias = "startIndex")]
    start_index: Option<i64>,
    #[serde(default, rename = "Limit", alias = "limit")]
    limit: Option<i64>,
    #[serde(
        default = "default_true",
        rename = "EnableTotalRecordCount",
        alias = "enableTotalRecordCount"
    )]
    enable_total_record_count: bool,
    #[serde(default, rename = "UserId", alias = "userId")]
    user_id: Option<Uuid>,
}

#[derive(Default)]
struct PersonListParams {
    parent_id: Option<Uuid>,
    search_term: Option<String>,
    person_types: Option<Vec<String>>,
    appears_in_item_id: Option<Uuid>,
    start_index: Option<i64>,
    limit: Option<i64>,
    enable_total_record_count: bool,
    user_id: Option<Uuid>,
}

#[derive(Deserialize, Default)]
struct PersonByNameParams {
    #[serde(default, rename = "UserId", alias = "userId")]
    user_id: Option<Uuid>,
}

async fn list_music_persons(
    State(state): State<AppState>,
    CurrentUser(current): CurrentUser,
    RawQuery(raw_query): RawQuery,
) -> Result<Json<ItemsResultDto>, ApiError> {
    let params = parse_person_list_params(raw_query.as_deref())?;
    let user = selected_user(&state, &current, params.user_id).await?;
    ensure_parent_visible(&state, &user, params.parent_id).await?;
    let (start_index, limit) = page_values(params.start_index, params.limit, &state)?;
    let search_term = normalized_search_term(params.search_term)?;
    let include_music_people = requested_music_person_type(params.person_types.as_deref())?;

    if let Some(appears_in_item_id) = params.appears_in_item_id {
        if params.parent_id.is_some() {
            return Err(ApiError::BadRequest(
                "ParentId cannot be combined with AppearsInItemId".to_owned(),
            ));
        }
        let source = db::get_item(&state.db, appears_in_item_id)
            .await?
            .ok_or(ApiError::NotFound)?;
        if !db::item_visible_to_user(&state.db, &user, &source).await? {
            return Err(ApiError::NotFound);
        }
        let artist_id = if source.item_type == "MusicArtist" {
            Some(source.id)
        } else if matches!(source.item_type.as_str(), "Audio" | "MusicAlbum") {
            let linked_artist = db::item_navigation_links(&state.db, &[source.id])
                .await?
                .get(&source.id)
                .and_then(|links| links.artist_id);
            if linked_artist.is_some() || source.item_type != "Audio" {
                linked_artist
            } else if let Some(parent_id) = source.parent_id {
                db::get_item(&state.db, parent_id)
                    .await?
                    .filter(|parent| parent.item_type == "MusicArtist")
                    .map(|parent| parent.id)
            } else {
                None
            }
        } else {
            None
        };
        if !include_music_people {
            return Ok(Json(empty_items_result(
                start_index,
                params.enable_total_record_count,
            )));
        }
        let Some(artist_id) = artist_id else {
            return Ok(Json(empty_items_result(
                start_index,
                params.enable_total_record_count,
            )));
        };
        let Some(artist) = db::get_item(&state.db, artist_id).await? else {
            return Ok(Json(empty_items_result(
                start_index,
                params.enable_total_record_count,
            )));
        };
        let matches_search = search_term
            .as_deref()
            .is_none_or(|term| artist.name.to_lowercase().contains(&term.to_lowercase()));
        if artist.item_type != "MusicArtist"
            || !matches_search
            || !db::item_visible_to_user(&state.db, &user, &artist).await?
        {
            return Ok(Json(empty_items_result(
                start_index,
                params.enable_total_record_count,
            )));
        }
        let mut dto = item_dto_for_user(&state, &user, &artist).await?;
        set_music_person_kind(&mut dto);
        return Ok(Json(single_item_page(
            dto,
            start_index,
            params.enable_total_record_count,
        )));
    }

    if !include_music_people {
        return Ok(Json(empty_items_result(
            start_index,
            params.enable_total_record_count,
        )));
    }
    let query = ItemQuery {
        parent_id: params.parent_id,
        search_term,
        exact_name: None,
        include_item_types: vec!["MusicArtist".to_owned()],
        recursive: true,
        start_index,
        limit,
        enable_total_record_count: params.enable_total_record_count,
        sort_by: "SortName".to_owned(),
        sort_order: "Ascending".to_owned(),
        is_played: None,
        is_favorite: false,
        is_resumable: false,
    };
    let mut result = item_query_result(&state, &user, query).await?;
    for item in &mut result.items {
        set_music_person_kind(item);
    }
    Ok(Json(result))
}

fn parse_person_list_params(raw_query: Option<&str>) -> Result<PersonListParams, ApiError> {
    const MAX_QUERY_BYTES: usize = 16 * 1024;
    const MAX_QUERY_PAIRS: usize = 64;

    let raw_query = raw_query.unwrap_or_default();
    if raw_query.len() > MAX_QUERY_BYTES {
        return Err(ApiError::BadRequest("Person query is too large".to_owned()));
    }

    let mut params = PersonListParams {
        enable_total_record_count: true,
        ..PersonListParams::default()
    };
    let mut person_types = Vec::new();
    let mut seen = HashSet::new();
    let mut pair_count = 0;
    for (key, value) in url::form_urlencoded::parse(raw_query.as_bytes()) {
        pair_count += 1;
        if pair_count > MAX_QUERY_PAIRS {
            return Err(ApiError::BadRequest(
                "Person query has too many parameters".to_owned(),
            ));
        }
        let value = value.as_ref();
        match key.as_ref() {
            "PersonTypes" | "personTypes" => person_types.push(value.to_owned()),
            "ParentId" | "parentId" => {
                ensure_person_scalar_once(&mut seen, "ParentId")?;
                params.parent_id = Some(parse_person_query_value("ParentId", value)?);
            }
            "SearchTerm" | "searchTerm" => {
                ensure_person_scalar_once(&mut seen, "SearchTerm")?;
                params.search_term = Some(value.to_owned());
            }
            "AppearsInItemId" | "appearsInItemId" => {
                ensure_person_scalar_once(&mut seen, "AppearsInItemId")?;
                params.appears_in_item_id =
                    Some(parse_person_query_value("AppearsInItemId", value)?);
            }
            "StartIndex" | "startIndex" => {
                ensure_person_scalar_once(&mut seen, "StartIndex")?;
                params.start_index = Some(parse_person_query_value("StartIndex", value)?);
            }
            "Limit" | "limit" => {
                ensure_person_scalar_once(&mut seen, "Limit")?;
                params.limit = Some(parse_person_query_value("Limit", value)?);
            }
            "EnableTotalRecordCount" | "enableTotalRecordCount" => {
                ensure_person_scalar_once(&mut seen, "EnableTotalRecordCount")?;
                params.enable_total_record_count =
                    parse_person_query_value("EnableTotalRecordCount", value)?;
            }
            "UserId" | "userId" => {
                ensure_person_scalar_once(&mut seen, "UserId")?;
                params.user_id = Some(parse_person_query_value("UserId", value)?);
            }
            _ => {}
        }
    }
    if !person_types.is_empty() {
        params.person_types = Some(person_types);
    }
    Ok(params)
}

fn ensure_person_scalar_once(
    seen: &mut HashSet<&'static str>,
    key: &'static str,
) -> Result<(), ApiError> {
    if seen.insert(key) {
        Ok(())
    } else {
        Err(ApiError::BadRequest(format!(
            "Person query parameter {key} may only be specified once"
        )))
    }
}

fn parse_person_query_value<T: std::str::FromStr>(key: &str, value: &str) -> Result<T, ApiError> {
    value
        .parse()
        .map_err(|_| ApiError::BadRequest(format!("Invalid {key} query parameter")))
}

async fn get_music_person(
    State(state): State<AppState>,
    CurrentUser(current): CurrentUser,
    Path(name): Path<String>,
    Query(params): Query<PersonByNameParams>,
) -> Result<Json<BaseItemDto>, ApiError> {
    let user = selected_user(&state, &current, params.user_id).await?;
    let name = normalized_search_term(Some(name))?
        .ok_or_else(|| ApiError::BadRequest("Person name is required".to_owned()))?;
    // Exact-name matching runs in the catalog query so policy filtering happens
    // in SQL and the result does not depend on a capped substring-search page.
    let query = ItemQuery {
        exact_name: Some(name),
        include_item_types: vec!["MusicArtist".to_owned()],
        recursive: true,
        start_index: 0,
        limit: 1,
        enable_total_record_count: false,
        sort_by: "SortName".to_owned(),
        sort_order: "Ascending".to_owned(),
        ..Default::default()
    };
    let (artists, _) = db::browse_items(&state.db, &user, query).await?;
    let artist = artists.into_iter().next().ok_or(ApiError::NotFound)?;
    let mut dto = item_dto_for_user(&state, &user, &artist).await?;
    set_music_person_kind(&mut dto);
    Ok(Json(dto))
}

fn requested_music_person_type(values: Option<&[String]>) -> Result<bool, ApiError> {
    // This catalog slice stores MusicArtist nodes, not the general person-to-item
    // credit graph. Recognized non-music kinds therefore filter to no results.
    let Some(values) = values else {
        return Ok(true);
    };
    let types = values
        .iter()
        .flat_map(|value| value.split(','))
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .collect::<Vec<_>>();
    if types.is_empty() {
        return Ok(true);
    }
    if types.len() > 32 || types.iter().any(|value| value.len() > 64) {
        return Err(ApiError::BadRequest(
            "PersonTypes contains too many or oversized values".to_owned(),
        ));
    }
    let mut artist = false;
    let mut album_artist = false;
    for person_type in types {
        match person_type.to_ascii_lowercase().as_str() {
            "artist" => artist = true,
            "albumartist" => album_artist = true,
            "actor" | "author" | "arranger" | "colorist" | "composer" | "conductor"
            | "coverartist" | "creator" | "director" | "editor" | "engineer" | "gueststar"
            | "illustrator" | "inker" | "letterer" | "lyricist" | "mixer" | "narrator"
            | "penciller" | "producer" | "remixer" | "translator" | "unknown" | "writer" => {}
            _ => {
                return Err(ApiError::BadRequest(format!(
                    "Unsupported PersonType: {person_type}"
                )));
            }
        }
    }
    Ok(artist || album_artist)
}

fn set_music_person_kind(item: &mut BaseItemDto) {
    item.item_type = "Person".to_owned();
    item.is_folder = false;
    item.media_type = None;
    item.artist_items = None;
}

fn empty_items_result(start_index: i64, include_total: bool) -> ItemsResultDto {
    ItemsResultDto {
        items: Vec::new(),
        total_record_count: include_total.then_some(0),
        start_index,
    }
}

fn single_item_page(item: BaseItemDto, start_index: i64, include_total: bool) -> ItemsResultDto {
    ItemsResultDto {
        items: if start_index == 0 {
            vec![item]
        } else {
            Vec::new()
        },
        total_record_count: include_total.then_some(1),
        start_index,
    }
}

async fn list_music_artists(
    State(state): State<AppState>,
    CurrentUser(current): CurrentUser,
    Query(params): Query<ArtistListParams>,
) -> Result<Json<ItemsResultDto>, ApiError> {
    let user = selected_user(&state, &current, params.user_id).await?;
    ensure_parent_visible(&state, &user, params.parent_id).await?;
    let (start_index, limit) = page_values(params.start_index, params.limit, &state)?;
    let query = ItemQuery {
        parent_id: params.parent_id,
        search_term: normalized_search_term(params.search_term)?,
        exact_name: None,
        include_item_types: vec!["MusicArtist".to_owned()],
        recursive: true,
        start_index,
        limit,
        enable_total_record_count: params.enable_total_record_count,
        sort_by: "SortName".to_owned(),
        sort_order: "Ascending".to_owned(),
        is_played: None,
        is_favorite: false,
        is_resumable: false,
    };
    Ok(Json(item_query_result(&state, &user, query).await?))
}

fn page_values(
    start_index: Option<i64>,
    limit: Option<i64>,
    state: &AppState,
) -> Result<(i64, i64), ApiError> {
    let start_index = start_index.unwrap_or(0);
    if start_index < 0 {
        return Err(ApiError::BadRequest(
            "StartIndex cannot be negative".to_owned(),
        ));
    }
    Ok((
        start_index,
        limit
            .unwrap_or(100)
            .clamp(1, state.config.max_page_size.min(MAX_CATALOG_PAGE_SIZE)),
    ))
}

fn normalized_search_term(value: Option<String>) -> Result<Option<String>, ApiError> {
    match value {
        Some(value) if value.len() > 200 => Err(ApiError::BadRequest(
            "SearchTerm exceeds the 200-character limit".to_owned(),
        )),
        Some(value) => {
            let value = value.trim().to_owned();
            Ok((!value.is_empty()).then_some(value))
        }
        None => Ok(None),
    }
}

fn parse_item_types(value: Option<&str>) -> Result<Vec<String>, ApiError> {
    let Some(value) = value else {
        return Ok(Vec::new());
    };
    let items = value
        .split(',')
        .map(str::trim)
        .filter(|item| !item.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    if items.len() > 32 || items.iter().any(|item| item.len() > 64) {
        return Err(ApiError::BadRequest(
            "IncludeItemTypes contains too many or oversized values".to_owned(),
        ));
    }
    Ok(items)
}

async fn get_item(
    State(state): State<AppState>,
    CurrentUser(current): CurrentUser,
    Path(item_id): Path<Uuid>,
    Query(params): Query<UserViewsParams>,
) -> Result<Response, ApiError> {
    let user = selected_user(&state, &current, params.user_id).await?;
    catalog_item_response(&state, &user, item_id).await
}

async fn get_user_item(
    State(state): State<AppState>,
    CurrentUser(current): CurrentUser,
    Path((user_id, item_id)): Path<(Uuid, Uuid)>,
    Query(params): Query<UserViewsParams>,
) -> Result<Response, ApiError> {
    if params.user_id.is_some_and(|requested| requested != user_id) {
        return Err(ApiError::BadRequest(
            "Conflicting user identifiers".to_owned(),
        ));
    }
    let user = selected_user(&state, &current, Some(user_id)).await?;
    catalog_item_response(&state, &user, item_id).await
}

async fn catalog_item_response(
    state: &AppState,
    user: &UserRecord,
    item_id: Uuid,
) -> Result<Response, ApiError> {
    if let Some(library) = db::get_library(&state.db, item_id).await? {
        if !db::library_visible_to_user(&state.db, user, library.id).await? {
            return Err(ApiError::NotFound);
        }
        return Ok(Json(library_view_dto(&library, state.server_id)).into_response());
    }
    let item = db::get_item(&state.db, item_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !db::item_visible_to_user(&state.db, user, &item).await? {
        return Err(ApiError::NotFound);
    }
    Ok(Json(item_dto_for_user(state, user, &item).await?).into_response())
}

pub(crate) async fn item_dto_for_user(
    state: &AppState,
    user: &UserRecord,
    item: &ItemRecord,
) -> Result<BaseItemDto, ApiError> {
    let item_id = item.id;
    let data = db::item_user_data(&state.db, user.id, &[item_id])
        .await?
        .remove(&item_id)
        .map(|data| user_data_dto(&data, item_id, item.runtime_ticks));
    let navigation = db::item_navigation_links(&state.db, &[item_id]).await?;
    let display_metadata = crate::metadata::load_display_metadata(&state.db, &[item_id]).await?;
    Ok(item_dto(
        item,
        state.server_id,
        Some(data.unwrap_or_else(|| empty_user_data(item_id))),
        navigation.get(&item_id),
        display_metadata.get(&item_id),
        user.is_admin,
    ))
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct ItemCountsDto {
    movie_count: i64,
    series_count: i64,
    episode_count: i64,
    artist_count: i64,
    album_count: i64,
    song_count: i64,
    book_count: i64,
}

async fn item_counts(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> Result<Json<ItemCountsDto>, ApiError> {
    let counts = db::item_counts(&state.db, &user).await?;
    Ok(Json(ItemCountsDto {
        movie_count: counts.movie_count,
        series_count: counts.series_count,
        episode_count: counts.episode_count,
        artist_count: counts.artist_count,
        album_count: counts.album_count,
        song_count: counts.song_count,
        book_count: counts.book_count,
    }))
}

#[derive(Deserialize, Default)]
struct SearchQuery {
    #[serde(default, rename = "SearchTerm", alias = "searchTerm")]
    search_term: Option<String>,
    #[serde(default, rename = "StartIndex", alias = "startIndex")]
    start_index: Option<i64>,
    #[serde(default, rename = "Limit", alias = "limit")]
    limit: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct SearchHintsResultDto {
    search_hints: Vec<BaseItemDto>,
    total_record_count: i64,
}

async fn search_hints(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(params): Query<SearchQuery>,
) -> Result<Json<SearchHintsResultDto>, ApiError> {
    let search = params
        .search_term
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .ok_or_else(|| ApiError::BadRequest("SearchTerm is required".to_owned()))?;
    if search.len() > 200 {
        return Err(ApiError::BadRequest(
            "SearchTerm exceeds the 200-character limit".to_owned(),
        ));
    }
    let start = params.start_index.unwrap_or(0);
    if start < 0 {
        return Err(ApiError::BadRequest(
            "StartIndex cannot be negative".to_owned(),
        ));
    }
    let limit = params
        .limit
        .unwrap_or(20)
        .clamp(1, state.config.max_page_size.min(MAX_CATALOG_PAGE_SIZE));
    let query = ItemQuery {
        parent_id: None,
        search_term: Some(search.to_owned()),
        exact_name: None,
        include_item_types: Vec::new(),
        recursive: true,
        start_index: start,
        limit,
        enable_total_record_count: true,
        sort_by: "SortName".to_owned(),
        sort_order: "Ascending".to_owned(),
        is_played: None,
        is_favorite: false,
        is_resumable: false,
    };
    let (items, total) = db::browse_items(&state.db, &user, query).await?;
    let item_ids = items.iter().map(|item| item.id).collect::<Vec<_>>();
    let navigation = db::item_navigation_links(&state.db, &item_ids).await?;
    let display_metadata = crate::metadata::load_display_metadata(&state.db, &item_ids).await?;
    let search_hints = items
        .iter()
        .map(|item| {
            item_dto(
                item,
                state.server_id,
                None,
                navigation.get(&item.id),
                display_metadata.get(&item.id),
                user.is_admin,
            )
        })
        .collect();
    Ok(Json(SearchHintsResultDto {
        search_hints,
        total_record_count: total.unwrap_or(0),
    }))
}

#[derive(Deserialize, Default)]
struct UserDataRequest {
    #[serde(default, rename = "Played", alias = "played")]
    played: Option<bool>,
    #[serde(default, rename = "IsFavorite", alias = "isFavorite")]
    is_favorite: Option<bool>,
    #[serde(
        default,
        rename = "PlaybackPositionTicks",
        alias = "playbackPositionTicks"
    )]
    playback_position_ticks: Option<i64>,
    #[serde(default, rename = "PlayedPercentage", alias = "playedPercentage")]
    played_percentage: Option<f64>,
    #[serde(default, rename = "PlayCount", alias = "playCount")]
    play_count: Option<i32>,
    #[serde(default, rename = "LastPlayedDate", alias = "lastPlayedDate")]
    last_played_date: Option<DateTime<Utc>>,
    #[serde(default, rename = "Likes", alias = "likes")]
    likes: Option<bool>,
    #[serde(default, rename = "Rating", alias = "rating")]
    rating: Option<f32>,
    #[serde(default, rename = "Key", alias = "key")]
    key: Option<String>,
    #[serde(default, rename = "ItemId", alias = "itemId")]
    item_id: Option<Uuid>,
}

#[derive(Deserialize, Default)]
struct UserDataQuery {
    #[serde(default, rename = "UserId", alias = "userId")]
    user_id: Option<Uuid>,
    #[serde(default, rename = "DatePlayed", alias = "datePlayed")]
    date_played: Option<DateTime<Utc>>,
}

async fn get_user_item_data(
    State(state): State<AppState>,
    CurrentUser(current): CurrentUser,
    Path(item_id): Path<Uuid>,
    Query(params): Query<UserDataQuery>,
) -> Result<Json<UserDataDto>, ApiError> {
    let user = selected_user(&state, &current, params.user_id).await?;
    let item = db::get_item(&state.db, item_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !db::item_visible_to_user(&state.db, &user, &item).await? {
        return Err(ApiError::NotFound);
    }
    let data = db::item_user_data(&state.db, user.id, &[item_id])
        .await?
        .remove(&item_id);
    Ok(Json(
        data.as_ref()
            .map(|data| user_data_dto(data, item_id, item.runtime_ticks))
            .unwrap_or_else(|| empty_user_data(item_id)),
    ))
}

async fn update_user_item_data(
    State(state): State<AppState>,
    CurrentUser(current): CurrentUser,
    Path(item_id): Path<Uuid>,
    Query(params): Query<UserDataQuery>,
    Json(body): Json<UserDataRequest>,
) -> Result<Json<UserDataDto>, ApiError> {
    let user = selected_user(&state, &current, params.user_id).await?;
    let item = db::get_item(&state.db, item_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !db::item_visible_to_user(&state.db, &user, &item).await? {
        return Err(ApiError::NotFound);
    }
    if body
        .playback_position_ticks
        .is_some_and(|ticks| !(0..=3_155_760_000_000_000).contains(&ticks))
        || body.played_percentage.is_some_and(|percentage| {
            !percentage.is_finite() || !(0.0..=100.0).contains(&percentage)
        })
    {
        return Err(ApiError::BadRequest(
            "Playback position or played percentage is outside the supported range".to_owned(),
        ));
    }
    if body.item_id.is_some_and(|requested| requested != item_id) {
        return Err(ApiError::BadRequest(
            "ItemId in user data does not match the route item".to_owned(),
        ));
    }
    let _server_calculated_fields = (
        body.play_count,
        body.last_played_date,
        body.likes,
        body.rating,
        body.key,
    );
    if body.playback_position_ticks.is_none()
        && body.played_percentage.is_some()
        && !item.runtime_ticks.is_some_and(|duration| duration > 0)
    {
        return Err(ApiError::BadRequest(
            "PlayedPercentage requires a known item runtime".to_owned(),
        ));
    }
    let position_ticks = body.playback_position_ticks.or_else(|| {
        body.played_percentage
            .zip(item.runtime_ticks.filter(|duration| *duration > 0))
            .map(|(percentage, duration)| ((duration as f64 * percentage / 100.0).round()) as i64)
    });
    db::upsert_user_item_data(
        &state.db,
        state.run_id,
        user.id,
        item_id,
        body.played,
        body.is_favorite,
        position_ticks,
    )
    .await?;
    let data = db::item_user_data(&state.db, user.id, &[item_id])
        .await?
        .remove(&item_id);
    Ok(Json(
        data.as_ref()
            .map(|data| user_data_dto(data, item_id, item.runtime_ticks))
            .unwrap_or_else(|| empty_user_data(item_id)),
    ))
}

async fn mark_item_played(
    State(state): State<AppState>,
    CurrentUser(current): CurrentUser,
    Path(item_id): Path<Uuid>,
    Query(params): Query<UserDataQuery>,
) -> Result<Json<UserDataDto>, ApiError> {
    update_item_played(&state, &current, item_id, params, true).await
}

async fn mark_item_unplayed(
    State(state): State<AppState>,
    CurrentUser(current): CurrentUser,
    Path(item_id): Path<Uuid>,
    Query(params): Query<UserDataQuery>,
) -> Result<Json<UserDataDto>, ApiError> {
    update_item_played(&state, &current, item_id, params, false).await
}

async fn update_item_played(
    state: &AppState,
    current: &UserRecord,
    item_id: Uuid,
    params: UserDataQuery,
    played: bool,
) -> Result<Json<UserDataDto>, ApiError> {
    let user = selected_user(state, current, params.user_id).await?;
    let item = db::get_item(&state.db, item_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !db::item_visible_to_user(&state.db, &user, &item).await? {
        return Err(ApiError::NotFound);
    }
    db::set_item_played(
        &state.db,
        state.run_id,
        user.id,
        item_id,
        played,
        params.date_played,
    )
    .await?;
    Ok(Json(
        user_item_data_dto(state, user.id, item_id, item.runtime_ticks).await?,
    ))
}

async fn mark_item_favorite(
    State(state): State<AppState>,
    CurrentUser(current): CurrentUser,
    Path(item_id): Path<Uuid>,
    Query(params): Query<UserDataQuery>,
) -> Result<Json<UserDataDto>, ApiError> {
    update_item_favorite(&state, &current, item_id, params, true).await
}

async fn unmark_item_favorite(
    State(state): State<AppState>,
    CurrentUser(current): CurrentUser,
    Path(item_id): Path<Uuid>,
    Query(params): Query<UserDataQuery>,
) -> Result<Json<UserDataDto>, ApiError> {
    update_item_favorite(&state, &current, item_id, params, false).await
}

async fn update_item_favorite(
    state: &AppState,
    current: &UserRecord,
    item_id: Uuid,
    params: UserDataQuery,
    favorite: bool,
) -> Result<Json<UserDataDto>, ApiError> {
    let user = selected_user(state, current, params.user_id).await?;
    let item = db::get_item(&state.db, item_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !db::item_visible_to_user(&state.db, &user, &item).await? {
        return Err(ApiError::NotFound);
    }
    db::set_item_favorite(&state.db, state.run_id, user.id, item_id, favorite).await?;
    Ok(Json(
        user_item_data_dto(state, user.id, item_id, item.runtime_ticks).await?,
    ))
}

async fn user_item_data_dto(
    state: &AppState,
    user_id: Uuid,
    item_id: Uuid,
    runtime_ticks: Option<i64>,
) -> Result<UserDataDto, ApiError> {
    let data = db::item_user_data(&state.db, user_id, &[item_id])
        .await?
        .remove(&item_id);
    Ok(data
        .as_ref()
        .map(|data| user_data_dto(data, item_id, runtime_ticks))
        .unwrap_or_else(|| empty_user_data(item_id)))
}

#[cfg(test)]
mod item_dto_tests {
    use super::*;

    #[test]
    fn person_kind_filters_accept_csv_and_repeated_query_values() {
        let csv = vec!["Actor,Artist".to_owned()];
        let repeated = vec!["Actor".to_owned(), "AlbumArtist".to_owned()];
        let non_music = vec!["Actor".to_owned(), "Author".to_owned()];

        assert!(requested_music_person_type(Some(&csv)).unwrap());
        assert!(requested_music_person_type(Some(&repeated)).unwrap());
        assert!(!requested_music_person_type(Some(&non_music)).unwrap());
        assert!(requested_music_person_type(None).unwrap());
    }

    #[test]
    fn person_query_parser_accepts_csv_and_repeated_person_types() {
        let csv = parse_person_list_params(Some(
            "ParentId=00000000-0000-0000-0000-000000000001&PersonTypes=Actor%2CArtist&Limit=10",
        ))
        .unwrap();
        assert_eq!(csv.limit, Some(10));
        assert!(requested_music_person_type(csv.person_types.as_deref()).unwrap());

        let repeated = parse_person_list_params(Some(
            "PersonTypes=Actor&PersonTypes=AlbumArtist&enableTotalRecordCount=false",
        ))
        .unwrap();
        assert_eq!(repeated.person_types.as_deref().unwrap().len(), 2);
        assert!(!repeated.enable_total_record_count);
        assert!(requested_music_person_type(repeated.person_types.as_deref()).unwrap());
    }

    #[test]
    fn person_query_parser_rejects_duplicate_scalar_parameters() {
        assert!(parse_person_list_params(Some("Limit=1&limit=2")).is_err());
    }

    #[test]
    fn person_items_use_the_standard_person_base_item_kind() {
        let item = ItemRecord {
            id: Uuid::new_v4(),
            library_id: Uuid::new_v4(),
            parent_id: None,
            name: "Example Artist".to_owned(),
            sort_name: "example artist".to_owned(),
            item_type: "MusicArtist".to_owned(),
            path: PathBuf::from("/music/Example Artist"),
            container: None,
            size_bytes: None,
            runtime_ticks: None,
            date_added: Utc::now(),
            date_modified: None,
            rating: None,
            overview: None,
            metadata_json: serde_json::Value::Null,
        };
        let mut dto = item_dto(&item, Uuid::new_v4(), None, None, None, false);
        set_music_person_kind(&mut dto);
        let wire = serde_json::to_value(dto).unwrap();

        assert_eq!(wire["Type"], "Person");
        assert!(wire.get("PersonType").is_none());
    }

    #[test]
    fn appeared_person_paging_keeps_total_and_applies_start_index() {
        let item = BaseItemDto {
            id: Uuid::new_v4(),
            server_id: Uuid::new_v4(),
            name: "Example Artist".to_owned(),
            item_type: "Person".to_owned(),
            is_folder: false,
            media_type: None,
            parent_id: None,
            container: None,
            run_time_ticks: None,
            date_created: Utc::now(),
            date_modified: None,
            overview: None,
            genres: Vec::new(),
            official_rating: None,
            community_rating: None,
            premiere_date: None,
            image_tags: None,
            series_id: None,
            season_id: None,
            index_number: None,
            parent_index_number: None,
            album: None,
            album_id: None,
            artist_items: None,
            path: None,
            user_data: None,
        };

        let result = single_item_page(item, 1, true);
        assert!(result.items.is_empty());
        assert_eq!(result.total_record_count, Some(1));
        assert_eq!(result.start_index, 1);
    }

    #[test]
    fn serializes_navigation_links_with_public_field_names() {
        let item_id = Uuid::new_v4();
        let series_id = Uuid::new_v4();
        let season_id = Uuid::new_v4();
        let artist_id = Uuid::new_v4();
        let item = ItemRecord {
            id: item_id,
            library_id: Uuid::new_v4(),
            parent_id: Some(season_id),
            name: "S01E02 - Pilot.mkv".to_owned(),
            sort_name: "s01e02 - pilot.mkv".to_owned(),
            item_type: "Episode".to_owned(),
            path: PathBuf::from("/library/Series/Season 1/S01E02 - Pilot.mkv"),
            container: Some("mkv".to_owned()),
            size_bytes: Some(10),
            runtime_ticks: Some(1_000),
            date_added: Utc::now(),
            date_modified: None,
            rating: None,
            overview: None,
            metadata_json: serde_json::Value::Null,
        };
        let navigation = db::ItemNavigationLinks {
            series_id: Some(series_id),
            season_id: Some(season_id),
            index_number: Some(2),
            parent_index_number: Some(1),
            artist_id: Some(artist_id),
            artist: Some("Example Artist".to_owned()),
            ..Default::default()
        };
        let dto = serde_json::to_value(item_dto(
            &item,
            Uuid::new_v4(),
            None,
            Some(&navigation),
            None,
            false,
        ))
        .unwrap();
        assert_eq!(dto["Type"], "Episode");
        assert_eq!(dto["SeriesId"], series_id.to_string());
        assert_eq!(dto["SeasonId"], season_id.to_string());
        assert_eq!(dto["IndexNumber"], 2);
        assert_eq!(dto["ParentIndexNumber"], 1);
        assert_eq!(dto["ArtistItems"][0]["Name"], "Example Artist");
        assert_eq!(dto["ArtistItems"][0]["Id"], artist_id.to_string());
        assert_eq!(dto["ArtistItems"][0]["Type"], "Artist");
        assert!(dto.get("Path").is_none());
    }

    #[test]
    fn item_dto_exposes_provider_metadata_and_primary_image_tag() {
        let item = ItemRecord {
            id: Uuid::new_v4(),
            library_id: Uuid::new_v4(),
            parent_id: None,
            name: "scanner-title.mkv".to_owned(),
            sort_name: "scanner-title.mkv".to_owned(),
            item_type: "Movie".to_owned(),
            path: PathBuf::from("/library/scanner-title.mkv"),
            container: Some("mkv".to_owned()),
            size_bytes: Some(10),
            runtime_ticks: Some(10_000),
            date_added: Utc::now(),
            date_modified: None,
            rating: None,
            overview: Some("Scanner description".to_owned()),
            metadata_json: serde_json::Value::Null,
        };
        let metadata = crate::metadata::DisplayMetadata {
            name: Some("Provider title".to_owned()),
            overview: Some("Provider description".to_owned()),
            premiere_date: Some(chrono::NaiveDate::from_ymd_opt(2022, 3, 4).unwrap()),
            genres: vec!["Drama".to_owned()],
            official_rating: Some("PG-13".to_owned()),
            community_score: Some(8.2),
            artwork_url: Some(format!("/Items/{}/Images/Primary", item.id)),
            primary_image_tag: Some("a".repeat(64)),
        };
        let server_id = Uuid::new_v4();
        let dto = serde_json::to_value(item_dto(
            &item,
            server_id,
            None,
            None,
            Some(&metadata),
            false,
        ))
        .unwrap();
        assert_eq!(dto["ServerId"], server_id.to_string());
        assert_eq!(dto["Name"], "Provider title");
        assert_eq!(dto["Overview"], "Provider description");
        assert_eq!(dto["Genres"][0], "Drama");
        assert_eq!(dto["OfficialRating"], "PG-13");
        assert_eq!(dto["CommunityRating"], 8.2);
        assert_eq!(dto["PremiereDate"], "2022-03-04T00:00:00Z");
        assert_eq!(dto["ImageTags"]["Primary"], "a".repeat(64));
    }

    #[tokio::test]
    async fn parental_rating_catalog_matches_the_versioned_policy_scale() {
        let ratings = serde_json::to_value(parental_ratings().await.0).unwrap();
        assert_eq!(
            ratings
                .as_array()
                .unwrap()
                .iter()
                .map(|rating| rating["Value"].as_i64().unwrap())
                .collect::<Vec<_>>(),
            [0, 25, 50, 75, 100]
        );
        assert_eq!(ratings[2]["Name"], "US-MPAA-v1: PG-13");
        assert_eq!(ratings[2]["RatingScore"]["Score"], 50);
        assert!(ratings[2]["RatingScore"]["SubScore"].is_null());
    }
}

#[cfg(test)]
mod user_write_request_tests {
    use super::{UserWriteRequest, login_identity, login_identity_with_limit};

    #[test]
    fn long_device_identity_requires_matching_header_and_body() {
        let device_id = "opaque-browser-id-".repeat(24);
        assert_eq!(
            login_identity_with_limit(
                Some(&device_id),
                Some(&device_id),
                crate::auth::MAX_DEVICE_ID_BYTES
            )
            .unwrap(),
            Some(device_id.clone()),
        );
        assert!(login_identity(Some(&device_id), None).is_err());
        assert!(
            login_identity_with_limit(
                Some(&device_id),
                Some("different-device"),
                crate::auth::MAX_DEVICE_ID_BYTES
            )
            .is_err()
        );
        assert!(
            login_identity_with_limit(None, Some("device\nname"), crate::auth::MAX_DEVICE_ID_BYTES)
                .is_err()
        );
    }

    #[test]
    fn new_users_need_explicit_remote_access_opt_in() {
        let omitted: UserWriteRequest = serde_json::from_str("{}").unwrap();
        let explicit_true: UserWriteRequest =
            serde_json::from_str(r#"{"EnableRemoteAccess":true}"#).unwrap();
        let explicit_false: UserWriteRequest =
            serde_json::from_str(r#"{"enableRemoteAccess":false}"#).unwrap();

        assert!(!omitted.remote_access_for_new_user());
        assert!(explicit_true.remote_access_for_new_user());
        assert!(!explicit_false.remote_access_for_new_user());
    }
}
