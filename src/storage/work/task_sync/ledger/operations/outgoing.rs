//! Persist observations only; transport and branch publication belong to callers.

use super::{
    DateTime, Direction, Event, InspectedExchange, Ledger, ObjectId, SqliteStore, StoreError,
    Transition, Utc, active, admit, append, basis, external, invalid, require, retention,
};
use crate::task_sync::ledger::OutgoingProgress;

impl SqliteStore {
    /// Record one outgoing step under the same serialized binding/basis checks
    /// as preparation. Exact retries return the original event identity. They
    /// do not repeat an external action or imply that an earlier action failed.
    ///
    /// # Errors
    /// Refuses changed requests, local drift, invalid order and unresolved state.
    pub(crate) fn record_task_sync_exchange_progress(
        &mut self,
        input: &InspectedExchange,
        progress: OutgoingProgress,
        now: DateTime<Utc>,
    ) -> Result<ObjectId, StoreError> {
        admit(input, &self.connection)?;
        require(
            input.intent.direction == Direction::Outgoing,
            "exchange is not outgoing",
        )?;
        let transaction = self.begin_work_mutation()?;
        let mut ledger = Ledger::load(&transaction)?;
        active(&ledger, input)?;
        external(&ledger, input)?;
        input
            .recheck_files()
            .map_err(|error| invalid(error.to_string()))?;
        require(
            basis(&transaction, &input.binding, Direction::Outgoing)?
                == input.intent.local_planning_basis,
            "local planning changed during outgoing exchange; reconcile retained state",
        )?;
        let prepared = ledger
            .events
            .iter()
            .find_map(|(id, event)| match &event.transition {
                Transition::Prepared { intent }
                    if event.binding == input.binding_id
                        && intent.operation == input.intent.operation =>
                {
                    Some((id.clone(), intent))
                }
                _ => None,
            })
            .ok_or_else(|| invalid("exchange intent is missing; reconcile retained artifacts"))?;
        require(
            prepared.1.as_ref() == &input.intent,
            "exchange intent differs from recorded request",
        )?;
        let transition = match progress {
            OutgoingProgress::LocalCommitRecorded => Transition::LocalCommitRecorded {
                prepared: prepared.0,
            },
            OutgoingProgress::PushStarted => Transition::PushStarted {
                prepared: prepared.0,
            },
            OutgoingProgress::Observed(observation) => Transition::PushObserved {
                prepared: prepared.0,
                observation,
            },
        };
        if let Some((id, _)) = ledger
            .events
            .iter()
            .find(|(_, event)| event.binding == input.binding_id && event.transition == transition)
        {
            return Ok(id.clone());
        }
        let object = append(&transaction, &mut ledger, input, transition, now)?;
        let event: Event = object.decode()?;
        if matches!(event.transition, Transition::PushStarted { .. }) {
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
        Ok(object.key().clone())
    }
}

#[cfg(test)]
mod tests;
