//! Bounded, user-authorized HLS jobs backed by an operator-supplied FFmpeg.

use std::{
    collections::HashMap,
    io::{self, Read as StdRead},
    os::unix::fs::OpenOptionsExt as StdOpenOptionsExt,
    path::{Path, PathBuf},
    process::Stdio,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

use axum::{
    body::Body,
    extract::{Path as AxumPath, Query, State},
    http::{
        HeaderValue, StatusCode,
        header::{CACHE_CONTROL, CONTENT_LENGTH, CONTENT_TYPE},
    },
    response::Response,
};
use cap_std::fs::{Dir as CapabilityDir, OpenOptions as CapabilityOpenOptions, OpenOptionsExt};
use serde::Deserialize;
use tokio::{
    fs::{self, File},
    io::AsyncReadExt,
    process::{Child, Command},
    sync::{Mutex, Notify, Semaphore, oneshot},
    time::{interval, sleep, timeout},
};
use uuid::Uuid;

use crate::{
    ApiError,
    auth::{CurrentUser, MediaUser},
    state::AppState,
};

use super::{
    authorized_media, is_audio_type, is_video_type,
    probe::{self, ProbeInfo},
    process_limits::{MediaChildSandbox, apply_child_limits, stop_child},
    secure_path::ResolvedMedia,
    subtitles,
};

const MAX_RUNNING_HLS: usize = 2;
const MAX_SESSION_RECORDS: usize = 32;
const MAX_HLS_OUTPUT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const MAX_HLS_SEGMENTS: usize = 20_000;
const MAX_SEGMENT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_TIMESTAMP_SCAN_BYTES: u64 = 512 * 1024;
const MAX_PLAYLIST_BYTES: usize = 256 * 1024;
const MAX_SUBTITLE_BYTES: u64 = 16 * 1024 * 1024;
const MAX_PROCESS_LIFETIME: Duration = Duration::from_secs(4 * 60 * 60);
const IDLE_KILL_AFTER: Duration = Duration::from_secs(60);
const COMPLETED_RETENTION: Duration = Duration::from_secs(120);
const PROBE_COMMAND_TIMEOUT: Duration = Duration::from_secs(5);

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
pub(super) struct HlsOptions {
    pub(super) play_session_id: Option<Uuid>,
    #[serde(default)]
    pub(super) stream_copy: bool,
    #[serde(default)]
    pub(super) full_timeline: bool,
    #[serde(alias = "StartTimeTicks")]
    pub(super) start_time_ticks: Option<i64>,
    pub(super) audio_stream_index: Option<i32>,
    pub(super) subtitle_stream_index: Option<i32>,
    pub(super) max_streaming_bitrate: Option<u64>,
    pub(super) max_audio_channels: Option<u32>,
    pub(super) audio_bit_rate: Option<u64>,
    pub(super) audio_sample_rate: Option<u32>,
    #[serde(default)]
    pub(super) audio_fmp4: bool,
    // Universal audio clients seek using the original item's timeline. Keep
    // the whole source and advertise a start hint instead of clipping twice.
    #[serde(skip)]
    pub(super) audio_full_timeline: bool,
    // This is only present when an external Jellyfin client explicitly uses
    // the current query-token media auth form. The bundled UI uses cookies.
    #[serde(rename = "ApiKey")]
    pub(super) api_key: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum MediaKind {
    Video,
    Audio,
}

impl MediaKind {
    pub(super) fn route_prefix(self) -> &'static str {
        match self {
            Self::Video => "Videos",
            Self::Audio => "Audio",
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum SessionState {
    Running,
    Complete,
    Stopping,
}

struct HlsSession {
    generation: Uuid,
    item_id: Uuid,
    owner_id: Uuid,
    kind: MediaKind,
    directory: PathBuf,
    last_accessed: Arc<AtomicU64>,
    cancel: Option<oneshot::Sender<()>>,
    state: SessionState,
    duration_seconds: u32,
    has_subtitles: bool,
    has_audio: bool,
    bandwidth: u64,
    options: HlsOptions,
    vod: Option<Arc<super::vod_hls::VodSession>>,
}

struct HlsJob {
    session_id: Uuid,
    generation: Uuid,
    directory: PathBuf,
    child: Child,
    cancel_rx: oneshot::Receiver<()>,
    last_accessed: Arc<AtomicU64>,
    permit: tokio::sync::OwnedSemaphorePermit,
}

struct HlsManager {
    sessions: Mutex<HashMap<Uuid, HlsSession>>,
    permits: Arc<Semaphore>,
    shutdown: Notify,
    shutting_down: AtomicBool,
}

struct PrivateDirectoryGuard(Option<(PathBuf, tokio::sync::OwnedSemaphorePermit)>);

impl PrivateDirectoryGuard {
    fn new(path: PathBuf, cleanup_permit: tokio::sync::OwnedSemaphorePermit) -> Self {
        Self(Some((path, cleanup_permit)))
    }
    fn keep(&mut self) {
        self.0 = None;
    }

    fn cleanup_holding(mut self, admission: tokio::sync::OwnedSemaphorePermit) {
        let Some((path, cleanup_permit)) = self.0.take() else {
            drop(admission);
            return;
        };
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn_blocking(move || {
                let _permits = (cleanup_permit, admission);
                let _ = std::fs::remove_dir_all(path);
            });
        } else {
            let _permits = (cleanup_permit, admission);
            let _ = std::fs::remove_dir_all(path);
        }
    }
}

impl Drop for PrivateDirectoryGuard {
    fn drop(&mut self) {
        if let Some((path, cleanup_permit)) = self.0.take() {
            if let Ok(runtime) = tokio::runtime::Handle::try_current() {
                runtime.spawn_blocking(move || {
                    let _permit = cleanup_permit;
                    let _ = std::fs::remove_dir_all(path);
                });
            } else {
                let _permit = cleanup_permit;
                let _ = std::fs::remove_dir_all(path);
            }
        }
    }
}

impl HlsManager {
    fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            permits: Arc::new(Semaphore::new(MAX_RUNNING_HLS)),
            shutdown: Notify::new(),
            shutting_down: AtomicBool::new(false),
        }
    }
}

fn manager() -> &'static HlsManager {
    static MANAGER: OnceLock<HlsManager> = OnceLock::new();
    MANAGER.get_or_init(HlsManager::new)
}

pub(super) async fn shutdown() -> bool {
    let manager = manager();
    manager.shutting_down.store(true, Ordering::Release);
    manager.shutdown.notify_waiters();
    {
        let mut sessions = manager.sessions.lock().await;
        for session in sessions.values_mut() {
            if matches!(
                session.state,
                SessionState::Running | SessionState::Complete
            ) {
                session.state = SessionState::Stopping;
                if let Some(cancel) = session.cancel.take() {
                    let _ = cancel.send(());
                }
            }
        }
    }
    timeout(Duration::from_secs(5), async {
        loop {
            if manager.sessions.lock().await.is_empty() {
                return true;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or(false)
}

fn encoder_cache() -> &'static Mutex<HashMap<PathBuf, bool>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

fn copy_muxer_cache() -> &'static Mutex<HashMap<PathBuf, bool>> {
    static CACHE: OnceLock<Mutex<HashMap<PathBuf, bool>>> = OnceLock::new();
    CACHE.get_or_init(|| Mutex::new(HashMap::new()))
}

pub(super) async fn ffmpeg_hls_available(state: &AppState) -> bool {
    let Some(program) = state.config.ffmpeg_path.clone() else {
        return false;
    };
    if let Some(value) = encoder_cache().lock().await.get(&program).copied() {
        return value;
    }
    static VERIFY_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let lock = VERIFY_LOCK.get_or_init(|| Mutex::new(()));
    let _guard = lock.lock().await;
    if let Some(value) = encoder_cache().lock().await.get(&program).copied() {
        return value;
    }
    let supported = verify_ffmpeg(&program).await;
    encoder_cache().lock().await.insert(program, supported);
    supported
}

pub(super) async fn ffmpeg_hls_copy_available(state: &AppState) -> bool {
    let Some(program) = state.config.ffmpeg_path.clone() else {
        return false;
    };
    if let Some(value) = copy_muxer_cache().lock().await.get(&program).copied() {
        return value;
    }
    static VERIFY_LOCK: OnceLock<Mutex<()>> = OnceLock::new();
    let lock = VERIFY_LOCK.get_or_init(|| Mutex::new(()));
    let _guard = lock.lock().await;
    if let Some(value) = copy_muxer_cache().lock().await.get(&program).copied() {
        return value;
    }
    let supported = verify_hls_copy_muxers(&program).await;
    copy_muxer_cache().lock().await.insert(program, supported);
    supported
}

async fn verify_hls_copy_muxers(program: &Path) -> bool {
    let Some(muxers) =
        bounded_output(program, &["-hide_banner", "-muxers"], PROBE_COMMAND_TIMEOUT).await
    else {
        return false;
    };
    let mut found_hls = false;
    let mut found_mpegts = false;
    for line in String::from_utf8_lossy(&muxers).lines() {
        let parts = line.split_whitespace().collect::<Vec<_>>();
        if !parts.first().is_some_and(|flags| flags.contains('E')) {
            continue;
        }
        found_hls |= parts.get(1).is_some_and(|name| *name == "hls");
        found_mpegts |= parts.get(1).is_some_and(|name| *name == "mpegts");
    }
    found_hls && found_mpegts
}

async fn verify_ffmpeg(program: &Path) -> bool {
    let Some(encoders) = bounded_output(
        program,
        &["-hide_banner", "-encoders"],
        PROBE_COMMAND_TIMEOUT,
    )
    .await
    else {
        return false;
    };
    let encoder_text = String::from_utf8_lossy(&encoders);
    let has_x264 = encoder_text.lines().any(|line| {
        let parts = line.split_whitespace().collect::<Vec<_>>();
        parts.first().is_some_and(|flags| flags.starts_with('V'))
            && parts.get(1).is_some_and(|name| *name == "libx264")
    });
    let has_aac = encoder_text.lines().any(|line| {
        let parts = line.split_whitespace().collect::<Vec<_>>();
        parts.first().is_some_and(|flags| flags.starts_with('A'))
            && parts.get(1).is_some_and(|name| *name == "aac")
    });
    if !has_x264 || !has_aac {
        return false;
    }
    let Some(muxers) =
        bounded_output(program, &["-hide_banner", "-muxers"], PROBE_COMMAND_TIMEOUT).await
    else {
        return false;
    };
    let mut found_hls = false;
    let mut found_mpegts = false;
    for line in String::from_utf8_lossy(&muxers).lines() {
        let parts = line.split_whitespace().collect::<Vec<_>>();
        if parts.first().is_some_and(|flags| flags.contains('E')) {
            found_hls |= parts.get(1).is_some_and(|name| *name == "hls");
            found_mpegts |= parts.get(1).is_some_and(|name| *name == "mpegts");
        }
    }
    found_hls && found_mpegts
}

async fn bounded_output(program: &Path, args: &[&str], limit: Duration) -> Option<Vec<u8>> {
    timeout(limit, async {
        let mut command = Command::new(program);
        apply_child_limits(&mut command, 5, 512 * 1024 * 1024, 0, None);
        command
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true)
            .args(args);
        let mut child = command.spawn().ok()?;
        let stdout = child.stdout.take()?;
        let mut output = Vec::with_capacity(64 * 1024);
        let mut limited = stdout.take(1024 * 1024 + 1);
        if limited.read_to_end(&mut output).await.is_err() || output.len() > 1024 * 1024 {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return None;
        }
        let status = child.wait().await.ok()?;
        status.success().then_some(output)
    })
    .await
    .ok()?
}

pub(super) async fn video_master(
    State(state): State<AppState>,
    MediaUser(user): MediaUser,
    AxumPath(item_id): AxumPath<Uuid>,
    Query(options): Query<HlsOptions>,
) -> Result<Response, ApiError> {
    start_master(state, user, item_id, options, MediaKind::Video).await
}

pub(super) async fn video_master_head(
    State(state): State<AppState>,
    MediaUser(user): MediaUser,
    AxumPath(item_id): AxumPath<Uuid>,
    Query(options): Query<HlsOptions>,
) -> Result<Response, ApiError> {
    head_master(state, user, item_id, options, MediaKind::Video).await
}

pub(super) async fn audio_master(
    State(state): State<AppState>,
    MediaUser(user): MediaUser,
    AxumPath(item_id): AxumPath<Uuid>,
    Query(options): Query<HlsOptions>,
) -> Result<Response, ApiError> {
    start_master(state, user, item_id, options, MediaKind::Audio).await
}

pub(super) async fn audio_master_head(
    State(state): State<AppState>,
    MediaUser(user): MediaUser,
    AxumPath(item_id): AxumPath<Uuid>,
    Query(options): Query<HlsOptions>,
) -> Result<Response, ApiError> {
    head_master(state, user, item_id, options, MediaKind::Audio).await
}

pub(super) async fn head_master(
    state: AppState,
    user: crate::auth::UserRecord,
    item_id: Uuid,
    options: HlsOptions,
    kind: MediaKind,
) -> Result<Response, ApiError> {
    let media = authorized_media(&state, &user, item_id).await?;
    match kind {
        MediaKind::Video if !is_video_type(&media.item.item_type) => {
            return Err(ApiError::NotFound);
        }
        MediaKind::Audio if !is_audio_type(&media.item.item_type) => {
            return Err(ApiError::NotFound);
        }
        _ => {}
    }
    let available = if options.stream_copy {
        ffmpeg_hls_copy_available(&state).await
    } else {
        ffmpeg_hls_available(&state).await
    };
    if !available {
        return Err(ApiError::Unavailable);
    }
    let mut response = Response::new(Body::empty());
    *response.status_mut() = StatusCode::OK;
    response.headers_mut().insert(
        CONTENT_TYPE,
        HeaderValue::from_static("application/vnd.apple.mpegurl"),
    );
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    response
        .headers_mut()
        .insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    Ok(response)
}

pub(super) async fn start_master(
    state: AppState,
    user: crate::auth::UserRecord,
    item_id: Uuid,
    options: HlsOptions,
    kind: MediaKind,
) -> Result<Response, ApiError> {
    let media = authorized_media(&state, &user, item_id).await?;
    match kind {
        MediaKind::Video if !is_video_type(&media.item.item_type) => {
            return Err(ApiError::NotFound);
        }
        MediaKind::Audio if !is_audio_type(&media.item.item_type) => {
            return Err(ApiError::NotFound);
        }
        _ => {}
    }
    let version = if options.audio_fmp4 { 7 } else { 3 };
    let session_id = start_hls(&state, media, options, kind, user.id).await?;
    let session_path = match kind {
        MediaKind::Video => format!("/Videos/{item_id}/hls/{session_id}"),
        MediaKind::Audio => format!("/Audio/{item_id}/hls/{session_id}"),
    };
    let (has_subtitles, has_audio, bandwidth, api_key, stream_copy, start_ticks) =
        session_master_info(session_id, user.id, item_id, kind).await?;
    let codec_list = match (kind, has_audio) {
        (MediaKind::Video, true) => "avc1.42E01F,mp4a.40.2",
        (MediaKind::Video, false) => "avc1.42E01F",
        (MediaKind::Audio, _) => "mp4a.40.2",
    };
    let playlist_path =
        append_api_key(&format!("{session_path}/playlist.m3u8"), api_key.as_deref());
    let subtitle_path =
        append_api_key(&format!("{session_path}/subtitle.m3u8"), api_key.as_deref());
    let mut stream_info = format!("#EXT-X-STREAM-INF:BANDWIDTH={bandwidth}");
    if !stream_copy {
        stream_info.push_str(&format!(",CODECS=\"{codec_list}\""));
    }
    let subtitle_media = if has_subtitles {
        format!(
            "#EXT-X-MEDIA:TYPE=SUBTITLES,GROUP-ID=\"puffin-subtitles\",NAME=\"Selected\",DEFAULT=YES,AUTOSELECT=YES,URI=\"{subtitle_path}\"\n"
        )
    } else {
        String::new()
    };
    if has_subtitles {
        stream_info.push_str(",SUBTITLES=\"puffin-subtitles\"");
    }
    let playlist = format!(
        "#EXTM3U\n#EXT-X-VERSION:{version}\n{}{subtitle_media}{stream_info}\n{playlist_path}\n",
        playlist_start_hint(start_ticks)
    );
    Ok(text_response(
        playlist,
        "application/vnd.apple.mpegurl",
        StatusCode::OK,
    ))
}

async fn start_hls(
    state: &AppState,
    media: ResolvedMedia,
    options: HlsOptions,
    kind: MediaKind,
    owner_id: Uuid,
) -> Result<Uuid, ApiError> {
    if manager().shutting_down.load(Ordering::Acquire) {
        return Err(ApiError::Unavailable);
    }
    let session_id = options.play_session_id.unwrap_or_else(Uuid::new_v4);
    let mut options = options;
    options.play_session_id = Some(session_id);
    if (options.audio_fmp4
        || options.audio_full_timeline
        || options.audio_sample_rate.is_some()
        || options.audio_bit_rate.is_some())
        && (kind != MediaKind::Audio || options.full_timeline || options.stream_copy)
    {
        return Err(ApiError::BadRequest(
            "These audio output options require an audio transcode without fullTimeline".to_owned(),
        ));
    }
    if options.audio_sample_rate.is_some_and(|rate| {
        !matches!(
            rate,
            8000 | 11025 | 12000 | 16000 | 22050 | 24000 | 32000 | 44100 | 48000
        )
    }) || options
        .audio_bit_rate
        .is_some_and(|rate| !(16_000..=320_000).contains(&rate))
    {
        return Err(ApiError::BadRequest(
            "Unsupported AAC output rate".to_owned(),
        ));
    }
    {
        let mut sessions = manager().sessions.lock().await;
        if let Some(existing) = sessions.get_mut(&session_id) {
            if session_matches(existing, owner_id, media.item.id, kind)
                && existing.options == options
            {
                match existing.state {
                    SessionState::Stopping => {
                        return Err(ApiError::Conflict(
                            "PlaySessionId is still stopping".to_owned(),
                        ));
                    }
                    SessionState::Running | SessionState::Complete => {}
                }
                existing
                    .last_accessed
                    .store(now_millis(), Ordering::Relaxed);
                return Ok(session_id);
            }
            return Err(ApiError::Conflict(
                "PlaySessionId is already active".to_owned(),
            ));
        }
    }
    let permit = manager()
        .permits
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::RateLimited)?;
    prune_sessions().await;
    {
        let sessions = manager().sessions.lock().await;
        if sessions.contains_key(&session_id) {
            return Err(ApiError::Conflict(
                "PlaySessionId is already active".to_owned(),
            ));
        }
        if sessions.len() >= MAX_SESSION_RECORDS {
            return Err(ApiError::RateLimited);
        }
    }
    let ffmpeg_available = if options.stream_copy {
        ffmpeg_hls_copy_available(state).await
    } else {
        ffmpeg_hls_available(state).await
    };
    if !ffmpeg_available {
        return Err(ApiError::Unavailable);
    }
    let ffmpeg = state
        .config
        .ffmpeg_path
        .clone()
        .ok_or(ApiError::Unavailable)?;
    let (demuxer, _) = probe::allowed_demuxer(&media.absolute_path).ok_or_else(|| {
        ApiError::BadRequest("This container is not available for transcoding".to_owned())
    })?;
    let source_info = probe::probe(state, &media)
        .await?
        .ok_or(ApiError::Unavailable)?;
    let after_index = source_info.streams.iter().map(|stream| stream.index).max();
    let sidecars = subtitles::list_sidecars(&media, after_index).await?;
    validate_options(&options, &source_info, &sidecars)?;
    if options
        .max_streaming_bitrate
        .is_some_and(|value| value == 0)
        || options
            .max_audio_channels
            .is_some_and(|value| value == 0 || value > 32)
    {
        return Err(ApiError::BadRequest(
            "Playback bitrate and channel limits must be positive and bounded".to_owned(),
        ));
    }
    if matches!(kind, MediaKind::Audio)
        && options
            .subtitle_stream_index
            .is_some_and(|index| index >= 0)
    {
        return Err(ApiError::BadRequest(
            "Subtitles are supported for video playback only".to_owned(),
        ));
    }

    let generation = Uuid::new_v4();
    let directory = session_work_directory(&state.config.data_dir, session_id, generation);
    let (mut directory_guard, mut permit) = create_private_dir(directory.clone(), permit).await?;

    let source_duration = source_info.duration_seconds.or_else(|| {
        source_info
            .catalog_identity_matches
            .then_some(media.item.runtime_ticks)
            .flatten()
            .map(|ticks| ticks as f64 / 10_000_000.0)
    });
    if source_duration.is_some_and(|duration| duration > 14_400.0) {
        return Err(ApiError::Unavailable);
    }
    let full_duration_seconds = source_duration
        .map(|value| value.ceil().max(1.0) as u32)
        .unwrap_or(14_400);
    let start_seconds = options
        .start_time_ticks
        .unwrap_or_default()
        .saturating_div(10_000_000)
        .clamp(0, 14_399) as u32;
    let duration_seconds = if options.full_timeline || options.audio_full_timeline {
        full_duration_seconds
    } else {
        full_duration_seconds.saturating_sub(start_seconds).max(1)
    };
    if options.full_timeline && (options.stream_copy || source_duration.is_none()) {
        return Err(ApiError::BadRequest(
            "Full-timeline HLS requires transcoding and a known media duration".to_owned(),
        ));
    }
    let selected_video = if matches!(kind, MediaKind::Video) {
        probe::default_stream(&source_info.streams, "video")
    } else {
        None
    };
    if matches!(kind, MediaKind::Video) && selected_video.is_none() {
        return Err(ApiError::BadRequest(
            "The catalogued video has no supported video stream".to_owned(),
        ));
    }
    let selected_audio = match options.audio_stream_index {
        Some(-1) => None,
        Some(index) if index >= 0 => source_info
            .streams
            .iter()
            .find(|stream| stream.index == index as u32 && stream.kind == "audio"),
        _ => probe::default_stream(&source_info.streams, "audio"),
    };
    if matches!(kind, MediaKind::Audio) && selected_audio.is_none() {
        return Err(ApiError::BadRequest(
            "The catalogued audio item has no supported audio stream".to_owned(),
        ));
    }
    if options.audio_stream_index.is_some_and(|index| index >= 0) && selected_audio.is_none() {
        return Err(ApiError::BadRequest(
            "AudioStreamIndex does not identify an audio stream".to_owned(),
        ));
    }
    let copy_bandwidth = if options.stream_copy {
        if options.start_time_ticks.is_some_and(|ticks| ticks > 0) {
            return Err(ApiError::BadRequest(
                "HLS stream copy cannot seek between keyframes; use transcoding for resume playback"
                    .to_owned(),
            ));
        }
        if selected_video.is_some_and(|stream| stream.codec.as_deref() != Some("h264"))
            || selected_audio.is_some_and(|stream| stream.codec.as_deref() != Some("aac"))
            || (selected_video.is_none() && selected_audio.is_none())
        {
            return Err(ApiError::BadRequest(
                "HLS stream copy supports H.264 video and AAC audio only".to_owned(),
            ));
        }
        let bandwidth = source_stream_bandwidth(&source_info, selected_video, selected_audio)
            .ok_or_else(|| {
                ApiError::BadRequest("HLS stream copy requires known source bitrates".to_owned())
            })?;
        let declared = bandwidth.saturating_mul(110) / 100;
        if options
            .max_streaming_bitrate
            .is_some_and(|maximum| declared > maximum)
        {
            return Err(ApiError::BadRequest(
                "The source bitrate exceeds the requested streaming limit".to_owned(),
            ));
        }
        if selected_audio.is_some_and(|stream| {
            stream
                .channels
                .is_none_or(|channels| channels > options.max_audio_channels.unwrap_or(2))
        }) {
            return Err(ApiError::BadRequest(
                "The selected audio stream exceeds the requested channel limit".to_owned(),
            ));
        }
        Some(declared.max(1))
    } else {
        None
    };
    let max_bitrate = options
        .max_streaming_bitrate
        .unwrap_or(2_000_000)
        .min(8_000_000);
    let minimum_bitrate = if kind == MediaKind::Audio {
        32_000
    } else {
        320_000
    };
    if !options.stream_copy && max_bitrate < minimum_bitrate {
        let _ = fs::remove_dir_all(&directory).await;
        return Err(ApiError::BadRequest(
            "The requested bitrate is too low for the supported HLS profile".to_owned(),
        ));
    }
    let output_channels = options.max_audio_channels.unwrap_or(2).clamp(1, 2);
    let bounded_bitrate = max_bitrate.saturating_mul(95) / 100;
    let audio_bitrate = if selected_audio.is_some() {
        if options
            .audio_bit_rate
            .is_some_and(|rate| rate > bounded_bitrate)
        {
            return Err(ApiError::BadRequest(
                "The requested audio bitrate exceeds the streaming limit".to_owned(),
            ));
        }
        bounded_bitrate.min(options.audio_bit_rate.unwrap_or(128_000))
    } else {
        0
    };
    let video_bitrate = if matches!(kind, MediaKind::Video) {
        bounded_bitrate.saturating_sub(audio_bitrate)
    } else {
        0
    };
    let selected_subtitle = options
        .subtitle_stream_index
        .filter(|index| *index >= 0)
        .map(|index| index as u32);
    if let Some(index) = selected_subtitle {
        if let Some(stream) = source_info
            .streams
            .iter()
            .find(|stream| stream.index == index && stream.kind == "subtitle")
        {
            let codec = stream.codec.as_deref().ok_or_else(|| {
                ApiError::BadRequest("Subtitle stream codec is unknown".to_owned())
            })?;
            if !subtitle_codec_supported(codec) {
                return Err(ApiError::BadRequest(
                    "This subtitle codec cannot be converted to WebVTT".to_owned(),
                ));
            }
            permit = extract_subtitle(
                &media,
                &directory,
                &ffmpeg,
                demuxer,
                index,
                if options.full_timeline {
                    None
                } else {
                    options.start_time_ticks
                },
                permit,
            )
            .await?;
        } else {
            let sidecar = sidecars
                .iter()
                .find(|track| track.index == index)
                .ok_or_else(|| {
                    ApiError::BadRequest(
                        "SubtitleStreamIndex does not identify a subtitle stream".to_owned(),
                    )
                })?;
            let vtt = subtitles::read_sidecar_vtt(
                &media,
                sidecar,
                if options.full_timeline {
                    None
                } else {
                    options.start_time_ticks
                },
            )
            .await?;
            fs::write(directory.join("subtitle.vtt"), vtt)
                .await
                .map_err(|_| ApiError::Unavailable)?;
        }
    }

    if let (Some(duration), Some(start)) = (source_info.duration_seconds, options.start_time_ticks)
        && start as f64 / 10_000_000.0 >= duration
    {
        return Err(ApiError::BadRequest(
            "startTimeTicks is beyond the media duration".to_owned(),
        ));
    }
    if options.full_timeline {
        let last_accessed = Arc::new(AtomicU64::new(now_millis()));
        let (vod, worker) = super::vod_hls::prepare_session(
            super::vod_hls::VodPlan {
                media: media.clone(),
                ffmpeg,
                demuxer: demuxer.to_owned(),
                directory: directory.clone(),
                duration_millis: (source_duration.ok_or(ApiError::Unavailable)? * 1000.0).ceil()
                    as u64,
                resume_ticks: options.start_time_ticks,
                video_index: selected_video.map(|stream| stream.index),
                audio_index: selected_audio.map(|stream| stream.index),
                video_bitrate,
                audio_bitrate,
                output_channels,
            },
            last_accessed.clone(),
            manager().permits.clone(),
        )?;
        let mut sessions = manager().sessions.lock().await;
        if manager().shutting_down.load(Ordering::Acquire) {
            return Err(ApiError::Unavailable);
        }
        if session_id_reserved(&sessions, session_id) {
            return Err(ApiError::Conflict(
                "PlaySessionId is already active".to_owned(),
            ));
        }
        if sessions.len() >= MAX_SESSION_RECORDS {
            return Err(ApiError::RateLimited);
        }
        let (cancel_tx, cancel_rx) = oneshot::channel();
        sessions.insert(
            session_id,
            HlsSession {
                generation,
                item_id: media.item.id,
                owner_id,
                kind,
                directory,
                last_accessed,
                cancel: Some(cancel_tx),
                state: SessionState::Running,
                duration_seconds,
                has_subtitles: selected_subtitle.is_some(),
                has_audio: selected_audio.is_some(),
                bandwidth: max_bitrate,
                options,
                vod: Some(vod),
            },
        );
        tokio::spawn(super::vod_hls::run_worker(
            worker, session_id, generation, cancel_rx, permit,
        ));
        directory_guard.keep();
        return Ok(session_id);
    }
    let source_file = super::secure_path::open_media(media.clone()).await?.file;
    let sandbox = MediaChildSandbox::prepare_bounded(
        ffmpeg.clone(),
        source_file,
        Some(directory.clone()),
        super::secure_path::filesystem_permit()?,
    )
    .await?;
    let input_fd = sandbox.input_fd().to_string();
    let manifest_path = directory.join("stream.m3u8");
    let segment_pattern = directory.join(if options.audio_fmp4 {
        "segment%06d.m4s"
    } else {
        "segment%06d.ts"
    });
    let mut command = Command::new(sandbox.executable());
    apply_child_limits(
        &mut command,
        MAX_PROCESS_LIFETIME.as_secs(),
        2 * 1024 * 1024 * 1024,
        64 * 1024 * 1024,
        Some(sandbox),
    );
    command
        .env_clear()
        .current_dir(&directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .arg("-nostdin")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-threads")
        .arg("2")
        .arg("-filter_threads")
        .arg("1")
        .arg("-protocol_whitelist")
        .arg("fd")
        .arg("-fd")
        .arg(&input_fd)
        .arg("-probesize")
        .arg("8000000")
        .arg("-analyzeduration")
        .arg("8000000");
    if let Some(ticks) = options
        .start_time_ticks
        .filter(|ticks| *ticks > 0 && !options.audio_full_timeline)
    {
        let seconds = ticks as f64 / 10_000_000.0;
        command.arg("-ss").arg(format!("{seconds:.3}"));
    }
    command.arg("-f").arg(demuxer).arg("-i").arg("fd:");
    if let Some(stream) = selected_video {
        command.arg("-map").arg(format!("0:{}", stream.index));
        if options.stream_copy {
            command.arg("-c:v").arg("copy");
        } else {
            command
            .arg("-c:v").arg("libx264")
            .arg("-preset").arg("veryfast")
            .arg("-tune").arg("zerolatency")
            .arg("-profile:v").arg("baseline")
            .arg("-level:v").arg("3.1")
            .arg("-pix_fmt").arg("yuv420p")
            .arg("-vf").arg("scale='min(1280,iw)':'min(720,ih)':force_original_aspect_ratio=decrease:force_divisible_by=2")
            .arg("-r").arg("30")
            .arg("-fps_mode").arg("cfr")
            .arg("-b:v").arg(format!("{video_bitrate}"))
            .arg("-maxrate").arg(format!("{video_bitrate}"))
            .arg("-bufsize").arg(format!("{}", video_bitrate.saturating_mul(2)))
                .arg("-threads:v").arg("2");
        }
    } else {
        command.arg("-vn");
    }
    if let Some(stream) = selected_audio {
        command.arg("-map").arg(format!("0:{}", stream.index));
        if options.stream_copy {
            command.arg("-c:a").arg("copy");
        } else {
            command
                .arg("-c:a")
                .arg("aac")
                .arg("-b:a")
                .arg(format!("{audio_bitrate}"))
                .arg("-ac")
                .arg(format!("{output_channels}"));
            if let Some(rate) = options.audio_sample_rate {
                command.arg("-ar").arg(rate.to_string());
            }
        }
    } else {
        command.arg("-an");
    }
    command.arg("-sn").arg("-dn");
    if options.audio_fmp4 {
        command.args([
            "-hls_segment_type",
            "fmp4",
            "-hls_fmp4_init_filename",
            "init.mp4",
        ]);
    }
    if source_duration.is_some() {
        command.arg("-t").arg(duration_seconds.to_string());
    }
    command
        .arg("-f")
        .arg("hls")
        .arg("-hls_time")
        .arg("4")
        .arg("-hls_list_size")
        .arg("0")
        .arg("-hls_playlist_type")
        .arg("event")
        .arg("-hls_flags")
        .arg(if options.stream_copy {
            "temp_file"
        } else {
            "independent_segments+temp_file"
        })
        .arg("-hls_segment_filename")
        .arg(segment_pattern)
        .arg("-y")
        .arg(&manifest_path);

    // Acquire the map lock before creating the child. From `spawn()` through
    // session insertion and supervisor handoff there are no await points, so
    // dropping the HTTP future cannot release admission while a child remains
    // untracked.
    let mut sessions = manager().sessions.lock().await;
    if manager().shutting_down.load(Ordering::Acquire) {
        return Err(ApiError::Unavailable);
    }
    if session_id_reserved(&sessions, session_id) {
        return Err(ApiError::Conflict(
            "PlaySessionId is already active".to_owned(),
        ));
    }
    if sessions.len() >= MAX_SESSION_RECORDS {
        return Err(ApiError::RateLimited);
    }

    let child = command.spawn().map_err(|_| ApiError::Unavailable)?;
    drop(command);
    let (cancel_tx, cancel_rx) = oneshot::channel();
    let last_accessed = Arc::new(AtomicU64::new(now_millis()));
    let session = HlsSession {
        generation,
        item_id: media.item.id,
        owner_id,
        kind,
        directory: directory.clone(),
        last_accessed: last_accessed.clone(),
        cancel: Some(cancel_tx),
        state: SessionState::Running,
        duration_seconds,
        has_subtitles: selected_subtitle.is_some(),
        has_audio: selected_audio.is_some(),
        bandwidth: copy_bandwidth.unwrap_or(max_bitrate),
        options: options.clone(),
        vod: None,
    };
    let job = HlsJob {
        session_id,
        generation,
        directory,
        child,
        cancel_rx,
        last_accessed,
        permit,
    };
    let _worker = hand_off_hls_job(&mut sessions, session_id, session, job);
    directory_guard.keep();
    drop(sessions);
    Ok(session_id)
}

fn validate_options(
    options: &HlsOptions,
    info: &ProbeInfo,
    sidecars: &[subtitles::SidecarSubtitle],
) -> Result<(), ApiError> {
    if options.start_time_ticks.is_some_and(|ticks| ticks < 0) {
        return Err(ApiError::BadRequest(
            "startTimeTicks cannot be negative".to_owned(),
        ));
    }
    if let Some(ticks) = options.start_time_ticks
        && info
            .duration_seconds
            .is_some_and(|duration| ticks as f64 / 10_000_000.0 >= duration)
    {
        return Err(ApiError::BadRequest(
            "startTimeTicks is beyond the media duration".to_owned(),
        ));
    }
    if let Some(index) = options.audio_stream_index
        && (index < -1
            || (index >= 0
                && !info
                    .streams
                    .iter()
                    .any(|stream| stream.index == index as u32 && stream.kind == "audio")))
    {
        return Err(ApiError::BadRequest(
            "audioStreamIndex does not identify an audio stream".to_owned(),
        ));
    }
    if let Some(index) = options.subtitle_stream_index
        && (index < -1
            || (index >= 0
                && !info
                    .streams
                    .iter()
                    .any(|stream| stream.index == index as u32 && stream.kind == "subtitle")
                && !sidecars.iter().any(|track| track.index == index as u32)))
    {
        return Err(ApiError::BadRequest(
            "subtitleStreamIndex does not identify a subtitle stream".to_owned(),
        ));
    }
    Ok(())
}

fn source_stream_bandwidth(
    info: &ProbeInfo,
    video: Option<&super::probe::ProbedStream>,
    audio: Option<&super::probe::ProbedStream>,
) -> Option<u64> {
    if let Some(rate) = info.bit_rate.filter(|rate| *rate > 0) {
        return Some(rate);
    }
    let streams = [video, audio].into_iter().flatten().collect::<Vec<_>>();
    if streams.is_empty() || streams.iter().any(|stream| stream.bit_rate.is_none()) {
        return None;
    }
    let rate = streams
        .into_iter()
        .filter_map(|stream| stream.bit_rate)
        .fold(0_u64, u64::saturating_add);
    (rate > 0).then_some(rate)
}

fn subtitle_codec_supported(codec: &str) -> bool {
    matches!(
        codec,
        "subrip" | "srt" | "ass" | "ssa" | "webvtt" | "mov_text" | "text" | "ttml"
    )
}

async fn extract_subtitle(
    media: &ResolvedMedia,
    directory: &Path,
    ffmpeg: &Path,
    demuxer: &str,
    index: u32,
    start_ticks: Option<i64>,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<tokio::sync::OwnedSemaphorePermit, ApiError> {
    let source_file = super::secure_path::open_media(media.clone()).await?.file;
    let sandbox = MediaChildSandbox::prepare_bounded(
        ffmpeg.to_path_buf(),
        source_file,
        Some(directory.to_path_buf()),
        super::secure_path::filesystem_permit()?,
    )
    .await?;
    let input_fd = sandbox.input_fd().to_string();
    let raw_output = directory.join("subtitle.unsanitized.vtt");
    let mut command = Command::new(sandbox.executable());
    apply_child_limits(
        &mut command,
        45,
        1024 * 1024 * 1024,
        64 * 1024 * 1024,
        Some(sandbox),
    );
    command
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .current_dir(directory)
        .arg("-nostdin")
        .arg("-hide_banner")
        .arg("-loglevel")
        .arg("error")
        .arg("-protocol_whitelist")
        .arg("fd")
        .arg("-fd")
        .arg(&input_fd);
    command
        .arg("-f")
        .arg(demuxer)
        .arg("-i")
        .arg("fd:")
        .arg("-map")
        .arg(format!("0:{index}"))
        .arg("-c:s")
        .arg("webvtt")
        .arg("-fs")
        .arg(MAX_SUBTITLE_BYTES.to_string())
        .arg("-y")
        .arg(&raw_output);
    let (status, permit) =
        wait_for_child_with_admission(command, permit, Duration::from_secs(45)).await?;
    if !status.success() {
        return Err(ApiError::Unavailable);
    }
    let metadata = fs::metadata(&raw_output)
        .await
        .map_err(|_| ApiError::Unavailable)?;
    if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_SUBTITLE_BYTES {
        return Err(ApiError::Unavailable);
    }
    let raw = fs::read(&raw_output)
        .await
        .map_err(|_| ApiError::Unavailable)?;
    let sanitized = subtitles::sanitize_vtt_bytes(raw, start_ticks).await?;
    if sanitized.len() as u64 > MAX_SUBTITLE_BYTES {
        return Err(ApiError::Unavailable);
    }
    fs::write(directory.join("subtitle.vtt"), sanitized)
        .await
        .map_err(|_| ApiError::Unavailable)?;
    let _ = fs::remove_file(raw_output).await;
    Ok(permit)
}

pub(super) async fn wait_for_child_with_admission(
    mut command: Command,
    permit: tokio::sync::OwnedSemaphorePermit,
    child_timeout: Duration,
) -> Result<(std::process::ExitStatus, tokio::sync::OwnedSemaphorePermit), ApiError> {
    command.kill_on_drop(true);
    let mut child = command.spawn().map_err(|_| ApiError::Unavailable)?;
    drop(command);
    let (completed_tx, completed_rx) = oneshot::channel();
    let (cancel_tx, mut cancel_rx) = oneshot::channel();
    tokio::spawn(async move {
        let outcome = tokio::select! {
            _ = &mut cancel_rx => {
                let _ = stop_child(&mut child).await;
                None
            }
            result = timeout(child_timeout, child.wait()) => {
                match result {
                    Ok(Ok(status)) => Some(Ok(status)),
                    Ok(Err(_)) | Err(_) => {
                        let _ = child.kill().await;
                        let _ = child.wait().await;
                        Some(Err(ApiError::Unavailable))
                    }
                }
            }
        };
        if let Some(outcome) = outcome {
            if let Err(abandoned) = completed_tx.send((outcome, permit)) {
                drop(abandoned);
            }
        } else {
            drop(permit);
        }
    });
    let mut cancellation = ChildWaitCancellation(Some(cancel_tx));
    let (outcome, permit) = completed_rx.await.map_err(|_| ApiError::Unavailable)?;
    cancellation.disarm();
    outcome.map(|status| (status, permit))
}

struct ChildWaitCancellation(Option<oneshot::Sender<()>>);

impl ChildWaitCancellation {
    fn disarm(&mut self) {
        self.0.take();
    }
}

impl Drop for ChildWaitCancellation {
    fn drop(&mut self) {
        if let Some(cancel) = self.0.take() {
            let _ = cancel.send(());
        }
    }
}

async fn create_private_dir(
    directory: PathBuf,
    permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<(PrivateDirectoryGuard, tokio::sync::OwnedSemaphorePermit), ApiError> {
    let cleanup_permit = super::secure_path::filesystem_permit()?;
    create_private_dir_with(
        directory,
        permit,
        |directory| {
            if let Some(parent) = directory.parent() {
                std::fs::create_dir_all(parent)?;
            }
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                let mut builder = std::fs::DirBuilder::new();
                builder.mode(0o700).create(directory)
            }
            #[cfg(not(unix))]
            {
                std::fs::create_dir(directory)
            }
        },
        cleanup_permit,
    )
    .await
}

async fn create_private_dir_with<F>(
    directory: PathBuf,
    permit: tokio::sync::OwnedSemaphorePermit,
    create: F,
    cleanup_permit: tokio::sync::OwnedSemaphorePermit,
) -> Result<(PrivateDirectoryGuard, tokio::sync::OwnedSemaphorePermit), ApiError>
where
    F: FnOnce(&Path) -> io::Result<()> + Send + 'static,
{
    let (created_tx, created_rx) = oneshot::channel();
    tokio::task::spawn_blocking(move || {
        let result = create(&directory)
            .map(|()| {
                (
                    PrivateDirectoryGuard::new(directory, cleanup_permit),
                    permit,
                )
            })
            .map_err(|_| ApiError::Unavailable);
        // If the request was cancelled while the filesystem operation ran,
        // send returns ownership of the guard and permit here. Dropping that
        // value removes any directory created by the now-abandoned request.
        if let Err(abandoned) = created_tx.send(result) {
            match abandoned {
                Ok((directory_guard, admission)) => {
                    directory_guard.cleanup_holding(admission);
                }
                Err(error) => drop(error),
            }
        }
    });
    created_rx.await.map_err(|_| ApiError::Unavailable)?
}

async fn run_hls_job(job: HlsJob) {
    let HlsJob {
        session_id,
        generation,
        directory,
        mut child,
        mut cancel_rx,
        last_accessed,
        permit,
    } = job;
    let started = Instant::now();
    let mut ticker = interval(Duration::from_secs(2));
    let mut completed = false;
    loop {
        tokio::select! {
            status = child.wait() => {
                completed = status.is_ok_and(|status| status.success());
                break;
            }
            _ = &mut cancel_rx => {
                let _ = stop_child(&mut child).await;
                break;
            }
            _ = ticker.tick() => {
                let idle_ms = now_millis().saturating_sub(last_accessed.load(Ordering::Relaxed));
                let max_idle_ms = IDLE_KILL_AFTER.as_millis().min(u64::MAX as u128) as u64;
                let total_size = dir_size(&directory).await.unwrap_or(MAX_HLS_OUTPUT_BYTES + 1);
                if idle_ms > max_idle_ms || started.elapsed() > MAX_PROCESS_LIFETIME
                    || total_size > MAX_HLS_OUTPUT_BYTES
                {
                    let _ = stop_child(&mut child).await;
                    break;
                }
            }
        }
    }
    drop(permit);

    if completed {
        let mut sessions = manager().sessions.lock().await;
        let stopping = if let Some(session) = sessions
            .get_mut(&session_id)
            .filter(|session| session.generation == generation)
        {
            if session.state == SessionState::Stopping {
                true
            } else {
                session.state = SessionState::Complete;
                session.cancel = None;
                false
            }
        } else {
            true
        };
        drop(sessions);
        if stopping {
            remove_session(session_id, generation, &directory).await;
            return;
        }
        loop {
            let manager_ref = manager();
            if manager_ref.shutting_down.load(Ordering::Acquire) {
                remove_session(session_id, generation, &directory).await;
                break;
            }
            let shutdown = manager_ref.shutdown.notified();
            tokio::pin!(shutdown);
            tokio::select! {
                _ = sleep(Duration::from_secs(1)) => {},
                _ = &mut shutdown => {},
            }
            if manager_ref.shutting_down.load(Ordering::Acquire) {
                remove_session(session_id, generation, &directory).await;
                break;
            }
            let state = manager()
                .sessions
                .lock()
                .await
                .get(&session_id)
                .filter(|session| session.generation == generation)
                .map(|session| session.state);
            let expired = state.is_none_or(|state| state == SessionState::Stopping)
                || manager()
                    .sessions
                    .lock()
                    .await
                    .get(&session_id)
                    .filter(|session| session.generation == generation)
                    .is_none_or(|session| {
                        now_millis().saturating_sub(session.last_accessed.load(Ordering::Relaxed))
                            > COMPLETED_RETENTION.as_secs().saturating_mul(1000)
                    });
            if expired {
                remove_session(session_id, generation, &directory).await;
                break;
            }
        }
    } else {
        remove_session(session_id, generation, &directory).await;
    }
}

pub(super) async fn dir_size(directory: &Path) -> io::Result<u64> {
    dir_size_with(directory, |entry| async move { entry.metadata().await }).await
}

async fn dir_size_with<F, Fut>(directory: &Path, mut metadata_for: F) -> io::Result<u64>
where
    F: FnMut(fs::DirEntry) -> Fut,
    Fut: std::future::Future<Output = io::Result<std::fs::Metadata>>,
{
    let mut entries = fs::read_dir(directory).await?;
    let mut total = 0_u64;
    let mut count = 0_usize;
    while let Some(entry) = entries.next_entry().await? {
        count += 1;
        if count > MAX_HLS_SEGMENTS + 16 {
            return Ok(MAX_HLS_OUTPUT_BYTES + 1);
        }
        let metadata = match metadata_for(entry).await {
            Ok(metadata) => metadata,
            // FFmpeg publishes segments and playlists by renaming temporary files.
            // A listed entry can disappear before stat without exceeding any budget.
            Err(error) if error.kind() == io::ErrorKind::NotFound => continue,
            Err(error) => return Err(error),
        };
        if metadata.is_file() {
            total = total.saturating_add(metadata.len());
            if total > MAX_HLS_OUTPUT_BYTES {
                return Ok(total);
            }
        }
    }
    Ok(total)
}

pub(super) async fn remove_session(session_id: Uuid, generation: Uuid, directory: &Path) {
    {
        let mut sessions = manager().sessions.lock().await;
        if let Some(session) = sessions
            .get_mut(&session_id)
            .filter(|session| session.generation == generation)
        {
            session.state = SessionState::Stopping;
        }
    }
    let _ = fs::remove_dir_all(directory).await;
    let mut sessions = manager().sessions.lock().await;
    remove_session_record_if_generation(&mut sessions, session_id, generation);
    drop(sessions);
    manager().shutdown.notify_waiters();
}

fn remove_session_record_if_generation(
    sessions: &mut HashMap<Uuid, HlsSession>,
    session_id: Uuid,
    generation: Uuid,
) -> bool {
    if sessions
        .get(&session_id)
        .is_some_and(|session| session.generation == generation)
    {
        sessions.remove(&session_id);
        true
    } else {
        false
    }
}

fn session_id_reserved(sessions: &HashMap<Uuid, HlsSession>, session_id: Uuid) -> bool {
    sessions.contains_key(&session_id)
}

fn hand_off_hls_job(
    sessions: &mut HashMap<Uuid, HlsSession>,
    session_id: Uuid,
    session: HlsSession,
    job: HlsJob,
) -> tokio::task::JoinHandle<()> {
    sessions.insert(session_id, session);
    tokio::spawn(run_hls_job(job))
}

fn session_work_directory(data_dir: &Path, session_id: Uuid, generation: Uuid) -> PathBuf {
    data_dir
        .join("transcodes")
        .join(format!("{session_id}-{generation}"))
}

async fn prune_sessions() {
    let now = now_millis();
    let retention_ms = COMPLETED_RETENTION.as_secs().saturating_mul(1000);
    let stale = {
        let mut sessions = manager().sessions.lock().await;
        let expired = sessions
            .iter()
            .filter_map(|(id, session)| {
                let idle = now.saturating_sub(session.last_accessed.load(Ordering::Relaxed));
                (session.state == SessionState::Complete && idle > retention_ms).then_some(*id)
            })
            .collect::<Vec<_>>();
        expired
            .into_iter()
            .filter_map(|id| {
                sessions
                    .remove(&id)
                    .map(|session| (id, session.directory, session.cancel))
            })
            .collect::<Vec<_>>()
    };
    for (id, directory, cancel) in stale {
        if let Some(cancel) = cancel {
            let _ = cancel.send(());
        }
        let _ = fs::remove_dir_all(directory).await;
        tracing::debug!(session_id = %id, "expired HLS session removed");
    }
}

async fn session_master_info(
    session_id: Uuid,
    owner_id: Uuid,
    item_id: Uuid,
    kind: MediaKind,
) -> Result<(bool, bool, u64, Option<String>, bool, Option<i64>), ApiError> {
    let sessions = manager().sessions.lock().await;
    let session = sessions
        .get(&session_id)
        .filter(|session| {
            session_matches(session, owner_id, item_id, kind)
                && session.state != SessionState::Stopping
        })
        .ok_or(ApiError::NotFound)?;
    Ok((
        session.has_subtitles,
        session.has_audio,
        session.bandwidth,
        session.options.api_key.clone(),
        session.options.stream_copy,
        session
            .options
            .audio_full_timeline
            .then_some(session.options.start_time_ticks)
            .flatten(),
    ))
}

fn session_matches(session: &HlsSession, owner_id: Uuid, item_id: Uuid, kind: MediaKind) -> bool {
    session.owner_id == owner_id && session.item_id == item_id && session.kind == kind
}

#[cfg(test)]
fn take_owned_session(
    sessions: &mut HashMap<Uuid, HlsSession>,
    session_id: Uuid,
    owner_id: Uuid,
    item_id: Uuid,
    kind: MediaKind,
) -> Option<HlsSession> {
    if sessions
        .get(&session_id)
        .is_some_and(|session| session_matches(session, owner_id, item_id, kind))
    {
        sessions.remove(&session_id)
    } else {
        None
    }
}

pub(super) async fn stop_playback_session(owner_id: Uuid, item_id: Uuid, session_id: Uuid) -> bool {
    let cancel = {
        let mut sessions = manager().sessions.lock().await;
        let Some(session) = sessions.get_mut(&session_id) else {
            return false;
        };
        if session.owner_id != owner_id || session.item_id != item_id {
            return false;
        }
        if session.state == SessionState::Stopping {
            None
        } else {
            session.state = SessionState::Stopping;
            session.last_accessed.store(now_millis(), Ordering::Relaxed);
            session.cancel.take()
        }
    };
    if let Some(cancel) = cancel {
        let _ = cancel.send(());
    }
    manager().shutdown.notify_waiters();
    true
}

pub(super) async fn touch_playback_session(
    owner_id: Uuid,
    item_id: Uuid,
    session_id: Uuid,
) -> bool {
    let mut sessions = manager().sessions.lock().await;
    touch_owned_session(&mut sessions, session_id, owner_id, item_id, now_millis())
}

fn touch_owned_session(
    sessions: &mut HashMap<Uuid, HlsSession>,
    session_id: Uuid,
    owner_id: Uuid,
    item_id: Uuid,
    at_millis: u64,
) -> bool {
    let Some(session) = sessions.get_mut(&session_id) else {
        return false;
    };
    if session.owner_id != owner_id
        || session.item_id != item_id
        || session.state == SessionState::Stopping
    {
        return false;
    }
    session.last_accessed.store(at_millis, Ordering::Relaxed);
    true
}

async fn session_directory(
    item_id: Uuid,
    owner_id: Uuid,
    kind: MediaKind,
    session_id: Uuid,
) -> Result<(PathBuf, bool, u32, SessionState), ApiError> {
    let mut sessions = manager().sessions.lock().await;
    let Some(session) = sessions.get_mut(&session_id) else {
        return Err(ApiError::NotFound);
    };
    if !session_matches(session, owner_id, item_id, kind) || session.state == SessionState::Stopping
    {
        return Err(ApiError::NotFound);
    }
    session.last_accessed.store(now_millis(), Ordering::Relaxed);
    Ok((
        session.directory.clone(),
        session.has_subtitles,
        session.duration_seconds,
        session.state,
    ))
}

async fn session_vod(
    item_id: Uuid,
    owner_id: Uuid,
    kind: MediaKind,
    session_id: Uuid,
) -> Result<Option<Arc<super::vod_hls::VodSession>>, ApiError> {
    let mut sessions = manager().sessions.lock().await;
    let session = sessions
        .get_mut(&session_id)
        .filter(|session| {
            session_matches(session, owner_id, item_id, kind)
                && session.state != SessionState::Stopping
        })
        .ok_or(ApiError::NotFound)?;
    session.last_accessed.store(now_millis(), Ordering::Relaxed);
    Ok(session.vod.clone())
}

pub(super) async fn video_playlist(
    State(state): State<AppState>,
    MediaUser(user): MediaUser,
    AxumPath((item_id, session_id)): AxumPath<(Uuid, Uuid)>,
) -> Result<Response, ApiError> {
    serve_playlist(state, user, item_id, session_id, MediaKind::Video).await
}

pub(super) async fn audio_playlist(
    State(state): State<AppState>,
    MediaUser(user): MediaUser,
    AxumPath((item_id, session_id)): AxumPath<(Uuid, Uuid)>,
) -> Result<Response, ApiError> {
    serve_playlist(state, user, item_id, session_id, MediaKind::Audio).await
}

async fn serve_playlist(
    state: AppState,
    user: crate::auth::UserRecord,
    item_id: Uuid,
    session_id: Uuid,
    kind: MediaKind,
) -> Result<Response, ApiError> {
    let _media = authorized_media(&state, &user, item_id).await?;
    if let Some(vod) = session_vod(item_id, user.id, kind, session_id).await? {
        let (_, _, _, api_key, _, _) =
            session_master_info(session_id, user.id, item_id, kind).await?;
        return Ok(text_response(
            super::vod_hls::playlist(&vod, item_id, session_id, kind, api_key.as_deref()),
            "application/vnd.apple.mpegurl",
            StatusCode::OK,
        ));
    }
    let (directory, _, _, _) = session_directory(item_id, user.id, kind, session_id).await?;
    let manifest = directory.join("stream.m3u8");
    let bytes = wait_for_file(&manifest, MAX_PLAYLIST_BYTES, Duration::from_secs(10)).await?;
    let (_, _, _, api_key, _, start_ticks) =
        session_master_info(session_id, user.id, item_id, kind).await?;
    let body = rewrite_playlist_with_api_key(
        &bytes,
        item_id,
        session_id,
        kind,
        api_key.as_deref(),
        start_ticks,
    )?;
    Ok(text_response(
        body,
        "application/vnd.apple.mpegurl",
        StatusCode::OK,
    ))
}

pub(super) async fn video_segment(
    State(state): State<AppState>,
    MediaUser(user): MediaUser,
    AxumPath((item_id, session_id, segment_name)): AxumPath<(Uuid, Uuid, String)>,
) -> Result<Response, ApiError> {
    serve_segment(
        state,
        user,
        item_id,
        session_id,
        segment_name,
        MediaKind::Video,
    )
    .await
}

pub(super) async fn audio_segment(
    State(state): State<AppState>,
    MediaUser(user): MediaUser,
    AxumPath((item_id, session_id, segment_name)): AxumPath<(Uuid, Uuid, String)>,
) -> Result<Response, ApiError> {
    serve_segment(
        state,
        user,
        item_id,
        session_id,
        segment_name,
        MediaKind::Audio,
    )
    .await
}

async fn serve_segment(
    state: AppState,
    user: crate::auth::UserRecord,
    item_id: Uuid,
    session_id: Uuid,
    segment_name: String,
    kind: MediaKind,
) -> Result<Response, ApiError> {
    let _media = authorized_media(&state, &user, item_id).await?;
    let fmp4 = {
        let sessions = manager().sessions.lock().await;
        let session = sessions
            .get(&session_id)
            .filter(|session| {
                session_matches(session, user.id, item_id, kind)
                    && session.state != SessionState::Stopping
            })
            .ok_or(ApiError::NotFound)?;
        session.options.audio_fmp4
    };
    if segment_name == "init.mp4" && fmp4 {
        let (directory, _, _, _) = session_directory(item_id, user.id, kind, session_id).await?;
        return serve_generated_file(directory.join("init.mp4"), "audio/mp4", MAX_SEGMENT_BYTES)
            .await;
    }
    let index = parse_segment_name(&segment_name).ok_or(ApiError::NotFound)?;
    if segment_name.ends_with(".m4s") != fmp4 {
        return Err(ApiError::NotFound);
    }
    if let Some(vod) = session_vod(item_id, user.id, kind, session_id).await? {
        return super::vod_hls::segment(vod, index).await;
    }
    let (directory, _, _, _) = session_directory(item_id, user.id, kind, session_id).await?;
    let path = directory.join(segment_name);
    serve_generated_file(
        path,
        if fmp4 { "audio/mp4" } else { "video/mp2t" },
        MAX_SEGMENT_BYTES,
    )
    .await
}

pub(super) async fn subtitle_playlist(
    State(state): State<AppState>,
    MediaUser(user): MediaUser,
    AxumPath((item_id, session_id)): AxumPath<(Uuid, Uuid)>,
) -> Result<Response, ApiError> {
    let _media = authorized_media(&state, &user, item_id).await?;
    let (directory, has_subtitles, duration, _) =
        session_directory(item_id, user.id, MediaKind::Video, session_id).await?;
    if !has_subtitles {
        return Err(ApiError::NotFound);
    }
    let (_, _, _, api_key, _, _) =
        session_master_info(session_id, user.id, item_id, MediaKind::Video).await?;
    let subtitle_path = append_api_key(
        &format!("/Videos/{item_id}/hls/{session_id}/subtitle.vtt"),
        api_key.as_deref(),
    );
    let playlist = format!(
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:{duration}\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:{duration}.000,\n{subtitle_path}\n#EXT-X-ENDLIST\n"
    );
    // Do not serve a cue track until extraction has completed and the file has
    // a bounded, regular-file size.
    let _ = directory;
    Ok(text_response(
        playlist,
        "application/vnd.apple.mpegurl",
        StatusCode::OK,
    ))
}

pub(super) async fn subtitle_file(
    State(state): State<AppState>,
    MediaUser(user): MediaUser,
    AxumPath((item_id, session_id)): AxumPath<(Uuid, Uuid)>,
) -> Result<Response, ApiError> {
    let _media = authorized_media(&state, &user, item_id).await?;
    let (directory, has_subtitles, _, _) =
        session_directory(item_id, user.id, MediaKind::Video, session_id).await?;
    if !has_subtitles {
        return Err(ApiError::NotFound);
    }
    if session_vod(item_id, user.id, MediaKind::Video, session_id)
        .await?
        .is_some()
    {
        let bytes = wait_for_file(
            &directory.join("subtitle.vtt"),
            MAX_SUBTITLE_BYTES as usize,
            Duration::from_secs(10),
        )
        .await?;
        return Ok(text_response(
            add_hls_timestamp_map(&bytes, 0)?,
            "text/vtt; charset=utf-8",
            StatusCode::OK,
        ));
    }
    let subtitle = read_hls_subtitle_with_timestamp_map(directory, MediaKind::Video).await?;
    Ok(text_response(
        subtitle,
        "text/vtt; charset=utf-8",
        StatusCode::OK,
    ))
}

async fn read_hls_subtitle_with_timestamp_map(
    directory: PathBuf,
    kind: MediaKind,
) -> Result<String, ApiError> {
    let deadline = Instant::now() + Duration::from_secs(10);
    loop {
        let subtitle_ready =
            generated_file_ready(&directory.join("subtitle.vtt"), MAX_SUBTITLE_BYTES).await?;
        let segment_ready =
            generated_file_ready(&directory.join("segment000000.ts"), MAX_SEGMENT_BYTES).await?;
        if subtitle_ready && segment_ready {
            break;
        }
        if Instant::now() >= deadline {
            return Err(ApiError::Unavailable);
        }
        sleep(Duration::from_millis(250)).await;
    }

    let permit = super::secure_path::filesystem_permit()?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        read_hls_subtitle_with_timestamp_map_blocking(&directory, kind)
    })
    .await
    .map_err(|_| ApiError::Unavailable)?
}

fn read_hls_subtitle_with_timestamp_map_blocking(
    directory: &Path,
    kind: MediaKind,
) -> Result<String, ApiError> {
    let dir_file = std::fs::OpenOptions::new()
        .read(true)
        .custom_flags(libc::O_DIRECTORY | libc::O_NOFOLLOW | libc::O_CLOEXEC)
        .open(directory)
        .map_err(|_| ApiError::Unavailable)?;
    if !dir_file
        .metadata()
        .map_err(|_| ApiError::Unavailable)?
        .is_dir()
    {
        return Err(ApiError::Unavailable);
    }
    let capability = CapabilityDir::from_std_file(dir_file);

    let subtitle = read_hls_output_child(&capability, "subtitle.vtt", MAX_SUBTITLE_BYTES, None)?
        .ok_or(ApiError::Unavailable)?;
    let segment = read_hls_output_child(
        &capability,
        "segment000000.ts",
        MAX_SEGMENT_BYTES,
        Some(MAX_TIMESTAMP_SCAN_BYTES),
    )?
    .ok_or(ApiError::Unavailable)?;
    if subtitle.is_empty() || segment.is_empty() {
        return Err(ApiError::Unavailable);
    }
    let timestamp = first_presentation_timestamp(&segment, kind).ok_or(ApiError::Unavailable)?;
    add_hls_timestamp_map(&subtitle, timestamp)
}

async fn generated_file_ready(path: &Path, max_bytes: u64) -> Result<bool, ApiError> {
    let path = path.to_path_buf();
    let permit = super::secure_path::filesystem_permit()?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        match std::fs::symlink_metadata(path) {
            Ok(metadata)
                if metadata.file_type().is_file()
                    && metadata.len() > 0
                    && metadata.len() <= max_bytes =>
            {
                Ok(true)
            }
            Ok(_) => Err(ApiError::Unavailable),
            Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(false),
            Err(_) => Err(ApiError::Unavailable),
        }
    })
    .await
    .map_err(|_| ApiError::Unavailable)?
}

fn read_hls_output_child(
    directory: &CapabilityDir,
    name: &str,
    max_bytes: u64,
    prefix_bytes: Option<u64>,
) -> Result<Option<Vec<u8>>, ApiError> {
    let mut options = CapabilityOpenOptions::new();
    options
        .read(true)
        .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
    let file = match directory.open_with(name, &options) {
        Ok(file) => file,
        Err(error) if error.raw_os_error() == Some(libc::ENOENT) => return Ok(None),
        Err(_) => return Err(ApiError::Unavailable),
    };
    let metadata = file.metadata().map_err(|_| ApiError::Unavailable)?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        return Err(ApiError::Unavailable);
    }
    let read_bytes = prefix_bytes.unwrap_or(metadata.len()).min(metadata.len());
    let mut bytes = Vec::with_capacity(read_bytes as usize);
    file.into_std()
        .take(read_bytes)
        .read_to_end(&mut bytes)
        .map_err(|_| ApiError::Unavailable)?;
    if bytes.len() as u64 != read_bytes {
        return Err(ApiError::Unavailable);
    }
    Ok(Some(bytes))
}

fn add_hls_timestamp_map(vtt: &[u8], presentation_timestamp: u64) -> Result<String, ApiError> {
    let source = std::str::from_utf8(vtt).map_err(|_| ApiError::Unavailable)?;
    let (header, remainder) = source.split_once('\n').ok_or(ApiError::Unavailable)?;
    let header = header.trim_end_matches('\r');
    let cue_data = remainder.trim_start_matches(['\r', '\n']);
    let vtt_header = cue_data.split("\n\n").next().unwrap_or_default();
    if !header.starts_with("WEBVTT")
        || header
            .as_bytes()
            .get(6)
            .is_some_and(|byte| !byte.is_ascii_whitespace())
        || vtt_header
            .lines()
            .any(|line| line.starts_with("X-TIMESTAMP-MAP="))
    {
        return Err(ApiError::Unavailable);
    }
    Ok(format!(
        "{header}\nX-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:{presentation_timestamp}\n\n{cue_data}"
    ))
}

fn first_presentation_timestamp(segment: &[u8], kind: MediaKind) -> Option<u64> {
    const TS_PACKET_BYTES: usize = 188;
    const PTS_MODULUS: u64 = 1 << 33;
    let sync_offset = (0..TS_PACKET_BYTES).find(|offset| {
        segment.get(*offset) == Some(&0x47)
            && segment
                .get(offset + TS_PACKET_BYTES)
                .is_some_and(|byte| *byte == 0x47)
    })?;
    let expected_stream_id = |stream_id: u8| match kind {
        MediaKind::Video => (0xe0..=0xef).contains(&stream_id),
        MediaKind::Audio => (0xc0..=0xdf).contains(&stream_id),
    };
    let (packets, _) = segment[sync_offset..].as_chunks::<TS_PACKET_BYTES>();
    for packet in packets {
        if packet[0] != 0x47 {
            return None;
        }
        let adaptation_control = (packet[3] >> 4) & 0x03;
        if adaptation_control == 0 || adaptation_control == 2 {
            continue;
        }
        let payload_offset = if adaptation_control == 3 {
            5_usize.checked_add(usize::from(packet[4]))?
        } else {
            4
        };
        if payload_offset + 14 > TS_PACKET_BYTES || packet[1] & 0x40 == 0 {
            continue;
        }
        let payload = &packet[payload_offset..];
        if payload[..3] != [0, 0, 1] || !expected_stream_id(payload[3]) {
            continue;
        }
        let flags = payload[7] >> 6;
        if !matches!(flags, 2 | 3) || payload[8] < 5 {
            continue;
        }
        let pts = payload.get(9..14)?;
        let expected_prefix = if flags == 3 { 0x30 } else { 0x20 };
        if pts[0] & 0xf0 != expected_prefix || pts[0] & 1 == 0 || pts[2] & 1 == 0 || pts[4] & 1 == 0
        {
            return None;
        }
        let value = (u64::from((pts[0] >> 1) & 0x07) << 30)
            | (u64::from(pts[1]) << 22)
            | (u64::from((pts[2] >> 1) & 0x7f) << 15)
            | (u64::from(pts[3]) << 7)
            | u64::from((pts[4] >> 1) & 0x7f);
        return (value < PTS_MODULUS).then_some(value);
    }
    None
}

pub(super) async fn stop_video_session(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    AxumPath((item_id, session_id)): AxumPath<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    stop_session(state, user, item_id, session_id, MediaKind::Video).await
}

pub(super) async fn keepalive_video_session(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    AxumPath((item_id, session_id)): AxumPath<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    keepalive_session(state, user, item_id, session_id, MediaKind::Video).await
}

pub(super) async fn stop_audio_session(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    AxumPath((item_id, session_id)): AxumPath<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    stop_session(state, user, item_id, session_id, MediaKind::Audio).await
}

pub(super) async fn keepalive_audio_session(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    AxumPath((item_id, session_id)): AxumPath<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    keepalive_session(state, user, item_id, session_id, MediaKind::Audio).await
}

async fn keepalive_session(
    state: AppState,
    user: crate::auth::UserRecord,
    item_id: Uuid,
    session_id: Uuid,
    kind: MediaKind,
) -> Result<StatusCode, ApiError> {
    let media = authorized_media(&state, &user, item_id).await?;
    let kind_matches = match kind {
        MediaKind::Video => is_video_type(&media.item.item_type),
        MediaKind::Audio => is_audio_type(&media.item.item_type),
    };
    if !kind_matches || !touch_playback_session(user.id, item_id, session_id).await {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn stop_session(
    state: AppState,
    user: crate::auth::UserRecord,
    item_id: Uuid,
    session_id: Uuid,
    kind: MediaKind,
) -> Result<StatusCode, ApiError> {
    let _media = authorized_media(&state, &user, item_id).await?;
    let cancel = {
        let mut sessions = manager().sessions.lock().await;
        let session = sessions.get_mut(&session_id).ok_or(ApiError::NotFound)?;
        if !session_matches(session, user.id, item_id, kind) {
            return Err(ApiError::NotFound);
        }
        if session.state == SessionState::Stopping {
            None
        } else {
            session.state = SessionState::Stopping;
            session.last_accessed.store(now_millis(), Ordering::Relaxed);
            session.cancel.take()
        }
    };
    if let Some(cancel) = cancel {
        let _ = cancel.send(());
    }
    manager().shutdown.notify_waiters();
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn serve_generated_file(
    path: PathBuf,
    content_type: &'static str,
    max_bytes: u64,
) -> Result<Response, ApiError> {
    let metadata = fs::metadata(&path).await.map_err(|_| ApiError::NotFound)?;
    if !metadata.is_file() || metadata.len() > max_bytes {
        return Err(ApiError::NotFound);
    }
    let file = File::open(path).await.map_err(|_| ApiError::NotFound)?;
    let mut response = Response::new(Body::from_stream(tokio_util::io::ReaderStream::new(file)));
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response.headers_mut().insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&metadata.len().to_string()).expect("decimal length is valid"),
    );
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
        .headers_mut()
        .insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    Ok(response)
}

async fn wait_for_file(
    path: &Path,
    max_bytes: usize,
    wait_for: Duration,
) -> Result<Vec<u8>, ApiError> {
    let deadline = Instant::now() + wait_for;
    loop {
        match read_limited_file(path, max_bytes).await {
            Ok(Some(bytes)) if !bytes.is_empty() => return Ok(bytes),
            Ok(Some(_)) => return Err(ApiError::Unavailable),
            Ok(None) => {}
            Err(_) => return Err(ApiError::Unavailable),
        }
        if Instant::now() >= deadline {
            return Err(ApiError::Unavailable);
        }
        sleep(Duration::from_millis(200)).await;
    }
}

async fn read_limited_file(path: &Path, limit: usize) -> io::Result<Option<Vec<u8>>> {
    let file = match File::open(path).await {
        Ok(file) => file,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(None),
        Err(error) => return Err(error),
    };
    let mut output = Vec::with_capacity(16 * 1024);
    let mut limited = file.take((limit + 1) as u64);
    limited.read_to_end(&mut output).await?;
    if output.len() > limit {
        return Err(io::Error::new(
            io::ErrorKind::InvalidData,
            "manifest exceeded limit",
        ));
    }
    Ok(Some(output))
}

#[cfg(test)]
fn rewrite_playlist(
    bytes: &[u8],
    item_id: Uuid,
    session_id: Uuid,
    kind: MediaKind,
) -> Result<String, ApiError> {
    rewrite_playlist_with_api_key(bytes, item_id, session_id, kind, None, None)
}

fn playlist_start_hint(ticks: Option<i64>) -> String {
    let Some(ticks) = ticks.filter(|ticks| *ticks > 0) else {
        return String::new();
    };
    format!(
        "#EXT-X-START:TIME-OFFSET={}.{:07},PRECISE=YES\n",
        ticks / 10_000_000,
        ticks % 10_000_000
    )
}

fn rewrite_playlist_with_api_key(
    bytes: &[u8],
    item_id: Uuid,
    session_id: Uuid,
    kind: MediaKind,
    api_key: Option<&str>,
    start_ticks: Option<i64>,
) -> Result<String, ApiError> {
    let source = std::str::from_utf8(bytes).map_err(|_| ApiError::Unavailable)?;
    let fmp4 = kind == MediaKind::Audio
        && source
            .lines()
            .any(|line| line.trim() == "#EXT-X-MAP:URI=\"init.mp4\"");
    let mut output = String::with_capacity(bytes.len() + 128);
    for line in source.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if line == "#EXTM3U" || line == "#EXT-X-INDEPENDENT-SEGMENTS" || line == "#EXT-X-ENDLIST" {
            output.push_str(line);
        } else if let Some(value) = line.strip_prefix("#EXT-X-VERSION:") {
            if !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(ApiError::Unavailable);
            }
            output.push_str(line);
        } else if let Some(value) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
            if !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(ApiError::Unavailable);
            }
            output.push_str(line);
        } else if let Some(value) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
            if !value.bytes().all(|byte| byte.is_ascii_digit()) {
                return Err(ApiError::Unavailable);
            }
            output.push_str(line);
        } else if line == "#EXT-X-PLAYLIST-TYPE:EVENT" {
            output.push_str(line);
        } else if let Some(value) = line.strip_prefix("#EXTINF:") {
            let duration = value.split(',').next().unwrap_or_default();
            if duration
                .parse::<f64>()
                .ok()
                .filter(|n| n.is_finite() && *n > 0.0)
                .is_none()
            {
                return Err(ApiError::Unavailable);
            }
            output.push_str(line);
        } else if line == "#EXT-X-MAP:URI=\"init.mp4\"" && fmp4 {
            let path = format!("/Audio/{item_id}/hls/{session_id}/init.mp4");
            output.push_str(&format!(
                "#EXT-X-MAP:URI=\"{}\"",
                append_api_key(&path, api_key)
            ));
        } else if line.starts_with('#') {
            // Do not pass through URI-bearing or unknown tags from a generated
            // playlist. They could expose a path or redirect media requests.
            return Err(ApiError::Unavailable);
        } else {
            let index = parse_segment_name(line).ok_or(ApiError::Unavailable)?;
            if line.ends_with(".m4s") != fmp4 {
                return Err(ApiError::Unavailable);
            }
            let extension = if fmp4 { "m4s" } else { "ts" };
            let path = format!(
                "/{}/{item_id}/hls/{session_id}/segment{index:06}.{extension}",
                kind.route_prefix()
            );
            output.push_str(&append_api_key(&path, api_key));
        }
        output.push('\n');
        if line == "#EXTM3U" {
            output.push_str(&playlist_start_hint(start_ticks));
        }
    }
    if !output.starts_with("#EXTM3U\n") {
        return Err(ApiError::Unavailable);
    }
    Ok(output)
}

pub(super) fn append_api_key(path: &str, api_key: Option<&str>) -> String {
    let Some(api_key) = api_key else {
        return path.to_owned();
    };
    let mut serializer = url::form_urlencoded::Serializer::new(String::new());
    serializer.append_pair("ApiKey", api_key);
    let separator = if path.contains('?') { '&' } else { '?' };
    format!("{path}{separator}{}", serializer.finish())
}

fn parse_segment_name(value: &str) -> Option<u32> {
    let numbered = value.strip_prefix("segment")?;
    let digits = numbered
        .strip_suffix(".ts")
        .or_else(|| numbered.strip_suffix(".m4s"))?;
    if digits.len() != 6 || !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let index = digits.parse::<u32>().ok()?;
    (index < MAX_HLS_SEGMENTS as u32).then_some(index)
}

fn text_response(body: String, content_type: &'static str, status: StatusCode) -> Response {
    let mut response = Response::new(Body::from(body));
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
        .headers_mut()
        .insert(CACHE_CONTROL, HeaderValue::from_static("private, no-store"));
    response.headers_mut().insert(
        "x-content-type-options",
        HeaderValue::from_static("nosniff"),
    );
    response
        .headers_mut()
        .insert("referrer-policy", HeaderValue::from_static("no-referrer"));
    response
}

pub(super) fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

#[cfg(test)]
mod tests {
    use super::{
        HlsJob, HlsOptions, HlsSession, MediaKind, SessionState, add_hls_timestamp_map,
        append_api_key, create_private_dir_with, first_presentation_timestamp, hand_off_hls_job,
        manager, parse_segment_name, remove_session_record_if_generation, rewrite_playlist,
        rewrite_playlist_with_api_key, session_id_reserved, session_work_directory, start_hls,
        stop_playback_session, take_owned_session, touch_owned_session,
        wait_for_child_with_admission,
    };
    use crate::{
        config::Config, library::ItemRecord, media_features::secure_path::ResolvedMedia,
        state::AppState,
    };
    use chrono::Utc;
    use sqlx::postgres::PgPoolOptions;
    use std::{
        collections::HashMap,
        fs,
        path::{Path, PathBuf},
        process::Stdio,
        sync::mpsc,
        sync::{
            Arc,
            atomic::{AtomicU64, Ordering},
        },
        time::Duration,
    };
    use tokio::{
        process::Command,
        sync::{Semaphore, oneshot},
        time::sleep,
    };
    use uuid::Uuid;

    #[tokio::test]
    async fn output_size_scan_tolerates_atomic_segment_publish() {
        let directory = std::env::temp_dir().join(format!("puffinbox-hls-size-{}", Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let temporary = directory.join("segment000001.ts.tmp");
        let published = directory.join("segment000001.ts");
        let stable = directory.join("segment000000.ts");
        fs::write(&temporary, [0_u8; 512]).unwrap();
        fs::write(&stable, [0_u8; 40]).unwrap();
        let result = super::dir_size_with(&directory, |entry| async move {
            if entry.file_name() == "segment000001.ts.tmp" {
                // Publish after enumeration and before stat, exactly as FFmpeg can.
                tokio::fs::rename(
                    entry.path(),
                    entry.path().with_file_name("segment000001.ts"),
                )
                .await?;
            }
            entry.metadata().await
        })
        .await;
        let complete = super::dir_size(&directory).await;
        fs::remove_file(temporary).ok();
        fs::remove_file(published).unwrap();
        fs::remove_file(stable).unwrap();
        fs::remove_dir(directory).unwrap();
        assert!(
            result
                .as_ref()
                .is_ok_and(|bytes| (40..=552).contains(bytes)),
            "an atomic segment publish must not fail the output-budget scan: {result:?}"
        );
        assert_eq!(
            complete.unwrap(),
            552,
            "the next scan includes the published segment"
        );
    }

    #[tokio::test]
    async fn output_size_scan_propagates_access_and_directory_errors() {
        let directory = std::env::temp_dir().join(format!("puffinbox-hls-size-{}", Uuid::new_v4()));
        fs::create_dir(&directory).unwrap();
        let file = directory.join("segment000000.ts");
        fs::write(&file, [0_u8; 16]).unwrap();
        let denied = super::dir_size_with(&directory, |_| async {
            Err(std::io::Error::from(std::io::ErrorKind::PermissionDenied))
        })
        .await;
        fs::remove_file(file).unwrap();
        fs::remove_dir(&directory).unwrap();
        assert_eq!(
            denied.unwrap_err().kind(),
            std::io::ErrorKind::PermissionDenied
        );
        assert_eq!(
            super::dir_size(&directory).await.unwrap_err().kind(),
            std::io::ErrorKind::NotFound,
            "a vanished output directory must still stop the job"
        );
    }

    #[test]
    fn hls_resume_queries_accept_client_and_legacy_spelling() {
        let client = axum::extract::Query::<HlsOptions>::try_from_uri(
            &"/master.m3u8?StartTimeTicks=123456789".parse().unwrap(),
        )
        .unwrap()
        .0;
        let legacy = axum::extract::Query::<HlsOptions>::try_from_uri(
            &"/master.m3u8?startTimeTicks=123456789".parse().unwrap(),
        )
        .unwrap()
        .0;
        assert_eq!(client.start_time_ticks, Some(123_456_789));
        assert_eq!(client, legacy);
        assert!(
            axum::extract::Query::<HlsOptions>::try_from_uri(
                &"/master.m3u8?StartTimeTicks=123456789&startTimeTicks=0"
                    .parse()
                    .unwrap(),
            )
            .is_err()
        );
    }

    #[test]
    fn playlist_rewriter_accepts_only_safe_hls_tags_and_session_segments() {
        let item = Uuid::nil();
        let session = Uuid::new_v4();
        let manifest = b"#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:0\n#EXTINF:4.000000,\nsegment000001.ts\n";
        let rewritten = rewrite_playlist(manifest, item, session, MediaKind::Video).unwrap();
        assert!(rewritten.contains(&format!("/Videos/{item}/hls/{session}/segment000001.ts")));
        assert!(
            rewrite_playlist(
                b"#EXTM3U\n#EXT-X-KEY:URI=\"/etc/passwd\"\n",
                item,
                session,
                MediaKind::Video
            )
            .is_err()
        );
        assert!(
            rewrite_playlist(
                b"#EXTM3U\nhttp://127.0.0.1/evil.ts\n",
                item,
                session,
                MediaKind::Video
            )
            .is_err()
        );
        let audio = rewrite_playlist(manifest, item, session, MediaKind::Audio).unwrap();
        assert!(audio.contains(&format!("/Audio/{item}/hls/{session}/segment000001.ts")));
    }

    #[test]
    fn playlist_child_urls_keep_only_encoded_explicit_query_auth() {
        let item = Uuid::new_v4();
        let session = Uuid::new_v4();
        let manifest = b"#EXTM3U\n#EXTINF:4.0,\nsegment000001.ts\n";
        let api_key = "token+/=value";
        let rewritten = rewrite_playlist_with_api_key(
            manifest,
            item,
            session,
            MediaKind::Video,
            Some(api_key),
            None,
        )
        .unwrap();
        assert!(rewritten.contains(&format!(
            "/Videos/{item}/hls/{session}/segment000001.ts?ApiKey=token%2B%2F%3Dvalue"
        )));
        assert_eq!(append_api_key("/playlist.m3u8", None), "/playlist.m3u8");
        assert_eq!(
            append_api_key("/master.m3u8?playSessionId=one", Some("token value")),
            "/master.m3u8?playSessionId=one&ApiKey=token+value"
        );
    }

    #[test]
    fn fragmented_audio_rewrites_only_its_fixed_init_and_segment_names() {
        let item = Uuid::new_v4();
        let session = Uuid::new_v4();
        let manifest = b"#EXTM3U\n#EXT-X-VERSION:7\n#EXT-X-MAP:URI=\"init.mp4\"\n#EXTINF:1.5,\nsegment000000.m4s\n#EXT-X-ENDLIST\n";
        let body = rewrite_playlist_with_api_key(
            manifest,
            item,
            session,
            MediaKind::Audio,
            Some("token+/="),
            Some(12_345_678),
        )
        .unwrap();
        assert!(body.starts_with("#EXTM3U\n#EXT-X-START:TIME-OFFSET=1.2345678,PRECISE=YES\n"));
        assert!(body.contains(&format!(
            "#EXT-X-MAP:URI=\"/Audio/{item}/hls/{session}/init.mp4?ApiKey=token%2B%2F%3D\""
        )));
        assert!(body.contains(&format!(
            "/Audio/{item}/hls/{session}/segment000000.m4s?ApiKey=token%2B%2F%3D"
        )));
        assert!(rewrite_playlist(manifest, item, session, MediaKind::Video).is_err());
        for replacement in [
            "../init.mp4",
            "https://example.invalid/init.mp4",
            "init.mp4\",BYTERANGE=\"1@0",
        ] {
            let changed = String::from_utf8(manifest.to_vec())
                .unwrap()
                .replace("init.mp4", replacement);
            assert!(rewrite_playlist(changed.as_bytes(), item, session, MediaKind::Audio).is_err());
        }
        let mixed = String::from_utf8(manifest.to_vec())
            .unwrap()
            .replace("segment000000.m4s", "segment000000.ts");
        assert!(rewrite_playlist(mixed.as_bytes(), item, session, MediaKind::Audio).is_err());
    }

    #[test]
    fn segment_names_are_fixed_length_numeric_ids() {
        assert_eq!(parse_segment_name("segment000001.ts"), Some(1));
        assert_eq!(parse_segment_name("segment000001.m4s"), Some(1));
        for name in [
            "../../secret.ts",
            "segment1.ts",
            "segment000001.m3u8",
            "segment999999.ts",
            "../segment000001.ts",
            "https://example.invalid/segment000001.m4s",
        ] {
            assert_eq!(parse_segment_name(name), None);
        }
    }

    #[tokio::test]
    async fn cancelled_private_directory_creation_keeps_lease_and_removes_orphan() {
        let directory =
            std::env::temp_dir().join(format!("puffinbox-hls-create-{}", Uuid::new_v4()));
        let permits = Arc::new(Semaphore::new(1));
        let permit = permits.clone().try_acquire_owned().unwrap();
        let (entered_tx, entered_rx) = mpsc::channel();
        let (release_tx, release_rx) = mpsc::channel();
        let operation = tokio::spawn(create_private_dir_with(
            directory.clone(),
            permit,
            move |path| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                std::fs::create_dir(path)
            },
            crate::media_features::secure_path::filesystem_permit().unwrap(),
        ));

        tokio::task::spawn_blocking(move || {
            entered_rx
                .recv_timeout(Duration::from_secs(2))
                .expect("directory worker entered its blocking section");
        })
        .await
        .unwrap();
        operation.abort();
        assert!(
            permits.clone().try_acquire_owned().is_err(),
            "the blocking directory operation retains the HLS admission lease after request cancellation"
        );

        release_tx.send(()).unwrap();
        tokio::time::timeout(Duration::from_secs(2), async {
            loop {
                if !directory.exists() && permits.clone().try_acquire_owned().is_ok() {
                    break;
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("directory worker releases its lease after the abandoned result is dropped");
        assert!(!directory.exists());
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn cancelled_request_after_hls_handoff_keeps_child_lease_until_reap() {
        let session_id = Uuid::new_v4();
        let owner_id = Uuid::new_v4();
        let item_id = Uuid::new_v4();
        let generation = Uuid::new_v4();
        let directory = std::env::temp_dir().join(format!("puffinbox-hls-handoff-{session_id}"));
        let marker = std::env::temp_dir().join(format!("puffinbox-hls-handoff-pid-{session_id}"));
        fs::create_dir(&directory).unwrap();

        let admission = Arc::new(Semaphore::new(1));
        let permit = admission.clone().try_acquire_owned().unwrap();
        let last_accessed = Arc::new(AtomicU64::new(super::now_millis()));
        let (cancel_tx, cancel_rx) = oneshot::channel();
        let child = waiting_test_child(&marker).spawn().unwrap();
        let session = HlsSession {
            generation,
            item_id,
            owner_id,
            kind: MediaKind::Video,
            directory: directory.clone(),
            last_accessed: last_accessed.clone(),
            cancel: Some(cancel_tx),
            state: SessionState::Running,
            duration_seconds: 30,
            has_subtitles: false,
            has_audio: false,
            bandwidth: 320_000,
            options: HlsOptions {
                play_session_id: Some(session_id),
                ..HlsOptions::default()
            },
            vod: None,
        };
        let job = HlsJob {
            session_id,
            generation,
            directory: directory.clone(),
            child,
            cancel_rx,
            last_accessed,
            permit,
        };
        let (handed_off_tx, handed_off_rx) = oneshot::channel();
        let caller = tokio::spawn(async move {
            let mut sessions = manager().sessions.lock().await;
            let _worker = hand_off_hls_job(&mut sessions, session_id, session, job);
            drop(sessions);
            let _ = handed_off_tx.send(());
            std::future::pending::<()>().await;
        });
        handed_off_rx.await.unwrap();
        let pid = wait_for_test_child_pid(&marker).await;

        caller.abort();
        assert!(admission.clone().try_acquire_owned().is_err());
        assert!(manager().sessions.lock().await.contains_key(&session_id));
        assert!(stop_playback_session(owner_id, item_id, session_id).await);

        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let permit = admission.clone().try_acquire_owned().ok();
                let child_alive = PathBuf::from(format!("/proc/{pid}")).exists();
                let session_present = manager().sessions.lock().await.contains_key(&session_id);
                if let Some(permit) = permit {
                    drop(permit);
                    if !child_alive && !session_present {
                        break;
                    }
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("handoff worker kills and reaps its child before releasing admission");
        let _ = fs::remove_file(marker);
        let _ = fs::remove_dir_all(directory);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn cancelled_subtitle_child_retains_hls_lease_until_kill_and_reap() {
        let marker = std::env::temp_dir().join(format!("puffinbox-child-pid-{}", Uuid::new_v4()));
        let permits = Arc::new(Semaphore::new(1));
        let permit = permits.clone().try_acquire_owned().unwrap();
        let operation = tokio::spawn(wait_for_child_with_admission(
            waiting_test_child(&marker),
            permit,
            Duration::from_secs(30),
        ));
        let pid = wait_for_test_child_pid(&marker).await;

        operation.abort();
        assert!(permits.clone().try_acquire_owned().is_err());
        wait_for_child_lease_and_exit(&permits, pid).await;
        let _ = fs::remove_file(marker);
    }

    #[cfg(target_os = "linux")]
    #[tokio::test]
    async fn subtitle_child_timeout_kills_and_reaps_before_releasing_hls_lease() {
        let marker =
            std::env::temp_dir().join(format!("puffinbox-child-timeout-{}", Uuid::new_v4()));
        let permits = Arc::new(Semaphore::new(1));
        let permit = permits.clone().try_acquire_owned().unwrap();
        let result = wait_for_child_with_admission(
            waiting_test_child(&marker),
            permit,
            Duration::from_millis(250),
        )
        .await;
        assert!(matches!(result, Err(crate::ApiError::Unavailable)));
        let pid = wait_for_test_child_pid(&marker).await;
        wait_for_child_lease_and_exit(&permits, pid).await;
        let _ = fs::remove_file(marker);
    }

    #[cfg(target_os = "linux")]
    fn waiting_test_child(marker: &Path) -> Command {
        let mut command = Command::new(std::env::current_exe().unwrap());
        command
            .kill_on_drop(true)
            .arg("--exact")
            .arg("media_features::hls::tests::waiting_test_child_probe")
            .arg("--nocapture")
            .env("PUFFINBOX_HLS_WAIT_CHILD", marker)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null());
        command
    }

    #[test]
    fn waiting_test_child_probe() {
        let Some(marker) = std::env::var_os("PUFFINBOX_HLS_WAIT_CHILD") else {
            return;
        };
        let marker = PathBuf::from(marker);
        let temporary = marker.with_extension(format!("tmp-{}", std::process::id()));
        std::fs::write(&temporary, std::process::id().to_string()).unwrap();
        std::fs::rename(temporary, marker).unwrap();
        std::thread::sleep(Duration::from_secs(30));
    }

    #[cfg(target_os = "linux")]
    async fn wait_for_test_child_pid(marker: &Path) -> libc::pid_t {
        for _ in 0..200 {
            if let Ok(value) = tokio::fs::read_to_string(marker).await
                && let Ok(pid) = value.trim().parse::<libc::pid_t>()
            {
                return pid;
            }
            sleep(Duration::from_millis(10)).await;
        }
        panic!("test subprocess did not publish its PID")
    }

    #[cfg(target_os = "linux")]
    async fn wait_for_child_lease_and_exit(permits: &Arc<Semaphore>, pid: libc::pid_t) {
        tokio::time::timeout(Duration::from_secs(3), async {
            loop {
                let permit = permits.clone().try_acquire_owned().ok();
                let child_alive = PathBuf::from(format!("/proc/{pid}")).exists();
                if let Some(permit) = permit {
                    drop(permit);
                    if !child_alive {
                        return;
                    }
                }
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("the child is reaped before its HLS lease becomes available");
    }

    #[test]
    fn session_removal_requires_matching_owner_item_and_media_route() {
        let session_id = Uuid::new_v4();
        let owner_id = Uuid::new_v4();
        let item_id = Uuid::new_v4();
        let session = HlsSession {
            generation: Uuid::new_v4(),
            item_id,
            owner_id,
            kind: MediaKind::Video,
            directory: std::env::temp_dir(),
            last_accessed: Arc::new(AtomicU64::new(0)),
            cancel: None,
            state: SessionState::Running,
            duration_seconds: 1,
            has_subtitles: false,
            has_audio: true,
            bandwidth: 320_000,
            options: HlsOptions {
                play_session_id: Some(session_id),
                ..HlsOptions::default()
            },
            vod: None,
        };
        let mut sessions = HashMap::from([(session_id, session)]);

        assert!(
            take_owned_session(
                &mut sessions,
                session_id,
                Uuid::new_v4(),
                item_id,
                MediaKind::Video
            )
            .is_none()
        );
        assert!(
            take_owned_session(
                &mut sessions,
                session_id,
                owner_id,
                Uuid::new_v4(),
                MediaKind::Video
            )
            .is_none()
        );
        assert!(
            take_owned_session(
                &mut sessions,
                session_id,
                owner_id,
                item_id,
                MediaKind::Audio
            )
            .is_none()
        );
        assert!(
            sessions.contains_key(&session_id),
            "failed ownership checks must preserve the live session"
        );
        assert!(
            take_owned_session(
                &mut sessions,
                session_id,
                owner_id,
                item_id,
                MediaKind::Video
            )
            .is_some()
        );
        assert!(!sessions.contains_key(&session_id));
    }

    #[test]
    fn late_cleanup_cannot_remove_a_replacement_with_the_same_play_session_id() {
        let base =
            std::env::temp_dir().join(format!("puffinbox-session-generation-{}", Uuid::new_v4()));
        let session_id = Uuid::new_v4();
        let owner_id = Uuid::new_v4();
        let item_id = Uuid::new_v4();
        let old_generation = Uuid::new_v4();
        let new_generation = Uuid::new_v4();
        let old_directory = session_work_directory(&base, session_id, old_generation);
        let new_directory = session_work_directory(&base, session_id, new_generation);
        fs::create_dir_all(&old_directory).unwrap();
        let old = HlsSession {
            generation: old_generation,
            item_id,
            owner_id,
            kind: MediaKind::Video,
            directory: old_directory.clone(),
            last_accessed: Arc::new(AtomicU64::new(5)),
            cancel: None,
            state: SessionState::Stopping,
            duration_seconds: 1,
            has_subtitles: false,
            has_audio: true,
            bandwidth: 320_000,
            options: HlsOptions {
                play_session_id: Some(session_id),
                ..HlsOptions::default()
            },
            vod: None,
        };
        let mut sessions = HashMap::from([(session_id, old)]);
        assert!(session_id_reserved(&sessions, session_id));
        assert!(
            sessions
                .get(&session_id)
                .is_some_and(|session| session.state == SessionState::Stopping),
            "the ID stays reserved until the old worker acknowledges cancellation"
        );
        assert!(remove_session_record_if_generation(
            &mut sessions,
            session_id,
            old_generation
        ));
        fs::create_dir_all(&new_directory).unwrap();

        let replacement = HlsSession {
            generation: new_generation,
            item_id,
            owner_id,
            kind: MediaKind::Video,
            directory: new_directory.clone(),
            last_accessed: Arc::new(AtomicU64::new(10)),
            cancel: None,
            state: SessionState::Running,
            duration_seconds: 1,
            has_subtitles: false,
            has_audio: true,
            bandwidth: 320_000,
            options: HlsOptions {
                play_session_id: Some(session_id),
                ..HlsOptions::default()
            },
            vod: None,
        };
        sessions.insert(session_id, replacement);

        assert_ne!(old_directory, new_directory);
        assert!(!remove_session_record_if_generation(
            &mut sessions,
            session_id,
            old_generation
        ));
        fs::remove_dir_all(old_directory).unwrap();
        let current = sessions.get(&session_id).unwrap();
        assert_eq!(current.generation, new_generation);
        assert_eq!(current.directory, new_directory);
        assert!(new_directory.is_dir());
        fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn playback_heartbeat_renews_only_the_matching_hls_lease() {
        let session_id = Uuid::new_v4();
        let owner_id = Uuid::new_v4();
        let item_id = Uuid::new_v4();
        let last_accessed = Arc::new(AtomicU64::new(5));
        let session = HlsSession {
            generation: Uuid::new_v4(),
            item_id,
            owner_id,
            kind: MediaKind::Video,
            directory: std::env::temp_dir(),
            last_accessed: last_accessed.clone(),
            cancel: None,
            state: SessionState::Complete,
            duration_seconds: 1,
            has_subtitles: false,
            has_audio: true,
            bandwidth: 320_000,
            options: HlsOptions {
                play_session_id: Some(session_id),
                ..HlsOptions::default()
            },
            vod: None,
        };
        let mut sessions = HashMap::from([(session_id, session)]);

        assert!(!touch_owned_session(
            &mut sessions,
            session_id,
            Uuid::new_v4(),
            item_id,
            10
        ));
        assert!(!touch_owned_session(
            &mut sessions,
            session_id,
            owner_id,
            Uuid::new_v4(),
            10
        ));
        assert_eq!(last_accessed.load(Ordering::Relaxed), 5);
        assert!(touch_owned_session(
            &mut sessions,
            session_id,
            owner_id,
            item_id,
            42
        ));
        assert_eq!(last_accessed.load(Ordering::Relaxed), 42);
        sessions.get_mut(&session_id).unwrap().state = SessionState::Stopping;
        assert!(!touch_owned_session(
            &mut sessions,
            session_id,
            owner_id,
            item_id,
            50
        ));
        assert_eq!(last_accessed.load(Ordering::Relaxed), 42);
    }

    #[test]
    fn hls_timestamp_uses_selected_pes_pts_instead_of_dts_or_other_streams() {
        let audio_pts = 126_000;
        let video_pts = 127_920;
        let video_dts = 123_456;
        let mut segment = test_ts_packet(0xc0, 0x101, audio_pts, None);
        segment.extend(test_ts_packet(0xe0, 0x102, video_pts, Some(video_dts)));
        segment.extend(test_ts_packet(0xe0, 0x102, video_pts + 3_003, None));

        assert_eq!(
            first_presentation_timestamp(&segment, MediaKind::Video),
            Some(video_pts),
            "video subtitles map to video presentation time, not the earlier audio PTS or video DTS"
        );
        assert_eq!(
            first_presentation_timestamp(&segment, MediaKind::Audio),
            Some(audio_pts)
        );
    }

    #[test]
    fn hls_timestamp_parser_handles_pts_wrap_and_rejects_bad_markers() {
        let wrapped_pts = (1_u64 << 33) - 7;
        let mut segment = test_ts_packet(0xe0, 0x100, wrapped_pts, Some(wrapped_pts - 90_000));
        segment.extend(test_ts_packet(0xc0, 0x101, 90_000, None));
        assert_eq!(
            first_presentation_timestamp(&segment, MediaKind::Video),
            Some(wrapped_pts)
        );

        let mut malformed = test_ts_packet(0xe0, 0x100, 90_000, None);
        malformed.extend(test_ts_packet(0xc0, 0x101, 90_000, None));
        malformed[4 + 9] ^= 0x01;
        assert_eq!(
            first_presentation_timestamp(&malformed, MediaKind::Video),
            None,
            "bad PTS marker bits must fail instead of inventing a timestamp"
        );
    }

    #[test]
    fn hls_map_is_added_only_to_hls_delivery_and_preserves_cue_text() {
        let standalone = b"WEBVTT\n\n00:00:02.219 --> 00:00:06.219\nlocalized cue\n";
        let mapped = add_hls_timestamp_map(standalone, 127_920).unwrap();
        assert!(mapped.starts_with("WEBVTT\nX-TIMESTAMP-MAP=LOCAL:00:00:00.000,MPEGTS:127920\n\n"));
        assert!(mapped.ends_with("00:00:02.219 --> 00:00:06.219\nlocalized cue\n"));
        assert!(
            !std::str::from_utf8(standalone)
                .unwrap()
                .contains("X-TIMESTAMP-MAP")
        );
        assert!(add_hls_timestamp_map(mapped.as_bytes(), 42).is_err());
    }

    fn test_ts_packet(stream_id: u8, pid: u16, pts: u64, dts: Option<u64>) -> Vec<u8> {
        let mut packet = vec![0xff; 188];
        packet[0] = 0x47;
        packet[1] = 0x40 | ((pid >> 8) as u8 & 0x1f);
        packet[2] = pid as u8;
        packet[3] = 0x10;
        let payload = &mut packet[4..];
        payload[..4].copy_from_slice(&[0x00, 0x00, 0x01, stream_id]);
        payload[4..6].copy_from_slice(&[0x00, 0x00]);
        payload[6] = 0x80;
        payload[7] = if dts.is_some() { 0xc0 } else { 0x80 };
        payload[8] = if dts.is_some() { 10 } else { 5 };
        payload[9..14].copy_from_slice(&encode_test_pts(pts, if dts.is_some() { 3 } else { 2 }));
        if let Some(dts) = dts {
            payload[14..19].copy_from_slice(&encode_test_pts(dts, 1));
        }
        packet
    }

    fn encode_test_pts(value: u64, prefix: u8) -> [u8; 5] {
        [
            (prefix << 4) | (((value >> 30) as u8 & 0x07) << 1) | 1,
            (value >> 22) as u8,
            (((value >> 15) as u8 & 0x7f) << 1) | 1,
            (value >> 7) as u8,
            ((value as u8 & 0x7f) << 1) | 1,
        ]
    }

    #[tokio::test]
    async fn external_ffmpeg_transcodes_video_with_subtitles_and_audio_only_media() {
        let Some(ffmpeg) = find_program("ffmpeg") else {
            eprintln!("ffmpeg is not installed; skipped external HLS fixture");
            return;
        };
        let Some(ffprobe) = ffmpeg.parent().map(|path| path.join("ffprobe")) else {
            return;
        };
        if !ffprobe.is_file() {
            eprintln!("ffprobe is not beside configured ffmpeg; skipped external HLS fixture");
            return;
        }

        let base = std::env::temp_dir().join(format!("puffinbox-hls-{}", Uuid::new_v4()));
        fs::create_dir_all(&base).unwrap();
        let video_path = base.join("fixture.mkv");
        let subtitle_path = base.join("fixture.srt");
        fs::write(
            &subtitle_path,
            "1\n00:00:00,000 --> 00:00:02,500\n<script>alert(1)</script> fixture subtitle\n",
        )
        .unwrap();
        let generated = Command::new(&ffmpeg)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .arg("-hide_banner")
            .arg("-loglevel")
            .arg("error")
            .arg("-nostdin")
            .arg("-f")
            .arg("lavfi")
            .arg("-i")
            .arg("testsrc2=size=320x240:rate=24:duration=3")
            .arg("-f")
            .arg("lavfi")
            .arg("-i")
            .arg("sine=frequency=440:sample_rate=48000:duration=3")
            .arg("-f")
            .arg("srt")
            .arg("-i")
            .arg(&subtitle_path)
            .args([
                "-map",
                "0:v:0",
                "-map",
                "1:a:0",
                "-map",
                "2:0",
                "-c:v",
                "libx264",
                "-threads",
                "1",
                "-pix_fmt",
                "yuv420p",
                "-c:a",
                "aac",
                "-c:s",
                "srt",
                "-t",
                "3",
                "-output_ts_offset",
                "3.25",
                "-y",
            ])
            .arg(&video_path)
            .output()
            .await
            .unwrap();
        assert!(
            generated.status.success(),
            "fixture generation failed: {}",
            String::from_utf8_lossy(&generated.stderr)
        );
        let source_timing = Command::new(&ffprobe)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .args([
                "-v",
                "error",
                "-show_entries",
                "format=start_time:stream=codec_type,start_time",
                "-of",
                "json",
            ])
            .arg(&video_path)
            .output()
            .await
            .unwrap();
        assert!(
            source_timing.status.success(),
            "ffprobe must inspect the nonzero-timestamp source fixture: {}",
            String::from_utf8_lossy(&source_timing.stderr)
        );
        let timing: serde_json::Value = serde_json::from_slice(&source_timing.stdout).unwrap();
        let source_start = timing["format"]["start_time"]
            .as_str()
            .unwrap()
            .parse::<f64>()
            .unwrap();
        let video_start = timing["streams"]
            .as_array()
            .unwrap()
            .iter()
            .find(|stream| stream["codec_type"] == "video")
            .and_then(|stream| stream["start_time"].as_str())
            .unwrap()
            .parse::<f64>()
            .unwrap();
        let audio_start = timing["streams"]
            .as_array()
            .unwrap()
            .iter()
            .find(|stream| stream["codec_type"] == "audio")
            .and_then(|stream| stream["start_time"].as_str())
            .unwrap()
            .parse::<f64>()
            .unwrap();
        assert!(
            (3.24..3.26).contains(&video_start),
            "video stream start time should be 3.25s, got {video_start}"
        );
        assert!(
            (3.20..3.30).contains(&source_start) && (3.20..3.30).contains(&audio_start),
            "container and audio start times should be nonzero despite encoder priming: format={source_start}, audio={audio_start}"
        );

        let data_dir = base.join("data");
        let config = Config {
            bind: "127.0.0.1:8096".parse().unwrap(),
            public_base_url: None,
            database_url: "postgres://fixture:fixture@127.0.0.1/fixture".to_owned(),
            server_name: "fixture".to_owned(),
            web_root: base.join("web"),
            data_dir: data_dir.clone(),
            ffmpeg_path: Some(ffmpeg.clone()),
            max_scan_workers: 1,
            max_page_size: 50,
            access_token_lifetime_hours: 24,
            cookie_secure: false,
            cors_origins: Vec::new(),
            trusted_proxies: Vec::new(),
            local_networks: Vec::new(),
            setup_token: None,
            bootstrap_admin_username: None,
            bootstrap_admin_password: None,
        };
        let db = PgPoolOptions::new()
            .connect_lazy(&config.database_url)
            .unwrap();
        let state = AppState::new(db, Arc::new(config), Uuid::new_v4(), None);
        assert!(
            super::ffmpeg_hls_available(&state).await,
            "external FFmpeg must expose libx264, AAC and HLS"
        );

        let video_id = Uuid::new_v4();
        let owner_id = Uuid::new_v4();
        let video_media = resolved_fixture(&base, &video_path, video_id, "Movie");
        let info = super::probe::probe(&state, &video_media)
            .await
            .unwrap()
            .expect("probe fixture video");
        let audio_index = info
            .streams
            .iter()
            .find(|stream| stream.kind == "audio")
            .expect("audio track")
            .index as i32;
        let subtitle_index = info
            .streams
            .iter()
            .find(|stream| stream.kind == "subtitle")
            .expect("embedded subtitle track")
            .index as i32;
        let embedded_stream = info
            .streams
            .iter()
            .find(|stream| stream.index == subtitle_index as u32)
            .unwrap();
        let subtitle_admission = super::super::subtitles::subtitle_request_permit().unwrap();
        let (standalone_raw_vtt, subtitle_admission) = super::super::subtitles::extract_embedded(
            &state,
            &video_media,
            embedded_stream,
            super::super::subtitles::OutputFormat::WebVtt,
            subtitle_admission,
        )
        .await
        .expect("public embedded-subtitle extraction path");
        let fractional_resume_ticks = 12_345_678;
        let standalone_vtt = super::super::subtitles::sanitize_vtt_bytes(
            standalone_raw_vtt,
            Some(fractional_resume_ticks),
        )
        .await
        .unwrap();
        drop(subtitle_admission);
        let standalone_vtt = String::from_utf8(standalone_vtt).unwrap();
        assert!(!standalone_vtt.contains("X-TIMESTAMP-MAP"));
        let standalone_cue = standalone_vtt
            .lines()
            .find(|line| line.contains("-->"))
            .expect("standalone embedded subtitle cue");
        let (standalone_start, standalone_end) = standalone_cue.split_once("-->").unwrap();
        assert_eq!(parse_vtt_time(standalone_start.trim()), Some(0));
        assert!(
            (1_100..1_500).contains(&parse_vtt_time(standalone_end.trim()).unwrap()),
            "standalone at-position subtitle uses the output time origin: {standalone_cue}"
        );
        assert!(!standalone_vtt.contains("<script>"));

        let profile_request = serde_json::from_value(serde_json::json!({
            "DeviceProfile": {
                "DirectPlayProfiles": [{
                    "Type": "Video",
                    "Container": "mp4",
                    "VideoCodec": "h264",
                    "AudioCodec": "aac"
                }],
                "TranscodingProfiles": [{
                    "Type": "Video",
                    "Container": "ts",
                    "Protocol": "hls",
                    "VideoCodec": "h264",
                    "AudioCodec": "aac",
                    "MaxAudioChannels": "1"
                }],
                "MaxStreamingBitrate": 10000000,
                "MaxAudioChannels": 1,
                "SubtitleProfiles": [{"Format": "vtt", "Method": "External"}]
            },
            "AudioStreamIndex": audio_index,
            "SubtitleStreamIndex": subtitle_index,
            "MaxStreamingBitrate": 10000000,
            "MaxAudioChannels": 1
        }))
        .unwrap();
        let negotiated =
            super::super::playback::negotiate(&state, video_media.clone(), profile_request)
                .await
                .expect("H.264/AAC in Matroska can be copied into HLS TS for this profile");
        let negotiated_wire = serde_json::to_value(negotiated).unwrap();
        let negotiated_source = &negotiated_wire["MediaSources"][0];
        assert_eq!(negotiated_source["SupportsDirectPlay"], false);
        assert_eq!(negotiated_source["SupportsDirectStream"], true);
        assert_eq!(negotiated_source["SupportsTranscoding"], true);
        assert_eq!(negotiated_source["TranscodingSubProtocol"], "hls");
        assert_eq!(negotiated_source["TranscodingContainer"], "ts");
        assert_eq!(negotiated_source["DefaultAudioStreamIndex"], audio_index);
        assert_eq!(
            negotiated_source["DefaultSubtitleStreamIndex"],
            subtitle_index
        );
        let negotiated_url = negotiated_source["DirectStreamUrl"].as_str().unwrap();
        assert!(negotiated_url.starts_with(&format!("/Videos/{video_id}/master.m3u8?")));
        assert!(negotiated_url.contains("streamCopy=true"));
        assert!(negotiated_url.contains(&format!("audioStreamIndex={audio_index}")));
        assert!(negotiated_url.contains(&format!("subtitleStreamIndex={subtitle_index}")));
        assert!(negotiated_url.contains("maxAudioChannels=1"));
        let negotiated_session_id =
            Uuid::parse_str(negotiated_wire["PlaySessionId"].as_str().unwrap()).unwrap();

        let play_session_id = Uuid::new_v4();
        let video_options = HlsOptions {
            play_session_id: Some(play_session_id),
            audio_stream_index: Some(audio_index),
            subtitle_stream_index: Some(subtitle_index),
            max_streaming_bitrate: Some(900_000),
            max_audio_channels: Some(1),
            ..HlsOptions::default()
        };
        let session_id = start_hls(
            &state,
            video_media.clone(),
            video_options.clone(),
            MediaKind::Video,
            owner_id,
        )
        .await
        .unwrap();
        assert_eq!(
            session_id, play_session_id,
            "HLS uses the negotiated playback session ID"
        );
        let video_directory = wait_for_completed_session(session_id).await;
        let video_manifest = fs::read_to_string(video_directory.join("stream.m3u8")).unwrap();
        assert!(video_manifest.contains("#EXTINF:"));
        let video_segment = find_segment(&video_directory);
        let subtitle = fs::read_to_string(video_directory.join("subtitle.vtt")).unwrap();
        assert!(subtitle.starts_with("WEBVTT"));
        assert!(!subtitle.contains("X-TIMESTAMP-MAP"));
        assert!(subtitle.contains("fixture subtitle"));
        assert!(
            !subtitle.contains("<script>"),
            "embedded cues must be plain text safe for browser subtitle rendering"
        );
        // FFmpeg may itself entity-encode markup while converting through
        // mov_text, so the stable security property is that no tag reaches the
        // delivered WebVTT as markup. The sidecar sanitizer test covers exact
        // escaping when raw SRT text is supplied directly.
        let video_pts =
            first_presentation_timestamp(&fs::read(&video_segment).unwrap(), MediaKind::Video)
                .expect("video HLS segment must expose the selected video PTS");
        assert!(video_pts > 0, "HLS segment has a valid nonzero video PTS");
        let hls_subtitle = add_hls_timestamp_map(subtitle.as_bytes(), video_pts).unwrap();
        assert!(hls_subtitle.contains(&format!("MPEGTS:{video_pts}")));
        assert!(hls_subtitle.contains("fixture subtitle"));
        assert_segment_codecs(&ffprobe, &video_segment, true, Some("h264"), 1).await;

        let resume_session_id = Uuid::new_v4();
        let resume_options = HlsOptions {
            play_session_id: Some(resume_session_id),
            start_time_ticks: Some(fractional_resume_ticks),
            audio_stream_index: Some(audio_index),
            subtitle_stream_index: Some(subtitle_index),
            max_streaming_bitrate: Some(900_000),
            max_audio_channels: Some(1),
            ..HlsOptions::default()
        };
        let resumed_id = start_hls(
            &state,
            video_media.clone(),
            resume_options,
            MediaKind::Video,
            owner_id,
        )
        .await
        .expect("fractional resume offset should create a transcode HLS session");
        let resumed_directory = wait_for_completed_session(resumed_id).await;
        let resumed_subtitle = fs::read_to_string(resumed_directory.join("subtitle.vtt")).unwrap();
        let first_cue = resumed_subtitle
            .lines()
            .find(|line| line.contains("-->"))
            .expect("resumed subtitle cue");
        let (cue_start, cue_end) = first_cue.split_once("-->").unwrap();
        assert_eq!(parse_vtt_time(cue_start.trim()), Some(0));
        let cue_end_ms = parse_vtt_time(cue_end.trim()).expect("valid resumed cue end");
        assert!(
            (1_100..1_500).contains(&cue_end_ms),
            "subtitle cue should be offset by the fractional seek: {first_cue}"
        );
        let resumed_segment = find_segment(&resumed_directory);
        let resumed_pts =
            first_presentation_timestamp(&fs::read(&resumed_segment).unwrap(), MediaKind::Video)
                .expect("fractionally resumed segment must expose its video PTS");
        let resumed_hls_vtt = add_hls_timestamp_map(resumed_subtitle.as_bytes(), resumed_pts)
            .expect("fractional resume maps cue-local time to actual segment PTS");
        assert!(resumed_hls_vtt.contains(&format!("MPEGTS:{resumed_pts}")));
        assert!(resumed_hls_vtt.contains(first_cue));
        assert_segment_codecs(&ffprobe, &resumed_segment, true, Some("h264"), 1).await;
        let resumed_duration = segment_duration_seconds(&ffprobe, &resumed_segment).await;
        assert!(
            (1.8..2.2).contains(&resumed_duration),
            "resumed media should contain only the source tail, got {resumed_duration:.3}s"
        );
        assert!(super::stop_playback_session(owner_id, video_id, resumed_id).await);

        let retried_id = start_hls(
            &state,
            video_media.clone(),
            video_options,
            MediaKind::Video,
            owner_id,
        )
        .await
        .unwrap();
        assert_eq!(
            retried_id, session_id,
            "a repeated negotiated master request must reuse its existing session"
        );

        assert!(super::ffmpeg_hls_copy_available(&state).await);
        let copy_session_id = negotiated_session_id;
        let copy_options = HlsOptions {
            play_session_id: Some(copy_session_id),
            stream_copy: true,
            audio_stream_index: Some(audio_index),
            subtitle_stream_index: Some(subtitle_index),
            max_streaming_bitrate: Some(900_000),
            max_audio_channels: Some(1),
            ..HlsOptions::default()
        };
        let copied_session_id = start_hls(
            &state,
            video_media.clone(),
            copy_options,
            MediaKind::Video,
            owner_id,
        )
        .await
        .expect("compatible H.264/AAC media should be remuxable to HLS");
        let copied_directory = wait_for_completed_session(copied_session_id).await;
        let copied_segment = find_segment(&copied_directory);
        let copied_pts =
            first_presentation_timestamp(&fs::read(&copied_segment).unwrap(), MediaKind::Video)
                .expect("copied HLS segment must expose its video PTS");
        let copied_subtitle = fs::read(copied_directory.join("subtitle.vtt")).unwrap();
        let copied_hls_vtt = add_hls_timestamp_map(&copied_subtitle, copied_pts)
            .expect("remux subtitle mapping uses the actual copied mux timeline");
        assert!(copied_hls_vtt.contains(&format!("MPEGTS:{copied_pts}")));
        assert_segment_codecs(&ffprobe, &copied_segment, true, Some("h264"), 1).await;
        let original_frame_hashes = decoded_video_frame_hashes(&ffmpeg, &video_path).await;
        let copied_frame_hashes = decoded_video_frame_hashes(&ffmpeg, &copied_segment).await;
        assert!(!original_frame_hashes.is_empty());
        assert_eq!(
            copied_frame_hashes, original_frame_hashes,
            "stream copy must preserve every decoded video frame exactly"
        );
        assert!(super::stop_playback_session(owner_id, video_id, copied_session_id).await);
        assert!(super::stop_playback_session(owner_id, video_id, session_id).await);

        let last_stream_index = info.streams.iter().map(|stream| stream.index).max();
        let sidecars = super::super::subtitles::list_sidecars(&video_media, last_stream_index)
            .await
            .unwrap();
        let sidecar_index = sidecars.first().expect("external fixture subtitle").index as i32;
        let sidecar_session_id = Uuid::new_v4();
        let sidecar_options = HlsOptions {
            play_session_id: Some(sidecar_session_id),
            subtitle_stream_index: Some(sidecar_index),
            ..HlsOptions::default()
        };
        let sidecar_session = start_hls(
            &state,
            video_media,
            sidecar_options,
            MediaKind::Video,
            owner_id,
        )
        .await
        .unwrap();
        let sidecar_directory = wait_for_completed_session(sidecar_session).await;
        let sidecar_vtt = fs::read_to_string(sidecar_directory.join("subtitle.vtt")).unwrap();
        assert!(sidecar_vtt.contains("fixture subtitle"));
        assert!(!sidecar_vtt.contains("<script>"));
        assert!(super::stop_playback_session(owner_id, video_id, sidecar_session).await);

        let audio_path = base.join("fixture.flac");
        let generated_audio = Command::new(&ffmpeg)
            .env_clear()
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .args([
                "-hide_banner",
                "-loglevel",
                "error",
                "-nostdin",
                "-f",
                "lavfi",
                "-i",
                "sine=frequency=660:sample_rate=44100:duration=2",
                "-c:a",
                "flac",
                "-y",
            ])
            .arg(&audio_path)
            .output()
            .await
            .unwrap();
        assert!(
            generated_audio.status.success(),
            "audio fixture generation failed: {}",
            String::from_utf8_lossy(&generated_audio.stderr)
        );
        let audio_id = Uuid::new_v4();
        let audio_media = resolved_fixture(&base, &audio_path, audio_id, "Audio");
        for start_ticks in [0, 12_345_678] {
            let request = serde_json::from_value(serde_json::json!({
                "DeviceProfile": {
                    "DirectPlayProfiles": [{"Type": "Audio", "Container": "flac", "AudioCodec": "flac"}]
                },
                "StartTimeTicks": start_ticks,
                "EnableTranscoding": false
            }))
            .unwrap();
            let response = super::super::playback::negotiate(&state, audio_media.clone(), request)
                .await
                .expect("a matching original FLAC remains directly playable at a resume position");
            let wire = serde_json::to_value(response).unwrap();
            let source = &wire["MediaSources"][0];
            assert_eq!(source["SupportsDirectPlay"], true);
            assert_eq!(source["SupportsTranscoding"], false);
            assert_eq!(
                source["DirectStreamUrl"],
                format!("/Audio/{audio_id}/stream")
            );
            // This fixture deliberately has no matching catalog identity.
            assert!(source["RunTimeTicks"].is_null());
            assert!(source.get("TranscodingUrl").is_none());
        }
        let audio_session_id = start_hls(
            &state,
            audio_media.clone(),
            HlsOptions::default(),
            MediaKind::Audio,
            owner_id,
        )
        .await
        .unwrap();
        let audio_directory = wait_for_completed_session(audio_session_id).await;
        let audio_manifest = fs::read_to_string(audio_directory.join("stream.m3u8")).unwrap();
        assert!(audio_manifest.contains("#EXTINF:"));
        assert_segment_codecs(&ffprobe, &find_segment(&audio_directory), false, None, 2).await;
        assert!(super::stop_playback_session(owner_id, audio_id, audio_session_id).await);
        let fmp4_options = super::super::universal_audio::transcode_options_for_test(
            "Container=mp3&TranscodingProtocol=hls&TranscodingContainer=mp4&AudioCodec=aac&AudioBitRate=64000&MaxAudioChannels=1&MaxAudioSampleRate=22050&StartTimeTicks=5000000",
        );
        let fmp4_session_id = start_hls(
            &state,
            audio_media,
            fmp4_options,
            MediaKind::Audio,
            owner_id,
        )
        .await
        .unwrap();
        let fmp4_directory = wait_for_completed_session(fmp4_session_id).await;
        let fmp4_manifest = fs::read(fmp4_directory.join("stream.m3u8")).unwrap();
        let rewritten = rewrite_playlist_with_api_key(
            &fmp4_manifest,
            audio_id,
            fmp4_session_id,
            MediaKind::Audio,
            None,
            Some(5_000_000),
        )
        .unwrap();
        assert!(rewritten.contains("#EXT-X-MAP:URI="));
        assert!(rewritten.contains("#EXT-X-START:TIME-OFFSET=0.5000000,PRECISE=YES"));
        assert!(rewritten.contains("segment000000.m4s"));
        let mut encoded = fs::read(fmp4_directory.join("init.mp4")).unwrap();
        encoded.extend(fs::read(fmp4_directory.join("segment000000.m4s")).unwrap());
        let combined = base.join("converted.m4a");
        fs::write(&combined, encoded).unwrap();
        let inspected = Command::new(&ffprobe)
            .env_clear()
            .args([
                "-v",
                "error",
                "-show_entries",
                "stream=codec_name,channels,sample_rate,start_time:format=duration",
                "-of",
                "json",
            ])
            .arg(&combined)
            .output()
            .await
            .unwrap();
        assert!(
            inspected.status.success(),
            "fragmented AAC did not decode: {}",
            String::from_utf8_lossy(&inspected.stderr)
        );
        let info: serde_json::Value = serde_json::from_slice(&inspected.stdout).unwrap();
        assert_eq!(info["streams"][0]["codec_name"], "aac");
        assert_eq!(info["streams"][0]["sample_rate"], "22050");
        assert_eq!(info["streams"][0]["channels"], 1);
        let start: f64 = info["streams"][0]["start_time"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            start.abs() < 0.05,
            "universal audio lost its original timeline: {start}"
        );
        let duration: f64 = info["format"]["duration"]
            .as_str()
            .unwrap()
            .parse()
            .unwrap();
        assert!(
            (1.95..2.15).contains(&duration),
            "universal audio clipped the original timeline: {duration}"
        );
        assert!(super::stop_playback_session(owner_id, audio_id, fmp4_session_id).await);
        let _ = fs::remove_dir_all(base);
    }

    fn find_program(name: &str) -> Option<PathBuf> {
        std::env::var_os("PATH")?
            .to_string_lossy()
            .split(':')
            .map(Path::new)
            .map(|directory| directory.join(name))
            .find(|path| path.is_file())
    }

    fn resolved_fixture(root: &Path, path: &Path, id: Uuid, item_type: &str) -> ResolvedMedia {
        let canonical_root = fs::canonicalize(root).unwrap();
        let absolute_path = fs::canonicalize(path).unwrap();
        let relative = absolute_path
            .strip_prefix(&canonical_root)
            .unwrap()
            .to_path_buf();
        let root = Arc::new(
            cap_std::fs::Dir::open_ambient_dir(&canonical_root, cap_std::ambient_authority())
                .unwrap(),
        );
        let metadata = fs::metadata(path).unwrap();
        ResolvedMedia {
            item: ItemRecord {
                id,
                library_id: Uuid::new_v4(),
                parent_id: None,
                name: path.file_name().unwrap().to_string_lossy().into_owned(),
                sort_name: path.file_name().unwrap().to_string_lossy().into_owned(),
                item_type: item_type.to_owned(),
                path: absolute_path.clone(),
                container: path
                    .extension()
                    .map(|ext| ext.to_string_lossy().into_owned()),
                size_bytes: Some(metadata.len() as i64),
                runtime_ticks: None,
                date_added: Utc::now(),
                date_modified: None,
                rating: None,
                overview: None,
                metadata_json: serde_json::json!({}),
            },
            parent_directory: None,
            root,
            relative,
            absolute_path,
            catalog_identity_matches: false,
        }
    }

    async fn wait_for_completed_session(session_id: Uuid) -> PathBuf {
        for _ in 0..300 {
            let maybe = manager()
                .sessions
                .lock()
                .await
                .get(&session_id)
                .map(|session| (session.state, session.directory.clone()));
            match maybe {
                Some((SessionState::Complete, directory)) => return directory,
                Some((SessionState::Running | SessionState::Stopping, _)) => {
                    sleep(Duration::from_millis(100)).await;
                }
                None => {
                    panic!("FFmpeg session failed or disappeared")
                }
            }
        }
        panic!("FFmpeg session did not complete within 30 seconds")
    }

    fn find_segment(directory: &Path) -> PathBuf {
        fs::read_dir(directory)
            .unwrap()
            .filter_map(Result::ok)
            .map(|entry| entry.path())
            .find(|path| {
                path.file_name().is_some_and(|name| {
                    name.to_string_lossy().starts_with("segment")
                        && path.extension().is_some_and(|ext| ext == "ts")
                })
            })
            .expect("HLS segment output")
    }

    fn parse_vtt_time(value: &str) -> Option<u64> {
        let (hour, rest) = value.split_once(':')?;
        let (minute, rest) = rest.split_once(':')?;
        let (second, millis) = rest.split_once('.')?;
        Some(
            hour.parse::<u64>().ok()? * 3_600_000
                + minute.parse::<u64>().ok()? * 60_000
                + second.parse::<u64>().ok()? * 1000
                + millis.parse::<u64>().ok()?,
        )
    }

    async fn segment_duration_seconds(ffprobe: &Path, segment: &Path) -> f64 {
        let output = Command::new(ffprobe)
            .env_clear()
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .stdout(Stdio::piped())
            .args([
                "-v",
                "error",
                "-protocol_whitelist",
                "file",
                "-f",
                "mpegts",
                "-show_entries",
                "format=duration",
                "-of",
                "json",
                "-i",
            ])
            .arg(segment)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "segment duration inspection failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        value["format"]["duration"]
            .as_str()
            .and_then(|duration| duration.parse::<f64>().ok())
            .expect("segment duration")
    }

    async fn assert_segment_codecs(
        ffprobe: &Path,
        segment: &Path,
        expect_video: bool,
        expected_video_codec: Option<&str>,
        expected_channels: u32,
    ) {
        let output = Command::new(ffprobe)
            .env_clear()
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .stdout(Stdio::piped())
            .args([
                "-v",
                "error",
                "-protocol_whitelist",
                "file",
                "-f",
                "mpegts",
                "-show_entries",
                "stream=codec_type,codec_name,channels",
                "-of",
                "json",
                "-i",
            ])
            .arg(segment)
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "segment inspection failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        let value: serde_json::Value = serde_json::from_slice(&output.stdout).unwrap();
        let streams = value["streams"].as_array().unwrap();
        assert_eq!(
            streams.iter().any(|stream| stream["codec_type"] == "video"),
            expect_video
        );
        if let Some(codec) = expected_video_codec {
            let video = streams
                .iter()
                .find(|stream| stream["codec_type"] == "video")
                .expect("video output stream");
            assert_eq!(video["codec_name"], codec);
        }
        let audio = streams
            .iter()
            .find(|stream| stream["codec_type"] == "audio")
            .expect("AAC output stream");
        assert_eq!(audio["codec_name"], "aac");
        assert_eq!(
            audio["channels"].as_u64(),
            Some(u64::from(expected_channels))
        );
    }

    async fn decoded_video_frame_hashes(ffmpeg: &Path, input: &Path) -> Vec<String> {
        let output = Command::new(ffmpeg)
            .env_clear()
            .stdin(Stdio::null())
            .stderr(Stdio::piped())
            .stdout(Stdio::piped())
            .args(["-hide_banner", "-loglevel", "error", "-i"])
            .arg(input)
            .args(["-map", "0:v:0", "-f", "framemd5", "-"])
            .output()
            .await
            .unwrap();
        assert!(
            output.status.success(),
            "decoded frame hashing failed: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        String::from_utf8_lossy(&output.stdout)
            .lines()
            .filter(|line| !line.starts_with('#') && line.contains(','))
            .filter_map(|line| line.rsplit(',').next().map(str::trim).map(str::to_owned))
            .collect()
    }
}
