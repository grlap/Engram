//! Canonical work events and item projections, with their decoding instrumentation.

use super::{
    CanonicalObject, Connection, ObjectId, OptionalExtension, StoreError, WorkEvent, WorkId,
    WorkItem, latest_canonical_work_event_for_item_optional, latest_restored_record,
    load_work_item, params, query_row_with_cost,
};

#[cfg(test)]
use super::super::{WORK_EVENT_DECODE_COUNT, WORK_ITEM_PROJECTION_DECODE_COUNT};

pub(super) fn latest_canonical_work_event_on_snapshot(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Option<WorkEvent>, StoreError> {
    let projected_hash = connection
        .query_row(
            "SELECT latest_event_id FROM work_items WHERE work_id = ?1",
            [work_id.0.to_string()],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?;
    let latest = connection
        .query_row(
            "SELECT object.object_id, object.canonical_json
             FROM objects object
             JOIN work_feed_entries entry ON entry.object_id = object.object_id
             WHERE object.object_kind = 'work_event'
               AND entry.feed_kind = 'project'
               AND entry.object_kind = 'work_event'
               AND entry.work_id = ?1
             ORDER BY entry.position DESC LIMIT 1",
            [work_id.0.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    super::super::search_budget::blob(connection, row, 1)?,
                ))
            },
        )
        .optional()?;
    match (projected_hash.flatten(), latest) {
        (None, None) => Ok(None),
        (Some(projected_hash), Some((stored_hash, bytes))) => {
            let projected_hash = ObjectId::from_stored(projected_hash.clone())
                .ok_or(StoreError::InvalidStoredKey(projected_hash))?;
            let stored_hash_value = ObjectId::from_stored(stored_hash.clone())
                .ok_or(StoreError::InvalidStoredKey(stored_hash.clone()))?;
            if projected_hash != stored_hash_value {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "work item {work_id:?} latest-event binding differs from its indexed feed head"
                )));
            }
            let event = decode_canonical_work_event((stored_hash, bytes))?;
            if event.work_id != work_id {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "work item {work_id:?} latest-event binding names another item"
                )));
            }
            Ok(Some(event))
        }
        _ => Err(StoreError::InvalidWorkProjection(format!(
            "work item {work_id:?} latest-event binding is incomplete"
        ))),
    }
}

pub(in crate::storage) fn canonical_work_events_for_item(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<WorkEvent>, StoreError> {
    item_events(connection, work_id, "")
}

/// The item's canonical events that can bear on its evaluation-mode mark, in
/// order: its first event, the baseline that tells a mark carried in by
/// restored history from one set here, and every creation or revision after
/// it. Only a creation or a revision sets or clears the mark, so every event
/// left out repeats the mark of the one before it, and the bodies fetched and
/// decoded do not grow with claims, notes or gates. The filter itself may
/// still visit the item's whole event index.
pub(in crate::storage) fn canonical_work_mark_events_for_item(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<WorkEvent>, StoreError> {
    item_events(
        connection,
        work_id,
        "AND (json_extract(object.canonical_json, '$.transition.kind') IN ('created', 'revised')
           OR entry.position = (SELECT MIN(first.position) FROM work_feed_entries first
             WHERE first.feed_kind = 'project' AND first.object_kind = 'work_event'
               AND first.work_id = ?1))",
    )
}

/// The item's canonical events that also meet `filter`, in feed order.
fn item_events(
    connection: &Connection,
    work_id: WorkId,
    filter: &str,
) -> Result<Vec<WorkEvent>, StoreError> {
    let mut statement = connection.prepare(&format!(
        "SELECT object.object_id, object.canonical_json
         FROM objects object
         JOIN work_feed_entries entry ON entry.object_id = object.object_id
         WHERE object.object_kind = 'work_event'
           AND entry.feed_kind = 'project'
           AND entry.object_kind = 'work_event'
           AND entry.work_id = ?1 {filter}
         ORDER BY entry.position"
    ))?;
    statement
        .query_map([work_id.0.to_string()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?
        .map(|row| decode_canonical_work_event(row?))
        .collect()
}

/// The item's canonical events that carry one claim epoch at one work
/// revision, newest first. The item's event index yields them in that order
/// without a sort, so a reader that stops at its first match reads no older
/// rows.
pub(super) const CLAIM_EPOCH_EVENTS_SQL: &str = "SELECT object.object_id, object.canonical_json
     FROM objects object
     JOIN work_feed_entries entry ON entry.object_id = object.object_id
     WHERE object.object_kind = 'work_event'
       AND entry.feed_kind = 'project'
       AND entry.object_kind = 'work_event'
       AND entry.work_id = ?1
       AND json_extract(object.canonical_json, '$.work.revision') = ?2
       AND json_extract(object.canonical_json, '$.claim.claim_id') = ?3
       AND json_extract(object.canonical_json, '$.claim.fence') = ?4
     ORDER BY entry.position DESC";

/// Whether any canonical event of `work_id` that carries claim `claim_id`
/// at `claim_fence` and work revision `work_revision` satisfies `recorded`.
/// Only such events can match. The read goes newest first and stops at the
/// first match, which is normally the item's newest event while the claim is
/// live, so the rows read and the events decoded do not grow with the item's
/// history. When nothing matches, as for a binding bind refuses, it reads
/// the item's whole event index but decodes only that epoch's events.
pub(in crate::storage) fn any_canonical_claim_epoch_event(
    connection: &Connection,
    work_id: WorkId,
    work_revision: i64,
    claim_id: crate::domain::WorkClaimId,
    claim_fence: i64,
    mut recorded: impl FnMut(&WorkEvent) -> bool,
) -> Result<bool, StoreError> {
    let mut statement = connection.prepare(CLAIM_EPOCH_EVENTS_SQL)?;
    let mut rows = statement.query(params![
        work_id.0.to_string(),
        work_revision,
        claim_id.0.to_string(),
        claim_fence
    ])?;
    while let Some(row) = rows.next()? {
        let event = decode_canonical_work_event((row.get(0)?, row.get(1)?))?;
        if recorded(&event) {
            return Ok(true);
        }
    }
    Ok(false)
}

pub(super) fn latest_canonical_work_event_on_feed(
    connection: &Connection,
    feed_kind: &str,
    feed_id: &str,
    required_snapshot: &str,
) -> Result<WorkEvent, StoreError> {
    let stored = query_row_with_cost(
        connection,
        "canonical_feed_event",
        "SELECT object.object_id, object.canonical_json
             FROM work_feed_entries entry
             JOIN objects object ON object.object_id = entry.object_id
             WHERE entry.feed_kind = ?1 AND entry.feed_id = ?2
               AND entry.object_kind = 'work_event'
               AND object.object_kind = 'work_event'
               AND json_type(object.canonical_json, ?3) IS NOT NULL
               AND json_type(object.canonical_json, ?3) != 'null'
             ORDER BY entry.position DESC LIMIT 1",
        params![feed_kind, feed_id, required_snapshot],
        |row| {
            Ok((
                row.get::<_, String>(0)?,
                super::super::search_budget::blob(connection, row, 1)?,
            ))
        },
    )
    .optional()?
    .ok_or_else(|| {
        StoreError::InvalidWorkProjection(format!(
            "work feed {feed_kind}:{feed_id} has no canonical event"
        ))
    })?;
    decode_canonical_work_event(stored)
}

pub(super) fn decode_canonical_work_event(
    stored: (String, Vec<u8>),
) -> Result<WorkEvent, StoreError> {
    #[cfg(test)]
    WORK_EVENT_DECODE_COUNT.with(|count| count.set(count.get().saturating_add(1)));
    let (stored_hash, bytes) = stored;
    let hash = ObjectId::from_stored(stored_hash.clone())
        .ok_or(StoreError::InvalidStoredKey(stored_hash))?;
    CanonicalObject::stored(&hash, bytes)?.decode()
}

pub(in crate::storage::work) fn load_work_item_projection(
    connection: &Connection,
    work_id: WorkId,
) -> Result<WorkItem, StoreError> {
    let row: Option<(Vec<u8>, bool)> = query_row_with_cost(connection, "work_item_projection",
            "SELECT item_json,
                    work_id = json_extract(item_json, '$.work_id') AND
                    project_id = json_extract(item_json, '$.project_id') AND
                    short_ref = json_extract(item_json, '$.short_ref') AND
                    root_id = json_extract(item_json, '$.root_id') AND
                    COALESCE(parent_id, '') = COALESCE(json_extract(item_json, '$.parent_id'), '') AND
                    child_requirement = json_extract(item_json, '$.child_requirement') AND
                    lifecycle = json_extract(item_json, '$.lifecycle') AND
                    priority = json_extract(item_json, '$.priority') AND
                    COALESCE(assigned_to, '') = COALESCE(json_extract(item_json, '$.assigned_to'), '') AND
                    COALESCE(deferred_until_ms, -1) = COALESCE(
                        CAST(strftime('%s', json_extract(item_json, '$.deferred_until')) AS INTEGER) * 1000
                        + CASE WHEN instr(json_extract(item_json, '$.deferred_until'), '.') > 0
                            THEN CAST(substr(
                                substr(
                                    json_extract(item_json, '$.deferred_until'),
                                    instr(json_extract(item_json, '$.deferred_until'), '.') + 1,
                                    instr(json_extract(item_json, '$.deferred_until'), 'Z')
                                        - instr(json_extract(item_json, '$.deferred_until'), '.') - 1
                                ) || '000', 1, 3
                            ) AS INTEGER)
                            ELSE 0 END, -1) AND
                    revision = json_extract(item_json, '$.revision') AND
                    COALESCE(active_run_id, '') = COALESCE(json_extract(item_json, '$.active_run_id'), '') AND
                    COALESCE(superseded_by, '') = COALESCE(json_extract(item_json, '$.superseded_by'), '') AND
                    COALESCE(source_snapshot_id, '') = COALESCE(json_extract(item_json, '$.source_snapshot_id'), '') AND
                    created_at_ms = CAST(strftime('%s', json_extract(item_json, '$.created_at')) AS INTEGER) * 1000
                        + CASE WHEN instr(json_extract(item_json, '$.created_at'), '.') > 0
                            THEN CAST(substr(
                                substr(
                                    json_extract(item_json, '$.created_at'),
                                    instr(json_extract(item_json, '$.created_at'), '.') + 1,
                                    instr(json_extract(item_json, '$.created_at'), 'Z')
                                        - instr(json_extract(item_json, '$.created_at'), '.') - 1
                                ) || '000', 1, 3
                            ) AS INTEGER)
                            ELSE 0 END AND
                    updated_at_ms = CAST(strftime('%s', json_extract(item_json, '$.updated_at')) AS INTEGER) * 1000
                        + CASE WHEN instr(json_extract(item_json, '$.updated_at'), '.') > 0
                            THEN CAST(substr(
                                substr(
                                    json_extract(item_json, '$.updated_at'),
                                    instr(json_extract(item_json, '$.updated_at'), '.') + 1,
                                    instr(json_extract(item_json, '$.updated_at'), 'Z')
                                        - instr(json_extract(item_json, '$.updated_at'), '.') - 1
                                ) || '000', 1, 3
                            ) AS INTEGER)
                            ELSE 0 END
             FROM work_items WHERE work_id = ?1",
            [work_id.0.to_string()],
            |row| Ok((super::super::search_budget::blob(connection, row, 0)?, row.get(1)?)),
        )
        .optional()?;
    let (bytes, scalar_bound) = row.ok_or(StoreError::WorkNotFound(work_id))?;
    #[cfg(test)]
    WORK_ITEM_PROJECTION_DECODE_COUNT.with(|count| count.set(count.get().saturating_add(1)));
    let item: WorkItem = serde_json::from_slice(&bytes)?;
    if !scalar_bound
        || item.work_id != work_id
        || item.schema_version != crate::schema::SCHEMA_VERSION
    {
        return Err(StoreError::InvalidWorkProjection(format!(
            "work item {work_id:?} differs from its scalar projection binding"
        )));
    }
    Ok(item)
}

pub(super) fn load_work_item_on_snapshot(
    connection: &Connection,
    work_id: WorkId,
) -> Result<WorkItem, StoreError> {
    let item = load_work_item_projection(connection, work_id)?;
    if let Some(event) = latest_canonical_work_event_for_item_optional(connection, work_id)? {
        super::super::task_sync::reported::validate_on(connection, &event)?;
        if event.work != item {
            return Err(StoreError::InvalidWorkProjection(format!(
                "work item {work_id:?} differs from its latest canonical event"
            )));
        }
    } else {
        let Some((record_id, record)) = latest_restored_record(connection, work_id)? else {
            return Err(StoreError::InvalidWorkProjection(format!(
                "work item {work_id:?} has neither canonical native nor restored history"
            )));
        };
        if !crate::graph_snapshot::restored_item_basis_matches(
            &record.item,
            &record.project_id,
            &item,
        ) {
            return Err(StoreError::InvalidWorkProjection(format!(
                "work item {work_id:?} differs from its canonical restored record"
            )));
        }
        if !super::super::task_sync::validate_item_origin(connection, &record_id, &record, &item)? {
            let load = super::super::super::graph_snapshot::work_graph_snapshot_load_origin_on(
                connection,
                &item.project_id,
            )?;
            if item.created_by != load.actor
                || item.created_at != load.loaded_at
                || item.updated_at != load.loaded_at
            {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "work item {work_id:?} differs from its canonical load attribution"
                )));
            }
        }
        super::super::planning::validated_current_work_relation_basis(connection, work_id)?;
    }
    Ok(item)
}

pub(super) fn native_work_event_optional(
    connection: &Connection,
    hash: &ObjectId,
) -> Result<Option<WorkEvent>, StoreError> {
    let stored = connection
        .query_row(
            "SELECT object_kind, canonical_json FROM objects WHERE object_id = ?1",
            [hash.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    super::super::search_budget::blob(connection, row, 1)?,
                ))
            },
        )
        .optional()?;
    let Some((kind, bytes)) = stored else {
        return Err(StoreError::InvalidWorkProjection(format!(
            "relation anchor {hash} is missing"
        )));
    };
    if kind != "work_event" {
        return Ok(None);
    }
    Ok(Some(CanonicalObject::stored(hash, bytes)?.decode()?))
}

pub(in crate::storage) fn verified_work_identity(
    connection: &Connection,
    work_id: WorkId,
) -> Result<(crate::domain::ProjectId, WorkId), StoreError> {
    let item = load_work_item(connection, work_id)?;
    Ok((item.project_id, item.root_id))
}

pub(in crate::storage::work) fn load_work_items_query(
    connection: &Connection,
    query: &str,
    work_id: WorkId,
) -> Result<Vec<WorkItem>, StoreError> {
    let mut statement = connection.prepare(query)?;
    statement
        .query_map([work_id.0.to_string()], |row| row.get::<_, Vec<u8>>(0))?
        .map(|row| serde_json::from_slice(&row?).map_err(StoreError::from))
        .collect()
}
