use serde::Deserialize;
use sqlx::{PgPool, Postgres, QueryBuilder, Row};
use uuid::Uuid;

use crate::{ApiError, auth::UserRecord, library::ItemQuery, metadata::catalog_sql};

#[derive(Default)]
pub(crate) struct StudioSelection {
    pub id: Option<Uuid>,
    pub name: Option<String>,
    pub search: Option<String>,
    pub starts_with: Option<String>,
    pub greater: Option<String>,
    pub less: Option<String>,
    pub exclude_types: Vec<String>,
    pub favorite: Option<bool>,
}

#[derive(Deserialize)]
pub(crate) struct StudioRecord {
    pub id: Uuid,
    pub name: String,
    pub child_count: i64,
    pub movie_count: i64,
    pub series_count: i64,
    pub episode_count: i64,
    pub song_count: i64,
    pub album_count: i64,
    pub music_video_count: i64,
    pub is_favorite: bool,
}

pub(crate) async fn studio_page(
    pool: &PgPool,
    user: &UserRecord,
    scope: &ItemQuery,
    selection: &StudioSelection,
) -> Result<(Vec<StudioRecord>, i64), ApiError> {
    let parent = super::item_query_parent(pool, scope.parent_id).await?;
    if scope.parent_id.is_some() && parent.is_none() {
        return Ok((Vec::new(), 0));
    }
    let mut builder = QueryBuilder::<Postgres>::new("");
    super::push_item_source(&mut builder, user, scope, parent);
    builder.push(if super::item_cte(parent, scope.recursive) {
        ", selected AS ("
    } else {
        "WITH selected AS ("
    });
    builder.push(format!(
        "SELECT i.id,i.item_type,{} AS studios FROM items i JOIN libraries l ON l.id=i.library_id",
        catalog_sql::studios("i.id")
    ));
    super::push_item_conditions(&mut builder, user, scope, parent, scope.recursive);
    if !selection.exclude_types.is_empty() {
        builder
            .push(" AND i.item_type <> ALL(")
            .push_bind(selection.exclude_types.clone())
            .push("::text[])");
    }
    builder.push("), credits AS (SELECT DISTINCT id,item_type,btrim(studio #>> '{}') AS value FROM selected CROSS JOIN LATERAL jsonb_array_elements(studios) studio WHERE jsonb_typeof(studio)='string' AND octet_length(btrim(studio #>> '{}')) BETWEEN 1 AND 512 AND (studio #>> '{}') !~ '[[:cntrl:]]'), studios AS (SELECT ")
        .push(catalog_sql::STUDIO_ID_SQL)
        .push(" AS id,value AS name,COUNT(*) AS child_count,COUNT(*) FILTER (WHERE item_type='Movie') AS movie_count,COUNT(*) FILTER (WHERE item_type='Series') AS series_count,COUNT(*) FILTER (WHERE item_type='Episode') AS episode_count,COUNT(*) FILTER (WHERE item_type='Audio') AS song_count,COUNT(*) FILTER (WHERE item_type='MusicAlbum') AS album_count,COUNT(*) FILTER (WHERE item_type='MusicVideo') AS music_video_count FROM credits GROUP BY value), filtered AS (SELECT studios.*,EXISTS (SELECT 1 FROM user_studio_favorites favorite WHERE favorite.user_id=")
        .push_bind(user.id).push(" AND favorite.studio_id=studios.id) AS is_favorite FROM studios WHERE TRUE");
    if let Some(id) = selection.id {
        builder.push(" AND id=").push_bind(id);
    }
    if let Some(name) = &selection.name {
        builder.push(" AND name=").push_bind(name.clone());
    }
    if let Some(search) = &selection.search {
        builder
            .push(" AND strpos(lower(name),lower(")
            .push_bind(search.clone())
            .push("))>0");
    }
    if let Some(prefix) = &selection.starts_with {
        builder
            .push(" AND starts_with(lower(name),lower(")
            .push_bind(prefix.clone())
            .push("))");
    }
    if let Some(greater) = &selection.greater {
        builder
            .push(" AND lower(name) COLLATE \"C\">=lower(")
            .push_bind(greater.clone())
            .push(") COLLATE \"C\"");
    }
    if let Some(less) = &selection.less {
        builder
            .push(" AND lower(name) COLLATE \"C\"<lower(")
            .push_bind(less.clone())
            .push(") COLLATE \"C\"");
    }
    builder.push("), favorites AS (SELECT * FROM filtered WHERE TRUE");
    if let Some(favorite) = selection.favorite {
        builder.push(" AND is_favorite=").push_bind(favorite);
    }
    builder.push("), page AS (SELECT * FROM favorites ORDER BY lower(name) COLLATE \"C\",name COLLATE \"C\",id LIMIT ")
        .push_bind(scope.limit).push(" OFFSET ").push_bind(scope.start_index)
        .push(") SELECT (SELECT COUNT(*) FROM favorites) AS total,COALESCE((SELECT jsonb_agg(to_jsonb(page) ORDER BY lower(name) COLLATE \"C\",name COLLATE \"C\",id) FROM page),'[]'::jsonb) AS items");
    let row = builder.build().fetch_one(pool).await?;
    let records =
        serde_json::from_value(row.try_get("items")?).map_err(|_| ApiError::Unavailable)?;
    Ok((records, row.try_get("total")?))
}

pub(crate) async fn set_studio_favorite(
    pool: &PgPool,
    run: Uuid,
    user: Uuid,
    studio: Uuid,
    favorite: bool,
) -> Result<(), sqlx::Error> {
    let mut tx = pool.begin().await?;
    super::require_active_run(&mut tx, run).await?;
    if favorite {
        sqlx::query("INSERT INTO user_studio_favorites(user_id,studio_id) VALUES ($1,$2) ON CONFLICT DO NOTHING")
            .bind(user).bind(studio).execute(&mut *tx).await?;
    } else {
        sqlx::query("DELETE FROM user_studio_favorites WHERE user_id=$1 AND studio_id=$2")
            .bind(user)
            .bind(studio)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await
}
