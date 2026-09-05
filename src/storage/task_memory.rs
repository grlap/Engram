use super::{
    ActorContext, ApplicableContradiction, AuthorizedContradiction, CanonicalObject, ChangeCursor,
    Connection, ContextAssembly, ContextItem, ContextOmission, ContextOmissionSummary,
    ContextPacket, ContextPacketHeader, ContextPacketPayload, ContradictionIntentFingerprint,
    DateTime, Delivery, HashMap, INDEX_CONTEXT_BUDGET, MAX_EXACT_CONTEXT_OMISSIONS,
    MAX_PROJECT_MEMORY_QUERY_BYTES, MAX_PROJECT_MEMORY_QUERY_TOKENS, MemoryAssertionEvent,
    MemoryContradictionEvent, MemoryContradictionReceipt, MemoryId, MemoryProjectionMode,
    MemoryRecord, MemoryStatus, MemorySummary, MemoryVersion, NoteIntentFingerprint, NoteIntentKey,
    NoteReceipt, NoteRequest, NoteVisibility, ObjectHash, OptionalExtension, PINNED_CONTEXT_BUDGET,
    PreparedNote, Redactor, SCHEMA_VERSION, Scope, Sensitivity, SessionId, SqliteStore, StoreError,
    TaskId, Transaction, TransactionBehavior, Utc, activation_policy, classify_note, params, work,
};

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
            if stored_request != request_object.hash().as_str() {
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
            prepared.version_object.hash(),
            prepared.assertion_object.hash(),
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
            version: prepared.version_object.hash().clone(),
            assertion: prepared.assertion_object.hash().clone(),
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
                request_object.hash().as_str(),
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

    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "authorization must bind the exact project, task, session, and actor view"
    )]
    fn authorize_contradiction_pair_on(
        connection: &Connection,
        project_id: &crate::domain::ProjectId,
        task_id: Option<TaskId>,
        work_id: Option<crate::domain::WorkId>,
        session_id: &SessionId,
        agent_id: &str,
        first_version: &ObjectHash,
        second_version: &ObjectHash,
        reason: &str,
    ) -> Result<AuthorizedContradiction, StoreError> {
        if let Some(task_id) = task_id {
            Self::ensure_active_task_on(connection, project_id, task_id, session_id)?;
        }
        let (focused_work_id, focused_root_id) =
            Self::focused_work_for_session_on(connection, project_id, session_id)?;
        if work_id.is_some() && work_id != focused_work_id {
            return Err(StoreError::InvalidContradiction(
                "work contradiction must match the session's persisted focus".into(),
            ));
        }
        // A caller that omits the work anchor still contradicts from its
        // validated focus: the anchor is the focused item, never a guess.
        let caller_work_id = work_id;
        let work_id = work_id.or(focused_work_id);
        if first_version == second_version {
            return Err(StoreError::InvalidContradiction(
                "a version cannot contradict itself".into(),
            ));
        }
        let reason = reason.trim();
        if reason.is_empty() {
            return Err(StoreError::InvalidContradiction(
                "an attributed reason is required".into(),
            ));
        }
        let first = Self::show_memory_on(
            connection,
            first_version,
            project_id,
            task_id,
            work_id,
            session_id,
            agent_id,
        )?;
        let second = Self::show_memory_on(
            connection,
            second_version,
            project_id,
            task_id,
            work_id,
            session_id,
            agent_id,
        )?;
        if matches!(first.version.scope, Scope::Agent { .. })
            || matches!(second.version.scope, Scope::Agent { .. })
        {
            return Err(StoreError::InvalidContradiction(
                "private memories cannot enter a shared contradiction edge".into(),
            ));
        }
        if first.version.project_key.is_some() || second.version.project_key.is_some() {
            return Err(StoreError::InvalidContradiction(
                "keyed project memories use the remember/forget lifecycle and cannot be contradiction endpoints"
                    .into(),
            ));
        }
        let scoped_task = |scope: &Scope| match scope {
            Scope::Task { task, .. } => Some(*task),
            Scope::Project { .. } | Scope::Work { .. } | Scope::Agent { .. } => None,
        };
        let first_task = scoped_task(&first.version.scope);
        let second_task = scoped_task(&second.version.scope);
        if first_task.is_some() && second_task.is_some() && first_task != second_task {
            return Err(StoreError::InvalidContradiction(
                "contradiction endpoints belong to different tasks".into(),
            ));
        }
        let task_anchor = first_task.or(second_task);

        let scoped_work_root =
            |scope: &Scope| -> Result<Option<crate::domain::WorkId>, StoreError> {
                match scope {
                    Scope::Work { work, .. } => {
                        work::verified_work_identity(connection, *work).map(|(_, root)| Some(root))
                    }
                    Scope::Project { .. } | Scope::Task { .. } | Scope::Agent { .. } => Ok(None),
                }
            };
        let first_root = scoped_work_root(&first.version.scope)?;
        let second_root = scoped_work_root(&second.version.scope)?;
        if first_root.is_some() && second_root.is_some() && first_root != second_root {
            return Err(StoreError::InvalidContradiction(
                "contradiction endpoints belong to different work roots".into(),
            ));
        }
        let work_root_anchor = first_root.or(second_root);
        let (task_anchor, work_root_anchor) = if task_anchor.is_none() && work_root_anchor.is_none()
        {
            if task_id.is_some() {
                (task_id, None)
            } else if focused_root_id.is_some() {
                (None, focused_root_id)
            } else {
                return Err(StoreError::InvalidContradiction(
                    "a contradiction requires an active task or work context".into(),
                ));
            }
        } else {
            (task_anchor, work_root_anchor)
        };
        if task_anchor.is_some() && task_anchor != task_id {
            return Err(StoreError::InvalidContradiction(
                "task-scoped contradiction does not match the active task".into(),
            ));
        }
        if work_root_anchor.is_some() && work_root_anchor != focused_root_id {
            return Err(StoreError::InvalidContradiction(
                "work-scoped contradiction does not match the focused work root".into(),
            ));
        }
        let (left, right) = if first_version < second_version {
            (first_version.clone(), second_version.clone())
        } else {
            (second_version.clone(), first_version.clone())
        };
        Ok(AuthorizedContradiction {
            left,
            right,
            reason: reason.into(),
            task_id: task_anchor,
            work_id: work_root_anchor.and(caller_work_id),
            feed_work_id: work_root_anchor.and(work_id),
            work_root_id: work_root_anchor,
        })
    }

    /// Declares an explicit contradiction between two visible, non-private
    /// memory versions. The immutable edge and both contested projections are
    /// committed with the applicable task and/or work-root feed events.
    ///
    /// Engram deliberately does not guess semantic conflicts from prose. An
    /// agent or human must name both versions and give an attributed reason.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when either memory is inaccessible, the pair is
    /// invalid or already linked, an idempotency key changes meaning, or the
    /// atomic write fails.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "the explicit authorization and idempotency inputs are part of the core boundary"
    )]
    pub fn record_memory_contradiction<R: Redactor>(
        &mut self,
        project_id: &crate::domain::ProjectId,
        task_id: Option<TaskId>,
        work_id: Option<crate::domain::WorkId>,
        session_id: &SessionId,
        agent_id: &str,
        first_version: &ObjectHash,
        second_version: &ObjectHash,
        reason: &str,
        idempotency_key: &str,
        actor: ActorContext,
        now: DateTime<Utc>,
        redactor: &R,
    ) -> Result<MemoryContradictionReceipt, StoreError> {
        inspect_generic_memory_actor_context(&actor, redactor)?;
        redactor
            .inspect(reason)
            .map_err(StoreError::RedactionRefused)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let authorized = Self::authorize_contradiction_pair_on(
            &transaction,
            project_id,
            task_id,
            work_id,
            session_id,
            agent_id,
            first_version,
            second_version,
            reason,
        )?;
        if authorized.feed_work_id.is_some() {
            work::require_work_schema_version(&transaction, self.work_schema_version)?;
        }
        let request = CanonicalObject::freeze(&ContradictionIntentFingerprint {
            project_id,
            task_id: authorized.task_id,
            work_id: authorized.work_id,
            work_root_id: authorized.work_root_id,
            left_version: &authorized.left,
            right_version: &authorized.right,
            reason: &authorized.reason,
            actor: &actor,
        })?;
        if let Some((stored_request, receipt_json)) = transaction
            .query_row(
                "SELECT request_hash, receipt_json FROM contradiction_intents
                 WHERE idempotency_key = ?1",
                [idempotency_key],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?
        {
            if stored_request != request.hash().as_str() {
                return Err(StoreError::ContradictionIdempotencyConflict(
                    idempotency_key.to_owned(),
                ));
            }
            let mut receipt: MemoryContradictionReceipt = serde_json::from_slice(&receipt_json)?;
            receipt.duplicate = true;
            return Ok(receipt);
        }
        let existing: Option<String> = transaction
            .query_row(
                "SELECT contradiction_hash FROM memory_contradiction_edges
                 WHERE left_version_hash = ?1 AND right_version_hash = ?2",
                params![authorized.left.as_str(), authorized.right.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            let hash = ObjectHash::from_stored(existing.clone())
                .ok_or(StoreError::InvalidStoredHash(existing))?;
            return Err(StoreError::ContradictionAlreadyRecorded(hash));
        }

        let event = MemoryContradictionEvent {
            schema_version: SCHEMA_VERSION,
            project_id: project_id.clone(),
            task_id: authorized.task_id,
            work_root_id: authorized.work_root_id,
            left_version: authorized.left.clone(),
            right_version: authorized.right.clone(),
            reason: authorized.reason,
            actor,
            created_at: now,
        };
        let object = CanonicalObject::freeze(&event)?;
        Self::insert_object(&transaction, "memory_contradiction_event", &object)?;
        transaction.execute(
            "INSERT INTO memory_contradiction_edges (
                 contradiction_hash, project_id, task_id, work_root_id,
                 left_version_hash, right_version_hash
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
            params![
                object.hash().as_str(),
                project_id.0,
                authorized.task_id.map(|task| task.0.to_string()),
                authorized.work_root_id.map(|work| work.0.to_string()),
                authorized.left.as_str(),
                authorized.right.as_str(),
            ],
        )?;
        if authorized.work_root_id.is_none()
            && let Some(task_id) = authorized.task_id
        {
            transaction.execute(
                "INSERT INTO memory_contradictions (
                     contradiction_hash, task_id, left_version_hash, right_version_hash
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![
                    object.hash().as_str(),
                    task_id.0.to_string(),
                    authorized.left.as_str(),
                    authorized.right.as_str(),
                ],
            )?;
        }
        transaction.execute(
            "UPDATE memory_heads SET status = 'contested'
             WHERE version_hash IN (?1, ?2) AND status IN ('active', 'stale')",
            params![authorized.left.as_str(), authorized.right.as_str()],
        )?;
        // A contradiction can change the globally visible status of a
        // project-scoped endpoint even when the edge itself is task/work
        // anchored. Fence every project context without publishing either
        // endpoint through an unrelated shared feed.
        Self::bump_project_context_revision_on(&transaction, project_id)?;
        let cursor = authorized
            .task_id
            .map(|task_id| {
                Self::insert_task_change(
                    &transaction,
                    task_id,
                    "memory_contradiction_event",
                    &object,
                )
            })
            .transpose()?;
        let work_positions = authorized.feed_work_id.map_or_else(
            || Ok(Vec::new()),
            |work_id| {
                work::append_context_object_to_work_feeds(
                    &transaction,
                    work_id,
                    "memory_contradiction_event",
                    &object,
                )
            },
        )?;
        let receipt = MemoryContradictionReceipt {
            idempotency_key: idempotency_key.into(),
            contradiction: object.hash().clone(),
            left_version: authorized.left,
            right_version: authorized.right,
            cursor,
            work_positions,
            duplicate: false,
        };
        transaction.execute(
            "INSERT INTO contradiction_intents (
                 idempotency_key, request_hash, receipt_json
             ) VALUES (?1, ?2, ?3)",
            params![
                idempotency_key,
                request.hash().as_str(),
                serde_json::to_vec(&receipt)?,
            ],
        )?;
        transaction.commit()?;
        Ok(receipt)
    }

    /// Returns memories visible to an agent, optionally narrowed by full-text
    /// query. Explicit search includes proposed records so review pressure is
    /// inspectable; context assembly applies its stricter status filter.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the derived index contains invalid data or
    /// SQLite cannot perform the query.
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
             h.status IN ('active', 'proposed', 'contested', 'stale') AND
             (h.scope_kind = 'agent' AND h.agent_id = ?3)";
        let root_visibility = "h.project_id = ?1 AND
             h.sensitivity != 'restricted' AND
             h.status IN ('active', 'proposed', 'contested', 'stale') AND
             h.scope_kind = 'work' AND h.work_id IN (
                 SELECT item.work_id FROM work_items item
                 WHERE item.project_id = ?1 AND item.root_id = ?4
             )";
        let visibility = format!("(({visibility}) OR ({root_visibility}))");
        let limit = limit.map_or(i64::MAX, |limit| i64::from(limit.clamp(1, 1_000)));
        let rows = if let Some(query) = query.filter(|value| !value.trim().is_empty()) {
            let fts_query = fts_query(query);
            let sql = format!(
                "SELECT h.memory_id, h.version_hash, h.status, h.memory_kind,
                        h.authority, h.delivery, h.scope_kind, h.project_id,
                        h.task_id, h.work_id, h.agent_id, h.title, h.body, h.sensitivity,
                        h.created_at_ms
                 FROM object_fts f JOIN memory_heads h
                   ON h.version_hash = f.object_hash
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
                "SELECT h.memory_id, h.version_hash, h.status, h.memory_kind,
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
             h.status IN ('active', 'proposed', 'contested', 'stale') AND
             NOT (h.scope_kind = 'project' AND EXISTS (
                 SELECT 1 FROM objects AS keyed
                 WHERE keyed.object_hash = h.version_hash
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
                "SELECT h.memory_id, h.version_hash, h.status, h.memory_kind,
                        h.authority, h.delivery, h.scope_kind, h.project_id,
                        h.task_id, h.work_id, h.agent_id, h.title, h.body, h.sensitivity,
                        h.created_at_ms
                 FROM object_fts f JOIN memory_heads h
                   ON h.version_hash = f.object_hash
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
                "SELECT h.memory_id, h.version_hash, h.status, h.memory_kind,
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
                "SELECT object_hash, canonical_json FROM objects
                 WHERE object_kind = 'memory_assertion_event'
                 ORDER BY created_at, object_hash",
            )?;
            let mapped = statement.query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?;
            mapped.collect::<Result<Vec<_>, _>>()?
        };
        let contradictions = {
            let mut statement = self.connection.prepare(
                "SELECT object_hash, canonical_json FROM objects
                 WHERE object_kind = 'memory_contradiction_event'
                 ORDER BY created_at, object_hash",
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
        transaction.execute("DELETE FROM memory_contradictions", [])?;
        transaction.execute("DELETE FROM memory_contradiction_edges", [])?;
        let mut activated = 0;
        for (stored_hash, bytes) in assertions {
            let assertion_hash = ObjectHash::from_stored(stored_hash.clone())
                .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
            let assertion_object = CanonicalObject::verify(&assertion_hash, bytes)?;
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
                     WHERE object_hash = ?1 AND object_kind = 'memory_version'",
                    [assertion.version.as_str()],
                    |row| row.get(0),
                )
                .optional()?;
            let Some(version_bytes) = version_bytes else {
                return Err(StoreError::InvalidMemoryProjection(format!(
                    "assertion {assertion_hash} references missing version {}",
                    assertion.version
                )));
            };
            let version_object = CanonicalObject::verify(&assertion.version, version_bytes)?;
            let version_value: serde_json::Value = serde_json::from_slice(version_object.bytes())?;
            if version_value
                .get("schema_version")
                .and_then(serde_json::Value::as_u64)
                != Some(u64::from(SCHEMA_VERSION))
            {
                continue;
            }
            let version: MemoryVersion = version_object.decode()?;
            Self::apply_memory_projection(
                &transaction,
                &assertion.version,
                &assertion_hash,
                &version,
                &assertion,
                MemoryProjectionMode::Replay,
            )?;
            activated += 1;
        }
        Self::rebuild_object_fts_from_heads_on(&transaction)?;
        Self::rebuild_project_memory_state_on(&transaction)?;
        Self::rebuild_contradiction_projection(&transaction, contradictions)?;
        Self::bump_rebuilt_context_revisions_on(&transaction)?;
        transaction.commit()?;
        Ok(activated)
    }

    #[cfg(test)]
    fn rebuild_contradiction_projection(
        transaction: &Transaction<'_>,
        contradictions: Vec<(String, Vec<u8>)>,
    ) -> Result<(), StoreError> {
        for (stored_hash, bytes) in contradictions {
            let contradiction_hash = ObjectHash::from_stored(stored_hash.clone())
                .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
            let object = CanonicalObject::verify(&contradiction_hash, bytes)?;
            let value: serde_json::Value = serde_json::from_slice(object.bytes())?;
            if value
                .get("schema_version")
                .and_then(serde_json::Value::as_u64)
                != Some(u64::from(SCHEMA_VERSION))
            {
                continue;
            }
            let edge: MemoryContradictionEvent = object.decode()?;
            transaction.execute(
                "INSERT INTO memory_contradiction_edges (
                     contradiction_hash, project_id, task_id, work_root_id,
                     left_version_hash, right_version_hash
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    contradiction_hash.as_str(),
                    edge.project_id.0,
                    edge.task_id.map(|task| task.0.to_string()),
                    edge.work_root_id.map(|work| work.0.to_string()),
                    edge.left_version.as_str(),
                    edge.right_version.as_str(),
                ],
            )?;
            if edge.work_root_id.is_none()
                && let Some(task_id) = edge.task_id
            {
                transaction.execute(
                    "INSERT INTO memory_contradictions (
                         contradiction_hash, task_id,
                         left_version_hash, right_version_hash
                     ) VALUES (?1, ?2, ?3, ?4)",
                    params![
                        contradiction_hash.as_str(),
                        task_id.0.to_string(),
                        edge.left_version.as_str(),
                        edge.right_version.as_str(),
                    ],
                )?;
            }
        }
        transaction.execute(
            "UPDATE memory_heads SET status = 'contested'
             WHERE status IN ('active', 'stale') AND version_hash IN (
                 SELECT left_version_hash FROM memory_contradiction_edges
                 UNION SELECT right_version_hash FROM memory_contradiction_edges
             )",
            [],
        )?;
        Ok(())
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
                 UNION SELECT DISTINCT project_id FROM memory_heads
                 UNION SELECT DISTINCT project_id FROM memory_contradiction_edges",
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
        let contradictions = Self::applicable_contradictions_on(
            transaction,
            project_id,
            task_id,
            work_root_id,
            &memories,
        )?;
        let assembly = assemble_context(memories, &contradictions)?;

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
        let object = CanonicalObject::freeze(&payload)?;
        Self::insert_object(transaction, "context_packet", &object)?;
        let packet = ContextPacket {
            header: ContextPacketHeader {
                project_id: project_id.clone(),
                task_id,
                work_id,
                work_feed_heads,
                project_context_revision,
                private_context_revision,
                packet_hash: object.hash().clone(),
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

    /// Explains a previously built packet only while its exact project, task,
    /// and focused-work context remains active for the requesting session.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] for unknown packets, integrity failures, or a
    /// current-context mismatch.
    pub fn explain_context(
        &self,
        packet_hash: &ObjectHash,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        agent_id: &str,
    ) -> Result<ContextPacketPayload, StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        let payload: ContextPacketPayload =
            Self::get_typed_object_on(&transaction, packet_hash, "context_packet")?
                .ok_or_else(|| StoreError::PacketAccessDenied(packet_hash.clone()))?;
        if payload.schema_version != SCHEMA_VERSION
            || payload.project_id != *project_id
            || payload.agent_id != agent_id
        {
            return Err(StoreError::PacketAccessDenied(packet_hash.clone()));
        }

        if let Some(task_id) = payload.task_id {
            match Self::ensure_active_task_on(&transaction, project_id, task_id, session_id) {
                Ok(()) => {}
                Err(StoreError::TaskAccessDenied { .. }) => {
                    return Err(StoreError::PacketAccessDenied(packet_hash.clone()));
                }
                Err(error) => return Err(error),
            }
        }

        if let Some(work_id) = payload.work_id {
            let (focused_work_id, _) =
                Self::focused_work_for_session_on(&transaction, project_id, session_id)?;
            if focused_work_id != Some(work_id) {
                return Err(StoreError::PacketAccessDenied(packet_hash.clone()));
            }
        }
        transaction.commit()?;
        Ok(payload)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "applicability verifies every projection anchor against the canonical edge"
    )]
    fn applicable_contradictions_on(
        connection: &Connection,
        project_id: &crate::domain::ProjectId,
        task_id: Option<TaskId>,
        work_root_id: Option<crate::domain::WorkId>,
        memories: &[MemorySummary],
    ) -> Result<Vec<ApplicableContradiction>, StoreError> {
        let visible: std::collections::HashSet<_> =
            memories.iter().map(|memory| &memory.version).collect();
        let mut statement = connection.prepare(
            "SELECT contradiction_hash, task_id, work_root_id,
                    left_version_hash, right_version_hash
             FROM memory_contradiction_edges
             WHERE project_id = ?1
               AND (task_id IS NULL OR task_id = ?2)
               AND (work_root_id IS NULL OR work_root_id = ?3)
             ORDER BY contradiction_hash",
        )?;
        let rows = statement.query_map(
            params![
                project_id.0,
                task_id.map(|task| task.0.to_string()),
                work_root_id.map(|work| work.0.to_string())
            ],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Option<String>>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            },
        )?;
        rows.filter_map(|row| match row {
            Ok((contradiction, stored_task, stored_root, left, right)) => {
                let parsed = (|| {
                    let contradiction = ObjectHash::from_stored(contradiction.clone())
                        .ok_or(StoreError::InvalidStoredHash(contradiction))?;
                    let left = ObjectHash::from_stored(left.clone())
                        .ok_or(StoreError::InvalidStoredHash(left))?;
                    let right = ObjectHash::from_stored(right.clone())
                        .ok_or(StoreError::InvalidStoredHash(right))?;
                    let stored_task = stored_task
                        .map(|task| {
                            uuid::Uuid::parse_str(&task).map(TaskId).map_err(|_| {
                                StoreError::InvalidMemoryProjection(format!(
                                    "contradiction edge has invalid task id {task}"
                                ))
                            })
                        })
                        .transpose()?;
                    let stored_root = stored_root
                        .map(|root| {
                            uuid::Uuid::parse_str(&root)
                                .map(crate::domain::WorkId)
                                .map_err(|_| {
                                    StoreError::InvalidMemoryProjection(format!(
                                        "contradiction edge has invalid work root id {root}"
                                    ))
                                })
                        })
                        .transpose()?;
                    let object = Self::get_canonical_object_on(
                        connection,
                        &contradiction,
                        "memory_contradiction_event",
                    )?
                    .ok_or_else(|| {
                        StoreError::InvalidMemoryProjection(format!(
                            "contradiction edge {contradiction} has no canonical object"
                        ))
                    })?;
                    let value: serde_json::Value = serde_json::from_slice(object.bytes())?;
                    if value
                        .get("schema_version")
                        .and_then(serde_json::Value::as_u64)
                        != Some(u64::from(SCHEMA_VERSION))
                    {
                        return Err(StoreError::InvalidMemoryProjection(format!(
                            "contradiction edge {contradiction} has an unsupported schema version"
                        )));
                    }
                    let event: MemoryContradictionEvent = object.decode()?;
                    if event.project_id != *project_id
                        || event.task_id != stored_task
                        || event.work_root_id != stored_root
                        || event.left_version != left
                        || event.right_version != right
                    {
                        return Err(StoreError::InvalidMemoryProjection(format!(
                            "contradiction edge {contradiction} differs from its canonical object"
                        )));
                    }
                    Ok(ApplicableContradiction {
                        contradiction,
                        left,
                        right,
                    })
                })();
                match parsed {
                    Ok(edge) if visible.contains(&edge.left) && visible.contains(&edge.right) => {
                        Some(Ok(edge))
                    }
                    Ok(_) => None,
                    Err(error) => Some(Err(error)),
                }
            }
            Err(error) => Some(Err(StoreError::Sqlite(error))),
        })
        .collect()
    }

    /// Shows a complete memory record only after checking its project, task,
    /// participant, private owner, and sensitivity boundaries.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::MemoryAccessDenied`] rather than exposing content
    /// when a valid hash crosses a scope boundary.
    pub fn show_memory(
        &self,
        version_hash: &ObjectHash,
        project_id: &crate::domain::ProjectId,
        task_id: Option<TaskId>,
        work_id: Option<crate::domain::WorkId>,
        session_id: &SessionId,
        agent_id: &str,
    ) -> Result<MemoryRecord, StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        let record = Self::show_memory_on(
            &transaction,
            version_hash,
            project_id,
            task_id,
            work_id,
            session_id,
            agent_id,
        )?;
        transaction.commit()?;
        Ok(record)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "authorization binds the exact persisted task/work session context"
    )]
    fn show_memory_on(
        connection: &Connection,
        version_hash: &ObjectHash,
        project_id: &crate::domain::ProjectId,
        task_id: Option<TaskId>,
        work_id: Option<crate::domain::WorkId>,
        session_id: &SessionId,
        agent_id: &str,
    ) -> Result<MemoryRecord, StoreError> {
        let (focused_work_id, focused_root_id) =
            Self::focused_work_for_session_on(connection, project_id, session_id)?;
        if work_id.is_some() && work_id != focused_work_id {
            return Err(StoreError::MemoryAccessDenied(version_hash.clone()));
        }
        let assertion_hash: Option<String> = connection
            .query_row(
                "SELECT assertion_hash FROM memory_heads WHERE version_hash = ?1",
                [version_hash.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        let Some(assertion_hash) = assertion_hash else {
            return Err(StoreError::MemoryNotFound(version_hash.clone()));
        };
        let assertion_hash = ObjectHash::from_stored(assertion_hash.clone())
            .ok_or(StoreError::InvalidStoredHash(assertion_hash))?;
        let version: MemoryVersion =
            Self::get_typed_object_on(connection, version_hash, "memory_version")?
                .ok_or_else(|| StoreError::MemoryNotFound(version_hash.clone()))?;
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
            return Err(StoreError::MemoryAccessDenied(version_hash.clone()));
        }
        let assertion: MemoryAssertionEvent =
            Self::get_typed_object_on(connection, &assertion_hash, "memory_assertion_event")?
                .ok_or_else(|| StoreError::MemoryNotFound(version_hash.clone()))?;
        Ok(MemoryRecord {
            version_hash: version_hash.clone(),
            assertion_hash,
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
    .hash()
    .as_str()
    .to_owned())
}

pub(super) fn claim_expiry(
    now: DateTime<Utc>,
    ttl_seconds: i64,
) -> Result<DateTime<Utc>, StoreError> {
    if !(1..=86_400).contains(&ttl_seconds) {
        return Err(StoreError::InvalidStoredClaim(
            "lease TTL must be from 1 through 86400 seconds".into(),
        ));
    }
    Ok(now + chrono::TimeDelta::seconds(ttl_seconds))
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
    let version_object = CanonicalObject::freeze(&version)?;
    let assertion = MemoryAssertionEvent {
        schema_version: SCHEMA_VERSION,
        memory_id,
        version: version_object.hash().clone(),
        status,
        policy_reason,
        actor: request.actor.clone(),
        created_at: request.created_at,
    };
    let assertion_object = CanonicalObject::freeze(&assertion)?;
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
fn assemble_context(
    mut memories: Vec<MemorySummary>,
    contradictions: &[ApplicableContradiction],
) -> Result<ContextAssembly, StoreError> {
    ensure_pinned_consistency(&memories, contradictions)?;
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
        if !matches!(
            memory.status,
            MemoryStatus::Active | MemoryStatus::Contested | MemoryStatus::Stale
        ) {
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
        let mut reason = retrieval_reason(&memory.scope, memory.delivery);
        if memory.status == MemoryStatus::Contested {
            reason.push_str("; unresolved contradiction is visible");
        }
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

fn ensure_pinned_consistency(
    memories: &[MemorySummary],
    contradictions: &[ApplicableContradiction],
) -> Result<(), StoreError> {
    let by_version: HashMap<_, _> = memories
        .iter()
        .map(|memory| (&memory.version, memory))
        .collect();
    for edge in contradictions {
        let Some(left) = by_version.get(&edge.left) else {
            continue;
        };
        let Some(right) = by_version.get(&edge.right) else {
            continue;
        };
        let unsafe_pinned = |memory: &MemorySummary| {
            matches!(
                memory.status,
                MemoryStatus::Active | MemoryStatus::Contested | MemoryStatus::Stale
            ) && memory.delivery == Delivery::Pinned
                && matches!(
                    memory.authority,
                    crate::domain::Authority::Hard | crate::domain::Authority::Firm
                )
                && memory.sensitivity != Sensitivity::Restricted
        };
        if unsafe_pinned(left) && unsafe_pinned(right) {
            return Err(StoreError::PinnedContradiction {
                contradiction: edge.contradiction.clone(),
                left: edge.left.clone(),
                right: edge.right.clone(),
            });
        }
    }
    Ok(())
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
