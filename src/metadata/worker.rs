use std::{path::PathBuf, sync::atomic::Ordering, time::Duration};

use chrono::NaiveDate;
use serde_json::{Value, json};
use sqlx::{Postgres, Row, Transaction, types::Json};
use uuid::Uuid;

use crate::{
    db,
    error::ApiError,
    media_features::{self, AdjacentFileRead},
    plugins::{self, PluginInput},
    state::AppState,
};

use super::{nfo, tvmaze};

const POLL_INTERVAL: Duration = Duration::from_secs(1);
const MAX_BATCH: i64 = 500;
const MAX_ARTWORK_BYTES: usize = 4 * 1024 * 1024;
const MAX_RETRIES: i16 = 5;

pub(super) fn start(state: AppState) {
    tokio::spawn(run(state));
}

async fn run(state: AppState) {
    while !state.shutdown_requested.load(Ordering::Acquire) {
        match claim_next_job(&state).await {
            Ok(Some(job)) => {
                let job_id = job.id;
                if let Err(error) = process_job(&state, job).await {
                    if is_stale_run(&error) {
                        tracing::warn!(
                            "metadata worker stopped after losing the active server run"
                        );
                        return;
                    }
                    tracing::warn!(error = %error, "metadata refresh worker could not finish a job");
                    loop {
                        if state.shutdown_requested.load(Ordering::Acquire) {
                            return;
                        }
                        match release_failed_job(&state, job_id).await {
                            Ok(()) => break,
                            Err(recovery_error) if is_stale_run(&recovery_error) => {
                                tracing::warn!(
                                    "metadata worker stopped after losing the active server run"
                                );
                                return;
                            }
                            Err(recovery_error) => {
                                tracing::warn!(error = %recovery_error, "metadata worker could not release a failed job claim; will retry");
                                tokio::time::sleep(POLL_INTERVAL).await;
                            }
                        }
                    }
                }
            }
            Ok(None) => tokio::time::sleep(POLL_INTERVAL).await,
            Err(error) if is_stale_run(&error) => {
                tracing::warn!("metadata worker stopped after losing the active server run");
                return;
            }
            Err(error) => {
                tracing::warn!(error = %error, "metadata worker could not claim a job");
                tokio::time::sleep(POLL_INTERVAL).await;
            }
        }
    }
}

#[derive(Clone, Debug)]
struct Job {
    id: Uuid,
    scope_kind: String,
    scope_library_id: Option<Uuid>,
    scope_item_id: Option<Uuid>,
    provider_key: String,
    cursor_item_id: Option<Uuid>,
    upper_item_id: Option<Uuid>,
    batch_limit: i16,
    attempt_count: i16,
}

async fn claim_next_job(state: &AppState) -> Result<Option<Job>, sqlx::Error> {
    let mut tx = state.db.begin().await?;
    db::require_active_run(&mut tx, state.run_id).await?;
    let Some(row) = sqlx::query("SELECT id,scope_kind,scope_library_id,scope_item_id,provider_key,cursor_item_id,upper_item_id,batch_limit,attempt_count FROM metadata_refresh_runs WHERE status IN ('queued','retry_wait') AND (next_attempt_at IS NULL OR next_attempt_at<=NOW()) ORDER BY created_at,id LIMIT 1 FOR UPDATE SKIP LOCKED")
        .fetch_optional(&mut *tx)
        .await? else {
            tx.commit().await?;
            return Ok(None);
        };
    let mut job = Job {
        id: row.try_get("id")?,
        scope_kind: row.try_get("scope_kind")?,
        scope_library_id: row.try_get("scope_library_id")?,
        scope_item_id: row.try_get("scope_item_id")?,
        provider_key: row.try_get("provider_key")?,
        cursor_item_id: row.try_get("cursor_item_id")?,
        upper_item_id: row.try_get("upper_item_id")?,
        batch_limit: row.try_get("batch_limit")?,
        attempt_count: row.try_get("attempt_count")?,
    };

    if !scope_enabled(&mut tx, &job).await? {
        sqlx::query("UPDATE metadata_refresh_runs SET status='cancelled',claimed_run_id=NULL,finished_at=NOW(),updated_at=NOW(),last_error_code='scope-disabled' WHERE id=$1 AND status IN ('queued','retry_wait')")
            .bind(job.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(None);
    }
    if let Some(plugin_id) = job.provider_key.strip_prefix("plugin:") {
        let enabled: bool = sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM trusted_plugins WHERE plugin_id=$1 AND enabled=TRUE AND status='enabled')")
            .bind(plugin_id)
            .fetch_one(&mut *tx)
            .await?;
        if !enabled {
            sqlx::query("UPDATE metadata_refresh_runs SET status='failed',claimed_run_id=NULL,finished_at=NOW(),updated_at=NOW(),last_error_code='plugin-not-enabled' WHERE id=$1 AND status IN ('queued','retry_wait')")
                .bind(job.id)
                .execute(&mut *tx)
                .await?;
            tx.commit().await?;
            return Ok(None);
        }
    }
    if job.upper_item_id.is_none() {
        job.upper_item_id = scope_upper_bound(&mut tx, &job).await?;
    }
    job.attempt_count = job.attempt_count.saturating_add(1).min(MAX_RETRIES);
    sqlx::query("UPDATE metadata_refresh_runs SET status='running',claimed_run_id=$2,upper_item_id=$3,attempt_count=$4,next_attempt_at=NULL,started_at=COALESCE(started_at,NOW()),finished_at=NULL,updated_at=NOW() WHERE id=$1")
        .bind(job.id)
        .bind(state.run_id)
        .bind(job.upper_item_id)
        .bind(job.attempt_count)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(Some(job))
}

async fn scope_enabled(tx: &mut Transaction<'_, Postgres>, job: &Job) -> Result<bool, sqlx::Error> {
    match (
        job.scope_kind.as_str(),
        job.scope_library_id,
        job.scope_item_id,
    ) {
        ("library", Some(library_id), None) => {
            sqlx::query_scalar("SELECT enabled FROM libraries WHERE id=$1 FOR SHARE")
                .bind(library_id)
                .fetch_optional(&mut **tx)
                .await
                .map(|enabled: Option<bool>| enabled.unwrap_or(false))
        }
        ("item", None, Some(item_id)) => {
            let row = sqlx::query("SELECT i.id FROM items i JOIN libraries l ON l.id=i.library_id WHERE i.id=$1 AND l.enabled=TRUE FOR SHARE OF i,l")
                .bind(item_id)
                .fetch_optional(&mut **tx)
                .await?;
            Ok(row.is_some())
        }
        _ => Ok(false),
    }
}

async fn scope_upper_bound(
    tx: &mut Transaction<'_, Postgres>,
    job: &Job,
) -> Result<Option<Uuid>, sqlx::Error> {
    let filter_series = job.provider_key == "tvmaze";
    match (job.scope_kind.as_str(), job.scope_library_id, job.scope_item_id) {
        ("library", Some(library_id), None) if filter_series => {
            sqlx::query_scalar("SELECT id FROM items WHERE library_id=$1 AND item_type='Series' ORDER BY id DESC LIMIT 1")
                .bind(library_id)
                .fetch_optional(&mut **tx)
                .await
        }
        ("library", Some(library_id), None) => {
            sqlx::query_scalar("SELECT id FROM items WHERE library_id=$1 ORDER BY id DESC LIMIT 1")
                .bind(library_id)
                .fetch_optional(&mut **tx)
                .await
        }
        ("item", None, Some(item_id)) if filter_series => {
            sqlx::query_scalar("SELECT id FROM items WHERE id=$1 AND item_type='Series'")
                .bind(item_id)
                .fetch_optional(&mut **tx)
                .await
        }
        ("item", None, Some(item_id)) => {
            sqlx::query_scalar("SELECT id FROM items WHERE id=$1")
                .bind(item_id)
                .fetch_optional(&mut **tx)
                .await
        }
        _ => Ok(None),
    }
}

async fn process_job(state: &AppState, mut job: Job) -> Result<(), sqlx::Error> {
    let Some(upper) = job.upper_item_id else {
        return finish_job(state, &job).await;
    };
    loop {
        if state.shutdown_requested.load(Ordering::Acquire) {
            return Ok(());
        }
        let batch = load_batch(state, &job, upper).await?;
        if batch.is_empty() {
            return finish_job(state, &job).await;
        }
        for item in batch {
            if state.shutdown_requested.load(Ordering::Acquire) {
                return Ok(());
            }
            let outcome = prepare_item(state, &job.provider_key, &item).await;
            let result = persist_item_outcome(state, &job, &item, outcome).await?;
            match result {
                ItemPersistResult::Continue { cursor } => job.cursor_item_id = Some(cursor),
                ItemPersistResult::Retry => return Ok(()),
                ItemPersistResult::Cancelled | ItemPersistResult::Stale => return Ok(()),
            }
        }
    }
}

#[derive(Clone, Debug)]
struct WorkItem {
    id: Uuid,
    name: String,
    item_type: String,
    path: PathBuf,
    overview: Option<String>,
}

async fn load_batch(
    state: &AppState,
    job: &Job,
    upper: Uuid,
) -> Result<Vec<WorkItem>, sqlx::Error> {
    let limit = i64::from(job.batch_limit).clamp(1, MAX_BATCH);
    let rows = match (job.scope_kind.as_str(), job.scope_library_id, job.scope_item_id) {
        ("library", Some(library_id), None) if job.provider_key == "tvmaze" => {
            sqlx::query("SELECT id,name,item_type,path,overview FROM items WHERE library_id=$1 AND id<=$2 AND ($3::uuid IS NULL OR id>$3) AND item_type='Series' ORDER BY id LIMIT $4")
                .bind(library_id).bind(upper).bind(job.cursor_item_id).bind(limit).fetch_all(&state.db).await?
        }
        ("library", Some(library_id), None) => {
            sqlx::query("SELECT id,name,item_type,path,overview FROM items WHERE library_id=$1 AND id<=$2 AND ($3::uuid IS NULL OR id>$3) ORDER BY id LIMIT $4")
                .bind(library_id).bind(upper).bind(job.cursor_item_id).bind(limit).fetch_all(&state.db).await?
        }
        ("item", None, Some(item_id)) if job.provider_key == "tvmaze" => {
            if job.cursor_item_id.is_some() { return Ok(Vec::new()); }
            sqlx::query("SELECT id,name,item_type,path,overview FROM items WHERE id=$1 AND item_type='Series'")
                .bind(item_id).fetch_all(&state.db).await?
        }
        ("item", None, Some(item_id)) => {
            if job.cursor_item_id.is_some() { return Ok(Vec::new()); }
            sqlx::query("SELECT id,name,item_type,path,overview FROM items WHERE id=$1")
                .bind(item_id).fetch_all(&state.db).await?
        }
        _ => return Ok(Vec::new()),
    };
    rows.into_iter()
        .map(|row| {
            Ok(WorkItem {
                id: row.try_get("id")?,
                name: row.try_get("name")?,
                item_type: row.try_get("item_type")?,
                path: PathBuf::from(row.try_get::<String, _>("path")?),
                overview: row.try_get("overview")?,
            })
        })
        .collect()
}

#[derive(Debug)]
enum PreparedOutcome {
    Write(Box<MetadataWrite>),
    Delete,
    ClearLocalPolicy { code: &'static str },
    ClearLocalMetadata,
    NoChange,
    Failure { code: &'static str, retryable: bool },
}

#[derive(Clone, Debug)]
struct MetadataWrite {
    provider_key: String,
    external_id: Option<String>,
    title: Option<String>,
    overview: Option<String>,
    premiere_date: Option<NaiveDate>,
    genres: Vec<String>,
    metadata_json: Value,
    content_rating: Option<String>,
    policy_rating_value: Option<i16>,
    artwork: ArtworkMutation,
    attribution_name: Option<String>,
    attribution_url: Option<String>,
    attribution_license: Option<String>,
}

#[derive(Clone, Debug)]
enum ArtworkMutation {
    Keep,
    Clear,
    Replace {
        mime: &'static str,
        bytes: Vec<u8>,
        sha256: String,
    },
}

async fn prepare_item(state: &AppState, provider: &str, item: &WorkItem) -> PreparedOutcome {
    match provider {
        "local-nfo" => prepare_local_nfo(state, item).await,
        "tvmaze" => prepare_tvmaze(state, item).await,
        _ => {
            let Some(plugin_id) = provider.strip_prefix("plugin:") else {
                return PreparedOutcome::Failure {
                    code: "provider-unknown",
                    retryable: false,
                };
            };
            prepare_plugin(state, plugin_id, item).await
        }
    }
}

async fn prepare_local_nfo(state: &AppState, item: &WorkItem) -> PreparedOutcome {
    let filename = nfo_filename(item);
    let parsed = match filename.as_deref() {
        Some(filename) => match read_item_asset(state, item, filename, nfo::MAX_NFO_BYTES).await {
            Ok(AdjacentFileRead::Missing) => None,
            Ok(AdjacentFileRead::Unsafe) => {
                return PreparedOutcome::ClearLocalPolicy {
                    code: "nfo-sidecar-unsafe",
                };
            }
            Ok(AdjacentFileRead::Content(bytes)) => match nfo::parse(&bytes) {
                Ok(parsed) => Some(parsed),
                Err(error) => {
                    tracing::warn!(item_id = %item.id, code = error, "local metadata sidecar was rejected");
                    return PreparedOutcome::ClearLocalPolicy {
                        code: "nfo-invalid-document",
                    };
                }
            },
            Err(error) => return map_read_error(error),
        },
        None => None,
    };

    // NFO and artwork are independent inputs. In particular, a poster-only
    // folder must still produce a metadata row, and an unavailable/unsafe
    // image must not prevent a valid content classification from being saved.
    let artwork_read = read_artwork(state, item).await;
    local_nfo_outcome(parsed, artwork_read, filename.is_some())
}

fn local_nfo_outcome(
    parsed: Option<nfo::LocalNfo>,
    artwork_read: ArtworkRead,
    has_nfo_sidecar: bool,
) -> PreparedOutcome {
    if parsed.is_none() {
        match &artwork_read {
            ArtworkRead::Missing => {
                return if has_nfo_sidecar {
                    PreparedOutcome::Delete
                } else {
                    PreparedOutcome::NoChange
                };
            }
            ArtworkRead::Unsafe | ArtworkRead::Unavailable => {
                return if has_nfo_sidecar {
                    PreparedOutcome::ClearLocalMetadata
                } else {
                    PreparedOutcome::NoChange
                };
            }
            ArtworkRead::Found(_) => {}
        }
    }
    let artwork = match artwork_read {
        ArtworkRead::Found(artwork) => ArtworkMutation::Replace {
            mime: artwork.mime,
            sha256: artwork.sha256,
            bytes: artwork.bytes,
        },
        ArtworkRead::Missing => ArtworkMutation::Clear,
        ArtworkRead::Unsafe | ArtworkRead::Unavailable => ArtworkMutation::Keep,
    };
    let mut metadata = serde_json::Map::new();
    if let Some(year) = parsed.as_ref().and_then(|parsed| parsed.year) {
        metadata.insert("year".to_owned(), json!(year));
    }
    if let Some(parsed) = parsed.as_ref() {
        metadata.insert("tags".to_owned(), json!(parsed.tags));
    }
    if let Some(id) = parsed.as_ref().and_then(|parsed| parsed.tvmaze_id) {
        metadata.insert("tvmazeId".to_owned(), json!(id));
    }
    PreparedOutcome::Write(Box::new(MetadataWrite {
        provider_key: "local-nfo".to_owned(),
        external_id: None,
        title: parsed.as_ref().and_then(|parsed| parsed.title.clone()),
        overview: parsed.as_ref().and_then(|parsed| parsed.overview.clone()),
        premiere_date: parsed.as_ref().and_then(|parsed| parsed.premiere_date),
        genres: parsed
            .as_ref()
            .map(|parsed| parsed.genres.clone())
            .unwrap_or_default(),
        metadata_json: Value::Object(metadata),
        content_rating: parsed
            .as_ref()
            .and_then(|parsed| parsed.content_rating.clone()),
        policy_rating_value: parsed
            .as_ref()
            .and_then(|parsed| parsed.policy_rating_value),
        artwork,
        attribution_name: None,
        attribution_url: None,
        attribution_license: None,
    }))
}

async fn prepare_tvmaze(state: &AppState, item: &WorkItem) -> PreparedOutcome {
    if item.item_type != "Series" {
        return PreparedOutcome::NoChange;
    }
    let selected = match nfo_filename(item) {
        Some(filename) => match read_item_asset(state, item, &filename, nfo::MAX_NFO_BYTES).await {
            Ok(AdjacentFileRead::Content(bytes)) => match nfo::parse(&bytes) {
                Ok(parsed) if parsed.tvmaze_id_invalid => {
                    return PreparedOutcome::Failure {
                        code: "invalid-tvmazeid",
                        retryable: false,
                    };
                }
                Ok(parsed) => parsed,
                Err(_) => {
                    return PreparedOutcome::Failure {
                        code: "nfo-invalid-for-tvmaze-choice",
                        retryable: false,
                    };
                }
            },
            Ok(AdjacentFileRead::Missing) => nfo::LocalNfo::default(),
            Ok(AdjacentFileRead::Unsafe) => {
                return PreparedOutcome::Failure {
                    code: "nfo-sidecar-unsafe",
                    retryable: false,
                };
            }
            Err(error) => return map_read_error(error),
        },
        None => nfo::LocalNfo::default(),
    };
    let result = if let Some(id) = selected.tvmaze_id {
        tvmaze::lookup_show_id(id).await
    } else {
        let title = selected.title.as_deref().unwrap_or(&item.name);
        tvmaze::lookup_exact_title(title).await
    };
    match result {
        Ok(Some(metadata)) => {
            let metadata_json = json!({
                "communityScore": metadata.community_score,
                "sourceDataLicense": metadata.attribution_license,
                "sourceDataAttribution": metadata.attribution_name,
                "sourceAttributionUrl": metadata.attribution_url,
            });
            PreparedOutcome::Write(Box::new(MetadataWrite {
                provider_key: "tvmaze".to_owned(),
                external_id: Some(metadata.external_id),
                title: Some(metadata.title),
                overview: metadata.overview,
                premiere_date: metadata.premiere_date,
                genres: metadata.genres,
                metadata_json,
                content_rating: None,
                policy_rating_value: None,
                artwork: ArtworkMutation::Keep,
                attribution_name: Some(metadata.attribution_name.to_owned()),
                attribution_url: Some(metadata.attribution_url.to_owned()),
                attribution_license: Some(metadata.attribution_license.to_owned()),
            }))
        }
        Ok(None) => PreparedOutcome::Delete,
        Err(tvmaze::LookupError::Ambiguous) => PreparedOutcome::Failure {
            code: "ambiguous-title-requires-tvmazeid",
            retryable: false,
        },
        Err(tvmaze::LookupError::NoPublicAddress | tvmaze::LookupError::Network) => {
            PreparedOutcome::Failure {
                code: "tvmaze-network",
                retryable: true,
            }
        }
        Err(tvmaze::LookupError::HttpStatus(429 | 500..=599)) => PreparedOutcome::Failure {
            code: "tvmaze-temporarily-unavailable",
            retryable: true,
        },
        Err(tvmaze::LookupError::HttpStatus(_)) => PreparedOutcome::Failure {
            code: "tvmaze-http-status",
            retryable: false,
        },
        Err(tvmaze::LookupError::TooLarge) => PreparedOutcome::Failure {
            code: "tvmaze-response-too-large",
            retryable: false,
        },
        Err(tvmaze::LookupError::InvalidData) => PreparedOutcome::Failure {
            code: "tvmaze-response-invalid",
            retryable: false,
        },
    }
}

async fn prepare_plugin(state: &AppState, plugin_id: &str, item: &WorkItem) -> PreparedOutcome {
    let input = match bounded_plugin_input(item) {
        Ok(input) => input,
        Err(code) => {
            return PreparedOutcome::Failure {
                code,
                retryable: false,
            };
        }
    };
    match plugins::run_enabled_plugin(state, plugin_id, input).await {
        Ok(execution) => {
            let output = execution.output;
            if output.overview.is_none() && output.genres.as_ref().is_none_or(Vec::is_empty) {
                return PreparedOutcome::Delete;
            }
            let metadata_json = json!({
                "manifestSha256": execution.manifest_sha256,
                "moduleSha256": execution.binary_sha256,
            });
            PreparedOutcome::Write(Box::new(MetadataWrite {
                provider_key: format!("plugin:{plugin_id}"),
                external_id: None,
                title: None,
                overview: output.overview,
                premiere_date: None,
                genres: output.genres.unwrap_or_default(),
                metadata_json,
                content_rating: None,
                policy_rating_value: None,
                artwork: ArtworkMutation::Keep,
                attribution_name: None,
                attribution_url: None,
                attribution_license: None,
            }))
        }
        Err("plugin-worker-timeout" | "plugin-worker-busy" | "plugin-database-error") => {
            PreparedOutcome::Failure {
                code: "plugin-temporarily-unavailable",
                retryable: true,
            }
        }
        Err(code) => PreparedOutcome::Failure {
            code,
            retryable: false,
        },
    }
}

fn bounded_plugin_input(item: &WorkItem) -> Result<PluginInput, &'static str> {
    let mut input = PluginInput {
        id: item.id.to_string(),
        name: truncate(&item.name, 512),
        item_type: truncate(&item.item_type, 64),
        overview: item
            .overview
            .as_deref()
            .map(|value| truncate(value, 20_000)),
    };
    loop {
        let encoded = serde_json::to_vec(&input).map_err(|_| "plugin-input-invalid")?;
        if encoded.len() <= plugins::MAX_INPUT_BYTES {
            return Ok(input);
        }
        let Some(overview) = input.overview.as_mut() else {
            return Err("plugin-input-too-large");
        };
        if overview.is_empty() {
            return Err("plugin-input-too-large");
        }
        *overview = truncate(overview, overview.len() / 2);
    }
}

fn nfo_filename(item: &WorkItem) -> Option<String> {
    match item.item_type.as_str() {
        "Series" => Some("tvshow.nfo".to_owned()),
        "MusicAlbum" => Some("album.nfo".to_owned()),
        "MusicArtist" => Some("artist.nfo".to_owned()),
        "Movie" | "Episode" | "Audio" | "Photo" | "Book" | "AudioBook" | "EBook" | "MusicVideo" => {
            let stem = item.path.file_stem()?.to_str()?;
            (!stem.is_empty() && stem.len() <= 240).then(|| format!("{stem}.nfo"))
        }
        _ => None,
    }
}

async fn read_item_asset(
    state: &AppState,
    item: &WorkItem,
    name: &str,
    max_bytes: usize,
) -> Result<AdjacentFileRead, ApiError> {
    media_features::read_adjacent_file(state, item.id, name, max_bytes).await
}

fn map_read_error(error: ApiError) -> PreparedOutcome {
    match error {
        ApiError::NotFound => PreparedOutcome::NoChange,
        ApiError::Unavailable => PreparedOutcome::Failure {
            code: "sidecar-read-unavailable",
            retryable: true,
        },
        _ => PreparedOutcome::Failure {
            code: "sidecar-read-failed",
            retryable: false,
        },
    }
}

struct Artwork {
    mime: &'static str,
    bytes: Vec<u8>,
    sha256: String,
}

enum ArtworkRead {
    Found(Artwork),
    Missing,
    Unsafe,
    Unavailable,
}

async fn read_artwork(state: &AppState, item: &WorkItem) -> ArtworkRead {
    let candidates = if matches!(
        item.item_type.as_str(),
        "Series" | "MusicAlbum" | "MusicArtist"
    ) {
        vec![
            "poster.jpg".to_owned(),
            "poster.png".to_owned(),
            "poster.webp".to_owned(),
            "folder.jpg".to_owned(),
            "folder.png".to_owned(),
            "folder.webp".to_owned(),
        ]
    } else {
        let Some(stem) = item.path.file_stem().and_then(|value| value.to_str()) else {
            return ArtworkRead::Missing;
        };
        if stem.is_empty() || stem.len() > 220 {
            return ArtworkRead::Missing;
        }
        ["jpg", "png", "webp"]
            .into_iter()
            .map(|extension| format!("{stem}-poster.{extension}"))
            .collect()
    };
    for candidate in candidates {
        match read_item_asset(state, item, &candidate, MAX_ARTWORK_BYTES).await {
            Ok(AdjacentFileRead::Missing) => continue,
            Ok(AdjacentFileRead::Unsafe) => return ArtworkRead::Unsafe,
            Ok(AdjacentFileRead::Content(bytes)) => {
                let Some(mime) = sniff_raster(&bytes) else {
                    return ArtworkRead::Unsafe;
                };
                let sha256 = Sha256Hex::digest(&bytes);
                return ArtworkRead::Found(Artwork {
                    mime,
                    bytes,
                    sha256,
                });
            }
            Err(ApiError::NotFound) => continue,
            Err(ApiError::Unavailable) => return ArtworkRead::Unavailable,
            Err(_) => return ArtworkRead::Unsafe,
        }
    }
    ArtworkRead::Missing
}

fn sniff_raster(bytes: &[u8]) -> Option<&'static str> {
    if bytes.starts_with(b"\xff\xd8\xff") {
        Some("image/jpeg")
    } else if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
        Some("image/png")
    } else if bytes.len() >= 12 && &bytes[..4] == b"RIFF" && &bytes[8..12] == b"WEBP" {
        Some("image/webp")
    } else {
        None
    }
}

struct Sha256Hex;

impl Sha256Hex {
    fn digest(bytes: &[u8]) -> String {
        use sha2::{Digest, Sha256};
        Sha256::digest(bytes)
            .iter()
            .map(|byte| format!("{byte:02x}"))
            .collect()
    }
}

async fn persist_item_outcome(
    state: &AppState,
    job: &Job,
    item: &WorkItem,
    mut outcome: PreparedOutcome,
) -> Result<ItemPersistResult, sqlx::Error> {
    let mut tx = state.db.begin().await?;
    db::require_active_run(&mut tx, state.run_id).await?;
    let job_row = sqlx::query("SELECT attempt_count FROM metadata_refresh_runs WHERE id=$1 AND status='running' AND claimed_run_id=$2 FOR UPDATE")
        .bind(job.id)
        .bind(state.run_id)
        .fetch_optional(&mut *tx)
        .await?;
    let Some(job_row) = job_row else {
        tx.rollback().await?;
        return Ok(ItemPersistResult::Stale);
    };
    if !scope_enabled(&mut tx, job).await? {
        sqlx::query("UPDATE metadata_refresh_runs SET status='cancelled',claimed_run_id=NULL,finished_at=NOW(),updated_at=NOW(),last_error_code='scope-disabled' WHERE id=$1")
            .bind(job.id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(ItemPersistResult::Cancelled);
    }
    let item_in_scope: bool = match (
        job.scope_kind.as_str(),
        job.scope_library_id,
        job.scope_item_id,
    ) {
        ("library", Some(library_id), None) => {
            sqlx::query_scalar("SELECT EXISTS(SELECT 1 FROM items WHERE id=$1 AND library_id=$2)")
                .bind(item.id)
                .bind(library_id)
                .fetch_one(&mut *tx)
                .await?
        }
        ("item", None, Some(item_id)) => item_id == item.id,
        _ => false,
    };
    if !item_in_scope {
        sqlx::query("UPDATE metadata_refresh_runs SET cursor_item_id=$2,attempt_count=0,items_seen=items_seen+1,items_errors=items_errors+1,last_error_code='item-left-refresh-scope',updated_at=NOW() WHERE id=$1 AND status='running' AND claimed_run_id=$3")
            .bind(job.id)
            .bind(item.id)
            .bind(state.run_id)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(ItemPersistResult::Continue { cursor: item.id });
    }

    if let Some(plugin_id) = job.provider_key.strip_prefix("plugin:") {
        let expected_hashes = match &outcome {
            PreparedOutcome::Write(write) => Some((
                write
                    .metadata_json
                    .get("manifestSha256")
                    .and_then(Value::as_str),
                write
                    .metadata_json
                    .get("moduleSha256")
                    .and_then(Value::as_str),
            )),
            _ => None,
        };
        let valid = match expected_hashes {
            Some((Some(manifest_hash), Some(module_hash))) => {
                plugins::verify_enabled_plugin_tx(
                    &mut tx,
                    plugin_id,
                    Some((manifest_hash, module_hash)),
                )
                .await?
            }
            Some(_) => false,
            None => plugins::verify_enabled_plugin_tx(&mut tx, plugin_id, None).await?,
        };
        if !valid {
            outcome = PreparedOutcome::Failure {
                code: "plugin-trust-changed",
                retryable: false,
            };
        }
    }

    let attempt_count: i16 = job_row.try_get("attempt_count")?;
    if let PreparedOutcome::Failure {
        code,
        retryable: true,
    } = &outcome
        && attempt_count < MAX_RETRIES
    {
        let delay = 1_i64 << i32::from(attempt_count.saturating_sub(1).clamp(0, 6));
        sqlx::query("UPDATE metadata_refresh_runs SET status='retry_wait',claimed_run_id=NULL,next_attempt_at=NOW()+($2 * INTERVAL '1 second'),last_error_code=$3,updated_at=NOW() WHERE id=$1")
            .bind(job.id)
            .bind(delay)
            .bind(*code)
            .execute(&mut *tx)
            .await?;
        tx.commit().await?;
        return Ok(ItemPersistResult::Retry);
    }

    match &outcome {
        PreparedOutcome::Write(write) => {
            write_metadata(&mut tx, item.id, write.as_ref().clone()).await?
        }
        PreparedOutcome::Delete => {
            sqlx::query("DELETE FROM item_metadata WHERE item_id=$1 AND provider_key=$2")
                .bind(item.id)
                .bind(&job.provider_key)
                .execute(&mut *tx)
                .await?;
        }
        PreparedOutcome::ClearLocalPolicy { .. } => {
            sqlx::query("UPDATE item_metadata SET content_rating=NULL,policy_rating_scale=NULL,policy_rating_value=NULL,updated_at=NOW() WHERE item_id=$1 AND provider_key='local-nfo'")
                .bind(item.id).execute(&mut *tx).await?;
        }
        PreparedOutcome::ClearLocalMetadata => {
            sqlx::query("UPDATE item_metadata SET external_id=NULL,title=NULL,overview=NULL,premiere_date=NULL,genres='[]'::JSONB,metadata_json='{}'::JSONB,content_rating=NULL,policy_rating_scale=NULL,policy_rating_value=NULL,updated_at=NOW() WHERE item_id=$1 AND provider_key='local-nfo'")
                .bind(item.id).execute(&mut *tx).await?;
        }
        PreparedOutcome::Failure { .. } | PreparedOutcome::NoChange => {}
    }
    let succeeded = matches!(
        outcome,
        PreparedOutcome::Write(_)
            | PreparedOutcome::Delete
            | PreparedOutcome::ClearLocalMetadata
            | PreparedOutcome::NoChange
    );
    let error_code = match &outcome {
        PreparedOutcome::Failure { code, .. } => Some(*code),
        PreparedOutcome::ClearLocalPolicy { code } => Some(*code),
        _ => None,
    };
    sqlx::query("UPDATE metadata_refresh_runs SET cursor_item_id=$2,attempt_count=0,items_seen=items_seen+1,items_succeeded=items_succeeded+CASE WHEN $3 THEN 1 ELSE 0 END,items_errors=items_errors+CASE WHEN $3 THEN 0 ELSE 1 END,last_error_code=COALESCE($4,last_error_code),updated_at=NOW() WHERE id=$1 AND status='running' AND claimed_run_id=$5")
        .bind(job.id)
        .bind(item.id)
        .bind(succeeded)
        .bind(error_code)
        .bind(state.run_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(ItemPersistResult::Continue { cursor: item.id })
}

async fn write_metadata(
    tx: &mut Transaction<'_, Postgres>,
    item_id: Uuid,
    write: MetadataWrite,
) -> Result<(), sqlx::Error> {
    let (art_mime, art_size, art_hash, art_bytes, art_action) = match write.artwork {
        ArtworkMutation::Keep => (None, None, None, None, 0_i16),
        ArtworkMutation::Clear => (None, None, None, None, 1_i16),
        ArtworkMutation::Replace {
            mime,
            bytes,
            sha256,
        } => (
            Some(mime),
            Some(bytes.len() as i32),
            Some(sha256),
            Some(bytes),
            2_i16,
        ),
    };
    sqlx::query("INSERT INTO item_metadata(item_id,provider_key,external_id,title,overview,premiere_date,genres,metadata_json,content_rating,policy_rating_scale,policy_rating_value,artwork_mime,artwork_size,artwork_sha256,artwork_bytes,attribution_name,attribution_url,attribution_license) VALUES ($1,$2,$3,$4,$5,$6,$7,$8,$9,CASE WHEN $10::smallint IS NULL THEN NULL ELSE 'US-MPAA-v1' END,$10,$11,$12,$13,$14,$15,$16,$17) ON CONFLICT(item_id,provider_key) DO UPDATE SET external_id=EXCLUDED.external_id,title=EXCLUDED.title,overview=EXCLUDED.overview,premiere_date=EXCLUDED.premiere_date,genres=EXCLUDED.genres,metadata_json=EXCLUDED.metadata_json,content_rating=EXCLUDED.content_rating,policy_rating_scale=EXCLUDED.policy_rating_scale,policy_rating_value=EXCLUDED.policy_rating_value,artwork_mime=CASE WHEN $18=0 THEN item_metadata.artwork_mime WHEN $18=1 THEN NULL ELSE EXCLUDED.artwork_mime END,artwork_size=CASE WHEN $18=0 THEN item_metadata.artwork_size WHEN $18=1 THEN NULL ELSE EXCLUDED.artwork_size END,artwork_sha256=CASE WHEN $18=0 THEN item_metadata.artwork_sha256 WHEN $18=1 THEN NULL ELSE EXCLUDED.artwork_sha256 END,artwork_bytes=CASE WHEN $18=0 THEN item_metadata.artwork_bytes WHEN $18=1 THEN NULL ELSE EXCLUDED.artwork_bytes END,attribution_name=EXCLUDED.attribution_name,attribution_url=EXCLUDED.attribution_url,attribution_license=EXCLUDED.attribution_license,updated_at=NOW()")
        .bind(item_id)
        .bind(write.provider_key)
        .bind(write.external_id)
        .bind(write.title)
        .bind(write.overview)
        .bind(write.premiere_date)
        .bind(Json(write.genres))
        .bind(Json(write.metadata_json))
        .bind(write.content_rating)
        .bind(write.policy_rating_value)
        .bind(art_mime)
        .bind(art_size)
        .bind(art_hash)
        .bind(art_bytes)
        .bind(write.attribution_name)
        .bind(write.attribution_url)
        .bind(write.attribution_license)
        .bind(art_action)
        .execute(&mut **tx)
        .await?;
    Ok(())
}

enum ItemPersistResult {
    Continue { cursor: Uuid },
    Retry,
    Cancelled,
    Stale,
}

async fn finish_job(state: &AppState, job: &Job) -> Result<(), sqlx::Error> {
    let mut tx = state.db.begin().await?;
    db::require_active_run(&mut tx, state.run_id).await?;
    sqlx::query("UPDATE metadata_refresh_runs SET status=CASE WHEN items_errors=0 THEN 'completed' ELSE 'completed_with_errors' END,claimed_run_id=NULL,next_attempt_at=NULL,finished_at=NOW(),updated_at=NOW() WHERE id=$1 AND status='running' AND claimed_run_id=$2")
        .bind(job.id)
        .bind(state.run_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

async fn release_failed_job(state: &AppState, job_id: Uuid) -> Result<(), sqlx::Error> {
    let mut tx = state.db.begin().await?;
    db::require_active_run(&mut tx, state.run_id).await?;
    sqlx::query("UPDATE metadata_refresh_runs SET status=CASE WHEN attempt_count < $3 THEN 'retry_wait' ELSE 'failed' END,claimed_run_id=NULL,next_attempt_at=CASE WHEN attempt_count < $3 THEN NOW()+INTERVAL '30 seconds' ELSE NULL END,finished_at=CASE WHEN attempt_count < $3 THEN NULL ELSE NOW() END,last_error_code='worker-database-error',updated_at=NOW() WHERE id=$1 AND status='running' AND claimed_run_id=$2")
        .bind(job_id)
        .bind(state.run_id)
        .bind(MAX_RETRIES)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(())
}

fn is_stale_run(error: &sqlx::Error) -> bool {
    matches!(error, sqlx::Error::Protocol(message) if message.contains(db::STALE_SERVER_RUN_ERROR))
}

fn truncate(value: &str, max_bytes: usize) -> String {
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use std::path::PathBuf;

    use uuid::Uuid;

    use super::{
        Artwork, ArtworkMutation, ArtworkRead, MetadataWrite, PreparedOutcome, WorkItem,
        bounded_plugin_input, local_nfo_outcome, nfo,
    };
    use crate::plugins::MAX_INPUT_BYTES;

    fn item(overview: Option<String>) -> WorkItem {
        WorkItem {
            id: Uuid::from_u128(42),
            name: "Synthetic item".to_owned(),
            item_type: "Movie".to_owned(),
            path: PathBuf::from("/media/Synthetic item.mkv"),
            overview,
        }
    }

    #[test]
    fn plugin_input_caps_the_serialized_payload_after_json_escaping() {
        let original = "\0".repeat(20_000);
        let bounded = bounded_plugin_input(&item(Some(original))).unwrap();
        let encoded = serde_json::to_vec(&bounded).unwrap();
        assert!(encoded.len() <= MAX_INPUT_BYTES);
        assert!(bounded.overview.as_ref().unwrap().len() < 20_000);
    }

    #[test]
    fn ordinary_long_overview_is_bounded_without_losing_required_fields() {
        let bounded =
            bounded_plugin_input(&item(Some("A useful summary. ".repeat(2_000)))).unwrap();
        let encoded = serde_json::to_vec(&bounded).unwrap();
        assert!(encoded.len() <= MAX_INPUT_BYTES);
        assert_eq!(bounded.id, Uuid::from_u128(42).to_string());
        assert_eq!(bounded.name, "Synthetic item");
        assert!(!bounded.overview.as_deref().unwrap_or_default().is_empty());
    }

    #[test]
    fn boxed_write_outcome_preserves_metadata_payload() {
        let outcome = PreparedOutcome::Write(Box::new(MetadataWrite {
            provider_key: "local-nfo".to_owned(),
            external_id: None,
            title: Some("A title".to_owned()),
            overview: Some("A summary".to_owned()),
            premiere_date: None,
            genres: vec!["Drama".to_owned()],
            metadata_json: serde_json::json!({ "year": 2020 }),
            content_rating: Some("PG-13".to_owned()),
            policy_rating_value: Some(50),
            artwork: super::ArtworkMutation::Keep,
            attribution_name: None,
            attribution_url: None,
            attribution_license: None,
        }));
        let PreparedOutcome::Write(write) = outcome else {
            panic!("metadata write outcome should retain its payload");
        };
        let cloned = write.as_ref().clone();
        assert_eq!(cloned.provider_key, "local-nfo");
        assert_eq!(cloned.title.as_deref(), Some("A title"));
        assert_eq!(cloned.policy_rating_value, Some(50));
        assert_eq!(cloned.genres, vec!["Drama".to_owned()]);
    }

    #[test]
    fn artwork_read_failure_does_not_block_nfo_policy_classification() {
        let parsed = nfo::LocalNfo {
            content_rating: Some("PG-13".to_owned()),
            policy_rating_value: Some(50),
            ..Default::default()
        };
        let outcome = local_nfo_outcome(Some(parsed), ArtworkRead::Unavailable, true);
        let PreparedOutcome::Write(write) = outcome else {
            panic!("a valid NFO must persist even if artwork is unavailable");
        };
        assert_eq!(write.content_rating.as_deref(), Some("PG-13"));
        assert_eq!(write.policy_rating_value, Some(50));
        assert!(matches!(write.artwork, ArtworkMutation::Keep));
    }

    #[test]
    fn poster_without_nfo_creates_artwork_only_metadata() {
        let outcome = local_nfo_outcome(
            None,
            ArtworkRead::Found(Artwork {
                mime: "image/jpeg",
                bytes: vec![0xff, 0xd8, 0xff],
                sha256: "a".repeat(64),
            }),
            true,
        );
        let PreparedOutcome::Write(write) = outcome else {
            panic!("poster-only media must import an artwork row");
        };
        assert!(write.title.is_none());
        assert!(write.content_rating.is_none());
        assert!(matches!(
            write.artwork,
            ArtworkMutation::Replace {
                mime: "image/jpeg",
                ..
            }
        ));
    }

    #[test]
    fn absent_nfo_and_artwork_removes_stale_local_provider_row() {
        assert!(matches!(
            local_nfo_outcome(None, ArtworkRead::Missing, true),
            PreparedOutcome::Delete
        ));
        assert!(matches!(
            local_nfo_outcome(None, ArtworkRead::Missing, false),
            PreparedOutcome::NoChange
        ));
    }

    #[test]
    fn unreadable_poster_without_nfo_does_not_create_or_replace_metadata() {
        assert!(matches!(
            local_nfo_outcome(None, ArtworkRead::Unavailable, true),
            PreparedOutcome::ClearLocalMetadata
        ));
        assert!(matches!(
            local_nfo_outcome(None, ArtworkRead::Unavailable, false),
            PreparedOutcome::NoChange
        ));
    }
}
