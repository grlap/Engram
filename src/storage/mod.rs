//! Local SQLite object store and integrity verification.

mod control_runtime;
mod control_support;
mod doctor;
mod graph_snapshot;
mod objects_tasks;
mod open_schema;
mod policy_admin;
mod project_memory;
mod schema_diagnostics;
mod task_memory;
mod work;

pub use schema_diagnostics::{
    StoreOpenRefusalKind, running_schema_reference, store_open_refusal_kind, store_schema_reference,
};

pub(crate) const PENDING_HANDOFF_REFUSAL: &str =
    "a live handoff offer blocks this operation; cancel the offer, or let it be accepted or expire";

pub(crate) fn parent_not_open_remedy(lifecycle: crate::domain::WorkLifecycle) -> &'static str {
    match lifecycle {
        crate::domain::WorkLifecycle::Proposed => {
            "the parent is proposed, not open; inspect it before adding children"
        }
        crate::domain::WorkLifecycle::Open => "inspect the parent before retrying",
        crate::domain::WorkLifecycle::Completed
        | crate::domain::WorkLifecycle::Cancelled
        | crate::domain::WorkLifecycle::Superseded => {
            "file an independent root follow-up or add under an open ancestor"
        }
    }
}

#[cfg(test)]
mod test_support;

#[cfg(test)]
use control_runtime::resolve_verification_environment_on;
use control_support::{
    normalize_control_policy_actor, normalize_control_policy_idempotency_key,
    normalize_control_text,
};
use project_memory::{
    derived_project_memory_state_on, derived_project_memory_state_rows_on,
    lookup_project_memory_on, project_memory_state_on, validate_keyed_project_memory_shape,
    validate_stored_project_memory_key,
};
use task_memory::{claim_expiry, fts_query, normalize_project_memory_query};

pub(crate) use work::WorkNotePage;
pub(crate) use work::{WorkEvidenceProjectionSummary, WorkObligationRecord};

pub(crate) use work::{
    CompleteWorkStorageResult, CompletionRecoverySnapshot, StageWorkSessionDelivery,
    WorkNoteCapture, normalize_completion_acceptance_shape,
};

pub(crate) const PROCESS_DEFAULT_WORK_SESSION_NAMESPACE: &str = "local-process-";
pub(crate) const PROCESS_DEFAULT_WORK_SESSION_PREFIX: &str = "local-process-v1-";
pub(crate) const PROCESS_DEFAULT_WORK_SESSION_RETENTION_SECONDS: i64 = 7 * 24 * 60 * 60;
pub(crate) const PROCESS_DEFAULT_WORK_SESSION_REUSE_REFUSAL: &str = "process-default work session cannot be reused; run without --session-id to receive a fresh process default";

#[cfg(test)]
pub(crate) use work::{
    reset_work_catalog_count_queries, reset_work_event_decode_count,
    reset_work_item_projection_decode_count, work_catalog_count_queries, work_event_decode_count,
    work_item_projection_decode_count,
};

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    time::Duration,
};

#[cfg(test)]
use std::cell::Cell;

#[cfg(test)]
type TestTableColumn = (i64, String, String, i64, Option<String>, i64);

#[cfg(test)]
#[derive(Debug, Eq, PartialEq)]
pub(crate) struct TestDatabaseShapeSnapshot {
    schema: Vec<(String, String, String, i64, Option<String>)>,
    table_info: Vec<(String, Vec<TestTableColumn>)>,
    rows: Vec<(String, Vec<Vec<u8>>)>,
}

#[cfg(test)]
pub(crate) fn test_database_shape_snapshot(
    connection: &Connection,
) -> Result<TestDatabaseShapeSnapshot, rusqlite::Error> {
    let schema = connection
        .prepare(
            "SELECT type, name, tbl_name, rootpage, sql
             FROM sqlite_master ORDER BY type, name",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, i64>(3)?,
                row.get::<_, Option<String>>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let table_names = schema
        .iter()
        .filter_map(|(kind, name, _, _, _)| (kind == "table").then_some(name.clone()))
        .collect::<Vec<_>>();
    let mut table_info = Vec::with_capacity(table_names.len());
    let mut rows = Vec::with_capacity(table_names.len());
    for table in table_names {
        let quoted = table.replace('"', "\"\"");
        let info = connection
            .prepare(&format!("PRAGMA table_info(\"{quoted}\")"))?
            .query_map([], |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                    row.get(5)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        table_info.push((table.clone(), info));

        let mut statement = connection.prepare(&format!("SELECT * FROM \"{quoted}\""))?;
        let column_count = statement.column_count();
        let mut table_rows = statement
            .query_map([], |row| {
                let mut encoded = Vec::new();
                for index in 0..column_count {
                    let value = match row.get_ref(index)? {
                        rusqlite::types::ValueRef::Null => vec![0],
                        rusqlite::types::ValueRef::Integer(value) => {
                            let mut bytes = vec![1];
                            bytes.extend_from_slice(&value.to_be_bytes());
                            bytes
                        }
                        rusqlite::types::ValueRef::Real(value) => {
                            let mut bytes = vec![2];
                            bytes.extend_from_slice(&value.to_bits().to_be_bytes());
                            bytes
                        }
                        rusqlite::types::ValueRef::Text(value) => {
                            let mut bytes = vec![3];
                            bytes.extend_from_slice(value);
                            bytes
                        }
                        rusqlite::types::ValueRef::Blob(value) => {
                            let mut bytes = vec![4];
                            bytes.extend_from_slice(value);
                            bytes
                        }
                    };
                    encoded.extend_from_slice(&(value.len() as u64).to_be_bytes());
                    encoded.extend_from_slice(&value);
                }
                Ok(encoded)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        table_rows.sort();
        rows.push((table, table_rows));
    }
    Ok(TestDatabaseShapeSnapshot {
        schema,
        table_info,
        rows,
    })
}

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;

use crate::{
    CanonicalObject, ObjectHash,
    control::{LeasePolicyInput, effective_mediated_effects, evaluate_lease_policy},
    domain::{
        ActorContext, AssuranceLevel, Authority, CONTROL_SCHEMA_VERSION, ChangeCursor, ContextItem,
        ContextOmission, ContextOmissionSummary, ContextPacket, ContextPacketHeader,
        ContextPacketPayload, ControlAssurance, ControlDelivery, ControlEpochs, ControlHealth,
        ControlPolicy, ControlSessionBinding, ControlSessionStatus, ControlTurnBeginDecision,
        ControlTurnCheckpointDecision, ControlTurnDecision, ControlWorkBinding, Delivery,
        DeliveryPage, DeltaItem, EffectClass, EnvironmentComponents, EnvironmentEvidence,
        EnvironmentEvidenceInput, EnvironmentEvidenceReference, ExecutionObservation,
        ExecutionObservationInput, ExecutionObservationReference, ExecutionOutcome,
        ForgetProjectMemoryRequest, HostPathPolicy, IssuedTurnGrant, LocalTask,
        MAX_PROJECT_MEMORY_BODY_BYTES, MAX_PROJECT_MEMORY_KEY_BYTES,
        MAX_PROJECT_MEMORY_QUERY_BYTES, MAX_PROJECT_MEMORY_QUERY_TOKENS, MemoryAssertionEvent,
        MemoryContradictionEvent, MemoryContradictionReceipt, MemoryId, MemoryKind, MemoryRecord,
        MemoryStatus, MemorySummary, MemoryVersion, NoteReceipt, NoteRequest, NoteVisibility,
        OBLIGATION_RULE_SET_SCHEMA_VERSION, ObligationRuleSet, ObservedTurnDecision,
        OpenWorkObligation, PacketSafety, ParticipantMembership, ProjectId, ProjectMemoryFull,
        ProjectMemoryList, ProjectMemoryListRow, ProjectMemoryMutationReceipt,
        ProjectPolicyAuthorityDecision, ProjectPolicyEpoch, ProjectPolicyOperation,
        RememberProjectMemoryRequest, SCHEMA_VERSION, Scope, Sensitivity, SessionId, SessionPhase,
        TaskAdmissionEpoch, TaskBindReceipt, TaskClaimEvent, TaskDelta, TaskId, TaskJoinedEvent,
        TaskLease, TaskStartedEvent, TaskState, TurnBeginDecision, TurnBeginReceipt,
        TurnBeginSnapshot, TurnCheckpointDecision, TurnCheckpointEvent, TurnCheckpointReceipt,
        TurnCheckpointSnapshot, TurnDecision, TurnEvaluationInput, TurnGrantState,
        TurnGrantSupersession, TurnGrantSupersessionReason, TurnIntent, TurnNextIntent,
        VerificationEvidence, VerificationEvidenceInput, VerificationKind, VerificationResult,
        WorkCompletionRecoveryCause, WorkLease, WorkLeaseDecision, WorkLeaseEvent,
        WorkLeaseReleaseReceipt, WorkLeaseTransition, WorkReferenceCandidate,
    },
    memory::{DevelopmentNoopRedactor, Redactor, activation_policy, classify_note},
    schema::{
        CONTROL_POLICY_AUTHORITY_SCHEMA_VERSION,
        CONTROL_POLICY_OPERATION_FINGERPRINT_SCHEMA_VERSION, CONTROL_POLICY_SCHEMA_VERSION,
        CONTROL_POLICY_STATE_SCHEMA_VERSION, WORK_LEASE_ACQUIRE_FINGERPRINT_SCHEMA_VERSION,
    },
};

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SchemaOwner {
    Core,
    Work,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum SchemaDurability {
    Durable,
    Rebuildable,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
struct SchemaDefinition {
    object_type: String,
    name: String,
    sql: String,
}

// `project_memory_advertisements` is discardable delivery bookkeeping rather
// than canonical state: explicit projection repair may drop it and cause one
// harmless reannouncement. `project_memory_state` is reconstructed from
// verified project-memory versions and assertion events.
const CORE_REBUILDABLE_SCHEMA_OBJECTS: &[&str] = &[
    "object_fts",
    "objects_memory_assertion_version",
    "objects_project_memory_key",
    "objects_graph_snapshot_audit",
    "objects_graph_snapshot_load_audit",
    "memory_heads_scope",
    "memory_heads_work_scope",
    "project_memory_state",
    "project_memory_advertisements",
    "memory_contradictions_versions",
    "memory_contradiction_edges_context",
    "task_changes_task_cursor",
    "control_observations_session_sequence",
    "control_sessions_work_run",
    "control_work_leases_task_state",
];

const DIFFERENT_BUILD_STORE_MESSAGE: &str =
    "the store was created by a different Engram build; restore a current backup or re-initialize";

static CURRENT_SCHEMA_REFERENCE: std::sync::OnceLock<Vec<SchemaDefinition>> =
    std::sync::OnceLock::new();

std::thread_local! {
    static BUILDING_SCHEMA_REFERENCE: std::cell::Cell<bool> = const { std::cell::Cell::new(false) };
}

struct SchemaReferenceBuildGuard;

impl SchemaReferenceBuildGuard {
    fn enter() -> Result<Self, StoreError> {
        let already_building = BUILDING_SCHEMA_REFERENCE.with(|state| state.replace(true));
        if already_building {
            return Err(StoreError::InvalidControlProjection(
                "recursive current-schema reference construction".into(),
            ));
        }
        Ok(Self)
    }
}

impl Drop for SchemaReferenceBuildGuard {
    fn drop(&mut self) {
        BUILDING_SCHEMA_REFERENCE.with(|state| state.set(false));
    }
}

#[cfg(test)]
fn building_schema_reference() -> bool {
    BUILDING_SCHEMA_REFERENCE.with(std::cell::Cell::get)
}

fn normalized_schema_definition(sql: &str) -> String {
    sql.split_whitespace().collect::<Vec<_>>().join(" ")
}

pub(super) fn different_build_store_error() -> StoreError {
    StoreError::InvalidControlProjection(DIFFERENT_BUILD_STORE_MESSAGE.into())
}

pub(super) fn require_current_schema_marker(stored: i64, current: i64) -> Result<(), StoreError> {
    if stored == current {
        Ok(())
    } else {
        Err(different_build_store_error())
    }
}

fn stored_schema_definitions(connection: &Connection) -> Result<Vec<SchemaDefinition>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT type, name, sql
         FROM sqlite_schema
         WHERE name NOT LIKE 'sqlite_%' AND sql IS NOT NULL
         ORDER BY type, name",
    )?;
    let rows = statement.query_map([], |row| {
        Ok(SchemaDefinition {
            object_type: row.get(0)?,
            name: row.get(1)?,
            sql: normalized_schema_definition(&row.get::<_, String>(2)?),
        })
    })?;
    Ok(rows.collect::<Result<Vec<_>, _>>()?)
}

fn current_schema_reference() -> Result<&'static [SchemaDefinition], StoreError> {
    if let Some(reference) = CURRENT_SCHEMA_REFERENCE.get() {
        return Ok(reference);
    }
    let guard = SchemaReferenceBuildGuard::enter()?;
    let store = SqliteStore::open_in_memory_with_host_path_identity(None)?;
    let reference = stored_schema_definitions(&store.connection)?;
    drop(store);
    drop(guard);
    let _ = CURRENT_SCHEMA_REFERENCE.set(reference);
    CURRENT_SCHEMA_REFERENCE
        .get()
        .map(Vec::as_slice)
        .ok_or_else(|| {
            StoreError::InvalidControlProjection(
                "current-schema reference was not initialized".into(),
            )
        })
}

fn schema_object_matches_owner(definition: &SchemaDefinition, owner: SchemaOwner) -> bool {
    let work_owned = work::owns_schema_object(&definition.name);
    matches!(
        (owner, work_owned),
        (SchemaOwner::Core, false) | (SchemaOwner::Work, true)
    )
}

fn schema_object_matches_durability(
    definition: &SchemaDefinition,
    durability: SchemaDurability,
) -> bool {
    let rebuildable = if work::owns_schema_object(&definition.name) {
        work::is_rebuildable_schema_object(&definition.name)
    } else {
        definition.name == "object_fts"
            || definition.name.starts_with("object_fts_")
            || CORE_REBUILDABLE_SCHEMA_OBJECTS.contains(&definition.name.as_str())
    };
    matches!(
        (durability, rebuildable),
        (SchemaDurability::Durable, false) | (SchemaDurability::Rebuildable, true)
    )
}

pub(super) fn current_schema_definition_issue(
    connection: &Connection,
    owner: SchemaOwner,
    durability: SchemaDurability,
) -> Result<Option<String>, StoreError> {
    let expected = current_schema_reference()?
        .iter()
        .filter(|definition| schema_object_matches_owner(definition, owner))
        .filter(|definition| schema_object_matches_durability(definition, durability))
        .cloned()
        .collect::<Vec<_>>();
    let actual = stored_schema_definitions(connection)?
        .into_iter()
        .filter(|definition| schema_object_matches_owner(definition, owner))
        .filter(|definition| schema_object_matches_durability(definition, durability))
        .collect::<Vec<_>>();
    if actual == expected {
        return Ok(None);
    }

    for definition in &expected {
        match actual
            .iter()
            .find(|candidate| candidate.name == definition.name)
        {
            None => {
                return Ok(Some(format!(
                    "missing required {} {}",
                    definition.object_type, definition.name
                )));
            }
            Some(candidate) if candidate != definition => {
                return Ok(Some(format!(
                    "{} {} has a different definition",
                    definition.object_type, definition.name
                )));
            }
            Some(_) => {}
        }
    }
    let expected_names = expected
        .iter()
        .map(|definition| definition.name.as_str())
        .collect::<HashSet<_>>();
    if let Some(unexpected) = actual
        .iter()
        .find(|definition| !expected_names.contains(definition.name.as_str()))
    {
        return Ok(Some(format!(
            "unexpected {} {}",
            unexpected.object_type, unexpected.name
        )));
    }
    Ok(Some(
        "schema definitions differ from the current build".into(),
    ))
}

fn stored_schema_definition(
    connection: &Connection,
    object: &str,
) -> Result<Option<(String, String, String)>, StoreError> {
    connection
        .query_row(
            "SELECT type, name, sql FROM sqlite_schema WHERE name = ?1",
            [object],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()
        .map_err(StoreError::from)
}

pub(super) fn drop_schema_object(connection: &Connection, name: &str) -> Result<bool, StoreError> {
    let Some((object_type, _, _)) = stored_schema_definition(connection, name)? else {
        return Ok(false);
    };
    let drop_kind = match object_type.as_str() {
        "table" => "TABLE",
        "index" => "INDEX",
        "trigger" => "TRIGGER",
        "view" => "VIEW",
        other => {
            return Err(StoreError::InvalidControlProjection(format!(
                "cannot replace schema object {name} with unsupported type {other}"
            )));
        }
    };
    let quoted = name.replace('"', "\"\"");
    connection.execute_batch(&format!("DROP {drop_kind} \"{quoted}\";"))?;
    Ok(true)
}

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
    evidence: &'a [ObjectHash],
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

pub(crate) struct BeginWorkProtocolAttempt<'a, T, B> {
    pub(crate) project_id: &'a crate::domain::ProjectId,
    pub(crate) session_id: &'a SessionId,
    pub(crate) operation: &'a str,
    pub(crate) idempotency_key: &'a str,
    pub(crate) intent: &'a T,
    pub(crate) basis: &'a B,
    pub(crate) now: DateTime<Utc>,
}

pub(crate) struct BeginGateWorkProtocolAttempt<'a, B> {
    pub(crate) project_id: &'a crate::domain::ProjectId,
    pub(crate) session_id: &'a SessionId,
    pub(crate) basis: &'a B,
    pub(crate) now: DateTime<Utc>,
}

#[derive(Serialize)]
struct ContradictionIntentFingerprint<'a> {
    project_id: &'a crate::domain::ProjectId,
    task_id: Option<TaskId>,
    work_id: Option<crate::domain::WorkId>,
    work_root_id: Option<crate::domain::WorkId>,
    left_version: &'a ObjectHash,
    right_version: &'a ObjectHash,
    reason: &'a str,
    actor: &'a ActorContext,
}

#[derive(Serialize)]
struct TurnObservationIntentFingerprint<'a> {
    control_schema_version: u16,
    session_id: &'a SessionId,
    task_id: Option<TaskId>,
    intent: &'a TurnIntent,
}

#[derive(Serialize)]
struct ControlSessionBindFingerprint<'a> {
    control_schema_version: u16,
    project_id: &'a crate::domain::ProjectId,
    external_ref: &'a str,
    title: &'a str,
    session_id: &'a SessionId,
    actor: &'a ActorContext,
    assurance: ControlAssurance,
    mediated_effects: &'a [EffectClass],
    #[serde(skip_serializing_if = "Option::is_none")]
    work_binding: Option<&'a ControlWorkBinding>,
    capability_map_revision: i64,
    idempotency_key: &'a str,
}

#[derive(Serialize)]
struct ControlTurnBeginFingerprint<'a> {
    control_schema_version: u16,
    session_id: &'a SessionId,
    grant_id: &'a str,
    delivery_tokens: &'a [String],
    idempotency_key: &'a str,
}

#[derive(Serialize)]
struct ControlTurnCheckpointFingerprint<'a> {
    control_schema_version: u16,
    session_id: &'a SessionId,
    grant_id: &'a str,
    next_intent: TurnNextIntent,
    #[serde(skip_serializing_if = "execution_observations_are_empty")]
    observations: &'a [ExecutionObservationInput],
    #[serde(skip_serializing_if = "verification_evidence_inputs_are_empty")]
    verification_evidence: &'a [VerificationEvidenceInput],
    #[serde(skip_serializing_if = "environment_evidence_inputs_are_empty")]
    environment_evidence: &'a [EnvironmentEvidenceInput],
    idempotency_key: &'a str,
}

fn execution_observations_are_empty(value: &&[ExecutionObservationInput]) -> bool {
    value.is_empty()
}

fn verification_evidence_inputs_are_empty(value: &&[VerificationEvidenceInput]) -> bool {
    value.is_empty()
}

fn environment_evidence_inputs_are_empty(value: &&[EnvironmentEvidenceInput]) -> bool {
    value.is_empty()
}

#[derive(Serialize)]
struct WorkLeaseAcquireFingerprint<'a> {
    fingerprint_schema_version: u16,
    session_id: &'a SessionId,
    bind_intent_hash: &'a str,
    kind: crate::domain::LeaseKind,
    mode: crate::domain::LeaseMode,
    subject: &'a crate::domain::ResourceSubject,
    ttl_seconds: i64,
    idempotency_key: &'a str,
}

#[derive(Serialize)]
struct WorkLeaseReleaseFingerprint<'a> {
    control_schema_version: u16,
    session_id: &'a SessionId,
    lease_id: &'a str,
    idempotency_key: &'a str,
}

#[derive(Serialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
enum ControlPolicyOperationFingerprint<'a> {
    SetRequiredAssurance {
        fingerprint_schema_version: u16,
        idempotency_key: &'a str,
        required_assurance: ControlAssurance,
        authorized_by: &'a ActorContext,
        reason: &'a str,
        expected_policy: Option<&'a ObjectHash>,
    },
    SetObligationRuleSet {
        fingerprint_schema_version: u16,
        idempotency_key: &'a str,
        obligation_rule_set: &'a ObjectHash,
        authorized_by: &'a ActorContext,
        reason: &'a str,
        expected_policy: Option<&'a ObjectHash>,
    },
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
    #[error("project memory key {0:?} already exists")]
    ProjectMemoryExists(String),
    #[error("project memory key {0:?} is permanently retired")]
    ProjectMemoryRetired(String),
    #[error("project memory key {0:?} was not found")]
    ProjectMemoryNotFound(String),
    #[error("the asserted actor/session binding for project memories is absent or inconsistent")]
    ProjectMemoryBindingInvalid,
    #[error("project memory input is invalid: {0}")]
    InvalidProjectMemory(String),
    #[error("caller is not authorized to explain context packet {0}")]
    PacketAccessDenied(ObjectHash),
    #[error("turn observation idempotency key {0:?} was reused for a different intent")]
    TurnObservationIdempotencyConflict(String),
    #[error("control observation projection contains invalid data: {0}")]
    InvalidControlObservation(String),
    #[error("control session input is invalid: {0}")]
    InvalidControlSession(String),
    #[error(
        "the project root's filesystem identity is unresolved, so path leases are refused; pass --host-path-policy case_fold|case_sensitive or set ENGRAM_HOST_PATH_POLICY"
    )]
    HostPathIdentityUnresolved,
    #[error("session {0:?} has no host-private control binding")]
    ControlSessionNotBound(String),
    #[error("routing token does not match control session {0:?}")]
    ControlSessionTokenMismatch(String),
    #[error("host control connection for session {0:?} was superseded")]
    ControlConnectionSuperseded(String),
    #[error("control session bind key {0:?} was reused for a different intent")]
    ControlSessionBindConflict(String),
    #[error("turn request key {0:?} was reused for a different intent")]
    ControlTurnIdempotencyConflict(String),
    #[error("control operation {operation} key {key:?} was reused for a different intent")]
    ControlOperationIdempotencyConflict { operation: String, key: String },
    #[error("control work binding for {work:?} is stale; reread the live claim and rebind")]
    ControlWorkBindingStale { work: crate::domain::WorkId },
    #[error("execution observation {observation_id:?} is outside the turn grant scope")]
    ControlGrantScopeMismatch { observation_id: String },
    #[error("execution observation {observation_id:?} does not match the bound work scope")]
    ControlObservationScopeMismatch { observation_id: String },
    #[error("verification producer observation {0:?} cannot be resolved for this checkpoint")]
    VerificationProducerObservationNotFound(String),
    #[error("environment fingerprint does not match the canonical component identity")]
    EnvironmentFingerprintMismatch,
    #[error("environment evidence {0:?} cannot be resolved for this checkpoint")]
    EnvironmentEvidenceNotFound(String),
    #[error("environment evidence {0:?} does not match the verification run/source basis")]
    EnvironmentBasisMismatch(String),
    #[error("turn grant {0:?} does not exist")]
    ControlTurnGrantNotFound(String),
    #[error("work lease {0:?} does not exist")]
    WorkLeaseNotFound(String),
    #[error("work lease {lease_id:?} is not held by session {session:?}")]
    WorkLeaseNotHeld { lease_id: String, session: String },
    #[error("work lease {lease_id:?} expired at {expired_at}")]
    WorkLeaseExpired {
        lease_id: String,
        expired_at: DateTime<Utc>,
    },
    #[error("control projection contains invalid data: {0}")]
    InvalidControlProjection(String),
    #[error("active control policy changed: expected {expected}, current policy is {current}")]
    ControlPolicyConflict {
        expected: ObjectHash,
        current: ObjectHash,
    },
    #[error("pinned context requires {required} bytes, exceeding the {budget}-byte budget")]
    PinnedBudgetExceeded { required: usize, budget: usize },
    #[error("local work item {0:?} does not exist")]
    WorkNotFound(crate::domain::WorkId),
    #[error("local work input is invalid: {0}")]
    InvalidWork(String),
    #[error(
        "work reference {reference:?} is ambiguous; use a full work id for one of {candidates:?}; {more} additional candidates omitted"
    )]
    WorkReferenceAmbiguous {
        reference: String,
        candidates: Vec<WorkReferenceCandidate>,
        more: usize,
    },
    #[error("work projection contains invalid data: {0}")]
    InvalidWorkProjection(String),
    #[error(
        "graph_destination_not_empty: destination project already contains work or project memory"
    )]
    GraphDestinationNotEmpty,
    #[error(
        "graph_project_mismatch: snapshot project {snapshot:?} does not match destination {destination:?}"
    )]
    GraphProjectMismatch {
        snapshot: ProjectId,
        destination: ProjectId,
    },
    #[error("snapshot format differs from this Engram build; use the build that wrote the file")]
    GraphDifferentBuild,
    #[error("graph_snapshot_corrupt: {0}")]
    InvalidGraphSnapshot(String),
    #[error(
        "work revision changed for {work:?}: expected {expected}, current revision is {current}"
    )]
    WorkRevisionConflict {
        work: crate::domain::WorkId,
        expected: i64,
        current: i64,
    },
    #[error("work operation {operation} key {key:?} was reused for a different intent")]
    WorkOperationIdempotencyConflict { operation: String, key: String },
    #[error("work completion dependency graph would contain a cycle")]
    WorkDependencyCycle,
    #[error("work prerequisite {0:?} is already completed; no edge is needed")]
    WorkPrerequisiteAlreadySatisfied(crate::domain::WorkId),
    #[error("work {0:?} is not open for this operation")]
    WorkNotOpen(crate::domain::WorkId),
    #[error(
        "cannot add beneath {lifecycle:?} work; {}", parent_not_open_remedy(*.lifecycle)
    )]
    WorkParentNotOpen {
        parent: crate::domain::WorkId,
        lifecycle: crate::domain::WorkLifecycle,
    },
    #[error(
        "a peer may propose only optional children without prerequisites beneath held work; ask the parent holder to add required children or prerequisites"
    )]
    WorkPeerDecompositionRefused { parent: crate::domain::WorkId },
    #[error("work {work:?} is claimed by session {holder} until {expires_at}")]
    WorkClaimHeld {
        work: crate::domain::WorkId,
        holder: String,
        expires_at: i64,
    },
    #[error("claim authority for work {work:?} is stale or does not match the holder")]
    WorkClaimMismatch { work: crate::domain::WorkId },
    #[error("claim for work {work:?} lapsed at {expired_at}")]
    WorkClaimLapsed {
        work: crate::domain::WorkId,
        expired_at: DateTime<Utc>,
    },
    #[error("completion for work {work:?} was refused: {reason}")]
    WorkCompletionRefused {
        work: crate::domain::WorkId,
        reason: String,
    },
    #[error("completion for work {work:?} requires recovery: {cause:?}")]
    WorkCompletionRecoveryRequired {
        work: crate::domain::WorkId,
        cause: WorkCompletionRecoveryCause,
    },
    #[error("completion for work {work:?} has open work obligations")]
    OpenWorkObligations {
        work: crate::domain::WorkId,
        obligations: Vec<OpenWorkObligation>,
        omitted_count: usize,
    },
}

/// Result of scanning every immutable object in the store.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct IntegrityReport {
    pub checked_objects: usize,
    pub invalid_objects: Vec<String>,
    pub checked_graph_snapshot_audits: usize,
    pub invalid_graph_snapshot_audits: Vec<String>,
    pub checked_control_records: usize,
    pub invalid_control_records: Vec<String>,
    pub checked_work_records: usize,
    pub invalid_work_records: Vec<String>,
}

/// One invalid binding found by diagnostics-only control-policy recovery.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ControlPolicyRecoveryFinding {
    /// Stable projection identity suitable for operator correlation.
    pub record: String,
    /// Exact verification failure; this is guidance, never a repair action.
    pub detail: String,
}

/// Read-only control-policy report for a store that ordinary open refuses.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct ControlPolicyRecoveryReport {
    pub checked_control_records: usize,
    pub invalid_control_records: Vec<ControlPolicyRecoveryFinding>,
    /// Recovery deliberately restores verified bytes; it never selects or
    /// rewrites a policy head on the operator's behalf.
    pub guidance: String,
}

/// Operator-facing summary of the currently enforceable control envelope.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ControlDiagnostics {
    pub control_schema_version: u16,
    pub active_policy: ObjectHash,
    pub policy_epoch: ProjectPolicyEpoch,
    pub required_assurance: ControlAssurance,
    pub supported_effects: Vec<EffectClass>,
    pub obligation_rule_set: ObjectHash,
    pub unenforced_effects: Vec<EffectClass>,
    pub active_sessions: usize,
    pub issued_turns: usize,
    pub begun_turns: usize,
    pub action_gating_available: bool,
    pub authority_mediation_available: bool,
    pub action_outcome_tracking_available: bool,
}

/// Operator-facing receipt for one idempotent project control-policy update.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ControlPolicyUpdateReceipt {
    pub changed: bool,
    pub active_policy: ObjectHash,
    pub previous_policy: Option<ObjectHash>,
    pub authority: ObjectHash,
    pub policy_epoch: ProjectPolicyEpoch,
    pub previous_required_assurance: ControlAssurance,
    pub required_assurance: ControlAssurance,
    pub activated_at: DateTime<Utc>,
}

/// Operator-facing receipt for one immutable obligation rule-set activation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ObligationRuleSetUpdateReceipt {
    pub changed: bool,
    pub active_policy: ObjectHash,
    pub previous_policy: Option<ObjectHash>,
    pub authority: ObjectHash,
    pub policy_epoch: ProjectPolicyEpoch,
    pub previous_rule_set: Option<ObjectHash>,
    pub obligation_rule_set: ObjectHash,
    pub activated_at: DateTime<Utc>,
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
    Option<String>,
    String,
    String,
    String,
    i64,
);

#[derive(Debug, Eq, PartialEq)]
struct MemoryHeadProjectionRow {
    memory_id: String,
    version_hash: String,
    assertion_hash: String,
    schema_version: i64,
    status: String,
    scope_kind: String,
    project_id: String,
    task_id: Option<String>,
    work_id: Option<String>,
    agent_id: Option<String>,
    memory_kind: String,
    authority: String,
    delivery: String,
    sensitivity: String,
    title: String,
    body: String,
    created_at_ms: i64,
}

struct PreparedNote {
    version: MemoryVersion,
    assertion: MemoryAssertionEvent,
    version_object: CanonicalObject,
    assertion_object: CanonicalObject,
}

struct StoredProjectMemory {
    version_hash: ObjectHash,
    version: MemoryVersion,
    assertion: MemoryAssertionEvent,
}

struct PreparedProjectMemory {
    version: MemoryVersion,
    assertion: MemoryAssertionEvent,
    version_object: CanonicalObject,
    assertion_object: CanonicalObject,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum MemoryProjectionMode {
    Live,
    Replay,
}

#[derive(Clone, Debug)]
pub(crate) struct ProjectMemoryAdvertisement {
    pub count: usize,
    pub changed: bool,
    change_position: i64,
    context_generation_digest: Option<String>,
}

const PROJECT_MEMORY_LIST_LIMIT: usize = 20;
const MAX_PROJECT_MEMORY_ADVERTISEMENTS_PER_PROJECT: i64 = 1_024;
const PROJECT_MEMORY_FIRST_LINE_BYTES: usize = 160;
const MAX_CONTEXT_GENERATION_BYTES: usize = 256;
const MAX_PROJECT_MEMORY_ATTRIBUTION_TEXT_BYTES: usize = 4_096;
const MAX_PROJECT_MEMORY_PROVENANCE_LINKS: usize = 32;
const MAX_PROJECT_MEMORY_ATTRIBUTION_BYTES: usize = 64 * 1_024;

const PINNED_CONTEXT_BUDGET: usize = 4 * 1_024;
const INDEX_CONTEXT_BUDGET: usize = 8 * 1_024;
const MAX_CONTROL_DELIVERY_EVENTS: i64 = 128;
const MAX_CONTROL_DELIVERY_OBJECT_BYTES: i64 = 128 * 1_024;
const MAX_CONTROL_DELIVERY_BYTES: usize = 256 * 1_024;
const MAX_EXECUTION_OBSERVATIONS_PER_CHECKPOINT: usize = 64;
const MAX_VERIFICATION_EVIDENCE_PER_CHECKPOINT: usize = 16;
const MAX_ENVIRONMENT_EVIDENCE_PER_CHECKPOINT: usize = 4;
const MAX_TYPED_EVIDENCE_SUMMARY_BYTES: usize = 4 * 1_024;
const MAX_TYPED_EVIDENCE_REFS: usize = 64;
const MAX_TYPED_EVIDENCE_REF_BYTES: usize = 1_024;
const MAX_TASK_CHANGE_OBJECT_BYTES: usize = 64 * 1_024;
const MAX_EXACT_CONTEXT_OMISSIONS: usize = 128;
const BUILTIN_CONTROL_GRANT_TTL_SECONDS: i64 = 30;
const MAX_CONTROL_POLICY_PROVENANCE_LINKS: usize = 32;
const MAX_CONTROL_POLICY_ATTRIBUTION_BYTES: usize = 64 * 1_024;
const MAX_CONTROL_POLICY_AUTHORITY_BYTES: usize = 72 * 1_024;
const MAX_CONTROL_POLICY_OPERATION_INTENT_BYTES: usize = 96 * 1_024;
const MAX_CONTROL_POLICY_OPERATION_RESULT_BYTES: usize = 16 * 1_024;
const MAX_CONTROL_POLICY_IDEMPOTENCY_KEY_BYTES: usize = 512;

#[cfg(test)]
thread_local! {
    static CONTROL_POLICY_VERSION_LOAD_COUNT: Cell<usize> = const { Cell::new(0) };
    static FAIL_COLD_SCHEMA_AFTER_DDL: Cell<bool> = const { Cell::new(false) };
}

#[cfg(test)]
fn reset_control_policy_version_load_count() {
    CONTROL_POLICY_VERSION_LOAD_COUNT.set(0);
}

#[cfg(test)]
fn control_policy_version_load_count() -> usize {
    CONTROL_POLICY_VERSION_LOAD_COUNT.get()
}

#[cfg(test)]
fn fail_cold_schema_after_ddl() -> bool {
    FAIL_COLD_SCHEMA_AFTER_DDL.replace(false)
}

struct ContextAssembly {
    pinned: Vec<ContextItem>,
    index: Vec<ContextItem>,
    omissions: Vec<ContextOmission>,
    omission_summaries: Vec<ContextOmissionSummary>,
    proposed_count: u32,
    stale_count: u32,
}

struct StoredControlObservation {
    sequence: i64,
    session_id: String,
    task_id: Option<String>,
    idempotency_key: String,
    intent_hash: String,
    observed_at_ms: i64,
    input_hash: String,
    input_json: Vec<u8>,
    decision_hash: String,
    decision_json: Vec<u8>,
}

struct StoredControlSession {
    project_id: crate::domain::ProjectId,
    task_id: TaskId,
    work_binding: Option<ControlWorkBinding>,
    session_id: SessionId,
    routing_token: String,
    actor: ActorContext,
    bind_key: String,
    bind_intent_hash: String,
    phase: SessionPhase,
    assurance: ControlAssurance,
    mediated_effects: Vec<EffectClass>,
    confirmed_cursor: ChangeCursor,
    tentative_cursor: Option<ChangeCursor>,
    epochs: ControlEpochs,
    blocking_watermark: ChangeCursor,
    capability_map_revision: i64,
    revision: i64,
    open_grant_id: Option<String>,
}

struct RawControlSession {
    project_id: String,
    task_id: String,
    root_execution_id: Option<String>,
    work_id: Option<String>,
    run_id: Option<String>,
    work_revision: Option<i64>,
    claim_id: Option<String>,
    claim_fence: Option<i64>,
    routing_token: String,
    actor_json: Vec<u8>,
    bind_key: String,
    bind_intent_hash: String,
    bind_intent_json: Vec<u8>,
    phase: String,
    assurance: String,
    mediated_effects_json: String,
    confirmed_cursor: i64,
    tentative_cursor: Option<i64>,
    project_policy_epoch: i64,
    task_admission_epoch: i64,
    blocking_watermark: i64,
    capability_map_revision: i64,
    revision: i64,
    open_grant_id: Option<String>,
}

struct ControlPolicyProjection {
    state_schema_version: i64,
    policy_hash: ObjectHash,
    authority_hash: ObjectHash,
    epoch: ProjectPolicyEpoch,
    required_assurance: ControlAssurance,
    supported_effects: Vec<EffectClass>,
    grant_ttl_seconds: i64,
    obligation_rule_set: ObjectHash,
    activated_at: DateTime<Utc>,
}

struct InitialControlPolicy {
    required_assurance: ControlAssurance,
    authorized_by: ActorContext,
    reason: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OpenWriteNeed {
    Current,
    NeedsWrite,
}

struct StoredTurnGrant {
    grant: IssuedTurnGrant,
    state: TurnGrantState,
}

fn safely_redeliverable_partial_recovery(grant: &IssuedTurnGrant) -> bool {
    matches!(grant.basis.purpose, crate::domain::TurnPurpose::Recovery)
        && !grant.basis.requested_effects.is_empty()
        && grant
            .basis
            .requested_effects
            .iter()
            .all(|effect| matches!(effect, EffectClass::Observe))
        && grant
            .delivery
            .as_ref()
            .is_some_and(|delivery| delivery.page.has_more)
        && crate::control::delivery_matches_grant(grant)
}

struct StoredControlTurnResult {
    sequence: i64,
    session_id: String,
    task_id: String,
    idempotency_key: String,
    intent_hash: String,
    intent_json: Vec<u8>,
    decision_hash: String,
    decision_json: Vec<u8>,
}

struct StoredControlGrantRow {
    grant_id: String,
    session_id: String,
    task_id: String,
    request_key: String,
    grant_hash: String,
    grant_json: Vec<u8>,
    state: String,
    issued_at_ms: i64,
    expires_at_ms: i64,
}

struct PendingTurnGrantSupersession {
    grant_id: String,
    request_key: String,
}

struct StoredTurnGrantSupersession {
    superseded_grant_id: String,
    session_id: String,
    task_id: String,
    replacement_request_key: String,
    replacement_decision_hash: String,
    supersession_hash: String,
    supersession_json: Vec<u8>,
    superseded_at_ms: i64,
}

struct StoredControlOperation {
    sequence: i64,
    session_id: String,
    operation: String,
    idempotency_key: String,
    intent_hash: String,
    intent_json: Vec<u8>,
    result_hash: String,
    result_json: Vec<u8>,
}

struct StoredControlPolicyOperation {
    sequence: i64,
    operation: String,
    idempotency_key: String,
    intent_hash: String,
    intent_json: Vec<u8>,
    result_hash: String,
    result_json: Vec<u8>,
}

struct StoredWorkLeaseRow {
    lease_id: String,
    task_id: String,
    holder_session_id: String,
    lease_hash: String,
    lease_json: Vec<u8>,
    state: String,
    expires_at_ms: i64,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct ApplicableContradiction {
    contradiction: ObjectHash,
    left: ObjectHash,
    right: ObjectHash,
}

struct AuthorizedContradiction {
    left: ObjectHash,
    right: ObjectHash,
    reason: String,
    task_id: Option<TaskId>,
    /// The caller's own work anchor, kept for the idempotency fingerprint so a
    /// retry of an omitted-work request still replays.
    work_id: Option<crate::domain::WorkId>,
    /// The work whose feeds receive the event: the caller's anchor, or the
    /// validated focus when the caller omitted it.
    feed_work_id: Option<crate::domain::WorkId>,
    work_root_id: Option<crate::domain::WorkId>,
}

impl IntegrityReport {
    /// Whether every stored object passed canonicalization and digest checks.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.invalid_objects.is_empty()
            && self.invalid_graph_snapshot_audits.is_empty()
            && self.invalid_control_records.is_empty()
            && self.invalid_work_records.is_empty()
    }
}

impl ControlPolicyRecoveryReport {
    /// Whether the active selector and every reachable policy record verify.
    #[must_use]
    pub fn is_healthy(&self) -> bool {
        self.invalid_control_records.is_empty()
    }
}

/// Human-readable form of a host path policy for diagnostics and refusals.
#[must_use]
pub fn describe_host_path_policy(policy: HostPathPolicy) -> String {
    format!(
        "{}, windows alias rules {}",
        if policy.case_fold_paths {
            "case_fold"
        } else {
            "case_sensitive"
        },
        if policy.windows_alias_rules {
            "on"
        } else {
            "off"
        }
    )
}

/// What a verified backup copy contains. A backup is a full copy of the
/// store, including host-private state and private scratch, so it is exactly as
/// sensitive as the store itself.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct BackupManifest {
    pub path: std::path::PathBuf,
    /// SHA-256 of the backup file bytes after verification.
    pub file_sha256: String,
    pub file_bytes: u64,
    pub checked_objects: usize,
    pub checked_control_records: usize,
    pub checked_work_records: usize,
    pub created_at: DateTime<Utc>,
}

/// A sibling path only this process will use, for staging a file before it
/// is published under its final name.
fn unique_sibling_path(path: &Path, label: &str) -> std::path::PathBuf {
    let nanos = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default();
    let name = path.file_name().map_or_else(
        || "store".into(),
        |name| name.to_string_lossy().into_owned(),
    );
    path.with_file_name(format!(
        ".{name}.{label}-{}-{nanos}.tmp",
        std::process::id()
    ))
}

/// Publishes a staged file under `target` without replacing anything: the
/// final name is created exclusively, the staged bytes are copied in and
/// flushed, and the staged file is removed. An existing `target` is an error
/// and leaves both files untouched; a failure while writing removes the
/// partial target so a retry is not blocked by it.
fn publish_without_replacing(staged: &Path, target: &Path) -> Result<(), StoreError> {
    let io_error = |what: &str, error: std::io::Error| {
        StoreError::InvalidWork(format!("cannot {what} {}: {error}", target.display()))
    };
    let mut out = std::fs::OpenOptions::new()
        .write(true)
        .create_new(true)
        .open(target)
        .map_err(|error| {
            if error.kind() == std::io::ErrorKind::AlreadyExists {
                StoreError::InvalidWork(format!(
                    "backup target {} already exists",
                    target.display()
                ))
            } else {
                io_error("create", error)
            }
        })?;
    let written = (|| -> std::io::Result<()> {
        let mut input = std::fs::File::open(staged)?;
        std::io::copy(&mut input, &mut out)?;
        out.sync_all()
    })();
    drop(out);
    if let Err(error) = written {
        let _ = std::fs::remove_file(target);
        return Err(io_error("write", error));
    }
    remove_store_files(staged).map_err(|error| io_error("clean up after", error))?;
    Ok(())
}

/// Installs a verified staged copy as `target` without replacing anything;
/// hosts use it for a restore into an absent store.
///
/// # Errors
///
/// Returns [`StoreError`] when `target` already exists or the copy fails.
pub fn install_store_copy_without_replacing(
    staged: &Path,
    target: &Path,
) -> Result<(), StoreError> {
    publish_without_replacing(staged, target)
}

/// The log sidecars SQLite may keep beside a store file.
fn store_sidecars(path: &Path) -> [std::path::PathBuf; 3] {
    let base = path.display().to_string();
    [
        std::path::PathBuf::from(format!("{base}-wal")),
        std::path::PathBuf::from(format!("{base}-shm")),
        std::path::PathBuf::from(format!("{base}-journal")),
    ]
}

/// Removes a store file and any log sidecars an open may have left beside it.
fn remove_store_files(path: &Path) -> std::io::Result<()> {
    std::fs::remove_file(path)?;
    for sidecar in store_sidecars(path) {
        if sidecar.exists() {
            std::fs::remove_file(&sidecar)?;
        }
    }
    Ok(())
}

/// A `file:` URI that opens `path` as an immutable database: SQLite then
/// reads the file bytes alone and never touches or creates log sidecars.
fn immutable_uri(path: &Path) -> Result<String, StoreError> {
    let absolute = std::path::absolute(path).map_err(|error| {
        StoreError::InvalidWork(format!("cannot resolve {}: {error}", path.display()))
    })?;
    let mut text = absolute.to_string_lossy().replace('\\', "/");
    if let Some(stripped) = text.strip_prefix("//?/") {
        text = stripped.to_owned();
    }
    let encoded = text
        .chars()
        .map(|character| match character {
            '%' => "%25".to_owned(),
            '?' => "%3F".to_owned(),
            '#' => "%23".to_owned(),
            other => other.to_string(),
        })
        .collect::<String>();
    Ok(format!(
        "file:///{}?immutable=1",
        encoded.trim_start_matches('/')
    ))
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

/// V1's canonical local persistence backend.
pub struct SqliteStore {
    connection: Connection,
    /// Local-work schema generation this connection opened and understands.
    /// Every work mutation compares it with durable metadata inside the write
    /// transaction so a process with a non-current view cannot write.
    work_schema_version: i64,
    /// The project root's filesystem identity for this opener. `None` means
    /// unresolved: reads and work proceed, path leases fail closed.
    host_path_policy: Option<HostPathPolicy>,
}
