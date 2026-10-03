//! Authorized media file delivery and playback negotiation.
//!
//! This module intentionally owns every path-to-file transition: catalog paths
//! are treated as untrusted, checked against the selected library roots, and
//! revalidated before each stream-related operation.

mod books;
mod dlna;
mod hls;
mod livetv;
mod livetv_api;
mod livetv_recorder;
mod livetv_runtime;
mod playback;
mod probe;
mod process_limits;
pub mod range;
mod secure_path;
mod subtitles;
mod universal_audio;
mod vod_hls;

use std::{
    io::{self, Read, Seek, SeekFrom},
    path::{Component, Path as FsPath, PathBuf},
    sync::{Arc, OnceLock},
    time::{SystemTime, UNIX_EPOCH},
};

use axum::{
    Json, Router,
    body::Body,
    extract::{Path, Query, State},
    http::{
        HeaderMap, HeaderName, HeaderValue, Method, StatusCode,
        header::{
            ACCEPT_RANGES, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_RANGE, CONTENT_TYPE, ETAG,
            IF_RANGE, RANGE,
        },
    },
    response::Response,
    routing::{delete, get},
};
use bytes::Bytes;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use uuid::Uuid;

use crate::{
    ApiError,
    auth::{CurrentUser, MediaUser, UserRecord},
    db,
    library::ItemRecord,
    state::AppState,
};

use self::{
    playback::{PlaybackInfoRequest, PlaybackInfoResponse},
    secure_path::ResolvedMedia,
};

pub(crate) use self::secure_path::{AdjacentFileRead, OpenedMedia};

/// Routes use the public Jellyfin-style resource paths where practical. The
/// media URL itself relies on same-origin session cookies in the bundled UI or
/// standard authorization headers for external clients. Current Jellyfin-style
/// ApiKey query authentication is carried only through that HLS session's
/// generated child URLs; the bundled UI uses cookies and does not need it.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/Videos/{item_id}/stream", get(stream_video).head(stream_video))
        .route("/Videos/{item_id}/stream.{container}", get(stream_video_container).head(stream_video_container))
        .route(
            "/Audio/{item_id}/stream",
            get(stream_audio).head(stream_audio),
        )
        .route("/Audio/{item_id}/stream.{container}", get(stream_audio_container).head(stream_audio_container))
        .route(
            "/Audio/{item_id}/universal",
            get(universal_audio::stream).head(universal_audio::stream),
        )
        .route("/Items/{item_id}/File", get(stream_file).head(stream_file))
        .route("/Items/{item_id}/Download", get(download_item).head(download_item))
        .route("/Items/{item_id}/PlaybackInfo", get(playback_info_get).post(playback_info))
        .route("/Videos/{item_id}/{media_source_id}/Subtitles/{route_index}/Stream.vtt", get(subtitles::stream_vtt))
        .route("/Videos/{item_id}/{media_source_id}/Subtitles/{route_index}/Stream.srt", get(subtitles::stream_srt))
        .route("/Videos/{item_id}/{media_source_id}/Subtitles/{route_index}/{start_position_ticks}/Stream.vtt", get(subtitles::stream_vtt_at_position))
        .route("/Videos/{item_id}/{media_source_id}/Subtitles/{route_index}/{start_position_ticks}/Stream.srt", get(subtitles::stream_srt_at_position))
        .route("/Videos/{item_id}/master.m3u8", get(hls::video_master).head(hls::video_master_head))
        .route("/Audio/{item_id}/master.m3u8", get(hls::audio_master).head(hls::audio_master_head))
        .route("/Videos/{item_id}/hls/{session_id}/playlist.m3u8", get(hls::video_playlist))
        .route("/Audio/{item_id}/hls/{session_id}/playlist.m3u8", get(hls::audio_playlist))
        .route("/Videos/{item_id}/hls/{session_id}/{segment_name}", get(hls::video_segment))
        .route("/Audio/{item_id}/hls/{session_id}/{segment_name}", get(hls::audio_segment))
        .route("/Videos/{item_id}/hls/{session_id}/subtitle.m3u8", get(hls::subtitle_playlist))
        .route("/Videos/{item_id}/hls/{session_id}/subtitle.vtt", get(hls::subtitle_file))
        .route("/Videos/{item_id}/hls/{session_id}", delete(hls::stop_video_session))
        .route("/Audio/{item_id}/hls/{session_id}", delete(hls::stop_audio_session))
        .route("/Videos/{item_id}/hls/{session_id}/keepalive", axum::routing::post(hls::keepalive_video_session))
        .route("/Audio/{item_id}/hls/{session_id}/keepalive", axum::routing::post(hls::keepalive_audio_session))
        .with_state(state.clone())
        .merge(books::router(state.clone()))
        .merge(livetv_api::router(state.clone()))
        .merge(livetv_runtime::router(state.clone()))
        .merge(dlna::router(state))
}

/// Stop active media work before the database pool and runtime are closed.
pub async fn shutdown() -> bool {
    let (hls_stopped, live_stopped, recorder_stopped, dlna_stopped) = tokio::join!(
        hls::shutdown(),
        livetv_runtime::shutdown(),
        livetv_recorder::shutdown(),
        dlna::shutdown(),
    );
    hls_stopped && live_stopped && recorder_stopped && dlna_stopped
}

/// Start opt-in DLNA discovery after the server has acquired its database run.
pub async fn start_dlna(state: AppState) -> Result<(), String> {
    dlna::start(state).await
}

/// Recover bounded server-owned Live TV scratch artifacts after this process
/// has acquired the exclusive database lock.
pub async fn recover_livetv(state: &AppState) -> Result<usize, ApiError> {
    let runtime_orphans = livetv_runtime::recover_orphan_directories(state).await?;
    let recorder_jobs = livetv_recorder::recover(state).await?;
    Ok(runtime_orphans + recorder_jobs)
}

/// Start the bounded scheduler after prior-run recording state has been
/// reconciled and the process owns the active server run.
pub async fn start_livetv_recorder(state: AppState) {
    livetv_recorder::start(state).await;
}

pub(crate) async fn cancel_livetv_recording(timer_id: Uuid) -> bool {
    livetv_recorder::cancel(timer_id).await
}

async fn stream_video(
    State(state): State<AppState>,
    MediaUser(user): MediaUser,
    Path(item_id): Path<Uuid>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let media = authorized_media(&state, &user, item_id).await?;
    if !is_video_type(&media.item.item_type) {
        return Err(ApiError::NotFound);
    }
    stream_resolved(media, &headers, &method, false).await
}

async fn stream_audio(
    State(state): State<AppState>,
    MediaUser(user): MediaUser,
    Path(item_id): Path<Uuid>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let media = authorized_media(&state, &user, item_id).await?;
    if !is_audio_type(&media.item.item_type) {
        return Err(ApiError::NotFound);
    }
    stream_resolved(media, &headers, &method, false).await
}

async fn stream_video_container(
    State(state): State<AppState>,
    MediaUser(user): MediaUser,
    Path((item_id, container)): Path<(Uuid, String)>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    stream_original_container(&state, &user, item_id, &container, true, &headers, &method).await
}

async fn stream_audio_container(
    State(state): State<AppState>,
    MediaUser(user): MediaUser,
    Path((item_id, container)): Path<(Uuid, String)>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    stream_original_container(&state, &user, item_id, &container, false, &headers, &method).await
}

async fn stream_original_container(
    state: &AppState,
    user: &UserRecord,
    item_id: Uuid,
    container: &str,
    video: bool,
    headers: &HeaderMap,
    method: &Method,
) -> Result<Response, ApiError> {
    if !valid_stream_container(container) {
        return Err(ApiError::NotFound);
    }
    let media = authorized_media(state, user, item_id).await?;
    let correct_type = if video {
        is_video_type(&media.item.item_type)
    } else {
        is_audio_type(&media.item.item_type)
    };
    // This route serves original bytes. A different suffix would imply a
    // remux or conversion, which this handler does not perform.
    let source_container = media.relative.extension().and_then(|value| value.to_str());
    if !correct_type || !source_container.is_some_and(|value| value.eq_ignore_ascii_case(container))
    {
        return Err(ApiError::NotFound);
    }
    stream_resolved(media, headers, method, false).await
}

pub(crate) fn valid_stream_container(container: &str) -> bool {
    !container.is_empty()
        && container.len() <= 16
        && container.bytes().all(|byte| byte.is_ascii_alphanumeric())
}

async fn stream_file(
    State(state): State<AppState>,
    MediaUser(user): MediaUser,
    Path(item_id): Path<Uuid>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let media = authorized_media(&state, &user, item_id).await?;
    stream_resolved(media, &headers, &method, false).await
}

/// Deliver a raster photo through the same confined reader and admission
/// limits as the original-file endpoint. Primary images do not imply that an
/// arbitrary file can be rendered inline.
pub(crate) async fn stream_photo(
    state: &AppState,
    user: &UserRecord,
    item_id: Uuid,
    method: &Method,
    headers: &HeaderMap,
) -> Result<Response, ApiError> {
    let media = authorized_media(state, user, item_id).await?;
    if media.item.item_type != "Photo"
        || !content_type_for(&media.absolute_path).starts_with("image/")
    {
        return Err(ApiError::NotFound);
    }
    stream_resolved(media, headers, method, false).await
}

async fn download_item(
    State(state): State<AppState>,
    MediaUser(user): MediaUser,
    Path(item_id): Path<Uuid>,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    if !user.enable_content_downloading {
        return Err(ApiError::Forbidden);
    }
    let media = authorized_media(&state, &user, item_id).await?;
    stream_resolved(media, &headers, &method, true).await
}

async fn playback_info(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(item_id): Path<Uuid>,
    headers: HeaderMap,
    Json(mut request): Json<PlaybackInfoRequest>,
) -> Result<Json<PlaybackInfoResponse>, ApiError> {
    // Ignore body credentials. Native decoders cannot forward the web view's
    // cookies or headers, so returned HLS URLs use a read-only child of the
    // authenticated session instead of exposing its general-purpose token.
    request.api_key = None;
    if let Some(item) = db::get_item(&state.db, item_id).await?
        && item.item_type == "LiveTvChannel"
    {
        let mut info =
            livetv_runtime::playback_info(&state, &user, item_id, item.name, &request).await?;
        authorize_playback_urls(&state, &user, &headers, &mut info).await?;
        return Ok(Json(info));
    }
    let media = authorized_media(&state, &user, item_id).await?;
    let mut info = playback::negotiate(&state, media, request).await?;
    authorize_playback_urls(&state, &user, &headers, &mut info).await?;
    Ok(Json(info))
}

async fn authorize_playback_urls(
    state: &AppState,
    user: &UserRecord,
    headers: &HeaderMap,
    info: &mut PlaybackInfoResponse,
) -> Result<(), ApiError> {
    if !info.has_hls_urls() {
        return Ok(());
    }
    let (token, _) = crate::auth::extract_raw_token(headers)?.ok_or(ApiError::Unauthorized)?;
    let (parent_id, parent_user) =
        db::active_auth_identity(&state.db, &crate::auth::token_digest(&token))
            .await?
            .ok_or(ApiError::Unauthorized)?;
    if parent_user.id != user.id {
        return Err(ApiError::Unauthorized);
    }
    let media_token = crate::auth::issue_media_access_token(state, parent_id).await?;
    info.authorize_hls_urls(&media_token.token);
    Ok(())
}

async fn playback_info_get(
    State(state): State<AppState>,
    MediaUser(user): MediaUser,
    Path(item_id): Path<Uuid>,
    Query(request): Query<PlaybackInfoRequest>,
) -> Result<Json<PlaybackInfoResponse>, ApiError> {
    if let Some(item) = db::get_item(&state.db, item_id).await?
        && item.item_type == "LiveTvChannel"
    {
        let info =
            livetv_runtime::playback_info(&state, &user, item_id, item.name, &request).await?;
        return Ok(Json(info));
    }
    let media = authorized_media(&state, &user, item_id).await?;
    let info = playback::negotiate(&state, media, request).await?;
    Ok(Json(info))
}

/// Look up an item, enforce per-user library and parental controls, and open
/// only a regular file that resolves beneath one of the library's configured
/// roots. The canonical path is retained to avoid using the untrusted catalog
/// string again during this request.
async fn authorized_media(
    state: &AppState,
    user: &UserRecord,
    item_id: Uuid,
) -> Result<ResolvedMedia, ApiError> {
    if user.disabled || !user.allow_media_playback {
        return Err(ApiError::Forbidden);
    }
    let item = db::get_item(&state.db, item_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if item.item_type == "LiveTvChannel" {
        livetv_api::require_live_tv_access(user)?;
        return Err(ApiError::NotFound);
    }
    if !db::item_visible_to_user(&state.db, user, &item).await? {
        return Err(ApiError::NotFound);
    }
    let library = db::get_library(&state.db, item.library_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    secure_path::resolve_under_roots(&state.db, item, &library.locations).await
}

/// Read one bounded sibling asset, such as an NFO file or local poster, using
/// the catalog item's registered library root and capability-relative opens.
/// This internal helper accepts a filename rather than a path so callers
/// cannot leave the item's directory or follow a symlink.
#[allow(dead_code)] // The metadata reader will consume this when its routes are registered.
pub(crate) async fn read_adjacent_file(
    state: &AppState,
    item_id: Uuid,
    sibling_name: &str,
    max_bytes: usize,
) -> Result<AdjacentFileRead, ApiError> {
    const MAX_ADJACENT_FILE_BYTES: usize = 4 * 1024 * 1024;

    let mut components = FsPath::new(sibling_name).components();
    let Some(Component::Normal(name)) = components.next() else {
        return Err(ApiError::BadRequest(
            "Adjacent asset name must be one filename".to_owned(),
        ));
    };
    if components.next().is_some()
        || name.to_str() != Some(sibling_name)
        || max_bytes == 0
        || max_bytes > MAX_ADJACENT_FILE_BYTES
    {
        return Err(ApiError::BadRequest(
            "Adjacent asset request is outside the supported bounds".to_owned(),
        ));
    }

    let item = db::get_item(&state.db, item_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let library = db::get_library(&state.db, item.library_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if folder_asset_item_type(&item.item_type) {
        let directory =
            secure_path::resolve_directory_under_roots(&state.db, item, &library.locations).await?;
        return secure_path::read_directory_child_bounded(
            directory,
            PathBuf::from(sibling_name),
            max_bytes,
        )
        .await;
    }
    let media = secure_path::resolve_under_roots(&state.db, item, &library.locations).await?;
    secure_path::read_adjacent_bounded(media, PathBuf::from(sibling_name), max_bytes).await
}

/// Internal metadata jobs use the same registered roots, descriptor-only
/// input, process sandbox and output bounds as playback probing. The returned
/// catalog snapshot must still be checked in the metadata write transaction.
pub(crate) async fn probe_embedded_audio(
    state: &AppState,
    item_id: Uuid,
) -> Result<Option<(ItemRecord, crate::metadata::EmbeddedAudioMetadata)>, ApiError> {
    let item = db::get_item(&state.db, item_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if item.item_type != "Audio" {
        return Ok(None);
    }
    let library = db::get_library(&state.db, item.library_id)
        .await?
        .ok_or(ApiError::NotFound)?;
    let media = secure_path::resolve_under_roots(&state.db, item, &library.locations).await?;
    let Some(info) = probe::probe(state, &media).await? else {
        return Err(ApiError::Unavailable);
    };
    if !info.catalog_identity_matches || !info.streams.iter().any(|stream| stream.kind == "audio") {
        return Err(ApiError::Unavailable);
    }
    Ok(Some((media.item, info.embedded_audio)))
}

fn folder_asset_item_type(item_type: &str) -> bool {
    matches!(
        item_type.to_ascii_lowercase().as_str(),
        "folder" | "series" | "musicalbum" | "artist" | "musicartist"
    )
}

pub(crate) async fn open_authorized_item(
    state: &AppState,
    user: &UserRecord,
    item_id: Uuid,
) -> Result<OpenedMedia, ApiError> {
    let media = authorized_media(state, user, item_id).await?;
    secure_path::open_media(media).await
}

/// Cancel an active HLS operation only when all three identity values match.
/// Playback lifecycle handlers use this after independently authorizing the
/// corresponding catalog item for the caller.
pub(crate) async fn cancel_playback_session(
    owner_id: Uuid,
    item_id: Uuid,
    session_id: Uuid,
) -> bool {
    hls::stop_playback_session(owner_id, item_id, session_id).await
}

/// Keep an authorized player's running or completed HLS job available while
/// the playback lifecycle endpoint reports progress, including while paused.
pub(crate) async fn touch_playback_session(
    owner_id: Uuid,
    item_id: Uuid,
    session_id: Uuid,
) -> bool {
    hls::touch_playback_session(owner_id, item_id, session_id).await
}

async fn stream_resolved(
    media: ResolvedMedia,
    request_headers: &HeaderMap,
    method: &Method,
    attachment: bool,
) -> Result<Response, ApiError> {
    let stream_permit = if method == Method::GET {
        Some(direct_stream_permit()?)
    } else {
        None
    };
    let opened = secure_path::open_media(media.clone()).await?;
    let capability_file = opened.file;
    let total_len = opened.size;
    let modified = opened.modified;
    let etag = make_etag(total_len, modified);
    let content_type = if attachment {
        attachment_content_type_for(&media.absolute_path)
    } else {
        content_type_for(&media.absolute_path)
    };
    let should_download = attachment || !is_safe_inline_type(&content_type);

    let mut selected_range = None;
    if let Some(raw_range) = request_headers.get(RANGE) {
        // If-Range requires a matching strong validator before a partial
        // response can be sent. We currently expose a weak metadata ETag, so
        // conservatively send the full representation when If-Range is used.
        let if_range_present = request_headers.contains_key(IF_RANGE);
        if !if_range_present {
            let raw_range = raw_range
                .to_str()
                .map_err(|_| ApiError::BadRequest("Invalid Range header".to_owned()))?;
            match range::parse_range_header(raw_range, total_len) {
                Ok(range) => selected_range = range,
                Err(_) => return Ok(range_not_satisfiable(total_len)),
            }
        }
    }

    let (status, start, length) = match selected_range {
        Some(range) => (StatusCode::PARTIAL_CONTENT, range.start(), range.len()),
        None => (StatusCode::OK, 0, total_len),
    };

    let mut response_headers = HeaderMap::new();
    response_headers.insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    response_headers.insert(ETAG, etag);
    response_headers.insert(
        CONTENT_TYPE,
        HeaderValue::from_str(&content_type)
            .unwrap_or_else(|_| HeaderValue::from_static("application/octet-stream")),
    );
    response_headers.insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&length.to_string()).expect("decimal content length is valid"),
    );
    if let Some(range) = selected_range {
        let value = format!(
            "bytes {}-{}/{}",
            range.start(),
            range.end_inclusive(),
            total_len
        );
        response_headers.insert(
            CONTENT_RANGE,
            HeaderValue::from_str(&value).expect("generated content range is valid"),
        );
    }
    if should_download {
        response_headers.insert(
            CONTENT_DISPOSITION,
            safe_disposition(&media.item.name, true),
        );
    }
    response_headers.insert(
        HeaderName::from_static("x-content-type-options"),
        HeaderValue::from_static("nosniff"),
    );
    response_headers.insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    response_headers.insert(
        HeaderName::from_static("cache-control"),
        HeaderValue::from_static("private, no-store"),
    );

    let body = if method == Method::HEAD || length == 0 {
        Body::empty()
    } else {
        let (file, permit) = seek_file_bounded(
            capability_file,
            start,
            stream_permit.ok_or(ApiError::Unavailable)?,
        )
        .await?;
        bounded_file_body(file, length, permit)
    };
    let mut response = Response::new(body);
    *response.status_mut() = status;
    *response.headers_mut() = response_headers;
    Ok(response)
}

fn direct_stream_permit() -> Result<OwnedSemaphorePermit, ApiError> {
    static PERMITS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    PERMITS
        .get_or_init(|| Arc::new(Semaphore::new(256)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::RateLimited)
}

async fn seek_file_bounded(
    file: std::fs::File,
    start: u64,
    permit: OwnedSemaphorePermit,
) -> Result<(std::fs::File, Arc<OwnedSemaphorePermit>), ApiError> {
    let permit = Arc::new(permit);
    let worker_permit = permit.clone();
    let file = tokio::task::spawn_blocking(move || {
        let _permit = worker_permit;
        let mut file = file;
        file.seek(SeekFrom::Start(start))
            .map_err(|_| ApiError::NotFound)?;
        Ok::<std::fs::File, ApiError>(file)
    })
    .await
    .map_err(|_| ApiError::Unavailable)??;
    Ok((file, permit))
}

fn bounded_file_body(file: std::fs::File, length: u64, permit: Arc<OwnedSemaphorePermit>) -> Body {
    const CHUNK_SIZE: usize = 64 * 1024;
    let stream = futures_util::stream::try_unfold(
        (file, length, permit),
        |(file, remaining, permit)| async move {
            if remaining == 0 {
                return Ok::<_, io::Error>(None);
            }
            let requested = usize::try_from(remaining.min(CHUNK_SIZE as u64))
                .expect("chunk length is bounded by a usize constant");
            let (file, mut buffer, read) =
                read_bounded_chunk(file, requested, permit.clone()).await?;
            if read == 0 {
                return Ok(None);
            }
            buffer.truncate(read);
            let remaining = remaining.saturating_sub(read as u64);
            Ok(Some((Bytes::from(buffer), (file, remaining, permit))))
        },
    );
    Body::from_stream(stream)
}

async fn read_bounded_chunk<R>(
    reader: R,
    requested: usize,
    permit: Arc<OwnedSemaphorePermit>,
) -> Result<(R, Vec<u8>, usize), io::Error>
where
    R: Read + Send + 'static,
{
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut buffer = vec![0_u8; requested];
        let mut reader = reader;
        let read = reader.read(&mut buffer)?;
        Ok((reader, buffer, read))
    })
    .await
    .map_err(io::Error::other)?
}

fn range_not_satisfiable(total_len: u64) -> Response {
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::RANGE_NOT_SATISFIABLE;
    response
        .headers_mut()
        .insert(ACCEPT_RANGES, HeaderValue::from_static("bytes"));
    response.headers_mut().insert(
        CONTENT_RANGE,
        HeaderValue::from_str(&format!("bytes */{total_len}"))
            .expect("generated content range is valid"),
    );
    response
        .headers_mut()
        .insert(CONTENT_LENGTH, HeaderValue::from_static("0"));
    response.headers_mut().insert(
        HeaderName::from_static("cache-control"),
        HeaderValue::from_static("private, no-store"),
    );
    response.headers_mut().insert(
        HeaderName::from_static("referrer-policy"),
        HeaderValue::from_static("no-referrer"),
    );
    response
}

fn make_etag(len: u64, modified: Option<SystemTime>) -> HeaderValue {
    let modified = modified
        .unwrap_or(SystemTime::UNIX_EPOCH)
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default();
    let tag = format!(
        "W/\"{:x}-{:x}-{:x}\"",
        len,
        modified.as_secs(),
        modified.subsec_nanos()
    );
    HeaderValue::from_str(&tag).expect("generated ETag is valid")
}

fn content_type_for(path: &std::path::Path) -> String {
    let extension = path
        .extension()
        .and_then(|value| value.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    match extension.as_str() {
        "mp4" | "m4v" | "mov" => "video/mp4",
        "mkv" => "video/x-matroska",
        "webm" => "video/webm",
        "avi" => "video/x-msvideo",
        "mpeg" | "mpg" => "video/mpeg",
        "ts" | "m2ts" => "video/mp2t",
        "mp3" => "audio/mpeg",
        "m4a" => "audio/mp4",
        "aac" => "audio/aac",
        "flac" => "audio/flac",
        "ogg" | "oga" => "audio/ogg",
        "opus" => "audio/ogg; codecs=opus",
        "wav" => "audio/wav",
        "aiff" | "aif" => "audio/aiff",
        "jpg" | "jpeg" => "image/jpeg",
        "png" => "image/png",
        "gif" => "image/gif",
        "webp" => "image/webp",
        "avif" => "image/avif",
        "bmp" => "image/bmp",
        "pdf" => "application/pdf",
        // Documents, vector images and unknown formats are never rendered
        // inline in the application origin.
        _ => "application/octet-stream",
    }
    .to_owned()
}

fn attachment_content_type_for(path: &std::path::Path) -> String {
    if path
        .extension()
        .and_then(|value| value.to_str())
        .is_some_and(|extension| extension.eq_ignore_ascii_case("epub"))
    {
        "application/epub+zip".to_owned()
    } else {
        content_type_for(path)
    }
}

fn is_safe_inline_type(content_type: &str) -> bool {
    content_type.starts_with("video/")
        || content_type.starts_with("audio/")
        || content_type.starts_with("image/")
}

fn safe_disposition(name: &str, attachment: bool) -> HeaderValue {
    let disposition = if attachment { "attachment" } else { "inline" };
    let filename: String = name
        .chars()
        .filter(|ch| !ch.is_control() && !matches!(ch, '/' | '\\' | '"'))
        .take(100)
        .collect();
    let filename = filename.trim().trim_start_matches('.');
    let filename = if filename.is_empty() {
        "download.bin"
    } else {
        filename
    };
    HeaderValue::from_str(&format!("{disposition}; filename=\"{filename}\""))
        .unwrap_or_else(|_| HeaderValue::from_static("attachment; filename=\"download.bin\""))
}

fn is_video_type(item_type: &str) -> bool {
    matches!(
        item_type.to_ascii_lowercase().as_str(),
        "movie" | "episode" | "video" | "recording" | "musicvideo"
    )
}

fn is_audio_type(item_type: &str) -> bool {
    matches!(
        item_type.to_ascii_lowercase().as_str(),
        "audio" | "song" | "musictrack" | "audiobook" | "podcast" | "audio book"
    )
}

#[cfg(test)]
mod http_tests {
    use super::secure_path::ResolvedMedia;
    use super::{
        bounded_file_body, content_type_for, is_safe_inline_type, read_bounded_chunk,
        safe_disposition, stream_resolved,
    };
    use crate::library::ItemRecord;
    use axum::{
        body::to_bytes,
        http::{
            HeaderMap, HeaderValue, Method, StatusCode,
            header::{CONTENT_LENGTH, CONTENT_RANGE, RANGE},
        },
    };
    use chrono::Utc;
    use std::{
        fs,
        io::{self, Read},
        path::Path,
        sync::{Arc, mpsc},
        time::Duration,
    };
    use tokio::{
        sync::{Semaphore, oneshot},
        time::timeout,
    };
    use uuid::Uuid;

    #[test]
    fn active_or_unknown_formats_are_downloaded_as_octets() {
        for name in [
            "cover.svg",
            "book.html",
            "book.epub",
            "scan.tiff",
            "x.unknown",
        ] {
            assert_eq!(
                content_type_for(Path::new(name)),
                "application/octet-stream"
            );
        }
        assert!(!is_safe_inline_type(&content_type_for(Path::new(
            "cover.svg"
        ))));
        assert!(is_safe_inline_type(&content_type_for(Path::new(
            "cover.png"
        ))));
        assert!(is_safe_inline_type(&content_type_for(Path::new(
            "cover.webp"
        ))));
    }

    #[test]
    fn disposition_never_contains_path_controls_or_quotes() {
        let value = safe_disposition("../weird\r\n\"name.epub", true);
        let text = value.to_str().unwrap();
        assert!(text.starts_with("attachment; filename=\""));
        assert!(!text.contains(".."));
        assert!(!text.contains('\r'));
        assert!(!text.contains('\n'));
    }

    #[tokio::test]
    async fn direct_file_responses_cover_range_head_empty_and_invalid_ranges() {
        let root_path = std::env::temp_dir().join(format!("puffinbox-range-{}", Uuid::new_v4()));
        fs::create_dir_all(&root_path).unwrap();
        let path = root_path.join("sample.mp4");
        fs::write(&path, b"abcdef").unwrap();
        let media = resolved(&root_path, &path);

        let mut full_range_headers = HeaderMap::new();
        full_range_headers.insert(RANGE, HeaderValue::from_static("bytes=0-"));
        let full_range = stream_resolved(media.clone(), &full_range_headers, &Method::GET, false)
            .await
            .unwrap();
        assert_eq!(full_range.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(full_range.headers()[CONTENT_RANGE], "bytes 0-5/6");
        assert_eq!(full_range.headers()[CONTENT_LENGTH], "6");
        let full_body = to_bytes(full_range.into_body(), 16).await.unwrap();
        assert!(full_body.as_ref() == b"abcdef");

        let mut headers = HeaderMap::new();
        headers.insert(RANGE, HeaderValue::from_static("bytes=2-4"));
        let partial = stream_resolved(media.clone(), &headers, &Method::GET, false)
            .await
            .unwrap();
        assert_eq!(partial.status(), StatusCode::PARTIAL_CONTENT);
        assert_eq!(partial.headers()[CONTENT_RANGE], "bytes 2-4/6");
        assert_eq!(partial.headers()[CONTENT_LENGTH], "3");
        assert_eq!(
            to_bytes(partial.into_body(), 16).await.unwrap().as_ref(),
            b"cde"
        );

        let head = stream_resolved(media.clone(), &HeaderMap::new(), &Method::HEAD, false)
            .await
            .unwrap();
        assert_eq!(head.status(), StatusCode::OK);
        assert_eq!(head.headers()[CONTENT_LENGTH], "6");
        assert!(to_bytes(head.into_body(), 16).await.unwrap().is_empty());

        for range in [
            "bytes=1-2,4-5",
            "bytes=999999999999999999999999-",
            "bytes=0-18446744073709551616",
            "bytes=bad-3",
        ] {
            let mut headers = HeaderMap::new();
            headers.insert(RANGE, HeaderValue::from_bytes(range.as_bytes()).unwrap());
            let rejected = stream_resolved(media.clone(), &headers, &Method::GET, false)
                .await
                .unwrap();
            assert_eq!(
                rejected.status(),
                StatusCode::RANGE_NOT_SATISFIABLE,
                "range {range}"
            );
            assert_eq!(rejected.headers()[CONTENT_RANGE], "bytes */6");
        }

        let empty_path = root_path.join("empty.mp4");
        fs::write(&empty_path, b"").unwrap();
        let empty = resolved(&root_path, &empty_path);
        let empty_get = stream_resolved(empty.clone(), &HeaderMap::new(), &Method::GET, false)
            .await
            .unwrap();
        assert_eq!(empty_get.status(), StatusCode::OK);
        assert_eq!(empty_get.headers()[CONTENT_LENGTH], "0");
        assert!(
            to_bytes(empty_get.into_body(), 16)
                .await
                .unwrap()
                .is_empty()
        );
        let empty_head = stream_resolved(empty, &HeaderMap::new(), &Method::HEAD, false)
            .await
            .unwrap();
        assert_eq!(empty_head.status(), StatusCode::OK);
        assert_eq!(empty_head.headers()[CONTENT_LENGTH], "0");
        let _ = fs::remove_dir_all(root_path);
    }

    #[tokio::test]
    async fn epub_generic_and_attachment_responses_keep_download_safeguards() {
        let root_path = std::env::temp_dir().join(format!("puffinbox-epub-{}", Uuid::new_v4()));
        fs::create_dir_all(&root_path).unwrap();
        let path = root_path.join("book.epub");
        fs::write(&path, b"epub fixture").unwrap();
        let media = resolved(&root_path, &path);

        let generic = stream_resolved(media.clone(), &HeaderMap::new(), &Method::GET, false)
            .await
            .unwrap();
        assert_eq!(
            generic.headers()["content-type"],
            "application/octet-stream"
        );
        assert!(
            generic.headers()["content-disposition"]
                .to_str()
                .unwrap()
                .starts_with("attachment;")
        );
        assert_eq!(generic.headers()["x-content-type-options"], "nosniff");
        assert_eq!(
            to_bytes(generic.into_body(), 32).await.unwrap().as_ref(),
            b"epub fixture"
        );

        let attachment = stream_resolved(media, &HeaderMap::new(), &Method::GET, true)
            .await
            .unwrap();
        assert_eq!(attachment.headers()["content-type"], "application/epub+zip");
        assert!(
            attachment.headers()["content-disposition"]
                .to_str()
                .unwrap()
                .starts_with("attachment;")
        );
        assert_eq!(attachment.headers()["x-content-type-options"], "nosniff");
        assert_eq!(
            to_bytes(attachment.into_body(), 32).await.unwrap().as_ref(),
            b"epub fixture"
        );

        let _ = fs::remove_dir_all(root_path);
    }

    #[tokio::test]
    async fn direct_file_body_holds_stream_admission_until_body_drop() {
        let root_path = std::env::temp_dir().join(format!("puffinbox-stream-{}", Uuid::new_v4()));
        fs::create_dir_all(&root_path).unwrap();
        let path = root_path.join("sample.mp4");
        fs::write(&path, b"stream fixture").unwrap();
        let file = fs::File::open(&path).unwrap();
        let permits = Arc::new(Semaphore::new(1));
        let permit = permits.clone().try_acquire_owned().unwrap();
        let length = fs::metadata(&path).unwrap().len();
        let body = bounded_file_body(file, length, Arc::new(permit));
        assert!(permits.clone().try_acquire_owned().is_err());
        drop(body);
        assert!(permits.try_acquire_owned().is_ok());
        let _ = fs::remove_dir_all(root_path);
    }

    #[tokio::test]
    async fn cancelled_direct_read_keeps_admission_until_blocking_read_finishes() {
        let permits = Arc::new(Semaphore::new(1));
        let permit = Arc::new(permits.clone().try_acquire_owned().unwrap());
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let (finished_tx, finished_rx) = oneshot::channel();
        let worker_permit = permit.clone();
        let task = tokio::spawn(async move {
            let result = read_bounded_chunk(
                PausedReader {
                    started: Some(started_tx),
                    release: release_rx,
                    finished: Some(finished_tx),
                },
                1,
                worker_permit,
            )
            .await;
            assert!(result.is_ok());
        });
        timeout(Duration::from_secs(2), started_rx)
            .await
            .expect("blocking read did not start")
            .expect("read start signal was dropped");

        drop(permit);
        task.abort();
        assert!(permits.clone().try_acquire_owned().is_err());

        release_tx.send(()).unwrap();
        timeout(Duration::from_secs(2), finished_rx)
            .await
            .expect("blocking reader did not finish")
            .expect("reader completion signal was dropped");
        let available = timeout(Duration::from_secs(2), async {
            loop {
                if let Ok(permit) = permits.clone().try_acquire_owned() {
                    break permit;
                }
                tokio::task::yield_now().await;
            }
        })
        .await
        .expect("stream admission remained held after the read completed");
        drop(available);
    }

    struct PausedReader {
        started: Option<oneshot::Sender<()>>,
        release: mpsc::Receiver<()>,
        finished: Option<oneshot::Sender<()>>,
    }

    impl Read for PausedReader {
        fn read(&mut self, buffer: &mut [u8]) -> io::Result<usize> {
            if let Some(started) = self.started.take() {
                let _ = started.send(());
            }
            self.release
                .recv()
                .map_err(|_| io::Error::other("test reader release was dropped"))?;
            if let Some(finished) = self.finished.take() {
                let _ = finished.send(());
            }
            if let Some(first) = buffer.first_mut() {
                *first = b'x';
                Ok(1)
            } else {
                Ok(0)
            }
        }
    }

    fn resolved(root_path: &Path, path: &Path) -> ResolvedMedia {
        let absolute_path = fs::canonicalize(path).unwrap();
        let canonical_root = fs::canonicalize(root_path).unwrap();
        let relative = absolute_path
            .strip_prefix(&canonical_root)
            .unwrap()
            .to_path_buf();
        let metadata = fs::metadata(path).unwrap();
        let item = ItemRecord {
            id: Uuid::new_v4(),
            library_id: Uuid::new_v4(),
            parent_id: None,
            name: path.file_name().unwrap().to_string_lossy().into_owned(),
            sort_name: "sample".to_owned(),
            item_type: "Movie".to_owned(),
            path: absolute_path.clone(),
            container: Some("mp4".to_owned()),
            size_bytes: Some(metadata.len() as i64),
            runtime_ticks: None,
            date_added: Utc::now(),
            date_modified: None,
            rating: None,
            overview: None,
            metadata_json: serde_json::json!({}),
        };
        let root = Arc::new(
            cap_std::fs::Dir::open_ambient_dir(&canonical_root, cap_std::ambient_authority())
                .unwrap(),
        );
        ResolvedMedia {
            item,
            parent_directory: None,
            root,
            relative,
            absolute_path,
            catalog_identity_matches: false,
        }
    }
}
