use std::collections::HashMap;

use chrono::NaiveDate;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use sqlx::{PgPool, Postgres, QueryBuilder, Row};
use uuid::Uuid;

use super::catalog_sql;
use crate::{ApiError, auth::UserRecord};

const MAX_DISPLAY_ITEMS: usize = 10_000;
const DISPLAY_CHUNK: usize = 500;
const MAX_DETAIL_PROVIDERS: usize = 64;

pub(crate) fn music_sort_name(name: &str, disc: Option<i32>, track: Option<i32>) -> String {
    let mut value = String::new();
    for number in [disc, track].into_iter().flatten() {
        use std::fmt::Write;
        write!(value, "{number:04} - ").expect("writing to a String cannot fail");
    }
    value.push_str(name);
    value
}

#[derive(Clone, Debug, Default, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct DisplayMetadata {
    #[serde(skip_serializing)]
    pub(crate) has_current_audio_source: bool,
    pub name: Option<String>,
    pub overview: Option<String>,
    pub premiere_date: Option<NaiveDate>,
    pub genres: Vec<String>,
    pub tags: Vec<String>,
    pub studios: Vec<NamedItem>,
    pub production_year: Option<i32>,
    pub track_number: Option<i32>,
    pub disc_number: Option<i32>,
    pub album: Option<String>,
    /// The original provider label. This is not a numeric policy threshold.
    pub official_rating: Option<String>,
    /// TVMaze's community score, kept separate from official/classification labels.
    pub community_score: Option<f64>,
    pub artwork_url: Option<String>,
    /// Digest used by Jellyfin-style `ImageTags.Primary` cache validation.
    pub primary_image_tag: Option<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct NamedItem {
    pub name: String,
    pub id: Uuid,
}

#[derive(Clone, Debug, Serialize)]
#[serde(rename_all = "PascalCase")]
pub struct ProviderMetadata {
    pub provider_key: String,
    pub external_id: Option<String>,
    pub title: Option<String>,
    pub overview: Option<String>,
    pub premiere_date: Option<NaiveDate>,
    pub genres: Vec<String>,
    pub content_rating: Option<String>,
    pub metadata: Value,
    pub attribution_name: Option<String>,
    pub attribution_url: Option<String>,
    pub attribution_license: Option<String>,
    pub has_artwork: bool,
}

/// Load one bounded item page with a single small-projection query per 500 IDs.
/// Precedence is local NFO, then current embedded audio tags, then enabled local plugin output, then opt-in
/// TVMaze data. Policy values are intentionally absent: core authorization
/// reads only its correlated local-NFO/US-MPAA-v1 row.
pub async fn load_display_metadata(
    pool: &PgPool,
    user: &UserRecord,
    item_ids: &[Uuid],
) -> Result<HashMap<Uuid, DisplayMetadata>, ApiError> {
    if item_ids.len() > MAX_DISPLAY_ITEMS {
        return Err(ApiError::BadRequest(
            "Metadata pages are limited to 10,000 item IDs".to_owned(),
        ));
    }
    let mut result = HashMap::with_capacity(item_ids.len());
    for chunk in item_ids.chunks(DISPLAY_CHUNK) {
        let studios = format!(
            "(SELECT COALESCE(jsonb_agg(jsonb_build_object('Name',value,'Id',{}) ORDER BY value COLLATE \"C\"),'[]'::jsonb) FROM {} names)",
            catalog_sql::STUDIO_ID_SQL,
            catalog_sql::studio_names("requested.item_id")
        );
        let projection = format!(
            "SELECT requested.item_id, {} AS title, {} AS overview, {} AS premiere_date, \
             {} AS genres, {} AS official_rating, {} AS tags, {} AS production_year, \
             {} AS track_number, {} AS disc_number, {} AS album, {studios} AS studios, \
             (SELECT m.metadata_json->>'communityScore' FROM item_metadata m \
              WHERE m.item_id=requested.item_id AND m.provider_key='tvmaze') AS community_score, \
             {} AS primary_image_tag, {} AS has_current_audio_source FROM unnest(",
            catalog_sql::catalog_title("requested.item_id"),
            catalog_sql::preferred("requested.item_id", "m.overview", "m.overview IS NOT NULL"),
            catalog_sql::preferred(
                "requested.item_id",
                "m.premiere_date",
                "m.premiere_date IS NOT NULL"
            ),
            catalog_sql::genres("requested.item_id"),
            catalog_sql::rating("requested.item_id"),
            catalog_sql::tags("requested.item_id"),
            catalog_sql::year("requested.item_id"),
            catalog_sql::music_number("requested.item_id", false),
            catalog_sql::music_number("requested.item_id", true),
            catalog_sql::album("requested.item_id"),
            catalog_sql::preferred(
                "requested.item_id",
                "m.artwork_sha256",
                "m.artwork_size IS NOT NULL"
            ),
            catalog_sql::current_audio_source("requested.item_id"),
        );
        let mut builder = QueryBuilder::<Postgres>::new("");
        crate::db::push_visible_album_tracks(&mut builder, user);
        builder
            .push(projection)
            .push_bind(chunk)
            .push("::uuid[]) AS requested(item_id)");
        let rows = builder.build().fetch_all(pool).await?;
        for row in rows {
            let item_id: Uuid = row.try_get("item_id")?;
            let mut display = DisplayMetadata {
                has_current_audio_source: row.try_get("has_current_audio_source")?,
                name: row.try_get("title")?,
                overview: row.try_get("overview")?,
                premiere_date: row.try_get("premiere_date")?,
                official_rating: row.try_get("official_rating")?,
                production_year: row.try_get("production_year")?,
                track_number: row.try_get("track_number")?,
                disc_number: row.try_get("disc_number")?,
                album: row.try_get("album")?,
                studios: serde_json::from_value(row.try_get("studios")?)
                    .map_err(|_| ApiError::Unavailable)?,
                ..DisplayMetadata::default()
            };
            let genres: Option<Value> = row.try_get("genres")?;
            display.genres = genres
                .and_then(|value| value.as_array().cloned())
                .into_iter()
                .flatten()
                .filter_map(|value| value.as_str().map(str::to_owned))
                .collect();
            let tags: Value = row.try_get("tags")?;
            display.tags = tags
                .as_array()
                .into_iter()
                .flatten()
                .filter_map(|value| value.as_str().map(str::to_owned))
                .collect();
            let score: Option<String> = row.try_get("community_score")?;
            display.community_score = score
                .and_then(|value| value.parse::<f64>().ok())
                .filter(|score| score.is_finite() && (0.0..=10.0).contains(score));
            if let Some(tag) = row.try_get::<Option<String>, _>("primary_image_tag")? {
                let tag = tag.trim().to_owned();
                if !tag.is_empty() {
                    display.artwork_url = Some(format!("/Items/{item_id}/Images/Primary"));
                    display.primary_image_tag = Some(tag);
                }
            }
            result.insert(item_id, display);
        }
    }
    Ok(result)
}

pub(super) async fn load_provider_details(
    pool: &PgPool,
    item_id: Uuid,
) -> Result<Vec<ProviderMetadata>, ApiError> {
    let query = format!(
        "SELECT m.provider_key,m.external_id,m.title,m.overview,m.premiere_date,m.genres,m.content_rating,m.metadata_json,m.attribution_name,m.attribution_url,m.attribution_license,m.artwork_size FROM item_metadata m WHERE m.item_id=$1 AND {} ORDER BY {} LIMIT $2",
        catalog_sql::valid_provider(),
        catalog_sql::PROVIDER_ORDER
    );
    let rows = sqlx::query(&query)
        .bind(item_id)
        .bind((MAX_DETAIL_PROVIDERS + 1) as i64)
        .fetch_all(pool)
        .await?;
    if rows.len() > MAX_DETAIL_PROVIDERS {
        return Err(ApiError::Unavailable);
    }
    let mut providers = Vec::with_capacity(rows.len());
    for row in rows {
        let genres: Value = row.try_get("genres")?;
        let genres = genres
            .as_array()
            .into_iter()
            .flatten()
            .filter_map(Value::as_str)
            .map(str::to_owned)
            .collect();
        providers.push(ProviderMetadata {
            provider_key: row.try_get("provider_key")?,
            external_id: row.try_get("external_id")?,
            title: row.try_get("title")?,
            overview: row.try_get("overview")?,
            premiere_date: row.try_get("premiere_date")?,
            genres,
            content_rating: row.try_get("content_rating")?,
            metadata: row.try_get("metadata_json")?,
            attribution_name: row.try_get("attribution_name")?,
            attribution_url: row.try_get("attribution_url")?,
            attribution_license: row.try_get("attribution_license")?,
            has_artwork: row.try_get::<Option<i32>, _>("artwork_size")?.is_some(),
        });
    }
    Ok(providers)
}
