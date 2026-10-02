//! Music queues generated from the caller's visible catalogue.

use axum::{
    Json, Router,
    extract::{Path, Query, State},
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
        .route("/Items/{item_id}/InstantMix", get(item_mix))
        .route("/Songs/{item_id}/InstantMix", get(song_mix))
        .route("/Albums/{item_id}/InstantMix", get(album_mix))
        .route("/Artists/{item_id}/InstantMix", get(artist_mix))
        .route("/Playlists/{item_id}/InstantMix", get(playlist_mix))
        .route("/MusicGenres/{name}/InstantMix", get(genre_name_mix))
        .route("/MusicGenres/InstantMix", get(genre_id_mix))
        .with_state(state)
}

#[derive(Default, Deserialize)]
struct MixQuery {
    #[serde(rename = "userId", alias = "UserId")]
    user_id: Option<Uuid>,
    #[serde(rename = "id", alias = "Id")]
    genre_id: Option<Uuid>,
    #[serde(rename = "limit", alias = "Limit")]
    limit: Option<i64>,
    #[serde(rename = "fields", alias = "Fields")]
    fields: Option<String>,
    #[serde(rename = "enableImages", alias = "EnableImages")]
    enable_images: Option<bool>,
    #[serde(rename = "enableUserData", alias = "EnableUserData")]
    enable_user_data: Option<bool>,
    #[serde(rename = "imageTypeLimit", alias = "ImageTypeLimit")]
    image_type_limit: Option<i64>,
    #[serde(rename = "enableImageTypes", alias = "EnableImageTypes")]
    enable_image_types: Option<String>,
}

impl MixQuery {
    fn options(&self, maximum: i64) -> Result<(usize, bool, bool), ApiError> {
        if self
            .limit
            .is_some_and(|limit| !(0..=i64::from(i32::MAX)).contains(&limit))
            || self
                .image_type_limit
                .is_some_and(|limit| !(0..=i64::from(i32::MAX)).contains(&limit))
            || self
                .fields
                .as_ref()
                .is_some_and(|fields| fields.len() > 4096 || fields.chars().any(char::is_control))
        {
            return Err(ApiError::BadRequest(
                "Invalid Instant Mix options".to_owned(),
            ));
        }
        let image_types = self.enable_image_types.as_deref().unwrap_or("Primary");
        if image_types.len() > 512
            || image_types
                .split(',')
                .filter(|kind| !kind.trim().is_empty())
                .any(|kind| {
                    ![
                        "Primary",
                        "Art",
                        "Backdrop",
                        "Banner",
                        "Box",
                        "BoxRear",
                        "Chapter",
                        "Disc",
                        "Logo",
                        "Menu",
                        "Profile",
                        "Screenshot",
                        "Thumb",
                    ]
                    .contains(&kind.trim())
                })
        {
            return Err(ApiError::BadRequest(
                "Invalid Instant Mix image types".to_owned(),
            ));
        }
        Ok((
            self.limit.unwrap_or(20).min(maximum.clamp(0, 100)) as usize,
            self.enable_images.unwrap_or(true)
                && self.image_type_limit != Some(0)
                && image_types.split(',').any(|kind| kind.trim() == "Primary"),
            self.enable_user_data.unwrap_or(true),
        ))
    }
}

#[derive(Serialize)]
#[serde(rename_all = "PascalCase")]
struct MixResult {
    items: Vec<api::BaseItemDto>,
    total_record_count: i64,
    start_index: usize,
}

async fn response(
    state: &AppState,
    user: &UserRecord,
    query: &MixQuery,
    seeds: &[Uuid],
    source: Option<Uuid>,
    genre: Option<String>,
    playlist: Option<Uuid>,
) -> Result<Json<MixResult>, ApiError> {
    let (limit, images, user_data) = query.options(state.config.max_page_size)?;
    if seeds.len() > 1000 {
        return Err(ApiError::Conflict(
            "Instant Mix seed queue exceeds 1000 tracks".to_owned(),
        ));
    }
    let (items, total) = db::instant_mix(
        &state.db,
        user,
        db::MusicMixSeed {
            tracks: seeds,
            item: source,
            genre: genre.as_deref(),
            playlist,
        },
        limit,
    )
    .await?;
    let mut result = MixResult {
        items: Vec::with_capacity(items.len()),
        total_record_count: total,
        start_index: 0,
    };
    for item in items {
        let mut dto = api::item_dto_for_user(state, user, &item).await?;
        dto.include_images_and_user_data(images, user_data);
        result.items.push(dto);
    }
    Ok(Json(result))
}

async fn from_item(
    state: AppState,
    current: UserRecord,
    id: Uuid,
    query: MixQuery,
    expected_type: Option<&str>,
) -> Result<Json<MixResult>, ApiError> {
    let user = api::selected_user(&state, &current, query.user_id).await?;
    query.options(state.config.max_page_size)?;
    let item = db::get_item(&state.db, id).await?;
    if expected_type == Some("Playlist") || (expected_type.is_none() && item.is_none()) {
        if current.id != user.id {
            return Err(ApiError::NotFound);
        }
        let tracks = crate::playlists::mix_seed_items(&state, &user, id).await?;
        let seeds = tracks.iter().map(|track| track.id).collect::<Vec<_>>();
        return response(&state, &user, &query, &seeds, None, None, Some(id)).await;
    }
    let item = item.ok_or(ApiError::NotFound)?;
    if !db::item_visible_to_user(&state.db, &user, &item).await?
        || expected_type.is_some_and(|kind| kind != item.item_type)
    {
        return Err(ApiError::NotFound);
    }
    let seeds = match item.item_type.as_str() {
        "Audio" => vec![item.id],
        "MusicAlbum" | "MusicArtist" => db::browse_items(
            &state.db,
            &user,
            ItemQuery {
                parent_id: Some(item.id),
                recursive: true,
                include_item_types: vec!["Audio".to_owned()],
                limit: 32,
                sort_by: "SortName".to_owned(),
                sort_order: "Ascending".to_owned(),
                ..Default::default()
            },
        )
        .await?
        .0
        .into_iter()
        .map(|track| track.id)
        .collect(),
        _ => {
            return Err(ApiError::BadRequest(
                "Instant Mix requires a song, album, artist, playlist, or music genre".to_owned(),
            ));
        }
    };
    response(&state, &user, &query, &seeds, Some(item.id), None, None).await
}

async fn item_mix(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
    Query(query): Query<MixQuery>,
) -> Result<Json<MixResult>, ApiError> {
    from_item(state, user, id, query, None).await
}
async fn song_mix(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
    Query(query): Query<MixQuery>,
) -> Result<Json<MixResult>, ApiError> {
    from_item(state, user, id, query, Some("Audio")).await
}
async fn album_mix(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
    Query(query): Query<MixQuery>,
) -> Result<Json<MixResult>, ApiError> {
    from_item(state, user, id, query, Some("MusicAlbum")).await
}
async fn artist_mix(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
    Query(query): Query<MixQuery>,
) -> Result<Json<MixResult>, ApiError> {
    from_item(state, user, id, query, Some("MusicArtist")).await
}
async fn playlist_mix(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    Path(id): Path<Uuid>,
    Query(query): Query<MixQuery>,
) -> Result<Json<MixResult>, ApiError> {
    from_item(state, user, id, query, Some("Playlist")).await
}
async fn genre_name_mix(
    State(state): State<AppState>,
    CurrentUser(current): CurrentUser,
    Path(name): Path<String>,
    Query(query): Query<MixQuery>,
) -> Result<Json<MixResult>, ApiError> {
    let user = api::selected_user(&state, &current, query.user_id).await?;
    query.options(state.config.max_page_size)?;
    if name.trim().is_empty() || name.len() > 128 || name.chars().any(char::is_control) {
        return Err(ApiError::BadRequest("Invalid music genre name".to_owned()));
    }
    let genre = db::visible_music_genre(&state.db, &user, Some(name.trim()), None)
        .await?
        .ok_or(ApiError::NotFound)?;
    response(&state, &user, &query, &[], None, Some(genre), None).await
}
async fn genre_id_mix(
    State(state): State<AppState>,
    CurrentUser(current): CurrentUser,
    Query(query): Query<MixQuery>,
) -> Result<Json<MixResult>, ApiError> {
    let user = api::selected_user(&state, &current, query.user_id).await?;
    query.options(state.config.max_page_size)?;
    let id = query
        .genre_id
        .ok_or_else(|| ApiError::BadRequest("A music genre id is required".to_owned()))?;
    let genre = db::visible_music_genre(&state.db, &user, None, Some(id))
        .await?
        .ok_or(ApiError::NotFound)?;
    response(&state, &user, &query, &[], None, Some(genre), None).await
}
