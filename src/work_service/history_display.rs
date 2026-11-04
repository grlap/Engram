//! Event facts for receipt history, read once and shared by the transient
//! display and the stored delivery summary, whose bytes stay fixed.

use super::{
    MAX_HISTORY_BLOCKER_DETAIL_BYTES, MAX_HISTORY_DETAIL_BYTES, MAX_HISTORY_TITLE_BYTES,
    WorkBlockerKind, WorkChangeSummary, WorkEvidenceKind, WorkLifecycle, blocker_selector,
    compact_text, compact_text_to, projected_actor_context,
};

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
        // A clear event carries only the blocker's id; the blocker it removed
        // is read from its retained row, checked against the event that raised
        // it, never from the active set or a row position.
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

    /// The stored delivery summary, built from the same facts as the
    /// transient display, so neither reads the planning or blocker rows twice.
    pub(super) fn stored_summary(&self) -> super::WorkChangeSummary {
        agent_work_event_summary_with(&self.event, &self.fields, self.cleared.as_ref())
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
        T::Blocked { blocker_id } => history_blocker(blocker_id, event.blocker.as_ref()),
        T::Unblocked { blocker_id } => {
            format!("cleared {}", history_blocker(blocker_id, cleared))
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
        T::SupersededByImport { reason, .. } => {
            format!("retired by import without successful completion because {reason}")
        }
        T::ReconciliationHoldAbandoned { reason, .. } => {
            format!("reconciliation hold abandoned because {reason}")
        }
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

pub(super) fn project_work_event(
    store: &SqliteStore,
    event: &WorkEvent,
    position: &FeedPosition,
) -> Result<WorkChangeSummary, StoreError> {
    Ok(HistoryDisplay::load(store, event, position)?.stored_summary())
}

#[cfg(test)]
pub(super) fn agent_work_event_summary(
    event: &WorkEvent,
    revised_fields: &[&str],
) -> WorkChangeSummary {
    agent_work_event_summary_with(event, revised_fields, None)
}

fn agent_work_event_summary_with(
    event: &WorkEvent,
    revised_fields: &[&str],
    cleared: Option<&crate::WorkBlocker>,
) -> WorkChangeSummary {
    let change_kind = work_transition_kind(&event.transition);
    WorkChangeSummary {
        schema_version: event.schema_version,
        object_kind: "work_event".into(),
        work_id: Some(event.work_id),
        work_ref: Some(event.work.short_ref.clone()),
        revision: Some(event.revision),
        change_kind: change_kind.into(),
        summary: compact_text(&format!(
            "{change_kind}: {}",
            work_transition_summary(event, revised_fields, cleared)
        )),
        actor_id: Some(compact_text(&event.actor.actor_id)),
        actor_context: projected_actor_context(&event.actor),
        created_at: event.created_at,
    }
}

/// One blocker as history names it: its selector first, whole, then its kind
/// and a bounded preview of the detail it was raised with.
fn history_blocker(blocker_id: &str, blocker: Option<&crate::WorkBlocker>) -> String {
    let selector = blocker_selector::encode(blocker_id);
    blocker.map_or_else(
        || format!("blocker {selector}"),
        |blocker| {
            format!(
                "blocker {selector} ({}) \"{}\"",
                blocker_kind_word(blocker.kind),
                compact_text_to(&blocker.detail, MAX_HISTORY_BLOCKER_DETAIL_BYTES)
            )
        },
    )
}

/// The stored spelling of a blocker kind, as `show` names it beside a
/// blocker's selector.
pub(crate) fn blocker_kind_word(kind: WorkBlockerKind) -> &'static str {
    match kind {
        WorkBlockerKind::Manual => "manual",
        WorkBlockerKind::HumanDecision => "human_decision",
        WorkBlockerKind::ExternalInput => "external_input",
        WorkBlockerKind::Policy => "policy",
    }
}

fn work_transition_summary(
    event: &WorkEvent,
    revised_fields: &[&str],
    cleared: Option<&crate::WorkBlocker>,
) -> String {
    let title = compact_text_to(&event.work.title, MAX_HISTORY_TITLE_BYTES);
    let detail = |value: &str| compact_text_to(value, MAX_HISTORY_DETAIL_BYTES);
    match &event.transition {
        WorkTransition::Created { .. }
            if event
                .actor
                .provenance_chain
                .iter()
                .any(crate::domain::is_peer_child_proposal_marker) =>
        {
            format!("peer optional-child proposal: \"{title}\"")
        }
        WorkTransition::Created { prerequisites } if prerequisites.is_empty() => {
            format!("without prerequisites: \"{title}\"")
        }
        WorkTransition::Created { prerequisites } => {
            format!("with {} prerequisite(s): \"{title}\"", prerequisites.len())
        }
        WorkTransition::Decomposed { children, .. } => {
            format!("added {} child item(s): \"{title}\"", children.len())
        }
        WorkTransition::Revised { .. } => {
            let fields = if revised_fields.is_empty() {
                "no planning change".into()
            } else {
                revised_fields.join(", ")
            };
            format!("{fields}: \"{title}\"")
        }
        WorkTransition::PrerequisiteAdded { .. } => {
            format!("added one prerequisite: \"{title}\"")
        }
        WorkTransition::PrerequisiteRemoved { .. } => {
            format!("removed one prerequisite: \"{title}\"")
        }
        WorkTransition::Blocked { blocker_id } => format!(
            "{}: \"{title}\"",
            history_blocker(blocker_id, event.blocker.as_ref())
        ),
        WorkTransition::Unblocked { blocker_id } => format!(
            "cleared {}: \"{title}\"",
            history_blocker(blocker_id, cleared)
        ),
        WorkTransition::Claimed {
            recovered: true, ..
        } => format!("after recovery by a session: \"{title}\""),
        WorkTransition::Claimed {
            recovered: false, ..
        } => format!("by a session: \"{title}\""),
        WorkTransition::ClaimRenewed { .. } => format!("renewed by its holder: \"{title}\""),
        WorkTransition::Released { reason, .. }
        | WorkTransition::HandoffCancelled { reason, .. }
        | WorkTransition::Reopened { reason, .. }
        | WorkTransition::SupersededByImport { reason, .. }
        | WorkTransition::ReconciliationHoldAbandoned { reason, .. } => {
            format!("because {}: \"{title}\"", detail(reason))
        }
        WorkTransition::Checkpointed { .. } => format!("progress: \"{title}\""),
        WorkTransition::HandoffOffered { .. } => {
            format!("to another session: \"{title}\"")
        }
        WorkTransition::HandoffExpired { .. } => format!("expired: \"{title}\""),
        WorkTransition::HandedOff { .. } => {
            format!("from one session to another: \"{title}\"")
        }
        WorkTransition::EvidenceAdded { .. } => format!("for: \"{title}\""),
        WorkTransition::MemoryCaptured { .. } => {
            format!("shared work memory: \"{title}\"")
        }
        WorkTransition::TypedEvidenceAdded { evidence_kind, .. } => {
            format!(
                "{} evidence: \"{title}\"",
                work_evidence_kind_word(*evidence_kind)
            )
        }
        WorkTransition::Completed { .. } => format!("\"{title}\""),
        WorkTransition::Disposed {
            lifecycle, reason, ..
        } => format!(
            "to {} because {}: \"{title}\"",
            work_lifecycle_word(*lifecycle),
            detail(reason)
        ),
        WorkTransition::RequiredChildWaived { reason, .. } => {
            format!(
                "waived one required child because {}: \"{title}\"",
                detail(reason)
            )
        }
    }
}

pub(super) fn work_evidence_kind_word(kind: WorkEvidenceKind) -> &'static str {
    kind.word()
}

pub(super) fn work_lifecycle_word(lifecycle: WorkLifecycle) -> &'static str {
    lifecycle.word()
}

fn work_transition_kind(transition: &WorkTransition) -> &'static str {
    match transition {
        WorkTransition::Created { .. } => "created",
        WorkTransition::Decomposed { .. } => "decomposed",
        WorkTransition::Revised { .. } => "revised",
        WorkTransition::PrerequisiteAdded { .. } => "prerequisite_added",
        WorkTransition::PrerequisiteRemoved { .. } => "prerequisite_removed",
        WorkTransition::Blocked { .. } => "blocked",
        WorkTransition::Unblocked { .. } => "unblocked",
        WorkTransition::Claimed { .. } => "claimed",
        WorkTransition::ClaimRenewed { .. } => "claim_renewed",
        WorkTransition::Released { .. } => "released",
        WorkTransition::Checkpointed { .. } => "checkpointed",
        WorkTransition::HandoffOffered { .. } => "handoff_offered",
        WorkTransition::HandoffExpired { .. } => "handoff_expired",
        WorkTransition::HandoffCancelled { .. } => "handoff_cancelled",
        WorkTransition::HandedOff { .. } => "handed_off",
        WorkTransition::EvidenceAdded { .. } => "evidence_added",
        WorkTransition::MemoryCaptured { .. } => "memory_captured",
        WorkTransition::TypedEvidenceAdded { .. } => "typed_evidence_added",
        WorkTransition::Completed { .. } => "completed",
        WorkTransition::Disposed { .. } => "disposed",
        WorkTransition::RequiredChildWaived { .. } => "required_child_waived",
        WorkTransition::Reopened { .. } => "reopened",
        WorkTransition::SupersededByImport { .. } => "superseded_by_import",
        WorkTransition::ReconciliationHoldAbandoned { .. } => "reconciliation_hold_abandoned",
    }
}
