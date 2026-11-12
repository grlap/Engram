//! Explicit incoming selection over the existing Git and private custody owners.

use super::{
    BTreeMap, Binding, Cancellation, DateTime, DeltaBasis, DeltaCandidatePayload, DeltaIntent,
    Direction, GitSelection, InspectedDelta, Instant, ObjectId, Result, RunLimits, SqliteStore,
    Utc, ensure, observe_git_selection, remaining, retention,
};
use crate::task_sync::{delta::RetainedDeltaCandidate, inventory::InventoryRoot};

/// Retain the exact incoming payload without importing planning or contacting a
/// remote. Existing bytes are replayed, never replaced or reminted.
///
/// # Errors
/// Refuses mismatched scope, selected commits, profiles, membership, payload,
/// custody, origin continuity, local basis, limits and cancellation.
pub async fn retain_incoming_delta(
    store: &mut SqliteStore,
    binding_id: &ObjectId,
    binding: &Binding,
    intent: &DeltaIntent,
    now: DateTime<Utc>,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<RetainedDeltaCandidate> {
    let started = std::time::Instant::now();
    limits.validate_request()?;
    let _warning = crate::operation_time::Warning::start("task exchange retain incoming delta");
    let limits = crate::task_sync::exchange::remaining(limits, started)?;
    retain_incoming_delta_remaining(store, binding_id, binding, intent, now, limits, cancel).await
}

// The public ingress admitted the full budget; internal calls spend its remainder.
pub(crate) async fn retain_incoming_delta_remaining(
    store: &mut SqliteStore,
    binding_id: &ObjectId,
    binding: &Binding,
    intent: &DeltaIntent,
    now: DateTime<Utc>,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<RetainedDeltaCandidate> {
    limits.validate()?;
    binding.validate()?;
    intent.validate(binding_id, binding)?;
    ensure!(
        intent.direction == Direction::Incoming,
        "delta retention requires an incoming intent"
    );
    if let Some(payload) = retention::delta::read_candidate(binding_id, binding, intent)? {
        return Ok(store.retain_task_sync_incoming_delta(
            &InspectedDelta {
                binding_id: binding_id.clone(),
                binding: binding.clone(),
                intent: intent.clone(),
                payload,
            },
            now,
        )?);
    }
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
    let data = super::super::admission::observe_git_selection_data(
        binding, selected, limits, started, cancel,
    )
    .await?;
    data.check_commit_within(
        &source_before.snapshot.commit.oid,
        crate::task_sync::data_worktree::Budget::within(started, limits.deadline)?,
    )?;
    crate::task_sync::exchange::verify_retained_commit_remaining(
        &binding.repository.main_checkout,
        &source_before.snapshot.commit,
        remaining(limits, started)?,
        cancel,
    )
    .await?;
    let delta = crate::task_sync::git::read_task_delta(
        &binding.repository.main_checkout,
        &binding.project,
        &source_before.snapshot.commit.oid,
        &source_after.commit.oid,
    )?;
    let mut documents = delta
        .upserts
        .into_iter()
        .map(|(id, task)| (id, Some(task)))
        .chain(delta.removed.into_iter().map(|id| (id, None)))
        .collect::<BTreeMap<_, _>>();
    ensure!(
        documents
            .keys()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            == *source_changed,
        "incoming delta changed keys differ from selected Git commits"
    );
    // Retention is an explicit cold inspection. Read the selected snapshot to
    // prove its declared membership and include destination-only changed keys.
    let snapshot = crate::task_sync::git::read_task_snapshot_bounded(
        &binding.repository.main_checkout,
        &binding.project,
        &source_after.commit.oid,
        remaining(limits, started)?.deadline,
    )?;
    let root = snapshot
        .tree
        .tasks
        .iter()
        .try_fold(InventoryRoot::ZERO, InventoryRoot::including)?;
    ensure!(
        root == source_after.membership.inventory_root
            && snapshot.envelope.profile == source_after.membership.profile,
        "incoming delta membership or profile differs from selected Git commit"
    );
    let tasks = snapshot
        .tree
        .tasks
        .into_iter()
        .map(|task| (task.id, task))
        .collect::<BTreeMap<_, _>>();
    for id in &intent.candidate.changed {
        documents
            .entry(*id)
            .or_insert_with(|| tasks.get(id).cloned());
    }
    let payload = DeltaCandidatePayload {
        format: crate::task_sync::delta::DELTA_PAYLOAD_FORMAT.into(),
        project_id: binding.project.clone(),
        operation: intent.operation,
        output: intent.candidate.output.clone(),
        documents,
    };
    payload.validate(binding_id, binding, intent)?;
    observe_git_selection(
        binding,
        selected,
        remaining(limits, started)?,
        Instant::now(),
        cancel,
    )
    .await?;
    Ok(store.retain_task_sync_incoming_delta(
        &InspectedDelta {
            binding_id: binding_id.clone(),
            binding: binding.clone(),
            intent: intent.clone(),
            payload,
        },
        now,
    )?)
}
