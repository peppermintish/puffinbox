mod display;
mod nfo;
mod tvmaze;
mod worker;

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, State},
    http::{HeaderMap, HeaderValue, Method, StatusCode, header},
    response::Response,
    routing::get,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use sqlx::Row;
use uuid::Uuid;

use crate::{
    auth::{AdminUser, CurrentUser, MediaUser, UserRecord},
    db,
    error::ApiError,
    library::ItemRecord,
    state::AppState,
};

pub use display::{DisplayMetadata, load_display_metadata};
pub(crate) mod catalog_sql;
use display::{ProviderMetadata, load_provider_details};

/// Register Puffinbox metadata extension routes. External-provider refreshes
/// are explicit and TVMaze data is always marked with its CC BY-SA attribution.
pub fn router(state: AppState) -> Router<()> {
    Router::new()
        .route(
            "/Puffinbox/Metadata/Refreshes",
            get(list_refreshes).post(enqueue_refresh),
        )
        .route(
            "/Puffinbox/Metadata/Items/{item_id}",
            get(get_item_metadata),
        )
        .route(
            "/Puffinbox/Metadata/Items/{item_id}/Artwork",
            get(get_item_artwork),
        )
        .route("/Items/{item_id}/Images/Primary", get(get_primary_image))
        .with_state(state)
}

/// Start the persistent metadata queue after the database run has been fenced.
pub fn start_worker(state: AppState) {
    worker::start(state);
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase", deny_unknown_fields)]
struct EnqueueRequest {
    library_id: Option<Uuid>,
    item_id: Option<Uuid>,
    providers: Vec<String>,
    /// Explicit opt-in to import TVMaze's CC BY-SA data for this refresh.
    #[serde(default)]
    allow_cc_by_sa_provider: bool,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct EnqueueResponse {
    jobs: Vec<QueuedJobDto>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct QueuedJobDto {
    id: Uuid,
    provider_key: String,
    status: &'static str,
}

async fn enqueue_refresh(
    State(state): State<AppState>,
    AdminUser(admin): AdminUser,
    Json(request): Json<EnqueueRequest>,
) -> Result<(StatusCode, Json<EnqueueResponse>), ApiError> {
    if request.library_id.is_some() == request.item_id.is_some() {
        return Err(ApiError::BadRequest(
            "Specify exactly one of LibraryId or ItemId".to_owned(),
        ));
    }
    if request.providers.is_empty() || request.providers.len() > 8 {
        return Err(ApiError::BadRequest(
            "Providers must contain between 1 and 8 entries".to_owned(),
        ));
    }
    let mut providers = request.providers;
    providers.sort();
    providers.dedup();
    if providers.len() > 8 {
        return Err(ApiError::BadRequest("Too many providers".to_owned()));
    }
    if providers.iter().any(|key| key == "tvmaze") && !request.allow_cc_by_sa_provider {
        return Err(ApiError::BadRequest(
            "TVMaze data is CC BY-SA; set AllowCcBySaProvider=true to opt in".to_owned(),
        ));
    }
    for provider in &providers {
        if !valid_provider_key(provider) {
            return Err(ApiError::BadRequest(format!(
                "Unsupported provider key: {provider}"
            )));
        }
    }

    let scope_input = request
        .library_id
        .map(ScopeInput::Library)
        .or_else(|| request.item_id.map(ScopeInput::Item))
        .expect("exactly one scope was validated");

    let mut tx = state.db.begin().await?;
    db::require_active_run(&mut tx, state.run_id).await?;
    let scope = match scope_input {
        ScopeInput::Library(library_id) => {
            let row =
                sqlx::query("SELECT name FROM libraries WHERE id=$1 AND enabled=TRUE FOR SHARE")
                    .bind(library_id)
                    .fetch_optional(&mut *tx)
                    .await?
                    .ok_or(ApiError::NotFound)?;
            Scope::Library {
                id: library_id,
                name: row.try_get("name")?,
            }
        }
        ScopeInput::Item(item_id) => {
            let row = sqlx::query("SELECT i.id FROM items i JOIN libraries l ON l.id=i.library_id WHERE i.id=$1 AND l.enabled=TRUE FOR SHARE OF i,l")
                .bind(item_id)
                .fetch_optional(&mut *tx)
                .await?;
            row.ok_or(ApiError::NotFound)?;
            Scope::Item(item_id)
        }
    };
    let mut jobs = Vec::with_capacity(providers.len());
    for provider in providers {
        if let Some(plugin_id) = provider.strip_prefix("plugin:") {
            let enabled: bool = sqlx::query_scalar(
                "SELECT enabled FROM trusted_plugins WHERE plugin_id=$1 AND status='enabled'",
            )
            .bind(plugin_id)
            .fetch_optional(&mut *tx)
            .await?
            .unwrap_or(false);
            if !enabled {
                return Err(ApiError::Conflict(format!(
                    "Plugin provider {plugin_id} is not enabled"
                )));
            }
        }
        let id = Uuid::new_v4();
        let result = match &scope {
            Scope::Library { id: library_id, name } => {
                sqlx::query("INSERT INTO metadata_refresh_runs(id,scope_kind,scope_library_id,scope_library_name,provider_key,status,batch_limit,requested_by) VALUES ($1,'library',$2,$3,$4,'queued',250,$5)")
                    .bind(id).bind(library_id).bind(name).bind(&provider).bind(admin.id).execute(&mut *tx).await
            }
            Scope::Item(item_id) => {
                sqlx::query("INSERT INTO metadata_refresh_runs(id,scope_kind,scope_item_id,provider_key,status,batch_limit,requested_by) VALUES ($1,'item',$2,$3,'queued',1,$4)")
                    .bind(id).bind(item_id).bind(&provider).bind(admin.id).execute(&mut *tx).await
            }
        };
        if let Err(error) = result {
            if error
                .as_database_error()
                .is_some_and(|database| database.code().as_deref() == Some("23505"))
            {
                return Err(ApiError::Conflict(
                    "A refresh for this provider and scope is already queued or running".to_owned(),
                ));
            }
            return Err(error.into());
        }
        jobs.push(QueuedJobDto {
            id,
            provider_key: provider,
            status: "queued",
        });
    }
    tx.commit().await?;
    Ok((StatusCode::ACCEPTED, Json(EnqueueResponse { jobs })))
}

enum Scope {
    Library { id: Uuid, name: String },
    Item(Uuid),
}

enum ScopeInput {
    Library(Uuid),
    Item(Uuid),
}

fn valid_provider_key(value: &str) -> bool {
    if matches!(value, "local-nfo" | "tvmaze") {
        return true;
    }
    value
        .strip_prefix("plugin:")
        .is_some_and(crate::plugins::valid_plugin_id)
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct RefreshQuery {
    limit: Option<i64>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct RefreshDto {
    id: Uuid,
    scope_kind: String,
    library_id: Option<Uuid>,
    library_name: Option<String>,
    item_id: Option<Uuid>,
    provider_key: String,
    status: String,
    items_seen: i64,
    items_succeeded: i64,
    items_errors: i64,
    attempt_count: i16,
    next_attempt_at: Option<DateTime<Utc>>,
    last_error_code: Option<String>,
    created_at: DateTime<Utc>,
    started_at: Option<DateTime<Utc>>,
    finished_at: Option<DateTime<Utc>>,
    updated_at: DateTime<Utc>,
}

async fn list_refreshes(
    State(state): State<AppState>,
    AdminUser(_admin): AdminUser,
    axum::extract::Query(query): axum::extract::Query<RefreshQuery>,
) -> Result<Json<Vec<RefreshDto>>, ApiError> {
    let limit = query.limit.unwrap_or(100).clamp(1, 500);
    let rows = sqlx::query("SELECT id,scope_kind,scope_library_id,scope_library_name,scope_item_id,provider_key,status,items_seen,items_succeeded,items_errors,attempt_count,next_attempt_at,last_error_code,created_at,started_at,finished_at,updated_at FROM metadata_refresh_runs ORDER BY created_at DESC,id DESC LIMIT $1")
        .bind(limit)
        .fetch_all(&state.db)
        .await?;
    let mut output = Vec::with_capacity(rows.len());
    for row in rows {
        output.push(RefreshDto {
            id: row.try_get("id")?,
            scope_kind: row.try_get("scope_kind")?,
            library_id: row.try_get("scope_library_id")?,
            library_name: row.try_get("scope_library_name")?,
            item_id: row.try_get("scope_item_id")?,
            provider_key: row.try_get("provider_key")?,
            status: row.try_get("status")?,
            items_seen: row.try_get("items_seen")?,
            items_succeeded: row.try_get("items_succeeded")?,
            items_errors: row.try_get("items_errors")?,
            attempt_count: row.try_get("attempt_count")?,
            next_attempt_at: row.try_get("next_attempt_at")?,
            last_error_code: row.try_get("last_error_code")?,
            created_at: row.try_get("created_at")?,
            started_at: row.try_get("started_at")?,
            finished_at: row.try_get("finished_at")?,
            updated_at: row.try_get("updated_at")?,
        });
    }
    Ok(Json(output))
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct ItemMetadataResponse {
    item_id: Uuid,
    preferred: DisplayMetadata,
    providers: Vec<ProviderMetadata>,
}

async fn get_item_metadata(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(item_id): Path<Uuid>,
) -> Result<(HeaderMap, Json<ItemMetadataResponse>), ApiError> {
    let _item = authorized_item(&state, &user, item_id).await?;
    let preferred = load_display_metadata(&state.db, &[item_id])
        .await?
        .remove(&item_id)
        .unwrap_or_default();
    let providers = load_provider_details(&state.db, item_id).await?;
    let mut headers = HeaderMap::new();
    headers.insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, no-store"),
    );
    Ok((
        headers,
        Json(ItemMetadataResponse {
            item_id,
            preferred,
            providers,
        }),
    ))
}

async fn get_primary_image(
    State(state): State<AppState>,
    MediaUser(user): MediaUser,
    Path(item_id): Path<Uuid>,
    method: Method,
    request_headers: HeaderMap,
) -> Result<Response, ApiError> {
    let item = authorized_item(&state, &user, item_id).await?;
    if item.item_type == "Photo" {
        return crate::media_features::stream_photo(
            &state,
            &user,
            item_id,
            &method,
            &request_headers,
        )
        .await;
    }
    get_item_artwork(
        State(state),
        CurrentUser(user),
        Path(item_id),
        request_headers,
    )
    .await
}

async fn get_item_artwork(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(item_id): Path<Uuid>,
    request_headers: HeaderMap,
) -> Result<Response, ApiError> {
    let _item = authorized_item(&state, &user, item_id).await?;
    let row = sqlx::query("SELECT m.artwork_mime,m.artwork_bytes,m.artwork_sha256 FROM item_metadata m WHERE m.item_id=$1 AND m.artwork_bytes IS NOT NULL AND (m.provider_key IN ('local-nfo','tvmaze') OR (m.provider_key LIKE 'plugin:%' AND EXISTS (SELECT 1 FROM trusted_plugins p WHERE p.plugin_id=substring(m.provider_key FROM 8) AND p.enabled=TRUE AND p.status='enabled' AND m.metadata_json->>'manifestSha256'=p.manifest_sha256 AND m.metadata_json->>'moduleSha256'=p.binary_sha256))) ORDER BY CASE m.provider_key WHEN 'local-nfo' THEN 0 WHEN 'tvmaze' THEN 2 ELSE 1 END,m.provider_key LIMIT 1")
        .bind(item_id)
        .fetch_optional(&state.db)
        .await?
        .ok_or(ApiError::NotFound)?;
    let mime: String = row.try_get("artwork_mime")?;
    let bytes: Vec<u8> = row.try_get("artwork_bytes")?;
    let digest: String = row.try_get("artwork_sha256")?;
    if !matches!(mime.as_str(), "image/jpeg" | "image/png" | "image/webp")
        || bytes.len() > 4 * 1024 * 1024
    {
        return Err(ApiError::NotFound);
    }
    let actual_digest = Sha256::digest(&bytes)
        .iter()
        .map(|byte| format!("{byte:02x}"))
        .collect::<String>();
    if actual_digest != digest.trim() {
        return Err(ApiError::Internal(
            "stored artwork digest did not match its bytes".to_owned(),
        ));
    }
    let etag = format!("\"{actual_digest}\"");
    if if_none_match_matches(&request_headers, &actual_digest) {
        let mut response = Response::new(Body::empty());
        *response.status_mut() = StatusCode::NOT_MODIFIED;
        response.headers_mut().insert(
            header::ETAG,
            HeaderValue::from_str(&etag)
                .map_err(|_| ApiError::Internal("stored artwork hash was invalid".to_owned()))?,
        );
        response.headers_mut().insert(
            header::CACHE_CONTROL,
            HeaderValue::from_static("private, max-age=0, must-revalidate"),
        );
        return Ok(response);
    }
    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        header::CONTENT_TYPE,
        HeaderValue::from_str(&mime)
            .map_err(|_| ApiError::Internal("stored artwork MIME was invalid".to_owned()))?,
    );
    response.headers_mut().insert(
        header::ETAG,
        HeaderValue::from_str(&etag)
            .map_err(|_| ApiError::Internal("stored artwork hash was invalid".to_owned()))?,
    );
    response.headers_mut().insert(
        header::CACHE_CONTROL,
        HeaderValue::from_static("private, max-age=0, must-revalidate"),
    );
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}

fn if_none_match_matches(headers: &HeaderMap, digest: &str) -> bool {
    headers
        .get_all(header::IF_NONE_MATCH)
        .iter()
        .filter_map(|value| value.to_str().ok())
        .any(|value| {
            value.split(',').any(|candidate| {
                let candidate = candidate.trim();
                if candidate == "*" {
                    return true;
                }
                let candidate = candidate.strip_prefix("W/").unwrap_or(candidate).trim();
                candidate
                    .strip_prefix('"')
                    .and_then(|candidate| candidate.strip_suffix('"'))
                    == Some(digest)
            })
        })
}

async fn authorized_item(
    state: &AppState,
    user: &UserRecord,
    item_id: Uuid,
) -> Result<ItemRecord, ApiError> {
    let item = db::get_item(&state.db, item_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !db::item_visible_to_user(&state.db, user, &item).await? {
        return Err(ApiError::NotFound);
    }
    Ok(item)
}

#[cfg(test)]
mod artwork_tests {
    use axum::http::{HeaderMap, HeaderValue, header};

    use super::if_none_match_matches;

    #[test]
    fn image_etag_match_accepts_lists_weak_tags_and_wildcard() {
        let digest = "a".repeat(64);
        for header_value in [
            format!("W/\"{digest}\""),
            format!("\"{}\", \"{digest}\"", "b".repeat(64)),
            "*".to_owned(),
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(
                header::IF_NONE_MATCH,
                HeaderValue::from_str(&header_value).unwrap(),
            );
            assert!(if_none_match_matches(&headers, &digest), "{header_value}");
        }
    }

    #[test]
    fn image_etag_mismatch_does_not_match() {
        let mut headers = HeaderMap::new();
        headers.insert(
            header::IF_NONE_MATCH,
            HeaderValue::from_static("\"old-digest\""),
        );
        assert!(!if_none_match_matches(&headers, &"a".repeat(64)));
    }

    #[test]
    fn image_etag_match_checks_repeated_header_fields() {
        let digest = "c".repeat(64);
        let mut headers = HeaderMap::new();
        headers.append(header::IF_NONE_MATCH, HeaderValue::from_static("\"older\""));
        headers.append(
            header::IF_NONE_MATCH,
            HeaderValue::from_str(&format!("\"{digest}\"")).unwrap(),
        );
        assert!(if_none_match_matches(&headers, &digest));
    }
}
