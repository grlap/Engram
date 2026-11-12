//! Explicit abandonment of an unrequested fetched pin. No task or evidence
//! membership is inferred from a carrier that could not enter the ledger.

use super::{
    BTreeMap, Binding, CanonicalObject, CustodyArtifactKind, CustodyArtifactRef,
    CustodyAuditRequest, CustodySeedSelection, Ledger, ObjectId, StoreError, Transition, invalid,
    require, retention, task_sync,
};
use crate::task_sync::ledger::RetainedCommit;
use std::path::Path;

pub(super) fn selection(request: &CustodyAuditRequest) -> Option<(&ObjectId, &RetainedCommit)> {
    match &request.seed {
        CustodySeedSelection::AbandonFetchedCommit { binding, commit } => Some((binding, commit)),
        _ => None,
    }
}

pub(super) fn for_audit(
    ledger: &Ledger,
    root: &Path,
    request: Option<(&Binding, &CustodyAuditRequest)>,
) -> Result<BTreeMap<uuid::Uuid, CustodyArtifactRef>, StoreError> {
    let mut result = known(ledger, root)?;
    if let Some((binding, request)) = request
        && let Some((original, commit)) = selection(request)
        && crate::storage::store_path_text(&binding.repository.git_directory)
            == crate::storage::store_path_text(root)
    {
        admit(ledger, request)?;
        result.insert(
            request.operation,
            artifact(request.operation, original, commit, &mut 0)?,
        );
    }
    Ok(result)
}

pub(super) fn retained_artifact(
    root: &Path,
    retained: &CustodyArtifactRef,
    kind: CustodyArtifactKind,
    aliases: bool,
    total: &mut usize,
) -> Result<CustodyArtifactRef, StoreError> {
    require(
        kind == CustodyArtifactKind::Commit && !aliases,
        "abandoned carrier aliases canonical progress",
    )?;
    let selected = retention::commit_selection(root, retained.operation)
        .map_err(|error| invalid(error.to_string()))?
        .ok_or_else(|| invalid("abandoned carrier selection missing during audit"))?;
    artifact(
        retained.operation,
        retained
            .record
            .as_ref()
            .ok_or_else(|| invalid("abandoned carrier binding missing"))?,
        &selected,
        total,
    )
}

pub(super) fn lineage(
    bindings: &BTreeMap<ObjectId, Binding>,
    current: &ObjectId,
    original: &ObjectId,
) -> Result<(), StoreError> {
    let active = bindings
        .get(current)
        .ok_or_else(|| invalid("recovery binding missing"))?;
    let old = bindings
        .get(original)
        .ok_or_else(|| invalid("original fetched binding missing"))?;
    require(
        old.project == active.project
            && old.repository == active.repository
            && old.store.origin == active.store.origin
            && old.store.restore_occurrence == active.store.restore_occurrence
            && crate::storage::store_path_text(&old.store.database)
                == crate::storage::store_path_text(&active.store.database),
        "abandoned carrier repository, store or origin binding differs",
    )?;
    let mut cursor = current;
    for _ in 0..=bindings.len() {
        if cursor == original {
            return Ok(());
        }
        cursor = bindings
            .get(cursor)
            .and_then(|binding| binding.supersedes.as_ref())
            .ok_or_else(|| {
                invalid("recovery binding does not succeed the original fetched binding")
            })?;
    }
    Err(invalid("recovery binding lineage is cyclic"))
}

pub(super) fn admit(ledger: &Ledger, request: &CustodyAuditRequest) -> Result<(), StoreError> {
    admit_retained(ledger, request)?;
    if let Some((_, commit)) = selection(request) {
        let binding = &ledger.bindings[&request.binding];
        task_sync::data_worktree::DataWorktree::from_binding(binding)
            .map_err(|error| invalid(error.to_string()))?;
        git_pin(&binding.repository.git_directory, commit)?;
    }
    Ok(())
}

/// Recheck durable and retained facts under the writer, without launching Git.
pub(super) fn admit_retained(
    ledger: &Ledger,
    request: &CustodyAuditRequest,
) -> Result<(), StoreError> {
    let Some((original, commit)) = selection(request) else {
        return Ok(());
    };
    lineage(&ledger.bindings, &request.binding, original)?;
    let fingerprint = request.fingerprint()?;
    require(
        ledger.events.values().all(|event| {
            event.transition.retention_operation() != Some(request.operation)
                && !matches!(&event.transition, Transition::CustodyCheckpoint { checkpoint }
                    if checkpoint.operation == request.operation
                        && (event.binding != request.binding || checkpoint.request_fingerprint != fingerprint))
        }),
        "abandoned fetched operation already has canonical progress",
    )?;
    let binding = &ledger.bindings[&request.binding];
    let inventory = retention::inventory(&binding.repository.git_directory)
        .map_err(|error| invalid(error.to_string()))?;
    validate_pin(
        &binding.repository.git_directory,
        &inventory,
        request.operation,
        commit,
    )
}

pub(in crate::storage::work::task_sync::ledger) fn git_pin(
    repository: &Path,
    commit: &RetainedCommit,
) -> Result<(), StoreError> {
    let query = |args: &[&str]| {
        task_sync::git::query(
            repository,
            args,
            false,
            None,
            crate::operation_time::MINIMUM,
        )
        .map_err(|error| invalid(error.to_string()))
    };
    require(
        query(&[
            "for-each-ref",
            "--format=%(objectname) %(symref)",
            &commit.retention_ref,
        ])? == format!("{} \n", commit.oid).as_bytes(),
        "abandoned carrier Git pin differs; preserve uncertainty",
    )?;
    let bytes = query(&["cat-file", "commit", &commit.oid])?;
    let observed = task_sync::git::observe_commit(&commit.oid, &bytes)
        .map_err(|error| invalid(error.to_string()))?;
    require(
        observed.oid == commit.oid
            && observed.tree == commit.tree
            && observed.parents == commit.parents
            && observed.raw_bytes_fingerprint == commit.raw_bytes_fingerprint,
        "abandoned carrier Git bytes differ",
    )
}

pub(in crate::storage::work::task_sync::ledger) fn validate_pin(
    root: &Path,
    inventory: &retention::RetainedInventory,
    operation: uuid::Uuid,
    commit: &RetainedCommit,
) -> Result<(), StoreError> {
    super::data::validate_pin(root, inventory, operation, commit)
}

pub(super) fn artifact(
    operation: uuid::Uuid,
    original: &ObjectId,
    commit: &RetainedCommit,
    total: &mut usize,
) -> Result<CustodyArtifactRef, StoreError> {
    let mut artifact = super::data::artifact(operation, commit, total)?;
    artifact.kind = CustodyArtifactKind::AbandonedFetchedCommit;
    artifact.record = Some(original.clone());
    Ok(artifact)
}

pub(in crate::storage::work::task_sync::ledger) fn validate_accounted(
    bindings: &BTreeMap<ObjectId, Binding>,
    current: &ObjectId,
    root: &Path,
    inventory: &retention::RetainedInventory,
    artifact: &CustodyArtifactRef,
) -> Result<RetainedCommit, StoreError> {
    let original = artifact
        .record
        .as_ref()
        .ok_or_else(|| invalid("abandoned carrier binding missing"))?;
    lineage(bindings, current, original)?;
    let commit = retention::commit_selection(root, artifact.operation)
        .map_err(|error| invalid(error.to_string()))?
        .ok_or_else(|| invalid("abandoned carrier selection missing"))?;
    require(
        artifact.content_fingerprint == *CanonicalObject::freeze(&commit)?.key(),
        "abandoned carrier selection changed",
    )?;
    validate_pin(root, inventory, artifact.operation, &commit)?;
    Ok(commit)
}

pub(in crate::storage::work::task_sync::ledger) fn known(
    ledger: &Ledger,
    root: &Path,
) -> Result<BTreeMap<uuid::Uuid, CustodyArtifactRef>, StoreError> {
    known_with(ledger, root, |_, commit| git_pin(root, commit))
}

pub(in crate::storage::work::task_sync::ledger) fn known_captured(
    ledger: &Ledger,
    root: &Path,
    observed: &super::super::custody::Observed,
) -> Result<BTreeMap<uuid::Uuid, CustodyArtifactRef>, StoreError> {
    known_with(ledger, root, |operation, commit| {
        observed.abandoned_pin(operation, commit)
    })
}

fn known_with(
    ledger: &Ledger,
    root: &Path,
    check: impl Fn(uuid::Uuid, &RetainedCommit) -> Result<(), StoreError>,
) -> Result<BTreeMap<uuid::Uuid, CustodyArtifactRef>, StoreError> {
    let mut result = BTreeMap::new();
    let inventory = retention::inventory(root).map_err(|error| invalid(error.to_string()))?;
    for event in ledger.events.values() {
        if let Transition::CustodyCheckpoint { checkpoint } = &event.transition
            && let Some(artifact) = checkpoint.abandoned_commit()
            && ledger.bindings.get(&event.binding).is_some_and(|binding| {
                crate::storage::store_path_text(&binding.repository.git_directory)
                    == crate::storage::store_path_text(root)
            })
        {
            let commit =
                validate_accounted(&ledger.bindings, &event.binding, root, &inventory, artifact)?;
            check(artifact.operation, &commit)?;
            require(
                result
                    .insert(artifact.operation, artifact.clone())
                    .is_none(),
                "abandoned carrier has competing checkpoint owners",
            )?;
        }
    }
    Ok(result)
}
