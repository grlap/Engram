//! Admit an inspected merged recipe without a second preparation or dispatch.

use super::{ObjectId, SqliteStore, StoreError, Transition, invalid, producing_origin, require};
use crate::task_sync::{exchange::InspectedDeltaResolution, ledger::OutgoingReconciliation};
use chrono::{DateTime, Utc};

impl SqliteStore {
    pub(crate) fn record_task_sync_delta_resolution(
        &mut self,
        input: &InspectedDeltaResolution,
        now: DateTime<Utc>,
    ) -> Result<ObjectId, StoreError> {
        crate::storage::admit_live_actor_session(&input.original.intent.actor)?;
        input
            .recheck_files()
            .map_err(|error| invalid(error.to_string()))?;
        let request = input
            .request_fingerprint()
            .map_err(|error| invalid(error.to_string()))?;
        producing_origin::with_delta_resolution(self, input, &request, |store, guard| {
            let transaction = store.begin_work_mutation()?;
            require(
                guard.is_some(),
                "delta resolution requires private origin continuity",
            )?;
            producing_origin::check_exchange_on(
                &transaction,
                &input.original.binding.project,
                guard,
                &request,
            )?;
            let operation = super::commit::checked_with_resolution_on(
                &transaction,
                &input.original,
                &input.selection,
                Some(input),
                guard,
            )?;
            require(operation.events.iter().any(|(_, event)| event.binding == input.original.binding_id
                && matches!(&event.transition, Transition::DeltaLocalCommitRecorded {prepared,commit}
                    if prepared == &input.selection.prepared && commit == &input.selection.commit)),
                "delta resolution original selected commit missing")?;
            let transition = Transition::OutgoingReconciled {
                prepared: input.selection.prepared.clone(),
                reconciliation: Box::new(input.recipe.clone()),
            };
            if let Some((id, _)) = operation.events.iter().find(|(_, event)| {
                event.binding == input.original.binding_id && event.transition == transition
            }) {
                return Ok(id.clone());
            }
            let OutgoingReconciliation::DeltaResolved {
                resolved, payload, ..
            } = &input.recipe
            else {
                return Err(invalid("delta resolution recipe missing"));
            };
            // The merged Git membership is admitted as a change to the
            // original preparation's export membership. Its graph is checked
            // on the component the change touches, both ways, so an unchanged
            // task still pointing at a removed one refuses without a read of
            // every merged task.
            crate::storage::work::task_sync::closure::overlay::check_export_change_on(
                &transaction,
                &input.original.binding_id,
                &input.original.binding.project,
                &input.original.intent.candidate.output,
                &payload.documents,
            )?;
            require(
                super::view::read::request_by(
                    &transaction,
                    &input.original.binding_id,
                    resolved.operation,
                    &resolved.key,
                )?
                .is_none()
                    && super::view::read::reconciliation_parent_for_operation(
                        &transaction,
                        &input.original.binding_id,
                        resolved.operation,
                    )?
                    .is_none(),
                "delta resolution operation or key already retained",
            )?;
            // The common reducer proves the exact failure predecessor, original
            // actor/native cut, two parents and the single shared retry budget.
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
            transaction.commit()?;
            Ok(object.key().clone())
        })
    }
}
