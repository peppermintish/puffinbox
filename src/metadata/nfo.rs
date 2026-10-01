use chrono::NaiveDate;
use quick_xml::{
    Reader,
    escape::unescape,
    events::{BytesStart, Event},
};

pub const MAX_NFO_BYTES: usize = 4 * 1024 * 1024;
const MAX_DEPTH: usize = 32;
const MAX_TEXT_BYTES: usize = 24 * 1024;
const MAX_GENRES: usize = 128;
const MAX_EVENTS: usize = 100_000;
const MAX_ATTRIBUTES: usize = 10_000;
const MAX_ATTRIBUTES_PER_ELEMENT: usize = 64;
const MAX_ATTRIBUTE_BYTES: usize = 16 * 1024;

#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub(super) struct LocalNfo {
    pub title: Option<String>,
    pub original_title: Option<String>,
    pub overview: Option<String>,
    pub premiere_date: Option<NaiveDate>,
    pub year: Option<i32>,
    pub genres: Vec<String>,
    pub tags: Vec<String>,
    pub content_rating: Option<String>,
    pub policy_rating_value: Option<i16>,
    /// An explicit TVMaze series choice supplied by the library operator.
    /// Invalid values are remembered so a malformed choice never falls back
    /// to an automatic title search.
    pub tvmaze_id: Option<u64>,
    pub tvmaze_id_invalid: bool,
}

pub(super) fn parse(input: &[u8]) -> Result<LocalNfo, &'static str> {
    if input.len() > MAX_NFO_BYTES {
        return Err("nfo-too-large");
    }
    let mut reader = Reader::from_reader(input);
    reader.config_mut().trim_text(false);
    let mut buffer = Vec::with_capacity(4096);
    let mut stack = Vec::<String>::with_capacity(8);
    let mut fields = LocalNfo::default();
    let mut current_text = String::new();
    let mut current_name: Option<String> = None;
    let mut event_count = 0usize;
    let mut attribute_count = 0usize;
    let mut root_seen = false;
    let mut root_closed = false;

    loop {
        buffer.clear();
        match reader.read_event_into(&mut buffer) {
            Ok(Event::Start(element)) => {
                bump_event(&mut event_count)?;
                validate_attributes(&element, &mut attribute_count)?;
                if stack.is_empty() {
                    if root_seen || root_closed {
                        return Err("nfo-multiple-roots");
                    }
                    root_seen = true;
                }
                if stack.len() >= MAX_DEPTH {
                    return Err("nfo-depth-limit");
                }
                let name = local_name(element.local_name().as_ref())?;
                stack.push(name.clone());
                if stack.len() == 2 && is_supported_field(&name) {
                    current_name = Some(name);
                    current_text.clear();
                }
            }
            Ok(Event::Empty(element)) => {
                bump_event(&mut event_count)?;
                validate_attributes(&element, &mut attribute_count)?;
                if stack.is_empty() {
                    if root_seen || root_closed {
                        return Err("nfo-multiple-roots");
                    }
                    root_seen = true;
                    root_closed = true;
                }
                if stack.len() == 1 {
                    let name = local_name(element.local_name().as_ref())?;
                    if is_supported_field(&name) {
                        apply_field(&mut fields, &name, "")?;
                    }
                }
            }
            Ok(Event::Text(text)) => {
                bump_event(&mut event_count)?;
                let decoded = text.decode().map_err(|_| "nfo-invalid-text")?;
                if stack.is_empty() {
                    if !decoded.trim().is_empty() {
                        return Err("nfo-text-outside-root");
                    }
                    continue;
                }
                if current_name.is_some() {
                    let decoded = unescape(&decoded).map_err(|_| "nfo-invalid-entity")?;
                    append_text(&mut current_text, &decoded)?;
                }
            }
            Ok(Event::CData(text)) => {
                bump_event(&mut event_count)?;
                if stack.is_empty() {
                    return Err("nfo-text-outside-root");
                }
                if current_name.is_some() {
                    let decoded = text.decode().map_err(|_| "nfo-invalid-text")?;
                    append_text(&mut current_text, &decoded)?;
                }
            }
            Ok(Event::End(_)) => {
                bump_event(&mut event_count)?;
                if stack.len() == 2
                    && let Some(name) = current_name.take()
                {
                    apply_field(&mut fields, &name, &current_text)?;
                    current_text.clear();
                }
                if stack.pop().is_none() {
                    return Err("nfo-invalid-structure");
                }
                if stack.is_empty() {
                    root_closed = true;
                }
            }
            Ok(Event::GeneralRef(reference)) => {
                bump_event(&mut event_count)?;
                if stack.is_empty() {
                    return Err("nfo-reference-outside-field");
                }
                let character = if let Some(character) = reference
                    .resolve_char_ref()
                    .map_err(|_| "nfo-invalid-entity")?
                {
                    character
                } else {
                    let entity = std::str::from_utf8(reference.as_ref())
                        .map_err(|_| "nfo-invalid-entity")?;
                    match quick_xml::escape::resolve_predefined_entity(entity) {
                        Some(value) => {
                            let mut characters = value.chars();
                            let character = characters.next().ok_or("nfo-invalid-entity")?;
                            if characters.next().is_some() {
                                return Err("nfo-invalid-entity");
                            }
                            character
                        }
                        None => return Err("nfo-invalid-entity"),
                    }
                };
                if !is_xml_character(character) {
                    return Err("nfo-invalid-entity");
                }
                if current_name.is_some() {
                    append_text(&mut current_text, &character.to_string())?;
                }
            }
            Ok(Event::DocType(_)) => {
                bump_event(&mut event_count)?;
                return Err("nfo-doctype-not-allowed");
            }
            Ok(Event::Eof) => break,
            Ok(_) => bump_event(&mut event_count)?,
            Err(_) => return Err("nfo-invalid-xml"),
        }
    }
    if !root_seen || !root_closed || !stack.is_empty() || current_name.is_some() {
        return Err("nfo-invalid-structure");
    }
    Ok(fields)
}

fn is_xml_character(character: char) -> bool {
    matches!(character, '\u{9}' | '\u{a}' | '\u{d}')
        || ('\u{20}'..='\u{d7ff}').contains(&character)
        || ('\u{e000}'..='\u{fffd}').contains(&character)
        || ('\u{10000}'..='\u{10ffff}').contains(&character)
}

fn bump_event(count: &mut usize) -> Result<(), &'static str> {
    *count = count.saturating_add(1);
    if *count > MAX_EVENTS {
        Err("nfo-event-limit")
    } else {
        Ok(())
    }
}

fn validate_attributes(
    element: &BytesStart<'_>,
    total_count: &mut usize,
) -> Result<(), &'static str> {
    let mut element_count = 0usize;
    for attribute in element.attributes().with_checks(true) {
        let attribute = attribute.map_err(|_| "nfo-invalid-attribute")?;
        element_count = element_count.saturating_add(1);
        *total_count = (*total_count).saturating_add(1);
        if element_count > MAX_ATTRIBUTES_PER_ELEMENT
            || *total_count > MAX_ATTRIBUTES
            || attribute.key.as_ref().len() > 256
            || attribute.value.len() > MAX_ATTRIBUTE_BYTES
        {
            return Err("nfo-attribute-limit");
        }
    }
    Ok(())
}

fn local_name(bytes: &[u8]) -> Result<String, &'static str> {
    let name = std::str::from_utf8(bytes).map_err(|_| "nfo-invalid-name")?;
    if name.len() > 64
        || !name
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || byte == b'_' || byte == b'-')
    {
        return Err("nfo-invalid-name");
    }
    Ok(name.to_ascii_lowercase())
}

fn is_supported_field(name: &str) -> bool {
    matches!(
        name,
        "title"
            | "originaltitle"
            | "plot"
            | "outline"
            | "premiered"
            | "releasedate"
            | "year"
            | "mpaa"
            | "genre"
            | "tag"
            | "tvmazeid"
    )
}

fn append_text(target: &mut String, value: &str) -> Result<(), &'static str> {
    if target.len().saturating_add(value.len()) > MAX_TEXT_BYTES {
        return Err("nfo-field-too-large");
    }
    target.push_str(value);
    Ok(())
}

fn apply_field(fields: &mut LocalNfo, name: &str, raw: &str) -> Result<(), &'static str> {
    let value = raw.trim();
    if value.is_empty() {
        return Ok(());
    }
    match name {
        "title" => fields.title = Some(bound(value, 512)?),
        "originaltitle" => fields.original_title = Some(bound(value, 512)?),
        "plot" | "outline" => fields.overview = Some(bound(value, 20_000)?),
        "premiered" | "releasedate" => {
            fields.premiere_date = NaiveDate::parse_from_str(value, "%Y-%m-%d").ok();
        }
        "year" => {
            fields.year = value
                .parse::<i32>()
                .ok()
                .filter(|year| (1800..=2300).contains(year))
        }
        "mpaa" => {
            fields.content_rating = Some(bound(value, 64)?);
            fields.policy_rating_value = parse_us_mpaa_v1(value);
        }
        "tvmazeid" => {
            let id = value.parse::<u64>().ok().filter(|id| *id > 0);
            fields.tvmaze_id = id;
            fields.tvmaze_id_invalid = id.is_none();
        }
        "genre" if fields.genres.len() < MAX_GENRES => {
            let genre = bound(value, 128)?;
            if !fields
                .genres
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(&genre))
            {
                fields.genres.push(genre);
            }
        }
        "genre" => return Err("nfo-too-many-genres"),
        "tag" if fields.tags.len() < MAX_GENRES => {
            let tag = bound(value, 128)?;
            if !fields
                .tags
                .iter()
                .any(|existing| existing.eq_ignore_ascii_case(&tag))
            {
                fields.tags.push(tag);
            }
        }
        "tag" => return Err("nfo-too-many-tags"),
        _ => {}
    }
    Ok(())
}

fn bound(value: &str, max_bytes: usize) -> Result<String, &'static str> {
    if value.len() > max_bytes {
        return Err("nfo-field-too-large");
    }
    Ok(value.to_owned())
}

/// Puffinbox's first local film-label policy ordinal. These values are policy
/// thresholds, not ages, Jellyfin scores, or coverage for TV/regional systems.
pub(super) fn parse_us_mpaa_v1(raw: &str) -> Option<i16> {
    let label = raw.trim();
    let label = label
        .strip_prefix("Rated ")
        .or_else(|| label.strip_prefix("rated "))
        .unwrap_or(label)
        .trim();
    if label.eq_ignore_ascii_case("G") {
        Some(0)
    } else if label.eq_ignore_ascii_case("PG") {
        Some(25)
    } else if label.eq_ignore_ascii_case("PG-13") {
        Some(50)
    } else if label.eq_ignore_ascii_case("R") {
        Some(75)
    } else if label.eq_ignore_ascii_case("NC-17") {
        Some(100)
    } else {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::{LocalNfo, parse, parse_us_mpaa_v1};

    #[test]
    fn parses_whitelisted_nfo_fields_and_keeps_unknown_rating_unrated() {
        let parsed = parse(br#"<movie><title>Sea &amp; Sky</title><plot><![CDATA[A quiet story.]]></plot><premiered>2021-05-04</premiered><year>2021</year><genre>Drama</genre><mpaa>Rated PG-13</mpaa><tvmazeid>42</tvmazeid><ratings><rating>R</rating></ratings></movie>"#).unwrap();
        assert_eq!(parsed.title.as_deref(), Some("Sea & Sky"));
        assert_eq!(parsed.overview.as_deref(), Some("A quiet story."));
        assert_eq!(parsed.policy_rating_value, Some(50));
        assert_eq!(parsed.content_rating.as_deref(), Some("Rated PG-13"));
        assert_eq!(parsed.tvmaze_id, Some(42));
        assert!(!parsed.tvmaze_id_invalid);
        assert_eq!(parsed.genres, ["Drama"]);

        let unknown = parse(br#"<movie><mpaa>TV-MA</mpaa></movie>"#).unwrap();
        assert_eq!(unknown.content_rating.as_deref(), Some("TV-MA"));
        assert_eq!(unknown.policy_rating_value, None);
    }

    #[test]
    fn only_exact_versioned_us_labels_map_to_policy_ordinals() {
        assert_eq!(parse_us_mpaa_v1("G"), Some(0));
        assert_eq!(parse_us_mpaa_v1("Rated PG"), Some(25));
        assert_eq!(parse_us_mpaa_v1("rated pg-13"), Some(50));
        assert_eq!(parse_us_mpaa_v1(" R "), Some(75));
        assert_eq!(parse_us_mpaa_v1("NC-17"), Some(100));
        for unknown in ["TV-MA", "PG 13", "Approved", "Unrated", "4"] {
            assert_eq!(
                parse_us_mpaa_v1(unknown),
                None,
                "unexpected mapping for {unknown}"
            );
        }
    }

    #[test]
    fn tags_are_bounded_decoded_and_deduplicated() {
        let parsed = parse(b"<movie><tag> Weekend </tag><tag>weekend</tag><tag>Sea &amp; Sky</tag><nested><tag>ignored</tag></nested></movie>").unwrap();
        assert_eq!(parsed.tags, ["Weekend", "Sea & Sky"]);
        let oversized = format!("<movie><tag>{}</tag></movie>", "x".repeat(129));
        assert_eq!(parse(oversized.as_bytes()), Err("nfo-field-too-large"));
        let tags = (0..129)
            .map(|index| format!("<tag>tag{index}</tag>"))
            .collect::<String>();
        assert_eq!(
            parse(format!("<movie>{tags}</movie>").as_bytes()),
            Err("nfo-too-many-tags")
        );
    }

    #[test]
    fn rejects_doctypes_depth_and_unbounded_fields() {
        assert_eq!(
            parse(b"<!DOCTYPE movie [<!ENTITY x 'boom'>]><movie/>"),
            Err("nfo-doctype-not-allowed")
        );
        let nested = format!("{}<title>x</title>{}", "<a>".repeat(33), "</a>".repeat(33));
        assert_eq!(parse(nested.as_bytes()), Err("nfo-depth-limit"));
        assert_eq!(
            parse(&vec![b'x'; super::MAX_NFO_BYTES + 1]),
            Err("nfo-too-large")
        );
        let many = format!("<m>{}</m>", "<g/>".repeat(super::MAX_EVENTS + 1));
        assert_eq!(parse(many.as_bytes()), Err("nfo-event-limit"));
        let attributes = format!(
            "<m {} />",
            (0..=super::MAX_ATTRIBUTES_PER_ELEMENT)
                .map(|n| format!("a{n}=\"x\" "))
                .collect::<String>()
        );
        assert_eq!(parse(attributes.as_bytes()), Err("nfo-attribute-limit"));
    }

    #[test]
    fn defaults_are_empty_and_nested_rating_values_are_ignored() {
        assert_eq!(
            parse(b"<movie><ratings><mpaa>R</mpaa></ratings></movie>").unwrap(),
            LocalNfo::default()
        );
    }

    #[test]
    fn malformed_explicit_tvmaze_choice_is_not_treated_as_absent() {
        let malformed =
            parse(b"<tvshow><title>Example</title><tvmazeid>0</tvmazeid></tvshow>").unwrap();
        assert_eq!(malformed.tvmaze_id, None);
        assert!(malformed.tvmaze_id_invalid);
        let overflow =
            parse(b"<tvshow><tvmazeid>18446744073709551616</tvmazeid></tvshow>").unwrap();
        assert!(overflow.tvmaze_id_invalid);
    }

    #[test]
    fn rejects_multiple_roots_and_ignores_nested_provider_choices() {
        assert_eq!(
            parse(b"<movie/><tvshow><mpaa>R</mpaa></tvshow>"),
            Err("nfo-multiple-roots")
        );
        assert_eq!(
            parse(b"<movie><external><tvmazeid>42</tvmazeid></external></movie>")
                .unwrap()
                .tvmaze_id,
            None
        );
    }
}
