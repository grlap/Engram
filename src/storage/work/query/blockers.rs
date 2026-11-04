//! Blocker projection readers and canonical binding validation.

use super::{
    Connection, ObjectId, OptionalExtension, StoreError, WorkBlocker, WorkId, WorkTransition,
    native_work_event_optional, params, restored_record_binds_work,
};

pub(in crate::storage) fn load_active_blocker_projections(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<WorkBlocker>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT blocker_json, created_event_id,
                blocker_id = json_extract(blocker_json, '$.blocker_id') AND
                work_id = json_extract(blocker_json, '$.work_id')
         FROM work_blockers
         WHERE work_id = ?1 AND state = 'active'
         ORDER BY blocker_id",
    )?;
    statement
        .query_map([work_id.0.to_string()], |row| {
            super::super::search_budget::record_row(connection, row)?;
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, bool>(2)?,
            ))
        })?
        .map(|row| {
            let (bytes, event_id, scalar_bound) = row?;
            verified_blocker_row(
                connection,
                work_id,
                &bytes,
                event_id,
                scalar_bound,
                "active",
            )
        })
        .collect()
}

/// The cleared blocker `blocker_id` of `work_id` as it was recorded when it
/// was raised: its retained row, checked against the event that created it
/// exactly as an active one is. `None` when the item holds no cleared blocker
/// of that id.
pub(in crate::storage) fn load_cleared_blocker_projection(
    connection: &Connection,
    work_id: WorkId,
    blocker_id: &str,
) -> Result<Option<WorkBlocker>, StoreError> {
    connection
        .query_row(
            "SELECT blocker_json, created_event_id,
                    blocker_id = json_extract(blocker_json, '$.blocker_id') AND
                    work_id = json_extract(blocker_json, '$.work_id')
             FROM work_blockers
             WHERE blocker_id = ?1 AND work_id = ?2 AND state = 'cleared'",
            params![blocker_id, work_id.0.to_string()],
            |row| {
                Ok((
                    row.get::<_, Vec<u8>>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, bool>(2)?,
                ))
            },
        )
        .optional()?
        .map(|(bytes, event_id, scalar_bound)| {
            verified_blocker_row(
                connection,
                work_id,
                &bytes,
                event_id,
                scalar_bound,
                "cleared",
            )
        })
        .transpose()
}

/// The blocker whose clear moved `work_id` to `revision`: the cleared row
/// whose clear event is the item's native unblock event at that revision,
/// then read and checked as [`load_cleared_blocker_projection`] reads it.
/// `None` when the item's change to that revision cleared no blocker.
pub(in crate::storage) fn load_blocker_cleared_at(
    connection: &Connection,
    work_id: WorkId,
    revision: i64,
) -> Result<Option<WorkBlocker>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT blocker_id, cleared_event_id FROM work_blockers
         WHERE work_id = ?1 AND state = 'cleared' AND cleared_event_id IS NOT NULL
         ORDER BY blocker_id",
    )?;
    let cleared = statement
        .query_map([work_id.0.to_string()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (blocker_id, event_id) in cleared {
        let event_id = ObjectId::from_stored(event_id.clone())
            .ok_or(StoreError::InvalidStoredKey(event_id))?;
        let clears_at_revision =
            native_work_event_optional(connection, &event_id)?.is_some_and(|event| {
                event.work_id == work_id
                    && event.revision == revision
                    && matches!(
                        event.transition,
                        WorkTransition::Unblocked { blocker_id: ref cleared }
                            if cleared == &blocker_id
                    )
            });
        if clears_at_revision {
            return load_cleared_blocker_projection(connection, work_id, &blocker_id);
        }
    }
    Ok(None)
}

/// One stored blocker row, accepted only when its scalar columns agree with
/// its JSON and it is bound to the native event that raised it or to a
/// restored record of the same item.
fn verified_blocker_row(
    connection: &Connection,
    work_id: WorkId,
    bytes: &[u8],
    event_id: String,
    scalar_bound: bool,
    state: &str,
) -> Result<WorkBlocker, StoreError> {
    let blocker: WorkBlocker = serde_json::from_slice(bytes)?;
    let event_id =
        ObjectId::from_stored(event_id.clone()).ok_or(StoreError::InvalidStoredKey(event_id))?;
    let native_binding = native_work_event_optional(connection, &event_id)?.is_some_and(|event| {
        event.work_id == work_id
            && event.blocker.as_ref() == Some(&blocker)
            && matches!(
                event.transition,
                WorkTransition::Blocked { ref blocker_id }
                    if blocker_id == &blocker.blocker_id
            )
    });
    let restored_binding = restored_record_binds_work(connection, &event_id, work_id)?;
    if !scalar_bound || blocker.work_id != work_id || !(native_binding || restored_binding) {
        return Err(StoreError::InvalidWorkProjection(format!(
            "{state} blocker {} differs from its scalar or event binding",
            blocker.blocker_id
        )));
    }
    Ok(blocker)
}
