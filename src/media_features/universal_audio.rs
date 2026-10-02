//! Direct audio delivery through the public universal-audio resource.

use std::collections::HashSet;

use axum::{
    extract::{Path, RawQuery, State},
    http::{HeaderMap, Method},
    response::Response,
};
use uuid::Uuid;

use crate::{ApiError, auth::MediaUser, state::AppState};

use super::{authorized_media, is_audio_type, playback, stream_resolved};

#[derive(Debug, Default)]
struct AudioRequest {
    containers: Vec<(String, Option<String>)>,
    user_id: Option<Uuid>,
    media_source_id: Option<Uuid>,
    max_streaming_bitrate: Option<u64>,
    max_audio_channels: Option<u32>,
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
            request.containers,
            request.max_streaming_bitrate,
            request.max_audio_channels,
        ),
    )
    .await?;
    if !negotiation.supports_direct_play() {
        return Err(ApiError::BadRequest(
            "The source does not fit the requested direct audio formats and limits; universal audio transcoding is not available".to_owned(),
        ));
    }
    stream_resolved(media, &headers, &method, false).await
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
            "starttimeticks" => {
                if value.parse::<i64>().map_err(|_| invalid())? != 0 {
                    return Err(invalid());
                }
            }
            "audiocodec" => {
                if value.split(',').count() > 32 {
                    return Err(invalid());
                }
                for codec in value.split(',') {
                    token(codec)?;
                }
            }
            "transcodingcontainer" => token(&value)?,
            "transcodingprotocol" => {
                if !matches!(value.as_ref(), "http" | "hls") {
                    return Err(invalid());
                }
            }
            "transcodingaudiochannels" => {
                positive(&value, 32)?;
            }
            "audiobitrate" => {
                positive(&value, i32::MAX as u64)?;
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
            }
            // These source limits need probe fields and negotiation rules that
            // are not implemented yet. Do not silently ignore them.
            "maxaudiosamplerate" | "maxaudiobitdepth" => return Err(invalid()),
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
    use super::parse_request;

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
            "Container=flac&StartTimeTicks=1",
            "Container=flac&MaxAudioSampleRate=48000",
            "Container=flac&MaxAudioBitDepth=16",
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
