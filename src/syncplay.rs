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
        .with_state(state)
}

pub(crate) struct SyncGroups {
    registry: Mutex<Registry>,
    events: broadcast::Sender<GroupEvent>,
}

impl SyncGroups {
    pub(crate) fn new() -> Self {
        let (events, _) = broadcast::channel(256);
        Self {
            registry: Mutex::new(Registry::default()),
            events,
        }
    }

    pub(crate) fn subscribe(&self) -> broadcast::Receiver<GroupEvent> {
        self.events.subscribe()
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
        }
    }

    fn stop(session_id: Uuid, group: &Group) -> Self {
        Self {
            session_id,
            group_id: group.id,
            message_type: "SyncPlayCommand",
            kind: "Stop",
            data: json!({
                "GroupId": group.id.simple().to_string(),
                "PlaylistItemId": Uuid::nil().simple().to_string(),
                "When": group.created_at, "PositionTicks": 0,
                "Command": "Stop", "EmittedAt": Utc::now(),
            }),
            requires_membership: true,
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
}

#[derive(Clone)]
struct Group {
    id: Uuid,
    name: String,
    created_at: DateTime<Utc>,
    members: Vec<Member>,
}

#[derive(Clone, Serialize)]
#[serde(rename_all = "PascalCase")]
struct GroupInfo {
    group_id: String,
    group_name: String,
    state: &'static str,
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
            state: "Idle",
            participants,
            last_updated_at: Utc::now(),
        }
    }

    fn joined(&self, session_id: Uuid, events: &mut Vec<GroupEvent>) {
        events.push(GroupEvent::update(
            session_id,
            self.id,
            "GroupJoined",
            json!(self.info()),
        ));
        events.push(GroupEvent::stop(session_id, self));
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
                !active
                    .get(&member.session_id)
                    .is_some_and(LiveMember::eligible)
            })
            .map(|member| member.session_id)
            .collect::<Vec<_>>();
        for session_id in removed {
            self.remove(session_id, events);
        }
        for group in self.groups.values_mut() {
            for member in &mut group.members {
                if let Some(live) = active.get(&member.session_id)
                    && member.username != live.member.username
                {
                    member.username.clone_from(&live.member.username);
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
    access: SyncPlayAccess,
    playback: bool,
    requested: bool,
}

impl LiveMember {
    fn eligible(&self) -> bool {
        self.playback && self.access != SyncPlayAccess::None
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
    let rows = sqlx::query("SELECT t.id,u.id AS user_id,u.username,u.sync_play_access,u.allow_media_playback,t.token_hash=$2 AS requested FROM auth_tokens t JOIN users u ON u.id=t.user_id WHERE (t.id=ANY($1) OR t.token_hash=$2) AND t.revoked_at IS NULL AND t.expires_at>NOW() AND u.disabled=FALSE FOR SHARE OF t,u")
        .bind(ids).bind(token_hash).fetch_all(&mut *tx).await?;
    let mut active = HashMap::new();
    for row in rows {
        let access = match row.try_get::<&str, _>("sync_play_access")? {
            "CreateAndJoinGroups" => SyncPlayAccess::CreateAndJoinGroups,
            "JoinGroups" => SyncPlayAccess::JoinGroups,
            "None" => SyncPlayAccess::None,
            _ => return Err(ApiError::Unavailable),
        };
        let member = Member {
            session_id: row.try_get("id")?,
            user_id: row.try_get("user_id")?,
            username: row.try_get("username")?,
        };
        active.insert(
            member.session_id,
            LiveMember {
                member,
                access,
                playback: row.try_get("allow_media_playback")?,
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
    let reply = match action {
        Action::Refresh => Reply::Empty,
        Action::Deliver(event) => {
            let group = candidate.groups.get(&event.group_id);
            let allowed = event.session_id == session_id
                && (!event.requires_membership
                    || (requested.eligible()
                        && group.is_some_and(|group| {
                            group
                                .members
                                .iter()
                                .any(|member| member.session_id == session_id)
                        })));
            let data = if event.kind == "GroupJoined" {
                group.map(|group| json!(group.info()))
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
            Reply::List(groups.into_iter().map(Group::info).collect())
        }
        Action::Detail(id) => {
            if !requested.eligible() {
                return Err(ApiError::Forbidden);
            }
            Reply::Group(candidate.groups.get(&id).ok_or(ApiError::NotFound)?.info())
        }
        Action::New(name) => {
            if !requested.eligible() || requested.access != SyncPlayAccess::CreateAndJoinGroups {
                return Err(ApiError::Forbidden);
            }
            candidate.remove(session_id, &mut events);
            if candidate.groups.len() >= MAX_GROUPS
                || candidate.user_group_count(user_id) >= MAX_GROUPS_PER_USER
            {
                return Err(ApiError::RateLimited);
            }
            let now = Utc::now();
            let group = Group {
                id: Uuid::new_v4(),
                name,
                created_at: now,
                members: vec![requested.member.clone()],
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
            if !candidate.groups.contains_key(&id) {
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
