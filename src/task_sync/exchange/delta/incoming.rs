//! Explicit incoming selection over the existing Git and private custody owners.

use super::{
    BTreeMap, Binding, Cancellation, DateTime, DeltaBasis, DeltaCandidatePayload, DeltaIntent,
    Direction, GitSelection, InspectedDelta, Instant, ObjectId, Result, RunLimits, SqliteStore,
    Utc, ensure, observe_git_selection, remaining, retention,
};
use crate::task_sync::delta::RetainedDeltaCandidate;

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
    // The source-before root was verified when it was recorded, so the
    // selected commit's membership follows from the changed paths alone.
    let delta = crate::task_sync::git::read_verified_task_delta(
        &binding.repository.main_checkout,
        &binding.project,
        &source_before.snapshot.commit.oid,
        source_before.snapshot.membership.inventory_root,
        &source_after.commit.oid,
        remaining(limits, started)?.deadline,
    )?;
    delta.require_root(source_after.membership.inventory_root)?;
    ensure!(
        delta.incoming.profile == source_after.membership.profile,
        "incoming delta profile differs from selected Git commit"
    );
    let mut documents = delta
        .changed
        .into_iter()
        .map(|(id, task)| (id, task.after))
        .collect::<BTreeMap<_, _>>();
    ensure!(
        documents
            .keys()
            .copied()
            .collect::<std::collections::BTreeSet<_>>()
            == *source_changed,
        "incoming delta changed keys differ from selected Git commits"
    );
    // A destination-only key is unchanged by the source, so its value is the
    // selected commit's document at that path, read alone.
    let destination_only = intent
        .candidate
        .changed
        .iter()
        .copied()
        .filter(|id| !documents.contains_key(id))
        .collect::<Vec<_>>();
    documents.extend(crate::task_sync::git::read_tasks_at(
        &binding.repository.main_checkout,
        &binding.project,
        &source_after.commit.oid,
        destination_only,
        remaining(limits, started)?.deadline,
    )?);
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
