//! Admission and application of the scalar edits a resolved folder asks for.

use std::fmt::Write as _;

use super::{origin, query};
use crate::{
    ActorContext, WorkId, WorkLifecycle, WorkOrigin,
    domain::{WorkPlanningAuthority, WorkTransition},
    storage::StoreError,
    task_sync::{
        TaskSyncRefusal,
        folder_plan::{DeltaPlan, TaskDelta, TaskField, UnsupportedChange},
    },
};

/// Why one task the folder would change cannot take that change here.
struct Conflict {
    id: WorkId,
    reason: &'static str,
    detail: String,
}

/// Applies a classified folder whole, or refuses it before any write. Every
/// requested delta is judged first: an unsupported delta and an edit its task
/// cannot take are both conflicts, and the refusal names each one in task-id
/// order, so a single correction of the folder can address them all.
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

/// Whether a transition changes only planning, never execution.
const fn is_planning(transition: &WorkTransition) -> bool {
    matches!(
        transition,
        WorkTransition::Created { .. }
            | WorkTransition::Decomposed { .. }
            | WorkTransition::Revised { .. }
            | WorkTransition::PrerequisiteAdded { .. }
            | WorkTransition::PrerequisiteRemoved { .. }
            | WorkTransition::Blocked { .. }
            | WorkTransition::Unblocked { .. }
            | WorkTransition::Disposed { .. }
            | WorkTransition::RequiredChildWaived { .. }
    )
}

fn refusal(plan: &DeltaPlan, conflicts: &[Conflict]) -> StoreError {
    let mut message = String::from("task folder import refuses the selected changes:");
    for change in &plan.project {
        let _ = write!(message, " project: {};", change.code());
    }
    for conflict in conflicts {
        let _ = write!(
            message,
            " {}: {} ({});",
            conflict.id.0, conflict.reason, conflict.detail
        );
    }
    message.pop();
    TaskSyncRefusal::ImportConflict.invalid(message)
}
