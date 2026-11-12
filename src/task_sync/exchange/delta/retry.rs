//! Dispatch the single admitted resolved retry; replay only observes.

use super::super::{
    ExchangeTarget, PublicationAttempt, RunError, TargetSelection, attempt::GitIo, remaining,
};
use super::{
    Cancellation, InspectedDelta, InspectedDeltaResolution, RunLimits,
    attempt::{DeltaAttemptIo, DeltaGitIo},
    resolution::{DeltaResolutionRequest, inspect_delta_resolution_remaining},
};
use crate::{
    ObjectId,
    storage::SqliteStore,
    task_sync::{
        data_worktree::DataWorktree,
        delta::DeltaCommitSelection,
        ledger::{
            OutgoingFailure, OutgoingProgress, OutgoingReconciliation, PublicationObservation,
            Transition,
        },
    },
};
use anyhow::{Context, Result, ensure};
use std::time::Instant;

/// Publish the admitted merged commit once. A canonical confirmed or stopped
/// outcome returns offline. An existing retry start or Unknown only observes;
/// it never dispatches again. A second known refusal records an explicit stop.
/// Invocation grants no Git authority or live authority from another host.
///
/// # Errors
/// Refuses changed recipe, Git/files/custody, invalid stage or bounded transport.
/// An unrecordable result retains the original start and its uncertainty.
pub async fn publish_delta_retry(
    store: &mut SqliteStore,
    input: &InspectedDeltaResolution,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<PublicationAttempt> {
    let started = std::time::Instant::now();
    limits.validate_request()?;
    let _warning = crate::operation_time::Warning::start("task exchange publish delta retry");
    let limits = crate::task_sync::exchange::remaining(limits, started)?;
    Box::pin(publish_delta_retry_remaining(store, input, limits, cancel)).await
}

// The public ingress admitted the full budget; internal calls spend its remainder.
pub(crate) async fn publish_delta_retry_remaining(
    store: &mut SqliteStore,
    input: &InspectedDeltaResolution,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<PublicationAttempt> {
    limits.validate()?;
    let started = Instant::now();
    if let Mode::Terminal(outcome) = mode(store, input)? {
        return Ok(outcome);
    }
    #[cfg(test)]
    let phase = super::super::phase_trace::phase("inspect-resolution");
    let checked = inspect_delta_resolution_remaining(
        store,
        &DeltaResolutionRequest {
            binding_id: &input.original.binding_id,
            binding: &input.original.binding,
            original: &input.original.intent,
            selection: &input.selection,
            recipe: &input.recipe,
        },
        remaining(limits, started)?,
        cancel,
    )
    .await?;
    #[cfg(test)]
    drop(phase);
    #[cfg(test)]
    let phase = super::super::phase_trace::phase("capture-target");
    let target = ExchangeTarget::capture_remaining(
        &TargetSelection {
            repository: input.original.binding.repository.main_checkout.clone(),
            remote: input.original.binding.target.remote.clone(),
            reference: input.original.binding.target.reference.clone(),
        },
        remaining(limits, started)?,
        cancel,
    )
    .await?;
    #[cfg(test)]
    drop(phase);
    ensure!(
        target.fingerprint()? == input.original.binding.target.configuration_fingerprint,
        "delta retry target changed"
    );
    let mut io = DeltaGitIo(GitIo {
        target,
        data: DataWorktree::from_binding_within(
            &input.original.binding,
            crate::task_sync::data_worktree::Budget::within(started, limits.deadline)?,
        )?,
        limits,
        started,
        cancel,
        operation: input.original.intent.operation,
    });
    publish_with(store, &checked, &mut io).await
}

enum Mode {
    Fresh,
    Observe,
    /// A recorded exact refusal of the retry start; resume to its stop.
    Refused(ObjectId),
    Terminal(PublicationAttempt),
}

fn mode(store: &SqliteStore, input: &InspectedDeltaResolution) -> Result<Mode> {
    input.recipe.validate_delta_resolution(
        &input.original.binding_id,
        &input.original.binding,
        &input.selection.prepared,
        &input.original.intent,
        &input.selection.commit,
    )?;
    ensure!(
        store
            .task_sync_binding(&input.original.binding_id)?
            .as_ref()
            == Some(&input.original.binding),
        "delta retry binding differs"
    );
    let events = store
        .task_sync_delta_outgoing_events(&input.original.binding_id, &input.original.intent)?;
    let (recipe,recipe_event)=events.iter().find(|(_,event)| matches!(&event.transition,
        Transition::OutgoingReconciled {prepared,reconciliation} if prepared==&input.selection.prepared && reconciliation.as_ref()==&input.recipe))
        .context("record the exact delta resolution before retry dispatch")?;
    let (latest, event) = events.last().context("delta retry progress missing")?;
    match &event.transition {
        Transition::PushObserved {
            observation: PublicationObservation::Confirmed { observed_tip },
            ..
        } => {
            let (_, selected) = resolved(input)?;
            ensure!(
                event.sequence > recipe_event.sequence && observed_tip == &selected.commit.oid,
                "delta retry confirmed commit differs"
            );
            Ok(Mode::Terminal(PublicationAttempt::Confirmed {
                event: latest.clone(),
            }))
        }
        Transition::OutgoingReconciled { reconciliation, .. }
            if matches!(
                reconciliation.as_ref(),
                OutgoingReconciliation::Stopped { .. }
            ) =>
        {
            Ok(Mode::Terminal(PublicationAttempt::Stopped {
                event: latest.clone(),
            }))
        }
        Transition::OutgoingReconciled { reconciliation, .. }
            if latest == recipe && reconciliation.as_ref() == &input.recipe =>
        {
            Ok(Mode::Fresh)
        }
        Transition::OutgoingReconciled { reconciliation, .. } if matches!(reconciliation.as_ref(),OutgoingReconciliation::PushStarted {reconciliation} if reconciliation==recipe) => {
            Ok(Mode::Observe)
        }
        Transition::PushObserved { .. } if event.sequence > recipe_event.sequence => {
            Ok(Mode::Observe)
        }
        Transition::PushRejected { push_started, .. } if event.sequence > recipe_event.sequence => {
            Ok(Mode::Refused(push_started.clone()))
        }
        _ => anyhow::bail!("delta retry requires its admitted recipe or exact progress"),
    }
}

fn resolved(input: &InspectedDeltaResolution) -> Result<(InspectedDelta, DeltaCommitSelection)> {
    let OutgoingReconciliation::DeltaResolved {
        resolved,
        payload,
        commit,
        ..
    } = &input.recipe
    else {
        anyhow::bail!("delta retry recipe missing");
    };
    Ok((
        InspectedDelta {
            binding_id: input.original.binding_id.clone(),
            binding: input.original.binding.clone(),
            intent: resolved.as_ref().clone(),
            payload: payload.as_ref().clone(),
        },
        DeltaCommitSelection {
            prepared: input.selection.prepared.clone(),
            commit: commit.as_ref().clone(),
        },
    ))
}

async fn record(
    store: &mut SqliteStore,
    input: &InspectedDeltaResolution,
    io: &mut impl DeltaAttemptIo,
    progress: OutgoingProgress,
) -> Result<(ObjectId, bool)> {
    let (selected, commit) = resolved(input)?;
    io.revalidate(&selected, &commit).await?;
    input.recheck_files()?;
    Ok(store.record_task_sync_delta_retry_once(input, &progress, chrono::Utc::now())?)
}

async fn stop(
    store: &mut SqliteStore,
    input: &InspectedDeltaResolution,
    io: &mut impl DeltaAttemptIo,
    failure: OutgoingFailure,
) -> Result<PublicationAttempt> {
    let (event, _) = record(
        store,
        input,
        io,
        OutgoingProgress::Reconciled {
            prepared: input.selection.prepared.clone(),
            reconciliation: Box::new(OutgoingReconciliation::Stopped { failure }),
        },
    )
    .await?;
    Ok(PublicationAttempt::Stopped { event })
}

pub(crate) async fn publish_with(
    store: &mut SqliteStore,
    input: &InspectedDeltaResolution,
    io: &mut impl DeltaAttemptIo,
) -> Result<PublicationAttempt> {
    match mode(store, input)? {
        Mode::Terminal(outcome) => return Ok(outcome),
        Mode::Refused(start) => return refused(store, input, io, start).await,
        Mode::Observe => return observe(store, input, io).await,
        Mode::Fresh => (),
    }
    let (selected, commit) = resolved(input)?;
    io.revalidate(&selected, &commit).await?;
    if let Some(tip) = io.tip().await? {
        io.fetch(&tip).await?;
        if !io.ancestor(&tip, &commit.commit.oid).await? {
            return stop(
                store,
                input,
                io,
                OutgoingFailure::AncestryChanged { fetched_tip: tip },
            )
            .await;
        }
    }
    let (start, fresh) = record(store, input, io, OutgoingProgress::PushStarted).await?;
    if !fresh {
        if let Mode::Refused(rejected) = mode(store, input)? {
            return refused(store, input, io, rejected).await;
        }
        return observe(store, input, io).await;
    }
    // Record the exact refusal before any follow-up call, so a lost fetch or
    // response still ends in the explicit stop rather than uncertainty.
    if let Err(RunError::NonFastForward) = io.push(&commit.commit.oid).await {
        record(
            store,
            input,
            io,
            OutgoingProgress::PushRejected {
                push_started: start.clone(),
            },
        )
        .await?;
        return refused(store, input, io, start).await;
    }
    observe(store, input, io).await
}

const REFUSED: &str =
    "the delta retry push was refused as non-fast-forward; publish again to record its stop";

/// Resume a recorded refusal of the single retry: fetch the replacement tip,
/// never push again, and settle with the explicit stop. No second recipe.
async fn refused(
    store: &mut SqliteStore,
    input: &InspectedDeltaResolution,
    io: &mut impl DeltaAttemptIo,
    push_started: ObjectId,
) -> Result<PublicationAttempt> {
    let tip = io.tip().await.context(REFUSED)?.context(REFUSED)?;
    io.fetch(&tip).await.context(REFUSED)?;
    stop(
        store,
        input,
        io,
        OutgoingFailure::NonFastForward {
            push_started,
            fetched_tip: tip,
        },
    )
    .await
}

async fn observe(
    store: &mut SqliteStore,
    input: &InspectedDeltaResolution,
    io: &mut impl DeltaAttemptIo,
) -> Result<PublicationAttempt> {
    #[cfg(test)]
    let _phase = super::super::phase_trace::phase("observe-retry");
    let (_, commit) = resolved(input)?;
    let observed_tip = io.tip().await.ok().flatten();
    let confirmed = observed_tip.as_deref() == Some(commit.commit.oid.as_str());
    let observation = if confirmed {
        PublicationObservation::Confirmed {
            observed_tip: commit.commit.oid,
        }
    } else {
        PublicationObservation::Unknown { observed_tip }
    };
    let (event, _) = record(store, input, io, OutgoingProgress::Observed(observation)).await?;
    Ok(if confirmed {
        PublicationAttempt::Confirmed { event }
    } else {
        PublicationAttempt::Unknown { event }
    })
}
