//! Construction and inspection of historical memory rows for regression tests.
//!
//! These helpers preserve the retired capture format; they are not product APIs.

use super::{SqliteStore, StoreError, fts_query};
use crate::{
    canonical::CanonicalObject,
    domain::{
        ActorContext, MemoryAssertionEvent, MemoryId, MemoryRecord, MemorySummary, MemoryVersion,
        NoteReceipt, NoteRequest, NoteVisibility, SCHEMA_VERSION, Scope, Sensitivity, SessionId,
        TaskId,
    },
    memory::{
        Redactor,
        historical_fixtures::{activation_policy, classify_note},
    },
    storage::{MemoryProjectionMode, ObjectId, work},
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};
use serde::Serialize;

#[derive(Serialize)]
struct NoteIntentFingerprint<'a> {
    project_id: &'a crate::domain::ProjectId,
    task_id: Option<TaskId>,
    work_id: Option<crate::domain::WorkId>,
    prose: &'a str,
    visibility: NoteVisibility,
    kind: Option<crate::domain::MemoryKind>,
    authority: Option<crate::domain::Authority>,
    sensitivity: Option<Sensitivity>,
    title: Option<&'a str>,
    tags: &'a [String],
    evidence: &'a [ObjectId],
    refs: &'a [String],
    actor: &'a ActorContext,
}

#[derive(Serialize)]
struct NoteIntentKey<'a> {
    project_id: &'a crate::domain::ProjectId,
    actor_id: &'a str,
    session_id: Option<&'a SessionId>,
    caller_key: &'a str,
}

struct PreparedNote {
    version: MemoryVersion,
    assertion: MemoryAssertionEvent,
    version_object: CanonicalObject,
    assertion_object: CanonicalObject,
}

impl SqliteStore {
    /// Inserts a historical capture fixture with its original transaction semantics.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when inspection refuses the prose, an
    /// idempotency key changes meaning, or persistence fails.
    #[allow(
        clippy::too_many_lines,
        reason = "note objects, memory projections, claim renewal, work feeds, and the replay receipt remain one atomic transaction"
    )]
    pub(crate) fn insert_historical_memory_fixture<R: Redactor>(
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
        let work_positions = if prepared.version.scope.is_work_shared() {
            let work_id = prepared.version.scope.work_id().ok_or_else(|| {
                StoreError::InvalidMemoryProjection("shared work scope has no work id".into())
            })?;
            let holder = request.actor.session_id.as_ref().ok_or_else(|| {
                StoreError::InvalidMemoryProjection(
                    "work-scoped memory requires an attributed session".into(),
                )
            })?;
            work::append_fixture_memory_to_work_feeds(
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

    /// Inspects historical projections under their original visibility boundaries.
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
    pub(crate) fn inspect_historical_memory_projections(
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
        let memories = Self::inspect_historical_memory_projections_on(
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
}

impl SqliteStore {
    #[cfg(test)]
    #[allow(
        clippy::too_many_arguments,
        reason = "task-memory search binds project, task, work focus, work root and actor"
    )]
    fn inspect_historical_memory_projections_on(
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
            // A query with no searchable fragment finds nothing.
            let Some(fts_query) = fts_query(query)? else {
                return Ok(Vec::new());
            };
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

    /// Shows a complete memory record only after checking its project, task,
    /// participant, private owner, and sensitivity boundaries.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::MemoryAccessDenied`] rather than exposing content
    /// when a valid hash crosses a scope boundary.
    #[cfg(test)]
    pub(crate) fn inspect_historical_memory_record(
        &self,
        version_id: &ObjectId,
        project_id: &crate::domain::ProjectId,
        task_id: Option<TaskId>,
        work_id: Option<crate::domain::WorkId>,
        session_id: &SessionId,
        agent_id: &str,
    ) -> Result<MemoryRecord, StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        let record = Self::inspect_historical_memory_record_on(
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
    fn inspect_historical_memory_record_on(
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
        retiring_target: None,
        retiring_target_cleared: false,
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
