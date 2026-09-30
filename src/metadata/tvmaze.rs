use std::{
    net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr},
    sync::OnceLock,
    time::{Duration, Instant},
};

use chrono::NaiveDate;
use serde::Deserialize;

const HOST: &str = "api.tvmaze.com";
const RESPONSE_LIMIT: usize = 2 * 1024 * 1024;
const DESCRIPTION_LIMIT: usize = 20 * 1024;
const GENRE_COUNT_LIMIT: usize = 64;
const GENRE_BYTES_LIMIT: usize = 64;
const ATTRIBUTION_URL: &str = "https://www.tvmaze.com/api";
const ATTRIBUTION_LICENSE: &str = "CC BY-SA";

#[derive(Clone, Debug, PartialEq)]
pub(super) struct TvMazeMetadata {
    pub external_id: String,
    pub title: String,
    pub overview: Option<String>,
    pub premiere_date: Option<NaiveDate>,
    pub genres: Vec<String>,
    /// A community score only. It is never a parental classification.
    pub community_score: Option<f64>,
    pub attribution_name: &'static str,
    pub attribution_url: &'static str,
    pub attribution_license: &'static str,
}

#[derive(Debug, PartialEq, Eq)]
pub(super) enum LookupError {
    NoPublicAddress,
    Network,
    HttpStatus(u16),
    TooLarge,
    InvalidData,
    Ambiguous,
}

#[derive(Deserialize)]
struct SearchResult {
    show: TvMazeShow,
}

#[derive(Deserialize)]
struct TvMazeShow {
    id: u64,
    name: String,
    summary: Option<String>,
    premiered: Option<String>,
    #[serde(default)]
    genres: Vec<String>,
    rating: Option<TvMazeRating>,
}

#[derive(Deserialize)]
struct TvMazeRating {
    average: Option<f64>,
}

pub(super) async fn lookup_exact_title(title: &str) -> Result<Option<TvMazeMetadata>, LookupError> {
    let title = title.trim();
    if title.is_empty() || title.len() > 512 {
        return Ok(None);
    }
    let client = provider_client().await?;
    let response = tokio::time::timeout(
        Duration::from_secs(10),
        client
            .get(format!("https://{HOST}/search/shows"))
            .query(&[("q", title)])
            .send(),
    )
    .await
    .map_err(|_| LookupError::Network)?
    .map_err(|_| LookupError::Network)?;
    if !response.status().is_success() {
        return Err(LookupError::HttpStatus(response.status().as_u16()));
    }
    let body = read_bounded(response).await?;
    parse_results(&body, title)
}

/// Resolve a TVMaze identifier that the operator supplied in the local NFO.
/// The request stays on the fixed TVMaze API origin and never follows redirects.
pub(super) async fn lookup_show_id(id: u64) -> Result<Option<TvMazeMetadata>, LookupError> {
    if id == 0 {
        return Err(LookupError::InvalidData);
    }
    let client = provider_client().await?;
    let response = tokio::time::timeout(
        Duration::from_secs(10),
        client.get(format!("https://{HOST}/shows/{id}")).send(),
    )
    .await
    .map_err(|_| LookupError::Network)?
    .map_err(|_| LookupError::Network)?;
    if response.status() == reqwest::StatusCode::NOT_FOUND {
        return Ok(None);
    }
    if !response.status().is_success() {
        return Err(LookupError::HttpStatus(response.status().as_u16()));
    }
    let body = read_bounded(response).await?;
    let show: TvMazeShow = serde_json::from_slice(&body).map_err(|_| LookupError::InvalidData)?;
    if show.id != id {
        return Err(LookupError::InvalidData);
    }
    Ok(Some(metadata_from_show(&show)))
}

async fn provider_client() -> Result<reqwest::Client, LookupError> {
    let addresses = public_addresses().await?;
    rate_limit().await;
    reqwest::Client::builder()
        .https_only(true)
        .no_proxy()
        .redirect(reqwest::redirect::Policy::none())
        .connect_timeout(Duration::from_secs(3))
        .timeout(Duration::from_secs(8))
        .resolve_to_addrs(HOST, &addresses)
        .user_agent("Puffinbox metadata provider/0.1")
        .build()
        .map_err(|_| LookupError::Network)
}

fn parse_results(body: &[u8], title: &str) -> Result<Option<TvMazeMetadata>, LookupError> {
    let mut results: Vec<SearchResult> =
        serde_json::from_slice(body).map_err(|_| LookupError::InvalidData)?;
    if results.len() > 100 {
        results.truncate(100);
    }
    let target = normalize_title(title);
    let matching: Vec<_> = results
        .into_iter()
        .map(|result| result.show)
        .filter(|show| normalize_title(&show.name) == target)
        .take(2)
        .collect();
    let show = match matching.as_slice() {
        [] => return Ok(None),
        [show] => show,
        _ => return Err(LookupError::Ambiguous),
    };

    Ok(Some(metadata_from_show(show)))
}

fn metadata_from_show(show: &TvMazeShow) -> TvMazeMetadata {
    let overview = show
        .summary
        .as_deref()
        .map(strip_html)
        .map(|value| value.trim().to_owned())
        .filter(|value| !value.is_empty())
        .map(|value| truncate_utf8(&value, DESCRIPTION_LIMIT));
    let premiere_date = show
        .premiered
        .as_deref()
        .and_then(|value| NaiveDate::parse_from_str(value, "%Y-%m-%d").ok());
    let mut genres = Vec::new();
    for genre in show.genres.iter().take(GENRE_COUNT_LIMIT) {
        let genre = genre.trim();
        if genre.is_empty() {
            continue;
        }
        let genre = truncate_utf8(genre, GENRE_BYTES_LIMIT);
        if !genres
            .iter()
            .any(|existing: &String| existing.eq_ignore_ascii_case(&genre))
        {
            genres.push(genre);
        }
    }
    let community_score = show
        .rating
        .as_ref()
        .and_then(|rating| rating.average)
        .filter(|score| score.is_finite() && (0.0..=10.0).contains(score));
    TvMazeMetadata {
        external_id: show.id.to_string(),
        title: truncate_utf8(show.name.trim(), 512),
        overview,
        premiere_date,
        genres,
        community_score,
        attribution_name: "TVMaze",
        attribution_url: ATTRIBUTION_URL,
        attribution_license: ATTRIBUTION_LICENSE,
    }
}

async fn read_bounded(response: reqwest::Response) -> Result<Vec<u8>, LookupError> {
    if response
        .content_length()
        .is_some_and(|length| length > RESPONSE_LIMIT as u64)
    {
        return Err(LookupError::TooLarge);
    }
    let mut response = response;
    let mut output = Vec::new();
    while let Some(chunk) = tokio::time::timeout(Duration::from_secs(3), response.chunk())
        .await
        .map_err(|_| LookupError::Network)?
        .map_err(|_| LookupError::Network)?
    {
        if output.len().saturating_add(chunk.len()) > RESPONSE_LIMIT {
            return Err(LookupError::TooLarge);
        }
        output.extend_from_slice(&chunk);
    }
    Ok(output)
}

async fn public_addresses() -> Result<Vec<SocketAddr>, LookupError> {
    let resolved =
        tokio::time::timeout(Duration::from_secs(3), tokio::net::lookup_host((HOST, 443)))
            .await
            .map_err(|_| LookupError::Network)?
            .map_err(|_| LookupError::Network)?;
    let mut addresses: Vec<_> = resolved
        .filter(|address| is_public_address(address.ip()))
        .map(|address| SocketAddr::new(address.ip(), 443))
        .collect();
    addresses.sort_unstable();
    addresses.dedup();
    if addresses.is_empty() {
        return Err(LookupError::NoPublicAddress);
    }
    Ok(addresses)
}

fn is_public_address(address: IpAddr) -> bool {
    match address {
        IpAddr::V4(address) => is_public_v4(address),
        IpAddr::V6(address) => is_public_v6(address),
    }
}

fn is_public_v4(address: Ipv4Addr) -> bool {
    if address.is_private()
        || address.is_loopback()
        || address.is_link_local()
        || address.is_broadcast()
        || address.is_unspecified()
        || address.is_multicast()
    {
        return false;
    }
    let [a, b, c, _] = address.octets();
    !matches!(
        (a, b, c),
        (0, _, _) // this network
            | (100, 64..=127, _) // carrier-grade NAT
            | (192, 0, 0) // protocol assignments
            | (192, 0, 2) // documentation
            | (192, 88, 99) // deprecated 6to4 relay
            | (198, 18..=19, _) // benchmarking
            | (198, 51, 100) // documentation
            | (203, 0, 113) // documentation
    ) && a < 224
}

fn is_public_v6(address: Ipv6Addr) -> bool {
    if address.is_loopback()
        || address.is_unspecified()
        || address.is_multicast()
        || address.is_unique_local()
        || address.is_unicast_link_local()
    {
        return false;
    }
    if let Some(v4) = address.to_ipv4_mapped() {
        return is_public_v4(v4);
    }
    let segments = address.segments();
    // Permit only the global unicast allocation (2000::/3), then deny the
    // special-purpose protocol, documentation, transition and reserved
    // ranges inside it. IPv4-compatible, translation and other unallocated
    // IPv6 space is rejected by the initial allow range.
    segments[0] & 0xe000 == 0x2000
        && !(segments[0] == 0x2001 && segments[1] <= 0x01ff)
        && !(segments[0] == 0x2001 && segments[1] == 0x0db8)
        && segments[0] != 0x2002
        && !(segments[0] == 0x3fff && segments[1] & 0xfff0 == 0)
}

fn reserve_rate_limit_slot(next: &mut Instant, now: Instant) -> Duration {
    let scheduled = (*next).max(now);
    let wait = scheduled.saturating_duration_since(now);
    *next = scheduled + Duration::from_secs(1);
    wait
}

async fn rate_limit() {
    static NEXT_REQUEST: OnceLock<tokio::sync::Mutex<Instant>> = OnceLock::new();
    let lock = NEXT_REQUEST.get_or_init(|| tokio::sync::Mutex::new(Instant::now()));
    let wait = {
        let mut next = lock.lock().await;
        reserve_rate_limit_slot(&mut next, Instant::now())
    };
    if !wait.is_zero() {
        tokio::time::sleep(wait).await;
    }
}

fn normalize_title(value: &str) -> String {
    value
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .to_lowercase()
}

fn strip_html(input: &str) -> String {
    let mut output = String::with_capacity(input.len().min(DESCRIPTION_LIMIT));
    let mut in_tag = false;
    let mut entity = String::new();
    let mut in_entity = false;
    for character in input.chars() {
        if in_tag {
            if character == '>' {
                in_tag = false;
                output.push(' ');
            }
            continue;
        }
        if in_entity {
            if character == ';' {
                output.push(decode_entity(&entity));
                entity.clear();
                in_entity = false;
            } else if entity.len() < 16 && (character.is_ascii_alphanumeric() || character == '#') {
                entity.push(character);
            } else {
                output.push('&');
                output.push_str(&entity);
                output.push(character);
                entity.clear();
                in_entity = false;
            }
            continue;
        }
        match character {
            '<' => in_tag = true,
            '&' => in_entity = true,
            _ => output.push(character),
        }
        if output.len() > DESCRIPTION_LIMIT * 2 {
            break;
        }
    }
    if in_entity {
        output.push('&');
        output.push_str(&entity);
    }
    normalize_whitespace(&output, DESCRIPTION_LIMIT)
}

/// Collapse provider markup and decoded entities to a bounded, single-spaced
/// display string. The parser caps its intermediate buffer before this pass.
fn normalize_whitespace(input: &str, limit: usize) -> String {
    let mut output = String::with_capacity(input.len().min(limit));
    let mut pending_space = false;
    for character in input.chars() {
        if character.is_whitespace() {
            pending_space = !output.is_empty();
            continue;
        }
        let extra = character.len_utf8() + if pending_space { 1 } else { 0 };
        if output.len().saturating_add(extra) > limit {
            break;
        }
        if pending_space {
            output.push(' ');
            pending_space = false;
        }
        output.push(character);
    }
    output
}

fn decode_entity(entity: &str) -> char {
    match entity {
        "amp" => '&',
        "lt" => '<',
        "gt" => '>',
        "quot" => '"',
        "apos" => '\'',
        "nbsp" => ' ',
        _ if entity.starts_with("#x") => u32::from_str_radix(&entity[2..], 16)
            .ok()
            .and_then(char::from_u32)
            .unwrap_or('�'),
        _ if entity.starts_with('#') => entity[1..]
            .parse::<u32>()
            .ok()
            .and_then(char::from_u32)
            .unwrap_or('�'),
        _ => '�',
    }
}

fn truncate_utf8(value: &str, limit: usize) -> String {
    if value.len() <= limit {
        return value.to_owned();
    }
    let mut end = limit;
    while !value.is_char_boundary(end) {
        end -= 1;
    }
    value[..end].to_owned()
}

#[cfg(test)]
mod tests {
    use std::time::{Duration, Instant};

    use std::net::{IpAddr, Ipv4Addr, Ipv6Addr};

    use super::{
        LookupError, decode_entity, is_public_address, normalize_title, normalize_whitespace,
        parse_results, reserve_rate_limit_slot, strip_html, truncate_utf8,
    };

    #[test]
    fn removes_markup_and_decodes_only_bounded_safe_text() {
        assert_eq!(
            strip_html("<p>Tom &amp; Jerry</p>  \n <script>bad</script>"),
            "Tom & Jerry bad"
        );
        assert_eq!(strip_html("<p>A&nbsp;&nbsp;B</p>\tC"), "A B C");
        assert_eq!(decode_entity("unknown"), '�');
        assert_eq!(normalize_title("  The   Example  "), "the example");
    }

    #[test]
    fn collapsed_description_whitespace_respects_utf8_byte_limit() {
        assert_eq!(normalize_whitespace(" café   au lait ", 8), "café au");
        assert_eq!(normalize_whitespace("one   two", 5), "one t");
    }

    #[test]
    fn truncates_only_at_utf8_boundaries() {
        assert_eq!(truncate_utf8("café", 4), "caf");
        assert_eq!(truncate_utf8("café", 5), "café");
    }

    #[test]
    fn exact_provider_match_keeps_community_score_separate_from_policy() {
        let body = br#"[
          {"score":1.0,"show":{"id":22,"name":"A Different Show","summary":"wrong","premiered":"2020-01-01","genres":["Drama"],"rating":{"average":9.8},"image":{"original":"https://untrusted.invalid/poster.jpg"}}},
          {"score":0.9,"show":{"id":23,"name":"The Example","summary":"<p>Safe &amp; useful</p>","premiered":"2021-05-04","genres":["Drama"," drama ","Comedy"],"rating":{"average":8.5},"image":{"original":"http://127.0.0.1/private.png"}}}
        ]"#;
        let metadata = parse_results(body, "  the   example ").unwrap().unwrap();
        assert_eq!(metadata.external_id, "23");
        assert_eq!(metadata.title, "The Example");
        assert_eq!(metadata.overview.as_deref(), Some("Safe & useful"));
        assert_eq!(metadata.premiere_date.unwrap().to_string(), "2021-05-04");
        assert_eq!(metadata.genres, ["Drama", "Comedy"]);
        assert_eq!(metadata.community_score, Some(8.5));
        assert_eq!(metadata.attribution_license, "CC BY-SA");
        let serialized = serde_json::to_string(&metadata.community_score).unwrap();
        assert_eq!(serialized, "8.5");
    }

    #[test]
    fn no_exact_provider_result_is_not_a_title_guess() {
        let body = br#"[{"show":{"id":1,"name":"The Example (US)","genres":[]}}]"#;
        assert_eq!(parse_results(body, "The Example").unwrap(), None);
        assert_eq!(
            parse_results(b"{}", "The Example"),
            Err(super::LookupError::InvalidData)
        );
        let duplicate = br#"[{"show":{"id":1,"name":"The Example","genres":[]}},{"show":{"id":2,"name":"The Example","genres":[]}}]"#;
        assert_eq!(
            parse_results(duplicate, "The Example"),
            Err(LookupError::Ambiguous)
        );
    }

    #[test]
    fn rate_limit_reserves_nonoverlapping_slots() {
        let now = Instant::now();
        let mut next = now;
        assert_eq!(reserve_rate_limit_slot(&mut next, now), Duration::ZERO);
        assert_eq!(
            reserve_rate_limit_slot(&mut next, now),
            Duration::from_secs(1)
        );
        assert_eq!(
            reserve_rate_limit_slot(&mut next, now),
            Duration::from_secs(2)
        );
        assert_eq!(
            reserve_rate_limit_slot(&mut next, now + Duration::from_secs(5)),
            Duration::ZERO
        );
    }

    #[test]
    fn rejects_local_special_and_documentation_dns_results() {
        for address in [
            IpAddr::V4(Ipv4Addr::new(127, 0, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(10, 1, 2, 3)),
            IpAddr::V4(Ipv4Addr::new(169, 254, 2, 9)),
            IpAddr::V4(Ipv4Addr::new(100, 64, 0, 1)),
            IpAddr::V4(Ipv4Addr::new(192, 0, 2, 10)),
            IpAddr::V6(Ipv6Addr::LOCALHOST),
            IpAddr::V6("fc00::1".parse().unwrap()),
            IpAddr::V6("fe80::1".parse().unwrap()),
            IpAddr::V6("2001:db8::1".parse().unwrap()),
            IpAddr::V6("::192.0.2.1".parse().unwrap()),
            IpAddr::V6("3fff::1".parse().unwrap()),
            IpAddr::V6("2001:20::1".parse().unwrap()),
        ] {
            assert!(
                !is_public_address(address),
                "allowed restricted address {address}"
            );
        }
        assert!(is_public_address(IpAddr::V4(Ipv4Addr::new(1, 1, 1, 1))));
        assert!(is_public_address(IpAddr::V6(
            "2606:4700:4700::1111".parse().unwrap()
        )));
        assert!(is_public_address(IpAddr::V6(
            "2001:4860:4860::8888".parse().unwrap()
        )));
    }
}
