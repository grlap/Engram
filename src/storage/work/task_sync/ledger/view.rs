//! Rebuildable keyed exchange links. Canonical objects retain every intent and
//! receipt; these rows contain no second copy of either body.

use rusqlite::params;

use super::reduce::{ChainView, Head, RequestProgress};
use super::{Connection, Event, Ledger, ObjectId, SqliteStore, StoreError, Transition, require};
use crate::task_sync::ledger::{CompletedExchange, CompletedExchangeRequest, Direction};

pub(in crate::storage::work::task_sync::ledger) mod append;
pub(super) mod delta;
#[cfg(test)]
mod delta_tests;
pub(super) mod evidence;
pub(in crate::storage::work::task_sync) mod read;
pub(super) mod select;
pub(super) mod successor;

pub(in crate::storage::work) fn create_schema(connection: &Connection) -> Result<(), StoreError> {
    super::recovery::projection::create_schema(connection)?;
    append::create_schema(connection)?;
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS work_task_sync_exchange_operations (
             binding_id TEXT NOT NULL REFERENCES objects(object_id),
             operation TEXT NOT NULL,
             operation_key TEXT NOT NULL,
             prepared_id TEXT NOT NULL REFERENCES objects(object_id),
             latest_event_id TEXT NOT NULL REFERENCES objects(object_id),
             completed_event_id TEXT REFERENCES objects(object_id),
             direction TEXT NOT NULL CHECK(direction IN ('incoming','outgoing')),
             stage TEXT NOT NULL CHECK(stage IN ('settled','prepared','committed','pushing','unknown')),
             PRIMARY KEY(binding_id,operation),
             UNIQUE(binding_id,operation_key)
         ) STRICT;
         CREATE TABLE IF NOT EXISTS work_task_sync_exchange_heads (
             binding_id TEXT PRIMARY KEY REFERENCES objects(object_id),
             tail_id TEXT NOT NULL REFERENCES objects(object_id),
             sequence INTEGER NOT NULL CHECK(sequence > 0),
             pending_prepared_id TEXT REFERENCES objects(object_id),
             stage TEXT NOT NULL CHECK(stage IN ('settled','prepared','committed','pushing','unknown')),
             superseded_by TEXT REFERENCES objects(object_id),
             last_incoming_completed_id TEXT REFERENCES objects(object_id),
             last_outgoing_completed_id TEXT REFERENCES objects(object_id),
             last_checkpoint_id TEXT REFERENCES objects(object_id)
         ) STRICT;
         CREATE INDEX IF NOT EXISTS objects_task_sync_exchange_sequence
             ON objects(json_extract(canonical_json,'$.binding'),
                        json_extract(canonical_json,'$.sequence'))
             WHERE object_kind='task_sync_exchange_event' AND json_valid(canonical_json);
         CREATE INDEX IF NOT EXISTS objects_task_sync_exchange_operation
             ON objects(json_extract(canonical_json,'$.binding'),
                        COALESCE(json_extract(canonical_json,'$.transition.intent.operation'),
                                 json_extract(canonical_json,'$.transition.checkpoint.operation'),
                                 json_extract(canonical_json,'$.transition.recipe.request.intent.operation')))
             WHERE object_kind='task_sync_exchange_event' AND json_valid(canonical_json);
         CREATE INDEX IF NOT EXISTS objects_task_sync_exchange_key
             ON objects(json_extract(canonical_json,'$.binding'),
                        COALESCE(json_extract(canonical_json,'$.transition.intent.key'),
                                 json_extract(canonical_json,'$.transition.checkpoint.key'),
                                 json_extract(canonical_json,'$.transition.recipe.request.intent.key')))
             WHERE object_kind='task_sync_exchange_event' AND json_valid(canonical_json);
         CREATE INDEX IF NOT EXISTS objects_task_sync_exchange_prepared
             ON objects(json_extract(canonical_json,'$.binding'),
                        json_extract(canonical_json,'$.transition.prepared'),
                        json_extract(canonical_json,'$.sequence'))
             WHERE object_kind='task_sync_exchange_event' AND json_valid(canonical_json);
         CREATE INDEX IF NOT EXISTS objects_task_sync_exchange_checkpoint
             ON objects(json_extract(canonical_json,'$.binding'),json_extract(canonical_json,'$.sequence'))
             WHERE object_kind='task_sync_exchange_event' AND json_valid(canonical_json)
               AND json_type(canonical_json,'$.transition.checkpoint')='object';
         CREATE INDEX IF NOT EXISTS objects_task_sync_exchange_reconciliation
             ON objects(json_extract(canonical_json,'$.binding'),
                        json_extract(canonical_json,'$.transition.reconciliation.resolved.operation'))
             WHERE object_kind='task_sync_exchange_event' AND json_valid(canonical_json)
               AND json_extract(canonical_json,'$.transition.transition')='outgoing_reconciled'
               AND json_extract(canonical_json,'$.transition.reconciliation.step') IN ('resolved','delta_resolved');
         CREATE INDEX IF NOT EXISTS objects_task_sync_exchange_reconciliation_key
             ON objects(json_extract(canonical_json,'$.binding'),
                        json_extract(canonical_json,'$.transition.reconciliation.resolved.key'))
             WHERE object_kind='task_sync_exchange_event' AND json_valid(canonical_json)
               AND json_extract(canonical_json,'$.transition.transition')='outgoing_reconciled'
               AND json_extract(canonical_json,'$.transition.reconciliation.step') IN ('resolved','delta_resolved');",
    )?;
    select::create_schema(connection)
}

fn derived(ledger: &Ledger, id: &ObjectId) -> Result<ChainView, StoreError> {
    let binding = ledger
        .bindings
        .get(id)
        .ok_or_else(|| super::super::invalid("exchange binding is missing"))?;
    let mut chain = ledger
        .events
        .iter()
        .filter(|(_, event)| &event.binding == id)
        .collect::<Vec<_>>();
    chain.sort_by_key(|(_, event)| event.sequence);
    let mut result = super::reduce::reduce(None, binding, &chain)?;
    result.head.superseded_by = ledger
        .bindings
        .iter()
        .find(|(_, binding)| binding.supersedes.as_ref() == Some(id))
        .map(|(id, _)| id.clone());
    Ok(result)
}

fn direction(value: Direction) -> &'static str {
    match value {
        Direction::Incoming => "incoming",
        Direction::Outgoing => "outgoing",
    }
}

fn put_head(connection: &Connection, binding: &ObjectId, head: &Head) -> Result<(), StoreError> {
    let sequence = i64::try_from(head.sequence)
        .map_err(|_| super::super::invalid("exchange sequence exceeds SQLite integer bound"))?;
    connection.execute(
        "INSERT INTO work_task_sync_exchange_heads
         (binding_id,tail_id,sequence,pending_prepared_id,stage,superseded_by,
          last_incoming_completed_id,last_outgoing_completed_id,last_checkpoint_id)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8,?9)
         ON CONFLICT(binding_id) DO UPDATE SET tail_id=excluded.tail_id,
             sequence=excluded.sequence,pending_prepared_id=excluded.pending_prepared_id,
             stage=excluded.stage,superseded_by=excluded.superseded_by,
             last_incoming_completed_id=excluded.last_incoming_completed_id,
             last_outgoing_completed_id=excluded.last_outgoing_completed_id,
             last_checkpoint_id=excluded.last_checkpoint_id",
        params![
            binding.as_str(),
            head.tail.as_str(),
            sequence,
            head.pending.as_ref().map(ObjectId::as_str),
            head.stage.text(),
            head.superseded_by.as_ref().map(ObjectId::as_str),
            head.last_incoming.as_ref().map(ObjectId::as_str),
            head.last_outgoing.as_ref().map(ObjectId::as_str),
            head.last_checkpoint.as_ref().map(ObjectId::as_str)
        ],
    )?;
    Ok(())
}

fn put_operation(
    connection: &Connection,
    binding: &ObjectId,
    row: &RequestProgress,
) -> Result<(), StoreError> {
    connection.execute(
        "INSERT INTO work_task_sync_exchange_operations
         (binding_id,operation,operation_key,prepared_id,latest_event_id,completed_event_id,direction,stage)
         VALUES (?1,?2,?3,?4,?5,?6,?7,?8)
         ON CONFLICT(binding_id,operation) DO UPDATE SET operation_key=excluded.operation_key,
             prepared_id=excluded.prepared_id,latest_event_id=excluded.latest_event_id,
             completed_event_id=excluded.completed_event_id,direction=excluded.direction,stage=excluded.stage",
        params![binding.as_str(),row.operation,row.key,row.prepared.as_str(),row.latest.as_str(),
            row.completed.as_ref().map(ObjectId::as_str),direction(row.direction),row.stage.text()],
    )?;
    Ok(())
}

/// Called inside the existing canonical writer's transaction, after insertion.
/// Full ledger admission is unchanged; only the affected operation/head is written.
pub(super) fn record(
    connection: &Connection,
    ledger: &Ledger,
    event: &Event,
) -> Result<(), StoreError> {
    let view = derived(ledger, &event.binding)?;
    put_head(connection, &event.binding, &view.head)?;
    if let Some(row) = view
        .operations
        .values()
        .find(|row| row.latest == view.head.tail)
    {
        put_operation(connection, &event.binding, row)?;
    }
    if matches!(event.transition, Transition::Activated)
        && let Some(previous) = ledger
            .bindings
            .get(&event.binding)
            .and_then(|binding| binding.supersedes.as_ref())
    {
        put_head(connection, previous, &derived(ledger, previous)?.head)?;
    }
    Ok(())
}

pub(in crate::storage::work) fn rebuild(connection: &Connection) -> Result<(), StoreError> {
    let ledger = Ledger::load_canonical(connection)?;
    for id in ledger.bindings.keys() {
        let view = derived(&ledger, id)?;
        put_head(connection, id, &view.head)?;
        for row in view.operations.values() {
            put_operation(connection, id, row)?;
        }
    }
    Ok(())
}

/// Preserve semantic integrity findings alongside the other repair owners;
/// backend failures are errors, never a skipped projection.
pub(in crate::storage::work) fn rebuild_for_repair(
    connection: &Connection,
) -> Result<Vec<String>, StoreError> {
    match rebuild(connection) {
        Ok(()) => Ok(Vec::new()),
        Err(error) if canonical_problem(&error) => Ok(vec![super::failure_label(&error)]),
        Err(error) => Err(error),
    }
}

fn canonical_problem(error: &StoreError) -> bool {
    match error {
        StoreError::InvalidWork(_)
        | StoreError::InvalidStoredKey(_)
        | StoreError::ObjectKindMismatch { .. }
        | StoreError::Json(_)
        | StoreError::WorkNotFound(_)
        | StoreError::InvalidWorkProjection(_) => true,
        StoreError::TaskSyncRefused { inner, .. } => canonical_problem(inner),
        _ => false,
    }
}

type Cells = Vec<Option<String>>;

fn head_cells(head: &Head) -> Cells {
    vec![
        Some(head.tail.to_string()),
        Some(head.sequence.to_string()),
        head.pending.as_ref().map(ToString::to_string),
        Some(head.stage.text().into()),
        head.superseded_by.as_ref().map(ToString::to_string),
        head.last_incoming.as_ref().map(ToString::to_string),
        head.last_outgoing.as_ref().map(ToString::to_string),
        head.last_checkpoint.as_ref().map(ToString::to_string),
    ]
}

fn operation_cells(row: &RequestProgress) -> Cells {
    vec![
        Some(row.key.clone()),
        Some(row.prepared.to_string()),
        Some(row.latest.to_string()),
        row.completed.as_ref().map(ToString::to_string),
        Some(direction(row.direction).into()),
        Some(row.stage.text().into()),
    ]
}

pub(super) fn verify(connection: &Connection, ledger: &Ledger) -> Result<(), StoreError> {
    let mut heads = std::collections::BTreeMap::new();
    let mut operations = std::collections::BTreeMap::new();
    for id in ledger.bindings.keys() {
        let view = derived(ledger, id)?;
        heads.insert(id.to_string(), head_cells(&view.head));
        for row in view.operations.values() {
            operations.insert(
                (id.to_string(), row.operation.clone()),
                operation_cells(row),
            );
        }
    }
    let mut query = connection.prepare("SELECT binding_id,tail_id,CAST(sequence AS TEXT),pending_prepared_id,stage,superseded_by,last_incoming_completed_id,last_outgoing_completed_id,last_checkpoint_id FROM work_task_sync_exchange_heads")?;
    let actual = query
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                (1..9)
                    .map(|index| row.get(index))
                    .collect::<Result<Cells, _>>()?,
            ))
        })?
        .collect::<Result<std::collections::BTreeMap<_, _>, _>>()?;
    require(
        actual == heads,
        "exchange head projection differs; repair projections explicitly",
    )?;
    let mut query = connection.prepare("SELECT binding_id,operation,operation_key,prepared_id,latest_event_id,completed_event_id,direction,stage FROM work_task_sync_exchange_operations")?;
    let actual = query
        .query_map([], |row| {
            Ok((
                (row.get::<_, String>(0)?, row.get::<_, String>(1)?),
                (2..8)
                    .map(|index| row.get(index))
                    .collect::<Result<Cells, _>>()?,
            ))
        })?
        .collect::<Result<std::collections::BTreeMap<_, _>, _>>()?;
    require(
        actual == operations,
        "exchange operation projection differs; repair projections explicitly",
    )
}

impl SqliteStore {
    /// Resolve one exact delta operation through canonical records and keyed
    /// projection checks, without reading current task or retained file state.
    pub(crate) fn task_sync_delta_outgoing_events(
        &self,
        binding_id: &ObjectId,
        intent: &crate::task_sync::delta::DeltaIntent,
    ) -> Result<Vec<(ObjectId, Event)>, StoreError> {
        self.work_read_snapshot(|store| {
            let connection = &store.connection;
            let mut budget = read::ReadBudget::default();
            let binding = read::binding_on(connection, binding_id, &mut budget)?;
            intent.validate(binding_id, &binding)?;
            require(intent.direction == Direction::Outgoing, "delta is not outgoing")?;
            read::check_head(connection, binding_id)?;
            let prepared = read::request_by(connection, binding_id, intent.operation, &intent.key)?
                .ok_or_else(|| super::super::invalid("outgoing delta preparation is missing"))?;
            let operation = read::operation_on(connection, binding_id, &binding, &prepared, &mut budget)?;
            require(
                matches!(&operation.intent, read::CheckedRequest::Delta(old) if old.as_ref() == intent),
                "delta intent differs from recorded request",
            )?;
            Ok(operation.events)
        })
    }

    /// Read only the selected canonical operation, using the same keyed reducer
    /// and projection checks as historical completion. This grants no dispatch.
    pub(crate) fn task_sync_outgoing_events(
        &self,
        request: &CompletedExchangeRequest,
    ) -> Result<Vec<(ObjectId, Event)>, StoreError> {
        request.intent.validate()?;
        require(
            request.intent.direction == Direction::Outgoing,
            "exchange is not outgoing",
        )?;
        self.work_read_snapshot(|store| {
            let connection = &store.connection;
            let mut budget = read::ReadBudget::default();
            let binding = read::binding_on(connection, &request.binding, &mut budget)?;
            read::check_head(connection, &request.binding)?;
            let prepared = read::prepared_by(connection, &request.binding, &request.intent)?
                .ok_or_else(|| super::super::invalid("outgoing preparation is missing"))?;
            let operation = read::operation_on(
                connection,
                &request.binding,
                &binding,
                &prepared,
                &mut budget,
            )?;
            require(
                operation.intent.legacy() == Some(&request.intent),
                "exchange intent differs from recorded request",
            )?;
            Ok(operation.events)
        })
    }

    /// Resolve the original completed canonical outcome before inspecting payloads
    /// or current planning state. Later exchanges and supersession do not change it.
    /// This read grants no import, publication or continuation authority.
    ///
    /// # Errors
    /// Refuses changed intents, broken canonical links and missing/stale views.
    pub fn completed_task_sync_exchange(
        &self,
        request: &CompletedExchangeRequest,
    ) -> Result<Option<CompletedExchange>, StoreError> {
        request.intent.validate()?;
        self.work_read_snapshot(|store| lookup(&store.connection, request))
    }
}

fn lookup(
    connection: &Connection,
    request: &CompletedExchangeRequest,
) -> Result<Option<CompletedExchange>, StoreError> {
    let mut budget = read::ReadBudget::default();
    let binding = read::binding_on(connection, &request.binding, &mut budget)?;
    read::check_head(connection, &request.binding)?;
    let Some(prepared) = read::prepared_by(connection, &request.binding, &request.intent)? else {
        let orphan: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_task_sync_exchange_operations WHERE binding_id=?1 AND operation=?2)",
            params![request.binding.as_str(),request.intent.operation.to_string()], |row|row.get(0),
        )?;
        require(
            !orphan,
            "orphan exchange operation projection; repair projections explicitly",
        )?;
        return Ok(None);
    };
    let operation = read::operation_on(
        connection,
        &request.binding,
        &binding,
        &prepared,
        &mut budget,
    )?;
    require(
        operation.intent.legacy() == Some(&request.intent),
        "exchange intent differs from recorded request",
    )?;
    let Some(outcome) = operation.progress.completed else {
        return Ok(None);
    };
    let event = operation
        .events
        .into_iter()
        .find(|(id, _)| id == &outcome)
        .ok_or_else(|| super::super::invalid("completed exchange outcome is missing"))?
        .1;
    Ok(Some(CompletedExchange {
        prepared,
        outcome,
        event,
    }))
}

#[cfg(test)]
thread_local! { pub(super) static READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) }; }
