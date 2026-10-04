//! Bounded live-channel playback using server-fetched MPEG-TS input and the
//! separately supplied, network-denied FFmpeg process.

use std::{
    collections::{HashMap, HashSet},
    fs::{self, OpenOptions},
    io::Read,
    os::unix::fs::OpenOptionsExt,
    path::{Path as FsPath, PathBuf},
    process::Stdio,
    sync::{
        Arc, OnceLock,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, SystemTime, UNIX_EPOCH},
};

use axum::{
    Router,
    body::Body,
    extract::{Path, Query, State},
    http::{HeaderValue, StatusCode, header},
    response::Response,
    routing::get,
};
use serde::Deserialize;
use tokio::{
    fs as tokio_fs,
    process::Child,
    sync::{Mutex, Notify, OwnedSemaphorePermit, Semaphore, oneshot, watch},
    time::{sleep, timeout},
};
use uuid::Uuid;

use crate::{
    ApiError,
    auth::{CurrentUser, MediaUser, UserRecord},
    state::AppState,
};

use super::{
    livetv::{
        FeedError, LiveInput, MAX_LIVE_INPUT_BYTES, MAX_LIVE_INPUT_SECONDS, OriginPin,
        open_live_input,
    },
    playback::{PlaybackInfoRequest, PlaybackInfoResponse},
    process_limits::{MediaChildSandbox, apply_child_limits, media_command, stop_child},
};

// Hard local limit; concurrent sessions beyond this return 429 until an
// operator-configured admission setting is introduced and validated.
const MAX_LIVE_SESSIONS: usize = 8;
const MAX_SESSION_RECORDS: usize = 128;
const MAX_PLAYLIST_BYTES: usize = 256 * 1024;
const MAX_SEGMENT_BYTES: u64 = 16 * 1024 * 1024;
const LIVE_IDLE_TIMEOUT: Duration = Duration::from_secs(90);
const COMPLETED_RETENTION: Duration = Duration::from_secs(5 * 60);
const PROCESS_MEMORY_BYTES: u64 = 1024 * 1024 * 1024;

#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "camelCase")]
struct LiveHlsOptions {
    #[serde(alias = "PlaySessionId")]
    play_session_id: Option<Uuid>,
    max_streaming_bitrate: Option<u64>,
    max_audio_channels: Option<u32>,
    #[serde(rename = "ApiKey")]
    api_key: Option<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum LiveSessionState {
    Starting,
    Running,
    Complete,
    Stopping,
}

struct LiveSession {
    generation: Uuid,
    run_id: Uuid,
    item_id: Uuid,
    owner_id: Uuid,
    directory: PathBuf,
    options: LiveHlsOptions,
    last_accessed: Arc<AtomicU64>,
    cancel: watch::Sender<bool>,
    state: LiveSessionState,
}

struct LiveSetupJob {
    state: AppState,
    session_id: Uuid,
    generation: Uuid,
    directory: PathBuf,
    source_url: String,
    pins: Vec<OriginPin>,
    options: LiveHlsOptions,
    permit: OwnedSemaphorePermit,
    cancel: watch::Receiver<bool>,
}

struct RunningLiveJob {
    session_id: Uuid,
    generation: Uuid,
    directory: PathBuf,
    child: Child,
    stdin: tokio::process::ChildStdin,
    input: LiveInput,
    cancel: watch::Receiver<bool>,
    permit: OwnedSemaphorePermit,
    ready_tx: oneshot::Sender<()>,
}

struct LiveManager {
    sessions: Mutex<HashMap<Uuid, LiveSession>>,
    permits: Arc<Semaphore>,
    shutdown: Notify,
    shutting_down: AtomicBool,
}

impl LiveManager {
    fn new() -> Self {
        Self {
            sessions: Mutex::new(HashMap::new()),
            permits: Arc::new(Semaphore::new(MAX_LIVE_SESSIONS)),
            shutdown: Notify::new(),
            shutting_down: AtomicBool::new(false),
        }
    }
}

fn manager() -> &'static LiveManager {
    static MANAGER: OnceLock<LiveManager> = OnceLock::new();
    MANAGER.get_or_init(LiveManager::new)
}

pub(super) fn router(state: AppState) -> Router {
    Router::new()
        .route(
            "/LiveTv/Channels/{item_id}/master.m3u8",
            get(master_playlist).head(master_playlist_head),
        )
        .route(
            "/LiveTv/Channels/{item_id}/hls/{session_id}/playlist.m3u8",
            get(media_playlist),
        )
        .route(
            "/LiveTv/Channels/{item_id}/hls/{session_id}/{segment_name}",
            get(media_segment),
        )
        .route(
            "/LiveTv/Channels/{item_id}/hls/{session_id}",
            axum::routing::delete(stop_session),
        )
        .route(
            "/LiveTv/Channels/{item_id}/hls/{session_id}/keepalive",
            axum::routing::post(keepalive),
        )
        .with_state(state)
}

pub(super) async fn playback_info(
    state: &AppState,
    user: &UserRecord,
    item_id: Uuid,
    item_name: String,
    request: &PlaybackInfoRequest,
) -> Result<PlaybackInfoResponse, ApiError> {
    let ffmpeg_available = state
        .config
        .ffmpeg_path
        .as_ref()
        .is_some_and(|path| path.is_file());
    let (url, _pins, _name) = super::livetv_api::channel_source(state, user, item_id).await?;
    validate_live_url(&url)?;
    super::playback::live_playback_info(item_id, item_name, request, ffmpeg_available)
}

fn validate_live_url(raw: &str) -> Result<(), ApiError> {
    if raw.len() > 8192 || raw.chars().any(char::is_control) {
        return Err(ApiError::Unavailable);
    }
    Ok(())
}

async fn master_playlist(
    State(state): State<AppState>,
    MediaUser(user): MediaUser,
    Path(item_id): Path<Uuid>,
    Query(mut options): Query<LiveHlsOptions>,
) -> Result<Response, ApiError> {
    start_session(&state, &user, item_id, &mut options).await?;
    let session_id = options.play_session_id.ok_or(ApiError::Unavailable)?;
    let variant = child_playlist_url(item_id, session_id, options.api_key.as_deref());
    let playlist = format!(
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-STREAM-INF:BANDWIDTH={}\n{}\n",
        options
            .max_streaming_bitrate
            .unwrap_or(2_000_000)
            .min(8_000_000),
        variant
    );
    Ok(text_response(
        playlist,
        "application/vnd.apple.mpegurl",
        StatusCode::OK,
    ))
}

async fn master_playlist_head(
    State(state): State<AppState>,
    MediaUser(user): MediaUser,
    Path(item_id): Path<Uuid>,
    Query(options): Query<LiveHlsOptions>,
) -> Result<Response, ApiError> {
    super::livetv_api::authorize_live_channel(&state, &user, item_id).await?;
    let session_id = options.play_session_id.ok_or(ApiError::BadRequest(
        "PlaySessionId is required for live playback".to_owned(),
    ))?;
    validate_live_options(&options)?;
    let variant = child_playlist_url(item_id, session_id, options.api_key.as_deref());
    let playlist = format!(
        "#EXTM3U\n#EXT-X-VERSION:3\n#EXT-X-STREAM-INF:BANDWIDTH={}\n{}\n",
        options
            .max_streaming_bitrate
            .unwrap_or(2_000_000)
            .min(8_000_000),
        variant
    );
    Ok(text_response(
        playlist,
        "application/vnd.apple.mpegurl",
        StatusCode::OK,
    ))
}

async fn start_session(
    state: &AppState,
    user: &UserRecord,
    item_id: Uuid,
    options: &mut LiveHlsOptions,
) -> Result<(), ApiError> {
    if manager().shutting_down.load(Ordering::Acquire) {
        return Err(ApiError::Unavailable);
    }
    super::livetv_api::authorize_channel(state, user, item_id).await?;
    validate_live_options(options)?;
    let session_id = options.play_session_id.unwrap_or_else(Uuid::new_v4);
    options.play_session_id = Some(session_id);
    {
        let mut sessions = manager().sessions.lock().await;
        if let Some(existing) = sessions.get_mut(&session_id) {
            if session_matches(existing, state.run_id, user.id, item_id)
                && existing.options == *options
                && existing.state != LiveSessionState::Stopping
            {
                touch(existing);
                return Ok(());
            }
            return Err(ApiError::Conflict(
                "PlaySessionId is already active".to_owned(),
            ));
        }
        if sessions.len() >= MAX_SESSION_RECORDS {
            prune_sessions(&mut sessions);
        }
        if sessions.len() >= MAX_SESSION_RECORDS {
            return Err(ApiError::RateLimited);
        }
    }
    let permit = manager()
        .permits
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::RateLimited)?;
    let generation = Uuid::new_v4();
    let directory = live_work_directory(&state.config.data_dir, session_id, generation);
    let (cancel_tx, cancel_rx) = watch::channel(false);
    let session = LiveSession {
        generation,
        run_id: state.run_id,
        item_id,
        owner_id: user.id,
        directory: directory.clone(),
        options: options.clone(),
        last_accessed: Arc::new(AtomicU64::new(now_millis())),
        cancel: cancel_tx,
        state: LiveSessionState::Starting,
    };
    {
        let mut sessions = manager().sessions.lock().await;
        if sessions.contains_key(&session_id) {
            return Err(ApiError::Conflict(
                "PlaySessionId is already active".to_owned(),
            ));
        }
        sessions.insert(session_id, session);
    }

    // Resolve the source after registering the session. If a source is
    // disabled between authorization and this read, it fails closed; if it is
    // disabled afterward, the source lifecycle hook can find and cancel it.
    let (source_url, pins, _) = match super::livetv_api::channel_source(state, user, item_id).await
    {
        Ok(source) => source,
        Err(error) => {
            remove_failed_session(session_id, generation, &directory).await;
            return Err(error);
        }
    };

    let (ready_tx, ready_rx) = oneshot::channel();
    let state = state.clone();
    let options = options.clone();
    tokio::spawn(async move {
        let result = setup_live_job(LiveSetupJob {
            state,
            session_id,
            generation,
            directory: directory.clone(),
            source_url,
            pins,
            options,
            permit,
            cancel: cancel_rx,
        })
        .await;
        if let Err(error) = &result {
            tracing::warn!(session_id = %session_id, error = ?error, "live playback setup failed");
            remove_failed_session(session_id, generation, &directory).await;
        }
        let _ = ready_tx.send(result);
    });
    match timeout(Duration::from_secs(40), ready_rx).await {
        Ok(Ok(result)) => result?,
        Ok(Err(_)) => return Err(ApiError::Unavailable),
        Err(_) => {
            request_session_stop(session_id, generation).await;
            return Err(ApiError::Unavailable);
        }
    }
    Ok(())
}

async fn request_session_stop(session_id: Uuid, generation: Uuid) {
    let mut sessions = manager().sessions.lock().await;
    if let Some(session) = sessions
        .get_mut(&session_id)
        .filter(|session| session.generation == generation)
    {
        session.state = LiveSessionState::Stopping;
        let _ = session.cancel.send(true);
    }
}

/// Stop live sessions for channels invalidated by an administrator source
/// edit or deletion. The admin API calls this after the database transaction
/// that disables those channels has committed.
pub(super) async fn stop_channels(item_ids: &[Uuid]) -> usize {
    if item_ids.is_empty() {
        return 0;
    }
    let item_ids: HashSet<Uuid> = item_ids.iter().copied().collect();
    let mut sessions = manager().sessions.lock().await;
    let mut stopped = 0;
    for session in sessions.values_mut() {
        if item_ids.contains(&session.item_id) && session.state != LiveSessionState::Stopping {
            session.state = LiveSessionState::Stopping;
            let _ = session.cancel.send(true);
            stopped += 1;
        }
    }
    drop(sessions);
    if stopped > 0 {
        manager().shutdown.notify_waiters();
    }
    stopped
}

fn validate_live_options(options: &LiveHlsOptions) -> Result<(), ApiError> {
    if options.max_streaming_bitrate == Some(0)
        || options
            .max_audio_channels
            .is_some_and(|channels| !(1..=32).contains(&channels))
        || options.api_key.as_ref().is_some_and(|value| {
            value.len() > 256 || !value.bytes().all(|byte| byte.is_ascii_graphic())
        })
    {
        return Err(ApiError::BadRequest(
            "Live playback options are invalid".to_owned(),
        ));
    }
    if options
        .max_streaming_bitrate
        .is_some_and(|value| value < 320_000)
    {
        return Err(ApiError::BadRequest(
            "The selected live HLS profile requires at least 320 kbit/s".to_owned(),
        ));
    }
    Ok(())
}

async fn setup_live_job(job: LiveSetupJob) -> Result<(), ApiError> {
    let LiveSetupJob {
        state,
        session_id,
        generation,
        directory,
        source_url,
        pins,
        options,
        permit,
        mut cancel,
    } = job;
    create_live_directory(directory.clone()).await?;
    let input = tokio::select! {
        _ = wait_for_cancel(&mut cancel) => return Err(ApiError::Conflict("Live session stopped".to_owned())),
        result = timeout(Duration::from_secs(30), open_live_input(&source_url, &pins)) => {
            result.map_err(|_| ApiError::Unavailable)?.map_err(feed_error_to_api)?
        }
    };
    let ffmpeg = state
        .config
        .ffmpeg_path
        .clone()
        .ok_or(ApiError::Unavailable)?;
    let sandbox = MediaChildSandbox::prepare_output_bounded(
        ffmpeg,
        Some(directory.clone()),
        super::secure_path::filesystem_permit()?,
    )
    .await?;
    let bitrate = options
        .max_streaming_bitrate
        .unwrap_or(2_000_000)
        .min(8_000_000);
    let audio_channels = options.max_audio_channels.unwrap_or(2).min(2);
    let manifest = directory.join("live.m3u8");
    let segment_pattern = directory.join("segment%05d.ts");
    let mut command = media_command(sandbox.executable());
    apply_child_limits(
        &mut command,
        MAX_LIVE_INPUT_SECONDS.as_secs().saturating_add(60),
        PROCESS_MEMORY_BYTES,
        64 * 1024 * 1024,
        Some(sandbox),
    );
    command
        .env_clear()
        .current_dir(&directory)
        .stdin(Stdio::piped())
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
        .arg("pipe,file")
        .arg("-probesize")
        .arg("8000000")
        .arg("-analyzeduration")
        .arg("8000000")
        .arg("-f")
        .arg("mpegts")
        .arg("-i")
        .arg("pipe:0")
        .arg("-map")
        .arg("0:v:0?")
        .arg("-map")
        .arg("0:a:0?")
        .arg("-sn")
        .arg("-dn")
        .arg("-c:v")
        .arg("libx264")
        .arg("-preset")
        .arg("ultrafast")
        .arg("-tune")
        .arg("zerolatency")
        .arg("-pix_fmt")
        .arg("yuv420p")
        .arg("-b:v")
        .arg((bitrate.saturating_sub(128_000)).to_string())
        .arg("-maxrate")
        .arg((bitrate.saturating_sub(128_000)).to_string())
        .arg("-bufsize")
        .arg(bitrate.to_string())
        .arg("-c:a")
        .arg("aac")
        .arg("-ac")
        .arg(audio_channels.to_string())
        .arg("-b:a")
        .arg("128k")
        .arg("-f")
        .arg("hls")
        .arg("-hls_time")
        .arg("3")
        .arg("-hls_list_size")
        .arg("6")
        .arg("-hls_flags")
        .arg("delete_segments+append_list+omit_endlist+independent_segments+temp_file")
        .arg("-hls_segment_filename")
        .arg(&segment_pattern)
        .arg(&manifest);
    let mut child = command.spawn().map_err(|_| ApiError::Unavailable)?;
    let stdin = child.stdin.take().ok_or(ApiError::Unavailable)?;
    let cancel_rx = cancel.clone();
    if *cancel_rx.borrow() {
        let _ = stop_child(&mut child).await;
        return Err(ApiError::Conflict("Live session stopped".to_owned()));
    }
    let stop_before_start = {
        let mut sessions = manager().sessions.lock().await;
        if let Some(session) = sessions
            .get_mut(&session_id)
            .filter(|session| session.generation == generation)
        {
            let stopping = session.state == LiveSessionState::Stopping;
            if !stopping {
                session.state = LiveSessionState::Running;
            }
            stopping
        } else {
            true
        }
    };
    if stop_before_start {
        let _ = stop_child(&mut child).await;
        return Err(ApiError::Conflict("Live session stopped".to_owned()));
    }
    let (ready_tx, ready_rx) = oneshot::channel();
    tokio::spawn(async move {
        run_live_job(RunningLiveJob {
            session_id,
            generation,
            directory,
            child,
            stdin,
            input,
            cancel: cancel_rx,
            permit,
            ready_tx,
        })
        .await;
    });
    timeout(Duration::from_secs(35), ready_rx)
        .await
        .map_err(|_| ApiError::Unavailable)?
        .map_err(|_| ApiError::Unavailable)?;
    Ok(())
}

async fn run_live_job(job: RunningLiveJob) {
    let RunningLiveJob {
        session_id,
        generation,
        directory,
        mut child,
        mut stdin,
        mut input,
        mut cancel,
        permit,
        ready_tx,
    } = job;
    let (feed_cancel_tx, mut feed_cancel_rx) = oneshot::channel();
    let feed = tokio::spawn(async move {
        let result = input
            .copy_to(
                &mut stdin,
                &mut feed_cancel_rx,
                MAX_LIVE_INPUT_BYTES,
                MAX_LIVE_INPUT_SECONDS,
            )
            .await;
        drop(stdin);
        result
    });
    let _ = ready_tx.send(());
    let started = tokio::time::Instant::now();
    let mut tick = tokio::time::interval(Duration::from_secs(2));
    let mut feed = feed;
    let mut feed_cancel_tx = Some(feed_cancel_tx);
    let completed = loop {
        tokio::select! {
            status = child.wait() => {
                break status.map(|status| status.success()).unwrap_or(false);
            }
            feed_result = &mut feed => {
                let finished = matches!(feed_result, Ok(Ok(_)));
                if finished {
                    break matches!(timeout(Duration::from_secs(8), child.wait()).await, Ok(Ok(status)) if status.success());
                } else {
                    if let Some(cancel) = feed_cancel_tx.take() {
                        let _ = cancel.send(());
                    }
                    let _ = stop_child(&mut child).await;
                    break false;
                }
            }
            changed = cancel.changed() => {
                if changed.is_err() || *cancel.borrow() {
                    if let Some(cancel) = feed_cancel_tx.take() {
                        let _ = cancel.send(());
                    }
                    feed.abort();
                    let _ = stop_child(&mut child).await;
                    break false;
                }
            }
            _ = tick.tick() => {
                let idle = manager().sessions.lock().await.get(&session_id)
                    .filter(|session| session.generation == generation)
                    .is_none_or(|session| now_millis().saturating_sub(session.last_accessed.load(Ordering::Relaxed)) > LIVE_IDLE_TIMEOUT.as_millis() as u64);
                if idle || started.elapsed() > MAX_LIVE_INPUT_SECONDS || dir_size(&directory).await > 2 * 1024 * 1024 * 1024 {
                    if let Some(cancel) = feed_cancel_tx.take() {
                        let _ = cancel.send(());
                    }
                    feed.abort();
                    let _ = stop_child(&mut child).await;
                    let _ = (&mut feed).await;
                    break false;
                }
            }
        }
    };
    drop(permit);
    if completed {
        let mut sessions = manager().sessions.lock().await;
        let stopping = sessions
            .get_mut(&session_id)
            .filter(|session| session.generation == generation)
            .is_some_and(|session| {
                let stopping = session.state == LiveSessionState::Stopping;
                if !stopping {
                    session.state = LiveSessionState::Complete;
                }
                stopping
            });
        drop(sessions);
        if stopping {
            remove_live_session(session_id, generation, &directory).await;
            return;
        }
        loop {
            if manager().shutting_down.load(Ordering::Acquire) {
                break;
            }
            let keep = manager()
                .sessions
                .lock()
                .await
                .get(&session_id)
                .filter(|session| session.generation == generation)
                .is_some_and(|session| {
                    session.state != LiveSessionState::Stopping
                        && now_millis()
                            .saturating_sub(session.last_accessed.load(Ordering::Relaxed))
                            <= COMPLETED_RETENTION.as_millis() as u64
                });
            if !keep {
                break;
            }
            tokio::select! {
                _ = sleep(Duration::from_secs(1)) => {},
                _ = manager().shutdown.notified() => {},
                _ = cancel.changed() => if *cancel.borrow() { break; },
            }
        }
    }
    if let Some(cancel) = feed_cancel_tx.take() {
        let _ = cancel.send(());
    }
    feed.abort();
    remove_live_session(session_id, generation, &directory).await;
}

async fn create_live_directory(directory: PathBuf) -> Result<(), ApiError> {
    let permit = super::secure_path::filesystem_permit()?;
    let (done_tx, done_rx) = oneshot::channel();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let result = (|| {
            let parent = directory
                .parent()
                .ok_or_else(|| std::io::Error::other("missing scratch parent"))?;
            std::fs::create_dir_all(parent)?;
            #[cfg(unix)]
            {
                use std::os::unix::fs::DirBuilderExt;
                let mut builder = std::fs::DirBuilder::new();
                builder.mode(0o700).create(&directory)?;
            }
            #[cfg(not(unix))]
            std::fs::create_dir(&directory)?;
            Ok::<(), std::io::Error>(())
        })();
        if let Err(error) = result {
            let _ = done_tx.send(Err(error.kind()));
        } else if done_tx.send(Ok(())).is_err() {
            let _ = std::fs::remove_dir_all(directory);
        }
    });
    match done_rx.await.map_err(|_| ApiError::Unavailable)? {
        Ok(()) => Ok(()),
        Err(_) => Err(ApiError::Unavailable),
    }
}

async fn remove_failed_session(session_id: Uuid, generation: Uuid, directory: &FsPath) {
    let _ = tokio_fs::remove_dir_all(directory).await;
    let mut sessions = manager().sessions.lock().await;
    if sessions
        .get(&session_id)
        .is_some_and(|session| session.generation == generation)
    {
        sessions.remove(&session_id);
    }
    drop(sessions);
    manager().shutdown.notify_waiters();
}

async fn remove_live_session(session_id: Uuid, generation: Uuid, directory: &FsPath) {
    let _ = tokio_fs::remove_dir_all(directory).await;
    let mut sessions = manager().sessions.lock().await;
    if sessions
        .get(&session_id)
        .is_some_and(|session| session.generation == generation)
    {
        sessions.remove(&session_id);
    }
    drop(sessions);
    manager().shutdown.notify_waiters();
}

fn live_work_directory(data_dir: &FsPath, session_id: Uuid, generation: Uuid) -> PathBuf {
    data_dir
        .join("livetv")
        .join(format!("job-{session_id}-{generation}"))
}

fn now_millis() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .min(u64::MAX as u128) as u64
}

fn touch(session: &mut LiveSession) {
    session.last_accessed.store(now_millis(), Ordering::Relaxed);
}

fn session_matches(session: &LiveSession, run_id: Uuid, owner_id: Uuid, item_id: Uuid) -> bool {
    session.run_id == run_id && session.owner_id == owner_id && session.item_id == item_id
}

fn prune_sessions(sessions: &mut HashMap<Uuid, LiveSession>) {
    sessions.retain(|_, session| {
        session.state != LiveSessionState::Stopping
            && (session.state != LiveSessionState::Complete
                || now_millis().saturating_sub(session.last_accessed.load(Ordering::Relaxed))
                    <= COMPLETED_RETENTION.as_millis() as u64)
    });
}

async fn media_playlist(
    State(state): State<AppState>,
    MediaUser(user): MediaUser,
    Path((item_id, session_id)): Path<(Uuid, Uuid)>,
    Query(options): Query<LiveHlsOptions>,
) -> Result<Response, ApiError> {
    super::livetv_api::authorize_live_channel(&state, &user, item_id).await?;
    let (directory, last_accessed) =
        session_directory(&state, &user, item_id, session_id, &options).await?;
    last_accessed.store(now_millis(), Ordering::Relaxed);
    let path = directory.join("live.m3u8");
    let raw = wait_and_read(&path, MAX_PLAYLIST_BYTES, Duration::from_secs(8)).await?;
    let text = std::str::from_utf8(&raw).map_err(|_| ApiError::Unavailable)?;
    let mut rewritten = String::new();
    for line in text.lines() {
        if line.trim().is_empty() || line.starts_with('#') {
            rewritten.push_str(line);
            rewritten.push('\n');
            continue;
        }
        let name = line.trim();
        if !valid_segment_name(name) {
            return Err(ApiError::Unavailable);
        }
        rewritten.push_str(&segment_url(
            item_id,
            session_id,
            name,
            options.api_key.as_deref(),
        ));
        rewritten.push('\n');
    }
    Ok(text_response(
        rewritten,
        "application/vnd.apple.mpegurl",
        StatusCode::OK,
    ))
}

async fn media_segment(
    State(state): State<AppState>,
    MediaUser(user): MediaUser,
    Path((item_id, session_id, segment_name)): Path<(Uuid, Uuid, String)>,
    Query(options): Query<LiveHlsOptions>,
) -> Result<Response, ApiError> {
    super::livetv_api::authorize_live_channel(&state, &user, item_id).await?;
    if !valid_segment_name(&segment_name) {
        return Err(ApiError::NotFound);
    }
    let (directory, last_accessed) =
        session_directory(&state, &user, item_id, session_id, &options).await?;
    last_accessed.store(now_millis(), Ordering::Relaxed);
    let path = directory.join(&segment_name);
    let bytes = read_scratch_file(path, MAX_SEGMENT_BYTES).await?;
    Ok(binary_response(bytes, "video/mp2t"))
}

async fn session_directory(
    state: &AppState,
    user: &UserRecord,
    item_id: Uuid,
    session_id: Uuid,
    options: &LiveHlsOptions,
) -> Result<(PathBuf, Arc<AtomicU64>), ApiError> {
    let mut sessions = manager().sessions.lock().await;
    let session = sessions.get_mut(&session_id).ok_or(ApiError::NotFound)?;
    if !session_matches(session, state.run_id, user.id, item_id)
        || session.state == LiveSessionState::Stopping
        || options.api_key != session.options.api_key
        || options
            .play_session_id
            .is_some_and(|requested| requested != session_id)
    {
        return Err(ApiError::NotFound);
    }
    touch(session);
    Ok((session.directory.clone(), session.last_accessed.clone()))
}

async fn keepalive(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((item_id, session_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    super::livetv_api::authorize_live_channel(&state, &user, item_id).await?;
    if !touch_playback_session(state.run_id, user.id, item_id, session_id).await {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn stop_session(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((item_id, session_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    super::livetv_api::authorize_channel(&state, &user, item_id).await?;
    if !stop_playback_session(state.run_id, user.id, item_id, session_id).await {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

pub(super) async fn stop_playback_session(
    run_id: Uuid,
    owner_id: Uuid,
    item_id: Uuid,
    session_id: Uuid,
) -> bool {
    let mut sessions = manager().sessions.lock().await;
    let Some(session) = sessions.get_mut(&session_id) else {
        return false;
    };
    if !session_matches(session, run_id, owner_id, item_id)
        || session.state == LiveSessionState::Stopping
    {
        return false;
    }
    session.state = LiveSessionState::Stopping;
    let _ = session.cancel.send(true);
    manager().shutdown.notify_waiters();
    true
}

pub(super) async fn touch_playback_session(
    run_id: Uuid,
    owner_id: Uuid,
    item_id: Uuid,
    session_id: Uuid,
) -> bool {
    let mut sessions = manager().sessions.lock().await;
    let Some(session) = sessions.get_mut(&session_id) else {
        return false;
    };
    if !session_matches(session, run_id, owner_id, item_id)
        || session.state == LiveSessionState::Stopping
    {
        return false;
    }
    touch(session);
    true
}

pub(super) async fn shutdown() -> bool {
    manager().shutting_down.store(true, Ordering::Release);
    {
        let mut sessions = manager().sessions.lock().await;
        for session in sessions.values_mut() {
            session.state = LiveSessionState::Stopping;
            let _ = session.cancel.send(true);
        }
    }
    manager().shutdown.notify_waiters();
    timeout(Duration::from_secs(5), async {
        loop {
            if manager().sessions.lock().await.is_empty() {
                return true;
            }
            sleep(Duration::from_millis(50)).await;
        }
    })
    .await
    .unwrap_or(false)
}

pub(super) async fn recover_orphan_directories(state: &AppState) -> Result<usize, ApiError> {
    let root = state.config.data_dir.join("livetv");
    let permit = super::secure_path::filesystem_permit()?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let metadata = match fs::symlink_metadata(&root) {
            Ok(metadata) if metadata.is_dir() && !metadata.file_type().is_symlink() => metadata,
            Ok(_) => return Err(ApiError::Unavailable),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => return Ok(0),
            Err(_) => return Err(ApiError::Unavailable),
        };
        let _ = metadata;
        let entries = fs::read_dir(&root).map_err(|_| ApiError::Unavailable)?;
        let mut removed = 0;
        let mut inspected = 0;
        for entry in entries {
            inspected += 1;
            if inspected > 256 {
                return Err(ApiError::Unavailable);
            }
            let entry = entry.map_err(|_| ApiError::Unavailable)?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if !name.starts_with("job-") || name.len() > 96 {
                continue;
            }
            let metadata = fs::symlink_metadata(entry.path()).map_err(|_| ApiError::Unavailable)?;
            if metadata.is_dir() && !metadata.file_type().is_symlink() {
                fs::remove_dir_all(entry.path()).map_err(|_| ApiError::Unavailable)?;
                removed += 1;
            }
        }
        Ok(removed)
    })
    .await
    .map_err(|_| ApiError::Unavailable)?
}

async fn read_scratch_file(path: PathBuf, maximum: u64) -> Result<Vec<u8>, ApiError> {
    let permit = super::secure_path::filesystem_permit()?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let mut options = OpenOptions::new();
        options
            .read(true)
            .custom_flags(libc::O_NOFOLLOW | libc::O_NONBLOCK | libc::O_CLOEXEC);
        let file = options.open(path).map_err(|_| ApiError::NotFound)?;
        let metadata = file.metadata().map_err(|_| ApiError::Unavailable)?;
        if !metadata.is_file() || metadata.len() > maximum {
            return Err(ApiError::Unavailable);
        }
        let capacity = usize::try_from(metadata.len()).map_err(|_| ApiError::Unavailable)?;
        let mut bytes = Vec::with_capacity(capacity);
        file.take(maximum.saturating_add(1))
            .read_to_end(&mut bytes)
            .map_err(|_| ApiError::Unavailable)?;
        if bytes.len() as u64 > maximum {
            return Err(ApiError::Unavailable);
        }
        Ok(bytes)
    })
    .await
    .map_err(|_| ApiError::Unavailable)?
}

async fn wait_and_read(path: &FsPath, maximum: usize, wait: Duration) -> Result<Vec<u8>, ApiError> {
    let deadline = tokio::time::Instant::now() + wait;
    loop {
        match read_scratch_file(path.to_owned(), maximum as u64).await {
            Ok(bytes) => return Ok(bytes),
            Err(ApiError::NotFound) if tokio::time::Instant::now() < deadline => {
                sleep(Duration::from_millis(200)).await;
            }
            Err(error) => return Err(error),
        }
    }
}

async fn dir_size(directory: &FsPath) -> u64 {
    let Ok(mut entries) = tokio_fs::read_dir(directory).await else {
        return u64::MAX;
    };
    let mut total = 0_u64;
    let mut count = 0;
    loop {
        match entries.next_entry().await {
            Ok(Some(entry)) => {
                count += 1;
                if count > 4096 {
                    return u64::MAX;
                }
                if let Ok(metadata) = entry.metadata().await
                    && metadata.is_file()
                {
                    total = total.saturating_add(metadata.len());
                }
                if total > 2 * 1024 * 1024 * 1024 {
                    return total;
                }
            }
            Ok(None) => return total,
            Err(_) => return u64::MAX,
        }
    }
}

fn valid_segment_name(value: &str) -> bool {
    value.len() == 15
        && value.starts_with("segment")
        && value.ends_with(".ts")
        && value[7..12].bytes().all(|byte| byte.is_ascii_digit())
}

fn child_playlist_url(item_id: Uuid, session_id: Uuid, api_key: Option<&str>) -> String {
    let base = format!("/LiveTv/Channels/{item_id}/hls/{session_id}/playlist.m3u8");
    append_api_key(base, api_key)
}

fn segment_url(item_id: Uuid, session_id: Uuid, name: &str, api_key: Option<&str>) -> String {
    let base = format!("/LiveTv/Channels/{item_id}/hls/{session_id}/{name}");
    append_api_key(base, api_key)
}

fn append_api_key(mut url: String, api_key: Option<&str>) -> String {
    if let Some(api_key) = api_key {
        let mut serializer = url::form_urlencoded::Serializer::new(String::new());
        serializer.append_pair("ApiKey", api_key);
        url.push('?');
        url.push_str(&serializer.finish());
    }
    url
}

fn text_response(
    body: impl Into<Body>,
    content_type: &'static str,
    status: StatusCode,
) -> Response {
    let mut response = Response::new(body.into());
    *response.status_mut() = status;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

fn binary_response(bytes: Vec<u8>, content_type: &'static str) -> Response {
    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = StatusCode::OK;
    response
        .headers_mut()
        .insert(header::CONTENT_TYPE, HeaderValue::from_static(content_type));
    response
        .headers_mut()
        .insert(header::CACHE_CONTROL, HeaderValue::from_static("no-store"));
    response.headers_mut().insert(
        header::X_CONTENT_TYPE_OPTIONS,
        HeaderValue::from_static("nosniff"),
    );
    response.headers_mut().insert(
        header::REFERRER_POLICY,
        HeaderValue::from_static("no-referrer"),
    );
    response
}

fn feed_error_to_api(error: FeedError) -> ApiError {
    match error {
        FeedError::Unavailable => ApiError::Unavailable,
        FeedError::TooLarge | FeedError::QuotaExceeded => ApiError::RateLimited,
        FeedError::Cancelled => ApiError::Conflict("Live session stopped".to_owned()),
        FeedError::InvalidEncoding
        | FeedError::InvalidStructure
        | FeedError::InvalidValue
        | FeedError::DisallowedOrigin
        | FeedError::LimitExceeded
        | FeedError::DuplicateId
        | FeedError::UnsupportedTransport => ApiError::BadRequest(
            "The configured live source uses an unsupported transport".to_owned(),
        ),
    }
}

async fn wait_for_cancel(cancel: &mut watch::Receiver<bool>) {
    while !*cancel.borrow() {
        if cancel.changed().await.is_err() {
            return;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        LiveHlsOptions, LiveSession, LiveSessionState, child_playlist_url, live_work_directory,
        manager, stop_channels, valid_segment_name, validate_live_options,
    };
    use axum::{extract::Query, http::Uri};
    use uuid::Uuid;

    #[test]
    fn validates_live_profile_bounds_and_credentials() {
        assert!(validate_live_options(&LiveHlsOptions::default()).is_ok());
        assert!(
            validate_live_options(&LiveHlsOptions {
                max_audio_channels: Some(0),
                ..LiveHlsOptions::default()
            })
            .is_err()
        );
        assert!(
            validate_live_options(&LiveHlsOptions {
                max_audio_channels: Some(33),
                ..LiveHlsOptions::default()
            })
            .is_err()
        );
        assert!(
            validate_live_options(&LiveHlsOptions {
                max_streaming_bitrate: Some(0),
                ..LiveHlsOptions::default()
            })
            .is_err()
        );
        assert!(
            validate_live_options(&LiveHlsOptions {
                max_streaming_bitrate: Some(128_000),
                ..LiveHlsOptions::default()
            })
            .is_err()
        );
    }

    #[test]
    fn live_hls_options_accept_the_playback_info_session_id_casing() {
        let session = Uuid::new_v4();
        let uri = Uri::try_from(format!(
            "/?PlaySessionId={session}&maxStreamingBitrate=2000000&maxAudioChannels=2"
        ))
        .unwrap();
        let Query(options) = Query::<LiveHlsOptions>::try_from_uri(&uri).unwrap();
        assert_eq!(options.play_session_id, Some(session));
        assert_eq!(options.max_streaming_bitrate, Some(2_000_000));
        assert_eq!(options.max_audio_channels, Some(2));
    }

    #[tokio::test]
    async fn stop_channels_cancels_only_sessions_for_matching_channels() {
        let channel_id = Uuid::new_v4();
        let other_channel_id = Uuid::new_v4();
        let session_id = Uuid::new_v4();
        let other_session_id = Uuid::new_v4();
        let generation = Uuid::new_v4();
        let other_generation = Uuid::new_v4();
        let (cancel, cancel_rx) = tokio::sync::watch::channel(false);
        let (other_cancel, other_cancel_rx) = tokio::sync::watch::channel(false);
        let make_session = |item_id, generation, cancel| LiveSession {
            generation,
            run_id: Uuid::new_v4(),
            item_id,
            owner_id: Uuid::new_v4(),
            directory: std::path::PathBuf::new(),
            options: LiveHlsOptions::default(),
            last_accessed: std::sync::Arc::new(std::sync::atomic::AtomicU64::new(0)),
            cancel,
            state: LiveSessionState::Running,
        };

        {
            let mut sessions = manager().sessions.lock().await;
            assert!(!sessions.contains_key(&session_id));
            assert!(!sessions.contains_key(&other_session_id));
            sessions.insert(session_id, make_session(channel_id, generation, cancel));
            sessions.insert(
                other_session_id,
                make_session(other_channel_id, other_generation, other_cancel),
            );
        }

        let stopped = stop_channels(&[channel_id]).await;
        let cancel_signalled = *cancel_rx.borrow();
        let other_cancel_signalled = *other_cancel_rx.borrow();
        let (state, other_state) = {
            let sessions = manager().sessions.lock().await;
            (
                sessions.get(&session_id).map(|session| session.state),
                sessions.get(&other_session_id).map(|session| session.state),
            )
        };

        // Remove only our synthetic sessions before assertions so a failure
        // cannot leave test state in the process-wide manager.
        {
            let mut sessions = manager().sessions.lock().await;
            if sessions
                .get(&session_id)
                .is_some_and(|session| session.generation == generation)
            {
                sessions.remove(&session_id);
            }
            if sessions
                .get(&other_session_id)
                .is_some_and(|session| session.generation == other_generation)
            {
                sessions.remove(&other_session_id);
            }
        }

        assert_eq!(stopped, 1);
        assert!(cancel_signalled);
        assert!(!other_cancel_signalled);
        assert_eq!(state, Some(LiveSessionState::Stopping));
        assert_eq!(other_state, Some(LiveSessionState::Running));
    }

    #[test]
    fn generated_hls_uris_keep_credentials_scoped_and_segment_names_bounded() {
        let item = Uuid::new_v4();
        let session = Uuid::new_v4();
        assert_eq!(
            child_playlist_url(item, session, None),
            format!("/LiveTv/Channels/{item}/hls/{session}/playlist.m3u8")
        );
        let with_key = child_playlist_url(item, session, Some("one&two"));
        assert!(with_key.ends_with("?ApiKey=one%26two"));
        assert!(valid_segment_name("segment00042.ts"));
        assert!(!valid_segment_name("../segment.ts"));
        assert!(!valid_segment_name("segment00042.ts/.."));
    }

    #[test]
    fn work_directory_names_are_session_and_generation_scoped() {
        let item = Uuid::new_v4();
        let session = Uuid::new_v4();
        let first = live_work_directory(std::path::Path::new("/data"), session, item);
        let second = live_work_directory(std::path::Path::new("/data"), session, Uuid::new_v4());
        assert_ne!(first, second);
        assert!(first.starts_with("/data/livetv"));
    }
}
