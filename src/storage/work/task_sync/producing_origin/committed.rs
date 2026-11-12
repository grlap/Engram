//! Finish only the private acknowledgement of an exact committed receipt.

use super::{PreparedContinuity, cut_on, load_on, origin_facts_on, private};
use crate::storage::work::task_sync::ledger::{self, view};
use crate::task_sync::ledger::{
    CompletedExchange, EVENT_KIND, Event, EvidenceImportRecipe, EvidenceIntent, require,
};
use crate::{
    ObjectId,
    storage::{SqliteStore, StoreError},
};

impl SqliteStore {
    /// A mutating replay may finish its own proven committed witness. This is
    /// not custody admission: unknown carriers remain unknown, and no new
    /// exchange effect or offline lookup obtains a custody exception.
    ///
    /// # Errors
    /// Refuses a changed store, restore, origin, pending request or exact
    /// canonical successor, and leaves a failed private write recoverable.
    pub(crate) fn acknowledge_committed_task_sync_evidence(
        &mut self,
        binding_id: &ObjectId,
        intent: &EvidenceIntent,
        original: &CompletedExchange,
    ) -> Result<(), StoreError> {
        let binding = view::read::binding_on(
            &self.connection,
            binding_id,
            &mut view::read::ReadBudget::default(),
        )?;
        // Without an enrolled local origin there is no private witness to
        // acknowledge. Historical lookup remains independently usable.
        if load_on(&self.connection, &binding.project)?.is_none() {
            return Ok(());
        }
        ledger::check_local_store_on(&self.connection, &binding)?;
        let guard = PreparedContinuity::acquire(&binding.store.database, &binding.project)?;
        let witness = guard
            .state()
            .witness
            .as_ref()
            .ok_or_else(|| super::super::invalid("committed replay has no origin witness"))?;
        if witness.pending_exchange.is_none() {
            return Ok(());
        }
        if let Some(pending) = &guard.state().pending {
            return Err(private::pending_enrollment_error(pending));
        }
        if let Some(audit) = &guard.state().audit {
            return Err(private::pending_audit_error(audit));
        }
        crate::storage::admit_live_actor_session(&intent.actor)?;
        guard.recheck()?;
        let transaction = self.begin_work_mutation()?;
        ledger::check_local_store_on(&transaction, &binding)?;
        require(
            view::read::binding_on(
                &transaction,
                binding_id,
                &mut view::read::ReadBudget::default(),
            )? == binding
                && ledger::completed_evidence_on(&transaction, binding_id, intent)?.as_ref()
                    == Some(original),
            "committed replay differs from its original canonical receipt",
        )?;
        let preparation: Event =
            SqliteStore::get_typed_object_on(&transaction, &original.prepared, EVENT_KIND)?
                .ok_or_else(|| super::super::invalid("committed replay preparation is missing"))?;
        let recipe = EvidenceImportRecipe::recover(
            original.prepared.clone(),
            preparation,
            original.outcome.clone(),
            &original.event,
        )?;
        recipe.validate(binding_id, &binding)?;
        require(
            recipe.intent()? == intent,
            "committed replay request changed",
        )?;
        let request = crate::task_sync::retention::evidence::request_fingerprint(&recipe)
            .map_err(|error| super::super::invalid(error.to_string()))?;
        let (origin, witness, actual) = origin_facts_on(&transaction, &binding.project, &guard)?;
        require(
            witness.pending_exchange.as_ref() == Some(&request)
                && actual.as_ref().is_some_and(|anchor| {
                    &anchor.binding == binding_id
                        && anchor.head == original.outcome
                        && anchor.sequence == original.event.sequence
                })
                && ledger::matches_origin_recovery_on(
                    &transaction,
                    &binding.project,
                    &origin.origin_id,
                    witness.exchange.as_ref(),
                    &request,
                )?,
            "committed replay does not prove the exact pending successor; preserve uncertainty",
        )?;
        let mut next = witness.clone();
        next.cut = cut_on(&transaction, &binding.project)?;
        next.exchange = actual;
        next.pending_exchange = None;
        guard.recheck()?;
        // The canonical writer is still borrowed here. No canonical row is
        // written or committed; only this exact private acknowledgement moves.
        private::advance(guard.database(), &witness, &next)?;
        Ok(())
    }
}
