//! Bounded subtitle discovery, extraction, and plain-text cue delivery.

use std::{
    io::{self, Read},
    path::{Path, PathBuf},
    sync::{Arc, OnceLock},
    time::Duration,
};

use axum::{
    body::Body,
    extract::{Path as AxumPath, State},
    http::{
        HeaderValue, StatusCode,
        header::{CACHE_CONTROL, CONTENT_DISPOSITION, CONTENT_LENGTH, CONTENT_TYPE},
    },
    response::Response,
};
use tokio::{
    process::Command,
    sync::{OwnedSemaphorePermit, Semaphore, oneshot},
};
use uuid::Uuid;

use crate::{ApiError, auth::MediaUser, state::AppState};

use super::{
    authorized_media, is_video_type, probe,
    process_limits::{MediaChildSandbox, apply_child_limits},
    secure_path::ResolvedMedia,
};

const MAX_SUBTITLE_BYTES: usize = 16 * 1024 * 1024;
const MAX_SUBTITLE_OUTPUT_BYTES: usize = 32 * 1024 * 1024;
const MAX_SIDECAR_TRACKS: usize = 64;
const MAX_SIDECAR_DIRECTORY_ENTRIES: usize = 8192;
const MAX_SUBTITLE_LINES: usize = 250_000;
const MAX_CUES: usize = 100_000;
const MAX_CUE_LINES: usize = 256;
const MAX_CUE_LINE_CHARS: usize = 16_384;
const MAX_BLOCKING_SUBTITLE_WORK: usize = 8;

#[derive(Clone, Debug)]
pub(super) struct SidecarSubtitle {
    pub index: u32,
    pub relative_path: PathBuf,
    pub format: String,
    pub language: Option<String>,
    pub title: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum OutputFormat {
    WebVtt,
    SubRip,
}

pub(super) async fn list_sidecars(
    media: &ResolvedMedia,
    after_index: Option<u32>,
) -> Result<Vec<SidecarSubtitle>, ApiError> {
    static PERMITS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    let permit = PERMITS
        .get_or_init(|| Arc::new(Semaphore::new(4)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::RateLimited)?;
    let media = media.clone();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let stem = media
            .relative
            .file_stem()
            .and_then(|value| value.to_str())
            .ok_or(ApiError::NotFound)?;
        let parent = media
            .relative
            .parent()
            .filter(|path| !path.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let directory = media
            .open_relative_dir(parent)
            .map_err(|_| ApiError::NotFound)?;
        let entries = directory.read_dir(".").map_err(|_| ApiError::NotFound)?;
        let prefix = format!("{stem}.");
        let mut found = Vec::<(String, PathBuf, String, Option<String>)>::new();
        for (examined, entry) in entries.enumerate() {
            if examined >= MAX_SIDECAR_DIRECTORY_ENTRIES {
                return Err(ApiError::Unavailable);
            }
            let Ok(entry) = entry else { continue };
            let Some(name) = entry.file_name().into_string().ok() else {
                continue;
            };
            if !name.starts_with(&prefix) {
                continue;
            }
            let extension = Path::new(&name)
                .extension()
                .and_then(|value| value.to_str())
                .map(str::to_ascii_lowercase);
            let Some(format) = extension.filter(|value| matches!(value.as_str(), "srt" | "vtt"))
            else {
                continue;
            };
            let relative = parent.join(&name);
            let Ok(file) = media.open_relative_file(&relative) else {
                continue;
            };
            let Ok(metadata) = file.metadata() else {
                continue;
            };
            if !metadata.is_file()
                || metadata.len() == 0
                || metadata.len() > MAX_SUBTITLE_BYTES as u64
            {
                continue;
            }
            if found.len() >= MAX_SIDECAR_TRACKS {
                return Err(ApiError::Unavailable);
            }
            let suffix = name.strip_prefix(&prefix).unwrap_or_default();
            let language_part = suffix
                .rsplit_once('.')
                .map(|(label, _)| label)
                .unwrap_or_default();
            let language = (!language_part.is_empty()
                && language_part.len() <= 35
                && language_part
                    .bytes()
                    .all(|byte| byte.is_ascii_alphanumeric() || b"-_".contains(&byte)))
            .then(|| language_part.to_owned());
            found.push((name, relative, format, language));
        }
        found.sort_by(|left, right| left.0.cmp(&right.0));
        if found.is_empty() {
            return Ok(Vec::new());
        }
        let index_start = after_index
            .map_or(Some(0), |value| value.checked_add(1))
            .ok_or(ApiError::Unavailable)?;
        found
            .into_iter()
            .enumerate()
            .map(|(offset, (_name, relative_path, format, language))| {
                let index = index_start
                    .checked_add(u32::try_from(offset).map_err(|_| ApiError::Unavailable)?)
                    .ok_or(ApiError::Unavailable)?;
                let title = language.clone().unwrap_or_else(|| "External".to_owned());
                let track = SidecarSubtitle {
                    index,
                    relative_path,
                    format,
                    language,
                    title,
                };
                Ok(track)
            })
            .collect::<Result<Vec<_>, ApiError>>()
    })
    .await
    .map_err(|_| ApiError::Unavailable)?
}

pub(super) async fn read_sidecar_vtt(
    media: &ResolvedMedia,
    sidecar: &SidecarSubtitle,
    start_time_ticks: Option<i64>,
) -> Result<Vec<u8>, ApiError> {
    let bytes = read_sidecar(media, sidecar).await?;
    sanitize_bounded(bytes, OutputFormat::WebVtt, start_time_ticks).await
}

pub(super) async fn sanitize_vtt_bytes(
    bytes: Vec<u8>,
    start_time_ticks: Option<i64>,
) -> Result<Vec<u8>, ApiError> {
    sanitize_bounded(bytes, OutputFormat::WebVtt, start_time_ticks).await
}

async fn read_sidecar(
    media: &ResolvedMedia,
    sidecar: &SidecarSubtitle,
) -> Result<Vec<u8>, ApiError> {
    static PERMITS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    let permit = PERMITS
        .get_or_init(|| Arc::new(Semaphore::new(8)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::RateLimited)?;
    let media = media.clone();
    let sidecar = sidecar.clone();
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        let file = media
            .open_relative_file(&sidecar.relative_path)
            .map_err(|_| ApiError::NotFound)?;
        let metadata = file.metadata().map_err(|_| ApiError::NotFound)?;
        if !metadata.is_file() || metadata.len() > MAX_SUBTITLE_BYTES as u64 {
            return Err(ApiError::NotFound);
        }
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.into_std()
            .take((MAX_SUBTITLE_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| ApiError::Unavailable)?;
        if bytes.len() > MAX_SUBTITLE_BYTES {
            return Err(ApiError::NotFound);
        }
        Ok(bytes)
    })
    .await
    .map_err(|_| ApiError::Unavailable)?
}

pub(super) async fn stream_vtt(
    State(state): State<AppState>,
    MediaUser(user): MediaUser,
    AxumPath((item_id, media_source_id, index)): AxumPath<(Uuid, Uuid, u32)>,
) -> Result<Response, ApiError> {
    stream_subtitle(
        state,
        user,
        item_id,
        media_source_id,
        index,
        None,
        OutputFormat::WebVtt,
    )
    .await
}

pub(super) async fn stream_vtt_at_position(
    State(state): State<AppState>,
    MediaUser(user): MediaUser,
    AxumPath((item_id, media_source_id, index, start_ticks)): AxumPath<(Uuid, Uuid, u32, i64)>,
) -> Result<Response, ApiError> {
    stream_subtitle(
        state,
        user,
        item_id,
        media_source_id,
        index,
        Some(start_ticks),
        OutputFormat::WebVtt,
    )
    .await
}

pub(super) async fn stream_srt(
    State(state): State<AppState>,
    MediaUser(user): MediaUser,
    AxumPath((item_id, media_source_id, index)): AxumPath<(Uuid, Uuid, u32)>,
) -> Result<Response, ApiError> {
    stream_subtitle(
        state,
        user,
        item_id,
        media_source_id,
        index,
        None,
        OutputFormat::SubRip,
    )
    .await
}

pub(super) async fn stream_srt_at_position(
    State(state): State<AppState>,
    MediaUser(user): MediaUser,
    AxumPath((item_id, media_source_id, index, start_ticks)): AxumPath<(Uuid, Uuid, u32, i64)>,
) -> Result<Response, ApiError> {
    stream_subtitle(
        state,
        user,
        item_id,
        media_source_id,
        index,
        Some(start_ticks),
        OutputFormat::SubRip,
    )
    .await
}

async fn stream_subtitle(
    state: AppState,
    user: crate::auth::UserRecord,
    item_id: Uuid,
    media_source_id: Uuid,
    index: u32,
    start_time_ticks: Option<i64>,
    output_format: OutputFormat,
) -> Result<Response, ApiError> {
    let admission = subtitle_request_permit()?;
    if item_id != media_source_id {
        return Err(ApiError::NotFound);
    }
    if start_time_ticks.is_some_and(|ticks| ticks < 0) {
        return Err(ApiError::BadRequest(
            "Start position cannot be negative".to_owned(),
        ));
    }
    let media = authorized_media(&state, &user, item_id).await?;
    if !is_video_type(&media.item.item_type) {
        return Err(ApiError::NotFound);
    }
    let probe = probe::probe(&state, &media)
        .await?
        .ok_or(ApiError::Unavailable)?;
    let last_index = probe.streams.iter().map(|stream| stream.index).max();
    let sidecars = list_sidecars(&media, last_index).await?;
    let (source_bytes, offset_ticks, admission) = if let Some(stream) = probe
        .streams
        .iter()
        .find(|stream| stream.index == index && stream.kind == "subtitle")
    {
        let (bytes, admission) =
            extract_embedded(&state, &media, stream, output_format, admission).await?;
        (bytes, start_time_ticks, admission)
    } else {
        let sidecar = sidecars
            .iter()
            .find(|track| track.index == index)
            .ok_or(ApiError::NotFound)?;
        (
            read_sidecar(&media, sidecar).await?,
            start_time_ticks,
            admission,
        )
    };
    let body = sanitize_bounded(source_bytes, output_format, offset_ticks).await?;
    drop(admission);
    if body.len() > MAX_SUBTITLE_OUTPUT_BYTES {
        return Err(ApiError::Unavailable);
    }
    subtitle_response(body, output_format)
}

pub(super) async fn extract_embedded(
    state: &AppState,
    media: &ResolvedMedia,
    stream: &probe::ProbedStream,
    output_format: OutputFormat,
    admission: OwnedSemaphorePermit,
) -> Result<(Vec<u8>, OwnedSemaphorePermit), ApiError> {
    if !matches!(
        stream.codec.as_deref(),
        Some("subrip" | "srt" | "ass" | "ssa" | "webvtt" | "mov_text" | "text" | "ttml")
    ) {
        return Err(ApiError::Unavailable);
    }
    let ffmpeg = state
        .config
        .ffmpeg_path
        .clone()
        .ok_or(ApiError::Unavailable)?;
    let (demuxer, _) = probe::allowed_demuxer(&media.absolute_path).ok_or(ApiError::Unavailable)?;
    let directory = state
        .config
        .data_dir
        .join("transcodes")
        .join(format!("subtitle-{}", Uuid::new_v4()));
    let (mut directory_guard, admission) = create_private_dir(directory.clone(), admission).await?;
    let file = super::secure_path::open_media(media.clone()).await?.file;
    let sandbox = MediaChildSandbox::prepare_bounded(
        ffmpeg.clone(),
        file,
        Some(directory.clone()),
        super::secure_path::filesystem_permit()?,
    )
    .await?;
    let input_fd = sandbox.input_fd().to_string();
    let output_path = directory.join(match output_format {
        OutputFormat::WebVtt => "subtitle.vtt",
        OutputFormat::SubRip => "subtitle.srt",
    });
    let mut command = Command::new(sandbox.executable());
    apply_child_limits(
        &mut command,
        45,
        1024 * 1024 * 1024,
        MAX_SUBTITLE_OUTPUT_BYTES as u64,
        Some(sandbox),
    );
    command
        .env_clear()
        .current_dir(&directory)
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .kill_on_drop(true)
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
        .arg(format!("0:{}", stream.index))
        .arg("-c:s")
        .arg(match output_format {
            OutputFormat::WebVtt => "webvtt",
            OutputFormat::SubRip => "srt",
        })
        .arg("-fs")
        .arg(MAX_SUBTITLE_OUTPUT_BYTES.to_string())
        .arg("-y")
        .arg(&output_path);
    let (status, admission) =
        super::hls::wait_for_child_with_admission(command, admission, Duration::from_secs(45))
            .await?;
    if !status.success() {
        return Err(ApiError::Unavailable);
    }
    let bytes = read_generated(&output_path).await?;
    directory_guard.remove().await;
    Ok((bytes, admission))
}

pub(super) fn subtitle_request_permit() -> Result<tokio::sync::OwnedSemaphorePermit, ApiError> {
    static PERMITS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    PERMITS
        .get_or_init(|| Arc::new(Semaphore::new(2)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::RateLimited)
}

async fn sanitize_bounded(
    bytes: Vec<u8>,
    format: OutputFormat,
    start_time_ticks: Option<i64>,
) -> Result<Vec<u8>, ApiError> {
    run_bounded_blocking(subtitle_work_permits(), move || {
        sanitize_subtitles(&bytes, format, start_time_ticks)
    })
    .await
}

async fn read_generated(path: &Path) -> Result<Vec<u8>, ApiError> {
    let path = path.to_path_buf();
    run_bounded_blocking(subtitle_work_permits(), move || {
        let metadata = std::fs::metadata(&path).map_err(|_| ApiError::Unavailable)?;
        if !metadata.is_file()
            || metadata.len() == 0
            || metadata.len() > MAX_SUBTITLE_OUTPUT_BYTES as u64
        {
            return Err(ApiError::Unavailable);
        }
        let file = std::fs::File::open(path).map_err(|_| ApiError::Unavailable)?;
        let mut bytes = Vec::with_capacity(metadata.len() as usize);
        file.take((MAX_SUBTITLE_OUTPUT_BYTES + 1) as u64)
            .read_to_end(&mut bytes)
            .map_err(|_| ApiError::Unavailable)?;
        if bytes.len() > MAX_SUBTITLE_OUTPUT_BYTES {
            return Err(ApiError::Unavailable);
        }
        Ok(bytes)
    })
    .await
}

async fn create_private_dir(
    path: PathBuf,
    admission: OwnedSemaphorePermit,
) -> Result<(PrivateDirectoryGuard, OwnedSemaphorePermit), ApiError> {
    let work_permit = subtitle_work_permits()
        .try_acquire_owned()
        .map_err(|_| ApiError::RateLimited)?;
    let cleanup_permit = super::secure_path::filesystem_permit()?;
    create_private_dir_with(path, work_permit, admission, cleanup_permit, |path| {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        #[cfg(unix)]
        {
            use std::os::unix::fs::DirBuilderExt;
            let mut builder = std::fs::DirBuilder::new();
            builder.mode(0o700).create(path)
        }
        #[cfg(not(unix))]
        {
            std::fs::create_dir(path)
        }
    })
    .await
}

async fn create_private_dir_with<F>(
    path: PathBuf,
    work_permit: OwnedSemaphorePermit,
    admission: OwnedSemaphorePermit,
    cleanup_permit: OwnedSemaphorePermit,
    create: F,
) -> Result<(PrivateDirectoryGuard, OwnedSemaphorePermit), ApiError>
where
    F: FnOnce(&Path) -> io::Result<()> + Send + 'static,
{
    let (created_tx, created_rx) = oneshot::channel();
    tokio::task::spawn_blocking(move || {
        let _work_permit = work_permit;
        let result = create(&path)
            .map(|()| (PrivateDirectoryGuard::new(path, cleanup_permit), admission))
            .map_err(|_| ApiError::Unavailable);
        if let Err(abandoned) = created_tx.send(result)
            && let Ok((directory_guard, admission)) = abandoned
        {
            directory_guard.cleanup_holding(admission);
        }
    });
    created_rx.await.map_err(|_| ApiError::Unavailable)?
}

fn subtitle_work_permits() -> Arc<Semaphore> {
    static PERMITS: OnceLock<Arc<Semaphore>> = OnceLock::new();
    PERMITS
        .get_or_init(|| Arc::new(Semaphore::new(MAX_BLOCKING_SUBTITLE_WORK)))
        .clone()
}

async fn run_bounded_blocking<T>(
    permits: Arc<Semaphore>,
    work: impl FnOnce() -> Result<T, ApiError> + Send + 'static,
) -> Result<T, ApiError>
where
    T: Send + 'static,
{
    let permit = permits
        .try_acquire_owned()
        .map_err(|_| ApiError::RateLimited)?;
    tokio::task::spawn_blocking(move || {
        let _permit = permit;
        work()
    })
    .await
    .map_err(|_| ApiError::Unavailable)?
}

struct PrivateDirectoryGuard(Option<(PathBuf, OwnedSemaphorePermit)>);

impl PrivateDirectoryGuard {
    fn new(path: PathBuf, cleanup_permit: OwnedSemaphorePermit) -> Self {
        Self(Some((path, cleanup_permit)))
    }

    fn cleanup_holding(mut self, admission: OwnedSemaphorePermit) {
        if let Some((path, cleanup_permit)) = self.0.take() {
            cleanup_directory(path, cleanup_permit, Some(admission));
        } else {
            drop(admission);
        }
    }

    async fn remove(&mut self) {
        if let Some((path, cleanup_permit)) = self.0.take() {
            let _ = tokio::task::spawn_blocking(move || {
                let _permit = cleanup_permit;
                std::fs::remove_dir_all(path)
            })
            .await;
        }
    }
}

impl Drop for PrivateDirectoryGuard {
    fn drop(&mut self) {
        if let Some((path, cleanup_permit)) = self.0.take() {
            cleanup_directory(path, cleanup_permit, None);
        }
    }
}

fn cleanup_directory(
    path: PathBuf,
    cleanup_permit: OwnedSemaphorePermit,
    admission: Option<OwnedSemaphorePermit>,
) {
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

#[derive(Clone, Debug)]
struct Cue {
    start_ms: u64,
    end_ms: u64,
    lines: Vec<String>,
}

fn sanitize_subtitles(
    bytes: &[u8],
    format: OutputFormat,
    start_time_ticks: Option<i64>,
) -> Result<Vec<u8>, ApiError> {
    if bytes.len() > MAX_SUBTITLE_OUTPUT_BYTES {
        return Err(ApiError::Unavailable);
    }
    let input = std::str::from_utf8(bytes).map_err(|_| ApiError::Unavailable)?;
    let input = input.strip_prefix('\u{feff}').unwrap_or(input);
    let offset_ms = start_time_ticks.unwrap_or_default().max(0) as u64 / 10_000;
    let mut cues = Vec::<Cue>::new();
    let mut current: Option<Cue> = None;
    for (line_number, line) in input.lines().enumerate() {
        if line_number >= MAX_SUBTITLE_LINES {
            return Err(ApiError::Unavailable);
        }
        let line = line.trim_end_matches('\r');
        if let Some((start, end)) = parse_timing(line) {
            if let Some(cue) = current.take() {
                push_cue(&mut cues, cue)?;
            }
            if end > offset_ms {
                current = Some(Cue {
                    start_ms: start.saturating_sub(offset_ms),
                    end_ms: end - offset_ms,
                    lines: Vec::new(),
                });
            }
        } else if line.is_empty() {
            if let Some(cue) = current.take() {
                push_cue(&mut cues, cue)?;
            }
        } else if let Some(cue) = current.as_mut() {
            if cue.lines.len() >= MAX_CUE_LINES {
                return Err(ApiError::Unavailable);
            }
            let text = line
                .chars()
                .filter(|ch| !ch.is_control() || *ch == '\t')
                .take(MAX_CUE_LINE_CHARS + 1)
                .collect::<String>();
            if text.chars().count() > MAX_CUE_LINE_CHARS {
                return Err(ApiError::Unavailable);
            }
            cue.lines.push(text);
        }
    }
    if let Some(cue) = current {
        push_cue(&mut cues, cue)?;
    }
    let mut output = String::with_capacity(bytes.len().min(MAX_SUBTITLE_OUTPUT_BYTES));
    match format {
        OutputFormat::WebVtt => output.push_str("WEBVTT\n\n"),
        OutputFormat::SubRip => {}
    }
    for (index, cue) in cues.into_iter().enumerate() {
        if cue.end_ms <= cue.start_ms {
            continue;
        }
        if format == OutputFormat::SubRip {
            output.push_str(&format!("{}\n", index.saturating_add(1)));
        }
        output.push_str(&format!(
            "{} --> {}\n",
            format_time(cue.start_ms, format),
            format_time(cue.end_ms, format)
        ));
        for line in cue.lines {
            escape_cue_text(&line, &mut output);
            output.push('\n');
        }
        output.push('\n');
        if output.len() > MAX_SUBTITLE_OUTPUT_BYTES {
            return Err(ApiError::Unavailable);
        }
    }
    Ok(output.into_bytes())
}

fn push_cue(cues: &mut Vec<Cue>, cue: Cue) -> Result<(), ApiError> {
    if cues.len() >= MAX_CUES {
        return Err(ApiError::Unavailable);
    }
    cues.push(cue);
    Ok(())
}

fn parse_timing(line: &str) -> Option<(u64, u64)> {
    let (start, rest) = line.split_once("-->")?;
    let start = parse_time(start.trim())?;
    let end = parse_time(rest.split_whitespace().next()?)?;
    (end >= start).then_some((start, end))
}

fn parse_time(value: &str) -> Option<u64> {
    let mut parts = value.split(':');
    let first = parts.next()?;
    let (hours, minutes, seconds) = match (parts.next(), parts.next(), parts.next()) {
        (Some(minutes), Some(seconds), None) => (
            first.parse::<u64>().ok()?,
            minutes.parse::<u64>().ok()?,
            seconds,
        ),
        (Some(seconds), None, None) => (0, first.parse::<u64>().ok()?, seconds),
        _ => return None,
    };
    if minutes >= 60 {
        return None;
    }
    let (seconds, millis) = seconds
        .split_once('.')
        .or_else(|| seconds.split_once(','))?;
    let seconds = seconds.parse::<u64>().ok()?;
    if seconds >= 60
        || millis.is_empty()
        || millis.len() > 3
        || !millis.bytes().all(|byte| byte.is_ascii_digit())
    {
        return None;
    }
    let millis = format!("{millis:0<3}").parse::<u64>().ok()?;
    hours
        .checked_mul(3_600_000)?
        .checked_add(minutes.checked_mul(60_000)?)?
        .checked_add(seconds.checked_mul(1000)?)?
        .checked_add(millis)
}

fn format_time(milliseconds: u64, format: OutputFormat) -> String {
    let hours = milliseconds / 3_600_000;
    let minutes = milliseconds / 60_000 % 60;
    let seconds = milliseconds / 1000 % 60;
    let millis = milliseconds % 1000;
    let separator = if format == OutputFormat::WebVtt {
        '.'
    } else {
        ','
    };
    format!("{hours:02}:{minutes:02}:{seconds:02}{separator}{millis:03}")
}

fn escape_cue_text(input: &str, output: &mut String) {
    for ch in input.chars() {
        match ch {
            '&' => output.push_str("&amp;"),
            '<' => output.push_str("&lt;"),
            '>' => output.push_str("&gt;"),
            _ => output.push(ch),
        }
    }
}

fn subtitle_response(bytes: Vec<u8>, format: OutputFormat) -> Result<Response, ApiError> {
    let content_type = match format {
        OutputFormat::WebVtt => "text/vtt; charset=utf-8",
        OutputFormat::SubRip => "application/x-subrip; charset=utf-8",
    };
    let length = bytes.len();
    let mut response = Response::new(Body::from(bytes));
    *response.status_mut() = StatusCode::OK;
    response
        .headers_mut()
        .insert(CONTENT_TYPE, HeaderValue::from_static(content_type));
    response.headers_mut().insert(
        CONTENT_LENGTH,
        HeaderValue::from_str(&length.to_string())
            .map_err(|_| ApiError::Internal("subtitle response headers invalid".to_owned()))?,
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
    if format == OutputFormat::SubRip {
        response.headers_mut().insert(
            CONTENT_DISPOSITION,
            HeaderValue::from_static("attachment; filename=\"subtitle.srt\""),
        );
    }
    Ok(response)
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_CUE_LINE_CHARS, MAX_CUE_LINES, MAX_SUBTITLE_LINES, OutputFormat,
        create_private_dir_with, format_time, list_sidecars, parse_time, read_sidecar_vtt,
        run_bounded_blocking, sanitize_subtitles,
    };
    use crate::{ApiError, library::ItemRecord, media_features::secure_path::ResolvedMedia};
    use chrono::Utc;
    use std::{fs, path::PathBuf, sync::Arc};
    use tokio::sync::{Semaphore, oneshot};
    use uuid::Uuid;

    #[tokio::test]
    async fn blocking_subtitle_admission_survives_request_cancellation() {
        let permits = Arc::new(Semaphore::new(1));
        let (started_tx, started_rx) = oneshot::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let (finished_tx, finished_rx) = oneshot::channel();
        let task = tokio::spawn(run_bounded_blocking(permits.clone(), move || {
            let _ = started_tx.send(());
            release_rx.recv().unwrap();
            let _ = finished_tx.send(());
            Ok(())
        }));

        started_rx.await.unwrap();
        task.abort();
        let _ = task.await;
        assert_eq!(permits.available_permits(), 0);
        assert!(permits.clone().try_acquire_owned().is_err());

        release_tx.send(()).unwrap();
        finished_rx.await.unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            while permits.available_permits() == 0 {
                tokio::task::yield_now().await;
            }
        })
        .await
        .unwrap();
        assert_eq!(permits.available_permits(), 1);
    }

    #[tokio::test]
    async fn cancelled_subtitle_directory_creation_keeps_admission_and_removes_orphan() {
        let path = std::env::temp_dir().join(format!("puffinbox-subtitle-dir-{}", Uuid::new_v4()));
        let admission = Arc::new(Semaphore::new(1));
        let work = Arc::new(Semaphore::new(1));
        let admission_permit = admission.clone().try_acquire_owned().unwrap();
        let work_permit = work.clone().try_acquire_owned().unwrap();
        let cleanup_permit = crate::media_features::secure_path::filesystem_permit().unwrap();
        let (entered_tx, entered_rx) = std::sync::mpsc::channel();
        let (release_tx, release_rx) = std::sync::mpsc::channel();
        let task = tokio::spawn(create_private_dir_with(
            path.clone(),
            work_permit,
            admission_permit,
            cleanup_permit,
            move |path| {
                entered_tx.send(()).unwrap();
                release_rx.recv().unwrap();
                std::fs::create_dir(path)
            },
        ));

        tokio::task::spawn_blocking(move || {
            entered_rx
                .recv_timeout(std::time::Duration::from_secs(2))
                .expect("subtitle directory worker entered its blocking section");
        })
        .await
        .unwrap();
        task.abort();
        assert!(admission.clone().try_acquire_owned().is_err());

        release_tx.send(()).unwrap();
        tokio::time::timeout(std::time::Duration::from_secs(2), async {
            loop {
                if !path.exists() && admission.clone().try_acquire_owned().is_ok() {
                    break;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("abandoned subtitle directory is removed before admission is released");
        assert!(!path.exists());
    }

    #[test]
    fn subtitle_payload_is_plain_text_and_offsets_are_applied() {
        let input = b"1\n00:00:02,000 --> 00:00:04,500\n<script>alert(1)</script> & safe\n";
        let output = sanitize_subtitles(input, OutputFormat::WebVtt, Some(10_000_000)).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(output.starts_with("WEBVTT\n\n00:00:01.000 --> 00:00:03.500\n"));
        assert!(output.contains("&lt;script&gt;alert(1)&lt;/script&gt; &amp; safe"));
        assert!(!output.contains("<script>"));
    }

    #[test]
    fn subtitle_offset_drops_ended_fixture_cue_and_shifts_next_cue() {
        let input = b"7\n00:01:38,000 --> 00:01:42,000\nEnglish timed acceptance cue 07\n\n8\n00:01:53,000 --> 00:01:57,000\nEnglish timed acceptance cue 08\n";
        let output = sanitize_subtitles(input, OutputFormat::SubRip, Some(1_030_000_000)).unwrap();
        let output = String::from_utf8(output).unwrap();
        assert!(!output.contains("English timed acceptance cue 07"));
        assert!(
            output.contains("1\n00:00:10,000 --> 00:00:14,000\nEnglish timed acceptance cue 08")
        );
    }

    #[test]
    fn subtitle_parser_rejects_invalid_times_and_preserves_srt_output_shape() {
        assert_eq!(parse_time("01:02.5"), Some(62_500));
        assert_eq!(parse_time("00:99:00,000"), None);
        assert_eq!(format_time(62_500, OutputFormat::SubRip), "00:01:02,500");
        let output = sanitize_subtitles(
            b"00:00:01.000 --> 00:00:02.000\nHello\n",
            OutputFormat::SubRip,
            None,
        )
        .unwrap();
        assert!(
            String::from_utf8(output)
                .unwrap()
                .contains("1\n00:00:01,000 --> 00:00:02,000\nHello")
        );
    }

    #[test]
    fn subtitle_limits_report_oversized_cues_instead_of_truncating() {
        let mut too_many_lines = "WEBVTT\n\n".to_owned();
        too_many_lines.push_str(&"\n".repeat(MAX_SUBTITLE_LINES + 1));
        assert!(sanitize_subtitles(too_many_lines.as_bytes(), OutputFormat::WebVtt, None).is_err());

        let mut too_many_cue_lines = "00:00:01.000 --> 00:00:02.000\n".to_owned();
        too_many_cue_lines.push_str(&"line\n".repeat(MAX_CUE_LINES + 1));
        assert!(
            sanitize_subtitles(too_many_cue_lines.as_bytes(), OutputFormat::WebVtt, None).is_err()
        );

        let overlong_line = format!(
            "00:00:01.000 --> 00:00:02.000\n{}\n",
            "x".repeat(MAX_CUE_LINE_CHARS + 1)
        );
        assert!(sanitize_subtitles(overlong_line.as_bytes(), OutputFormat::WebVtt, None).is_err());
    }

    #[tokio::test]
    async fn sidecar_discovery_is_basename_scoped_and_reads_through_root_capability() {
        let base = std::env::temp_dir().join(format!("puffinbox-sidecar-{}", Uuid::new_v4()));
        let root_path = base.join("library");
        fs::create_dir_all(&root_path).unwrap();
        let media_path = root_path.join("movie.mkv");
        fs::write(&media_path, b"not used by the sidecar reader").unwrap();
        fs::write(
            root_path.join("movie.en.srt"),
            "1\n00:00:01,000 --> 00:00:02,000\n<script>unsafe</script>\n",
        )
        .unwrap();
        fs::write(
            root_path.join("movie.vtt"),
            "WEBVTT\n\n00:00:03.000 --> 00:00:04.000\nsidecar\n",
        )
        .unwrap();
        fs::write(root_path.join("movie2.srt"), "not part of movie").unwrap();
        fs::write(root_path.join("movie.ass"), "unsupported extension").unwrap();
        let outside = base.join("outside.vtt");
        fs::write(
            &outside,
            "WEBVTT\n\n00:00:00.000 --> 00:00:01.000\noutside-secret\n",
        )
        .unwrap();
        #[cfg(unix)]
        std::os::unix::fs::symlink(&outside, root_path.join("movie.escape.vtt")).unwrap();

        let canonical_root = fs::canonicalize(&root_path).unwrap();
        let media = ResolvedMedia {
            item: ItemRecord {
                id: Uuid::new_v4(),
                library_id: Uuid::new_v4(),
                parent_id: None,
                name: "movie.mkv".to_owned(),
                sort_name: "movie".to_owned(),
                item_type: "Movie".to_owned(),
                path: media_path.clone(),
                container: Some("mkv".to_owned()),
                size_bytes: None,
                runtime_ticks: None,
                date_added: Utc::now(),
                date_modified: None,
                rating: None,
                overview: None,
                metadata_json: serde_json::json!({}),
            },
            parent_directory: None,
            root: Arc::new(
                cap_std::fs::Dir::open_ambient_dir(&canonical_root, cap_std::ambient_authority())
                    .unwrap(),
            ),
            relative: PathBuf::from("movie.mkv"),
            absolute_path: fs::canonicalize(&media_path).unwrap(),
            catalog_identity_matches: false,
        };
        let tracks = list_sidecars(&media, Some(0)).await.unwrap();
        assert_eq!(
            tracks.len(),
            2,
            "wrong basenames, unsupported types, and escaped symlinks are ignored"
        );
        assert_eq!(tracks[0].index, 1);
        assert_eq!(tracks[0].language.as_deref(), Some("en"));
        let vtt = read_sidecar_vtt(&media, &tracks[0], None).await.unwrap();
        let vtt = String::from_utf8(vtt).unwrap();
        assert!(vtt.contains("&lt;script&gt;unsafe&lt;/script&gt;"));
        assert!(!vtt.contains("outside-secret"));
        let _ = fs::remove_dir_all(base);
    }

    #[tokio::test]
    async fn sidecar_indices_are_deterministic_and_track_overflow_is_an_error() {
        let base = std::env::temp_dir().join(format!("puffinbox-sidecar-limit-{}", Uuid::new_v4()));
        let root_path = base.join("library");
        fs::create_dir_all(&root_path).unwrap();
        let media_path = root_path.join("movie.mkv");
        fs::write(&media_path, b"media").unwrap();
        for index in (0..65).rev() {
            fs::write(
                root_path.join(format!("movie.{index:02}.srt")),
                "1\n00:00:01,000 --> 00:00:02,000\ntext\n",
            )
            .unwrap();
        }
        let canonical_root = fs::canonicalize(&root_path).unwrap();
        let media = ResolvedMedia {
            item: ItemRecord {
                id: Uuid::new_v4(),
                library_id: Uuid::new_v4(),
                parent_id: None,
                name: "movie.mkv".to_owned(),
                sort_name: "movie".to_owned(),
                item_type: "Movie".to_owned(),
                path: media_path.clone(),
                container: Some("mkv".to_owned()),
                size_bytes: None,
                runtime_ticks: None,
                date_added: Utc::now(),
                date_modified: None,
                rating: None,
                overview: None,
                metadata_json: serde_json::json!({}),
            },
            parent_directory: None,
            root: Arc::new(
                cap_std::fs::Dir::open_ambient_dir(&canonical_root, cap_std::ambient_authority())
                    .unwrap(),
            ),
            relative: PathBuf::from("movie.mkv"),
            absolute_path: fs::canonicalize(&media_path).unwrap(),
            catalog_identity_matches: false,
        };
        assert!(matches!(
            list_sidecars(&media, None).await,
            Err(ApiError::Unavailable)
        ));
        fs::remove_file(root_path.join("movie.64.srt")).unwrap();
        let tracks = list_sidecars(&media, None).await.unwrap();
        assert_eq!(tracks.len(), 64);
        assert_eq!(tracks[0].index, 0);
        assert_eq!(tracks[0].relative_path.file_name().unwrap(), "movie.00.srt");
        assert_eq!(tracks[63].index, 63);
        let _ = fs::remove_dir_all(base);
    }
}
