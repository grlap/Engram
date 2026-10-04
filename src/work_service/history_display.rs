//! Transient event facts for receipt history; stored delivery summaries stay fixed.

use super::{FeedPosition, SqliteStore, StoreError, WorkEvent, WorkTransition};

#[derive(Clone, Debug)]
pub(crate) struct HistoryDisplay {
    event: WorkEvent,
    fields: Vec<&'static str>,
    cleared: Option<crate::WorkBlocker>,
}

impl HistoryDisplay {
    pub(super) fn load(
        store: &SqliteStore,
        event: &WorkEvent,
        position: &FeedPosition,
    ) -> Result<Self, StoreError> {
        let fields = if matches!(event.transition, WorkTransition::Revised { .. }) {
            let previous = store.work_planning_before(position, event)?;
            let current = serde_json::to_value(&event.work)?;
            [
                ("title", "title"),
                ("outcome", "outcome"),
                ("acceptance", "acceptance"),
                ("kind", "kind"),
                ("priority", "priority"),
                ("labels", "labels"),
                ("external_ref", "external reference"),
                ("assigned_to", "assignment"),
                ("deferred_until", "deferral"),
                ("evaluation_mode", "evaluation mode"),
            ]
            .into_iter()
            .filter_map(|(key, word)| (previous[key] != current[key]).then_some(word))
            .collect()
        } else {
            Vec::new()
        };
        let cleared = match &event.transition {
            WorkTransition::Unblocked { blocker_id } => {
                store.cleared_work_blocker(event.work_id, blocker_id)?
            }
            _ => None,
        };
        Ok(Self {
            event: event.clone(),
            fields,
            cleared,
        })
    }

    pub(crate) fn summary(&self, include_title: bool) -> String {
        let detail = transition_detail(&self.event, &self.fields, self.cleared.as_ref());
        // Bound the final line, rather than each ingredient: a long reason uses
        // the available bytes before the optional peer item's title does.
        let line = if include_title {
            format!("{detail}: \"{}\"", self.event.work.title)
        } else {
            detail
        };
        super::compact_text_to(&line, 192)
    }
}

fn transition_detail(
    event: &WorkEvent,
    fields: &[&str],
    cleared: Option<&crate::WorkBlocker>,
) -> String {
    use WorkTransition as T;
    let work_ref = |id: crate::WorkId| {
        let simple = id.0.simple().to_string();
        format!("w-{}", simple.get(20..).unwrap_or(&simple))
    };
    match &event.transition {
        T::Created { .. }
            if event
                .actor
                .provenance_chain
                .iter()
                .any(crate::domain::is_peer_child_proposal_marker) =>
        {
            "peer optional-child proposal".into()
        }
        T::Created { prerequisites } => match prerequisites.len() {
            0 => "without prerequisites".into(),
            1 => "1 prerequisite".into(),
            count => format!("{count} prerequisites"),
        },
        T::Decomposed { children, .. } => match children.len() {
            1 => "added 1 child item".into(),
            count => format!("added {count} child items"),
        },
        T::Revised { .. } => {
            if fields.is_empty() {
                "no planning change".into()
            } else {
                fields.join(", ")
            }
        }
        T::PrerequisiteAdded {
            prerequisite_id, ..
        } => format!("added prerequisite {}", work_ref(*prerequisite_id)),
        T::PrerequisiteRemoved {
            prerequisite_id, ..
        } => format!("removed prerequisite {}", work_ref(*prerequisite_id)),
        T::Blocked { blocker_id } => super::history_blocker(blocker_id, event.blocker.as_ref()),
        T::Unblocked { blocker_id } => {
            format!("cleared {}", super::history_blocker(blocker_id, cleared))
        }
        T::Claimed { recovered, .. } => {
            if *recovered {
                "after recovery by a session".into()
            } else {
                "by a session".into()
            }
        }
        T::ClaimRenewed { .. } => "renewed by its holder".into(),
        T::Released { reason, .. } | T::HandoffCancelled { reason, .. } => {
            format!("because {reason}")
        }
        T::Reopened {
            generation, reason, ..
        } => format!("generation {generation} because {reason}"),
        T::Checkpointed { .. } => "progress checkpoint".into(),
        T::HandoffOffered { .. } => "to another session".into(),
        T::HandoffExpired { .. } => "offer expired".into(),
        T::HandedOff { .. } => "from one session to another".into(),
        T::EvidenceAdded { .. } => "recorded evidence".into(),
        T::MemoryCaptured { .. } => "shared work memory".into(),
        T::TypedEvidenceAdded { evidence_kind, .. } => format!("{} evidence", evidence_kind.word()),
        T::Completed { .. } => "completed".into(),
        T::Disposed {
            lifecycle,
            replacement_id,
            reason,
        } => {
            let replacement = replacement_id
                .map(|id| format!(" by {}", work_ref(id)))
                .unwrap_or_default();
            format!("to {}{replacement} because {reason}", lifecycle.word())
        }
        T::RequiredChildWaived {
            child_id, reason, ..
        } => format!(
            "waived required child {} because {reason}",
            work_ref(*child_id)
        ),
    }
}

pub(super) fn hydrate(
    store: &SqliteStore,
    entry: &super::WorkFeedEntry,
    object: &serde_json::Value,
) -> Result<Option<HistoryDisplay>, StoreError> {
    if entry.object_kind != "work_event" {
        return Ok(None);
    }
    let event: WorkEvent = serde_json::from_value(object.clone())?;
    HistoryDisplay::load(store, &event, &entry.position).map(Some)
}

#[cfg(test)]
mod tests;
