//! Retain one native delta and append its preparation under checked custody.

use super::{
    Connection, DeltaIntent, Direction, Event, ObjectId, SqliteStore, StoreError, Transition,
    inspect_on, invalid, origin, producing_origin, require, view,
};
use crate::{CanonicalObject, task_sync::exchange::delta::InspectedDelta};
use chrono::{DateTime, Utc};

pub(super) fn input_on(
    connection: &Connection,
    binding_id: &ObjectId,
    intent: &DeltaIntent,
) -> Result<InspectedDelta, StoreError> {
    let mut budget = view::read::ReadBudget::default();
    let binding = view::read::binding_on(connection, binding_id, &mut budget)?;
    intent.validate(binding_id, &binding)?;
    require(
        intent.direction == Direction::Outgoing,
        "delta preparation is not outgoing",
    )?;
    super::super::local_store(connection, &binding)?;
    let payload = if let Some(prepared) =
        view::read::request_by(connection, binding_id, intent.operation, &intent.key)?
    {
        // A replay reads the original selected operation. The native project
        // and its export head may have moved since these bytes were admitted.
        let operation =
            view::read::operation_on(connection, binding_id, &binding, &prepared, &mut budget)?;
        require(
            matches!(&operation.intent, view::read::CheckedRequest::Delta(old) if old.as_ref() == intent),
            "outgoing delta request differs from its original preparation",
        )?;
        let (_, event) = operation
            .events
            .first()
            .ok_or_else(|| invalid("delta preparation missing"))?;
        let Transition::DeltaPrepared { payload, .. } = &event.transition else {
            return Err(invalid("delta preparation kind differs"));
        };
        *payload.clone()
    } else {
        // An interrupted preparation keeps its retained candidate: an exact
        // retry completes it with those bytes rather than recapturing native
        // state that a later local edit may have changed.
        let retained =
            crate::task_sync::retention::delta::read_candidate(binding_id, &binding, intent)
                .map_err(|error| invalid(error.to_string()))?;
        origin::with_origin_scope(connection, || {
            inspect_on(connection, binding_id, intent, retained.as_ref())
        })?
    };
    Ok(InspectedDelta {
        binding_id: binding_id.clone(),
        binding,
        intent: intent.clone(),
        payload,
    })
}

impl SqliteStore {
    /// Retain selected native export bytes and their canonical preparation.
    /// This local operation creates no Git commit, ref, transport or confirmed
    /// publication. The caller supplies an exact delta descriptor; it cannot
    /// substitute document bytes for the current native capture.
    ///
    /// Exact retries preserve the original event and retained bytes, including
    /// after later native edits, and also when the first attempt retained its
    /// candidate but its SQL commit failed. A partial retained request stays
    /// available for exact recovery and is never reconstructed as a different
    /// operation.
    ///
    /// # Errors
    /// Refuses changed scope, cuts, membership, request reuse, unresolved prior
    /// operations, custody uncertainty and byte limits before admitting SQL.
    pub fn prepare_task_sync_delta_export(
        &mut self,
        binding_id: &ObjectId,
        intent: &DeltaIntent,
        now: DateTime<Utc>,
    ) -> Result<ObjectId, StoreError> {
        crate::storage::admit_live_actor_session(&intent.actor)?;
        crate::task_sync::ledger::attribution(&intent.actor)?;
        let input =
            self.work_read_snapshot(|store| input_on(&store.connection, binding_id, intent))?;
        super::super::preflight(&input, now)?;
        let request = crate::task_sync::retention::delta::request_fingerprint(binding_id, intent)
            .map_err(|error| invalid(error.to_string()))?;
        producing_origin::with_exchange(
            self,
            &input.binding.project,
            &input.binding.repository.git_directory,
            Some(intent.operation),
            &request,
            |store, guard| {
                let transaction = store.begin_work_mutation()?;
                require(
                    guard.is_some(),
                    "outgoing delta requires private origin continuity",
                )?;
                producing_origin::check_exchange_on(
                    &transaction,
                    &input.binding.project,
                    guard,
                    &request,
                )?;
                let actual = input_on(&transaction, binding_id, intent)?;
                require(
                    actual.binding == input.binding && actual.payload == input.payload,
                    "outgoing delta changed before preparation",
                )?;
                // The binding, its successor and its tail are read by key, and
                // capacity from stored lengths; no other exchange is replayed.
                require(
                    view::select::successor(&transaction, binding_id)?.is_none(),
                    "outgoing delta binding was superseded",
                )?;
                require(
                    !super::super::super::recovery::quarantined(
                        &transaction,
                        binding_id,
                        &input.binding,
                    )?,
                    "outgoing delta binding was quarantined",
                )?;
                if let Some(prepared) =
                    view::read::request_by(&transaction, binding_id, intent.operation, &intent.key)?
                {
                    let retained = crate::task_sync::retention::delta::read_candidate(
                        binding_id,
                        &input.binding,
                        intent,
                    )
                    .map_err(|error| invalid(error.to_string()))?;
                    require(
                        retained.as_ref() == Some(&input.payload),
                        "prepared outgoing delta bytes are missing or changed",
                    )?;
                    return Ok(prepared);
                }
                let checked = view::read::tail_on(
                    &transaction,
                    binding_id,
                    &input.binding,
                    &mut view::read::ReadBudget::default(),
                )?;
                let (previous, tail) = (&checked.id, &checked.event);
                require(
                    super::super::super::settled_tail(tail),
                    "outgoing delta cannot bypass an unresolved operation",
                )?;
                let object = CanonicalObject::mint(&Event {
                    schema_version: 1,
                    binding: binding_id.clone(),
                    sequence: tail
                        .sequence
                        .checked_add(1)
                        .ok_or_else(|| invalid("delta sequence overflow"))?,
                    predecessor: Some(previous.clone()),
                    actor: intent.actor.clone(),
                    recorded_at: now,
                    transition: Transition::DeltaPrepared {
                        intent: Box::new(intent.clone()),
                        payload: Box::new(input.payload.clone()),
                    },
                })?;
                super::super::super::capacity::require_on(&transaction, &[&object])?;
                // Validate the shared reducer before the first retained write.
                view::append::propose(&transaction, &input.binding, &object)?;
                crate::task_sync::retention::delta::retain_candidate(
                    binding_id,
                    &input.binding,
                    intent,
                    &input.payload,
                )
                .map_err(|error| invalid(error.to_string()))?;
                super::super::super::operations::store_event_keyed(
                    &transaction,
                    &input.binding,
                    &object,
                )?;
                transaction.commit()?;
                Ok(object.key().clone())
            },
        )
    }
}
