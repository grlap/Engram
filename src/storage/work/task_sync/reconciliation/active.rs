//! Rebuildable active membership. Canonical facts remain the only hold history.

use super::{Connection, ObjectId, Records, StoreError, WorkId, invalid, params, require};

pub(in crate::storage::work) fn create_schema(connection: &Connection) -> Result<(), StoreError> {
    connection.execute_batch(
        "CREATE TABLE IF NOT EXISTS work_task_sync_reconciliation_active (
             work_id TEXT PRIMARY KEY REFERENCES work_items(work_id),
             hold_id TEXT NOT NULL REFERENCES objects(object_id)
         ) STRICT;
         CREATE INDEX IF NOT EXISTS work_task_sync_reconciliation_active_hold
             ON work_task_sync_reconciliation_active(hold_id);
         CREATE INDEX IF NOT EXISTS work_task_sync_reconciliation_holds_by_hold
             ON work_task_sync_reconciliation_holds(hold_id);
         CREATE TRIGGER IF NOT EXISTS work_reconciliation_active_no_update
         BEFORE UPDATE ON work_task_sync_reconciliation_active
         BEGIN SELECT RAISE(ABORT,'active reconciliation membership is immutable'); END;
         CREATE TRIGGER IF NOT EXISTS work_reconciliation_active_guard_delete
         BEFORE DELETE ON work_task_sync_reconciliation_active
         WHEN NOT EXISTS (
             SELECT 1 FROM objects event JOIN objects held ON held.object_id=OLD.hold_id
             WHERE (event.object_kind='work_event'
                    AND json_extract(event.canonical_json,'$.transition.kind')='reconciliation_hold_abandoned'
                    AND json_extract(event.canonical_json,'$.transition.hold')=OLD.hold_id
                    AND json_extract(event.canonical_json,'$.actor')=json_extract(held.canonical_json,'$.actor'))
                OR (event.object_kind='task_sync_exchange_event'
                    AND json_extract(event.canonical_json,'$.transition.transition')='reconciled_import_applied'
                    AND json_extract(event.canonical_json,'$.binding')=json_extract(held.canonical_json,'$.binding')
                    AND json_extract(event.canonical_json,'$.transition.prepared')=json_extract(held.canonical_json,'$.prepared')
                    AND json_extract(event.canonical_json,'$.transition.receipt.candidate_fingerprint')=json_extract(held.canonical_json,'$.candidate_fingerprint')
                    AND OLD.hold_id IN (SELECT value FROM json_each(event.canonical_json,'$.transition.receipt.holds')))
         )
         BEGIN SELECT RAISE(ABORT,'active reconciliation membership has no canonical resolution'); END;",
    )?;
    Ok(())
}

pub(super) fn lookup(
    connection: &Connection,
    work: WorkId,
) -> Result<Option<ObjectId>, StoreError> {
    use rusqlite::OptionalExtension;
    let id: Option<String> = connection
        .query_row(
            "SELECT hold_id FROM work_task_sync_reconciliation_active WHERE work_id=?1",
            [work.0.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    id.map(|id| ObjectId::from_stored(id).ok_or_else(|| invalid("invalid active hold identity")))
        .transpose()
}

#[cfg_attr(
    not(test),
    expect(
        dead_code,
        reason = "The atomic reconciliation writer resolves active membership in the next slice"
    )
)]
pub(in crate::storage) fn resolve_on(
    connection: &Connection,
    id: &ObjectId,
) -> Result<(), StoreError> {
    let hold = super::SqliteStore::get_typed_object_on::<super::ReconciliationHold>(
        connection,
        id,
        super::HOLD_KIND,
    )?
    .ok_or_else(|| invalid("resolved hold is missing"))?;
    super::holds::validate_hold(connection, id, &hold)?;
    require(
        super::holds::resolution_on(connection, id, &hold)?.is_some(),
        "active hold has no canonical resolution",
    )?;
    connection.execute(
        "DELETE FROM work_task_sync_reconciliation_active WHERE hold_id=?1",
        [id.as_str()],
    )?;
    Ok(())
}

fn expected(
    connection: &Connection,
    records: &Records,
) -> Result<std::collections::BTreeMap<String, String>, StoreError> {
    let mut result = std::collections::BTreeMap::new();
    for (id, hold) in &records.holds {
        super::holds::validate_hold(connection, id, hold)?;
        if super::holds::resolution_on(connection, id, hold)?.is_none() {
            for work in &hold.affected {
                require(
                    result
                        .insert(work.work_id.0.to_string(), id.as_str().into())
                        .is_none(),
                    "canonical reconciliation holds overlap",
                )?;
            }
        }
    }
    Ok(result)
}

pub(in crate::storage::work) fn rebuild(connection: &Connection) -> Result<(), StoreError> {
    let records = Records::load(connection)?;
    for (work, hold) in expected(connection, &records)? {
        connection.execute(
            "INSERT INTO work_task_sync_reconciliation_active(work_id,hold_id) VALUES (?1,?2)",
            params![work, hold],
        )?;
    }
    Ok(())
}

pub(super) fn verify(connection: &Connection, records: &Records) -> Result<(), StoreError> {
    let mut statement = connection.prepare(
        "SELECT work_id,hold_id FROM work_task_sync_reconciliation_active ORDER BY work_id",
    )?;
    let actual = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<std::collections::BTreeMap<_, _>, _>>()?;
    require(
        actual == expected(connection, records)?,
        "active reconciliation projection differs from canonical membership",
    )
}
