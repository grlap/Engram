//! Rebuildable links from a recovery fact to its quarantined historical bindings.

use super::{ObjectId, RecoveryRecord, StoreError, read, require};
use rusqlite::{Connection, params};
use std::collections::BTreeMap;

pub(in crate::storage::work) fn create_schema(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch(
        "CREATE INDEX IF NOT EXISTS objects_task_sync_origin_recovery_operation
             ON objects(json_extract(canonical_json,'$.request.operation'))
             WHERE object_kind='task_sync_origin_recovery' AND json_valid(canonical_json);
         CREATE INDEX IF NOT EXISTS objects_task_sync_origin_recovery_predecessor
             ON objects(json_extract(canonical_json,'$.request.predecessor'))
             WHERE object_kind='task_sync_origin_recovery' AND json_valid(canonical_json);
         CREATE TABLE IF NOT EXISTS work_task_sync_origin_recovery_bindings (
             binding_id TEXT PRIMARY KEY REFERENCES objects(object_id),
             recovery_id TEXT NOT NULL REFERENCES objects(object_id)
         ) STRICT;",
    )?;
    Ok(())
}

pub(in crate::storage::work) fn record(
    connection: &Connection,
    id: &ObjectId,
) -> Result<(), StoreError> {
    let record = read(connection, id)?;
    for binding in &record.bindings {
        connection.execute(
            "INSERT INTO work_task_sync_origin_recovery_bindings(binding_id,recovery_id) VALUES (?1,?2)",
            params![binding.binding.as_str(), id.as_str()])?;
    }
    Ok(())
}

fn canonical(connection: &Connection) -> Result<Vec<(ObjectId, RecoveryRecord)>, StoreError> {
    let mut query = connection.prepare(
        "SELECT object_id FROM objects WHERE object_kind='task_sync_origin_recovery'
         ORDER BY object_id LIMIT 4097",
    )?;
    let ids = query
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    require(
        ids.len() <= 4096,
        "origin recovery history exceeds supported repair bound",
    )?;
    let mut operations = std::collections::BTreeSet::new();
    let mut predecessors = std::collections::BTreeSet::new();
    ids.into_iter()
        .map(|id| {
            let id = ObjectId::from_stored(id)
                .ok_or_else(|| super::invalid("invalid recovery identity"))?;
            let record = read(connection, &id)?;
            require(
                operations.insert(record.request.operation)
                    && predecessors.insert(record.request.predecessor.clone()),
                "origin recovery operation or lineage forks",
            )?;
            Ok((id, record))
        })
        .collect()
}

fn derived(records: &[(ObjectId, RecoveryRecord)]) -> Result<BTreeMap<String, String>, StoreError> {
    let mut expected = BTreeMap::new();
    for (id, record) in records {
        for binding in &record.bindings {
            require(
                expected
                    .insert(binding.binding.to_string(), id.to_string())
                    .is_none(),
                "origin recovery binding is quarantined twice",
            )?;
        }
    }
    Ok(expected)
}

pub(in crate::storage::work) fn rebuild(connection: &Connection) -> Result<(), StoreError> {
    // Validate every canonical fact before replacing any derived row. The repair
    // owner supplies the outer transaction and rolls back all owners on failure.
    let expected = derived(&canonical(connection)?)?;
    connection.execute("DELETE FROM work_task_sync_origin_recovery_bindings", [])?;
    for (binding, recovery) in expected {
        connection.execute(
            "INSERT INTO work_task_sync_origin_recovery_bindings VALUES (?1,?2)",
            params![binding, recovery],
        )?;
    }
    Ok(())
}

pub(in crate::storage::work) fn verify(connection: &Connection) -> Result<usize, StoreError> {
    let records = canonical(connection)?;
    let expected = derived(&records)?;
    let mut query = connection.prepare(
        "SELECT binding_id,recovery_id FROM work_task_sync_origin_recovery_bindings ORDER BY binding_id")?;
    let actual = query
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<BTreeMap<_, _>, _>>()?;
    require(
        expected == actual,
        "origin recovery binding projection differs; repair projections explicitly",
    )?;
    Ok(records.len())
}
