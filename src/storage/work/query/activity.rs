//! Canonical event identities for informational task-sync activity projection.

use super::{Connection, ObjectId, StoreError, WorkEvent, WorkId, decode_canonical_work_event};

/// Original canonical identities, in the same project-feed order as event reads.
pub(in crate::storage) fn canonical_work_event_records_for_item(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<(ObjectId, WorkEvent)>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT object.object_id, object.canonical_json
         FROM objects object JOIN work_feed_entries entry ON entry.object_id = object.object_id
         WHERE object.object_kind = 'work_event' AND entry.feed_kind = 'project'
           AND entry.object_kind = 'work_event' AND entry.work_id = ?1
         ORDER BY entry.position",
    )?;
    statement
        .query_map([work_id.0.to_string()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?
        .map(|row| {
            let stored = row?;
            let id = ObjectId::from_stored(stored.0.clone())
                .ok_or_else(|| StoreError::InvalidStoredKey(stored.0.clone()))?;
            Ok((id, decode_canonical_work_event(stored)?))
        })
        .collect()
}
