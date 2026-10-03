use std::collections::HashMap;

use sqlx::{PgPool, Postgres, QueryBuilder, Row};
use uuid::Uuid;

use crate::{auth::UserRecord, library::ItemQuery};

use super::ItemNavigationLinks;

// Only explicit, bounded local-NFO names supply credits. Resolve them to
// visible artists in the same library. Folder relationships remain the
// fallback when no names were supplied; hidden explicit names never fall
// back to another artist. No synthetic artist or album is invented here.
pub(super) const CREDIT_CTES: &str = r#",
music_names AS (
    SELECT i.id AS item_id, role.key AS role, btrim(n.value #>> '{}') AS name
    FROM visible_catalog_nodes i JOIN item_metadata m ON m.item_id=i.id AND m.provider_key='local-nfo'
    CROSS JOIN (VALUES ('artists'),('albumArtists')) role(key)
    CROSS JOIN LATERAL jsonb_array_elements(CASE WHEN jsonb_typeof(m.metadata_json->role.key)='array'
        THEN m.metadata_json->role.key ELSE '[]'::jsonb END) WITH ORDINALITY n(value, ordinal)
    WHERE i.item_type IN ('Audio','MusicAlbum') AND n.ordinal<=32 AND jsonb_typeof(n.value)='string'
        AND octet_length(btrim(n.value #>> '{}')) BETWEEN 1 AND 512
), music_named_artists AS (
    SELECT DISTINCT n.item_id,n.role,artist.id AS artist_id
    FROM music_names n JOIN visible_catalog_nodes source ON source.id=n.item_id
    JOIN visible_catalog_nodes artist ON artist.item_type='MusicArtist' AND artist.library_id=source.library_id
        AND lower(artist.name)=lower(n.name)
), music_album_artists AS (
    SELECT album.id AS item_id,n.artist_id FROM visible_catalog_nodes album
    JOIN music_named_artists n ON n.item_id=album.id AND n.role='albumArtists'
    WHERE album.item_type='MusicAlbum'
    UNION SELECT album.id,artist.id FROM visible_catalog_nodes album
    JOIN visible_catalog_nodes artist ON artist.id=album.parent_id AND artist.item_type='MusicArtist'
        AND artist.library_id=album.library_id
    WHERE album.item_type='MusicAlbum' AND NOT EXISTS (
        SELECT 1 FROM music_names n WHERE n.item_id=album.id AND n.role='albumArtists')
), music_track_album_artists AS (
    SELECT track.id AS item_id,n.artist_id FROM visible_catalog_nodes track
    JOIN music_named_artists n ON n.item_id=track.id AND n.role='albumArtists'
    WHERE track.item_type='Audio'
    UNION SELECT track.id,credit.artist_id FROM visible_catalog_nodes track
    JOIN visible_catalog_nodes album ON album.id=track.parent_id AND album.item_type='MusicAlbum'
        AND album.library_id=track.library_id JOIN music_album_artists credit ON credit.item_id=album.id
    WHERE track.item_type='Audio' AND NOT EXISTS (
        SELECT 1 FROM music_names n WHERE n.item_id=track.id AND n.role='albumArtists')
    UNION SELECT track.id,artist.id FROM visible_catalog_nodes track
    JOIN visible_catalog_nodes artist ON artist.id=track.parent_id AND artist.item_type='MusicArtist'
        AND artist.library_id=track.library_id
    WHERE track.item_type='Audio' AND NOT EXISTS (
        SELECT 1 FROM music_names n WHERE n.item_id=track.id AND n.role='albumArtists')
), music_track_artists AS (
    SELECT track.id AS item_id,n.artist_id FROM visible_catalog_nodes track
    JOIN music_named_artists n ON n.item_id=track.id AND n.role='artists' WHERE track.item_type='Audio'
    UNION SELECT credit.item_id,credit.artist_id FROM music_track_album_artists credit WHERE NOT EXISTS (
        SELECT 1 FROM music_names n WHERE n.item_id=credit.item_id AND n.role='artists')
), music_track_credits AS (
    SELECT item_id,artist_id,FALSE AS contributing FROM music_track_album_artists
    UNION ALL SELECT credit.item_id,credit.artist_id,NOT EXISTS (
        SELECT 1 FROM music_track_album_artists album WHERE album.item_id=credit.item_id AND album.artist_id=credit.artist_id)
    FROM music_track_artists credit
), music_credits AS (
    SELECT item_id,artist_id,bool_or(contributing) AS contributing FROM (
        SELECT * FROM music_track_credits
        UNION ALL SELECT item_id,artist_id,FALSE FROM music_album_artists
        UNION ALL SELECT album.id,credit.artist_id,credit.contributing FROM music_track_credits credit
        JOIN visible_catalog_nodes track ON track.id=credit.item_id
        JOIN visible_catalog_nodes album ON album.id=track.parent_id AND album.item_type='MusicAlbum'
            AND album.library_id=track.library_id
    ) credits GROUP BY item_id,artist_id
), music_performers AS (
    SELECT item_id,artist_id FROM music_track_artists
    UNION SELECT album.id,credit.artist_id FROM music_track_artists credit
        JOIN visible_catalog_nodes track ON track.id=credit.item_id
        JOIN visible_catalog_nodes album ON album.id=track.parent_id AND album.item_type='MusicAlbum'
            AND album.library_id=track.library_id
    UNION SELECT credit.item_id,credit.artist_id FROM music_album_artists credit WHERE NOT EXISTS (
        SELECT 1 FROM visible_catalog_nodes track JOIN music_track_artists performer ON performer.item_id=track.id
        WHERE track.parent_id=credit.item_id)
), music_album_roles AS (
    SELECT item_id,artist_id FROM music_track_album_artists UNION SELECT item_id,artist_id FROM music_album_artists
) "#;

#[derive(Clone, Debug)]
pub struct MusicArtistCredit {
    pub id: Uuid,
    pub name: String,
}

#[derive(Clone, Debug, Default)]
pub struct MusicNavigation {
    pub artists: Vec<MusicArtistCredit>,
    pub album_artists: Vec<MusicArtistCredit>,
    pub song_count: Option<i64>,
    pub album_count: Option<i64>,
    pub runtime_ticks: Option<i64>,
}

pub(super) async fn enrich_navigation(
    pool: &PgPool,
    user: &UserRecord,
    item_ids: &[Uuid],
    links: &mut HashMap<Uuid, ItemNavigationLinks>,
) -> Result<(), sqlx::Error> {
    let music_ids: Vec<Uuid> = sqlx::query_scalar(
        "SELECT id FROM items WHERE id=ANY($1) AND item_type IN ('Audio','MusicAlbum','MusicArtist')",
    )
    .bind(item_ids)
    .fetch_all(pool)
    .await?;
    if music_ids.is_empty() {
        return Ok(());
    }
    let mut builder = QueryBuilder::<Postgres>::new(
        "WITH visible_catalog_nodes AS (SELECT i.id,i.name,i.library_id,i.parent_id,i.item_type,i.runtime_ticks FROM items i JOIN libraries l ON l.id=i.library_id",
    );
    super::push_item_conditions(&mut builder, user, &ItemQuery::default(), None, true);
    builder
        .push(" AND i.item_type IN ('Audio','MusicAlbum','MusicArtist')) ")
        .push(CREDIT_CTES)
        .push(" SELECT target.id,target.item_type,role.kind,artist.id AS artist_id,artist.name,counts.song_count,counts.album_count,counts.runtime_ticks FROM visible_catalog_nodes target LEFT JOIN LATERAL (
            SELECT 'artist' AS kind,artist_id FROM music_performers WHERE item_id=target.id
            UNION ALL SELECT 'album',artist_id FROM music_album_roles WHERE item_id=target.id
        ) role ON TRUE LEFT JOIN visible_catalog_nodes artist ON artist.id=role.artist_id
        LEFT JOIN LATERAL (
            SELECT COUNT(*) FILTER (WHERE item.item_type='Audio')::BIGINT AS song_count,
                COUNT(*) FILTER (WHERE item.item_type='MusicAlbum')::BIGINT AS album_count,
                COALESCE(SUM(GREATEST(item.runtime_ticks,0)) FILTER (WHERE item.item_type='Audio'),0)::BIGINT AS runtime_ticks
            FROM music_credits credit JOIN visible_catalog_nodes item ON item.id=credit.item_id
            WHERE credit.artist_id=target.id AND target.item_type='MusicArtist'
        ) counts ON target.item_type='MusicArtist' WHERE target.id=ANY(")
        .push_bind(music_ids)
        .push("::uuid[]) ORDER BY target.id,artist.name,artist.id");
    for row in builder.build().fetch_all(pool).await? {
        let id: Uuid = row.try_get("id")?;
        let Some(links) = links.get_mut(&id) else {
            continue;
        };
        let music = links.music.get_or_insert_with(MusicNavigation::default);
        if row.try_get::<&str, _>("item_type")? == "MusicArtist" {
            music.song_count = row.try_get("song_count")?;
            music.album_count = row.try_get("album_count")?;
            music.runtime_ticks = row.try_get("runtime_ticks")?;
        }
        if let Some(id) = row.try_get("artist_id")? {
            let credit = MusicArtistCredit {
                id,
                name: row.try_get("name")?,
            };
            match row.try_get::<&str, _>("kind")? {
                "artist" => music.artists.push(credit),
                "album" => music.album_artists.push(credit),
                _ => unreachable!("SQL role has two fixed values"),
            }
        }
    }
    Ok(())
}
