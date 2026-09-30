use axum::{
    Json, Router,
    extract::{Path, Query, RawQuery, State},
    http::StatusCode,
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, Transaction};
use uuid::Uuid;

use crate::{
    auth::{CurrentUser, UserRecord},
    db,
    error::ApiError,
    library::ItemRecord,
    state::AppState,
};

const MAX_PLAYLIST_ITEMS: usize = 1_000;
const MAX_PAGE_SIZE: i64 = 100;

/// User-owned music playlists. The collection GET and playlist DELETE routes
/// are Puffinbox extensions; the item and metadata routes follow Jellyfin 12.
pub fn router(state: AppState) -> Router {
    Router::new()
        .route("/Playlists", get(list_playlists).post(create_playlist))
        .route(
            "/Playlists/{playlist_id}",
            get(get_playlist)
                .post(update_playlist)
                .delete(delete_playlist),
        )
        .route(
            "/Playlists/{playlist_id}/Items",
            get(get_playlist_items)
                .post(add_playlist_items)
                .delete(remove_playlist_items),
        )
        .route(
            "/Playlists/{playlist_id}/Items/{entry_id}/Move/{new_index}",
            post(move_playlist_item),
        )
        .with_state(state)
}

#[derive(Debug, Deserialize, Default)]
struct CreatePlaylistRequest {
    #[serde(default, rename = "Name", alias = "name")]
    name: Option<String>,
    #[serde(default, rename = "Ids", alias = "ids")]
    ids: Vec<Uuid>,
    #[serde(default, rename = "UserId", alias = "userId")]
    user_id: Option<Uuid>,
    #[serde(default, rename = "MediaType", alias = "mediaType")]
    media_type: Option<String>,
    #[serde(default, rename = "Users", alias = "users")]
    users: Vec<PlaylistUserPermission>,
    #[serde(default, rename = "IsPublic", alias = "isPublic")]
    is_public: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct UpdatePlaylistRequest {
    #[serde(default, rename = "Name", alias = "name")]
    name: Option<String>,
    #[serde(default, rename = "Ids", alias = "ids")]
    ids: Option<Vec<Uuid>>,
    #[serde(default, rename = "Users", alias = "users")]
    users: Option<Vec<PlaylistUserPermission>>,
    #[serde(default, rename = "IsPublic", alias = "isPublic")]
    is_public: Option<bool>,
}

#[derive(Debug, Deserialize)]
struct PlaylistUserPermission {
    #[serde(rename = "UserId", alias = "userId")]
    _user_id: Uuid,
    #[serde(rename = "CanEdit", alias = "canEdit")]
    _can_edit: bool,
}

#[derive(Deserialize, Default)]
struct PageQuery {
    #[serde(default, rename = "StartIndex", alias = "startIndex")]
    start_index: Option<i64>,
    #[serde(default, rename = "Limit", alias = "limit")]
    limit: Option<i64>,
    #[serde(default, rename = "UserId", alias = "userId")]
    user_id: Option<Uuid>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct PlaylistCreationResult {
    id: String,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct PlaylistDto {
    open_access: bool,
    shares: Vec<serde_json::Value>,
    item_ids: Vec<Uuid>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct PlaylistListItemDto {
    id: Uuid,
    name: String,
    server_id: String,
    #[serde(rename = "Type")]
    item_type: &'static str,
    is_folder: bool,
    media_type: &'static str,
    date_created: DateTime<Utc>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct PlaylistItemsResultDto {
    items: Vec<PlaylistItemDto>,
    total_record_count: i32,
    start_index: i32,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct PlaylistItemDto {
    id: Uuid,
    name: String,
    server_id: String,
    source_type: &'static str,
    #[serde(rename = "Type")]
    item_type: &'static str,
    playlist_item_id: String,
    is_folder: bool,
    media_type: &'static str,
    play_access: &'static str,
    run_time_ticks: Option<i64>,
    container: Option<String>,
    date_created: DateTime<Utc>,
    user_data: PlaylistUserDataDto,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct PlaylistUserDataDto {
    played: bool,
    play_count: i32,
    is_favorite: bool,
    playback_position_ticks: i64,
    last_played_date: Option<DateTime<Utc>>,
    item_id: Uuid,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct PlaylistsResultDto {
    items: Vec<PlaylistListItemDto>,
    total_record_count: i32,
    start_index: i32,
}

#[derive(sqlx::FromRow)]
struct PlaylistRecord {
    id: Uuid,
    name: String,
    created_at: DateTime<Utc>,
}

#[derive(Clone, Debug, sqlx::FromRow)]
struct PlaylistEntry {
    id: Uuid,
    item_id: Uuid,
}

struct VisiblePlaylistEntry {
    id: Uuid,
    item: ItemRecord,
}

async fn create_playlist(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    RawQuery(raw_query): RawQuery,
    body: Option<Json<CreatePlaylistRequest>>,
) -> Result<Json<PlaylistCreationResult>, ApiError> {
    let body = body.map(|Json(value)| value).unwrap_or_default();
    let query_name = query_single(raw_query.as_deref(), "Name")?;
    let query_user = query_uuid(raw_query.as_deref(), "UserId")?;
    let query_media_type = query_single(raw_query.as_deref(), "MediaType")?;
    ensure_self_user(&user, query_user.or(body.user_id))?;
    ensure_audio_media_type(query_media_type.as_deref().or(body.media_type.as_deref()))?;
    ensure_private_playlist_request(&body.users, body.is_public)?;

    let query_ids = query_uuids(raw_query.as_deref(), "Ids")?;
    let item_ids = if query_ids.is_empty() {
        body.ids
    } else {
        query_ids
    };
    validate_item_ids(&state, &user, &item_ids).await?;
    let name = validate_name(query_name.or(body.name))?;

    let playlist_id = Uuid::new_v4();
    let mut tx = state.db.begin().await?;
    sqlx::query(
        "INSERT INTO playlists(id,owner_user_id,name,media_type) VALUES ($1,$2,$3,'Audio')",
    )
    .bind(playlist_id)
    .bind(user.id)
    .bind(name)
    .execute(&mut *tx)
    .await?;
    let entries = item_ids
        .into_iter()
        .map(|item_id| PlaylistEntry {
            id: Uuid::new_v4(),
            item_id,
        })
        .collect::<Vec<_>>();
    persist_entries(&mut tx, playlist_id, &entries).await?;
    tx.commit().await?;

    Ok(Json(PlaylistCreationResult {
        id: playlist_id.simple().to_string(),
    }))
}

async fn list_playlists(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Query(query): Query<PageQuery>,
) -> Result<Json<PlaylistsResultDto>, ApiError> {
    ensure_self_user(&user, query.user_id)?;
    let start_index = query.start_index.unwrap_or(0);
    if start_index < 0 {
        return Err(ApiError::BadRequest(
            "StartIndex cannot be negative".to_owned(),
        ));
    }
    let limit = query.limit.unwrap_or(MAX_PAGE_SIZE).clamp(1, MAX_PAGE_SIZE);
    let total_record_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM playlists WHERE owner_user_id=$1")
            .bind(user.id)
            .fetch_one(&state.db)
            .await?;
    let records = sqlx::query_as::<_, PlaylistRecord>(
        "SELECT id,name,created_at FROM playlists WHERE owner_user_id=$1 ORDER BY lower(name),id OFFSET $2 LIMIT $3",
    )
    .bind(user.id)
    .bind(start_index)
    .bind(limit)
    .fetch_all(&state.db)
    .await?;
    let items = records
        .into_iter()
        .map(|record| PlaylistListItemDto {
            id: record.id,
            name: record.name,
            server_id: state.server_id.to_string(),
            item_type: "Playlist",
            is_folder: true,
            media_type: "Audio",
            date_created: record.created_at,
        })
        .collect();
    Ok(Json(PlaylistsResultDto {
        items,
        total_record_count: as_i32(total_record_count)?,
        start_index: as_i32(start_index)?,
    }))
}

async fn get_playlist(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(playlist_id): Path<Uuid>,
) -> Result<Json<PlaylistDto>, ApiError> {
    ensure_owned(&state, &user, playlist_id).await?;
    let items = visible_entries(&state, &user, playlist_id).await?;
    Ok(Json(PlaylistDto {
        open_access: false,
        shares: Vec::new(),
        item_ids: items.into_iter().map(|entry| entry.item.id).collect(),
    }))
}

async fn update_playlist(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(playlist_id): Path<Uuid>,
    Json(body): Json<UpdatePlaylistRequest>,
) -> Result<StatusCode, ApiError> {
    ensure_owned(&state, &user, playlist_id).await?;
    ensure_private_playlist_request(body.users.as_deref().unwrap_or_default(), body.is_public)?;
    let name = body
        .name
        .map(|name| validate_name(Some(name)))
        .transpose()?;
    if let Some(ids) = body.ids.as_ref() {
        validate_item_ids(&state, &user, ids).await?;
    }

    let mut tx = state.db.begin().await?;
    lock_owned_playlist(&mut tx, &user, playlist_id).await?;
    if let Some(name) = name {
        sqlx::query(
            "UPDATE playlists SET name=$3,updated_at=NOW() WHERE id=$1 AND owner_user_id=$2",
        )
        .bind(playlist_id)
        .bind(user.id)
        .bind(name)
        .execute(&mut *tx)
        .await?;
    }
    if let Some(ids) = body.ids {
        let entries = ids
            .into_iter()
            .map(|item_id| PlaylistEntry {
                id: Uuid::new_v4(),
                item_id,
            })
            .collect::<Vec<_>>();
        persist_entries(&mut tx, playlist_id, &entries).await?;
        sqlx::query("UPDATE playlists SET updated_at=NOW() WHERE id=$1")
            .bind(playlist_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn delete_playlist(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(playlist_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    let result = sqlx::query("DELETE FROM playlists WHERE id=$1 AND owner_user_id=$2")
        .bind(playlist_id)
        .bind(user.id)
        .execute(&state.db)
        .await?;
    if result.rows_affected() == 0 {
        return Err(ApiError::NotFound);
    }
    Ok(StatusCode::NO_CONTENT)
}

async fn get_playlist_items(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(playlist_id): Path<Uuid>,
    Query(query): Query<PageQuery>,
) -> Result<Json<PlaylistItemsResultDto>, ApiError> {
    ensure_owned(&state, &user, playlist_id).await?;
    ensure_playback_allowed(&user)?;
    ensure_self_user(&user, query.user_id)?;
    let start_index = query.start_index.unwrap_or(0);
    if start_index < 0 {
        return Err(ApiError::BadRequest(
            "StartIndex cannot be negative".to_owned(),
        ));
    }
    let limit = query.limit.unwrap_or(MAX_PAGE_SIZE).clamp(1, MAX_PAGE_SIZE);
    let mut entries = visible_entries(&state, &user, playlist_id).await?;
    let total_record_count = entries.len() as i64;
    let start = usize::try_from(start_index).unwrap_or(usize::MAX);
    let items = if start >= entries.len() {
        Vec::new()
    } else {
        let end = start.saturating_add(limit as usize).min(entries.len());
        entries.drain(start..end).collect::<Vec<_>>()
    };
    let item_ids = items.iter().map(|entry| entry.item.id).collect::<Vec<_>>();
    let user_data = db::item_user_data(&state.db, user.id, &item_ids).await?;
    let items = items
        .into_iter()
        .map(|entry| {
            let data = user_data.get(&entry.item.id);
            PlaylistItemDto {
                id: entry.item.id,
                name: entry.item.name,
                server_id: state.server_id.to_string(),
                source_type: "Library",
                item_type: "Audio",
                playlist_item_id: entry.id.simple().to_string(),
                is_folder: false,
                media_type: "Audio",
                play_access: "Full",
                run_time_ticks: entry.item.runtime_ticks,
                container: entry.item.container,
                date_created: entry.item.date_added,
                user_data: PlaylistUserDataDto {
                    played: data.is_some_and(|value| value.played),
                    play_count: data.map_or(0, |value| value.play_count),
                    is_favorite: data.is_some_and(|value| value.is_favorite),
                    playback_position_ticks: data.map_or(0, |value| value.playback_position_ticks),
                    last_played_date: data.and_then(|value| value.last_played_at),
                    item_id: entry.item.id,
                },
            }
        })
        .collect();
    Ok(Json(PlaylistItemsResultDto {
        items,
        total_record_count: as_i32(total_record_count)?,
        start_index: as_i32(start_index)?,
    }))
}

async fn add_playlist_items(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(playlist_id): Path<Uuid>,
    RawQuery(raw_query): RawQuery,
) -> Result<StatusCode, ApiError> {
    ensure_owned(&state, &user, playlist_id).await?;
    ensure_playback_allowed(&user)?;
    ensure_self_user(&user, query_uuid(raw_query.as_deref(), "UserId")?)?;
    let item_ids = query_uuids(raw_query.as_deref(), "Ids")?;
    if item_ids.is_empty() {
        return Err(ApiError::BadRequest("Ids is required".to_owned()));
    }
    validate_item_ids(&state, &user, &item_ids).await?;
    let requested_position = query_i32(raw_query.as_deref(), "Position")?;

    let mut tx = state.db.begin().await?;
    lock_owned_playlist(&mut tx, &user, playlist_id).await?;
    let mut entries = read_entries(&mut tx, playlist_id).await?;
    if entries.len().saturating_add(item_ids.len()) > MAX_PLAYLIST_ITEMS {
        return Err(ApiError::BadRequest(
            "Playlist cannot contain more than 1000 items".to_owned(),
        ));
    }
    let position = requested_position.unwrap_or(entries.len() as i32);
    if position < 0 || position as usize > entries.len() {
        return Err(ApiError::BadRequest(
            "Position is outside the playlist".to_owned(),
        ));
    }
    let additions = item_ids
        .into_iter()
        .map(|item_id| PlaylistEntry {
            id: Uuid::new_v4(),
            item_id,
        })
        .collect::<Vec<_>>();
    entries.splice(position as usize..position as usize, additions);
    persist_entries(&mut tx, playlist_id, &entries).await?;
    sqlx::query("UPDATE playlists SET updated_at=NOW() WHERE id=$1")
        .bind(playlist_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_playlist_items(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(playlist_id): Path<Uuid>,
    RawQuery(raw_query): RawQuery,
) -> Result<StatusCode, ApiError> {
    ensure_owned(&state, &user, playlist_id).await?;
    let entry_ids = query_uuids(raw_query.as_deref(), "EntryIds")?;
    if entry_ids.is_empty() {
        return Err(ApiError::BadRequest("EntryIds is required".to_owned()));
    }
    let mut tx = state.db.begin().await?;
    lock_owned_playlist(&mut tx, &user, playlist_id).await?;
    let mut entries = read_entries(&mut tx, playlist_id).await?;
    let original_len = entries.len();
    entries.retain(|entry| !entry_ids.contains(&entry.id));
    if entries.len() != original_len {
        persist_entries(&mut tx, playlist_id, &entries).await?;
        sqlx::query("UPDATE playlists SET updated_at=NOW() WHERE id=$1")
            .bind(playlist_id)
            .execute(&mut *tx)
            .await?;
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn move_playlist_item(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((playlist_id, entry_id, new_index)): Path<(Uuid, Uuid, i32)>,
) -> Result<StatusCode, ApiError> {
    ensure_owned(&state, &user, playlist_id).await?;
    if new_index < 0 {
        return Err(ApiError::BadRequest(
            "NewIndex cannot be negative".to_owned(),
        ));
    }
    let mut tx = state.db.begin().await?;
    lock_owned_playlist(&mut tx, &user, playlist_id).await?;
    let mut entries = read_entries(&mut tx, playlist_id).await?;
    let old_index = entries
        .iter()
        .position(|entry| entry.id == entry_id)
        .ok_or(ApiError::NotFound)?;
    if new_index as usize >= entries.len() {
        return Err(ApiError::BadRequest(
            "NewIndex is outside the playlist".to_owned(),
        ));
    }
    let entry = entries.remove(old_index);
    entries.insert(new_index as usize, entry);
    persist_entries(&mut tx, playlist_id, &entries).await?;
    sqlx::query("UPDATE playlists SET updated_at=NOW() WHERE id=$1")
        .bind(playlist_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn ensure_owned(
    state: &AppState,
    user: &UserRecord,
    playlist_id: Uuid,
) -> Result<(), ApiError> {
    let exists: bool = sqlx::query_scalar(
        "SELECT EXISTS(SELECT 1 FROM playlists WHERE id=$1 AND owner_user_id=$2)",
    )
    .bind(playlist_id)
    .bind(user.id)
    .fetch_one(&state.db)
    .await?;
    if exists {
        Ok(())
    } else {
        Err(ApiError::NotFound)
    }
}

async fn lock_owned_playlist(
    tx: &mut Transaction<'_, Postgres>,
    user: &UserRecord,
    playlist_id: Uuid,
) -> Result<(), ApiError> {
    let row = sqlx::query("SELECT id FROM playlists WHERE id=$1 AND owner_user_id=$2 FOR UPDATE")
        .bind(playlist_id)
        .bind(user.id)
        .fetch_optional(&mut **tx)
        .await?;
    if row.is_some() {
        Ok(())
    } else {
        Err(ApiError::NotFound)
    }
}

async fn read_entries(
    tx: &mut Transaction<'_, Postgres>,
    playlist_id: Uuid,
) -> Result<Vec<PlaylistEntry>, sqlx::Error> {
    sqlx::query_as::<_, PlaylistEntry>(
        "SELECT id,item_id FROM playlist_items WHERE playlist_id=$1 ORDER BY position,id",
    )
    .bind(playlist_id)
    .fetch_all(&mut **tx)
    .await
}

async fn persist_entries(
    tx: &mut Transaction<'_, Postgres>,
    playlist_id: Uuid,
    entries: &[PlaylistEntry],
) -> Result<(), sqlx::Error> {
    sqlx::query("DELETE FROM playlist_items WHERE playlist_id=$1")
        .bind(playlist_id)
        .execute(&mut **tx)
        .await?;
    for (position, entry) in entries.iter().enumerate() {
        sqlx::query(
            "INSERT INTO playlist_items(id,playlist_id,item_id,position) VALUES ($1,$2,$3,$4)",
        )
        .bind(entry.id)
        .bind(playlist_id)
        .bind(entry.item_id)
        .bind(i32::try_from(position).unwrap_or(i32::MAX))
        .execute(&mut **tx)
        .await?;
    }
    Ok(())
}

async fn visible_entries(
    state: &AppState,
    user: &UserRecord,
    playlist_id: Uuid,
) -> Result<Vec<VisiblePlaylistEntry>, ApiError> {
    ensure_playback_allowed(user)?;
    let entries = sqlx::query_as::<_, PlaylistEntry>(
        "SELECT id,item_id FROM playlist_items WHERE playlist_id=$1 ORDER BY position,id",
    )
    .bind(playlist_id)
    .fetch_all(&state.db)
    .await?;
    let mut visible = Vec::with_capacity(entries.len());
    for entry in entries {
        let Some(item) = db::get_item(&state.db, entry.item_id).await? else {
            continue;
        };
        if item.item_type == "Audio" && db::item_visible_to_user(&state.db, user, &item).await? {
            visible.push(VisiblePlaylistEntry { id: entry.id, item });
        }
    }
    Ok(visible)
}

async fn validate_item_ids(
    state: &AppState,
    user: &UserRecord,
    item_ids: &[Uuid],
) -> Result<(), ApiError> {
    if item_ids.len() > MAX_PLAYLIST_ITEMS {
        return Err(ApiError::BadRequest(
            "Playlist cannot contain more than 1000 items".to_owned(),
        ));
    }
    if item_ids.is_empty() {
        return Ok(());
    }
    ensure_playback_allowed(user)?;
    for item_id in item_ids {
        let item = db::get_item(&state.db, *item_id)
            .await?
            .ok_or(ApiError::NotFound)?;
        if item.item_type != "Audio" {
            return Err(ApiError::BadRequest(
                "Only Audio items can be added to music playlists".to_owned(),
            ));
        }
        if !db::item_visible_to_user(&state.db, user, &item).await? {
            return Err(ApiError::NotFound);
        }
    }
    Ok(())
}

fn ensure_self_user(user: &UserRecord, requested: Option<Uuid>) -> Result<(), ApiError> {
    if requested.is_some_and(|requested| requested != user.id) {
        Err(ApiError::Forbidden)
    } else {
        Ok(())
    }
}

fn ensure_playback_allowed(user: &UserRecord) -> Result<(), ApiError> {
    if user.allow_media_playback {
        Ok(())
    } else {
        Err(ApiError::Forbidden)
    }
}

fn ensure_audio_media_type(media_type: Option<&str>) -> Result<(), ApiError> {
    if media_type.is_none_or(|value| value.eq_ignore_ascii_case("Audio")) {
        Ok(())
    } else {
        Err(ApiError::BadRequest(
            "Only Audio playlists are supported".to_owned(),
        ))
    }
}

fn ensure_private_playlist_request(
    users: &[PlaylistUserPermission],
    is_public: Option<bool>,
) -> Result<(), ApiError> {
    if !users.is_empty() {
        Err(ApiError::BadRequest(
            "Playlist sharing is not supported".to_owned(),
        ))
    } else if is_public == Some(true) {
        Err(ApiError::BadRequest(
            "Public playlists are not supported".to_owned(),
        ))
    } else {
        Ok(())
    }
}

fn validate_name(name: Option<String>) -> Result<String, ApiError> {
    let name = name
        .map(|name| name.trim().to_owned())
        .filter(|name| !name.is_empty())
        .ok_or_else(|| ApiError::BadRequest("Name is required".to_owned()))?;
    if name.chars().count() > 255 {
        return Err(ApiError::BadRequest(
            "Name exceeds the 255-character limit".to_owned(),
        ));
    }
    Ok(name)
}

fn as_i32(value: i64) -> Result<i32, ApiError> {
    i32::try_from(value)
        .map_err(|_| ApiError::Internal("playlist result exceeds the API range".to_owned()))
}

fn query_values(raw_query: Option<&str>, key: &str) -> Vec<String> {
    raw_query
        .into_iter()
        .flat_map(|query| url::form_urlencoded::parse(query.as_bytes()))
        .filter(|(name, _)| name.eq_ignore_ascii_case(key))
        .map(|(_, value)| value.into_owned())
        .collect()
}

fn query_single(raw_query: Option<&str>, key: &str) -> Result<Option<String>, ApiError> {
    let values = query_values(raw_query, key);
    if values.len() > 1 {
        return Err(ApiError::BadRequest(format!(
            "{key} may only be specified once"
        )));
    }
    Ok(values.into_iter().next())
}

fn query_uuid(raw_query: Option<&str>, key: &str) -> Result<Option<Uuid>, ApiError> {
    query_single(raw_query, key)?
        .map(|value| {
            Uuid::parse_str(&value)
                .map_err(|_| ApiError::BadRequest(format!("{key} must be a UUID")))
        })
        .transpose()
}

fn query_i32(raw_query: Option<&str>, key: &str) -> Result<Option<i32>, ApiError> {
    query_single(raw_query, key)?
        .map(|value| {
            value
                .parse::<i32>()
                .map_err(|_| ApiError::BadRequest(format!("{key} must be an integer")))
        })
        .transpose()
}

fn query_uuids(raw_query: Option<&str>, key: &str) -> Result<Vec<Uuid>, ApiError> {
    let mut values = Vec::new();
    for value in query_values(raw_query, key) {
        for value in value
            .split(',')
            .map(str::trim)
            .filter(|value| !value.is_empty())
        {
            values.push(
                Uuid::parse_str(value)
                    .map_err(|_| ApiError::BadRequest(format!("{key} must contain UUIDs")))?,
            );
            if values.len() > MAX_PLAYLIST_ITEMS {
                return Err(ApiError::BadRequest(
                    "Playlist request contains more than 1000 items".to_owned(),
                ));
            }
        }
    }
    Ok(values)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn query_uuid_lists_accept_repeated_and_comma_delimited_values() {
        let first = Uuid::new_v4();
        let second = Uuid::new_v4();
        let query = format!("Ids={first}&ids={second}%2C{first}");
        assert_eq!(
            query_uuids(Some(&query), "Ids").unwrap(),
            vec![first, second, first]
        );
    }

    #[test]
    fn playlist_name_and_media_type_validation_are_bounded() {
        assert_eq!(
            validate_name(Some("  Sunday drive  ".to_owned())).unwrap(),
            "Sunday drive"
        );
        assert!(validate_name(Some("  ".to_owned())).is_err());
        assert!(ensure_audio_media_type(Some("Video")).is_err());
        assert!(ensure_audio_media_type(Some("Unknown")).is_err());
        assert!(ensure_audio_media_type(Some("Audio")).is_ok());
    }
}
