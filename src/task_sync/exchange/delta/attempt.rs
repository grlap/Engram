//! First delta dispatch through the existing confined Git transport. A known
//! refusal returns the agent step; replay and uncertainty observe only.

use super::super::{
    ExchangeTarget, PublicationAttempt, RunError, TargetSelection,
    admission::{GitSelection, observe_git_selection_data},
    attempt::{AttemptIo, GitIo},
    remaining,
};
use super::{Cancellation, InspectedDelta, RunLimits, retention};
use crate::{
    ObjectId,
    storage::SqliteStore,
    task_sync::{
        data_worktree::DataWorktree,
        delta::{DeltaCommitSelection, DeltaIntent},
        ledger::{Binding, OutgoingFailure, OutgoingProgress, PublicationObservation, Transition},
    },
};
use anyhow::{Context, Result, ensure};
use std::time::Instant;

/// The original preparation and its already admitted retained data commit.
pub struct DeltaOutgoingSelection<'a> {
    pub binding_id: &'a ObjectId,
    pub binding: &'a Binding,
    pub intent: &'a DeltaIntent,
    pub commit: &'a DeltaCommitSelection,
}

/// Publish the original selected delta commit once. A confirmed canonical
/// outcome returns without reading Git or files. An existing start or unknown
/// outcome only observes the remote; it never dispatches the push again.
/// Known ancestry or non-fast-forward failure requests agent reconciliation.
/// This entry does not admit a resolved delta retry or grant Git authority.
///
/// # Errors
/// Refuses changed scope or retained selection, a missing canonical commit,
/// unsupported transport, custody uncertainty and bounded execution failures.
pub async fn publish_delta_outgoing(
    store: &mut SqliteStore,
    selected: &DeltaOutgoingSelection<'_>,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<PublicationAttempt> {
    let started = std::time::Instant::now();
    limits.validate_request()?;
    let _warning = crate::operation_time::Warning::start("task exchange publish delta outgoing");
    let limits = crate::task_sync::exchange::remaining(limits, started)?;
    publish_delta_outgoing_remaining(store, selected, limits, cancel).await
}

// The public ingress admitted the full budget; internal calls spend its remainder.
pub(crate) async fn publish_delta_outgoing_remaining(
    store: &mut SqliteStore,
    selected: &DeltaOutgoingSelection<'_>,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<PublicationAttempt> {
    limits.validate()?;
    let started = Instant::now();
    let mode = mode(store, selected)?;
    if let Mode::Confirmed(event) = mode {
        return Ok(PublicationAttempt::Confirmed { event });
    }
    let payload =
        retention::delta::read_candidate(selected.binding_id, selected.binding, selected.intent)?
            .context("retained outgoing delta candidate is missing")?;
    let input = InspectedDelta {
        binding_id: selected.binding_id.clone(),
        binding: selected.binding.clone(),
        intent: selected.intent.clone(),
        payload,
    };
    let target = ExchangeTarget::capture_remaining(
        &TargetSelection {
            repository: input.binding.repository.main_checkout.clone(),
            remote: input.binding.target.remote.clone(),
            reference: input.binding.target.reference.clone(),
        },
        remaining(limits, started)?,
        cancel,
    )
    .await?;
    ensure!(
        target.fingerprint()? == input.binding.target.configuration_fingerprint,
        "selected delta publication target changed"
    );
    let mut io = DeltaGitIo(GitIo {
        target,
        data: DataWorktree::from_binding_within(
            &input.binding,
            crate::task_sync::data_worktree::Budget::within(started, limits.deadline)?,
        )?,
        limits,
        started,
        cancel,
        operation: input.intent.operation,
    });
    publish_with(store, &input, selected.commit, &mut io).await
}

enum Mode {
    Fresh,
    Observe,
    /// A recorded exact refusal of this start; resume its fetch only.
    Refused(ObjectId),
    Confirmed(ObjectId),
}

fn mode(store: &SqliteStore, selected: &DeltaOutgoingSelection<'_>) -> Result<Mode> {
    selected.commit.validate(
        selected.binding_id,
        selected.binding,
        selected.intent,
        &selected.commit.prepared,
    )?;
    ensure!(
        store.task_sync_binding(selected.binding_id)?.as_ref() == Some(selected.binding),
        "delta publication binding differs"
    );
    let events = store.task_sync_delta_outgoing_events(selected.binding_id, selected.intent)?;
    ensure!(
        events.iter().any(|(_, event)| matches!(&event.transition,
        Transition::DeltaLocalCommitRecorded { prepared, commit }
        if prepared == &selected.commit.prepared && commit == &selected.commit.commit)),
        "record the exact delta commit before publication"
    );
    let (latest, event) = events
        .last()
        .context("delta publication progress is missing")?;
    match &event.transition {
        Transition::PushObserved {
            observation: PublicationObservation::Confirmed { .. },
            ..
        } => Ok(Mode::Confirmed(latest.clone())),
        Transition::PushRejected { push_started, .. } => Ok(Mode::Refused(push_started.clone())),
        Transition::PushStarted { .. } | Transition::PushObserved { .. } => Ok(Mode::Observe),
        Transition::DeltaLocalCommitRecorded { .. } => Ok(Mode::Fresh),
        _ => anyhow::bail!("delta publication requires its original committed selection"),
    }
}

pub(crate) trait DeltaAttemptIo {
    async fn revalidate(
        &mut self,
        input: &InspectedDelta,
        commit: &DeltaCommitSelection,
    ) -> Result<()>;
    async fn tip(&mut self) -> Result<Option<String>>;
    async fn fetch(&mut self, tip: &str) -> Result<String>;
    async fn ancestor(&mut self, tip: &str, commit: &str) -> Result<bool>;
    async fn push(&mut self, commit: &str) -> std::result::Result<(), RunError>;
}

async fn record(
    store: &mut SqliteStore,
    input: &InspectedDelta,
    commit: &DeltaCommitSelection,
    io: &mut impl DeltaAttemptIo,
    progress: OutgoingProgress,
) -> Result<(ObjectId, bool)> {
    io.revalidate(input, commit).await?;
    Ok(store.record_task_sync_delta_progress_once(input, commit, progress, chrono::Utc::now())?)
}

fn current_mode(
    store: &SqliteStore,
    input: &InspectedDelta,
    commit: &DeltaCommitSelection,
) -> Result<Mode> {
    mode(
        store,
        &DeltaOutgoingSelection {
            binding_id: &input.binding_id,
            binding: &input.binding,
            intent: &input.intent,
            commit,
        },
    )
}

pub(crate) async fn publish_with(
    store: &mut SqliteStore,
    input: &InspectedDelta,
    commit: &DeltaCommitSelection,
    io: &mut impl DeltaAttemptIo,
) -> Result<PublicationAttempt> {
    match current_mode(store, input, commit)? {
        Mode::Confirmed(event) => return Ok(PublicationAttempt::Confirmed { event }),
        Mode::Refused(start) => return refused(io, start).await,
        Mode::Observe => return observe(store, input, commit, io).await,
        Mode::Fresh => (),
    }
    io.revalidate(input, commit).await?;
    if let Some(tip) = io.tip().await? {
        let fetched_ref = io.fetch(&tip).await?;
        if !io.ancestor(&tip, &commit.commit.oid).await? {
            return Ok(PublicationAttempt::ReconciliationRequired {
                failure: OutgoingFailure::AncestryChanged { fetched_tip: tip },
                fetched_ref,
            });
        }
    }
    let (start, fresh) = record(store, input, commit, io, OutgoingProgress::PushStarted).await?;
    if !fresh {
        if let Mode::Refused(rejected) = current_mode(store, input, commit)? {
            return refused(io, rejected).await;
        }
        return observe(store, input, commit, io).await;
    }
    // Record the exact refusal before any follow-up call; a lost fetch or
    // response then stays a known refusal, never an uncertain publication.
    if let Err(RunError::NonFastForward) = io.push(&commit.commit.oid).await {
        record(
            store,
            input,
            commit,
            io,
            OutgoingProgress::PushRejected {
                push_started: start.clone(),
            },
        )
        .await?;
        return refused(io, start).await;
    }
    observe(store, input, commit, io).await
}

const REFUSED: &str =
    "the delta push was refused as non-fast-forward; publish again to fetch the replacement tip";

/// Resume a recorded refusal by reading and fetching the replacement tip only.
/// The push is never repeated; a failure keeps the refusal for the next call.
async fn refused(
    io: &mut impl DeltaAttemptIo,
    push_started: ObjectId,
) -> Result<PublicationAttempt> {
    let tip = io.tip().await.context(REFUSED)?.context(REFUSED)?;
    let fetched_ref = io.fetch(&tip).await.context(REFUSED)?;
    Ok(PublicationAttempt::ReconciliationRequired {
        failure: OutgoingFailure::NonFastForward {
            push_started,
            fetched_tip: tip,
        },
        fetched_ref,
    })
}

async fn observe(
    store: &mut SqliteStore,
    input: &InspectedDelta,
    commit: &DeltaCommitSelection,
    io: &mut impl DeltaAttemptIo,
) -> Result<PublicationAttempt> {
    let observed_tip = io.tip().await.ok().flatten();
    let confirmed = observed_tip.as_deref() == Some(commit.commit.oid.as_str());
    let observation = if confirmed {
        PublicationObservation::Confirmed {
            observed_tip: commit.commit.oid.clone(),
        }
    } else {
        PublicationObservation::Unknown { observed_tip }
    };
    let (event, _) = record(
        store,
        input,
        commit,
        io,
        OutgoingProgress::Observed(observation),
    )
    .await?;
    Ok(if confirmed {
        PublicationAttempt::Confirmed { event }
    } else {
        PublicationAttempt::Unknown { event }
    })
}

pub(super) struct DeltaGitIo<'a>(pub(super) GitIo<'a>);

#[cfg(all(test, windows))]
pub(in crate::task_sync::exchange) fn test_git_io(io: GitIo<'_>) -> impl DeltaAttemptIo + '_ {
    DeltaGitIo(io)
}

impl DeltaAttemptIo for DeltaGitIo<'_> {
    async fn revalidate(
        &mut self,
        input: &InspectedDelta,
        commit: &DeltaCommitSelection,
    ) -> Result<()> {
        #[cfg(test)]
        let _phase = super::super::phase_trace::phase("revalidate-delta");
        input.recheck_candidate()?;
        ensure!(
            retention::commit_selection(
                &input.binding.repository.git_directory,
                input.intent.operation
            )?
            .as_ref()
                == Some(&commit.commit),
            "delta publication retained commit selection changed"
        );
        let observed = observe_git_selection_data(
            &input.binding,
            GitSelection {
                expected_head: input.intent.expected_head.as_deref(),
                commit: &commit.commit,
                common_commit: input.intent.common_commit.as_ref(),
                published_head: true,
            },
            remaining(self.0.limits, self.0.started)?,
            Instant::now(),
            self.0.cancel,
        )
        .await?;
        ensure!(
            observed == self.0.data,
            "data worktree changed since observation"
        );
        Ok(())
    }

    async fn tip(&mut self) -> Result<Option<String>> {
        #[cfg(test)]
        let _phase = super::super::phase_trace::phase("remote-tip");
        self.0.tip().await
    }
    async fn fetch(&mut self, tip: &str) -> Result<String> {
        #[cfg(test)]
        let _phase = super::super::phase_trace::phase("fetch-tip");
        self.0.fetch(tip).await
    }
    async fn ancestor(&mut self, tip: &str, commit: &str) -> Result<bool> {
        #[cfg(test)]
        let _phase = super::super::phase_trace::phase("check-ancestor");
        self.0.ancestor(tip, commit).await
    }
    async fn push(&mut self, commit: &str) -> std::result::Result<(), RunError> {
        #[cfg(test)]
        let _phase = super::super::phase_trace::phase("push-delta");
        self.0.push(commit).await
    }
}
