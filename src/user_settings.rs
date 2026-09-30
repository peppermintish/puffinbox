use std::collections::BTreeMap;

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    http::StatusCode,
    routing::{get, post},
};
use serde::{Deserialize, Serialize};
use sqlx::types::Json as SqlJson;
use uuid::Uuid;

use crate::{auth::CurrentUser, db, error::ApiError, state::AppState};

pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/Users/Configuration", post(update_configuration))
        .route(
            "/DisplayPreferences/{displayPreferencesId}",
            get(get_display_preferences).post(update_display_preferences),
        )
        .with_state(state)
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
pub struct UserConfiguration {
    pub audio_language_preference: Option<String>,
    pub play_default_audio_track: bool,
    pub subtitle_language_preference: Option<String>,
    pub display_missing_episodes: bool,
    pub grouped_folders: Vec<Uuid>,
    pub subtitle_mode: SubtitleMode,
    pub display_collections_view: bool,
    pub enable_local_password: bool,
    pub ordered_views: Vec<Uuid>,
    pub latest_items_excludes: Vec<Uuid>,
    pub my_media_excludes: Vec<Uuid>,
    pub hide_played_in_latest: bool,
    pub remember_audio_selections: bool,
    pub remember_subtitle_selections: bool,
    pub enable_next_episode_auto_play: bool,
    pub cast_receiver_id: Option<String>,
}

impl Default for UserConfiguration {
    fn default() -> Self {
        Self {
            audio_language_preference: None,
            play_default_audio_track: true,
            subtitle_language_preference: None,
            display_missing_episodes: false,
            grouped_folders: Vec::new(),
            subtitle_mode: SubtitleMode::Default,
            display_collections_view: false,
            enable_local_password: false,
            ordered_views: Vec::new(),
            latest_items_excludes: Vec::new(),
            my_media_excludes: Vec::new(),
            hide_played_in_latest: false,
            remember_audio_selections: true,
            remember_subtitle_selections: true,
            enable_next_episode_auto_play: true,
            cast_receiver_id: None,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub enum SubtitleMode {
    #[default]
    Default,
    Always,
    OnlyForced,
    None,
    Smart,
}

impl UserConfiguration {
    fn validate(&self) -> Result<(), ApiError> {
        if self.enable_local_password {
            return Err(ApiError::BadRequest(
                "Local PIN authentication is unavailable".to_owned(),
            ));
        }
        if self
            .audio_language_preference
            .iter()
            .chain(self.subtitle_language_preference.iter())
            .any(|value| !bounded_text(value, 32))
            || self
                .cast_receiver_id
                .iter()
                .any(|value| !bounded_text(value, 512))
            || [
                &self.grouped_folders,
                &self.ordered_views,
                &self.latest_items_excludes,
                &self.my_media_excludes,
            ]
            .iter()
            .any(|values| values.len() > 256)
        {
            return Err(ApiError::BadRequest(
                "User configuration exceeds supported limits".to_owned(),
            ));
        }
        Ok(())
    }
}

#[derive(Default, Deserialize)]
struct UserQuery {
    #[serde(rename = "userId", alias = "UserId")]
    user_id: Option<Uuid>,
}

fn owner_id(user: &crate::auth::UserRecord, requested: Option<Uuid>) -> Result<Uuid, ApiError> {
    let owner = requested.unwrap_or(user.id);
    if owner != user.id && !user.is_admin {
        return Err(ApiError::Forbidden);
    }
    Ok(owner)
}

async fn update_configuration(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(query): Query<UserQuery>,
    Json(configuration): Json<UserConfiguration>,
) -> Result<StatusCode, ApiError> {
    let owner = owner_id(&user, query.user_id)?;
    configuration.validate()?;
    let mut tx = state.db.begin().await?;
    db::require_active_run(&mut tx, state.run_id).await?;
    let result = sqlx::query("UPDATE users SET configuration=$1 WHERE id=$2")
        .bind(SqlJson(configuration))
        .bind(owner)
        .execute(&mut *tx)
        .await?;
    if result.rows_affected() != 1 {
        return Err(ApiError::NotFound);
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

#[derive(Default, Deserialize)]
struct DisplayQuery {
    #[serde(rename = "userId", alias = "UserId")]
    user_id: Option<Uuid>,
    #[serde(alias = "Client")]
    client: String,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(default, rename_all = "PascalCase")]
struct DisplayPreferences {
    id: Option<String>,
    view_type: Option<String>,
    sort_by: Option<String>,
    index_by: Option<String>,
    remember_indexing: bool,
    primary_image_height: i32,
    primary_image_width: i32,
    custom_prefs: BTreeMap<String, Option<String>>,
    scroll_direction: ScrollDirection,
    show_backdrop: bool,
    remember_sorting: bool,
    sort_order: SortOrder,
    show_sidebar: bool,
    client: Option<String>,
}

impl Default for DisplayPreferences {
    fn default() -> Self {
        Self {
            id: None,
            view_type: Some("Poster".to_owned()),
            sort_by: Some("SortName".to_owned()),
            index_by: None,
            remember_indexing: false,
            primary_image_height: 160,
            primary_image_width: 160,
            custom_prefs: BTreeMap::new(),
            scroll_direction: ScrollDirection::Vertical,
            show_backdrop: true,
            remember_sorting: true,
            sort_order: SortOrder::Ascending,
            show_sidebar: false,
            client: None,
        }
    }
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
enum ScrollDirection {
    Horizontal,
    #[default]
    Vertical,
}

#[derive(Clone, Debug, Default, Deserialize, Serialize)]
enum SortOrder {
    #[default]
    Ascending,
    Descending,
}

fn bounded_text(value: &str, limit: usize) -> bool {
    value.len() <= limit && !value.chars().any(char::is_control)
}

fn validate_display_key(id: &str, client: &str) -> Result<(), ApiError> {
    if id.is_empty() || client.is_empty() || !bounded_text(id, 128) || !bounded_text(client, 128) {
        return Err(ApiError::BadRequest(
            "Invalid display preference identity".to_owned(),
        ));
    }
    Ok(())
}

async fn get_display_preferences(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Query(query): Query<DisplayQuery>,
) -> Result<Json<DisplayPreferences>, ApiError> {
    let owner = owner_id(&user, query.user_id)?;
    validate_display_key(&id, &query.client)?;
    if db::get_user(&state.db, owner).await?.is_none() {
        return Err(ApiError::NotFound);
    }
    let stored: Option<SqlJson<DisplayPreferences>> = sqlx::query_scalar("SELECT preferences FROM user_display_preferences WHERE user_id=$1 AND preference_id=$2 AND client=$3")
        .bind(owner).bind(&id).bind(&query.client).fetch_optional(&state.db).await?;
    let mut preferences = stored.map(|value| value.0).unwrap_or_default();
    preferences.id = Some(id);
    preferences.client = Some(query.client);
    Ok(Json(preferences))
}

async fn update_display_preferences(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<String>,
    Query(query): Query<DisplayQuery>,
    Json(mut preferences): Json<DisplayPreferences>,
) -> Result<StatusCode, ApiError> {
    let owner = owner_id(&user, query.user_id)?;
    validate_display_key(&id, &query.client)?;
    if preferences.id.as_ref().is_some_and(|value| value != &id)
        || preferences
            .client
            .as_ref()
            .is_some_and(|value| value != &query.client)
        || preferences
            .view_type
            .iter()
            .chain(preferences.sort_by.iter())
            .chain(preferences.index_by.iter())
            .any(|value| !bounded_text(value, 128))
        || !(0..=4096).contains(&preferences.primary_image_height)
        || !(0..=4096).contains(&preferences.primary_image_width)
        || preferences.custom_prefs.len() > 128
        || preferences.custom_prefs.iter().any(|(key, value)| {
            !bounded_text(key, 128)
                || value
                    .as_ref()
                    .is_some_and(|value| !bounded_text(value, 4096))
        })
    {
        return Err(ApiError::BadRequest(
            "Invalid display preferences".to_owned(),
        ));
    }
    preferences.id = Some(id.clone());
    preferences.client = Some(query.client.clone());
    let mut tx = state.db.begin().await?;
    db::require_active_run(&mut tx, state.run_id).await?;
    let existing_user: Option<Uuid> =
        sqlx::query_scalar("SELECT id FROM users WHERE id=$1 FOR UPDATE")
            .bind(owner)
            .fetch_optional(&mut *tx)
            .await?;
    if existing_user.is_none() {
        return Err(ApiError::NotFound);
    }
    let new_group: bool = sqlx::query_scalar("SELECT NOT EXISTS(SELECT 1 FROM user_display_preferences WHERE user_id=$1 AND preference_id=$2 AND client=$3)")
        .bind(owner).bind(&id).bind(&query.client).fetch_one(&mut *tx).await?;
    if new_group {
        let count: i64 =
            sqlx::query_scalar("SELECT COUNT(*) FROM user_display_preferences WHERE user_id=$1")
                .bind(owner)
                .fetch_one(&mut *tx)
                .await?;
        if count >= 4096 {
            return Err(ApiError::Conflict(
                "Display preference group limit reached".to_owned(),
            ));
        }
    }
    sqlx::query("INSERT INTO user_display_preferences(user_id,preference_id,client,preferences) VALUES ($1,$2,$3,$4) ON CONFLICT(user_id,preference_id,client) DO UPDATE SET preferences=EXCLUDED.preferences")
        .bind(owner).bind(id).bind(query.client).bind(SqlJson(preferences)).execute(&mut *tx).await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}
