use axum::{
    Json, Router,
    extract::{Query, State},
    routing::get,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{ApiError, AppState, api, auth::CurrentUser, db, library::ItemFacetFilters};

pub(crate) fn router(state: AppState) -> Router {
    Router::new()
        .route("/Items/Filters", get(legacy_filters))
        .route("/Items/Filters2", get(filters))
        .with_state(state)
}

#[derive(Default, Deserialize)]
struct FilterQuery {
    #[serde(rename = "userId", alias = "UserId")]
    user_id: Option<Uuid>,
    #[serde(rename = "parentId", alias = "ParentId")]
    parent_id: Option<Uuid>,
    #[serde(rename = "includeItemTypes", alias = "IncludeItemTypes")]
    include_item_types: Option<String>,
    #[serde(rename = "mediaTypes", alias = "MediaTypes")]
    media_types: Option<String>,
    #[serde(rename = "recursive", alias = "Recursive")]
    recursive: Option<bool>,
    #[serde(rename = "isAiring", alias = "IsAiring")]
    is_airing: Option<bool>,
    #[serde(rename = "isMovie", alias = "IsMovie")]
    is_movie: Option<bool>,
    #[serde(rename = "isSports", alias = "IsSports")]
    is_sports: Option<bool>,
    #[serde(rename = "isKids", alias = "IsKids")]
    is_kids: Option<bool>,
    #[serde(rename = "isNews", alias = "IsNews")]
    is_news: Option<bool>,
    #[serde(rename = "isSeries", alias = "IsSeries")]
    is_series: Option<bool>,
}

async fn scoped_facets(
    state: &AppState,
    current: &crate::auth::UserRecord,
    query: FilterQuery,
) -> Result<db::CatalogFacets, ApiError> {
    if [
        query.is_airing,
        query.is_movie,
        query.is_sports,
        query.is_kids,
        query.is_news,
        query.is_series,
    ]
    .into_iter()
    .any(|value| value.is_some())
    {
        return Err(ApiError::BadRequest(
            "Live TV classification filters are not supported here".to_owned(),
        ));
    }
    let user = api::selected_user(state, current, query.user_id).await?;
    api::ensure_parent_visible(state, &user, query.parent_id).await?;
    // Reuse ordinary browse validation and authorization, while retrieving distinct
    // values in SQL instead of scanning a truncated application page.
    let scope = api::item_query(
        api::ItemsQueryParams::facet_scope(
            query.parent_id,
            query.include_item_types,
            query.media_types,
            query.recursive.unwrap_or(true),
        ),
        state,
    )?;
    db::catalog_facets(&state.db, &user, &scope).await
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct LegacyFilters {
    genres: Vec<String>,
    tags: Vec<String>,
    official_ratings: Vec<String>,
    years: Vec<i32>,
}

async fn legacy_filters(
    State(state): State<AppState>,
    CurrentUser(current): CurrentUser,
    Query(query): Query<FilterQuery>,
) -> Result<Json<LegacyFilters>, ApiError> {
    let facets = scoped_facets(&state, &current, query).await?;
    Ok(Json(LegacyFilters {
        genres: facets.genres.into_iter().map(|value| value.name).collect(),
        tags: facets.tags,
        official_ratings: facets.official_ratings,
        years: facets.years,
    }))
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct Filters {
    genres: Vec<db::GenreFacet>,
    tags: Vec<String>,
    audio_languages: Vec<LanguageFacet>,
    subtitle_languages: Vec<LanguageFacet>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct LanguageFacet {
    name: String,
    value: String,
}

async fn filters(
    State(state): State<AppState>,
    CurrentUser(current): CurrentUser,
    Query(query): Query<FilterQuery>,
) -> Result<Json<Filters>, ApiError> {
    let facets = scoped_facets(&state, &current, query).await?;
    Ok(Json(Filters {
        genres: facets.genres,
        tags: facets.tags,
        audio_languages: Vec::new(),
        subtitle_languages: Vec::new(),
    }))
}

fn values(raw: Option<&str>, separator: char, name: &str) -> Result<Vec<String>, ApiError> {
    let raw = raw.unwrap_or("");
    if raw.len() > 8192 {
        return Err(ApiError::BadRequest(format!(
            "{name} exceeds the query length limit"
        )));
    }
    let mut result = Vec::new();
    for value in raw
        .split(separator)
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        if value.len() > 128 || value.chars().any(char::is_control) || result.len() >= 32 {
            return Err(ApiError::BadRequest(format!(
                "{name} accepts at most 32 values of 128 bytes"
            )));
        }
        result.push(value.to_owned());
    }
    Ok(result)
}

pub(crate) fn selections(
    genres: Option<&str>,
    ids: Option<&str>,
    tags: Option<&str>,
    ratings: Option<&str>,
    years: Option<&str>,
) -> Result<ItemFacetFilters, ApiError> {
    Ok(ItemFacetFilters {
        genres: values(genres, '|', "Genres")?,
        genre_ids: values(ids, '|', "GenreIds")?
            .into_iter()
            .map(|value| {
                Uuid::parse_str(&value)
                    .map_err(|_| ApiError::BadRequest("GenreIds must contain UUIDs".to_owned()))
            })
            .collect::<Result<_, _>>()?,
        tags: values(tags, '|', "Tags")?,
        official_ratings: values(ratings, '|', "OfficialRatings")?,
        years: values(years, ',', "Years")?
            .into_iter()
            .map(|value| {
                value
                    .parse::<i32>()
                    .ok()
                    .filter(|year| (1800..=2300).contains(year))
                    .ok_or_else(|| {
                        ApiError::BadRequest("Years must be between 1800 and 2300".to_owned())
                    })
            })
            .collect::<Result<_, _>>()?,
    })
}
