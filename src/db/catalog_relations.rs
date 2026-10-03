use sqlx::{PgPool, Postgres, QueryBuilder, Row};
use uuid::Uuid;

use crate::{
    auth::UserRecord,
    library::{ItemQuery, ItemRecord},
    metadata::catalog_sql,
};

/// Rank visible items of the source type by shared display genres, visible
/// albums and resolved music credits. This is Puffinbox's own recommendation rule.
pub(crate) async fn similar_items(
    pool: &PgPool,
    user: &UserRecord,
    source_id: Uuid,
    limit: usize,
    excluded_artists: &[Uuid],
) -> Result<(Vec<ItemRecord>, i64), sqlx::Error> {
    let genres = catalog_sql::genres("i.id");
    let rating = super::policy_rating_sql("i");
    let mut builder = QueryBuilder::<Postgres>::new(format!(
        "WITH visible_catalog_nodes AS (SELECT i.id,i.library_id,i.parent_id,i.name,i.sort_name,\
         i.item_type,i.path,i.container,i.size_bytes,i.runtime_ticks,i.date_added,\
         i.date_modified,{rating} AS rating,i.overview,i.metadata_json,{genres} AS genres \
         FROM items i JOIN libraries l ON l.id=i.library_id"
    ));
    super::push_item_conditions(&mut builder, user, &ItemQuery::default(), None, true);
    builder.push(")").push(super::music_credits::CREDIT_CTES).push(
        ", candidates AS (SELECT i.*,affinity.genre_count*4 \
         + CASE WHEN album.id=source_album.id THEN 6 ELSE 0 END \
         + CASE WHEN EXISTS (SELECT 1 FROM music_credits credit JOIN music_credits source_credit \
             ON source_credit.artist_id=credit.artist_id WHERE credit.item_id=i.id AND source_credit.item_id=s.id) \
             THEN 8 ELSE 0 END AS score \
         FROM visible_catalog_nodes i JOIN visible_catalog_nodes s ON s.id="
    );
    builder.push_bind(source_id).push(
        " LEFT JOIN visible_catalog_nodes album ON album.id=CASE WHEN i.item_type='Audio' THEN i.parent_id \
         WHEN i.item_type='MusicAlbum' THEN i.id END AND album.item_type='MusicAlbum' AND album.library_id=i.library_id \
         LEFT JOIN visible_catalog_nodes source_album ON source_album.id=CASE WHEN s.item_type='Audio' THEN s.parent_id \
         WHEN s.item_type='MusicAlbum' THEN s.id END AND source_album.item_type='MusicAlbum' AND source_album.library_id=s.library_id \
         CROSS JOIN LATERAL (SELECT COUNT(DISTINCT lower(btrim(g.value #>> '{}'))) AS genre_count \
         FROM jsonb_array_elements(COALESCE(i.genres,'[]'::jsonb)) g(value) \
         JOIN jsonb_array_elements(COALESCE(s.genres,'[]'::jsonb)) sg(value) \
         ON lower(btrim(g.value #>> '{}'))=lower(btrim(sg.value #>> '{}')) \
         WHERE jsonb_typeof(g.value)='string' AND jsonb_typeof(sg.value)='string' \
         AND btrim(g.value #>> '{}') <> '') affinity \
         WHERE i.id<>s.id AND i.item_type=s.item_type AND NOT EXISTS (SELECT 1 FROM music_credits excluded \
             WHERE excluded.item_id=i.id AND excluded.artist_id=ANY("
    );
    builder.push_bind(excluded_artists.to_vec()).push("::uuid[]))) SELECT *,COUNT(*) OVER() AS total FROM candidates WHERE score>0 ORDER BY score DESC,sort_name,id LIMIT ")
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
