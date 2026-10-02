//! Jellyfin-shaped PlaybackInfo negotiation that only advertises capabilities
//! the source probe and configured local toolchain actually support.

use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{ApiError, state::AppState};

use super::{
    hls,
    probe::{self, ProbeInfo, ProbedStream},
    secure_path::ResolvedMedia,
    subtitles,
};

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct PlaybackInfoRequest {
    pub device_profile: Option<DeviceProfile>,
    pub start_time_ticks: Option<i64>,
    #[serde(default, deserialize_with = "deserialize_stream_index")]
    pub audio_stream_index: Option<i32>,
    #[serde(default, deserialize_with = "deserialize_stream_index")]
    pub subtitle_stream_index: Option<i32>,
    pub max_streaming_bitrate: Option<u64>,
    pub max_audio_channels: Option<u32>,
    pub enable_direct_play: Option<bool>,
    pub enable_direct_stream: Option<bool>,
    pub enable_transcoding: Option<bool>,
    pub allow_video_stream_copy: Option<bool>,
    pub allow_audio_stream_copy: Option<bool>,
    pub always_burn_in_subtitle_when_transcoding: Option<bool>,
    #[serde(rename = "ApiKey")]
    pub(super) api_key: Option<String>,
}

fn deserialize_stream_index<'de, D>(deserializer: D) -> Result<Option<i32>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct StreamIndex;

    impl<'de> serde::de::Visitor<'de> for StreamIndex {
        type Value = Option<i32>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter) -> std::fmt::Result {
            formatter.write_str("a stream index, null, or an empty string")
        }

        fn visit_unit<E>(self) -> Result<Self::Value, E> {
            Ok(None)
        }

        fn visit_i64<E: serde::de::Error>(self, value: i64) -> Result<Self::Value, E> {
            i32::try_from(value).map(Some).map_err(E::custom)
        }

        fn visit_u64<E: serde::de::Error>(self, value: u64) -> Result<Self::Value, E> {
            i32::try_from(value).map(Some).map_err(E::custom)
        }

        fn visit_str<E: serde::de::Error>(self, value: &str) -> Result<Self::Value, E> {
            // The official web client uses an empty string when no subtitle is selected.
            if value.is_empty() {
                Ok(None)
            } else {
                value.parse::<i32>().map(Some).map_err(E::custom)
            }
        }
    }

    deserializer.deserialize_any(StreamIndex)
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct DeviceProfile {
    #[serde(default)]
    direct_play_profiles: Vec<DirectPlayProfile>,
    #[serde(default)]
    transcoding_profiles: Vec<TranscodingProfile>,
    max_streaming_bitrate: Option<u64>,
    max_audio_channels: Option<u32>,
    #[serde(default)]
    codec_profiles: Vec<CodecProfile>,
    #[serde(default)]
    container_profiles: Vec<serde_json::Value>,
    #[serde(default)]
    subtitle_profiles: Vec<SubtitleProfile>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct DirectPlayProfile {
    #[serde(rename = "Type")]
    kind: Option<String>,
    container: Option<String>,
    audio_codec: Option<String>,
    video_codec: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct CodecProfile {
    #[serde(rename = "Type")]
    kind: Option<String>,
    codec: Option<String>,
    #[serde(default)]
    conditions: Vec<serde_json::Value>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct SubtitleProfile {
    #[serde(rename = "Format")]
    format: Option<String>,
    #[serde(rename = "Method")]
    method: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct TranscodingProfile {
    #[serde(rename = "Type")]
    kind: Option<String>,
    container: Option<String>,
    protocol: Option<String>,
    audio_codec: Option<String>,
    video_codec: Option<String>,
    max_audio_channels: Option<String>,
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct PlaybackInfoResponse {
    pub play_session_id: String,
    pub media_sources: Vec<MediaSource>,
}

impl PlaybackInfoResponse {
    pub(super) fn supports_direct_play(&self) -> bool {
        self.media_sources
            .first()
            .is_some_and(|source| source.supports_direct_play)
    }

    pub(super) fn has_hls_urls(&self) -> bool {
        self.media_sources.iter().any(|source| {
            source.transcoding_url.is_some()
                || source
                    .direct_stream_url
                    .as_deref()
                    .is_some_and(|url| url.contains("/master.m3u8?"))
        })
    }

    pub(super) fn authorize_hls_urls(&mut self, api_key: &str) {
        for source in &mut self.media_sources {
            if let Some(url) = &mut source.transcoding_url {
                *url = hls::append_api_key(url, Some(api_key));
            }
            if let Some(url) = &mut source.direct_stream_url
                && url.contains("/master.m3u8?")
            {
                *url = hls::append_api_key(url, Some(api_key));
            }
        }
    }
}

/// Reuse the probed PlaybackInfo rules for the universal audio direct path.
pub(super) fn audio_direct_request(
    containers: Vec<(String, Option<String>)>,
    max_streaming_bitrate: Option<u64>,
    max_audio_channels: Option<u32>,
) -> PlaybackInfoRequest {
    PlaybackInfoRequest {
        device_profile: Some(DeviceProfile {
            direct_play_profiles: containers
                .into_iter()
                .map(|(container, audio_codec)| {
                    // Bare universal container names declare these common
                    // audio codecs. Keep unknown combinations conservative.
                    let audio_codec = audio_codec.or_else(|| {
                        match container.as_str() {
                            "flac" => Some("flac"),
                            "mp3" => Some("mp3"),
                            "aac" => Some("aac"),
                            "opus" => Some("opus"),
                            "ogg" => Some("vorbis,opus,flac"),
                            "wav" => Some("pcm_s16le,pcm_s24le,pcm_s32le,pcm_u8,pcm_f32le"),
                            "webm" | "webma" => Some("opus,vorbis"),
                            "m4a" | "m4b" => Some("aac,alac"),
                            _ => None,
                        }
                        .map(str::to_owned)
                    });
                    DirectPlayProfile {
                        kind: Some("Audio".to_owned()),
                        container: Some(container),
                        audio_codec,
                        ..Default::default()
                    }
                })
                .collect(),
            ..Default::default()
        }),
        max_streaming_bitrate,
        max_audio_channels,
        enable_direct_play: Some(true),
        enable_direct_stream: Some(false),
        enable_transcoding: Some(false),
        ..Default::default()
    }
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub(super) struct MediaSource {
    id: String,
    name: String,
    path: Option<String>,
    protocol: String,
    #[serde(rename = "Type")]
    source_type: String,
    container: Option<String>,
    size: u64,
    run_time_ticks: Option<i64>,
    supports_direct_play: bool,
    supports_direct_stream: bool,
    supports_transcoding: bool,
    default_audio_stream_index: Option<i32>,
    default_subtitle_stream_index: Option<i32>,
    media_streams: Vec<MediaStream>,
    formats: Vec<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    direct_stream_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    transcoding_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    transcoding_sub_protocol: Option<&'static str>,
    #[serde(skip_serializing_if = "Option::is_none")]
    transcoding_container: Option<&'static str>,
}

/// Build a deliberately conservative Live TV response. A tuner/IPTV source
/// has not yet been probed, so this advertises only the HLS encoder that the
/// service will actually start, and never claims direct play or stream copy.
pub(super) fn live_playback_info(
    item_id: Uuid,
    name: String,
    request: &PlaybackInfoRequest,
    ffmpeg_available: bool,
) -> Result<PlaybackInfoResponse, ApiError> {
    if request.start_time_ticks.is_some_and(|ticks| ticks != 0)
        || request.audio_stream_index.is_some_and(|index| index >= 0)
        || request
            .subtitle_stream_index
            .is_some_and(|index| index >= 0)
    {
        return Err(ApiError::BadRequest(
            "Live channels do not support seeking or selecting unprobed tracks".to_owned(),
        ));
    }
    if request.max_streaming_bitrate == Some(0)
        || request
            .max_audio_channels
            .is_some_and(|channels| channels == 0 || channels > 32)
    {
        return Err(ApiError::BadRequest(
            "Playback limits must be positive and bounded".to_owned(),
        ));
    }
    let bitrate = request
        .max_streaming_bitrate
        .unwrap_or(2_000_000)
        .min(8_000_000);
    let channels = request.max_audio_channels.unwrap_or(2).min(2);
    let profile_supported = request.device_profile.as_ref().is_none_or(|profile| {
        profile
            .transcoding_profiles
            .iter()
            .any(|candidate| hls_profile_matches(candidate, true, true, Some(bitrate), channels))
    });
    let transcode = ffmpeg_available
        && request.enable_transcoding != Some(false)
        && bitrate >= 320_000
        && profile_supported;
    let play_session_id = Uuid::new_v4();
    let transcoding_url = transcode.then(|| {
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        query.append_pair("PlaySessionId", &play_session_id.to_string());
        query.append_pair("maxStreamingBitrate", &bitrate.to_string());
        query.append_pair("maxAudioChannels", &channels.to_string());
        if let Some(api_key) = request.api_key.as_deref() {
            query.append_pair("ApiKey", api_key);
        }
        format!("/LiveTv/Channels/{item_id}/master.m3u8?{}", query.finish())
    });
    Ok(PlaybackInfoResponse {
        play_session_id: play_session_id.to_string(),
        media_sources: vec![MediaSource {
            id: item_id.to_string(),
            name,
            path: None,
            protocol: "Http".to_owned(),
            source_type: "Default".to_owned(),
            container: Some("m3u8".to_owned()),
            size: 0,
            run_time_ticks: None,
            supports_direct_play: false,
            supports_direct_stream: false,
            supports_transcoding: transcode,
            default_audio_stream_index: None,
            default_subtitle_stream_index: None,
            media_streams: Vec::new(),
            formats: vec!["m3u8".to_owned()],
            direct_stream_url: None,
            transcoding_url,
            transcoding_sub_protocol: transcode.then_some("hls"),
            transcoding_container: transcode.then_some("ts"),
        }],
    })
}

#[derive(Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
struct MediaStream {
    index: u32,
    #[serde(rename = "Type")]
    kind: String,
    codec: Option<String>,
    profile: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    video_range_type: Option<&'static str>,
    language: Option<String>,
    title: Option<String>,
    is_default: bool,
    is_forced: bool,
    width: Option<u32>,
    height: Option<u32>,
    channels: Option<u32>,
    sample_rate: Option<u32>,
    bit_rate: Option<u64>,
    is_external: bool,
    is_text_subtitle_stream: bool,
    supports_external_stream: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    delivery_method: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    delivery_url: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    display_title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    is_external_url: Option<bool>,
}

pub(super) async fn negotiate(
    state: &AppState,
    media: ResolvedMedia,
    request: PlaybackInfoRequest,
) -> Result<PlaybackInfoResponse, ApiError> {
    let play_session_id = Uuid::new_v4().to_string();
    if request.start_time_ticks.is_some_and(|ticks| ticks < 0) {
        return Err(ApiError::BadRequest(
            "StartTimeTicks cannot be negative".to_owned(),
        ));
    }
    let video = is_video_type(&media.item.item_type);
    let audio = is_audio_type(&media.item.item_type);
    if !video && !audio {
        return Err(ApiError::BadRequest(
            "PlaybackInfo is supported for audio and video items".to_owned(),
        ));
    }

    let metadata = probe::probe(state, &media).await?;
    let Some(metadata) = metadata else {
        // Without a successful, bounded source probe, no profile-dependent
        // playback capability can be claimed.
        return Ok(PlaybackInfoResponse {
            play_session_id: play_session_id.clone(),
            media_sources: vec![fallback_source(&media, &play_session_id)],
        });
    };

    if let Some(index) = request.audio_stream_index
        && (index < -1
            || (index >= 0
                && !metadata
                    .streams
                    .iter()
                    .any(|stream| stream.index == index as u32 && stream.kind == "audio")))
    {
        return Err(ApiError::BadRequest(
            "AudioStreamIndex does not identify an audio stream".to_owned(),
        ));
    }
    if request
        .subtitle_stream_index
        .is_some_and(|index| index < -1)
    {
        return Err(ApiError::BadRequest(
            "SubtitleStreamIndex cannot be less than -1".to_owned(),
        ));
    }
    let after_index = metadata.streams.iter().map(|stream| stream.index).max();
    let sidecars = subtitles::list_sidecars(&media, after_index).await?;
    if let Some(index) = request
        .subtitle_stream_index
        .filter(|index| *index >= 0)
        .map(|index| index as u32)
        && !metadata
            .streams
            .iter()
            .any(|stream| stream.index == index && stream.kind == "subtitle")
        && !sidecars.iter().any(|track| track.index == index)
    {
        return Err(ApiError::BadRequest(
            "SubtitleStreamIndex does not identify a subtitle stream".to_owned(),
        ));
    }

    let source_extension = media
        .absolute_path
        .extension()
        .and_then(|v| v.to_str())
        .unwrap_or_default()
        .to_ascii_lowercase();
    let max_streaming_bitrate = [
        request.max_streaming_bitrate,
        request
            .device_profile
            .as_ref()
            .and_then(|profile| profile.max_streaming_bitrate),
    ]
    .into_iter()
    .flatten()
    .min();
    if request.max_streaming_bitrate == Some(0)
        || request
            .max_audio_channels
            .is_some_and(|value| value == 0 || value > 32)
    {
        return Err(ApiError::BadRequest(
            "Playback bitrate and channel limits must be positive and bounded".to_owned(),
        ));
    }
    let no_start_offset = request.start_time_ticks.unwrap_or_default() == 0;
    let no_track_selection = request.audio_stream_index.is_none()
        && request
            .subtitle_stream_index
            .is_none_or(|index| index == -1);
    let direct = request.enable_direct_play.unwrap_or(true)
        && request.device_profile.as_ref().is_some_and(|profile| {
            // Original-file streams preserve the full timeline; the client
            // seeks to StartTimeTicks using the source's byte-range support.
            no_track_selection
                && max_streaming_bitrate
                    .is_none_or(|max| total_bit_rate(&metadata).is_some_and(|rate| rate <= max))
                && direct_audio_channels_fit(profile, &metadata, request.max_audio_channels)
                && !has_unhandled_codec_constraints(profile, &metadata)
                && profile.container_profiles.is_empty()
                && profile.direct_play_profiles.iter().any(|candidate| {
                    profile_matches(candidate, &source_extension, &metadata, video, audio)
                })
        });
    let source_has_audio = metadata.streams.iter().any(|stream| stream.kind == "audio");
    let output_audio =
        (audio || video) && source_has_audio && request.audio_stream_index != Some(-1);
    let selected_video = video
        .then(|| probe::default_stream(&metadata.streams, "video"))
        .flatten();
    let selected_audio = if output_audio {
        match request.audio_stream_index {
            Some(index) if index >= 0 => metadata
                .streams
                .iter()
                .find(|stream| stream.index == index as u32 && stream.kind == "audio"),
            _ => probe::default_stream(&metadata.streams, "audio"),
        }
    } else {
        None
    };
    let subtitle_selected = request
        .subtitle_stream_index
        .is_some_and(|index| index >= 0);
    let subtitle_supported = request
        .subtitle_stream_index
        .filter(|index| *index >= 0)
        .is_none_or(|index| {
            subtitle_can_be_extracted(&metadata, index as u32)
                || sidecars.iter().any(|track| {
                    track.index == index as u32 && matches!(track.format.as_str(), "srt" | "vtt")
                })
        })
        && !(subtitle_selected
            && request
                .always_burn_in_subtitle_when_transcoding
                .unwrap_or(false));
    let device_channel_limit = request
        .device_profile
        .as_ref()
        .and_then(|profile| profile.max_audio_channels);
    let requested_channel_limit = [request.max_audio_channels, device_channel_limit]
        .into_iter()
        .flatten()
        .min();
    let mut hls_output_channels = requested_channel_limit.unwrap_or(2).min(2);
    let hls_subtitles_supported = request
        .subtitle_stream_index
        .filter(|index| *index >= 0)
        .is_none_or(|_| {
            request
                .device_profile
                .as_ref()
                .is_some_and(hls_subtitle_profile_supported)
        });
    let bounded_duration = metadata
        .duration_seconds
        .or_else(|| {
            metadata
                .catalog_identity_matches
                .then_some(media.item.runtime_ticks)
                .flatten()
                .map(|ticks| ticks as f64 / 10_000_000.0)
        })
        .is_some_and(|duration| duration > 0.0 && duration <= 14_400.0);
    let transcode_preconditions = request.enable_transcoding.unwrap_or(true)
        && (video || output_audio)
        && !(audio && subtitle_selected)
        && subtitle_supported;
    let ffmpeg_hls_available = transcode_preconditions && hls::ffmpeg_hls_available(state).await;
    let transcode_output_channels = request.device_profile.as_ref().and_then(|profile| {
        hls_transcode_output_channels(
            profile,
            video,
            output_audio,
            max_streaming_bitrate,
            requested_channel_limit,
            ffmpeg_hls_available,
            hls_subtitles_supported,
        )
    });
    let transcode_supported = transcode_preconditions && transcode_output_channels.is_some();
    if let Some(channels) = transcode_output_channels {
        hls_output_channels = channels;
    }
    let copy_bandwidth = total_bit_rate(&metadata).map(|rate| rate.saturating_mul(110) / 100);
    let copy_within_bitrate = copy_bandwidth
        .is_some_and(|bandwidth| max_streaming_bitrate.is_none_or(|maximum| bandwidth <= maximum));
    let copy_channels = selected_audio.and_then(|stream| stream.channels);
    let copy_channels_fit = selected_audio.is_none_or(|_| {
        let Some(channels) = copy_channels else {
            return false;
        };
        channels <= requested_channel_limit.unwrap_or(2)
    });
    let copy_source_codecs_fit = selected_video
        .is_none_or(|stream| stream.codec.as_deref() == Some("h264"))
        && selected_audio.is_none_or(|stream| stream.codec.as_deref() == Some("aac"))
        && (video || output_audio);
    let direct_stream_supported = request.enable_direct_stream.unwrap_or(true)
        && selected_video.is_none_or(|_| request.allow_video_stream_copy.unwrap_or(true))
        && (!output_audio || request.allow_audio_stream_copy.unwrap_or(true))
        && no_start_offset
        && subtitle_supported
        && hls_subtitles_supported
        && copy_within_bitrate
        && copy_channels_fit
        && copy_source_codecs_fit
        && (!video || selected_video.is_some())
        && (!output_audio || selected_audio.is_some())
        && request.device_profile.as_ref().is_some_and(|profile| {
            hls_copy_profile_supported(profile, video, output_audio, copy_channels.unwrap_or(0))
        })
        && hls::ffmpeg_hls_copy_available(state).await;
    let item_id = media.item.id;
    let direct_play_url = direct.then(|| {
        if video {
            format!("/Videos/{item_id}/stream")
        } else {
            format!("/Audio/{item_id}/stream")
        }
    });
    let direct_stream_url = direct_play_url
        .map(|url| hls::append_api_key(&url, request.api_key.as_deref()))
        .or_else(|| {
            direct_stream_supported.then(|| {
                build_hls_url(HlsUrlOptions {
                    item_id,
                    play_session_id: &play_session_id,
                    request: &request,
                    video,
                    output_audio,
                    output_channels: requested_channel_limit,
                    stream_copy: true,
                    full_timeline: false,
                    max_streaming_bitrate,
                })
            })
        });
    let transcoding_url = transcode_supported.then(|| {
        build_hls_url(HlsUrlOptions {
            item_id,
            play_session_id: &play_session_id,
            request: &request,
            video,
            output_audio,
            output_channels: Some(hls_output_channels),
            stream_copy: false,
            full_timeline: bounded_duration,
            max_streaming_bitrate,
        })
    });
    let ffmpeg_available = state
        .config
        .ffmpeg_path
        .as_ref()
        .is_some_and(|path| path.is_file());
    let mut streams = metadata
        .streams
        .iter()
        .map(|stream| MediaStream::from_probe(stream, item_id, ffmpeg_available))
        .collect::<Vec<_>>();
    streams.extend(sidecars.into_iter().map(|sidecar| MediaStream {
        index: sidecar.index,
        kind: "Subtitle".to_owned(),
        codec: Some(sidecar.format),
        profile: None,
        video_range_type: None,
        language: sidecar.language,
        title: Some(sidecar.title.clone()),
        is_default: false,
        is_forced: false,
        width: None,
        height: None,
        channels: None,
        sample_rate: None,
        bit_rate: None,
        is_external: true,
        is_text_subtitle_stream: true,
        supports_external_stream: true,
        delivery_method: Some("External".to_owned()),
        delivery_url: Some(format!(
            "/Videos/{item_id}/{item_id}/Subtitles/{}/Stream.vtt",
            sidecar.index
        )),
        display_title: Some(sidecar.title),
        is_external_url: Some(false),
    }));
    let duration_ticks = (metadata.catalog_identity_matches)
        .then_some(media.item.runtime_ticks)
        .flatten()
        .or_else(|| {
            metadata.duration_seconds.and_then(|seconds| {
                let ticks = (seconds * 10_000_000.0).round();
                (ticks.is_finite() && ticks >= 0.0 && ticks <= i64::MAX as f64)
                    .then_some(ticks as i64)
            })
        });
    let size = media
        .item
        .size_bytes
        .and_then(|value| u64::try_from(value).ok())
        .unwrap_or_default();
    let container = media
        .item
        .container
        .clone()
        .or_else(|| Some(source_extension.clone()));
    let formats = metadata.format_names.clone();

    let response = PlaybackInfoResponse {
        play_session_id,
        media_sources: vec![MediaSource {
            id: item_id.to_string(),
            name: media.item.name,
            path: None,
            protocol: "File".to_owned(),
            source_type: "Default".to_owned(),
            container,
            size,
            run_time_ticks: duration_ticks,
            supports_direct_play: direct,
            supports_direct_stream: direct_stream_supported,
            supports_transcoding: transcode_supported,
            default_audio_stream_index: selected_audio
                .and_then(|stream| i32::try_from(stream.index).ok()),
            default_subtitle_stream_index: request
                .subtitle_stream_index
                .filter(|index| *index >= 0),
            media_streams: streams,
            formats,
            direct_stream_url,
            transcoding_url,
            transcoding_sub_protocol: (transcode_supported || direct_stream_supported)
                .then_some("hls"),
            transcoding_container: (transcode_supported || direct_stream_supported).then_some("ts"),
        }],
    };
    let facts = PlaybackProfileFacts::for_negotiation(
        &request,
        &response,
        &metadata,
        &source_extension,
        video,
        audio,
        output_audio,
        max_streaming_bitrate,
        requested_channel_limit,
        subtitle_supported,
        hls_subtitles_supported,
        ffmpeg_hls_available,
    );
    tracing::info!(
        target: "puffinbox::playback",
        diagnostic = "profile_negotiation",
        media_video = facts.media_video,
        media_audio = facts.media_audio,
        device_profile_present = facts.device_profile_present,
        direct_profiles = facts.direct_profiles,
        direct_profiles_with_container = facts.direct_profiles_with_container,
        direct_profiles_with_video_codec = facts.direct_profiles_with_video_codec,
        direct_profiles_with_audio_codec = facts.direct_profiles_with_audio_codec,
        matching_direct_profiles = facts.matching_direct_profiles,
        transcoding_profiles = facts.transcoding_profiles,
        hls_profiles = facts.hls_profiles,
        hls_profiles_with_ts = facts.hls_profiles_with_ts,
        hls_profiles_with_h264 = facts.hls_profiles_with_h264,
        hls_profiles_with_aac = facts.hls_profiles_with_aac,
        matching_hls_profiles = facts.matching_hls_profiles,
        codec_profiles = facts.codec_profiles,
        codec_profiles_with_conditions = facts.codec_profiles_with_conditions,
        codec_condition_count = facts.codec_condition_count,
        codec_condition_property_category = facts.codec_condition_property_category,
        codec_condition_operator_category = facts.codec_condition_operator_category,
        codec_condition_value_category = facts.codec_condition_value_category,
        container_profiles = facts.container_profiles,
        subtitle_profiles = facts.subtitle_profiles,
        subtitle_vtt_hls_profiles = facts.subtitle_vtt_hls_profiles,
        subtitle_vtt_external_profiles = facts.subtitle_vtt_external_profiles,
        subtitle_vtt_other_method_profiles = facts.subtitle_vtt_other_method_profiles,
        subtitle_hls_other_format_profiles = facts.subtitle_hls_other_format_profiles,
        subtitle_format_vtt_profiles = facts.subtitle_format_vtt_profiles,
        subtitle_format_srt_profiles = facts.subtitle_format_srt_profiles,
        subtitle_format_ass_profiles = facts.subtitle_format_ass_profiles,
        subtitle_format_bitmap_profiles = facts.subtitle_format_bitmap_profiles,
        subtitle_format_other_profiles = facts.subtitle_format_other_profiles,
        subtitle_format_missing_profiles = facts.subtitle_format_missing_profiles,
        subtitle_srt_external_profiles = facts.subtitle_srt_external_profiles,
        subtitle_method_hls_profiles = facts.subtitle_method_hls_profiles,
        subtitle_method_external_profiles = facts.subtitle_method_external_profiles,
        subtitle_method_embed_profiles = facts.subtitle_method_embed_profiles,
        subtitle_method_encode_profiles = facts.subtitle_method_encode_profiles,
        subtitle_method_drop_profiles = facts.subtitle_method_drop_profiles,
        subtitle_method_other_profiles = facts.subtitle_method_other_profiles,
        subtitle_method_missing_profiles = facts.subtitle_method_missing_profiles,
        source_codec_constraints_supported = facts.source_codec_constraints_supported,
        stream_copy_codec_constraints_supported = facts.stream_copy_codec_constraints_supported,
        container_constraints_empty = facts.container_constraints_empty,
        selected_subtitles_supported = facts.selected_subtitles_supported,
        extractable_text_subtitle_streams = facts.extractable_text_subtitle_streams,
        hls_subtitles_supported = facts.hls_subtitles_supported,
        hls_encoder_available = facts.hls_encoder_available,
        source_count = facts.source_count,
        direct_play = facts.direct_play,
        direct_stream = facts.direct_stream,
        transcoding = facts.transcoding,
        direct_stream_url_present = facts.direct_stream_url_present,
        transcoding_url_present = facts.transcoding_url_present,
        "media playback profile diagnostic"
    );
    Ok(response)
}

#[derive(Debug, PartialEq, Eq)]
struct PlaybackProfileFacts {
    media_video: bool,
    media_audio: bool,
    device_profile_present: bool,
    direct_profiles: usize,
    direct_profiles_with_container: usize,
    direct_profiles_with_video_codec: usize,
    direct_profiles_with_audio_codec: usize,
    matching_direct_profiles: usize,
    transcoding_profiles: usize,
    hls_profiles: usize,
    hls_profiles_with_ts: usize,
    hls_profiles_with_h264: usize,
    hls_profiles_with_aac: usize,
    matching_hls_profiles: usize,
    codec_profiles: usize,
    codec_profiles_with_conditions: usize,
    codec_condition_count: usize,
    codec_condition_property_category: &'static str,
    codec_condition_operator_category: &'static str,
    codec_condition_value_category: &'static str,
    container_profiles: usize,
    subtitle_profiles: usize,
    subtitle_vtt_hls_profiles: usize,
    subtitle_vtt_external_profiles: usize,
    subtitle_vtt_other_method_profiles: usize,
    subtitle_hls_other_format_profiles: usize,
    subtitle_format_vtt_profiles: usize,
    subtitle_format_srt_profiles: usize,
    subtitle_format_ass_profiles: usize,
    subtitle_format_bitmap_profiles: usize,
    subtitle_format_other_profiles: usize,
    subtitle_format_missing_profiles: usize,
    subtitle_srt_external_profiles: usize,
    subtitle_method_hls_profiles: usize,
    subtitle_method_external_profiles: usize,
    subtitle_method_embed_profiles: usize,
    subtitle_method_encode_profiles: usize,
    subtitle_method_drop_profiles: usize,
    subtitle_method_other_profiles: usize,
    subtitle_method_missing_profiles: usize,
    source_codec_constraints_supported: bool,
    stream_copy_codec_constraints_supported: bool,
    container_constraints_empty: bool,
    selected_subtitles_supported: bool,
    extractable_text_subtitle_streams: usize,
    hls_subtitles_supported: bool,
    hls_encoder_available: bool,
    source_count: usize,
    direct_play: bool,
    direct_stream: bool,
    transcoding: bool,
    direct_stream_url_present: bool,
    transcoding_url_present: bool,
}

impl PlaybackProfileFacts {
    #[allow(clippy::too_many_arguments)]
    fn for_negotiation(
        request: &PlaybackInfoRequest,
        response: &PlaybackInfoResponse,
        metadata: &ProbeInfo,
        source_extension: &str,
        media_video: bool,
        media_audio: bool,
        output_audio: bool,
        max_streaming_bitrate: Option<u64>,
        requested_channel_limit: Option<u32>,
        selected_subtitles_supported: bool,
        hls_subtitles_supported: bool,
        hls_encoder_available: bool,
    ) -> Self {
        let profile = request.device_profile.as_ref();
        let direct_profiles = profile.map_or(&[][..], |profile| &profile.direct_play_profiles);
        let transcoding_profiles = profile.map_or(&[][..], |profile| &profile.transcoding_profiles);
        let hls_profiles = transcoding_profiles
            .iter()
            .filter(|candidate| {
                candidate
                    .protocol
                    .as_deref()
                    .is_some_and(|protocol| protocol.eq_ignore_ascii_case("hls"))
            })
            .collect::<Vec<_>>();
        let matching_direct_profiles = direct_profiles
            .iter()
            .filter(|candidate| {
                profile_matches(
                    candidate,
                    source_extension,
                    metadata,
                    media_video,
                    media_audio,
                )
            })
            .count();
        let matching_hls_profiles = transcoding_profiles
            .iter()
            .filter(|candidate| {
                hls_profile_channels(candidate, output_audio, requested_channel_limit).is_some_and(
                    |channels| {
                        hls_profile_matches(
                            candidate,
                            media_video,
                            output_audio,
                            max_streaming_bitrate,
                            channels,
                        )
                    },
                )
            })
            .count();
        let source = response.media_sources.first();
        let condition_summary = codec_condition_summary(profile);
        let subtitle_profile_summary = SubtitleProfileSummary::for_profile(profile);
        Self {
            media_video,
            media_audio,
            device_profile_present: profile.is_some(),
            direct_profiles: direct_profiles.len(),
            direct_profiles_with_container: direct_profiles
                .iter()
                .filter(|candidate| candidate.container.is_some())
                .count(),
            direct_profiles_with_video_codec: direct_profiles
                .iter()
                .filter(|candidate| candidate.video_codec.is_some())
                .count(),
            direct_profiles_with_audio_codec: direct_profiles
                .iter()
                .filter(|candidate| candidate.audio_codec.is_some())
                .count(),
            matching_direct_profiles,
            transcoding_profiles: transcoding_profiles.len(),
            hls_profiles: hls_profiles.len(),
            hls_profiles_with_ts: hls_profiles
                .iter()
                .filter(|candidate| {
                    candidate
                        .container
                        .as_deref()
                        .is_some_and(|container| csv_contains(container, "ts"))
                })
                .count(),
            hls_profiles_with_h264: hls_profiles
                .iter()
                .filter(|candidate| {
                    candidate
                        .video_codec
                        .as_deref()
                        .is_some_and(|codec| csv_contains(codec, "h264"))
                })
                .count(),
            hls_profiles_with_aac: hls_profiles
                .iter()
                .filter(|candidate| {
                    candidate
                        .audio_codec
                        .as_deref()
                        .is_some_and(|codec| csv_contains(codec, "aac"))
                })
                .count(),
            matching_hls_profiles,
            codec_profiles: profile.map_or(0, |profile| profile.codec_profiles.len()),
            codec_profiles_with_conditions: profile.map_or(0, |profile| {
                profile
                    .codec_profiles
                    .iter()
                    .filter(|candidate| !candidate.conditions.is_empty())
                    .count()
            }),
            codec_condition_count: condition_summary.count,
            codec_condition_property_category: condition_summary.property,
            codec_condition_operator_category: condition_summary.operator,
            codec_condition_value_category: condition_summary.value,
            container_profiles: profile.map_or(0, |profile| profile.container_profiles.len()),
            subtitle_profiles: profile.map_or(0, |profile| profile.subtitle_profiles.len()),
            subtitle_vtt_hls_profiles: subtitle_profile_summary.vtt_hls,
            subtitle_vtt_external_profiles: subtitle_profile_summary.vtt_external,
            subtitle_vtt_other_method_profiles: subtitle_profile_summary.vtt_other_method,
            subtitle_hls_other_format_profiles: subtitle_profile_summary.hls_other_format,
            subtitle_format_vtt_profiles: subtitle_profile_summary.format_vtt,
            subtitle_format_srt_profiles: subtitle_profile_summary.format_srt,
            subtitle_format_ass_profiles: subtitle_profile_summary.format_ass,
            subtitle_format_bitmap_profiles: subtitle_profile_summary.format_bitmap,
            subtitle_format_other_profiles: subtitle_profile_summary.format_other,
            subtitle_format_missing_profiles: subtitle_profile_summary.format_missing,
            subtitle_srt_external_profiles: subtitle_profile_summary.srt_external,
            subtitle_method_hls_profiles: subtitle_profile_summary.method_hls,
            subtitle_method_external_profiles: subtitle_profile_summary.method_external,
            subtitle_method_embed_profiles: subtitle_profile_summary.method_embed,
            subtitle_method_encode_profiles: subtitle_profile_summary.method_encode,
            subtitle_method_drop_profiles: subtitle_profile_summary.method_drop,
            subtitle_method_other_profiles: subtitle_profile_summary.method_other,
            subtitle_method_missing_profiles: subtitle_profile_summary.method_missing,
            source_codec_constraints_supported: profile
                .is_some_and(|profile| !has_unhandled_codec_constraints(profile, metadata)),
            stream_copy_codec_constraints_supported: profile
                .is_some_and(|profile| !has_unhandled_hls_codec_constraints(profile)),
            container_constraints_empty: profile
                .is_some_and(|profile| profile.container_profiles.is_empty()),
            selected_subtitles_supported,
            extractable_text_subtitle_streams: source.map_or(0, |source| {
                source
                    .media_streams
                    .iter()
                    .filter(|stream| {
                        stream.is_text_subtitle_stream && stream.supports_external_stream
                    })
                    .take(33)
                    .count()
                    .min(32)
            }),
            hls_subtitles_supported,
            hls_encoder_available,
            source_count: response.media_sources.len(),
            direct_play: source.is_some_and(|source| source.supports_direct_play),
            direct_stream: source.is_some_and(|source| source.supports_direct_stream),
            transcoding: source.is_some_and(|source| source.supports_transcoding),
            direct_stream_url_present: source
                .is_some_and(|source| source.direct_stream_url.is_some()),
            transcoding_url_present: source.is_some_and(|source| source.transcoding_url.is_some()),
        }
    }
}

#[derive(Debug, Default, PartialEq, Eq)]
struct SubtitleProfileSummary {
    vtt_hls: usize,
    vtt_external: usize,
    vtt_other_method: usize,
    hls_other_format: usize,
    srt_external: usize,
    format_vtt: usize,
    format_srt: usize,
    format_ass: usize,
    format_bitmap: usize,
    format_other: usize,
    format_missing: usize,
    method_hls: usize,
    method_external: usize,
    method_embed: usize,
    method_encode: usize,
    method_drop: usize,
    method_other: usize,
    method_missing: usize,
}

impl SubtitleProfileSummary {
    fn for_profile(profile: Option<&DeviceProfile>) -> Self {
        let mut summary = Self::default();
        for subtitle in profile
            .into_iter()
            .flat_map(|profile| &profile.subtitle_profiles)
        {
            let format_category = subtitle_format_category(subtitle.format.as_deref());
            let method_category = subtitle_method_category(subtitle.method.as_deref());
            summary.bump_format(format_category);
            summary.bump_method(method_category);
            let is_vtt = format_category == "vtt";
            let is_srt = format_category == "srt";
            let is_hls = method_category == "hls";
            let is_external = method_category == "external";
            if is_vtt && is_hls {
                summary.vtt_hls += 1;
            } else if is_vtt && is_external {
                summary.vtt_external += 1;
            } else if is_vtt {
                summary.vtt_other_method += 1;
            } else if is_hls {
                summary.hls_other_format += 1;
            }
            if is_srt && is_external {
                summary.srt_external += 1;
            }
        }
        summary
    }

    fn bump_format(&mut self, category: &'static str) {
        match category {
            "vtt" => self.format_vtt += 1,
            "srt" => self.format_srt += 1,
            "ass" => self.format_ass += 1,
            "bitmap" => self.format_bitmap += 1,
            "other" => self.format_other += 1,
            _ => self.format_missing += 1,
        }
    }

    fn bump_method(&mut self, category: &'static str) {
        match category {
            "hls" => self.method_hls += 1,
            "external" => self.method_external += 1,
            "embed" => self.method_embed += 1,
            "encode" => self.method_encode += 1,
            "drop" => self.method_drop += 1,
            "other" => self.method_other += 1,
            _ => self.method_missing += 1,
        }
    }
}

fn subtitle_format_category(format: Option<&str>) -> &'static str {
    match format.map(str::to_ascii_lowercase).as_deref() {
        Some("vtt" | "webvtt") => "vtt",
        Some("srt" | "subrip") => "srt",
        Some("ass" | "ssa") => "ass",
        Some("pgs" | "pgssub" | "vobsub" | "dvdsub" | "dvd_subtitle" | "dvbsub") => "bitmap",
        Some(_) => "other",
        None => "missing",
    }
}

fn subtitle_method_category(method: Option<&str>) -> &'static str {
    match method.map(str::to_ascii_lowercase).as_deref() {
        Some("hls") => "hls",
        Some("external") => "external",
        Some("embed") => "embed",
        Some("encode") => "encode",
        Some("drop") => "drop",
        Some(_) => "other",
        None => "missing",
    }
}

#[derive(Debug, PartialEq, Eq)]
struct CodecConditionSummary {
    count: usize,
    property: &'static str,
    operator: &'static str,
    value: &'static str,
}

fn codec_condition_summary(profile: Option<&DeviceProfile>) -> CodecConditionSummary {
    let mut count = 0;
    let mut property = None;
    let mut operator = None;
    let mut value = None;
    if let Some(profile) = profile {
        for condition in profile
            .codec_profiles
            .iter()
            .flat_map(|codec| codec.conditions.iter())
        {
            count += 1;
            let categories = codec_condition_categories(condition);
            property = merge_category(property, categories.0);
            operator = merge_category(operator, categories.1);
            value = merge_category(value, categories.2);
        }
    }
    CodecConditionSummary {
        count,
        property: property.unwrap_or("none"),
        operator: operator.unwrap_or("none"),
        value: value.unwrap_or("none"),
    }
}

fn merge_category(current: Option<&'static str>, next: &'static str) -> Option<&'static str> {
    match current {
        Some("multiple") => current,
        Some(previous) if previous != next => Some("multiple"),
        Some(previous) => Some(previous),
        None => Some(next),
    }
}

fn codec_condition_categories(
    condition: &serde_json::Value,
) -> (&'static str, &'static str, &'static str) {
    let property = condition
        .get("Property")
        .and_then(serde_json::Value::as_str)
        .map(|value| match value.to_ascii_lowercase().as_str() {
            "videoprofile" => "video_profile",
            "videolevel" => "video_level",
            "videorangetype" => "video_range_type",
            "videobitdepth" => "video_bit_depth",
            "width" => "width",
            "height" => "height",
            "audiochannels" => "audio_channels",
            "issecondaryaudio" => "secondary_audio",
            "isinterlaced" => "interlaced",
            "isanamorphic" => "anamorphic",
            _ => "other",
        })
        .unwrap_or("missing");
    let operator = condition
        .get("Condition")
        .and_then(serde_json::Value::as_str)
        .map(|value| match value.to_ascii_lowercase().as_str() {
            "equals" => "equals",
            "notequals" => "not_equals",
            "equalsany" => "equals_any",
            "notequalsany" => "not_equals_any",
            "lessthan" => "less_than",
            "lessthanequal" => "less_than_equal",
            "greaterthan" => "greater_than",
            "greaterthanequal" => "greater_than_equal",
            _ => "other",
        })
        .unwrap_or("missing");
    let value = condition
        .get("Value")
        .map(|value| match value.as_str() {
            Some(value) if value.eq_ignore_ascii_case("SDR") => "sdr",
            Some(value) if value.eq_ignore_ascii_case("DOVI") => "dovi",
            Some(value) if value.eq_ignore_ascii_case("true") => "true",
            Some(value) if value.eq_ignore_ascii_case("false") => "false",
            Some(value) if value.contains('|') => "list",
            Some(value) if value.parse::<f64>().is_ok() => "number",
            Some(_) => "text",
            None if value.is_number() => "number",
            _ => "other",
        })
        .unwrap_or("missing");
    (property, operator, value)
}

fn fallback_source(media: &ResolvedMedia, _play_session_id: &str) -> MediaSource {
    let item_id = media.item.id;
    MediaSource {
        id: item_id.to_string(),
        name: media.item.name.clone(),
        path: None,
        protocol: "File".to_owned(),
        source_type: "Default".to_owned(),
        container: media.item.container.clone().or_else(|| {
            media
                .absolute_path
                .extension()
                .and_then(|v| v.to_str())
                .map(str::to_owned)
        }),
        size: media
            .item
            .size_bytes
            .and_then(|value| u64::try_from(value).ok())
            .unwrap_or_default(),
        run_time_ticks: media
            .catalog_identity_matches
            .then_some(media.item.runtime_ticks)
            .flatten(),
        supports_direct_play: false,
        supports_direct_stream: false,
        supports_transcoding: false,
        default_audio_stream_index: None,
        default_subtitle_stream_index: None,
        media_streams: Vec::new(),
        formats: Vec::new(),
        direct_stream_url: None,
        transcoding_url: None,
        transcoding_sub_protocol: None,
        transcoding_container: None,
    }
}

struct HlsUrlOptions<'a> {
    item_id: Uuid,
    play_session_id: &'a str,
    request: &'a PlaybackInfoRequest,
    video: bool,
    output_audio: bool,
    output_channels: Option<u32>,
    stream_copy: bool,
    full_timeline: bool,
    max_streaming_bitrate: Option<u64>,
}

fn build_hls_url(options: HlsUrlOptions<'_>) -> String {
    let HlsUrlOptions {
        item_id,
        play_session_id,
        request,
        video,
        output_audio,
        output_channels,
        stream_copy,
        full_timeline,
        max_streaming_bitrate,
    } = options;
    let path = if video {
        format!("/Videos/{item_id}/master.m3u8")
    } else {
        format!("/Audio/{item_id}/master.m3u8")
    };
    let mut query = vec![format!("playSessionId={play_session_id}")];
    if stream_copy {
        query.push("streamCopy=true".to_owned());
    } else if full_timeline {
        query.push("fullTimeline=true".to_owned());
    } else if let Some(ticks) = request.start_time_ticks.filter(|ticks| *ticks > 0) {
        query.push(format!("StartTimeTicks={ticks}"));
    }
    if let Some(index) = request.audio_stream_index.filter(|_| output_audio) {
        query.push(format!("audioStreamIndex={index}"));
    } else if request.audio_stream_index == Some(-1) {
        query.push("audioStreamIndex=-1".to_owned());
    }
    if let Some(index) = request.subtitle_stream_index {
        query.push(format!("subtitleStreamIndex={index}"));
    }
    if let Some(bitrate) = max_streaming_bitrate {
        query.push(format!("maxStreamingBitrate={bitrate}"));
    }
    if output_audio && let Some(channels) = output_channels {
        query.push(format!("maxAudioChannels={channels}"));
    }
    hls::append_api_key(
        &format!("{path}?{}", query.join("&")),
        request.api_key.as_deref(),
    )
}

fn profile_matches(
    profile: &DirectPlayProfile,
    extension: &str,
    metadata: &ProbeInfo,
    video: bool,
    audio: bool,
) -> bool {
    let expected_kind = if video { "video" } else { "audio" };
    if !profile
        .kind
        .as_deref()
        .is_some_and(|kind| kind.eq_ignore_ascii_case(expected_kind))
    {
        return false;
    }
    let type_only = profile.container.is_none()
        && profile.video_codec.is_none()
        && profile.audio_codec.is_none();
    if profile.container.is_none() && !type_only {
        return false;
    }
    if !extension_agrees_with_probe(extension, &metadata.format_names) {
        return false;
    }
    if profile.container.as_deref().is_some_and(|containers| {
        !csv_contains(containers, extension)
            && !metadata
                .format_names
                .iter()
                .any(|format| csv_contains(containers, format))
    }) {
        return false;
    }
    let active_streams = metadata
        .streams
        .iter()
        .filter(|stream| {
            (video && stream.kind == "video")
                || (audio && stream.kind == "audio")
                || (video && stream.kind == "audio")
        })
        .collect::<Vec<_>>();
    if active_streams.is_empty() {
        return false;
    }
    active_streams
        .into_iter()
        .all(|stream| match stream.kind.as_str() {
            "video" if type_only => stream
                .codec
                .as_deref()
                .is_some_and(|codec| !codec.trim().is_empty()),
            "audio" if type_only => stream
                .codec
                .as_deref()
                .is_some_and(|codec| !codec.trim().is_empty()),
            "video" => codec_list_matches(profile.video_codec.as_deref(), stream.codec.as_deref()),
            "audio" => codec_list_matches(profile.audio_codec.as_deref(), stream.codec.as_deref()),
            _ => true,
        })
}

fn hls_profile_matches(
    profile: &TranscodingProfile,
    video: bool,
    audio: bool,
    max_streaming_bitrate: Option<u64>,
    output_audio_channels: u32,
) -> bool {
    let expected_kind = if video {
        "video"
    } else if audio {
        "audio"
    } else {
        return false;
    };
    let kind_matches = profile
        .kind
        .as_deref()
        .is_some_and(|kind| kind.eq_ignore_ascii_case(expected_kind));
    let protocol_matches = profile
        .protocol
        .as_deref()
        .is_some_and(|protocol| protocol.eq_ignore_ascii_case("hls"));
    let container_matches = profile
        .container
        .as_deref()
        .is_some_and(|container| csv_contains(container, "ts"));
    let video_codec_matches = if video {
        profile
            .video_codec
            .as_deref()
            .is_some_and(|codec| csv_contains(codec, "h264"))
    } else {
        profile.video_codec.as_deref().is_none_or(str::is_empty)
    };
    let audio_codec_matches = !audio
        || profile
            .audio_codec
            .as_deref()
            .is_some_and(|codec| csv_contains(codec, "aac"));
    let audio_channels_supported = profile
        .max_audio_channels
        .as_deref()
        .map(|value| {
            value
                .parse::<u32>()
                .ok()
                .is_some_and(|profile_max| profile_max >= output_audio_channels)
        })
        .unwrap_or(output_audio_channels <= 2);
    let bitrate_is_enough = max_streaming_bitrate.is_none_or(|max| max >= 320_000);
    kind_matches
        && protocol_matches
        && container_matches
        && video_codec_matches
        && audio_codec_matches
        && bitrate_is_enough
        && audio_channels_supported
}

fn hls_profile_channels(
    profile: &TranscodingProfile,
    audio: bool,
    requested_limit: Option<u32>,
) -> Option<u32> {
    if !audio {
        return Some(0);
    }
    let profile_limit = match profile.max_audio_channels.as_deref() {
        Some(value) => value.parse::<u32>().ok().filter(|value| *value > 0)?,
        None => 2,
    };
    let output = requested_limit.unwrap_or(2).min(profile_limit).min(2);
    (output > 0).then_some(output)
}

fn hls_transcode_output_channels(
    profile: &DeviceProfile,
    video: bool,
    audio: bool,
    max_streaming_bitrate: Option<u64>,
    requested_channel_limit: Option<u32>,
    encoder_available: bool,
    subtitles_supported: bool,
) -> Option<u32> {
    if !encoder_available || !subtitles_supported || !profile.container_profiles.is_empty() {
        return None;
    }
    profile.transcoding_profiles.iter().find_map(|candidate| {
        let channels = hls_profile_channels(candidate, audio, requested_channel_limit)?;
        hls_profile_matches(candidate, video, audio, max_streaming_bitrate, channels)
            .then_some(channels)
    })
}

fn hls_subtitle_profile_supported(profile: &DeviceProfile) -> bool {
    profile.subtitle_profiles.is_empty()
        || profile.subtitle_profiles.iter().any(|subtitle| {
            subtitle.format.as_deref().is_some_and(|format| {
                format.eq_ignore_ascii_case("vtt") || format.eq_ignore_ascii_case("webvtt")
            }) && subtitle.method.as_deref().is_some_and(|method| {
                method.eq_ignore_ascii_case("external") || method.eq_ignore_ascii_case("hls")
            })
        })
}

fn extension_agrees_with_probe(extension: &str, formats: &[String]) -> bool {
    let expected: &[&str] = match extension {
        "mp4" | "m4v" | "mov" | "m4a" | "3gp" => &["mov", "mp4", "3gp"],
        "mkv" | "mka" => &["matroska"],
        "webm" => &["matroska", "webm"],
        "avi" => &["avi"],
        "mpeg" | "mpg" | "vob" => &["mpeg"],
        "ts" | "m2ts" => &["mpegts"],
        "mp3" => &["mp3"],
        "aac" => &["aac"],
        "flac" => &["flac"],
        "wav" => &["wav"],
        "aiff" | "aif" => &["aiff"],
        "ogg" | "oga" | "opus" => &["ogg"],
        _ => return false,
    };
    formats.iter().any(|format| {
        expected
            .iter()
            .any(|expected| format.eq_ignore_ascii_case(expected))
    })
}

fn subtitle_can_be_extracted(metadata: &ProbeInfo, index: u32) -> bool {
    metadata
        .streams
        .iter()
        .find(|stream| stream.index == index && stream.kind == "subtitle")
        .and_then(|stream| stream.codec.as_deref())
        .is_some_and(|codec| {
            matches!(
                codec,
                "subrip" | "srt" | "ass" | "ssa" | "webvtt" | "mov_text" | "text" | "ttml"
            )
        })
}

fn has_unhandled_hls_codec_constraints(profile: &DeviceProfile) -> bool {
    profile.codec_profiles.iter().any(|codec_profile| {
        let output_codec = codec_profile.codec.as_deref().is_none_or(|codec| {
            codec.trim().is_empty()
                || ["h264", "aac", "libx264"]
                    .iter()
                    .any(|wanted| csv_contains(codec, wanted))
        });
        output_codec && !codec_profile.conditions.is_empty()
    })
}

fn hls_copy_profile_supported(
    profile: &DeviceProfile,
    video: bool,
    audio: bool,
    output_audio_channels: u32,
) -> bool {
    !has_unhandled_hls_codec_constraints(profile)
        && profile.container_profiles.is_empty()
        && profile.transcoding_profiles.iter().any(|candidate| {
            hls_profile_matches(candidate, video, audio, None, output_audio_channels)
        })
}

fn total_bit_rate(metadata: &ProbeInfo) -> Option<u64> {
    if let Some(rate) = metadata.bit_rate {
        return (rate > 0).then_some(rate);
    }
    let streams = metadata
        .streams
        .iter()
        .filter(|stream| stream.kind == "video" || stream.kind == "audio")
        .collect::<Vec<_>>();
    if streams.is_empty() || streams.iter().any(|stream| stream.bit_rate.is_none()) {
        return None;
    }
    let rate = streams
        .into_iter()
        .filter_map(|stream| stream.bit_rate)
        .fold(0_u64, u64::saturating_add);
    (rate > 0).then_some(rate)
}

fn has_unhandled_codec_constraints(profile: &DeviceProfile, metadata: &ProbeInfo) -> bool {
    profile.codec_profiles.iter().any(|codec_profile| {
        if codec_profile.kind.as_deref().is_some_and(|kind| {
            !["video", "audio", "videoaudio"]
                .iter()
                .any(|known| kind.eq_ignore_ascii_case(known))
        }) {
            return !codec_profile.conditions.is_empty();
        }
        metadata.streams.iter().any(|stream| {
            let is_media_stream = stream.kind == "video" || stream.kind == "audio";
            let kind_applies = match codec_profile.kind.as_deref() {
                Some(kind) if kind.eq_ignore_ascii_case("video") => stream.kind == "video",
                Some(kind) if kind.eq_ignore_ascii_case("audio") => stream.kind == "audio",
                Some(kind) if kind.eq_ignore_ascii_case("videoaudio") => is_media_stream,
                _ => is_media_stream,
            };
            if !kind_applies {
                return false;
            }
            let codec_applies = match codec_profile.codec.as_deref() {
                None => true,
                Some(wanted) if wanted.trim().is_empty() => true,
                Some(wanted) => match stream.codec.as_deref() {
                    Some(codec) if !codec.trim().is_empty() => csv_contains(wanted, codec),
                    _ => true,
                },
            };
            codec_applies
                && codec_profile
                    .conditions
                    .iter()
                    .any(|condition| !source_condition_matches(condition, stream))
        })
    })
}

fn source_condition_matches(condition: &serde_json::Value, stream: &ProbedStream) -> bool {
    let Some(property) = condition
        .get("Property")
        .and_then(serde_json::Value::as_str)
    else {
        return false;
    };
    if !property.eq_ignore_ascii_case("VideoRangeType") || stream.kind != "video" {
        return false;
    }
    let Some(actual) = stream.video_range_type else {
        return false;
    };
    let Some(wanted) = condition.get("Value").and_then(serde_json::Value::as_str) else {
        return false;
    };
    if ![
        "Unknown",
        "SDR",
        "HDR10",
        "HLG",
        "DOVI",
        "DOVIWithHDR10",
        "DOVIWithHLG",
        "DOVIWithSDR",
        "DOVIWithEL",
        "DOVIWithHDR10Plus",
        "DOVIWithELHDR10Plus",
        "DOVIInvalid",
        "HDR10Plus",
    ]
    .iter()
    .any(|range| range.eq_ignore_ascii_case(wanted))
    {
        return false;
    }
    match condition
        .get("Condition")
        .and_then(serde_json::Value::as_str)
    {
        Some(operator) if operator.eq_ignore_ascii_case("Equals") => {
            actual.eq_ignore_ascii_case(wanted)
        }
        Some(operator) if operator.eq_ignore_ascii_case("NotEquals") => {
            !actual.eq_ignore_ascii_case(wanted)
        }
        _ => false,
    }
}

fn direct_audio_channels_fit(
    profile: &DeviceProfile,
    metadata: &ProbeInfo,
    requested: Option<u32>,
) -> bool {
    let limit = [profile.max_audio_channels, requested]
        .into_iter()
        .flatten()
        .min();
    let Some(limit) = limit else {
        return true;
    };
    metadata
        .streams
        .iter()
        .filter(|stream| stream.kind == "audio")
        .all(|stream| stream.channels.is_some_and(|channels| channels <= limit))
}

fn csv_contains(csv: &str, wanted: &str) -> bool {
    csv.split(',').any(|part| {
        part.trim()
            .trim_start_matches('.')
            .eq_ignore_ascii_case(wanted)
    })
}

fn codec_list_matches(supported: Option<&str>, actual: Option<&str>) -> bool {
    let Some(actual) = actual else { return false };
    let Some(supported) = supported else {
        return false;
    };
    supported
        .split(',')
        .any(|codec| codec.trim().eq_ignore_ascii_case(actual))
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

impl MediaStream {
    fn from_probe(stream: &ProbedStream, item_id: Uuid, ffmpeg_available: bool) -> Self {
        let is_text_subtitle_stream = stream.kind == "subtitle"
            && subtitle_can_be_extracted_from_codec(stream.codec.as_deref());
        let supports_external_stream = is_text_subtitle_stream && ffmpeg_available;
        let delivery_url = supports_external_stream.then(|| {
            format!(
                "/Videos/{item_id}/{item_id}/Subtitles/{}/Stream.vtt",
                stream.index
            )
        });
        Self {
            index: stream.index,
            kind: match stream.kind.as_str() {
                "video" => "Video",
                "audio" => "Audio",
                "subtitle" => "Subtitle",
                "lyric" => "Lyric",
                "embedded_image" => "EmbeddedImage",
                // The public wire enum has no Attachment or Unknown member;
                // keep those non-audio/video streams in the valid Data bucket.
                "attachment" | "data" => "Data",
                _ => "Data",
            }
            .to_owned(),
            codec: stream.codec.clone(),
            profile: stream.profile.clone(),
            video_range_type: stream.video_range_type,
            language: stream.language.clone(),
            title: stream.title.clone(),
            is_default: stream.is_default,
            is_forced: stream.is_forced,
            width: stream.width,
            height: stream.height,
            channels: stream.channels,
            sample_rate: stream.sample_rate,
            bit_rate: stream.bit_rate,
            is_external: false,
            is_text_subtitle_stream,
            supports_external_stream,
            delivery_method: supports_external_stream.then(|| "Encode".to_owned()),
            delivery_url,
            display_title: stream
                .title
                .clone()
                .or_else(|| stream.language.clone())
                .or_else(|| stream.codec.clone()),
            is_external_url: None,
        }
    }
}

fn subtitle_can_be_extracted_from_codec(codec: Option<&str>) -> bool {
    codec.is_some_and(|codec| {
        matches!(
            codec,
            "subrip" | "srt" | "ass" | "ssa" | "webvtt" | "mov_text" | "text" | "ttml"
        )
    })
}

#[cfg(test)]
mod tests {
    use super::{
        CodecProfile, DeviceProfile, DirectPlayProfile, HlsUrlOptions, MediaSource, MediaStream,
        PlaybackInfoRequest, PlaybackInfoResponse, PlaybackProfileFacts, ProbeInfo, ProbedStream,
        SubtitleProfile, SubtitleProfileSummary, TranscodingProfile, build_hls_url,
        codec_condition_summary, codec_list_matches, csv_contains, has_unhandled_codec_constraints,
        has_unhandled_hls_codec_constraints, hls_copy_profile_supported, hls_profile_channels,
        hls_profile_matches, hls_subtitle_profile_supported, hls_transcode_output_channels,
        profile_matches, total_bit_rate,
    };

    #[test]
    fn playback_stream_indices_accept_web_client_empty_and_query_values() {
        for (wire, expected) in [
            (serde_json::json!(null), None),
            (serde_json::json!(""), None),
            (serde_json::json!("-1"), Some(-1)),
            (serde_json::json!("2"), Some(2)),
            (serde_json::json!(3), Some(3)),
        ] {
            let request: PlaybackInfoRequest = serde_json::from_value(serde_json::json!({
                "AudioStreamIndex": wire,
                "SubtitleStreamIndex": wire
            }))
            .unwrap();
            assert_eq!(request.audio_stream_index, expected);
            assert_eq!(request.subtitle_stream_index, expected);
        }
        for wire in [
            serde_json::json!(false),
            serde_json::json!(1.5),
            serde_json::json!(2147483648_u64),
            serde_json::json!("2147483648"),
            serde_json::json!("invalid"),
        ] {
            assert!(
                serde_json::from_value::<PlaybackInfoRequest>(serde_json::json!({
                    "SubtitleStreamIndex": wire
                }))
                .is_err()
            );
        }
        let uri = "/Items/fixture/PlaybackInfo?AudioStreamIndex=2&SubtitleStreamIndex="
            .parse()
            .unwrap();
        let request = axum::extract::Query::<PlaybackInfoRequest>::try_from_uri(&uri).unwrap();
        assert_eq!(request.audio_stream_index, Some(2));
        assert_eq!(request.subtitle_stream_index, None);
    }

    #[test]
    fn live_playback_identifies_the_offered_streaming_protocol() {
        for available in [true, false] {
            let response = super::live_playback_info(
                uuid::Uuid::new_v4(),
                "Synthetic channel".to_owned(),
                &PlaybackInfoRequest::default(),
                available,
            )
            .unwrap();
            let wire = serde_json::to_value(response).unwrap();
            let source = &wire["MediaSources"][0];
            assert_eq!(source["SupportsTranscoding"], available);
            assert!(source["DefaultAudioStreamIndex"].is_null());
            assert!(source["DefaultSubtitleStreamIndex"].is_null());
            if available {
                assert_eq!(source["TranscodingSubProtocol"], "hls");
                assert_eq!(source["TranscodingContainer"], "ts");
                assert!(
                    source["TranscodingUrl"]
                        .as_str()
                        .unwrap()
                        .contains("/master.m3u8?")
                );
            } else {
                assert!(source.get("TranscodingSubProtocol").is_none());
                assert!(source.get("TranscodingContainer").is_none());
                assert!(source.get("TranscodingUrl").is_none());
            }
        }
    }

    fn audio_probe(codec: Option<&str>, format: &str) -> ProbeInfo {
        ProbeInfo {
            format_names: vec![format.to_owned()],
            duration_seconds: Some(1.0),
            bit_rate: Some(256_000),
            catalog_identity_matches: true,
            streams: vec![ProbedStream {
                index: 0,
                kind: "audio".to_owned(),
                codec: codec.map(str::to_owned),
                profile: None,
                video_range_type: None,
                language: None,
                title: None,
                is_default: true,
                is_forced: false,
                width: None,
                height: None,
                channels: Some(2),
                sample_rate: Some(44_100),
                bit_rate: Some(256_000),
            }],
        }
    }

    #[test]
    fn hls_copy_and_transcode_urls_preserve_selected_options_and_scoped_auth() {
        let item_id = uuid::Uuid::new_v4();
        let session = uuid::Uuid::new_v4();
        let request = PlaybackInfoRequest {
            start_time_ticks: Some(12_345_678),
            audio_stream_index: Some(-1),
            subtitle_stream_index: Some(4),
            api_key: Some("token+/=".to_owned()),
            ..PlaybackInfoRequest::default()
        };
        let copied = build_hls_url(HlsUrlOptions {
            item_id,
            play_session_id: &session.to_string(),
            request: &request,
            video: true,
            output_audio: false,
            output_channels: None,
            stream_copy: true,
            full_timeline: false,
            max_streaming_bitrate: Some(1_000_000),
        });
        assert_eq!(
            copied,
            format!(
                "/Videos/{item_id}/master.m3u8?playSessionId={session}&streamCopy=true&audioStreamIndex=-1&subtitleStreamIndex=4&maxStreamingBitrate=1000000&ApiKey=token%2B%2F%3D"
            )
        );
        let transcode_request = PlaybackInfoRequest {
            audio_stream_index: Some(2),
            ..request
        };
        let transcoded = build_hls_url(HlsUrlOptions {
            item_id,
            play_session_id: &session.to_string(),
            request: &transcode_request,
            video: true,
            output_audio: true,
            output_channels: Some(1),
            stream_copy: false,
            full_timeline: true,
            max_streaming_bitrate: Some(800_000),
        });
        assert!(transcoded.contains("fullTimeline=true"));
        assert!(!transcoded.contains("StartTimeTicks="));
        assert!(transcoded.contains("audioStreamIndex=2"));
        assert!(transcoded.contains("maxAudioChannels=1"));
        assert!(transcoded.ends_with("ApiKey=token%2B%2F%3D"));
        assert!(!copied.contains("StartTimeTicks="));
    }

    #[test]
    fn source_selection_prefers_the_same_explicit_default_for_playback_and_hls() {
        let streams = vec![
            ProbedStream {
                index: 0,
                kind: "video".to_owned(),
                codec: Some("hevc".to_owned()),
                profile: None,
                video_range_type: None,
                language: None,
                title: None,
                is_default: false,
                is_forced: false,
                width: Some(1920),
                height: Some(1080),
                channels: None,
                sample_rate: None,
                bit_rate: Some(1_000_000),
            },
            ProbedStream {
                index: 2,
                kind: "video".to_owned(),
                codec: Some("h264".to_owned()),
                profile: None,
                video_range_type: None,
                language: None,
                title: None,
                is_default: true,
                is_forced: false,
                width: Some(1280),
                height: Some(720),
                channels: None,
                sample_rate: None,
                bit_rate: Some(500_000),
            },
        ];
        let selected = super::super::probe::default_stream(&streams, "video").unwrap();
        assert_eq!(selected.index, 2);
        assert_eq!(selected.codec.as_deref(), Some("h264"));
    }

    #[test]
    fn direct_play_requires_matching_container_and_every_audio_video_codec() {
        let profile = DirectPlayProfile {
            kind: Some("Video".to_owned()),
            container: Some("mp4,m4v".to_owned()),
            video_codec: Some("h264".to_owned()),
            audio_codec: Some("aac,mp3".to_owned()),
        };
        let media = ProbeInfo {
            format_names: vec!["mov".to_owned(), "mp4".to_owned()],
            duration_seconds: Some(20.0),
            bit_rate: Some(1_000_000),
            catalog_identity_matches: true,
            streams: vec![
                ProbedStream {
                    index: 0,
                    kind: "video".to_owned(),
                    codec: Some("h264".to_owned()),
                    profile: None,
                    video_range_type: None,
                    language: None,
                    title: None,
                    is_default: true,
                    is_forced: false,
                    width: Some(1280),
                    height: Some(720),
                    channels: None,
                    sample_rate: None,
                    bit_rate: None,
                },
                ProbedStream {
                    index: 1,
                    kind: "audio".to_owned(),
                    codec: Some("aac".to_owned()),
                    profile: None,
                    video_range_type: None,
                    language: None,
                    title: None,
                    is_default: true,
                    is_forced: false,
                    width: None,
                    height: None,
                    channels: Some(2),
                    sample_rate: Some(48000),
                    bit_rate: None,
                },
            ],
        };
        assert!(profile_matches(&profile, "mp4", &media, true, false));
        assert!(!profile_matches(&profile, "mkv", &media, true, false));
        let mut incompatible = media.clone();
        incompatible.streams[1].codec = Some("dts".to_owned());
        assert!(!profile_matches(
            &profile,
            "mp4",
            &incompatible,
            true,
            false
        ));
    }

    #[test]
    fn type_only_audio_direct_profile_requires_known_probe_format_and_codec() {
        let profile = DirectPlayProfile {
            kind: Some("Audio".to_owned()),
            ..DirectPlayProfile::default()
        };
        let flac = audio_probe(Some("flac"), "flac");
        assert!(profile_matches(&profile, "flac", &flac, false, true));
        let video_dovi_constraint = DeviceProfile {
            codec_profiles: vec![CodecProfile {
                kind: Some("Video".to_owned()),
                codec: Some("h264".to_owned()),
                conditions: vec![serde_json::json!({
                    "Condition": "NotEquals",
                    "Property": "VideoRangeType",
                    "Value": "DOVI"
                })],
            }],
            ..DeviceProfile::default()
        };
        assert!(
            !has_unhandled_codec_constraints(&video_dovi_constraint, &flac),
            "a video-only source condition cannot veto an audio-only item"
        );

        let conditioned_profile = DeviceProfile {
            direct_play_profiles: vec![DirectPlayProfile {
                kind: Some("Audio".to_owned()),
                ..DirectPlayProfile::default()
            }],
            codec_profiles: vec![CodecProfile {
                kind: Some("Audio".to_owned()),
                codec: Some("flac".to_owned()),
                conditions: vec![serde_json::json!({
                    "Condition": "NotEquals",
                    "Property": "AudioChannels",
                    "Value": "2"
                })],
            }],
            ..DeviceProfile::default()
        };
        assert!(profile_matches(
            &conditioned_profile.direct_play_profiles[0],
            "flac",
            &flac,
            false,
            true
        ));
        assert!(has_unhandled_codec_constraints(&conditioned_profile, &flac));

        assert!(!profile_matches(
            &profile,
            "flac",
            &audio_probe(None, "flac"),
            false,
            true
        ));
        assert!(!profile_matches(&profile, "mkv", &flac, false, true));
        assert!(!profile_matches(&profile, "unknown", &flac, false, true));
    }

    #[test]
    fn universal_audio_container_profiles_accept_flac_and_keep_codec_restrictions() {
        let metadata = audio_probe(Some("flac"), "flac");
        for (container, codec, expected) in [
            ("flac", None, true),
            ("flac", Some("aac"), false),
            ("mp3", None, false),
            ("unknown", None, false),
        ] {
            let request = super::audio_direct_request(
                vec![(container.to_owned(), codec.map(str::to_owned))],
                None,
                None,
            );
            let profile = request.device_profile.unwrap();
            assert_eq!(
                profile_matches(
                    &profile.direct_play_profiles[0],
                    "flac",
                    &metadata,
                    false,
                    true
                ),
                expected,
            );
            assert_eq!(request.enable_transcoding, Some(false));
            assert_eq!(request.enable_direct_stream, Some(false));
        }
    }

    #[test]
    fn video_range_conditions_allow_proven_sdr_and_reject_unknown_or_failed_constraints() {
        let mut media = audio_probe(Some("h264"), "mp4");
        let stream = &mut media.streams[0];
        stream.kind = "video".to_owned();
        let mut condition = serde_json::json!({
            "Condition":"NotEquals", "Property":"VideoRangeType", "Value":"DOVI",
            "IsRequired":false,
        });
        assert!(
            !super::source_condition_matches(&condition, stream),
            "unknown range remains conservative even for an optional condition"
        );
        stream.video_range_type = Some("SDR");
        assert!(super::source_condition_matches(&condition, stream));
        condition["Condition"] = "Equals".into();
        assert!(!super::source_condition_matches(&condition, stream));
        condition["Value"] = "SDR".into();
        assert!(super::source_condition_matches(&condition, stream));
        for (property, operator, wanted) in [
            ("VideoRangeType", "NotEquals", "MisspelledRange"),
            ("VideoRangeType", "GreaterThanEqual", "SDR"),
            ("VideoRangeType", "NotEquals", ""),
            ("UnrecognisedProperty", "NotEquals", "DOVI"),
        ] {
            let unsupported =
                serde_json::json!({"Property":property,"Condition":operator,"Value":wanted});
            assert!(!super::source_condition_matches(&unsupported, stream));
        }
        let profile = DeviceProfile {
            codec_profiles: vec![CodecProfile {
                kind: Some("Video".to_owned()),
                codec: None,
                conditions: vec![
                    serde_json::json!({"Condition":"NotEquals","Property":"VideoRangeType","Value":"DOVI"}),
                ],
            }],
            ..DeviceProfile::default()
        };
        assert!(!has_unhandled_codec_constraints(&profile, &media));
        let mut invalid_profile = profile;
        invalid_profile.codec_profiles[0].kind = Some("Unrecognised".to_owned());
        assert!(has_unhandled_codec_constraints(&invalid_profile, &media));
        invalid_profile.codec_profiles[0].kind = Some("Video".to_owned());
        media.streams[0].video_range_type = None;
        assert!(has_unhandled_codec_constraints(&invalid_profile, &media));
    }

    #[test]
    fn type_only_direct_profile_does_not_weaken_explicit_container_or_codec_mismatches() {
        let flac = audio_probe(Some("flac"), "flac");
        let wrong_container = DirectPlayProfile {
            kind: Some("Audio".to_owned()),
            container: Some("mp3".to_owned()),
            audio_codec: Some("flac".to_owned()),
            ..DirectPlayProfile::default()
        };
        assert!(!profile_matches(
            &wrong_container,
            "flac",
            &flac,
            false,
            true
        ));

        let wrong_codec = DirectPlayProfile {
            kind: Some("Audio".to_owned()),
            container: Some("flac".to_owned()),
            audio_codec: Some("aac".to_owned()),
            ..DirectPlayProfile::default()
        };
        assert!(!profile_matches(&wrong_codec, "flac", &flac, false, true));
    }

    #[test]
    fn unknown_codec_condition_applicability_fails_closed_for_active_streams() {
        let video = ProbeInfo {
            format_names: vec!["matroska".to_owned()],
            duration_seconds: Some(1.0),
            bit_rate: Some(500_000),
            catalog_identity_matches: true,
            streams: vec![ProbedStream {
                index: 0,
                kind: "video".to_owned(),
                codec: None,
                profile: None,
                video_range_type: None,
                language: None,
                title: None,
                is_default: true,
                is_forced: false,
                width: Some(1280),
                height: Some(720),
                channels: None,
                sample_rate: None,
                bit_rate: Some(500_000),
            }],
        };
        for (kind, codec) in [
            (None, Some("h264")),
            (Some("Unrecognized"), Some("h264")),
            (Some("Video"), None),
        ] {
            let profile = DeviceProfile {
                codec_profiles: vec![CodecProfile {
                    kind: kind.map(str::to_owned),
                    codec: codec.map(str::to_owned),
                    conditions: vec![serde_json::json!({
                        "Condition": "NotEquals",
                        "Property": "VideoRangeType",
                        "Value": "DOVI"
                    })],
                }],
                ..DeviceProfile::default()
            };
            assert!(has_unhandled_codec_constraints(&profile, &video));
        }
    }

    #[test]
    fn bitrate_cap_requires_known_rates_for_all_audio_and_video_streams() {
        let media = ProbeInfo {
            format_names: vec!["matroska".to_owned()],
            duration_seconds: Some(20.0),
            bit_rate: None,
            catalog_identity_matches: true,
            streams: vec![
                ProbedStream {
                    index: 0,
                    kind: "video".to_owned(),
                    codec: Some("h264".to_owned()),
                    profile: None,
                    video_range_type: None,
                    language: None,
                    title: None,
                    is_default: true,
                    is_forced: false,
                    width: Some(1280),
                    height: Some(720),
                    channels: None,
                    sample_rate: None,
                    bit_rate: None,
                },
                ProbedStream {
                    index: 1,
                    kind: "audio".to_owned(),
                    codec: Some("aac".to_owned()),
                    profile: None,
                    video_range_type: None,
                    language: None,
                    title: None,
                    is_default: true,
                    is_forced: false,
                    width: None,
                    height: None,
                    channels: Some(2),
                    sample_rate: Some(48000),
                    bit_rate: Some(96_000),
                },
            ],
        };
        assert_eq!(total_bit_rate(&media), None);
        let mut zero_rate = media.clone();
        zero_rate.bit_rate = Some(0);
        assert_eq!(total_bit_rate(&zero_rate), None);
    }

    #[test]
    fn comma_separated_codec_profiles_apply_to_every_matching_codec() {
        let profile = DeviceProfile {
            codec_profiles: vec![CodecProfile {
                kind: Some("Video".to_owned()),
                codec: Some("hevc,h264".to_owned()),
                conditions: vec![
                    serde_json::json!({"Condition":"LessThanEqual","Property":"Width","Value":"640"}),
                ],
            }],
            ..DeviceProfile::default()
        };
        let media = ProbeInfo {
            format_names: vec!["mp4".to_owned()],
            duration_seconds: Some(1.0),
            bit_rate: Some(500_000),
            catalog_identity_matches: true,
            streams: vec![ProbedStream {
                index: 0,
                kind: "video".to_owned(),
                codec: Some("h264".to_owned()),
                profile: None,
                video_range_type: None,
                language: None,
                title: None,
                is_default: true,
                is_forced: false,
                width: Some(1920),
                height: Some(1080),
                channels: None,
                sample_rate: None,
                bit_rate: Some(500_000),
            }],
        };
        assert!(has_unhandled_codec_constraints(&profile, &media));
        assert!(has_unhandled_hls_codec_constraints(&profile));
    }

    #[test]
    fn dovi_source_constraint_blocks_direct_and_copy_but_not_supported_hls_transcode() {
        let output_profile = || TranscodingProfile {
            kind: Some("Video".to_owned()),
            container: Some("ts".to_owned()),
            protocol: Some("hls".to_owned()),
            audio_codec: Some("aac".to_owned()),
            video_codec: Some("h264".to_owned()),
            max_audio_channels: Some("2".to_owned()),
        };
        let mut profile = DeviceProfile {
            direct_play_profiles: vec![DirectPlayProfile {
                kind: Some("Video".to_owned()),
                container: Some("mkv".to_owned()),
                video_codec: Some("h264".to_owned()),
                audio_codec: Some("aac".to_owned()),
            }],
            transcoding_profiles: vec![output_profile()],
            codec_profiles: vec![CodecProfile {
                kind: Some("Video".to_owned()),
                codec: Some("h264".to_owned()),
                conditions: vec![serde_json::json!({
                    "Condition": "NotEquals",
                    "Property": "VideoRangeType",
                    "Value": "DOVI"
                })],
            }],
            ..DeviceProfile::default()
        };
        let source = ProbeInfo {
            format_names: vec!["matroska".to_owned()],
            duration_seconds: Some(1.0),
            bit_rate: Some(500_000),
            catalog_identity_matches: true,
            streams: vec![
                ProbedStream {
                    index: 0,
                    kind: "video".to_owned(),
                    codec: Some("h264".to_owned()),
                    profile: None,
                    video_range_type: None,
                    language: None,
                    title: None,
                    is_default: true,
                    is_forced: false,
                    width: Some(1280),
                    height: Some(720),
                    channels: None,
                    sample_rate: None,
                    bit_rate: Some(500_000),
                },
                ProbedStream {
                    index: 1,
                    kind: "audio".to_owned(),
                    codec: Some("aac".to_owned()),
                    profile: None,
                    video_range_type: None,
                    language: None,
                    title: None,
                    is_default: true,
                    is_forced: false,
                    width: None,
                    height: None,
                    channels: Some(2),
                    sample_rate: Some(48_000),
                    bit_rate: Some(128_000),
                },
            ],
        };

        assert!(profile_matches(
            &profile.direct_play_profiles[0],
            "mkv",
            &source,
            true,
            true
        ));
        assert!(has_unhandled_codec_constraints(&profile, &source));
        assert!(!hls_copy_profile_supported(&profile, true, true, 2));
        assert_eq!(
            hls_transcode_output_channels(&profile, true, true, Some(500_000), None, true, true),
            Some(2)
        );

        profile.transcoding_profiles.clear();
        assert_eq!(
            hls_transcode_output_channels(&profile, true, true, Some(500_000), None, true, true),
            None,
            "a missing HLS output profile must not enable transcoding"
        );

        profile.transcoding_profiles = vec![TranscodingProfile {
            video_codec: Some("hevc".to_owned()),
            ..output_profile()
        }];
        assert_eq!(
            hls_transcode_output_channels(&profile, true, true, Some(500_000), None, true, true),
            None,
            "a mismatched output codec must not enable transcoding"
        );

        profile.transcoding_profiles = vec![output_profile()];
        assert_eq!(
            hls_transcode_output_channels(&profile, true, true, Some(500_000), None, false, true),
            None,
            "an unavailable HLS encoder must not enable transcoding"
        );
    }

    #[test]
    fn hls_channel_limit_is_applied_to_the_output_profile() {
        let profile = TranscodingProfile {
            kind: Some("Video".to_owned()),
            container: Some("ts".to_owned()),
            protocol: Some("hls".to_owned()),
            audio_codec: Some("aac".to_owned()),
            video_codec: Some("h264".to_owned()),
            max_audio_channels: Some("1".to_owned()),
        };
        let channels = hls_profile_channels(&profile, true, None).unwrap();
        assert_eq!(channels, 1);
        assert!(hls_profile_matches(
            &profile,
            true,
            true,
            Some(500_000),
            channels
        ));
        assert_eq!(hls_profile_channels(&profile, true, Some(0)), None);
        assert_eq!(hls_profile_channels(&profile, true, Some(2)), Some(1));
    }

    #[test]
    fn hls_subtitle_capability_accepts_hls_webvtt_and_legacy_external_delivery() {
        let hls_webvtt = DeviceProfile {
            subtitle_profiles: vec![SubtitleProfile {
                format: Some("WebVTT".to_owned()),
                method: Some("Hls".to_owned()),
            }],
            ..DeviceProfile::default()
        };
        assert!(hls_subtitle_profile_supported(&hls_webvtt));

        let external_vtt = DeviceProfile {
            subtitle_profiles: vec![SubtitleProfile {
                format: Some("vtt".to_owned()),
                method: Some("External".to_owned()),
            }],
            ..DeviceProfile::default()
        };
        assert!(hls_subtitle_profile_supported(&external_vtt));

        for subtitle in [
            SubtitleProfile {
                format: Some("srt".to_owned()),
                method: Some("Hls".to_owned()),
            },
            SubtitleProfile {
                format: Some("vtt".to_owned()),
                method: Some("Encode".to_owned()),
            },
            SubtitleProfile {
                format: Some("vtt".to_owned()),
                method: None,
            },
        ] {
            let profile = DeviceProfile {
                subtitle_profiles: vec![subtitle],
                ..DeviceProfile::default()
            };
            assert!(!hls_subtitle_profile_supported(&profile));
        }

        assert!(hls_subtitle_profile_supported(&DeviceProfile::default()));
    }

    #[test]
    fn subtitle_profile_telemetry_uses_only_bounded_format_and_method_categories() {
        let profile = DeviceProfile {
            subtitle_profiles: vec![
                SubtitleProfile {
                    format: Some("vtt".to_owned()),
                    method: Some("Hls".to_owned()),
                },
                SubtitleProfile {
                    format: Some("WebVTT".to_owned()),
                    method: Some("External".to_owned()),
                },
                SubtitleProfile {
                    format: Some("vtt".to_owned()),
                    method: Some("Encode".to_owned()),
                },
                SubtitleProfile {
                    format: Some("srt".to_owned()),
                    method: Some("Hls".to_owned()),
                },
                SubtitleProfile {
                    format: Some("SubRip".to_owned()),
                    method: Some("External".to_owned()),
                },
                SubtitleProfile {
                    format: Some("fixture-format-value".to_owned()),
                    method: Some("fixture-method-value".to_owned()),
                },
                SubtitleProfile {
                    format: Some("SSA".to_owned()),
                    method: Some("Embed".to_owned()),
                },
                SubtitleProfile {
                    format: Some("PGS".to_owned()),
                    method: Some("Drop".to_owned()),
                },
                SubtitleProfile {
                    format: None,
                    method: None,
                },
            ],
            ..DeviceProfile::default()
        };
        let summary = SubtitleProfileSummary::for_profile(Some(&profile));
        assert_eq!(
            summary,
            SubtitleProfileSummary {
                vtt_hls: 1,
                vtt_external: 1,
                vtt_other_method: 1,
                hls_other_format: 1,
                srt_external: 1,
                format_vtt: 3,
                format_srt: 2,
                format_ass: 1,
                format_bitmap: 1,
                format_other: 1,
                format_missing: 1,
                method_hls: 2,
                method_external: 2,
                method_embed: 1,
                method_encode: 1,
                method_drop: 1,
                method_other: 1,
                method_missing: 1,
            }
        );
        let summary_text = format!("{summary:?}");
        assert!(!summary_text.contains("fixture-format-value"));
        assert!(!summary_text.contains("fixture-method-value"));
    }

    #[test]
    fn stream_type_wire_names_are_public_enum_values() {
        let stream = ProbedStream {
            index: 0,
            kind: "attachment".to_owned(),
            codec: Some("mjpeg".to_owned()),
            profile: None,
            video_range_type: None,
            language: None,
            title: None,
            is_default: false,
            is_forced: false,
            width: None,
            height: None,
            channels: None,
            sample_rate: None,
            bit_rate: None,
        };
        assert_eq!(
            MediaStream::from_probe(&stream, uuid::Uuid::nil(), false).kind,
            "Data"
        );
    }

    #[test]
    fn text_subtitles_advertise_the_safe_delivery_route() {
        let stream = ProbedStream {
            index: 3,
            kind: "subtitle".to_owned(),
            codec: Some("subrip".to_owned()),
            profile: None,
            video_range_type: None,
            language: Some("en".to_owned()),
            title: None,
            is_default: false,
            is_forced: false,
            width: None,
            height: None,
            channels: None,
            sample_rate: None,
            bit_rate: None,
        };
        let item = uuid::Uuid::new_v4();
        let wire = MediaStream::from_probe(&stream, item, true);
        assert!(wire.is_text_subtitle_stream);
        assert!(wire.supports_external_stream);
        assert_eq!(wire.delivery_method.as_deref(), Some("Encode"));
        assert_eq!(
            wire.delivery_url.as_deref(),
            Some(format!("/Videos/{item}/{item}/Subtitles/3/Stream.vtt").as_str())
        );
        let unavailable = MediaStream::from_probe(&stream, item, false);
        assert!(unavailable.is_text_subtitle_stream);
        assert!(!unavailable.supports_external_stream);
    }

    #[test]
    fn profile_tokens_are_split_without_loose_substring_matches() {
        assert!(csv_contains("mp4, m4v", "m4v"));
        assert!(!csv_contains("mp4,m4v", "mp"));
        assert!(codec_list_matches(Some("h264,hevc"), Some("hevc")));
        assert!(!codec_list_matches(Some("h264"), Some("h264_videotoolbox")));
    }

    #[test]
    fn playback_profile_diagnostics_keep_values_out_of_facts() {
        let request: PlaybackInfoRequest = serde_json::from_value(serde_json::json!({
            "DeviceProfile": {
                "DirectPlayProfiles": [{
                    "Type": "Audio",
                    "Container": "fixture-container-value",
                    "AudioCodec": "fixture-audio-codec-value"
                }],
                "TranscodingProfiles": [{
                    "Type": "Audio",
                    "Container": "ts",
                    "Protocol": "HLS",
                    "AudioCodec": "aac"
                }],
                "CodecProfiles": [{
                    "Type": "Audio",
                    "Codec": "fixture-profile-value",
                    "Conditions": [{
                        "Condition": "LessThanEqual",
                        "Property": "VideoBitDepth",
                        "Value": "fixture-condition-value"
                    }]
                }],
                "ContainerProfiles": [{"Value": "fixture-container-profile-value"}],
                "SubtitleProfiles": [
                    {"Format": "fixture-subtitle-value", "Method": "External"},
                    {"Format": "SubRip", "Method": "External"}
                ]
            }
        }))
        .expect("profile request should deserialize");
        let extractable_text_subtitle = MediaStream::from_probe(
            &ProbedStream {
                index: 3,
                kind: "subtitle".to_owned(),
                codec: Some("subrip".to_owned()),
                profile: None,
                video_range_type: None,
                language: None,
                title: None,
                is_default: false,
                is_forced: false,
                width: None,
                height: None,
                channels: None,
                sample_rate: None,
                bit_rate: None,
            },
            uuid::Uuid::nil(),
            true,
        );
        let media_source = MediaSource {
            id: "fixture-item-id".to_owned(),
            name: "fixture-name".to_owned(),
            path: Some("fixture-media-path".to_owned()),
            protocol: "File".to_owned(),
            source_type: "Default".to_owned(),
            container: Some("fixture-response-container".to_owned()),
            size: 1,
            run_time_ticks: None,
            supports_direct_play: false,
            supports_direct_stream: false,
            supports_transcoding: false,
            default_audio_stream_index: None,
            default_subtitle_stream_index: None,
            media_streams: vec![extractable_text_subtitle],
            formats: Vec::new(),
            direct_stream_url: Some("fixture-direct-url-with-secret".to_owned()),
            transcoding_url: Some("fixture-transcode-url-with-secret".to_owned()),
            transcoding_sub_protocol: Some("hls"),
            transcoding_container: Some("ts"),
        };
        let response = PlaybackInfoResponse {
            play_session_id: "fixture-session-id".to_owned(),
            media_sources: vec![media_source],
        };
        let metadata = ProbeInfo {
            format_names: vec!["flac".to_owned()],
            duration_seconds: Some(1.0),
            bit_rate: Some(128_000),
            catalog_identity_matches: true,
            streams: Vec::new(),
        };
        let facts = PlaybackProfileFacts::for_negotiation(
            &request, &response, &metadata, "flac", false, true, true, None, None, true, true, true,
        );

        assert_eq!(facts.direct_profiles, 1);
        assert_eq!(facts.direct_profiles_with_container, 1);
        assert_eq!(facts.transcoding_profiles, 1);
        assert_eq!(facts.hls_profiles, 1);
        assert_eq!(facts.hls_profiles_with_ts, 1);
        assert_eq!(facts.matching_hls_profiles, 1);
        assert_eq!(facts.codec_profiles_with_conditions, 1);
        assert_eq!(facts.codec_condition_count, 1);
        assert_eq!(facts.codec_condition_property_category, "video_bit_depth");
        assert_eq!(facts.codec_condition_operator_category, "less_than_equal");
        assert_eq!(facts.codec_condition_value_category, "text");
        assert!(facts.source_codec_constraints_supported);
        assert_eq!(facts.container_profiles, 1);
        assert_eq!(facts.subtitle_profiles, 2);
        assert_eq!(facts.subtitle_srt_external_profiles, 1);
        assert_eq!(facts.extractable_text_subtitle_streams, 1);
        let facts_text = format!("{facts:?}");
        for private_value in [
            "fixture-container-value",
            "fixture-audio-codec-value",
            "fixture-profile-value",
            "fixture-condition-value",
            "fixture-container-profile-value",
            "fixture-subtitle-value",
            "fixture-item-id",
            "fixture-name",
            "fixture-media-path",
            "fixture-response-container",
            "fixture-direct-url-with-secret",
            "fixture-transcode-url-with-secret",
            "fixture-session-id",
        ] {
            assert!(!facts_text.contains(private_value));
        }
    }

    #[test]
    fn codec_condition_summary_uses_bounded_categories_without_values() {
        let profile: DeviceProfile = serde_json::from_value(serde_json::json!({
            "CodecProfiles": [{
                "Type": "Video",
                "Conditions": [{
                    "Condition": "NotEquals",
                    "Property": "VideoRangeType",
                    "Value": "fixture-condition-value"
                }]
            }]
        }))
        .expect("codec profile should deserialize");

        let summary = codec_condition_summary(Some(&profile));

        assert_eq!(summary.count, 1);
        assert_eq!(summary.property, "video_range_type");
        assert_eq!(summary.operator, "not_equals");
        assert_eq!(summary.value, "text");
        assert!(!format!("{summary:?}").contains("fixture-condition-value"));
    }
}
