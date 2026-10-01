use std::{collections::HashSet, path::PathBuf};

use axum::{
    Json, Router,
    extract::{Path, Query, State},
    routing::get,
};
use serde::{Deserialize, Serialize};
use uuid::Uuid;

use crate::{
    api::{self, BaseItemDto, LibraryViewDto},
    auth::{CurrentUser, UserRecord},
    db,
    error::ApiError,
    library::ItemRecord,
    state::AppState,
};

pub(crate) fn router(state: AppState) -> Router {
    Router::new()
        .route("/Items/{item_id}/Ancestors", get(ancestors))
        .route("/Items/{item_id}/ThemeMedia", get(theme_media))
        .with_state(state)
}

#[derive(Default, Deserialize)]
struct NavigationQuery {
    #[serde(rename = "userId", alias = "UserId")]
    user_id: Option<Uuid>,
    #[serde(default, rename = "inheritFromParent", alias = "InheritFromParent")]
    inherit_from_parent: bool,
    #[serde(rename = "sortBy", alias = "SortBy")]
    sort_by: Option<String>,
    #[serde(rename = "sortOrder", alias = "SortOrder")]
    sort_order: Option<String>,
}

#[derive(Serialize)]
#[serde(untagged)]
enum CatalogEntry {
    Item(Box<BaseItemDto>),
    Library(LibraryViewDto),
}

async fn visible_item(
    state: &AppState,
    user: &UserRecord,
    id: Uuid,
) -> Result<ItemRecord, ApiError> {
    let item = db::get_item(&state.db, id)
        .await?
        .ok_or(ApiError::NotFound)?;
    if !db::item_visible_to_user(&state.db, user, &item).await? {
        return Err(ApiError::NotFound);
    }
    Ok(item)
}

async fn parent_chain(state: &AppState, item: &ItemRecord) -> Result<Vec<ItemRecord>, ApiError> {
    let parents = db::item_ancestors(&state.db, item).await?;
    if parents
        .last()
        .map_or(item.parent_id, |parent| parent.parent_id)
        .is_some()
    {
        return Err(ApiError::Conflict(
            "Catalog parent chain is incomplete or cyclic".to_owned(),
        ));
    }
    Ok(parents)
}

async fn ancestors(
    State(state): State<AppState>,
    CurrentUser(current): CurrentUser,
    Path(id): Path<Uuid>,
    Query(query): Query<NavigationQuery>,
) -> Result<Json<Vec<CatalogEntry>>, ApiError> {
    let user = api::selected_user(&state, &current, query.user_id).await?;
    if let Some(library) = db::get_library(&state.db, id).await? {
        if !db::library_visible_to_user(&state.db, &user, library.id).await? {
            return Err(ApiError::NotFound);
        }
        return Ok(Json(Vec::new()));
    }
    let item = visible_item(&state, &user, id).await?;
    let parents = parent_chain(&state, &item).await?;
    let mut entries = Vec::with_capacity(parents.len() + 1);
    for parent in parents {
        if db::item_visible_to_user(&state.db, &user, &parent).await? {
            entries.push(CatalogEntry::Item(Box::new(
                api::item_dto_for_user(&state, &user, &parent).await?,
            )));
        }
    }
    if let Some(library) = db::get_library(&state.db, item.library_id).await?
        && db::library_visible_to_user(&state.db, &user, library.id).await?
    {
        entries.push(CatalogEntry::Library(api::library_view_dto(
            &library,
            state.server_id,
        )));
    }
    Ok(Json(entries))
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct ThemeResult {
    items: Vec<BaseItemDto>,
    total_record_count: usize,
    start_index: usize,
    owner_id: Uuid,
}

impl ThemeResult {
    fn empty(owner_id: Uuid) -> Self {
        Self {
            items: Vec::new(),
            total_record_count: 0,
            start_index: 0,
            owner_id,
        }
    }
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct ThemeMediaResult {
    theme_songs_result: ThemeResult,
    theme_videos_result: ThemeResult,
    soundtrack_songs_result: ThemeResult,
}

fn theme_directory(item: &ItemRecord) -> Option<PathBuf> {
    if !item.path.is_absolute() {
        return None;
    }
    if matches!(
        item.item_type.as_str(),
        "Folder" | "Series" | "Season" | "MusicAlbum" | "MusicArtist" | "BoxSet"
    ) {
        Some(item.path.clone())
    } else {
        item.path.parent().map(PathBuf::from)
    }
}

fn sort_options(query: &NavigationQuery) -> Result<(db::ThemeMediaSort, bool), ApiError> {
    let sort_by = query.sort_by.as_deref().unwrap_or("SortName");
    let sort_order = query.sort_order.as_deref().unwrap_or("Ascending");
    if !["SortName", "Name", "Random"]
        .iter()
        .any(|allowed| sort_by.eq_ignore_ascii_case(allowed))
        || !["Ascending", "Descending"]
            .iter()
            .any(|allowed| sort_order.eq_ignore_ascii_case(allowed))
    {
        return Err(ApiError::BadRequest(
            "Unsupported theme media sort order".to_owned(),
        ));
    }
    Ok((
        if sort_by.eq_ignore_ascii_case("Name") {
            db::ThemeMediaSort::Name
        } else if sort_by.eq_ignore_ascii_case("Random") {
            db::ThemeMediaSort::Random
        } else {
            db::ThemeMediaSort::SortName
        },
        sort_order.eq_ignore_ascii_case("Descending"),
    ))
}

async fn theme_media(
    State(state): State<AppState>,
    CurrentUser(current): CurrentUser,
    Path(id): Path<Uuid>,
    Query(query): Query<NavigationQuery>,
) -> Result<Json<ThemeMediaResult>, ApiError> {
    let user = api::selected_user(&state, &current, query.user_id).await?;
    let (sort, descending) = sort_options(&query)?;
    let mut result = ThemeMediaResult {
        theme_songs_result: ThemeResult::empty(id),
        theme_videos_result: ThemeResult::empty(id),
        soundtrack_songs_result: ThemeResult::empty(id),
    };
    if let Some(library) = db::get_library(&state.db, id).await? {
        if !db::library_visible_to_user(&state.db, &user, library.id).await? {
            return Err(ApiError::NotFound);
        }
        return Ok(Json(result));
    }
    let item = visible_item(&state, &user, id).await?;
    let parents = if query.inherit_from_parent {
        parent_chain(&state, &item).await?
    } else {
        Vec::new()
    };
    let mut directories = HashSet::new();
    for owner in std::iter::once(item).chain(parents) {
        if !db::item_visible_to_user(&state.db, &user, &owner).await? {
            continue;
        }
        let Some(directory) =
            theme_directory(&owner).filter(|path| directories.insert(path.clone()))
        else {
            continue;
        };
        let themes = db::theme_media_items(
            &state.db,
            &user,
            owner.library_id,
            &directory,
            sort,
            descending,
        )
        .await?;
        if themes.len() > 256 {
            return Err(ApiError::Conflict(
                "Theme media exceeds the supported 256-item limit".to_owned(),
            ));
        }
        let mut songs = Vec::new();
        let mut videos = Vec::new();
        let mut soundtracks = Vec::new();
        for theme in themes {
            let soundtrack = theme.item_type == "Audio"
                && theme
                    .path
                    .parent()
                    .is_some_and(|parent| parent.ends_with("soundtracks"));
            let audio = theme.item_type == "Audio";
            let dto = api::item_dto_for_user(&state, &user, &theme).await?;
            if soundtrack {
                soundtracks.push(dto);
            } else if audio {
                songs.push(dto);
            } else {
                videos.push(dto);
            }
        }
        for (destination, items) in [
            (&mut result.theme_songs_result, songs),
            (&mut result.theme_videos_result, videos),
            (&mut result.soundtrack_songs_result, soundtracks),
        ] {
            if destination.items.is_empty() && !items.is_empty() {
                destination.owner_id = owner.id;
                destination.total_record_count = items.len();
                destination.items = items;
            }
        }
        if !query.inherit_from_parent
            || (!result.theme_songs_result.items.is_empty()
                && !result.theme_videos_result.items.is_empty()
                && !result.soundtrack_songs_result.items.is_empty())
        {
            break;
        }
    }
    Ok(Json(result))
}
