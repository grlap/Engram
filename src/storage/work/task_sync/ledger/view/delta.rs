//! Receipt-first incoming lookup uses canonical metadata, never task payloads.

use super::{Connection, ObjectId, SqliteStore, StoreError, Transition, read, require};
use crate::task_sync::{
    delta::DeltaIntent,
    ledger::{CompletedExchange, Direction, EVENT_KIND, Event},
};
use rusqlite::{OptionalExtension, params};

impl SqliteStore {
    /// Read an original incoming delta outcome before retained files or current
    /// planning. Later movement does not alter it. This observation grants no
    /// application or private-continuity recovery authority.
    ///
    /// # Errors
    /// Refuses changed requests, broken canonical links or stale keyed views.
    pub fn completed_task_sync_delta_import(
        &self,
        binding: &ObjectId,
        intent: &DeltaIntent,
    ) -> Result<Option<CompletedExchange>, StoreError> {
        self.work_read_snapshot(|store| lookup(&store.connection, binding, intent))
    }
}

pub(in crate::storage::work::task_sync) fn lookup(
    connection: &Connection,
    binding_id: &ObjectId,
    intent: &DeltaIntent,
) -> Result<Option<CompletedExchange>, StoreError> {
    let mut budget = read::ReadBudget::default();
    let binding = read::binding_on(connection, binding_id, &mut budget)?;
    intent.validate(binding_id, &binding)?;
    require(
        intent.direction == Direction::Incoming,
        "delta import lookup requires an incoming request",
    )?;
    read::check_head(connection, binding_id)?;
    let Some(prepared) = read::request_by(connection, binding_id, intent.operation, &intent.key)?
    else {
        let orphan: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_task_sync_exchange_operations WHERE binding_id=?1 AND (operation=?2 OR operation_key=?3))",
            params![binding_id.as_str(),intent.operation.to_string(),&intent.key],|row|row.get(0),
        )?;
        require(
            !orphan,
            "orphan delta operation projection; repair projections explicitly",
        )?;
        return Ok(None);
    };
    let (recorded, sequence) =
        super::super::super::closure::membership::head::delta_preparation_on(
            connection, &prepared, binding_id, &binding,
        )?;
    require(
        recorded.as_ref() == intent,
        "delta intent differs from recorded request",
    )?;
    // Incoming strict preparation and application are one atomic operation.
    // Reading a pending fixture is supported, but no other transition can
    // masquerade as its one original outcome.
    let mut statement = connection.prepare(
        "SELECT object_id FROM objects INDEXED BY objects_task_sync_exchange_prepared
         WHERE object_kind='task_sync_exchange_event' AND json_valid(canonical_json)
           AND json_extract(canonical_json,'$.binding')=?1
           AND json_extract(canonical_json,'$.transition.prepared')=?2
         ORDER BY json_extract(canonical_json,'$.sequence') LIMIT 2",
    )?;
    let ids = statement
        .query_map(params![binding_id.as_str(), prepared.as_str()], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    require(
        ids.len() <= 1,
        "delta import has competing canonical outcomes",
    )?;
    let outcome = ids
        .first()
        .map(|id| {
            ObjectId::from_stored(id.clone())
                .ok_or_else(|| super::super::super::invalid("invalid delta outcome identity"))
        })
        .transpose()?;
    let event: Option<Event> = outcome
        .as_ref()
        .map(|id| read::object(connection, id, EVENT_KIND, &mut budget))
        .transpose()?;
    if let Some(event) = &event {
        require(
            event.schema_version == 1
                && event.binding == *binding_id
                && event.predecessor.as_ref() == Some(&prepared)
                && sequence.checked_add(1) == Some(event.sequence)
                && event.actor == intent.actor,
            "delta outcome binding, predecessor or attribution differs",
        )?;
        let selected = match &event.transition {
            Transition::DeltaImportApplied { prepared, receipt } => {
                receipt.validate(binding_id, &binding, intent)?;
                prepared
            }
            Transition::PlanningDeferred {
                prepared,
                hold,
                candidate_fingerprint,
                ..
            } => {
                require(
                    hold.as_str().len() == 32
                        && candidate_fingerprint == &intent.candidate.content_fingerprint,
                    "deferred planning outcome differs",
                )?;
                prepared
            }
            _ => {
                return Err(super::super::super::invalid(
                    "delta import outcome kind differs",
                ));
            }
        };
        require(
            selected == &prepared,
            "delta import outcome preparation differs",
        )?;
    }
    let row: Option<super::Cells> = connection
        .query_row(
            "SELECT operation_key,prepared_id,latest_event_id,completed_event_id,direction,stage
         FROM work_task_sync_exchange_operations WHERE binding_id=?1 AND operation=?2",
            params![binding_id.as_str(), intent.operation.to_string()],
            |row| (0..6).map(|index| row.get(index)).collect(),
        )
        .optional()?;
    let expected = vec![
        Some(intent.key.clone()),
        Some(prepared.to_string()),
        Some(outcome.as_ref().unwrap_or(&prepared).to_string()),
        outcome.as_ref().map(ToString::to_string),
        Some("incoming".into()),
        Some(
            if outcome.is_some() {
                "settled"
            } else {
                "prepared"
            }
            .into(),
        ),
    ];
    require(
        row.as_ref() == Some(&expected),
        "delta operation projection missing or stale; repair projections explicitly",
    )?;
    Ok(match (outcome, event) {
        (Some(outcome), Some(event)) => Some(CompletedExchange {
            prepared,
            outcome,
            event,
        }),
        _ => None,
    })
}
