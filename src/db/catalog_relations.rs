use sqlx::{PgPool, Postgres, QueryBuilder, Row};
use uuid::Uuid;

use crate::{
    auth::UserRecord,
    library::{ItemQuery, ItemRecord},
    metadata::catalog_sql,
};

/// Rank visible items of the source type by shared display genres, catalogued
/// album and artist relationships. This is Puffinbox's own recommendation rule.
pub(crate) async fn similar_items(
    pool: &PgPool,
    user: &UserRecord,
    source_id: Uuid,
    limit: usize,
    excluded_artists: &[Uuid],
) -> Result<(Vec<ItemRecord>, i64), sqlx::Error> {
    let genres = catalog_sql::genres("i.id");
    let source_genres = catalog_sql::genres("s.id");
    let rating = super::policy_rating_sql("i");
    let mut builder = QueryBuilder::<Postgres>::new(format!(
        "WITH candidates AS (SELECT i.id,i.library_id,i.parent_id,i.name,i.sort_name,\
         i.item_type,i.path,i.container,i.size_bytes,i.runtime_ticks,i.date_added,\
         i.date_modified,{rating} AS rating,i.overview,i.metadata_json, \
         affinity.genre_count*4 + CASE WHEN album.id=source_album.id THEN 6 ELSE 0 END \
         + CASE WHEN artist.id=source_artist.id THEN 8 ELSE 0 END AS score \
         FROM items i JOIN libraries l ON l.id=i.library_id JOIN items s ON s.id="
    ));
    builder.push_bind(source_id).push(
        " LEFT JOIN items album ON album.id=CASE WHEN i.item_type='Audio' THEN i.parent_id \
         WHEN i.item_type='MusicAlbum' THEN i.id END AND album.item_type='MusicAlbum' AND album.library_id=i.library_id \
         LEFT JOIN items artist ON artist.id=CASE WHEN i.item_type='MusicArtist' THEN i.id ELSE album.parent_id END \
         AND artist.item_type='MusicArtist' AND artist.library_id=i.library_id \
         LEFT JOIN items source_album ON source_album.id=CASE WHEN s.item_type='Audio' THEN s.parent_id \
         WHEN s.item_type='MusicAlbum' THEN s.id END AND source_album.item_type='MusicAlbum' AND source_album.library_id=s.library_id \
         LEFT JOIN items source_artist ON source_artist.id=CASE WHEN s.item_type='MusicArtist' THEN s.id ELSE source_album.parent_id END \
         AND source_artist.item_type='MusicArtist' AND source_artist.library_id=s.library_id "
    ).push(format!(
        "CROSS JOIN LATERAL (SELECT COUNT(DISTINCT lower(btrim(g.value #>> '{{}}'))) AS genre_count \
         FROM jsonb_array_elements(COALESCE({genres},'[]'::jsonb)) g(value) \
         JOIN jsonb_array_elements(COALESCE({source_genres},'[]'::jsonb)) sg(value) \
         ON lower(btrim(g.value #>> '{{}}'))=lower(btrim(sg.value #>> '{{}}')) \
         WHERE jsonb_typeof(g.value)='string' AND jsonb_typeof(sg.value)='string' \
         AND btrim(g.value #>> '{{}}') <> '') affinity "
    ));
    super::push_item_conditions(&mut builder, user, &ItemQuery::default(), None, true);
    builder.push(" AND i.id<>s.id AND i.item_type=s.item_type AND (artist.id IS NULL OR NOT artist.id=ANY(")
        .push_bind(excluded_artists.to_vec()).push("::uuid[]))) SELECT *,COUNT(*) OVER() AS total FROM candidates WHERE score>0 ORDER BY score DESC,sort_name,id LIMIT ")
        .push_bind(limit.max(1) as i64);
    let rows = builder.build().fetch_all(pool).await?;
    let total = rows
        .first()
        .map(|row| row.try_get("total"))
        .transpose()?
        .unwrap_or(0);
    let items = rows
        .iter()
        .take(limit)
        .map(super::item_from_row)
        .collect::<Result<_, _>>()?;
    Ok((items, total))
}
