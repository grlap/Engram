//! Encode observed aggregate states as the current canonical root-delta model.

use std::collections::BTreeMap;

use crate::domain::{
    RootExecutionDelta, RootExecutionHeader, RootExecutionMember, RootExecutionRef,
};
use crate::{CanonicalObject, ObjectHash, RootExecution};

use super::{MigrationError, refused};

#[cfg(test)]
mod tests;

/// New current-format objects and the address of their complete observed state.
#[derive(Clone, Debug)]
pub struct EncodedRoot {
    pub reference: RootExecutionRef,
    pub objects: Vec<CanonicalObject>,
}

/// One root execution's ordered conversion. References inside observed members
/// must already have been explicitly mapped by the versioned importer. Original
/// aggregate bytes remain in migration provenance; only set order is normalized.
#[derive(Default)]
pub struct RootHistoryEncoder {
    state: Option<RootExecution>,
    reference: Option<RootExecutionRef>,
    sequence: u64,
}

impl RootHistoryEncoder {
    /// Encodes an observed state without making up any of its header fields.
    /// A new empty delta origin is a representation detail, not an assertion
    /// that the original host emitted an empty-state event.
    ///
    /// # Errors
    /// Refuses duplicate members, cross-root identity, changed origin metadata
    /// or backward revisions. Equal snapshots reuse their existing address.
    pub fn push(&mut self, mut value: RootExecution) -> Result<EncodedRoot, MigrationError> {
        normalize(&mut value)?;
        let metadata = header(&value);
        let mut objects = Vec::new();
        if self.state.is_none() {
            let mut empty = value.clone();
            empty.run_ids.clear();
            empty.required_child_seals.clear();
            empty.required_child_waivers.clear();
            empty.expected_contributors.clear();
            empty.contributions.clear();
            empty.waivers.clear();
            let origin = CanonicalObject::freeze(&RootExecutionDelta {
                header: metadata.clone(),
                sequence: 0,
                predecessor: None,
                previous_revision: None,
                removed: Vec::new(),
                added: Vec::new(),
                state_checksum: CanonicalObject::freeze(&empty)?.hash().clone(),
            })?;
            self.reference = Some(address(&metadata, origin.hash().clone()));
            self.state = Some(empty);
            objects.push(origin);
        }
        let prior = self
            .state
            .as_ref()
            .ok_or_else(|| refused("root encoder has no state"))?;
        let prior_ref = self
            .reference
            .as_ref()
            .ok_or_else(|| refused("root encoder has no address"))?;
        if address(&metadata, prior_ref.head.clone()) != *prior_ref
            || value.schema_version != prior.schema_version
            || value.created_at != prior.created_at
            || value.revision < prior.revision
        {
            return Err(refused(
                "observed root identity, origin or revision changed illegally",
            ));
        }
        if prior != &value {
            let old = members(prior)?;
            let new = members(&value)?;
            let sequence = self
                .sequence
                .checked_add(1)
                .ok_or_else(|| refused("root sequence overflow"))?;
            let delta = CanonicalObject::freeze(&RootExecutionDelta {
                header: metadata.clone(),
                sequence,
                predecessor: Some(prior_ref.head.clone()),
                previous_revision: Some(prior.revision),
                removed: old
                    .iter()
                    .filter(|(hash, _)| !new.contains_key(*hash))
                    .map(|(_, member)| member.clone())
                    .collect(),
                added: new
                    .iter()
                    .filter(|(hash, _)| !old.contains_key(*hash))
                    .map(|(_, member)| member.clone())
                    .collect(),
                state_checksum: CanonicalObject::freeze(&value)?.hash().clone(),
            })?;
            self.reference = Some(address(&metadata, delta.hash().clone()));
            self.state = Some(value);
            self.sequence = sequence;
            objects.push(delta);
        }
        Ok(EncodedRoot {
            reference: self
                .reference
                .clone()
                .ok_or_else(|| refused("root encoder lost its address"))?,
            objects,
        })
    }
}

pub(super) fn header(value: &RootExecution) -> RootExecutionHeader {
    RootExecutionHeader {
        schema_version: value.schema_version,
        root_execution_id: value.root_execution_id,
        project_id: value.project_id.clone(),
        root_id: value.root_id,
        generation: value.generation,
        state: value.state,
        revision: value.revision,
        created_at: value.created_at,
        updated_at: value.updated_at,
    }
}

fn address(header: &RootExecutionHeader, head: ObjectHash) -> RootExecutionRef {
    RootExecutionRef {
        root_execution_id: header.root_execution_id,
        project_id: header.project_id.clone(),
        root_id: header.root_id,
        generation: header.generation,
        head,
    }
}

pub(super) fn members(
    value: &RootExecution,
) -> Result<BTreeMap<ObjectHash, RootExecutionMember>, MigrationError> {
    let values = value
        .run_ids
        .iter()
        .copied()
        .map(RootExecutionMember::Run)
        .chain(
            value
                .required_child_seals
                .iter()
                .cloned()
                .map(RootExecutionMember::ChildSeal),
        )
        .chain(
            value
                .required_child_waivers
                .iter()
                .cloned()
                .map(RootExecutionMember::ChildWaiver),
        )
        .chain(
            value
                .expected_contributors
                .iter()
                .cloned()
                .map(RootExecutionMember::Contributor),
        )
        .chain(
            value
                .contributions
                .iter()
                .cloned()
                .map(RootExecutionMember::Contribution),
        )
        .chain(
            value
                .waivers
                .iter()
                .cloned()
                .map(RootExecutionMember::Waiver),
        );
    let mut result = BTreeMap::new();
    for member in values {
        if result
            .insert(CanonicalObject::freeze(&member)?.hash().clone(), member)
            .is_some()
        {
            return Err(refused("duplicate observed root member"));
        }
    }
    Ok(result)
}

pub(super) fn normalize(value: &mut RootExecution) -> Result<(), MigrationError> {
    if value.schema_version != crate::domain::SCHEMA_VERSION
        || value.generation < 1
        || value.revision < 1
    {
        return Err(refused(
            "invalid observed root version, generation or revision",
        ));
    }
    value.run_ids.sort_by_key(|run| run.0);
    value.required_child_seals.sort();
    value
        .required_child_waivers
        .sort_by_key(|waiver| waiver.work_id.0);
    value.expected_contributors.sort_by(|a, b| a.0.cmp(&b.0));
    value.contributions.sort_by(|a, b| {
        a.participant
            .0
            .cmp(&b.participant.0)
            .then_with(|| a.object.cmp(&b.object))
    });
    value
        .waivers
        .sort_by(|a, b| a.participant.0.cmp(&b.participant.0));
    members(value)?;
    if value
        .required_child_waivers
        .windows(2)
        .any(|pair| pair[0].work_id == pair[1].work_id)
        || value
            .waivers
            .windows(2)
            .any(|pair| pair[0].participant == pair[1].participant)
    {
        return Err(refused(
            "multiple root facts have the same logical member key",
        ));
    }
    Ok(())
}
