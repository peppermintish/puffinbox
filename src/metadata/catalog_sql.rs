// These expressions use the same field precedence as item display metadata.
// References and fields are internal SQL fragments, never request values.
pub(crate) const PROVIDER_ORDER: &str = "CASE m.provider_key WHEN 'local-nfo' THEN 0 WHEN 'embedded-audio' THEN 1 WHEN 'tvmaze' THEN 3 ELSE 2 END,m.provider_key";

pub(crate) const EMBEDDED_SOURCE_CURRENT: &str = "m.provider_key='embedded-audio' AND EXISTS (SELECT 1 FROM items source WHERE source.id=m.item_id AND source.item_type='Audio' AND source.library_id=m.source_library_id AND source.path_hash=m.source_path_hash AND source.size_bytes=m.source_size_bytes AND source.date_modified=m.source_date_modified)";

pub(crate) fn current_audio_source(item: &str) -> String {
    format!(
        "EXISTS(SELECT 1 FROM item_metadata m WHERE m.item_id={item} AND {EMBEDDED_SOURCE_CURRENT})"
    )
}

pub(crate) fn valid_provider() -> String {
    format!(
        "(m.provider_key IN ('local-nfo','tvmaze') OR ({EMBEDDED_SOURCE_CURRENT}) OR \
         (m.provider_key LIKE 'plugin:%' AND EXISTS (SELECT 1 FROM trusted_plugins p \
         WHERE p.plugin_id=substring(m.provider_key FROM 8) AND p.enabled=TRUE AND p.status='enabled' AND \
         m.metadata_json->>'manifestSha256'=p.manifest_sha256 AND \
         m.metadata_json->>'moduleSha256'=p.binary_sha256)))"
    )
}

pub(crate) fn preferred(item: &str, field: &str, present: &str) -> String {
    let valid = valid_provider();
    format!(
        "(SELECT {field} FROM item_metadata m WHERE m.item_id={item} AND {present} AND \
         {valid} ORDER BY {PROVIDER_ORDER} LIMIT 1)"
    )
}

pub(crate) fn title(item: &str) -> String {
    let valid = valid_provider();
    // A filename stem is only a last resort. It must not suppress an actual
    // title supplied by any otherwise valid metadata provider.
    format!(
        "(SELECT m.title FROM item_metadata m WHERE m.item_id={item} AND m.title IS NOT NULL AND \
         {valid} ORDER BY CASE WHEN m.provider_key='embedded-audio' AND \
         m.metadata_json->'titleIsFileFallback'='true'::jsonb THEN 1 ELSE 0 END, \
         {PROVIDER_ORDER} LIMIT 1)"
    )
}

pub(crate) fn genres(item: &str) -> String {
    preferred(item, "m.genres", "jsonb_array_length(m.genres)>0")
}

pub(crate) const STUDIO_ID_SQL: &str = "md5('puffinbox/studio/v1:' || value)::uuid";

// Studio credits currently come from bounded local NFO metadata. Keep display,
// selection and named-item enumeration on the same exact trimmed names.
pub(crate) fn studios(item: &str) -> String {
    format!(
        "COALESCE((SELECT CASE WHEN jsonb_typeof(m.metadata_json->'studios')='array' \
         THEN m.metadata_json->'studios' ELSE '[]'::jsonb END FROM item_metadata m \
         WHERE m.item_id={item} AND m.provider_key='local-nfo'), '[]'::jsonb)"
    )
}

pub(crate) fn studio_names(item: &str) -> String {
    format!(
        "(SELECT DISTINCT btrim(studio #>> '{{}}') AS value FROM \
         jsonb_array_elements({}) studio WHERE jsonb_typeof(studio)='string' \
         AND octet_length(btrim(studio #>> '{{}}')) BETWEEN 1 AND 512 \
         AND (studio #>> '{{}}') !~ '[[:cntrl:]]')",
        studios(item)
    )
}

pub(crate) fn tags(item: &str) -> String {
    format!(
        "COALESCE((SELECT CASE WHEN jsonb_typeof(m.metadata_json->'tags')='array' \
         THEN m.metadata_json->'tags' ELSE '[]'::jsonb END FROM item_metadata m \
         WHERE m.item_id={item} AND m.provider_key='local-nfo'), '[]'::jsonb)"
    )
}

pub(crate) fn rating(item: &str) -> String {
    format!(
        "(SELECT m.content_rating FROM item_metadata m \
         WHERE m.item_id={item} AND m.provider_key='local-nfo')"
    )
}

pub(crate) fn year(item: &str) -> String {
    let date = premiere_date(item);
    format!(
        "COALESCE(EXTRACT(YEAR FROM {date})::int, \
         (SELECT CASE WHEN m.metadata_json->>'year' ~ '^[0-9]{{4}}$' \
         THEN (m.metadata_json->>'year')::int END FROM item_metadata m \
         WHERE m.item_id={item} AND m.provider_key='local-nfo'))"
    )
}

pub(crate) fn premiere_date(item: &str) -> String {
    preferred(item, "m.premiere_date", "m.premiere_date IS NOT NULL")
}

fn checked_index(value: &str, positive: bool) -> String {
    let minimum = if positive { 1 } else { 0 };
    // Keep casts inside CASE: malformed, oversized or out-of-range metadata
    // must sort as unknown rather than fail the entire catalogue request.
    format!(
        "(SELECT CASE WHEN value ~ '^[0-9]{{1,10}}$' THEN \
             CASE WHEN value::numeric BETWEEN {minimum} AND 2147483647 THEN value::int END END \
             FROM (SELECT {value} AS value) numeric_value)"
    )
}

pub(crate) fn music_number(item: &str, disc: bool) -> String {
    let key = if disc { "discNumber" } else { "trackNumber" };
    let field = format!("m.metadata_json->>'{key}'");
    let checked = checked_index(&field, false);
    preferred(item, &checked, &format!("{checked} IS NOT NULL"))
}

pub(crate) fn music_sort_name(alias: &str) -> String {
    let item = format!("{alias}.id");
    let title = title(&item);
    let disc = music_number(&item, true);
    let track = music_number(&item, false);
    // Missing numbers contribute no prefix. GREATEST preserves all digits
    // when a number is wider than the reference's four-character padding.
    format!(
        "(SELECT COALESCE(lpad(disc::text,GREATEST(4,length(disc::text)),'0') || ' - ','') || \
         COALESCE(lpad(track::text,GREATEST(4,length(track::text)),'0') || ' - ','') || \
         COALESCE({title},{alias}.name) FROM (SELECT {disc} AS disc,{track} AS track) music_numbers)"
    )
}

pub(crate) fn album(item: &str) -> String {
    preferred(
        item,
        "m.metadata_json->>'album'",
        "jsonb_typeof(m.metadata_json->'album')='string' AND octet_length(m.metadata_json->>'album') BETWEEN 1 AND 512",
    )
}

pub(crate) fn index_number(alias: &str, parent: bool) -> String {
    if parent {
        let season = checked_index("substring(season.name FROM '([0-9]+)[[:space:]]*$')", false);
        return format!(
            "CASE WHEN {alias}.item_type='Audio' THEN {} \
            WHEN {alias}.item_type='Episode' THEN (SELECT {season} FROM visible_catalog_nodes season \
            WHERE season.id={alias}.parent_id AND season.library_id={alias}.library_id AND season.item_type='Season') END",
            music_number(&format!("{alias}.id"), true)
        );
    }
    let leading = checked_index(
        &format!("substring({alias}.name FROM '^[[:space:]]*([0-9]+)')"),
        false,
    );
    let episode = checked_index(
        &format!("substring(upper({alias}.name) FROM 'S[0-9]+E([0-9]+)')"),
        false,
    );
    let trailing = checked_index(
        &format!("substring({alias}.name FROM '([0-9]+)[[:space:]]*$')"),
        false,
    );
    format!(
        "CASE WHEN {alias}.item_type='Audio' THEN COALESCE({}, CASE WHEN NOT {} THEN {leading} END) \
             WHEN {alias}.item_type='Episode' THEN {episode} \
             WHEN {alias}.item_type IN ('Season','MusicAlbum') THEN {trailing} END",
        music_number(&format!("{alias}.id"), false),
        current_audio_source(&format!("{alias}.id")),
    )
}
