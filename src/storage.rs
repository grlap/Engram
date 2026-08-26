//! Local SQLite object store and integrity verification.

use std::{collections::HashMap, path::Path, time::Duration};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Serialize, de::DeserializeOwned};
use thiserror::Error;

use crate::{
    CanonicalObject, ObjectHash,
    domain::{
        ActorContext, ChangeCursor, ContextItem, ContextOmission, ContextPacket,
        ContextPacketHeader, ContextPacketPayload, Delivery, DeltaItem, LocalTask,
        MemoryAssertionEvent, MemoryContradictionEvent, MemoryContradictionReceipt, MemoryId,
        MemoryRecord, MemoryStatus, MemorySummary, MemoryVersion, NoteReceipt, NoteRequest,
        NoteVisibility, SCHEMA_VERSION, Scope, Sensitivity, SessionId, TaskBindReceipt,
        TaskClaimEvent, TaskDelta, TaskId, TaskJoinedEvent, TaskLease, TaskStartedEvent, TaskState,
    },
    memory::{Redactor, activation_policy, classify_note},
};

#[derive(Serialize)]
struct NoteIntentFingerprint<'a> {
    project_id: &'a crate::domain::ProjectId,
    task_id: Option<TaskId>,
    prose: &'a str,
    visibility: NoteVisibility,
    kind: Option<crate::domain::MemoryKind>,
    authority: Option<crate::domain::Authority>,
    sensitivity: Option<Sensitivity>,
    title: Option<&'a str>,
    tags: &'a [String],
    evidence: &'a [ObjectHash],
    refs: &'a [String],
    actor: &'a ActorContext,
}

#[derive(Serialize)]
struct ContradictionIntentFingerprint<'a> {
    project_id: &'a crate::domain::ProjectId,
    task_id: TaskId,
    left_version: &'a ObjectHash,
    right_version: &'a ObjectHash,
    reason: &'a str,
    actor: &'a ActorContext,
}

/// Errors at the immutable storage boundary.
#[derive(Debug, Error)]
pub enum StoreError {
    #[error("failed to parse JSON: {0}")]
    Json(#[from] serde_json::Error),
    #[error("SQLite operation failed: {0}")]
    Sqlite(#[from] rusqlite::Error),
    #[error("object {0} is not RFC 8785 canonical JSON")]
    NonCanonicalObject(ObjectHash),
    #[error("object hash mismatch: expected {expected}, got {actual}")]
    HashMismatch {
        expected: ObjectHash,
        actual: ObjectHash,
    },
    #[error("immutable object collision at {0}")]
    ImmutableCollision(ObjectHash),
    #[error("object {hash} is stored as kind {stored:?}, not {requested:?}")]
    ObjectKindMismatch {
        hash: ObjectHash,
        stored: String,
        requested: String,
    },
    #[error("stored object hash is invalid: {0}")]
    InvalidStoredHash(String),
    #[error("task is claimed by session {holder} until {expires_at}")]
    TaskClaimHeld { holder: String, expires_at: i64 },
    #[error("claim idempotency key {0:?} was reused for a different task, holder, or TTL")]
    ClaimIdempotencyConflict(String),
    #[error("contradiction idempotency key {0:?} was reused for different content")]
    ContradictionIdempotencyConflict(String),
    #[error("memory contradiction is invalid: {0}")]
    InvalidContradiction(String),
    #[error("these versions are already linked by contradiction object {0}")]
    ContradictionAlreadyRecorded(ObjectHash),
    #[error(
        "pinned context is unsafe: contradiction {contradiction} links applicable versions {left} and {right}"
    )]
    PinnedContradiction {
        contradiction: ObjectHash,
        left: ObjectHash,
        right: ObjectHash,
    },
    #[error("stored claim data is invalid: {0}")]
    InvalidStoredClaim(String),
    #[error("note idempotency key {0:?} was reused for different content")]
    NoteIdempotencyConflict(String),
    #[error("note prose must not be empty")]
    EmptyNote,
    #[error("pre-write redaction refused capture: {0}")]
    RedactionRefused(String),
    #[error("memory projection contains invalid data: {0}")]
    InvalidMemoryProjection(String),
    #[error("no local task is bound to external reference {0:?}")]
    TaskReferenceNotFound(String),
    #[error("external task reference and title must not be empty")]
    InvalidTaskBinding,
    #[error("task projection contains invalid data: {0}")]
    InvalidTaskProjection(String),
    #[error("session {0:?} has no active Engram task binding")]
    NoActiveTask(String),
    #[error("session {session:?} is not a participant of task {task:?}")]
    TaskAccessDenied { task: TaskId, session: String },
    #[error("memory {0} does not exist or its schema is not active")]
    MemoryNotFound(ObjectHash),
    #[error("caller is not authorized to read memory {0}")]
    MemoryAccessDenied(ObjectHash),
    #[error("caller is not authorized to explain context packet {0}")]
    PacketAccessDenied(ObjectHash),
    #[error("pinned context requires {required} bytes, exceeding the {budget}-byte budget")]
    PinnedBudgetExceeded { required: usize, budget: usize },
}

/// Result of scanning every immutable object in the store.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IntegrityReport {
    pub checked_objects: usize,
    pub invalid_objects: Vec<String>,
}

/// One ordered entry in a task's authoritative local change feed.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct TaskChange {
    pub cursor: ChangeCursor,
    pub task_id: TaskId,
    pub object_kind: String,
    pub object_hash: ObjectHash,
}

type MemorySummaryRow = (
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    String,
    Option<String>,
    Option<String>,
    String,
    String,
    String,
    i64,
);

struct PreparedNote {
    version: MemoryVersion,
    assertion: MemoryAssertionEvent,
    version_object: CanonicalObject,
    assertion_object: CanonicalObject,
}

const PINNED_CONTEXT_BUDGET: usize = 4 * 1_024;
const INDEX_CONTEXT_BUDGET: usize = 8 * 1_024;

struct ContextAssembly {
    pinned: Vec<ContextItem>,
    index: Vec<ContextItem>,
    omissions: Vec<ContextOmission>,
    proposed_count: u32,
    stale_count: u32,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ApplicableContradiction {
    contradiction: ObjectHash,
    left: ObjectHash,
    right: ObjectHash,
}

impl IntegrityReport {
    /// Whether every stored object passed canonicalization and digest checks.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.invalid_objects.is_empty()
    }
}

/// V1's canonical local persistence backend.
pub struct SqliteStore {
    connection: Connection,
}

impl SqliteStore {
    /// Opens or creates a local database and applies idempotent schema setup.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when SQLite cannot open or initialize the store.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        let connection = Connection::open(path)?;
        Self::from_connection(connection)
    }

    /// Creates an isolated store for tests or ephemeral runs.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when SQLite cannot initialize the schema.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::from_connection(Connection::open_in_memory()?)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the complete idempotent SQLite schema is kept together for auditability"
    )]
    fn from_connection(connection: Connection) -> Result<Self, StoreError> {
        connection.busy_timeout(Duration::from_secs(5))?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA journal_mode = WAL;
             PRAGMA synchronous = NORMAL;
             CREATE TABLE IF NOT EXISTS objects (
                 object_hash TEXT PRIMARY KEY,
                 object_kind TEXT NOT NULL,
                 canonical_json BLOB NOT NULL,
                 created_at TEXT NOT NULL DEFAULT CURRENT_TIMESTAMP
             ) STRICT;
             CREATE TABLE IF NOT EXISTS publication_intents (
                 idempotency_key TEXT PRIMARY KEY,
                 report_hash TEXT NOT NULL REFERENCES objects(object_hash),
                 external_ref TEXT,
                 state TEXT NOT NULL,
                 last_error TEXT,
                 attempt_count INTEGER NOT NULL DEFAULT 0,
                 receipt_json TEXT
             ) STRICT;
             CREATE VIRTUAL TABLE IF NOT EXISTS object_fts USING fts5(
                 object_hash UNINDEXED,
                 title,
                 body
             );
             CREATE TABLE IF NOT EXISTS memory_heads (
                 memory_id TEXT PRIMARY KEY,
                 version_hash TEXT NOT NULL REFERENCES objects(object_hash),
                 assertion_hash TEXT NOT NULL REFERENCES objects(object_hash),
                 schema_version INTEGER NOT NULL,
                 status TEXT NOT NULL,
                 scope_kind TEXT NOT NULL,
                 project_id TEXT NOT NULL,
                 task_id TEXT,
                 agent_id TEXT,
                 memory_kind TEXT NOT NULL,
                 authority TEXT NOT NULL,
                 delivery TEXT NOT NULL,
                 sensitivity TEXT NOT NULL,
                 title TEXT NOT NULL,
                 body TEXT NOT NULL,
                 created_at_ms INTEGER NOT NULL
             ) STRICT;
             CREATE INDEX IF NOT EXISTS memory_heads_scope
                 ON memory_heads(project_id, task_id, agent_id, status);
             CREATE TABLE IF NOT EXISTS note_intents (
                 idempotency_key TEXT PRIMARY KEY,
                 request_hash TEXT NOT NULL,
                 receipt_json BLOB NOT NULL
             ) STRICT;
             CREATE TABLE IF NOT EXISTS memory_contradictions (
                 contradiction_hash TEXT PRIMARY KEY REFERENCES objects(object_hash),
                 task_id TEXT NOT NULL REFERENCES tasks(task_id),
                 left_version_hash TEXT NOT NULL REFERENCES objects(object_hash),
                 right_version_hash TEXT NOT NULL REFERENCES objects(object_hash),
                 UNIQUE(left_version_hash, right_version_hash),
                 CHECK(left_version_hash < right_version_hash)
             ) STRICT;
             CREATE INDEX IF NOT EXISTS memory_contradictions_versions
                 ON memory_contradictions(left_version_hash, right_version_hash);
             CREATE TABLE IF NOT EXISTS contradiction_intents (
                 idempotency_key TEXT PRIMARY KEY,
                 request_hash TEXT NOT NULL,
                 receipt_json BLOB NOT NULL
             ) STRICT;
             CREATE TABLE IF NOT EXISTS tasks (
                 task_id TEXT PRIMARY KEY,
                 project_id TEXT NOT NULL,
                 external_ref TEXT NOT NULL,
                 title TEXT NOT NULL,
                 state TEXT NOT NULL,
                 event_cursor INTEGER NOT NULL DEFAULT 0,
                 created_at_ms INTEGER NOT NULL,
                 updated_at_ms INTEGER NOT NULL,
                 UNIQUE(project_id, external_ref)
             ) STRICT;
             CREATE TABLE IF NOT EXISTS task_participants (
                 task_id TEXT NOT NULL REFERENCES tasks(task_id),
                 session_id TEXT NOT NULL,
                 joined_at_ms INTEGER NOT NULL,
                 PRIMARY KEY(task_id, session_id)
             ) STRICT;
             CREATE TABLE IF NOT EXISTS session_bindings (
                 session_id TEXT PRIMARY KEY,
                 task_id TEXT NOT NULL REFERENCES tasks(task_id),
                 bound_at_ms INTEGER NOT NULL
             ) STRICT;
             CREATE TABLE IF NOT EXISTS task_changes (
                 cursor INTEGER PRIMARY KEY AUTOINCREMENT,
                 task_id TEXT NOT NULL,
                 object_kind TEXT NOT NULL,
                 object_hash TEXT NOT NULL REFERENCES objects(object_hash),
                 UNIQUE(task_id, object_hash)
             ) STRICT;
             CREATE INDEX IF NOT EXISTS task_changes_task_cursor
                 ON task_changes(task_id, cursor);
             CREATE TABLE IF NOT EXISTS task_claims (
                 task_id TEXT PRIMARY KEY,
                 lease_id TEXT NOT NULL UNIQUE,
                 holder_session_id TEXT NOT NULL,
                 idempotency_key TEXT NOT NULL,
                 expires_at_ms INTEGER NOT NULL,
                 revision INTEGER NOT NULL
             ) STRICT;
             CREATE TABLE IF NOT EXISTS task_claim_intents (
                 idempotency_key TEXT PRIMARY KEY,
                 task_id TEXT NOT NULL,
                 holder_session_id TEXT NOT NULL,
                 lease_json BLOB NOT NULL
             ) STRICT;",
        )?;
        Ok(Self { connection })
    }

    /// Starts a task or joins the existing task already bound to the same
    /// project and external reference. The reference is the public rendezvous
    /// key; callers never need to relay Engram's UUID out of band.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when required binding data is empty or the
    /// atomic task/event write fails.
    pub fn start_task(
        &mut self,
        project_id: &crate::domain::ProjectId,
        external_ref: &str,
        title: &str,
        participant: &SessionId,
        actor: ActorContext,
        now: DateTime<Utc>,
    ) -> Result<TaskBindReceipt, StoreError> {
        if external_ref.trim().is_empty() || title.trim().is_empty() {
            return Err(StoreError::InvalidTaskBinding);
        }
        self.bind_task(
            project_id,
            external_ref.trim(),
            Some(title.trim()),
            participant,
            actor,
            now,
        )
    }

    /// Joins an existing task using only its external reference.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::TaskReferenceNotFound`] when no matching task
    /// exists, or another storage error when joining cannot commit.
    pub fn join_task(
        &mut self,
        project_id: &crate::domain::ProjectId,
        external_ref: &str,
        participant: &SessionId,
        actor: ActorContext,
        now: DateTime<Utc>,
    ) -> Result<TaskBindReceipt, StoreError> {
        if external_ref.trim().is_empty() {
            return Err(StoreError::InvalidTaskBinding);
        }
        self.bind_task(
            project_id,
            external_ref.trim(),
            None,
            participant,
            actor,
            now,
        )
    }

    fn bind_task(
        &mut self,
        project_id: &crate::domain::ProjectId,
        external_ref: &str,
        create_title: Option<&str>,
        participant: &SessionId,
        actor: ActorContext,
        now: DateTime<Utc>,
    ) -> Result<TaskBindReceipt, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let existing_task: Option<String> = transaction
            .query_row(
                "SELECT task_id FROM tasks
                 WHERE project_id = ?1 AND external_ref = ?2",
                params![project_id.0, external_ref],
                |row| row.get(0),
            )
            .optional()?;

        let (task_id, joined, cursor) = if let Some(stored_task_id) = existing_task {
            let task_uuid = uuid::Uuid::parse_str(&stored_task_id)
                .map_err(|error| StoreError::InvalidTaskProjection(error.to_string()))?;
            let task_id = TaskId(task_uuid);
            let inserted = transaction.execute(
                "INSERT OR IGNORE INTO task_participants
                 (task_id, session_id, joined_at_ms) VALUES (?1, ?2, ?3)",
                params![stored_task_id, participant.0, now.timestamp_millis()],
            )?;
            let cursor = if inserted == 1 {
                let event = TaskJoinedEvent {
                    schema_version: SCHEMA_VERSION,
                    task_id,
                    participant: participant.clone(),
                    actor,
                    created_at: now,
                };
                let object = CanonicalObject::freeze(&event)?;
                Self::insert_object(&transaction, "task_joined_event", &object)?;
                Self::insert_task_change(&transaction, task_id, "task_joined_event", &object)?
            } else {
                Self::latest_task_cursor(&transaction, task_id)?
            };
            (task_id, inserted == 1, cursor)
        } else {
            let title = create_title
                .ok_or_else(|| StoreError::TaskReferenceNotFound(external_ref.to_owned()))?;
            let task_id = TaskId::new();
            transaction.execute(
                "INSERT INTO tasks (
                     task_id, project_id, external_ref, title, state,
                     event_cursor, created_at_ms, updated_at_ms
                 ) VALUES (?1, ?2, ?3, ?4, 'active', 0, ?5, ?5)",
                params![
                    task_id.0.to_string(),
                    project_id.0,
                    external_ref,
                    title,
                    now.timestamp_millis(),
                ],
            )?;
            transaction.execute(
                "INSERT INTO task_participants
                 (task_id, session_id, joined_at_ms) VALUES (?1, ?2, ?3)",
                params![task_id.0.to_string(), participant.0, now.timestamp_millis()],
            )?;
            let event = TaskStartedEvent {
                schema_version: SCHEMA_VERSION,
                task_id,
                project_id: project_id.clone(),
                title: title.into(),
                external_ref: external_ref.into(),
                participant: participant.clone(),
                actor,
                created_at: now,
            };
            let object = CanonicalObject::freeze(&event)?;
            Self::insert_object(&transaction, "task_started_event", &object)?;
            let cursor =
                Self::insert_task_change(&transaction, task_id, "task_started_event", &object)?;
            (task_id, true, cursor)
        };
        transaction.execute(
            "UPDATE tasks SET event_cursor = ?2, updated_at_ms = ?3
             WHERE task_id = ?1",
            params![task_id.0.to_string(), cursor.0, now.timestamp_millis()],
        )?;
        transaction.execute(
            "INSERT INTO session_bindings (session_id, task_id, bound_at_ms)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id) DO UPDATE SET
                 task_id = excluded.task_id,
                 bound_at_ms = excluded.bound_at_ms",
            params![participant.0, task_id.0.to_string(), now.timestamp_millis()],
        )?;
        let task = Self::load_task(&transaction, task_id)?;
        transaction.commit()?;
        Ok(TaskBindReceipt {
            task,
            joined,
            cursor,
        })
    }

    /// Resolves the task most recently bound by this session. Bindings are a
    /// durable local projection so restarting an MCP process does not require
    /// the agent to relay Engram's task UUID again.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NoActiveTask`] when the session has not started
    /// or joined a task in this project.
    pub fn bound_task(
        &self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
    ) -> Result<TaskId, StoreError> {
        let stored: Option<String> = self
            .connection
            .query_row(
                "SELECT b.task_id FROM session_bindings b JOIN tasks t
                   ON t.task_id = b.task_id
                 WHERE b.session_id = ?1 AND t.project_id = ?2",
                params![session_id.0, project_id.0],
                |row| row.get(0),
            )
            .optional()?;
        let stored = stored.ok_or_else(|| StoreError::NoActiveTask(session_id.0.clone()))?;
        uuid::Uuid::parse_str(&stored)
            .map(TaskId)
            .map_err(|error| StoreError::InvalidTaskProjection(error.to_string()))
    }

    /// Captures one attributed prose note through the configured pre-write
    /// inspection port. Classification, canonical objects, projections, peer
    /// feed entry, and idempotency receipt commit atomically.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when inspection refuses the prose, an
    /// idempotency key changes meaning, or persistence fails.
    pub fn capture_note<R: Redactor>(
        &mut self,
        request: &NoteRequest,
        redactor: &R,
    ) -> Result<NoteReceipt, StoreError> {
        if request.prose.trim().is_empty() {
            return Err(StoreError::EmptyNote);
        }
        redactor
            .inspect(&request.prose)
            .map_err(StoreError::RedactionRefused)?;

        let request_object = note_fingerprint(request)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some((stored_request, receipt_json)) = transaction
            .query_row(
                "SELECT request_hash, receipt_json FROM note_intents
                 WHERE idempotency_key = ?1",
                [&request.idempotency_key],
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
        )?;

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
            classification_reason: prepared.version.classification_reason.clone(),
            policy_reason: prepared.assertion.policy_reason.clone(),
            duplicate: false,
        };
        transaction.execute(
            "INSERT INTO note_intents (idempotency_key, request_hash, receipt_json)
             VALUES (?1, ?2, ?3)",
            params![
                request.idempotency_key,
                request_object.hash().as_str(),
                serde_json::to_vec(&receipt)?,
            ],
        )?;
        transaction.commit()?;
        Ok(receipt)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "authorization must bind the exact project, task, session, and actor view"
    )]
    fn authorize_contradiction_pair(
        &self,
        project_id: &crate::domain::ProjectId,
        task_id: TaskId,
        session_id: &SessionId,
        agent_id: &str,
        first_version: &ObjectHash,
        second_version: &ObjectHash,
        reason: &str,
    ) -> Result<(ObjectHash, ObjectHash, String), StoreError> {
        self.ensure_task_participant(project_id, task_id, session_id)?;
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
        let first = self.show_memory(
            first_version,
            project_id,
            Some(task_id),
            session_id,
            agent_id,
        )?;
        let second = self.show_memory(
            second_version,
            project_id,
            Some(task_id),
            session_id,
            agent_id,
        )?;
        if matches!(first.version.scope, Scope::Agent { .. })
            || matches!(second.version.scope, Scope::Agent { .. })
        {
            return Err(StoreError::InvalidContradiction(
                "private memories cannot enter a task-shared contradiction edge".into(),
            ));
        }
        let (left, right) = if first_version < second_version {
            (first_version.clone(), second_version.clone())
        } else {
            (second_version.clone(), first_version.clone())
        };
        Ok((left, right, reason.into()))
    }

    /// Declares an explicit contradiction between two visible, non-private
    /// memory versions. The immutable edge and both contested projections are
    /// committed with one ordered task-feed event.
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
        reason = "the explicit authorization and idempotency inputs are part of the core boundary"
    )]
    pub fn record_memory_contradiction(
        &mut self,
        project_id: &crate::domain::ProjectId,
        task_id: TaskId,
        session_id: &SessionId,
        agent_id: &str,
        first_version: &ObjectHash,
        second_version: &ObjectHash,
        reason: &str,
        idempotency_key: &str,
        actor: ActorContext,
        now: DateTime<Utc>,
    ) -> Result<MemoryContradictionReceipt, StoreError> {
        let (left_version, right_version, reason) = self.authorize_contradiction_pair(
            project_id,
            task_id,
            session_id,
            agent_id,
            first_version,
            second_version,
            reason,
        )?;
        let request = CanonicalObject::freeze(&ContradictionIntentFingerprint {
            project_id,
            task_id,
            left_version: &left_version,
            right_version: &right_version,
            reason: &reason,
            actor: &actor,
        })?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
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
                "SELECT contradiction_hash FROM memory_contradictions
                 WHERE left_version_hash = ?1 AND right_version_hash = ?2",
                params![left_version.as_str(), right_version.as_str()],
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
            task_id,
            left_version: left_version.clone(),
            right_version: right_version.clone(),
            reason,
            actor,
            created_at: now,
        };
        let object = CanonicalObject::freeze(&event)?;
        Self::insert_object(&transaction, "memory_contradiction_event", &object)?;
        transaction.execute(
            "INSERT INTO memory_contradictions (
                 contradiction_hash, task_id, left_version_hash, right_version_hash
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                object.hash().as_str(),
                task_id.0.to_string(),
                left_version.as_str(),
                right_version.as_str(),
            ],
        )?;
        transaction.execute(
            "UPDATE memory_heads SET status = 'contested'
             WHERE version_hash IN (?1, ?2) AND status IN ('active', 'stale')",
            params![left_version.as_str(), right_version.as_str()],
        )?;
        let cursor =
            Self::insert_task_change(&transaction, task_id, "memory_contradiction_event", &object)?;
        let receipt = MemoryContradictionReceipt {
            idempotency_key: idempotency_key.into(),
            contradiction: object.hash().clone(),
            left_version: left_version.clone(),
            right_version: right_version.clone(),
            cursor,
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
    pub fn search_memories(
        &self,
        project_id: &crate::domain::ProjectId,
        task_id: Option<TaskId>,
        agent_id: &str,
        query: Option<&str>,
        limit: u32,
    ) -> Result<Vec<MemorySummary>, StoreError> {
        let visibility = "h.project_id = ?1 AND (h.scope_kind = 'project' OR
             (h.scope_kind = 'task' AND h.task_id = ?2) OR
             (h.scope_kind = 'agent' AND h.agent_id = ?3 AND
              (h.task_id IS NULL OR h.task_id = ?2)))";
        let limit = i64::from(limit.clamp(1, 1_000));
        let rows = if let Some(query) = query.filter(|value| !value.trim().is_empty()) {
            let fts_query = fts_query(query);
            let sql = format!(
                "SELECT h.memory_id, h.version_hash, h.status, h.memory_kind,
                        h.authority, h.delivery, h.scope_kind, h.project_id,
                        h.task_id, h.agent_id, h.title, h.body, h.sensitivity,
                        h.created_at_ms
                 FROM object_fts f JOIN memory_heads h
                   ON h.version_hash = f.object_hash
                 WHERE {visibility} AND object_fts MATCH ?4
                 ORDER BY bm25(object_fts), h.created_at_ms DESC LIMIT ?5"
            );
            let mut statement = self.connection.prepare(&sql)?;
            let mapped = statement.query_map(
                params![
                    project_id.0,
                    task_id.map(|value| value.0.to_string()),
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
                        h.task_id, h.agent_id, h.title, h.body, h.sensitivity,
                        h.created_at_ms
                 FROM memory_heads h WHERE {visibility}
                 ORDER BY h.created_at_ms DESC, h.memory_id LIMIT ?4"
            );
            let mut statement = self.connection.prepare(&sql)?;
            let mapped = statement.query_map(
                params![
                    project_id.0,
                    task_id.map(|value| value.0.to_string()),
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
    pub fn rebuild_memory_index(&mut self) -> Result<usize, StoreError> {
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
            )?;
            activated += 1;
        }
        Self::rebuild_contradiction_projection(&transaction, contradictions)?;
        transaction.commit()?;
        Ok(activated)
    }

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
                "INSERT INTO memory_contradictions (
                     contradiction_hash, task_id, left_version_hash, right_version_hash
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![
                    contradiction_hash.as_str(),
                    edge.task_id.0.to_string(),
                    edge.left_version.as_str(),
                    edge.right_version.as_str(),
                ],
            )?;
        }
        transaction.execute(
            "UPDATE memory_heads SET status = 'contested'
             WHERE status IN ('active', 'stale') AND version_hash IN (
                 SELECT left_version_hash FROM memory_contradictions
                 UNION SELECT right_version_hash FROM memory_contradictions
             )",
            [],
        )?;
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
        if let Some(task_id) = task_id {
            self.ensure_task_participant(project_id, task_id, session_id)?;
        }
        let memories = self.search_memories(project_id, task_id, agent_id, None, 1_000)?;
        let contradictions = self.applicable_contradictions(task_id, &memories)?;
        let assembly = assemble_context(memories, &contradictions)?;

        let event_cursor = task_id.map_or(Ok(ChangeCursor::default()), |task_id| {
            self.latest_task_cursor_read(task_id)
        })?;
        let payload = ContextPacketPayload {
            schema_version: SCHEMA_VERSION,
            project_id: project_id.clone(),
            task_id,
            agent_id: agent_id.into(),
            event_cursor,
            pinned: assembly.pinned.clone(),
            index: assembly.index.clone(),
            omissions: assembly.omissions.clone(),
            proposed_count: assembly.proposed_count,
            stale_count: assembly.stale_count,
            created_at: now,
        };
        let object = self.append("context_packet", &payload)?;
        Ok(ContextPacket {
            header: ContextPacketHeader {
                project_id: project_id.clone(),
                task_id,
                packet_hash: object.hash().clone(),
                event_cursor,
                proposed_count: assembly.proposed_count,
                stale_count: assembly.stale_count,
            },
            pinned: assembly.pinned,
            index: assembly.index,
            omissions: assembly.omissions,
        })
    }

    /// Explains a previously built packet, with owner authorization enforced
    /// before any packet content is returned.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] for unknown packets, integrity failures, or an
    /// owner mismatch.
    pub fn explain_context(
        &self,
        packet_hash: &ObjectHash,
        agent_id: &str,
    ) -> Result<ContextPacketPayload, StoreError> {
        let payload: ContextPacketPayload =
            self.get_typed_object(packet_hash, "context_packet")?
                .ok_or_else(|| StoreError::PacketAccessDenied(packet_hash.clone()))?;
        if payload.schema_version != SCHEMA_VERSION || payload.agent_id != agent_id {
            return Err(StoreError::PacketAccessDenied(packet_hash.clone()));
        }
        Ok(payload)
    }

    fn applicable_contradictions(
        &self,
        task_id: Option<TaskId>,
        memories: &[MemorySummary],
    ) -> Result<Vec<ApplicableContradiction>, StoreError> {
        let Some(task_id) = task_id else {
            return Ok(Vec::new());
        };
        let visible: std::collections::HashSet<_> =
            memories.iter().map(|memory| &memory.version).collect();
        let mut statement = self.connection.prepare(
            "SELECT contradiction_hash, left_version_hash, right_version_hash
             FROM memory_contradictions WHERE task_id = ?1
             ORDER BY contradiction_hash",
        )?;
        let rows = statement.query_map([task_id.0.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
            ))
        })?;
        rows.filter_map(|row| match row {
            Ok((contradiction, left, right)) => {
                let parsed = (|| {
                    let contradiction = ObjectHash::from_stored(contradiction.clone())
                        .ok_or(StoreError::InvalidStoredHash(contradiction))?;
                    let left = ObjectHash::from_stored(left.clone())
                        .ok_or(StoreError::InvalidStoredHash(left))?;
                    let right = ObjectHash::from_stored(right.clone())
                        .ok_or(StoreError::InvalidStoredHash(right))?;
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
        session_id: &SessionId,
        agent_id: &str,
    ) -> Result<MemoryRecord, StoreError> {
        let assertion_hash: Option<String> = self
            .connection
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
        let version: MemoryVersion = self
            .get_typed_object(version_hash, "memory_version")?
            .ok_or_else(|| StoreError::MemoryNotFound(version_hash.clone()))?;
        let authorized = match &version.scope {
            Scope::Project { project } => project == project_id,
            Scope::Task { project, task } => {
                project == project_id
                    && Some(*task) == task_id
                    && self
                        .ensure_task_participant(project_id, *task, session_id)
                        .is_ok()
            }
            Scope::Agent {
                project,
                task,
                agent,
            } => project == project_id && *task == task_id && agent == agent_id,
        };
        if !authorized || version.sensitivity == Sensitivity::Restricted {
            return Err(StoreError::MemoryAccessDenied(version_hash.clone()));
        }
        let assertion: MemoryAssertionEvent = self
            .get_typed_object(&assertion_hash, "memory_assertion_event")?
            .ok_or_else(|| StoreError::MemoryNotFound(version_hash.clone()))?;
        Ok(MemoryRecord {
            version_hash: version_hash.clone(),
            assertion_hash,
            version,
            assertion,
        })
    }

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
        self.ensure_task_participant(project_id, task_id, session_id)?;
        let visible = self.search_memories(project_id, Some(task_id), agent_id, None, 1_000)?;
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
            "SELECT cursor, object_kind, object_hash
             FROM task_changes
             WHERE task_id = ?1 AND cursor > ?2
             ORDER BY cursor
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

    /// Verifies canonical bytes and hashes for every stored object.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when SQLite cannot scan the object table.
    pub fn verify_all(&self) -> Result<IntegrityReport, StoreError> {
        let mut statement = self
            .connection
            .prepare("SELECT object_hash, canonical_json FROM objects ORDER BY object_hash")?;
        let rows = statement.query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?;

        let mut report = IntegrityReport::default();
        for row in rows {
            let (stored_hash, bytes) = row?;
            report.checked_objects += 1;
            let valid = ObjectHash::from_stored(stored_hash.clone())
                .is_some_and(|expected| CanonicalObject::verify(&expected, bytes).is_ok());
            if !valid {
                report.invalid_objects.push(stored_hash);
            }
        }
        Ok(report)
    }

    fn apply_memory_projection(
        transaction: &Transaction<'_>,
        version_hash: &ObjectHash,
        assertion_hash: &ObjectHash,
        version: &MemoryVersion,
        assertion: &MemoryAssertionEvent,
    ) -> Result<(), StoreError> {
        if version.schema_version != SCHEMA_VERSION
            || assertion.schema_version != SCHEMA_VERSION
            || version.memory_id != assertion.memory_id
            || &assertion.version != version_hash
        {
            return Err(StoreError::InvalidMemoryProjection(
                "version and assertion identities do not agree".into(),
            ));
        }

        let (scope_kind, project_id, task_id, agent_id) = match &version.scope {
            Scope::Project { project } => ("project", &project.0, None, None),
            Scope::Task { project, task } => ("task", &project.0, Some(task.0.to_string()), None),
            Scope::Agent {
                project,
                task,
                agent,
            } => (
                "agent",
                &project.0,
                task.map(|value| value.0.to_string()),
                Some(agent.as_str()),
            ),
        };
        transaction.execute(
            "INSERT INTO memory_heads (
                 memory_id, version_hash, assertion_hash, schema_version,
                 status, scope_kind, project_id, task_id, agent_id,
                 memory_kind, authority, delivery, sensitivity, title, body,
                 created_at_ms
             ) VALUES (
                 ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                 ?14, ?15, ?16
             )
             ON CONFLICT(memory_id) DO UPDATE SET
                 version_hash = excluded.version_hash,
                 assertion_hash = excluded.assertion_hash,
                 schema_version = excluded.schema_version,
                 status = excluded.status,
                 scope_kind = excluded.scope_kind,
                 project_id = excluded.project_id,
                 task_id = excluded.task_id,
                 agent_id = excluded.agent_id,
                 memory_kind = excluded.memory_kind,
                 authority = excluded.authority,
                 delivery = excluded.delivery,
                 sensitivity = excluded.sensitivity,
                 title = excluded.title,
                 body = excluded.body,
                 created_at_ms = excluded.created_at_ms",
            params![
                version.memory_id.0.to_string(),
                version_hash.as_str(),
                assertion_hash.as_str(),
                i64::from(version.schema_version),
                enum_name(assertion.status)?,
                scope_kind,
                project_id,
                task_id,
                agent_id,
                enum_name(version.kind)?,
                enum_name(version.authority)?,
                enum_name(version.delivery)?,
                enum_name(version.sensitivity)?,
                version.title,
                version.body,
                version.created_at.timestamp_millis(),
            ],
        )?;
        transaction.execute(
            "DELETE FROM object_fts WHERE object_hash = ?1",
            [version_hash.as_str()],
        )?;
        transaction.execute(
            "INSERT INTO object_fts (object_hash, title, body) VALUES (?1, ?2, ?3)",
            params![version_hash.as_str(), version.title, version.body],
        )?;
        Ok(())
    }

    fn latest_task_cursor(
        transaction: &Transaction<'_>,
        task_id: TaskId,
    ) -> Result<ChangeCursor, StoreError> {
        let cursor = transaction.query_row(
            "SELECT COALESCE(MAX(cursor), 0) FROM task_changes WHERE task_id = ?1",
            [task_id.0.to_string()],
            |row| row.get(0),
        )?;
        Ok(ChangeCursor(cursor))
    }

    fn latest_task_cursor_read(&self, task_id: TaskId) -> Result<ChangeCursor, StoreError> {
        let cursor = self.connection.query_row(
            "SELECT COALESCE(MAX(cursor), 0) FROM task_changes WHERE task_id = ?1",
            [task_id.0.to_string()],
            |row| row.get(0),
        )?;
        Ok(ChangeCursor(cursor))
    }

    fn ensure_task_participant(
        &self,
        project_id: &crate::domain::ProjectId,
        task_id: TaskId,
        session_id: &SessionId,
    ) -> Result<(), StoreError> {
        let participant: Option<i64> = self
            .connection
            .query_row(
                "SELECT 1 FROM tasks t JOIN task_participants p
                   ON p.task_id = t.task_id
                 WHERE t.task_id = ?1 AND t.project_id = ?2
                   AND p.session_id = ?3",
                params![task_id.0.to_string(), project_id.0, session_id.0],
                |row| row.get(0),
            )
            .optional()?;
        if participant.is_none() {
            return Err(StoreError::TaskAccessDenied {
                task: task_id,
                session: session_id.0.clone(),
            });
        }
        Ok(())
    }

    fn get_typed_object<T: DeserializeOwned>(
        &self,
        hash: &ObjectHash,
        object_kind: &str,
    ) -> Result<Option<T>, StoreError> {
        let stored: Option<(String, Vec<u8>)> = self
            .connection
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
        CanonicalObject::verify(hash, bytes)?.decode().map(Some)
    }

    fn load_task(transaction: &Transaction<'_>, task_id: TaskId) -> Result<LocalTask, StoreError> {
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

    fn decode_memory_summary(row: &rusqlite::Row<'_>) -> rusqlite::Result<MemorySummaryRow> {
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
        ))
    }

    fn parse_memory_summary(row: MemorySummaryRow) -> Result<MemorySummary, StoreError> {
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
        let scope = match scope_kind.as_str() {
            "project" => Scope::Project { project },
            "task" => Scope::Task {
                project,
                task: task.ok_or_else(|| {
                    StoreError::InvalidMemoryProjection("task scope has no task id".into())
                })?,
            },
            "agent" => Scope::Agent {
                project,
                task,
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

    fn insert_object(
        transaction: &Transaction<'_>,
        object_kind: &str,
        object: &CanonicalObject,
    ) -> Result<(), StoreError> {
        let existing: Option<(String, Vec<u8>)> = transaction
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
                transaction.execute(
                    "INSERT INTO objects (object_hash, object_kind, canonical_json)
                     VALUES (?1, ?2, ?3)",
                    params![object.hash().as_str(), object_kind, object.bytes()],
                )?;
                Ok(())
            }
        }
    }

    fn insert_task_change(
        transaction: &Transaction<'_>,
        task_id: TaskId,
        object_kind: &str,
        object: &CanonicalObject,
    ) -> Result<ChangeCursor, StoreError> {
        transaction.execute(
            "INSERT OR IGNORE INTO task_changes (task_id, object_kind, object_hash)
             VALUES (?1, ?2, ?3)",
            params![task_id.0.to_string(), object_kind, object.hash().as_str()],
        )?;
        let cursor = transaction.query_row(
            "SELECT cursor FROM task_changes WHERE task_id = ?1 AND object_hash = ?2",
            params![task_id.0.to_string(), object.hash().as_str()],
            |row| row.get(0),
        )?;
        Ok(ChangeCursor(cursor))
    }
}

fn note_fingerprint(request: &NoteRequest) -> Result<CanonicalObject, StoreError> {
    CanonicalObject::freeze(&NoteIntentFingerprint {
        project_id: &request.project_id,
        task_id: request.task_id,
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

fn claim_expiry(now: DateTime<Utc>, ttl_seconds: i64) -> Result<DateTime<Utc>, StoreError> {
    if !(1..=86_400).contains(&ttl_seconds) {
        return Err(StoreError::InvalidStoredClaim(
            "lease TTL must be from 1 through 86400 seconds".into(),
        ));
    }
    Ok(now + chrono::TimeDelta::seconds(ttl_seconds))
}

fn prepare_note(request: &NoteRequest) -> Result<PreparedNote, StoreError> {
    let classification = classify_note(
        &request.prose,
        request.title.as_deref(),
        request.kind,
        request.authority,
        request.visibility,
    );
    let scope = match request.visibility {
        NoteVisibility::Shared => request.task_id.map_or_else(
            || Scope::Project {
                project: request.project_id.clone(),
            },
            |task| Scope::Task {
                project: request.project_id.clone(),
                task,
            },
        ),
        NoteVisibility::Private => Scope::Agent {
            project: request.project_id.clone(),
            task: request.task_id,
            agent: request.actor.actor_id.clone(),
        },
    };
    let (status, policy_reason) = activation_policy(&scope, classification.kind);
    let memory_id = MemoryId::new();
    let version = MemoryVersion {
        schema_version: SCHEMA_VERSION,
        memory_id,
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
            assembly.omissions.push(ContextOmission {
                memory_id: memory.memory_id,
                version: memory.version,
                reason: "restricted sensitivity requires an unavailable authorization".into(),
            });
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
            Delivery::Index => assembly.omissions.push(ContextOmission {
                memory_id: memory.memory_id,
                version: memory.version,
                reason: "index byte budget exhausted".into(),
            }),
            Delivery::OnDemand => assembly.omissions.push(ContextOmission {
                memory_id: memory.memory_id,
                version: memory.version,
                reason: "on-demand memory is available through search".into(),
            }),
            Delivery::Suppressed => assembly.omissions.push(ContextOmission {
                memory_id: memory.memory_id,
                version: memory.version,
                reason: "delivery is suppressed by attributed policy".into(),
            }),
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

fn enum_name<T: Serialize>(value: T) -> Result<String, StoreError> {
    serde_json::to_value(value)?
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| StoreError::InvalidMemoryProjection("enum did not serialize as text".into()))
}

fn parse_enum<T: DeserializeOwned>(value: &str) -> Result<T, StoreError> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).map_err(StoreError::Json)
}

fn fts_query(query: &str) -> String {
    let tokens: Vec<_> = query
        .split(|character: char| !character.is_alphanumeric() && character != '_')
        .filter(|token| !token.is_empty())
        .map(|token| format!("\"{token}\"*"))
        .collect();
    if tokens.is_empty() {
        "\"__engram_no_match__\"".into()
    } else {
        tokens.join(" AND ")
    }
}

fn usize_to_u32(value: usize) -> u32 {
    u32::try_from(value).unwrap_or(u32::MAX)
}

fn retrieval_reason(scope: &Scope, delivery: Delivery) -> String {
    let scope_reason = match scope {
        Scope::Project { .. } => "applicable project memory",
        Scope::Task { .. } => "shared memory for the active task",
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

#[cfg(test)]
mod tests {
    use chrono::TimeDelta;
    use serde::{Deserialize, Serialize};

    use super::*;
    use crate::{
        DevelopmentNoopRedactor,
        domain::{AssuranceLevel, MemoryStatus, NoteRequest, NoteVisibility, ProjectId},
    };

    #[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
    struct Example {
        title: String,
        body: String,
    }

    fn actor(session: &str) -> ActorContext {
        ActorContext {
            actor_id: session.into(),
            actor_kind: "agent".into(),
            assurance: AssuranceLevel::Asserted,
            run_id: None,
            session_id: Some(SessionId(session.into())),
            source_tool: Some("test".into()),
            source_skill: None,
            provenance_chain: Vec::new(),
            reason: "exercise coordination semantics".into(),
        }
    }

    fn note_request(
        task_id: TaskId,
        session: &str,
        prose: &str,
        key: &str,
        visibility: NoteVisibility,
    ) -> NoteRequest {
        NoteRequest {
            project_id: ProjectId("project-a".into()),
            task_id: Some(task_id),
            prose: prose.into(),
            visibility,
            kind: None,
            authority: None,
            sensitivity: None,
            title: None,
            tags: Vec::new(),
            evidence: Vec::new(),
            refs: Vec::new(),
            actor: actor(session),
            idempotency_key: key.into(),
            created_at: Utc::now(),
        }
    }

    #[test]
    fn append_is_idempotent_and_round_trips_verified_content() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let value = Example {
            title: "Decision".into(),
            body: "Freeze reports before publishing.".into(),
        };

        let first = store.append("memory_version", &value).unwrap();
        let second = store.append("memory_version", &value).unwrap();
        let loaded: Example = store.get(first.hash()).unwrap().unwrap();

        assert_eq!(first, second);
        assert_eq!(loaded, value);
        assert_eq!(
            store.verify_all().unwrap(),
            IntegrityReport {
                checked_objects: 1,
                invalid_objects: Vec::new(),
            }
        );
    }

    #[test]
    fn object_kind_is_bound_to_the_content_address() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let value = Example {
            title: "Decision".into(),
            body: "Task memory is shared by default.".into(),
        };

        store.append("memory_version", &value).unwrap();
        let mismatch = store.append("report", &value);

        assert!(matches!(
            mismatch,
            Err(StoreError::ObjectKindMismatch { .. })
        ));
    }

    #[test]
    fn task_changes_are_ordered_and_idempotent() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let task_id = TaskId::new();
        let first = Example {
            title: "Decision".into(),
            body: "Task memory is shared by default.".into(),
        };
        let second = Example {
            title: "Evidence".into(),
            body: "A peer confirmed the decision.".into(),
        };

        let (first_object, first_cursor) = store
            .append_task_object(task_id, "memory_version", &first)
            .unwrap();
        let (_, replay_cursor) = store
            .append_task_object(task_id, "memory_version", &first)
            .unwrap();
        let (second_object, second_cursor) = store
            .append_task_object(task_id, "memory_version", &second)
            .unwrap();

        assert_eq!(first_cursor, replay_cursor);
        assert!(second_cursor > first_cursor);
        assert_eq!(
            store
                .task_changes_since(task_id, first_cursor, 100)
                .unwrap(),
            vec![TaskChange {
                cursor: second_cursor,
                task_id,
                object_kind: "memory_version".into(),
                object_hash: second_object.hash().clone(),
            }]
        );
        assert_ne!(first_object.hash(), second_object.hash());
    }

    #[test]
    fn sessions_rendezvous_using_only_the_external_reference() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let project = ProjectId("project-a".into());
        let now = Utc::now();
        let first = store
            .start_task(
                &project,
                "dummy:TASK-7",
                "Dogfood the memory loop",
                &SessionId("eval-a".into()),
                actor("eval-a"),
                now,
            )
            .unwrap();
        let peer = store
            .join_task(
                &project,
                "dummy:TASK-7",
                &SessionId("eval-b".into()),
                actor("eval-b"),
                now + TimeDelta::milliseconds(1),
            )
            .unwrap();
        let replay = store
            .join_task(
                &project,
                "dummy:TASK-7",
                &SessionId("eval-b".into()),
                actor("eval-b"),
                now + TimeDelta::milliseconds(2),
            )
            .unwrap();

        assert_eq!(first.task.task_id, peer.task.task_id);
        assert_eq!(peer.task.participants.len(), 2);
        assert_eq!(peer.cursor, replay.cursor);
        assert!(!replay.joined);
        assert_eq!(
            store
                .task_changes_since(first.task.task_id, ChangeCursor::default(), 20)
                .unwrap()
                .len(),
            2
        );
        assert!(matches!(
            store.join_task(
                &project,
                "dummy:MISSING",
                &SessionId("eval-c".into()),
                actor("eval-c"),
                now,
            ),
            Err(StoreError::TaskReferenceNotFound(_))
        ));
    }

    #[test]
    fn note_capture_is_idempotent_searchable_and_explainable() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let task_id = TaskId::new();
        let request = note_request(
            task_id,
            "session-a",
            "Decision: use canonical task memory as the shared source",
            "note-a",
            NoteVisibility::Shared,
        );

        let first = store
            .capture_note(&request, &DevelopmentNoopRedactor)
            .unwrap();
        let mut retry_request = request.clone();
        retry_request.created_at += TimeDelta::seconds(1);
        let replay = store
            .capture_note(&retry_request, &DevelopmentNoopRedactor)
            .unwrap();
        let visible = store
            .search_memories(
                &request.project_id,
                Some(task_id),
                "session-b",
                Some("canonical source"),
                20,
            )
            .unwrap();

        assert_eq!(first.memory_id, replay.memory_id);
        assert!(!first.duplicate);
        assert!(replay.duplicate);
        assert_eq!(first.status, MemoryStatus::Active);
        assert_eq!(first.kind, crate::domain::MemoryKind::Decision);
        assert_eq!(visible.len(), 1);
        assert_eq!(visible[0].version, first.version);
        assert!(first.cursor.is_some());

        let mut conflict = request.clone();
        conflict.prose = "Decision: reuse the key for something else".into();
        assert!(matches!(
            store.capture_note(&conflict, &DevelopmentNoopRedactor),
            Err(StoreError::NoteIdempotencyConflict(_))
        ));
    }

    #[test]
    fn private_task_scratch_never_enters_the_peer_feed() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let task_id = TaskId::new();
        let request = note_request(
            task_id,
            "agent-a",
            "Hypothesis: the failure may be environmental.",
            "private-a",
            NoteVisibility::Private,
        );
        let receipt = store
            .capture_note(&request, &DevelopmentNoopRedactor)
            .unwrap();

        assert!(receipt.cursor.is_none());
        assert_eq!(
            store
                .search_memories(&request.project_id, Some(task_id), "agent-a", None, 20,)
                .unwrap()
                .len(),
            1
        );
        assert!(
            store
                .search_memories(&request.project_id, Some(task_id), "agent-b", None, 20,)
                .unwrap()
                .is_empty()
        );
        assert!(
            store
                .task_changes_since(task_id, ChangeCursor::default(), 20)
                .unwrap()
                .is_empty()
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one scenario must preserve the exact pre/post-restart cursor and hashes"
    )]
    fn context_delta_show_and_private_scope_survive_restart() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("engram.db");
        let project = ProjectId("project-a".into());
        let session_a = SessionId("eval-a".into());
        let session_b = SessionId("eval-b".into());
        let now = Utc::now();
        let (task_id, first_receipt, packet, expected_delta, private_hash) = {
            let mut store = SqliteStore::open(&database).unwrap();
            let task = store
                .start_task(
                    &project,
                    "dummy:TASK-7",
                    "Dogfood",
                    &session_a,
                    actor("eval-a"),
                    now,
                )
                .unwrap();
            let task_id = task.task.task_id;
            store
                .join_task(
                    &project,
                    "dummy:TASK-7",
                    &session_b,
                    actor("eval-b"),
                    now + TimeDelta::milliseconds(1),
                )
                .unwrap();
            let first_request = note_request(
                task_id,
                "eval-a",
                "Decision: freeze one report payload per retry key",
                "first",
                NoteVisibility::Shared,
            );
            let first_receipt = store
                .capture_note(&first_request, &DevelopmentNoopRedactor)
                .unwrap();
            let packet = store
                .build_context(
                    &project,
                    Some(task_id),
                    &session_b,
                    "eval-b",
                    now + TimeDelta::milliseconds(2),
                )
                .unwrap();
            assert_eq!(packet.index.len(), 1);
            assert_eq!(packet.index[0].version, first_receipt.version);

            let second_request = note_request(
                task_id,
                "eval-a",
                "Evidence: retry integration test returns byte-identical content",
                "second",
                NoteVisibility::Shared,
            );
            store
                .capture_note(&second_request, &DevelopmentNoopRedactor)
                .unwrap();
            let expected_delta = store
                .task_delta(
                    &project,
                    task_id,
                    &session_b,
                    "eval-b",
                    packet.header.event_cursor,
                    20,
                )
                .unwrap();
            assert_eq!(expected_delta.changes.len(), 1);

            let private_request = note_request(
                task_id,
                "eval-a",
                "scratch: half-formed hypothesis Z",
                "private",
                NoteVisibility::Private,
            );
            let private_receipt = store
                .capture_note(&private_request, &DevelopmentNoopRedactor)
                .unwrap();
            assert!(matches!(
                store.show_memory(
                    &private_receipt.version,
                    &project,
                    Some(task_id),
                    &session_b,
                    "eval-b",
                ),
                Err(StoreError::MemoryAccessDenied(_))
            ));
            assert!(
                store
                    .search_memories(&project, Some(task_id), "eval-b", Some("hypothesis Z"), 20,)
                    .unwrap()
                    .is_empty()
            );
            (
                task_id,
                first_receipt,
                packet,
                expected_delta,
                private_receipt.version,
            )
        };

        let reopened = SqliteStore::open(&database).unwrap();
        let after_restart = reopened
            .task_delta(
                &project,
                task_id,
                &session_b,
                "eval-b",
                packet.header.event_cursor,
                20,
            )
            .unwrap();
        assert_eq!(
            serde_json::to_vec(&after_restart).unwrap(),
            serde_json::to_vec(&expected_delta).unwrap()
        );
        let shown = reopened
            .show_memory(
                &first_receipt.version,
                &project,
                Some(task_id),
                &session_b,
                "eval-b",
            )
            .unwrap();
        assert_eq!(shown.version.actor.session_id, Some(session_a));
        assert!(!shown.version.classification_reason.is_empty());
        assert_eq!(
            reopened
                .explain_context(&packet.header.packet_hash, "eval-b")
                .unwrap()
                .event_cursor,
            packet.header.event_cursor
        );
        assert!(matches!(
            reopened.show_memory(&private_hash, &project, Some(task_id), &session_b, "eval-b",),
            Err(StoreError::MemoryAccessDenied(_))
        ));
    }

    #[test]
    fn memory_projection_rebuilds_from_canonical_objects() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let task_id = TaskId::new();
        let request = note_request(
            task_id,
            "agent-a",
            "Evidence: the integration test passes after restart",
            "evidence-a",
            NoteVisibility::Shared,
        );
        store
            .capture_note(&request, &DevelopmentNoopRedactor)
            .unwrap();

        assert_eq!(store.rebuild_memory_index().unwrap(), 1);
        let rebuilt = store
            .search_memories(
                &request.project_id,
                Some(task_id),
                "agent-b",
                Some("integration restart"),
                20,
            )
            .unwrap();
        assert_eq!(rebuilt.len(), 1);
        assert_eq!(rebuilt[0].kind, crate::domain::MemoryKind::Fact);
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one scenario verifies declaration, replay, fail-closed delivery, status, and rebuild"
    )]
    fn applicable_pinned_contradictions_fail_closed_and_rebuild() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let project = ProjectId("project-a".into());
        let session_a = SessionId("agent-a".into());
        let session_b = SessionId("agent-b".into());
        let now = Utc::now();
        let task = store
            .start_task(
                &project,
                "dummy:CONFLICT-1",
                "Exercise contradiction safety",
                &session_a,
                actor("agent-a"),
                now,
            )
            .unwrap();
        store
            .join_task(
                &project,
                "dummy:CONFLICT-1",
                &session_b,
                actor("agent-b"),
                now,
            )
            .unwrap();
        let first = store
            .capture_note(
                &note_request(
                    task.task.task_id,
                    "agent-a",
                    "Never publish before every participant is ready.",
                    "constraint-a",
                    NoteVisibility::Shared,
                ),
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        let second = store
            .capture_note(
                &note_request(
                    task.task.task_id,
                    "agent-a",
                    "Always publish immediately when the implementation passes.",
                    "constraint-b",
                    NoteVisibility::Shared,
                ),
                &DevelopmentNoopRedactor,
            )
            .unwrap();

        let edge = store
            .record_memory_contradiction(
                &project,
                task.task.task_id,
                &session_a,
                "agent-a",
                &first.version,
                &second.version,
                "the publication timing rules cannot both be followed",
                "contradiction-a",
                actor("agent-a"),
                now,
            )
            .unwrap();
        let replay = store
            .record_memory_contradiction(
                &project,
                task.task.task_id,
                &session_a,
                "agent-a",
                &second.version,
                &first.version,
                "the publication timing rules cannot both be followed",
                "contradiction-a",
                actor("agent-a"),
                now + TimeDelta::seconds(1),
            )
            .unwrap();
        assert_eq!(replay.contradiction, edge.contradiction);
        assert!(replay.duplicate);

        let assert_fails_closed = |store: &mut SqliteStore| {
            let result = store.build_context(
                &project,
                Some(task.task.task_id),
                &session_b,
                "agent-b",
                now,
            );
            match result {
                Err(StoreError::PinnedContradiction {
                    contradiction,
                    left,
                    right,
                }) => {
                    assert_eq!(contradiction, edge.contradiction);
                    let actual = [left, right];
                    assert!(actual.contains(&first.version));
                    assert!(actual.contains(&second.version));
                }
                other => panic!("expected pinned contradiction, got {other:?}"),
            }
        };
        assert_fails_closed(&mut store);
        let visible = store
            .search_memories(&project, Some(task.task.task_id), "agent-b", None, 20)
            .unwrap();
        assert!(
            visible
                .iter()
                .all(|memory| memory.status == MemoryStatus::Contested)
        );

        assert_eq!(store.rebuild_memory_index().unwrap(), 2);
        assert_fails_closed(&mut store);
    }

    #[test]
    fn soft_contradictions_are_delivered_and_flagged() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let project = ProjectId("project-a".into());
        let session = SessionId("agent-a".into());
        let now = Utc::now();
        let task = store
            .start_task(
                &project,
                "dummy:CONFLICT-2",
                "Surface soft conflicts",
                &session,
                actor("agent-a"),
                now,
            )
            .unwrap();
        let first = store
            .capture_note(
                &note_request(
                    task.task.task_id,
                    "agent-a",
                    "Fact: the integration uses polling.",
                    "soft-a",
                    NoteVisibility::Shared,
                ),
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        let second = store
            .capture_note(
                &note_request(
                    task.task.task_id,
                    "agent-a",
                    "Fact: the integration uses notifications only.",
                    "soft-b",
                    NoteVisibility::Shared,
                ),
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        store
            .record_memory_contradiction(
                &project,
                task.task.task_id,
                &session,
                "agent-a",
                &first.version,
                &second.version,
                "the transport descriptions disagree",
                "soft-conflict",
                actor("agent-a"),
                now,
            )
            .unwrap();

        let packet = store
            .build_context(&project, Some(task.task.task_id), &session, "agent-a", now)
            .unwrap();
        assert_eq!(packet.index.len(), 2);
        assert!(packet.index.iter().all(|item| {
            item.status == MemoryStatus::Contested
                && item.retrieval_reason.contains("unresolved contradiction")
        }));
    }

    #[test]
    fn live_task_claims_are_atomic_across_connections() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("engram.db");
        let mut first_store = SqliteStore::open(&database).unwrap();
        let mut peer_store = SqliteStore::open(&database).unwrap();
        let task_id = TaskId::new();
        let now = Utc::now();
        let first = first_store
            .claim_task(
                task_id,
                &SessionId("session-a".into()),
                "claim-a",
                now,
                300,
                actor("session-a"),
            )
            .unwrap();
        let replay = first_store
            .claim_task(
                task_id,
                &SessionId("session-a".into()),
                "claim-a",
                now + TimeDelta::seconds(2),
                300,
                actor("session-a"),
            )
            .unwrap();
        let conflict = peer_store.claim_task(
            task_id,
            &SessionId("session-b".into()),
            "claim-b",
            now,
            300,
            actor("session-b"),
        );

        assert_eq!(first, replay);
        assert!(matches!(conflict, Err(StoreError::TaskClaimHeld { .. })));
        assert!(matches!(
            first_store.claim_task(
                task_id,
                &SessionId("session-a".into()),
                "claim-a",
                now,
                360,
                actor("session-a"),
            ),
            Err(StoreError::ClaimIdempotencyConflict(_))
        ));

        let after_expiry = first.expires_at + TimeDelta::milliseconds(1);
        let peer = peer_store
            .claim_task(
                task_id,
                &SessionId("session-b".into()),
                "claim-b-after-expiry",
                after_expiry,
                300,
                actor("session-b"),
            )
            .unwrap();

        assert_eq!(peer.revision, first.revision + 1);
        assert_eq!(
            peer_store
                .task_changes_since(task_id, ChangeCursor::default(), 100)
                .unwrap()
                .len(),
            2
        );
    }
}
