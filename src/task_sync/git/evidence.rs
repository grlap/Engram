//! Read only explicitly selected immutable evidence blobs from the data carrier.

use super::{CommitObject, QueryFailure, delta::Reader};
use crate::{
    ObjectId,
    task_sync::{
        data_worktree::{Budget, DataWorktree},
        evidence::{EvidenceError, EvidencePack},
        ledger::{Binding, EvidenceIntent},
    },
};
use anyhow::{Context, Result, ensure};
use sha2::{Digest, Sha256};

pub struct CommitEvidenceBatch {
    pub commit: CommitObject,
    pub packs: Vec<EvidencePack>,
}

/// The selected immutable carrier definitely disagrees with the frozen request:
/// its commit, disclosure profile, pack path, declared size, fingerprint, pack
/// structure or pack header. Reading the same immutable objects again can only
/// give the same answer, unlike an unavailable, interrupted or late Git read.
#[derive(Debug, thiserror::Error)]
#[error("{0}")]
pub struct SelectionMismatch(pub(crate) &'static str);

fn mismatch(holds: bool, reason: &'static str) -> Result<()> {
    if holds {
        Ok(())
    } else {
        Err(SelectionMismatch(reason).into())
    }
}

/// Read the exact selected pack paths, with bounded raw blobs and a total deadline.
/// This observes Git only: no retention, SQL, index, branch or network changes.
/// The caller checks completed-operation replay before invoking this reader.
///
/// # Errors
/// Refuses a non-data carrier, changed commit bytes/profile, missing or unsafe
/// selected paths, changed pack bytes or declared bounds, and bounded Git failures.
pub fn read_selected_evidence(
    binding_id: &ObjectId,
    binding: &Binding,
    intent: &EvidenceIntent,
    deadline: std::time::Duration,
) -> Result<CommitEvidenceBatch> {
    ensure!(
        deadline >= crate::operation_time::MINIMUM,
        "selected evidence request deadline must allow at least 60 seconds"
    );
    let warning = crate::operation_time::Warning::start("Git selected evidence read");
    read_selected_evidence_remaining(binding_id, binding, intent, deadline, &warning)
}

// Internal exchange phases already admitted their request's full allowance.
// Their positive remainder is not a fresh duration override.
pub(crate) fn read_selected_evidence_remaining(
    binding_id: &ObjectId,
    binding: &Binding,
    intent: &EvidenceIntent,
    deadline: std::time::Duration,
    warning: &crate::operation_time::Warning,
) -> Result<CommitEvidenceBatch> {
    let started = std::time::Instant::now();
    ensure!(!deadline.is_zero(), "evidence read deadline is exhausted");
    binding.validate()?;
    intent.validate(binding_id, binding)?;
    // The data-worktree observations at both ends spend the same total budget
    // as the bounded reader between them.
    let budget = Budget::observed_within(started, deadline, warning)?;
    let data = DataWorktree::from_binding_within(binding, budget)?;
    data.check_commit_within(&intent.source.oid, budget)?;
    let remaining = deadline
        .checked_sub(started.elapsed())
        .filter(|left| !left.is_zero())
        .context("evidence read deadline is exhausted")?;
    let reader = Reader::bounded(
        &binding.repository.main_checkout,
        &binding.project,
        remaining,
    )?;
    let commit = reader.commit_object(&intent.source.oid, "evidence carrier")?;
    mismatch(
        commit.tree == intent.source.tree
            && commit.parents == intent.source.parents
            && commit.raw_bytes_fingerprint == intent.source.raw_bytes_fingerprint,
        "selected evidence carrier differs from retained commit bytes",
    )?;
    let (metadata, _, _) = reader.metadata_with_size(&commit.tree, &binding.project)?;
    mismatch(
        metadata.profile == binding.profile,
        "selected evidence disclosure profile differs",
    )?;
    let mut packs = Vec::with_capacity(intent.packs.len());
    for (id, selection) in &intent.packs {
        let path = crate::task_sync::evidence::path::pack(&selection.publishing_origin, id);
        let entries = reader.entries(&commit.tree, &[&path])?;
        let entry = entries
            .first()
            .ok_or(SelectionMismatch("selected evidence blob is unavailable"))?;
        mismatch(
            entries.len() == 1
                && entry.name == path
                && entry.mode == "100644"
                && entry.kind == "blob",
            "selected evidence path is not one ordinary blob",
        )?;
        // A blob larger than its declared size exceeds the bounded read.
        let bytes = reader
            .evidence_blob(&entry.oid, selection.encoded_bytes)
            .map_err(|error| {
                if matches!(
                    error.downcast_ref::<QueryFailure>(),
                    Some(QueryFailure::OutputLimit)
                ) {
                    SelectionMismatch("selected evidence blob exceeds its declared size").into()
                } else {
                    error
                }
            })?;
        mismatch(
            bytes.len() == selection.encoded_bytes
                && format!("{:x}", Sha256::digest(&bytes))
                    == selection.content_fingerprint.as_str(),
            "selected evidence encoding differs from frozen request",
        )?;
        // Malformed immutable bytes are a definite answer too; other pack
        // errors keep their own evidence refusal meaning.
        let pack = EvidencePack::read(bytes.as_slice()).map_err(|error| match error {
            EvidenceError::InvalidPack(_) => {
                anyhow::Error::new(SelectionMismatch("selected evidence pack is malformed"))
            }
            other => anyhow::Error::new(other),
        })?;
        mismatch(
            pack.pack_id() == id
                && pack.project_id() == &binding.project
                && pack.publishing_origin() == &selection.publishing_origin
                && pack.records().len() == selection.records,
            "selected evidence pack differs from frozen request",
        )?;
        packs.push(pack);
    }
    data.revalidate_within(budget)?;
    ensure!(
        started.elapsed() <= deadline,
        "evidence read deadline is exhausted"
    );
    Ok(CommitEvidenceBatch { commit, packs })
}
