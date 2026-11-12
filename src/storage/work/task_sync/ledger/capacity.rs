//! One aggregate capacity contract for loaded ledgers and incremental writers.

use rusqlite::{Connection, params};

use super::{
    BINDING_KIND, CanonicalObject, EVENT_KIND, MAX_BYTES, MAX_RECORD_BYTES, MAX_RECORDS, StoreError,
};

pub(super) fn require(
    records: usize,
    bytes: usize,
    objects: &[&CanonicalObject],
) -> Result<(), StoreError> {
    super::require(
        records
            .checked_add(objects.len())
            .is_some_and(|total| total <= MAX_RECORDS)
            && objects
                .iter()
                .all(|object| object.bytes().len() <= MAX_RECORD_BYTES)
            && objects
                .iter()
                .try_fold(bytes, |total, object| {
                    total.checked_add(object.bytes().len())
                })
                .is_some_and(|total| total <= MAX_BYTES),
        "exchange discovery capacity exceeded; preserve retained input",
    )
}

/// Read only canonical lengths, never transfer or decode unrelated payloads.
/// The caller holds the existing outer writer transaction, so a later append
/// includes earlier inserts and any refusal rolls back the complete import.
pub(super) fn require_on(
    connection: &Connection,
    objects: &[&CanonicalObject],
) -> Result<(), StoreError> {
    let (records, bytes, largest, blobs): (i64, i64, i64, bool) = connection.query_row(
        "SELECT count(*),coalesce(sum(length(canonical_json)),0),
                coalesce(max(length(canonical_json)),0),
                coalesce(min(typeof(canonical_json)='blob'),1)
         FROM objects WHERE object_kind IN (?1,?2)",
        params![BINDING_KIND, EVENT_KIND],
        |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
    )?;
    let invalid = || {
        super::super::invalid("exchange discovery exceeds supported bound; preserve all records")
    };
    super::require(
        blobs && usize::try_from(largest).is_ok_and(|length| length <= MAX_RECORD_BYTES),
        "exchange discovery exceeds supported bound; preserve all records",
    )?;
    require(
        usize::try_from(records).map_err(|_| invalid())?,
        usize::try_from(bytes).map_err(|_| invalid())?,
        objects,
    )
}
