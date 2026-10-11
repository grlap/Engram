//! Reported machine activity is shared information, never execution authority.

use super::{SharedActor, invalid, notes, validate_shared_actor};
use crate::{ObjectId, WorkId, storage::StoreError};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

pub type ActivityMap = BTreeMap<String, BTreeMap<ObjectId, ActivityNotice>>;

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActivityState {
    Started,
    Released,
    Abandoned,
    Finished,
}

/// Identity comes from the canonical start event; provenance names the latest
/// recorded meaningful transition. Neither field restores an executable token.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ActivityNotice {
    pub activity_id: ObjectId,
    pub task_id: WorkId,
    pub origin_machine: String,
    pub actor: SharedActor,
    pub state: ActivityState,
    pub state_revision: u64,
    pub event_id: ObjectId,
}

/// Export options are explicit disclosure choices, not host identity detection.
#[derive(Clone, Debug, Default)]
pub struct TaskSyncExportOptions {
    pub include_notes: bool,
    pub origin_machine: Option<String>,
}

impl TaskSyncExportOptions {
    /// # Errors
    /// Refuses notes without an alias, and aliases containing paths or prose.
    pub fn validate(&self) -> Result<(), StoreError> {
        if let Some(machine) = &self.origin_machine {
            notes::validate_machine_alias(machine)?;
        } else if self.include_notes {
            return Err(invalid("origin-machine required for shared notes"));
        }
        Ok(())
    }
}

pub(crate) fn validate_structure(activity: &ActivityMap, task: WorkId) -> Result<(), StoreError> {
    for (machine, entries) in activity {
        notes::validate_machine_alias(machine)?;
        if entries.is_empty() {
            return Err(invalid("empty activity origin map"));
        }
        for (id, notice) in entries {
            validate_shared_actor(&notice.actor)?;
            if id != &notice.activity_id
                || machine != &notice.origin_machine
                || task != notice.task_id
            {
                return Err(invalid("activity map key or task binding differs"));
            }
            if ObjectId::from_stored(id.as_str().into()).is_none()
                || ObjectId::from_stored(notice.event_id.as_str().into()).is_none()
                || notice.state_revision == 0
            {
                return Err(invalid(
                    "activity requires original ids and a positive state revision",
                ));
            }
        }
    }
    Ok(())
}

pub(crate) fn exclusion(notice: &ActivityNotice, labels: &[String]) -> Option<&'static str> {
    if notes::restricted(labels) {
        Some("restricted-task")
    } else {
        notes::author_exclusion(&notice.actor.actor_id)
            .or_else(|| notes::author_exclusion(&notice.actor.actor_kind))
    }
}

pub(crate) fn validate_disclosure(
    activity: &ActivityMap,
    labels: &[String],
) -> Result<(), StoreError> {
    for notice in activity.values().flat_map(BTreeMap::values) {
        let valid_state = match notice.state {
            ActivityState::Started => {
                notice.state_revision == 1 && notice.event_id == notice.activity_id
            }
            ActivityState::Released | ActivityState::Abandoned | ActivityState::Finished => {
                notice.state_revision >= 2 && notice.event_id != notice.activity_id
            }
        };
        if !valid_state {
            return Err(invalid(
                "activity state, revision and event identity disagree",
            ));
        }
        if let Some(reason) = exclusion(notice, labels) {
            return Err(invalid(format!("shared activity refused: {reason}")));
        }
    }
    Ok(())
}
