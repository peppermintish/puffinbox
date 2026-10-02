//! Bounded ffprobe invocation for user-controlled media.

use std::{
    path::{Path, PathBuf},
    process::Stdio,
    time::Duration,
};

use serde::Deserialize;
use tokio::{
    io::{AsyncRead, AsyncReadExt},
    process::Command,
    sync::{OwnedSemaphorePermit, Semaphore},
    time::timeout,
};

use crate::{ApiError, db, state::AppState};

use super::process_limits::{MediaChildSandbox, apply_child_limits};
use super::secure_path::{self, ResolvedMedia};

const MAX_PROBE_OUTPUT: usize = 1024 * 1024;
const PROBE_TIMEOUT: Duration = Duration::from_secs(12);
const MAX_STREAMS: usize = 256;

#[derive(Clone, Debug)]
pub(super) struct ProbeInfo {
    pub format_names: Vec<String>,
    pub duration_seconds: Option<f64>,
    pub bit_rate: Option<u64>,
    pub streams: Vec<ProbedStream>,
    /// True only when the descriptor opened for this probe has the size and
    /// modification time recorded by the catalog.
    pub catalog_identity_matches: bool,
}

#[derive(Clone, Debug)]
pub(super) struct ProbedStream {
    pub index: u32,
    pub kind: String,
    pub codec: Option<String>,
    pub profile: Option<String>,
    /// Set only for an explicitly signalled range that we can classify.
    pub video_range_type: Option<&'static str>,
    pub language: Option<String>,
    pub title: Option<String>,
    pub is_default: bool,
    pub is_forced: bool,
    pub width: Option<u32>,
    pub height: Option<u32>,
    pub channels: Option<u32>,
    pub sample_rate: Option<u32>,
    pub bit_depth: Option<u32>,
    pub bit_rate: Option<u64>,
}

pub(super) fn default_stream<'a>(
    streams: &'a [ProbedStream],
    kind: &str,
) -> Option<&'a ProbedStream> {
    streams
        .iter()
        .find(|stream| stream.kind == kind && stream.is_default)
        .or_else(|| streams.iter().find(|stream| stream.kind == kind))
}

#[derive(Deserialize)]
struct RawProbe {
    #[serde(default)]
    streams: Vec<RawStream>,
    format: Option<RawFormat>,
}

#[derive(Deserialize)]
struct RawStream {
    index: Option<i64>,
    codec_type: Option<String>,
    codec_name: Option<String>,
    profile: Option<String>,
    color_transfer: Option<String>,
    #[serde(default)]
    side_data_list: Vec<RawSideData>,
    width: Option<u32>,
    height: Option<u32>,
    channels: Option<u32>,
    sample_rate: Option<String>,
    bits_per_raw_sample: Option<String>,
    bits_per_sample: Option<u32>,
    bit_rate: Option<String>,
    tags: Option<std::collections::HashMap<String, String>>,
    disposition: Option<RawDisposition>,
}

#[derive(Deserialize)]
struct RawDisposition {
    default: Option<i32>,
    forced: Option<i32>,
    attached_pic: Option<i32>,
}

fn audio_bit_depth(stream: &RawStream) -> Option<u32> {
    if stream.codec_type.as_deref() != Some("audio") {
        return None;
    }
    let valid = |depth: u32| (1..=64).contains(&depth);
    match stream.bits_per_raw_sample.as_deref() {
        Some(value) if value != "0" && !value.is_empty() => {
            return value.parse().ok().filter(|depth| valid(*depth));
        }
        _ => {}
    }
    // Coded bits can describe compressed codewords rather than sample
    // precision. Only PCM can use that value when raw precision is absent.
    stream
        .codec_name
        .as_deref()
        .filter(|codec| codec.starts_with("pcm_"))?;
    stream.bits_per_sample.filter(|depth| valid(*depth))
}

#[derive(Deserialize)]
struct RawSideData {
    side_data_type: Option<String>,
}

fn explicit_video_range(stream: &RawStream) -> Option<&'static str> {
    if stream.codec_type.as_deref() != Some("video")
        || stream
            .disposition
            .as_ref()
            .is_some_and(|disposition| disposition.attached_pic.unwrap_or_default() != 0)
    {
        return None;
    }
    // A transfer function alone cannot rule out dynamic HDR metadata. Accept
    // only unrelated side-data records we recognise; unknown records stay
    // unclassified instead of turning an HDR source into an SDR claim.
    if stream.side_data_list.iter().any(|data| {
        !matches!(
            data.side_data_type.as_deref(),
            Some("Display Matrix" | "Stereo 3D" | "Spherical Mapping" | "CPB properties")
        )
    }) {
        return None;
    }
    match stream.color_transfer.as_deref() {
        Some("bt709" | "bt470m" | "bt470bg" | "smpte170m" | "smpte240m" | "iec61966-2-1") => {
            Some("SDR")
        }
        _ => None,
    }
}

#[derive(Deserialize)]
struct RawFormat {
    format_name: Option<String>,
    duration: Option<String>,
    bit_rate: Option<String>,
}

/// Probe a file only when its extension maps to a concrete, non-playlist
/// demuxer. The format is forced, remote protocols are denied, process output
/// and runtime are capped, and the input is passed through a fixed inherited
/// descriptor instead of a mutable pathname or procfs path.
pub(super) async fn probe(
    state: &AppState,
    media: &ResolvedMedia,
) -> Result<Option<ProbeInfo>, ApiError> {
    static PROBE_PERMITS: std::sync::OnceLock<std::sync::Arc<Semaphore>> =
        std::sync::OnceLock::new();
    let permit = PROBE_PERMITS
        .get_or_init(|| std::sync::Arc::new(Semaphore::new(4)))
        .clone()
        .try_acquire_owned()
        .map_err(|_| ApiError::RateLimited)?;
    let Some((demuxer, extension)) = allowed_demuxer(&media.absolute_path) else {
        return Ok(None);
    };
    let Some(program) = ffprobe_path(state) else {
        return Ok(None);
    };
    let item_id = media.item.id;
    let filesystem_permit = secure_path::filesystem_permit()?;
    let media = media.clone();
    let opened = tokio::task::spawn_blocking(move || {
        let _filesystem_permit = filesystem_permit;
        let opened = secure_path::open_in_blocking(&media).ok()?;
        let size = i64::try_from(opened.size).ok();
        let modified = opened.modified_utc;
        Some((opened, size, modified, permit))
    })
    .await
    .map_err(|_| ApiError::Unavailable)?;
    let Some((opened, size, modified, permit)) = opened else {
        return Ok(None);
    };
    let result = run_ffprobe(
        program,
        opened.file,
        demuxer,
        &extension,
        opened.catalog_identity_matches,
        permit,
    )
    .await?;
    if let Some(info) = result.as_ref()
        && info.catalog_identity_matches
        && let Some(ticks) = info.duration_seconds.and_then(duration_to_ticks)
    {
        db::set_item_runtime_ticks(&state.db, state.run_id, item_id, size, modified, ticks).await?;
    }
    Ok(result)
}

async fn run_ffprobe(
    program: PathBuf,
    file: std::fs::File,
    demuxer: &str,
    extension: &str,
    catalog_identity_matches: bool,
    probe_permit: OwnedSemaphorePermit,
) -> Result<Option<ProbeInfo>, ApiError> {
    let sandbox =
        MediaChildSandbox::prepare_bounded(program, file, None, secure_path::filesystem_permit()?)
            .await?;
    let input_fd = sandbox.input_fd().to_string();
    let mut command = Command::new(sandbox.executable());
    apply_child_limits(&mut command, 12, 512 * 1024 * 1024, 0, Some(sandbox));
    command
        .env_clear()
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .kill_on_drop(true)
        .arg("-v")
        .arg("error")
        .arg("-protocol_whitelist")
        .arg("fd")
        .arg("-fd")
        .arg(&input_fd)
        .arg("-probesize")
        .arg("8000000")
        .arg("-analyzeduration")
        .arg("8000000")
        .arg("-show_format")
        .arg("-show_streams")
        .arg("-of")
        .arg("json")
        .arg("-f")
        .arg(demuxer)
        .arg("-i")
        .arg("fd:");

    let (mut child_task, mut cancel_guard) = launch_probe_child(command, probe_permit);
    let output = match timeout(PROBE_TIMEOUT, &mut child_task).await {
        Ok(Ok(output)) => {
            cancel_guard.disarm();
            output
        }
        Ok(Err(_)) => {
            cancel_guard.disarm();
            None
        }
        Err(_) => {
            cancel_guard.cancel();
            let _ = timeout(Duration::from_secs(2), &mut child_task).await;
            None
        }
    };
    let Some(mut result) = output.and_then(|output| parse_output(&output, extension)) else {
        return Ok(None);
    };

    result.catalog_identity_matches = catalog_identity_matches;
    if !catalog_identity_matches {
        // A probe may describe the newly opened bytes, but it must not be
        // presented or cached as the catalog item's known runtime when the
        // descriptor no longer matches the catalog identity.
        result.duration_seconds = None;
    }

    Ok(Some(result))
}

type ProbeChildTask = tokio::task::JoinHandle<Option<Vec<u8>>>;

struct ProbeChildCancellation {
    sender: Option<tokio::sync::oneshot::Sender<()>>,
}

impl ProbeChildCancellation {
    fn cancel(&mut self) {
        if let Some(sender) = self.sender.take() {
            let _ = sender.send(());
        }
    }

    fn disarm(&mut self) {
        self.sender.take();
    }
}

impl Drop for ProbeChildCancellation {
    fn drop(&mut self) {
        self.cancel();
    }
}

fn launch_probe_child(
    command: Command,
    permit: OwnedSemaphorePermit,
) -> (ProbeChildTask, ProbeChildCancellation) {
    launch_probe_child_gated(command, permit, None)
}

#[cfg(test)]
struct ProbeStartGate {
    ready: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
}

#[cfg(test)]
fn launch_probe_child_with_start_gate(
    command: Command,
    permit: OwnedSemaphorePermit,
    ready: tokio::sync::oneshot::Sender<()>,
    release: tokio::sync::oneshot::Receiver<()>,
) -> (ProbeChildTask, ProbeChildCancellation) {
    launch_probe_child_gated(command, permit, Some(ProbeStartGate { ready, release }))
}

fn launch_probe_child_gated(
    mut command: Command,
    permit: OwnedSemaphorePermit,
    #[cfg(test)] start_gate: Option<ProbeStartGate>,
    #[cfg(not(test))] _start_gate: Option<()>,
) -> (ProbeChildTask, ProbeChildCancellation) {
    let (cancel_sender, mut cancel_receiver) = tokio::sync::oneshot::channel();
    let task = tokio::spawn(async move {
        let _permit = permit;
        #[cfg(test)]
        if let Some(start_gate) = start_gate {
            let _ = start_gate.ready.send(());
            if start_gate.release.await.is_err() || cancel_receiver.try_recv().is_ok() {
                return None;
            }
        }
        let mut child = command.spawn().ok()?;
        drop(command);
        let mut stdout = child.stdout.take()?;
        let mut output = Vec::with_capacity(64 * 1024);
        let read_result = tokio::select! {
            _ = &mut cancel_receiver => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return None;
            }
            result = read_limited(&mut stdout, &mut output, MAX_PROBE_OUTPUT + 1) => result,
        };
        if read_result.is_err() || output.len() > MAX_PROBE_OUTPUT {
            let _ = child.kill().await;
            let _ = child.wait().await;
            return None;
        }
        let status = tokio::select! {
            _ = &mut cancel_receiver => {
                let _ = child.kill().await;
                let _ = child.wait().await;
                return None;
            }
            status = child.wait() => status.ok()?,
        };
        status.success().then_some(output)
    });
    (
        task,
        ProbeChildCancellation {
            sender: Some(cancel_sender),
        },
    )
}

async fn read_limited<R: AsyncRead + Unpin>(
    reader: &mut R,
    output: &mut Vec<u8>,
    limit: usize,
) -> std::io::Result<()> {
    let mut limited = reader.take(limit as u64);
    limited.read_to_end(output).await.map(|_| ())
}

fn ffprobe_path(state: &AppState) -> Option<PathBuf> {
    if let Some(ffmpeg) = state.config.ffmpeg_path.as_ref() {
        let parent = ffmpeg.parent()?;
        #[cfg(windows)]
        let name = "ffprobe.exe";
        #[cfg(not(windows))]
        let name = "ffprobe";
        let candidate = parent.join(name);
        if candidate.is_file() {
            return Some(candidate);
        }
        return None;
    }
    // When FFmpeg was not explicitly configured, the external ffprobe command
    // may still be present in PATH. No bundled binaries are used.
    Some(PathBuf::from("ffprobe"))
}

pub(super) fn allowed_demuxer(path: &Path) -> Option<(&'static str, String)> {
    let extension = path.extension()?.to_str()?.to_ascii_lowercase();
    let demuxer = match extension.as_str() {
        "mkv" | "mka" | "webm" => "matroska",
        "mp4" | "m4v" | "mov" | "m4a" | "3gp" => "mov",
        "avi" => "avi",
        "mpeg" | "mpg" | "vob" => "mpeg",
        "ts" | "m2ts" => "mpegts",
        "mp3" => "mp3",
        "aac" => "aac",
        "flac" => "flac",
        "wav" => "wav",
        "aiff" | "aif" => "aiff",
        "ogg" | "oga" | "opus" => "ogg",
        // Playlists, manifests, archives and unknown formats are deliberately
        // not auto-detected: they could contain nested local or remote inputs.
        _ => return None,
    };
    Some((demuxer, extension))
}

fn parse_output(output: &[u8], extension: &str) -> Option<ProbeInfo> {
    let raw: RawProbe = serde_json::from_slice(output).ok()?;
    if raw.streams.len() > MAX_STREAMS {
        return None;
    }
    let mut streams = Vec::with_capacity(raw.streams.len());
    for stream in raw.streams {
        let video_range_type = explicit_video_range(&stream);
        let bit_depth = audio_bit_depth(&stream);
        let index = u32::try_from(stream.index?).ok()?;
        let raw_kind = stream.codec_type?.to_ascii_lowercase();
        let disposition = stream.disposition.unwrap_or(RawDisposition {
            default: None,
            forced: None,
            attached_pic: None,
        });
        let kind = if raw_kind == "video" && disposition.attached_pic.unwrap_or_default() != 0 {
            "embedded_image".to_owned()
        } else {
            raw_kind
        };
        if !matches!(
            kind.as_str(),
            "video" | "audio" | "subtitle" | "attachment" | "data" | "lyric" | "embedded_image"
        ) {
            continue;
        }
        let tags = stream.tags.unwrap_or_default();
        let find_tag = |wanted: &str| -> Option<String> {
            tags.iter()
                .find(|(key, _)| key.eq_ignore_ascii_case(wanted))
                .map(|(_, value)| value.clone())
        };
        streams.push(ProbedStream {
            index,
            kind,
            codec: stream.codec_name.map(|value| value.to_ascii_lowercase()),
            profile: stream.profile,
            video_range_type,
            language: find_tag("language"),
            title: find_tag("title"),
            is_default: disposition.default.unwrap_or_default() != 0,
            is_forced: disposition.forced.unwrap_or_default() != 0,
            width: stream.width,
            height: stream.height,
            channels: stream.channels,
            sample_rate: stream
                .sample_rate
                .and_then(|v| v.parse().ok())
                .filter(|rate| *rate > 0 && *rate <= i32::MAX as u32),
            bit_depth,
            bit_rate: stream.bit_rate.and_then(|v| v.parse().ok()),
        });
    }
    let format = raw.format;
    let format_names = format
        .as_ref()
        .and_then(|value| value.format_name.as_deref())
        .map(|names| names.split(',').map(str::to_ascii_lowercase).collect())
        .unwrap_or_else(|| vec![extension.to_owned()]);
    let duration_seconds = format
        .as_ref()
        .and_then(|value| value.duration.as_deref())
        .and_then(|value| value.parse::<f64>().ok())
        .filter(|value| value.is_finite() && *value >= 0.0);
    let bit_rate = format
        .as_ref()
        .and_then(|value| value.bit_rate.as_deref())
        .and_then(|value| value.parse().ok());
    Some(ProbeInfo {
        format_names,
        duration_seconds,
        bit_rate,
        streams,
        catalog_identity_matches: false,
    })
}

fn duration_to_ticks(seconds: f64) -> Option<i64> {
    let ticks = (seconds * 10_000_000.0).round();
    (ticks.is_finite() && ticks >= 0.0 && ticks <= i64::MAX as f64).then_some(ticks as i64)
}

#[cfg(test)]
mod tests {
    use super::{
        MediaChildSandbox, Semaphore, allowed_demuxer, apply_child_limits, launch_probe_child,
        launch_probe_child_with_start_gate, parse_output, run_ffprobe,
    };
    use std::{fs, os::fd::AsRawFd, path::Path, process::Stdio, time::Duration};

    use tokio::{
        net::TcpListener,
        process::Command,
        time::{sleep, timeout},
    };

    #[test]
    fn only_known_non_playlist_demuxers_are_allowed() {
        assert_eq!(
            allowed_demuxer(Path::new("movie.mp4")).map(|(name, _)| name),
            Some("mov")
        );
        assert_eq!(
            allowed_demuxer(Path::new("track.flac")).map(|(name, _)| name),
            Some("flac")
        );
        for name in [
            "playlist.m3u8",
            "playlist.m3u",
            "feeds.mpd",
            "list.concat",
            "book.epub",
            "mystery.bin",
        ] {
            assert!(allowed_demuxer(Path::new(name)).is_none());
        }
    }

    #[test]
    fn parses_codec_streams_without_unbounded_or_untrusted_fields() {
        let parsed = parse_output(br#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264","width":1920,"height":1080,"tags":{"LANGUAGE":"en"},"disposition":{"default":1}}],"format":{"format_name":"mov,mp4","duration":"12.5","bit_rate":"1000000"}}"#, "mp4").unwrap();
        assert_eq!(parsed.format_names, vec!["mov", "mp4"]);
        assert_eq!(parsed.duration_seconds, Some(12.5));
        assert_eq!(parsed.streams[0].codec.as_deref(), Some("h264"));
        assert_eq!(parsed.streams[0].language.as_deref(), Some("en"));
        assert!(parsed.streams[0].is_default);
    }

    #[test]
    fn attached_picture_disposition_is_not_a_video_stream() {
        let parsed = parse_output(br#"{"streams":[{"index":0,"codec_type":"video","codec_name":"mjpeg","disposition":{"attached_pic":1}},{"index":1,"codec_type":"video","codec_name":"h264","width":640,"height":360,"disposition":{"attached_pic":0}}],"format":{"format_name":"mov,mp4"}}"#, "m4a").unwrap();
        assert_eq!(parsed.streams[0].kind, "embedded_image");
        assert_eq!(parsed.streams[1].kind, "video");
    }

    #[test]
    fn video_range_requires_explicit_sdr_signalling_without_hdr_side_data() {
        for (transfer, side_data, expected) in [
            ("bt709", None, Some("SDR")),
            ("smpte170m", Some("Display Matrix"), Some("SDR")),
            ("unknown", None, None),
            ("smpte2084", None, None),
            ("arib-std-b67", None, None),
            ("bt709", Some("DOVI configuration record"), None),
            ("bt709", Some("Mastering display metadata"), None),
            (
                "bt709",
                Some("HDR Dynamic Metadata SMPTE2094-40 (HDR10+)"),
                None,
            ),
            ("bt709", Some("Unrecognised future metadata"), None),
        ] {
            let wire = serde_json::json!({
                "streams": [{"index":0,"codec_type":"video","codec_name":"h264",
                    "color_transfer":transfer,
                    "side_data_list":side_data.map(|kind| vec![serde_json::json!({"side_data_type":kind})]).unwrap_or_default()}],
                "format":{"format_name":"mov,mp4"}
            });
            let parsed = parse_output(&serde_json::to_vec(&wire).unwrap(), "mp4").unwrap();
            assert_eq!(
                parsed.streams[0].video_range_type, expected,
                "{transfer} / {side_data:?}"
            );
        }
        let missing = parse_output(
            br#"{"streams":[{"index":0,"codec_type":"video","codec_name":"h264"}]}"#,
            "mp4",
        )
        .unwrap();
        assert_eq!(missing.streams[0].video_range_type, None);
    }

    #[test]
    fn audio_depth_uses_valid_precision_without_treating_compressed_codewords_as_samples() {
        for (codec, raw, coded, expected) in [
            ("flac", Some("16"), 0, Some(16)),
            ("pcm_s32le", Some("24"), 32, Some(24)),
            ("pcm_s16le", None, 16, Some(16)),
            ("pcm_s24le", Some("0"), 24, Some(24)),
            ("pcm_f64le", None, 64, Some(64)),
            ("adpcm_ima_wav", None, 4, None),
            ("aac", None, 0, None),
            ("flac", Some("0"), 16, None),
            ("pcm_s16le", Some("N/A"), 16, None),
            ("pcm_s16le", Some("999"), 16, None),
            ("pcm_s16le", Some("-1"), 16, None),
            ("pcm_s16le", None, 0, None),
            ("pcm_s16le", None, 65, None),
        ] {
            let output = serde_json::to_vec(&serde_json::json!({
                "streams":[{"index":0,"codec_type":"audio","codec_name":codec,
                    "bits_per_raw_sample":raw,"bits_per_sample":coded,"sample_rate":"44100"}],
                "format":{"format_name":"wav"},
            }))
            .unwrap();
            let parsed = parse_output(&output, "wav").unwrap();
            assert_eq!(
                parsed.streams[0].bit_depth, expected,
                "codec={codec}, raw={raw:?}, coded={coded}"
            );
            assert_eq!(parsed.streams[0].sample_rate, Some(44_100));
        }
        for rate in ["0", "-1", "invalid", "2147483648"] {
            let output = serde_json::to_vec(&serde_json::json!({
                "streams":[{"index":0,"codec_type":"audio","codec_name":"flac","sample_rate":rate}],
                "format":{"format_name":"flac"},
            }))
            .unwrap();
            assert_eq!(
                parse_output(&output, "flac").unwrap().streams[0].sample_rate,
                None
            );
        }
    }

    #[tokio::test]
    async fn external_ffprobe_does_not_follow_a_playlist_disguised_as_mp4() {
        let available = Command::new("ffprobe")
            .arg("-version")
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()
            .await;
        if !matches!(available, Ok(status) if status.success()) {
            eprintln!("ffprobe is not installed; skipped external probe isolation fixture");
            return;
        }

        let canary = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let canary_addr = canary.local_addr().unwrap();
        let fixture_dir =
            std::env::temp_dir().join(format!("puffinbox-probe-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&fixture_dir).unwrap();
        let outside_file = fixture_dir.join("outside-secret.txt");
        fs::write(&outside_file, "puffinbox-outside-secret-marker").unwrap();
        let playlist_as_mp4 = fixture_dir.join("malicious.mp4");
        fs::write(
            &playlist_as_mp4,
            format!(
                "#EXTM3U\n#EXTINF:1,fixture\nfile://{}\nhttp://{canary_addr}/probe-canary\n",
                outside_file.display()
            ),
        )
        .unwrap();
        let file = fs::File::open(&playlist_as_mp4).unwrap();
        let permit = std::sync::Arc::new(Semaphore::new(1))
            .acquire_owned()
            .await
            .unwrap();
        let result = run_ffprobe(
            Path::new("ffprobe").to_path_buf(),
            file,
            "mov",
            "mp4",
            false,
            permit,
        )
        .await;
        assert!(
            result.unwrap().is_none(),
            "playlist bytes must not be auto-detected as a media manifest"
        );
        assert!(
            timeout(Duration::from_millis(200), canary.accept())
                .await
                .is_err(),
            "ffprobe must not make an HTTP request from a crafted local playlist"
        );
        fs::remove_dir_all(fixture_dir).unwrap();
    }

    #[tokio::test]
    async fn probe_process_permit_survives_caller_cancellation_until_child_exit() {
        let base =
            std::env::temp_dir().join(format!("puffinbox-probe-lease-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&base).unwrap();
        let marker = base.join("started");
        let permits = std::sync::Arc::new(Semaphore::new(1));
        let permit = permits.clone().try_acquire_owned().unwrap();
        let mut command = Command::new(std::env::current_exe().unwrap());
        crate::media_features::process_limits::apply_child_limits(
            &mut command,
            10,
            4 * 1024 * 1024 * 1024,
            4096,
            None,
        );
        command
            .env_clear()
            .env("PUFFINBOX_PROBE_DELAY_CHILD", "1")
            .env("PUFFINBOX_PROBE_DELAY_MARKER", &marker)
            .arg("--exact")
            .arg("media_features::probe::tests::probe_delay_child")
            .arg("--nocapture")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let (mut child_task, cancel_guard) = launch_probe_child(command, permit);
        timeout(Duration::from_secs(3), async {
            while !marker.exists() {
                sleep(Duration::from_millis(10)).await;
            }
        })
        .await
        .expect("delayed probe child did not start");
        let child_pid = fs::read_to_string(&marker)
            .unwrap()
            .parse::<libc::pid_t>()
            .unwrap();
        assert!(
            permits.clone().try_acquire_owned().is_err(),
            "the probe permit must remain occupied while ffprobe is running"
        );

        // Dropping the caller's cancellation guard models an aborted API
        // request. The detached process task must keep owning the permit until
        // it has killed and reaped the child.
        drop(cancel_guard);
        assert!(
            timeout(Duration::from_secs(3), &mut child_task)
                .await
                .expect("probe child cleanup timed out")
                .expect("probe worker should complete")
                .is_none()
        );
        assert_eq!(unsafe { libc::kill(child_pid, 0) }, -1);
        assert_eq!(
            std::io::Error::last_os_error().raw_os_error(),
            Some(libc::ESRCH)
        );
        let permit = permits
            .try_acquire_owned()
            .expect("probe permit should release only after child exit");
        drop(permit);
        fs::remove_dir_all(base).unwrap();
    }

    #[tokio::test]
    async fn cancelled_probe_before_spawn_keeps_input_fd_from_reuse() {
        let base =
            std::env::temp_dir().join(format!("puffinbox-probe-race-{}", uuid::Uuid::new_v4()));
        fs::create_dir_all(&base).unwrap();
        let input_path = base.join("approved-input.bin");
        let canary_path = base.join("unrelated-canary.bin");
        let marker_path = base.join("must-not-start");
        fs::write(&input_path, b"approved media source").unwrap();
        fs::write(&canary_path, b"unrelated descriptor contents").unwrap();
        let input = fs::File::open(&input_path).unwrap();
        let parent_input_fd = input.as_raw_fd();
        let sandbox =
            MediaChildSandbox::prepare(&std::env::current_exe().unwrap(), input, None).unwrap();
        let permits = std::sync::Arc::new(Semaphore::new(1));
        let permit = permits.clone().try_acquire_owned().unwrap();
        let mut command = Command::new(sandbox.executable());
        apply_child_limits(&mut command, 10, 4 * 1024 * 1024 * 1024, 0, Some(sandbox));
        command
            .env_clear()
            .env("PUFFINBOX_PROBE_DELAY_CHILD", "1")
            .env("PUFFINBOX_PROBE_DELAY_MARKER", &marker_path)
            .arg("--exact")
            .arg("media_features::probe::tests::probe_delay_child")
            .arg("--nocapture")
            .stdin(Stdio::null())
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .kill_on_drop(true);
        let (ready_tx, ready_rx) = tokio::sync::oneshot::channel();
        let (release_tx, release_rx) = tokio::sync::oneshot::channel();
        let (mut child_task, cancel_guard) =
            launch_probe_child_with_start_gate(command, permit, ready_tx, release_rx);
        timeout(Duration::from_secs(2), ready_rx)
            .await
            .expect("worker did not reach the pre-spawn barrier")
            .expect("pre-spawn barrier was dropped");

        // Model an aborted request while the subprocess worker is paused. The
        // command-owned sandbox must keep the input open, so a new canary file
        // cannot reuse that descriptor before any child spawn occurs.
        drop(cancel_guard);
        let canary = fs::File::open(&canary_path).unwrap();
        assert_ne!(canary.as_raw_fd(), parent_input_fd);
        assert!(permits.clone().try_acquire_owned().is_err());
        release_tx.send(()).unwrap();
        assert!(
            timeout(Duration::from_secs(2), &mut child_task)
                .await
                .expect("cancelled probe worker did not stop")
                .expect("probe worker should return normally")
                .is_none()
        );
        assert!(
            !marker_path.exists(),
            "cancelled probe must not spawn a child"
        );
        let permit = permits
            .try_acquire_owned()
            .expect("probe permit releases after pre-spawn cleanup");
        drop(permit);
        drop(canary);
        fs::remove_dir_all(base).unwrap();
    }

    #[test]
    fn probe_delay_child() {
        if std::env::var_os("PUFFINBOX_PROBE_DELAY_CHILD").is_none() {
            return;
        }
        let marker = std::env::var("PUFFINBOX_PROBE_DELAY_MARKER").unwrap();
        let temporary_marker = format!("{marker}.tmp");
        fs::write(&temporary_marker, std::process::id().to_string()).unwrap();
        fs::rename(temporary_marker, marker).unwrap();
        std::thread::sleep(Duration::from_secs(30));
    }
}
