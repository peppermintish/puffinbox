use std::collections::HashMap;

use sqlx::{Postgres, Row, Transaction};
use uuid::Uuid;

use super::{Group, GroupEvent, LiveMember, Registry};
use crate::{ApiError, auth::UserRecord, db, library::ItemRecord};

pub(super) struct Snapshot {
    lineages: HashMap<Uuid, Vec<ItemRecord>>,
}

impl Snapshot {
    pub(super) async fn load(
        tx: &mut Transaction<'_, Postgres>,
        registry: &Registry,
        new_ids: &[Uuid],
    ) -> Result<Self, ApiError> {
        let mut ids = registry
            .groups
            .values()
            .flat_map(|group| group.playback.entries.iter().map(|entry| entry.item))
            .chain(new_ids.iter().copied())
            .collect::<Vec<_>>();
        ids.sort_unstable();
        ids.dedup();
        if ids.len() > 4096 + super::queue::MAX_QUEUE {
            return Err(ApiError::RateLimited);
        }
        if ids.is_empty() {
            return Ok(Self {
                lineages: HashMap::new(),
            });
        }
        let rating = db::policy_rating_sql("i");
        // A bounded, cycle-aware lineage includes policy ratings on containers.
        // Holding library/item share locks makes disable/delete operations wait
        // until this snapshot has authorized the candidate mutation.
        let rows = sqlx::query(&format!(
            "WITH RECURSIVE lineage(root,id,parent_id,depth,visited) AS (\
             SELECT id,id,parent_id,0,ARRAY[id] FROM items WHERE id=ANY($1) \
             UNION ALL SELECT lineage.root,parent.id,parent.parent_id,lineage.depth+1,lineage.visited || parent.id \
             FROM items parent JOIN lineage ON parent.id=lineage.parent_id JOIN items leaf ON leaf.id=lineage.root \
             WHERE parent.library_id=leaf.library_id AND lineage.depth<63 AND NOT parent.id=ANY(lineage.visited)) \
             SELECT lineage.root,lineage.depth,i.id,i.library_id,i.parent_id,i.name,i.sort_name,i.item_type,i.path,i.container,i.size_bytes,i.runtime_ticks,i.date_added,i.date_modified,{rating} AS rating,i.overview,i.metadata_json \
             FROM lineage JOIN items i ON i.id=lineage.id JOIN libraries l ON l.id=i.library_id AND l.enabled=TRUE \
             WHERE i.item_type<>'LiveTvChannel' OR EXISTS(SELECT 1 FROM live_tv_channels tv JOIN live_tv_sources src ON src.id=tv.source_id AND src.library_id=tv.library_id WHERE tv.item_id=i.id AND tv.enabled=TRUE AND src.enabled=TRUE) \
             ORDER BY lineage.root,lineage.depth FOR SHARE OF i,l"
        )).bind(ids).fetch_all(&mut **tx).await?;
        let mut lineages: HashMap<Uuid, Vec<ItemRecord>> = HashMap::new();
        for row in rows {
            lineages
                .entry(row.try_get("root")?)
                .or_default()
                .push(db::item_from_row(&row)?);
        }
        Ok(Self { lineages })
    }

    pub(super) fn allowed(&self, id: Uuid, user: &UserRecord) -> bool {
        let Some(lineage) = self.lineages.get(&id) else {
            return false;
        };
        let Some(leaf) = lineage.first() else {
            return false;
        };
        leaf.id == id
            && matches!(
                leaf.item_type.as_str(),
                "Movie"
                    | "Episode"
                    | "Audio"
                    | "MusicVideo"
                    | "Video"
                    | "Trailer"
                    | "AudioBook"
                    | "LiveTvChannel"
            )
            && lineage.last().is_some_and(|root| root.parent_id.is_none())
            && lineage
                .windows(2)
                .all(|pair| pair[0].parent_id == Some(pair[1].id))
            && lineage
                .iter()
                .all(|item| db::user_policy_allows_item(user, item))
    }

    pub(super) fn group_allowed(&self, group: &Group, user: &UserRecord) -> bool {
        group
            .playback
            .entries
            .iter()
            .all(|entry| self.allowed(entry.item, user))
    }

    pub(super) fn prune(
        &self,
        registry: &mut Registry,
        active: &HashMap<Uuid, LiveMember>,
        events: &mut Vec<GroupEvent>,
    ) {
        let removed = registry
            .groups
            .values()
            .flat_map(|group| {
                group
                    .members
                    .iter()
                    .filter(|member| {
                        !active
                            .get(&member.session_id)
                            .is_some_and(|live| self.group_allowed(group, &live.user))
                    })
                    .map(|member| member.session_id)
            })
            .collect::<Vec<_>>();
        for session in removed {
            registry.remove(session, events);
        }
    }
}
