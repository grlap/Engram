use super::{
    ActorContext, CanonicalObject, ChangeCursor, Connection, DateTime, DeltaItem, DeserializeOwned,
    LocalTask, MAX_CONTROL_DELIVERY_EVENTS, MAX_CONTROL_DELIVERY_OBJECT_BYTES,
    MAX_TASK_CHANGE_OBJECT_BYTES, MemoryAssertionEvent, MemoryId, MemoryStatus, MemorySummary,
    MemorySummaryRow, ObjectHash, ObservedTurnDecision, OptionalExtension, ParticipantMembership,
    SCHEMA_VERSION, Scope, Serialize, SessionId, SqliteStore, StoreError, StoredControlObservation,
    TaskChange, TaskClaimEvent, TaskDelta, TaskId, TaskLease, TaskState, Transaction,
    TransactionBehavior, TurnEvaluationInput, TurnObservationIntentFingerprint, Utc, claim_expiry,
    lookup_project_memory_on, params, parse_enum,
};

#[cfg(test)]
mod tests;

impl SqliteStore {
    /// Returns the authorized ordered task feed after a cursor.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when task membership fails or a referenced
    /// canonical object is corrupt.
    pub fn task_delta(
        &self,
        project_id: &crate::domain::ProjectId,
        task_id: TaskId,
        session_id: &SessionId,
        agent_id: &str,
        after: ChangeCursor,
        limit: u32,
    ) -> Result<TaskDelta, StoreError> {
        Self::ensure_active_task_on(&self.connection, project_id, task_id, session_id)?;
        let visible = self.search_memories(
            project_id,
            Some(task_id),
            None,
            session_id,
            agent_id,
            None,
            1_000,
        )?;
        let changes = self.task_changes_since(task_id, after, limit)?;
        let mut items = Vec::with_capacity(changes.len());
        for change in changes {
            let object: serde_json::Value = self
                .get_typed_object(&change.object_hash, &change.object_kind)?
                .ok_or_else(|| {
                    StoreError::InvalidTaskProjection(format!(
                        "change {} references a missing object",
                        change.cursor.0
                    ))
                })?;
            let memory = if change.object_kind == "memory_assertion_event" {
                let assertion: MemoryAssertionEvent = serde_json::from_value(object.clone())?;
                visible
                    .iter()
                    .find(|candidate| candidate.version == assertion.version)
                    .cloned()
            } else {
                None
            };
            items.push(DeltaItem {
                cursor: change.cursor,
                object_kind: change.object_kind,
                object_hash: change.object_hash,
                memory,
                object,
            });
        }
        let cursor = items.last().map_or(after, |item| item.cursor);
        Ok(TaskDelta {
            task_id,
            after,
            cursor,
            changes: items,
        })
    }

    /// Evaluates and durably records one shadow-only turn decision.
    ///
    /// An exact retry for the same session and intent returns the originally
    /// observed bytes even after restart. Reusing the request key for a
    /// different intent is rejected. This operation never creates a grant and
    /// does not alter the advisory CLI/MCP path.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when canonicalization, persistence, or replay
    /// validation fails.
    pub fn record_turn_observation(
        &mut self,
        input: &TurnEvaluationInput,
    ) -> Result<ObservedTurnDecision, StoreError> {
        let intent = CanonicalObject::freeze(&TurnObservationIntentFingerprint {
            control_schema_version: input.control_schema_version,
            session_id: &input.session_id,
            task_id: input.task_id,
            intent: &input.intent,
        })?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let mut evaluated_input = input.clone();
        Self::hydrate_durable_turn_state(&transaction, &mut evaluated_input)?;
        let input_object = CanonicalObject::freeze(&evaluated_input)?;
        let existing = transaction
            .query_row(
                "SELECT intent_hash, sequence, session_id, task_id, idempotency_key,
                        observed_at_ms, input_hash, input_json, decision_hash, decision_json
                 FROM control_observations
                 WHERE session_id = ?1 AND idempotency_key = ?2",
                params![input.session_id.0, input.intent.idempotency_key],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        StoredControlObservation {
                            sequence: row.get(1)?,
                            session_id: row.get(2)?,
                            task_id: row.get(3)?,
                            idempotency_key: row.get(4)?,
                            intent_hash: row.get(0)?,
                            observed_at_ms: row.get(5)?,
                            input_hash: row.get(6)?,
                            input_json: row.get(7)?,
                            decision_hash: row.get(8)?,
                            decision_json: row.get(9)?,
                        },
                    ))
                },
            )
            .optional()?;

        if let Some((stored_intent_hash, stored)) = existing {
            if stored_intent_hash != intent.hash().as_str() {
                return Err(StoreError::TurnObservationIdempotencyConflict(
                    input.intent.idempotency_key.clone(),
                ));
            }
            let observation = Self::decode_control_observation(&stored)?;
            transaction.commit()?;
            return Ok(observation);
        }

        let observation = crate::control::observe_turn(&evaluated_input);
        let decision_object = CanonicalObject::freeze(&observation)?;
        transaction.execute(
            "INSERT INTO control_observations (
                 session_id, task_id, idempotency_key, intent_hash, input_hash,
                 input_json, decision_hash, decision_json, observed_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                evaluated_input.session_id.0,
                evaluated_input.task_id.map(|task_id| task_id.0.to_string()),
                evaluated_input.intent.idempotency_key,
                intent.hash().as_str(),
                input_object.hash().as_str(),
                input_object.bytes(),
                decision_object.hash().as_str(),
                decision_object.bytes(),
                evaluated_input.evaluated_at.timestamp_millis(),
            ],
        )?;
        transaction.commit()?;
        Ok(observation)
    }

    fn hydrate_durable_turn_state(
        transaction: &Transaction<'_>,
        input: &mut TurnEvaluationInput,
    ) -> Result<(), StoreError> {
        let Some(task_id) = input.task_id else {
            input.task_state = None;
            input.participant_membership = ParticipantMembership::NotMember;
            input.head_cursor = ChangeCursor::default();
            return Ok(());
        };
        let stored = transaction
            .query_row(
                "SELECT state,
                        (
                            SELECT COALESCE(MAX(task_cursor), 0) FROM task_changes
                            WHERE task_id = ?1
                        ),
                        EXISTS(
                            SELECT 1 FROM task_participants
                            WHERE task_id = ?1 AND session_id = ?2
                        ),
                        EXISTS(
                            SELECT 1 FROM session_bindings
                            WHERE task_id = ?1 AND session_id = ?2
                        )
                 FROM tasks WHERE task_id = ?1",
                params![task_id.0.to_string(), input.session_id.0],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, i64>(2)?,
                        row.get::<_, i64>(3)?,
                    ))
                },
            )
            .optional()?;

        if let Some((state, cursor, is_participant, is_bound)) = stored {
            input.task_state = Some(parse_enum::<TaskState>(&state)?);
            input.head_cursor = ChangeCursor(cursor);
            input.participant_membership = if is_participant == 1 && is_bound == 1 {
                ParticipantMembership::Member
            } else {
                ParticipantMembership::NotMember
            };
        } else {
            input.task_state = None;
            input.participant_membership = ParticipantMembership::NotMember;
            input.head_cursor = ChangeCursor::default();
        }
        Ok(())
    }

    /// Appends an immutable object. Re-appending identical content is
    /// idempotent; the same digest with different bytes is a hard error.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] on serialization, SQLite, or immutable-collision
    /// failure.
    pub fn append<T: Serialize>(
        &mut self,
        object_kind: &str,
        value: &T,
    ) -> Result<CanonicalObject, StoreError> {
        let object = CanonicalObject::freeze(value)?;
        let transaction = self.connection.transaction()?;
        Self::insert_object(&transaction, object_kind, &object)?;
        transaction.commit()?;
        Ok(object)
    }

    /// Appends an immutable task object and records its ordered peer-visible
    /// change in the same transaction. Replays return the original cursor.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when canonicalization or the atomic SQLite write
    /// fails.
    pub fn append_task_object<T: Serialize>(
        &mut self,
        task_id: TaskId,
        object_kind: &str,
        value: &T,
    ) -> Result<(CanonicalObject, ChangeCursor), StoreError> {
        let object = CanonicalObject::freeze(value)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::insert_object(&transaction, object_kind, &object)?;
        let cursor = Self::insert_task_change(&transaction, task_id, object_kind, &object)?;
        transaction.commit()?;
        Ok((object, cursor))
    }

    /// Returns ordered changes after the caller's last processed cursor.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when SQLite cannot read the feed or a stored
    /// object hash is invalid.
    pub fn task_changes_since(
        &self,
        task_id: TaskId,
        after: ChangeCursor,
        limit: u32,
    ) -> Result<Vec<TaskChange>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT task_cursor, object_kind, object_hash
             FROM task_changes
             WHERE task_id = ?1 AND task_cursor > ?2
             ORDER BY task_cursor
             LIMIT ?3",
        )?;
        let rows = statement.query_map(
            params![
                task_id.0.to_string(),
                after.0,
                i64::from(limit.clamp(1, 1_000))
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?;

        rows.map(|row| {
            let (cursor, object_kind, stored_hash) = row?;
            let object_hash = ObjectHash::from_stored(stored_hash.clone())
                .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
            Ok(TaskChange {
                cursor: ChangeCursor(cursor),
                task_id,
                object_kind,
                object_hash,
            })
        })
        .collect()
    }

    pub(super) fn task_delta_range_on(
        transaction: &Transaction<'_>,
        task_id: TaskId,
        after: ChangeCursor,
        through: ChangeCursor,
    ) -> Result<TaskDelta, StoreError> {
        if through < after {
            return Err(StoreError::InvalidTaskProjection(
                "task delivery range ends before its confirmed cursor".into(),
            ));
        }
        let raw = {
            let mut statement = transaction.prepare(
                "SELECT task_cursor, object_kind, object_hash
                 FROM task_changes
                 WHERE task_id = ?1 AND task_cursor > ?2 AND task_cursor <= ?3
                 ORDER BY task_cursor",
            )?;
            statement
                .query_map(params![task_id.0.to_string(), after.0, through.0], |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut changes = Vec::with_capacity(raw.len());
        for (cursor, object_kind, stored_hash) in raw {
            let object_hash = ObjectHash::from_stored(stored_hash.clone())
                .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
            let stored: Option<(String, Vec<u8>)> = transaction
                .query_row(
                    "SELECT object_kind, canonical_json FROM objects WHERE object_hash = ?1",
                    [object_hash.as_str()],
                    |row| Ok((row.get(0)?, row.get(1)?)),
                )
                .optional()?;
            let Some((stored_kind, bytes)) = stored else {
                return Err(StoreError::InvalidTaskProjection(format!(
                    "task change {cursor} references a missing object"
                )));
            };
            if stored_kind != object_kind {
                return Err(StoreError::ObjectKindMismatch {
                    hash: object_hash,
                    stored: stored_kind,
                    requested: object_kind,
                });
            }
            let object = CanonicalObject::verify(&object_hash, bytes)?.decode()?;
            changes.push(DeltaItem {
                cursor: ChangeCursor(cursor),
                object_kind: stored_kind,
                object_hash,
                memory: None,
                object,
            });
        }
        let distance = through.0.checked_sub(after.0).ok_or_else(|| {
            StoreError::InvalidTaskProjection("task delivery range overflowed".into())
        })?;
        let expected = usize::try_from(distance).map_err(|_| {
            StoreError::InvalidTaskProjection("task delivery range overflowed".into())
        })?;
        let dense = changes.iter().enumerate().all(|(offset, change)| {
            i64::try_from(offset).is_ok_and(|offset| {
                after
                    .0
                    .checked_add(offset)
                    .and_then(|cursor| cursor.checked_add(1))
                    .is_some_and(|cursor| change.cursor.0 == cursor)
            })
        });
        if changes.len() != expected || !dense {
            return Err(StoreError::InvalidTaskProjection(format!(
                "task delivery interval ({}, {}] is not dense",
                after.0, through.0
            )));
        }
        Ok(TaskDelta {
            task_id,
            after,
            cursor: through,
            changes,
        })
    }

    pub(super) fn task_delivery_page_end(
        transaction: &Transaction<'_>,
        task_id: TaskId,
        after: ChangeCursor,
    ) -> Result<ChangeCursor, StoreError> {
        let mut statement = transaction.prepare(
            "SELECT change.task_cursor, LENGTH(object.canonical_json)
             FROM task_changes change
             JOIN objects object ON object.object_hash = change.object_hash
             WHERE change.task_id = ?1 AND change.task_cursor > ?2
             ORDER BY change.task_cursor
             LIMIT ?3",
        )?;
        let rows = statement
            .query_map(
                params![task_id.0.to_string(), after.0, MAX_CONTROL_DELIVERY_EVENTS],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )?
            .collect::<Result<Vec<_>, _>>()?;
        let mut total_bytes = 0_i64;
        let mut through = None;
        for (cursor, bytes) in rows {
            let Some(next_total) = total_bytes.checked_add(bytes) else {
                break;
            };
            if next_total > MAX_CONTROL_DELIVERY_OBJECT_BYTES {
                break;
            }
            total_bytes = next_total;
            through = Some(ChangeCursor(cursor));
        }
        through.ok_or_else(|| {
            StoreError::InvalidTaskProjection(
                "one task event exceeds the bounded host-delivery object budget".into(),
            )
        })
    }

    /// Atomically acquires an execution lease. An exact idempotent retry
    /// returns its original lease; a live claim by another session conflicts.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] for an invalid lease interval, idempotency
    /// conflict, live competing claim, corrupt stored intent, or SQLite
    /// transaction failure.
    pub fn claim_task(
        &mut self,
        task_id: TaskId,
        holder: &SessionId,
        idempotency_key: &str,
        now: DateTime<Utc>,
        ttl_seconds: i64,
        actor: ActorContext,
    ) -> Result<TaskLease, StoreError> {
        let expires_at = claim_expiry(now, ttl_seconds)?;

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let prior_intent: Option<(String, String, Vec<u8>)> = transaction
            .query_row(
                "SELECT task_id, holder_session_id, lease_json
                 FROM task_claim_intents WHERE idempotency_key = ?1",
                [idempotency_key],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?;
        if let Some((stored_task, stored_holder, lease_json)) = prior_intent {
            let lease: TaskLease = serde_json::from_slice(&lease_json)?;
            if stored_task != task_id.0.to_string()
                || stored_holder != holder.0
                || (lease.ttl_seconds != 0 && lease.ttl_seconds != ttl_seconds)
            {
                return Err(StoreError::ClaimIdempotencyConflict(
                    idempotency_key.to_owned(),
                ));
            }
            return Ok(lease);
        }

        let current: Option<(String, String, i64, i64)> = transaction
            .query_row(
                "SELECT lease_id, holder_session_id, expires_at_ms, revision
                 FROM task_claims WHERE task_id = ?1",
                [task_id.0.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        if let Some((_, current_holder, current_expiry, _)) = &current
            && *current_expiry > now.timestamp_millis()
        {
            return Err(StoreError::TaskClaimHeld {
                holder: current_holder.clone(),
                expires_at: *current_expiry,
            });
        }

        let revision = current
            .as_ref()
            .map_or(1, |(_, _, _, revision)| revision + 1);
        let lease = TaskLease {
            task_id,
            lease_id: uuid::Uuid::now_v7().to_string(),
            holder: holder.clone(),
            idempotency_key: idempotency_key.to_owned(),
            ttl_seconds,
            expires_at,
            revision,
        };
        let previous_holder = current.map(|(_, holder, _, _)| SessionId(holder));

        transaction.execute(
            "INSERT INTO task_claims (
                 task_id, lease_id, holder_session_id, idempotency_key,
                 expires_at_ms, revision
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)
             ON CONFLICT(task_id) DO UPDATE SET
                 lease_id = excluded.lease_id,
                 holder_session_id = excluded.holder_session_id,
                 idempotency_key = excluded.idempotency_key,
                 expires_at_ms = excluded.expires_at_ms,
                 revision = excluded.revision",
            params![
                task_id.0.to_string(),
                lease.lease_id,
                holder.0,
                idempotency_key,
                expires_at.timestamp_millis(),
                revision,
            ],
        )?;
        transaction.execute(
            "INSERT INTO task_claim_intents (
                 idempotency_key, task_id, holder_session_id, lease_json
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                idempotency_key,
                task_id.0.to_string(),
                lease.holder.0,
                serde_json::to_vec(&lease)?,
            ],
        )?;

        let event = TaskClaimEvent {
            schema_version: SCHEMA_VERSION,
            lease: lease.clone(),
            previous_holder,
            actor,
            created_at: now,
        };
        let object = CanonicalObject::freeze(&event)?;
        Self::insert_object(&transaction, "task_claim_event", &object)?;
        Self::insert_task_change(&transaction, task_id, "task_claim_event", &object)?;
        transaction.commit()?;
        Ok(lease)
    }

    /// Loads and verifies an object before deserializing it.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when SQLite fails, stored bytes fail integrity
    /// verification, or the object cannot be decoded as `T`.
    pub fn get<T: DeserializeOwned>(&self, hash: &ObjectHash) -> Result<Option<T>, StoreError> {
        let bytes: Option<Vec<u8>> = self
            .connection
            .query_row(
                "SELECT canonical_json FROM objects WHERE object_hash = ?1",
                [hash.as_str()],
                |row| row.get(0),
            )
            .optional()?;

        bytes
            .map(|bytes| CanonicalObject::verify(hash, bytes)?.decode())
            .transpose()
    }

    pub(super) fn get_typed_object<T: DeserializeOwned>(
        &self,
        hash: &ObjectHash,
        object_kind: &str,
    ) -> Result<Option<T>, StoreError> {
        Self::get_typed_object_on(&self.connection, hash, object_kind)
    }

    pub(super) fn get_typed_object_on<T: DeserializeOwned>(
        connection: &Connection,
        hash: &ObjectHash,
        object_kind: &str,
    ) -> Result<Option<T>, StoreError> {
        Self::get_canonical_object_on(connection, hash, object_kind)?
            .map(|object| object.decode())
            .transpose()
    }

    pub(super) fn get_canonical_object_on(
        connection: &Connection,
        hash: &ObjectHash,
        object_kind: &str,
    ) -> Result<Option<CanonicalObject>, StoreError> {
        let stored: Option<(String, Vec<u8>)> = connection
            .query_row(
                "SELECT object_kind, canonical_json FROM objects WHERE object_hash = ?1",
                [hash.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((stored_kind, bytes)) = stored else {
            return Ok(None);
        };
        if stored_kind != object_kind {
            return Err(StoreError::ObjectKindMismatch {
                hash: hash.clone(),
                stored: stored_kind,
                requested: object_kind.into(),
            });
        }
        CanonicalObject::verify(hash, bytes).map(Some)
    }

    pub(super) fn load_task(
        transaction: &Transaction<'_>,
        task_id: TaskId,
    ) -> Result<LocalTask, StoreError> {
        let (project_id, title, external_ref, state, cursor, created_at_ms, updated_at_ms): (
            String,
            String,
            String,
            String,
            i64,
            i64,
            i64,
        ) = transaction.query_row(
            "SELECT project_id, title, external_ref, state, event_cursor,
                    created_at_ms, updated_at_ms
             FROM tasks WHERE task_id = ?1",
            [task_id.0.to_string()],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                    row.get(6)?,
                ))
            },
        )?;
        let mut statement = transaction.prepare(
            "SELECT session_id FROM task_participants
             WHERE task_id = ?1 ORDER BY joined_at_ms, session_id",
        )?;
        let participants = statement
            .query_map([task_id.0.to_string()], |row| {
                row.get::<_, String>(0).map(SessionId)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let created_at = DateTime::from_timestamp_millis(created_at_ms).ok_or_else(|| {
            StoreError::InvalidTaskProjection(format!(
                "invalid task created-at timestamp {created_at_ms}"
            ))
        })?;
        let updated_at = DateTime::from_timestamp_millis(updated_at_ms).ok_or_else(|| {
            StoreError::InvalidTaskProjection(format!(
                "invalid task updated-at timestamp {updated_at_ms}"
            ))
        })?;
        Ok(LocalTask {
            schema_version: SCHEMA_VERSION,
            project_id: crate::domain::ProjectId(project_id),
            task_id,
            title,
            external_ref: Some(external_ref),
            participants,
            state: parse_enum::<TaskState>(&state)?,
            event_cursor: ChangeCursor(cursor),
            created_at,
            updated_at,
        })
    }

    pub(super) fn decode_memory_summary(
        row: &rusqlite::Row<'_>,
    ) -> rusqlite::Result<MemorySummaryRow> {
        Ok((
            row.get(0)?,
            row.get(1)?,
            row.get(2)?,
            row.get(3)?,
            row.get(4)?,
            row.get(5)?,
            row.get(6)?,
            row.get(7)?,
            row.get(8)?,
            row.get(9)?,
            row.get(10)?,
            row.get(11)?,
            row.get(12)?,
            row.get(13)?,
            row.get(14)?,
        ))
    }

    pub(super) fn parse_memory_summary(row: MemorySummaryRow) -> Result<MemorySummary, StoreError> {
        let (
            memory_id,
            version,
            status,
            kind,
            authority,
            delivery,
            scope_kind,
            project_id,
            task_id,
            work_id,
            agent_id,
            title,
            body,
            sensitivity,
            created_at_ms,
        ) = row;
        let memory_id = uuid::Uuid::parse_str(&memory_id)
            .map(MemoryId)
            .map_err(|error| StoreError::InvalidMemoryProjection(error.to_string()))?;
        let version = ObjectHash::from_stored(version.clone())
            .ok_or(StoreError::InvalidStoredHash(version))?;
        let project = crate::domain::ProjectId(project_id);
        let task = task_id
            .map(|value| {
                uuid::Uuid::parse_str(&value)
                    .map(TaskId)
                    .map_err(|error| StoreError::InvalidMemoryProjection(error.to_string()))
            })
            .transpose()?;
        let work = work_id
            .map(|value| {
                uuid::Uuid::parse_str(&value)
                    .map(crate::domain::WorkId)
                    .map_err(|error| StoreError::InvalidMemoryProjection(error.to_string()))
            })
            .transpose()?;
        let scope = match scope_kind.as_str() {
            "project" => Scope::Project { project },
            "task" => Scope::Task {
                project,
                task: task.ok_or_else(|| {
                    StoreError::InvalidMemoryProjection("task scope has no task id".into())
                })?,
            },
            "work" => Scope::Work {
                project,
                work: work.ok_or_else(|| {
                    StoreError::InvalidMemoryProjection("work scope has no work id".into())
                })?,
            },
            "agent" => Scope::Agent {
                project,
                task,
                work,
                agent: agent_id.ok_or_else(|| {
                    StoreError::InvalidMemoryProjection("agent scope has no agent id".into())
                })?,
            },
            other => {
                return Err(StoreError::InvalidMemoryProjection(format!(
                    "unknown scope kind {other:?}"
                )));
            }
        };
        let created_at = DateTime::from_timestamp_millis(created_at_ms).ok_or_else(|| {
            StoreError::InvalidMemoryProjection(format!(
                "invalid created-at timestamp {created_at_ms}"
            ))
        })?;

        Ok(MemorySummary {
            memory_id,
            version,
            status: parse_enum(&status)?,
            kind: parse_enum(&kind)?,
            authority: parse_enum(&authority)?,
            delivery: parse_enum(&delivery)?,
            scope,
            title,
            body,
            sensitivity: parse_enum(&sensitivity)?,
            created_at,
        })
    }

    pub(super) fn insert_object(
        connection: &Connection,
        object_kind: &str,
        object: &CanonicalObject,
    ) -> Result<(), StoreError> {
        let existing: Option<(String, Vec<u8>)> = connection
            .query_row(
                "SELECT object_kind, canonical_json FROM objects WHERE object_hash = ?1",
                [object.hash().as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        match existing {
            Some((stored_kind, bytes)) if bytes == object.bytes() && stored_kind == object_kind => {
                Ok(())
            }
            Some((stored_kind, bytes)) if bytes == object.bytes() => {
                Err(StoreError::ObjectKindMismatch {
                    hash: object.hash().clone(),
                    stored: stored_kind,
                    requested: object_kind.to_owned(),
                })
            }
            Some(_) => Err(StoreError::ImmutableCollision(object.hash().clone())),
            None => {
                connection.execute(
                    "INSERT INTO objects (object_hash, object_kind, canonical_json)
                     VALUES (?1, ?2, ?3)",
                    params![object.hash().as_str(), object_kind, object.bytes()],
                )?;
                Ok(())
            }
        }
    }

    pub(super) fn insert_project_memory_version_object(
        connection: &Connection,
        object: &CanonicalObject,
        project_id: &crate::domain::ProjectId,
        key: &str,
    ) -> Result<(), StoreError> {
        match Self::insert_object(connection, "memory_version", object) {
            Ok(()) => Ok(()),
            Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(failure, _)))
                if failure.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_UNIQUE =>
            {
                match lookup_project_memory_on(connection, project_id, key)? {
                    Some(existing) => match existing.assertion.status {
                        MemoryStatus::Active => {
                            Err(StoreError::ProjectMemoryExists(key.to_owned()))
                        }
                        MemoryStatus::Tombstoned => {
                            Err(StoreError::ProjectMemoryRetired(key.to_owned()))
                        }
                        status => Err(StoreError::InvalidMemoryProjection(format!(
                            "project memory key has unsupported status {status:?}"
                        ))),
                    },
                    None => Err(StoreError::InvalidMemoryProjection(
                        "project memory uniqueness was violated without a canonical key binding"
                            .into(),
                    )),
                }
            }
            Err(other) => Err(other),
        }
    }

    pub(super) fn insert_task_change(
        transaction: &Transaction<'_>,
        task_id: TaskId,
        object_kind: &str,
        object: &CanonicalObject,
    ) -> Result<ChangeCursor, StoreError> {
        if object.bytes().len() > MAX_TASK_CHANGE_OBJECT_BYTES {
            return Err(StoreError::InvalidTaskProjection(format!(
                "task event requires {} bytes, exceeding the {}-byte object limit",
                object.bytes().len(),
                MAX_TASK_CHANGE_OBJECT_BYTES
            )));
        }
        let task_id_text = task_id.0.to_string();
        if let Some(cursor) = transaction
            .query_row(
                "SELECT task_cursor FROM task_changes
             WHERE task_id = ?1 AND object_hash = ?2",
                params![task_id_text, object.hash().as_str()],
                |row| row.get(0),
            )
            .optional()?
        {
            return Ok(ChangeCursor(cursor));
        }
        let current = transaction.query_row(
            "SELECT MAX(task_cursor) FROM task_changes WHERE task_id = ?1",
            [&task_id_text],
            |row| row.get::<_, Option<i64>>(0),
        )?;
        let cursor = current.unwrap_or(0).checked_add(1).ok_or_else(|| {
            StoreError::InvalidTaskProjection("task change cursor overflowed".into())
        })?;
        transaction.execute(
            "INSERT INTO task_changes (
                 task_id, task_cursor, object_kind, object_hash
             ) VALUES (?1, ?2, ?3, ?4)",
            params![task_id_text, cursor, object_kind, object.hash().as_str()],
        )?;
        Ok(ChangeCursor(cursor))
    }
}
