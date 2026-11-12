//! Selected delta bytes under the existing repository custody reservation.
//! These files establish no canonical admission or managed-writer activation.

use super::{
    audit::{self, AuditReservation},
    container, missing, write_new,
};
use crate::{
    ObjectId,
    task_sync::{
        delta::{DeltaCandidatePayload, DeltaIntent, RetainedDeltaCandidate},
        ledger::Binding,
        setup::PrivateDirectory,
    },
};
use anyhow::{Context, Result, ensure};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

const ROOT: &str = "engram-task-sync-delta-candidates";
const LIMIT: usize = 16 * 1024 * 1024;

/// A complete retained request observed by the cold custody audit. Its presence
/// grants no admission, completion or managed-writer authority.
#[derive(Clone, Debug, Deserialize, Serialize, Eq, PartialEq)]
#[serde(deny_unknown_fields)]
pub struct RetainedRequest {
    pub binding: ObjectId,
    pub intent: DeltaIntent,
}

pub(crate) fn request_fingerprint(binding: &ObjectId, intent: &DeltaIntent) -> Result<ObjectId> {
    Ok(
        crate::CanonicalObject::freeze(&("strict_delta", binding, intent))?
            .key()
            .clone(),
    )
}

pub(crate) fn commit_request_fingerprint(
    binding: &ObjectId,
    intent: &DeltaIntent,
    selection: &crate::task_sync::delta::DeltaCommitSelection,
) -> Result<ObjectId> {
    Ok(
        crate::CanonicalObject::freeze(&("strict_delta_commit", binding, intent, selection))?
            .key()
            .clone(),
    )
}

pub(crate) fn progress_request_fingerprint(
    binding: &ObjectId,
    intent: &DeltaIntent,
    selection: &crate::task_sync::delta::DeltaCommitSelection,
    progress: &crate::task_sync::ledger::OutgoingProgress,
) -> Result<ObjectId> {
    Ok(crate::CanonicalObject::freeze(&(
        "strict_delta_progress",
        binding,
        intent,
        selection,
        progress,
    ))?
    .key()
    .clone())
}

pub(crate) fn resolution_request_fingerprint(
    binding: &ObjectId,
    original: &DeltaIntent,
    selection: &crate::task_sync::delta::DeltaCommitSelection,
    recipe: &crate::task_sync::ledger::OutgoingReconciliation,
) -> Result<ObjectId> {
    Ok(crate::CanonicalObject::freeze(&(
        "strict_delta_resolution",
        binding,
        original,
        selection,
        recipe,
    ))?
    .key()
    .clone())
}

pub(crate) fn retry_request_fingerprint(
    binding: &ObjectId,
    original: &DeltaIntent,
    selection: &crate::task_sync::delta::DeltaCommitSelection,
    recipe: &crate::task_sync::ledger::OutgoingReconciliation,
    progress: &crate::task_sync::ledger::OutgoingProgress,
) -> Result<ObjectId> {
    Ok(crate::CanonicalObject::freeze(&(
        "strict_delta_retry",
        binding,
        original,
        selection,
        recipe,
        progress,
    ))?
    .key()
    .clone())
}

pub(super) fn inventory(
    git: &PrivateDirectory,
    git_directory: &Path,
) -> Result<std::collections::BTreeMap<uuid::Uuid, RetainedRequest>> {
    let root = match git.child(Path::new(ROOT)) {
        Ok(root) => root,
        Err(error) if missing(&error) => return Ok(std::collections::BTreeMap::default()),
        Err(error) => return Err(error),
    };
    let mut result = std::collections::BTreeMap::new();
    let mut total = 0_usize;
    for name in root.names(super::MAX_TASKS)? {
        let text = name.to_str().context("non-UTF8 retained delta operation")?;
        let operation = uuid::Uuid::parse_str(text)?;
        ensure!(
            !operation.is_nil() && text == operation.to_string(),
            "noncanonical retained delta operation"
        );
        let (request, payload, bytes) = observe(&root.child(Path::new(&name))?)?;
        total = total
            .checked_add(bytes)
            .context("retained delta byte bound exceeded")?;
        ensure!(
            total <= super::MAX_TREE,
            "retained delta byte bound exceeded"
        );
        ensure!(
            request.intent.operation == operation
                && request.intent.candidate.operation == operation
                && request.intent.candidate.directory
                    == candidate_directory(git_directory, operation),
            "retained delta names another operation or directory"
        );
        payload.validate_declared(&request.intent)?;
        result.insert(operation, request);
    }
    Ok(result)
}

/// The selected operation's fixed location in the existing retention owner.
/// It is a local file location, never an event or publication identity.
#[must_use]
pub fn candidate_directory(git_directory: &Path, operation: uuid::Uuid) -> PathBuf {
    git_directory.join(ROOT).join(operation.to_string())
}

fn selection(binding_id: &ObjectId, binding: &Binding, intent: &DeltaIntent) -> Result<()> {
    intent.validate(binding_id, binding)?;
    ensure!(
        intent.candidate.directory
            == candidate_directory(&binding.repository.git_directory, intent.operation),
        "delta candidate must use its exact operation retention directory"
    );
    Ok(())
}

/// Retain the exact validated request before its selected document bytes.
/// Reuses the audit reservation; no SQL, Git ref or ordinary sync is changed.
///
/// # Errors
/// Refuses bounds before filesystem effects, competing custody, another exact
/// intent, and any partial prior attempt. Existing bytes are never replaced.
pub fn retain_candidate(
    binding_id: &ObjectId,
    binding: &Binding,
    intent: &DeltaIntent,
    payload: &DeltaCandidatePayload,
) -> Result<RetainedDeltaCandidate> {
    selection(binding_id, binding, intent)?;
    payload.validate(binding_id, binding, intent)?;
    let reservation = RetainedRequest {
        binding: binding_id.clone(),
        intent: intent.clone(),
    };
    let request_bytes = crate::canonical::canonical_bytes(&reservation)?;
    let payload_bytes = crate::canonical::canonical_bytes(payload)?;
    ensure!(
        request_bytes.len() <= LIMIT && payload_bytes.len() <= LIMIT,
        "retained delta exceeds byte bound"
    );
    let _guard = AuditReservation::acquire(&binding.repository.git_directory)?;
    let git = PrivateDirectory::open(&binding.repository.git_directory)?;
    let root = container(&git, Path::new(ROOT))?;
    let name = intent.operation.to_string();
    let operation = match root.create_child(Path::new(&name)) {
        Ok(operation) => operation,
        Err(error) => {
            let operation = root
                .child(Path::new(&name))
                .with_context(|| format!("delta reservation failed: {error}"))?;
            ensure!(
                read_at(&operation, &reservation, binding)? == *payload,
                "retained delta differs from exact retry"
            );
            return Ok(intent.candidate.clone());
        }
    };
    // The reserved directory and complete intent stay if publication stops.
    // A partial request or payload never permits recapture or reminting.
    write_new(&operation, Path::new("intent.pending"), &request_bytes)?;
    #[cfg(unix)]
    audit::sync(&operation)?;
    #[cfg(unix)]
    audit::sync(&root)?;
    write_new(&operation, Path::new("payload.json"), &payload_bytes)?;
    #[cfg(unix)]
    audit::sync(&operation)?;
    ensure!(
        read_at(&operation, &reservation, binding)? == *payload,
        "delta candidate changed during retention"
    );
    Ok(intent.candidate.clone())
}

/// Read only the named operation; disconnected history is not enumerated.
/// Missing means no selected artifact, never an empty source membership.
///
/// # Errors
/// Refuses unsafe or partial files, changed request, scope or selected payload.
pub fn read_candidate(
    binding_id: &ObjectId,
    binding: &Binding,
    intent: &DeltaIntent,
) -> Result<Option<DeltaCandidatePayload>> {
    selection(binding_id, binding, intent)?;
    let git = PrivateDirectory::open(&binding.repository.git_directory)?;
    let operation = match git
        .child(Path::new(ROOT))
        .and_then(|root| root.child(Path::new(&intent.operation.to_string())))
    {
        Ok(operation) => operation,
        Err(error) if missing(&error) => return Ok(None),
        Err(error) => return Err(error),
    };
    read_at(
        &operation,
        &RetainedRequest {
            binding: binding_id.clone(),
            intent: intent.clone(),
        },
        binding,
    )
    .map(Some)
}

fn read_at(
    operation: &PrivateDirectory,
    expected: &RetainedRequest,
    binding: &Binding,
) -> Result<DeltaCandidatePayload> {
    let (actual, payload, _) = observe(operation)?;
    ensure!(
        &actual == expected,
        "retained delta intent differs from exact retry"
    );
    payload.validate(&expected.binding, binding, &expected.intent)?;
    Ok(payload)
}

/// Observe the complete retained bytes for explicit custody accounting. This
/// proves neither canonical admission nor ownership of the selected binding.
pub(crate) fn observe_construction(
    git_directory: &Path,
    operation: uuid::Uuid,
) -> Result<(RetainedRequest, DeltaCandidatePayload)> {
    let git = PrivateDirectory::open(git_directory)?;
    let directory = git
        .child(Path::new(ROOT))?
        .child(Path::new(&operation.to_string()))?;
    let (request, payload, _) = observe(&directory)?;
    ensure!(
        request.intent.operation == operation
            && request.intent.candidate.directory == candidate_directory(git_directory, operation),
        "retained construction operation or directory differs"
    );
    payload.validate_declared(&request.intent)?;
    Ok((request, payload))
}

fn observe(
    operation: &PrivateDirectory,
) -> Result<(RetainedRequest, DeltaCandidatePayload, usize)> {
    ensure!(
        operation.names(2)? == ["intent.pending", "payload.json"].map(std::ffi::OsString::from),
        "partial or unknown retained delta; preserve exact request"
    );
    let request = audit::read_bytes(operation, Path::new("intent.pending"), LIMIT)?
        .context("retained delta intent missing")?;
    let actual: RetainedRequest = serde_json::from_slice(&request)?;
    ensure!(
        crate::canonical::canonical_bytes(&actual)? == request,
        "retained delta intent is noncanonical"
    );
    let bytes = audit::read_bytes(operation, Path::new("payload.json"), LIMIT)?
        .context("retained delta payload missing")?;
    let payload: DeltaCandidatePayload = serde_json::from_slice(&bytes)?;
    ensure!(
        crate::canonical::canonical_bytes(&payload)? == bytes,
        "retained delta payload is noncanonical"
    );
    Ok((actual, payload, request.len() + bytes.len()))
}
