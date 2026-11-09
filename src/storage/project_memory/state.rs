//! Project-memory change positions and rebuildable state derivation.

use super::{
    Connection, MemoryAssertionEvent, ObjectId, OptionalExtension, SCHEMA_VERSION, SqliteStore,
    StoreError, params,
};

pub(in crate::storage) fn project_memory_state_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
) -> Result<(usize, i64), StoreError> {
    let state = connection
        .query_row(
            "SELECT active_count, change_position
             FROM project_memory_state WHERE project_id = ?1",
            [project_id.0.as_str()],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .optional()?;
    let state = if let Some(state) = state {
        state
    } else {
        let has_project_memory = connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM objects INDEXED BY objects_project_memory_key
                 WHERE object_kind = 'memory_version'
                   AND json_extract(canonical_json, '$.scope.kind') = 'project'
                   AND json_type(canonical_json, '$.project_key') = 'text'
                   AND json_extract(canonical_json, '$.scope.project') = ?1
                 LIMIT 1
             )",
            [project_id.0.as_str()],
            |row| row.get::<_, bool>(0),
        )?;
        if has_project_memory {
            return Err(StoreError::InvalidMemoryProjection(
                "project memory state is missing for a retained project key".into(),
            ));
        }
        (0, 0)
    };
    let count = usize::try_from(state.0).map_err(|_| {
        StoreError::InvalidMemoryProjection("project memory count is invalid".into())
    })?;
    Ok((count, state.1))
}

/// The keys behind the project's memory changes after position `since`, up
/// to `current`. Each change stored one assertion and advanced the position
/// by one in the same transaction, and writes are serialized, so the newest
/// `current - since` assertions in insertion order are exactly those changes.
/// Every path that inserts assertions otherwise (graph restore, replay,
/// projection repair, migration import) starts the recorded positions over,
/// so no recorded position spans one. A key counts once, with its newest
/// change deciding whether it was forgotten.
pub(super) fn project_memory_key_changes_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
    since: i64,
    current: i64,
) -> Result<crate::domain::ProjectMemoryKeyChanges, StoreError> {
    let newer = current.saturating_sub(since);
    if newer <= 0 {
        return Ok(crate::domain::ProjectMemoryKeyChanges::default());
    }
    let rows = connection
        .prepare(
            "WITH events AS (
                 SELECT json_extract(version.canonical_json, '$.project_key') AS key,
                        json_extract(assertion.canonical_json, '$.status') AS status,
                        assertion.rowid AS sequence
                 FROM objects AS version INDEXED BY objects_project_memory_key
                 JOIN objects AS assertion INDEXED BY objects_memory_assertion_version
                   ON assertion.object_kind = 'memory_assertion_event'
                  AND json_extract(assertion.canonical_json, '$.version') = version.object_id
                 WHERE version.object_kind = 'memory_version'
                   AND json_extract(version.canonical_json, '$.scope.kind') = 'project'
                   AND json_type(version.canonical_json, '$.project_key') = 'text'
                   AND json_extract(version.canonical_json, '$.scope.project') = ?1
                 ORDER BY assertion.rowid DESC
                 LIMIT ?2
             )
             SELECT key, status, MAX(sequence) AS newest
             FROM events
             GROUP BY key
             ORDER BY newest DESC",
        )?
        .query_map(params![project_id.0, newer], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut changes = crate::domain::ProjectMemoryKeyChanges {
        count: rows.len(),
        ..Default::default()
    };
    for (key, status) in rows {
        let forgotten = status != "active";
        changes.forgotten += usize::from(forgotten);
        if changes.keys.len() < crate::domain::MAX_NAMED_PROJECT_MEMORY_CHANGES {
            changes
                .keys
                .push(crate::domain::ProjectMemoryKeyChange { key, forgotten });
        }
    }
    Ok(changes)
}

pub(super) fn advance_project_memory_state_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
    active_delta: i64,
) -> Result<(), StoreError> {
    let changed = if active_delta == 1 {
        connection.execute(
            "INSERT INTO project_memory_state (
                 project_id, active_count, change_position
             ) VALUES (?1, 1, 1)
             ON CONFLICT(project_id) DO UPDATE SET
                 active_count = project_memory_state.active_count + 1,
                 change_position = project_memory_state.change_position + 1",
            [project_id.0.as_str()],
        )?
    } else if active_delta == -1 {
        connection.execute(
            "UPDATE project_memory_state
             SET active_count = active_count - 1,
                 change_position = change_position + 1
             WHERE project_id = ?1 AND active_count > 0",
            [project_id.0.as_str()],
        )?
    } else if active_delta == 0 {
        connection.execute(
            "UPDATE project_memory_state SET change_position = change_position + 1
             WHERE project_id = ?1 AND active_count > 0",
            [project_id.0.as_str()],
        )?
    } else {
        return Err(StoreError::InvalidMemoryProjection(
            "project memory state delta must be minus one, zero or one".into(),
        ));
    };
    if changed != 1 {
        return Err(StoreError::InvalidMemoryProjection(
            "project memory state is missing or inconsistent".into(),
        ));
    }
    Ok(())
}

pub(in crate::storage) fn derived_project_memory_state_rows_on(
    connection: &Connection,
) -> Result<Vec<(String, i64, i64)>, StoreError> {
    Ok(connection
        .prepare(
            "WITH assertion_counts AS (
                 SELECT json_extract(canonical_json, '$.version') AS version_id,
                        COUNT(*) AS assertion_count
                 FROM objects
                 WHERE object_kind = 'memory_assertion_event'
                 GROUP BY version_id
             )
             SELECT json_extract(version.canonical_json, '$.scope.project') AS project_id,
                    SUM(CASE WHEN head.status = 'active' THEN 1 ELSE 0 END),
                    SUM(COALESCE(assertion_counts.assertion_count, 0))
             FROM objects AS version
             LEFT JOIN memory_heads AS head ON head.version_id = version.object_id
             LEFT JOIN assertion_counts ON assertion_counts.version_id = version.object_id
             WHERE version.object_kind = 'memory_version'
               AND json_extract(version.canonical_json, '$.scope.kind') = 'project'
               AND json_type(version.canonical_json, '$.project_key') = 'text'
             GROUP BY json_extract(version.canonical_json, '$.scope.project')
             ORDER BY json_extract(version.canonical_json, '$.scope.project')",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, i64>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?)
}

pub(in crate::storage) fn derived_project_memory_state_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
) -> Result<(i64, i64), StoreError> {
    let heads = connection
        .prepare(
            "SELECT json_extract(version.canonical_json, '$.memory_id'),
                    COALESCE(head.status, 'superseded'), version.object_id
             FROM objects AS version INDEXED BY objects_project_memory_key
             LEFT JOIN memory_heads AS head ON head.version_id = version.object_id
             WHERE version.object_kind = 'memory_version'
               AND json_extract(version.canonical_json, '$.scope.kind') = 'project'
               AND json_type(version.canonical_json, '$.project_key') = 'text'
               AND json_extract(version.canonical_json, '$.scope.project') = ?1
             ORDER BY json_extract(version.canonical_json, '$.project_key')",
        )?
        .query_map([project_id.0.as_str()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut active_count = 0_i64;
    let mut change_position = 0_i64;
    let mut assertion_statement = connection.prepare(
        "SELECT object_id
         FROM objects INDEXED BY objects_memory_assertion_version
         WHERE object_kind = 'memory_assertion_event'
           AND json_extract(canonical_json, '$.version') = ?1
         ORDER BY object_id",
    )?;
    for (memory_id, status, version_id) in heads {
        active_count = active_count
            .checked_add(i64::from(status == "active"))
            .ok_or_else(|| {
                StoreError::InvalidMemoryProjection(
                    "project memory active count exceeds SQLite range".into(),
                )
            })?;
        let assertion_hashes = assertion_statement
            .query_map([version_id.as_str()], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for stored_hash in assertion_hashes {
            let assertion_id = ObjectId::from_stored(stored_hash.clone())
                .ok_or(StoreError::InvalidStoredKey(stored_hash))?;
            let assertion: MemoryAssertionEvent = SqliteStore::get_typed_object_on(
                connection,
                &assertion_id,
                "memory_assertion_event",
            )?
            .ok_or_else(|| {
                StoreError::InvalidMemoryProjection(format!(
                    "project memory assertion {assertion_id} is missing"
                ))
            })?;
            if assertion.schema_version != SCHEMA_VERSION
                || assertion.memory_id.0.to_string() != memory_id
                || assertion.version.as_str() != version_id
            {
                return Err(StoreError::InvalidMemoryProjection(format!(
                    "project memory assertion {assertion_id} disagrees with its head"
                )));
            }
            change_position = change_position.checked_add(1).ok_or_else(|| {
                StoreError::InvalidMemoryProjection(
                    "project memory change position exceeds SQLite range".into(),
                )
            })?;
        }
    }
    Ok((active_count, change_position))
}
