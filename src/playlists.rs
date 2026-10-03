use axum::{
    Json, Router,
    extract::{Path, Query, RawQuery, State},
    http::StatusCode,
    response::{IntoResponse, Response},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use sqlx::{Postgres, QueryBuilder, Transaction};
use uuid::Uuid;

use crate::{
    auth::{CurrentUser, UserRecord},
    db,
    error::ApiError,
    library::{ItemQuery, ItemRecord, MediaType},
    state::AppState,
};

const MAX_PLAYLIST_ITEMS: usize = 1_000;
const MAX_PAGE_SIZE: i64 = 100;
const MAX_PLAYLIST_USERS: usize = 1_000;

/// Owned and explicitly shared music playlists. The collection GET and playlist DELETE routes
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
            "/Playlists/{playlist_id}/Users/{user_id}",
            get(get_playlist_user)
                .post(update_playlist_user)
                .delete(remove_playlist_user),
        )
        .route("/Playlists/{playlist_id}/Users", get(get_playlist_users))
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

#[derive(Debug, Deserialize, Serialize, sqlx::FromRow)]
#[serde(deny_unknown_fields)]
struct PlaylistUserPermission {
    #[serde(rename = "UserId", alias = "userId")]
    user_id: Uuid,
    #[serde(rename = "CanEdit", alias = "canEdit")]
    can_edit: bool,
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
    shares: Vec<PlaylistUserPermission>,
    item_ids: Vec<Uuid>,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct PlaylistOwnerPermissionDto {
    user_id: Uuid,
    can_edit: bool,
}

async fn get_playlist_user(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((playlist_id, user_id)): Path<(Uuid, Uuid)>,
) -> Result<Json<PlaylistOwnerPermissionDto>, ApiError> {
    let access = playlist_access(&state, &user, playlist_id).await?;
    if user_id != user.id && access.owner_user_id != user.id {
        return Err(ApiError::Forbidden);
    }
    let can_edit = if user_id == access.owner_user_id {
        true
    } else if let Some(can_edit) = sqlx::query_scalar::<_, bool>(
        "SELECT can_edit FROM playlist_users WHERE playlist_id=$1 AND user_id=$2",
    )
    .bind(playlist_id)
    .bind(user_id)
    .fetch_optional(&state.db)
    .await?
    {
        can_edit
    } else {
        return Err(ApiError::NotFound);
    };
    Ok(Json(PlaylistOwnerPermissionDto { user_id, can_edit }))
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
    date_modified: DateTime<Utc>,
    can_delete: bool,
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
    updated_at: DateTime<Utc>,
    owner_user_id: Uuid,
}

impl PlaylistRecord {
    fn into_dto(self, server_id: Uuid, user_id: Uuid) -> PlaylistListItemDto {
        PlaylistListItemDto {
            id: self.id,
            name: self.name,
            server_id: server_id.to_string(),
            item_type: "Playlist",
            is_folder: true,
            media_type: "Audio",
            date_created: self.created_at,
            date_modified: self.updated_at,
            can_delete: self.owner_user_id == user_id,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct PlaylistCatalogResultDto {
    items: Vec<PlaylistListItemDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_record_count: Option<i64>,
    start_index: i64,
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct PlaylistCatalogEntriesDto {
    items: Vec<PlaylistItemDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    total_record_count: Option<i64>,
    start_index: i64,
}

pub(crate) async fn catalog_children_result(
    state: &AppState,
    user: &UserRecord,
    playlist_id: Uuid,
    query: ItemQuery,
) -> Result<Response, ApiError> {
    playlist_access(state, user, playlist_id).await?;
    if query.is_folder.is_some()
        || !query.exclude_item_ids.is_empty()
        || !query.artist_ids.is_empty()
        || !query.album_artist_ids.is_empty()
        || query.is_played.is_some()
        || query.is_favorite
        || query.is_liked.is_some()
        || query.is_resumable
        || query.search_term.is_some()
        || !query.facets.genres.is_empty()
        || !query.facets.genre_ids.is_empty()
        || !query.facets.tags.is_empty()
        || !query.facets.official_ratings.is_empty()
        || !query.facets.years.is_empty()
    {
        return Err(ApiError::BadRequest(
            "Playlist entry catalog filters are not available yet".to_owned(),
        ));
    }
    let selected = (query.include_item_types.is_empty()
        || query
            .include_item_types
            .iter()
            .any(|kind| kind.eq_ignore_ascii_case("Audio")))
        && (query.media_types.is_empty() || query.media_types.contains(&MediaType::Audio));
    ensure_playback_allowed(user)?;
    let (items, total) = if selected {
        playlist_item_page(state, user, playlist_id, query.start_index, query.limit).await?
    } else {
        (Vec::new(), 0)
    };
    Ok(Json(PlaylistCatalogEntriesDto {
        items,
        total_record_count: query.enable_total_record_count.then_some(total),
        start_index: query.start_index,
    })
    .into_response())
}

pub(crate) fn validate_catalog_query(raw_query: Option<&str>) -> Result<(), ApiError> {
    validate_catalog_options(raw_query, false)
}

pub(crate) fn validate_catalog_children_query(raw_query: Option<&str>) -> Result<(), ApiError> {
    validate_catalog_options(raw_query, true)
}

fn validate_catalog_options(raw_query: Option<&str>, entries: bool) -> Result<(), ApiError> {
    for (key, value) in url::form_urlencoded::parse(raw_query.unwrap_or_default().as_bytes()) {
        if entries {
            match key.to_ascii_lowercase().as_str() {
                // Entries contain scanned Audio files, never virtual items or box sets.
                "excludelocationtypes" if value.eq_ignore_ascii_case("Virtual") => continue,
                "collapseboxsetitems" if value.eq_ignore_ascii_case("false") => continue,
                _ => {}
            }
        }
        if !matches!(
            key.to_ascii_lowercase().as_str(),
            "parentid" | "searchterm" | "includeitemtypes" | "mediatypes" | "recursive"
            | "startindex" | "limit" | "enabletotalrecordcount" | "sortby" | "sortorder"
            | "isplayed" | "filters" | "userid" | "genres" | "genreids" | "tags"
            | "officialratings" | "years" | "audiolanguages" | "subtitlelanguages"
            // These presentation options do not change which playlists are selected.
            | "fields" | "enableuserdata" | "enableimages" | "imagetypelimit" | "enableimagetypes"
        ) {
            return Err(ApiError::BadRequest(
                "Unsupported playlist catalog query parameter".to_owned(),
            ));
        }
    }
    Ok(())
}

/// Playlists are permission-checked catalog objects, separate from scanned library files.
pub(crate) async fn catalog_result(
    state: &AppState,
    user: &UserRecord,
    query: ItemQuery,
) -> Result<Response, ApiError> {
    if let Some(parent_id) = query.parent_id {
        let library = db::get_library(&state.db, parent_id)
            .await?
            .ok_or(ApiError::NotFound)?;
        if !db::library_visible_to_user(&state.db, user, library.id).await? {
            return Err(ApiError::NotFound);
        }
        if !library.collection_type.eq_ignore_ascii_case("music") {
            return Err(ApiError::BadRequest(
                "Playlist parent views require a music library".to_owned(),
            ));
        }
    }
    if query.include_item_types.len() != 1
        || !query.item_ids.is_empty()
        || !query.exclude_item_ids.is_empty()
        || !query.artist_ids.is_empty()
        || !query.album_artist_ids.is_empty()
        || query.is_folder.is_some()
        || query.is_played.is_some()
        || query.is_favorite
        || query.is_liked.is_some()
        || query.is_resumable
        || !query.facets.genres.is_empty()
        || !query.facets.genre_ids.is_empty()
        || !query.facets.tags.is_empty()
        || !query.facets.official_ratings.is_empty()
        || !query.facets.years.is_empty()
    {
        return Err(ApiError::BadRequest(
            "Playlist catalog queries require only Playlist, without playback or metadata filters"
                .to_owned(),
        ));
    }
    let sort_column = match query.sort_by.to_ascii_lowercase().as_str() {
        "name" | "sortname" => "lower(p.name)",
        "datecreated" | "dateadded" => "p.created_at",
        "datemodified" => "p.updated_at",
        _ => {
            return Err(ApiError::BadRequest(
                "Unsupported playlist SortBy field".to_owned(),
            ));
        }
    };
    let audio_selected =
        query.media_types.is_empty() || query.media_types.contains(&MediaType::Audio);
    let total_record_count = if query.enable_total_record_count {
        let mut count = QueryBuilder::<Postgres>::new("SELECT COUNT(*) FROM playlists p");
        push_catalog_conditions(&mut count, user, &query, audio_selected);
        Some(
            count
                .build_query_scalar::<i64>()
                .fetch_one(&state.db)
                .await?,
        )
    } else {
        None
    };
    let direction = if query.sort_order.eq_ignore_ascii_case("Descending")
        || query.sort_order.eq_ignore_ascii_case("Desc")
    {
        "DESC"
    } else {
        "ASC"
    };
    let mut page = QueryBuilder::<Postgres>::new(
        "SELECT p.id,p.name,p.created_at,p.updated_at,p.owner_user_id FROM playlists p",
    );
    push_catalog_conditions(&mut page, user, &query, audio_selected);
    page.push(format!(
        " ORDER BY {sort_column} {direction},p.id {direction} OFFSET "
    ))
    .push_bind(query.start_index)
    .push(" LIMIT ")
    .push_bind(query.limit);
    let records = page
        .build_query_as::<PlaylistRecord>()
        .fetch_all(&state.db)
        .await?;
    Ok(Json(PlaylistCatalogResultDto {
        items: records
            .into_iter()
            .map(|record| record.into_dto(state.server_id, user.id))
            .collect(),
        total_record_count,
        start_index: query.start_index,
    })
    .into_response())
}

fn push_catalog_conditions(
    builder: &mut QueryBuilder<'_, Postgres>,
    user: &UserRecord,
    query: &ItemQuery,
    audio_selected: bool,
) {
    builder.push(" WHERE (p.owner_user_id=").push_bind(user.id)
        .push(" OR EXISTS (SELECT 1 FROM playlist_users share WHERE share.playlist_id=p.id AND share.user_id=")
        .push_bind(user.id).push("))");
    if !audio_selected {
        builder.push(" AND FALSE");
    }
    if let Some(search) = &query.search_term {
        builder
            .push(" AND strpos(lower(p.name),lower(")
            .push_bind(search.clone())
            .push("))>0");
    }
    if let Some(parent_id) = query.parent_id {
        // A music-library view includes playlists with a visible track in that
        // library. Root queries also include accessible empty playlists.
        builder.push(" AND EXISTS (SELECT 1 FROM playlist_items entry JOIN items i ON i.id=entry.item_id JOIN libraries l ON l.id=i.library_id WHERE entry.playlist_id=p.id AND l.enabled=TRUE AND i.item_type='Audio' AND i.path !~ '(^|/)[.]' AND i.library_id=")
            .push_bind(parent_id);
        db::push_user_visibility_filters(builder, user);
        builder.push(")");
    }
}

pub(crate) async fn catalog_item_response(
    state: &AppState,
    current: &UserRecord,
    selected: &UserRecord,
    playlist_id: Uuid,
) -> Result<Option<Response>, ApiError> {
    if current.id != selected.id {
        return Ok(None);
    }
    Ok(sqlx::query_as::<_, PlaylistRecord>(
        "SELECT p.id,p.name,p.created_at,p.updated_at,p.owner_user_id FROM playlists p WHERE p.id=$1 AND (p.owner_user_id=$2 OR EXISTS (SELECT 1 FROM playlist_users share WHERE share.playlist_id=p.id AND share.user_id=$2))",
    )
    .bind(playlist_id)
    .bind(current.id)
    .fetch_optional(&state.db)
    .await?
    .map(|record| Json(record.into_dto(state.server_id, current.id)).into_response()))
}

pub(crate) async fn delete_catalog_playlists(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    RawQuery(raw_query): RawQuery,
) -> Result<StatusCode, ApiError> {
    ensure_self_user(&user, query_uuid(raw_query.as_deref(), "UserId")?)?;
    let ids = query_uuids(raw_query.as_deref(), "Ids")?;
    if ids.is_empty() {
        return Err(ApiError::BadRequest("Ids is required".to_owned()));
    }
    delete_owned_playlists(&state, &user, ids).await
}

async fn delete_owned_playlists(
    state: &AppState,
    user: &UserRecord,
    mut ids: Vec<Uuid>,
) -> Result<StatusCode, ApiError> {
    ids.sort_unstable();
    ids.dedup();
    let mut tx = state.db.begin().await?;
    let deleted = sqlx::query_scalar::<_, Uuid>(
        "DELETE FROM playlists WHERE id=ANY($1) AND owner_user_id=$2 RETURNING id",
    )
    .bind(&ids)
    .bind(user.id)
    .fetch_all(&mut *tx)
    .await?;
    if deleted.len() != ids.len() {
        tx.rollback().await?;
        return Err(ApiError::NotFound);
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
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
    ensure_private_playlist_request(body.is_public)?;
    validate_playlist_users(user.id, &body.users)?;

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
    persist_playlist_users(&mut tx, playlist_id, user.id, &body.users).await?;
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
        sqlx::query_scalar("SELECT COUNT(*) FROM playlists p WHERE p.owner_user_id=$1 OR EXISTS (SELECT 1 FROM playlist_users share WHERE share.playlist_id=p.id AND share.user_id=$1)")
            .bind(user.id)
            .fetch_one(&state.db)
            .await?;
    let records = sqlx::query_as::<_, PlaylistRecord>(
        "SELECT p.id,p.name,p.created_at,p.updated_at,p.owner_user_id FROM playlists p WHERE p.owner_user_id=$1 OR EXISTS (SELECT 1 FROM playlist_users share WHERE share.playlist_id=p.id AND share.user_id=$1) ORDER BY lower(p.name),p.id OFFSET $2 LIMIT $3",
    )
    .bind(user.id)
    .bind(start_index)
    .bind(limit)
    .fetch_all(&state.db)
    .await?;
    let items = records
        .into_iter()
        .map(|record| record.into_dto(state.server_id, user.id))
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
    let access = playlist_access(&state, &user, playlist_id).await?;
    let items = visible_entries(&state, &user, playlist_id).await?;
    Ok(Json(PlaylistDto {
        open_access: false,
        shares: if access.owner_user_id == user.id {
            read_playlist_users(&state, playlist_id).await?
        } else {
            vec![PlaylistUserPermission {
                user_id: user.id,
                can_edit: access.can_edit,
            }]
        },
        item_ids: items.into_iter().map(|entry| entry.item.id).collect(),
    }))
}

async fn update_playlist(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(playlist_id): Path<Uuid>,
    Json(body): Json<UpdatePlaylistRequest>,
) -> Result<StatusCode, ApiError> {
    let access = ensure_editable(&state, &user, playlist_id).await?;
    if (body.users.is_some() || body.is_public.is_some()) && access.owner_user_id != user.id {
        return Err(ApiError::Forbidden);
    }
    ensure_private_playlist_request(body.is_public)?;
    if let Some(users) = &body.users {
        validate_playlist_users(user.id, users)?;
    }
    let name = body
        .name
        .map(|name| validate_name(Some(name)))
        .transpose()?;
    if let Some(ids) = body.ids.as_ref() {
        validate_item_ids(&state, &user, ids).await?;
    }

    let mut tx = state.db.begin().await?;
    let access = lock_editable_playlist(&mut tx, &user, playlist_id).await?;
    if body.ids.is_some() {
        ensure_editable_entries_visible(&state, &mut tx, &user, playlist_id, &access).await?;
    }
    if let Some(users) = &body.users {
        persist_playlist_users(&mut tx, playlist_id, user.id, users).await?;
        sqlx::query("UPDATE playlists SET updated_at=NOW() WHERE id=$1")
            .bind(playlist_id)
            .execute(&mut *tx)
            .await?;
    }
    if let Some(name) = name {
        sqlx::query("UPDATE playlists SET name=$2,updated_at=NOW() WHERE id=$1")
            .bind(playlist_id)
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

pub(crate) async fn delete_playlist(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(playlist_id): Path<Uuid>,
) -> Result<StatusCode, ApiError> {
    delete_owned_playlists(&state, &user, vec![playlist_id]).await
}

async fn get_playlist_items(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(playlist_id): Path<Uuid>,
    Query(query): Query<PageQuery>,
) -> Result<Json<PlaylistItemsResultDto>, ApiError> {
    playlist_access(&state, &user, playlist_id).await?;
    ensure_playback_allowed(&user)?;
    ensure_self_user(&user, query.user_id)?;
    let start_index = query.start_index.unwrap_or(0);
    if start_index < 0 {
        return Err(ApiError::BadRequest(
            "StartIndex cannot be negative".to_owned(),
        ));
    }
    let limit = query
        .limit
        .unwrap_or(MAX_PAGE_SIZE)
        .clamp(1, state.config.max_page_size.min(MAX_PLAYLIST_ITEMS as i64));
    let (items, total_record_count) =
        playlist_item_page(&state, &user, playlist_id, start_index, limit).await?;
    Ok(Json(PlaylistItemsResultDto {
        items,
        total_record_count: as_i32(total_record_count)?,
        start_index: as_i32(start_index)?,
    }))
}

async fn playlist_item_page(
    state: &AppState,
    user: &UserRecord,
    playlist_id: Uuid,
    start_index: i64,
    limit: i64,
) -> Result<(Vec<PlaylistItemDto>, i64), ApiError> {
    let mut entries = visible_entries(state, user, playlist_id).await?;
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
    Ok((items, total_record_count))
}

async fn add_playlist_items(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(playlist_id): Path<Uuid>,
    RawQuery(raw_query): RawQuery,
) -> Result<StatusCode, ApiError> {
    ensure_editable(&state, &user, playlist_id).await?;
    ensure_playback_allowed(&user)?;
    ensure_self_user(&user, query_uuid(raw_query.as_deref(), "UserId")?)?;
    let item_ids = query_uuids(raw_query.as_deref(), "Ids")?;
    if item_ids.is_empty() {
        return Err(ApiError::BadRequest("Ids is required".to_owned()));
    }
    validate_item_ids(&state, &user, &item_ids).await?;
    let requested_position = query_i32(raw_query.as_deref(), "Position")?;

    let mut tx = state.db.begin().await?;
    let access = lock_editable_playlist(&mut tx, &user, playlist_id).await?;
    ensure_editable_entries_visible(&state, &mut tx, &user, playlist_id, &access).await?;
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
    ensure_editable(&state, &user, playlist_id).await?;
    let entry_ids = query_uuids(raw_query.as_deref(), "EntryIds")?;
    if entry_ids.is_empty() {
        return Err(ApiError::BadRequest("EntryIds is required".to_owned()));
    }
    let mut tx = state.db.begin().await?;
    let access = lock_editable_playlist(&mut tx, &user, playlist_id).await?;
    ensure_editable_entries_visible(&state, &mut tx, &user, playlist_id, &access).await?;
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
    ensure_editable(&state, &user, playlist_id).await?;
    if new_index < 0 {
        return Err(ApiError::BadRequest(
            "NewIndex cannot be negative".to_owned(),
        ));
    }
    let mut tx = state.db.begin().await?;
    let access = lock_editable_playlist(&mut tx, &user, playlist_id).await?;
    ensure_editable_entries_visible(&state, &mut tx, &user, playlist_id, &access).await?;
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

#[derive(sqlx::FromRow)]
struct PlaylistAccess {
    owner_user_id: Uuid,
    can_edit: bool,
}

async fn playlist_access(
    state: &AppState,
    user: &UserRecord,
    playlist_id: Uuid,
) -> Result<PlaylistAccess, ApiError> {
    sqlx::query_as::<_, PlaylistAccess>(
        "SELECT p.owner_user_id,(p.owner_user_id=$2 OR COALESCE(share.can_edit,FALSE)) AS can_edit FROM playlists p LEFT JOIN playlist_users share ON share.playlist_id=p.id AND share.user_id=$2 WHERE p.id=$1 AND (p.owner_user_id=$2 OR share.user_id IS NOT NULL)",
    )
    .bind(playlist_id).bind(user.id).fetch_optional(&state.db).await?
    .ok_or(ApiError::NotFound)
}

pub(crate) async fn can_read_playlist(
    state: &AppState,
    user: &UserRecord,
    playlist_id: Uuid,
) -> Result<bool, ApiError> {
    match playlist_access(state, user, playlist_id).await {
        Ok(_) => Ok(true),
        Err(ApiError::NotFound) => Ok(false),
        Err(error) => Err(error),
    }
}

pub(crate) async fn mix_seed_items(
    state: &AppState,
    user: &UserRecord,
    playlist_id: Uuid,
) -> Result<Vec<crate::library::ItemRecord>, ApiError> {
    playlist_access(state, user, playlist_id).await?;
    Ok(visible_entries(state, user, playlist_id)
        .await?
        .into_iter()
        .map(|entry| entry.item)
        .collect())
}

async fn ensure_editable(
    state: &AppState,
    user: &UserRecord,
    playlist_id: Uuid,
) -> Result<PlaylistAccess, ApiError> {
    let access = playlist_access(state, user, playlist_id).await?;
    if !access.can_edit {
        return Err(ApiError::Forbidden);
    }
    Ok(access)
}

async fn lock_editable_playlist(
    tx: &mut Transaction<'_, Postgres>,
    user: &UserRecord,
    playlist_id: Uuid,
) -> Result<PlaylistAccess, ApiError> {
    // Sharing writes take this same row lock. Read permissions after acquiring
    // it so an editor waiting behind a revocation cannot use stale access.
    let owner_user_id =
        sqlx::query_scalar::<_, Uuid>("SELECT owner_user_id FROM playlists WHERE id=$1 FOR UPDATE")
            .bind(playlist_id)
            .fetch_optional(&mut **tx)
            .await?
            .ok_or(ApiError::NotFound)?;
    let can_edit = if owner_user_id == user.id {
        true
    } else {
        sqlx::query_scalar::<_, bool>(
            "SELECT can_edit FROM playlist_users WHERE playlist_id=$1 AND user_id=$2",
        )
        .bind(playlist_id)
        .bind(user.id)
        .fetch_optional(&mut **tx)
        .await?
        .ok_or(ApiError::NotFound)?
    };
    if !can_edit {
        return Err(ApiError::Forbidden);
    }
    Ok(PlaylistAccess {
        owner_user_id,
        can_edit,
    })
}

async fn ensure_editable_entries_visible(
    state: &AppState,
    tx: &mut Transaction<'_, Postgres>,
    user: &UserRecord,
    playlist_id: Uuid,
    access: &PlaylistAccess,
) -> Result<(), ApiError> {
    if access.owner_user_id == user.id {
        return Ok(());
    }
    ensure_playback_allowed(user)?;
    // A recipient sees a filtered queue. Reject whole-queue mutations when
    // that view omits entries rather than deleting or reordering hidden media.
    for entry in read_entries(tx, playlist_id).await? {
        let item = db::get_item(&state.db, entry.item_id)
            .await?
            .ok_or(ApiError::NotFound)?;
        if item.item_type != "Audio" || !db::item_visible_to_user(&state.db, user, &item).await? {
            return Err(ApiError::Forbidden);
        }
    }
    Ok(())
}

fn validate_playlist_users(
    owner_id: Uuid,
    users: &[PlaylistUserPermission],
) -> Result<(), ApiError> {
    if users.len() > MAX_PLAYLIST_USERS {
        return Err(ApiError::BadRequest(
            "Playlist cannot have more than 1000 shared users".to_owned(),
        ));
    }
    let mut seen = std::collections::HashSet::new();
    for permission in users {
        if !seen.insert(permission.user_id) {
            return Err(ApiError::BadRequest(
                "Playlist users must be unique".to_owned(),
            ));
        }
        if permission.user_id == owner_id && !permission.can_edit {
            return Err(ApiError::BadRequest(
                "The playlist owner retains edit access".to_owned(),
            ));
        }
    }
    Ok(())
}

async fn persist_playlist_users(
    tx: &mut Transaction<'_, Postgres>,
    playlist_id: Uuid,
    owner_id: Uuid,
    users: &[PlaylistUserPermission],
) -> Result<(), ApiError> {
    let ids = users
        .iter()
        .filter(|permission| permission.user_id != owner_id)
        .map(|permission| permission.user_id)
        .collect::<Vec<_>>();
    let existing = sqlx::query_scalar::<_, Uuid>(
        "SELECT id FROM users WHERE id=ANY($1) AND disabled=FALSE FOR KEY SHARE",
    )
    .bind(&ids)
    .fetch_all(&mut **tx)
    .await?;
    if existing.len() != ids.len() {
        return Err(ApiError::BadRequest(
            "Unknown or disabled playlist user".to_owned(),
        ));
    }
    sqlx::query("DELETE FROM playlist_users WHERE playlist_id=$1")
        .bind(playlist_id)
        .execute(&mut **tx)
        .await?;
    for permission in users
        .iter()
        .filter(|permission| permission.user_id != owner_id)
    {
        sqlx::query("INSERT INTO playlist_users(playlist_id,user_id,can_edit) VALUES ($1,$2,$3)")
            .bind(playlist_id)
            .bind(permission.user_id)
            .bind(permission.can_edit)
            .execute(&mut **tx)
            .await?;
    }
    Ok(())
}

async fn read_playlist_users(
    state: &AppState,
    playlist_id: Uuid,
) -> Result<Vec<PlaylistUserPermission>, ApiError> {
    Ok(sqlx::query_as::<_, PlaylistUserPermission>(
        "SELECT user_id,can_edit FROM playlist_users WHERE playlist_id=$1 ORDER BY user_id",
    )
    .bind(playlist_id)
    .fetch_all(&state.db)
    .await?)
}

async fn get_playlist_users(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(playlist_id): Path<Uuid>,
) -> Result<Json<Vec<PlaylistUserPermission>>, ApiError> {
    let access = playlist_access(&state, &user, playlist_id).await?;
    if access.owner_user_id != user.id {
        return Err(ApiError::Forbidden);
    }
    Ok(Json(read_playlist_users(&state, playlist_id).await?))
}

#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct UpdatePlaylistUserRequest {
    #[serde(default, rename = "CanEdit", alias = "canEdit")]
    can_edit: Option<bool>,
}

async fn update_playlist_user(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((playlist_id, user_id)): Path<(Uuid, Uuid)>,
    Json(body): Json<UpdatePlaylistUserRequest>,
) -> Result<StatusCode, ApiError> {
    let access = playlist_access(&state, &user, playlist_id).await?;
    if access.owner_user_id != user.id {
        return Err(ApiError::Forbidden);
    }
    let mut tx = state.db.begin().await?;
    lock_owned_playlist(&mut tx, &user, playlist_id).await?;
    let mut shares = sqlx::query_as::<_, PlaylistUserPermission>(
        "SELECT user_id,can_edit FROM playlist_users WHERE playlist_id=$1 ORDER BY user_id",
    )
    .bind(playlist_id)
    .fetch_all(&mut *tx)
    .await?;
    if let Some(can_edit) = body.can_edit {
        if user_id == user.id {
            if !can_edit {
                return Err(ApiError::BadRequest(
                    "The playlist owner retains edit access".to_owned(),
                ));
            }
        } else if let Some(share) = shares.iter_mut().find(|share| share.user_id == user_id) {
            share.can_edit = can_edit;
        } else {
            shares.push(PlaylistUserPermission { user_id, can_edit });
        }
        validate_playlist_users(user.id, &shares)?;
        persist_playlist_users(&mut tx, playlist_id, user.id, &shares).await?;
        sqlx::query("UPDATE playlists SET updated_at=NOW() WHERE id=$1")
            .bind(playlist_id)
            .execute(&mut *tx)
            .await?;
    } else if user_id != user.id && !shares.iter().any(|share| share.user_id == user_id) {
        return Err(ApiError::NotFound);
    }
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn remove_playlist_user(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path((playlist_id, user_id)): Path<(Uuid, Uuid)>,
) -> Result<StatusCode, ApiError> {
    let access = playlist_access(&state, &user, playlist_id).await?;
    if access.owner_user_id != user.id || user_id == user.id {
        return Err(ApiError::Forbidden);
    }
    let mut tx = state.db.begin().await?;
    lock_owned_playlist(&mut tx, &user, playlist_id).await?;
    sqlx::query("DELETE FROM playlist_users WHERE playlist_id=$1 AND user_id=$2")
        .bind(playlist_id)
        .bind(user_id)
        .execute(&mut *tx)
        .await?;
    sqlx::query("UPDATE playlists SET updated_at=NOW() WHERE id=$1")
        .bind(playlist_id)
        .execute(&mut *tx)
        .await?;
    tx.commit().await?;
    Ok(StatusCode::NO_CONTENT)
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

fn ensure_private_playlist_request(is_public: Option<bool>) -> Result<(), ApiError> {
    if is_public == Some(true) {
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
