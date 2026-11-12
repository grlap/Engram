//! Pure bootstrap checks; canonical resolution and complete admission stay in storage.

use super::{
    BootstrapMember, BootstrapMembers, CheckpointReason, CheckpointSeed, CustodyAuditRequest,
    CustodySeedSelection, validate,
};
use crate::{
    storage::StoreError,
    task_sync::{
        FORMAT, ProjectDocument, TaskSyncRefusal,
        inventory::InventoryRoot,
        ledger::{Binding, require},
    },
};

impl CustodyAuditRequest {
    /// Validate an explicitly named audit basis without inspecting any file.
    ///
    /// # Errors
    /// Refuses a malformed selection, origin-qualified cut or bootstrap request.
    pub fn validate(&self, binding: &Binding) -> Result<(), StoreError> {
        binding.validate()?;
        self.expected_local_cut.validate(&self.binding, binding)?;
        require(!self.operation.is_nil(), "missing custody audit operation")?;
        crate::task_sync::ledger::text(&self.key, 4096)?;
        if let CustodySeedSelection::AbandonRetainedConstruction {
            construction_fingerprint,
            ..
        } = &self.seed
        {
            crate::task_sync::ledger::fingerprint(construction_fingerprint)?;
        }
        if let CustodySeedSelection::IncomingCommit { commit, .. } = &self.seed {
            crate::task_sync::ledger::oid(commit)?;
        }
        if let CustodySeedSelection::IncomingDataCommit { commit }
        | CustodySeedSelection::AbandonFetchedCommit { commit, .. } = &self.seed
        {
            commit.validate()?;
            require(
                commit.retention_ref
                    == format!("refs/engram/task-sync/exchange/{}/commit", self.operation),
                "data bootstrap commit belongs to another operation",
            )?;
        }
        require(
            self.reason != CheckpointReason::Bootstrap
                || !matches!(
                    self.seed,
                    CustodySeedSelection::CustodyOnly
                        | CustodySeedSelection::AbandonFetchedCommit { .. }
                        | CustodySeedSelection::AbandonRetainedConstruction { .. }
                ),
            "bootstrap audit requires an explicit membership seed",
        )
    }

    /// Compare exact request content; this is never a checkpoint identity.
    ///
    /// # Errors
    /// Refuses a request that cannot be canonically encoded.
    pub fn fingerprint(&self) -> Result<crate::ObjectId, StoreError> {
        Ok(crate::CanonicalObject::freeze(self)?.key().clone())
    }
}

impl super::ManagedCustodyCheckpoint {
    pub(crate) fn abandoned_commit(&self) -> Option<&super::CustodyArtifactRef> {
        self.artifacts.iter().find(|artifact| {
            artifact.kind == super::CustodyArtifactKind::AbandonedFetchedCommit
                && artifact.operation == self.operation
        })
    }

    pub(crate) fn data_commit(&self) -> Option<&super::RetainedCommit> {
        let Some(CheckpointSeed::Incoming { snapshot, .. }) = &self.seed else {
            return None;
        };
        self.artifacts
            .iter()
            .any(|artifact| {
                artifact.kind == super::CustodyArtifactKind::IncomingDataCommit
                    && artifact.operation == self.operation
                    && artifact.record.is_none()
            })
            .then_some(&snapshot.commit)
    }
}

impl CheckpointSeed {
    pub(super) fn validate(&self, binding: &Binding) -> Result<(), StoreError> {
        let (output, members) = match self {
            Self::Incoming { snapshot, members } => {
                validate::snapshot(snapshot)?;
                require(
                    snapshot.membership.profile.include_notes == binding.profile.include_notes,
                    "incoming checkpoint note profile differs",
                )?;
                (&snapshot.membership, members)
            }
            Self::PreparedExport {
                membership,
                members,
            } => {
                validate::membership(membership)?;
                require(
                    membership.profile == binding.profile,
                    "export checkpoint profile differs",
                )?;
                require(
                    matches!(members, BootstrapMembers::Captured { .. }),
                    "export checkpoint requires captured membership",
                )?;
                (membership, members)
            }
        };
        let BootstrapMembers::Captured { members } = members else {
            // The initial outcome's kind and complete task set require storage.
            return Ok(());
        };
        require(
            members.len() <= validate::MAX_CHANGED_MEMBERS,
            "checkpoint member bound exceeded",
        )?;
        let mut root = InventoryRoot::ZERO;
        let mut all_embedded = true;
        for (id, member) in members {
            match member {
                BootstrapMember::Canonical { .. } => all_embedded = false,
                BootstrapMember::Document { task } => {
                    require(task.id == *id, "checkpoint task key differs")?;
                    root = root.including(task)?;
                }
            }
        }
        crate::task_sync::validate_selected_documents(
            &ProjectDocument {
                format: FORMAT.into(),
                project_id: binding.project.clone(),
                notes_included: output.profile.include_notes,
            },
            members.values().filter_map(|member| match member {
                BootstrapMember::Canonical { .. } => None,
                BootstrapMember::Document { task } => Some(task.as_ref()),
            }),
        )
        .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
        // With links, only storage can compute the complete root. Even a fully
        // embedded match still requires graph and exact snapshot admission.
        require(
            !all_embedded || root == output.inventory_root,
            "checkpoint embedded membership root differs",
        )
    }
}
