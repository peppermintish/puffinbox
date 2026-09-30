//! Durable, quota-reserved offline media packages.
//!
//! The server stores a verified copy in bounded one-megabyte chunks. Clients
//! can resume by requesting aligned ranges and checking each chunk digest.

use std::{
    fs::{self, File},
    io::{self, Read, Seek, SeekFrom},
    os::unix::fs::PermissionsExt,
    path::Path,
    sync::{Arc, OnceLock, atomic::Ordering},
    time::Duration,
};

use axum::{
    Json, Router,
    body::Body,
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, HeaderValue, StatusCode, header},
    response::Response,
    routing::{get, put},
};
use cap_std::fs::{Dir, OpenOptions as CapOpenOptions, OpenOptionsExt as CapOpenOptionsExt};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::{Postgres, QueryBuilder, Row, Transaction};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{OwnedSemaphorePermit, Semaphore},
};
use uuid::Uuid;

use crate::{
    ApiError,
    auth::{AdminUser, CurrentUser, UserRecord},
    db,
    library::ItemRecord,
    media_features,
    state::AppState,
};

pub const OFFLINE_CHUNK_BYTES: usize = 1024 * 1024;
pub const OFFLINE_MAX_ITEM_BYTES: i64 = 8 * 1024 * 1024 * 1024;
pub const OFFLINE_DEFAULT_USER_QUOTA: i64 = 20 * 1024 * 1024 * 1024;
pub const OFFLINE_MAX_USER_QUOTA: i64 = 1024 * 1024 * 1024 * 1024;
pub const OFFLINE_DEFAULT_GLOBAL_QUOTA: i64 = 1024 * 1024 * 1024 * 1024;
const MAX_ACTIVE_PACKAGES_PER_USER: i64 = 1000;
const SWEEP_ENTRIES_PER_TICK: usize = 128;
const MAX_CONCURRENT_CHUNK_READS: usize = 8;
const MAX_CONTENT_TYPE: &str = "application/octet-stream";
static CHUNK_READ_SLOTS: OnceLock<Arc<Semaphore>> = OnceLock::new();

pub fn router(state: AppState) -> Router<()> {
    Router::new()
        .route("/Puffinbox/Offline/Settings", get(settings))
        .route(
            "/Puffinbox/Offline/Packages",
            get(list_packages).post(queue_package),
        )
        .route(
            "/Puffinbox/Offline/Packages/{package_id}",
            get(get_package).delete(delete_package),
        )
        .route(
            "/Puffinbox/Offline/Packages/{package_id}/Content",
            get(download_package_chunk),
        )
        .route(
            "/Puffinbox/Offline/Users/{user_id}/Quota",
            put(set_user_quota),
        )
        .with_state(state)
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct SettingsDto {
    enabled: bool,
    quota_bytes: i64,
    reserved_bytes: i64,
    used_bytes: i64,
    cleanup_bytes: i64,
    max_item_bytes: i64,
    chunk_bytes: usize,
    max_active_packages: i64,
}

async fn settings(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
) -> Result<Json<SettingsDto>, ApiError> {
    let mut tx = state.db.begin().await?;
    db::require_active_run(&mut tx, state.run_id).await?;
    ensure_user_quota(&mut tx, user.id).await?;
    let row = sqlx::query(
        "SELECT quota_bytes,reserved_bytes,used_bytes,cleanup_bytes FROM offline_user_quotas WHERE user_id=$1",
    )
    .bind(user.id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok(Json(SettingsDto {
        enabled: user.enable_content_downloading,
        quota_bytes: row.try_get("quota_bytes")?,
        reserved_bytes: row.try_get("reserved_bytes")?,
        used_bytes: row.try_get("used_bytes")?,
        cleanup_bytes: row.try_get("cleanup_bytes")?,
        max_item_bytes: OFFLINE_MAX_ITEM_BYTES,
        chunk_bytes: OFFLINE_CHUNK_BYTES,
        max_active_packages: MAX_ACTIVE_PACKAGES_PER_USER,
    }))
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct PackageDto {
    id: Uuid,
    item_id: Uuid,
    library_id: Uuid,
    item_name: String,
    item_type: String,
    file_name: String,
    content_type: String,
    source_size: i64,
    bytes_copied: i64,
    status: String,
    sha256: Option<String>,
    chunk_count: i64,
    error_code: Option<String>,
    created_at: DateTime<Utc>,
    finished_at: Option<DateTime<Utc>>,
    content_url: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
struct PackageRequest {
    item_id: Uuid,
}

#[derive(Deserialize, Default)]
#[serde(rename_all = "PascalCase")]
struct PackageListQuery {
    start_index: Option<i64>,
    limit: Option<i64>,
}

async fn list_packages(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(query): Query<PackageListQuery>,
) -> Result<Json<Vec<PackageDto>>, ApiError> {
    let start = query.start_index.unwrap_or(0).clamp(0, i64::MAX / 2);
    let limit = query.limit.unwrap_or(100).clamp(1, 100);
    let rows = sqlx::query(
        "SELECT p.id,p.item_id,p.library_id,p.item_name,p.item_type,p.file_name,p.container,p.source_size,p.bytes_copied,p.status,p.sha256,p.error_code,p.created_at,p.finished_at,(SELECT count(*)::BIGINT FROM offline_package_chunks c WHERE c.package_id=p.id) AS chunk_count FROM offline_packages p WHERE p.user_id=$1 ORDER BY p.created_at DESC,p.id LIMIT $2 OFFSET $3",
    )
    .bind(user.id)
    .bind(limit)
    .bind(start)
    .fetch_all(&state.db)
    .await?;
    let mut visible = Vec::with_capacity(rows.len());
    for row in rows {
        if package_is_currently_allowed(&state, &user, &row).await? {
            visible.push(package_dto(row)?);
        }
    }
    Ok(Json(visible))
}

async fn get_package(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    AxumPath(package_id): AxumPath<Uuid>,
) -> Result<Json<PackageDto>, ApiError> {
    let row = sqlx::query(
        "SELECT p.id,p.item_id,p.library_id,p.item_name,p.item_type,p.file_name,p.container,p.source_size,p.bytes_copied,p.status,p.sha256,p.error_code,p.created_at,p.finished_at,(SELECT count(*)::BIGINT FROM offline_package_chunks c WHERE c.package_id=p.id) AS chunk_count FROM offline_packages p WHERE p.user_id=$1 AND p.id=$2",
    )
    .bind(user.id)
    .bind(package_id)
    .fetch_optional(&state.db)
    .await?
    .ok_or(ApiError::NotFound)?;
    if !package_is_currently_allowed(&state, &user, &row).await? {
        return Err(ApiError::NotFound);
    }
    Ok(Json(package_dto(row)?))
}

fn package_dto(row: sqlx::postgres::PgRow) -> Result<PackageDto, sqlx::Error> {
    let id: Uuid = row.try_get("id")?;
    let status: String = row.try_get("status")?;
    Ok(PackageDto {
        id,
        item_id: row.try_get("item_id")?,
        library_id: row.try_get("library_id")?,
        item_name: row.try_get("item_name")?,
        item_type: row.try_get("item_type")?,
        file_name: row.try_get("file_name")?,
        content_type: content_type_for(
            row.try_get("file_name")?,
            row.try_get("container")?,
            row.try_get("item_type")?,
        ),
        source_size: row.try_get("source_size")?,
        bytes_copied: row.try_get("bytes_copied")?,
        content_url: (status == "ready")
            .then(|| format!("/Puffinbox/Offline/Packages/{id}/Content")),
        status,
        sha256: row
            .try_get::<Option<String>, _>("sha256")?
            .map(|value| value.trim().to_owned()),
        chunk_count: row.try_get("chunk_count")?,
        error_code: row.try_get("error_code")?,
        created_at: row.try_get("created_at")?,
        finished_at: row.try_get("finished_at")?,
    })
}

async fn package_is_currently_allowed(
    state: &AppState,
    user: &UserRecord,
    row: &sqlx::postgres::PgRow,
) -> Result<bool, ApiError> {
    if !user.enable_content_downloading || !user.allow_media_playback || user.disabled {
        return Ok(false);
    }
    let item_id: Uuid = row.try_get("item_id")?;
    let library_id: Uuid = row.try_get("library_id")?;
    let Some(item) = db::get_item(&state.db, item_id).await? else {
        return Ok(false);
    };
    if item.library_id != library_id || !download_policy_allows(user, &item) {
        return Ok(false);
    }
    Ok(db::item_visible_to_user(&state.db, user, &item).await?)
}

fn content_type_for(file_name: &str, container: Option<String>, item_type: &str) -> String {
    let extension = Path::new(file_name)
        .extension()
        .and_then(|value| value.to_str())
        .map(str::to_ascii_lowercase)
        .or_else(|| container.map(|value| value.to_ascii_lowercase()));
    let mime = match extension.as_deref() {
        Some("mp4" | "m4v" | "m4a") => "video/mp4",
        Some("mkv") => "video/x-matroska",
        Some("webm") => "video/webm",
        Some("mov") => "video/quicktime",
        Some("avi") => "video/x-msvideo",
        Some("mp3") => "audio/mpeg",
        Some("aac") => "audio/aac",
        Some("flac") => "audio/flac",
        Some("ogg" | "oga") => "audio/ogg",
        Some("wav") => "audio/wav",
        Some("jpg" | "jpeg") => "image/jpeg",
        Some("png") => "image/png",
        Some("gif") => "image/gif",
        Some("webp") => "image/webp",
        Some("avif") => "image/avif",
        Some("pdf") if matches!(item_type, "Book" | "EBook" | "AudioBook") => "application/pdf",
        Some("epub") => "application/epub+zip",
        _ => MAX_CONTENT_TYPE,
    };
    mime.to_owned()
}

async fn queue_package(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Json(request): Json<PackageRequest>,
) -> Result<(StatusCode, Json<PackageDto>), ApiError> {
    if !user.enable_content_downloading {
        return Err(ApiError::Forbidden);
    }
    let mut tx = state.db.begin().await?;
    db::require_active_run(&mut tx, state.run_id).await?;
    let current_user = load_user_for_update(&mut tx, user.id)
        .await?
        .ok_or(ApiError::Unauthorized)?;
    let Some(item) = load_downloadable_item_for_share(&mut tx, request.item_id).await? else {
        return Err(ApiError::NotFound);
    };
    if !download_policy_allows(&current_user, &item) {
        return Err(if current_user.enable_content_downloading {
            ApiError::NotFound
        } else {
            ApiError::Forbidden
        });
    }
    let source_size = item
        .size_bytes
        .filter(|size| (1..=OFFLINE_MAX_ITEM_BYTES).contains(size))
        .ok_or_else(|| {
            ApiError::Conflict(
                "This item has no known size or exceeds the offline item limit".to_owned(),
            )
        })?;
    let source_modified_at = item.date_modified.ok_or_else(|| {
        ApiError::Conflict(
            "This item needs a completed library scan before offline caching".to_owned(),
        )
    })?;
    if let Some(existing) = sqlx::query(
        "SELECT p.id,p.item_id,p.library_id,p.item_name,p.item_type,p.file_name,p.container,p.source_size,p.bytes_copied,p.status,p.sha256,p.error_code,p.created_at,p.finished_at,(SELECT count(*)::BIGINT FROM offline_package_chunks c WHERE c.package_id=p.id) AS chunk_count FROM offline_packages p WHERE p.user_id=$1 AND p.item_id=$2 AND p.status IN ('queued','running','ready') FOR UPDATE OF p",
    )
    .bind(user.id)
    .bind(request.item_id)
    .fetch_optional(&mut *tx)
    .await?
    {
        tx.commit().await?;
        return Ok((StatusCode::OK, Json(package_dto(existing)?)));
    }
    ensure_global_quota(&mut tx).await?;
    let global_quota = sqlx::query(
        "SELECT quota_bytes,reserved_bytes,used_bytes,cleanup_bytes FROM offline_global_quota WHERE singleton=TRUE FOR UPDATE",
    )
    .fetch_one(&mut *tx)
    .await?;
    ensure_user_quota(&mut tx, user.id).await?;
    let user_quota = sqlx::query(
        "SELECT quota_bytes,reserved_bytes,used_bytes,cleanup_bytes FROM offline_user_quotas WHERE user_id=$1 FOR UPDATE",
    )
    .bind(user.id)
    .fetch_one(&mut *tx)
    .await?;
    let active_count: i64 = sqlx::query_scalar(
        "SELECT count(*)::BIGINT FROM offline_packages WHERE user_id=$1 AND status IN ('queued','running','ready')",
    )
    .bind(user.id)
    .fetch_one(&mut *tx)
    .await?;
    if active_count >= MAX_ACTIVE_PACKAGES_PER_USER {
        return Err(ApiError::Conflict(
            "The active offline package limit was reached".to_owned(),
        ));
    }
    let user_current: i64 = user_quota.try_get("reserved_bytes")?;
    let user_used: i64 = user_quota.try_get("used_bytes")?;
    let user_cleanup: i64 = user_quota.try_get("cleanup_bytes")?;
    let user_limit: i64 = user_quota.try_get("quota_bytes")?;
    if user_current + user_used + user_cleanup + source_size > user_limit {
        return Err(ApiError::Conflict(
            "The user offline storage quota would be exceeded".to_owned(),
        ));
    }
    let global_current: i64 = global_quota.try_get("reserved_bytes")?;
    let global_used: i64 = global_quota.try_get("used_bytes")?;
    let global_cleanup: i64 = global_quota.try_get("cleanup_bytes")?;
    let global_limit: i64 = global_quota.try_get("quota_bytes")?;
    if global_current + global_used + global_cleanup + source_size > global_limit {
        return Err(ApiError::Unavailable);
    }
    let id = Uuid::new_v4();
    let file_name = item_file_name(&item);
    let result = sqlx::query("INSERT INTO offline_packages(id,user_id,item_id,library_id,item_name,item_type,file_name,container,source_size,source_modified_at,reservation_bytes,status) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,$10,$9,'queued')")
        .bind(id)
        .bind(user.id)
        .bind(item.id)
        .bind(item.library_id)
        .bind(&item.name)
        .bind(&item.item_type)
        .bind(&file_name)
        .bind(&item.container)
        .bind(source_size)
        .bind(source_modified_at)
        .execute(&mut *tx)
        .await;
    if let Err(error) = result {
        if error
            .as_database_error()
            .is_some_and(|database| database.code().as_deref() == Some("23505"))
        {
            return Err(ApiError::Conflict(
                "An offline package for this item is already active".to_owned(),
            ));
        }
        return Err(error.into());
    }
    sqlx::query("UPDATE offline_user_quotas SET reserved_bytes=reserved_bytes+$2,updated_at=NOW() WHERE user_id=$1")
        .bind(user.id)
        .bind(source_size)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE offline_global_quota SET reserved_bytes=reserved_bytes+$1,updated_at=NOW() WHERE singleton=TRUE")
        .bind(source_size)
        .execute(&mut *tx)
        .await?;
    let row = sqlx::query(
        "SELECT id,item_id,library_id,item_name,item_type,file_name,container,source_size,bytes_copied,status,sha256,error_code,created_at,finished_at,0::BIGINT AS chunk_count FROM offline_packages WHERE id=$1",
    )
    .bind(id)
    .fetch_one(&mut *tx)
    .await?;
    tx.commit().await?;
    Ok((StatusCode::ACCEPTED, Json(package_dto(row)?)))
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
struct UserQuotaRequest {
    quota_bytes: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct UserQuotaDto {
    user_id: Uuid,
    quota_bytes: i64,
    reserved_bytes: i64,
    used_bytes: i64,
    cleanup_bytes: i64,
}

async fn set_user_quota(
    State(state): State<AppState>,
    AdminUser(_admin): AdminUser,
    AxumPath(user_id): AxumPath<Uuid>,
    Json(request): Json<UserQuotaRequest>,
) -> Result<Json<UserQuotaDto>, ApiError> {
    if !(0..=OFFLINE_MAX_USER_QUOTA).contains(&request.quota_bytes) {
        return Err(ApiError::BadRequest(
            "QuotaBytes must be between 0 and 1 TiB".to_owned(),
        ));
    }
    let mut tx = state.db.begin().await?;
    db::require_active_run(&mut tx, state.run_id).await?;
    let exists: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM users WHERE id=$1)")
        .bind(user_id)
        .fetch_one(&mut *tx)
        .await?;
    if !exists {
        return Err(ApiError::NotFound);
    }
    ensure_user_quota(&mut tx, user_id).await?;
    let row = sqlx::query("UPDATE offline_user_quotas SET quota_bytes=$2,updated_at=NOW() WHERE user_id=$1 AND reserved_bytes+used_bytes+cleanup_bytes <= $2 RETURNING quota_bytes,reserved_bytes,used_bytes,cleanup_bytes")
        .bind(user_id)
        .bind(request.quota_bytes)
        .fetch_optional(&mut *tx)
        .await?
        .ok_or_else(|| ApiError::Conflict("QuotaBytes is below this user's reserved plus used storage".to_owned()))?;
    let response = UserQuotaDto {
        user_id,
        quota_bytes: row.try_get("quota_bytes")?,
        reserved_bytes: row.try_get("reserved_bytes")?,
        used_bytes: row.try_get("used_bytes")?,
        cleanup_bytes: row.try_get("cleanup_bytes")?,
    };
    tx.commit().await?;
    Ok(Json(response))
}

async fn delete_package(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    AxumPath(package_id): AxumPath<Uuid>,
) -> Result<StatusCode, ApiError> {
    let mut tx = state.db.begin().await?;
    db::require_active_run(&mut tx, state.run_id).await?;
    let _current_user = load_user_for_update(&mut tx, user.id)
        .await?
        .ok_or(ApiError::Unauthorized)?;
    let package_item: Uuid =
        sqlx::query_scalar("SELECT item_id FROM offline_packages WHERE id=$1 AND user_id=$2")
            .bind(package_id)
            .bind(user.id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(ApiError::NotFound)?;
    let _item = sqlx::query("SELECT id FROM items WHERE id=$1 FOR SHARE")
        .bind(package_item)
        .fetch_optional(&mut *tx)
        .await?;
    ensure_global_quota(&mut tx).await?;
    let _global_lock =
        sqlx::query("SELECT singleton FROM offline_global_quota WHERE singleton=TRUE FOR UPDATE")
            .fetch_one(&mut *tx)
            .await?;
    let row =
        sqlx::query("SELECT status FROM offline_packages WHERE id=$1 AND user_id=$2 FOR UPDATE")
            .bind(package_id)
            .bind(user.id)
            .fetch_optional(&mut *tx)
            .await?
            .ok_or(ApiError::NotFound)?;
    let status: String = row.try_get("status")?;
    if matches!(status.as_str(), "queued" | "running" | "ready") {
        sqlx::query("UPDATE offline_packages SET status='cancelled',reservation_bytes=0,actual_size=NULL,sha256=NULL,relative_path=NULL,claimed_run_id=NULL,error_code='cancelled',finished_at=NOW(),updated_at=NOW() WHERE id=$1")
            .bind(package_id).execute(&mut *tx).await?;
        sqlx::query("DELETE FROM offline_package_chunks WHERE package_id=$1")
            .bind(package_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn download_package_chunk(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    AxumPath(package_id): AxumPath<Uuid>,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if !user.enable_content_downloading {
        return Err(ApiError::Forbidden);
    }
    let row = sqlx::query("SELECT p.item_id,p.library_id,p.item_type,p.file_name,p.container,p.source_size,p.actual_size,p.sha256,p.relative_path,p.user_id,p.status FROM offline_packages p WHERE p.id=$1 AND p.user_id=$2 AND p.status='ready'")
        .bind(package_id)
        .bind(user.id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(ApiError::NotFound)?;
    let total_size: i64 = row.try_get("source_size")?;
    let actual_size: Option<i64> = row.try_get("actual_size")?;
    let full_hash: Option<String> = row.try_get("sha256")?;
    let relative: Option<String> = row.try_get("relative_path")?;
    if actual_size != Some(total_size) || full_hash.is_none() || relative.is_none() {
        return Err(ApiError::Unavailable);
    }
    if !package_is_currently_allowed(&state, &user, &row).await? {
        return Err(ApiError::NotFound);
    }
    let total = u64::try_from(total_size).map_err(|_| ApiError::Unavailable)?;
    let requested = match headers.get(header::RANGE) {
        Some(value) => {
            let value = value
                .to_str()
                .map_err(|_| ApiError::BadRequest("Invalid Range header".to_owned()))?;
            match crate::media_features::range::parse_range_header(value, total) {
                Ok(Some(range)) => Some(range),
                Ok(None) => {
                    return Err(ApiError::BadRequest(
                        "A byte range is required for offline content".to_owned(),
                    ));
                }
                Err(_) => return Ok(range_not_satisfiable(total)),
            }
        }
        None => None,
    };
    let (start, length, partial) = match requested {
        Some(range) => (range.start(), range.len(), true),
        None if total <= OFFLINE_CHUNK_BYTES as u64 => (0, total, false),
        None => {
            return Err(ApiError::BadRequest(
                "Request one aligned 1 MiB byte range at a time".to_owned(),
            ));
        }
    };
    if length == 0 || length > OFFLINE_CHUNK_BYTES as u64 || start % OFFLINE_CHUNK_BYTES as u64 != 0
    {
        return Ok(range_not_satisfiable(total));
    }
    let expected_len = (total - start).min(OFFLINE_CHUNK_BYTES as u64);
    if length != expected_len {
        return Ok(range_not_satisfiable(total));
    }
    let chunk_index =
        i32::try_from(start / OFFLINE_CHUNK_BYTES as u64).map_err(|_| ApiError::Unavailable)?;
    let chunk_row = sqlx::query("SELECT byte_length,sha256 FROM offline_package_chunks WHERE package_id=$1 AND chunk_index=$2")
        .bind(package_id)
        .bind(chunk_index)
        .fetch_optional(&state.db)
        .await?
        .ok_or(ApiError::Unavailable)?;
    let expected_chunk_len: i32 = chunk_row.try_get("byte_length")?;
    let expected_chunk_hash: String = chunk_row.try_get("sha256")?;
    if u64::try_from(expected_chunk_len).ok() != Some(length) {
        return Err(ApiError::Unavailable);
    }
    let slots = CHUNK_READ_SLOTS
        .get_or_init(|| Arc::new(Semaphore::new(MAX_CONCURRENT_CHUNK_READS)))
        .clone();
    let permit = slots
        .try_acquire_owned()
        .map_err(|_| ApiError::Unavailable)?;
    let data_dir = state.config.data_dir.clone();
    let relative = relative.expect("checked above");
    let (bytes, permit) = tokio::task::spawn_blocking(move || {
        let bytes = read_verified_chunk(
            &data_dir,
            &relative,
            start,
            length,
            total,
            &expected_chunk_hash,
        )?;
        Ok::<_, ApiError>((bytes, permit))
    })
    .await
    .map_err(|_| ApiError::Unavailable)??;
    let chunk_hash = hex_sha256(&bytes);
    let mut response = Response::new(guarded_chunk_body(bytes, permit));
    *response.status_mut() = if partial {
        StatusCode::PARTIAL_CONTENT
    } else {
        StatusCode::OK
    };
    let file_name: String = row.try_get("file_name")?;
    let container: Option<String> = row.try_get("container")?;
    let item_type: String = row.try_get("item_type")?;
    let content_type = content_type_for(&file_name, container, &item_type);
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&content_type).map_err(|_| ApiError::Unavailable)?,
    );
    response.headers_mut().insert(
        header::CONTENT_DISPOSITION,
        HeaderValue::from_str(&content_disposition(&file_name, &item_type, &content_type))
            .map_err(|_| ApiError::Unavailable)?,
    );
    response.headers_mut().insert(
        header::CONTENT_LENGTH,
        HeaderValue::from_str(&length.to_string()).map_err(|_| ApiError::Unavailable)?,
    );
    response
        .headers_mut()
        .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    response.headers_mut().insert(
        header::ETAG,
        HeaderValue::from_str(&format!("\"{}\"", full_hash.unwrap().trim()))
            .map_err(|_| ApiError::Unavailable)?,
    );
    response.headers_mut().insert(
        "x-chunk-sha256",
        HeaderValue::from_str(&chunk_hash).map_err(|_| ApiError::Unavailable)?,
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    response
        .headers_mut()
        .insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    if partial {
        response.headers_mut().insert(
            header::CONTENT_RANGE,
            HeaderValue::from_str(&format!("bytes {}-{}/{}", start, start + length - 1, total))
                .map_err(|_| ApiError::Unavailable)?,
        );
    }
    Ok(response)
}

fn guarded_chunk_body(bytes: Vec<u8>, permit: OwnedSemaphorePermit) -> Body {
    let stream = futures_util::stream::unfold(
        (Some(bytes::Bytes::from(bytes)), permit),
        |(chunk, permit)| async move {
            chunk.map(|bytes| (Ok::<_, std::convert::Infallible>(bytes), (None, permit)))
        },
    );
    Body::from_stream(stream)
}

fn range_not_satisfiable(total: u64) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
    response.headers_mut().insert(
        header::CONTENT_RANGE,
        HeaderValue::from_str(&format!("bytes */{total}"))
            .expect("generated content range is a valid header"),
    );
    response
        .headers_mut()
        .insert(header::ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    response
}

async fn ensure_user_quota(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
) -> Result<(), sqlx::Error> {
    sqlx::query(
        "INSERT INTO offline_user_quotas(user_id) VALUES ($1) ON CONFLICT(user_id) DO NOTHING",
    )
    .bind(user_id)
    .execute(&mut **tx)
    .await?;
    Ok(())
}

async fn ensure_global_quota(tx: &mut Transaction<'_, Postgres>) -> Result<(), sqlx::Error> {
    sqlx::query("INSERT INTO offline_global_quota(singleton) VALUES(TRUE) ON CONFLICT(singleton) DO NOTHING")
        .execute(&mut **tx).await?;
    Ok(())
}

async fn load_user_for_share(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
) -> Result<Option<UserRecord>, sqlx::Error> {
    let row = sqlx::query("SELECT u.id,u.username,u.is_admin,u.disabled,u.enable_remote_access,u.allow_media_playback,u.enable_content_downloading,u.enable_live_tv_access,u.enable_live_tv_management,u.restrict_libraries,u.configuration,u.max_parental_rating,u.block_unrated_items,COALESCE(ARRAY(SELECT a.library_id FROM user_library_access a WHERE a.user_id=u.id ORDER BY a.library_id),ARRAY[]::uuid[]) AS allowed_library_ids FROM users u WHERE u.id=$1 AND u.disabled=FALSE FOR SHARE OF u")
        .bind(user_id).fetch_optional(&mut **tx).await?;
    row.as_ref().map(db::user_from_row).transpose()
}

async fn load_user_for_update(
    tx: &mut Transaction<'_, Postgres>,
    user_id: Uuid,
) -> Result<Option<UserRecord>, sqlx::Error> {
    let row = sqlx::query("SELECT u.id,u.username,u.is_admin,u.disabled,u.enable_remote_access,u.allow_media_playback,u.enable_content_downloading,u.enable_live_tv_access,u.enable_live_tv_management,u.restrict_libraries,u.configuration,u.max_parental_rating,u.block_unrated_items,COALESCE(ARRAY(SELECT a.library_id FROM user_library_access a WHERE a.user_id=u.id ORDER BY a.library_id),ARRAY[]::uuid[]) AS allowed_library_ids FROM users u WHERE u.id=$1 AND u.disabled=FALSE FOR UPDATE OF u")
        .bind(user_id).fetch_optional(&mut **tx).await?;
    row.as_ref().map(db::user_from_row).transpose()
}

fn item_file_name(item: &ItemRecord) -> String {
    let candidate = Path::new(&item.path)
        .file_name()
        .and_then(|value| value.to_str())
        .unwrap_or(&item.name);
    let sanitized = candidate
        .chars()
        .map(|character| {
            if character.is_ascii_alphanumeric() || matches!(character, '.' | '-' | '_' | ' ') {
                character
            } else {
                '_'
            }
        })
        .take(240)
        .collect::<String>()
        .trim_matches([' ', '.'])
        .to_owned();
    if sanitized.is_empty() {
        "offline-media".to_owned()
    } else {
        sanitized
    }
}

fn content_disposition(file_name: &str, item_type: &str, content_type: &str) -> String {
    let disposition = if matches!(
        item_type,
        "Movie" | "Episode" | "Audio" | "MusicVideo" | "Photo" | "Image"
    ) && (content_type.starts_with("video/")
        || content_type.starts_with("audio/")
        || content_type.starts_with("image/"))
    {
        "inline"
    } else {
        "attachment"
    };
    let encoded = file_name
        .as_bytes()
        .iter()
        .map(|byte| {
            if byte.is_ascii_alphanumeric() || matches!(*byte, b'-' | b'_' | b'.' | b'~') {
                (*byte as char).to_string()
            } else {
                format!("%{byte:02X}")
            }
        })
        .collect::<String>();
    format!("{disposition}; filename*=UTF-8''{encoded}")
}

async fn load_downloadable_item_for_share(
    tx: &mut Transaction<'_, Postgres>,
    item_id: Uuid,
) -> Result<Option<ItemRecord>, sqlx::Error> {
    let rating = db::policy_rating_sql("i");
    let row = sqlx::query(&format!("SELECT i.id,i.library_id,i.parent_id,i.name,i.sort_name,i.item_type,i.path,i.container,i.size_bytes,i.runtime_ticks,i.date_added,i.date_modified,{rating} AS rating,i.overview,i.metadata_json FROM items i JOIN libraries l ON l.id=i.library_id WHERE i.id=$1 AND l.enabled=TRUE FOR SHARE OF i,l"))
        .bind(item_id).fetch_optional(&mut **tx).await?;
    row.as_ref().map(db::item_from_row).transpose()
}

fn download_policy_allows(user: &UserRecord, item: &ItemRecord) -> bool {
    !user.disabled
        && user.enable_content_downloading
        && user.allow_media_playback
        && matches!(
            item.item_type.as_str(),
            "Movie"
                | "Episode"
                | "Audio"
                | "MusicVideo"
                | "Photo"
                | "Image"
                | "Book"
                | "AudioBook"
                | "EBook"
        )
        && db::user_policy_allows_item(user, item)
}

#[derive(Clone, Debug)]
struct QueuedJob {
    id: Uuid,
    user_id: Uuid,
    item_id: Uuid,
    source_size: i64,
    source_modified_at: DateTime<Utc>,
    relative_partial: String,
}

/// Start one bounded worker. Claims and mutations are serialized with the
/// server run marker and each package row; restart recovery is in activate_run.
pub fn start_worker(state: AppState) {
    let cleanup_state = state.clone();
    tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(10));
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        while !cleanup_state.shutdown_requested.load(Ordering::Acquire) {
            interval.tick().await;
            if cleanup_state.shutdown_requested.load(Ordering::Acquire) {
                return;
            }
            if let Err(error) = sweep_orphans(&cleanup_state).await {
                if is_stale_run(&error) {
                    return;
                }
                tracing::warn!(error=%error, "offline cleanup pass failed");
            }
        }
    });
    tokio::spawn(async move {
        while !state.shutdown_requested.load(Ordering::Acquire) {
            match claim_next(&state).await {
                Ok(Some(job)) => {
                    if let Err(error_code) = process_job(&state, &job).await {
                        if error_code == "server-restarted" {
                            return;
                        }
                        tracing::warn!(package_id=%job.id, code=error_code, "offline package copy did not complete");
                        finish_failed_until_recorded(&state, &job, error_code).await;
                    }
                }
                Ok(None) => {
                    tokio::time::sleep(Duration::from_secs(2)).await;
                }
                Err(error) => {
                    if error.to_string().contains(db::STALE_SERVER_RUN_ERROR) {
                        return;
                    }
                    tracing::warn!(error=%error, "offline worker could not claim a package");
                    tokio::time::sleep(Duration::from_secs(3)).await;
                }
            }
        }
    });
}

fn is_stale_run(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Protocol(message) if message.contains(db::STALE_SERVER_RUN_ERROR))
}

fn map_run_error(error: &sqlx::Error) -> &'static str {
    if is_stale_run(error) {
        "server-restarted"
    } else {
        "copy-failed"
    }
}

async fn finish_failed_until_recorded(state: &AppState, job: &QueuedJob, code: &'static str) {
    loop {
        if state.shutdown_requested.load(Ordering::Acquire) {
            return;
        }
        match finish_failed(state, job, code).await {
            Ok(()) => return,
            Err(error) if is_stale_run(&error) => return,
            Err(error) => {
                tracing::warn!(package_id=%job.id, error=%error, "offline failure state is waiting for database recovery");
                tokio::time::sleep(Duration::from_secs(2)).await;
            }
        }
    }
}

async fn claim_next(state: &AppState) -> Result<Option<QueuedJob>, sqlx::Error> {
    let mut tx = state.db.begin().await?;
    db::require_active_run(&mut tx, state.run_id).await?;
    let row = sqlx::query("SELECT p.id,p.user_id,p.item_id,p.source_size,p.source_modified_at FROM offline_packages p WHERE p.status='queued' AND NOT EXISTS (SELECT 1 FROM offline_orphan_files o WHERE o.package_id=p.id) ORDER BY p.created_at,p.id FOR UPDATE OF p SKIP LOCKED LIMIT 1")
        .fetch_optional(&mut *tx).await?;
    let Some(row) = row else {
        tx.commit().await?;
        return Ok(None);
    };
    let job_id: Uuid = row.try_get("id")?;
    let relative_partial = package_relative_path(state.run_id, job_id, "partial");
    sqlx::query("UPDATE offline_packages SET status='running',claimed_run_id=$2,started_at=NOW(),finished_at=NULL,relative_path=$3,bytes_copied=0,error_code=NULL,updated_at=NOW() WHERE id=$1 AND status='queued'")
        .bind(job_id).bind(state.run_id).bind(&relative_partial).execute(&mut *tx).await?;
    let job = QueuedJob {
        id: job_id,
        user_id: row.try_get("user_id")?,
        item_id: row.try_get("item_id")?,
        source_size: row.try_get("source_size")?,
        source_modified_at: row.try_get("source_modified_at")?,
        relative_partial,
    };
    tx.commit().await?;
    Ok(Some(job))
}

async fn process_job(state: &AppState, job: &QueuedJob) -> Result<(), &'static str> {
    let user = db::get_user(&state.db, job.user_id)
        .await
        .map_err(|_| "source-unavailable")?
        .ok_or("policy-revoked")?;
    if !user.enable_content_downloading {
        return Err("policy-revoked");
    }
    let item = db::get_item(&state.db, job.item_id)
        .await
        .map_err(|_| "source-unavailable")?
        .ok_or("source-unavailable")?;
    if !download_policy_allows(&user, &item)
        || !db::item_visible_to_user(&state.db, &user, &item)
            .await
            .map_err(|_| "source-unavailable")?
    {
        return Err("policy-revoked");
    }
    let opened = media_features::open_authorized_item(state, &user, job.item_id)
        .await
        .map_err(|_| "source-unavailable")?;
    if !opened.catalog_identity_matches
        || i64::try_from(opened.size).ok() != Some(job.source_size)
        || item.size_bytes != Some(job.source_size)
        || item.date_modified != Some(job.source_modified_at)
        || opened.modified_utc != Some(job.source_modified_at)
    {
        return Err("source-changed");
    }
    let output = create_claim_file(state, job).await?;
    let mut source = tokio::fs::File::from_std(opened.file);
    let mut output = tokio::fs::File::from_std(output);
    let mut buffer = vec![0_u8; OFFLINE_CHUNK_BYTES];
    let mut full_hash = Sha256::new();
    let mut chunk_hash = Sha256::new();
    let mut chunk_rows: Vec<(i32, i32, String)> = Vec::new();
    let mut total_written = 0_u64;
    let mut chunk_written = 0_usize;
    let mut chunk_index = 0_i32;
    loop {
        if state.shutdown_requested.load(Ordering::Acquire) {
            return Err("server-restarted");
        }
        let remaining = job.source_size as u64 - total_written;
        if remaining == 0 {
            let mut extra = [0_u8; 1];
            let read = source
                .read(&mut extra)
                .await
                .map_err(|_| "source-unavailable")?;
            if read != 0 {
                return Err("source-changed");
            }
            break;
        }
        let cap = next_copy_read_len(remaining, chunk_written, buffer.len(), OFFLINE_CHUNK_BYTES);
        if cap == 0 {
            return Err("copy-failed");
        }
        let read = source
            .read(&mut buffer[..cap])
            .await
            .map_err(|_| "source-unavailable")?;
        if read == 0 {
            return Err("source-changed");
        }
        let next_total = total_written + read as u64;
        write_copy_chunk_guarded(state, job, &mut output, &buffer[..read], next_total as i64)
            .await?;
        full_hash.update(&buffer[..read]);
        chunk_hash.update(&buffer[..read]);
        total_written += read as u64;
        chunk_written += read;
        if chunk_written == OFFLINE_CHUNK_BYTES || total_written == job.source_size as u64 {
            chunk_rows.push((
                chunk_index,
                i32::try_from(chunk_written).map_err(|_| "copy-failed")?,
                hex_digest(chunk_hash.finalize_reset()),
            ));
            chunk_index += 1;
            chunk_written = 0;
        }
    }
    let final_source_metadata = source.metadata().await.map_err(|_| "source-unavailable")?;
    let final_source_modified = final_source_metadata
        .modified()
        .ok()
        .map(DateTime::<Utc>::from)
        .map(normalize_timestamp_micros);
    if !final_source_metadata.is_file()
        || final_source_metadata.len() != job.source_size as u64
        || final_source_modified != Some(job.source_modified_at)
    {
        return Err("source-changed");
    }
    output.flush().await.map_err(|_| "copy-failed")?;
    output.sync_all().await.map_err(|_| "copy-failed")?;
    drop(output);
    drop(source);
    if !still_authorized(state, job).await? {
        return Err("policy-revoked");
    }
    let final_path = package_relative_path(state.run_id, job.id, "media");
    let hash = hex_digest(full_hash.finalize());
    finish_ready(
        state,
        job,
        &final_path,
        &hash,
        total_written as i64,
        chunk_rows,
        &job.relative_partial,
    )
    .await
}

fn normalize_timestamp_micros(value: DateTime<Utc>) -> DateTime<Utc> {
    DateTime::from_timestamp_micros(value.timestamp_micros()).unwrap_or(value)
}

fn next_copy_read_len(
    remaining: u64,
    chunk_written: usize,
    buffer_len: usize,
    chunk_limit: usize,
) -> usize {
    let chunk_remaining = chunk_limit.saturating_sub(chunk_written);
    usize::try_from(remaining.min(buffer_len.min(chunk_remaining) as u64)).unwrap_or(0)
}

async fn create_claim_file(state: &AppState, job: &QueuedJob) -> Result<File, &'static str> {
    if state.shutdown_requested.load(Ordering::Acquire) {
        return Err("server-restarted");
    }
    let mut tx = state.db.begin().await.map_err(|_| "copy-failed")?;
    db::require_active_run(&mut tx, state.run_id)
        .await
        .map_err(|error| map_run_error(&error))?;
    let active = sqlx::query("SELECT id FROM offline_packages WHERE id=$1 AND status='running' AND claimed_run_id=$2 AND relative_path=$3 FOR UPDATE")
        .bind(job.id).bind(state.run_id).bind(&job.relative_partial).fetch_optional(&mut *tx).await.map_err(|_| "copy-failed")?;
    if active.is_none() {
        return Err("cancelled");
    }
    let data_dir = state.config.data_dir.clone();
    let relative = job.relative_partial.clone();
    let output = tokio::task::spawn_blocking(move || create_partial_file(&data_dir, &relative))
        .await
        .map_err(|_| "copy-failed")?
        .map_err(|_| "copy-failed")?;
    tx.commit().await.map_err(|_| "copy-failed")?;
    Ok(output)
}

async fn write_copy_chunk_guarded(
    state: &AppState,
    job: &QueuedJob,
    output: &mut tokio::fs::File,
    bytes: &[u8],
    copied: i64,
) -> Result<(), &'static str> {
    let mut tx = state.db.begin().await.map_err(|_| "copy-failed")?;
    db::require_active_run(&mut tx, state.run_id)
        .await
        .map_err(|error| map_run_error(&error))?;
    let active = sqlx::query("SELECT id FROM offline_packages WHERE id=$1 AND status='running' AND claimed_run_id=$2 FOR UPDATE")
        .bind(job.id).bind(state.run_id).fetch_optional(&mut *tx).await.map_err(|_| "copy-failed")?;
    if active.is_none() {
        return Err("cancelled");
    }
    output.write_all(bytes).await.map_err(|_| "copy-failed")?;
    const PROGRESS_INTERVAL: u64 = OFFLINE_CHUNK_BYTES as u64 * 16;
    if copied as u64 % PROGRESS_INTERVAL < bytes.len() as u64 {
        sqlx::query("UPDATE offline_packages SET bytes_copied=$3,updated_at=NOW() WHERE id=$1 AND status='running' AND claimed_run_id=$2 AND bytes_copied <= $3")
            .bind(job.id).bind(state.run_id).bind(copied).execute(&mut *tx).await.map_err(|_| "copy-failed")?;
    }
    tx.commit().await.map_err(|_| "copy-failed")
}

async fn still_authorized(state: &AppState, job: &QueuedJob) -> Result<bool, &'static str> {
    let user = db::get_user(&state.db, job.user_id)
        .await
        .map_err(|_| "source-unavailable")?;
    let Some(user) = user else {
        return Ok(false);
    };
    if !user.enable_content_downloading {
        return Ok(false);
    }
    let item = db::get_item(&state.db, job.item_id)
        .await
        .map_err(|_| "source-unavailable")?;
    let Some(item) = item else {
        return Ok(false);
    };
    if !download_policy_allows(&user, &item) {
        return Ok(false);
    }
    db::item_visible_to_user(&state.db, &user, &item)
        .await
        .map_err(|_| "source-unavailable")
}

async fn finish_ready(
    state: &AppState,
    job: &QueuedJob,
    final_path: &str,
    full_hash: &str,
    total: i64,
    chunks: Vec<(i32, i32, String)>,
    partial_path: &str,
) -> Result<(), &'static str> {
    let mut tx = state.db.begin().await.map_err(|_| "copy-failed")?;
    db::require_active_run(&mut tx, state.run_id)
        .await
        .map_err(|error| map_run_error(&error))?;
    let Some(user) = load_user_for_share(&mut tx, job.user_id)
        .await
        .map_err(|_| "copy-failed")?
    else {
        return Err("policy-revoked");
    };
    let Some(item) = load_downloadable_item_for_share(&mut tx, job.item_id)
        .await
        .map_err(|_| "copy-failed")?
    else {
        return Err("policy-revoked");
    };
    if !download_policy_allows(&user, &item)
        || item.size_bytes != Some(job.source_size)
        || item.date_modified != Some(job.source_modified_at)
    {
        return Err("policy-revoked");
    }
    ensure_global_quota(&mut tx)
        .await
        .map_err(|_| "copy-failed")?;
    sqlx::query("SELECT singleton FROM offline_global_quota WHERE singleton=TRUE FOR UPDATE")
        .fetch_one(&mut *tx)
        .await
        .map_err(|_| "copy-failed")?;
    let row = sqlx::query("SELECT reservation_bytes FROM offline_packages WHERE id=$1 AND user_id=$2 AND status='running' AND claimed_run_id=$3 FOR UPDATE")
        .bind(job.id).bind(job.user_id).bind(state.run_id).fetch_optional(&mut *tx).await.map_err(|_| "copy-failed")?;
    let Some(row) = row else {
        return Err("cancelled");
    };
    let reservation: i64 = row
        .try_get("reservation_bytes")
        .map_err(|_| "copy-failed")?;
    if reservation != job.source_size || total != job.source_size {
        return Err("source-changed");
    }
    ensure_user_quota(&mut tx, job.user_id)
        .await
        .map_err(|_| "copy-failed")?;
    sqlx::query("SELECT user_id FROM offline_user_quotas WHERE user_id=$1 FOR UPDATE")
        .bind(job.user_id)
        .fetch_one(&mut *tx)
        .await
        .map_err(|_| "copy-failed")?;
    for batch in chunks.chunks(500) {
        let mut builder = QueryBuilder::<Postgres>::new(
            "INSERT INTO offline_package_chunks(package_id,chunk_index,byte_length,sha256) ",
        );
        builder.push_values(batch, |mut row, chunk| {
            row.push_bind(job.id)
                .push_bind(chunk.0)
                .push_bind(chunk.1)
                .push_bind(&chunk.2);
        });
        builder
            .build()
            .execute(&mut *tx)
            .await
            .map_err(|_| "copy-failed")?;
    }
    let data_dir = state.config.data_dir.clone();
    let from = partial_path.to_owned();
    let to = final_path.to_owned();
    tokio::task::spawn_blocking(move || rename_data_file(&data_dir, &from, &to))
        .await
        .map_err(|_| "copy-failed")?
        .map_err(|_| "copy-failed")?;
    sqlx::query("UPDATE offline_user_quotas SET reserved_bytes=GREATEST(0,reserved_bytes-$2),used_bytes=used_bytes+$2,updated_at=NOW() WHERE user_id=$1")
        .bind(job.user_id).bind(reservation).execute(&mut *tx).await.map_err(|_| "copy-failed")?;
    sqlx::query("UPDATE offline_global_quota SET reserved_bytes=GREATEST(0,reserved_bytes-$1),used_bytes=used_bytes+$1,updated_at=NOW() WHERE singleton=TRUE")
        .bind(reservation).execute(&mut *tx).await.map_err(|_| "copy-failed")?;
    let changed = sqlx::query("UPDATE offline_packages SET status='ready',claimed_run_id=NULL,bytes_copied=$4,actual_size=$4,sha256=$5,relative_path=$6,reservation_bytes=0,error_code=NULL,finished_at=NOW(),updated_at=NOW() WHERE id=$1 AND user_id=$2 AND claimed_run_id=$3")
        .bind(job.id).bind(job.user_id).bind(state.run_id).bind(total).bind(full_hash).bind(final_path).execute(&mut *tx).await.map_err(|_| "copy-failed")?.rows_affected();
    if changed != 1 {
        return Err("cancelled");
    }
    tx.commit().await.map_err(|_| "copy-failed")
}

async fn finish_failed(
    state: &AppState,
    job: &QueuedJob,
    code: &'static str,
) -> Result<(), sqlx::Error> {
    if !matches!(
        code,
        "source-changed"
            | "source-unavailable"
            | "policy-revoked"
            | "copy-failed"
            | "checksum-failed"
            | "quota-exceeded"
            | "server-restarted"
    ) {
        return Ok(());
    }
    let mut tx = state.db.begin().await?;
    db::require_active_run(&mut tx, state.run_id).await?;
    let _user = load_user_for_share(&mut tx, job.user_id).await?;
    let _item = sqlx::query("SELECT id FROM items WHERE id=$1 FOR SHARE")
        .bind(job.item_id)
        .fetch_optional(&mut *tx)
        .await?;
    ensure_global_quota(&mut tx).await?;
    sqlx::query("SELECT singleton FROM offline_global_quota WHERE singleton=TRUE FOR UPDATE")
        .fetch_one(&mut *tx)
        .await?;
    let row = sqlx::query("SELECT reservation_bytes FROM offline_packages WHERE id=$1 AND user_id=$2 AND status='running' AND claimed_run_id=$3 FOR UPDATE")
        .bind(job.id).bind(job.user_id).bind(state.run_id).fetch_optional(&mut *tx).await?;
    let Some(row) = row else {
        tx.commit().await?;
        return Ok(());
    };
    let _: i64 = row.try_get("reservation_bytes")?;
    ensure_user_quota(&mut tx, job.user_id).await?;
    sqlx::query("SELECT user_id FROM offline_user_quotas WHERE user_id=$1 FOR UPDATE")
        .bind(job.user_id)
        .fetch_optional(&mut *tx)
        .await?;
    sqlx::query("UPDATE offline_packages SET status='failed',claimed_run_id=NULL,reservation_bytes=0,relative_path=NULL,error_code=$4,finished_at=NOW(),updated_at=NOW() WHERE id=$1 AND user_id=$2 AND claimed_run_id=$3")
        .bind(job.id).bind(job.user_id).bind(state.run_id).bind(code).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(())
}

async fn sweep_orphans(state: &AppState) -> Result<(), sqlx::Error> {
    for _ in 0..SWEEP_ENTRIES_PER_TICK {
        let mut tx = state.db.begin().await?;
        db::require_active_run(&mut tx, state.run_id).await?;
        sqlx::query("SELECT singleton FROM offline_global_quota WHERE singleton=TRUE FOR UPDATE")
            .fetch_one(&mut *tx)
            .await?;
        let row = sqlx::query("SELECT relative_path,package_id,user_id,byte_size,charge_accounted FROM offline_orphan_files ORDER BY last_attempt_at NULLS FIRST, created_at, relative_path LIMIT 1 FOR UPDATE SKIP LOCKED")
            .fetch_optional(&mut *tx).await?;
        let Some(row) = row else {
            tx.commit().await?;
            break;
        };
        let relative: String = row.try_get("relative_path")?;
        let user_id: Option<Uuid> = row.try_get("user_id")?;
        let byte_size: i64 = row.try_get("byte_size")?;
        let charge_accounted: bool = row.try_get("charge_accounted")?;
        let data_dir = state.config.data_dir.clone();
        let path = relative.clone();
        let removed = tokio::task::spawn_blocking(move || remove_data_file(&data_dir, &path))
            .await
            .map_err(|error| {
                sqlx::Error::Protocol(format!("offline cleanup task failed: {error}"))
            })?;
        match removed {
            Ok(()) => {
                if charge_accounted && byte_size > 0 {
                    if let Some(user_id) = user_id {
                        sqlx::query("UPDATE offline_user_quotas SET cleanup_bytes=GREATEST(0,cleanup_bytes-$2),updated_at=NOW() WHERE user_id=$1")
                            .bind(user_id).bind(byte_size).execute(&mut *tx).await?;
                    }
                    sqlx::query("UPDATE offline_global_quota SET cleanup_bytes=GREATEST(0,cleanup_bytes-$1),updated_at=NOW() WHERE singleton=TRUE")
                        .bind(byte_size).execute(&mut *tx).await?;
                }
                sqlx::query("DELETE FROM offline_orphan_files WHERE relative_path=$1")
                    .bind(&relative)
                    .execute(&mut *tx)
                    .await?;
            }
            Err(error) => {
                sqlx::query("UPDATE offline_orphan_files SET attempts=attempts+1,last_attempt_at=NOW() WHERE relative_path=$1")
                    .bind(&relative)
                    .execute(&mut *tx)
                    .await?;
                tracing::warn!(path=%relative, error=%error, "offline orphan file cleanup will retry");
            }
        }
        tx.commit().await?;
    }
    Ok(())
}

fn package_relative_path(run_id: Uuid, package_id: Uuid, suffix: &str) -> String {
    format!("{run_id}/{package_id}.{suffix}")
}

fn parse_relative(relative: &str) -> io::Result<(String, String)> {
    let (run_id, file) = relative
        .split_once('/')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid offline path"))?;
    Uuid::parse_str(run_id)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid offline run id"))?;
    let (package_id, suffix) = file
        .rsplit_once('.')
        .ok_or_else(|| io::Error::new(io::ErrorKind::InvalidInput, "invalid offline filename"))?;
    Uuid::parse_str(package_id)
        .map_err(|_| io::Error::new(io::ErrorKind::InvalidInput, "invalid offline package id"))?;
    if !matches!(suffix, "partial" | "media") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "invalid offline file type",
        ));
    }
    Ok((run_id.to_owned(), file.to_owned()))
}

fn private_directory(path: &Path) -> io::Result<()> {
    match fs::create_dir(path) {
        Ok(()) => {}
        Err(error) if error.kind() == io::ErrorKind::AlreadyExists => {}
        Err(error) => return Err(error),
    }
    let metadata = fs::symlink_metadata(path)?;
    if !metadata.is_dir() || metadata.file_type().is_symlink() {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "offline directory is not a real directory",
        ));
    }
    fs::set_permissions(path, fs::Permissions::from_mode(0o700))?;
    Ok(())
}

fn open_child_directory(parent: &Dir, name: &str) -> io::Result<Dir> {
    let mut options = CapOpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    let file = parent.open_with(name, &options)?;
    if !file.metadata()?.is_dir() {
        return Err(io::Error::new(
            io::ErrorKind::NotADirectory,
            "offline path component is not a directory",
        ));
    }
    Ok(Dir::from_std_file(file.into_std()))
}

fn open_offline_run_dir(data_dir: &Path, run_id: &str) -> io::Result<Dir> {
    let canonical = fs::canonicalize(data_dir)?;
    let root = Dir::open_ambient_dir(canonical, cap_std::ambient_authority())?;
    let offline = open_child_directory(&root, "offline")?;
    open_child_directory(&offline, run_id)
}

fn create_partial_file(data_dir: &Path, relative: &str) -> io::Result<File> {
    let (run_id, leaf) = parse_relative(relative)?;
    let offline_path = data_dir.join("offline");
    private_directory(&offline_path)?;
    private_directory(&offline_path.join(&run_id))?;
    let directory = open_offline_run_dir(data_dir, &run_id)?;
    let mut options = CapOpenOptions::new();
    options
        .write(true)
        .create_new(true)
        .mode(0o600)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    directory
        .open_with(leaf, &options)
        .map(|file| file.into_std())
}

fn open_stored_file(data_dir: &Path, relative: &str, expected_size: u64) -> io::Result<File> {
    let (run_id, leaf) = parse_relative(relative)?;
    if !leaf.ends_with(".media") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "offline package is not published",
        ));
    }
    let directory = open_offline_run_dir(data_dir, &run_id)?;
    let mut options = CapOpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    let file = directory.open_with(leaf, &options)?.into_std();
    let metadata = file.metadata()?;
    if !metadata.is_file() || metadata.len() != expected_size {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "offline file identity or size did not match",
        ));
    }
    Ok(file)
}

fn rename_data_file(data_dir: &Path, from: &str, to: &str) -> io::Result<()> {
    let (from_run, from_leaf) = parse_relative(from)?;
    let (to_run, to_leaf) = parse_relative(to)?;
    if from_run != to_run || !from_leaf.ends_with(".partial") || !to_leaf.ends_with(".media") {
        return Err(io::Error::new(
            io::ErrorKind::InvalidInput,
            "offline rename paths are invalid",
        ));
    }
    let directory = open_offline_run_dir(data_dir, &from_run)?;
    directory.rename(from_leaf, &directory, to_leaf)?;
    directory.into_std_file().sync_all()?;
    Ok(())
}

fn remove_data_file(data_dir: &Path, relative: &str) -> io::Result<()> {
    let (run_id, leaf) = parse_relative(relative)?;
    let directory = match open_offline_run_dir(data_dir, &run_id) {
        Ok(directory) => directory,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(()),
        Err(error) => return Err(error),
    };
    let sibling = if leaf.ends_with(".partial") {
        leaf.replace(".partial", ".media")
    } else {
        leaf.replace(".media", ".partial")
    };
    let mut removed_any = false;
    for candidate in [leaf.as_str(), sibling.as_str()] {
        match directory.remove_file(candidate) {
            Ok(()) => removed_any = true,
            Err(error) if error.kind() == io::ErrorKind::NotFound => {}
            Err(error) => return Err(error),
        }
    }
    if removed_any {
        directory.into_std_file().sync_all()?;
    }
    Ok(())
}

fn read_verified_chunk(
    data_dir: &Path,
    relative: &str,
    start: u64,
    length: u64,
    total: u64,
    expected_sha256: &str,
) -> Result<Vec<u8>, ApiError> {
    let mut file =
        open_stored_file(data_dir, relative, total).map_err(|_| ApiError::Unavailable)?;
    let metadata = file.metadata().map_err(|_| ApiError::Unavailable)?;
    if !metadata.is_file() || start.saturating_add(length) > metadata.len() {
        return Err(ApiError::Unavailable);
    }
    file.seek(SeekFrom::Start(start))
        .map_err(|_| ApiError::Unavailable)?;
    let mut bytes = vec![0; usize::try_from(length).map_err(|_| ApiError::Unavailable)?];
    file.read_exact(&mut bytes)
        .map_err(|_| ApiError::Unavailable)?;
    if hex_sha256(&bytes) != expected_sha256.trim() {
        return Err(ApiError::Unavailable);
    }
    Ok(bytes)
}

fn hex_sha256(bytes: &[u8]) -> String {
    hex_digest(Sha256::digest(bytes))
}

fn hex_digest(digest: impl AsRef<[u8]>) -> String {
    digest
        .as_ref()
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect()
}

#[cfg(test)]
mod tests {
    use std::{fs, io::Write};

    use axum::body::Bytes;
    use futures_util::StreamExt;
    use tokio::sync::Semaphore;

    use super::{
        OFFLINE_CHUNK_BYTES, create_partial_file, guarded_chunk_body, hex_sha256,
        next_copy_read_len, open_stored_file, package_relative_path, parse_relative,
        read_verified_chunk, remove_data_file, rename_data_file,
    };
    use uuid::Uuid;

    #[test]
    fn generated_paths_are_bounded_and_only_uuid_scoped() {
        let run_id = Uuid::new_v4();
        let package_id = Uuid::new_v4();
        let relative = package_relative_path(run_id, package_id, "partial");
        assert_eq!(
            parse_relative(&relative).unwrap(),
            (run_id.to_string(), format!("{package_id}.partial"))
        );
        for path in [
            "../file.media",
            "/tmp/file.media",
            "run/file",
            "not-a-uuid/file.media",
            &format!("{run_id}/../../secret.media"),
        ] {
            assert!(parse_relative(path).is_err(), "{path}");
        }
        assert_eq!(OFFLINE_CHUNK_BYTES, 1024 * 1024);
    }

    #[test]
    fn short_reads_never_overfill_a_chunk_boundary() {
        assert_eq!(next_copy_read_len(9, 0, 16, 8), 8);
        assert_eq!(next_copy_read_len(100, 7, 16, 8), 1);
        assert_eq!(next_copy_read_len(100, 8, 16, 8), 0);
        assert_eq!(next_copy_read_len(3, 0, 16, 8), 3);
        assert_eq!(next_copy_read_len(100, 4, 4, 8), 4);
    }

    #[tokio::test]
    async fn chunk_admission_stays_held_until_response_body_is_dropped() {
        let slots = std::sync::Arc::new(Semaphore::new(1));
        let permit = slots.clone().acquire_owned().await.unwrap();
        let body = guarded_chunk_body(vec![1, 2, 3], permit);
        assert_eq!(slots.available_permits(), 0);
        let mut data = http_body_util::BodyExt::into_data_stream(body);
        assert_eq!(
            data.next().await.unwrap().unwrap(),
            Bytes::from_static(&[1, 2, 3])
        );
        assert_eq!(slots.available_permits(), 0);
        drop(data);
        assert_eq!(slots.available_permits(), 1);
    }

    #[test]
    fn package_files_are_confined_published_atomically_and_hash_checked_by_chunk() {
        let data_dir = std::env::temp_dir().join(format!("puffinbox-offline-{}", Uuid::new_v4()));
        fs::create_dir_all(&data_dir).unwrap();
        let run_id = Uuid::new_v4();
        let package_id = Uuid::new_v4();
        let partial = package_relative_path(run_id, package_id, "partial");
        let final_name = package_relative_path(run_id, package_id, "media");
        let mut file = create_partial_file(&data_dir, &partial).unwrap();
        file.write_all(b"checked offline bytes").unwrap();
        file.sync_all().unwrap();
        drop(file);
        rename_data_file(&data_dir, &partial, &final_name).unwrap();

        let expected = hex_sha256(b"checked offline bytes");
        let downloaded = read_verified_chunk(&data_dir, &final_name, 0, 21, 21, &expected).unwrap();
        assert_eq!(downloaded, b"checked offline bytes");
        assert_eq!(
            open_stored_file(&data_dir, &final_name, 21)
                .unwrap()
                .metadata()
                .unwrap()
                .len(),
            21
        );
        assert!(
            read_verified_chunk(&data_dir, &final_name, 0, 21, 21, "0".repeat(64).as_str())
                .is_err()
        );
        assert!(open_stored_file(&data_dir, "../escape.media", 21).is_err());
        let outside = data_dir.join("outside");
        fs::write(&outside, b"outside").unwrap();
        let published = data_dir
            .join("offline")
            .join(run_id.to_string())
            .join(format!("{package_id}.media"));
        fs::remove_file(&published).unwrap();
        std::os::unix::fs::symlink(&outside, &published).unwrap();
        assert!(open_stored_file(&data_dir, &final_name, 21).is_err());
        let _ = fs::remove_dir_all(data_dir);
    }

    #[test]
    fn orphan_cleanup_for_partial_candidate_removes_a_renamed_media_sibling() {
        let data_dir =
            std::env::temp_dir().join(format!("puffinbox-offline-recovery-{}", Uuid::new_v4()));
        fs::create_dir_all(&data_dir).unwrap();
        let run_id = Uuid::new_v4();
        let package_id = Uuid::new_v4();
        let partial = package_relative_path(run_id, package_id, "partial");
        let published = package_relative_path(run_id, package_id, "media");
        let mut file = create_partial_file(&data_dir, &partial).unwrap();
        file.write_all(b"before-commit-crash").unwrap();
        file.sync_all().unwrap();
        drop(file);
        rename_data_file(&data_dir, &partial, &published).unwrap();
        assert!(
            data_dir
                .join("offline")
                .join(run_id.to_string())
                .join(format!("{package_id}.media"))
                .exists()
        );

        remove_data_file(&data_dir, &partial).unwrap();
        assert!(
            !data_dir
                .join("offline")
                .join(run_id.to_string())
                .join(format!("{package_id}.partial"))
                .exists()
        );
        assert!(
            !data_dir
                .join("offline")
                .join(run_id.to_string())
                .join(format!("{package_id}.media"))
                .exists()
        );
        let _ = fs::remove_dir_all(data_dir);
    }
}
