//! Immutable trigger facts, independent of the native or retained feed index.
use crate::domain::{SourceObservation, WorkEvent, WorkObligation};

pub(crate) fn binding_trigger_matches(
    obligation: &WorkObligation,
    trigger: &WorkEvent,
    criterion: usize,
) -> bool {
    trigger.project_id == obligation.project_id
        && trigger.root_id == obligation.root_id
        && trigger.work_id == obligation.work_id
        && trigger.run_id == Some(obligation.run_id)
        && trigger.revision == obligation.work_revision
        && trigger.created_at == obligation.opened_at
        && trigger.work.acceptance_bindings.iter().any(|binding| {
            binding.criterion == criterion && binding.requirement == obligation.requirement
        })
}

pub(crate) fn source_trigger_matches(
    obligation: &WorkObligation,
    trigger: &SourceObservation,
) -> bool {
    trigger.source_changed
        && trigger.project_id == obligation.project_id
        && trigger.binding.root_execution_id == obligation.root_execution_id
        && trigger.binding.work_id == obligation.work_id
        && trigger.binding.run_id == obligation.run_id
        && trigger.binding.work_revision == obligation.work_revision
        && trigger.recorded_at == obligation.opened_at
}
