//! Inspect a caller-owned delta publication step; never dispatch transport.

use super::super::admission::{GitSelection, observe_git_selection};
use super::super::attempt::{AttemptIo, GitIo};
use super::super::{ExchangeTarget, TargetSelection};
use super::{Cancellation, InspectedDelta, RunLimits, retention};
use crate::{
    ObjectId,
    storage::SqliteStore,
    task_sync::{
        data_worktree::{Budget, DataWorktree},
        delta::{DeltaCommitSelection, DeltaIntent},
        ledger::{Binding, OutgoingProgress, PublicationObservation, Transition},
    },
};
use anyhow::{Context, Result, ensure};
use std::time::Instant;

/// One transient selection over the original canonical request and commit.
/// This is caller attribution, not a transport receipt or Git authority.
pub struct DeltaPublicationRequest<'a> {
    pub binding_id: &'a ObjectId,
    pub binding: &'a Binding,
    pub intent: &'a DeltaIntent,
    pub selection: &'a DeltaCommitSelection,
    pub progress: OutgoingProgress,
}

/// Record a start or observation of the original selected outgoing delta.
/// Returns the original event and false on replay. Only a newly committed
/// start can authorize dispatch; true for a new observation grants none.
/// The caller must never dispatch from an old start.
/// This function creates no commit, changes no branch and never pushes. An
/// observation is the owner's own read-only read of the selected remote tip;
/// the caller's stated observation can never confirm on its own.
/// It checks selected Git metadata and retained bytes without rescanning the
/// complete task snapshot already checked when binding the commit. Existing
/// exhaustive ledger custody remains an explicit storage cost.
///
/// # Errors
/// Refuses a missing canonical commit, changed request, files or Git selection,
/// invalid transition order, custody uncertainty, supersession and limits.
pub async fn record_delta_publication(
    store: &mut SqliteStore,
    request: &DeltaPublicationRequest<'_>,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<(ObjectId, bool)> {
    let started = std::time::Instant::now();
    limits.validate_request()?;
    let _warning = crate::operation_time::Warning::start("task exchange record delta publication");
    let limits = crate::task_sync::exchange::remaining(limits, started)?;
    record_delta_publication_remaining(store, request, limits, cancel).await
}

// The public ingress admitted the full budget; internal calls spend its remainder.
pub(crate) async fn record_delta_publication_remaining(
    store: &mut SqliteStore,
    request: &DeltaPublicationRequest<'_>,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<(ObjectId, bool)> {
    limits.validate()?;
    let started = Instant::now();
    request.selection.validate(
        request.binding_id,
        request.binding,
        request.intent,
        &request.selection.prepared,
    )?;
    ensure!(
        matches!(
            &request.progress,
            OutgoingProgress::PushStarted
                | OutgoingProgress::PushRejected { .. }
                | OutgoingProgress::Observed(_)
        ),
        "delta publication requires a start, a refusal or an observation"
    );
    ensure!(
        store.task_sync_binding(request.binding_id)?.as_ref() == Some(request.binding),
        "delta publication binding differs"
    );
    let events = store.task_sync_delta_outgoing_events(request.binding_id, request.intent)?;
    ensure!(
        events.iter().any(|(_, event)| matches!(&event.transition,
            Transition::DeltaLocalCommitRecorded { prepared, commit }
            if prepared == &request.selection.prepared && commit == &request.selection.commit)),
        "record the exact delta commit before publication"
    );
    let selected = GitSelection {
        expected_head: request.intent.expected_head.as_deref(),
        commit: &request.selection.commit,
        common_commit: request.intent.common_commit.as_ref(),
        published_head: true,
    };
    let budget = Budget::within(started, limits.deadline)?;
    let data = DataWorktree::from_binding_within(request.binding, budget)?;
    data.check_commit_within(&request.selection.commit.oid, budget)?;
    observe_git_selection(request.binding, selected, limits, started, cancel).await?;
    ensure!(
        retention::commit_selection(
            &request.binding.repository.git_directory,
            request.intent.operation,
        )?
        .as_ref()
            == Some(&request.selection.commit),
        "delta publication retained commit selection changed"
    );
    let payload =
        retention::delta::read_candidate(request.binding_id, request.binding, request.intent)?
            .context("retained outgoing delta candidate is missing")?;
    let input = InspectedDelta {
        binding_id: request.binding_id.clone(),
        binding: request.binding.clone(),
        intent: request.intent.clone(),
        payload,
    };
    data.revalidate_within(budget)?;
    observe_git_selection(
        request.binding,
        selected,
        super::remaining(limits, started)?,
        Instant::now(),
        cancel,
    )
    .await?;
    let progress = match &request.progress {
        OutgoingProgress::Observed(claimed) => OutgoingProgress::Observed(
            observe_selected_tip(
                request.binding,
                request.intent.operation,
                &request.selection.commit.oid,
                claimed,
                limits,
                started,
                cancel,
            )
            .await?,
        ),
        other => other.clone(),
    };
    Ok(store.record_task_sync_delta_progress_once(
        &input,
        request.selection,
        progress,
        chrono::Utc::now(),
    )?)
}

/// The observation this owner itself makes of the binding's selected remote
/// reference, through the same confined read-only transport the publication
/// driver observes with. A caller states only that it observed; it can never
/// confirm a publication the remote does not hold. Confirmed means the
/// selected tip is exactly the retained commit; anything else stays Unknown.
/// A caller's confirmation that this observation contradicts refuses, so
/// nothing is recorded. No push is made and no Git authority is granted.
///
/// # Errors
/// Refuses a changed target configuration, an unavailable observation and a
/// caller confirmation of another or an absent remote tip.
pub(super) async fn observe_selected_tip(
    binding: &Binding,
    operation: uuid::Uuid,
    commit: &str,
    claimed: &PublicationObservation,
    limits: RunLimits,
    started: Instant,
    cancel: &Cancellation,
) -> Result<PublicationObservation> {
    let target = ExchangeTarget::capture_remaining(
        &TargetSelection {
            repository: binding.repository.main_checkout.clone(),
            remote: binding.target.remote.clone(),
            reference: binding.target.reference.clone(),
        },
        super::remaining(limits, started)?,
        cancel,
    )
    .await?;
    ensure!(
        target.fingerprint()? == binding.target.configuration_fingerprint,
        "selected delta publication target changed"
    );
    let mut io = GitIo {
        target,
        data: DataWorktree::from_binding_within(
            binding,
            Budget::within(started, limits.deadline)?,
        )?,
        limits,
        started,
        cancel,
        operation,
    };
    let observed_tip = io
        .tip()
        .await
        .context("the selected remote tip could not be observed; nothing was recorded")?;
    let confirmed = observed_tip.as_deref() == Some(commit);
    if let PublicationObservation::Confirmed { observed_tip: tip } = claimed {
        ensure!(
            confirmed && tip == commit,
            "the selected remote tip is not the exact retained commit; publication is not confirmed"
        );
    }
    Ok(if confirmed {
        PublicationObservation::Confirmed {
            observed_tip: commit.to_owned(),
        }
    } else {
        PublicationObservation::Unknown { observed_tip }
    })
}
