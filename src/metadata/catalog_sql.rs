// These expressions use the same field precedence as item display metadata.
// References and fields are internal SQL fragments, never request values.
pub(crate) fn preferred(item: &str, field: &str, present: &str) -> String {
    format!(
        "(SELECT {field} FROM item_metadata m WHERE m.item_id={item} AND {present} AND \
         (m.provider_key IN ('local-nfo','tvmaze') OR (m.provider_key LIKE 'plugin:%' AND \
         EXISTS (SELECT 1 FROM trusted_plugins p WHERE p.plugin_id=substring(m.provider_key FROM 8) \
         AND p.enabled=TRUE AND p.status='enabled' AND \
         m.metadata_json->>'manifestSha256'=p.manifest_sha256 AND \
         m.metadata_json->>'moduleSha256'=p.binary_sha256))) \
         ORDER BY CASE m.provider_key WHEN 'local-nfo' THEN 0 WHEN 'tvmaze' THEN 2 ELSE 1 END, \
         m.provider_key LIMIT 1)"
    )
}

pub(crate) fn genres(item: &str) -> String {
    preferred(item, "m.genres", "jsonb_array_length(m.genres)>0")
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
    checked_index(
        &format!(
            "(SELECT m.metadata_json->>'{key}' FROM item_metadata m \
        WHERE m.item_id={item} AND m.provider_key='local-nfo')"
        ),
        true,
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
        "CASE WHEN {alias}.item_type='Audio' THEN COALESCE({}, {leading}) \
             WHEN {alias}.item_type='Episode' THEN {episode} \
             WHEN {alias}.item_type IN ('Season','MusicAlbum') THEN {trailing} END",
        music_number(&format!("{alias}.id"), false)
    )
}
