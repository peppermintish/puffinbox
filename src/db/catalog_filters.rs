use serde::Serialize;
use sqlx::{PgPool, Postgres, QueryBuilder, Row};
use uuid::Uuid;

use crate::{
    ApiError,
    auth::UserRecord,
    library::{ItemFacetFilters, ItemQuery},
    metadata::catalog_sql,
};

const MAX_FACET_VALUES: usize = 4096;
// IDs are opaque catalog keys derived from an exact, trimmed UTF-8 genre name.
pub(super) const GENRE_ID_SQL: &str = "md5('puffinbox/genre/v1:' || value)::uuid";

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
pub(crate) struct GenreFacet {
    pub name: String,
    pub id: Uuid,
}

#[derive(Default)]
pub(crate) struct CatalogFacets {
    pub genres: Vec<GenreFacet>,
    pub tags: Vec<String>,
    pub official_ratings: Vec<String>,
    pub years: Vec<i32>,
}

pub(crate) async fn catalog_facets(
    pool: &PgPool,
    user: &UserRecord,
    query: &ItemQuery,
) -> Result<CatalogFacets, ApiError> {
    let parent = super::item_query_parent(pool, query.parent_id).await?;
    if query.parent_id.is_some() && parent.is_none() {
        return Ok(CatalogFacets::default());
    }
    let mut builder = QueryBuilder::<Postgres>::new("");
    super::push_item_source(&mut builder, user, query, parent);
    builder.push(if super::item_cte(parent, query.recursive) {
        ", selected AS ("
    } else {
        "WITH selected AS ("
    });
    builder.push(format!(
        "SELECT {} AS genres, {} AS tags, {} AS rating, {} AS year \
         FROM items i JOIN libraries l ON l.id=i.library_id",
        catalog_sql::genres("i.id"),
        catalog_sql::tags("i.id"),
        catalog_sql::rating("i.id"),
        catalog_sql::year("i.id")
    ));
    super::push_item_conditions(&mut builder, user, query, parent, query.recursive);
    builder.push(
        "), facet_values AS ( \
         SELECT 'genre'::text AS kind, btrim(g.value #>> '{}') AS value FROM selected \
         CROSS JOIN LATERAL jsonb_array_elements(COALESCE(genres,'[]'::jsonb)) g(value) \
         WHERE jsonb_typeof(g.value)='string' \
         UNION ALL SELECT 'tag', btrim(t.value #>> '{}') FROM selected \
         CROSS JOIN LATERAL jsonb_array_elements(tags) t(value) WHERE jsonb_typeof(t.value)='string' \
         UNION ALL SELECT 'rating', btrim(rating) FROM selected WHERE rating IS NOT NULL \
         UNION ALL SELECT 'year', year::text FROM selected WHERE year BETWEEN 1800 AND 2300 \
         ), distinct_values AS (SELECT DISTINCT kind,value FROM facet_values \
         WHERE octet_length(value) BETWEEN 1 AND 128), bounded AS (SELECT kind,value, \
         row_number() OVER (PARTITION BY kind ORDER BY value COLLATE \"C\") AS ordinal FROM distinct_values) \
         SELECT kind,value,CASE WHEN kind='genre' THEN "
    ).push(GENRE_ID_SQL).push(" END AS id FROM bounded WHERE ordinal <= ")
        .push_bind((MAX_FACET_VALUES + 1) as i64)
        .push(" ORDER BY kind COLLATE \"C\", value COLLATE \"C\"");
    let mut facets = CatalogFacets::default();
    for row in builder.build().fetch_all(pool).await? {
        let value: String = row.try_get("value")?;
        match row.try_get::<&str, _>("kind")? {
            "genre" => facets.genres.push(GenreFacet {
                name: value,
                id: row.try_get("id")?,
            }),
            "tag" => facets.tags.push(value),
            "rating" => facets.official_ratings.push(value),
            "year" => facets
                .years
                .push(value.parse().map_err(|_| ApiError::Unavailable)?),
            _ => return Err(ApiError::Unavailable),
        }
    }
    if [
        facets.genres.len(),
        facets.tags.len(),
        facets.official_ratings.len(),
        facets.years.len(),
    ]
    .into_iter()
    .any(|count| count > MAX_FACET_VALUES)
    {
        return Err(ApiError::Unavailable);
    }
    facets.years.sort_unstable();
    Ok(facets)
}

pub(super) fn push_selections(
    builder: &mut QueryBuilder<'_, Postgres>,
    filters: &ItemFacetFilters,
) {
    let genres = catalog_sql::genres("i.id");
    if !filters.genres.is_empty() || !filters.genre_ids.is_empty() {
        builder.push(format!(
            " AND EXISTS (SELECT 1 FROM (SELECT btrim(g.value #>> '{{}}') AS value \
             FROM jsonb_array_elements(COALESCE({genres},'[]'::jsonb)) g(value) \
             WHERE jsonb_typeof(g.value)='string') genres WHERE "
        ));
        if !filters.genres.is_empty() {
            builder
                .push("lower(value) IN (SELECT lower(unnest(")
                .push_bind(filters.genres.clone())
                .push("::text[])))");
        }
        if !filters.genre_ids.is_empty() {
            if !filters.genres.is_empty() {
                builder.push(" OR ");
            }
            builder
                .push(GENRE_ID_SQL)
                .push(" = ANY(")
                .push_bind(filters.genre_ids.clone())
                .push(")");
        }
        builder.push(") ");
    }
    if !filters.tags.is_empty() {
        let tags = catalog_sql::tags("i.id");
        builder
            .push(format!(
                " AND EXISTS (SELECT 1 FROM jsonb_array_elements({tags}) t(value) \
             WHERE jsonb_typeof(t.value)='string' AND lower(btrim(t.value #>> '{{}}')) \
             IN (SELECT lower(unnest("
            ))
            .push_bind(filters.tags.clone())
            .push("::text[])))) ");
    }
    if !filters.official_ratings.is_empty() {
        builder
            .push(format!(
                " AND lower(btrim({})) IN (SELECT lower(unnest(",
                catalog_sql::rating("i.id")
            ))
            .push_bind(filters.official_ratings.clone())
            .push("::text[]))) ");
    }
    if !filters.years.is_empty() {
        builder
            .push(format!(" AND {} = ANY(", catalog_sql::year("i.id")))
            .push_bind(filters.years.clone())
            .push(") ");
    }
}
