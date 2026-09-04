//! First-class local work graph persistence.

#![allow(
    clippy::too_many_lines,
    reason = "work lifecycle transactions stay contiguous so their atomic invariants remain auditable"
)]

mod completion;
mod execution;
mod feeds;
mod integrity;
mod planning;
mod query;
mod schema;
mod session;

#[cfg(test)]
mod test_support;

#[cfg(test)]
use std::cell::Cell;
#[cfg(test)]
use std::collections::HashMap;

use chrono::{DateTime, Utc};
#[cfg(test)]
use rusqlite::{Connection, TransactionBehavior, params};
use serde::{Deserialize, Serialize};

#[cfg(test)]
use super::{BeginGateWorkProtocolAttempt, BeginWorkProtocolAttempt, SqliteStore, StoreError};
use crate::{
    CanonicalObject, ObjectHash,
    domain::{
        ActorContext, CompletionSeal, FeedPosition, RootExecution, SCHEMA_VERSION, SessionId,
        TaskId, WorkBlocker, WorkClaim, WorkClaimId, WorkCompletionRecovery, WorkEvent,
        WorkEvidenceKind, WorkFeedEntry, WorkHandoffOffer, WorkId, WorkItem, WorkObligation,
        WorkObligationId, WorkObligationResolutionEvent, WorkObligationState,
        WorkPrerequisiteState, WorkRun, WorkRunId, WorkTransition,
    },
    schema::WORK_SCHEMA_VERSION as CURRENT_WORK_SCHEMA_VERSION,
};

#[cfg(test)]
use crate::{
    domain::{
        AcceptWorkHandoffRequest, AcceptanceResult, AddWorkBlockerRequest,
        ChangeWorkPrerequisiteRequest, ChildRequirement, ClaimWorkRequest, ClearWorkBlockerRequest,
        ControlWorkBinding, DEFAULT_WORK_CLAIM_TTL_SECONDS, DecomposeWorkRequest,
        DisposeWorkRequest, ExecutionObservation, FeedId, OfferWorkHandoffRequest,
        RecordGateEvidenceRequest, ReleaseWorkRequest, ReviseWorkRequest, WorkAvailability,
        WorkDependencyRef, WorkDisposition, WorkObligationResolution, WorkOrigin,
        WorkReadinessReason,
    },
    memory::Redactor,
};

const MAX_WORK_TTL_SECONDS: i64 = 86_400;
const MAX_WORK_SOURCE_SNAPSHOT_BYTES: usize = 128 * 1_024;
const MAX_WORK_DEPTH: u32 = 4;
const MAX_OPEN_WORK_DESCENDANTS: u32 = 128;
const MAX_CHILDREN_PER_DECOMPOSITION: usize = 16;

// A checkpoint acknowledges the run feed immediately before its own object and
// its matching checkpoint event are appended.
const CHECKPOINT_APPEND_COUNT: i64 = 2;
const MAX_OPEN_COMPLETION_OBLIGATIONS: usize = 16;
const MAX_COMPLETION_ENVIRONMENT_EVIDENCE: usize = 64;

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

pub(crate) use completion::normalize_completion_acceptance_shape;
pub(super) use completion::{
    append_control_environment_evidence_on, append_control_execution_observation_on,
    append_control_verification_evidence_on,
};
pub(super) use feeds::{
    append_context_object_to_work_feeds, append_memory_capture_to_work_feeds,
    load_control_environment_evidence_on, load_control_execution_observation_on,
};
#[cfg(test)]
use planning::persist_work_item;
pub(super) use planning::validate_control_work_binding_on;
#[cfg(test)]
use query::{canonical_work_events_for_item, feed_parts};
pub(super) use query::{context_work_feed_heads, verified_work_identity};
pub(super) use schema::{
    initialize_schema, is_rebuildable_schema_object, owns_schema_object, preflight_schema,
    repair_rebuildable_schema_on, require_work_schema_version, schema_version,
};

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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) checkpoint: Option<ObjectHash>,
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

#[derive(Clone, Debug)]
pub(crate) enum CompleteWorkStorageResult {
    Completed(Box<CompletionSeal>),
    Recovery(CompletionRecoverySnapshot),
}

#[derive(Clone, Debug)]
pub(crate) struct CompletionRecoverySnapshot {
    pub(crate) recovery: WorkCompletionRecovery,
    pub(crate) obligations: Vec<WorkObligationRecord>,
}
