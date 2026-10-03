use std::collections::HashMap;

use chrono::NaiveDate;

const MAX_TAGS: usize = 128;
const MAX_TEXT_BYTES: usize = 512;

#[derive(Clone, Debug, Default)]
pub(crate) struct EmbeddedAudioMetadata {
    pub title: Option<String>,
    pub album: Option<String>,
    pub artists: Vec<String>,
    pub album_artists: Vec<String>,
    pub premiere_date: Option<NaiveDate>,
    pub genres: Vec<String>,
    pub track_number: Option<i32>,
    pub disc_number: Option<i32>,
}

impl EmbeddedAudioMetadata {
    pub(crate) fn from_tags(
        format: Option<&HashMap<String, String>>,
        stream: Option<&HashMap<String, String>>,
    ) -> Self {
        let sources = TagSources { format, stream };
        let artists = sources.text(&["artist"]).into_iter().collect::<Vec<_>>();
        let album_artists = sources
            .text(&["album_artist", "albumartist"])
            .map(|name| vec![name])
            .unwrap_or_else(|| artists.clone());
        Self {
            title: sources.text(&["title"]),
            album: sources.text(&["album"]),
            artists,
            album_artists,
            premiere_date: sources
                .text(&["date", "year"])
                .as_deref()
                .and_then(parse_date),
            genres: sources.text(&["genre"]).into_iter().collect(),
            track_number: sources
                .text(&["track", "tracknumber"])
                .as_deref()
                .and_then(parse_number),
            disc_number: sources
                .text(&["disc", "discnumber"])
                .as_deref()
                .and_then(parse_number),
        }
    }
}

struct TagSources<'a> {
    format: Option<&'a HashMap<String, String>>,
    stream: Option<&'a HashMap<String, String>>,
}

impl TagSources<'_> {
    fn text(&self, aliases: &[&str]) -> Option<String> {
        for tags in [self.format, self.stream].into_iter().flatten() {
            if tags.len() > MAX_TAGS {
                continue;
            }
            for alias in aliases {
                let mut values = tags
                    .iter()
                    .filter(|(key, _)| key.eq_ignore_ascii_case(alias))
                    .map(|(_, value)| value.trim());
                let Some(value) = values.next() else {
                    continue;
                };
                // Reject conflicting case variants rather than depend on hash
                // iteration order. Oversized and control-bearing values are
                // discarded rather than truncated into another credit's name.
                if values.any(|other| other != value)
                    || value.is_empty()
                    || value.len() > MAX_TEXT_BYTES
                    || value.chars().any(char::is_control)
                {
                    continue;
                }
                return Some(value.to_owned());
            }
        }
        None
    }
}

fn parse_date(value: &str) -> Option<NaiveDate> {
    if value.len() == 4 && value.bytes().all(|byte| byte.is_ascii_digit()) {
        let year = value.parse::<i32>().ok()?;
        return (1..=9999)
            .contains(&year)
            .then(|| NaiveDate::from_ymd_opt(year, 1, 1))
            .flatten();
    }
    if value.len() != 10 || value.as_bytes()[4] != b'-' || value.as_bytes()[7] != b'-' {
        return None;
    }
    let date = NaiveDate::parse_from_str(value, "%Y-%m-%d").ok()?;
    use chrono::Datelike;
    (1..=9999).contains(&date.year()).then_some(date)
}

fn parse_number(value: &str) -> Option<i32> {
    let mut parts = value.split('/');
    let parse = |part: &str| {
        (!part.is_empty() && part.len() <= 10 && part.bytes().all(|b| b.is_ascii_digit()))
            .then(|| part.parse::<i32>().ok())
            .flatten()
    };
    let number = parse(parts.next()?)?;
    if let Some(total) = parts.next()
        && parse(total)? == 0
    {
        return None;
    }
    parts.next().is_none().then_some(number)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tags(values: &[(&str, &str)]) -> HashMap<String, String> {
        values
            .iter()
            .map(|(k, v)| (k.to_string(), v.to_string()))
            .collect()
    }

    #[test]
    fn opaque_reference_flac_tags_keep_semicolons_and_number_totals() {
        let raw = tags(&[
            ("TITLE", "Embedded Alpha Title"),
            ("artist", "Embedded Lead; Embedded Guest"),
            ("album_artist", "Embedded Album Artist"),
            ("album", "Embedded Album"),
            ("date", "2021-04-05"),
            ("track", "3/12"),
            ("disc", "2/4"),
            ("genre", "Rock; Jazz"),
            ("comment", "Synthetic embedded overview"),
        ]);
        let parsed = EmbeddedAudioMetadata::from_tags(Some(&raw), None);
        assert_eq!(parsed.title.as_deref(), Some("Embedded Alpha Title"));
        assert_eq!(parsed.album.as_deref(), Some("Embedded Album"));
        assert_eq!(parsed.artists, ["Embedded Lead; Embedded Guest"]);
        assert_eq!(parsed.album_artists, ["Embedded Album Artist"]);
        assert_eq!(parsed.premiere_date, NaiveDate::from_ymd_opt(2021, 4, 5));
        assert_eq!(parsed.track_number, Some(3));
        assert_eq!(parsed.disc_number, Some(2));
        assert_eq!(parsed.genres, ["Rock; Jazz"]);
    }

    #[test]
    fn year_only_and_missing_album_artist_match_the_reference() {
        let raw = tags(&[
            ("artist", "Embedded Lead"),
            ("date", "2023"),
            ("track", "04/12"),
        ]);
        let parsed = EmbeddedAudioMetadata::from_tags(Some(&raw), None);
        assert_eq!(parsed.album_artists, ["Embedded Lead"]);
        assert_eq!(parsed.premiere_date, NaiveDate::from_ymd_opt(2023, 1, 1));
        assert_eq!(parsed.track_number, Some(4));
        assert!(parsed.album.is_none());
    }

    #[test]
    fn format_tags_precede_audio_stream_tags_and_missing_fields_fall_back() {
        let format = tags(&[("title", "Container title")]);
        let stream = tags(&[
            ("title", "Stream title"),
            ("albumartist", "Stream artist"),
            ("discnumber", "02"),
        ]);
        let parsed = EmbeddedAudioMetadata::from_tags(Some(&format), Some(&stream));
        assert_eq!(parsed.title.as_deref(), Some("Container title"));
        assert_eq!(parsed.album_artists, ["Stream artist"]);
        assert_eq!(parsed.disc_number, Some(2));
    }

    #[test]
    fn invalid_dates_and_numbers_do_not_become_catalog_fields() {
        assert_eq!(parse_number("0"), Some(0));
        assert_eq!(parse_number("0/12"), Some(0));
        for value in ["-1", "2147483648", "3/no", "3/0", "3/12/2", " 3", ""] {
            assert_eq!(parse_number(value), None, "{value}");
        }
        for value in [
            "0000",
            "2023-02-29",
            "2021-04",
            "99999",
            "2020-01-01T00:00:00Z",
            "ééééé",
        ] {
            assert_eq!(parse_date(value), None, "{value}");
        }
        assert_eq!(
            parse_date("2024-02-29"),
            NaiveDate::from_ymd_opt(2024, 2, 29)
        );
    }

    #[test]
    fn tags_are_bounded_and_ambiguous_case_variants_are_rejected() {
        let raw = tags(&[
            ("artist", "Lead"),
            ("ARTIST", "Other"),
            ("album", "Unsafe\nAlbum"),
        ]);
        let parsed = EmbeddedAudioMetadata::from_tags(Some(&raw), None);
        assert!(parsed.artists.is_empty());
        assert!(parsed.album.is_none());
        let raw = tags(&[("title", &"é".repeat(257))]);
        assert!(
            EmbeddedAudioMetadata::from_tags(Some(&raw), None)
                .title
                .is_none()
        );
        let raw = (0..129)
            .map(|n| (n.to_string(), n.to_string()))
            .chain([("title".to_string(), "Too many".to_string())])
            .collect();
        assert!(
            EmbeddedAudioMetadata::from_tags(Some(&raw), None)
                .title
                .is_none()
        );
    }
}
