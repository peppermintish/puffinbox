use axum::{
    Json, Router,
    extract::{Path, Query, State},
    response::{IntoResponse, Response},
    routing::get,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    ApiError, AppState, api,
    auth::{CurrentUser, UserRecord},
    db,
    library::ItemQuery,
};

pub(crate) fn router(state: AppState) -> Router {
    Router::new()
        .route("/Studios", get(list))
        .route("/Studios/{name}", get(detail))
        .with_state(state)
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct StudioQuery {
    #[serde(alias = "userId")]
    user_id: Option<Uuid>,
    #[serde(alias = "parentId")]
    parent_id: Option<Uuid>,
    #[serde(alias = "startIndex")]
    start_index: Option<i64>,
    #[serde(alias = "limit")]
    limit: Option<i64>,
    #[serde(alias = "includeItemTypes")]
    include_item_types: Option<String>,
    #[serde(alias = "excludeItemTypes")]
    exclude_item_types: Option<String>,
    #[serde(alias = "searchTerm")]
    search_term: Option<String>,
    #[serde(alias = "nameStartsWith")]
    name_starts_with: Option<String>,
    #[serde(alias = "nameStartsWithOrGreater")]
    name_starts_with_or_greater: Option<String>,
    #[serde(alias = "nameLessThan")]
    name_less_than: Option<String>,
    #[serde(alias = "isFavorite")]
    is_favorite: Option<bool>,
    #[serde(alias = "enableUserData")]
    enable_user_data: Option<bool>,
    #[serde(alias = "enableImages")]
    enable_images: Option<bool>,
    #[serde(alias = "enableTotalRecordCount")]
    enable_total_record_count: Option<bool>,
}

fn text(value: Option<String>) -> Result<Option<String>, ApiError> {
    value
        .map(|value| {
            let value = value.trim();
            if value.len() > 512 || value.chars().any(char::is_control) {
                return Err(ApiError::BadRequest(
                    "Studio name filters accept at most 512 bytes without control characters"
                        .to_owned(),
                ));
            }
            Ok(value.to_owned())
        })
        .transpose()
}

fn item_types(values: Vec<String>) -> Result<Vec<String>, ApiError> {
    // Public Jellyfin 12 BaseItemKind names, independent of which media kinds
    // are currently indexed by Puffinbox.
    const KINDS: &str = "AggregateFolder,Audio,AudioBook,BasePluginFolder,Book,BoxSet,Channel,ChannelFolderItem,CollectionFolder,Episode,Folder,Genre,ManualPlaylistsFolder,Movie,LiveTvChannel,LiveTvProgram,MusicAlbum,MusicArtist,MusicGenre,MusicVideo,Person,Photo,PhotoAlbum,Playlist,PlaylistsFolder,Program,Recording,Season,Series,Studio,Trailer,TvChannel,TvProgram,UserRootFolder,UserView,Video,Year";
    values
        .into_iter()
        .map(|value| {
            KINDS
                .split(',')
                .find(|kind| kind.eq_ignore_ascii_case(&value))
                .map(str::to_owned)
                .ok_or_else(|| {
                    ApiError::BadRequest(
                        "Studio item type filters must use Jellyfin BaseItemKind values".to_owned(),
                    )
                })
        })
        .collect()
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct StudioDto {
    id: Uuid,
    server_id: Uuid,
    name: String,
    #[serde(rename = "Type")]
    item_type: &'static str,
    media_type: &'static str,
    child_count: i64,
    movie_count: i64,
    series_count: i64,
    episode_count: i64,
    song_count: i64,
    album_count: i64,
    music_video_count: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    user_data: Option<api::UserDataDto>,
    #[serde(skip_serializing_if = "Option::is_none")]
    image_tags: Option<serde_json::Value>,
}

fn dto(state: &AppState, studio: db::StudioRecord, user_data: bool, images: bool) -> StudioDto {
    StudioDto {
        id: studio.id,
        server_id: state.server_id,
        name: studio.name,
        item_type: "Studio",
        media_type: "Unknown",
        child_count: studio.child_count,
        movie_count: studio.movie_count,
        series_count: studio.series_count,
        episode_count: studio.episode_count,
        song_count: studio.song_count,
        album_count: studio.album_count,
        music_video_count: studio.music_video_count,
        user_data: user_data.then(|| api::studio_user_data(studio.id, studio.is_favorite)),
        image_tags: images.then(|| serde_json::json!({})),
    }
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct StudioPage {
    items: Vec<StudioDto>,
    total_record_count: i64,
    start_index: i64,
}

async fn list(
    State(state): State<AppState>,
    CurrentUser(current): CurrentUser,
    Query(query): Query<StudioQuery>,
) -> Result<Json<StudioPage>, ApiError> {
    let user = api::selected_user(&state, &current, query.user_id).await?;
    api::ensure_parent_visible(&state, &user, query.parent_id).await?;
    let mut scope = api::item_query(
        api::ItemsQueryParams::facet_scope(query.parent_id, query.include_item_types, None, true),
        &state,
    )?;
    scope.include_item_types = item_types(scope.include_item_types)?;
    scope.start_index = query.start_index.unwrap_or(0);
    scope.limit = query.limit.unwrap_or(scope.limit);
    let maximum = state.config.max_page_size.min(100);
    if scope.start_index < 0 || !(0..=maximum).contains(&scope.limit) {
        return Err(ApiError::BadRequest(format!(
            "Studio paging requires StartIndex >= 0 and Limit between 0 and {maximum}"
        )));
    }
    let exclude = item_types(
        api::item_query(
            api::ItemsQueryParams::facet_scope(None, query.exclude_item_types, None, true),
            &state,
        )?
        .include_item_types,
    )?;
    let selection = db::StudioSelection {
        search: text(query.search_term)?,
        starts_with: text(query.name_starts_with)?,
        greater: text(query.name_starts_with_or_greater)?,
        less: text(query.name_less_than)?,
        exclude_types: exclude,
        favorite: query.is_favorite,
        ..Default::default()
    };
    let (items, total) = db::studio_page(&state.db, &user, &scope, &selection).await?;
    Ok(Json(StudioPage {
        items: items
            .into_iter()
            .map(|studio| {
                dto(
                    &state,
                    studio,
                    query.enable_user_data.unwrap_or(true),
                    query.enable_images.unwrap_or(true),
                )
            })
            .collect(),
        total_record_count: if query.enable_total_record_count.unwrap_or(true) {
            total
        } else {
            0
        },
        start_index: scope.start_index,
    }))
}

async fn one(
    state: &AppState,
    user: &UserRecord,
    selection: db::StudioSelection,
) -> Result<Option<db::StudioRecord>, ApiError> {
    let scope = ItemQuery {
        recursive: true,
        limit: 1,
        ..Default::default()
    };
    Ok(db::studio_page(&state.db, user, &scope, &selection)
        .await?
        .0
        .into_iter()
        .next())
}

async fn detail(
    State(state): State<AppState>,
    CurrentUser(current): CurrentUser,
    Path(name): Path<String>,
    Query(query): Query<StudioQuery>,
) -> Result<Json<StudioDto>, ApiError> {
    let user = api::selected_user(&state, &current, query.user_id).await?;
    let studio = one(
        &state,
        &user,
        db::StudioSelection {
            name: text(Some(name))?,
            ..Default::default()
        },
    )
    .await?
    .ok_or(ApiError::NotFound)?;
    Ok(Json(dto(&state, studio, true, true)))
}

pub(crate) async fn item_response(
    state: &AppState,
    user: &UserRecord,
    id: Uuid,
) -> Result<Option<Response>, ApiError> {
    Ok(one(
        state,
        user,
        db::StudioSelection {
            id: Some(id),
            ..Default::default()
        },
    )
    .await?
    .map(|studio| Json(dto(state, studio, true, true)).into_response()))
}

pub(crate) async fn user_data(
    state: &AppState,
    user: &UserRecord,
    id: Uuid,
) -> Result<Option<api::UserDataDto>, ApiError> {
    Ok(one(
        state,
        user,
        db::StudioSelection {
            id: Some(id),
            ..Default::default()
        },
    )
    .await?
    .map(|studio| api::studio_user_data(id, studio.is_favorite)))
}

pub(crate) async fn favorite(
    state: &AppState,
    user: &UserRecord,
    id: Uuid,
    value: bool,
) -> Result<api::UserDataDto, ApiError> {
    one(
        state,
        user,
        db::StudioSelection {
            id: Some(id),
            ..Default::default()
        },
    )
    .await?
    .ok_or(ApiError::NotFound)?;
    db::set_studio_favorite(&state.db, state.run_id, user.id, id, value).await?;
    state.user_events.publish(user.id, id);
    Ok(api::studio_user_data(id, value))
}
