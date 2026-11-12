//! Inspect retained delta inputs before their owning storage transitions.

pub(crate) use attempt::publish_delta_outgoing_remaining;
pub(crate) use incoming::retain_incoming_delta_remaining;
pub(crate) use publication::record_delta_publication_remaining;
pub(crate) use resolution::{
    inspect_delta_resolution_remaining, record_delta_resolution_remaining,
    record_delta_retry_progress_remaining,
};
pub(crate) use retry::publish_delta_retry_remaining;

use super::{
    Cancellation, RunLimits,
    admission::{GitSelection, observe_git_selection},
    remaining,
};
use crate::{
    ObjectId,
    storage::SqliteStore,
    task_sync::{
        delta::{DeltaBasis, DeltaCandidatePayload, DeltaIntent},
        ledger::{Binding, CompletedExchange, Direction},
        retention,
    },
};
use anyhow::{Context, Result, ensure};
use chrono::{DateTime, Utc};
use std::{collections::BTreeMap, time::Instant};

mod incoming;
mod publication;
pub use incoming::retain_incoming_delta;
pub use publication::{DeltaPublicationRequest, record_delta_publication};
pub(crate) mod resolution;
pub use resolution::{
    DeltaResolutionRequest, InspectedDeltaResolution, inspect_delta_resolution,
    record_delta_resolution, record_delta_retry_progress,
};
pub(crate) mod attempt;
pub use attempt::{DeltaOutgoingSelection, publish_delta_outgoing};
pub(crate) mod retry;
pub use retry::publish_delta_retry;

#[derive(Clone, Debug)]
pub(crate) struct InspectedDelta {
    pub(crate) binding_id: ObjectId,
    pub(crate) binding: Binding,
    pub(crate) intent: DeltaIntent,
    pub(crate) payload: DeltaCandidatePayload,
}

impl InspectedDelta {
    pub(crate) fn recheck_candidate(&self) -> Result<()> {
        ensure!(
            retention::delta::read_candidate(&self.binding_id, &self.binding, &self.intent)?
                .as_ref()
                == Some(&self.payload),
            "retained delta input changed after inspection"
        );
        Ok(())
    }

    pub(crate) fn recheck_files(&self) -> Result<()> {
        self.recheck_candidate()?;
        let DeltaBasis::Incoming { source_after, .. } = &self.intent.basis else {
            anyhow::bail!("delta incoming basis is missing");
        };
        ensure!(
            retention::commit_selection(
                &self.binding.repository.git_directory,
                self.intent.operation
            )?
            .as_ref()
                == Some(&source_after.commit),
            "retained delta commit selection changed after inspection"
        );
        Ok(())
    }
}

/// Bind an already retained, data-only Git commit to its exact outgoing delta.
/// This explicit inspection reads the complete selected snapshot; it is not
/// the ordinary proportional publication path. It creates no commit, changes
/// no branch and contacts no remote. Repeating it preserves the original event.
///
/// # Errors
/// Refuses missing preparation, changed retained bytes or pins, a foreign data
/// worktree, a different snapshot membership, custody uncertainty and limits.
pub async fn record_delta_export_commit(
    store: &mut SqliteStore,
    binding_id: &ObjectId,
    binding: &Binding,
    intent: &DeltaIntent,
    selection: &crate::task_sync::delta::DeltaCommitSelection,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<ObjectId> {
    let started = std::time::Instant::now();
    limits.validate_request()?;
    let _warning =
        crate::operation_time::Warning::start("task exchange record delta export commit");
    let limits = crate::task_sync::exchange::remaining(limits, started)?;
    record_delta_export_commit_remaining(
        store, binding_id, binding, intent, selection, limits, cancel,
    )
    .await
}

// The public ingress admitted the full budget; internal calls spend its remainder.
pub(crate) async fn record_delta_export_commit_remaining(
    store: &mut SqliteStore,
    binding_id: &ObjectId,
    binding: &Binding,
    intent: &DeltaIntent,
    selection: &crate::task_sync::delta::DeltaCommitSelection,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<ObjectId> {
    selection.validate(binding_id, binding, intent, &selection.prepared)?;
    let started = Instant::now();
    let budget = crate::task_sync::data_worktree::Budget::within(started, limits.deadline)?;
    let data = crate::task_sync::data_worktree::DataWorktree::from_binding_within(binding, budget)?;
    data.check_commit_within(&selection.commit.oid, budget)?;
    let selected = GitSelection {
        expected_head: intent.expected_head.as_deref(),
        commit: &selection.commit,
        common_commit: intent.common_commit.as_ref(),
        published_head: true,
    };
    observe_git_selection(binding, selected, limits, started, cancel).await?;
    let payload = retention::delta::read_candidate(binding_id, binding, intent)?
        .context("retained outgoing delta candidate is missing")?;
    let snapshot = crate::task_sync::git::read_task_snapshot_bounded(
        &binding.repository.main_checkout,
        &binding.project,
        &selection.commit.oid,
        remaining(limits, started)?.deadline,
    )?;
    let root = snapshot.tree.tasks.iter().try_fold(
        crate::task_sync::inventory::InventoryRoot::ZERO,
        crate::task_sync::inventory::InventoryRoot::including,
    )?;
    let documents = snapshot
        .tree
        .tasks
        .iter()
        .map(|task| (task.id, task))
        .collect::<BTreeMap<_, _>>();
    ensure!(
        root == payload.output.inventory_root
            && snapshot.envelope.profile == payload.output.profile
            && payload
                .documents
                .iter()
                .all(|(id, expected)| { documents.get(id).copied() == expected.as_ref() }),
        "selected Git snapshot differs from the prepared delta membership"
    );
    data.revalidate_within(budget)?;
    observe_git_selection(
        binding,
        selected,
        remaining(limits, started)?,
        Instant::now(),
        cancel,
    )
    .await?;
    let input = InspectedDelta {
        binding_id: binding_id.clone(),
        binding: binding.clone(),
        intent: intent.clone(),
        payload,
    };
    Ok(store.record_task_sync_delta_export_commit(&input, selection, Utc::now())?)
}

/// Apply one incoming delta through exact historical replay or inspected Git
/// input and the atomic storage writer. No fetch, commit or publication runs.
///
/// # Errors
/// Refuses changed requests, unsafe Git/configuration or retained state, and
/// every fresh-state, closure or write refusal of the storage owner.
pub async fn apply_delta_exchange(
    store: &mut SqliteStore,
    binding_id: &ObjectId,
    binding: &Binding,
    intent: &DeltaIntent,
    now: DateTime<Utc>,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<CompletedExchange> {
    let started = std::time::Instant::now();
    limits.validate_request()?;
    let _warning = crate::operation_time::Warning::start("task exchange apply delta exchange");
    let limits = crate::task_sync::exchange::remaining(limits, started)?;
    apply_delta_exchange_remaining(store, binding_id, binding, intent, now, limits, cancel).await
}

// The public ingress admitted the full budget; internal calls spend its remainder.
pub(crate) async fn apply_delta_exchange_remaining(
    store: &mut SqliteStore,
    binding_id: &ObjectId,
    binding: &Binding,
    intent: &DeltaIntent,
    now: DateTime<Utc>,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<CompletedExchange> {
    if let Some(original) = store.completed_task_sync_delta_import(binding_id, intent)? {
        return Ok(original);
    }
    binding.validate()?;
    intent.validate(binding_id, binding)?;
    ensure!(
        intent.direction == Direction::Incoming,
        "delta application requires an incoming intent"
    );
    let DeltaBasis::Incoming {
        source_before,
        source_after,
        source_changed,
        ..
    } = &intent.basis
    else {
        anyhow::bail!("delta incoming basis is missing");
    };
    let started = Instant::now();
    let selected = GitSelection {
        expected_head: intent.expected_head.as_deref(),
        commit: &source_after.commit,
        common_commit: intent.common_commit.as_ref(),
        published_head: false,
    };
    observe_git_selection(binding, selected, limits, started, cancel).await?;
    let payload = retention::delta::read_candidate(binding_id, binding, intent)?
        .context("retained delta candidate is missing; preserve exact intent")?;
    let delta = crate::task_sync::git::read_task_delta(
        &binding.repository.main_checkout,
        &binding.project,
        &source_before.snapshot.commit.oid,
        &source_after.commit.oid,
    )?;
    let changes = delta
        .upserts
        .into_iter()
        .map(|(id, task)| (id, Some(task)))
        .chain(delta.removed.into_iter().map(|id| (id, None)))
        .collect::<BTreeMap<_, _>>();
    ensure!(
        changes
            .keys()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            == *source_changed
            && changes
                .iter()
                .all(|(id, task)| payload.documents.get(id) == Some(task)),
        "retained delta source changes differ from the exact Git commits"
    );
    ensure!(
        delta.incoming.profile.include_notes == binding.profile.include_notes,
        "delta Git note profile differs from binding"
    );
    // Share the legacy inspection owner's exact admin, branch, target and ref
    // checks again after selected blob inspection. Files are checked in SQL too.
    observe_git_selection(
        binding,
        selected,
        remaining(limits, started)?,
        Instant::now(),
        cancel,
    )
    .await?;
    let input = InspectedDelta {
        binding_id: binding_id.clone(),
        binding: binding.clone(),
        intent: intent.clone(),
        payload,
    };
    input.recheck_files()?;
    Ok(store.apply_task_sync_delta_exchange(&input, now)?)
}
