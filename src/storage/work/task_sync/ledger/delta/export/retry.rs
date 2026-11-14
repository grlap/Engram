//! Start the one resolved retry once, or observe/stop that same attempt.

use super::{ObjectId, SqliteStore, StoreError, Transition, invalid, producing_origin, require};
use crate::task_sync::{
    exchange::InspectedDeltaResolution,
    ledger::{Event, OutgoingProgress, OutgoingReconciliation},
    retention,
};
use chrono::{DateTime, Utc};

impl SqliteStore {
    pub(crate) fn record_task_sync_delta_retry_once(
        &mut self,
        input: &InspectedDeltaResolution,
        progress: &OutgoingProgress,
        now: DateTime<Utc>,
    ) -> Result<(ObjectId, bool), StoreError> {
        crate::storage::admit_live_actor_session(&input.original.intent.actor)?;
        input
            .recheck_files()
            .map_err(|error| invalid(error.to_string()))?;
        let request = retention::delta::retry_request_fingerprint(
            &input.original.binding_id,
            &input.original.intent,
            &input.selection,
            &input.recipe,
            progress,
        )
        .map_err(|error| invalid(error.to_string()))?;
        let operation = input
            .operation()
            .map_err(|error| invalid(error.to_string()))?;
        producing_origin::with_exchange(
            self,
            &input.original.binding.project,
            &input.original.binding.repository.git_directory,
            Some(operation),
            &request,
            |store, guard| {
                let transaction = store.begin_work_mutation()?;
                require(
                    guard.is_some(),
                    "delta retry requires private origin continuity",
                )?;
                producing_origin::check_exchange_on(
                    &transaction,
                    &input.original.binding.project,
                    guard,
                    &request,
                )?;
                let selected = super::commit::checked_on(
                    &transaction,
                    &input.original,
                    &input.selection,
                    guard,
                )?;
                let (recipe_id, recipe_event) = selected.events.iter().find(|(_, event)|
                event.binding == input.original.binding_id && matches!(&event.transition,
                    Transition::OutgoingReconciled {prepared, reconciliation}
                    if prepared == &input.selection.prepared && reconciliation.as_ref() == &input.recipe))
                .ok_or_else(|| invalid("record the exact resolved delta recipe before retry progress"))?;
                let recipe_sequence = recipe_event.sequence;
                let transition = match progress {
                    OutgoingProgress::PushStarted => Transition::OutgoingReconciled {
                        prepared: input.selection.prepared.clone(),
                        reconciliation: Box::new(OutgoingReconciliation::PushStarted {
                            reconciliation: recipe_id.clone(),
                        }),
                    },
                    OutgoingProgress::PushRejected { push_started } => Transition::PushRejected {
                        prepared: input.selection.prepared.clone(),
                        push_started: push_started.clone(),
                    },
                    OutgoingProgress::Observed(observation) => Transition::PushObserved {
                        prepared: input.selection.prepared.clone(),
                        observation: observation.clone(),
                    },
                    OutgoingProgress::Reconciled {
                        prepared,
                        reconciliation,
                    } if prepared == &input.selection.prepared
                        && matches!(
                            reconciliation.as_ref(),
                            OutgoingReconciliation::Stopped { .. }
                        ) =>
                    {
                        Transition::OutgoingReconciled {
                            prepared: prepared.clone(),
                            reconciliation: reconciliation.clone(),
                        }
                    }
                    _ => {
                        return Err(invalid(
                            "delta retry requires its start, observation or explicit stop",
                        ));
                    }
                };
                if let Some((id, _)) = selected.events.iter().find(|(_, event)| {
                    event.binding == input.original.binding_id
                        && event.sequence > recipe_sequence
                        && event.transition == transition
                }) {
                    return Ok((id.clone(), false));
                }
                input
                    .recheck_files()
                    .map_err(|error| invalid(error.to_string()))?;
                let mut chain = super::super::Chain::resume(
                    &transaction,
                    &input.original,
                    &input.selection.prepared,
                )?;
                let object = super::super::append_keyed(
                    &transaction,
                    &mut chain,
                    &input.original,
                    transition,
                    now,
                )?;
                let event: Event = object.decode()?;
                if event.transition.publication_request().is_some() {
                    // The resolved operation owns this distinct witness. A rollback
                    // keeps it ahead of SQL; it never permits another dispatch.
                    retention::retain_publication_witness(
                        &input.original.binding.repository.git_directory,
                        &retention::PublicationWitness {
                            operation,
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
