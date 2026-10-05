use axum::{
    Json,
    extract::{State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
};
use serde::Deserialize;
use uuid::Uuid;

use super::{
    Action, execute,
    queue::{Control, QueueMode, Repeat, Report, Shuffle},
    token_hash,
};
use crate::{ApiError, AppState, auth::CurrentUser};

async fn run(
    state: AppState,
    user: CurrentUser,
    headers: HeaderMap,
    control: Control,
) -> Result<StatusCode, ApiError> {
    execute(
        &state,
        &token_hash(&headers)?,
        user.0.id,
        Action::Control(control),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

fn body<T>(body: Result<Json<T>, JsonRejection>) -> Result<T, ApiError> {
    body.map(|Json(body)| body)
        .map_err(|_| ApiError::BadRequest("Invalid SyncPlay request".to_owned()))
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub(super) struct Play {
    #[serde(alias = "playingQueue")]
    playing_queue: Option<Vec<Uuid>>,
    #[serde(alias = "playingItemPosition")]
    playing_item_position: i32,
    #[serde(alias = "startPositionTicks")]
    start_position_ticks: i64,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub(super) struct Queue {
    #[serde(alias = "itemIds")]
    item_ids: Option<Vec<Uuid>>,
    #[serde(alias = "mode")]
    mode: QueueMode,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub(super) struct Selection {
    #[serde(alias = "playlistItemId")]
    playlist_item_id: Uuid,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub(super) struct Move {
    #[serde(alias = "playlistItemId")]
    playlist_item_id: Uuid,
    #[serde(alias = "newIndex")]
    new_index: i32,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub(super) struct Remove {
    #[serde(alias = "playlistItemIds")]
    playlist_item_ids: Option<Vec<Uuid>>,
    #[serde(alias = "clearPlaylist")]
    clear_playlist: bool,
    #[serde(alias = "clearPlayingItem")]
    clear_playing_item: bool,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub(super) struct RepeatRequest {
    #[serde(alias = "mode")]
    mode: Repeat,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub(super) struct ShuffleRequest {
    #[serde(alias = "mode")]
    mode: Shuffle,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub(super) struct Seek {
    #[serde(alias = "positionTicks")]
    position_ticks: i64,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub(super) struct Ignore {
    #[serde(alias = "ignoreWait")]
    ignore_wait: bool,
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub(super) struct Ping {
    #[serde(alias = "ping")]
    ping: i64,
}

macro_rules! request {
    ($name:ident, $body:ty, $value:ident, $control:expr) => {
        pub(super) async fn $name(
            State(state): State<AppState>,
            user: CurrentUser,
            headers: HeaderMap,
            input: Result<Json<$body>, JsonRejection>,
        ) -> Result<StatusCode, ApiError> {
            let $value = body(input)?;
            run(state, user, headers, $control).await
        }
    };
}

request!(
    set_queue,
    Play,
    b,
    Control::Set(
        b.playing_queue.unwrap_or_default(),
        b.playing_item_position,
        b.start_position_ticks
    )
);
request!(
    queue,
    Queue,
    b,
    Control::Queue(b.item_ids.unwrap_or_default(), b.mode)
);
request!(select, Selection, b, Control::Select(b.playlist_item_id));
request!(next, Selection, b, Control::Next(b.playlist_item_id));
request!(
    previous,
    Selection,
    b,
    Control::Previous(b.playlist_item_id)
);
request!(
    move_item,
    Move,
    b,
    Control::Move(b.playlist_item_id, b.new_index)
);
request!(
    remove,
    Remove,
    b,
    Control::Remove(
        b.playlist_item_ids.unwrap_or_default(),
        b.clear_playlist,
        b.clear_playing_item
    )
);
request!(repeat, RepeatRequest, b, Control::Repeat(b.mode));
request!(shuffle, ShuffleRequest, b, Control::Shuffle(b.mode));
request!(seek, Seek, b, Control::Seek(b.position_ticks));
request!(ignore, Ignore, b, Control::Ignore(b.ignore_wait));
request!(ping, Ping, b, Control::Ping(b.ping));
request!(ready, Report, b, Control::Ready(b));
request!(buffer, Report, b, Control::Buffer(b));

macro_rules! empty {
    ($name:ident, $control:expr) => {
        pub(super) async fn $name(
            State(state): State<AppState>,
            user: CurrentUser,
            headers: HeaderMap,
        ) -> Result<StatusCode, ApiError> {
            run(state, user, headers, $control).await
        }
    };
}
empty!(pause, Control::Pause);
empty!(unpause, Control::Unpause);
empty!(stop, Control::Stop);
