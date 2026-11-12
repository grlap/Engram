//! Validate an append from the canonical head and its selected operation only.

use super::super::reduce::Stage;
use super::super::{
    Binding, CanonicalObject, Event, ObjectId, StoreError, Transition, reduce, require,
};
use super::{ChainView, Head, head_cells, read, select};
use rusqlite::{Connection, OptionalExtension, params};

pub(super) fn create_schema(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch(
        "CREATE INDEX IF NOT EXISTS objects_task_sync_exchange_incoming_completed
             ON objects(json_extract(canonical_json,'$.binding'),json_extract(canonical_json,'$.sequence'))
             WHERE object_kind='task_sync_exchange_event' AND json_valid(canonical_json)
             AND json_extract(canonical_json,'$.transition.transition') IN ('import_applied','reconciled_import_applied','delta_import_applied','planning_successor_applied');
         CREATE INDEX IF NOT EXISTS objects_task_sync_exchange_outgoing_completed
             ON objects(json_extract(canonical_json,'$.binding'),json_extract(canonical_json,'$.sequence'))
             WHERE object_kind='task_sync_exchange_event' AND json_valid(canonical_json)
             AND json_extract(canonical_json,'$.transition.transition')='push_observed'
             AND json_extract(canonical_json,'$.transition.observation.outcome')='confirmed';
         CREATE INDEX IF NOT EXISTS objects_task_sync_exchange_delta_planning
             ON objects(json_extract(canonical_json,'$.transition.receipt.planning'))
             WHERE object_kind='task_sync_exchange_event' AND json_valid(canonical_json)
             AND json_extract(canonical_json,'$.transition.transition') IN ('delta_import_applied','planning_successor_applied');
         CREATE INDEX IF NOT EXISTS objects_task_sync_exchange_successor_hold
             ON objects(json_extract(canonical_json,'$.binding'),
                        json_extract(canonical_json,'$.transition.recipe.request.hold'))
             WHERE object_kind='task_sync_exchange_event' AND json_valid(canonical_json)
               AND json_extract(canonical_json,'$.transition.transition')='planning_successor_prepared';"
    )?;
    Ok(())
}

fn completed(
    connection: &Connection,
    binding: &ObjectId,
    incoming: bool,
) -> Result<Option<ObjectId>, StoreError> {
    let (index, predicate) = if incoming {
        (
            "incoming",
            "json_extract(canonical_json,'$.transition.transition') IN ('import_applied','reconciled_import_applied','delta_import_applied','planning_successor_applied')",
        )
    } else {
        (
            "outgoing",
            "json_extract(canonical_json,'$.transition.transition')='push_observed' AND json_extract(canonical_json,'$.transition.observation.outcome')='confirmed'",
        )
    };
    let value: Option<String> = connection
        .query_row(
            &format!(
        "SELECT object_id FROM objects INDEXED BY objects_task_sync_exchange_{index}_completed
         WHERE object_kind='task_sync_exchange_event' AND json_valid(canonical_json) AND {predicate}
           AND json_extract(canonical_json,'$.binding')=?1
         ORDER BY json_extract(canonical_json,'$.sequence') DESC LIMIT 1"),
            [binding.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    value
        .map(|value| {
            ObjectId::from_stored(value)
                .ok_or_else(|| super::super::super::invalid("invalid completed exchange identity"))
        })
        .transpose()
}

fn checked_head(
    connection: &Connection,
    id: &ObjectId,
    tail: &read::CheckedTail,
) -> Result<Head, StoreError> {
    let last_checkpoint: Option<String> = connection.query_row(
        "SELECT last_checkpoint_id FROM work_task_sync_exchange_heads WHERE binding_id=?1",
        [id.as_str()],
        |row| row.get(0),
    )?;
    let head = Head {
        tail: tail.id.clone(),
        sequence: tail.event.sequence,
        pending: tail.operation.as_ref().and_then(|operation| {
            (operation.progress.stage != Stage::Settled).then(|| operation.prepared.clone())
        }),
        stage: tail
            .operation
            .as_ref()
            .map_or(Stage::Settled, |operation| operation.progress.stage),
        superseded_by: select::successor(connection, id)?,
        last_incoming: completed(connection, id, true)?,
        last_outgoing: completed(connection, id, false)?,
        last_checkpoint: last_checkpoint
            .map(|id| {
                ObjectId::from_stored(id)
                    .ok_or_else(|| super::super::super::invalid("invalid checkpoint identity"))
            })
            .transpose()?,
    };
    let actual: super::Cells = connection.query_row(
        "SELECT tail_id,CAST(sequence AS TEXT),pending_prepared_id,stage,superseded_by,last_incoming_completed_id,last_outgoing_completed_id,last_checkpoint_id
         FROM work_task_sync_exchange_heads WHERE binding_id=?1", [id.as_str()],
        |row| (0..8).map(|index| row.get(index)).collect())?;
    require(
        actual == head_cells(&head),
        "exchange head projection differs; repair projections explicitly",
    )?;
    Ok(head)
}

pub(in crate::storage::work::task_sync::ledger) fn propose(
    connection: &Connection,
    binding: &Binding,
    object: &CanonicalObject,
) -> Result<ChainView, StoreError> {
    let event: Event = object.decode()?;
    super::super::audit::retained::validate_checkpoint_on(connection, object.key(), &event)?;
    if let Transition::DeltaImportApplied { receipt, .. }
    | Transition::PlanningSuccessorApplied { receipt, .. } = &event.transition
    {
        let exists: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM objects INDEXED BY objects_task_sync_exchange_delta_planning
             WHERE object_kind='task_sync_exchange_event' AND json_valid(canonical_json)
               AND json_extract(canonical_json,'$.transition.transition') IN ('delta_import_applied','planning_successor_applied')
               AND json_extract(canonical_json,'$.transition.receipt.planning')=?1)",
            params![receipt.planning.as_str()], |row| row.get(0))?;
        require(!exists, "delta planning is shared by distinct outcomes")?;
    }
    let mut budget = read::ReadBudget::default();
    let tail = read::tail_on(connection, &event.binding, binding, &mut budget)?;
    let head = checked_head(connection, &event.binding, &tail)?;
    require(
        head.superseded_by.is_none(),
        "exchange binding was superseded",
    )?;
    require(
        event.predecessor.as_ref() == Some(&head.tail)
            && head.sequence.checked_add(1) == Some(event.sequence),
        "exchange predecessor chain is broken",
    )?;
    let next_request = match &event.transition {
        Transition::Prepared { intent } => Some((intent.operation, intent.key.as_str())),
        Transition::DeltaPrepared { intent, .. } => Some((intent.operation, intent.key.as_str())),
        Transition::EvidencePrepared { intent } => Some((intent.operation, intent.key.as_str())),
        Transition::PlanningSuccessorPrepared { recipe, .. } => Some((
            recipe.request.intent.operation,
            recipe.request.intent.key.as_str(),
        )),
        Transition::CustodyCheckpoint { checkpoint } => {
            Some((checkpoint.operation, checkpoint.key.as_str()))
        }
        _ => None,
    };
    if let Some((operation, key)) = next_request {
        require(
            read::request_by(connection, &event.binding, operation, key)?.is_none(),
            "duplicate exchange request key or operation",
        )?;
        return reduce::next(connection, binding, &head, &[(object.key(), &event)]);
    }
    let operation = tail
        .operation
        .ok_or_else(|| super::super::super::invalid("exchange transition has no intent"))?;
    require(
        head.pending.as_ref() == Some(&operation.prepared),
        "exchange transition has no pending intent",
    )?;
    let mut chain = operation
        .events
        .iter()
        .map(|(id, event)| (id, event))
        .collect::<Vec<_>>();
    chain.push((object.key(), &event));
    let mut result = reduce::continued(connection, binding, &chain)?;
    if result.head.last_incoming.is_none() {
        result.head.last_incoming = head.last_incoming;
    }
    if result.head.last_outgoing.is_none() {
        result.head.last_outgoing = head.last_outgoing;
    }
    result.head.last_checkpoint = head.last_checkpoint;
    Ok(result)
}

pub(in crate::storage::work::task_sync::ledger) fn record(
    connection: &Connection,
    binding: &ObjectId,
    proposed: &ChainView,
) -> Result<(), StoreError> {
    super::put_head(connection, binding, &proposed.head)?;
    for row in proposed.operations.values() {
        super::put_operation(connection, binding, row)?;
    }
    Ok(())
}
