use chrono::{DateTime, Utc};
use sqlx::{PgPool, Postgres, QueryBuilder, Row};
use uuid::Uuid;

use crate::{
    auth::UserRecord,
    library::{ItemQuery, ItemRecord},
    metadata::catalog_sql,
};

pub(crate) struct NextUpQuery {
    pub series_id: Option<Uuid>,
    pub library_id: Option<Uuid>,
    pub date_cutoff: Option<DateTime<Utc>>,
    pub resumable: bool,
    pub rewatching: bool,
    pub start: i64,
    pub limit: i64,
}

pub(crate) async fn next_up_items(
    pool: &PgPool,
    user: &UserRecord,
    query: &NextUpQuery,
) -> Result<(Vec<ItemRecord>, i64), sqlx::Error> {
    let rating = super::policy_rating_sql("i");
    let season_number = catalog_sql::index_number("i", true);
    let episode_number = catalog_sql::index_number("i", false);
    // Candidate series come from this user's history (or an explicitly named
    // series). Episode enumeration can then use the existing parent indexes.
    // The inlined visibility relation also gates ancestors, not only leaves.
    let mut builder = QueryBuilder::<Postgres>::new(format!(
        "WITH visible_catalog_nodes AS NOT MATERIALIZED (SELECT i.id,i.library_id,i.parent_id,\
         i.name,i.sort_name,i.item_type,i.path,i.container,i.size_bytes,i.runtime_ticks,\
         i.date_added,i.date_modified,{rating} AS rating,i.overview,i.metadata_json \
         FROM items i JOIN libraries l ON l.id=i.library_id"
    ));
    super::push_item_conditions(&mut builder, user, &ItemQuery::default(), None, true);
    builder.push(" AND i.item_type IN ('Series','Season','Episode')");
    if let Some(library) = query.library_id {
        builder.push(" AND i.library_id=").push_bind(library);
    }
    builder.push(
        "), history_series AS (SELECT DISTINCT COALESCE(season.parent_id,i.parent_id) AS id \
         FROM user_item_data history JOIN visible_catalog_nodes i ON i.id=history.item_id \
         LEFT JOIN visible_catalog_nodes season ON season.id=i.parent_id AND season.item_type='Season' \
         AND season.library_id=i.library_id WHERE i.item_type='Episode' AND history.user_id="
    ).push_bind(user.id).push(
        " AND (history.played OR history.last_played_at IS NOT NULL OR history.playback_position_ticks>0)"
    );
    if let Some(series) = query.series_id {
        builder.push(" UNION SELECT ").push_bind(series);
    }
    builder.push(format!(
        "), numbered AS (SELECT i.*,series.id AS series_id,series.sort_name AS series_sort_name,\
         {season_number} AS season_number,{episode_number} AS episode_number \
         FROM visible_catalog_nodes i JOIN visible_catalog_nodes season \
         ON season.id=i.parent_id AND season.item_type='Season' AND season.library_id=i.library_id \
         JOIN visible_catalog_nodes series ON series.id=season.parent_id AND series.item_type='Series' \
         AND series.library_id=i.library_id JOIN history_series history ON history.id=series.id \
         WHERE i.item_type='Episode'"
    ));
    if let Some(series) = query.series_id {
        builder.push(" AND series.id=").push_bind(series);
    }
    builder.push(
        "), episodes AS (SELECT numbered.*,ROW_NUMBER() OVER(PARTITION BY series_id \
         ORDER BY season_number,episode_number,sort_name,id) AS ordinal \
         FROM numbered WHERE season_number>0 AND episode_number>0), \
         progress AS (SELECT e.*,COALESCE(data.played,FALSE) AS played,\
         COALESCE(data.playback_position_ticks,0) AS position,data.last_played_at \
         FROM episodes e LEFT JOIN user_item_data data ON data.item_id=e.id AND data.user_id="
    ).push_bind(user.id).push(
        "), series_state AS (SELECT series_id,MAX(ordinal) FILTER(WHERE played) AS last_watched,\
         MAX(last_played_at) FILTER(WHERE played) AS watched_at,MAX(last_played_at) AS activity_at,\
         BOOL_OR(played OR position>0 OR last_played_at IS NOT NULL) AS started \
         FROM progress GROUP BY series_id), \
         latest_watched AS (SELECT DISTINCT ON(series_id) series_id,ordinal \
         FROM progress WHERE played ORDER BY series_id,last_played_at DESC NULLS LAST,ordinal DESC), \
         regular AS (SELECT DISTINCT ON(e.series_id) e.*,s.watched_at,s.activity_at \
         FROM progress e JOIN series_state s ON s.series_id=e.series_id \
         WHERE NOT e.played AND e.ordinal>COALESCE(s.last_watched,0) AND (s.started OR e.series_id="
    ).push_bind(query.series_id).push(
        ") ORDER BY e.series_id,e.ordinal), \
         rewatch AS (SELECT DISTINCT ON(e.series_id) e.*,s.watched_at,s.activity_at \
         FROM progress e JOIN series_state s ON s.series_id=e.series_id \
         JOIN latest_watched latest ON latest.series_id=e.series_id \
         WHERE e.played AND e.ordinal>latest.ordinal AND "
    ).push_bind(query.rewatching).push(
        " ORDER BY e.series_id,e.ordinal), candidates AS (SELECT * FROM regular UNION ALL SELECT * FROM rewatch), \
         eligible AS (SELECT * FROM candidates WHERE (position=0 OR "
    ).push_bind(query.resumable).push(") AND (").push_bind(query.date_cutoff)
        .push("::timestamptz IS NULL OR activity_at>=").push_bind(query.date_cutoff)
        .push(
            ")), totals AS (SELECT COUNT(*)::bigint AS total FROM eligible) \
             SELECT page.*,totals.total FROM totals LEFT JOIN LATERAL \
             (SELECT * FROM eligible ORDER BY watched_at DESC NULLS LAST,series_sort_name,series_id,ordinal LIMIT "
        ).push_bind(query.limit).push(" OFFSET ").push_bind(query.start)
        .push(") page ON TRUE ORDER BY page.watched_at DESC NULLS LAST,page.series_sort_name,page.series_id,page.ordinal");
    let rows = builder.build().fetch_all(pool).await?;
    let total = rows
        .first()
        .map(|row| row.try_get("total"))
        .transpose()?
        .unwrap_or(0);
    let mut items = Vec::new();
    for row in &rows {
        if row.try_get::<Option<Uuid>, _>("id")?.is_some() {
            items.push(super::item_from_row(row)?);
        }
    }
    Ok((items, total))
}
