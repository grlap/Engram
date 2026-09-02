//! First-class local work graph persistence.

#![allow(
    clippy::too_many_lines,
    reason = "work lifecycle transactions stay contiguous so their atomic invariants remain auditable"
)]

use std::collections::{HashMap, HashSet};

#[cfg(test)]
use std::cell::Cell;

use chrono::{DateTime, Utc};
use rusqlite::{
    Connection, OptionalExtension, Transaction, TransactionBehavior, params, types::Value,
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};
use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;

use super::{
    BeginGateWorkProtocolAttempt, BeginWorkProtocolAttempt, SchemaDurability, SchemaOwner,
    SqliteStore, StoreError, WorkAuthorityGrantStatus,
};
use crate::{
    CanonicalObject, ObjectHash,
    domain::{
        AcceptWorkHandoffRequest, AcceptanceResult, ActorContext, AddWorkBlockerRequest,
        COMPLETION_ENVIRONMENT_SCHEMA_VERSION, COMPLETION_OBLIGATION_SCHEMA_VERSION,
        CancelWorkHandoffRequest, ChangeWorkPrerequisiteRequest, ChildRequirement,
        ClaimWorkRequest, ClearWorkBlockerRequest, CompleteWorkRequest,
        CompletionObligationBinding, CompletionSeal, CompletionWaiver, ControlWorkBinding,
        CreateWorkRequest, DEFAULT_WORK_CLAIM_TTL_SECONDS, DecomposeWorkRequest,
        DisposeWorkRequest, EnvironmentEvidence, ExecutionObservation, FeedId, FeedPosition,
        GATE_EVIDENCE_SUMMARY, GateEvidenceRecord, LifecycleAuthorityDecision,
        MemoryAssertionEvent, MemoryVersion, OfferWorkHandoffRequest, OpenWorkObligation,
        ReadyWork, RecordGateEvidenceRequest, RecordWorkEvidenceRequest, RecordWorkNoteRequest,
        ReleaseWorkRequest, ReopenWorkRequest, RequiredChildWaiver, ReviseWorkRequest,
        RootContribution, RootExecution, RootExecutionId, RootExecutionState, SCHEMA_VERSION,
        SessionId, TaskId, VerificationEvidence, WaiveRequiredChildRequest,
        WaiveWorkObligationRequest, WorkAuthorityGrant, WorkAuthorityOperation,
        WorkAuthorityRevocation, WorkAuthorityScope, WorkAvailability, WorkBlocker,
        WorkCatalogPage, WorkCatalogQuery, WorkCheckpoint, WorkClaim, WorkClaimId, WorkClaimState,
        WorkCompletionRecovery, WorkCompletionRecoveryCause, WorkDecomposition, WorkDependencyRef,
        WorkDisposition, WorkEvent, WorkEvidence, WorkEvidenceKind, WorkFeedEntry,
        WorkHandoffOffer, WorkHandoffOfferId, WorkHandoffState, WorkId, WorkItem, WorkLifecycle,
        WorkObligation, WorkObligationId, WorkObligationResolution, WorkObligationResolutionEvent,
        WorkObligationState, WorkObligationWaiverDecision, WorkObligationWaiverReceipt,
        WorkObligationWaiverRefusalCode, WorkOrigin, WorkPlanningAuthority, WorkPlanningBudget,
        WorkPrerequisiteState, WorkReadinessReason, WorkReferenceCandidate, WorkRun, WorkRunId,
        WorkRunState, WorkSessionState, WorkSourceSnapshot, WorkTransition,
        normalize_gate_evidence_input, validate_gate_evidence_payload,
    },
    memory::Redactor,
    schema::WORK_SCHEMA_VERSION as CURRENT_WORK_SCHEMA_VERSION,
};

const MAX_WORK_TTL_SECONDS: i64 = 86_400;
const MAX_WORK_SOURCE_SNAPSHOT_BYTES: usize = 128 * 1_024;
const REBUILDABLE_WORK_SCHEMA_OBJECTS: &[&str] = &[
    "work_catalog_fts",
    "objects_work_authority_revocation_grant",
    "objects_work_evidence_gate_name",
    "objects_work_event_work_id",
    "work_feed_entries_work_event_item",
    "work_feed_entries_environment_cut",
    "work_authority_grants_active",
    "work_blockers_active",
    "work_claims_live",
    "work_handoff_offer_active",
    "work_items_parent",
    "work_items_assigned",
    "work_items_catalog_after",
    "work_items_ready",
    "work_items_root",
    "work_item_labels_lookup",
    "work_prerequisites_reverse",
    "work_root_execution_active",
    "work_run_active",
    "work_run_evidence_run",
    "work_run_obligations_run",
    "work_feed_entries_require_work_id",
];

pub(super) fn owns_schema_object(name: &str) -> bool {
    name.starts_with("work_") || name.starts_with("objects_work_")
}

pub(super) fn is_rebuildable_schema_object(name: &str) -> bool {
    name == "work_catalog_fts"
        || name.starts_with("work_catalog_fts_")
        || REBUILDABLE_WORK_SCHEMA_OBJECTS.contains(&name)
}

// A checkpoint acknowledges the run feed immediately before its own object and
// its matching checkpoint event are appended.
const CHECKPOINT_APPEND_COUNT: i64 = 2;
const MAX_OPEN_COMPLETION_OBLIGATIONS: usize = 16;
const MAX_COMPLETION_ENVIRONMENT_EVIDENCE: usize = 64;
const PROJECTED_WORK_AVAILABILITY_SQL: &str = r"
    CASE
        WHEN candidate.lifecycle != 'open' THEN 'closed'
        WHEN NOT EXISTS (
            SELECT 1
            FROM work_runs run
            JOIN work_root_executions execution
              ON execution.root_execution_id = run.root_execution_id
            WHERE run.run_id = candidate.active_run_id
              AND run.work_id = candidate.work_id
              AND execution.project_id = candidate.project_id
              AND execution.root_id = candidate.root_id
              AND execution.state = 'active'
        ) OR EXISTS (
            WITH RECURSIVE ancestors(work_id, parent_id, lifecycle) AS (
                SELECT parent.work_id, parent.parent_id, parent.lifecycle
                FROM work_items parent
                WHERE parent.work_id = candidate.parent_id
                UNION ALL
                SELECT parent.work_id, parent.parent_id, parent.lifecycle
                FROM work_items parent
                JOIN ancestors ON parent.work_id = ancestors.parent_id
            )
            SELECT 1 FROM ancestors WHERE lifecycle != 'open'
        ) THEN 'blocked'
        WHEN candidate.deferred_until_ms IS NOT NULL
          AND candidate.deferred_until_ms > ?2 THEN 'deferred'
        WHEN EXISTS (
            SELECT 1 FROM work_blockers blocker
            WHERE blocker.work_id = candidate.work_id AND blocker.state = 'active'
        ) OR EXISTS (
            SELECT 1
            FROM work_prerequisites edge
            LEFT JOIN work_items prerequisite
              ON prerequisite.work_id = edge.prerequisite_id
            LEFT JOIN work_items replacement
              ON replacement.work_id = prerequisite.superseded_by
            WHERE edge.work_id = candidate.work_id
              AND (
                  prerequisite.work_id IS NULL
                  OR NOT (
                      prerequisite.lifecycle = 'completed'
                      OR (
                          prerequisite.lifecycle = 'superseded'
                          AND replacement.lifecycle = 'completed'
                      )
                  )
              )
        ) THEN 'blocked'
        WHEN EXISTS (
            SELECT 1 FROM work_claims claim
            WHERE claim.run_id = candidate.active_run_id
              AND claim.state = 'active'
              AND claim.expires_at_ms > ?2
        ) THEN CASE WHEN EXISTS (
            SELECT 1 FROM work_runs run
            WHERE run.run_id = candidate.active_run_id
              AND run.last_checkpoint_hash IS NOT NULL
        ) THEN 'active' ELSE 'claimed' END
        ELSE 'ready'
    END
";
const PROJECTED_WORK_HAS_BLOCKER_SQL: &str = r"
    EXISTS (
        SELECT 1 FROM work_blockers blocker
        WHERE blocker.work_id = candidate.work_id AND blocker.state = 'active'
    ) OR EXISTS (
        SELECT 1
        FROM work_prerequisites edge
        LEFT JOIN work_items prerequisite
          ON prerequisite.work_id = edge.prerequisite_id
        LEFT JOIN work_items replacement
          ON replacement.work_id = prerequisite.superseded_by
        WHERE edge.work_id = candidate.work_id
          AND (
              prerequisite.work_id IS NULL
              OR NOT (
                  prerequisite.lifecycle = 'completed'
                  OR (
                      prerequisite.lifecycle = 'superseded'
                      AND replacement.lifecycle = 'completed'
                  )
              )
          )
    )
";

#[derive(Clone, Copy)]
pub(crate) struct StageWorkSessionDelivery<'a> {
    pub expected_confirmed_through: i64,
    pub expected_focused_work_id: Option<WorkId>,
    pub expected_bound_task_id: Option<TaskId>,
    pub delivered_through: i64,
    pub delivered_entries: &'a [WorkFeedEntry],
    pub delivery_payload: &'a CanonicalObject,
    pub now: DateTime<Utc>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
struct EvidenceProjectionRow {
    work_id: String,
    run_id: String,
    evidence_kind: String,
    workspace_id: Option<String>,
    source_revision: Option<String>,
    producer_session_id: Option<String>,
    producer_observation_hash: Option<String>,
    check_fingerprint: Option<String>,
    verification_result: Option<String>,
    observed_at_ms: Option<i64>,
    environment_fingerprint: Option<String>,
    environment_evidence_hash: Option<String>,
    components_json: Option<Vec<u8>>,
}

#[derive(Clone, Debug)]
struct ObligationProjectionRow {
    obligation_id: String,
    definition_hash: String,
    project_id: String,
    root_execution_id: String,
    root_id: String,
    work_id: String,
    run_id: String,
    work_revision: i64,
    rule_set_hash: String,
    rule_id: String,
    rule_version: i64,
    triggering_observation_hash: String,
    trigger_position: i64,
    check_kind: String,
    check_fingerprint: Option<String>,
    state: String,
    resolution_hash: Option<String>,
    resolution_kind: Option<String>,
    evidence_hash: Option<String>,
    opened_at_ms: i64,
    resolved_at_ms: Option<i64>,
}

#[derive(Serialize)]
struct WorkObligationWaiverFingerprint<'a> {
    obligation_id: WorkObligationId,
    expected_definition: &'a ObjectHash,
    waived_by: &'a str,
    reason: &'a str,
    authority: &'a LifecycleAuthorityDecision,
    actor: &'a crate::domain::ActorContext,
    idempotency_key: &'a str,
}

#[derive(Serialize)]
struct ControlWorkObligationWaiverFingerprint<'a> {
    control_schema_version: u16,
    session_id: &'a SessionId,
    bind_intent_hash: &'a str,
    obligation_id: WorkObligationId,
    expected_definition: &'a ObjectHash,
    authority_grant: &'a ObjectHash,
    waived_by: &'a str,
    reason: &'a str,
    idempotency_key: &'a str,
}

#[derive(Clone, Debug, Serialize)]
struct WorkRelationBasis {
    schema_version: u16,
    prerequisite_ids: Vec<WorkId>,
    active_blockers: Vec<WorkRelationBlockerBasis>,
}

#[derive(Clone, Debug, Serialize)]
struct WorkRelationBlockerBasis {
    blocker_id: String,
    blocker_hash: ObjectHash,
}

#[derive(Clone, Debug)]
struct WorkEventDraft {
    schema_version: u16,
    project_id: crate::domain::ProjectId,
    root_id: WorkId,
    work_id: WorkId,
    run_id: Option<WorkRunId>,
    revision: i64,
    work: WorkItem,
    run: Option<WorkRun>,
    root_execution: Option<RootExecution>,
    claim: Option<WorkClaim>,
    handoff_offer: Option<WorkHandoffOffer>,
    blocker: Option<WorkBlocker>,
    transition: WorkTransition,
    actor: crate::domain::ActorContext,
    created_at: DateTime<Utc>,
}

impl WorkEventDraft {
    fn finalize(self, relation_fingerprint: ObjectHash) -> WorkEvent {
        WorkEvent {
            schema_version: self.schema_version,
            project_id: self.project_id,
            root_id: self.root_id,
            work_id: self.work_id,
            run_id: self.run_id,
            revision: self.revision,
            work: self.work,
            run: self.run,
            root_execution: self.root_execution,
            claim: self.claim,
            handoff_offer: self.handoff_offer,
            blocker: self.blocker,
            relation_fingerprint,
            transition: self.transition,
            actor: self.actor,
            created_at: self.created_at,
        }
    }
}

#[cfg(test)]
impl From<&WorkEvent> for WorkEventDraft {
    fn from(event: &WorkEvent) -> Self {
        Self {
            schema_version: event.schema_version,
            project_id: event.project_id.clone(),
            root_id: event.root_id,
            work_id: event.work_id,
            run_id: event.run_id,
            revision: event.revision,
            work: event.work.clone(),
            run: event.run.clone(),
            root_execution: event.root_execution.clone(),
            claim: event.claim.clone(),
            handoff_offer: event.handoff_offer.clone(),
            blocker: event.blocker.clone(),
            transition: event.transition.clone(),
            actor: event.actor.clone(),
            created_at: event.created_at,
        }
    }
}

fn empty_work_relation_basis() -> WorkRelationBasis {
    WorkRelationBasis {
        schema_version: SCHEMA_VERSION,
        prerequisite_ids: Vec::new(),
        active_blockers: Vec::new(),
    }
}

/// Hash-verified immutable obligation plus its durable terminal-state projection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkObligationRecord {
    pub definition_hash: ObjectHash,
    pub obligation: WorkObligation,
    pub state: WorkObligationState,
    pub resolution_hash: Option<ObjectHash>,
    pub resolution: Option<WorkObligationResolutionEvent>,
    pub resolution_position: Option<FeedPosition>,
}

/// Hash-verified evidence selection basis used to choose a bounded focus page.
/// The loader derives these fields from canonical bytes and rejects any
/// disagreement with the durable run projection before selection.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct WorkEvidenceProjectionSummary {
    pub hash: ObjectHash,
    pub kind: WorkEvidenceKind,
    pub environment: Option<ObjectHash>,
}

struct StoredWorkEvidenceSelectionRow {
    hash: String,
    projected_work: String,
    projected_run: String,
    projected_kind: String,
    projected_environment: Option<String>,
    object_kind: Option<String>,
    canonical_json: Option<Vec<u8>>,
}

#[cfg(test)]
thread_local! {
    static WORK_EVENT_DECODE_COUNT: Cell<usize> = const { Cell::new(0) };
    static WORK_ITEM_PROJECTION_DECODE_COUNT: Cell<usize> = const { Cell::new(0) };
}

#[cfg(test)]
pub(crate) fn reset_work_event_decode_count() {
    WORK_EVENT_DECODE_COUNT.with(|count| count.set(0));
}

#[cfg(test)]
pub(crate) fn work_event_decode_count() -> usize {
    WORK_EVENT_DECODE_COUNT.with(Cell::get)
}

#[cfg(test)]
pub(crate) fn reset_work_item_projection_decode_count() {
    WORK_ITEM_PROJECTION_DECODE_COUNT.with(|count| count.set(0));
}

#[cfg(test)]
pub(crate) fn work_item_projection_decode_count() -> usize {
    WORK_ITEM_PROJECTION_DECODE_COUNT.with(Cell::get)
}

pub(super) fn preflight_schema(
    connection: &Connection,
    allow_initialization: bool,
) -> Result<(), StoreError> {
    let metadata_exists = connection.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM sqlite_master
             WHERE type = 'table' AND name = 'work_schema_metadata'
         )",
        [],
        |row| row.get::<_, bool>(0),
    )?;
    if !metadata_exists {
        let existing_work_tables = connection.query_row(
            "SELECT COUNT(*) FROM sqlite_master
             WHERE type = 'table' AND name GLOB 'work_*'",
            [],
            |row| row.get::<_, i64>(0),
        )?;
        if existing_work_tables == 0 && allow_initialization {
            return Ok(());
        }
        if existing_work_tables == 0 {
            return Err(super::different_build_store_error());
        }
        return Err(StoreError::InvalidWorkProjection(
            "local-work tables exist without schema metadata".into(),
        ));
    }
    if current_work_durable_schema_issue(connection)?.is_some() {
        return Err(super::different_build_store_error());
    }
    let version = connection.query_row(
        "SELECT schema_version FROM work_schema_metadata WHERE singleton = 1",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    super::require_current_schema_marker(version, CURRENT_WORK_SCHEMA_VERSION)
}

fn current_work_durable_schema_issue(
    connection: &Connection,
) -> Result<Option<String>, StoreError> {
    super::current_schema_definition_issue(connection, SchemaOwner::Work, SchemaDurability::Durable)
}

fn current_work_rebuildable_schema_issue(
    connection: &Connection,
) -> Result<Option<String>, StoreError> {
    super::current_schema_definition_issue(
        connection,
        SchemaOwner::Work,
        SchemaDurability::Rebuildable,
    )
}

pub(super) fn schema_version(connection: &Connection) -> Result<i64, StoreError> {
    connection
        .query_row(
            "SELECT schema_version FROM work_schema_metadata WHERE singleton = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .ok_or_else(|| {
            StoreError::InvalidWorkProjection(
                "local-work schema metadata has no singleton row".into(),
            )
        })
}

pub(super) fn require_work_schema_version(
    connection: &Connection,
    expected_version: i64,
) -> Result<(), StoreError> {
    let version = schema_version(connection)?;
    super::require_current_schema_marker(version, expected_version)
}

pub(super) fn initialize_schema(
    connection: &mut Connection,
    allow_initialization: bool,
) -> Result<(), StoreError> {
    preflight_schema(connection, allow_initialization)?;
    if current_work_schema_is_complete(connection)? {
        return Ok(());
    }
    let metadata_exists = connection.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM sqlite_master
             WHERE type = 'table' AND name = 'work_schema_metadata'
         )",
        [],
        |row| row.get::<_, bool>(0),
    )?;
    if metadata_exists {
        return Err(StoreError::InvalidWorkProjection(
            "current local-work schema is missing rebuildable projections; run `engram doctor --repair-projections` explicitly"
                .into(),
        ));
    }
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    preflight_schema(&transaction, allow_initialization)?;
    transaction.execute_batch(
        "CREATE TABLE IF NOT EXISTS work_schema_metadata (
             singleton INTEGER PRIMARY KEY CHECK(singleton = 1),
             schema_version INTEGER NOT NULL CHECK(schema_version > 0)
         ) STRICT;
         CREATE TABLE IF NOT EXISTS work_authority_grants (
             grant_hash TEXT PRIMARY KEY REFERENCES objects(object_hash),
             project_id TEXT NOT NULL,
             policy_ref TEXT NOT NULL,
             subject_actor_id TEXT NOT NULL,
             valid_until_ms INTEGER NOT NULL,
             revoked_at_ms INTEGER,
             grant_json BLOB NOT NULL
         ) STRICT;
         CREATE INDEX IF NOT EXISTS work_authority_grants_active
             ON work_authority_grants(project_id, policy_ref, subject_actor_id, valid_until_ms)
             WHERE revoked_at_ms IS NULL;
         CREATE TABLE IF NOT EXISTS work_authority_revocations (
             grant_hash TEXT PRIMARY KEY REFERENCES work_authority_grants(grant_hash),
             revocation_hash TEXT NOT NULL UNIQUE REFERENCES objects(object_hash),
             revoked_at_ms INTEGER NOT NULL,
             revocation_json BLOB NOT NULL
         ) STRICT;
         CREATE TABLE IF NOT EXISTS work_items (
             work_id TEXT PRIMARY KEY,
             project_id TEXT NOT NULL,
             short_ref TEXT NOT NULL,
             root_id TEXT NOT NULL REFERENCES work_items(work_id),
             parent_id TEXT REFERENCES work_items(work_id),
             child_requirement TEXT NOT NULL,
             lifecycle TEXT NOT NULL,
             priority INTEGER NOT NULL,
             assigned_to TEXT,
             assigned_to_key TEXT,
             search_text_key TEXT NOT NULL DEFAULT '',
             deferred_until_ms INTEGER,
             revision INTEGER NOT NULL,
             active_run_id TEXT,
             superseded_by TEXT REFERENCES work_items(work_id),
             source_snapshot_hash TEXT REFERENCES objects(object_hash),
             latest_event_hash TEXT REFERENCES objects(object_hash),
             created_at_ms INTEGER NOT NULL,
             updated_at_ms INTEGER NOT NULL,
             item_json BLOB NOT NULL,
             UNIQUE(project_id, short_ref),
             CHECK(priority BETWEEN 0 AND 4),
             CHECK(revision > 0)
         ) STRICT;
         CREATE INDEX IF NOT EXISTS work_items_ready
             ON work_items(project_id, lifecycle, priority, deferred_until_ms, created_at_ms);
         CREATE INDEX IF NOT EXISTS work_items_parent
             ON work_items(parent_id, lifecycle);
         CREATE INDEX IF NOT EXISTS work_items_root
             ON work_items(project_id, root_id, work_id);
         CREATE TABLE IF NOT EXISTS work_item_labels (
             work_id TEXT NOT NULL REFERENCES work_items(work_id) ON DELETE CASCADE,
             label_key TEXT NOT NULL,
             PRIMARY KEY(work_id, label_key)
         ) STRICT;
         CREATE INDEX IF NOT EXISTS work_item_labels_lookup
             ON work_item_labels(label_key, work_id);
         CREATE VIRTUAL TABLE IF NOT EXISTS work_catalog_fts USING fts5(
             work_id UNINDEXED,
             search_text,
             tokenize='trigram'
         );
         CREATE TABLE IF NOT EXISTS work_root_executions (
             root_execution_id TEXT PRIMARY KEY,
             project_id TEXT NOT NULL,
             root_id TEXT NOT NULL REFERENCES work_items(work_id),
             generation INTEGER NOT NULL,
             state TEXT NOT NULL,
             revision INTEGER NOT NULL,
             created_at_ms INTEGER NOT NULL,
             updated_at_ms INTEGER NOT NULL,
             execution_json BLOB NOT NULL,
             UNIQUE(root_id, generation)
         ) STRICT;
         CREATE UNIQUE INDEX IF NOT EXISTS work_root_execution_active
             ON work_root_executions(root_id) WHERE state = 'active';
         CREATE TABLE IF NOT EXISTS work_runs (
             run_id TEXT PRIMARY KEY,
             root_execution_id TEXT NOT NULL REFERENCES work_root_executions(root_execution_id),
             work_id TEXT NOT NULL REFERENCES work_items(work_id),
             generation INTEGER NOT NULL,
             executor_session_id TEXT,
             state TEXT NOT NULL,
             revision INTEGER NOT NULL,
             claim_fence_head INTEGER NOT NULL DEFAULT 0,
             last_checkpoint_hash TEXT REFERENCES objects(object_hash),
             completion_seal_hash TEXT REFERENCES objects(object_hash),
             created_at_ms INTEGER NOT NULL,
             updated_at_ms INTEGER NOT NULL,
             run_json BLOB NOT NULL,
             UNIQUE(work_id, generation),
             CHECK(revision > 0),
             CHECK(claim_fence_head >= 0)
         ) STRICT;
         CREATE UNIQUE INDEX IF NOT EXISTS work_run_active
             ON work_runs(work_id) WHERE state != 'completed' AND state != 'cancelled';
         CREATE TABLE IF NOT EXISTS work_claims (
             run_id TEXT PRIMARY KEY REFERENCES work_runs(run_id),
             work_id TEXT NOT NULL REFERENCES work_items(work_id),
             claim_id TEXT NOT NULL UNIQUE,
             holder_session_id TEXT NOT NULL,
             state TEXT NOT NULL,
             expires_at_ms INTEGER NOT NULL,
             revision INTEGER NOT NULL,
             fence INTEGER NOT NULL,
             claim_json BLOB NOT NULL,
             CHECK(revision > 0),
             CHECK(fence > 0)
         ) STRICT;
         CREATE INDEX IF NOT EXISTS work_claims_live
             ON work_claims(work_id, state, expires_at_ms);
         CREATE TABLE IF NOT EXISTS work_handoff_offers (
             offer_id TEXT PRIMARY KEY,
             run_id TEXT NOT NULL REFERENCES work_runs(run_id),
             work_id TEXT NOT NULL REFERENCES work_items(work_id),
             state TEXT NOT NULL,
             expires_at_ms INTEGER NOT NULL,
             offer_hash TEXT REFERENCES objects(object_hash),
             offer_json BLOB NOT NULL
         ) STRICT;
         CREATE UNIQUE INDEX IF NOT EXISTS work_handoff_offer_active
             ON work_handoff_offers(run_id) WHERE state = 'offered';
         CREATE TABLE IF NOT EXISTS work_prerequisites (
             work_id TEXT NOT NULL REFERENCES work_items(work_id),
             prerequisite_id TEXT NOT NULL REFERENCES work_items(work_id),
             event_hash TEXT NOT NULL REFERENCES objects(object_hash),
             PRIMARY KEY(work_id, prerequisite_id),
             CHECK(work_id != prerequisite_id)
         ) STRICT;
         CREATE INDEX IF NOT EXISTS work_prerequisites_reverse
             ON work_prerequisites(prerequisite_id, work_id);
         CREATE TABLE IF NOT EXISTS work_blockers (
             blocker_id TEXT PRIMARY KEY,
             work_id TEXT NOT NULL REFERENCES work_items(work_id),
             state TEXT NOT NULL,
             blocker_json BLOB NOT NULL,
             created_event_hash TEXT NOT NULL REFERENCES objects(object_hash),
             cleared_event_hash TEXT REFERENCES objects(object_hash)
         ) STRICT;
         CREATE INDEX IF NOT EXISTS work_blockers_active
             ON work_blockers(work_id, state);
         CREATE TABLE IF NOT EXISTS work_run_evidence (
             evidence_hash TEXT PRIMARY KEY REFERENCES objects(object_hash),
             work_id TEXT NOT NULL REFERENCES work_items(work_id),
             run_id TEXT NOT NULL REFERENCES work_runs(run_id),
             evidence_kind TEXT NOT NULL DEFAULT 'generic',
             workspace_id TEXT,
             source_revision TEXT,
             producer_session_id TEXT,
             producer_observation_hash TEXT REFERENCES objects(object_hash),
             check_fingerprint TEXT,
             verification_result TEXT,
             observed_at_ms INTEGER,
             environment_fingerprint TEXT,
             environment_evidence_hash TEXT REFERENCES objects(object_hash),
             components_json BLOB
         ) STRICT;
         CREATE INDEX IF NOT EXISTS work_run_evidence_run
             ON work_run_evidence(run_id, evidence_hash);
         CREATE TABLE IF NOT EXISTS work_run_obligations (
             obligation_id TEXT PRIMARY KEY,
             definition_hash TEXT NOT NULL UNIQUE REFERENCES objects(object_hash),
             project_id TEXT NOT NULL,
             root_execution_id TEXT NOT NULL REFERENCES work_root_executions(root_execution_id),
             root_id TEXT NOT NULL REFERENCES work_items(work_id),
             work_id TEXT NOT NULL REFERENCES work_items(work_id),
             run_id TEXT NOT NULL REFERENCES work_runs(run_id),
             work_revision INTEGER NOT NULL,
             rule_set_hash TEXT NOT NULL REFERENCES objects(object_hash),
             rule_id TEXT NOT NULL,
             rule_version INTEGER NOT NULL,
             triggering_observation_hash TEXT NOT NULL REFERENCES objects(object_hash),
             trigger_position INTEGER NOT NULL,
             check_kind TEXT NOT NULL,
             check_fingerprint TEXT,
             state TEXT NOT NULL,
             resolution_hash TEXT UNIQUE REFERENCES objects(object_hash),
             resolution_kind TEXT,
             evidence_hash TEXT REFERENCES objects(object_hash),
             opened_at_ms INTEGER NOT NULL,
             resolved_at_ms INTEGER,
             UNIQUE(run_id, rule_id, rule_version, triggering_observation_hash),
             CHECK(work_revision > 0),
             CHECK(rule_version > 0),
             CHECK(trigger_position > 0)
         ) STRICT;
         CREATE INDEX IF NOT EXISTS work_run_obligations_run
             ON work_run_obligations(run_id, state, trigger_position, obligation_id);
         CREATE TABLE IF NOT EXISTS work_completion_seals (
             seal_hash TEXT PRIMARY KEY REFERENCES objects(object_hash),
             work_id TEXT NOT NULL REFERENCES work_items(work_id),
             run_id TEXT NOT NULL UNIQUE REFERENCES work_runs(run_id),
             root_execution_id TEXT NOT NULL REFERENCES work_root_executions(root_execution_id),
             seal_json BLOB NOT NULL
         ) STRICT;
         CREATE TABLE IF NOT EXISTS work_feed_heads (
             feed_kind TEXT NOT NULL,
             feed_id TEXT NOT NULL,
             position INTEGER NOT NULL,
             PRIMARY KEY(feed_kind, feed_id),
             CHECK(position >= 0)
         ) STRICT;
         CREATE TABLE IF NOT EXISTS work_feed_entries (
             feed_kind TEXT NOT NULL,
             feed_id TEXT NOT NULL,
             position INTEGER NOT NULL,
             object_kind TEXT NOT NULL,
             object_hash TEXT NOT NULL REFERENCES objects(object_hash),
             work_id TEXT REFERENCES work_items(work_id),
             PRIMARY KEY(feed_kind, feed_id, position),
             UNIQUE(feed_kind, feed_id, object_hash),
             FOREIGN KEY(feed_kind, feed_id)
                 REFERENCES work_feed_heads(feed_kind, feed_id)
         ) STRICT;
         CREATE INDEX IF NOT EXISTS objects_work_event_work_id
             ON objects(json_extract(canonical_json, '$.work_id'))
             WHERE object_kind = 'work_event';
         CREATE INDEX IF NOT EXISTS objects_work_evidence_gate_name
             ON objects(
                 json_extract(canonical_json, '$.run_id'),
                 json_extract(canonical_json, '$.gate.name')
             )
             WHERE object_kind = 'work_evidence'
               AND json_type(canonical_json, '$.gate') = 'object';
         CREATE INDEX IF NOT EXISTS objects_work_authority_revocation_grant
             ON objects(json_extract(canonical_json, '$.grant'))
             WHERE object_kind = 'work_authority_revocation';
         CREATE TABLE IF NOT EXISTS work_operation_results (
             operation TEXT NOT NULL,
             idempotency_key TEXT NOT NULL,
             request_hash TEXT NOT NULL,
             result_json BLOB NOT NULL,
             PRIMARY KEY(operation, idempotency_key)
         ) STRICT;
         CREATE TABLE IF NOT EXISTS work_session_state (
             project_id TEXT NOT NULL,
             session_id TEXT NOT NULL,
             focused_work_id TEXT REFERENCES work_items(work_id),
             project_cursor INTEGER NOT NULL DEFAULT 0 CHECK(project_cursor >= 0),
             tentative_project_cursor INTEGER CHECK(tentative_project_cursor >= 0),
             tentative_delivery_token TEXT,
             tentative_delivery_payload_hash TEXT,
             tentative_delivery_payload BLOB,
             updated_at_ms INTEGER NOT NULL,
             PRIMARY KEY(project_id, session_id)
         ) STRICT;
         CREATE TABLE IF NOT EXISTS work_protocol_attempts (
             project_id TEXT NOT NULL,
             session_id TEXT NOT NULL,
             operation TEXT NOT NULL,
             idempotency_key TEXT NOT NULL,
             request_hash TEXT NOT NULL,
             basis_hash TEXT,
             basis_json BLOB,
             initiated_at_ms INTEGER NOT NULL,
             result_hash TEXT REFERENCES objects(object_hash),
             result_json BLOB,
             PRIMARY KEY(project_id, session_id, operation, idempotency_key)
         ) STRICT;",
    )?;
    transaction.execute(
        "INSERT INTO work_schema_metadata (singleton, schema_version)
         VALUES (1, ?1) ON CONFLICT(singleton) DO NOTHING",
        [CURRENT_WORK_SCHEMA_VERSION],
    )?;
    transaction.execute_batch(
        "CREATE INDEX IF NOT EXISTS work_feed_entries_work_event_item
             ON work_feed_entries(feed_kind, work_id, position DESC)
             WHERE object_kind = 'work_event' AND work_id IS NOT NULL;
         CREATE INDEX IF NOT EXISTS work_feed_entries_environment_cut
             ON work_feed_entries(feed_id, position, object_hash)
             WHERE feed_kind = 'run_execution'
               AND object_kind = 'environment_evidence';
         CREATE TRIGGER IF NOT EXISTS work_feed_entries_require_work_id
             BEFORE INSERT ON work_feed_entries
             WHEN NEW.object_kind = 'work_event' AND NEW.work_id IS NULL
             BEGIN
                 SELECT RAISE(ABORT, 'work-event feed entry requires work_id');
             END;
         CREATE INDEX IF NOT EXISTS work_items_assigned
             ON work_items(project_id, assigned_to_key, work_id);
         CREATE INDEX IF NOT EXISTS work_items_catalog_after
             ON work_items(project_id, work_id);",
    )?;
    transaction.execute(
        "UPDATE work_schema_metadata SET schema_version = ?1 WHERE singleton = 1",
        [CURRENT_WORK_SCHEMA_VERSION],
    )?;
    transaction.commit()?;
    Ok(())
}

pub(super) fn repair_rebuildable_schema_on(connection: &Connection) -> Result<bool, StoreError> {
    preflight_schema(connection, false)?;
    for object in REBUILDABLE_WORK_SCHEMA_OBJECTS {
        super::drop_schema_object(connection, object)?;
    }
    connection.execute_batch(
        "CREATE VIRTUAL TABLE IF NOT EXISTS work_catalog_fts USING fts5(
             work_id UNINDEXED,
             search_text,
             tokenize='trigram'
         );
         CREATE INDEX IF NOT EXISTS work_authority_grants_active
             ON work_authority_grants(project_id, policy_ref, subject_actor_id, valid_until_ms)
             WHERE revoked_at_ms IS NULL;
         CREATE INDEX IF NOT EXISTS work_items_ready
             ON work_items(project_id, lifecycle, priority, deferred_until_ms, created_at_ms);
         CREATE INDEX IF NOT EXISTS work_items_parent
             ON work_items(parent_id, lifecycle);
         CREATE INDEX IF NOT EXISTS work_items_root
             ON work_items(project_id, root_id, work_id);
         CREATE INDEX IF NOT EXISTS work_item_labels_lookup
             ON work_item_labels(label_key, work_id);
         CREATE UNIQUE INDEX IF NOT EXISTS work_root_execution_active
             ON work_root_executions(root_id) WHERE state = 'active';
         CREATE UNIQUE INDEX IF NOT EXISTS work_run_active
             ON work_runs(work_id) WHERE state != 'completed' AND state != 'cancelled';
         CREATE INDEX IF NOT EXISTS work_claims_live
             ON work_claims(work_id, state, expires_at_ms);
         CREATE UNIQUE INDEX IF NOT EXISTS work_handoff_offer_active
             ON work_handoff_offers(run_id) WHERE state = 'offered';
         CREATE INDEX IF NOT EXISTS work_prerequisites_reverse
             ON work_prerequisites(prerequisite_id, work_id);
         CREATE INDEX IF NOT EXISTS work_blockers_active
             ON work_blockers(work_id, state);
         CREATE INDEX IF NOT EXISTS work_run_evidence_run
             ON work_run_evidence(run_id, evidence_hash);
         CREATE INDEX IF NOT EXISTS work_run_obligations_run
             ON work_run_obligations(run_id, state, trigger_position, obligation_id);
         CREATE INDEX IF NOT EXISTS objects_work_event_work_id
             ON objects(json_extract(canonical_json, '$.work_id'))
             WHERE object_kind = 'work_event';
         CREATE INDEX IF NOT EXISTS objects_work_evidence_gate_name
             ON objects(
                 json_extract(canonical_json, '$.run_id'),
                 json_extract(canonical_json, '$.gate.name')
             )
             WHERE object_kind = 'work_evidence'
               AND json_type(canonical_json, '$.gate') = 'object';
         CREATE INDEX IF NOT EXISTS objects_work_authority_revocation_grant
             ON objects(json_extract(canonical_json, '$.grant'))
             WHERE object_kind = 'work_authority_revocation';
         CREATE INDEX IF NOT EXISTS work_feed_entries_work_event_item
             ON work_feed_entries(feed_kind, work_id, position DESC)
             WHERE object_kind = 'work_event' AND work_id IS NOT NULL;
         CREATE INDEX IF NOT EXISTS work_feed_entries_environment_cut
             ON work_feed_entries(feed_id, position, object_hash)
             WHERE feed_kind = 'run_execution'
               AND object_kind = 'environment_evidence';
         CREATE TRIGGER IF NOT EXISTS work_feed_entries_require_work_id
             BEFORE INSERT ON work_feed_entries
             WHEN NEW.object_kind = 'work_event' AND NEW.work_id IS NULL
             BEGIN
                 SELECT RAISE(ABORT, 'work-event feed entry requires work_id');
             END;
         CREATE INDEX IF NOT EXISTS work_items_assigned
             ON work_items(project_id, assigned_to_key, work_id);
          CREATE INDEX IF NOT EXISTS work_items_catalog_after
             ON work_items(project_id, work_id);",
    )?;
    connection.execute("DELETE FROM work_catalog_fts", [])?;
    connection.execute(
        "INSERT INTO work_catalog_fts (work_id, search_text)
         SELECT work_id, search_text_key FROM work_items ORDER BY work_id",
        [],
    )?;
    let mut checked = 0;
    let mut invalid = Vec::new();
    verify_work_catalog_projections(connection, &mut checked, &mut invalid)?;
    if !invalid.is_empty() {
        return Err(StoreError::InvalidWorkProjection(format!(
            "explicit projection repair did not rebuild work_catalog_fts: {}",
            invalid.join(", ")
        )));
    }
    if current_work_rebuildable_schema_issue(connection)?.is_some() {
        return Err(StoreError::InvalidWorkProjection(
            "explicit local-work projection repair did not restore the current schema".into(),
        ));
    }
    Ok(true)
}

pub(super) fn current_work_schema_is_complete(connection: &Connection) -> Result<bool, StoreError> {
    let metadata_exists = connection.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM sqlite_master
             WHERE type = 'table' AND name = 'work_schema_metadata'
         )",
        [],
        |row| row.get::<_, bool>(0),
    )?;
    if !metadata_exists {
        return Ok(false);
    }
    let version = connection
        .query_row(
            "SELECT schema_version FROM work_schema_metadata WHERE singleton = 1",
            [],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    if version != Some(CURRENT_WORK_SCHEMA_VERSION) {
        return Ok(false);
    }
    Ok(current_work_durable_schema_issue(connection)?.is_none()
        && current_work_rebuildable_schema_issue(connection)?.is_none())
}

fn latest_canonical_handoff_offer(
    connection: &Connection,
    offer_id: &str,
    work_id: &str,
) -> Result<Option<WorkHandoffOffer>, StoreError> {
    let stored = connection
        .query_row(
            "SELECT entry.object_hash, object.object_kind, object.canonical_json
             FROM work_feed_entries entry
             JOIN objects object ON object.object_hash = entry.object_hash
             WHERE entry.feed_kind = 'project'
                AND entry.object_kind = 'work_event'
                AND entry.work_id = ?2
                AND json_extract(object.canonical_json, '$.handoff_offer.offer_id') = ?1
             ORDER BY entry.position DESC LIMIT 1",
            params![offer_id, work_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((stored_hash, object_kind, bytes)) = stored else {
        return Ok(None);
    };
    if object_kind != "work_event" {
        return Err(StoreError::InvalidWorkProjection(format!(
            "handoff offer {offer_id} is bound to a non-event canonical object"
        )));
    }
    let hash = ObjectHash::from_stored(stored_hash.clone())
        .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
    let event: WorkEvent = CanonicalObject::verify(&hash, bytes)?.decode()?;
    if event
        .handoff_offer
        .as_ref()
        .is_some_and(|offer| offer.offer_id.0.to_string() == offer_id)
    {
        Ok(event.handoff_offer)
    } else {
        Err(StoreError::InvalidWorkProjection(format!(
            "handoff offer {offer_id} is not bound to its canonical event"
        )))
    }
}

#[derive(Debug)]
pub(crate) struct WorkProtocolAttempt {
    pub(crate) result: Option<serde_json::Value>,
    pub(crate) basis_matches: bool,
    pub(crate) basis: Option<serde_json::Value>,
}

#[derive(Debug)]
pub(crate) struct GateWorkProtocolAttempt {
    pub(crate) evidence: ObjectHash,
    pub(crate) idempotency_key: String,
    pub(crate) result: Option<serde_json::Value>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct WorkNoteCapture {
    pub(crate) evidence: ObjectHash,
    pub(crate) checkpoint: ObjectHash,
}

pub(crate) struct WorkPrerequisitePage {
    pub(crate) items: Vec<(WorkItem, WorkPrerequisiteState)>,
    /// Omitted dead, pending, and satisfied entries, in that order.
    pub(crate) omitted_by_state: [usize; 3],
}

#[derive(Serialize)]
struct GateWorkProtocolIntent<'a> {
    schema_version: u16,
    project_id: &'a crate::domain::ProjectId,
    session_id: &'a SessionId,
    actor: &'a ActorContext,
    work_id: WorkId,
    run_id: WorkRunId,
    claim_id: WorkClaimId,
    claim_fence: i64,
    name: &'a str,
    failed: &'a [String],
    refs: &'a [String],
    previous: Option<&'a ObjectHash>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) enum CompleteWorkStorageResult {
    Completed(Box<CompletionSeal>),
    Recovery(WorkCompletionRecovery),
}

#[derive(Serialize)]
struct WorkCompletionRecoveryFingerprint<'a> {
    schema_version: u16,
    work_id: WorkId,
    run_id: WorkRunId,
    expected_work_revision: i64,
    holder: &'a SessionId,
    claim_id: WorkClaimId,
    claim_fence: i64,
    cause: &'a WorkCompletionRecoveryCause,
    idempotency_key: &'a str,
}

struct WorkProtocolAttemptRow {
    request_hash: String,
    basis_hash: Option<String>,
    basis_json: Option<Vec<u8>>,
    result_hash: Option<String>,
    result_json: Option<Vec<u8>>,
}

type WorkAuthorityGrantStatusRow = (String, String, String, i64, Vec<u8>, Option<i64>);

fn begin_work_protocol_attempt_on<T: Serialize, B: Serialize>(
    connection: &Connection,
    request: &BeginWorkProtocolAttempt<'_, T, B>,
) -> Result<WorkProtocolAttempt, StoreError> {
    let project_id = request.project_id;
    let session_id = request.session_id;
    let operation = request.operation;
    let idempotency_key = normalize_text(request.idempotency_key, "work idempotency key")?;
    let request_object = CanonicalObject::freeze(request.intent)?;
    let basis_object = CanonicalObject::freeze(request.basis)?;
    connection.execute(
        "INSERT INTO work_protocol_attempts (
             project_id, session_id, operation, idempotency_key,
             request_hash, basis_hash, basis_json, initiated_at_ms,
             result_hash, result_json
          ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, NULL)
         ON CONFLICT(project_id, session_id, operation, idempotency_key)
         DO NOTHING",
        params![
            project_id.0,
            session_id.0,
            operation,
            idempotency_key,
            request_object.hash().as_str(),
            basis_object.hash().as_str(),
            basis_object.bytes(),
            request.now.timestamp_millis()
        ],
    )?;
    let stored = connection.query_row(
        "SELECT request_hash, basis_hash, basis_json, result_hash, result_json
         FROM work_protocol_attempts
         WHERE project_id = ?1 AND session_id = ?2
           AND operation = ?3 AND idempotency_key = ?4",
        params![project_id.0, session_id.0, operation, idempotency_key],
        |row| {
            Ok(WorkProtocolAttemptRow {
                request_hash: row.get(0)?,
                basis_hash: row.get(1)?,
                basis_json: row.get(2)?,
                result_hash: row.get(3)?,
                result_json: row.get(4)?,
            })
        },
    )?;
    if stored.request_hash != request_object.hash().as_str() {
        return Err(StoreError::WorkOperationIdempotencyConflict {
            operation: operation.to_owned(),
            key: idempotency_key,
        });
    }
    let basis_matches = stored.basis_hash.as_deref() == Some(basis_object.hash().as_str())
        && stored.basis_json.as_deref() == Some(basis_object.bytes());
    let stored_basis = match (&stored.basis_hash, &stored.basis_json) {
        (Some(stored_hash), Some(bytes)) => {
            let hash = ObjectHash::from_stored(stored_hash.clone())
                .ok_or_else(|| StoreError::InvalidStoredHash(stored_hash.clone()))?;
            Some(CanonicalObject::verify(&hash, bytes.clone())?.decode()?)
        }
        (_, None) if stored.result_hash.is_some() && stored.result_json.is_some() => None,
        _ => {
            return Err(StoreError::InvalidWorkProjection(
                "pending work-protocol attempt has no verified durable basis".into(),
            ));
        }
    };
    let result = match (stored.result_hash, stored.result_json) {
        (None, None) => None,
        (Some(stored_hash), Some(bytes)) => {
            let hash = ObjectHash::from_stored(stored_hash.clone())
                .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
            let value = load_typed_work_object::<serde_json::Value>(
                connection,
                &hash,
                "work_protocol_result",
            )?;
            let object = CanonicalObject::freeze(&value)?;
            if object.hash() != &hash || object.bytes() != bytes {
                return Err(StoreError::InvalidWorkProjection(
                    "work-protocol replay bytes differ from their canonical result".into(),
                ));
            }
            validate_work_protocol_result_binding(connection, &project_id.0, operation, &value)?;
            Some(value)
        }
        _ => {
            return Err(StoreError::InvalidWorkProjection(
                "work-protocol result hash and bytes must be present together".into(),
            ));
        }
    };
    Ok(WorkProtocolAttempt {
        result,
        basis_matches,
        basis: stored_basis,
    })
}

impl SqliteStore {
    fn begin_work_mutation(&mut self) -> Result<Transaction<'_>, StoreError> {
        let expected_version = self.work_schema_version;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_work_schema_version(&transaction, expected_version)?;
        Ok(transaction)
    }

    /// Installs one canonical host-resolved work authority grant.
    ///
    /// This host-SDK boundary is intentionally not exposed through the agent
    /// protocol. Host adapters resolve identity and organizational policy, then
    /// persist the resulting asserted/authenticated/signed grant here.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when grant content, redaction, canonicalization,
    /// or persistence validation fails.
    pub fn install_work_authority_grant<R: Redactor>(
        &mut self,
        mut grant: WorkAuthorityGrant,
        redactor: &R,
    ) -> Result<ObjectHash, StoreError> {
        inspect_work_request(redactor, &grant)?;
        grant.policy_ref = normalize_text(&grant.policy_ref, "authority policy reference")?;
        grant.subject_actor_id = normalize_text(&grant.subject_actor_id, "authority subject")?;
        grant.issued_by.actor_id =
            normalize_text(&grant.issued_by.actor_id, "authority issuer actor")?;
        grant.issued_by.reason =
            normalize_text(&grant.issued_by.reason, "authority issuer reason")?;
        grant.reason = normalize_text(&grant.reason, "authority grant reason")?;
        grant.operations.sort();
        grant.operations.dedup();
        if grant.schema_version != SCHEMA_VERSION
            || grant.operations.is_empty()
            || grant.valid_until <= grant.issued_at
        {
            return Err(StoreError::InvalidWork(
                "authority grant schema, operations, or validity window is invalid".into(),
            ));
        }
        if grant.operations.contains(&WorkAuthorityOperation::Plan)
            != grant.planning_budget.is_some()
        {
            return Err(StoreError::InvalidWork(
                "planning authority grants must carry exactly one planning budget".into(),
            ));
        }
        if let Some(budget) = grant.planning_budget.as_ref()
            && (budget.max_depth == 0
                || budget.max_open_descendants < 2
                || budget.max_children_per_decomposition < 1
                || budget.max_children_per_decomposition > 64)
        {
            return Err(StoreError::InvalidWork(
                "authority planning budget is invalid".into(),
            ));
        }
        let object = CanonicalObject::freeze(&grant)?;
        let transaction = self.begin_work_mutation()?;
        SqliteStore::insert_object(&transaction, "work_authority_grant", &object)?;
        transaction.execute(
            "INSERT INTO work_authority_grants (
                 grant_hash, project_id, policy_ref, subject_actor_id,
                 valid_until_ms, revoked_at_ms, grant_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, NULL, ?6)
             ON CONFLICT(grant_hash) DO NOTHING",
            params![
                object.hash().as_str(),
                grant.project_id.0,
                grant.policy_ref,
                grant.subject_actor_id,
                grant.valid_until.timestamp_millis(),
                object.bytes()
            ],
        )?;
        transaction.commit()?;
        Ok(object.hash().clone())
    }

    /// Reports whether one work-authority hash is installed and its public
    /// validity envelope. Unknown hashes are a successful negative query.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when an installed grant or its immutable
    /// revocation projection fails integrity verification.
    pub fn work_authority_grant_status(
        &self,
        grant_hash: &ObjectHash,
    ) -> Result<WorkAuthorityGrantStatus, StoreError> {
        let stored: Option<WorkAuthorityGrantStatusRow> = self
            .connection
            .query_row(
                "SELECT project_id, policy_ref, subject_actor_id, valid_until_ms,
                        grant_json, revoked_at_ms
                 FROM work_authority_grants WHERE grant_hash = ?1",
                [grant_hash.as_str()],
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
            .optional()?;
        let Some((project_id, policy_ref, subject_actor_id, valid_until_ms, bytes, revoked_at_ms)) =
            stored
        else {
            return Ok(WorkAuthorityGrantStatus {
                installed: false,
                subject_actor_id: None,
                issued_by: None,
                valid_from: None,
                valid_until: None,
                revoked_at: None,
                operations: None,
                scope: None,
            });
        };
        let grant: WorkAuthorityGrant = CanonicalObject::verify(grant_hash, bytes)?.decode()?;
        require_authority_revocation_integrity(&self.connection, grant_hash, revoked_at_ms)?;
        if project_id != grant.project_id.0
            || policy_ref != grant.policy_ref
            || subject_actor_id != grant.subject_actor_id
            || valid_until_ms != grant.valid_until.timestamp_millis()
        {
            return Err(StoreError::InvalidWorkProjection(format!(
                "authority grant {grant_hash} has an invalid installed projection"
            )));
        }
        let revoked_at = revoked_at_ms
            .map(|value| {
                DateTime::from_timestamp_millis(value).ok_or_else(|| {
                    StoreError::InvalidWorkProjection(format!(
                        "authority grant {grant_hash} has an invalid revocation time"
                    ))
                })
            })
            .transpose()?;
        Ok(WorkAuthorityGrantStatus {
            installed: true,
            subject_actor_id: Some(grant.subject_actor_id),
            issued_by: Some(grant.issued_by.actor_id),
            valid_from: Some(grant.issued_at),
            valid_until: Some(grant.valid_until),
            revoked_at,
            operations: Some(grant.operations),
            scope: Some(grant.scope),
        })
    }

    /// Revokes a host-issued work-authority grant through an immutable record.
    ///
    /// This host-SDK boundary is not agent-facing. Repeating a revocation
    /// returns the original immutable revocation hash.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the grant is absent, attribution or content
    /// is invalid, or the atomic revocation cannot be persisted.
    pub fn revoke_work_authority_grant<R: Redactor>(
        &mut self,
        grant_hash: &ObjectHash,
        revoked_by: &crate::domain::ActorContext,
        reason: &str,
        revoked_at: DateTime<Utc>,
        redactor: &R,
    ) -> Result<ObjectHash, StoreError> {
        let reason = normalize_text(reason, "authority revocation reason")?;
        if revoked_at > Utc::now() {
            return Err(StoreError::InvalidWork(
                "authority revocation time cannot be in the future".into(),
            ));
        }
        let transaction = self.begin_work_mutation()?;
        let existing: Option<String> = transaction
            .query_row(
                "SELECT revocation_hash FROM work_authority_revocations WHERE grant_hash = ?1",
                [grant_hash.as_str()],
                |row| row.get(0),
            )
            .optional()?;
        if let Some(existing) = existing {
            let hash = ObjectHash::from_stored(existing.clone())
                .ok_or(StoreError::InvalidStoredHash(existing))?;
            transaction.commit()?;
            return Ok(hash);
        }
        let grant: WorkAuthorityGrant =
            load_typed_work_object(&transaction, grant_hash, "work_authority_grant")?;
        let projected_revocation: Option<i64> = transaction
            .query_row(
                "SELECT revoked_at_ms FROM work_authority_grants WHERE grant_hash = ?1",
                [grant_hash.as_str()],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        if projected_revocation.is_some() || revoked_at < grant.issued_at {
            return Err(StoreError::InvalidWork(
                "authority grant revocation state or time is invalid".into(),
            ));
        }
        let revocation = WorkAuthorityRevocation {
            schema_version: SCHEMA_VERSION,
            grant: grant_hash.clone(),
            revoked_by: revoked_by.clone(),
            reason,
            revoked_at,
        };
        inspect_work_request(redactor, &revocation)?;
        let object = CanonicalObject::freeze(&revocation)?;
        SqliteStore::insert_object(&transaction, "work_authority_revocation", &object)?;
        let changed = transaction.execute(
            "UPDATE work_authority_grants SET revoked_at_ms = ?2
             WHERE grant_hash = ?1 AND revoked_at_ms IS NULL",
            params![grant_hash.as_str(), revoked_at.timestamp_millis()],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidWorkProjection(format!(
                "authority grant {grant_hash} was not active during revocation"
            )));
        }
        transaction.execute(
            "INSERT INTO work_authority_revocations (
                 grant_hash, revocation_hash, revoked_at_ms, revocation_json
             ) VALUES (?1, ?2, ?3, ?4)",
            params![
                grant_hash.as_str(),
                object.hash().as_str(),
                revoked_at.timestamp_millis(),
                object.bytes()
            ],
        )?;
        transaction.commit()?;
        Ok(object.hash().clone())
    }

    /// Returns one current work projection.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the item is absent, corrupt, or unreadable.
    pub fn get_work_item(&self, work_id: WorkId) -> Result<WorkItem, StoreError> {
        load_work_item(&self.connection, work_id)
    }

    /// Returns the active or historical run by stable identity.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the run is absent, corrupt, or unreadable.
    pub fn get_work_run(&self, run_id: WorkRunId) -> Result<WorkRun, StoreError> {
        load_work_run(&self.connection, run_id)
    }

    /// Returns the newest run generation for work, including a terminal run.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when a persisted run identity or projection is invalid.
    pub fn latest_work_run(&self, work_id: WorkId) -> Result<Option<WorkRun>, StoreError> {
        let run_id: Option<String> = self
            .connection
            .query_row(
                "SELECT run_id FROM work_runs WHERE work_id = ?1
                 ORDER BY generation DESC LIMIT 1",
                [work_id.0.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        run_id
            .map(|value| parse_work_run_id(&value).and_then(|run_id| self.get_work_run(run_id)))
            .transpose()
    }

    /// Resolves a model-facing short reference or full UUID inside one project.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the reference is empty, absent, ambiguous,
    /// or its stored projection is invalid.
    pub fn resolve_work_ref(
        &self,
        project_id: &crate::domain::ProjectId,
        work_ref: &str,
    ) -> Result<WorkItem, StoreError> {
        resolve_work_ref_on(&self.connection, project_id, work_ref)
    }

    /// Freezes a pre-completion recovery snapshot under the same SQLite
    /// snapshot that proves the missing acceptance criterion.
    #[allow(
        clippy::too_many_arguments,
        reason = "the exact claim fence and protocol replay key are explicit recovery evidence"
    )]
    pub(crate) fn persist_preflight_completion_recovery(
        &mut self,
        work_id: WorkId,
        run_id: WorkRunId,
        expected_work_revision: i64,
        holder: &SessionId,
        claim_id: WorkClaimId,
        claim_fence: i64,
        cause: &WorkCompletionRecoveryCause,
        idempotency_key: &str,
        now: DateTime<Utc>,
    ) -> Result<WorkCompletionRecovery, StoreError> {
        let fingerprint = WorkCompletionRecoveryFingerprint {
            schema_version: SCHEMA_VERSION,
            work_id,
            run_id,
            expected_work_revision,
            holder,
            claim_id,
            claim_fence,
            cause,
            idempotency_key,
        };
        let request_object = request_object(&fingerprint)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(recovery) = replay_operation::<WorkCompletionRecovery>(
            &transaction,
            "complete_work_recovery",
            idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(recovery);
        }
        let work = match cause {
            WorkCompletionRecoveryCause::MissingAcceptance { criterion } => {
                let (work, _, _) = validate_live_claim_on(
                    &transaction,
                    work_id,
                    run_id,
                    expected_work_revision,
                    holder,
                    claim_id,
                    claim_fence,
                    now,
                    false,
                )?;
                if !work.acceptance.contains(criterion) {
                    return Err(StoreError::InvalidWorkProjection(
                        "completion recovery criterion is not in the deciding work revision".into(),
                    ));
                }
                work
            }
            _ => {
                return Err(StoreError::InvalidWork(
                    "preflight completion recovery accepts only missing acceptance".into(),
                ));
            }
        };
        let recovery = persist_completion_recovery_on(
            &transaction,
            &work,
            cause.clone(),
            "complete_work_recovery",
            idempotency_key,
            request_object.hash(),
        )?;
        transaction.commit()?;
        Ok(recovery)
    }

    /// Returns ambient navigation state, defaulting to no focus and cursor zero.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the projection cannot be read.
    pub fn work_session_state(
        &self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        now: DateTime<Utc>,
    ) -> Result<WorkSessionState, StoreError> {
        let row = self
            .connection
            .query_row(
                "SELECT focused_work_id, project_cursor,
                        tentative_project_cursor, tentative_delivery_token,
                        tentative_delivery_payload_hash,
                        tentative_delivery_payload, updated_at_ms
                 FROM work_session_state WHERE project_id = ?1 AND session_id = ?2",
                params![project_id.0, session_id.0],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<Vec<u8>>>(5)?,
                        row.get::<_, i64>(6)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            focused_work_id,
            project_cursor,
            tentative_project_cursor,
            tentative_delivery_token,
            tentative_delivery_payload_hash,
            tentative_delivery_payload,
            updated_at_ms,
        )) = row
        else {
            return Ok(WorkSessionState {
                project_id: project_id.clone(),
                session_id: session_id.clone(),
                focused_work_id: None,
                project_cursor: 0,
                tentative_project_cursor: None,
                tentative_delivery_token: None,
                updated_at: now,
            });
        };
        let pending_fields = [
            tentative_project_cursor.is_some(),
            tentative_delivery_token.is_some(),
            tentative_delivery_payload_hash.is_some(),
            tentative_delivery_payload.is_some(),
        ];
        if pending_fields
            .iter()
            .any(|present| *present != pending_fields[0])
        {
            return Err(StoreError::InvalidWorkProjection(
                "staged work delivery cursor, token, payload hash, and payload must be present together"
                    .into(),
            ));
        }
        if let (Some(hash), Some(payload)) =
            (tentative_delivery_payload_hash, tentative_delivery_payload)
        {
            let hash =
                ObjectHash::from_stored(hash.clone()).ok_or(StoreError::InvalidStoredHash(hash))?;
            CanonicalObject::verify(&hash, payload)?;
        }
        Ok(WorkSessionState {
            project_id: project_id.clone(),
            session_id: session_id.clone(),
            focused_work_id: focused_work_id
                .map(|value| parse_work_id(&value))
                .transpose()?,
            project_cursor,
            tentative_project_cursor,
            tentative_delivery_token,
            updated_at: DateTime::from_timestamp_millis(updated_at_ms).ok_or_else(|| {
                StoreError::InvalidWorkProjection(format!(
                    "invalid work-session timestamp {updated_at_ms}"
                ))
            })?,
        })
    }

    /// Resolves the currently live operations admitted by one host-bound grant
    /// for a focused item. Invalid, expired, revoked, wrong-actor, wrong-policy,
    /// and wrong-scope grants yield no advertised capability.
    pub fn allowed_work_authority_operations(
        &self,
        decision: &LifecycleAuthorityDecision,
        actor: &crate::domain::ActorContext,
        item: &WorkItem,
        now: DateTime<Utc>,
    ) -> Vec<WorkAuthorityOperation> {
        let target = AuthorityTarget {
            project_id: &item.project_id,
            policy_ref: &item.authority_policy_ref,
            work_id: Some(item.work_id),
            root_id: Some(item.root_id),
            run_id: item.active_run_id,
        };
        let Ok(grant) =
            resolve_work_authority_grant(&self.connection, decision, actor, target, now)
        else {
            return Vec::new();
        };
        [
            WorkAuthorityOperation::Plan,
            WorkAuthorityOperation::Claim,
            WorkAuthorityOperation::Dispose,
            WorkAuthorityOperation::RootComplete,
            WorkAuthorityOperation::Reopen,
            WorkAuthorityOperation::ClaimRecovery,
            WorkAuthorityOperation::CompletionWaiver,
            WorkAuthorityOperation::CompletionDrain,
        ]
        .into_iter()
        .filter(|operation| grant.operations.contains(operation))
        .collect()
    }

    /// Returns direct required children that the supplied grant can currently
    /// waive from this parent's completion barrier.
    ///
    /// The check deliberately reuses the mutation's exact child-scoped
    /// authority target. Agent guidance must not advertise a parent-scoped
    /// capability that the eventual waiver operation will refuse.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the parent/root projections or canonical
    /// waiver history are invalid, or when candidate children cannot be read.
    pub fn waivable_required_children(
        &self,
        decision: &LifecycleAuthorityDecision,
        actor: &crate::domain::ActorContext,
        parent: &WorkItem,
        now: DateTime<Utc>,
        limit: usize,
    ) -> Result<Vec<WorkItem>, StoreError> {
        if limit == 0 || parent.lifecycle != WorkLifecycle::Open {
            return Ok(Vec::new());
        }
        let mut statement = self.connection.prepare(
            "SELECT work_id FROM work_items
             WHERE parent_id = ?1
               AND child_requirement = 'required'
               AND lifecycle IN ('cancelled', 'superseded')
             ORDER BY created_at_ms, work_id",
        )?;
        let child_ids = statement
            .query_map([parent.work_id.0.to_string()], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        if child_ids.is_empty() {
            return Ok(Vec::new());
        }
        let root_execution = active_root_execution(&self.connection, parent.root_id)?;
        let waived =
            validated_required_child_waivers(&self.connection, parent.work_id, &root_execution)?
                .into_iter()
                .map(|waiver| waiver.work_id)
                .collect::<HashSet<_>>();

        let mut eligible = Vec::new();
        for stored_id in child_ids {
            let child_id = parse_work_id(&stored_id)?;
            if waived.contains(&child_id) {
                continue;
            }
            let child = load_work_item(&self.connection, child_id)?;
            match resolve_work_authority(
                &self.connection,
                decision,
                actor,
                WorkAuthorityOperation::CompletionWaiver,
                AuthorityTarget {
                    project_id: &child.project_id,
                    policy_ref: &child.authority_policy_ref,
                    work_id: Some(child.work_id),
                    root_id: Some(child.root_id),
                    run_id: child.active_run_id,
                },
                now,
            ) {
                Ok(_) => eligible.push(child),
                Err(StoreError::InvalidWork(_)) => continue,
                Err(error) => return Err(error),
            }
            if eligible.len() == limit {
                break;
            }
        }
        Ok(eligible)
    }

    /// Starts or replays one caller-visible ambient protocol intent before any
    /// mutable focus, claim, revision, or handoff state is inferred.
    pub(crate) fn begin_work_protocol_attempt<T: Serialize, B: Serialize>(
        &mut self,
        request: &BeginWorkProtocolAttempt<'_, T, B>,
    ) -> Result<WorkProtocolAttempt, StoreError> {
        let transaction = self.begin_work_mutation()?;
        let attempt = begin_work_protocol_attempt_on(&transaction, request)?;
        transaction.commit()?;
        Ok(attempt)
    }

    /// Persists the caller-visible result for exact lost-response replay.
    pub(crate) fn finish_work_protocol_attempt<T: Serialize>(
        &mut self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        operation: &str,
        idempotency_key: &str,
        result: &T,
    ) -> Result<(), StoreError> {
        let compact_result =
            compact_work_protocol_result(operation, serde_json::to_value(result)?)?;
        let transaction = self.begin_work_mutation()?;
        validate_work_protocol_result_binding(
            &transaction,
            &project_id.0,
            operation,
            &compact_result,
        )?;
        let result_object = CanonicalObject::freeze(&compact_result)?;
        Self::insert_object(&transaction, "work_protocol_result", &result_object)?;
        let changed = transaction.execute(
            "UPDATE work_protocol_attempts
             SET basis_json = NULL, result_hash = ?5, result_json = ?6
             WHERE project_id = ?1 AND session_id = ?2
               AND operation = ?3 AND idempotency_key = ?4
               AND result_hash IS NULL AND result_json IS NULL",
            params![
                project_id.0,
                session_id.0,
                operation,
                idempotency_key,
                result_object.hash().as_str(),
                result_object.bytes()
            ],
        )?;
        if changed == 0 {
            let stored = transaction
                .query_row(
                    "SELECT result_hash, result_json FROM work_protocol_attempts
                     WHERE project_id = ?1 AND session_id = ?2
                       AND operation = ?3 AND idempotency_key = ?4",
                    params![project_id.0, session_id.0, operation, idempotency_key],
                    |row| {
                        Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, Option<Vec<u8>>>(1)?,
                        ))
                    },
                )
                .optional()?;
            if stored
                != Some((
                    Some(result_object.hash().as_str().to_owned()),
                    Some(result_object.bytes().to_vec()),
                ))
            {
                return Err(StoreError::InvalidWorkProjection(
                    "work-protocol result completion conflicted with its durable attempt".into(),
                ));
            }
        } else if changed != 1 {
            return Err(StoreError::InvalidWorkProjection(
                "work-protocol result updated more than one durable attempt".into(),
            ));
        }
        transaction.commit()?;
        Ok(())
    }

    /// Returns a committed core-operation result after an interrupted ambient
    /// wrapper. The already-matched protocol intent makes this lookup safe.
    pub(crate) fn work_operation_result_value(
        &self,
        operation: &str,
        idempotency_key: &str,
    ) -> Result<Option<serde_json::Value>, StoreError> {
        let bytes: Option<Vec<u8>> = self
            .connection
            .query_row(
                "SELECT result_json FROM work_operation_results
                 WHERE operation = ?1 AND idempotency_key = ?2",
                params![operation, idempotency_key],
                |row| row.get(0),
            )
            .optional()?;
        bytes
            .map(|value| serde_json::from_slice(&value).map_err(StoreError::from))
            .transpose()
    }

    /// Loads the canonical object named by a committed hash-valued core
    /// operation result. The caller still replays the operation so its stored
    /// request hash verifies the reconstructed request exactly.
    pub(crate) fn work_operation_result_object<T: DeserializeOwned>(
        &self,
        operation: &str,
        idempotency_key: &str,
        object_kind: &str,
    ) -> Result<Option<T>, StoreError> {
        let Some(value) = self.work_operation_result_value(operation, idempotency_key)? else {
            return Ok(None);
        };
        let hash: ObjectHash = serde_json::from_value(value)?;
        load_typed_work_object(&self.connection, &hash, object_kind).map(Some)
    }

    /// Changes ambient focus without claiming or releasing work.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the target is outside the project or the
    /// mutable navigation projection cannot be written.
    pub fn focus_work_session(
        &mut self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        work_id: WorkId,
        now: DateTime<Utc>,
    ) -> Result<WorkSessionState, StoreError> {
        let transaction = self.begin_work_mutation()?;
        let item = load_work_item(&transaction, work_id)?;
        if item.project_id != *project_id {
            return Err(StoreError::InvalidWork(
                "focused work must belong to the bound project".into(),
            ));
        }
        // A staged change page was projected under the previous focus. A
        // different focus discards it so the next call recomputes the same
        // interval under the new visibility basis; nothing is confirmed here.
        transaction.execute(
            "UPDATE work_session_state SET
                 tentative_project_cursor = NULL,
                 tentative_delivery_token = NULL,
                 tentative_delivery_payload_hash = NULL,
                 tentative_delivery_payload = NULL
             WHERE project_id = ?1 AND session_id = ?2
               AND focused_work_id IS NOT ?3",
            params![project_id.0, session_id.0, work_id.0.to_string()],
        )?;
        transaction.execute(
            "INSERT INTO work_session_state (
                 project_id, session_id, focused_work_id, project_cursor, updated_at_ms
             ) VALUES (?1, ?2, ?3, 0, ?4)
             ON CONFLICT(project_id, session_id) DO UPDATE SET
                 focused_work_id = excluded.focused_work_id,
                 updated_at_ms = excluded.updated_at_ms",
            params![
                project_id.0,
                session_id.0,
                work_id.0.to_string(),
                now.timestamp_millis()
            ],
        )?;
        transaction.commit()?;
        self.work_session_state(project_id, session_id, now)
    }

    /// Stages one replayable delivery without acknowledging it.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the cursor is invalid or cannot be stored.
    pub(crate) fn stage_work_session_delivery(
        &mut self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        request: StageWorkSessionDelivery<'_>,
    ) -> Result<Option<WorkSessionState>, StoreError> {
        let StageWorkSessionDelivery {
            expected_confirmed_through,
            expected_focused_work_id,
            expected_bound_task_id,
            delivered_through,
            delivered_entries,
            delivery_payload,
            now,
        } = request;
        if expected_confirmed_through < 0 || delivered_through < 0 {
            return Err(StoreError::InvalidWork(
                "work delivery cursor must not be negative".into(),
            ));
        }
        let transaction = self.begin_work_mutation()?;
        transaction.execute(
            "INSERT INTO work_session_state (
                 project_id, session_id, focused_work_id, project_cursor,
                 updated_at_ms
              ) VALUES (?1, ?2, NULL, 0, ?3)
              ON CONFLICT(project_id, session_id) DO NOTHING",
            params![project_id.0, session_id.0, now.timestamp_millis()],
        )?;
        let (focused_work_id, confirmed, pending): (Option<String>, i64, Option<i64>) = transaction
            .query_row(
                "SELECT focused_work_id, project_cursor, tentative_project_cursor
             FROM work_session_state WHERE project_id = ?1 AND session_id = ?2",
                params![project_id.0, session_id.0],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
        let focused_work_id = focused_work_id
            .map(|value| parse_work_id(&value))
            .transpose()?;
        let bound_task_id = transaction
            .query_row(
                "SELECT b.task_id FROM session_bindings b JOIN tasks t
                   ON t.task_id = b.task_id
                 WHERE b.session_id = ?1 AND t.project_id = ?2",
                params![session_id.0, project_id.0],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|stored| {
                uuid::Uuid::parse_str(&stored)
                    .map(TaskId)
                    .map_err(|error| StoreError::InvalidTaskProjection(error.to_string()))
            })
            .transpose()?;
        if confirmed != expected_confirmed_through
            || focused_work_id != expected_focused_work_id
            || bound_task_id != expected_bound_task_id
            || pending.is_some()
        {
            return Ok(None);
        }
        let feed = FeedId::Project(project_id.clone());
        let head = feed_head(&transaction, &feed)?;
        if delivered_through < confirmed || delivered_through > head {
            return Err(StoreError::InvalidWork(format!(
                "work delivery cursor {delivered_through} must be within [{confirmed}, {head}]"
            )));
        }
        if delivered_through > confirmed {
            let expected_count = usize::try_from(delivered_through - confirmed).map_err(|_| {
                StoreError::InvalidWork("work delivery interval size overflowed".into())
            })?;
            if delivered_entries.len() != expected_count
                || delivered_entries.iter().enumerate().any(|(offset, entry)| {
                    entry.position.feed != feed
                        || i64::try_from(offset)
                            .ok()
                            .is_none_or(|offset| entry.position.position != confirmed + offset + 1)
                })
            {
                return Err(StoreError::InvalidWork(
                    "work delivery payload does not bind the exact dense interval".into(),
                ));
            }
            let (feed_kind, feed_id) = feed_parts(&feed);
            let stored_entries = transaction
                .prepare(
                    "SELECT position, object_kind, object_hash
                 FROM work_feed_entries
                 WHERE feed_kind = ?1 AND feed_id = ?2
                   AND position > ?3 AND position <= ?4
                 ORDER BY position",
                )?
                .query_map(
                    params![feed_kind, feed_id, confirmed, delivered_through],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )?
                .collect::<Result<Vec<_>, _>>()?;
            let stored_matches = stored_entries.len() == expected_count
                && stored_entries.iter().zip(delivered_entries).all(
                    |((position, object_kind, object_hash), delivered)| {
                        *position == delivered.position.position
                            && object_kind == &delivered.object_kind
                            && object_hash == delivered.object_hash.as_str()
                    },
                );
            if !stored_matches {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "work delivery interval ({confirmed}, {delivered_through}] is not dense"
                )));
            }
        } else if !delivered_entries.is_empty() {
            return Err(StoreError::InvalidWork(
                "an empty work delivery interval cannot bind feed entries".into(),
            ));
        }
        let new_delivery_token = uuid::Uuid::new_v4().to_string();
        let tentative = (delivered_through > confirmed).then_some(delivered_through);
        let tentative_delivery_token = tentative.map(|_| new_delivery_token);
        let changed = transaction.execute(
            "UPDATE work_session_state SET
                 tentative_project_cursor = ?3,
                 tentative_delivery_token = ?4,
                 tentative_delivery_payload_hash = ?5,
                 tentative_delivery_payload = ?6,
                 updated_at_ms = ?7
             WHERE project_id = ?1 AND session_id = ?2
               AND project_cursor = ?8
               AND tentative_project_cursor IS NULL
               AND focused_work_id IS ?9",
            params![
                project_id.0,
                session_id.0,
                tentative,
                tentative_delivery_token.as_deref(),
                tentative.map(|_| delivery_payload.hash().as_str()),
                tentative.map(|_| delivery_payload.bytes()),
                now.timestamp_millis(),
                expected_confirmed_through,
                expected_focused_work_id.map(|work_id| work_id.0.to_string())
            ],
        )?;
        if changed != 1 {
            return Ok(None);
        }
        let staged = WorkSessionState {
            project_id: project_id.clone(),
            session_id: session_id.clone(),
            focused_work_id,
            project_cursor: confirmed,
            tentative_project_cursor: tentative,
            tentative_delivery_token,
            updated_at: now,
        };
        transaction.commit()?;
        Ok(Some(staged))
    }

    /// Loads the exact canonical agent page bound to a pending delivery.
    pub(crate) fn staged_work_session_delivery_payload(
        &self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
    ) -> Result<Option<CanonicalObject>, StoreError> {
        let row = self
            .connection
            .query_row(
                "SELECT tentative_delivery_payload_hash, tentative_delivery_payload
                 FROM work_session_state
                 WHERE project_id = ?1 AND session_id = ?2",
                params![project_id.0, session_id.0],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<Vec<u8>>>(1)?,
                    ))
                },
            )
            .optional()?;
        match row {
            None | Some((None, None)) => Ok(None),
            Some((Some(hash), Some(bytes))) => {
                let hash = ObjectHash::from_stored(hash.clone())
                    .ok_or(StoreError::InvalidStoredHash(hash))?;
                Ok(Some(CanonicalObject::verify(&hash, bytes)?))
            }
            Some(_) => Err(StoreError::InvalidWorkProjection(
                "staged work delivery payload hash and bytes must be present together".into(),
            )),
        }
    }

    /// Acknowledges the exact staged delivery using compare-and-swap semantics.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the acknowledgement is stale, was never
    /// staged, or cannot be persisted.
    pub fn acknowledge_work_session_delivery(
        &mut self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        through: i64,
        delivery_token: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<WorkSessionState, StoreError> {
        if through < 0 {
            return Err(StoreError::InvalidWork(
                "work acknowledgement cursor must not be negative".into(),
            ));
        }
        let transaction = self.begin_work_mutation()?;
        let changed = transaction.execute(
            "UPDATE work_session_state SET
                 project_cursor = ?3,
                 tentative_project_cursor = NULL,
                 tentative_delivery_token = NULL,
                 tentative_delivery_payload_hash = NULL,
                 tentative_delivery_payload = NULL,
                 updated_at_ms = ?5
             WHERE project_id = ?1 AND session_id = ?2
               AND tentative_project_cursor = ?3
               AND tentative_delivery_token = ?4",
            params![
                project_id.0,
                session_id.0,
                through,
                delivery_token,
                now.timestamp_millis()
            ],
        )?;
        transaction.commit()?;
        let state = self.work_session_state(project_id, session_id, now)?;
        if changed == 0 {
            if state.project_cursor == through {
                return Ok(state);
            }
            return Err(StoreError::InvalidWork(
                "work delivery acknowledgement does not match the pending page; replay it with work_next (changes selected, no acknowledgement) and acknowledge the delivered_through and delivery_token you receive"
                    .into(),
            ));
        }
        Ok(state)
    }

    /// Lists direct children in stable creation order.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when a child projection cannot be decoded.
    pub fn work_children(&self, work_id: WorkId) -> Result<Vec<WorkItem>, StoreError> {
        load_work_items_query(
            &self.connection,
            "SELECT item_json FROM work_items
             WHERE parent_id = ?1 ORDER BY created_at_ms, work_id",
            work_id,
        )
    }

    /// Lists explicit prerequisites in stable id order.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when a prerequisite projection cannot be decoded.
    pub fn work_prerequisites(&self, work_id: WorkId) -> Result<Vec<WorkItem>, StoreError> {
        load_work_items_query(
            &self.connection,
            "SELECT prerequisite.item_json
             FROM work_prerequisites edge
             JOIN work_items prerequisite ON prerequisite.work_id = edge.prerequisite_id
             WHERE edge.work_id = ?1 ORDER BY prerequisite.work_id",
            work_id,
        )
    }

    pub(crate) fn work_prerequisites_with_state(
        &self,
        work_id: WorkId,
        limit: usize,
    ) -> Result<WorkPrerequisitePage, StoreError> {
        bounded_prerequisite_projection_rows(&self.connection, work_id, limit)
    }

    /// Returns the projected claim for the active run, or the latest
    /// historical run after a terminal transition clears the active run.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when work or claim projections are invalid.
    pub fn current_work_claim(&self, work_id: WorkId) -> Result<Option<WorkClaim>, StoreError> {
        let item = load_work_item(&self.connection, work_id)?;
        self.current_work_claim_for_item(&item)
    }

    pub(crate) fn current_work_claim_for_item(
        &self,
        item: &WorkItem,
    ) -> Result<Option<WorkClaim>, StoreError> {
        item.active_run_id
            .or(self.latest_work_run(item.work_id)?.map(|run| run.run_id))
            .map(|run_id| load_work_claim_optional(&self.connection, run_id))
            .transpose()
            .map(Option::flatten)
    }

    /// Lists the work this session holds under a live claim, with each claim's
    /// expiry, from the claim projection alone.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when a stored work id is invalid.
    pub fn work_held_by(
        &self,
        holder: &SessionId,
        now: DateTime<Utc>,
    ) -> Result<Vec<(WorkId, DateTime<Utc>)>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT work_id, expires_at_ms FROM work_claims
             WHERE holder_session_id = ?1 AND state = 'active' AND expires_at_ms > ?2
             ORDER BY expires_at_ms, work_id",
        )?;
        let rows = statement.query_map(params![holder.0, now.timestamp_millis()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut held = Vec::new();
        for row in rows {
            let (work_id, expires_at_ms) = row?;
            let work_id = uuid::Uuid::parse_str(&work_id).map(WorkId).map_err(|_| {
                StoreError::InvalidWorkProjection(format!("claim has invalid work id {work_id}"))
            })?;
            let expires_at =
                DateTime::<Utc>::from_timestamp_millis(expires_at_ms).ok_or_else(|| {
                    StoreError::InvalidWorkProjection("claim has an invalid expiry".into())
                })?;
            held.push((work_id, expires_at));
        }
        Ok(held)
    }

    /// Lists every live project claim needed to render compact catalog rows.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when a projected work id or expiry is invalid.
    pub fn live_work_claims(
        &self,
        project_id: &crate::domain::ProjectId,
        now: DateTime<Utc>,
    ) -> Result<Vec<(WorkId, SessionId, DateTime<Utc>)>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT claim.work_id, claim.holder_session_id, claim.expires_at_ms
             FROM work_claims claim
             JOIN work_items item ON item.work_id = claim.work_id
             WHERE item.project_id = ?1
               AND claim.state = 'active' AND claim.expires_at_ms > ?2
             ORDER BY claim.work_id",
        )?;
        let rows = statement.query_map(
            params![project_id.0.as_str(), now.timestamp_millis()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )?;
        let mut claims = Vec::new();
        for row in rows {
            let (work_id, holder, expires_at_ms) = row?;
            let work_id = uuid::Uuid::parse_str(&work_id).map(WorkId).map_err(|_| {
                StoreError::InvalidWorkProjection(format!("claim has invalid work id {work_id}"))
            })?;
            let expires_at =
                DateTime::<Utc>::from_timestamp_millis(expires_at_ms).ok_or_else(|| {
                    StoreError::InvalidWorkProjection("claim has an invalid expiry".into())
                })?;
            claims.push((work_id, SessionId(holder), expires_at));
        }
        Ok(claims)
    }

    /// Reports whether taking the current run from another prior holder needs
    /// an attributed claim-recovery waiver.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the work, run, claim, or root projection is
    /// invalid.
    pub fn work_claim_recovery_required(
        &self,
        work_id: WorkId,
        claimant: &SessionId,
    ) -> Result<bool, StoreError> {
        let item = load_work_item(&self.connection, work_id)?;
        let claim = self.current_work_claim_for_item(&item)?;
        self.work_claim_recovery_required_for_item(&item, claim.as_ref(), claimant)
    }

    pub(crate) fn work_claim_recovery_required_for_item(
        &self,
        item: &WorkItem,
        claim: Option<&WorkClaim>,
        claimant: &SessionId,
    ) -> Result<bool, StoreError> {
        if claim.is_none_or(|claim| claim.holder == *claimant) {
            return Ok(false);
        }
        let Some(run_id) = item.active_run_id else {
            return Ok(false);
        };
        let run = load_work_run(&self.connection, run_id)?;
        let execution = load_root_execution(&self.connection, run.root_execution_id)?;
        Ok(claim.is_some_and(|claim| {
            claim.run_id == run_id
                && claim.holder != *claimant
                && !root_participant_is_accounted(&execution, &claim.holder)
        }))
    }

    /// Returns every handoff offer for work in stable offer order.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when an offer projection cannot be decoded.
    pub fn work_handoff_offers(
        &self,
        work_id: WorkId,
    ) -> Result<Vec<WorkHandoffOffer>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT offer_hash, offer_json FROM work_handoff_offers
             WHERE work_id = ?1 ORDER BY offer_id",
        )?;
        let rows = statement
            .query_map([work_id.0.to_string()], |row| {
                Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|row| load_handoff_offer_projection(&self.connection, row))
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn add_expected_root_contributor_fixture(
        &mut self,
        work_id: WorkId,
        participant: &SessionId,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let transaction = self.begin_work_mutation()?;
        let item = load_work_item(&transaction, work_id)?;
        let run = active_run_snapshot(&transaction, &item)?.ok_or_else(|| {
            StoreError::InvalidWorkProjection("fixture work has no active run".into())
        })?;
        let mut root_execution = load_root_execution(&transaction, run.root_execution_id)?;
        if expect_root_contributor(&mut root_execution, participant) {
            root_execution.revision += 1;
            root_execution.updated_at = now;
            persist_root_execution(&transaction, &root_execution)?;
        }
        let mut event = latest_canonical_work_event_for_item(&transaction, work_id)?;
        event.root_execution = Some(root_execution);
        event.created_at = now;
        append_work_event(&transaction, &WorkEventDraft::from(&event))?;
        transaction.commit()?;
        Ok(())
    }

    /// Returns canonical evidence hashes recorded for one run.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when a stored hash is invalid.
    pub fn work_run_evidence(&self, run_id: WorkRunId) -> Result<Vec<ObjectHash>, StoreError> {
        work_run_evidence_on(&self.connection, run_id)
    }

    /// Returns a bounded, hash-verified selection basis for one focus page.
    /// Candidate identity is chosen only by the immutable evidence hash; the
    /// canonical kind and environment binding are verified before selection.
    pub(crate) fn work_run_evidence_projection(
        &self,
        run_id: WorkRunId,
        required_environments: &[ObjectHash],
        limit: usize,
    ) -> Result<Vec<WorkEvidenceProjectionSummary>, StoreError> {
        work_run_evidence_projection_on(&self.connection, run_id, required_environments, limit)
    }

    pub(crate) fn work_run_evidence_count(&self, run_id: WorkRunId) -> Result<usize, StoreError> {
        let count = self.connection.query_row(
            "SELECT COUNT(*) FROM work_run_evidence WHERE run_id = ?1",
            [run_id.0.to_string()],
            |row| row.get::<_, i64>(0),
        )?;
        usize::try_from(count).map_err(|_| {
            StoreError::InvalidWorkProjection("run evidence count does not fit usize".into())
        })
    }

    /// Returns hash-verified obligation definitions and terminal resolutions
    /// for one exact run in trigger order.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when canonical bytes, feed positions, redundant
    /// scalar bindings, or resolution authority do not agree.
    pub(crate) fn work_run_obligations(
        &self,
        run_id: WorkRunId,
    ) -> Result<Vec<WorkObligationRecord>, StoreError> {
        load_work_obligation_records_on(&self.connection, run_id, None)
    }

    /// Derives the obligations that were still open at one exact immutable
    /// run-feed cut. A3 consumes this helper when sealing completion.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the cut names another feed, exceeds the
    /// current head, splits an atomic observation/definition append, or any
    /// canonical obligation binding is invalid.
    #[allow(
        dead_code,
        reason = "A2 ships the verified cut query as the explicit basis for the A3 completion gate"
    )]
    pub(crate) fn open_work_obligations_at_cut(
        &self,
        run_id: WorkRunId,
        cut: &FeedPosition,
    ) -> Result<Vec<WorkObligationId>, StoreError> {
        let records = applicable_work_obligations_at_cut_on(&self.connection, run_id, cut)?;
        let mut open = Vec::new();
        for record in records {
            let terminal_at_cut = record
                .resolution_hash
                .as_ref()
                .map(|hash| run_feed_position_for_object_on(&self.connection, run_id, hash))
                .transpose()?
                .is_some_and(|position| position.position <= cut.position);
            if !terminal_at_cut {
                open.push(record.obligation.obligation_id);
            }
        }
        Ok(open)
    }

    /// Resolves and validates the typed category of one run evidence object.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the hash is absent, belongs to another run,
    /// or its canonical object disagrees with the redundant projection.
    pub fn work_evidence_kind(
        &self,
        run_id: WorkRunId,
        evidence_hash: &ObjectHash,
    ) -> Result<WorkEvidenceKind, StoreError> {
        work_evidence_kind_on(&self.connection, run_id, evidence_hash)
    }

    pub(crate) fn load_verification_evidence(
        &self,
        evidence_hash: &ObjectHash,
    ) -> Result<VerificationEvidence, StoreError> {
        expected_verification_projection(&self.connection, evidence_hash)?;
        load_typed_work_object(&self.connection, evidence_hash, "verification_evidence")
    }

    pub(crate) fn load_environment_evidence(
        &self,
        evidence_hash: &ObjectHash,
    ) -> Result<EnvironmentEvidence, StoreError> {
        expected_environment_projection(&self.connection, evidence_hash)?;
        load_typed_work_object(&self.connection, evidence_hash, "environment_evidence")
    }

    /// Reads the current head for an exact work feed.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the head projection cannot be read.
    pub fn work_feed_head(&self, feed: &FeedId) -> Result<i64, StoreError> {
        feed_head(&self.connection, feed)
    }

    /// Returns a deterministic derived status with exact blocker reasons.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when graph or projection data cannot be verified.
    pub fn inspect_work(
        &self,
        work_id: WorkId,
        now: DateTime<Utc>,
    ) -> Result<ReadyWork, StoreError> {
        inspect_work_on(&self.connection, work_id, now)
    }

    /// Whether current local projections satisfy every non-acceptance
    /// completion precondition for this session. The final call must still
    /// supply criterion results and pass authority revalidation atomically.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when canonical checkpoint/evidence projections
    /// are corrupt or SQLite cannot evaluate the graph.
    pub fn work_completion_readiness(
        &self,
        work_id: WorkId,
        session_id: &SessionId,
        now: DateTime<Utc>,
    ) -> Result<(bool, bool), StoreError> {
        let item = load_work_item(&self.connection, work_id)?;
        let claim = self.current_work_claim_for_item(&item)?;
        self.work_completion_readiness_for_item(&item, claim.as_ref(), session_id, now)
    }

    pub(crate) fn work_completion_readiness_for_item(
        &self,
        item: &WorkItem,
        claim: Option<&WorkClaim>,
        session_id: &SessionId,
        now: DateTime<Utc>,
    ) -> Result<(bool, bool), StoreError> {
        let work_id = item.work_id;
        if item.lifecycle != WorkLifecycle::Open {
            return Ok((false, false));
        }
        let Some(run_id) = item.active_run_id else {
            return Ok((false, false));
        };
        let run = load_work_run(&self.connection, run_id)?;
        let Some(claim) = claim else {
            return Ok((false, false));
        };
        if claim.work_id != item.work_id
            || claim.run_id != run_id
            || claim.state != WorkClaimState::Active
            || claim.holder != *session_id
            || claim.expires_at <= now
        {
            return Ok((false, false));
        }
        let live_handoff = self.connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM work_handoff_offers
                 WHERE run_id = ?1 AND state = 'offered' AND expires_at_ms > ?2
             )",
            params![run_id.0.to_string(), now.timestamp_millis()],
            |row| row.get::<_, bool>(0),
        )?;
        let active_blocker = self.connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM work_blockers WHERE work_id = ?1 AND state = 'active'
             )",
            [work_id.0.to_string()],
            |row| row.get::<_, bool>(0),
        )?;
        if live_handoff
            || active_blocker
            || !incomplete_prerequisites(&self.connection, work_id)?.is_empty()
        {
            return Ok((false, false));
        }
        let required_child_count = self.connection.query_row(
            "SELECT COUNT(*) FROM work_items child
             WHERE child.parent_id = ?1
               AND child.child_requirement = 'required'",
            [work_id.0.to_string()],
            |row| row.get::<_, i64>(0),
        )?;
        if required_child_count != 0 {
            let required_child_seal_count =
                required_child_seals(&self.connection, work_id, run.root_execution_id)?.len();
            let root_execution = load_root_execution(&self.connection, run.root_execution_id)?;
            let required_child_waiver_count =
                validated_required_child_waivers(&self.connection, work_id, &root_execution)?.len();
            if usize::try_from(required_child_count).ok()
                != Some(required_child_seal_count + required_child_waiver_count)
            {
                return Ok((false, false));
            }
        }
        let evidence = self.work_run_evidence(run_id)?;
        let Some(checkpoint_hash) = run.last_checkpoint else {
            return Ok((true, false));
        };
        if evidence.is_empty() {
            return Ok((true, false));
        }
        let checkpoint: WorkCheckpoint =
            load_typed_work_object(&self.connection, &checkpoint_hash, "work_checkpoint")?;
        let current_cut = feed_head(&self.connection, &FeedId::RunExecution(run_id))?;
        let checkpoint_cut = checkpoint_feed_end(checkpoint.acknowledged_run_position.position)?;
        Ok((
            true,
            checkpoint.work_id == work_id
                && checkpoint.run_id == run_id
                && checkpoint.claim_id == claim.claim_id
                && checkpoint.claim_fence == claim.fence
                && evidence
                    .iter()
                    .all(|hash| checkpoint.evidence.contains(hash))
                && checkpoint.acknowledged_run_position.feed == FeedId::RunExecution(run_id)
                && checkpoint_cut == current_cut,
        ))
    }

    /// Returns ready work ordered by priority, unblocking value, age, and stable id.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when readiness projections cannot be read or verified.
    pub fn ready_work(
        &self,
        project_id: &crate::domain::ProjectId,
        now: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<ReadyWork>, StoreError> {
        let sql = format!(
            "WITH classified AS (
                 SELECT candidate.work_id, candidate.priority,
                        candidate.created_at_ms,
                        ({PROJECTED_WORK_AVAILABILITY_SQL}) AS availability
                 FROM work_items candidate
                 WHERE candidate.project_id = ?1
                   AND candidate.lifecycle = 'open'
             )
             SELECT work_id FROM classified
             WHERE availability = 'ready'
             ORDER BY priority,
                      (SELECT COUNT(*)
                       FROM work_prerequisites dependency
                       JOIN work_items dependant
                         ON dependant.work_id = dependency.work_id
                       WHERE dependency.prerequisite_id = classified.work_id
                         AND dependant.lifecycle = 'open') DESC,
                      created_at_ms, work_id
             LIMIT ?3"
        );
        let mut statement = self.connection.prepare(&sql)?;
        let ids = statement
            .query_map(
                params![
                    project_id.0.as_str(),
                    now.timestamp_millis(),
                    i64::from(limit.clamp(1, 1_000))
                ],
                |row| row.get::<_, String>(0),
            )?
            .map(|row| parse_work_id(&row?))
            .collect::<Result<Vec<_>, _>>()?;
        ids.into_iter()
            .map(|id| inspect_work_catalog_on(&self.connection, id, now))
            .filter_map(|result| match result {
                Ok(view) if view.availability == WorkAvailability::Ready => Some(Ok(view)),
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            })
            .collect()
    }

    /// Queries every lifecycle/availability class without mutating focus.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when canonical work projections cannot be read.
    pub fn query_work_catalog(
        &self,
        project_id: &crate::domain::ProjectId,
        now: DateTime<Utc>,
        query: &WorkCatalogQuery,
    ) -> Result<WorkCatalogPage, StoreError> {
        let ids = query_work_catalog_ids(&self.connection, project_id, now, query)?;
        let limit = query.limit.clamp(1, 1_000) as usize;
        let mut items = Vec::with_capacity(limit.saturating_add(1));
        for work_id in ids {
            items.push(inspect_work_catalog_on(&self.connection, work_id, now)?);
        }
        let next_after = (items.len() > limit).then(|| items[limit - 1].work.work_id);
        items.truncate(limit);
        Ok(WorkCatalogPage { items, next_after })
    }

    /// Reads immutable entries after a dense position in one exact feed.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the feed contains invalid hashes or cannot be read.
    pub fn work_feed_after(
        &self,
        feed: &FeedId,
        after: i64,
        limit: u32,
    ) -> Result<Vec<WorkFeedEntry>, StoreError> {
        let (feed_kind, feed_id) = feed_parts(feed);
        let mut statement = self.connection.prepare(
            "SELECT position, object_kind, object_hash
             FROM work_feed_entries
             WHERE feed_kind = ?1 AND feed_id = ?2 AND position > ?3
             ORDER BY position LIMIT ?4",
        )?;
        let rows = statement.query_map(
            params![
                feed_kind,
                feed_id,
                after.max(0),
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
            let (position, object_kind, hash) = row?;
            let object_hash =
                ObjectHash::from_stored(hash.clone()).ok_or(StoreError::InvalidStoredHash(hash))?;
            Ok(WorkFeedEntry {
                position: FeedPosition {
                    feed: feed.clone(),
                    position,
                },
                object_kind,
                object_hash,
            })
        })
        .collect()
    }

    /// Replays one exact staged feed interval, inclusive of its upper bound.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the interval or stored hashes are invalid.
    pub fn work_feed_between(
        &self,
        feed: &FeedId,
        after: i64,
        through: i64,
    ) -> Result<Vec<WorkFeedEntry>, StoreError> {
        let distance = through.checked_sub(after).ok_or_else(|| {
            StoreError::InvalidWorkProjection(format!(
                "staged feed interval ({after}, {through}] overflowed"
            ))
        })?;
        if !(0..=1_000).contains(&distance) {
            return Err(StoreError::InvalidWorkProjection(format!(
                "invalid staged feed interval ({after}, {through}]"
            )));
        }
        let (feed_kind, feed_id) = feed_parts(feed);
        let mut statement = self.connection.prepare(
            "SELECT position, object_kind, object_hash
             FROM work_feed_entries
             WHERE feed_kind = ?1 AND feed_id = ?2
               AND position > ?3 AND position <= ?4
             ORDER BY position",
        )?;
        let rows = statement
            .query_map(params![feed_kind, feed_id, after, through], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let expected_count = usize::try_from(distance).map_err(|_| {
            StoreError::InvalidWorkProjection("staged feed interval size overflowed".into())
        })?;
        if rows.len() != expected_count {
            return Err(StoreError::InvalidWorkProjection(format!(
                "staged feed interval ({after}, {through}] is not dense"
            )));
        }
        for (offset, row) in rows.iter().enumerate() {
            let offset = i64::try_from(offset).map_err(|_| {
                StoreError::InvalidWorkProjection(
                    "staged feed dense-position offset overflowed".into(),
                )
            })?;
            let expected_position = after
                .checked_add(offset)
                .and_then(|position| position.checked_add(1))
                .ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "staged feed dense-position arithmetic overflowed".into(),
                    )
                })?;
            if row.0 != expected_position {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "staged feed interval ({after}, {through}] is not dense"
                )));
            }
        }
        rows.into_iter()
            .map(|(position, object_kind, hash)| {
                let object_hash = ObjectHash::from_stored(hash.clone())
                    .ok_or(StoreError::InvalidStoredHash(hash))?;
                Ok(WorkFeedEntry {
                    position: FeedPosition {
                        feed: feed.clone(),
                        position,
                    },
                    object_kind,
                    object_hash,
                })
            })
            .collect()
    }

    /// Reads the newest immutable entries in one exact feed, returned oldest
    /// to newest within the bounded tail.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the feed projection or an object hash is
    /// invalid.
    pub fn work_feed_tail(
        &self,
        feed: &FeedId,
        limit: u32,
    ) -> Result<Vec<WorkFeedEntry>, StoreError> {
        let (feed_kind, feed_id) = feed_parts(feed);
        let mut statement = self.connection.prepare(
            "SELECT position, object_kind, object_hash FROM (
                 SELECT position, object_kind, object_hash
                 FROM work_feed_entries
                 WHERE feed_kind = ?1 AND feed_id = ?2
                 ORDER BY position DESC LIMIT ?3
             ) ORDER BY position",
        )?;
        statement
            .query_map(
                params![feed_kind, feed_id, i64::from(limit.clamp(1, 10_000))],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )?
            .map(|row| {
                let (position, object_kind, hash) = row?;
                Ok(WorkFeedEntry {
                    position: FeedPosition {
                        feed: feed.clone(),
                        position,
                    },
                    object_kind,
                    object_hash: ObjectHash::from_stored(hash.clone())
                        .ok_or(StoreError::InvalidStoredHash(hash))?,
                })
            })
            .collect()
    }

    /// Returns the newest work events for one exact item without applying a
    /// root-wide pre-limit that could hide older item history.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when item/feed identities or hashes are invalid.
    pub fn work_event_tail(
        &self,
        work_id: WorkId,
        limit: u32,
    ) -> Result<Vec<WorkFeedEntry>, StoreError> {
        let item = self.get_work_item(work_id)?;
        let mut statement = self.connection.prepare(
            "SELECT position, object_hash FROM (
                 SELECT entry.position, entry.object_hash
                 FROM work_feed_entries entry
                 JOIN objects object ON object.object_hash = entry.object_hash
                 WHERE entry.feed_kind = 'root_work'
                   AND entry.feed_id = ?1
                   AND entry.object_kind = 'work_event'
                   AND entry.work_id = ?2
                 ORDER BY entry.position DESC LIMIT ?3
             ) ORDER BY position",
        )?;
        statement
            .query_map(
                params![
                    item.root_id.0.to_string(),
                    work_id.0.to_string(),
                    i64::from(limit.clamp(1, 1_000))
                ],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )?
            .map(|row| {
                let (position, hash) = row?;
                let object_hash = ObjectHash::from_stored(hash.clone())
                    .ok_or(StoreError::InvalidStoredHash(hash))?;
                Ok(WorkFeedEntry {
                    position: FeedPosition {
                        feed: FeedId::RootWork(item.root_id),
                        position,
                    },
                    object_kind: "work_event".into(),
                    object_hash,
                })
            })
            .collect()
    }

    /// Counts canonical lifecycle events for one exact work item.
    pub(crate) fn work_event_count(&self, work_id: WorkId) -> Result<usize, StoreError> {
        let item = self.get_work_item(work_id)?;
        let count: i64 = self.connection.query_row(
            "SELECT COUNT(*)
             FROM work_feed_entries entry
             JOIN objects object ON object.object_hash = entry.object_hash
             WHERE entry.feed_kind = 'root_work'
               AND entry.feed_id = ?1
               AND entry.object_kind = 'work_event'
               AND entry.work_id = ?2",
            params![item.root_id.0.to_string(), work_id.0.to_string()],
            |row| row.get(0),
        )?;
        usize::try_from(count).map_err(|_| {
            StoreError::InvalidWorkProjection("work event count overflowed usize".into())
        })
    }

    #[cfg(test)]
    pub(crate) fn append_test_work_event(&mut self, event: &WorkEvent) -> Result<(), StoreError> {
        let transaction = self.begin_work_mutation()?;
        append_work_event(&transaction, &WorkEventDraft::from(event))?;
        transaction.commit()?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn append_test_work_events(
        &mut self,
        events: &[WorkEvent],
    ) -> Result<(), StoreError> {
        let transaction = self.begin_work_mutation()?;
        for event in events {
            append_work_event(&transaction, &WorkEventDraft::from(event))?;
        }
        transaction.commit()?;
        Ok(())
    }
}

const MAX_AMBIGUOUS_WORK_CANDIDATES: usize = 8;

fn resolve_work_ref_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
    work_ref: &str,
) -> Result<WorkItem, StoreError> {
    let work_ref = work_ref.trim();
    if work_ref.is_empty() {
        return Err(StoreError::InvalidWork(
            "work reference must not be empty".into(),
        ));
    }
    let mut statement = connection.prepare(
        "SELECT work_id, COUNT(*) OVER()
         FROM work_items
         WHERE project_id = ?1 AND (short_ref = ?2 OR work_id = ?2)
         ORDER BY work_id LIMIT ?3",
    )?;
    let matches = statement
        .query_map(
            params![
                project_id.0,
                work_ref,
                i64::try_from(MAX_AMBIGUOUS_WORK_CANDIDATES).map_err(|_| {
                    StoreError::InvalidWorkProjection(
                        "ambiguous-work candidate limit overflowed SQLite".into(),
                    )
                })?
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    match matches.as_slice() {
        [(work_id, _)] => load_work_item(connection, parse_work_id(work_id)?),
        [] => Err(StoreError::InvalidWork(format!(
            "work reference {work_ref:?} does not exist in project {:?}",
            project_id.0
        ))),
        _ => {
            let total = usize::try_from(matches[0].1).map_err(|_| {
                StoreError::InvalidWorkProjection(
                    "ambiguous-work candidate count overflowed usize".into(),
                )
            })?;
            let candidates = matches
                .iter()
                .map(|(work_id, _)| load_work_item(connection, parse_work_id(work_id)?))
                .collect::<Result<Vec<_>, StoreError>>()?;
            let more = total.saturating_sub(candidates.len());
            Err(ambiguous_work_reference_error(work_ref, candidates, more))
        }
    }
}

fn command_work_ref_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
    item: &WorkReferenceCandidate,
) -> Result<String, StoreError> {
    match resolve_work_ref_on(connection, project_id, &item.short_ref) {
        Ok(resolved) if resolved.work_id == item.work_id => Ok(item.short_ref.clone()),
        Ok(resolved) => Err(StoreError::InvalidWorkProjection(format!(
            "short reference {:?} resolved to work {:?} instead of {:?}",
            item.short_ref, resolved.work_id.0, item.work_id.0
        ))),
        Err(StoreError::WorkReferenceAmbiguous { .. }) => Ok(item.work_id.0.to_string()),
        Err(error) => Err(error),
    }
}

fn persist_completion_recovery_on(
    transaction: &Transaction<'_>,
    work: &WorkItem,
    cause: WorkCompletionRecoveryCause,
    operation: &str,
    idempotency_key: &str,
    request_hash: &ObjectHash,
) -> Result<WorkCompletionRecovery, StoreError> {
    let recovery = completion_recovery_on(transaction, work, cause)?;
    persist_operation_result(
        transaction,
        operation,
        idempotency_key,
        request_hash,
        &recovery,
    )?;
    Ok(recovery)
}

fn completion_recovery_on(
    connection: &Connection,
    work: &WorkItem,
    cause: WorkCompletionRecoveryCause,
) -> Result<WorkCompletionRecovery, StoreError> {
    let affected_id = match &cause {
        WorkCompletionRecoveryCause::RequiredChildUnsealed { child } => *child,
        _ => work.work_id,
    };
    let affected = if affected_id == work.work_id {
        work.clone()
    } else {
        load_work_item(connection, affected_id)?
    };
    let item = WorkReferenceCandidate {
        work_id: affected.work_id,
        short_ref: affected.short_ref.clone(),
        title: affected.title,
        lifecycle: affected.lifecycle,
    };
    let command_ref = command_work_ref_on(connection, &affected.project_id, &item)?;
    let command = match &cause {
        WorkCompletionRecoveryCause::OpenObligation { obligation_id, .. } => format!(
            "engram work done {command_ref} --note \"retry after host verification for obligation {}\"",
            obligation_id.0
        ),
        WorkCompletionRecoveryCause::RequiredChildUnsealed { .. }
            if item.lifecycle == WorkLifecycle::Open =>
        {
            format!("engram work claim {command_ref}")
        }
        WorkCompletionRecoveryCause::RequiredChildUnsealed { .. } => {
            let parent = WorkReferenceCandidate {
                work_id: work.work_id,
                short_ref: work.short_ref.clone(),
                title: work.title.clone(),
                lifecycle: work.lifecycle,
            };
            let parent_ref = command_work_ref_on(connection, &work.project_id, &parent)?;
            format!(
                "engram work core update --work-ref {parent_ref} --input '{{\"kind\":\"waive_required_child\",\"child\":\"{command_ref}\",\"reason\":\"account for disposed required child\"}}'"
            )
        }
        WorkCompletionRecoveryCause::MissingContribution { participant } => {
            let participant = recovery_command_atom(&participant.0)?;
            format!(
                "engram work handoff {command_ref} --to {participant} --summary \"transfer root so the missing participant can contribute\""
            )
        }
        WorkCompletionRecoveryCause::MissingAcceptance { .. } => {
            format!("engram work done {command_ref} --note \"acceptance verified\"")
        }
    };
    Ok(WorkCompletionRecovery {
        cause,
        item,
        command,
    })
}

fn recovery_command_atom(value: &str) -> Result<&str, StoreError> {
    if !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'@')
        })
    {
        return Ok(value);
    }
    Err(StoreError::InvalidWorkProjection(
        "completion recovery target contains characters that cannot be rendered as a shell-safe CLI argument"
            .into(),
    ))
}

fn ambiguous_work_reference_error(
    reference: &str,
    mut items: Vec<WorkItem>,
    more: usize,
) -> StoreError {
    items.sort_by(|left, right| left.work_id.0.as_bytes().cmp(right.work_id.0.as_bytes()));
    StoreError::WorkReferenceAmbiguous {
        reference: reference.to_owned(),
        candidates: items
            .into_iter()
            .map(|item| WorkReferenceCandidate {
                work_id: item.work_id,
                short_ref: item.short_ref,
                title: item.title,
                lifecycle: item.lifecycle,
            })
            .collect(),
        more,
    }
}

impl SqliteStore {
    pub(super) fn verify_work_projections(&self) -> Result<(usize, Vec<String>), StoreError> {
        Self::verify_work_projections_on(&self.connection)
    }

    fn verify_work_projections_on(
        connection: &Connection,
    ) -> Result<(usize, Vec<String>), StoreError> {
        let mut checked = 0_usize;
        let mut invalid = Vec::new();
        let mut seen_events = HashSet::new();
        let mut work_items = HashMap::new();
        let mut runs = HashMap::new();
        let mut root_executions = HashMap::new();
        let mut claims = HashMap::new();
        let mut handoffs = HashMap::new();
        let mut blockers = HashMap::new();
        let mut prerequisite_rows = HashMap::new();
        let mut blocker_rows = HashMap::new();
        let mut relation_bases = HashMap::new();
        let mut evidence_rows = HashMap::new();
        let mut gate_heads = HashMap::new();
        let mut completion_rows = HashMap::new();

        let mut statement = connection.prepare(
            "SELECT entry.feed_id, entry.position, entry.object_hash,
                    object.object_kind, object.canonical_json
             FROM work_feed_entries entry
             LEFT JOIN objects object ON object.object_hash = entry.object_hash
             WHERE entry.feed_kind = 'project' AND entry.object_kind = 'work_event'
             ORDER BY entry.feed_id, entry.position",
        )?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, Option<Vec<u8>>>(4)?,
            ))
        })?;
        for row in rows {
            let (feed_id, position, stored_hash, object_kind, bytes) = row?;
            checked += 1;
            let label = format!("work_event:{feed_id}:{position}:{stored_hash}");
            let Some(hash) = ObjectHash::from_stored(stored_hash) else {
                invalid.push(label);
                continue;
            };
            let Some(bytes) = bytes else {
                invalid.push(label);
                continue;
            };
            if object_kind.as_deref() != Some("work_event") || !seen_events.insert(hash.clone()) {
                invalid.push(label);
                continue;
            }
            let event = CanonicalObject::verify(&hash, bytes)
                .and_then(|object| object.decode::<WorkEvent>());
            let Ok(event) = event else {
                invalid.push(label);
                continue;
            };
            let internally_bound = event.schema_version == SCHEMA_VERSION
                && event.project_id.0 == feed_id
                && event.work_id == event.work.work_id
                && event.root_id == event.work.root_id
                && event.project_id == event.work.project_id
                && event.revision == event.work.revision
                && event.run.as_ref().is_none_or(|run| {
                    event.run_id == Some(run.run_id) && run.work_id == event.work_id
                })
                && event.root_execution.as_ref().is_none_or(|execution| {
                    execution.project_id == event.project_id && execution.root_id == event.root_id
                })
                && event.claim.as_ref().is_none_or(|claim| {
                    claim.work_id == event.work_id && event.run_id == Some(claim.run_id)
                })
                && event.handoff_offer.as_ref().is_none_or(|offer| {
                    offer.work_id == event.work_id && event.run_id == Some(offer.run_id)
                })
                && event
                    .blocker
                    .as_ref()
                    .is_none_or(|blocker| blocker.work_id == event.work_id);
            if !internally_bound {
                invalid.push(label);
                continue;
            }
            let current_basis = relation_bases
                .entry(event.work_id)
                .or_insert_with(empty_work_relation_basis);
            if apply_work_relation_transition(
                current_basis,
                &event.transition,
                event.blocker.as_ref(),
            )
            .is_err()
                || work_relation_fingerprint(current_basis)
                    .map_or(true, |actual| actual != event.relation_fingerprint)
            {
                invalid.push(format!("{label}:relation_fingerprint"));
                continue;
            }
            match &event.transition {
                WorkTransition::Created {
                    prerequisites,
                    authority_grant,
                } => {
                    let required_operation = if event.work.parent_id.is_some() {
                        WorkAuthorityOperation::Plan
                    } else {
                        WorkAuthorityOperation::RootCreate
                    };
                    let authority_is_bound = load_typed_work_object::<WorkAuthorityGrant>(
                        connection,
                        authority_grant,
                        "work_authority_grant",
                    )
                    .is_ok_and(|grant| {
                        grant.project_id == event.project_id
                            && grant.policy_ref == event.work.authority_policy_ref
                            && grant.subject_actor_id == event.actor.actor_id
                            && assurance_covers(event.actor.assurance, grant.assurance)
                            && grant.operations.contains(&required_operation)
                            && grant.issued_at <= event.created_at
                            && grant.valid_until > event.created_at
                    });
                    if !authority_is_bound {
                        invalid.push(format!("{label}:invalid_admission_authority"));
                    }
                    for prerequisite in prerequisites {
                        prerequisite_rows.insert(
                            (event.work_id.0.to_string(), prerequisite.0.to_string()),
                            hash.as_str().to_owned(),
                        );
                    }
                }
                WorkTransition::Claimed {
                    authority_grant, ..
                } => {
                    let authority_is_bound = load_typed_work_object::<WorkAuthorityGrant>(
                        connection,
                        authority_grant,
                        "work_authority_grant",
                    )
                    .is_ok_and(|grant| {
                        grant.project_id == event.project_id
                            && grant.policy_ref == event.work.authority_policy_ref
                            && grant.subject_actor_id == event.actor.actor_id
                            && assurance_covers(event.actor.assurance, grant.assurance)
                            && grant.operations.contains(&WorkAuthorityOperation::Claim)
                            && grant.issued_at <= event.created_at
                            && grant.valid_until > event.created_at
                            && authority_scope_matches(
                                &grant.scope,
                                AuthorityTarget {
                                    project_id: &event.project_id,
                                    policy_ref: &event.work.authority_policy_ref,
                                    work_id: Some(event.work_id),
                                    root_id: Some(event.root_id),
                                    run_id: event.run_id,
                                },
                            )
                    });
                    if !authority_is_bound {
                        invalid.push(format!("{label}:invalid_claim_authority"));
                    }
                }
                WorkTransition::HandedOff {
                    authority_grant, ..
                } => {
                    let authority_is_bound = load_typed_work_object::<WorkAuthorityGrant>(
                        connection,
                        authority_grant,
                        "work_authority_grant",
                    )
                    .is_ok_and(|grant| {
                        grant.project_id == event.project_id
                            && grant.policy_ref == event.work.authority_policy_ref
                            && grant.subject_actor_id == event.actor.actor_id
                            && assurance_covers(event.actor.assurance, grant.assurance)
                            && grant.operations.contains(&WorkAuthorityOperation::Claim)
                            && grant.issued_at <= event.created_at
                            && grant.valid_until > event.created_at
                            && authority_scope_matches(
                                &grant.scope,
                                AuthorityTarget {
                                    project_id: &event.project_id,
                                    policy_ref: &event.work.authority_policy_ref,
                                    work_id: Some(event.work_id),
                                    root_id: Some(event.root_id),
                                    run_id: event.run_id,
                                },
                            )
                    });
                    if !authority_is_bound {
                        invalid.push(format!("{label}:invalid_handoff_authority"));
                    }
                }
                WorkTransition::Disposed {
                    lifecycle,
                    replacement_id,
                    authority_grant,
                    ..
                } => {
                    let transition_is_bound = *lifecycle == event.work.lifecycle
                        && *replacement_id == event.work.superseded_by
                        && matches!(
                            lifecycle,
                            WorkLifecycle::Cancelled | WorkLifecycle::Superseded
                        );
                    let authority_is_bound = load_typed_work_object::<WorkAuthorityGrant>(
                        connection,
                        authority_grant,
                        "work_authority_grant",
                    )
                    .is_ok_and(|grant| {
                        grant.project_id == event.project_id
                            && grant.policy_ref == event.work.authority_policy_ref
                            && grant.subject_actor_id == event.actor.actor_id
                            && assurance_covers(event.actor.assurance, grant.assurance)
                            && grant.operations.contains(&WorkAuthorityOperation::Dispose)
                            && grant.issued_at <= event.created_at
                            && grant.valid_until > event.created_at
                            && authority_scope_matches(
                                &grant.scope,
                                AuthorityTarget {
                                    project_id: &event.project_id,
                                    policy_ref: &event.work.authority_policy_ref,
                                    work_id: Some(event.work_id),
                                    root_id: Some(event.root_id),
                                    run_id: event.run_id,
                                },
                            )
                    });
                    if !transition_is_bound || !authority_is_bound {
                        invalid.push(format!("{label}:invalid_disposal_binding"));
                    }
                }
                WorkTransition::RequiredChildWaived {
                    child_id,
                    child_revision,
                    authority_grant,
                    ..
                } => {
                    let child = load_work_item(connection, *child_id);
                    let transition_is_bound = child.as_ref().is_ok_and(|child| {
                        child.parent_id == Some(event.work_id)
                            && child.child_requirement == ChildRequirement::Required
                            && matches!(
                                child.lifecycle,
                                WorkLifecycle::Cancelled | WorkLifecycle::Superseded
                            )
                            && child.revision == *child_revision
                            && event.root_execution.as_ref().is_some_and(|execution| {
                                execution.required_child_waivers.iter().any(|waiver| {
                                    waiver.work_id == *child_id
                                        && waiver.work_revision == *child_revision
                                        && waiver.authority_grant == *authority_grant
                                })
                            })
                    });
                    let authority_is_bound = child.as_ref().is_ok_and(|child| {
                        load_typed_work_object::<WorkAuthorityGrant>(
                            connection,
                            authority_grant,
                            "work_authority_grant",
                        )
                        .is_ok_and(|grant| {
                            grant.project_id == event.project_id
                                && grant.policy_ref == child.authority_policy_ref
                                && grant.subject_actor_id == event.actor.actor_id
                                && assurance_covers(event.actor.assurance, grant.assurance)
                                && grant
                                    .operations
                                    .contains(&WorkAuthorityOperation::CompletionWaiver)
                                && grant.issued_at <= event.created_at
                                && grant.valid_until > event.created_at
                                && authority_scope_matches(
                                    &grant.scope,
                                    AuthorityTarget {
                                        project_id: &event.project_id,
                                        policy_ref: &child.authority_policy_ref,
                                        work_id: Some(*child_id),
                                        root_id: Some(event.root_id),
                                        run_id: child.active_run_id,
                                    },
                                )
                        })
                    });
                    if !transition_is_bound || !authority_is_bound {
                        invalid.push(format!("{label}:invalid_required_child_waiver"));
                    }
                }
                WorkTransition::PrerequisiteAdded {
                    prerequisite_id, ..
                } => {
                    prerequisite_rows.insert(
                        (event.work_id.0.to_string(), prerequisite_id.0.to_string()),
                        hash.as_str().to_owned(),
                    );
                }
                WorkTransition::PrerequisiteRemoved {
                    prerequisite_id, ..
                } => {
                    prerequisite_rows
                        .remove(&(event.work_id.0.to_string(), prerequisite_id.0.to_string()));
                }
                WorkTransition::Blocked { blocker_id } => {
                    if event
                        .blocker
                        .as_ref()
                        .map(|blocker| blocker.blocker_id.as_str())
                        == Some(blocker_id.as_str())
                    {
                        blocker_rows.insert(
                            blocker_id.clone(),
                            (
                                "active".to_owned(),
                                hash.as_str().to_owned(),
                                None::<String>,
                            ),
                        );
                    } else {
                        invalid.push(label.clone());
                    }
                }
                WorkTransition::Unblocked { blocker_id } => {
                    if let Some((state, _, cleared)) = blocker_rows.get_mut(blocker_id) {
                        *state = "cleared".into();
                        *cleared = Some(hash.as_str().to_owned());
                    } else {
                        invalid.push(format!("{label}:missing_block_event"));
                    }
                }
                WorkTransition::EvidenceAdded { evidence } => {
                    match load_typed_work_object::<WorkEvidence>(
                        connection,
                        evidence,
                        "work_evidence",
                    ) {
                        Ok(value)
                            if value.work_id == event.work_id
                                && Some(value.run_id) == event.run_id =>
                        {
                            if validate_gate_evidence_chain(evidence, &value, &mut gate_heads)
                                .is_err()
                            {
                                invalid.push(format!("{label}:invalid_gate_chain"));
                                continue;
                            }
                            evidence_rows.insert(
                                evidence.as_str().to_owned(),
                                EvidenceProjectionRow {
                                    work_id: value.work_id.0.to_string(),
                                    run_id: value.run_id.0.to_string(),
                                    evidence_kind: "generic".into(),
                                    workspace_id: None,
                                    source_revision: None,
                                    producer_session_id: None,
                                    producer_observation_hash: None,
                                    check_fingerprint: None,
                                    verification_result: None,
                                    observed_at_ms: None,
                                    environment_fingerprint: None,
                                    environment_evidence_hash: None,
                                    components_json: None,
                                },
                            );
                        }
                        _ => invalid.push(format!("{label}:invalid_evidence_binding")),
                    }
                }
                WorkTransition::MemoryCaptured { version, assertion } => {
                    let binding = load_typed_work_object::<MemoryVersion>(
                        connection,
                        version,
                        "memory_version",
                    )
                    .and_then(|memory| {
                        let assertion_event = load_typed_work_object::<MemoryAssertionEvent>(
                            connection,
                            assertion,
                            "memory_assertion_event",
                        )?;
                        Ok((memory, assertion_event))
                    });
                    let valid = binding.is_ok_and(|(memory, assertion_event)| {
                        matches!(
                            memory.scope,
                            crate::domain::Scope::Work { ref project, work }
                                if project == &event.project_id && work == event.work_id
                        ) && memory.memory_id == assertion_event.memory_id
                            && assertion_event.version == *version
                            && memory.actor == event.actor
                            && assertion_event.actor == event.actor
                            && memory.created_at == event.created_at
                            && assertion_event.created_at == event.created_at
                            && event.claim.as_ref().is_some_and(|claim| {
                                event.actor.session_id.as_ref() == Some(&claim.holder)
                                    && claim.state == WorkClaimState::Active
                                    && claim.expires_at > event.created_at
                            })
                    });
                    if !valid {
                        invalid.push(format!("{label}:invalid_memory_capture_binding"));
                    }
                }
                WorkTransition::TypedEvidenceAdded {
                    evidence,
                    evidence_kind,
                } => {
                    let expected = match evidence_kind {
                        WorkEvidenceKind::Generic => None,
                        WorkEvidenceKind::Verification => {
                            expected_verification_projection(connection, evidence).ok()
                        }
                        WorkEvidenceKind::Environment => {
                            expected_environment_projection(connection, evidence).ok()
                        }
                    };
                    if let Some(expected) = expected.filter(|expected| {
                        expected.work_id == event.work_id.0.to_string()
                            && event
                                .run_id
                                .is_some_and(|run_id| expected.run_id == run_id.0.to_string())
                    }) {
                        evidence_rows.insert(evidence.as_str().to_owned(), expected);
                    } else {
                        invalid.push(format!("{label}:invalid_typed_evidence_binding"));
                    }
                }
                WorkTransition::Completed { seal } => {
                    match load_typed_work_object::<CompletionSeal>(
                        connection,
                        seal,
                        "completion_seal",
                    ) {
                        Ok(value)
                            if value.work_id == event.work_id
                                && value.root_id == event.root_id
                                && Some(value.run_id) == event.run_id
                                && value.completion_cut.feed
                                    == FeedId::RunExecution(value.run_id)
                                && event.run.as_ref().is_some_and(|run| {
                                    run.state == WorkRunState::Completed
                                        && run.completion_seal.as_ref() == Some(seal)
                                }) =>
                        {
                            completion_rows.insert(
                                seal.as_str().to_owned(),
                                (
                                    value.work_id.0.to_string(),
                                    value.run_id.0.to_string(),
                                    value.root_execution_id.0.to_string(),
                                    serde_json::to_value(value)?,
                                ),
                            );
                        }
                        _ => invalid.push(format!("{label}:invalid_completion_binding")),
                    }
                }
                _ => {}
            }
            work_items.insert(
                event.work_id.0.to_string(),
                serde_json::to_value(&event.work)?,
            );
            if let Some(run) = event.run {
                runs.insert(run.run_id.0.to_string(), serde_json::to_value(run)?);
            }
            if let Some(execution) = event.root_execution {
                root_executions.insert(
                    execution.root_execution_id.0.to_string(),
                    serde_json::to_value(execution)?,
                );
            }
            if let Some(claim) = event.claim {
                claims.insert(claim.run_id.0.to_string(), serde_json::to_value(claim)?);
            }
            if let Some(offer) = event.handoff_offer {
                handoffs.insert(offer.offer_id.0.to_string(), serde_json::to_value(offer)?);
            }
            if let Some(blocker) = event.blocker {
                blockers.insert(blocker.blocker_id.clone(), serde_json::to_value(blocker)?);
            }
        }
        drop(statement);

        verify_json_projection(
            connection,
            "work_item",
            "SELECT work_id, item_json FROM work_items ORDER BY work_id",
            &work_items,
            &mut checked,
            &mut invalid,
        )?;
        verify_json_projection(
            connection,
            "work_run",
            "SELECT run_id, run_json FROM work_runs ORDER BY run_id",
            &runs,
            &mut checked,
            &mut invalid,
        )?;
        verify_json_projection(
            connection,
            "work_root_execution",
            "SELECT root_execution_id, execution_json FROM work_root_executions ORDER BY root_execution_id",
            &root_executions,
            &mut checked,
            &mut invalid,
        )?;
        verify_json_projection(
            connection,
            "work_claim",
            "SELECT run_id, claim_json FROM work_claims ORDER BY run_id",
            &claims,
            &mut checked,
            &mut invalid,
        )?;
        verify_json_projection(
            connection,
            "work_handoff_offer",
            "SELECT offer_id, offer_json FROM work_handoff_offers ORDER BY offer_id",
            &handoffs,
            &mut checked,
            &mut invalid,
        )?;
        verify_json_projection(
            connection,
            "work_blocker",
            "SELECT blocker_id, blocker_json FROM work_blockers ORDER BY blocker_id",
            &blockers,
            &mut checked,
            &mut invalid,
        )?;
        verify_prerequisite_rows(connection, &prerequisite_rows, &mut checked, &mut invalid)?;
        verify_blocker_rows(connection, &blocker_rows, &mut checked, &mut invalid)?;
        verify_evidence_rows(connection, &evidence_rows, &mut checked, &mut invalid)?;
        verify_obligation_rows(connection, &mut checked, &mut invalid)?;
        verify_completion_rows(connection, &completion_rows, &mut checked, &mut invalid)?;
        verify_work_feed_integrity(connection, &work_items, &mut checked, &mut invalid)?;
        verify_work_catalog_projections(connection, &mut checked, &mut invalid)?;
        verify_work_scalar_bindings(connection, &mut checked, &mut invalid)?;
        verify_canonical_work_rows(connection, &mut checked, &mut invalid)?;
        verify_authority_revocation_bindings(connection, &mut checked, &mut invalid)?;
        verify_required_child_waiver_bindings(connection, &mut checked, &mut invalid)?;
        verify_work_protocol_attempts(connection, &mut checked, &mut invalid)?;
        verify_anchored_memory_feeds(connection, &mut checked, &mut invalid)?;
        Ok((checked, invalid))
    }

    /// Completes one run only after acceptance, evidence, graph, and fence checks.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when authority is stale, acceptance is incomplete,
    /// evidence is invalid, graph barriers remain, or persistence fails.
    pub fn complete_work<R: Redactor>(
        &mut self,
        request: &CompleteWorkRequest,
        redactor: &R,
    ) -> Result<CompletionSeal, StoreError> {
        match self.complete_work_internal(request, redactor, false)? {
            CompleteWorkStorageResult::Completed(seal) => Ok(*seal),
            CompleteWorkStorageResult::Recovery(_) => Err(StoreError::InvalidWorkProjection(
                "core completion unexpectedly returned an ambient recovery receipt".into(),
            )),
        }
    }

    pub(crate) fn complete_work_for_protocol<R: Redactor>(
        &mut self,
        request: &CompleteWorkRequest,
        redactor: &R,
    ) -> Result<CompleteWorkStorageResult, StoreError> {
        self.complete_work_internal(request, redactor, true)
    }

    fn complete_work_internal<R: Redactor>(
        &mut self,
        request: &CompleteWorkRequest,
        redactor: &R,
        persist_recovery: bool,
    ) -> Result<CompleteWorkStorageResult, StoreError> {
        inspect_work_request(redactor, request)?;
        assert_actor_session(&request.actor, &request.holder)?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(seal) = replay_operation::<CompletionSeal>(
            &transaction,
            "complete_work",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(CompleteWorkStorageResult::Completed(Box::new(seal)));
        }
        if persist_recovery
            && let Some(recovery) = replay_operation::<WorkCompletionRecovery>(
                &transaction,
                "complete_work_recovery",
                &request.idempotency_key,
                request_object.hash(),
            )?
        {
            transaction.commit()?;
            return Ok(CompleteWorkStorageResult::Recovery(recovery));
        }
        expire_handoff_offers(
            &transaction,
            request.run_id,
            request.completed_at,
            &request.actor,
        )?;
        let (mut item, mut run, mut claim) = validate_live_claim_on(
            &transaction,
            request.work_id,
            request.run_id,
            request.expected_work_revision,
            &request.holder,
            request.claim_id,
            request.claim_fence,
            request.completed_at,
            false,
        )?;
        let offered_handoffs = transaction.query_row(
            "SELECT COUNT(*) FROM work_handoff_offers WHERE run_id = ?1 AND state = 'offered'",
            [run.run_id.0.to_string()],
            |row| row.get::<_, i64>(0),
        )?;
        if offered_handoffs != 0 {
            return Err(StoreError::InvalidWorkProjection(
                "completion cannot terminalize a run with an offered handoff".into(),
            ));
        }
        let relation_basis = validated_current_work_relation_basis(&transaction, item.work_id)?;
        if !relation_basis.active_blockers.is_empty() {
            return Err(StoreError::WorkCompletionRefused {
                work: item.work_id,
                reason: "one or more explicit blockers remain active".into(),
            });
        }
        let checkpoint =
            run.last_checkpoint
                .clone()
                .ok_or_else(|| StoreError::WorkCompletionRefused {
                    work: item.work_id,
                    reason: "the current run has no checkpoint".into(),
                })?;
        let checkpoint_value: WorkCheckpoint =
            load_typed_work_object(&transaction, &checkpoint, "work_checkpoint")?;
        if checkpoint_value.work_id != item.work_id
            || checkpoint_value.run_id != run.run_id
            || checkpoint_value.claim_id != claim.claim_id
            || checkpoint_value.claim_fence != claim.fence
        {
            return Err(StoreError::WorkCompletionRefused {
                work: item.work_id,
                reason: "the latest checkpoint was not written under the completing claim fence"
                    .into(),
            });
        }
        let evidence = unique_hashes(&request.evidence);
        if evidence.is_empty() {
            return Err(StoreError::WorkCompletionRefused {
                work: item.work_id,
                reason: "completion requires at least one evidence object".into(),
            });
        }
        ensure_run_evidence(&transaction, run.run_id, &evidence)?;
        if !evidence
            .iter()
            .all(|hash| checkpoint_value.evidence.contains(hash))
        {
            return Err(StoreError::WorkCompletionRefused {
                work: item.work_id,
                reason:
                    "the final checkpoint does not acknowledge every completion evidence object"
                        .into(),
            });
        }
        let acceptance = match validate_acceptance(
            &transaction,
            &item,
            run.run_id,
            &evidence,
            &request.acceptance,
            request.actor.assurance,
        ) {
            Ok(value) => value,
            Err(StoreError::WorkCompletionRecoveryRequired { cause, .. }) if persist_recovery => {
                let recovery = persist_completion_recovery_on(
                    &transaction,
                    &item,
                    cause,
                    "complete_work_recovery",
                    &request.idempotency_key,
                    request_object.hash(),
                )?;
                transaction.commit()?;
                return Ok(CompleteWorkStorageResult::Recovery(recovery));
            }
            Err(error) => return Err(error),
        };
        resolve_work_authority(
            &transaction,
            &request.drain.decision,
            &request.actor,
            WorkAuthorityOperation::CompletionDrain,
            AuthorityTarget {
                project_id: &item.project_id,
                policy_ref: &item.authority_policy_ref,
                work_id: Some(item.work_id),
                root_id: Some(item.root_id),
                run_id: Some(run.run_id),
            },
            request.completed_at,
        )?;
        let drain = request.drain.clone();
        if !drain.reconciled_action_outcomes.is_empty()
            || !drain.released_resource_leases.is_empty()
        {
            return Err(StoreError::WorkCompletionRefused {
                work: item.work_id,
                reason: "V1 completion drain accepts only a grant-backed zero-linked-state attestation until action and resource projections are linked to work runs".into(),
            });
        }
        let incomplete = incomplete_prerequisite_projections(&transaction, item.work_id)?;
        if !incomplete.is_empty() {
            return Err(StoreError::WorkCompletionRefused {
                work: item.work_id,
                reason: format!("prerequisites remain incomplete: {incomplete:?}"),
            });
        }
        let mut root_execution = load_root_execution(&transaction, run.root_execution_id)?;
        let required_child_seals =
            required_child_seals(&transaction, item.work_id, run.root_execution_id)?;
        let required_child_waivers =
            validated_required_child_waivers(&transaction, item.work_id, &root_execution)?;
        let unfinished_optional_children =
            unfinished_optional_children(&transaction, item.work_id)?;
        let required_child_count = transaction.query_row(
            "SELECT COUNT(*) FROM work_items child
             WHERE child.parent_id = ?1
               AND child.child_requirement = 'required'",
            [item.work_id.0.to_string()],
            |row| row.get::<_, i64>(0),
        )?;
        if usize::try_from(required_child_count).ok()
            != Some(required_child_seals.len() + required_child_waivers.len())
        {
            let sealed_children = required_child_seals
                .iter()
                .map(|hash| {
                    load_typed_work_object::<CompletionSeal>(&transaction, hash, "completion_seal")
                        .map(|seal| seal.work_id)
                })
                .collect::<Result<HashSet<_>, StoreError>>()?;
            let waived_children = required_child_waivers
                .iter()
                .map(|waiver| waiver.work_id)
                .collect::<HashSet<_>>();
            let mut statement = transaction.prepare(
                "SELECT work_id FROM work_items
                 WHERE parent_id = ?1 AND child_requirement = 'required'
                 ORDER BY work_id",
            )?;
            let required_children = statement
                .query_map([item.work_id.0.to_string()], |row| row.get::<_, String>(0))?
                .collect::<Result<Vec<_>, _>>()?;
            drop(statement);
            let child = required_children
                .into_iter()
                .map(|value| parse_work_id(&value))
                .collect::<Result<Vec<_>, StoreError>>()?
                .into_iter()
                .find(|child| !sealed_children.contains(child) && !waived_children.contains(child))
                .ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "required-child barrier count disagrees with its accounted identities"
                            .into(),
                    )
                })?;
            let child_item = load_work_item(&transaction, child)?;
            if child_item.lifecycle == WorkLifecycle::Completed {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "completed required child {child:?} has no completion seal in the active root execution"
                )));
            }
            let cause = WorkCompletionRecoveryCause::RequiredChildUnsealed { child };
            if persist_recovery {
                let recovery = persist_completion_recovery_on(
                    &transaction,
                    &item,
                    cause,
                    "complete_work_recovery",
                    &request.idempotency_key,
                    request_object.hash(),
                )?;
                transaction.commit()?;
                return Ok(CompleteWorkStorageResult::Recovery(recovery));
            }
            return Err(StoreError::WorkCompletionRecoveryRequired {
                work: item.work_id,
                cause,
            });
        }
        let completion_cut = FeedPosition {
            feed: FeedId::RunExecution(run.run_id),
            position: feed_head(&transaction, &FeedId::RunExecution(run.run_id))?,
        };
        let checkpoint_cut =
            checkpoint_feed_end(checkpoint_value.acknowledged_run_position.position)?;
        if checkpoint_value.acknowledged_run_position.feed != FeedId::RunExecution(run.run_id)
            || checkpoint_cut != completion_cut.position
        {
            return Err(StoreError::WorkCompletionRefused {
                work: item.work_id,
                reason: "the final checkpoint does not reach the current pre-seal run-feed cut"
                    .into(),
            });
        }
        let obligations = match completion_obligation_basis_on(
            &transaction,
            item.work_id,
            run.run_id,
            &completion_cut,
        ) {
            Ok(value) => value,
            Err(StoreError::OpenWorkObligations { obligations, .. }) if persist_recovery => {
                let obligation = obligations.first().ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "open-obligation refusal contains no exact obligation".into(),
                    )
                })?;
                let cause = WorkCompletionRecoveryCause::OpenObligation {
                    obligation_id: obligation.obligation_id,
                    definition: obligation.definition.clone(),
                    required_check: obligation.required_check,
                };
                let recovery = persist_completion_recovery_on(
                    &transaction,
                    &item,
                    cause,
                    "complete_work_recovery",
                    &request.idempotency_key,
                    request_object.hash(),
                )?;
                transaction.commit()?;
                return Ok(CompleteWorkStorageResult::Recovery(recovery));
            }
            Err(error) => return Err(error),
        };
        let environment =
            completion_environment_basis_on(&transaction, run.run_id, &completion_cut)?;
        if environment.len() > MAX_COMPLETION_ENVIRONMENT_EVIDENCE {
            return Err(StoreError::WorkCompletionRefused {
                work: item.work_id,
                reason: format!(
                    "completion cites {} environment records; checkpoint fewer environment records (maximum {})",
                    environment.len(),
                    MAX_COMPLETION_ENVIRONMENT_EVIDENCE
                ),
            });
        }
        if live_descendant_execution_authority(&transaction, item.work_id, request.completed_at)? {
            return Err(StoreError::WorkCompletionRefused {
                work: item.work_id,
                reason: "completion requires every descendant claim and handoff offer to be released, completed, or expired".into(),
            });
        }
        let accepted_work_revision = CanonicalObject::freeze(&item)?;
        SqliteStore::insert_object(&transaction, "work_item_revision", &accepted_work_revision)?;
        expect_root_contributor(&mut root_execution, &claim.holder);
        add_root_contribution(&mut root_execution, &claim.holder, &checkpoint);
        let root_authority = if item.work_id == item.root_id {
            let decision = request.root_authority.as_ref().ok_or_else(|| {
                StoreError::WorkCompletionRefused {
                    work: item.work_id,
                    reason: "root completion requires an explicit lifecycle authority decision"
                        .into(),
                }
            })?;
            resolve_work_authority(
                &transaction,
                decision,
                &request.actor,
                WorkAuthorityOperation::RootComplete,
                AuthorityTarget {
                    project_id: &item.project_id,
                    policy_ref: &item.authority_policy_ref,
                    work_id: Some(item.work_id),
                    root_id: Some(item.root_id),
                    run_id: Some(run.run_id),
                },
                request.completed_at,
            )?;
            if let Some(participant) = first_unaccounted_root_contributor(&root_execution) {
                let cause = WorkCompletionRecoveryCause::MissingContribution {
                    participant: participant.clone(),
                };
                if persist_recovery {
                    let recovery = persist_completion_recovery_on(
                        &transaction,
                        &item,
                        cause,
                        "complete_work_recovery",
                        &request.idempotency_key,
                        request_object.hash(),
                    )?;
                    transaction.commit()?;
                    return Ok(CompleteWorkStorageResult::Recovery(recovery));
                }
                return Err(StoreError::WorkCompletionRecoveryRequired {
                    work: item.work_id,
                    cause,
                });
            }
            Some(decision.clone())
        } else {
            if request.root_authority.is_some() {
                return Err(StoreError::InvalidWork(
                    "root completion authority is only valid for root work".into(),
                ));
            }
            None
        };
        let seal = CompletionSeal {
            schema_version: SCHEMA_VERSION,
            work_id: item.work_id,
            root_id: item.root_id,
            root_execution_id: run.root_execution_id,
            run_id: run.run_id,
            run_generation: run.generation,
            accepted_work_revision: item.revision,
            accepted_work_revision_hash: accepted_work_revision.hash().clone(),
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
            completion_cut,
            checkpoint: Some(checkpoint),
            evidence,
            acceptance,
            obligation_schema_version: COMPLETION_OBLIGATION_SCHEMA_VERSION,
            obligations,
            environment_schema_version: COMPLETION_ENVIRONMENT_SCHEMA_VERSION,
            environment,
            required_child_seals,
            required_child_waivers,
            unfinished_optional_children,
            expected_contributors: root_execution.expected_contributors.clone(),
            contributions: root_execution.contributions.clone(),
            waivers: root_execution.waivers.clone(),
            root_authority,
            drain,
            actor: request.actor.clone(),
            completed_at: request.completed_at,
        };
        validate_completion_seal_obligation_basis_on(&transaction, &seal)?;
        validate_completion_seal_environment_basis_on(&transaction, &seal)?;
        validate_completion_seal_children_on(&transaction, &seal, 0)?;
        let seal_object = CanonicalObject::freeze(&seal)?;
        SqliteStore::insert_object(&transaction, "completion_seal", &seal_object)?;
        transaction.execute(
            "INSERT INTO work_completion_seals (
                 seal_hash, work_id, run_id, root_execution_id, seal_json
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                seal_object.hash().as_str(),
                item.work_id.0.to_string(),
                run.run_id.0.to_string(),
                run.root_execution_id.0.to_string(),
                serde_json::to_vec(&seal)?
            ],
        )?;

        claim.state = WorkClaimState::Completed;
        claim.revision += 1;
        claim.fence += 1;
        claim.expires_at = request.completed_at;
        run.state = WorkRunState::Completed;
        run.completion_seal = Some(seal_object.hash().clone());
        run.revision += 1;
        run.updated_at = request.completed_at;
        item.lifecycle = WorkLifecycle::Completed;
        item.active_run_id = None;
        item.revision += 1;
        item.updated_at = request.completed_at;
        persist_claim(&transaction, &claim)?;
        persist_work_run(&transaction, &run, claim.fence)?;
        persist_work_item(&transaction, &item)?;

        if item.work_id == item.root_id {
            root_execution.state = RootExecutionState::Completed;
            root_execution
                .required_child_seals
                .clone_from(&seal.required_child_seals);
        } else if item.child_requirement == ChildRequirement::Required {
            root_execution
                .required_child_seals
                .push(seal_object.hash().clone());
            root_execution
                .required_child_seals
                .sort_by(|left, right| left.as_str().cmp(right.as_str()));
            root_execution.required_child_seals.dedup();
        }
        root_execution.revision += 1;
        root_execution.updated_at = request.completed_at;
        persist_root_execution(&transaction, &root_execution)?;

        let event = WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: Some(run.run_id),
            revision: item.revision,
            work: item.clone(),
            run: Some(run.clone()),
            root_execution: Some(root_execution.clone()),
            claim: Some(claim.clone()),
            handoff_offer: None,
            blocker: None,
            transition: WorkTransition::Completed {
                seal: seal_object.hash().clone(),
            },
            actor: request.actor.clone(),
            created_at: request.completed_at,
        };
        append_work_event(&transaction, &event)?;
        persist_operation_result(
            &transaction,
            "complete_work",
            &request.idempotency_key,
            request_object.hash(),
            &seal,
        )?;
        transaction.commit()?;
        Ok(CompleteWorkStorageResult::Completed(Box::new(seal)))
    }

    /// Reopens completed work as a clean run generation without reviving authority.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the expected revision changed, an ancestor
    /// already consumed the child seal, or the new generation cannot be persisted.
    pub fn reopen_work<R: Redactor>(
        &mut self,
        request: &ReopenWorkRequest,
        redactor: &R,
    ) -> Result<WorkRun, StoreError> {
        inspect_work_request(redactor, request)?;
        let reason = normalize_text(&request.reason, "reopen reason")?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(run) = replay_operation::<WorkRun>(
            &transaction,
            "reopen_work",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(run);
        }
        let mut item = load_work_item(&transaction, request.work_id)?;
        assert_revision(&item, request.expected_work_revision)?;
        resolve_work_authority(
            &transaction,
            &request.authority,
            &request.actor,
            WorkAuthorityOperation::Reopen,
            AuthorityTarget {
                project_id: &item.project_id,
                policy_ref: &item.authority_policy_ref,
                work_id: Some(item.work_id),
                root_id: Some(item.root_id),
                run_id: item.active_run_id,
            },
            request.reopened_at,
        )?;
        if item.lifecycle != WorkLifecycle::Completed {
            return Err(StoreError::InvalidWork(
                "only completed work can be reopened".into(),
            ));
        }
        if item.parent_id.is_some() {
            refuse_completed_ancestor(&transaction, &item)?;
        } else {
            let open_descendants = transaction.query_row(
                "WITH RECURSIVE descendants(work_id) AS (
                     SELECT work_id FROM work_items WHERE parent_id = ?1
                     UNION
                     SELECT child.work_id FROM work_items child
                     JOIN descendants parent ON child.parent_id = parent.work_id
                 )
                 SELECT COUNT(*) FROM descendants
                 JOIN work_items item USING(work_id)
                 WHERE item.lifecycle IN ('proposed', 'open')",
                [item.work_id.0.to_string()],
                |row| row.get::<_, i64>(0),
            )?;
            if open_descendants != 0 {
                return Err(StoreError::InvalidWork(
                    "dispose unfinished descendants before reopening a completed root execution"
                        .into(),
                ));
            }
        }
        let generation = transaction.query_row(
            "SELECT COALESCE(MAX(generation), 0) + 1 FROM work_runs WHERE work_id = ?1",
            [item.work_id.0.to_string()],
            |row| row.get::<_, i64>(0),
        )?;
        let root_execution = if item.work_id == item.root_id {
            let root_generation = transaction.query_row(
                "SELECT COALESCE(MAX(generation), 0) + 1
                 FROM work_root_executions WHERE root_id = ?1",
                [item.root_id.0.to_string()],
                |row| row.get::<_, i64>(0),
            )?;
            let execution = RootExecution {
                schema_version: SCHEMA_VERSION,
                root_execution_id: RootExecutionId::new(),
                project_id: item.project_id.clone(),
                root_id: item.root_id,
                generation: root_generation,
                state: RootExecutionState::Active,
                revision: 1,
                run_ids: Vec::new(),
                required_child_seals: Vec::new(),
                required_child_waivers: Vec::new(),
                expected_contributors: Vec::new(),
                contributions: Vec::new(),
                waivers: Vec::new(),
                created_at: request.reopened_at,
                updated_at: request.reopened_at,
            };
            transaction.execute(
                "INSERT INTO work_root_executions (
                     root_execution_id, project_id, root_id, generation, state,
                     revision, created_at_ms, updated_at_ms, execution_json
                 ) VALUES (?1, ?2, ?3, ?4, 'active', 1, ?5, ?6, ?7)",
                params![
                    execution.root_execution_id.0.to_string(),
                    execution.project_id.0,
                    execution.root_id.0.to_string(),
                    execution.generation,
                    execution.created_at.timestamp_millis(),
                    execution.updated_at.timestamp_millis(),
                    serde_json::to_vec(&execution)?
                ],
            )?;
            execution
        } else {
            let mut execution = active_root_execution(&transaction, item.root_id)?;
            if item.child_requirement == ChildRequirement::Required {
                let old_seal: Option<String> = transaction
                    .query_row(
                        "SELECT seal_hash FROM work_completion_seals WHERE work_id = ?1
                         ORDER BY rowid DESC LIMIT 1",
                        [item.work_id.0.to_string()],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(old_seal) = old_seal {
                    execution
                        .required_child_seals
                        .retain(|hash| hash.as_str() != old_seal);
                    execution.revision += 1;
                    execution.updated_at = request.reopened_at;
                    persist_root_execution(&transaction, &execution)?;
                }
            }
            execution
        };
        let run = WorkRun {
            schema_version: SCHEMA_VERSION,
            run_id: WorkRunId::new(),
            root_execution_id: root_execution.root_execution_id,
            work_id: item.work_id,
            generation,
            executor: None,
            state: WorkRunState::Open,
            revision: 1,
            last_checkpoint: None,
            completion_seal: None,
            created_at: request.reopened_at,
            updated_at: request.reopened_at,
        };
        let mut root_execution = root_execution;
        if !root_execution.run_ids.contains(&run.run_id) {
            root_execution.run_ids.push(run.run_id);
            root_execution.run_ids.sort_by_key(|run_id| run_id.0);
            root_execution.revision += 1;
            root_execution.updated_at = request.reopened_at;
            persist_root_execution(&transaction, &root_execution)?;
        }
        transaction.execute(
            "INSERT INTO work_runs (
                 run_id, root_execution_id, work_id, generation,
                 executor_session_id, state, revision, claim_fence_head,
                 last_checkpoint_hash, completion_seal_hash,
                 created_at_ms, updated_at_ms, run_json
             ) VALUES (?1, ?2, ?3, ?4, NULL, 'open', 1, 0, NULL, NULL, ?5, ?6, ?7)",
            params![
                run.run_id.0.to_string(),
                run.root_execution_id.0.to_string(),
                run.work_id.0.to_string(),
                run.generation,
                run.created_at.timestamp_millis(),
                run.updated_at.timestamp_millis(),
                serde_json::to_vec(&run)?
            ],
        )?;
        item.lifecycle = WorkLifecycle::Open;
        item.active_run_id = Some(run.run_id);
        item.revision += 1;
        item.updated_at = request.reopened_at;
        persist_work_item(&transaction, &item)?;
        let event = WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: Some(run.run_id),
            revision: item.revision,
            work: item.clone(),
            run: Some(run.clone()),
            root_execution: Some(root_execution.clone()),
            claim: None,
            handoff_offer: None,
            blocker: None,
            transition: WorkTransition::Reopened {
                run_id: run.run_id,
                generation: run.generation,
                authority: request.authority.clone(),
                reason,
            },
            actor: request.actor.clone(),
            created_at: request.reopened_at,
        };
        append_work_event(&transaction, &event)?;
        persist_operation_result(
            &transaction,
            "reopen_work",
            &request.idempotency_key,
            request_object.hash(),
            &run,
        )?;
        transaction.commit()?;
        Ok(run)
    }

    /// Cancels or supersedes open work without recording false completion.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when authority, revision, claim ownership,
    /// replacement linkage, or descendant-drain invariants are not satisfied.
    pub fn dispose_work<R: Redactor>(
        &mut self,
        request: &DisposeWorkRequest,
        redactor: &R,
    ) -> Result<WorkItem, StoreError> {
        inspect_work_request(redactor, request)?;
        let reason = normalize_text(&request.reason, "work disposal reason")?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(item) = replay_operation::<WorkItem>(
            &transaction,
            "dispose_work",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(item);
        }
        let mut item = load_work_item(&transaction, request.work_id)?;
        assert_revision(&item, request.expected_work_revision)?;
        if item.lifecycle != WorkLifecycle::Open {
            return Err(StoreError::WorkNotOpen(item.work_id));
        }
        resolve_work_authority(
            &transaction,
            &request.authority,
            &request.actor,
            WorkAuthorityOperation::Dispose,
            AuthorityTarget {
                project_id: &item.project_id,
                policy_ref: &item.authority_policy_ref,
                work_id: Some(item.work_id),
                root_id: Some(item.root_id),
                run_id: item.active_run_id,
            },
            request.disposed_at,
        )?;
        let open_descendants = transaction.query_row(
            "WITH RECURSIVE descendants(work_id) AS (
                 SELECT work_id FROM work_items WHERE parent_id = ?1
                 UNION
                 SELECT child.work_id FROM work_items child
                 JOIN descendants parent ON child.parent_id = parent.work_id
             )
             SELECT COUNT(*) FROM descendants
             JOIN work_items item USING(work_id)
             WHERE item.lifecycle IN ('proposed', 'open')",
            [item.work_id.0.to_string()],
            |row| row.get::<_, i64>(0),
        )?;
        if open_descendants != 0 {
            return Err(StoreError::InvalidWork(
                "dispose open descendants before disposing their parent".into(),
            ));
        }
        let replacement = match (request.disposition, request.replacement_id) {
            (WorkDisposition::Cancelled, None) => None,
            (WorkDisposition::Cancelled, Some(_)) => {
                return Err(StoreError::InvalidWork(
                    "cancelled work must not name a replacement".into(),
                ));
            }
            (WorkDisposition::Superseded, None) => {
                return Err(StoreError::InvalidWork(
                    "superseded work requires a replacement".into(),
                ));
            }
            (WorkDisposition::Superseded, Some(replacement_id)) => {
                if replacement_id == item.work_id {
                    return Err(StoreError::InvalidWork(
                        "work cannot supersede itself".into(),
                    ));
                }
                let replacement = load_work_item(&transaction, replacement_id)?;
                if replacement.project_id != item.project_id
                    || matches!(
                        replacement.lifecycle,
                        WorkLifecycle::Cancelled | WorkLifecycle::Superseded
                    )
                {
                    return Err(StoreError::InvalidWork(
                        "replacement must be live or completed work in the same project".into(),
                    ));
                }
                if !combined_graph_is_acyclic_with_dependency(
                    &transaction,
                    &item.project_id.0,
                    Some((item.work_id, replacement.work_id)),
                )? {
                    return Err(StoreError::WorkDependencyCycle);
                }
                Some(replacement)
            }
        };
        let mut run = item
            .active_run_id
            .map(|run_id| load_work_run(&transaction, run_id))
            .transpose()?;
        let mut claim = if let Some(run) = run.as_ref() {
            expire_handoff_offers(
                &transaction,
                run.run_id,
                request.disposed_at,
                &request.actor,
            )?;
            load_work_claim_optional(&transaction, run.run_id)?
        } else {
            None
        };
        let unaccounted_holder = claim
            .as_ref()
            .filter(|claim| claim.state == WorkClaimState::Active)
            .map(|claim| claim.holder.clone());
        if let Some(current) = claim.as_ref()
            && current.state == WorkClaimState::Active
            && current.expires_at > request.disposed_at
        {
            assert_actor_session(&request.actor, &current.holder)?;
        }
        let claim_fence = if let Some(current) = claim.as_mut() {
            if current.state == WorkClaimState::Active {
                current.state = WorkClaimState::Released;
                current.revision += 1;
                current.fence += 1;
                current.expires_at = request.disposed_at;
                persist_claim(&transaction, current)?;
            }
            current.fence
        } else if let Some(run) = run.as_ref() {
            transaction.query_row(
                "SELECT claim_fence_head FROM work_runs WHERE run_id = ?1",
                [run.run_id.0.to_string()],
                |row| row.get::<_, i64>(0),
            )?
        } else {
            0
        };
        if let Some(current_run) = run.as_mut() {
            current_run.executor = None;
            current_run.state = WorkRunState::Cancelled;
            current_run.revision += 1;
            current_run.updated_at = request.disposed_at;
            persist_work_run(&transaction, current_run, claim_fence)?;
        }
        item.lifecycle = match request.disposition {
            WorkDisposition::Cancelled => WorkLifecycle::Cancelled,
            WorkDisposition::Superseded => WorkLifecycle::Superseded,
        };
        item.superseded_by = replacement.as_ref().map(|work| work.work_id);
        item.active_run_id = None;
        item.revision += 1;
        item.updated_at = request.disposed_at;
        persist_work_item(&transaction, &item)?;
        let mut root_execution = if let Some(current_run) = run.as_ref() {
            load_root_execution(&transaction, current_run.root_execution_id)?
        } else {
            active_root_execution(&transaction, item.root_id)?
        };
        let mut root_changed = false;
        if item.work_id != item.root_id
            && let Some(holder) = unaccounted_holder
            && !root_execution
                .contributions
                .iter()
                .any(|contribution| contribution.participant == holder)
            && !root_execution
                .waivers
                .iter()
                .any(|waiver| waiver.participant == holder)
        {
            let grant = resolve_work_authority(
                &transaction,
                &request.authority,
                &request.actor,
                WorkAuthorityOperation::CompletionWaiver,
                AuthorityTarget {
                    project_id: &item.project_id,
                    policy_ref: &item.authority_policy_ref,
                    work_id: Some(item.work_id),
                    root_id: Some(item.root_id),
                    run_id: run.as_ref().map(|run| run.run_id),
                },
                request.disposed_at,
            )?;
            root_changed |= waive_root_contributor(
                &mut root_execution,
                &holder,
                &request.authority,
                &grant,
                &reason,
            );
        }
        if item.work_id == item.root_id {
            root_execution.state = RootExecutionState::Cancelled;
            root_changed = true;
        }
        if root_changed {
            root_execution.revision += 1;
            root_execution.updated_at = request.disposed_at;
            persist_root_execution(&transaction, &root_execution)?;
        }
        let event = WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: run.as_ref().map(|run| run.run_id),
            revision: item.revision,
            work: item.clone(),
            run,
            root_execution: Some(root_execution),
            claim,
            handoff_offer: None,
            blocker: None,
            transition: WorkTransition::Disposed {
                lifecycle: item.lifecycle,
                replacement_id: item.superseded_by,
                reason,
                authority_grant: request.authority.grant.clone(),
            },
            actor: request.actor.clone(),
            created_at: request.disposed_at,
        };
        append_work_event(&transaction, &event)?;
        persist_operation_result(
            &transaction,
            "dispose_work",
            &request.idempotency_key,
            request_object.hash(),
            &item,
        )?;
        transaction.commit()?;
        Ok(item)
    }

    /// Accounts for one deliberately cancelled or superseded required child
    /// under explicit completion-waiver authority.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the parent revision changed, the child is
    /// not a directly required disposed child, authority is absent, or the
    /// waiver conflicts with an earlier request.
    pub fn waive_required_child<R: Redactor>(
        &mut self,
        request: &WaiveRequiredChildRequest,
        redactor: &R,
    ) -> Result<RequiredChildWaiver, StoreError> {
        inspect_work_request(redactor, request)?;
        let reason = normalize_text(&request.reason, "required-child waiver reason")?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(waiver) = replay_operation::<RequiredChildWaiver>(
            &transaction,
            "waive_required_child",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(waiver);
        }
        let parent = load_work_item(&transaction, request.parent_id)?;
        assert_revision(&parent, request.expected_parent_revision)?;
        if parent.lifecycle != WorkLifecycle::Open {
            return Err(StoreError::WorkNotOpen(parent.work_id));
        }
        let child = load_work_item(&transaction, request.child_id)?;
        if child.parent_id != Some(parent.work_id)
            || child.child_requirement != ChildRequirement::Required
            || !matches!(
                child.lifecycle,
                WorkLifecycle::Cancelled | WorkLifecycle::Superseded
            )
        {
            return Err(StoreError::InvalidWork(
                "completion waiver requires a directly required cancelled or superseded child"
                    .into(),
            ));
        }
        let grant = resolve_work_authority(
            &transaction,
            &request.authority,
            &request.actor,
            WorkAuthorityOperation::CompletionWaiver,
            AuthorityTarget {
                project_id: &child.project_id,
                policy_ref: &child.authority_policy_ref,
                work_id: Some(child.work_id),
                root_id: Some(child.root_id),
                run_id: child.active_run_id,
            },
            request.waived_at,
        )?;
        let mut root_execution = active_root_execution(&transaction, parent.root_id)?;
        if root_execution
            .required_child_waivers
            .iter()
            .any(|waiver| waiver.work_id == child.work_id)
        {
            return Err(StoreError::InvalidWork(
                "required child already has a completion waiver in this root execution".into(),
            ));
        }
        let waiver = RequiredChildWaiver {
            work_id: child.work_id,
            work_revision: child.revision,
            authority_grant: request.authority.grant.clone(),
            waived_by: grant.issued_by.actor_id,
            reason: reason.clone(),
        };
        root_execution.required_child_waivers.push(waiver.clone());
        root_execution
            .required_child_waivers
            .sort_by(|left, right| left.work_id.0.as_bytes().cmp(right.work_id.0.as_bytes()));
        root_execution.revision += 1;
        root_execution.updated_at = request.waived_at;
        persist_root_execution(&transaction, &root_execution)?;
        let parent_run = parent
            .active_run_id
            .map(|run_id| load_work_run(&transaction, run_id))
            .transpose()?;
        let event = WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: parent.project_id.clone(),
            root_id: parent.root_id,
            work_id: parent.work_id,
            run_id: parent.active_run_id,
            revision: parent.revision,
            work: parent,
            run: parent_run,
            root_execution: Some(root_execution),
            claim: None,
            handoff_offer: None,
            blocker: None,
            transition: WorkTransition::RequiredChildWaived {
                child_id: child.work_id,
                child_revision: child.revision,
                reason,
                authority_grant: request.authority.grant.clone(),
            },
            actor: request.actor.clone(),
            created_at: request.waived_at,
        };
        append_work_event(&transaction, &event)?;
        persist_operation_result(
            &transaction,
            "waive_required_child",
            &request.idempotency_key,
            request_object.hash(),
            &waiver,
        )?;
        transaction.commit()?;
        Ok(waiver)
    }

    /// Resolves one exact open obligation through a bound host-control session.
    /// Policy refusals are frozen as typed results under the control-operation
    /// idempotency key; transport, routing, and integrity faults remain errors.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the session capabilities are invalid, the
    /// request key conflicts, or canonical storage cannot be verified.
    #[allow(clippy::too_many_arguments)]
    pub fn waive_bound_work_obligation<R: Redactor>(
        &mut self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        connection_token: &str,
        routing_token: &str,
        obligation_id: WorkObligationId,
        expected_definition: &ObjectHash,
        authority_grant: &ObjectHash,
        waived_by: &str,
        reason: &str,
        actor: &crate::domain::ActorContext,
        idempotency_key: &str,
        waived_at: DateTime<Utc>,
        redactor: &R,
    ) -> Result<WorkObligationWaiverDecision, StoreError> {
        let request = WaiveWorkObligationRequest {
            obligation_id,
            expected_definition: expected_definition.clone(),
            waived_by: waived_by.to_owned(),
            reason: reason.to_owned(),
            authority: LifecycleAuthorityDecision {
                grant: authority_grant.clone(),
            },
            actor: actor.clone(),
            idempotency_key: idempotency_key.to_owned(),
            waived_at,
        };
        inspect_work_request(redactor, &request)?;
        let waived_by = normalize_text(&request.waived_by, "obligation waiver actor")?;
        let reason = normalize_text(&request.reason, "obligation waiver reason")?;
        let transaction = self.begin_work_mutation()?;
        Self::verify_control_connection(&transaction, session_id, connection_token)?;
        let session = Self::load_control_session_on(&transaction, session_id)?
            .ok_or_else(|| StoreError::ControlSessionNotBound(session_id.0.clone()))?;
        Self::verify_control_session(&session, project_id, routing_token)?;
        if actor.session_id.as_ref() != Some(session_id) || actor.actor_id != session.actor.actor_id
        {
            return Err(StoreError::InvalidControlSession(
                "obligation waiver actor is not the server-fixed control session".into(),
            ));
        }
        let intent = CanonicalObject::freeze(&ControlWorkObligationWaiverFingerprint {
            control_schema_version: crate::CONTROL_SCHEMA_VERSION,
            session_id,
            bind_intent_hash: &session.bind_intent_hash,
            obligation_id,
            expected_definition,
            authority_grant,
            waived_by: &request.waived_by,
            reason: &request.reason,
            idempotency_key,
        })?;
        if let Some(replay) = Self::replay_control_operation(
            &transaction,
            session_id,
            "obligation_waive",
            idempotency_key,
            intent.hash(),
        )? {
            transaction.commit()?;
            return Ok(replay);
        }
        let record = load_work_obligation_by_id_on(&transaction, obligation_id)?;
        let binding_admitted = session.work_binding.as_ref().is_some_and(|binding| {
            binding.run_id == record.obligation.run_id
                && binding.work_id == record.obligation.work_id
                && binding.root_execution_id == record.obligation.root_execution_id
        });
        if !binding_admitted {
            let decision = WorkObligationWaiverDecision::Refused {
                code: WorkObligationWaiverRefusalCode::WaiverNotAdmitted,
                obligation_id,
                current_definition: Some(record.definition_hash),
                remedy: "bind the host control session to the live claim for this obligation run"
                    .into(),
            };
            Self::persist_control_operation(
                &transaction,
                session_id,
                "obligation_waive",
                idempotency_key,
                &intent,
                &decision,
                waived_at,
            )?;
            transaction.commit()?;
            return Ok(decision);
        }
        let binding = session.work_binding.as_ref().ok_or_else(|| {
            StoreError::InvalidControlProjection(
                "admitted obligation waiver lost its work binding".into(),
            )
        })?;
        if let Err(error) = validate_control_work_binding_on(
            &transaction,
            project_id,
            session_id,
            binding,
            waived_at,
        ) {
            if matches!(error, StoreError::ControlWorkBindingStale { .. }) {
                let decision = WorkObligationWaiverDecision::Refused {
                    code: WorkObligationWaiverRefusalCode::WaiverNotAdmitted,
                    obligation_id,
                    current_definition: Some(record.definition_hash),
                    remedy: "reread the live claim, then bind the current work generation".into(),
                };
                Self::persist_control_operation(
                    &transaction,
                    session_id,
                    "obligation_waive",
                    idempotency_key,
                    &intent,
                    &decision,
                    waived_at,
                )?;
                transaction.commit()?;
                return Ok(decision);
            }
            return Err(error);
        }
        if record.definition_hash != *expected_definition {
            let decision = WorkObligationWaiverDecision::Refused {
                code: WorkObligationWaiverRefusalCode::DefinitionChanged,
                obligation_id,
                current_definition: Some(record.definition_hash),
                remedy:
                    "reread obligation_page and retry only after reviewing the current definition"
                        .into(),
            };
            Self::persist_control_operation(
                &transaction,
                session_id,
                "obligation_waive",
                idempotency_key,
                &intent,
                &decision,
                waived_at,
            )?;
            transaction.commit()?;
            return Ok(decision);
        }
        if record.state != WorkObligationState::Open {
            let decision = WorkObligationWaiverDecision::Refused {
                code: WorkObligationWaiverRefusalCode::ObligationNotOpen,
                obligation_id,
                current_definition: Some(record.definition_hash),
                remedy: "reread obligation_page; this obligation already has a terminal resolution"
                    .into(),
            };
            Self::persist_control_operation(
                &transaction,
                session_id,
                "obligation_waive",
                idempotency_key,
                &intent,
                &decision,
                waived_at,
            )?;
            transaction.commit()?;
            return Ok(decision);
        }
        let item = load_work_item(&transaction, record.obligation.work_id)?;
        let mut authority_actor = request.actor.clone();
        authority_actor.actor_id.clone_from(&waived_by);
        let authority = resolve_work_authority(
            &transaction,
            &request.authority,
            &authority_actor,
            WorkAuthorityOperation::ObligationWaiver,
            AuthorityTarget {
                project_id: &record.obligation.project_id,
                policy_ref: &item.authority_policy_ref,
                work_id: Some(record.obligation.work_id),
                root_id: Some(record.obligation.root_id),
                run_id: Some(record.obligation.run_id),
            },
            waived_at,
        );
        if let Err(StoreError::InvalidWork(_)) = authority {
            let decision = WorkObligationWaiverDecision::Refused {
                code: WorkObligationWaiverRefusalCode::WaiverNotAdmitted,
                obligation_id,
                current_definition: Some(record.definition_hash),
                remedy:
                    "obtain a live obligation_waiver grant for this operator and exact work scope"
                        .into(),
            };
            Self::persist_control_operation(
                &transaction,
                session_id,
                "obligation_waive",
                idempotency_key,
                &intent,
                &decision,
                waived_at,
            )?;
            transaction.commit()?;
            return Ok(decision);
        }
        authority?;
        let event = WorkObligationResolutionEvent {
            schema_version: SCHEMA_VERSION,
            project_id: record.obligation.project_id.clone(),
            obligation_id,
            definition: record.definition_hash.clone(),
            run_id: record.obligation.run_id,
            resolution: WorkObligationResolution::Waived {
                authority_grant: authority_grant.clone(),
                waived_by: waived_by.clone(),
                reason,
            },
            actor: request.actor,
            created_at: waived_at,
        };
        let resolution = append_obligation_resolution_on(&transaction, &record, &event)?;
        let decision = WorkObligationWaiverDecision::Waived {
            receipt: WorkObligationWaiverReceipt {
                obligation_id,
                definition: record.definition_hash,
                resolution,
                state: WorkObligationState::Waived,
                waived_by,
                waived_at,
            },
        };
        Self::persist_control_operation(
            &transaction,
            session_id,
            "obligation_waive",
            idempotency_key,
            &intent,
            &decision,
            waived_at,
        )?;
        transaction.commit()?;
        Ok(decision)
    }

    /// Resolves one exact open obligation through dedicated host/operator
    /// authority. This operation is intentionally absent from the ambient
    /// agent work protocol.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the definition changed, the obligation is
    /// already terminal, the dedicated grant is invalid, or the request
    /// conflicts with an idempotent replay.
    pub fn waive_work_obligation<R: Redactor>(
        &mut self,
        request: &WaiveWorkObligationRequest,
        redactor: &R,
    ) -> Result<WorkObligationResolutionEvent, StoreError> {
        inspect_work_request(redactor, request)?;
        let waived_by = normalize_text(&request.waived_by, "obligation waiver actor")?;
        let reason = normalize_text(&request.reason, "obligation waiver reason")?;
        let request_object = request_object(&WorkObligationWaiverFingerprint {
            obligation_id: request.obligation_id,
            expected_definition: &request.expected_definition,
            waived_by: &request.waived_by,
            reason: &request.reason,
            authority: &request.authority,
            actor: &request.actor,
            idempotency_key: &request.idempotency_key,
        })?;
        let transaction = self.begin_work_mutation()?;
        if let Some(event) = replay_operation::<WorkObligationResolutionEvent>(
            &transaction,
            "waive_work_obligation",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(event);
        }
        let record = load_work_obligation_by_id_on(&transaction, request.obligation_id)?;
        if record.definition_hash != request.expected_definition {
            return Err(StoreError::InvalidWork(format!(
                "obligation {} definition changed: expected {}, current {}",
                request.obligation_id.0, request.expected_definition, record.definition_hash
            )));
        }
        if record.state != WorkObligationState::Open {
            return Err(StoreError::InvalidWork(format!(
                "obligation {} is already terminal",
                request.obligation_id.0
            )));
        }
        let item = load_work_item(&transaction, record.obligation.work_id)?;
        let mut authority_actor = request.actor.clone();
        authority_actor.actor_id.clone_from(&waived_by);
        resolve_work_authority(
            &transaction,
            &request.authority,
            &authority_actor,
            WorkAuthorityOperation::ObligationWaiver,
            AuthorityTarget {
                project_id: &record.obligation.project_id,
                policy_ref: &item.authority_policy_ref,
                work_id: Some(record.obligation.work_id),
                root_id: Some(record.obligation.root_id),
                run_id: Some(record.obligation.run_id),
            },
            request.waived_at,
        )?;
        let event = WorkObligationResolutionEvent {
            schema_version: SCHEMA_VERSION,
            project_id: record.obligation.project_id.clone(),
            obligation_id: record.obligation.obligation_id,
            definition: record.definition_hash.clone(),
            run_id: record.obligation.run_id,
            resolution: WorkObligationResolution::Waived {
                authority_grant: request.authority.grant.clone(),
                waived_by,
                reason,
            },
            actor: request.actor.clone(),
            created_at: request.waived_at,
        };
        append_obligation_resolution_on(&transaction, &record, &event)?;
        persist_operation_result(
            &transaction,
            "waive_work_obligation",
            &request.idempotency_key,
            request_object.hash(),
            &event,
        )?;
        transaction.commit()?;
        Ok(event)
    }
}

impl SqliteStore {
    /// Atomically claims ready work or recovers an expired/released claim.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when work is not ready, another live claim wins,
    /// the request conflicts with an idempotent retry, or persistence fails.
    pub fn claim_work<R: Redactor>(
        &mut self,
        request: &ClaimWorkRequest,
        redactor: &R,
    ) -> Result<WorkClaim, StoreError> {
        inspect_work_request(redactor, request)?;
        assert_actor_session(&request.actor, &request.holder)?;
        let expires_at = claim_expiry(request.claimed_at, request.ttl_seconds)?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(claim) = replay_operation::<WorkClaim>(
            &transaction,
            "claim_work",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(claim);
        }
        let item = load_work_item(&transaction, request.work_id)?;
        assert_revision(&item, request.expected_work_revision)?;
        if item.active_run_id != Some(request.expected_run_id) {
            return Err(StoreError::InvalidWorkProjection(
                "claim request does not match the current active run".into(),
            ));
        }
        expire_handoff_offers(
            &transaction,
            request.expected_run_id,
            request.claimed_at,
            &request.actor,
        )?;
        if item.lifecycle != WorkLifecycle::Open {
            return Err(StoreError::WorkNotOpen(item.work_id));
        }
        resolve_work_authority(
            &transaction,
            &request.authority,
            &request.actor,
            WorkAuthorityOperation::Claim,
            AuthorityTarget {
                project_id: &item.project_id,
                policy_ref: &item.authority_policy_ref,
                work_id: Some(item.work_id),
                root_id: Some(item.root_id),
                run_id: Some(request.expected_run_id),
            },
            request.claimed_at,
        )?;
        let view = inspect_work_canonical_on(&transaction, item.work_id, request.claimed_at)?;
        if !matches!(view.availability, WorkAvailability::Ready) {
            if matches!(
                view.availability,
                WorkAvailability::Claimed | WorkAvailability::Active
            ) && let Some(run_id) = item.active_run_id
                && let Some(claim) = load_work_claim_optional(&transaction, run_id)?
            {
                // Claiming work this session already holds is a no-op that
                // returns the live claim; only another holder is a conflict.
                if claim.holder == request.holder
                    && claim.state == WorkClaimState::Active
                    && claim.expires_at > request.claimed_at
                {
                    transaction.commit()?;
                    return Ok(claim);
                }
                return Err(StoreError::WorkClaimHeld {
                    work: item.work_id,
                    holder: claim.holder.0,
                    expires_at: claim.expires_at.timestamp_millis(),
                });
            }
            return Err(StoreError::InvalidWork(format!(
                "work is not ready: {:?}",
                view.availability
            )));
        }
        let run_id = item.active_run_id.ok_or_else(|| {
            StoreError::InvalidWorkProjection("work has no active run for this operation".into())
        })?;
        if run_id != request.expected_run_id {
            return Err(StoreError::InvalidWorkProjection(
                "claim request does not match the current active run".into(),
            ));
        }
        let mut run = load_work_run(&transaction, run_id)?;
        let prior = load_work_claim_optional(&transaction, run_id)?;
        if let Some(claim) = prior.as_ref()
            && claim.state == WorkClaimState::Active
            && claim.expires_at > request.claimed_at
        {
            return Err(StoreError::WorkClaimHeld {
                work: item.work_id,
                holder: claim.holder.0.clone(),
                expires_at: claim.expires_at.timestamp_millis(),
            });
        }
        let fence_head = transaction.query_row(
            "SELECT claim_fence_head FROM work_runs WHERE run_id = ?1",
            [run_id.0.to_string()],
            |row| row.get::<_, i64>(0),
        )?;
        let recovered = prior.is_some();
        let claim = WorkClaim {
            claim_id: prior
                .as_ref()
                .map_or_else(WorkClaimId::new, |claim| claim.claim_id),
            work_id: item.work_id,
            run_id,
            accepted_work_revision: item.revision,
            holder: request.holder.clone(),
            expires_at,
            revision: prior.as_ref().map_or(1, |claim| claim.revision + 1),
            fence: fence_head + 1,
            state: WorkClaimState::Active,
        };
        let mut root_execution = load_root_execution(&transaction, run.root_execution_id)?;
        let mut root_changed = expect_root_contributor(&mut root_execution, &request.holder);
        if let Some(prior_claim) = prior.as_ref()
            && prior_claim.holder != request.holder
            && !root_participant_is_accounted(&root_execution, &prior_claim.holder)
        {
            let decision = request.recovery_authority.as_ref().ok_or_else(|| {
                StoreError::InvalidWork(
                    "claim recovery requires explicit authority to waive an unaccounted prior holder"
                        .into(),
                )
            })?;
            let grant = resolve_work_authority(
                &transaction,
                decision,
                &request.actor,
                WorkAuthorityOperation::ClaimRecovery,
                AuthorityTarget {
                    project_id: &item.project_id,
                    policy_ref: &item.authority_policy_ref,
                    work_id: Some(item.work_id),
                    root_id: Some(item.root_id),
                    run_id: Some(run_id),
                },
                request.claimed_at,
            )?;
            let reason = request.recovery_reason.as_deref().ok_or_else(|| {
                StoreError::InvalidWork(
                    "claim recovery requires an explicit attributed reason".into(),
                )
            })?;
            let reason = normalize_text(reason, "claim recovery reason")?;
            root_changed |= waive_root_contributor(
                &mut root_execution,
                &prior_claim.holder,
                decision,
                &grant,
                &reason,
            );
        }
        if root_changed {
            root_execution.revision += 1;
            root_execution.updated_at = request.claimed_at;
            persist_root_execution(&transaction, &root_execution)?;
        }
        let preserve_active_run = run.state == WorkRunState::Active
            && prior
                .as_ref()
                .is_some_and(|claim| claim.holder == request.holder);
        run.executor = Some(request.holder.clone());
        if !preserve_active_run {
            run.state = WorkRunState::Claimed;
        }
        run.revision += 1;
        run.updated_at = request.claimed_at;
        persist_claim(&transaction, &claim)?;
        persist_work_run(&transaction, &run, claim.fence)?;
        let event = WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: Some(run_id),
            revision: item.revision,
            work: item.clone(),
            run: Some(run.clone()),
            root_execution: Some(root_execution),
            claim: Some(claim.clone()),
            handoff_offer: None,
            blocker: None,
            transition: WorkTransition::Claimed {
                claim: claim.clone(),
                recovered,
                authority_grant: request.authority.grant.clone(),
            },
            actor: request.actor.clone(),
            created_at: request.claimed_at,
        };
        append_work_event(&transaction, &event)?;
        persist_operation_result(
            &transaction,
            "claim_work",
            &request.idempotency_key,
            request_object.hash(),
            &claim,
        )?;
        transaction.commit()?;
        Ok(claim)
    }

    /// Releases a live claim and advances its fence without reviving old authority.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the exact work, run, claim, revision, holder,
    /// or fence basis is stale, or persistence fails.
    pub fn release_work<R: Redactor>(
        &mut self,
        request: &ReleaseWorkRequest,
        redactor: &R,
    ) -> Result<WorkClaim, StoreError> {
        inspect_work_request(redactor, request)?;
        assert_actor_session(&request.actor, &request.holder)?;
        let reason = normalize_text(&request.reason, "release reason")?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(claim) = replay_operation::<WorkClaim>(
            &transaction,
            "release_work",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(claim);
        }
        expire_handoff_offers(
            &transaction,
            request.run_id,
            request.released_at,
            &request.actor,
        )?;
        let (item, mut run, mut claim) = validate_live_claim_on(
            &transaction,
            request.work_id,
            request.run_id,
            request.expected_work_revision,
            &request.holder,
            request.claim_id,
            request.claim_fence,
            request.released_at,
            false,
        )?;
        let mut root_execution = load_root_execution(&transaction, run.root_execution_id)?;
        if !root_participant_is_accounted(&root_execution, &claim.holder) {
            let decision = request.waiver_authority.as_ref().ok_or_else(|| {
                StoreError::InvalidWork(
                    "release requires a contribution or an explicit completion waiver".into(),
                )
            })?;
            let grant = resolve_work_authority(
                &transaction,
                decision,
                &request.actor,
                WorkAuthorityOperation::CompletionWaiver,
                AuthorityTarget {
                    project_id: &item.project_id,
                    policy_ref: &item.authority_policy_ref,
                    work_id: Some(item.work_id),
                    root_id: Some(item.root_id),
                    run_id: Some(run.run_id),
                },
                request.released_at,
            )?;
            let reason = request.waiver_reason.as_deref().ok_or_else(|| {
                StoreError::InvalidWork(
                    "completion waiver requires an explicit attributed reason".into(),
                )
            })?;
            let reason = normalize_text(reason, "completion waiver reason")?;
            if waive_root_contributor(
                &mut root_execution,
                &claim.holder,
                decision,
                &grant,
                &reason,
            ) {
                root_execution.revision += 1;
                root_execution.updated_at = request.released_at;
                persist_root_execution(&transaction, &root_execution)?;
            }
        }
        claim.state = WorkClaimState::Released;
        claim.revision += 1;
        claim.fence += 1;
        claim.expires_at = request.released_at;
        run.executor = None;
        run.state = WorkRunState::Open;
        run.revision += 1;
        run.updated_at = request.released_at;
        persist_claim(&transaction, &claim)?;
        persist_work_run(&transaction, &run, claim.fence)?;
        let event = WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: Some(run.run_id),
            revision: item.revision,
            work: item.clone(),
            run: Some(run.clone()),
            root_execution: Some(root_execution),
            claim: Some(claim.clone()),
            handoff_offer: None,
            blocker: None,
            transition: WorkTransition::Released {
                claim_id: claim.claim_id,
                fence: claim.fence,
                reason,
            },
            actor: request.actor.clone(),
            created_at: request.released_at,
        };
        append_work_event(&transaction, &event)?;
        persist_operation_result(
            &transaction,
            "release_work",
            &request.idempotency_key,
            request_object.hash(),
            &claim,
        )?;
        transaction.commit()?;
        Ok(claim)
    }

    /// Captures a checkpoint under the exact work, run, claim, and fence basis.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when authority is stale, cited evidence is invalid,
    /// a handoff is pending, or persistence fails.
    pub fn checkpoint_work<R: Redactor>(
        &mut self,
        request: &crate::domain::CheckpointWorkRequest,
        redactor: &R,
    ) -> Result<ObjectHash, StoreError> {
        inspect_work_request(redactor, request)?;
        assert_actor_session(&request.actor, &request.holder)?;
        let summary = normalize_text(&request.summary, "checkpoint summary")?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(hash) = replay_operation::<ObjectHash>(
            &transaction,
            "checkpoint_work",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(hash);
        }
        expire_handoff_offers(
            &transaction,
            request.run_id,
            request.checkpointed_at,
            &request.actor,
        )?;
        let (item, run, mut claim) = validate_live_claim_on(
            &transaction,
            request.work_id,
            request.run_id,
            request.expected_work_revision,
            &request.holder,
            request.claim_id,
            request.claim_fence,
            request.checkpointed_at,
            false,
        )?;
        let evidence = match request.evidence.as_ref() {
            Some(evidence) => unique_hashes(evidence),
            None => work_run_evidence_on(&transaction, run.run_id)?,
        };
        ensure_run_evidence(&transaction, run.run_id, &evidence)?;
        renew_holder_claim(&transaction, &mut claim, request.checkpointed_at)?;
        let checkpoint = persist_work_checkpoint_on(
            &transaction,
            &item,
            run,
            claim,
            summary,
            evidence,
            &request.actor,
            request.checkpointed_at,
        )?;
        persist_operation_result(
            &transaction,
            "checkpoint_work",
            &request.idempotency_key,
            request_object.hash(),
            &checkpoint,
        )?;
        transaction.commit()?;
        Ok(checkpoint)
    }

    /// Adds immutable evidence under the exact live claim basis.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when authority is stale, a handoff is pending,
    /// content is invalid, or persistence fails.
    pub fn record_work_evidence<R: Redactor>(
        &mut self,
        request: &RecordWorkEvidenceRequest,
        redactor: &R,
    ) -> Result<ObjectHash, StoreError> {
        inspect_work_request(redactor, request)?;
        assert_actor_session(&request.actor, &request.holder)?;
        let summary = normalize_text(&request.summary, "evidence summary")?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(hash) = replay_operation::<ObjectHash>(
            &transaction,
            "record_work_evidence",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(hash);
        }
        expire_handoff_offers(
            &transaction,
            request.run_id,
            request.recorded_at,
            &request.actor,
        )?;
        let (item, run, mut claim) = validate_live_claim_on(
            &transaction,
            request.work_id,
            request.run_id,
            request.expected_work_revision,
            &request.holder,
            request.claim_id,
            request.claim_fence,
            request.recorded_at,
            false,
        )?;
        renew_holder_claim(&transaction, &mut claim, request.recorded_at)?;
        let evidence = WorkEvidence {
            schema_version: SCHEMA_VERSION,
            work_id: item.work_id,
            run_id: run.run_id,
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
            summary,
            refs: normalize_strings(&request.refs),
            gate: None,
            actor: request.actor.clone(),
            created_at: request.recorded_at,
        };
        let evidence_hash = persist_work_evidence_on(&transaction, &item, &run, claim, &evidence)?;
        persist_operation_result(
            &transaction,
            "record_work_evidence",
            &request.idempotency_key,
            request_object.hash(),
            &evidence_hash,
        )?;
        transaction.commit()?;
        Ok(evidence_hash)
    }

    /// Captures one note as evidence and the checkpoint that acknowledges the
    /// resulting run-evidence cut in one exact-claim transaction.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when authority is stale, content is invalid, a
    /// handoff is pending, or either immutable object cannot be persisted.
    pub(crate) fn record_work_note<R: Redactor>(
        &mut self,
        request: &RecordWorkNoteRequest,
        redactor: &R,
    ) -> Result<WorkNoteCapture, StoreError> {
        inspect_work_request(redactor, request)?;
        assert_actor_session(&request.actor, &request.holder)?;
        let summary = normalize_text(&request.summary, "note summary")?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(capture) = replay_operation::<WorkNoteCapture>(
            &transaction,
            "record_work_note",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(capture);
        }
        expire_handoff_offers(
            &transaction,
            request.run_id,
            request.recorded_at,
            &request.actor,
        )?;
        let (item, run, mut claim) = validate_live_claim_on(
            &transaction,
            request.work_id,
            request.run_id,
            request.expected_work_revision,
            &request.holder,
            request.claim_id,
            request.claim_fence,
            request.recorded_at,
            false,
        )?;
        renew_holder_claim(&transaction, &mut claim, request.recorded_at)?;
        let evidence = WorkEvidence {
            schema_version: SCHEMA_VERSION,
            work_id: item.work_id,
            run_id: run.run_id,
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
            summary: summary.clone(),
            refs: normalize_strings(&request.refs),
            gate: None,
            actor: request.actor.clone(),
            created_at: request.recorded_at,
        };
        let evidence_hash =
            persist_work_evidence_on(&transaction, &item, &run, claim.clone(), &evidence)?;
        let acknowledged_evidence = work_run_evidence_on(&transaction, run.run_id)?;
        ensure_run_evidence(&transaction, run.run_id, &acknowledged_evidence)?;
        let checkpoint_hash = persist_work_checkpoint_on(
            &transaction,
            &item,
            run,
            claim,
            summary,
            acknowledged_evidence,
            &request.actor,
            request.recorded_at,
        )?;
        let capture = WorkNoteCapture {
            evidence: evidence_hash,
            checkpoint: checkpoint_hash,
        };
        persist_operation_result(
            &transaction,
            "record_work_note",
            &request.idempotency_key,
            request_object.hash(),
            &capture,
        )?;
        transaction.commit()?;
        Ok(capture)
    }

    /// Records a quality-gate transition, or replays the latest consecutive
    /// identical observation, under one SQLite write transaction.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when a new observation has a stale live-claim
    /// basis, the typed gate payload is inconsistent, or persistence fails.
    /// An exact consecutive replay is a read of the already-recorded fact and
    /// does not revalidate the historical claim.
    #[cfg(test)]
    pub(crate) fn record_gate_evidence<R: Redactor>(
        &mut self,
        request: &RecordGateEvidenceRequest,
        redactor: &R,
    ) -> Result<ObjectHash, StoreError> {
        inspect_work_request(redactor, request)?;
        assert_actor_session(&request.actor, &request.holder)?;
        let normalized = normalize_gate_evidence_input(
            &request.name,
            &request.failed,
            request.evidence_ref.as_deref(),
        )
        .map_err(StoreError::InvalidWork)?;
        let name = normalized.name;
        let failed = normalized.failed;
        let refs = normalized.evidence_ref.into_iter().collect::<Vec<_>>();
        let transaction = self.begin_work_mutation()?;
        let previous = latest_gate_evidence_on(&transaction, request.run_id, &name)?;
        if let Some((hash, evidence)) = previous.as_ref()
            && gate_observation_matches(evidence, request, &name, &failed, &refs)
        {
            transaction.commit()?;
            return Ok(hash.clone());
        }

        let evidence_hash = append_gate_evidence_on(
            &transaction,
            request,
            name,
            failed,
            refs,
            previous.as_ref().map(|(hash, _)| hash.clone()),
        )?;
        transaction.commit()?;
        Ok(evidence_hash)
    }

    /// Atomically reserves the gate transition's caller-visible protocol
    /// attempt and records (or reuses) its evidence object. The attempt key is
    /// derived from normalized input plus the previous distinct observation,
    /// so pass -> fail -> pass remains three transitions while an exact retry
    /// after a crash resumes the same pending attempt.
    pub(crate) fn record_gate_evidence_protocol<B: Serialize, R: Redactor>(
        &mut self,
        request: &RecordGateEvidenceRequest,
        protocol: &BeginGateWorkProtocolAttempt<'_, B>,
        redactor: &R,
    ) -> Result<GateWorkProtocolAttempt, StoreError> {
        inspect_work_request(redactor, request)?;
        assert_actor_session(&request.actor, &request.holder)?;
        let normalized = normalize_gate_evidence_input(
            &request.name,
            &request.failed,
            request.evidence_ref.as_deref(),
        )
        .map_err(StoreError::InvalidWork)?;
        let name = normalized.name;
        let failed = normalized.failed;
        let refs = normalized.evidence_ref.into_iter().collect::<Vec<_>>();
        let transaction = self.begin_work_mutation()?;
        let latest = latest_gate_evidence_on(&transaction, request.run_id, &name)?;
        let exact = latest.as_ref().is_some_and(|(_, evidence)| {
            gate_observation_matches(evidence, request, &name, &failed, &refs)
        });
        let previous = if exact {
            latest
                .as_ref()
                .and_then(|(_, evidence)| evidence.gate.as_ref())
                .and_then(|gate| gate.previous.as_ref())
        } else {
            latest.as_ref().map(|(hash, _)| hash)
        };
        let intent = GateWorkProtocolIntent {
            schema_version: SCHEMA_VERSION,
            project_id: protocol.project_id,
            session_id: protocol.session_id,
            actor: &request.actor,
            work_id: request.work_id,
            run_id: request.run_id,
            claim_id: request.claim_id,
            claim_fence: request.claim_fence,
            name: &name,
            failed: &failed,
            refs: &refs,
            previous,
        };
        let intent_object = CanonicalObject::freeze(&intent)?;
        let idempotency_key = format!("gate:{}", intent_object.hash().as_str());
        let attempt = begin_work_protocol_attempt_on(
            &transaction,
            &BeginWorkProtocolAttempt {
                project_id: protocol.project_id,
                session_id: protocol.session_id,
                operation: "work_update:gate",
                idempotency_key: &idempotency_key,
                intent: &intent,
                basis: protocol.basis,
                now: protocol.now,
            },
        )?;
        if attempt.result.is_some() && !exact {
            return Err(StoreError::InvalidWorkProjection(
                "completed gate protocol attempt disagrees with the latest same-name evidence"
                    .into(),
            ));
        }
        let evidence = if exact {
            latest
                .as_ref()
                .map(|(hash, _)| hash.clone())
                .ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "an exact gate replay has no latest evidence object".into(),
                    )
                })?
        } else {
            append_gate_evidence_on(
                &transaction,
                request,
                name,
                failed,
                refs,
                latest.as_ref().map(|(hash, _)| hash.clone()),
            )?
        };
        transaction.commit()?;
        Ok(GateWorkProtocolAttempt {
            evidence,
            idempotency_key,
            result: attempt.result,
        })
    }

    /// Offers a checkpoint-coupled handoff without transferring authority yet.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when authority is stale, a prior offer remains,
    /// the destination is invalid, or the atomic write fails.
    pub fn offer_work_handoff<R: Redactor>(
        &mut self,
        request: &OfferWorkHandoffRequest,
        redactor: &R,
    ) -> Result<WorkHandoffOffer, StoreError> {
        inspect_work_request(redactor, request)?;
        assert_actor_session(&request.actor, &request.from)?;
        if request.from == request.to {
            return Err(StoreError::InvalidWork(
                "handoff source and destination must differ".into(),
            ));
        }
        let summary = normalize_text(&request.checkpoint_summary, "checkpoint summary")?;
        let requested_expiry = claim_expiry(request.offered_at, request.ttl_seconds)?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(offer) = replay_operation::<WorkHandoffOffer>(
            &transaction,
            "offer_work_handoff",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(offer);
        }
        expire_handoff_offers(
            &transaction,
            request.run_id,
            request.offered_at,
            &request.actor,
        )?;
        let (item, mut run, mut claim) = validate_live_claim_on(
            &transaction,
            request.work_id,
            request.run_id,
            request.expected_work_revision,
            &request.from,
            request.claim_id,
            request.claim_fence,
            request.offered_at,
            false,
        )?;
        renew_holder_claim(&transaction, &mut claim, request.offered_at)?;
        let acknowledged_run_position = FeedPosition {
            feed: FeedId::RunExecution(run.run_id),
            position: feed_head(&transaction, &FeedId::RunExecution(run.run_id))?,
        };
        let checkpoint = WorkCheckpoint {
            schema_version: SCHEMA_VERSION,
            work_id: item.work_id,
            run_id: run.run_id,
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
            acknowledged_run_position,
            summary,
            evidence: Vec::new(),
            actor: request.actor.clone(),
            created_at: request.offered_at,
        };
        let checkpoint_object = CanonicalObject::freeze(&checkpoint)?;
        SqliteStore::insert_object(&transaction, "work_checkpoint", &checkpoint_object)?;
        append_to_work_feeds(
            &transaction,
            &item.project_id,
            item.root_id,
            Some(run.run_id),
            None,
            "work_checkpoint",
            &checkpoint_object,
        )?;
        run.last_checkpoint = Some(checkpoint_object.hash().clone());
        run.state = WorkRunState::Active;
        run.revision += 1;
        run.updated_at = request.offered_at;
        persist_work_run(&transaction, &run, claim.fence)?;
        let mut root_execution = load_root_execution(&transaction, run.root_execution_id)?;
        let root_changed = expect_root_contributor(&mut root_execution, &claim.holder)
            | add_root_contribution(&mut root_execution, &claim.holder, checkpoint_object.hash());
        if root_changed {
            root_execution.revision += 1;
            root_execution.updated_at = request.offered_at;
            persist_root_execution(&transaction, &root_execution)?;
        }
        let offer = WorkHandoffOffer {
            offer_id: WorkHandoffOfferId::new(),
            work_id: item.work_id,
            run_id: run.run_id,
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
            work_revision: item.revision,
            from: request.from.clone(),
            to: request.to.clone(),
            checkpoint: checkpoint_object.hash().clone(),
            accepted_ttl_seconds: request.ttl_seconds,
            offered_at: request.offered_at,
            expires_at: claim.expires_at.min(requested_expiry),
            state: WorkHandoffState::Offered,
        };
        let offer_object = CanonicalObject::freeze(&offer)?;
        SqliteStore::insert_object(&transaction, "work_handoff_offer", &offer_object)?;
        transaction.execute(
            "INSERT INTO work_handoff_offers (
                 offer_id, run_id, work_id, state, expires_at_ms,
                 offer_hash, offer_json
             ) VALUES (?1, ?2, ?3, 'offered', ?4, ?5, ?6)",
            params![
                offer.offer_id.0.to_string(),
                offer.run_id.0.to_string(),
                offer.work_id.0.to_string(),
                offer.expires_at.timestamp_millis(),
                offer_object.hash().as_str(),
                serde_json::to_vec(&offer)?
            ],
        )?;
        let event = WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: Some(run.run_id),
            revision: item.revision,
            work: item.clone(),
            run: Some(run.clone()),
            root_execution: Some(root_execution),
            claim: Some(claim.clone()),
            handoff_offer: Some(offer.clone()),
            blocker: None,
            transition: WorkTransition::HandoffOffered {
                offer_id: offer.offer_id,
                to: offer.to.clone(),
                checkpoint: offer.checkpoint.clone(),
                offer: offer_object.hash().clone(),
            },
            actor: request.actor.clone(),
            created_at: request.offered_at,
        };
        append_work_event(&transaction, &event)?;
        persist_operation_result(
            &transaction,
            "offer_work_handoff",
            &request.idempotency_key,
            request_object.hash(),
            &offer,
        )?;
        transaction.commit()?;
        Ok(offer)
    }

    /// Accepts a pending handoff, transfers the claim, and advances its fence.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the offer expired or changed, its authority
    /// basis is stale, the destination differs, or persistence fails.
    pub fn accept_work_handoff<R: Redactor>(
        &mut self,
        request: &AcceptWorkHandoffRequest,
        redactor: &R,
    ) -> Result<WorkClaim, StoreError> {
        inspect_work_request(redactor, request)?;
        assert_actor_session(&request.actor, &request.to)?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(claim) = replay_operation::<WorkClaim>(
            &transaction,
            "accept_work_handoff",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(claim);
        }
        let offer_row: Option<(Option<String>, Vec<u8>)> = transaction
            .query_row(
                "SELECT offer_hash, offer_json FROM work_handoff_offers
                 WHERE offer_id = ?1",
                [request.offer_id.0.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let mut offer = offer_row
            .map(|row| load_handoff_offer_projection(&transaction, row))
            .transpose()?
            .ok_or_else(|| StoreError::InvalidWork("handoff offer is not active".into()))?;
        if offer.state != WorkHandoffState::Offered {
            return Err(StoreError::InvalidWork(
                "handoff offer is not active".into(),
            ));
        }
        if offer.work_id != request.work_id || offer.to != request.to {
            return Err(StoreError::InvalidWork(
                "handoff offer does not match this work or destination".into(),
            ));
        }
        if offer.expires_at <= request.accepted_at {
            expire_handoff_offers(
                &transaction,
                offer.run_id,
                request.accepted_at,
                &request.actor,
            )?;
            transaction.commit()?;
            return Err(StoreError::InvalidWork("handoff offer has expired".into()));
        }
        let (item, mut run, mut claim) = validate_live_claim_on(
            &transaction,
            offer.work_id,
            offer.run_id,
            offer.work_revision,
            &offer.from,
            offer.claim_id,
            offer.claim_fence,
            request.accepted_at,
            true,
        )?;
        resolve_work_authority(
            &transaction,
            &request.authority,
            &request.actor,
            WorkAuthorityOperation::Claim,
            AuthorityTarget {
                project_id: &item.project_id,
                policy_ref: &item.authority_policy_ref,
                work_id: Some(item.work_id),
                root_id: Some(item.root_id),
                run_id: Some(offer.run_id),
            },
            request.accepted_at,
        )?;
        claim.holder = request.to.clone();
        claim.fence += 1;
        claim.revision += 1;
        claim.expires_at = claim_expiry(request.accepted_at, offer.accepted_ttl_seconds)?;
        run.executor = Some(request.to.clone());
        run.state = WorkRunState::Active;
        run.revision += 1;
        run.updated_at = request.accepted_at;
        persist_claim(&transaction, &claim)?;
        persist_work_run(&transaction, &run, claim.fence)?;
        let mut root_execution = load_root_execution(&transaction, run.root_execution_id)?;
        if expect_root_contributor(&mut root_execution, &request.to) {
            root_execution.revision += 1;
            root_execution.updated_at = request.accepted_at;
            persist_root_execution(&transaction, &root_execution)?;
        }
        offer.state = WorkHandoffState::Accepted;
        let accepted_offer_object = CanonicalObject::freeze(&offer)?;
        SqliteStore::insert_object(&transaction, "work_handoff_offer", &accepted_offer_object)?;
        transaction.execute(
            "UPDATE work_handoff_offers
             SET state = 'accepted', offer_hash = ?2, offer_json = ?3
             WHERE offer_id = ?1",
            params![
                offer.offer_id.0.to_string(),
                accepted_offer_object.hash().as_str(),
                serde_json::to_vec(&offer)?
            ],
        )?;
        let event = WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: Some(run.run_id),
            revision: item.revision,
            work: item.clone(),
            run: Some(run.clone()),
            root_execution: Some(root_execution),
            claim: Some(claim.clone()),
            handoff_offer: Some(offer.clone()),
            blocker: None,
            transition: WorkTransition::HandedOff {
                offer_id: offer.offer_id,
                claim_id: claim.claim_id,
                from: offer.from,
                to: claim.holder.clone(),
                fence: claim.fence,
                checkpoint: offer.checkpoint,
                offer: accepted_offer_object.hash().clone(),
                authority_grant: request.authority.grant.clone(),
            },
            actor: request.actor.clone(),
            created_at: request.accepted_at,
        };
        append_work_event(&transaction, &event)?;
        persist_operation_result(
            &transaction,
            "accept_work_handoff",
            &request.idempotency_key,
            request_object.hash(),
            &claim,
        )?;
        transaction.commit()?;
        Ok(claim)
    }

    /// Cancels an unaccepted handoff while retaining the outgoing claim.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the offer, claim, holder, fence, or revision
    /// basis is stale, the offer expired, or persistence fails.
    pub fn cancel_work_handoff<R: Redactor>(
        &mut self,
        request: &CancelWorkHandoffRequest,
        redactor: &R,
    ) -> Result<WorkHandoffOffer, StoreError> {
        inspect_work_request(redactor, request)?;
        assert_actor_session(&request.actor, &request.holder)?;
        let reason = normalize_text(&request.reason, "handoff cancellation reason")?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(offer) = replay_operation::<WorkHandoffOffer>(
            &transaction,
            "cancel_work_handoff",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(offer);
        }
        let offer_row: Option<(Option<String>, Vec<u8>)> = transaction
            .query_row(
                "SELECT offer_hash, offer_json FROM work_handoff_offers
                 WHERE offer_id = ?1",
                [request.offer_id.0.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let mut offer = offer_row
            .map(|row| load_handoff_offer_projection(&transaction, row))
            .transpose()?
            .ok_or_else(|| StoreError::InvalidWork("handoff offer does not exist".into()))?;
        if offer.state != WorkHandoffState::Offered
            || offer.work_id != request.work_id
            || offer.run_id != request.run_id
            || offer.claim_id != request.claim_id
            || offer.claim_fence != request.claim_fence
            || offer.from != request.holder
        {
            return Err(StoreError::InvalidWork(
                "handoff offer does not match the live outgoing authority basis".into(),
            ));
        }
        if offer.expires_at <= request.cancelled_at {
            expire_handoff_offers(
                &transaction,
                offer.run_id,
                request.cancelled_at,
                &request.actor,
            )?;
            transaction.commit()?;
            return Err(StoreError::InvalidWork("handoff offer has expired".into()));
        }
        let (item, run, mut claim) = validate_live_claim_on(
            &transaction,
            request.work_id,
            request.run_id,
            request.expected_work_revision,
            &request.holder,
            request.claim_id,
            request.claim_fence,
            request.cancelled_at,
            true,
        )?;
        renew_holder_claim(&transaction, &mut claim, request.cancelled_at)?;
        offer.state = WorkHandoffState::Cancelled;
        let offer_object = CanonicalObject::freeze(&offer)?;
        SqliteStore::insert_object(&transaction, "work_handoff_offer", &offer_object)?;
        transaction.execute(
            "UPDATE work_handoff_offers
             SET state = 'cancelled', offer_hash = ?2, offer_json = ?3
             WHERE offer_id = ?1 AND state = 'offered'",
            params![
                offer.offer_id.0.to_string(),
                offer_object.hash().as_str(),
                serde_json::to_vec(&offer)?
            ],
        )?;
        let root_execution = load_root_execution(&transaction, run.root_execution_id)?;
        let event = WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: Some(run.run_id),
            revision: item.revision,
            work: item,
            run: Some(run),
            root_execution: Some(root_execution),
            claim: Some(claim),
            handoff_offer: Some(offer.clone()),
            blocker: None,
            transition: WorkTransition::HandoffCancelled {
                offer_id: offer.offer_id,
                offer: offer_object.hash().clone(),
                reason,
            },
            actor: request.actor.clone(),
            created_at: request.cancelled_at,
        };
        append_work_event(&transaction, &event)?;
        persist_operation_result(
            &transaction,
            "cancel_work_handoff",
            &request.idempotency_key,
            request_object.hash(),
            &offer,
        )?;
        transaction.commit()?;
        Ok(offer)
    }
}

impl SqliteStore {
    /// Creates a local root with an initial run and immutable event.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when input, source provenance, idempotency, or
    /// persistence validation fails.
    pub fn create_work<R: Redactor>(
        &mut self,
        request: &CreateWorkRequest,
        redactor: &R,
    ) -> Result<WorkItem, StoreError> {
        inspect_work_request(redactor, request)?;
        if request.parent_id.is_some() {
            return Err(StoreError::InvalidWork(
                "direct child creation is not allowed; use decompose_work with parent revision and planning authority".into(),
            ));
        }
        if !(0..=4).contains(&request.priority) {
            return Err(StoreError::InvalidWork(
                "priority must be an integer from 0 through 4".into(),
            ));
        }
        match (request.origin, request.source_snapshot_id.as_ref()) {
            (WorkOrigin::Local, None) | (WorkOrigin::Imported, Some(_)) => {}
            (WorkOrigin::Local, Some(_)) => {
                return Err(StoreError::InvalidWork(
                    "local work cannot carry an imported source snapshot".into(),
                ));
            }
            (WorkOrigin::Imported, None) => {
                return Err(StoreError::InvalidWork(
                    "imported work requires a source snapshot".into(),
                ));
            }
        }
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(item) = replay_operation::<WorkItem>(
            &transaction,
            "create_work",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(item);
        }
        resolve_work_authority(
            &transaction,
            &request.authority,
            &request.actor,
            WorkAuthorityOperation::RootCreate,
            AuthorityTarget {
                project_id: &request.project_id,
                policy_ref: &request.authority_policy_ref,
                work_id: None,
                root_id: None,
                run_id: None,
            },
            request.created_at,
        )?;
        if let Some(snapshot) = request.source_snapshot_id.as_ref() {
            let source = load_typed_work_object::<WorkSourceSnapshot>(
                &transaction,
                snapshot,
                "work_source_snapshot",
            )
            .map_err(|error| {
                    StoreError::InvalidWork(format!(
                        "import source {snapshot} is not a verified work_source_snapshot object: {error}"
                    ))
                })?;
            validate_work_source_snapshot(&source, request.created_at)?;
        }

        let title = normalize_text(&request.title, "title")?;
        let outcome = normalize_text(&request.outcome, "outcome")?;
        let authority_policy_ref =
            normalize_text(&request.authority_policy_ref, "authority policy reference")?;
        let work_id = WorkId::new();
        let run_id = WorkRunId::new();
        let root_id = work_id;
        let root_execution = RootExecution {
            schema_version: SCHEMA_VERSION,
            root_execution_id: RootExecutionId::new(),
            project_id: request.project_id.clone(),
            root_id,
            generation: 1,
            state: RootExecutionState::Active,
            revision: 1,
            run_ids: vec![run_id],
            required_child_seals: Vec::new(),
            required_child_waivers: Vec::new(),
            expected_contributors: Vec::new(),
            contributions: Vec::new(),
            waivers: Vec::new(),
            created_at: request.created_at,
            updated_at: request.created_at,
        };
        let item = WorkItem {
            schema_version: SCHEMA_VERSION,
            project_id: request.project_id.clone(),
            work_id,
            short_ref: short_ref(work_id),
            root_id,
            parent_id: None,
            child_requirement: request.child_requirement,
            title,
            outcome,
            acceptance: normalize_strings(&request.acceptance),
            kind: request.kind,
            priority: request.priority,
            labels: normalize_strings(&request.labels),
            assigned_to: normalize_optional(request.assigned_to.clone()),
            deferred_until: request.deferred_until,
            origin: request.origin,
            source_snapshot_id: request.source_snapshot_id.clone(),
            authority_policy_ref,
            lifecycle: WorkLifecycle::Open,
            revision: 1,
            active_run_id: Some(run_id),
            superseded_by: None,
            created_by: request.actor.clone(),
            created_at: request.created_at,
            updated_at: request.created_at,
        };
        transaction.execute(
            "INSERT INTO work_items (
                 work_id, project_id, short_ref, root_id, parent_id,
                 child_requirement, lifecycle, priority, assigned_to,
                 deferred_until_ms, revision, active_run_id, source_snapshot_hash,
                 created_at_ms, updated_at_ms, item_json
             ) VALUES (
                 ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
                 ?14, ?15, ?16
             )",
            params![
                item.work_id.0.to_string(),
                item.project_id.0,
                item.short_ref,
                item.root_id.0.to_string(),
                item.parent_id.map(|value| value.0.to_string()),
                encode_state(item.child_requirement)?,
                encode_state(item.lifecycle)?,
                item.priority,
                item.assigned_to,
                item.deferred_until.map(|value| value.timestamp_millis()),
                item.revision,
                run_id.0.to_string(),
                item.source_snapshot_id.as_ref().map(ObjectHash::as_str),
                item.created_at.timestamp_millis(),
                item.updated_at.timestamp_millis(),
                serde_json::to_vec(&item)?
            ],
        )?;
        refresh_work_catalog_projection(&transaction, &item)?;
        transaction.execute(
            "INSERT INTO work_root_executions (
                 root_execution_id, project_id, root_id, generation, state,
                 revision, created_at_ms, updated_at_ms, execution_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                root_execution.root_execution_id.0.to_string(),
                root_execution.project_id.0,
                root_execution.root_id.0.to_string(),
                root_execution.generation,
                encode_state(root_execution.state)?,
                root_execution.revision,
                root_execution.created_at.timestamp_millis(),
                root_execution.updated_at.timestamp_millis(),
                serde_json::to_vec(&root_execution)?
            ],
        )?;
        let run = WorkRun {
            schema_version: SCHEMA_VERSION,
            run_id,
            root_execution_id: root_execution.root_execution_id,
            work_id,
            generation: 1,
            executor: None,
            state: WorkRunState::Open,
            revision: 1,
            last_checkpoint: None,
            completion_seal: None,
            created_at: request.created_at,
            updated_at: request.created_at,
        };
        transaction.execute(
            "INSERT INTO work_runs (
                 run_id, root_execution_id, work_id, generation,
                 executor_session_id, state, revision, claim_fence_head,
                 last_checkpoint_hash, completion_seal_hash,
                 created_at_ms, updated_at_ms, run_json
             ) VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, 0, NULL, NULL, ?7, ?8, ?9)",
            params![
                run.run_id.0.to_string(),
                run.root_execution_id.0.to_string(),
                run.work_id.0.to_string(),
                run.generation,
                encode_state(run.state)?,
                run.revision,
                run.created_at.timestamp_millis(),
                run.updated_at.timestamp_millis(),
                serde_json::to_vec(&run)?
            ],
        )?;
        if !combined_graph_is_acyclic(&transaction, &item.project_id.0)? {
            return Err(StoreError::WorkDependencyCycle);
        }
        let event = WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id,
            run_id: Some(run_id),
            revision: item.revision,
            work: item.clone(),
            run: Some(run.clone()),
            root_execution: Some(root_execution.clone()),
            claim: None,
            handoff_offer: None,
            blocker: None,
            transition: WorkTransition::Created {
                prerequisites: Vec::new(),
                authority_grant: request.authority.grant.clone(),
            },
            actor: request.actor.clone(),
            created_at: request.created_at,
        };
        append_work_event(&transaction, &event)?;
        persist_operation_result(
            &transaction,
            "create_work",
            &request.idempotency_key,
            request_object.hash(),
            &item,
        )?;
        transaction.commit()?;
        Ok(item)
    }

    /// Atomically creates a bounded set of direct children and prerequisites.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the parent revision changed, a child or edge
    /// is invalid, the combined graph cycles, or the transaction cannot commit.
    pub fn decompose_work<R: Redactor>(
        &mut self,
        request: &DecomposeWorkRequest,
        redactor: &R,
    ) -> Result<WorkDecomposition, StoreError> {
        inspect_work_request(redactor, request)?;
        if request.children.is_empty() || request.children.len() > 64 {
            return Err(StoreError::InvalidWork(
                "decomposition must contain from 1 through 64 children".into(),
            ));
        }
        let mut keys = HashSet::new();
        for child in &request.children {
            let key = normalize_text(&child.local_key, "child local key")?;
            if !keys.insert(key) {
                return Err(StoreError::InvalidWork(
                    "child local keys must be unique".into(),
                ));
            }
            if !(0..=4).contains(&child.priority) {
                return Err(StoreError::InvalidWork(
                    "child priority must be an integer from 0 through 4".into(),
                ));
            }
        }
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(decomposition) = replay_operation::<WorkDecomposition>(
            &transaction,
            "decompose_work",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(decomposition);
        }
        let mut parent = load_work_item(&transaction, request.parent_id)?;
        assert_revision(&parent, request.expected_parent_revision)?;
        let planning_grant = validate_planning_authority(
            &transaction,
            &parent,
            &request.authority,
            &request.actor,
            request.created_at,
        )?;
        validate_decomposition_budget(
            &transaction,
            &parent,
            planning_grant.planning_budget.as_ref().ok_or_else(|| {
                StoreError::InvalidWorkProjection(
                    "resolved planning grant has no planning budget".into(),
                )
            })?,
            request.children.len(),
        )?;
        if parent.lifecycle != WorkLifecycle::Open {
            return Err(StoreError::WorkNotOpen(parent.work_id));
        }
        let mut root_execution = active_root_execution(&transaction, parent.root_id)?;
        let mut ids = HashMap::new();
        for child in &request.children {
            ids.insert(child.local_key.trim().to_owned(), WorkId::new());
        }
        let mut prerequisites: HashMap<String, Vec<WorkId>> = HashMap::new();
        for edge in &request.prerequisites {
            let work_key = edge.work_key.trim();
            if !ids.contains_key(work_key) {
                return Err(StoreError::InvalidWork(format!(
                    "prerequisite edge references unknown child {work_key:?}"
                )));
            }
            let prerequisite = match &edge.prerequisite {
                WorkDependencyRef::Existing(work_id) => {
                    let existing = load_work_item(&transaction, *work_id)?;
                    if existing.project_id != parent.project_id {
                        return Err(StoreError::InvalidWork(
                            "prerequisite edges cannot cross projects".into(),
                        ));
                    }
                    if existing.lifecycle == WorkLifecycle::Completed {
                        return Err(StoreError::WorkPrerequisiteAlreadySatisfied(
                            existing.work_id,
                        ));
                    }
                    if existing.lifecycle != WorkLifecycle::Open {
                        return Err(StoreError::WorkNotOpen(existing.work_id));
                    }
                    if existing.work_id == parent.work_id
                        || work_is_ancestor_of(&transaction, existing.work_id, &parent)?
                    {
                        return Err(StoreError::WorkDependencyCycle);
                    }
                    *work_id
                }
                WorkDependencyRef::Proposed(key) => {
                    ids.get(key.trim()).copied().ok_or_else(|| {
                        StoreError::InvalidWork(format!(
                            "prerequisite edge references unknown proposed child {key:?}"
                        ))
                    })?
                }
            };
            if ids[work_key] == prerequisite {
                return Err(StoreError::WorkDependencyCycle);
            }
            prerequisites
                .entry(work_key.to_owned())
                .or_default()
                .push(prerequisite);
        }
        for values in prerequisites.values_mut() {
            values.sort_by_key(|value| value.0);
            values.dedup();
        }

        let mut children = Vec::with_capacity(request.children.len());
        let mut runs = HashMap::new();
        for draft in &request.children {
            let key = draft.local_key.trim();
            let work_id = ids[key];
            let run_id = WorkRunId::new();
            let mut labels = parent.labels.clone();
            labels.extend(draft.labels.clone());
            let item = WorkItem {
                schema_version: SCHEMA_VERSION,
                project_id: parent.project_id.clone(),
                work_id,
                short_ref: short_ref(work_id),
                root_id: parent.root_id,
                parent_id: Some(parent.work_id),
                child_requirement: draft.child_requirement,
                title: normalize_text(&draft.title, "child title")?,
                outcome: normalize_text(&draft.outcome, "child outcome")?,
                acceptance: normalize_strings(&draft.acceptance),
                kind: draft.kind,
                priority: draft.priority,
                labels: normalize_strings(&labels),
                assigned_to: normalize_optional(draft.assigned_to.clone()),
                deferred_until: draft.deferred_until,
                origin: WorkOrigin::Local,
                source_snapshot_id: None,
                authority_policy_ref: parent.authority_policy_ref.clone(),
                lifecycle: WorkLifecycle::Open,
                revision: 1,
                active_run_id: Some(run_id),
                superseded_by: None,
                created_by: request.actor.clone(),
                created_at: request.created_at,
                updated_at: request.created_at,
            };
            transaction.execute(
                "INSERT INTO work_items (
                     work_id, project_id, short_ref, root_id, parent_id,
                     child_requirement, lifecycle, priority, assigned_to,
                     deferred_until_ms, revision, active_run_id, source_snapshot_hash,
                     created_at_ms, updated_at_ms, item_json
                 ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'open', ?7, ?8, ?9, 1, ?10,
                           NULL, ?11, ?12, ?13)",
                params![
                    item.work_id.0.to_string(),
                    item.project_id.0,
                    item.short_ref,
                    item.root_id.0.to_string(),
                    parent.work_id.0.to_string(),
                    encode_state(item.child_requirement)?,
                    item.priority,
                    item.assigned_to,
                    item.deferred_until.map(|value| value.timestamp_millis()),
                    run_id.0.to_string(),
                    item.created_at.timestamp_millis(),
                    item.updated_at.timestamp_millis(),
                    serde_json::to_vec(&item)?
                ],
            )?;
            refresh_work_catalog_projection(&transaction, &item)?;
            let run = WorkRun {
                schema_version: SCHEMA_VERSION,
                run_id,
                root_execution_id: root_execution.root_execution_id,
                work_id,
                generation: 1,
                executor: None,
                state: WorkRunState::Open,
                revision: 1,
                last_checkpoint: None,
                completion_seal: None,
                created_at: request.created_at,
                updated_at: request.created_at,
            };
            transaction.execute(
                "INSERT INTO work_runs (
                     run_id, root_execution_id, work_id, generation,
                     executor_session_id, state, revision, claim_fence_head,
                     last_checkpoint_hash, completion_seal_hash,
                     created_at_ms, updated_at_ms, run_json
                 ) VALUES (?1, ?2, ?3, 1, NULL, 'open', 1, 0, NULL, NULL, ?4, ?5, ?6)",
                params![
                    run.run_id.0.to_string(),
                    run.root_execution_id.0.to_string(),
                    run.work_id.0.to_string(),
                    run.created_at.timestamp_millis(),
                    run.updated_at.timestamp_millis(),
                    serde_json::to_vec(&run)?
                ],
            )?;
            runs.insert(key.to_owned(), run);
            children.push(item);
        }
        root_execution
            .run_ids
            .extend(runs.values().map(|run| run.run_id));
        root_execution.run_ids.sort_by_key(|run_id| run_id.0);
        root_execution.run_ids.dedup();
        root_execution.revision += 1;
        root_execution.updated_at = request.created_at;
        persist_root_execution(&transaction, &root_execution)?;
        for (draft, item) in request.children.iter().zip(&children) {
            let item_prerequisites = prerequisites
                .get(draft.local_key.trim())
                .cloned()
                .unwrap_or_default();
            let run = &runs[draft.local_key.trim()];
            let run_id = run.run_id;
            let event = WorkEventDraft {
                schema_version: SCHEMA_VERSION,
                project_id: item.project_id.clone(),
                root_id: item.root_id,
                work_id: item.work_id,
                run_id: Some(run_id),
                revision: item.revision,
                work: item.clone(),
                run: Some(run.clone()),
                root_execution: Some(root_execution.clone()),
                claim: None,
                handoff_offer: None,
                blocker: None,
                transition: WorkTransition::Created {
                    prerequisites: item_prerequisites.clone(),
                    authority_grant: match &request.authority {
                        WorkPlanningAuthority::Claim { grant, .. }
                        | WorkPlanningAuthority::Delegated { grant } => grant.clone(),
                    },
                },
                actor: request.actor.clone(),
                created_at: request.created_at,
            };
            let (event_hash, _) = append_work_event(&transaction, &event)?;
            for prerequisite in item_prerequisites {
                transaction.execute(
                    "INSERT INTO work_prerequisites (work_id, prerequisite_id, event_hash)
                     VALUES (?1, ?2, ?3)",
                    params![
                        item.work_id.0.to_string(),
                        prerequisite.0.to_string(),
                        event_hash.as_str()
                    ],
                )?;
            }
        }
        if !combined_graph_is_acyclic(&transaction, &parent.project_id.0)? {
            return Err(StoreError::WorkDependencyCycle);
        }
        parent.revision += 1;
        parent.updated_at = request.created_at;
        persist_work_item(&transaction, &parent)?;
        let (claim_snapshot, rebased_run) = rebase_planning_claim(
            &transaction,
            &parent,
            &request.authority,
            request.created_at,
        )?;
        let event = WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: parent.project_id.clone(),
            root_id: parent.root_id,
            work_id: parent.work_id,
            run_id: parent.active_run_id,
            revision: parent.revision,
            work: parent.clone(),
            run: match rebased_run {
                Some(run) => Some(run),
                None => active_run_snapshot(&transaction, &parent)?,
            },
            root_execution: Some(root_execution.clone()),
            claim: claim_snapshot,
            handoff_offer: None,
            blocker: None,
            transition: WorkTransition::Decomposed {
                children: children.iter().map(|child| child.work_id).collect(),
                authority: request.authority.clone(),
            },
            actor: request.actor.clone(),
            created_at: request.created_at,
        };
        append_work_event(&transaction, &event)?;
        let decomposition = WorkDecomposition { parent, children };
        persist_operation_result(
            &transaction,
            "decompose_work",
            &request.idempotency_key,
            request_object.hash(),
            &decomposition,
        )?;
        transaction.commit()?;
        Ok(decomposition)
    }

    /// Revises planning fields under optimistic work revision control.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the patch is invalid, the revision changed,
    /// work is not open, idempotency conflicts, or persistence fails.
    pub fn revise_work<R: Redactor>(
        &mut self,
        request: &ReviseWorkRequest,
        redactor: &R,
    ) -> Result<WorkItem, StoreError> {
        inspect_work_request(redactor, request)?;
        if request.patch.clear_assignment && request.patch.assigned_to.is_some() {
            return Err(StoreError::InvalidWork(
                "assignment cannot be set and cleared in one revision".into(),
            ));
        }
        if request.patch.clear_deferral && request.patch.deferred_until.is_some() {
            return Err(StoreError::InvalidWork(
                "deferral cannot be set and cleared in one revision".into(),
            ));
        }
        if request.patch.labels.is_some()
            && (!request.patch.add_labels.is_empty() || !request.patch.remove_labels.is_empty())
        {
            return Err(StoreError::InvalidWork(
                "labels cannot be replaced and incrementally changed in one revision".into(),
            ));
        }
        let add_labels = normalize_strings(&request.patch.add_labels);
        let remove_labels = normalize_strings(&request.patch.remove_labels);
        let add_label_keys = add_labels
            .iter()
            .map(|label| normalize_work_catalog_key(label))
            .collect::<HashSet<_>>();
        let remove_label_keys = remove_labels
            .iter()
            .map(|label| normalize_work_catalog_key(label))
            .collect::<HashSet<_>>();
        if !add_label_keys.is_disjoint(&remove_label_keys) {
            return Err(StoreError::InvalidWork(
                "the same label cannot be added and removed in one revision".into(),
            ));
        }
        if request
            .patch
            .priority
            .is_some_and(|priority| !(0..=4).contains(&priority))
        {
            return Err(StoreError::InvalidWork(
                "priority must be an integer from 0 through 4".into(),
            ));
        }
        let changed = request.patch.title.is_some()
            || request.patch.outcome.is_some()
            || request.patch.acceptance.is_some()
            || request.patch.kind.is_some()
            || request.patch.priority.is_some()
            || request.patch.labels.is_some()
            || !add_labels.is_empty()
            || !remove_labels.is_empty()
            || request.patch.assigned_to.is_some()
            || request.patch.clear_assignment
            || request.patch.deferred_until.is_some()
            || request.patch.clear_deferral;
        if !changed {
            return Err(StoreError::InvalidWork("revision patch is empty".into()));
        }
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(item) = replay_operation::<WorkItem>(
            &transaction,
            "revise_work",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(item);
        }
        let mut item = load_work_item(&transaction, request.work_id)?;
        assert_revision(&item, request.expected_revision)?;
        validate_planning_authority(
            &transaction,
            &item,
            &request.authority,
            &request.actor,
            request.updated_at,
        )?;
        if !matches!(
            item.lifecycle,
            WorkLifecycle::Open | WorkLifecycle::Proposed
        ) {
            return Err(StoreError::WorkNotOpen(item.work_id));
        }
        if let Some(title) = request.patch.title.as_deref() {
            item.title = normalize_text(title, "title")?;
        }
        if let Some(outcome) = request.patch.outcome.as_deref() {
            item.outcome = normalize_text(outcome, "outcome")?;
        }
        if let Some(acceptance) = request.patch.acceptance.as_ref() {
            item.acceptance = normalize_strings(acceptance);
        }
        if let Some(kind) = request.patch.kind {
            item.kind = kind;
        }
        if let Some(priority) = request.patch.priority {
            item.priority = priority;
        }
        if let Some(labels) = request.patch.labels.as_ref() {
            item.labels = normalize_strings(labels);
        } else if !add_labels.is_empty() || !remove_labels.is_empty() {
            item.labels.retain(|current| {
                !remove_label_keys.contains(&normalize_work_catalog_key(current))
            });
            let mut current_label_keys = item
                .labels
                .iter()
                .map(|label| normalize_work_catalog_key(label))
                .collect::<HashSet<_>>();
            for label in add_labels {
                if current_label_keys.insert(normalize_work_catalog_key(&label)) {
                    item.labels.push(label);
                }
            }
            item.labels = normalize_strings(&item.labels);
        }
        if request.patch.clear_assignment {
            item.assigned_to = None;
        } else if request.patch.assigned_to.is_some() {
            item.assigned_to = normalize_optional(request.patch.assigned_to.clone());
        }
        if request.patch.clear_deferral {
            item.deferred_until = None;
        } else if let Some(deferred_until) = request.patch.deferred_until {
            item.deferred_until = Some(deferred_until);
        }
        item.revision += 1;
        item.updated_at = request.updated_at;
        persist_work_item(&transaction, &item)?;
        let (claim_snapshot, rebased_run) =
            rebase_planning_claim(&transaction, &item, &request.authority, request.updated_at)?;
        let event = WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: item.active_run_id,
            revision: item.revision,
            work: item.clone(),
            run: match rebased_run {
                Some(run) => Some(run),
                None => active_run_snapshot(&transaction, &item)?,
            },
            root_execution: None,
            claim: claim_snapshot,
            handoff_offer: None,
            blocker: None,
            transition: WorkTransition::Revised {
                authority: request.authority.clone(),
            },
            actor: request.actor.clone(),
            created_at: request.updated_at,
        };
        append_work_event(&transaction, &event)?;
        persist_operation_result(
            &transaction,
            "revise_work",
            &request.idempotency_key,
            request_object.hash(),
            &item,
        )?;
        transaction.commit()?;
        Ok(item)
    }

    /// Adds an explicit prerequisite and rejects cycles in the combined graph.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when work is absent or stale, projects differ,
    /// the combined graph cycles, or persistence fails.
    pub fn add_work_prerequisite<R: Redactor>(
        &mut self,
        request: &ChangeWorkPrerequisiteRequest,
        redactor: &R,
    ) -> Result<WorkItem, StoreError> {
        self.change_work_prerequisite(request, redactor, true)
    }

    /// Removes an explicit prerequisite under optimistic revision control.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when work is absent or stale, projects differ,
    /// idempotency conflicts, or persistence fails.
    pub fn remove_work_prerequisite<R: Redactor>(
        &mut self,
        request: &ChangeWorkPrerequisiteRequest,
        redactor: &R,
    ) -> Result<WorkItem, StoreError> {
        self.change_work_prerequisite(request, redactor, false)
    }

    fn change_work_prerequisite<R: Redactor>(
        &mut self,
        request: &ChangeWorkPrerequisiteRequest,
        redactor: &R,
        add: bool,
    ) -> Result<WorkItem, StoreError> {
        inspect_work_request(redactor, request)?;
        if request.work_id == request.prerequisite_id {
            return Err(StoreError::WorkDependencyCycle);
        }
        let operation = if add {
            "add_work_prerequisite"
        } else {
            "remove_work_prerequisite"
        };
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(item) = replay_operation::<WorkItem>(
            &transaction,
            operation,
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(item);
        }
        let mut item = load_work_item(&transaction, request.work_id)?;
        let prerequisite = load_work_item(&transaction, request.prerequisite_id)?;
        require_work_item_relation_integrity(&transaction, item.work_id)?;
        assert_revision(&item, request.expected_revision)?;
        validate_planning_authority(
            &transaction,
            &item,
            &request.authority,
            &request.actor,
            request.changed_at,
        )?;
        if item.project_id != prerequisite.project_id {
            return Err(StoreError::InvalidWork(
                "prerequisite edges cannot cross projects".into(),
            ));
        }
        if item.lifecycle != WorkLifecycle::Open {
            return Err(StoreError::WorkNotOpen(item.work_id));
        }
        if add {
            if prerequisite.lifecycle == WorkLifecycle::Completed {
                return Err(StoreError::WorkPrerequisiteAlreadySatisfied(
                    prerequisite.work_id,
                ));
            }
            if prerequisite.lifecycle != WorkLifecycle::Open {
                return Err(StoreError::WorkNotOpen(prerequisite.work_id));
            }
            if work_is_ancestor_of(&transaction, prerequisite.work_id, &item)? {
                return Err(StoreError::WorkDependencyCycle);
            }
        }
        let exists: Option<String> = transaction
            .query_row(
                "SELECT event_hash FROM work_prerequisites
                 WHERE work_id = ?1 AND prerequisite_id = ?2",
                params![
                    item.work_id.0.to_string(),
                    prerequisite.work_id.0.to_string()
                ],
                |row| row.get(0),
            )
            .optional()?;
        if add == exists.is_some() {
            persist_operation_result(
                &transaction,
                operation,
                &request.idempotency_key,
                request_object.hash(),
                &item,
            )?;
            transaction.commit()?;
            return Ok(item);
        }
        item.revision += 1;
        item.updated_at = request.changed_at;
        let (claim_snapshot, rebased_run) =
            rebase_planning_claim(&transaction, &item, &request.authority, request.changed_at)?;
        let event = WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: item.active_run_id,
            revision: item.revision,
            work: item.clone(),
            run: match rebased_run {
                Some(run) => Some(run),
                None => active_run_snapshot(&transaction, &item)?,
            },
            root_execution: None,
            claim: claim_snapshot,
            handoff_offer: None,
            blocker: None,
            transition: if add {
                WorkTransition::PrerequisiteAdded {
                    prerequisite_id: prerequisite.work_id,
                    authority: request.authority.clone(),
                }
            } else {
                WorkTransition::PrerequisiteRemoved {
                    prerequisite_id: prerequisite.work_id,
                    authority: request.authority.clone(),
                }
            },
            actor: request.actor.clone(),
            created_at: request.changed_at,
        };
        let (event_hash, _) = append_work_event(&transaction, &event)?;
        if add {
            transaction.execute(
                "INSERT INTO work_prerequisites (work_id, prerequisite_id, event_hash)
                 VALUES (?1, ?2, ?3)",
                params![
                    item.work_id.0.to_string(),
                    prerequisite.work_id.0.to_string(),
                    event_hash.as_str()
                ],
            )?;
            if !combined_graph_is_acyclic(&transaction, &item.project_id.0)? {
                return Err(StoreError::WorkDependencyCycle);
            }
        } else {
            transaction.execute(
                "DELETE FROM work_prerequisites
                 WHERE work_id = ?1 AND prerequisite_id = ?2",
                params![
                    item.work_id.0.to_string(),
                    prerequisite.work_id.0.to_string()
                ],
            )?;
        }
        persist_work_item(&transaction, &item)?;
        persist_operation_result(
            &transaction,
            operation,
            &request.idempotency_key,
            request_object.hash(),
            &item,
        )?;
        transaction.commit()?;
        Ok(item)
    }

    /// Adds a typed blocker that participates in derived readiness.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when work is not open, blocker content is invalid,
    /// idempotency conflicts, or persistence fails.
    pub fn add_work_blocker<R: Redactor>(
        &mut self,
        request: &AddWorkBlockerRequest,
        redactor: &R,
    ) -> Result<WorkBlocker, StoreError> {
        inspect_work_request(redactor, request)?;
        let detail = normalize_text(&request.detail, "blocker detail")?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(blocker) = replay_operation::<WorkBlocker>(
            &transaction,
            "add_work_blocker",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(blocker);
        }
        let mut item = load_work_item(&transaction, request.work_id)?;
        require_work_item_relation_integrity(&transaction, item.work_id)?;
        assert_revision(&item, request.expected_work_revision)?;
        validate_planning_authority(
            &transaction,
            &item,
            &request.authority,
            &request.actor,
            request.blocked_at,
        )?;
        if item.lifecycle != WorkLifecycle::Open {
            return Err(StoreError::WorkNotOpen(item.work_id));
        }
        let blocker = WorkBlocker {
            blocker_id: uuid::Uuid::now_v7().to_string(),
            work_id: item.work_id,
            kind: request.kind,
            detail,
            created_by: request.actor.clone(),
            created_at: request.blocked_at,
        };
        let blocker_object = CanonicalObject::freeze(&blocker)?;
        SqliteStore::insert_object(&transaction, "work_blocker", &blocker_object)?;
        item.revision += 1;
        item.updated_at = request.blocked_at;
        persist_work_item(&transaction, &item)?;
        let (claim_snapshot, rebased_run) =
            rebase_planning_claim(&transaction, &item, &request.authority, request.blocked_at)?;
        let event = WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: item.active_run_id,
            revision: item.revision,
            work: item.clone(),
            run: match rebased_run {
                Some(run) => Some(run),
                None => active_run_snapshot(&transaction, &item)?,
            },
            root_execution: None,
            claim: claim_snapshot,
            handoff_offer: None,
            blocker: Some(blocker.clone()),
            transition: WorkTransition::Blocked {
                blocker_id: blocker.blocker_id.clone(),
            },
            actor: request.actor.clone(),
            created_at: request.blocked_at,
        };
        let (event_hash, _) = append_work_event(&transaction, &event)?;
        transaction.execute(
            "INSERT INTO work_blockers (
                 blocker_id, work_id, state, blocker_json, created_event_hash
             ) VALUES (?1, ?2, 'active', ?3, ?4)",
            params![
                blocker.blocker_id,
                blocker.work_id.0.to_string(),
                serde_json::to_vec(&blocker)?,
                event_hash.as_str()
            ],
        )?;
        refresh_work_catalog_projection(&transaction, &item)?;
        persist_operation_result(
            &transaction,
            "add_work_blocker",
            &request.idempotency_key,
            request_object.hash(),
            &blocker,
        )?;
        transaction.commit()?;
        Ok(blocker)
    }

    /// Resolves a blocker through an immutable event and revision bump.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the blocker is absent or belongs elsewhere,
    /// idempotency conflicts, or persistence fails.
    pub fn clear_work_blocker<R: Redactor>(
        &mut self,
        request: &ClearWorkBlockerRequest,
        redactor: &R,
    ) -> Result<WorkItem, StoreError> {
        inspect_work_request(redactor, request)?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(item) = replay_operation::<WorkItem>(
            &transaction,
            "clear_work_blocker",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(item);
        }
        let mut item = load_work_item(&transaction, request.work_id)?;
        require_work_item_relation_integrity(&transaction, item.work_id)?;
        assert_revision(&item, request.expected_work_revision)?;
        validate_planning_authority(
            &transaction,
            &item,
            &request.authority,
            &request.actor,
            request.cleared_at,
        )?;
        if item.lifecycle != WorkLifecycle::Open {
            return Err(StoreError::WorkNotOpen(item.work_id));
        }
        let blocker_work: Option<String> = transaction
            .query_row(
                "SELECT work_id FROM work_blockers
                 WHERE blocker_id = ?1 AND state = 'active'",
                [&request.blocker_id],
                |row| row.get(0),
            )
            .optional()?;
        if blocker_work.as_deref() != Some(&item.work_id.0.to_string()) {
            return Err(StoreError::InvalidWork(
                "unknown blocker id for this work item".into(),
            ));
        }
        item.revision += 1;
        item.updated_at = request.cleared_at;
        persist_work_item(&transaction, &item)?;
        let (claim_snapshot, rebased_run) =
            rebase_planning_claim(&transaction, &item, &request.authority, request.cleared_at)?;
        let event = WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: item.active_run_id,
            revision: item.revision,
            work: item.clone(),
            run: match rebased_run {
                Some(run) => Some(run),
                None => active_run_snapshot(&transaction, &item)?,
            },
            root_execution: None,
            claim: claim_snapshot,
            handoff_offer: None,
            blocker: None,
            transition: WorkTransition::Unblocked {
                blocker_id: request.blocker_id.clone(),
            },
            actor: request.actor.clone(),
            created_at: request.cleared_at,
        };
        let (event_hash, _) = append_work_event(&transaction, &event)?;
        transaction.execute(
            "UPDATE work_blockers SET state = 'cleared', cleared_event_hash = ?2
             WHERE blocker_id = ?1",
            params![request.blocker_id, event_hash.as_str()],
        )?;
        refresh_work_catalog_projection(&transaction, &item)?;
        persist_operation_result(
            &transaction,
            "clear_work_blocker",
            &request.idempotency_key,
            request_object.hash(),
            &item,
        )?;
        transaction.commit()?;
        Ok(item)
    }
}

fn push_catalog_parameter(parameters: &mut Vec<Value>, value: Value) -> String {
    parameters.push(value);
    format!("?{}", parameters.len())
}

fn catalog_literal_fts_query(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn query_work_catalog_ids(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
    now: DateTime<Utc>,
    query: &WorkCatalogQuery,
) -> Result<Vec<WorkId>, StoreError> {
    let mut parameters = vec![
        Value::Text(project_id.0.clone()),
        Value::Integer(now.timestamp_millis()),
    ];
    let mut candidate_filters = vec!["candidate.project_id = ?1".to_owned()];
    if let Some(after) = query.after {
        let parameter = push_catalog_parameter(&mut parameters, Value::Text(after.0.to_string()));
        candidate_filters.push(format!("candidate.work_id > {parameter}"));
    }
    if !query.lifecycles.is_empty() {
        let placeholders = query
            .lifecycles
            .iter()
            .map(|lifecycle| {
                Ok(push_catalog_parameter(
                    &mut parameters,
                    Value::Text(encode_state(*lifecycle)?),
                ))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        candidate_filters.push(format!(
            "candidate.lifecycle IN ({})",
            placeholders.join(", ")
        ));
    }
    if let Some(assigned_to) = query
        .assigned_to
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let parameter = push_catalog_parameter(
            &mut parameters,
            Value::Text(normalize_work_catalog_key(assigned_to)),
        );
        candidate_filters.push(format!("candidate.assigned_to_key = {parameter}"));
    }
    if let Some(label) = query
        .label
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let parameter = push_catalog_parameter(
            &mut parameters,
            Value::Text(normalize_work_catalog_key(label)),
        );
        candidate_filters.push(format!(
            "EXISTS (
                 SELECT 1 FROM work_item_labels label
                 WHERE label.work_id = candidate.work_id
                   AND label.label_key = {parameter}
             )"
        ));
    }
    if let Some(search) = query
        .search
        .as_deref()
        .map(normalize_work_catalog_key)
        .filter(|value| !value.is_empty())
    {
        let search_characters = search.chars().count();
        let parameter = push_catalog_parameter(
            &mut parameters,
            Value::Text(if search_characters >= 3 {
                catalog_literal_fts_query(&search)
            } else {
                search
            }),
        );
        if search_characters >= 3 {
            candidate_filters.push(format!(
                "candidate.work_id IN (
                     SELECT work_id FROM work_catalog_fts
                     WHERE work_catalog_fts MATCH {parameter}
                 )"
            ));
        } else {
            candidate_filters.push(format!("instr(candidate.search_text_key, {parameter}) > 0"));
        }
    }

    let mut classified_filters = Vec::new();
    if !query.availabilities.is_empty() {
        let placeholders = query
            .availabilities
            .iter()
            .map(|availability| {
                Ok(push_catalog_parameter(
                    &mut parameters,
                    Value::Text(encode_state(*availability)?),
                ))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        classified_filters.push(format!("availability IN ({})", placeholders.join(", ")));
    }
    if query.blocked_only {
        candidate_filters.push(format!("({PROJECTED_WORK_HAS_BLOCKER_SQL})"));
    }
    let limit = i64::from(query.limit.clamp(1, 1_000)).saturating_add(1);
    let limit_parameter = push_catalog_parameter(&mut parameters, Value::Integer(limit));
    let classified_where = if classified_filters.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", classified_filters.join(" AND "))
    };
    let sql = format!(
        "WITH classified AS (
             SELECT candidate.work_id,
                    ({PROJECTED_WORK_AVAILABILITY_SQL}) AS availability
             FROM work_items candidate
             WHERE {}
         )
         SELECT work_id FROM classified
         {classified_where}
         ORDER BY work_id
         LIMIT {limit_parameter}",
        candidate_filters.join(" AND ")
    );
    let mut statement = connection.prepare(&sql)?;
    statement
        .query_map(rusqlite::params_from_iter(parameters.iter()), |row| {
            row.get::<_, String>(0)
        })?
        .map(|row| parse_work_id(&row?))
        .collect()
}

fn inspect_work_on(
    connection: &Connection,
    work_id: WorkId,
    now: DateTime<Utc>,
) -> Result<ReadyWork, StoreError> {
    let work = load_work_item(connection, work_id)?;
    require_work_item_relation_integrity(connection, work_id)?;
    let blockers = load_active_blocker_projections(connection, work_id)?;
    let prerequisites = classified_prerequisite_projections(connection, work_id)?;
    derive_projected_work_availability(connection, work, blockers, prerequisites, now)
}

fn inspect_work_catalog_on(
    connection: &Connection,
    work_id: WorkId,
    now: DateTime<Utc>,
) -> Result<ReadyWork, StoreError> {
    let work = load_work_item_projection(connection, work_id)?;
    let blockers = load_active_blocker_catalog_rows(connection, work_id)?;
    let prerequisites = classified_prerequisite_catalog_rows(connection, work_id)?;
    derive_projected_work_availability(connection, work, blockers, prerequisites, now)
}

fn derive_projected_work_availability(
    connection: &Connection,
    work: WorkItem,
    blockers: Vec<WorkBlocker>,
    prerequisites: Vec<(WorkId, WorkPrerequisiteState)>,
    now: DateTime<Utc>,
) -> Result<ReadyWork, StoreError> {
    let (blocked_by, has_dead_prerequisite) = prerequisite_readiness(prerequisites);
    let mut why = Vec::new();
    let mut reason_codes = Vec::new();
    let availability = if !matches!(work.lifecycle, WorkLifecycle::Open) {
        reason_codes.push(WorkReadinessReason::LifecycleClosed);
        why.push(format!("lifecycle is {:?}", work.lifecycle));
        WorkAvailability::Closed
    } else if !projected_ancestors_admit_execution(connection, &work)?
        || !projected_run_uses_active_root_execution(connection, &work)?
    {
        reason_codes.push(WorkReadinessReason::ParentDisallowsExecution);
        why.push("the ancestor or root-execution generation does not admit execution".into());
        WorkAvailability::Blocked
    } else if work.deferred_until.is_some_and(|until| until > now) {
        reason_codes.push(WorkReadinessReason::DeferredUntil);
        why.push("deferred wake time has not arrived".into());
        WorkAvailability::Deferred
    } else if !blockers.is_empty() || !blocked_by.is_empty() {
        if !blocked_by.is_empty() {
            reason_codes.push(WorkReadinessReason::PrerequisiteIncomplete);
            why.push(if has_dead_prerequisite {
                "one or more prerequisites are dead and must be removed".into()
            } else {
                "one or more prerequisites are incomplete".into()
            });
        }
        if !blockers.is_empty() {
            reason_codes.push(WorkReadinessReason::TypedBlockerActive);
            why.push("one or more typed blockers remain active".into());
        }
        WorkAvailability::Blocked
    } else {
        projected_claim_availability(connection, &work, now, &mut reason_codes, &mut why)?
    };
    if availability == WorkAvailability::Ready {
        reason_codes.push(WorkReadinessReason::ReadyUnclaimed);
        why.push("open, admitted, unblocked, and unclaimed".into());
    }
    Ok(ReadyWork {
        work,
        availability,
        reason_codes,
        why,
        blocked_by,
        blockers,
    })
}

fn inspect_work_canonical_on(
    connection: &Connection,
    work_id: WorkId,
    now: DateTime<Utc>,
) -> Result<ReadyWork, StoreError> {
    let work = load_work_item(connection, work_id)?;
    require_work_item_relation_integrity(connection, work_id)?;
    let blockers = load_active_blocker_projections(connection, work_id)?;
    let prerequisites = classified_prerequisite_projections(connection, work_id)?;
    derive_work_availability(connection, work, blockers, prerequisites, now)
}

fn derive_work_availability(
    connection: &Connection,
    work: WorkItem,
    blockers: Vec<WorkBlocker>,
    prerequisites: Vec<(WorkId, WorkPrerequisiteState)>,
    now: DateTime<Utc>,
) -> Result<ReadyWork, StoreError> {
    let (blocked_by, has_dead_prerequisite) = prerequisite_readiness(prerequisites);
    let mut why = Vec::new();
    let mut reason_codes = Vec::new();
    let availability = if !matches!(work.lifecycle, WorkLifecycle::Open) {
        reason_codes.push(WorkReadinessReason::LifecycleClosed);
        why.push(format!("lifecycle is {:?}", work.lifecycle));
        WorkAvailability::Closed
    } else if !ancestors_admit_execution(connection, &work)?
        || !work_run_uses_active_root_execution(connection, &work)?
    {
        reason_codes.push(WorkReadinessReason::ParentDisallowsExecution);
        why.push("the ancestor or root-execution generation does not admit execution".into());
        WorkAvailability::Blocked
    } else if work.deferred_until.is_some_and(|until| until > now) {
        reason_codes.push(WorkReadinessReason::DeferredUntil);
        why.push("deferred wake time has not arrived".into());
        WorkAvailability::Deferred
    } else if !blockers.is_empty() || !blocked_by.is_empty() {
        if !blocked_by.is_empty() {
            reason_codes.push(WorkReadinessReason::PrerequisiteIncomplete);
            why.push(if has_dead_prerequisite {
                "one or more prerequisites are dead and must be removed".into()
            } else {
                "one or more prerequisites are incomplete".into()
            });
        }
        if !blockers.is_empty() {
            reason_codes.push(WorkReadinessReason::TypedBlockerActive);
            why.push("one or more typed blockers remain active".into());
        }
        WorkAvailability::Blocked
    } else {
        claim_availability(connection, &work, now, &mut reason_codes, &mut why)?
    };
    if availability == WorkAvailability::Ready {
        reason_codes.push(WorkReadinessReason::ReadyUnclaimed);
        why.push("open, admitted, unblocked, and unclaimed".into());
    }
    Ok(ReadyWork {
        work,
        availability,
        reason_codes,
        why,
        blocked_by,
        blockers,
    })
}

fn prerequisite_readiness(
    prerequisites: Vec<(WorkId, WorkPrerequisiteState)>,
) -> (Vec<WorkId>, bool) {
    let has_dead = prerequisites
        .iter()
        .any(|(_, state)| *state == WorkPrerequisiteState::Dead);
    let blocked_by = prerequisites
        .into_iter()
        .filter_map(|(work_id, state)| {
            (state != WorkPrerequisiteState::Satisfied).then_some(work_id)
        })
        .collect();
    (blocked_by, has_dead)
}

fn claim_availability(
    connection: &Connection,
    work: &WorkItem,
    now: DateTime<Utc>,
    reason_codes: &mut Vec<WorkReadinessReason>,
    why: &mut Vec<String>,
) -> Result<WorkAvailability, StoreError> {
    let Some(run_id) = work.active_run_id else {
        return Err(StoreError::InvalidWorkProjection(format!(
            "open work {:?} has no active run",
            work.work_id
        )));
    };
    let run = load_work_run(connection, run_id)?;
    let Some(claim) = load_work_claim_optional(connection, run_id)? else {
        return Ok(WorkAvailability::Ready);
    };
    if claim.state != WorkClaimState::Active || claim.expires_at <= now {
        reason_codes.push(WorkReadinessReason::PriorClaimRecoverable);
        why.push("prior claim is recoverable".into());
        return Ok(WorkAvailability::Ready);
    }
    if run.last_checkpoint.is_some() {
        reason_codes.push(WorkReadinessReason::LiveClaimWithCheckpoint);
        why.push("live claim has checkpointed progress".into());
        Ok(WorkAvailability::Active)
    } else {
        reason_codes.push(WorkReadinessReason::LiveClaimWithoutCheckpoint);
        why.push("live claim has not checkpointed progress".into());
        Ok(WorkAvailability::Claimed)
    }
}

fn projected_ancestors_admit_execution(
    connection: &Connection,
    item: &WorkItem,
) -> Result<bool, StoreError> {
    let mut parent_id = item.parent_id;
    let mut visited = HashSet::new();
    let mut reached_root = item.work_id == item.root_id;
    while let Some(parent) = parent_id {
        if !visited.insert(parent) || visited.len() > 1_024 {
            return Err(StoreError::InvalidWorkProjection(
                "work hierarchy is cyclic or exceeds the corruption guard".into(),
            ));
        }
        let row: Option<(String, String, Option<String>, String)> = connection
            .query_row(
                "SELECT project_id, root_id, parent_id, lifecycle
                 FROM work_items WHERE work_id = ?1",
                [parent.0.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let (project_id, root_id, next_parent, lifecycle) = row.ok_or_else(|| {
            StoreError::InvalidWorkProjection(format!("work ancestor {parent:?} is missing"))
        })?;
        if project_id != item.project_id.0 || root_id != item.root_id.0.to_string() {
            return Err(StoreError::InvalidWorkProjection(format!(
                "work ancestor {parent:?} crosses its project or root boundary"
            )));
        }
        if lifecycle != "open" {
            return Ok(false);
        }
        reached_root |= parent == item.root_id;
        parent_id = next_parent.map(|value| parse_work_id(&value)).transpose()?;
    }
    if !reached_root {
        return Err(StoreError::InvalidWorkProjection(format!(
            "work {:?} does not reach its declared root {:?}",
            item.work_id, item.root_id
        )));
    }
    Ok(true)
}

fn projected_run_uses_active_root_execution(
    connection: &Connection,
    item: &WorkItem,
) -> Result<bool, StoreError> {
    let run_id = item.active_run_id.ok_or_else(|| {
        StoreError::InvalidWorkProjection(format!("open work {:?} has no active run", item.work_id))
    })?;
    connection
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM work_runs run
                 JOIN work_root_executions execution
                   ON execution.root_execution_id = run.root_execution_id
                 WHERE run.run_id = ?1 AND run.work_id = ?2
                   AND execution.project_id = ?3 AND execution.root_id = ?4
                   AND execution.state = 'active'
             )",
            params![
                run_id.0.to_string(),
                item.work_id.0.to_string(),
                item.project_id.0,
                item.root_id.0.to_string()
            ],
            |row| row.get(0),
        )
        .map_err(StoreError::from)
}

fn projected_claim_availability(
    connection: &Connection,
    work: &WorkItem,
    now: DateTime<Utc>,
    reason_codes: &mut Vec<WorkReadinessReason>,
    why: &mut Vec<String>,
) -> Result<WorkAvailability, StoreError> {
    let run_id = work.active_run_id.ok_or_else(|| {
        StoreError::InvalidWorkProjection(format!("open work {:?} has no active run", work.work_id))
    })?;
    let row: Option<(Option<String>, Option<String>, Option<i64>)> = connection
        .query_row(
            "SELECT run.last_checkpoint_hash, claim.state, claim.expires_at_ms
             FROM work_runs run
             LEFT JOIN work_claims claim ON claim.run_id = run.run_id
             WHERE run.run_id = ?1 AND run.work_id = ?2",
            params![run_id.0.to_string(), work.work_id.0.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let (checkpoint, claim_state, expires_at) =
        row.ok_or_else(|| StoreError::InvalidWorkProjection(format!("run {run_id:?} is missing")))?;
    if claim_state.as_deref() != Some("active")
        || expires_at.is_none_or(|expires_at| expires_at <= now.timestamp_millis())
    {
        if claim_state.is_some() {
            reason_codes.push(WorkReadinessReason::PriorClaimRecoverable);
            why.push("prior claim is recoverable".into());
        }
        return Ok(WorkAvailability::Ready);
    }
    if checkpoint.is_some() {
        reason_codes.push(WorkReadinessReason::LiveClaimWithCheckpoint);
        why.push("live claim has checkpointed progress".into());
        Ok(WorkAvailability::Active)
    } else {
        reason_codes.push(WorkReadinessReason::LiveClaimWithoutCheckpoint);
        why.push("live claim has not checkpointed progress".into());
        Ok(WorkAvailability::Claimed)
    }
}

fn latest_canonical_work_event_for_item(
    connection: &Connection,
    work_id: WorkId,
) -> Result<WorkEvent, StoreError> {
    latest_canonical_work_event_for_item_optional(connection, work_id)?.ok_or_else(|| {
        StoreError::InvalidWorkProjection(format!("work item {work_id:?} has no canonical event"))
    })
}

fn latest_canonical_work_event_for_item_optional(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Option<WorkEvent>, StoreError> {
    let projected_hash = connection
        .query_row(
            "SELECT latest_event_hash FROM work_items WHERE work_id = ?1",
            [work_id.0.to_string()],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?;
    let latest = connection
        .query_row(
            "SELECT object.object_hash, object.canonical_json
             FROM objects object
             JOIN work_feed_entries entry ON entry.object_hash = object.object_hash
             WHERE object.object_kind = 'work_event'
               AND entry.feed_kind = 'project'
               AND entry.object_kind = 'work_event'
               AND entry.work_id = ?1
             ORDER BY entry.position DESC LIMIT 1",
            [work_id.0.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?;
    match (projected_hash.flatten(), latest) {
        (None, None) => Ok(None),
        (Some(projected_hash), Some((stored_hash, bytes))) => {
            let projected_hash = ObjectHash::from_stored(projected_hash.clone())
                .ok_or(StoreError::InvalidStoredHash(projected_hash))?;
            let stored_hash_value = ObjectHash::from_stored(stored_hash.clone())
                .ok_or(StoreError::InvalidStoredHash(stored_hash.clone()))?;
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

fn canonical_work_events_for_item(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<WorkEvent>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT object.object_hash, object.canonical_json
         FROM objects object
         JOIN work_feed_entries entry ON entry.object_hash = object.object_hash
         WHERE object.object_kind = 'work_event'
           AND entry.feed_kind = 'project'
           AND entry.object_kind = 'work_event'
           AND entry.work_id = ?1
         ORDER BY entry.position",
    )?;
    statement
        .query_map([work_id.0.to_string()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?
        .map(|row| decode_canonical_work_event(row?))
        .collect()
}

fn latest_canonical_work_event_on_feed(
    connection: &Connection,
    feed_kind: &str,
    feed_id: &str,
    required_snapshot: &str,
) -> Result<WorkEvent, StoreError> {
    let stored = connection
        .query_row(
            "SELECT object.object_hash, object.canonical_json
             FROM work_feed_entries entry
             JOIN objects object ON object.object_hash = entry.object_hash
             WHERE entry.feed_kind = ?1 AND entry.feed_id = ?2
               AND entry.object_kind = 'work_event'
               AND object.object_kind = 'work_event'
               AND json_type(object.canonical_json, ?3) IS NOT NULL
               AND json_type(object.canonical_json, ?3) != 'null'
             ORDER BY entry.position DESC LIMIT 1",
            params![feed_kind, feed_id, required_snapshot],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?
        .ok_or_else(|| {
            StoreError::InvalidWorkProjection(format!(
                "work feed {feed_kind}:{feed_id} has no canonical event"
            ))
        })?;
    decode_canonical_work_event(stored)
}

fn decode_canonical_work_event(stored: (String, Vec<u8>)) -> Result<WorkEvent, StoreError> {
    #[cfg(test)]
    WORK_EVENT_DECODE_COUNT.with(|count| count.set(count.get().saturating_add(1)));
    let (stored_hash, bytes) = stored;
    let hash = ObjectHash::from_stored(stored_hash.clone())
        .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
    CanonicalObject::verify(&hash, bytes)?.decode()
}

fn load_work_item_projection(
    connection: &Connection,
    work_id: WorkId,
) -> Result<WorkItem, StoreError> {
    let row: Option<(Vec<u8>, bool)> = connection
        .query_row(
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
                    COALESCE(source_snapshot_hash, '') = COALESCE(json_extract(item_json, '$.source_snapshot_id'), '') AND
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
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (bytes, scalar_bound) = row.ok_or(StoreError::WorkNotFound(work_id))?;
    #[cfg(test)]
    WORK_ITEM_PROJECTION_DECODE_COUNT.with(|count| count.set(count.get().saturating_add(1)));
    let item: WorkItem = serde_json::from_slice(&bytes)?;
    if !scalar_bound || item.work_id != work_id {
        return Err(StoreError::InvalidWorkProjection(format!(
            "work item {work_id:?} differs from its scalar projection binding"
        )));
    }
    Ok(item)
}

fn load_work_item(connection: &Connection, work_id: WorkId) -> Result<WorkItem, StoreError> {
    let item = load_work_item_projection(connection, work_id)?;
    let event = latest_canonical_work_event_for_item(connection, work_id)?;
    if event.work != item {
        return Err(StoreError::InvalidWorkProjection(format!(
            "work item {work_id:?} differs from its latest canonical event"
        )));
    }
    Ok(item)
}

pub(super) fn verified_work_identity(
    connection: &Connection,
    work_id: WorkId,
) -> Result<(crate::domain::ProjectId, WorkId), StoreError> {
    let item = load_work_item(connection, work_id)?;
    Ok((item.project_id, item.root_id))
}

pub(super) fn context_work_feed_heads(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<FeedPosition>, StoreError> {
    let item = load_work_item(connection, work_id)?;
    let mut feeds = vec![
        FeedId::Project(item.project_id),
        FeedId::RootWork(item.root_id),
    ];
    if let Some(run_id) = item.active_run_id {
        feeds.push(FeedId::RunExecution(run_id));
    }
    feeds
        .into_iter()
        .map(|feed| {
            Ok(FeedPosition {
                position: feed_head(connection, &feed)?,
                feed,
            })
        })
        .collect()
}

fn load_work_items_query(
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

fn load_work_run(connection: &Connection, run_id: WorkRunId) -> Result<WorkRun, StoreError> {
    let row: Option<(Vec<u8>, bool)> = connection
        .query_row(
            "SELECT run_json,
                    run_id = json_extract(run_json, '$.run_id') AND
                    root_execution_id = json_extract(run_json, '$.root_execution_id') AND
                    work_id = json_extract(run_json, '$.work_id') AND
                    generation = json_extract(run_json, '$.generation') AND
                    COALESCE(executor_session_id, '') = COALESCE(json_extract(run_json, '$.executor'), '') AND
                    state = json_extract(run_json, '$.state') AND
                    revision = json_extract(run_json, '$.revision') AND
                    COALESCE(last_checkpoint_hash, '') = COALESCE(json_extract(run_json, '$.last_checkpoint'), '') AND
                    COALESCE(completion_seal_hash, '') = COALESCE(json_extract(run_json, '$.completion_seal'), '')
             FROM work_runs WHERE run_id = ?1",
            [run_id.0.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (bytes, scalar_bound) =
        row.ok_or_else(|| StoreError::InvalidWorkProjection(format!("run {run_id:?} is missing")))?;
    let run: WorkRun = serde_json::from_slice(&bytes)?;
    let event = latest_canonical_work_event_on_feed(
        connection,
        "run_execution",
        &run_id.0.to_string(),
        "$.run",
    )?;
    if !scalar_bound || run.run_id != run_id || event.run.as_ref() != Some(&run) {
        return Err(StoreError::InvalidWorkProjection(format!(
            "work run {run_id:?} differs from its scalar or canonical event binding"
        )));
    }
    Ok(run)
}

fn active_run_snapshot(
    connection: &Connection,
    item: &WorkItem,
) -> Result<Option<WorkRun>, StoreError> {
    item.active_run_id
        .map(|run_id| load_work_run(connection, run_id))
        .transpose()
}

fn load_root_execution(
    connection: &Connection,
    root_execution_id: RootExecutionId,
) -> Result<RootExecution, StoreError> {
    let row: Option<(Vec<u8>, bool)> = connection
        .query_row(
            "SELECT execution_json,
                    root_execution_id = json_extract(execution_json, '$.root_execution_id') AND
                    project_id = json_extract(execution_json, '$.project_id') AND
                    root_id = json_extract(execution_json, '$.root_id') AND
                    generation = json_extract(execution_json, '$.generation') AND
                    state = json_extract(execution_json, '$.state') AND
                    revision = json_extract(execution_json, '$.revision')
             FROM work_root_executions WHERE root_execution_id = ?1",
            [root_execution_id.0.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (bytes, scalar_bound) = row.ok_or_else(|| {
        StoreError::InvalidWorkProjection(format!(
            "root execution {root_execution_id:?} is missing"
        ))
    })?;
    let execution: RootExecution = serde_json::from_slice(&bytes)?;
    let event = latest_canonical_work_event_on_feed(
        connection,
        "root_work",
        &execution.root_id.0.to_string(),
        "$.root_execution",
    )?;
    if !scalar_bound
        || execution.root_execution_id != root_execution_id
        || event.root_execution.as_ref() != Some(&execution)
    {
        return Err(StoreError::InvalidWorkProjection(format!(
            "root execution {root_execution_id:?} differs from its scalar or canonical event binding"
        )));
    }
    Ok(execution)
}

fn active_root_execution(
    connection: &Connection,
    root_id: WorkId,
) -> Result<RootExecution, StoreError> {
    active_root_execution_optional(connection, root_id)?.ok_or_else(|| {
        StoreError::InvalidWorkProjection(format!("root work {root_id:?} has no active execution"))
    })
}

fn active_root_execution_optional(
    connection: &Connection,
    root_id: WorkId,
) -> Result<Option<RootExecution>, StoreError> {
    let row: Option<(Vec<u8>, bool)> = connection
        .query_row(
            "SELECT execution_json,
                    root_execution_id = json_extract(execution_json, '$.root_execution_id') AND
                    project_id = json_extract(execution_json, '$.project_id') AND
                    root_id = json_extract(execution_json, '$.root_id') AND
                    generation = json_extract(execution_json, '$.generation') AND
                    state = json_extract(execution_json, '$.state') AND
                    revision = json_extract(execution_json, '$.revision')
             FROM work_root_executions
             WHERE root_id = ?1 AND state = 'active'",
            [root_id.0.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((bytes, scalar_bound)) = row else {
        return Ok(None);
    };
    let execution: RootExecution = serde_json::from_slice(&bytes)?;
    let event = latest_canonical_work_event_on_feed(
        connection,
        "root_work",
        &root_id.0.to_string(),
        "$.root_execution",
    )?;
    if !scalar_bound
        || execution.root_id != root_id
        || event.root_execution.as_ref() != Some(&execution)
    {
        return Err(StoreError::InvalidWorkProjection(format!(
            "active root execution for {root_id:?} differs from canonical history"
        )));
    }
    Ok(Some(execution))
}

fn load_work_claim_optional(
    connection: &Connection,
    run_id: WorkRunId,
) -> Result<Option<WorkClaim>, StoreError> {
    let row = connection
        .query_row(
            "SELECT claim_json,
                    run_id = json_extract(claim_json, '$.run_id') AND
                    work_id = json_extract(claim_json, '$.work_id') AND
                    claim_id = json_extract(claim_json, '$.claim_id') AND
                    holder_session_id = json_extract(claim_json, '$.holder') AND
                    state = json_extract(claim_json, '$.state') AND
                    revision = json_extract(claim_json, '$.revision') AND
                    fence = json_extract(claim_json, '$.fence') AND
                    expires_at_ms = CAST(strftime('%s', json_extract(claim_json, '$.expires_at')) AS INTEGER) * 1000
                        + CASE WHEN instr(json_extract(claim_json, '$.expires_at'), '.') > 0
                            THEN CAST(substr(
                                substr(
                                    json_extract(claim_json, '$.expires_at'),
                                    instr(json_extract(claim_json, '$.expires_at'), '.') + 1,
                                    instr(json_extract(claim_json, '$.expires_at'), 'Z')
                                        - instr(json_extract(claim_json, '$.expires_at'), '.') - 1
                                ) || '000', 1, 3
                            ) AS INTEGER)
                            ELSE 0 END
             FROM work_claims WHERE run_id = ?1",
            [run_id.0.to_string()],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, bool>(1)?)),
        )
        .optional()?;
    let required_snapshot = if row.is_some() { "$.claim" } else { "$.run" };
    let event = latest_canonical_work_event_on_feed(
        connection,
        "run_execution",
        &run_id.0.to_string(),
        required_snapshot,
    )?;
    match row {
        None if event.claim.is_none() => Ok(None),
        Some((bytes, scalar_bound)) => {
            let claim: WorkClaim = serde_json::from_slice(&bytes)?;
            if !scalar_bound || claim.run_id != run_id || event.claim.as_ref() != Some(&claim) {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "work claim for {run_id:?} differs from canonical history"
                )));
            }
            Ok(Some(claim))
        }
        None => Err(StoreError::InvalidWorkProjection(format!(
            "canonical run {run_id:?} has a missing claim projection"
        ))),
    }
}

fn load_active_blocker_projections(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<WorkBlocker>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT blocker_json, created_event_hash,
                blocker_id = json_extract(blocker_json, '$.blocker_id') AND
                work_id = json_extract(blocker_json, '$.work_id')
         FROM work_blockers
         WHERE work_id = ?1 AND state = 'active'
         ORDER BY blocker_id",
    )?;
    statement
        .query_map([work_id.0.to_string()], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, bool>(2)?,
            ))
        })?
        .map(|row| {
            let (bytes, event_hash, scalar_bound) = row?;
            let blocker: WorkBlocker = serde_json::from_slice(&bytes)?;
            let event_hash = ObjectHash::from_stored(event_hash.clone())
                .ok_or(StoreError::InvalidStoredHash(event_hash))?;
            let event: WorkEvent = load_typed_work_object(connection, &event_hash, "work_event")?;
            if !scalar_bound
                || blocker.work_id != work_id
                || event.work_id != work_id
                || event.blocker.as_ref() != Some(&blocker)
                || !matches!(
                    event.transition,
                    WorkTransition::Blocked { ref blocker_id }
                        if blocker_id == &blocker.blocker_id
                )
            {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "active blocker {} differs from its scalar or event binding",
                    blocker.blocker_id
                )));
            }
            Ok(blocker)
        })
        .collect()
}

fn load_active_blocker_catalog_rows(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<WorkBlocker>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT blocker_json,
                blocker_id = json_extract(blocker_json, '$.blocker_id') AND
                work_id = json_extract(blocker_json, '$.work_id')
         FROM work_blockers
         WHERE work_id = ?1 AND state = 'active'
         ORDER BY blocker_id",
    )?;
    statement
        .query_map([work_id.0.to_string()], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, bool>(1)?))
        })?
        .map(|row| {
            let (bytes, scalar_bound) = row?;
            let blocker: WorkBlocker = serde_json::from_slice(&bytes)?;
            if !scalar_bound || blocker.work_id != work_id {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "active blocker {} differs from its scalar binding",
                    blocker.blocker_id
                )));
            }
            Ok(blocker)
        })
        .collect()
}

fn classified_prerequisite_projections(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<(WorkId, WorkPrerequisiteState)>, StoreError> {
    let prerequisite_ids = load_prerequisite_projection_ids(connection, work_id)?;
    let mut classified = Vec::with_capacity(prerequisite_ids.len());
    for prerequisite_id in prerequisite_ids {
        let prerequisite = load_work_item(connection, prerequisite_id)?;
        classified.push((
            prerequisite_id,
            work_prerequisite_state(connection, &prerequisite)?,
        ));
    }
    Ok(classified)
}

fn incomplete_prerequisite_projections(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<WorkId>, StoreError> {
    Ok(classified_prerequisite_projections(connection, work_id)?
        .into_iter()
        .filter_map(|(work_id, state)| {
            (state != WorkPrerequisiteState::Satisfied).then_some(work_id)
        })
        .collect())
}

fn load_prerequisite_projection_ids(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<WorkId>, StoreError> {
    let prerequisite_ids = {
        let mut statement = connection.prepare(
            "SELECT prerequisite_id, event_hash FROM work_prerequisites
             WHERE work_id = ?1 ORDER BY prerequisite_id",
        )?;
        statement
            .query_map([work_id.0.to_string()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .map(|row| {
                let (prerequisite_id, event_hash) = row?;
                Ok((
                    parse_work_id(&prerequisite_id)?,
                    ObjectHash::from_stored(event_hash.clone())
                        .ok_or(StoreError::InvalidStoredHash(event_hash))?,
                ))
            })
            .collect::<Result<Vec<_>, StoreError>>()?
    };
    let mut bound = Vec::with_capacity(prerequisite_ids.len());
    for (prerequisite_id, event_hash) in prerequisite_ids {
        let event: WorkEvent = load_typed_work_object(connection, &event_hash, "work_event")?;
        let event_binds_edge = event.work_id == work_id
            && match &event.transition {
                WorkTransition::Created { prerequisites, .. } => {
                    prerequisites.contains(&prerequisite_id)
                }
                WorkTransition::PrerequisiteAdded {
                    prerequisite_id: added,
                    ..
                } => *added == prerequisite_id,
                _ => false,
            };
        if !event_binds_edge {
            return Err(StoreError::InvalidWorkProjection(format!(
                "prerequisite edge {work_id:?}->{prerequisite_id:?} differs from its event binding"
            )));
        }
        bound.push(prerequisite_id);
    }
    Ok(bound)
}

fn classified_prerequisite_catalog_rows(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<(WorkId, WorkPrerequisiteState)>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT edge.prerequisite_id, prerequisite.lifecycle, replacement.lifecycle
         FROM work_prerequisites edge
         LEFT JOIN work_items prerequisite
           ON prerequisite.work_id = edge.prerequisite_id
         LEFT JOIN work_items replacement
           ON replacement.work_id = prerequisite.superseded_by
         WHERE edge.work_id = ?1
         ORDER BY edge.prerequisite_id",
    )?;
    statement
        .query_map([work_id.0.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .map(|row| {
            let (prerequisite_id, lifecycle, replacement_lifecycle) = row?;
            let prerequisite_id = parse_work_id(&prerequisite_id)?;
            let lifecycle = lifecycle.ok_or_else(|| {
                StoreError::InvalidWorkProjection(format!(
                    "prerequisite {prerequisite_id:?} is missing"
                ))
            })?;
            let state = projected_prerequisite_state(
                &lifecycle,
                replacement_lifecycle.as_deref(),
                prerequisite_id,
            )?;
            Ok((prerequisite_id, state))
        })
        .collect()
}

fn bounded_prerequisite_projection_rows(
    connection: &Connection,
    work_id: WorkId,
    limit: usize,
) -> Result<WorkPrerequisitePage, StoreError> {
    let mut statement = connection.prepare(
        "SELECT edge.prerequisite_id, prerequisite.short_ref,
                prerequisite.lifecycle, replacement.lifecycle
         FROM work_prerequisites edge
         LEFT JOIN work_items prerequisite
           ON prerequisite.work_id = edge.prerequisite_id
         LEFT JOIN work_items replacement
           ON replacement.work_id = prerequisite.superseded_by
         WHERE edge.work_id = ?1",
    )?;
    let mut classified = statement
        .query_map([work_id.0.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?
        .map(|row| {
            let (stored_id, short_ref, lifecycle, replacement_lifecycle) = row?;
            let prerequisite_id = parse_work_id(&stored_id)?;
            let short_ref = short_ref.ok_or_else(|| {
                StoreError::InvalidWorkProjection(format!(
                    "prerequisite {prerequisite_id:?} is missing its catalog row"
                ))
            })?;
            let lifecycle = lifecycle.ok_or_else(|| {
                StoreError::InvalidWorkProjection(format!(
                    "prerequisite {prerequisite_id:?} is missing"
                ))
            })?;
            let state = projected_prerequisite_state(
                &lifecycle,
                replacement_lifecycle.as_deref(),
                prerequisite_id,
            )?;
            Ok((prerequisite_id, short_ref, state))
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    drop(statement);

    classified.sort_by(|left, right| {
        prerequisite_state_rank(left.2)
            .cmp(&prerequisite_state_rank(right.2))
            .then_with(|| left.1.cmp(&right.1))
    });
    let mut totals = [0_usize; 3];
    for (_, _, state) in &classified {
        totals[prerequisite_state_rank(*state)] += 1;
    }
    let selected = classified.into_iter().take(limit).collect::<Vec<_>>();
    let mut selected_counts = [0_usize; 3];
    let mut items = Vec::with_capacity(selected.len());
    for (prerequisite_id, _, state) in selected {
        selected_counts[prerequisite_state_rank(state)] += 1;
        items.push((load_work_item(connection, prerequisite_id)?, state));
    }
    Ok(WorkPrerequisitePage {
        items,
        omitted_by_state: std::array::from_fn(|index| totals[index] - selected_counts[index]),
    })
}

const fn prerequisite_state_rank(state: WorkPrerequisiteState) -> usize {
    match state {
        WorkPrerequisiteState::Dead => 0,
        WorkPrerequisiteState::Pending => 1,
        WorkPrerequisiteState::Satisfied => 2,
    }
}

fn work_prerequisite_state(
    connection: &Connection,
    prerequisite: &WorkItem,
) -> Result<WorkPrerequisiteState, StoreError> {
    let replacement_lifecycle = prerequisite
        .superseded_by
        .map(|replacement| load_work_item(connection, replacement))
        .transpose()?
        .map(|replacement| replacement.lifecycle);
    classify_prerequisite_state(
        prerequisite.lifecycle,
        replacement_lifecycle,
        prerequisite.work_id,
    )
}

fn classify_prerequisite_state(
    lifecycle: WorkLifecycle,
    replacement_lifecycle: Option<WorkLifecycle>,
    prerequisite_id: WorkId,
) -> Result<WorkPrerequisiteState, StoreError> {
    match lifecycle {
        WorkLifecycle::Completed => Ok(WorkPrerequisiteState::Satisfied),
        WorkLifecycle::Cancelled => Ok(WorkPrerequisiteState::Dead),
        WorkLifecycle::Open | WorkLifecycle::Proposed => Ok(WorkPrerequisiteState::Pending),
        WorkLifecycle::Superseded => match replacement_lifecycle {
            Some(WorkLifecycle::Completed) => Ok(WorkPrerequisiteState::Satisfied),
            Some(WorkLifecycle::Open | WorkLifecycle::Proposed) => {
                Ok(WorkPrerequisiteState::Pending)
            }
            Some(WorkLifecycle::Cancelled | WorkLifecycle::Superseded) => {
                Ok(WorkPrerequisiteState::Dead)
            }
            None => Err(StoreError::InvalidWorkProjection(format!(
                "superseded prerequisite {prerequisite_id:?} has no replacement projection"
            ))),
        },
    }
}

fn projected_prerequisite_state(
    lifecycle: &str,
    replacement_lifecycle: Option<&str>,
    prerequisite_id: WorkId,
) -> Result<WorkPrerequisiteState, StoreError> {
    let lifecycle = match lifecycle {
        "proposed" => WorkLifecycle::Proposed,
        "open" => WorkLifecycle::Open,
        "completed" => WorkLifecycle::Completed,
        "cancelled" => WorkLifecycle::Cancelled,
        "superseded" => WorkLifecycle::Superseded,
        value => {
            return Err(StoreError::InvalidWorkProjection(format!(
                "prerequisite {prerequisite_id:?} has unknown lifecycle {value:?}"
            )));
        }
    };
    let replacement_lifecycle = replacement_lifecycle
        .map(|value| match value {
            "proposed" => Ok(WorkLifecycle::Proposed),
            "open" => Ok(WorkLifecycle::Open),
            "completed" => Ok(WorkLifecycle::Completed),
            "cancelled" => Ok(WorkLifecycle::Cancelled),
            "superseded" => Ok(WorkLifecycle::Superseded),
            value => Err(StoreError::InvalidWorkProjection(format!(
                "prerequisite {prerequisite_id:?} has replacement with unknown lifecycle {value:?}"
            ))),
        })
        .transpose()?;
    classify_prerequisite_state(lifecycle, replacement_lifecycle, prerequisite_id)
}

fn incomplete_prerequisites(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<WorkId>, StoreError> {
    require_work_item_relation_integrity(connection, work_id)?;
    incomplete_prerequisite_projections(connection, work_id)
}

fn parse_work_id(value: &str) -> Result<WorkId, StoreError> {
    uuid::Uuid::parse_str(value).map(WorkId).map_err(|error| {
        StoreError::InvalidWorkProjection(format!("invalid work id {value:?}: {error}"))
    })
}

fn parse_work_run_id(value: &str) -> Result<WorkRunId, StoreError> {
    uuid::Uuid::parse_str(value)
        .map(WorkRunId)
        .map_err(|error| {
            StoreError::InvalidWorkProjection(format!("invalid work run id {value:?}: {error}"))
        })
}

fn feed_parts(feed: &FeedId) -> (&'static str, String) {
    match feed {
        FeedId::Project(project) => ("project", project.0.clone()),
        FeedId::RootWork(root) => ("root_work", root.0.to_string()),
        FeedId::RunExecution(run) => ("run_execution", run.0.to_string()),
    }
}

fn reserve_feed_position(
    transaction: &Transaction<'_>,
    feed: &FeedId,
) -> Result<FeedPosition, StoreError> {
    let (feed_kind, feed_id) = feed_parts(feed);
    let current = transaction
        .query_row(
            "SELECT position FROM work_feed_heads
             WHERE feed_kind = ?1 AND feed_id = ?2",
            params![feed_kind, feed_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?;
    let position = if let Some(current) = current {
        let next = current.checked_add(1).ok_or_else(|| {
            StoreError::InvalidWorkProjection(format!("work feed {feed:?} position overflowed"))
        })?;
        let changed = transaction.execute(
            "UPDATE work_feed_heads SET position = ?3
             WHERE feed_kind = ?1 AND feed_id = ?2 AND position = ?4",
            params![feed_kind, feed_id, next, current],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidWorkProjection(format!(
                "work feed {feed:?} head changed during allocation"
            )));
        }
        next
    } else {
        transaction.execute(
            "INSERT INTO work_feed_heads (feed_kind, feed_id, position)
             VALUES (?1, ?2, 1)",
            params![feed_kind, feed_id],
        )?;
        1
    };
    Ok(FeedPosition {
        feed: feed.clone(),
        position,
    })
}

fn checkpoint_feed_end(position: i64) -> Result<i64, StoreError> {
    position
        .checked_add(CHECKPOINT_APPEND_COUNT)
        .ok_or_else(|| {
            StoreError::InvalidWorkProjection(
                "checkpoint run-feed position arithmetic overflowed".into(),
            )
        })
}

fn insert_reserved_feed_entry(
    transaction: &Transaction<'_>,
    position: &FeedPosition,
    object_kind: &str,
    object: &CanonicalObject,
    work_id: Option<WorkId>,
) -> Result<(), StoreError> {
    let (feed_kind, feed_id) = feed_parts(&position.feed);
    transaction.execute(
        "INSERT INTO work_feed_entries (
             feed_kind, feed_id, position, object_kind, object_hash, work_id
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            feed_kind,
            feed_id,
            position.position,
            object_kind,
            object.hash().as_str(),
            work_id.map(|work_id| work_id.0.to_string())
        ],
    )?;
    Ok(())
}

fn append_to_work_feeds(
    transaction: &Transaction<'_>,
    project_id: &crate::domain::ProjectId,
    root_id: WorkId,
    run_id: Option<WorkRunId>,
    work_id: Option<WorkId>,
    object_kind: &str,
    object: &CanonicalObject,
) -> Result<Vec<FeedPosition>, StoreError> {
    let mut feeds = vec![
        FeedId::Project(project_id.clone()),
        FeedId::RootWork(root_id),
    ];
    if let Some(run_id) = run_id {
        feeds.push(FeedId::RunExecution(run_id));
    }
    feeds
        .into_iter()
        .map(|feed| {
            let position = reserve_feed_position(transaction, &feed)?;
            insert_reserved_feed_entry(transaction, &position, object_kind, object, work_id)?;
            Ok(position)
        })
        .collect()
}

#[allow(
    clippy::too_many_arguments,
    reason = "the exact work, holder, time, actor, typed memory, and canonical objects form one audited capture binding"
)]
pub(super) fn append_memory_capture_to_work_feeds(
    transaction: &Transaction<'_>,
    work_id: WorkId,
    holder: &SessionId,
    captured_at: DateTime<Utc>,
    actor: &crate::domain::ActorContext,
    version: &MemoryVersion,
    assertion: &MemoryAssertionEvent,
    version_object: &CanonicalObject,
    assertion_object: &CanonicalObject,
) -> Result<Vec<FeedPosition>, StoreError> {
    let crate::domain::Scope::Work { project, work } = &version.scope else {
        return Err(StoreError::InvalidMemoryProjection(
            "shared work capture must carry work scope".into(),
        ));
    };
    if *work != work_id
        || assertion.memory_id != version.memory_id
        || assertion.version != *version_object.hash()
        || version.actor != *actor
        || assertion.actor != *actor
        || version.created_at != captured_at
        || assertion.created_at != captured_at
    {
        return Err(StoreError::InvalidMemoryProjection(
            "shared work capture is not bound to its note, actor, and timestamp".into(),
        ));
    }
    assert_actor_session(actor, holder)?;
    let projected_item = load_work_item(transaction, work_id)?;
    if project != &projected_item.project_id {
        return Err(StoreError::InvalidMemoryProjection(
            "shared work capture project differs from the focused work".into(),
        ));
    }
    let run_id = projected_item
        .active_run_id
        .ok_or(StoreError::WorkClaimMismatch { work: work_id })?;
    let projected_claim = load_work_claim_optional(transaction, run_id)?
        .ok_or(StoreError::WorkClaimMismatch { work: work_id })?;
    let (item, run, mut claim) = validate_live_claim_on(
        transaction,
        work_id,
        run_id,
        projected_item.revision,
        holder,
        projected_claim.claim_id,
        projected_claim.fence,
        captured_at,
        false,
    )?;
    renew_holder_claim(transaction, &mut claim, captured_at)?;
    let root_execution = load_root_execution(transaction, run.root_execution_id)?;
    let mut positions = append_to_work_feeds(
        transaction,
        &item.project_id,
        item.root_id,
        item.active_run_id,
        None,
        "memory_version",
        version_object,
    )?;
    positions.extend(append_to_work_feeds(
        transaction,
        &item.project_id,
        item.root_id,
        item.active_run_id,
        None,
        "memory_assertion_event",
        assertion_object,
    )?);
    let event = WorkEventDraft {
        schema_version: SCHEMA_VERSION,
        project_id: item.project_id.clone(),
        root_id: item.root_id,
        work_id: item.work_id,
        run_id: Some(run.run_id),
        revision: item.revision,
        work: item,
        run: Some(run),
        root_execution: Some(root_execution),
        claim: Some(claim),
        handoff_offer: None,
        blocker: None,
        transition: WorkTransition::MemoryCaptured {
            version: version_object.hash().clone(),
            assertion: assertion_object.hash().clone(),
        },
        actor: actor.clone(),
        created_at: captured_at,
    };
    let (_, event_positions) = append_work_event(transaction, &event)?;
    positions.extend(event_positions);
    Ok(positions)
}

pub(super) fn append_context_object_to_work_feeds(
    transaction: &Transaction<'_>,
    work_id: WorkId,
    object_kind: &str,
    object: &CanonicalObject,
) -> Result<Vec<FeedPosition>, StoreError> {
    let item = load_work_item(transaction, work_id)?;
    append_to_work_feeds(
        transaction,
        &item.project_id,
        item.root_id,
        item.active_run_id,
        None,
        object_kind,
        object,
    )
}

pub(super) fn load_control_execution_observation_on(
    connection: &Connection,
    hash: &ObjectHash,
) -> Result<Option<ExecutionObservation>, StoreError> {
    let stored = connection
        .query_row(
            "SELECT object_kind, canonical_json FROM objects WHERE object_hash = ?1",
            [hash.as_str()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?;
    let Some((kind, bytes)) = stored else {
        return Ok(None);
    };
    if kind != "execution_observation" {
        return Ok(None);
    }
    Ok(Some(CanonicalObject::verify(hash, bytes)?.decode()?))
}

pub(super) fn load_control_environment_evidence_on(
    connection: &Connection,
    hash: &ObjectHash,
) -> Result<Option<EnvironmentEvidence>, StoreError> {
    let stored = connection
        .query_row(
            "SELECT object_kind, canonical_json FROM objects WHERE object_hash = ?1",
            [hash.as_str()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?;
    let Some((kind, bytes)) = stored else {
        return Ok(None);
    };
    if kind != "environment_evidence" {
        return Ok(None);
    }
    let evidence = CanonicalObject::verify(hash, bytes)?.decode()?;
    expected_environment_projection(connection, hash)?;
    Ok(Some(evidence))
}

fn applicable_work_obligations_at_cut_on(
    connection: &Connection,
    run_id: WorkRunId,
    cut: &FeedPosition,
) -> Result<Vec<WorkObligationRecord>, StoreError> {
    if cut.feed != FeedId::RunExecution(run_id) {
        return Err(StoreError::InvalidWorkProjection(
            "obligation cut does not name the requested run feed".into(),
        ));
    }
    if cut.position > feed_head(connection, &cut.feed)? {
        return Err(StoreError::InvalidWorkProjection(
            "obligation cut exceeds the current run-feed head".into(),
        ));
    }
    let records = load_work_obligation_records_on(connection, run_id, None)?;
    let mut applicable = Vec::new();
    for record in records {
        if record.obligation.trigger_position.position > cut.position {
            continue;
        }
        let definition_position =
            run_feed_position_for_object_on(connection, run_id, &record.definition_hash)?;
        if definition_position.position > cut.position {
            return Err(StoreError::InvalidWorkProjection(format!(
                "run-feed cut {} splits mutation obligation {} from its trigger",
                cut.position, record.obligation.obligation_id.0
            )));
        }
        applicable.push(record);
    }
    Ok(applicable)
}

fn completion_obligation_basis_on(
    connection: &Connection,
    work_id: WorkId,
    run_id: WorkRunId,
    cut: &FeedPosition,
) -> Result<Vec<CompletionObligationBinding>, StoreError> {
    let records = applicable_work_obligations_at_cut_on(connection, run_id, cut)?;
    let mut open = Vec::new();
    let mut bindings = Vec::new();
    for record in records {
        let terminal_at_cut = record
            .resolution_hash
            .as_ref()
            .map(|hash| run_feed_position_for_object_on(connection, run_id, hash))
            .transpose()?
            .filter(|position| position.position <= cut.position);
        let Some(resolution_position) = terminal_at_cut else {
            open.push(OpenWorkObligation {
                obligation_id: record.obligation.obligation_id,
                definition: record.definition_hash,
                required_check: record.obligation.requirement.check_kind,
            });
            continue;
        };
        if resolution_position.feed != cut.feed {
            return Err(StoreError::InvalidWorkProjection(
                "obligation resolution position names another run feed".into(),
            ));
        }
        bindings.push(CompletionObligationBinding {
            obligation_id: record.obligation.obligation_id,
            definition: record.definition_hash,
            resolution: record.resolution_hash.ok_or_else(|| {
                StoreError::InvalidWorkProjection(
                    "terminal work obligation has no resolution hash".into(),
                )
            })?,
        });
    }
    open.sort_by(|left, right| {
        left.obligation_id
            .0
            .as_bytes()
            .cmp(right.obligation_id.0.as_bytes())
    });
    if !open.is_empty() {
        let omitted_count = open.len().saturating_sub(MAX_OPEN_COMPLETION_OBLIGATIONS);
        open.truncate(MAX_OPEN_COMPLETION_OBLIGATIONS);
        return Err(StoreError::OpenWorkObligations {
            work: work_id,
            obligations: open,
            omitted_count,
        });
    }
    bindings.sort_by(|left, right| {
        left.obligation_id
            .0
            .as_bytes()
            .cmp(right.obligation_id.0.as_bytes())
            .then_with(|| left.definition.as_str().cmp(right.definition.as_str()))
    });
    Ok(bindings)
}

fn completion_environment_basis_on(
    connection: &Connection,
    run_id: WorkRunId,
    cut: &FeedPosition,
) -> Result<Vec<ObjectHash>, StoreError> {
    if cut.feed != FeedId::RunExecution(run_id) {
        return Err(StoreError::InvalidWorkProjection(
            "environment cut does not name the requested run feed".into(),
        ));
    }
    if cut.position > feed_head(connection, &cut.feed)? {
        return Err(StoreError::InvalidWorkProjection(
            "environment cut exceeds the current run-feed head".into(),
        ));
    }
    let mut statement = connection.prepare(
        "SELECT DISTINCT object_hash FROM work_feed_entries
         WHERE feed_kind = 'run_execution' AND feed_id = ?1
           AND position <= ?2 AND object_kind = 'environment_evidence'
         ORDER BY object_hash LIMIT ?3",
    )?;
    let limit = i64::try_from(MAX_COMPLETION_ENVIRONMENT_EVIDENCE + 1).map_err(|_| {
        StoreError::InvalidWorkProjection("completion environment limit does not fit SQLite".into())
    })?;
    let rows = statement
        .query_map(params![run_id.0.to_string(), cut.position, limit], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut environment = Vec::with_capacity(rows.len());
    for stored in rows {
        let hash =
            ObjectHash::from_stored(stored.clone()).ok_or(StoreError::InvalidStoredHash(stored))?;
        expected_environment_projection(connection, &hash)?;
        environment.push(hash);
    }
    Ok(environment)
}

fn validate_completion_seal_environment_basis_on(
    connection: &Connection,
    seal: &CompletionSeal,
) -> Result<(), StoreError> {
    if seal.environment_schema_version != COMPLETION_ENVIRONMENT_SCHEMA_VERSION {
        return Err(StoreError::InvalidWorkProjection(format!(
            "completion seal for run {} has unsupported environment schema {}",
            seal.run_id.0, seal.environment_schema_version
        )));
    }
    let expected = completion_environment_basis_on(connection, seal.run_id, &seal.completion_cut)?;
    if expected.len() > MAX_COMPLETION_ENVIRONMENT_EVIDENCE || seal.environment != expected {
        return Err(StoreError::InvalidWorkProjection(format!(
            "completion seal for run {} does not bind the exact environment cut",
            seal.run_id.0
        )));
    }
    Ok(())
}

fn validate_completion_seal_obligation_basis_on(
    connection: &Connection,
    seal: &CompletionSeal,
) -> Result<(), StoreError> {
    if seal.obligation_schema_version != COMPLETION_OBLIGATION_SCHEMA_VERSION {
        return Err(StoreError::InvalidWorkProjection(format!(
            "completion seal for run {} has unsupported obligation schema {}",
            seal.run_id.0, seal.obligation_schema_version
        )));
    }
    let expected =
        completion_obligation_basis_on(connection, seal.work_id, seal.run_id, &seal.completion_cut)
            .map_err(|error| match error {
                StoreError::OpenWorkObligations { .. } => {
                    StoreError::InvalidWorkProjection(format!(
                        "completion seal for run {} was frozen with open obligations",
                        seal.run_id.0
                    ))
                }
                other => other,
            })?;
    if seal.obligations != expected {
        return Err(StoreError::InvalidWorkProjection(format!(
            "completion seal for run {} does not bind the exact obligation cut",
            seal.run_id.0
        )));
    }
    Ok(())
}

fn load_work_obligation_records_on(
    connection: &Connection,
    run_id: WorkRunId,
    state: Option<WorkObligationState>,
) -> Result<Vec<WorkObligationRecord>, StoreError> {
    let state = state.map(encode_state).transpose()?;
    let mut statement = connection.prepare(
        "SELECT obligation_id, definition_hash, project_id, root_execution_id,
                root_id, work_id, run_id, work_revision, rule_set_hash, rule_id, rule_version,
                triggering_observation_hash, trigger_position, check_kind,
                check_fingerprint, state, resolution_hash, resolution_kind,
                evidence_hash, opened_at_ms, resolved_at_ms
         FROM work_run_obligations
         WHERE run_id = ?1 AND (?2 IS NULL OR state = ?2)
         ORDER BY trigger_position, obligation_id",
    )?;
    let rows = statement
        .query_map(params![run_id.0.to_string(), state], |row| {
            Ok(ObligationProjectionRow {
                obligation_id: row.get(0)?,
                definition_hash: row.get(1)?,
                project_id: row.get(2)?,
                root_execution_id: row.get(3)?,
                root_id: row.get(4)?,
                work_id: row.get(5)?,
                run_id: row.get(6)?,
                work_revision: row.get(7)?,
                rule_set_hash: row.get(8)?,
                rule_id: row.get(9)?,
                rule_version: row.get(10)?,
                triggering_observation_hash: row.get(11)?,
                trigger_position: row.get(12)?,
                check_kind: row.get(13)?,
                check_fingerprint: row.get(14)?,
                state: row.get(15)?,
                resolution_hash: row.get(16)?,
                resolution_kind: row.get(17)?,
                evidence_hash: row.get(18)?,
                opened_at_ms: row.get(19)?,
                resolved_at_ms: row.get(20)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let records = rows
        .into_iter()
        .map(|row| load_work_obligation_record_on(connection, &row))
        .collect::<Result<Vec<_>, _>>()?;
    if state.is_none() {
        require_expected_obligations_on(connection, run_id, &records)?;
    }
    Ok(records)
}

fn require_expected_obligations_on(
    connection: &Connection,
    run_id: WorkRunId,
    records: &[WorkObligationRecord],
) -> Result<(), StoreError> {
    let expected = connection
        .prepare(
            "SELECT entry.position, entry.object_hash, object.canonical_json
             FROM work_feed_entries entry
             JOIN objects object ON object.object_hash = entry.object_hash
             WHERE entry.feed_kind = 'run_execution' AND entry.feed_id = ?1
               AND entry.object_kind = 'execution_observation'
               AND json_extract(object.canonical_json, '$.source_changed') = 1
             ORDER BY entry.position",
        )?
        .query_map([run_id.0.to_string()], |row| {
            Ok((
                row.get::<_, i64>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, Vec<u8>>(2)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (position, stored_hash, bytes) in expected {
        let hash = ObjectHash::from_stored(stored_hash.clone())
            .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
        let observation: ExecutionObservation = CanonicalObject::verify(&hash, bytes)?.decode()?;
        let rule_set = obligation_rule_set_for_observation_on(connection, &observation)?;
        for (rule, requirement) in
            crate::control::evaluate_obligation_rules(&rule_set, &observation)
        {
            let matches = records
                .iter()
                .filter(|record| {
                    record.obligation.run_id == run_id
                        && record.obligation.triggering_observation == hash
                        && record.obligation.trigger_position.position == position
                        && record.obligation.rule_set == observation.obligation_rule_set
                        && record.obligation.rule == rule
                        && record.obligation.requirement == requirement
                })
                .count();
            if matches != 1 {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "run {run_id:?} source mutation {hash} has {matches} matching builtin obligation definitions"
                )));
            }
        }
    }
    Ok(())
}

fn load_work_obligation_by_id_on(
    connection: &Connection,
    obligation_id: WorkObligationId,
) -> Result<WorkObligationRecord, StoreError> {
    let run_id = connection
        .query_row(
            "SELECT run_id FROM work_run_obligations WHERE obligation_id = ?1",
            [obligation_id.0.to_string()],
            |row| row.get::<_, String>(0),
        )
        .optional()?
        .ok_or_else(|| {
            StoreError::InvalidWork(format!(
                "work obligation {} does not exist",
                obligation_id.0
            ))
        })?;
    let run_id = parse_work_run_id(&run_id)?;
    load_work_obligation_records_on(connection, run_id, None)?
        .into_iter()
        .find(|record| record.obligation.obligation_id == obligation_id)
        .ok_or_else(|| {
            StoreError::InvalidWorkProjection(format!(
                "obligation {} disappeared during its verified load",
                obligation_id.0
            ))
        })
}

fn load_work_obligation_record_on(
    connection: &Connection,
    row: &ObligationProjectionRow,
) -> Result<WorkObligationRecord, StoreError> {
    let definition_hash = ObjectHash::from_stored(row.definition_hash.clone())
        .ok_or(StoreError::InvalidStoredHash(row.definition_hash.clone()))?;
    let obligation =
        load_typed_work_object::<WorkObligation>(connection, &definition_hash, "work_obligation")?;
    let state: WorkObligationState =
        serde_json::from_value(serde_json::Value::String(row.state.clone()))?;
    let check_kind: crate::domain::VerificationKind =
        serde_json::from_value(serde_json::Value::String(row.check_kind.clone()))?;
    let check_fingerprint = row
        .check_fingerprint
        .as_ref()
        .map(|value| {
            ObjectHash::from_stored(value.clone())
                .ok_or_else(|| StoreError::InvalidStoredHash(value.clone()))
        })
        .transpose()?;
    let expected_rule_set = ObjectHash::from_stored(row.rule_set_hash.clone())
        .ok_or_else(|| StoreError::InvalidStoredHash(row.rule_set_hash.clone()))?;
    let expected_trigger = ObjectHash::from_stored(row.triggering_observation_hash.clone()).ok_or(
        StoreError::InvalidStoredHash(row.triggering_observation_hash.clone()),
    )?;
    let scalar_matches = obligation.obligation_id.0.to_string() == row.obligation_id
        && obligation.project_id.0 == row.project_id
        && obligation.root_execution_id.0.to_string() == row.root_execution_id
        && obligation.root_id.0.to_string() == row.root_id
        && obligation.work_id.0.to_string() == row.work_id
        && obligation.run_id.0.to_string() == row.run_id
        && obligation.work_revision == row.work_revision
        && obligation.rule_set == expected_rule_set
        && obligation.rule.rule_id == row.rule_id
        && i64::from(obligation.rule.rule_version) == row.rule_version
        && obligation.triggering_observation == expected_trigger
        && obligation.trigger_position
            == (FeedPosition {
                feed: FeedId::RunExecution(obligation.run_id),
                position: row.trigger_position,
            })
        && obligation.requirement.check_kind == check_kind
        && obligation.requirement.check_fingerprint == check_fingerprint
        && obligation.opened_at.timestamp_millis() == row.opened_at_ms;
    if !scalar_matches {
        return Err(StoreError::InvalidWorkProjection(format!(
            "obligation {} does not match its redundant projection",
            row.obligation_id
        )));
    }
    let trigger = load_typed_work_object::<ExecutionObservation>(
        connection,
        &obligation.triggering_observation,
        "execution_observation",
    )?;
    let trigger_entry_matches = connection.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM work_feed_entries
             WHERE feed_kind = 'run_execution' AND feed_id = ?1
               AND position = ?2 AND object_kind = 'execution_observation'
               AND object_hash = ?3
         )",
        params![
            obligation.run_id.0.to_string(),
            obligation.trigger_position.position,
            obligation.triggering_observation.as_str()
        ],
        |query| query.get::<_, bool>(0),
    )?;
    let definition_position: Option<i64> = connection
        .query_row(
            "SELECT position FROM work_feed_entries
             WHERE feed_kind = 'run_execution' AND feed_id = ?1
               AND object_kind = 'work_obligation' AND object_hash = ?2",
            params![obligation.run_id.0.to_string(), definition_hash.as_str()],
            |query| query.get(0),
        )
        .optional()?;
    if !trigger.source_changed
        || trigger.project_id != obligation.project_id
        || trigger.binding.root_execution_id != obligation.root_execution_id
        || trigger.binding.work_id != obligation.work_id
        || trigger.binding.run_id != obligation.run_id
        || trigger.binding.work_revision != obligation.work_revision
        || trigger.recorded_at != obligation.opened_at
        || !trigger_entry_matches
        || definition_position
            .is_none_or(|position| position <= obligation.trigger_position.position)
    {
        return Err(StoreError::InvalidWorkProjection(format!(
            "obligation {} has an invalid trigger or feed binding",
            obligation.obligation_id.0
        )));
    }
    let resolution_hash = row
        .resolution_hash
        .as_ref()
        .map(|value| {
            ObjectHash::from_stored(value.clone())
                .ok_or_else(|| StoreError::InvalidStoredHash(value.clone()))
        })
        .transpose()?;
    let resolution = resolution_hash
        .as_ref()
        .map(|hash| {
            load_typed_work_object::<WorkObligationResolutionEvent>(
                connection,
                hash,
                "work_obligation_resolution",
            )
        })
        .transpose()?;
    let resolution_position = validate_obligation_resolution_projection(
        connection,
        &definition_hash,
        &obligation,
        state,
        resolution_hash.as_ref(),
        resolution.as_ref(),
        row.resolution_kind.as_deref(),
        row.evidence_hash.as_deref(),
        row.resolved_at_ms,
    )?;
    Ok(WorkObligationRecord {
        definition_hash,
        obligation,
        state,
        resolution_hash,
        resolution,
        resolution_position,
    })
}

#[allow(
    clippy::too_many_arguments,
    reason = "obligation resolution validation keeps every redundant binding explicit"
)]
fn validate_obligation_resolution_projection(
    connection: &Connection,
    definition_hash: &ObjectHash,
    obligation: &WorkObligation,
    state: WorkObligationState,
    resolution_hash: Option<&ObjectHash>,
    event: Option<&WorkObligationResolutionEvent>,
    projected_kind: Option<&str>,
    projected_evidence: Option<&str>,
    resolved_at_ms: Option<i64>,
) -> Result<Option<FeedPosition>, StoreError> {
    if state == WorkObligationState::Open {
        if resolution_hash.is_some()
            || event.is_some()
            || projected_kind.is_some()
            || projected_evidence.is_some()
            || resolved_at_ms.is_some()
        {
            return Err(StoreError::InvalidWorkProjection(format!(
                "open obligation {} carries terminal projection data",
                obligation.obligation_id.0
            )));
        }
        return Ok(None);
    }
    let (resolution_hash, event, resolved_at_ms) = resolution_hash
        .zip(event)
        .zip(resolved_at_ms)
        .map(|((hash, event), at)| (hash, event, at))
        .ok_or_else(|| {
            StoreError::InvalidWorkProjection(format!(
                "terminal obligation {} has incomplete resolution data",
                obligation.obligation_id.0
            ))
        })?;
    if event.project_id != obligation.project_id
        || event.obligation_id != obligation.obligation_id
        || event.definition != *definition_hash
        || event.run_id != obligation.run_id
        || event.created_at.timestamp_millis() != resolved_at_ms
    {
        return Err(StoreError::InvalidWorkProjection(format!(
            "obligation resolution {resolution_hash} crosses its definition binding"
        )));
    }
    let resolution_position =
        run_feed_position_for_object_on(connection, obligation.run_id, resolution_hash)?;
    match &event.resolution {
        WorkObligationResolution::Satisfied {
            evidence,
            evaluated_cut,
        } => {
            if state != WorkObligationState::Satisfied
                || projected_kind != Some("satisfied")
                || projected_evidence != Some(evidence.as_str())
                || evaluated_cut.feed != FeedId::RunExecution(obligation.run_id)
                || evaluated_cut.position >= resolution_position.position
            {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "satisfied obligation {} has inconsistent terminal bindings",
                    obligation.obligation_id.0
                )));
            }
            let verification = load_typed_work_object::<VerificationEvidence>(
                connection,
                evidence,
                "verification_evidence",
            )?;
            let producer = load_typed_work_object::<ExecutionObservation>(
                connection,
                &verification.producer_observation,
                "execution_observation",
            )?;
            let evidence_position =
                run_feed_position_for_object_on(connection, obligation.run_id, evidence)?;
            let (mutation_position, latest_mutation) =
                latest_source_mutation_on(connection, obligation.run_id, evaluated_cut.position)?
                    .ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "satisfied obligation has no source mutation at its evaluated cut".into(),
                    )
                })?;
            let satisfied = crate::control::evaluate_obligation_satisfaction(
                &crate::control::ObligationSatisfactionInput {
                    open_obligations: std::slice::from_ref(obligation),
                    evidence: &verification,
                    producer: &producer,
                    latest_mutation: &latest_mutation,
                    evidence_position: evidence_position.position,
                    latest_mutation_position: mutation_position,
                    evaluated_cut,
                },
            );
            if satisfied != [obligation.obligation_id] {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "satisfied obligation {} does not match its verification evidence",
                    obligation.obligation_id.0
                )));
            }
        }
        WorkObligationResolution::Waived {
            authority_grant,
            waived_by,
            reason,
        } => {
            if state != WorkObligationState::Waived
                || projected_kind != Some("waived")
                || projected_evidence.is_some()
                || waived_by.trim().is_empty()
                || waived_by.trim() != waived_by
                || reason.trim().is_empty()
                || reason.trim() != reason
            {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "waived obligation {} has inconsistent terminal bindings",
                    obligation.obligation_id.0
                )));
            }
            validate_obligation_waiver_authority(
                connection,
                obligation,
                authority_grant,
                waived_by,
                event.actor.assurance,
                event.created_at,
            )?;
        }
    }
    Ok(Some(resolution_position))
}

fn validate_obligation_waiver_authority(
    connection: &Connection,
    obligation: &WorkObligation,
    grant_hash: &ObjectHash,
    waived_by: &str,
    actor_assurance: crate::domain::AssuranceLevel,
    at: DateTime<Utc>,
) -> Result<(), StoreError> {
    let grant = load_typed_work_object::<WorkAuthorityGrant>(
        connection,
        grant_hash,
        "work_authority_grant",
    )?;
    let revoked_at_ms: Option<i64> = connection.query_row(
        "SELECT revoked_at_ms FROM work_authority_grants WHERE grant_hash = ?1",
        [grant_hash.as_str()],
        |row| row.get(0),
    )?;
    require_authority_revocation_integrity(connection, grant_hash, revoked_at_ms)?;
    let target = AuthorityTarget {
        project_id: &obligation.project_id,
        policy_ref: &load_work_item(connection, obligation.work_id)?.authority_policy_ref,
        work_id: Some(obligation.work_id),
        root_id: Some(obligation.root_id),
        run_id: Some(obligation.run_id),
    };
    let valid = grant.project_id == obligation.project_id
        && grant.subject_actor_id == waived_by
        && assurance_covers(actor_assurance, grant.assurance)
        && grant
            .operations
            .contains(&WorkAuthorityOperation::ObligationWaiver)
        && authority_scope_matches(&grant.scope, target)
        && grant.issued_at <= at
        && grant.valid_until > at
        && revoked_at_ms.is_none_or(|revoked| revoked > at.timestamp_millis());
    if !valid {
        return Err(StoreError::InvalidWorkProjection(format!(
            "obligation {} waiver authority is invalid",
            obligation.obligation_id.0
        )));
    }
    Ok(())
}

struct TypedEvidenceProjection<'a> {
    kind: WorkEvidenceKind,
    workspace_id: &'a str,
    source_revision: &'a str,
    producer_session_id: &'a SessionId,
    producer_observation: Option<&'a ObjectHash>,
    check_fingerprint: Option<&'a ObjectHash>,
    verification_result: Option<String>,
    observed_at: DateTime<Utc>,
    environment_fingerprint: Option<&'a ObjectHash>,
    environment_evidence: Option<&'a ObjectHash>,
    components_json: Option<Vec<u8>>,
}

pub(super) fn append_control_verification_evidence_on(
    transaction: &Transaction<'_>,
    evidence: &VerificationEvidence,
) -> Result<ObjectHash, StoreError> {
    let object = CanonicalObject::freeze(evidence)?;
    let result = encode_state(evidence.result)?;
    let evidence_hash = append_control_typed_evidence_on(
        transaction,
        &evidence.project_id,
        &evidence.binding,
        &evidence.session_id,
        &evidence.actor,
        evidence.recorded_at,
        &object,
        &TypedEvidenceProjection {
            kind: WorkEvidenceKind::Verification,
            workspace_id: &evidence.source_basis.workspace_id,
            source_revision: &evidence.source_basis.source_revision,
            producer_session_id: &evidence.session_id,
            producer_observation: Some(&evidence.producer_observation),
            check_fingerprint: Some(&evidence.check_fingerprint),
            verification_result: Some(result),
            observed_at: evidence.completed_at,
            environment_fingerprint: None,
            environment_evidence: evidence.environment.as_ref(),
            components_json: None,
        },
    )?;
    satisfy_open_obligations_on(transaction, evidence, &evidence_hash)?;
    Ok(evidence_hash)
}

pub(super) fn append_control_environment_evidence_on(
    transaction: &Transaction<'_>,
    evidence: &EnvironmentEvidence,
) -> Result<ObjectHash, StoreError> {
    let object = CanonicalObject::freeze(evidence)?;
    append_control_typed_evidence_on(
        transaction,
        &evidence.project_id,
        &evidence.binding,
        &evidence.session_id,
        &evidence.actor,
        evidence.recorded_at,
        &object,
        &TypedEvidenceProjection {
            kind: WorkEvidenceKind::Environment,
            workspace_id: &evidence.source_basis.workspace_id,
            source_revision: &evidence.source_basis.source_revision,
            producer_session_id: &evidence.session_id,
            producer_observation: None,
            check_fingerprint: None,
            verification_result: None,
            observed_at: evidence.observed_at,
            environment_fingerprint: Some(&evidence.environment_fingerprint),
            environment_evidence: None,
            components_json: evidence
                .components
                .as_ref()
                .map(serde_json::to_vec)
                .transpose()?,
        },
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "typed evidence persistence keeps every redundant binding explicit"
)]
fn append_control_typed_evidence_on(
    transaction: &Transaction<'_>,
    project_id: &crate::domain::ProjectId,
    binding: &ControlWorkBinding,
    session_id: &SessionId,
    actor: &crate::domain::ActorContext,
    recorded_at: DateTime<Utc>,
    object: &CanonicalObject,
    projection: &TypedEvidenceProjection<'_>,
) -> Result<ObjectHash, StoreError> {
    let item = load_work_item(transaction, binding.work_id)?;
    let run = load_work_run(transaction, binding.run_id)?;
    let mut root_execution = load_root_execution(transaction, binding.root_execution_id)?;
    if &item.project_id != project_id
        || item.root_id != root_execution.root_id
        || run.work_id != item.work_id
        || run.root_execution_id != root_execution.root_execution_id
        || binding.root_execution_id != run.root_execution_id
    {
        return Err(StoreError::InvalidWorkProjection(
            "typed evidence binding does not match canonical work state".into(),
        ));
    }
    let object_kind = match projection.kind {
        WorkEvidenceKind::Generic => {
            return Err(StoreError::InvalidWorkProjection(
                "generic evidence cannot use the typed evidence writer".into(),
            ));
        }
        WorkEvidenceKind::Verification => "verification_evidence",
        WorkEvidenceKind::Environment => "environment_evidence",
    };
    SqliteStore::insert_object(transaction, object_kind, object)?;
    transaction.execute(
        "INSERT INTO work_run_evidence (
             evidence_hash, work_id, run_id, evidence_kind,
             workspace_id, source_revision, producer_session_id,
             producer_observation_hash, check_fingerprint,
             verification_result, observed_at_ms, environment_fingerprint,
             environment_evidence_hash, components_json
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14)",
        params![
            object.hash().as_str(),
            item.work_id.0.to_string(),
            run.run_id.0.to_string(),
            encode_state(projection.kind)?,
            projection.workspace_id,
            projection.source_revision,
            projection.producer_session_id.0,
            projection.producer_observation.map(ObjectHash::as_str),
            projection.check_fingerprint.map(ObjectHash::as_str),
            projection.verification_result.as_deref(),
            projection.observed_at.timestamp_millis(),
            projection.environment_fingerprint.map(ObjectHash::as_str),
            projection.environment_evidence.map(ObjectHash::as_str),
            projection.components_json.as_deref(),
        ],
    )?;
    append_to_work_feeds(
        transaction,
        &item.project_id,
        item.root_id,
        Some(run.run_id),
        None,
        object_kind,
        object,
    )?;
    let root_changed = expect_root_contributor(&mut root_execution, session_id)
        | add_root_contribution(&mut root_execution, session_id, object.hash());
    if root_changed {
        root_execution.revision += 1;
        root_execution.updated_at = recorded_at;
        persist_root_execution(transaction, &root_execution)?;
    }
    let claim = load_work_claim_optional(transaction, run.run_id)?;
    let event = WorkEventDraft {
        schema_version: SCHEMA_VERSION,
        project_id: item.project_id.clone(),
        root_id: item.root_id,
        work_id: item.work_id,
        run_id: Some(run.run_id),
        revision: item.revision,
        work: item,
        run: Some(run),
        root_execution: Some(root_execution),
        claim,
        handoff_offer: None,
        blocker: None,
        transition: WorkTransition::TypedEvidenceAdded {
            evidence: object.hash().clone(),
            evidence_kind: projection.kind,
        },
        actor: actor.clone(),
        created_at: recorded_at,
    };
    append_work_event(transaction, &event)?;
    Ok(object.hash().clone())
}

pub(super) fn append_control_execution_observation_on(
    transaction: &Transaction<'_>,
    observation: &ExecutionObservation,
) -> Result<ObjectHash, StoreError> {
    let item = load_work_item(transaction, observation.binding.work_id)?;
    let run = load_work_run(transaction, observation.binding.run_id)?;
    let root_execution = load_root_execution(transaction, observation.binding.root_execution_id)?;
    if item.project_id != observation.project_id
        || item.root_id != root_execution.root_id
        || run.work_id != item.work_id
        || run.root_execution_id != root_execution.root_execution_id
        || observation.binding.root_execution_id != run.root_execution_id
    {
        return Err(StoreError::InvalidWorkProjection(
            "execution observation binding does not match canonical work state".into(),
        ));
    }
    let object = CanonicalObject::freeze(observation)?;
    SqliteStore::insert_object(transaction, "execution_observation", &object)?;
    let positions = append_to_work_feeds(
        transaction,
        &item.project_id,
        item.root_id,
        Some(run.run_id),
        None,
        "execution_observation",
        &object,
    )?;
    let trigger_position = positions
        .iter()
        .find(|position| position.feed == FeedId::RunExecution(run.run_id))
        .cloned()
        .ok_or_else(|| {
            StoreError::InvalidWorkProjection(
                "execution observation did not receive a run-feed position".into(),
            )
        })?;
    append_builtin_obligations_on(transaction, observation, object.hash(), &trigger_position)?;
    Ok(object.hash().clone())
}

fn obligation_rule_set_for_observation_on(
    connection: &Connection,
    observation: &ExecutionObservation,
) -> Result<crate::domain::ObligationRuleSet, StoreError> {
    SqliteStore::load_obligation_rule_set_on(connection, &observation.obligation_rule_set)
}

fn append_builtin_obligations_on(
    transaction: &Transaction<'_>,
    observation: &ExecutionObservation,
    observation_hash: &ObjectHash,
    trigger_position: &FeedPosition,
) -> Result<Vec<ObjectHash>, StoreError> {
    let item = load_work_item(transaction, observation.binding.work_id)?;
    let mut definitions = Vec::new();
    let rule_set = obligation_rule_set_for_observation_on(transaction, observation)?;
    for (rule, requirement) in crate::control::evaluate_obligation_rules(&rule_set, observation) {
        let obligation = WorkObligation {
            schema_version: SCHEMA_VERSION,
            obligation_id: WorkObligationId::new(),
            project_id: item.project_id.clone(),
            root_execution_id: observation.binding.root_execution_id,
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: observation.binding.run_id,
            work_revision: observation.binding.work_revision,
            rule_set: observation.obligation_rule_set.clone(),
            rule,
            triggering_observation: observation_hash.clone(),
            trigger_position: trigger_position.clone(),
            requirement,
            opened_at: observation.recorded_at,
        };
        let object = CanonicalObject::freeze(&obligation)?;
        SqliteStore::insert_object(transaction, "work_obligation", &object)?;
        append_to_work_feeds(
            transaction,
            &obligation.project_id,
            obligation.root_id,
            Some(obligation.run_id),
            None,
            "work_obligation",
            &object,
        )?;
        transaction.execute(
            "INSERT INTO work_run_obligations (
                 obligation_id, definition_hash, project_id, root_execution_id,
                 root_id, work_id, run_id, work_revision, rule_set_hash, rule_id, rule_version,
                 triggering_observation_hash, trigger_position, check_kind,
                 check_fingerprint, state, opened_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13, ?14, ?15, ?16, ?17)",
            params![
                obligation.obligation_id.0.to_string(),
                object.hash().as_str(),
                obligation.project_id.0,
                obligation.root_execution_id.0.to_string(),
                obligation.root_id.0.to_string(),
                obligation.work_id.0.to_string(),
                obligation.run_id.0.to_string(),
                obligation.work_revision,
                obligation.rule_set.as_str(),
                obligation.rule.rule_id,
                obligation.rule.rule_version,
                obligation.triggering_observation.as_str(),
                obligation.trigger_position.position,
                encode_state(obligation.requirement.check_kind)?,
                obligation
                    .requirement
                    .check_fingerprint
                    .as_ref()
                    .map(ObjectHash::as_str),
                encode_state(WorkObligationState::Open)?,
                obligation.opened_at.timestamp_millis(),
            ],
        )?;
        definitions.push(object.hash().clone());
    }
    Ok(definitions)
}

fn satisfy_open_obligations_on(
    transaction: &Transaction<'_>,
    evidence: &VerificationEvidence,
    evidence_hash: &ObjectHash,
) -> Result<Vec<ObjectHash>, StoreError> {
    let evidence_position =
        run_feed_position_for_object_on(transaction, evidence.binding.run_id, evidence_hash)?;
    let evaluated_cut = current_run_feed_cut_on(transaction, evidence.binding.run_id)?;
    let Some((latest_mutation_position, latest_mutation)) =
        latest_source_mutation_on(transaction, evidence.binding.run_id, evaluated_cut.position)?
    else {
        return Ok(Vec::new());
    };
    let producer = load_typed_work_object::<ExecutionObservation>(
        transaction,
        &evidence.producer_observation,
        "execution_observation",
    )?;
    let records = load_work_obligation_records_on(transaction, evidence.binding.run_id, None)?
        .into_iter()
        .filter(|record| record.state == WorkObligationState::Open)
        .collect::<Vec<_>>();
    let obligations = records
        .iter()
        .map(|record| record.obligation.clone())
        .collect::<Vec<_>>();
    let satisfied = crate::control::evaluate_obligation_satisfaction(
        &crate::control::ObligationSatisfactionInput {
            open_obligations: &obligations,
            evidence,
            producer: &producer,
            latest_mutation: &latest_mutation,
            evidence_position: evidence_position.position,
            latest_mutation_position,
            evaluated_cut: &evaluated_cut,
        },
    );
    let by_id = records
        .into_iter()
        .map(|record| (record.obligation.obligation_id, record))
        .collect::<HashMap<_, _>>();
    let mut resolution_hashes = Vec::new();
    for obligation_id in satisfied {
        let record = by_id.get(&obligation_id).ok_or_else(|| {
            StoreError::InvalidWorkProjection(
                "pure obligation evaluation returned an unknown definition".into(),
            )
        })?;
        let event = WorkObligationResolutionEvent {
            schema_version: SCHEMA_VERSION,
            project_id: evidence.project_id.clone(),
            obligation_id,
            definition: record.definition_hash.clone(),
            run_id: evidence.binding.run_id,
            resolution: WorkObligationResolution::Satisfied {
                evidence: evidence_hash.clone(),
                evaluated_cut: evaluated_cut.clone(),
            },
            actor: evidence.actor.clone(),
            created_at: evidence.recorded_at,
        };
        let object = append_obligation_resolution_on(transaction, record, &event)?;
        resolution_hashes.push(object);
    }
    Ok(resolution_hashes)
}

fn append_obligation_resolution_on(
    transaction: &Transaction<'_>,
    record: &WorkObligationRecord,
    event: &WorkObligationResolutionEvent,
) -> Result<ObjectHash, StoreError> {
    let (state, kind, evidence_hash) = match &event.resolution {
        WorkObligationResolution::Satisfied { evidence, .. } => (
            WorkObligationState::Satisfied,
            "satisfied",
            Some(evidence.as_str()),
        ),
        WorkObligationResolution::Waived { .. } => (WorkObligationState::Waived, "waived", None),
    };
    let object = CanonicalObject::freeze(event)?;
    SqliteStore::insert_object(transaction, "work_obligation_resolution", &object)?;
    append_to_work_feeds(
        transaction,
        &record.obligation.project_id,
        record.obligation.root_id,
        Some(record.obligation.run_id),
        None,
        "work_obligation_resolution",
        &object,
    )?;
    let changed = transaction.execute(
        "UPDATE work_run_obligations SET
             state = ?3, resolution_hash = ?4, resolution_kind = ?5,
             evidence_hash = ?6, resolved_at_ms = ?7
         WHERE obligation_id = ?1 AND definition_hash = ?2
           AND state = 'open' AND resolution_hash IS NULL",
        params![
            event.obligation_id.0.to_string(),
            record.definition_hash.as_str(),
            encode_state(state)?,
            object.hash().as_str(),
            kind,
            evidence_hash,
            event.created_at.timestamp_millis(),
        ],
    )?;
    if changed != 1 {
        return Err(StoreError::InvalidWorkProjection(format!(
            "obligation {} lost its open-state compare-and-swap",
            event.obligation_id.0
        )));
    }
    Ok(object.hash().clone())
}

/// Every work-anchored memory version and contradiction must sit in the
/// project feed and its root-work feed; a missing entry would let peers miss a
/// contested or new rule without any cursor gap.
fn verify_anchored_memory_feeds(
    connection: &Connection,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let feed_has = |object_hash: &ObjectHash, feed_kind: &str, feed_id: &str| {
        connection
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM work_feed_entries
                     WHERE feed_kind = ?1 AND feed_id = ?2 AND object_hash = ?3
                 )",
                params![feed_kind, feed_id, object_hash.as_str()],
                |row| row.get::<_, bool>(0),
            )
            .map_err(StoreError::from)
    };
    let mut statement = connection.prepare(
        "SELECT object_hash, object_kind, canonical_json FROM objects
         WHERE object_kind IN ('memory_contradiction_event', 'memory_version')
         ORDER BY object_hash",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, Vec<u8>>(2)?,
        ))
    })?;
    for row in rows {
        let (stored_hash, object_kind, bytes) = row?;
        let label = format!("{object_kind}:{stored_hash}:work-feed");
        let Some(hash) = ObjectHash::from_stored(stored_hash) else {
            invalid.push(label);
            continue;
        };
        let Ok(object) = CanonicalObject::verify(&hash, bytes) else {
            invalid.push(label);
            continue;
        };
        // Objects retained under another schema are never activated or fed;
        // only the current schema carries feed expectations.
        let current_schema = serde_json::from_slice::<serde_json::Value>(object.bytes())
            .ok()
            .and_then(|value| {
                value
                    .get("schema_version")
                    .and_then(serde_json::Value::as_u64)
            })
            == Some(u64::from(SCHEMA_VERSION));
        if !current_schema {
            continue;
        }
        let anchor = if object_kind == "memory_contradiction_event" {
            object
                .decode::<crate::domain::MemoryContradictionEvent>()
                .ok()
                .and_then(|event| {
                    event
                        .work_root_id
                        .map(|root_id| (event.project_id, root_id))
                })
        } else if let Ok(crate::domain::MemoryVersion {
            scope: crate::domain::Scope::Work { project, work },
            ..
        }) = object.decode::<crate::domain::MemoryVersion>()
        {
            let Ok((_, root)) = verified_work_identity(connection, work) else {
                invalid.push(label);
                continue;
            };
            Some((project, root))
        } else {
            None
        };
        let Some((project_id, root_id)) = anchor else {
            continue;
        };
        *checked += 1;
        if !feed_has(&hash, "project", &project_id.0)?
            || !feed_has(&hash, "root_work", &root_id.0.to_string())?
        {
            invalid.push(label);
        }
    }
    Ok(())
}

fn run_feed_position_for_object_on(
    connection: &Connection,
    run_id: WorkRunId,
    object_hash: &ObjectHash,
) -> Result<FeedPosition, StoreError> {
    let position = connection
        .query_row(
            "SELECT position FROM work_feed_entries
             WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND object_hash = ?2",
            params![run_id.0.to_string(), object_hash.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .ok_or_else(|| {
            StoreError::InvalidWorkProjection(format!(
                "object {object_hash} is missing from run {run_id:?} feed"
            ))
        })?;
    Ok(FeedPosition {
        feed: FeedId::RunExecution(run_id),
        position,
    })
}

fn current_run_feed_cut_on(
    connection: &Connection,
    run_id: WorkRunId,
) -> Result<FeedPosition, StoreError> {
    let position = connection.query_row(
        "SELECT position FROM work_feed_heads
         WHERE feed_kind = 'run_execution' AND feed_id = ?1",
        [run_id.0.to_string()],
        |row| row.get::<_, i64>(0),
    )?;
    Ok(FeedPosition {
        feed: FeedId::RunExecution(run_id),
        position,
    })
}

fn latest_source_mutation_on(
    connection: &Connection,
    run_id: WorkRunId,
    through: i64,
) -> Result<Option<(i64, ExecutionObservation)>, StoreError> {
    let stored = connection
        .query_row(
            "SELECT entry.position, entry.object_hash, object.canonical_json
             FROM work_feed_entries entry
             JOIN objects object ON object.object_hash = entry.object_hash
             WHERE entry.feed_kind = 'run_execution' AND entry.feed_id = ?1
               AND entry.position <= ?2
               AND entry.object_kind = 'execution_observation'
               AND json_extract(object.canonical_json, '$.source_changed') = 1
             ORDER BY entry.position DESC LIMIT 1",
            params![run_id.0.to_string(), through],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            },
        )
        .optional()?;
    stored
        .map(|(position, stored_hash, bytes)| {
            let hash = ObjectHash::from_stored(stored_hash.clone())
                .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
            let observation = CanonicalObject::verify(&hash, bytes)?.decode()?;
            Ok((position, observation))
        })
        .transpose()
}

fn append_work_event(
    transaction: &Transaction<'_>,
    event: &WorkEventDraft,
) -> Result<(ObjectHash, Vec<FeedPosition>), StoreError> {
    let mut relation_basis =
        if latest_canonical_work_event_for_item_optional(transaction, event.work_id)?.is_some() {
            validated_current_work_relation_basis(transaction, event.work_id)?
        } else {
            projected_work_relation_basis(transaction, event.work_id)?
        };
    apply_work_relation_transition(
        &mut relation_basis,
        &event.transition,
        event.blocker.as_ref(),
    )?;
    let event = event
        .clone()
        .finalize(work_relation_fingerprint(&relation_basis)?);
    let object = CanonicalObject::freeze(&event)?;
    SqliteStore::insert_object(transaction, "work_event", &object)?;
    let positions = append_to_work_feeds(
        transaction,
        &event.project_id,
        event.root_id,
        event.run_id,
        Some(event.work_id),
        "work_event",
        &object,
    )?;
    let changed = transaction.execute(
        "UPDATE work_items SET latest_event_hash = ?2 WHERE work_id = ?1",
        params![event.work_id.0.to_string(), object.hash().as_str()],
    )?;
    if changed != 1 {
        return Err(StoreError::InvalidWorkProjection(format!(
            "work event append lost item {:?}",
            event.work_id
        )));
    }
    Ok((object.hash().clone(), positions))
}

fn request_object<T: Serialize>(request: &T) -> Result<CanonicalObject, StoreError> {
    CanonicalObject::freeze(request)
}

fn load_typed_work_object<T: DeserializeOwned>(
    connection: &Connection,
    hash: &ObjectHash,
    object_kind: &str,
) -> Result<T, StoreError> {
    let stored: Option<(String, Vec<u8>)> = connection
        .query_row(
            "SELECT object_kind, canonical_json FROM objects WHERE object_hash = ?1",
            [hash.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (stored_kind, bytes) = stored
        .ok_or_else(|| StoreError::InvalidWorkProjection(format!("object {hash} is missing")))?;
    if stored_kind != object_kind {
        return Err(StoreError::ObjectKindMismatch {
            hash: hash.clone(),
            stored: stored_kind,
            requested: object_kind.into(),
        });
    }
    CanonicalObject::verify(hash, bytes)?.decode()
}

fn load_handoff_offer_projection(
    connection: &Connection,
    row: (Option<String>, Vec<u8>),
) -> Result<WorkHandoffOffer, StoreError> {
    let (stored_hash, projection_bytes) = row;
    let stored_hash = stored_hash
        .and_then(ObjectHash::from_stored)
        .ok_or_else(|| {
            StoreError::InvalidWorkProjection(
                "handoff offer projection has no valid canonical hash".into(),
            )
        })?;
    let canonical =
        load_typed_work_object::<WorkHandoffOffer>(connection, &stored_hash, "work_handoff_offer")?;
    let projection: WorkHandoffOffer = serde_json::from_slice(&projection_bytes)?;
    if projection != canonical {
        return Err(StoreError::InvalidWorkProjection(format!(
            "handoff offer {} differs from canonical object {stored_hash}",
            projection.offer_id.0
        )));
    }
    if latest_canonical_handoff_offer(
        connection,
        &projection.offer_id.0.to_string(),
        &projection.work_id.0.to_string(),
    )?
    .as_ref()
        != Some(&canonical)
    {
        return Err(StoreError::InvalidWorkProjection(format!(
            "handoff offer {} differs from the latest canonical work event",
            projection.offer_id.0
        )));
    }
    Ok(canonical)
}

fn compact_work_protocol_result(
    operation: &str,
    mut result: serde_json::Value,
) -> Result<serde_json::Value, StoreError> {
    let object = result.as_object_mut().ok_or_else(|| {
        StoreError::InvalidWorkProjection("work-protocol result must be a JSON object".into())
    })?;
    if operation == "work_update:waive_required_child"
        && let Some(receipt) = object
            .get_mut("receipt")
            .and_then(serde_json::Value::as_object_mut)
    {
        receipt.remove("authority_grant");
        if let Some(result) = receipt
            .get_mut("result")
            .and_then(serde_json::Value::as_object_mut)
        {
            result.remove("authority_grant");
        }
    }
    Ok(result)
}

fn validate_work_protocol_result_binding(
    connection: &Connection,
    project_id: &str,
    operation: &str,
    result: &serde_json::Value,
) -> Result<(), StoreError> {
    let mut bound_items = Vec::new();
    match operation {
        "work_propose:root" => {
            let work_id = result
                .pointer("/work/work_id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "root proposal replay has no work identity".into(),
                    )
                })?;
            bound_items.push(load_work_item(connection, parse_work_id(work_id)?)?);
        }
        "work_propose:decompose" => {
            let parent_id = result
                .pointer("/parent/work_id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "decomposition replay has no parent identity".into(),
                    )
                })?;
            bound_items.push(load_work_item(connection, parse_work_id(parent_id)?)?);
            let children = result
                .get("children")
                .and_then(serde_json::Value::as_array)
                .ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "decomposition replay has no child identities".into(),
                    )
                })?;
            for child in children {
                let work_id = child
                    .get("work_id")
                    .and_then(serde_json::Value::as_str)
                    .ok_or_else(|| {
                        StoreError::InvalidWorkProjection(
                            "decomposition replay child has no work identity".into(),
                        )
                    })?;
                bound_items.push(load_work_item(connection, parse_work_id(work_id)?)?);
            }
        }
        "work_complete" => {
            let work_id = result
                .get("work_id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "completion replay has no work identity".into(),
                    )
                })?;
            bound_items.push(load_work_item(connection, parse_work_id(work_id)?)?);
        }
        operation
            if operation.starts_with("work_update:") || operation.starts_with("work_handoff:") =>
        {
            let work_id = result
                .pointer("/receipt/work_id")
                .and_then(serde_json::Value::as_str)
                .ok_or_else(|| {
                    StoreError::InvalidWorkProjection("ambient replay has no work identity".into())
                })?;
            bound_items.push(load_work_item(connection, parse_work_id(work_id)?)?);
        }
        _ => {
            return Err(StoreError::InvalidWorkProjection(format!(
                "unknown durable work-protocol operation {operation}"
            )));
        }
    }
    if bound_items
        .iter()
        .any(|item| item.project_id.0 != project_id)
    {
        return Err(StoreError::InvalidWorkProjection(format!(
            "work-protocol result {operation} crosses its project binding"
        )));
    }
    Ok(())
}

fn validate_work_source_snapshot(
    snapshot: &WorkSourceSnapshot,
    imported_at: DateTime<Utc>,
) -> Result<(), StoreError> {
    let required_text_is_valid = [
        &snapshot.adapter_kind,
        &snapshot.canonical_ref,
        &snapshot.fingerprint,
    ]
    .into_iter()
    .all(|value| !value.trim().is_empty() && value.trim() == value);
    let optional_text_is_valid = snapshot
        .source_revision
        .as_ref()
        .into_iter()
        .chain(snapshot.canonical_url.as_ref())
        .chain(snapshot.projected.title.as_ref())
        .chain(snapshot.projected.status.as_ref())
        .chain(snapshot.projected.owner.as_ref())
        .all(|value| !value.trim().is_empty() && value.trim() == value);
    if snapshot.schema_version != SCHEMA_VERSION
        || !required_text_is_valid
        || !optional_text_is_valid
        || snapshot.captured_at > imported_at
    {
        return Err(StoreError::InvalidWork(
            "work source snapshot has invalid schema, canonical text, or capture time".into(),
        ));
    }
    let object = CanonicalObject::freeze(snapshot)?;
    if object.bytes().len() > MAX_WORK_SOURCE_SNAPSHOT_BYTES {
        return Err(StoreError::InvalidWork(format!(
            "work source snapshot exceeds the {MAX_WORK_SOURCE_SNAPSHOT_BYTES}-byte canonical limit"
        )));
    }
    Ok(())
}

fn inspect_work_request<R: Redactor, T: Serialize>(
    redactor: &R,
    request: &T,
) -> Result<(), StoreError> {
    let candidate = serde_json::to_string(request)?;
    redactor
        .inspect(&candidate)
        .map_err(StoreError::RedactionRefused)
}

fn expire_handoff_offers(
    transaction: &Transaction<'_>,
    run_id: WorkRunId,
    now: DateTime<Utc>,
    actor: &crate::domain::ActorContext,
) -> Result<Vec<WorkHandoffOffer>, StoreError> {
    let mut statement = transaction.prepare(
        "SELECT offer_hash, offer_json FROM work_handoff_offers
         WHERE run_id = ?1 AND state = 'offered' AND expires_at_ms <= ?2
         ORDER BY offer_id",
    )?;
    let rows = statement
        .query_map(
            params![run_id.0.to_string(), now.timestamp_millis()],
            |row| Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let item_run = if rows.is_empty() {
        None
    } else {
        let run = load_work_run(transaction, run_id)?;
        let item = load_work_item(transaction, run.work_id)?;
        let root_execution = load_root_execution(transaction, run.root_execution_id)?;
        let claim = load_work_claim_optional(transaction, run_id)?;
        Some((item, run, root_execution, claim))
    };
    let mut expired = Vec::with_capacity(rows.len());
    for row in rows {
        let mut offer = load_handoff_offer_projection(transaction, row)?;
        offer.state = WorkHandoffState::Expired;
        let offer_object = CanonicalObject::freeze(&offer)?;
        SqliteStore::insert_object(transaction, "work_handoff_offer", &offer_object)?;
        let changed = transaction.execute(
            "UPDATE work_handoff_offers
             SET state = 'expired', offer_hash = ?2, offer_json = ?3
              WHERE offer_id = ?1 AND state = 'offered'",
            params![
                offer.offer_id.0.to_string(),
                offer_object.hash().as_str(),
                serde_json::to_vec(&offer)?
            ],
        )?;
        if changed != 1 {
            return Err(StoreError::InvalidWorkProjection(format!(
                "handoff offer {:?} was not offered during expiry",
                offer.offer_id
            )));
        }
        if let Some((item, run, root_execution, claim)) = item_run.as_ref() {
            let event = WorkEventDraft {
                schema_version: SCHEMA_VERSION,
                project_id: item.project_id.clone(),
                root_id: item.root_id,
                work_id: item.work_id,
                run_id: Some(run.run_id),
                revision: item.revision,
                work: item.clone(),
                run: Some(run.clone()),
                root_execution: Some(root_execution.clone()),
                claim: claim.clone(),
                handoff_offer: Some(offer.clone()),
                blocker: None,
                transition: WorkTransition::HandoffExpired {
                    offer_id: offer.offer_id,
                    offer: offer_object.hash().clone(),
                },
                actor: actor.clone(),
                created_at: now,
            };
            append_work_event(transaction, &event)?;
        }
        expired.push(offer);
    }
    Ok(expired)
}

fn replay_operation<T: DeserializeOwned>(
    transaction: &Transaction<'_>,
    operation: &str,
    key: &str,
    request_hash: &ObjectHash,
) -> Result<Option<T>, StoreError> {
    let stored: Option<(String, Vec<u8>)> = transaction
        .query_row(
            "SELECT request_hash, result_json FROM work_operation_results
             WHERE operation = ?1 AND idempotency_key = ?2",
            params![operation, key],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((stored_hash, result)) = stored else {
        return Ok(None);
    };
    if stored_hash != request_hash.as_str() {
        return Err(StoreError::WorkOperationIdempotencyConflict {
            operation: operation.into(),
            key: key.into(),
        });
    }
    serde_json::from_slice(&result)
        .map(Some)
        .map_err(StoreError::from)
}

fn projected_work_relation_basis(
    connection: &Connection,
    work_id: WorkId,
) -> Result<WorkRelationBasis, StoreError> {
    let mut prerequisite_ids = load_prerequisite_projection_ids(connection, work_id)?;
    prerequisite_ids.sort_by_key(|prerequisite_id| prerequisite_id.0);
    let mut active_blockers = load_active_blocker_projections(connection, work_id)?
        .into_iter()
        .map(|blocker| {
            let blocker_id = blocker.blocker_id.clone();
            let blocker_hash = CanonicalObject::freeze(&blocker)?.hash().clone();
            Ok(WorkRelationBlockerBasis {
                blocker_id,
                blocker_hash,
            })
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    active_blockers.sort_by(|left, right| left.blocker_id.cmp(&right.blocker_id));
    Ok(WorkRelationBasis {
        schema_version: SCHEMA_VERSION,
        prerequisite_ids,
        active_blockers,
    })
}

fn work_relation_fingerprint(basis: &WorkRelationBasis) -> Result<ObjectHash, StoreError> {
    Ok(CanonicalObject::freeze(basis)?.hash().clone())
}

fn validated_current_work_relation_basis(
    connection: &Connection,
    work_id: WorkId,
) -> Result<WorkRelationBasis, StoreError> {
    let latest = latest_canonical_work_event_for_item(connection, work_id)?;
    let basis = projected_work_relation_basis(connection, work_id)?;
    let actual = work_relation_fingerprint(&basis)?;
    if actual != latest.relation_fingerprint {
        return Err(StoreError::InvalidWorkProjection(format!(
            "relations for {work_id:?} differ from the latest canonical fingerprint"
        )));
    }
    Ok(basis)
}

fn apply_work_relation_transition(
    basis: &mut WorkRelationBasis,
    transition: &WorkTransition,
    blocker: Option<&WorkBlocker>,
) -> Result<(), StoreError> {
    match transition {
        WorkTransition::Created { prerequisites, .. } => {
            basis.prerequisite_ids.clone_from(prerequisites);
            basis
                .prerequisite_ids
                .sort_by_key(|prerequisite_id| prerequisite_id.0);
            basis.prerequisite_ids.dedup();
        }
        WorkTransition::PrerequisiteAdded {
            prerequisite_id, ..
        } => {
            if basis.prerequisite_ids.contains(prerequisite_id) {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "prerequisite {prerequisite_id:?} is already active"
                )));
            }
            basis.prerequisite_ids.push(*prerequisite_id);
            basis
                .prerequisite_ids
                .sort_by_key(|prerequisite_id| prerequisite_id.0);
        }
        WorkTransition::PrerequisiteRemoved {
            prerequisite_id, ..
        } => {
            let previous = basis.prerequisite_ids.len();
            basis
                .prerequisite_ids
                .retain(|candidate| candidate != prerequisite_id);
            if basis.prerequisite_ids.len() == previous {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "prerequisite {prerequisite_id:?} is not active"
                )));
            }
        }
        WorkTransition::Blocked { blocker_id } => {
            let blocker = blocker.ok_or_else(|| {
                StoreError::InvalidWorkProjection(format!(
                    "block event {blocker_id} has no blocker snapshot"
                ))
            })?;
            if blocker.blocker_id != *blocker_id
                || basis
                    .active_blockers
                    .iter()
                    .any(|candidate| candidate.blocker_id == *blocker_id)
            {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "block event {blocker_id} has an invalid relation transition"
                )));
            }
            basis.active_blockers.push(WorkRelationBlockerBasis {
                blocker_id: blocker_id.clone(),
                blocker_hash: CanonicalObject::freeze(blocker)?.hash().clone(),
            });
            basis
                .active_blockers
                .sort_by(|left, right| left.blocker_id.cmp(&right.blocker_id));
        }
        WorkTransition::Unblocked { blocker_id } => {
            let previous = basis.active_blockers.len();
            basis
                .active_blockers
                .retain(|candidate| candidate.blocker_id != *blocker_id);
            if basis.active_blockers.len() == previous {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "blocker {blocker_id} is not active"
                )));
            }
        }
        _ => {}
    }
    Ok(())
}

fn require_work_item_relation_integrity(
    connection: &Connection,
    work_id: WorkId,
) -> Result<(), StoreError> {
    validated_current_work_relation_basis(connection, work_id)?;
    Ok(())
}

fn persist_operation_result<T: Serialize>(
    transaction: &Transaction<'_>,
    operation: &str,
    key: &str,
    request_hash: &ObjectHash,
    result: &T,
) -> Result<(), StoreError> {
    transaction.execute(
        "INSERT INTO work_operation_results (
             operation, idempotency_key, request_hash, result_json
         ) VALUES (?1, ?2, ?3, ?4)",
        params![
            operation,
            key,
            request_hash.as_str(),
            serde_json::to_vec(result)?
        ],
    )?;
    Ok(())
}

fn normalize_text(value: &str, label: &str) -> Result<String, StoreError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(StoreError::InvalidWork(format!(
            "{label} must not be empty"
        )));
    }
    Ok(trimmed.to_owned())
}

fn normalize_optional(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    })
}

fn normalize_strings(values: &[String]) -> Vec<String> {
    let mut normalized = values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    normalized.sort();
    normalized.dedup();
    normalized
}

fn short_ref(work_id: WorkId) -> String {
    let simple = work_id.0.simple().to_string();
    format!("w-{}", simple.get(20..).unwrap_or(&simple))
}

fn claim_expiry(now: DateTime<Utc>, ttl_seconds: i64) -> Result<DateTime<Utc>, StoreError> {
    if !(1..=MAX_WORK_TTL_SECONDS).contains(&ttl_seconds) {
        return Err(StoreError::InvalidWork(format!(
            "claim TTL must be from 1 through {MAX_WORK_TTL_SECONDS} seconds"
        )));
    }
    now.checked_add_signed(chrono::TimeDelta::seconds(ttl_seconds))
        .ok_or_else(|| StoreError::InvalidWork("claim expiry exceeds the supported clock".into()))
}

fn encode_state<T: Serialize>(value: T) -> Result<String, StoreError> {
    let value = serde_json::to_value(value)?;
    value.as_str().map(str::to_owned).ok_or_else(|| {
        StoreError::InvalidWorkProjection("enum did not serialize as a string".into())
    })
}

fn assert_revision(item: &WorkItem, expected: i64) -> Result<(), StoreError> {
    if item.revision == expected {
        Ok(())
    } else {
        Err(StoreError::WorkRevisionConflict {
            work: item.work_id,
            expected,
            current: item.revision,
        })
    }
}

fn assert_actor_session(
    actor: &crate::domain::ActorContext,
    expected: &SessionId,
) -> Result<(), StoreError> {
    if actor.session_id.as_ref() == Some(expected) {
        Ok(())
    } else {
        Err(StoreError::InvalidWork(format!(
            "actor session {:?} does not match lifecycle holder {:?}",
            actor.session_id.as_ref().map(|session| &session.0),
            expected.0
        )))
    }
}

#[derive(Clone, Copy)]
struct AuthorityTarget<'a> {
    project_id: &'a crate::domain::ProjectId,
    policy_ref: &'a str,
    work_id: Option<WorkId>,
    root_id: Option<WorkId>,
    run_id: Option<WorkRunId>,
}

fn resolve_work_authority(
    connection: &Connection,
    decision: &LifecycleAuthorityDecision,
    actor: &crate::domain::ActorContext,
    operation: WorkAuthorityOperation,
    target: AuthorityTarget<'_>,
    at: DateTime<Utc>,
) -> Result<WorkAuthorityGrant, StoreError> {
    let grant = resolve_work_authority_grant(connection, decision, actor, target, at)?;
    if !grant.operations.contains(&operation) {
        return Err(StoreError::InvalidWork(format!(
            "work authority grant does not admit {operation:?}"
        )));
    }
    Ok(grant)
}

fn resolve_work_authority_grant(
    connection: &Connection,
    decision: &LifecycleAuthorityDecision,
    actor: &crate::domain::ActorContext,
    target: AuthorityTarget<'_>,
    at: DateTime<Utc>,
) -> Result<WorkAuthorityGrant, StoreError> {
    let stored: Option<(Vec<u8>, Option<i64>)> = connection
        .query_row(
            "SELECT grant_json, revoked_at_ms FROM work_authority_grants
             WHERE grant_hash = ?1",
            [decision.grant.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (bytes, revoked_at_ms) = stored.ok_or_else(|| {
        StoreError::InvalidWork("referenced work authority grant is not installed".into())
    })?;
    let grant: WorkAuthorityGrant = CanonicalObject::verify(&decision.grant, bytes)?.decode()?;
    require_authority_revocation_integrity(connection, &decision.grant, revoked_at_ms)?;
    // The caller and the target are checked first, so a grant hash in the
    // wrong hands learns only that it does not apply; an eligible caller then
    // hears exactly why: revoked, expired, not yet valid, or not admitted.
    if grant.subject_actor_id != actor.actor_id
        || !assurance_covers(actor.assurance, grant.assurance)
    {
        return Err(StoreError::InvalidWork(
            "work authority grant was issued to another actor or assurance".into(),
        ));
    }
    if grant.project_id != *target.project_id
        || grant.policy_ref != target.policy_ref
        || !authority_scope_matches(&grant.scope, target)
    {
        return Err(StoreError::InvalidWork(
            "work authority grant does not cover this project, policy, or work scope".into(),
        ));
    }
    if revoked_at_ms.is_some() {
        return Err(StoreError::InvalidWork(
            "work authority grant was revoked by the host".into(),
        ));
    }
    if grant.valid_until <= at {
        return Err(StoreError::InvalidWork(format!(
            "work authority grant expired at {}",
            grant.valid_until.format("%Y-%m-%d %H:%M:%S UTC")
        )));
    }
    if grant.issued_at > at {
        return Err(StoreError::InvalidWork(format!(
            "work authority grant is not valid before {}",
            grant.issued_at.format("%Y-%m-%d %H:%M:%S UTC")
        )));
    }
    Ok(grant)
}

fn require_authority_revocation_integrity(
    connection: &Connection,
    grant_hash: &ObjectHash,
    projected_at: Option<i64>,
) -> Result<(), StoreError> {
    let canonical_rows = {
        let mut statement = connection.prepare(
            "SELECT object_hash, canonical_json FROM objects
             WHERE object_kind = 'work_authority_revocation'
               AND json_extract(canonical_json, '$.grant') = ?1
             ORDER BY object_hash",
        )?;
        statement
            .query_map([grant_hash.as_str()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    if canonical_rows.len() > 1 {
        return Err(StoreError::InvalidWorkProjection(format!(
            "authority grant {grant_hash} has multiple canonical revocations"
        )));
    }
    let canonical = canonical_rows
        .into_iter()
        .next()
        .map(|(stored_hash, bytes)| {
            let hash = ObjectHash::from_stored(stored_hash.clone())
                .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
            let revocation: WorkAuthorityRevocation =
                CanonicalObject::verify(&hash, bytes)?.decode()?;
            if revocation.grant != *grant_hash {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "authority revocation {hash} crosses its grant binding"
                )));
            }
            Ok((hash, revocation))
        })
        .transpose()?;
    let projection: Option<(String, i64, Vec<u8>)> = connection
        .query_row(
            "SELECT revocation_hash, revoked_at_ms, revocation_json
             FROM work_authority_revocations WHERE grant_hash = ?1",
            [grant_hash.as_str()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        )
        .optional()?;
    let valid = match (canonical, projection, projected_at) {
        (None, None, None) => true,
        (Some((hash, revocation)), Some((row_hash, row_at, row_bytes)), Some(grant_at)) => {
            hash.as_str() == row_hash
                && revocation.revoked_at.timestamp_millis() == row_at
                && row_at == grant_at
                && CanonicalObject::verify(&hash, row_bytes)
                    .and_then(|object| object.decode::<WorkAuthorityRevocation>())
                    .is_ok_and(|row| row == revocation)
        }
        _ => false,
    };
    if !valid {
        return Err(StoreError::InvalidWorkProjection(format!(
            "authority grant {grant_hash} has an invalid revocation projection"
        )));
    }
    Ok(())
}

fn assurance_covers(
    actor: crate::domain::AssuranceLevel,
    required: crate::domain::AssuranceLevel,
) -> bool {
    fn rank(level: crate::domain::AssuranceLevel) -> u8 {
        match level {
            crate::domain::AssuranceLevel::Asserted => 0,
            crate::domain::AssuranceLevel::Authenticated => 1,
            crate::domain::AssuranceLevel::Signed => 2,
        }
    }
    rank(actor) >= rank(required)
}

fn authority_scope_matches(scope: &WorkAuthorityScope, target: AuthorityTarget<'_>) -> bool {
    match scope {
        WorkAuthorityScope::Project => true,
        WorkAuthorityScope::Root(root_id) => target.root_id == Some(*root_id),
        WorkAuthorityScope::Work(work_id) => target.work_id == Some(*work_id),
        WorkAuthorityScope::Run(run_id) => target.run_id == Some(*run_id),
    }
}

fn validate_planning_authority(
    transaction: &Transaction<'_>,
    item: &WorkItem,
    authority: &WorkPlanningAuthority,
    actor: &crate::domain::ActorContext,
    at: DateTime<Utc>,
) -> Result<WorkAuthorityGrant, StoreError> {
    if let Some(run_id) = item.active_run_id {
        expire_handoff_offers(transaction, run_id, at, actor)?;
    }
    let grant_hash = match authority {
        WorkPlanningAuthority::Claim { grant, .. } | WorkPlanningAuthority::Delegated { grant } => {
            grant
        }
    };
    let decision = LifecycleAuthorityDecision {
        grant: grant_hash.clone(),
    };
    let grant = resolve_work_authority(
        transaction,
        &decision,
        actor,
        WorkAuthorityOperation::Plan,
        AuthorityTarget {
            project_id: &item.project_id,
            policy_ref: &item.authority_policy_ref,
            work_id: Some(item.work_id),
            root_id: Some(item.root_id),
            run_id: item.active_run_id,
        },
        at,
    )?;
    match authority {
        WorkPlanningAuthority::Delegated { .. } => {
            if let Some(run_id) = item.active_run_id
                && load_work_claim_optional(transaction, run_id)?.is_some_and(|claim| {
                    claim.state == WorkClaimState::Active && claim.expires_at > at
                })
            {
                return Err(StoreError::InvalidWork(
                    "delegated planning cannot revise work held by a live claim; use the holder's claim-bound planning authority or wait for recovery"
                        .into(),
                ));
            }
        }
        WorkPlanningAuthority::Claim {
            run_id,
            holder,
            claim_id,
            claim_fence,
            ..
        } => {
            if actor.session_id.as_ref() != Some(holder) {
                return Err(StoreError::InvalidWork(
                    "planning claim holder must match the attributed actor session".into(),
                ));
            }
            validate_live_claim_on(
                transaction,
                item.work_id,
                *run_id,
                item.revision,
                holder,
                *claim_id,
                *claim_fence,
                at,
                false,
            )?;
        }
    }
    Ok(grant)
}

fn validate_decomposition_budget(
    connection: &Connection,
    parent: &WorkItem,
    budget: &WorkPlanningBudget,
    proposed_children: usize,
) -> Result<(), StoreError> {
    let proposed = u32::try_from(proposed_children)
        .map_err(|_| StoreError::InvalidWork("decomposition size overflow".into()))?;
    if budget.max_children_per_decomposition < 1 || proposed > budget.max_children_per_decomposition
    {
        return Err(StoreError::InvalidWork(
            "decomposition exceeds the authorized per-operation child budget".into(),
        ));
    }
    let depth = work_depth(connection, parent.work_id)? + 1;
    if depth > i64::from(budget.max_depth) {
        return Err(StoreError::InvalidWork(
            "decomposition exceeds the authorized hierarchy depth".into(),
        ));
    }
    let open_descendants = connection.query_row(
        "WITH RECURSIVE descendants(work_id) AS (
             SELECT work_id FROM work_items WHERE parent_id = ?1
             UNION
             SELECT child.work_id FROM work_items child
             JOIN descendants parent ON child.parent_id = parent.work_id
         )
         SELECT COUNT(*) FROM descendants
         JOIN work_items item USING(work_id)
         WHERE item.lifecycle IN ('proposed', 'open')",
        [parent.root_id.0.to_string()],
        |row| row.get::<_, i64>(0),
    )?;
    if open_descendants + i64::from(proposed) > i64::from(budget.max_open_descendants) {
        return Err(StoreError::InvalidWork(
            "decomposition exceeds the authorized open-descendant budget".into(),
        ));
    }
    Ok(())
}

fn work_depth(connection: &Connection, work_id: WorkId) -> Result<i64, StoreError> {
    let mut depth = 0_i64;
    let mut current = load_work_item(connection, work_id)?;
    while let Some(parent_id) = current.parent_id {
        depth += 1;
        if depth > 1_024 {
            return Err(StoreError::InvalidWorkProjection(
                "work hierarchy depth exceeds the corruption guard".into(),
            ));
        }
        current = load_work_item(connection, parent_id)?;
    }
    Ok(depth)
}

fn rebase_planning_claim(
    transaction: &Transaction<'_>,
    item: &WorkItem,
    authority: &WorkPlanningAuthority,
    at: DateTime<Utc>,
) -> Result<(Option<WorkClaim>, Option<WorkRun>), StoreError> {
    let WorkPlanningAuthority::Claim { run_id, .. } = authority else {
        return Ok((None, None));
    };
    let mut claim = load_work_claim_optional(transaction, *run_id)?
        .ok_or(StoreError::WorkClaimMismatch { work: item.work_id })?;
    let mut run = load_work_run(transaction, *run_id)?;
    claim.accepted_work_revision = item.revision;
    renew_holder_claim(transaction, &mut claim, at)?;
    run.revision += 1;
    run.updated_at = at;
    persist_work_run(transaction, &run, claim.fence)?;
    Ok((Some(claim), Some(run)))
}

fn renew_holder_claim(
    transaction: &Transaction<'_>,
    claim: &mut WorkClaim,
    at: DateTime<Utc>,
) -> Result<(), StoreError> {
    claim.expires_at = claim_expiry(at, DEFAULT_WORK_CLAIM_TTL_SECONDS)?;
    claim.revision += 1;
    persist_claim(transaction, claim)
}

fn normalize_work_catalog_key(value: &str) -> String {
    let nfc = value.trim().nfc().collect::<String>();
    nfc.as_str().case_fold().collect::<String>().nfc().collect()
}

fn work_catalog_search_text(
    connection: &Connection,
    item: &WorkItem,
) -> Result<String, StoreError> {
    let mut parts = vec![
        item.short_ref.clone(),
        item.title.clone(),
        item.outcome.clone(),
    ];
    parts.extend(item.labels.iter().cloned());
    let mut statement = connection.prepare(
        "SELECT blocker_json FROM work_blockers
         WHERE work_id = ?1 AND state = 'active' ORDER BY blocker_id",
    )?;
    let blocker_details = statement
        .query_map([item.work_id.0.to_string()], |row| row.get::<_, Vec<u8>>(0))?
        .map(|row| {
            serde_json::from_slice::<WorkBlocker>(&row?)
                .map(|blocker| blocker.detail)
                .map_err(StoreError::from)
        })
        .collect::<Result<Vec<_>, _>>()?;
    parts.extend(blocker_details);
    Ok(normalize_work_catalog_key(&parts.join("\n")))
}

fn refresh_work_catalog_projection(
    connection: &Connection,
    item: &WorkItem,
) -> Result<(), StoreError> {
    let assigned_to_key = item.assigned_to.as_deref().map(normalize_work_catalog_key);
    let search_text_key = work_catalog_search_text(connection, item)?;
    let changed = connection.execute(
        "UPDATE work_items
         SET assigned_to_key = ?2, search_text_key = ?3
         WHERE work_id = ?1",
        params![item.work_id.0.to_string(), assigned_to_key, search_text_key],
    )?;
    if changed != 1 {
        return Err(StoreError::InvalidWorkProjection(format!(
            "work catalog refresh lost item {:?}",
            item.work_id
        )));
    }
    connection.execute(
        "DELETE FROM work_item_labels WHERE work_id = ?1",
        [item.work_id.0.to_string()],
    )?;
    let mut label_keys = item
        .labels
        .iter()
        .map(|label| normalize_work_catalog_key(label))
        .collect::<Vec<_>>();
    label_keys.sort();
    label_keys.dedup();
    for label_key in label_keys {
        connection.execute(
            "INSERT INTO work_item_labels (work_id, label_key) VALUES (?1, ?2)",
            params![item.work_id.0.to_string(), label_key],
        )?;
    }
    connection.execute(
        "DELETE FROM work_catalog_fts WHERE work_id = ?1",
        [item.work_id.0.to_string()],
    )?;
    connection.execute(
        "INSERT INTO work_catalog_fts (work_id, search_text) VALUES (?1, ?2)",
        params![item.work_id.0.to_string(), search_text_key],
    )?;
    Ok(())
}

fn persist_work_item(transaction: &Transaction<'_>, item: &WorkItem) -> Result<(), StoreError> {
    transaction.execute(
        "UPDATE work_items SET
             lifecycle = ?2, priority = ?3, assigned_to = ?4,
             deferred_until_ms = ?5, revision = ?6, active_run_id = ?7,
             superseded_by = ?8, updated_at_ms = ?9, item_json = ?10
         WHERE work_id = ?1",
        params![
            item.work_id.0.to_string(),
            encode_state(item.lifecycle)?,
            item.priority,
            item.assigned_to,
            item.deferred_until.map(|value| value.timestamp_millis()),
            item.revision,
            item.active_run_id.map(|value| value.0.to_string()),
            item.superseded_by.map(|value| value.0.to_string()),
            item.updated_at.timestamp_millis(),
            serde_json::to_vec(item)?
        ],
    )?;
    refresh_work_catalog_projection(transaction, item)
}

fn persist_work_run(
    transaction: &Transaction<'_>,
    run: &WorkRun,
    claim_fence_head: i64,
) -> Result<(), StoreError> {
    transaction.execute(
        "UPDATE work_runs SET
             executor_session_id = ?2, state = ?3, revision = ?4,
             claim_fence_head = ?5, last_checkpoint_hash = ?6,
             completion_seal_hash = ?7, updated_at_ms = ?8, run_json = ?9
         WHERE run_id = ?1",
        params![
            run.run_id.0.to_string(),
            run.executor.as_ref().map(|value| value.0.as_str()),
            encode_state(run.state)?,
            run.revision,
            claim_fence_head,
            run.last_checkpoint.as_ref().map(ObjectHash::as_str),
            run.completion_seal.as_ref().map(ObjectHash::as_str),
            run.updated_at.timestamp_millis(),
            serde_json::to_vec(run)?
        ],
    )?;
    Ok(())
}

fn persist_claim(transaction: &Transaction<'_>, claim: &WorkClaim) -> Result<(), StoreError> {
    transaction.execute(
        "INSERT INTO work_claims (
             run_id, work_id, claim_id, holder_session_id, state,
             expires_at_ms, revision, fence, claim_json
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(run_id) DO UPDATE SET
             claim_id = excluded.claim_id,
             holder_session_id = excluded.holder_session_id,
             state = excluded.state,
             expires_at_ms = excluded.expires_at_ms,
             revision = excluded.revision,
             fence = excluded.fence,
             claim_json = excluded.claim_json",
        params![
            claim.run_id.0.to_string(),
            claim.work_id.0.to_string(),
            claim.claim_id.0.to_string(),
            claim.holder.0,
            encode_state(claim.state)?,
            claim.expires_at.timestamp_millis(),
            claim.revision,
            claim.fence,
            serde_json::to_vec(claim)?
        ],
    )?;
    Ok(())
}

fn persist_root_execution(
    transaction: &Transaction<'_>,
    execution: &RootExecution,
) -> Result<(), StoreError> {
    transaction.execute(
        "UPDATE work_root_executions SET
             state = ?2, revision = ?3, updated_at_ms = ?4, execution_json = ?5
         WHERE root_execution_id = ?1",
        params![
            execution.root_execution_id.0.to_string(),
            encode_state(execution.state)?,
            execution.revision,
            execution.updated_at.timestamp_millis(),
            serde_json::to_vec(execution)?
        ],
    )?;
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "the exact authority basis is intentionally explicit at the storage boundary"
)]
fn validate_live_claim_on(
    connection: &Connection,
    work_id: WorkId,
    run_id: WorkRunId,
    expected_work_revision: i64,
    holder: &SessionId,
    claim_id: WorkClaimId,
    claim_fence: i64,
    now: DateTime<Utc>,
    allow_pending_handoff: bool,
) -> Result<(WorkItem, WorkRun, WorkClaim), StoreError> {
    let item = load_work_item(connection, work_id)?;
    assert_revision(&item, expected_work_revision)?;
    if item.lifecycle != WorkLifecycle::Open || item.active_run_id != Some(run_id) {
        return Err(StoreError::WorkClaimMismatch { work: work_id });
    }
    let run = load_work_run(connection, run_id)?;
    if run.work_id != work_id
        || !matches!(run.state, WorkRunState::Claimed | WorkRunState::Active)
        || !ancestors_admit_execution(connection, &item)?
        || !run_uses_active_root_execution(connection, &item, &run)?
    {
        return Err(StoreError::WorkClaimMismatch { work: work_id });
    }
    let claim = load_work_claim_optional(connection, run_id)?
        .ok_or(StoreError::WorkClaimMismatch { work: work_id })?;
    let exact_authority_basis = claim.work_id == work_id
        && claim.run_id == run_id
        && claim.claim_id == claim_id
        && claim.accepted_work_revision == expected_work_revision
        && claim.fence == claim_fence
        && &claim.holder == holder
        && claim.state == WorkClaimState::Active;
    if exact_authority_basis && claim.expires_at <= now {
        return Err(StoreError::WorkClaimLapsed {
            work: work_id,
            expired_at: claim.expires_at,
        });
    }
    if !exact_authority_basis {
        return Err(StoreError::WorkClaimMismatch { work: work_id });
    }
    if !allow_pending_handoff {
        let pending: Option<i64> = connection
            .query_row(
                "SELECT 1 FROM work_handoff_offers
                 WHERE run_id = ?1 AND state = 'offered' AND expires_at_ms > ?2",
                params![run_id.0.to_string(), now.timestamp_millis()],
                |row| row.get(0),
            )
            .optional()?;
        if pending.is_some() {
            return Err(StoreError::InvalidWork(
                "a live handoff offer must be accepted or expire before this operation".into(),
            ));
        }
    }
    Ok((item, run, claim))
}

pub(super) fn validate_control_work_binding_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
    session_id: &SessionId,
    binding: &ControlWorkBinding,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    let item = match load_work_item(connection, binding.work_id) {
        Ok(item) => item,
        Err(StoreError::WorkNotFound(_)) => {
            return Err(StoreError::WorkClaimMismatch {
                work: binding.work_id,
            });
        }
        Err(error) => return Err(error),
    };
    if &item.project_id != project_id
        || !control_work_binding_was_valid_on(connection, project_id, session_id, binding)?
    {
        return Err(StoreError::WorkClaimMismatch {
            work: binding.work_id,
        });
    }
    match validate_live_claim_on(
        connection,
        binding.work_id,
        binding.run_id,
        binding.work_revision,
        session_id,
        binding.claim_id,
        binding.claim_fence,
        now,
        false,
    ) {
        Ok(_) => Ok(()),
        Err(
            StoreError::WorkRevisionConflict { .. }
            | StoreError::WorkClaimMismatch { .. }
            | StoreError::WorkClaimLapsed { .. }
            | StoreError::InvalidWork(_),
        ) => Err(StoreError::ControlWorkBindingStale {
            work: binding.work_id,
        }),
        Err(error) => Err(error),
    }
}

fn control_work_binding_was_valid_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
    session_id: &SessionId,
    binding: &ControlWorkBinding,
) -> Result<bool, StoreError> {
    Ok(canonical_work_events_for_item(connection, binding.work_id)?
        .iter()
        .any(|event| {
            event.project_id == *project_id
                && event.work_id == binding.work_id
                && event.work.lifecycle == WorkLifecycle::Open
                && event.work.revision == binding.work_revision
                && event.work.active_run_id == Some(binding.run_id)
                && event.run.as_ref().is_some_and(|run| {
                    run.run_id == binding.run_id
                        && run.work_id == binding.work_id
                        && run.root_execution_id == binding.root_execution_id
                        && matches!(run.state, WorkRunState::Claimed | WorkRunState::Active)
                })
                && event.claim.as_ref().is_some_and(|claim| {
                    claim.work_id == binding.work_id
                        && claim.run_id == binding.run_id
                        && claim.claim_id == binding.claim_id
                        && claim.accepted_work_revision == binding.work_revision
                        && claim.fence == binding.claim_fence
                        && claim.holder == *session_id
                        && claim.state == WorkClaimState::Active
                        && claim.expires_at > event.created_at
                })
        }))
}

fn unique_hashes(values: &[ObjectHash]) -> Vec<ObjectHash> {
    let mut seen = HashSet::new();
    values
        .iter()
        .filter(|value| seen.insert((*value).clone()))
        .cloned()
        .collect()
}

fn expect_root_contributor(execution: &mut RootExecution, participant: &SessionId) -> bool {
    if execution.expected_contributors.contains(participant) {
        return false;
    }
    execution.expected_contributors.push(participant.clone());
    execution
        .expected_contributors
        .sort_by(|left, right| left.0.cmp(&right.0));
    true
}

fn add_root_contribution(
    execution: &mut RootExecution,
    participant: &SessionId,
    object: &ObjectHash,
) -> bool {
    let contribution = RootContribution {
        participant: participant.clone(),
        object: object.clone(),
    };
    if execution.contributions.contains(&contribution) {
        return false;
    }
    execution.contributions.push(contribution);
    execution.contributions.sort_by(|left, right| {
        left.participant
            .0
            .cmp(&right.participant.0)
            .then_with(|| left.object.as_str().cmp(right.object.as_str()))
    });
    true
}

fn waive_root_contributor(
    execution: &mut RootExecution,
    participant: &SessionId,
    decision: &LifecycleAuthorityDecision,
    grant: &WorkAuthorityGrant,
    reason: &str,
) -> bool {
    if execution
        .contributions
        .iter()
        .any(|contribution| &contribution.participant == participant)
        || execution
            .waivers
            .iter()
            .any(|waiver| &waiver.participant == participant)
    {
        return false;
    }
    execution.waivers.push(CompletionWaiver {
        participant: participant.clone(),
        authority_grant: decision.grant.clone(),
        waived_by: grant.issued_by.actor_id.clone(),
        reason: reason.trim().to_owned(),
    });
    execution
        .waivers
        .sort_by(|left, right| left.participant.0.cmp(&right.participant.0));
    true
}

fn first_unaccounted_root_contributor(execution: &RootExecution) -> Option<&SessionId> {
    execution
        .expected_contributors
        .iter()
        .find(|participant| !root_participant_is_accounted(execution, participant))
}

fn root_participant_is_accounted(execution: &RootExecution, participant: &SessionId) -> bool {
    execution
        .contributions
        .iter()
        .any(|contribution| &contribution.participant == participant)
        || execution
            .waivers
            .iter()
            .any(|waiver| &waiver.participant == participant)
}

fn work_run_evidence_on(
    connection: &Connection,
    run_id: WorkRunId,
) -> Result<Vec<ObjectHash>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT evidence_hash FROM work_run_evidence
         WHERE run_id = ?1 ORDER BY evidence_hash",
    )?;
    statement
        .query_map([run_id.0.to_string()], |row| row.get::<_, String>(0))?
        .map(|row| {
            let value = row?;
            ObjectHash::from_stored(value.clone()).ok_or(StoreError::InvalidStoredHash(value))
        })
        .collect()
}

#[allow(
    clippy::too_many_arguments,
    reason = "the checkpoint object and event retain the complete exact claim basis"
)]
fn persist_work_checkpoint_on(
    transaction: &Transaction<'_>,
    item: &WorkItem,
    mut run: WorkRun,
    claim: WorkClaim,
    summary: String,
    evidence: Vec<ObjectHash>,
    actor: &crate::domain::ActorContext,
    checkpointed_at: DateTime<Utc>,
) -> Result<ObjectHash, StoreError> {
    let acknowledged_run_position = FeedPosition {
        feed: FeedId::RunExecution(run.run_id),
        position: feed_head(transaction, &FeedId::RunExecution(run.run_id))?,
    };
    let checkpoint = WorkCheckpoint {
        schema_version: SCHEMA_VERSION,
        work_id: item.work_id,
        run_id: run.run_id,
        claim_id: claim.claim_id,
        claim_fence: claim.fence,
        acknowledged_run_position,
        summary,
        evidence,
        actor: actor.clone(),
        created_at: checkpointed_at,
    };
    let object = CanonicalObject::freeze(&checkpoint)?;
    SqliteStore::insert_object(transaction, "work_checkpoint", &object)?;
    append_to_work_feeds(
        transaction,
        &item.project_id,
        item.root_id,
        Some(run.run_id),
        None,
        "work_checkpoint",
        &object,
    )?;
    run.last_checkpoint = Some(object.hash().clone());
    run.state = WorkRunState::Active;
    run.revision += 1;
    run.updated_at = checkpointed_at;
    persist_work_run(transaction, &run, claim.fence)?;
    let mut root_execution = load_root_execution(transaction, run.root_execution_id)?;
    let root_changed = expect_root_contributor(&mut root_execution, &claim.holder)
        | add_root_contribution(&mut root_execution, &claim.holder, object.hash());
    if root_changed {
        root_execution.revision += 1;
        root_execution.updated_at = checkpointed_at;
        persist_root_execution(transaction, &root_execution)?;
    }
    append_work_event(
        transaction,
        &WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: Some(run.run_id),
            revision: item.revision,
            work: item.clone(),
            run: Some(run),
            root_execution: Some(root_execution),
            claim: Some(claim),
            handoff_offer: None,
            blocker: None,
            transition: WorkTransition::Checkpointed {
                checkpoint: object.hash().clone(),
            },
            actor: actor.clone(),
            created_at: checkpointed_at,
        },
    )?;
    Ok(object.hash().clone())
}

fn persist_work_evidence_on(
    transaction: &Transaction<'_>,
    item: &WorkItem,
    run: &WorkRun,
    claim: WorkClaim,
    evidence: &WorkEvidence,
) -> Result<ObjectHash, StoreError> {
    let object = CanonicalObject::freeze(evidence)?;
    SqliteStore::insert_object(transaction, "work_evidence", &object)?;
    transaction.execute(
        "INSERT INTO work_run_evidence (evidence_hash, work_id, run_id)
         VALUES (?1, ?2, ?3)",
        params![
            object.hash().as_str(),
            item.work_id.0.to_string(),
            run.run_id.0.to_string()
        ],
    )?;
    append_to_work_feeds(
        transaction,
        &item.project_id,
        item.root_id,
        Some(run.run_id),
        None,
        "work_evidence",
        &object,
    )?;
    let mut root_execution = load_root_execution(transaction, run.root_execution_id)?;
    let root_changed = expect_root_contributor(&mut root_execution, &claim.holder)
        | add_root_contribution(&mut root_execution, &claim.holder, object.hash());
    if root_changed {
        root_execution.revision += 1;
        root_execution.updated_at = evidence.created_at;
        persist_root_execution(transaction, &root_execution)?;
    }
    append_work_event(
        transaction,
        &WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: Some(run.run_id),
            revision: item.revision,
            work: item.clone(),
            run: Some(run.clone()),
            root_execution: Some(root_execution),
            claim: Some(claim),
            handoff_offer: None,
            blocker: None,
            transition: WorkTransition::EvidenceAdded {
                evidence: object.hash().clone(),
            },
            actor: evidence.actor.clone(),
            created_at: evidence.created_at,
        },
    )?;
    Ok(object.hash().clone())
}

fn append_gate_evidence_on(
    transaction: &Transaction<'_>,
    request: &RecordGateEvidenceRequest,
    name: String,
    failed: Vec<String>,
    refs: Vec<String>,
    previous: Option<ObjectHash>,
) -> Result<ObjectHash, StoreError> {
    expire_handoff_offers(
        transaction,
        request.run_id,
        request.recorded_at,
        &request.actor,
    )?;
    let (item, run, mut claim) = validate_live_claim_on(
        transaction,
        request.work_id,
        request.run_id,
        request.expected_work_revision,
        &request.holder,
        request.claim_id,
        request.claim_fence,
        request.recorded_at,
        false,
    )?;
    let gate = GateEvidenceRecord {
        schema_version: SCHEMA_VERSION,
        name,
        passed: failed.is_empty(),
        failed,
        previous,
    };
    renew_holder_claim(transaction, &mut claim, request.recorded_at)?;
    let evidence = WorkEvidence {
        schema_version: SCHEMA_VERSION,
        work_id: item.work_id,
        run_id: run.run_id,
        claim_id: claim.claim_id,
        claim_fence: claim.fence,
        summary: GATE_EVIDENCE_SUMMARY.into(),
        refs,
        gate: Some(gate),
        actor: request.actor.clone(),
        created_at: request.recorded_at,
    };
    persist_work_evidence_on(transaction, &item, &run, claim, &evidence)
}

const LATEST_GATE_EVIDENCE_SQL: &str = "SELECT entry.object_hash
     FROM work_feed_entries entry
     WHERE entry.feed_kind = 'run_execution'
       AND entry.feed_id = ?1
       AND entry.object_kind = 'work_evidence'
       AND entry.position = (
           SELECT MAX(candidate.position)
           FROM objects object INDEXED BY objects_work_evidence_gate_name
           JOIN work_run_evidence evidence
             ON evidence.run_id = ?1
            AND evidence.evidence_hash = object.object_hash
           JOIN work_feed_entries candidate
             ON candidate.feed_kind = 'run_execution'
            AND candidate.feed_id = evidence.run_id
            AND candidate.object_hash = evidence.evidence_hash
           WHERE object.object_kind = 'work_evidence'
             AND json_extract(object.canonical_json, '$.run_id') = ?1
             AND json_type(object.canonical_json, '$.gate') = 'object'
             AND json_extract(object.canonical_json, '$.gate.name') = ?2
       )";

fn latest_gate_evidence_on(
    connection: &Connection,
    run_id: WorkRunId,
    name: &str,
) -> Result<Option<(ObjectHash, WorkEvidence)>, StoreError> {
    // The canonical run feed is the sole source of the previous transition.
    // The rebuildable expression index narrows candidates, but a mutable head
    // must never redirect an immutable `previous` link.
    let stored = connection
        .query_row(
            LATEST_GATE_EVIDENCE_SQL,
            params![run_id.0.to_string(), name],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    stored
        .map(|stored| {
            let hash = ObjectHash::from_stored(stored.clone())
                .ok_or(StoreError::InvalidStoredHash(stored))?;
            let evidence =
                load_typed_work_object::<WorkEvidence>(connection, &hash, "work_evidence")?;
            let gate = evidence.gate.as_ref().ok_or_else(|| {
                StoreError::InvalidWorkProjection(format!(
                    "gate evidence {hash} has no typed gate payload"
                ))
            })?;
            if evidence.run_id != run_id || gate.name != name {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "gate evidence {hash} disagrees with its indexed identity"
                )));
            }
            validate_gate_evidence(&hash, &evidence)?;
            Ok((hash, evidence))
        })
        .transpose()
}

fn validate_gate_evidence(
    evidence_hash: &ObjectHash,
    evidence: &WorkEvidence,
) -> Result<(), StoreError> {
    validate_gate_evidence_payload(evidence).map_err(|detail| {
        StoreError::InvalidWorkProjection(format!(
            "gate evidence {evidence_hash} has an invalid typed payload: {detail}"
        ))
    })
}

fn validate_gate_evidence_chain(
    evidence_hash: &ObjectHash,
    evidence: &WorkEvidence,
    gate_heads: &mut HashMap<(WorkRunId, String), ObjectHash>,
) -> Result<(), StoreError> {
    validate_gate_evidence(evidence_hash, evidence)?;
    let Some(gate) = &evidence.gate else {
        return Ok(());
    };
    let key = (evidence.run_id, gate.name.clone());
    if gate.previous.as_ref() != gate_heads.get(&key) {
        return Err(StoreError::InvalidWorkProjection(format!(
            "gate evidence {evidence_hash} does not name the prior same-run, same-name observation"
        )));
    }
    gate_heads.insert(key, evidence_hash.clone());
    Ok(())
}

fn gate_observation_matches(
    evidence: &WorkEvidence,
    request: &RecordGateEvidenceRequest,
    name: &str,
    failed: &[String],
    refs: &[String],
) -> bool {
    evidence.work_id == request.work_id
        && evidence.run_id == request.run_id
        && evidence.claim_id == request.claim_id
        && evidence.claim_fence == request.claim_fence
        && evidence.actor == request.actor
        && evidence.refs == refs
        && evidence.gate.as_ref().is_some_and(|gate| {
            gate.schema_version == SCHEMA_VERSION
                && gate.name == name
                && gate.passed == failed.is_empty()
                && gate.failed == failed
        })
}

fn work_run_evidence_projection_on(
    connection: &Connection,
    run_id: WorkRunId,
    required_environments: &[ObjectHash],
    limit: usize,
) -> Result<Vec<WorkEvidenceProjectionSummary>, StoreError> {
    let mut selected =
        load_work_evidence_selection_rows_on(connection, run_id, required_environments, limit)?;
    for required in required_environments {
        if let Some(candidate) = selected
            .iter()
            .find(|candidate| candidate.hash == *required)
            && candidate.kind != WorkEvidenceKind::Environment
        {
            return Err(StoreError::InvalidWorkProjection(format!(
                "required focus environment {required} is not typed environment evidence for run {}",
                run_id.0
            )));
        }
    }
    let selected_hashes = selected
        .iter()
        .map(|candidate| candidate.hash.clone())
        .collect::<HashSet<_>>();
    let linked_environments = selected
        .iter()
        .filter_map(|candidate| candidate.environment.clone())
        .filter(|hash| !selected_hashes.contains(hash))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if !linked_environments.is_empty() {
        let closure =
            load_work_evidence_selection_rows_on(connection, run_id, &linked_environments, 0)?;
        for environment in &linked_environments {
            if !closure.iter().any(|candidate| {
                candidate.hash == *environment && candidate.kind == WorkEvidenceKind::Environment
            }) {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "verification evidence names non-environment evidence {environment} for run {}",
                    run_id.0
                )));
            }
        }
        selected.extend(closure);
    }
    selected.sort_by(|left, right| left.hash.as_str().cmp(right.hash.as_str()));
    selected.dedup_by(|left, right| left.hash == right.hash);
    Ok(selected)
}

fn load_work_evidence_selection_rows_on(
    connection: &Connection,
    run_id: WorkRunId,
    required: &[ObjectHash],
    limit: usize,
) -> Result<Vec<WorkEvidenceProjectionSummary>, StoreError> {
    let required_json =
        serde_json::to_string(&required.iter().map(ObjectHash::as_str).collect::<Vec<_>>())?;
    let limit = i64::try_from(limit).map_err(|_| {
        StoreError::InvalidWorkProjection("focus evidence limit does not fit SQLite".into())
    })?;
    let mut statement = connection.prepare(
        "WITH recent(evidence_hash) AS (
             SELECT evidence_hash FROM work_run_evidence
             WHERE run_id = ?1 ORDER BY evidence_hash DESC LIMIT ?2
         ), requested(evidence_hash) AS (
             SELECT value FROM json_each(?3)
         ), candidates(evidence_hash) AS (
             SELECT evidence_hash FROM recent
             UNION
             SELECT evidence_hash FROM requested
         )
         SELECT projection.evidence_hash, projection.work_id, projection.run_id,
                projection.evidence_kind, projection.environment_evidence_hash,
                object.object_kind, object.canonical_json
         FROM candidates candidate
         JOIN work_run_evidence projection
           ON projection.run_id = ?1
          AND projection.evidence_hash = candidate.evidence_hash
         LEFT JOIN objects object ON object.object_hash = projection.evidence_hash
         ORDER BY projection.evidence_hash",
    )?;
    let rows = statement
        .query_map(params![run_id.0.to_string(), limit, required_json], |row| {
            Ok(StoredWorkEvidenceSelectionRow {
                hash: row.get(0)?,
                projected_work: row.get(1)?,
                projected_run: row.get(2)?,
                projected_kind: row.get(3)?,
                projected_environment: row.get(4)?,
                object_kind: row.get(5)?,
                canonical_json: row.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(|row| {
            let hash = ObjectHash::from_stored(row.hash.clone())
                .ok_or(StoreError::InvalidStoredHash(row.hash))?;
            let object_kind = row.object_kind.ok_or_else(|| {
                StoreError::InvalidWorkProjection(format!(
                    "run evidence projection names missing object {hash}"
                ))
            })?;
            let bytes = row.canonical_json.ok_or_else(|| {
                StoreError::InvalidWorkProjection(format!(
                    "run evidence projection names missing canonical bytes {hash}"
                ))
            })?;
            let object = CanonicalObject::verify(&hash, bytes)?;
            let (kind, environment, canonical_work, canonical_run) = match object_kind.as_str() {
                "work_evidence" => {
                    let evidence: WorkEvidence = object.decode()?;
                    if evidence.schema_version != SCHEMA_VERSION {
                        return Err(StoreError::InvalidWorkProjection(format!(
                            "generic evidence {hash} has an unsupported schema"
                        )));
                    }
                    (
                        WorkEvidenceKind::Generic,
                        None,
                        evidence.work_id,
                        evidence.run_id,
                    )
                }
                "verification_evidence" => {
                    let evidence: VerificationEvidence = object.decode()?;
                    if evidence.schema_version != SCHEMA_VERSION {
                        return Err(StoreError::InvalidWorkProjection(format!(
                            "verification evidence {hash} has an unsupported schema"
                        )));
                    }
                    (
                        WorkEvidenceKind::Verification,
                        evidence.environment,
                        evidence.binding.work_id,
                        evidence.binding.run_id,
                    )
                }
                "environment_evidence" => {
                    let evidence: EnvironmentEvidence = object.decode()?;
                    if evidence.schema_version != SCHEMA_VERSION {
                        return Err(StoreError::InvalidWorkProjection(format!(
                            "environment evidence {hash} has an unsupported schema"
                        )));
                    }
                    (
                        WorkEvidenceKind::Environment,
                        None,
                        evidence.binding.work_id,
                        evidence.binding.run_id,
                    )
                }
                other => {
                    return Err(StoreError::InvalidWorkProjection(format!(
                        "run evidence object {hash} has unsupported kind {other:?}"
                    )));
                }
            };
            let projected_environment = row
                .projected_environment
                .map(|stored| {
                    ObjectHash::from_stored(stored.clone())
                        .ok_or(StoreError::InvalidStoredHash(stored))
                })
                .transpose()?;
            let expected_kind = match kind {
                WorkEvidenceKind::Generic => "generic",
                WorkEvidenceKind::Verification => "verification",
                WorkEvidenceKind::Environment => "environment",
            };
            if row.projected_kind != expected_kind
                || projected_environment != environment
                || row.projected_work != canonical_work.0.to_string()
                || row.projected_run != canonical_run.0.to_string()
                || canonical_run != run_id
            {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "evidence object {hash} disagrees with its selection projection"
                )));
            }
            Ok(WorkEvidenceProjectionSummary {
                hash,
                kind,
                environment,
            })
        })
        .collect()
}

fn work_evidence_kind_on(
    connection: &Connection,
    run_id: WorkRunId,
    evidence_hash: &ObjectHash,
) -> Result<WorkEvidenceKind, StoreError> {
    let projected = connection
        .query_row(
            "SELECT work_id, run_id, evidence_kind,
                    workspace_id, source_revision, producer_session_id,
                    producer_observation_hash, check_fingerprint,
                    verification_result, observed_at_ms, environment_fingerprint,
                    environment_evidence_hash, components_json
             FROM work_run_evidence
             WHERE run_id = ?1 AND evidence_hash = ?2",
            params![run_id.0.to_string(), evidence_hash.as_str()],
            |row| {
                Ok(EvidenceProjectionRow {
                    work_id: row.get(0)?,
                    run_id: row.get(1)?,
                    evidence_kind: row.get(2)?,
                    workspace_id: row.get(3)?,
                    source_revision: row.get(4)?,
                    producer_session_id: row.get(5)?,
                    producer_observation_hash: row.get(6)?,
                    check_fingerprint: row.get(7)?,
                    verification_result: row.get(8)?,
                    observed_at_ms: row.get(9)?,
                    environment_fingerprint: row.get(10)?,
                    environment_evidence_hash: row.get(11)?,
                    components_json: row.get(12)?,
                })
            },
        )
        .optional()?
        .ok_or_else(|| {
            StoreError::InvalidWork(format!(
                "evidence object {evidence_hash} does not belong to run {run_id:?}"
            ))
        })?;
    let (kind, expected) = match projected.evidence_kind.as_str() {
        "generic" => {
            let evidence =
                load_typed_work_object::<WorkEvidence>(connection, evidence_hash, "work_evidence")?;
            if evidence.schema_version != SCHEMA_VERSION {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "generic evidence {evidence_hash} has an unsupported schema"
                )));
            }
            validate_gate_evidence(evidence_hash, &evidence)?;
            (
                WorkEvidenceKind::Generic,
                EvidenceProjectionRow {
                    work_id: evidence.work_id.0.to_string(),
                    run_id: evidence.run_id.0.to_string(),
                    evidence_kind: "generic".into(),
                    workspace_id: None,
                    source_revision: None,
                    producer_session_id: None,
                    producer_observation_hash: None,
                    check_fingerprint: None,
                    verification_result: None,
                    observed_at_ms: None,
                    environment_fingerprint: None,
                    environment_evidence_hash: None,
                    components_json: None,
                },
            )
        }
        "verification" => (
            WorkEvidenceKind::Verification,
            expected_verification_projection(connection, evidence_hash)?,
        ),
        "environment" => (
            WorkEvidenceKind::Environment,
            expected_environment_projection(connection, evidence_hash)?,
        ),
        kind => {
            return Err(StoreError::InvalidWorkProjection(format!(
                "evidence object {evidence_hash} has unknown kind {kind:?}"
            )));
        }
    };
    if projected != expected {
        return Err(StoreError::InvalidWorkProjection(format!(
            "evidence object {evidence_hash} disagrees with its redundant run projection"
        )));
    }
    Ok(kind)
}

fn ensure_run_evidence(
    connection: &Connection,
    run_id: WorkRunId,
    evidence: &[ObjectHash],
) -> Result<(), StoreError> {
    for hash in unique_hashes(evidence) {
        work_evidence_kind_on(connection, run_id, &hash)?;
    }
    Ok(())
}

fn validate_acceptance(
    connection: &Connection,
    item: &WorkItem,
    run_id: WorkRunId,
    completion_evidence: &[ObjectHash],
    results: &[AcceptanceResult],
    actor_assurance: crate::domain::AssuranceLevel,
) -> Result<Vec<AcceptanceResult>, StoreError> {
    let shaped = normalize_completion_acceptance_shape(item, results, actor_assurance)?;
    let completion_evidence = completion_evidence
        .iter()
        .map(ObjectHash::as_str)
        .collect::<HashSet<_>>();
    let mut normalized = Vec::with_capacity(shaped.len());
    for mut result in shaped {
        let evidence = unique_hashes(&result.evidence);
        if evidence.is_empty()
            || evidence
                .iter()
                .any(|hash| !completion_evidence.contains(hash.as_str()))
        {
            return Err(StoreError::WorkCompletionRefused {
                work: item.work_id,
                reason: format!(
                    "acceptance criterion {:?} must cite completion evidence",
                    result.criterion
                ),
            });
        }
        ensure_run_evidence(connection, run_id, &evidence)?;
        result.evidence = evidence;
        normalized.push(result);
    }
    Ok(normalized)
}

pub(crate) fn normalize_completion_acceptance_shape(
    item: &WorkItem,
    results: &[AcceptanceResult],
    actor_assurance: crate::domain::AssuranceLevel,
) -> Result<Vec<AcceptanceResult>, StoreError> {
    if item.acceptance.len() != results.len() {
        let missing = item.acceptance.iter().find(|criterion| {
            !results
                .iter()
                .any(|result| result.criterion.trim() == criterion.as_str())
        });
        if let Some(criterion) = missing {
            return Err(StoreError::WorkCompletionRecoveryRequired {
                work: item.work_id,
                cause: WorkCompletionRecoveryCause::MissingAcceptance {
                    criterion: criterion.clone(),
                },
            });
        }
        return Err(StoreError::WorkCompletionRefused {
            work: item.work_id,
            reason: "acceptance results do not cover every current criterion".into(),
        });
    }
    let mut by_criterion = HashMap::new();
    for result in results {
        if result.assurance != actor_assurance {
            return Err(StoreError::WorkCompletionRefused {
                work: item.work_id,
                reason: "acceptance assurance must equal the completing actor assurance".into(),
            });
        }
        let criterion = normalize_text(&result.criterion, "acceptance criterion")?;
        if by_criterion.insert(criterion, result).is_some() {
            return Err(StoreError::WorkCompletionRefused {
                work: item.work_id,
                reason: "acceptance results contain a duplicate criterion".into(),
            });
        }
    }
    let mut normalized = Vec::with_capacity(item.acceptance.len());
    for criterion in &item.acceptance {
        let Some(result) = by_criterion.get(criterion) else {
            return Err(StoreError::WorkCompletionRecoveryRequired {
                work: item.work_id,
                cause: WorkCompletionRecoveryCause::MissingAcceptance {
                    criterion: criterion.clone(),
                },
            });
        };
        if !result.satisfied {
            return Err(StoreError::WorkCompletionRecoveryRequired {
                work: item.work_id,
                cause: WorkCompletionRecoveryCause::MissingAcceptance {
                    criterion: criterion.clone(),
                },
            });
        }
        normalized.push(AcceptanceResult {
            criterion: criterion.clone(),
            satisfied: true,
            evidence: result.evidence.clone(),
            assurance: result.assurance,
            note: result.note.trim().to_owned(),
        });
    }
    Ok(normalized)
}

fn validate_completion_seal_children_on(
    connection: &Connection,
    seal: &CompletionSeal,
    depth: usize,
) -> Result<(), StoreError> {
    if depth > 1_024 {
        return Err(StoreError::InvalidWorkProjection(
            "completion-seal child graph exceeds the corruption guard".into(),
        ));
    }
    let mut seen = HashSet::new();
    for child_hash in &seal.required_child_seals {
        if !seen.insert(child_hash.clone()) {
            return Err(StoreError::InvalidWorkProjection(format!(
                "completion seal for run {} repeats child seal {child_hash}",
                seal.run_id.0
            )));
        }
        let child_seal: CompletionSeal =
            load_typed_work_object(connection, child_hash, "completion_seal")?;
        let child = load_work_item(connection, child_seal.work_id)?;
        if child.parent_id != Some(seal.work_id)
            || child.child_requirement != ChildRequirement::Required
            || child_seal.root_id != seal.root_id
            || child_seal.root_execution_id != seal.root_execution_id
        {
            return Err(StoreError::InvalidWorkProjection(format!(
                "completion seal for run {} cites unrelated child seal {child_hash}",
                seal.run_id.0
            )));
        }
        validate_completion_seal_obligation_basis_on(connection, &child_seal)?;
        validate_completion_seal_environment_basis_on(connection, &child_seal)?;
        validate_completion_seal_children_on(connection, &child_seal, depth + 1)?;
    }
    Ok(())
}

fn required_child_seals(
    connection: &Connection,
    parent_id: WorkId,
    root_execution_id: RootExecutionId,
) -> Result<Vec<ObjectHash>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT child.work_id, run.run_id, seals.seal_hash
         FROM work_items child
         JOIN work_runs run ON run.work_id = child.work_id
         JOIN work_completion_seals seals ON seals.run_id = run.run_id
         WHERE child.parent_id = ?1
           AND run.root_execution_id = ?2
           AND child.child_requirement = 'required'
           AND child.lifecycle = 'completed'
           AND run.state = 'completed'
           AND run.generation = (
               SELECT MAX(latest.generation) FROM work_runs latest
               WHERE latest.work_id = child.work_id
           )
         ORDER BY child.work_id",
    )?;
    let rows = statement
        .query_map(
            params![parent_id.0.to_string(), root_execution_id.0.to_string()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    let mut hashes = Vec::with_capacity(rows.len());
    for (child_work, child_run, stored_hash) in rows {
        let hash = ObjectHash::from_stored(stored_hash.clone())
            .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
        let seal: CompletionSeal = load_typed_work_object(connection, &hash, "completion_seal")?;
        if seal.work_id.0.to_string() != child_work
            || seal.run_id.0.to_string() != child_run
            || seal.root_execution_id != root_execution_id
        {
            return Err(StoreError::InvalidWorkProjection(format!(
                "required child seal {hash} does not match its child run"
            )));
        }
        validate_completion_seal_obligation_basis_on(connection, &seal)?;
        validate_completion_seal_environment_basis_on(connection, &seal)?;
        validate_completion_seal_children_on(connection, &seal, 0)?;
        hashes.push(hash);
    }
    Ok(hashes)
}

fn validated_required_child_waivers(
    connection: &Connection,
    parent_id: WorkId,
    execution: &RootExecution,
) -> Result<Vec<RequiredChildWaiver>, StoreError> {
    // The root-execution projection is already bound to the latest canonical
    // event. An empty projected waiver set cannot authorize completion, so it
    // is safe to avoid replaying the retained root history here. Doctor still
    // performs the exhaustive comparison below for nonempty projected sets.
    if execution.required_child_waivers.is_empty() {
        return Ok(Vec::new());
    }
    let mut statement = connection.prepare(
        "SELECT object_hash FROM work_feed_entries
         WHERE feed_kind = 'root_work' AND feed_id = ?1
           AND object_kind = 'work_event'
         ORDER BY position",
    )?;
    let hashes = statement
        .query_map([execution.root_id.0.to_string()], |row| {
            row.get::<_, String>(0)
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);

    let mut events = HashMap::new();
    for stored_hash in hashes {
        let hash = ObjectHash::from_stored(stored_hash.clone())
            .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
        let event: WorkEvent = load_typed_work_object(connection, &hash, "work_event")?;
        let Some(event_execution) = event.root_execution.as_ref() else {
            continue;
        };
        if event_execution.root_execution_id != execution.root_execution_id {
            continue;
        }
        let WorkTransition::RequiredChildWaived {
            child_id,
            child_revision,
            reason,
            authority_grant,
        } = &event.transition
        else {
            continue;
        };
        let child = load_work_item(connection, *child_id)?;
        let grant: WorkAuthorityGrant =
            load_typed_work_object(connection, authority_grant, "work_authority_grant")?;
        let revoked_at_ms: Option<i64> = connection
            .query_row(
                "SELECT revoked_at_ms FROM work_authority_grants WHERE grant_hash = ?1",
                [authority_grant.as_str()],
                |row| row.get(0),
            )
            .optional()?
            .flatten();
        let waiver = RequiredChildWaiver {
            work_id: *child_id,
            work_revision: *child_revision,
            authority_grant: authority_grant.clone(),
            waived_by: grant.issued_by.actor_id.clone(),
            reason: reason.clone(),
        };
        let event_contains_exact_waiver = event_execution
            .required_child_waivers
            .iter()
            .filter(|candidate| *candidate == &waiver)
            .count()
            == 1;
        let valid = event.schema_version == SCHEMA_VERSION
            && event.project_id == execution.project_id
            && event.root_id == execution.root_id
            && event.work_id == child.parent_id.unwrap_or(event.work_id)
            && child.parent_id == Some(event.work_id)
            && child.root_id == execution.root_id
            && child.child_requirement == ChildRequirement::Required
            && matches!(
                child.lifecycle,
                WorkLifecycle::Cancelled | WorkLifecycle::Superseded
            )
            && child.revision == *child_revision
            && grant.schema_version == SCHEMA_VERSION
            && grant.project_id == event.project_id
            && grant.policy_ref == child.authority_policy_ref
            && grant.subject_actor_id == event.actor.actor_id
            && assurance_covers(event.actor.assurance, grant.assurance)
            && grant
                .operations
                .contains(&WorkAuthorityOperation::CompletionWaiver)
            && grant.issued_at <= event.created_at
            && grant.valid_until > event.created_at
            && revoked_at_ms.is_none_or(|revoked| revoked > event.created_at.timestamp_millis())
            && authority_scope_matches(
                &grant.scope,
                AuthorityTarget {
                    project_id: &event.project_id,
                    policy_ref: &child.authority_policy_ref,
                    work_id: Some(child.work_id),
                    root_id: Some(child.root_id),
                    run_id: child.active_run_id,
                },
            )
            && event_contains_exact_waiver;
        if !valid || events.insert(*child_id, waiver).is_some() {
            return Err(StoreError::InvalidWorkProjection(format!(
                "required-child waiver event {hash} is not uniquely bound"
            )));
        }
    }

    let mut projected = HashMap::new();
    for waiver in &execution.required_child_waivers {
        if projected.insert(waiver.work_id, waiver.clone()).is_some() {
            return Err(StoreError::InvalidWorkProjection(format!(
                "root execution {:?} duplicates a required-child waiver for {:?}",
                execution.root_execution_id, waiver.work_id
            )));
        }
    }
    if projected != events {
        return Err(StoreError::InvalidWorkProjection(format!(
            "root execution {:?} required-child waivers do not match canonical events",
            execution.root_execution_id
        )));
    }

    let mut direct = Vec::new();
    for waiver in projected.into_values() {
        let child = load_work_item(connection, waiver.work_id)?;
        if child.parent_id == Some(parent_id) {
            direct.push(waiver);
        }
    }
    direct.sort_by(|left, right| left.work_id.0.as_bytes().cmp(right.work_id.0.as_bytes()));
    Ok(direct)
}

fn verify_required_child_waiver_bindings(
    connection: &Connection,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let mut statement = connection.prepare(
        "SELECT root_execution_id, execution_json
         FROM work_root_executions ORDER BY root_execution_id",
    )?;
    let rows = statement
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);
    for (root_execution_id, bytes) in rows {
        *checked += 1;
        let valid = serde_json::from_slice::<RootExecution>(&bytes).is_ok_and(|execution| {
            validated_required_child_waivers(connection, execution.root_id, &execution).is_ok()
        });
        if !valid {
            invalid.push(format!(
                "work_root_execution:{root_execution_id}:invalid_required_child_waivers"
            ));
        }
    }
    Ok(())
}

fn unfinished_optional_children(
    connection: &Connection,
    parent_id: WorkId,
) -> Result<Vec<WorkId>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT work_id FROM work_items
         WHERE parent_id = ?1 AND child_requirement = 'optional'
           AND lifecycle != 'completed'
         ORDER BY work_id",
    )?;
    statement
        .query_map([parent_id.0.to_string()], |row| row.get::<_, String>(0))?
        .map(|row| parse_work_id(&row?))
        .collect()
}

fn live_descendant_execution_authority(
    connection: &Connection,
    root_id: WorkId,
    now: DateTime<Utc>,
) -> Result<bool, StoreError> {
    let descendant_ids = {
        let mut statement = connection.prepare(
            "WITH RECURSIVE descendants(work_id) AS (
                 SELECT work_id FROM work_items WHERE parent_id = ?1
                 UNION
                 SELECT child.work_id FROM work_items child
                 JOIN descendants parent ON child.parent_id = parent.work_id
             )
             SELECT work_id FROM descendants ORDER BY work_id",
        )?;
        statement
            .query_map([root_id.0.to_string()], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?
    };
    for stored_id in descendant_ids {
        let item = load_work_item(connection, parse_work_id(&stored_id)?)?;
        let Some(run_id) = item.active_run_id else {
            continue;
        };
        let run = load_work_run(connection, run_id)?;
        if run.work_id != item.work_id {
            return Err(StoreError::InvalidWorkProjection(format!(
                "active run {run_id:?} belongs to a different work item"
            )));
        }
        if load_work_claim_optional(connection, run_id)?
            .is_some_and(|claim| claim.state == WorkClaimState::Active && claim.expires_at > now)
        {
            return Ok(true);
        }
        let offers = {
            let mut statement = connection.prepare(
                "SELECT offer_hash, offer_json FROM work_handoff_offers
                 WHERE run_id = ?1 ORDER BY offer_id",
            )?;
            statement
                .query_map([run_id.0.to_string()], |row| {
                    Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Vec<u8>>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?
        };
        for row in offers {
            let offer = load_handoff_offer_projection(connection, row)?;
            if offer.state == WorkHandoffState::Offered && offer.expires_at > now {
                return Ok(true);
            }
        }
    }
    Ok(false)
}

fn ancestors_admit_execution(connection: &Connection, item: &WorkItem) -> Result<bool, StoreError> {
    let mut parent_id = item.parent_id;
    let mut visited = HashSet::new();
    let mut reached_root = item.work_id == item.root_id;
    while let Some(parent) = parent_id {
        if !visited.insert(parent) || visited.len() > 1_024 {
            return Err(StoreError::InvalidWorkProjection(
                "work hierarchy is cyclic or exceeds the corruption guard".into(),
            ));
        }
        let ancestor = load_work_item(connection, parent)?;
        if ancestor.project_id != item.project_id || ancestor.root_id != item.root_id {
            return Err(StoreError::InvalidWorkProjection(format!(
                "work ancestor {:?} crosses its project or root boundary",
                ancestor.work_id
            )));
        }
        if ancestor.lifecycle != WorkLifecycle::Open {
            return Ok(false);
        }
        reached_root |= ancestor.work_id == item.root_id;
        parent_id = ancestor.parent_id;
    }
    if !reached_root {
        return Err(StoreError::InvalidWorkProjection(format!(
            "work {:?} does not reach its declared root {:?}",
            item.work_id, item.root_id
        )));
    }
    Ok(true)
}

fn work_run_uses_active_root_execution(
    connection: &Connection,
    item: &WorkItem,
) -> Result<bool, StoreError> {
    let run_id = item.active_run_id.ok_or_else(|| {
        StoreError::InvalidWorkProjection(format!("open work {:?} has no active run", item.work_id))
    })?;
    let run = load_work_run(connection, run_id)?;
    run_uses_active_root_execution(connection, item, &run)
}

fn run_uses_active_root_execution(
    connection: &Connection,
    item: &WorkItem,
    run: &WorkRun,
) -> Result<bool, StoreError> {
    if run.work_id != item.work_id {
        return Err(StoreError::InvalidWorkProjection(format!(
            "run {:?} does not belong to work {:?}",
            run.run_id, item.work_id
        )));
    }
    let Some(execution) = active_root_execution_optional(connection, item.root_id)? else {
        return Ok(false);
    };
    if execution.project_id != item.project_id || execution.root_id != item.root_id {
        return Err(StoreError::InvalidWorkProjection(format!(
            "root execution {:?} crosses the work project or root boundary",
            execution.root_execution_id
        )));
    }
    Ok(execution.root_execution_id == run.root_execution_id)
}

fn feed_head(connection: &Connection, feed: &FeedId) -> Result<i64, StoreError> {
    let (feed_kind, feed_id) = feed_parts(feed);
    Ok(connection
        .query_row(
            "SELECT position FROM work_feed_heads WHERE feed_kind = ?1 AND feed_id = ?2",
            params![feed_kind, feed_id],
            |row| row.get::<_, i64>(0),
        )
        .optional()?
        .unwrap_or(0))
}

fn refuse_completed_ancestor(connection: &Connection, item: &WorkItem) -> Result<(), StoreError> {
    let mut parent_id = item.parent_id;
    let mut depth = 0_u16;
    while let Some(parent) = parent_id {
        depth += 1;
        if depth > 1_024 {
            return Err(StoreError::InvalidWorkProjection(
                "work hierarchy depth exceeds the corruption guard".into(),
            ));
        }
        let ancestor = load_work_item(connection, parent)?;
        if ancestor.lifecycle == WorkLifecycle::Completed {
            return Err(StoreError::InvalidWork(format!(
                "cannot reopen child work while completed ancestor {:?} consumes its seal",
                ancestor.work_id
            )));
        }
        parent_id = ancestor.parent_id;
    }
    Ok(())
}

fn work_is_ancestor_of(
    connection: &Connection,
    candidate: WorkId,
    descendant: &WorkItem,
) -> Result<bool, StoreError> {
    let mut parent_id = descendant.parent_id;
    let mut depth = 0_u16;
    while let Some(parent) = parent_id {
        depth += 1;
        if depth > 1_024 {
            return Err(StoreError::InvalidWorkProjection(
                "work hierarchy depth exceeds the corruption guard".into(),
            ));
        }
        let ancestor = load_work_item(connection, parent)?;
        if ancestor.project_id != descendant.project_id || ancestor.root_id != descendant.root_id {
            return Err(StoreError::InvalidWorkProjection(format!(
                "work ancestor {:?} crosses its project or root boundary",
                ancestor.work_id
            )));
        }
        if ancestor.work_id == candidate {
            return Ok(true);
        }
        parent_id = ancestor.parent_id;
    }
    Ok(false)
}

fn combined_graph_is_acyclic(
    connection: &Connection,
    project_id: &str,
) -> Result<bool, StoreError> {
    combined_graph_is_acyclic_with_dependency(connection, project_id, None)
}

fn combined_graph_is_acyclic_with_dependency(
    connection: &Connection,
    project_id: &str,
    proposed_supersession: Option<(WorkId, WorkId)>,
) -> Result<bool, StoreError> {
    let mut graph: HashMap<WorkId, Vec<WorkId>> = HashMap::new();
    let mut statement = connection.prepare(
        "SELECT work_id, parent_id, child_requirement, superseded_by
         FROM work_items WHERE project_id = ?1",
    )?;
    let rows = statement.query_map([project_id], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<String>>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;
    for row in rows {
        let (child, parent, requirement, superseded_by) = row?;
        let child = parse_work_id(&child)?;
        graph.entry(child).or_default();
        if requirement == "required"
            && let Some(parent) = parent
        {
            graph
                .entry(parse_work_id(&parent)?)
                .or_default()
                .push(child);
        }
        if let Some(replacement) = superseded_by {
            graph
                .entry(child)
                .or_default()
                .push(parse_work_id(&replacement)?);
        }
    }
    let mut statement = connection.prepare(
        "SELECT p.work_id, p.prerequisite_id
         FROM work_prerequisites p
         JOIN work_items w ON w.work_id = p.work_id
         WHERE w.project_id = ?1",
    )?;
    let rows = statement.query_map([project_id], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        let (work, prerequisite) = row?;
        graph
            .entry(parse_work_id(&work)?)
            .or_default()
            .push(parse_work_id(&prerequisite)?);
    }
    if let Some((source, replacement)) = proposed_supersession {
        graph.entry(source).or_default().push(replacement);
        graph.entry(replacement).or_default();
    }

    let mut incoming = graph
        .keys()
        .copied()
        .map(|node| (node, 0_usize))
        .collect::<HashMap<_, _>>();
    for edges in graph.values() {
        for target in edges {
            *incoming.entry(*target).or_default() += 1;
        }
    }
    let mut ready = incoming
        .iter()
        .filter_map(|(node, count)| (*count == 0).then_some(*node))
        .collect::<Vec<_>>();
    let mut removed = 0_usize;
    while let Some(node) = ready.pop() {
        removed += 1;
        if let Some(edges) = graph.get(&node) {
            for target in edges {
                let count = incoming
                    .get_mut(target)
                    .expect("every graph target has an incoming count");
                *count -= 1;
                if *count == 0 {
                    ready.push(*target);
                }
            }
        }
    }
    Ok(removed == incoming.len())
}

fn verify_json_projection(
    connection: &Connection,
    kind: &str,
    sql: &str,
    expected: &HashMap<String, serde_json::Value>,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let mut seen = HashSet::new();
    let mut statement = connection.prepare(sql)?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    for row in rows {
        let (id, bytes) = row?;
        *checked += 1;
        seen.insert(id.clone());
        let projected = serde_json::from_slice::<serde_json::Value>(&bytes);
        if projected.as_ref().ok() != expected.get(&id) {
            invalid.push(format!("{kind}:{id}"));
        }
    }
    for id in expected.keys().filter(|id| !seen.contains(*id)) {
        invalid.push(format!("{kind}:{id}:missing"));
    }
    Ok(())
}

fn verify_prerequisite_rows(
    connection: &Connection,
    expected: &HashMap<(String, String), String>,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let mut seen = HashSet::new();
    let mut statement = connection.prepare(
        "SELECT work_id, prerequisite_id, event_hash
         FROM work_prerequisites ORDER BY work_id, prerequisite_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
        ))
    })?;
    for row in rows {
        let (work_id, prerequisite_id, event_hash) = row?;
        *checked += 1;
        let key = (work_id, prerequisite_id);
        seen.insert(key.clone());
        if expected.get(&key) != Some(&event_hash) {
            invalid.push(format!("work_prerequisite:{}:{}", key.0, key.1));
        }
    }
    for key in expected.keys().filter(|key| !seen.contains(*key)) {
        invalid.push(format!("work_prerequisite:{}:{}:missing", key.0, key.1));
    }
    drop(statement);

    let mut statement = connection.prepare("SELECT DISTINCT project_id FROM work_items")?;
    let projects = statement
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    for project in projects {
        *checked += 1;
        if !combined_graph_is_acyclic(connection, &project)? {
            invalid.push(format!("work_graph:{project}:cycle"));
        }
    }
    drop(statement);

    let mut statement = connection.prepare(
        "SELECT object_hash, canonical_json
         FROM objects
         WHERE object_kind = 'work_authority_revocation'
         ORDER BY object_hash",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
    })?;
    for row in rows {
        let (stored_hash, bytes) = row?;
        *checked += 1;
        let valid = ObjectHash::from_stored(stored_hash.clone()).is_some_and(|hash| {
            CanonicalObject::verify(&hash, bytes)
                .and_then(|object| object.decode::<WorkAuthorityRevocation>())
                .is_ok_and(|revocation| {
                    connection
                        .query_row(
                            "SELECT projection.revocation_hash,
                                    projection.revoked_at_ms,
                                    grant.revoked_at_ms
                             FROM work_authority_revocations projection
                             JOIN work_authority_grants grant
                               ON grant.grant_hash = projection.grant_hash
                             WHERE projection.grant_hash = ?1",
                            [revocation.grant.as_str()],
                            |row| {
                                Ok((
                                    row.get::<_, String>(0)?,
                                    row.get::<_, i64>(1)?,
                                    row.get::<_, Option<i64>>(2)?,
                                ))
                            },
                        )
                        .optional()
                        .is_ok_and(|binding| {
                            binding.is_some_and(|(projected_hash, row_at, grant_at)| {
                                projected_hash == stored_hash
                                    && row_at == revocation.revoked_at.timestamp_millis()
                                    && grant_at == Some(row_at)
                            })
                        })
                })
        });
        if !valid {
            invalid.push(format!(
                "work_authority_revocation:{stored_hash}:orphaned_or_mismatched"
            ));
        }
    }
    Ok(())
}

fn verify_blocker_rows(
    connection: &Connection,
    expected: &HashMap<String, (String, String, Option<String>)>,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let mut seen = HashSet::new();
    let mut statement = connection.prepare(
        "SELECT blocker_id, state, created_event_hash, cleared_event_hash
         FROM work_blockers ORDER BY blocker_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, Option<String>>(3)?,
        ))
    })?;
    for row in rows {
        let (blocker_id, state, created, cleared) = row?;
        *checked += 1;
        seen.insert(blocker_id.clone());
        if expected.get(&blocker_id) != Some(&(state, created, cleared)) {
            invalid.push(format!("work_blocker:{blocker_id}:event_binding"));
        }
    }
    for blocker_id in expected.keys().filter(|id| !seen.contains(*id)) {
        invalid.push(format!("work_blocker:{blocker_id}:missing"));
    }
    Ok(())
}

fn expected_verification_projection(
    connection: &Connection,
    evidence_hash: &ObjectHash,
) -> Result<EvidenceProjectionRow, StoreError> {
    let evidence = load_typed_work_object::<VerificationEvidence>(
        connection,
        evidence_hash,
        "verification_evidence",
    )?;
    let producer = load_typed_work_object::<ExecutionObservation>(
        connection,
        &evidence.producer_observation,
        "execution_observation",
    )?;
    let environment_matches = if let Some(environment_hash) = &evidence.environment {
        expected_environment_projection(connection, environment_hash)?;
        let environment = load_typed_work_object::<EnvironmentEvidence>(
            connection,
            environment_hash,
            "environment_evidence",
        )?;
        environment.project_id == evidence.project_id
            && environment.binding.root_execution_id == evidence.binding.root_execution_id
            && environment.binding.work_id == evidence.binding.work_id
            && environment.binding.run_id == evidence.binding.run_id
            && environment.source_basis.source_revision == evidence.source_basis.source_revision
    } else {
        true
    };
    let run_id = evidence.binding.run_id.0.to_string();
    let result_matches = matches!(
        (producer.outcome, evidence.result),
        (
            crate::domain::ExecutionOutcome::Succeeded,
            crate::domain::VerificationResult::Passed
        ) | (
            crate::domain::ExecutionOutcome::Failed,
            crate::domain::VerificationResult::Failed
        ) | (
            crate::domain::ExecutionOutcome::Unknown,
            crate::domain::VerificationResult::Indeterminate
        )
    );
    let bound = evidence.schema_version == SCHEMA_VERSION
        && producer.project_id == evidence.project_id
        && producer.binding == evidence.binding
        && producer.session_id == evidence.session_id
        && producer.source_basis.as_ref() == Some(&evidence.source_basis)
        && producer.observed_at == Some(evidence.completed_at)
        && producer.action_fingerprint == evidence.check_fingerprint
        && result_matches
        && evidence.completed_at <= evidence.recorded_at
        && producer.recorded_at <= evidence.recorded_at
        && evidence.actor.session_id.as_ref() == Some(&evidence.session_id)
        && evidence.actor.run_id.as_deref() == Some(run_id.as_str())
        && environment_matches;
    if !bound {
        return Err(StoreError::InvalidWorkProjection(format!(
            "verification evidence {evidence_hash} is not bound to its producer observation"
        )));
    }
    Ok(EvidenceProjectionRow {
        work_id: evidence.binding.work_id.0.to_string(),
        run_id,
        evidence_kind: "verification".into(),
        workspace_id: Some(evidence.source_basis.workspace_id),
        source_revision: Some(evidence.source_basis.source_revision),
        producer_session_id: Some(evidence.session_id.0),
        producer_observation_hash: Some(evidence.producer_observation.to_string()),
        check_fingerprint: Some(evidence.check_fingerprint.to_string()),
        verification_result: Some(encode_state(evidence.result)?),
        observed_at_ms: Some(evidence.completed_at.timestamp_millis()),
        environment_fingerprint: None,
        environment_evidence_hash: evidence.environment.map(|hash| hash.to_string()),
        components_json: None,
    })
}

fn expected_environment_projection(
    connection: &Connection,
    evidence_hash: &ObjectHash,
) -> Result<EvidenceProjectionRow, StoreError> {
    let evidence = load_typed_work_object::<EnvironmentEvidence>(
        connection,
        evidence_hash,
        "environment_evidence",
    )?;
    let run_id = evidence.binding.run_id.0.to_string();
    let source_text_is_valid = |value: &str| {
        let trimmed = value.trim();
        !trimmed.is_empty() && trimmed == value && value.len() <= 512
    };
    let source_basis_matches_contract = source_text_is_valid(&evidence.source_basis.workspace_id)
        && source_text_is_valid(&evidence.source_basis.source_revision);
    let components_match = if let Some(components) = &evidence.components {
        let text_is_valid = |value: &str| {
            let trimmed = value.trim();
            !trimmed.is_empty() && trimmed == value && value.len() <= 256
        };
        text_is_valid(&components.toolchain)
            && text_is_valid(&components.workspace_id)
            && components.sandbox.as_deref().is_none_or(text_is_valid)
            && components.workspace_id == evidence.source_basis.workspace_id
            && components.capability_map_revision > 0
            && CanonicalObject::freeze(components)?.hash() == &evidence.environment_fingerprint
    } else {
        true
    };
    let bound = evidence.schema_version == SCHEMA_VERSION
        && source_basis_matches_contract
        && evidence.observed_at <= evidence.recorded_at
        && evidence.actor.session_id.as_ref() == Some(&evidence.session_id)
        && evidence.actor.run_id.as_deref() == Some(run_id.as_str())
        && components_match;
    if !bound {
        return Err(StoreError::InvalidWorkProjection(format!(
            "environment evidence {evidence_hash} has an invalid run/session binding"
        )));
    }
    Ok(EvidenceProjectionRow {
        work_id: evidence.binding.work_id.0.to_string(),
        run_id,
        evidence_kind: "environment".into(),
        workspace_id: Some(evidence.source_basis.workspace_id),
        source_revision: Some(evidence.source_basis.source_revision),
        producer_session_id: Some(evidence.session_id.0),
        producer_observation_hash: None,
        check_fingerprint: None,
        verification_result: None,
        observed_at_ms: Some(evidence.observed_at.timestamp_millis()),
        environment_fingerprint: Some(evidence.environment_fingerprint.to_string()),
        environment_evidence_hash: None,
        components_json: evidence
            .components
            .as_ref()
            .map(serde_json::to_vec)
            .transpose()?,
    })
}

fn verify_evidence_rows(
    connection: &Connection,
    expected: &HashMap<String, EvidenceProjectionRow>,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let mut seen = HashSet::new();
    let mut statement = connection.prepare(
        "SELECT evidence_hash, work_id, run_id, evidence_kind,
                workspace_id, source_revision, producer_session_id,
                producer_observation_hash, check_fingerprint,
                verification_result, observed_at_ms, environment_fingerprint,
                environment_evidence_hash, components_json
         FROM work_run_evidence ORDER BY evidence_hash",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            EvidenceProjectionRow {
                work_id: row.get(1)?,
                run_id: row.get(2)?,
                evidence_kind: row.get(3)?,
                workspace_id: row.get(4)?,
                source_revision: row.get(5)?,
                producer_session_id: row.get(6)?,
                producer_observation_hash: row.get(7)?,
                check_fingerprint: row.get(8)?,
                verification_result: row.get(9)?,
                observed_at_ms: row.get(10)?,
                environment_fingerprint: row.get(11)?,
                environment_evidence_hash: row.get(12)?,
                components_json: row.get(13)?,
            },
        ))
    })?;
    for row in rows {
        let (evidence_hash, projected) = row?;
        *checked += 1;
        seen.insert(evidence_hash.clone());
        if expected.get(&evidence_hash) != Some(&projected) {
            invalid.push(format!("work_evidence:{evidence_hash}:run_binding"));
        }
    }
    for evidence_hash in expected.keys().filter(|hash| !seen.contains(*hash)) {
        invalid.push(format!("work_evidence:{evidence_hash}:missing"));
    }
    Ok(())
}

fn verify_obligation_rows(
    connection: &Connection,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let obligation_ids = connection
        .prepare("SELECT obligation_id FROM work_run_obligations ORDER BY obligation_id")?
        .query_map([], |row| row.get::<_, String>(0))?
        .collect::<Result<Vec<_>, _>>()?;
    let mut projected_definitions = HashSet::new();
    let mut projected_resolutions = HashSet::new();
    for stored_id in obligation_ids {
        *checked += 1;
        let id = uuid::Uuid::parse_str(&stored_id)
            .map(WorkObligationId)
            .map_err(|error| {
                StoreError::InvalidWorkProjection(format!(
                    "obligation projection id {stored_id:?} is invalid: {error}"
                ))
            });
        match id.and_then(|id| load_work_obligation_by_id_on(connection, id)) {
            Ok(record) => {
                projected_definitions.insert(record.definition_hash);
                if let Some(resolution) = record.resolution_hash {
                    projected_resolutions.insert(resolution);
                }
            }
            Err(_) => invalid.push(format!("work_obligation:{stored_id}")),
        }
    }
    for (kind, projected) in [
        ("work_obligation", &projected_definitions),
        ("work_obligation_resolution", &projected_resolutions),
    ] {
        let hashes = connection
            .prepare("SELECT object_hash FROM objects WHERE object_kind = ?1 ORDER BY object_hash")?
            .query_map([kind], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        for stored_hash in hashes {
            *checked += 1;
            let hash = ObjectHash::from_stored(stored_hash.clone());
            if hash.as_ref().is_none_or(|hash| !projected.contains(hash)) {
                invalid.push(format!("{kind}:{stored_hash}:missing_projection"));
            }
        }
    }
    let expected = connection
        .prepare(
            "SELECT entry.feed_id, entry.position, entry.object_hash, object.canonical_json
             FROM work_feed_entries entry
             JOIN objects object ON object.object_hash = entry.object_hash
             WHERE entry.feed_kind = 'run_execution'
               AND entry.object_kind = 'execution_observation'
               AND json_extract(object.canonical_json, '$.source_changed') = 1
             ORDER BY entry.feed_id, entry.position",
        )?
        .query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, i64>(1)?,
                row.get::<_, String>(2)?,
                row.get::<_, Vec<u8>>(3)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    for (run_id, position, stored_hash, bytes) in expected {
        *checked += 1;
        let Some(hash) = ObjectHash::from_stored(stored_hash.clone()) else {
            invalid.push(format!("work_obligation_trigger:{run_id}:{position}"));
            continue;
        };
        let Ok(observation) = CanonicalObject::verify(&hash, bytes)
            .and_then(|object| object.decode::<ExecutionObservation>())
        else {
            invalid.push(format!("work_obligation_trigger:{run_id}:{position}"));
            continue;
        };
        let Ok(rule_set) = obligation_rule_set_for_observation_on(connection, &observation) else {
            invalid.push(format!(
                "work_obligation_trigger:{run_id}:{position}:invalid_rule_set"
            ));
            continue;
        };
        for (rule, _) in crate::control::evaluate_obligation_rules(&rule_set, &observation) {
            let exists = connection.query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM work_run_obligations
                     WHERE run_id = ?1 AND rule_id = ?2 AND rule_version = ?3
                       AND triggering_observation_hash = ?4 AND trigger_position = ?5
                       AND rule_set_hash = ?6
                 )",
                params![
                    run_id,
                    rule.rule_id,
                    rule.rule_version,
                    hash.as_str(),
                    position,
                    observation.obligation_rule_set.as_str(),
                ],
                |row| row.get::<_, bool>(0),
            )?;
            if !exists {
                invalid.push(format!(
                    "work_obligation_trigger:{run_id}:{position}:missing_definition"
                ));
            }
        }
    }
    Ok(())
}

fn verify_completion_rows(
    connection: &Connection,
    expected: &HashMap<String, (String, String, String, serde_json::Value)>,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let mut seen = HashSet::new();
    let mut statement = connection.prepare(
        "SELECT seal_hash, work_id, run_id, root_execution_id, seal_json
         FROM work_completion_seals ORDER BY seal_hash",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, Vec<u8>>(4)?,
        ))
    })?;
    for row in rows {
        let (seal_hash, work_id, run_id, root_execution_id, bytes) = row?;
        *checked += 1;
        seen.insert(seal_hash.clone());
        let projected = serde_json::from_slice::<serde_json::Value>(&bytes).ok();
        let valid = expected.get(&seal_hash).is_some_and(|expected| {
            expected.0 == work_id
                && expected.1 == run_id
                && expected.2 == root_execution_id
                && projected.as_ref() == Some(&expected.3)
        });
        if !valid {
            invalid.push(format!("completion_seal:{seal_hash}:projection_binding"));
            continue;
        }
        let Ok(seal) = serde_json::from_slice::<CompletionSeal>(&bytes) else {
            invalid.push(format!("completion_seal:{seal_hash}:decode"));
            continue;
        };
        if validate_completion_seal_obligation_basis_on(connection, &seal).is_err() {
            invalid.push(format!("completion_seal:{seal_hash}:obligation_basis"));
            continue;
        }
        if validate_completion_seal_environment_basis_on(connection, &seal).is_err() {
            invalid.push(format!("completion_seal:{seal_hash}:environment_basis"));
            continue;
        }
        if validate_completion_seal_children_on(connection, &seal, 0).is_err() {
            invalid.push(format!(
                "completion_seal:{seal_hash}:child_obligation_basis"
            ));
        }
    }
    for seal_hash in expected.keys().filter(|hash| !seen.contains(*hash)) {
        invalid.push(format!("completion_seal:{seal_hash}:missing"));
    }
    Ok(())
}

fn verify_work_feed_integrity(
    connection: &Connection,
    work_items: &HashMap<String, serde_json::Value>,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let mut actual_occurrences: HashMap<String, HashSet<String>> = HashMap::new();
    let mut expected_occurrences: HashMap<String, HashSet<String>> = HashMap::new();
    let mut feed_sequences: HashMap<String, Vec<String>> = HashMap::new();
    let mut statement = connection.prepare(
        "SELECT entry.feed_kind, entry.feed_id, entry.position, entry.object_kind,
                entry.object_hash, entry.work_id, object.object_kind, object.canonical_json
         FROM work_feed_entries entry
         LEFT JOIN objects object ON object.object_hash = entry.object_hash
         ORDER BY entry.feed_kind, entry.feed_id, entry.position",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, Option<String>>(6)?,
            row.get::<_, Option<Vec<u8>>>(7)?,
        ))
    })?;
    for row in rows {
        let (
            feed_kind,
            feed_id,
            position,
            entry_kind,
            stored_hash,
            projected_work_id,
            object_kind,
            bytes,
        ) = row?;
        *checked += 1;
        let label = format!("work_feed:{feed_kind}:{feed_id}:{position}");
        let feed_key = format!("{feed_kind}:{feed_id}");
        feed_sequences
            .entry(feed_key.clone())
            .or_default()
            .push(stored_hash.clone());
        if !actual_occurrences
            .entry(stored_hash.clone())
            .or_default()
            .insert(feed_key.clone())
        {
            invalid.push(label);
            continue;
        }
        let Some(hash) = ObjectHash::from_stored(stored_hash.clone()) else {
            invalid.push(label);
            continue;
        };
        let Some(bytes) = bytes else {
            invalid.push(label);
            continue;
        };
        let Ok(object) = CanonicalObject::verify(&hash, bytes) else {
            invalid.push(label);
            continue;
        };
        if object_kind.as_deref() != Some(entry_kind.as_str()) {
            invalid.push(label);
            continue;
        }
        if entry_kind == "work_event" {
            if object
                .decode::<WorkEvent>()
                .ok()
                .map(|event| event.work_id.0.to_string())
                != projected_work_id
            {
                invalid.push(format!("{label}:work_id_binding"));
                continue;
            }
        } else if projected_work_id.is_some() {
            invalid.push(format!("{label}:unexpected_work_id_binding"));
            continue;
        }
        let expected = match entry_kind.as_str() {
            "work_event" => object
                .decode::<WorkEvent>()
                .ok()
                .map(|event| expected_work_feeds(&event.project_id.0, event.root_id, event.run_id)),
            "work_checkpoint" => object
                .decode::<WorkCheckpoint>()
                .ok()
                .and_then(|checkpoint| {
                    expected_feeds_for_work(work_items, checkpoint.work_id, Some(checkpoint.run_id))
                }),
            "work_evidence" => object.decode::<WorkEvidence>().ok().and_then(|evidence| {
                expected_feeds_for_work(work_items, evidence.work_id, Some(evidence.run_id))
            }),
            "execution_observation" => object
                .decode::<ExecutionObservation>()
                .ok()
                .map(|observation| {
                    expected_execution_observation_feeds(connection, work_items, &observation)
                })
                .transpose()?
                .flatten(),
            "verification_evidence" => object
                .decode::<VerificationEvidence>()
                .ok()
                .and_then(|evidence| {
                    expected_verification_projection(connection, &hash)
                        .ok()
                        .map(|_| evidence)
                })
                .and_then(|evidence| {
                    expected_feeds_for_work(
                        work_items,
                        evidence.binding.work_id,
                        Some(evidence.binding.run_id),
                    )
                }),
            "environment_evidence" => object
                .decode::<EnvironmentEvidence>()
                .ok()
                .and_then(|evidence| {
                    expected_environment_projection(connection, &hash)
                        .ok()
                        .map(|_| evidence)
                })
                .and_then(|evidence| {
                    expected_feeds_for_work(
                        work_items,
                        evidence.binding.work_id,
                        Some(evidence.binding.run_id),
                    )
                }),
            "work_obligation" => object
                .decode::<WorkObligation>()
                .ok()
                .and_then(|obligation| {
                    load_work_obligation_by_id_on(connection, obligation.obligation_id)
                        .ok()
                        .filter(|record| record.definition_hash == hash)
                        .map(|_| obligation)
                })
                .map(|obligation| {
                    expected_work_feeds(
                        &obligation.project_id.0,
                        obligation.root_id,
                        Some(obligation.run_id),
                    )
                }),
            "work_obligation_resolution" => object
                .decode::<WorkObligationResolutionEvent>()
                .ok()
                .and_then(|event| {
                    load_work_obligation_by_id_on(connection, event.obligation_id)
                        .ok()
                        .filter(|record| record.resolution_hash.as_ref() == Some(&hash))
                        .map(|record| record.obligation)
                })
                .map(|obligation| {
                    expected_work_feeds(
                        &obligation.project_id.0,
                        obligation.root_id,
                        Some(obligation.run_id),
                    )
                }),
            "memory_version" => object
                .decode::<MemoryVersion>()
                .ok()
                .map(|version| {
                    expected_work_memory_feeds(connection, work_items, &stored_hash, &version)
                })
                .transpose()?
                .flatten(),
            "memory_assertion_event" => object
                .decode::<MemoryAssertionEvent>()
                .ok()
                .and_then(|assertion| {
                    load_typed_work_object::<MemoryVersion>(
                        connection,
                        &assertion.version,
                        "memory_version",
                    )
                    .ok()
                    .filter(|version| version.memory_id == assertion.memory_id)
                })
                .map(|version| {
                    expected_work_memory_feeds(connection, work_items, &stored_hash, &version)
                })
                .transpose()?
                .flatten(),
            "memory_contradiction_event" => object
                .decode::<crate::domain::MemoryContradictionEvent>()
                .ok()
                .map(|event| {
                    expected_work_contradiction_feeds(connection, work_items, &stored_hash, &event)
                })
                .transpose()?
                .flatten(),
            _ => None,
        };
        let Some(expected) = expected else {
            invalid.push(format!("{label}:unsupported_or_unbound_object"));
            continue;
        };
        if !expected.contains(&feed_key) {
            invalid.push(format!("{label}:wrong_membership"));
        }
        match expected_occurrences.entry(stored_hash) {
            std::collections::hash_map::Entry::Vacant(entry) => {
                entry.insert(expected);
            }
            std::collections::hash_map::Entry::Occupied(entry) if entry.get() != &expected => {
                invalid.push(format!("{label}:inconsistent_typed_membership"));
            }
            std::collections::hash_map::Entry::Occupied(_) => {}
        }
    }
    drop(statement);

    for (hash, expected) in &expected_occurrences {
        *checked += 1;
        if actual_occurrences.get(hash) != Some(expected) {
            invalid.push(format!("work_feed_object:{hash}:occurrences"));
        }
    }
    verify_cross_feed_order(
        work_items,
        &expected_occurrences,
        &feed_sequences,
        checked,
        invalid,
    );

    let mut statement = connection.prepare(
        "SELECT head.feed_kind, head.feed_id, head.position,
                COUNT(entry.position), COALESCE(MIN(entry.position), 0),
                COALESCE(MAX(entry.position), 0)
         FROM work_feed_heads head
         LEFT JOIN work_feed_entries entry
           ON entry.feed_kind = head.feed_kind AND entry.feed_id = head.feed_id
         GROUP BY head.feed_kind, head.feed_id, head.position
         ORDER BY head.feed_kind, head.feed_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, i64>(5)?,
        ))
    })?;
    for row in rows {
        let (feed_kind, feed_id, head, count, minimum, maximum) = row?;
        *checked += 1;
        if head <= 0 || count != head || minimum != 1 || maximum != head {
            invalid.push(format!("work_feed_head:{feed_kind}:{feed_id}"));
        }
    }
    drop(statement);

    let missing_heads = connection.query_row(
        "SELECT COUNT(*) FROM work_feed_entries entry
         LEFT JOIN work_feed_heads head
           ON head.feed_kind = entry.feed_kind AND head.feed_id = entry.feed_id
         WHERE head.feed_id IS NULL",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    *checked += 1;
    if missing_heads != 0 {
        invalid.push("work_feed_entries:missing_heads".into());
    }

    let mut statement = connection.prepare(
        "SELECT object.object_kind, object.object_hash FROM objects object
         LEFT JOIN work_feed_entries entry
           ON entry.object_hash = object.object_hash
         WHERE object.object_kind IN (
             'work_event', 'work_checkpoint', 'work_evidence',
             'verification_evidence', 'environment_evidence',
             'work_obligation', 'work_obligation_resolution'
         )
           AND entry.object_hash IS NULL
         ORDER BY object.object_hash",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
    })?;
    for row in rows {
        *checked += 1;
        let (kind, hash) = row?;
        invalid.push(format!("{kind}:{hash}:missing_work_feeds"));
    }
    Ok(())
}

fn expected_work_memory_feeds(
    connection: &Connection,
    work_items: &HashMap<String, serde_json::Value>,
    object_hash: &str,
    version: &MemoryVersion,
) -> Result<Option<HashSet<String>>, StoreError> {
    let crate::domain::Scope::Work { project, work } = &version.scope else {
        return Ok(None);
    };
    let Some(item) = work_items.get(&work.0.to_string()) else {
        return Ok(None);
    };
    let Some(item_project) = item.get("project_id").and_then(serde_json::Value::as_str) else {
        return Ok(None);
    };
    let Some(root_id) = item
        .get("root_id")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| uuid::Uuid::parse_str(value).ok())
        .map(WorkId)
    else {
        return Ok(None);
    };
    if item_project != project.0 {
        return Ok(None);
    }
    let mut statement = connection.prepare(
        "SELECT feed_kind, feed_id FROM work_feed_entries
         WHERE object_hash = ?1 ORDER BY feed_kind, feed_id",
    )?;
    let feeds = statement
        .query_map([object_hash], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let mut expected = HashSet::new();
    for (kind, id) in feeds {
        let valid = match kind.as_str() {
            "project" => id == project.0,
            "root_work" => id == root_id.0.to_string(),
            "run_execution" => connection
                .query_row(
                    "SELECT work_id FROM work_runs WHERE run_id = ?1",
                    [&id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .is_some_and(|run_work| run_work == work.0.to_string()),
            _ => false,
        };
        if !valid || !expected.insert(format!("{kind}:{id}")) {
            return Ok(None);
        }
    }
    let required = HashSet::from([
        format!("project:{}", project.0),
        format!("root_work:{}", root_id.0),
    ]);
    Ok(required.is_subset(&expected).then_some(expected))
}

fn expected_work_contradiction_feeds(
    connection: &Connection,
    work_items: &HashMap<String, serde_json::Value>,
    object_hash: &str,
    event: &crate::domain::MemoryContradictionEvent,
) -> Result<Option<HashSet<String>>, StoreError> {
    let Some(root_id) = event.work_root_id else {
        return Ok(None);
    };
    let project_id = &event.project_id;
    let Some(root) = work_items.get(&root_id.0.to_string()) else {
        return Ok(None);
    };
    let root_id_text = root_id.0.to_string();
    if root.get("project_id").and_then(serde_json::Value::as_str) != Some(project_id.0.as_str())
        || root.get("root_id").and_then(serde_json::Value::as_str) != Some(root_id_text.as_str())
    {
        return Ok(None);
    }
    let feeds = {
        let mut statement = connection.prepare(
            "SELECT feed_kind, feed_id FROM work_feed_entries
             WHERE object_hash = ?1 ORDER BY feed_kind, feed_id",
        )?;
        statement
            .query_map([object_hash], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?
    };
    let mut expected = HashSet::new();
    for (kind, id) in feeds {
        let valid = match kind.as_str() {
            "project" => id == project_id.0,
            "root_work" => id == root_id_text,
            "run_execution" => connection
                .query_row(
                    "SELECT item.root_id FROM work_runs run
                     JOIN work_items item ON item.work_id = run.work_id
                     WHERE run.run_id = ?1",
                    [&id],
                    |row| row.get::<_, String>(0),
                )
                .optional()?
                .is_some_and(|run_root| run_root == root_id_text),
            _ => false,
        };
        if !valid || !expected.insert(format!("{kind}:{id}")) {
            return Ok(None);
        }
    }
    Ok((expected.contains(&format!("project:{}", project_id.0))
        && expected.contains(&format!("root_work:{}", root_id.0)))
    .then_some(expected))
}

fn expected_work_feeds(
    project_id: &str,
    root_id: WorkId,
    run_id: Option<WorkRunId>,
) -> HashSet<String> {
    let mut feeds = HashSet::from([
        format!("project:{project_id}"),
        format!("root_work:{}", root_id.0),
    ]);
    if let Some(run_id) = run_id {
        feeds.insert(format!("run_execution:{}", run_id.0));
    }
    feeds
}

fn expected_feeds_for_work(
    work_items: &HashMap<String, serde_json::Value>,
    work_id: WorkId,
    run_id: Option<WorkRunId>,
) -> Option<HashSet<String>> {
    let item = work_items.get(&work_id.0.to_string())?;
    let project_id = item.get("project_id")?.as_str()?;
    let root_id = uuid::Uuid::parse_str(item.get("root_id")?.as_str()?)
        .ok()
        .map(WorkId)?;
    Some(expected_work_feeds(project_id, root_id, run_id))
}

fn expected_execution_observation_feeds(
    connection: &Connection,
    work_items: &HashMap<String, serde_json::Value>,
    observation: &ExecutionObservation,
) -> Result<Option<HashSet<String>>, StoreError> {
    if observation.actor.session_id.as_ref() != Some(&observation.session_id)
        || observation.actor.run_id.as_deref()
            != Some(observation.binding.run_id.0.to_string().as_str())
        || observation.binding.work_revision <= 0
        || observation.binding.claim_fence <= 0
    {
        return Ok(None);
    }
    let Some(item) = work_items.get(&observation.binding.work_id.0.to_string()) else {
        return Ok(None);
    };
    if item.get("project_id").and_then(serde_json::Value::as_str)
        != Some(observation.project_id.0.as_str())
    {
        return Ok(None);
    }
    let Some(root_id) = item
        .get("root_id")
        .and_then(serde_json::Value::as_str)
        .and_then(|value| uuid::Uuid::parse_str(value).ok())
        .map(WorkId)
    else {
        return Ok(None);
    };
    let relation_matches = connection
        .query_row(
            "SELECT 1 FROM work_runs run
             JOIN work_root_executions execution
               ON execution.root_execution_id = run.root_execution_id
             WHERE run.run_id = ?1 AND run.work_id = ?2
               AND run.root_execution_id = ?3 AND execution.root_id = ?4",
            params![
                observation.binding.run_id.0.to_string(),
                observation.binding.work_id.0.to_string(),
                observation.binding.root_execution_id.0.to_string(),
                root_id.0.to_string()
            ],
            |_| Ok(()),
        )
        .optional()?
        .is_some();
    Ok(relation_matches.then(|| {
        expected_work_feeds(
            &observation.project_id.0,
            root_id,
            Some(observation.binding.run_id),
        )
    }))
}

fn verify_cross_feed_order(
    work_items: &HashMap<String, serde_json::Value>,
    expected_occurrences: &HashMap<String, HashSet<String>>,
    feed_sequences: &HashMap<String, Vec<String>>,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) {
    for (feed, sequence) in feed_sequences {
        let parent_feed = if let Some(root_id) = feed.strip_prefix("root_work:") {
            work_items.get(root_id).and_then(|item| {
                item.get("project_id")
                    .and_then(serde_json::Value::as_str)
                    .map(|project| format!("project:{project}"))
            })
        } else if feed.starts_with("run_execution:") {
            sequence.iter().find_map(|hash| {
                expected_occurrences.get(hash).and_then(|feeds| {
                    feeds
                        .iter()
                        .find(|candidate| candidate.starts_with("root_work:"))
                        .cloned()
                })
            })
        } else {
            None
        };
        let Some(parent_feed) = parent_feed else {
            continue;
        };
        *checked += 1;
        let parent_projection = feed_sequences
            .get(&parent_feed)
            .into_iter()
            .flatten()
            .filter(|hash| {
                expected_occurrences
                    .get(*hash)
                    .is_some_and(|feeds| feeds.contains(feed))
            })
            .collect::<Vec<_>>();
        if parent_projection != sequence.iter().collect::<Vec<_>>() {
            invalid.push(format!("work_feed_order:{feed}"));
        }
    }
}

fn verify_work_catalog_projections(
    connection: &Connection,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let mut statement = connection.prepare(
        "SELECT work_id, item_json, assigned_to_key, search_text_key
         FROM work_items ORDER BY work_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Vec<u8>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, String>(3)?,
        ))
    })?;
    for row in rows {
        let (work_id, item_json, assigned_to_key, search_text_key) = row?;
        *checked += 1;
        let Ok(item) = serde_json::from_slice::<WorkItem>(&item_json) else {
            invalid.push(format!("work_catalog:{work_id}:item_decode"));
            continue;
        };
        let expected_assigned = item.assigned_to.as_deref().map(normalize_work_catalog_key);
        let expected_search = work_catalog_search_text(connection, &item)?;
        let mut expected_labels = item
            .labels
            .iter()
            .map(|label| normalize_work_catalog_key(label))
            .collect::<Vec<_>>();
        expected_labels.sort();
        expected_labels.dedup();
        let mut label_statement = connection.prepare(
            "SELECT label_key FROM work_item_labels
             WHERE work_id = ?1 ORDER BY label_key",
        )?;
        let actual_labels = label_statement
            .query_map([work_id.as_str()], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let mut fts_statement =
            connection.prepare("SELECT search_text FROM work_catalog_fts WHERE work_id = ?1")?;
        let fts_rows = fts_statement
            .query_map([work_id.as_str()], |row| row.get::<_, String>(0))?
            .collect::<Result<Vec<_>, _>>()?;
        let fts_index_valid = connection
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM work_catalog_fts
                     WHERE work_id = ?1 AND work_catalog_fts MATCH ?2
                 )",
                params![
                    work_id.as_str(),
                    catalog_literal_fts_query(&expected_search)
                ],
                |row| row.get::<_, bool>(0),
            )
            .unwrap_or(false);
        if item.work_id.0.to_string() != work_id
            || assigned_to_key != expected_assigned
            || search_text_key != expected_search
            || actual_labels != expected_labels
            || fts_rows.len() != 1
            || fts_rows.first() != Some(&expected_search)
        {
            invalid.push(format!("work_catalog:{work_id}:projection_binding"));
        }
        if !fts_index_valid {
            invalid.push(format!("work_catalog:{work_id}:fts_index"));
        }
    }
    drop(statement);
    let orphaned_fts = connection.query_row(
        "SELECT COUNT(*) FROM work_catalog_fts catalog
         WHERE NOT EXISTS (
             SELECT 1 FROM work_items item WHERE item.work_id = catalog.work_id
         )",
        [],
        |row| row.get::<_, i64>(0),
    )?;
    if orphaned_fts != 0 {
        invalid.push("work_catalog:orphaned_fts_rows".into());
    }
    Ok(())
}

fn verify_work_scalar_bindings(
    connection: &Connection,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let checks = [
        (
            "work_item",
            "SELECT item.work_id FROM work_items item WHERE
             work_id != json_extract(item_json, '$.work_id') OR
             project_id != json_extract(item_json, '$.project_id') OR
             short_ref != json_extract(item_json, '$.short_ref') OR
             root_id != json_extract(item_json, '$.root_id') OR
             COALESCE(parent_id, '') != COALESCE(json_extract(item_json, '$.parent_id'), '') OR
             child_requirement != json_extract(item_json, '$.child_requirement') OR
             lifecycle != json_extract(item_json, '$.lifecycle') OR
             priority != json_extract(item_json, '$.priority') OR
             COALESCE(assigned_to, '') != COALESCE(json_extract(item_json, '$.assigned_to'), '') OR
             revision != json_extract(item_json, '$.revision') OR
             COALESCE(active_run_id, '') != COALESCE(json_extract(item_json, '$.active_run_id'), '') OR
             COALESCE(source_snapshot_hash, '') != COALESCE(json_extract(item_json, '$.source_snapshot_id'), '')",
        ),
        (
            "work_run",
            "SELECT run_id FROM work_runs WHERE
             run_id != json_extract(run_json, '$.run_id') OR
             root_execution_id != json_extract(run_json, '$.root_execution_id') OR
             work_id != json_extract(run_json, '$.work_id') OR
             generation != json_extract(run_json, '$.generation') OR
             COALESCE(executor_session_id, '') != COALESCE(json_extract(run_json, '$.executor'), '') OR
             state != json_extract(run_json, '$.state') OR
             revision != json_extract(run_json, '$.revision') OR
             COALESCE(last_checkpoint_hash, '') != COALESCE(json_extract(run_json, '$.last_checkpoint'), '') OR
             COALESCE(completion_seal_hash, '') != COALESCE(json_extract(run_json, '$.completion_seal'), '')",
        ),
        (
            "work_root_execution",
            "SELECT root_execution_id FROM work_root_executions WHERE
             root_execution_id != json_extract(execution_json, '$.root_execution_id') OR
             project_id != json_extract(execution_json, '$.project_id') OR
             root_id != json_extract(execution_json, '$.root_id') OR
             generation != json_extract(execution_json, '$.generation') OR
             state != json_extract(execution_json, '$.state') OR
             revision != json_extract(execution_json, '$.revision')",
        ),
        (
            "work_claim",
            "SELECT run_id FROM work_claims WHERE
             run_id != json_extract(claim_json, '$.run_id') OR
             work_id != json_extract(claim_json, '$.work_id') OR
             claim_id != json_extract(claim_json, '$.claim_id') OR
             holder_session_id != json_extract(claim_json, '$.holder') OR
             state != json_extract(claim_json, '$.state') OR
             revision != json_extract(claim_json, '$.revision') OR
             fence != json_extract(claim_json, '$.fence')",
        ),
        (
            "work_handoff_offer",
            "SELECT offer_id FROM work_handoff_offers WHERE
             offer_hash IS NULL OR
             offer_id != json_extract(offer_json, '$.offer_id') OR
             run_id != json_extract(offer_json, '$.run_id') OR
             work_id != json_extract(offer_json, '$.work_id') OR
             state != json_extract(offer_json, '$.state')",
        ),
        (
            "work_blocker",
            "SELECT blocker_id FROM work_blockers WHERE
             blocker_id != json_extract(blocker_json, '$.blocker_id') OR
             work_id != json_extract(blocker_json, '$.work_id')",
        ),
    ];
    for (kind, sql) in checks {
        let mut statement = connection.prepare(sql)?;
        let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
        for row in rows {
            *checked += 1;
            invalid.push(format!("{kind}:{}:scalar_binding", row?));
        }
    }

    let mut statement = connection.prepare(
        "SELECT item.work_id FROM work_items item WHERE
         COALESCE(item.latest_event_hash, '') != COALESCE((
             SELECT entry.object_hash FROM work_feed_entries entry
             WHERE entry.feed_kind = 'project'
               AND entry.object_kind = 'work_event'
               AND entry.work_id = item.work_id
             ORDER BY entry.position DESC LIMIT 1
         ), '')",
    )?;
    let rows = statement.query_map([], |row| row.get::<_, String>(0))?;
    for row in rows {
        *checked += 1;
        invalid.push(format!("work_item:{}:latest_event_hash", row?));
    }
    drop(statement);

    let mut statement = connection.prepare(
        "SELECT work_id, deferred_until_ms, superseded_by, created_at_ms, updated_at_ms, item_json
         FROM work_items ORDER BY work_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<i64>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, Vec<u8>>(5)?,
        ))
    })?;
    for row in rows {
        let (id, deferred, superseded_by, created_at, updated_at, bytes) = row?;
        *checked += 1;
        let valid = serde_json::from_slice::<WorkItem>(&bytes).is_ok_and(|item| {
            deferred == item.deferred_until.map(|value| value.timestamp_millis())
                && superseded_by == item.superseded_by.map(|value| value.0.to_string())
                && created_at == item.created_at.timestamp_millis()
                && updated_at == item.updated_at.timestamp_millis()
        });
        if !valid {
            invalid.push(format!("work_item:{id}:extended_scalar_binding"));
        }
    }
    drop(statement);

    let mut statement = connection.prepare(
        "SELECT run.run_id, run.claim_fence_head, claim.fence,
                run.created_at_ms, run.updated_at_ms, run.run_json
         FROM work_runs run
         LEFT JOIN work_claims claim ON claim.run_id = run.run_id
         ORDER BY run.run_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, Option<i64>>(2)?,
            row.get::<_, i64>(3)?,
            row.get::<_, i64>(4)?,
            row.get::<_, Vec<u8>>(5)?,
        ))
    })?;
    for row in rows {
        let (id, fence_head, claim_fence, created_at, updated_at, bytes) = row?;
        *checked += 1;
        let valid = serde_json::from_slice::<WorkRun>(&bytes).is_ok_and(|run| {
            fence_head == claim_fence.unwrap_or(0)
                && created_at == run.created_at.timestamp_millis()
                && updated_at == run.updated_at.timestamp_millis()
        });
        if !valid {
            invalid.push(format!("work_run:{id}:extended_scalar_binding"));
        }
    }
    drop(statement);

    let mut statement = connection.prepare(
        "SELECT root_execution_id, created_at_ms, updated_at_ms, execution_json
         FROM work_root_executions ORDER BY root_execution_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, i64>(2)?,
            row.get::<_, Vec<u8>>(3)?,
        ))
    })?;
    for row in rows {
        let (id, created_at, updated_at, bytes) = row?;
        *checked += 1;
        let valid = serde_json::from_slice::<RootExecution>(&bytes).is_ok_and(|execution| {
            created_at == execution.created_at.timestamp_millis()
                && updated_at == execution.updated_at.timestamp_millis()
        });
        if !valid {
            invalid.push(format!("work_root_execution:{id}:extended_scalar_binding"));
        }
    }
    drop(statement);

    let mut statement = connection
        .prepare("SELECT run_id, expires_at_ms, claim_json FROM work_claims ORDER BY run_id")?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, Vec<u8>>(2)?,
        ))
    })?;
    for row in rows {
        let (id, expires_at, bytes) = row?;
        *checked += 1;
        let valid = serde_json::from_slice::<WorkClaim>(&bytes)
            .is_ok_and(|claim| expires_at == claim.expires_at.timestamp_millis());
        if !valid {
            invalid.push(format!("work_claim:{id}:extended_scalar_binding"));
        }
    }
    drop(statement);

    let mut statement = connection.prepare(
        "SELECT offer_id, expires_at_ms, offer_json
         FROM work_handoff_offers ORDER BY offer_id",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, i64>(1)?,
            row.get::<_, Vec<u8>>(2)?,
        ))
    })?;
    for row in rows {
        let (id, expires_at, bytes) = row?;
        *checked += 1;
        let valid = serde_json::from_slice::<WorkHandoffOffer>(&bytes)
            .is_ok_and(|offer| expires_at == offer.expires_at.timestamp_millis());
        if !valid {
            invalid.push(format!("work_handoff_offer:{id}:extended_scalar_binding"));
        }
    }
    Ok(())
}

fn verify_canonical_work_rows(
    connection: &Connection,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let projections = [
        (
            "work_authority_grant",
            "SELECT projection.grant_hash, projection.grant_json,
                    object.object_kind, object.canonical_json
             FROM work_authority_grants projection
             LEFT JOIN objects object ON object.object_hash = projection.grant_hash
             ORDER BY projection.grant_hash",
            "work_authority_grant",
        ),
        (
            "completion_seal",
            "SELECT projection.seal_hash, projection.seal_json,
                    object.object_kind, object.canonical_json
             FROM work_completion_seals projection
             LEFT JOIN objects object ON object.object_hash = projection.seal_hash
             ORDER BY projection.seal_hash",
            "completion_seal",
        ),
        (
            "work_authority_revocation",
            "SELECT projection.revocation_hash, projection.revocation_json,
                    object.object_kind, object.canonical_json
             FROM work_authority_revocations projection
             LEFT JOIN objects object ON object.object_hash = projection.revocation_hash
             ORDER BY projection.revocation_hash",
            "work_authority_revocation",
        ),
        (
            "work_handoff_offer",
            "SELECT projection.offer_hash, projection.offer_json,
                    object.object_kind, object.canonical_json
             FROM work_handoff_offers projection
             LEFT JOIN objects object ON object.object_hash = projection.offer_hash
             ORDER BY projection.offer_id",
            "work_handoff_offer",
        ),
    ];
    for (label, sql, expected_kind) in projections {
        let mut statement = connection.prepare(sql)?;
        let rows = statement.query_map([], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Vec<u8>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<Vec<u8>>>(3)?,
            ))
        })?;
        for row in rows {
            let (stored_hash, projection, object_kind, canonical) = row?;
            *checked += 1;
            let valid = match (
                ObjectHash::from_stored(stored_hash.clone()),
                canonical.as_ref(),
            ) {
                (Some(hash), Some(bytes)) => {
                    CanonicalObject::verify(&hash, bytes.clone()).is_ok()
                        && object_kind.as_deref() == Some(expected_kind)
                        && serde_json::from_slice::<serde_json::Value>(&projection).ok()
                            == serde_json::from_slice::<serde_json::Value>(bytes).ok()
                }
                _ => false,
            };
            if !valid {
                invalid.push(format!("{label}:{stored_hash}"));
            }
        }
    }
    Ok(())
}

fn verify_authority_revocation_bindings(
    connection: &Connection,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let mut statement = connection.prepare(
        "SELECT grant.grant_hash, grant.revoked_at_ms,
                revocation.revocation_hash, revocation.revoked_at_ms,
                revocation.revocation_json
         FROM work_authority_grants grant
         LEFT JOIN work_authority_revocations revocation
           ON revocation.grant_hash = grant.grant_hash
         ORDER BY grant.grant_hash",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, Option<i64>>(1)?,
            row.get::<_, Option<String>>(2)?,
            row.get::<_, Option<i64>>(3)?,
            row.get::<_, Option<Vec<u8>>>(4)?,
        ))
    })?;
    for row in rows {
        let (grant_hash, projected_at, revocation_hash, revocation_at, bytes) = row?;
        *checked += 1;
        let valid = match (projected_at, revocation_hash, revocation_at, bytes) {
            (None, None, None, None) => true,
            (Some(projected_at), Some(stored_hash), Some(row_at), Some(bytes))
                if projected_at == row_at =>
            {
                ObjectHash::from_stored(stored_hash).is_some_and(|hash| {
                    CanonicalObject::verify(&hash, bytes)
                        .and_then(|object| object.decode::<WorkAuthorityRevocation>())
                        .is_ok_and(|revocation| {
                            revocation.grant.as_str() == grant_hash
                                && revocation.revoked_at.timestamp_millis() == row_at
                        })
                })
            }
            _ => false,
        };
        if !valid {
            invalid.push(format!(
                "work_authority_grant:{grant_hash}:revocation_binding"
            ));
        }
    }
    Ok(())
}

fn verify_work_protocol_attempts(
    connection: &Connection,
    checked: &mut usize,
    invalid: &mut Vec<String>,
) -> Result<(), StoreError> {
    let mut statement = connection.prepare(
        "SELECT project_id, session_id, operation, idempotency_key,
                request_hash, basis_hash, basis_json, result_hash, result_json
         FROM work_protocol_attempts
         ORDER BY project_id, session_id, operation, idempotency_key",
    )?;
    let rows = statement.query_map([], |row| {
        Ok((
            row.get::<_, String>(0)?,
            row.get::<_, String>(1)?,
            row.get::<_, String>(2)?,
            row.get::<_, String>(3)?,
            row.get::<_, String>(4)?,
            row.get::<_, Option<String>>(5)?,
            row.get::<_, Option<Vec<u8>>>(6)?,
            row.get::<_, Option<String>>(7)?,
            row.get::<_, Option<Vec<u8>>>(8)?,
        ))
    })?;
    for row in rows {
        let (
            project_id,
            session_id,
            operation,
            key,
            request_hash,
            basis_hash,
            basis_json,
            result_hash,
            result_json,
        ) = row?;
        *checked += 1;
        let label = format!("work_protocol_attempt:{project_id}:{session_id}:{operation}:{key}");
        let request_valid = ObjectHash::from_stored(request_hash).is_some();
        let basis_valid = match (&basis_hash, &basis_json, &result_hash, &result_json) {
            (Some(stored_hash), Some(bytes), None, None) => {
                ObjectHash::from_stored(stored_hash.clone())
                    .is_some_and(|hash| CanonicalObject::verify(&hash, bytes.clone()).is_ok())
            }
            (stored_hash, None, Some(_), Some(_)) => stored_hash
                .as_ref()
                .is_none_or(|hash| ObjectHash::from_stored(hash.clone()).is_some()),
            _ => false,
        };
        let result_valid = match (result_hash, result_json) {
            (None, None) => true,
            (Some(stored_hash), Some(bytes)) => ObjectHash::from_stored(stored_hash)
                .and_then(|hash| {
                    load_typed_work_object::<serde_json::Value>(
                        connection,
                        &hash,
                        "work_protocol_result",
                    )
                    .ok()
                    .map(|value| (hash, value))
                })
                .is_some_and(|(hash, value)| {
                    CanonicalObject::freeze(&value).is_ok_and(|object| {
                        object.hash() == &hash
                            && object.bytes() == bytes
                            && validate_work_protocol_result_binding(
                                connection,
                                &project_id,
                                &operation,
                                &value,
                            )
                            .is_ok()
                    })
                }),
            _ => false,
        };
        if !request_valid || !basis_valid || !result_valid {
            invalid.push(label);
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};

    use super::*;
    use crate::domain::{
        ActorContext, AssuranceLevel, CheckpointWorkRequest, ChildWorkDraft, ChildWorkPrerequisite,
        CompleteWorkRequest, ControlAssurance, ControlRefusalCode, ControlTurnBeginDecision,
        ControlTurnCheckpointDecision, ControlTurnDecision, CreateWorkRequest,
        DecomposeWorkRequest, DisposeWorkRequest, EffectClass, EnvironmentComponents,
        EnvironmentEvidenceInput, EnvironmentEvidenceReference, ExecutionObservationInput,
        ExecutionObservationReference, ExecutionOutcome, ExecutionSourceBasis,
        LifecycleAuthorityDecision, NoteRequest, NoteVisibility, ProvenanceLink,
        RecordWorkEvidenceRequest, ReopenWorkRequest, Scope, Sensitivity, TurnIntent,
        TurnNextIntent, TurnPurpose, VerificationEvidenceInput, VerificationEvidenceMismatch,
        VerificationKind, VerificationResult, WaiveRequiredChildRequest, WorkDependencyRef,
        WorkItemKind, WorkPlanningAuthority, WorkPlanningBudget, WorkRevisionPatch,
    };
    use crate::memory::DevelopmentNoopRedactor;
    use crate::storage::test_database_shape_snapshot;
    use crate::work_service::{
        LocalWorkService, WorkAcceptanceInput, WorkCompleteInput, WorkCompleteResult,
    };
    use crate::{ProjectId, VerificationEvidenceMatchInput, match_verification_evidence};

    fn at(second: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 27, 1, 0, 0)
            .single()
            .expect("fixed test timestamp")
            + Duration::seconds(second)
    }

    fn builtin_rule_set_hash() -> ObjectHash {
        CanonicalObject::freeze(&crate::control::builtin_obligation_rule_set())
            .expect("canonical built-in obligation rule set")
            .hash()
            .clone()
    }

    #[test]
    fn revision_kind_and_label_deltas_preserve_unmentioned_labels() {
        let project = "revision-metadata";
        let mut store = SqliteStore::open_in_memory().expect("metadata fixture");
        install_grant(&mut store, project, "planner");
        let mut request = root_request(project, "metadata-root", 1);
        request.labels = (0..12).map(|index| format!("label-{index:02}")).collect();
        request.labels.push("Straße".into());
        let root = store
            .create_work(&request, &DevelopmentNoopRedactor)
            .expect("metadata root");

        let revised = store
            .revise_work(
                &ReviseWorkRequest {
                    work_id: root.work_id,
                    expected_revision: root.revision,
                    patch: WorkRevisionPatch {
                        kind: Some(WorkItemKind::Bug),
                        add_labels: vec!["phoenix".into()],
                        remove_labels: vec!["LABEL-00".into(), "STRASSE".into()],
                        ..WorkRevisionPatch::default()
                    },
                    authority: delegated(project, "planner"),
                    actor: actor("planner"),
                    idempotency_key: "revise-metadata".into(),
                    updated_at: at(2),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("revise metadata");

        assert_eq!(revised.kind, WorkItemKind::Bug);
        assert_eq!(revised.labels.len(), 12);
        assert!(!revised.labels.iter().any(|label| label == "label-00"));
        assert!(!revised.labels.iter().any(|label| label == "Straße"));
        assert!(revised.labels.iter().any(|label| label == "label-11"));
        assert!(revised.labels.iter().any(|label| label == "phoenix"));
        let latest = latest_canonical_work_event_for_item(&store.connection, root.work_id)
            .expect("latest revised event");
        assert_eq!(latest.work.kind, WorkItemKind::Bug);
        assert_eq!(latest.work.labels, revised.labels);
        assert!(
            store
                .verify_all()
                .expect("metadata integrity")
                .invalid_work_records
                .is_empty()
        );
    }

    #[test]
    fn latest_gate_observation_uses_indexed_canonical_run_history() {
        let project = "canonical-gate-history";
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, project, "planner");
        let work = store
            .create_work(
                &root_request(project, "canonical-gate-root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("gate work");
        let claim = claim(
            &mut store,
            &work,
            "gate-agent",
            "canonical-gate-claim",
            1,
            10_000,
        );
        let mut hashes = Vec::new();
        for index in 0..128_i64 {
            let failed = (index % 2 != 0).then(|| vec!["cargo-test".into()]);
            hashes.push(
                store
                    .record_gate_evidence(
                        &RecordGateEvidenceRequest {
                            work_id: work.work_id,
                            run_id: claim.run_id,
                            expected_work_revision: work.revision,
                            holder: claim.holder.clone(),
                            claim_id: claim.claim_id,
                            claim_fence: claim.fence,
                            name: "cargo-test".into(),
                            failed: failed.unwrap_or_default(),
                            evidence_ref: None,
                            actor: actor("gate-agent"),
                            recorded_at: at(index + 2),
                        },
                        &DevelopmentNoopRedactor,
                    )
                    .expect("alternating gate observation"),
            );
        }
        assert!(hashes.windows(2).all(|pair| pair[0] != pair[1]));
        for (index, hash) in hashes.iter().enumerate() {
            let evidence = store
                .get::<WorkEvidence>(hash)
                .expect("gate evidence read")
                .expect("gate evidence");
            assert_eq!(
                evidence.gate.expect("typed gate").previous.as_ref(),
                index.checked_sub(1).map(|previous| &hashes[previous])
            );
        }
        let latest = latest_gate_evidence_on(&store.connection, claim.run_id, "cargo-test")
            .expect("latest canonical gate observation")
            .expect("gate observation");
        assert_eq!(latest.0, *hashes.last().expect("last gate hash"));

        let explain = format!("EXPLAIN QUERY PLAN {LATEST_GATE_EVIDENCE_SQL}");
        let mut statement = store
            .connection
            .prepare(&explain)
            .expect("prepare gate lookup plan");
        let plan = statement
            .query_map(params![claim.run_id.0.to_string(), "cargo-test"], |row| {
                row.get::<_, String>(3)
            })
            .expect("explain gate lookup")
            .collect::<Result<Vec<_>, _>>()
            .expect("collect gate lookup plan");
        assert!(
            plan.iter().any(|detail| {
                detail.contains("SEARCH object USING INDEX objects_work_evidence_gate_name")
            }),
            "gate lookup does not search its expression index: {plan:?}"
        );
        assert!(
            plan.iter().all(|detail| !detail.contains("SCAN ")),
            "gate lookup contains an unbounded scan: {plan:?}"
        );
        assert!(
            plan.iter()
                .all(|detail| !detail.contains("USE TEMP B-TREE")),
            "gate lookup sorts through a temporary B-tree: {plan:?}"
        );
        assert!(
            store
                .verify_all()
                .expect("gate history integrity")
                .is_healthy()
        );
        let mutable_head_exists = store
            .connection
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM sqlite_master
                     WHERE name = 'work_gate_evidence_heads'
                 )",
                [],
                |row| row.get::<_, bool>(0),
            )
            .expect("gate head absence");
        assert!(!mutable_head_exists);
        let expression_index_exists = store
            .connection
            .query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM sqlite_master
                     WHERE name = 'objects_work_evidence_gate_name'
                       AND type = 'index'
                 )",
                [],
                |row| row.get::<_, bool>(0),
            )
            .expect("gate expression index");
        assert!(expression_index_exists);
    }

    #[test]
    fn gate_replay_never_reuses_another_actors_attribution() {
        let project = "gate-actor-attribution";
        let holder = "shared-session";
        let mut store = SqliteStore::open_in_memory().expect("gate actor fixture");
        install_grant(&mut store, project, "planner");
        let work = store
            .create_work(
                &root_request(project, "gate-actor-root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("gate actor root");
        let claim = claim(&mut store, &work, holder, "gate-actor-claim", 1, 300);
        let project_id = crate::domain::ProjectId(project.into());
        let session_id = SessionId(holder.into());
        let basis = serde_json::json!({"test_basis": work.work_id});
        let actor_a = actor(holder);
        let failed = Vec::<String>::new();
        let refs = Vec::<String>::new();
        let pending_intent = GateWorkProtocolIntent {
            schema_version: SCHEMA_VERSION,
            project_id: &project_id,
            session_id: &session_id,
            actor: &actor_a,
            work_id: work.work_id,
            run_id: claim.run_id,
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
            name: "cargo-test",
            failed: &failed,
            refs: &refs,
            previous: None,
        };
        let pending_object = CanonicalObject::freeze(&pending_intent).expect("actor A intent");
        let actor_a_key = format!("gate:{}", pending_object.hash().as_str());
        assert!(
            store
                .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
                    project_id: &project_id,
                    session_id: &session_id,
                    operation: "work_update:gate",
                    idempotency_key: &actor_a_key,
                    intent: &pending_intent,
                    basis: &basis,
                    now: at(2),
                })
                .expect("reserve actor A attempt")
                .result
                .is_none()
        );

        let mut actor_b = actor(holder);
        actor_b.actor_id = "different-actor".into();
        actor_b.source_tool = Some("different-tool".into());
        let actor_b_request = RecordGateEvidenceRequest {
            work_id: work.work_id,
            run_id: claim.run_id,
            expected_work_revision: work.revision,
            holder: session_id.clone(),
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
            name: "cargo-test".into(),
            failed: Vec::new(),
            evidence_ref: None,
            actor: actor_b.clone(),
            recorded_at: at(2),
        };
        let actor_b_attempt = store
            .record_gate_evidence_protocol(
                &actor_b_request,
                &BeginGateWorkProtocolAttempt {
                    project_id: &project_id,
                    session_id: &session_id,
                    basis: &basis,
                    now: at(2),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("actor B records its own gate observation");
        assert_ne!(actor_b_attempt.idempotency_key, actor_a_key);
        let different_attribution = store
            .get::<WorkEvidence>(&actor_b_attempt.evidence)
            .expect("actor B evidence read")
            .expect("actor B evidence");
        assert_eq!(different_attribution.actor, actor_b);
        assert_eq!(
            different_attribution
                .gate
                .as_ref()
                .expect("actor B gate")
                .previous,
            None
        );

        let actor_a_hash = store
            .record_gate_evidence(
                &RecordGateEvidenceRequest {
                    actor: actor_a.clone(),
                    recorded_at: at(3),
                    ..actor_b_request
                },
                &DevelopmentNoopRedactor,
            )
            .expect("actor A records a distinct attributed observation");
        assert_ne!(actor_a_hash, actor_b_attempt.evidence);
        let original_attribution = store
            .get::<WorkEvidence>(&actor_a_hash)
            .expect("actor A evidence read")
            .expect("actor A evidence");
        assert_eq!(original_attribution.actor, actor_a);
        assert_eq!(
            original_attribution
                .gate
                .expect("actor A gate")
                .previous
                .as_ref(),
            Some(&actor_b_attempt.evidence)
        );
    }

    #[test]
    fn completed_gate_attempt_mismatch_refuses_before_appending() {
        let project = "completed-gate-attempt-mismatch";
        let holder = "gate-holder";
        let mut store = SqliteStore::open_in_memory().expect("gate attempt fixture");
        install_grant(&mut store, project, "planner");
        let work = store
            .create_work(
                &root_request(project, "gate-attempt-root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("gate attempt root");
        let claim = claim(&mut store, &work, holder, "gate-attempt-claim", 1, 300);
        let pass = RecordGateEvidenceRequest {
            work_id: work.work_id,
            run_id: claim.run_id,
            expected_work_revision: work.revision,
            holder: SessionId(holder.into()),
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
            name: "cargo-test".into(),
            failed: Vec::new(),
            evidence_ref: None,
            actor: actor(holder),
            recorded_at: at(2),
        };
        let pass_hash = store
            .record_gate_evidence(&pass, &DevelopmentNoopRedactor)
            .expect("initial gate observation");
        let failed = vec!["suite::failure".into()];
        let request = RecordGateEvidenceRequest {
            failed: failed.clone(),
            recorded_at: at(3),
            ..pass
        };
        let project_id = crate::domain::ProjectId(project.into());
        let session_id = SessionId(holder.into());
        let basis = serde_json::json!({"test_basis": work.work_id});
        let intent = GateWorkProtocolIntent {
            schema_version: SCHEMA_VERSION,
            project_id: &project_id,
            session_id: &session_id,
            actor: &request.actor,
            work_id: work.work_id,
            run_id: claim.run_id,
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
            name: "cargo-test",
            failed: &failed,
            refs: &[],
            previous: Some(&pass_hash),
        };
        let intent_object = CanonicalObject::freeze(&intent).expect("gate intent");
        let idempotency_key = format!("gate:{}", intent_object.hash().as_str());
        assert!(
            store
                .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
                    project_id: &project_id,
                    session_id: &session_id,
                    operation: "work_update:gate",
                    idempotency_key: &idempotency_key,
                    intent: &intent,
                    basis: &basis,
                    now: at(3),
                })
                .expect("reserve synthetic completed attempt")
                .result
                .is_none()
        );
        store
            .finish_work_protocol_attempt(
                &project_id,
                &session_id,
                "work_update:gate",
                &idempotency_key,
                &serde_json::json!({"receipt": {"work_id": work.work_id}}),
            )
            .expect("complete synthetic attempt without evidence");

        assert!(matches!(
            store
                .record_gate_evidence_protocol(
                    &request,
                    &BeginGateWorkProtocolAttempt {
                        project_id: &project_id,
                        session_id: &session_id,
                        basis: &basis,
                        now: at(3),
                    },
                    &DevelopmentNoopRedactor,
                )
                .expect_err("a completed attempt cannot append disagreeing evidence"),
            StoreError::InvalidWorkProjection(detail)
                if detail.contains("disagrees with the latest same-name evidence")
        ));
        assert_eq!(
            store
                .work_run_evidence(claim.run_id)
                .expect("bounded gate evidence history"),
            vec![pass_hash]
        );
    }

    #[test]
    fn resolver_sql_bounds_collisions_and_recovers_an_omitted_target_by_full_id() {
        let mut store = SqliteStore::open_in_memory().expect("collision fixture");
        install_grant(&mut store, "ambiguous-project", "planner");
        let mut items = (0..9)
            .map(|index| {
                store
                    .create_work(
                        &root_request("ambiguous-project", &format!("candidate-{index}"), index),
                        &DevelopmentNoopRedactor,
                    )
                    .expect("candidate")
            })
            .collect::<Vec<_>>();

        // Production enforces uniqueness. This deliberately corrupt-shaped
        // fixture replaces the constrained table with an unconstrained copy so
        // the real resolver SQL and canonical projection loader exercise the
        // defensive ambiguity path.
        store
            .connection
            .execute_batch(
                "PRAGMA foreign_keys = OFF;
                 CREATE TABLE work_items_collision AS SELECT * FROM work_items;
                 DROP TABLE work_items;
                 ALTER TABLE work_items_collision RENAME TO work_items;",
            )
            .expect("remove short-ref uniqueness for collision fixture");
        let transaction = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("collision transaction");
        for (index, item) in items.iter_mut().enumerate() {
            let mut event = latest_canonical_work_event_for_item(&transaction, item.work_id)
                .expect("latest candidate event");
            item.short_ref = "w-collision".into();
            item.title = format!("Collision candidate {index}");
            item.revision += 1;
            item.updated_at = at(10 + i64::try_from(index).expect("small index"));
            transaction
                .execute(
                    "UPDATE work_items SET short_ref = ?2 WHERE work_id = ?1",
                    params![item.work_id.0.to_string(), item.short_ref],
                )
                .expect("persist colliding short ref");
            persist_work_item(&transaction, item).expect("persist colliding candidate");
            event.revision = item.revision;
            event.work.clone_from(item);
            event.created_at = item.updated_at;
            append_work_event(&transaction, &WorkEventDraft::from(&event))
                .expect("append colliding candidate event");
        }
        transaction.commit().expect("commit collision fixture");
        store
            .connection
            .execute_batch("PRAGMA foreign_keys = ON;")
            .expect("restore foreign-key enforcement after collision fixture");

        let error = store
            .resolve_work_ref(&ProjectId("ambiguous-project".into()), "w-collision")
            .expect_err("nine matching rows must be ambiguous");
        let StoreError::WorkReferenceAmbiguous {
            reference,
            candidates,
            more,
        } = error
        else {
            panic!("expected typed ambiguous-reference error, got {error:?}");
        };
        assert_eq!(reference, "w-collision");
        assert_eq!(candidates.len(), MAX_AMBIGUOUS_WORK_CANDIDATES);
        assert_eq!(more, 1);
        let mut expected = items.iter().map(|item| item.work_id).collect::<Vec<_>>();
        expected.sort_by_key(|work_id| work_id.0);
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.work_id)
                .collect::<Vec<_>>(),
            expected[..MAX_AMBIGUOUS_WORK_CANDIDATES]
        );
        assert_eq!(
            candidates
                .iter()
                .map(|candidate| candidate.title.as_str())
                .collect::<Vec<_>>(),
            expected[..MAX_AMBIGUOUS_WORK_CANDIDATES]
                .iter()
                .map(|work_id| {
                    items
                        .iter()
                        .find(|item| item.work_id == *work_id)
                        .expect("expected item")
                        .title
                        .as_str()
                })
                .collect::<Vec<_>>()
        );
        let omitted_id = expected[MAX_AMBIGUOUS_WORK_CANDIDATES];
        let omitted = items
            .iter()
            .find(|item| item.work_id == omitted_id)
            .expect("omitted target");
        let omitted_candidate = WorkReferenceCandidate {
            work_id: omitted.work_id,
            short_ref: omitted.short_ref.clone(),
            title: omitted.title.clone(),
            lifecycle: omitted.lifecycle,
        };
        assert_eq!(
            command_work_ref_on(
                &store.connection,
                &ProjectId("ambiguous-project".into()),
                &omitted_candidate,
            )
            .expect("known omitted target uses its full id"),
            omitted_id.0.to_string()
        );
    }

    #[test]
    fn completion_recovery_rejects_a_shell_unsafe_participant_id() {
        let mut store = SqliteStore::open_in_memory().expect("recovery fixture");
        install_grant(&mut store, "unsafe-recovery-project", "planner");
        let work = store
            .create_work(
                &root_request("unsafe-recovery-project", "unsafe-recovery", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("root work");
        let error = completion_recovery_on(
            &store.connection,
            &work,
            WorkCompletionRecoveryCause::MissingContribution {
                participant: SessionId("peer; Remove-Item important".into()),
            },
        )
        .expect_err("shell metacharacters must not enter executable recovery guidance");
        assert!(
            matches!(error, StoreError::InvalidWorkProjection(reason) if reason.contains("shell-safe CLI argument"))
        );
    }

    #[test]
    fn gate_integrity_requires_a_valid_payload_and_prior_same_name_head() {
        let work_id = WorkId(uuid::Uuid::now_v7());
        let run_id = WorkRunId(uuid::Uuid::now_v7());
        let claim_id = WorkClaimId(uuid::Uuid::now_v7());
        let make = |previous: Option<ObjectHash>, passed: bool| WorkEvidence {
            schema_version: SCHEMA_VERSION,
            work_id,
            run_id,
            claim_id,
            claim_fence: 1,
            summary: GATE_EVIDENCE_SUMMARY.into(),
            refs: Vec::new(),
            gate: Some(GateEvidenceRecord {
                schema_version: SCHEMA_VERSION,
                name: "cargo-test".into(),
                passed,
                failed: Vec::new(),
                previous,
            }),
            actor: actor("gate-integrity"),
            created_at: at(1),
        };

        let first = make(None, true);
        let first_hash = CanonicalObject::freeze(&first)
            .expect("first gate object")
            .hash()
            .clone();
        let mut heads = HashMap::new();
        validate_gate_evidence_chain(&first_hash, &first, &mut heads)
            .expect("first normalized gate starts the chain");

        let mut alternate_summary = first.clone();
        alternate_summary.summary = "human rendering may evolve".into();
        validate_gate_evidence_payload(&alternate_summary)
            .expect("the typed gate field, not a summary literal, is the discriminator");

        let second = make(Some(first_hash.clone()), true);
        let second_hash = CanonicalObject::freeze(&second)
            .expect("second gate object")
            .hash()
            .clone();
        validate_gate_evidence_chain(&second_hash, &second, &mut heads)
            .expect("the exact prior head advances the chain");

        let dangling_target = CanonicalObject::freeze(&"dangling")
            .expect("dangling target")
            .hash()
            .clone();
        let dangling = make(Some(dangling_target), true);
        let dangling_hash = CanonicalObject::freeze(&dangling)
            .expect("dangling gate object")
            .hash()
            .clone();
        assert!(validate_gate_evidence_chain(&dangling_hash, &dangling, &mut heads).is_err());

        let malformed = make(Some(second_hash), false);
        let malformed_hash = CanonicalObject::freeze(&malformed)
            .expect("malformed gate object")
            .hash()
            .clone();
        assert!(validate_gate_evidence_chain(&malformed_hash, &malformed, &mut heads).is_err());
    }

    #[test]
    fn prerequisite_state_uses_one_hop_satisfaction_and_dead_edge_rules() {
        let prerequisite_id = WorkId::new();
        assert_eq!(
            classify_prerequisite_state(WorkLifecycle::Completed, None, prerequisite_id)
                .expect("completed state"),
            WorkPrerequisiteState::Satisfied
        );
        assert_eq!(
            classify_prerequisite_state(WorkLifecycle::Open, None, prerequisite_id)
                .expect("open state"),
            WorkPrerequisiteState::Pending
        );
        assert_eq!(
            classify_prerequisite_state(WorkLifecycle::Cancelled, None, prerequisite_id)
                .expect("cancelled state"),
            WorkPrerequisiteState::Dead
        );
        assert_eq!(
            classify_prerequisite_state(
                WorkLifecycle::Superseded,
                Some(WorkLifecycle::Completed),
                prerequisite_id,
            )
            .expect("completed replacement"),
            WorkPrerequisiteState::Satisfied
        );
        assert_eq!(
            classify_prerequisite_state(
                WorkLifecycle::Superseded,
                Some(WorkLifecycle::Open),
                prerequisite_id,
            )
            .expect("live replacement"),
            WorkPrerequisiteState::Pending
        );
        assert_eq!(
            classify_prerequisite_state(
                WorkLifecycle::Superseded,
                Some(WorkLifecycle::Cancelled),
                prerequisite_id,
            )
            .expect("cancelled replacement"),
            WorkPrerequisiteState::Dead
        );
    }

    fn restore_savepoint(store: &SqliteStore) {
        store
            .connection
            .execute_batch("ROLLBACK TO corrupt; RELEASE corrupt")
            .expect("restore corruption savepoint");
    }

    fn actor(session: &str) -> ActorContext {
        ActorContext {
            actor_id: session.into(),
            actor_kind: "test_agent".into(),
            assurance: AssuranceLevel::Asserted,
            run_id: None,
            session_id: Some(SessionId(session.into())),
            source_tool: Some("work_test".into()),
            source_skill: None,
            provenance_chain: Vec::<ProvenanceLink>::new(),
            reason: "exercise local work lifecycle".into(),
        }
    }

    fn test_grant(
        project: &str,
        actor_id: &str,
        budget: WorkPlanningBudget,
        valid_until: DateTime<Utc>,
    ) -> WorkAuthorityGrant {
        WorkAuthorityGrant {
            schema_version: SCHEMA_VERSION,
            project_id: crate::domain::ProjectId(project.into()),
            policy_ref: "project-default".into(),
            subject_actor_id: actor_id.into(),
            issued_by: actor("host-operator"),
            assurance: AssuranceLevel::Asserted,
            operations: vec![
                WorkAuthorityOperation::RootCreate,
                WorkAuthorityOperation::Plan,
                WorkAuthorityOperation::Claim,
                WorkAuthorityOperation::Dispose,
                WorkAuthorityOperation::RootComplete,
                WorkAuthorityOperation::Reopen,
                WorkAuthorityOperation::ClaimRecovery,
                WorkAuthorityOperation::CompletionWaiver,
                WorkAuthorityOperation::CompletionDrain,
                WorkAuthorityOperation::ObligationWaiver,
            ],
            scope: WorkAuthorityScope::Project,
            planning_budget: Some(budget),
            issued_at: at(-1_000),
            valid_until,
            reason: "test host-authorized local work".into(),
        }
    }

    fn default_budget() -> WorkPlanningBudget {
        WorkPlanningBudget {
            max_depth: 8,
            max_open_descendants: 64,
            max_children_per_decomposition: 16,
        }
    }

    fn install_grant(store: &mut SqliteStore, project: &str, actor_id: &str) -> ObjectHash {
        store
            .install_work_authority_grant(
                test_grant(project, actor_id, default_budget(), at(1_000)),
                &DevelopmentNoopRedactor,
            )
            .expect("install test work authority grant")
    }

    fn authority(project: &str, actor_id: &str) -> LifecycleAuthorityDecision {
        let object =
            CanonicalObject::freeze(&test_grant(project, actor_id, default_budget(), at(1_000)))
                .expect("canonical test work authority grant");
        LifecycleAuthorityDecision {
            grant: object.hash().clone(),
        }
    }

    fn delegated(project: &str, actor_id: &str) -> WorkPlanningAuthority {
        WorkPlanningAuthority::Delegated {
            grant: authority(project, actor_id).grant,
        }
    }

    fn install_delegated_with_budget(
        store: &mut SqliteStore,
        project: &str,
        actor_id: &str,
        budget: WorkPlanningBudget,
        valid_until: DateTime<Utc>,
    ) -> WorkPlanningAuthority {
        let grant = store
            .install_work_authority_grant(
                test_grant(project, actor_id, budget, valid_until),
                &DevelopmentNoopRedactor,
            )
            .expect("install bounded planning grant");
        WorkPlanningAuthority::Delegated { grant }
    }

    struct RejectingRedactor;

    impl Redactor for RejectingRedactor {
        fn inspect(&self, _prose: &str) -> Result<(), String> {
            Err("test policy refused candidate work content".into())
        }

        fn description(&self) -> &'static str {
            "test rejecting redactor"
        }
    }

    struct RevocationReasonRejectingRedactor;

    impl Redactor for RevocationReasonRejectingRedactor {
        fn inspect(&self, prose: &str) -> Result<(), String> {
            if prose.contains("secret revocation material") {
                Err("test policy refused revocation reason".into())
            } else {
                Ok(())
            }
        }

        fn description(&self) -> &'static str {
            "test revocation-reason rejecting redactor"
        }
    }

    fn root_request(project: &str, key: &str, second: i64) -> CreateWorkRequest {
        CreateWorkRequest {
            project_id: crate::domain::ProjectId(project.into()),
            parent_id: None,
            child_requirement: ChildRequirement::Required,
            title: "Ship local work".into(),
            outcome: "The local work lifecycle operates end to end".into(),
            acceptance: vec!["root accepted".into()],
            kind: WorkItemKind::Feature,
            priority: 1,
            labels: vec!["local-work".into()],
            assigned_to: None,
            deferred_until: None,
            origin: WorkOrigin::Local,
            source_snapshot_id: None,
            authority_policy_ref: "project-default".into(),
            authority: authority(project, "planner"),
            actor: actor("planner"),
            idempotency_key: key.into(),
            created_at: at(second),
        }
    }

    fn child(key: &str, requirement: ChildRequirement, title: &str) -> ChildWorkDraft {
        ChildWorkDraft {
            local_key: key.into(),
            child_requirement: requirement,
            title: title.into(),
            outcome: format!("{title} outcome"),
            acceptance: vec![format!("{key} accepted")],
            kind: WorkItemKind::Task,
            priority: 1,
            labels: vec![key.into()],
            assigned_to: None,
            deferred_until: None,
        }
    }

    fn claim(
        store: &mut SqliteStore,
        work: &WorkItem,
        holder: &str,
        key: &str,
        second: i64,
        ttl_seconds: i64,
    ) -> WorkClaim {
        install_grant(store, &work.project_id.0, holder);
        store
            .claim_work(
                &ClaimWorkRequest {
                    work_id: work.work_id,
                    expected_work_revision: work.revision,
                    expected_run_id: work.active_run_id.expect("active run"),
                    holder: SessionId(holder.into()),
                    ttl_seconds,
                    authority: authority(&work.project_id.0, holder),
                    recovery_authority: Some(authority(&work.project_id.0, holder)),
                    recovery_reason: Some("recover abandoned test claim".into()),
                    actor: actor(holder),
                    idempotency_key: key.into(),
                    claimed_at: at(second),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("claim work")
    }

    fn checkpoint(
        store: &mut SqliteStore,
        work: &WorkItem,
        claim: &WorkClaim,
        holder: &str,
        key: &str,
        second: i64,
        evidence: &[ObjectHash],
    ) -> ObjectHash {
        store
            .checkpoint_work(
                &CheckpointWorkRequest {
                    work_id: work.work_id,
                    run_id: claim.run_id,
                    expected_work_revision: work.revision,
                    holder: SessionId(holder.into()),
                    claim_id: claim.claim_id,
                    claim_fence: claim.fence,
                    summary: "checkpointed implementation progress".into(),
                    evidence: Some(evidence.to_vec()),
                    actor: actor(holder),
                    idempotency_key: key.into(),
                    checkpointed_at: at(second),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("checkpoint work")
    }

    fn evidence(
        store: &mut SqliteStore,
        work: &WorkItem,
        claim: &WorkClaim,
        holder: &str,
        key: &str,
        second: i64,
    ) -> ObjectHash {
        store
            .record_work_evidence(
                &RecordWorkEvidenceRequest {
                    work_id: work.work_id,
                    run_id: claim.run_id,
                    expected_work_revision: work.revision,
                    holder: SessionId(holder.into()),
                    claim_id: claim.claim_id,
                    claim_fence: claim.fence,
                    summary: "focused validation passed".into(),
                    refs: vec!["cargo:test".into()],
                    actor: actor(holder),
                    idempotency_key: key.into(),
                    recorded_at: at(second),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("record evidence")
    }

    fn completion_request(
        work: &WorkItem,
        claim: &WorkClaim,
        holder: &str,
        evidence: &ObjectHash,
        key: &str,
        second: i64,
    ) -> CompleteWorkRequest {
        CompleteWorkRequest {
            work_id: work.work_id,
            run_id: claim.run_id,
            holder: SessionId(holder.into()),
            expected_work_revision: work.revision,
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
            evidence: vec![evidence.clone()],
            acceptance: work
                .acceptance
                .iter()
                .map(|criterion| AcceptanceResult {
                    criterion: criterion.clone(),
                    satisfied: true,
                    evidence: vec![evidence.clone()],
                    assurance: AssuranceLevel::Asserted,
                    note: "verified".into(),
                })
                .collect(),
            drain: crate::domain::CompletionDrainAttestation {
                reconciled_action_outcomes: Vec::new(),
                released_resource_leases: Vec::new(),
                decision: authority(&work.project_id.0, holder),
            },
            root_authority: (work.work_id == work.root_id)
                .then(|| authority(&work.project_id.0, holder)),
            actor: actor(holder),
            idempotency_key: key.into(),
            completed_at: at(second),
        }
    }

    fn complete(
        store: &mut SqliteStore,
        work: &WorkItem,
        claim: &WorkClaim,
        holder: &str,
        evidence: &ObjectHash,
        key: &str,
        second: i64,
    ) -> Result<CompletionSeal, StoreError> {
        install_grant(store, &work.project_id.0, holder);
        store.complete_work(
            &completion_request(work, claim, holder, evidence, key, second),
            &DevelopmentNoopRedactor,
        )
    }

    #[test]
    fn catalog_uses_unicode_keys_and_ready_ranking_is_deterministic() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        let project = "project-catalog-index";
        install_grant(&mut store, project, "planner");

        let mut oldest_request = root_request(project, "catalog-oldest", 0);
        oldest_request.title = "Maße der Größe".into();
        oldest_request.labels = vec!["Größe".into()];
        oldest_request.assigned_to = Some("Straße".into());
        let oldest = store
            .create_work(&oldest_request, &DevelopmentNoopRedactor)
            .expect("oldest root");

        let mut unblocks_request = root_request(project, "catalog-unblocks", 1);
        unblocks_request.title = "Dependency provider".into();
        let unblocks = store
            .create_work(&unblocks_request, &DevelopmentNoopRedactor)
            .expect("dependency provider");

        let mut dependent_request = root_request(project, "catalog-dependent", 2);
        dependent_request.title = "Dependent work".into();
        let dependent = store
            .create_work(&dependent_request, &DevelopmentNoopRedactor)
            .expect("dependent root");
        store
            .add_work_prerequisite(
                &ChangeWorkPrerequisiteRequest {
                    work_id: dependent.work_id,
                    prerequisite_id: unblocks.work_id,
                    expected_revision: dependent.revision,
                    authority: delegated(project, "planner"),
                    actor: actor("planner"),
                    idempotency_key: "catalog-prerequisite".into(),
                    changed_at: at(3),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("add ranking prerequisite");

        let terminal = store
            .create_work(
                &root_request(project, "catalog-terminal-dependent", 3),
                &DevelopmentNoopRedactor,
            )
            .expect("terminal dependent");
        let terminal = store
            .add_work_prerequisite(
                &ChangeWorkPrerequisiteRequest {
                    work_id: terminal.work_id,
                    prerequisite_id: oldest.work_id,
                    expected_revision: terminal.revision,
                    authority: delegated(project, "planner"),
                    actor: actor("planner"),
                    idempotency_key: "catalog-terminal-prerequisite".into(),
                    changed_at: at(4),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("add terminal prerequisite");
        let terminal = store
            .dispose_work(
                &DisposeWorkRequest {
                    work_id: terminal.work_id,
                    expected_work_revision: terminal.revision,
                    disposition: WorkDisposition::Cancelled,
                    replacement_id: None,
                    reason: "terminal dependant must not affect ready rank".into(),
                    authority: authority(project, "planner"),
                    actor: actor("planner"),
                    idempotency_key: "catalog-terminal-dispose".into(),
                    disposed_at: at(5),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("dispose terminal dependent");

        reset_work_event_decode_count();
        reset_work_item_projection_decode_count();
        let ready = store
            .ready_work(&crate::domain::ProjectId(project.into()), at(6), 10)
            .expect("rank ready work");
        assert_eq!(
            ready
                .iter()
                .map(|candidate| candidate.work.work_id)
                .collect::<Vec<_>>(),
            vec![unblocks.work_id, oldest.work_id]
        );
        assert_eq!(work_event_decode_count(), 0);
        assert_eq!(work_item_projection_decode_count(), 2);

        for (query, expected) in [
            (
                WorkCatalogQuery {
                    assigned_to: Some("STRASSE".into()),
                    limit: 10,
                    ..WorkCatalogQuery::default()
                },
                oldest.work_id,
            ),
            (
                WorkCatalogQuery {
                    label: Some("GRÖSSE".into()),
                    limit: 10,
                    ..WorkCatalogQuery::default()
                },
                oldest.work_id,
            ),
            (
                WorkCatalogQuery {
                    search: Some("MASSE DER GRÖSSE".into()),
                    limit: 10,
                    ..WorkCatalogQuery::default()
                },
                oldest.work_id,
            ),
            (
                WorkCatalogQuery {
                    availabilities: vec![WorkAvailability::Blocked],
                    limit: 10,
                    ..WorkCatalogQuery::default()
                },
                dependent.work_id,
            ),
        ] {
            reset_work_event_decode_count();
            let page = store
                .query_work_catalog(&crate::domain::ProjectId(project.into()), at(6), &query)
                .expect("indexed catalog query");
            assert_eq!(page.items.len(), 1);
            assert_eq!(page.items[0].work.work_id, expected);
            assert_eq!(work_event_decode_count(), 0);
        }

        store
            .add_work_blocker(
                &AddWorkBlockerRequest {
                    work_id: oldest.work_id,
                    expected_work_revision: oldest.revision,
                    kind: crate::domain::WorkBlockerKind::Manual,
                    detail: "deferred work still has an independent blocker".into(),
                    authority: delegated(project, "planner"),
                    actor: actor("planner"),
                    idempotency_key: "catalog-deferred-blocker".into(),
                    blocked_at: at(7),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("block deferred candidate");
        let oldest = store.get_work_item(oldest.work_id).expect("blocked oldest");
        store
            .revise_work(
                &ReviseWorkRequest {
                    work_id: oldest.work_id,
                    expected_revision: oldest.revision,
                    patch: WorkRevisionPatch {
                        deferred_until: Some(at(100)),
                        ..WorkRevisionPatch::default()
                    },
                    authority: delegated(project, "planner"),
                    actor: actor("planner"),
                    idempotency_key: "catalog-deferred-revision".into(),
                    updated_at: at(8),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("defer blocked candidate");
        let blocked_only = store
            .query_work_catalog(
                &crate::domain::ProjectId(project.into()),
                at(9),
                &WorkCatalogQuery {
                    blocked_only: true,
                    limit: 10,
                    ..WorkCatalogQuery::default()
                },
            )
            .expect("independent blocked-only query");
        let blocked_availability = blocked_only
            .items
            .iter()
            .map(|item| (item.work.work_id, item.availability))
            .collect::<HashMap<_, _>>();
        assert_eq!(
            blocked_availability.get(&oldest.work_id),
            Some(&WorkAvailability::Deferred)
        );
        assert_eq!(
            blocked_availability.get(&terminal.work_id),
            Some(&WorkAvailability::Closed)
        );
        assert_eq!(
            blocked_availability.get(&dependent.work_id),
            Some(&WorkAvailability::Blocked)
        );
        assert!(store.verify_all().expect("catalog integrity").is_healthy());
    }

    #[test]
    fn doctor_exercises_work_catalog_fts_index() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "project-catalog-fts-integrity", "planner");
        let item = store
            .create_work(
                &root_request("project-catalog-fts-integrity", "fts-root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("indexed root");
        assert!(store.verify_all().expect("healthy catalog").is_healthy());

        store
            .connection
            .execute("DELETE FROM work_catalog_fts_data WHERE id > 1", [])
            .expect("corrupt only the FTS segment data");
        let report = store.verify_all().expect("catalog corruption report");
        assert!(
            report
                .invalid_work_records
                .iter()
                .any(|record| { record == &format!("work_catalog:{}:fts_index", item.work_id.0) })
        );
    }

    #[test]
    fn work_feed_arithmetic_refuses_overflow() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        let feed = FeedId::Project(ProjectId("project-feed-overflow".into()));

        let interval = store.work_feed_between(&feed, i64::MIN, i64::MAX);
        assert!(matches!(
            interval,
            Err(StoreError::InvalidWorkProjection(reason)) if reason.contains("overflowed")
        ));

        let (feed_kind, feed_id) = feed_parts(&feed);
        store
            .connection
            .execute(
                "INSERT INTO work_feed_heads (feed_kind, feed_id, position)
                 VALUES (?1, ?2, ?3)",
                params![feed_kind, feed_id, i64::MAX],
            )
            .expect("install saturated feed head");
        let transaction = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("overflow transaction");
        let allocation = reserve_feed_position(&transaction, &feed);
        assert!(matches!(
            allocation,
            Err(StoreError::InvalidWorkProjection(reason)) if reason.contains("position overflowed")
        ));
        transaction.rollback().expect("discard saturated feed head");

        assert!(matches!(
            checkpoint_feed_end(i64::MAX),
            Err(StoreError::InvalidWorkProjection(reason))
                if reason.contains("checkpoint run-feed position arithmetic overflowed")
        ));
    }

    #[test]
    fn staged_work_delivery_rejects_future_stale_and_gapped_ranges() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "project-delivery", "planner");
        let first = store
            .create_work(
                &root_request("project-delivery", "delivery-root-a", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("first root");
        store
            .create_work(
                &root_request("project-delivery", "delivery-root-b", 1),
                &DevelopmentNoopRedactor,
            )
            .expect("second root");
        let feed = FeedId::Project(first.project_id.clone());
        let head = store.work_feed_head(&feed).expect("project feed head");
        assert!(head > 1);
        let entries = store
            .work_feed_between(&feed, 0, head)
            .expect("dense source entries");
        let payload = CanonicalObject::freeze(&entries).expect("test delivery payload");
        let empty_payload = CanonicalObject::freeze(&Vec::<WorkFeedEntry>::new())
            .expect("empty test delivery payload");

        assert!(matches!(
            store.stage_work_session_delivery(
                &first.project_id,
                &SessionId("future".into()),
                StageWorkSessionDelivery {
                    expected_confirmed_through: 0,
                    expected_focused_work_id: None,
                    expected_bound_task_id: None,
                    delivered_through: head + 1,
                    delivered_entries: &[],
                    delivery_payload: &empty_payload,
                    now: at(2),
                },
            ),
            Err(StoreError::InvalidWork(_))
        ));

        let session = SessionId("confirmed".into());
        let staged = store
            .stage_work_session_delivery(
                &first.project_id,
                &session,
                StageWorkSessionDelivery {
                    expected_confirmed_through: 0,
                    expected_focused_work_id: None,
                    expected_bound_task_id: None,
                    delivered_through: head,
                    delivered_entries: &entries,
                    delivery_payload: &payload,
                    now: at(2),
                },
            )
            .expect("stage exact head");
        let staged = staged.expect("stage wins exact compare-and-swap");
        let delivery_token = staged
            .tentative_delivery_token
            .clone()
            .expect("staged delivery token");
        let expected_refusal = "work delivery acknowledgement does not match the pending page; replay it with work_next (changes selected, no acknowledgement) and acknowledge the delivered_through and delivery_token you receive";
        for (through, token) in [(head + 1_000, "wrong-token"), (head, "wrong-token")] {
            match store.acknowledge_work_session_delivery(
                &first.project_id,
                &session,
                through,
                Some(token),
                at(3),
            ) {
                Err(StoreError::InvalidWork(message)) => assert_eq!(message, expected_refusal),
                result => panic!("invalid acknowledgement must fail generically: {result:?}"),
            }
        }
        let still_pending = store
            .work_session_state(&first.project_id, &session, at(3))
            .expect("pending delivery survives rejected acknowledgements");
        assert_eq!(still_pending.project_cursor, 0);
        assert_eq!(still_pending.tentative_project_cursor, Some(head));
        assert_eq!(
            still_pending.tentative_delivery_token.as_deref(),
            Some(delivery_token.as_str())
        );
        store
            .acknowledge_work_session_delivery(
                &first.project_id,
                &session,
                head,
                Some(&delivery_token),
                at(3),
            )
            .expect("acknowledge exact head");
        assert!(matches!(
            store.stage_work_session_delivery(
                &first.project_id,
                &session,
                StageWorkSessionDelivery {
                    expected_confirmed_through: head,
                    expected_focused_work_id: None,
                    expected_bound_task_id: None,
                    delivered_through: head - 1,
                    delivered_entries: &[],
                    delivery_payload: &empty_payload,
                    now: at(4),
                }
            ),
            Err(StoreError::InvalidWork(_))
        ));

        let (feed_kind, feed_id) = feed_parts(&feed);
        store
            .connection
            .execute(
                "DELETE FROM work_feed_entries
                 WHERE feed_kind = ?1 AND feed_id = ?2 AND position = 1",
                params![feed_kind, feed_id],
            )
            .expect("create a corrupt feed gap");
        assert!(matches!(
            store.stage_work_session_delivery(
                &first.project_id,
                &SessionId("gap".into()),
                StageWorkSessionDelivery {
                    expected_confirmed_through: 0,
                    expected_focused_work_id: None,
                    expected_bound_task_id: None,
                    delivered_through: head,
                    delivered_entries: &entries,
                    delivery_payload: &payload,
                    now: at(5),
                },
            ),
            Err(StoreError::InvalidWorkProjection(_))
        ));
    }

    #[test]
    fn staged_work_delivery_cas_binds_the_current_task() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "project-delivery-task-cas", "planner");
        let work = store
            .create_work(
                &root_request("project-delivery-task-cas", "delivery-root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("root");
        let feed = FeedId::Project(work.project_id.clone());
        let head = store.work_feed_head(&feed).expect("project feed head");
        let entries = store
            .work_feed_between(&feed, 0, head)
            .expect("dense source entries");
        let payload = CanonicalObject::freeze(&entries).expect("test delivery payload");
        let session = SessionId("task-bound-delivery".into());
        let task = store
            .start_task(
                &work.project_id,
                "dummy:DELIVERY-TASK-CAS",
                "Delivery task CAS",
                &session,
                actor("task-bound-delivery"),
                at(1),
            )
            .expect("task binding")
            .task;

        assert!(
            store
                .stage_work_session_delivery(
                    &work.project_id,
                    &session,
                    StageWorkSessionDelivery {
                        expected_confirmed_through: 0,
                        expected_focused_work_id: None,
                        expected_bound_task_id: None,
                        delivered_through: head,
                        delivered_entries: &entries,
                        delivery_payload: &payload,
                        now: at(2),
                    },
                )
                .expect("basis mismatch is a retry")
                .is_none()
        );
        let staged = store
            .stage_work_session_delivery(
                &work.project_id,
                &session,
                StageWorkSessionDelivery {
                    expected_confirmed_through: 0,
                    expected_focused_work_id: None,
                    expected_bound_task_id: Some(task.task_id),
                    delivered_through: head,
                    delivered_entries: &entries,
                    delivery_payload: &payload,
                    now: at(3),
                },
            )
            .expect("current task basis stages")
            .expect("exact staging CAS");
        assert_eq!(staged.tentative_project_cursor, Some(head));
        assert!(staged.tentative_delivery_token.is_some());
    }

    #[test]
    fn focus_derived_work_contradiction_publishes_one_feed_delta_and_doctor_backstops() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "project-contra", "planner");
        let root = store
            .create_work(
                &root_request("project-contra", "contra-root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("root");
        let project = root.project_id.clone();
        let session = SessionId("contra-session".into());
        claim(&mut store, &root, "contra-session", "contra-claim", 1, 300);
        store
            .focus_work_session(&project, &session, root.work_id, at(1))
            .expect("focus the root");
        let note = |prose: &str, key: &str, second: i64| crate::NoteRequest {
            project_id: project.clone(),
            task_id: None,
            work_id: Some(root.work_id),
            prose: prose.into(),
            visibility: crate::NoteVisibility::Shared,
            kind: None,
            authority: None,
            sensitivity: None,
            title: None,
            tags: Vec::new(),
            evidence: Vec::new(),
            refs: Vec::new(),
            actor: actor("contra-session"),
            idempotency_key: key.into(),
            created_at: at(second),
        };
        let left = store
            .capture_note(
                &note("Constraint: use the first rule", "contra-left", 2),
                &DevelopmentNoopRedactor,
            )
            .expect("left note");
        let right = store
            .capture_note(
                &note("Constraint: use the second rule", "contra-right", 3),
                &DevelopmentNoopRedactor,
            )
            .expect("right note");
        let third = store
            .capture_note(
                &note("Constraint: use the third rule", "contra-third", 4),
                &DevelopmentNoopRedactor,
            )
            .expect("third note");

        // The caller omits the work anchor; the validated focus supplies it.
        let derived = store
            .record_memory_contradiction(
                &project,
                None,
                None,
                &session,
                "contra-session",
                &left.version,
                &right.version,
                "the first and second rules cannot both apply",
                "contra-derived",
                actor("contra-session"),
                at(5),
            )
            .expect("focus-derived contradiction");
        let explicit = store
            .record_memory_contradiction(
                &project,
                None,
                Some(root.work_id),
                &session,
                "contra-session",
                &left.version,
                &third.version,
                "the first and third rules cannot both apply",
                "contra-explicit",
                actor("contra-session"),
                at(6),
            )
            .expect("explicit contradiction");
        let feeds = |positions: &[FeedPosition]| {
            positions
                .iter()
                .map(|position| match &position.feed {
                    FeedId::Project(_) => "project",
                    FeedId::RootWork(_) => "root_work",
                    FeedId::RunExecution(_) => "run_execution",
                })
                .collect::<Vec<_>>()
        };
        assert!(!derived.work_positions.is_empty());
        assert_eq!(
            feeds(&derived.work_positions),
            feeds(&explicit.work_positions)
        );
        assert!(derived.work_positions.iter().any(
            |position| matches!(position.feed, FeedId::RootWork(root_id) if root_id == root.work_id)
        ));
        assert!(store.verify_all().expect("integrity").is_healthy());

        // Doctor names a work-anchored contradiction that lost its feed entry.
        store
            .connection
            .execute(
                "DELETE FROM work_feed_entries WHERE feed_kind = 'root_work' AND object_hash = ?1",
                [derived.contradiction.as_str()],
            )
            .expect("simulate a missing anchored feed entry");
        let report = store.verify_all().expect("integrity after damage");
        assert!(!report.is_healthy());
        assert!(
            report.invalid_work_records.iter().any(|label| {
                label
                    == &format!(
                        "memory_contradiction_event:{}:work-feed",
                        derived.contradiction
                    )
            }),
            "{:?}",
            report.invalid_work_records
        );
    }

    #[test]
    fn focus_change_and_pending_delivery_serialize_across_connections() {
        let directory = tempfile::tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let mut writer = SqliteStore::open(&database).expect("writer");
        install_grant(&mut writer, "project-focus-delivery", "planner");
        let first = writer
            .create_work(
                &root_request("project-focus-delivery", "focus-first", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("first root");
        let second = writer
            .create_work(
                &root_request("project-focus-delivery", "focus-second", 1),
                &DevelopmentNoopRedactor,
            )
            .expect("second root");
        let session = SessionId("focus-delivery-session".into());
        writer
            .focus_work_session(&first.project_id, &session, first.work_id, at(2))
            .expect("initial focus");
        let head = writer
            .work_feed_head(&FeedId::Project(first.project_id.clone()))
            .expect("feed head");

        let mut delivery = SqliteStore::open(&database).expect("delivery connection");
        let feed = FeedId::Project(first.project_id.clone());
        let entries = delivery
            .work_feed_between(&feed, 0, head)
            .expect("dense source entries");
        let payload = CanonicalObject::freeze(&entries).expect("test delivery payload");
        let staged = delivery
            .stage_work_session_delivery(
                &first.project_id,
                &session,
                StageWorkSessionDelivery {
                    expected_confirmed_through: 0,
                    expected_focused_work_id: Some(first.work_id),
                    expected_bound_task_id: None,
                    delivered_through: head,
                    delivered_entries: &entries,
                    delivery_payload: &payload,
                    now: at(3),
                },
            )
            .expect("stage from a second connection");
        let staged = staged.expect("delivery wins exact compare-and-swap");
        // Re-focusing the same item keeps the staged page.
        let same = writer
            .focus_work_session(&first.project_id, &session, first.work_id, at(4))
            .expect("same focus");
        assert_eq!(same.tentative_project_cursor, Some(head));
        // A different focus discards the page projected under the old focus
        // without confirming anything.
        let discarded = writer
            .focus_work_session(&first.project_id, &session, second.work_id, at(4))
            .expect("focus changes while a page is still staged");
        assert_eq!(discarded.focused_work_id, Some(second.work_id));
        assert_eq!(discarded.tentative_project_cursor, None);
        assert_eq!(discarded.tentative_delivery_token, None);
        assert_eq!(discarded.project_cursor, 0);
        assert!(
            delivery
                .acknowledge_work_session_delivery(
                    &first.project_id,
                    &session,
                    head,
                    staged.tentative_delivery_token.as_deref(),
                    at(5),
                )
                .is_err(),
            "a discarded page cannot be acknowledged"
        );

        // The next staging recomputes the interval under the new focus.
        let restaged = delivery
            .stage_work_session_delivery(
                &first.project_id,
                &session,
                StageWorkSessionDelivery {
                    expected_confirmed_through: 0,
                    expected_focused_work_id: Some(second.work_id),
                    expected_bound_task_id: None,
                    delivered_through: head,
                    delivered_entries: &entries,
                    delivery_payload: &payload,
                    now: at(6),
                },
            )
            .expect("restage under the new focus")
            .expect("exact staging CAS");
        delivery
            .acknowledge_work_session_delivery(
                &first.project_id,
                &session,
                head,
                restaged.tentative_delivery_token.as_deref(),
                at(7),
            )
            .expect("acknowledge the restaged page");
        let acknowledged = writer
            .work_session_state(&first.project_id, &session, at(8))
            .expect("acknowledged state");
        assert_eq!(acknowledged.focused_work_id, Some(second.work_id));
        assert_eq!(acknowledged.tentative_project_cursor, None);
        assert_eq!(acknowledged.project_cursor, head);
    }

    #[test]
    fn disposing_claimed_child_requires_and_records_participant_waiver() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "project-dispose-claim", "planner");
        let root = store
            .create_work(
                &root_request("project-dispose-claim", "dispose-claim-root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("root");
        let decomposition = store
            .decompose_work(
                &DecomposeWorkRequest {
                    parent_id: root.work_id,
                    expected_parent_revision: root.revision,
                    children: vec![
                        child(
                            "optional-child",
                            ChildRequirement::Optional,
                            "Optional child",
                        ),
                        child("unused-child", ChildRequirement::Optional, "Unused child"),
                    ],
                    prerequisites: Vec::new(),
                    authority: delegated("project-dispose-claim", "planner"),
                    actor: actor("planner"),
                    idempotency_key: "dispose-claim-decompose".into(),
                    created_at: at(1),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("decompose");
        let child = decomposition.children[0].clone();

        let mut limited_grant = test_grant(
            "project-dispose-claim",
            "child-agent",
            default_budget(),
            at(1_000),
        );
        limited_grant.operations = vec![
            WorkAuthorityOperation::Claim,
            WorkAuthorityOperation::Dispose,
        ];
        limited_grant.planning_budget = None;
        let limited_hash = store
            .install_work_authority_grant(limited_grant, &DevelopmentNoopRedactor)
            .expect("limited child grant");
        let limited_authority = LifecycleAuthorityDecision {
            grant: limited_hash,
        };
        let child_claim = store
            .claim_work(
                &ClaimWorkRequest {
                    work_id: child.work_id,
                    expected_work_revision: child.revision,
                    expected_run_id: child.active_run_id.expect("child run"),
                    holder: SessionId("child-agent".into()),
                    ttl_seconds: 100,
                    authority: limited_authority.clone(),
                    recovery_authority: None,
                    recovery_reason: None,
                    actor: actor("child-agent"),
                    idempotency_key: "dispose-claim-child-claim".into(),
                    claimed_at: at(2),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("claim child");
        let refused = store.dispose_work(
            &DisposeWorkRequest {
                work_id: child.work_id,
                expected_work_revision: child.revision,
                disposition: WorkDisposition::Cancelled,
                replacement_id: None,
                reason: "optional path was abandoned".into(),
                authority: limited_authority,
                actor: actor("child-agent"),
                idempotency_key: "dispose-claim-without-waiver".into(),
                disposed_at: at(3),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(refused, Err(StoreError::InvalidWork(_))));
        assert_eq!(
            load_work_item(&store.connection, child.work_id).expect("child after refusal"),
            child
        );
        assert_eq!(
            load_work_claim_optional(&store.connection, child_claim.run_id)
                .expect("claim after refusal"),
            Some(child_claim.clone())
        );
        let child_run =
            load_work_run(&store.connection, child_claim.run_id).expect("child run after refusal");
        assert!(
            load_root_execution(&store.connection, child_run.root_execution_id)
                .expect("root execution after refusal")
                .waivers
                .is_empty()
        );

        install_grant(&mut store, "project-dispose-claim", "child-agent");
        store
            .dispose_work(
                &DisposeWorkRequest {
                    work_id: child.work_id,
                    expected_work_revision: child.revision,
                    disposition: WorkDisposition::Cancelled,
                    replacement_id: None,
                    reason: "optional path was abandoned".into(),
                    authority: authority("project-dispose-claim", "child-agent"),
                    actor: actor("child-agent"),
                    idempotency_key: "dispose-claim-with-waiver".into(),
                    disposed_at: at(4),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("dispose with participant waiver");
        let unused_child = &decomposition.children[1];
        store
            .dispose_work(
                &DisposeWorkRequest {
                    work_id: unused_child.work_id,
                    expected_work_revision: unused_child.revision,
                    disposition: WorkDisposition::Cancelled,
                    replacement_id: None,
                    reason: "unused optional path".into(),
                    authority: authority("project-dispose-claim", "planner"),
                    actor: actor("planner"),
                    idempotency_key: "dispose-unused-child".into(),
                    disposed_at: at(4),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("dispose unused child");

        let root_claim = claim(
            &mut store,
            &decomposition.parent,
            "root-agent",
            "dispose-claim-root-claim",
            5,
            100,
        );
        let root_evidence = evidence(
            &mut store,
            &decomposition.parent,
            &root_claim,
            "root-agent",
            "dispose-claim-root-evidence",
            6,
        );
        checkpoint(
            &mut store,
            &decomposition.parent,
            &root_claim,
            "root-agent",
            "dispose-claim-root-checkpoint",
            7,
            std::slice::from_ref(&root_evidence),
        );
        let seal = complete(
            &mut store,
            &decomposition.parent,
            &root_claim,
            "root-agent",
            &root_evidence,
            "dispose-claim-root-complete",
            8,
        )
        .expect("root completes with child participant accounted");
        assert!(seal.waivers.iter().any(|waiver| {
            waiver.participant == child_claim.holder
                && waiver.reason == "optional path was abandoned"
        }));
    }

    #[test]
    fn cancelled_required_child_blocks_completion_until_an_authorized_waiver() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "project-waiver", "planner");
        let root = store
            .create_work(
                &root_request("project-waiver", "waiver-root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("root");
        let decomposition = store
            .decompose_work(
                &DecomposeWorkRequest {
                    parent_id: root.work_id,
                    expected_parent_revision: root.revision,
                    children: vec![
                        child(
                            "required-cancelled",
                            ChildRequirement::Required,
                            "Required but deliberately omitted",
                        ),
                        child(
                            "optional-open",
                            ChildRequirement::Optional,
                            "Optional work may remain open",
                        ),
                    ],
                    prerequisites: Vec::new(),
                    authority: delegated("project-waiver", "planner"),
                    actor: actor("planner"),
                    idempotency_key: "waiver-decompose".into(),
                    created_at: at(1),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("decompose");
        let root = decomposition.parent;
        let child = decomposition.children[0].clone();
        store
            .dispose_work(
                &DisposeWorkRequest {
                    work_id: child.work_id,
                    expected_work_revision: child.revision,
                    disposition: WorkDisposition::Cancelled,
                    replacement_id: None,
                    reason: "child is no longer required for the accepted outcome".into(),
                    authority: authority("project-waiver", "planner"),
                    actor: actor("planner"),
                    idempotency_key: "cancel-required-child".into(),
                    disposed_at: at(2),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("cancel required child");

        let root_claim = claim(&mut store, &root, "root-agent", "waiver-root-claim", 3, 100);
        let root_evidence = evidence(
            &mut store,
            &root,
            &root_claim,
            "root-agent",
            "waiver-root-evidence",
            4,
        );
        checkpoint(
            &mut store,
            &root,
            &root_claim,
            "root-agent",
            "waiver-root-checkpoint-before",
            5,
            std::slice::from_ref(&root_evidence),
        );
        assert!(
            !store
                .work_completion_readiness(root.work_id, &root_claim.holder, at(6))
                .expect("completion readiness")
                .0
        );
        assert!(matches!(
            complete(
                &mut store,
                &root,
                &root_claim,
                "root-agent",
                &root_evidence,
                "root-without-waiver",
                6,
            ),
            Err(StoreError::WorkCompletionRecoveryRequired {
                cause: WorkCompletionRecoveryCause::RequiredChildUnsealed {
                    child: blocked_child,
                },
                ..
            }) if blocked_child == child.work_id
        ));

        let waiver_request = WaiveRequiredChildRequest {
            parent_id: root.work_id,
            child_id: child.work_id,
            expected_parent_revision: root.revision,
            reason: "the omission is explicit, attributed, and accepted".into(),
            authority: authority("project-waiver", "planner"),
            actor: actor("planner"),
            idempotency_key: "waive-required-child".into(),
            waived_at: at(7),
        };
        let waiver = store
            .waive_required_child(&waiver_request, &DevelopmentNoopRedactor)
            .expect("authorized waiver");
        assert_eq!(
            store
                .waive_required_child(&waiver_request, &DevelopmentNoopRedactor)
                .expect("idempotent waiver"),
            waiver
        );
        let root_execution_id = store
            .get_work_run(root.active_run_id.expect("root run"))
            .expect("root run projection")
            .root_execution_id;
        let original_execution_json: Vec<u8> = store
            .connection
            .query_row(
                "SELECT execution_json FROM work_root_executions
                 WHERE root_execution_id = ?1",
                [root_execution_id.0.to_string()],
                |row| row.get(0),
            )
            .expect("root execution bytes");
        let mut corrupted_execution: RootExecution =
            serde_json::from_slice(&original_execution_json).expect("root execution");
        corrupted_execution
            .required_child_waivers
            .push(waiver.clone());
        store
            .connection
            .execute(
                "UPDATE work_root_executions SET execution_json = ?2
                 WHERE root_execution_id = ?1",
                params![
                    root_execution_id.0.to_string(),
                    serde_json::to_vec(&corrupted_execution).expect("corrupt execution JSON")
                ],
            )
            .expect("inject duplicate waiver");
        let corrupted = store.verify_all().expect("waiver integrity report");
        assert!(
            corrupted
                .invalid_work_records
                .iter()
                .any(|record| { record.ends_with(":invalid_required_child_waivers") })
        );
        assert!(matches!(
            store.work_completion_readiness(root.work_id, &root_claim.holder, at(8)),
            Err(StoreError::InvalidWorkProjection(_))
        ));
        store
            .connection
            .execute(
                "UPDATE work_root_executions SET execution_json = ?2
                 WHERE root_execution_id = ?1",
                params![root_execution_id.0.to_string(), original_execution_json],
            )
            .expect("restore root execution");
        checkpoint(
            &mut store,
            &root,
            &root_claim,
            "root-agent",
            "waiver-root-checkpoint-after",
            8,
            std::slice::from_ref(&root_evidence),
        );
        let seal = complete(
            &mut store,
            &root,
            &root_claim,
            "root-agent",
            &root_evidence,
            "root-with-waiver",
            9,
        )
        .expect("complete with explicit waiver");
        assert!(seal.required_child_seals.is_empty());
        assert_eq!(seal.required_child_waivers, vec![waiver]);
    }

    #[test]
    fn superseding_required_work_with_a_completed_optional_child_still_requires_a_waiver() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "project-supersede", "planner");
        let root = store
            .create_work(
                &root_request("project-supersede", "supersede-root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("root");
        let decomposition = store
            .decompose_work(
                &DecomposeWorkRequest {
                    parent_id: root.work_id,
                    expected_parent_revision: root.revision,
                    children: vec![
                        child(
                            "required-superseded",
                            ChildRequirement::Required,
                            "Required work",
                        ),
                        child(
                            "optional-replacement",
                            ChildRequirement::Optional,
                            "Unrelated optional work",
                        ),
                    ],
                    prerequisites: Vec::new(),
                    authority: delegated("project-supersede", "planner"),
                    actor: actor("planner"),
                    idempotency_key: "supersede-decompose".into(),
                    created_at: at(1),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("decompose");
        let root = decomposition.parent;
        let required = decomposition.children[0].clone();
        let optional = decomposition.children[1].clone();
        let optional_claim = claim(
            &mut store,
            &optional,
            "optional-agent",
            "optional-claim",
            2,
            100,
        );
        let optional_evidence = evidence(
            &mut store,
            &optional,
            &optional_claim,
            "optional-agent",
            "optional-evidence",
            3,
        );
        checkpoint(
            &mut store,
            &optional,
            &optional_claim,
            "optional-agent",
            "optional-checkpoint",
            4,
            std::slice::from_ref(&optional_evidence),
        );
        complete(
            &mut store,
            &optional,
            &optional_claim,
            "optional-agent",
            &optional_evidence,
            "optional-complete",
            5,
        )
        .expect("complete optional replacement");
        store
            .dispose_work(
                &DisposeWorkRequest {
                    work_id: required.work_id,
                    expected_work_revision: required.revision,
                    disposition: WorkDisposition::Superseded,
                    replacement_id: Some(optional.work_id),
                    reason: "attempt to substitute unrelated completed optional work".into(),
                    authority: authority("project-supersede", "planner"),
                    actor: actor("planner"),
                    idempotency_key: "supersede-required".into(),
                    disposed_at: at(6),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("supersede required child");

        let root_claim = claim(
            &mut store,
            &root,
            "root-agent",
            "supersede-root-claim",
            7,
            100,
        );
        let root_evidence = evidence(
            &mut store,
            &root,
            &root_claim,
            "root-agent",
            "supersede-root-evidence",
            8,
        );
        checkpoint(
            &mut store,
            &root,
            &root_claim,
            "root-agent",
            "supersede-root-checkpoint-before",
            9,
            std::slice::from_ref(&root_evidence),
        );
        assert!(matches!(
            complete(
                &mut store,
                &root,
                &root_claim,
                "root-agent",
                &root_evidence,
                "supersede-root-without-waiver",
                10,
            ),
            Err(StoreError::WorkCompletionRecoveryRequired {
                cause: WorkCompletionRecoveryCause::RequiredChildUnsealed { child },
                ..
            }) if child == required.work_id
        ));
        let waiver = store
            .waive_required_child(
                &WaiveRequiredChildRequest {
                    parent_id: root.work_id,
                    child_id: required.work_id,
                    expected_parent_revision: root.revision,
                    reason: "explicitly accept the superseded required outcome".into(),
                    authority: authority("project-supersede", "planner"),
                    actor: actor("planner"),
                    idempotency_key: "waive-superseded-required".into(),
                    waived_at: at(11),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("waive superseded required child");
        checkpoint(
            &mut store,
            &root,
            &root_claim,
            "root-agent",
            "supersede-root-checkpoint-after",
            12,
            std::slice::from_ref(&root_evidence),
        );
        let seal = complete(
            &mut store,
            &root,
            &root_claim,
            "root-agent",
            &root_evidence,
            "supersede-root-with-waiver",
            13,
        )
        .expect("complete after explicit waiver");
        assert_eq!(seal.required_child_waivers, vec![waiver]);
    }

    #[test]
    fn supersession_ref_and_replacement_matrix_is_enforced_in_storage() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "project-supersession-matrix", "planner");
        install_grant(&mut store, "other-supersession-project", "planner");
        let source = store
            .create_work(
                &root_request("project-supersession-matrix", "matrix-source", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("source");
        let live_replacement = store
            .create_work(
                &root_request("project-supersession-matrix", "matrix-live", 1),
                &DevelopmentNoopRedactor,
            )
            .expect("live replacement");
        let cross_project = store
            .create_work(
                &root_request("other-supersession-project", "matrix-cross", 2),
                &DevelopmentNoopRedactor,
            )
            .expect("cross-project replacement");

        let supersede = |work: &WorkItem,
                         replacement_id: WorkId,
                         key: &str,
                         second: i64|
         -> DisposeWorkRequest {
            DisposeWorkRequest {
                work_id: work.work_id,
                expected_work_revision: work.revision,
                disposition: WorkDisposition::Superseded,
                replacement_id: Some(replacement_id),
                reason: "matrix validation".into(),
                authority: authority("project-supersession-matrix", "planner"),
                actor: actor("planner"),
                idempotency_key: key.into(),
                disposed_at: at(second),
            }
        };

        assert!(matches!(
            store.dispose_work(
                &supersede(&source, source.work_id, "matrix-self", 3),
                &DevelopmentNoopRedactor,
            ),
            Err(StoreError::InvalidWork(_))
        ));
        assert!(matches!(
            store.dispose_work(
                &supersede(&source, cross_project.work_id, "matrix-cross", 4),
                &DevelopmentNoopRedactor,
            ),
            Err(StoreError::InvalidWork(_))
        ));

        let cancelled_replacement = store
            .create_work(
                &root_request("project-supersession-matrix", "matrix-cancelled", 5),
                &DevelopmentNoopRedactor,
            )
            .expect("cancelled replacement");
        let cancelled_replacement = store
            .dispose_work(
                &DisposeWorkRequest {
                    work_id: cancelled_replacement.work_id,
                    expected_work_revision: cancelled_replacement.revision,
                    disposition: WorkDisposition::Cancelled,
                    replacement_id: None,
                    reason: "cancel replacement".into(),
                    authority: authority("project-supersession-matrix", "planner"),
                    actor: actor("planner"),
                    idempotency_key: "matrix-cancel-replacement".into(),
                    disposed_at: at(6),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("cancel replacement");
        assert!(matches!(
            store.dispose_work(
                &supersede(
                    &source,
                    cancelled_replacement.work_id,
                    "matrix-cancelled-target",
                    7,
                ),
                &DevelopmentNoopRedactor,
            ),
            Err(StoreError::InvalidWork(_))
        ));

        let obsolete_replacement = store
            .create_work(
                &root_request("project-supersession-matrix", "matrix-obsolete", 8),
                &DevelopmentNoopRedactor,
            )
            .expect("obsolete replacement");
        let obsolete_replacement = store
            .dispose_work(
                &supersede(
                    &obsolete_replacement,
                    live_replacement.work_id,
                    "matrix-obsolete-dispose",
                    9,
                ),
                &DevelopmentNoopRedactor,
            )
            .expect("supersede obsolete replacement");
        assert!(matches!(
            store.dispose_work(
                &supersede(
                    &source,
                    obsolete_replacement.work_id,
                    "matrix-superseded-target",
                    10,
                ),
                &DevelopmentNoopRedactor,
            ),
            Err(StoreError::InvalidWork(_))
        ));

        let disposed = store
            .dispose_work(
                &supersede(&source, live_replacement.work_id, "matrix-valid", 11),
                &DevelopmentNoopRedactor,
            )
            .expect("open replacement is admitted");
        assert_eq!(disposed.lifecycle, WorkLifecycle::Superseded);
        assert_eq!(disposed.superseded_by, Some(live_replacement.work_id));
        assert!(matches!(
            store.dispose_work(
                &supersede(
                    &disposed,
                    live_replacement.work_id,
                    "matrix-closed-source",
                    12,
                ),
                &DevelopmentNoopRedactor,
            ),
            Err(StoreError::WorkNotOpen(work_id)) if work_id == disposed.work_id
        ));

        let direct_source = store
            .create_work(
                &root_request("project-supersession-matrix", "matrix-direct-source", 13),
                &DevelopmentNoopRedactor,
            )
            .expect("direct cycle source");
        let direct_replacement = store
            .create_work(
                &root_request(
                    "project-supersession-matrix",
                    "matrix-direct-replacement",
                    14,
                ),
                &DevelopmentNoopRedactor,
            )
            .expect("direct cycle replacement");
        store
            .add_work_prerequisite(
                &ChangeWorkPrerequisiteRequest {
                    work_id: direct_replacement.work_id,
                    prerequisite_id: direct_source.work_id,
                    expected_revision: direct_replacement.revision,
                    authority: delegated("project-supersession-matrix", "planner"),
                    actor: actor("planner"),
                    idempotency_key: "matrix-direct-prerequisite".into(),
                    changed_at: at(15),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("direct prerequisite");
        assert!(matches!(
            store.dispose_work(
                &supersede(
                    &direct_source,
                    direct_replacement.work_id,
                    "matrix-direct-cycle",
                    16,
                ),
                &DevelopmentNoopRedactor,
            ),
            Err(StoreError::WorkDependencyCycle)
        ));

        let transitive_source = store
            .create_work(
                &root_request(
                    "project-supersession-matrix",
                    "matrix-transitive-source",
                    17,
                ),
                &DevelopmentNoopRedactor,
            )
            .expect("transitive cycle source");
        let transitive_middle = store
            .create_work(
                &root_request(
                    "project-supersession-matrix",
                    "matrix-transitive-middle",
                    18,
                ),
                &DevelopmentNoopRedactor,
            )
            .expect("transitive cycle middle");
        let transitive_replacement = store
            .create_work(
                &root_request(
                    "project-supersession-matrix",
                    "matrix-transitive-replacement",
                    19,
                ),
                &DevelopmentNoopRedactor,
            )
            .expect("transitive cycle replacement");
        store
            .add_work_prerequisite(
                &ChangeWorkPrerequisiteRequest {
                    work_id: transitive_middle.work_id,
                    prerequisite_id: transitive_source.work_id,
                    expected_revision: transitive_middle.revision,
                    authority: delegated("project-supersession-matrix", "planner"),
                    actor: actor("planner"),
                    idempotency_key: "matrix-transitive-first".into(),
                    changed_at: at(20),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("first transitive prerequisite");
        store
            .add_work_prerequisite(
                &ChangeWorkPrerequisiteRequest {
                    work_id: transitive_replacement.work_id,
                    prerequisite_id: transitive_middle.work_id,
                    expected_revision: transitive_replacement.revision,
                    authority: delegated("project-supersession-matrix", "planner"),
                    actor: actor("planner"),
                    idempotency_key: "matrix-transitive-second".into(),
                    changed_at: at(21),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("second transitive prerequisite");
        assert!(matches!(
            store.dispose_work(
                &supersede(
                    &transitive_source,
                    transitive_replacement.work_id,
                    "matrix-transitive-cycle",
                    22,
                ),
                &DevelopmentNoopRedactor,
            ),
            Err(StoreError::WorkDependencyCycle)
        ));
    }

    #[test]
    fn local_decomposition_is_atomic_cycle_safe_and_uses_dense_named_feeds() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "project-a", "planner");
        let root = store
            .create_work(
                &root_request("project-a", "root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("create root");
        let replay = store
            .create_work(
                &root_request("project-a", "root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("idempotent create");
        assert_eq!(replay, root);
        assert_eq!(
            store
                .inspect_work(root.work_id, at(0))
                .expect("inspect")
                .availability,
            WorkAvailability::Ready
        );

        let decomposition = store
            .decompose_work(
                &DecomposeWorkRequest {
                    parent_id: root.work_id,
                    expected_parent_revision: root.revision,
                    children: vec![
                        child("required", ChildRequirement::Required, "Required child"),
                        child("optional", ChildRequirement::Optional, "Optional child"),
                    ],
                    prerequisites: vec![ChildWorkPrerequisite {
                        work_key: "optional".into(),
                        prerequisite: WorkDependencyRef::Proposed("required".into()),
                    }],
                    authority: delegated(&root.project_id.0, "planner"),
                    actor: actor("planner"),
                    idempotency_key: "decompose".into(),
                    created_at: at(1),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("decompose");
        let required = &decomposition.children[0];
        let optional = &decomposition.children[1];
        assert!(required.labels.contains(&"local-work".into()));
        assert_eq!(
            store
                .inspect_work(required.work_id, at(2))
                .expect("required")
                .availability,
            WorkAvailability::Ready
        );
        let optional_view = store
            .inspect_work(optional.work_id, at(2))
            .expect("optional");
        assert_eq!(optional_view.availability, WorkAvailability::Blocked);
        assert_eq!(optional_view.blocked_by, vec![required.work_id]);

        let optional_parent_cycle = store.add_work_prerequisite(
            &ChangeWorkPrerequisiteRequest {
                work_id: optional.work_id,
                prerequisite_id: root.work_id,
                expected_revision: optional.revision,
                authority: delegated(&root.project_id.0, "planner"),
                actor: actor("planner"),
                idempotency_key: "optional-parent-cycle".into(),
                changed_at: at(3),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(
            optional_parent_cycle,
            Err(StoreError::WorkDependencyCycle)
        ));
        let nested_ancestor_cycle = store.decompose_work(
            &DecomposeWorkRequest {
                parent_id: optional.work_id,
                expected_parent_revision: optional.revision,
                children: vec![child(
                    "nested-optional",
                    ChildRequirement::Optional,
                    "Nested optional child",
                )],
                prerequisites: vec![ChildWorkPrerequisite {
                    work_key: "nested-optional".into(),
                    prerequisite: WorkDependencyRef::Existing(root.work_id),
                }],
                authority: delegated(&root.project_id.0, "planner"),
                actor: actor("planner"),
                idempotency_key: "nested-ancestor-cycle".into(),
                created_at: at(3),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(
            nested_ancestor_cycle,
            Err(StoreError::WorkDependencyCycle)
        ));

        let cycle = store.add_work_prerequisite(
            &ChangeWorkPrerequisiteRequest {
                work_id: required.work_id,
                prerequisite_id: root.work_id,
                expected_revision: required.revision,
                authority: delegated(&root.project_id.0, "planner"),
                actor: actor("planner"),
                idempotency_key: "union-cycle".into(),
                changed_at: at(3),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(cycle, Err(StoreError::WorkDependencyCycle)));
        assert_eq!(
            store
                .get_work_item(required.work_id)
                .expect("required remains")
                .revision,
            1
        );

        let completed_prerequisite = store
            .create_work(
                &root_request("project-a", "completed-prerequisite", 4),
                &DevelopmentNoopRedactor,
            )
            .expect("create completed prerequisite");
        let completed_claim = claim(
            &mut store,
            &completed_prerequisite,
            "planner",
            "completed-prerequisite-claim",
            5,
            300,
        );
        let completed_evidence = evidence(
            &mut store,
            &completed_prerequisite,
            &completed_claim,
            "planner",
            "completed-prerequisite-evidence",
            6,
        );
        checkpoint(
            &mut store,
            &completed_prerequisite,
            &completed_claim,
            "planner",
            "completed-prerequisite-checkpoint",
            7,
            std::slice::from_ref(&completed_evidence),
        );
        complete(
            &mut store,
            &completed_prerequisite,
            &completed_claim,
            "planner",
            &completed_evidence,
            "completed-prerequisite-complete",
            8,
        )
        .expect("complete prerequisite");
        let completed_target = store.add_work_prerequisite(
            &ChangeWorkPrerequisiteRequest {
                work_id: required.work_id,
                prerequisite_id: completed_prerequisite.work_id,
                expected_revision: required.revision,
                authority: delegated(&root.project_id.0, "planner"),
                actor: actor("planner"),
                idempotency_key: "completed-prerequisite-refusal".into(),
                changed_at: at(9),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(
            completed_target,
            Err(StoreError::WorkPrerequisiteAlreadySatisfied(work_id))
                if work_id == completed_prerequisite.work_id
        ));
        let completed_decomposition = store.decompose_work(
            &DecomposeWorkRequest {
                parent_id: root.work_id,
                expected_parent_revision: decomposition.parent.revision,
                children: vec![child(
                    "completed-existing",
                    ChildRequirement::Optional,
                    "Completed existing prerequisite",
                )],
                prerequisites: vec![ChildWorkPrerequisite {
                    work_key: "completed-existing".into(),
                    prerequisite: WorkDependencyRef::Existing(completed_prerequisite.work_id),
                }],
                authority: delegated(&root.project_id.0, "planner"),
                actor: actor("planner"),
                idempotency_key: "completed-existing-decompose".into(),
                created_at: at(9),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(
            completed_decomposition,
            Err(StoreError::WorkPrerequisiteAlreadySatisfied(work_id))
                if work_id == completed_prerequisite.work_id
        ));

        let terminal_prerequisite = store
            .create_work(
                &root_request("project-a", "terminal-prerequisite", 4),
                &DevelopmentNoopRedactor,
            )
            .expect("create terminal prerequisite");
        let terminal_prerequisite = store
            .dispose_work(
                &DisposeWorkRequest {
                    work_id: terminal_prerequisite.work_id,
                    expected_work_revision: terminal_prerequisite.revision,
                    disposition: WorkDisposition::Cancelled,
                    replacement_id: None,
                    reason: "terminal prerequisites are refused".into(),
                    authority: authority("project-a", "planner"),
                    actor: actor("planner"),
                    idempotency_key: "cancel-terminal-prerequisite".into(),
                    disposed_at: at(5),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("cancel terminal prerequisite");
        let closed_target = store.add_work_prerequisite(
            &ChangeWorkPrerequisiteRequest {
                work_id: required.work_id,
                prerequisite_id: terminal_prerequisite.work_id,
                expected_revision: required.revision,
                authority: delegated(&root.project_id.0, "planner"),
                actor: actor("planner"),
                idempotency_key: "terminal-prerequisite-refusal".into(),
                changed_at: at(6),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(
            closed_target,
            Err(StoreError::WorkNotOpen(work_id)) if work_id == terminal_prerequisite.work_id
        ));

        let before = store
            .connection
            .query_row("SELECT COUNT(*) FROM work_items", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("count before");
        let closed_decomposition = store.decompose_work(
            &DecomposeWorkRequest {
                parent_id: root.work_id,
                expected_parent_revision: decomposition.parent.revision,
                children: vec![child(
                    "closed-existing",
                    ChildRequirement::Required,
                    "Closed existing prerequisite",
                )],
                prerequisites: vec![ChildWorkPrerequisite {
                    work_key: "closed-existing".into(),
                    prerequisite: WorkDependencyRef::Existing(terminal_prerequisite.work_id),
                }],
                authority: delegated(&root.project_id.0, "planner"),
                actor: actor("planner"),
                idempotency_key: "closed-existing-decompose".into(),
                created_at: at(7),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(
            closed_decomposition,
            Err(StoreError::WorkNotOpen(work_id)) if work_id == terminal_prerequisite.work_id
        ));
        assert_eq!(
            before,
            store
                .connection
                .query_row("SELECT COUNT(*) FROM work_items", [], |row| {
                    row.get::<_, i64>(0)
                })
                .expect("count after closed prerequisite refusal")
        );
        let bad = store.decompose_work(
            &DecomposeWorkRequest {
                parent_id: root.work_id,
                expected_parent_revision: decomposition.parent.revision,
                children: vec![child("new", ChildRequirement::Required, "New child")],
                prerequisites: vec![ChildWorkPrerequisite {
                    work_key: "new".into(),
                    prerequisite: WorkDependencyRef::Proposed("missing".into()),
                }],
                authority: delegated(&root.project_id.0, "planner"),
                actor: actor("planner"),
                idempotency_key: "bad-decompose".into(),
                created_at: at(4),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(bad.is_err());
        let after = store
            .connection
            .query_row("SELECT COUNT(*) FROM work_items", [], |row| {
                row.get::<_, i64>(0)
            })
            .expect("count after");
        assert_eq!(before, after);

        let entries = store
            .work_feed_after(&FeedId::Project(root.project_id.clone()), 0, 100)
            .expect("project feed");
        assert!(entries.len() >= 4);
        for (index, entry) in entries.iter().enumerate() {
            assert_eq!(entry.position.position, i64::try_from(index).unwrap() + 1);
        }
    }

    #[test]
    fn focused_work_memory_is_shared_once_while_private_scratch_stays_actor_local() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "project-memory", "planner");
        let root = store
            .create_work(
                &root_request("project-memory", "root-memory", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("create root");
        let owner_session = SessionId("planner".into());
        let work_claim = claim(&mut store, &root, "planner", "work-memory-claim", 1, 300);
        store
            .focus_work_session(&root.project_id, &owner_session, root.work_id, at(1))
            .expect("focus work before capture");
        let shared = store
            .capture_note(
                &NoteRequest {
                    project_id: root.project_id.clone(),
                    task_id: None,
                    work_id: Some(root.work_id),
                    prose: "Constraint: never bypass the focused work safety contract".into(),
                    visibility: NoteVisibility::Shared,
                    kind: None,
                    authority: None,
                    sensitivity: Some(Sensitivity::Internal),
                    title: None,
                    tags: Vec::new(),
                    evidence: Vec::new(),
                    refs: Vec::new(),
                    actor: actor("planner"),
                    idempotency_key: "shared-work-memory".into(),
                    created_at: at(1),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("capture shared work memory");
        assert_eq!(
            shared.scope,
            Scope::Work {
                project: root.project_id.clone(),
                work: root.work_id,
            }
        );
        assert_eq!(shared.cursor, None);
        assert_eq!(shared.work_positions.len(), 9);

        let private = store
            .capture_note(
                &NoteRequest {
                    project_id: root.project_id.clone(),
                    task_id: None,
                    work_id: Some(root.work_id),
                    prose: "scratch: private focused hypothesis".into(),
                    visibility: NoteVisibility::Private,
                    kind: None,
                    authority: None,
                    sensitivity: Some(Sensitivity::Internal),
                    title: None,
                    tags: Vec::new(),
                    evidence: Vec::new(),
                    refs: Vec::new(),
                    actor: actor("planner"),
                    idempotency_key: "private-work-memory".into(),
                    created_at: at(2),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("capture private work memory");
        assert!(
            matches!(private.scope, Scope::Agent { work: Some(work), .. } if work == root.work_id)
        );
        assert!(private.work_positions.is_empty());

        let packet = store
            .build_context(&root.project_id, None, &owner_session, "planner", at(3))
            .expect("build focused work context");
        assert_eq!(packet.header.work_id, Some(root.work_id));
        assert!(
            packet
                .pinned
                .iter()
                .any(|item| item.version == shared.version)
        );
        store
            .show_memory(
                &private.version,
                &root.project_id,
                Some(crate::TaskId::new()),
                Some(root.work_id),
                &owner_session,
                "planner",
            )
            .expect("unrelated task binding does not hide owned work scratch");

        let restricted = store
            .capture_note(
                &NoteRequest {
                    project_id: root.project_id.clone(),
                    task_id: None,
                    work_id: Some(root.work_id),
                    prose: "restricted: must never appear in focus or search".into(),
                    visibility: NoteVisibility::Shared,
                    kind: None,
                    authority: None,
                    sensitivity: Some(Sensitivity::Restricted),
                    title: None,
                    tags: Vec::new(),
                    evidence: Vec::new(),
                    refs: Vec::new(),
                    actor: actor("planner"),
                    idempotency_key: "restricted-work-memory".into(),
                    created_at: at(3),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("capture restricted work memory");

        let peer_session = SessionId("peer".into());
        store
            .focus_work_session(&root.project_id, &peer_session, root.work_id, at(3))
            .expect("focus peer on root work");
        let owner = store
            .search_work_memories(
                &root.project_id,
                root.work_id,
                &owner_session,
                "planner",
                None,
                Some(20),
            )
            .expect("owner work memories");
        assert_eq!(owner.len(), 2);
        let peer = store
            .search_work_memories(
                &root.project_id,
                root.work_id,
                &peer_session,
                "peer",
                None,
                Some(20),
            )
            .expect("peer work memories");
        assert_eq!(peer.len(), 1);
        assert_eq!(peer[0].version, shared.version);
        store
            .show_memory(
                &shared.version,
                &root.project_id,
                None,
                Some(root.work_id),
                &SessionId("peer".into()),
                "peer",
            )
            .expect("peer can inspect shared work memory");
        assert!(matches!(
            store.show_memory(
                &private.version,
                &root.project_id,
                None,
                Some(root.work_id),
                &SessionId("peer".into()),
                "peer",
            ),
            Err(StoreError::MemoryAccessDenied(_))
        ));
        assert!(matches!(
            store.show_memory(
                &restricted.version,
                &root.project_id,
                None,
                Some(root.work_id),
                &SessionId("planner".into()),
                "planner",
            ),
            Err(StoreError::MemoryAccessDenied(_))
        ));

        let project_feed = store
            .work_feed_after(&FeedId::Project(root.project_id.clone()), 0, 100)
            .expect("project feed");
        assert!(
            project_feed
                .iter()
                .any(|entry| entry.object_hash == shared.version)
        );
        assert!(
            project_feed
                .iter()
                .all(|entry| entry.object_hash != private.version)
        );

        let decomposition = store
            .decompose_work(
                &DecomposeWorkRequest {
                    parent_id: root.work_id,
                    expected_parent_revision: root.revision,
                    children: vec![
                        child("memory-child", ChildRequirement::Optional, "Memory child"),
                        child(
                            "memory-sibling",
                            ChildRequirement::Optional,
                            "Memory sibling",
                        ),
                    ],
                    prerequisites: Vec::new(),
                    authority: WorkPlanningAuthority::Claim {
                        run_id: work_claim.run_id,
                        holder: work_claim.holder.clone(),
                        claim_id: work_claim.claim_id,
                        claim_fence: work_claim.fence,
                        grant: authority("project-memory", "planner").grant,
                    },
                    actor: actor("planner"),
                    idempotency_key: "memory-child-decompose".into(),
                    created_at: at(4),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("create child focus");
        let child_work = &decomposition.children[0];
        let sibling_work_id = decomposition.children[1].work_id;
        store
            .focus_work_session(&root.project_id, &peer_session, child_work.work_id, at(5))
            .expect("move peer focus to child work");
        claim(
            &mut store,
            child_work,
            "peer",
            "child-work-memory-claim",
            5,
            300,
        );
        let child_view = store
            .search_work_memories(
                &root.project_id,
                child_work.work_id,
                &peer_session,
                "peer",
                None,
                Some(20),
            )
            .expect("root-shared memory is applicable from child focus");
        assert!(
            child_view
                .iter()
                .any(|memory| memory.version == shared.version)
        );
        store
            .show_memory(
                &shared.version,
                &root.project_id,
                None,
                Some(child_work.work_id),
                &SessionId("peer".into()),
                "peer",
            )
            .expect("child focus can inspect root-shared memory");
        assert!(matches!(
            store.show_memory(
                &private.version,
                &root.project_id,
                None,
                Some(child_work.work_id),
                &owner_session,
                "planner",
            ),
            Err(StoreError::MemoryAccessDenied(_))
        ));

        assert!(matches!(
            store.search_work_memories(
                &root.project_id,
                root.work_id,
                &peer_session,
                "peer",
                None,
                Some(20),
            ),
            Err(StoreError::InvalidWork(_))
        ));

        let connection_token = store
            .resume_control_connection(&owner_session, at(6))
            .expect("open host control connection");
        let control_binding = store
            .bind_control_session(
                &root.project_id,
                "dummy:WORK-CONTEXT",
                "Fence focused work context",
                &owner_session,
                &connection_token,
                &actor("planner"),
                ControlAssurance::TurnGated,
                &[EffectClass::Observe],
                1,
                "bind-work-context",
                at(6),
            )
            .expect("bind control session");
        let synchronize = store
            .evaluate_control_turn(
                &root.project_id,
                &owner_session,
                &connection_token,
                &control_binding.routing_token,
                &TurnIntent {
                    idempotency_key: "sync-work-context".into(),
                    intent_fingerprint: ObjectHash::from_canonical_bytes(b"sync-work-context"),
                    purpose: TurnPurpose::Ordinary,
                    requested_effects: vec![EffectClass::Observe],
                    resource_intents: Vec::new(),
                },
                at(7),
            )
            .expect("evaluate synchronization turn");
        let ControlTurnDecision::Grant { grant: synchronize } = synchronize else {
            panic!("initial work-context turn must grant");
        };
        let sync_token = synchronize
            .delivery
            .as_ref()
            .expect("initial task synchronization delivery")
            .page
            .delivery_token
            .clone();
        assert!(matches!(
            store
                .begin_control_turn(
                    &root.project_id,
                    &owner_session,
                    &connection_token,
                    &control_binding.routing_token,
                    &synchronize.grant_id,
                    &[sync_token],
                    "begin-sync-work-context",
                    at(8),
                )
                .expect("begin synchronization turn"),
            ControlTurnBeginDecision::Begin { .. }
        ));
        assert!(matches!(
            store
                .checkpoint_control_turn(
                    &root.project_id,
                    &owner_session,
                    &connection_token,
                    &control_binding.routing_token,
                    &synchronize.grant_id,
                    TurnNextIntent::Continue,
                    "checkpoint-sync-work-context",
                    at(9),
                )
                .expect("checkpoint synchronization turn"),
            ControlTurnCheckpointDecision::Checkpointed { .. }
        ));
        let guarded = store
            .evaluate_control_turn(
                &root.project_id,
                &owner_session,
                &connection_token,
                &control_binding.routing_token,
                &TurnIntent {
                    idempotency_key: "guard-work-context".into(),
                    intent_fingerprint: ObjectHash::from_canonical_bytes(b"guard-work-context"),
                    purpose: TurnPurpose::Ordinary,
                    requested_effects: vec![EffectClass::Observe],
                    resource_intents: Vec::new(),
                },
                at(10),
            )
            .expect("evaluate context-only guarded turn");
        let ControlTurnDecision::Grant { grant: guarded } = guarded else {
            panic!("context-only work turn must grant");
        };
        let guarded_delivery = guarded
            .delivery
            .as_ref()
            .expect("work context is delivered even at the task-feed head");
        assert_eq!(
            guarded_delivery.page.from_cursor,
            guarded_delivery.page.to_cursor
        );
        let guarded_token = guarded_delivery.page.delivery_token.clone();
        let private_after_grant = store
            .capture_note(
                &NoteRequest {
                    project_id: root.project_id.clone(),
                    task_id: None,
                    work_id: Some(root.work_id),
                    prose: "Constraint: newly captured private work rules require a fresh packet"
                        .into(),
                    visibility: NoteVisibility::Private,
                    kind: None,
                    authority: None,
                    sensitivity: Some(Sensitivity::Internal),
                    title: None,
                    tags: Vec::new(),
                    evidence: Vec::new(),
                    refs: Vec::new(),
                    actor: actor("planner"),
                    idempotency_key: "private-work-after-grant".into(),
                    created_at: at(11),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("capture owner-private work memory after grant");
        assert!(private_after_grant.work_positions.is_empty());
        assert!(matches!(
            store
                .begin_control_turn(
                    &root.project_id,
                    &owner_session,
                    &connection_token,
                    &control_binding.routing_token,
                    &guarded.grant_id,
                    &[guarded_token],
                    "begin-stale-work-context",
                    at(11),
                )
                .expect("stale work context is a typed refusal"),
            ControlTurnBeginDecision::Refuse {
                code: ControlRefusalCode::DeltaRequired
            }
        ));
        let conflicting = store
            .capture_note(
                &NoteRequest {
                    project_id: root.project_id.clone(),
                    task_id: None,
                    work_id: Some(child_work.work_id),
                    prose: "Constraint: bypass the focused work safety contract".into(),
                    visibility: NoteVisibility::Shared,
                    kind: None,
                    authority: None,
                    sensitivity: Some(Sensitivity::Internal),
                    title: None,
                    tags: Vec::new(),
                    evidence: Vec::new(),
                    refs: Vec::new(),
                    actor: actor("peer"),
                    idempotency_key: "conflicting-work-memory".into(),
                    created_at: at(12),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("capture conflicting root-shared memory from child focus");
        let contradiction = store
            .record_memory_contradiction(
                &root.project_id,
                None,
                Some(child_work.work_id),
                &peer_session,
                "peer",
                &shared.version,
                &conflicting.version,
                "the focused work safety rules cannot both guide execution",
                "work-contradiction",
                actor("peer"),
                at(7),
            )
            .expect("record pure-work contradiction");
        assert!(contradiction.cursor.is_none());
        assert!(!contradiction.work_positions.is_empty());
        assert!(matches!(
            store.build_context(&root.project_id, None, &peer_session, "peer", at(8)),
            Err(StoreError::PinnedContradiction { .. })
        ));
        store
            .focus_work_session(&root.project_id, &peer_session, sibling_work_id, at(9))
            .expect("move peer focus to sibling in the same root");
        assert!(matches!(
            store.build_context(&root.project_id, None, &peer_session, "peer", at(10)),
            Err(StoreError::PinnedContradiction { .. })
        ));
    }

    #[test]
    fn context_explanation_requires_the_current_work_focus() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "project-context-focus", "planner");
        let first = store
            .create_work(
                &root_request("project-context-focus", "context-focus-first", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("first root");
        let second = store
            .create_work(
                &root_request("project-context-focus", "context-focus-second", 1),
                &DevelopmentNoopRedactor,
            )
            .expect("second root");
        let session = SessionId("planner".into());
        store
            .focus_work_session(&first.project_id, &session, first.work_id, at(2))
            .expect("focus first root");
        let packet = store
            .build_context(&first.project_id, None, &session, "planner", at(3))
            .expect("focused context");
        assert_eq!(
            store
                .explain_context(
                    &packet.header.packet_hash,
                    &first.project_id,
                    &session,
                    "planner",
                )
                .expect("current focus remains authorized")
                .work_id,
            Some(first.work_id)
        );

        store
            .focus_work_session(&first.project_id, &session, second.work_id, at(4))
            .expect("change focus");
        assert!(matches!(
            store.explain_context(
                &packet.header.packet_hash,
                &first.project_id,
                &session,
                "planner",
            ),
            Err(StoreError::PacketAccessDenied(_))
        ));
    }

    #[test]
    fn redaction_child_creation_and_unattributed_authority_fail_closed() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "project-policy", "planner");
        install_grant(&mut store, "project-policy", "different-actor");
        let uninstalled = store.create_work(
            &root_request("project-uninstalled", "uninstalled-grant", 0),
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(uninstalled, Err(StoreError::InvalidWork(_))));
        let rejected = store.create_work(
            &root_request("project-policy", "redacted-root", 0),
            &RejectingRedactor,
        );
        assert!(matches!(rejected, Err(StoreError::RedactionRefused(_))));
        let root = store
            .create_work(
                &root_request("project-policy", "root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("root");
        let bad_authority = store.decompose_work(
            &DecomposeWorkRequest {
                parent_id: root.work_id,
                expected_parent_revision: root.revision,
                children: vec![child("child", ChildRequirement::Required, "Child")],
                prerequisites: Vec::new(),
                authority: delegated(&root.project_id.0, "different-actor"),
                actor: actor("planner"),
                idempotency_key: "bad-authority".into(),
                created_at: at(1),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(bad_authority, Err(StoreError::InvalidWork(_))));

        let mut direct_child = root_request("project-policy", "direct-child", 2);
        direct_child.parent_id = Some(root.work_id);
        let direct = store.create_work(&direct_child, &DevelopmentNoopRedactor);
        assert!(matches!(direct, Err(StoreError::InvalidWork(_))));

        let claim = claim(&mut store, &root, "root-agent", "root-claim", 3, 100);
        let evidence = evidence(&mut store, &root, &claim, "root-agent", "root-evidence", 4);
        checkpoint(
            &mut store,
            &root,
            &claim,
            "root-agent",
            "root-checkpoint",
            5,
            std::slice::from_ref(&evidence),
        );
        let mut unverified_drain = completion_request(
            &root,
            &claim,
            "root-agent",
            &evidence,
            "unverified-drain",
            6,
        );
        unverified_drain
            .drain
            .released_resource_leases
            .push("agent-supplied-lease".into());
        let unverified_drain = store.complete_work(&unverified_drain, &DevelopmentNoopRedactor);
        assert!(matches!(
            unverified_drain,
            Err(StoreError::WorkCompletionRefused { .. })
        ));

        let mut request = completion_request(
            &root,
            &claim,
            "root-agent",
            &evidence,
            "no-root-authority",
            7,
        );
        request.root_authority = None;
        let without_root_authority = store.complete_work(&request, &DevelopmentNoopRedactor);
        assert!(matches!(
            without_root_authority,
            Err(StoreError::WorkCompletionRefused { .. })
        ));
        assert_eq!(
            store
                .get_work_item(root.work_id)
                .expect("root remains")
                .lifecycle,
            WorkLifecycle::Open
        );
    }

    #[test]
    fn delegated_planning_cannot_revise_a_foreign_live_claim() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "project-claimed-planning", "planner");
        let root = store
            .create_work(
                &root_request("project-claimed-planning", "root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("root");
        let live_claim = claim(&mut store, &root, "holder", "holder-claim", 1, 300);
        let delegated_result = store.decompose_work(
            &DecomposeWorkRequest {
                parent_id: root.work_id,
                expected_parent_revision: root.revision,
                children: vec![
                    child("first", ChildRequirement::Required, "First"),
                    child("second", ChildRequirement::Required, "Second"),
                ],
                prerequisites: Vec::new(),
                authority: delegated(&root.project_id.0, "planner"),
                actor: actor("planner"),
                idempotency_key: "foreign-delegated-plan".into(),
                created_at: at(2),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(delegated_result, Err(StoreError::InvalidWork(_))));
        assert_eq!(store.get_work_item(root.work_id).unwrap(), root);
        assert_eq!(
            store.current_work_claim(root.work_id).unwrap(),
            Some(live_claim.clone())
        );

        let decomposition = store
            .decompose_work(
                &DecomposeWorkRequest {
                    parent_id: root.work_id,
                    expected_parent_revision: root.revision,
                    children: vec![
                        child("first", ChildRequirement::Required, "First"),
                        child("second", ChildRequirement::Required, "Second"),
                    ],
                    prerequisites: Vec::new(),
                    authority: WorkPlanningAuthority::Claim {
                        run_id: live_claim.run_id,
                        holder: live_claim.holder.clone(),
                        claim_id: live_claim.claim_id,
                        claim_fence: live_claim.fence,
                        grant: authority(&root.project_id.0, "holder").grant,
                    },
                    actor: actor("holder"),
                    idempotency_key: "holder-claim-plan".into(),
                    created_at: at(3),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("claim holder can plan without stranding its claim");
        let rebased_claim = store
            .current_work_claim(root.work_id)
            .unwrap()
            .expect("claim remains active");
        assert_eq!(rebased_claim.claim_id, live_claim.claim_id);
        assert_eq!(rebased_claim.fence, live_claim.fence);
        assert_eq!(
            rebased_claim.accepted_work_revision,
            decomposition.parent.revision
        );
    }

    #[test]
    fn submillisecond_work_and_claim_times_bind_to_millisecond_projections() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "project-fractional-time", "planner");
        let created_at = at(0) + Duration::nanoseconds(999_999_999);
        let mut request = root_request("project-fractional-time", "root", 0);
        request.created_at = created_at;
        let root = store
            .create_work(&request, &DevelopmentNoopRedactor)
            .expect("create work with submillisecond timestamp");
        assert_eq!(store.get_work_item(root.work_id).unwrap(), root);

        install_grant(&mut store, &root.project_id.0, "holder");
        let claimed_at = at(1) + Duration::nanoseconds(999_999_999);
        let claim = store
            .claim_work(
                &ClaimWorkRequest {
                    work_id: root.work_id,
                    expected_work_revision: root.revision,
                    expected_run_id: root.active_run_id.expect("active run"),
                    holder: SessionId("holder".into()),
                    ttl_seconds: 30,
                    authority: authority(&root.project_id.0, "holder"),
                    recovery_authority: None,
                    recovery_reason: None,
                    actor: actor("holder"),
                    idempotency_key: "fractional-claim".into(),
                    claimed_at,
                },
                &DevelopmentNoopRedactor,
            )
            .expect("claim with submillisecond timestamp");
        assert_eq!(store.current_work_claim(root.work_id).unwrap(), Some(claim));
    }

    #[test]
    fn claim_expiry_overflow_is_a_typed_refusal() {
        assert!(matches!(
            claim_expiry(DateTime::<Utc>::MAX_UTC, 1),
            Err(StoreError::InvalidWork(_))
        ));
    }

    #[test]
    fn work_authority_status_reports_installed_revoked_and_unknown_hashes() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        let grant_hash = install_grant(&mut store, "project-authority-status", "holder");

        let installed = store
            .work_authority_grant_status(&grant_hash)
            .expect("installed authority status");
        assert!(installed.installed);
        assert_eq!(installed.subject_actor_id.as_deref(), Some("holder"));
        assert_eq!(installed.issued_by.as_deref(), Some("host-operator"));
        assert_eq!(installed.valid_from, Some(at(-1_000)));
        assert_eq!(installed.valid_until, Some(at(1_000)));
        assert_eq!(installed.revoked_at, None);
        assert!(
            installed
                .operations
                .as_ref()
                .is_some_and(|operations| operations.contains(&WorkAuthorityOperation::Claim))
        );
        assert_eq!(installed.scope, Some(WorkAuthorityScope::Project));

        store
            .revoke_work_authority_grant(
                &grant_hash,
                &actor("host-operator"),
                "operator revoked the installed grant",
                at(1),
                &DevelopmentNoopRedactor,
            )
            .expect("revoke installed authority");
        let revoked = store
            .work_authority_grant_status(&grant_hash)
            .expect("revoked authority status");
        assert!(revoked.installed);
        assert_eq!(revoked.revoked_at, Some(at(1)));

        let unknown_hash = CanonicalObject::freeze(&test_grant(
            "project-authority-status",
            "unknown-holder",
            default_budget(),
            at(1_000),
        ))
        .expect("canonical unknown grant")
        .hash()
        .clone();
        let unknown = store
            .work_authority_grant_status(&unknown_hash)
            .expect("unknown authority status is a successful query");
        assert_eq!(
            unknown,
            WorkAuthorityGrantStatus {
                installed: false,
                subject_actor_id: None,
                issued_by: None,
                valid_from: None,
                valid_until: None,
                revoked_at: None,
                operations: None,
                scope: None,
            }
        );
    }

    #[test]
    fn host_revocation_is_immutable_idempotent_and_blocks_later_authority_use() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        let grant = install_grant(&mut store, "project-revocation", "planner");
        let root = store
            .create_work(
                &root_request("project-revocation", "root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("root before revocation");
        let future_rejection = store.revoke_work_authority_grant(
            &grant,
            &actor("host-operator"),
            "future timestamps cannot delay an irreversible revocation",
            Utc::now() + Duration::hours(1),
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(future_rejection, Err(StoreError::InvalidWork(_))));
        let rejected = store.revoke_work_authority_grant(
            &grant,
            &actor("host-operator"),
            "secret revocation material",
            at(1),
            &RevocationReasonRejectingRedactor,
        );
        assert!(matches!(rejected, Err(StoreError::RedactionRefused(_))));
        let first = store
            .revoke_work_authority_grant(
                &grant,
                &actor("host-operator"),
                "user withdrew standing planning authority",
                at(1),
                &DevelopmentNoopRedactor,
            )
            .expect("revoke authority");
        let replay = store
            .revoke_work_authority_grant(
                &grant,
                &actor("host-operator"),
                "a replay cannot rewrite the immutable reason",
                at(2),
                &DevelopmentNoopRedactor,
            )
            .expect("idempotent revocation");
        assert_eq!(replay, first);
        let refused = store.decompose_work(
            &DecomposeWorkRequest {
                parent_id: root.work_id,
                expected_parent_revision: root.revision,
                children: vec![
                    child("a", ChildRequirement::Required, "A"),
                    child("b", ChildRequirement::Required, "B"),
                ],
                prerequisites: Vec::new(),
                authority: delegated(&root.project_id.0, "planner"),
                actor: actor("planner"),
                idempotency_key: "after-revocation".into(),
                created_at: at(3),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(refused, Err(StoreError::InvalidWork(_))));
        let refused_backdated = store.decompose_work(
            &DecomposeWorkRequest {
                parent_id: root.work_id,
                expected_parent_revision: root.revision,
                children: vec![
                    child("backdated-a", ChildRequirement::Required, "Backdated A"),
                    child("backdated-b", ChildRequirement::Required, "Backdated B"),
                ],
                prerequisites: Vec::new(),
                authority: delegated(&root.project_id.0, "planner"),
                actor: actor("planner"),
                idempotency_key: "backdated-after-revocation".into(),
                created_at: at(0),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(refused_backdated, Err(StoreError::InvalidWork(_))));
        assert!(
            store
                .verify_all()
                .expect("revocation integrity")
                .is_healthy()
        );
        let events_before_corruption = store
            .work_event_tail(root.work_id, 100)
            .expect("event tail before corruption")
            .len();
        store
            .connection
            .execute(
                "DELETE FROM work_authority_revocations WHERE grant_hash = ?1",
                [grant.as_str()],
            )
            .expect("delete revocation projection");
        store
            .connection
            .execute(
                "UPDATE work_authority_grants SET revoked_at_ms = NULL WHERE grant_hash = ?1",
                [grant.as_str()],
            )
            .expect("corrupt revocation projection");
        let report = store.verify_all().expect("revocation corruption report");
        assert!(
            report
                .invalid_work_records
                .iter()
                .any(|record| record.contains("work_authority_revocation"))
        );
        let refused_corrupt_projection = store.decompose_work(
            &DecomposeWorkRequest {
                parent_id: root.work_id,
                expected_parent_revision: root.revision,
                children: vec![
                    child("corrupt-a", ChildRequirement::Required, "Corrupt A"),
                    child("corrupt-b", ChildRequirement::Required, "Corrupt B"),
                ],
                prerequisites: Vec::new(),
                authority: delegated(&root.project_id.0, "planner"),
                actor: actor("planner"),
                idempotency_key: "corrupt-revocation-mutation".into(),
                created_at: at(4),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(
            refused_corrupt_projection,
            Err(StoreError::InvalidWorkProjection(_))
        ));
        assert_eq!(
            store
                .work_event_tail(root.work_id, 100)
                .expect("event tail after refused mutation")
                .len(),
            events_before_corruption
        );
    }

    #[test]
    fn scoped_mutation_ignores_unrelated_corruption_but_refuses_its_target() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "project-scoped-integrity", "planner");
        let healthy = store
            .create_work(
                &root_request("project-scoped-integrity", "healthy", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("healthy root");
        let unrelated = store
            .create_work(
                &root_request("project-scoped-integrity", "unrelated", 1),
                &DevelopmentNoopRedactor,
            )
            .expect("unrelated root");
        store
            .connection
            .execute(
                "UPDATE work_items
                 SET item_json = CAST(json_set(item_json, '$.title', 'corrupt unrelated') AS BLOB)
                 WHERE work_id = ?1",
                [unrelated.work_id.0.to_string()],
            )
            .expect("corrupt unrelated projection");

        let revised = store
            .revise_work(
                &ReviseWorkRequest {
                    work_id: healthy.work_id,
                    expected_revision: healthy.revision,
                    patch: WorkRevisionPatch {
                        title: Some("healthy revision".into()),
                        ..WorkRevisionPatch::default()
                    },
                    authority: delegated(&healthy.project_id.0, "planner"),
                    actor: actor("planner"),
                    idempotency_key: "healthy-revision".into(),
                    updated_at: at(2),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("healthy scoped mutation");
        assert!(
            !store
                .verify_all()
                .expect("doctor")
                .invalid_work_records
                .is_empty()
        );
        let project_feed = FeedId::Project(healthy.project_id.clone());
        let head_before = store
            .work_feed_head(&project_feed)
            .expect("project feed head");
        store
            .connection
            .execute(
                "UPDATE work_items
                 SET item_json = CAST(json_set(item_json, '$.title', 'corrupt target') AS BLOB)
                 WHERE work_id = ?1",
                [healthy.work_id.0.to_string()],
            )
            .expect("corrupt target projection");
        let refused = store.revise_work(
            &ReviseWorkRequest {
                work_id: healthy.work_id,
                expected_revision: revised.revision,
                patch: WorkRevisionPatch {
                    title: Some("must not commit".into()),
                    ..WorkRevisionPatch::default()
                },
                authority: delegated(&healthy.project_id.0, "planner"),
                actor: actor("planner"),
                idempotency_key: "corrupt-target-revision".into(),
                updated_at: at(3),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(refused, Err(StoreError::InvalidWorkProjection(_))));
        assert_eq!(
            store
                .work_feed_head(&project_feed)
                .expect("project feed head"),
            head_before
        );
    }

    #[test]
    fn doctor_binds_work_projections_to_canonical_events_and_scalar_columns() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "project-integrity", "planner");
        let root = store
            .create_work(
                &root_request("project-integrity", "root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("root");
        let healthy = store.verify_all().expect("healthy work integrity report");
        assert!(healthy.is_healthy(), "{healthy:?}");
        assert!(healthy.checked_work_records > 1);

        store
            .connection
            .execute(
                "UPDATE work_items SET priority = 4 WHERE work_id = ?1",
                [root.work_id.0.to_string()],
            )
            .expect("corrupt scalar projection");
        let scalar_corruption = store.verify_all().expect("scalar corruption report");
        assert!(
            scalar_corruption
                .invalid_work_records
                .iter()
                .any(|record| record.contains("scalar_binding"))
        );

        store
            .connection
            .execute(
                "UPDATE work_items SET priority = ?2,
                     item_json = CAST(json_set(item_json, '$.title', 'tampered') AS BLOB)
                 WHERE work_id = ?1",
                params![root.work_id.0.to_string(), root.priority],
            )
            .expect("corrupt JSON projection");
        let json_corruption = store.verify_all().expect("JSON corruption report");
        assert!(
            json_corruption
                .invalid_work_records
                .iter()
                .any(|record| record.starts_with("work_item:"))
        );
        let refused = store.revise_work(
            &ReviseWorkRequest {
                work_id: root.work_id,
                expected_revision: root.revision,
                patch: WorkRevisionPatch {
                    title: Some("must not canonize corruption".into()),
                    ..WorkRevisionPatch::default()
                },
                authority: delegated("project-integrity", "planner"),
                actor: actor("planner"),
                idempotency_key: "corrupt-revision".into(),
                updated_at: at(1),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(refused, Err(StoreError::InvalidWorkProjection(_))));
    }

    #[test]
    fn stale_self_consistent_projection_cannot_authorize_a_new_event() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "project-stale-projection", "planner");
        let original = store
            .create_work(
                &root_request("project-stale-projection", "root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("root");
        let revised = store
            .revise_work(
                &ReviseWorkRequest {
                    work_id: original.work_id,
                    expected_revision: original.revision,
                    patch: WorkRevisionPatch {
                        title: Some("canonical revision two".into()),
                        ..WorkRevisionPatch::default()
                    },
                    authority: delegated("project-stale-projection", "planner"),
                    actor: actor("planner"),
                    idempotency_key: "revision-two".into(),
                    updated_at: at(1),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("revision two");
        assert_eq!(revised.revision, original.revision + 1);
        let event_count_before = store
            .work_event_tail(original.work_id, 100)
            .expect("event tail")
            .len();

        store
            .connection
            .execute(
                "UPDATE work_items SET revision = ?2, updated_at_ms = ?3, item_json = ?4
                 WHERE work_id = ?1",
                params![
                    original.work_id.0.to_string(),
                    original.revision,
                    original.updated_at.timestamp_millis(),
                    serde_json::to_vec(&original).expect("original projection")
                ],
            )
            .expect("restore stale but internally consistent projection");

        let refused = store.revise_work(
            &ReviseWorkRequest {
                work_id: original.work_id,
                expected_revision: original.revision,
                patch: WorkRevisionPatch {
                    title: Some("must not become revision three".into()),
                    ..WorkRevisionPatch::default()
                },
                authority: delegated("project-stale-projection", "planner"),
                actor: actor("planner"),
                idempotency_key: "revision-after-corruption".into(),
                updated_at: at(2),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(refused, Err(StoreError::InvalidWorkProjection(_))));
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT COUNT(*)
                     FROM objects object
                     JOIN work_feed_entries entry ON entry.object_hash = object.object_hash
                     WHERE entry.feed_kind = 'project'
                       AND object.object_kind = 'work_event'
                       AND json_extract(object.canonical_json, '$.work_id') = ?1",
                    [original.work_id.0.to_string()],
                    |row| row.get::<_, i64>(0),
                )
                .expect("event count after refusal"),
            i64::try_from(event_count_before).expect("event count fits i64"),
        );
    }

    #[test]
    fn indexed_feed_work_identity_is_fail_closed_and_doctor_visible() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "project-feed-work-id", "planner");
        let root = store
            .create_work(
                &root_request("project-feed-work-id", "root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("root");
        let other = store
            .create_work(
                &root_request("project-feed-work-id", "other", 1),
                &DevelopmentNoopRedactor,
            )
            .expect("other root");
        claim(&mut store, &root, "planner", "claim-feed-work-id", 2, 120);
        let head_before = store
            .work_feed_head(&FeedId::Project(root.project_id.clone()))
            .expect("project feed head");
        store
            .connection
            .execute(
                "UPDATE work_feed_entries SET work_id = ?2
                 WHERE object_kind = 'work_event'
                   AND work_id = ?1
                   AND object_hash = (
                       SELECT latest_event_hash FROM work_items WHERE work_id = ?1
                   )",
                params![root.work_id.0.to_string(), other.work_id.0.to_string()],
            )
            .expect("corrupt indexed work identity");

        assert!(matches!(
            store.get_work_item(root.work_id),
            Err(StoreError::InvalidWorkProjection(_))
        ));
        let report = store.verify_all().expect("feed identity report");
        assert!(
            report
                .invalid_work_records
                .iter()
                .any(|record| record.contains("work_id_binding"))
        );
        assert!(report.invalid_work_records.iter().any(|record| {
            record == &format!("work_item:{}:latest_event_hash", root.work_id.0)
        }));
        assert_eq!(
            store
                .work_feed_head(&FeedId::Project(root.project_id))
                .expect("unchanged project feed head"),
            head_before
        );
    }

    #[test]
    fn relation_fingerprint_refuses_projection_drift_before_append() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "project-relation-fingerprint", "planner");
        let root = store
            .create_work(
                &root_request("project-relation-fingerprint", "root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("root");
        let decomposition = store
            .decompose_work(
                &DecomposeWorkRequest {
                    parent_id: root.work_id,
                    expected_parent_revision: root.revision,
                    children: vec![
                        child("first", ChildRequirement::Required, "First"),
                        child("second", ChildRequirement::Required, "Second"),
                    ],
                    prerequisites: vec![ChildWorkPrerequisite {
                        work_key: "second".into(),
                        prerequisite: WorkDependencyRef::Proposed("first".into()),
                    }],
                    authority: delegated(&root.project_id.0, "planner"),
                    actor: actor("planner"),
                    idempotency_key: "decompose-relation-fingerprint".into(),
                    created_at: at(1),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("decompose");
        let first = &decomposition.children[0];
        let second = &decomposition.children[1];
        let feed = FeedId::Project(root.project_id.clone());
        let head_before = store.work_feed_head(&feed).expect("project feed head");
        store
            .connection
            .execute(
                "DELETE FROM work_prerequisites
                 WHERE work_id = ?1 AND prerequisite_id = ?2",
                params![second.work_id.0.to_string(), first.work_id.0.to_string()],
            )
            .expect("omit prerequisite projection");

        assert!(matches!(
            store.inspect_work(second.work_id, at(2)),
            Err(StoreError::InvalidWorkProjection(_))
        ));
        let refused = store.revise_work(
            &ReviseWorkRequest {
                work_id: second.work_id,
                expected_revision: second.revision,
                patch: WorkRevisionPatch {
                    title: Some("must not promote omitted relation".into()),
                    ..WorkRevisionPatch::default()
                },
                authority: delegated(&root.project_id.0, "planner"),
                actor: actor("planner"),
                idempotency_key: "refuse-omitted-relation".into(),
                updated_at: at(2),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(refused, Err(StoreError::InvalidWorkProjection(_))));
        assert_eq!(
            store.work_feed_head(&feed).expect("feed after refusal"),
            head_before
        );
        assert!(!store.verify_all().expect("relation report").is_healthy());
    }

    #[test]
    fn work_event_trigger_rejects_null_work_binding() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "project-trigger", "planner");
        let root = store
            .create_work(
                &root_request("project-trigger", "root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("root");
        store
            .connection
            .execute(
                "INSERT INTO work_feed_heads (feed_kind, feed_id, position)
                 VALUES ('project', 'trigger-probe', 0)",
                [],
            )
            .expect("trigger probe feed");
        let event_hash = store
            .connection
            .query_row(
                "SELECT latest_event_hash FROM work_items WHERE work_id = ?1",
                [root.work_id.0.to_string()],
                |row| row.get::<_, String>(0),
            )
            .expect("root latest event");
        assert!(
            store
                .connection
                .execute(
                    "INSERT INTO work_feed_entries (
                         feed_kind, feed_id, position, object_kind, object_hash, work_id
                     ) VALUES ('project', 'trigger-probe', 1, 'work_event', ?1, NULL)",
                    [event_hash],
                )
                .is_err(),
            "schema trigger must reject a work event without work_id"
        );
    }

    #[test]
    fn current_schema_missing_state_tables_is_refused_before_repair_ddl() {
        for table in ["work_claims", "work_feed_entries"] {
            let directory = tempfile::tempdir().expect("temp directory");
            let database = directory.path().join(format!("missing-{table}.sqlite3"));
            let mut store = SqliteStore::open(&database).expect("initialize current schema");
            install_grant(&mut store, "missing-current-table", "planner");
            let root = store
                .create_work(
                    &root_request("missing-current-table", "root", 0),
                    &DevelopmentNoopRedactor,
                )
                .expect("nonempty work state");
            store
                .claim_work(
                    &ClaimWorkRequest {
                        work_id: root.work_id,
                        expected_work_revision: root.revision,
                        expected_run_id: root.active_run_id.expect("active run"),
                        holder: SessionId("planner".into()),
                        ttl_seconds: 60,
                        authority: authority("missing-current-table", "planner"),
                        recovery_authority: None,
                        recovery_reason: None,
                        actor: actor("planner"),
                        idempotency_key: format!("claim-before-dropping-{table}"),
                        claimed_at: at(1),
                    },
                    &DevelopmentNoopRedactor,
                )
                .expect("live claim fixture");
            drop(store);

            let connection = Connection::open(&database).expect("damage fixture");
            match table {
                "work_claims" => connection
                    .execute_batch("DROP TABLE work_claims")
                    .expect("drop claims table"),
                "work_feed_entries" => connection
                    .execute_batch("DROP TABLE work_feed_entries")
                    .expect("drop feed entries table"),
                _ => unreachable!("fixed test table"),
            }
            let before = connection
                .prepare("SELECT name, type, sql FROM sqlite_master ORDER BY name")
                .expect("schema query")
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                })
                .expect("schema rows")
                .collect::<Result<Vec<_>, _>>()
                .expect("schema snapshot");
            drop(connection);

            let Err(error) = SqliteStore::open(&database) else {
                panic!("damaged current schema was accepted");
            };
            assert!(
                matches!(&error, StoreError::InvalidControlProjection(message)
                    if message == crate::storage::DIFFERENT_BUILD_STORE_MESSAGE),
                "unexpected error for {table}: {error}"
            );
            let connection = Connection::open(&database).expect("inspect refused schema");
            let after = connection
                .prepare("SELECT name, type, sql FROM sqlite_master ORDER BY name")
                .expect("schema query")
                .query_map([], |row| {
                    Ok((
                        row.get::<_, String>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, Option<String>>(2)?,
                    ))
                })
                .expect("schema rows")
                .collect::<Result<Vec<_>, _>>()
                .expect("schema snapshot");
            assert_eq!(after, before, "reopen must not recreate {table}");
        }
    }

    #[test]
    fn explicit_projection_repair_rebuilds_missing_work_indexes_and_fts() {
        let directory = tempfile::tempdir().expect("temp directory");
        let database = directory.path().join("missing-indexes.sqlite3");
        let mut store = SqliteStore::open(&database).expect("initialize current schema");
        install_grant(&mut store, "missing-current-index", "planner");
        let root = store
            .create_work(
                &root_request("missing-current-index", "root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("nonempty work state");
        drop(store);
        let connection = Connection::open(&database).expect("index fixture");
        connection
            .execute_batch(
                "DROP INDEX work_items_ready;
                 CREATE INDEX work_items_ready ON work_items(work_id);
                 DROP INDEX work_run_active;
                 CREATE UNIQUE INDEX work_run_active ON work_runs(run_id);
                 DROP TABLE work_catalog_fts;
                 CREATE TABLE work_catalog_fts (
                     work_id TEXT,
                     search_text TEXT
                 ) STRICT;",
            )
            .expect("replace rebuildable projections with wrong definitions");
        drop(connection);

        let Err(error) = SqliteStore::open(&database) else {
            panic!("ordinary open must not repair");
        };
        assert!(
            matches!(error, StoreError::InvalidWorkProjection(message) if message.contains("--repair-projections")),
            "ordinary open should direct the operator to explicit repair"
        );
        let refused = Connection::open(&database).expect("inspect refused repair fixture");
        assert_eq!(
            refused
                .query_row(
                    "SELECT COUNT(*) FROM sqlite_master
                     WHERE name IN ('work_items_ready', 'work_run_active', 'work_catalog_fts')",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .expect("count missing projections"),
            3,
            "ordinary open must preserve the wrong definitions"
        );
        drop(refused);

        let report = SqliteStore::repair_rebuildable_projections(&database)
            .expect("explicitly rebuild work projections");
        assert!(report.is_healthy(), "{report:?}");
        let reopened = SqliteStore::open(&database).expect("open repaired schema");
        assert_eq!(
            reopened.get_work_item(root.work_id).expect("work survives"),
            root
        );
        for index in ["work_items_ready", "work_run_active"] {
            assert_eq!(
                reopened
                    .connection
                    .query_row(
                        "SELECT type FROM sqlite_master WHERE name = ?1",
                        [index],
                        |row| row.get::<_, String>(0),
                    )
                    .expect("rebuilt index"),
                "index"
            );
        }
        assert_eq!(
            reopened
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM work_catalog_fts WHERE work_id = ?1",
                    [root.work_id.0.to_string()],
                    |row| row.get::<_, i64>(0),
                )
                .expect("rebuilt catalog row"),
            1
        );
        reopened
            .connection
            .execute("DELETE FROM work_catalog_fts", [])
            .expect("drift existing work FTS content");
        let corrupt = reopened.verify_all().expect("diagnose work FTS drift");
        assert!(
            corrupt
                .invalid_work_records
                .iter()
                .any(|record| record.starts_with("work_catalog:")),
            "work FTS drift should be visible: {corrupt:?}"
        );
        drop(reopened);
        let repaired = SqliteStore::repair_rebuildable_projections(&database)
            .expect("repair existing work FTS content");
        assert!(repaired.is_healthy(), "{repaired:?}");
    }

    #[test]
    fn projection_repair_rolls_back_when_durable_work_state_is_corrupt() {
        let directory = tempfile::tempdir().expect("temp directory");
        let database = directory.path().join("corrupt-work-repair.sqlite3");
        let mut store = SqliteStore::open(&database).expect("initialize current schema");
        install_grant(&mut store, "corrupt-work-repair", "planner");
        let root = store
            .create_work(
                &root_request("corrupt-work-repair", "root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("create work fixture");
        store
            .connection
            .execute(
                "UPDATE work_items SET priority = priority + 1 WHERE work_id = ?1",
                [root.work_id.0.to_string()],
            )
            .expect("corrupt durable work scalar projection");
        store
            .connection
            .execute("DELETE FROM work_catalog_fts", [])
            .expect("drift rebuildable work projection");
        let before =
            test_database_shape_snapshot(&store.connection).expect("snapshot corrupt work state");
        drop(store);

        let error = SqliteStore::repair_rebuildable_projections(&database)
            .expect_err("repair must refuse corrupt durable work state");
        assert!(
            matches!(&error, StoreError::InvalidControlProjection(message) if message.contains("verification found")),
            "unexpected repair refusal: {error}"
        );
        let after = Connection::open(&database).expect("inspect rolled-back repair");
        assert_eq!(
            test_database_shape_snapshot(&after).expect("snapshot rolled-back repair"),
            before,
            "repair must roll back every projection change when durable work verification fails"
        );
    }

    #[test]
    fn same_name_wrong_work_table_definition_is_refused_without_mutation() {
        let directory = tempfile::tempdir().expect("temp directory");
        let database = directory.path().join("wrong-work-table.sqlite3");
        drop(SqliteStore::open(&database).expect("initialize current schema"));
        let fixture = Connection::open(&database).expect("open work definition fixture");
        fixture
            .execute_batch(
                "PRAGMA foreign_keys = OFF;
                 ALTER TABLE work_item_labels RENAME TO work_item_labels_old;
                 CREATE TABLE work_item_labels (
                     work_id TEXT NOT NULL REFERENCES work_items(work_id) ON DELETE CASCADE,
                     label_key TEXT,
                     PRIMARY KEY(work_id, label_key)
                 ) STRICT;
                 DROP TABLE work_item_labels_old;
                 PRAGMA foreign_keys = ON;",
            )
            .expect("replace work table with a weaker same-name definition");
        let before =
            test_database_shape_snapshot(&fixture).expect("snapshot malformed work schema");
        drop(fixture);

        for operation in [
            SqliteStore::open(&database).map(|_| ()),
            SqliteStore::repair_rebuildable_projections(&database).map(|_| ()),
        ] {
            let error = operation.expect_err("wrong durable work definition must be refused");
            assert!(
                matches!(&error, StoreError::InvalidControlProjection(_)),
                "unexpected exact work-schema diagnostic: {error}"
            );
        }
        let after = Connection::open(&database).expect("inspect refused work schema");
        assert_eq!(
            test_database_shape_snapshot(&after).expect("snapshot refused work schema"),
            before,
            "open and projection repair must not rewrite durable work definitions"
        );
    }

    #[test]
    fn doctor_rejects_tampered_pending_protocol_basis() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        let project = crate::domain::ProjectId("project-pending-attempt".into());
        let session = SessionId("pending-session".into());
        store
            .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
                project_id: &project,
                session_id: &session,
                operation: "work_next",
                idempotency_key: "pending-attempt",
                intent: &serde_json::json!({"query":"ready"}),
                basis: &serde_json::json!({"cursor":0}),
                now: at(0),
            })
            .expect("begin pending attempt");
        store
            .connection
            .execute(
                "UPDATE work_protocol_attempts SET basis_json = ?1
                 WHERE project_id = ?2 AND session_id = ?3
                   AND operation = 'work_next' AND idempotency_key = 'pending-attempt'",
                params![b"{}".as_slice(), project.0, session.0],
            )
            .expect("tamper pending basis");

        let report = store.verify_all().expect("integrity report");
        assert!(report.invalid_work_records.iter().any(|record| {
            record.contains("work_protocol_attempt:project-pending-attempt:pending-session")
        }));
    }

    #[test]
    fn imported_work_requires_a_hash_verified_typed_source_snapshot() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "project-import", "planner");
        let snapshot = |reference: &str, fingerprint: &str, captured_at| WorkSourceSnapshot {
            schema_version: SCHEMA_VERSION,
            adapter_kind: "beads".into(),
            canonical_ref: reference.into(),
            projected: crate::domain::WorkSourceProjection {
                title: Some("Imported work".into()),
                body: None,
                status: Some("open".into()),
                owner: None,
            },
            captured_at,
            source_revision: Some(fingerprint.into()),
            fingerprint: fingerprint.into(),
            canonical_url: Some(format!("https://tracker.invalid/{reference}")),
            payload_hash: CanonicalObject::freeze(&serde_json::json!({
                "reference": reference,
                "fingerprint": fingerprint
            }))
            .expect("payload")
            .hash()
            .clone(),
            raw: std::collections::BTreeMap::default(),
        };

        let valid = snapshot("tracker:ENG-1", "etag-valid", at(0));
        let valid_object = CanonicalObject::freeze(&valid).expect("valid snapshot");
        let transaction = store
            .connection
            .transaction()
            .expect("snapshot transaction");
        SqliteStore::insert_object(&transaction, "work_source_snapshot", &valid_object)
            .expect("store valid snapshot");
        transaction.commit().expect("commit valid snapshot");
        let mut request = root_request("project-import", "valid-import", 1);
        request.origin = WorkOrigin::Imported;
        request.source_snapshot_id = Some(valid_object.hash().clone());
        store
            .create_work(&request, &DevelopmentNoopRedactor)
            .expect("verified imported work");

        let wrong_kind = CanonicalObject::freeze(&snapshot("tracker:ENG-2", "etag-kind", at(2)))
            .expect("wrong-kind snapshot");
        let transaction = store
            .connection
            .transaction()
            .expect("snapshot transaction");
        SqliteStore::insert_object(&transaction, "not_source_snapshot", &wrong_kind)
            .expect("store wrong kind");
        transaction.commit().expect("commit wrong kind");
        let mut request = root_request("project-import", "wrong-kind", 2);
        request.origin = WorkOrigin::Imported;
        request.source_snapshot_id = Some(wrong_kind.hash().clone());
        assert!(matches!(
            store.create_work(&request, &DevelopmentNoopRedactor),
            Err(StoreError::InvalidWork(_))
        ));

        let malformed = CanonicalObject::freeze(&serde_json::json!({"unexpected": true}))
            .expect("malformed typed object");
        let transaction = store
            .connection
            .transaction()
            .expect("snapshot transaction");
        SqliteStore::insert_object(&transaction, "work_source_snapshot", &malformed)
            .expect("store malformed source snapshot");
        transaction.commit().expect("commit malformed snapshot");
        let mut request = root_request("project-import", "malformed", 3);
        request.origin = WorkOrigin::Imported;
        request.source_snapshot_id = Some(malformed.hash().clone());
        assert!(matches!(
            store.create_work(&request, &DevelopmentNoopRedactor),
            Err(StoreError::InvalidWork(_))
        ));

        let corrupt = CanonicalObject::freeze(&snapshot("tracker:ENG-3", "etag-corrupt", at(4)))
            .expect("corrupt snapshot identity");
        store
            .connection
            .execute(
                "INSERT INTO objects (object_hash, object_kind, canonical_json)
                 VALUES (?1, 'work_source_snapshot', CAST('{}' AS BLOB))",
                [corrupt.hash().as_str()],
            )
            .expect("store corrupt snapshot bytes");
        let mut request = root_request("project-import", "corrupt", 4);
        request.origin = WorkOrigin::Imported;
        request.source_snapshot_id = Some(corrupt.hash().clone());
        assert!(matches!(
            store.create_work(&request, &DevelopmentNoopRedactor),
            Err(StoreError::InvalidWork(_))
        ));

        let invalid = snapshot("", "etag-future", at(10));
        let invalid_object = CanonicalObject::freeze(&invalid).expect("invalid snapshot object");
        let transaction = store
            .connection
            .transaction()
            .expect("snapshot transaction");
        SqliteStore::insert_object(&transaction, "work_source_snapshot", &invalid_object)
            .expect("store invalid source snapshot");
        transaction.commit().expect("commit invalid snapshot");
        let mut request = root_request("project-import", "invalid-shape", 5);
        request.origin = WorkOrigin::Imported;
        request.source_snapshot_id = Some(invalid_object.hash().clone());
        assert!(matches!(
            store.create_work(&request, &DevelopmentNoopRedactor),
            Err(StoreError::InvalidWork(_))
        ));
    }

    #[test]
    fn doctor_reconstructs_safety_rows_and_typed_feed_membership() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "project-safety-integrity", "planner");
        let root = store
            .create_work(
                &root_request("project-safety-integrity", "root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("root");
        let decomposition = store
            .decompose_work(
                &DecomposeWorkRequest {
                    parent_id: root.work_id,
                    expected_parent_revision: root.revision,
                    children: vec![
                        child("first", ChildRequirement::Required, "First"),
                        child("second", ChildRequirement::Required, "Second"),
                    ],
                    prerequisites: vec![ChildWorkPrerequisite {
                        work_key: "second".into(),
                        prerequisite: WorkDependencyRef::Proposed("first".into()),
                    }],
                    authority: delegated(&root.project_id.0, "planner"),
                    actor: actor("planner"),
                    idempotency_key: "decompose".into(),
                    created_at: at(1),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("decompose");
        let root = decomposition.parent;
        let first = decomposition.children[0].clone();
        let second = decomposition.children[1].clone();
        let blocker = store
            .add_work_blocker(
                &AddWorkBlockerRequest {
                    work_id: root.work_id,
                    expected_work_revision: root.revision,
                    kind: crate::domain::WorkBlockerKind::Manual,
                    detail: "exercise blocker projection".into(),
                    authority: delegated(&root.project_id.0, "planner"),
                    actor: actor("planner"),
                    idempotency_key: "block".into(),
                    blocked_at: at(2),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("block root");
        let blocked_root = store.get_work_item(root.work_id).expect("blocked root");
        store
            .clear_work_blocker(
                &ClearWorkBlockerRequest {
                    work_id: root.work_id,
                    expected_work_revision: blocked_root.revision,
                    blocker_id: blocker.blocker_id.clone(),
                    authority: delegated(&root.project_id.0, "planner"),
                    actor: actor("planner"),
                    idempotency_key: "clear".into(),
                    cleared_at: at(3),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("clear root blocker");
        let claim = claim(&mut store, &first, "worker", "claim", 4, 100);
        let evidence = evidence(&mut store, &first, &claim, "worker", "evidence", 5);
        checkpoint(
            &mut store,
            &first,
            &claim,
            "worker",
            "checkpoint",
            6,
            std::slice::from_ref(&evidence),
        );
        complete(
            &mut store, &first, &claim, "worker", &evidence, "complete", 7,
        )
        .expect("complete first child");
        let healthy = store
            .verify_all()
            .expect("healthy safety projection report");
        assert!(healthy.is_healthy(), "{healthy:?}");

        let prerequisite_event = store
            .connection
            .query_row(
                "SELECT event_hash FROM work_prerequisites
                 WHERE work_id = ?1 AND prerequisite_id = ?2",
                params![second.work_id.0.to_string(), first.work_id.0.to_string()],
                |row| row.get::<_, String>(0),
            )
            .expect("prerequisite event");
        let other_event = store
            .connection
            .query_row(
                "SELECT object_hash FROM objects
                 WHERE object_kind = 'work_event' AND object_hash != ?1 LIMIT 1",
                [&prerequisite_event],
                |row| row.get::<_, String>(0),
            )
            .expect("other event");
        store
            .connection
            .execute_batch("SAVEPOINT corrupt")
            .expect("savepoint");
        store
            .connection
            .execute(
                "UPDATE work_prerequisites SET event_hash = ?3
                 WHERE work_id = ?1 AND prerequisite_id = ?2",
                params![
                    second.work_id.0.to_string(),
                    first.work_id.0.to_string(),
                    other_event
                ],
            )
            .expect("corrupt prerequisite binding");
        let report = store.verify_all().expect("prerequisite corruption report");
        assert!(
            report
                .invalid_work_records
                .iter()
                .any(|record| record.starts_with("work_prerequisite:"))
        );
        restore_savepoint(&store);

        store
            .connection
            .execute_batch("SAVEPOINT corrupt")
            .expect("savepoint");
        store
            .connection
            .execute(
                "UPDATE work_blockers SET state = 'active' WHERE blocker_id = ?1",
                [&blocker.blocker_id],
            )
            .expect("corrupt blocker state");
        let report = store.verify_all().expect("blocker corruption report");
        assert!(
            report
                .invalid_work_records
                .iter()
                .any(|record| record.contains("event_binding"))
        );
        restore_savepoint(&store);

        store
            .connection
            .execute_batch("SAVEPOINT corrupt")
            .expect("savepoint");
        store
            .connection
            .execute(
                "UPDATE work_run_evidence SET work_id = ?2, run_id = ?3
                 WHERE evidence_hash = ?1",
                params![
                    evidence.as_str(),
                    root.work_id.0.to_string(),
                    root.active_run_id.expect("root run").0.to_string()
                ],
            )
            .expect("move evidence binding");
        let report = store.verify_all().expect("evidence corruption report");
        assert!(
            report
                .invalid_work_records
                .iter()
                .any(|record| record.contains("run_binding"))
        );
        restore_savepoint(&store);

        store
            .connection
            .execute_batch("SAVEPOINT corrupt")
            .expect("savepoint");
        store
            .connection
            .execute(
                "DELETE FROM work_completion_seals WHERE run_id = ?1",
                [claim.run_id.0.to_string()],
            )
            .expect("delete expected seal");
        let report = store.verify_all().expect("seal corruption report");
        assert!(
            report.invalid_work_records.iter().any(
                |record| record.starts_with("completion_seal:") && record.ends_with(":missing")
            )
        );
        restore_savepoint(&store);

        let moved_root = WorkId::new();
        store
            .connection
            .execute_batch("SAVEPOINT corrupt")
            .expect("savepoint");
        store
            .connection
            .execute(
                "INSERT INTO work_feed_heads (feed_kind, feed_id, position)
                 SELECT feed_kind, ?2, position FROM work_feed_heads
                 WHERE feed_kind = 'root_work' AND feed_id = ?1",
                params![root.root_id.0.to_string(), moved_root.0.to_string()],
            )
            .expect("copy root feed head");
        store
            .connection
            .execute(
                "UPDATE work_feed_entries SET feed_id = ?2
                 WHERE feed_kind = 'root_work' AND feed_id = ?1",
                params![root.root_id.0.to_string(), moved_root.0.to_string()],
            )
            .expect("move root feed entries");
        store
            .connection
            .execute(
                "DELETE FROM work_feed_heads
                 WHERE feed_kind = 'root_work' AND feed_id = ?1",
                [root.root_id.0.to_string()],
            )
            .expect("delete original root feed head");
        let report = store.verify_all().expect("feed corruption report");
        assert!(report.invalid_work_records.iter().any(|record| {
            record.contains("wrong_membership") || record.contains("occurrences")
        }));
        restore_savepoint(&store);
        assert!(store.verify_all().expect("restored report").is_healthy());
    }

    #[test]
    fn decomposition_enforces_expiry_depth_fanout_and_cumulative_open_budget() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "project-budget", "planner");
        let root = store
            .create_work(
                &root_request("project-budget", "root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("root");
        let no_children = store.decompose_work(
            &DecomposeWorkRequest {
                parent_id: root.work_id,
                expected_parent_revision: root.revision,
                children: Vec::new(),
                prerequisites: Vec::new(),
                authority: delegated(&root.project_id.0, "planner"),
                actor: actor("planner"),
                idempotency_key: "no-children".into(),
                created_at: at(1),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(no_children, Err(StoreError::InvalidWork(_))));

        let expired_authority = install_delegated_with_budget(
            &mut store,
            "project-budget",
            "planner",
            default_budget(),
            at(1),
        );
        let expired = store.decompose_work(
            &DecomposeWorkRequest {
                parent_id: root.work_id,
                expected_parent_revision: root.revision,
                children: vec![
                    child("a", ChildRequirement::Required, "A"),
                    child("b", ChildRequirement::Required, "B"),
                ],
                prerequisites: Vec::new(),
                authority: expired_authority,
                actor: actor("planner"),
                idempotency_key: "expired-authority".into(),
                created_at: at(1),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(expired, Err(StoreError::InvalidWork(_))));

        let tight_authority = install_delegated_with_budget(
            &mut store,
            "project-budget",
            "planner",
            WorkPlanningBudget {
                max_depth: 1,
                max_open_descendants: 2,
                max_children_per_decomposition: 2,
            },
            at(1_000),
        );
        let first = store
            .decompose_work(
                &DecomposeWorkRequest {
                    parent_id: root.work_id,
                    expected_parent_revision: root.revision,
                    children: vec![
                        child("a", ChildRequirement::Required, "A"),
                        child("b", ChildRequirement::Required, "B"),
                    ],
                    prerequisites: Vec::new(),
                    authority: tight_authority.clone(),
                    actor: actor("planner"),
                    idempotency_key: "within-budget".into(),
                    created_at: at(2),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("first decomposition");
        let cumulative = store.decompose_work(
            &DecomposeWorkRequest {
                parent_id: root.work_id,
                expected_parent_revision: first.parent.revision,
                children: vec![
                    child("c", ChildRequirement::Required, "C"),
                    child("d", ChildRequirement::Required, "D"),
                ],
                prerequisites: Vec::new(),
                authority: tight_authority,
                actor: actor("planner"),
                idempotency_key: "over-cumulative-budget".into(),
                created_at: at(3),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(cumulative, Err(StoreError::InvalidWork(_))));
    }

    #[test]
    fn decomposition_open_descendant_budget_is_root_wide_across_siblings() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "project-root-budget", "planner");
        let root = store
            .create_work(
                &root_request("project-root-budget", "root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("root");
        let bounded = install_delegated_with_budget(
            &mut store,
            "project-root-budget",
            "planner",
            WorkPlanningBudget {
                max_depth: 2,
                max_open_descendants: 4,
                max_children_per_decomposition: 2,
            },
            at(1_000),
        );
        let siblings = store
            .decompose_work(
                &DecomposeWorkRequest {
                    parent_id: root.work_id,
                    expected_parent_revision: root.revision,
                    children: vec![
                        child("left", ChildRequirement::Required, "Left"),
                        child("right", ChildRequirement::Required, "Right"),
                    ],
                    prerequisites: Vec::new(),
                    authority: bounded.clone(),
                    actor: actor("planner"),
                    idempotency_key: "root-siblings".into(),
                    created_at: at(1),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("create sibling branches");
        let left = siblings.children[0].clone();
        let right = siblings.children[1].clone();
        store
            .decompose_work(
                &DecomposeWorkRequest {
                    parent_id: left.work_id,
                    expected_parent_revision: left.revision,
                    children: vec![
                        child("left-a", ChildRequirement::Required, "Left A"),
                        child("left-b", ChildRequirement::Required, "Left B"),
                    ],
                    prerequisites: Vec::new(),
                    authority: bounded.clone(),
                    actor: actor("planner"),
                    idempotency_key: "left-children".into(),
                    created_at: at(2),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("consume the remaining root-wide budget");
        let refused = store.decompose_work(
            &DecomposeWorkRequest {
                parent_id: right.work_id,
                expected_parent_revision: right.revision,
                children: vec![
                    child("right-a", ChildRequirement::Required, "Right A"),
                    child("right-b", ChildRequirement::Required, "Right B"),
                ],
                prerequisites: Vec::new(),
                authority: bounded,
                actor: actor("planner"),
                idempotency_key: "right-children-over-budget".into(),
                created_at: at(3),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(refused, Err(StoreError::InvalidWork(_))));
        assert_eq!(store.work_children(right.work_id).unwrap(), Vec::new());
        assert!(store.verify_all().expect("integrity").is_healthy());
    }

    #[test]
    fn claims_recover_across_connections_and_handoff_fences_old_sessions() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database = directory.path().join("work.db");
        let mut first = SqliteStore::open(&database).expect("first connection");
        install_grant(&mut first, "project-b", "planner");
        let root = first
            .create_work(
                &root_request("project-b", "root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("root");
        let mut second = SqliteStore::open(&database).expect("second connection");
        let initial = claim(&mut first, &root, "agent-a", "claim-a", 1, 10);
        install_grant(&mut second, "project-b", "agent-b");
        let conflict = second.claim_work(
            &ClaimWorkRequest {
                work_id: root.work_id,
                expected_work_revision: root.revision,
                expected_run_id: root.active_run_id.expect("active run"),
                holder: SessionId("agent-b".into()),
                ttl_seconds: 10,
                authority: authority("project-b", "agent-b"),
                recovery_authority: None,
                recovery_reason: None,
                actor: actor("agent-b"),
                idempotency_key: "claim-b-too-soon".into(),
                claimed_at: at(2),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(conflict, Err(StoreError::WorkClaimHeld { .. })));

        let missing_recovery = second.claim_work(
            &ClaimWorkRequest {
                work_id: root.work_id,
                expected_work_revision: root.revision,
                expected_run_id: root.active_run_id.expect("active run"),
                holder: SessionId("agent-b".into()),
                ttl_seconds: 20,
                authority: authority("project-b", "agent-b"),
                recovery_authority: None,
                recovery_reason: None,
                actor: actor("agent-b"),
                idempotency_key: "claim-b-missing-recovery".into(),
                claimed_at: at(12),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(missing_recovery, Err(StoreError::InvalidWork(_))));
        assert_eq!(
            second.current_work_claim(root.work_id).unwrap(),
            Some(initial.clone())
        );

        let whitespace_recovery = second.claim_work(
            &ClaimWorkRequest {
                work_id: root.work_id,
                expected_work_revision: root.revision,
                expected_run_id: root.active_run_id.expect("active run"),
                holder: SessionId("agent-b".into()),
                ttl_seconds: 20,
                authority: authority("project-b", "agent-b"),
                recovery_authority: Some(authority("project-b", "agent-b")),
                recovery_reason: Some("   ".into()),
                actor: actor("agent-b"),
                idempotency_key: "claim-b-empty-recovery-reason".into(),
                claimed_at: at(12),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(
            whitespace_recovery,
            Err(StoreError::InvalidWork(_))
        ));
        assert_eq!(
            second.current_work_claim(root.work_id).unwrap(),
            Some(initial.clone())
        );

        let recovered = claim(&mut second, &root, "agent-b", "claim-b", 12, 20);
        assert_eq!(recovered.claim_id, initial.claim_id);
        assert!(recovered.fence > initial.fence);
        let stale = first.checkpoint_work(
            &CheckpointWorkRequest {
                work_id: root.work_id,
                run_id: initial.run_id,
                expected_work_revision: root.revision,
                holder: SessionId("agent-a".into()),
                claim_id: initial.claim_id,
                claim_fence: initial.fence,
                summary: "stale".into(),
                evidence: Some(Vec::new()),
                actor: actor("agent-a"),
                idempotency_key: "stale-checkpoint".into(),
                checkpointed_at: at(13),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(stale, Err(StoreError::WorkClaimMismatch { .. })));

        let offer = second
            .offer_work_handoff(
                &OfferWorkHandoffRequest {
                    work_id: root.work_id,
                    run_id: recovered.run_id,
                    expected_work_revision: root.revision,
                    from: recovered.holder.clone(),
                    to: SessionId("agent-c".into()),
                    claim_id: recovered.claim_id,
                    claim_fence: recovered.fence,
                    ttl_seconds: 30,
                    checkpoint_summary: "ready for agent-c".into(),
                    actor: actor("agent-b"),
                    idempotency_key: "offer".into(),
                    offered_at: at(14),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("offer handoff");
        second
            .connection
            .execute_batch("SAVEPOINT corrupt")
            .expect("handoff corruption savepoint");
        let mut tampered_offer = offer.clone();
        tampered_offer.to = SessionId("forged-recipient".into());
        second
            .connection
            .execute(
                "UPDATE work_handoff_offers SET offer_json = ?2 WHERE offer_id = ?1",
                params![
                    offer.offer_id.0.to_string(),
                    serde_json::to_vec(&tampered_offer).expect("tampered offer JSON")
                ],
            )
            .expect("tamper offer projection");
        assert!(matches!(
            second.work_handoff_offers(root.work_id),
            Err(StoreError::InvalidWorkProjection(_))
        ));
        let corrupted = second.verify_all().expect("inspect handoff corruption");
        assert!(
            corrupted
                .invalid_work_records
                .iter()
                .any(|record| record.contains("work_handoff_offer"))
        );
        restore_savepoint(&second);
        let blocked_while_pending = second.record_work_evidence(
            &RecordWorkEvidenceRequest {
                work_id: root.work_id,
                run_id: recovered.run_id,
                expected_work_revision: root.revision,
                holder: recovered.holder.clone(),
                claim_id: recovered.claim_id,
                claim_fence: recovered.fence,
                summary: "must not land".into(),
                refs: Vec::new(),
                actor: actor("agent-b"),
                idempotency_key: "pending-write".into(),
                recorded_at: at(15),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(
            blocked_while_pending,
            Err(StoreError::InvalidWork(_))
        ));
        install_grant(&mut first, "project-b", "agent-c");
        let accepted = first
            .accept_work_handoff(
                &AcceptWorkHandoffRequest {
                    work_id: root.work_id,
                    offer_id: offer.offer_id,
                    to: SessionId("agent-c".into()),
                    authority: authority("project-b", "agent-c"),
                    actor: actor("agent-c"),
                    idempotency_key: "accept".into(),
                    accepted_at: at(16),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("accept handoff");
        assert_eq!(accepted.holder, SessionId("agent-c".into()));
        assert!(accepted.fence > recovered.fence);
        let replay = first
            .accept_work_handoff(
                &AcceptWorkHandoffRequest {
                    work_id: root.work_id,
                    offer_id: offer.offer_id,
                    to: SessionId("agent-c".into()),
                    authority: authority("project-b", "agent-c"),
                    actor: actor("agent-c"),
                    idempotency_key: "accept".into(),
                    accepted_at: at(16),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("idempotent accept");
        assert_eq!(replay, accepted);
        let accepted_evidence = evidence(
            &mut first,
            &root,
            &accepted,
            "agent-c",
            "accepted-evidence",
            17,
        );
        let predecessor_checkpoint = complete(
            &mut first,
            &root,
            &accepted,
            "agent-c",
            &accepted_evidence,
            "stale-handoff-complete",
            18,
        );
        assert!(matches!(
            predecessor_checkpoint,
            Err(StoreError::WorkCompletionRefused { .. })
        ));
        checkpoint(
            &mut first,
            &root,
            &accepted,
            "agent-c",
            "accepted-checkpoint",
            19,
            std::slice::from_ref(&accepted_evidence),
        );
        let seal = complete(
            &mut first,
            &root,
            &accepted,
            "agent-c",
            &accepted_evidence,
            "accepted-complete",
            20,
        )
        .expect("complete after current-fence checkpoint");
        assert_eq!(seal.waivers.len(), 1);
        assert_eq!(seal.expected_contributors.len(), 3);
        let run = second.get_work_run(accepted.run_id).expect("persisted run");
        assert_eq!(run.executor, Some(SessionId("agent-c".into())));
    }

    #[test]
    fn same_holder_plain_claim_retakes_a_lapsed_claim_and_replays_exactly() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "same-holder-retake", "planner");
        let root = store
            .create_work(
                &root_request("same-holder-retake", "root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("root");
        let initial = claim(&mut store, &root, "holder", "initial-claim", 1, 2);
        checkpoint(
            &mut store,
            &root,
            &initial,
            "holder",
            "activate-before-plain-retake",
            2,
            &[],
        );
        let active = store
            .current_work_claim(root.work_id)
            .expect("claim after checkpoint")
            .expect("active claim");
        let current_grant = store
            .install_work_authority_grant(
                test_grant(
                    "same-holder-retake",
                    "holder",
                    default_budget(),
                    active.expires_at + Duration::hours(1),
                ),
                &DevelopmentNoopRedactor,
            )
            .expect("current claim authority");
        let request = ClaimWorkRequest {
            work_id: root.work_id,
            expected_work_revision: root.revision,
            expected_run_id: active.run_id,
            holder: active.holder.clone(),
            ttl_seconds: 60,
            authority: LifecycleAuthorityDecision {
                grant: current_grant,
            },
            recovery_authority: None,
            recovery_reason: None,
            actor: actor("holder"),
            idempotency_key: "plain-same-holder-retake".into(),
            claimed_at: active.expires_at,
        };
        let retaken = store
            .claim_work(&request, &DevelopmentNoopRedactor)
            .expect("same holder retakes without recovery ceremony");
        assert_eq!(retaken.claim_id, active.claim_id);
        assert!(retaken.fence > active.fence);
        assert_eq!(
            retaken.expires_at,
            active.expires_at + Duration::seconds(60)
        );
        assert_eq!(
            store
                .get_work_run(retaken.run_id)
                .expect("retaken run")
                .state,
            WorkRunState::Active,
            "ordinary same-holder retake preserves checkpointed run state"
        );
        let events_before_replay =
            canonical_work_events_for_item(&store.connection, root.work_id).expect("claim history");
        assert!(matches!(
            events_before_replay.last().map(|event| &event.transition),
            Some(WorkTransition::Claimed {
                recovered: true,
                ..
            })
        ));

        let replay = store
            .claim_work(&request, &DevelopmentNoopRedactor)
            .expect("exact retake replay");
        assert_eq!(replay, retaken);
        assert_eq!(
            canonical_work_events_for_item(&store.connection, root.work_id)
                .expect("history after replay")
                .len(),
            events_before_replay.len(),
            "exact replay must not append a second retake event"
        );
        assert!(store.verify_all().expect("integrity").is_healthy());
    }

    #[test]
    fn same_holder_plain_retake_uses_only_the_supplied_grant() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "retake-exact-grant", "planner");
        let root = store
            .create_work(
                &root_request("retake-exact-grant", "root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("root");
        let initial = claim(&mut store, &root, "holder", "initial", 1, 1);
        let supplied = authority("retake-exact-grant", "holder");
        let alternate = store
            .install_work_authority_grant(
                test_grant("retake-exact-grant", "holder", default_budget(), at(2_000)),
                &DevelopmentNoopRedactor,
            )
            .expect("install alternate current grant");
        assert_ne!(alternate, supplied.grant);
        store
            .revoke_work_authority_grant(
                &supplied.grant,
                &actor("host-operator"),
                "revoke the grant bound to the caller",
                at(2),
                &DevelopmentNoopRedactor,
            )
            .expect("revoke supplied grant");

        let refused = store.claim_work(
            &ClaimWorkRequest {
                work_id: root.work_id,
                expected_work_revision: root.revision,
                expected_run_id: initial.run_id,
                holder: initial.holder.clone(),
                ttl_seconds: 60,
                authority: supplied,
                recovery_authority: None,
                recovery_reason: None,
                actor: actor("holder"),
                idempotency_key: "same-holder-revoked-grant".into(),
                claimed_at: at(3),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(
            matches!(refused, Err(StoreError::InvalidWork(reason)) if reason.contains("revoked"))
        );
        assert_eq!(
            store
                .current_work_claim(root.work_id)
                .expect("claim after refusal"),
            Some(initial)
        );
        assert_eq!(
            canonical_work_events_for_item(&store.connection, root.work_id)
                .expect("claim history")
                .into_iter()
                .filter(|event| matches!(
                    event.transition,
                    WorkTransition::Claimed {
                        recovered: true,
                        ..
                    }
                ))
                .count(),
            0,
            "storage must not borrow the alternate actor grant"
        );
    }

    #[test]
    fn same_holder_plain_retake_refuses_blocked_and_deferred_work() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "retake-readiness", "planner");
        let grant = store
            .install_work_authority_grant(
                test_grant(
                    "retake-readiness",
                    "holder",
                    default_budget(),
                    at(2_000_000),
                ),
                &DevelopmentNoopRedactor,
            )
            .expect("install long-lived holder grant");
        let authority = LifecycleAuthorityDecision {
            grant: grant.clone(),
        };

        let blocked = store
            .create_work(
                &root_request("retake-readiness", "blocked-root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("blocked root");
        let blocked_claim = store
            .claim_work(
                &ClaimWorkRequest {
                    work_id: blocked.work_id,
                    expected_work_revision: blocked.revision,
                    expected_run_id: blocked.active_run_id.expect("active run"),
                    holder: SessionId("holder".into()),
                    ttl_seconds: 1,
                    authority: authority.clone(),
                    recovery_authority: None,
                    recovery_reason: None,
                    actor: actor("holder"),
                    idempotency_key: "blocked-claim".into(),
                    claimed_at: at(1),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("blocked candidate claim");
        store
            .add_work_blocker(
                &AddWorkBlockerRequest {
                    work_id: blocked.work_id,
                    expected_work_revision: blocked.revision,
                    kind: crate::domain::WorkBlockerKind::Manual,
                    detail: "retake must respect this blocker".into(),
                    authority: WorkPlanningAuthority::Claim {
                        run_id: blocked_claim.run_id,
                        holder: blocked_claim.holder.clone(),
                        claim_id: blocked_claim.claim_id,
                        claim_fence: blocked_claim.fence,
                        grant: grant.clone(),
                    },
                    actor: actor("holder"),
                    idempotency_key: "block-before-retake".into(),
                    blocked_at: at(1),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("block claimed work");
        let blocked = store.get_work_item(blocked.work_id).expect("blocked work");
        let blocked_claim = store
            .current_work_claim(blocked.work_id)
            .expect("blocked claim")
            .expect("current blocked claim");
        let refused = store.claim_work(
            &ClaimWorkRequest {
                work_id: blocked.work_id,
                expected_work_revision: blocked.revision,
                expected_run_id: blocked_claim.run_id,
                holder: blocked_claim.holder.clone(),
                ttl_seconds: 60,
                authority: authority.clone(),
                recovery_authority: None,
                recovery_reason: None,
                actor: actor("holder"),
                idempotency_key: "blocked-plain-retake".into(),
                claimed_at: blocked_claim.expires_at,
            },
            &DevelopmentNoopRedactor,
        );
        assert!(
            matches!(refused, Err(StoreError::InvalidWork(reason)) if reason.contains("Blocked"))
        );

        let deferred = store
            .create_work(
                &root_request("retake-readiness", "deferred-root", 10),
                &DevelopmentNoopRedactor,
            )
            .expect("deferred root");
        let deferred_claim = store
            .claim_work(
                &ClaimWorkRequest {
                    work_id: deferred.work_id,
                    expected_work_revision: deferred.revision,
                    expected_run_id: deferred.active_run_id.expect("active run"),
                    holder: SessionId("holder".into()),
                    ttl_seconds: 1,
                    authority: authority.clone(),
                    recovery_authority: None,
                    recovery_reason: None,
                    actor: actor("holder"),
                    idempotency_key: "deferred-claim".into(),
                    claimed_at: at(11),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("deferred candidate claim");
        store
            .revise_work(
                &ReviseWorkRequest {
                    work_id: deferred.work_id,
                    expected_revision: deferred.revision,
                    patch: WorkRevisionPatch {
                        deferred_until: Some(at(2_000_000)),
                        ..WorkRevisionPatch::default()
                    },
                    authority: WorkPlanningAuthority::Claim {
                        run_id: deferred_claim.run_id,
                        holder: deferred_claim.holder.clone(),
                        claim_id: deferred_claim.claim_id,
                        claim_fence: deferred_claim.fence,
                        grant: grant.clone(),
                    },
                    actor: actor("holder"),
                    idempotency_key: "defer-before-retake".into(),
                    updated_at: at(11),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("defer claimed work");
        let deferred = store
            .get_work_item(deferred.work_id)
            .expect("deferred work");
        let deferred_claim = store
            .current_work_claim(deferred.work_id)
            .expect("deferred claim")
            .expect("current deferred claim");
        let refused = store.claim_work(
            &ClaimWorkRequest {
                work_id: deferred.work_id,
                expected_work_revision: deferred.revision,
                expected_run_id: deferred_claim.run_id,
                holder: deferred_claim.holder.clone(),
                ttl_seconds: 60,
                authority,
                recovery_authority: None,
                recovery_reason: None,
                actor: actor("holder"),
                idempotency_key: "deferred-plain-retake".into(),
                claimed_at: deferred_claim.expires_at,
            },
            &DevelopmentNoopRedactor,
        );
        assert!(
            matches!(refused, Err(StoreError::InvalidWork(reason)) if reason.contains("Deferred"))
        );
    }

    #[test]
    fn same_holder_plain_retake_replays_across_connections() {
        let directory = tempfile::tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let mut first = SqliteStore::open(&database).expect("first connection");
        install_grant(&mut first, "retake-contention", "planner");
        let root = first
            .create_work(
                &root_request("retake-contention", "root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("root");
        let initial = claim(&mut first, &root, "holder", "initial", 1, 1);
        let request = ClaimWorkRequest {
            work_id: root.work_id,
            expected_work_revision: root.revision,
            expected_run_id: initial.run_id,
            holder: initial.holder.clone(),
            ttl_seconds: 60,
            authority: authority("retake-contention", "holder"),
            recovery_authority: None,
            recovery_reason: None,
            actor: actor("holder"),
            idempotency_key: "retake-contention-key".into(),
            claimed_at: at(3),
        };
        let mut second = SqliteStore::open(&database).expect("second connection");
        let retaken = first
            .claim_work(&request, &DevelopmentNoopRedactor)
            .expect("first connection retakes");
        assert!(retaken.fence > initial.fence);
        let replay = second
            .claim_work(&request, &DevelopmentNoopRedactor)
            .expect("second connection replays the retake");
        assert_eq!(replay, retaken);
        assert_eq!(
            canonical_work_events_for_item(&first.connection, root.work_id)
                .expect("claim history")
                .into_iter()
                .filter(|event| matches!(
                    event.transition,
                    WorkTransition::Claimed {
                        recovered: true,
                        ..
                    }
                ))
                .count(),
            1
        );
    }

    #[test]
    fn holder_mutations_renew_refusals_do_not_and_plain_claim_retakes() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "project-renewal", "planner");
        let root = store
            .create_work(
                &root_request("project-renewal", "renewal-root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("root");
        let initial = claim(&mut store, &root, "holder", "renewal-claim", 1, 10);
        assert_eq!(initial.expires_at, at(11));

        evidence(&mut store, &root, &initial, "holder", "renewal-evidence", 2);
        let after_evidence = store
            .current_work_claim(root.work_id)
            .expect("claim after evidence")
            .expect("live claim");
        assert_eq!(
            after_evidence.expires_at,
            at(2) + Duration::seconds(DEFAULT_WORK_CLAIM_TTL_SECONDS)
        );
        assert_eq!(after_evidence.revision, initial.revision + 1);

        checkpoint(
            &mut store,
            &root,
            &after_evidence,
            "holder",
            "renewal-checkpoint",
            3,
            &[],
        );
        let after_checkpoint = store
            .current_work_claim(root.work_id)
            .expect("claim after checkpoint")
            .expect("live claim");
        assert_eq!(
            after_checkpoint.expires_at,
            at(3) + Duration::seconds(DEFAULT_WORK_CLAIM_TTL_SECONDS)
        );
        assert_eq!(after_checkpoint.revision, after_evidence.revision + 1);

        let revised = store
            .revise_work(
                &ReviseWorkRequest {
                    work_id: root.work_id,
                    expected_revision: root.revision,
                    patch: WorkRevisionPatch {
                        priority: Some(2),
                        ..WorkRevisionPatch::default()
                    },
                    authority: WorkPlanningAuthority::Claim {
                        run_id: after_checkpoint.run_id,
                        holder: after_checkpoint.holder.clone(),
                        claim_id: after_checkpoint.claim_id,
                        claim_fence: after_checkpoint.fence,
                        grant: authority("project-renewal", "holder").grant,
                    },
                    actor: actor("holder"),
                    idempotency_key: "renewal-revise".into(),
                    updated_at: at(4),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("claim-bound revision");
        let after_revision = store
            .current_work_claim(root.work_id)
            .expect("claim after revision")
            .expect("live claim");
        assert_eq!(
            after_revision.expires_at,
            at(4) + Duration::seconds(DEFAULT_WORK_CLAIM_TTL_SECONDS)
        );
        assert_eq!(after_revision.revision, after_checkpoint.revision + 1);
        assert_eq!(after_revision.accepted_work_revision, revised.revision);

        let offer = store
            .offer_work_handoff(
                &OfferWorkHandoffRequest {
                    work_id: revised.work_id,
                    run_id: after_revision.run_id,
                    expected_work_revision: revised.revision,
                    from: after_revision.holder.clone(),
                    to: SessionId("next-holder".into()),
                    claim_id: after_revision.claim_id,
                    claim_fence: after_revision.fence,
                    ttl_seconds: 30,
                    checkpoint_summary: "handoff renewal checkpoint".into(),
                    actor: actor("holder"),
                    idempotency_key: "renewal-offer".into(),
                    offered_at: at(5),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("handoff offer");
        let after_offer = store
            .current_work_claim(root.work_id)
            .expect("claim after offer")
            .expect("live claim");
        assert_eq!(
            after_offer.expires_at,
            at(5) + Duration::seconds(DEFAULT_WORK_CLAIM_TTL_SECONDS)
        );
        assert_eq!(after_offer.revision, after_revision.revision + 1);
        assert_eq!(offer.expires_at, at(35));

        store
            .cancel_work_handoff(
                &CancelWorkHandoffRequest {
                    work_id: revised.work_id,
                    run_id: after_offer.run_id,
                    expected_work_revision: revised.revision,
                    holder: after_offer.holder.clone(),
                    offer_id: offer.offer_id,
                    claim_id: after_offer.claim_id,
                    claim_fence: after_offer.fence,
                    reason: "continue with the current holder".into(),
                    actor: actor("holder"),
                    idempotency_key: "renewal-cancel".into(),
                    cancelled_at: at(6),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("cancel handoff");
        let after_cancel = store
            .current_work_claim(root.work_id)
            .expect("claim after cancellation")
            .expect("live claim");
        assert_eq!(
            after_cancel.expires_at,
            at(6) + Duration::seconds(DEFAULT_WORK_CLAIM_TTL_SECONDS)
        );
        assert_eq!(after_cancel.revision, after_offer.revision + 1);

        let refused_at = at(7);
        assert!(matches!(
            store.checkpoint_work(
                &CheckpointWorkRequest {
                    work_id: revised.work_id,
                    run_id: after_cancel.run_id,
                    expected_work_revision: revised.revision - 1,
                    holder: after_cancel.holder.clone(),
                    claim_id: after_cancel.claim_id,
                    claim_fence: after_cancel.fence,
                    summary: "stale revision must not renew".into(),
                    evidence: Some(Vec::new()),
                    actor: actor("holder"),
                    idempotency_key: "refused-renewal".into(),
                    checkpointed_at: refused_at,
                },
                &DevelopmentNoopRedactor,
            ),
            Err(StoreError::WorkRevisionConflict { .. })
        ));
        assert_eq!(
            store
                .current_work_claim(root.work_id)
                .expect("claim after refused checkpoint")
                .expect("live claim"),
            after_cancel,
            "a refused holder mutation must not renew the claim"
        );

        let retake_root = store
            .create_work(
                &root_request("project-renewal", "retake-renewal-root", 10),
                &DevelopmentNoopRedactor,
            )
            .expect("second root");
        let lapsed = claim(
            &mut store,
            &retake_root,
            "holder",
            "lapsed-renewal-claim",
            11,
            2,
        );
        checkpoint(
            &mut store,
            &retake_root,
            &lapsed,
            "holder",
            "activate-before-lapse",
            12,
            &[],
        );
        let active = store
            .current_work_claim(retake_root.work_id)
            .expect("claim after activating checkpoint")
            .expect("active claim");
        let retaken_at = active.expires_at;
        let current_grant = store
            .install_work_authority_grant(
                test_grant(
                    "project-renewal",
                    "holder",
                    default_budget(),
                    retaken_at + Duration::hours(1),
                ),
                &DevelopmentNoopRedactor,
            )
            .expect("install current retake authority");
        let retaken_claim = store
            .claim_work(
                &ClaimWorkRequest {
                    work_id: retake_root.work_id,
                    expected_work_revision: retake_root.revision,
                    expected_run_id: active.run_id,
                    holder: active.holder.clone(),
                    ttl_seconds: DEFAULT_WORK_CLAIM_TTL_SECONDS,
                    authority: LifecycleAuthorityDecision {
                        grant: current_grant,
                    },
                    recovery_authority: None,
                    recovery_reason: None,
                    actor: ActorContext {
                        source_tool: Some("work_update".into()),
                        reason: "claim ambient local work".into(),
                        ..actor("holder")
                    },
                    idempotency_key: "plain-retake-before-checkpoint".into(),
                    claimed_at: retaken_at,
                },
                &DevelopmentNoopRedactor,
            )
            .expect("holder plain retake");
        assert!(retaken_claim.fence > active.fence);
        assert_eq!(
            store
                .get_work_run(retaken_claim.run_id)
                .expect("run after retake")
                .state,
            WorkRunState::Active,
            "plain retake preserves checkpointed run state"
        );
        let retake_event = canonical_work_events_for_item(&store.connection, retake_root.work_id)
            .expect("retake history")
            .pop()
            .expect("retake event");
        assert_eq!(
            retake_event.actor.source_tool.as_deref(),
            Some("work_update")
        );
        assert_eq!(retake_event.actor.reason, "claim ambient local work");
        store
            .checkpoint_work(
                &CheckpointWorkRequest {
                    work_id: retake_root.work_id,
                    run_id: retaken_claim.run_id,
                    expected_work_revision: retake_root.revision,
                    holder: retaken_claim.holder.clone(),
                    claim_id: retaken_claim.claim_id,
                    claim_fence: retaken_claim.fence,
                    summary: "resume after plain retake".into(),
                    evidence: Some(Vec::new()),
                    actor: actor("holder"),
                    idempotency_key: "retaken-renewal".into(),
                    checkpointed_at: retaken_at,
                },
                &DevelopmentNoopRedactor,
            )
            .expect("checkpoint revalidates the preflight fence");
        let after_retake_checkpoint = store
            .current_work_claim(retake_root.work_id)
            .expect("claim after retake")
            .expect("active retaken claim");
        assert_eq!(after_retake_checkpoint.fence, retaken_claim.fence);
        assert_eq!(
            after_retake_checkpoint.expires_at,
            retaken_at + Duration::seconds(DEFAULT_WORK_CLAIM_TTL_SECONDS)
        );
        assert!(store.verify_all().expect("integrity").is_healthy());
    }

    #[test]
    fn shared_work_capture_requires_the_exact_live_holder_and_renews_once() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        let project = ProjectId("project-work-note-claim".into());
        install_grant(&mut store, &project.0, "planner");
        let root = store
            .create_work(
                &root_request(&project.0, "work-note-root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("root");
        let initial = claim(&mut store, &root, "holder", "work-note-claim", 1, 10);
        let holder = SessionId("holder".into());
        let peer = SessionId("peer".into());
        store
            .focus_work_session(&project, &holder, root.work_id, at(1))
            .expect("holder focus");
        store
            .focus_work_session(&project, &peer, root.work_id, at(1))
            .expect("peer focus");

        let request = NoteRequest {
            project_id: project.clone(),
            task_id: None,
            work_id: Some(root.work_id),
            prose: "Evidence: the exact claim holder captured shared work memory".into(),
            visibility: NoteVisibility::Shared,
            kind: None,
            authority: None,
            sensitivity: None,
            title: None,
            tags: Vec::new(),
            evidence: Vec::new(),
            refs: vec!["test:work-note-holder".into()],
            actor: actor("holder"),
            idempotency_key: "work-note-holder".into(),
            created_at: at(2),
        };
        let receipt = store
            .capture_note(&request, &DevelopmentNoopRedactor)
            .expect("live holder capture");
        assert_eq!(receipt.work_positions.len(), 9);
        let renewed = store
            .current_work_claim(root.work_id)
            .expect("claim after capture")
            .expect("live claim");
        assert_eq!(renewed.revision, initial.revision + 1);
        assert_eq!(
            renewed.expires_at,
            at(2) + Duration::seconds(DEFAULT_WORK_CLAIM_TTL_SECONDS)
        );
        let latest = canonical_work_events_for_item(&store.connection, root.work_id)
            .expect("canonical work history")
            .pop()
            .expect("memory capture event");
        assert_eq!(latest.claim.as_ref(), Some(&renewed));
        assert!(matches!(
            latest.transition,
            WorkTransition::MemoryCaptured { version, assertion }
                if version == receipt.version && assertion == receipt.assertion
        ));

        let replay = store
            .capture_note(&request, &DevelopmentNoopRedactor)
            .expect("exact replay");
        assert!(replay.duplicate);
        assert_eq!(
            store
                .current_work_claim(root.work_id)
                .expect("claim after replay")
                .expect("live claim"),
            renewed
        );

        let mut foreign = request.clone();
        foreign.actor = actor("peer");
        foreign.idempotency_key = "work-note-peer".into();
        foreign.created_at = at(3);
        assert!(matches!(
            store.capture_note(&foreign, &DevelopmentNoopRedactor),
            Err(StoreError::WorkClaimMismatch { work }) if work == root.work_id
        ));
        assert_eq!(
            store
                .current_work_claim(root.work_id)
                .expect("claim after foreign refusal")
                .expect("live claim"),
            renewed
        );

        let mut lapsed = request;
        lapsed.idempotency_key = "work-note-lapsed".into();
        lapsed.created_at = renewed.expires_at;
        assert!(matches!(
            store.capture_note(&lapsed, &DevelopmentNoopRedactor),
            Err(StoreError::WorkClaimLapsed { work, .. }) if work == root.work_id
        ));
        let current_authority = install_delegated_with_budget(
            &mut store,
            &project.0,
            "holder",
            default_budget(),
            at(10_000),
        );
        let WorkPlanningAuthority::Delegated { grant } = current_authority else {
            panic!("test helper returns delegated authority");
        };
        let retaken_claim = store
            .claim_work(
                &ClaimWorkRequest {
                    work_id: root.work_id,
                    expected_work_revision: root.revision,
                    expected_run_id: renewed.run_id,
                    holder: renewed.holder.clone(),
                    ttl_seconds: DEFAULT_WORK_CLAIM_TTL_SECONDS,
                    authority: LifecycleAuthorityDecision { grant },
                    recovery_authority: None,
                    recovery_reason: None,
                    actor: ActorContext {
                        source_tool: Some("work_update".into()),
                        reason: "claim ambient local work".into(),
                        ..actor("holder")
                    },
                    idempotency_key: "plain-retake-before-note".into(),
                    claimed_at: lapsed.created_at,
                },
                &DevelopmentNoopRedactor,
            )
            .expect("own lapsed claim retake");
        let lapsed_receipt = store
            .capture_note(&lapsed, &DevelopmentNoopRedactor)
            .expect("note capture uses the retaken claim");
        let retaken = store
            .current_work_claim(root.work_id)
            .expect("claim after note capture")
            .expect("retaken claim");
        assert_eq!(retaken.fence, retaken_claim.fence);
        assert!(retaken.fence > renewed.fence);
        assert_eq!(
            retaken.expires_at,
            lapsed.created_at + Duration::seconds(DEFAULT_WORK_CLAIM_TTL_SECONDS)
        );
        let events_before_replay =
            canonical_work_events_for_item(&store.connection, root.work_id).expect("work history");
        assert!(matches!(
            events_before_replay
                .iter()
                .rev()
                .nth(1)
                .map(|event| &event.transition),
            Some(WorkTransition::Claimed {
                recovered: true,
                ..
            })
        ));
        let replay = store
            .capture_note(&lapsed, &DevelopmentNoopRedactor)
            .expect("exact post-retake note replay");
        assert!(replay.duplicate);
        assert_eq!(replay.version, lapsed_receipt.version);
        assert_eq!(
            canonical_work_events_for_item(&store.connection, root.work_id)
                .expect("history after replay")
                .len(),
            events_before_replay.len()
        );
        assert_eq!(
            store
                .current_work_claim(root.work_id)
                .expect("claim after replay")
                .expect("retaken claim"),
            retaken
        );
        assert!(store.verify_all().expect("integrity").is_healthy());
    }

    #[test]
    fn release_requires_nonempty_waiver_reason_and_persists_audit_reasons() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "project-release-reason", "planner");
        let root = store
            .create_work(
                &root_request("project-release-reason", "root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("root");
        let claim = claim(&mut store, &root, "holder", "claim", 1, 100);
        let request = ReleaseWorkRequest {
            work_id: root.work_id,
            run_id: claim.run_id,
            expected_work_revision: root.revision,
            holder: claim.holder.clone(),
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
            reason: "  planned pause  ".into(),
            waiver_authority: Some(authority("project-release-reason", "holder")),
            waiver_reason: Some("   ".into()),
            actor: actor("holder"),
            idempotency_key: "release-empty-waiver-reason".into(),
            released_at: at(2),
        };
        assert!(matches!(
            store.release_work(&request, &DevelopmentNoopRedactor),
            Err(StoreError::InvalidWork(_))
        ));
        assert_eq!(
            store.current_work_claim(root.work_id).unwrap(),
            Some(claim.clone())
        );

        let released = store
            .release_work(
                &ReleaseWorkRequest {
                    waiver_reason: Some("  holder left before contributing  ".into()),
                    idempotency_key: "release-with-audit-reasons".into(),
                    ..request
                },
                &DevelopmentNoopRedactor,
            )
            .expect("release with attributed waiver");
        assert_eq!(released.state, WorkClaimState::Released);
        let entry = store
            .work_event_tail(root.work_id, 1)
            .expect("release event")
            .pop()
            .expect("release tail");
        let event: WorkEvent =
            load_typed_work_object(&store.connection, &entry.object_hash, "work_event")
                .expect("canonical release event");
        assert!(matches!(
            event.transition,
            WorkTransition::Released { reason, .. } if reason == "planned pause"
        ));
        assert_eq!(
            event
                .root_execution
                .expect("release root execution")
                .waivers[0]
                .reason,
            "holder left before contributing"
        );
        let next_holder = SessionId("next-holder".into());
        assert!(
            !store
                .work_claim_recovery_required(root.work_id, &next_holder)
                .expect("waived holder is already accounted")
        );
        install_grant(&mut store, "project-release-reason", "next-holder");
        let successor = store
            .claim_work(
                &ClaimWorkRequest {
                    work_id: root.work_id,
                    expected_work_revision: root.revision,
                    expected_run_id: root.active_run_id.expect("active run"),
                    holder: next_holder,
                    ttl_seconds: 60,
                    authority: authority("project-release-reason", "next-holder"),
                    recovery_authority: None,
                    recovery_reason: None,
                    actor: actor("next-holder"),
                    idempotency_key: "ordinary-claim-after-waiver".into(),
                    claimed_at: at(3),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("an accounted prior holder needs no second recovery waiver");
        assert_eq!(successor.holder, SessionId("next-holder".into()));
        assert!(store.verify_all().expect("integrity").is_healthy());
    }

    #[test]
    fn expired_handoff_is_audited_and_does_not_block_a_new_offer() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "project-expiry", "planner");
        let root = store
            .create_work(
                &root_request("project-expiry", "root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("root");
        let initial = claim(&mut store, &root, "agent-a", "claim-a", 1, 10);
        let expired_offer = store
            .offer_work_handoff(
                &OfferWorkHandoffRequest {
                    work_id: root.work_id,
                    run_id: initial.run_id,
                    expected_work_revision: root.revision,
                    from: initial.holder.clone(),
                    to: SessionId("agent-b".into()),
                    claim_id: initial.claim_id,
                    claim_fence: initial.fence,
                    ttl_seconds: 10,
                    checkpoint_summary: "handoff that will expire".into(),
                    actor: actor("agent-a"),
                    idempotency_key: "first-offer".into(),
                    offered_at: at(2),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("first offer");
        let renewed = store
            .current_work_claim(root.work_id)
            .expect("renewed claim")
            .expect("current holder remains authoritative");
        let replacement = store
            .offer_work_handoff(
                &OfferWorkHandoffRequest {
                    work_id: root.work_id,
                    run_id: renewed.run_id,
                    expected_work_revision: root.revision,
                    from: renewed.holder.clone(),
                    to: SessionId("agent-d".into()),
                    claim_id: renewed.claim_id,
                    claim_fence: renewed.fence,
                    ttl_seconds: 20,
                    checkpoint_summary: "replacement handoff".into(),
                    actor: actor("agent-a"),
                    idempotency_key: "replacement-offer".into(),
                    offered_at: at(13),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("replacement offer");
        assert_ne!(replacement.offer_id, expired_offer.offer_id);
        let expired_state = store
            .connection
            .query_row(
                "SELECT state FROM work_handoff_offers WHERE offer_id = ?1",
                [expired_offer.offer_id.0.to_string()],
                |row| row.get::<_, String>(0),
            )
            .expect("expired state");
        assert_eq!(expired_state, "expired");
        let expired_events = store
            .work_feed_after(&FeedId::RunExecution(initial.run_id), 0, 100)
            .expect("run feed")
            .into_iter()
            .filter(|entry| entry.object_kind == "work_event")
            .filter_map(|entry| {
                load_typed_work_object::<WorkEvent>(
                    &store.connection,
                    &entry.object_hash,
                    "work_event",
                )
                .ok()
            })
            .filter(|event| matches!(event.transition, WorkTransition::HandoffExpired { .. }))
            .count();
        assert_eq!(expired_events, 1);
    }

    #[test]
    fn expired_handoff_is_swept_before_progress_and_terminal_completion() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "project-terminal-expiry", "planner");
        let root = store
            .create_work(
                &root_request("project-terminal-expiry", "root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("root");
        let claim = claim(&mut store, &root, "agent-a", "claim", 1, 100);
        let offer = store
            .offer_work_handoff(
                &OfferWorkHandoffRequest {
                    work_id: root.work_id,
                    run_id: claim.run_id,
                    expected_work_revision: root.revision,
                    from: claim.holder.clone(),
                    to: SessionId("agent-b".into()),
                    claim_id: claim.claim_id,
                    claim_fence: claim.fence,
                    ttl_seconds: 2,
                    checkpoint_summary: "short-lived transfer".into(),
                    actor: actor("agent-a"),
                    idempotency_key: "offer".into(),
                    offered_at: at(2),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("offer");
        assert_eq!(offer.expires_at, at(4));

        let blocked = store.record_work_evidence(
            &RecordWorkEvidenceRequest {
                work_id: root.work_id,
                run_id: claim.run_id,
                expected_work_revision: root.revision,
                holder: claim.holder.clone(),
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                summary: "must wait".into(),
                refs: Vec::new(),
                actor: actor("agent-a"),
                idempotency_key: "blocked-evidence".into(),
                recorded_at: at(3),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(blocked, Err(StoreError::InvalidWork(_))));

        let evidence = evidence(
            &mut store,
            &root,
            &claim,
            "agent-a",
            "post-expiry-evidence",
            5,
        );
        let offer_state = store
            .connection
            .query_row(
                "SELECT state FROM work_handoff_offers WHERE offer_id = ?1",
                [offer.offer_id.0.to_string()],
                |row| row.get::<_, String>(0),
            )
            .expect("expired offer state");
        assert_eq!(offer_state, "expired");
        checkpoint(
            &mut store,
            &root,
            &claim,
            "agent-a",
            "post-expiry-checkpoint",
            6,
            std::slice::from_ref(&evidence),
        );
        complete(
            &mut store,
            &root,
            &claim,
            "agent-a",
            &evidence,
            "post-expiry-complete",
            7,
        )
        .expect("terminal completion after expired handoff sweep");
    }

    #[test]
    fn outgoing_holder_can_cancel_a_handoff_and_resume_progress() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "project-cancel", "planner");
        let root = store
            .create_work(
                &root_request("project-cancel", "root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("root");
        let claim = claim(&mut store, &root, "agent-a", "claim", 1, 100);
        let offer = store
            .offer_work_handoff(
                &OfferWorkHandoffRequest {
                    work_id: root.work_id,
                    run_id: claim.run_id,
                    expected_work_revision: root.revision,
                    from: claim.holder.clone(),
                    to: SessionId("agent-b".into()),
                    claim_id: claim.claim_id,
                    claim_fence: claim.fence,
                    ttl_seconds: 30,
                    checkpoint_summary: "possible transfer".into(),
                    actor: actor("agent-a"),
                    idempotency_key: "offer".into(),
                    offered_at: at(2),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("offer");
        let cancelled = store
            .cancel_work_handoff(
                &CancelWorkHandoffRequest {
                    work_id: root.work_id,
                    run_id: claim.run_id,
                    expected_work_revision: root.revision,
                    holder: claim.holder.clone(),
                    offer_id: offer.offer_id,
                    claim_id: claim.claim_id,
                    claim_fence: claim.fence,
                    reason: "  destination did not accept  ".into(),
                    actor: actor("agent-a"),
                    idempotency_key: "cancel".into(),
                    cancelled_at: at(3),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("cancel");
        assert_eq!(cancelled.state, WorkHandoffState::Cancelled);
        let cancelled_entry = store
            .work_event_tail(root.work_id, 1)
            .expect("handoff cancellation event")
            .pop()
            .expect("handoff cancellation tail");
        let cancelled_event: WorkEvent = load_typed_work_object(
            &store.connection,
            &cancelled_entry.object_hash,
            "work_event",
        )
        .expect("canonical handoff cancellation event");
        assert!(matches!(
            cancelled_event.transition,
            WorkTransition::HandoffCancelled { reason, .. }
                if reason == "destination did not accept"
        ));
        evidence(&mut store, &root, &claim, "agent-a", "resumed-evidence", 4);
    }

    #[test]
    fn foreign_holder_cannot_commit_an_expired_handoff_sweep() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "project-cancel-auth", "planner");
        let root = store
            .create_work(
                &root_request("project-cancel-auth", "root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("root");
        let claim = claim(&mut store, &root, "agent-a", "claim", 1, 100);
        let offer = store
            .offer_work_handoff(
                &OfferWorkHandoffRequest {
                    work_id: root.work_id,
                    run_id: claim.run_id,
                    expected_work_revision: root.revision,
                    from: claim.holder.clone(),
                    to: SessionId("agent-b".into()),
                    claim_id: claim.claim_id,
                    claim_fence: claim.fence,
                    ttl_seconds: 2,
                    checkpoint_summary: "short transfer window".into(),
                    actor: actor("agent-a"),
                    idempotency_key: "offer".into(),
                    offered_at: at(2),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("offer");
        let events_before = store
            .work_event_tail(root.work_id, 100)
            .expect("event tail")
            .len();

        let refused = store.cancel_work_handoff(
            &CancelWorkHandoffRequest {
                work_id: root.work_id,
                run_id: claim.run_id,
                expected_work_revision: root.revision,
                holder: SessionId("intruder".into()),
                offer_id: offer.offer_id,
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                reason: "expire another holder's offer".into(),
                actor: actor("intruder"),
                idempotency_key: "foreign-cancel".into(),
                cancelled_at: at(5),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(refused, Err(StoreError::InvalidWork(_))));
        let state: String = store
            .connection
            .query_row(
                "SELECT state FROM work_handoff_offers WHERE offer_id = ?1",
                [offer.offer_id.0.to_string()],
                |row| row.get(0),
            )
            .expect("offer state");
        assert_eq!(state, "offered");
        assert_eq!(
            store
                .work_event_tail(root.work_id, 100)
                .expect("event tail")
                .len(),
            events_before
        );
    }

    #[test]
    fn completion_seals_required_children_and_reopen_starts_a_clean_generation() {
        let directory = tempfile::tempdir().expect("tempdir");
        let database = directory.path().join("work.db");
        let (root_id, old_run, reopened_run) = {
            let mut store = SqliteStore::open(&database).expect("store");
            install_grant(&mut store, "project-c", "planner");
            let root = store
                .create_work(
                    &root_request("project-c", "root", 0),
                    &DevelopmentNoopRedactor,
                )
                .expect("root");
            let decomposition = store
                .decompose_work(
                    &DecomposeWorkRequest {
                        parent_id: root.work_id,
                        expected_parent_revision: root.revision,
                        children: vec![
                            child("required", ChildRequirement::Required, "Required child"),
                            child("optional", ChildRequirement::Optional, "Optional child"),
                        ],
                        prerequisites: Vec::new(),
                        authority: delegated(&root.project_id.0, "planner"),
                        actor: actor("planner"),
                        idempotency_key: "decompose".into(),
                        created_at: at(1),
                    },
                    &DevelopmentNoopRedactor,
                )
                .expect("decompose");
            let root = decomposition.parent;
            let required = decomposition.children[0].clone();
            let optional = decomposition.children[1].clone();

            let root_claim = claim(&mut store, &root, "root-agent", "root-claim", 2, 100);
            let root_evidence = evidence(
                &mut store,
                &root,
                &root_claim,
                "root-agent",
                "root-evidence",
                3,
            );
            checkpoint(
                &mut store,
                &root,
                &root_claim,
                "root-agent",
                "root-cp",
                4,
                std::slice::from_ref(&root_evidence),
            );
            let early = complete(
                &mut store,
                &root,
                &root_claim,
                "root-agent",
                &root_evidence,
                "root-too-early",
                5,
            );
            assert!(matches!(
                early,
                Err(StoreError::WorkCompletionRecoveryRequired {
                    cause: WorkCompletionRecoveryCause::RequiredChildUnsealed { child },
                    ..
                }) if child == required.work_id
            ));

            let child_claim = claim(&mut store, &required, "child-agent", "child-claim", 6, 100);
            let child_run =
                load_work_run(&store.connection, child_claim.run_id).expect("required child run");
            let child_binding = ControlWorkBinding {
                root_execution_id: child_run.root_execution_id,
                work_id: required.work_id,
                run_id: child_run.run_id,
                work_revision: child_claim.accepted_work_revision,
                claim_id: child_claim.claim_id,
                claim_fence: child_claim.fence,
            };
            let mut child_actor = actor("child-agent");
            child_actor.run_id = Some(child_run.run_id.0.to_string());
            let child_basis = ExecutionSourceBasis {
                workspace_id: "workspace-child".into(),
                source_revision: "required-child-revision".into(),
            };
            let child_verification = {
                let transaction = store
                    .connection
                    .transaction_with_behavior(TransactionBehavior::Immediate)
                    .expect("child obligation transaction");
                append_control_execution_observation_on(
                    &transaction,
                    &ExecutionObservation {
                        schema_version: SCHEMA_VERSION,
                        project_id: required.project_id.clone(),
                        binding: child_binding.clone(),
                        session_id: SessionId("child-agent".into()),
                        grant_id: "child-obligation-grant".into(),
                        observation_id: "child-source-mutation".into(),
                        action_fingerprint: ObjectHash::from_canonical_bytes(
                            b"write required child",
                        ),
                        effect: EffectClass::MutateLocal,
                        outcome: ExecutionOutcome::Succeeded,
                        source_changed: true,
                        obligation_rule_set: builtin_rule_set_hash(),
                        source_basis: Some(child_basis.clone()),
                        observed_at: Some(at(7)),
                        actor: child_actor.clone(),
                        recorded_at: at(7),
                    },
                )
                .expect("append child mutation");
                let producer = append_control_execution_observation_on(
                    &transaction,
                    &ExecutionObservation {
                        schema_version: SCHEMA_VERSION,
                        project_id: required.project_id.clone(),
                        binding: child_binding.clone(),
                        session_id: SessionId("child-agent".into()),
                        grant_id: "child-obligation-grant".into(),
                        observation_id: "child-verification".into(),
                        action_fingerprint: ObjectHash::from_canonical_bytes(
                            b"cargo test required child",
                        ),
                        effect: EffectClass::Observe,
                        outcome: ExecutionOutcome::Succeeded,
                        source_changed: false,
                        obligation_rule_set: builtin_rule_set_hash(),
                        source_basis: Some(child_basis.clone()),
                        observed_at: Some(at(7)),
                        actor: child_actor.clone(),
                        recorded_at: at(7),
                    },
                )
                .expect("append child verification producer");
                let verification = append_control_verification_evidence_on(
                    &transaction,
                    &VerificationEvidence {
                        schema_version: SCHEMA_VERSION,
                        project_id: required.project_id.clone(),
                        binding: child_binding,
                        session_id: SessionId("child-agent".into()),
                        producer_observation: producer,
                        source_basis: child_basis,
                        environment: None,
                        check_kind: VerificationKind::Test,
                        check_fingerprint: ObjectHash::from_canonical_bytes(
                            b"cargo test required child",
                        ),
                        result: VerificationResult::Passed,
                        completed_at: at(7),
                        summary: "required-child tests passed on the latest source basis".into(),
                        refs: Vec::new(),
                        actor: child_actor,
                        recorded_at: at(7),
                    },
                )
                .expect("append child verification evidence");
                transaction.commit().expect("commit child obligation");
                verification
            };
            let child_evidence = evidence(
                &mut store,
                &required,
                &child_claim,
                "child-agent",
                "child-evidence",
                7,
            );
            checkpoint(
                &mut store,
                &required,
                &child_claim,
                "child-agent",
                "child-cp",
                8,
                &[child_evidence.clone(), child_verification],
            );
            let child_seal = complete(
                &mut store,
                &required,
                &child_claim,
                "child-agent",
                &child_evidence,
                "child-complete",
                9,
            )
            .expect("complete required child");
            assert_eq!(
                child_seal.obligation_schema_version,
                COMPLETION_OBLIGATION_SCHEMA_VERSION
            );
            assert_eq!(child_seal.obligations.len(), 1);
            let child_seal_object =
                CanonicalObject::freeze(&child_seal).expect("freeze valid child completion seal");
            let mut forged_child_seal = child_seal.clone();
            forged_child_seal.environment_schema_version += 1;
            let forged_child_object = CanonicalObject::freeze(&forged_child_seal)
                .expect("freeze child seal with invalid environment basis");
            SqliteStore::insert_object(&store.connection, "completion_seal", &forged_child_object)
                .expect("insert forged child seal fixture");
            store
                .connection
                .execute(
                    "UPDATE work_completion_seals SET seal_hash = ?1, seal_json = ?2
                     WHERE run_id = ?3",
                    params![
                        forged_child_object.hash().as_str(),
                        forged_child_object.bytes(),
                        child_seal.run_id.0.to_string(),
                    ],
                )
                .expect("bind forged child seal projection");
            store
                .connection
                .execute(
                    "UPDATE work_runs SET completion_seal_hash = ?1 WHERE run_id = ?2",
                    params![
                        forged_child_object.hash().as_str(),
                        child_seal.run_id.0.to_string(),
                    ],
                )
                .expect("bind forged child seal to its run");
            let refused_corrupt_child = complete(
                &mut store,
                &root,
                &root_claim,
                "root-agent",
                &root_evidence,
                "root-corrupt-child-environment",
                10,
            );
            assert!(matches!(
                refused_corrupt_child,
                Err(StoreError::InvalidWorkProjection(_))
            ));
            assert!(
                store
                    .verify_all()
                    .expect("corrupt child environment report")
                    .invalid_work_records
                    .iter()
                    .any(|record| record.contains("completion_seal"))
            );
            store
                .connection
                .execute(
                    "UPDATE work_completion_seals SET seal_hash = ?1, seal_json = ?2
                     WHERE run_id = ?3",
                    params![
                        child_seal_object.hash().as_str(),
                        child_seal_object.bytes(),
                        child_seal.run_id.0.to_string(),
                    ],
                )
                .expect("restore valid child seal projection");
            store
                .connection
                .execute(
                    "UPDATE work_runs SET completion_seal_hash = ?1 WHERE run_id = ?2",
                    params![
                        child_seal_object.hash().as_str(),
                        child_seal.run_id.0.to_string(),
                    ],
                )
                .expect("restore valid child seal run binding");
            store
                .connection
                .execute(
                    "DELETE FROM objects WHERE object_hash = ?1",
                    [forged_child_object.hash().as_str()],
                )
                .expect("remove forged child seal fixture");
            let root_seal = complete(
                &mut store,
                &root,
                &root_claim,
                "root-agent",
                &root_evidence,
                "root-complete",
                10,
            )
            .expect("complete root");
            assert_eq!(root_seal.required_child_seals.len(), 1);
            assert_eq!(
                root_seal.required_child_seals[0],
                CanonicalObject::freeze(&child_seal)
                    .expect("freeze child seal")
                    .hash()
                    .clone()
            );
            assert_eq!(
                root_seal.obligation_schema_version,
                COMPLETION_OBLIGATION_SCHEMA_VERSION
            );
            assert!(root_seal.obligations.is_empty());
            assert_eq!(
                store
                    .get_work_item(optional.work_id)
                    .expect("optional")
                    .lifecycle,
                WorkLifecycle::Open
            );
            let completion_tail = store
                .work_feed_after(
                    &FeedId::RunExecution(root_claim.run_id),
                    root_seal.completion_cut.position,
                    10,
                )
                .expect("completion tail");
            assert_eq!(completion_tail.len(), 1);
            assert_eq!(
                completion_tail[0].position.position,
                root_seal.completion_cut.position + 1
            );

            let required_current = store.get_work_item(required.work_id).expect("required");
            install_grant(&mut store, "project-c", "human");
            let child_reopen = store.reopen_work(
                &ReopenWorkRequest {
                    work_id: required.work_id,
                    expected_work_revision: required_current.revision,
                    reason: "invalidate completed child".into(),
                    authority: authority("project-c", "human"),
                    actor: actor("human"),
                    idempotency_key: "child-reopen".into(),
                    reopened_at: at(11),
                },
                &DevelopmentNoopRedactor,
            );
            assert!(matches!(child_reopen, Err(StoreError::InvalidWork(_))));

            let root_current = store.get_work_item(root.work_id).expect("completed root");
            let blocked_root_reopen = store.reopen_work(
                &ReopenWorkRequest {
                    work_id: root.work_id,
                    expected_work_revision: root_current.revision,
                    reason: "unfinished optional child still belongs to the sealed execution"
                        .into(),
                    authority: authority("project-c", "human"),
                    actor: actor("human"),
                    idempotency_key: "root-reopen-before-optional-disposal".into(),
                    reopened_at: at(12),
                },
                &DevelopmentNoopRedactor,
            );
            assert!(matches!(
                blocked_root_reopen,
                Err(StoreError::InvalidWork(_))
            ));
            store
                .dispose_work(
                    &DisposeWorkRequest {
                        work_id: optional.work_id,
                        expected_work_revision: optional.revision,
                        disposition: WorkDisposition::Cancelled,
                        replacement_id: None,
                        reason: "retire optional work omitted by the sealed execution".into(),
                        authority: authority("project-c", "human"),
                        actor: actor("human"),
                        idempotency_key: "dispose-optional-before-root-reopen".into(),
                        disposed_at: at(13),
                    },
                    &DevelopmentNoopRedactor,
                )
                .expect("dispose unfinished optional child");
            let reopened = store
                .reopen_work(
                    &ReopenWorkRequest {
                        work_id: root.work_id,
                        expected_work_revision: root_current.revision,
                        reason: "  new root execution generation  ".into(),
                        authority: authority("project-c", "human"),
                        actor: actor("human"),
                        idempotency_key: "root-reopen".into(),
                        reopened_at: at(14),
                    },
                    &DevelopmentNoopRedactor,
                )
                .expect("reopen root");
            assert_eq!(reopened.generation, 2);
            assert_ne!(reopened.run_id, root_claim.run_id);
            let reopened_entry = store
                .work_event_tail(root.work_id, 1)
                .expect("reopen event")
                .pop()
                .expect("reopen event tail");
            let reopened_event: WorkEvent = load_typed_work_object(
                &store.connection,
                &reopened_entry.object_hash,
                "work_event",
            )
            .expect("canonical reopen event");
            assert!(matches!(
                reopened_event.transition,
                WorkTransition::Reopened { reason, .. }
                    if reason == "new root execution generation"
            ));
            (root.work_id, root_claim.run_id, reopened.run_id)
        };

        let reopened_store = SqliteStore::open(&database).expect("reopen database");
        let item = reopened_store
            .get_work_item(root_id)
            .expect("persisted item");
        assert_eq!(item.lifecycle, WorkLifecycle::Open);
        assert_eq!(item.active_run_id, Some(reopened_run));
        assert_eq!(
            reopened_store.get_work_run(old_run).expect("old run").state,
            WorkRunState::Completed
        );
        assert_eq!(
            reopened_store
                .get_work_run(reopened_run)
                .expect("new run")
                .state,
            WorkRunState::Open
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the regression pins root sealing, stale authority, descendant disposal, and clean-generation reopen as one lifecycle"
    )]
    fn root_completion_fences_live_optional_descendants_and_old_generations() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "project-optional-fence", "planner");
        let root = store
            .create_work(
                &root_request("project-optional-fence", "root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("root");
        let decomposition = store
            .decompose_work(
                &DecomposeWorkRequest {
                    parent_id: root.work_id,
                    expected_parent_revision: root.revision,
                    children: vec![
                        child("live-optional", ChildRequirement::Optional, "Live optional"),
                        child("idle-optional", ChildRequirement::Optional, "Idle optional"),
                    ],
                    prerequisites: Vec::new(),
                    authority: delegated(&root.project_id.0, "planner"),
                    actor: actor("planner"),
                    idempotency_key: "decompose-optionals".into(),
                    created_at: at(1),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("decompose optionals");
        let root = decomposition.parent;
        let live_optional = decomposition.children[0].clone();
        let idle_optional = decomposition.children[1].clone();

        let root_claim = claim(&mut store, &root, "root-agent", "root-claim", 2, 100);
        let root_evidence = evidence(
            &mut store,
            &root,
            &root_claim,
            "root-agent",
            "root-evidence",
            3,
        );
        checkpoint(
            &mut store,
            &root,
            &root_claim,
            "root-agent",
            "root-checkpoint",
            4,
            std::slice::from_ref(&root_evidence),
        );
        let optional_claim = claim(
            &mut store,
            &live_optional,
            "optional-agent",
            "optional-claim",
            5,
            100,
        );
        let optional_evidence = evidence(
            &mut store,
            &live_optional,
            &optional_claim,
            "optional-agent",
            "optional-evidence",
            6,
        );
        checkpoint(
            &mut store,
            &live_optional,
            &optional_claim,
            "optional-agent",
            "optional-checkpoint",
            7,
            std::slice::from_ref(&optional_evidence),
        );
        let expiring_claim = claim(
            &mut store,
            &idle_optional,
            "expiring-agent",
            "expiring-claim",
            5,
            3,
        );
        let expiring_offer = store
            .offer_work_handoff(
                &OfferWorkHandoffRequest {
                    work_id: idle_optional.work_id,
                    run_id: expiring_claim.run_id,
                    expected_work_revision: idle_optional.revision,
                    from: expiring_claim.holder.clone(),
                    to: SessionId("late-recipient".into()),
                    claim_id: expiring_claim.claim_id,
                    claim_fence: expiring_claim.fence,
                    ttl_seconds: 2,
                    checkpoint_summary: "offer expires before root completion".into(),
                    actor: actor("expiring-agent"),
                    idempotency_key: "expiring-offer".into(),
                    offered_at: at(6),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("offer expiring optional handoff");

        let live_descendant = complete(
            &mut store,
            &root,
            &root_claim,
            "root-agent",
            &root_evidence,
            "root-complete-with-live-optional",
            8,
        );
        assert!(matches!(
            live_descendant,
            Err(StoreError::WorkCompletionRefused { .. })
        ));

        store
            .release_work(
                &ReleaseWorkRequest {
                    work_id: live_optional.work_id,
                    run_id: optional_claim.run_id,
                    expected_work_revision: live_optional.revision,
                    holder: optional_claim.holder.clone(),
                    claim_id: optional_claim.claim_id,
                    claim_fence: optional_claim.fence,
                    reason: "  root is sealing without this optional child  ".into(),
                    waiver_authority: None,
                    waiver_reason: None,
                    actor: actor("optional-agent"),
                    idempotency_key: "release-optional".into(),
                    released_at: at(9),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("release optional claim");
        store
            .release_work(
                &ReleaseWorkRequest {
                    work_id: idle_optional.work_id,
                    run_id: expiring_claim.run_id,
                    expected_work_revision: idle_optional.revision,
                    holder: expiring_claim.holder.clone(),
                    claim_id: expiring_claim.claim_id,
                    claim_fence: expiring_claim.fence,
                    reason: "handoff expired before root sealing".into(),
                    waiver_authority: None,
                    waiver_reason: None,
                    actor: actor("expiring-agent"),
                    idempotency_key: "release-expiring-optional".into(),
                    released_at: at(9),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("release renewed claim after handoff expiry");
        let release_entry = store
            .work_event_tail(live_optional.work_id, 1)
            .expect("release event")
            .pop()
            .expect("release event tail");
        let release_event: WorkEvent =
            load_typed_work_object(&store.connection, &release_entry.object_hash, "work_event")
                .expect("canonical release event");
        assert!(matches!(
            release_event.transition,
            WorkTransition::Released { reason, .. }
                if reason == "root is sealing without this optional child"
        ));
        let seal = complete(
            &mut store,
            &root,
            &root_claim,
            "root-agent",
            &root_evidence,
            "root-complete-after-release",
            10,
        )
        .expect("complete root after descendant release");
        let mut expected_unfinished = vec![idle_optional.work_id, live_optional.work_id];
        expected_unfinished.sort_by_key(|work_id| work_id.0);
        assert_eq!(seal.unfinished_optional_children, expected_unfinished);

        install_grant(&mut store, "project-optional-fence", "late-recipient");
        let backdated_accept = store.accept_work_handoff(
            &AcceptWorkHandoffRequest {
                work_id: idle_optional.work_id,
                offer_id: expiring_offer.offer_id,
                to: SessionId("late-recipient".into()),
                authority: authority("project-optional-fence", "late-recipient"),
                actor: actor("late-recipient"),
                idempotency_key: "backdated-post-root-accept".into(),
                accepted_at: at(7),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(backdated_accept, Err(StoreError::InvalidWork(_))));

        let stale_checkpoint = store.checkpoint_work(
            &CheckpointWorkRequest {
                work_id: live_optional.work_id,
                run_id: optional_claim.run_id,
                expected_work_revision: live_optional.revision,
                holder: optional_claim.holder.clone(),
                claim_id: optional_claim.claim_id,
                claim_fence: optional_claim.fence,
                summary: "must remain fenced after root completion".into(),
                evidence: Some(vec![optional_evidence]),
                actor: actor("optional-agent"),
                idempotency_key: "stale-post-root-checkpoint".into(),
                checkpointed_at: at(11),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(
            stale_checkpoint,
            Err(StoreError::WorkClaimMismatch { .. })
        ));
        let blocked = inspect_work_on(&store.connection, live_optional.work_id, at(11))
            .expect("inspect blocked optional");
        assert_eq!(blocked.availability, WorkAvailability::Blocked);
        assert!(
            blocked
                .reason_codes
                .contains(&WorkReadinessReason::ParentDisallowsExecution)
        );

        install_grant(&mut store, "project-optional-fence", "human");
        let root_current = store.get_work_item(root.work_id).expect("completed root");
        let premature_reopen = store.reopen_work(
            &ReopenWorkRequest {
                work_id: root.work_id,
                expected_work_revision: root_current.revision,
                reason: "unfinished descendants must be resolved first".into(),
                authority: authority("project-optional-fence", "human"),
                actor: actor("human"),
                idempotency_key: "premature-root-reopen".into(),
                reopened_at: at(12),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(premature_reopen, Err(StoreError::InvalidWork(_))));

        for (item, key, second) in [
            (&live_optional, "dispose-live-optional", 13),
            (&idle_optional, "dispose-idle-optional", 14),
        ] {
            store
                .dispose_work(
                    &DisposeWorkRequest {
                        work_id: item.work_id,
                        expected_work_revision: item.revision,
                        disposition: WorkDisposition::Cancelled,
                        replacement_id: None,
                        reason: "retire optional work omitted by the completed root".into(),
                        authority: authority("project-optional-fence", "human"),
                        actor: actor("human"),
                        idempotency_key: key.into(),
                        disposed_at: at(second),
                    },
                    &DevelopmentNoopRedactor,
                )
                .expect("dispose optional descendant");
        }
        let reopened = store
            .reopen_work(
                &ReopenWorkRequest {
                    work_id: root.work_id,
                    expected_work_revision: root_current.revision,
                    reason: "start a clean root generation".into(),
                    authority: authority("project-optional-fence", "human"),
                    actor: actor("human"),
                    idempotency_key: "reopen-clean-root".into(),
                    reopened_at: at(15),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("reopen clean root");
        let old_optional_run =
            load_work_run(&store.connection, optional_claim.run_id).expect("old optional run");
        assert_ne!(
            old_optional_run.root_execution_id,
            reopened.root_execution_id
        );
        assert_eq!(old_optional_run.state, WorkRunState::Cancelled);
        assert_eq!(
            store
                .get_work_item(live_optional.work_id)
                .expect("disposed optional")
                .lifecycle,
            WorkLifecycle::Cancelled
        );
        let final_report = store.verify_all().expect("integrity report");
        assert!(final_report.is_healthy(), "{final_report:?}");
    }

    #[test]
    fn completion_refuses_open_obligations_then_seals_the_exact_terminal_basis() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = directory.path().join("engram.sqlite3");
        let mut store = SqliteStore::open(&database).expect("store");
        install_grant(&mut store, "project-completion-obligations", "planner");
        let work = store
            .create_work(
                &root_request(
                    "project-completion-obligations",
                    "create-completion-obligation-work",
                    1,
                ),
                &DevelopmentNoopRedactor,
            )
            .expect("create local work");
        let claim = claim(
            &mut store,
            &work,
            "runner",
            "claim-completion-obligation-work",
            2,
            300,
        );
        let run = load_work_run(&store.connection, claim.run_id).expect("claimed run");
        let binding = ControlWorkBinding {
            root_execution_id: run.root_execution_id,
            work_id: work.work_id,
            run_id: run.run_id,
            work_revision: claim.accepted_work_revision,
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
        };
        let mut run_actor = actor("runner");
        run_actor.run_id = Some(run.run_id.0.to_string());
        let source_basis = ExecutionSourceBasis {
            workspace_id: "workspace-completion".into(),
            source_revision: "revision-after-mutation".into(),
        };
        let mutation = ExecutionObservation {
            schema_version: SCHEMA_VERSION,
            project_id: work.project_id.clone(),
            binding: binding.clone(),
            session_id: SessionId("runner".into()),
            grant_id: "completion-obligation-grant".into(),
            observation_id: "completion-source-mutation".into(),
            action_fingerprint: ObjectHash::from_canonical_bytes(b"write src/lib.rs"),
            effect: EffectClass::MutateLocal,
            outcome: ExecutionOutcome::Succeeded,
            source_changed: true,
            obligation_rule_set: builtin_rule_set_hash(),
            source_basis: Some(source_basis.clone()),
            observed_at: Some(at(3)),
            actor: run_actor.clone(),
            recorded_at: at(3),
        };
        {
            let transaction = store
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .expect("mutation transaction");
            append_control_execution_observation_on(&transaction, &mutation)
                .expect("append mutation and obligation");
            transaction.commit().expect("commit mutation");
        }
        let opened = store
            .work_run_obligations(run.run_id)
            .expect("open obligation");
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0].state, WorkObligationState::Open);

        let generic_evidence = evidence(
            &mut store,
            &work,
            &claim,
            "runner",
            "completion-generic-evidence",
            4,
        );
        checkpoint(
            &mut store,
            &work,
            &claim,
            "runner",
            "completion-before-verification",
            5,
            std::slice::from_ref(&generic_evidence),
        );
        let refused = complete(
            &mut store,
            &work,
            &claim,
            "runner",
            &generic_evidence,
            "completion-open-obligation",
            6,
        );
        let Err(StoreError::OpenWorkObligations {
            work: refused_work,
            obligations,
            omitted_count,
        }) = refused
        else {
            panic!("completion must return the typed open-obligation refusal");
        };
        assert_eq!(refused_work, work.work_id);
        assert_eq!(omitted_count, 0);
        assert_eq!(obligations.len(), 1);
        assert_eq!(
            obligations[0].obligation_id,
            opened[0].obligation.obligation_id
        );
        assert_eq!(obligations[0].definition, opened[0].definition_hash);
        assert_eq!(obligations[0].required_check, VerificationKind::Test);

        let verification_observation = ExecutionObservation {
            schema_version: SCHEMA_VERSION,
            project_id: work.project_id.clone(),
            binding,
            session_id: SessionId("runner".into()),
            grant_id: "completion-obligation-grant".into(),
            observation_id: "completion-verification".into(),
            action_fingerprint: ObjectHash::from_canonical_bytes(b"cargo test --workspace"),
            effect: EffectClass::Observe,
            outcome: ExecutionOutcome::Succeeded,
            source_changed: false,
            obligation_rule_set: builtin_rule_set_hash(),
            source_basis: Some(source_basis.clone()),
            observed_at: Some(at(7)),
            actor: run_actor.clone(),
            recorded_at: at(7),
        };
        let environment_components = EnvironmentComponents {
            toolchain: "rustc-1.89.0".into(),
            sandbox: Some("completion-sandbox-v1".into()),
            workspace_id: source_basis.workspace_id.clone(),
            capability_map_revision: 1,
        };
        let environment_fingerprint = CanonicalObject::freeze(&environment_components)
            .expect("freeze completion environment components")
            .hash()
            .clone();
        let (verification_hash, environment_hash) = {
            let transaction = store
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .expect("verification transaction");
            let producer =
                append_control_execution_observation_on(&transaction, &verification_observation)
                    .expect("append verification producer");
            let environment_hash = append_control_environment_evidence_on(
                &transaction,
                &EnvironmentEvidence {
                    schema_version: SCHEMA_VERSION,
                    project_id: work.project_id.clone(),
                    binding: verification_observation.binding.clone(),
                    session_id: SessionId("runner".into()),
                    source_basis: source_basis.clone(),
                    environment_fingerprint,
                    components: Some(environment_components.clone()),
                    observed_at: at(7),
                    actor: run_actor.clone(),
                    recorded_at: at(7),
                },
            )
            .expect("append completion environment evidence");
            let hash = append_control_verification_evidence_on(
                &transaction,
                &VerificationEvidence {
                    schema_version: SCHEMA_VERSION,
                    project_id: work.project_id.clone(),
                    binding: verification_observation.binding.clone(),
                    session_id: SessionId("runner".into()),
                    producer_observation: producer,
                    source_basis,
                    environment: Some(environment_hash.clone()),
                    check_kind: VerificationKind::Test,
                    check_fingerprint: verification_observation.action_fingerprint.clone(),
                    result: VerificationResult::Passed,
                    completed_at: at(7),
                    summary: "host observed tests on the latest source basis".into(),
                    refs: vec!["command:cargo-test-workspace".into()],
                    actor: run_actor,
                    recorded_at: at(7),
                },
            )
            .expect("append matching verification evidence");
            transaction.commit().expect("commit verification");
            (hash, environment_hash)
        };
        let terminal = store
            .work_run_obligations(run.run_id)
            .expect("terminal obligation");
        assert_eq!(terminal.len(), 1);
        assert_eq!(terminal[0].state, WorkObligationState::Satisfied);
        assert!(matches!(
            terminal[0]
                .resolution
                .as_ref()
                .map(|resolution| &resolution.resolution),
            Some(WorkObligationResolution::Satisfied { evidence, .. })
                if evidence == &verification_hash
        ));

        let all_evidence = store
            .work_run_evidence(run.run_id)
            .expect("all completion evidence");
        assert_eq!(all_evidence.len(), 3);
        assert!(all_evidence.contains(&generic_evidence));
        assert!(all_evidence.contains(&verification_hash));
        assert!(all_evidence.contains(&environment_hash));
        checkpoint(
            &mut store,
            &work,
            &claim,
            "runner",
            "completion-after-verification",
            8,
            &all_evidence,
        );
        let seal = complete(
            &mut store,
            &work,
            &claim,
            "runner",
            &generic_evidence,
            "completion-after-obligation",
            9,
        )
        .expect("complete after terminal obligation and acknowledging checkpoint");
        assert_eq!(seal.schema_version, crate::schema::SCHEMA_VERSION);
        assert_eq!(
            seal.obligation_schema_version,
            crate::schema::COMPLETION_OBLIGATION_SCHEMA_VERSION
        );
        assert_eq!(
            seal.obligations,
            vec![CompletionObligationBinding {
                obligation_id: terminal[0].obligation.obligation_id,
                definition: terminal[0].definition_hash.clone(),
                resolution: terminal[0]
                    .resolution_hash
                    .clone()
                    .expect("terminal resolution hash"),
            }]
        );
        assert_eq!(
            seal.environment_schema_version,
            crate::schema::COMPLETION_ENVIRONMENT_SCHEMA_VERSION
        );
        assert_eq!(seal.environment, vec![environment_hash.clone()]);
        validate_completion_seal_environment_basis_on(&store.connection, &seal)
            .expect("reconstruct exact completion environment basis");
        let mut forged_environment_basis = seal.clone();
        forged_environment_basis.environment.clear();
        assert!(
            validate_completion_seal_environment_basis_on(
                &store.connection,
                &forged_environment_basis,
            )
            .is_err(),
            "completion accepted a seal that omitted environment evidence"
        );
        validate_completion_seal_obligation_basis_on(&store.connection, &seal)
            .expect("reconstruct exact completion basis");
        let report = store.verify_all().expect("integrity report");
        assert!(report.is_healthy(), "{report:?}");
        let seal_hash = CanonicalObject::freeze(&seal)
            .expect("freeze sealed obligation basis")
            .hash()
            .clone();
        let mut forged_seal = seal.clone();
        forged_seal.obligations.clear();
        store
            .connection
            .execute_batch("SAVEPOINT corrupt_completion_obligations")
            .expect("start completion-seal corruption fixture");
        store
            .connection
            .execute(
                "UPDATE work_completion_seals SET seal_json = ?2 WHERE seal_hash = ?1",
                params![
                    seal_hash.as_str(),
                    serde_json::to_vec(&forged_seal).expect("forged seal JSON")
                ],
            )
            .expect("corrupt completion obligation projection");
        let corrupt_report = store.verify_all().expect("corrupt integrity report");
        assert!(
            corrupt_report
                .invalid_work_records
                .iter()
                .any(|record| record.contains("completion_seal")),
            "{corrupt_report:?}"
        );
        store
            .connection
            .execute_batch(
                "ROLLBACK TO corrupt_completion_obligations; RELEASE corrupt_completion_obligations",
            )
            .expect("restore completion-seal projection");
        assert!(
            store
                .verify_all()
                .expect("restored integrity report")
                .is_healthy()
        );
    }

    #[test]
    fn completion_refuses_more_than_the_bounded_environment_basis() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "project-bounded-environment", "planner");
        let work = store
            .create_work(
                &root_request(
                    "project-bounded-environment",
                    "create-bounded-environment-work",
                    1,
                ),
                &DevelopmentNoopRedactor,
            )
            .expect("create local work");
        let claim = claim(
            &mut store,
            &work,
            "runner",
            "claim-bounded-environment-work",
            2,
            300,
        );
        let run = load_work_run(&store.connection, claim.run_id).expect("claimed run");
        let binding = ControlWorkBinding {
            root_execution_id: run.root_execution_id,
            work_id: work.work_id,
            run_id: run.run_id,
            work_revision: claim.accepted_work_revision,
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
        };
        let mut run_actor = actor("runner");
        run_actor.run_id = Some(run.run_id.0.to_string());
        let environment_hashes = {
            let transaction = store
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .expect("environment transaction");
            let mut hashes = Vec::new();
            for index in 0..=MAX_COMPLETION_ENVIRONMENT_EVIDENCE {
                let components = EnvironmentComponents {
                    toolchain: format!("toolchain-{index}"),
                    sandbox: Some("bounded-environment-sandbox".into()),
                    workspace_id: "workspace-bounded-environment".into(),
                    capability_map_revision: 1,
                };
                let environment_fingerprint = CanonicalObject::freeze(&components)
                    .expect("freeze bounded environment components")
                    .hash()
                    .clone();
                hashes.push(
                    append_control_environment_evidence_on(
                        &transaction,
                        &EnvironmentEvidence {
                            schema_version: SCHEMA_VERSION,
                            project_id: work.project_id.clone(),
                            binding: binding.clone(),
                            session_id: SessionId("runner".into()),
                            source_basis: ExecutionSourceBasis {
                                workspace_id: components.workspace_id.clone(),
                                source_revision: "bounded-environment-revision".into(),
                            },
                            environment_fingerprint,
                            components: Some(components),
                            observed_at: at(3),
                            actor: run_actor.clone(),
                            recorded_at: at(3),
                        },
                    )
                    .expect("append bounded environment evidence"),
                );
            }
            transaction.commit().expect("commit environment evidence");
            hashes
        };
        assert_eq!(
            environment_hashes.len(),
            MAX_COMPLETION_ENVIRONMENT_EVIDENCE + 1
        );
        let generic_evidence = evidence(
            &mut store,
            &work,
            &claim,
            "runner",
            "bounded-environment-generic-evidence",
            4,
        );
        let mut acknowledged = environment_hashes;
        acknowledged.push(generic_evidence.clone());
        checkpoint(
            &mut store,
            &work,
            &claim,
            "runner",
            "bounded-environment-checkpoint",
            5,
            &acknowledged,
        );
        let result = complete(
            &mut store,
            &work,
            &claim,
            "runner",
            &generic_evidence,
            "bounded-environment-completion",
            6,
        );
        assert!(
            matches!(
                result,
                Err(StoreError::WorkCompletionRefused { ref reason, .. })
                    if reason.contains("maximum 64")
            ),
            "completion did not refuse an oversized environment basis: {result:?}"
        );
    }

    #[test]
    fn open_completion_obligation_refusal_is_bounded_and_counts_omissions() {
        let mut store = SqliteStore::open_in_memory().expect("store");
        install_grant(&mut store, "project-bounded-obligations", "planner");
        let work = store
            .create_work(
                &root_request(
                    "project-bounded-obligations",
                    "create-bounded-obligation-work",
                    1,
                ),
                &DevelopmentNoopRedactor,
            )
            .expect("create local work");
        let claim = claim(
            &mut store,
            &work,
            "runner",
            "claim-bounded-obligation-work",
            2,
            300,
        );
        let run = load_work_run(&store.connection, claim.run_id).expect("claimed run");
        let binding = ControlWorkBinding {
            root_execution_id: run.root_execution_id,
            work_id: work.work_id,
            run_id: run.run_id,
            work_revision: claim.accepted_work_revision,
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
        };
        let mut run_actor = actor("runner");
        run_actor.run_id = Some(run.run_id.0.to_string());
        let transaction = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("bounded-obligation transaction");
        for index in 0..=MAX_OPEN_COMPLETION_OBLIGATIONS {
            append_control_execution_observation_on(
                &transaction,
                &ExecutionObservation {
                    schema_version: SCHEMA_VERSION,
                    project_id: work.project_id.clone(),
                    binding: binding.clone(),
                    session_id: SessionId("runner".into()),
                    grant_id: "bounded-obligation-grant".into(),
                    observation_id: format!("bounded-source-mutation-{index}"),
                    action_fingerprint: ObjectHash::from_canonical_bytes(
                        format!("write source {index}").as_bytes(),
                    ),
                    effect: EffectClass::MutateLocal,
                    outcome: ExecutionOutcome::Succeeded,
                    source_changed: true,
                    obligation_rule_set: builtin_rule_set_hash(),
                    source_basis: Some(ExecutionSourceBasis {
                        workspace_id: "workspace-bounded".into(),
                        source_revision: format!("revision-{index}"),
                    }),
                    observed_at: Some(at(3)),
                    actor: run_actor.clone(),
                    recorded_at: at(3),
                },
            )
            .expect("append bounded mutation obligation");
        }
        transaction.commit().expect("commit bounded obligations");
        let cut = FeedPosition {
            feed: FeedId::RunExecution(run.run_id),
            position: feed_head(&store.connection, &FeedId::RunExecution(run.run_id))
                .expect("run head"),
        };
        let Err(StoreError::OpenWorkObligations {
            work: refused_work,
            obligations,
            omitted_count,
        }) = completion_obligation_basis_on(&store.connection, work.work_id, run.run_id, &cut)
        else {
            panic!("the exact cut must refuse its open obligations");
        };
        assert_eq!(refused_work, work.work_id);
        assert_eq!(obligations.len(), MAX_OPEN_COMPLETION_OBLIGATIONS);
        assert_eq!(omitted_count, 1);
        assert!(
            obligations
                .iter()
                .all(|obligation| obligation.required_check == VerificationKind::Test)
        );
        assert!(obligations.windows(2).all(|window| {
            window[0].obligation_id.0.as_bytes() < window[1].obligation_id.0.as_bytes()
        }));
    }

    #[test]
    fn ambient_completion_returns_and_replays_a_typed_open_obligation_result() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("project-protocol-obligations".into());
        let session = SessionId("runner".into());
        let (grant, work, evidence_hash, expected_obligation) = {
            let mut store = SqliteStore::open(&database).expect("store");
            install_grant(&mut store, &project.0, "planner");
            let grant = install_grant(&mut store, &project.0, &session.0);
            let work = store
                .create_work(
                    &root_request(&project.0, "create-protocol-obligation-work", 1),
                    &DevelopmentNoopRedactor,
                )
                .expect("create local work");
            store
                .focus_work_session(&project, &session, work.work_id, at(2))
                .expect("focus work session");
            let claim = claim(
                &mut store,
                &work,
                &session.0,
                "claim-protocol-obligation-work",
                2,
                300,
            );
            let run = load_work_run(&store.connection, claim.run_id).expect("claimed run");
            let binding = ControlWorkBinding {
                root_execution_id: run.root_execution_id,
                work_id: work.work_id,
                run_id: run.run_id,
                work_revision: claim.accepted_work_revision,
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
            };
            let mut run_actor = actor(&session.0);
            run_actor.run_id = Some(run.run_id.0.to_string());
            let transaction = store
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .expect("protocol-obligation transaction");
            append_control_execution_observation_on(
                &transaction,
                &ExecutionObservation {
                    schema_version: SCHEMA_VERSION,
                    project_id: project.clone(),
                    binding,
                    session_id: session.clone(),
                    grant_id: "protocol-obligation-grant".into(),
                    observation_id: "protocol-source-mutation".into(),
                    action_fingerprint: ObjectHash::from_canonical_bytes(b"write protocol source"),
                    effect: EffectClass::MutateLocal,
                    outcome: ExecutionOutcome::Succeeded,
                    source_changed: true,
                    obligation_rule_set: builtin_rule_set_hash(),
                    source_basis: Some(ExecutionSourceBasis {
                        workspace_id: "workspace-protocol".into(),
                        source_revision: "revision-protocol".into(),
                    }),
                    observed_at: Some(at(3)),
                    actor: run_actor,
                    recorded_at: at(3),
                },
            )
            .expect("append protocol mutation obligation");
            transaction.commit().expect("commit protocol obligation");
            let obligation = store
                .work_run_obligations(run.run_id)
                .expect("protocol obligation")
                .pop()
                .expect("one protocol obligation");
            let evidence_hash = evidence(
                &mut store,
                &work,
                &claim,
                &session.0,
                "protocol-completion-evidence",
                4,
            );
            checkpoint(
                &mut store,
                &work,
                &claim,
                &session.0,
                "protocol-completion-checkpoint",
                5,
                std::slice::from_ref(&evidence_hash),
            );
            (grant, work, evidence_hash, obligation)
        };
        let service = LocalWorkService::new(
            database.clone(),
            project,
            "runner".into(),
            session,
            Some("obligation-protocol-test".into()),
            Some(grant),
        );
        let input = WorkCompleteInput {
            capture: None,
            evidence: vec![evidence_hash.to_string()],
            acceptance: Some(vec![WorkAcceptanceInput {
                criterion: None,
                satisfied: true,
                evidence: vec![evidence_hash.to_string()],
                note: "completion evidence is present".into(),
            }]),
            note: None,
            idempotency_key: "typed-open-obligation-result".into(),
        };
        let first = service
            .work_complete(input.clone(), at(6))
            .expect("open obligation is a typed result");
        let WorkCompleteResult::Refused(refusal) = &first else {
            panic!("open obligation must not complete the work");
        };
        assert_eq!(refusal.code, "open_work_obligations");
        assert_eq!(refusal.work_id, work.work_id);
        assert_eq!(refusal.obligation_page.items.len(), 1);
        assert_eq!(
            refusal.obligation_page.items[0].obligation_id,
            expected_obligation.obligation.obligation_id
        );
        assert_eq!(
            refusal.obligation_page.items[0].definition,
            expected_obligation.definition_hash
        );
        assert_eq!(
            refusal.obligation_page.items[0].requirement.check_kind,
            VerificationKind::Test
        );
        assert_eq!(refusal.obligation_page.omitted_count, 0);
        let recovery = &refusal.recovery;
        assert!(matches!(
            &recovery.cause,
            WorkCompletionRecoveryCause::OpenObligation {
                obligation_id,
                definition,
                required_check: VerificationKind::Test,
            } if *obligation_id == expected_obligation.obligation.obligation_id
                && *definition == expected_obligation.definition_hash
        ));
        assert_eq!(recovery.item.work_id, work.work_id);
        assert_eq!(recovery.item.title, work.title);
        assert!(
            recovery
                .command
                .starts_with(&format!("engram work done {}", work.short_ref))
        );
        assert_eq!(
            refusal.remedy,
            "record the matching host verification, then checkpoint_work acknowledging it, then complete; or request a host/operator waiver"
        );
        let replay = service
            .work_complete(input, at(7))
            .expect("typed refusal replays from the durable protocol attempt");
        assert_eq!(
            serde_json::to_value(replay).expect("replay JSON"),
            serde_json::to_value(first).expect("first JSON")
        );
        let stored = SqliteStore::open(&database).expect("inspect store");
        assert_eq!(
            stored
                .get_work_item(work.work_id)
                .expect("refused work remains readable")
                .lifecycle,
            WorkLifecycle::Open
        );
        assert_eq!(
            stored
                .current_work_claim(work.work_id)
                .expect("refused claim lookup")
                .expect("refused claim remains live")
                .state,
            WorkClaimState::Active
        );
        let report = stored.verify_all().expect("typed refusal integrity report");
        assert!(report.is_healthy(), "{report:?}");
    }

    #[test]
    fn basisless_mutation_is_waiver_only_until_a_later_verified_source_state() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = directory.path().join("engram.sqlite3");
        let mut store = SqliteStore::open(&database).expect("store");
        install_grant(&mut store, "project-obligations", "planner");
        let waiver_grant = install_grant(&mut store, "project-obligations", "operator");
        let work = store
            .create_work(
                &root_request("project-obligations", "create-obligation-work", 1),
                &DevelopmentNoopRedactor,
            )
            .expect("create local work");
        let claim = claim(&mut store, &work, "runner", "claim-obligation-work", 2, 120);
        let run = load_work_run(&store.connection, claim.run_id).expect("claimed run");
        let binding = ControlWorkBinding {
            root_execution_id: run.root_execution_id,
            work_id: work.work_id,
            run_id: run.run_id,
            work_revision: claim.accepted_work_revision,
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
        };
        let mut run_actor = actor("runner");
        run_actor.run_id = Some(run.run_id.0.to_string());
        let observation = |id: &str,
                           source_changed: bool,
                           basis: Option<ExecutionSourceBasis>,
                           action: &str,
                           at_time: DateTime<Utc>| ExecutionObservation {
            schema_version: SCHEMA_VERSION,
            project_id: work.project_id.clone(),
            binding: binding.clone(),
            session_id: SessionId("runner".into()),
            grant_id: "direct-test-grant".into(),
            observation_id: id.into(),
            action_fingerprint: ObjectHash::from_canonical_bytes(action.as_bytes()),
            effect: if source_changed {
                EffectClass::MutateLocal
            } else {
                EffectClass::Observe
            },
            outcome: ExecutionOutcome::Succeeded,
            source_changed,
            obligation_rule_set: builtin_rule_set_hash(),
            source_basis: basis,
            observed_at: Some(at_time),
            actor: run_actor.clone(),
            recorded_at: at_time,
        };

        let basisless = observation(
            "basisless-mutation",
            true,
            None,
            "write without basis",
            at(3),
        );
        let basisless_hash = {
            let transaction = store
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .expect("basisless transaction");
            let hash = append_control_execution_observation_on(&transaction, &basisless)
                .expect("append basisless mutation");
            transaction.commit().expect("commit basisless mutation");
            hash
        };
        let opened = store
            .work_run_obligations(run.run_id)
            .expect("basisless open obligation");
        assert_eq!(opened.len(), 1);
        assert_eq!(opened[0].state, WorkObligationState::Open);
        assert_eq!(opened[0].obligation.triggering_observation, basisless_hash);

        let first_test = observation(
            "test-after-basisless",
            false,
            Some(ExecutionSourceBasis {
                workspace_id: "workspace-a".into(),
                source_revision: "revision-a".into(),
            }),
            "cargo test",
            at(4),
        );
        {
            let transaction = store
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .expect("first test transaction");
            let producer = append_control_execution_observation_on(&transaction, &first_test)
                .expect("append first test producer");
            append_control_verification_evidence_on(
                &transaction,
                &VerificationEvidence {
                    schema_version: SCHEMA_VERSION,
                    project_id: work.project_id.clone(),
                    binding: binding.clone(),
                    session_id: SessionId("runner".into()),
                    producer_observation: producer,
                    source_basis: first_test.source_basis.clone().expect("first test basis"),
                    environment: None,
                    check_kind: VerificationKind::Test,
                    check_fingerprint: first_test.action_fingerprint.clone(),
                    result: VerificationResult::Passed,
                    completed_at: at(4),
                    summary: "tests passed after a basisless mutation".into(),
                    refs: Vec::new(),
                    actor: run_actor.clone(),
                    recorded_at: at(4),
                },
            )
            .expect("append first test evidence");
            transaction.commit().expect("commit first test");
        }
        assert_eq!(
            store
                .work_run_obligations(run.run_id)
                .expect("still-open obligation")[0]
                .state,
            WorkObligationState::Open
        );

        let based_mutation = observation(
            "based-mutation",
            true,
            Some(ExecutionSourceBasis {
                workspace_id: "workspace-b".into(),
                source_revision: "revision-b".into(),
            }),
            "write with basis",
            at(5),
        );
        let final_test = observation(
            "test-after-based-mutation",
            false,
            based_mutation.source_basis.clone(),
            "cargo test --workspace",
            at(6),
        );
        {
            let transaction = store
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .expect("verified mutation transaction");
            append_control_execution_observation_on(&transaction, &based_mutation)
                .expect("append based mutation");
            let producer = append_control_execution_observation_on(&transaction, &final_test)
                .expect("append final test producer");
            append_control_verification_evidence_on(
                &transaction,
                &VerificationEvidence {
                    schema_version: SCHEMA_VERSION,
                    project_id: work.project_id.clone(),
                    binding: binding.clone(),
                    session_id: SessionId("runner".into()),
                    producer_observation: producer,
                    source_basis: final_test.source_basis.clone().expect("final test basis"),
                    environment: None,
                    check_kind: VerificationKind::Test,
                    check_fingerprint: final_test.action_fingerprint.clone(),
                    result: VerificationResult::Passed,
                    completed_at: at(6),
                    summary: "tests passed on the latest full source state".into(),
                    refs: Vec::new(),
                    actor: run_actor.clone(),
                    recorded_at: at(6),
                },
            )
            .expect("append final test evidence");
            transaction.commit().expect("commit verified mutation");
        }
        let satisfied = store
            .work_run_obligations(run.run_id)
            .expect("satisfied obligations");
        assert_eq!(satisfied.len(), 2);
        assert!(
            satisfied
                .iter()
                .all(|record| record.state == WorkObligationState::Satisfied)
        );
        let evaluated_cut = satisfied
            .iter()
            .find_map(|record| match &record.resolution.as_ref()?.resolution {
                WorkObligationResolution::Satisfied { evaluated_cut, .. } => {
                    Some(evaluated_cut.clone())
                }
                WorkObligationResolution::Waived { .. } => None,
            })
            .expect("satisfaction evaluated cut");
        assert_eq!(
            store
                .open_work_obligations_at_cut(run.run_id, &evaluated_cut)
                .expect("derive obligations before terminal appends")
                .len(),
            2
        );

        let waiver_target = &satisfied[0];
        assert!(matches!(
            store.waive_work_obligation(
                &WaiveWorkObligationRequest {
                    obligation_id: waiver_target.obligation.obligation_id,
                    expected_definition: waiver_target.definition_hash.clone(),
                    waived_by: "operator".into(),
                    reason: "already terminal must not be waived".into(),
                    authority: LifecycleAuthorityDecision {
                        grant: waiver_grant.clone(),
                    },
                    actor: actor("operator"),
                    idempotency_key: "waive-terminal-obligation".into(),
                    waived_at: at(7),
                },
                &DevelopmentNoopRedactor,
            ),
            Err(StoreError::InvalidWork(message)) if message.contains("already terminal")
        ));
        let waiver_mutation = observation(
            "waiver-only-mutation",
            true,
            None,
            "write requiring operator waiver",
            at(7),
        );
        {
            let transaction = store
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .expect("waiver mutation transaction");
            append_control_execution_observation_on(&transaction, &waiver_mutation)
                .expect("append waiver mutation");
            transaction.commit().expect("commit waiver mutation");
        }
        let waiver_target = store
            .work_run_obligations(run.run_id)
            .expect("open waiver target")
            .into_iter()
            .find(|record| record.state == WorkObligationState::Open)
            .expect("one open waiver target");
        let waiver_request = WaiveWorkObligationRequest {
            obligation_id: waiver_target.obligation.obligation_id,
            expected_definition: waiver_target.definition_hash.clone(),
            waived_by: "operator".into(),
            reason: "operator accepted the unverified final mutation".into(),
            authority: LifecycleAuthorityDecision {
                grant: waiver_grant,
            },
            actor: actor("operator"),
            idempotency_key: "waive-open-obligation".into(),
            waived_at: at(8),
        };
        let waived = store
            .waive_work_obligation(&waiver_request, &DevelopmentNoopRedactor)
            .expect("waive exact open obligation");
        assert!(matches!(
            waived.resolution,
            WorkObligationResolution::Waived { ref reason, .. }
                if reason == "operator accepted the unverified final mutation"
        ));
        let mut replay_request = waiver_request.clone();
        replay_request.waived_at = at(9);
        assert_eq!(
            store
                .waive_work_obligation(&replay_request, &DevelopmentNoopRedactor)
                .expect("replay obligation waiver after an uncertain response"),
            waived
        );
        let terminal = store
            .work_run_obligations(run.run_id)
            .expect("terminal obligations");
        assert_eq!(terminal.len(), 3);
        assert_eq!(
            terminal
                .iter()
                .find(|record| record.obligation.obligation_id == waiver_request.obligation_id)
                .expect("waived projection")
                .state,
            WorkObligationState::Waived
        );
        let terminal_cut =
            current_run_feed_cut_on(&store.connection, run.run_id).expect("terminal run-feed cut");
        assert!(
            store
                .open_work_obligations_at_cut(run.run_id, &terminal_cut)
                .expect("derive terminal obligation state")
                .is_empty()
        );
        let report = store.verify_all().expect("obligation integrity report");
        assert!(report.is_healthy(), "{report:?}");
        let target = terminal
            .iter()
            .find(|record| record.obligation.obligation_id == waiver_request.obligation_id)
            .expect("waived corruption target");
        let obligation_id = target.obligation.obligation_id.0.to_string();
        let definition = target.definition_hash.as_str();
        let resolution = target
            .resolution_hash
            .as_ref()
            .expect("waiver resolution")
            .as_str();
        let forged_uuid = uuid::Uuid::new_v4().to_string();
        let corruptions = [
            format!(
                "UPDATE work_run_obligations SET obligation_id = '{forged_uuid}' WHERE obligation_id = '{obligation_id}'"
            ),
            format!(
                "UPDATE work_run_obligations SET definition_hash = '{resolution}' WHERE obligation_id = '{obligation_id}'"
            ),
            format!(
                "UPDATE work_run_obligations SET project_id = 'forged-project' WHERE obligation_id = '{obligation_id}'"
            ),
            format!(
                "UPDATE work_run_obligations SET root_execution_id = '{forged_uuid}' WHERE obligation_id = '{obligation_id}'"
            ),
            format!(
                "UPDATE work_run_obligations SET root_id = '{forged_uuid}' WHERE obligation_id = '{obligation_id}'"
            ),
            format!(
                "UPDATE work_run_obligations SET work_id = '{forged_uuid}' WHERE obligation_id = '{obligation_id}'"
            ),
            format!(
                "UPDATE work_run_obligations SET run_id = '{forged_uuid}' WHERE obligation_id = '{obligation_id}'"
            ),
            format!(
                "UPDATE work_run_obligations SET work_revision = work_revision + 1 WHERE obligation_id = '{obligation_id}'"
            ),
            format!(
                "UPDATE work_run_obligations SET rule_id = 'forged-rule' WHERE obligation_id = '{obligation_id}'"
            ),
            format!(
                "UPDATE work_run_obligations SET rule_version = rule_version + 1 WHERE obligation_id = '{obligation_id}'"
            ),
            format!(
                "UPDATE work_run_obligations SET triggering_observation_hash = '{definition}' WHERE obligation_id = '{obligation_id}'"
            ),
            format!(
                "UPDATE work_run_obligations SET trigger_position = trigger_position + 1 WHERE obligation_id = '{obligation_id}'"
            ),
            format!(
                "UPDATE work_run_obligations SET check_kind = 'build' WHERE obligation_id = '{obligation_id}'"
            ),
            format!(
                "UPDATE work_run_obligations SET check_fingerprint = '{definition}' WHERE obligation_id = '{obligation_id}'"
            ),
            format!(
                "UPDATE work_run_obligations SET state = 'satisfied' WHERE obligation_id = '{obligation_id}'"
            ),
            format!(
                "UPDATE work_run_obligations SET resolution_hash = '{definition}' WHERE obligation_id = '{obligation_id}'"
            ),
            format!(
                "UPDATE work_run_obligations SET resolution_kind = 'satisfied' WHERE obligation_id = '{obligation_id}'"
            ),
            format!(
                "UPDATE work_run_obligations SET evidence_hash = '{definition}' WHERE obligation_id = '{obligation_id}'"
            ),
            format!(
                "UPDATE work_run_obligations SET opened_at_ms = opened_at_ms + 1 WHERE obligation_id = '{obligation_id}'"
            ),
            format!(
                "UPDATE work_run_obligations SET resolved_at_ms = resolved_at_ms + 1 WHERE obligation_id = '{obligation_id}'"
            ),
        ];
        store
            .connection
            .execute_batch("PRAGMA foreign_keys = OFF")
            .expect("disable foreign keys for corruption fixtures");
        for (index, update) in corruptions.iter().enumerate() {
            store
                .connection
                .execute_batch("SAVEPOINT corrupt_obligation")
                .expect("start obligation corruption savepoint");
            store
                .connection
                .execute(update, [])
                .unwrap_or_else(|error| panic!("apply obligation corruption {index}: {error}"));
            assert!(
                store.work_run_obligations(run.run_id).is_err(),
                "obligation corruption {index} was accepted by lifecycle reads"
            );
            let corrupt_report = store
                .verify_all()
                .unwrap_or_else(|error| panic!("verify obligation corruption {index}: {error}"));
            assert!(
                !corrupt_report.invalid_work_records.is_empty(),
                "obligation corruption {index} was not reported: {corrupt_report:?}"
            );
            store
                .connection
                .execute_batch("ROLLBACK TO corrupt_obligation; RELEASE corrupt_obligation")
                .expect("restore obligation projection");
        }
        store
            .connection
            .execute_batch("PRAGMA foreign_keys = ON")
            .expect("restore foreign key enforcement");
        let final_report = store
            .verify_all()
            .expect("final obligation integrity report");
        assert!(final_report.is_healthy(), "{final_report:?}");
    }

    #[test]
    fn bound_host_obligation_waiver_is_typed_human_attributed_and_replayable() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = directory.path().join("engram.sqlite3");
        let mut store = SqliteStore::open(&database).expect("store");
        let planner_grant = install_grant(&mut store, "project-host-waiver", "planner");
        let waiver_grant = install_grant(&mut store, "project-host-waiver", "human-operator");
        let work = store
            .create_work(
                &root_request("project-host-waiver", "create-host-waiver-work", 1),
                &DevelopmentNoopRedactor,
            )
            .expect("create local work");
        let claim = claim(
            &mut store,
            &work,
            "runner",
            "claim-host-waiver-work",
            2,
            120,
        );
        let run = load_work_run(&store.connection, claim.run_id).expect("claimed run");
        let binding = ControlWorkBinding {
            root_execution_id: run.root_execution_id,
            work_id: work.work_id,
            run_id: run.run_id,
            work_revision: claim.accepted_work_revision,
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
        };
        let mut host_actor = actor("runner");
        host_actor.run_id = Some(run.run_id.0.to_string());
        let observation = ExecutionObservation {
            schema_version: SCHEMA_VERSION,
            project_id: work.project_id.clone(),
            binding: binding.clone(),
            session_id: SessionId("runner".into()),
            grant_id: "host-waiver-test-turn".into(),
            observation_id: "host-waiver-mutation".into(),
            action_fingerprint: ObjectHash::from_canonical_bytes(b"host waiver mutation"),
            effect: EffectClass::MutateLocal,
            outcome: ExecutionOutcome::Succeeded,
            source_changed: true,
            obligation_rule_set: builtin_rule_set_hash(),
            source_basis: None,
            observed_at: Some(at(3)),
            actor: host_actor.clone(),
            recorded_at: at(3),
        };
        {
            let transaction = store
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .expect("observation transaction");
            append_control_execution_observation_on(&transaction, &observation)
                .expect("append mutation and obligation");
            transaction.commit().expect("commit mutation");
        }
        let open = store
            .work_run_obligations(run.run_id)
            .expect("open obligation")
            .pop()
            .expect("one obligation");
        let unbound_session = SessionId("unbound-runner".into());
        let unbound_connection = store
            .resume_control_connection(&unbound_session, at(4))
            .expect("resume unbound host connection");
        let unbound_actor = actor("unbound-runner");
        let unbound = store
            .bind_control_session(
                &work.project_id,
                "local-work:unbound-host-waiver",
                "Unbound obligation waiver attempt",
                &unbound_session,
                &unbound_connection,
                &unbound_actor,
                ControlAssurance::TurnGated,
                &[EffectClass::Observe],
                1,
                "bind-unbound-host-waiver",
                at(4),
            )
            .expect("bind unbound host session");
        let unbound_refusal = store
            .waive_bound_work_obligation(
                &work.project_id,
                &unbound_session,
                &unbound_connection,
                &unbound.routing_token,
                open.obligation.obligation_id,
                &open.definition_hash,
                &waiver_grant,
                "human-operator",
                "reviewed the exact obligation",
                &unbound_actor,
                "unbound-host-waiver",
                at(5),
                &DevelopmentNoopRedactor,
            )
            .expect("unbound refusal is typed");
        assert!(matches!(
            unbound_refusal,
            WorkObligationWaiverDecision::Refused {
                code: WorkObligationWaiverRefusalCode::WaiverNotAdmitted,
                ..
            }
        ));
        let session_id = SessionId("runner".into());
        let connection_token = store
            .resume_control_connection(&session_id, at(4))
            .expect("resume host connection");
        let bound = store
            .bind_control_session_with_work(
                &work.project_id,
                "local-work:host-waiver",
                "Host-authorized obligation waiver",
                &session_id,
                &connection_token,
                &host_actor,
                Some(&binding),
                ControlAssurance::TurnGated,
                &[EffectClass::Observe, EffectClass::MutateLocal],
                1,
                "bind-host-waiver",
                at(4),
            )
            .expect("bind host session");
        let wrong_definition = store
            .waive_bound_work_obligation(
                &work.project_id,
                &session_id,
                &connection_token,
                &bound.routing_token,
                open.obligation.obligation_id,
                &ObjectHash::from_canonical_bytes(b"wrong definition"),
                &waiver_grant,
                "human-operator",
                "reviewed the exact obligation",
                &host_actor,
                "host-waiver-wrong-definition",
                at(5),
                &DevelopmentNoopRedactor,
            )
            .expect("definition mismatch is typed");
        assert!(matches!(
            wrong_definition,
            WorkObligationWaiverDecision::Refused {
                code: WorkObligationWaiverRefusalCode::DefinitionChanged,
                ..
            }
        ));
        let not_admitted = store
            .waive_bound_work_obligation(
                &work.project_id,
                &session_id,
                &connection_token,
                &bound.routing_token,
                open.obligation.obligation_id,
                &open.definition_hash,
                &planner_grant,
                "human-operator",
                "reviewed the exact obligation",
                &host_actor,
                "host-waiver-not-admitted",
                at(5),
                &DevelopmentNoopRedactor,
            )
            .expect("authority refusal is typed");
        assert!(matches!(
            not_admitted,
            WorkObligationWaiverDecision::Refused {
                code: WorkObligationWaiverRefusalCode::WaiverNotAdmitted,
                ..
            }
        ));
        let waived = store
            .waive_bound_work_obligation(
                &work.project_id,
                &session_id,
                &connection_token,
                &bound.routing_token,
                open.obligation.obligation_id,
                &open.definition_hash,
                &waiver_grant,
                "human-operator",
                "reviewed the exact obligation",
                &host_actor,
                "host-waiver-success",
                at(6),
                &DevelopmentNoopRedactor,
            )
            .expect("waive obligation");
        let replay = store
            .waive_bound_work_obligation(
                &work.project_id,
                &session_id,
                &connection_token,
                &bound.routing_token,
                open.obligation.obligation_id,
                &open.definition_hash,
                &waiver_grant,
                "human-operator",
                "reviewed the exact obligation",
                &host_actor,
                "host-waiver-success",
                at(7),
                &DevelopmentNoopRedactor,
            )
            .expect("replay host waiver");
        assert_eq!(replay, waived);
        let WorkObligationWaiverDecision::Waived { receipt } = waived else {
            panic!("expected waived receipt");
        };
        assert_eq!(receipt.waived_by, "human-operator");
        let resolved = store
            .work_run_obligations(run.run_id)
            .expect("resolved obligation")
            .pop()
            .expect("one obligation");
        let event = resolved.resolution.expect("waiver resolution");
        assert_eq!(event.actor.actor_id, "runner");
        assert_eq!(event.actor.session_id, Some(session_id.clone()));
        assert!(matches!(
            event.resolution,
            WorkObligationResolution::Waived { waived_by, .. }
                if waived_by == "human-operator"
        ));
        let already_terminal = store
            .waive_bound_work_obligation(
                &work.project_id,
                &session_id,
                &connection_token,
                &bound.routing_token,
                open.obligation.obligation_id,
                &open.definition_hash,
                &waiver_grant,
                "human-operator",
                "reviewed the exact obligation",
                &host_actor,
                "host-waiver-terminal",
                at(8),
                &DevelopmentNoopRedactor,
            )
            .expect("terminal refusal is typed");
        assert!(matches!(
            already_terminal,
            WorkObligationWaiverDecision::Refused {
                code: WorkObligationWaiverRefusalCode::ObligationNotOpen,
                ..
            }
        ));
    }

    #[test]
    fn work_bound_control_checkpoint_records_execution_observation_once() {
        let directory = tempfile::tempdir().expect("temporary directory");
        let database = directory.path().join("engram.sqlite3");
        let mut store = SqliteStore::open(&database).expect("store");
        let frozen_rule_set = store
            .control_diagnostics()
            .expect("initial control policy")
            .obligation_rule_set;
        install_grant(&mut store, "project-control-work", "planner");
        let work = store
            .create_work(
                &root_request("project-control-work", "create-control-work", 1),
                &DevelopmentNoopRedactor,
            )
            .expect("create local work");
        let claim = claim(&mut store, &work, "runner", "claim-control-work", 2, 120);
        let run = load_work_run(&store.connection, claim.run_id).expect("load claimed run");
        let work_binding = ControlWorkBinding {
            root_execution_id: run.root_execution_id,
            work_id: work.work_id,
            run_id: run.run_id,
            work_revision: claim.accepted_work_revision,
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
        };
        let session_id = SessionId("runner".into());
        let connection_token = store
            .resume_control_connection(&session_id, at(3))
            .expect("resume host control connection");
        let mut host_actor = actor("runner");
        host_actor.run_id = Some(run.run_id.0.to_string());
        let control_binding = store
            .bind_control_session_with_work(
                &work.project_id,
                "local-work:control-observation",
                "Record one bound execution observation",
                &session_id,
                &connection_token,
                &host_actor,
                Some(&work_binding),
                ControlAssurance::TurnGated,
                &[EffectClass::Observe, EffectClass::MutateLocal],
                1,
                "bind-control-work",
                at(3),
            )
            .expect("bind control session to live claim");
        assert_eq!(
            control_binding.status.work_binding.as_ref(),
            Some(&work_binding)
        );
        let peer_session = SessionId("peer-runner".into());
        let peer_connection = store
            .resume_control_connection(&peer_session, at(3))
            .expect("resume peer host control connection");
        let mut peer_actor = actor("peer-runner");
        peer_actor.run_id = Some(run.run_id.0.to_string());
        assert!(matches!(
            store.bind_control_session_with_work(
                &work.project_id,
                "local-work:peer-observation",
                "Peer must not inherit another claim",
                &peer_session,
                &peer_connection,
                &peer_actor,
                Some(&work_binding),
                ControlAssurance::TurnGated,
                &[EffectClass::Observe],
                1,
                "bind-peer-control-work",
                at(3),
            ),
            Err(StoreError::WorkClaimMismatch { .. })
        ));

        let synchronize = store
            .evaluate_control_turn(
                &work.project_id,
                &session_id,
                &connection_token,
                &control_binding.routing_token,
                &TurnIntent {
                    idempotency_key: "synchronize-control-work".into(),
                    intent_fingerprint: ObjectHash::from_canonical_bytes(b"sync bound work"),
                    purpose: TurnPurpose::Ordinary,
                    requested_effects: vec![EffectClass::Observe],
                    resource_intents: Vec::new(),
                },
                at(4),
            )
            .expect("evaluate synchronization turn");
        let ControlTurnDecision::Grant { grant: synchronize } = synchronize else {
            panic!("bound work synchronization must grant: {synchronize:?}");
        };
        assert_eq!(synchronize.basis.work_binding.as_ref(), Some(&work_binding));
        let sync_tokens = synchronize
            .delivery
            .as_ref()
            .map(|delivery| vec![delivery.page.delivery_token.clone()])
            .unwrap_or_default();
        assert!(matches!(
            store
                .begin_control_turn(
                    &work.project_id,
                    &session_id,
                    &connection_token,
                    &control_binding.routing_token,
                    &synchronize.grant_id,
                    &sync_tokens,
                    "begin-control-work-sync",
                    at(5),
                )
                .expect("begin bound work synchronization"),
            ControlTurnBeginDecision::Begin { .. }
        ));
        assert!(matches!(
            store
                .checkpoint_control_turn(
                    &work.project_id,
                    &session_id,
                    &connection_token,
                    &control_binding.routing_token,
                    &synchronize.grant_id,
                    TurnNextIntent::Continue,
                    "checkpoint-control-work-sync",
                    at(6),
                )
                .expect("checkpoint bound work synchronization"),
            ControlTurnCheckpointDecision::Checkpointed { .. }
        ));

        let subject = crate::domain::ResourceSubject::Path {
            project_id: work.project_id.clone(),
            segments: vec!["src".into()],
            coverage: crate::domain::ResourceCoverage::Tree,
        };
        let lease = store
            .acquire_work_lease(
                &work.project_id,
                &session_id,
                &connection_token,
                &control_binding.routing_token,
                crate::domain::LeaseKind::Execution,
                crate::domain::LeaseMode::Exclusive,
                &subject,
                60,
                "lease-control-work",
                at(7),
            )
            .expect("acquire bound execution lease");
        assert!(matches!(
            lease,
            crate::domain::WorkLeaseDecision::Granted { .. }
        ));
        let decision = store
            .evaluate_control_turn(
                &work.project_id,
                &session_id,
                &connection_token,
                &control_binding.routing_token,
                &TurnIntent {
                    idempotency_key: "evaluate-control-work".into(),
                    intent_fingerprint: ObjectHash::from_canonical_bytes(b"mutate bound work"),
                    purpose: TurnPurpose::Ordinary,
                    requested_effects: vec![EffectClass::MutateLocal],
                    resource_intents: vec![subject],
                },
                at(8),
            )
            .expect("evaluate bound work mutation");
        let ControlTurnDecision::Grant { grant } = decision else {
            panic!("live bound work mutation must grant: {decision:?}");
        };
        assert_eq!(grant.basis.work_binding.as_ref(), Some(&work_binding));
        let delivery_tokens = grant
            .delivery
            .as_ref()
            .map(|delivery| vec![delivery.page.delivery_token.clone()])
            .unwrap_or_default();
        assert!(matches!(
            store
                .begin_control_turn(
                    &work.project_id,
                    &session_id,
                    &connection_token,
                    &control_binding.routing_token,
                    &grant.grant_id,
                    &delivery_tokens,
                    "begin-control-work",
                    at(9),
                )
                .expect("begin bound work turn"),
            ControlTurnBeginDecision::Begin { .. }
        ));
        let empty_rule_set = crate::domain::ObligationRuleSet {
            schema_version: crate::domain::OBLIGATION_RULE_SET_SCHEMA_VERSION,
            rules: Vec::new(),
        };
        let changed_rules = store
            .set_obligation_rule_set(
                &empty_rule_set,
                &actor("obligation-rule-admin"),
                "disable future built-in obligation triggers in this test",
                "work-obligation-rule-disable",
                None,
                at(9),
                &DevelopmentNoopRedactor,
            )
            .expect("activate empty future rule set after turn begin");
        assert_ne!(changed_rules.obligation_rule_set, frozen_rule_set);
        let out_of_scope = ExecutionObservationInput {
            observation_id: "outside-grant-scope".into(),
            action_fingerprint: ObjectHash::from_canonical_bytes(b"observe after mutation grant"),
            effect: EffectClass::Observe,
            outcome: ExecutionOutcome::Succeeded,
            source_changed: false,
            source_basis: None,
            observed_at: None,
        };
        assert!(matches!(
            store.checkpoint_control_turn_with_observations(
                &work.project_id,
                &session_id,
                &connection_token,
                &control_binding.routing_token,
                &grant.grant_id,
                TurnNextIntent::Continue,
                &[out_of_scope],
                "checkpoint-control-work-scope",
                at(10),
            ),
            Err(StoreError::ControlObservationScopeMismatch { observation_id })
                if observation_id == "outside-grant-scope"
        ));
        let observations = vec![
            ExecutionObservationInput {
                observation_id: "source-mutation-1".into(),
                action_fingerprint: ObjectHash::from_canonical_bytes(b"write src/lib.rs"),
                effect: EffectClass::MutateLocal,
                outcome: ExecutionOutcome::Succeeded,
                source_changed: true,
                source_basis: Some(ExecutionSourceBasis {
                    workspace_id: "workspace-a".into(),
                    source_revision: "content-revision-1".into(),
                }),
                observed_at: Some(at(9)),
            },
            ExecutionObservationInput {
                observation_id: "verification-command-1".into(),
                action_fingerprint: ObjectHash::from_canonical_bytes(b"cargo test --workspace"),
                effect: EffectClass::MutateLocal,
                outcome: ExecutionOutcome::Succeeded,
                source_changed: false,
                source_basis: Some(ExecutionSourceBasis {
                    workspace_id: "workspace-b".into(),
                    source_revision: "content-revision-1".into(),
                }),
                observed_at: Some(at(9)),
            },
        ];
        let verification_inputs = vec![VerificationEvidenceInput {
            producer_observation: ExecutionObservationReference::ObservationId {
                observation_id: "verification-command-1".into(),
            },
            check_kind: VerificationKind::Test,
            environment: Some(EnvironmentEvidenceReference::Index { index: 0 }),
            summary: Some("host observed the workspace test suite".into()),
            refs: vec!["command:cargo-test-workspace".into()],
        }];
        let environment_components = EnvironmentComponents {
            toolchain: "rustc-1.89.0".into(),
            sandbox: Some("windows-host-sandbox-v1".into()),
            workspace_id: "workspace-b".into(),
            capability_map_revision: 1,
        };
        let environment_fingerprint = CanonicalObject::freeze(&environment_components)
            .expect("freeze environment components")
            .hash()
            .clone();
        let environment_inputs = vec![EnvironmentEvidenceInput {
            source_basis: ExecutionSourceBasis {
                workspace_id: "workspace-b".into(),
                source_revision: "content-revision-1".into(),
            },
            environment_fingerprint,
            components: Some(environment_components.clone()),
            observed_at: at(9),
        }];
        let missing_producer = VerificationEvidenceInput {
            producer_observation: ExecutionObservationReference::ObjectHash {
                object_hash: ObjectHash::from_canonical_bytes(b"missing producer"),
            },
            check_kind: VerificationKind::Test,
            environment: None,
            summary: None,
            refs: Vec::new(),
        };
        assert!(matches!(
            store.checkpoint_control_turn_with_evidence(
                &work.project_id,
                &session_id,
                &connection_token,
                &control_binding.routing_token,
                &grant.grant_id,
                TurnNextIntent::Continue,
                &[],
                &[],
                &[EnvironmentEvidenceInput {
                    environment_fingerprint: ObjectHash::from_canonical_bytes(
                        b"wrong environment fingerprint"
                    ),
                    ..environment_inputs[0].clone()
                }],
                "checkpoint-mismatched-environment-fingerprint",
                at(10),
            ),
            Err(StoreError::EnvironmentFingerprintMismatch)
        ));
        let mismatched_components = EnvironmentComponents {
            capability_map_revision: 2,
            ..environment_components.clone()
        };
        assert!(matches!(
            store.checkpoint_control_turn_with_evidence(
                &work.project_id,
                &session_id,
                &connection_token,
                &control_binding.routing_token,
                &grant.grant_id,
                TurnNextIntent::Continue,
                &[],
                &[],
                &[EnvironmentEvidenceInput {
                    environment_fingerprint: CanonicalObject::freeze(&mismatched_components)
                        .expect("freeze mismatched environment components")
                        .hash()
                        .clone(),
                    components: Some(mismatched_components),
                    ..environment_inputs[0].clone()
                }],
                "checkpoint-mismatched-capability-map",
                at(10),
            ),
            Err(StoreError::EnvironmentBasisMismatch(_))
        ));
        assert!(matches!(
            store.checkpoint_control_turn_with_evidence(
                &work.project_id,
                &session_id,
                &connection_token,
                &control_binding.routing_token,
                &grant.grant_id,
                TurnNextIntent::Continue,
                &observations,
                &verification_inputs,
                &[],
                "checkpoint-missing-same-request-environment",
                at(10),
            ),
            Err(StoreError::EnvironmentEvidenceNotFound(index)) if index == "0"
        ));
        assert!(matches!(
            store.checkpoint_control_turn_with_evidence(
                &work.project_id,
                &session_id,
                &connection_token,
                &control_binding.routing_token,
                &grant.grant_id,
                TurnNextIntent::Continue,
                &[],
                &[missing_producer],
                &[],
                "checkpoint-missing-verification-producer",
                at(10),
            ),
            Err(StoreError::VerificationProducerObservationNotFound(_))
        ));
        let checkpointed = store
            .checkpoint_control_turn_with_evidence(
                &work.project_id,
                &session_id,
                &connection_token,
                &control_binding.routing_token,
                &grant.grant_id,
                TurnNextIntent::Continue,
                &observations,
                &verification_inputs,
                &environment_inputs,
                "checkpoint-control-work",
                at(10),
            )
            .expect("checkpoint bound work turn");
        let ControlTurnCheckpointDecision::Checkpointed { receipt } = &checkpointed else {
            panic!("bound work turn must checkpoint");
        };
        assert_eq!(receipt.execution_observations.len(), 2);
        assert_eq!(receipt.verification_evidence.len(), 1);
        assert_eq!(receipt.environment_evidence.len(), 1);
        let observation_hash = &receipt.execution_observations[0];
        let observation = load_typed_work_object::<ExecutionObservation>(
            &store.connection,
            observation_hash,
            "execution_observation",
        )
        .expect("load canonical execution observation");
        assert_eq!(observation.binding, work_binding);
        assert_eq!(observation.session_id, session_id);
        assert!(observation.source_changed);
        assert_eq!(observation.obligation_rule_set, frozen_rule_set);
        assert_eq!(observation.source_basis, observations[0].source_basis);
        assert_eq!(observation.observed_at, observations[0].observed_at);
        assert_eq!(observation.recorded_at, at(10));
        let feed_count = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM work_feed_entries WHERE object_hash = ?1",
                [observation_hash.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .expect("count observation feed entries");
        assert_eq!(feed_count, 3);

        let verification_hash = &receipt.verification_evidence[0];
        let verification = store
            .load_verification_evidence(verification_hash)
            .expect("load typed verification evidence");
        let producer_hash = &receipt.execution_observations[1];
        let producer = load_control_execution_observation_on(&store.connection, producer_hash)
            .expect("load verification producer")
            .expect("verification producer exists");
        assert_eq!(verification.producer_observation, *producer_hash);
        assert_eq!(verification.source_basis.workspace_id, "workspace-b");
        assert_eq!(
            verification.source_basis.source_revision,
            "content-revision-1"
        );
        assert_eq!(verification.check_fingerprint, producer.action_fingerprint);
        assert_eq!(
            verification.environment.as_ref(),
            Some(&receipt.environment_evidence[0])
        );
        assert_eq!(
            verification.result,
            crate::domain::VerificationResult::Passed
        );
        assert_eq!(
            store
                .work_evidence_kind(run.run_id, verification_hash)
                .expect("verification projection kind"),
            WorkEvidenceKind::Verification
        );
        let not_yet_recorded_environment =
            ObjectHash::from_canonical_bytes(b"required environment not yet produced");
        let focus_candidates = store
            .work_run_evidence_projection(
                run.run_id,
                std::slice::from_ref(&not_yet_recorded_environment),
                8,
            )
            .expect("an unmet required environment is not projection corruption");
        assert!(
            focus_candidates
                .iter()
                .all(|candidate| candidate.hash != not_yet_recorded_environment)
        );
        assert!(matches!(
            store.work_run_evidence_projection(
                run.run_id,
                std::slice::from_ref(verification_hash),
                8,
            ),
            Err(StoreError::InvalidWorkProjection(_))
        ));
        store
            .connection
            .execute(
                "UPDATE work_run_evidence SET evidence_kind = 'generic'
                 WHERE evidence_hash = ?1",
                [verification_hash.as_str()],
            )
            .expect("corrupt selection-driving evidence kind");
        assert!(matches!(
            store.work_run_evidence_projection(run.run_id, &[], 8),
            Err(StoreError::InvalidWorkProjection(_))
        ));
        store
            .connection
            .execute(
                "UPDATE work_run_evidence SET evidence_kind = 'verification'
                 WHERE evidence_hash = ?1",
                [verification_hash.as_str()],
            )
            .expect("restore evidence selection projection");
        let environment_hash = &receipt.environment_evidence[0];
        let environment = store
            .load_environment_evidence(environment_hash)
            .expect("load typed environment evidence");
        assert_eq!(environment.source_basis.workspace_id, "workspace-b");
        assert_eq!(
            environment.environment_fingerprint,
            CanonicalObject::freeze(&environment_components)
                .expect("re-freeze environment components")
                .hash()
                .clone()
        );
        assert_eq!(
            environment.components.as_ref(),
            Some(&environment_components)
        );
        let opaque_environment = EnvironmentEvidence {
            components: None,
            ..environment.clone()
        };
        let opaque_object = CanonicalObject::freeze(&opaque_environment)
            .expect("freeze opaque environment evidence");
        assert!(
            !std::str::from_utf8(opaque_object.bytes())
                .expect("environment evidence is UTF-8 JSON")
                .contains("components"),
            "the optional component field changed opaque canonical bytes"
        );
        assert_eq!(
            opaque_object
                .decode::<EnvironmentEvidence>()
                .expect("decode opaque environment evidence"),
            opaque_environment
        );
        assert_eq!(
            super::super::resolve_verification_environment_on(
                &store.connection,
                Some(&EnvironmentEvidenceReference::ObjectHash {
                    object_hash: environment_hash.clone(),
                }),
                &[],
                &work.project_id,
                &work_binding,
                &verification.source_basis,
            )
            .expect("resolve existing environment by hash"),
            Some(environment_hash.clone())
        );
        let mut wrong_environment_binding = work_binding.clone();
        wrong_environment_binding.run_id = WorkRunId(uuid::Uuid::now_v7());
        assert!(matches!(
            super::super::resolve_verification_environment_on(
                &store.connection,
                Some(&EnvironmentEvidenceReference::ObjectHash {
                    object_hash: environment_hash.clone(),
                }),
                &[],
                &work.project_id,
                &wrong_environment_binding,
                &verification.source_basis,
            ),
            Err(StoreError::EnvironmentBasisMismatch(_))
        ));
        let wrong_source_basis = ExecutionSourceBasis {
            source_revision: "content-revision-2".into(),
            ..verification.source_basis.clone()
        };
        assert!(matches!(
            super::super::resolve_verification_environment_on(
                &store.connection,
                Some(&EnvironmentEvidenceReference::ObjectHash {
                    object_hash: environment_hash.clone(),
                }),
                &[],
                &work.project_id,
                &work_binding,
                &wrong_source_basis,
            ),
            Err(StoreError::EnvironmentBasisMismatch(_))
        ));
        assert!(matches!(
            super::super::resolve_verification_environment_on(
                &store.connection,
                Some(&EnvironmentEvidenceReference::ObjectHash {
                    object_hash: ObjectHash::from_canonical_bytes(b"missing environment"),
                }),
                &[],
                &work.project_id,
                &work_binding,
                &verification.source_basis,
            ),
            Err(StoreError::EnvironmentEvidenceNotFound(_))
        ));
        assert_eq!(
            store
                .work_evidence_kind(run.run_id, environment_hash)
                .expect("environment projection kind"),
            WorkEvidenceKind::Environment
        );
        for evidence_hash in [verification_hash, environment_hash] {
            let typed_feed_count = store
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM work_feed_entries WHERE object_hash = ?1",
                    [evidence_hash.as_str()],
                    |row| row.get::<_, i64>(0),
                )
                .expect("count typed evidence feed entries");
            assert_eq!(typed_feed_count, 3);
        }
        let run_positions = |hash: &ObjectHash| {
            store
                .connection
                .query_row(
                    "SELECT position FROM work_feed_entries
                     WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND object_hash = ?2",
                    params![run.run_id.0.to_string(), hash.as_str()],
                    |row| row.get::<_, i64>(0),
                )
                .expect("typed run-feed position")
        };
        let verification_match = VerificationEvidenceMatchInput {
            candidate_kind: WorkEvidenceKind::Verification,
            evidence: Some(&verification),
            producer: Some(&producer),
            latest_mutation: &observation,
            evidence_position: run_positions(verification_hash),
            latest_mutation_position: run_positions(observation_hash),
            requirement: &crate::domain::VerificationRequirement {
                check_kind: crate::domain::VerificationKind::Test,
                check_fingerprint: Some(producer.action_fingerprint.clone()),
                required_environment: None,
            },
        };
        assert_eq!(match_verification_evidence(&verification_match), Ok(()));
        let exact_environment_requirement = crate::domain::VerificationRequirement {
            check_kind: crate::domain::VerificationKind::Test,
            check_fingerprint: Some(producer.action_fingerprint.clone()),
            required_environment: Some(environment_hash.clone()),
        };
        assert_eq!(
            match_verification_evidence(&VerificationEvidenceMatchInput {
                requirement: &exact_environment_requirement,
                ..verification_match
            }),
            Ok(())
        );
        let wrong_environment_requirement = crate::domain::VerificationRequirement {
            required_environment: Some(ObjectHash::from_canonical_bytes(b"other environment")),
            ..exact_environment_requirement
        };
        assert_eq!(
            match_verification_evidence(&VerificationEvidenceMatchInput {
                requirement: &wrong_environment_requirement,
                ..verification_match
            }),
            Err(VerificationEvidenceMismatch::EnvironmentMismatch)
        );
        let obligations = store
            .work_run_obligations(run.run_id)
            .expect("load immutable run obligations");
        assert_eq!(obligations.len(), 1);
        let obligation = &obligations[0];
        assert_eq!(obligation.state, WorkObligationState::Satisfied);
        assert_eq!(obligation.obligation.rule_set, frozen_rule_set);
        assert_eq!(
            obligation.obligation.triggering_observation,
            *observation_hash
        );
        assert_eq!(
            obligation.obligation.requirement.check_kind,
            VerificationKind::Test
        );
        assert_eq!(obligation.obligation.requirement.check_fingerprint, None);
        assert!(matches!(
            obligation
                .resolution
                .as_ref()
                .map(|event| &event.resolution),
            Some(WorkObligationResolution::Satisfied { evidence, .. })
                if evidence == verification_hash
        ));
        for hash in [
            &obligation.definition_hash,
            obligation
                .resolution_hash
                .as_ref()
                .expect("satisfied resolution hash"),
        ] {
            let feed_count = store
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM work_feed_entries WHERE object_hash = ?1",
                    [hash.as_str()],
                    |row| row.get::<_, i64>(0),
                )
                .expect("count obligation feed entries");
            assert_eq!(feed_count, 3);
        }
        let mut later_mutation = observation.clone();
        later_mutation
            .source_basis
            .as_mut()
            .expect("mutation source basis")
            .source_revision = "content-revision-2".into();
        let stale_match = VerificationEvidenceMatchInput {
            latest_mutation: &later_mutation,
            latest_mutation_position: run_positions(verification_hash) + 1,
            evidence_position: run_positions(verification_hash),
            ..verification_match
        };
        assert_eq!(
            match_verification_evidence(&stale_match),
            Err(VerificationEvidenceMismatch::StaleSourceRevision)
        );

        let objects_before_attach = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM objects WHERE object_hash = ?1",
                [verification_hash.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .expect("count verification objects before attach");
        let feeds_before_attach = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM work_feed_entries WHERE object_hash = ?1",
                [verification_hash.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .expect("count verification feeds before attach");
        let work_protocol = crate::LocalWorkService::new(
            database.clone(),
            work.project_id.clone(),
            "runner".into(),
            session_id.clone(),
            Some("typed-evidence-attach-test".into()),
            None,
        );
        work_protocol
            .work_focus(&work.short_ref, at(10))
            .expect("focus claimed work before attach");
        let attached = work_protocol
            .work_update(
                crate::WorkUpdateInput::Evidence {
                    summary: String::new(),
                    refs: Vec::new(),
                    attach: Some(crate::WorkEvidenceAttachInput {
                        evidence: verification_hash.to_string(),
                    }),
                    idempotency_key: "attach-verification-evidence".into(),
                },
                at(10),
            )
            .expect("attach host-minted verification evidence");
        assert_eq!(attached.receipt.result["attached"], true);
        assert_eq!(
            attached.receipt.result["evidence"],
            verification_hash.as_str()
        );
        assert_eq!(attached.receipt.result["evidence_kind"], "verification");
        let focus = work_protocol
            .work_focus(&work.short_ref, at(10))
            .expect("focus after obligation resolution");
        assert_eq!(focus.obligation_page.items.len(), 1);
        assert_eq!(
            focus.obligation_page.items[0].state,
            WorkObligationState::Satisfied
        );
        let objects_after_attach = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM objects WHERE object_hash = ?1",
                [verification_hash.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .expect("count verification objects after attach");
        let feeds_after_attach = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM work_feed_entries WHERE object_hash = ?1",
                [verification_hash.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .expect("count verification feeds after attach");
        assert_eq!(objects_after_attach, objects_before_attach);
        assert_eq!(feeds_after_attach, feeds_before_attach);

        let replay = store
            .checkpoint_control_turn_with_evidence(
                &work.project_id,
                &session_id,
                &connection_token,
                &control_binding.routing_token,
                &grant.grant_id,
                TurnNextIntent::Continue,
                &observations,
                &verification_inputs,
                &environment_inputs,
                "checkpoint-control-work",
                at(11),
            )
            .expect("replay checkpoint exactly");
        assert_eq!(replay, checkpointed);
        let replay_feed_count = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM work_feed_entries WHERE object_hash = ?1",
                [observation_hash.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .expect("count replayed observation feed entries");
        assert_eq!(replay_feed_count, 3);

        let mut changed_observations = observations.clone();
        changed_observations[0].source_changed = false;
        assert!(matches!(
            store.checkpoint_control_turn_with_observations(
                &work.project_id,
                &session_id,
                &connection_token,
                &control_binding.routing_token,
                &grant.grant_id,
                TurnNextIntent::Continue,
                &changed_observations,
                "checkpoint-control-work",
                at(12),
            ),
            Err(StoreError::ControlOperationIdempotencyConflict { operation, key })
                if operation == "turn_checkpoint" && key == "checkpoint-control-work"
        ));

        let mut future_observation = observation.clone();
        future_observation.observation_id = "source-mutation-under-empty-rules".into();
        future_observation.action_fingerprint =
            ObjectHash::from_canonical_bytes(b"write after rule-set activation");
        future_observation.obligation_rule_set = changed_rules.obligation_rule_set.clone();
        future_observation
            .source_basis
            .as_mut()
            .expect("future mutation source basis")
            .source_revision = "content-revision-2".into();
        future_observation.observed_at = Some(at(12));
        future_observation.recorded_at = at(12);
        let transaction = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("future observation transaction");
        append_control_execution_observation_on(&transaction, &future_observation)
            .expect("append observation under the newly active empty rule set");
        transaction.commit().expect("commit future observation");
        let retained = store
            .work_run_obligations(run.run_id)
            .expect("reload obligations after rule-set activation");
        assert_eq!(retained.len(), 1);
        assert_eq!(retained[0].definition_hash, obligation.definition_hash);
        store
            .connection
            .execute_batch("SAVEPOINT corrupt_obligation_rule_set")
            .expect("start obligation rule-set corruption fixture");
        store
            .connection
            .execute(
                "UPDATE work_run_obligations SET rule_set_hash = ?1
                 WHERE definition_hash = ?2",
                params![
                    changed_rules.obligation_rule_set.as_str(),
                    obligation.definition_hash.as_str()
                ],
            )
            .expect("corrupt obligation rule-set projection");
        assert!(store.work_run_obligations(run.run_id).is_err());
        let corrupt_rules = store
            .verify_all()
            .expect("report obligation rule-set corruption");
        assert!(
            corrupt_rules
                .invalid_work_records
                .iter()
                .any(|record| record.contains("work_obligation")),
            "rule-set corruption was not reported: {corrupt_rules:?}"
        );
        store
            .connection
            .execute_batch(
                "ROLLBACK TO corrupt_obligation_rule_set; RELEASE corrupt_obligation_rule_set",
            )
            .expect("restore obligation rule-set projection");

        let stale = store
            .evaluate_control_turn(
                &work.project_id,
                &session_id,
                &connection_token,
                &control_binding.routing_token,
                &TurnIntent {
                    idempotency_key: "evaluate-expired-control-work".into(),
                    intent_fingerprint: ObjectHash::from_canonical_bytes(b"expired bound work"),
                    purpose: TurnPurpose::Ordinary,
                    requested_effects: vec![EffectClass::Observe],
                    resource_intents: Vec::new(),
                },
                at(123),
            )
            .expect("expired bound work is a typed refusal");
        assert!(matches!(
            stale,
            ControlTurnDecision::Refuse { directive }
                if directive.code == ControlRefusalCode::StaleFence
        ));
        let stale_connection = store
            .resume_control_connection(&session_id, at(124))
            .expect("resume stale binding connection");
        assert!(matches!(
            store.bind_control_session_with_work(
                &work.project_id,
                "local-work:stale-observation",
                "Stale binding must require a reread",
                &session_id,
                &stale_connection,
                &host_actor,
                Some(&work_binding),
                ControlAssurance::TurnGated,
                &[EffectClass::Observe],
                1,
                "bind-stale-control-work",
                at(124),
            ),
            Err(StoreError::ControlWorkBindingStale { .. })
        ));
        let projection_corruptions = [
            (
                verification_hash,
                "verification-kind",
                "UPDATE work_run_evidence SET evidence_kind = 'environment'
                 WHERE evidence_hash = ?1",
                None,
            ),
            (
                verification_hash,
                "verification-workspace",
                "UPDATE work_run_evidence SET workspace_id = 'forged-workspace'
                 WHERE evidence_hash = ?1",
                None,
            ),
            (
                verification_hash,
                "verification-revision",
                "UPDATE work_run_evidence SET source_revision = 'forged-revision'
                 WHERE evidence_hash = ?1",
                None,
            ),
            (
                verification_hash,
                "verification-session",
                "UPDATE work_run_evidence SET producer_session_id = 'forged-session'
                 WHERE evidence_hash = ?1",
                None,
            ),
            (
                verification_hash,
                "verification-producer",
                "UPDATE work_run_evidence SET producer_observation_hash = ?2
                 WHERE evidence_hash = ?1",
                Some(observation_hash.as_str()),
            ),
            (
                verification_hash,
                "verification-check",
                "UPDATE work_run_evidence SET check_fingerprint = ?2
                 WHERE evidence_hash = ?1",
                Some(environment_hash.as_str()),
            ),
            (
                verification_hash,
                "verification-result",
                "UPDATE work_run_evidence SET verification_result = 'failed'
                 WHERE evidence_hash = ?1",
                None,
            ),
            (
                verification_hash,
                "verification-time",
                "UPDATE work_run_evidence SET observed_at_ms = observed_at_ms + 1
                 WHERE evidence_hash = ?1",
                None,
            ),
            (
                verification_hash,
                "verification-environment-fingerprint",
                "UPDATE work_run_evidence SET environment_fingerprint = ?2
                 WHERE evidence_hash = ?1",
                Some(environment_hash.as_str()),
            ),
            (
                verification_hash,
                "verification-environment-link",
                "UPDATE work_run_evidence SET environment_evidence_hash = ?2
                 WHERE evidence_hash = ?1",
                Some(producer_hash.as_str()),
            ),
            (
                verification_hash,
                "verification-components",
                "UPDATE work_run_evidence SET components_json = X'7B7D'
                 WHERE evidence_hash = ?1",
                None,
            ),
            (
                environment_hash,
                "environment-kind",
                "UPDATE work_run_evidence SET evidence_kind = 'verification'
                 WHERE evidence_hash = ?1",
                None,
            ),
            (
                environment_hash,
                "environment-workspace",
                "UPDATE work_run_evidence SET workspace_id = 'forged-workspace'
                 WHERE evidence_hash = ?1",
                None,
            ),
            (
                environment_hash,
                "environment-revision",
                "UPDATE work_run_evidence SET source_revision = 'forged-revision'
                 WHERE evidence_hash = ?1",
                None,
            ),
            (
                environment_hash,
                "environment-session",
                "UPDATE work_run_evidence SET producer_session_id = 'forged-session'
                 WHERE evidence_hash = ?1",
                None,
            ),
            (
                environment_hash,
                "environment-producer",
                "UPDATE work_run_evidence SET producer_observation_hash = ?2
                 WHERE evidence_hash = ?1",
                Some(producer_hash.as_str()),
            ),
            (
                environment_hash,
                "environment-check",
                "UPDATE work_run_evidence SET check_fingerprint = ?2
                 WHERE evidence_hash = ?1",
                Some(verification_hash.as_str()),
            ),
            (
                environment_hash,
                "environment-result",
                "UPDATE work_run_evidence SET verification_result = 'passed'
                 WHERE evidence_hash = ?1",
                None,
            ),
            (
                environment_hash,
                "environment-time",
                "UPDATE work_run_evidence SET observed_at_ms = observed_at_ms + 1
                 WHERE evidence_hash = ?1",
                None,
            ),
            (
                environment_hash,
                "environment-fingerprint",
                "UPDATE work_run_evidence SET environment_fingerprint = ?2
                 WHERE evidence_hash = ?1",
                Some(verification_hash.as_str()),
            ),
            (
                environment_hash,
                "environment-link",
                "UPDATE work_run_evidence SET environment_evidence_hash = ?2
                 WHERE evidence_hash = ?1",
                Some(verification_hash.as_str()),
            ),
            (
                environment_hash,
                "environment-components",
                "UPDATE work_run_evidence SET components_json = X'7B7D'
                 WHERE evidence_hash = ?1",
                None,
            ),
        ];
        for (evidence_hash, label, sql, second_value) in projection_corruptions {
            store
                .connection
                .execute_batch("SAVEPOINT corrupt_typed_evidence")
                .expect("start typed-evidence corruption savepoint");
            match second_value {
                Some(value) => store
                    .connection
                    .execute(sql, params![evidence_hash.as_str(), value]),
                None => store.connection.execute(sql, [evidence_hash.as_str()]),
            }
            .unwrap_or_else(|error| panic!("corrupt {label}: {error}"));
            assert!(
                work_evidence_kind_on(&store.connection, run.run_id, evidence_hash).is_err(),
                "{label} remained readable through the lifecycle path"
            );
            assert!(
                ensure_run_evidence(
                    &store.connection,
                    run.run_id,
                    std::slice::from_ref(evidence_hash),
                )
                .is_err(),
                "{label} remained checkpointable"
            );
            let corrupt_report = store
                .verify_all()
                .unwrap_or_else(|error| panic!("verify {label}: {error}"));
            assert!(
                corrupt_report.invalid_work_records.iter().any(|record| {
                    record == &format!("work_evidence:{evidence_hash}:run_binding")
                }),
                "{label} was not reported: {corrupt_report:?}"
            );
            store
                .connection
                .execute_batch("ROLLBACK TO corrupt_typed_evidence; RELEASE corrupt_typed_evidence")
                .unwrap_or_else(|error| panic!("restore {label}: {error}"));
        }
        let final_report = store.verify_all().expect("integrity report");
        assert!(final_report.is_healthy(), "{final_report:?}");
    }
}
