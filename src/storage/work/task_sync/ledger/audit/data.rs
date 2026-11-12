//! A cold folder-envelope seed has its own checkpoint-owned Git pin.

use super::{
    BTreeMap, Binding, CanonicalObject, CheckpointSeed, CustodyArtifactKind, CustodyArtifactRef,
    CustodyAuditRequest, CustodySeedSelection, Ledger, MembershipSnapshot, StoreError, Transition,
    embedded, invalid, membership, require, retention, task_sync,
};
use crate::task_sync::ledger::RetainedCommit;
use std::path::Path;

pub(super) fn selection(request: &CustodyAuditRequest) -> Option<&RetainedCommit> {
    match &request.seed {
        CustodySeedSelection::IncomingDataCommit { commit } => Some(commit),
        _ => None,
    }
}

pub(super) fn seed(
    binding: &Binding,
    operation: uuid::Uuid,
    commit: &RetainedCommit,
) -> Result<CheckpointSeed, StoreError> {
    let inventory = retention::inventory(&binding.repository.git_directory)
        .map_err(|error| invalid(error.to_string()))?;
    require(
        inventory.commits.contains(&operation)
            && !inventory.candidates.contains(&operation)
            && !inventory.delta_candidates.contains_key(&operation)
            && !inventory.evidence_imports.contains_key(&operation)
            && !inventory.planning_successors.contains_key(&operation)
            && !inventory.imports.contains(&operation)
            && !inventory.pushes.contains(&operation)
            && retention::commit_selection(&binding.repository.git_directory, operation)
                .map_err(|error| invalid(error.to_string()))?
                .as_ref()
                == Some(commit),
        "data bootstrap requires its exact commit-only retained operation",
    )?;
    task_sync::data_worktree::DataWorktree::from_binding(binding)
        .and_then(|data| data.check_commit(&commit.oid))
        .map_err(|error| invalid(error.to_string()))?;
    let source = task_sync::git::read_task_snapshot(
        &binding.repository.main_checkout,
        &binding.project,
        &commit.oid,
    )
    .map_err(|error| invalid(error.to_string()))?;
    require(
        source.commit.oid == commit.oid
            && source.commit.tree == commit.tree
            && source.commit.parents == commit.parents
            && source.commit.raw_bytes_fingerprint == commit.raw_bytes_fingerprint
            && source.envelope.profile == binding.profile,
        "data bootstrap snapshot or disclosure profile differs",
    )?;
    let reference = task_sync::git::query(
        &binding.repository.main_checkout,
        &["show-ref", "--hash", "--verify", &commit.retention_ref],
        false,
        None,
        crate::operation_time::MINIMUM,
    )
    .map_err(|error| {
        invalid(format!(
            "data bootstrap retention reference unavailable: {error}"
        ))
    })?;
    require(
        reference == format!("{}\n", commit.oid).as_bytes(),
        "data bootstrap retention reference moved",
    )?;
    Ok(CheckpointSeed::Incoming {
        snapshot: Box::new(MembershipSnapshot {
            commit: commit.clone(),
            membership: membership(&source.tree, source.envelope.profile)?,
        }),
        members: embedded(source.tree),
    })
}

pub(super) fn artifact(
    operation: uuid::Uuid,
    commit: &RetainedCommit,
    total: &mut usize,
) -> Result<CustodyArtifactRef, StoreError> {
    let comparison = CanonicalObject::freeze(commit)?;
    *total = total
        .checked_add(comparison.bytes().len())
        .ok_or_else(|| invalid("cold custody artifact byte bound exceeded"))?;
    require(
        *total <= super::super::MAX_BYTES,
        "cold custody artifact byte bound exceeded",
    )?;
    Ok(CustodyArtifactRef {
        operation,
        kind: CustodyArtifactKind::IncomingDataCommit,
        content_fingerprint: comparison.key().clone(),
        record: None,
    })
}

pub(in crate::storage::work::task_sync::ledger) fn known(
    ledger: &Ledger,
    root: &Path,
) -> Result<BTreeMap<uuid::Uuid, task_sync::ledger::RetainedCommit>, StoreError> {
    let mut result = BTreeMap::new();
    for event in ledger.events.values() {
        if let Transition::CustodyCheckpoint { checkpoint } = &event.transition
            && let Some(commit) = checkpoint.data_commit()
            && ledger.bindings.get(&event.binding).is_some_and(|binding| {
                crate::storage::store_path_text(&binding.repository.git_directory)
                    == crate::storage::store_path_text(root)
            })
        {
            require(
                result
                    .insert(checkpoint.operation, commit.clone())
                    .is_none(),
                "data bootstrap pin has competing checkpoint owners",
            )?;
        }
    }
    Ok(result)
}

pub(in crate::storage::work::task_sync::ledger) fn validate_pin(
    root: &Path,
    inventory: &retention::RetainedInventory,
    operation: uuid::Uuid,
    commit: &RetainedCommit,
) -> Result<(), StoreError> {
    require(
        inventory.commits.contains(&operation)
            && !inventory.candidates.contains(&operation)
            && !inventory.delta_candidates.contains_key(&operation)
            && !inventory.evidence_imports.contains_key(&operation)
            && !inventory.planning_successors.contains_key(&operation)
            && !inventory.imports.contains(&operation)
            && !inventory.pushes.contains(&operation)
            && retention::commit_selection(root, operation)
                .map_err(|error| invalid(error.to_string()))?
                .as_ref()
                == Some(commit),
        "checkpoint-owned data commit custody differs",
    )
}
