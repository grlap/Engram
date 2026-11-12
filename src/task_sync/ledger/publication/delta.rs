//! Shape checks only. Storage and Git admission must establish the actual
//! merged graph and bytes before this recipe becomes a writable transition.

use super::{OutgoingFailure, OutgoingReconciliation};
use crate::task_sync::{
    delta::{DeltaBasis, DeltaCommitSelection, DeltaIntent, PreparedExportAnchor},
    ledger::{Binding, Direction, RetainedCommit},
};
use crate::{ObjectId, StoreError};

impl OutgoingReconciliation {
    pub(crate) fn validate_delta_resolution(
        &self,
        binding_id: &ObjectId,
        binding: &Binding,
        prepared: &ObjectId,
        original: &DeltaIntent,
        original_commit: &RetainedCommit,
    ) -> Result<(), StoreError> {
        let Self::DeltaResolved {
            failure,
            resolved,
            payload,
            commit,
        } = self
        else {
            return Err(crate::task_sync::invalid(
                "delta reconciliation recipe is missing",
            ));
        };
        payload.validate(binding_id, binding, resolved)?;
        DeltaResolutionRecipe {
            failure,
            resolved,
            commit,
        }
        .validate(binding_id, binding, prepared, original, original_commit)
    }
}

/// Comparison fields shared by the reducer and historical metadata reads.
/// This alone does not validate retained payload bytes or admit a graph.
pub(crate) struct DeltaResolutionRecipe<'a> {
    pub failure: &'a OutgoingFailure,
    pub resolved: &'a DeltaIntent,
    pub commit: &'a RetainedCommit,
}

impl DeltaResolutionRecipe<'_> {
    pub(crate) fn validate(
        &self,
        binding_id: &ObjectId,
        binding: &Binding,
        prepared: &ObjectId,
        original: &DeltaIntent,
        original_commit: &RetainedCommit,
    ) -> Result<(), StoreError> {
        let Self {
            failure,
            resolved,
            commit,
        } = *self;
        DeltaCommitSelection {
            prepared: prepared.clone(),
            commit: commit.clone(),
        }
        .validate(binding_id, binding, resolved, prepared)?;
        let (
            DeltaBasis::Outgoing {
                confirmed_before: original_confirmed,
                ..
            },
            DeltaBasis::Outgoing {
                export_before,
                confirmed_before,
            },
        ) = (&original.basis, &resolved.basis)
        else {
            return Err(crate::task_sync::invalid(
                "delta reconciliation requires outgoing bases",
            ));
        };
        super::super::require(
            original.direction == Direction::Outgoing
                && resolved.operation != original.operation
                && resolved.key != original.key
                && resolved.actor == original.actor
                && resolved.producer == original.producer
                && resolved.local_cut == original.local_cut
                && resolved.common_commit == original.common_commit
                && export_before.anchor
                    == (PreparedExportAnchor::Prepared {
                        event: prepared.clone(),
                    })
                && export_before.membership == original.candidate.output
                && export_before.cut == original.local_cut
                && confirmed_before == original_confirmed
                && resolved.candidate.directory != original.candidate.directory,
            "resolved delta must retain its original native preparation and attribution",
        )?;
        super::super::require(
            resolved.expected_head.as_deref() == Some(original_commit.oid.as_str())
                && commit.oid.len() == original_commit.oid.len()
                && commit.oid != original_commit.oid
                && commit.parents
                    == [
                        original_commit.oid.clone(),
                        failure.fetched_tip().to_owned(),
                    ]
                && original_commit.oid != failure.fetched_tip(),
            "delta reconciliation must retain both original data parents",
        )
    }
}
