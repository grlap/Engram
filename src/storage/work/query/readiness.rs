use std::collections::HashSet;

use rusqlite::{Connection, params};

use super::{RootExecution, StoreError, WorkId, WorkLifecycle, load_work_item};
use crate::ChildRequirement;

#[cfg(test)]
mod tests;

/// Advisory waiver membership from an already canonical-bound execution.
/// Validate the current child bindings, but do not replay root history while
/// shaping guidance. Completion and doctor retain the exhaustive event proof.
pub(in crate::storage::work) fn current_required_child_waivers(
    connection: &Connection,
    parent_id: WorkId,
    execution: &RootExecution,
) -> Result<HashSet<WorkId>, StoreError> {
    let mut seen = HashSet::new();
    let mut direct = HashSet::new();
    for waiver in &execution.required_child_waivers {
        let child = load_work_item(connection, waiver.work_id)?;
        if !seen.insert(waiver.work_id)
            || child.root_id != execution.root_id
            || child.project_id != execution.project_id
            || child.parent_id.is_none()
            || child.child_requirement != ChildRequirement::Required
            || !matches!(
                child.lifecycle,
                WorkLifecycle::Cancelled | WorkLifecycle::Superseded
            )
            || child.revision != waiver.work_revision
            || waiver.waived_by.trim().is_empty()
            || waiver.reason.trim().is_empty()
        {
            return Err(StoreError::InvalidWorkProjection(format!(
                "root execution {:?} has an invalid current required-child waiver for {:?}",
                execution.root_execution_id, waiver.work_id
            )));
        }
        if child.parent_id == Some(parent_id) {
            direct.insert(child.work_id);
        }
    }
    Ok(direct)
}

/// Suggestion only, never completion authority. Indexed current child/run/seal
/// bindings and the verified execution snapshot answer whether to offer done;
/// the completion transaction still verifies every canonical seal and waiver.
pub(super) fn required_children_ready(
    connection: &Connection,
    parent_id: WorkId,
    execution: &RootExecution,
) -> Result<bool, StoreError> {
    let mut waived = current_required_child_waivers(connection, parent_id, execution)?
        .into_iter()
        .collect::<Vec<_>>();
    waived.sort_unstable_by_key(|work_id| work_id.0);
    let waived_json = serde_json::to_string(&waived)?;
    let seals_json = serde_json::to_string(&execution.required_child_seals)?;
    Ok(connection.query_row(
        "SELECT NOT EXISTS (
             SELECT 1 FROM work_items child
             WHERE child.parent_id = ?1 AND child.child_requirement = 'required'
               AND NOT EXISTS (
                   SELECT 1 FROM json_each(?3) waiver
                   WHERE waiver.value = child.work_id
                     AND child.lifecycle IN ('cancelled', 'superseded')
               )
               AND NOT EXISTS (
                   SELECT 1 FROM work_runs run
                   JOIN work_completion_seals seal ON seal.run_id = run.run_id
                   WHERE run.work_id = child.work_id
                     AND child.lifecycle = 'completed' AND run.state = 'completed'
                     AND run.root_execution_id = ?2 AND seal.root_execution_id = ?2
                     AND seal.work_id = child.work_id
                     AND seal.seal_hash = run.completion_seal_hash
                     AND run.generation = (
                         SELECT MAX(latest.generation) FROM work_runs latest
                         WHERE latest.work_id = child.work_id
                     )
                     AND seal.seal_hash IN (SELECT value FROM json_each(?4))
               )
         )",
        params![
            parent_id.0.to_string(),
            execution.root_execution_id.0.to_string(),
            waived_json,
            seals_json
        ],
        |row| row.get(0),
    )?)
}
