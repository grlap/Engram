use super::{
    ChildRequirement, CompletionSeal, Connection, HashSet, StoreError, WorkEvent, WorkId, WorkItem,
    WorkLifecycle, load_typed_work_object,
};
use crate::RequiredChildResolution;

#[cfg(test)]
mod tests;

/// Validate only immutable facts cited at this seal's cut. Current lifecycle
/// changes (including reopen) must not reinterpret previously sealed credit.
/// Successor proofs are members of the ordinary recursively checked seal set.
pub(super) fn validate_resolutions_on(
    connection: &Connection,
    seal: &CompletionSeal,
    accounted: &mut HashSet<WorkId>,
) -> Result<(), StoreError> {
    for resolution in &seal.required_child_resolutions {
        let RequiredChildResolution::ResolvedBySuccessor {
            work_id,
            work_revision,
            supersession,
            successor,
            successor_seal,
        } = resolution;
        let event: WorkEvent = load_typed_work_object(connection, supersession, "work_event")?;
        let proof: CompletionSeal =
            load_typed_work_object(connection, successor_seal, "completion_seal")?;
        let accepted: WorkItem = load_typed_work_object(
            connection,
            &proof.accepted_work_revision_hash,
            "work_item_revision",
        )?;
        if !accounted.insert(*work_id)
            || seal
                .required_child_waivers
                .iter()
                .any(|waiver| waiver.work_id == *work_id)
            || !seal.required_child_seals.contains(successor_seal)
            || event.work_id != *work_id
            || event.revision != *work_revision
            || event.work.parent_id != Some(seal.work_id)
            || event.root_id != seal.root_id
            || *work_id == *successor
            || !super::super::child_resolution::supersession_binds(
                &event,
                &event.work,
                *successor,
                seal.root_execution_id,
            )
            || proof.work_id != *successor
            || proof.root_execution_id != seal.root_execution_id
            || proof.root_id != seal.root_id
            || accepted.work_id != *successor
            || accepted.parent_id != Some(seal.work_id)
            || accepted.root_id != seal.root_id
            || accepted.project_id != event.project_id
            || accepted.child_requirement != ChildRequirement::Required
            || accepted.lifecycle != WorkLifecycle::Open
            || accepted.revision != proof.accepted_work_revision
            || accepted.active_run_id != Some(proof.run_id)
        {
            return Err(StoreError::InvalidWorkProjection(
                "required-child successor resolution differs from its immutable supersession or sibling seal binding".into(),
            ));
        }
    }
    Ok(())
}
