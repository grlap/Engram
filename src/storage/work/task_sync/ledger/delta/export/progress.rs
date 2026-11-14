//! Durable dispatch admission for the original selected outgoing delta commit.

use super::{ObjectId, SqliteStore, StoreError, Transition, invalid, producing_origin, require};
use crate::task_sync::{
    delta::DeltaCommitSelection,
    exchange::delta::InspectedDelta,
    ledger::{Event, OutgoingProgress},
    retention,
};
use chrono::{DateTime, Utc};

impl SqliteStore {
    /// Only the invocation that commits a new transition receives a fresh flag.
    /// Replay and uncertain observations never authorize another dispatch.
    /// Reconciliation remains a separately admitted agent operation.
    pub(crate) fn record_task_sync_delta_progress_once(
        &mut self,
        input: &InspectedDelta,
        selection: &DeltaCommitSelection,
        progress: OutgoingProgress,
        now: DateTime<Utc>,
    ) -> Result<(ObjectId, bool), StoreError> {
        crate::storage::admit_live_actor_session(&input.intent.actor)?;
        selection.validate(
            &input.binding_id,
            &input.binding,
            &input.intent,
            &selection.prepared,
        )?;
        let request = retention::delta::progress_request_fingerprint(
            &input.binding_id,
            &input.intent,
            selection,
            &progress,
        )
        .map_err(|error| invalid(error.to_string()))?;
        let transition = match progress {
            OutgoingProgress::PushStarted => Transition::PushStarted {
                prepared: selection.prepared.clone(),
            },
            OutgoingProgress::PushRejected { push_started } => Transition::PushRejected {
                prepared: selection.prepared.clone(),
                push_started,
            },
            OutgoingProgress::Observed(observation) => Transition::PushObserved {
                prepared: selection.prepared.clone(),
                observation,
            },
            _ => {
                return Err(invalid(
                    "delta publication requires a start, a refusal or an observation",
                ));
            }
        };
        producing_origin::with_exchange(
            self,
            &input.binding.project,
            &input.binding.repository.git_directory,
            Some(input.intent.operation),
            &request,
            |store, guard| {
                let transaction = store.begin_work_mutation()?;
                require(
                    guard.is_some(),
                    "delta progress requires private origin continuity",
                )?;
                producing_origin::check_exchange_on(
                    &transaction,
                    &input.binding.project,
                    guard,
                    &request,
                )?;
                let operation = super::commit::checked_on(&transaction, input, selection, guard)?;
                require(
                    operation.events.iter().any(|(_, event)| {
                        event.binding == input.binding_id
                            && matches!(&event.transition,
                                Transition::DeltaLocalCommitRecorded { prepared, commit }
                                if prepared == &selection.prepared && commit == &selection.commit)
                    }),
                    "record the exact delta commit before publication",
                )?;
                if let Some((id, _)) = operation.events.iter().find(|(_, event)| {
                    event.binding == input.binding_id && event.transition == transition
                }) {
                    return Ok((id.clone(), false));
                }
                let mut chain =
                    super::super::Chain::resume(&transaction, input, &selection.prepared)?;
                let object =
                    super::super::append_keyed(&transaction, &mut chain, input, transition, now)?;
                let event: Event = object.decode()?;
                // Flush before SQL commit. Rollback or rewind leaves custody
                // uncertainty, never a second dispatch permission.
                if event.transition.publication_request().is_some() {
                    retention::retain_publication_witness(
                        &input.binding.repository.git_directory,
                        &retention::PublicationWitness {
                            operation: input.intent.operation,
                            event_id: object.key().clone(),
                            event,
                        },
                    )
                    .map_err(|error| invalid(error.to_string()))?;
                }
                transaction.commit()?;
                Ok((object.key().clone(), true))
            },
        )
    }
}
