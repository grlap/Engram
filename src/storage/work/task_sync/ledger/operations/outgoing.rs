//! Persist observations only; transport and branch publication belong to callers.

use super::{
    DateTime, Direction, Event, InspectedExchange, Ledger, ObjectId, SqliteStore, StoreError,
    Transition, Utc, active, admit, append, basis, external, invalid, require, retention,
};
use crate::task_sync::ledger::OutgoingProgress;

mod reconciliation;

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
        self.record_task_sync_exchange_progress_once(input, progress, now)
            .map(|(id, _)| id)
    }

    /// The fresh flag is returned only after this call commits the transition.
    /// In particular, an exact retry of `PushStarted` cannot dispatch another push.
    pub(crate) fn record_task_sync_exchange_progress_once(
        &mut self,
        input: &InspectedExchange,
        progress: OutgoingProgress,
        now: DateTime<Utc>,
    ) -> Result<(ObjectId, bool), StoreError> {
        admit(input, &self.connection)?;
        require(
            input.intent.direction == Direction::Outgoing,
            "exchange is not outgoing",
        )?;
        let request = super::super::origin_request(
            "progress",
            &input.binding,
            Some(&input.binding_id),
            Some(&input.intent),
            Some(&progress),
        )?;
        super::super::super::producing_origin::with_exchange(
            self,
            &input.binding.project,
            &input.binding.repository.git_directory,
            Some(input.intent.operation),
            &request,
            |store, guard| {
                store.record_task_sync_exchange_progress_checked(
                    input, progress, now, guard, &request,
                )
            },
        )
    }

    fn record_task_sync_exchange_progress_checked(
        &mut self,
        input: &InspectedExchange,
        progress: OutgoingProgress,
        now: DateTime<Utc>,
        guard: Option<&crate::task_sync::origin::continuity::PreparedContinuity>,
        request: &ObjectId,
    ) -> Result<(ObjectId, bool), StoreError> {
        admit(input, &self.connection)?;
        require(
            input.intent.direction == Direction::Outgoing,
            "exchange is not outgoing",
        )?;
        let observed =
            super::super::custody::Observed::capture_cold(&input.binding.repository.git_directory)?;
        let transaction = self.begin_work_mutation()?;
        super::super::recovery::require_scope_on(&transaction, &input.binding)?;
        super::super::super::producing_origin::check_exchange_on(
            &transaction,
            &input.binding.project,
            guard,
            request,
        )?;
        let mut ledger = Ledger::load(&transaction)?;
        active(&ledger, input)?;
        external(&ledger, input, &observed)?;
        input
            .recheck_files()
            .map_err(|error| invalid(error.to_string()))?;
        require(
            basis(&transaction, &input.binding, Direction::Outgoing)?
                == input.intent.local_planning_basis,
            "local planning changed during outgoing exchange; reconcile retained state",
        )?;
        let prepared = reconciliation::prepared(&ledger, input, &progress)?;
        let transition = match progress {
            OutgoingProgress::LocalCommitRecorded => Transition::LocalCommitRecorded { prepared },
            OutgoingProgress::PushStarted => Transition::PushStarted { prepared },
            OutgoingProgress::PushRejected { push_started } => Transition::PushRejected {
                prepared,
                push_started,
            },
            OutgoingProgress::Observed(observation) => Transition::PushObserved {
                prepared,
                observation,
            },
            OutgoingProgress::Reconciled { reconciliation, .. } => Transition::OutgoingReconciled {
                prepared,
                reconciliation,
            },
        };
        if let Some((id, _)) = ledger
            .events
            .iter()
            .find(|(_, event)| event.binding == input.binding_id && event.transition == transition)
        {
            return Ok((id.clone(), false));
        }
        let object = append(&transaction, &mut ledger, input, transition, now)?;
        let event: Event = object.decode()?;
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
    }
}

#[cfg(test)]
mod tests;
