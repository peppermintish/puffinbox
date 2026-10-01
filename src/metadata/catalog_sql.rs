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
    let date = preferred(item, "m.premiere_date", "m.premiere_date IS NOT NULL");
    format!(
        "COALESCE(EXTRACT(YEAR FROM {date})::int, \
         (SELECT CASE WHEN m.metadata_json->>'year' ~ '^[0-9]{{4}}$' \
         THEN (m.metadata_json->>'year')::int END FROM item_metadata m \
         WHERE m.item_id={item} AND m.provider_key='local-nfo'))"
    )
}
