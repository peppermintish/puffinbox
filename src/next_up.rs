use axum::{
    Json, Router,
    extract::{Query, State},
    http::Uri,
    routing::get,
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    ApiError, AppState,
    api::{self, BaseItemDto},
    auth::CurrentUser,
    db,
};

pub(crate) fn router(state: AppState) -> Router {
    Router::new()
        .route("/Shows/NextUp", get(next_up))
        .with_state(state)
}

#[derive(Default, Deserialize)]
struct NextUpParams {
    #[serde(rename = "userId", alias = "UserId")]
    user_id: Option<Uuid>,
    #[serde(rename = "seriesId", alias = "SeriesId")]
    series_id: Option<Uuid>,
    #[serde(rename = "parentId", alias = "ParentId")]
    parent_id: Option<Uuid>,
    #[serde(rename = "startIndex", alias = "StartIndex")]
    start_index: Option<i64>,
    #[serde(rename = "limit", alias = "Limit")]
    limit: Option<i64>,
    #[serde(rename = "fields", alias = "Fields")]
    fields: Option<String>,
    #[serde(rename = "nextUpDateCutoff", alias = "NextUpDateCutoff")]
    date_cutoff: Option<DateTime<Utc>>,
    #[serde(rename = "enableTotalRecordCount", alias = "EnableTotalRecordCount")]
    total_count: Option<bool>,
    #[serde(rename = "enableResumable", alias = "EnableResumable")]
    resumable: Option<bool>,
    #[serde(rename = "enableRewatching", alias = "EnableRewatching")]
    rewatching: Option<bool>,
    #[serde(rename = "enableImages", alias = "EnableImages")]
    images: Option<bool>,
    #[serde(rename = "enableUserData", alias = "EnableUserData")]
    user_data: Option<bool>,
    #[serde(rename = "imageTypeLimit", alias = "ImageTypeLimit")]
    image_limit: Option<i32>,
    #[serde(rename = "enableImageTypes", alias = "EnableImageTypes")]
    image_types: Option<String>,
}

impl NextUpParams {
    fn from_uri(uri: &Uri) -> Result<Self, ApiError> {
        let raw = uri.query().unwrap_or_default();
        if raw.len() > 16_384 {
            return Err(ApiError::BadRequest("Next Up query is too long".to_owned()));
        }
        let mut query = url::form_urlencoded::Serializer::new(String::new());
        let mut fields = Vec::new();
        let mut image_types = Vec::new();
        for (key, value) in url::form_urlencoded::parse(raw.as_bytes()) {
            match key.as_ref() {
                "Fields" | "fields" => fields.push(value.into_owned()),
                "EnableImageTypes" | "enableImageTypes" => image_types.push(value.into_owned()),
                _ => {
                    query.append_pair(&key, &value);
                }
            }
        }
        // Public clients use CSV and repeated values for these two arrays.
        // Scalars stay separate, and re-encoding keeps delimiters inside values.
        if !fields.is_empty() {
            query.append_pair("fields", &fields.join(","));
        }
        if !image_types.is_empty() {
            query.append_pair("enableImageTypes", &image_types.join(","));
        }
        let normalized = Uri::builder()
            .path_and_query(format!("{}?{}", uri.path(), query.finish()))
            .build()
            .map_err(|_| ApiError::BadRequest("Invalid Next Up query".to_owned()))?;
        Query::<Self>::try_from_uri(&normalized)
            .map(|Query(params)| params)
            .map_err(|_| ApiError::BadRequest("Invalid Next Up query".to_owned()))
    }
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct NextUpResult {
    items: Vec<BaseItemDto>,
    total_record_count: i64,
    start_index: i64,
}

async fn next_up(
    State(state): State<AppState>,
    CurrentUser(current): CurrentUser,
    uri: Uri,
) -> Result<Json<NextUpResult>, ApiError> {
    let params = NextUpParams::from_uri(&uri)?;
    let user = api::selected_user(&state, &current, params.user_id).await?;
    let start = params.start_index.unwrap_or(0);
    let maximum = state.config.max_page_size.clamp(1, 100);
    let unlimited = params.limit.is_none_or(|limit| limit == 0);
    let limit = params.limit.unwrap_or(maximum);
    if !(0..=i64::from(i32::MAX)).contains(&start)
        || limit < 0
        || params.image_limit.is_some_and(|limit| limit < 0)
        || params
            .fields
            .as_ref()
            .is_some_and(|fields| fields.len() > 4096 || fields.chars().any(char::is_control))
        || params
            .image_types
            .as_ref()
            .is_some_and(|types| types.len() > 512 || types.chars().any(char::is_control))
    {
        return Err(ApiError::BadRequest("Invalid Next Up options".to_owned()));
    }
    let mut library_id = None;
    let mut folder_scope = false;
    if let Some(parent) = params.parent_id {
        if let Some(library) = db::get_library(&state.db, parent).await? {
            if !db::library_visible_to_user(&state.db, &user, library.id).await? {
                return Err(ApiError::NotFound);
            }
            library_id = Some(library.id);
        } else {
            crate::catalog_navigation::visible_item(&state, &user, parent).await?;
            // The observed Jellyfin 12 operation scopes parentId to library
            // roots; an item parent does not select episodes within a series.
            folder_scope = true;
        }
    }
    if let Some(series) = params.series_id {
        let item = crate::catalog_navigation::visible_item(&state, &user, series).await?;
        if item.item_type != "Series" {
            return Err(ApiError::NotFound);
        }
    }
    if folder_scope {
        return Ok(Json(NextUpResult {
            items: Vec::new(),
            total_record_count: 0,
            start_index: start,
        }));
    }
    let (items, total) = db::next_up_items(
        &state.db,
        &user,
        &db::NextUpQuery {
            series_id: params.series_id,
            library_id,
            date_cutoff: params.date_cutoff,
            resumable: params.resumable.unwrap_or(true),
            rewatching: params.rewatching.unwrap_or(false),
            start,
            limit: if unlimited {
                maximum
            } else {
                limit.min(maximum)
            },
        },
    )
    .await?;
    if unlimited && total.saturating_sub(start) > maximum {
        return Err(ApiError::Conflict(
            "Next Up exceeds the page limit; request an explicit Limit and StartIndex".to_owned(),
        ));
    }
    let images = params.images.unwrap_or(true)
        && params.image_limit != Some(0)
        && params
            .image_types
            .as_ref()
            .is_none_or(|types| types.split(',').any(|kind| kind.trim() == "Primary"));
    let mut items = api::item_dtos_for_user(&state, &user, &items).await?;
    for item in &mut items {
        item.include_images_and_user_data(images, params.user_data.unwrap_or(true));
    }
    Ok(Json(NextUpResult {
        items,
        total_record_count: if params.total_count.unwrap_or(true) {
            total
        } else {
            0
        },
        start_index: start,
    }))
}
