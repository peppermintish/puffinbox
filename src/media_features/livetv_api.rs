//! Authenticated Live TV catalog, guide, source, timer, and recording routes.
//!
//! Source URLs and pin sets are administrator-owned. Feed data is fetched in
//! Rust with redirects and proxies disabled; untrusted source text is never
//! passed to an external process or returned as a catalog path.

use std::collections::HashMap;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use chrono::{DateTime, Duration, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, QueryBuilder, Row, error::DatabaseError, types::Json as SqlJson};
use uuid::Uuid;

use crate::{
    ApiError,
    auth::{AdminUser, CurrentUser, UserRecord},
    db,
    state::AppState,
};

use super::livetv::{
    FeedError, M3uChannel, OriginPin, XmltvProgram, approved_urls, fetch_bounded_feed, parse_m3u,
    parse_xmltv,
};

const MAX_LIST_LIMIT: i64 = 500;
const MAX_GUIDE_WINDOW: Duration = Duration::days(31);
const MAX_TIMER_DURATION: Duration = Duration::hours(4);
const MAX_SERIES_TIMERS_PER_USER: i64 = 64;
const MAX_SERIES_TIMER_PAGE_SIZE: i64 = 64;
const MAX_SERIES_TIMER_START_INDEX: i64 = 1_000_000;
const MAX_QUEUED_SERIES_AIRINGS: i64 = 250;
const MAX_SERIES_MATERIALIZE_BATCH: i64 = 5_000;

pub(super) fn router(state: AppState) -> Router {
    Router::new()
        .route("/LiveTv/Channels", get(list_channels))
        .route("/LiveTv/Programs", get(list_programs))
        .route("/LiveTv/Timers/Defaults", get(timer_defaults))
        .route("/LiveTv/Timers", get(list_timers).post(create_timer))
        .route(
            "/LiveTv/Timers/{timer_id}",
            get(get_timer).post(update_timer).delete(cancel_timer),
        )
        .route(
            "/LiveTv/SeriesTimers",
            get(list_series_timers).post(create_series_timer),
        )
        .route(
            "/LiveTv/SeriesTimers/{timer_id}",
            get(get_series_timer)
                .post(update_series_timer)
                .delete(cancel_series_timer),
        )
        .route("/LiveTv/Recordings", get(list_recordings))
        .route("/LiveTv/TunerHosts", get(list_tuner_hosts))
        .route(
            "/Admin/LiveTv/Sources",
            get(list_sources).post(create_source),
        )
        .route(
            "/Admin/LiveTv/Sources/{source_id}",
            post(update_source).delete(delete_source),
        )
        .route(
            "/Admin/LiveTv/Sources/{source_id}/Refresh",
            post(refresh_source),
        )
        .with_state(state)
}

pub(super) fn require_live_tv_access(user: &UserRecord) -> Result<(), ApiError> {
    if user.is_admin || user.enable_live_tv_access {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
}

fn require_live_tv_management(user: &UserRecord) -> Result<(), ApiError> {
    if user.is_admin || (user.enable_live_tv_access && user.enable_live_tv_management) {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
}

async fn lock_live_tv_mutation_user(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    run_id: Uuid,
    request_user_id: Uuid,
    require_management: bool,
) -> Result<UserRecord, ApiError> {
    db::require_active_run(tx, run_id).await?;
    let row = sqlx::query(
        "SELECT u.id,u.username,u.is_admin,u.disabled,u.enable_remote_access, \
         u.allow_media_playback,u.sync_play_access,u.enable_content_downloading,u.enable_live_tv_access, \
         u.enable_live_tv_management,u.restrict_libraries,u.configuration,u.max_parental_rating, \
         u.block_unrated_items,ARRAY[]::uuid[] AS allowed_library_ids \
         FROM users u WHERE u.id=$1 FOR UPDATE OF u",
    )
    .bind(request_user_id)
    .fetch_optional(&mut **tx)
    .await?
    .ok_or(ApiError::Forbidden)?;
    let mut user = db::user_from_row(&row)?;
    // Use a separate READ COMMITTED statement after acquiring the user-row
    // lock. If a policy update held that lock first, this statement observes
    // its committed library membership edits rather than the earlier
    // statement snapshot used while waiting for the lock.
    user.allowed_library_ids = sqlx::query_scalar(
        "SELECT library_id FROM user_library_access WHERE user_id=$1 ORDER BY library_id",
    )
    .bind(request_user_id)
    .fetch_all(&mut **tx)
    .await?;
    require_live_tv_access(&user)?;
    if user.disabled || !user.allow_media_playback {
        return Err(ApiError::Forbidden);
    }
    if require_management {
        require_live_tv_management(&user)?;
    }
    Ok(user)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SourceRequest {
    library_id: Uuid,
    name: String,
    playlist_url: String,
    guide_url: Option<String>,
    origin_pins: Vec<OriginPin>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SourceUpdateRequest {
    library_id: Option<Uuid>,
    name: Option<String>,
    playlist_url: Option<String>,
    // Missing preserves the existing guide; JSON null explicitly clears it.
    #[serde(default)]
    guide_url: PatchField<Option<String>>,
    origin_pins: Option<Vec<OriginPin>>,
}

#[derive(Debug, Default)]
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

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct SourceDto {
    id: Uuid,
    library_id: Uuid,
    name: String,
    enabled: bool,
    refresh_status: String,
    last_refreshed_at: Option<DateTime<Utc>>,
    last_error_code: Option<String>,
}

#[derive(Clone)]
struct SourceRecord {
    id: Uuid,
    library_id: Uuid,
    playlist_url: String,
    guide_url: Option<String>,
    pins: Vec<OriginPin>,
}

async fn list_sources(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
) -> Result<Json<Vec<SourceDto>>, ApiError> {
    require_live_tv_management(&admin)?;
    let rows = sqlx::query(
        "SELECT id,library_id,name,enabled,refresh_status,last_refreshed_at,last_error_code \
         FROM live_tv_sources WHERE deleted_at IS NULL ORDER BY name,id LIMIT 1000",
    )
    .fetch_all(&state.db)
    .await?;
    let mut values = Vec::with_capacity(rows.len());
    for row in rows {
        values.push(SourceDto {
            id: row.try_get("id")?,
            library_id: row.try_get("library_id")?,
            name: row.try_get("name")?,
            enabled: row.try_get("enabled")?,
            refresh_status: row.try_get("refresh_status")?,
            last_refreshed_at: row.try_get("last_refreshed_at")?,
            last_error_code: row.try_get("last_error_code")?,
        });
    }
    Ok(Json(values))
}

async fn create_source(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Json(request): Json<SourceRequest>,
) -> Result<(StatusCode, Json<SourceDto>), ApiError> {
    require_live_tv_management(&admin)?;
    validate_source_request(&request)?;
    let library = db::get_library(&state.db, request.library_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if library.locations.is_empty() {
        return Err(ApiError::BadRequest(
            "The target library has no configured storage root".to_owned(),
        ));
    }
    let id = Uuid::new_v4();
    let pins = serde_json::to_value(&request.origin_pins)
        .map_err(|_| ApiError::BadRequest("The origin pin set is invalid".to_owned()))?;
    let mut tx = state.db.begin().await?;
    db::require_active_run(&mut tx, state.run_id).await?;
    sqlx::query(
        "INSERT INTO live_tv_sources(id,library_id,name,playlist_url,guide_url,origin_pins) \
         VALUES($1,$2,$3,$4,$5,$6)",
    )
    .bind(id)
    .bind(request.library_id)
    .bind(request.name.trim())
    .bind(request.playlist_url.trim())
    .bind(request.guide_url.as_deref().map(str::trim))
    .bind(SqlJson(pins))
    .execute(&mut *tx)
    .await
    .map_err(source_name_conflict)?;
    tx.commit().await?;

    Ok((
        StatusCode::CREATED,
        Json(SourceDto {
            id,
            library_id: request.library_id,
            name: request.name.trim().to_owned(),
            enabled: true,
            refresh_status: "queued".to_owned(),
            last_refreshed_at: None,
            last_error_code: None,
        }),
    ))
}

fn validate_source_request(request: &SourceRequest) -> Result<(), ApiError> {
    validate_source_name(&request.name)?;
    if request.playlist_url.len() > 8192
        || request
            .guide_url
            .as_ref()
            .is_some_and(|url| url.len() > 8192)
    {
        return Err(ApiError::BadRequest("Source URL is too long".to_owned()));
    }
    let origins = approved_urls(&request.origin_pins).map_err(feed_error)?;
    if !url_uses_approved_origin(&request.playlist_url, &origins)
        || request
            .guide_url
            .as_deref()
            .is_some_and(|url| !url_uses_approved_origin(url, &origins))
    {
        return Err(ApiError::BadRequest(
            "Feed URLs must use an administrator-pinned HTTP(S) origin".to_owned(),
        ));
    }
    Ok(())
}

fn validate_source_name(name: &str) -> Result<(), ApiError> {
    if name.trim().is_empty() || name.trim().len() > 160 {
        return Err(ApiError::BadRequest(
            "Source name must be between 1 and 160 bytes".to_owned(),
        ));
    }
    Ok(())
}

fn source_name_conflict(error: sqlx::Error) -> ApiError {
    if error
        .as_database_error()
        .is_some_and(DatabaseError::is_unique_violation)
    {
        ApiError::Conflict(
            "An active source with this name already exists in that library".to_owned(),
        )
    } else {
        error.into()
    }
}

async fn validate_source_library(state: &AppState, library_id: Uuid) -> Result<(), ApiError> {
    let library = db::get_library(&state.db, library_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if library.locations.is_empty() {
        return Err(ApiError::BadRequest(
            "The target library has no configured storage root".to_owned(),
        ));
    }
    Ok(())
}

async fn update_source(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(source_id): Path<Uuid>,
    Json(request): Json<SourceUpdateRequest>,
) -> Result<Json<SourceDto>, ApiError> {
    require_live_tv_management(&admin)?;
    if let Some(name) = request.name.as_deref() {
        validate_source_name(name)?;
    }

    let mut tx = state.db.begin().await?;
    db::require_active_run(&mut tx, state.run_id).await?;
    // Refresh persistence locks users before it writes the source row. Use
    // that same order so source edits cannot deadlock with an in-flight refresh.
    lock_series_materialization_users(&mut tx, Some(source_id), None).await?;
    let row = sqlx::query(
        "SELECT library_id,name,playlist_url,guide_url,origin_pins,refresh_status \
         FROM live_tv_sources WHERE id=$1 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(source_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::NotFound)?;
    if row.try_get::<String, _>("refresh_status")? == "running" {
        return Err(ApiError::Conflict(
            "This source is already refreshing".to_owned(),
        ));
    }
    let current_library_id: Uuid = row.try_get("library_id")?;
    let current_name: String = row.try_get("name")?;
    let current_playlist_url: String = row.try_get("playlist_url")?;
    let current_guide_url: Option<String> = row.try_get("guide_url")?;
    let current_pins: SqlJson<Vec<OriginPin>> = row.try_get("origin_pins")?;
    let library_id = request.library_id.unwrap_or(current_library_id);
    let name = request.name.as_deref().unwrap_or(&current_name).trim();
    let playlist_url = request
        .playlist_url
        .as_deref()
        .unwrap_or(&current_playlist_url)
        .trim();
    let guide_url = match request.guide_url {
        PatchField::Missing => current_guide_url.clone(),
        PatchField::Present(guide_url) => guide_url.as_deref().map(str::trim).map(str::to_owned),
    };
    let origin_pins = request.origin_pins.as_ref().unwrap_or(&current_pins.0);
    let effective_request = SourceRequest {
        library_id,
        name: name.to_owned(),
        playlist_url: playlist_url.to_owned(),
        guide_url: guide_url.clone(),
        origin_pins: origin_pins.clone(),
    };
    validate_source_request(&effective_request)?;
    validate_source_library(&state, library_id).await?;
    let has_channels: bool =
        sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM live_tv_channels WHERE source_id=$1)")
            .bind(source_id)
            .fetch_one(&mut *tx)
            .await?;
    if has_channels && current_library_id != library_id {
        return Err(ApiError::Conflict(
            "The recording library cannot be changed after channels have been imported".to_owned(),
        ));
    }
    let channel_configuration_changed = current_library_id != library_id
        || current_playlist_url != playlist_url
        || current_pins.0 != *origin_pins;
    let guide_configuration_changed = current_guide_url != guide_url;
    let ingest_configuration_changed = channel_configuration_changed || guide_configuration_changed;
    let channels_to_stop = if channel_configuration_changed {
        invalidate_source_catalog(&mut tx, source_id).await?
    } else if guide_configuration_changed {
        delete_source_programs(&mut tx, source_id).await?;
        Vec::new()
    } else {
        Vec::new()
    };
    sqlx::query(
        "UPDATE live_tv_sources SET library_id=$2,name=$3,playlist_url=$4,guide_url=$5, \
         origin_pins=$6,refresh_status=CASE WHEN $7 THEN 'queued' ELSE refresh_status END, \
         last_refreshed_at=CASE WHEN $7 THEN NULL ELSE last_refreshed_at END, \
         last_error_code=CASE WHEN $7 THEN NULL ELSE last_error_code END, \
         updated_at=NOW() WHERE id=$1 AND deleted_at IS NULL",
    )
    .bind(source_id)
    .bind(library_id)
    .bind(name)
    .bind(playlist_url)
    .bind(guide_url.as_deref())
    .bind(SqlJson(serde_json::to_value(origin_pins).map_err(
        |_| ApiError::BadRequest("The origin pin set is invalid".to_owned()),
    )?))
    .bind(ingest_configuration_changed)
    .execute(&mut *tx)
    .await
    .map_err(source_name_conflict)?;
    tx.commit().await?;
    super::livetv_runtime::stop_channels(&channels_to_stop).await;
    load_source_dto(&state, source_id).await.map(Json)
}

async fn delete_source(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(source_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    require_live_tv_management(&admin)?;
    let mut tx = state.db.begin().await?;
    db::require_active_run(&mut tx, state.run_id).await?;
    // Keep the same lock order as refresh persistence and timer mutations.
    lock_series_materialization_users(&mut tx, Some(source_id), None).await?;
    let row = sqlx::query(
        "SELECT refresh_status FROM live_tv_sources \
         WHERE id=$1 AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(source_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::NotFound)?;
    if row.try_get::<String, _>("refresh_status")? == "running" {
        return Err(ApiError::Conflict(
            "This source is already refreshing".to_owned(),
        ));
    }
    let channels_to_stop = invalidate_source_catalog(&mut tx, source_id).await?;
    sqlx::query(
        "UPDATE live_tv_sources SET enabled=FALSE,playlist_url=NULL,guide_url=NULL,origin_pins=NULL, \
         deleted_at=NOW(),refresh_status='queued',refresh_claimed_run_id=NULL,refresh_started_at=NULL, \
         last_error_code=NULL,updated_at=NOW() WHERE id=$1 AND deleted_at IS NULL",
    )
    .bind(source_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    super::livetv_runtime::stop_channels(&channels_to_stop).await;
    Ok(StatusCode::NO_CONTENT)
}

/// Retire imported channel data after a feed configuration change or source
/// deletion. The catalog rows remain for timer/recording history, while the
/// feed URLs and pins are either replaced by update_source or erased by
/// delete_source.
async fn invalidate_source_catalog(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    source_id: Uuid,
) -> Result<Vec<Uuid>, ApiError> {
    let channel_ids = sqlx::query_scalar::<_, Uuid>(
        "SELECT item_id FROM live_tv_channels WHERE source_id=$1 ORDER BY item_id",
    )
    .bind(source_id)
    .fetch_all(&mut **tx)
    .await?;
    if channel_ids.is_empty() {
        return Ok(channel_ids);
    }

    let _locked_rules = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM live_tv_series_timers WHERE channel_item_id=ANY($1) ORDER BY id FOR UPDATE",
    )
    .bind(&channel_ids)
    .fetch_all(&mut **tx)
    .await?;
    let timer_statuses = sqlx::query_scalar::<_, String>(
        "SELECT status FROM live_tv_timers WHERE channel_item_id=ANY($1) \
         AND status IN ('scheduled','recording') ORDER BY id FOR UPDATE",
    )
    .bind(&channel_ids)
    .fetch_all(&mut **tx)
    .await?;
    if timer_statuses.iter().any(|status| status == "recording") {
        return Err(ApiError::Conflict(
            "A source with an active recording cannot be changed or deleted".to_owned(),
        ));
    }
    let active_recordings = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM live_tv_recordings WHERE channel_item_id=ANY($1) \
         AND status IN ('recording','publishing') ORDER BY id FOR UPDATE",
    )
    .bind(&channel_ids)
    .fetch_all(&mut **tx)
    .await?;
    if !active_recordings.is_empty() {
        return Err(ApiError::Conflict(
            "A source with an active recording cannot be changed or deleted".to_owned(),
        ));
    }

    sqlx::query(
        "UPDATE live_tv_series_timers SET enabled=FALSE,updated_at=NOW() \
         WHERE channel_item_id=ANY($1) AND enabled=TRUE",
    )
    .bind(&channel_ids)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE live_tv_timers SET status='cancelled',finished_at=NOW(),last_error_code='cancelled',updated_at=NOW() \
         WHERE channel_item_id=ANY($1) AND status='scheduled'",
    )
    .bind(&channel_ids)
    .execute(&mut **tx)
    .await?;
    sqlx::query(
        "UPDATE live_tv_channels SET enabled=FALSE,stream_url=NULL,logo_url=NULL,updated_at=NOW() \
         WHERE source_id=$1",
    )
    .bind(source_id)
    .execute(&mut **tx)
    .await?;
    delete_source_programs(tx, source_id).await?;
    Ok(channel_ids)
}

async fn delete_source_programs(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    source_id: Uuid,
) -> Result<(), ApiError> {
    sqlx::query(
        "DELETE FROM live_tv_programs p USING live_tv_channels c \
         WHERE p.channel_item_id=c.item_id AND c.source_id=$1",
    )
    .bind(source_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

fn url_uses_approved_origin(raw: &str, origins: &[url::Url]) -> bool {
    let Ok(candidate) = url::Url::parse(raw) else {
        return false;
    };
    if !matches!(candidate.scheme(), "http" | "https")
        || !candidate.username().is_empty()
        || candidate.password().is_some()
        || candidate.fragment().is_some()
        || candidate.host_str().is_none()
    {
        return false;
    }
    origins.iter().any(|origin| {
        candidate.scheme().eq_ignore_ascii_case(origin.scheme())
            && candidate
                .host_str()
                .zip(origin.host_str())
                .is_some_and(|(left, right)| left.eq_ignore_ascii_case(right))
            && candidate.port_or_known_default() == origin.port_or_known_default()
    })
}

async fn refresh_source(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Path(source_id): Path<Uuid>,
) -> Result<Json<SourceDto>, ApiError> {
    require_live_tv_management(&admin)?;
    let source = claim_source_refresh(&state, source_id).await?;
    match refresh_source_contents(&source).await {
        Ok((channels, guide_programs)) => {
            persist_refresh(&state, &source, channels, guide_programs).await?;
        }
        Err(error) => {
            let public_code = match error {
                FeedError::Unavailable => "fetch-failed",
                FeedError::TooLarge
                | FeedError::InvalidEncoding
                | FeedError::InvalidStructure
                | FeedError::InvalidValue
                | FeedError::DisallowedOrigin
                | FeedError::LimitExceeded
                | FeedError::DuplicateId
                | FeedError::UnsupportedTransport
                | FeedError::QuotaExceeded
                | FeedError::Cancelled => "invalid-playlist",
            };
            finish_refresh_error(&state, source_id, public_code).await?;
            return Err(match error {
                FeedError::Unavailable => ApiError::Unavailable,
                _ => ApiError::BadRequest("Configured feed did not pass validation".to_owned()),
            });
        }
    }
    load_source_dto(&state, source_id).await.map(Json)
}

async fn claim_source_refresh(state: &AppState, source_id: Uuid) -> Result<SourceRecord, ApiError> {
    let mut tx = state.db.begin().await?;
    db::require_active_run(&mut tx, state.run_id).await?;
    let row = sqlx::query(
        "SELECT id,library_id,playlist_url,guide_url,origin_pins \
         FROM live_tv_sources WHERE id=$1 AND enabled=TRUE FOR UPDATE",
    )
    .bind(source_id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::NotFound)?;
    let claimed = sqlx::query(
        "UPDATE live_tv_sources SET refresh_status='running',refresh_claimed_run_id=$2, \
         refresh_started_at=NOW(),last_error_code=NULL,updated_at=NOW() WHERE id=$1 \
         AND refresh_status <> 'running'",
    )
    .bind(source_id)
    .bind(state.run_id)
    .execute(&mut *tx)
    .await?;
    if claimed.rows_affected() != 1 {
        return Err(ApiError::Conflict(
            "This source is already refreshing".to_owned(),
        ));
    }
    let pins: SqlJson<Vec<OriginPin>> = row.try_get("origin_pins")?;
    let source = SourceRecord {
        id: row.try_get("id")?,
        library_id: row.try_get("library_id")?,
        playlist_url: row.try_get("playlist_url")?,
        guide_url: row.try_get("guide_url")?,
        pins: pins.0,
    };
    tx.commit().await?;
    Ok(source)
}

async fn refresh_source_contents(
    source: &SourceRecord,
) -> Result<(Vec<ResolvedChannel>, Vec<ResolvedProgram>), FeedError> {
    let approved = approved_urls(&source.pins)?;
    let playlist_bytes =
        fetch_bounded_feed(&source.playlist_url, &source.pins, 4 * 1024 * 1024).await?;
    let playlist_url =
        url::Url::parse(&source.playlist_url).map_err(|_| FeedError::InvalidValue)?;
    let channels = parse_m3u(&playlist_bytes, &playlist_url, &approved)?;
    let mut resolved_channels = Vec::with_capacity(channels.len());
    let mut source_ids = HashMap::new();
    for channel in channels {
        let external_id = channel.source_id.clone().unwrap_or_else(|| {
            let normalized = channel
                .name
                .split_whitespace()
                .collect::<Vec<_>>()
                .join(" ");
            format!("name:{}", stable_digest(&normalized.to_ascii_lowercase()))
        });
        if source_ids.insert(external_id.clone(), ()).is_some() {
            return Err(FeedError::DuplicateId);
        }
        resolved_channels.push(ResolvedChannel {
            external_id,
            channel,
        });
    }

    let mut guide_programs = Vec::new();
    if let Some(guide_url) = &source.guide_url {
        let guide_bytes = fetch_bounded_feed(guide_url, &source.pins, 16 * 1024 * 1024).await?;
        let guide_url = url::Url::parse(guide_url).map_err(|_| FeedError::InvalidValue)?;
        let (_guide_channels, programs) = parse_xmltv(&guide_bytes, &guide_url, &approved)?;
        guide_programs.extend(
            programs
                .into_iter()
                .map(|program| ResolvedProgram { program }),
        );
    }
    Ok((resolved_channels, guide_programs))
}

struct ResolvedChannel {
    external_id: String,
    channel: M3uChannel,
}

struct ResolvedProgram {
    program: XmltvProgram,
}

async fn persist_refresh(
    state: &AppState,
    source: &SourceRecord,
    channels: Vec<ResolvedChannel>,
    programs: Vec<ResolvedProgram>,
) -> Result<(), ApiError> {
    let mut tx = state.db.begin().await?;
    db::require_active_run(&mut tx, state.run_id).await?;
    // Timer mutations lock users before series rows. Lock the source's active
    // timer/rule owners and eligible users before guide replacement can take
    // program/FK locks, preserving that order for the whole refresh transaction.
    lock_series_materialization_users(&mut tx, Some(source.id), None).await?;
    // Recorder claims take source -> channel -> timer locks. Keep refresh
    // persistence in the source lifecycle's user -> source order so stale
    // channel cleanup cannot race a newly inserted active recording.
    let source_locked: Option<Uuid> = sqlx::query_scalar(
        "SELECT id FROM live_tv_sources WHERE id=$1 AND refresh_status='running' \
         AND refresh_claimed_run_id=$2 AND enabled=TRUE AND deleted_at IS NULL FOR UPDATE",
    )
    .bind(source.id)
    .bind(state.run_id)
    .fetch_optional(&mut *tx)
    .await?;
    if source_locked.is_none() {
        return Err(ApiError::Conflict(
            "Source refresh claim expired".to_owned(),
        ));
    }
    let mut channel_ids = Vec::with_capacity(channels.len());
    let mut item_by_external = HashMap::new();
    for entry in &channels {
        let item_id = stable_channel_id(source.id, &entry.external_id);
        channel_ids.push(item_id);
        item_by_external.insert(entry.external_id.clone(), item_id);
        let opaque_path = format!("puffinbox://livetv/{}/{}", source.id, item_id);
        let overview = entry.channel.group.as_deref();
        let metadata = json!({ "SourceId": source.id, "ChannelId": entry.external_id });
        db::upsert_virtual_item(
            &mut tx,
            state.run_id,
            db::VirtualItemInput {
                library_id: source.library_id,
                item_id,
                name: &entry.channel.name,
                item_type: "LiveTvChannel",
                opaque_path: &opaque_path,
                overview,
                metadata_json: &metadata,
            },
        )
        .await?;
        sqlx::query(
            "INSERT INTO live_tv_channels(item_id,library_id,source_id,source_channel_id,name,group_name,stream_url,logo_url,enabled,updated_at) \
             VALUES($1,$2,$3,$4,$5,$6,$7,$8,TRUE,NOW()) \
             ON CONFLICT(source_id,source_channel_id) DO UPDATE SET item_id=EXCLUDED.item_id, \
             name=EXCLUDED.name,group_name=EXCLUDED.group_name,stream_url=EXCLUDED.stream_url, \
             logo_url=EXCLUDED.logo_url,enabled=TRUE,updated_at=NOW() \
             WHERE live_tv_channels.library_id=EXCLUDED.library_id",
        )
        .bind(item_id)
        .bind(source.library_id)
        .bind(source.id)
        .bind(&entry.external_id)
        .bind(&entry.channel.name)
        .bind(entry.channel.group.as_deref())
        .bind(entry.channel.stream_url.as_str())
        .bind(entry.channel.logo_url.as_ref().map(url::Url::as_str))
        .execute(&mut *tx)
        .await?;
        if let Some(existing_item_id) = sqlx::query_scalar::<_, Uuid>(
            "SELECT item_id FROM live_tv_channels WHERE source_id=$1 AND source_channel_id=$2",
        )
        .bind(source.id)
        .bind(&entry.external_id)
        .fetch_optional(&mut *tx)
        .await?
            && existing_item_id != item_id
        {
            return Err(ApiError::Conflict(
                "A channel identity changed unexpectedly".to_owned(),
            ));
        }
    }
    sqlx::query(
        "UPDATE live_tv_channels c SET enabled=FALSE, \
         stream_url=CASE WHEN EXISTS(SELECT 1 FROM live_tv_recordings r \
             WHERE r.channel_item_id=c.item_id AND r.status IN ('recording','publishing')) \
             THEN c.stream_url ELSE NULL END, \
         logo_url=CASE WHEN EXISTS(SELECT 1 FROM live_tv_recordings r \
             WHERE r.channel_item_id=c.item_id AND r.status IN ('recording','publishing')) \
             THEN c.logo_url ELSE NULL END,updated_at=NOW() \
         WHERE c.source_id=$1 AND NOT (c.item_id = ANY($2))",
    )
    .bind(source.id)
    .bind(&channel_ids)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "DELETE FROM live_tv_programs p USING live_tv_channels c \
         WHERE p.channel_item_id=c.item_id AND c.source_id=$1",
    )
    .bind(source.id)
    .execute(&mut *tx)
    .await?;
    for entry in programs {
        let Some(channel_item_id) = item_by_external.get(&entry.program.channel_id) else {
            continue;
        };
        let program_id = stable_program_id(
            *channel_item_id,
            entry.program.start.timestamp_micros(),
            &entry.program.title,
        );
        sqlx::query(
            "INSERT INTO live_tv_programs( \
             id,channel_item_id,source_program_id,start_at,end_at,title,description,category, \
             rating_system,content_rating,policy_rating_scale,policy_rating_value) \
             VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,CASE WHEN $11::smallint IS NULL \
             THEN NULL ELSE 'US-PARENTAL-v1' END,$11)",
        )
        .bind(program_id)
        .bind(channel_item_id)
        .bind(entry.program.channel_id)
        .bind(entry.program.start)
        .bind(entry.program.stop)
        .bind(entry.program.title)
        .bind(entry.program.description)
        .bind(entry.program.category)
        .bind(entry.program.rating_system)
        .bind(entry.program.content_rating)
        .bind(entry.program.policy_rating_value)
        .execute(&mut *tx)
        .await?;
    }
    materialize_series_timers(&mut tx, Some(source.id), None).await?;
    sqlx::query(
        "UPDATE live_tv_sources SET refresh_status='ready',refresh_claimed_run_id=NULL, \
         refresh_started_at=NULL,last_refreshed_at=NOW(),last_error_code=NULL,updated_at=NOW() \
         WHERE id=$1 AND refresh_claimed_run_id=$2",
    )
    .bind(source.id)
    .bind(state.run_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn finish_refresh_error(
    state: &AppState,
    source_id: Uuid,
    error_code: &str,
) -> Result<(), ApiError> {
    let mut tx = state.db.begin().await?;
    db::require_active_run(&mut tx, state.run_id).await?;
    sqlx::query(
        "UPDATE live_tv_sources SET refresh_status='failed',refresh_claimed_run_id=NULL, \
         refresh_started_at=NULL,last_error_code=$3,updated_at=NOW() \
         WHERE id=$1 AND refresh_claimed_run_id=$2",
    )
    .bind(source_id)
    .bind(state.run_id)
    .bind(error_code)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(())
}

async fn load_source_dto(state: &AppState, id: Uuid) -> Result<SourceDto, ApiError> {
    let row = sqlx::query(
        "SELECT id,library_id,name,enabled,refresh_status,last_refreshed_at,last_error_code \
         FROM live_tv_sources WHERE id=$1",
    )
    .bind(id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(ApiError::NotFound)?;
    Ok(SourceDto {
        id: row.try_get("id")?,
        library_id: row.try_get("library_id")?,
        name: row.try_get("name")?,
        enabled: row.try_get("enabled")?,
        refresh_status: row.try_get("refresh_status")?,
        last_refreshed_at: row.try_get("last_refreshed_at")?,
        last_error_code: row.try_get("last_error_code")?,
    })
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct PageQuery {
    start_index: Option<i64>,
    limit: Option<i64>,
}

fn page(query: PageQuery) -> Result<(i64, i64), ApiError> {
    let start = query.start_index.unwrap_or(0);
    let limit = query.limit.unwrap_or(100);
    if !(0..=1_000_000).contains(&start) || !(1..=MAX_LIST_LIMIT).contains(&limit) {
        return Err(ApiError::BadRequest("Page bounds are invalid".to_owned()));
    }
    Ok((start, limit))
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct QueryResult<T> {
    items: Vec<T>,
    total_record_count: usize,
    start_index: usize,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct ChannelDto {
    id: Uuid,
    name: String,
    #[serde(rename = "Type")]
    item_type: &'static str,
    media_type: &'static str,
    channel_type: &'static str,
    is_folder: bool,
    overview: Option<String>,
}

async fn list_channels(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(query): Query<PageQuery>,
) -> Result<Json<QueryResult<ChannelDto>>, ApiError> {
    require_live_tv_access(&user)?;
    let (start, limit) = page(query)?;
    let total = channel_query(&user, true)
        .build_query_scalar::<i64>()
        .fetch_one(&state.db)
        .await?;
    let mut rows_query = channel_query(&user, false);
    rows_query
        .push(" ORDER BY i.sort_name,c.item_id LIMIT ")
        .push_bind(limit)
        .push(" OFFSET ")
        .push_bind(start);
    let rows = rows_query.build().fetch_all(&state.db).await?;
    let items = rows
        .into_iter()
        .map(|row| {
            Ok(ChannelDto {
                id: row.try_get("item_id")?,
                name: row.try_get("name")?,
                item_type: "LiveTvChannel",
                media_type: "Video",
                channel_type: "TV",
                is_folder: false,
                overview: row.try_get("overview")?,
            })
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    Ok(Json(QueryResult {
        items,
        total_record_count: usize::try_from(total).unwrap_or(usize::MAX),
        start_index: start as usize,
    }))
}

fn channel_query(user: &UserRecord, count_only: bool) -> QueryBuilder<'static, Postgres> {
    let select = if count_only {
        "SELECT COUNT(*)::BIGINT"
    } else {
        "SELECT c.item_id,i.name,i.overview"
    };
    let mut query = QueryBuilder::<Postgres>::new(select);
    query.push(
        " FROM live_tv_channels c JOIN items i ON i.id=c.item_id \
         JOIN libraries l ON l.id=c.library_id \
         LEFT JOIN item_metadata channel_metadata ON channel_metadata.item_id=i.id \
         AND channel_metadata.provider_key='local-nfo' \
         AND channel_metadata.policy_rating_scale='US-MPAA-v1' \
         LEFT JOIN LATERAL (SELECT p.id,p.policy_rating_value FROM live_tv_programs p \
         WHERE p.channel_item_id=c.item_id AND p.start_at <= NOW() AND p.end_at > NOW() \
         ORDER BY p.start_at DESC,p.id LIMIT 1) current_program ON TRUE \
         WHERE c.enabled=TRUE AND l.enabled=TRUE AND i.item_type='LiveTvChannel' \
         AND i.path LIKE 'puffinbox://livetv/%'",
    );
    append_library_policy(&mut query, user, "c.library_id");
    if !user.is_admin {
        append_rating_policy(
            &mut query,
            user,
            "channel_metadata.policy_rating_value",
            "LiveTvChannel",
        );
        append_rating_policy(
            &mut query,
            user,
            "current_program.policy_rating_value",
            "LiveTvProgram",
        );
        if user
            .block_unrated_items
            .iter()
            .any(|value| value == "LiveTvProgram")
        {
            query.push(" AND current_program.id IS NOT NULL ");
        }
    }
    query
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct ProgramQuery {
    start_index: Option<i64>,
    limit: Option<i64>,
    min_start_date: Option<DateTime<Utc>>,
    max_start_date: Option<DateTime<Utc>>,
    channel_id: Option<Uuid>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct ProgramDto {
    id: Uuid,
    name: String,
    #[serde(rename = "Type")]
    item_type: &'static str,
    channel_id: Uuid,
    start_date: DateTime<Utc>,
    end_date: DateTime<Utc>,
    overview: Option<String>,
    category: Option<String>,
    official_rating: Option<String>,
}

async fn list_programs(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(query): Query<ProgramQuery>,
) -> Result<Json<QueryResult<ProgramDto>>, ApiError> {
    require_live_tv_access(&user)?;
    let (start, limit) = page(PageQuery {
        start_index: query.start_index,
        limit: query.limit,
    })?;
    let minimum = query.min_start_date.unwrap_or_else(Utc::now);
    let maximum = query
        .max_start_date
        .unwrap_or_else(|| minimum + Duration::days(7));
    if maximum <= minimum || maximum - minimum > MAX_GUIDE_WINDOW {
        return Err(ApiError::BadRequest(
            "Guide date range is invalid".to_owned(),
        ));
    }
    let total = program_query(&user, query.channel_id, minimum, maximum, true)
        .build_query_scalar::<i64>()
        .fetch_one(&state.db)
        .await?;
    let mut rows_query = program_query(&user, query.channel_id, minimum, maximum, false);
    rows_query
        .push(" ORDER BY p.start_at,p.channel_item_id,p.id LIMIT ")
        .push_bind(limit)
        .push(" OFFSET ")
        .push_bind(start);
    let rows = rows_query.build().fetch_all(&state.db).await?;
    let items = rows
        .into_iter()
        .map(|row| {
            Ok(ProgramDto {
                id: row.try_get("id")?,
                name: row.try_get("title")?,
                item_type: "LiveTvProgram",
                channel_id: row.try_get("channel_item_id")?,
                start_date: row.try_get("start_at")?,
                end_date: row.try_get("end_at")?,
                overview: row.try_get("description")?,
                category: row.try_get("category")?,
                official_rating: row.try_get("content_rating")?,
            })
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    Ok(Json(QueryResult {
        items,
        total_record_count: usize::try_from(total).unwrap_or(usize::MAX),
        start_index: start as usize,
    }))
}

fn program_query(
    user: &UserRecord,
    channel_id: Option<Uuid>,
    minimum: DateTime<Utc>,
    maximum: DateTime<Utc>,
    count_only: bool,
) -> QueryBuilder<'static, Postgres> {
    let select = if count_only {
        "SELECT COUNT(*)::BIGINT"
    } else {
        "SELECT p.id,p.channel_item_id,p.start_at,p.end_at,p.title,p.description,p.category,p.content_rating"
    };
    let mut query = QueryBuilder::<Postgres>::new(select);
    query
        .push(
            " FROM live_tv_programs p JOIN live_tv_channels c ON c.item_id=p.channel_item_id \
             JOIN items i ON i.id=c.item_id JOIN libraries l ON l.id=c.library_id \
             LEFT JOIN item_metadata channel_metadata ON channel_metadata.item_id=i.id \
             AND channel_metadata.provider_key='local-nfo' \
             AND channel_metadata.policy_rating_scale='US-MPAA-v1' \
             WHERE c.enabled=TRUE AND l.enabled=TRUE AND i.item_type='LiveTvChannel' \
             AND i.path LIKE 'puffinbox://livetv/%' AND p.start_at >= ",
        )
        .push_bind(minimum)
        .push(" AND p.start_at < ")
        .push_bind(maximum)
        .push(" AND (")
        .push_bind(channel_id.is_none())
        .push(" OR p.channel_item_id = ")
        .push_bind(channel_id)
        .push(")");
    append_library_policy(&mut query, user, "c.library_id");
    if !user.is_admin {
        append_rating_policy(
            &mut query,
            user,
            "channel_metadata.policy_rating_value",
            "LiveTvChannel",
        );
        append_rating_policy(&mut query, user, "p.policy_rating_value", "LiveTvProgram");
    }
    query
}

fn append_library_policy(
    query: &mut QueryBuilder<'_, Postgres>,
    user: &UserRecord,
    library_column: &str,
) {
    if !user.is_admin && user.restrict_libraries {
        query
            .push(" AND ")
            .push(library_column)
            .push(" = ANY(")
            .push_bind(user.allowed_library_ids.clone())
            .push(")");
    }
}

fn append_rating_policy(
    query: &mut QueryBuilder<'_, Postgres>,
    user: &UserRecord,
    rating_column: &str,
    category: &str,
) {
    if let Some(maximum) = user.max_parental_rating {
        query
            .push(" AND (")
            .push(rating_column)
            .push(" IS NULL OR ")
            .push(rating_column)
            .push(" <= ")
            .push_bind(maximum as i16)
            .push(")");
    }
    if user
        .block_unrated_items
        .iter()
        .any(|blocked| blocked == category)
    {
        query.push(" AND ").push(rating_column).push(" IS NOT NULL");
    }
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct TimerRequest {
    name: Option<String>,
    channel_id: Uuid,
    program_id: Option<Uuid>,
    start_date: Option<DateTime<Utc>>,
    end_date: Option<DateTime<Utc>>,
    pre_padding_seconds: Option<i32>,
    post_padding_seconds: Option<i32>,
    output_library_id: Option<Uuid>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct TimerListQuery {
    channel_id: Option<Uuid>,
    series_timer_id: Option<Uuid>,
    is_active: Option<bool>,
    is_scheduled: Option<bool>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct TimerDto {
    id: Uuid,
    name: String,
    channel_id: Uuid,
    program_id: Option<Uuid>,
    series_timer_id: Option<Uuid>,
    start_date: DateTime<Utc>,
    end_date: DateTime<Utc>,
    pre_padding_seconds: i32,
    post_padding_seconds: i32,
    status: String,
    output_library_id: Uuid,
    last_error_code: Option<String>,
    official_rating: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct TimerDefaultsQuery {
    #[serde(alias = "programId")]
    program_id: Option<Uuid>,
}

/// The public Timer defaults endpoint returns the standard SeriesTimerInfoDto
/// shape. Keep the Puffinbox-only recording destination out of this response.
#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct TimerDefaultsDto {
    #[serde(rename = "Type")]
    type_name: &'static str,
    server_id: Uuid,
    #[serde(skip_serializing_if = "Option::is_none")]
    channel_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    channel_name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    program_id: Option<Uuid>,
    #[serde(skip_serializing_if = "Option::is_none")]
    name: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    overview: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    start_date: Option<DateTime<Utc>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    end_date: Option<DateTime<Utc>>,
    priority: i32,
    pre_padding_seconds: i32,
    post_padding_seconds: i32,
    is_pre_padding_required: bool,
    is_post_padding_required: bool,
    keep_until: &'static str,
    record_any_time: bool,
    skip_episodes_in_library: bool,
    record_any_channel: bool,
    keep_up_to: i32,
    record_new_only: bool,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct TimerUpdateRequest {
    id: Option<Uuid>,
    channel_id: Option<Uuid>,
    #[serde(default, deserialize_with = "deserialize_nullable_uuid")]
    program_id: Option<Option<Uuid>>,
    name: Option<String>,
    start_date: Option<DateTime<Utc>>,
    end_date: Option<DateTime<Utc>>,
    pre_padding_seconds: Option<i32>,
    post_padding_seconds: Option<i32>,
    output_library_id: Option<Uuid>,
    status: Option<String>,
    priority: Option<i32>,
    is_pre_padding_required: Option<bool>,
    is_post_padding_required: Option<bool>,
    keep_until: Option<String>,
}

fn deserialize_nullable_uuid<'de, D>(deserializer: D) -> Result<Option<Option<Uuid>>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    Option::<Uuid>::deserialize(deserializer).map(Some)
}

async fn timer_defaults(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(query): Query<TimerDefaultsQuery>,
) -> Result<Json<TimerDefaultsDto>, ApiError> {
    require_live_tv_access(&user)?;
    if user.disabled || !user.allow_media_playback {
        return Err(ApiError::Forbidden);
    }
    let (channel_id, channel_name, program_id, name, overview, start_date, end_date) =
        if let Some(program_id) = query.program_id {
            let row = sqlx::query(
                "SELECT p.id,p.channel_item_id,p.title,p.description,p.start_at,p.end_at, \
                 p.policy_rating_value,c.name AS channel_name,c.library_id \
                 FROM live_tv_programs p JOIN live_tv_channels c ON c.item_id=p.channel_item_id \
                 WHERE p.id=$1 AND c.enabled=TRUE",
            )
            .bind(program_id)
            .fetch_optional(&state.db)
            .await?
            .ok_or(ApiError::NotFound)?;
            let channel_id: Uuid = row.try_get("channel_item_id")?;
            authorize_channel(&state, &user, channel_id).await?;
            if !rating_visible(&user, row.try_get("policy_rating_value")?, "LiveTvProgram") {
                return Err(ApiError::NotFound);
            }
            let library_id: Uuid = row.try_get("library_id")?;
            if !db::library_visible_to_user(&state.db, &user, library_id).await? {
                return Err(ApiError::NotFound);
            }
            (
                Some(channel_id),
                Some(row.try_get("channel_name")?),
                Some(row.try_get("id")?),
                Some(row.try_get("title")?),
                row.try_get("description")?,
                Some(row.try_get("start_at")?),
                Some(row.try_get("end_at")?),
            )
        } else {
            (None, None, None, None, None, None, None)
        };
    Ok(Json(TimerDefaultsDto {
        type_name: "SeriesTimerInfoDto",
        server_id: state.server_id,
        channel_id,
        channel_name,
        program_id,
        name,
        overview,
        start_date,
        end_date,
        priority: 0,
        pre_padding_seconds: 0,
        post_padding_seconds: 0,
        is_pre_padding_required: false,
        is_post_padding_required: false,
        keep_until: "UntilDeleted",
        record_any_time: true,
        skip_episodes_in_library: false,
        record_any_channel: false,
        keep_up_to: 0,
        record_new_only: false,
    }))
}

async fn create_timer(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Json(request): Json<TimerRequest>,
) -> Result<StatusCode, ApiError> {
    let mut tx = state.db.begin().await?;
    let user = lock_live_tv_mutation_user(&mut tx, state.run_id, user.id, true).await?;
    if user.disabled || !user.allow_media_playback {
        return Err(ApiError::Forbidden);
    }
    authorize_channel(&state, &user, request.channel_id).await?;
    if request.program_id.is_none() && blocks_unrated(&user, "LiveTvProgram") {
        return Err(ApiError::NotFound);
    }
    // Standard Jellyfin TimerInfoDto payloads do not carry an output library.
    // Default to the channel's already-authorized Live TV library; callers
    // with an explicit destination still pass through the same user policy.
    let output_library_id = if let Some(output_library_id) = request.output_library_id {
        output_library_id
    } else {
        sqlx::query_scalar::<_, Uuid>(
            "SELECT library_id FROM live_tv_channels WHERE item_id=$1 AND enabled=TRUE",
        )
        .bind(request.channel_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(ApiError::NotFound)?
    };
    if !db::library_visible_to_user(&state.db, &user, output_library_id).await? {
        return Err(ApiError::NotFound);
    }
    let (start, end, rating_system, content_rating, rating_value, stored_name) = if let Some(
        program_id,
    ) =
        request.program_id
    {
        let row = sqlx::query(
            "SELECT start_at,end_at,channel_item_id,policy_rating_value,rating_system,content_rating,title \
             FROM live_tv_programs WHERE id=$1",
        )
        .bind(program_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(ApiError::NotFound)?;
        let channel_id: Uuid = row.try_get("channel_item_id")?;
        if channel_id != request.channel_id {
            return Err(ApiError::BadRequest(
                "Program does not belong to the selected channel".to_owned(),
            ));
        }
        let rating_value: Option<i16> = row.try_get("policy_rating_value")?;
        if !rating_visible(&user, rating_value, "LiveTvProgram") {
            return Err(ApiError::NotFound);
        }
        let program_title: String = row.try_get("title")?;
        (
            request.start_date.unwrap_or(row.try_get("start_at")?),
            request.end_date.unwrap_or(row.try_get("end_at")?),
            row.try_get("rating_system")?,
            row.try_get("content_rating")?,
            rating_value,
            Some(program_title),
        )
    } else {
        let supplied_name = request
            .name
            .as_deref()
            .map(sanitize_timer_name)
            .transpose()?;
        (
            request
                .start_date
                .ok_or_else(|| ApiError::BadRequest("StartDate is required".to_owned()))?,
            request
                .end_date
                .ok_or_else(|| ApiError::BadRequest("EndDate is required".to_owned()))?,
            None::<String>,
            None::<String>,
            None::<i16>,
            supplied_name,
        )
    };
    let now = Utc::now();
    let pre = request.pre_padding_seconds.unwrap_or(0);
    let post = request.post_padding_seconds.unwrap_or(0);
    if start <= now - Duration::minutes(2)
        || end <= start
        || end - start + Duration::seconds(i64::from(pre) + i64::from(post)) > MAX_TIMER_DURATION
        || !(0..=3600).contains(&pre)
        || !(0..=3600).contains(&post)
    {
        return Err(ApiError::BadRequest(
            "Timer schedule is outside supported bounds".to_owned(),
        ));
    }
    let id = Uuid::new_v4();
    sqlx::query(
        "INSERT INTO live_tv_timers(id,owner_user_id,channel_item_id,program_id,start_at,end_at, \
         padding_before_seconds,padding_after_seconds,output_library_id,rating_system,content_rating, \
         policy_rating_scale,policy_rating_value,display_name) \
         VALUES($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$11, \
         CASE WHEN $12::smallint IS NULL THEN NULL ELSE 'US-PARENTAL-v1' END,$12,$13)",
    )
    .bind(id)
    .bind(user.id)
    .bind(request.channel_id)
    .bind(request.program_id)
    .bind(start)
    .bind(end)
    .bind(pre)
    .bind(post)
    .bind(output_library_id)
    .bind(rating_system)
    .bind(content_rating)
    .bind(rating_value)
    .bind(stored_name)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_timers(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(query): Query<TimerListQuery>,
) -> Result<Json<QueryResult<TimerDto>>, ApiError> {
    require_live_tv_access(&user)?;
    let total = timer_query(&user, &query, true)
        .build_query_scalar::<i64>()
        .fetch_one(&state.db)
        .await?;
    let mut rows_query = timer_query(&user, &query, false);
    rows_query.push(" ORDER BY t.start_at,t.id LIMIT 500");
    let rows = rows_query.build().fetch_all(&state.db).await?;
    let items = rows
        .iter()
        .map(timer_dto)
        .collect::<Result<Vec<_>, ApiError>>()?;
    Ok(Json(QueryResult {
        items,
        total_record_count: usize::try_from(total).unwrap_or(usize::MAX),
        start_index: 0,
    }))
}

async fn update_timer(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(timer_id): Path<Uuid>,
    Json(request): Json<TimerUpdateRequest>,
) -> Result<StatusCode, ApiError> {
    if request.id.is_some_and(|id| id != timer_id) {
        return Err(ApiError::BadRequest(
            "Id does not match the timer path".to_owned(),
        ));
    }
    if request
        .status
        .as_deref()
        .is_some_and(|status| !status.eq_ignore_ascii_case("New"))
        || request.priority.is_some_and(|priority| priority != 0)
        || request.is_pre_padding_required == Some(true)
        || request.is_post_padding_required == Some(true)
        || request
            .keep_until
            .as_deref()
            .is_some_and(|keep| !keep.eq_ignore_ascii_case("UntilDeleted"))
    {
        return Err(ApiError::BadRequest(
            "This IPTV timer only supports scheduled entries and UntilDeleted retention".to_owned(),
        ));
    }

    let mut tx = state.db.begin().await?;
    let user = lock_live_tv_mutation_user(&mut tx, state.run_id, user.id, true).await?;
    if user.disabled || !user.allow_media_playback {
        return Err(ApiError::Forbidden);
    }
    let row = sqlx::query(
        "SELECT channel_item_id,program_id,start_at,end_at,padding_before_seconds, \
         padding_after_seconds,output_library_id,display_name FROM live_tv_timers \
         WHERE id=$1 AND owner_user_id=$2 AND status='scheduled' FOR UPDATE",
    )
    .bind(timer_id)
    .bind(user.id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::NotFound)?;
    let old_channel_id: Uuid = row.try_get("channel_item_id")?;
    let old_program_id: Option<Uuid> = row.try_get("program_id")?;
    let channel_id = request.channel_id.unwrap_or(old_channel_id);
    authorize_channel(&state, &user, channel_id).await?;
    let program_id = request.program_id.unwrap_or(old_program_id);

    let program_defaults = if let Some(program_id) = program_id {
        let program = sqlx::query(
            "SELECT channel_item_id,start_at,end_at,title,rating_system,content_rating, \
             policy_rating_value FROM live_tv_programs WHERE id=$1",
        )
        .bind(program_id)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or(ApiError::NotFound)?;
        if program.try_get::<Uuid, _>("channel_item_id")? != channel_id {
            return Err(ApiError::BadRequest(
                "Program does not belong to the selected channel".to_owned(),
            ));
        }
        let rating: Option<i16> = program.try_get("policy_rating_value")?;
        if !rating_visible(&user, rating, "LiveTvProgram") {
            return Err(ApiError::NotFound);
        }
        Some((
            program.try_get::<DateTime<Utc>, _>("start_at")?,
            program.try_get::<DateTime<Utc>, _>("end_at")?,
            program.try_get::<Option<String>, _>("title")?,
            program.try_get::<Option<String>, _>("rating_system")?,
            program.try_get::<Option<String>, _>("content_rating")?,
            rating,
        ))
    } else {
        if blocks_unrated(&user, "LiveTvProgram") {
            return Err(ApiError::NotFound);
        }
        None
    };
    let program_changed = program_id != old_program_id;
    let start_default = program_defaults
        .as_ref()
        .filter(|_| program_changed)
        .map(|values| values.0)
        .unwrap_or(row.try_get("start_at")?);
    let end_default = program_defaults
        .as_ref()
        .filter(|_| program_changed)
        .map(|values| values.1)
        .unwrap_or(row.try_get("end_at")?);
    let start = request.start_date.unwrap_or(start_default);
    let end = request.end_date.unwrap_or(end_default);
    let pre_padding = request
        .pre_padding_seconds
        .unwrap_or(row.try_get("padding_before_seconds")?);
    let post_padding = request
        .post_padding_seconds
        .unwrap_or(row.try_get("padding_after_seconds")?);
    if start <= Utc::now() - Duration::minutes(2)
        || end <= start
        || end - start + Duration::seconds(i64::from(pre_padding) + i64::from(post_padding))
            > MAX_TIMER_DURATION
        || !(0..=3600).contains(&pre_padding)
        || !(0..=3600).contains(&post_padding)
    {
        return Err(ApiError::BadRequest(
            "Timer schedule is outside supported bounds".to_owned(),
        ));
    }
    let output_library_id = request
        .output_library_id
        .unwrap_or(row.try_get("output_library_id")?);
    if !db::library_visible_to_user(&state.db, &user, output_library_id).await? {
        return Err(ApiError::NotFound);
    }
    let (rating_system, content_rating, rating_value, display_name) = if let Some((
        _,
        _,
        title,
        rating_system,
        content_rating,
        rating_value,
    )) = program_defaults
    {
        (rating_system, content_rating, rating_value, title)
    } else {
        let name = request
            .name
            .as_deref()
            .map(sanitize_timer_name)
            .transpose()?
            .or(row.try_get("display_name")?);
        (None, None, None, name)
    };

    sqlx::query(
        "UPDATE live_tv_timers SET channel_item_id=$3,program_id=$4,start_at=$5,end_at=$6, \
         padding_before_seconds=$7,padding_after_seconds=$8,output_library_id=$9, \
         rating_system=$10,content_rating=$11, \
         policy_rating_scale=CASE WHEN $12::smallint IS NULL THEN NULL ELSE 'US-PARENTAL-v1' END, \
         policy_rating_value=$12,display_name=$13,updated_at=NOW() \
         WHERE id=$1 AND owner_user_id=$2 AND status='scheduled'",
    )
    .bind(timer_id)
    .bind(user.id)
    .bind(channel_id)
    .bind(program_id)
    .bind(start)
    .bind(end)
    .bind(pre_padding)
    .bind(post_padding)
    .bind(output_library_id)
    .bind(rating_system)
    .bind(content_rating)
    .bind(rating_value)
    .bind(display_name)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SeriesTimerRequest {
    id: Option<Uuid>,
    channel_id: Option<Uuid>,
    program_id: Option<Uuid>,
    name: Option<String>,
    start_date: Option<DateTime<Utc>>,
    end_date: Option<DateTime<Utc>>,
    pre_padding_seconds: Option<i32>,
    post_padding_seconds: Option<i32>,
    output_library_id: Option<Uuid>,
    days: Option<Vec<String>>,
    day_pattern: Option<String>,
    record_any_time: Option<bool>,
    record_any_channel: Option<bool>,
    skip_episodes_in_library: Option<bool>,
    record_new_only: Option<bool>,
    keep_up_to: Option<i32>,
    keep_until: Option<String>,
    priority: Option<i32>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SeriesTimerListQuery {
    sort_by: Option<String>,
    sort_order: Option<String>,
    start_index: Option<i64>,
    limit: Option<i64>,
    enable_total_record_count: Option<bool>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct SeriesTimerDto {
    id: Uuid,
    #[serde(rename = "Type")]
    type_name: &'static str,
    server_id: Uuid,
    external_id: Option<String>,
    channel_id: Uuid,
    external_channel_id: Option<String>,
    channel_name: String,
    program_id: Option<Uuid>,
    external_program_id: Option<String>,
    name: String,
    start_date: DateTime<Utc>,
    end_date: DateTime<Utc>,
    service_name: Option<String>,
    priority: i32,
    pre_padding_seconds: i32,
    post_padding_seconds: i32,
    is_pre_padding_required: bool,
    is_post_padding_required: bool,
    keep_until: &'static str,
    record_any_time: bool,
    skip_episodes_in_library: bool,
    record_any_channel: bool,
    keep_up_to: i32,
    record_new_only: bool,
    days: Option<Vec<&'static str>>,
    day_pattern: Option<&'static str>,
    output_library_id: Uuid,
}

async fn create_series_timer(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Json(request): Json<SeriesTimerRequest>,
) -> Result<StatusCode, ApiError> {
    validate_series_options(&request)?;
    let channel_id = request
        .channel_id
        .ok_or_else(|| ApiError::BadRequest("ChannelId is required".to_owned()))?;
    let program_id = request.program_id.ok_or_else(|| {
        ApiError::BadRequest("ProgramId is required to anchor a series timer".to_owned())
    })?;
    let mut tx = state.db.begin().await?;
    let user = lock_live_tv_mutation_user(&mut tx, state.run_id, user.id, true).await?;
    if user.disabled || !user.allow_media_playback {
        return Err(ApiError::Forbidden);
    }
    authorize_channel(&state, &user, channel_id).await?;
    let anchor = sqlx::query(
        "SELECT p.channel_item_id,p.start_at,p.end_at,p.title,p.rating_system,p.content_rating, \
         p.policy_rating_scale,p.policy_rating_value,c.library_id,c.source_id \
         FROM live_tv_programs p JOIN live_tv_channels c ON c.item_id=p.channel_item_id \
         WHERE p.id=$1 AND c.enabled=TRUE",
    )
    .bind(program_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(ApiError::NotFound)?;
    if anchor.try_get::<Uuid, _>("channel_item_id")? != channel_id {
        return Err(ApiError::BadRequest(
            "Program does not belong to the selected channel".to_owned(),
        ));
    }
    let rating_value: Option<i16> = anchor.try_get("policy_rating_value")?;
    if !rating_visible(&user, rating_value, "LiveTvProgram") {
        return Err(ApiError::NotFound);
    }
    let channel_library: Uuid = anchor.try_get("library_id")?;
    let output_library = request.output_library_id.unwrap_or(channel_library);
    if !db::library_visible_to_user(&state.db, &user, output_library).await? {
        return Err(ApiError::NotFound);
    }
    let start = request
        .start_date
        .unwrap_or(anchor.try_get::<DateTime<Utc>, _>("start_at")?);
    let end = request
        .end_date
        .unwrap_or(anchor.try_get::<DateTime<Utc>, _>("end_at")?);
    let pre = request.pre_padding_seconds.unwrap_or(0);
    let post = request.post_padding_seconds.unwrap_or(0);
    validate_series_window(start, end, pre, post)?;
    let days_mask = parse_series_days(request.days.as_deref(), request.day_pattern.as_deref())?;
    let title: String = anchor.try_get("title")?;
    let name = request
        .name
        .as_deref()
        .map(sanitize_timer_name)
        .transpose()?
        .unwrap_or_else(|| title.clone());
    let id = Uuid::new_v4();
    let active_count: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM live_tv_series_timers WHERE owner_user_id=$1 AND enabled=TRUE",
    )
    .bind(user.id)
    .fetch_one(&mut *tx)
    .await?;
    if active_count >= MAX_SERIES_TIMERS_PER_USER {
        return Err(ApiError::RateLimited);
    }
    sqlx::query(
        "INSERT INTO live_tv_series_timers( \
         id,owner_user_id,channel_item_id,program_id,name,match_title,match_title_key,start_at,end_at, \
         days_mask,padding_before_seconds,padding_after_seconds,output_library_id,rating_system, \
         content_rating,policy_rating_scale,policy_rating_value) \
         VALUES($1,$2,$3,$4,$5,$6,lower(regexp_replace(btrim($6),'[[:space:]]+',' ','g')), \
         $7,$8,$9,$10,$11,$12,$13,$14,$15,$16)",
    )
    .bind(id)
    .bind(user.id)
    .bind(channel_id)
    .bind(program_id)
    .bind(&name)
    .bind(&title)
    .bind(start)
    .bind(end)
    .bind(days_mask)
    .bind(pre)
    .bind(post)
    .bind(output_library)
    .bind(anchor.try_get::<Option<String>, _>("rating_system")?)
    .bind(anchor.try_get::<Option<String>, _>("content_rating")?)
    .bind(anchor.try_get::<Option<String>, _>("policy_rating_scale")?)
    .bind(rating_value)
    .execute(&mut *tx)
    .await?;
    materialize_series_timers(&mut tx, None, Some(id)).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn list_series_timers(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(query): Query<SeriesTimerListQuery>,
) -> Result<Json<QueryResult<SeriesTimerDto>>, ApiError> {
    require_live_tv_access(&user)?;
    validate_series_sort(&query)?;
    let start_index = query.start_index.unwrap_or(0);
    let limit = query.limit.unwrap_or(MAX_SERIES_TIMER_PAGE_SIZE);
    if !(0..=MAX_SERIES_TIMER_START_INDEX).contains(&start_index)
        || !(1..=MAX_SERIES_TIMER_PAGE_SIZE).contains(&limit)
    {
        return Err(ApiError::BadRequest(
            "Series timer paging must use StartIndex 0..1000000 and Limit 1..64".to_owned(),
        ));
    }
    let total_record_count = if query.enable_total_record_count.unwrap_or(true) {
        usize::try_from(
            series_timer_query(&user, None, true)
                .build_query_scalar::<i64>()
                .fetch_one(&state.db)
                .await?,
        )
        .unwrap_or(usize::MAX)
    } else {
        0
    };
    let mut rows_query = series_timer_query(&user, None, false);
    let descending = query
        .sort_order
        .as_deref()
        .is_some_and(|order| order.eq_ignore_ascii_case("Descending"));
    rows_query.push(if descending {
        " ORDER BY s.name DESC,s.id DESC LIMIT "
    } else {
        " ORDER BY s.name,s.id LIMIT "
    });
    rows_query
        .push_bind(limit)
        .push(" OFFSET ")
        .push_bind(start_index);
    let rows = rows_query.build().fetch_all(&state.db).await?;
    let items = rows
        .iter()
        .map(|row| series_timer_dto(row, state.server_id))
        .collect::<Result<Vec<_>, ApiError>>()?;
    Ok(Json(QueryResult {
        items,
        total_record_count,
        start_index: usize::try_from(start_index).unwrap_or(usize::MAX),
    }))
}

async fn get_series_timer(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(timer_id): Path<Uuid>,
) -> Result<Json<SeriesTimerDto>, ApiError> {
    require_live_tv_access(&user)?;
    let row = series_timer_query(&user, Some(timer_id), false)
        .build()
        .fetch_optional(&state.db)
        .await?
        .ok_or(ApiError::NotFound)?;
    Ok(Json(series_timer_dto(&row, state.server_id)?))
}

async fn update_series_timer(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(timer_id): Path<Uuid>,
    Json(request): Json<SeriesTimerRequest>,
) -> Result<StatusCode, ApiError> {
    if request.id.is_some_and(|id| id != timer_id) {
        return Err(ApiError::BadRequest(
            "Id does not match the path".to_owned(),
        ));
    }
    validate_series_options(&request)?;
    let mut tx = state.db.begin().await?;
    let user = lock_live_tv_mutation_user(&mut tx, state.run_id, user.id, true).await?;
    let row = sqlx::query(
        "SELECT channel_item_id,program_id,name,start_at,end_at,days_mask,padding_before_seconds, \
         padding_after_seconds,output_library_id FROM live_tv_series_timers \
         WHERE id=$1 AND owner_user_id=$2 AND enabled=TRUE FOR UPDATE",
    )
    .bind(timer_id)
    .bind(user.id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::NotFound)?;
    let channel_id: Uuid = row.try_get("channel_item_id")?;
    if request.channel_id.is_some_and(|id| id != channel_id) {
        return Err(ApiError::BadRequest(
            "ChannelId cannot be changed for an existing series timer".to_owned(),
        ));
    }
    if request.program_id.is_some() && request.program_id != row.try_get("program_id")? {
        return Err(ApiError::BadRequest(
            "ProgramId cannot be changed; cancel and create a new rule".to_owned(),
        ));
    }
    authorize_channel(&state, &user, channel_id).await?;
    let start = request
        .start_date
        .unwrap_or(row.try_get::<DateTime<Utc>, _>("start_at")?);
    let end = request
        .end_date
        .unwrap_or(row.try_get::<DateTime<Utc>, _>("end_at")?);
    let pre = request
        .pre_padding_seconds
        .unwrap_or(row.try_get("padding_before_seconds")?);
    let post = request
        .post_padding_seconds
        .unwrap_or(row.try_get("padding_after_seconds")?);
    validate_series_window(start, end, pre, post)?;
    let current_days: i16 = row.try_get("days_mask")?;
    let days_mask = if request.days.is_some() || request.day_pattern.is_some() {
        parse_series_days(request.days.as_deref(), request.day_pattern.as_deref())?
    } else {
        current_days
    };
    let current_name: String = row.try_get("name")?;
    let name = request
        .name
        .as_deref()
        .map(sanitize_timer_name)
        .transpose()?
        .unwrap_or(current_name);
    let output_library = request
        .output_library_id
        .unwrap_or(row.try_get("output_library_id")?);
    if !db::library_visible_to_user(&state.db, &user, output_library).await? {
        return Err(ApiError::NotFound);
    }
    sqlx::query(
        "UPDATE live_tv_timers SET status='cancelled',series_timer_id=NULL,finished_at=NOW(), \
         last_error_code='cancelled',updated_at=NOW() \
         WHERE series_timer_id=$1 AND status='scheduled'",
    )
    .bind(timer_id)
    .execute(&mut *tx)
    .await?;
    sqlx::query(
        "UPDATE live_tv_series_timers SET name=$3,start_at=$4,end_at=$5,days_mask=$6, \
         padding_before_seconds=$7,padding_after_seconds=$8,output_library_id=$9,updated_at=NOW() \
         WHERE id=$1 AND owner_user_id=$2 AND enabled=TRUE",
    )
    .bind(timer_id)
    .bind(user.id)
    .bind(name)
    .bind(start)
    .bind(end)
    .bind(days_mask)
    .bind(pre)
    .bind(post)
    .bind(output_library)
    .execute(&mut *tx)
    .await?;
    materialize_series_timers(&mut tx, None, Some(timer_id)).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn cancel_series_timer(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(timer_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let mut tx = state.db.begin().await?;
    let user = lock_live_tv_mutation_user(&mut tx, state.run_id, user.id, true).await?;
    let row = sqlx::query(
        "SELECT channel_item_id,output_library_id FROM live_tv_series_timers \
         WHERE id=$1 AND owner_user_id=$2 AND enabled=TRUE FOR UPDATE",
    )
    .bind(timer_id)
    .bind(user.id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::NotFound)?;
    let channel_id: Uuid = row.try_get("channel_item_id")?;
    let output_library: Uuid = row.try_get("output_library_id")?;
    authorize_channel(&state, &user, channel_id).await?;
    if !db::library_visible_to_user(&state.db, &user, output_library).await? {
        return Err(ApiError::NotFound);
    }
    sqlx::query("UPDATE live_tv_series_timers SET enabled=FALSE,updated_at=NOW() WHERE id=$1")
        .bind(timer_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query(
        "UPDATE live_tv_timers SET status='cancelled',finished_at=NOW(),last_error_code='cancelled',updated_at=NOW() \
         WHERE series_timer_id=$1 AND status='scheduled'",
    )
    .bind(timer_id)
    .execute(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

fn validate_series_options(request: &SeriesTimerRequest) -> Result<(), ApiError> {
    if request.record_any_time == Some(false)
        || request.record_any_channel == Some(true)
        || request.skip_episodes_in_library == Some(true)
        || request.record_new_only == Some(true)
        || request.keep_up_to.is_some_and(|value| value != 0)
        || request
            .keep_until
            .as_deref()
            .is_some_and(|value| !value.eq_ignore_ascii_case("UntilDeleted"))
        || request.priority.is_some_and(|priority| priority != 0)
    {
        return Err(ApiError::BadRequest(
            "This IPTV series timer supports exact-title, same-channel repeat capture only"
                .to_owned(),
        ));
    }
    Ok(())
}

fn validate_series_window(
    start: DateTime<Utc>,
    end: DateTime<Utc>,
    pre: i32,
    post: i32,
) -> Result<(), ApiError> {
    let now = Utc::now();
    if start <= now - Duration::minutes(2)
        || start > now + MAX_GUIDE_WINDOW
        || end <= start
        || end - start + Duration::seconds(i64::from(pre) + i64::from(post)) > MAX_TIMER_DURATION
        || !(0..=3600).contains(&pre)
        || !(0..=3600).contains(&post)
    {
        return Err(ApiError::BadRequest(
            "Series timer anchor is outside supported schedule bounds".to_owned(),
        ));
    }
    Ok(())
}

fn parse_series_days(days: Option<&[String]>, day_pattern: Option<&str>) -> Result<i16, ApiError> {
    if days.is_some() && day_pattern.is_some() {
        return Err(ApiError::BadRequest(
            "Specify either Days or DayPattern, not both".to_owned(),
        ));
    }
    if let Some(pattern) = day_pattern {
        if pattern.eq_ignore_ascii_case("Daily") {
            return Ok(0b111_1111);
        }
        if pattern.eq_ignore_ascii_case("Weekdays") {
            return Ok(0b011_1110);
        }
        if pattern.eq_ignore_ascii_case("Weekends") {
            return Ok(0b100_0001);
        }
        return Err(ApiError::BadRequest("Unsupported DayPattern".to_owned()));
    }
    let Some(days) = days else {
        return Ok(0b111_1111);
    };
    if days.is_empty() {
        return Ok(0b111_1111);
    }
    if days.len() > 7 {
        return Err(ApiError::BadRequest(
            "Days must contain at most seven values".to_owned(),
        ));
    }
    let mut mask = 0_i16;
    for day in days {
        let index = [
            "Sunday",
            "Monday",
            "Tuesday",
            "Wednesday",
            "Thursday",
            "Friday",
            "Saturday",
        ]
        .iter()
        .position(|candidate| candidate.eq_ignore_ascii_case(day))
        .ok_or_else(|| ApiError::BadRequest("Days contains an unknown weekday".to_owned()))?;
        let bit = 1_i16 << index;
        if mask & bit != 0 {
            return Err(ApiError::BadRequest(
                "Days contains a duplicate weekday".to_owned(),
            ));
        }
        mask |= bit;
    }
    Ok(mask)
}

fn validate_series_sort(query: &SeriesTimerListQuery) -> Result<(), ApiError> {
    if query.sort_by.as_deref().is_some_and(|sort| {
        !sort.eq_ignore_ascii_case("SortName") && !sort.eq_ignore_ascii_case("Priority")
    }) || query.sort_order.as_deref().is_some_and(|order| {
        !order.eq_ignore_ascii_case("Ascending") && !order.eq_ignore_ascii_case("Descending")
    }) {
        return Err(ApiError::BadRequest(
            "Unsupported series timer sort".to_owned(),
        ));
    }
    Ok(())
}

fn series_timer_query(
    user: &UserRecord,
    id: Option<Uuid>,
    count_only: bool,
) -> QueryBuilder<'static, Postgres> {
    let select = if count_only {
        "SELECT COUNT(*)::BIGINT"
    } else {
        "SELECT s.id,s.channel_item_id,s.program_id,s.name,s.start_at,s.end_at,s.days_mask, \
         s.padding_before_seconds,s.padding_after_seconds,s.output_library_id,c.name AS channel_name"
    };
    let mut query = QueryBuilder::<Postgres>::new(select);
    query.push(
        " FROM live_tv_series_timers s JOIN live_tv_channels c ON c.item_id=s.channel_item_id \
         JOIN items i ON i.id=c.item_id JOIN libraries channel_library ON channel_library.id=c.library_id \
         JOIN libraries output_library ON output_library.id=s.output_library_id \
         LEFT JOIN item_metadata channel_metadata ON channel_metadata.item_id=i.id \
         AND channel_metadata.provider_key='local-nfo' AND channel_metadata.policy_rating_scale='US-MPAA-v1' \
         WHERE s.owner_user_id=",
    )
    .push_bind(user.id)
    .push(" AND s.enabled=TRUE AND c.enabled=TRUE AND channel_library.enabled=TRUE \
           AND output_library.enabled=TRUE AND i.item_type='LiveTvChannel' \
           AND i.path LIKE 'puffinbox://livetv/%'");
    append_library_policy(&mut query, user, "s.output_library_id");
    append_library_policy(&mut query, user, "c.library_id");
    if let Some(id) = id {
        query.push(" AND s.id=").push_bind(id);
    }
    if !user.is_admin {
        append_rating_policy(
            &mut query,
            user,
            "channel_metadata.policy_rating_value",
            "LiveTvChannel",
        );
        append_rating_policy(&mut query, user, "s.policy_rating_value", "LiveTvProgram");
    }
    query
}

fn series_timer_dto(
    row: &sqlx::postgres::PgRow,
    server_id: Uuid,
) -> Result<SeriesTimerDto, ApiError> {
    let mask: i16 = row.try_get("days_mask")?;
    let (days, day_pattern) = if mask == 0b111_1111 {
        (None, Some("Daily"))
    } else if mask == 0b011_1110 {
        (None, Some("Weekdays"))
    } else if mask == 0b100_0001 {
        (None, Some("Weekends"))
    } else {
        let names = [
            "Sunday",
            "Monday",
            "Tuesday",
            "Wednesday",
            "Thursday",
            "Friday",
            "Saturday",
        ];
        (
            Some(
                names
                    .iter()
                    .enumerate()
                    .filter(|(index, _)| mask & (1_i16 << index) != 0)
                    .map(|(_, name)| *name)
                    .collect(),
            ),
            None,
        )
    };
    Ok(SeriesTimerDto {
        id: row.try_get("id")?,
        type_name: "SeriesTimerInfoDto",
        server_id,
        external_id: None,
        channel_id: row.try_get("channel_item_id")?,
        external_channel_id: None,
        channel_name: row.try_get("channel_name")?,
        program_id: row.try_get("program_id")?,
        external_program_id: None,
        name: row.try_get("name")?,
        start_date: row.try_get("start_at")?,
        end_date: row.try_get("end_at")?,
        service_name: None,
        priority: 0,
        pre_padding_seconds: row.try_get("padding_before_seconds")?,
        post_padding_seconds: row.try_get("padding_after_seconds")?,
        is_pre_padding_required: false,
        is_post_padding_required: false,
        keep_until: "UntilDeleted",
        record_any_time: true,
        skip_episodes_in_library: false,
        record_any_channel: false,
        keep_up_to: 0,
        record_new_only: false,
        days,
        day_pattern,
        output_library_id: row.try_get("output_library_id")?,
    })
}

async fn materialize_series_timers(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    source_id: Option<Uuid>,
    series_timer_id: Option<Uuid>,
) -> Result<u64, sqlx::Error> {
    if source_id.is_none() && series_timer_id.is_none() {
        return Ok(0);
    }
    lock_series_materialization_users(tx, source_id, series_timer_id).await?;

    let mut lock_query = QueryBuilder::<Postgres>::new(
        "SELECT s.id FROM live_tv_series_timers s JOIN live_tv_channels c ON c.item_id=s.channel_item_id",
    );
    lock_query.push(" WHERE s.enabled=TRUE");
    if let Some(source_id) = source_id {
        lock_query.push(" AND c.source_id=").push_bind(source_id);
    }
    if let Some(series_timer_id) = series_timer_id {
        lock_query.push(" AND s.id=").push_bind(series_timer_id);
    }
    lock_query.push(" ORDER BY s.id FOR UPDATE OF s");
    // Rule edits/cancellation take this same lock. Keep the lock in this
    // transaction and run the materialization as a separate statement so its
    // READ COMMITTED snapshot observes whichever edit won the row lock.
    let _locked_rules = lock_query
        .build_query_scalar::<Uuid>()
        .fetch_all(&mut **tx)
        .await?;

    // Guide refreshes can null a manual timer's program FK. Keep the channel
    // and airing interval as a stable duplicate key for series materialization.
    sqlx::query(
        "WITH ranked AS ( \
           SELECT s.id AS series_timer_id,s.owner_user_id,p.channel_item_id,p.id AS program_id, \
             p.start_at,p.end_at,s.padding_before_seconds,s.padding_after_seconds, \
             s.output_library_id,p.rating_system,p.content_rating,p.policy_rating_scale, \
             p.policy_rating_value,p.title, \
             (SELECT COUNT(*) FROM live_tv_timers pending \
              WHERE pending.series_timer_id=s.id AND pending.status IN ('scheduled','recording') \
                AND pending.end_at > NOW()) AS active_airings, \
             ROW_NUMBER() OVER (PARTITION BY s.id ORDER BY p.start_at,p.id) AS airing_number \
           FROM live_tv_series_timers s \
           JOIN live_tv_channels c ON c.item_id=s.channel_item_id AND c.enabled=TRUE \
           JOIN live_tv_sources source ON source.id=c.source_id AND source.enabled=TRUE \
           JOIN libraries channel_library ON channel_library.id=c.library_id AND channel_library.enabled=TRUE \
           JOIN libraries output_library ON output_library.id=s.output_library_id AND output_library.enabled=TRUE \
           JOIN items i ON i.id=c.item_id AND i.item_type='LiveTvChannel' \
             AND i.path LIKE 'puffinbox://livetv/%' \
           LEFT JOIN item_metadata channel_metadata ON channel_metadata.item_id=i.id \
             AND channel_metadata.provider_key='local-nfo' \
             AND channel_metadata.policy_rating_scale='US-MPAA-v1' \
           JOIN users u ON u.id=s.owner_user_id AND u.disabled=FALSE \
             AND u.allow_media_playback=TRUE \
             AND (u.is_admin=TRUE OR (u.enable_live_tv_access=TRUE \
               AND u.enable_live_tv_management=TRUE)) \
           JOIN live_tv_programs p ON p.channel_item_id=c.item_id \
           WHERE s.enabled=TRUE AND ($1::uuid IS NULL OR source.id=$1) \
             AND ($2::uuid IS NULL OR s.id=$2) \
             AND p.start_at >= NOW()-INTERVAL '120 seconds' \
             AND p.start_at < NOW()+INTERVAL '31 days' AND p.end_at > NOW() \
             AND lower(regexp_replace(btrim(p.title),'[[:space:]]+',' ','g'))=s.match_title_key \
             AND (s.days_mask::integer & (1 << EXTRACT(DOW FROM p.start_at)::integer)) <> 0 \
             AND p.end_at-p.start_at \
               + (s.padding_before_seconds+s.padding_after_seconds)*INTERVAL '1 second' <= INTERVAL '4 hours' \
             AND (u.is_admin OR NOT u.restrict_libraries OR ( \
               EXISTS(SELECT 1 FROM user_library_access a WHERE a.user_id=u.id AND a.library_id=c.library_id) \
               AND EXISTS(SELECT 1 FROM user_library_access a WHERE a.user_id=u.id AND a.library_id=s.output_library_id))) \
             AND (u.is_admin OR ( \
               (u.max_parental_rating IS NULL OR p.policy_rating_value IS NULL OR p.policy_rating_value<=u.max_parental_rating) \
               AND ('LiveTvProgram' <> ALL(u.block_unrated_items) OR p.policy_rating_value IS NOT NULL) \
               AND (u.max_parental_rating IS NULL OR channel_metadata.policy_rating_value IS NULL \
                    OR channel_metadata.policy_rating_value<=u.max_parental_rating) \
               AND ('LiveTvChannel' <> ALL(u.block_unrated_items) OR channel_metadata.policy_rating_value IS NOT NULL))) \
             AND NOT EXISTS (SELECT 1 FROM live_tv_timers old \
               WHERE old.owner_user_id=s.owner_user_id AND old.status<>'cancelled' \
                 AND (old.program_id=p.id OR (old.channel_item_id=p.channel_item_id \
                   AND old.start_at=p.start_at AND old.end_at=p.end_at))) \
             AND NOT EXISTS (SELECT 1 FROM live_tv_timers skipped \
               WHERE skipped.series_timer_id=s.id AND skipped.status='cancelled' \
                 AND skipped.start_at=p.start_at AND skipped.end_at=p.end_at \
                 AND lower(regexp_replace(btrim(COALESCE(skipped.display_name,'')),'[[:space:]]+',' ','g'))=s.match_title_key) \
         ) \
         INSERT INTO live_tv_timers( \
           id,owner_user_id,channel_item_id,program_id,series_timer_id,start_at,end_at, \
           padding_before_seconds,padding_after_seconds,output_library_id,rating_system,content_rating, \
           policy_rating_scale,policy_rating_value,display_name) \
         SELECT gen_random_uuid(),owner_user_id,channel_item_id,program_id,series_timer_id,start_at,end_at, \
           padding_before_seconds,padding_after_seconds,output_library_id,rating_system,content_rating, \
           policy_rating_scale,policy_rating_value,title \
         FROM ranked WHERE airing_number+active_airings <= $3 \
         ORDER BY start_at,series_timer_id LIMIT $4 ON CONFLICT DO NOTHING",
    )
    .bind(source_id)
    .bind(series_timer_id)
    .bind(MAX_QUEUED_SERIES_AIRINGS)
    .bind(MAX_SERIES_MATERIALIZE_BATCH)
    .execute(&mut **tx)
    .await
    .map(|result| result.rows_affected())
}

async fn lock_series_materialization_users(
    tx: &mut sqlx::Transaction<'_, Postgres>,
    source_id: Option<Uuid>,
    series_timer_id: Option<Uuid>,
) -> Result<(), sqlx::Error> {
    // Keep the global lock order user→series for timer mutations, refreshes,
    // and materialization. A source refresh locks all users in stable order
    // before changing guide rows: otherwise a newly granted user could hold
    // its row while waiting on a program FK update, as refresh waits on that
    // same row. Serialize user creation/policy updates too, so no new user can
    // appear between the all-user snapshot and guide replacement. This is a
    // short database-only phase; feed I/O is already done.
    if source_id.is_some() {
        sqlx::query("SELECT pg_advisory_xact_lock(82473011)")
            .execute(&mut **tx)
            .await?;
    }
    let mut query = QueryBuilder::<Postgres>::new("SELECT u.id FROM users u WHERE ");
    if let Some(series_timer_id) = series_timer_id {
        query
            .push(
                "EXISTS(SELECT 1 FROM live_tv_series_timers s WHERE s.owner_user_id=u.id AND s.id=",
            )
            .push_bind(series_timer_id)
            .push(")");
    } else if source_id.is_some() {
        query.push("TRUE");
    }
    query.push(" ORDER BY u.id FOR UPDATE OF u");
    let _locked_users = query
        .build_query_scalar::<Uuid>()
        .fetch_all(&mut **tx)
        .await?;
    Ok(())
}

fn timer_query(
    user: &UserRecord,
    filters: &TimerListQuery,
    count_only: bool,
) -> QueryBuilder<'static, Postgres> {
    let select = if count_only {
        "SELECT COUNT(*)::BIGINT"
    } else {
        "SELECT t.id,t.channel_item_id,t.program_id,t.series_timer_id,t.start_at,t.end_at, \
         t.padding_before_seconds,t.padding_after_seconds,t.status,t.output_library_id, \
         t.last_error_code,t.content_rating,COALESCE(p.title,t.display_name,c.name) AS timer_name"
    };
    let mut query = QueryBuilder::<Postgres>::new(select);
    query.push(
        " FROM live_tv_timers t JOIN live_tv_channels c ON c.item_id=t.channel_item_id \
         JOIN items i ON i.id=c.item_id JOIN libraries channel_library ON channel_library.id=c.library_id \
         JOIN libraries output_library ON output_library.id=t.output_library_id \
         LEFT JOIN live_tv_programs p ON p.id=t.program_id \
         LEFT JOIN item_metadata channel_metadata ON channel_metadata.item_id=i.id \
         AND channel_metadata.provider_key='local-nfo' AND channel_metadata.policy_rating_scale='US-MPAA-v1' \
         WHERE t.owner_user_id = ",
    )
    .push_bind(user.id)
    .push(" AND t.status<>'cancelled' AND c.enabled=TRUE AND channel_library.enabled=TRUE \
           AND output_library.enabled=TRUE AND i.item_type='LiveTvChannel' \
           AND i.path LIKE 'puffinbox://livetv/%'");
    append_library_policy(&mut query, user, "t.output_library_id");
    append_library_policy(&mut query, user, "c.library_id");
    if let Some(channel_id) = filters.channel_id {
        query
            .push(" AND t.channel_item_id = ")
            .push_bind(channel_id);
    }
    if let Some(series_timer_id) = filters.series_timer_id {
        query
            .push(" AND t.series_timer_id = ")
            .push_bind(series_timer_id);
    }
    if let Some(is_active) = filters.is_active {
        query
            .push(" AND (t.status = 'recording') = ")
            .push_bind(is_active);
    }
    if let Some(is_scheduled) = filters.is_scheduled {
        query
            .push(" AND (t.status = 'scheduled') = ")
            .push_bind(is_scheduled);
    }
    if !user.is_admin {
        append_rating_policy(
            &mut query,
            user,
            "channel_metadata.policy_rating_value",
            "LiveTvChannel",
        );
        append_rating_policy(&mut query, user, "t.policy_rating_value", "LiveTvProgram");
    }
    query
}

async fn get_timer(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(timer_id): Path<Uuid>,
) -> Result<Json<TimerDto>, ApiError> {
    require_live_tv_access(&user)?;
    let row = sqlx::query(
        "SELECT t.id,t.channel_item_id,t.program_id,t.series_timer_id,t.start_at,t.end_at,t.padding_before_seconds, \
         t.padding_after_seconds,t.status,t.output_library_id,t.last_error_code,t.content_rating, \
         t.policy_rating_value,COALESCE(p.title,t.display_name,c.name) AS timer_name \
         FROM live_tv_timers t JOIN live_tv_channels c ON c.item_id=t.channel_item_id \
         LEFT JOIN live_tv_programs p ON p.id=t.program_id WHERE t.id=$1 AND t.owner_user_id=$2",
    )
    .bind(timer_id)
    .bind(user.id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(ApiError::NotFound)?;
    let channel_id: Uuid = row.try_get("channel_item_id")?;
    authorize_channel(&state, &user, channel_id).await?;
    let policy_rating: Option<i16> = row.try_get("policy_rating_value")?;
    if !rating_visible(&user, policy_rating, "LiveTvProgram") {
        return Err(ApiError::NotFound);
    }
    let output_library_id: Uuid = row.try_get("output_library_id")?;
    if !db::library_visible_to_user(&state.db, &user, output_library_id).await? {
        return Err(ApiError::NotFound);
    }
    Ok(Json(timer_dto(&row)?))
}

async fn cancel_timer(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(timer_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let mut tx = state.db.begin().await?;
    let user = lock_live_tv_mutation_user(&mut tx, state.run_id, user.id, true).await?;
    let row = sqlx::query(
        "SELECT channel_item_id,status FROM live_tv_timers \
         WHERE id=$1 AND owner_user_id=$2 AND status IN ('scheduled','recording') FOR UPDATE",
    )
    .bind(timer_id)
    .bind(user.id)
    .fetch_optional(&mut *tx)
    .await?
    .ok_or(ApiError::NotFound)?;
    let channel_id: Uuid = row.try_get("channel_item_id")?;
    let timer_status: String = row.try_get("status")?;
    if timer_status == "recording" {
        let recording =
            sqlx::query("SELECT status FROM live_tv_recordings WHERE timer_id=$1 FOR UPDATE")
                .bind(timer_id)
                .fetch_optional(&mut *tx)
                .await?;
        if recording
            .as_ref()
            .map(|row| row.try_get::<String, _>("status"))
            .transpose()?
            .as_deref()
            == Some("publishing")
        {
            return Err(ApiError::Conflict(
                "Recording publication has already started".to_owned(),
            ));
        }
    }
    let Some(item) = db::get_item(&state.db, channel_id).await? else {
        return Err(ApiError::NotFound);
    };
    if !db::item_visible_to_user(&state.db, &user, &item).await? {
        return Err(ApiError::NotFound);
    }
    let changed = sqlx::query(
        "UPDATE live_tv_timers SET status='cancelled',claimed_run_id=NULL,started_at=NULL, \
         finished_at=NOW(),last_error_code='cancelled',updated_at=NOW() \
         WHERE id=$1 AND owner_user_id=$2 AND status IN ('scheduled','recording')",
    )
    .bind(timer_id)
    .bind(user.id)
    .execute(&mut *tx)
    .await?
    .rows_affected();
    if changed != 1 {
        return Err(ApiError::Conflict(
            "Timer state changed during cancellation".to_owned(),
        ));
    }
    tx.commit().await?;
    super::cancel_livetv_recording(timer_id).await;
    Ok(StatusCode::NO_CONTENT)
}

fn sanitize_timer_name(input: &str) -> Result<String, ApiError> {
    let name = input.trim();
    if name.is_empty() || name.chars().count() > 512 || name.chars().any(char::is_control) {
        return Err(ApiError::BadRequest(
            "Timer Name must contain 1 to 512 printable characters".to_owned(),
        ));
    }
    Ok(name.to_owned())
}

fn timer_dto(row: &sqlx::postgres::PgRow) -> Result<TimerDto, ApiError> {
    let status: String = row.try_get("status")?;
    Ok(TimerDto {
        id: row.try_get("id")?,
        name: row.try_get("timer_name")?,
        channel_id: row.try_get("channel_item_id")?,
        program_id: row.try_get("program_id")?,
        series_timer_id: row.try_get("series_timer_id")?,
        start_date: row.try_get("start_at")?,
        end_date: row.try_get("end_at")?,
        pre_padding_seconds: row.try_get("padding_before_seconds")?,
        post_padding_seconds: row.try_get("padding_after_seconds")?,
        status: timer_status_dto(&status).to_owned(),
        output_library_id: row.try_get("output_library_id")?,
        last_error_code: row.try_get("last_error_code")?,
        official_rating: row.try_get("content_rating")?,
    })
}

fn timer_status_dto(status: &str) -> &'static str {
    match status {
        "scheduled" => "New",
        "recording" => "InProgress",
        "completed" => "Completed",
        "cancelled" => "Cancelled",
        "failed" | "interrupted" => "Error",
        _ => "Error",
    }
}

async fn list_recordings(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(query): Query<PageQuery>,
) -> Result<Json<QueryResult<RecordingDto>>, ApiError> {
    require_live_tv_access(&user)?;
    let (start, limit) = page(query)?;
    let total = recording_query(&user, true)
        .build_query_scalar::<i64>()
        .fetch_one(&state.db)
        .await?;
    let mut rows_query = recording_query(&user, false);
    rows_query
        .push(" ORDER BY r.started_at DESC,r.id LIMIT ")
        .push_bind(limit)
        .push(" OFFSET ")
        .push_bind(start);
    let rows = rows_query.build().fetch_all(&state.db).await?;
    let items = rows
        .into_iter()
        .map(|row| {
            Ok(RecordingDto {
                id: row.try_get("id")?,
                timer_id: row.try_get("timer_id")?,
                channel_id: row.try_get("channel_item_id")?,
                library_id: row.try_get("library_id")?,
                channel_name: row.try_get("channel_name")?,
                name: row.try_get("title")?,
                item_id: row.try_get("item_id")?,
                status: row.try_get("status")?,
                byte_count: row.try_get("byte_count")?,
                sha256: row.try_get("sha256")?,
                start_date: row.try_get("started_at")?,
                end_date: row.try_get("finished_at")?,
                error_code: row.try_get("last_error_code")?,
            })
        })
        .collect::<Result<Vec<_>, ApiError>>()?;
    Ok(Json(QueryResult {
        items,
        total_record_count: usize::try_from(total).unwrap_or(usize::MAX),
        start_index: start as usize,
    }))
}

fn recording_query(user: &UserRecord, count_only: bool) -> QueryBuilder<'static, Postgres> {
    let select = if count_only {
        "SELECT COUNT(*)::BIGINT"
    } else {
        "SELECT r.id,r.timer_id,r.channel_item_id,r.library_id,r.channel_name,r.title, \
         r.item_id,r.status,r.byte_count,r.sha256,r.started_at,r.finished_at,r.last_error_code"
    };
    let mut query = QueryBuilder::<Postgres>::new(select);
    query.push(
        " FROM live_tv_recordings r JOIN live_tv_timers t ON t.id=r.timer_id \
         JOIN live_tv_channels c ON c.item_id=r.channel_item_id \
         JOIN items i ON i.id=c.item_id JOIN libraries channel_library ON channel_library.id=c.library_id \
         JOIN libraries output_library ON output_library.id=r.library_id \
         LEFT JOIN item_metadata channel_metadata ON channel_metadata.item_id=i.id \
         AND channel_metadata.provider_key='local-nfo' AND channel_metadata.policy_rating_scale='US-MPAA-v1' \
         WHERE t.owner_user_id = ",
    )
    .push_bind(user.id)
    .push(" AND c.enabled=TRUE AND channel_library.enabled=TRUE AND output_library.enabled=TRUE \
           AND i.item_type='LiveTvChannel' AND i.path LIKE 'puffinbox://livetv/%'");
    append_library_policy(&mut query, user, "r.library_id");
    append_library_policy(&mut query, user, "c.library_id");
    if !user.is_admin {
        append_rating_policy(
            &mut query,
            user,
            "channel_metadata.policy_rating_value",
            "LiveTvChannel",
        );
        append_rating_policy(&mut query, user, "t.policy_rating_value", "LiveTvProgram");
    }
    query
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct RecordingDto {
    id: Uuid,
    timer_id: Uuid,
    channel_id: Uuid,
    library_id: Uuid,
    channel_name: String,
    name: String,
    item_id: Option<Uuid>,
    status: String,
    byte_count: i64,
    sha256: Option<String>,
    start_date: DateTime<Utc>,
    end_date: Option<DateTime<Utc>>,
    error_code: Option<String>,
}

async fn list_tuner_hosts(
    CurrentUser(user): CurrentUser,
) -> Result<Json<QueryResult<Value>>, ApiError> {
    require_live_tv_access(&user)?;
    Ok(Json(QueryResult {
        items: Vec::new(),
        total_record_count: 0,
        start_index: 0,
    }))
}

pub(super) async fn authorize_channel(
    state: &AppState,
    user: &UserRecord,
    item_id: Uuid,
) -> Result<(), ApiError> {
    require_live_tv_access(user)?;
    if user.disabled || !user.allow_media_playback {
        return Err(ApiError::Forbidden);
    }
    let enabled: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM live_tv_channels WHERE item_id=$1 AND enabled=TRUE)",
    )
    .bind(item_id)
    .fetch_one(&state.db)
    .await?;
    if !enabled {
        return Err(ApiError::NotFound);
    }
    let item = db::get_item(&state.db, item_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if item.item_type != "LiveTvChannel"
        || !db::item_visible_to_user(&state.db, user, &item).await?
    {
        return Err(ApiError::NotFound);
    }
    Ok(())
}

pub(super) async fn authorize_live_channel(
    state: &AppState,
    user: &UserRecord,
    item_id: Uuid,
) -> Result<(), ApiError> {
    authorize_channel(state, user, item_id).await?;
    let current_program_rating = sqlx::query_scalar::<_, Option<i16>>(
        "SELECT p.policy_rating_value FROM live_tv_programs p \
         WHERE p.channel_item_id=$1 AND p.start_at <= NOW() AND p.end_at > NOW() \
         ORDER BY p.start_at DESC,p.id LIMIT 1",
    )
    .bind(item_id)
    .fetch_optional(&state.db)
    .await?
    .flatten();
    if !rating_visible(user, current_program_rating, "LiveTvProgram") {
        return Err(ApiError::NotFound);
    }
    Ok(())
}

fn blocks_unrated(user: &UserRecord, category: &str) -> bool {
    !user.is_admin
        && user
            .block_unrated_items
            .iter()
            .any(|blocked| blocked == category)
}

fn rating_visible(user: &UserRecord, rating: Option<i16>, category: &str) -> bool {
    if user.is_admin {
        return true;
    }
    if user
        .max_parental_rating
        .is_some_and(|maximum| rating.is_some_and(|rating| i32::from(rating) > maximum))
    {
        return false;
    }
    rating.is_some() || !blocks_unrated(user, category)
}

pub(super) async fn channel_source(
    state: &AppState,
    user: &UserRecord,
    item_id: Uuid,
) -> Result<(String, Vec<OriginPin>, String), ApiError> {
    authorize_live_channel(state, user, item_id).await?;
    let row = sqlx::query(
        "SELECT c.stream_url,s.origin_pins,i.name FROM live_tv_channels c \
         JOIN live_tv_sources s ON s.id=c.source_id JOIN items i ON i.id=c.item_id \
         WHERE c.item_id=$1 AND c.enabled=TRUE AND s.enabled=TRUE",
    )
    .bind(item_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(ApiError::NotFound)?;
    let pins: SqlJson<Vec<OriginPin>> = row.try_get("origin_pins")?;
    Ok((row.try_get("stream_url")?, pins.0, row.try_get("name")?))
}

fn stable_channel_id(source_id: Uuid, channel_key: &str) -> Uuid {
    let mut hash = Sha256::new();
    hash.update(source_id.as_bytes());
    hash.update([0]);
    hash.update(channel_key.as_bytes());
    uuid_from_hash(hash.finalize().into())
}

fn stable_program_id(channel_id: Uuid, start_micros: i64, title: &str) -> Uuid {
    let mut hash = Sha256::new();
    hash.update(channel_id.as_bytes());
    hash.update(start_micros.to_be_bytes());
    hash.update([0]);
    hash.update(title.as_bytes());
    uuid_from_hash(hash.finalize().into())
}

fn uuid_from_hash(digest: [u8; 32]) -> Uuid {
    let mut bytes = [0_u8; 16];
    bytes.copy_from_slice(&digest[..16]);
    bytes[6] = (bytes[6] & 0x0f) | 0x50;
    bytes[8] = (bytes[8] & 0x3f) | 0x80;
    Uuid::from_bytes(bytes)
}

fn stable_digest(value: &str) -> String {
    let digest = Sha256::digest(value.as_bytes());
    use std::fmt::Write as _;

    let mut output = String::with_capacity(32);
    for byte in &digest[..16] {
        let _ = write!(output, "{byte:02x}");
    }
    output
}

fn feed_error(error: FeedError) -> ApiError {
    match error {
        FeedError::Unavailable => ApiError::Unavailable,
        FeedError::TooLarge
        | FeedError::InvalidEncoding
        | FeedError::InvalidStructure
        | FeedError::InvalidValue
        | FeedError::DisallowedOrigin
        | FeedError::LimitExceeded
        | FeedError::DuplicateId
        | FeedError::UnsupportedTransport
        | FeedError::QuotaExceeded
        | FeedError::Cancelled => ApiError::BadRequest("Configured feed is invalid".to_owned()),
    }
}

#[cfg(test)]
mod tests {
    use super::{
        PatchField, SourceUpdateRequest, rating_visible, stable_channel_id, stable_program_id,
        url_uses_approved_origin,
    };
    use crate::auth::UserRecord;
    use serde_json::json;
    use uuid::Uuid;

    #[test]
    fn channel_and_program_ids_are_stable_and_domain_separated() {
        let source = Uuid::parse_str("123e4567-e89b-42d3-a456-426614174000").unwrap();
        let first = stable_channel_id(source, "news");
        assert_eq!(first, stable_channel_id(source, "news"));
        assert_ne!(first, stable_channel_id(source, "sports"));
        assert_eq!(first.get_version_num(), 5);
        let program = stable_program_id(first, 1_799_991_000_000_000, "Evening News");
        assert_eq!(
            program,
            stable_program_id(first, 1_799_991_000_000_000, "Evening News")
        );
        assert_ne!(program, first);
    }

    #[test]
    fn source_url_validation_matches_scheme_host_port_without_credentials() {
        let allowed = vec![url::Url::parse("http://127.0.0.1:1234/").unwrap()];
        assert!(url_uses_approved_origin(
            "http://127.0.0.1:1234/playlist",
            &allowed
        ));
        assert!(!url_uses_approved_origin(
            "http://127.0.0.1:1235/playlist",
            &allowed
        ));
        assert!(!url_uses_approved_origin(
            "http://user@127.0.0.1:1234/playlist",
            &allowed
        ));
        assert!(!url_uses_approved_origin(
            "http://127.0.0.1:1234/playlist#x",
            &allowed
        ));
    }

    #[test]
    fn source_update_distinguishes_omitted_guide_from_explicit_clear() {
        let omitted: SourceUpdateRequest =
            serde_json::from_value(json!({ "Name": "Renamed" })).unwrap();
        assert!(matches!(omitted.guide_url, PatchField::Missing));

        let cleared: SourceUpdateRequest =
            serde_json::from_value(json!({ "GuideUrl": null })).unwrap();
        assert!(matches!(cleared.guide_url, PatchField::Present(None)));

        let replaced: SourceUpdateRequest =
            serde_json::from_value(json!({ "GuideUrl": "https://guide.example/listings.xml" }))
                .unwrap();
        assert!(matches!(
            replaced.guide_url,
            PatchField::Present(Some(url)) if url == "https://guide.example/listings.xml"
        ));
    }

    #[test]
    fn program_rating_policy_blocks_explicit_and_unrated_content_as_configured() {
        let user = UserRecord {
            id: Uuid::new_v4(),
            username: "viewer".to_owned(),
            is_admin: false,
            disabled: false,
            enable_remote_access: false,
            allow_media_playback: true,
            sync_play_access: Default::default(),
            enable_content_downloading: false,
            enable_live_tv_access: false,
            enable_live_tv_management: false,
            restrict_libraries: false,
            max_parental_rating: Some(75),
            block_unrated_items: Vec::new(),
            allowed_library_ids: Vec::new(),
            configuration: Default::default(),
        };
        assert!(rating_visible(&user, Some(75), "LiveTvProgram"));
        assert!(!rating_visible(&user, Some(100), "LiveTvProgram"));
        assert!(rating_visible(&user, None, "LiveTvProgram"));

        let mut blocks_unrated = user.clone();
        blocks_unrated
            .block_unrated_items
            .push("LiveTvProgram".to_owned());
        assert!(!rating_visible(&blocks_unrated, None, "LiveTvProgram"));
        assert!(rating_visible(&blocks_unrated, Some(75), "LiveTvProgram"));

        let mut admin = blocks_unrated;
        admin.is_admin = true;
        assert!(rating_visible(&admin, Some(100), "LiveTvProgram"));
        assert!(rating_visible(&admin, None, "LiveTvProgram"));
    }
}
