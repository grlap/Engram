//! A confirmed retry selects its own Git output, retaining the original native
//! preparation. Historical reads neither recapture planning nor touch files.

use super::{
    Binding, Connection, DeltaIntent, Event, ObjectId, StoreError, Transition, closure, invalid,
    require, view,
};
use crate::task_sync::{
    delta::ConfirmedExportBasis,
    ledger::{
        DeltaResolutionRecipe, EVENT_KIND, OutgoingFailure, OutgoingReconciliation,
        PublicationObservation, RetainedCommit,
    },
};
use rusqlite::params;

pub(super) struct Confirmation<'a> {
    pub binding_id: &'a ObjectId,
    pub binding: &'a Binding,
    pub declared: &'a ConfirmedExportBasis,
    pub original: &'a DeltaIntent,
    pub local_id: &'a ObjectId,
    pub local_sequence: u64,
    pub local_commit: &'a RetainedCommit,
    pub outcome: &'a Event,
}

impl Confirmation<'_> {
    pub(super) fn resolved_on(
        &self,
        connection: &Connection,
        budget: &mut view::read::ReadBudget,
    ) -> Result<bool, StoreError> {
        let mut query = connection.prepare("SELECT object_id FROM objects INDEXED BY objects_task_sync_exchange_prepared
            WHERE object_kind='task_sync_exchange_event' AND json_valid(canonical_json)
              AND json_extract(canonical_json,'$.binding')=?1
              AND json_extract(canonical_json,'$.transition.prepared')=?2
              AND json_extract(canonical_json,'$.transition.transition')='outgoing_reconciled'
              AND json_extract(canonical_json,'$.transition.reconciliation.step')='delta_resolved' LIMIT 2")?;
        let ids = query
            .query_map(
                params![self.binding_id.as_str(), self.declared.prepared.as_str()],
                |row| row.get::<_, String>(0),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        require(ids.len() <= 1, "confirmed delta resolution is forked")?;
        let Some(id) = ids.first() else {
            return Ok(false);
        };
        let id = ObjectId::from_stored(id.clone())
            .ok_or_else(|| invalid("invalid delta resolution identity"))?;
        let metadata =
            closure::membership::head::delta_resolution_on(connection, &id, self.binding_id)?;
        require(
            metadata.prepared == self.declared.prepared && metadata.actor == self.original.actor,
            "confirmed delta resolution preparation or attribution differs",
        )?;
        DeltaResolutionRecipe {
            failure: &metadata.failure,
            resolved: &metadata.resolved,
            commit: &metadata.commit,
        }
        .validate(
            self.binding_id,
            self.binding,
            &self.declared.prepared,
            self.original,
            self.local_commit,
        )?;
        require(
            metadata.commit.as_ref() == &self.declared.snapshot.commit
                && metadata.resolved.candidate.output == self.declared.snapshot.membership,
            "confirmed merged membership or retained commit differs",
        )?;
        let (previous, sequence) = match &metadata.failure {
            OutgoingFailure::AncestryChanged { .. } => (self.local_id.clone(), self.local_sequence),
            OutgoingFailure::NonFastForward { push_started, .. } => {
                let event: Event =
                    view::read::object(connection, push_started, EVENT_KIND, budget)?;
                require(
                    event.schema_version == 1
                        && event.binding == *self.binding_id
                        && event.actor == self.original.actor
                        && event.predecessor.as_ref() == Some(self.local_id)
                        && self.local_sequence.checked_add(1) == Some(event.sequence)
                        && matches!(&event.transition, Transition::PushStarted { prepared } if prepared == &self.declared.prepared),
                    "confirmed resolution refusal names another push start",
                )?;
                if metadata.predecessor == *push_started {
                    (push_started.clone(), event.sequence)
                } else {
                    // A refusal recorded before the replacement fetch sits
                    // between the start and its resolution.
                    let refusal: Event =
                        view::read::object(connection, &metadata.predecessor, EVENT_KIND, budget)?;
                    require(
                        refusal.schema_version == 1
                            && refusal.binding == *self.binding_id
                            && refusal.actor == self.original.actor
                            && refusal.predecessor.as_ref() == Some(push_started)
                            && event.sequence.checked_add(1) == Some(refusal.sequence)
                            && matches!(&refusal.transition, Transition::PushRejected { prepared, push_started: rejected }
                                if prepared == &self.declared.prepared && rejected == push_started),
                        "confirmed resolution refusal names another push start",
                    )?;
                    (metadata.predecessor.clone(), refusal.sequence)
                }
            }
        };
        require(
            metadata.predecessor == previous && sequence.checked_add(1) == Some(metadata.sequence),
            "confirmed resolution predecessor or sequence differs",
        )?;
        let mut current = Event::clone(self.outcome);
        loop {
            let previous = current
                .predecessor
                .as_ref()
                .ok_or_else(|| invalid("confirmed retry predecessor is missing"))?;
            let event: Event = view::read::object(connection, previous, EVENT_KIND, budget)?;
            require(
                event.schema_version == 1
                    && event.binding == *self.binding_id
                    && event.actor == self.original.actor
                    && event.sequence.checked_add(1) == Some(current.sequence),
                "confirmed retry publication chain differs",
            )?;
            match &event.transition {
                Transition::OutgoingReconciled {
                    prepared,
                    reconciliation,
                } if prepared == &self.declared.prepared
                    && matches!(reconciliation.as_ref(),
                        OutgoingReconciliation::PushStarted { reconciliation } if reconciliation == &id) =>
                {
                    require(
                        event.predecessor.as_ref() == Some(&id)
                            && metadata.sequence.checked_add(1) == Some(event.sequence),
                        "confirmed retry start differs from its resolution",
                    )?;
                    return Ok(true);
                }
                Transition::PushObserved {
                    prepared,
                    observation: PublicationObservation::Unknown { .. },
                } if prepared == &self.declared.prepared => current = event,
                _ => {
                    return Err(invalid(
                        "confirmed retry must follow its exact publication start",
                    ));
                }
            }
        }
    }
}
