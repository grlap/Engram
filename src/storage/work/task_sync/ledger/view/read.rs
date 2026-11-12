//! Bounded canonical binding and operation reads shared by ledger consumers.

use rusqlite::{Connection, OptionalExtension, params};
use std::collections::BTreeMap;

use super::super::super::invalid;
use super::super::reduce::{RequestProgress, Stage};
use super::super::{
    BINDING_KIND, Binding, CanonicalObject, EVENT_KIND, Event, MAX_BYTES, MAX_RECORD_BYTES,
    MAX_RECORDS, ObjectId, StoreError, Transition, require,
};
use super::{Cells, operation_cells};
use crate::task_sync::delta::DeltaIntent;
use crate::task_sync::ledger::Intent;

#[derive(Default)]
pub(in crate::storage::work::task_sync) struct ReadBudget {
    objects: BTreeMap<ObjectId, (String, usize)>,
    bytes: usize,
}

impl ReadBudget {
    // Capacity counts distinct canonical objects, not visits to those objects.
    // Retain metadata only: every visit still reads and decodes the stored bytes.
    fn admit(&mut self, id: &ObjectId, kind: &str, length: usize) -> Result<(), StoreError> {
        if let Some((previous_kind, previous_length)) = self.objects.get(id) {
            return require(
                previous_kind == kind && *previous_length == length,
                "exchange canonical record metadata differs between reads",
            );
        }
        let bytes = self
            .bytes
            .checked_add(length)
            .ok_or_else(|| invalid("exchange lookup exceeds byte bound"))?;
        require(bytes <= MAX_BYTES, "exchange lookup exceeds byte bound")?;
        require(
            self.objects.len() < MAX_RECORDS,
            "exchange lookup exceeds record bound",
        )?;
        self.objects.insert(id.clone(), (kind.to_owned(), length));
        self.bytes = bytes;
        Ok(())
    }
}

#[cfg(test)]
mod tests;

/// Bounded canonical-object read; never loads a task, origin or retained file.
pub(in crate::storage::work::task_sync) fn object<T: serde::de::DeserializeOwned>(
    connection: &Connection,
    id: &ObjectId,
    kind: &str,
    budget: &mut ReadBudget,
) -> Result<T, StoreError> {
    let limit = i64::try_from(MAX_RECORD_BYTES)
        .map_err(|_| invalid("exchange byte limit exceeds SQLite integer bound"))?;
    let stored: Option<(String, Option<Vec<u8>>)> = connection
        .query_row(
            "SELECT object_kind,CASE WHEN length(canonical_json)<=?2 THEN canonical_json END
         FROM objects WHERE object_id=?1",
            params![id.as_str(), limit],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((stored_kind, bytes)) = stored else {
        return Err(invalid(if kind == BINDING_KIND {
            "exchange binding is missing"
        } else {
            "exchange canonical record is missing; preserve the store and recover its canonical history"
        }));
    };
    require(
        stored_kind == kind,
        "exchange canonical record kind differs",
    )?;
    let bytes =
        bytes.ok_or_else(|| invalid("exchange canonical record exceeds supported byte bound"))?;
    budget.admit(id, &stored_kind, bytes.len())?;
    #[cfg(test)]
    super::READS.with(|count| count.set(count.get() + 1));
    CanonicalObject::stored(id, bytes)?.decode()
}

fn identities(
    connection: &Connection,
    sql: &str,
    binding: &ObjectId,
    selected: &str,
) -> Result<Vec<ObjectId>, StoreError> {
    let mut statement = connection.prepare(sql)?;
    let rows = statement.query_map(params![binding.as_str(), selected], |row| {
        row.get::<_, String>(0)
    })?;
    rows.map(|id| ObjectId::from_stored(id?).ok_or_else(|| invalid("invalid exchange identity")))
        .collect()
}

pub(in crate::storage::work::task_sync) fn prepared_by(
    connection: &Connection,
    binding: &ObjectId,
    intent: &crate::task_sync::ledger::Intent,
) -> Result<Option<ObjectId>, StoreError> {
    request_by(connection, binding, intent.operation, &intent.key)
}

pub(in crate::storage::work::task_sync) fn request_by(
    connection: &Connection,
    binding: &ObjectId,
    operation: uuid::Uuid,
    key: &str,
) -> Result<Option<ObjectId>, StoreError> {
    super::super::audit::retained::refuse_retired_on(connection, binding, operation, key)?;
    request_identity_on(connection, binding, operation, key)
}

/// Audit replay resolves its own checkpoint without granting ordinary request
/// replay to a retired construction.
pub(in crate::storage::work::task_sync) fn request_identity_on(
    connection: &Connection,
    binding: &ObjectId,
    operation: uuid::Uuid,
    key: &str,
) -> Result<Option<ObjectId>, StoreError> {
    let retry_key = identities(
        connection,
        "SELECT object_id FROM objects INDEXED BY objects_task_sync_exchange_reconciliation_key
         WHERE object_kind='task_sync_exchange_event' AND json_valid(canonical_json)
           AND json_extract(canonical_json,'$.transition.transition')='outgoing_reconciled'
           AND json_extract(canonical_json,'$.transition.reconciliation.step') IN ('resolved','delta_resolved')
           AND json_extract(canonical_json,'$.binding')=?1
           AND json_extract(canonical_json,'$.transition.reconciliation.resolved.key')=?2 LIMIT 1",
        binding,
        key,
    )?;
    require(
        retry_key.is_empty()
            && reconciliation_parent_for_operation(connection, binding, operation)?.is_none(),
        "exchange request aliases a retained reconciliation recipe",
    )?;
    let operation = identities(connection,
        "SELECT object_id FROM objects INDEXED BY objects_task_sync_exchange_operation
         WHERE object_kind='task_sync_exchange_event' AND json_valid(canonical_json) AND json_extract(canonical_json,'$.binding')=?1
         AND COALESCE(json_extract(canonical_json,'$.transition.intent.operation'),json_extract(canonical_json,'$.transition.checkpoint.operation'),json_extract(canonical_json,'$.transition.recipe.request.intent.operation'))=?2 LIMIT 2",binding,&operation.to_string())?;
    let key = identities(connection,
        "SELECT object_id FROM objects INDEXED BY objects_task_sync_exchange_key
         WHERE object_kind='task_sync_exchange_event' AND json_valid(canonical_json) AND json_extract(canonical_json,'$.binding')=?1
         AND COALESCE(json_extract(canonical_json,'$.transition.intent.key'),json_extract(canonical_json,'$.transition.checkpoint.key'),json_extract(canonical_json,'$.transition.recipe.request.intent.key'))=?2 LIMIT 2",binding,key)?;
    require(
        operation.len() <= 1 && operation == key,
        "exchange request key or operation differs from recorded intent",
    )?;
    Ok(operation.into_iter().next())
}

pub(in crate::storage::work::task_sync) fn prepared_for_operation(
    connection: &Connection,
    binding: &ObjectId,
    operation: uuid::Uuid,
) -> Result<Option<ObjectId>, StoreError> {
    let ids = identities(connection,
        "SELECT object_id FROM objects INDEXED BY objects_task_sync_exchange_operation
         WHERE object_kind='task_sync_exchange_event' AND json_valid(canonical_json) AND json_extract(canonical_json,'$.binding')=?1
           AND COALESCE(json_extract(canonical_json,'$.transition.intent.operation'),json_extract(canonical_json,'$.transition.checkpoint.operation'),json_extract(canonical_json,'$.transition.recipe.request.intent.operation'))=?2 LIMIT 2",
        binding,&operation.to_string())?;
    require(
        ids.len() <= 1,
        "duplicate exchange request key or operation",
    )?;
    Ok(ids.into_iter().next())
}

pub(in crate::storage::work::task_sync) fn reconciliation_parent_for_operation(
    connection: &Connection,
    binding: &ObjectId,
    operation: uuid::Uuid,
) -> Result<Option<ObjectId>, StoreError> {
    let ids = identities(connection,
        "SELECT json_extract(canonical_json,'$.transition.prepared') FROM objects
         INDEXED BY objects_task_sync_exchange_reconciliation
         WHERE object_kind='task_sync_exchange_event' AND json_valid(canonical_json)
           AND json_extract(canonical_json,'$.transition.transition')='outgoing_reconciled'
           AND json_extract(canonical_json,'$.transition.reconciliation.step') IN ('resolved','delta_resolved')
           AND json_extract(canonical_json,'$.binding')=?1
           AND json_extract(canonical_json,'$.transition.reconciliation.resolved.operation')=?2 LIMIT 2",
        binding, &operation.to_string())?;
    require(
        ids.len() <= 1,
        "reconciled retention operation has competing parents",
    )?;
    Ok(ids.into_iter().next())
}

pub(in crate::storage::work::task_sync) fn check_head(
    connection: &Connection,
    binding: &ObjectId,
) -> Result<(), StoreError> {
    let head: Option<(String, i64)> = connection
        .query_row(
            "SELECT tail_id,sequence FROM work_task_sync_exchange_heads WHERE binding_id=?1",
            [binding.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let head = head.ok_or_else(|| {
        invalid("exchange head projection missing; repair projections explicitly")
    })?;
    let mut query = connection.prepare(
        "SELECT object_id,json_extract(canonical_json,'$.sequence') FROM objects
         INDEXED BY objects_task_sync_exchange_sequence
         WHERE object_kind='task_sync_exchange_event' AND json_valid(canonical_json) AND json_extract(canonical_json,'$.binding')=?1
         ORDER BY json_extract(canonical_json,'$.sequence') DESC LIMIT 2")?;
    let tails = query
        .query_map([binding.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, rusqlite::types::Value>(1)?,
            ))
        })?
        .map(|row| {
            let (id, sequence) = row?;
            match sequence {
                rusqlite::types::Value::Integer(sequence) => Ok((id, sequence)),
                _ => Err(invalid(
                    "exchange sequence is invalid; preserve the store and recover its canonical history",
                )),
            }
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    require(
        tails.first() == Some(&head)
            && head.1 > 0
            && tails.get(1).is_none_or(|(_, sequence)| *sequence < head.1),
        "exchange head projection is stale or forked; repair projections explicitly",
    )?;
    let projected: Option<String> = connection.query_row(
        "SELECT last_checkpoint_id FROM work_task_sync_exchange_heads WHERE binding_id=?1",
        [binding.as_str()],
        |row| row.get(0),
    )?;
    let canonical: Option<String> = connection
        .query_row(
            "SELECT object_id FROM objects INDEXED BY objects_task_sync_exchange_checkpoint
         WHERE object_kind='task_sync_exchange_event' AND json_valid(canonical_json)
           AND json_type(canonical_json,'$.transition.checkpoint')='object'
           AND json_extract(canonical_json,'$.binding')=?1
         ORDER BY json_extract(canonical_json,'$.sequence') DESC LIMIT 1",
            [binding.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    require(
        projected == canonical,
        "custody checkpoint head differs; repair projections explicitly",
    )
}

pub(in crate::storage::work::task_sync) fn binding_on(
    connection: &Connection,
    id: &ObjectId,
    budget: &mut ReadBudget,
) -> Result<Binding, StoreError> {
    let binding: Binding = object(connection, id, BINDING_KIND, budget)?;
    binding.validate()?;
    Ok(binding)
}

pub(in crate::storage::work::task_sync) enum CheckedRequest {
    Legacy(Box<Intent>),
    Delta(Box<DeltaIntent>),
    Evidence(Box<crate::task_sync::ledger::EvidenceIntent>),
    Successor(Box<crate::task_sync::delta::PlanningSuccessorRecipe>),
}

impl CheckedRequest {
    pub(in crate::storage::work::task_sync) fn operation(&self) -> uuid::Uuid {
        match self {
            Self::Legacy(intent) => intent.operation,
            Self::Delta(intent) => intent.operation,
            Self::Evidence(intent) => intent.operation,
            Self::Successor(recipe) => recipe.request.intent.operation,
        }
    }

    fn key(&self) -> &str {
        match self {
            Self::Legacy(intent) => &intent.key,
            Self::Delta(intent) => &intent.key,
            Self::Evidence(intent) => &intent.key,
            Self::Successor(recipe) => &recipe.request.intent.key,
        }
    }

    pub(in crate::storage::work::task_sync) fn legacy(&self) -> Option<&Intent> {
        match self {
            Self::Legacy(intent) => Some(intent),
            Self::Delta(_) | Self::Evidence(_) | Self::Successor(_) => None,
        }
    }

    pub(in crate::storage::work::task_sync) fn cut(
        &self,
        event: &Event,
    ) -> crate::task_sync::progress::ExportCut {
        match self {
            Self::Legacy(intent) => intent.candidate.source_cut.clone(),
            Self::Evidence(intent) => crate::task_sync::progress::ExportCut {
                position: intent.local_cut.position.position,
                anchor: intent.local_cut.anchor.clone(),
            },
            Self::Successor(recipe) => {
                let cut = match &event.transition {
                    Transition::PlanningSuccessorApplied { receipt, .. } => &receipt.after_cut,
                    _ => &recipe.request.intent.local_cut,
                };
                crate::task_sync::progress::ExportCut {
                    position: cut.position.position,
                    anchor: cut.anchor.clone(),
                }
            }
            Self::Delta(intent) => {
                let cut = match &event.transition {
                    Transition::DeltaImportApplied { receipt, .. } => &receipt.after_cut,
                    Transition::PlanningDeferred { after_cut, .. } => after_cut,
                    _ => &intent.local_cut,
                };
                crate::task_sync::progress::ExportCut {
                    position: cut.position.position,
                    anchor: cut.anchor.clone(),
                }
            }
        }
    }
}

pub(in crate::storage::work::task_sync) struct CheckedOperation {
    pub(in crate::storage::work::task_sync) prepared: ObjectId,
    pub(in crate::storage::work::task_sync) intent: CheckedRequest,
    pub(in crate::storage::work::task_sync) events: Vec<(ObjectId, Event)>,
    pub(in crate::storage::work::task_sync::ledger) progress: RequestProgress,
}

impl CheckedOperation {
    pub(in crate::storage::work::task_sync) fn progress_intent(&self) -> Option<&Intent> {
        self.events
            .iter()
            .rev()
            .find_map(|(_, event)| event.transition.retained_intent())
    }

    pub(in crate::storage::work::task_sync) fn cut(
        &self,
        event: &Event,
    ) -> crate::task_sync::progress::ExportCut {
        self.progress_intent().map_or_else(
            || self.intent.cut(event),
            |intent| intent.candidate.source_cut.clone(),
        )
    }
}

/// The historical lookup and fresh origin readers use the same selected
/// canonical operation, reducer and exact projection comparison.
pub(in crate::storage::work::task_sync) fn operation_on(
    connection: &Connection,
    binding_id: &ObjectId,
    binding: &Binding,
    prepared: &ObjectId,
    budget: &mut ReadBudget,
) -> Result<CheckedOperation, StoreError> {
    let prepared_event: Event = object(connection, prepared, EVENT_KIND, budget)?;
    let intent = match &prepared_event.transition {
        Transition::Prepared { intent } if &prepared_event.binding == binding_id => {
            CheckedRequest::Legacy(intent.clone())
        }
        Transition::DeltaPrepared { intent, .. } if &prepared_event.binding == binding_id => {
            CheckedRequest::Delta(intent.clone())
        }
        Transition::PlanningSuccessorPrepared { recipe, .. }
            if &prepared_event.binding == binding_id =>
        {
            CheckedRequest::Successor(recipe.clone())
        }
        Transition::EvidencePrepared { intent } if &prepared_event.binding == binding_id => {
            CheckedRequest::Evidence(intent.clone())
        }
        _ => return Err(invalid("exchange intent differs from recorded request")),
    };
    require(
        request_by(connection, binding_id, intent.operation(), intent.key())?.as_ref()
            == Some(prepared),
        "exchange request key or operation differs from recorded intent",
    )?;
    let predecessor = prepared_event
        .predecessor
        .as_ref()
        .ok_or_else(|| invalid("prepared exchange predecessor is missing"))?;
    let previous: Event = object(connection, predecessor, EVENT_KIND, budget)?;
    require(
        &previous.binding == binding_id
            && previous.sequence.checked_add(1) == Some(prepared_event.sequence),
        "prepared exchange predecessor differs",
    )?;
    let record_limit = i64::try_from(MAX_RECORDS)
        .ok()
        .and_then(|limit| limit.checked_add(1))
        .ok_or_else(|| invalid("exchange record limit exceeds SQLite integer bound"))?;
    let mut query = connection.prepare(
        "SELECT object_id FROM objects INDEXED BY objects_task_sync_exchange_prepared
         WHERE object_kind='task_sync_exchange_event' AND json_valid(canonical_json) AND json_extract(canonical_json,'$.binding')=?1
           AND json_extract(canonical_json,'$.transition.prepared')=?2
         ORDER BY json_extract(canonical_json,'$.sequence') LIMIT ?3")?;
    let ids = query
        .query_map(
            params![binding_id.as_str(), prepared.as_str(), record_limit],
            |row| row.get::<_, String>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    require(
        ids.len() < MAX_RECORDS,
        "exchange operation exceeds supported bound",
    )?;
    let mut events = vec![(prepared.clone(), prepared_event)];
    for id in ids {
        let id =
            ObjectId::from_stored(id).ok_or_else(|| invalid("invalid exchange event identity"))?;
        let event = object(connection, &id, EVENT_KIND, budget)?;
        events.push((id, event));
    }
    let chain = events
        .iter()
        .map(|(id, event)| (id, event))
        .collect::<Vec<_>>();
    let reduced = super::super::reduce::operation(binding, &chain)?;
    let progress = reduced
        .operations
        .get(&intent.operation().to_string())
        .ok_or_else(|| invalid("exchange prepared operation is missing"))?
        .clone();
    let row: Option<Cells> = connection
        .query_row(
            "SELECT operation_key,prepared_id,latest_event_id,completed_event_id,direction,stage
         FROM work_task_sync_exchange_operations WHERE binding_id=?1 AND operation=?2",
            params![binding_id.as_str(), intent.operation().to_string()],
            |row| (0..6).map(|index| row.get(index)).collect(),
        )
        .optional()?;
    require(
        row.as_ref() == Some(&operation_cells(&progress)),
        "exchange operation projection missing or stale; repair projections explicitly",
    )?;
    Ok(CheckedOperation {
        prepared: prepared.clone(),
        intent,
        events,
        progress,
    })
}

pub(in crate::storage::work::task_sync) struct CheckedTail {
    pub(in crate::storage::work::task_sync) id: ObjectId,
    pub(in crate::storage::work::task_sync) event: Event,
    pub(in crate::storage::work::task_sync) operation: Option<CheckedOperation>,
}

pub(in crate::storage::work::task_sync) fn tail_on(
    connection: &Connection,
    binding_id: &ObjectId,
    binding: &Binding,
    budget: &mut ReadBudget,
) -> Result<CheckedTail, StoreError> {
    check_head(connection, binding_id)?;
    let (tail,sequence,pending,stage): (String,i64,Option<String>,String) = connection.query_row(
        "SELECT tail_id,sequence,pending_prepared_id,stage FROM work_task_sync_exchange_heads WHERE binding_id=?1",
        [binding_id.as_str()],|row|Ok((row.get(0)?,row.get(1)?,row.get(2)?,row.get(3)?)),
    )?;
    let tail =
        ObjectId::from_stored(tail).ok_or_else(|| invalid("invalid exchange tail identity"))?;
    let event: Event = object(connection, &tail, EVENT_KIND, budget)?;
    require(
        &event.binding == binding_id && u64::try_from(sequence).ok() == Some(event.sequence),
        "exchange tail binding or sequence differs",
    )?;
    let prepared = match &event.transition {
        Transition::Activated | Transition::CustodyCheckpoint { .. } => None,
        Transition::Prepared { .. }
        | Transition::DeltaPrepared { .. }
        | Transition::EvidencePrepared { .. }
        | Transition::PlanningSuccessorPrepared { .. } => Some(&tail),
        Transition::LocalCommitRecorded { prepared }
        | Transition::ImportApplied { prepared, .. }
        | Transition::ReconciledImportApplied { prepared, .. }
        | Transition::DeltaLocalCommitRecorded { prepared, .. }
        | Transition::DeltaImportApplied { prepared, .. }
        | Transition::PlanningDeferred { prepared, .. }
        | Transition::PlanningSuccessorApplied { prepared, .. }
        | Transition::EvidenceImportApplied { prepared, .. }
        | Transition::EvidenceStopped { prepared, .. }
        | Transition::PushStarted { prepared }
        | Transition::PushRejected { prepared, .. }
        | Transition::OutgoingReconciled { prepared, .. }
        | Transition::PushObserved { prepared, .. } => Some(prepared),
    };
    let operation = if let Some(prepared) = prepared {
        let operation = operation_on(connection, binding_id, binding, prepared, budget)?;
        let expected_pending =
            (operation.progress.stage != Stage::Settled).then(|| operation.prepared.to_string());
        require(
            operation.progress.latest == tail
                && operation
                    .events
                    .last()
                    .is_some_and(|(_, actual)| actual == &event)
                && stage == operation.progress.stage.text()
                && pending == expected_pending,
            "exchange tail operation or stage projection differs; repair projections explicitly",
        )?;
        Some(operation)
    } else {
        if let Transition::CustodyCheckpoint { checkpoint } = &event.transition {
            checkpoint.validate(binding_id, binding)?;
            let previous = event
                .predecessor
                .as_ref()
                .ok_or_else(|| invalid("custody checkpoint predecessor is missing"))?;
            let previous: Event = object(connection, previous, EVENT_KIND, budget)?;
            require(
                previous.binding == event.binding
                    && previous.sequence.checked_add(1) == Some(event.sequence)
                    && super::super::settled_tail(&previous),
                "custody checkpoint predecessor is not settled",
            )?;
            let prior_checkpoint: Option<String> = connection
                .query_row(
                    "SELECT object_id FROM objects INDEXED BY objects_task_sync_exchange_checkpoint
                 WHERE object_kind='task_sync_exchange_event' AND json_valid(canonical_json)
                   AND json_type(canonical_json,'$.transition.checkpoint')='object'
                   AND json_extract(canonical_json,'$.binding')=?1
                   AND json_extract(canonical_json,'$.sequence')<?2
                 ORDER BY json_extract(canonical_json,'$.sequence') DESC LIMIT 1",
                    params![binding_id.as_str(), sequence],
                    |row| row.get(0),
                )
                .optional()?;
            require(
                prior_checkpoint.as_deref()
                    == checkpoint
                        .previous_checkpoint
                        .as_ref()
                        .map(ObjectId::as_str),
                "custody checkpoint predecessor pointer differs",
            )?;
        } else {
            super::super::reduce::reduce(None, binding, &[(&tail, &event)])?;
        }
        require(
            stage == Stage::Settled.text() && pending.is_none(),
            "exchange activation projection differs; repair projections explicitly",
        )?;
        None
    };
    Ok(CheckedTail {
        id: tail,
        event,
        operation,
    })
}
