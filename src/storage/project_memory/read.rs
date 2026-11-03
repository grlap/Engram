//! Full and bounded listing reads over validated project-memory history.

use super::retirement::retiring_state_on;
use super::state::project_memory_state_on;
use super::{
    ActorContext, Authority, Connection, Delivery, FILTERED_SEARCH_AFTER_REFUSAL,
    MemoryAssertionEvent, MemoryHeadProjectionRow, MemoryKind, MemoryStatus, MemoryVersion,
    ObjectId, OptionalExtension, PROJECT_MEMORY_FIRST_LINE_BYTES, PROJECT_MEMORY_LIST_LIMIT,
    ProjectMemoryFull, ProjectMemoryList, ProjectMemoryListRow, ProjectMemoryListingCut, Scope,
    SessionId, SqliteStore, StoreError, StoredProjectMemory, admit_live_project_memory_sessions,
    fts_query, history, memory_full, normalize_project_memory_query, params,
    project_memory_history_on, validate_keyed_project_memory_shape,
    validate_project_memory_authorization, validate_project_memory_key,
};

impl SqliteStore {
    /// Returns a dedicated bounded full-read envelope for one live key.
    ///
    /// # Errors
    ///
    /// Returns a typed project-memory refusal when authorization, key
    /// resolution, lifecycle, or stored-envelope validation fails.
    pub fn project_memory_full(
        &self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        actor: &ActorContext,
        key: &str,
        revision: Option<u64>,
    ) -> Result<ProjectMemoryFull, StoreError> {
        admit_live_project_memory_sessions(session_id, actor)?;
        validate_project_memory_authorization(session_id, actor)?;
        if self.connection.is_autocommit() {
            let snapshot = self.connection.unchecked_transaction()?;
            let full = self.project_memory_full(project_id, session_id, actor, key, revision)?;
            snapshot.commit()?;
            return Ok(full);
        }
        let key = validate_project_memory_key(key)?;
        let history = lookup_project_memory_history_on(&self.connection, project_id, &key)?;
        let existing = history
            .last()
            .ok_or_else(|| StoreError::ProjectMemoryNotFound(key.clone()))?;
        if existing.assertion.status == MemoryStatus::Tombstoned {
            return Err(StoreError::ProjectMemoryRetired(key));
        }
        if existing.assertion.status != MemoryStatus::Active {
            return Err(StoreError::InvalidMemoryProjection(format!(
                "project memory key has unsupported status {:?}",
                existing.assertion.status
            )));
        }
        let current = history_revision(&history)?;
        let revision = revision.unwrap_or(current);
        let index = revision
            .checked_sub(1)
            .and_then(|n| usize::try_from(n).ok())
            .filter(|index| *index < history.len())
            .ok_or_else(|| StoreError::ProjectMemoryRevisionNotFound {
                key: key.clone(),
                revision,
                current,
            })?;
        let mut full = memory_full(&key, &history, index, current);
        full.retiring_state =
            retiring_state_on(&self.connection, project_id, full.retiring_target.as_ref())?;
        Ok(full)
    }

    /// Lists live project memories without returning their bodies.
    ///
    /// # Errors
    ///
    /// Returns a typed project-memory refusal when authorization, cursor, or
    /// query validation fails, or when the stored projection is invalid.
    pub fn project_memories(
        &self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        actor: &ActorContext,
        query: Option<&str>,
        after: Option<&str>,
    ) -> Result<ProjectMemoryList, StoreError> {
        self.project_memories_at_cut(project_id, session_id, actor, query, after, false)
            .map(|(list, _)| list)
    }

    /// The listing, and with `records` the memory position its own snapshot
    /// read. A listing that carries a context generation records that
    /// position, so a memory a peer writes after the snapshot is announced
    /// again. Only that listing reads the position: every other form lists
    /// from the memory rows alone, as it always did, even while the position
    /// is missing. The record is advisory, so a position that cannot be read
    /// leaves the listing delivered and unrecorded.
    pub(crate) fn project_memories_at_cut(
        &self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        actor: &ActorContext,
        query: Option<&str>,
        after: Option<&str>,
        records: bool,
    ) -> Result<(ProjectMemoryList, Option<ProjectMemoryListingCut>), StoreError> {
        admit_live_project_memory_sessions(session_id, actor)?;
        validate_project_memory_authorization(session_id, actor)?;
        let normalized_query = normalize_project_memory_query(query)?;
        if normalized_query.is_some() && after.is_some() {
            return Err(StoreError::InvalidProjectMemory(
                FILTERED_SEARCH_AFTER_REFUSAL.cli.into(),
            ));
        }
        let normalized_after = after.map(validate_project_memory_key).transpose()?;
        // One snapshot: its own, or an enclosing read transaction it joins.
        let read = |connection: &Connection| -> Result<_, StoreError> {
            let rows = project_memory_rows_on(
                connection,
                project_id,
                normalized_query,
                normalized_after.as_deref(),
                PROJECT_MEMORY_LIST_LIMIT + 1,
            )?;
            let state = records
                .then(|| project_memory_state_on(connection, project_id).ok())
                .flatten();
            Ok((rows, state))
        };
        let ((rows, total_matches), state) = if self.connection.is_autocommit() {
            let transaction = self.connection.unchecked_transaction()?;
            let read = read(&transaction)?;
            transaction.commit()?;
            read
        } else {
            read(&self.connection)?
        };
        let has_more = rows.len() > PROJECT_MEMORY_LIST_LIMIT;
        let memories = rows
            .into_iter()
            .take(PROJECT_MEMORY_LIST_LIMIT)
            .collect::<Vec<_>>();
        let omitted_count = total_matches
            .unwrap_or(memories.len())
            .saturating_sub(memories.len());
        let next_after = if normalized_query.is_none() && has_more {
            memories.last().map(|row| row.key.clone())
        } else {
            None
        };
        let exhausted = if normalized_query.is_some() {
            omitted_count == 0
        } else {
            next_after.is_none()
        };
        let listing = state.map(|(_, change_position)| ProjectMemoryListingCut { change_position });
        Ok((
            ProjectMemoryList {
                memories,
                next_after,
                omitted_count,
                exhausted,
            },
            listing,
        ))
    }
}

pub(in crate::storage) fn lookup_project_memory_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
    key: &str,
) -> Result<Option<StoredProjectMemory>, StoreError> {
    Ok(lookup_project_memory_history_on(connection, project_id, key)?.pop())
}

pub(super) fn history_revision(history: &[StoredProjectMemory]) -> Result<u64, StoreError> {
    u64::try_from(history.len()).map_err(|_| {
        StoreError::InvalidMemoryProjection("memory revision exceeds its range".into())
    })
}

// Return the validated chain to callers needing its head and revision count;
// neither counting nor selecting a historical body bypasses validation.
pub(super) fn lookup_project_memory_history_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
    key: &str,
) -> Result<Vec<StoredProjectMemory>, StoreError> {
    // The hard index requirement makes a missing or incompatible rebuildable
    // projection fail closed; open/doctor names the explicit repair command.
    let stored = connection
        .query_row(
            "SELECT head.memory_id, head.version_id, head.assertion_id,
                    head.schema_version, head.status, head.scope_kind,
                    head.project_id, head.task_id, head.work_id, head.agent_id,
                    head.memory_kind, head.authority, head.delivery,
                    head.sensitivity, head.title, head.body, head.created_at_ms
             FROM objects AS object INDEXED BY objects_project_memory_key
             JOIN memory_heads AS head ON head.version_id = object.object_id
             WHERE object.object_kind = 'memory_version'
               AND json_extract(object.canonical_json, '$.scope.kind') = 'project'
               AND json_type(object.canonical_json, '$.project_key') = 'text'
               AND json_extract(object.canonical_json, '$.scope.project') = ?1
               AND json_extract(object.canonical_json, '$.project_key') = ?2",
            params![project_id.0, key],
            |row| {
                Ok(MemoryHeadProjectionRow {
                    memory_id: row.get(0)?,
                    version_id: row.get(1)?,
                    assertion_id: row.get(2)?,
                    schema_version: row.get(3)?,
                    status: row.get(4)?,
                    scope_kind: row.get(5)?,
                    project_id: row.get(6)?,
                    task_id: row.get(7)?,
                    work_id: row.get(8)?,
                    agent_id: row.get(9)?,
                    memory_kind: row.get(10)?,
                    authority: row.get(11)?,
                    delivery: row.get(12)?,
                    sensitivity: row.get(13)?,
                    title: row.get(14)?,
                    body: row.get(15)?,
                    created_at_ms: row.get(16)?,
                })
            },
        )
        .optional()?;
    let Some(stored) = stored else {
        let reserved = connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM objects INDEXED BY objects_project_memory_key
                 WHERE object_kind = 'memory_version'
                   AND json_extract(canonical_json, '$.scope.kind') = 'project'
                   AND json_type(canonical_json, '$.project_key') = 'text'
                   AND json_extract(canonical_json, '$.scope.project') = ?1
                   AND json_extract(canonical_json, '$.project_key') = ?2
             )",
            params![project_id.0, key],
            |row| row.get::<_, bool>(0),
        )?;
        if reserved {
            return Err(StoreError::InvalidMemoryProjection(
                "project memory key is reserved but its durable head is missing".into(),
            ));
        }
        return Ok(Vec::new());
    };
    let version_id = ObjectId::from_stored(stored.version_id.clone())
        .ok_or_else(|| StoreError::InvalidStoredKey(stored.version_id.clone()))?;
    let assertion_id = ObjectId::from_stored(stored.assertion_id.clone())
        .ok_or_else(|| StoreError::InvalidStoredKey(stored.assertion_id.clone()))?;
    let version: MemoryVersion =
        SqliteStore::get_typed_object_on(connection, &version_id, "memory_version")?.ok_or_else(
            || StoreError::InvalidMemoryProjection("project memory version is missing".into()),
        )?;
    let assertion: MemoryAssertionEvent =
        SqliteStore::get_typed_object_on(connection, &assertion_id, "memory_assertion_event")?
            .ok_or_else(|| {
                StoreError::InvalidMemoryProjection("project memory assertion is missing".into())
            })?;
    validate_keyed_project_memory_shape(&version, &assertion)?;
    let expected = SqliteStore::expected_memory_head_projection(
        &version_id,
        &assertion_id,
        &version,
        &assertion,
        assertion.status,
    )?;
    let shape_matches = stored == expected
        && version.project_key.as_deref() == Some(key)
        && matches!(&version.scope, Scope::Project { project } if project == project_id)
        && version.kind == MemoryKind::Episode
        && version.authority == Authority::Soft
        && version.delivery == Delivery::OnDemand
        && assertion.memory_id == version.memory_id
        && assertion.version == version_id;
    if !shape_matches {
        return Err(StoreError::InvalidMemoryProjection(
            "project memory key projection does not match its canonical objects".into(),
        ));
    }
    let history = project_memory_history_on(connection, project_id, key)?;
    if history
        .last()
        .is_none_or(|entry| entry.version_id != version_id || entry.assertion != assertion)
    {
        return Err(StoreError::InvalidMemoryProjection(
            "project memory head is not the current canonical revision".into(),
        ));
    }
    Ok(history)
}

pub(in crate::storage) fn validate_stored_project_memory_key(
    key: &str,
) -> Result<String, StoreError> {
    validate_project_memory_key(key).map_err(|_| {
        StoreError::InvalidMemoryProjection(
            "project memory list candidate has an unsafe canonical key".into(),
        )
    })
}

pub(super) fn project_memory_rows_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
    query: Option<&str>,
    after: Option<&str>,
    limit: usize,
) -> Result<(Vec<ProjectMemoryListRow>, Option<usize>), StoreError> {
    let limit = i64::try_from(limit)
        .map_err(|_| StoreError::InvalidProjectMemory("memory list limit is invalid".into()))?;
    let (keys, total_matches) = if let Some(query) = query {
        let lowered_key_query = query.to_ascii_lowercase();
        let escaped_key_query = lowered_key_query
            .replace('\\', "\\\\")
            .replace('%', "\\%")
            .replace('_', "\\_");
        // A query with no searchable fragment finds nothing.
        let Some(fts_query) = fts_query(query)? else {
            return Ok((Vec::new(), Some(0)));
        };
        let mut statement = connection.prepare(
            "SELECT json_extract(object.canonical_json, '$.project_key'),
                    COUNT(*) OVER()
             FROM object_fts AS f
             JOIN memory_heads AS head ON head.version_id = f.object_id
             JOIN objects AS object ON object.object_id = head.version_id
             WHERE object.object_kind = 'memory_version'
               AND json_extract(object.canonical_json, '$.scope.kind') = 'project'
               AND json_extract(object.canonical_json, '$.scope.project') = ?1
               AND json_type(object.canonical_json, '$.project_key') = 'text'
               AND head.status = 'active'
               AND object_fts MATCH ?2
             ORDER BY
                 CASE
                     WHEN json_extract(object.canonical_json, '$.project_key') = ?3 THEN 0
                     WHEN lower(json_extract(object.canonical_json, '$.project_key'))
                         LIKE ?4 || '%' ESCAPE '\\' THEN 1
                     ELSE 2
                 END,
                 f.rank,
                 json_extract(object.canonical_json, '$.project_key')
             LIMIT ?5",
        )?;
        let matches = statement
            .query_map(
                params![
                    project_id.0,
                    fts_query,
                    lowered_key_query,
                    escaped_key_query,
                    limit
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let total = matches
            .first()
            .map_or(Ok(0), |(_, total)| usize::try_from(*total))
            .map_err(|_| {
                StoreError::InvalidMemoryProjection("memory match count is invalid".into())
            })?;
        (
            matches.into_iter().map(|(key, _)| key).collect(),
            Some(total),
        )
    } else {
        let mut statement = connection.prepare(
            "SELECT json_extract(object.canonical_json, '$.project_key')
             FROM memory_heads AS head
             JOIN objects AS object ON object.object_id = head.version_id
             WHERE object.object_kind = 'memory_version'
               AND json_extract(object.canonical_json, '$.scope.kind') = 'project'
               AND json_extract(object.canonical_json, '$.scope.project') = ?1
               AND json_type(object.canonical_json, '$.project_key') = 'text'
               AND head.status = 'active'
               AND (?2 IS NULL OR json_extract(object.canonical_json, '$.project_key') > ?2)
             ORDER BY json_extract(object.canonical_json, '$.project_key')
             LIMIT ?3",
        )?;
        (
            statement
                .query_map(params![project_id.0, after, limit], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?,
            None,
        )
    };
    let rows = keys
        .into_iter()
        .map(|key| {
            let key = validate_stored_project_memory_key(&key)?;
            let history = lookup_project_memory_history_on(connection, project_id, &key)?;
            let stored = history.last().ok_or_else(|| {
                StoreError::InvalidMemoryProjection(
                    "project memory list candidate has no canonical binding".into(),
                )
            })?;
            if stored.assertion.status != MemoryStatus::Active {
                return Err(StoreError::InvalidMemoryProjection(
                    "project memory list candidate is not active".into(),
                ));
            }
            let revision = history_revision(&history)?;
            let mut row = project_memory_list_row(key, stored, revision);
            row.retiring_state =
                retiring_state_on(connection, project_id, row.retiring_target.as_ref())?;
            row.retiring_target_dropped =
                history::retiring_target_dropped(&history, history.len() - 1);
            Ok(row)
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    Ok((rows, total_matches))
}

fn project_memory_list_row(
    key: String,
    stored: &StoredProjectMemory,
    revision: u64,
) -> ProjectMemoryListRow {
    ProjectMemoryListRow {
        key,
        revision,
        first_line: project_memory_first_line(&stored.version.body),
        remembered_at: stored.version.created_at,
        actor_id: stored.version.actor.actor_id.clone(),
        actor_context: stored
            .version
            .actor
            .attribution_context()
            .map(str::to_owned),
        retiring_target: stored.version.retiring_target.clone(),
        retiring_state: None,
        retiring_target_dropped: None,
        workaround: stored.version.retiring_target.as_ref().map(|_| true),
    }
}

pub(super) fn project_memory_first_line(body: &str) -> String {
    let line = body
        .lines()
        .map(|line| line.split_whitespace().collect::<Vec<_>>().join(" "))
        .find(|line| !line.is_empty())
        .unwrap_or_default();
    if line.len() <= PROJECT_MEMORY_FIRST_LINE_BYTES {
        return line;
    }
    let mut end = PROJECT_MEMORY_FIRST_LINE_BYTES;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", line[..end].trim_end())
}
