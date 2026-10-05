//! Session-bound SyncPlay groups and their public socket notifications.

use std::{collections::HashMap, sync::atomic::Ordering};

use axum::{
    Json, Router,
    extract::{Path, State, rejection::JsonRejection},
    http::{HeaderMap, StatusCode},
    routing::{get, post},
};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use sqlx::Row;
use tokio::sync::{Mutex, broadcast};
use uuid::Uuid;

use crate::{
    ApiError, AppState,
    auth::{self, CurrentUser, SyncPlayAccess},
    db,
};

mod queue;
mod requests;
mod visibility;

use queue::{Control, Playback, PlaybackState};

const MAX_GROUPS: usize = 128;
const MAX_MEMBERS: usize = 32;
const MAX_GROUPS_PER_USER: usize = 4;

pub(crate) fn router(state: AppState) -> Router {
    Router::new()
        .route("/SyncPlay/List", get(list))
        .route("/SyncPlay/{group_id}", get(detail))
        .route("/SyncPlay/New", post(new))
        .route("/SyncPlay/Join", post(join))
        .route("/SyncPlay/Leave", post(leave))
        .route("/SyncPlay/SetNewQueue", post(requests::set_queue))
        .route("/SyncPlay/Queue", post(requests::queue))
        .route("/SyncPlay/MovePlaylistItem", post(requests::move_item))
        .route("/SyncPlay/RemoveFromPlaylist", post(requests::remove))
        .route("/SyncPlay/SetPlaylistItem", post(requests::select))
        .route("/SyncPlay/NextItem", post(requests::next))
        .route("/SyncPlay/PreviousItem", post(requests::previous))
        .route("/SyncPlay/SetRepeatMode", post(requests::repeat))
        .route("/SyncPlay/SetShuffleMode", post(requests::shuffle))
        .route("/SyncPlay/Pause", post(requests::pause))
        .route("/SyncPlay/Unpause", post(requests::unpause))
        .route("/SyncPlay/Stop", post(requests::stop))
        .route("/SyncPlay/Seek", post(requests::seek))
        .route("/SyncPlay/Ready", post(requests::ready))
        .route("/SyncPlay/Buffering", post(requests::buffer))
        .route("/SyncPlay/SetIgnoreWait", post(requests::ignore))
        .route("/SyncPlay/Ping", post(requests::ping))
        .with_state(state)
}

pub(crate) struct SyncGroups {
    registry: Mutex<Registry>,
    events: broadcast::Sender<GroupEvent>,
    connections: std::sync::Mutex<HashMap<Uuid, usize>>,
}

impl SyncGroups {
    pub(crate) fn new() -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            registry: Mutex::new(Registry::default()),
            events,
            connections: std::sync::Mutex::new(HashMap::new()),
        }
    }

    pub(crate) fn enter(
        self: &std::sync::Arc<Self>,
        session_id: Uuid,
    ) -> Result<Connection, ApiError> {
        let mut connections = self.connections.lock().map_err(|_| ApiError::Unavailable)?;
        *connections.entry(session_id).or_default() += 1;
        Ok(Connection {
            groups: self.clone(),
            session_id,
        })
    }

    fn connected(&self, session_id: Uuid) -> Result<bool, ApiError> {
        Ok(self
            .connections
            .lock()
            .map_err(|_| ApiError::Unavailable)?
            .get(&session_id)
            .is_some_and(|count| *count > 0))
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<GroupEvent> {
        self.events.subscribe()
    }
}

pub(crate) struct Connection {
    groups: std::sync::Arc<SyncGroups>,
    session_id: Uuid,
}

impl Drop for Connection {
    fn drop(&mut self) {
        if let Ok(mut connections) = self.groups.connections.lock()
            && let Some(count) = connections.get_mut(&self.session_id)
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                connections.remove(&self.session_id);
            }
        }
    }
}

#[derive(Clone)]
pub(crate) struct GroupEvent {
    pub(crate) session_id: Uuid,
    group_id: Uuid,
    message_type: &'static str,
    kind: &'static str,
    data: Value,
    requires_membership: bool,
    revision: Option<EventRevision>,
}

#[derive(Clone, Copy)]
enum EventRevision {
    Queue(u64),
    Command(u64),
}

impl EventRevision {
    fn current(self, group: &Group) -> bool {
        match self {
            Self::Queue(revision) => group.playback.queue_revision == revision,
            Self::Command(revision) => group.playback.revision == revision,
        }
    }
}

impl GroupEvent {
    fn update(session_id: Uuid, group_id: Uuid, kind: &'static str, data: Value) -> Self {
        Self {
            session_id,
            group_id,
            message_type: "SyncPlayGroupUpdate",
            kind,
            data,
            requires_membership: !matches!(kind, "GroupLeft" | "NotInGroup" | "GroupDoesNotExist"),
            revision: None,
        }
    }
}

#[derive(Clone, Default)]
struct Registry {
    groups: HashMap<Uuid, Group>,
}

#[derive(Clone)]
struct Member {
    session_id: Uuid,
    user_id: Uuid,
    username: String,
    connected: bool,
}

#[derive(Clone)]
struct Group {
    id: Uuid,
    name: String,
    created_at: DateTime<Utc>,
    members: Vec<Member>,
    playback: Playback,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
struct GroupInfo {
    group_id: String,
    group_name: String,
    state: PlaybackState,
    participants: Vec<String>,
    last_updated_at: DateTime<Utc>,
}

impl Group {
    fn info(&self) -> GroupInfo {
        let mut participants = Vec::new();
        for member in &self.members {
            if !participants.contains(&member.username) {
                participants.push(member.username.clone());
            }
        }
        GroupInfo {
            group_id: self.id.simple().to_string(),
            group_name: self.name.clone(),
            state: self.playback.state,
            participants,
            last_updated_at: Utc::now(),
        }
    }

    fn joined(&mut self, session_id: Uuid, events: &mut Vec<GroupEvent>) {
        events.push(GroupEvent::update(
            session_id,
            self.id,
            "GroupJoined",
            json!(self.info()),
        ));
        self.bootstrap(session_id, events);
    }
}

impl Registry {
    fn membership(&self, session_id: Uuid) -> Option<Uuid> {
        self.groups
            .values()
            .find(|group| {
                group
                    .members
                    .iter()
                    .any(|member| member.session_id == session_id)
            })
            .map(|group| group.id)
    }

    fn remove(&mut self, session_id: Uuid, events: &mut Vec<GroupEvent>) -> bool {
        let Some(id) = self.membership(session_id) else {
            return false;
        };
        let group = self.groups.get_mut(&id).expect("membership exists");
        let index = group
            .members
            .iter()
            .position(|member| member.session_id == session_id)
            .expect("member exists");
        let member = group.members.remove(index);
        events.push(GroupEvent::update(
            session_id,
            id,
            "GroupLeft",
            json!(id.to_string()),
        ));
        for peer in &group.members {
            events.push(GroupEvent::update(
                peer.session_id,
                id,
                "UserLeft",
                json!(member.username),
            ));
        }
        group.departed(session_id, events);
        if group.members.is_empty() {
            self.groups.remove(&id);
        }
        true
    }

    fn prune(&mut self, active: &HashMap<Uuid, LiveMember>, events: &mut Vec<GroupEvent>) {
        let removed = self
            .groups
            .values()
            .flat_map(|group| &group.members)
            .filter(|member| {
                !active.get(&member.session_id).is_some_and(|live| {
                    live.eligible() && (!member.connected || live.member.connected)
                })
            })
            .map(|member| member.session_id)
            .collect::<Vec<_>>();
        for session_id in removed {
            self.remove(session_id, events);
        }
        for group in self.groups.values_mut() {
            for member in &mut group.members {
                if let Some(live) = active.get(&member.session_id) {
                    member.username.clone_from(&live.member.username);
                    member.connected |= live.member.connected;
                }
            }
        }
    }

    fn user_group_count(&self, user_id: Uuid) -> usize {
        self.groups
            .values()
            .filter(|group| group.members.iter().any(|member| member.user_id == user_id))
            .count()
    }
}

struct LiveMember {
    member: Member,
    user: auth::UserRecord,
    requested: bool,
}

impl LiveMember {
    fn eligible(&self) -> bool {
        self.user.allow_media_playback && self.user.sync_play_access != SyncPlayAccess::None
    }
}

enum Action {
    List,
    Detail(Uuid),
    New(String),
    Join(Uuid),
    Leave,
    Refresh,
    Deliver(GroupEvent),
    Control(Control),
}

enum Reply {
    List(Vec<GroupInfo>),
    Group(GroupInfo),
    Empty,
    Delivery(Option<(&'static str, Value)>),
}

// Holding the active-run and session/user share locks until commit prevents a
// concurrent logout, policy change or replacement run from authorizing an
// in-memory mutation. The candidate becomes visible only after that commit.
async fn execute(
    state: &AppState,
    token_hash: &str,
    user_id: Uuid,
    action: Action,
) -> Result<Reply, ApiError> {
    if let Action::Control(control) = &action {
        control.validate()?;
    }
    if state.shutdown_requested.load(Ordering::Acquire) {
        return Err(ApiError::Unavailable);
    }
    let mut registry = state.syncplay.registry.lock().await;
    let mut tx = state.db.begin().await?;
    if !db::active_run_is_current(&mut tx, state.run_id).await? {
        return Err(ApiError::Unavailable);
    }
    // User changes and deletion take this guard exclusively before locking
    // users and revoking tokens. Share it to avoid reversing that lock order.
    sqlx::query("SELECT pg_advisory_xact_lock_shared(82473011)")
        .execute(&mut *tx)
        .await?;
    let ids = registry
        .groups
        .values()
        .flat_map(|group| group.members.iter().map(|member| member.session_id))
        .collect::<Vec<_>>();
    let rows = sqlx::query("SELECT t.id AS session_id,u.id,u.username,u.is_admin,u.disabled,u.enable_remote_access,u.allow_media_playback,u.sync_play_access,u.enable_content_downloading,u.enable_live_tv_access,u.enable_live_tv_management,u.restrict_libraries,u.configuration,u.max_parental_rating,u.block_unrated_items,COALESCE(ARRAY(SELECT a.library_id FROM user_library_access a WHERE a.user_id=u.id ORDER BY a.library_id),ARRAY[]::uuid[]) AS allowed_library_ids,t.token_hash=$2 AS requested FROM auth_tokens t JOIN users u ON u.id=t.user_id WHERE (t.id=ANY($1) OR t.token_hash=$2) AND t.revoked_at IS NULL AND t.expires_at>clock_timestamp() AND u.disabled=FALSE FOR SHARE OF t,u")
        .bind(ids).bind(token_hash).fetch_all(&mut *tx).await?;
    let mut active = HashMap::new();
    for row in rows {
        let user = db::user_from_row(&row)?;
        let member = Member {
            session_id: row.try_get("session_id")?,
            user_id: user.id,
            username: user.username.clone(),
            connected: state.syncplay.connected(row.try_get("session_id")?)?,
        };
        active.insert(
            member.session_id,
            LiveMember {
                member,
                user,
                requested: row.try_get("requested")?,
            },
        );
    }
    let requested = active
        .values()
        .find(|member| member.requested && member.member.user_id == user_id)
        .ok_or(ApiError::Unauthorized)?;
    let session_id = requested.member.session_id;
    let mut candidate = registry.clone();
    let mut events = Vec::new();
    candidate.prune(&active, &mut events);
    let new_ids = match &action {
        Action::Control(control) => control.item_ids(),
        _ => &[],
    };
    let media = visibility::Snapshot::load(&mut tx, &candidate, new_ids).await?;
    media.prune(&mut candidate, &active, &mut events);
    let reply = match action {
        Action::Refresh => Reply::Empty,
        Action::Deliver(event) => {
            let group = candidate.groups.get(&event.group_id);
            let allowed = event.session_id == session_id
                && event
                    .revision
                    .is_none_or(|revision| group.is_some_and(|group| revision.current(group)))
                && (!event.requires_membership
                    || (requested.eligible()
                        && group.is_some_and(|group| {
                            group
                                .members
                                .iter()
                                .any(|member| member.session_id == session_id)
                        })));
            let data = if event.kind == "GroupJoined" {
                group.map(|group| {
                    let mut data = event.data;
                    data["Participants"] = json!(group.info().participants);
                    data
                })
            } else {
                Some(event.data)
            };
            let message = if allowed {
                data.map(|data| {
                    let data = if event.message_type == "SyncPlayGroupUpdate" {
                        json!({
                            "GroupId": event.group_id.simple().to_string(),
                            "Type": event.kind,
                            "Data": data,
                        })
                    } else {
                        data
                    };
                    (event.message_type, data)
                })
            } else {
                None
            };
            Reply::Delivery(message)
        }
        Action::Leave => {
            if !candidate.remove(session_id, &mut events) {
                events.push(GroupEvent::update(
                    session_id,
                    Uuid::nil(),
                    "NotInGroup",
                    json!(""),
                ));
            }
            Reply::Empty
        }
        Action::List => {
            if !requested.eligible() {
                return Err(ApiError::Forbidden);
            }
            let mut groups = candidate.groups.values().collect::<Vec<_>>();
            groups.sort_by_key(|group| (group.created_at, group.id));
            Reply::List(
                groups
                    .into_iter()
                    .filter(|group| media.group_allowed(group, &requested.user))
                    .map(Group::info)
                    .collect(),
            )
        }
        Action::Detail(id) => {
            if !requested.eligible() {
                return Err(ApiError::Forbidden);
            }
            let group = candidate
                .groups
                .get(&id)
                .filter(|group| media.group_allowed(group, &requested.user))
                .ok_or(ApiError::NotFound)?;
            Reply::Group(group.info())
        }
        Action::New(name) => {
            if !requested.eligible()
                || requested.user.sync_play_access != SyncPlayAccess::CreateAndJoinGroups
            {
                return Err(ApiError::Forbidden);
            }
            candidate.remove(session_id, &mut events);
            if candidate.groups.len() >= MAX_GROUPS
                || candidate.user_group_count(user_id) >= MAX_GROUPS_PER_USER
            {
                return Err(ApiError::RateLimited);
            }
            let now = Utc::now();
            let mut group = Group {
                id: Uuid::new_v4(),
                name,
                created_at: now,
                members: vec![requested.member.clone()],
                playback: Playback::new(now),
            };
            group.joined(session_id, &mut events);
            let info = group.info();
            candidate.groups.insert(group.id, group);
            Reply::Group(info)
        }
        Action::Join(id) => {
            if !requested.eligible() {
                return Err(ApiError::Forbidden);
            }
            if !candidate
                .groups
                .get(&id)
                .is_some_and(|group| media.group_allowed(group, &requested.user))
            {
                events.push(GroupEvent::update(
                    session_id,
                    Uuid::nil(),
                    "GroupDoesNotExist",
                    json!(""),
                ));
            } else {
                let previous = candidate.membership(session_id);
                if previous != Some(id) {
                    let target = &candidate.groups[&id];
                    if target.members.len() >= MAX_MEMBERS
                        || (!target
                            .members
                            .iter()
                            .any(|member| member.user_id == user_id)
                            && candidate.user_group_count(user_id) >= MAX_GROUPS_PER_USER)
                    {
                        return Err(ApiError::RateLimited);
                    }
                    candidate.remove(session_id, &mut events);
                }
                let group = candidate.groups.get_mut(&id).expect("target group exists");
                if previous != Some(id) {
                    group.members.push(requested.member.clone());
                } else if let Some(member) = group
                    .members
                    .iter_mut()
                    .find(|member| member.session_id == session_id)
                {
                    member.connected = requested.member.connected;
                }
                for peer in &group.members {
                    if peer.session_id != session_id {
                        events.push(GroupEvent::update(
                            peer.session_id,
                            id,
                            "UserJoined",
                            json!(requested.member.username),
                        ));
                    }
                }
                group.joined(session_id, &mut events);
            }
            Reply::Empty
        }
        Action::Control(control) => {
            if !requested.eligible() {
                return Err(ApiError::Forbidden);
            }
            control.validate()?;
            if let Some(id) = candidate.membership(session_id) {
                let group = candidate.groups.get_mut(&id).expect("member group exists");
                if !control.item_ids().iter().all(|item| {
                    group.members.iter().all(|member| {
                        active
                            .get(&member.session_id)
                            .is_some_and(|live| media.allowed(*item, &live.user))
                    })
                }) {
                    return Err(ApiError::Forbidden);
                }
                group.control(session_id, control, &mut events)?;
                if candidate
                    .groups
                    .values()
                    .map(|group| group.playback.entries.len())
                    .sum::<usize>()
                    > 4096
                {
                    return Err(ApiError::RateLimited);
                }
            } else {
                events.push(GroupEvent::update(
                    session_id,
                    Uuid::nil(),
                    "NotInGroup",
                    json!(""),
                ));
            }
            Reply::Empty
        }
    };
    tx.commit().await?;
    *registry = candidate;
    for event in events {
        let _ = state.syncplay.events.send(event);
    }
    Ok(reply)
}

fn token_hash(headers: &HeaderMap) -> Result<String, ApiError> {
    let (token, _) = auth::extract_raw_token(headers)?.ok_or(ApiError::Unauthorized)?;
    Ok(auth::token_digest(&token))
}

pub(crate) async fn refresh_session(
    state: &AppState,
    token_hash: &str,
    user_id: Uuid,
) -> Result<(), ApiError> {
    execute(state, token_hash, user_id, Action::Refresh).await?;
    Ok(())
}

pub(crate) async fn socket_event(
    state: &AppState,
    token_hash: &str,
    user_id: Uuid,
    event: GroupEvent,
) -> Result<Option<(&'static str, Value)>, ApiError> {
    let Reply::Delivery(data) = execute(state, token_hash, user_id, Action::Deliver(event)).await?
    else {
        unreachable!()
    };
    Ok(data)
}

async fn list(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    headers: HeaderMap,
) -> Result<Json<Vec<GroupInfo>>, ApiError> {
    let Reply::List(groups) =
        execute(&state, &token_hash(&headers)?, user.id, Action::List).await?
    else {
        unreachable!()
    };
    Ok(Json(groups))
}

async fn detail(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    headers: HeaderMap,
    Path(id): Path<Uuid>,
) -> Result<Json<GroupInfo>, ApiError> {
    let Reply::Group(group) =
        execute(&state, &token_hash(&headers)?, user.id, Action::Detail(id)).await?
    else {
        unreachable!()
    };
    Ok(Json(group))
}

#[derive(Default, Deserialize)]
#[serde(rename_all = "PascalCase")]
struct NewGroup {
    #[serde(alias = "groupName")]
    group_name: Option<String>,
}

async fn new(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    headers: HeaderMap,
    body: Result<Json<NewGroup>, JsonRejection>,
) -> Result<Json<GroupInfo>, ApiError> {
    let Json(body) = body.map_err(|_| ApiError::BadRequest("Invalid group request".to_owned()))?;
    let name = body.group_name.unwrap_or_default();
    if name.encode_utf16().count() > 200 || name.chars().any(char::is_control) {
        return Err(ApiError::BadRequest("Invalid group name".to_owned()));
    }
    let Reply::Group(group) = execute(
        &state,
        &token_hash(&headers)?,
        user.id,
        Action::New(name.trim().to_owned()),
    )
    .await?
    else {
        unreachable!()
    };
    Ok(Json(group))
}

#[derive(Deserialize)]
#[serde(rename_all = "PascalCase")]
struct JoinGroup {
    #[serde(alias = "groupId")]
    group_id: Option<Uuid>,
}

async fn join(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    headers: HeaderMap,
    body: Result<Json<JoinGroup>, JsonRejection>,
) -> Result<StatusCode, ApiError> {
    let Json(body) = body.map_err(|_| ApiError::BadRequest("Invalid group request".to_owned()))?;
    execute(
        &state,
        &token_hash(&headers)?,
        user.id,
        Action::Join(body.group_id.unwrap_or_default()),
    )
    .await?;
    Ok(StatusCode::NO_CONTENT)
}

async fn leave(
    State(state): State<AppState>,
    CurrentUser(user): CurrentUser,
    headers: HeaderMap,
) -> Result<StatusCode, ApiError> {
    execute(&state, &token_hash(&headers)?, user.id, Action::Leave).await?;
    Ok(StatusCode::NO_CONTENT)
}
