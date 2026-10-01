//! Authenticated, bounded user-data notifications over the public socket API.

use std::{
    collections::HashMap,
    sync::{Arc, Mutex, atomic::Ordering},
    time::{Duration, Instant},
};

use axum::{
    extract::{
        State,
        ws::{CloseFrame, Message, WebSocket, WebSocketUpgrade},
    },
    response::Response,
};
use serde::Deserialize;
use serde_json::{Value, json};
use tokio::{
    sync::{OwnedSemaphorePermit, Semaphore, broadcast, watch},
    time::{interval, timeout},
};
use uuid::Uuid;

use crate::{
    ApiError, AppState, api,
    auth::{SocketIdentity, UserRecord},
    db,
};

const MAX_CONNECTIONS: usize = 512;
const MAX_USER_CONNECTIONS: usize = 8;
const MAX_MESSAGE_BYTES: usize = 16 * 1024;
const SEND_TIMEOUT: Duration = Duration::from_secs(3);
const AUTH_INTERVAL: Duration = Duration::from_secs(5);
const KEEPALIVE_SECONDS: u64 = 90;

#[derive(Clone, Copy)]
pub(crate) struct UserEvent {
    user_id: Uuid,
    item_id: Uuid,
}

pub(crate) struct UserEvents {
    sender: broadcast::Sender<UserEvent>,
    slots: Arc<Semaphore>,
    users: Mutex<HashMap<Uuid, usize>>,
    shutdown: watch::Sender<bool>,
}

impl UserEvents {
    pub(crate) fn new() -> Self {
        let (sender, _) = broadcast::channel(256);
        let (shutdown, _) = watch::channel(false);
        Self {
            sender,
            slots: Arc::new(Semaphore::new(MAX_CONNECTIONS)),
            users: Mutex::new(HashMap::new()),
            shutdown,
        }
    }

    pub(crate) fn publish(&self, user_id: Uuid, item_id: Uuid) {
        // Only identities enter this queue. The receiving socket reloads the
        // committed data under its current token, user and item policies.
        let _ = self.sender.send(UserEvent { user_id, item_id });
    }

    fn enter(self: &Arc<Self>, user_id: Uuid) -> Result<Registration, ApiError> {
        if *self.shutdown.borrow() {
            return Err(ApiError::Unavailable);
        }
        let slot = self
            .slots
            .clone()
            .try_acquire_owned()
            .map_err(|_| ApiError::RateLimited)?;
        if *self.shutdown.borrow() {
            return Err(ApiError::Unavailable);
        }
        let mut users = self.users.lock().map_err(|_| ApiError::Unavailable)?;
        let count = users.entry(user_id).or_default();
        if *count >= MAX_USER_CONNECTIONS {
            return Err(ApiError::RateLimited);
        }
        *count += 1;
        Ok(Registration {
            events: self.clone(),
            user_id,
            _slot: slot,
        })
    }

    pub(crate) async fn drain(&self) -> bool {
        self.shutdown.send_replace(true);
        matches!(
            timeout(
                Duration::from_secs(5),
                self.slots.acquire_many(MAX_CONNECTIONS as u32)
            )
            .await,
            Ok(Ok(_))
        )
    }
}

struct Registration {
    events: Arc<UserEvents>,
    user_id: Uuid,
    _slot: OwnedSemaphorePermit,
}

impl Drop for Registration {
    fn drop(&mut self) {
        if let Ok(mut users) = self.events.users.lock()
            && let Some(count) = users.get_mut(&self.user_id)
        {
            *count = count.saturating_sub(1);
            if *count == 0 {
                users.remove(&self.user_id);
            }
        }
    }
}

pub(crate) async fn connect(
    State(state): State<AppState>,
    identity: SocketIdentity,
    upgrade: WebSocketUpgrade,
) -> Result<Response, ApiError> {
    if state.shutdown_requested.load(Ordering::Acquire) {
        return Err(ApiError::Unavailable);
    }
    let registration = state.user_events.enter(identity.user.id)?;
    let events = state.user_events.sender.subscribe();
    Ok(upgrade
        .read_buffer_size(MAX_MESSAGE_BYTES)
        .write_buffer_size(0)
        .max_write_buffer_size(64 * 1024)
        .max_message_size(MAX_MESSAGE_BYTES)
        .max_frame_size(MAX_MESSAGE_BYTES)
        .on_upgrade(move |socket| serve(socket, state, identity, registration, events)))
}

#[derive(Deserialize)]
struct Incoming {
    #[serde(rename = "MessageType")]
    message_type: String,
}

async fn current_user(state: &AppState, identity: &SocketIdentity) -> Result<UserRecord, ApiError> {
    if state.shutdown_requested.load(Ordering::Acquire) {
        return Err(ApiError::Unavailable);
    }
    let (token_id, user) = db::active_auth_identity(&state.db, &identity.token_hash)
        .await?
        .ok_or(ApiError::Unauthorized)?;
    if user.id != identity.user.id {
        return Err(ApiError::Unauthorized);
    }
    if !user.enable_remote_access && !identity.local_client {
        return Err(ApiError::Forbidden);
    }
    db::touch_auth_token(&state.db, state.run_id, token_id).await?;
    Ok(user)
}

fn envelope(message_type: &str, data: Option<Value>) -> Message {
    let mut value = json!({ "MessageType": message_type, "MessageId": Uuid::new_v4() });
    if let Some(data) = data {
        value["Data"] = data;
    }
    Message::Text(value.to_string().into())
}

async fn send(socket: &mut WebSocket, message: Message) -> bool {
    matches!(
        timeout(SEND_TIMEOUT, socket.send(message)).await,
        Ok(Ok(()))
    )
}

async fn close(socket: &mut WebSocket, code: u16, reason: &'static str) {
    let _ = send(
        socket,
        Message::Close(Some(CloseFrame {
            code,
            reason: reason.into(),
        })),
    )
    .await;
}

async fn close_session(socket: &mut WebSocket, state: &AppState) {
    if state.shutdown_requested.load(Ordering::Acquire) {
        close(socket, 1001, "Server is shutting down").await;
    } else {
        close(socket, 1008, "Session is no longer available").await;
    }
}

async fn serve(
    mut socket: WebSocket,
    state: AppState,
    identity: SocketIdentity,
    _registration: Registration,
    mut events: broadcast::Receiver<UserEvent>,
) {
    let mut shutdown = state.user_events.shutdown.subscribe();
    if *shutdown.borrow() || state.shutdown_requested.load(Ordering::Acquire) {
        close(&mut socket, 1001, "Server is shutting down").await;
        return;
    }
    // ForceKeepAlive's Data is the public timeout in seconds. Clients reply
    // with KeepAlive; protocol-level Ping/Pong also keeps the socket alive.
    if !send(
        &mut socket,
        envelope("ForceKeepAlive", Some(json!(KEEPALIVE_SECONDS))),
    )
    .await
    {
        return;
    }
    let mut check = interval(AUTH_INTERVAL);
    let mut keepalive = interval(Duration::from_secs(30));
    keepalive.tick().await;
    let mut last_received = Instant::now();
    let mut rate_window = Instant::now();
    let mut received_in_window = 0u32;
    loop {
        tokio::select! {
            _ = shutdown.changed() => {
                close(&mut socket, 1001, "Server is shutting down").await;
                return;
            },
            _ = check.tick() => {
                if current_user(&state, &identity).await.is_err() {
                    close_session(&mut socket, &state).await;
                    return;
                }
                if last_received.elapsed() > Duration::from_secs(KEEPALIVE_SECONDS) {
                    close(&mut socket, 1001, "Keepalive expired").await;
                    return;
                }
            },
            _ = keepalive.tick() => {
                if !send(&mut socket, envelope("ForceKeepAlive", Some(json!(KEEPALIVE_SECONDS)))).await { return; }
            },
            incoming = socket.recv() => {
                let Some(Ok(message)) = incoming else { return; };
                if rate_window.elapsed() >= Duration::from_secs(1) {
                    rate_window = Instant::now(); received_in_window = 0;
                }
                received_in_window += 1;
                if received_in_window > 32 {
                    close(&mut socket, 1008, "Message rate exceeded").await; return;
                }
                match message {
                    Message::Close(_) => { let _ = send(&mut socket, Message::Close(None)).await; return; },
                    Message::Ping(bytes) => { last_received = Instant::now(); if !send(&mut socket, Message::Pong(bytes)).await { return; } },
                    Message::Pong(_) => { last_received = Instant::now(); },
                    Message::Binary(_) => { close(&mut socket, 1003, "Text messages are required").await; return; },
                    Message::Text(text) => {
                        let Ok(incoming) = serde_json::from_str::<Incoming>(&text) else {
                            close(&mut socket, 1007, "Invalid message").await; return;
                        };
                        if incoming.message_type == "KeepAlive" {
                            last_received = Instant::now();
                            if !send(&mut socket, envelope("KeepAlive", None)).await { return; }
                        }
                    },
                }
            },
            event = events.recv() => {
                let event = match event {
                    Ok(event) => event,
                    Err(broadcast::error::RecvError::Lagged(_)) => {
                        close(&mut socket, 1013, "Reconnect and refresh user data").await; return;
                    },
                    Err(broadcast::error::RecvError::Closed) => return,
                };
                if event.user_id != identity.user.id { continue; }
                let user = match current_user(&state, &identity).await {
                    Ok(user) => user,
                    Err(_) => { close_session(&mut socket, &state).await; return; },
                };
                let data = match api::socket_user_data(&state, &user, event.item_id).await {
                    Ok(Some(data)) => data,
                    Ok(None) => continue,
                    Err(_) => { close(&mut socket, 1011, "User data is unavailable").await; return; },
                };
                let message = envelope("UserDataChanged", Some(json!({
                    "UserId": user.id, "UserDataList": [data],
                })));
                if !send(&mut socket, message).await { return; }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn connection_admission_is_per_user_and_releases_on_drop() {
        let events = Arc::new(UserEvents::new());
        let user = Uuid::new_v4();
        let connections = (0..MAX_USER_CONNECTIONS)
            .map(|_| events.enter(user).unwrap())
            .collect::<Vec<_>>();
        assert!(matches!(events.enter(user), Err(ApiError::RateLimited)));
        let other = events.enter(Uuid::new_v4()).unwrap();
        drop(connections);
        assert!(events.enter(user).is_ok());
        drop(other);
        assert_eq!(events.slots.available_permits(), MAX_CONNECTIONS);
        assert!(events.users.lock().unwrap().is_empty());
    }

    #[test]
    fn global_admission_cannot_grow_with_distinct_users() {
        let events = Arc::new(UserEvents::new());
        let connections = (0..MAX_CONNECTIONS)
            .map(|_| events.enter(Uuid::new_v4()).unwrap())
            .collect::<Vec<_>>();
        assert!(matches!(
            events.enter(Uuid::new_v4()),
            Err(ApiError::RateLimited)
        ));
        drop(connections);
        assert!(events.users.lock().unwrap().is_empty());
    }

    #[tokio::test]
    async fn shutdown_waits_for_registrations_and_rejects_new_connections() {
        let events = Arc::new(UserEvents::new());
        let registration = events.enter(Uuid::new_v4()).unwrap();
        let mut drain = Box::pin(events.drain());
        assert!(
            timeout(Duration::from_millis(10), &mut drain)
                .await
                .is_err()
        );
        assert!(matches!(
            events.enter(Uuid::new_v4()),
            Err(ApiError::Unavailable)
        ));
        drop(registration);
        assert!(drain.await);
    }
}
