//! Restored work history, completion classification, and evidence readers.

use super::{
    CompletionSeal, Connection, ObjectId, OptionalExtension, RestoredRecord, RestoredWorkEvidence,
    StoreError, WorkId, WorkItem, WorkLifecycle, WorkRunState,
    latest_canonical_work_event_for_item_optional, load_typed_work_object, params,
};

pub(in crate::storage::work) fn work_completed_by_restored_record_on(
    connection: &Connection,
    item: &WorkItem,
) -> Result<bool, StoreError> {
    if !item.restored || item.lifecycle != WorkLifecycle::Completed || item.active_run_id.is_some()
    {
        return Ok(false);
    }
    if let Some(event) = latest_canonical_work_event_for_item_optional(connection, item.work_id)? {
        if super::super::task_sync::reported::validate_on(connection, &event)?.is_some() {
            if event.work != *item {
                return Err(StoreError::InvalidWorkProjection(
                    "reported completion differs from its canonical item".into(),
                ));
            }
            return Ok(true);
        }
        let run = event.run.as_ref().ok_or_else(|| {
            StoreError::InvalidWorkProjection("native completed work has no canonical run".into())
        })?;
        let seal_id = run.completion_seal.as_ref().ok_or_else(|| {
            StoreError::InvalidWorkProjection("native completed work has no canonical seal".into())
        })?;
        let projected = connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM work_completion_seals
                 WHERE work_id = ?1 AND run_id = ?2 AND seal_id = ?3
             )",
            params![
                item.work_id.0.to_string(),
                run.run_id.0.to_string(),
                seal_id.as_str()
            ],
            |row| row.get::<_, bool>(0),
        )?;
        let seal: CompletionSeal = load_typed_work_object(connection, seal_id, "completion_seal")?;
        if event.work != *item
            || run.work_id != item.work_id
            || run.state != WorkRunState::Completed
            || !projected
            || seal.work_id != item.work_id
            || seal.run_id != run.run_id
            || seal.root_id != item.root_id
            || seal.root_execution_id != run.root_execution_id
        {
            return Err(StoreError::InvalidWorkProjection(
                "native completed work differs from its canonical seal or seal projection".into(),
            ));
        }
        return Ok(false);
    }
    let has_native_seal = connection.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM work_completion_seals WHERE work_id = ?1
         )",
        [item.work_id.0.to_string()],
        |row| row.get::<_, bool>(0),
    )?;
    let (_, record) = latest_restored_record(connection, item.work_id)?.ok_or_else(|| {
        StoreError::InvalidWorkProjection("completed-by-record work has no restored record".into())
    })?;
    if has_native_seal
        || record.history.completion.is_none()
        || !crate::graph_snapshot::restored_item_basis_matches(
            &record.item,
            &record.project_id,
            item,
        )
    {
        return Err(StoreError::InvalidWorkProjection(
            "completed-by-record work differs from its canonical restored completion".into(),
        ));
    }
    Ok(true)
}

#[cfg(test)]
pub(in crate::storage) fn latest_restored_record_hash(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Option<ObjectId>, StoreError> {
    Ok(latest_restored_record(connection, work_id)?.map(|(hash, _)| hash))
}

pub(in crate::storage::work) fn latest_restored_record(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Option<(ObjectId, RestoredRecord)>, StoreError> {
    let row = connection
        .query_row(
            "SELECT generation_index, record_id
             FROM work_restored_records WHERE work_id = ?1
             ORDER BY generation_index DESC LIMIT 1",
            [work_id.0.to_string()],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    row.map(|(generation_index, stored_hash)| {
        let hash = ObjectId::from_stored(stored_hash.clone())
            .ok_or(StoreError::InvalidStoredKey(stored_hash))?;
        let record: RestoredRecord =
            load_typed_work_object(connection, &hash, "work_restored_record")?;
        if record.work_id != work_id
            || i64::try_from(record.generation_index).ok() != Some(generation_index)
        {
            return Err(StoreError::InvalidWorkProjection(format!(
                "restored record for {work_id:?} differs from its projection binding"
            )));
        }
        Ok((hash, record))
    })
    .transpose()
}

pub(in crate::storage::work) fn restored_records_for_item(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<RestoredRecord>, StoreError> {
    Ok(restored_records_with_id_for_item(connection, work_id)?
        .into_iter()
        .map(|(_, record)| record)
        .collect())
}

pub(in crate::storage::work) fn restored_records_with_id_for_item(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<(ObjectId, RestoredRecord)>, StoreError> {
    let rows = connection
        .prepare(
            "SELECT generation_index, record_id FROM work_restored_records
             WHERE work_id = ?1 ORDER BY generation_index",
        )?
        .query_map([work_id.0.to_string()], |row| {
            super::super::search_budget::record_row(connection, row)?;
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter()
        .enumerate()
        .map(|(expected, (generation, stored_record_id))| {
            if i64::try_from(expected).ok() != Some(generation) {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "restored history for {work_id:?} is not dense"
                )));
            }
            let record_id = ObjectId::from_stored(stored_record_id.clone())
                .ok_or(StoreError::InvalidStoredKey(stored_record_id))?;
            let record: RestoredRecord =
                load_typed_work_object(connection, &record_id, "work_restored_record")?;
            if record.work_id != work_id || record.generation_index != expected {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "restored history for {work_id:?} differs from its projection binding"
                )));
            }
            Ok((record_id, record))
        })
        .collect()
}

pub(super) fn restored_record_binds_work(
    connection: &Connection,
    record_id: &ObjectId,
    work_id: WorkId,
) -> Result<bool, StoreError> {
    let projected: bool = connection.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM work_restored_records
             WHERE work_id = ?1 AND record_id = ?2
         )",
        params![work_id.0.to_string(), record_id.as_str()],
        |row| row.get(0),
    )?;
    if !projected {
        return Ok(false);
    }
    let record: RestoredRecord =
        load_typed_work_object(connection, record_id, "work_restored_record")?;
    Ok(record.work_id == work_id)
}

pub(super) fn restored_work_evidence_for_item(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<(ObjectId, RestoredWorkEvidence)>, StoreError> {
    let rows = connection
        .prepare(
            "SELECT evidence_id, record_id, sequence, gate_name, created_at_ms
             FROM work_restored_evidence INDEXED BY work_restored_evidence_work
             WHERE work_id = ?1
             ORDER BY sequence, evidence_id",
        )?
        .query_map([work_id.0.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter()
        .enumerate()
        .map(
            |(index, (stored_hash, stored_record, sequence, gate_name, created_at_ms))| {
                let expected_sequence = i64::try_from(index)
                    .ok()
                    .and_then(|value| value.checked_add(1));
                let hash = ObjectId::from_stored(stored_hash.clone())
                    .ok_or(StoreError::InvalidStoredKey(stored_hash))?;
                let evidence: RestoredWorkEvidence =
                    load_typed_work_object(connection, &hash, "work_restored_evidence")?;
                let scalar_bound = evidence.work_id == work_id
                    && evidence.restored_record.as_str() == stored_record
                    && evidence.sequence == sequence
                    && Some(sequence) == expected_sequence
                    && evidence.gate.as_ref().map(|gate| gate.name.as_str())
                        == gate_name.as_deref()
                    && evidence.created_at.timestamp_millis() == created_at_ms
                    && restored_record_binds_work(connection, &evidence.restored_record, work_id)?;
                if !scalar_bound {
                    return Err(StoreError::InvalidWorkProjection(format!(
                        "restored evidence {hash} differs from its projection binding"
                    )));
                }
                Ok((hash, evidence))
            },
        )
        .collect()
}
