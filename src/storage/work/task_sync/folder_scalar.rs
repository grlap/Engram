//! Admission and application of the scalar edits a resolved folder asks for.

use super::{origin, query};
use crate::{
    ActorContext, WorkId, WorkLifecycle, WorkOrigin,
    domain::{WorkPlanningAuthority, WorkTransition},
    storage::StoreError,
    task_sync::{
        TaskSyncRefusal,
        folder_plan::{DeltaPlan, ProjectChange, TaskDelta, TaskField, UnsupportedChange},
    },
};

/// How many refused tasks a refusal names before it counts the rest.
pub(super) const NAMED_CONFLICT_LIMIT: usize = 20;

/// Why one task the folder would change cannot take that change here.
pub(super) struct Conflict {
    pub(super) id: WorkId,
    pub(super) reason: &'static str,
    pub(super) detail: String,
}

/// Applies a classified folder whole, or refuses it before any write. Every
/// requested delta is judged first: an unsupported delta and an edit its task
/// cannot take are both conflicts. The refusal names the first ones in
/// task-id order, up to a fixed limit, and gives the exact count of the rest.
pub(super) fn apply_plan(
    transaction: &rusqlite::Transaction<'_>,
    plan: &DeltaPlan,
    actor: &ActorContext,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(), StoreError> {
    let mut conflicts = Vec::new();
    for task in &plan.tasks {
        match &task.delta {
            TaskDelta::Unsupported(change) => conflicts.push(unsupported(task.id, change)),
            TaskDelta::Scalar(_) => {
                if let Some(conflict) = admit(transaction, task.id)? {
                    conflicts.push(conflict);
                }
            }
            TaskDelta::Unchanged | TaskDelta::RetainedLocal => {}
        }
    }
    if !plan.project.is_empty() || !conflicts.is_empty() {
        return Err(refusal(plan, &conflicts));
    }
    for (id, change) in plan.scalar_changes() {
        super::super::planning::apply_folder_scalar_revision_on(
            transaction,
            id,
            change,
            actor,
            now,
        )?;
    }
    Ok(())
}

fn unsupported(id: WorkId, change: &UnsupportedChange) -> Conflict {
    Conflict {
        id,
        reason: change.code(),
        detail: match change {
            UnsupportedChange::Added => "not held by this destination".into(),
            UnsupportedChange::Deleted => "omitted from the folder".into(),
            UnsupportedChange::Fields(fields) => fields
                .iter()
                .copied()
                .map(TaskField::as_str)
                .collect::<Vec<_>>()
                .join(", "),
        },
    }
}

/// An edited task takes a scalar change only when nothing binds its current
/// revision: a task restored by a task-sync import, Open, with no run, whose
/// history holds only run-free planning revisions. Claims, handoffs,
/// evaluations, obligations and seals all attach to a run of the task itself,
/// so a run-free task carries none; other tasks' execution does not bind it.
fn admit(
    transaction: &rusqlite::Transaction<'_>,
    id: WorkId,
) -> Result<Option<Conflict>, StoreError> {
    let conflict = |reason: &'static str, detail: String| -> Result<Option<Conflict>, StoreError> {
        Ok(Some(Conflict { id, reason, detail }))
    };
    let item = query::load_work_item(transaction, id).map_err(origin::classify_binding)?;
    if !item.restored
        || item.origin != WorkOrigin::Local
        || item.source_snapshot_id.is_some()
        || origin::for_item(transaction, &item)
            .map_err(origin::classify_binding)?
            .is_none()
    {
        return conflict(
            "unsupported_origin",
            "not a task restored by a task-sync import".into(),
        );
    }
    match item.lifecycle {
        WorkLifecycle::Open => {}
        WorkLifecycle::Completed => {
            return conflict("restored_completion", "the task is completed".into());
        }
        other => {
            return conflict("lifecycle", format!("the task is {other:?}"));
        }
    }
    if let Some(run) = item.active_run_id {
        return conflict("active_run", format!("run {}", run.0));
    }
    for (event_id, event) in query::canonical_work_event_records_for_item(transaction, id)? {
        let run_bound = event.run_id.is_some()
            || event.run.is_some()
            || event.claim.is_some()
            || event.handoff_offer.is_some();
        if run_bound || !is_planning(&event.transition) {
            return conflict("execution_history", format!("event {}", event_id.as_str()));
        }
        if !matches!(
            event.transition,
            WorkTransition::Revised {
                authority: WorkPlanningAuthority::Project
            }
        ) {
            return conflict("planning_history", format!("event {}", event_id.as_str()));
        }
    }
    Ok(None)
}

/// Whether a transition changes only planning, never execution. Every
/// variant is named, so a new one cannot compile until it is classified.
const fn is_planning(transition: &WorkTransition) -> bool {
    match transition {
        WorkTransition::Created { .. }
        | WorkTransition::Decomposed { .. }
        | WorkTransition::Revised { .. }
        | WorkTransition::PrerequisiteAdded { .. }
        | WorkTransition::PrerequisiteRemoved { .. }
        | WorkTransition::Blocked { .. }
        | WorkTransition::Unblocked { .. }
        | WorkTransition::Disposed { .. } => true,
        // Execution, including a required-child waiver: it accounts for the
        // root's execution and can be recorded without a run of its own.
        WorkTransition::Claimed { .. }
        | WorkTransition::ClaimRenewed { .. }
        | WorkTransition::Released { .. }
        | WorkTransition::Checkpointed { .. }
        | WorkTransition::HandoffOffered { .. }
        | WorkTransition::HandoffExpired { .. }
        | WorkTransition::HandoffCancelled { .. }
        | WorkTransition::HandedOff { .. }
        | WorkTransition::EvidenceAdded { .. }
        | WorkTransition::MemoryCaptured { .. }
        | WorkTransition::TypedEvidenceAdded { .. }
        | WorkTransition::Completed { .. }
        | WorkTransition::RequiredChildWaived { .. }
        | WorkTransition::Reopened { .. } => false,
    }
}

fn refusal(plan: &DeltaPlan, conflicts: &[Conflict]) -> StoreError {
    TaskSyncRefusal::ImportConflict.invalid(conflict_message(&plan.project, conflicts))
}

/// The refusal text: every project change, then the first refused tasks in
/// the order given, up to the limit, then the exact count of the rest.
pub(super) fn conflict_message(project: &[ProjectChange], conflicts: &[Conflict]) -> String {
    let mut entries = project
        .iter()
        .map(|change| format!("project: {}", change.code()))
        .chain(conflicts.iter().take(NAMED_CONFLICT_LIMIT).map(|conflict| {
            format!(
                "{}: {} ({})",
                conflict.id.0, conflict.reason, conflict.detail
            )
        }))
        .collect::<Vec<_>>();
    match conflicts.len().saturating_sub(NAMED_CONFLICT_LIMIT) {
        0 => {}
        1 => entries.push("and 1 more refused task".into()),
        rest => entries.push(format!("and {rest} more refused tasks")),
    }
    format!(
        "task folder import refuses the selected changes: {}",
        entries.join("; ")
    )
}
