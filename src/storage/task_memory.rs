use super::{
    ActorContext, CanonicalObject, ChangeCursor, Connection, ContextAssembly, ContextItem,
    ContextOmission, ContextOmissionSummary, ContextPacket, ContextPacketHeader,
    ContextPacketPayload, DateTime, Delivery, INDEX_CONTEXT_BUDGET, MAX_EXACT_CONTEXT_OMISSIONS,
    MAX_PROJECT_MEMORY_QUERY_BYTES, MAX_PROJECT_MEMORY_QUERY_TOKENS, MemoryAssertionEvent,
    MemoryId, MemoryProjectionMode, MemoryStatus, MemorySummary, MemoryVersion,
    NoteIntentFingerprint, NoteIntentKey, NoteReceipt, NoteRequest, NoteVisibility,
    OptionalExtension, PINNED_CONTEXT_BUDGET, PreparedNote, Redactor, SCHEMA_VERSION, Scope,
    Sensitivity, SessionId, SqliteStore, StoreError, TaskId, Transaction, TransactionBehavior, Utc,
    activation_policy, classify_note, params, work,
};

#[cfg(test)]
use super::{HashMap, MemoryRecord, ObjectId};

#[cfg(test)]
mod tests;

impl SqliteStore {
    /// Captures one attributed prose note through the configured pre-write
    /// inspection port. Classification, canonical objects, projections, peer
    /// feed entry, and idempotency receipt commit atomically.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when inspection refuses the prose, an
    /// idempotency key changes meaning, or persistence fails.
    #[allow(
        clippy::too_many_lines,
        reason = "note objects, memory projections, claim renewal, work feeds, and the replay receipt remain one atomic transaction"
    )]
    pub fn capture_note<R: Redactor>(
        &mut self,
        request: &NoteRequest,
        redactor: &R,
    ) -> Result<NoteReceipt, StoreError> {
        crate::storage::admit_live_actor_session(&request.actor)?;
        Self::validate_note_content(request, redactor)?;

        let request_object = note_fingerprint(request)?;
        let intent_key = note_intent_key(request)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if matches!(request.visibility, NoteVisibility::Shared) && request.work_id.is_some() {
            work::require_work_schema_version(&transaction, self.work_schema_version)?;
        }
        Self::validate_note_anchors_on(&transaction, request)?;
        if let Some((stored_request, receipt_json)) = transaction
            .query_row(
                "SELECT request_hash, receipt_json FROM note_intents
                 WHERE idempotency_key = ?1",
                [&intent_key],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?
        {
            if stored_request != request_object.key().as_str() {
                return Err(StoreError::NoteIdempotencyConflict(
                    request.idempotency_key.clone(),
                ));
            }
            let mut receipt: NoteReceipt = serde_json::from_slice(&receipt_json)?;
            receipt.duplicate = true;
            return Ok(receipt);
        }

        let prepared = prepare_note(request)?;

        Self::insert_object(&transaction, "memory_version", &prepared.version_object)?;
        Self::insert_object(
            &transaction,
            "memory_assertion_event",
            &prepared.assertion_object,
        )?;
        let cursor = if prepared.version.scope.is_task_shared() {
            let task_id = prepared.version.scope.task_id().ok_or_else(|| {
                StoreError::InvalidMemoryProjection("shared scope has no task id".into())
            })?;
            Some(Self::insert_task_change(
                &transaction,
                task_id,
                "memory_assertion_event",
                &prepared.assertion_object,
            )?)
        } else {
            None
        };
        Self::apply_memory_projection(
            &transaction,
            prepared.version_object.key(),
            prepared.assertion_object.key(),
            &prepared.version,
            &prepared.assertion,
            MemoryProjectionMode::Live,
        )?;
        Self::bump_memory_context_revision_on(&transaction, &prepared.version.scope)?;
        let work_positions = if prepared.version.scope.is_work_shared() {
            let work_id = prepared.version.scope.work_id().ok_or_else(|| {
                StoreError::InvalidMemoryProjection("shared work scope has no work id".into())
            })?;
            let holder = request.actor.session_id.as_ref().ok_or_else(|| {
                StoreError::InvalidMemoryProjection(
                    "work-scoped memory requires an attributed session".into(),
                )
            })?;
            work::append_memory_capture_to_work_feeds(
                &transaction,
                work_id,
                holder,
                request.created_at,
                &request.actor,
                &prepared.version,
                &prepared.assertion,
                &prepared.version_object,
                &prepared.assertion_object,
            )?
        } else {
            Vec::new()
        };

        let receipt = NoteReceipt {
            idempotency_key: request.idempotency_key.clone(),
            memory_id: prepared.version.memory_id,
            version: prepared.version_object.key().clone(),
            assertion: prepared.assertion_object.key().clone(),
            status: prepared.assertion.status,
            kind: prepared.version.kind,
            authority: prepared.version.authority,
            delivery: prepared.version.delivery,
            scope: prepared.version.scope.clone(),
            cursor,
            work_positions,
            classification_reason: prepared.version.classification_reason.clone(),
            policy_reason: prepared.assertion.policy_reason.clone(),
            duplicate: false,
        };
        transaction.execute(
            "INSERT INTO note_intents (idempotency_key, request_hash, receipt_json)
              VALUES (?1, ?2, ?3)",
            params![
                intent_key,
                request_object.key().as_str(),
                serde_json::to_vec(&receipt)?,
            ],
        )?;
        transaction.commit()?;
        Ok(receipt)
    }

    fn validate_note_content<R: Redactor>(
        request: &NoteRequest,
        redactor: &R,
    ) -> Result<(), StoreError> {
        if request.prose.trim().is_empty() {
            return Err(StoreError::EmptyNote);
        }
        inspect_generic_memory_actor_context(&request.actor, redactor)?;
        redactor
            .inspect(&request.prose)
            .map_err(StoreError::RedactionRefused)?;
        Ok(())
    }

    fn validate_note_anchors_on(
        connection: &Connection,
        request: &NoteRequest,
    ) -> Result<(), StoreError> {
        if let Some(task_id) = request.task_id {
            let session_id = request.actor.session_id.as_ref().ok_or_else(|| {
                StoreError::InvalidMemoryProjection(
                    "task-scoped memory requires an attributed session".into(),
                )
            })?;
            Self::ensure_active_task_on(connection, &request.project_id, task_id, session_id)?;
        }
        if let Some(work_id) = request.work_id {
            let session_id = request.actor.session_id.as_ref().ok_or_else(|| {
                StoreError::InvalidMemoryProjection(
                    "work-scoped memory requires an attributed session".into(),
                )
            })?;
            let (focused_work_id, _) =
                Self::focused_work_for_session_on(connection, &request.project_id, session_id)?;
            if focused_work_id != Some(work_id) {
                return Err(StoreError::InvalidMemoryProjection(
                    "work-scoped memory must match the session's persisted focus".into(),
                ));
            }
        }
        Ok(())
    }

    /// Returns memories visible to an agent, optionally narrowed by full-text
    /// query. Explicit search includes proposed records so review pressure is
    /// inspectable; context assembly applies its stricter status filter.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the derived index contains invalid data or
    /// SQLite cannot perform the query.
    #[cfg(test)]
    #[allow(
        clippy::too_many_arguments,
        reason = "search authorization binds project, task, work focus, session, and actor"
    )]
    pub fn search_memories(
        &self,
        project_id: &crate::domain::ProjectId,
        task_id: Option<TaskId>,
        work_id: Option<crate::domain::WorkId>,
        session_id: &SessionId,
        agent_id: &str,
        query: Option<&str>,
        limit: u32,
    ) -> Result<Vec<MemorySummary>, StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        if let Some(task_id) = task_id {
            Self::ensure_active_task_on(&transaction, project_id, task_id, session_id)?;
        }
        let (focused_work_id, focused_root_id) =
            Self::focused_work_for_session_on(&transaction, project_id, session_id)?;
        if work_id.is_some() && work_id != focused_work_id {
            return Err(StoreError::InvalidWork(
                "work-memory search must match the session's persisted focus".into(),
            ));
        }
        let work_root_id = work_id.and(focused_root_id);
        let memories = Self::search_memories_on(
            &transaction,
            project_id,
            task_id,
            work_id,
            work_root_id,
            agent_id,
            query,
            Some(limit),
        )?;
        transaction.commit()?;
        Ok(memories)
    }

    /// Returns current memories bound to one local work item and visible to
    /// the requesting actor. Shared work memories are visible to every actor
    /// focused on the item; agent-scoped work memories remain private.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the work belongs to another project, a
    /// canonical projection is invalid, or SQLite cannot perform the query.
    pub fn search_work_memories(
        &self,
        project_id: &crate::domain::ProjectId,
        work_id: crate::domain::WorkId,
        session_id: &SessionId,
        agent_id: &str,
        query: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Vec<MemorySummary>, StoreError> {
        let read_guard = self
            .connection
            .is_autocommit()
            .then(|| self.connection.unchecked_transaction())
            .transpose()?;
        let transaction = &self.connection;
        let (focused_work_id, _) =
            Self::focused_work_for_session_on(transaction, project_id, session_id)?;
        if focused_work_id != Some(work_id) {
            return Err(StoreError::InvalidWork(
                "work-memory query must match the session's persisted focus".into(),
            ));
        }
        let (work_project, work_root_id) = work::verified_work_identity(transaction, work_id)?;
        if work_project != *project_id {
            return Err(StoreError::InvalidWork(
                "work-memory query must stay within the bound project".into(),
            ));
        }
        let visibility = "h.project_id = ?1 AND h.work_id = ?2 AND
             h.sensitivity != 'restricted' AND
             h.status IN ('active', 'proposed', 'stale') AND
             (h.scope_kind = 'agent' AND h.agent_id = ?3)";
        let root_visibility = "h.project_id = ?1 AND
             h.sensitivity != 'restricted' AND
             h.status IN ('active', 'proposed', 'stale') AND
             h.scope_kind = 'work' AND h.work_id IN (
                 SELECT item.work_id FROM work_items item
                 WHERE item.project_id = ?1 AND item.root_id = ?4
             )";
        let visibility = format!("(({visibility}) OR ({root_visibility}))");
        let limit = limit.map_or(i64::MAX, |limit| i64::from(limit.clamp(1, 1_000)));
        let rows = if let Some(query) = query.filter(|value| !value.trim().is_empty()) {
            let fts_query = fts_query(query);
            let sql = format!(
                "SELECT h.memory_id, h.version_id, h.status, h.memory_kind,
                        h.authority, h.delivery, h.scope_kind, h.project_id,
                        h.task_id, h.work_id, h.agent_id, h.title, h.body, h.sensitivity,
                        h.created_at_ms
                 FROM object_fts f JOIN memory_heads h
                   ON h.version_id = f.object_id
                 WHERE {visibility} AND object_fts MATCH ?5
                 ORDER BY bm25(object_fts), h.created_at_ms DESC LIMIT ?6"
            );
            let mut statement = transaction.prepare(&sql)?;
            let mapped = statement.query_map(
                params![
                    project_id.0,
                    work_id.0.to_string(),
                    agent_id,
                    work_root_id.0.to_string(),
                    fts_query,
                    limit
                ],
                Self::decode_memory_summary,
            )?;
            mapped.collect::<Result<Vec<_>, _>>()?
        } else {
            let sql = format!(
                "SELECT h.memory_id, h.version_id, h.status, h.memory_kind,
                        h.authority, h.delivery, h.scope_kind, h.project_id,
                        h.task_id, h.work_id, h.agent_id, h.title, h.body, h.sensitivity,
                        h.created_at_ms
                 FROM memory_heads h WHERE {visibility}
                 ORDER BY h.created_at_ms DESC, h.memory_id LIMIT ?5"
            );
            let mut statement = transaction.prepare(&sql)?;
            let mapped = statement.query_map(
                params![
                    project_id.0,
                    work_id.0.to_string(),
                    agent_id,
                    work_root_id.0.to_string(),
                    limit
                ],
                Self::decode_memory_summary,
            )?;
            mapped.collect::<Result<Vec<_>, _>>()?
        };
        let memories = rows
            .into_iter()
            .map(Self::parse_memory_summary)
            .collect::<Result<Vec<_>, _>>()?;
        if let Some(read_guard) = read_guard {
            read_guard.commit()?;
        }
        Ok(memories)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "context assembly supplies independently verified task and work-root anchors"
    )]
    fn search_memories_on(
        connection: &Connection,
        project_id: &crate::domain::ProjectId,
        task_id: Option<TaskId>,
        work_id: Option<crate::domain::WorkId>,
        work_root_id: Option<crate::domain::WorkId>,
        agent_id: &str,
        query: Option<&str>,
        limit: Option<u32>,
    ) -> Result<Vec<MemorySummary>, StoreError> {
        let visibility = "h.project_id = ?1 AND h.sensitivity != 'restricted' AND
             h.status IN ('active', 'proposed', 'stale') AND
             NOT (h.scope_kind = 'project' AND EXISTS (
                 SELECT 1 FROM objects AS keyed
                 WHERE keyed.object_id = h.version_id
                   AND keyed.object_kind = 'memory_version'
                   AND json_type(keyed.canonical_json, '$.project_key') = 'text'
             )) AND
             (h.scope_kind = 'project' OR
              (h.scope_kind = 'task' AND h.task_id = ?2) OR
              (h.scope_kind = 'work' AND h.work_id IN (
                   SELECT item.work_id FROM work_items item
                   WHERE item.project_id = ?1 AND item.root_id = ?4
               )) OR
              (h.scope_kind = 'agent' AND h.agent_id = ?5 AND
               (h.task_id IS NULL OR h.task_id = ?2) AND
               (h.work_id IS NULL OR h.work_id = ?3)))";
        let limit = limit.map_or(i64::MAX, |limit| i64::from(limit.clamp(1, 1_000)));
        let rows = if let Some(query) = query.filter(|value| !value.trim().is_empty()) {
            let fts_query = fts_query(query);
            let sql = format!(
                "SELECT h.memory_id, h.version_id, h.status, h.memory_kind,
                        h.authority, h.delivery, h.scope_kind, h.project_id,
                        h.task_id, h.work_id, h.agent_id, h.title, h.body, h.sensitivity,
                        h.created_at_ms
                 FROM object_fts f JOIN memory_heads h
                   ON h.version_id = f.object_id
                 WHERE {visibility} AND object_fts MATCH ?6
                 ORDER BY bm25(object_fts), h.created_at_ms DESC LIMIT ?7"
            );
            let mut statement = connection.prepare(&sql)?;
            let mapped = statement.query_map(
                params![
                    project_id.0,
                    task_id.map(|value| value.0.to_string()),
                    work_id.map(|value| value.0.to_string()),
                    work_root_id.map(|value| value.0.to_string()),
                    agent_id,
                    fts_query,
                    limit,
                ],
                Self::decode_memory_summary,
            )?;
            mapped.collect::<Result<Vec<_>, _>>()?
        } else {
            let sql = format!(
                "SELECT h.memory_id, h.version_id, h.status, h.memory_kind,
                        h.authority, h.delivery, h.scope_kind, h.project_id,
                        h.task_id, h.work_id, h.agent_id, h.title, h.body, h.sensitivity,
                        h.created_at_ms
                 FROM memory_heads h WHERE {visibility}
                 ORDER BY h.created_at_ms DESC, h.memory_id LIMIT ?6"
            );
            let mut statement = connection.prepare(&sql)?;
            let mapped = statement.query_map(
                params![
                    project_id.0,
                    task_id.map(|value| value.0.to_string()),
                    work_id.map(|value| value.0.to_string()),
                    work_root_id.map(|value| value.0.to_string()),
                    agent_id,
                    limit,
                ],
                Self::decode_memory_summary,
            )?;
            mapped.collect::<Result<Vec<_>, _>>()?
        };
        rows.into_iter().map(Self::parse_memory_summary).collect()
    }

    /// Rebuilds all disposable memory projections from verified canonical
    /// assertion and version objects. Unsupported schemas remain stored but
    /// are intentionally not activated.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when canonical objects fail verification or the
    /// derived tables cannot be replaced atomically.
    #[cfg(test)]
    pub(super) fn rebuild_memory_index(&mut self) -> Result<usize, StoreError> {
        let assertions = {
            let mut statement = self.connection.prepare(
                "SELECT object_id, canonical_json FROM objects
                 WHERE object_kind = 'memory_assertion_event'
                 ORDER BY created_at, object_id",
            )?;
            let mapped = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?;
            mapped.collect::<Result<Vec<_>, _>>()?
        };

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        transaction.execute("DELETE FROM memory_heads", [])?;
        transaction.execute("DELETE FROM object_fts", [])?;
        let mut activated = 0;
        // Canonical objects do not change during this rebuild transaction.
        // Validate each complete keyed chain once before selecting its head;
        // ordinary per-assertion validation below still applies to every row.
        let mut project_heads = HashMap::new();
        for (stored_hash, bytes) in assertions {
            let assertion_id = ObjectId::from_stored(stored_hash.clone())
                .ok_or(StoreError::InvalidStoredKey(stored_hash))?;
            let assertion_object = CanonicalObject::stored(&assertion_id, bytes)?;
            let value: serde_json::Value = serde_json::from_slice(assertion_object.bytes())?;
            if value
                .get("schema_version")
                .and_then(serde_json::Value::as_u64)
                != Some(u64::from(SCHEMA_VERSION))
            {
                continue;
            }
            let assertion: MemoryAssertionEvent = assertion_object.decode()?;
            let version_bytes: Option<Vec<u8>> = transaction
                .query_row(
                    "SELECT canonical_json FROM objects
                     WHERE object_id = ?1 AND object_kind = 'memory_version'",
                    [assertion.version.as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(version_bytes) = version_bytes else {
                return Err(StoreError::InvalidMemoryProjection(format!(
                    "assertion {assertion_id} references missing version {}",
                    assertion.version
                )));
            };
            let version_object = CanonicalObject::stored(&assertion.version, version_bytes)?;
            let version_value: serde_json::Value = serde_json::from_slice(version_object.bytes())?;
            if version_value
                .get("schema_version")
                .and_then(serde_json::Value::as_u64)
                != Some(u64::from(SCHEMA_VERSION))
            {
                continue;
            }
            let version: MemoryVersion = version_object.decode()?;
            if let (Some(key), Scope::Project { project }) = (&version.project_key, &version.scope)
            {
                let identity = (project.clone(), key.clone());
                if !project_heads.contains_key(&identity) {
                    let history = super::project_memory::project_memory_history_on(
                        &transaction,
                        project,
                        key,
                    )?;
                    let head = history.last().ok_or_else(|| {
                        StoreError::InvalidMemoryProjection(
                            "rebuilt project memory has no canonical head".into(),
                        )
                    })?;
                    project_heads.insert(identity.clone(), head.version_id.clone());
                }
                if project_heads.get(&identity) != Some(&assertion.version) {
                    activated += 1;
                    continue;
                }
            }
            Self::apply_memory_projection(
                &transaction,
                &assertion.version,
                &assertion_id,
                &version,
                &assertion,
                MemoryProjectionMode::Replay,
            )?;
            activated += 1;
        }
        Self::rebuild_object_fts_from_heads_on(&transaction)?;
        Self::rebuild_project_memory_state_on(&transaction)?;
        Self::bump_rebuilt_context_revisions_on(&transaction)?;
        transaction.commit()?;
        Ok(activated)
    }

    pub(super) fn context_revisions_on(
        connection: &Connection,
        project_id: &crate::domain::ProjectId,
        agent_id: &str,
    ) -> Result<(i64, i64), StoreError> {
        connection
            .query_row(
                "SELECT
                     COALESCE((
                         SELECT revision FROM project_context_revisions
                         WHERE project_id = ?1
                     ), 0),
                     COALESCE((
                         SELECT revision FROM agent_context_revisions
                         WHERE project_id = ?1 AND agent_id = ?2
                     ), 0)",
                params![project_id.0, agent_id],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .map_err(StoreError::from)
    }

    pub(super) fn bump_project_context_revision_on(
        connection: &Connection,
        project_id: &crate::domain::ProjectId,
    ) -> Result<(), StoreError> {
        connection.execute(
            "INSERT INTO project_context_revisions (project_id, revision)
             VALUES (?1, 1)
             ON CONFLICT(project_id) DO UPDATE
             SET revision = revision + 1",
            [project_id.0.as_str()],
        )?;
        Ok(())
    }

    fn bump_agent_context_revision_on(
        connection: &Connection,
        project_id: &crate::domain::ProjectId,
        agent_id: &str,
    ) -> Result<(), StoreError> {
        connection.execute(
            "INSERT INTO agent_context_revisions (project_id, agent_id, revision)
             VALUES (?1, ?2, 1)
             ON CONFLICT(project_id, agent_id) DO UPDATE
             SET revision = revision + 1",
            params![project_id.0, agent_id],
        )?;
        Ok(())
    }

    fn bump_memory_context_revision_on(
        connection: &Connection,
        scope: &Scope,
    ) -> Result<(), StoreError> {
        match scope {
            Scope::Project { project } => {
                Self::bump_project_context_revision_on(connection, project)
            }
            Scope::Agent { project, agent, .. } => {
                Self::bump_agent_context_revision_on(connection, project, agent)
            }
            Scope::Task { .. } | Scope::Work { .. } => Ok(()),
        }
    }

    #[cfg(test)]
    fn bump_rebuilt_context_revisions_on(connection: &Connection) -> Result<(), StoreError> {
        let affected_projects = {
            let mut statement = connection.prepare(
                "SELECT project_id FROM project_context_revisions
                 UNION SELECT DISTINCT project_id FROM memory_heads",
            )?;
            let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for project_id in affected_projects {
            Self::bump_project_context_revision_on(
                connection,
                &crate::domain::ProjectId(project_id),
            )?;
        }
        let affected_agents = {
            let mut statement = connection.prepare(
                "SELECT project_id, agent_id FROM agent_context_revisions
                 UNION SELECT DISTINCT project_id, agent_id FROM memory_heads
                 WHERE scope_kind = 'agent' AND agent_id IS NOT NULL",
            )?;
            let rows = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?;
            rows.collect::<Result<Vec<_>, _>>()?
        };
        for (project_id, agent_id) in affected_agents {
            Self::bump_agent_context_revision_on(
                connection,
                &crate::domain::ProjectId(project_id),
                &agent_id,
            )?;
        }
        Ok(())
    }

    /// Builds and stores one budgeted, explainable context packet.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the session has not joined the requested
    /// task, pinned memory exceeds its fail-closed budget, or persistence
    /// fails.
    #[cfg(test)]
    pub fn build_context(
        &mut self,
        project_id: &crate::domain::ProjectId,
        task_id: Option<TaskId>,
        session_id: &SessionId,
        agent_id: &str,
        now: DateTime<Utc>,
    ) -> Result<ContextPacket, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let packet =
            Self::build_context_on(&transaction, project_id, task_id, session_id, agent_id, now)?;
        transaction.commit()?;
        Ok(packet)
    }

    pub(super) fn build_context_on(
        transaction: &Transaction<'_>,
        project_id: &crate::domain::ProjectId,
        task_id: Option<TaskId>,
        session_id: &SessionId,
        agent_id: &str,
        now: DateTime<Utc>,
    ) -> Result<ContextPacket, StoreError> {
        if let Some(task_id) = task_id {
            Self::ensure_active_task_on(transaction, project_id, task_id, session_id)?;
        }
        let (work_id, work_root_id) =
            Self::focused_work_for_session_on(transaction, project_id, session_id)?;
        let work_feed_heads = work_id.map_or_else(
            || Ok(Vec::new()),
            |work_id| work::context_work_feed_heads(transaction, work_id),
        )?;
        let (project_context_revision, private_context_revision) =
            Self::context_revisions_on(transaction, project_id, agent_id)?;
        let memories = Self::search_memories_on(
            transaction,
            project_id,
            task_id,
            work_id,
            work_root_id,
            agent_id,
            None,
            None,
        )?;
        let assembly = assemble_context(memories)?;

        let event_cursor = task_id.map_or(Ok(ChangeCursor::default()), |task_id| {
            Self::latest_task_cursor(transaction, task_id)
        })?;
        let payload = ContextPacketPayload {
            schema_version: SCHEMA_VERSION,
            project_id: project_id.clone(),
            task_id,
            work_id,
            work_feed_heads: work_feed_heads.clone(),
            project_context_revision,
            private_context_revision,
            agent_id: agent_id.into(),
            event_cursor,
            pinned: assembly.pinned.clone(),
            index: assembly.index.clone(),
            omissions: assembly.omissions.clone(),
            omission_summaries: assembly.omission_summaries.clone(),
            proposed_count: assembly.proposed_count,
            stale_count: assembly.stale_count,
            created_at: now,
        };
        let object = CanonicalObject::mint(&payload)?;
        Self::insert_object(transaction, "context_packet", &object)?;
        let packet = ContextPacket {
            header: ContextPacketHeader {
                project_id: project_id.clone(),
                task_id,
                work_id,
                work_feed_heads,
                project_context_revision,
                private_context_revision,
                packet_hash: object.key().clone(),
                event_cursor,
                proposed_count: assembly.proposed_count,
                stale_count: assembly.stale_count,
            },
            pinned: assembly.pinned,
            index: assembly.index,
            omissions: assembly.omissions,
            omission_summaries: assembly.omission_summaries,
        };
        Ok(packet)
    }

    pub(super) fn focused_work_for_session_on(
        connection: &Connection,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
    ) -> Result<(Option<crate::domain::WorkId>, Option<crate::domain::WorkId>), StoreError> {
        let stored = connection
            .query_row(
                "SELECT focused_work_id FROM work_session_state
                 WHERE project_id = ?1 AND session_id = ?2",
                params![project_id.0, session_id.0],
                |row| row.get::<_, Option<String>>(0),
            )
            .optional()?
            .flatten();
        let Some(stored) = stored else {
            return Ok((None, None));
        };
        let work_id = uuid::Uuid::parse_str(&stored)
            .map(crate::domain::WorkId)
            .map_err(|_| {
                StoreError::InvalidWorkProjection(format!(
                    "work session focus contains invalid work id {stored}"
                ))
            })?;
        let (work_project, root_id) = work::verified_work_identity(connection, work_id)?;
        if work_project != *project_id {
            return Err(StoreError::InvalidWorkProjection(
                "focused work crosses its session project binding".into(),
            ));
        }
        Ok((Some(work_id), Some(root_id)))
    }

    /// Shows a complete memory record only after checking its project, task,
    /// participant, private owner, and sensitivity boundaries.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::MemoryAccessDenied`] rather than exposing content
    /// when a valid hash crosses a scope boundary.
    #[cfg(test)]
    pub fn show_memory(
        &self,
        version_id: &ObjectId,
        project_id: &crate::domain::ProjectId,
        task_id: Option<TaskId>,
        work_id: Option<crate::domain::WorkId>,
        session_id: &SessionId,
        agent_id: &str,
    ) -> Result<MemoryRecord, StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        let record = Self::show_memory_on(
            &transaction,
            version_id,
            project_id,
            task_id,
            work_id,
            session_id,
            agent_id,
        )?;
        transaction.commit()?;
        Ok(record)
    }

    #[cfg(test)]
    #[allow(
        clippy::too_many_arguments,
        reason = "authorization binds the exact persisted task/work session context"
    )]
    fn show_memory_on(
        connection: &Connection,
        version_id: &ObjectId,
        project_id: &crate::domain::ProjectId,
        task_id: Option<TaskId>,
        work_id: Option<crate::domain::WorkId>,
        session_id: &SessionId,
        agent_id: &str,
    ) -> Result<MemoryRecord, StoreError> {
        let (focused_work_id, focused_root_id) =
            Self::focused_work_for_session_on(connection, project_id, session_id)?;
        if work_id.is_some() && work_id != focused_work_id {
            return Err(StoreError::MemoryAccessDenied(version_id.clone()));
        }
        let assertion_id: Option<String> = connection
            .query_row(
                "SELECT assertion_id FROM memory_heads WHERE version_id = ?1",
                [version_id.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(assertion_id) = assertion_id else {
            return Err(StoreError::MemoryNotFound(version_id.clone()));
        };
        let assertion_id = ObjectId::from_stored(assertion_id.clone())
            .ok_or(StoreError::InvalidStoredKey(assertion_id))?;
        let version: MemoryVersion =
            Self::get_typed_object_on(connection, version_id, "memory_version")?
                .ok_or_else(|| StoreError::MemoryNotFound(version_id.clone()))?;
        let authorized = match &version.scope {
            Scope::Project { project } => project == project_id,
            Scope::Task { project, task } => {
                project == project_id
                    && Some(*task) == task_id
                    && Self::ensure_active_task_on(connection, project_id, *task, session_id)
                        .is_ok()
            }
            Scope::Work { project, work } => {
                if project != project_id {
                    false
                } else if let Some(focused_root) = focused_root_id {
                    let (scoped_project, scoped_root) =
                        work::verified_work_identity(connection, *work)?;
                    scoped_project == *project_id && scoped_root == focused_root
                } else {
                    false
                }
            }
            Scope::Agent {
                project,
                task,
                work,
                agent,
            } => {
                let task_authorized = task.is_none_or(|task| {
                    Some(task) == task_id
                        && Self::ensure_active_task_on(connection, project_id, task, session_id)
                            .is_ok()
                });
                let work_authorized = work.is_none_or(|work| Some(work) == focused_work_id);
                project == project_id && task_authorized && work_authorized && agent == agent_id
            }
        };
        if !authorized || version.sensitivity == Sensitivity::Restricted {
            return Err(StoreError::MemoryAccessDenied(version_id.clone()));
        }
        let assertion: MemoryAssertionEvent =
            Self::get_typed_object_on(connection, &assertion_id, "memory_assertion_event")?
                .ok_or_else(|| StoreError::MemoryNotFound(version_id.clone()))?;
        Ok(MemoryRecord {
            version_id: version_id.clone(),
            assertion_id,
            version,
            assertion,
        })
    }
}

fn note_fingerprint(request: &NoteRequest) -> Result<CanonicalObject, StoreError> {
    CanonicalObject::freeze(&NoteIntentFingerprint {
        project_id: &request.project_id,
        task_id: request.task_id,
        work_id: request.work_id,
        prose: &request.prose,
        visibility: request.visibility,
        kind: request.kind,
        authority: request.authority,
        sensitivity: request.sensitivity,
        title: request.title.as_deref(),
        tags: &request.tags,
        evidence: &request.evidence,
        refs: &request.refs,
        actor: &request.actor,
    })
}

fn note_intent_key(request: &NoteRequest) -> Result<String, StoreError> {
    Ok(CanonicalObject::freeze(&NoteIntentKey {
        project_id: &request.project_id,
        actor_id: &request.actor.actor_id,
        session_id: request.actor.session_id.as_ref(),
        caller_key: &request.idempotency_key,
    })?
    .key()
    .as_str()
    .to_owned())
}

fn inspect_generic_memory_actor_context<R: Redactor>(
    actor: &ActorContext,
    redactor: &R,
) -> Result<(), StoreError> {
    actor.validate_attribution_context().map_err(|detail| {
        StoreError::InvalidMemoryProjection(format!("invalid actor context: {detail}"))
    })?;
    for link in actor.provenance_chain.iter().filter(|link| {
        matches!(
            link.reference.as_deref(),
            Some(
                crate::domain::ACTOR_CONTEXT_PROVENANCE_REFERENCE
                    | crate::domain::ACTOR_CONTEXT_NORMALIZED_REFERENCE
            )
        )
    }) {
        redactor
            .inspect(&link.source)
            .map_err(StoreError::RedactionRefused)?;
        if let Some(reference) = link.reference.as_deref() {
            redactor
                .inspect(reference)
                .map_err(StoreError::RedactionRefused)?;
        }
    }
    Ok(())
}

fn prepare_note(request: &NoteRequest) -> Result<PreparedNote, StoreError> {
    let classification = classify_note(
        &request.prose,
        request.title.as_deref(),
        request.kind,
        request.authority,
        request.visibility,
    );
    if request.task_id.is_some() && request.work_id.is_some() {
        return Err(StoreError::InvalidMemoryProjection(
            "one note cannot belong to both task and local-work scope".into(),
        ));
    }
    let scope = match request.visibility {
        NoteVisibility::Shared => match (request.task_id, request.work_id) {
            (Some(task), None) => Scope::Task {
                project: request.project_id.clone(),
                task,
            },
            (None, Some(work)) => Scope::Work {
                project: request.project_id.clone(),
                work,
            },
            (None, None) => Scope::Project {
                project: request.project_id.clone(),
            },
            (Some(_), Some(_)) => unreachable!("validated above"),
        },
        NoteVisibility::Private => Scope::Agent {
            project: request.project_id.clone(),
            task: request.task_id,
            work: request.work_id,
            agent: request.actor.actor_id.clone(),
        },
    };
    let (status, policy_reason) = activation_policy(&scope, classification.kind);
    let memory_id = MemoryId::new();
    let version = MemoryVersion {
        schema_version: SCHEMA_VERSION,
        memory_id,
        project_key: None,
        parents: Vec::new(),
        kind: classification.kind,
        authority: classification.authority,
        delivery: classification.delivery,
        scope,
        title: classification.title,
        body: classification.body,
        structured_value: None,
        tags: request.tags.clone(),
        evidence: request.evidence.clone(),
        refs: request.refs.clone(),
        source_snapshot: None,
        confidence: None,
        sensitivity: request.sensitivity.unwrap_or(Sensitivity::Internal),
        classification_reason: classification.classification_reason,
        delivery_override_reason: classification.delivery_override_reason,
        valid_from: None,
        valid_until: None,
        review_by: None,
        last_verified: None,
        actor: request.actor.clone(),
        created_at: request.created_at,
    };
    let version_object = CanonicalObject::mint(&version)?;
    let assertion = MemoryAssertionEvent {
        schema_version: SCHEMA_VERSION,
        memory_id,
        version: version_object.key().clone(),
        status,
        policy_reason,
        actor: request.actor.clone(),
        created_at: request.created_at,
    };
    let assertion_object = CanonicalObject::mint(&assertion)?;
    Ok(PreparedNote {
        version,
        assertion,
        version_object,
        assertion_object,
    })
}

#[allow(
    clippy::too_many_lines,
    reason = "context selection, omission accounting, and both byte budgets stay contiguous so the fail-closed packet contract is auditable"
)]
fn assemble_context(mut memories: Vec<MemorySummary>) -> Result<ContextAssembly, StoreError> {
    memories.sort_by(|left, right| {
        left.title
            .cmp(&right.title)
            .then_with(|| left.version.cmp(&right.version))
    });
    let proposed_count = usize_to_u32(
        memories
            .iter()
            .filter(|memory| memory.status == MemoryStatus::Proposed)
            .count(),
    );
    let stale_count = usize_to_u32(
        memories
            .iter()
            .filter(|memory| memory.status == MemoryStatus::Stale)
            .count(),
    );
    let mut assembly = ContextAssembly {
        pinned: Vec::new(),
        index: Vec::new(),
        omissions: Vec::new(),
        omission_summaries: Vec::new(),
        proposed_count,
        stale_count,
    };
    let mut pinned_bytes = 0;
    let mut index_bytes = 0;
    for memory in memories {
        if !matches!(memory.status, MemoryStatus::Active | MemoryStatus::Stale) {
            continue;
        }
        if memory.sensitivity == Sensitivity::Restricted {
            record_context_omission(
                &mut assembly,
                ContextOmission {
                    memory_id: memory.memory_id,
                    version: memory.version,
                    reason: "restricted sensitivity requires an unavailable authorization".into(),
                },
            );
            continue;
        }
        let reason = retrieval_reason(&memory.scope, memory.delivery);
        match memory.delivery {
            Delivery::Pinned => {
                pinned_bytes += memory.title.len() + memory.body.len() + 2;
                assembly.pinned.push(ContextItem {
                    memory_id: memory.memory_id,
                    version: memory.version,
                    kind: memory.kind,
                    authority: memory.authority,
                    status: memory.status,
                    title: memory.title,
                    body: Some(memory.body),
                    retrieval_reason: reason,
                });
            }
            Delivery::Index if index_bytes + memory.title.len() + 96 <= INDEX_CONTEXT_BUDGET => {
                index_bytes += memory.title.len() + 96;
                assembly.index.push(ContextItem {
                    memory_id: memory.memory_id,
                    version: memory.version,
                    kind: memory.kind,
                    authority: memory.authority,
                    status: memory.status,
                    title: memory.title,
                    body: None,
                    retrieval_reason: reason,
                });
            }
            Delivery::Index => record_context_omission(
                &mut assembly,
                ContextOmission {
                    memory_id: memory.memory_id,
                    version: memory.version,
                    reason: "index byte budget exhausted".into(),
                },
            ),
            Delivery::OnDemand => record_context_omission(
                &mut assembly,
                ContextOmission {
                    memory_id: memory.memory_id,
                    version: memory.version,
                    reason: "on-demand memory is available through search".into(),
                },
            ),
            Delivery::Suppressed => record_context_omission(
                &mut assembly,
                ContextOmission {
                    memory_id: memory.memory_id,
                    version: memory.version,
                    reason: "delivery is suppressed by attributed policy".into(),
                },
            ),
        }
    }
    if pinned_bytes > PINNED_CONTEXT_BUDGET {
        return Err(StoreError::PinnedBudgetExceeded {
            required: pinned_bytes,
            budget: PINNED_CONTEXT_BUDGET,
        });
    }
    Ok(assembly)
}

fn record_context_omission(assembly: &mut ContextAssembly, omission: ContextOmission) {
    if assembly.omissions.len() < MAX_EXACT_CONTEXT_OMISSIONS {
        assembly.omissions.push(omission);
        return;
    }
    if let Some(summary) = assembly
        .omission_summaries
        .iter_mut()
        .find(|summary| summary.reason == omission.reason)
    {
        summary.count = summary.count.saturating_add(1);
    } else {
        assembly.omission_summaries.push(ContextOmissionSummary {
            reason: omission.reason,
            count: 1,
        });
    }
}

pub(super) fn fts_query(query: &str) -> String {
    let tokens: Vec<_> = fts_tokens(query)
        .map(|token| format!("\"{token}\"*"))
        .collect();
    if tokens.is_empty() {
        "\"__engram_no_match__\"".into()
    } else {
        tokens.join(" AND ")
    }
}

fn fts_tokens(query: &str) -> impl Iterator<Item = &str> {
    query
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|token| !token.is_empty())
}

pub(super) fn normalize_project_memory_query(
    query: Option<&str>,
) -> Result<Option<&str>, StoreError> {
    let Some(raw) = query else {
        return Ok(None);
    };
    if raw.len() > MAX_PROJECT_MEMORY_QUERY_BYTES {
        return Err(StoreError::InvalidProjectMemory(format!(
            "memory query exceeds {MAX_PROJECT_MEMORY_QUERY_BYTES} UTF-8 bytes"
        )));
    }
    let query = raw.trim();
    if query.is_empty() {
        return Ok(None);
    }
    if fts_tokens(query)
        .take(MAX_PROJECT_MEMORY_QUERY_TOKENS + 1)
        .count()
        > MAX_PROJECT_MEMORY_QUERY_TOKENS
    {
        return Err(StoreError::InvalidProjectMemory(format!(
            "memory query exceeds {MAX_PROJECT_MEMORY_QUERY_TOKENS} search tokens"
        )));
    }
    Ok(Some(query))
}

fn usize_to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn retrieval_reason(scope: &Scope, delivery: Delivery) -> String {
    let scope_reason = match scope {
        Scope::Project { .. } => "applicable project memory",
        Scope::Task { .. } => "shared memory for the active task",
        Scope::Work { .. } => "shared memory for focused local work",
        Scope::Agent { .. } => "private memory owned by this agent",
    };
    let delivery_reason = match delivery {
        Delivery::Pinned => "pinned by classification policy",
        Delivery::Index => "selected for the bounded title index",
        Delivery::OnDemand => "available on demand",
        Delivery::Suppressed => "suppressed",
    };
    format!("{scope_reason}; {delivery_reason}")
}
