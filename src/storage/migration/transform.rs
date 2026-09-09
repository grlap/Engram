//! Explicit aggregate-format field transforms. Opaque prose is never remapped.

use serde::{Serialize, de::DeserializeOwned};
use serde_json::Value;

use crate::domain::{
    CompletionSeal, RequiredChildResolution, RootExecution, RootExecutionRef, WorkEvent,
    WorkObservation, WorkObservationBasis, WorkTransition,
};
use crate::{CanonicalObject, ObjectHash};

use super::{MigrationError, refused};

#[cfg(test)]
mod tests;

type Resolver<'a> = dyn FnMut(&ObjectHash) -> Result<ObjectHash, MigrationError> + 'a;

pub(super) fn strict<T: DeserializeOwned + Serialize>(value: &Value) -> Result<T, MigrationError> {
    let typed = serde_json::from_value::<T>(value.clone())?;
    if serde_json::to_value(&typed)? != *value {
        return Err(refused(
            "source object has unsupported or implicitly defaulted fields",
        ));
    }
    Ok(typed)
}

fn hashes(values: &mut [ObjectHash], resolve: &mut Resolver<'_>) -> Result<(), MigrationError> {
    for value in values {
        *value = resolve(value)?;
    }
    Ok(())
}

fn optional(
    value: &mut Option<ObjectHash>,
    resolve: &mut Resolver<'_>,
) -> Result<(), MigrationError> {
    if let Some(hash) = value {
        *hash = resolve(hash)?;
    }
    Ok(())
}

/// Rebinds only declared root-member object references, not actor or reason text.
///
/// # Errors
/// Returns the resolver's refusal for an absent or invalid mapping.
pub fn map_root_references(
    root: &mut RootExecution,
    resolve: &mut Resolver<'_>,
) -> Result<(), MigrationError> {
    hashes(&mut root.required_child_seals, resolve)?;
    for contribution in &mut root.contributions {
        contribution.object = resolve(&contribution.object)?;
    }
    Ok(())
}

/// Converts the known aggregate event shape using an already observed root cut.
/// Source bytes must have been verified and retained by the caller first.
///
/// # Errors
/// Refuses unknown shape, changed root presence, or unresolved typed references.
pub fn convert_event(
    mut source: Value,
    root: Option<RootExecutionRef>,
    resolve: &mut Resolver<'_>,
) -> Result<CanonicalObject, MigrationError> {
    let fields = source
        .as_object_mut()
        .ok_or_else(|| refused("event is not an object"))?;
    let old_root = fields
        .get("root_execution")
        .ok_or_else(|| refused("event has no root field"))?;
    if old_root.is_null() != root.is_none() {
        return Err(refused("event root presence changed"));
    }
    if !old_root.is_null() {
        strict::<RootExecution>(old_root)?;
    }
    fields.insert("root_execution".into(), serde_json::to_value(root)?);
    // The supported source contains native events from before this optional
    // field was written. Validate its existing read meaning, then preserve the
    // omission in the converted bytes; do not materialize unrelated defaults.
    let work = source
        .get_mut("work")
        .and_then(Value::as_object_mut)
        .ok_or_else(|| refused("event work is not an object"))?;
    let omitted_restored = !work.contains_key("restored");
    if omitted_restored {
        work.insert("restored".into(), Value::Bool(false));
    }
    let mut event: WorkEvent = strict(&source)?;
    optional(&mut event.work.source_snapshot_id, resolve)?;
    if let Some(run) = &mut event.run {
        optional(&mut run.last_checkpoint, resolve)?;
        optional(&mut run.completion_seal, resolve)?;
    }
    // Exhaustive matching makes a newly introduced transition an explicit
    // migration decision. Text that merely looks like a hash stays unchanged.
    match &mut event.transition {
        WorkTransition::Checkpointed { checkpoint }
        | WorkTransition::HandoffOffered { checkpoint, .. }
        | WorkTransition::HandedOff { checkpoint, .. } => *checkpoint = resolve(checkpoint)?,
        WorkTransition::EvidenceAdded { evidence }
        | WorkTransition::TypedEvidenceAdded { evidence, .. } => *evidence = resolve(evidence)?,
        WorkTransition::Completed { seal } => *seal = resolve(seal)?,
        WorkTransition::MemoryCaptured { version, assertion } => {
            *version = resolve(version)?;
            *assertion = resolve(assertion)?;
        }
        WorkTransition::Created { .. }
        | WorkTransition::Decomposed { .. }
        | WorkTransition::Revised { .. }
        | WorkTransition::PrerequisiteAdded { .. }
        | WorkTransition::PrerequisiteRemoved { .. }
        | WorkTransition::Blocked { .. }
        | WorkTransition::Unblocked { .. }
        | WorkTransition::Claimed { .. }
        | WorkTransition::ClaimRenewed { .. }
        | WorkTransition::Released { .. }
        | WorkTransition::HandoffExpired { .. }
        | WorkTransition::HandoffCancelled { .. }
        | WorkTransition::Disposed { .. }
        | WorkTransition::RequiredChildWaived { .. }
        | WorkTransition::Reopened { .. } => {}
    }
    match &mut event.transition {
        WorkTransition::HandoffOffered { offer, .. }
        | WorkTransition::HandedOff { offer, .. }
        | WorkTransition::HandoffExpired { offer, .. }
        | WorkTransition::HandoffCancelled { offer, .. } => *offer = resolve(offer)?,
        _ => {}
    }
    let mut output = serde_json::to_value(event)?;
    if omitted_restored {
        output
            .get_mut("work")
            .and_then(Value::as_object_mut)
            .ok_or_else(|| refused("converted work is not an object"))?
            .remove("restored");
    }
    CanonicalObject::freeze(&output).map_err(Into::into)
}

/// Converts a seal only after the caller verified its exact observed predecessor.
/// Removed copied accounting remains in the retained original and addressed root.
///
/// # Errors
/// Refuses unknown shape, missing copied accounting or unresolved typed links.
pub fn convert_seal(
    mut source: Value,
    root: RootExecutionRef,
    resolve: &mut Resolver<'_>,
) -> Result<CanonicalObject, MigrationError> {
    let fields = source
        .as_object_mut()
        .ok_or_else(|| refused("seal is not an object"))?;
    for field in ["expected_contributors", "contributions", "waivers"] {
        if fields.remove(field).is_none() {
            return Err(refused("aggregate seal is missing copied accounting"));
        }
    }
    if fields
        .insert("root_execution".into(), serde_json::to_value(root)?)
        .is_some()
    {
        return Err(refused("source seal already has a root reference"));
    }
    let omitted_restored = !fields.contains_key("restored");
    let omitted_children = !fields.contains_key("restored_child_completions");
    if omitted_restored {
        fields.insert("restored".into(), Value::Bool(false));
    }
    if omitted_children {
        fields.insert(
            "restored_child_completions".into(),
            Value::Array(Vec::new()),
        );
    }
    let mut seal: CompletionSeal = strict(&source)?;
    seal.accepted_work_revision_hash = resolve(&seal.accepted_work_revision_hash)?;
    optional(&mut seal.checkpoint, resolve)?;
    hashes(&mut seal.evidence, resolve)?;
    for criterion in &mut seal.acceptance {
        hashes(&mut criterion.evidence, resolve)?;
    }
    for obligation in &mut seal.obligations {
        obligation.definition = resolve(&obligation.definition)?;
        obligation.resolution = resolve(&obligation.resolution)?;
    }
    hashes(&mut seal.environment, resolve)?;
    hashes(&mut seal.required_child_seals, resolve)?;
    hashes(&mut seal.restored_child_completions, resolve)?;
    for resolution in &mut seal.required_child_resolutions {
        let RequiredChildResolution::ResolvedBySuccessor {
            supersession,
            successor_seal,
            ..
        } = resolution;
        *supersession = resolve(supersession)?;
        *successor_seal = resolve(successor_seal)?;
    }
    hashes(&mut seal.drain.reconciled_action_outcomes, resolve)?;
    let mut output = serde_json::to_value(seal)?;
    let fields = output
        .as_object_mut()
        .ok_or_else(|| refused("converted seal is not an object"))?;
    if omitted_restored {
        fields.remove("restored");
    }
    if omitted_children {
        fields.remove("restored_child_completions");
    }
    CanonicalObject::freeze(&output).map_err(Into::into)
}

/// Rebinds the declared observation basis; preserves summary, refs and attribution.
///
/// # Errors
/// Refuses unknown fields or missing basis mappings.
pub fn convert_observation(
    source: &Value,
    resolve: &mut Resolver<'_>,
) -> Result<CanonicalObject, MigrationError> {
    let mut observation: WorkObservation = strict(source)?;
    match &mut observation.basis {
        WorkObservationBasis::NativeEvent { event } => *event = resolve(event)?,
        WorkObservationBasis::RestoredRecord { record } => *record = resolve(record)?,
    }
    CanonicalObject::freeze(&observation).map_err(Into::into)
}
