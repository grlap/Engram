//! Private incoming retention shares the applying writer's exact reservation.

use super::{
    DateTime, DeltaBasis, Direction, InspectedDelta, SqliteStore, StoreError, Utc, fresh, invalid,
    preflight, producing_origin, require, task_sync, view,
};
use crate::task_sync::delta::RetainedDeltaCandidate;

impl SqliteStore {
    pub(crate) fn retain_task_sync_incoming_delta(
        &mut self,
        input: &InspectedDelta,
        now: DateTime<Utc>,
    ) -> Result<RetainedDeltaCandidate, StoreError> {
        crate::storage::admit_live_actor_session(&input.intent.actor)?;
        task_sync::ledger::attribution(&input.intent.actor)?;
        preflight(input, now)?;
        require(
            input.intent.direction == Direction::Incoming,
            "delta retention requires incoming direction",
        )?;
        if self
            .completed_task_sync_delta_import(&input.binding_id, &input.intent)?
            .is_some()
        {
            input
                .recheck_candidate()
                .map_err(|error| invalid(error.to_string()))?;
            return Ok(input.intent.candidate.clone());
        }
        fresh(&self.connection, input)?;
        let request =
            task_sync::retention::delta::request_fingerprint(&input.binding_id, &input.intent)
                .map_err(|error| invalid(error.to_string()))?;
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
                    "incoming retention requires private origin continuity",
                )?;
                producing_origin::check_exchange_on(
                    &transaction,
                    &input.binding.project,
                    guard,
                    &request,
                )?;
                fresh(&transaction, input)?;
                let mut budget = view::read::ReadBudget::default();
                let binding = view::read::binding_on(&transaction, &input.binding_id, &mut budget)?;
                require(
                    binding == input.binding
                        && view::select::successor(&transaction, &input.binding_id)?.is_none(),
                    "delta retention binding changed or was superseded",
                )?;
                require(
                    !super::super::recovery::quarantined(
                        &transaction,
                        &input.binding_id,
                        &input.binding,
                    )?,
                    "delta retention binding was quarantined",
                )?;
                require(
                    super::super::settled_tail(
                        &view::read::tail_on(
                            &transaction,
                            &input.binding_id,
                            &binding,
                            &mut budget,
                        )?
                        .event,
                    ),
                    "incoming retention cannot bypass an unresolved operation",
                )?;
                super::deferred::refuse_held_scope_on(&transaction, input)?;
                let DeltaBasis::Incoming { source_after, .. } = &input.intent.basis else {
                    return Err(invalid("incoming delta basis missing"));
                };
                require(
                    task_sync::retention::commit_selection(
                        &input.binding.repository.git_directory,
                        input.intent.operation,
                    )
                    .map_err(|error| invalid(error.to_string()))?
                    .as_ref()
                        == Some(&source_after.commit),
                    "retained incoming commit selection differs",
                )?;
                task_sync::retention::delta::retain_candidate(
                    &input.binding_id,
                    &input.binding,
                    &input.intent,
                    &input.payload,
                )
                .map_err(|error| invalid(error.to_string()))
            },
        )
    }
}
