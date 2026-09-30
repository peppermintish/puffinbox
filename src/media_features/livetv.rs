//! Bounded parsers for administrator-configured IPTV and XMLTV feeds.
//!
//! Live TV feed parsing and input transport. The HTTP API accepts only
//! administrator-pinned origins; network data is fetched by this process and
//! passed as bytes to an isolated local decoder.

use std::{
    collections::{BTreeMap, HashMap, HashSet},
    net::{IpAddr, SocketAddr},
    time::{Duration, Instant},
};

use chrono::{DateTime, FixedOffset, Utc};
use futures_util::StreamExt;
use quick_xml::{Reader, events::Event};
use serde::{Deserialize, Serialize};
use tokio::io::AsyncWriteExt;
use url::Url;

const MAX_M3U_BYTES: usize = 4 * 1024 * 1024;
const MAX_M3U_LINE_BYTES: usize = 16 * 1024;
const MAX_M3U_CHANNELS: usize = 10_000;
const MAX_M3U_ATTRIBUTES: usize = 32;
const MAX_ATTRIBUTE_BYTES: usize = 2 * 1024;
const MAX_CHANNEL_ID_BYTES: usize = 256;
const MAX_CHANNEL_NAME_BYTES: usize = 512;
const MAX_XMLTV_BYTES: usize = 16 * 1024 * 1024;
const MAX_XMLTV_EVENTS: usize = 500_000;
const MAX_XMLTV_DEPTH: usize = 32;
const MAX_XMLTV_ATTRIBUTES: usize = 32;
const MAX_XMLTV_ATTRIBUTE_BYTES: usize = 4 * 1024;
const MAX_XMLTV_CHANNELS: usize = 10_000;
const MAX_XMLTV_PROGRAMS: usize = 100_000;
const MAX_XMLTV_TEXT_BYTES: usize = 16 * 1024;
const MAX_PINNED_ORIGINS: usize = 32;
const MAX_PINNED_ADDRESSES: usize = 16;
const FEED_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const FEED_TOTAL_TIMEOUT: Duration = Duration::from_secs(30);
const LIVE_CONNECT_TIMEOUT: Duration = Duration::from_secs(5);
const LIVE_READ_TIMEOUT: Duration = Duration::from_secs(30);
const MAX_HLS_MANIFEST_BYTES: usize = 1024 * 1024;
const MAX_HLS_SEGMENT_BYTES: u64 = 64 * 1024 * 1024;
const MAX_HLS_SEGMENTS: usize = 2_000;
const MAX_HLS_VARIANTS: usize = 128;
pub(super) const MAX_LIVE_INPUT_BYTES: u64 = 32 * 1024 * 1024 * 1024;
pub(super) const MAX_LIVE_INPUT_SECONDS: Duration = Duration::from_secs(4 * 60 * 60);

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct M3uChannel {
    pub source_id: Option<String>,
    pub name: String,
    pub group: Option<String>,
    pub logo_url: Option<Url>,
    pub stream_url: Url,
}

struct ParsedExtinf {
    source_id: Option<String>,
    name: String,
    group: Option<String>,
    logo_text: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct XmltvChannel {
    pub id: String,
    pub display_name: String,
    pub icon_url: Option<Url>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(super) struct XmltvProgram {
    pub channel_id: String,
    pub start: DateTime<Utc>,
    pub stop: DateTime<Utc>,
    pub title: String,
    pub description: Option<String>,
    pub category: Option<String>,
    pub rating_system: Option<String>,
    pub content_rating: Option<String>,
    pub policy_rating_value: Option<i16>,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(super) struct OriginPin {
    pub origin: String,
    pub addresses: Vec<IpAddr>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum FeedError {
    TooLarge,
    Unavailable,
    InvalidEncoding,
    InvalidStructure,
    InvalidValue,
    DisallowedOrigin,
    LimitExceeded,
    DuplicateId,
    UnsupportedTransport,
    QuotaExceeded,
    Cancelled,
}

#[derive(Clone, Debug)]
struct HlsSegment {
    sequence: u64,
    url: Url,
}

#[derive(Clone, Debug)]
pub(super) struct HlsMediaPlaylist {
    target_duration: Duration,
    segments: Vec<HlsSegment>,
    end_list: bool,
}

#[derive(Clone, Debug)]
struct HlsVariant {
    bandwidth: u64,
    url: Url,
}

enum ParsedHlsPlaylist {
    Master(Vec<HlsVariant>),
    Media(HlsMediaPlaylist),
}

/// A server-fetched input source. HLS is restricted to unencrypted MPEG-TS
/// segments; no manifest, key, or segment URL is ever given to FFmpeg.
pub(super) enum LiveInput {
    Direct(reqwest::Response),
    Hls {
        client: reqwest::Client,
        pins: Vec<OriginPin>,
        playlist_url: Url,
        playlist: HlsMediaPlaylist,
        last_sequence: Option<u64>,
    },
}

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
struct Origin {
    scheme: String,
    host: String,
    port: u16,
}

impl Origin {
    fn of(url: &Url) -> Option<Self> {
        if !matches!(url.scheme(), "http" | "https")
            || !url.username().is_empty()
            || url.password().is_some()
            || url.host_str().is_none()
        {
            return None;
        }
        Some(Self {
            scheme: url.scheme().to_owned(),
            host: url.host_str()?.to_ascii_lowercase(),
            port: url.port_or_known_default()?,
        })
    }
}

impl OriginPin {
    fn parsed(&self) -> Result<(Url, Vec<SocketAddr>), FeedError> {
        if self.addresses.is_empty() || self.addresses.len() > MAX_PINNED_ADDRESSES {
            return Err(FeedError::LimitExceeded);
        }
        let url = Url::parse(&self.origin).map_err(|_| FeedError::InvalidValue)?;
        if url.path() != "/"
            || url.query().is_some()
            || url.fragment().is_some()
            || url.port() == Some(0)
            || Origin::of(&url).is_none()
        {
            return Err(FeedError::InvalidValue);
        }
        let origin = Origin::of(&url).ok_or(FeedError::InvalidValue)?;
        let literal_ip = origin.host.parse::<IpAddr>().ok();
        let mut seen = HashSet::new();
        let mut addresses = Vec::with_capacity(self.addresses.len());
        for address in &self.addresses {
            if address.is_unspecified()
                || address.is_multicast()
                || matches!(address, IpAddr::V4(ip) if ip.is_broadcast())
                || !seen.insert(*address)
            {
                return Err(FeedError::InvalidValue);
            }
            if literal_ip.is_some_and(|literal| literal != *address) {
                return Err(FeedError::InvalidValue);
            }
            addresses.push(SocketAddr::new(*address, 0));
        }
        Ok((url, addresses))
    }
}

pub(super) fn approved_urls(pins: &[OriginPin]) -> Result<Vec<Url>, FeedError> {
    if pins.is_empty() || pins.len() > MAX_PINNED_ORIGINS {
        return Err(FeedError::LimitExceeded);
    }
    let mut seen = HashSet::new();
    let mut urls = Vec::with_capacity(pins.len());
    for pin in pins {
        let (url, _) = pin.parsed()?;
        let origin = Origin::of(&url).ok_or(FeedError::InvalidValue)?;
        if !seen.insert(origin) {
            return Err(FeedError::DuplicateId);
        }
        urls.push(url);
    }
    Ok(urls)
}

/// Fetch one configured feed without consulting process proxy settings or
/// resolving names again. Redirects are rejected; every destination must be
/// an administrator-pinned origin and address.
pub(super) async fn fetch_bounded_feed(
    raw_url: &str,
    pins: &[OriginPin],
    maximum_bytes: usize,
) -> Result<Vec<u8>, FeedError> {
    if raw_url.len() > 8 * 1024 || maximum_bytes == 0 || maximum_bytes > MAX_XMLTV_BYTES {
        return Err(FeedError::LimitExceeded);
    }
    let url = Url::parse(raw_url).map_err(|_| FeedError::InvalidValue)?;
    let approved = approved_urls(pins)?;
    if url.fragment().is_some() || !approved_origin(&url, &approved) {
        return Err(FeedError::DisallowedOrigin);
    }

    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(FEED_CONNECT_TIMEOUT)
        .timeout(FEED_TOTAL_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none());
    for pin in pins {
        let (origin_url, addresses) = pin.parsed()?;
        let host = origin_url.host_str().ok_or(FeedError::InvalidValue)?;
        builder = builder.resolve_to_addrs(host, &addresses);
    }
    let client = builder.build().map_err(|_| FeedError::Unavailable)?;
    let response = client
        .get(url)
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .send()
        .await
        .map_err(|_| FeedError::Unavailable)?;
    if response.status().is_redirection() {
        return Err(FeedError::DisallowedOrigin);
    }
    if !response.status().is_success() {
        return Err(FeedError::Unavailable);
    }
    if response
        .headers()
        .get(reqwest::header::CONTENT_ENCODING)
        .is_some_and(|value| value.as_bytes() != b"identity")
    {
        return Err(FeedError::InvalidStructure);
    }
    if response
        .content_length()
        .is_some_and(|length| length > maximum_bytes as u64)
    {
        return Err(FeedError::TooLarge);
    }
    let mut response = response.bytes_stream();
    let mut output = Vec::new();
    while let Some(chunk) = response.next().await {
        let chunk = chunk.map_err(|_| FeedError::Unavailable)?;
        if output.len().saturating_add(chunk.len()) > maximum_bytes {
            return Err(FeedError::TooLarge);
        }
        output.extend_from_slice(&chunk);
    }
    Ok(output)
}

/// Open one direct MPEG-TS HTTP source for a supervised live worker. Unlike
/// metadata fetches this response has no total-body timeout; callers enforce
/// an idle read timeout, recording/session duration, byte quota, cancellation,
/// and backpressure while forwarding chunks to the isolated decoder's stdin.
/// HLS manifests and redirects are intentionally unsupported.
#[cfg(test)]
pub(super) async fn open_direct_ts_stream(
    raw_url: &str,
    pins: &[OriginPin],
) -> Result<reqwest::Response, FeedError> {
    if raw_url.len() > 8 * 1024 {
        return Err(FeedError::LimitExceeded);
    }
    let url = Url::parse(raw_url).map_err(|_| FeedError::InvalidValue)?;
    let approved = approved_urls(pins)?;
    if url.fragment().is_some() || !approved_origin(&url, &approved) {
        return Err(FeedError::DisallowedOrigin);
    }
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(LIVE_CONNECT_TIMEOUT)
        .read_timeout(LIVE_READ_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none());
    for pin in pins {
        let (origin_url, addresses) = pin.parsed()?;
        let host = origin_url.host_str().ok_or(FeedError::InvalidValue)?;
        builder = builder.resolve_to_addrs(host, &addresses);
    }
    let client = builder.build().map_err(|_| FeedError::Unavailable)?;
    let response = client
        .get(url)
        .header(
            reqwest::header::ACCEPT,
            "video/mp2t, video/mpeg, application/octet-stream",
        )
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .send()
        .await
        .map_err(|_| FeedError::Unavailable)?;
    if response.status().is_redirection() {
        return Err(FeedError::DisallowedOrigin);
    }
    if !response.status().is_success() {
        return Err(FeedError::Unavailable);
    }
    if response
        .headers()
        .get(reqwest::header::CONTENT_ENCODING)
        .is_some_and(|value| value.as_bytes() != b"identity")
    {
        return Err(FeedError::InvalidStructure);
    }
    if response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .is_some_and(|value| {
            let media_type = value.split(';').next().unwrap_or_default().trim();
            !matches!(
                media_type.to_ascii_lowercase().as_str(),
                "video/mp2t" | "video/mpeg" | "application/octet-stream"
            )
        })
    {
        return Err(FeedError::InvalidStructure);
    }
    Ok(response)
}

/// Open a configured live source. Direct MPEG-TS is supported, as are HLS
/// media playlists made only of unencrypted MPEG-TS segments. Master playlists
/// select one bounded variant; alternate audio renditions, encryption,
/// byte-ranges, and fragmented MP4 are rejected.
pub(super) async fn open_live_input(
    raw_url: &str,
    pins: &[OriginPin],
) -> Result<LiveInput, FeedError> {
    if raw_url.len() > 8 * 1024 {
        return Err(FeedError::LimitExceeded);
    }
    let url = Url::parse(raw_url).map_err(|_| FeedError::InvalidValue)?;
    let approved = approved_urls(pins)?;
    if url.fragment().is_some() || !approved_origin(&url, &approved) {
        return Err(FeedError::DisallowedOrigin);
    }
    let client = build_live_client(pins)?;
    let response = request_live_resource(&client, &url).await?;
    let content_type = response_content_type(&response);
    let manifest_hint = url.path().to_ascii_lowercase().ends_with(".m3u8")
        || is_hls_content_type(content_type.as_deref());
    if !manifest_hint {
        if !is_ts_content_type(content_type.as_deref()) {
            return Err(FeedError::UnsupportedTransport);
        }
        return Ok(LiveInput::Direct(response));
    }

    let manifest = read_limited_response(response, MAX_HLS_MANIFEST_BYTES).await?;
    let parsed = parse_hls_manifest(&manifest, &url, pins)?;
    let (playlist_url, playlist) = match parsed {
        ParsedHlsPlaylist::Media(playlist) => (url, playlist),
        ParsedHlsPlaylist::Master(mut variants) => {
            variants.sort_by_key(|variant| (variant.bandwidth, variant.url.as_str().to_owned()));
            let variant = variants
                .into_iter()
                .rev()
                .find(|variant| variant.bandwidth <= 8_000_000)
                .ok_or(FeedError::UnsupportedTransport)?;
            let response = request_live_resource(&client, &variant.url).await?;
            if !is_hls_content_type(response_content_type(&response).as_deref())
                && !variant.url.path().to_ascii_lowercase().ends_with(".m3u8")
            {
                return Err(FeedError::UnsupportedTransport);
            }
            let manifest = read_limited_response(response, MAX_HLS_MANIFEST_BYTES).await?;
            match parse_hls_manifest(&manifest, &variant.url, pins)? {
                ParsedHlsPlaylist::Media(playlist) => (variant.url, playlist),
                ParsedHlsPlaylist::Master(_) => return Err(FeedError::UnsupportedTransport),
            }
        }
    };
    Ok(LiveInput::Hls {
        client,
        pins: pins.to_vec(),
        playlist_url,
        playlist,
        last_sequence: None,
    })
}

fn build_live_client(pins: &[OriginPin]) -> Result<reqwest::Client, FeedError> {
    let mut builder = reqwest::Client::builder()
        .no_proxy()
        .connect_timeout(LIVE_CONNECT_TIMEOUT)
        .read_timeout(LIVE_READ_TIMEOUT)
        .redirect(reqwest::redirect::Policy::none());
    for pin in pins {
        let (origin_url, addresses) = pin.parsed()?;
        let host = origin_url.host_str().ok_or(FeedError::InvalidValue)?;
        builder = builder.resolve_to_addrs(host, &addresses);
    }
    builder.build().map_err(|_| FeedError::Unavailable)
}

async fn request_live_resource(
    client: &reqwest::Client,
    url: &Url,
) -> Result<reqwest::Response, FeedError> {
    let response = client
        .get(url.clone())
        .header(
            reqwest::header::ACCEPT,
            "video/mp2t, video/mpeg, application/vnd.apple.mpegurl, application/x-mpegURL, application/octet-stream",
        )
        .header(reqwest::header::ACCEPT_ENCODING, "identity")
        .header(reqwest::header::CACHE_CONTROL, "no-cache")
        .send()
        .await
        .map_err(|_| FeedError::Unavailable)?;
    if response.status().is_redirection() {
        return Err(FeedError::DisallowedOrigin);
    }
    if !response.status().is_success() {
        return Err(FeedError::Unavailable);
    }
    if response
        .headers()
        .get(reqwest::header::CONTENT_ENCODING)
        .is_some_and(|value| value.as_bytes() != b"identity")
    {
        return Err(FeedError::InvalidStructure);
    }
    Ok(response)
}

fn response_content_type(response: &reqwest::Response) -> Option<String> {
    response
        .headers()
        .get(reqwest::header::CONTENT_TYPE)
        .and_then(|value| value.to_str().ok())
        .map(|value| {
            value
                .split(';')
                .next()
                .unwrap_or_default()
                .trim()
                .to_ascii_lowercase()
        })
}

fn is_hls_content_type(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|value| {
        matches!(
            value,
            "application/vnd.apple.mpegurl"
                | "application/x-mpegurl"
                | "audio/mpegurl"
                | "audio/x-mpegurl"
        )
    })
}

fn is_ts_content_type(content_type: Option<&str>) -> bool {
    content_type.is_some_and(|value| {
        matches!(
            value,
            "video/mp2t" | "video/mpeg" | "application/octet-stream"
        )
    })
}

async fn read_limited_response(
    response: reqwest::Response,
    maximum_bytes: usize,
) -> Result<Vec<u8>, FeedError> {
    if response
        .content_length()
        .is_some_and(|length| length > maximum_bytes as u64)
    {
        return Err(FeedError::TooLarge);
    }
    tokio::time::timeout(LIVE_READ_TIMEOUT, async move {
        let mut stream = response.bytes_stream();
        let mut bytes = Vec::new();
        while let Some(chunk) = stream.next().await {
            let chunk = chunk.map_err(|_| FeedError::Unavailable)?;
            if bytes.len().saturating_add(chunk.len()) > maximum_bytes {
                return Err(FeedError::TooLarge);
            }
            bytes.extend_from_slice(&chunk);
        }
        Ok(bytes)
    })
    .await
    .map_err(|_| FeedError::Unavailable)?
}

fn parse_hls_manifest(
    bytes: &[u8],
    playlist_url: &Url,
    pins: &[OriginPin],
) -> Result<ParsedHlsPlaylist, FeedError> {
    if bytes.len() > MAX_HLS_MANIFEST_BYTES {
        return Err(FeedError::TooLarge);
    }
    let approved = approved_urls(pins)?;
    if !approved_origin(playlist_url, &approved) {
        return Err(FeedError::DisallowedOrigin);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| FeedError::InvalidEncoding)?;
    let mut lines = text.lines();
    if lines.next().map(|line| line.trim_end_matches('\r').trim()) != Some("#EXTM3U") {
        return Err(FeedError::InvalidStructure);
    }
    let mut variants = Vec::new();
    let mut pending_bandwidth = None;
    let mut segments = Vec::new();
    let mut media_sequence = 0_u64;
    let mut target_duration = Duration::from_secs(6);
    let mut segment_duration_pending = false;
    let mut end_list = false;
    for line in lines {
        let line = line.trim_end_matches('\r').trim();
        if line.is_empty() {
            continue;
        }
        if line.len() > MAX_M3U_LINE_BYTES {
            return Err(FeedError::LimitExceeded);
        }
        if let Some(attributes) = line.strip_prefix("#EXT-X-STREAM-INF:") {
            if pending_bandwidth.is_some() || !segments.is_empty() || !variants.is_empty() {
                return Err(FeedError::InvalidStructure);
            }
            let attrs = parse_hls_attributes(attributes)?;
            let bandwidth = attrs
                .get("bandwidth")
                .ok_or(FeedError::InvalidValue)?
                .parse::<u64>()
                .map_err(|_| FeedError::InvalidValue)?;
            if bandwidth == 0 || bandwidth > 100_000_000 {
                return Err(FeedError::LimitExceeded);
            }
            if attrs
                .get("audio")
                .is_some_and(|value| !value.eq_ignore_ascii_case("none"))
            {
                return Err(FeedError::UnsupportedTransport);
            }
            pending_bandwidth = Some(bandwidth);
            continue;
        }
        if line.starts_with('#') {
            if line.starts_with("#EXT-X-KEY:")
                || line.starts_with("#EXT-X-SESSION-KEY:")
                || line.starts_with("#EXT-X-MAP:")
                || line.starts_with("#EXT-X-BYTERANGE:")
                || line.starts_with("#EXT-X-I-FRAME-STREAM-INF:")
                || line.starts_with("#EXT-X-MEDIA:")
            {
                return Err(FeedError::UnsupportedTransport);
            }
            if let Some(value) = line.strip_prefix("#EXT-X-MEDIA-SEQUENCE:") {
                if !segments.is_empty() {
                    return Err(FeedError::InvalidStructure);
                }
                media_sequence = value.parse().map_err(|_| FeedError::InvalidValue)?;
                continue;
            }
            if let Some(value) = line.strip_prefix("#EXT-X-TARGETDURATION:") {
                let seconds = value.parse::<u64>().map_err(|_| FeedError::InvalidValue)?;
                if !(1..=60).contains(&seconds) {
                    return Err(FeedError::LimitExceeded);
                }
                target_duration = Duration::from_secs(seconds);
                continue;
            }
            if line.starts_with("#EXTINF:") {
                if segment_duration_pending || pending_bandwidth.is_some() {
                    return Err(FeedError::InvalidStructure);
                }
                let raw = line
                    .strip_prefix("#EXTINF:")
                    .and_then(|value| {
                        value
                            .split_once(',')
                            .map(|(duration, _)| duration)
                            .or(Some(value))
                    })
                    .ok_or(FeedError::InvalidStructure)?;
                let duration = raw.parse::<f64>().map_err(|_| FeedError::InvalidValue)?;
                if !duration.is_finite() || !(0.1..=60.0).contains(&duration) {
                    return Err(FeedError::LimitExceeded);
                }
                segment_duration_pending = true;
                continue;
            }
            if line == "#EXT-X-ENDLIST"
                || line == "#EXT-X-DISCONTINUITY"
                || line == "#EXT-X-INDEPENDENT-SEGMENTS"
                || line.starts_with("#EXT-X-PROGRAM-DATE-TIME:")
                || line.starts_with("#EXT-X-PLAYLIST-TYPE:")
                || line.starts_with("#EXT-X-VERSION:")
                || line.starts_with("#EXT-X-ALLOW-CACHE:")
            {
                end_list |= line == "#EXT-X-ENDLIST";
                continue;
            }
            return Err(FeedError::UnsupportedTransport);
        }
        let uri = resolve_feed_url(line, playlist_url, &approved)?;
        if let Some(bandwidth) = pending_bandwidth.take() {
            variants.push(HlsVariant {
                bandwidth,
                url: uri,
            });
            if variants.len() > MAX_HLS_VARIANTS {
                return Err(FeedError::LimitExceeded);
            }
        } else if segment_duration_pending {
            let offset = u64::try_from(segments.len()).map_err(|_| FeedError::LimitExceeded)?;
            let sequence = media_sequence
                .checked_add(offset)
                .ok_or(FeedError::LimitExceeded)?;
            segments.push(HlsSegment { sequence, url: uri });
            segment_duration_pending = false;
            if segments.len() > MAX_HLS_SEGMENTS {
                return Err(FeedError::LimitExceeded);
            }
        } else {
            return Err(FeedError::InvalidStructure);
        }
    }
    if pending_bandwidth.is_some() || segment_duration_pending {
        return Err(FeedError::InvalidStructure);
    }
    if !variants.is_empty() {
        if !segments.is_empty() || end_list {
            return Err(FeedError::InvalidStructure);
        }
        return Ok(ParsedHlsPlaylist::Master(variants));
    }
    if segments.is_empty() {
        return Err(FeedError::InvalidStructure);
    }
    Ok(ParsedHlsPlaylist::Media(HlsMediaPlaylist {
        target_duration,
        segments,
        end_list,
    }))
}

fn parse_hls_attributes(input: &str) -> Result<HashMap<String, String>, FeedError> {
    let mut attributes = HashMap::new();
    let mut quoted = false;
    let mut escaped = false;
    let mut start = 0;
    for (index, character) in input.char_indices() {
        if escaped {
            escaped = false;
        } else if character == '\\' && quoted {
            escaped = true;
        } else if character == '"' {
            quoted = !quoted;
        } else if character == ',' && !quoted {
            insert_hls_attribute(&input[start..index], &mut attributes)?;
            start = index + character.len_utf8();
        }
    }
    if quoted || escaped {
        return Err(FeedError::InvalidStructure);
    }
    insert_hls_attribute(&input[start..], &mut attributes)?;
    Ok(attributes)
}

fn insert_hls_attribute(
    raw: &str,
    attributes: &mut HashMap<String, String>,
) -> Result<(), FeedError> {
    let (key, value) = raw.split_once('=').ok_or(FeedError::InvalidStructure)?;
    let key = key.trim().to_ascii_lowercase();
    let value = value.trim().trim_matches('"').to_owned();
    if key.is_empty()
        || key.len() > 64
        || value.len() > 2048
        || value.chars().any(char::is_control)
        || attributes.insert(key, value).is_some()
    {
        return Err(FeedError::InvalidValue);
    }
    Ok(())
}

impl LiveInput {
    /// Forward bounded TS data into a supervised local child. The caller owns
    /// process cancellation and must close the writer on stop. For live media
    /// this method returns only on upstream ENDLIST/EOF, quota, deadline, or
    /// cancellation.
    pub(super) async fn copy_to<W>(
        &mut self,
        writer: &mut W,
        cancel: &mut tokio::sync::oneshot::Receiver<()>,
        max_bytes: u64,
        max_duration: Duration,
    ) -> Result<u64, FeedError>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        let deadline = Instant::now() + max_duration.min(MAX_LIVE_INPUT_SECONDS);
        self.copy_until_inner(writer, cancel, max_bytes, deadline, false, false)
            .await
    }

    /// Capture only until an externally scheduled end time. Unlike live
    /// playback, an early EOF or finite HLS playlist is an incomplete
    /// recording and must not be published as successful.
    pub(super) async fn copy_recording_until<W>(
        &mut self,
        writer: &mut W,
        cancel: &mut tokio::sync::oneshot::Receiver<()>,
        max_bytes: u64,
        stop_at: Instant,
    ) -> Result<u64, FeedError>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        self.copy_until_inner(writer, cancel, max_bytes, stop_at, true, true)
            .await
    }

    async fn copy_until_inner<W>(
        &mut self,
        writer: &mut W,
        cancel: &mut tokio::sync::oneshot::Receiver<()>,
        max_bytes: u64,
        deadline: Instant,
        stop_at_deadline_is_success: bool,
        reject_early_end: bool,
    ) -> Result<u64, FeedError>
    where
        W: tokio::io::AsyncWrite + Unpin,
    {
        let mut total = 0_u64;
        match self {
            Self::Direct(response) => loop {
                let next = tokio::select! {
                    _ = &mut *cancel => return Err(FeedError::Cancelled),
                    _ = tokio::time::sleep_until(deadline.into()) => {
                        return if stop_at_deadline_is_success {
                            Ok(total)
                        } else {
                            Err(FeedError::QuotaExceeded)
                        };
                    },
                    value = response.chunk() => value.map_err(|_| FeedError::Unavailable)?,
                };
                let Some(chunk) = next else {
                    return if reject_early_end {
                        Err(FeedError::Unavailable)
                    } else {
                        Ok(total)
                    };
                };
                total = checked_live_bytes(total, chunk.len(), max_bytes)?;
                writer
                    .write_all(&chunk)
                    .await
                    .map_err(|_| FeedError::Unavailable)?;
            },
            Self::Hls {
                client,
                pins,
                playlist_url,
                playlist,
                last_sequence,
            } => loop {
                let current = playlist.clone();
                for segment in current.segments {
                    if last_sequence.is_some_and(|last| segment.sequence <= last) {
                        continue;
                    }
                    if Instant::now() >= deadline {
                        return if stop_at_deadline_is_success {
                            Ok(total)
                        } else {
                            Err(FeedError::QuotaExceeded)
                        };
                    }
                    let response = tokio::select! {
                        _ = &mut *cancel => return Err(FeedError::Cancelled),
                        _ = tokio::time::sleep_until(deadline.into()) => {
                            return if stop_at_deadline_is_success {
                                Ok(total)
                            } else {
                                Err(FeedError::QuotaExceeded)
                            };
                        },
                        result = request_live_resource(client, &segment.url) => result?,
                    };
                    if !is_ts_content_type(response_content_type(&response).as_deref()) {
                        return Err(FeedError::UnsupportedTransport);
                    }
                    if response
                        .content_length()
                        .is_some_and(|length| length > MAX_HLS_SEGMENT_BYTES)
                    {
                        return Err(FeedError::TooLarge);
                    }
                    let mut stream = response.bytes_stream();
                    let mut segment_bytes = 0_u64;
                    loop {
                        let next = tokio::select! {
                            _ = &mut *cancel => return Err(FeedError::Cancelled),
                            _ = tokio::time::sleep_until(deadline.into()) => {
                                return if stop_at_deadline_is_success {
                                    Ok(total)
                                } else {
                                    Err(FeedError::QuotaExceeded)
                                };
                            },
                            value = stream.next() => value,
                        };
                        let Some(chunk) = next else { break };
                        let chunk = chunk.map_err(|_| FeedError::Unavailable)?;
                        segment_bytes = segment_bytes.saturating_add(chunk.len() as u64);
                        if segment_bytes > MAX_HLS_SEGMENT_BYTES {
                            return Err(FeedError::TooLarge);
                        }
                        total = checked_live_bytes(total, chunk.len(), max_bytes)?;
                        writer
                            .write_all(&chunk)
                            .await
                            .map_err(|_| FeedError::Unavailable)?;
                    }
                    *last_sequence = Some(segment.sequence);
                }
                writer.flush().await.map_err(|_| FeedError::Unavailable)?;
                if playlist.end_list {
                    return if reject_early_end {
                        Err(FeedError::Unavailable)
                    } else {
                        Ok(total)
                    };
                }
                let target_duration = playlist.target_duration;
                loop {
                    let delay = (target_duration / 2)
                        .max(Duration::from_secs(1))
                        .min(Duration::from_secs(5));
                    tokio::select! {
                        _ = &mut *cancel => return Err(FeedError::Cancelled),
                        _ = tokio::time::sleep_until(deadline.into()) => {
                            return if stop_at_deadline_is_success {
                                Ok(total)
                            } else {
                                Err(FeedError::QuotaExceeded)
                            };
                        },
                        _ = tokio::time::sleep(delay) => {}
                    }
                    let response = tokio::select! {
                        _ = &mut *cancel => return Err(FeedError::Cancelled),
                        _ = tokio::time::sleep_until(deadline.into()) => {
                            return if stop_at_deadline_is_success {
                                Ok(total)
                            } else {
                                Err(FeedError::QuotaExceeded)
                            };
                        },
                        result = request_live_resource(client, playlist_url) => result?,
                    };
                    if !is_hls_content_type(response_content_type(&response).as_deref())
                        && !playlist_url.path().to_ascii_lowercase().ends_with(".m3u8")
                    {
                        return Err(FeedError::UnsupportedTransport);
                    }
                    let bytes = read_limited_response(response, MAX_HLS_MANIFEST_BYTES).await?;
                    let ParsedHlsPlaylist::Media(next) =
                        parse_hls_manifest(&bytes, playlist_url, pins)?
                    else {
                        return Err(FeedError::UnsupportedTransport);
                    };
                    if next.end_list
                        || next
                            .segments
                            .iter()
                            .any(|entry| last_sequence.is_none_or(|last| entry.sequence > last))
                    {
                        if next.end_list && reject_early_end {
                            return Err(FeedError::Unavailable);
                        }
                        *playlist = next;
                        break;
                    }
                    if Instant::now() >= deadline {
                        return if stop_at_deadline_is_success {
                            Ok(total)
                        } else {
                            Err(FeedError::QuotaExceeded)
                        };
                    }
                }
            },
        }
    }
}

fn checked_live_bytes(total: u64, chunk_size: usize, maximum: u64) -> Result<u64, FeedError> {
    let total = total.saturating_add(chunk_size as u64);
    if total > maximum.min(MAX_LIVE_INPUT_BYTES) {
        return Err(FeedError::QuotaExceeded);
    }
    Ok(total)
}

#[cfg(test)]
fn approved_origin_pinned(url: &Url, pins: &[OriginPin]) -> Result<bool, FeedError> {
    let origin = Origin::of(url).ok_or(FeedError::InvalidValue)?;
    for pin in pins {
        let (pinned, _) = pin.parsed()?;
        if Origin::of(&pinned).as_ref() == Some(&origin) {
            return Ok(true);
        }
    }
    Ok(false)
}

fn approved_origin(url: &Url, approved: &[Url]) -> bool {
    let Some(origin) = Origin::of(url) else {
        return false;
    };
    approved
        .iter()
        .any(|entry| Origin::of(entry).as_ref() == Some(&origin))
}

fn resolve_feed_url(value: &str, base: &Url, approved: &[Url]) -> Result<Url, FeedError> {
    if value.is_empty() || value.len() > 8 * 1024 || value.chars().any(char::is_control) {
        return Err(FeedError::InvalidValue);
    }
    let url = base.join(value).map_err(|_| FeedError::InvalidValue)?;
    if url.fragment().is_some() || !approved_origin(&url, approved) {
        return Err(FeedError::DisallowedOrigin);
    }
    Ok(url)
}

pub(super) fn parse_m3u(
    bytes: &[u8],
    playlist_url: &Url,
    approved_origins: &[Url],
) -> Result<Vec<M3uChannel>, FeedError> {
    if bytes.len() > MAX_M3U_BYTES {
        return Err(FeedError::TooLarge);
    }
    if !approved_origin(playlist_url, approved_origins) {
        return Err(FeedError::DisallowedOrigin);
    }
    let text = std::str::from_utf8(bytes).map_err(|_| FeedError::InvalidEncoding)?;
    let text = text.strip_prefix('\u{feff}').unwrap_or(text);
    let mut lines = text.lines();
    let Some(header) = lines.find(|line| !line.trim().is_empty()) else {
        return Err(FeedError::InvalidStructure);
    };
    if header.len() > MAX_M3U_LINE_BYTES {
        return Err(FeedError::LimitExceeded);
    }
    if header.trim_end_matches('\r').trim() != "#EXTM3U" {
        return Err(FeedError::InvalidStructure);
    }

    let mut pending = None;
    let mut channels = Vec::new();
    let mut ids = HashSet::new();
    for line in lines {
        let line = line.trim_end_matches('\r').trim();
        if line.len() > MAX_M3U_LINE_BYTES {
            return Err(FeedError::LimitExceeded);
        }
        if line.is_empty() {
            continue;
        }
        if let Some(payload) = line.strip_prefix("#EXTINF:") {
            if pending.is_some() {
                return Err(FeedError::InvalidStructure);
            }
            pending = Some(parse_extinf(payload)?);
            continue;
        }
        if line.starts_with('#') {
            continue;
        }
        let Some(extinf) = pending.take() else {
            return Err(FeedError::InvalidStructure);
        };
        let stream_url = resolve_feed_url(line, playlist_url, approved_origins)?;
        let logo_url = extinf
            .logo_text
            .as_deref()
            .map(|logo| resolve_feed_url(logo, playlist_url, approved_origins))
            .transpose()?;
        if let Some(id) = extinf.source_id.as_deref()
            && !ids.insert(id.to_owned())
        {
            return Err(FeedError::DuplicateId);
        }
        channels.push(M3uChannel {
            source_id: extinf.source_id,
            name: extinf.name,
            group: extinf.group,
            logo_url,
            stream_url,
        });
        if channels.len() > MAX_M3U_CHANNELS {
            return Err(FeedError::LimitExceeded);
        }
    }
    if pending.is_some() || channels.is_empty() {
        return Err(FeedError::InvalidStructure);
    }
    Ok(channels)
}

fn parse_extinf(payload: &str) -> Result<ParsedExtinf, FeedError> {
    let comma = comma_outside_quotes(payload).ok_or(FeedError::InvalidStructure)?;
    let header = payload[..comma].trim();
    let title = payload[comma + 1..].trim();
    if title.is_empty() || title.len() > MAX_CHANNEL_NAME_BYTES {
        return Err(FeedError::InvalidValue);
    }
    let mut words = header.splitn(2, char::is_whitespace);
    let duration = words.next().ok_or(FeedError::InvalidStructure)?;
    let duration = duration
        .parse::<f64>()
        .map_err(|_| FeedError::InvalidValue)?;
    if !duration.is_finite() || !(-1.0..=86_400.0).contains(&duration) {
        return Err(FeedError::InvalidValue);
    }
    let attributes = parse_m3u_attributes(words.next().unwrap_or_default())?;
    let source_id = attributes
        .get("tvg-id")
        .filter(|value| !value.is_empty())
        .cloned();
    if source_id
        .as_ref()
        .is_some_and(|value| value.len() > MAX_CHANNEL_ID_BYTES || contains_unsafe_text(value))
    {
        return Err(FeedError::InvalidValue);
    }
    let name = attributes
        .get("tvg-name")
        .filter(|value| !value.is_empty())
        .cloned()
        .unwrap_or_else(|| title.to_owned());
    if name.len() > MAX_CHANNEL_NAME_BYTES || contains_unsafe_text(&name) {
        return Err(FeedError::InvalidValue);
    }
    let group = attributes
        .get("group-title")
        .filter(|value| !value.is_empty())
        .cloned();
    if group
        .as_ref()
        .is_some_and(|value| value.len() > MAX_CHANNEL_NAME_BYTES || contains_unsafe_text(value))
    {
        return Err(FeedError::InvalidValue);
    }
    let logo_text = attributes
        .get("tvg-logo")
        .filter(|value| !value.is_empty())
        .cloned();
    Ok(ParsedExtinf {
        source_id,
        name,
        group,
        logo_text,
    })
}

fn comma_outside_quotes(text: &str) -> Option<usize> {
    let mut quoted = false;
    let mut escaped = false;
    for (index, character) in text.char_indices() {
        if escaped {
            escaped = false;
            continue;
        }
        if character == '\\' && quoted {
            escaped = true;
        } else if character == '"' {
            quoted = !quoted;
        } else if character == ',' && !quoted {
            return Some(index);
        }
    }
    (!quoted)
        .then_some(text.len())
        .filter(|index| *index < text.len())
}

fn parse_m3u_attributes(input: &str) -> Result<BTreeMap<String, String>, FeedError> {
    let bytes = input.as_bytes();
    let mut cursor = 0;
    let mut attributes = BTreeMap::new();
    while cursor < bytes.len() {
        while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        if cursor == bytes.len() {
            break;
        }
        let key_start = cursor;
        while bytes
            .get(cursor)
            .is_some_and(|byte| byte.is_ascii_alphanumeric() || matches!(byte, b'_' | b'-'))
        {
            cursor += 1;
        }
        if cursor == key_start {
            return Err(FeedError::InvalidStructure);
        }
        let key = std::str::from_utf8(&bytes[key_start..cursor])
            .map_err(|_| FeedError::InvalidEncoding)?
            .to_ascii_lowercase();
        while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        if bytes.get(cursor) != Some(&b'=') {
            return Err(FeedError::InvalidStructure);
        }
        cursor += 1;
        while bytes.get(cursor).is_some_and(u8::is_ascii_whitespace) {
            cursor += 1;
        }
        let quoted = bytes.get(cursor) == Some(&b'"');
        if quoted {
            cursor += 1;
        }
        let mut value = Vec::new();
        while let Some(&byte) = bytes.get(cursor) {
            if quoted {
                if byte == b'"' {
                    cursor += 1;
                    break;
                }
                if byte == b'\\' && bytes.get(cursor + 1) == Some(&b'"') {
                    value.push(b'"');
                    cursor += 2;
                    continue;
                }
            } else if byte.is_ascii_whitespace() {
                break;
            }
            value.push(byte);
            cursor += 1;
        }
        if quoted && bytes.get(cursor.wrapping_sub(1)) != Some(&b'"') {
            return Err(FeedError::InvalidStructure);
        }
        if value.len() > MAX_ATTRIBUTE_BYTES || value.iter().any(|byte| byte.is_ascii_control()) {
            return Err(FeedError::InvalidValue);
        }
        let value = String::from_utf8(value).map_err(|_| FeedError::InvalidEncoding)?;
        if attributes.insert(key, value).is_some() {
            return Err(FeedError::InvalidStructure);
        }
        if attributes.len() > MAX_M3U_ATTRIBUTES {
            return Err(FeedError::LimitExceeded);
        }
    }
    Ok(attributes)
}

fn contains_unsafe_text(value: &str) -> bool {
    value.chars().any(char::is_control)
}

pub(super) fn parse_xmltv(
    bytes: &[u8],
    guide_url: &Url,
    approved_origins: &[Url],
) -> Result<(Vec<XmltvChannel>, Vec<XmltvProgram>), FeedError> {
    if bytes.len() > MAX_XMLTV_BYTES {
        return Err(FeedError::TooLarge);
    }
    if !approved_origin(guide_url, approved_origins) {
        return Err(FeedError::DisallowedOrigin);
    }
    let mut reader = Reader::from_reader(bytes);
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::new();
    let mut depth = 0usize;
    let mut event_count = 0usize;
    let mut channels = Vec::new();
    let mut programs = Vec::new();
    let mut channel_ids = HashSet::new();
    let mut current_channel: Option<ChannelDraft> = None;
    let mut current_program: Option<ProgramDraft> = None;
    let mut capture: Option<TextCapture> = None;
    let mut rating_system: Option<String> = None;
    let mut ignore_depth: Option<usize> = None;
    let mut root_seen = false;
    let mut root_closed = false;

    loop {
        let event = reader
            .read_event_into(&mut buffer)
            .map_err(|_| FeedError::InvalidStructure)?;
        event_count += 1;
        if event_count > MAX_XMLTV_EVENTS {
            return Err(FeedError::LimitExceeded);
        }
        if let Some(skip_depth) = ignore_depth {
            match event {
                Event::Start(start) => {
                    depth += 1;
                    if depth > MAX_XMLTV_DEPTH {
                        return Err(FeedError::LimitExceeded);
                    }
                    if matches!(start.name().as_ref(), b"tv" | b"channel" | b"programme") {
                        return Err(FeedError::InvalidStructure);
                    }
                    let _ = parse_xml_attrs(&start, reader.decoder())?;
                }
                Event::Empty(empty) => {
                    if matches!(empty.name().as_ref(), b"tv" | b"channel" | b"programme") {
                        return Err(FeedError::InvalidStructure);
                    }
                    let _ = parse_xml_attrs(&empty, reader.decoder())?;
                }
                Event::End(_) => {
                    if depth == skip_depth {
                        ignore_depth = None;
                    }
                    depth = depth.saturating_sub(1);
                }
                Event::DocType(_) => return Err(FeedError::InvalidStructure),
                _ => {}
            }
            buffer.clear();
            continue;
        }
        match event {
            Event::Start(start) => {
                depth += 1;
                if depth > MAX_XMLTV_DEPTH {
                    return Err(FeedError::LimitExceeded);
                }
                let attrs = parse_xml_attrs(&start, reader.decoder())?;
                let name = start.name();
                match name.as_ref() {
                    b"tv" if depth == 1 && !root_seen => root_seen = true,
                    b"tv" => return Err(FeedError::InvalidStructure),
                    b"channel"
                        if depth == 2
                            && root_seen
                            && !root_closed
                            && current_channel.is_none()
                            && current_program.is_none() =>
                    {
                        let id = attrs.get("id").cloned().ok_or(FeedError::InvalidValue)?;
                        validate_xml_id(&id)?;
                        current_channel = Some(ChannelDraft {
                            id,
                            display_name: None,
                            icon_url: None,
                        });
                    }
                    b"programme"
                        if depth == 2
                            && root_seen
                            && !root_closed
                            && current_channel.is_none()
                            && current_program.is_none() =>
                    {
                        let channel_id = attrs
                            .get("channel")
                            .cloned()
                            .ok_or(FeedError::InvalidValue)?;
                        validate_xml_id(&channel_id)?;
                        let start =
                            parse_xmltv_time(attrs.get("start").ok_or(FeedError::InvalidValue)?)?;
                        let stop =
                            parse_xmltv_time(attrs.get("stop").ok_or(FeedError::InvalidValue)?)?;
                        let span = stop.signed_duration_since(start);
                        if span <= chrono::Duration::zero() || span > chrono::Duration::hours(48) {
                            return Err(FeedError::InvalidValue);
                        }
                        current_program = Some(ProgramDraft {
                            channel_id,
                            start,
                            stop,
                            title: None,
                            description: None,
                            category: None,
                            rating_system: None,
                            content_rating: None,
                            policy_rating_value: None,
                        });
                    }
                    b"channel" | b"programme" => return Err(FeedError::InvalidStructure),
                    b"display-name"
                        if depth == 3
                            && current_channel.is_some()
                            && current_program.is_none()
                            && capture.is_none() =>
                    {
                        capture = Some(TextCapture::ChannelName(String::new()));
                    }
                    b"title"
                        if depth == 3
                            && current_program.is_some()
                            && current_channel.is_none()
                            && capture.is_none() =>
                    {
                        capture = Some(TextCapture::ProgramTitle(String::new()));
                    }
                    b"desc"
                        if depth == 3
                            && current_program.is_some()
                            && current_channel.is_none()
                            && capture.is_none() =>
                    {
                        capture = Some(TextCapture::ProgramDescription(String::new()));
                    }
                    b"category"
                        if depth == 3
                            && current_program.is_some()
                            && current_channel.is_none()
                            && capture.is_none() =>
                    {
                        capture = Some(TextCapture::ProgramCategory(String::new()));
                    }
                    b"rating"
                        if depth == 3
                            && current_program.is_some()
                            && current_channel.is_none()
                            && capture.is_none()
                            && rating_system.is_none() =>
                    {
                        rating_system = Some(attrs.get("system").cloned().unwrap_or_default());
                    }
                    b"value"
                        if depth == 4
                            && current_program.is_some()
                            && rating_system.is_some()
                            && capture.is_none() =>
                    {
                        capture = Some(TextCapture::ProgramRating {
                            system: rating_system.clone().unwrap_or_default(),
                            text: String::new(),
                        });
                    }
                    b"icon"
                        if depth == 3
                            && current_channel.is_some()
                            && current_program.is_none()
                            && capture.is_none() =>
                    {
                        if let Some(src) = attrs.get("src") {
                            let icon = resolve_feed_url(src, guide_url, approved_origins)?;
                            let channel = current_channel
                                .as_mut()
                                .ok_or(FeedError::InvalidStructure)?;
                            if channel.icon_url.is_none() {
                                channel.icon_url = Some(icon);
                            }
                        }
                        ignore_depth = Some(depth);
                    }
                    _ if depth >= 3
                        && root_seen
                        && !root_closed
                        && capture.is_none()
                        && (current_channel.is_some() || current_program.is_some()) =>
                    {
                        // Standard XMLTV has many optional channel/program
                        // children. Bound and parse their attributes, then
                        // skip their subtree without treating it as display
                        // text or trusting any embedded URLs.
                        ignore_depth = Some(depth);
                    }
                    _ => return Err(FeedError::InvalidStructure),
                }
            }
            Event::Empty(empty) => {
                let attrs = parse_xml_attrs(&empty, reader.decoder())?;
                let name = empty.name();
                if name.as_ref() == b"icon"
                    && depth == 2
                    && current_channel.is_some()
                    && current_program.is_none()
                    && capture.is_none()
                {
                    if let Some(src) = attrs.get("src") {
                        let icon = resolve_feed_url(src, guide_url, approved_origins)?;
                        let channel = current_channel
                            .as_mut()
                            .ok_or(FeedError::InvalidStructure)?;
                        if channel.icon_url.is_none() {
                            channel.icon_url = Some(icon);
                        }
                    }
                } else if name.as_ref() == b"icon"
                    && depth == 2
                    && current_program.is_some()
                    && capture.is_none()
                {
                    // XMLTV permits per-programme icons. This API does not
                    // expose them yet, so ignore the bounded element without
                    // fetching or persisting its untrusted URL.
                } else if depth >= 3
                    && root_seen
                    && !root_closed
                    && capture.is_none()
                    && (current_channel.is_some() || current_program.is_some())
                    && !matches!(name.as_ref(), b"channel" | b"programme")
                {
                    // Ignore a bounded, self-closing optional XMLTV child.
                } else {
                    return Err(FeedError::InvalidStructure);
                }
            }
            Event::Text(text) => {
                if let Some(capture) = capture.as_mut() {
                    append_xml_text(
                        capture,
                        &text.decode().map_err(|_| FeedError::InvalidEncoding)?,
                    )?;
                } else if !text
                    .decode()
                    .map_err(|_| FeedError::InvalidEncoding)?
                    .trim()
                    .is_empty()
                {
                    return Err(FeedError::InvalidStructure);
                }
            }
            Event::CData(text) => {
                let value = text.decode().map_err(|_| FeedError::InvalidEncoding)?;
                if let Some(capture) = capture.as_mut() {
                    capture.push(&value)?;
                } else if !value.trim().is_empty() {
                    return Err(FeedError::InvalidStructure);
                }
            }
            Event::End(end) => {
                let name = end.local_name();
                match name.as_ref() {
                    b"tv" => {
                        if depth != 1 || !root_seen || root_closed {
                            return Err(FeedError::InvalidStructure);
                        }
                        root_closed = true;
                    }
                    b"display-name" => {
                        if depth != 3
                            || !matches!(capture.as_ref(), Some(TextCapture::ChannelName(_)))
                        {
                            return Err(FeedError::InvalidStructure);
                        }
                        capture
                            .take()
                            .ok_or(FeedError::InvalidStructure)?
                            .assign(&mut current_channel, &mut current_program)?;
                    }
                    b"title" => {
                        if depth != 3
                            || !matches!(capture.as_ref(), Some(TextCapture::ProgramTitle(_)))
                        {
                            return Err(FeedError::InvalidStructure);
                        }
                        capture
                            .take()
                            .ok_or(FeedError::InvalidStructure)?
                            .assign(&mut current_channel, &mut current_program)?;
                    }
                    b"desc" => {
                        if depth != 3
                            || !matches!(capture.as_ref(), Some(TextCapture::ProgramDescription(_)))
                        {
                            return Err(FeedError::InvalidStructure);
                        }
                        capture
                            .take()
                            .ok_or(FeedError::InvalidStructure)?
                            .assign(&mut current_channel, &mut current_program)?;
                    }
                    b"category" => {
                        if depth != 3
                            || !matches!(capture.as_ref(), Some(TextCapture::ProgramCategory(_)))
                        {
                            return Err(FeedError::InvalidStructure);
                        }
                        capture
                            .take()
                            .ok_or(FeedError::InvalidStructure)?
                            .assign(&mut current_channel, &mut current_program)?;
                    }
                    b"value" => {
                        if depth != 4
                            || rating_system.is_none()
                            || !matches!(capture.as_ref(), Some(TextCapture::ProgramRating { .. }))
                        {
                            return Err(FeedError::InvalidStructure);
                        }
                        capture
                            .take()
                            .ok_or(FeedError::InvalidStructure)?
                            .assign(&mut current_channel, &mut current_program)?;
                    }
                    b"rating" => {
                        if depth != 3 || rating_system.is_none() || capture.is_some() {
                            return Err(FeedError::InvalidStructure);
                        }
                        rating_system = None;
                    }
                    b"channel" => {
                        if depth != 2 || capture.is_some() || current_program.is_some() {
                            return Err(FeedError::InvalidStructure);
                        }
                        let draft = current_channel.take().ok_or(FeedError::InvalidStructure)?;
                        if !channel_ids.insert(draft.id.clone()) {
                            return Err(FeedError::DuplicateId);
                        }
                        let display_name = draft.display_name.ok_or(FeedError::InvalidValue)?;
                        if channels.len() >= MAX_XMLTV_CHANNELS {
                            return Err(FeedError::LimitExceeded);
                        }
                        channels.push(XmltvChannel {
                            id: draft.id,
                            display_name,
                            icon_url: draft.icon_url,
                        });
                    }
                    b"programme" => {
                        if depth != 2 || capture.is_some() || current_channel.is_some() {
                            return Err(FeedError::InvalidStructure);
                        }
                        let draft = current_program.take().ok_or(FeedError::InvalidStructure)?;
                        let title = draft.title.ok_or(FeedError::InvalidValue)?;
                        if programs.len() >= MAX_XMLTV_PROGRAMS {
                            return Err(FeedError::LimitExceeded);
                        }
                        programs.push(XmltvProgram {
                            channel_id: draft.channel_id,
                            start: draft.start,
                            stop: draft.stop,
                            title,
                            description: draft.description,
                            category: draft.category,
                            rating_system: draft.rating_system,
                            content_rating: draft.content_rating,
                            policy_rating_value: draft.policy_rating_value,
                        });
                    }
                    _ => return Err(FeedError::InvalidStructure),
                }
                depth = depth.saturating_sub(1);
            }
            Event::DocType(_) => return Err(FeedError::InvalidStructure),
            Event::GeneralRef(reference) => {
                let character = match reference
                    .resolve_char_ref()
                    .map_err(|_| FeedError::InvalidStructure)?
                {
                    Some(character) => character,
                    None => match reference
                        .decode()
                        .map_err(|_| FeedError::InvalidEncoding)?
                        .as_ref()
                    {
                        "amp" => '&',
                        "lt" => '<',
                        "gt" => '>',
                        "apos" => '\'',
                        "quot" => '"',
                        _ => return Err(FeedError::InvalidStructure),
                    },
                };
                if !is_legal_xml_character(character) {
                    return Err(FeedError::InvalidStructure);
                }
                if let Some(capture) = capture.as_mut() {
                    capture.push(&character.to_string())?;
                } else {
                    return Err(FeedError::InvalidStructure);
                }
            }
            Event::Eof => break,
            Event::Decl(_) | Event::Comment(_) | Event::PI(_) => {}
        }
        buffer.clear();
    }
    if !root_seen
        || !root_closed
        || depth != 0
        || current_channel.is_some()
        || current_program.is_some()
        || capture.is_some()
    {
        return Err(FeedError::InvalidStructure);
    }
    if channels.is_empty()
        || programs
            .iter()
            .any(|program| !channel_ids.contains(&program.channel_id))
    {
        return Err(FeedError::InvalidValue);
    }
    Ok((channels, programs))
}

#[derive(Debug)]
struct ChannelDraft {
    id: String,
    display_name: Option<String>,
    icon_url: Option<Url>,
}

#[derive(Debug)]
struct ProgramDraft {
    channel_id: String,
    start: DateTime<Utc>,
    stop: DateTime<Utc>,
    title: Option<String>,
    description: Option<String>,
    category: Option<String>,
    rating_system: Option<String>,
    content_rating: Option<String>,
    policy_rating_value: Option<i16>,
}

enum TextCapture {
    ChannelName(String),
    ProgramTitle(String),
    ProgramDescription(String),
    ProgramCategory(String),
    ProgramRating { system: String, text: String },
}

impl TextCapture {
    fn push(&mut self, value: &str) -> Result<(), FeedError> {
        let target = match self {
            Self::ChannelName(value)
            | Self::ProgramTitle(value)
            | Self::ProgramDescription(value)
            | Self::ProgramCategory(value) => value,
            Self::ProgramRating { text, .. } => text,
        };
        if target.len().saturating_add(value.len()) > MAX_XMLTV_TEXT_BYTES {
            return Err(FeedError::LimitExceeded);
        }
        target.push_str(value);
        Ok(())
    }

    fn assign(
        self,
        channel: &mut Option<ChannelDraft>,
        program: &mut Option<ProgramDraft>,
    ) -> Result<(), FeedError> {
        match self {
            Self::ChannelName(value) => {
                let value = clean_xml_text(value, MAX_CHANNEL_NAME_BYTES)?;
                let target = &mut channel
                    .as_mut()
                    .ok_or(FeedError::InvalidStructure)?
                    .display_name;
                if target.is_none() && !value.is_empty() {
                    *target = Some(value);
                }
            }
            Self::ProgramTitle(value) => {
                let value = clean_xml_text(value, MAX_CHANNEL_NAME_BYTES)?;
                let target = &mut program.as_mut().ok_or(FeedError::InvalidStructure)?.title;
                if target.is_none() && !value.is_empty() {
                    *target = Some(value);
                }
            }
            Self::ProgramDescription(value) => {
                let value = clean_xml_text(value, MAX_XMLTV_TEXT_BYTES)?;
                let target = &mut program
                    .as_mut()
                    .ok_or(FeedError::InvalidStructure)?
                    .description;
                if target.is_none() && !value.is_empty() {
                    *target = Some(value);
                }
            }
            Self::ProgramCategory(value) => {
                let value = clean_xml_text(value, MAX_CHANNEL_NAME_BYTES)?;
                let target = &mut program
                    .as_mut()
                    .ok_or(FeedError::InvalidStructure)?
                    .category;
                if target.is_none() && !value.is_empty() {
                    *target = Some(value);
                }
            }
            Self::ProgramRating { system, text } => {
                let label = clean_xml_text(text, 64)?;
                if label.is_empty() {
                    return Ok(());
                }
                let target = program.as_mut().ok_or(FeedError::InvalidStructure)?;
                let policy_value = parse_xmltv_parental_rating(&system, &label);
                let replace = match (target.policy_rating_value, policy_value) {
                    (None, Some(_)) => true,
                    (Some(current), Some(candidate)) => candidate > current,
                    (None, None) => target.content_rating.is_none(),
                    (Some(_), None) => false,
                };
                if replace {
                    target.rating_system = bound_xmltv_rating_system(&system)?;
                    target.content_rating = Some(label);
                    target.policy_rating_value = policy_value;
                }
            }
        }
        Ok(())
    }
}

fn bound_xmltv_rating_system(system: &str) -> Result<Option<String>, FeedError> {
    if system.len() > 64 || system.chars().any(char::is_control) {
        return Err(FeedError::InvalidValue);
    }
    let cleaned = system.trim();
    Ok((!cleaned.is_empty()).then(|| cleaned.to_owned()))
}

/// Convert only known US broadcast/movie labels into the server's bounded
/// parental-policy ordinal. Unknown systems or labels remain unrated.
fn parse_xmltv_parental_rating(system: &str, label: &str) -> Option<i16> {
    let system = system.trim().to_ascii_uppercase();
    let label = label.trim().to_ascii_uppercase();
    match system.as_str() {
        "MPAA" | "US-MPAA" => match label.as_str() {
            "G" => Some(0),
            "PG" => Some(25),
            "PG-13" => Some(50),
            "R" => Some(75),
            "NC-17" => Some(100),
            _ => None,
        },
        "VCHIP" | "US-TV" | "TV PARENTAL GUIDELINES" => match label.as_str() {
            "TV-Y" => Some(0),
            "TV-Y7" => Some(20),
            "TV-G" => Some(25),
            "TV-PG" => Some(50),
            "TV-14" => Some(75),
            "TV-MA" => Some(100),
            _ => None,
        },
        _ => None,
    }
}

fn append_xml_text(capture: &mut TextCapture, text: &str) -> Result<(), FeedError> {
    let text = quick_xml::escape::unescape(text).map_err(|_| FeedError::InvalidStructure)?;
    capture.push(&text)
}

fn is_legal_xml_character(character: char) -> bool {
    matches!(character, '\t' | '\n' | '\r')
        || ('\u{20}'..='\u{D7FF}').contains(&character)
        || ('\u{E000}'..='\u{FFFD}').contains(&character)
        || ('\u{10000}'..='\u{10FFFF}').contains(&character)
}

fn clean_xml_text(value: String, max_bytes: usize) -> Result<String, FeedError> {
    let value = value.split_whitespace().collect::<Vec<_>>().join(" ");
    if value.len() > max_bytes || value.chars().any(char::is_control) {
        return Err(FeedError::InvalidValue);
    }
    Ok(value)
}

fn validate_xml_id(value: &str) -> Result<(), FeedError> {
    if value.is_empty()
        || value.len() > MAX_CHANNEL_ID_BYTES
        || value
            .chars()
            .any(|character| character.is_control() || character.is_whitespace())
    {
        return Err(FeedError::InvalidValue);
    }
    Ok(())
}

fn parse_xml_attrs(
    element: &quick_xml::events::BytesStart<'_>,
    decoder: quick_xml::encoding::Decoder,
) -> Result<HashMap<String, String>, FeedError> {
    let mut attrs = HashMap::new();
    for (index, attribute) in element.attributes().with_checks(true).enumerate() {
        if index >= MAX_XMLTV_ATTRIBUTES {
            return Err(FeedError::LimitExceeded);
        }
        let attribute = attribute.map_err(|_| FeedError::InvalidStructure)?;
        if attribute.value.len() > MAX_XMLTV_ATTRIBUTE_BYTES {
            return Err(FeedError::LimitExceeded);
        }
        let key = std::str::from_utf8(attribute.key.as_ref())
            .map_err(|_| FeedError::InvalidEncoding)?
            .to_ascii_lowercase();
        let value = attribute
            .decoded_and_normalized_value(quick_xml::XmlVersion::Implicit1_0, decoder)
            .map_err(|_| FeedError::InvalidStructure)?
            .into_owned();
        if value.len() > MAX_XMLTV_ATTRIBUTE_BYTES || value.chars().any(char::is_control) {
            return Err(FeedError::LimitExceeded);
        }
        if attrs.insert(key, value).is_some() {
            return Err(FeedError::InvalidStructure);
        }
    }
    Ok(attrs)
}

fn parse_xmltv_time(value: &str) -> Result<DateTime<Utc>, FeedError> {
    let parsed = DateTime::<FixedOffset>::parse_from_str(value, "%Y%m%d%H%M%S %z")
        .or_else(|_| DateTime::<FixedOffset>::parse_from_str(value, "%Y%m%d%H%M %z"))
        .map_err(|_| FeedError::InvalidValue)?;
    Ok(parsed.with_timezone(&Utc))
}

#[cfg(test)]
mod tests {
    use super::{FeedError, MAX_XMLTV_ATTRIBUTES, parse_m3u, parse_xmltv};
    use url::Url;

    fn source() -> Url {
        Url::parse("https://guide.example/playlist.m3u").unwrap()
    }

    fn allowlist() -> Vec<Url> {
        vec![Url::parse("https://guide.example").unwrap()]
    }

    #[test]
    fn parses_bounded_m3u_entries_and_resolves_relative_streams() {
        let input = b"\xef\xbb\xbf#EXTM3U\r\n#EXTINF:-1 tvg-id=\"one\" tvg-name=\"News\" group-title=\"Local\" tvg-logo=\"/logos/news.png\",Display Name\r\n/live/news.ts\r\n";
        let channels = parse_m3u(input, &source(), &allowlist()).unwrap();
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].source_id.as_deref(), Some("one"));
        assert_eq!(channels[0].name, "News");
        assert_eq!(channels[0].group.as_deref(), Some("Local"));
        assert_eq!(
            channels[0].stream_url.as_str(),
            "https://guide.example/live/news.ts"
        );
        assert_eq!(
            channels[0].logo_url.as_ref().unwrap().as_str(),
            "https://guide.example/logos/news.png"
        );
        let bad_logo = b"#EXTM3U\n#EXTINF:-1 tvg-logo=\"https://other.example/logo.png\",News\n/live/news.ts\n";
        assert_eq!(
            parse_m3u(bad_logo, &source(), &allowlist()),
            Err(FeedError::DisallowedOrigin)
        );
    }

    #[tokio::test]
    async fn origin_pins_are_required_and_reject_unpinned_network_targets() {
        use std::net::IpAddr;

        use super::{OriginPin, approved_origin_pinned, approved_urls, fetch_bounded_feed};

        let pin = OriginPin {
            origin: "http://tuner.example:8080/".to_owned(),
            addresses: vec!["192.168.1.20".parse::<IpAddr>().unwrap()],
        };
        let urls = approved_urls(std::slice::from_ref(&pin)).unwrap();
        let allowed = Url::parse("http://tuner.example:8080/channels.m3u").unwrap();
        assert!(approved_origin_pinned(&allowed, std::slice::from_ref(&pin)).unwrap());
        assert_eq!(urls[0].as_str(), "http://tuner.example:8080/");

        let unpinned = Url::parse("http://other.example:8080/channels.m3u").unwrap();
        assert!(!approved_origin_pinned(&unpinned, std::slice::from_ref(&pin)).unwrap());
        assert!(
            fetch_bounded_feed(unpinned.as_str(), std::slice::from_ref(&pin), 1024)
                .await
                .is_err()
        );

        let bad_pin = OriginPin {
            origin: "http://tuner.example:8080/".to_owned(),
            addresses: vec!["0.0.0.0".parse().unwrap()],
        };
        assert!(approved_urls(&[bad_pin]).is_err());
    }

    #[tokio::test]
    async fn pinned_fetch_reads_direct_body_and_enforces_chunked_limit() {
        use std::{
            io::{Read, Write},
            net::{Ipv4Addr, TcpListener},
            thread,
            time::Duration,
        };

        use super::{OriginPin, fetch_bounded_feed};

        fn one_response(raw_response: Vec<u8>) -> (String, thread::JoinHandle<()>) {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            listener
                .set_nonblocking(false)
                .expect("local fixture listener should be blocking");
            let address = listener.local_addr().unwrap();
            let worker = thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut request = [0_u8; 2048];
                let _ = socket.read(&mut request);
                socket.write_all(&raw_response).unwrap();
                socket.flush().unwrap();
            });
            (format!("http://{address}/feed"), worker)
        }

        fn pin_for(url: &str) -> OriginPin {
            let parsed = Url::parse(url).unwrap();
            OriginPin {
                origin: format!(
                    "{}://{}:{}/",
                    parsed.scheme(),
                    parsed.host_str().unwrap(),
                    parsed.port().unwrap()
                ),
                addresses: vec![Ipv4Addr::LOCALHOST.into()],
            }
        }

        let body = b"#EXTM3U\n#EXTINF:-1,News\n/live.ts\n";
        let chunked = format!(
            "HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n{:x}\r\n{}\r\n0\r\n\r\n",
            body.len(),
            String::from_utf8_lossy(body)
        );
        let (url, worker) = one_response(chunked.into_bytes());
        let pin = pin_for(&url);
        assert_eq!(fetch_bounded_feed(&url, &[pin], 1024).await.unwrap(), body);
        worker.join().unwrap();

        let chunked = b"HTTP/1.1 200 OK\r\nTransfer-Encoding: chunked\r\nConnection: close\r\n\r\n6\r\nabcdef\r\n0\r\n\r\n";
        let (url, worker) = one_response(chunked.to_vec());
        let pin = pin_for(&url);
        assert_eq!(
            fetch_bounded_feed(&url, &[pin], 5).await,
            Err(super::FeedError::TooLarge)
        );
        worker.join().unwrap();
    }

    #[tokio::test]
    async fn pinned_fetch_rejects_redirects_and_non_identity_content_encoding() {
        use std::{
            io::{Read, Write},
            net::{Ipv4Addr, TcpListener},
            thread,
            time::Duration,
        };

        use super::{FeedError, OriginPin, fetch_bounded_feed};

        fn serve(raw_response: Vec<u8>) -> (String, thread::JoinHandle<()>) {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let address = listener.local_addr().unwrap();
            let worker = thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut request = [0_u8; 2048];
                let _ = socket.read(&mut request);
                socket.write_all(&raw_response).unwrap();
            });
            (format!("http://{address}/feed"), worker)
        }

        fn pin_for(url: &str) -> OriginPin {
            let parsed = Url::parse(url).unwrap();
            OriginPin {
                origin: format!(
                    "{}://{}:{}/",
                    parsed.scheme(),
                    parsed.host_str().unwrap(),
                    parsed.port().unwrap()
                ),
                addresses: vec![Ipv4Addr::LOCALHOST.into()],
            }
        }

        let redirect = b"HTTP/1.1 302 Found\r\nLocation: http://127.0.0.1/elsewhere\r\nContent-Length: 0\r\nConnection: close\r\n\r\n";
        let (url, worker) = serve(redirect.to_vec());
        assert_eq!(
            fetch_bounded_feed(&url, &[pin_for(&url)], 1024).await,
            Err(FeedError::DisallowedOrigin)
        );
        worker.join().unwrap();

        let encoded = b"HTTP/1.1 200 OK\r\nContent-Encoding: gzip\r\nContent-Length: 1\r\nConnection: close\r\n\r\nx";
        let (url, worker) = serve(encoded.to_vec());
        assert_eq!(
            fetch_bounded_feed(&url, &[pin_for(&url)], 1024).await,
            Err(FeedError::InvalidStructure)
        );
        worker.join().unwrap();
    }

    #[tokio::test]
    async fn live_transport_source_accepts_direct_ts_and_rejects_manifest_mime() {
        use std::{
            io::{Read, Write},
            net::{Ipv4Addr, TcpListener},
            thread,
            time::Duration,
        };

        use super::{FeedError, OriginPin, open_direct_ts_stream};

        fn serve(response: Vec<u8>) -> (String, OriginPin, thread::JoinHandle<()>) {
            let listener = TcpListener::bind((Ipv4Addr::LOCALHOST, 0)).unwrap();
            let address = listener.local_addr().unwrap();
            let url = format!("http://{address}/live.ts");
            let pin = OriginPin {
                origin: format!("http://{address}/"),
                addresses: vec![Ipv4Addr::LOCALHOST.into()],
            };
            let worker = thread::spawn(move || {
                let (mut socket, _) = listener.accept().unwrap();
                socket
                    .set_read_timeout(Some(Duration::from_secs(3)))
                    .unwrap();
                let mut request = [0_u8; 2048];
                let _ = socket.read(&mut request);
                socket.write_all(&response).unwrap();
            });
            (url, pin, worker)
        }

        let (url, pin, worker) = serve(
            b"HTTP/1.1 200 OK\r\nContent-Type: video/mp2t\r\nContent-Length: 4\r\nConnection: close\r\n\r\nTS!!".to_vec(),
        );
        let response = open_direct_ts_stream(&url, &[pin]).await.unwrap();
        assert_eq!(response.bytes().await.unwrap().as_ref(), b"TS!!");
        worker.join().unwrap();

        let (url, pin, worker) = serve(
            b"HTTP/1.1 200 OK\r\nContent-Type: application/vnd.apple.mpegurl\r\nContent-Length: 6\r\nConnection: close\r\n\r\n#EXTM3U".to_vec(),
        );
        assert_eq!(
            open_direct_ts_stream(&url, &[pin]).await.map(|_| ()),
            Err(FeedError::InvalidStructure)
        );
        worker.join().unwrap();
    }

    #[test]
    fn rejects_m3u_origin_escape_malformed_attributes_and_duplicate_ids() {
        let escape = b"#EXTM3U\n#EXTINF:-1,News\nhttp://127.0.0.1/stream\n";
        assert_eq!(
            parse_m3u(escape, &source(), &allowlist()),
            Err(FeedError::DisallowedOrigin)
        );

        let malformed = b"#EXTM3U\n#EXTINF:-1 tvg-id=\"broken,News\n/live\n";
        assert!(parse_m3u(malformed, &source(), &allowlist()).is_err());

        let duplicate =
            b"#EXTM3U\n#EXTINF:-1 tvg-id=\"same\",A\n/a\n#EXTINF:-1 tvg-id=\"same\",B\n/b\n";
        assert_eq!(
            parse_m3u(duplicate, &source(), &allowlist()),
            Err(FeedError::DuplicateId)
        );
    }

    #[test]
    fn parses_xmltv_text_and_timezone_offsets_without_dtds() {
        let input = br#"<?xml version="1.0"?>
<tv><channel id="news.example"><display-name>Law &amp; Order News &#x26; Weather</display-name><icon src="/logos/news.png"/></channel>
<programme channel="news.example" start="20260927120000 +0930" stop="20260927130000 +0930"><title lang="en">Midday &amp; More</title><desc>Local bulletin</desc><category>News</category></programme></tv>"#;
        let (channels, programs) = parse_xmltv(input, &source(), &allowlist()).unwrap();
        assert_eq!(channels[0].display_name, "Law & Order News & Weather");
        assert_eq!(
            channels[0].icon_url.as_ref().unwrap().as_str(),
            "https://guide.example/logos/news.png"
        );
        assert_eq!(programs[0].title, "Midday & More");
        assert_eq!(programs[0].description.as_deref(), Some("Local bulletin"));
        assert_eq!(programs[0].start.to_rfc3339(), "2026-09-27T02:30:00+00:00");

        let cdata = br#"<tv><channel id="c"><display-name><![CDATA[A &notentity; B]]></display-name></channel></tv>"#;
        let (channels, _) = parse_xmltv(cdata, &source(), &allowlist()).unwrap();
        assert_eq!(channels[0].display_name, "A &notentity; B");

        let dtd = br#"<!DOCTYPE tv [<!ENTITY x "expanded">]><tv><channel id="c"><display-name>&x;</display-name></channel></tv>"#;
        assert!(parse_xmltv(dtd, &source(), &allowlist()).is_err());

        let foreign_icon = br#"<tv><channel id="c"><display-name>C</display-name><icon src="https://other.example/icon.png"/></channel></tv>"#;
        assert!(parse_xmltv(foreign_icon, &source(), &allowlist()).is_err());
    }

    #[test]
    fn safely_skips_bounded_standard_optional_xmltv_children() {
        let input = br#"<tv>
<channel id="c"><display-name>Example One</display-name><url>https://untrusted.example/</url><credits><writer>Ignored credit</writer></credits></channel>
<programme channel="c" start="20260927120000 +0000" stop="20260927123000 +0000"><title lang="en">The Show</title><length units="minutes">30</length><credits><actor role="host">Ignored host</actor></credits><rating system="example"><value>TV-PG</value></rating><icon src="https://untrusted.example/icon.png"/></programme>
</tv>"#;
        let (channels, programs) = parse_xmltv(input, &source(), &allowlist()).unwrap();
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].display_name, "Example One");
        assert_eq!(programs.len(), 1);
        assert_eq!(programs[0].title, "The Show");
        assert_eq!(programs[0].content_rating.as_deref(), Some("TV-PG"));
        assert_eq!(programs[0].policy_rating_value, None);
        assert_eq!(channels[0].icon_url, None);
    }

    #[test]
    fn xmltv_ratings_are_bounded_and_unknown_values_remain_unrated() {
        use super::parse_xmltv_parental_rating;

        let input = br#"<tv><channel id="c"><display-name>Example</display-name></channel>
<programme channel="c" start="20260927120000 +0000" stop="20260927130000 +0000"><title>Rated show</title><rating system="VCHIP"><value>TV-14</value></rating><rating system="MPAA"><value>NC-17</value></rating></programme>
<programme channel="c" start="20260927130000 +0000" stop="20260927140000 +0000"><title>Unrecognized label</title><rating system="example"><value>TV-MA</value></rating></programme></tv>"#;
        let (_, programs) = parse_xmltv(input, &source(), &allowlist()).unwrap();
        assert_eq!(programs[0].rating_system.as_deref(), Some("MPAA"));
        assert_eq!(programs[0].content_rating.as_deref(), Some("NC-17"));
        assert_eq!(programs[0].policy_rating_value, Some(100));
        assert_eq!(programs[1].content_rating.as_deref(), Some("TV-MA"));
        assert_eq!(programs[1].policy_rating_value, None);

        assert_eq!(parse_xmltv_parental_rating("VCHIP", "TV-MA"), Some(100));
        assert_eq!(parse_xmltv_parental_rating("MPAA", "PG-13"), Some(50));
        assert_eq!(parse_xmltv_parental_rating("unknown", "TV-MA"), None);
    }

    #[test]
    fn xmltv_requires_one_root_and_valid_record_depth() {
        for input in [
            br#"<tv></tv><tv></tv>"#.as_slice(),
            br#"<wrapper><tv></tv></wrapper>"#.as_slice(),
            br#"<tv><wrapper><channel id="c"><display-name>C</display-name></channel></wrapper></tv>"#.as_slice(),
            br#"<tv><channel id="c"><unknown/></channel></tv>"#.as_slice(),
            br#"<tv><channel id="c"><display-name>C<unknown/>D</display-name></channel></tv>"#.as_slice(),
            br#"<tv><channel id="c"><display-name>C<tv/></display-name></channel></tv>"#.as_slice(),
            br#"<tv><channel id="c"><display-name>C</display-name></channel><wrapper><programme channel="c" start="20260927120000 +0000" stop="20260927130000 +0000"><title>Show</title></programme></wrapper></tv>"#.as_slice(),
            br#"<tv>unexpected text</tv>"#.as_slice(),
            br#"<tv><channel id="c"><display-name>C</display-name></channel></tv><extra/>"#.as_slice(),
        ] {
            assert!(parse_xmltv(input, &source(), &allowlist()).is_err());
        }
    }

    #[test]
    fn rejects_xmltv_missing_channels_bad_time_and_unbounded_attributes() {
        let missing = br#"<tv><programme channel="missing" start="20260927120000 +0000" stop="20260927130000 +0000"><title>Show</title></programme></tv>"#;
        assert!(parse_xmltv(missing, &source(), &allowlist()).is_err());
        let bad_time = br#"<tv><channel id="c"><display-name>C</display-name></channel><programme channel="c" start="bad" stop="20260927130000 +0000"><title>Show</title></programme></tv>"#;
        assert!(parse_xmltv(bad_time, &source(), &allowlist()).is_err());

        let long_attribute = "x".repeat(4 * 1024 + 1);
        let oversized = format!(
            "<tv><channel id=\"c\" large=\"{long_attribute}\"><display-name>C</display-name></channel></tv>"
        );
        assert!(parse_xmltv(oversized.as_bytes(), &source(), &allowlist()).is_err());

        let too_many_attributes = (0..MAX_XMLTV_ATTRIBUTES + 1)
            .map(|index| format!(" a{index}=\"x\""))
            .collect::<String>();
        let oversized = format!(
            "<tv><channel id=\"c\"{too_many_attributes}><display-name>C</display-name></channel></tv>"
        );
        assert!(parse_xmltv(oversized.as_bytes(), &source(), &allowlist()).is_err());
    }
}
