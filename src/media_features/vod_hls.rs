//! Full-duration HLS playlists with bounded, demand-driven transcode batches.

use std::{
    collections::HashSet,
    path::PathBuf,
    process::Stdio,
    sync::{
        Arc,
        atomic::{AtomicBool, AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

use axum::response::Response;
use tokio::{
    fs,
    process::Command,
    sync::{Mutex, OwnedSemaphorePermit, Semaphore, mpsc, oneshot},
    time::{interval, sleep},
};
use uuid::Uuid;

use super::{
    hls::{self, MediaKind},
    process_limits::{MediaChildSandbox, apply_child_limits, stop_child},
    secure_path::{self, ResolvedMedia},
};
use crate::ApiError;

const SEGMENT_MILLIS: u64 = 4000;
const BATCH_SEGMENTS: u32 = 16;
const MAX_SEGMENT_BYTES: u64 = 16 * 1024 * 1024;
const MAX_OUTPUT_BYTES: u64 = 2 * 1024 * 1024 * 1024;
const ROOT_RESERVED_BYTES: u64 = 16 * 1024 * 1024;
const MAX_BATCH_BYTES: u64 = BATCH_SEGMENTS as u64 * MAX_SEGMENT_BYTES;
const IDLE_MILLIS: u64 = 60_000;
const BATCH_TIMEOUT: Duration = Duration::from_secs(120);
const SEGMENT_WAIT: Duration = Duration::from_secs(30);

pub(super) struct VodPlan {
    pub media: ResolvedMedia,
    pub ffmpeg: PathBuf,
    pub demuxer: String,
    pub directory: PathBuf,
    pub duration_millis: u64,
    pub video_index: Option<u32>,
    pub audio_index: Option<u32>,
    pub video_bitrate: u64,
    pub audio_bitrate: u64,
    pub output_channels: u32,
}

pub(super) struct VodSession {
    plan: VodPlan,
    commands: mpsc::Sender<u32>,
    requested: Mutex<HashSet<u32>>,
    failed: Mutex<HashSet<u32>>,
    last_accessed: Arc<AtomicU64>,
    output_bytes: AtomicU64,
    closed: AtomicBool,
}

pub(super) struct VodWorker {
    session: Arc<VodSession>,
    commands: mpsc::Receiver<u32>,
    permits: Arc<Semaphore>,
}

pub(super) fn prepare_session(
    plan: VodPlan,
    last_accessed: Arc<AtomicU64>,
    permits: Arc<Semaphore>,
) -> Result<(Arc<VodSession>, VodWorker), ApiError> {
    if !(1..=14_400_000).contains(&plan.duration_millis) {
        return Err(ApiError::BadRequest(
            "HLS duration is outside the supported range".to_owned(),
        ));
    }
    let (tx, rx) = mpsc::channel(8);
    let session = Arc::new(VodSession {
        plan,
        commands: tx,
        requested: Mutex::new(HashSet::from([0])),
        failed: Mutex::new(HashSet::new()),
        last_accessed,
        output_bytes: AtomicU64::new(ROOT_RESERVED_BYTES),
        closed: AtomicBool::new(false),
    });
    let worker = VodWorker {
        session: session.clone(),
        commands: rx,
        permits,
    };
    Ok((session, worker))
}

fn segment_count(duration_millis: u64) -> u32 {
    duration_millis.div_ceil(SEGMENT_MILLIS) as u32
}

pub(super) fn playlist(
    session: &VodSession,
    item_id: Uuid,
    session_id: Uuid,
    kind: MediaKind,
    api_key: Option<&str>,
) -> String {
    playlist_for_duration(
        session.plan.duration_millis,
        item_id,
        session_id,
        kind,
        api_key,
    )
}

fn playlist_for_duration(
    duration_millis: u64,
    item_id: Uuid,
    session_id: Uuid,
    kind: MediaKind,
    api_key: Option<&str>,
) -> String {
    let mut body = "#EXTM3U\n#EXT-X-VERSION:6\n#EXT-X-TARGETDURATION:4\n#EXT-X-MEDIA-SEQUENCE:0\n#EXT-X-PLAYLIST-TYPE:VOD\n#EXT-X-INDEPENDENT-SEGMENTS\n".to_owned();
    for index in 0..segment_count(duration_millis) {
        let millis = (duration_millis - index as u64 * SEGMENT_MILLIS).min(SEGMENT_MILLIS);
        let path = hls::append_api_key(
            &format!(
                "/{}/{item_id}/hls/{session_id}/segment{index:06}.ts",
                kind.route_prefix()
            ),
            api_key,
        );
        body.push_str(&format!(
            "#EXTINF:{}.{:03},\n{path}\n",
            millis / 1000,
            millis % 1000
        ));
    }
    body.push_str("#EXT-X-ENDLIST\n");
    body
}

pub(super) async fn segment(session: Arc<VodSession>, index: u32) -> Result<Response, ApiError> {
    if index >= segment_count(session.plan.duration_millis)
        || session.closed.load(Ordering::Acquire)
    {
        return Err(ApiError::NotFound);
    }
    let batch = index / BATCH_SEGMENTS;
    let path = session
        .plan
        .directory
        .join(format!("batch{batch:06}"))
        .join(format!("segment{index:06}.ts"));
    if fs::metadata(&path).await.is_err() {
        let mut requested = session.requested.lock().await;
        if requested.insert(batch) && session.commands.try_send(batch).is_err() {
            requested.remove(&batch);
            return Err(ApiError::RateLimited);
        }
    }
    let deadline = Instant::now() + SEGMENT_WAIT;
    loop {
        if session.closed.load(Ordering::Acquire) {
            return Err(ApiError::NotFound);
        }
        if session.failed.lock().await.contains(&batch) {
            return Err(ApiError::Unavailable);
        }
        if let Ok(metadata) = fs::symlink_metadata(&path).await {
            if !metadata.is_file() || metadata.len() == 0 || metadata.len() > MAX_SEGMENT_BYTES {
                return Err(ApiError::Unavailable);
            }
            return hls::serve_generated_file(path, "video/mp2t", MAX_SEGMENT_BYTES).await;
        }
        if Instant::now() >= deadline {
            return Err(ApiError::Unavailable);
        }
        sleep(Duration::from_millis(50)).await;
    }
}

fn expired(session: &VodSession) -> bool {
    hls::now_millis().saturating_sub(session.last_accessed.load(Ordering::Relaxed)) > IDLE_MILLIS
}

pub(super) async fn run_worker(
    mut worker: VodWorker,
    session_id: Uuid,
    generation: Uuid,
    mut cancel: oneshot::Receiver<()>,
    initial_permit: OwnedSemaphorePermit,
) {
    let session = worker.session.clone();
    let mut initial = Some((0, initial_permit));
    let mut ticker = interval(Duration::from_secs(1));
    loop {
        let (batch, permit) = if let Some(initial) = initial.take() {
            initial
        } else {
            let batch = tokio::select! {
                _ = &mut cancel => break,
                _ = ticker.tick() => {
                    if expired(&session) { break; }
                    continue;
                },
                batch = worker.commands.recv() => {
                    let Some(batch) = batch else { break; };
                    batch
                },
            };
            let admission = worker.permits.clone().acquire_owned();
            tokio::pin!(admission);
            let permit = loop {
                tokio::select! {
                    _ = &mut cancel => break None,
                    _ = ticker.tick() => {
                        if expired(&session) { break None; }
                    },
                    permit = &mut admission => break permit.ok(),
                }
            };
            let Some(permit) = permit else {
                break;
            };
            (batch, permit)
        };
        let outcome = run_batch(&session, batch, &mut cancel, permit).await;
        let size = hls::dir_size(&session.plan.directory.join(format!("batch{batch:06}")))
            .await
            .unwrap_or(MAX_OUTPUT_BYTES + 1);
        let prior = session.output_bytes.fetch_add(size, Ordering::Relaxed);
        if prior.saturating_add(size) > MAX_OUTPUT_BYTES {
            break;
        }
        match outcome {
            BatchResult::Complete => {}
            BatchResult::Failed => {
                session.failed.lock().await.insert(batch);
            }
            BatchResult::Stopped => break,
        }
    }
    session.closed.store(true, Ordering::Release);
    hls::remove_session(session_id, generation, &session.plan.directory).await;
}

enum BatchResult {
    Complete,
    Failed,
    Stopped,
}

async fn run_batch(
    session: &VodSession,
    batch: u32,
    cancel: &mut oneshot::Receiver<()>,
    _permit: OwnedSemaphorePermit,
) -> BatchResult {
    let plan = &session.plan;
    let directory = plan.directory.join(format!("batch{batch:06}"));
    let mut command = match prepare_command(plan, batch, directory.clone()).await {
        Ok(command) => command,
        Err(_) => return BatchResult::Failed,
    };
    match cancel.try_recv() {
        Ok(()) | Err(oneshot::error::TryRecvError::Closed) => return BatchResult::Stopped,
        Err(oneshot::error::TryRecvError::Empty) => {}
    }
    let mut child = match command.spawn() {
        Ok(child) => child,
        Err(_) => return BatchResult::Failed,
    };
    drop(command);
    let started = Instant::now();
    let mut ticker = interval(Duration::from_millis(250));
    loop {
        tokio::select! {
            status = child.wait() => {
                if status.is_err() {
                    let _ = stop_child(&mut child).await;
                    return BatchResult::Failed;
                }
                if !status.is_ok_and(|status| status.success()) { return BatchResult::Failed; }
                return BatchResult::Complete;
            },
            _ = &mut *cancel => {
                let _ = stop_child(&mut child).await;
                return BatchResult::Stopped;
            },
            _ = ticker.tick() => {
                let size = match hls::dir_size(&directory).await {
                    Ok(size) => size,
                    Err(error) => {
                        tracing::warn!(batch, error_kind = ?error.kind(), "HLS output size scan failed");
                        let _ = stop_child(&mut child).await;
                        return BatchResult::Stopped;
                    }
                };
                if expired(session) || started.elapsed() > BATCH_TIMEOUT
                    || size > MAX_BATCH_BYTES || session.output_bytes.load(Ordering::Relaxed).saturating_add(size) > MAX_OUTPUT_BYTES {
                    let reason = if expired(session) { "idle" }
                        else if started.elapsed() > BATCH_TIMEOUT { "timeout" }
                        else { "output_budget" };
                    tracing::warn!(batch, reason, "HLS transcode batch stopped by a resource limit");
                    let _ = stop_child(&mut child).await;
                    return BatchResult::Stopped;
                }
            },
        }
    }
}

async fn prepare_command(
    plan: &VodPlan,
    batch: u32,
    directory: PathBuf,
) -> Result<Command, ApiError> {
    let first_segment = batch * BATCH_SEGMENTS;
    let start_millis = first_segment as u64 * SEGMENT_MILLIS;
    if start_millis >= plan.duration_millis {
        return Err(ApiError::NotFound);
    }
    fs::create_dir(&directory)
        .await
        .map_err(|_| ApiError::Unavailable)?;
    let opened = secure_path::open_media(plan.media.clone()).await?;
    let sandbox = MediaChildSandbox::prepare_bounded(
        plan.ffmpeg.clone(),
        opened.file,
        Some(directory.clone()),
        secure_path::filesystem_permit()?,
    )
    .await?;
    let input_fd = sandbox.input_fd().to_string();
    let mut command = Command::new(sandbox.executable());
    apply_child_limits(
        &mut command,
        BATCH_TIMEOUT.as_secs(),
        2 * 1024 * 1024 * 1024,
        MAX_SEGMENT_BYTES,
        Some(sandbox),
    );
    command
        .env_clear()
        .current_dir(&directory)
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .args([
            "-nostdin",
            "-hide_banner",
            "-loglevel",
            "error",
            "-threads",
            "2",
            "-filter_threads",
            "1",
            "-protocol_whitelist",
            "fd",
            "-fd",
        ])
        .arg(input_fd)
        .args([
            "-probesize",
            "8000000",
            "-analyzeduration",
            "8000000",
            "-ss",
        ])
        .arg(format!("{:.3}", start_millis as f64 / 1000.0))
        .arg("-f")
        .arg(&plan.demuxer)
        .args(["-i", "fd:"]);
    if let Some(index) = plan.video_index {
        command.arg("-map").arg(format!("0:{index}"))
            .args(["-c:v", "libx264", "-preset", "veryfast", "-tune", "zerolatency", "-profile:v", "baseline", "-level:v", "3.1", "-pix_fmt", "yuv420p", "-vf"])
            .arg("scale='min(1280,iw)':'min(720,ih)':force_original_aspect_ratio=decrease:force_divisible_by=2")
            .args(["-r", "30", "-fps_mode", "cfr", "-g", "120", "-keyint_min", "120", "-sc_threshold", "0", "-b:v"])
            .arg(plan.video_bitrate.to_string()).arg("-maxrate").arg(plan.video_bitrate.to_string())
            .arg("-bufsize").arg(plan.video_bitrate.saturating_mul(2).to_string()).args(["-threads:v", "2"]);
    } else {
        command.arg("-vn");
    }
    if let Some(index) = plan.audio_index {
        command
            .arg("-map")
            .arg(format!("0:{index}"))
            .args(["-c:a", "aac", "-b:a"])
            .arg(plan.audio_bitrate.to_string())
            .arg("-ac")
            .arg(plan.output_channels.to_string())
            .args(["-ar", "48000"]);
    } else {
        command.arg("-an");
    }
    let duration_millis =
        (plan.duration_millis - start_millis).min(BATCH_SEGMENTS as u64 * SEGMENT_MILLIS);
    command
        .args(["-sn", "-dn", "-t"])
        .arg(format!("{:.3}", duration_millis as f64 / 1000.0))
        .arg("-output_ts_offset")
        .arg(format!("{:.3}", start_millis as f64 / 1000.0))
        .args([
            "-f",
            "hls",
            "-hls_time",
            "4",
            "-hls_list_size",
            "0",
            "-hls_playlist_type",
            "vod",
            "-start_number",
        ])
        .arg(first_segment.to_string())
        .args([
            "-muxdelay",
            "0",
            "-muxpreload",
            "0",
            "-hls_segment_options",
            // Each batch starts a new muxer and resets transport packet counters.
            "mpegts_copyts=1:mpegts_flags=+initial_discontinuity",
            "-hls_flags",
            "independent_segments+temp_file",
            "-hls_segment_filename",
        ])
        .arg(directory.join("segment%06d.ts"))
        .arg(directory.join("stream.m3u8"));
    Ok(command)
}

#[cfg(test)]
mod tests {
    use super::{MediaKind, playlist_for_duration};
    use uuid::Uuid;

    #[test]
    fn vod_playlist_preserves_duration_tail_and_authenticated_child_urls() {
        let item = Uuid::new_v4();
        let session = Uuid::new_v4();
        let body = playlist_for_duration(
            12_345,
            item,
            session,
            MediaKind::Video,
            Some("token+/=value"),
        );
        assert!(body.contains("#EXT-X-PLAYLIST-TYPE:VOD\n"));
        assert!(body.contains("#EXT-X-MEDIA-SEQUENCE:0\n"));
        assert!(body.ends_with("#EXT-X-ENDLIST\n"));
        let durations = body
            .lines()
            .filter_map(|line| line.strip_prefix("#EXTINF:"))
            .map(|line| line.trim_end_matches(',').parse::<f64>().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(durations, [4.0, 4.0, 4.0, 0.345]);
        for (index, url) in body
            .lines()
            .filter(|line| !line.starts_with('#'))
            .enumerate()
        {
            assert_eq!(
                url,
                format!(
                    "/Videos/{item}/hls/{session}/segment{index:06}.ts?ApiKey=token%2B%2F%3Dvalue"
                )
            );
        }
        let audio = playlist_for_duration(4001, item, session, MediaKind::Audio, None);
        assert!(audio.contains(&format!("/Audio/{item}/hls/{session}/segment000001.ts")));
        assert!(!audio.contains("ApiKey"));
    }
}
