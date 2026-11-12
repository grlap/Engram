//! One dispatch per canonical publication witness. An agent supplies the one
//! resolved recipe separately; this runner never invents a merge or a retry.

use super::{
    Cancellation, ExchangeOperation, ExchangeTarget, InspectedExchange, Instant, RunError,
    RunLimits, TargetSelection, remaining, retained, run_remaining,
};
use crate::{
    ObjectId,
    storage::SqliteStore,
    task_sync::{
        data_worktree::{Budget, DataWorktree},
        ledger::{
            CompletedExchangeRequest, OutgoingFailure, OutgoingProgress, OutgoingReconciliation,
            PublicationObservation, Transition, oid,
        },
    },
};
use anyhow::{Context, Result, ensure};

/// A known refusal can request one agent reconciliation. Missing or uncertain
/// transport facts never consume that route or imply that a push did not apply.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PublicationAttempt {
    Confirmed {
        event: ObjectId,
    },
    ReconciliationRequired {
        failure: OutgoingFailure,
        fetched_ref: String,
    },
    Stopped {
        event: ObjectId,
    },
    Unknown {
        event: ObjectId,
    },
}

/// Publish the currently retained original or resolved outgoing candidate once.
/// The request always names the original preparation, including on a retry.
/// A repeated or concurrent call never dispatches an already-recorded start.
/// Invocation grants no Git authority; the service admits opt-in and attribution.
///
/// # Errors
/// Refuses a changed selection, missing local commit, unsafe data history,
/// unsupported transport, failed custody, or an unrecordable observation. A
/// failure after start preserves its canonical witness and needs observation.
pub async fn publish_outgoing(
    store: &mut SqliteStore,
    input: &InspectedExchange,
    request: &CompletedExchangeRequest,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<PublicationAttempt> {
    let started = std::time::Instant::now();
    limits.validate_request()?;
    let _warning = crate::operation_time::Warning::start("task exchange publish outgoing");
    let limits = crate::task_sync::exchange::remaining(limits, started)?;
    publish_outgoing_remaining(store, input, request, limits, cancel).await
}

// The public ingress admitted the full budget; internal calls spend its remainder.
pub(crate) async fn publish_outgoing_remaining(
    store: &mut SqliteStore,
    input: &InspectedExchange,
    request: &CompletedExchangeRequest,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<PublicationAttempt> {
    limits.validate()?;
    let started = Instant::now();
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
        "selected publication target changed"
    );
    let data = DataWorktree::from_binding_within(
        &input.binding,
        Budget::within(started, limits.deadline)?,
    )?;
    let mut io = GitIo {
        target,
        data,
        limits,
        started,
        cancel,
        operation: input.intent.operation,
    };
    publish_with(store, input, request, &mut io).await
}

pub(super) trait AttemptIo {
    async fn revalidate(&mut self, input: &InspectedExchange) -> Result<()>;
    async fn tip(&mut self) -> Result<Option<String>>;
    async fn fetch(&mut self, tip: &str) -> Result<String>;
    async fn ancestor(&mut self, tip: &str, commit: &str) -> Result<bool>;
    async fn push(&mut self, commit: &str) -> std::result::Result<(), RunError>;
}

async fn record(
    store: &mut SqliteStore,
    input: &InspectedExchange,
    io: &mut impl AttemptIo,
    progress: OutgoingProgress,
) -> Result<(ObjectId, bool)> {
    io.revalidate(input).await?;
    Ok(store.record_task_sync_exchange_progress_once(input, progress, chrono::Utc::now())?)
}

async fn publish_with(
    store: &mut SqliteStore,
    input: &InspectedExchange,
    request: &CompletedExchangeRequest,
    io: &mut impl AttemptIo,
) -> Result<PublicationAttempt> {
    ensure!(
        request.binding == input.binding_id,
        "publication binding differs"
    );
    let events = store.task_sync_outgoing_events(request)?;
    let (prepared, _) = events.first().context("outgoing preparation is missing")?;
    let selected = events
        .iter()
        .rev()
        .find_map(|(_, event)| event.transition.retained_intent())
        .context("outgoing recipe is missing")?;
    ensure!(
        selected == &input.intent,
        "publication recipe differs from current operation"
    );
    let retry = events
        .iter()
        .find_map(|(id, event)| match &event.transition {
            Transition::OutgoingReconciled { reconciliation, .. }
                if matches!(
                    reconciliation.as_ref(),
                    OutgoingReconciliation::Resolved { .. }
                ) =>
            {
                Some(id)
            }
            _ => None,
        });
    let (latest, event) = events.last().context("outgoing progress is missing")?;
    match &event.transition {
        Transition::PushObserved {
            observation: PublicationObservation::Confirmed { .. },
            ..
        } => {
            return Ok(PublicationAttempt::Confirmed {
                event: latest.clone(),
            });
        }
        Transition::PushRejected { push_started, .. } => {
            return refused(
                store,
                input,
                io,
                prepared,
                retry.is_some(),
                push_started.clone(),
            )
            .await;
        }
        Transition::OutgoingReconciled { reconciliation, .. }
            if matches!(
                reconciliation.as_ref(),
                OutgoingReconciliation::Stopped { .. }
            ) =>
        {
            return Ok(PublicationAttempt::Stopped {
                event: latest.clone(),
            });
        }
        Transition::PushStarted { .. } | Transition::PushObserved { .. } => {
            return observe(store, input, io).await;
        }
        Transition::OutgoingReconciled { reconciliation, .. }
            if matches!(
                reconciliation.as_ref(),
                OutgoingReconciliation::PushStarted { .. }
            ) =>
        {
            return observe(store, input, io).await;
        }
        Transition::LocalCommitRecorded { .. } | Transition::OutgoingReconciled { .. } => {}
        _ => anyhow::bail!("record the local data commit before publication"),
    }
    io.revalidate(input).await?;
    if let Some(tip) = io.tip().await? {
        let fetched_ref = io.fetch(&tip).await?;
        if !io.ancestor(&tip, &input.intent.commit.oid).await? {
            return known_failure(
                store,
                input,
                io,
                prepared,
                retry.is_some(),
                OutgoingFailure::AncestryChanged { fetched_tip: tip },
                fetched_ref,
            )
            .await;
        }
    }
    let progress = retry.map_or(OutgoingProgress::PushStarted, |reconciliation| {
        OutgoingProgress::Reconciled {
            prepared: prepared.clone(),
            reconciliation: Box::new(OutgoingReconciliation::PushStarted {
                reconciliation: reconciliation.clone(),
            }),
        }
    });
    let (start, fresh) = record(store, input, io, progress).await?;
    if !fresh {
        // Another invocation won, or this is replay after a crash. The returned
        // old id is evidence of uncertainty, never permission to push again.
        if let Some(rejected) = recorded_refusal(store, request)? {
            return refused(store, input, io, prepared, retry.is_some(), rejected).await;
        }
        return observe(store, input, io).await;
    }
    // Only a completed exact single-ref rejection can request reconciliation.
    // It is recorded before any follow-up call, so a lost fetch or response
    // leaves a known refusal rather than an uncertain publication.
    if let Err(RunError::NonFastForward) = io.push(&input.intent.commit.oid).await {
        record(
            store,
            input,
            io,
            OutgoingProgress::PushRejected {
                push_started: start.clone(),
            },
        )
        .await?;
        return refused(store, input, io, prepared, retry.is_some(), start).await;
    }
    // Confirm every result by the exact remote tip. A timeout or broken pipe
    // preserves uncertainty; observing never repeats the push.
    observe(store, input, io).await
}

const REFUSED: &str =
    "the push was refused as non-fast-forward; publish again to fetch the replacement tip";

/// The start named by a refusal recorded as the latest outgoing step.
fn recorded_refusal(
    store: &SqliteStore,
    request: &CompletedExchangeRequest,
) -> Result<Option<ObjectId>> {
    Ok(store
        .task_sync_outgoing_events(request)?
        .last()
        .and_then(|(_, event)| match &event.transition {
            Transition::PushRejected { push_started, .. } => Some(push_started.clone()),
            _ => None,
        }))
}

/// Resume a recorded refusal. Only the replacement tip is read and fetched
/// again, never the push; a failure keeps the refusal for the next call.
async fn refused(
    store: &mut SqliteStore,
    input: &InspectedExchange,
    io: &mut impl AttemptIo,
    prepared: &ObjectId,
    retry: bool,
    push_started: ObjectId,
) -> Result<PublicationAttempt> {
    let tip = io.tip().await.context(REFUSED)?.context(REFUSED)?;
    let fetched_ref = io.fetch(&tip).await.context(REFUSED)?;
    known_failure(
        store,
        input,
        io,
        prepared,
        retry,
        OutgoingFailure::NonFastForward {
            push_started,
            fetched_tip: tip,
        },
        fetched_ref,
    )
    .await
}

async fn known_failure(
    store: &mut SqliteStore,
    input: &InspectedExchange,
    io: &mut impl AttemptIo,
    prepared: &ObjectId,
    retry: bool,
    failure: OutgoingFailure,
    fetched_ref: String,
) -> Result<PublicationAttempt> {
    if !retry {
        return Ok(PublicationAttempt::ReconciliationRequired {
            failure,
            fetched_ref,
        });
    }
    let (event, _) = record(
        store,
        input,
        io,
        OutgoingProgress::Reconciled {
            prepared: prepared.clone(),
            reconciliation: Box::new(OutgoingReconciliation::Stopped { failure }),
        },
    )
    .await?;
    Ok(PublicationAttempt::Stopped { event })
}

async fn observe(
    store: &mut SqliteStore,
    input: &InspectedExchange,
    io: &mut impl AttemptIo,
) -> Result<PublicationAttempt> {
    let observed_tip = io.tip().await.ok().flatten();
    let confirmed = observed_tip.as_deref() == Some(input.intent.commit.oid.as_str());
    let observation = if confirmed {
        PublicationObservation::Confirmed {
            observed_tip: input.intent.commit.oid.clone(),
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

pub(super) struct GitIo<'a> {
    pub(super) target: ExchangeTarget,
    pub(super) data: DataWorktree,
    pub(super) limits: RunLimits,
    pub(super) started: Instant,
    pub(super) cancel: &'a Cancellation,
    pub(super) operation: uuid::Uuid,
}

impl AttemptIo for GitIo<'_> {
    async fn revalidate(&mut self, input: &InspectedExchange) -> Result<()> {
        input
            .revalidate(remaining(self.limits, self.started)?, self.cancel)
            .await
    }

    async fn tip(&mut self) -> Result<Option<String>> {
        let output = run_remaining(
            &self.target,
            &ExchangeOperation::Observe,
            remaining(self.limits, self.started)?,
            self.cancel,
        )
        .await?;
        selected_tip(&output.stdout, &self.target.selection.reference)
    }

    async fn fetch(&mut self, tip: &str) -> Result<String> {
        oid(tip)?;
        let reference = format!("refs/engram-exchange/{}/publication/{tip}", self.operation);
        run_remaining(
            &self.target,
            &ExchangeOperation::Fetch {
                commit: tip.into(),
                retention_ref: reference.clone(),
            },
            remaining(self.limits, self.started)?,
            self.cancel,
        )
        .await?;
        let pin = retained::local(
            &self.target.selection.repository,
            &[
                "for-each-ref",
                "--format=%(objectname) %(symref)",
                &reference,
            ],
            remaining(self.limits, self.started)?,
            self.cancel,
        )
        .await?;
        ensure!(
            pin == format!("{tip} \n").as_bytes(),
            "fetched publication tip differs"
        );
        let bytes = retained::local(
            &self.target.selection.repository,
            &["cat-file", "commit", tip],
            remaining(self.limits, self.started)?,
            self.cancel,
        )
        .await?;
        crate::task_sync::git::observe_commit(tip, &bytes)?;
        self.data
            .check_commit_within(tip, Budget::within(self.started, self.limits.deadline)?)?;
        Ok(reference)
    }

    async fn ancestor(&mut self, tip: &str, commit: &str) -> Result<bool> {
        oid(tip)?;
        oid(commit)?;
        let output = retained::local(
            &self.target.selection.repository,
            &["rev-list", "--max-count=1", tip, "--not", commit, "--"],
            remaining(self.limits, self.started)?,
            self.cancel,
        )
        .await?;
        if output.is_empty() {
            return Ok(true);
        }
        let value = std::str::from_utf8(&output)?
            .strip_suffix('\n')
            .context("invalid publication ancestry output")?;
        oid(value)?;
        Ok(false)
    }

    async fn push(&mut self, commit: &str) -> std::result::Result<(), RunError> {
        run_remaining(
            &self.target,
            &ExchangeOperation::Push {
                commit: commit.into(),
            },
            remaining(self.limits, self.started)?,
            self.cancel,
        )
        .await
        .map(|_| ())
    }
}

fn selected_tip(bytes: &[u8], reference: &str) -> Result<Option<String>> {
    if bytes.is_empty() {
        return Ok(None);
    }
    let text = std::str::from_utf8(bytes)?;
    let line = text
        .strip_suffix('\n')
        .context("incomplete publication tip")?;
    let (tip, selected) = line.split_once('\t').context("invalid publication tip")?;
    ensure!(
        selected == reference,
        "publication returned another selected ref"
    );
    oid(tip)?;
    Ok(Some(tip.into()))
}

#[cfg(test)]
mod tests;
