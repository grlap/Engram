//! Inspect the actual merged graph and Git bytes without recording a recipe.

use super::super::{
    admission::{GitSelection, observe_git_selection_data},
    remaining, retained,
};
use super::{Cancellation, InspectedDelta, RunLimits, retention};
use crate::{
    ObjectId,
    storage::SqliteStore,
    task_sync::{
        delta::{DeltaCandidatePayload, DeltaCommitSelection, DeltaIntent, OutputMembership},
        git::CommitTaskSnapshot,
        inventory::InventoryRoot,
        ledger::{Binding, OutgoingReconciliation, RetainedCommit, Transition},
    },
};
use anyhow::{Context, Result, ensure};
use std::{collections::BTreeMap, time::Instant};

/// Caller-selected recipe under the original pending exchange. This is an
/// inspection request, not evidence that transport failed or a grant to push.
pub struct DeltaResolutionRequest<'a> {
    pub binding_id: &'a ObjectId,
    pub binding: &'a Binding,
    pub original: &'a DeltaIntent,
    pub selection: &'a DeltaCommitSelection,
    pub recipe: &'a OutgoingReconciliation,
}

/// An observed exact recipe; private fields prevent caller substitution.
/// It is not durable admission. A writer must recheck custody, canonical state,
/// files and Git before appending under the original pending exchange.
#[derive(Clone, Debug)]
pub struct InspectedDeltaResolution {
    pub(crate) original: InspectedDelta,
    pub(crate) selection: DeltaCommitSelection,
    pub(crate) recipe: OutgoingReconciliation,
}

impl InspectedDeltaResolution {
    /// The original preparation remains the sole pending operation.
    #[must_use]
    pub fn prepared(&self) -> &ObjectId {
        &self.selection.prepared
    }

    pub(crate) fn recheck_files(&self) -> Result<()> {
        self.original.recheck_candidate()?;
        self.recipe.validate_delta_resolution(
            &self.original.binding_id,
            &self.original.binding,
            &self.selection.prepared,
            &self.original.intent,
            &self.selection.commit,
        )?;
        let OutgoingReconciliation::DeltaResolved {
            resolved,
            payload,
            commit,
            ..
        } = &self.recipe
        else {
            anyhow::bail!("resolved delta recipe missing");
        };
        ensure!(
            retention::delta::read_candidate(
                &self.original.binding_id,
                &self.original.binding,
                resolved
            )?
            .as_ref()
                == Some(payload.as_ref()),
            "resolved delta retained payload changed"
        );
        let root = &self.original.binding.repository.git_directory;
        ensure!(
            retention::commit_selection(root, self.original.intent.operation)?.as_ref()
                == Some(&self.selection.commit)
                && retention::commit_selection(root, resolved.operation)?.as_ref()
                    == Some(commit.as_ref()),
            "resolved delta retained commit selection changed"
        );
        Ok(())
    }

    pub(crate) fn operation(&self) -> Result<uuid::Uuid> {
        let OutgoingReconciliation::DeltaResolved { resolved, .. } = &self.recipe else {
            anyhow::bail!("resolved delta recipe missing");
        };
        Ok(resolved.operation)
    }

    pub(crate) fn request_fingerprint(&self) -> Result<ObjectId> {
        retention::delta::resolution_request_fingerprint(
            &self.original.binding_id,
            &self.original.intent,
            &self.selection,
            &self.recipe,
        )
    }
}

/// Reobserve the actual Git graphs and files before recording this exact recipe
/// under the original pending exchange. Recording grants no transport dispatch.
///
/// # Errors
/// Refuses changed Git, files, canonical stage or private custody. An interrupted
/// acknowledgement can recover only the entire original recipe, never new text.
pub async fn record_delta_resolution(
    store: &mut SqliteStore,
    input: &InspectedDeltaResolution,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<ObjectId> {
    let started = std::time::Instant::now();
    limits.validate_request()?;
    let _warning = crate::operation_time::Warning::start("task exchange record delta resolution");
    let limits = crate::task_sync::exchange::remaining(limits, started)?;
    record_delta_resolution_remaining(store, input, limits, cancel).await
}

// The public ingress admitted the full budget; internal calls spend its remainder.
pub(crate) async fn record_delta_resolution_remaining(
    store: &mut SqliteStore,
    input: &InspectedDeltaResolution,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<ObjectId> {
    let checked = inspect_delta_resolution_remaining(
        store,
        &DeltaResolutionRequest {
            binding_id: &input.original.binding_id,
            binding: &input.original.binding,
            original: &input.original.intent,
            selection: &input.selection,
            recipe: &input.recipe,
        },
        limits,
        cancel,
    )
    .await?;
    store
        .record_task_sync_delta_resolution(&checked, chrono::Utc::now())
        .map_err(Into::into)
}

/// Record progress of the one admitted resolved retry, after repeating actual
/// Git inspection. A fresh start alone can authorize transport; replay returns
/// false. This function never dispatches, creates a commit or advances a ref.
/// The explicit complete snapshot inspection remains a bounded cold-read cost.
///
/// # Errors
/// Refuses a missing recipe, changed files/Git/custody, uncertainty, a second
/// retry or mismatched stop. A retained ahead witness is never silently erased.
pub async fn record_delta_retry_progress(
    store: &mut SqliteStore,
    input: &InspectedDeltaResolution,
    progress: &crate::task_sync::ledger::OutgoingProgress,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<(ObjectId, bool)> {
    let started = std::time::Instant::now();
    limits.validate_request()?;
    let _warning =
        crate::operation_time::Warning::start("task exchange record delta retry progress");
    let limits = crate::task_sync::exchange::remaining(limits, started)?;
    record_delta_retry_progress_remaining(store, input, progress, limits, cancel).await
}

// The public ingress admitted the full budget; internal calls spend its remainder.
pub(crate) async fn record_delta_retry_progress_remaining(
    store: &mut SqliteStore,
    input: &InspectedDeltaResolution,
    progress: &crate::task_sync::ledger::OutgoingProgress,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<(ObjectId, bool)> {
    let started = Instant::now();
    let checked = inspect_delta_resolution_remaining(
        store,
        &DeltaResolutionRequest {
            binding_id: &input.original.binding_id,
            binding: &input.original.binding,
            original: &input.original.intent,
            selection: &input.selection,
            recipe: &input.recipe,
        },
        limits,
        cancel,
    )
    .await?;
    // A retry's observation is the owner's own read of the selected remote
    // tip against the resolved merge commit, never the caller's statement.
    let progress = match progress {
        crate::task_sync::ledger::OutgoingProgress::Observed(claimed) => {
            let OutgoingReconciliation::DeltaResolved { commit, .. } = &checked.recipe else {
                anyhow::bail!("resolved delta recipe missing");
            };
            crate::task_sync::ledger::OutgoingProgress::Observed(
                super::publication::observe_selected_tip(
                    &checked.original.binding,
                    checked.original.intent.operation,
                    &commit.oid,
                    claimed,
                    limits,
                    started,
                    cancel,
                )
                .await?,
            )
        }
        other => other.clone(),
    };
    store
        .record_task_sync_delta_retry_once(&checked, &progress, chrono::Utc::now())
        .map_err(Into::into)
}

/// Inspect a retained agent merge against the original canonical native delta,
/// its selected data commit, the fetched failure pin, and both actual Git task
/// snapshots. Complete snapshots admit the merged graph; exact task differences
/// bind every selected changed byte. Embedded inventory text is not trusted.
/// This explicit bounded cold read makes no SQL, file, ref or transport change.
/// It never captures the merged text as native planning.
///
/// # Errors
/// Refuses another original request/commit, changed retained bytes or Git state,
/// a missing fetched pin, mismatched membership/delta, invalid graphs and limits.
pub async fn inspect_delta_resolution(
    store: &SqliteStore,
    request: &DeltaResolutionRequest<'_>,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<InspectedDeltaResolution> {
    let started = std::time::Instant::now();
    limits.validate_request()?;
    let _warning = crate::operation_time::Warning::start("task exchange inspect delta resolution");
    let limits = crate::task_sync::exchange::remaining(limits, started)?;
    inspect_delta_resolution_remaining(store, request, limits, cancel).await
}

// The public ingress admitted the full budget; internal calls spend its remainder.
pub(crate) async fn inspect_delta_resolution_remaining(
    store: &SqliteStore,
    request: &DeltaResolutionRequest<'_>,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<InspectedDeltaResolution> {
    limits.validate()?;
    let started = Instant::now();
    request.selection.validate(
        request.binding_id,
        request.binding,
        request.original,
        &request.selection.prepared,
    )?;
    request.recipe.validate_delta_resolution(
        request.binding_id,
        request.binding,
        &request.selection.prepared,
        request.original,
        &request.selection.commit,
    )?;
    ensure!(
        store.task_sync_binding(request.binding_id)?.as_ref() == Some(request.binding),
        "delta resolution binding differs"
    );
    let events = store.task_sync_delta_outgoing_events(request.binding_id, request.original)?;
    let payload = events
        .iter()
        .find_map(|(id, event)| match &event.transition {
            Transition::DeltaPrepared { intent, payload }
                if id == &request.selection.prepared && intent.as_ref() == request.original =>
            {
                Some(payload.as_ref().clone())
            }
            _ => None,
        })
        .context("delta resolution original canonical preparation differs")?;
    ensure!(
        events.iter().any(|(_, event)| matches!(&event.transition,
        Transition::DeltaLocalCommitRecorded { prepared, commit }
        if prepared == &request.selection.prepared && commit == &request.selection.commit)),
        "delta resolution original canonical commit differs"
    );
    let input = InspectedDeltaResolution {
        original: InspectedDelta {
            binding_id: request.binding_id.clone(),
            binding: request.binding.clone(),
            intent: request.original.clone(),
            payload,
        },
        selection: request.selection.clone(),
        recipe: request.recipe.clone(),
    };
    input.recheck_files()?;
    let OutgoingReconciliation::DeltaResolved {
        failure,
        resolved,
        payload,
        commit,
    } = request.recipe
    else {
        anyhow::bail!("resolved delta recipe missing");
    };
    let selected = GitSelection {
        expected_head: Some(&request.selection.commit.oid),
        commit,
        common_commit: resolved.common_commit.as_ref(),
        published_head: true,
    };
    let data =
        observe_git_selection_data(request.binding, selected, limits, started, cancel).await?;
    let budget = crate::task_sync::data_worktree::Budget::within(started, limits.deadline)?;
    data.check_commit_within(&request.selection.commit.oid, budget)?;
    data.check_commit_within(failure.fetched_tip(), budget)?;
    super::super::verify_retained_commit_remaining(
        &request.binding.repository.main_checkout,
        &request.selection.commit,
        remaining(limits, started)?,
        cancel,
    )
    .await?;
    let reference = format!(
        "refs/engram-exchange/{}/publication/{}",
        request.original.operation,
        failure.fetched_tip()
    );
    let pin = retained::local(
        &request.binding.repository.main_checkout,
        &[
            "for-each-ref",
            "--format=%(objectname) %(symref)",
            &reference,
        ],
        remaining(limits, started)?,
        cancel,
    )
    .await?;
    ensure!(
        pin == format!("{} \n", failure.fetched_tip()).as_bytes(),
        "delta resolution fetched failure pin differs"
    );
    let original_snapshot = crate::task_sync::git::read_task_snapshot_bounded(
        &request.binding.repository.main_checkout,
        &request.binding.project,
        &request.selection.commit.oid,
        remaining(limits, started)?.deadline,
    )?;
    let merged_snapshot = crate::task_sync::git::read_task_snapshot_bounded(
        &request.binding.repository.main_checkout,
        &request.binding.project,
        &commit.oid,
        remaining(limits, started)?.deadline,
    )?;
    same_commit(&original_snapshot, &request.selection.commit)?;
    same_commit(&merged_snapshot, commit)?;
    ensure!(
        membership(&original_snapshot)? == request.original.candidate.output,
        "delta resolution original Git membership differs from native preparation"
    );
    validate_snapshots(&original_snapshot, &merged_snapshot, payload)?;
    input.recheck_files()?;
    let observed = observe_git_selection_data(
        request.binding,
        selected,
        remaining(limits, started)?,
        Instant::now(),
        cancel,
    )
    .await?;
    ensure!(observed == data, "data worktree changed since observation");
    Ok(input)
}

fn same_commit(snapshot: &CommitTaskSnapshot, expected: &RetainedCommit) -> Result<()> {
    let actual = &snapshot.commit;
    ensure!(
        actual.oid == expected.oid
            && actual.tree == expected.tree
            && actual.parents == expected.parents
            && actual.raw_bytes_fingerprint == expected.raw_bytes_fingerprint,
        "delta resolution Git commit bytes differ"
    );
    Ok(())
}

fn membership(snapshot: &CommitTaskSnapshot) -> Result<OutputMembership> {
    Ok(OutputMembership {
        inventory_root: snapshot
            .tree
            .tasks
            .iter()
            .try_fold(InventoryRoot::ZERO, InventoryRoot::including)?,
        profile: snapshot.envelope.profile.clone(),
    })
}

pub(crate) fn validate_snapshots(
    original: &CommitTaskSnapshot,
    merged: &CommitTaskSnapshot,
    payload: &DeltaCandidatePayload,
) -> Result<()> {
    ensure!(
        original.tree.project == merged.tree.project
            && payload.project_id == merged.tree.project.project_id
            && original.envelope.profile == merged.envelope.profile
            && payload.output == membership(merged)?,
        "resolved Git project, profile or actual membership differs"
    );
    let before = original
        .tree
        .tasks
        .iter()
        .map(|task| (task.id, task))
        .collect::<BTreeMap<_, _>>();
    let after = merged
        .tree
        .tasks
        .iter()
        .map(|task| (task.id, task))
        .collect::<BTreeMap<_, _>>();
    let changed = before
        .keys()
        .chain(after.keys())
        .copied()
        .collect::<std::collections::BTreeSet<_>>()
        .into_iter()
        .filter(|id| before.get(id) != after.get(id))
        .map(|id| (id, after.get(&id).map(|task| (*task).clone())))
        .collect::<BTreeMap<_, _>>();
    ensure!(
        changed == payload.documents,
        "resolved delta payload differs from actual Git task changes"
    );
    Ok(())
}
