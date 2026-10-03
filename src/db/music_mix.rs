use sqlx::{PgPool, Postgres, QueryBuilder, Row};
use uuid::Uuid;

use crate::{
    auth::UserRecord,
    library::{ItemQuery, ItemRecord},
    metadata::catalog_sql,
};

pub(crate) struct MusicMixSeed<'a> {
    pub tracks: &'a [Uuid],
    pub item: Option<Uuid>,
    pub genre: Option<&'a str>,
    pub playlist: Option<Uuid>,
}

fn push_visible_music(builder: &mut QueryBuilder<'_, Postgres>, user: &UserRecord) {
    builder.push(format!(
        "SELECT i.id,i.library_id,i.parent_id,i.name,i.sort_name,i.item_type,i.path,\
         i.container,i.size_bytes,i.runtime_ticks,i.date_added,i.date_modified,\
         {} AS rating,i.overview,i.metadata_json,{} AS genres \
         FROM items i JOIN libraries l ON l.id=i.library_id",
        super::policy_rating_sql("i"),
        catalog_sql::genres("i.id")
    ));
    super::push_item_conditions(builder, user, &ItemQuery::default(), None, true);
    builder.push(" AND i.item_type IN ('Audio','MusicAlbum','MusicArtist') ");
}

/// Keep seed tracks in queue order, then rank real related audio by shared
/// genres (4), visible albums (6) and visible artists (8). Ties are stable.
pub(crate) async fn instant_mix(
    pool: &PgPool,
    user: &UserRecord,
    seed: MusicMixSeed<'_>,
    limit: usize,
) -> Result<(Vec<ItemRecord>, i64), sqlx::Error> {
    let mut builder = QueryBuilder::<Postgres>::new("WITH visible_music AS (");
    push_visible_music(&mut builder, user);
    builder
        .push("), visible_catalog_nodes AS (SELECT * FROM visible_music)")
        .push(super::music_credits::CREDIT_CTES)
        .push(", requested_tracks AS (SELECT id,MIN(ordinal) AS ordinal FROM unnest(")
        .push_bind(seed.tracks.to_vec())
        .push("::uuid[]) WITH ORDINALITY AS request(id,ordinal) GROUP BY id), seed_tracks AS (\
               SELECT i.*,request.ordinal FROM visible_music i JOIN requested_tracks request ON request.id=i.id \
               WHERE i.item_type='Audio'");
    if let Some(source) = seed.item {
        builder
            .push(" AND EXISTS (SELECT 1 FROM visible_music source WHERE source.id=")
            .push_bind(source)
            .push(")");
    }
    if let Some(playlist) = seed.playlist {
        // Recheck the share and queue membership in this statement. A removed
        // share or entry must not survive as a cached recommendation seed.
        builder
            .push(" AND EXISTS (SELECT 1 FROM playlists p JOIN playlist_items entry ON entry.playlist_id=p.id \
                   WHERE entry.item_id=i.id AND p.id=")
            .push_bind(playlist)
            .push(" AND (p.owner_user_id=")
            .push_bind(user.id)
            .push(" OR EXISTS (SELECT 1 FROM playlist_users share WHERE share.playlist_id=p.id AND share.user_id=")
            .push_bind(user.id)
            .push(")))");
    }
    builder.push(
        "), source_nodes AS (SELECT id,library_id,parent_id,item_type,genres FROM seed_tracks \
         UNION SELECT id,library_id,parent_id,item_type,genres FROM visible_music WHERE id=",
    ).push_bind(seed.item).push(
        " AND EXISTS (SELECT 1 FROM seed_tracks)), seed_albums AS (\
         SELECT DISTINCT album.id,album.library_id,album.parent_id FROM source_nodes source \
         JOIN visible_music album ON album.id=CASE WHEN source.item_type='MusicAlbum' THEN source.id ELSE source.parent_id END \
         AND album.library_id=source.library_id AND album.item_type='MusicAlbum'), seed_artists AS (\
         SELECT credit.artist_id AS id FROM source_nodes source JOIN music_credits credit ON credit.item_id=source.id \
         UNION SELECT id FROM source_nodes WHERE item_type='MusicArtist'), seed_genres AS (\
         SELECT DISTINCT lower(btrim(g.value #>> '{}')) AS genre FROM source_nodes \
         CROSS JOIN LATERAL jsonb_array_elements(COALESCE(genres,'[]'::jsonb)) g(value) \
         WHERE jsonb_typeof(g.value)='string' AND octet_length(btrim(g.value #>> '{}')) BETWEEN 1 AND 128), scored AS (\
         SELECT i.*,seed.ordinal,affinity.genre_count*4 \
         + CASE WHEN album.id IN (SELECT id FROM seed_albums) THEN 6 ELSE 0 END \
         + CASE WHEN EXISTS (SELECT 1 FROM music_credits credit WHERE credit.item_id=i.id \
             AND credit.artist_id IN (SELECT id FROM seed_artists)) THEN 8 ELSE 0 END AS score \
         FROM visible_music i LEFT JOIN seed_tracks seed ON seed.id=i.id \
         LEFT JOIN visible_music album ON album.id=i.parent_id AND album.library_id=i.library_id AND album.item_type='MusicAlbum' \
         CROSS JOIN LATERAL (SELECT COUNT(DISTINCT genre) AS genre_count FROM seed_genres \
         JOIN jsonb_array_elements(COALESCE(i.genres,'[]'::jsonb)) g(value) ON genre=lower(btrim(g.value #>> '{}')) \
         WHERE jsonb_typeof(g.value)='string') affinity WHERE i.item_type='Audio'), candidates AS (SELECT * FROM scored WHERE ",
    );
    if let Some(genre) = seed.genre {
        builder
            .push(
                "EXISTS (SELECT 1 FROM jsonb_array_elements(COALESCE(genres,'[]'::jsonb)) g(value) \
                   WHERE jsonb_typeof(g.value)='string' AND lower(btrim(g.value #>> '{}'))=lower(",
            )
            .push_bind(genre.to_owned())
            .push("))");
    } else {
        builder.push("EXISTS (SELECT 1 FROM seed_tracks) AND (ordinal IS NOT NULL OR score>0)");
    }
    builder
        .push(
            ") SELECT *,COUNT(*) OVER() AS total FROM candidates \
               ORDER BY ordinal NULLS LAST,score DESC,sort_name,id LIMIT ",
        )
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

pub(crate) async fn visible_music_genre(
    pool: &PgPool,
    user: &UserRecord,
    name: Option<&str>,
    id: Option<Uuid>,
) -> Result<Option<String>, sqlx::Error> {
    let mut builder = QueryBuilder::<Postgres>::new("WITH visible_music AS (");
    push_visible_music(&mut builder, user);
    builder.push(
        "), genre_values AS (SELECT DISTINCT btrim(g.value #>> '{}') AS value FROM visible_music \
         CROSS JOIN LATERAL jsonb_array_elements(COALESCE(genres,'[]'::jsonb)) g(value) \
         WHERE jsonb_typeof(g.value)='string' AND octet_length(btrim(g.value #>> '{}')) BETWEEN 1 AND 128) \
         SELECT value FROM genre_values WHERE ",
    );
    if let Some(name) = name {
        builder
            .push("lower(value)=lower(")
            .push_bind(name.to_owned())
            .push(")");
    } else {
        builder
            .push(super::catalog_filters::GENRE_ID_SQL)
            .push("=")
            .push_bind(id);
    }
    builder.push(" ORDER BY value COLLATE \"C\" LIMIT 1");
    builder.build_query_scalar().fetch_optional(pool).await
}
