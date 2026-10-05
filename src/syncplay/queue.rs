use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Duration, Utc};
use rand::seq::SliceRandom;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use uuid::Uuid;

use super::{EventRevision, Group, GroupEvent};
use crate::ApiError;

pub(super) const MAX_QUEUE: usize = 512;

#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
pub(super) enum Repeat {
    #[serde(rename = "RepeatOne")]
    One,
    #[serde(rename = "RepeatAll")]
    All,
    #[default]
    #[serde(rename = "RepeatNone")]
    None,
}

#[derive(Clone, Copy, Default, Deserialize, Serialize, PartialEq, Eq)]
pub(super) enum Shuffle {
    #[default]
    Sorted,
    Shuffle,
}

#[derive(Clone, Copy, Default, Deserialize, PartialEq, Eq)]
pub(super) enum QueueMode {
    #[default]
    Queue,
    QueueNext,
}

#[derive(Clone, Copy, Default, Serialize, PartialEq, Eq)]
pub(super) enum PlaybackState {
    #[default]
    Idle,
    Waiting,
    Paused,
    Playing,
}

#[derive(Clone)]
pub(super) struct Entry {
    pub(super) item: Uuid,
    id: Uuid,
    order: u64,
}

#[derive(Clone)]
pub(super) struct Playback {
    pub(super) entries: Vec<Entry>,
    index: Option<usize>,
    pub(super) state: PlaybackState,
    position: i64,
    when: DateTime<Utc>,
    last_update: DateTime<Utc>,
    repeat: Repeat,
    shuffle: Shuffle,
    ready: HashSet<Uuid>,
    ignored: HashSet<Uuid>,
    pings: HashMap<Uuid, i64>,
    next_order: u64,
    pub(super) revision: u64,
    pub(super) queue_revision: u64,
}

impl Playback {
    pub(super) fn new(now: DateTime<Utc>) -> Self {
        Self {
            entries: Vec::new(),
            index: None,
            state: PlaybackState::Idle,
            position: 0,
            when: now,
            last_update: now,
            repeat: Repeat::default(),
            shuffle: Shuffle::default(),
            ready: HashSet::new(),
            ignored: HashSet::new(),
            pings: HashMap::new(),
            next_order: 0,
            revision: 0,
            queue_revision: 0,
        }
    }

    fn current(&self) -> Uuid {
        self.index
            .and_then(|index| self.entries.get(index))
            .map_or(Uuid::nil(), |entry| entry.id)
    }

    fn position_at(&self, now: DateTime<Utc>) -> i64 {
        let elapsed = if self.state == PlaybackState::Playing {
            now.signed_duration_since(self.when)
                .num_microseconds()
                .unwrap_or(i64::MAX)
                .max(0)
                .saturating_mul(10)
        } else {
            0
        };
        self.position.saturating_add(elapsed)
    }

    fn freeze(&mut self, now: DateTime<Utc>) {
        self.position = self.position_at(now);
        self.when = now;
    }

    fn add(&mut self, ids: Vec<Uuid>) -> Vec<Entry> {
        ids.into_iter()
            .map(|item| {
                let entry = Entry {
                    item,
                    id: Uuid::new_v4(),
                    order: self.next_order,
                };
                self.next_order = self.next_order.saturating_add(1);
                entry
            })
            .collect()
    }

    fn remember_order(&mut self) {
        if self.shuffle == Shuffle::Sorted {
            for (index, entry) in self.entries.iter_mut().enumerate() {
                entry.order = index as u64;
            }
            self.next_order = self.entries.len() as u64;
        }
    }

    fn queue(&self, reason: &'static str) -> Value {
        json!({
            "Reason": reason, "LastUpdate": self.last_update,
            "Playlist": self.entries.iter().map(|entry| json!({
                "ItemId": entry.item.simple().to_string(), "PlaylistItemId": entry.id.simple().to_string(),
            })).collect::<Vec<_>>(),
            "PlayingItemIndex": self.index.map_or(-1, |index| index as i64),
            "StartPositionTicks": self.position, "IsPlaying": self.state == PlaybackState::Playing,
            "ShuffleMode": self.shuffle, "RepeatMode": self.repeat,
        })
    }

    fn changed(&mut self) {
        self.revision = self.revision.wrapping_add(1);
    }

    fn wait(&mut self, now: DateTime<Utc>) {
        self.freeze(now);
        self.state = PlaybackState::Waiting;
    }

    fn select(&mut self, index: usize, now: DateTime<Utc>) {
        self.index = Some(index);
        self.position = 0;
        self.when = now;
        self.state = PlaybackState::Waiting;
        self.ready.clear();
    }
}

pub(super) enum Control {
    Set(Vec<Uuid>, i32, i64),
    Queue(Vec<Uuid>, QueueMode),
    Move(Uuid, i32),
    Remove(Vec<Uuid>, bool, bool),
    Select(Uuid),
    Next(Uuid),
    Previous(Uuid),
    Repeat(Repeat),
    Shuffle(Shuffle),
    Pause,
    Unpause,
    Stop,
    Seek(i64),
    Ready(Report),
    Buffer(Report),
    Ignore(bool),
    Ping(i64),
}

impl Control {
    pub(super) fn item_ids(&self) -> &[Uuid] {
        match self {
            Self::Set(ids, ..) | Self::Queue(ids, _) => ids,
            _ => &[],
        }
    }

    pub(super) fn validate(&self) -> Result<(), ApiError> {
        let invalid = match self {
            Self::Set(ids, _, ticks) => ids.len() > MAX_QUEUE || *ticks < 0,
            Self::Queue(ids, _) => ids.len() > MAX_QUEUE,
            Self::Remove(ids, ..) => ids.len() > MAX_QUEUE,
            Self::Seek(ticks) => *ticks < 0,
            Self::Ping(ping) => !(0..=60_000).contains(ping),
            Self::Ready(report) | Self::Buffer(report) => report.position_ticks < 0,
            _ => false,
        };
        if invalid {
            Err(ApiError::BadRequest("Invalid SyncPlay control".to_owned()))
        } else {
            Ok(())
        }
    }
}

#[derive(Default, Deserialize)]
#[serde(default, rename_all = "PascalCase")]
pub(super) struct Report {
    #[serde(alias = "when")]
    pub(super) when: Option<DateTime<Utc>>,
    #[serde(alias = "positionTicks")]
    pub(super) position_ticks: i64,
    #[serde(alias = "isPlaying")]
    pub(super) is_playing: bool,
    #[serde(alias = "playlistItemId")]
    pub(super) playlist_item_id: Uuid,
}

impl Group {
    fn update_all(&self, kind: &'static str, data: Value, events: &mut Vec<GroupEvent>) {
        for member in &self.members {
            let mut event = GroupEvent::update(member.session_id, self.id, kind, data.clone());
            event.revision = Some(if kind == "PlayQueue" {
                EventRevision::Queue(self.playback.queue_revision)
            } else {
                EventRevision::Command(self.playback.revision)
            });
            events.push(event);
        }
    }

    fn state_update(&self, reason: &'static str, events: &mut Vec<GroupEvent>) {
        self.update_all(
            "StateUpdate",
            json!({"State": self.playback.state, "Reason": reason}),
            events,
        );
    }

    fn queue_update(
        &mut self,
        reason: &'static str,
        now: DateTime<Utc>,
        events: &mut Vec<GroupEvent>,
    ) {
        self.playback.changed();
        self.playback.last_update = now;
        self.playback.queue_revision = self.playback.queue_revision.wrapping_add(1);
        self.update_all("PlayQueue", self.playback.queue(reason), events);
    }

    fn command(
        &self,
        session: Option<Uuid>,
        command: &'static str,
        when: DateTime<Utc>,
        position: i64,
        events: &mut Vec<GroupEvent>,
    ) {
        let data = json!({
            "GroupId": self.id.simple().to_string(), "PlaylistItemId": self.playback.current().simple().to_string(),
            "When": when, "PositionTicks": position, "Command": command, "EmittedAt": Utc::now(),
        });
        for member in &self.members {
            if session.is_none_or(|id| id == member.session_id) {
                events.push(GroupEvent {
                    session_id: member.session_id,
                    group_id: self.id,
                    message_type: "SyncPlayCommand",
                    kind: command,
                    data: data.clone(),
                    requires_membership: true,
                    revision: Some(EventRevision::Command(self.playback.revision)),
                });
            }
        }
    }

    pub(super) fn bootstrap(&mut self, session: Uuid, events: &mut Vec<GroupEvent>) {
        if !self.playback.entries.is_empty() {
            if matches!(
                self.playback.state,
                PlaybackState::Playing | PlaybackState::Waiting
            ) {
                let now = Utc::now();
                if self.playback.state == PlaybackState::Playing {
                    self.playback.wait(now);
                }
                self.playback.ready.remove(&session);
                self.playback.revision = self.playback.revision.wrapping_add(1);
                for peer in &self.members {
                    if peer.session_id != session {
                        self.command(
                            Some(peer.session_id),
                            "Pause",
                            self.playback.when,
                            self.playback.position,
                            events,
                        );
                    }
                }
            }
            let mut event = GroupEvent::update(
                session,
                self.id,
                "PlayQueue",
                self.playback.queue("NewPlaylist"),
            );
            event.revision = Some(EventRevision::Queue(self.playback.queue_revision));
            events.push(event);
            return;
        }
        let command = match self.playback.state {
            PlaybackState::Idle => "Stop",
            PlaybackState::Playing => "Unpause",
            PlaybackState::Paused | PlaybackState::Waiting => "Pause",
        };
        self.command(
            Some(session),
            command,
            self.playback.when,
            self.playback.position,
            events,
        );
    }

    fn start_if_ready(
        &mut self,
        now: DateTime<Utc>,
        reason: &'static str,
        events: &mut Vec<GroupEvent>,
    ) -> bool {
        if self.playback.state != PlaybackState::Waiting
            || self.playback.index.is_none()
            || !self.members.iter().all(|member| {
                self.playback.ready.contains(&member.session_id)
                    || self.playback.ignored.contains(&member.session_id)
            })
        {
            return false;
        }
        let delay = self
            .members
            .iter()
            .map(|member| {
                self.playback
                    .pings
                    .get(&member.session_id)
                    .copied()
                    .unwrap_or(0)
            })
            .max()
            .unwrap_or(0)
            .saturating_mul(2)
            .max(1000);
        self.playback.when = now + Duration::milliseconds(delay);
        self.playback.state = PlaybackState::Playing;
        self.playback.changed();
        self.command(
            None,
            "Unpause",
            self.playback.when,
            self.playback.position,
            events,
        );
        self.state_update(reason, events);
        true
    }

    pub(super) fn departed(&mut self, session: Uuid, events: &mut Vec<GroupEvent>) {
        self.playback.ready.remove(&session);
        self.playback.ignored.remove(&session);
        self.playback.pings.remove(&session);
        self.start_if_ready(Utc::now(), "UserLeft", events);
    }

    pub(super) fn control(
        &mut self,
        session: Uuid,
        control: Control,
        events: &mut Vec<GroupEvent>,
    ) -> Result<(), ApiError> {
        let now = Utc::now();
        match control {
            Control::Set(ids, position, ticks) => {
                let Ok(index) = usize::try_from(position) else {
                    return Ok(());
                };
                if index >= ids.len() {
                    return Ok(());
                }
                self.playback.entries = self.playback.add(ids);
                self.playback.index = Some(index);
                self.playback.position = ticks;
                self.playback.when = now;
                self.playback.state = PlaybackState::Waiting;
                self.playback.ready.clear();
                self.playback.remember_order();
                self.queue_update("NewPlaylist", now, events);
            }
            Control::Queue(ids, mode) => {
                if ids.is_empty() {
                    return Ok(());
                }
                if self.playback.entries.len().saturating_add(ids.len()) > MAX_QUEUE {
                    return Err(ApiError::RateLimited);
                }
                let entries = self.playback.add(ids);
                let reason = if mode == QueueMode::QueueNext {
                    let index = self.playback.index.map_or(0, |index| index + 1);
                    self.playback.entries.splice(index..index, entries);
                    "QueueNext"
                } else {
                    self.playback.entries.extend(entries);
                    "Queue"
                };
                if self.playback.index.is_none() {
                    self.playback.select(0, now);
                }
                self.playback.remember_order();
                self.queue_update(reason, now, events);
            }
            Control::Move(id, new_index) => {
                let Some(old) = self
                    .playback
                    .entries
                    .iter()
                    .position(|entry| entry.id == id)
                else {
                    return Ok(());
                };
                let new = usize::try_from(new_index)
                    .unwrap_or(0)
                    .min(self.playback.entries.len() - 1);
                let current = self.playback.current();
                let entry = self.playback.entries.remove(old);
                self.playback.entries.insert(new, entry);
                self.playback.index = self
                    .playback
                    .entries
                    .iter()
                    .position(|entry| entry.id == current);
                self.playback.remember_order();
                self.queue_update("MoveItem", now, events);
            }
            Control::Remove(ids, clear, clear_current) => {
                let current = self.playback.current();
                let old_index = self.playback.index.unwrap_or(0);
                self.playback.entries.retain(|entry| {
                    if clear {
                        !clear_current && entry.id == current
                    } else {
                        !ids.contains(&entry.id)
                    }
                });
                let index = self
                    .playback
                    .entries
                    .iter()
                    .position(|entry| entry.id == current);
                if self.playback.entries.is_empty() {
                    self.playback.index = None;
                    self.playback.state = PlaybackState::Idle;
                    self.playback.position = 0;
                    self.playback.when = now;
                    self.playback.ready.clear();
                } else if index.is_none() {
                    self.playback
                        .select(old_index.min(self.playback.entries.len() - 1), now);
                } else {
                    self.playback.index = index;
                }
                self.queue_update("RemoveItems", now, events);
                if self.playback.index.is_none() {
                    self.command(None, "Stop", now, 0, events);
                }
            }
            Control::Select(id) => {
                if let Some(index) = self
                    .playback
                    .entries
                    .iter()
                    .position(|entry| entry.id == id)
                {
                    self.playback.select(index, now);
                    self.queue_update("SetCurrentItem", now, events);
                }
            }
            Control::Next(id) | Control::Previous(id)
                if id != self.playback.current() || id.is_nil() => {}
            Control::Next(_) => {
                let Some(index) = self.playback.index else {
                    return Ok(());
                };
                let next = match self.playback.repeat {
                    Repeat::One => Some(index),
                    Repeat::All => Some((index + 1) % self.playback.entries.len()),
                    Repeat::None => (index + 1 < self.playback.entries.len()).then_some(index + 1),
                };
                if let Some(index) = next {
                    self.playback.select(index, now);
                } else {
                    self.playback.freeze(now);
                    self.playback.state = PlaybackState::Idle;
                }
                self.queue_update("NextItem", now, events);
                if next.is_none() {
                    self.command(None, "Stop", now, self.playback.position, events);
                }
            }
            Control::Previous(_) => {
                let Some(index) = self.playback.index else {
                    return Ok(());
                };
                let previous = if self.playback.repeat == Repeat::One {
                    index
                } else if index > 0 {
                    index - 1
                } else if self.playback.repeat == Repeat::All {
                    self.playback.entries.len() - 1
                } else {
                    0
                };
                self.playback.select(previous, now);
                self.queue_update("PreviousItem", now, events);
            }
            Control::Repeat(mode) => {
                self.playback.repeat = mode;
                self.queue_update("RepeatMode", now, events);
            }
            Control::Shuffle(mode) => {
                let current = self.playback.current();
                if self.playback.shuffle != mode {
                    if mode == Shuffle::Shuffle {
                        let playing = self
                            .playback
                            .index
                            .map(|index| self.playback.entries.remove(index));
                        self.playback.entries.shuffle(&mut rand::thread_rng());
                        if let Some(entry) = playing {
                            self.playback.entries.insert(0, entry);
                        }
                    } else {
                        self.playback.entries.sort_by_key(|entry| entry.order);
                    }
                    self.playback.index = self
                        .playback
                        .entries
                        .iter()
                        .position(|entry| entry.id == current);
                    self.playback.shuffle = mode;
                }
                self.queue_update("ShuffleMode", now, events);
            }
            Control::Pause => {
                if self.playback.index.is_none() {
                    return Ok(());
                }
                self.playback.freeze(now);
                self.playback.state = PlaybackState::Paused;
                self.playback.changed();
                self.command(None, "Pause", now, self.playback.position, events);
                self.state_update("Pause", events);
            }
            Control::Stop => {
                if self.playback.state == PlaybackState::Playing {
                    self.playback.freeze(now);
                }
                self.playback.state = PlaybackState::Idle;
                self.playback.ready.clear();
                self.playback.changed();
                self.command(
                    None,
                    "Stop",
                    self.playback.when,
                    self.playback.position,
                    events,
                );
            }
            Control::Seek(ticks) => {
                if self.playback.index.is_none() {
                    return Ok(());
                }
                self.playback.position = ticks;
                self.playback.when = now;
                self.playback.state = PlaybackState::Waiting;
                self.playback.ready.clear();
                self.playback.changed();
                self.command(None, "Seek", now, ticks, events);
                self.state_update("Seek", events);
            }
            Control::Unpause => {
                if self.playback.index.is_none() {
                    return Ok(());
                }
                self.playback.wait(now);
                self.playback.changed();
                if !self.start_if_ready(now, "Unpause", events) {
                    self.state_update("Unpause", events);
                }
            }
            Control::Ready(report) => {
                if report.playlist_item_id != self.playback.current()
                    || self.playback.index.is_none()
                {
                    return Ok(());
                }
                if self.playback.state != PlaybackState::Waiting {
                    let command = match self.playback.state {
                        PlaybackState::Playing => "Unpause",
                        PlaybackState::Paused => "Pause",
                        _ => "Stop",
                    };
                    self.command(
                        Some(session),
                        command,
                        self.playback.when,
                        self.playback.position,
                        events,
                    );
                    return Ok(());
                }
                let drift = report.position_ticks.abs_diff(self.playback.position);
                if drift > 5_000_000 {
                    self.playback.ready.remove(&session);
                    self.command(
                        Some(session),
                        "Seek",
                        self.playback.when,
                        self.playback.position,
                        events,
                    );
                    self.state_update("Ready", events);
                } else {
                    self.playback.ready.insert(session);
                    if !self.start_if_ready(now, "Ready", events) {
                        let when = if report.is_playing {
                            report
                                .when
                                .filter(|when| {
                                    when.signed_duration_since(now).num_seconds().abs() <= 300
                                })
                                .unwrap_or(now)
                        } else {
                            now - Duration::microseconds(
                                report.position_ticks.saturating_sub(self.playback.position) / 10,
                            )
                        };
                        self.command(Some(session), "Pause", when, self.playback.position, events);
                    }
                }
            }
            Control::Buffer(report) => {
                if report.playlist_item_id != self.playback.current()
                    || self.playback.index.is_none()
                    || self.playback.state == PlaybackState::Idle
                {
                    return Ok(());
                }
                self.playback.wait(now);
                self.playback.ready.remove(&session);
                self.playback.changed();
                for peer in &self.members {
                    if peer.session_id != session {
                        self.command(
                            Some(peer.session_id),
                            "Pause",
                            now,
                            self.playback.position,
                            events,
                        );
                    }
                }
                self.state_update("Buffer", events);
            }
            Control::Ignore(ignore) => {
                if ignore {
                    self.playback.ignored.insert(session);
                } else {
                    self.playback.ignored.remove(&session);
                }
                self.start_if_ready(now, "SetIgnoreWait", events);
            }
            Control::Ping(ping) => {
                self.playback.pings.insert(session, ping);
            }
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::syncplay::Member;

    #[test]
    fn seek_preserves_the_queue_frame_and_stop_invalidates_old_commands() {
        let now = Utc::now();
        let session = Uuid::new_v4();
        let mut group = Group {
            id: Uuid::new_v4(),
            name: String::new(),
            created_at: now,
            members: vec![Member {
                session_id: session,
                user_id: Uuid::new_v4(),
                username: "member".to_owned(),
                connected: false,
            }],
            playback: Playback::new(now),
        };
        let mut events = Vec::new();
        group
            .control(
                session,
                Control::Set(vec![Uuid::new_v4()], 0, 50_000_000),
                &mut events,
            )
            .unwrap();
        let queue = events.remove(0);
        group
            .control(session, Control::Seek(100_000_000), &mut events)
            .unwrap();
        assert!(queue.revision.unwrap().current(&group));
        let seek = events.remove(0);
        assert!(seek.revision.unwrap().current(&group));
        group.control(session, Control::Stop, &mut events).unwrap();
        assert!(!seek.revision.unwrap().current(&group));
        assert!(queue.revision.unwrap().current(&group));
        group
            .control(
                session,
                Control::Set(vec![Uuid::new_v4()], 0, 0),
                &mut events,
            )
            .unwrap();
        assert!(!queue.revision.unwrap().current(&group));
    }

    #[test]
    fn scheduled_position_waits_for_start_and_saturates_without_overflow() {
        let now = Utc::now();
        let mut playback = Playback::new(now);
        playback.position = i64::MAX - 10;
        playback.state = PlaybackState::Playing;
        playback.when = now + Duration::seconds(1);
        assert_eq!(playback.position_at(now), i64::MAX - 10);
        assert_eq!(playback.position_at(now + Duration::seconds(2)), i64::MAX);
        playback.freeze(now + Duration::seconds(2));
        playback.state = PlaybackState::Paused;
        assert_eq!(playback.position_at(now + Duration::days(1)), i64::MAX);
    }
}
