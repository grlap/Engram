//! Local SQLite object store and integrity verification.

mod work;
pub(crate) use work::WorkObligationRecord;

pub(crate) use work::{StageWorkSessionDelivery, normalize_completion_acceptance_shape};

#[cfg(test)]
pub(crate) use work::{
    reset_work_event_decode_count, reset_work_item_projection_decode_count,
    work_event_decode_count, work_item_projection_decode_count,
};

use std::{
    collections::{HashMap, HashSet},
    path::Path,
    time::Duration,
};

#[cfg(test)]
use std::cell::Cell;

#[cfg(test)]
use crate::domain::ControlRefusalCode;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use thiserror::Error;

use crate::{
    CanonicalObject, ObjectHash,
    control::{LeasePolicyInput, effective_mediated_effects, evaluate_lease_policy},
    domain::{
        ActorContext, AssuranceLevel, CONTROL_SCHEMA_VERSION, ChangeCursor, ContextItem,
        ContextOmission, ContextOmissionSummary, ContextPacket, ContextPacketHeader,
        ContextPacketPayload, ControlAssurance, ControlDelivery, ControlEpochs, ControlHealth,
        ControlPolicy, ControlSessionBinding, ControlSessionStatus, ControlTurnBeginDecision,
        ControlTurnCheckpointDecision, ControlTurnDecision, ControlWorkBinding, Delivery,
        DeliveryPage, DeltaItem, EffectClass, EnvironmentComponents, EnvironmentEvidence,
        EnvironmentEvidenceInput, EnvironmentEvidenceReference, ExecutionObservation,
        ExecutionObservationInput, ExecutionObservationReference, ExecutionOutcome, HostPathPolicy,
        IssuedTurnGrant, LocalTask, MemoryAssertionEvent, MemoryContradictionEvent,
        MemoryContradictionReceipt, MemoryId, MemoryRecord, MemoryStatus, MemorySummary,
        MemoryVersion, NoteReceipt, NoteRequest, NoteVisibility,
        OBLIGATION_RULE_SET_SCHEMA_VERSION, ObligationRuleSet, ObservedTurnDecision,
        OpenWorkObligation, PacketSafety, ParticipantMembership, ProjectPolicyAuthorityDecision,
        ProjectPolicyEpoch, ProjectPolicyOperation, SCHEMA_VERSION, Scope, Sensitivity, SessionId,
        SessionPhase, TaskAdmissionEpoch, TaskBindReceipt, TaskClaimEvent, TaskDelta, TaskId,
        TaskJoinedEvent, TaskLease, TaskStartedEvent, TaskState, TurnBeginDecision,
        TurnBeginReceipt, TurnBeginSnapshot, TurnCheckpointDecision, TurnCheckpointEvent,
        TurnCheckpointReceipt, TurnCheckpointSnapshot, TurnDecision, TurnEvaluationInput,
        TurnGrantState, TurnIntent, TurnNextIntent, VerificationEvidence,
        VerificationEvidenceInput, VerificationKind, VerificationResult, WorkAuthorityOperation,
        WorkAuthorityScope, WorkLease, WorkLeaseDecision, WorkLeaseEvent, WorkLeaseReleaseReceipt,
        WorkLeaseTransition,
    },
    memory::{DevelopmentNoopRedactor, Redactor, activation_policy, classify_note},
};

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
    #[error(
        "backup was written under an older store schema{}; restore migrates a staged copy before installing it",
        from_version.map(|version| format!(" (work schema {version})")).unwrap_or_default()
    )]
    BackupNeedsMigration { from_version: Option<i64> },
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
    #[error("work projection contains invalid data: {0}")]
    InvalidWorkProjection(String),
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
    #[error("work {0:?} is not open for this operation")]
    WorkNotOpen(crate::domain::WorkId),
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
    pub checked_control_records: usize,
    pub invalid_control_records: Vec<String>,
    pub checked_work_records: usize,
    pub invalid_work_records: Vec<String>,
    /// Valid immutable records written before a currently enforceable schema.
    pub legacy_work_records: Vec<String>,
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

/// Host-facing installation and revocation status for one work-authority hash.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
pub struct WorkAuthorityGrantStatus {
    pub installed: bool,
    pub subject_actor_id: Option<String>,
    pub issued_by: Option<String>,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_until: Option<DateTime<Utc>>,
    pub revoked_at: Option<DateTime<Utc>>,
    pub operations: Option<Vec<WorkAuthorityOperation>>,
    pub scope: Option<WorkAuthorityScope>,
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

type LegacyContradictionRow = (String, String, String, String, String);

struct PreparedNote {
    version: MemoryVersion,
    assertion: MemoryAssertionEvent,
    version_object: CanonicalObject,
    assertion_object: CanonicalObject,
}

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
const CONTROL_POLICY_STATE_SCHEMA_VERSION: i64 = 4;
const LEGACY_REPLAYLESS_CONTROL_POLICY_STATE_SCHEMA_VERSION: i64 = 3;
const LEGACY_VERSIONED_CONTROL_POLICY_STATE_SCHEMA_VERSION: i64 = 2;
const CONTROL_POLICY_SCHEMA_VERSION_V1: u16 = 1;
const CONTROL_POLICY_AUTHORITY_SCHEMA_VERSION_V1: u16 = 1;
const CONTROL_POLICY_CONTROL_SCHEMA_VERSION_V1: u16 = 1;
const BUILTIN_CONTROL_GRANT_TTL_SECONDS: i64 = 30;
const CONTROL_POLICY_V1_MAX_GRANT_TTL_SECONDS: i64 = 300;
const MAX_CONTROL_GRANT_TTL_SECONDS: i64 = CONTROL_POLICY_V1_MAX_GRANT_TTL_SECONDS;
const MAX_CONTROL_POLICY_PROVENANCE_LINKS: usize = 32;
const MAX_CONTROL_POLICY_ATTRIBUTION_BYTES: usize = 64 * 1_024;
const MAX_CONTROL_POLICY_AUTHORITY_BYTES: usize = 72 * 1_024;
const MAX_CONTROL_POLICY_OPERATION_INTENT_BYTES: usize = 96 * 1_024;
const MAX_CONTROL_POLICY_OPERATION_RESULT_BYTES: usize = 16 * 1_024;
const MAX_CONTROL_POLICY_IDEMPOTENCY_KEY_BYTES: usize = 512;
const CONTROL_POLICY_OPERATION_FINGERPRINT_SCHEMA_VERSION: u16 = 1;
const WORK_LEASE_ACQUIRE_FINGERPRINT_SCHEMA_VERSION: u16 = 2;

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
    obligation_rule_set: Option<ObjectHash>,
    activated_at: DateTime<Utc>,
}

struct InitialControlPolicy {
    required_assurance: ControlAssurance,
    authorized_by: ActorContext,
    reason: String,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum OpenMigrationNeed {
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
            && self.invalid_control_records.is_empty()
            && self.invalid_work_records.is_empty()
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
/// store, including host grants and private scratch, so it is exactly as
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

/// V1's canonical local persistence backend.
pub struct SqliteStore {
    connection: Connection,
    /// Local-work schema generation this connection opened and understands.
    /// Every work mutation compares it with durable metadata inside the write
    /// transaction so a long-lived older process cannot write after migration.
    work_schema_version: i64,
    /// The project root's filesystem identity for this opener. `None` means
    /// unresolved: reads and work proceed, path leases fail closed.
    host_path_policy: Option<HostPathPolicy>,
}

impl SqliteStore {
    /// Opens or creates a local database and applies idempotent schema setup
    /// under the running target's conservative path policy. Embedding hosts
    /// and the CLI should prefer [`Self::open_with_host_path_identity`] with
    /// the project root's probed or host-supplied identity.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when SQLite cannot open or initialize the store.
    pub fn open(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::open_with_host_path_identity(path, Some(HostPathPolicy::host_default()))
    }

    /// Opens a local database without asserting the project root's filesystem
    /// identity: work and memory operations proceed against any persisted
    /// policy, and path-bearing leases fail closed. Agent-facing services that
    /// never lease paths open this way so they cannot disagree with the
    /// resolved policy the host bound.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when SQLite cannot open or initialize the store.
    pub fn open_unresolved(path: impl AsRef<Path>) -> Result<Self, StoreError> {
        Self::open_with_host_path_identity(path, None)
    }

    /// Writes a consistent copy of this store to `path` through SQLite's own
    /// online backup (`VACUUM INTO`), then opens the copy and verifies every
    /// immutable object and hash-bound record in it. The copy is a full store:
    /// it carries host grants and private scratch and must be kept where the
    /// store itself may be kept.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the copy cannot be written, opened, or
    /// fails verification; a failed copy is removed.
    pub fn backup_to(&self, path: &Path) -> Result<BackupManifest, StoreError> {
        if let Some(parent) = path.parent()
            && !parent.as_os_str().is_empty()
        {
            std::fs::create_dir_all(parent).map_err(|error| {
                StoreError::InvalidWork(format!(
                    "cannot create backup directory {}: {error}",
                    parent.display()
                ))
            })?;
        }
        // The copy is written to a path only this invocation knows, verified
        // and hashed there, and then published under the requested name
        // without ever replacing an existing file.
        let staged = unique_sibling_path(path, "backup");
        let target = staged.to_string_lossy().into_owned();
        if let Err(error) = self.connection.execute("VACUUM INTO ?1", [&target]) {
            let _ = std::fs::remove_file(&staged);
            return Err(error.into());
        }
        // The staged copy is ours: one ordinary open settles its journal mode
        // so the copy verifies and restores through read-only opens later.
        // Closing that connection folds and removes its log; any sidecar left
        // behind is empty and must not travel with the copy.
        if let Err(error) = Self::open_with_host_path_identity(&staged, None) {
            let _ = remove_store_files(&staged);
            return Err(error);
        }
        for sidecar in store_sidecars(&staged) {
            if sidecar.exists()
                && let Err(error) = std::fs::remove_file(&sidecar)
            {
                let _ = remove_store_files(&staged);
                return Err(StoreError::InvalidWork(format!(
                    "cannot remove {}: {error}",
                    sidecar.display()
                )));
            }
        }
        let manifest = match Self::verify_backup(&staged) {
            Ok(manifest) => manifest,
            Err(error) => {
                let _ = std::fs::remove_file(&staged);
                return Err(error);
            }
        };
        if let Err(error) = publish_without_replacing(&staged, path) {
            let _ = std::fs::remove_file(&staged);
            return Err(error);
        }
        Ok(BackupManifest {
            path: path.to_path_buf(),
            ..manifest
        })
    }

    /// Makes a staged restore copy current: a copy written under an older
    /// schema is migrated in place (it is the caller's private file), its log
    /// sidecars are removed, and the result is verified. Returns the manifest
    /// of the installable bytes and the schema version the copy came from
    /// when a migration happened.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the copy cannot be migrated or fails
    /// verification.
    pub fn prepare_restore_copy(
        staged: &Path,
    ) -> Result<(BackupManifest, Option<i64>), StoreError> {
        match Self::verify_backup(staged) {
            Ok(manifest) => Ok((manifest, None)),
            Err(StoreError::BackupNeedsMigration { from_version }) => {
                drop(Self::open_with_host_path_identity(staged, None)?);
                for sidecar in store_sidecars(staged) {
                    if sidecar.exists() {
                        std::fs::remove_file(&sidecar).map_err(|error| {
                            StoreError::InvalidWork(format!(
                                "cannot remove {}: {error}",
                                sidecar.display()
                            ))
                        })?;
                    }
                }
                Ok((Self::verify_backup(staged)?, from_version.or(Some(0))))
            }
            Err(error) => Err(error),
        }
    }

    /// Verifies an existing backup file without creating, migrating, or
    /// modifying anything: the bytes are hashed first, then the file is
    /// opened read-only and every immutable object and hash-bound record is
    /// checked. A file written under an older schema is reported as
    /// [`StoreError::BackupNeedsMigration`] rather than modified.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the path is not an existing regular file,
    /// cannot be opened read-only as a current store, or fails verification.
    pub fn verify_backup(path: &Path) -> Result<BackupManifest, StoreError> {
        if !path.is_file() {
            return Err(StoreError::InvalidWork(format!(
                "backup {} is not an existing file",
                path.display()
            )));
        }
        // A backup is one self-contained file. Log sidecars beside it mean it
        // was opened read-write after it was written, so its main file may
        // not hold everything; refuse rather than verify a stale picture.
        for sidecar in store_sidecars(path) {
            if sidecar.exists() {
                return Err(StoreError::InvalidWork(format!(
                    "backup {} has a log sidecar {}; it was opened after it was written",
                    path.display(),
                    sidecar.display()
                )));
            }
        }
        let bytes = std::fs::read(path).map_err(|error| {
            StoreError::InvalidWork(format!("cannot read backup {}: {error}", path.display()))
        })?;
        let digest = <sha2::Sha256 as sha2::Digest>::digest(&bytes);
        // `immutable=1` reads exactly the hashed bytes: no shared-memory or log
        // file is consulted or created, so a read-only directory works too.
        let immutable = || {
            Connection::open_with_flags(
                immutable_uri(path)?,
                rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_URI,
            )
            .map_err(StoreError::from)
        };
        let store = match Self::from_connection(immutable()?, None, None) {
            Ok(store) => store,
            // An older but supported schema needs a write to migrate, which
            // an immutable open refuses; name that instead of the raw error so
            // a restore can migrate a staged copy on purpose.
            Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(error, _)))
                if error.code == rusqlite::ErrorCode::ReadOnly =>
            {
                let from_version = work::schema_version(&immutable()?).ok();
                return Err(StoreError::BackupNeedsMigration { from_version });
            }
            Err(error) => return Err(error),
        };
        let report = store.verify_all()?;
        if !report.is_healthy() {
            return Err(StoreError::InvalidWork(format!(
                "backup {} failed verification: {} object(s), {} control record(s), {} work record(s) invalid",
                path.display(),
                report.invalid_objects.len(),
                report.invalid_control_records.len(),
                report.invalid_work_records.len()
            )));
        }
        Ok(BackupManifest {
            path: path.to_path_buf(),
            file_sha256: format!("{digest:x}"),
            file_bytes: bytes.len() as u64,
            checked_objects: report.checked_objects,
            checked_control_records: report.checked_control_records,
            checked_work_records: report.checked_work_records,
            created_at: Utc::now(),
        })
    }

    /// Opens or creates a local database with the project root's resolved
    /// filesystem identity, probed or host-supplied, or `None` when it could
    /// not be resolved. The first resolved opener persists the policy; later
    /// resolved openers must present the same one.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when SQLite cannot open or initialize the store
    /// or the persisted policy differs from the resolved one.
    pub fn open_with_host_path_identity(
        path: impl AsRef<Path>,
        identity: Option<HostPathPolicy>,
    ) -> Result<Self, StoreError> {
        let connection = Connection::open(path)?;
        Self::from_connection(connection, identity, None)
    }

    /// The filesystem identity this opener resolved, if any.
    #[must_use]
    pub const fn host_path_identity(&self) -> Option<HostPathPolicy> {
        self.host_path_policy
    }

    /// Reads the policy persisted by the first resolved opener, if any.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the policy row cannot be read.
    pub fn stored_host_path_policy(&self) -> Result<Option<HostPathPolicy>, StoreError> {
        Self::stored_host_path_policy_on(&self.connection)
    }

    fn stored_host_path_policy_on(
        connection: &Connection,
    ) -> Result<Option<HostPathPolicy>, StoreError> {
        if !Self::sqlite_table_exists(connection, "control_host_path_policy")? {
            return Ok(None);
        }
        Ok(connection
            .query_row(
                "SELECT case_fold_paths, windows_alias_rules
                 FROM control_host_path_policy WHERE singleton = 1",
                [],
                |row| {
                    Ok(HostPathPolicy {
                        case_fold_paths: row.get::<_, bool>(0)?,
                        windows_alias_rules: row.get::<_, bool>(1)?,
                    })
                },
            )
            .optional()?)
    }

    /// The policy that normalizes one lease subject: logical subjects never
    /// need one, path subjects need the resolved identity.
    fn path_policy_for(
        &self,
        subject: &crate::domain::ResourceSubject,
    ) -> Result<HostPathPolicy, StoreError> {
        match subject {
            crate::domain::ResourceSubject::Logical { .. } => Ok(HostPathPolicy {
                case_fold_paths: false,
                windows_alias_rules: false,
            }),
            crate::domain::ResourceSubject::Path { .. } => self
                .host_path_policy
                .ok_or(StoreError::HostPathIdentityUnresolved),
        }
    }

    /// Creates a store with an explicit bootstrap control-assurance requirement.
    ///
    /// The requested value and asserted operator context apply only while
    /// installing the first policy. Reconfiguring an existing store requires
    /// an attributed policy update.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when initialization fails or an existing policy
    /// has a different assurance requirement.
    pub fn open_with_initial_control_assurance<R: Redactor>(
        path: impl AsRef<Path>,
        identity: Option<HostPathPolicy>,
        required_assurance: ControlAssurance,
        authorized_by: &ActorContext,
        reason: &str,
        redactor: &R,
    ) -> Result<Self, StoreError> {
        if authorized_by.assurance != AssuranceLevel::Asserted {
            return Err(StoreError::InvalidControlProjection(
                "V1 control-policy bootstrap records asserted host context only".into(),
            ));
        }
        let authorized_by = normalize_control_policy_actor(authorized_by, redactor)?;
        let reason = normalize_control_text(reason, "control policy bootstrap reason")?;
        redactor
            .inspect(&reason)
            .map_err(StoreError::RedactionRefused)?;
        let connection = Connection::open(path)?;
        Self::from_connection(
            connection,
            identity,
            Some(InitialControlPolicy {
                required_assurance,
                authorized_by,
                reason,
            }),
        )
    }

    /// Opens a store with an explicit embedding-host filesystem identity policy.
    ///
    /// The first opener persists the policy. Later openers must present the
    /// same policy so resource lease identities cannot drift between hosts.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when initialization fails or the stored policy differs.
    pub fn open_with_host_path_policy(
        path: impl AsRef<Path>,
        policy: HostPathPolicy,
    ) -> Result<Self, StoreError> {
        Self::open_with_host_path_identity(path, Some(policy))
    }

    /// Creates an isolated store for tests or ephemeral runs under the running
    /// target's conservative path policy.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when SQLite cannot initialize the schema.
    pub fn open_in_memory() -> Result<Self, StoreError> {
        Self::open_in_memory_with_host_path_identity(Some(HostPathPolicy::host_default()))
    }

    /// Creates an isolated store with an explicit or unresolved filesystem
    /// identity.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when SQLite cannot initialize the schema.
    pub fn open_in_memory_with_host_path_identity(
        identity: Option<HostPathPolicy>,
    ) -> Result<Self, StoreError> {
        Self::from_connection(Connection::open_in_memory()?, identity, None)
    }

    fn from_connection(
        connection: Connection,
        host_path_policy: Option<HostPathPolicy>,
        initial_control_policy: Option<InitialControlPolicy>,
    ) -> Result<Self, StoreError> {
        Self::from_connection_with_busy_timeout(
            connection,
            host_path_policy,
            initial_control_policy,
            Duration::from_secs(5),
        )
    }

    #[allow(
        clippy::if_not_else,
        clippy::too_many_lines,
        reason = "the cold-schema branch stays adjacent to the complete idempotent DDL for auditability"
    )]
    fn from_connection_with_busy_timeout(
        mut connection: Connection,
        host_path_policy: Option<HostPathPolicy>,
        initial_control_policy: Option<InitialControlPolicy>,
        busy_timeout: Duration,
    ) -> Result<Self, StoreError> {
        connection.busy_timeout(busy_timeout)?;
        work::preflight_schema(&connection)?;
        if Self::sqlite_table_exists(&connection, "task_changes")? {
            Self::verify_task_change_cursor_schema(&connection)?;
        }
        Self::preflight_host_path_policy(&connection, host_path_policy)?;
        Self::preflight_control_policy_schema(&connection)?;
        let control_policy_preexisted = Self::control_policy_preexisted(&connection)?;
        Self::preflight_legacy_initial_control_assurance(
            &connection,
            control_policy_preexisted,
            initial_control_policy
                .as_ref()
                .map(|policy| policy.required_assurance),
        )?;
        let core_schema_complete = Self::current_core_schema_is_complete(&connection)?;
        connection.execute_batch(
            "PRAGMA foreign_keys = ON;
             PRAGMA synchronous = NORMAL;",
        )?;
        let journal_mode =
            connection.query_row("PRAGMA journal_mode", [], |row| row.get::<_, String>(0))?;
        if !matches!(journal_mode.as_str(), "wal" | "memory") {
            connection.execute_batch("PRAGMA journal_mode = WAL;")?;
        }
        if !core_schema_complete {
            connection.execute_batch("BEGIN IMMEDIATE;")?;
            connection.execute_batch(
                "CREATE TABLE IF NOT EXISTS objects (
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
                 work_id TEXT,
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
                 ON memory_heads(project_id, task_id, work_id, agent_id, status);
             CREATE TABLE IF NOT EXISTS note_intents (
                 idempotency_key TEXT PRIMARY KEY,
                 request_hash TEXT NOT NULL,
                 receipt_json BLOB NOT NULL
             ) STRICT;
             CREATE TABLE IF NOT EXISTS project_context_revisions (
                 project_id TEXT PRIMARY KEY,
                 revision INTEGER NOT NULL CHECK(revision >= 0)
             ) STRICT;
             CREATE TABLE IF NOT EXISTS agent_context_revisions (
                 project_id TEXT NOT NULL,
                 agent_id TEXT NOT NULL,
                 revision INTEGER NOT NULL CHECK(revision >= 0),
                 PRIMARY KEY(project_id, agent_id)
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
             CREATE TABLE IF NOT EXISTS memory_contradiction_edges (
                 contradiction_hash TEXT PRIMARY KEY REFERENCES objects(object_hash),
                 project_id TEXT NOT NULL,
                 task_id TEXT,
                 work_root_id TEXT,
                 left_version_hash TEXT NOT NULL REFERENCES objects(object_hash),
                 right_version_hash TEXT NOT NULL REFERENCES objects(object_hash),
                 UNIQUE(left_version_hash, right_version_hash),
                 CHECK(left_version_hash < right_version_hash)
             ) STRICT;
             CREATE INDEX IF NOT EXISTS memory_contradiction_edges_context
                 ON memory_contradiction_edges(project_id, task_id, work_root_id);
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
                  sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                  task_id TEXT NOT NULL,
                  task_cursor INTEGER NOT NULL CHECK(task_cursor > 0),
                  object_kind TEXT NOT NULL,
                  object_hash TEXT NOT NULL REFERENCES objects(object_hash),
                  UNIQUE(task_id, task_cursor),
                  UNIQUE(task_id, object_hash)
              ) STRICT;
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
             ) STRICT;
             CREATE TABLE IF NOT EXISTS control_observations (
                 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id TEXT NOT NULL,
                 task_id TEXT,
                 idempotency_key TEXT NOT NULL,
                 intent_hash TEXT NOT NULL,
                 input_hash TEXT NOT NULL,
                 input_json BLOB NOT NULL,
                 decision_hash TEXT NOT NULL,
                 decision_json BLOB NOT NULL,
                 observed_at_ms INTEGER NOT NULL,
                 UNIQUE(session_id, idempotency_key)
             ) STRICT;
             CREATE INDEX IF NOT EXISTS control_observations_session_sequence
                 ON control_observations(session_id, sequence);
             CREATE TABLE IF NOT EXISTS control_policy_state (
                 singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
                 schema_version INTEGER NOT NULL,
                 policy_epoch INTEGER NOT NULL,
                 required_assurance TEXT NOT NULL,
                 supported_effects_json TEXT NOT NULL,
                 grant_ttl_seconds INTEGER NOT NULL,
                 policy_hash TEXT REFERENCES objects(object_hash)
             ) STRICT;
             CREATE TABLE IF NOT EXISTS control_policy_versions (
                 policy_hash TEXT PRIMARY KEY REFERENCES objects(object_hash),
                 policy_epoch INTEGER NOT NULL UNIQUE CHECK(policy_epoch > 0),
                 authority_hash TEXT NOT NULL REFERENCES objects(object_hash),
                 policy_json BLOB NOT NULL
             ) STRICT;
             CREATE TABLE IF NOT EXISTS control_policy_operation_results (
                 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                 operation TEXT NOT NULL,
                 idempotency_key TEXT NOT NULL,
                 intent_hash TEXT NOT NULL,
                 intent_json BLOB NOT NULL,
                 result_hash TEXT NOT NULL,
                 result_json BLOB NOT NULL,
                 created_at_ms INTEGER NOT NULL,
                 UNIQUE(operation, idempotency_key)
             ) STRICT;
             CREATE TABLE IF NOT EXISTS task_control_state (
                 task_id TEXT PRIMARY KEY REFERENCES tasks(task_id),
                 admission_epoch INTEGER NOT NULL
             ) STRICT;
             CREATE TABLE IF NOT EXISTS control_connections (
                 session_id TEXT PRIMARY KEY,
                 connection_token TEXT NOT NULL,
                 opened_at_ms INTEGER NOT NULL
             ) STRICT;
             CREATE TABLE IF NOT EXISTS control_sessions (
                 session_id TEXT PRIMARY KEY,
                 project_id TEXT NOT NULL,
                 task_id TEXT NOT NULL REFERENCES tasks(task_id),
                 root_execution_id TEXT,
                 work_id TEXT,
                 run_id TEXT,
                 work_revision INTEGER,
                 claim_id TEXT,
                 claim_fence INTEGER,
                 routing_token TEXT NOT NULL,
                 actor_json BLOB NOT NULL,
                 bind_key TEXT NOT NULL,
                 bind_intent_hash TEXT NOT NULL,
                 bind_intent_json BLOB NOT NULL,
                 phase TEXT NOT NULL,
                 assurance TEXT NOT NULL,
                 mediated_effects_json TEXT NOT NULL,
                 confirmed_cursor INTEGER NOT NULL,
                 tentative_cursor INTEGER,
                 project_policy_epoch INTEGER NOT NULL,
                 task_admission_epoch INTEGER NOT NULL,
                 blocking_watermark INTEGER NOT NULL,
                 capability_map_revision INTEGER NOT NULL,
                 revision INTEGER NOT NULL,
                 updated_at_ms INTEGER NOT NULL
             ) STRICT;
             CREATE INDEX IF NOT EXISTS control_sessions_work_run
                 ON control_sessions(project_id, run_id, session_id);
             CREATE TABLE IF NOT EXISTS control_turn_results (
                 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id TEXT NOT NULL REFERENCES control_sessions(session_id),
                 task_id TEXT NOT NULL REFERENCES tasks(task_id),
                 idempotency_key TEXT NOT NULL,
                 intent_hash TEXT NOT NULL,
                 intent_json BLOB NOT NULL,
                 decision_hash TEXT NOT NULL,
                 decision_json BLOB NOT NULL,
                 created_at_ms INTEGER NOT NULL,
                 UNIQUE(session_id, idempotency_key)
             ) STRICT;
             CREATE TABLE IF NOT EXISTS control_turn_grants (
                 grant_id TEXT PRIMARY KEY,
                 session_id TEXT NOT NULL REFERENCES control_sessions(session_id),
                 task_id TEXT NOT NULL REFERENCES tasks(task_id),
                 request_key TEXT NOT NULL,
                 grant_hash TEXT NOT NULL,
                 grant_json BLOB NOT NULL,
                 state TEXT NOT NULL,
                 issued_at_ms INTEGER NOT NULL,
                 expires_at_ms INTEGER NOT NULL,
                 begun_at_ms INTEGER,
                 completed_at_ms INTEGER,
                 UNIQUE(session_id, request_key)
             ) STRICT;
             CREATE TABLE IF NOT EXISTS control_work_leases (
                 lease_id TEXT PRIMARY KEY,
                 task_id TEXT NOT NULL REFERENCES tasks(task_id),
                 holder_session_id TEXT NOT NULL REFERENCES control_sessions(session_id),
                 lease_hash TEXT NOT NULL,
                 lease_json BLOB NOT NULL,
                 state TEXT NOT NULL,
                 expires_at_ms INTEGER NOT NULL,
                 UNIQUE(holder_session_id, lease_id)
             ) STRICT;
             CREATE INDEX IF NOT EXISTS control_work_leases_task_state
                 ON control_work_leases(task_id, state, expires_at_ms);
             CREATE TABLE IF NOT EXISTS control_operation_results (
                 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                 session_id TEXT NOT NULL REFERENCES control_sessions(session_id),
                 operation TEXT NOT NULL,
                 idempotency_key TEXT NOT NULL,
                 intent_hash TEXT NOT NULL,
                 intent_json BLOB NOT NULL,
                 result_hash TEXT NOT NULL,
                 result_json BLOB NOT NULL,
                 created_at_ms INTEGER NOT NULL,
                 UNIQUE(session_id, operation, idempotency_key)
             ) STRICT;",
            )?;
            #[cfg(test)]
            if fail_cold_schema_after_ddl() {
                return Err(StoreError::InvalidControlProjection(
                    "injected cold-schema failure after DDL".into(),
                ));
            }
        }
        if core_schema_complete {
            Self::bind_host_path_policy(&mut connection, host_path_policy)?;
        } else if let Some(policy) = host_path_policy {
            Self::bind_host_path_policy_on(&connection, policy)?;
        } else {
            Self::preflight_host_path_policy(&connection, None)?;
        }
        if !core_schema_complete {
            let has_memory_work_id = connection.query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM pragma_table_info('memory_heads')
                     WHERE name = 'work_id'
                 )",
                [],
                |row| row.get::<_, bool>(0),
            )?;
            if !has_memory_work_id {
                connection.execute("ALTER TABLE memory_heads ADD COLUMN work_id TEXT", [])?;
            }
            connection.execute(
                "CREATE INDEX IF NOT EXISTS memory_heads_work_scope
                 ON memory_heads(project_id, work_id, agent_id, status)",
                [],
            )?;
            connection.execute_batch(
                "INSERT INTO project_context_revisions (project_id, revision)
                 SELECT project_id, COUNT(*)
                 FROM (
                     SELECT project_id FROM memory_heads WHERE scope_kind = 'project'
                     UNION ALL
                     SELECT project_id FROM memory_contradiction_edges
                 )
                 GROUP BY project_id
                 ON CONFLICT(project_id) DO NOTHING;
                 INSERT INTO agent_context_revisions (project_id, agent_id, revision)
                 SELECT project_id, agent_id, COUNT(*)
                 FROM memory_heads
                 WHERE scope_kind = 'agent' AND agent_id IS NOT NULL
                 GROUP BY project_id, agent_id
                 ON CONFLICT(project_id, agent_id) DO NOTHING;",
            )?;
        }
        Self::verify_task_change_cursor_schema(&connection)?;
        if !core_schema_complete {
            connection.execute(
                "CREATE UNIQUE INDEX IF NOT EXISTS task_changes_task_cursor
                 ON task_changes(task_id, task_cursor)",
                [],
            )?;
        }
        if core_schema_complete {
            Self::migrate_control_policy(
                &mut connection,
                control_policy_preexisted,
                initial_control_policy,
            )?;
        } else {
            Self::migrate_control_policy_on(
                &connection,
                control_policy_preexisted,
                initial_control_policy,
            )?;
            connection.execute_batch("COMMIT;")?;
        }
        work::migrate(&mut connection)?;
        let work_schema_version = work::schema_version(&connection)?;
        Self::migrate_control_work_bindings(&mut connection)?;
        Self::migrate_memory_contradiction_edges(&mut connection)?;
        Ok(Self {
            connection,
            work_schema_version,
            host_path_policy,
        })
    }

    fn control_policy_row_exists(connection: &Connection) -> Result<bool, StoreError> {
        if !Self::sqlite_table_exists(connection, "control_policy_state")? {
            return Ok(false);
        }
        connection
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM control_policy_state WHERE singleton = 1
                 )",
                [],
                |row| row.get::<_, bool>(0),
            )
            .map_err(StoreError::from)
    }

    fn sqlite_table_exists(connection: &Connection, table: &str) -> Result<bool, StoreError> {
        connection
            .query_row(
                "SELECT EXISTS(
                 SELECT 1 FROM sqlite_master
                 WHERE type = 'table' AND name = ?1
             )",
                [table],
                |row| row.get::<_, bool>(0),
            )
            .map_err(StoreError::from)
    }

    fn control_policy_family_exists(connection: &Connection) -> Result<bool, StoreError> {
        for table in [
            "control_policy_state",
            "control_policy_versions",
            "control_policy_operation_results",
            "control_sessions",
            "control_turn_grants",
        ] {
            if Self::sqlite_table_exists(connection, table)? {
                return Ok(true);
            }
        }
        Ok(false)
    }

    fn canonical_control_policy_objects_exist(connection: &Connection) -> Result<bool, StoreError> {
        if !Self::sqlite_table_exists(connection, "objects")? {
            return Ok(false);
        }
        connection
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM objects
                     WHERE object_kind IN (
                         'control_policy', 'project_policy_authority_decision'
                     )
                 )",
                [],
                |row| row.get::<_, bool>(0),
            )
            .map_err(StoreError::from)
    }

    fn control_policy_preexisted(connection: &Connection) -> Result<bool, StoreError> {
        // A recognized legacy singleton is already an established policy,
        // even when the store has never recorded any other object. A pre-fix
        // interrupted bootstrap is byte-identical to that valid legacy state,
        // so guessing from data presence could silently replace turn_gated at
        // epoch one. New bootstrap inserts this selector in the same migration
        // transaction, eliminating the crash window without a heuristic.
        Self::control_policy_row_exists(connection)
    }

    fn control_policy_state_columns(
        connection: &Connection,
    ) -> Result<HashSet<String>, StoreError> {
        let mut statement = connection.prepare("PRAGMA table_info('control_policy_state')")?;
        let rows = statement.query_map([], |row| row.get::<_, String>(1))?;
        Ok(rows.collect::<Result<HashSet<_>, _>>()?)
    }

    fn control_policy_operation_results_table_is_complete(
        connection: &Connection,
    ) -> Result<bool, StoreError> {
        if !Self::sqlite_table_exists(connection, "control_policy_operation_results")? {
            return Ok(false);
        }
        let mut statement =
            connection.prepare("PRAGMA table_info('control_policy_operation_results')")?;
        let columns = statement
            .query_map([], |row| row.get::<_, String>(1))?
            .collect::<Result<HashSet<_>, _>>()?;
        Ok([
            "sequence",
            "operation",
            "idempotency_key",
            "intent_hash",
            "intent_json",
            "result_hash",
            "result_json",
            "created_at_ms",
        ]
        .iter()
        .all(|column| columns.contains(*column)))
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the fail-before-DDL validation stays together so every accepted legacy/current shape is auditable"
    )]
    fn preflight_control_policy_schema(connection: &Connection) -> Result<(), StoreError> {
        if !Self::sqlite_table_exists(connection, "control_policy_state")? {
            if Self::control_policy_family_exists(connection)?
                || Self::canonical_control_policy_objects_exist(connection)?
            {
                return Err(StoreError::InvalidControlProjection(
                    "control policy state is missing from an established store".into(),
                ));
            }
            return Ok(());
        }
        if !Self::control_policy_row_exists(connection)? {
            return Err(StoreError::InvalidControlProjection(
                "control policy singleton is missing from an established store".into(),
            ));
        }
        let columns = Self::control_policy_state_columns(connection)?;
        for required in [
            "singleton",
            "schema_version",
            "policy_epoch",
            "required_assurance",
            "supported_effects_json",
            "grant_ttl_seconds",
        ] {
            if !columns.contains(required) {
                return Err(StoreError::InvalidControlProjection(format!(
                    "control policy state is missing required column {required:?}"
                )));
            }
        }
        let (schema_version, epoch, required_assurance, supported_effects_json, grant_ttl): (
            i64,
            i64,
            String,
            String,
            i64,
        ) = connection.query_row(
            "SELECT schema_version, policy_epoch, required_assurance,
                    supported_effects_json, grant_ttl_seconds
             FROM control_policy_state WHERE singleton = 1",
            [],
            |row| {
                Ok((
                    row.get(0)?,
                    row.get(1)?,
                    row.get(2)?,
                    row.get(3)?,
                    row.get(4)?,
                ))
            },
        )?;
        match schema_version {
            1 => {
                let effects: Vec<EffectClass> = serde_json::from_str(&supported_effects_json)?;
                if epoch != 1
                    || required_assurance != "turn_gated"
                    || !Self::is_recognized_builtin_envelope(&effects, grant_ttl)
                    || (columns.contains("policy_hash")
                        && connection.query_row(
                            "SELECT policy_hash IS NOT NULL FROM control_policy_state
                             WHERE singleton = 1",
                            [],
                            |row| row.get::<_, bool>(0),
                        )?)
                {
                    return Err(StoreError::InvalidControlProjection(
                        "legacy control policy is not the recognized stock V1 row".into(),
                    ));
                }
            }
            LEGACY_VERSIONED_CONTROL_POLICY_STATE_SCHEMA_VERSION
            | LEGACY_REPLAYLESS_CONTROL_POLICY_STATE_SCHEMA_VERSION
            | CONTROL_POLICY_STATE_SCHEMA_VERSION => {
                if !columns.contains("policy_hash") {
                    return Err(StoreError::InvalidControlProjection(
                        "current control policy state has no active policy hash".into(),
                    ));
                }
                let versions_exist = connection.query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM sqlite_master
                         WHERE type = 'table' AND name = 'control_policy_versions'
                     )",
                    [],
                    |row| row.get::<_, bool>(0),
                )?;
                if !versions_exist {
                    return Err(StoreError::InvalidControlProjection(
                        "current control policy version table is missing".into(),
                    ));
                }
                // Validate the entire immutable chain before any schema setup
                // can mutate a partially corrupt current store.
                let snapshot = connection.unchecked_transaction()?;
                Self::verify_control_policy_history(&snapshot)?;
                if schema_version >= LEGACY_REPLAYLESS_CONTROL_POLICY_STATE_SCHEMA_VERSION {
                    let (_, policy, _) = Self::load_control_policy_head(&snapshot)?;
                    Self::validate_migratable_active_control_policy(&policy)?;
                    let rule_set = policy.obligation_rule_set.as_ref().ok_or_else(|| {
                        StoreError::InvalidControlProjection(
                            "current control policy has no obligation rule-set selection".into(),
                        )
                    })?;
                    Self::load_obligation_rule_set_on(&snapshot, rule_set)?;
                }
                if schema_version == CONTROL_POLICY_STATE_SCHEMA_VERSION
                    && !Self::control_policy_operation_results_table_is_complete(&snapshot)?
                {
                    return Err(StoreError::InvalidControlProjection(
                        "current control policy operation-result table is missing".into(),
                    ));
                }
                snapshot.commit()?;
            }
            other => {
                return Err(StoreError::InvalidControlProjection(format!(
                    "control policy state schema {other} is not supported"
                )));
            }
        }
        Ok(())
    }

    fn preflight_legacy_initial_control_assurance(
        connection: &Connection,
        policy_preexisted: bool,
        initial_required_assurance: Option<ControlAssurance>,
    ) -> Result<(), StoreError> {
        let Some(requested) = initial_required_assurance else {
            return Ok(());
        };
        if !policy_preexisted {
            return Ok(());
        }
        let required_assurance: String = connection.query_row(
            "SELECT required_assurance
             FROM control_policy_state WHERE singleton = 1",
            [],
            |row| row.get(0),
        )?;
        let projected_assurance: ControlAssurance = parse_enum(&required_assurance)?;
        if projected_assurance != requested {
            return Err(StoreError::InvalidControlProjection(
                "initial assurance cannot replace an existing policy; use control-policy set-required-assurance"
                    .into(),
            ));
        }
        Ok(())
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the version preflight, canonical bootstrap, and active-policy CAS stay adjacent for migration auditability"
    )]
    fn migrate_control_policy(
        connection: &mut Connection,
        policy_preexisted: bool,
        initial_control_policy: Option<InitialControlPolicy>,
    ) -> Result<(), StoreError> {
        let snapshot = connection.unchecked_transaction()?;
        let need = Self::control_policy_migration_need_on(
            &snapshot,
            policy_preexisted,
            initial_control_policy.as_ref(),
        )?;
        snapshot.commit()?;
        if need == OpenMigrationNeed::Current {
            return Ok(());
        }
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if Self::control_policy_migration_need_on(
            &transaction,
            policy_preexisted,
            initial_control_policy.as_ref(),
        )? == OpenMigrationNeed::NeedsWrite
        {
            Self::migrate_control_policy_on(
                &transaction,
                policy_preexisted,
                initial_control_policy,
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    fn control_policy_migration_need_on(
        connection: &Connection,
        policy_preexisted: bool,
        initial_control_policy: Option<&InitialControlPolicy>,
    ) -> Result<OpenMigrationNeed, StoreError> {
        let schema_version = connection
            .query_row(
                "SELECT schema_version FROM control_policy_state WHERE singleton = 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        match schema_version {
            Some(CONTROL_POLICY_STATE_SCHEMA_VERSION) => {
                let current = Self::verify_control_policy_history(connection)?;
                let (_, policy, _) = Self::load_control_policy_head(connection)?;
                Self::validate_migratable_active_control_policy(&policy)?;
                if initial_control_policy
                    .is_some_and(|initial| initial.required_assurance != current.required_assurance)
                {
                    return Err(StoreError::InvalidControlProjection(
                        "initial assurance cannot replace an existing policy; use control-policy set-required-assurance"
                            .into(),
                    ));
                }
                if current.supported_effects != Self::builtin_control_effects()
                    || current.grant_ttl_seconds != BUILTIN_CONTROL_GRANT_TTL_SECONDS
                {
                    return Ok(OpenMigrationNeed::NeedsWrite);
                }
                Self::validate_active_control_policy(&policy)?;
                let rule_set = current.obligation_rule_set.as_ref().ok_or_else(|| {
                    StoreError::InvalidControlProjection(
                        "current control policy has no obligation rule-set selection".into(),
                    )
                })?;
                Self::load_obligation_rule_set_on(connection, rule_set)?;
                if !Self::control_policy_operation_results_table_is_complete(connection)? {
                    return Err(StoreError::InvalidControlProjection(
                        "current control policy operation-result table is missing".into(),
                    ));
                }
                Ok(OpenMigrationNeed::Current)
            }
            Some(LEGACY_REPLAYLESS_CONTROL_POLICY_STATE_SCHEMA_VERSION) => {
                let current = Self::verify_control_policy_history(connection)?;
                let (_, policy, _) = Self::load_control_policy_head(connection)?;
                Self::validate_active_control_policy(&policy)?;
                if initial_control_policy
                    .is_some_and(|initial| initial.required_assurance != current.required_assurance)
                {
                    return Err(StoreError::InvalidControlProjection(
                        "initial assurance cannot replace an existing policy; use control-policy set-required-assurance"
                            .into(),
                    ));
                }
                let rule_set = current.obligation_rule_set.as_ref().ok_or_else(|| {
                    StoreError::InvalidControlProjection(
                        "current control policy has no obligation rule-set selection".into(),
                    )
                })?;
                Self::load_obligation_rule_set_on(connection, rule_set)?;
                Ok(OpenMigrationNeed::NeedsWrite)
            }
            Some(LEGACY_VERSIONED_CONTROL_POLICY_STATE_SCHEMA_VERSION) => {
                let current = Self::verify_control_policy_history(connection)?;
                let (_, policy, _) = Self::load_control_policy_head(connection)?;
                Self::validate_migratable_active_control_policy(&policy)?;
                if initial_control_policy
                    .is_some_and(|initial| initial.required_assurance != current.required_assurance)
                {
                    return Err(StoreError::InvalidControlProjection(
                        "initial assurance cannot replace an existing policy; use control-policy set-required-assurance"
                            .into(),
                    ));
                }
                Ok(OpenMigrationNeed::NeedsWrite)
            }
            Some(1) => Ok(OpenMigrationNeed::NeedsWrite),
            None if !policy_preexisted => Ok(OpenMigrationNeed::NeedsWrite),
            None => Err(StoreError::InvalidControlProjection(
                "control policy singleton disappeared from an established store".into(),
            )),
            Some(other) => Err(StoreError::InvalidControlProjection(format!(
                "control policy state schema {other} is not supported"
            ))),
        }
    }

    #[allow(
        clippy::too_many_lines,
        reason = "legacy projection capture, canonical epoch-one binding, and envelope upgrade remain adjacent for migration auditability"
    )]
    fn migrate_control_policy_on(
        connection: &Connection,
        policy_preexisted: bool,
        initial_control_policy: Option<InitialControlPolicy>,
    ) -> Result<(), StoreError> {
        Self::ensure_control_policy_operation_results_table(connection)?;
        let initial_required_assurance = initial_control_policy
            .as_ref()
            .map(|policy| policy.required_assurance);
        let projected_state: Option<(i64, String, String, i64)> = connection
            .query_row(
                "SELECT schema_version, required_assurance,
                        supported_effects_json, grant_ttl_seconds
                 FROM control_policy_state WHERE singleton = 1",
                [],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        if projected_state
            .as_ref()
            .is_some_and(|(schema_version, _, _, _)| {
                *schema_version == CONTROL_POLICY_STATE_SCHEMA_VERSION
            })
        {
            let (policy, _, _) = Self::load_control_policy_head(connection)?;
            if initial_required_assurance
                .is_some_and(|requested| requested != policy.required_assurance)
            {
                return Err(StoreError::InvalidControlProjection(
                    "initial assurance cannot replace an existing policy; use control-policy set-required-assurance"
                        .into(),
                ));
            }
            let policy = Self::upgrade_builtin_control_envelope_on(connection, policy, Utc::now())?;
            Self::upgrade_builtin_obligation_rules_on(connection, policy, Utc::now())?;
            return Ok(());
        }
        let (schema_version, projected_assurance, projected_effects_json, projected_grant_ttl) =
            if let Some(projected_state) = projected_state {
                projected_state
            } else {
                if policy_preexisted {
                    return Err(StoreError::InvalidControlProjection(
                        "control policy singleton disappeared from an established store".into(),
                    ));
                }
                let assurance = initial_required_assurance.unwrap_or(ControlAssurance::TurnGated);
                let assurance_name = enum_name(assurance)?;
                let effects_json = serde_json::to_string(&Self::builtin_control_effects())?;
                connection.execute(
                    "INSERT INTO control_policy_state (
                         singleton, schema_version, policy_epoch, required_assurance,
                         supported_effects_json, grant_ttl_seconds
                     ) VALUES (1, 1, 1, ?1, ?2, ?3)",
                    params![
                        assurance_name,
                        effects_json,
                        BUILTIN_CONTROL_GRANT_TTL_SECONDS,
                    ],
                )?;
                (
                    1,
                    assurance_name,
                    serde_json::to_string(&Self::builtin_control_effects())?,
                    BUILTIN_CONTROL_GRANT_TTL_SECONDS,
                )
            };
        if schema_version == LEGACY_REPLAYLESS_CONTROL_POLICY_STATE_SCHEMA_VERSION {
            let current = Self::verify_control_policy_history(connection)?;
            if initial_required_assurance
                .is_some_and(|requested| requested != current.required_assurance)
            {
                return Err(StoreError::InvalidControlProjection(
                    "initial assurance cannot replace an existing policy; use control-policy set-required-assurance"
                        .into(),
                ));
            }
            connection.execute(
                "UPDATE control_policy_state SET schema_version = ?1 WHERE singleton = 1",
                [CONTROL_POLICY_STATE_SCHEMA_VERSION],
            )?;
            Self::verify_control_policy_history(connection)?;
            return Ok(());
        }
        if schema_version == LEGACY_VERSIONED_CONTROL_POLICY_STATE_SCHEMA_VERSION {
            let (policy, _, _) = Self::load_control_policy_head(connection)?;
            if initial_required_assurance
                .is_some_and(|requested| requested != policy.required_assurance)
            {
                return Err(StoreError::InvalidControlProjection(
                    "initial assurance cannot replace an existing policy; use control-policy set-required-assurance"
                        .into(),
                ));
            }
            let policy = Self::upgrade_builtin_control_envelope_on(connection, policy, Utc::now())?;
            Self::upgrade_builtin_obligation_rules_on(connection, policy, Utc::now())?;
            connection.execute(
                "UPDATE control_policy_state SET schema_version = ?1 WHERE singleton = 1",
                [CONTROL_POLICY_STATE_SCHEMA_VERSION],
            )?;
            Self::verify_control_policy_history(connection)?;
            return Ok(());
        }
        if schema_version != 1 {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy state schema {schema_version} is not supported"
            )));
        }
        let projected_assurance = parse_enum(&projected_assurance)?;
        let projected_effects: Vec<EffectClass> = serde_json::from_str(&projected_effects_json)?;
        if policy_preexisted
            && initial_required_assurance.is_some_and(|requested| requested != projected_assurance)
        {
            return Err(StoreError::InvalidControlProjection(
                "initial assurance cannot replace an existing policy; use control-policy set-required-assurance"
                    .into(),
                ));
        }
        let has_policy_hash = connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM pragma_table_info('control_policy_state')
                 WHERE name = 'policy_hash'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if !has_policy_hash {
            connection.execute(
                "ALTER TABLE control_policy_state
                 ADD COLUMN policy_hash TEXT REFERENCES objects(object_hash)",
                [],
            )?;
        }
        connection.execute(
            "CREATE TABLE IF NOT EXISTS control_policy_versions (
                 policy_hash TEXT PRIMARY KEY REFERENCES objects(object_hash),
                 policy_epoch INTEGER NOT NULL UNIQUE CHECK(policy_epoch > 0),
                 authority_hash TEXT NOT NULL REFERENCES objects(object_hash),
                 policy_json BLOB NOT NULL
             ) STRICT",
            [],
        )?;
        let now = Utc::now();
        let (required_assurance, authorized_by, reason) =
            match (policy_preexisted, initial_control_policy) {
                (_, Some(initial)) => (
                    initial.required_assurance,
                    initial.authorized_by,
                    initial.reason,
                ),
                (true, None) => {
                    let source = "engram:migration";
                    let reason = "bind the recognized stock V1 control policy to canonical history";
                    (
                        projected_assurance,
                        ActorContext {
                            actor_id: source.into(),
                            actor_kind: "system".into(),
                            assurance: AssuranceLevel::Asserted,
                            run_id: None,
                            session_id: None,
                            source_tool: Some(source.into()),
                            source_skill: None,
                            provenance_chain: Vec::new(),
                            reason: reason.into(),
                        },
                        reason.to_owned(),
                    )
                }
                (false, None) => {
                    let source = "engram:init";
                    let reason = "install the default project bootstrap control policy";
                    (
                        ControlAssurance::TurnGated,
                        ActorContext {
                            actor_id: source.into(),
                            actor_kind: "system".into(),
                            assurance: AssuranceLevel::Asserted,
                            run_id: None,
                            session_id: None,
                            source_tool: Some(source.into()),
                            source_skill: None,
                            provenance_chain: Vec::new(),
                            reason: reason.into(),
                        },
                        reason.to_owned(),
                    )
                }
            };
        let obligation_rule_set = Self::insert_builtin_obligation_rule_set(connection)?;
        let authority = ProjectPolicyAuthorityDecision {
            schema_version: CONTROL_POLICY_AUTHORITY_SCHEMA_VERSION_V1,
            operation: ProjectPolicyOperation::SetRequiredAssurance,
            policy_epoch: ProjectPolicyEpoch(1),
            previous_policy: None,
            required_assurance,
            obligation_rule_set: Some(obligation_rule_set.clone()),
            authorized_by,
            reason,
            decided_at: now,
        };
        let authority_object = CanonicalObject::freeze(&authority)?;
        if authority_object.bytes().len() > MAX_CONTROL_POLICY_AUTHORITY_BYTES {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy authority exceeds the {MAX_CONTROL_POLICY_AUTHORITY_BYTES}-byte canonical limit"
            )));
        }
        Self::insert_object(
            connection,
            "project_policy_authority_decision",
            &authority_object,
        )?;
        let policy = ControlPolicy {
            schema_version: CONTROL_POLICY_SCHEMA_VERSION_V1,
            control_schema_version: CONTROL_POLICY_CONTROL_SCHEMA_VERSION_V1,
            policy_epoch: ProjectPolicyEpoch(1),
            previous_policy: None,
            required_assurance,
            supported_effects: projected_effects,
            grant_ttl_seconds: projected_grant_ttl,
            obligation_rule_set: Some(obligation_rule_set),
            authority: authority_object.hash().clone(),
            activated_at: now,
        };
        let policy_object = CanonicalObject::freeze(&policy)?;
        Self::insert_object(connection, "control_policy", &policy_object)?;
        connection.execute(
            "INSERT INTO control_policy_versions (
                     policy_hash, policy_epoch, authority_hash, policy_json
                 ) VALUES (?1, ?2, ?3, ?4)",
            params![
                policy_object.hash().as_str(),
                policy.policy_epoch.0,
                authority_object.hash().as_str(),
                policy_object.bytes(),
            ],
        )?;
        connection.execute(
            "UPDATE control_policy_state SET
                     schema_version = ?1, policy_epoch = ?2,
                     required_assurance = ?3, supported_effects_json = ?4,
                     grant_ttl_seconds = ?5, policy_hash = ?6
                 WHERE singleton = 1",
            params![
                CONTROL_POLICY_STATE_SCHEMA_VERSION,
                policy.policy_epoch.0,
                enum_name(policy.required_assurance)?,
                serde_json::to_string(&policy.supported_effects)?,
                policy.grant_ttl_seconds,
                policy_object.hash().as_str(),
            ],
        )?;
        let policy = Self::verify_control_policy_history(connection)?;
        if initial_required_assurance
            .is_some_and(|requested| requested != policy.required_assurance)
        {
            return Err(StoreError::InvalidControlProjection(
                "initial assurance cannot replace an existing policy; use control-policy set-required-assurance"
                    .into(),
                ));
        }
        Self::upgrade_builtin_control_envelope_on(connection, policy, now)?;
        Ok(())
    }

    fn ensure_control_policy_operation_results_table(
        connection: &Connection,
    ) -> Result<(), StoreError> {
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS control_policy_operation_results (
                 sequence INTEGER PRIMARY KEY AUTOINCREMENT,
                 operation TEXT NOT NULL,
                 idempotency_key TEXT NOT NULL,
                 intent_hash TEXT NOT NULL,
                 intent_json BLOB NOT NULL,
                 result_hash TEXT NOT NULL,
                 result_json BLOB NOT NULL,
                 created_at_ms INTEGER NOT NULL,
                 UNIQUE(operation, idempotency_key)
             ) STRICT;",
        )?;
        Ok(())
    }

    fn builtin_control_effects() -> Vec<EffectClass> {
        vec![
            EffectClass::Observe,
            EffectClass::Communicate,
            EffectClass::Coordinate,
            EffectClass::MutateLocal,
        ]
    }

    fn legacy_v1_control_effects() -> Vec<EffectClass> {
        vec![
            EffectClass::Observe,
            EffectClass::Communicate,
            EffectClass::MutateLocal,
        ]
    }

    fn recognized_builtin_envelopes() -> Vec<(Vec<EffectClass>, i64)> {
        vec![
            (
                Self::legacy_v1_control_effects(),
                BUILTIN_CONTROL_GRANT_TTL_SECONDS,
            ),
            (
                Self::builtin_control_effects(),
                BUILTIN_CONTROL_GRANT_TTL_SECONDS,
            ),
        ]
    }

    fn is_recognized_builtin_envelope(effects: &[EffectClass], grant_ttl: i64) -> bool {
        Self::recognized_builtin_envelopes()
            .iter()
            .any(|(recognized_effects, recognized_ttl)| {
                effects == recognized_effects && grant_ttl == *recognized_ttl
            })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the attributed authority, immutable successor, selector CAS, and post-CAS verification form one auditable operation"
    )]
    fn upgrade_builtin_control_envelope_on(
        connection: &Connection,
        current: ControlPolicyProjection,
        now: DateTime<Utc>,
    ) -> Result<ControlPolicyProjection, StoreError> {
        if current.supported_effects == Self::builtin_control_effects()
            && current.grant_ttl_seconds == BUILTIN_CONTROL_GRANT_TTL_SECONDS
        {
            return Ok(current);
        }
        if !Self::is_recognized_builtin_envelope(
            &current.supported_effects,
            current.grant_ttl_seconds,
        ) {
            return Err(StoreError::InvalidControlProjection(
                "active control policy uses an unrecognized built-in envelope".into(),
            ));
        }
        let next_epoch = current.epoch.0.checked_add(1).ok_or_else(|| {
            StoreError::InvalidControlProjection("control policy epoch overflowed".into())
        })?;
        let old_envelope = serde_json::to_string(&current.supported_effects)?;
        let new_effects = Self::builtin_control_effects();
        let new_envelope = serde_json::to_string(&new_effects)?;
        let source = "engram:migration";
        let reason = format!(
            "upgrade the recognized built-in control envelope from {old_envelope} to {new_envelope}"
        );
        let authority = ProjectPolicyAuthorityDecision {
            schema_version: CONTROL_POLICY_AUTHORITY_SCHEMA_VERSION_V1,
            operation: ProjectPolicyOperation::UpgradeBuiltinEnvelope,
            policy_epoch: ProjectPolicyEpoch(next_epoch),
            previous_policy: Some(current.policy_hash.clone()),
            required_assurance: current.required_assurance,
            obligation_rule_set: current.obligation_rule_set.clone(),
            authorized_by: ActorContext {
                actor_id: source.into(),
                actor_kind: "system".into(),
                assurance: AssuranceLevel::Asserted,
                run_id: None,
                session_id: None,
                source_tool: Some(source.into()),
                source_skill: None,
                provenance_chain: Vec::new(),
                reason: reason.clone(),
            },
            reason,
            decided_at: now,
        };
        let authority_object = CanonicalObject::freeze(&authority)?;
        if authority_object.bytes().len() > MAX_CONTROL_POLICY_AUTHORITY_BYTES {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy authority exceeds the {MAX_CONTROL_POLICY_AUTHORITY_BYTES}-byte canonical limit"
            )));
        }
        Self::insert_object(
            connection,
            "project_policy_authority_decision",
            &authority_object,
        )?;
        let policy = ControlPolicy {
            schema_version: CONTROL_POLICY_SCHEMA_VERSION_V1,
            control_schema_version: CONTROL_POLICY_CONTROL_SCHEMA_VERSION_V1,
            policy_epoch: ProjectPolicyEpoch(next_epoch),
            previous_policy: Some(current.policy_hash.clone()),
            required_assurance: current.required_assurance,
            supported_effects: new_effects,
            grant_ttl_seconds: BUILTIN_CONTROL_GRANT_TTL_SECONDS,
            obligation_rule_set: current.obligation_rule_set,
            authority: authority_object.hash().clone(),
            activated_at: now,
        };
        Self::validate_migratable_active_control_policy(&policy)?;
        let policy_object = CanonicalObject::freeze(&policy)?;
        Self::insert_object(connection, "control_policy", &policy_object)?;
        connection.execute(
            "INSERT INTO control_policy_versions (
                 policy_hash, policy_epoch, authority_hash, policy_json
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                policy_object.hash().as_str(),
                policy.policy_epoch.0,
                authority_object.hash().as_str(),
                policy_object.bytes(),
            ],
        )?;
        let changed = connection.execute(
            "UPDATE control_policy_state SET
                 schema_version = ?1, policy_epoch = ?2,
                 required_assurance = ?3, supported_effects_json = ?4,
                 grant_ttl_seconds = ?5, policy_hash = ?6
             WHERE singleton = 1 AND policy_epoch = ?7 AND policy_hash = ?8",
            params![
                CONTROL_POLICY_STATE_SCHEMA_VERSION,
                policy.policy_epoch.0,
                enum_name(policy.required_assurance)?,
                serde_json::to_string(&policy.supported_effects)?,
                policy.grant_ttl_seconds,
                policy_object.hash().as_str(),
                current.epoch.0,
                current.policy_hash.as_str(),
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidControlProjection(
                "built-in control envelope compare-and-swap matched no row".into(),
            ));
        }
        let activated = Self::verify_control_policy_history(connection)?;
        if activated.policy_hash != *policy_object.hash() || activated.epoch != policy.policy_epoch
        {
            return Err(StoreError::InvalidControlProjection(
                "built-in control envelope upgrade failed integrity validation".into(),
            ));
        }
        Ok(activated)
    }

    fn insert_builtin_obligation_rule_set(
        connection: &Connection,
    ) -> Result<ObjectHash, StoreError> {
        let rule_set = crate::control::builtin_obligation_rule_set();
        Self::validate_obligation_rule_set(&rule_set)?;
        let object = CanonicalObject::freeze(&rule_set)?;
        Self::insert_object(connection, "obligation_rule_set", &object)?;
        Ok(object.hash().clone())
    }

    pub(super) fn load_obligation_rule_set_on(
        connection: &Connection,
        hash: &ObjectHash,
    ) -> Result<ObligationRuleSet, StoreError> {
        let bytes = Self::load_control_object_bytes(connection, hash, "obligation_rule_set")?;
        let rule_set: ObligationRuleSet = CanonicalObject::verify(hash, bytes)?.decode()?;
        Self::validate_obligation_rule_set(&rule_set)?;
        Ok(rule_set)
    }

    pub(super) fn obligation_rule_set_for_policy_on(
        connection: &Connection,
        policy_hash: &ObjectHash,
    ) -> Result<(Option<ObjectHash>, ObligationRuleSet), StoreError> {
        let (policy, _) = Self::load_control_policy_version(connection, policy_hash)?;
        match policy.obligation_rule_set {
            Some(hash) => {
                let rule_set = Self::load_obligation_rule_set_on(connection, &hash)?;
                Ok((Some(hash), rule_set))
            }
            None => Ok((None, crate::control::builtin_obligation_rule_set())),
        }
    }

    fn obligation_rule_set_for_policy_epoch_on(
        connection: &Connection,
        epoch: ProjectPolicyEpoch,
    ) -> Result<(Option<ObjectHash>, ObligationRuleSet), StoreError> {
        let stored_hash = connection
            .query_row(
                "SELECT policy_hash FROM control_policy_versions WHERE policy_epoch = ?1",
                [epoch.0],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .ok_or_else(|| {
                StoreError::InvalidControlProjection(format!(
                    "turn grant names missing control policy epoch {}",
                    epoch.0
                ))
            })?;
        let policy_hash = ObjectHash::from_stored(stored_hash.clone())
            .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
        Self::obligation_rule_set_for_policy_on(connection, &policy_hash)
    }

    fn validate_obligation_rule_set(rule_set: &ObligationRuleSet) -> Result<(), StoreError> {
        const MAX_RULES: usize = 64;
        let mut identities = HashSet::new();
        let valid = rule_set.schema_version == OBLIGATION_RULE_SET_SCHEMA_VERSION
            && rule_set.rules.len() <= MAX_RULES
            && rule_set.rules.iter().all(|definition| {
                let id = definition.rule.rule_id.as_str();
                !id.is_empty()
                    && id.len() <= 128
                    && id.trim() == id
                    && definition.rule.rule_version > 0
                    && !matches!(
                        definition.trigger,
                        crate::domain::BuiltinObligationTrigger::Unknown
                    )
                    && identities.insert((id.to_owned(), definition.rule.rule_version))
            });
        if !valid {
            return Err(StoreError::InvalidControlProjection(
                "canonical obligation rule set has an unsupported or ambiguous shape".into(),
            ));
        }
        Ok(())
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the migration successor, authority, selector CAS, and history verification form one atomic policy upgrade"
    )]
    fn upgrade_builtin_obligation_rules_on(
        connection: &Connection,
        current: ControlPolicyProjection,
        now: DateTime<Utc>,
    ) -> Result<ControlPolicyProjection, StoreError> {
        if let Some(hash) = current.obligation_rule_set.as_ref() {
            Self::load_obligation_rule_set_on(connection, hash)?;
            return Ok(current);
        }
        let rule_set_hash = Self::insert_builtin_obligation_rule_set(connection)?;
        let next_epoch = current.epoch.0.checked_add(1).ok_or_else(|| {
            StoreError::InvalidControlProjection("control policy epoch overflowed".into())
        })?;
        let source = "engram:migration";
        let reason = "select the canonical stock V1 obligation rule set";
        let authority = ProjectPolicyAuthorityDecision {
            schema_version: CONTROL_POLICY_AUTHORITY_SCHEMA_VERSION_V1,
            operation: ProjectPolicyOperation::UpgradeBuiltinObligationRules,
            policy_epoch: ProjectPolicyEpoch(next_epoch),
            previous_policy: Some(current.policy_hash.clone()),
            required_assurance: current.required_assurance,
            obligation_rule_set: Some(rule_set_hash.clone()),
            authorized_by: ActorContext {
                actor_id: source.into(),
                actor_kind: "system".into(),
                assurance: AssuranceLevel::Asserted,
                run_id: None,
                session_id: None,
                source_tool: Some(source.into()),
                source_skill: None,
                provenance_chain: Vec::new(),
                reason: reason.into(),
            },
            reason: reason.into(),
            decided_at: now,
        };
        let authority_object = CanonicalObject::freeze(&authority)?;
        Self::insert_object(
            connection,
            "project_policy_authority_decision",
            &authority_object,
        )?;
        let policy = ControlPolicy {
            schema_version: CONTROL_POLICY_SCHEMA_VERSION_V1,
            control_schema_version: CONTROL_POLICY_CONTROL_SCHEMA_VERSION_V1,
            policy_epoch: ProjectPolicyEpoch(next_epoch),
            previous_policy: Some(current.policy_hash.clone()),
            required_assurance: current.required_assurance,
            supported_effects: current.supported_effects,
            grant_ttl_seconds: current.grant_ttl_seconds,
            obligation_rule_set: Some(rule_set_hash),
            authority: authority_object.hash().clone(),
            activated_at: now,
        };
        Self::validate_active_control_policy(&policy)?;
        let policy_object = CanonicalObject::freeze(&policy)?;
        Self::insert_object(connection, "control_policy", &policy_object)?;
        connection.execute(
            "INSERT INTO control_policy_versions (
                 policy_hash, policy_epoch, authority_hash, policy_json
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                policy_object.hash().as_str(),
                policy.policy_epoch.0,
                authority_object.hash().as_str(),
                policy_object.bytes(),
            ],
        )?;
        let changed = connection.execute(
            "UPDATE control_policy_state SET
                 schema_version = ?1, policy_epoch = ?2,
                 required_assurance = ?3, supported_effects_json = ?4,
                 grant_ttl_seconds = ?5, policy_hash = ?6
             WHERE singleton = 1 AND policy_epoch = ?7 AND policy_hash = ?8",
            params![
                CONTROL_POLICY_STATE_SCHEMA_VERSION,
                policy.policy_epoch.0,
                enum_name(policy.required_assurance)?,
                serde_json::to_string(&policy.supported_effects)?,
                policy.grant_ttl_seconds,
                policy_object.hash().as_str(),
                current.epoch.0,
                current.policy_hash.as_str(),
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidControlProjection(
                "obligation rule-set policy upgrade compare-and-swap matched no row".into(),
            ));
        }
        Self::verify_control_policy_history(connection)
    }

    fn current_core_schema_is_complete(connection: &Connection) -> Result<bool, StoreError> {
        let mut statement = connection.prepare("SELECT name FROM sqlite_master")?;
        let stored = statement
            .query_map([], |row| row.get::<_, String>(0))?
            .collect::<Result<HashSet<_>, _>>()?;
        for object in [
            "objects",
            "publication_intents",
            "object_fts",
            "memory_heads",
            "memory_heads_scope",
            "memory_heads_work_scope",
            "note_intents",
            "project_context_revisions",
            "agent_context_revisions",
            "memory_contradictions",
            "memory_contradictions_versions",
            "memory_contradiction_edges",
            "memory_contradiction_edges_context",
            "contradiction_intents",
            "tasks",
            "task_participants",
            "session_bindings",
            "task_changes",
            "task_changes_task_cursor",
            "task_claims",
            "task_claim_intents",
            "control_observations",
            "control_observations_session_sequence",
            "control_policy_state",
            "task_control_state",
            "control_connections",
            "control_sessions",
            "control_turn_results",
            "control_turn_grants",
            "control_work_leases",
            "control_work_leases_task_state",
            "control_operation_results",
        ] {
            if !stored.contains(object) {
                return Ok(false);
            }
        }
        let has_work_id = connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM pragma_table_info('memory_heads') WHERE name = 'work_id'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if !has_work_id {
            return Ok(false);
        }
        let policy_schema: Option<i64> = connection
            .query_row(
                "SELECT schema_version FROM control_policy_state WHERE singleton = 1",
                [],
                |row| row.get(0),
            )
            .optional()?;
        if policy_schema == Some(CONTROL_POLICY_STATE_SCHEMA_VERSION) {
            let has_control_policy_hash = connection.query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM pragma_table_info('control_policy_state')
                     WHERE name = 'policy_hash'
                 )",
                [],
                |row| row.get::<_, bool>(0),
            )?;
            let has_versions = connection.query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM sqlite_master
                     WHERE type = 'table' AND name = 'control_policy_versions'
                 )",
                [],
                |row| row.get::<_, bool>(0),
            )?;
            let has_policy_operation_results =
                Self::control_policy_operation_results_table_is_complete(connection)?;
            if !has_control_policy_hash || !has_versions || !has_policy_operation_results {
                return Ok(false);
            }
        }
        connection
            .query_row(
                "SELECT EXISTS(SELECT 1 FROM control_policy_state WHERE singleton = 1)",
                [],
                |row| row.get::<_, bool>(0),
            )
            .map_err(StoreError::from)
    }

    fn migrate_memory_contradiction_edges(connection: &mut Connection) -> Result<(), StoreError> {
        if Self::supported_legacy_contradiction_rows_on(connection)?.is_empty() {
            return Ok(());
        }
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        for (contradiction, project, task, left, right) in
            Self::supported_legacy_contradiction_rows_on(&transaction)?
        {
            transaction.execute(
                "INSERT INTO memory_contradiction_edges (
                     contradiction_hash, project_id, task_id, work_root_id,
                     left_version_hash, right_version_hash
                 ) VALUES (?1, ?2, ?3, NULL, ?4, ?5)",
                params![contradiction, project, task, left, right],
            )?;
        }
        transaction.commit()?;
        Ok(())
    }

    fn migrate_control_work_bindings(connection: &mut Connection) -> Result<(), StoreError> {
        const COLUMNS: [&str; 6] = [
            "root_execution_id",
            "work_id",
            "run_id",
            "work_revision",
            "claim_id",
            "claim_fence",
        ];
        let column_count = connection.query_row(
            "SELECT COUNT(*) FROM pragma_table_info('control_sessions')
             WHERE name IN (
                 'root_execution_id', 'work_id', 'run_id', 'work_revision',
                 'claim_id', 'claim_fence'
             )",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        let index_exists = connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM sqlite_master
                 WHERE type = 'index' AND name = 'control_sessions_work_run'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        let column_count = usize::try_from(column_count).map_err(|_| {
            StoreError::InvalidControlProjection(
                "control session work-binding column count overflowed".into(),
            )
        })?;
        if column_count == COLUMNS.len() && index_exists {
            return Ok(());
        }
        if column_count != 0 && column_count != COLUMNS.len() {
            return Err(StoreError::InvalidControlProjection(
                "control session work-binding columns are only partially installed".into(),
            ));
        }
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if column_count == 0 {
            transaction.execute_batch(
                "ALTER TABLE control_sessions ADD COLUMN root_execution_id TEXT;
                 ALTER TABLE control_sessions ADD COLUMN work_id TEXT;
                 ALTER TABLE control_sessions ADD COLUMN run_id TEXT;
                 ALTER TABLE control_sessions ADD COLUMN work_revision INTEGER;
                 ALTER TABLE control_sessions ADD COLUMN claim_id TEXT;
                 ALTER TABLE control_sessions ADD COLUMN claim_fence INTEGER;",
            )?;
        }
        transaction.execute(
            "CREATE INDEX IF NOT EXISTS control_sessions_work_run
             ON control_sessions(project_id, run_id, session_id)",
            [],
        )?;
        transaction.commit()?;
        Ok(())
    }

    fn supported_legacy_contradiction_rows_on(
        connection: &Connection,
    ) -> Result<Vec<LegacyContradictionRow>, StoreError> {
        let rows = {
            let mut statement = connection.prepare(
                "SELECT legacy.contradiction_hash, task.project_id, legacy.task_id,
                        legacy.left_version_hash, legacy.right_version_hash
                 FROM memory_contradictions legacy
                 JOIN tasks task ON task.task_id = legacy.task_id
                 LEFT JOIN memory_contradiction_edges edge
                   ON edge.contradiction_hash = legacy.contradiction_hash
                 WHERE edge.contradiction_hash IS NULL
                 ORDER BY legacy.contradiction_hash",
            )?;
            let mapped = statement.query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, String>(3)?,
                    row.get::<_, String>(4)?,
                ))
            })?;
            mapped.collect::<Result<Vec<_>, _>>()?
        };
        let mut supported = Vec::new();
        for (contradiction, project, task, left, right) in rows {
            let contradiction_hash = ObjectHash::from_stored(contradiction.clone())
                .ok_or_else(|| StoreError::InvalidStoredHash(contradiction.clone()))?;
            let left_hash = ObjectHash::from_stored(left.clone())
                .ok_or_else(|| StoreError::InvalidStoredHash(left.clone()))?;
            let right_hash = ObjectHash::from_stored(right.clone())
                .ok_or_else(|| StoreError::InvalidStoredHash(right.clone()))?;
            let task_id = uuid::Uuid::parse_str(&task).map(TaskId).map_err(|_| {
                StoreError::InvalidMemoryProjection(format!(
                    "legacy contradiction has invalid task id {task}"
                ))
            })?;
            let object = Self::get_canonical_object_on(
                connection,
                &contradiction_hash,
                "memory_contradiction_event",
            )?
            .ok_or_else(|| {
                StoreError::InvalidMemoryProjection(format!(
                    "legacy contradiction {contradiction_hash} has no canonical object"
                ))
            })?;
            let value: serde_json::Value = serde_json::from_slice(object.bytes())?;
            if value
                .get("schema_version")
                .and_then(serde_json::Value::as_u64)
                != Some(u64::from(SCHEMA_VERSION))
            {
                continue;
            }
            let event: MemoryContradictionEvent = object.decode()?;
            if event.task_id != Some(task_id)
                || event.work_root_id.is_some()
                || event.left_version != left_hash
                || event.right_version != right_hash
                || event
                    .project_id
                    .as_ref()
                    .is_some_and(|event_project| event_project.0 != project)
            {
                return Err(StoreError::InvalidMemoryProjection(format!(
                    "legacy contradiction {contradiction_hash} differs from its canonical object"
                )));
            }
            supported.push((contradiction, project, task, left, right));
        }
        Ok(supported)
    }

    fn preflight_host_path_policy(
        connection: &Connection,
        expected: Option<HostPathPolicy>,
    ) -> Result<(), StoreError> {
        let stored = Self::stored_host_path_policy_on(connection)?;
        let Some(stored) = stored else {
            if Self::has_path_bearing_control_state(connection)? {
                return Err(StoreError::InvalidControlSession(
                    "path-bearing control state exists without a bound host path policy".into(),
                ));
            }
            return Ok(());
        };
        // An unresolved opener asserts nothing and may read; only a resolved
        // opener that disagrees with the persisted identity is refused.
        if let Some(expected) = expected
            && stored != expected
        {
            return Err(StoreError::InvalidControlSession(format!(
                "the store's persisted host path policy ({}) differs from this opener's ({}); if the project moved to a different filesystem, supply --host-path-policy matching the store or re-initialize a fresh store",
                describe_host_path_policy(stored),
                describe_host_path_policy(expected)
            )));
        }
        Ok(())
    }

    fn bind_host_path_policy(
        connection: &mut Connection,
        expected: Option<HostPathPolicy>,
    ) -> Result<(), StoreError> {
        let Some(expected) = expected else {
            Self::preflight_host_path_policy(connection, None)?;
            return Ok(());
        };
        let snapshot = connection.unchecked_transaction()?;
        let need = Self::host_path_policy_migration_need_on(&snapshot, expected)?;
        snapshot.commit()?;
        if need == OpenMigrationNeed::Current {
            return Ok(());
        }
        let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
        if Self::host_path_policy_migration_need_on(&transaction, expected)?
            == OpenMigrationNeed::NeedsWrite
        {
            Self::bind_host_path_policy_on(&transaction, expected)?;
        }
        transaction.commit()?;
        Ok(())
    }

    fn host_path_policy_migration_need_on(
        connection: &Connection,
        expected: HostPathPolicy,
    ) -> Result<OpenMigrationNeed, StoreError> {
        Self::preflight_host_path_policy(connection, Some(expected))?;
        Ok(if Self::stored_host_path_policy_on(connection)?.is_some() {
            OpenMigrationNeed::Current
        } else {
            OpenMigrationNeed::NeedsWrite
        })
    }

    fn bind_host_path_policy_on(
        connection: &Connection,
        expected: HostPathPolicy,
    ) -> Result<(), StoreError> {
        if Self::stored_host_path_policy_on(connection)? == Some(expected) {
            return Ok(());
        }
        connection.execute_batch(
            "CREATE TABLE IF NOT EXISTS control_host_path_policy (
                 singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
                 case_fold_paths INTEGER NOT NULL CHECK(case_fold_paths IN (0, 1)),
                 windows_alias_rules INTEGER NOT NULL CHECK(windows_alias_rules IN (0, 1))
             ) STRICT;",
        )?;
        if Self::has_path_bearing_control_state(connection)?
            && connection.query_row("SELECT COUNT(*) FROM control_host_path_policy", [], |row| {
                row.get::<_, i64>(0)
            })? == 0
        {
            return Err(StoreError::InvalidControlSession(
                "path-bearing control state exists without a bound host path policy".into(),
            ));
        }
        connection.execute(
            "INSERT OR IGNORE INTO control_host_path_policy (
                 singleton, case_fold_paths, windows_alias_rules
             ) VALUES (1, ?1, ?2)",
            params![
                i64::from(expected.case_fold_paths),
                i64::from(expected.windows_alias_rules)
            ],
        )?;
        Self::preflight_host_path_policy(connection, Some(expected))?;
        Ok(())
    }

    fn has_path_bearing_control_state(connection: &Connection) -> Result<bool, StoreError> {
        for (table, column) in [
            ("control_work_leases", "lease_json"),
            ("control_turn_grants", "grant_json"),
        ] {
            let table_exists = connection.query_row(
                "SELECT EXISTS(SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = ?1)",
                [table],
                |row| row.get::<_, bool>(0),
            )?;
            if table_exists {
                let query = format!(
                    "SELECT EXISTS(SELECT 1 FROM {table} WHERE CAST({column} AS TEXT) LIKE '%\"kind\":\"path\"%')"
                );
                if connection.query_row(&query, [], |row| row.get::<_, bool>(0))? {
                    return Ok(true);
                }
            }
        }
        Ok(false)
    }

    fn verify_task_change_cursor_schema(connection: &Connection) -> Result<(), StoreError> {
        let has_task_cursor = connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM pragma_table_info('task_changes')
                 WHERE name = 'task_cursor'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )?;
        if has_task_cursor {
            return Ok(());
        }
        Err(StoreError::InvalidTaskProjection(
            "legacy global task cursors cannot be renumbered safely; export the old store and explicitly rebind/reset sessions into a fresh task-local-cursor store".into(),
        ))
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
        let receipt = Self::bind_task_on(
            &transaction,
            project_id,
            external_ref,
            create_title,
            participant,
            actor,
            now,
        )?;
        transaction.commit()?;
        Ok(receipt)
    }

    fn bind_task_on(
        transaction: &Transaction<'_>,
        project_id: &crate::domain::ProjectId,
        external_ref: &str,
        create_title: Option<&str>,
        participant: &SessionId,
        actor: ActorContext,
        now: DateTime<Utc>,
    ) -> Result<TaskBindReceipt, StoreError> {
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
                Self::insert_object(transaction, "task_joined_event", &object)?;
                Self::insert_task_change(transaction, task_id, "task_joined_event", &object)?
            } else {
                Self::latest_task_cursor(transaction, task_id)?
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
            Self::insert_object(transaction, "task_started_event", &object)?;
            let cursor =
                Self::insert_task_change(transaction, task_id, "task_started_event", &object)?;
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
        let task = Self::load_task(transaction, task_id)?;
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

    /// Binds a host-private control session to a local task and rotates any
    /// prior live, unbegun authority for that runtime session.
    ///
    /// The returned routing token prevents accidental cross-session request
    /// mix-ups. It is asserted host state, not authentication.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the bind is invalid, conflicts with an
    /// earlier request key, or cannot be persisted safely.
    #[allow(clippy::too_many_arguments)]
    pub fn bind_control_session(
        &mut self,
        project_id: &crate::domain::ProjectId,
        external_ref: &str,
        title: &str,
        session_id: &SessionId,
        connection_token: &str,
        actor: &ActorContext,
        assurance: ControlAssurance,
        mediated_effects: &[EffectClass],
        capability_map_revision: i64,
        idempotency_key: &str,
        now: DateTime<Utc>,
    ) -> Result<ControlSessionBinding, StoreError> {
        self.bind_control_session_with_work(
            project_id,
            external_ref,
            title,
            session_id,
            connection_token,
            actor,
            None,
            assurance,
            mediated_effects,
            capability_map_revision,
            idempotency_key,
            now,
        )
    }

    /// Binds a host-private control session to both its compatibility task and
    /// an exact live local-work claim basis.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the work/run/root/claim basis is stale,
    /// belongs to another session or project, or the bind cannot be persisted.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "the bind validates and rotates one auditable session projection transaction"
    )]
    pub fn bind_control_session_with_work(
        &mut self,
        project_id: &crate::domain::ProjectId,
        external_ref: &str,
        title: &str,
        session_id: &SessionId,
        connection_token: &str,
        actor: &ActorContext,
        work_binding: Option<&ControlWorkBinding>,
        assurance: ControlAssurance,
        mediated_effects: &[EffectClass],
        capability_map_revision: i64,
        idempotency_key: &str,
        now: DateTime<Utc>,
    ) -> Result<ControlSessionBinding, StoreError> {
        let external_ref = external_ref.trim();
        let title = title.trim();
        let idempotency_key = idempotency_key.trim();
        let effects_are_unique = mediated_effects
            .iter()
            .collect::<std::collections::HashSet<_>>()
            .len()
            == mediated_effects.len();
        if external_ref.is_empty()
            || title.is_empty()
            || idempotency_key.is_empty()
            || session_id.0.trim().is_empty()
            || actor.session_id.as_ref() != Some(session_id)
            || actor.run_id.as_deref()
                != work_binding
                    .map(|binding| binding.run_id.0.to_string())
                    .as_deref()
            || capability_map_revision < 0
            || mediated_effects.is_empty()
            || !effects_are_unique
            || matches!(assurance, ControlAssurance::ActionGated)
        {
            return Err(StoreError::InvalidControlSession(
                "bind fields, actor session, mediated effects, or capability revision are invalid"
                    .into(),
            ));
        }
        let bind_intent = CanonicalObject::freeze(&ControlSessionBindFingerprint {
            control_schema_version: CONTROL_SCHEMA_VERSION,
            project_id,
            external_ref,
            title,
            session_id,
            actor,
            assurance,
            mediated_effects,
            work_binding,
            capability_map_revision,
            idempotency_key,
        })?;

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::verify_control_connection(&transaction, session_id, connection_token)?;
        let existing = Self::load_control_session_on(&transaction, session_id)?;
        if let Some(existing) = &existing
            && existing.bind_key == idempotency_key
        {
            if existing.bind_intent_hash != bind_intent.hash().as_str() {
                return Err(StoreError::ControlSessionBindConflict(
                    idempotency_key.into(),
                ));
            }
            let binding = ControlSessionBinding {
                routing_token: existing.routing_token.clone(),
                effective_mediated_effects: effective_mediated_effects(
                    existing.assurance,
                    &existing.mediated_effects,
                ),
                status: Self::control_session_status_on(&transaction, existing)?,
            };
            transaction.commit()?;
            return Ok(binding);
        }
        if let Some(work_binding) = work_binding {
            work::validate_control_work_binding_on(
                &transaction,
                project_id,
                session_id,
                work_binding,
                now,
            )?;
        }
        if let Some(existing) = &existing {
            if matches!(existing.phase, SessionPhase::TurnOpen)
                && Self::session_has_begun_turn(&transaction, session_id)?
            {
                return Err(StoreError::InvalidControlSession(
                    "a begun turn must be checkpointed before rebinding".into(),
                ));
            }
            let target_task: Option<String> = transaction
                .query_row(
                    "SELECT task_id FROM tasks WHERE project_id = ?1 AND external_ref = ?2",
                    params![project_id.0, external_ref],
                    |row| row.get(0),
                )
                .optional()?;
            let changes_task = target_task
                .as_deref()
                .is_none_or(|task_id| task_id != existing.task_id.0.to_string());
            Self::terminalize_session_work_leases(&transaction, existing, now, true)?;
            if changes_task
                && transaction.query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM control_work_leases
                         WHERE holder_session_id = ?1 AND state = 'active'
                           AND expires_at_ms > ?2
                     )",
                    params![session_id.0.as_str(), now.timestamp_millis()],
                    |row| row.get::<_, i64>(0),
                )? == 1
            {
                return Err(StoreError::InvalidControlSession(
                    "release every active work lease before rebinding to another task".into(),
                ));
            }
        }

        let task = Self::bind_task_on(
            &transaction,
            project_id,
            external_ref,
            Some(title),
            session_id,
            actor.clone(),
            now,
        )?;
        let policy = Self::load_active_control_policy(&transaction)?;
        transaction.execute(
            "INSERT OR IGNORE INTO task_control_state (task_id, admission_epoch)
             VALUES (?1, 1)",
            [task.task.task_id.0.to_string()],
        )?;
        let admission_epoch = transaction.query_row(
            "SELECT admission_epoch FROM task_control_state WHERE task_id = ?1",
            [task.task.task_id.0.to_string()],
            |row| row.get::<_, i64>(0),
        )?;
        let previous_revision = existing.as_ref().map_or(0, |session| session.revision);
        let routing_token = uuid::Uuid::now_v7().to_string();
        let head = Self::latest_task_cursor(&transaction, task.task.task_id)?;
        transaction.execute(
            "UPDATE control_turn_grants SET state = 'expired'
             WHERE session_id = ?1 AND state = 'issued'",
            [session_id.0.as_str()],
        )?;
        transaction.execute(
            "INSERT INTO control_sessions (
                 session_id, project_id, task_id, root_execution_id, work_id,
                 run_id, work_revision, claim_id, claim_fence, routing_token,
                 actor_json, bind_key, bind_intent_hash, bind_intent_json, phase,
                 assurance, mediated_effects_json, confirmed_cursor,
                 tentative_cursor, project_policy_epoch, task_admission_epoch,
                 blocking_watermark, capability_map_revision, revision, updated_at_ms
             ) VALUES (
                 ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                 ?14, 'sync_required', ?15, ?16, 0, NULL, ?17, ?18, ?19, ?20,
                 ?21, ?22
             )
             ON CONFLICT(session_id) DO UPDATE SET
                 project_id = excluded.project_id,
                 task_id = excluded.task_id,
                 root_execution_id = excluded.root_execution_id,
                 work_id = excluded.work_id,
                 run_id = excluded.run_id,
                 work_revision = excluded.work_revision,
                 claim_id = excluded.claim_id,
                 claim_fence = excluded.claim_fence,
                 routing_token = excluded.routing_token,
                 actor_json = excluded.actor_json,
                 bind_key = excluded.bind_key,
                 bind_intent_hash = excluded.bind_intent_hash,
                 bind_intent_json = excluded.bind_intent_json,
                 phase = excluded.phase,
                 assurance = excluded.assurance,
                 mediated_effects_json = excluded.mediated_effects_json,
                 confirmed_cursor = excluded.confirmed_cursor,
                 tentative_cursor = excluded.tentative_cursor,
                 project_policy_epoch = excluded.project_policy_epoch,
                 task_admission_epoch = excluded.task_admission_epoch,
                 blocking_watermark = excluded.blocking_watermark,
                 capability_map_revision = excluded.capability_map_revision,
                 revision = excluded.revision,
                 updated_at_ms = excluded.updated_at_ms",
            params![
                session_id.0,
                project_id.0,
                task.task.task_id.0.to_string(),
                work_binding.map(|binding| binding.root_execution_id.0.to_string()),
                work_binding.map(|binding| binding.work_id.0.to_string()),
                work_binding.map(|binding| binding.run_id.0.to_string()),
                work_binding.map(|binding| binding.work_revision),
                work_binding.map(|binding| binding.claim_id.0.to_string()),
                work_binding.map(|binding| binding.claim_fence),
                routing_token,
                serde_json::to_vec(actor)?,
                idempotency_key,
                bind_intent.hash().as_str(),
                bind_intent.bytes(),
                enum_name(assurance)?,
                serde_json::to_string(mediated_effects)?,
                policy.epoch.0,
                admission_epoch,
                head.0,
                capability_map_revision,
                previous_revision + 1,
                now.timestamp_millis(),
            ],
        )?;
        let stored = Self::load_control_session_on(&transaction, session_id)?
            .ok_or_else(|| StoreError::ControlSessionNotBound(session_id.0.clone()))?;
        let binding = ControlSessionBinding {
            routing_token: stored.routing_token.clone(),
            effective_mediated_effects: effective_mediated_effects(
                stored.assurance,
                &stored.mediated_effects,
            ),
            status: Self::control_session_status_on(&transaction, &stored)?,
        };
        transaction.commit()?;
        Ok(binding)
    }

    /// Returns current host-control state after validating the routing token.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] for an unknown session, wrong project, or token
    /// mismatch.
    pub fn control_status(
        &mut self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        connection_token: &str,
        routing_token: &str,
        now: DateTime<Utc>,
    ) -> Result<ControlSessionStatus, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::verify_control_connection(&transaction, session_id, connection_token)?;
        let mut stored = Self::load_control_session_on(&transaction, session_id)?
            .ok_or_else(|| StoreError::ControlSessionNotBound(session_id.0.clone()))?;
        Self::verify_control_session(&stored, project_id, routing_token)?;
        if Self::expire_unbegun_turn(&transaction, &stored, now)? {
            stored = Self::load_control_session_on(&transaction, session_id)?
                .ok_or_else(|| StoreError::ControlSessionNotBound(session_id.0.clone()))?;
        }
        let status = Self::control_session_status_on(&transaction, &stored)?;
        transaction.commit()?;
        Ok(status)
    }

    /// Invalidates authority that was issued but never begun when a new
    /// host-control connection takes ownership of the runtime session.
    /// Begun turns remain checkpoint-required so an uncertain prompt outcome
    /// cannot be silently replayed.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the restart transition cannot be persisted.
    pub fn resume_control_connection(
        &mut self,
        session_id: &SessionId,
        now: DateTime<Utc>,
    ) -> Result<String, StoreError> {
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        let connection_token = uuid::Uuid::now_v7().to_string();
        transaction.execute(
            "INSERT INTO control_connections (session_id, connection_token, opened_at_ms)
             VALUES (?1, ?2, ?3)
             ON CONFLICT(session_id) DO UPDATE SET
                 connection_token = excluded.connection_token,
                 opened_at_ms = excluded.opened_at_ms",
            params![session_id.0, connection_token, now.timestamp_millis()],
        )?;
        let Some(session) = Self::load_control_session_on(&transaction, session_id)? else {
            transaction.commit()?;
            return Ok(connection_token);
        };
        let invalidated = transaction.execute(
            "UPDATE control_turn_grants SET state = 'expired'
             WHERE session_id = ?1 AND state = 'issued'",
            [session_id.0.as_str()],
        )?;
        if invalidated > 0
            && matches!(session.phase, SessionPhase::TurnOpen)
            && !Self::session_has_begun_turn(&transaction, session_id)?
        {
            transaction.execute(
                "UPDATE control_sessions SET
                     phase = 'sync_required', tentative_cursor = NULL,
                     revision = revision + 1, updated_at_ms = ?2
                 WHERE session_id = ?1",
                params![session_id.0, now.timestamp_millis()],
            )?;
        }
        transaction.commit()?;
        Ok(connection_token)
    }

    /// Atomically acquires one normalized resource lease for a synchronized
    /// control session.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] for invalid routing, unsafe resource shapes,
    /// unsynchronized sessions, idempotency conflicts, or persistence errors.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "lease acquisition validates routing, synchronization, conflicts, and audit event atomically"
    )]
    pub fn acquire_work_lease(
        &mut self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        connection_token: &str,
        routing_token: &str,
        kind: crate::domain::LeaseKind,
        mode: crate::domain::LeaseMode,
        subject: &crate::domain::ResourceSubject,
        ttl_seconds: i64,
        idempotency_key: &str,
        now: DateTime<Utc>,
    ) -> Result<WorkLeaseDecision, StoreError> {
        if !(1..=3_600).contains(&ttl_seconds) || idempotency_key.trim().is_empty() {
            return Err(StoreError::InvalidControlSession(
                "lease TTL or idempotency key is invalid".into(),
            ));
        }
        let subject = subject
            .normalized_for_project_with_policy(project_id, self.path_policy_for(subject)?)
            .ok_or_else(|| {
                StoreError::InvalidControlSession(
                    "lease subject is invalid or belongs to another project".into(),
                )
            })?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::verify_control_connection(&transaction, session_id, connection_token)?;
        let session = Self::load_control_session_on(&transaction, session_id)?
            .ok_or_else(|| StoreError::ControlSessionNotBound(session_id.0.clone()))?;
        Self::verify_control_session(&session, project_id, routing_token)?;
        let intent = CanonicalObject::freeze(&WorkLeaseAcquireFingerprint {
            fingerprint_schema_version: WORK_LEASE_ACQUIRE_FINGERPRINT_SCHEMA_VERSION,
            session_id,
            bind_intent_hash: &session.bind_intent_hash,
            kind,
            mode,
            subject: &subject,
            ttl_seconds,
            idempotency_key,
        })?;
        if let Some(replay) = Self::replay_control_operation(
            &transaction,
            session_id,
            "lease_acquire",
            idempotency_key,
            intent.hash(),
        )? {
            transaction.commit()?;
            return Ok(replay);
        }
        let policy = Self::load_active_control_policy(&transaction)?;
        let lease_effect = match kind {
            crate::domain::LeaseKind::Execution => EffectClass::MutateLocal,
            crate::domain::LeaseKind::Coordination => EffectClass::Coordinate,
        };
        let directive_key = format!("lease_acquire:{idempotency_key}");
        if let Err(refusal) = evaluate_lease_policy(&LeasePolicyInput {
            request_key: &directive_key,
            host_assurance: session.assurance,
            declared_mediated_effects: &session.mediated_effects,
            project_required_assurance: policy.required_assurance,
            policy_effects: &policy.supported_effects,
            session_policy_epoch: session.epochs.project_policy,
            active_policy_epoch: policy.epoch,
            effect: lease_effect,
        }) {
            let decision = Self::refuse_work_lease(
                &transaction,
                session_id,
                idempotency_key,
                &intent,
                refusal.directive,
                now,
            )?;
            if refusal.adopt_project_policy_epoch {
                transaction.execute(
                    "UPDATE control_sessions SET
                         project_policy_epoch = ?2,
                         revision = revision + 1, updated_at_ms = ?3
                     WHERE session_id = ?1",
                    params![session_id.0, policy.epoch.0, now.timestamp_millis()],
                )?;
            }
            transaction.commit()?;
            return Ok(decision);
        }
        let head = Self::latest_task_cursor(&transaction, session.task_id)?;
        if !matches!(session.phase, SessionPhase::Ready)
            || session.confirmed_cursor != head
            || !Self::session_is_current_participant(
                &transaction,
                project_id,
                session.task_id,
                session_id,
            )?
            || !matches!(
                Self::task_state_on(&transaction, project_id, session.task_id)?,
                TaskState::Active
            )
        {
            return Err(StoreError::InvalidControlSession(
                "lease acquisition requires a synchronized ready participant on an active task"
                    .into(),
            ));
        }

        let rows = Self::project_work_lease_rows(&transaction, project_id)?;
        let decoded = rows
            .iter()
            .map(Self::decode_work_lease_row)
            .collect::<Result<Vec<_>, _>>()?;
        let mut active = Vec::new();
        let mut expired_predecessor = false;
        for (row, lease) in rows.iter().zip(&decoded) {
            if row.state != "active" {
                continue;
            }
            let checkpoint_required =
                Self::begun_turn_pinning_lease(&transaction, &lease.holder, &lease.lease_id)?
                    .is_some();
            if lease.expires_at <= now
                && !checkpoint_required
                && Self::resource_subjects_overlap(&lease.subject, &subject)
            {
                Self::terminalize_work_lease(
                    &transaction,
                    row,
                    lease.clone(),
                    WorkLeaseTransition::Expired,
                    &session.actor,
                    now,
                )?;
                expired_predecessor = true;
                continue;
            }
            if lease.expires_at > now || checkpoint_required {
                active.push((lease, checkpoint_required));
            }
        }
        if active.iter().any(|(lease, _)| {
            lease.holder == *session_id && Self::resource_subjects_overlap(&lease.subject, &subject)
        }) {
            return Err(StoreError::InvalidControlSession(
                "the session already holds an overlapping lease with a different basis".into(),
            ));
        }
        if let Some((conflict, checkpoint_required)) = active.iter().find(|(lease, _)| {
            lease.holder != *session_id && Self::resource_subjects_overlap(&lease.subject, &subject)
        }) {
            let decision = WorkLeaseDecision::Defer {
                holder: conflict.holder.clone(),
                conflicting_lease_id: conflict.lease_id.clone(),
                expires_at: conflict.expires_at,
                checkpoint_required: *checkpoint_required,
            };
            Self::persist_control_operation(
                &transaction,
                session_id,
                "lease_acquire",
                idempotency_key,
                &intent,
                &decision,
                now,
            )?;
            transaction.commit()?;
            return Ok(decision);
        }
        let fence = decoded
            .iter()
            .filter(|lease| Self::resource_subjects_overlap(&lease.subject, &subject))
            .map(|lease| lease.fence)
            .max()
            .unwrap_or(0)
            + 1;
        let lease = WorkLease {
            control_schema_version: CONTROL_SCHEMA_VERSION,
            lease_id: uuid::Uuid::now_v7().to_string(),
            task_id: session.task_id,
            holder: session_id.clone(),
            kind,
            mode,
            subject,
            fence,
            revision: 1,
            idempotency_key: idempotency_key.into(),
            expires_at: now + chrono::TimeDelta::seconds(ttl_seconds),
        };
        let lease_object = CanonicalObject::freeze(&lease)?;
        transaction.execute(
            "INSERT INTO control_work_leases (
                 lease_id, task_id, holder_session_id, lease_hash, lease_json,
                 state, expires_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, 'active', ?6)",
            params![
                lease.lease_id,
                lease.task_id.0.to_string(),
                lease.holder.0,
                lease_object.hash().as_str(),
                lease_object.bytes(),
                lease.expires_at.timestamp_millis(),
            ],
        )?;
        let event = WorkLeaseEvent {
            schema_version: SCHEMA_VERSION,
            task_id: session.task_id,
            lease: lease.clone(),
            transition: WorkLeaseTransition::Acquired,
            actor: session.actor.clone(),
            created_at: now,
        };
        let event_object = CanonicalObject::freeze(&event)?;
        Self::insert_object(&transaction, "work_lease_event", &event_object)?;
        let cursor = Self::insert_task_change(
            &transaction,
            session.task_id,
            "work_lease_event",
            &event_object,
        )?;
        transaction.execute(
            "UPDATE control_sessions SET
                 confirmed_cursor = ?2, blocking_watermark = ?3,
                 revision = revision + 1, updated_at_ms = ?4
             WHERE session_id = ?1",
            params![
                session_id.0,
                if expired_predecessor {
                    session.confirmed_cursor.0
                } else {
                    cursor.0
                },
                cursor.0,
                now.timestamp_millis()
            ],
        )?;
        let decision = WorkLeaseDecision::Granted { lease };
        Self::persist_control_operation(
            &transaction,
            session_id,
            "lease_acquire",
            idempotency_key,
            &intent,
            &decision,
            now,
        )?;
        transaction.commit()?;
        Ok(decision)
    }

    /// Releases one held resource lease and appends its fenced transition.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] for invalid routing, ownership, idempotency, or
    /// persistence failures.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "lease release updates the projection and task-feed audit event atomically"
    )]
    pub fn release_work_lease(
        &mut self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        connection_token: &str,
        routing_token: &str,
        lease_id: &str,
        idempotency_key: &str,
        now: DateTime<Utc>,
    ) -> Result<WorkLeaseReleaseReceipt, StoreError> {
        let intent = CanonicalObject::freeze(&WorkLeaseReleaseFingerprint {
            control_schema_version: CONTROL_SCHEMA_VERSION,
            session_id,
            lease_id,
            idempotency_key,
        })?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::verify_control_connection(&transaction, session_id, connection_token)?;
        let session = Self::load_control_session_on(&transaction, session_id)?
            .ok_or_else(|| StoreError::ControlSessionNotBound(session_id.0.clone()))?;
        Self::verify_control_session(&session, project_id, routing_token)?;
        if let Some(replay) = Self::replay_control_operation(
            &transaction,
            session_id,
            "lease_release",
            idempotency_key,
            intent.hash(),
        )? {
            transaction.commit()?;
            return Ok(replay);
        }
        let row = Self::work_lease_row(&transaction, lease_id)?
            .ok_or_else(|| StoreError::WorkLeaseNotFound(lease_id.into()))?;
        let lease = Self::decode_work_lease_row(&row)?;
        if lease.holder != *session_id || lease.task_id != session.task_id {
            return Err(StoreError::WorkLeaseNotHeld {
                lease_id: lease_id.into(),
                session: session_id.0.clone(),
            });
        }
        if row.state == "expired" {
            return Err(StoreError::WorkLeaseExpired {
                lease_id: lease_id.into(),
                expired_at: lease.expires_at,
            });
        }
        if row.state != "active" {
            return Err(StoreError::WorkLeaseNotHeld {
                lease_id: lease_id.into(),
                session: session_id.0.clone(),
            });
        }
        if let Some(grant_id) = Self::begun_turn_pinning_lease(&transaction, session_id, lease_id)?
        {
            return Err(StoreError::InvalidControlSession(format!(
                "work lease {lease_id:?} is pinned by begun turn {grant_id:?}; checkpoint the turn before releasing the lease"
            )));
        }
        if lease.expires_at <= now {
            let cursor = Self::terminalize_work_lease(
                &transaction,
                &row,
                lease.clone(),
                WorkLeaseTransition::Expired,
                &session.actor,
                now,
            )?;
            transaction.execute(
                "UPDATE control_sessions SET blocking_watermark = ?2,
                     revision = revision + 1, updated_at_ms = ?3
                 WHERE session_id = ?1",
                params![session_id.0, cursor.0, now.timestamp_millis()],
            )?;
            transaction.commit()?;
            return Err(StoreError::WorkLeaseExpired {
                lease_id: lease_id.into(),
                expired_at: lease.expires_at,
            });
        }
        let head = Self::latest_task_cursor(&transaction, session.task_id)?;
        let cursor = Self::terminalize_work_lease(
            &transaction,
            &row,
            lease.clone(),
            WorkLeaseTransition::Released,
            &session.actor,
            now,
        )?;
        let confirmed_cursor =
            if session.confirmed_cursor == head && matches!(session.phase, SessionPhase::Ready) {
                cursor
            } else {
                session.confirmed_cursor
            };
        transaction.execute(
            "UPDATE control_sessions SET
                 confirmed_cursor = ?2, blocking_watermark = ?3,
                 revision = revision + 1, updated_at_ms = ?4
             WHERE session_id = ?1",
            params![
                session_id.0,
                confirmed_cursor.0,
                cursor.0,
                now.timestamp_millis(),
            ],
        )?;
        let receipt = WorkLeaseReleaseReceipt {
            lease_id: lease_id.into(),
            task_id: session.task_id,
            holder: session_id.clone(),
            fence: lease.fence,
            cursor,
            released_at: now,
        };
        Self::persist_control_operation(
            &transaction,
            session_id,
            "lease_release",
            idempotency_key,
            &intent,
            &receipt,
            now,
        )?;
        transaction.commit()?;
        Ok(receipt)
    }

    fn terminalize_session_work_leases(
        transaction: &Transaction<'_>,
        session: &StoredControlSession,
        now: DateTime<Utc>,
        expired_only: bool,
    ) -> Result<Vec<ChangeCursor>, StoreError> {
        let rows = {
            let mut statement = transaction.prepare(
                "SELECT lease_id, task_id, holder_session_id, lease_hash, lease_json,
                        state, expires_at_ms
                 FROM control_work_leases
                 WHERE holder_session_id = ?1 AND state = 'active'
                   AND (?2 = 0 OR expires_at_ms <= ?3)
                 ORDER BY lease_id",
            )?;
            statement
                .query_map(
                    params![
                        session.session_id.0,
                        i64::from(expired_only),
                        now.timestamp_millis()
                    ],
                    |row| {
                        Ok(StoredWorkLeaseRow {
                            lease_id: row.get(0)?,
                            task_id: row.get(1)?,
                            holder_session_id: row.get(2)?,
                            lease_hash: row.get(3)?,
                            lease_json: row.get(4)?,
                            state: row.get(5)?,
                            expires_at_ms: row.get(6)?,
                        })
                    },
                )?
                .collect::<Result<Vec<_>, _>>()?
        };
        let mut cursors = Vec::with_capacity(rows.len());
        for row in rows {
            let lease = Self::decode_work_lease_row(&row)?;
            if lease.task_id != session.task_id || lease.holder != session.session_id {
                return Err(StoreError::InvalidControlProjection(format!(
                    "work lease {} is not bound to its holder session",
                    lease.lease_id
                )));
            }
            let expired = lease.expires_at <= now;
            let transition = if expired {
                WorkLeaseTransition::Expired
            } else {
                WorkLeaseTransition::Released
            };
            cursors.push(Self::terminalize_work_lease(
                transaction,
                &row,
                lease,
                transition,
                &session.actor,
                now,
            )?);
        }
        Ok(cursors)
    }

    fn terminalize_work_lease(
        transaction: &Transaction<'_>,
        row: &StoredWorkLeaseRow,
        mut lease: WorkLease,
        transition: WorkLeaseTransition,
        actor: &ActorContext,
        now: DateTime<Utc>,
    ) -> Result<ChangeCursor, StoreError> {
        let state = match transition {
            WorkLeaseTransition::Released => "released",
            WorkLeaseTransition::Expired => "expired",
            WorkLeaseTransition::Acquired => {
                return Err(StoreError::InvalidControlProjection(
                    "an acquired work lease cannot be terminalized".into(),
                ));
            }
        };
        if row.state != "active" || row.lease_id != lease.lease_id {
            return Err(StoreError::InvalidControlProjection(format!(
                "work lease {} was not active during terminalization",
                lease.lease_id
            )));
        }
        lease.revision += 1;
        let lease_object = CanonicalObject::freeze(&lease)?;
        let changed = transaction.execute(
            "UPDATE control_work_leases SET lease_hash = ?2, lease_json = ?3, state = ?4
             WHERE lease_id = ?1 AND state = 'active'",
            params![
                lease.lease_id,
                lease_object.hash().as_str(),
                lease_object.bytes(),
                state
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidControlProjection(format!(
                "work lease {} was not active during terminalization",
                lease.lease_id
            )));
        }
        let event = WorkLeaseEvent {
            schema_version: SCHEMA_VERSION,
            task_id: lease.task_id,
            lease,
            transition,
            actor: actor.clone(),
            created_at: now,
        };
        let event_object = CanonicalObject::freeze(&event)?;
        Self::insert_object(transaction, "work_lease_event", &event_object)?;
        Self::insert_task_change(
            transaction,
            event.task_id,
            "work_lease_event",
            &event_object,
        )
    }

    /// Evaluates and persists one host-enforced turn request from durable
    /// policy, membership, lifecycle, and context state.
    ///
    /// The built-in alpha policy grants `observe`, `communicate`, and
    /// turn-gated `mutate_local`. Local mutation requires a live exclusive
    /// execution lease covering every declared resource intent.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] for invalid routing, idempotency conflicts,
    /// corrupt projections, or persistence failures.
    #[allow(
        clippy::too_many_lines,
        reason = "evaluation snapshots context and persists the decision and grant atomically"
    )]
    pub fn evaluate_control_turn(
        &mut self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        connection_token: &str,
        routing_token: &str,
        intent: &TurnIntent,
        now: DateTime<Utc>,
    ) -> Result<ControlTurnDecision, StoreError> {
        let mut intent = intent.clone();
        intent.resource_intents = intent
            .resource_intents
            .iter()
            .map(|resource| {
                self.path_policy_for(resource)
                    .map(|policy| resource.normalized_for_project_with_policy(project_id, policy))
            })
            .collect::<Result<Option<Vec<_>>, StoreError>>()?
            .ok_or_else(|| {
                StoreError::InvalidControlSession(
                    "turn resource intent is invalid or belongs to another project".into(),
                )
            })?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::verify_control_connection(&transaction, session_id, connection_token)?;
        let mut session = Self::load_control_session_on(&transaction, session_id)?
            .ok_or_else(|| StoreError::ControlSessionNotBound(session_id.0.clone()))?;
        Self::verify_control_session(&session, project_id, routing_token)?;
        let intent_object = CanonicalObject::freeze(&TurnObservationIntentFingerprint {
            control_schema_version: CONTROL_SCHEMA_VERSION,
            session_id,
            task_id: Some(session.task_id),
            intent: &intent,
        })?;
        if let Some((stored_intent_hash, decision_hash, decision_json)) = transaction
            .query_row(
                "SELECT intent_hash, decision_hash, decision_json
                 FROM control_turn_results
                 WHERE session_id = ?1 AND idempotency_key = ?2",
                params![session_id.0, intent.idempotency_key],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .optional()?
        {
            if stored_intent_hash != intent_object.hash().as_str() {
                return Err(StoreError::ControlTurnIdempotencyConflict(
                    intent.idempotency_key.clone(),
                ));
            }
            let decision = Self::decode_canonical_projection(&decision_hash, decision_json)?;
            transaction.commit()?;
            return Ok(decision);
        }

        if Self::expire_unbegun_turn(&transaction, &session, now)? {
            session = Self::load_control_session_on(&transaction, session_id)?
                .ok_or_else(|| StoreError::ControlSessionNotBound(session_id.0.clone()))?;
        }
        let policy = Self::load_active_control_policy(&transaction)?;
        let task_state = transaction
            .query_row(
                "SELECT state FROM tasks WHERE task_id = ?1 AND project_id = ?2",
                params![session.task_id.0.to_string(), project_id.0],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|state| parse_enum::<TaskState>(&state))
            .transpose()?;
        let membership = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM task_participants
                 WHERE task_id = ?1 AND session_id = ?2
             ) AND EXISTS(
                 SELECT 1 FROM session_bindings
                 WHERE task_id = ?1 AND session_id = ?2
             )",
            params![session.task_id.0.to_string(), session_id.0],
            |row| row.get::<_, i64>(0),
        )?;
        let task_admission_epoch = transaction.query_row(
            "SELECT admission_epoch FROM task_control_state WHERE task_id = ?1",
            [session.task_id.0.to_string()],
            |row| row.get::<_, i64>(0),
        )?;
        let head = Self::latest_task_cursor(&transaction, session.task_id)?;
        let page_to = if membership == 1 && session.confirmed_cursor < head {
            Some(Self::task_delivery_page_end(
                &transaction,
                session.task_id,
                session.confirmed_cursor,
            )?)
        } else {
            None
        };
        let has_more = page_to.is_some_and(|page_to| page_to < head);
        let (mut packet_safety, context) = if membership == 1 && !has_more {
            match Self::build_context_on(
                &transaction,
                project_id,
                Some(session.task_id),
                session_id,
                &session.actor.actor_id,
                now,
            ) {
                Ok(packet) => (PacketSafety::Safe, Some(packet)),
                Err(StoreError::PinnedContradiction { .. }) => {
                    (PacketSafety::PinnedContradiction, None)
                }
                Err(StoreError::PinnedBudgetExceeded { .. }) => {
                    (PacketSafety::PinnedBudgetExceeded, None)
                }
                Err(error) => return Err(error),
            }
        } else {
            (PacketSafety::Safe, None)
        };
        let delivery_to = page_to.or_else(|| context.as_ref().map(|_| head));
        let mut delivery = if let Some(page_to) = delivery_to
            && (has_more || context.is_some())
        {
            let delta = Self::task_delta_range_on(
                &transaction,
                session.task_id,
                session.confirmed_cursor,
                page_to,
            )?;
            let content_digest = crate::control::delivery_content_digest(context.as_ref(), &delta)?;
            let page = DeliveryPage {
                from_cursor: session.confirmed_cursor,
                to_cursor: page_to,
                head_cursor: head,
                has_more,
                content_digest,
                delivery_token: uuid::Uuid::now_v7().to_string(),
            };
            Some(ControlDelivery {
                page,
                context,
                delta,
            })
        } else {
            None
        };
        let delivery_too_large = delivery
            .as_ref()
            .map(CanonicalObject::freeze)
            .transpose()?
            .is_some_and(|object| object.bytes().len() > MAX_CONTROL_DELIVERY_BYTES);
        if delivery_too_large {
            packet_safety = PacketSafety::DeliveryBudgetExceeded;
            delivery = None;
        }
        let leases = Self::active_work_lease_bases(&transaction, session.task_id, session_id, now)?
            .into_iter()
            .filter(|lease| {
                intent
                    .resource_intents
                    .iter()
                    .any(|resource| lease.subject.covers(resource))
            })
            .collect();
        let work_binding_current = Self::control_work_binding_is_current(
            &transaction,
            project_id,
            session_id,
            session.work_binding.as_ref(),
            now,
        )?;
        let input = TurnEvaluationInput {
            control_schema_version: CONTROL_SCHEMA_VERSION,
            session_id: session_id.clone(),
            task_id: Some(session.task_id),
            work_binding: session.work_binding.clone(),
            work_binding_current,
            participant_membership: if membership == 1 {
                ParticipantMembership::Member
            } else {
                ParticipantMembership::NotMember
            },
            task_state,
            phase: session.phase,
            health: ControlHealth::Healthy,
            active_policy_known: true,
            host_assurance: session.assurance,
            required_assurance: policy.required_assurance,
            policy_effects: policy.supported_effects,
            mediated_effects: session.mediated_effects.clone(),
            current_epochs: ControlEpochs {
                project_policy: policy.epoch,
                task_admission: TaskAdmissionEpoch(task_admission_epoch),
            },
            session_epochs: session.epochs,
            confirmed_cursor: session.confirmed_cursor,
            head_cursor: head,
            pending_delivery: delivery.as_ref().map(|delivery| delivery.page.clone()),
            packet_safety,
            blocking_watermark: head,
            acknowledged_blocking_watermark: session.confirmed_cursor,
            has_unknown_action_outcome: false,
            authority_satisfied: true,
            capability_map_revision: session.capability_map_revision,
            leases,
            intent: intent.clone(),
            evaluated_at: now,
            grant_ttl_seconds: policy.grant_ttl_seconds,
        };
        let observed = crate::control::observe_turn(&input);
        let decision = match observed.decision {
            TurnDecision::Grant { basis } => {
                let grant = IssuedTurnGrant {
                    control_schema_version: CONTROL_SCHEMA_VERSION,
                    grant_id: uuid::Uuid::now_v7().to_string(),
                    request_key: intent.idempotency_key.clone(),
                    basis: *basis,
                    delivery,
                    issued_at: now,
                };
                let grant_object = CanonicalObject::freeze(&grant)?;
                transaction.execute(
                    "INSERT INTO control_turn_grants (
                         grant_id, session_id, task_id, request_key, grant_hash,
                         grant_json, state, issued_at_ms, expires_at_ms
                     ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'issued', ?7, ?8)",
                    params![
                        grant.grant_id,
                        session_id.0,
                        session.task_id.0.to_string(),
                        intent.idempotency_key,
                        grant_object.hash().as_str(),
                        grant_object.bytes(),
                        now.timestamp_millis(),
                        grant.basis.expires_at.timestamp_millis(),
                    ],
                )?;
                transaction.execute(
                    "UPDATE control_sessions SET
                         phase = 'turn_open', blocking_watermark = ?2,
                         revision = revision + 1, updated_at_ms = ?3
                     WHERE session_id = ?1",
                    params![session_id.0, head.0, now.timestamp_millis()],
                )?;
                ControlTurnDecision::Grant {
                    grant: Box::new(grant),
                }
            }
            TurnDecision::Refuse { directive } => {
                if directive.code == crate::domain::ControlRefusalCode::PolicyEpochChanged {
                    transaction.execute(
                        "UPDATE control_sessions SET
                             project_policy_epoch = ?2,
                             revision = revision + 1, updated_at_ms = ?3
                         WHERE session_id = ?1",
                        params![session_id.0, policy.epoch.0, now.timestamp_millis()],
                    )?;
                }
                ControlTurnDecision::Refuse { directive }
            }
            TurnDecision::Defer { deferral } => ControlTurnDecision::Defer { deferral },
        };
        let decision_object = CanonicalObject::freeze(&decision)?;
        transaction.execute(
            "INSERT INTO control_turn_results (
                 session_id, task_id, idempotency_key, intent_hash, intent_json,
                 decision_hash, decision_json, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                session_id.0,
                session.task_id.0.to_string(),
                intent.idempotency_key,
                intent_object.hash().as_str(),
                intent_object.bytes(),
                decision_object.hash().as_str(),
                decision_object.bytes(),
                now.timestamp_millis(),
            ],
        )?;
        transaction.commit()?;
        Ok(decision)
    }

    /// Atomically rechecks and begins one issued turn grant.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] for invalid routing, unknown grants,
    /// idempotency conflicts, corrupt projections, or persistence failures.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "begin rechecks and consumes the complete persisted grant basis atomically"
    )]
    pub fn begin_control_turn(
        &mut self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        connection_token: &str,
        routing_token: &str,
        grant_id: &str,
        delivery_tokens: &[String],
        idempotency_key: &str,
        now: DateTime<Utc>,
    ) -> Result<ControlTurnBeginDecision, StoreError> {
        let intent_object = CanonicalObject::freeze(&ControlTurnBeginFingerprint {
            control_schema_version: CONTROL_SCHEMA_VERSION,
            session_id,
            grant_id,
            delivery_tokens,
            idempotency_key,
        })?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::verify_control_connection(&transaction, session_id, connection_token)?;
        let session = Self::load_control_session_on(&transaction, session_id)?
            .ok_or_else(|| StoreError::ControlSessionNotBound(session_id.0.clone()))?;
        Self::verify_control_session(&session, project_id, routing_token)?;
        // An opener that could not resolve the project root's filesystem
        // identity must not begin, or replay the beginning of, a turn whose
        // basis names paths, even one issued earlier by a resolved opener.
        if self.host_path_policy.is_none()
            && Self::load_turn_grant(&transaction, session_id, grant_id)?.is_some_and(|grant| {
                grant
                    .grant
                    .basis
                    .resource_intents
                    .iter()
                    .any(|subject| matches!(subject, crate::domain::ResourceSubject::Path { .. }))
                    || !grant.grant.basis.leases.is_empty()
            })
        {
            return Err(StoreError::HostPathIdentityUnresolved);
        }
        if let Some(replay) = Self::replay_control_operation(
            &transaction,
            session_id,
            "turn_begin",
            idempotency_key,
            intent_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(replay);
        }
        let grant = Self::load_turn_grant(&transaction, session_id, grant_id)?
            .ok_or_else(|| StoreError::ControlTurnGrantNotFound(grant_id.into()))?;
        let policy = Self::load_active_control_policy(&transaction)?;
        let task_admission_epoch = transaction.query_row(
            "SELECT admission_epoch FROM task_control_state WHERE task_id = ?1",
            [session.task_id.0.to_string()],
            |row| row.get::<_, i64>(0),
        )?;
        let head = Self::latest_task_cursor(&transaction, session.task_id)?;
        let (task_state, membership) = transaction.query_row(
            "SELECT t.state,
                    EXISTS(
                        SELECT 1 FROM task_participants
                        WHERE task_id = t.task_id AND session_id = ?2
                    ) AND EXISTS(
                        SELECT 1 FROM session_bindings
                        WHERE task_id = t.task_id AND session_id = ?2
                    )
             FROM tasks t WHERE t.task_id = ?1 AND t.project_id = ?3",
            params![session.task_id.0.to_string(), session_id.0, project_id.0],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )?;
        let current_leases =
            Self::active_work_lease_bases(&transaction, session.task_id, session_id, now)?
                .into_iter()
                .filter(|lease| {
                    grant
                        .grant
                        .basis
                        .leases
                        .iter()
                        .any(|granted| granted.lease_id == lease.lease_id)
                })
                .collect();
        let context_current = if let Some(context) = grant
            .grant
            .delivery
            .as_ref()
            .and_then(|delivery| delivery.context.as_ref())
        {
            let (focused_work, _) =
                Self::focused_work_for_session_on(&transaction, project_id, session_id)?;
            let (project_context_revision, private_context_revision) =
                Self::context_revisions_on(&transaction, project_id, &session.actor.actor_id)?;
            if context.header.project_context_revision != project_context_revision
                || context.header.private_context_revision != private_context_revision
                || context.header.work_id != focused_work
            {
                false
            } else if let Some(work_id) = focused_work {
                work::context_work_feed_heads(&transaction, work_id)?
                    == context.header.work_feed_heads
            } else {
                context.header.work_feed_heads.is_empty()
            }
        } else {
            true
        };
        let work_binding_current = Self::control_work_binding_is_current(
            &transaction,
            project_id,
            session_id,
            session.work_binding.as_ref(),
            now,
        )?;
        let snapshot = TurnBeginSnapshot {
            control_schema_version: CONTROL_SCHEMA_VERSION,
            session_id: session_id.clone(),
            task_id: session.task_id,
            work_binding: session.work_binding.clone(),
            work_binding_current,
            phase: session.phase,
            participant_membership: if membership == 1 {
                ParticipantMembership::Member
            } else {
                ParticipantMembership::NotMember
            },
            task_state: Some(parse_enum(&task_state)?),
            grant_state: grant.state,
            current_epochs: ControlEpochs {
                project_policy: policy.epoch,
                task_admission: TaskAdmissionEpoch(task_admission_epoch),
            },
            current_head: head,
            context_current,
            capability_map_revision: session.capability_map_revision,
            delivery_tokens: delivery_tokens.to_vec(),
            leases: current_leases,
            observed_at: now,
        };
        let decision = match crate::control::evaluate_turn_begin(&grant.grant, &snapshot) {
            TurnBeginDecision::Begin => {
                let changed = transaction.execute(
                    "UPDATE control_turn_grants SET state = 'begun', begun_at_ms = ?2
                     WHERE grant_id = ?1 AND state = 'issued'",
                    params![grant_id, now.timestamp_millis()],
                )?;
                if changed != 1 {
                    return Err(StoreError::InvalidControlProjection(format!(
                        "turn grant {grant_id:?} was not issued during begin"
                    )));
                }
                let revision = transaction.query_row(
                    "UPDATE control_sessions SET
                         tentative_cursor = ?2, revision = revision + 1,
                         updated_at_ms = ?3
                     WHERE session_id = ?1
                     RETURNING revision",
                    params![
                        session_id.0,
                        grant.grant.basis.delivery_cursor.0,
                        now.timestamp_millis(),
                    ],
                    |row| row.get::<_, i64>(0),
                )?;
                ControlTurnBeginDecision::Begin {
                    receipt: TurnBeginReceipt {
                        grant_id: grant_id.into(),
                        session_id: session_id.clone(),
                        task_id: session.task_id,
                        phase: SessionPhase::TurnOpen,
                        tentative_cursor: grant.grant.basis.delivery_cursor,
                        session_revision: revision,
                        begun_at: now,
                    },
                }
            }
            TurnBeginDecision::Refuse { code } => {
                if matches!(
                    code,
                    crate::domain::ControlRefusalCode::GrantExpired
                        | crate::domain::ControlRefusalCode::PolicyEpochChanged
                        | crate::domain::ControlRefusalCode::TaskAdmissionEpochChanged
                        | crate::domain::ControlRefusalCode::DeltaRequired
                        | crate::domain::ControlRefusalCode::StaleFence
                ) && matches!(grant.state, TurnGrantState::Issued)
                {
                    transaction.execute(
                        "UPDATE control_turn_grants SET state = 'expired'
                         WHERE grant_id = ?1 AND state = 'issued'",
                        [grant_id],
                    )?;
                    let next_phase = if matches!(
                        code,
                        crate::domain::ControlRefusalCode::StaleFence
                            | crate::domain::ControlRefusalCode::PolicyEpochChanged
                    ) && session.confirmed_cursor == head
                    {
                        "ready"
                    } else {
                        "sync_required"
                    };
                    transaction.execute(
                        "UPDATE control_sessions SET
                             phase = ?2, tentative_cursor = NULL,
                             revision = revision + 1, updated_at_ms = ?3
                         WHERE session_id = ?1",
                        params![session_id.0, next_phase, now.timestamp_millis()],
                    )?;
                    if matches!(code, crate::domain::ControlRefusalCode::PolicyEpochChanged) {
                        transaction.execute(
                            "UPDATE control_sessions SET project_policy_epoch = ?2
                             WHERE session_id = ?1",
                            params![session_id.0, policy.epoch.0],
                        )?;
                    }
                }
                ControlTurnBeginDecision::Refuse { code }
            }
        };
        Self::persist_control_operation(
            &transaction,
            session_id,
            "turn_begin",
            idempotency_key,
            &intent_object,
            &decision,
            now,
        )?;
        transaction.commit()?;
        Ok(decision)
    }

    /// Checkpoints a begun turn, promotes its tentative delivery cursor, and
    /// emits one immutable task event.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] for invalid routing, unknown grants,
    /// idempotency conflicts, corrupt projections, or persistence failures.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "checkpoint closes the grant and emits its canonical transition atomically"
    )]
    pub fn checkpoint_control_turn(
        &mut self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        connection_token: &str,
        routing_token: &str,
        grant_id: &str,
        next_intent: TurnNextIntent,
        idempotency_key: &str,
        now: DateTime<Utc>,
    ) -> Result<ControlTurnCheckpointDecision, StoreError> {
        self.checkpoint_control_turn_with_evidence(
            project_id,
            session_id,
            connection_token,
            routing_token,
            grant_id,
            next_intent,
            &[],
            &[],
            &[],
            idempotency_key,
            now,
        )
    }

    /// Checkpoints a begun turn and atomically records asserted host execution
    /// observations against its frozen local-work binding.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when any observation is malformed, outside the
    /// grant effect envelope, or cannot be routed to the grant's exact run.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "checkpoint closes the grant and emits its canonical transition atomically"
    )]
    pub fn checkpoint_control_turn_with_observations(
        &mut self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        connection_token: &str,
        routing_token: &str,
        grant_id: &str,
        next_intent: TurnNextIntent,
        observations: &[ExecutionObservationInput],
        idempotency_key: &str,
        now: DateTime<Utc>,
    ) -> Result<ControlTurnCheckpointDecision, StoreError> {
        self.checkpoint_control_turn_with_evidence(
            project_id,
            session_id,
            connection_token,
            routing_token,
            grant_id,
            next_intent,
            observations,
            &[],
            &[],
            idempotency_key,
            now,
        )
    }

    /// Checkpoints a begun turn and atomically records host-captured execution,
    /// verification, and environment evidence against its frozen work basis.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when evidence is malformed, its producer cannot
    /// be resolved, or any fact falls outside the grant's exact run binding.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "checkpoint closes the grant and emits all host evidence atomically"
    )]
    pub fn checkpoint_control_turn_with_evidence(
        &mut self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        connection_token: &str,
        routing_token: &str,
        grant_id: &str,
        next_intent: TurnNextIntent,
        observations: &[ExecutionObservationInput],
        verification_evidence: &[VerificationEvidenceInput],
        environment_evidence: &[EnvironmentEvidenceInput],
        idempotency_key: &str,
        now: DateTime<Utc>,
    ) -> Result<ControlTurnCheckpointDecision, StoreError> {
        validate_execution_observation_inputs(observations)?;
        validate_typed_evidence_inputs(
            verification_evidence,
            environment_evidence,
            now,
            &DevelopmentNoopRedactor,
        )?;
        let intent_object = CanonicalObject::freeze(&ControlTurnCheckpointFingerprint {
            control_schema_version: CONTROL_SCHEMA_VERSION,
            session_id,
            grant_id,
            next_intent,
            observations,
            verification_evidence,
            environment_evidence,
            idempotency_key,
        })?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if !observations.is_empty()
            || !verification_evidence.is_empty()
            || !environment_evidence.is_empty()
        {
            work::require_work_schema_version(&transaction, self.work_schema_version)?;
        }
        Self::verify_control_connection(&transaction, session_id, connection_token)?;
        let session = Self::load_control_session_on(&transaction, session_id)?
            .ok_or_else(|| StoreError::ControlSessionNotBound(session_id.0.clone()))?;
        Self::verify_control_session(&session, project_id, routing_token)?;
        if let Some(replay) = Self::replay_control_operation(
            &transaction,
            session_id,
            "turn_checkpoint",
            idempotency_key,
            intent_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(replay);
        }
        let grant = Self::load_turn_grant(&transaction, session_id, grant_id)?
            .ok_or_else(|| StoreError::ControlTurnGrantNotFound(grant_id.into()))?;
        let snapshot = TurnCheckpointSnapshot {
            control_schema_version: CONTROL_SCHEMA_VERSION,
            session_id: session_id.clone(),
            task_id: session.task_id,
            work_binding: session.work_binding.clone(),
            phase: session.phase,
            grant_state: grant.state,
        };
        let decision = match crate::control::evaluate_turn_checkpoint(&grant.grant, &snapshot) {
            TurnCheckpointDecision::Checkpoint => {
                let records_work_evidence = !observations.is_empty()
                    || !verification_evidence.is_empty()
                    || !environment_evidence.is_empty();
                let binding = if records_work_evidence {
                    let binding = session.work_binding.clone().ok_or_else(|| {
                        StoreError::InvalidControlSession(
                            "host evidence requires an exact local-work binding".into(),
                        )
                    })?;
                    if grant.grant.basis.work_binding.as_ref() != Some(&binding) {
                        return Err(StoreError::WorkClaimMismatch {
                            work: binding.work_id,
                        });
                    }
                    Some(binding)
                } else {
                    None
                };
                let (obligation_rule_set, _) = Self::obligation_rule_set_for_policy_epoch_on(
                    &transaction,
                    grant.grant.basis.project_policy_epoch,
                )?;
                let mut observation_records = HashMap::new();
                let mut execution_observations = Vec::with_capacity(observations.len());
                for input in observations {
                    if !grant.grant.basis.requested_effects.contains(&input.effect) {
                        return Err(StoreError::ControlObservationScopeMismatch {
                            observation_id: input.observation_id.clone(),
                        });
                    }
                    let observation = ExecutionObservation {
                        schema_version: SCHEMA_VERSION,
                        project_id: project_id.clone(),
                        binding: binding.clone().ok_or_else(|| {
                            StoreError::InvalidControlSession(
                                "execution observation lost its work binding".into(),
                            )
                        })?,
                        session_id: session_id.clone(),
                        grant_id: grant_id.into(),
                        observation_id: input.observation_id.trim().into(),
                        action_fingerprint: input.action_fingerprint.clone(),
                        effect: input.effect,
                        outcome: input.outcome,
                        source_changed: input.source_changed,
                        obligation_rule_set: obligation_rule_set.clone(),
                        source_basis: input.source_basis.clone(),
                        observed_at: input.observed_at,
                        actor: session.actor.clone(),
                        recorded_at: now,
                    };
                    let hash =
                        work::append_control_execution_observation_on(&transaction, &observation)?;
                    observation_records.insert(
                        observation.observation_id.clone(),
                        (hash.clone(), observation),
                    );
                    execution_observations.push(hash);
                }
                let binding = binding.as_ref();
                let mut environment_hashes = Vec::with_capacity(environment_evidence.len());
                let mut environment_records = Vec::with_capacity(environment_evidence.len());
                for input in environment_evidence {
                    let binding = binding.ok_or_else(|| {
                        StoreError::InvalidControlSession(
                            "environment evidence lost its work binding".into(),
                        )
                    })?;
                    if input.components.as_ref().is_some_and(|components| {
                        components.capability_map_revision != session.capability_map_revision
                    }) {
                        return Err(StoreError::EnvironmentBasisMismatch(
                            input.environment_fingerprint.to_string(),
                        ));
                    }
                    let evidence = EnvironmentEvidence {
                        schema_version: SCHEMA_VERSION,
                        project_id: project_id.clone(),
                        binding: binding.clone(),
                        session_id: session_id.clone(),
                        source_basis: input.source_basis.clone(),
                        environment_fingerprint: input.environment_fingerprint.clone(),
                        components: input.components.clone(),
                        observed_at: input.observed_at,
                        actor: session.actor.clone(),
                        recorded_at: now,
                    };
                    let hash =
                        work::append_control_environment_evidence_on(&transaction, &evidence)?;
                    environment_hashes.push(hash.clone());
                    environment_records.push((hash, evidence));
                }
                let mut verification_hashes = Vec::with_capacity(verification_evidence.len());
                for input in verification_evidence {
                    let (producer_hash, producer) = match &input.producer_observation {
                        ExecutionObservationReference::ObjectHash { object_hash } => (
                            object_hash.clone(),
                            work::load_control_execution_observation_on(&transaction, object_hash)?
                                .ok_or_else(|| {
                                    StoreError::VerificationProducerObservationNotFound(
                                        object_hash.to_string(),
                                    )
                                })?,
                        ),
                        ExecutionObservationReference::ObservationId { observation_id } => {
                            observation_records
                                .get(observation_id)
                                .cloned()
                                .ok_or_else(|| {
                                    StoreError::VerificationProducerObservationNotFound(
                                        observation_id.clone(),
                                    )
                                })?
                        }
                    };
                    let binding = binding.ok_or_else(|| {
                        StoreError::InvalidControlSession(
                            "verification evidence lost its work binding".into(),
                        )
                    })?;
                    validate_verification_producer(
                        &producer, project_id, session_id, binding, now,
                    )?;
                    let completed_at = producer.observed_at.ok_or_else(|| {
                        StoreError::InvalidControlSession(
                            "verification producer has no observed_at timestamp".into(),
                        )
                    })?;
                    let source_basis = producer.source_basis.clone().ok_or_else(|| {
                        StoreError::InvalidControlSession(
                            "verification producer has no source content basis".into(),
                        )
                    })?;
                    let environment = resolve_verification_environment_on(
                        &transaction,
                        input.environment.as_ref(),
                        &environment_records,
                        project_id,
                        binding,
                        &source_basis,
                    )?;
                    let evidence = VerificationEvidence {
                        schema_version: SCHEMA_VERSION,
                        project_id: project_id.clone(),
                        binding: binding.clone(),
                        session_id: session_id.clone(),
                        producer_observation: producer_hash,
                        source_basis,
                        environment,
                        check_kind: input.check_kind,
                        check_fingerprint: producer.action_fingerprint.clone(),
                        result: verification_result(producer.outcome),
                        completed_at,
                        summary: normalize_verification_summary(input),
                        refs: normalize_typed_evidence_refs(&input.refs),
                        actor: session.actor.clone(),
                        recorded_at: now,
                    };
                    verification_hashes.push(work::append_control_verification_evidence_on(
                        &transaction,
                        &evidence,
                    )?);
                }
                if matches!(next_intent, TurnNextIntent::Exit) {
                    Self::terminalize_session_work_leases(&transaction, &session, now, false)?;
                }
                let event = TurnCheckpointEvent {
                    schema_version: SCHEMA_VERSION,
                    task_id: session.task_id,
                    session_id: session_id.clone(),
                    grant_id: grant_id.into(),
                    delivered_cursor: grant.grant.basis.delivery_cursor,
                    next_intent,
                    execution_observations: execution_observations.clone(),
                    verification_evidence: verification_hashes.clone(),
                    environment_evidence: environment_hashes.clone(),
                    actor: session.actor.clone(),
                    created_at: now,
                };
                let event_object = CanonicalObject::freeze(&event)?;
                Self::insert_object(&transaction, "turn_checkpoint_event", &event_object)?;
                let head_before_checkpoint =
                    Self::latest_task_cursor(&transaction, session.task_id)?;
                let cursor = Self::insert_task_change(
                    &transaction,
                    session.task_id,
                    "turn_checkpoint_event",
                    &event_object,
                )?;
                let confirmed_cursor =
                    if head_before_checkpoint == grant.grant.basis.delivery_cursor {
                        cursor
                    } else {
                        grant.grant.basis.delivery_cursor
                    };
                let phase = if matches!(next_intent, TurnNextIntent::Exit) {
                    SessionPhase::Exited
                } else if confirmed_cursor < cursor {
                    SessionPhase::SyncRequired
                } else {
                    SessionPhase::Ready
                };
                let changed = transaction.execute(
                    "UPDATE control_turn_grants SET
                         state = 'completed', completed_at_ms = ?2
                      WHERE grant_id = ?1 AND state = 'begun'",
                    params![grant_id, now.timestamp_millis()],
                )?;
                if changed != 1 {
                    return Err(StoreError::InvalidControlProjection(format!(
                        "turn grant {grant_id:?} was not begun during checkpoint"
                    )));
                }
                let revision = transaction.query_row(
                    "UPDATE control_sessions SET
                         phase = ?2, confirmed_cursor = ?3,
                         tentative_cursor = NULL, blocking_watermark = ?4,
                         revision = revision + 1, updated_at_ms = ?5
                     WHERE session_id = ?1
                     RETURNING revision",
                    params![
                        session_id.0,
                        enum_name(phase)?,
                        confirmed_cursor.0,
                        cursor.0,
                        now.timestamp_millis(),
                    ],
                    |row| row.get::<_, i64>(0),
                )?;
                ControlTurnCheckpointDecision::Checkpointed {
                    receipt: TurnCheckpointReceipt {
                        grant_id: grant_id.into(),
                        checkpoint: event_object.hash().clone(),
                        execution_observations,
                        verification_evidence: verification_hashes,
                        environment_evidence: environment_hashes,
                        cursor,
                        confirmed_cursor,
                        phase,
                        session_revision: revision,
                        checkpointed_at: now,
                    },
                }
            }
            TurnCheckpointDecision::Refuse { code } => {
                ControlTurnCheckpointDecision::Refuse { code }
            }
        };
        Self::persist_control_operation(
            &transaction,
            session_id,
            "turn_checkpoint",
            idempotency_key,
            &intent_object,
            &decision,
            now,
        )?;
        transaction.commit()?;
        Ok(decision)
    }

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
    pub fn record_memory_contradiction(
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
    ) -> Result<MemoryContradictionReceipt, StoreError> {
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
            project_id: Some(project_id.clone()),
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
        let transaction = self.connection.unchecked_transaction()?;
        let (focused_work_id, _) =
            Self::focused_work_for_session_on(&transaction, project_id, session_id)?;
        if focused_work_id != Some(work_id) {
            return Err(StoreError::InvalidWork(
                "work-memory query must match the session's persisted focus".into(),
            ));
        }
        let (work_project, work_root_id) = work::verified_work_identity(&transaction, work_id)?;
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
        transaction.commit()?;
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
            )?;
            activated += 1;
        }
        Self::rebuild_contradiction_projection(&transaction, contradictions)?;
        Self::bump_rebuilt_context_revisions_on(&transaction)?;
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
            let project_id = if let Some(project_id) = edge.project_id.clone() {
                project_id
            } else if let Some(task_id) = edge.task_id {
                let project_id = transaction
                    .query_row(
                        "SELECT project_id FROM tasks WHERE task_id = ?1",
                        [task_id.0.to_string()],
                        |row| row.get::<_, String>(0),
                    )
                    .optional()?
                    .ok_or_else(|| {
                        StoreError::InvalidMemoryProjection(
                            "legacy contradiction references an unknown task".into(),
                        )
                    })?;
                crate::domain::ProjectId(project_id)
            } else {
                return Err(StoreError::InvalidMemoryProjection(
                    "contradiction has no project or task anchor".into(),
                ));
            };
            transaction.execute(
                "INSERT INTO memory_contradiction_edges (
                     contradiction_hash, project_id, task_id, work_root_id,
                     left_version_hash, right_version_hash
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
                params![
                    contradiction_hash.as_str(),
                    project_id.0,
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

    fn context_revisions_on(
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

    fn bump_project_context_revision_on(
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

    fn build_context_on(
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

    fn focused_work_for_session_on(
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
                    let project_bound = if let Some(event_project) = event.project_id.as_ref() {
                        event_project == project_id
                    } else if let Some(event_task) = event.task_id {
                        connection
                            .query_row(
                                "SELECT project_id FROM tasks WHERE task_id = ?1",
                                [event_task.0.to_string()],
                                |row| row.get::<_, String>(0),
                            )
                            .optional()?
                            .as_deref()
                            == Some(project_id.0.as_str())
                    } else {
                        false
                    };
                    if !project_bound
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

    fn task_delta_range_on(
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
        let expected = usize::try_from(through.0 - after.0).map_err(|_| {
            StoreError::InvalidTaskProjection("task delivery range overflowed".into())
        })?;
        let dense = changes.iter().enumerate().all(|(offset, change)| {
            i64::try_from(offset).is_ok_and(|offset| change.cursor.0 == after.0 + offset + 1)
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

    fn task_delivery_page_end(
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

    /// Activates a new immutable project control-policy version.
    ///
    /// Reapplying the active assurance is an idempotent no-op. Callers may
    /// provide the policy hash they observed to prevent a concurrent operator
    /// update from being overwritten.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when attribution/redaction is invalid, the
    /// expected policy is stale, canonical history is corrupt, or persistence
    /// cannot complete atomically.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "the caller key, attribution, CAS guard, clock, and redactor are independent parts of one auditable policy transaction"
    )]
    pub fn set_required_control_assurance<R: Redactor>(
        &mut self,
        required_assurance: ControlAssurance,
        authorized_by: &ActorContext,
        reason: &str,
        idempotency_key: &str,
        expected_policy: Option<&ObjectHash>,
        now: DateTime<Utc>,
        redactor: &R,
    ) -> Result<ControlPolicyUpdateReceipt, StoreError> {
        if authorized_by.assurance != AssuranceLevel::Asserted {
            return Err(StoreError::InvalidControlProjection(
                "V1 control-policy administration records asserted host context only".into(),
            ));
        }
        let authorized_by = normalize_control_policy_actor(authorized_by, redactor)?;
        let reason = normalize_control_text(reason, "control policy update reason")?;
        redactor
            .inspect(&reason)
            .map_err(StoreError::RedactionRefused)?;
        let idempotency_key = normalize_control_policy_idempotency_key(idempotency_key)?;
        let intent =
            CanonicalObject::freeze(&ControlPolicyOperationFingerprint::SetRequiredAssurance {
                fingerprint_schema_version: CONTROL_POLICY_OPERATION_FINGERPRINT_SCHEMA_VERSION,
                idempotency_key,
                required_assurance,
                authorized_by: &authorized_by,
                reason: &reason,
                expected_policy,
            })?;
        if intent.bytes().len() > MAX_CONTROL_POLICY_OPERATION_INTENT_BYTES {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy operation intent exceeds the {MAX_CONTROL_POLICY_OPERATION_INTENT_BYTES}-byte canonical limit"
            )));
        }

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(receipt) = Self::replay_control_policy_operation::<ControlPolicyUpdateReceipt>(
            &transaction,
            "set_required_assurance",
            idempotency_key,
            &intent,
        )? {
            transaction.commit()?;
            return Ok(receipt);
        }
        let current = Self::verify_control_policy_history(&transaction)?;
        if let Some(expected) = expected_policy
            && expected != &current.policy_hash
        {
            return Err(StoreError::ControlPolicyConflict {
                expected: expected.clone(),
                current: current.policy_hash,
            });
        }
        if required_assurance == current.required_assurance {
            let (policy, _) =
                Self::load_control_policy_version(&transaction, &current.policy_hash)?;
            let receipt = ControlPolicyUpdateReceipt {
                changed: false,
                active_policy: current.policy_hash,
                previous_policy: policy.previous_policy,
                authority: current.authority_hash,
                policy_epoch: current.epoch,
                previous_required_assurance: current.required_assurance,
                required_assurance: current.required_assurance,
                activated_at: current.activated_at,
            };
            Self::persist_control_policy_operation(
                &transaction,
                "set_required_assurance",
                idempotency_key,
                &intent,
                &receipt,
                now,
            )?;
            transaction.commit()?;
            return Ok(receipt);
        }

        let next_epoch = current.epoch.0.checked_add(1).ok_or_else(|| {
            StoreError::InvalidControlProjection("control policy epoch overflowed".into())
        })?;
        let authority = ProjectPolicyAuthorityDecision {
            schema_version: CONTROL_POLICY_AUTHORITY_SCHEMA_VERSION_V1,
            operation: ProjectPolicyOperation::SetRequiredAssurance,
            policy_epoch: ProjectPolicyEpoch(next_epoch),
            previous_policy: Some(current.policy_hash.clone()),
            required_assurance,
            obligation_rule_set: current.obligation_rule_set.clone(),
            authorized_by,
            reason,
            decided_at: now,
        };
        let authority_object = CanonicalObject::freeze(&authority)?;
        if authority_object.bytes().len() > MAX_CONTROL_POLICY_AUTHORITY_BYTES {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy authority exceeds the {MAX_CONTROL_POLICY_AUTHORITY_BYTES}-byte canonical limit"
            )));
        }
        Self::insert_object(
            &transaction,
            "project_policy_authority_decision",
            &authority_object,
        )?;
        let policy = ControlPolicy {
            schema_version: CONTROL_POLICY_SCHEMA_VERSION_V1,
            control_schema_version: CONTROL_POLICY_CONTROL_SCHEMA_VERSION_V1,
            policy_epoch: ProjectPolicyEpoch(next_epoch),
            previous_policy: Some(current.policy_hash.clone()),
            required_assurance,
            supported_effects: current.supported_effects,
            grant_ttl_seconds: current.grant_ttl_seconds,
            obligation_rule_set: current.obligation_rule_set,
            authority: authority_object.hash().clone(),
            activated_at: now,
        };
        Self::validate_active_control_policy(&policy)?;
        let policy_object = CanonicalObject::freeze(&policy)?;
        Self::insert_object(&transaction, "control_policy", &policy_object)?;
        transaction.execute(
            "INSERT INTO control_policy_versions (
                 policy_hash, policy_epoch, authority_hash, policy_json
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                policy_object.hash().as_str(),
                policy.policy_epoch.0,
                authority_object.hash().as_str(),
                policy_object.bytes(),
            ],
        )?;
        let changed = transaction.execute(
            "UPDATE control_policy_state SET
                 schema_version = ?1, policy_epoch = ?2,
                 required_assurance = ?3, supported_effects_json = ?4,
                 grant_ttl_seconds = ?5, policy_hash = ?6
             WHERE singleton = 1 AND policy_epoch = ?7 AND policy_hash = ?8",
            params![
                CONTROL_POLICY_STATE_SCHEMA_VERSION,
                policy.policy_epoch.0,
                enum_name(policy.required_assurance)?,
                serde_json::to_string(&policy.supported_effects)?,
                policy.grant_ttl_seconds,
                policy_object.hash().as_str(),
                current.epoch.0,
                current.policy_hash.as_str(),
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidControlProjection(
                "active control policy compare-and-swap matched no row".into(),
            ));
        }
        let activated = Self::verify_control_policy_history(&transaction)?;
        if activated.policy_hash != *policy_object.hash() || activated.epoch != policy.policy_epoch
        {
            return Err(StoreError::InvalidControlProjection(
                "activated control policy failed post-CAS integrity validation".into(),
            ));
        }
        let receipt = ControlPolicyUpdateReceipt {
            changed: true,
            active_policy: policy_object.hash().clone(),
            previous_policy: policy.previous_policy,
            authority: authority_object.hash().clone(),
            policy_epoch: policy.policy_epoch,
            previous_required_assurance: current.required_assurance,
            required_assurance: policy.required_assurance,
            activated_at: policy.activated_at,
        };
        Self::persist_control_policy_operation(
            &transaction,
            "set_required_assurance",
            idempotency_key,
            &intent,
            &receipt,
            now,
        )?;
        transaction.commit()?;
        Ok(receipt)
    }

    /// Selects a canonical obligation rule set through a new immutable project
    /// policy version. This host/operator-only entry point is intentionally not
    /// exposed through the agent MCP or host turn protocol.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the rule set or asserted attribution is
    /// invalid, the expected policy is stale, history is corrupt, or the CAS
    /// activation cannot complete atomically.
    #[allow(
        clippy::too_many_arguments,
        clippy::too_many_lines,
        reason = "the caller key, rule set, attribution, CAS guard, clock, and redactor are independent parts of one auditable policy transaction"
    )]
    pub fn set_obligation_rule_set<R: Redactor>(
        &mut self,
        rule_set: &ObligationRuleSet,
        authorized_by: &ActorContext,
        reason: &str,
        idempotency_key: &str,
        expected_policy: Option<&ObjectHash>,
        now: DateTime<Utc>,
        redactor: &R,
    ) -> Result<ObligationRuleSetUpdateReceipt, StoreError> {
        Self::validate_obligation_rule_set(rule_set)?;
        if authorized_by.assurance != AssuranceLevel::Asserted {
            return Err(StoreError::InvalidControlProjection(
                "V1 control-policy administration records asserted host context only".into(),
            ));
        }
        let authorized_by = normalize_control_policy_actor(authorized_by, redactor)?;
        let reason = normalize_control_text(reason, "obligation rule-set update reason")?;
        redactor
            .inspect(&reason)
            .map_err(StoreError::RedactionRefused)?;
        let rule_set_object = CanonicalObject::freeze(rule_set)?;
        let idempotency_key = normalize_control_policy_idempotency_key(idempotency_key)?;
        let intent =
            CanonicalObject::freeze(&ControlPolicyOperationFingerprint::SetObligationRuleSet {
                fingerprint_schema_version: CONTROL_POLICY_OPERATION_FINGERPRINT_SCHEMA_VERSION,
                idempotency_key,
                obligation_rule_set: rule_set_object.hash(),
                authorized_by: &authorized_by,
                reason: &reason,
                expected_policy,
            })?;
        if intent.bytes().len() > MAX_CONTROL_POLICY_OPERATION_INTENT_BYTES {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy operation intent exceeds the {MAX_CONTROL_POLICY_OPERATION_INTENT_BYTES}-byte canonical limit"
            )));
        }

        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        if let Some(receipt) =
            Self::replay_control_policy_operation::<ObligationRuleSetUpdateReceipt>(
                &transaction,
                "set_obligation_rule_set",
                idempotency_key,
                &intent,
            )?
        {
            transaction.commit()?;
            return Ok(receipt);
        }
        let current = Self::verify_control_policy_history(&transaction)?;
        if let Some(expected) = expected_policy
            && expected != &current.policy_hash
        {
            return Err(StoreError::ControlPolicyConflict {
                expected: expected.clone(),
                current: current.policy_hash,
            });
        }
        let current_rule_set = current.obligation_rule_set.clone().ok_or_else(|| {
            StoreError::InvalidControlProjection(
                "active control policy has no obligation rule-set selection".into(),
            )
        })?;
        if current_rule_set == *rule_set_object.hash() {
            let (policy, _) =
                Self::load_control_policy_version(&transaction, &current.policy_hash)?;
            let receipt = ObligationRuleSetUpdateReceipt {
                changed: false,
                active_policy: current.policy_hash,
                previous_policy: policy.previous_policy,
                authority: current.authority_hash,
                policy_epoch: current.epoch,
                previous_rule_set: Some(current_rule_set.clone()),
                obligation_rule_set: current_rule_set,
                activated_at: current.activated_at,
            };
            Self::persist_control_policy_operation(
                &transaction,
                "set_obligation_rule_set",
                idempotency_key,
                &intent,
                &receipt,
                now,
            )?;
            transaction.commit()?;
            return Ok(receipt);
        }

        Self::insert_object(&transaction, "obligation_rule_set", &rule_set_object)?;
        let next_epoch = current.epoch.0.checked_add(1).ok_or_else(|| {
            StoreError::InvalidControlProjection("control policy epoch overflowed".into())
        })?;
        let authority = ProjectPolicyAuthorityDecision {
            schema_version: CONTROL_POLICY_AUTHORITY_SCHEMA_VERSION_V1,
            operation: ProjectPolicyOperation::SetObligationRuleSet,
            policy_epoch: ProjectPolicyEpoch(next_epoch),
            previous_policy: Some(current.policy_hash.clone()),
            required_assurance: current.required_assurance,
            obligation_rule_set: Some(rule_set_object.hash().clone()),
            authorized_by,
            reason,
            decided_at: now,
        };
        let authority_object = CanonicalObject::freeze(&authority)?;
        if authority_object.bytes().len() > MAX_CONTROL_POLICY_AUTHORITY_BYTES {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy authority exceeds the {MAX_CONTROL_POLICY_AUTHORITY_BYTES}-byte canonical limit"
            )));
        }
        Self::insert_object(
            &transaction,
            "project_policy_authority_decision",
            &authority_object,
        )?;
        let policy = ControlPolicy {
            schema_version: CONTROL_POLICY_SCHEMA_VERSION_V1,
            control_schema_version: CONTROL_POLICY_CONTROL_SCHEMA_VERSION_V1,
            policy_epoch: ProjectPolicyEpoch(next_epoch),
            previous_policy: Some(current.policy_hash.clone()),
            required_assurance: current.required_assurance,
            supported_effects: current.supported_effects,
            grant_ttl_seconds: current.grant_ttl_seconds,
            obligation_rule_set: Some(rule_set_object.hash().clone()),
            authority: authority_object.hash().clone(),
            activated_at: now,
        };
        Self::validate_active_control_policy(&policy)?;
        let policy_object = CanonicalObject::freeze(&policy)?;
        Self::insert_object(&transaction, "control_policy", &policy_object)?;
        transaction.execute(
            "INSERT INTO control_policy_versions (
                 policy_hash, policy_epoch, authority_hash, policy_json
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                policy_object.hash().as_str(),
                policy.policy_epoch.0,
                authority_object.hash().as_str(),
                policy_object.bytes(),
            ],
        )?;
        let changed = transaction.execute(
            "UPDATE control_policy_state SET
                 schema_version = ?1, policy_epoch = ?2,
                 required_assurance = ?3, supported_effects_json = ?4,
                 grant_ttl_seconds = ?5, policy_hash = ?6
             WHERE singleton = 1 AND policy_epoch = ?7 AND policy_hash = ?8",
            params![
                CONTROL_POLICY_STATE_SCHEMA_VERSION,
                policy.policy_epoch.0,
                enum_name(policy.required_assurance)?,
                serde_json::to_string(&policy.supported_effects)?,
                policy.grant_ttl_seconds,
                policy_object.hash().as_str(),
                current.epoch.0,
                current.policy_hash.as_str(),
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidControlProjection(
                "obligation rule-set policy compare-and-swap matched no row".into(),
            ));
        }
        let activated = Self::load_active_control_policy(&transaction)?;
        if activated.policy_hash != *policy_object.hash() || activated.epoch != policy.policy_epoch
        {
            return Err(StoreError::InvalidControlProjection(
                "activated obligation rule-set policy failed post-CAS integrity validation".into(),
            ));
        }
        Self::verify_control_policy_history(&transaction)?;
        let receipt = ObligationRuleSetUpdateReceipt {
            changed: true,
            active_policy: policy_object.hash().clone(),
            previous_policy: policy.previous_policy,
            authority: authority_object.hash().clone(),
            policy_epoch: policy.policy_epoch,
            previous_rule_set: Some(current_rule_set),
            obligation_rule_set: rule_set_object.hash().clone(),
            activated_at: policy.activated_at,
        };
        Self::persist_control_policy_operation(
            &transaction,
            "set_obligation_rule_set",
            idempotency_key,
            &intent,
            &receipt,
            now,
        )?;
        transaction.commit()?;
        Ok(receipt)
    }

    /// Summarizes the built-in control policy and live operational envelope.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when policy or control projections are invalid.
    pub fn control_diagnostics(&self) -> Result<ControlDiagnostics, StoreError> {
        let policy = if self.connection.is_autocommit() {
            let snapshot = self.connection.unchecked_transaction()?;
            let policy = Self::verify_control_policy_history(&snapshot)?;
            snapshot.commit()?;
            policy
        } else {
            Self::verify_control_policy_history(&self.connection)?
        };
        if policy.state_schema_version != CONTROL_POLICY_STATE_SCHEMA_VERSION {
            return Err(StoreError::InvalidControlProjection(
                "active control policy uses a migratable legacy state schema".into(),
            ));
        }
        let obligation_rule_set = policy.obligation_rule_set.clone().ok_or_else(|| {
            StoreError::InvalidControlProjection(
                "active control policy has no obligation rule-set selection".into(),
            )
        })?;
        Self::load_obligation_rule_set_on(&self.connection, &obligation_rule_set)?;
        let active_sessions = self.connection.query_row(
            "SELECT COUNT(*) FROM control_sessions WHERE phase != 'exited'",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        let issued_turns = self.connection.query_row(
            "SELECT COUNT(*) FROM control_turn_grants
             WHERE state = 'issued'
               AND expires_at_ms > CAST(strftime('%s', 'now') AS INTEGER) * 1000",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        let begun_turns = self.connection.query_row(
            "SELECT COUNT(*) FROM control_turn_grants WHERE state = 'begun'",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        let all_effects = [
            EffectClass::Observe,
            EffectClass::Communicate,
            EffectClass::Coordinate,
            EffectClass::MutateLocal,
            EffectClass::MutateShared,
            EffectClass::ExternalSideEffect,
            EffectClass::Lifecycle,
        ];
        let unenforced_effects = all_effects
            .into_iter()
            .filter(|effect| !policy.supported_effects.contains(effect))
            .collect();
        Ok(ControlDiagnostics {
            control_schema_version: CONTROL_SCHEMA_VERSION,
            active_policy: policy.policy_hash,
            policy_epoch: policy.epoch,
            required_assurance: policy.required_assurance,
            obligation_rule_set,
            supported_effects: policy.supported_effects,
            unenforced_effects,
            active_sessions: Self::control_count(active_sessions, "active session")?,
            issued_turns: Self::control_count(issued_turns, "issued turn")?,
            begun_turns: Self::control_count(begun_turns, "begun turn")?,
            // These are explicit alpha capability disclosures, not probes.
            action_gating_available: false,
            authority_mediation_available: false,
            action_outcome_tracking_available: false,
        })
    }

    /// Verifies canonical bytes and hashes for every stored object and control
    /// observation.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when SQLite cannot scan the object table.
    #[allow(
        clippy::too_many_lines,
        reason = "the integrity scanner enumerates every canonical and operational control tier"
    )]
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

        if self.connection.is_autocommit() {
            let policy_snapshot = self.connection.unchecked_transaction()?;
            Self::verify_control_policy_records_on(&policy_snapshot, &mut report)?;
            policy_snapshot.commit()?;
        } else {
            // Corruption fixtures and embedders may already own a transaction
            // or savepoint. Reuse that snapshot instead of nesting BEGIN.
            Self::verify_control_policy_records_on(&self.connection, &mut report)?;
        }

        let mut control_statement = self.connection.prepare(
            "SELECT sequence, session_id, task_id, idempotency_key, intent_hash,
                    observed_at_ms, input_hash, input_json, decision_hash, decision_json
             FROM control_observations ORDER BY sequence",
        )?;
        let control_rows = control_statement.query_map([], |row| {
            Ok(StoredControlObservation {
                sequence: row.get(0)?,
                session_id: row.get(1)?,
                task_id: row.get(2)?,
                idempotency_key: row.get(3)?,
                intent_hash: row.get(4)?,
                observed_at_ms: row.get(5)?,
                input_hash: row.get(6)?,
                input_json: row.get(7)?,
                decision_hash: row.get(8)?,
                decision_json: row.get(9)?,
            })
        })?;
        for row in control_rows {
            let stored = row?;
            report.checked_control_records += 1;
            if Self::decode_control_observation(&stored).is_err() {
                report
                    .invalid_control_records
                    .push(format!("control_observation:{}", stored.sequence));
            }
        }

        let mut session_statement = self
            .connection
            .prepare("SELECT session_id FROM control_sessions ORDER BY session_id")?;
        let session_rows = session_statement.query_map([], |row| row.get::<_, String>(0))?;
        for row in session_rows {
            let session_id = row?;
            report.checked_control_records += 1;
            if Self::load_control_session_on(&self.connection, &SessionId(session_id.clone()))
                .is_err()
            {
                report
                    .invalid_control_records
                    .push(format!("control_session:{session_id}"));
            }
        }

        let mut turn_statement = self.connection.prepare(
            "SELECT sequence, session_id, task_id, idempotency_key,
                    intent_hash, intent_json, decision_hash, decision_json
             FROM control_turn_results ORDER BY sequence",
        )?;
        let turn_rows = turn_statement.query_map([], |row| {
            Ok(StoredControlTurnResult {
                sequence: row.get(0)?,
                session_id: row.get(1)?,
                task_id: row.get(2)?,
                idempotency_key: row.get(3)?,
                intent_hash: row.get(4)?,
                intent_json: row.get(5)?,
                decision_hash: row.get(6)?,
                decision_json: row.get(7)?,
            })
        })?;
        for row in turn_rows {
            let stored = row?;
            report.checked_control_records += 1;
            if Self::verify_control_turn_result(&stored).is_err() {
                report
                    .invalid_control_records
                    .push(format!("control_turn_result:{}", stored.sequence));
            }
        }

        let mut grant_statement = self.connection.prepare(
            "SELECT grant_id, session_id, task_id, request_key, grant_hash,
                    grant_json, state, issued_at_ms, expires_at_ms
             FROM control_turn_grants ORDER BY issued_at_ms, grant_id",
        )?;
        let grant_rows = grant_statement.query_map([], |row| {
            Ok(StoredControlGrantRow {
                grant_id: row.get(0)?,
                session_id: row.get(1)?,
                task_id: row.get(2)?,
                request_key: row.get(3)?,
                grant_hash: row.get(4)?,
                grant_json: row.get(5)?,
                state: row.get(6)?,
                issued_at_ms: row.get(7)?,
                expires_at_ms: row.get(8)?,
            })
        })?;
        for row in grant_rows {
            let stored = row?;
            report.checked_control_records += 1;
            if Self::verify_control_grant_row(&stored).is_err() {
                report
                    .invalid_control_records
                    .push(format!("control_turn_grant:{}", stored.grant_id));
            }
        }

        let mut lease_statement = self.connection.prepare(
            "SELECT lease_id, task_id, holder_session_id, lease_hash,
                    lease_json, state, expires_at_ms
             FROM control_work_leases ORDER BY lease_id",
        )?;
        let lease_rows = lease_statement.query_map([], |row| {
            Ok(StoredWorkLeaseRow {
                lease_id: row.get(0)?,
                task_id: row.get(1)?,
                holder_session_id: row.get(2)?,
                lease_hash: row.get(3)?,
                lease_json: row.get(4)?,
                state: row.get(5)?,
                expires_at_ms: row.get(6)?,
            })
        })?;
        for row in lease_rows {
            let stored = row?;
            report.checked_control_records += 1;
            if Self::decode_work_lease_row(&stored).is_err() {
                report
                    .invalid_control_records
                    .push(format!("control_work_lease:{}", stored.lease_id));
            }
        }

        let mut operation_statement = self.connection.prepare(
            "SELECT sequence, session_id, operation, idempotency_key,
                    intent_hash, intent_json, result_hash, result_json
             FROM control_operation_results ORDER BY sequence",
        )?;
        let operation_rows = operation_statement.query_map([], |row| {
            Ok(StoredControlOperation {
                sequence: row.get(0)?,
                session_id: row.get(1)?,
                operation: row.get(2)?,
                idempotency_key: row.get(3)?,
                intent_hash: row.get(4)?,
                intent_json: row.get(5)?,
                result_hash: row.get(6)?,
                result_json: row.get(7)?,
            })
        })?;
        for row in operation_rows {
            let stored = row?;
            report.checked_control_records += 1;
            if Self::verify_control_operation(&stored).is_err() {
                report
                    .invalid_control_records
                    .push(format!("control_operation:{}", stored.sequence));
            }
        }
        let mut policy_operation_statement = self.connection.prepare(
            "SELECT sequence, operation, idempotency_key, intent_hash, intent_json,
                    result_hash, result_json
             FROM control_policy_operation_results ORDER BY sequence",
        )?;
        let policy_operation_rows = policy_operation_statement.query_map([], |row| {
            Ok(StoredControlPolicyOperation {
                sequence: row.get(0)?,
                operation: row.get(1)?,
                idempotency_key: row.get(2)?,
                intent_hash: row.get(3)?,
                intent_json: row.get(4)?,
                result_hash: row.get(5)?,
                result_json: row.get(6)?,
            })
        })?;
        for row in policy_operation_rows {
            let stored = row?;
            report.checked_control_records += 1;
            if Self::verify_control_policy_operation(&stored).is_err() {
                report
                    .invalid_control_records
                    .push(format!("control_policy_operation:{}", stored.sequence));
            }
        }
        let (checked_work_records, invalid_work_records, legacy_work_records) =
            self.verify_work_projections()?;
        report.checked_work_records = checked_work_records;
        report.invalid_work_records = invalid_work_records;
        report.legacy_work_records = legacy_work_records;
        Ok(report)
    }

    fn verify_control_policy_records_on(
        connection: &Connection,
        report: &mut IntegrityReport,
    ) -> Result<(), StoreError> {
        report.checked_control_records += 1;
        match Self::verify_control_policy_history(connection) {
            Ok(policy) => {
                let active_rules_are_valid = policy.state_schema_version
                    == CONTROL_POLICY_STATE_SCHEMA_VERSION
                    && policy.obligation_rule_set.as_ref().is_some_and(|hash| {
                        Self::load_obligation_rule_set_on(connection, hash).is_ok()
                    });
                if !active_rules_are_valid {
                    report
                        .invalid_control_records
                        .push("control_policy_state:active".into());
                }
            }
            Err(error @ StoreError::Sqlite(_)) => return Err(error),
            Err(_) => report
                .invalid_control_records
                .push("control_policy_state:active".into()),
        }
        let active_policy_epoch = connection
            .query_row(
                "SELECT policy_epoch FROM control_policy_state WHERE singleton = 1",
                [],
                |row| row.get::<_, i64>(0),
            )
            .optional()?;
        let policy_rows = {
            let mut statement = connection.prepare(
                "SELECT policy_hash, policy_epoch
                 FROM control_policy_versions ORDER BY policy_epoch",
            )?;
            statement
                .query_map([], |row| {
                    Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        for (stored_hash, projected_epoch) in policy_rows {
            report.checked_control_records += 1;
            let valid = ObjectHash::from_stored(stored_hash.clone())
                .ok_or_else(|| StoreError::InvalidStoredHash(stored_hash.clone()))
                .and_then(|hash| Self::load_control_policy_version(connection, &hash))
                .and_then(|(policy, authority)| {
                    if policy.schema_version == CONTROL_POLICY_SCHEMA_VERSION_V1
                        && let Some(rule_set) = policy.obligation_rule_set.as_ref()
                    {
                        Self::load_obligation_rule_set_on(connection, rule_set)?;
                    }
                    Ok((policy, authority))
                });
            let is_orphaned_successor =
                active_policy_epoch.is_none_or(|active_epoch| projected_epoch > active_epoch);
            let is_invalid = match valid {
                Ok(_) => false,
                Err(error @ StoreError::Sqlite(_)) => return Err(error),
                Err(_) => true,
            };
            if is_orphaned_successor || is_invalid {
                report
                    .invalid_control_records
                    .push(format!("control_policy_version:{stored_hash}"));
            }
        }
        Ok(())
    }

    fn decode_control_observation(
        stored: &StoredControlObservation,
    ) -> Result<ObservedTurnDecision, StoreError> {
        let input_hash = ObjectHash::from_stored(stored.input_hash.clone())
            .ok_or_else(|| StoreError::InvalidStoredHash(stored.input_hash.clone()))?;
        let input: TurnEvaluationInput =
            CanonicalObject::verify(&input_hash, stored.input_json.clone())?.decode()?;
        let expected_intent = CanonicalObject::freeze(&TurnObservationIntentFingerprint {
            control_schema_version: input.control_schema_version,
            session_id: &input.session_id,
            task_id: input.task_id,
            intent: &input.intent,
        })?;
        let decision_hash = ObjectHash::from_stored(stored.decision_hash.clone())
            .ok_or_else(|| StoreError::InvalidStoredHash(stored.decision_hash.clone()))?;
        let observation: ObservedTurnDecision =
            CanonicalObject::verify(&decision_hash, stored.decision_json.clone())?.decode()?;

        let input_task = input.task_id.map(|task_id| task_id.0.to_string());
        let row_matches = expected_intent.hash().as_str() == stored.intent_hash
            && observation.control_schema_version == CONTROL_SCHEMA_VERSION
            && input.session_id.0 == stored.session_id
            && input_task == stored.task_id
            && input.intent.idempotency_key == stored.idempotency_key
            && input.evaluated_at.timestamp_millis() == stored.observed_at_ms
            && observation.request_key == stored.idempotency_key
            && observation.observed_at.timestamp_millis() == stored.observed_at_ms;
        if !row_matches {
            return Err(StoreError::InvalidControlObservation(format!(
                "row {} does not match its input and decision",
                stored.sequence
            )));
        }

        let schema_matches = input.control_schema_version == CONTROL_SCHEMA_VERSION
            || matches!(
                &observation.decision,
                TurnDecision::Refuse { directive }
                    if directive.code == crate::domain::ControlRefusalCode::UnknownControlSchema
            );
        let decision_matches = schema_matches
            && match &observation.decision {
                TurnDecision::Grant { basis } => {
                    Some(basis.task_id) == input.task_id
                        && basis.session_id == input.session_id
                        && basis.purpose == input.intent.purpose
                        && basis.intent_fingerprint == input.intent.intent_fingerprint
                }
                TurnDecision::Refuse { directive } => {
                    directive.directive_id
                        == format!("{}:{}", stored.idempotency_key, directive.code.as_str())
                }
                TurnDecision::Defer { deferral } => !deferral.wake_condition.trim().is_empty(),
            };
        if !decision_matches {
            return Err(StoreError::InvalidControlObservation(format!(
                "decision {} is not bound to its input",
                stored.sequence
            )));
        }

        Ok(observation)
    }

    fn verify_control_turn_result(stored: &StoredControlTurnResult) -> Result<(), StoreError> {
        let intent = Self::decode_canonical_value(&stored.intent_hash, stored.intent_json.clone())?;
        let decision: ControlTurnDecision =
            Self::decode_canonical_projection(&stored.decision_hash, stored.decision_json.clone())?;
        let row_matches = intent.get("session_id").and_then(serde_json::Value::as_str)
            == Some(stored.session_id.as_str())
            && intent.get("task_id").and_then(serde_json::Value::as_str)
                == Some(stored.task_id.as_str())
            && intent
                .get("intent")
                .and_then(|value| value.get("idempotency_key"))
                .and_then(serde_json::Value::as_str)
                == Some(stored.idempotency_key.as_str());
        let decision_matches = match decision {
            ControlTurnDecision::Grant { grant } => {
                grant.control_schema_version == CONTROL_SCHEMA_VERSION
                    && grant.request_key == stored.idempotency_key
                    && grant.basis.session_id.0 == stored.session_id
                    && grant.basis.task_id.0.to_string() == stored.task_id
            }
            ControlTurnDecision::Refuse { directive } => directive
                .directive_id
                .starts_with(&format!("{}:", stored.idempotency_key)),
            ControlTurnDecision::Defer { deferral } => !deferral.wake_condition.trim().is_empty(),
        };
        if !row_matches || !decision_matches {
            return Err(StoreError::InvalidControlProjection(format!(
                "turn result {} is not bound to its row",
                stored.sequence
            )));
        }
        Ok(())
    }

    fn verify_control_grant_row(stored: &StoredControlGrantRow) -> Result<(), StoreError> {
        let grant: IssuedTurnGrant =
            Self::decode_canonical_projection(&stored.grant_hash, stored.grant_json.clone())?;
        let state = parse_enum::<TurnGrantState>(&stored.state)?;
        let delivery_matches = crate::control::delivery_matches_grant(&grant);
        let row_matches = grant.control_schema_version == CONTROL_SCHEMA_VERSION
            && grant.grant_id == stored.grant_id
            && grant.request_key == stored.request_key
            && grant.basis.session_id.0 == stored.session_id
            && grant.basis.task_id.0.to_string() == stored.task_id
            && grant.issued_at.timestamp_millis() == stored.issued_at_ms
            && grant.basis.expires_at.timestamp_millis() == stored.expires_at_ms
            && stored.expires_at_ms > stored.issued_at_ms
            && matches!(
                state,
                TurnGrantState::Issued
                    | TurnGrantState::Begun
                    | TurnGrantState::Completed
                    | TurnGrantState::Expired
            );
        if !row_matches || !delivery_matches {
            return Err(StoreError::InvalidControlProjection(format!(
                "turn grant {:?} is not bound to its row",
                stored.grant_id
            )));
        }
        Ok(())
    }

    fn verify_control_operation(stored: &StoredControlOperation) -> Result<(), StoreError> {
        let intent = Self::decode_canonical_value(&stored.intent_hash, stored.intent_json.clone())?;
        let result = Self::decode_canonical_value(&stored.result_hash, stored.result_json.clone())?;
        let row_matches = intent.get("session_id").and_then(serde_json::Value::as_str)
            == Some(stored.session_id.as_str())
            && intent
                .get("idempotency_key")
                .and_then(serde_json::Value::as_str)
                == Some(stored.idempotency_key.as_str())
            && match stored.operation.as_str() {
                "turn_begin" | "turn_checkpoint" | "lease_acquire" | "obligation_waive" => result
                    .get("decision")
                    .and_then(serde_json::Value::as_str)
                    .is_some(),
                "lease_release" => result
                    .get("lease_id")
                    .and_then(serde_json::Value::as_str)
                    .is_some(),
                _ => false,
            };
        if !row_matches {
            return Err(StoreError::InvalidControlProjection(format!(
                "control operation {} is not bound to its row",
                stored.sequence
            )));
        }
        Ok(())
    }

    fn verify_control_policy_operation(
        stored: &StoredControlPolicyOperation,
    ) -> Result<(), StoreError> {
        if stored.intent_json.len() > MAX_CONTROL_POLICY_OPERATION_INTENT_BYTES
            || stored.result_json.len() > MAX_CONTROL_POLICY_OPERATION_RESULT_BYTES
        {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy operation {} exceeds its canonical byte limits",
                stored.sequence
            )));
        }
        let intent = Self::decode_canonical_value(&stored.intent_hash, stored.intent_json.clone())?;
        let row_matches = intent
            .get("fingerprint_schema_version")
            .and_then(serde_json::Value::as_u64)
            == Some(u64::from(
                CONTROL_POLICY_OPERATION_FINGERPRINT_SCHEMA_VERSION,
            ))
            && intent.get("operation").and_then(serde_json::Value::as_str)
                == Some(stored.operation.as_str())
            && intent
                .get("idempotency_key")
                .and_then(serde_json::Value::as_str)
                == Some(stored.idempotency_key.as_str());
        if !row_matches {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy operation {} is not bound to its row",
                stored.sequence
            )));
        }
        match stored.operation.as_str() {
            "set_required_assurance" => {
                Self::decode_canonical_projection::<ControlPolicyUpdateReceipt>(
                    &stored.result_hash,
                    stored.result_json.clone(),
                )?;
            }
            "set_obligation_rule_set" => {
                Self::decode_canonical_projection::<ObligationRuleSetUpdateReceipt>(
                    &stored.result_hash,
                    stored.result_json.clone(),
                )?;
            }
            _ => {
                return Err(StoreError::InvalidControlProjection(format!(
                    "control policy operation {} has unknown operation {:?}",
                    stored.sequence, stored.operation
                )));
            }
        }
        Ok(())
    }

    fn decode_canonical_value(
        stored_hash: &str,
        bytes: Vec<u8>,
    ) -> Result<serde_json::Value, StoreError> {
        Self::decode_canonical_projection(stored_hash, bytes)
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

        let (scope_kind, project_id, task_id, work_id, agent_id) = match &version.scope {
            Scope::Project { project } => ("project", &project.0, None, None, None),
            Scope::Task { project, task } => {
                ("task", &project.0, Some(task.0.to_string()), None, None)
            }
            Scope::Work { project, work } => {
                ("work", &project.0, None, Some(work.0.to_string()), None)
            }
            Scope::Agent {
                project,
                task,
                work,
                agent,
            } => (
                "agent",
                &project.0,
                task.map(|value| value.0.to_string()),
                work.map(|value| value.0.to_string()),
                Some(agent.as_str()),
            ),
        };
        transaction.execute(
            "INSERT INTO memory_heads (
                 memory_id, version_hash, assertion_hash, schema_version,
                 status, scope_kind, project_id, task_id, work_id, agent_id,
                 memory_kind, authority, delivery, sensitivity, title, body,
                 created_at_ms
             ) VALUES (
                 ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                 ?14, ?15, ?16, ?17
             )
             ON CONFLICT(memory_id) DO UPDATE SET
                 version_hash = excluded.version_hash,
                 assertion_hash = excluded.assertion_hash,
                 schema_version = excluded.schema_version,
                 status = excluded.status,
                 scope_kind = excluded.scope_kind,
                 project_id = excluded.project_id,
                 task_id = excluded.task_id,
                 work_id = excluded.work_id,
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
                work_id,
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
            "SELECT COALESCE(MAX(task_cursor), 0) FROM task_changes WHERE task_id = ?1",
            [task_id.0.to_string()],
            |row| row.get(0),
        )?;
        Ok(ChangeCursor(cursor))
    }

    /// Loads the active policy used by one control decision without walking
    /// predecessor objects. The selected version is hash- and byte-verified,
    /// its scalar projection must match, and one aggregate must prove that it
    /// is the unique maximal contiguous head. Open, activation, and doctor use
    /// [`Self::verify_control_policy_history`] to additionally walk the audit
    /// chain; no historical version participates in a live grant decision.
    fn load_active_control_policy(
        connection: &Connection,
    ) -> Result<ControlPolicyProjection, StoreError> {
        let (projection, policy, _) = Self::load_control_policy_head(connection)?;
        if projection.state_schema_version != CONTROL_POLICY_STATE_SCHEMA_VERSION {
            return Err(StoreError::InvalidControlProjection(
                "active control policy uses a migratable legacy state schema".into(),
            ));
        }
        Self::validate_active_control_policy(&policy)?;
        let rule_set = policy.obligation_rule_set.as_ref().ok_or_else(|| {
            StoreError::InvalidControlProjection(
                "active control policy has no obligation rule-set selection".into(),
            )
        })?;
        Self::load_obligation_rule_set_on(connection, rule_set)?;
        Ok(projection)
    }

    fn verify_control_policy_history(
        connection: &Connection,
    ) -> Result<ControlPolicyProjection, StoreError> {
        let (projection, active_policy, active_authority) =
            Self::load_control_policy_head(connection)?;
        Self::verify_control_policy_chain(
            connection,
            &projection.policy_hash,
            &active_policy,
            active_authority,
        )?;
        Ok(projection)
    }

    fn load_control_policy_head(
        connection: &Connection,
    ) -> Result<
        (
            ControlPolicyProjection,
            ControlPolicy,
            ProjectPolicyAuthorityDecision,
        ),
        StoreError,
    > {
        let (
            schema_version,
            epoch,
            required_assurance,
            supported_effects,
            grant_ttl,
            policy_hash,
        ): (i64, i64, String, String, i64, Option<String>) = connection
            .query_row(
                "SELECT schema_version, policy_epoch, required_assurance,
                        supported_effects_json, grant_ttl_seconds, policy_hash
                 FROM control_policy_state WHERE singleton = 1",
                [],
                |row| {
                    Ok((
                        row.get(0)?,
                        row.get(1)?,
                        row.get(2)?,
                        row.get(3)?,
                        row.get(4)?,
                        row.get(5)?,
                    ))
                },
            )
            .optional()?
            .ok_or_else(|| {
                StoreError::InvalidControlProjection(
                    "control policy singleton is missing from an established store".into(),
                )
            })?;
        if ![
            LEGACY_VERSIONED_CONTROL_POLICY_STATE_SCHEMA_VERSION,
            LEGACY_REPLAYLESS_CONTROL_POLICY_STATE_SCHEMA_VERSION,
            CONTROL_POLICY_STATE_SCHEMA_VERSION,
        ]
        .contains(&schema_version)
            || epoch <= 0
            || grant_ttl <= 0
        {
            return Err(StoreError::InvalidControlProjection(
                "active control policy has an unknown state schema or invalid bounds".into(),
            ));
        }
        let policy_hash = policy_hash.ok_or_else(|| {
            StoreError::InvalidControlProjection(
                "active control policy has no selected version".into(),
            )
        })?;
        let active_hash = ObjectHash::from_stored(policy_hash.clone())
            .ok_or(StoreError::InvalidStoredHash(policy_hash))?;
        let (policy, authority) = Self::load_control_policy_version(connection, &active_hash)?;
        Self::validate_migratable_active_control_policy(&policy)?;
        if authority.schema_version != CONTROL_POLICY_AUTHORITY_SCHEMA_VERSION_V1 {
            return Err(StoreError::InvalidControlProjection(
                "active control policy authority uses an unsupported schema".into(),
            ));
        }
        let projected_effects: Vec<EffectClass> = serde_json::from_str(&supported_effects)?;
        let projected_assurance: ControlAssurance = parse_enum(&required_assurance)?;
        if policy.policy_epoch.0 != epoch
            || policy.required_assurance != projected_assurance
            || policy.supported_effects != projected_effects
            || policy.grant_ttl_seconds != grant_ttl
        {
            return Err(StoreError::InvalidControlProjection(
                "active control policy scalars do not match its canonical version".into(),
            ));
        }
        let successor_exists = connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM control_policy_versions WHERE policy_epoch > ?1
             )",
            [epoch],
            |row| row.get::<_, bool>(0),
        )?;
        if successor_exists {
            return Err(StoreError::InvalidControlProjection(
                "active control policy is not the maximal history head".into(),
            ));
        }
        let projection = ControlPolicyProjection {
            state_schema_version: schema_version,
            policy_hash: active_hash,
            authority_hash: policy.authority.clone(),
            epoch: policy.policy_epoch,
            required_assurance: policy.required_assurance,
            supported_effects: policy.supported_effects.clone(),
            grant_ttl_seconds: policy.grant_ttl_seconds,
            obligation_rule_set: policy.obligation_rule_set.clone(),
            activated_at: policy.activated_at,
        };
        Ok((projection, policy, authority))
    }

    fn load_control_policy_version(
        connection: &Connection,
        policy_hash: &ObjectHash,
    ) -> Result<(ControlPolicy, ProjectPolicyAuthorityDecision), StoreError> {
        #[cfg(test)]
        CONTROL_POLICY_VERSION_LOAD_COUNT.set(CONTROL_POLICY_VERSION_LOAD_COUNT.get() + 1);
        let (projected_epoch, authority_hash, projected_json): (i64, String, Vec<u8>) = connection
            .query_row(
                "SELECT policy_epoch, authority_hash, policy_json
                 FROM control_policy_versions WHERE policy_hash = ?1",
                [policy_hash.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )
            .optional()?
            .ok_or_else(|| {
                StoreError::InvalidControlProjection(format!(
                    "control policy version {policy_hash} is missing"
                ))
            })?;
        let policy_bytes =
            Self::load_control_object_bytes(connection, policy_hash, "control_policy")?;
        if policy_bytes != projected_json {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy {policy_hash} projection bytes do not match the canonical object"
            )));
        }
        let policy: ControlPolicy = CanonicalObject::verify(policy_hash, policy_bytes)?.decode()?;
        let stored_authority = ObjectHash::from_stored(authority_hash.clone())
            .ok_or(StoreError::InvalidStoredHash(authority_hash))?;
        if policy.policy_epoch.0 != projected_epoch || policy.authority != stored_authority {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy {policy_hash} is not bound to its version row"
            )));
        }
        Self::validate_control_policy_shape(&policy)?;
        let authority_bytes = Self::load_control_object_bytes(
            connection,
            &policy.authority,
            "project_policy_authority_decision",
        )?;
        let authority: ProjectPolicyAuthorityDecision =
            CanonicalObject::verify(&policy.authority, authority_bytes.clone())?.decode()?;
        if authority.schema_version == CONTROL_POLICY_AUTHORITY_SCHEMA_VERSION_V1
            && authority_bytes.len() > MAX_CONTROL_POLICY_AUTHORITY_BYTES
        {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy {policy_hash} authority exceeds its canonical byte limit"
            )));
        }
        // Historical authority schemas are checked against the common binding
        // envelope they record. V1 additionally pins its only known operation;
        // future schema-specific validators can extend this dispatch without
        // judging immutable predecessors by current active-policy constants.
        let authority_is_v1 =
            authority.schema_version == CONTROL_POLICY_AUTHORITY_SCHEMA_VERSION_V1;
        if authority.schema_version == 0
            || authority.policy_epoch != policy.policy_epoch
            || authority.previous_policy != policy.previous_policy
            || authority.required_assurance != policy.required_assurance
            || authority.obligation_rule_set != policy.obligation_rule_set
            || authority.decided_at != policy.activated_at
            || (authority_is_v1
                && (matches!(authority.operation, ProjectPolicyOperation::Unknown)
                    || authority.authorized_by.assurance != AssuranceLevel::Asserted))
        {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy {policy_hash} authority is invalid"
            )));
        }
        if authority_is_v1 {
            validate_control_policy_actor_shape(&authority.authorized_by)?;
            if normalize_control_text(&authority.reason, "control policy authority reason")?
                != authority.reason
            {
                return Err(StoreError::InvalidControlProjection(format!(
                    "control policy {policy_hash} authority reason is not normalized"
                )));
            }
        }
        Ok((policy, authority))
    }

    fn load_control_object_bytes(
        connection: &Connection,
        hash: &ObjectHash,
        expected_kind: &str,
    ) -> Result<Vec<u8>, StoreError> {
        let (stored_kind, bytes): (String, Vec<u8>) = connection
            .query_row(
                "SELECT object_kind, canonical_json FROM objects WHERE object_hash = ?1",
                [hash.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or_else(|| {
                StoreError::InvalidControlProjection(format!(
                    "canonical {expected_kind} object {hash} is missing"
                ))
            })?;
        if stored_kind != expected_kind {
            return Err(StoreError::ObjectKindMismatch {
                hash: hash.clone(),
                stored: stored_kind,
                requested: expected_kind.into(),
            });
        }
        CanonicalObject::verify(hash, bytes.clone())?;
        Ok(bytes)
    }

    fn validate_control_policy_shape(policy: &ControlPolicy) -> Result<(), StoreError> {
        // Every decodable historical schema shares these linkage and bounds
        // invariants. V1 has an additional schema-specific wire constraint;
        // active compatibility remains strict in validate_active_control_policy.
        let unique_effects: HashSet<_> = policy.supported_effects.iter().collect();
        if policy.schema_version == 0
            || policy.control_schema_version == 0
            || policy.policy_epoch.0 <= 0
            || policy.grant_ttl_seconds <= 0
            || policy.supported_effects.is_empty()
            || unique_effects.len() != policy.supported_effects.len()
            || (policy.policy_epoch.0 == 1) != policy.previous_policy.is_none()
        {
            return Err(StoreError::InvalidControlProjection(
                "canonical control policy has an invalid structural shape".into(),
            ));
        }
        if policy.schema_version == CONTROL_POLICY_SCHEMA_VERSION_V1
            && (policy.control_schema_version != CONTROL_POLICY_CONTROL_SCHEMA_VERSION_V1
                || policy.grant_ttl_seconds > CONTROL_POLICY_V1_MAX_GRANT_TTL_SECONDS)
        {
            return Err(StoreError::InvalidControlProjection(
                "canonical V1 control policy declares an incompatible control schema".into(),
            ));
        }
        Ok(())
    }

    fn validate_active_control_policy(policy: &ControlPolicy) -> Result<(), StoreError> {
        Self::validate_migratable_active_control_policy(policy)?;
        if policy.schema_version != CONTROL_POLICY_SCHEMA_VERSION_V1
            || policy.control_schema_version != CONTROL_SCHEMA_VERSION
            || policy.grant_ttl_seconds > MAX_CONTROL_GRANT_TTL_SECONDS
            || policy.supported_effects != Self::builtin_control_effects()
            || policy.obligation_rule_set.is_none()
        {
            return Err(StoreError::InvalidControlProjection(
                "active control policy has an unsupported schema or effect envelope".into(),
            ));
        }
        Ok(())
    }

    fn validate_migratable_active_control_policy(policy: &ControlPolicy) -> Result<(), StoreError> {
        Self::validate_control_policy_shape(policy)?;
        if policy.schema_version != CONTROL_POLICY_SCHEMA_VERSION_V1
            || policy.control_schema_version != CONTROL_SCHEMA_VERSION
            || !Self::is_recognized_builtin_envelope(
                &policy.supported_effects,
                policy.grant_ttl_seconds,
            )
        {
            return Err(StoreError::InvalidControlProjection(
                "active control policy has an unsupported schema or effect envelope".into(),
            ));
        }
        Ok(())
    }

    fn validate_control_policy_transition(
        previous: Option<&ControlPolicy>,
        current: &ControlPolicy,
        authority: &ProjectPolicyAuthorityDecision,
    ) -> Result<(), StoreError> {
        if current.schema_version != CONTROL_POLICY_SCHEMA_VERSION_V1
            || authority.schema_version != CONTROL_POLICY_AUTHORITY_SCHEMA_VERSION_V1
        {
            return Ok(());
        }
        match authority.operation {
            ProjectPolicyOperation::SetRequiredAssurance => {
                Self::validate_assurance_policy_transition(previous, current)?;
            }
            ProjectPolicyOperation::UpgradeBuiltinEnvelope => {
                Self::validate_builtin_envelope_transition(previous, current)?;
            }
            ProjectPolicyOperation::UpgradeBuiltinObligationRules => {
                Self::validate_builtin_obligation_rule_transition(previous, current)?;
            }
            ProjectPolicyOperation::SetObligationRuleSet => {
                Self::validate_obligation_rule_set_transition(previous, current)?;
            }
            ProjectPolicyOperation::Unknown => {
                return Err(StoreError::InvalidControlProjection(
                    "a V1 control policy authority uses an unknown operation".into(),
                ));
            }
        }
        Ok(())
    }

    fn validate_assurance_policy_transition(
        previous: Option<&ControlPolicy>,
        current: &ControlPolicy,
    ) -> Result<(), StoreError> {
        let envelope_changed = previous.is_some_and(|previous| {
            previous.schema_version == CONTROL_POLICY_SCHEMA_VERSION_V1
                && (current.supported_effects != previous.supported_effects
                    || current.grant_ttl_seconds != previous.grant_ttl_seconds)
        });
        let invalid_epoch_one = previous.is_none()
            && !Self::is_recognized_builtin_envelope(
                &current.supported_effects,
                current.grant_ttl_seconds,
            );
        let rule_set_changed = previous
            .is_some_and(|previous| current.obligation_rule_set != previous.obligation_rule_set);
        if envelope_changed || invalid_epoch_one || rule_set_changed {
            return Err(StoreError::InvalidControlProjection(
                "a V1 SetRequiredAssurance policy transition changed a preserved policy field"
                    .into(),
            ));
        }
        Ok(())
    }

    fn validate_builtin_envelope_transition(
        previous: Option<&ControlPolicy>,
        current: &ControlPolicy,
    ) -> Result<(), StoreError> {
        let Some(previous) = previous else {
            return Err(StoreError::InvalidControlProjection(
                "a V1 UpgradeBuiltinEnvelope policy transition cannot create epoch one".into(),
            ));
        };
        let previous_is_recognized = Self::is_recognized_builtin_envelope(
            &previous.supported_effects,
            previous.grant_ttl_seconds,
        );
        let current_is_recognized = Self::is_recognized_builtin_envelope(
            &current.supported_effects,
            current.grant_ttl_seconds,
        );
        let envelope_changed = current.supported_effects != previous.supported_effects
            || current.grant_ttl_seconds != previous.grant_ttl_seconds;
        if current.required_assurance != previous.required_assurance
            || current.obligation_rule_set != previous.obligation_rule_set
            || !previous_is_recognized
            || !current_is_recognized
            || !envelope_changed
        {
            return Err(StoreError::InvalidControlProjection(
                "a V1 UpgradeBuiltinEnvelope transition must preserve assurance and move between recognized built-in envelopes"
                    .into(),
            ));
        }
        Ok(())
    }

    fn validate_builtin_obligation_rule_transition(
        previous: Option<&ControlPolicy>,
        current: &ControlPolicy,
    ) -> Result<(), StoreError> {
        let Some(previous) = previous else {
            return Err(StoreError::InvalidControlProjection(
                "an obligation rule-set policy transition cannot create epoch one".into(),
            ));
        };
        let builtin_hash = CanonicalObject::freeze(&crate::control::builtin_obligation_rule_set())?
            .hash()
            .clone();
        if current.required_assurance != previous.required_assurance
            || current.supported_effects != previous.supported_effects
            || current.grant_ttl_seconds != previous.grant_ttl_seconds
            || previous.obligation_rule_set.is_some()
            || current.obligation_rule_set.as_ref() != Some(&builtin_hash)
        {
            return Err(StoreError::InvalidControlProjection(
                "the built-in obligation rule upgrade must select the stock set from an unselected legacy policy"
                    .into(),
            ));
        }
        Ok(())
    }

    fn validate_obligation_rule_set_transition(
        previous: Option<&ControlPolicy>,
        current: &ControlPolicy,
    ) -> Result<(), StoreError> {
        let Some(previous) = previous else {
            return Err(StoreError::InvalidControlProjection(
                "a rule-set selection cannot create policy epoch one".into(),
            ));
        };
        if current.required_assurance != previous.required_assurance
            || current.supported_effects != previous.supported_effects
            || current.grant_ttl_seconds != previous.grant_ttl_seconds
            || current.obligation_rule_set.is_none()
            || current.obligation_rule_set == previous.obligation_rule_set
        {
            return Err(StoreError::InvalidControlProjection(
                "a rule-set selection must change only the selected obligation rule set".into(),
            ));
        }
        Ok(())
    }

    fn verify_control_policy_chain(
        connection: &Connection,
        active_hash: &ObjectHash,
        active_policy: &ControlPolicy,
        active_authority: ProjectPolicyAuthorityDecision,
    ) -> Result<(), StoreError> {
        let mut seen = HashSet::new();
        let mut current_hash = active_hash.clone();
        let mut current_policy = active_policy.clone();
        let mut current_authority = active_authority;
        loop {
            if !seen.insert(current_hash.clone()) {
                return Err(StoreError::InvalidControlProjection(
                    "control policy history contains a cycle".into(),
                ));
            }
            match current_policy.previous_policy.clone() {
                Some(previous_hash) => {
                    let (previous, previous_authority) =
                        Self::load_control_policy_version(connection, &previous_hash)?;
                    if previous.policy_epoch.0.checked_add(1) != Some(current_policy.policy_epoch.0)
                    {
                        return Err(StoreError::InvalidControlProjection(
                            "control policy history has a non-contiguous epoch".into(),
                        ));
                    }
                    Self::validate_control_policy_transition(
                        Some(&previous),
                        &current_policy,
                        &current_authority,
                    )?;
                    current_hash = previous_hash;
                    current_policy = previous;
                    current_authority = previous_authority;
                }
                None if current_policy.policy_epoch.0 == 1 => {
                    Self::validate_control_policy_transition(
                        None,
                        &current_policy,
                        &current_authority,
                    )?;
                    break;
                }
                None => {
                    return Err(StoreError::InvalidControlProjection(
                        "control policy history ends before epoch one".into(),
                    ));
                }
            }
        }
        let expected_versions = usize::try_from(active_policy.policy_epoch.0).map_err(|_| {
            StoreError::InvalidControlProjection("control policy history count overflowed".into())
        })?;
        if seen.len() != expected_versions {
            return Err(StoreError::InvalidControlProjection(
                "control policy history contains unreachable version rows".into(),
            ));
        }
        Ok(())
    }

    fn control_count(value: i64, label: &str) -> Result<usize, StoreError> {
        usize::try_from(value)
            .map_err(|_| StoreError::InvalidControlProjection(format!("{label} count overflowed")))
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the loader verifies every redundant scalar and canonical work-binding field together"
    )]
    fn load_control_session_on(
        connection: &Connection,
        session_id: &SessionId,
    ) -> Result<Option<StoredControlSession>, StoreError> {
        let raw = connection
            .query_row(
                "SELECT project_id, task_id, root_execution_id, work_id, run_id,
                        work_revision, claim_id, claim_fence, routing_token, actor_json,
                        bind_key, bind_intent_hash, bind_intent_json, phase, assurance,
                        mediated_effects_json, confirmed_cursor, tentative_cursor,
                        project_policy_epoch, task_admission_epoch, blocking_watermark,
                        capability_map_revision, revision,
                        (SELECT grant_id FROM control_turn_grants g
                         WHERE g.session_id = control_sessions.session_id
                           AND g.state IN ('issued', 'begun')
                         ORDER BY g.issued_at_ms DESC LIMIT 1)
                 FROM control_sessions WHERE session_id = ?1",
                [session_id.0.as_str()],
                |row| {
                    Ok(RawControlSession {
                        project_id: row.get(0)?,
                        task_id: row.get(1)?,
                        root_execution_id: row.get(2)?,
                        work_id: row.get(3)?,
                        run_id: row.get(4)?,
                        work_revision: row.get(5)?,
                        claim_id: row.get(6)?,
                        claim_fence: row.get(7)?,
                        routing_token: row.get(8)?,
                        actor_json: row.get(9)?,
                        bind_key: row.get(10)?,
                        bind_intent_hash: row.get(11)?,
                        bind_intent_json: row.get(12)?,
                        phase: row.get(13)?,
                        assurance: row.get(14)?,
                        mediated_effects_json: row.get(15)?,
                        confirmed_cursor: row.get(16)?,
                        tentative_cursor: row.get(17)?,
                        project_policy_epoch: row.get(18)?,
                        task_admission_epoch: row.get(19)?,
                        blocking_watermark: row.get(20)?,
                        capability_map_revision: row.get(21)?,
                        revision: row.get(22)?,
                        open_grant_id: row.get(23)?,
                    })
                },
            )
            .optional()?;
        raw.map(|raw| {
            let task_id = uuid::Uuid::parse_str(&raw.task_id)
                .map(TaskId)
                .map_err(|error| StoreError::InvalidControlProjection(error.to_string()))?;
            let actor: ActorContext = serde_json::from_slice(&raw.actor_json)?;
            let mediated_effects: Vec<EffectClass> =
                serde_json::from_str(&raw.mediated_effects_json)?;
            let bind_hash = ObjectHash::from_stored(raw.bind_intent_hash.clone())
                .ok_or_else(|| StoreError::InvalidStoredHash(raw.bind_intent_hash.clone()))?;
            let bind_value: serde_json::Value =
                CanonicalObject::verify(&bind_hash, raw.bind_intent_json.clone())?.decode()?;
            let work_binding = match (
                raw.root_execution_id,
                raw.work_id,
                raw.run_id,
                raw.work_revision,
                raw.claim_id,
                raw.claim_fence,
            ) {
                (None, None, None, None, None, None) => None,
                (
                    Some(root_execution_id),
                    Some(work_id),
                    Some(run_id),
                    Some(work_revision),
                    Some(claim_id),
                    Some(claim_fence),
                ) => Some(ControlWorkBinding {
                    root_execution_id: crate::domain::RootExecutionId(
                        uuid::Uuid::parse_str(&root_execution_id).map_err(|error| {
                            StoreError::InvalidControlProjection(error.to_string())
                        })?,
                    ),
                    work_id: crate::domain::WorkId(uuid::Uuid::parse_str(&work_id).map_err(
                        |error| StoreError::InvalidControlProjection(error.to_string()),
                    )?),
                    run_id: crate::domain::WorkRunId(uuid::Uuid::parse_str(&run_id).map_err(
                        |error| StoreError::InvalidControlProjection(error.to_string()),
                    )?),
                    work_revision,
                    claim_id: crate::domain::WorkClaimId(
                        uuid::Uuid::parse_str(&claim_id).map_err(|error| {
                            StoreError::InvalidControlProjection(error.to_string())
                        })?,
                    ),
                    claim_fence,
                }),
                _ => {
                    return Err(StoreError::InvalidControlProjection(format!(
                        "control session {:?} has a partial work binding",
                        session_id.0
                    )));
                }
            };
            let canonical_work_binding = bind_value
                .get("work_binding")
                .cloned()
                .map(serde_json::from_value::<ControlWorkBinding>)
                .transpose()?;
            if raw.confirmed_cursor < 0
                || raw.tentative_cursor.is_some_and(|cursor| cursor < 0)
                || raw.project_policy_epoch < 0
                || raw.task_admission_epoch < 0
                || raw.blocking_watermark < 0
                || raw.capability_map_revision < 0
                || raw.revision <= 0
                || work_binding
                    .as_ref()
                    .is_some_and(|binding| binding.work_revision <= 0 || binding.claim_fence <= 0)
                || mediated_effects.is_empty()
                || actor.session_id.as_ref() != Some(session_id)
                || actor.run_id.as_deref()
                    != work_binding
                        .as_ref()
                        .map(|binding| binding.run_id.0.to_string())
                        .as_deref()
                || canonical_work_binding != work_binding
                || bind_value
                    .get("project_id")
                    .and_then(serde_json::Value::as_str)
                    != Some(raw.project_id.as_str())
                || bind_value
                    .get("session_id")
                    .and_then(serde_json::Value::as_str)
                    != Some(session_id.0.as_str())
                || bind_value
                    .get("idempotency_key")
                    .and_then(serde_json::Value::as_str)
                    != Some(raw.bind_key.as_str())
            {
                return Err(StoreError::InvalidControlProjection(format!(
                    "control session {:?} contains invalid bounds or actor binding",
                    session_id.0
                )));
            }
            Ok(StoredControlSession {
                project_id: crate::domain::ProjectId(raw.project_id),
                task_id,
                work_binding,
                session_id: session_id.clone(),
                routing_token: raw.routing_token,
                actor,
                bind_key: raw.bind_key,
                bind_intent_hash: raw.bind_intent_hash,
                phase: parse_enum(&raw.phase)?,
                assurance: parse_enum(&raw.assurance)?,
                mediated_effects,
                confirmed_cursor: ChangeCursor(raw.confirmed_cursor),
                tentative_cursor: raw.tentative_cursor.map(ChangeCursor),
                epochs: ControlEpochs {
                    project_policy: ProjectPolicyEpoch(raw.project_policy_epoch),
                    task_admission: TaskAdmissionEpoch(raw.task_admission_epoch),
                },
                blocking_watermark: ChangeCursor(raw.blocking_watermark),
                capability_map_revision: raw.capability_map_revision,
                revision: raw.revision,
                open_grant_id: raw.open_grant_id,
            })
        })
        .transpose()
    }

    fn control_session_status_on(
        connection: &Connection,
        session: &StoredControlSession,
    ) -> Result<ControlSessionStatus, StoreError> {
        let recoverable_grant = session
            .open_grant_id
            .as_deref()
            .map(|grant_id| Self::load_turn_grant(connection, &session.session_id, grant_id))
            .transpose()?
            .flatten()
            .filter(|stored| {
                matches!(stored.state, TurnGrantState::Begun)
                    && safely_redeliverable_partial_recovery(&stored.grant)
            })
            .map(|stored| Box::new(stored.grant));
        Ok(ControlSessionStatus {
            control_schema_version: CONTROL_SCHEMA_VERSION,
            project_id: session.project_id.clone(),
            task_id: session.task_id,
            work_binding: session.work_binding.clone(),
            session_id: session.session_id.clone(),
            phase: session.phase,
            assurance: session.assurance,
            mediated_effects: session.mediated_effects.clone(),
            confirmed_cursor: session.confirmed_cursor,
            tentative_cursor: session.tentative_cursor,
            epochs: session.epochs,
            blocking_watermark: session.blocking_watermark,
            capability_map_revision: session.capability_map_revision,
            revision: session.revision,
            open_grant_id: session.open_grant_id.clone(),
            recoverable_grant,
        })
    }

    fn verify_control_connection(
        connection: &Connection,
        session_id: &SessionId,
        connection_token: &str,
    ) -> Result<(), StoreError> {
        let current: Option<String> = connection
            .query_row(
                "SELECT connection_token FROM control_connections WHERE session_id = ?1",
                [session_id.0.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if connection_token.trim().is_empty() || current.as_deref() != Some(connection_token) {
            return Err(StoreError::ControlConnectionSuperseded(
                session_id.0.clone(),
            ));
        }
        Ok(())
    }

    fn verify_control_session(
        session: &StoredControlSession,
        project_id: &crate::domain::ProjectId,
        routing_token: &str,
    ) -> Result<(), StoreError> {
        if &session.project_id != project_id {
            return Err(StoreError::ControlSessionNotBound(
                session.session_id.0.clone(),
            ));
        }
        if session.routing_token != routing_token || routing_token.trim().is_empty() {
            return Err(StoreError::ControlSessionTokenMismatch(
                session.session_id.0.clone(),
            ));
        }
        Ok(())
    }

    fn control_work_binding_is_current(
        connection: &Connection,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        binding: Option<&ControlWorkBinding>,
        now: DateTime<Utc>,
    ) -> Result<bool, StoreError> {
        let Some(binding) = binding else {
            return Ok(true);
        };
        match work::validate_control_work_binding_on(
            connection, project_id, session_id, binding, now,
        ) {
            Ok(()) => Ok(true),
            Err(
                StoreError::ControlWorkBindingStale { .. }
                | StoreError::WorkClaimMismatch { .. }
                | StoreError::WorkClaimLapsed { .. }
                | StoreError::WorkRevisionConflict { .. }
                | StoreError::WorkNotFound(_)
                | StoreError::InvalidWork(_),
            ) => Ok(false),
            Err(error) => Err(error),
        }
    }

    fn session_has_begun_turn(
        connection: &Connection,
        session_id: &SessionId,
    ) -> Result<bool, StoreError> {
        let exists = connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM control_turn_grants
                 WHERE session_id = ?1 AND state = 'begun'
             )",
            [session_id.0.as_str()],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(exists == 1)
    }

    fn begun_turn_pinning_lease(
        connection: &Connection,
        session_id: &SessionId,
        lease_id: &str,
    ) -> Result<Option<String>, StoreError> {
        let grant_ids = {
            let mut statement = connection.prepare(
                "SELECT grant_id FROM control_turn_grants
                 WHERE session_id = ?1 AND state = 'begun'
                 ORDER BY issued_at_ms, grant_id",
            )?;
            statement
                .query_map([session_id.0.as_str()], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?
        };
        if grant_ids.len() > 1 {
            return Err(StoreError::InvalidControlProjection(format!(
                "control session {:?} has more than one begun turn",
                session_id.0
            )));
        }
        let Some(grant_id) = grant_ids.into_iter().next() else {
            return Ok(None);
        };
        let grant = Self::load_turn_grant(connection, session_id, &grant_id)?.ok_or_else(|| {
            StoreError::InvalidControlProjection(format!(
                "begun turn {grant_id:?} disappeared while checking lease {lease_id:?}"
            ))
        })?;
        if !matches!(grant.state, TurnGrantState::Begun) {
            return Err(StoreError::InvalidControlProjection(format!(
                "turn {grant_id:?} is not begun while checking lease {lease_id:?}"
            )));
        }
        Ok(grant
            .grant
            .basis
            .leases
            .iter()
            .any(|lease| lease.lease_id == lease_id)
            .then_some(grant_id))
    }

    fn session_is_current_participant(
        connection: &Connection,
        project_id: &crate::domain::ProjectId,
        task_id: TaskId,
        session_id: &SessionId,
    ) -> Result<bool, StoreError> {
        let current = connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM tasks t JOIN task_participants p
                   ON p.task_id = t.task_id
                 JOIN session_bindings b
                   ON b.task_id = t.task_id AND b.session_id = p.session_id
                 WHERE t.task_id = ?1 AND t.project_id = ?2
                   AND p.session_id = ?3
             )",
            params![task_id.0.to_string(), project_id.0, session_id.0],
            |row| row.get::<_, i64>(0),
        )?;
        Ok(current == 1)
    }

    fn task_state_on(
        connection: &Connection,
        project_id: &crate::domain::ProjectId,
        task_id: TaskId,
    ) -> Result<TaskState, StoreError> {
        let state = connection.query_row(
            "SELECT state FROM tasks WHERE task_id = ?1 AND project_id = ?2",
            params![task_id.0.to_string(), project_id.0],
            |row| row.get::<_, String>(0),
        )?;
        parse_enum(&state)
    }

    fn resource_subjects_overlap(
        left: &crate::domain::ResourceSubject,
        right: &crate::domain::ResourceSubject,
    ) -> bool {
        left.covers(right) || right.covers(left)
    }

    fn work_lease_rows(
        connection: &Connection,
        task_id: TaskId,
    ) -> Result<Vec<StoredWorkLeaseRow>, StoreError> {
        let mut statement = connection.prepare(
            "SELECT lease_id, task_id, holder_session_id, lease_hash,
                    lease_json, state, expires_at_ms
             FROM control_work_leases WHERE task_id = ?1 ORDER BY lease_id",
        )?;
        let rows = statement.query_map([task_id.0.to_string()], |row| {
            Ok(StoredWorkLeaseRow {
                lease_id: row.get(0)?,
                task_id: row.get(1)?,
                holder_session_id: row.get(2)?,
                lease_hash: row.get(3)?,
                lease_json: row.get(4)?,
                state: row.get(5)?,
                expires_at_ms: row.get(6)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::Sqlite)
    }

    fn project_work_lease_rows(
        connection: &Connection,
        project_id: &crate::domain::ProjectId,
    ) -> Result<Vec<StoredWorkLeaseRow>, StoreError> {
        let mut statement = connection.prepare(
            "SELECT lease.lease_id, lease.task_id, lease.holder_session_id,
                    lease.lease_hash, lease.lease_json, lease.state, lease.expires_at_ms
             FROM control_work_leases lease
             JOIN tasks task ON task.task_id = lease.task_id
             WHERE task.project_id = ?1
             ORDER BY lease.lease_id",
        )?;
        let rows = statement.query_map([&project_id.0], |row| {
            Ok(StoredWorkLeaseRow {
                lease_id: row.get(0)?,
                task_id: row.get(1)?,
                holder_session_id: row.get(2)?,
                lease_hash: row.get(3)?,
                lease_json: row.get(4)?,
                state: row.get(5)?,
                expires_at_ms: row.get(6)?,
            })
        })?;
        rows.collect::<Result<Vec<_>, _>>()
            .map_err(StoreError::Sqlite)
    }

    fn work_lease_row(
        connection: &Connection,
        lease_id: &str,
    ) -> Result<Option<StoredWorkLeaseRow>, StoreError> {
        connection
            .query_row(
                "SELECT lease_id, task_id, holder_session_id, lease_hash,
                        lease_json, state, expires_at_ms
                 FROM control_work_leases WHERE lease_id = ?1",
                [lease_id],
                |row| {
                    Ok(StoredWorkLeaseRow {
                        lease_id: row.get(0)?,
                        task_id: row.get(1)?,
                        holder_session_id: row.get(2)?,
                        lease_hash: row.get(3)?,
                        lease_json: row.get(4)?,
                        state: row.get(5)?,
                        expires_at_ms: row.get(6)?,
                    })
                },
            )
            .optional()
            .map_err(StoreError::Sqlite)
    }

    fn decode_work_lease_row(row: &StoredWorkLeaseRow) -> Result<WorkLease, StoreError> {
        let lease: WorkLease =
            Self::decode_canonical_projection(&row.lease_hash, row.lease_json.clone())?;
        if lease.control_schema_version != CONTROL_SCHEMA_VERSION
            || lease.lease_id != row.lease_id
            || lease.task_id.0.to_string() != row.task_id
            || lease.holder.0 != row.holder_session_id
            || lease.expires_at.timestamp_millis() != row.expires_at_ms
            || lease.fence <= 0
            || lease.revision <= 0
            || !lease.subject.has_valid_shape()
            || !matches!(row.state.as_str(), "active" | "released" | "expired")
        {
            return Err(StoreError::InvalidControlProjection(format!(
                "work lease {:?} is not bound to its row",
                row.lease_id
            )));
        }
        Ok(lease)
    }

    fn active_work_lease_bases(
        connection: &Connection,
        task_id: TaskId,
        session_id: &SessionId,
        now: DateTime<Utc>,
    ) -> Result<Vec<crate::domain::LeaseBasis>, StoreError> {
        Self::work_lease_rows(connection, task_id)?
            .into_iter()
            .filter(|row| row.state == "active")
            .map(|row| Self::decode_work_lease_row(&row))
            .filter_map(|lease| match lease {
                Ok(lease) if lease.holder == *session_id && lease.expires_at > now => {
                    Some(Ok(lease.basis()))
                }
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            })
            .collect()
    }

    fn expire_unbegun_turn(
        transaction: &Transaction<'_>,
        session: &StoredControlSession,
        now: DateTime<Utc>,
    ) -> Result<bool, StoreError> {
        let expired = transaction.execute(
            "UPDATE control_turn_grants SET state = 'expired'
             WHERE session_id = ?1 AND state = 'issued' AND expires_at_ms <= ?2",
            params![session.session_id.0, now.timestamp_millis()],
        )?;
        if expired > 0 && matches!(session.phase, SessionPhase::TurnOpen) {
            transaction.execute(
                "UPDATE control_sessions SET
                     phase = 'sync_required', tentative_cursor = NULL,
                     revision = revision + 1, updated_at_ms = ?2
                 WHERE session_id = ?1",
                params![session.session_id.0, now.timestamp_millis()],
            )?;
            return Ok(true);
        }
        Ok(false)
    }

    fn load_turn_grant(
        connection: &Connection,
        session_id: &SessionId,
        grant_id: &str,
    ) -> Result<Option<StoredTurnGrant>, StoreError> {
        let row = connection
            .query_row(
                "SELECT grant_hash, grant_json, state
                 FROM control_turn_grants
                 WHERE grant_id = ?1 AND session_id = ?2",
                params![grant_id, session_id.0],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, Vec<u8>>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )
            .optional()?;
        row.map(|(hash, bytes, state)| {
            let grant = Self::decode_canonical_projection(&hash, bytes)?;
            Ok(StoredTurnGrant {
                grant,
                state: parse_enum(&state)?,
            })
        })
        .transpose()
    }

    fn decode_canonical_projection<T: DeserializeOwned>(
        stored_hash: &str,
        bytes: Vec<u8>,
    ) -> Result<T, StoreError> {
        let hash = ObjectHash::from_stored(stored_hash.to_owned())
            .ok_or_else(|| StoreError::InvalidStoredHash(stored_hash.to_owned()))?;
        CanonicalObject::verify(&hash, bytes)?.decode()
    }

    fn replay_control_operation<T: DeserializeOwned>(
        connection: &Connection,
        session_id: &SessionId,
        operation: &str,
        idempotency_key: &str,
        intent_hash: &ObjectHash,
    ) -> Result<Option<T>, StoreError> {
        let stored = connection
            .query_row(
                "SELECT intent_hash, result_hash, result_json
                 FROM control_operation_results
                 WHERE session_id = ?1 AND operation = ?2 AND idempotency_key = ?3",
                params![session_id.0, operation, idempotency_key],
                |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .optional()?;
        let Some((stored_intent, result_hash, result_json)) = stored else {
            return Ok(None);
        };
        if stored_intent != intent_hash.as_str() {
            return Err(StoreError::ControlOperationIdempotencyConflict {
                operation: operation.into(),
                key: idempotency_key.into(),
            });
        }
        Self::decode_canonical_projection(&result_hash, result_json).map(Some)
    }

    fn replay_control_policy_operation<T: DeserializeOwned>(
        connection: &Connection,
        operation: &str,
        idempotency_key: &str,
        intent: &CanonicalObject,
    ) -> Result<Option<T>, StoreError> {
        let stored = connection
            .query_row(
                "SELECT sequence, intent_hash, intent_json, result_hash, result_json
                 FROM control_policy_operation_results
                 WHERE operation = ?1 AND idempotency_key = ?2",
                params![operation, idempotency_key],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                        row.get::<_, String>(3)?,
                        row.get::<_, Vec<u8>>(4)?,
                    ))
                },
            )
            .optional()?;
        let Some((sequence, stored_intent_hash, stored_intent_json, result_hash, result_json)) =
            stored
        else {
            return Ok(None);
        };
        let stored_intent = ObjectHash::from_stored(stored_intent_hash.clone())
            .ok_or_else(|| StoreError::InvalidStoredHash(stored_intent_hash))?;
        CanonicalObject::verify(&stored_intent, stored_intent_json.clone())?;
        if stored_intent != *intent.hash() || stored_intent_json != intent.bytes() {
            return Err(StoreError::ControlOperationIdempotencyConflict {
                operation: operation.into(),
                key: idempotency_key.into(),
            });
        }
        if result_json.len() > MAX_CONTROL_POLICY_OPERATION_RESULT_BYTES {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy operation result {sequence} exceeds its canonical byte limit"
            )));
        }
        Self::decode_canonical_projection(&result_hash, result_json).map(Some)
    }

    fn persist_control_policy_operation<T: Serialize>(
        transaction: &Transaction<'_>,
        operation: &str,
        idempotency_key: &str,
        intent: &CanonicalObject,
        result: &T,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        if intent.bytes().len() > MAX_CONTROL_POLICY_OPERATION_INTENT_BYTES {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy operation intent exceeds the {MAX_CONTROL_POLICY_OPERATION_INTENT_BYTES}-byte canonical limit"
            )));
        }
        let result = CanonicalObject::freeze(result)?;
        if result.bytes().len() > MAX_CONTROL_POLICY_OPERATION_RESULT_BYTES {
            return Err(StoreError::InvalidControlProjection(format!(
                "control policy operation result exceeds the {MAX_CONTROL_POLICY_OPERATION_RESULT_BYTES}-byte canonical limit"
            )));
        }
        transaction.execute(
            "INSERT INTO control_policy_operation_results (
                 operation, idempotency_key, intent_hash, intent_json,
                 result_hash, result_json, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
            params![
                operation,
                idempotency_key,
                intent.hash().as_str(),
                intent.bytes(),
                result.hash().as_str(),
                result.bytes(),
                now.timestamp_millis(),
            ],
        )?;
        Ok(())
    }

    fn refuse_work_lease(
        transaction: &Transaction<'_>,
        session_id: &SessionId,
        idempotency_key: &str,
        intent: &CanonicalObject,
        directive: crate::domain::ControlDirective,
        now: DateTime<Utc>,
    ) -> Result<WorkLeaseDecision, StoreError> {
        let decision = WorkLeaseDecision::Refuse { directive };
        Self::persist_control_operation(
            transaction,
            session_id,
            "lease_acquire",
            idempotency_key,
            intent,
            &decision,
            now,
        )?;
        Ok(decision)
    }

    #[allow(
        clippy::too_many_arguments,
        reason = "operation idempotency rows bind every independent key component"
    )]
    fn persist_control_operation<T: Serialize>(
        transaction: &Transaction<'_>,
        session_id: &SessionId,
        operation: &str,
        idempotency_key: &str,
        intent: &CanonicalObject,
        result: &T,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        if idempotency_key.trim().is_empty() {
            return Err(StoreError::InvalidControlSession(
                "control operation idempotency key is empty".into(),
            ));
        }
        let result = CanonicalObject::freeze(result)?;
        transaction.execute(
            "INSERT INTO control_operation_results (
                 session_id, operation, idempotency_key, intent_hash, intent_json,
                 result_hash, result_json, created_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            params![
                session_id.0,
                operation,
                idempotency_key,
                intent.hash().as_str(),
                intent.bytes(),
                result.hash().as_str(),
                result.bytes(),
                now.timestamp_millis(),
            ],
        )?;
        Ok(())
    }

    fn ensure_task_participant_on(
        connection: &Connection,
        project_id: &crate::domain::ProjectId,
        task_id: TaskId,
        session_id: &SessionId,
    ) -> Result<(), StoreError> {
        let participant: Option<i64> = connection
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

    fn ensure_active_task_on(
        connection: &Connection,
        project_id: &crate::domain::ProjectId,
        task_id: TaskId,
        session_id: &SessionId,
    ) -> Result<(), StoreError> {
        Self::ensure_task_participant_on(connection, project_id, task_id, session_id)?;
        let bound_task = connection
            .query_row(
                "SELECT task_id FROM session_bindings WHERE session_id = ?1",
                [session_id.0.as_str()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        if bound_task.as_deref() != Some(task_id.0.to_string().as_str()) {
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
        Self::get_typed_object_on(&self.connection, hash, object_kind)
    }

    fn get_typed_object_on<T: DeserializeOwned>(
        connection: &Connection,
        hash: &ObjectHash,
        object_kind: &str,
    ) -> Result<Option<T>, StoreError> {
        Self::get_canonical_object_on(connection, hash, object_kind)?
            .map(|object| object.decode())
            .transpose()
    }

    fn get_canonical_object_on(
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
            row.get(14)?,
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

    fn insert_object(
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

    fn insert_task_change(
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
        transaction.execute(
            "INSERT OR IGNORE INTO task_changes (
                 task_id, task_cursor, object_kind, object_hash
             )
             SELECT ?1, COALESCE(MAX(task_cursor), 0) + 1, ?2, ?3
             FROM task_changes WHERE task_id = ?1",
            params![task_id.0.to_string(), object_kind, object.hash().as_str()],
        )?;
        let cursor = transaction.query_row(
            "SELECT task_cursor FROM task_changes
             WHERE task_id = ?1 AND object_hash = ?2",
            params![task_id.0.to_string(), object.hash().as_str()],
            |row| row.get(0),
        )?;
        Ok(ChangeCursor(cursor))
    }
}

fn validate_execution_observation_inputs(
    observations: &[ExecutionObservationInput],
) -> Result<(), StoreError> {
    if observations.len() > MAX_EXECUTION_OBSERVATIONS_PER_CHECKPOINT {
        return Err(StoreError::InvalidControlProjection(format!(
            "turn checkpoint accepts at most {MAX_EXECUTION_OBSERVATIONS_PER_CHECKPOINT} execution observations"
        )));
    }
    let mut ids = HashSet::new();
    for observation in observations {
        let id = observation.observation_id.trim();
        if id.is_empty() || id.len() > 256 || id != observation.observation_id || !ids.insert(id) {
            return Err(StoreError::InvalidControlProjection(
                "execution observation ids must be unique, trimmed, nonempty, and at most 256 bytes"
                    .into(),
            ));
        }
        if observation.source_changed
            && !matches!(
                observation.effect,
                EffectClass::MutateLocal | EffectClass::MutateShared
            )
        {
            return Err(StoreError::InvalidControlProjection(format!(
                "execution observation {id:?} reports a source mutation for a non-mutation effect"
            )));
        }
        match (&observation.source_basis, observation.observed_at) {
            (None, None) => {}
            (Some(source_basis), Some(_)) => {
                validate_execution_source_basis(source_basis, id)?;
            }
            _ => {
                return Err(StoreError::InvalidControlProjection(format!(
                    "execution observation {id:?} must supply source_basis and observed_at together"
                )));
            }
        }
    }
    Ok(())
}

fn validate_typed_evidence_inputs<R: Redactor>(
    verification: &[VerificationEvidenceInput],
    environment: &[EnvironmentEvidenceInput],
    now: DateTime<Utc>,
    redactor: &R,
) -> Result<(), StoreError> {
    if verification.len() > MAX_VERIFICATION_EVIDENCE_PER_CHECKPOINT {
        return Err(StoreError::InvalidControlSession(format!(
            "turn checkpoint accepts at most {MAX_VERIFICATION_EVIDENCE_PER_CHECKPOINT} verification evidence records"
        )));
    }
    if environment.len() > MAX_ENVIRONMENT_EVIDENCE_PER_CHECKPOINT {
        return Err(StoreError::InvalidControlSession(format!(
            "turn checkpoint accepts at most {MAX_ENVIRONMENT_EVIDENCE_PER_CHECKPOINT} environment evidence records"
        )));
    }
    for input in verification {
        match &input.producer_observation {
            ExecutionObservationReference::ObjectHash { .. } => {}
            ExecutionObservationReference::ObservationId { observation_id } => {
                let trimmed = observation_id.trim();
                if trimmed.is_empty() || trimmed != observation_id || observation_id.len() > 256 {
                    return Err(StoreError::InvalidControlSession(
                        "verification observation ids must be trimmed, nonempty, and at most 256 bytes"
                            .into(),
                    ));
                }
            }
        }
        if input.summary.as_ref().is_some_and(|summary| {
            let trimmed = summary.trim();
            trimmed.is_empty()
                || trimmed != summary
                || summary.len() > MAX_TYPED_EVIDENCE_SUMMARY_BYTES
        }) {
            return Err(StoreError::InvalidControlSession(format!(
                "verification summaries must be trimmed, nonempty, and at most {MAX_TYPED_EVIDENCE_SUMMARY_BYTES} bytes"
            )));
        }
        validate_typed_evidence_refs(&input.refs)?;
    }
    for (index, input) in environment.iter().enumerate() {
        validate_execution_source_basis(&input.source_basis, &format!("environment-{index}"))?;
        if let Some(components) = &input.components {
            validate_environment_components(components, &input.source_basis, redactor)?;
            if environment_components_fingerprint(components)? != input.environment_fingerprint {
                return Err(StoreError::EnvironmentFingerprintMismatch);
            }
        }
        if input.observed_at > now {
            return Err(StoreError::InvalidControlSession(format!(
                "environment evidence {index} is timestamped after its checkpoint"
            )));
        }
    }
    Ok(())
}

fn environment_components_fingerprint(
    components: &EnvironmentComponents,
) -> Result<ObjectHash, StoreError> {
    Ok(CanonicalObject::freeze(components)?.hash().clone())
}

fn validate_environment_components<R: Redactor>(
    components: &EnvironmentComponents,
    source_basis: &crate::domain::ExecutionSourceBasis,
    redactor: &R,
) -> Result<(), StoreError> {
    let valid_text = |value: &str| {
        let trimmed = value.trim();
        !trimmed.is_empty() && trimmed == value && value.len() <= 256
    };
    if !valid_text(&components.toolchain)
        || !valid_text(&components.workspace_id)
        || components
            .sandbox
            .as_deref()
            .is_some_and(|value| !valid_text(value))
        || components.workspace_id != source_basis.workspace_id
        || components.capability_map_revision <= 0
    {
        return Err(StoreError::InvalidControlSession(
            "environment components must be trimmed, nonempty, at most 256 bytes, and match their source workspace"
                .into(),
        ));
    }
    redactor
        .inspect(&components.toolchain)
        .map_err(StoreError::RedactionRefused)?;
    redactor
        .inspect(&components.workspace_id)
        .map_err(StoreError::RedactionRefused)?;
    if let Some(sandbox) = &components.sandbox {
        redactor
            .inspect(sandbox)
            .map_err(StoreError::RedactionRefused)?;
    }
    Ok(())
}

fn resolve_verification_environment_on(
    connection: &Connection,
    reference: Option<&EnvironmentEvidenceReference>,
    same_checkpoint: &[(ObjectHash, EnvironmentEvidence)],
    project_id: &crate::domain::ProjectId,
    binding: &ControlWorkBinding,
    source_basis: &crate::domain::ExecutionSourceBasis,
) -> Result<Option<ObjectHash>, StoreError> {
    let Some(reference) = reference else {
        return Ok(None);
    };
    let (hash, evidence) = match reference {
        EnvironmentEvidenceReference::Index { index } => same_checkpoint
            .get(*index)
            .cloned()
            .ok_or_else(|| StoreError::EnvironmentEvidenceNotFound(index.to_string()))?,
        EnvironmentEvidenceReference::ObjectHash { object_hash } => {
            let evidence = work::load_control_environment_evidence_on(connection, object_hash)?
                .ok_or_else(|| StoreError::EnvironmentEvidenceNotFound(object_hash.to_string()))?;
            (object_hash.clone(), evidence)
        }
    };
    let same_run = &evidence.project_id == project_id
        && evidence.binding.root_execution_id == binding.root_execution_id
        && evidence.binding.work_id == binding.work_id
        && evidence.binding.run_id == binding.run_id;
    if !same_run || evidence.source_basis.source_revision != source_basis.source_revision {
        return Err(StoreError::EnvironmentBasisMismatch(hash.to_string()));
    }
    Ok(Some(hash))
}

fn validate_execution_source_basis(
    source_basis: &crate::domain::ExecutionSourceBasis,
    label: &str,
) -> Result<(), StoreError> {
    let workspace_id = source_basis.workspace_id.trim();
    let source_revision = source_basis.source_revision.trim();
    if workspace_id.is_empty()
        || source_revision.is_empty()
        || workspace_id != source_basis.workspace_id
        || source_revision != source_basis.source_revision
        || workspace_id.len() > 512
        || source_revision.len() > 512
    {
        return Err(StoreError::InvalidControlSession(format!(
            "evidence {label:?} source basis fields must be trimmed, nonempty, and at most 512 bytes"
        )));
    }
    Ok(())
}

fn validate_typed_evidence_refs(refs: &[String]) -> Result<(), StoreError> {
    if refs.len() > MAX_TYPED_EVIDENCE_REFS {
        return Err(StoreError::InvalidControlSession(format!(
            "typed evidence accepts at most {MAX_TYPED_EVIDENCE_REFS} references"
        )));
    }
    if refs.iter().any(|reference| {
        let trimmed = reference.trim();
        trimmed.is_empty() || trimmed != reference || reference.len() > MAX_TYPED_EVIDENCE_REF_BYTES
    }) {
        return Err(StoreError::InvalidControlSession(format!(
            "typed evidence references must be trimmed, nonempty, and at most {MAX_TYPED_EVIDENCE_REF_BYTES} bytes"
        )));
    }
    Ok(())
}

fn validate_verification_producer(
    producer: &ExecutionObservation,
    project_id: &crate::domain::ProjectId,
    session_id: &SessionId,
    binding: &ControlWorkBinding,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    let actor_run_id = binding.run_id.0.to_string();
    let consistent = &producer.project_id == project_id
        && &producer.session_id == session_id
        && &producer.binding == binding
        && producer.actor.session_id.as_ref() == Some(session_id)
        && producer.actor.run_id.as_deref() == Some(actor_run_id.as_str());
    if !consistent {
        return Err(StoreError::InvalidControlSession(
            "verification producer does not match the checkpoint work/session binding".into(),
        ));
    }
    let source_basis = producer.source_basis.as_ref().ok_or_else(|| {
        StoreError::InvalidControlSession(
            "verification producer must carry a source content basis".into(),
        )
    })?;
    validate_execution_source_basis(source_basis, &producer.observation_id)?;
    let observed_at = producer.observed_at.ok_or_else(|| {
        StoreError::InvalidControlSession(
            "verification producer must carry observed_at with its source basis".into(),
        )
    })?;
    if observed_at > producer.recorded_at || producer.recorded_at > now {
        return Err(StoreError::InvalidControlSession(
            "verification producer timestamps are not monotone".into(),
        ));
    }
    Ok(())
}

const fn verification_result(outcome: ExecutionOutcome) -> VerificationResult {
    match outcome {
        ExecutionOutcome::Succeeded => VerificationResult::Passed,
        ExecutionOutcome::Failed => VerificationResult::Failed,
        ExecutionOutcome::Unknown => VerificationResult::Indeterminate,
    }
}

fn normalize_verification_summary(input: &VerificationEvidenceInput) -> String {
    input.summary.clone().unwrap_or_else(|| {
        format!(
            "host-recorded {} verification",
            verification_kind_name(input.check_kind)
        )
    })
}

const fn verification_kind_name(kind: VerificationKind) -> &'static str {
    match kind {
        VerificationKind::Test => "test",
        VerificationKind::Build => "build",
        VerificationKind::Lint => "lint",
        VerificationKind::Review => "review",
        VerificationKind::Acceptance => "acceptance",
    }
}

fn normalize_typed_evidence_refs(refs: &[String]) -> Vec<String> {
    let mut refs = refs.to_vec();
    refs.sort();
    refs.dedup();
    refs
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
    if request.task_id.is_some() && request.work_id.is_some() {
        return Err(StoreError::InvalidMemoryProjection(
            "one note cannot belong to both legacy task and local work scope".into(),
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

fn enum_name<T: Serialize>(value: T) -> Result<String, StoreError> {
    serde_json::to_value(value)?
        .as_str()
        .map(ToOwned::to_owned)
        .ok_or_else(|| StoreError::InvalidMemoryProjection("enum did not serialize as text".into()))
}

fn parse_enum<T: DeserializeOwned>(value: &str) -> Result<T, StoreError> {
    serde_json::from_value(serde_json::Value::String(value.to_owned())).map_err(StoreError::Json)
}

fn normalize_control_text(value: &str, label: &str) -> Result<String, StoreError> {
    let normalized = value.trim();
    if normalized.is_empty() || normalized.len() > 4_096 {
        return Err(StoreError::InvalidControlProjection(format!(
            "{label} must contain from 1 through 4096 bytes"
        )));
    }
    Ok(normalized.to_owned())
}

fn normalize_control_policy_idempotency_key(value: &str) -> Result<&str, StoreError> {
    let normalized = value.trim();
    if normalized.is_empty() || normalized.len() > MAX_CONTROL_POLICY_IDEMPOTENCY_KEY_BYTES {
        return Err(StoreError::InvalidControlProjection(format!(
            "control policy idempotency key must contain from 1 through {MAX_CONTROL_POLICY_IDEMPOTENCY_KEY_BYTES} bytes"
        )));
    }
    Ok(normalized)
}

fn normalize_optional_control_text(
    value: Option<&str>,
    label: &str,
) -> Result<Option<String>, StoreError> {
    value
        .map(|value| normalize_control_text(value, label))
        .transpose()
}

fn normalized_control_policy_actor(actor: &ActorContext) -> Result<ActorContext, StoreError> {
    if actor.provenance_chain.len() > MAX_CONTROL_POLICY_PROVENANCE_LINKS {
        return Err(StoreError::InvalidControlProjection(format!(
            "control policy administrator provenance must contain at most {MAX_CONTROL_POLICY_PROVENANCE_LINKS} links"
        )));
    }
    let mut normalized = actor.clone();
    normalized.actor_id =
        normalize_control_text(&normalized.actor_id, "control policy administrator actor")?;
    normalized.actor_kind =
        normalize_control_text(&normalized.actor_kind, "control policy administrator kind")?;
    normalized.reason = normalize_control_text(
        &normalized.reason,
        "control policy administrator attribution",
    )?;
    normalized.run_id = normalize_optional_control_text(
        normalized.run_id.as_deref(),
        "control policy administrator run",
    )?;
    normalized.session_id = normalized
        .session_id
        .as_ref()
        .map(|session| {
            normalize_control_text(&session.0, "control policy administrator session")
                .map(SessionId)
        })
        .transpose()?;
    normalized.source_tool = normalize_optional_control_text(
        normalized.source_tool.as_deref(),
        "control policy administrator source tool",
    )?;
    normalized.source_skill = normalize_optional_control_text(
        normalized.source_skill.as_deref(),
        "control policy administrator source skill",
    )?;
    for (index, link) in normalized.provenance_chain.iter_mut().enumerate() {
        link.source = normalize_control_text(
            &link.source,
            &format!("control policy administrator provenance source {index}"),
        )?;
        link.reference = normalize_optional_control_text(
            link.reference.as_deref(),
            &format!("control policy administrator provenance reference {index}"),
        )?;
    }

    let canonical_candidate = CanonicalObject::freeze(&normalized)?;
    if canonical_candidate.bytes().len() > MAX_CONTROL_POLICY_ATTRIBUTION_BYTES {
        return Err(StoreError::InvalidControlProjection(format!(
            "control policy administrator attribution exceeds the {MAX_CONTROL_POLICY_ATTRIBUTION_BYTES}-byte canonical limit"
        )));
    }
    Ok(normalized)
}

fn normalize_control_policy_actor<R: Redactor>(
    actor: &ActorContext,
    redactor: &R,
) -> Result<ActorContext, StoreError> {
    let normalized = normalized_control_policy_actor(actor)?;
    for prose in [
        Some(normalized.actor_id.as_str()),
        Some(normalized.actor_kind.as_str()),
        Some(normalized.reason.as_str()),
        normalized.run_id.as_deref(),
        normalized
            .session_id
            .as_ref()
            .map(|session| session.0.as_str()),
        normalized.source_tool.as_deref(),
        normalized.source_skill.as_deref(),
    ]
    .into_iter()
    .flatten()
    {
        redactor
            .inspect(prose)
            .map_err(StoreError::RedactionRefused)?;
    }
    for link in &normalized.provenance_chain {
        redactor
            .inspect(&link.source)
            .map_err(StoreError::RedactionRefused)?;
        if let Some(reference) = link.reference.as_deref() {
            redactor
                .inspect(reference)
                .map_err(StoreError::RedactionRefused)?;
        }
    }
    Ok(normalized)
}

fn validate_control_policy_actor_shape(actor: &ActorContext) -> Result<(), StoreError> {
    if normalized_control_policy_actor(actor)? != *actor {
        return Err(StoreError::InvalidControlProjection(
            "control policy administrator attribution is not normalized".into(),
        ));
    }
    Ok(())
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

#[cfg(test)]
mod tests {
    use chrono::{TimeDelta, TimeZone};
    use serde::{Deserialize, Serialize};

    use super::*;
    use crate::{
        DevelopmentNoopRedactor,
        domain::{
            AssuranceLevel, ControlAssurance, ControlEpochs, ControlHealth, EffectClass,
            MemoryStatus, NoteRequest, NoteVisibility, PacketSafety, ProjectId, ProjectPolicyEpoch,
            ProvenanceLink, ProvenanceRelation, SessionPhase, TaskAdmissionEpoch, TurnDecision,
            TurnEvaluationInput, TurnIntent, TurnPurpose,
        },
    };

    #[derive(Debug, Deserialize, Eq, PartialEq, Serialize)]
    struct Example {
        title: String,
        body: String,
    }

    struct SentinelRedactor;

    impl Redactor for SentinelRedactor {
        fn inspect(&self, prose: &str) -> Result<(), String> {
            if prose.contains("reject-me") {
                Err("test sentinel was rejected".into())
            } else {
                Ok(())
            }
        }

        fn description(&self) -> &'static str {
            "test sentinel redactor"
        }
    }

    #[test]
    fn environment_components_are_redactor_inspected_before_canonicalization() {
        let components = EnvironmentComponents {
            toolchain: "reject-me-toolchain".into(),
            sandbox: Some("sandbox-v1".into()),
            workspace_id: "workspace-redaction".into(),
            capability_map_revision: 1,
        };
        let input = EnvironmentEvidenceInput {
            source_basis: crate::ExecutionSourceBasis {
                workspace_id: components.workspace_id.clone(),
                source_revision: "revision-redaction".into(),
            },
            environment_fingerprint: environment_components_fingerprint(&components)
                .expect("freeze environment components"),
            components: Some(components),
            observed_at: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
        };
        assert!(matches!(
            validate_typed_evidence_inputs(
                &[],
                &[input],
                Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
                &SentinelRedactor,
            ),
            Err(StoreError::RedactionRefused(_))
        ));
    }

    #[test]
    fn context_omissions_are_exact_then_losslessly_aggregated() {
        let memories = (0..200)
            .map(|index| MemorySummary {
                memory_id: MemoryId::new(),
                version: ObjectHash::from_canonical_bytes(format!("memory-{index}").as_bytes()),
                status: MemoryStatus::Active,
                kind: crate::domain::MemoryKind::Fact,
                authority: crate::domain::Authority::Soft,
                delivery: Delivery::OnDemand,
                scope: Scope::Project {
                    project: ProjectId("project-a".into()),
                },
                title: format!("Memory {index}"),
                body: "Available through search".into(),
                sensitivity: Sensitivity::Internal,
                created_at: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
            })
            .collect();
        let assembly = assemble_context(memories, &[]).expect("bounded context assembly");
        assert_eq!(assembly.omissions.len(), MAX_EXACT_CONTEXT_OMISSIONS);
        assert_eq!(
            assembly.omission_summaries,
            vec![ContextOmissionSummary {
                reason: "on-demand memory is available through search".into(),
                count: 72,
            }]
        );
        assert_eq!(
            assembly.omissions.len()
                + assembly
                    .omission_summaries
                    .iter()
                    .map(|summary| usize::try_from(summary.count).unwrap())
                    .sum::<usize>(),
            200
        );
    }

    #[test]
    fn context_assembly_never_hides_old_pinned_memory_behind_search_limits() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        let task_id = TaskId::new();
        install_memory_task(&store, task_id, &["agent-a"]);
        let mut pinned_request = note_request(
            task_id,
            "agent-a",
            "Constraint: preserve the oldest pinned rule",
            "oldest-pinned",
            NoteVisibility::Shared,
        );
        pinned_request.created_at = Utc::now() - TimeDelta::days(1);
        let pinned = store
            .capture_note(&pinned_request, &DevelopmentNoopRedactor)
            .expect("capture oldest pinned record");
        for index in 0..1_000 {
            let mut request = note_request(
                task_id,
                "agent-a",
                &format!("Observation: bounded filler record {index}"),
                &format!("filler-{index}"),
                NoteVisibility::Shared,
            );
            request.created_at = Utc::now() + TimeDelta::milliseconds(i64::from(index));
            store
                .capture_note(&request, &DevelopmentNoopRedactor)
                .expect("capture filler memory");
        }
        let packet = store
            .build_context(
                &ProjectId("project-a".into()),
                Some(task_id),
                &SessionId("agent-a".into()),
                "agent-a",
                Utc::now(),
            )
            .expect("context includes all pinned candidates before budgeting");
        assert!(
            packet
                .pinned
                .iter()
                .any(|item| item.version == pinned.version)
        );
    }

    #[test]
    fn store_persists_and_enforces_one_host_path_identity_policy() {
        let directory = tempfile::tempdir().expect("temp directory");
        let database = directory.path().join("path-policy.sqlite3");
        let policy = HostPathPolicy {
            case_fold_paths: false,
            windows_alias_rules: false,
        };
        drop(
            SqliteStore::open_with_host_path_policy(&database, policy)
                .expect("bind explicit path policy"),
        );
        drop(
            SqliteStore::open_with_host_path_policy(&database, policy)
                .expect("same policy reopens"),
        );
        let mismatch = SqliteStore::open_with_host_path_policy(
            &database,
            HostPathPolicy {
                case_fold_paths: true,
                windows_alias_rules: true,
            },
        );
        assert!(matches!(
            mismatch,
            Err(StoreError::InvalidControlSession(_))
        ));

        let recovery_database = directory.path().join("path-policy-recovery.sqlite3");
        drop(
            SqliteStore::open_with_host_path_policy(&recovery_database, policy)
                .expect("initialize recoverable policy store"),
        );
        let recovery = Connection::open(&recovery_database).expect("recovery connection");
        recovery
            .execute("DELETE FROM control_host_path_policy", [])
            .expect("simulate crash after table creation");
        drop(recovery);
        drop(
            SqliteStore::open_with_host_path_policy(&recovery_database, policy)
                .expect("empty policy table is rebound atomically"),
        );

        let unsafe_database = directory.path().join("path-policy-unsafe.sqlite3");
        drop(
            SqliteStore::open_with_host_path_policy(&unsafe_database, policy)
                .expect("initialize unsafe policy store"),
        );
        let unsafe_connection = Connection::open(&unsafe_database).expect("unsafe connection");
        unsafe_connection
            .execute("PRAGMA foreign_keys = OFF", [])
            .expect("disable fixture foreign keys");
        unsafe_connection
            .execute("DELETE FROM control_host_path_policy", [])
            .expect("remove policy binding");
        unsafe_connection
            .execute(
                "INSERT INTO control_work_leases (
                     lease_id, task_id, holder_session_id, lease_hash, lease_json,
                     state, expires_at_ms
                 ) VALUES ('legacy-path', 'task', 'session', 'hash',
                           CAST('{\"subject\":{\"kind\":\"path\"}}' AS BLOB),
                           'active', 1)",
                [],
            )
            .expect("insert legacy path-bearing state");
        drop(unsafe_connection);
        assert!(matches!(
            SqliteStore::open_with_host_path_policy(&unsafe_database, policy),
            Err(StoreError::InvalidControlSession(_))
        ));
    }

    #[test]
    fn backup_copies_a_live_store_and_verifies_the_copy() {
        let directory = tempfile::tempdir().expect("temp directory");
        let database = directory.path().join("live.sqlite3");
        let mut store = SqliteStore::open(&database).expect("live store");
        let project = ProjectId("backup-project".into());
        let session = SessionId("backup-session".into());
        store
            .start_task(
                &project,
                "dummy:BACKUP-1",
                "Backup fixture",
                &session,
                actor("backup-agent"),
                Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
            )
            .expect("fixture task");
        let copy = directory.path().join("backups").join("copy.sqlite3");
        let manifest = store.backup_to(&copy).expect("backup");
        assert_eq!(manifest.path, copy);
        assert!(manifest.checked_objects > 0);
        assert_eq!(manifest.file_sha256.len(), 64);
        assert_eq!(
            manifest.file_bytes,
            std::fs::metadata(&copy).expect("copy metadata").len()
        );
        let reverified = SqliteStore::verify_backup(&copy).expect("verify the copy again");
        assert_eq!(reverified.file_sha256, manifest.file_sha256);
        assert_eq!(reverified.checked_objects, manifest.checked_objects);
        // The copy is a complete store: it opens and answers on its own.
        let restored = SqliteStore::open(&copy).expect("open the copy");
        assert!(restored.bound_task(&project, &session).is_ok());
        // An existing target is never overwritten.
        assert!(matches!(
            store.backup_to(&copy),
            Err(StoreError::InvalidWork(_))
        ));
    }

    #[test]
    fn unresolved_path_identity_refuses_path_leases_but_not_logical_ones() {
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let mut store =
            SqliteStore::open_in_memory_with_host_path_identity(None).expect("unresolved store");
        assert_eq!(store.host_path_identity(), None);
        let effects = [EffectClass::Observe, EffectClass::MutateLocal];
        let session = bind_control_for(&mut store, "unresolved", "bind-unresolved", &effects, now);
        complete_control_turn(
            &mut store,
            &session,
            "sync-unresolved",
            vec![EffectClass::Observe],
            Vec::new(),
            now + TimeDelta::seconds(1),
        );
        let path = crate::domain::ResourceSubject::Path {
            project_id: ProjectId("project-a".into()),
            segments: vec!["src".into()],
            coverage: crate::domain::ResourceCoverage::Tree,
        };
        assert!(matches!(
            store.acquire_work_lease(
                &ProjectId("project-a".into()),
                &session.status.session_id,
                &session.connection_token,
                &session.routing_token,
                crate::domain::LeaseKind::Execution,
                crate::domain::LeaseMode::Exclusive,
                &path,
                300,
                "lease-unresolved-path",
                now + TimeDelta::seconds(2),
            ),
            Err(StoreError::HostPathIdentityUnresolved)
        ));
        let logical = crate::domain::ResourceSubject::Logical {
            namespace: "engram".into(),
            segments: vec!["report".into()],
            coverage: crate::domain::ResourceCoverage::Exact,
        };
        assert!(matches!(
            store
                .acquire_work_lease(
                    &ProjectId("project-a".into()),
                    &session.status.session_id,
                    &session.connection_token,
                    &session.routing_token,
                    crate::domain::LeaseKind::Execution,
                    crate::domain::LeaseMode::Exclusive,
                    &logical,
                    300,
                    "lease-unresolved-logical",
                    now + TimeDelta::seconds(3),
                )
                .expect("logical leases need no path identity"),
            WorkLeaseDecision::Granted { .. }
        ));
        // A persisted policy is still binding for a later resolved opener,
        // while an unresolved opener may read the same store.
        let directory = tempfile::tempdir().expect("temp directory");
        let database = directory.path().join("identity.sqlite3");
        let folded = HostPathPolicy {
            case_fold_paths: true,
            windows_alias_rules: false,
        };
        drop(SqliteStore::open_with_host_path_policy(&database, folded).expect("bind folded"));
        let reader = SqliteStore::open_unresolved(&database).expect("unresolved opener reads");
        assert_eq!(
            reader.stored_host_path_policy().expect("stored policy"),
            Some(folded)
        );
        assert!(matches!(
            SqliteStore::open_with_host_path_policy(
                &database,
                HostPathPolicy {
                    case_fold_paths: false,
                    windows_alias_rules: false,
                },
            ),
            Err(StoreError::InvalidControlSession(_))
        ));
    }

    #[test]
    fn case_aliases_conflict_only_under_a_folding_policy() {
        for (case_fold_paths, expect_conflict) in [(true, true), (false, false)] {
            let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
            let mut store =
                SqliteStore::open_in_memory_with_host_path_identity(Some(HostPathPolicy {
                    case_fold_paths,
                    windows_alias_rules: false,
                }))
                .expect("explicit policy store");
            let effects = [EffectClass::Observe, EffectClass::MutateLocal];
            let session_a = bind_control_for(&mut store, "case-a", "bind-case-a", &effects, now);
            let session_b = bind_control_for(&mut store, "case-b", "bind-case-b", &effects, now);
            complete_control_turn(
                &mut store,
                &session_b,
                "sync-case-b",
                vec![EffectClass::Observe],
                Vec::new(),
                now + TimeDelta::seconds(1),
            );
            complete_control_turn(
                &mut store,
                &session_a,
                "sync-case-a",
                vec![EffectClass::Observe],
                Vec::new(),
                now + TimeDelta::seconds(2),
            );
            let lower = crate::domain::ResourceSubject::Path {
                project_id: ProjectId("project-a".into()),
                segments: vec!["src".into(), "Main.rs".into()],
                coverage: crate::domain::ResourceCoverage::Exact,
            };
            assert!(matches!(
                store
                    .acquire_work_lease(
                        &ProjectId("project-a".into()),
                        &session_a.status.session_id,
                        &session_a.connection_token,
                        &session_a.routing_token,
                        crate::domain::LeaseKind::Execution,
                        crate::domain::LeaseMode::Exclusive,
                        &lower,
                        300,
                        "lease-case-a",
                        now + TimeDelta::seconds(3),
                    )
                    .expect("first lease"),
                WorkLeaseDecision::Granted { .. }
            ));
            complete_control_turn(
                &mut store,
                &session_b,
                "resync-case-b",
                vec![EffectClass::Observe],
                Vec::new(),
                now + TimeDelta::seconds(3),
            );
            let upper = crate::domain::ResourceSubject::Path {
                project_id: ProjectId("project-a".into()),
                segments: vec!["SRC".into(), "main.RS".into()],
                coverage: crate::domain::ResourceCoverage::Exact,
            };
            let decision = store
                .acquire_work_lease(
                    &ProjectId("project-a".into()),
                    &session_b.status.session_id,
                    &session_b.connection_token,
                    &session_b.routing_token,
                    crate::domain::LeaseKind::Execution,
                    crate::domain::LeaseMode::Exclusive,
                    &upper,
                    300,
                    "lease-case-b",
                    now + TimeDelta::seconds(4),
                )
                .expect("second lease decision");
            assert_eq!(
                matches!(decision, WorkLeaseDecision::Defer { .. }),
                expect_conflict,
                "case_fold_paths={case_fold_paths} decided {decision:?}"
            );
        }
    }

    #[test]
    fn verify_backup_touches_nothing_and_backups_never_replace() {
        let directory = tempfile::tempdir().expect("temp directory");
        let missing = directory.path().join("absent.sqlite3");
        assert!(matches!(
            SqliteStore::verify_backup(&missing),
            Err(StoreError::InvalidWork(_))
        ));
        assert!(!missing.exists(), "verification must not create a store");

        let database = directory.path().join("live.sqlite3");
        let store = SqliteStore::open(&database).expect("live store");
        let target = directory.path().join("copies").join("one.sqlite3");
        let first = store.backup_to(&target).expect("first backup");
        let second = store.backup_to(&target);
        assert!(matches!(second, Err(StoreError::InvalidWork(_))));
        assert_eq!(
            SqliteStore::verify_backup(&target)
                .expect("the published copy is untouched")
                .file_sha256,
            first.file_sha256
        );
        let leftovers = std::fs::read_dir(target.parent().expect("copies directory"))
            .expect("list copies")
            .filter_map(Result::ok)
            .filter(|entry| entry.file_name().to_string_lossy().ends_with(".tmp"))
            .count();
        assert_eq!(leftovers, 0, "staged files never survive a refusal");
    }

    #[test]
    fn unresolved_opener_cannot_begin_a_path_bearing_grant() {
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let directory = tempfile::tempdir().expect("temp directory");
        let database = directory.path().join("unresolved-begin.sqlite3");
        let mut store = SqliteStore::open(&database).expect("resolved opener");
        let effects = [EffectClass::Observe, EffectClass::MutateLocal];
        let session = bind_control_for(&mut store, "ub-a", "bind-ub-a", &effects, now);
        complete_control_turn(
            &mut store,
            &session,
            "sync-ub-a",
            vec![EffectClass::Observe],
            Vec::new(),
            now + TimeDelta::seconds(1),
        );
        let subject = crate::domain::ResourceSubject::Path {
            project_id: ProjectId("project-a".into()),
            segments: vec!["src".into()],
            coverage: crate::domain::ResourceCoverage::Tree,
        };
        let WorkLeaseDecision::Granted { .. } = store
            .acquire_work_lease(
                &ProjectId("project-a".into()),
                &session.status.session_id,
                &session.connection_token,
                &session.routing_token,
                crate::domain::LeaseKind::Execution,
                crate::domain::LeaseMode::Exclusive,
                &subject,
                60,
                "ub-lease",
                now + TimeDelta::seconds(2),
            )
            .unwrap()
        else {
            panic!("the lease must grant on the resolved opener");
        };
        let ControlTurnDecision::Grant { grant } = store
            .evaluate_control_turn(
                &ProjectId("project-a".into()),
                &session.status.session_id,
                &session.connection_token,
                &session.routing_token,
                &TurnIntent {
                    idempotency_key: "ub-turn".into(),
                    intent_fingerprint: ObjectHash::from_canonical_bytes(b"ub-turn"),
                    purpose: crate::domain::TurnPurpose::Ordinary,
                    requested_effects: vec![EffectClass::MutateLocal],
                    resource_intents: vec![crate::domain::ResourceSubject::Path {
                        project_id: ProjectId("project-a".into()),
                        segments: vec!["src".into(), "lib.rs".into()],
                        coverage: crate::domain::ResourceCoverage::Exact,
                    }],
                },
                now + TimeDelta::seconds(3),
            )
            .unwrap()
        else {
            panic!("the mutation turn must grant on the resolved opener");
        };
        drop(store);
        let delivery_tokens = grant
            .delivery
            .iter()
            .map(|delivery| delivery.page.delivery_token.clone())
            .collect::<Vec<_>>();
        let mut unresolved = SqliteStore::open_unresolved(&database).expect("unresolved opener");
        assert!(matches!(
            unresolved.begin_control_turn(
                &ProjectId("project-a".into()),
                &session.status.session_id,
                &session.connection_token,
                &session.routing_token,
                &grant.grant_id,
                &delivery_tokens,
                "ub-begin",
                now + TimeDelta::seconds(4),
            ),
            Err(StoreError::HostPathIdentityUnresolved)
        ));
    }

    #[test]
    fn older_schema_backups_are_named_and_migrated_on_the_staged_copy() {
        let directory = tempfile::tempdir().expect("temp directory");
        let database = directory.path().join("live.sqlite3");
        let store = SqliteStore::open(&database).expect("live store");
        let backup = directory.path().join("older.sqlite3");
        store.backup_to(&backup).expect("backup");
        drop(store);
        // Age the backup: an older work schema version makes the current
        // schema incomplete, so an ordinary open would have to migrate.
        let aged = Connection::open(&backup).expect("open backup for aging");
        aged.execute(
            "UPDATE work_schema_metadata SET schema_version = 9 WHERE singleton = 1",
            [],
        )
        .expect("age the backup");
        aged.execute_batch("PRAGMA wal_checkpoint(TRUNCATE);")
            .expect("checkpoint the aged backup");
        drop(aged);
        for sidecar in store_sidecars(&backup) {
            let _ = std::fs::remove_file(sidecar);
        }
        let before = std::fs::read(&backup).expect("aged bytes");
        assert!(matches!(
            SqliteStore::verify_backup(&backup),
            Err(StoreError::BackupNeedsMigration {
                from_version: Some(9)
            })
        ));
        assert_eq!(
            std::fs::read(&backup).expect("bytes after verification"),
            before,
            "verification must not touch the backup"
        );

        let staged = directory.path().join("staged.sqlite3");
        std::fs::copy(&backup, &staged).expect("stage a copy");
        let (manifest, migrated_from) =
            SqliteStore::prepare_restore_copy(&staged).expect("migrate the staged copy");
        assert_eq!(migrated_from, Some(9));
        assert!(manifest.checked_objects > 0);
        let current = SqliteStore::verify_backup(&staged).expect("the staged copy is current");
        assert_eq!(current.file_sha256, manifest.file_sha256);
        assert_eq!(
            std::fs::read(&backup).expect("backup bytes after restore"),
            before,
            "the backup itself is never migrated"
        );
    }

    #[test]
    fn current_store_reopens_through_a_read_only_connection() {
        let directory = tempfile::tempdir().expect("temporary store directory");
        let database = directory.path().join("engram.db");
        drop(SqliteStore::open(&database).expect("initialize current store"));
        let connection =
            Connection::open_with_flags(&database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .expect("open read-only SQLite connection");
        SqliteStore::from_connection(connection, Some(HostPathPolicy::host_default()), None)
            .expect("current schema opens without a write transaction");
    }

    #[test]
    fn replayless_policy_schema_migrates_once_then_requires_its_receipt_table() {
        let directory = tempfile::tempdir().expect("temporary store directory");
        let database = directory.path().join("policy-replay-migration.db");
        drop(SqliteStore::open(&database).expect("initialize current store"));
        let fixture = Connection::open(&database).expect("open schema-three fixture");
        fixture
            .execute(
                "UPDATE control_policy_state SET schema_version = ?1 WHERE singleton = 1",
                [LEGACY_REPLAYLESS_CONTROL_POLICY_STATE_SCHEMA_VERSION],
            )
            .expect("downgrade projection schema marker");
        fixture
            .execute_batch("DROP TABLE control_policy_operation_results;")
            .expect("remove future receipt table");
        drop(fixture);

        let migrated = SqliteStore::open(&database).expect("migrate schema three to four");
        assert_eq!(
            migrated
                .connection
                .query_row(
                    "SELECT schema_version FROM control_policy_state WHERE singleton = 1",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("read migrated schema"),
            CONTROL_POLICY_STATE_SCHEMA_VERSION
        );
        assert!(
            SqliteStore::sqlite_table_exists(
                &migrated.connection,
                "control_policy_operation_results"
            )
            .expect("inspect receipt table")
        );
        drop(migrated);

        let read_only =
            Connection::open_with_flags(&database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .expect("open migrated store read-only");
        drop(
            SqliteStore::from_connection(read_only, Some(HostPathPolicy::host_default()), None)
                .expect("current replay schema opens without a write transaction"),
        );

        let corrupt = Connection::open(&database).expect("open partial-current fixture");
        corrupt
            .execute_batch("DROP TABLE control_policy_operation_results;")
            .expect("remove required current receipt table");
        drop(corrupt);
        assert!(matches!(
            SqliteStore::open(&database),
            Err(StoreError::InvalidControlProjection(message))
                if message.contains("operation-result table is missing")
        ));
        let refused = Connection::open(&database).expect("inspect refused store");
        assert!(
            !SqliteStore::sqlite_table_exists(&refused, "control_policy_operation_results")
                .expect("confirm fail-before-DDL")
        );
    }

    #[test]
    fn warm_open_skips_the_writer_lock_but_a_needed_binding_escalates() {
        let directory = tempfile::tempdir().expect("temporary store directory");
        let database = directory.path().join("engram.db");
        drop(SqliteStore::open(&database).expect("initialize current store"));

        let mut blocker = Connection::open(&database).expect("open blocking connection");
        let blocking_transaction = blocker
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("hold the SQLite writer slot");
        let current = Connection::open(&database).expect("open current-store connection");
        drop(
            SqliteStore::from_connection_with_busy_timeout(
                current,
                Some(HostPathPolicy::host_default()),
                None,
                Duration::from_millis(25),
            )
            .expect("a current warm open remains read-only while another writer is active"),
        );
        blocking_transaction
            .rollback()
            .expect("release the SQLite writer slot");

        let repair = Connection::open(&database).expect("open repair fixture connection");
        repair
            .execute("DELETE FROM control_host_path_policy", [])
            .expect("remove the recoverable empty-store path binding");
        drop(repair);

        let mut blocker = Connection::open(&database).expect("reopen blocking connection");
        let blocking_transaction = blocker
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("hold the writer slot across the repair attempt");
        let candidate = Connection::open(&database).expect("open repair candidate connection");
        let result = SqliteStore::from_connection_with_busy_timeout(
            candidate,
            Some(HostPathPolicy::host_default()),
            None,
            Duration::from_millis(25),
        );
        let Err(StoreError::Sqlite(error)) = result else {
            panic!("a required path-policy binding must contend for the writer slot");
        };
        assert!(matches!(
            error.sqlite_error_code(),
            Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
        ));
        blocking_transaction
            .rollback()
            .expect("release the writer slot after the negative probe");
        drop(blocker);

        drop(SqliteStore::open(&database).expect("retry and persist the required binding"));
    }

    #[test]
    fn cold_schema_failure_after_ddl_rolls_back_every_control_table() {
        let directory = tempfile::tempdir().expect("temporary store directory");
        let database = directory.path().join("interrupted-cold-schema.db");
        FAIL_COLD_SCHEMA_AFTER_DDL.set(true);
        assert!(matches!(
            SqliteStore::open(&database),
            Err(StoreError::InvalidControlProjection(reason))
                if reason.contains("injected cold-schema failure")
        ));

        let raw = Connection::open(&database).expect("inspect rolled-back cold schema");
        for table in ["objects", "control_policy_state", "control_policy_versions"] {
            assert!(
                !SqliteStore::sqlite_table_exists(&raw, table).expect("inspect rolled-back table")
            );
        }
        drop(raw);
        drop(SqliteStore::open(&database).expect("retry cold bootstrap"));
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one restart fixture proves both policy setter receipts across conflicts, no-ops, later heads, and integrity scanning"
    )]
    fn policy_admin_receipts_replay_after_restart_and_later_policy_heads() {
        let directory = tempfile::tempdir().expect("temporary store directory");
        let database = directory.path().join("policy-operation-replay.db");
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let mut store = open_with_assurance(&database, ControlAssurance::Advisory)
            .expect("initialize advisory policy");
        let initial = store.control_diagnostics().expect("initial policy");
        let changed = store
            .set_required_control_assurance(
                ControlAssurance::TurnGated,
                &actor("durable-policy-admin"),
                "require durable turn mediation",
                "durable-assurance-update",
                Some(&initial.active_policy),
                now,
                &DevelopmentNoopRedactor,
            )
            .expect("activate policy");
        drop(store);

        let mut store = SqliteStore::open(&database).expect("reopen after uncertain response");
        let replay = store
            .set_required_control_assurance(
                ControlAssurance::TurnGated,
                &actor("durable-policy-admin"),
                "require durable turn mediation",
                "durable-assurance-update",
                Some(&initial.active_policy),
                now + TimeDelta::minutes(10),
                &DevelopmentNoopRedactor,
            )
            .expect("replay committed assurance receipt");
        assert_eq!(replay, changed);
        assert!(matches!(
            store.set_required_control_assurance(
                ControlAssurance::TurnGated,
                &actor("durable-policy-admin"),
                "different intent under the same key",
                "durable-assurance-update",
                Some(&initial.active_policy),
                now + TimeDelta::minutes(11),
                &DevelopmentNoopRedactor,
            ),
            Err(StoreError::ControlOperationIdempotencyConflict { .. })
        ));

        let no_op = store
            .set_required_control_assurance(
                ControlAssurance::TurnGated,
                &actor("durable-policy-admin"),
                "record an exact no-op receipt",
                "durable-assurance-noop",
                Some(&changed.active_policy),
                now + TimeDelta::minutes(12),
                &DevelopmentNoopRedactor,
            )
            .expect("record no-op receipt");
        assert!(!no_op.changed);
        drop(store);

        let mut store = SqliteStore::open(&database).expect("reopen no-op receipt");
        let no_op_replay = store
            .set_required_control_assurance(
                ControlAssurance::TurnGated,
                &actor("durable-policy-admin"),
                "record an exact no-op receipt",
                "durable-assurance-noop",
                Some(&changed.active_policy),
                now + TimeDelta::minutes(13),
                &DevelopmentNoopRedactor,
            )
            .expect("replay no-op receipt");
        assert_eq!(no_op_replay, no_op);

        let empty_rules = ObligationRuleSet {
            schema_version: OBLIGATION_RULE_SET_SCHEMA_VERSION,
            rules: Vec::new(),
        };
        let rule_changed = store
            .set_obligation_rule_set(
                &empty_rules,
                &actor("durable-rule-admin"),
                "select the empty obligation rule set",
                "durable-rule-update",
                Some(&changed.active_policy),
                now + TimeDelta::minutes(14),
                &DevelopmentNoopRedactor,
            )
            .expect("activate rule set");
        drop(store);

        let mut store = SqliteStore::open(&database).expect("reopen rule receipt");
        let rule_replay = store
            .set_obligation_rule_set(
                &empty_rules,
                &actor("durable-rule-admin"),
                "select the empty obligation rule set",
                "durable-rule-update",
                Some(&changed.active_policy),
                now + TimeDelta::minutes(15),
                &DevelopmentNoopRedactor,
            )
            .expect("replay rule-set receipt");
        assert_eq!(rule_replay, rule_changed);
        let later = store
            .set_required_control_assurance(
                ControlAssurance::Advisory,
                &actor("later-policy-admin"),
                "advance beyond the stored rule receipt",
                "later-assurance-update",
                Some(&rule_changed.active_policy),
                now + TimeDelta::minutes(16),
                &DevelopmentNoopRedactor,
            )
            .expect("activate later policy head");
        assert_eq!(later.policy_epoch.0, rule_changed.policy_epoch.0 + 1);
        let replay_after_later_head = store
            .set_obligation_rule_set(
                &empty_rules,
                &actor("durable-rule-admin"),
                "select the empty obligation rule set",
                "durable-rule-update",
                Some(&changed.active_policy),
                now + TimeDelta::minutes(17),
                &DevelopmentNoopRedactor,
            )
            .expect("replay rule receipt after later head");
        assert_eq!(replay_after_later_head, rule_changed);
        assert!(matches!(
            store.set_obligation_rule_set(
                &empty_rules,
                &actor("durable-rule-admin"),
                "different rule intent under the same key",
                "durable-rule-update",
                Some(&changed.active_policy),
                now + TimeDelta::minutes(18),
                &DevelopmentNoopRedactor,
            ),
            Err(StoreError::ControlOperationIdempotencyConflict { .. })
        ));
        assert!(
            store
                .verify_all()
                .expect("verify durable receipts")
                .is_healthy()
        );
        let corrupted_sequence = store
            .connection
            .query_row(
                "SELECT sequence FROM control_policy_operation_results
                 WHERE operation = 'set_required_assurance'
                   AND idempotency_key = 'durable-assurance-update'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("locate durable receipt");
        store
            .connection
            .execute(
                "UPDATE control_policy_operation_results SET result_json = ?1
                 WHERE sequence = ?2",
                params![b"{}".as_slice(), corrupted_sequence],
            )
            .expect("corrupt durable receipt projection");
        assert!(
            store
                .verify_all()
                .expect("scan corrupt durable receipt")
                .invalid_control_records
                .contains(&format!("control_policy_operation:{corrupted_sequence}"))
        );
    }

    #[test]
    fn failed_policy_receipt_insert_rolls_back_the_policy_activation() {
        let directory = tempfile::tempdir().expect("temporary store directory");
        let database = directory.path().join("policy-operation-rollback.db");
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let mut store = SqliteStore::open(&database).expect("initialize store");
        let initial = store.control_diagnostics().expect("initial policy");
        store
            .connection
            .execute_batch(
                "CREATE TRIGGER fail_policy_operation_receipt
                 BEFORE INSERT ON control_policy_operation_results
                 BEGIN
                     SELECT RAISE(ABORT, 'injected policy receipt failure');
                 END;",
            )
            .expect("install receipt failure trigger");
        assert!(matches!(
            store.set_required_control_assurance(
                ControlAssurance::Advisory,
                &actor("rollback-policy-admin"),
                "prove receipt and activation share one transaction",
                "rollback-policy-update",
                Some(&initial.active_policy),
                now,
                &DevelopmentNoopRedactor,
            ),
            Err(StoreError::Sqlite(_))
        ));
        store
            .connection
            .execute_batch("DROP TRIGGER fail_policy_operation_receipt;")
            .expect("drop receipt failure trigger");
        let after = store.control_diagnostics().expect("policy after rollback");
        assert_eq!(after.active_policy, initial.active_policy);
        assert_eq!(after.policy_epoch, initial.policy_epoch);
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM control_policy_operation_results",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("count policy receipts"),
            0
        );
        store
            .connection
            .execute_batch(
                "CREATE TRIGGER fail_policy_operation_receipt
                 BEFORE INSERT ON control_policy_operation_results
                 BEGIN
                     SELECT RAISE(ABORT, 'injected rule receipt failure');
                 END;",
            )
            .expect("reinstall receipt failure trigger");
        let empty_rules = ObligationRuleSet {
            schema_version: OBLIGATION_RULE_SET_SCHEMA_VERSION,
            rules: Vec::new(),
        };
        assert!(matches!(
            store.set_obligation_rule_set(
                &empty_rules,
                &actor("rollback-rule-admin"),
                "prove rule receipt and activation share one transaction",
                "rollback-rule-update",
                Some(&initial.active_policy),
                now + TimeDelta::seconds(1),
                &DevelopmentNoopRedactor,
            ),
            Err(StoreError::Sqlite(_))
        ));
        store
            .connection
            .execute_batch("DROP TRIGGER fail_policy_operation_receipt;")
            .expect("drop rule receipt failure trigger");
        let after_rule = store
            .control_diagnostics()
            .expect("policy after rule rollback");
        assert_eq!(after_rule.active_policy, initial.active_policy);
        assert_eq!(after_rule.obligation_rule_set, initial.obligation_rule_set);
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM control_policy_operation_results",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("count rule receipts"),
            0
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the legacy fixture proves both canonical epochs, their distinct authorities, and the next explicit operator change"
    )]
    fn unused_legacy_policy_is_preserved_then_builtin_envelope_upgrade_is_attributed() {
        let directory = tempfile::tempdir().expect("temporary store directory");
        let database = directory.path().join("unused-legacy.db");
        let legacy = Connection::open(&database).expect("open unused legacy fixture");
        legacy
            .execute_batch(
                "CREATE TABLE control_policy_state (
                     singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
                     schema_version INTEGER NOT NULL,
                     policy_epoch INTEGER NOT NULL,
                     required_assurance TEXT NOT NULL,
                     supported_effects_json TEXT NOT NULL,
                     grant_ttl_seconds INTEGER NOT NULL,
                     policy_hash TEXT
                 ) STRICT;
                 INSERT INTO control_policy_state (
                     singleton, schema_version, policy_epoch,
                     required_assurance, supported_effects_json,
                     grant_ttl_seconds, policy_hash
                 ) VALUES (
                     1, 1, 1, 'turn_gated',
                     '[\"observe\",\"communicate\",\"mutate_local\"]',
                     30, NULL
                 );",
            )
            .expect("seed valid unused legacy policy");
        drop(legacy);

        assert!(matches!(
            open_with_assurance(&database, ControlAssurance::Advisory),
            Err(StoreError::InvalidControlProjection(reason))
                if reason.contains("initial assurance cannot replace")
        ));
        let refused = Connection::open(&database).expect("inspect refused legacy");
        assert_eq!(
            refused
                .query_row(
                    "SELECT schema_version FROM control_policy_state WHERE singleton = 1",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("legacy selector remains unchanged"),
            1
        );
        assert!(
            !SqliteStore::sqlite_table_exists(&refused, "control_policy_versions")
                .expect("mismatching init ran no DDL")
        );
        drop(refused);

        let mut migrated = SqliteStore::open(&database).expect("migrate unused legacy policy");
        let upgraded = migrated
            .control_diagnostics()
            .expect("migrated diagnostics");
        assert_eq!(upgraded.policy_epoch, ProjectPolicyEpoch(2));
        assert_eq!(upgraded.required_assurance, ControlAssurance::TurnGated);
        let policy_hashes = {
            let mut statement = migrated
                .connection
                .prepare("SELECT policy_hash FROM control_policy_versions ORDER BY policy_epoch")
                .expect("prepare policy history query");
            statement
                .query_map([], |row| row.get::<_, String>(0))
                .expect("query policy history")
                .collect::<Result<Vec<_>, _>>()
                .expect("collect policy history")
        };
        assert_eq!(policy_hashes.len(), 2);
        let epoch_one_hash = ObjectHash::from_stored(policy_hashes[0].clone()).expect("epoch one");
        let epoch_two_hash = ObjectHash::from_stored(policy_hashes[1].clone()).expect("epoch two");
        let epoch_one_policy: ControlPolicy = migrated
            .get(&epoch_one_hash)
            .expect("read epoch-one policy")
            .expect("epoch-one policy object");
        let epoch_two_policy: ControlPolicy = migrated
            .get(&epoch_two_hash)
            .expect("read epoch-two policy")
            .expect("epoch-two policy object");
        assert_eq!(
            epoch_one_policy.supported_effects,
            SqliteStore::legacy_v1_control_effects()
        );
        assert_eq!(
            epoch_two_policy.supported_effects,
            SqliteStore::builtin_control_effects()
        );
        let epoch_one_authority: ProjectPolicyAuthorityDecision = migrated
            .get(&epoch_one_policy.authority)
            .expect("read epoch-one authority")
            .expect("epoch-one authority object");
        let epoch_two_authority: ProjectPolicyAuthorityDecision = migrated
            .get(&epoch_two_policy.authority)
            .expect("read epoch-two authority")
            .expect("epoch-two authority object");
        assert_eq!(
            epoch_one_authority.operation,
            ProjectPolicyOperation::SetRequiredAssurance
        );
        assert_eq!(
            epoch_two_authority.operation,
            ProjectPolicyOperation::UpgradeBuiltinEnvelope
        );
        assert_eq!(
            epoch_two_authority.authorized_by.actor_id,
            "engram:migration"
        );
        let changed = migrated
            .set_required_control_assurance(
                ControlAssurance::Advisory,
                &actor("legacy-policy-admin"),
                "explicitly lower the migrated requirement",
                "legacy-policy-lower",
                Some(&upgraded.active_policy),
                Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
                &DevelopmentNoopRedactor,
            )
            .expect("activate attributed epoch three");
        assert_eq!(changed.policy_epoch, ProjectPolicyEpoch(3));
        assert_eq!(changed.required_assurance, ControlAssurance::Advisory);
        let authority: ProjectPolicyAuthorityDecision = migrated
            .get(&changed.authority)
            .expect("read epoch-three authority")
            .expect("epoch-three authority object");
        assert_eq!(authority.authorized_by.actor_id, "legacy-policy-admin");
        assert_eq!(
            authority.reason,
            "explicitly lower the migrated requirement"
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the schema-two fixture must install a recognized historical envelope, reopen it, and exercise the persisted session fence"
    )]
    fn schema_two_legacy_envelope_upgrades_and_fences_a_bound_session_once() {
        let directory = tempfile::tempdir().expect("temporary store directory");
        let database = directory.path().join("schema-two-legacy-envelope.db");
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let mut store = SqliteStore::open(&database).expect("initialize current store");
        let binding = bind_control_for(
            &mut store,
            "legacy-envelope-session",
            "bind-legacy-envelope",
            &[EffectClass::Observe],
            now,
        );
        let initial = store.control_diagnostics().expect("initial diagnostics");
        let mut legacy_policy: ControlPolicy = store
            .get(&initial.active_policy)
            .expect("read current policy")
            .expect("current policy object");
        legacy_policy.supported_effects = SqliteStore::legacy_v1_control_effects();
        let legacy_object = CanonicalObject::freeze(&legacy_policy).expect("freeze legacy policy");
        let transaction = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("begin legacy-envelope replacement");
        SqliteStore::insert_object(&transaction, "control_policy", &legacy_object)
            .expect("insert legacy-envelope policy");
        transaction
            .execute("DELETE FROM control_policy_versions", [])
            .expect("replace current version row");
        transaction
            .execute(
                "INSERT INTO control_policy_versions (
                     policy_hash, policy_epoch, authority_hash, policy_json
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![
                    legacy_object.hash().as_str(),
                    legacy_policy.policy_epoch.0,
                    legacy_policy.authority.as_str(),
                    legacy_object.bytes(),
                ],
            )
            .expect("install legacy-envelope version row");
        transaction
            .execute(
                "UPDATE control_policy_state SET
                     supported_effects_json = ?1, policy_hash = ?2
                 WHERE singleton = 1",
                params![
                    serde_json::to_string(&legacy_policy.supported_effects)
                        .expect("serialize legacy envelope"),
                    legacy_object.hash().as_str(),
                ],
            )
            .expect("select legacy-envelope head");
        transaction
            .commit()
            .expect("commit legacy-envelope fixture");
        drop(store);

        let mut reopened = SqliteStore::open(&database).expect("upgrade recognized envelope");
        let upgraded = reopened
            .control_diagnostics()
            .expect("upgraded diagnostics");
        assert_eq!(upgraded.policy_epoch, ProjectPolicyEpoch(2));
        assert_eq!(
            upgraded.supported_effects,
            SqliteStore::builtin_control_effects()
        );
        let upgraded_policy: ControlPolicy = reopened
            .get(&upgraded.active_policy)
            .expect("read upgraded policy")
            .expect("upgraded policy object");
        let authority: ProjectPolicyAuthorityDecision = reopened
            .get(&upgraded_policy.authority)
            .expect("read upgrade authority")
            .expect("upgrade authority object");
        assert_eq!(
            authority.operation,
            ProjectPolicyOperation::UpgradeBuiltinEnvelope
        );

        let refused = reopened
            .evaluate_control_turn(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &TurnIntent {
                    idempotency_key: "legacy-envelope-stale-epoch".into(),
                    intent_fingerprint: ObjectHash::from_canonical_bytes(
                        b"legacy-envelope-stale-epoch",
                    ),
                    purpose: TurnPurpose::Ordinary,
                    requested_effects: vec![EffectClass::Observe],
                    resource_intents: Vec::new(),
                },
                now + TimeDelta::seconds(1),
            )
            .expect("evaluate stale policy epoch");
        assert!(matches!(
            refused,
            ControlTurnDecision::Refuse { directive }
                if directive.code == ControlRefusalCode::PolicyEpochChanged
        ));
        assert!(matches!(
            reopened
                .evaluate_control_turn(
                    &ProjectId("project-a".into()),
                    &binding.status.session_id,
                    &binding.connection_token,
                    &binding.routing_token,
                    &TurnIntent {
                        idempotency_key: "legacy-envelope-adopted-epoch".into(),
                        intent_fingerprint: ObjectHash::from_canonical_bytes(
                            b"legacy-envelope-adopted-epoch",
                        ),
                        purpose: TurnPurpose::Ordinary,
                        requested_effects: vec![EffectClass::Observe],
                        resource_intents: Vec::new(),
                    },
                    now + TimeDelta::seconds(2),
                )
                .expect("evaluate adopted policy epoch"),
            ControlTurnDecision::Grant { .. }
        ));
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one restart fixture keeps bootstrap attribution, CAS, corruption, and reopen behavior on the same immutable chain"
    )]
    fn control_policy_versions_are_canonical_idempotent_and_restart_safe() {
        let directory = tempfile::tempdir().expect("temporary store directory");
        let database = directory.path().join("engram.db");
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let mut store = open_with_assurance(&database, ControlAssurance::Advisory)
            .expect("initialize advisory policy");
        let initial = store.control_diagnostics().expect("initial diagnostics");
        assert_eq!(initial.required_assurance, ControlAssurance::Advisory);
        assert_eq!(initial.policy_epoch, ProjectPolicyEpoch(1));

        let initial_policy: ControlPolicy = store
            .get(&initial.active_policy)
            .expect("read initial policy")
            .expect("initial policy object");
        assert_eq!(initial_policy.previous_policy, None);
        let initial_authority: ProjectPolicyAuthorityDecision = store
            .get(&initial_policy.authority)
            .expect("read initial authority")
            .expect("initial authority object");
        assert_eq!(
            initial_authority.authorized_by.actor_id,
            "bootstrap-policy-admin"
        );
        assert_eq!(initial_authority.reason, "select the test bootstrap policy");

        let changed = store
            .set_required_control_assurance(
                ControlAssurance::TurnGated,
                &actor("policy-admin"),
                "require host turn mediation",
                "policy-turn-gated",
                Some(&initial.active_policy),
                now,
                &DevelopmentNoopRedactor,
            )
            .expect("activate turn-gated policy");
        assert!(changed.changed);
        assert_eq!(changed.previous_policy, Some(initial.active_policy.clone()));
        assert_eq!(changed.policy_epoch, ProjectPolicyEpoch(2));
        assert_eq!(changed.required_assurance, ControlAssurance::TurnGated);

        let replay = store
            .set_required_control_assurance(
                ControlAssurance::TurnGated,
                &actor("policy-admin"),
                "idempotent replay",
                "policy-turn-gated-noop",
                Some(&changed.active_policy),
                now + TimeDelta::seconds(1),
                &DevelopmentNoopRedactor,
            )
            .expect("reapply active policy");
        assert!(!replay.changed);
        assert_eq!(replay.active_policy, changed.active_policy);
        assert_eq!(replay.policy_epoch, changed.policy_epoch);
        assert!(matches!(
            store.set_required_control_assurance(
                ControlAssurance::ActionGated,
                &actor("policy-admin"),
                "stale compare and swap",
                "policy-stale-cas",
                Some(&initial.active_policy),
                now + TimeDelta::seconds(2),
                &DevelopmentNoopRedactor,
            ),
            Err(StoreError::ControlPolicyConflict { .. })
        ));
        assert!(
            store
                .verify_all()
                .expect("verify policy history")
                .is_healthy()
        );
        drop(store);

        let reopened = SqliteStore::open(&database).expect("reopen configured store");
        let diagnostics = reopened
            .control_diagnostics()
            .expect("reopened diagnostics");
        assert_eq!(diagnostics.active_policy, changed.active_policy);
        assert_eq!(diagnostics.policy_epoch, ProjectPolicyEpoch(2));
        assert_eq!(diagnostics.required_assurance, ControlAssurance::TurnGated);
        drop(reopened);
        assert!(matches!(
            open_with_assurance(&database, ControlAssurance::Advisory),
            Err(StoreError::InvalidControlProjection(_))
        ));

        let corrupted_store = SqliteStore::open(&database).expect("reopen corruption fixture");
        corrupted_store
            .connection
            .execute(
                "UPDATE control_policy_versions SET policy_json = ?1
                 WHERE policy_hash = ?2",
                params![b"{}".as_slice(), changed.active_policy.as_str()],
            )
            .expect("corrupt active policy projection");
        let corrupted = corrupted_store.verify_all().expect("scan corrupt policy");
        assert!(
            corrupted
                .invalid_control_records
                .contains(&"control_policy_state:active".into())
        );
        assert!(
            corrupted
                .invalid_control_records
                .contains(&format!("control_policy_version:{}", changed.active_policy))
        );
        drop(corrupted_store);
        assert!(SqliteStore::open(&database).is_err());
    }

    #[test]
    fn obligation_rule_set_activation_is_canonical_idempotent_and_restart_safe() {
        let directory = tempfile::tempdir().expect("temporary store directory");
        let database = directory.path().join("obligation-rules.db");
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let mut store = SqliteStore::open(&database).expect("initialize store");
        let initial = store.control_diagnostics().expect("initial diagnostics");
        let stock: ObligationRuleSet = store
            .get(&initial.obligation_rule_set)
            .expect("read stock rule set")
            .expect("stock rule set object");
        assert_eq!(stock, crate::control::builtin_obligation_rule_set());

        let empty = ObligationRuleSet {
            schema_version: OBLIGATION_RULE_SET_SCHEMA_VERSION,
            rules: Vec::new(),
        };
        let changed = store
            .set_obligation_rule_set(
                &empty,
                &actor("rule-policy-admin"),
                "disable future obligation triggers",
                "rule-set-empty",
                Some(&initial.active_policy),
                now,
                &DevelopmentNoopRedactor,
            )
            .expect("activate empty rule set");
        assert!(changed.changed);
        assert_eq!(changed.previous_rule_set, Some(initial.obligation_rule_set));
        assert_eq!(changed.policy_epoch, ProjectPolicyEpoch(2));
        let active = store.control_diagnostics().expect("changed diagnostics");
        assert_eq!(active.obligation_rule_set, changed.obligation_rule_set);

        let replay = store
            .set_obligation_rule_set(
                &empty,
                &actor("rule-policy-admin"),
                "exact semantic replay",
                "rule-set-empty-noop",
                Some(&changed.active_policy),
                now + TimeDelta::seconds(1),
                &DevelopmentNoopRedactor,
            )
            .expect("reapply selected rule set");
        assert!(!replay.changed);
        assert_eq!(replay.active_policy, changed.active_policy);
        drop(store);
        let mut store = SqliteStore::open(&database).expect("reopen rule-set no-op receipt");
        let replay_after_restart = store
            .set_obligation_rule_set(
                &empty,
                &actor("rule-policy-admin"),
                "exact semantic replay",
                "rule-set-empty-noop",
                Some(&changed.active_policy),
                now + TimeDelta::seconds(10),
                &DevelopmentNoopRedactor,
            )
            .expect("replay selected rule-set no-op after restart");
        assert_eq!(replay_after_restart, replay);
        assert!(matches!(
            store.set_obligation_rule_set(
                &ObligationRuleSet {
                    schema_version: OBLIGATION_RULE_SET_SCHEMA_VERSION + 1,
                    rules: Vec::new(),
                },
                &actor("rule-policy-admin"),
                "unknown schema must fail closed",
                "rule-set-invalid-schema",
                None,
                now + TimeDelta::seconds(2),
                &DevelopmentNoopRedactor,
            ),
            Err(StoreError::InvalidControlProjection(_))
        ));
        assert!(
            store
                .verify_all()
                .expect("verify rule-set history")
                .is_healthy()
        );
        drop(store);

        let reopened = SqliteStore::open(&database).expect("reopen selected rule set");
        let diagnostics = reopened
            .control_diagnostics()
            .expect("reopened diagnostics");
        assert_eq!(diagnostics.active_policy, changed.active_policy);
        assert_eq!(diagnostics.obligation_rule_set, changed.obligation_rule_set);
        let selected: ObligationRuleSet = reopened
            .get(&changed.obligation_rule_set)
            .expect("read selected rule set")
            .expect("selected rule set object");
        assert_eq!(selected, empty);
    }

    #[test]
    fn schema_two_policy_without_rule_selection_upgrades_once() {
        let directory = tempfile::tempdir().expect("temporary store directory");
        let database = directory.path().join("schema-two-obligation-rules.db");
        let store = SqliteStore::open(&database).expect("initialize current store");
        let initial = store.control_diagnostics().expect("initial diagnostics");
        let mut legacy_policy: ControlPolicy = store
            .get(&initial.active_policy)
            .expect("read initial policy")
            .expect("initial policy object");
        let mut legacy_authority: ProjectPolicyAuthorityDecision = store
            .get(&legacy_policy.authority)
            .expect("read initial authority")
            .expect("initial authority object");
        legacy_authority.obligation_rule_set = None;
        let authority_object =
            CanonicalObject::freeze(&legacy_authority).expect("freeze schema-two authority");
        legacy_policy.obligation_rule_set = None;
        legacy_policy.authority = authority_object.hash().clone();
        let policy_object =
            CanonicalObject::freeze(&legacy_policy).expect("freeze schema-two policy");
        let transaction = store
            .connection
            .unchecked_transaction()
            .expect("schema-two fixture transaction");
        SqliteStore::insert_object(
            &transaction,
            "project_policy_authority_decision",
            &authority_object,
        )
        .expect("insert schema-two authority");
        SqliteStore::insert_object(&transaction, "control_policy", &policy_object)
            .expect("insert schema-two policy");
        transaction
            .execute("DELETE FROM control_policy_versions", [])
            .expect("replace policy history");
        transaction
            .execute(
                "INSERT INTO control_policy_versions (
                     policy_hash, policy_epoch, authority_hash, policy_json
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![
                    policy_object.hash().as_str(),
                    legacy_policy.policy_epoch.0,
                    authority_object.hash().as_str(),
                    policy_object.bytes(),
                ],
            )
            .expect("insert schema-two version");
        transaction
            .execute(
                "UPDATE control_policy_state SET schema_version = ?1, policy_hash = ?2",
                params![
                    LEGACY_VERSIONED_CONTROL_POLICY_STATE_SCHEMA_VERSION,
                    policy_object.hash().as_str()
                ],
            )
            .expect("select schema-two policy");
        transaction.commit().expect("commit schema-two fixture");
        drop(store);

        let reopened = SqliteStore::open(&database).expect("upgrade schema-two policy");
        let upgraded = reopened
            .control_diagnostics()
            .expect("upgraded diagnostics");
        assert_eq!(upgraded.policy_epoch, ProjectPolicyEpoch(2));
        let upgraded_policy: ControlPolicy = reopened
            .get(&upgraded.active_policy)
            .expect("read upgraded policy")
            .expect("upgraded policy object");
        assert_eq!(
            upgraded_policy.obligation_rule_set.as_ref(),
            Some(&upgraded.obligation_rule_set)
        );
        let upgraded_authority: ProjectPolicyAuthorityDecision = reopened
            .get(&upgraded_policy.authority)
            .expect("read upgrade authority")
            .expect("upgrade authority object");
        assert_eq!(
            upgraded_authority.operation,
            ProjectPolicyOperation::UpgradeBuiltinObligationRules
        );
        drop(reopened);
        assert_eq!(
            SqliteStore::open(&database)
                .expect("idempotent upgraded reopen")
                .control_diagnostics()
                .expect("idempotent diagnostics")
                .policy_epoch,
            ProjectPolicyEpoch(2)
        );
    }

    #[test]
    fn live_control_policy_load_is_bounded_independently_of_history_depth() {
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let mut store = SqliteStore::open_in_memory().expect("store");
        let mut active = store
            .control_diagnostics()
            .expect("initial diagnostics")
            .active_policy;
        for epoch in 2_i64..=51 {
            let required_assurance = if epoch % 2 == 0 {
                ControlAssurance::Advisory
            } else {
                ControlAssurance::TurnGated
            };
            let receipt = store
                .set_required_control_assurance(
                    required_assurance,
                    &actor("policy-load-admin"),
                    &format!("install policy epoch {epoch}"),
                    &format!("policy-load-{epoch}"),
                    Some(&active),
                    now + TimeDelta::milliseconds(epoch),
                    &DevelopmentNoopRedactor,
                )
                .expect("extend policy history");
            assert_eq!(receipt.policy_epoch, ProjectPolicyEpoch(epoch));
            active = receipt.active_policy;
        }

        reset_control_policy_version_load_count();
        let binding = bind_control_for(
            &mut store,
            "bounded-policy-session",
            "bind-bounded-policy",
            &[EffectClass::Observe],
            now + TimeDelta::seconds(1),
        );
        assert_eq!(control_policy_version_load_count(), 1);

        reset_control_policy_version_load_count();
        let decision = store
            .evaluate_control_turn(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &TurnIntent {
                    idempotency_key: "bounded-policy-turn".into(),
                    intent_fingerprint: ObjectHash::from_canonical_bytes(b"bounded-policy-turn"),
                    purpose: TurnPurpose::Ordinary,
                    requested_effects: vec![EffectClass::Observe],
                    resource_intents: Vec::new(),
                },
                now + TimeDelta::seconds(2),
            )
            .expect("evaluate through bounded policy loader");
        assert_eq!(control_policy_version_load_count(), 1);
        let ControlTurnDecision::Grant { grant } = decision else {
            panic!("bounded policy fixture must grant");
        };
        let delivery_tokens = grant
            .delivery
            .iter()
            .map(|delivery| delivery.page.delivery_token.clone())
            .collect::<Vec<_>>();

        reset_control_policy_version_load_count();
        assert!(matches!(
            store
                .begin_control_turn(
                    &ProjectId("project-a".into()),
                    &binding.status.session_id,
                    &binding.connection_token,
                    &binding.routing_token,
                    &grant.grant_id,
                    &delivery_tokens,
                    "begin-bounded-policy-turn",
                    now + TimeDelta::seconds(3),
                )
                .expect("begin through bounded policy loader"),
            ControlTurnBeginDecision::Begin { .. }
        ));
        assert_eq!(control_policy_version_load_count(), 1);
    }

    #[test]
    fn established_store_missing_policy_state_refuses_without_bootstrap() {
        let directory = tempfile::tempdir().expect("temporary store directory");

        let missing_row_database = directory.path().join("missing-policy-row.db");
        let missing_row =
            SqliteStore::open(&missing_row_database).expect("initialize missing-row fixture");
        let missing_row_objects = missing_row
            .connection
            .query_row("SELECT COUNT(*) FROM objects", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("count missing-row objects");
        missing_row
            .connection
            .execute("DELETE FROM control_policy_state", [])
            .expect("remove policy singleton");
        drop(missing_row);
        assert!(matches!(
            SqliteStore::open(&missing_row_database),
            Err(StoreError::InvalidControlProjection(reason))
                if reason.contains("singleton")
        ));
        let missing_row_raw =
            Connection::open(&missing_row_database).expect("inspect missing-row refusal");
        assert_eq!(
            missing_row_raw
                .query_row("SELECT COUNT(*) FROM control_policy_state", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("singleton remains absent"),
            0
        );
        assert_eq!(
            missing_row_raw
                .query_row("SELECT COUNT(*) FROM objects", [], |row| row
                    .get::<_, i64>(0))
                .expect("objects remain unchanged"),
            missing_row_objects
        );

        let missing_table_database = directory.path().join("missing-policy-table.db");
        let missing_table =
            SqliteStore::open(&missing_table_database).expect("initialize missing-table fixture");
        let missing_table_objects = missing_table
            .connection
            .query_row("SELECT COUNT(*) FROM objects", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("count missing-table objects");
        missing_table
            .connection
            .execute("DROP TABLE control_policy_state", [])
            .expect("remove policy state table");
        drop(missing_table);
        assert!(matches!(
            SqliteStore::open(&missing_table_database),
            Err(StoreError::InvalidControlProjection(reason))
                if reason.contains("missing from an established store")
        ));
        let missing_table_raw =
            Connection::open(&missing_table_database).expect("inspect missing-table refusal");
        assert!(
            !missing_table_raw
                .query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM sqlite_master
                         WHERE type = 'table' AND name = 'control_policy_state'
                     )",
                    [],
                    |row| row.get::<_, bool>(0),
                )
                .expect("policy state table remains absent")
        );
        assert_eq!(
            missing_table_raw
                .query_row("SELECT COUNT(*) FROM objects", [], |row| row
                    .get::<_, i64>(0))
                .expect("objects remain unchanged"),
            missing_table_objects
        );
    }

    #[test]
    fn current_policy_with_null_selector_returns_a_typed_projection_error() {
        let store = SqliteStore::open_in_memory().expect("store");
        store
            .connection
            .execute(
                "UPDATE control_policy_state SET policy_hash = NULL WHERE singleton = 1",
                [],
            )
            .expect("clear active selector");

        assert!(matches!(
            SqliteStore::load_control_policy_head(&store.connection),
            Err(StoreError::InvalidControlProjection(reason))
                if reason.contains("no selected version")
        ));
    }

    #[test]
    fn partial_control_table_family_prevents_policy_rebootstrap() {
        let directory = tempfile::tempdir().expect("temporary store directory");
        let database = directory.path().join("ordinary-data.db");
        let mut store = SqliteStore::open(&database).expect("initialize established store");
        let ordinary = store
            .append(
                "example",
                &Example {
                    title: "ordinary durable object".into(),
                    body: "not control-policy evidence".into(),
                },
            )
            .expect("append ordinary canonical data");
        store
            .connection
            .execute_batch(
                "DROP TABLE control_policy_state;
                 DROP TABLE control_policy_versions;
                 DELETE FROM objects
                 WHERE object_kind IN (
                     'control_policy', 'project_policy_authority_decision',
                     'obligation_rule_set'
                 );",
            )
            .expect("remove every control-policy artifact");
        drop(store);

        assert!(matches!(
            SqliteStore::open(&database),
            Err(StoreError::InvalidControlProjection(reason))
                if reason.contains("missing from an established store")
        ));
        let raw = Connection::open(&database).expect("inspect refused established store");
        assert_eq!(
            raw.query_row("SELECT COUNT(*) FROM objects", [], |row| row
                .get::<_, i64>(0))
                .expect("ordinary object remains"),
            1
        );
        assert_eq!(
            raw.query_row(
                "SELECT object_kind FROM objects WHERE object_hash = ?1",
                [ordinary.hash().as_str()],
                |row| row.get::<_, String>(0),
            )
            .expect("ordinary object identity remains"),
            "example"
        );
        for table in ["control_policy_state", "control_policy_versions"] {
            assert!(
                !raw.query_row(
                    "SELECT EXISTS(
                         SELECT 1 FROM sqlite_master
                         WHERE type = 'table' AND name = ?1
                     )",
                    [table],
                    |row| row.get::<_, bool>(0),
                )
                .expect("policy table remains absent")
            );
        }
    }

    #[test]
    fn pre_control_plane_store_bootstraps_without_discarding_ordinary_objects() {
        let directory = tempfile::tempdir().expect("temporary store directory");
        let database = directory.path().join("pre-control-plane.db");
        let mut store = SqliteStore::open(&database).expect("initialize migration fixture");
        let ordinary = store
            .append(
                "example",
                &Example {
                    title: "pre-control durable object".into(),
                    body: "survives control-plane bootstrap".into(),
                },
            )
            .expect("append ordinary canonical data");
        store
            .connection
            .execute_batch(
                "DROP TABLE control_turn_grants;
                 DROP TABLE control_sessions;
                 DROP TABLE control_policy_operation_results;
                 DROP TABLE control_policy_versions;
                 DROP TABLE control_policy_state;
                 DELETE FROM objects
                 WHERE object_kind IN (
                     'control_policy', 'project_policy_authority_decision'
                 );",
            )
            .expect("simulate a pre-control-plane store");
        drop(store);

        let migrated = SqliteStore::open(&database).expect("bootstrap control plane");
        let diagnostics = migrated.control_diagnostics().expect("control diagnostics");
        assert_eq!(diagnostics.policy_epoch, ProjectPolicyEpoch(1));
        assert_eq!(diagnostics.required_assurance, ControlAssurance::TurnGated);
        let retained: Example = migrated
            .get(ordinary.hash())
            .expect("read ordinary object")
            .expect("ordinary object remains");
        assert_eq!(retained.title, "pre-control durable object");
    }

    #[test]
    fn active_policy_must_be_the_unique_maximal_history_head() {
        let directory = tempfile::tempdir().expect("temporary store directory");
        let database = directory.path().join("engram.db");
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let mut store = open_with_assurance(&database, ControlAssurance::Advisory)
            .expect("initialize policy history");
        let initial = store.control_diagnostics().expect("initial diagnostics");
        let initial_policy: ControlPolicy = store
            .get(&initial.active_policy)
            .expect("read initial policy")
            .expect("initial policy object");
        let changed = store
            .set_required_control_assurance(
                ControlAssurance::TurnGated,
                &actor("policy-admin"),
                "create a successor",
                "policy-successor-rollback",
                Some(&initial.active_policy),
                now,
                &DevelopmentNoopRedactor,
            )
            .expect("activate successor policy");
        let projected_assurance =
            enum_name(initial_policy.required_assurance).expect("serialize assurance");
        let projected_effects =
            serde_json::to_string(&initial_policy.supported_effects).expect("serialize effects");
        store
            .connection
            .execute(
                "UPDATE control_policy_state SET
                     policy_epoch = ?1, required_assurance = ?2,
                     supported_effects_json = ?3, grant_ttl_seconds = ?4,
                     policy_hash = ?5
                 WHERE singleton = 1",
                params![
                    initial_policy.policy_epoch.0,
                    projected_assurance,
                    projected_effects,
                    initial_policy.grant_ttl_seconds,
                    initial.active_policy.as_str(),
                ],
            )
            .expect("simulate selector rollback");

        let integrity = store.verify_all().expect("scan rolled-back selector");
        assert!(
            integrity
                .invalid_control_records
                .contains(&"control_policy_state:active".into())
        );
        assert!(
            integrity
                .invalid_control_records
                .contains(&format!("control_policy_version:{}", changed.active_policy))
        );
        drop(store);
        assert!(matches!(
            SqliteStore::open(&database),
            Err(StoreError::InvalidControlProjection(reason))
                if reason.contains("maximal history head")
        ));
    }

    #[test]
    fn missing_policy_rows_are_reported_as_integrity_records() {
        let version_store = SqliteStore::open_in_memory().expect("version fixture");
        let active = version_store
            .control_diagnostics()
            .expect("version diagnostics")
            .active_policy;
        version_store
            .connection
            .execute(
                "DELETE FROM control_policy_versions WHERE policy_hash = ?1",
                [active.as_str()],
            )
            .expect("delete projected version row");
        let report = version_store
            .verify_all()
            .expect("scan missing version row");
        assert!(
            report
                .invalid_control_records
                .contains(&"control_policy_state:active".into())
        );

        let object_store = SqliteStore::open_in_memory().expect("object fixture");
        let active = object_store
            .control_diagnostics()
            .expect("object diagnostics")
            .active_policy;
        object_store
            .connection
            .execute_batch("PRAGMA foreign_keys = OFF;")
            .expect("disable fixture foreign keys");
        object_store
            .connection
            .execute(
                "DELETE FROM objects WHERE object_hash = ?1",
                [active.as_str()],
            )
            .expect("delete canonical policy object");
        let report = object_store.verify_all().expect("scan missing object row");
        assert!(
            report
                .invalid_control_records
                .contains(&"control_policy_state:active".into())
        );
        assert!(
            report
                .invalid_control_records
                .contains(&format!("control_policy_version:{active}"))
        );
    }

    #[test]
    fn control_diagnostics_reuses_an_embedder_owned_snapshot() {
        let store = SqliteStore::open_in_memory().expect("store");
        let snapshot = store
            .connection
            .unchecked_transaction()
            .expect("open caller snapshot");
        let diagnostics = store
            .control_diagnostics()
            .expect("diagnostics reuse caller snapshot");
        assert_eq!(diagnostics.policy_epoch, ProjectPolicyEpoch(1));
        snapshot.rollback().expect("rollback caller snapshot");
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one mixed-schema fixture must rebind both historical authority and policy objects plus the active successor atomically"
    )]
    fn historical_policy_schema_is_decoupled_from_active_compatibility() {
        let directory = tempfile::tempdir().expect("temporary store directory");
        let database = directory.path().join("engram.db");
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let mut store = open_with_assurance(&database, ControlAssurance::Advisory)
            .expect("initialize policy history");
        let initial = store.control_diagnostics().expect("initial diagnostics");
        let mut historical_policy: ControlPolicy = store
            .get(&initial.active_policy)
            .expect("read initial policy")
            .expect("initial policy object");
        let mut historical_authority: ProjectPolicyAuthorityDecision = store
            .get(&historical_policy.authority)
            .expect("read initial authority")
            .expect("initial authority object");
        let changed = store
            .set_required_control_assurance(
                ControlAssurance::TurnGated,
                &actor("policy-admin"),
                "create current policy",
                "policy-current-forward-schema",
                Some(&initial.active_policy),
                now,
                &DevelopmentNoopRedactor,
            )
            .expect("activate current policy");
        let mut active_policy: ControlPolicy = store
            .get(&changed.active_policy)
            .expect("read active policy")
            .expect("active policy object");
        let mut unsupported_active = active_policy.clone();
        unsupported_active.schema_version = 2;
        assert!(SqliteStore::validate_active_control_policy(&unsupported_active).is_err());
        let mut active_authority: ProjectPolicyAuthorityDecision = store
            .get(&changed.authority)
            .expect("read active authority")
            .expect("active authority object");

        historical_authority.schema_version = 2;
        historical_authority.authorized_by.assurance = AssuranceLevel::Authenticated;
        let historical_authority_object = CanonicalObject::freeze(&historical_authority)
            .expect("freeze future-schema historical authority");
        historical_policy.schema_version = 2;
        historical_policy.supported_effects = vec![EffectClass::Observe];
        historical_policy.grant_ttl_seconds = CONTROL_POLICY_V1_MAX_GRANT_TTL_SECONDS + 1;
        historical_policy.authority = historical_authority_object.hash().clone();
        let historical_object =
            CanonicalObject::freeze(&historical_policy).expect("freeze historical policy");
        active_authority.previous_policy = Some(historical_object.hash().clone());
        let active_authority_object =
            CanonicalObject::freeze(&active_authority).expect("freeze rebound authority");
        active_policy.previous_policy = Some(historical_object.hash().clone());
        active_policy.authority = active_authority_object.hash().clone();
        let active_policy_object =
            CanonicalObject::freeze(&active_policy).expect("freeze rebound active policy");

        let transaction = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("begin history replacement");
        SqliteStore::insert_object(&transaction, "control_policy", &historical_object)
            .expect("insert historical policy");
        SqliteStore::insert_object(
            &transaction,
            "project_policy_authority_decision",
            &historical_authority_object,
        )
        .expect("insert future-schema historical authority");
        SqliteStore::insert_object(
            &transaction,
            "project_policy_authority_decision",
            &active_authority_object,
        )
        .expect("insert rebound authority");
        SqliteStore::insert_object(&transaction, "control_policy", &active_policy_object)
            .expect("insert rebound active policy");
        transaction
            .execute("DELETE FROM control_policy_versions", [])
            .expect("replace projected history");
        transaction
            .execute(
                "INSERT INTO control_policy_versions (
                     policy_hash, policy_epoch, authority_hash, policy_json
                 ) VALUES (?1, ?2, ?3, ?4), (?5, ?6, ?7, ?8)",
                params![
                    historical_object.hash().as_str(),
                    historical_policy.policy_epoch.0,
                    historical_policy.authority.as_str(),
                    historical_object.bytes(),
                    active_policy_object.hash().as_str(),
                    active_policy.policy_epoch.0,
                    active_authority_object.hash().as_str(),
                    active_policy_object.bytes(),
                ],
            )
            .expect("install replacement history");
        transaction
            .execute(
                "UPDATE control_policy_state SET policy_hash = ?1 WHERE singleton = 1",
                [active_policy_object.hash().as_str()],
            )
            .expect("select replacement active head");
        transaction.commit().expect("commit replacement history");
        drop(store);

        let reopened = SqliteStore::open(&database).expect("reopen compatible history");
        let diagnostics = reopened.control_diagnostics().expect("diagnostics");
        assert_eq!(diagnostics.active_policy, *active_policy_object.hash());
        assert_eq!(diagnostics.policy_epoch, ProjectPolicyEpoch(2));
        assert!(reopened.verify_all().expect("verify history").is_healthy());
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the corruption fixture must rebind both policy versions and the successor authority"
    )]
    fn set_required_assurance_history_cannot_change_effects_or_ttl() {
        let directory = tempfile::tempdir().expect("temporary store directory");
        let database = directory.path().join("engram.db");
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let mut store = open_with_assurance(&database, ControlAssurance::Advisory)
            .expect("initialize policy history");
        let initial = store.control_diagnostics().expect("initial diagnostics");
        let mut historical_policy: ControlPolicy = store
            .get(&initial.active_policy)
            .expect("read initial policy")
            .expect("initial policy object");
        let changed = store
            .set_required_control_assurance(
                ControlAssurance::TurnGated,
                &actor("policy-admin"),
                "create current policy",
                "policy-current-envelope",
                Some(&initial.active_policy),
                now,
                &DevelopmentNoopRedactor,
            )
            .expect("activate current policy");
        let mut active_policy: ControlPolicy = store
            .get(&changed.active_policy)
            .expect("read active policy")
            .expect("active policy object");
        let mut active_authority: ProjectPolicyAuthorityDecision = store
            .get(&changed.authority)
            .expect("read active authority")
            .expect("active authority object");

        historical_policy.supported_effects = SqliteStore::legacy_v1_control_effects();
        historical_policy.grant_ttl_seconds -= 1;
        let historical_object =
            CanonicalObject::freeze(&historical_policy).expect("freeze corrupted history");
        active_authority.previous_policy = Some(historical_object.hash().clone());
        let active_authority_object =
            CanonicalObject::freeze(&active_authority).expect("freeze rebound authority");
        active_policy.previous_policy = Some(historical_object.hash().clone());
        active_policy.authority = active_authority_object.hash().clone();
        let active_policy_object =
            CanonicalObject::freeze(&active_policy).expect("freeze rebound active policy");

        let transaction = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("begin corrupt history replacement");
        SqliteStore::insert_object(&transaction, "control_policy", &historical_object)
            .expect("insert corrupted historical policy");
        SqliteStore::insert_object(
            &transaction,
            "project_policy_authority_decision",
            &active_authority_object,
        )
        .expect("insert rebound authority");
        SqliteStore::insert_object(&transaction, "control_policy", &active_policy_object)
            .expect("insert rebound active policy");
        transaction
            .execute("DELETE FROM control_policy_versions", [])
            .expect("replace projected history");
        transaction
            .execute(
                "INSERT INTO control_policy_versions (
                     policy_hash, policy_epoch, authority_hash, policy_json
                 ) VALUES (?1, ?2, ?3, ?4), (?5, ?6, ?7, ?8)",
                params![
                    historical_object.hash().as_str(),
                    historical_policy.policy_epoch.0,
                    historical_policy.authority.as_str(),
                    historical_object.bytes(),
                    active_policy_object.hash().as_str(),
                    active_policy.policy_epoch.0,
                    active_authority_object.hash().as_str(),
                    active_policy_object.bytes(),
                ],
            )
            .expect("install corrupt history");
        transaction
            .execute(
                "UPDATE control_policy_state SET policy_hash = ?1 WHERE singleton = 1",
                [active_policy_object.hash().as_str()],
            )
            .expect("select corrupt active head");
        transaction.commit().expect("commit corrupt history");

        let integrity = store.verify_all().expect("scan corrupt history");
        assert!(
            integrity
                .invalid_control_records
                .contains(&"control_policy_state:active".into())
        );
        drop(store);
        assert!(matches!(
            SqliteStore::open(&database),
            Err(StoreError::InvalidControlProjection(reason))
                if reason.contains("SetRequiredAssurance")
        ));
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one table-driven boundary test keeps every nested attribution leaf and both size limits visibly covered"
    )]
    fn policy_administrator_attribution_is_fully_inspected_and_bounded() {
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let mut store = SqliteStore::open_in_memory().expect("store");
        let mut actors = Vec::new();
        for field in ["run", "session", "tool", "skill", "source", "reference"] {
            let mut candidate = actor("policy-admin");
            match field {
                "run" => candidate.run_id = Some("reject-me".into()),
                "session" => candidate.session_id = Some(SessionId("reject-me".into())),
                "tool" => candidate.source_tool = Some("reject-me".into()),
                "skill" => candidate.source_skill = Some("reject-me".into()),
                "source" => candidate.provenance_chain.push(ProvenanceLink {
                    relation: ProvenanceRelation::AssertedBy,
                    source: "reject-me".into(),
                    reference: None,
                }),
                "reference" => candidate.provenance_chain.push(ProvenanceLink {
                    relation: ProvenanceRelation::AssertedBy,
                    source: "test-source".into(),
                    reference: Some("reject-me".into()),
                }),
                _ => unreachable!("enumerated attribution field"),
            }
            actors.push(candidate);
        }
        for candidate in actors {
            assert!(matches!(
                store.set_required_control_assurance(
                    ControlAssurance::Advisory,
                    &candidate,
                    "inspect every attribution leaf",
                    "policy-redaction-leaf",
                    None,
                    now,
                    &SentinelRedactor,
                ),
                Err(StoreError::RedactionRefused(_))
            ));
        }

        let mut oversized_field = actor("policy-admin");
        oversized_field.source_skill = Some("x".repeat(4_097));
        assert!(matches!(
            store.set_required_control_assurance(
                ControlAssurance::Advisory,
                &oversized_field,
                "bound optional fields",
                "policy-oversized-field",
                None,
                now,
                &DevelopmentNoopRedactor,
            ),
            Err(StoreError::InvalidControlProjection(_))
        ));

        let mut too_many_links = actor("policy-admin");
        too_many_links.provenance_chain = (0..=MAX_CONTROL_POLICY_PROVENANCE_LINKS)
            .map(|index| ProvenanceLink {
                relation: ProvenanceRelation::DerivedFrom,
                source: format!("source-{index}"),
                reference: None,
            })
            .collect();
        assert!(matches!(
            store.set_required_control_assurance(
                ControlAssurance::Advisory,
                &too_many_links,
                "bound provenance count",
                "policy-provenance-count",
                None,
                now,
                &DevelopmentNoopRedactor,
            ),
            Err(StoreError::InvalidControlProjection(_))
        ));

        let mut oversized_attribution = actor("policy-admin");
        oversized_attribution.provenance_chain = (0..MAX_CONTROL_POLICY_PROVENANCE_LINKS)
            .map(|index| ProvenanceLink {
                relation: ProvenanceRelation::RelayedBy,
                source: format!("source-{index}-{}", "s".repeat(2_000)),
                reference: Some(format!("reference-{index}-{}", "r".repeat(2_000))),
            })
            .collect();
        assert!(matches!(
            store.set_required_control_assurance(
                ControlAssurance::Advisory,
                &oversized_attribution,
                "bound aggregate attribution",
                "policy-oversized-attribution",
                None,
                now,
                &DevelopmentNoopRedactor,
            ),
            Err(StoreError::InvalidControlProjection(_))
        ));

        let mut normalized = actor(" policy-admin ");
        normalized.run_id = Some(" run-1 ".into());
        normalized.session_id = Some(SessionId(" session-1 ".into()));
        normalized.source_tool = Some(" host-tool ".into());
        normalized.source_skill = Some(" control-skill ".into());
        normalized.provenance_chain = vec![ProvenanceLink {
            relation: ProvenanceRelation::AssertedBy,
            source: " host-observation ".into(),
            reference: Some(" receipt-1 ".into()),
        }];
        let receipt = store
            .set_required_control_assurance(
                ControlAssurance::Advisory,
                &normalized,
                " normalize persisted attribution ",
                "policy-normalized-attribution",
                None,
                now,
                &DevelopmentNoopRedactor,
            )
            .expect("persist normalized attribution");
        let authority: ProjectPolicyAuthorityDecision = store
            .get(&receipt.authority)
            .expect("read authority")
            .expect("authority object");
        assert_eq!(authority.authorized_by.actor_id, "policy-admin");
        assert_eq!(authority.authorized_by.run_id.as_deref(), Some("run-1"));
        assert_eq!(
            authority.authorized_by.session_id,
            Some(SessionId("session-1".into()))
        );
        assert_eq!(
            authority.authorized_by.provenance_chain[0]
                .reference
                .as_deref(),
            Some("receipt-1")
        );
        assert_eq!(authority.reason, "normalize persisted attribution");
    }

    #[test]
    fn stock_v1_policy_migrates_once_and_future_policy_schema_refuses_before_ddl() {
        let directory = tempfile::tempdir().expect("temporary store directory");
        let legacy_database = directory.path().join("legacy.db");
        let legacy = SqliteStore::open(&legacy_database).expect("initialize migration fixture");
        legacy
            .connection
            .execute(
                "UPDATE control_policy_state SET
                     schema_version = 1, policy_epoch = 1,
                     required_assurance = 'turn_gated',
                     supported_effects_json = '[\"observe\",\"communicate\",\"mutate_local\"]',
                     grant_ttl_seconds = 30, policy_hash = NULL
                 WHERE singleton = 1",
                [],
            )
            .expect("restore stock V1 state");
        legacy
            .connection
            .execute("DELETE FROM control_policy_versions", [])
            .expect("remove newer history projection");
        legacy
            .connection
            .execute(
                "DELETE FROM objects WHERE object_kind IN (
                     'control_policy', 'project_policy_authority_decision'
                 )",
                [],
            )
            .expect("remove newer canonical history");
        drop(legacy);

        let migrated = SqliteStore::open(&legacy_database).expect("migrate stock V1 policy");
        let diagnostics = migrated
            .control_diagnostics()
            .expect("migrated diagnostics");
        assert_eq!(diagnostics.policy_epoch, ProjectPolicyEpoch(2));
        assert_eq!(diagnostics.required_assurance, ControlAssurance::TurnGated);
        assert_eq!(
            migrated
                .connection
                .query_row("SELECT COUNT(*) FROM control_policy_versions", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("count migrated versions"),
            2
        );
        assert!(
            migrated
                .verify_all()
                .expect("verify migration")
                .is_healthy()
        );
        drop(migrated);
        drop(SqliteStore::open(&legacy_database).expect("idempotent migration reopen"));

        let future_database = directory.path().join("future.db");
        let future = SqliteStore::open(&future_database).expect("initialize future fixture");
        future
            .connection
            .execute("DROP INDEX control_observations_session_sequence", [])
            .expect("create observable missing DDL");
        future
            .connection
            .execute(
                "UPDATE control_policy_state SET schema_version = 999 WHERE singleton = 1",
                [],
            )
            .expect("install future policy schema marker");
        drop(future);
        assert!(matches!(
            SqliteStore::open(&future_database),
            Err(StoreError::InvalidControlProjection(_))
        ));
        let raw = Connection::open(&future_database).expect("inspect refused future store");
        assert!(
            !raw.query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM sqlite_master
                     WHERE type = 'index'
                       AND name = 'control_observations_session_sequence'
                 )",
                [],
                |row| row.get::<_, bool>(0),
            )
            .expect("future store remains unmodified")
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one lifecycle fixture keeps issued and begun grant behavior visibly comparable across the same policy transition"
    )]
    fn policy_epoch_change_expires_issued_grants_but_not_begun_checkpoints() {
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let mut store = SqliteStore::open_in_memory().expect("store");
        let begun = bind_control_for(
            &mut store,
            "policy-begun",
            "bind-policy-begun",
            &[EffectClass::Observe],
            now,
        );
        let issued = bind_control_for(
            &mut store,
            "policy-issued",
            "bind-policy-issued",
            &[EffectClass::Observe],
            now,
        );
        let evaluate = |store: &mut SqliteStore,
                        binding: &TestControlBinding,
                        key: &str,
                        at: DateTime<Utc>| {
            store
                .evaluate_control_turn(
                    &ProjectId("project-a".into()),
                    &binding.status.session_id,
                    &binding.connection_token,
                    &binding.routing_token,
                    &TurnIntent {
                        idempotency_key: key.into(),
                        intent_fingerprint: ObjectHash::from_canonical_bytes(key.as_bytes()),
                        purpose: TurnPurpose::Ordinary,
                        requested_effects: vec![EffectClass::Observe],
                        resource_intents: Vec::new(),
                    },
                    at,
                )
                .expect("evaluate controlled turn")
        };
        let ControlTurnDecision::Grant { grant: begun_grant } =
            evaluate(&mut store, &begun, "policy-begun-turn", now)
        else {
            panic!("begun fixture must grant");
        };
        let begun_tokens = begun_grant
            .delivery
            .iter()
            .map(|delivery| delivery.page.delivery_token.clone())
            .collect::<Vec<_>>();
        assert!(matches!(
            store
                .begin_control_turn(
                    &ProjectId("project-a".into()),
                    &begun.status.session_id,
                    &begun.connection_token,
                    &begun.routing_token,
                    &begun_grant.grant_id,
                    &begun_tokens,
                    "begin-before-policy-change",
                    now + TimeDelta::milliseconds(1),
                )
                .expect("begin pre-change grant"),
            ControlTurnBeginDecision::Begin { .. }
        ));
        let ControlTurnDecision::Grant {
            grant: issued_grant,
        } = evaluate(
            &mut store,
            &issued,
            "policy-issued-turn",
            now + TimeDelta::milliseconds(2),
        )
        else {
            panic!("issued fixture must grant");
        };

        let changed = store
            .set_required_control_assurance(
                ControlAssurance::Advisory,
                &actor("policy-admin"),
                "exercise policy epoch transition",
                "policy-epoch-transition",
                None,
                now + TimeDelta::seconds(1),
                &DevelopmentNoopRedactor,
            )
            .expect("activate new policy");
        assert_eq!(changed.policy_epoch, ProjectPolicyEpoch(2));
        assert!(matches!(
            store
                .checkpoint_control_turn(
                    &ProjectId("project-a".into()),
                    &begun.status.session_id,
                    &begun.connection_token,
                    &begun.routing_token,
                    &begun_grant.grant_id,
                    TurnNextIntent::Continue,
                    "checkpoint-after-policy-change",
                    now + TimeDelta::seconds(2),
                )
                .expect("checkpoint begun grant"),
            ControlTurnCheckpointDecision::Checkpointed { .. }
        ));
        assert!(matches!(
            evaluate(
                &mut store,
                &begun,
                "policy-refresh-after-checkpoint",
                now + TimeDelta::milliseconds(2_100),
            ),
            ControlTurnDecision::Refuse {
                directive: crate::domain::ControlDirective {
                    code: crate::domain::ControlRefusalCode::PolicyEpochChanged,
                    ..
                }
            }
        ));
        assert_eq!(
            store
                .control_status(
                    &ProjectId("project-a".into()),
                    &begun.status.session_id,
                    &begun.connection_token,
                    &begun.routing_token,
                    now + TimeDelta::milliseconds(2_100),
                )
                .expect("status after evaluation epoch refresh")
                .epochs
                .project_policy,
            ProjectPolicyEpoch(2)
        );
        assert!(matches!(
            evaluate(
                &mut store,
                &begun,
                "policy-turn-after-refresh",
                now + TimeDelta::milliseconds(2_200),
            ),
            ControlTurnDecision::Grant { .. }
        ));
        assert!(matches!(
            store
                .begin_control_turn(
                    &ProjectId("project-a".into()),
                    &issued.status.session_id,
                    &issued.connection_token,
                    &issued.routing_token,
                    &issued_grant.grant_id,
                    &[],
                    "begin-issued-after-policy-change",
                    now + TimeDelta::seconds(2),
                )
                .expect("refuse stale issued grant"),
            ControlTurnBeginDecision::Refuse {
                code: crate::domain::ControlRefusalCode::PolicyEpochChanged
            }
        ));
        assert_eq!(
            store
                .control_status(
                    &ProjectId("project-a".into()),
                    &issued.status.session_id,
                    &issued.connection_token,
                    &issued.routing_token,
                    now + TimeDelta::seconds(2),
                )
                .expect("status after epoch refusal")
                .epochs
                .project_policy,
            ProjectPolicyEpoch(2)
        );
        assert!(matches!(
            evaluate(
                &mut store,
                &issued,
                "policy-issued-re-evaluated",
                now + TimeDelta::seconds(3),
            ),
            ControlTurnDecision::Grant { .. }
        ));
    }

    #[test]
    fn action_gated_requirement_refuses_every_v1_host_fail_closed() {
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let mut store = SqliteStore::open_in_memory().expect("store");
        let turn_gated = bind_control_for(
            &mut store,
            "turn-gated-under-action-policy",
            "bind-before-action-policy",
            &[EffectClass::Observe],
            now,
        );
        let current = store.control_diagnostics().expect("current policy");
        store
            .set_required_control_assurance(
                ControlAssurance::ActionGated,
                &actor("action-policy-admin"),
                "prove the unavailable assurance fails closed",
                "policy-action-gated",
                Some(&current.active_policy),
                now + TimeDelta::seconds(1),
                &DevelopmentNoopRedactor,
            )
            .expect("activate action-gated requirement");

        let action_floor_lease = store
            .acquire_work_lease(
                &ProjectId("project-a".into()),
                &turn_gated.status.session_id,
                &turn_gated.connection_token,
                &turn_gated.routing_token,
                crate::domain::LeaseKind::Execution,
                crate::domain::LeaseMode::Exclusive,
                &crate::domain::ResourceSubject::Path {
                    project_id: ProjectId("project-a".into()),
                    segments: vec!["src".into()],
                    coverage: crate::domain::ResourceCoverage::Tree,
                },
                60,
                "lease-under-action-policy",
                now + TimeDelta::seconds(2),
            )
            .expect("action-gated project floor is a lease decision");
        assert!(matches!(
            action_floor_lease,
            WorkLeaseDecision::Refuse { directive }
                if directive.code == ControlRefusalCode::ControlAssuranceInsufficient
                    && directive.effect.is_none()
                    && directive.required_assurance == Some(ControlAssurance::ActionGated)
        ));

        assert!(matches!(
            store
                .evaluate_control_turn(
                    &ProjectId("project-a".into()),
                    &turn_gated.status.session_id,
                    &turn_gated.connection_token,
                    &turn_gated.routing_token,
                    &TurnIntent {
                        idempotency_key: "turn-under-action-policy".into(),
                        intent_fingerprint: ObjectHash::from_canonical_bytes(
                            b"turn-under-action-policy",
                        ),
                        purpose: TurnPurpose::Ordinary,
                        requested_effects: vec![EffectClass::Observe],
                        resource_intents: Vec::new(),
                    },
                    now + TimeDelta::seconds(2),
                )
                .expect("evaluate turn-gated host"),
            ControlTurnDecision::Refuse {
                directive: crate::domain::ControlDirective {
                    code: crate::domain::ControlRefusalCode::ControlAssuranceInsufficient,
                    ..
                }
            }
        ));

        let action_session = SessionId("action-gated-host".into());
        let connection_token = store
            .resume_control_connection(&action_session, now + TimeDelta::seconds(3))
            .expect("resume action-gated host connection");
        assert!(matches!(
            store.bind_control_session(
                &ProjectId("project-a".into()),
                "dummy:ACTION-GATED-HOST",
                "V1 must reject an action-gated declaration",
                &action_session,
                &connection_token,
                &actor("action-gated-host"),
                ControlAssurance::ActionGated,
                &[EffectClass::Observe],
                1,
                "bind-action-gated-host",
                now + TimeDelta::seconds(3),
            ),
            Err(StoreError::InvalidControlSession(reason))
                if reason.contains("bind fields")
        ));
    }

    #[test]
    fn turn_gated_observe_only_session_cannot_reserve_undeclared_effects() {
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let mut store = SqliteStore::open_in_memory().expect("store");
        let binding = bind_control_for(
            &mut store,
            "observe-only-host",
            "bind-observe-only-host",
            &[EffectClass::Observe],
            now,
        );
        complete_control_turn(
            &mut store,
            &binding,
            "sync-observe-only-host",
            vec![EffectClass::Observe],
            Vec::new(),
            now + TimeDelta::seconds(1),
        );
        let subject = crate::domain::ResourceSubject::Path {
            project_id: ProjectId("project-a".into()),
            segments: vec!["src".into()],
            coverage: crate::domain::ResourceCoverage::Tree,
        };

        for (kind, effect, key) in [
            (
                crate::domain::LeaseKind::Execution,
                EffectClass::MutateLocal,
                "observe-only-execution-lease",
            ),
            (
                crate::domain::LeaseKind::Coordination,
                EffectClass::Coordinate,
                "observe-only-coordination-lease",
            ),
        ] {
            let decision = store
                .acquire_work_lease(
                    &ProjectId("project-a".into()),
                    &binding.status.session_id,
                    &binding.connection_token,
                    &binding.routing_token,
                    kind,
                    crate::domain::LeaseMode::Exclusive,
                    &subject,
                    60,
                    key,
                    now + TimeDelta::seconds(2),
                )
                .expect("mediation refusal is a lease decision");
            let WorkLeaseDecision::Refuse { directive } = decision else {
                panic!("observe-only host must not reserve {effect:?}");
            };
            assert_eq!(
                directive.code,
                ControlRefusalCode::ControlAssuranceInsufficient
            );
            assert_eq!(directive.effect, Some(effect));
            assert_eq!(
                directive.required_assurance,
                Some(ControlAssurance::TurnGated)
            );
            assert_eq!(
                directive.declared_mediated_effects,
                Some(vec![EffectClass::Observe])
            );
            assert_eq!(
                directive.effective_mediated_effects,
                Some(vec![EffectClass::Observe])
            );
        }

        let turn = store
            .evaluate_control_turn(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &TurnIntent {
                    idempotency_key: "observe-only-mutation-turn".into(),
                    intent_fingerprint: ObjectHash::from_canonical_bytes(
                        b"observe-only-mutation-turn",
                    ),
                    purpose: TurnPurpose::Ordinary,
                    requested_effects: vec![EffectClass::MutateLocal],
                    resource_intents: vec![subject],
                },
                now + TimeDelta::seconds(3),
            )
            .expect("turn mediation refusal");
        let ControlTurnDecision::Refuse { directive } = turn else {
            panic!("observe-only host must not receive a mutation turn");
        };
        assert_eq!(directive.effect, Some(EffectClass::MutateLocal));
        assert_eq!(
            directive.declared_mediated_effects,
            Some(vec![EffectClass::Observe])
        );
        assert_eq!(
            directive.effective_mediated_effects,
            Some(vec![EffectClass::Observe])
        );
    }

    #[test]
    fn turn_gated_coordinate_session_can_acquire_a_coordination_lease() {
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let mut store = SqliteStore::open_in_memory().expect("store");
        let binding = bind_control_for(
            &mut store,
            "coordinate-host",
            "bind-coordinate-host",
            &[EffectClass::Observe, EffectClass::Coordinate],
            now,
        );
        complete_control_turn(
            &mut store,
            &binding,
            "sync-coordinate-host",
            vec![EffectClass::Observe],
            Vec::new(),
            now + TimeDelta::seconds(1),
        );

        let decision = store
            .acquire_work_lease(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                crate::domain::LeaseKind::Coordination,
                crate::domain::LeaseMode::Exclusive,
                &crate::domain::ResourceSubject::Path {
                    project_id: ProjectId("project-a".into()),
                    segments: vec!["coordination".into()],
                    coverage: crate::domain::ResourceCoverage::Tree,
                },
                60,
                "coordinate-lease",
                now + TimeDelta::seconds(2),
            )
            .expect("coordination lease decision");
        assert!(matches!(
            decision,
            WorkLeaseDecision::Granted { lease }
                if lease.kind == crate::domain::LeaseKind::Coordination
        ));
    }

    #[test]
    fn lease_acquire_replay_is_scoped_to_the_current_bind_generation() {
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let mut store = SqliteStore::open_in_memory().expect("store");
        let binding = bind_control_for(
            &mut store,
            "lease-rebind-host",
            "bind-lease-rebind-host",
            &[EffectClass::Observe, EffectClass::MutateLocal],
            now,
        );
        complete_control_turn(
            &mut store,
            &binding,
            "sync-lease-rebind-host",
            vec![EffectClass::Observe],
            Vec::new(),
            now + TimeDelta::seconds(1),
        );
        let subject = crate::domain::ResourceSubject::Path {
            project_id: ProjectId("project-a".into()),
            segments: vec!["src".into()],
            coverage: crate::domain::ResourceCoverage::Tree,
        };
        let WorkLeaseDecision::Granted { lease } = store
            .acquire_work_lease(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                crate::domain::LeaseKind::Execution,
                crate::domain::LeaseMode::Exclusive,
                &subject,
                60,
                "bind-scoped-acquire",
                now + TimeDelta::seconds(2),
            )
            .expect("initial lease")
        else {
            panic!("initial lease must grant");
        };
        store
            .release_work_lease(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &lease.lease_id,
                "release-before-rebind",
                now + TimeDelta::seconds(3),
            )
            .expect("release initial lease");
        let rebound = store
            .bind_control_session(
                &ProjectId("project-a".into()),
                "dummy:CONTROL-HOST-1",
                "Exercise the host control lifecycle",
                &binding.status.session_id,
                &binding.connection_token,
                &actor("lease-rebind-host"),
                ControlAssurance::TurnGated,
                &[EffectClass::Observe],
                2,
                "rebind-lease-rebind-host",
                now + TimeDelta::seconds(4),
            )
            .expect("rebind session");

        assert!(matches!(
            store.acquire_work_lease(
                &ProjectId("project-a".into()),
                &rebound.status.session_id,
                &binding.connection_token,
                &rebound.routing_token,
                crate::domain::LeaseKind::Execution,
                crate::domain::LeaseMode::Exclusive,
                &subject,
                60,
                "bind-scoped-acquire",
                now + TimeDelta::seconds(5),
            ),
            Err(StoreError::ControlOperationIdempotencyConflict { operation, key })
                if operation == "lease_acquire" && key == "bind-scoped-acquire"
        ));
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the lease epoch fixture keeps the refused replay and adopted fresh-key path adjacent"
    )]
    fn lease_epoch_refusal_is_sticky_and_adopts_for_a_fresh_key() {
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let mut store = SqliteStore::open_in_memory().expect("store");
        let binding = bind_control_for(
            &mut store,
            "lease-epoch-host",
            "bind-lease-epoch-host",
            &[EffectClass::Observe, EffectClass::MutateLocal],
            now,
        );
        complete_control_turn(
            &mut store,
            &binding,
            "sync-lease-epoch-host",
            vec![EffectClass::Observe],
            Vec::new(),
            now + TimeDelta::seconds(1),
        );
        let current = store.control_diagnostics().expect("current policy");
        store
            .set_required_control_assurance(
                ControlAssurance::Advisory,
                &actor("lease-epoch-admin"),
                "exercise lease epoch adoption",
                "policy-lease-epoch",
                Some(&current.active_policy),
                now + TimeDelta::seconds(2),
                &DevelopmentNoopRedactor,
            )
            .expect("activate epoch two");
        let subject = crate::domain::ResourceSubject::Path {
            project_id: ProjectId("project-a".into()),
            segments: vec!["src".into()],
            coverage: crate::domain::ResourceCoverage::Tree,
        };

        let stale = store
            .acquire_work_lease(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                crate::domain::LeaseKind::Execution,
                crate::domain::LeaseMode::Exclusive,
                &subject,
                60,
                "stale-epoch-lease",
                now + TimeDelta::seconds(3),
            )
            .expect("stale epoch is a lease decision");
        assert!(matches!(
            &stale,
            WorkLeaseDecision::Refuse { directive }
                if directive.code == ControlRefusalCode::PolicyEpochChanged
        ));
        assert_eq!(
            store
                .control_status(
                    &ProjectId("project-a".into()),
                    &binding.status.session_id,
                    &binding.connection_token,
                    &binding.routing_token,
                    now + TimeDelta::seconds(3),
                )
                .expect("status after lease epoch refusal")
                .epochs
                .project_policy,
            ProjectPolicyEpoch(2)
        );
        assert_eq!(
            store
                .acquire_work_lease(
                    &ProjectId("project-a".into()),
                    &binding.status.session_id,
                    &binding.connection_token,
                    &binding.routing_token,
                    crate::domain::LeaseKind::Execution,
                    crate::domain::LeaseMode::Exclusive,
                    &subject,
                    60,
                    "stale-epoch-lease",
                    now + TimeDelta::seconds(4),
                )
                .expect("sticky stale-epoch replay"),
            stale
        );
        assert!(matches!(
            store
                .acquire_work_lease(
                    &ProjectId("project-a".into()),
                    &binding.status.session_id,
                    &binding.connection_token,
                    &binding.routing_token,
                    crate::domain::LeaseKind::Execution,
                    crate::domain::LeaseMode::Exclusive,
                    &subject,
                    60,
                    "fresh-epoch-lease",
                    now + TimeDelta::seconds(5),
                )
                .expect("fresh key re-evaluates after epoch adoption"),
            WorkLeaseDecision::Granted { .. }
        ));
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one persisted lifecycle test pins bind caps, replayable lease refusal, and turn admission together"
    )]
    fn advisory_effect_floor_refuses_mutation_and_execution_lease() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = directory.path().join("engram.db");
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let mut store =
            open_with_assurance(&database, ControlAssurance::Advisory).expect("advisory store");
        let session_id = SessionId("advisory-effect-host".into());
        let connection_token = store
            .resume_control_connection(&session_id, now)
            .expect("resume advisory connection");
        let binding = store
            .bind_control_session(
                &ProjectId("project-a".into()),
                "dummy:ADVISORY-EFFECT-HOST",
                "Prove effect-specific assurance floors",
                &session_id,
                &connection_token,
                &actor("advisory-effect-host"),
                ControlAssurance::Advisory,
                &[
                    EffectClass::Observe,
                    EffectClass::Communicate,
                    EffectClass::MutateLocal,
                ],
                1,
                "bind-advisory-effect-host",
                now,
            )
            .expect("bind advisory host");
        assert_eq!(
            binding.effective_mediated_effects,
            vec![EffectClass::Observe, EffectClass::Communicate]
        );
        assert!(
            binding
                .status
                .mediated_effects
                .contains(&EffectClass::MutateLocal)
        );
        let advisory = TestControlBinding {
            binding,
            connection_token,
        };
        complete_control_turn(
            &mut store,
            &advisory,
            "sync-advisory-effect-host",
            vec![EffectClass::Observe, EffectClass::Communicate],
            Vec::new(),
            now + TimeDelta::seconds(1),
        );
        let subject = crate::domain::ResourceSubject::Path {
            project_id: ProjectId("project-a".into()),
            segments: vec!["src".into()],
            coverage: crate::domain::ResourceCoverage::Tree,
        };
        let lease_refusal = store
            .acquire_work_lease(
                &ProjectId("project-a".into()),
                &advisory.status.session_id,
                &advisory.connection_token,
                &advisory.routing_token,
                crate::domain::LeaseKind::Execution,
                crate::domain::LeaseMode::Exclusive,
                &subject,
                60,
                "lease-advisory-effect-host",
                now + TimeDelta::seconds(2),
            )
            .expect("policy refusal is a lease decision");
        assert!(matches!(
            &lease_refusal,
            WorkLeaseDecision::Refuse { directive }
                if directive.code
                    == crate::domain::ControlRefusalCode::ControlAssuranceInsufficient
                    && directive.effect == Some(EffectClass::MutateLocal)
                    && directive.required_assurance == Some(ControlAssurance::TurnGated)
        ));
        assert_eq!(
            store
                .acquire_work_lease(
                    &ProjectId("project-a".into()),
                    &advisory.status.session_id,
                    &advisory.connection_token,
                    &advisory.routing_token,
                    crate::domain::LeaseKind::Execution,
                    crate::domain::LeaseMode::Exclusive,
                    &subject,
                    60,
                    "lease-advisory-effect-host",
                    now + TimeDelta::seconds(3),
                )
                .expect("replay policy refusal"),
            lease_refusal
        );
        let decision = store
            .evaluate_control_turn(
                &ProjectId("project-a".into()),
                &advisory.status.session_id,
                &advisory.connection_token,
                &advisory.routing_token,
                &TurnIntent {
                    idempotency_key: "mutate-under-advisory-assurance".into(),
                    intent_fingerprint: ObjectHash::from_canonical_bytes(
                        b"mutate-under-advisory-assurance",
                    ),
                    purpose: TurnPurpose::Ordinary,
                    requested_effects: vec![EffectClass::MutateLocal],
                    resource_intents: vec![subject.clone()],
                },
                now + TimeDelta::seconds(4),
            )
            .expect("evaluate advisory mutation");
        let ControlTurnDecision::Refuse { directive } = decision else {
            panic!("advisory mutation must refuse");
        };
        assert_eq!(
            directive.code,
            crate::domain::ControlRefusalCode::ControlAssuranceInsufficient
        );
        assert_eq!(directive.effect, Some(EffectClass::MutateLocal));
        assert_eq!(
            directive.required_assurance,
            Some(ControlAssurance::TurnGated)
        );

        let turn_gated = bind_control_for_task(
            &mut store,
            "turn-gated-effect-host",
            "bind-turn-gated-effect-host",
            "dummy:ADVISORY-EFFECT-HOST",
            &[EffectClass::Observe, EffectClass::MutateLocal],
            now + TimeDelta::seconds(5),
        );
        complete_control_turn(
            &mut store,
            &turn_gated,
            "sync-turn-gated-effect-host",
            vec![EffectClass::Observe],
            Vec::new(),
            now + TimeDelta::seconds(6),
        );
        let WorkLeaseDecision::Granted { .. } = store
            .acquire_work_lease(
                &ProjectId("project-a".into()),
                &turn_gated.status.session_id,
                &turn_gated.connection_token,
                &turn_gated.routing_token,
                crate::domain::LeaseKind::Execution,
                crate::domain::LeaseMode::Exclusive,
                &subject,
                60,
                "lease-turn-gated-effect-host",
                now + TimeDelta::seconds(7),
            )
            .expect("turn-gated execution lease")
        else {
            panic!("turn-gated host must acquire an execution lease");
        };
        assert!(matches!(
            store
                .evaluate_control_turn(
                    &ProjectId("project-a".into()),
                    &turn_gated.status.session_id,
                    &turn_gated.connection_token,
                    &turn_gated.routing_token,
                    &TurnIntent {
                        idempotency_key: "mutate-under-turn-gated-assurance".into(),
                        intent_fingerprint: ObjectHash::from_canonical_bytes(
                            b"mutate-under-turn-gated-assurance",
                        ),
                        purpose: TurnPurpose::Ordinary,
                        requested_effects: vec![EffectClass::MutateLocal],
                        resource_intents: vec![subject],
                    },
                    now + TimeDelta::seconds(8),
                )
                .expect("evaluate turn-gated mutation"),
            ControlTurnDecision::Grant { .. }
        ));
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

    fn open_with_assurance(
        path: &Path,
        required_assurance: ControlAssurance,
    ) -> Result<SqliteStore, StoreError> {
        SqliteStore::open_with_initial_control_assurance(
            path,
            Some(HostPathPolicy::host_default()),
            required_assurance,
            &actor("bootstrap-policy-admin"),
            "select the test bootstrap policy",
            &DevelopmentNoopRedactor,
        )
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
            work_id: None,
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
    fn context_explanation_requires_the_current_task_and_project_binding() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        let project = ProjectId("context-auth-project".into());
        let session = SessionId("context-auth-session".into());
        let project_packet = store
            .build_context(&project, None, &session, "context-auth-session", Utc::now())
            .expect("project-only context");
        let first = store
            .start_task(
                &project,
                "dummy:CONTEXT-A",
                "First context",
                &session,
                actor("context-auth-session"),
                Utc::now(),
            )
            .expect("first task");
        assert!(
            store
                .explain_context(
                    &project_packet.header.packet_hash,
                    &project,
                    &session,
                    "context-auth-session",
                )
                .is_ok(),
            "an unrelated task binding must not revoke a project-only packet"
        );
        let packet = store
            .build_context(
                &project,
                Some(first.task.task_id),
                &session,
                "context-auth-session",
                Utc::now(),
            )
            .expect("task context");
        assert_eq!(
            store
                .explain_context(
                    &packet.header.packet_hash,
                    &project,
                    &session,
                    "context-auth-session",
                )
                .expect("current packet remains explainable")
                .task_id,
            Some(first.task.task_id)
        );

        for (requested_project, requested_session) in [
            (ProjectId("different-project".into()), session.clone()),
            (project.clone(), SessionId("different-session".into())),
        ] {
            assert!(matches!(
                store.explain_context(
                    &packet.header.packet_hash,
                    &requested_project,
                    &requested_session,
                    "context-auth-session",
                ),
                Err(StoreError::PacketAccessDenied(_))
            ));
        }

        store
            .start_task(
                &project,
                "dummy:CONTEXT-B",
                "Replacement context",
                &session,
                actor("context-auth-session"),
                Utc::now(),
            )
            .expect("replace active task binding");
        assert!(matches!(
            store.explain_context(
                &packet.header.packet_hash,
                &project,
                &session,
                "context-auth-session",
            ),
            Err(StoreError::PacketAccessDenied(_))
        ));
        store
            .join_task(
                &project,
                "dummy:CONTEXT-A",
                &session,
                actor("context-auth-session"),
                Utc::now(),
            )
            .expect("restore original task binding");
        assert!(
            store
                .explain_context(
                    &packet.header.packet_hash,
                    &project,
                    &session,
                    "context-auth-session",
                )
                .is_ok()
        );
    }

    fn install_memory_task(store: &SqliteStore, task_id: TaskId, sessions: &[&str]) {
        let now = Utc::now().timestamp_millis();
        store
            .connection
            .execute(
                "INSERT INTO tasks (
                     task_id, project_id, external_ref, title, state,
                     event_cursor, created_at_ms, updated_at_ms
                 ) VALUES (?1, 'project-a', ?2, 'Memory test', 'active', 0, ?3, ?3)",
                params![
                    task_id.0.to_string(),
                    format!("memory-test:{task_id:?}"),
                    now
                ],
            )
            .expect("install memory test task");
        for session in sessions {
            store
                .connection
                .execute(
                    "INSERT INTO task_participants (task_id, session_id, joined_at_ms)
                     VALUES (?1, ?2, ?3)",
                    params![task_id.0.to_string(), session, now],
                )
                .expect("install memory test participant");
            store
                .connection
                .execute(
                    "INSERT INTO session_bindings (session_id, task_id, bound_at_ms)
                     VALUES (?1, ?2, ?3)",
                    params![session, task_id.0.to_string(), now],
                )
                .expect("install memory test binding");
        }
    }

    fn turn_evaluation(task_id: TaskId) -> TurnEvaluationInput {
        TurnEvaluationInput {
            control_schema_version: crate::domain::CONTROL_SCHEMA_VERSION,
            session_id: SessionId("control-session".into()),
            task_id: Some(task_id),
            work_binding: None,
            work_binding_current: true,
            participant_membership: crate::domain::ParticipantMembership::Member,
            task_state: Some(TaskState::Active),
            phase: SessionPhase::Ready,
            health: ControlHealth::Healthy,
            active_policy_known: true,
            host_assurance: ControlAssurance::Advisory,
            required_assurance: ControlAssurance::Advisory,
            policy_effects: vec![
                EffectClass::Observe,
                EffectClass::Communicate,
                EffectClass::MutateLocal,
                EffectClass::MutateShared,
                EffectClass::ExternalSideEffect,
                EffectClass::Lifecycle,
            ],
            mediated_effects: vec![
                EffectClass::Observe,
                EffectClass::Communicate,
                EffectClass::MutateLocal,
                EffectClass::MutateShared,
                EffectClass::ExternalSideEffect,
                EffectClass::Lifecycle,
            ],
            current_epochs: ControlEpochs {
                project_policy: ProjectPolicyEpoch(1),
                task_admission: TaskAdmissionEpoch(2),
            },
            session_epochs: ControlEpochs {
                project_policy: ProjectPolicyEpoch(1),
                task_admission: TaskAdmissionEpoch(2),
            },
            confirmed_cursor: ChangeCursor(3),
            head_cursor: ChangeCursor(3),
            pending_delivery: None,
            packet_safety: PacketSafety::Safe,
            blocking_watermark: ChangeCursor(3),
            acknowledged_blocking_watermark: ChangeCursor(3),
            has_unknown_action_outcome: false,
            authority_satisfied: true,
            capability_map_revision: 1,
            leases: Vec::new(),
            intent: TurnIntent {
                idempotency_key: "observe-turn-a".into(),
                intent_fingerprint: ObjectHash::from_canonical_bytes(b"turn-a"),
                purpose: TurnPurpose::Ordinary,
                requested_effects: vec![EffectClass::Observe],
                resource_intents: Vec::new(),
            },
            evaluated_at: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
            grant_ttl_seconds: 30,
        }
    }

    struct TestControlBinding {
        binding: ControlSessionBinding,
        connection_token: String,
    }

    impl std::ops::Deref for TestControlBinding {
        type Target = ControlSessionBinding;

        fn deref(&self) -> &Self::Target {
            &self.binding
        }
    }

    fn bind_control(store: &mut SqliteStore, now: DateTime<Utc>) -> TestControlBinding {
        bind_control_for(
            store,
            "control-session",
            "bind-control-a",
            &[EffectClass::Observe, EffectClass::Communicate],
            now,
        )
    }

    fn bind_control_for(
        store: &mut SqliteStore,
        session: &str,
        bind_key: &str,
        mediated_effects: &[EffectClass],
        now: DateTime<Utc>,
    ) -> TestControlBinding {
        bind_control_for_task(
            store,
            session,
            bind_key,
            "dummy:CONTROL-HOST-1",
            mediated_effects,
            now,
        )
    }

    fn bind_control_for_task(
        store: &mut SqliteStore,
        session: &str,
        bind_key: &str,
        external_ref: &str,
        mediated_effects: &[EffectClass],
        now: DateTime<Utc>,
    ) -> TestControlBinding {
        let session_id = SessionId(session.into());
        let connection_token = store.resume_control_connection(&session_id, now).unwrap();
        let binding = store
            .bind_control_session(
                &ProjectId("project-a".into()),
                external_ref,
                "Exercise the host control lifecycle",
                &session_id,
                &connection_token,
                &actor(session),
                ControlAssurance::TurnGated,
                mediated_effects,
                1,
                bind_key,
                now,
            )
            .unwrap();
        TestControlBinding {
            binding,
            connection_token,
        }
    }

    fn complete_control_turn(
        store: &mut SqliteStore,
        binding: &TestControlBinding,
        key: &str,
        requested_effects: Vec<EffectClass>,
        resource_intents: Vec<crate::domain::ResourceSubject>,
        now: DateTime<Utc>,
    ) -> IssuedTurnGrant {
        let decision = store
            .evaluate_control_turn(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &TurnIntent {
                    idempotency_key: format!("turn-{key}"),
                    intent_fingerprint: ObjectHash::from_canonical_bytes(key.as_bytes()),
                    purpose: crate::domain::TurnPurpose::Ordinary,
                    requested_effects,
                    resource_intents,
                },
                now,
            )
            .unwrap();
        let ControlTurnDecision::Grant { grant } = decision else {
            panic!("control turn {key} must grant");
        };
        let delivery_tokens = grant
            .delivery
            .iter()
            .map(|delivery| delivery.page.delivery_token.clone())
            .collect::<Vec<_>>();
        assert!(matches!(
            store
                .begin_control_turn(
                    &ProjectId("project-a".into()),
                    &binding.status.session_id,
                    &binding.connection_token,
                    &binding.routing_token,
                    &grant.grant_id,
                    &delivery_tokens,
                    &format!("begin-{key}"),
                    now + TimeDelta::milliseconds(1),
                )
                .unwrap(),
            ControlTurnBeginDecision::Begin { .. }
        ));
        assert!(matches!(
            store
                .checkpoint_control_turn(
                    &ProjectId("project-a".into()),
                    &binding.status.session_id,
                    &binding.connection_token,
                    &binding.routing_token,
                    &grant.grant_id,
                    TurnNextIntent::Continue,
                    &format!("checkpoint-{key}"),
                    now + TimeDelta::milliseconds(2),
                )
                .unwrap(),
            ControlTurnCheckpointDecision::Checkpointed { .. }
        ));
        *grant
    }

    #[test]
    fn task_only_control_checkpoint_cannot_append_execution_observations() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let binding = bind_control(&mut store, now);
        complete_control_turn(
            &mut store,
            &binding,
            "task-only-sync",
            vec![EffectClass::Observe],
            Vec::new(),
            now + TimeDelta::seconds(1),
        );
        let decision = store
            .evaluate_control_turn(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &TurnIntent {
                    idempotency_key: "task-only-observation-turn".into(),
                    intent_fingerprint: ObjectHash::from_canonical_bytes(
                        b"task-only observation turn",
                    ),
                    purpose: TurnPurpose::Ordinary,
                    requested_effects: vec![EffectClass::Observe],
                    resource_intents: Vec::new(),
                },
                now + TimeDelta::seconds(2),
            )
            .expect("evaluate task-only observation turn");
        let ControlTurnDecision::Grant { grant } = decision else {
            panic!("task-only observation turn should otherwise grant");
        };
        let delivery_tokens = grant
            .delivery
            .iter()
            .map(|delivery| delivery.page.delivery_token.clone())
            .collect::<Vec<_>>();
        assert!(matches!(
            store
                .begin_control_turn(
                    &ProjectId("project-a".into()),
                    &binding.status.session_id,
                    &binding.connection_token,
                    &binding.routing_token,
                    &grant.grant_id,
                    &delivery_tokens,
                    "begin-task-only-observation",
                    now + TimeDelta::seconds(3),
                )
                .expect("begin task-only observation turn"),
            ControlTurnBeginDecision::Begin { .. }
        ));
        let rejected = store.checkpoint_control_turn_with_observations(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &grant.grant_id,
            TurnNextIntent::Continue,
            &[ExecutionObservationInput {
                observation_id: "task-only-observation".into(),
                action_fingerprint: ObjectHash::from_canonical_bytes(b"read task context"),
                effect: EffectClass::Observe,
                outcome: crate::domain::ExecutionOutcome::Succeeded,
                source_changed: false,
                source_basis: None,
                observed_at: None,
            }],
            "checkpoint-task-only-observation",
            now + TimeDelta::seconds(4),
        );
        assert!(matches!(
            rejected,
            Err(StoreError::InvalidControlSession(message))
                if message.contains("local-work binding")
        ));
        let observations = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM objects WHERE object_kind = 'execution_observation'",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count task-only observations");
        assert_eq!(observations, 0);
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
                checked_objects: 4,
                invalid_objects: Vec::new(),
                checked_control_records: 2,
                invalid_control_records: Vec::new(),
                checked_work_records: 1,
                invalid_work_records: Vec::new(),
                legacy_work_records: Vec::new(),
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
    fn task_local_cursors_keep_exact_host_delivery_dense_across_interleaved_tasks() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let binding = bind_control(&mut store, now);
        let task_a = binding.status.task_id;
        let task_b = store
            .start_task(
                &ProjectId("project-a".into()),
                "dummy:CONTROL-HOST-2",
                "Interleave another task",
                &SessionId("other-session".into()),
                actor("other-session"),
                now + TimeDelta::milliseconds(1),
            )
            .expect("second task")
            .task
            .task_id;
        store
            .capture_note(
                &note_request(
                    task_b,
                    "other-session",
                    "Decision: task B advances independently.",
                    "interleaved-b",
                    NoteVisibility::Shared,
                ),
                &DevelopmentNoopRedactor,
            )
            .expect("task B note");
        store
            .capture_note(
                &note_request(
                    task_a,
                    "control-session",
                    "Decision: task A delivery remains exact.",
                    "interleaved-a",
                    NoteVisibility::Shared,
                ),
                &DevelopmentNoopRedactor,
            )
            .expect("task A note");

        for task_id in [task_a, task_b] {
            let changes = store
                .task_changes_since(task_id, ChangeCursor(0), 100)
                .expect("task-local changes");
            assert!(changes.iter().enumerate().all(|(offset, change)| {
                change.cursor.0 == i64::try_from(offset).expect("small test offset") + 1
            }));
        }

        let decision = store
            .evaluate_control_turn(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &TurnIntent {
                    idempotency_key: "interleaved-turn-a".into(),
                    intent_fingerprint: ObjectHash::from_canonical_bytes(b"interleaved-turn-a"),
                    purpose: TurnPurpose::Ordinary,
                    requested_effects: vec![EffectClass::Observe],
                    resource_intents: Vec::new(),
                },
                now + TimeDelta::seconds(1),
            )
            .expect("evaluate exact task A delivery");
        let ControlTurnDecision::Grant { grant } = decision else {
            panic!("interleaved task delivery must grant");
        };
        let delivery = grant.delivery.as_ref().expect("initial exact delta");
        assert!(
            delivery
                .delta
                .changes
                .iter()
                .enumerate()
                .all(|(offset, change)| {
                    change.cursor.0 == i64::try_from(offset).expect("small test offset") + 1
                })
        );
        assert!(crate::control::delivery_matches_grant(&grant));
    }

    #[test]
    fn host_delivery_refuses_a_gap_in_the_task_local_feed() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let binding = bind_control(&mut store, now);
        store
            .capture_note(
                &note_request(
                    binding.status.task_id,
                    "control-session",
                    "Decision: create a second task-local event.",
                    "gap-note-a",
                    NoteVisibility::Shared,
                ),
                &DevelopmentNoopRedactor,
            )
            .expect("task note");
        let head = store
            .connection
            .query_row(
                "SELECT COALESCE(MAX(task_cursor), 0)
                 FROM task_changes WHERE task_id = ?1",
                [binding.status.task_id.0.to_string()],
                |row| row.get::<_, i64>(0).map(ChangeCursor),
            )
            .expect("task head");
        assert!(head.0 > 1);
        store
            .connection
            .execute(
                "DELETE FROM task_changes WHERE task_id = ?1 AND task_cursor = 1",
                [binding.status.task_id.0.to_string()],
            )
            .expect("create corrupt task-feed gap");
        assert!(matches!(
            store.evaluate_control_turn(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &TurnIntent {
                    idempotency_key: "gapped-turn".into(),
                    intent_fingerprint: ObjectHash::from_canonical_bytes(b"gapped-turn"),
                    purpose: TurnPurpose::Ordinary,
                    requested_effects: vec![EffectClass::Observe],
                    resource_intents: Vec::new(),
                },
                now + TimeDelta::seconds(1),
            ),
            Err(StoreError::InvalidTaskProjection(_))
        ));
    }

    #[test]
    fn another_agents_private_capture_does_not_invalidate_or_enter_a_grant() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let binding = bind_control(&mut store, now);
        let peer_session = SessionId("private-peer".into());
        store
            .join_task(
                &ProjectId("project-a".into()),
                "dummy:CONTROL-HOST-1",
                &peer_session,
                actor("private-peer"),
                now,
            )
            .expect("join private peer");
        let decision = store
            .evaluate_control_turn(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &TurnIntent {
                    idempotency_key: "owner-scoped-private-grant".into(),
                    intent_fingerprint: ObjectHash::from_canonical_bytes(
                        b"owner-scoped-private-grant",
                    ),
                    purpose: TurnPurpose::Ordinary,
                    requested_effects: vec![EffectClass::Observe],
                    resource_intents: Vec::new(),
                },
                now + TimeDelta::seconds(1),
            )
            .expect("evaluate owner context");
        let ControlTurnDecision::Grant { grant } = decision else {
            panic!("owner context must grant");
        };
        let peer_private = store
            .capture_note(
                &note_request(
                    binding.status.task_id,
                    "private-peer",
                    "Constraint: only the peer may see this private rule.",
                    "peer-private-after-grant",
                    NoteVisibility::Private,
                ),
                &DevelopmentNoopRedactor,
            )
            .expect("capture peer-private memory");
        assert_eq!(peer_private.cursor, None);
        let token = grant
            .delivery
            .as_ref()
            .expect("context delivery")
            .page
            .delivery_token
            .clone();
        assert!(matches!(
            store
                .begin_control_turn(
                    &ProjectId("project-a".into()),
                    &binding.status.session_id,
                    &binding.connection_token,
                    &binding.routing_token,
                    &grant.grant_id,
                    &[token],
                    "begin-after-peer-private",
                    now + TimeDelta::seconds(2),
                )
                .expect("peer-private state cannot invalidate owner context"),
            ControlTurnBeginDecision::Begin { .. }
        ));
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one recovery scenario proves every page advances and converges to an ordinary turn"
    )]
    fn recovery_turns_drain_a_bounded_backlog_before_ordinary_work_resumes() {
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let mut store = SqliteStore::open_in_memory().expect("store");
        let binding = bind_control(&mut store, now);
        for index in 0..(MAX_CONTROL_DELIVERY_EVENTS * 2) {
            store
                .append_task_object(
                    binding.status.task_id,
                    "backlog_test_event",
                    &Example {
                        title: format!("event-{index}"),
                        body: "bounded".into(),
                    },
                )
                .expect("append backlog event");
        }
        assert!(matches!(
            store
                .evaluate_control_turn(
                    &ProjectId("project-a".into()),
                    &binding.status.session_id,
                    &binding.connection_token,
                    &binding.routing_token,
                    &TurnIntent {
                        idempotency_key: "ordinary-before-recovery".into(),
                        intent_fingerprint: ObjectHash::from_canonical_bytes(
                            b"ordinary-before-recovery",
                        ),
                        purpose: TurnPurpose::Ordinary,
                        requested_effects: vec![EffectClass::Observe],
                        resource_intents: Vec::new(),
                    },
                    now + TimeDelta::milliseconds(1),
                )
                .expect("ordinary backlog decision"),
            ControlTurnDecision::Refuse { directive }
                if directive.code == crate::domain::ControlRefusalCode::RecoveryRequired
        ));

        let mut saw_partial = false;
        let mut pages = 0_i64;
        loop {
            pages += 1;
            assert!(pages <= 5, "bounded recovery must converge");
            let decision = store
                .evaluate_control_turn(
                    &ProjectId("project-a".into()),
                    &binding.status.session_id,
                    &binding.connection_token,
                    &binding.routing_token,
                    &TurnIntent {
                        idempotency_key: format!("recovery-page-{pages}"),
                        intent_fingerprint: ObjectHash::from_canonical_bytes(
                            format!("recovery-page-{pages}").as_bytes(),
                        ),
                        purpose: TurnPurpose::Recovery,
                        requested_effects: vec![EffectClass::Observe],
                        resource_intents: Vec::new(),
                    },
                    now + TimeDelta::seconds(pages),
                )
                .expect("recovery page decision");
            let ControlTurnDecision::Grant { grant } = decision else {
                panic!("recovery page must grant, got {decision:?}");
            };
            let delivery = grant.delivery.as_ref().expect("recovery delivery");
            assert!(
                delivery.delta.changes.len()
                    <= usize::try_from(MAX_CONTROL_DELIVERY_EVENTS).expect("positive event budget")
            );
            if delivery.page.has_more {
                saw_partial = true;
                assert!(delivery.context.is_none());
            } else {
                assert!(delivery.context.is_some());
            }
            let begun = store
                .begin_control_turn(
                    &ProjectId("project-a".into()),
                    &binding.status.session_id,
                    &binding.connection_token,
                    &binding.routing_token,
                    &grant.grant_id,
                    std::slice::from_ref(&delivery.page.delivery_token),
                    &format!("begin-recovery-page-{pages}"),
                    now + TimeDelta::seconds(pages) + TimeDelta::milliseconds(1),
                )
                .expect("begin recovery page");
            assert!(matches!(begun, ControlTurnBeginDecision::Begin { .. }));
            let checkpoint = store
                .checkpoint_control_turn(
                    &ProjectId("project-a".into()),
                    &binding.status.session_id,
                    &binding.connection_token,
                    &binding.routing_token,
                    &grant.grant_id,
                    TurnNextIntent::Continue,
                    &format!("checkpoint-recovery-page-{pages}"),
                    now + TimeDelta::seconds(pages) + TimeDelta::milliseconds(2),
                )
                .expect("checkpoint recovery page");
            let ControlTurnCheckpointDecision::Checkpointed { receipt } = checkpoint else {
                panic!("recovery page must checkpoint");
            };
            if !delivery.page.has_more {
                assert_eq!(receipt.phase, SessionPhase::Ready);
                break;
            }
            assert_eq!(receipt.phase, SessionPhase::SyncRequired);
        }
        assert!(saw_partial);

        let ordinary = store
            .evaluate_control_turn(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &TurnIntent {
                    idempotency_key: "ordinary-after-recovery".into(),
                    intent_fingerprint: ObjectHash::from_canonical_bytes(
                        b"ordinary-after-recovery",
                    ),
                    purpose: TurnPurpose::Ordinary,
                    requested_effects: vec![EffectClass::Observe],
                    resource_intents: Vec::new(),
                },
                now + TimeDelta::seconds(10),
            )
            .expect("ordinary turn after recovery");
        let ControlTurnDecision::Grant { grant } = ordinary else {
            panic!("ordinary turn must be granted after recovery");
        };
        let delivery = grant
            .delivery
            .expect("ordinary turn carries a context-only delivery basis");
        assert_eq!(delivery.page.from_cursor, delivery.page.to_cursor);
        assert_eq!(delivery.page.to_cursor, delivery.page.head_cursor);
        assert!(!delivery.page.has_more);
        assert!(delivery.delta.changes.is_empty());
        assert!(delivery.context.is_some());

        let oversized = store.append_task_object(
            binding.status.task_id,
            "oversized_test_event",
            &Example {
                title: "oversized".into(),
                body: "x".repeat(MAX_TASK_CHANGE_OBJECT_BYTES + 1),
            },
        );
        assert!(matches!(
            oversized,
            Err(StoreError::InvalidTaskProjection(_))
        ));
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the restart scenario keeps frozen delivery, cursor, fencing, and checkpoint assertions together"
    )]
    fn begun_partial_recovery_is_exactly_redeliverable_after_host_restart() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database = directory.path().join("engram.db");
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let mut store = SqliteStore::open(&database).expect("store");
        let binding = bind_control(&mut store, now);
        for index in 0..(MAX_CONTROL_DELIVERY_EVENTS * 2) {
            store
                .append_task_object(
                    binding.status.task_id,
                    "restart_backlog_event",
                    &Example {
                        title: format!("event-{index}"),
                        body: "bounded".into(),
                    },
                )
                .expect("append backlog event");
        }
        let decision = store
            .evaluate_control_turn(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &TurnIntent {
                    idempotency_key: "restart-recovery-page".into(),
                    intent_fingerprint: ObjectHash::from_canonical_bytes(b"restart-recovery-page"),
                    purpose: TurnPurpose::Recovery,
                    requested_effects: vec![EffectClass::Observe],
                    resource_intents: Vec::new(),
                },
                now + TimeDelta::seconds(1),
            )
            .expect("recovery decision");
        let ControlTurnDecision::Grant { grant } = decision else {
            panic!("partial recovery must grant");
        };
        let delivery = grant.delivery.as_ref().expect("delivery");
        assert!(delivery.page.has_more);
        store
            .begin_control_turn(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &grant.grant_id,
                std::slice::from_ref(&delivery.page.delivery_token),
                "begin-restart-recovery-page",
                now + TimeDelta::seconds(2),
            )
            .expect("begin recovery");
        store
            .append_task_object(
                binding.status.task_id,
                "post_begin_event",
                &Example {
                    title: "later".into(),
                    body: "must remain pending".into(),
                },
            )
            .expect("append event after begin");
        drop(store);

        let mut reopened = SqliteStore::open(&database).expect("reopen store");
        let connection_token = reopened
            .resume_control_connection(&binding.status.session_id, now + TimeDelta::seconds(3))
            .expect("resume host connection");
        let status = reopened
            .control_status(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &connection_token,
                &binding.routing_token,
                now + TimeDelta::seconds(3),
            )
            .expect("status after restart");
        assert_eq!(status.phase, SessionPhase::TurnOpen);
        assert_eq!(status.confirmed_cursor, binding.status.confirmed_cursor);
        assert_eq!(status.tentative_cursor, Some(grant.basis.delivery_cursor));
        assert_eq!(status.recoverable_grant.as_deref(), Some(grant.as_ref()));
        assert!(matches!(
            reopened.checkpoint_control_turn(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &grant.grant_id,
                TurnNextIntent::Continue,
                "checkpoint-from-superseded-host",
                now + TimeDelta::seconds(4),
            ),
            Err(StoreError::ControlConnectionSuperseded(_))
        ));

        let checkpoint = reopened
            .checkpoint_control_turn(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &connection_token,
                &binding.routing_token,
                &grant.grant_id,
                TurnNextIntent::Continue,
                "checkpoint-redelivered-recovery-page",
                now + TimeDelta::seconds(4),
            )
            .expect("checkpoint exact redelivery");
        let ControlTurnCheckpointDecision::Checkpointed { receipt } = checkpoint else {
            panic!("redelivered page must checkpoint");
        };
        assert_eq!(receipt.confirmed_cursor, grant.basis.delivery_cursor);
        assert_eq!(receipt.phase, SessionPhase::SyncRequired);
    }

    #[test]
    fn legacy_global_task_cursor_store_requires_an_explicit_reset_without_mutation() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database = directory.path().join("legacy.db");
        let connection = Connection::open(&database).expect("legacy connection");
        connection
            .execute_batch(
                "CREATE TABLE task_changes (
                     cursor INTEGER PRIMARY KEY AUTOINCREMENT,
                     task_id TEXT NOT NULL,
                     object_kind TEXT NOT NULL,
                     object_hash TEXT NOT NULL,
                     UNIQUE(task_id, object_hash)
                 ) STRICT;
                 CREATE TABLE control_turn_grants (
                     grant_id TEXT PRIMARY KEY,
                     session_id TEXT NOT NULL,
                     task_id TEXT NOT NULL,
                     request_key TEXT NOT NULL,
                     grant_hash TEXT NOT NULL,
                     grant_json BLOB NOT NULL,
                     state TEXT NOT NULL,
                     issued_at_ms INTEGER NOT NULL,
                     expires_at_ms INTEGER NOT NULL,
                     begun_at_ms INTEGER,
                     completed_at_ms INTEGER,
                     UNIQUE(session_id, request_key)
                 ) STRICT;
                 INSERT INTO control_turn_grants (
                     grant_id, session_id, task_id, request_key, grant_hash,
                     grant_json, state, issued_at_ms, expires_at_ms, begun_at_ms
                 ) VALUES (
                     'begun-grant', 'session-a', 'task-a', 'request-a',
                     'not-read-during-migration', X'7B7D', 'begun', 1, 2, 1
                 );",
            )
            .expect("legacy schema");
        drop(connection);

        assert!(matches!(
            SqliteStore::open(&database),
            Err(StoreError::InvalidTaskProjection(message))
                if message.contains("cannot be renumbered safely")
        ));
        let connection = Connection::open(&database).expect("inspect failed migration");
        let state = connection
            .query_row(
                "SELECT state FROM control_turn_grants WHERE grant_id = 'begun-grant'",
                [],
                |row| row.get::<_, String>(0),
            )
            .expect("begun grant remains");
        assert_eq!(state, "begun");
        let has_task_cursor = connection
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM pragma_table_info('task_changes')
                     WHERE name = 'task_cursor'
                 )",
                [],
                |row| row.get::<_, bool>(0),
            )
            .expect("legacy columns");
        assert!(!has_task_cursor);
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
        install_memory_task(&store, task_id, &["session-a", "session-b"]);
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
        let mut restricted_request = request.clone();
        restricted_request.prose = "restricted: never return this task memory body".into();
        restricted_request.sensitivity = Some(Sensitivity::Restricted);
        restricted_request.idempotency_key = "note-restricted".into();
        let restricted = store
            .capture_note(&restricted_request, &DevelopmentNoopRedactor)
            .expect("capture restricted task memory");
        let visible = store
            .search_memories(
                &request.project_id,
                Some(task_id),
                None,
                &SessionId("session-b".into()),
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
        assert_ne!(visible[0].version, restricted.version);
        assert!(first.cursor.is_some());

        let mut conflict = request.clone();
        conflict.prose = "Decision: reuse the key for something else".into();
        assert!(matches!(
            store.capture_note(&conflict, &DevelopmentNoopRedactor),
            Err(StoreError::NoteIdempotencyConflict(_))
        ));
    }

    #[test]
    fn note_idempotency_keys_are_scoped_to_the_calling_session() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let task_id = TaskId::new();
        install_memory_task(&store, task_id, &["session-a", "session-b"]);
        let first = note_request(
            task_id,
            "session-a",
            "Decision: first caller meaning",
            "local-retry-1",
            NoteVisibility::Shared,
        );
        let second = note_request(
            task_id,
            "session-b",
            "Decision: second caller meaning",
            "local-retry-1",
            NoteVisibility::Shared,
        );

        let first = store
            .capture_note(&first, &DevelopmentNoopRedactor)
            .expect("first caller-local key");
        let second = store
            .capture_note(&second, &DevelopmentNoopRedactor)
            .expect("same raw key is independent in another session");

        assert_ne!(first.memory_id, second.memory_id);
        assert_eq!(first.idempotency_key, second.idempotency_key);
    }

    #[test]
    fn private_task_scratch_never_enters_the_peer_feed() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let task_id = TaskId::new();
        install_memory_task(&store, task_id, &["agent-a", "agent-b"]);
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
                .search_memories(
                    &request.project_id,
                    Some(task_id),
                    None,
                    &SessionId("agent-a".into()),
                    "agent-a",
                    None,
                    20,
                )
                .unwrap()
                .len(),
            1
        );
        assert!(
            store
                .search_memories(
                    &request.project_id,
                    Some(task_id),
                    None,
                    &SessionId("agent-b".into()),
                    "agent-b",
                    None,
                    20,
                )
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
                    None,
                    &session_b,
                    "eval-b",
                ),
                Err(StoreError::MemoryAccessDenied(_))
            ));
            assert!(
                store
                    .search_memories(
                        &project,
                        Some(task_id),
                        None,
                        &session_b,
                        "eval-b",
                        Some("hypothesis Z"),
                        20,
                    )
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
                None,
                &session_b,
                "eval-b",
            )
            .unwrap();
        assert_eq!(shown.version.actor.session_id, Some(session_a));
        assert!(!shown.version.classification_reason.is_empty());
        assert_eq!(
            reopened
                .explain_context(&packet.header.packet_hash, &project, &session_b, "eval-b",)
                .unwrap()
                .event_cursor,
            packet.header.event_cursor
        );
        assert!(matches!(
            reopened.show_memory(
                &private_hash,
                &project,
                Some(task_id),
                None,
                &session_b,
                "eval-b",
            ),
            Err(StoreError::MemoryAccessDenied(_))
        ));
    }

    #[test]
    fn memory_projection_rebuilds_from_canonical_objects() {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let task_id = TaskId::new();
        install_memory_task(&store, task_id, &["agent-a", "agent-b"]);
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
                None,
                &SessionId("agent-b".into()),
                "agent-b",
                Some("integration restart"),
                20,
            )
            .unwrap();
        assert_eq!(rebuilt.len(), 1);
        assert_eq!(rebuilt[0].kind, crate::domain::MemoryKind::Fact);
    }

    #[test]
    fn rebuild_advances_context_revisions_even_when_a_scope_has_no_surviving_projection() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        store
            .connection
            .execute(
                "INSERT INTO project_context_revisions (project_id, revision)
                 VALUES ('removed-project-scope', 7)",
                [],
            )
            .expect("install prior project revision");
        store
            .connection
            .execute(
                "INSERT INTO agent_context_revisions (project_id, agent_id, revision)
                 VALUES ('removed-agent-scope', 'agent-a', 11)",
                [],
            )
            .expect("install prior private revision");

        assert_eq!(
            store.rebuild_memory_index().expect("rebuild empty index"),
            0
        );
        let revisions = store
            .connection
            .query_row(
                "SELECT
                     (SELECT revision FROM project_context_revisions
                      WHERE project_id = 'removed-project-scope'),
                     (SELECT revision FROM agent_context_revisions
                      WHERE project_id = 'removed-agent-scope' AND agent_id = 'agent-a')",
                [],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
            )
            .expect("read rebuilt revision fences");
        assert_eq!(revisions, (8, 12));
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
                Some(task.task.task_id),
                None,
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
                Some(task.task.task_id),
                None,
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
            .search_memories(
                &project,
                Some(task.task.task_id),
                None,
                &session_b,
                "agent-b",
                None,
                20,
            )
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
    #[allow(
        clippy::too_many_lines,
        reason = "one migration fixture proves both non-activation and fail-closed current projection behavior"
    )]
    fn unsupported_contradiction_schema_never_activates_or_passes_projection_checks() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database = directory.path().join("engram.db");
        let mut store = SqliteStore::open(&database).expect("store");
        let project = ProjectId("project-a".into());
        let task_id = TaskId::new();
        let now = Utc::now();
        install_memory_task(&store, task_id, &["agent-a"]);
        let first = store
            .capture_note(
                &note_request(
                    task_id,
                    "agent-a",
                    "Constraint: preserve the current contradiction schema.",
                    "unknown-edge-left",
                    NoteVisibility::Shared,
                ),
                &DevelopmentNoopRedactor,
            )
            .expect("first endpoint");
        let second = store
            .capture_note(
                &note_request(
                    task_id,
                    "agent-a",
                    "Constraint: reject unsupported contradiction schemas.",
                    "unknown-edge-right",
                    NoteVisibility::Shared,
                ),
                &DevelopmentNoopRedactor,
            )
            .expect("second endpoint");
        let (left, right) = if first.version < second.version {
            (first.version, second.version)
        } else {
            (second.version, first.version)
        };
        let object = CanonicalObject::freeze(&serde_json::json!({
            "schema_version": SCHEMA_VERSION + 1,
            "future_edge": { "opaque": true }
        }))
        .expect("canonical incompatible future event");
        let transaction = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("begin future-event fixture");
        SqliteStore::insert_object(&transaction, "memory_contradiction_event", &object)
            .expect("store future event without activating it");
        transaction.commit().expect("commit future-event fixture");
        store
            .connection
            .execute(
                "INSERT INTO memory_contradictions (
                     contradiction_hash, task_id, left_version_hash, right_version_hash
                 ) VALUES (?1, ?2, ?3, ?4)",
                params![
                    object.hash().as_str(),
                    task_id.0.to_string(),
                    left.as_str(),
                    right.as_str(),
                ],
            )
            .expect("install legacy projection fixture");
        drop(store);

        let store = SqliteStore::open(&database)
            .expect("writable reopen retains the object and skips its projection");
        let activated = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM memory_contradiction_edges
                 WHERE contradiction_hash = ?1",
                [object.hash().as_str()],
                |row| row.get::<_, i64>(0),
            )
            .expect("count activated edges");
        assert_eq!(activated, 0);
        let retained_legacy = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM memory_contradictions
                 WHERE contradiction_hash = ?1",
                [object.hash().as_str()],
                |row| row.get::<_, i64>(0),
            )
            .expect("count retained legacy projections");
        assert_eq!(retained_legacy, 1);
        assert!(
            SqliteStore::get_canonical_object_on(
                &store.connection,
                object.hash(),
                "memory_contradiction_event",
            )
            .expect("verify retained future object")
            .is_some()
        );
        drop(store);
        let read_only =
            Connection::open_with_flags(&database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
                .expect("open migrated store read-only");
        drop(
            SqliteStore::from_connection(read_only, Some(HostPathPolicy::host_default()), None)
                .expect("unsupported legacy projection does not require later write locks"),
        );

        let mut store = SqliteStore::open(&database).expect("reopen writable test store");
        store
            .connection
            .execute(
                "INSERT INTO memory_contradiction_edges (
                     contradiction_hash, project_id, task_id, work_root_id,
                     left_version_hash, right_version_hash
                 ) VALUES (?1, ?2, ?3, NULL, ?4, ?5)",
                params![
                    object.hash().as_str(),
                    project.0,
                    task_id.0.to_string(),
                    left.as_str(),
                    right.as_str(),
                ],
            )
            .expect("install unsupported current projection fixture");
        assert!(matches!(
            store.build_context(
                &project,
                Some(task_id),
                &SessionId("agent-a".into()),
                "agent-a",
                now,
            ),
            Err(StoreError::InvalidMemoryProjection(message))
                if message.contains("unsupported schema version")
        ));
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
                Some(task.task.task_id),
                None,
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

    #[test]
    fn shadow_turn_observations_are_idempotent_across_restart() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("engram.db");
        let (first, input) = {
            let mut store = SqliteStore::open(&database).unwrap();
            let binding = store
                .start_task(
                    &ProjectId("project-a".into()),
                    "dummy:CONTROL-1",
                    "Observe turn admission",
                    &SessionId("control-session".into()),
                    actor("control-session"),
                    Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
                )
                .unwrap();
            let note_cursor = store
                .capture_note(
                    &note_request(
                        binding.task.task_id,
                        "control-session",
                        "Evidence: the durable task feed advanced after task start.",
                        "control-note-a",
                        NoteVisibility::Shared,
                    ),
                    &DevelopmentNoopRedactor,
                )
                .unwrap()
                .cursor
                .expect("shared note must advance the task feed");
            assert!(note_cursor > binding.cursor);
            let mut input = turn_evaluation(binding.task.task_id);
            input.participant_membership = ParticipantMembership::NotMember;
            input.task_state = Some(TaskState::Published);
            input.confirmed_cursor = note_cursor;
            input.head_cursor = ChangeCursor(999);
            input.blocking_watermark = note_cursor;
            input.acknowledged_blocking_watermark = note_cursor;
            let first = store.record_turn_observation(&input).unwrap();
            (first, input)
        };
        assert!(matches!(first.decision, TurnDecision::Grant { .. }));

        let mut replay_input = input.clone();
        replay_input.evaluated_at += TimeDelta::minutes(5);
        replay_input.phase = SessionPhase::CheckpointRequired;
        let mut reopened = SqliteStore::open(&database).unwrap();
        let replay = reopened.record_turn_observation(&replay_input).unwrap();
        assert_eq!(first, replay);
        let healthy = reopened.verify_all().unwrap();
        assert!(healthy.is_healthy());
        assert_eq!(healthy.checked_control_records, 3);

        let mut unknown_schema = input.clone();
        unknown_schema.control_schema_version = CONTROL_SCHEMA_VERSION + 1;
        unknown_schema.intent.idempotency_key = "observe-turn-unknown-schema".into();
        unknown_schema.intent.intent_fingerprint =
            ObjectHash::from_canonical_bytes(b"turn-unknown-schema");
        let unknown_schema_observation = reopened.record_turn_observation(&unknown_schema).unwrap();
        assert!(matches!(
            unknown_schema_observation.decision,
            TurnDecision::Refuse { ref directive }
                if directive.code == crate::domain::ControlRefusalCode::UnknownControlSchema
        ));
        assert_eq!(
            reopened.record_turn_observation(&unknown_schema).unwrap(),
            unknown_schema_observation
        );
        let healthy_with_unknown_schema = reopened.verify_all().unwrap();
        assert!(healthy_with_unknown_schema.is_healthy());
        assert_eq!(healthy_with_unknown_schema.checked_control_records, 4);

        let mut conflicting = input.clone();
        conflicting
            .intent
            .requested_effects
            .push(EffectClass::MutateShared);
        assert!(matches!(
            reopened.record_turn_observation(&conflicting),
            Err(StoreError::TurnObservationIdempotencyConflict(_))
        ));

        reopened
            .connection
            .execute(
                "UPDATE control_observations SET decision_json = ?1
                 WHERE idempotency_key = 'observe-turn-a'",
                params![b"{}".as_slice()],
            )
            .unwrap();
        assert!(matches!(
            reopened.record_turn_observation(&replay_input),
            Err(StoreError::HashMismatch { .. })
        ));
        let corrupted = reopened.verify_all().unwrap();
        assert_eq!(
            corrupted.invalid_control_records,
            vec!["control_observation:1"]
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one lifecycle test preserves the restart and stale-grant sequence"
    )]
    fn host_control_turn_is_restart_safe_and_fails_closed_on_drift() {
        let directory = tempfile::tempdir().unwrap();
        let database = directory.path().join("engram.db");
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let mut store = SqliteStore::open(&database).unwrap();
        let binding = bind_control(&mut store, now);
        assert_eq!(binding.status.phase, SessionPhase::SyncRequired);
        assert_eq!(
            store
                .bind_control_session(
                    &ProjectId("project-a".into()),
                    "dummy:CONTROL-HOST-1",
                    "Exercise the host control lifecycle",
                    &binding.status.session_id,
                    &binding.connection_token,
                    &actor("control-session"),
                    ControlAssurance::TurnGated,
                    &[EffectClass::Observe, EffectClass::Communicate],
                    1,
                    "bind-control-a",
                    now,
                )
                .unwrap(),
            binding.binding
        );
        assert!(matches!(
            store.control_status(
                &ProjectId("project-a".into()),
                &SessionId("control-session".into()),
                &binding.connection_token,
                "wrong-token",
                now,
            ),
            Err(StoreError::ControlSessionTokenMismatch(_))
        ));
        let private_writer = SessionId("private-writer".into());
        store
            .join_task(
                &ProjectId("project-a".into()),
                "dummy:CONTROL-HOST-1",
                &private_writer,
                actor("private-writer"),
                now,
            )
            .expect("join a concurrent session for the same logical agent");

        let first_intent = TurnIntent {
            idempotency_key: "host-turn-a".into(),
            intent_fingerprint: ObjectHash::from_canonical_bytes(b"host-turn-a"),
            purpose: crate::domain::TurnPurpose::Ordinary,
            requested_effects: vec![EffectClass::Observe],
            resource_intents: Vec::new(),
        };
        let first = store
            .evaluate_control_turn(
                &ProjectId("project-a".into()),
                &SessionId("control-session".into()),
                &binding.connection_token,
                &binding.routing_token,
                &first_intent,
                now + TimeDelta::seconds(1),
            )
            .unwrap();
        let crate::domain::ControlTurnDecision::Grant { grant: first_grant } = first else {
            panic!("initial synchronized turn must grant");
        };
        assert!(first_grant.delivery.is_some());
        let mut private_request = note_request(
            binding.status.task_id,
            "private-writer",
            "Constraint: a new owner-private rule invalidates an unbegun turn.",
            "host-private-drift-a",
            NoteVisibility::Private,
        );
        private_request.actor.actor_id = "control-session".into();
        let private_receipt = store
            .capture_note(&private_request, &DevelopmentNoopRedactor)
            .unwrap();
        assert_eq!(private_receipt.cursor, None);
        let stale_begin = store
            .begin_control_turn(
                &ProjectId("project-a".into()),
                &SessionId("control-session".into()),
                &binding.connection_token,
                &binding.routing_token,
                &first_grant.grant_id,
                &[first_grant
                    .delivery
                    .as_ref()
                    .unwrap()
                    .page
                    .delivery_token
                    .clone()],
                "begin-stale-a",
                now + TimeDelta::seconds(2),
            )
            .unwrap();
        assert!(matches!(
            stale_begin,
            ControlTurnBeginDecision::Refuse {
                code: crate::domain::ControlRefusalCode::DeltaRequired
            }
        ));
        drop(store);

        let mut reopened = SqliteStore::open(&database).unwrap();
        let reopened_connection = reopened
            .resume_control_connection(
                &SessionId("control-session".into()),
                now + TimeDelta::seconds(3),
            )
            .unwrap();
        let second_intent = TurnIntent {
            idempotency_key: "host-turn-b".into(),
            intent_fingerprint: ObjectHash::from_canonical_bytes(b"host-turn-b"),
            purpose: crate::domain::TurnPurpose::Ordinary,
            requested_effects: vec![EffectClass::Observe, EffectClass::Communicate],
            resource_intents: Vec::new(),
        };
        let second = reopened
            .evaluate_control_turn(
                &ProjectId("project-a".into()),
                &SessionId("control-session".into()),
                &reopened_connection,
                &binding.routing_token,
                &second_intent,
                now + TimeDelta::seconds(3),
            )
            .unwrap();
        let crate::domain::ControlTurnDecision::Grant { grant } = second else {
            panic!("fresh synchronized turn must grant");
        };
        let delivery_token = grant.delivery.as_ref().unwrap().page.delivery_token.clone();
        let begun = reopened
            .begin_control_turn(
                &ProjectId("project-a".into()),
                &SessionId("control-session".into()),
                &reopened_connection,
                &binding.routing_token,
                &grant.grant_id,
                &[delivery_token],
                "begin-host-b",
                now + TimeDelta::seconds(4),
            )
            .unwrap();
        assert!(matches!(begun, ControlTurnBeginDecision::Begin { .. }));
        let checkpointed = reopened
            .checkpoint_control_turn(
                &ProjectId("project-a".into()),
                &SessionId("control-session".into()),
                &reopened_connection,
                &binding.routing_token,
                &grant.grant_id,
                TurnNextIntent::Continue,
                "checkpoint-host-b",
                now + TimeDelta::seconds(5),
            )
            .unwrap();
        assert!(matches!(
            checkpointed,
            ControlTurnCheckpointDecision::Checkpointed { .. }
        ));

        let denied_intent = TurnIntent {
            idempotency_key: "host-turn-mutation".into(),
            intent_fingerprint: ObjectHash::from_canonical_bytes(b"host-turn-mutation"),
            purpose: crate::domain::TurnPurpose::Ordinary,
            requested_effects: vec![EffectClass::MutateLocal],
            resource_intents: Vec::new(),
        };
        assert!(matches!(
            reopened
                .evaluate_control_turn(
                    &ProjectId("project-a".into()),
                    &SessionId("control-session".into()),
                    &reopened_connection,
                    &binding.routing_token,
                    &denied_intent,
                    now + TimeDelta::seconds(6),
                )
                .unwrap(),
            ControlTurnDecision::Refuse {
                directive: crate::domain::ControlDirective {
                    code: crate::domain::ControlRefusalCode::ControlAssuranceInsufficient,
                    ..
                }
            }
        ));
    }

    #[test]
    fn integrity_scanner_covers_enforced_control_records() {
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let mut store = SqliteStore::open_in_memory().unwrap();
        let binding = bind_control(&mut store, now);
        let decision = store
            .evaluate_control_turn(
                &ProjectId("project-a".into()),
                &SessionId("control-session".into()),
                &binding.connection_token,
                &binding.routing_token,
                &TurnIntent {
                    idempotency_key: "integrity-turn-a".into(),
                    intent_fingerprint: ObjectHash::from_canonical_bytes(b"integrity-turn-a"),
                    purpose: crate::domain::TurnPurpose::Ordinary,
                    requested_effects: vec![EffectClass::Observe],
                    resource_intents: Vec::new(),
                },
                now + TimeDelta::seconds(1),
            )
            .unwrap();
        let ControlTurnDecision::Grant { grant } = decision else {
            panic!("control integrity fixture must grant");
        };
        let healthy = store.verify_all().unwrap();
        assert!(healthy.is_healthy());
        assert_eq!(healthy.checked_control_records, 5);

        store
            .connection
            .execute(
                "UPDATE control_sessions SET bind_intent_json = ?1",
                params![b"{}".as_slice()],
            )
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE control_turn_results SET decision_json = ?1",
                params![b"{}".as_slice()],
            )
            .unwrap();
        store
            .connection
            .execute(
                "UPDATE control_turn_grants SET grant_json = ?1",
                params![b"{}".as_slice()],
            )
            .unwrap();
        let corrupted = store.verify_all().unwrap();
        assert_eq!(corrupted.checked_control_records, 5);
        assert_eq!(corrupted.invalid_control_records.len(), 3);
        assert!(
            corrupted
                .invalid_control_records
                .contains(&"control_session:control-session".into())
        );
        assert!(
            corrupted
                .invalid_control_records
                .contains(&"control_turn_result:1".into())
        );
        assert!(
            corrupted
                .invalid_control_records
                .contains(&format!("control_turn_grant:{}", grant.grant_id))
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the lease test preserves acquire, mutation, conflict, release, and fencing order"
    )]
    fn scoped_work_leases_gate_mutation_and_fence_transfer() {
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let mut store = SqliteStore::open_in_memory().unwrap();
        let effects = [
            EffectClass::Observe,
            EffectClass::Communicate,
            EffectClass::MutateLocal,
        ];
        let session_a = bind_control_for(&mut store, "lease-a", "bind-lease-a", &effects, now);
        let session_b = bind_control_for(&mut store, "lease-b", "bind-lease-b", &effects, now);
        complete_control_turn(
            &mut store,
            &session_a,
            "sync-lease-a",
            vec![EffectClass::Observe],
            Vec::new(),
            now + TimeDelta::seconds(1),
        );
        let lease_subject = crate::domain::ResourceSubject::Path {
            project_id: ProjectId("project-a".into()),
            segments: vec!["src".into()],
            coverage: crate::domain::ResourceCoverage::Tree,
        };
        let lease_a = store
            .acquire_work_lease(
                &ProjectId("project-a".into()),
                &session_a.status.session_id,
                &session_a.connection_token,
                &session_a.routing_token,
                crate::domain::LeaseKind::Execution,
                crate::domain::LeaseMode::Exclusive,
                &lease_subject,
                300,
                "lease-src-a",
                now + TimeDelta::seconds(2),
            )
            .unwrap();
        let WorkLeaseDecision::Granted { lease: lease_a } = lease_a else {
            panic!("first non-conflicting lease must grant");
        };
        assert_eq!(
            store
                .acquire_work_lease(
                    &ProjectId("project-a".into()),
                    &session_a.status.session_id,
                    &session_a.connection_token,
                    &session_a.routing_token,
                    crate::domain::LeaseKind::Execution,
                    crate::domain::LeaseMode::Exclusive,
                    &lease_subject,
                    300,
                    "lease-src-a",
                    now + TimeDelta::seconds(2),
                )
                .unwrap(),
            WorkLeaseDecision::Granted {
                lease: lease_a.clone()
            }
        );
        assert!(matches!(
            store.acquire_work_lease(
                &ProjectId("project-a".into()),
                &session_a.status.session_id,
                &session_a.connection_token,
                &session_a.routing_token,
                crate::domain::LeaseKind::Execution,
                crate::domain::LeaseMode::Exclusive,
                &lease_subject,
                299,
                "lease-src-a",
                now + TimeDelta::seconds(2),
            ),
            Err(StoreError::ControlOperationIdempotencyConflict { .. })
        ));
        complete_control_turn(
            &mut store,
            &session_a,
            "mutate-lease-a",
            vec![EffectClass::MutateLocal],
            vec![crate::domain::ResourceSubject::Path {
                project_id: ProjectId("project-a".into()),
                segments: vec!["src".into(), "lib.rs".into()],
                coverage: crate::domain::ResourceCoverage::Exact,
            }],
            now + TimeDelta::seconds(3),
        );

        complete_control_turn(
            &mut store,
            &session_b,
            "sync-lease-b",
            vec![EffectClass::Observe],
            Vec::new(),
            now + TimeDelta::seconds(4),
        );
        assert!(matches!(
            store
                .acquire_work_lease(
                    &ProjectId("project-a".into()),
                    &session_b.status.session_id,
                    &session_b.connection_token,
                    &session_b.routing_token,
                    crate::domain::LeaseKind::Execution,
                    crate::domain::LeaseMode::Exclusive,
                    &lease_subject,
                    300,
                    "lease-src-b-conflict",
                    now + TimeDelta::seconds(5),
                )
                .unwrap(),
            WorkLeaseDecision::Defer { .. }
        ));
        assert!(matches!(
            store.release_work_lease(
                &ProjectId("project-a".into()),
                &session_b.status.session_id,
                &session_b.connection_token,
                &session_b.routing_token,
                &lease_a.lease_id,
                "wrong-holder-release",
                now + TimeDelta::seconds(6),
            ),
            Err(StoreError::WorkLeaseNotHeld { .. })
        ));
        let released = store
            .release_work_lease(
                &ProjectId("project-a".into()),
                &session_a.status.session_id,
                &session_a.connection_token,
                &session_a.routing_token,
                &lease_a.lease_id,
                "release-src-a",
                now + TimeDelta::seconds(6),
            )
            .unwrap();
        assert_eq!(
            store
                .release_work_lease(
                    &ProjectId("project-a".into()),
                    &session_a.status.session_id,
                    &session_a.connection_token,
                    &session_a.routing_token,
                    &lease_a.lease_id,
                    "release-src-a",
                    now + TimeDelta::seconds(6),
                )
                .unwrap(),
            released
        );
        complete_control_turn(
            &mut store,
            &session_b,
            "resync-lease-b",
            vec![EffectClass::Observe],
            Vec::new(),
            now + TimeDelta::seconds(7),
        );
        let transferred = store
            .acquire_work_lease(
                &ProjectId("project-a".into()),
                &session_b.status.session_id,
                &session_b.connection_token,
                &session_b.routing_token,
                crate::domain::LeaseKind::Execution,
                crate::domain::LeaseMode::Exclusive,
                &lease_subject,
                300,
                "lease-src-b",
                now + TimeDelta::seconds(8),
            )
            .unwrap();
        let WorkLeaseDecision::Granted { lease: lease_b } = transferred else {
            panic!("released scope must transfer");
        };
        assert_eq!(lease_b.fence, lease_a.fence + 1);
        assert_eq!(lease_b.holder, session_b.status.session_id);
        assert!(store.verify_all().unwrap().is_healthy());
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the regression preserves two task bindings, conflict, release, and project-wide fence transfer in one sequence"
    )]
    fn resource_lease_conflicts_and_fences_span_tasks_within_a_project() {
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let mut store = SqliteStore::open_in_memory().unwrap();
        let effects = [EffectClass::Observe, EffectClass::MutateLocal];
        let session_a = bind_control_for_task(
            &mut store,
            "project-lease-a",
            "bind-project-lease-a",
            "dummy:PROJECT-LEASE-A",
            &effects,
            now,
        );
        let session_b = bind_control_for_task(
            &mut store,
            "project-lease-b",
            "bind-project-lease-b",
            "dummy:PROJECT-LEASE-B",
            &effects,
            now,
        );
        assert_ne!(session_a.status.task_id, session_b.status.task_id);
        complete_control_turn(
            &mut store,
            &session_a,
            "sync-project-lease-a",
            vec![EffectClass::Observe],
            Vec::new(),
            now + TimeDelta::seconds(1),
        );
        complete_control_turn(
            &mut store,
            &session_b,
            "sync-project-lease-b",
            vec![EffectClass::Observe],
            Vec::new(),
            now + TimeDelta::seconds(2),
        );
        let subject = crate::domain::ResourceSubject::Path {
            project_id: ProjectId("project-a".into()),
            segments: vec!["src".into(), "shared.rs".into()],
            coverage: crate::domain::ResourceCoverage::Exact,
        };
        let WorkLeaseDecision::Granted { lease: first } = store
            .acquire_work_lease(
                &ProjectId("project-a".into()),
                &session_a.status.session_id,
                &session_a.connection_token,
                &session_a.routing_token,
                crate::domain::LeaseKind::Execution,
                crate::domain::LeaseMode::Exclusive,
                &subject,
                300,
                "project-lease-first",
                now + TimeDelta::seconds(3),
            )
            .unwrap()
        else {
            panic!("the first task must acquire the project resource");
        };
        assert!(matches!(
            store
                .acquire_work_lease(
                    &ProjectId("project-a".into()),
                    &session_b.status.session_id,
                    &session_b.connection_token,
                    &session_b.routing_token,
                    crate::domain::LeaseKind::Execution,
                    crate::domain::LeaseMode::Exclusive,
                    &subject,
                    300,
                    "project-lease-conflict",
                    now + TimeDelta::seconds(4),
                )
                .unwrap(),
            WorkLeaseDecision::Defer {
                conflicting_lease_id,
                ..
            } if conflicting_lease_id == first.lease_id
        ));
        store
            .release_work_lease(
                &ProjectId("project-a".into()),
                &session_a.status.session_id,
                &session_a.connection_token,
                &session_a.routing_token,
                &first.lease_id,
                "release-project-lease-first",
                now + TimeDelta::seconds(5),
            )
            .unwrap();
        let WorkLeaseDecision::Granted { lease: second } = store
            .acquire_work_lease(
                &ProjectId("project-a".into()),
                &session_b.status.session_id,
                &session_b.connection_token,
                &session_b.routing_token,
                crate::domain::LeaseKind::Execution,
                crate::domain::LeaseMode::Exclusive,
                &subject,
                300,
                "project-lease-second",
                now + TimeDelta::seconds(6),
            )
            .unwrap()
        else {
            panic!("the second task must acquire the released project resource");
        };
        assert_eq!(second.task_id, session_b.status.task_id);
        assert_eq!(second.fence, first.fence + 1);
        assert!(store.verify_all().unwrap().is_healthy());
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the expiry test preserves grant, expiry, unwind, and fence continuity"
    )]
    fn expired_lease_invalidates_unbegun_turn_and_preserves_fence_history() {
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let mut store = SqliteStore::open_in_memory().unwrap();
        let effects = [EffectClass::Observe, EffectClass::MutateLocal];
        let binding = bind_control_for(&mut store, "expiry-a", "bind-expiry-a", &effects, now);
        complete_control_turn(
            &mut store,
            &binding,
            "sync-expiry-a",
            vec![EffectClass::Observe],
            Vec::new(),
            now + TimeDelta::seconds(1),
        );
        let subject = crate::domain::ResourceSubject::Path {
            project_id: ProjectId("project-a".into()),
            segments: vec!["src".into()],
            coverage: crate::domain::ResourceCoverage::Tree,
        };
        let WorkLeaseDecision::Granted { lease: first } = store
            .acquire_work_lease(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                crate::domain::LeaseKind::Execution,
                crate::domain::LeaseMode::Exclusive,
                &subject,
                5,
                "lease-expiry-first",
                now + TimeDelta::seconds(2),
            )
            .unwrap()
        else {
            panic!("initial lease must grant");
        };
        let decision = store
            .evaluate_control_turn(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &TurnIntent {
                    idempotency_key: "turn-before-lease-expiry".into(),
                    intent_fingerprint: ObjectHash::from_canonical_bytes(
                        b"turn-before-lease-expiry",
                    ),
                    purpose: crate::domain::TurnPurpose::Ordinary,
                    requested_effects: vec![EffectClass::MutateLocal],
                    resource_intents: vec![crate::domain::ResourceSubject::Path {
                        project_id: ProjectId("project-a".into()),
                        segments: vec!["src".into(), "lib.rs".into()],
                        coverage: crate::domain::ResourceCoverage::Exact,
                    }],
                },
                now + TimeDelta::seconds(3),
            )
            .unwrap();
        let ControlTurnDecision::Grant { grant } = decision else {
            panic!("live lease must authorize the mutation turn");
        };
        assert!(matches!(
            store
                .begin_control_turn(
                    &ProjectId("project-a".into()),
                    &binding.status.session_id,
                    &binding.connection_token,
                    &binding.routing_token,
                    &grant.grant_id,
                    &[],
                    "begin-after-lease-expiry",
                    now + TimeDelta::seconds(8),
                )
                .unwrap(),
            ControlTurnBeginDecision::Refuse {
                code: crate::domain::ControlRefusalCode::StaleFence
            }
        ));
        assert_eq!(
            store
                .control_status(
                    &ProjectId("project-a".into()),
                    &binding.status.session_id,
                    &binding.connection_token,
                    &binding.routing_token,
                    now + TimeDelta::seconds(8),
                )
                .unwrap()
                .phase,
            SessionPhase::Ready
        );
        let head_before_takeover = ChangeCursor(
            store
                .connection
                .query_row(
                    "SELECT COALESCE(MAX(task_cursor), 0) FROM task_changes
                     WHERE task_id = ?1",
                    [binding.status.task_id.0.to_string()],
                    |row| row.get(0),
                )
                .unwrap(),
        );
        let WorkLeaseDecision::Granted { lease: second } = store
            .acquire_work_lease(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                crate::domain::LeaseKind::Execution,
                crate::domain::LeaseMode::Exclusive,
                &subject,
                5,
                "lease-expiry-second",
                now + TimeDelta::seconds(9),
            )
            .unwrap()
        else {
            panic!("expired scope must be acquirable");
        };
        assert_eq!(second.fence, first.fence + 1);
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT state FROM control_work_leases WHERE lease_id = ?1",
                    [&first.lease_id],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "expired"
        );
        let (confirmed, blocking): (i64, i64) = store
            .connection
            .query_row(
                "SELECT confirmed_cursor, blocking_watermark FROM control_sessions
                 WHERE session_id = ?1",
                [&binding.status.session_id.0],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(confirmed, head_before_takeover.0);
        assert_eq!(blocking, head_before_takeover.0 + 2);
        let delta = store
            .task_delta(
                &ProjectId("project-a".into()),
                binding.status.task_id,
                &binding.status.session_id,
                "agent",
                head_before_takeover,
                10,
            )
            .unwrap();
        assert_eq!(delta.changes.len(), 2);
        let expired: WorkLeaseEvent =
            serde_json::from_value(delta.changes[0].object.clone()).unwrap();
        let acquired: WorkLeaseEvent =
            serde_json::from_value(delta.changes[1].object.clone()).unwrap();
        assert_eq!(expired.lease.lease_id, first.lease_id);
        assert_eq!(expired.transition, WorkLeaseTransition::Expired);
        assert_eq!(acquired.lease.lease_id, second.lease_id);
        assert_eq!(acquired.transition, WorkLeaseTransition::Acquired);
        assert_eq!(
            store
                .acquire_work_lease(
                    &ProjectId("project-a".into()),
                    &binding.status.session_id,
                    &binding.connection_token,
                    &binding.routing_token,
                    crate::domain::LeaseKind::Execution,
                    crate::domain::LeaseMode::Exclusive,
                    &subject,
                    5,
                    "lease-expiry-second",
                    now + TimeDelta::seconds(10),
                )
                .unwrap(),
            WorkLeaseDecision::Granted {
                lease: second.clone()
            }
        );
        assert!(matches!(
            store.release_work_lease(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &first.lease_id,
                "release-expired-first",
                now + TimeDelta::seconds(10),
            ),
            Err(StoreError::WorkLeaseExpired { .. })
        ));
        assert!(store.verify_all().unwrap().is_healthy());
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the hostile sequence proves a begun turn pins explicit release and expiry transfer until checkpoint"
    )]
    fn begun_mutation_turn_pins_its_lease_until_checkpoint() {
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let directory = tempfile::tempdir().expect("temp directory");
        let database = directory.path().join("lease-pin.sqlite3");
        let mut store = SqliteStore::open(&database).unwrap();
        let effects = [EffectClass::Observe, EffectClass::MutateLocal];
        let session_a = bind_control_for(&mut store, "pin-a", "bind-pin-a", &effects, now);
        let session_b = bind_control_for(&mut store, "pin-b", "bind-pin-b", &effects, now);
        complete_control_turn(
            &mut store,
            &session_b,
            "sync-pin-b",
            vec![EffectClass::Observe],
            Vec::new(),
            now + TimeDelta::seconds(1),
        );
        complete_control_turn(
            &mut store,
            &session_a,
            "sync-pin-a",
            vec![EffectClass::Observe],
            Vec::new(),
            now + TimeDelta::seconds(2),
        );
        let subject = crate::domain::ResourceSubject::Path {
            project_id: ProjectId("project-a".into()),
            segments: vec!["src".into()],
            coverage: crate::domain::ResourceCoverage::Tree,
        };
        let WorkLeaseDecision::Granted { lease: first } = store
            .acquire_work_lease(
                &ProjectId("project-a".into()),
                &session_a.status.session_id,
                &session_a.connection_token,
                &session_a.routing_token,
                crate::domain::LeaseKind::Execution,
                crate::domain::LeaseMode::Exclusive,
                &subject,
                5,
                "pin-first",
                now + TimeDelta::seconds(3),
            )
            .unwrap()
        else {
            panic!("the first lease must grant");
        };
        let decision = store
            .evaluate_control_turn(
                &ProjectId("project-a".into()),
                &session_a.status.session_id,
                &session_a.connection_token,
                &session_a.routing_token,
                &TurnIntent {
                    idempotency_key: "pin-mutation-turn".into(),
                    intent_fingerprint: ObjectHash::from_canonical_bytes(b"pin-mutation-turn"),
                    purpose: crate::domain::TurnPurpose::Ordinary,
                    requested_effects: vec![EffectClass::MutateLocal],
                    resource_intents: vec![crate::domain::ResourceSubject::Path {
                        project_id: ProjectId("project-a".into()),
                        segments: vec!["src".into(), "lib.rs".into()],
                        coverage: crate::domain::ResourceCoverage::Exact,
                    }],
                },
                now + TimeDelta::seconds(4),
            )
            .unwrap();
        let ControlTurnDecision::Grant { grant } = decision else {
            panic!("the mutation turn must grant");
        };
        let delivery_tokens = grant
            .delivery
            .iter()
            .map(|delivery| delivery.page.delivery_token.clone())
            .collect::<Vec<_>>();
        assert!(matches!(
            store
                .begin_control_turn(
                    &ProjectId("project-a".into()),
                    &session_a.status.session_id,
                    &session_a.connection_token,
                    &session_a.routing_token,
                    &grant.grant_id,
                    &delivery_tokens,
                    "begin-pinned-turn",
                    now + TimeDelta::seconds(5),
                )
                .unwrap(),
            ControlTurnBeginDecision::Begin { .. }
        ));
        drop(store);
        let mut store = SqliteStore::open(&database).expect("restart store");
        let resumed_connection = store
            .resume_control_connection(&session_a.status.session_id, now + TimeDelta::seconds(6))
            .expect("resume begun-turn holder");
        assert!(matches!(
            store.release_work_lease(
                &ProjectId("project-a".into()),
                &session_a.status.session_id,
                &resumed_connection,
                &session_a.routing_token,
                &first.lease_id,
                "release-while-pinned",
                now + TimeDelta::seconds(6),
            ),
            Err(StoreError::InvalidControlSession(message))
                if message.contains("checkpoint the turn")
        ));

        complete_control_turn(
            &mut store,
            &session_b,
            "sync-pin-b-after-acquire",
            vec![EffectClass::Observe],
            Vec::new(),
            now + TimeDelta::seconds(7),
        );
        assert!(matches!(
            store
                .acquire_work_lease(
                    &ProjectId("project-a".into()),
                    &session_b.status.session_id,
                    &session_b.connection_token,
                    &session_b.routing_token,
                    crate::domain::LeaseKind::Execution,
                    crate::domain::LeaseMode::Exclusive,
                    &subject,
                    30,
                    "pin-second-deferred",
                    now + TimeDelta::seconds(9),
                )
                .unwrap(),
            WorkLeaseDecision::Defer {
                checkpoint_required: true,
                ..
            }
        ));
        assert!(matches!(
            store
                .checkpoint_control_turn(
                    &ProjectId("project-a".into()),
                    &session_a.status.session_id,
                    &resumed_connection,
                    &session_a.routing_token,
                    &grant.grant_id,
                    TurnNextIntent::Continue,
                    "checkpoint-pinned-turn",
                    now + TimeDelta::seconds(10),
                )
                .unwrap(),
            ControlTurnCheckpointDecision::Checkpointed { .. }
        ));
        complete_control_turn(
            &mut store,
            &session_b,
            "sync-pin-b-after-checkpoint",
            vec![EffectClass::Observe],
            Vec::new(),
            now + TimeDelta::seconds(11),
        );
        let WorkLeaseDecision::Granted { lease: second } = store
            .acquire_work_lease(
                &ProjectId("project-a".into()),
                &session_b.status.session_id,
                &session_b.connection_token,
                &session_b.routing_token,
                crate::domain::LeaseKind::Execution,
                crate::domain::LeaseMode::Exclusive,
                &subject,
                30,
                "pin-second-granted",
                now + TimeDelta::seconds(12),
            )
            .unwrap()
        else {
            panic!("checkpointed expired scope must transfer");
        };
        assert_eq!(second.fence, first.fence + 1);
        assert_eq!(second.holder, session_b.status.session_id);
        assert!(store.verify_all().unwrap().is_healthy());
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the hostile sequence checks aliases, project binding, rebind rollback, and release"
    )]
    fn resource_aliases_and_active_leases_remain_task_isolated() {
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let mut store = SqliteStore::open_in_memory_with_host_path_identity(Some(HostPathPolicy {
            case_fold_paths: true,
            windows_alias_rules: false,
        }))
        .unwrap();
        let effects = [EffectClass::Observe, EffectClass::MutateLocal];
        let session_a = bind_control_for(&mut store, "alias-a", "bind-alias-a", &effects, now);
        let session_b = bind_control_for(&mut store, "alias-b", "bind-alias-b", &effects, now);
        complete_control_turn(
            &mut store,
            &session_b,
            "sync-alias-b",
            vec![EffectClass::Observe],
            Vec::new(),
            now + TimeDelta::seconds(1),
        );
        complete_control_turn(
            &mut store,
            &session_a,
            "sync-alias-a",
            vec![EffectClass::Observe],
            Vec::new(),
            now + TimeDelta::seconds(2),
        );
        let composed = crate::domain::ResourceSubject::Path {
            project_id: ProjectId("project-a".into()),
            segments: vec!["\u{17f}rc".into(), "caf\u{e9}.rs".into()],
            coverage: crate::domain::ResourceCoverage::Exact,
        };
        let WorkLeaseDecision::Granted { lease } = store
            .acquire_work_lease(
                &ProjectId("project-a".into()),
                &session_a.status.session_id,
                &session_a.connection_token,
                &session_a.routing_token,
                crate::domain::LeaseKind::Execution,
                crate::domain::LeaseMode::Exclusive,
                &composed,
                300,
                "lease-alias-a",
                now + TimeDelta::seconds(4),
            )
            .unwrap()
        else {
            panic!("first normalized subject must grant");
        };
        complete_control_turn(
            &mut store,
            &session_b,
            "resync-alias-b",
            vec![EffectClass::Observe],
            Vec::new(),
            now + TimeDelta::seconds(4),
        );
        let decomposed = crate::domain::ResourceSubject::Path {
            project_id: ProjectId("project-a".into()),
            segments: vec!["src".into(), "cafe\u{301}.rs".into()],
            coverage: crate::domain::ResourceCoverage::Exact,
        };
        assert!(matches!(
            store
                .acquire_work_lease(
                    &ProjectId("project-a".into()),
                    &session_b.status.session_id,
                    &session_b.connection_token,
                    &session_b.routing_token,
                    crate::domain::LeaseKind::Execution,
                    crate::domain::LeaseMode::Exclusive,
                    &decomposed,
                    300,
                    "lease-alias-b",
                    now + TimeDelta::seconds(5),
                )
                .unwrap(),
            WorkLeaseDecision::Defer { .. }
        ));
        let wrong_project = crate::domain::ResourceSubject::Path {
            project_id: ProjectId("project-b".into()),
            segments: vec!["src".into()],
            coverage: crate::domain::ResourceCoverage::Tree,
        };
        assert!(matches!(
            store.acquire_work_lease(
                &ProjectId("project-a".into()),
                &session_b.status.session_id,
                &session_b.connection_token,
                &session_b.routing_token,
                crate::domain::LeaseKind::Execution,
                crate::domain::LeaseMode::Exclusive,
                &wrong_project,
                300,
                "lease-wrong-project",
                now + TimeDelta::seconds(6),
            ),
            Err(StoreError::InvalidControlSession(_))
        ));
        assert!(matches!(
            store.bind_control_session(
                &ProjectId("project-a".into()),
                "dummy:OTHER-TASK",
                "A different task",
                &session_a.status.session_id,
                &session_a.connection_token,
                &actor("alias-a"),
                ControlAssurance::TurnGated,
                &effects,
                1,
                "rebind-alias-a",
                now + TimeDelta::seconds(7),
            ),
            Err(StoreError::InvalidControlSession(_))
        ));
        assert_eq!(
            store
                .control_status(
                    &ProjectId("project-a".into()),
                    &session_a.status.session_id,
                    &session_a.connection_token,
                    &session_a.routing_token,
                    now + TimeDelta::seconds(8),
                )
                .unwrap()
                .task_id,
            session_a.status.task_id
        );
        let rebound = store
            .bind_control_session(
                &ProjectId("project-a".into()),
                "dummy:OTHER-TASK",
                "A different task",
                &session_a.status.session_id,
                &session_a.connection_token,
                &actor("alias-a"),
                ControlAssurance::TurnGated,
                &effects,
                1,
                "rebind-after-expiry",
                now + TimeDelta::seconds(304),
            )
            .unwrap();
        assert_ne!(rebound.status.task_id, session_a.status.task_id);
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT state FROM control_work_leases WHERE lease_id = ?1",
                    [&lease.lease_id],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "expired"
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the exit regression preserves synchronization, acquisition, checkpoint terminalization, and fenced reacquisition order"
    )]
    fn exiting_a_control_session_releases_its_leases_for_the_next_holder() {
        let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
        let mut store = SqliteStore::open_in_memory().unwrap();
        let effects = [EffectClass::Observe, EffectClass::MutateLocal];
        let session_a = bind_control_for(&mut store, "exit-a", "bind-exit-a", &effects, now);
        let session_b = bind_control_for(&mut store, "exit-b", "bind-exit-b", &effects, now);
        complete_control_turn(
            &mut store,
            &session_a,
            "sync-exit-a",
            vec![EffectClass::Observe],
            Vec::new(),
            now + TimeDelta::seconds(1),
        );
        complete_control_turn(
            &mut store,
            &session_b,
            "sync-exit-b",
            vec![EffectClass::Observe],
            Vec::new(),
            now + TimeDelta::seconds(2),
        );
        complete_control_turn(
            &mut store,
            &session_a,
            "resync-exit-a",
            vec![EffectClass::Observe],
            Vec::new(),
            now + TimeDelta::seconds(3),
        );
        let subject = crate::domain::ResourceSubject::Path {
            project_id: ProjectId("project-a".into()),
            segments: vec!["src".into()],
            coverage: crate::domain::ResourceCoverage::Tree,
        };
        let WorkLeaseDecision::Granted { lease: first } = store
            .acquire_work_lease(
                &ProjectId("project-a".into()),
                &session_a.status.session_id,
                &session_a.connection_token,
                &session_a.routing_token,
                crate::domain::LeaseKind::Execution,
                crate::domain::LeaseMode::Exclusive,
                &subject,
                300,
                "lease-before-exit",
                now + TimeDelta::seconds(4),
            )
            .unwrap()
        else {
            panic!("the first session must acquire the lease");
        };
        let decision = store
            .evaluate_control_turn(
                &ProjectId("project-a".into()),
                &session_a.status.session_id,
                &session_a.connection_token,
                &session_a.routing_token,
                &TurnIntent {
                    idempotency_key: "turn-before-exit".into(),
                    intent_fingerprint: ObjectHash::from_canonical_bytes(b"turn-before-exit"),
                    purpose: crate::domain::TurnPurpose::Ordinary,
                    requested_effects: vec![EffectClass::Observe],
                    resource_intents: Vec::new(),
                },
                now + TimeDelta::seconds(5),
            )
            .unwrap();
        let ControlTurnDecision::Grant { grant } = decision else {
            panic!("the exit turn must grant");
        };
        let delivery_tokens = grant
            .delivery
            .iter()
            .map(|delivery| delivery.page.delivery_token.clone())
            .collect::<Vec<_>>();
        assert!(matches!(
            store
                .begin_control_turn(
                    &ProjectId("project-a".into()),
                    &session_a.status.session_id,
                    &session_a.connection_token,
                    &session_a.routing_token,
                    &grant.grant_id,
                    &delivery_tokens,
                    "begin-before-exit",
                    now + TimeDelta::seconds(6),
                )
                .unwrap(),
            ControlTurnBeginDecision::Begin { .. }
        ));
        assert!(matches!(
            store
                .checkpoint_control_turn(
                    &ProjectId("project-a".into()),
                    &session_a.status.session_id,
                    &session_a.connection_token,
                    &session_a.routing_token,
                    &grant.grant_id,
                    TurnNextIntent::Exit,
                    "checkpoint-exit",
                    now + TimeDelta::seconds(7),
                )
                .unwrap(),
            ControlTurnCheckpointDecision::Checkpointed {
                receipt: TurnCheckpointReceipt {
                    phase: SessionPhase::Exited,
                    ..
                }
            }
        ));
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT state FROM control_work_leases WHERE lease_id = ?1",
                    [&first.lease_id],
                    |row| row.get::<_, String>(0),
                )
                .unwrap(),
            "released"
        );
        complete_control_turn(
            &mut store,
            &session_b,
            "sync-after-exit",
            vec![EffectClass::Observe],
            Vec::new(),
            now + TimeDelta::seconds(8),
        );
        let WorkLeaseDecision::Granted { lease: second } = store
            .acquire_work_lease(
                &ProjectId("project-a".into()),
                &session_b.status.session_id,
                &session_b.connection_token,
                &session_b.routing_token,
                crate::domain::LeaseKind::Execution,
                crate::domain::LeaseMode::Exclusive,
                &subject,
                300,
                "lease-after-exit",
                now + TimeDelta::seconds(9),
            )
            .unwrap()
        else {
            panic!("the second session must acquire the released scope");
        };
        assert_eq!(second.fence, first.fence + 1);
    }
}
