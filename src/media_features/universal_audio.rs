//! Direct audio and bounded AAC HLS delivery through the universal-audio resource.

use std::collections::HashSet;

use axum::{
    extract::{Path, RawQuery, State},
    http::{HeaderMap, Method},
    response::Response,
};
use uuid::Uuid;

use crate::{
    ApiError,
    auth::{self, MediaUser},
    db,
    state::AppState,
};

use super::{authorized_media, hls, is_audio_type, playback, stream_resolved};

#[derive(Debug, Default)]
struct AudioRequest {
    containers: Vec<(String, Option<String>)>,
    user_id: Option<Uuid>,
    media_source_id: Option<Uuid>,
    max_streaming_bitrate: Option<u64>,
    max_audio_channels: Option<u32>,
    max_audio_sample_rate: Option<u32>,
    max_audio_bit_depth: Option<u32>,
    start_time_ticks: Option<i64>,
    audio_codecs: Vec<String>,
    transcoding_container: Option<String>,
    transcoding_protocol: Option<String>,
    transcoding_audio_channels: Option<u32>,
    audio_bit_rate: Option<u64>,
    play_session_id: Option<String>,
    api_key: Option<String>,
}

pub(super) async fn stream(
    State(state): State<AppState>,
    MediaUser(user): MediaUser,
    Path(item_id): Path<Uuid>,
    RawQuery(raw): RawQuery,
    method: Method,
    headers: HeaderMap,
) -> Result<Response, ApiError> {
    let request = parse_request(raw.as_deref())?;
    if request.user_id.is_some_and(|id| id != user.id) {
        return Err(ApiError::Forbidden);
    }
    if request.media_source_id.is_some_and(|id| id != item_id) {
        return Err(ApiError::NotFound);
    }
    let media = authorized_media(&state, &user, item_id).await?;
    if !is_audio_type(&media.item.item_type) {
        return Err(ApiError::NotFound);
    }
    let negotiation = playback::negotiate(
        &state,
        media.clone(),
        playback::audio_direct_request(
            request.containers.clone(),
            request.max_streaming_bitrate,
            request.max_audio_channels,
            request.max_audio_sample_rate,
            request.max_audio_bit_depth,
        ),
    )
    .await?;
    // Original bytes preserve the item's timeline. A compatible client seeks
    // to its saved position using byte ranges, just as it does for /stream.
    if negotiation.supports_direct_play() {
        return stream_resolved(media, &headers, &method, false).await;
    }
    let raw_session_id = request.play_session_id.clone();
    let query_token = request.api_key.clone();
    let mut options = transcode_options(request)?;
    if let Some(id) = raw_session_id.filter(|id| !id.is_empty()) {
        options.play_session_id = if let Ok(uuid) = id.parse() {
            Some(uuid)
        } else {
            let token = auth::extract_raw_token(&headers)?
                .map(|(token, _)| token)
                .or(query_token)
                .ok_or(ApiError::Unauthorized)?;
            let device_id =
                db::media_auth_device_id(&state.db, user.id, &auth::token_digest(&token))
                    .await?
                    .ok_or(ApiError::Unauthorized)?;
            crate::api::parse_play_session_id(Some(&id), state.run_id, user.id, &device_id)?
        };
    }
    if method == Method::HEAD {
        hls::head_master(state, user, item_id, options, hls::MediaKind::Audio).await
    } else {
        hls::start_master(state, user, item_id, options, hls::MediaKind::Audio).await
    }
}

fn transcode_options(request: AudioRequest) -> Result<hls::HlsOptions, ApiError> {
    if request.transcoding_protocol.as_deref() != Some("hls")
        || !matches!(request.transcoding_container.as_deref(), Some("mp4" | "ts"))
        || !request.audio_codecs.iter().any(|codec| codec == "aac")
        || request.max_audio_bit_depth.is_some()
        || request
            .max_streaming_bitrate
            .is_some_and(|rate| rate < 32_000)
    {
        return Err(ApiError::BadRequest("The source needs conversion; supported universal audio output is AAC HLS in mp4 or ts without a bit-depth constraint".to_owned()));
    }
    let channels = request
        .transcoding_audio_channels
        .unwrap_or_else(|| request.max_audio_channels.unwrap_or(2).min(2));
    if !(1..=2).contains(&channels)
        || request
            .max_audio_channels
            .is_some_and(|maximum| channels > maximum)
        || request.audio_bit_rate.is_some_and(|rate| {
            !(16_000..=320_000).contains(&rate)
                || rate
                    > request
                        .max_streaming_bitrate
                        .unwrap_or(2_000_000)
                        .min(8_000_000)
                        .saturating_mul(95)
                        / 100
        })
    {
        return Err(invalid());
    }
    let sample_rate = [
        48_000, 44_100, 32_000, 24_000, 22_050, 16_000, 12_000, 11_025, 8_000,
    ]
    .into_iter()
    .find(|rate| *rate <= request.max_audio_sample_rate.unwrap_or(48_000))
    .ok_or_else(invalid)?;
    Ok(hls::HlsOptions {
        start_time_ticks: request.start_time_ticks,
        max_streaming_bitrate: request.max_streaming_bitrate,
        max_audio_channels: Some(channels),
        audio_bit_rate: request.audio_bit_rate,
        audio_sample_rate: Some(sample_rate),
        audio_fmp4: request.transcoding_container.as_deref() == Some("mp4"),
        audio_full_timeline: true,
        api_key: request.api_key,
        ..Default::default()
    })
}

#[cfg(test)]
pub(super) fn transcode_options_for_test(query: &str) -> hls::HlsOptions {
    transcode_options(parse_request(Some(query)).unwrap()).unwrap()
}

fn invalid() -> ApiError {
    // Query values can contain credentials; never include them in errors.
    ApiError::BadRequest("Invalid or unsupported universal audio options".to_owned())
}

fn token(value: &str) -> Result<(), ApiError> {
    if value.is_empty()
        || value.len() > 40
        || !value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-' | b'.'))
    {
        return Err(invalid());
    }
    Ok(())
}

fn positive(value: &str, max: u64) -> Result<u64, ApiError> {
    let number = value.parse::<u64>().map_err(|_| invalid())?;
    if number == 0 || number > max {
        return Err(invalid());
    }
    Ok(number)
}

fn parse_request(raw: Option<&str>) -> Result<AudioRequest, ApiError> {
    let raw = raw.unwrap_or_default();
    if raw.len() > 8192 {
        return Err(invalid());
    }
    let mut request = AudioRequest::default();
    let mut seen = HashSet::new();
    for (name, value) in url::form_urlencoded::parse(raw.as_bytes()) {
        if seen.len() >= 32
            || !seen.insert(name.to_ascii_lowercase())
            || value.chars().any(char::is_control)
        {
            return Err(invalid());
        }
        match name.to_ascii_lowercase().as_str() {
            "container" => {
                for entry in value.split(',') {
                    if request.containers.len() >= 32 {
                        return Err(invalid());
                    }
                    let mut parts = entry.split('|');
                    let container = parts.next().ok_or_else(invalid)?;
                    token(container)?;
                    let codec = parts.next();
                    if let Some(codec) = codec {
                        token(codec)?;
                    }
                    if parts.next().is_some() {
                        return Err(invalid());
                    }
                    request.containers.push((
                        container.to_ascii_lowercase(),
                        codec.map(str::to_ascii_lowercase),
                    ));
                }
            }
            "userid" => request.user_id = Some(value.parse().map_err(|_| invalid())?),
            "mediasourceid" => {
                request.media_source_id = Some(value.parse().map_err(|_| invalid())?);
            }
            "maxstreamingbitrate" => {
                request.max_streaming_bitrate = Some(positive(&value, i32::MAX as u64)?);
            }
            "maxaudiochannels" => {
                request.max_audio_channels = Some(positive(&value, 32)? as u32);
            }
            "maxaudiosamplerate" => {
                request.max_audio_sample_rate = Some(positive(&value, i32::MAX as u64)? as u32);
            }
            "maxaudiobitdepth" => {
                request.max_audio_bit_depth = Some(positive(&value, i32::MAX as u64)? as u32);
            }
            "starttimeticks" => {
                let ticks = value.parse::<i64>().map_err(|_| invalid())?;
                if ticks < 0 {
                    return Err(invalid());
                }
                request.start_time_ticks = Some(ticks);
            }
            "audiocodec" => {
                if value.split(',').count() > 32 {
                    return Err(invalid());
                }
                for codec in value.split(',') {
                    token(codec)?;
                    request.audio_codecs.push(codec.to_ascii_lowercase());
                }
            }
            "transcodingcontainer" => {
                token(&value)?;
                request.transcoding_container = Some(value.to_ascii_lowercase());
            }
            "transcodingprotocol" => {
                if !matches!(value.as_ref(), "http" | "hls") {
                    return Err(invalid());
                }
                request.transcoding_protocol = Some(value.into_owned());
            }
            "transcodingaudiochannels" => {
                request.transcoding_audio_channels = Some(positive(&value, 32)? as u32);
            }
            "audiobitrate" => {
                request.audio_bit_rate = Some(positive(&value, i32::MAX as u64)?);
            }
            "enableremotemedia" | "enableaudiovbrencoding" | "enableredirection" => {
                if !value.eq_ignore_ascii_case("true") && !value.eq_ignore_ascii_case("false") {
                    return Err(invalid());
                }
            }
            "apikey" | "deviceid" | "playsessionid" => {
                if value.trim().is_empty() || value.len() > 512 {
                    return Err(invalid());
                }
                if name.eq_ignore_ascii_case("apikey") {
                    request.api_key = Some(value.into_owned());
                } else if name.eq_ignore_ascii_case("playsessionid") {
                    request.play_session_id = Some(value.into_owned());
                }
            }
            _ => return Err(invalid()),
        }
    }
    if request.containers.is_empty() {
        return Err(invalid());
    }
    Ok(request)
}

#[cfg(test)]
mod tests {
    use super::{parse_request, transcode_options};

    #[test]
    fn conversion_requires_an_explicit_supported_output_and_respects_limits() {
        let query = "Container=mp3&TranscodingProtocol=hls&TranscodingContainer=mp4&AudioCodec=mp3,aac&MaxAudioChannels=1&MaxAudioSampleRate=44099&AudioBitRate=64000&MaxStreamingBitrate=96000&StartTimeTicks=12345678";
        let options = transcode_options(parse_request(Some(query)).unwrap()).unwrap();
        assert!(options.audio_fmp4);
        assert_eq!(options.audio_sample_rate, Some(32_000));
        assert_eq!(options.audio_bit_rate, Some(64_000));
        assert_eq!(options.max_audio_channels, Some(1));
        assert_eq!(options.start_time_ticks, Some(12_345_678));
        for extra in [
            "TranscodingAudioChannels=2",
            "MaxAudioBitDepth=16",
            "EnableRemoteMedia=invalid",
        ] {
            assert!(
                parse_request(Some(&format!("{query}&{extra}")))
                    .and_then(transcode_options)
                    .is_err()
            );
        }
        for changed in [
            query.replace("hls", "http"),
            query.replace("mp4", "webm"),
            query.replace("mp3,aac", "mp3"),
            query.replace("96000", "32000"),
            query.replace("44099", "7999"),
        ] {
            assert!(
                parse_request(Some(&changed))
                    .and_then(transcode_options)
                    .is_err()
            );
        }
    }

    #[test]
    fn accepts_audio_sample_rate_and_bit_depth_limits() {
        for query in [
            "Container=flac&MaxAudioSampleRate=44100",
            "Container=flac&MaxAudioBitDepth=16",
            "Container=flac&MaxAudioSampleRate=48000&MaxAudioBitDepth=24",
        ] {
            assert!(parse_request(Some(query)).is_ok(), "rejected {query}");
        }
        let request = parse_request(Some(
            "Container=flac&MaxAudioSampleRate=48000&MaxAudioBitDepth=24",
        ))
        .unwrap();
        assert_eq!(request.max_audio_sample_rate, Some(48_000));
        assert_eq!(request.max_audio_bit_depth, Some(24));
    }

    #[test]
    fn accepts_observed_official_web_audio_options() {
        let request = parse_request(Some(
            "MaxStreamingBitrate=1911466591&Container=opus,webm%7Copus,ts%7Cmp3,mp3,aac,m4a%7Caac,m4b%7Caac,flac,webma,webm%7Cwebma,wav,ogg&TranscodingContainer=mp4&TranscodingProtocol=hls&AudioCodec=aac&StartTimeTicks=0&EnableRedirection=true&EnableRemoteMedia=false&EnableAudioVbrEncoding=true&DeviceId=browser&PlaySessionId=1790913876274&ApiKey=synthetic",
        ))
        .unwrap();
        assert_eq!(request.containers.len(), 12);
        assert_eq!(
            request.containers[1],
            ("webm".to_owned(), Some("opus".to_owned()))
        );
        assert_eq!(request.max_streaming_bitrate, Some(1_911_466_591));
    }

    #[test]
    fn rejects_ambiguous_or_unsupported_source_selection_and_limits() {
        for query in [
            "",
            "Container=",
            "Container=flac&container=mp3",
            "Container=flac%7Caac%7Copus",
            "Container=flac&MaxStreamingBitrate=0",
            "Container=flac&MaxAudioChannels=33",
            "Container=flac&StartTimeTicks=-1",
            "Container=flac&MaxAudioSampleRate=0",
            "Container=flac&MaxAudioSampleRate=-1",
            "Container=flac&MaxAudioSampleRate=2147483648",
            "Container=flac&MaxAudioBitDepth=0",
            "Container=flac&MaxAudioBitDepth=16.5",
            "Container=flac&MaxAudioBitDepth=2147483648",
            "Container=flac&MaxAudioBitDepth=16&maxaudiobitdepth=24",
            "Container=flac&DeviceId=x%0Ay",
            "Container=flac&EnableRemoteMedia=maybe",
            "Container=flac&UnknownOption=1",
            "Container=flac&UserId=bad",
        ] {
            assert!(parse_request(Some(query)).is_err(), "accepted {query}");
        }
        assert!(parse_request(Some(&format!("Container={}", vec!["flac"; 33].join(",")))).is_err());
    }
}
