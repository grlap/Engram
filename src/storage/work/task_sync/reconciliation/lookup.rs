use super::{
    CanonicalObject, Connection, MAX_BYTES, MAX_RECORD_BYTES, MAX_RECORDS, ObjectId, StoreError,
    invalid, require,
};

// Match the expression and predicate of the shared partial lookup index. Holds
// are project-scoped; exchange events are binding-scoped. The decoded records
// still undergo their owning validators before they can affect admission.
pub(super) const SCOPED_RECORDS_SQL: &str = "SELECT object_id, canonical_json FROM objects
    WHERE object_kind IN ('task_sync_reconciliation_hold', 'task_sync_exchange_event')
      AND object_kind=?1
      AND json_valid(canonical_json)
      AND CASE WHEN object_kind='task_sync_reconciliation_hold'
          THEN json_extract(canonical_json, '$.project_id')
          ELSE json_extract(canonical_json, '$.binding') END=?2
    LIMIT ?3";

pub(super) fn scoped_on<T: serde::de::DeserializeOwned>(
    connection: &Connection,
    kind: &str,
    scope: &str,
) -> Result<Vec<(ObjectId, T)>, StoreError> {
    let mut statement = connection.prepare(SCOPED_RECORDS_SQL)?;
    decode_rows(statement.query(rusqlite::params![kind, scope, record_limit()?])?)
}

pub(super) fn record_limit() -> Result<i64, StoreError> {
    i64::try_from(MAX_RECORDS + 1).map_err(|_| invalid("reconciliation SQL limit exceeds i64"))
}

pub(super) fn decode_rows<T: serde::de::DeserializeOwned>(
    mut rows: rusqlite::Rows<'_>,
) -> Result<Vec<(ObjectId, T)>, StoreError> {
    let mut result = Vec::new();
    let mut total = 0usize;
    while let Some(row) = rows.next()? {
        let id = ObjectId::from_stored(row.get(0)?)
            .ok_or_else(|| invalid("invalid scoped reconciliation record id"))?;
        let bytes: Vec<u8> = row.get(1)?;
        total = total
            .checked_add(bytes.len())
            .ok_or_else(|| invalid("scoped reconciliation byte bound exceeded"))?;
        require(
            result.len() < MAX_RECORDS && total <= MAX_BYTES && bytes.len() <= MAX_RECORD_BYTES,
            "scoped reconciliation records exceed supported bounds; preserve them",
        )?;
        let value = CanonicalObject::stored(&id, bytes)?.decode()?;
        result.push((id, value));
    }
    Ok(result)
}
