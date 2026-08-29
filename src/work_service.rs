//! Ambient six-operation protocol over the local work lifecycle.

use std::{path::PathBuf, str::FromStr};

#[cfg(test)]
use std::sync::{Arc, Barrier};

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::{
    AcceptWorkHandoffRequest, AcceptanceResult, ActorContext, AddWorkBlockerRequest,
    CancelWorkHandoffRequest, CanonicalObject, ChangeWorkPrerequisiteRequest,
    CheckpointWorkRequest, ChildRequirement, ChildWorkDraft, ChildWorkPrerequisite,
    ClaimWorkRequest, ClearWorkBlockerRequest, CompleteWorkRequest, CompletionDrainAttestation,
    CompletionSeal, ControlWorkBinding, CreateWorkRequest, DecomposeWorkRequest,
    DevelopmentNoopRedactor, DisposeWorkRequest, EnvironmentEvidence, ExecutionObservation, FeedId,
    LifecycleAuthorityDecision, MemorySummary, MemoryVersion, ObjectHash, OfferWorkHandoffRequest,
    ProjectId, ReadyWork, RecordWorkEvidenceRequest, ReleaseWorkRequest, ReopenWorkRequest,
    ReviseWorkRequest, SessionId, SqliteStore, TaskId, VerificationEvidence, VerificationKind,
    VerificationResult, WaiveRequiredChildRequest, WorkAuthorityOperation, WorkAvailability,
    WorkBlockerKind, WorkCatalogQuery, WorkCheckpoint, WorkClaim, WorkClaimState,
    WorkDecomposition, WorkDependencyRef, WorkDisposition, WorkEvent, WorkEvidence,
    WorkEvidenceKind, WorkFeedEntry, WorkHandoffOffer, WorkHandoffState, WorkId, WorkItem,
    WorkItemKind, WorkLifecycle, WorkObligation, WorkObligationResolution,
    WorkObligationResolutionEvent, WorkObligationState, WorkOrigin, WorkPlanningAuthority,
    WorkRevisionPatch, WorkRun, WorkRunId, WorkRunState, WorkSessionState, WorkTransition,
    domain::{
        AssuranceLevel, MemoryAssertionEvent, MemoryContradictionEvent, ProvenanceLink,
        ProvenanceRelation, SCHEMA_VERSION, Scope, Sensitivity,
    },
    storage::{
        BeginWorkProtocolAttempt, StageWorkSessionDelivery, StoreError,
        normalize_completion_acceptance_shape,
    },
};

/// Hard ceiling for every successful agent-facing work response.
pub const MAX_AGENT_WORK_RESPONSE_BYTES: usize = 12 * 1024;

const MAX_CHANGE_SECTION_BYTES: usize = 4 * 1024;
const MAX_READY_SECTION_BYTES: usize = 2 * 1024;
const MAX_CATALOG_SECTION_BYTES: usize = 3 * 1024;
const MAX_FOCUS_HISTORY: u32 = 4;
const MAX_FOCUS_RELATIONS: usize = 8;
const MAX_FOCUS_MEMORIES: u32 = 8;
const MAX_SUMMARY_BYTES: usize = 192;
const MAX_ACCEPTANCE_ITEMS: usize = 6;
const MAX_LABEL_ITEMS: usize = 8;
const MAX_DELIVERY_STAGE_RETRIES: usize = 8;

/// Immutable host context for one CLI or MCP work-service connection.
#[derive(Clone, Debug)]
pub struct LocalWorkService {
    database: PathBuf,
    project_id: ProjectId,
    actor_id: String,
    session_id: SessionId,
    source_skill: Option<String>,
    authority_grant: Option<ObjectHash>,
    #[cfg(test)]
    delivery_stage_hook: Option<DeliveryStageTestHook>,
}

#[cfg(test)]
#[derive(Clone, Debug)]
struct DeliveryStageTestHook {
    entered: Arc<Barrier>,
    release: Arc<Barrier>,
}

struct WorkGuidance {
    status: ReadyWork,
    allowed_next: Vec<String>,
    waivable_required_children: Vec<RequiredChildWaiverCandidate>,
    claim: Option<WorkClaim>,
    handoffs: Vec<WorkHandoffOffer>,
}

struct CompletionEvidencePlan<'a> {
    work: &'a WorkItem,
    claim: &'a WorkClaim,
    capture: Option<&'a WorkCompletionCaptureInput>,
    evidence: Vec<ObjectHash>,
    raw_key: &'a str,
    now: DateTime<Utc>,
}

#[derive(Serialize)]
struct WorkProtocolIntent<'a, T> {
    project_id: &'a ProjectId,
    session_id: &'a SessionId,
    actor_id: &'a str,
    source_skill: Option<&'a str>,
    authority_grant: Option<&'a ObjectHash>,
    input: &'a T,
}

#[derive(Deserialize, Serialize)]
struct WorkProtocolBasis {
    focused_work: Option<WorkItem>,
    claim: Option<WorkClaim>,
    handoffs: Vec<WorkHandoffOffer>,
}

#[derive(Serialize)]
struct WorkCoreOperationKey<'a> {
    project_id: &'a ProjectId,
    session_id: &'a SessionId,
    protocol_operation: &'a str,
    caller_key: &'a str,
    core_operation: &'a str,
}

/// Bounded `work_next` response using an ambient per-session project cursor.
/// `changes` is one exact dense staged range. The refreshed focus, readiness,
/// and catalog sections are advisory views assembled afterward and may observe
/// newer concurrent commits; every mutation revalidates its canonical basis.
#[derive(Clone, Debug, Serialize)]
pub struct WorkNextView {
    pub session: AgentWorkSession,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub focus: Option<WorkFocusView>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub ready: Option<Vec<ReadyWorkSummary>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub catalog: Option<WorkCatalogSummaryPage>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub changes: Option<Vec<WorkChange>>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivered_through: Option<i64>,
    /// Opaque capability required with `delivered_through` to acknowledge a
    /// staged page. Replay returns the same token; it is never exposed by an
    /// error or agent-session projection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery_token: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub omissions: Vec<WorkSectionOmission>,
}

/// Agent-safe navigation state. The tentative cursor and acknowledgement token
/// are intentionally hidden; callers may acknowledge only a page they received.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct AgentWorkSession {
    pub project_id: ProjectId,
    pub session_id: SessionId,
    pub focused_work_id: Option<WorkId>,
    pub confirmed_project_cursor: i64,
    pub pending_delivery: bool,
    pub updated_at: DateTime<Utc>,
}

/// Optional project-wide filters carried by `work_next` without adding a
/// seventh protocol verb.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct WorkNextQuery {
    /// Sections to return. Empty means the normal focus, ready, catalog, and
    /// changes packet. A changes-free query never stages or advances delivery.
    #[serde(default)]
    pub sections: Vec<WorkNextSection>,
    pub search: Option<String>,
    #[serde(default)]
    pub lifecycles: Vec<WorkLifecycle>,
    #[serde(default)]
    pub availabilities: Vec<WorkAvailability>,
    #[serde(default)]
    pub blocked_only: bool,
    pub assigned_to: Option<String>,
    pub label: Option<String>,
    pub after: Option<String>,
}

/// Selectable `work_next` response section.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkNextSection {
    Focus,
    Ready,
    Catalog,
    Changes,
}

impl FromStr for WorkNextSection {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "focus" => Ok(Self::Focus),
            "ready" => Ok(Self::Ready),
            "catalog" => Ok(Self::Catalog),
            "changes" => Ok(Self::Changes),
            _ => Err("expected focus, ready, catalog, or changes"),
        }
    }
}

/// One hash-verified source object at an exact project-feed position, exposed
/// as an authority-redacted projection. `entry.object_hash` binds the original
/// canonical bytes; it intentionally does not hash the compact `delivery`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkChange {
    pub entry: WorkFeedEntry,
    pub delivery: WorkChangeProjection,
}

/// Exact agent projection persisted beside an unacknowledged dense feed page.
/// Replays decode these bytes instead of rebuilding against mutable focus or
/// legacy-task bindings.
#[derive(Clone, Debug, Deserialize, Serialize)]
struct StagedWorkChangePage {
    schema_version: u16,
    changes: Vec<WorkChange>,
    omitted_count: usize,
}

/// Agent-facing projection of a verified feed object. Visible objects retain
/// their existing JSON shape; protected memory entries become a typed marker
/// so the dense project cursor can advance without disclosing their payload.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum WorkChangeProjection {
    Omitted(WorkChangeOmission),
    Visible(WorkChangeSummary),
}

/// Compact, non-canonical description of one verified source object. Fetch
/// content by `entry.object_hash` through an authorized object-specific read
/// when more detail is needed.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkChangeSummary {
    pub schema_version: u16,
    pub object_kind: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_id: Option<WorkId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub work_ref: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub revision: Option<i64>,
    pub change_kind: String,
    pub summary: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<String>,
    pub created_at: DateTime<Utc>,
}

/// Density-preserving marker for a feed object outside the current read boundary.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkChangeOmission {
    pub schema_version: u16,
    pub object_kind: String,
    pub omission: WorkChangeOmissionReason,
}

/// Why a verified project-feed object was not exposed to the agent.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkChangeOmissionReason {
    RestrictedSensitivity,
    OutsideFocusedRoot,
    OutsideBoundTask,
}

/// A response section was deliberately bounded without consuming omitted
/// project-feed positions.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkSectionOmission {
    pub section: WorkNextSection,
    pub reason: WorkSectionOmissionReason,
    pub omitted_count: usize,
}

/// Why an advisory response section is incomplete.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkSectionOmissionReason {
    ByteBudget,
    CountLimit,
}

/// Bounded work identity and planning fields used on the agent wire.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkItemSummary {
    pub work_id: WorkId,
    pub short_ref: String,
    pub root_id: WorkId,
    pub parent_id: Option<WorkId>,
    pub title: String,
    pub outcome: String,
    pub acceptance: Vec<String>,
    #[serde(default)]
    pub acceptance_count: usize,
    pub kind: WorkItemKind,
    pub priority: i32,
    pub labels: Vec<String>,
    pub assigned_to: Option<String>,
    pub lifecycle: WorkLifecycle,
    pub revision: i64,
    pub active_run_id: Option<WorkRunId>,
    pub superseded_by: Option<WorkId>,
    pub updated_at: DateTime<Utc>,
}

/// Compact readiness card suitable for repeated ambient delivery.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct ReadyWorkSummary {
    pub work: WorkItemSummary,
    pub availability: WorkAvailability,
    pub reason_codes: Vec<crate::WorkReadinessReason>,
    pub why: Vec<String>,
    pub blocked_by: Vec<WorkId>,
    pub blocker_count: usize,
}

/// Bounded catalog page; `next_after` remains a stable continuation cursor.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkCatalogSummaryPage {
    pub items: Vec<ReadyWorkSummary>,
    pub next_after: Option<WorkId>,
}

/// Body-free memory index. `version` is the authorized on-demand read key.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkMemoryIndexEntry {
    pub memory_id: crate::MemoryId,
    pub version: ObjectHash,
    pub status: crate::MemoryStatus,
    pub kind: crate::MemoryKind,
    pub title: String,
    pub sensitivity: Sensitivity,
    pub created_at: DateTime<Utc>,
}

/// Bounded focus history with an exact source count and newest summaries.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkHistoryView {
    pub total: usize,
    pub items: Vec<WorkChange>,
    pub omitted: usize,
}

/// Full bounded context for the ambient focused item.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkFocusView {
    pub session: AgentWorkSession,
    pub status: ReadyWorkSummary,
    pub run: Option<WorkRunSummary>,
    pub claim: Option<WorkClaim>,
    /// Paste-ready native control binding for this session's live claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_binding: Option<ControlWorkBinding>,
    pub children: Vec<WorkItemSummary>,
    pub prerequisites: Vec<WorkItemSummary>,
    pub handoffs: Vec<WorkHandoffSummary>,
    pub blockers: Vec<WorkBlockerSummary>,
    pub evidence: Vec<ObjectHash>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_items: Vec<WorkEvidenceSummary>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub obligation_items: Vec<WorkObligationSummary>,
    #[serde(default)]
    pub memories: Vec<WorkMemoryIndexEntry>,
    pub history: WorkHistoryView,
    /// Direct disposed required children for which the current host grant can
    /// execute `work_update:waive_required_child` now.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waivable_required_children: Vec<RequiredChildWaiverCandidate>,
    pub allowed_next: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub omissions: Vec<WorkSectionOmission>,
}

/// Compact agent-facing summary of one canonical run evidence object.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkEvidenceSummary {
    pub evidence: ObjectHash,
    pub evidence_kind: WorkEvidenceKind,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub producer_session_id: Option<SessionId>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check_kind: Option<VerificationKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check_fingerprint: Option<ObjectHash>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_result: Option<VerificationResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment_fingerprint: Option<ObjectHash>,
    pub summary: String,
    pub created_at: DateTime<Utc>,
}

/// Bounded agent-facing summary of one immutable run obligation.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkObligationSummary {
    pub obligation_id: crate::WorkObligationId,
    pub definition: ObjectHash,
    pub state: WorkObligationState,
    pub rule: crate::BuiltinObligationRuleRef,
    pub requirement: crate::VerificationRequirement,
    pub triggering_observation: ObjectHash,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<ObjectHash>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<ObjectHash>,
}

/// Bounded actionable input for one required-child completion waiver.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RequiredChildWaiverCandidate {
    pub work_id: WorkId,
    pub short_ref: String,
    pub lifecycle: WorkLifecycle,
}

/// Compact execution-generation state for focus packets.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkRunSummary {
    pub root_execution_id: crate::RootExecutionId,
    pub work_id: WorkId,
    pub run_id: WorkRunId,
    pub generation: i64,
    pub executor: Option<SessionId>,
    pub state: WorkRunState,
    pub revision: i64,
    pub last_checkpoint: Option<ObjectHash>,
    pub completion_seal: Option<ObjectHash>,
}

/// Compact live handoff state for focus packets.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkHandoffSummary {
    pub offer_id: crate::WorkHandoffOfferId,
    pub from: SessionId,
    pub to: SessionId,
    pub state: WorkHandoffState,
    pub expires_at: DateTime<Utc>,
}

/// Compact active blocker identity needed to construct an unblock request.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkBlockerSummary {
    pub blocker_id: String,
    pub kind: WorkBlockerKind,
    pub detail: String,
}

/// Low-ceremony root creation or atomic focused-work decomposition.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkProposeInput {
    Root {
        title: String,
        outcome: String,
        acceptance: Vec<String>,
        work_kind: Option<WorkItemKind>,
        priority: Option<i32>,
        #[serde(default)]
        labels: Vec<String>,
        assigned_to: Option<String>,
        deferred_until: Option<DateTime<Utc>>,
        authority_policy_ref: Option<String>,
        idempotency_key: String,
    },
    Decompose {
        children: Vec<WorkChildInput>,
        #[serde(default)]
        prerequisites: Vec<WorkPrerequisiteInput>,
        idempotency_key: String,
    },
}

/// One child in an atomic `work_propose` decomposition.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkChildInput {
    pub key: String,
    pub title: String,
    pub outcome: String,
    pub acceptance: Vec<String>,
    pub requirement: Option<ChildRequirement>,
    pub kind: Option<WorkItemKind>,
    pub priority: Option<i32>,
    #[serde(default)]
    pub labels: Vec<String>,
    pub assigned_to: Option<String>,
    pub deferred_until: Option<DateTime<Utc>>,
}

/// A child prerequisite whose target is a sibling key or an existing work ref.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkPrerequisiteInput {
    pub work_key: String,
    pub prerequisite: String,
}

/// Result of a root or decomposition proposal.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkProposeResult {
    Root {
        work: WorkItemSummary,
        focus: Box<WorkFocusView>,
    },
    Decomposition(WorkDecompositionSummary),
}

/// Bounded decomposition receipt.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkDecompositionSummary {
    pub parent: WorkItemSummary,
    /// Exact number of children created by the atomic decomposition.
    pub child_count: usize,
    /// Complete fixed-size identity list. Full child details are available by
    /// focusing any returned short reference.
    pub children: Vec<WorkDecompositionChildSummary>,
    #[serde(default)]
    pub details_omitted: bool,
}

/// Stable, response-bounded identity for one newly decomposed child.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkDecompositionChildSummary {
    pub work_id: WorkId,
    pub short_ref: String,
    pub revision: i64,
}

/// Typed update union applied to ambient focused work.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkUpdateInput {
    Claim {
        ttl_seconds: Option<i64>,
        /// Explicit host-authorized reason for recovering an unaccounted prior
        /// claimant. Omit for an ordinary claim.
        recovery_reason: Option<String>,
        idempotency_key: String,
    },
    Release {
        reason: String,
        /// Explicit host-authorized reason for waiving a missing contribution.
        /// Omit when the current holder has already contributed.
        waiver_reason: Option<String>,
        idempotency_key: String,
    },
    Checkpoint {
        summary: String,
        #[serde(default)]
        evidence: Vec<String>,
        idempotency_key: String,
    },
    Evidence {
        #[serde(default)]
        summary: String,
        #[serde(default)]
        refs: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attach: Option<WorkEvidenceAttachInput>,
        idempotency_key: String,
    },
    Block {
        blocker_kind: WorkBlockerKind,
        detail: String,
        idempotency_key: String,
    },
    Unblock {
        /// Omit when exactly one blocker is active on the focused item.
        blocker_id: Option<String>,
        idempotency_key: String,
    },
    Revise {
        patch: WorkRevisionPatch,
        idempotency_key: String,
    },
    AddPrerequisite {
        prerequisite: String,
        idempotency_key: String,
    },
    RemovePrerequisite {
        prerequisite: String,
        idempotency_key: String,
    },
    Reopen {
        reason: String,
        idempotency_key: String,
    },
    Cancel {
        reason: String,
        idempotency_key: String,
    },
    Supersede {
        replacement: String,
        reason: String,
        idempotency_key: String,
    },
    WaiveRequiredChild {
        child: String,
        reason: String,
        idempotency_key: String,
    },
}

/// Attach-only reference to typed evidence already minted by the host-private
/// checkpoint path.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkEvidenceAttachInput {
    pub evidence: String,
}

/// Terse update receipt and the obligations/next actions that matter now.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkUpdateResult {
    pub operation: String,
    pub receipt: WorkMutationReceipt,
    pub obligations: Vec<String>,
    pub allowed_next: Vec<String>,
}

/// Stable compact receipt shared by update and handoff operations.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct WorkMutationReceipt {
    pub work_id: WorkId,
    pub work_ref: String,
    pub revision: i64,
    /// Paste-ready native control binding produced by a successful live claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_binding: Option<ControlWorkBinding>,
    pub result: serde_json::Value,
}

/// One criterion result. `criterion` may be omitted only when work has exactly
/// one acceptance criterion.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkAcceptanceInput {
    pub criterion: Option<String>,
    pub satisfied: bool,
    #[serde(default)]
    pub evidence: Vec<String>,
    pub note: String,
}

/// Evidence-backed completion of ambient focused work.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkCompleteInput {
    /// Optional one-call capture: records this evidence and checkpoints the
    /// exact completion evidence set before attempting the seal.
    pub capture: Option<WorkCompletionCaptureInput>,
    #[serde(default)]
    pub evidence: Vec<String>,
    pub acceptance: Vec<WorkAcceptanceInput>,
    pub idempotency_key: String,
}

/// Evidence captured and checkpointed as part of one high-level completion.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkCompletionCaptureInput {
    pub summary: String,
    #[serde(default)]
    pub refs: Vec<String>,
}

/// Checkpoint-coupled handoff union for ambient focused work.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkHandoffInput {
    Offer {
        to: String,
        ttl_seconds: Option<i64>,
        checkpoint_summary: String,
        idempotency_key: String,
    },
    Accept {
        idempotency_key: String,
    },
    Cancel {
        reason: String,
        idempotency_key: String,
    },
}

/// Compact handoff receipt plus refreshed ambient focus.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkHandoffResult {
    pub operation: String,
    pub receipt: WorkMutationReceipt,
    pub obligations: Vec<String>,
    pub allowed_next: Vec<String>,
}

/// Agent-visible completion outcome. Successful receipts retain their original
/// flat JSON shape; policy refusals are typed success results rather than MCP
/// error envelopes.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(untagged)]
pub enum WorkCompleteResult {
    Completed(WorkCompletedReceipt),
    Refused(WorkCompleteRefusal),
}

/// Successful completion receipt. The canonical seal remains queryable by
/// hash, while host-bound grant references never cross the protocol boundary.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkCompletedReceipt {
    pub seal: ObjectHash,
    pub work_id: WorkId,
    pub run_id: crate::WorkRunId,
    pub completed_at: DateTime<Utc>,
}

/// Bounded policy refusal returned when an exact completion cut remains open.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkCompleteRefusal {
    pub code: String,
    pub work_id: WorkId,
    pub obligations: Vec<crate::OpenWorkObligation>,
    pub omitted_count: usize,
    pub remedy: String,
}

impl LocalWorkService {
    /// Constructs a service whose authority is fixed by its host, never by an
    /// agent request body.
    #[must_use]
    pub fn new(
        database: PathBuf,
        project_id: ProjectId,
        actor_id: String,
        session_id: SessionId,
        source_skill: Option<String>,
        authority_grant: Option<ObjectHash>,
    ) -> Self {
        Self {
            database,
            project_id,
            actor_id,
            session_id,
            source_skill,
            authority_grant,
            #[cfg(test)]
            delivery_stage_hook: None,
        }
    }

    /// Returns current focus, ready candidates, and the next bounded project delta.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when work projections cannot be read or the
    /// ambient cursor cannot be advanced.
    pub fn work_next(
        &self,
        limit: u32,
        query: WorkNextQuery,
        now: DateTime<Utc>,
    ) -> Result<WorkNextView, StoreError> {
        self.work_next_with_delivery_token(limit, None, None, query, now)
    }

    /// Executes `work_next` with the opaque capability returned by a prior
    /// staged page. Callers cannot advance a pending cursor without it.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] under the same conditions as [`Self::work_next`],
    /// or when the acknowledgement capability does not bind the pending page.
    #[allow(
        clippy::too_many_lines,
        reason = "section selection, exact delivery staging, and final byte fitting stay together so cursor advancement is auditable"
    )]
    pub fn work_next_with_delivery_token(
        &self,
        limit: u32,
        acknowledge_through: Option<i64>,
        acknowledge_token: Option<&str>,
        query: WorkNextQuery,
        now: DateTime<Utc>,
    ) -> Result<WorkNextView, StoreError> {
        let mut store = self.store()?;
        if let Some(through) = acknowledge_through {
            store.acknowledge_work_session_delivery(
                &self.project_id,
                &self.session_id,
                through,
                acknowledge_token,
                now,
            )?;
        } else if acknowledge_token.is_some() {
            return Err(StoreError::InvalidWork(
                "work delivery token requires acknowledge_through".into(),
            ));
        }
        let sections = selected_work_next_sections(&query.sections);
        let wants_focus = sections.contains(&WorkNextSection::Focus);
        let wants_ready = sections.contains(&WorkNextSection::Ready);
        let wants_catalog = sections.contains(&WorkNextSection::Catalog);
        let wants_changes = sections.contains(&WorkNextSection::Changes);
        let project_feed = FeedId::Project(self.project_id.clone());
        let initial_session = store.work_session_state(&self.project_id, &self.session_id, now)?;
        let mut omissions = Vec::new();
        let (session, changes, delivered_through) = if wants_changes {
            let mut delivery_session = initial_session;
            let mut stage_retries = 0;
            #[cfg(test)]
            let mut stage_hook_used = false;
            loop {
                if let Some(through) = delivery_session.tentative_project_cursor {
                    let payload = store
                        .staged_work_session_delivery_payload(&self.project_id, &self.session_id)?
                        .ok_or_else(|| {
                            StoreError::InvalidWorkProjection(
                                "pending work delivery has no exact staged payload".into(),
                            )
                        })?;
                    let page: StagedWorkChangePage = payload.decode()?;
                    verify_staged_work_change_page(
                        &store,
                        &project_feed,
                        delivery_session.project_cursor,
                        through,
                        &page,
                    )?;
                    if page.omitted_count > 0 {
                        omissions.push(WorkSectionOmission {
                            section: WorkNextSection::Changes,
                            reason: WorkSectionOmissionReason::ByteBudget,
                            omitted_count: page.omitted_count,
                        });
                    }
                    break (delivery_session, Some(page.changes), through);
                }
                let (focused_root_id, bound_task_id) = work_delivery_boundary(
                    &store,
                    &self.project_id,
                    &self.session_id,
                    delivery_session.focused_work_id,
                )?;
                let entries =
                    store.work_feed_after(&project_feed, delivery_session.project_cursor, limit)?;
                let candidate_count = entries.len();
                let changes = verified_bounded_work_changes(
                    &store,
                    &self.project_id,
                    focused_root_id,
                    bound_task_id,
                    entries,
                    delivery_session.project_cursor,
                    MAX_CHANGE_SECTION_BYTES,
                )?;
                let selected_through = changes
                    .last()
                    .map_or(delivery_session.project_cursor, |change| {
                        change.entry.position.position
                    });
                let omitted_count = candidate_count - changes.len();
                let payload = CanonicalObject::freeze(&StagedWorkChangePage {
                    schema_version: SCHEMA_VERSION,
                    changes: changes.clone(),
                    omitted_count,
                })?;
                let delivered_entries = changes
                    .iter()
                    .map(|change| change.entry.clone())
                    .collect::<Vec<_>>();
                #[cfg(test)]
                if !stage_hook_used && let Some(hook) = &self.delivery_stage_hook {
                    hook.entered.wait();
                    hook.release.wait();
                    stage_hook_used = true;
                }
                let staged = store.stage_work_session_delivery(
                    &self.project_id,
                    &self.session_id,
                    StageWorkSessionDelivery {
                        expected_confirmed_through: delivery_session.project_cursor,
                        expected_focused_work_id: delivery_session.focused_work_id,
                        expected_bound_task_id: bound_task_id,
                        delivered_through: selected_through,
                        delivered_entries: &delivered_entries,
                        delivery_payload: &payload,
                        now,
                    },
                )?;
                if let Some(staged) = staged {
                    if omitted_count > 0 {
                        omissions.push(WorkSectionOmission {
                            section: WorkNextSection::Changes,
                            reason: WorkSectionOmissionReason::ByteBudget,
                            omitted_count,
                        });
                    }
                    break (staged, Some(changes), selected_through);
                }
                stage_retries += 1;
                if stage_retries >= MAX_DELIVERY_STAGE_RETRIES {
                    return Err(StoreError::InvalidWork(
                        "work delivery basis changed repeatedly; retry work_next".into(),
                    ));
                }
                delivery_session =
                    store.work_session_state(&self.project_id, &self.session_id, now)?;
            }
        } else {
            let confirmed = initial_session.project_cursor;
            (initial_session, None, confirmed)
        };
        let focus = if wants_focus {
            session
                .focused_work_id
                .map(|work_id| self.focus_view(&store, work_id, now))
                .transpose()?
        } else {
            None
        };
        let ready = if wants_ready {
            let source = store.ready_work(&self.project_id, now, limit)?;
            let source_count = source.len();
            let bounded = bounded_ready_prefix(
                source.into_iter().map(ready_work_summary).collect(),
                MAX_READY_SECTION_BYTES,
            )?;
            if source_count > bounded.len() {
                omissions.push(WorkSectionOmission {
                    section: WorkNextSection::Ready,
                    reason: WorkSectionOmissionReason::ByteBudget,
                    omitted_count: source_count - bounded.len(),
                });
            }
            Some(bounded)
        } else {
            None
        };
        let after = query
            .after
            .as_deref()
            .map(|work_ref| store.resolve_work_ref(&self.project_id, work_ref))
            .transpose()?
            .map(|work| work.work_id);
        let catalog = if wants_catalog {
            let source = store.query_work_catalog(
                &self.project_id,
                now,
                &WorkCatalogQuery {
                    search: query.search,
                    lifecycles: query.lifecycles,
                    availabilities: query.availabilities,
                    blocked_only: query.blocked_only,
                    assigned_to: query.assigned_to,
                    label: query.label,
                    after,
                    limit,
                },
            )?;
            let source_count = source.items.len();
            let source_next_after = source.next_after;
            let items = bounded_ready_prefix(
                source.items.into_iter().map(ready_work_summary).collect(),
                MAX_CATALOG_SECTION_BYTES,
            )?;
            if source_count > items.len() {
                omissions.push(WorkSectionOmission {
                    section: WorkNextSection::Catalog,
                    reason: WorkSectionOmissionReason::ByteBudget,
                    omitted_count: source_count - items.len(),
                });
            }
            let next_after = if source_count > items.len() {
                items.last().map(|item| item.work.work_id)
            } else {
                source_next_after
            };
            Some(WorkCatalogSummaryPage { items, next_after })
        } else {
            None
        };
        let mut response = WorkNextView {
            session: agent_work_session(&session),
            focus,
            ready,
            catalog,
            changes,
            delivered_through: wants_changes.then_some(delivered_through),
            delivery_token: wants_changes
                .then(|| session.tentative_delivery_token.clone())
                .flatten(),
            omissions,
        };
        fit_work_next_response(&mut response)?;
        ensure_agent_response_budget(&response, "work_next")?;
        Ok(response)
    }

    /// Selects and inspects ambient work without implicitly changing its claim.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the reference is absent or projections are invalid.
    pub fn work_focus(
        &self,
        work_ref: &str,
        now: DateTime<Utc>,
    ) -> Result<WorkFocusView, StoreError> {
        let mut store = self.store()?;
        let item = store.resolve_work_ref(&self.project_id, work_ref)?;
        store.focus_work_session(&self.project_id, &self.session_id, item.work_id, now)?;
        self.focus_view(&store, item.work_id, now)
    }

    /// Creates a root or atomically decomposes ambient focused work.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when host authority is absent/stale, input is
    /// invalid, or the underlying lifecycle transaction refuses admission.
    #[allow(
        clippy::too_many_lines,
        reason = "root and decomposition translations remain together so the six-operation boundary is auditable"
    )]
    pub fn work_propose(
        &self,
        input: WorkProposeInput,
        now: DateTime<Utc>,
    ) -> Result<WorkProposeResult, StoreError> {
        let mut store = self.store()?;
        let basis = self.protocol_basis(
            &store,
            matches!(input, WorkProposeInput::Decompose { .. }),
            false,
            now,
        )?;
        let intent = self.protocol_intent(&input);
        let (protocol_operation, core_operation, raw_key) = propose_metadata(&input);
        let raw_key = raw_key.to_owned();
        let attempt = store.begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
            project_id: &self.project_id,
            session_id: &self.session_id,
            operation: protocol_operation,
            idempotency_key: &raw_key,
            intent: &intent,
            basis: &basis,
            now,
        })?;
        if let Some(result) = attempt.result {
            let replay: WorkProposeResult = serde_json::from_value(result)?;
            ensure_agent_response_budget(&replay, "work_propose")?;
            return Ok(replay);
        }
        let scoped_key = self.core_operation_key(protocol_operation, &raw_key, core_operation)?;
        let core_result = store.work_operation_result_value(core_operation, &scoped_key)?;
        ensure_protocol_basis(
            attempt.basis_matches,
            protocol_operation,
            &raw_key,
            core_result.is_some(),
        )?;
        let grant = self.authority_decision()?;
        let result = match input {
            WorkProposeInput::Root {
                title,
                outcome,
                acceptance,
                work_kind,
                priority,
                labels,
                assigned_to,
                deferred_until,
                authority_policy_ref,
                idempotency_key: _,
            } => {
                if let Some(value) = core_result {
                    let work: WorkItem = serde_json::from_value(value)?;
                    store.focus_work_session(
                        &self.project_id,
                        &self.session_id,
                        work.work_id,
                        now,
                    )?;
                    let focus = self.focus_view(&store, work.work_id, now)?;
                    let result = WorkProposeResult::Root {
                        work: work_item_summary(&work),
                        focus: Box::new(focus),
                    };
                    ensure_agent_response_budget(&result, "work_propose")?;
                    store.finish_work_protocol_attempt(
                        &self.project_id,
                        &self.session_id,
                        protocol_operation,
                        &raw_key,
                        &result,
                    )?;
                    return Ok(result);
                }
                let work = store.create_work(
                    &CreateWorkRequest {
                        project_id: self.project_id.clone(),
                        parent_id: None,
                        child_requirement: ChildRequirement::Required,
                        title,
                        outcome,
                        acceptance,
                        kind: work_kind.unwrap_or(WorkItemKind::Task),
                        priority: priority.unwrap_or(1),
                        labels,
                        assigned_to,
                        deferred_until,
                        origin: WorkOrigin::Local,
                        source_snapshot_id: None,
                        authority_policy_ref: authority_policy_ref
                            .unwrap_or_else(|| "project-default".into()),
                        authority: grant,
                        actor: self.actor("work_propose", "create local root work"),
                        idempotency_key: scoped_key,
                        created_at: now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                store.focus_work_session(&self.project_id, &self.session_id, work.work_id, now)?;
                let focus = self.focus_view(&store, work.work_id, now)?;
                WorkProposeResult::Root {
                    work: work_item_summary(&work),
                    focus: Box::new(focus),
                }
            }
            WorkProposeInput::Decompose {
                children,
                prerequisites,
                idempotency_key: _,
            } => {
                if let Some(value) = core_result {
                    let decomposition: WorkDecomposition = serde_json::from_value(value)?;
                    WorkProposeResult::Decomposition(work_decomposition_summary(&decomposition))
                } else {
                    let parent = basis.focused_work.clone().ok_or_else(|| {
                        StoreError::InvalidWorkProjection(
                            "decomposition attempt has no bound focused work".into(),
                        )
                    })?;
                    let local_keys = children
                        .iter()
                        .map(|child| child.key.trim().to_owned())
                        .collect::<Vec<_>>();
                    let children = children
                        .into_iter()
                        .map(|child| ChildWorkDraft {
                            local_key: child.key,
                            child_requirement: child
                                .requirement
                                .unwrap_or(ChildRequirement::Required),
                            title: child.title,
                            outcome: child.outcome,
                            acceptance: child.acceptance,
                            kind: child.kind.unwrap_or(WorkItemKind::Task),
                            priority: child.priority.unwrap_or(parent.priority),
                            labels: child.labels,
                            assigned_to: child.assigned_to,
                            deferred_until: child.deferred_until,
                        })
                        .collect();
                    let mut resolved = Vec::with_capacity(prerequisites.len());
                    for edge in prerequisites {
                        let prerequisite =
                            if local_keys.iter().any(|key| key == edge.prerequisite.trim()) {
                                WorkDependencyRef::Proposed(edge.prerequisite)
                            } else {
                                WorkDependencyRef::Existing(
                                    store
                                        .resolve_work_ref(&self.project_id, &edge.prerequisite)?
                                        .work_id,
                                )
                            };
                        resolved.push(ChildWorkPrerequisite {
                            work_key: edge.work_key,
                            prerequisite,
                        });
                    }
                    let authority = self.planning_authority(basis.claim.as_ref(), &parent, now)?;
                    let decomposition = store.decompose_work(
                        &DecomposeWorkRequest {
                            parent_id: parent.work_id,
                            expected_parent_revision: parent.revision,
                            children,
                            prerequisites: resolved,
                            authority,
                            actor: self
                                .actor("work_propose", "atomically decompose ambient local work"),
                            idempotency_key: scoped_key,
                            created_at: now,
                        },
                        &DevelopmentNoopRedactor,
                    )?;
                    WorkProposeResult::Decomposition(work_decomposition_summary(&decomposition))
                }
            }
        };
        ensure_agent_response_budget(&result, "work_propose")?;
        store.finish_work_protocol_attempt(
            &self.project_id,
            &self.session_id,
            protocol_operation,
            &raw_key,
            &result,
        )?;
        Ok(result)
    }

    /// Applies one typed update to ambient focused work.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when focus/authority/fences are absent or stale.
    #[allow(
        clippy::too_many_lines,
        reason = "the tagged update union is translated in one exhaustive match so new variants cannot bypass ambient fence inference"
    )]
    pub fn work_update(
        &self,
        input: WorkUpdateInput,
        now: DateTime<Utc>,
    ) -> Result<WorkUpdateResult, StoreError> {
        let mut store = self.store()?;
        let basis = self.protocol_basis(&store, true, false, now)?;
        let intent = self.protocol_intent(&input);
        let (operation, core_operation, raw_key) = update_metadata(&input);
        let raw_key = raw_key.to_owned();
        let protocol_operation = format!("work_update:{operation}");
        let attempt = store.begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
            project_id: &self.project_id,
            session_id: &self.session_id,
            operation: &protocol_operation,
            idempotency_key: &raw_key,
            intent: &intent,
            basis: &basis,
            now,
        })?;
        if let Some(result) = attempt.result {
            return serde_json::from_value(result).map_err(StoreError::from);
        }
        let scoped_key = self.core_operation_key(&protocol_operation, &raw_key, core_operation)?;
        if let Some(receipt) = store.work_operation_result_value(core_operation, &scoped_key)? {
            let durable_basis: WorkProtocolBasis =
                serde_json::from_value(attempt.basis.ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "core-committed update has no durable attempt basis".into(),
                    )
                })?)?;
            let current = durable_basis
                .focused_work
                .map(|work| work.work_id)
                .ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "core-committed update basis has no focused work".into(),
                    )
                })?;
            let result = self.work_update_result(
                &store,
                operation,
                current,
                agent_update_receipt(operation, receipt)?,
                now,
            )?;
            store.finish_work_protocol_attempt(
                &self.project_id,
                &self.session_id,
                &protocol_operation,
                &raw_key,
                &result,
            )?;
            return Ok(result);
        }
        ensure_protocol_basis(attempt.basis_matches, &protocol_operation, &raw_key, false)?;
        let work = basis.focused_work.clone().ok_or_else(|| {
            StoreError::InvalidWorkProjection("update attempt has no bound focused work".into())
        })?;
        let (_, receipt) = match input {
            WorkUpdateInput::Claim {
                ttl_seconds,
                recovery_reason,
                idempotency_key: _,
            } => {
                let run_id = active_run_id(&work)?;
                let claim = store.claim_work(
                    &ClaimWorkRequest {
                        work_id: work.work_id,
                        expected_work_revision: work.revision,
                        expected_run_id: run_id,
                        holder: self.session_id.clone(),
                        ttl_seconds: ttl_seconds.unwrap_or(3_600),
                        authority: self.authority_decision()?,
                        recovery_authority: recovery_reason
                            .as_ref()
                            .map(|_| self.authority_decision())
                            .transpose()?,
                        recovery_reason,
                        actor: self.actor("work_update", "claim ambient local work"),
                        idempotency_key: scoped_key,
                        claimed_at: now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                ("claim", serde_json::to_value(claim)?)
            }
            WorkUpdateInput::Release {
                reason,
                waiver_reason,
                idempotency_key: _,
            } => {
                let claim = self.live_protocol_claim(&basis, &work, now)?;
                let released = store.release_work(
                    &ReleaseWorkRequest {
                        work_id: work.work_id,
                        run_id: claim.run_id,
                        expected_work_revision: work.revision,
                        holder: self.session_id.clone(),
                        claim_id: claim.claim_id,
                        claim_fence: claim.fence,
                        reason,
                        waiver_authority: waiver_reason
                            .as_ref()
                            .map(|_| self.authority_decision())
                            .transpose()?,
                        waiver_reason,
                        actor: self.actor("work_update", "release ambient local work"),
                        idempotency_key: scoped_key,
                        released_at: now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                ("release", serde_json::to_value(released)?)
            }
            WorkUpdateInput::Checkpoint {
                summary,
                evidence,
                idempotency_key: _,
            } => {
                let claim = self.live_protocol_claim(&basis, &work, now)?;
                let checkpoint = store.checkpoint_work(
                    &CheckpointWorkRequest {
                        work_id: work.work_id,
                        run_id: claim.run_id,
                        expected_work_revision: work.revision,
                        holder: self.session_id.clone(),
                        claim_id: claim.claim_id,
                        claim_fence: claim.fence,
                        summary,
                        evidence: parse_hashes(&evidence)?,
                        actor: self.actor("work_update", "checkpoint ambient local work"),
                        idempotency_key: scoped_key,
                        checkpointed_at: now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                ("checkpoint", serde_json::to_value(checkpoint)?)
            }
            WorkUpdateInput::Evidence {
                summary,
                refs,
                attach,
                idempotency_key: _,
            } => {
                let claim = self.live_protocol_claim(&basis, &work, now)?;
                if let Some(attach) = attach {
                    if !summary.trim().is_empty() || !refs.is_empty() {
                        return Err(StoreError::InvalidWork(
                            "typed evidence attach cannot also supply generic summary or refs"
                                .into(),
                        ));
                    }
                    let evidence = parse_hash(&attach.evidence)?;
                    let evidence_kind = store.work_evidence_kind(claim.run_id, &evidence)?;
                    if evidence_kind == WorkEvidenceKind::Generic {
                        return Err(StoreError::InvalidWork(
                            "typed evidence attach requires verification or environment evidence"
                                .into(),
                        ));
                    }
                    (
                        "evidence",
                        serde_json::json!({
                            "attached": true,
                            "evidence": evidence,
                            "evidence_kind": evidence_kind,
                        }),
                    )
                } else {
                    let evidence = store.record_work_evidence(
                        &RecordWorkEvidenceRequest {
                            work_id: work.work_id,
                            run_id: claim.run_id,
                            expected_work_revision: work.revision,
                            holder: self.session_id.clone(),
                            claim_id: claim.claim_id,
                            claim_fence: claim.fence,
                            summary,
                            refs,
                            actor: self.actor("work_update", "record evidence for ambient work"),
                            idempotency_key: scoped_key,
                            recorded_at: now,
                        },
                        &DevelopmentNoopRedactor,
                    )?;
                    ("evidence", serde_json::to_value(evidence)?)
                }
            }
            WorkUpdateInput::Block {
                blocker_kind,
                detail,
                idempotency_key: _,
            } => {
                let authority = self.planning_authority(basis.claim.as_ref(), &work, now)?;
                let blocker = store.add_work_blocker(
                    &AddWorkBlockerRequest {
                        work_id: work.work_id,
                        expected_work_revision: work.revision,
                        kind: blocker_kind,
                        detail,
                        authority,
                        actor: self.actor("work_update", "block ambient local work"),
                        idempotency_key: scoped_key,
                        blocked_at: now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                ("block", serde_json::to_value(blocker)?)
            }
            WorkUpdateInput::Unblock {
                blocker_id,
                idempotency_key: _,
            } => {
                let authority = self.planning_authority(basis.claim.as_ref(), &work, now)?;
                let blocker_id = match blocker_id {
                    Some(blocker_id) if !blocker_id.trim().is_empty() => blocker_id,
                    Some(_) => {
                        return Err(StoreError::InvalidWork(
                            "blocker_id must not be empty; omit it to infer one active blocker"
                                .into(),
                        ));
                    }
                    None => unique_blocker_id(&store.inspect_work(work.work_id, now)?.blockers)?,
                };
                let item = store.clear_work_blocker(
                    &ClearWorkBlockerRequest {
                        work_id: work.work_id,
                        expected_work_revision: work.revision,
                        blocker_id,
                        authority,
                        actor: self.actor("work_update", "clear an ambient work blocker"),
                        idempotency_key: scoped_key,
                        cleared_at: now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                ("unblock", serde_json::to_value(item)?)
            }
            WorkUpdateInput::Revise {
                patch,
                idempotency_key: _,
            } => {
                let authority = self.planning_authority(basis.claim.as_ref(), &work, now)?;
                let item = store.revise_work(
                    &ReviseWorkRequest {
                        work_id: work.work_id,
                        expected_revision: work.revision,
                        patch,
                        authority,
                        actor: self.actor("work_update", "revise ambient local work"),
                        idempotency_key: scoped_key,
                        updated_at: now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                ("revise", serde_json::to_value(item)?)
            }
            WorkUpdateInput::AddPrerequisite {
                prerequisite,
                idempotency_key: _,
            } => {
                let prerequisite = store.resolve_work_ref(&self.project_id, &prerequisite)?;
                let authority = self.planning_authority(basis.claim.as_ref(), &work, now)?;
                let item = store.add_work_prerequisite(
                    &ChangeWorkPrerequisiteRequest {
                        work_id: work.work_id,
                        prerequisite_id: prerequisite.work_id,
                        expected_revision: work.revision,
                        authority,
                        actor: self.actor("work_update", "add an ambient work prerequisite"),
                        idempotency_key: scoped_key,
                        changed_at: now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                ("add_prerequisite", serde_json::to_value(item)?)
            }
            WorkUpdateInput::RemovePrerequisite {
                prerequisite,
                idempotency_key: _,
            } => {
                let prerequisite = store.resolve_work_ref(&self.project_id, &prerequisite)?;
                let authority = self.planning_authority(basis.claim.as_ref(), &work, now)?;
                let item = store.remove_work_prerequisite(
                    &ChangeWorkPrerequisiteRequest {
                        work_id: work.work_id,
                        prerequisite_id: prerequisite.work_id,
                        expected_revision: work.revision,
                        authority,
                        actor: self.actor("work_update", "remove an ambient work prerequisite"),
                        idempotency_key: scoped_key,
                        changed_at: now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                ("remove_prerequisite", serde_json::to_value(item)?)
            }
            WorkUpdateInput::Reopen {
                reason,
                idempotency_key: _,
            } => {
                let item = store.reopen_work(
                    &ReopenWorkRequest {
                        work_id: work.work_id,
                        expected_work_revision: work.revision,
                        reason,
                        authority: self.authority_decision()?,
                        actor: self.actor("work_update", "reopen ambient completed work"),
                        idempotency_key: scoped_key,
                        reopened_at: now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                ("reopen", serde_json::to_value(item)?)
            }
            WorkUpdateInput::Cancel {
                reason,
                idempotency_key: _,
            } => {
                let item = store.dispose_work(
                    &DisposeWorkRequest {
                        work_id: work.work_id,
                        expected_work_revision: work.revision,
                        disposition: WorkDisposition::Cancelled,
                        replacement_id: None,
                        reason,
                        authority: self.authority_decision()?,
                        actor: self.actor("work_update", "cancel ambient local work"),
                        idempotency_key: scoped_key,
                        disposed_at: now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                ("cancel", serde_json::to_value(item)?)
            }
            WorkUpdateInput::Supersede {
                replacement,
                reason,
                idempotency_key: _,
            } => {
                let replacement = store.resolve_work_ref(&self.project_id, &replacement)?;
                let item = store.dispose_work(
                    &DisposeWorkRequest {
                        work_id: work.work_id,
                        expected_work_revision: work.revision,
                        disposition: WorkDisposition::Superseded,
                        replacement_id: Some(replacement.work_id),
                        reason,
                        authority: self.authority_decision()?,
                        actor: self.actor("work_update", "supersede ambient local work"),
                        idempotency_key: scoped_key,
                        disposed_at: now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                ("supersede", serde_json::to_value(item)?)
            }
            WorkUpdateInput::WaiveRequiredChild {
                child,
                reason,
                idempotency_key: _,
            } => {
                let child = store.resolve_work_ref(&self.project_id, &child)?;
                let waiver = store.waive_required_child(
                    &WaiveRequiredChildRequest {
                        parent_id: work.work_id,
                        child_id: child.work_id,
                        expected_parent_revision: work.revision,
                        reason,
                        authority: self.authority_decision()?,
                        actor: self.actor(
                            "work_update",
                            "waive a cancelled required child from the completion barrier",
                        ),
                        idempotency_key: scoped_key,
                        waived_at: now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                ("waive_required_child", serde_json::to_value(waiver)?)
            }
        };
        let receipt = agent_update_receipt(operation, receipt)?;
        let result = self.work_update_result(&store, operation, work.work_id, receipt, now)?;
        store.finish_work_protocol_attempt(
            &self.project_id,
            &self.session_id,
            &protocol_operation,
            &raw_key,
            &result,
        )?;
        Ok(result)
    }

    fn work_update_result(
        &self,
        store: &SqliteStore,
        operation: &str,
        work_id: WorkId,
        receipt: serde_json::Value,
        now: DateTime<Utc>,
    ) -> Result<WorkUpdateResult, StoreError> {
        let guidance = self.work_guidance(store, work_id, now)?;
        let control_binding = if operation == "claim" {
            guidance
                .claim
                .as_ref()
                .map(|claim| store.get_work_run(claim.run_id))
                .transpose()?
                .as_ref()
                .and_then(|run| {
                    owned_control_work_binding(
                        &guidance.status.work,
                        run,
                        guidance.claim.as_ref(),
                        &self.session_id,
                        now,
                    )
                })
        } else {
            None
        };
        let result = WorkUpdateResult {
            operation: operation.to_owned(),
            receipt: compact_mutation_receipt(&guidance.status.work, control_binding, receipt),
            obligations: compact_obligations(&guidance.status),
            allowed_next: guidance.allowed_next,
        };
        ensure_agent_response_budget(&result, "work_update")?;
        Ok(result)
    }

    /// Completes ambient focused work under inferred run/claim/fence state.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when evidence/acceptance is incomplete, authority
    /// is absent, or any current lifecycle fence changed.
    pub fn work_complete(
        &self,
        input: WorkCompleteInput,
        now: DateTime<Utc>,
    ) -> Result<WorkCompleteResult, StoreError> {
        let mut store = self.store()?;
        let basis = self.protocol_basis(&store, true, false, now)?;
        let intent = self.protocol_intent(&input);
        let raw_key = input.idempotency_key.clone();
        let attempt = store.begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
            project_id: &self.project_id,
            session_id: &self.session_id,
            operation: "work_complete",
            idempotency_key: &raw_key,
            intent: &intent,
            basis: &basis,
            now,
        })?;
        if let Some(result) = attempt.result {
            return serde_json::from_value(result).map_err(StoreError::from);
        }
        let scoped_key = self.core_operation_key("work_complete", &raw_key, "complete_work")?;
        if let Some(value) = store.work_operation_result_value("complete_work", &scoped_key)? {
            let seal: CompletionSeal = serde_json::from_value(value)?;
            let result = completion_result(&seal)?;
            store.finish_work_protocol_attempt(
                &self.project_id,
                &self.session_id,
                "work_complete",
                &raw_key,
                &result,
            )?;
            return Ok(result);
        }
        ensure_protocol_basis(attempt.basis_matches, "work_complete", &raw_key, false)?;
        let WorkCompleteInput {
            capture,
            evidence: supplied_evidence,
            acceptance: supplied_acceptance,
            idempotency_key: _,
        } = input;
        let work = basis.focused_work.clone().ok_or_else(|| {
            StoreError::InvalidWorkProjection("completion attempt has no bound focused work".into())
        })?;
        let claim = self.live_protocol_claim(&basis, &work, now)?;
        let actor = self.actor("work_complete", "complete ambient local work");
        let evidence_basis = Self::completion_evidence_basis(&store, &claim, &supplied_evidence)?;
        let acceptance = Self::prevalidate_completion_acceptance(
            &work,
            &supplied_acceptance,
            &evidence_basis,
            actor.assurance,
        )?;
        let evidence = self.prepare_completion_evidence(
            &mut store,
            CompletionEvidencePlan {
                work: &work,
                claim: &claim,
                capture: capture.as_ref(),
                evidence: evidence_basis,
                raw_key: &raw_key,
                now,
            },
        )?;
        let acceptance = bind_completion_acceptance_evidence(acceptance, &evidence);
        let decision = self.authority_decision()?;
        let completion = store.complete_work(
            &CompleteWorkRequest {
                work_id: work.work_id,
                run_id: claim.run_id,
                holder: self.session_id.clone(),
                expected_work_revision: work.revision,
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                evidence,
                acceptance,
                drain: CompletionDrainAttestation {
                    reconciled_action_outcomes: Vec::new(),
                    released_resource_leases: Vec::new(),
                    decision: decision.clone(),
                },
                root_authority: (work.work_id == work.root_id).then_some(decision),
                actor,
                idempotency_key: scoped_key,
                completed_at: now,
            },
            &DevelopmentNoopRedactor,
        );
        let result = completion_protocol_result(completion)?;
        store.finish_work_protocol_attempt(
            &self.project_id,
            &self.session_id,
            "work_complete",
            &raw_key,
            &result,
        )?;
        Ok(result)
    }

    fn prepare_completion_evidence(
        &self,
        store: &mut SqliteStore,
        plan: CompletionEvidencePlan<'_>,
    ) -> Result<Vec<ObjectHash>, StoreError> {
        let CompletionEvidencePlan {
            work,
            claim,
            capture,
            mut evidence,
            raw_key,
            now,
        } = plan;
        let Some(capture) = capture else {
            return Ok(evidence);
        };
        let evidence_key =
            self.core_operation_key("work_complete", raw_key, "record_work_evidence")?;
        let recorded_at = store
            .work_operation_result_object::<WorkEvidence>(
                "record_work_evidence",
                &evidence_key,
                "work_evidence",
            )?
            .map_or(now, |committed| committed.created_at);
        let captured = store.record_work_evidence(
            &RecordWorkEvidenceRequest {
                work_id: work.work_id,
                run_id: claim.run_id,
                expected_work_revision: work.revision,
                holder: self.session_id.clone(),
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                summary: capture.summary.clone(),
                refs: capture.refs.clone(),
                actor: self.actor(
                    "work_complete",
                    "capture completion evidence for ambient local work",
                ),
                idempotency_key: evidence_key,
                recorded_at,
            },
            &DevelopmentNoopRedactor,
        )?;
        evidence.push(captured);
        evidence.sort();
        evidence.dedup();
        let checkpoint_key =
            self.core_operation_key("work_complete", raw_key, "checkpoint_work")?;
        let checkpointed_at = store
            .work_operation_result_object::<WorkCheckpoint>(
                "checkpoint_work",
                &checkpoint_key,
                "work_checkpoint",
            )?
            .map_or(now, |committed| committed.created_at);
        store.checkpoint_work(
            &CheckpointWorkRequest {
                work_id: work.work_id,
                run_id: claim.run_id,
                expected_work_revision: work.revision,
                holder: self.session_id.clone(),
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                summary: capture.summary.clone(),
                evidence: evidence.clone(),
                actor: self.actor(
                    "work_complete",
                    "checkpoint the exact completion evidence cut",
                ),
                idempotency_key: checkpoint_key,
                checkpointed_at,
            },
            &DevelopmentNoopRedactor,
        )?;
        Ok(evidence)
    }

    fn completion_evidence_basis(
        store: &SqliteStore,
        claim: &WorkClaim,
        supplied: &[String],
    ) -> Result<Vec<ObjectHash>, StoreError> {
        let available = store.work_run_evidence(claim.run_id)?;
        let mut requested = parse_hashes(supplied)?;
        if requested.is_empty() {
            return Ok(available);
        }
        let available = available.iter().collect::<std::collections::HashSet<_>>();
        if let Some(hash) = requested.iter().find(|hash| !available.contains(hash)) {
            return Err(StoreError::InvalidWork(format!(
                "evidence object {hash} does not belong to the focused run"
            )));
        }
        requested.sort();
        requested.dedup();
        Ok(requested)
    }

    fn prevalidate_completion_acceptance(
        work: &WorkItem,
        supplied: &[WorkAcceptanceInput],
        evidence_basis: &[ObjectHash],
        assurance: AssuranceLevel,
    ) -> Result<Vec<AcceptanceResult>, StoreError> {
        let translated = supplied
            .iter()
            .map(|result| {
                let criterion = match result.criterion.as_deref() {
                    Some(value) => value.trim().to_owned(),
                    None if work.acceptance.len() == 1 => work.acceptance[0].clone(),
                    None => {
                        return Err(StoreError::InvalidWork(
                            "criterion is required when work has multiple acceptance criteria"
                                .into(),
                        ));
                    }
                };
                Ok(AcceptanceResult {
                    criterion,
                    satisfied: result.satisfied,
                    evidence: parse_hashes(&result.evidence)?,
                    assurance,
                    note: result.note.clone(),
                })
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        let normalized = normalize_completion_acceptance_shape(work, &translated, assurance)?;
        let evidence_basis = evidence_basis
            .iter()
            .collect::<std::collections::HashSet<_>>();
        for result in &normalized {
            if let Some(hash) = result
                .evidence
                .iter()
                .find(|hash| !evidence_basis.contains(hash))
            {
                return Err(StoreError::WorkCompletionRefused {
                    work: work.work_id,
                    reason: format!(
                        "acceptance criterion {:?} cites evidence {hash} outside the requested completion basis",
                        result.criterion
                    ),
                });
            }
        }
        Ok(normalized)
    }

    /// Offers, accepts, or cancels a checkpoint-coupled ambient handoff.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when no unique matching offer exists or claim
    /// fences changed.
    pub fn work_handoff(
        &self,
        input: WorkHandoffInput,
        now: DateTime<Utc>,
    ) -> Result<WorkHandoffResult, StoreError> {
        let mut store = self.store()?;
        let basis = self.protocol_basis(&store, true, true, now)?;
        let intent = self.protocol_intent(&input);
        let (operation, core_operation, raw_key) = handoff_metadata(&input);
        let raw_key = raw_key.to_owned();
        let protocol_operation = format!("work_handoff:{operation}");
        let attempt = store.begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
            project_id: &self.project_id,
            session_id: &self.session_id,
            operation: &protocol_operation,
            idempotency_key: &raw_key,
            intent: &intent,
            basis: &basis,
            now,
        })?;
        if let Some(result) = attempt.result {
            return serde_json::from_value(result).map_err(StoreError::from);
        }
        let scoped_key = self.core_operation_key(&protocol_operation, &raw_key, core_operation)?;
        if let Some(receipt) = store.work_operation_result_value(core_operation, &scoped_key)? {
            let durable_basis: WorkProtocolBasis =
                serde_json::from_value(attempt.basis.ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "core-committed handoff has no durable attempt basis".into(),
                    )
                })?)?;
            let current = durable_basis
                .focused_work
                .map(|work| work.work_id)
                .ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "core-committed handoff basis has no focused work".into(),
                    )
                })?;
            let result = self.work_handoff_result(&store, operation, current, receipt, now)?;
            store.finish_work_protocol_attempt(
                &self.project_id,
                &self.session_id,
                &protocol_operation,
                &raw_key,
                &result,
            )?;
            return Ok(result);
        }
        ensure_protocol_basis(attempt.basis_matches, &protocol_operation, &raw_key, false)?;
        let work = basis.focused_work.clone().ok_or_else(|| {
            StoreError::InvalidWorkProjection("handoff attempt has no bound focused work".into())
        })?;
        let receipt =
            self.execute_work_handoff(&mut store, &basis, &work, input, scoped_key, now)?;
        let result = self.work_handoff_result(&store, operation, work.work_id, receipt, now)?;
        store.finish_work_protocol_attempt(
            &self.project_id,
            &self.session_id,
            &protocol_operation,
            &raw_key,
            &result,
        )?;
        Ok(result)
    }

    fn work_handoff_result(
        &self,
        store: &SqliteStore,
        operation: &str,
        work_id: WorkId,
        receipt: serde_json::Value,
        now: DateTime<Utc>,
    ) -> Result<WorkHandoffResult, StoreError> {
        let guidance = self.work_guidance(store, work_id, now)?;
        let result = WorkHandoffResult {
            operation: operation.to_owned(),
            receipt: compact_mutation_receipt(&guidance.status.work, None, receipt),
            obligations: compact_obligations(&guidance.status),
            allowed_next: guidance.allowed_next,
        };
        ensure_agent_response_budget(&result, "work_handoff")?;
        Ok(result)
    }

    fn execute_work_handoff(
        &self,
        store: &mut SqliteStore,
        basis: &WorkProtocolBasis,
        work: &WorkItem,
        input: WorkHandoffInput,
        scoped_key: String,
        now: DateTime<Utc>,
    ) -> Result<serde_json::Value, StoreError> {
        match input {
            WorkHandoffInput::Offer {
                to,
                ttl_seconds,
                checkpoint_summary,
                idempotency_key: _,
            } => {
                let claim = self.live_protocol_claim(basis, work, now)?;
                let offer = store.offer_work_handoff(
                    &OfferWorkHandoffRequest {
                        work_id: work.work_id,
                        run_id: claim.run_id,
                        expected_work_revision: work.revision,
                        from: self.session_id.clone(),
                        to: SessionId(to),
                        claim_id: claim.claim_id,
                        claim_fence: claim.fence,
                        ttl_seconds: ttl_seconds.unwrap_or(3_600),
                        checkpoint_summary,
                        actor: self.actor("work_handoff", "offer ambient work handoff"),
                        idempotency_key: scoped_key,
                        offered_at: now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                serde_json::to_value(offer).map_err(StoreError::from)
            }
            WorkHandoffInput::Accept { idempotency_key: _ } => {
                let offer = unique_offer(
                    basis
                        .handoffs
                        .iter()
                        .filter(|offer| {
                            offer.state == WorkHandoffState::Offered
                                && offer.to == self.session_id
                                && offer.expires_at > now
                        })
                        .cloned(),
                    "incoming",
                )?;
                let claim = store.accept_work_handoff(
                    &AcceptWorkHandoffRequest {
                        work_id: work.work_id,
                        offer_id: offer.offer_id,
                        to: self.session_id.clone(),
                        authority: self.authority_decision()?,
                        actor: self.actor("work_handoff", "accept ambient work handoff"),
                        idempotency_key: scoped_key,
                        accepted_at: now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                serde_json::to_value(claim).map_err(StoreError::from)
            }
            WorkHandoffInput::Cancel {
                reason,
                idempotency_key: _,
            } => {
                let claim = self.live_protocol_claim(basis, work, now)?;
                let offer = unique_offer(
                    basis
                        .handoffs
                        .iter()
                        .filter(|offer| {
                            offer.state == WorkHandoffState::Offered
                                && offer.from == self.session_id
                                && offer.expires_at > now
                        })
                        .cloned(),
                    "outgoing",
                )?;
                let offer = store.cancel_work_handoff(
                    &CancelWorkHandoffRequest {
                        work_id: work.work_id,
                        run_id: claim.run_id,
                        expected_work_revision: work.revision,
                        holder: self.session_id.clone(),
                        offer_id: offer.offer_id,
                        claim_id: claim.claim_id,
                        claim_fence: claim.fence,
                        reason,
                        actor: self.actor("work_handoff", "cancel ambient work handoff"),
                        idempotency_key: scoped_key,
                        cancelled_at: now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                serde_json::to_value(offer).map_err(StoreError::from)
            }
        }
    }

    fn store(&self) -> Result<SqliteStore, StoreError> {
        SqliteStore::open(&self.database)
    }

    fn protocol_intent<'a, T>(&'a self, input: &'a T) -> WorkProtocolIntent<'a, T> {
        WorkProtocolIntent {
            project_id: &self.project_id,
            session_id: &self.session_id,
            actor_id: &self.actor_id,
            source_skill: self.source_skill.as_deref(),
            authority_grant: self.authority_grant.as_ref(),
            input,
        }
    }

    fn protocol_basis(
        &self,
        store: &SqliteStore,
        bind_focus: bool,
        include_handoffs: bool,
        now: DateTime<Utc>,
    ) -> Result<WorkProtocolBasis, StoreError> {
        if !bind_focus {
            return Ok(WorkProtocolBasis {
                focused_work: None,
                claim: None,
                handoffs: Vec::new(),
            });
        }
        let work = self.focused_item(store, now)?;
        Ok(WorkProtocolBasis {
            claim: store.current_work_claim(work.work_id)?,
            handoffs: if include_handoffs {
                store.work_handoff_offers(work.work_id)?
            } else {
                Vec::new()
            },
            focused_work: Some(work),
        })
    }

    fn core_operation_key(
        &self,
        protocol_operation: &str,
        caller_key: &str,
        core_operation: &str,
    ) -> Result<String, StoreError> {
        let object = CanonicalObject::freeze(&WorkCoreOperationKey {
            project_id: &self.project_id,
            session_id: &self.session_id,
            protocol_operation,
            caller_key,
            core_operation,
        })?;
        Ok(format!("work:{}", object.hash().as_str()))
    }

    fn actor(&self, tool_name: &str, reason: &str) -> ActorContext {
        ActorContext {
            actor_id: self.actor_id.clone(),
            actor_kind: "agent".into(),
            assurance: AssuranceLevel::Asserted,
            run_id: None,
            session_id: Some(self.session_id.clone()),
            source_tool: Some(tool_name.into()),
            source_skill: self.source_skill.clone(),
            provenance_chain: vec![ProvenanceLink {
                relation: ProvenanceRelation::AssertedBy,
                source: self.actor_id.clone(),
                reference: Some(self.session_id.0.clone()),
            }],
            reason: reason.into(),
        }
    }

    fn authority_decision(&self) -> Result<LifecycleAuthorityDecision, StoreError> {
        self.authority_grant
            .clone()
            .map(|grant| LifecycleAuthorityDecision { grant })
            .ok_or_else(|| {
                StoreError::InvalidWork(
                    "the host did not bind a work-authority grant to this service".into(),
                )
            })
    }

    fn focused_item(
        &self,
        store: &SqliteStore,
        now: DateTime<Utc>,
    ) -> Result<WorkItem, StoreError> {
        let state = store.work_session_state(&self.project_id, &self.session_id, now)?;
        state
            .focused_work_id
            .map(|work_id| store.get_work_item(work_id))
            .transpose()?
            .ok_or_else(|| {
                StoreError::InvalidWork(
                    "this session has no focused work; call work_focus first".into(),
                )
            })
    }

    fn live_protocol_claim(
        &self,
        basis: &WorkProtocolBasis,
        work: &WorkItem,
        now: DateTime<Utc>,
    ) -> Result<WorkClaim, StoreError> {
        let claim = basis
            .claim
            .clone()
            .ok_or(StoreError::WorkClaimMismatch { work: work.work_id })?;
        if claim.work_id != work.work_id
            || claim.state != WorkClaimState::Active
            || claim.holder != self.session_id
            || claim.expires_at <= now
        {
            return Err(StoreError::WorkClaimMismatch { work: work.work_id });
        }
        Ok(claim)
    }

    fn planning_authority(
        &self,
        claim: Option<&WorkClaim>,
        work: &WorkItem,
        now: DateTime<Utc>,
    ) -> Result<WorkPlanningAuthority, StoreError> {
        let grant = self.authority_decision()?.grant;
        if let Some(claim) = claim
            && claim.work_id == work.work_id
            && claim.state == WorkClaimState::Active
            && claim.holder == self.session_id
            && claim.expires_at > now
        {
            return Ok(WorkPlanningAuthority::Claim {
                run_id: claim.run_id,
                holder: claim.holder.clone(),
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                grant,
            });
        }
        Ok(WorkPlanningAuthority::Delegated { grant })
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the bounded focus packet is assembled in one place so every relation and omission limit is visible"
    )]
    fn focus_view(
        &self,
        store: &SqliteStore,
        work_id: WorkId,
        now: DateTime<Utc>,
    ) -> Result<WorkFocusView, StoreError> {
        let session = store.work_session_state(&self.project_id, &self.session_id, now)?;
        let WorkGuidance {
            status,
            allowed_next,
            waivable_required_children,
            claim,
            handoffs,
        } = self.work_guidance(store, work_id, now)?;
        let run = if let Some(run_id) = status.work.active_run_id {
            Some(store.get_work_run(run_id)?)
        } else {
            store.latest_work_run(work_id)?
        };
        let mut evidence = run
            .as_ref()
            .map(|run| store.work_run_evidence(run.run_id))
            .transpose()?
            .unwrap_or_default();
        let evidence_total = evidence.len();
        if evidence.len() > MAX_FOCUS_RELATIONS {
            evidence = evidence.split_off(evidence.len() - MAX_FOCUS_RELATIONS);
        }
        let evidence_items = run
            .as_ref()
            .map(|run| {
                evidence
                    .iter()
                    .map(|hash| work_evidence_summary(store, run.run_id, hash))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default();
        let mut obligation_records = run
            .as_ref()
            .map(|run| store.work_run_obligations(run.run_id))
            .transpose()?
            .unwrap_or_default();
        let obligation_total = obligation_records.len();
        if obligation_records.len() > MAX_FOCUS_RELATIONS {
            obligation_records =
                obligation_records.split_off(obligation_records.len() - MAX_FOCUS_RELATIONS);
        }
        let obligation_items = obligation_records
            .iter()
            .map(work_obligation_summary)
            .collect();
        let history_total = store.work_event_count(work_id)?;
        let mut history = Vec::new();
        for entry in store.work_event_tail(work_id, MAX_FOCUS_HISTORY)? {
            let event = store
                .get::<crate::WorkEvent>(&entry.object_hash)?
                .ok_or_else(|| {
                    StoreError::InvalidWorkProjection(format!(
                        "root-work feed object {} is missing",
                        entry.object_hash
                    ))
                })?;
            if event.work_id != work_id {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "targeted history returned event for {} while loading {}",
                    event.work_id.0, work_id.0
                )));
            }
            history.push(WorkChange {
                entry,
                delivery: WorkChangeProjection::Visible(agent_work_event_summary(&event)),
            });
        }
        let children = store.work_children(work_id)?;
        let prerequisites = store.work_prerequisites(work_id)?;
        let memories = store.search_work_memories(
            &self.project_id,
            work_id,
            &self.session_id,
            &self.actor_id,
            None,
            Some(MAX_FOCUS_MEMORIES + 1),
        )?;
        let mut omissions = Vec::new();
        let blockers = status.blockers.clone();
        if children.len() > MAX_FOCUS_RELATIONS {
            omissions.push(count_omission(
                WorkNextSection::Focus,
                children.len() - MAX_FOCUS_RELATIONS,
            ));
        }
        if prerequisites.len() > MAX_FOCUS_RELATIONS {
            omissions.push(count_omission(
                WorkNextSection::Focus,
                prerequisites.len() - MAX_FOCUS_RELATIONS,
            ));
        }
        if handoffs.len() > MAX_FOCUS_RELATIONS {
            omissions.push(count_omission(
                WorkNextSection::Focus,
                handoffs.len() - MAX_FOCUS_RELATIONS,
            ));
        }
        if blockers.len() > MAX_FOCUS_RELATIONS {
            omissions.push(count_omission(
                WorkNextSection::Focus,
                blockers.len() - MAX_FOCUS_RELATIONS,
            ));
        }
        if evidence_total > evidence.len() {
            omissions.push(count_omission(
                WorkNextSection::Focus,
                evidence_total - evidence.len(),
            ));
        }
        if obligation_total > obligation_records.len() {
            omissions.push(count_omission(
                WorkNextSection::Focus,
                obligation_total - obligation_records.len(),
            ));
        }
        if memories.len() > usize::try_from(MAX_FOCUS_MEMORIES).unwrap_or(usize::MAX) {
            omissions.push(count_omission(
                WorkNextSection::Focus,
                memories.len() - usize::try_from(MAX_FOCUS_MEMORIES).unwrap_or(usize::MAX),
            ));
        }
        let control_binding = run.as_ref().and_then(|run| {
            owned_control_work_binding(&status.work, run, claim.as_ref(), &self.session_id, now)
        });
        let mut view = WorkFocusView {
            session: agent_work_session(&session),
            status: ready_work_summary(status),
            run: run.as_ref().map(work_run_summary),
            claim,
            control_binding,
            children: children
                .into_iter()
                .take(MAX_FOCUS_RELATIONS)
                .map(|work| work_item_summary(&work))
                .collect(),
            prerequisites: prerequisites
                .into_iter()
                .take(MAX_FOCUS_RELATIONS)
                .map(|work| work_item_summary(&work))
                .collect(),
            handoffs: handoffs
                .iter()
                .take(MAX_FOCUS_RELATIONS)
                .map(work_handoff_summary)
                .collect(),
            blockers: blockers
                .into_iter()
                .take(MAX_FOCUS_RELATIONS)
                .map(|blocker| WorkBlockerSummary {
                    blocker_id: blocker.blocker_id,
                    kind: blocker.kind,
                    detail: compact_text(&blocker.detail),
                })
                .collect(),
            evidence,
            evidence_items,
            obligation_items,
            memories: memories
                .into_iter()
                .take(usize::try_from(MAX_FOCUS_MEMORIES).unwrap_or(usize::MAX))
                .map(work_memory_index)
                .collect(),
            history: WorkHistoryView {
                total: history_total,
                omitted: history_total.saturating_sub(history.len()),
                items: history,
            },
            waivable_required_children,
            allowed_next,
            omissions,
        };
        fit_focus_response(&mut view)?;
        ensure_agent_response_budget(&view, "work_focus")?;
        Ok(view)
    }

    fn work_guidance(
        &self,
        store: &SqliteStore,
        work_id: WorkId,
        now: DateTime<Utc>,
    ) -> Result<WorkGuidance, StoreError> {
        let status = store.inspect_work(work_id, now)?;
        let claim = store.current_work_claim(work_id)?;
        let handoffs = store.work_handoff_offers(work_id)?;
        let actor = self.actor("work_focus", "inspect ambient local work");
        let (authority_operations, waivable_required_children) =
            if let Some(grant) = self.authority_grant.as_ref() {
                let decision = LifecycleAuthorityDecision {
                    grant: grant.clone(),
                };
                let operations =
                    store.allowed_work_authority_operations(&decision, &actor, &status.work, now);
                let candidates = store
                    .waivable_required_children(
                        &decision,
                        &actor,
                        &status.work,
                        now,
                        MAX_FOCUS_RELATIONS,
                    )?
                    .into_iter()
                    .map(required_child_waiver_candidate)
                    .collect();
                (operations, candidates)
            } else {
                (Vec::new(), Vec::new())
            };
        let (completion_capture_ready, completion_preflight_ready) =
            store.work_completion_readiness(work_id, &self.session_id, now)?;
        let claim_recovery_required =
            store.work_claim_recovery_required(work_id, &self.session_id)?;
        let next = allowed_next(
            &status,
            AllowedNextContext {
                claim: claim.as_ref(),
                handoffs: &handoffs,
                session: &self.session_id,
                now,
                authority_operations: &authority_operations,
                can_waive_required_child: !waivable_required_children.is_empty(),
                claim_recovery_required,
                completion_capture_ready,
                completion_preflight_ready,
            },
        );
        Ok(WorkGuidance {
            status,
            allowed_next: next,
            waivable_required_children,
            claim,
            handoffs,
        })
    }
}

fn selected_work_next_sections(requested: &[WorkNextSection]) -> Vec<WorkNextSection> {
    let mut sections = if requested.is_empty() {
        vec![
            WorkNextSection::Focus,
            WorkNextSection::Ready,
            WorkNextSection::Catalog,
            WorkNextSection::Changes,
        ]
    } else {
        requested.to_vec()
    };
    sections.sort_by_key(|section| match section {
        WorkNextSection::Focus => 0,
        WorkNextSection::Ready => 1,
        WorkNextSection::Catalog => 2,
        WorkNextSection::Changes => 3,
    });
    sections.dedup();
    sections
}

fn agent_work_session(state: &WorkSessionState) -> AgentWorkSession {
    AgentWorkSession {
        project_id: state.project_id.clone(),
        session_id: state.session_id.clone(),
        focused_work_id: state.focused_work_id,
        confirmed_project_cursor: state.project_cursor,
        pending_delivery: state.tentative_project_cursor.is_some(),
        updated_at: state.updated_at,
    }
}

fn compact_text(value: &str) -> String {
    compact_text_to(value, MAX_SUMMARY_BYTES)
}

fn compact_text_to(value: &str, max_bytes: usize) -> String {
    let value = value.trim();
    if value.len() <= max_bytes {
        return value.to_owned();
    }
    let mut end = max_bytes.saturating_sub(3).min(value.len());
    while !value.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    format!("{}...", &value[..end])
}

fn work_item_summary(work: &WorkItem) -> WorkItemSummary {
    WorkItemSummary {
        work_id: work.work_id,
        short_ref: work.short_ref.clone(),
        root_id: work.root_id,
        parent_id: work.parent_id,
        title: compact_text(&work.title),
        outcome: compact_text(&work.outcome),
        acceptance: work
            .acceptance
            .iter()
            .take(MAX_ACCEPTANCE_ITEMS)
            .map(|criterion| compact_text(criterion))
            .collect(),
        acceptance_count: work.acceptance.len(),
        kind: work.kind,
        priority: work.priority,
        labels: work
            .labels
            .iter()
            .take(MAX_LABEL_ITEMS)
            .map(|label| compact_text(label))
            .collect(),
        assigned_to: work.assigned_to.as_deref().map(compact_text),
        lifecycle: work.lifecycle,
        revision: work.revision,
        active_run_id: work.active_run_id,
        superseded_by: work.superseded_by,
        updated_at: work.updated_at,
    }
}

fn ready_work_summary(status: ReadyWork) -> ReadyWorkSummary {
    let blocker_count = status.blockers.len();
    ReadyWorkSummary {
        work: work_item_summary(&status.work),
        availability: status.availability,
        reason_codes: status.reason_codes,
        why: status
            .why
            .into_iter()
            .take(MAX_FOCUS_RELATIONS)
            .map(|reason| compact_text(&reason))
            .collect(),
        blocked_by: status
            .blocked_by
            .into_iter()
            .take(MAX_FOCUS_RELATIONS)
            .collect(),
        blocker_count,
    }
}

fn work_run_summary(run: &WorkRun) -> WorkRunSummary {
    WorkRunSummary {
        root_execution_id: run.root_execution_id,
        work_id: run.work_id,
        run_id: run.run_id,
        generation: run.generation,
        executor: run.executor.clone(),
        state: run.state,
        revision: run.revision,
        last_checkpoint: run.last_checkpoint.clone(),
        completion_seal: run.completion_seal.clone(),
    }
}

fn owned_control_work_binding(
    work: &WorkItem,
    run: &WorkRun,
    claim: Option<&WorkClaim>,
    session_id: &SessionId,
    now: DateTime<Utc>,
) -> Option<ControlWorkBinding> {
    let claim = claim?;
    (work.lifecycle == WorkLifecycle::Open
        && work.active_run_id == Some(run.run_id)
        && run.work_id == work.work_id
        && matches!(run.state, WorkRunState::Claimed | WorkRunState::Active)
        && claim.work_id == work.work_id
        && claim.run_id == run.run_id
        && claim.accepted_work_revision == work.revision
        && claim.holder == *session_id
        && claim.state == WorkClaimState::Active
        && claim.expires_at > now)
        .then_some(ControlWorkBinding {
            root_execution_id: run.root_execution_id,
            work_id: work.work_id,
            run_id: run.run_id,
            work_revision: work.revision,
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
        })
}

fn work_handoff_summary(offer: &WorkHandoffOffer) -> WorkHandoffSummary {
    WorkHandoffSummary {
        offer_id: offer.offer_id,
        from: offer.from.clone(),
        to: offer.to.clone(),
        state: offer.state,
        expires_at: offer.expires_at,
    }
}

fn required_child_waiver_candidate(work: WorkItem) -> RequiredChildWaiverCandidate {
    RequiredChildWaiverCandidate {
        work_id: work.work_id,
        short_ref: work.short_ref,
        lifecycle: work.lifecycle,
    }
}

fn work_memory_index(memory: MemorySummary) -> WorkMemoryIndexEntry {
    WorkMemoryIndexEntry {
        memory_id: memory.memory_id,
        version: memory.version,
        status: memory.status,
        kind: memory.kind,
        title: compact_text(&memory.title),
        sensitivity: memory.sensitivity,
        created_at: memory.created_at,
    }
}

fn work_decomposition_summary(decomposition: &WorkDecomposition) -> WorkDecompositionSummary {
    let mut parent = work_item_summary(&decomposition.parent);
    minimize_work_item_summary(&mut parent);
    WorkDecompositionSummary {
        parent,
        child_count: decomposition.children.len(),
        children: decomposition
            .children
            .iter()
            .map(|child| WorkDecompositionChildSummary {
                work_id: child.work_id,
                short_ref: child.short_ref.clone(),
                revision: child.revision,
            })
            .collect(),
        details_omitted: true,
    }
}

fn minimize_work_item_summary(work: &mut WorkItemSummary) {
    work.title = compact_text_to(&work.title, 64);
    work.outcome.clear();
    work.acceptance.clear();
    work.labels.clear();
}

fn compact_obligations(status: &ReadyWork) -> Vec<String> {
    status
        .why
        .iter()
        .take(MAX_FOCUS_RELATIONS)
        .map(|reason| compact_text(reason))
        .collect()
}

fn compact_mutation_receipt(
    work: &WorkItem,
    control_binding: Option<ControlWorkBinding>,
    receipt: serde_json::Value,
) -> WorkMutationReceipt {
    if let Ok(existing) = serde_json::from_value::<WorkMutationReceipt>(receipt.clone()) {
        return existing;
    }
    let result = match receipt {
        serde_json::Value::Object(object) => {
            let allowed = [
                "attached",
                "blocker_id",
                "checkpoint",
                "claim_id",
                "completion_seal",
                "evidence",
                "evidence_kind",
                "expires_at",
                "fence",
                "generation",
                "lifecycle",
                "offer_id",
                "revision",
                "run_id",
                "state",
                "superseded_by",
                "work_revision",
            ];
            let selected = object
                .into_iter()
                .filter(|(key, value)| {
                    allowed.contains(&key.as_str())
                        && (value.is_string()
                            || value.is_number()
                            || value.is_boolean()
                            || value.is_null())
                })
                .map(|(key, value)| {
                    let value = value.as_str().map_or(value.clone(), |text| {
                        serde_json::Value::String(compact_text(text))
                    });
                    (key, value)
                })
                .collect();
            serde_json::Value::Object(selected)
        }
        serde_json::Value::String(value) => serde_json::Value::String(compact_text(&value)),
        scalar @ (serde_json::Value::Null
        | serde_json::Value::Bool(_)
        | serde_json::Value::Number(_)) => scalar,
        serde_json::Value::Array(values) => serde_json::Value::Array(
            values
                .into_iter()
                .filter(|value| value.is_string() || value.is_number() || value.is_boolean())
                .take(MAX_FOCUS_RELATIONS)
                .map(|value| {
                    value.as_str().map_or(value.clone(), |text| {
                        serde_json::Value::String(compact_text(text))
                    })
                })
                .collect(),
        ),
    };
    WorkMutationReceipt {
        work_id: work.work_id,
        work_ref: work.short_ref.clone(),
        revision: work.revision,
        control_binding,
        result,
    }
}

fn bounded_ready_prefix(
    source: Vec<ReadyWorkSummary>,
    budget: usize,
) -> Result<Vec<ReadyWorkSummary>, StoreError> {
    let mut bounded = Vec::new();
    for mut item in source {
        bounded.push(item.clone());
        if serde_json::to_vec(&bounded)?.len() <= budget {
            continue;
        }
        bounded.pop();
        if bounded.is_empty() {
            minimize_work_item_summary(&mut item.work);
            item.why.clear();
            item.blocked_by.clear();
            item.reason_codes.truncate(MAX_FOCUS_RELATIONS);
            bounded.push(item);
            if serde_json::to_vec(&bounded)?.len() > budget {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "minimal work summary exceeds its {budget}-byte section budget"
                )));
            }
        }
        break;
    }
    Ok(bounded)
}

fn count_omission(section: WorkNextSection, omitted_count: usize) -> WorkSectionOmission {
    WorkSectionOmission {
        section,
        reason: WorkSectionOmissionReason::CountLimit,
        omitted_count,
    }
}

fn work_evidence_summary(
    store: &SqliteStore,
    run_id: WorkRunId,
    hash: &ObjectHash,
) -> Result<WorkEvidenceSummary, StoreError> {
    match store.work_evidence_kind(run_id, hash)? {
        WorkEvidenceKind::Generic => {
            let evidence = store.get::<WorkEvidence>(hash)?.ok_or_else(|| {
                StoreError::InvalidWorkProjection(format!(
                    "generic evidence object {hash} is missing"
                ))
            })?;
            Ok(WorkEvidenceSummary {
                evidence: hash.clone(),
                evidence_kind: WorkEvidenceKind::Generic,
                workspace_id: None,
                source_revision: None,
                producer_session_id: evidence.actor.session_id,
                check_kind: None,
                check_fingerprint: None,
                verification_result: None,
                environment_fingerprint: None,
                summary: compact_text(&evidence.summary),
                created_at: evidence.created_at,
            })
        }
        WorkEvidenceKind::Verification => {
            let evidence = store.load_verification_evidence(hash)?;
            Ok(WorkEvidenceSummary {
                evidence: hash.clone(),
                evidence_kind: WorkEvidenceKind::Verification,
                workspace_id: Some(compact_text(&evidence.source_basis.workspace_id)),
                source_revision: Some(compact_text(&evidence.source_basis.source_revision)),
                producer_session_id: Some(evidence.session_id),
                check_kind: Some(evidence.check_kind),
                check_fingerprint: Some(evidence.check_fingerprint),
                verification_result: Some(evidence.result),
                environment_fingerprint: None,
                summary: compact_text(&evidence.summary),
                created_at: evidence.completed_at,
            })
        }
        WorkEvidenceKind::Environment => {
            let evidence = store.load_environment_evidence(hash)?;
            Ok(WorkEvidenceSummary {
                evidence: hash.clone(),
                evidence_kind: WorkEvidenceKind::Environment,
                workspace_id: Some(compact_text(&evidence.source_basis.workspace_id)),
                source_revision: Some(compact_text(&evidence.source_basis.source_revision)),
                producer_session_id: Some(evidence.session_id),
                check_kind: None,
                check_fingerprint: None,
                verification_result: None,
                environment_fingerprint: Some(evidence.environment_fingerprint),
                summary: "host-recorded environment identity".into(),
                created_at: evidence.observed_at,
            })
        }
    }
}

fn work_obligation_summary(record: &crate::storage::WorkObligationRecord) -> WorkObligationSummary {
    let evidence = record.resolution.as_ref().and_then(|event| {
        if let WorkObligationResolution::Satisfied { evidence, .. } = &event.resolution {
            Some(evidence.clone())
        } else {
            None
        }
    });
    WorkObligationSummary {
        obligation_id: record.obligation.obligation_id,
        definition: record.definition_hash.clone(),
        state: record.state,
        rule: record.obligation.rule.clone(),
        requirement: record.obligation.requirement.clone(),
        triggering_observation: record.obligation.triggering_observation.clone(),
        resolution: record.resolution_hash.clone(),
        evidence,
    }
}

fn record_byte_omission(response: &mut WorkNextView, section: WorkNextSection) {
    if let Some(existing) = response.omissions.iter_mut().find(|entry| {
        entry.section == section && entry.reason == WorkSectionOmissionReason::ByteBudget
    }) {
        existing.omitted_count += 1;
    } else {
        response.omissions.push(WorkSectionOmission {
            section,
            reason: WorkSectionOmissionReason::ByteBudget,
            omitted_count: 1,
        });
    }
}

fn fit_work_next_response(response: &mut WorkNextView) -> Result<(), StoreError> {
    while serde_json::to_vec(response)?.len() > MAX_AGENT_WORK_RESPONSE_BYTES {
        if let Some(catalog) = response
            .catalog
            .as_mut()
            .filter(|catalog| catalog.items.len() > 1)
            && catalog.items.pop().is_some()
        {
            catalog.next_after = catalog.items.last().map(|item| item.work.work_id);
            record_byte_omission(response, WorkNextSection::Catalog);
            continue;
        }
        if response
            .ready
            .as_mut()
            .is_some_and(|ready| ready.len() > 1 && ready.pop().is_some())
        {
            record_byte_omission(response, WorkNextSection::Ready);
            continue;
        }
        if let Some(focus) = response.focus.as_mut()
            && trim_focus_once(focus)
        {
            record_byte_omission(response, WorkNextSection::Focus);
            continue;
        }
        break;
    }
    Ok(())
}

fn fit_focus_response(response: &mut WorkFocusView) -> Result<(), StoreError> {
    while serde_json::to_vec(response)?.len() > MAX_AGENT_WORK_RESPONSE_BYTES
        && trim_focus_once(response)
    {
        if let Some(existing) = response.omissions.iter_mut().find(|entry| {
            entry.section == WorkNextSection::Focus
                && entry.reason == WorkSectionOmissionReason::ByteBudget
        }) {
            existing.omitted_count += 1;
        } else {
            response.omissions.push(WorkSectionOmission {
                section: WorkNextSection::Focus,
                reason: WorkSectionOmissionReason::ByteBudget,
                omitted_count: 1,
            });
        }
    }
    Ok(())
}

fn trim_focus_once(focus: &mut WorkFocusView) -> bool {
    if focus.history.items.pop().is_some() {
        focus.history.omitted = focus.history.omitted.saturating_add(1);
        return true;
    }
    if let Some(blocker) = focus
        .blockers
        .iter_mut()
        .rev()
        .find(|blocker| !blocker.detail.is_empty())
    {
        blocker.detail.clear();
        return true;
    }
    focus.memories.pop().is_some()
        || focus.children.pop().is_some()
        || focus.prerequisites.pop().is_some()
        || focus.handoffs.pop().is_some()
        || focus.obligation_items.pop().is_some()
        || focus.evidence_items.pop().is_some()
        || focus.evidence.pop().is_some()
}

fn ensure_agent_response_budget<T: Serialize>(
    response: &T,
    operation: &str,
) -> Result<(), StoreError> {
    let size = serde_json::to_vec(response)?.len();
    if size > MAX_AGENT_WORK_RESPONSE_BYTES {
        return Err(StoreError::InvalidWorkProjection(format!(
            "{operation} response is {size} bytes, exceeding the {MAX_AGENT_WORK_RESPONSE_BYTES}-byte agent protocol limit"
        )));
    }
    Ok(())
}

fn active_run_id(work: &WorkItem) -> Result<crate::WorkRunId, StoreError> {
    work.active_run_id.ok_or_else(|| {
        StoreError::InvalidWorkProjection("work has no active run for this operation".into())
    })
}

fn propose_metadata(input: &WorkProposeInput) -> (&'static str, &'static str, &str) {
    match input {
        WorkProposeInput::Root {
            idempotency_key, ..
        } => ("work_propose:root", "create_work", idempotency_key),
        WorkProposeInput::Decompose {
            idempotency_key, ..
        } => ("work_propose:decompose", "decompose_work", idempotency_key),
    }
}

fn update_metadata(input: &WorkUpdateInput) -> (&'static str, &'static str, &str) {
    match input {
        WorkUpdateInput::Claim {
            idempotency_key, ..
        } => ("claim", "claim_work", idempotency_key),
        WorkUpdateInput::Release {
            idempotency_key, ..
        } => ("release", "release_work", idempotency_key),
        WorkUpdateInput::Checkpoint {
            idempotency_key, ..
        } => ("checkpoint", "checkpoint_work", idempotency_key),
        WorkUpdateInput::Evidence {
            idempotency_key, ..
        } => ("evidence", "record_work_evidence", idempotency_key),
        WorkUpdateInput::Block {
            idempotency_key, ..
        } => ("block", "add_work_blocker", idempotency_key),
        WorkUpdateInput::Unblock {
            idempotency_key, ..
        } => ("unblock", "clear_work_blocker", idempotency_key),
        WorkUpdateInput::Revise {
            idempotency_key, ..
        } => ("revise", "revise_work", idempotency_key),
        WorkUpdateInput::AddPrerequisite {
            idempotency_key, ..
        } => ("add_prerequisite", "add_work_prerequisite", idempotency_key),
        WorkUpdateInput::RemovePrerequisite {
            idempotency_key, ..
        } => (
            "remove_prerequisite",
            "remove_work_prerequisite",
            idempotency_key,
        ),
        WorkUpdateInput::Reopen {
            idempotency_key, ..
        } => ("reopen", "reopen_work", idempotency_key),
        WorkUpdateInput::Cancel {
            idempotency_key, ..
        } => ("cancel", "dispose_work", idempotency_key),
        WorkUpdateInput::Supersede {
            idempotency_key, ..
        } => ("supersede", "dispose_work", idempotency_key),
        WorkUpdateInput::WaiveRequiredChild {
            idempotency_key, ..
        } => (
            "waive_required_child",
            "waive_required_child",
            idempotency_key,
        ),
    }
}

fn handoff_metadata(input: &WorkHandoffInput) -> (&'static str, &'static str, &str) {
    match input {
        WorkHandoffInput::Offer {
            idempotency_key, ..
        } => ("offer", "offer_work_handoff", idempotency_key),
        WorkHandoffInput::Accept { idempotency_key } => {
            ("accept", "accept_work_handoff", idempotency_key)
        }
        WorkHandoffInput::Cancel {
            idempotency_key, ..
        } => ("cancel", "cancel_work_handoff", idempotency_key),
    }
}

fn completion_result(seal: &CompletionSeal) -> Result<WorkCompleteResult, StoreError> {
    Ok(WorkCompleteResult::Completed(WorkCompletedReceipt {
        seal: crate::CanonicalObject::freeze(seal)?.hash().clone(),
        work_id: seal.work_id,
        run_id: seal.run_id,
        completed_at: seal.completed_at,
    }))
}

fn completion_protocol_result(
    completion: Result<CompletionSeal, StoreError>,
) -> Result<WorkCompleteResult, StoreError> {
    match completion {
        Ok(seal) => completion_result(&seal),
        Err(StoreError::OpenWorkObligations {
            work,
            obligations,
            omitted_count,
        }) => Ok(WorkCompleteResult::Refused(WorkCompleteRefusal {
            code: "open_work_obligations".into(),
            work_id: work,
            obligations,
            omitted_count,
            remedy: "record the matching host verification, then checkpoint_work acknowledging it, then complete; or request a host/operator waiver"
                .into(),
        })),
        Err(error) => Err(error),
    }
}

fn bind_completion_acceptance_evidence(
    mut acceptance: Vec<AcceptanceResult>,
    completion_evidence: &[ObjectHash],
) -> Vec<AcceptanceResult> {
    for result in &mut acceptance {
        if result.evidence.is_empty() {
            result.evidence = completion_evidence.to_vec();
        }
    }
    acceptance
}

fn parse_hashes(values: &[String]) -> Result<Vec<ObjectHash>, StoreError> {
    values
        .iter()
        .map(|value| {
            ObjectHash::from_str(value)
                .map_err(|message| StoreError::InvalidWork(message.to_owned()))
        })
        .collect()
}

fn parse_hash(value: &str) -> Result<ObjectHash, StoreError> {
    ObjectHash::from_str(value).map_err(|message| StoreError::InvalidWork(message.to_owned()))
}

fn work_delivery_boundary(
    store: &SqliteStore,
    project_id: &ProjectId,
    session_id: &SessionId,
    focused_work_id: Option<WorkId>,
) -> Result<(Option<WorkId>, Option<TaskId>), StoreError> {
    let focused_root_id = focused_work_id
        .map(|work_id| {
            let work = store.get_work_item(work_id)?;
            if work.project_id != *project_id {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "focused work {} does not belong to project {:?}",
                    work.work_id.0, project_id.0
                )));
            }
            Ok(work.root_id)
        })
        .transpose()?;
    let bound_task_id = match store.bound_task(project_id, session_id) {
        Ok(task_id) => Some(task_id),
        Err(StoreError::NoActiveTask(_)) => None,
        Err(error) => return Err(error),
    };
    Ok((focused_root_id, bound_task_id))
}

fn verified_bounded_work_changes(
    store: &SqliteStore,
    project_id: &ProjectId,
    focused_root_id: Option<WorkId>,
    bound_task_id: Option<TaskId>,
    entries: Vec<WorkFeedEntry>,
    confirmed_through: i64,
    budget: usize,
) -> Result<Vec<WorkChange>, StoreError> {
    let has_entries = !entries.is_empty();
    let mut changes = Vec::new();
    for (offset, entry) in entries.into_iter().enumerate() {
        let expected = confirmed_through
            + i64::try_from(offset).map_err(|_| {
                StoreError::InvalidWorkProjection("work delivery offset overflowed".into())
            })?
            + 1;
        if entry.position.position != expected {
            return Err(StoreError::InvalidWorkProjection(format!(
                "work delivery expected position {expected} but found {}",
                entry.position.position
            )));
        }
        let object = store
            .get::<serde_json::Value>(&entry.object_hash)?
            .ok_or_else(|| {
                StoreError::InvalidWorkProjection(format!(
                    "project-feed object {} is missing",
                    entry.object_hash
                ))
            })?;
        changes.push(WorkChange {
            delivery: agent_change_object(
                store,
                project_id,
                focused_root_id,
                bound_task_id,
                &entry.object_kind,
                object,
            )?,
            entry,
        });
        if serde_json::to_vec(&changes)?.len() > budget {
            changes.pop();
            break;
        }
    }
    if has_entries && changes.is_empty() {
        return Err(StoreError::InvalidWorkProjection(format!(
            "one compact work change exceeds its {budget}-byte section budget"
        )));
    }
    Ok(changes)
}

fn verify_staged_work_change_page(
    store: &SqliteStore,
    feed: &FeedId,
    confirmed_through: i64,
    delivered_through: i64,
    page: &StagedWorkChangePage,
) -> Result<(), StoreError> {
    if page.schema_version != SCHEMA_VERSION {
        return Err(StoreError::InvalidWorkProjection(format!(
            "staged work delivery schema {} is unsupported",
            page.schema_version
        )));
    }
    let entries = store.work_feed_between(feed, confirmed_through, delivered_through)?;
    if entries.len() != page.changes.len()
        || entries.iter().zip(&page.changes).any(|(entry, change)| {
            entry.position != change.entry.position
                || entry.object_kind != change.entry.object_kind
                || entry.object_hash != change.entry.object_hash
        })
    {
        return Err(StoreError::InvalidWorkProjection(
            "staged work delivery payload does not bind its exact dense source interval".into(),
        ));
    }
    for entry in entries {
        if store
            .get::<serde_json::Value>(&entry.object_hash)?
            .is_none()
        {
            return Err(StoreError::InvalidWorkProjection(format!(
                "staged project-feed object {} is missing",
                entry.object_hash
            )));
        }
    }
    Ok(())
}

fn ensure_protocol_basis(
    basis_matches: bool,
    operation: &str,
    key: &str,
    core_committed: bool,
) -> Result<(), StoreError> {
    if basis_matches || core_committed {
        Ok(())
    } else {
        Err(StoreError::WorkOperationIdempotencyConflict {
            operation: operation.to_owned(),
            key: key.to_owned(),
        })
    }
}

#[allow(
    clippy::too_many_lines,
    reason = "the exhaustive object-kind projection keeps every privacy boundary and compact summary shape in one audited match"
)]
fn agent_change_object(
    store: &SqliteStore,
    project_id: &ProjectId,
    focused_root_id: Option<WorkId>,
    bound_task_id: Option<TaskId>,
    object_kind: &str,
    object: serde_json::Value,
) -> Result<WorkChangeProjection, StoreError> {
    match object_kind {
        "work_event" => serde_json::from_value::<WorkEvent>(object)
            .map(|event| agent_work_event_summary(&event))
            .map(WorkChangeProjection::Visible)
            .map_err(StoreError::from),
        "work_checkpoint" => {
            let checkpoint = serde_json::from_value::<WorkCheckpoint>(object)?;
            let item = store.get_work_item(checkpoint.work_id)?;
            Ok(WorkChangeProjection::Visible(WorkChangeSummary {
                schema_version: checkpoint.schema_version,
                object_kind: object_kind.into(),
                work_id: Some(checkpoint.work_id),
                work_ref: Some(item.short_ref),
                revision: None,
                change_kind: "checkpoint".into(),
                summary: compact_text(&checkpoint.summary),
                actor_id: Some(compact_text(&checkpoint.actor.actor_id)),
                created_at: checkpoint.created_at,
            }))
        }
        "work_evidence" => {
            let evidence = serde_json::from_value::<WorkEvidence>(object)?;
            let item = store.get_work_item(evidence.work_id)?;
            Ok(WorkChangeProjection::Visible(WorkChangeSummary {
                schema_version: evidence.schema_version,
                object_kind: object_kind.into(),
                work_id: Some(evidence.work_id),
                work_ref: Some(item.short_ref),
                revision: None,
                change_kind: "evidence".into(),
                summary: compact_text(&evidence.summary),
                actor_id: Some(compact_text(&evidence.actor.actor_id)),
                created_at: evidence.created_at,
            }))
        }
        "execution_observation" => {
            let observation = serde_json::from_value::<ExecutionObservation>(object)?;
            let item = store.get_work_item(observation.binding.work_id)?;
            if &observation.project_id != project_id {
                return Err(StoreError::InvalidWorkProjection(
                    "execution observation is bound outside its project work item".into(),
                ));
            }
            Ok(WorkChangeProjection::Visible(WorkChangeSummary {
                schema_version: observation.schema_version,
                object_kind: object_kind.into(),
                work_id: Some(observation.binding.work_id),
                work_ref: Some(item.short_ref),
                revision: Some(observation.binding.work_revision),
                change_kind: "execution_observation".into(),
                summary: compact_text(&format!(
                    "{:?} {:?}; source_changed={}",
                    observation.effect, observation.outcome, observation.source_changed
                )),
                actor_id: Some(compact_text(&observation.actor.actor_id)),
                created_at: observation.observed_at.unwrap_or(observation.recorded_at),
            }))
        }
        "verification_evidence" => {
            let evidence = serde_json::from_value::<VerificationEvidence>(object)?;
            let item = store.get_work_item(evidence.binding.work_id)?;
            if &evidence.project_id != project_id {
                return Err(StoreError::InvalidWorkProjection(
                    "verification evidence is bound outside its project work item".into(),
                ));
            }
            Ok(WorkChangeProjection::Visible(WorkChangeSummary {
                schema_version: evidence.schema_version,
                object_kind: object_kind.into(),
                work_id: Some(evidence.binding.work_id),
                work_ref: Some(item.short_ref),
                revision: Some(evidence.binding.work_revision),
                change_kind: "verification_evidence".into(),
                summary: compact_text(&format!(
                    "{:?} {:?}; source_revision={}",
                    evidence.check_kind, evidence.result, evidence.source_basis.source_revision
                )),
                actor_id: Some(compact_text(&evidence.actor.actor_id)),
                created_at: evidence.completed_at,
            }))
        }
        "environment_evidence" => {
            let evidence = serde_json::from_value::<EnvironmentEvidence>(object)?;
            let item = store.get_work_item(evidence.binding.work_id)?;
            if &evidence.project_id != project_id {
                return Err(StoreError::InvalidWorkProjection(
                    "environment evidence is bound outside its project work item".into(),
                ));
            }
            Ok(WorkChangeProjection::Visible(WorkChangeSummary {
                schema_version: evidence.schema_version,
                object_kind: object_kind.into(),
                work_id: Some(evidence.binding.work_id),
                work_ref: Some(item.short_ref),
                revision: Some(evidence.binding.work_revision),
                change_kind: "environment_evidence".into(),
                summary: compact_text(&format!(
                    "environment {}; source_revision={}",
                    evidence.environment_fingerprint, evidence.source_basis.source_revision
                )),
                actor_id: Some(compact_text(&evidence.actor.actor_id)),
                created_at: evidence.observed_at,
            }))
        }
        "work_obligation" => {
            let obligation = serde_json::from_value::<WorkObligation>(object)?;
            let item = store.get_work_item(obligation.work_id)?;
            if &obligation.project_id != project_id {
                return Err(StoreError::InvalidWorkProjection(
                    "work obligation is bound outside its project work item".into(),
                ));
            }
            Ok(WorkChangeProjection::Visible(WorkChangeSummary {
                schema_version: obligation.schema_version,
                object_kind: object_kind.into(),
                work_id: Some(obligation.work_id),
                work_ref: Some(item.short_ref),
                revision: Some(obligation.work_revision),
                change_kind: "obligation_opened".into(),
                summary: compact_text(&format!(
                    "{} v{} requires {:?}",
                    obligation.rule.rule_id,
                    obligation.rule.rule_version,
                    obligation.requirement.check_kind
                )),
                actor_id: None,
                created_at: obligation.opened_at,
            }))
        }
        "work_obligation_resolution" => {
            let event = serde_json::from_value::<WorkObligationResolutionEvent>(object)?;
            let record = store
                .work_run_obligations(event.run_id)?
                .into_iter()
                .find(|record| record.obligation.obligation_id == event.obligation_id)
                .ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "obligation resolution has no verified definition projection".into(),
                    )
                })?;
            let item = store.get_work_item(record.obligation.work_id)?;
            if &event.project_id != project_id {
                return Err(StoreError::InvalidWorkProjection(
                    "work obligation resolution is bound outside its project".into(),
                ));
            }
            let (change_kind, summary) = match event.resolution {
                WorkObligationResolution::Satisfied { evidence, .. } => (
                    "obligation_satisfied",
                    format!("{} satisfied by {evidence}", record.obligation.rule.rule_id),
                ),
                WorkObligationResolution::Waived { .. } => (
                    "obligation_waived",
                    format!(
                        "{} waived by host authority",
                        record.obligation.rule.rule_id
                    ),
                ),
            };
            Ok(WorkChangeProjection::Visible(WorkChangeSummary {
                schema_version: event.schema_version,
                object_kind: object_kind.into(),
                work_id: Some(record.obligation.work_id),
                work_ref: Some(item.short_ref),
                revision: Some(record.obligation.work_revision),
                change_kind: change_kind.into(),
                summary: compact_text(&summary),
                actor_id: Some(compact_text(&event.actor.actor_id)),
                created_at: event.created_at,
            }))
        }
        "memory_version" => {
            let version = serde_json::from_value::<MemoryVersion>(object)?;
            memory_change_projection(
                store,
                project_id,
                focused_root_id,
                object_kind,
                &version,
                || {
                    let (work_id, work_ref) = memory_work_identity(store, &version)?;
                    Ok(WorkChangeSummary {
                        schema_version: version.schema_version,
                        object_kind: object_kind.into(),
                        work_id: Some(work_id),
                        work_ref: Some(work_ref),
                        revision: None,
                        change_kind: "memory_version".into(),
                        summary: compact_text(&version.title),
                        actor_id: Some(compact_text(&version.actor.actor_id)),
                        created_at: version.created_at,
                    })
                },
            )
        }
        "memory_assertion_event" => {
            let assertion = serde_json::from_value::<MemoryAssertionEvent>(object)?;
            let version = store
                .get::<MemoryVersion>(&assertion.version)?
                .ok_or_else(|| {
                    StoreError::InvalidWorkProjection(format!(
                        "memory assertion {} references a missing version",
                        assertion.version
                    ))
                })?;
            if version.memory_id != assertion.memory_id {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "memory assertion {} is bound to a different memory id",
                    assertion.version
                )));
            }
            memory_change_projection(
                store,
                project_id,
                focused_root_id,
                object_kind,
                &version,
                || {
                    let (work_id, work_ref) = memory_work_identity(store, &version)?;
                    Ok(WorkChangeSummary {
                        schema_version: assertion.schema_version,
                        object_kind: object_kind.into(),
                        work_id: Some(work_id),
                        work_ref: Some(work_ref),
                        revision: None,
                        change_kind: format!("memory_{:?}", assertion.status).to_lowercase(),
                        summary: compact_text(&version.title),
                        actor_id: Some(compact_text(&assertion.actor.actor_id)),
                        created_at: assertion.created_at,
                    })
                },
            )
        }
        "memory_contradiction_event" => {
            let event = serde_json::from_value::<MemoryContradictionEvent>(object)?;
            let left = load_contradiction_version(store, &event.left_version)?;
            let right = load_contradiction_version(store, &event.right_version)?;
            let left_omission = shared_memory_endpoint_omission(
                store,
                project_id,
                focused_root_id,
                bound_task_id,
                &left,
            )?;
            let right_omission = shared_memory_endpoint_omission(
                store,
                project_id,
                focused_root_id,
                bound_task_id,
                &right,
            )?;
            let omission = [left_omission, right_omission]
                .into_iter()
                .flatten()
                .min_by_key(|reason| match reason {
                    WorkChangeOmissionReason::RestrictedSensitivity => 0,
                    WorkChangeOmissionReason::OutsideFocusedRoot => 1,
                    WorkChangeOmissionReason::OutsideBoundTask => 2,
                });
            if event.project_id.as_ref() != Some(project_id)
                || event.work_root_id != focused_root_id
            {
                return Ok(omitted_work_change(
                    object_kind,
                    omission.unwrap_or(WorkChangeOmissionReason::OutsideFocusedRoot),
                ));
            }
            omission.map_or_else(
                || {
                    Ok(WorkChangeProjection::Visible(WorkChangeSummary {
                        schema_version: event.schema_version,
                        object_kind: object_kind.into(),
                        work_id: event.work_root_id,
                        work_ref: None,
                        revision: None,
                        change_kind: "memory_contradiction".into(),
                        summary: compact_text(&event.reason),
                        actor_id: Some(compact_text(&event.actor.actor_id)),
                        created_at: event.created_at,
                    }))
                },
                |reason| Ok(omitted_work_change(object_kind, reason)),
            )
        }
        other => Err(StoreError::InvalidWorkProjection(format!(
            "project work feed contains unsupported agent object kind {other:?}"
        ))),
    }
}

fn load_contradiction_version(
    store: &SqliteStore,
    version_hash: &ObjectHash,
) -> Result<MemoryVersion, StoreError> {
    store.get::<MemoryVersion>(version_hash)?.ok_or_else(|| {
        StoreError::InvalidWorkProjection(format!(
            "memory contradiction references missing version {version_hash}"
        ))
    })
}

fn memory_change_projection(
    store: &SqliteStore,
    project_id: &ProjectId,
    focused_root_id: Option<WorkId>,
    object_kind: &str,
    version: &MemoryVersion,
    visible: impl FnOnce() -> Result<WorkChangeSummary, StoreError>,
) -> Result<WorkChangeProjection, StoreError> {
    match work_memory_change_omission(store, project_id, focused_root_id, version)? {
        Some(reason) => Ok(omitted_work_change(object_kind, reason)),
        None => visible().map(WorkChangeProjection::Visible),
    }
}

fn memory_work_identity(
    store: &SqliteStore,
    version: &MemoryVersion,
) -> Result<(WorkId, String), StoreError> {
    let Scope::Work { work, .. } = &version.scope else {
        return Err(StoreError::InvalidWorkProjection(
            "project work feed contains a memory outside work scope".into(),
        ));
    };
    let item = store.get_work_item(*work)?;
    Ok((*work, item.short_ref))
}

fn work_memory_change_omission(
    store: &SqliteStore,
    project_id: &ProjectId,
    focused_root_id: Option<WorkId>,
    version: &MemoryVersion,
) -> Result<Option<WorkChangeOmissionReason>, StoreError> {
    let Scope::Work { project, work } = &version.scope else {
        return Err(StoreError::InvalidWorkProjection(
            "project work feed contains a memory outside work scope".into(),
        ));
    };
    let item = store.get_work_item(*work)?;
    if project != project_id || item.project_id != *project_id || item.work_id != *work {
        return Err(StoreError::InvalidWorkProjection(format!(
            "work memory {} is bound outside project {:?}",
            version.memory_id.0, project_id.0
        )));
    }
    if version.sensitivity == Sensitivity::Restricted {
        return Ok(Some(WorkChangeOmissionReason::RestrictedSensitivity));
    }
    Ok((focused_root_id != Some(item.root_id))
        .then_some(WorkChangeOmissionReason::OutsideFocusedRoot))
}

fn shared_memory_endpoint_omission(
    store: &SqliteStore,
    project_id: &ProjectId,
    focused_root_id: Option<WorkId>,
    bound_task_id: Option<TaskId>,
    version: &MemoryVersion,
) -> Result<Option<WorkChangeOmissionReason>, StoreError> {
    let outside_context = match &version.scope {
        Scope::Project { project } => {
            if project != project_id {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "project memory {} is bound outside project {:?}",
                    version.memory_id.0, project_id.0
                )));
            }
            None
        }
        Scope::Task { project, task } => {
            if project != project_id {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "task memory {} is bound outside project {:?}",
                    version.memory_id.0, project_id.0
                )));
            }
            (bound_task_id != Some(*task)).then_some(WorkChangeOmissionReason::OutsideBoundTask)
        }
        Scope::Work { .. } => {
            work_memory_change_omission(store, project_id, focused_root_id, version)?
        }
        Scope::Agent { .. } => {
            return Err(StoreError::InvalidWorkProjection(
                "private memory cannot enter a shared work contradiction".into(),
            ));
        }
    };
    if version.sensitivity == Sensitivity::Restricted {
        return Ok(Some(WorkChangeOmissionReason::RestrictedSensitivity));
    }
    Ok(outside_context)
}

fn omitted_work_change(
    object_kind: &str,
    omission: WorkChangeOmissionReason,
) -> WorkChangeProjection {
    WorkChangeProjection::Omitted(WorkChangeOmission {
        schema_version: SCHEMA_VERSION,
        object_kind: object_kind.to_owned(),
        omission,
    })
}

fn agent_work_event_summary(event: &WorkEvent) -> WorkChangeSummary {
    let change_kind = work_transition_kind(&event.transition);
    WorkChangeSummary {
        schema_version: event.schema_version,
        object_kind: "work_event".into(),
        work_id: Some(event.work_id),
        work_ref: Some(event.work.short_ref.clone()),
        revision: Some(event.revision),
        change_kind: change_kind.into(),
        summary: compact_text(&format!("{change_kind}: {}", event.work.title)),
        actor_id: Some(compact_text(&event.actor.actor_id)),
        created_at: event.created_at,
    }
}

fn work_transition_kind(transition: &WorkTransition) -> &'static str {
    match transition {
        WorkTransition::Created { .. } => "created",
        WorkTransition::Decomposed { .. } => "decomposed",
        WorkTransition::Revised { .. } => "revised",
        WorkTransition::PrerequisiteAdded { .. } => "prerequisite_added",
        WorkTransition::PrerequisiteRemoved { .. } => "prerequisite_removed",
        WorkTransition::Blocked { .. } => "blocked",
        WorkTransition::Unblocked { .. } => "unblocked",
        WorkTransition::Claimed { .. } => "claimed",
        WorkTransition::Released { .. } => "released",
        WorkTransition::Checkpointed { .. } => "checkpointed",
        WorkTransition::HandoffOffered { .. } => "handoff_offered",
        WorkTransition::HandoffExpired { .. } => "handoff_expired",
        WorkTransition::HandoffCancelled { .. } => "handoff_cancelled",
        WorkTransition::HandedOff { .. } => "handed_off",
        WorkTransition::EvidenceAdded { .. } => "evidence_added",
        WorkTransition::TypedEvidenceAdded { .. } => "typed_evidence_added",
        WorkTransition::Completed { .. } => "completed",
        WorkTransition::Disposed { .. } => "disposed",
        WorkTransition::RequiredChildWaived { .. } => "required_child_waived",
        WorkTransition::Reopened { .. } => "reopened",
    }
}

fn agent_update_receipt(
    operation: &str,
    receipt: serde_json::Value,
) -> Result<serde_json::Value, StoreError> {
    if operation != "waive_required_child" {
        return Ok(receipt);
    }
    let waiver: crate::RequiredChildWaiver = serde_json::from_value(receipt)?;
    Ok(serde_json::json!({
        "work_id": waiver.work_id,
        "work_revision": waiver.work_revision,
        "waived_by": waiver.waived_by,
        "reason": waiver.reason,
    }))
}

fn unique_offer(
    offers: impl Iterator<Item = WorkHandoffOffer>,
    direction: &str,
) -> Result<WorkHandoffOffer, StoreError> {
    let offers = offers.collect::<Vec<_>>();
    match offers.as_slice() {
        [offer] => Ok(offer.clone()),
        [] => Err(StoreError::InvalidWork(format!(
            "ambient work has no live {direction} handoff offer"
        ))),
        _ => Err(StoreError::InvalidWorkProjection(format!(
            "ambient work has more than one live {direction} handoff offer"
        ))),
    }
}

fn unique_blocker_id(blockers: &[crate::WorkBlocker]) -> Result<String, StoreError> {
    match blockers {
        [blocker] => Ok(blocker.blocker_id.clone()),
        [] => Err(StoreError::InvalidWork(
            "focused work has no active blocker to infer".into(),
        )),
        _ => Err(StoreError::InvalidWork(
            "focused work has multiple active blockers; provide blocker_id".into(),
        )),
    }
}

#[derive(Clone, Copy)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "the booleans are independently verified admission facts, not one state machine"
)]
struct AllowedNextContext<'a> {
    claim: Option<&'a WorkClaim>,
    handoffs: &'a [WorkHandoffOffer],
    session: &'a SessionId,
    now: DateTime<Utc>,
    authority_operations: &'a [WorkAuthorityOperation],
    can_waive_required_child: bool,
    claim_recovery_required: bool,
    completion_capture_ready: bool,
    completion_preflight_ready: bool,
}

fn allowed_next(status: &ReadyWork, context: AllowedNextContext<'_>) -> Vec<String> {
    let AllowedNextContext {
        claim,
        handoffs,
        session,
        now,
        authority_operations,
        can_waive_required_child,
        claim_recovery_required,
        completion_capture_ready,
        completion_preflight_ready,
    } = context;
    let mut allowed = vec!["work_focus".into()];
    if status.work.lifecycle == WorkLifecycle::Completed {
        if authority_operations.contains(&WorkAuthorityOperation::Reopen) {
            allowed.push("work_update:reopen".into());
        }
        return allowed;
    }
    if status.work.lifecycle != WorkLifecycle::Open {
        return allowed;
    }
    if authority_operations.contains(&WorkAuthorityOperation::Plan) {
        allowed.extend([
            "work_update:revise".into(),
            "work_update:block".into(),
            "work_update:unblock".into(),
            "work_update:add_prerequisite".into(),
            "work_update:remove_prerequisite".into(),
            "work_propose:decompose".into(),
        ]);
    }
    if authority_operations.contains(&WorkAuthorityOperation::Dispose) {
        allowed.extend(["work_update:cancel".into(), "work_update:supersede".into()]);
    }
    if can_waive_required_child {
        allowed.push("work_update:waive_required_child".into());
    }
    match claim {
        Some(claim)
            if claim.state == WorkClaimState::Active
                && claim.holder == *session
                && claim.expires_at > now =>
        {
            allowed.extend([
                "work_update:checkpoint".into(),
                "work_update:evidence".into(),
                "work_update:release".into(),
            ]);
            let outgoing_offer = handoffs.iter().any(|offer| {
                offer.state == WorkHandoffState::Offered
                    && offer.from == *session
                    && offer.expires_at > now
            });
            allowed.push(if outgoing_offer {
                "work_handoff:cancel".into()
            } else {
                "work_handoff:offer".into()
            });
            let can_drain = authority_operations.contains(&WorkAuthorityOperation::CompletionDrain);
            let can_complete_root = status.work.work_id != status.work.root_id
                || authority_operations.contains(&WorkAuthorityOperation::RootComplete);
            if can_drain
                && can_complete_root
                && (completion_capture_ready || completion_preflight_ready)
            {
                allowed.push("work_complete".into());
            }
        }
        Some(claim)
            if claim.state == WorkClaimState::Active
                && claim.holder != *session
                && claim.expires_at > now => {}
        _ if authority_operations.contains(&WorkAuthorityOperation::Claim) => {
            if claim_recovery_required {
                if authority_operations.contains(&WorkAuthorityOperation::ClaimRecovery) {
                    allowed.push("work_update:claim(recovery_reason_required)".into());
                }
            } else {
                allowed.push("work_update:claim".into());
            }
        }
        _ => {}
    }
    if handoffs.iter().any(|offer| {
        offer.state == WorkHandoffState::Offered && offer.to == *session && offer.expires_at > now
    }) && authority_operations.contains(&WorkAuthorityOperation::Claim)
    {
        allowed.push("work_handoff:accept".into());
    }
    allowed.sort();
    allowed.dedup();
    allowed
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};
    use tempfile::tempdir;

    use super::*;
    use crate::{
        WorkAuthorityGrant, WorkAuthorityOperation, WorkAuthorityScope, WorkPlanningBudget,
        domain::SCHEMA_VERSION,
    };

    fn at(second: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 27, 3, 0, 0)
            .single()
            .expect("fixed timestamp")
            + Duration::seconds(second)
    }

    fn install_protocol_grant(
        database: &std::path::Path,
        project: &ProjectId,
        actor_id: &str,
    ) -> ObjectHash {
        install_protocol_grant_with_budget(
            database,
            project,
            actor_id,
            WorkPlanningBudget {
                max_depth: 4,
                max_open_descendants: 32,
                max_children_per_decomposition: 8,
            },
        )
    }

    fn install_protocol_grant_with_budget(
        database: &std::path::Path,
        project: &ProjectId,
        actor_id: &str,
        budget: WorkPlanningBudget,
    ) -> ObjectHash {
        SqliteStore::open(database)
            .expect("store")
            .install_work_authority_grant(
                WorkAuthorityGrant {
                    schema_version: SCHEMA_VERSION,
                    project_id: project.clone(),
                    policy_ref: "project-default".into(),
                    subject_actor_id: actor_id.into(),
                    issued_by: ActorContext {
                        actor_id: "test-host".into(),
                        actor_kind: "host_operator".into(),
                        assurance: AssuranceLevel::Asserted,
                        run_id: None,
                        session_id: None,
                        source_tool: Some("test".into()),
                        source_skill: None,
                        provenance_chain: Vec::new(),
                        reason: "issue test authority".into(),
                    },
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
                    ],
                    scope: WorkAuthorityScope::Project,
                    planning_budget: Some(budget),
                    issued_at: at(-1),
                    valid_until: at(3_600),
                    reason: "test host delegation".into(),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("grant")
    }

    fn root_input(title: &str, key: &str) -> WorkProposeInput {
        WorkProposeInput::Root {
            title: title.into(),
            outcome: format!("{title} outcome"),
            acceptance: vec![format!("{title} accepted")],
            work_kind: None,
            priority: None,
            labels: Vec::new(),
            assigned_to: None,
            deferred_until: None,
            authority_policy_ref: None,
            idempotency_key: key.into(),
        }
    }

    #[test]
    fn core_operation_keys_separate_protocol_variants_and_suboperations() {
        let service = LocalWorkService::new(
            PathBuf::from("unused.sqlite3"),
            ProjectId("key-project".into()),
            "agent".into(),
            SessionId("key-session".into()),
            None,
            None,
        );
        let cancel = service
            .core_operation_key("work_update:cancel", "same-key", "dispose_work")
            .expect("cancel key");
        let supersede = service
            .core_operation_key("work_update:supersede", "same-key", "dispose_work")
            .expect("supersede key");
        let capture = service
            .core_operation_key("work_complete", "same-key", "record_work_evidence")
            .expect("capture key");
        let checkpoint = service
            .core_operation_key("work_complete", "same-key", "checkpoint_work")
            .expect("checkpoint key");
        let complete = service
            .core_operation_key("work_complete", "same-key", "complete_work")
            .expect("completion key");

        assert_ne!(cancel, supersede);
        assert_ne!(capture, checkpoint);
        assert_ne!(checkpoint, complete);
        assert_ne!(capture, complete);
    }

    #[test]
    fn work_update_does_not_admit_obligation_waivers() {
        let attempted = serde_json::json!({
            "kind": "waive_obligation",
            "obligation_id": uuid::Uuid::now_v7(),
            "reason": "agent attempted to waive a host obligation",
            "idempotency_key": "agent-waiver"
        });

        assert!(serde_json::from_value::<WorkUpdateInput>(attempted).is_err());
    }

    #[test]
    fn execution_observation_has_a_compact_agent_work_projection() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("execution-observation-projection".into());
        let grant = install_protocol_grant(&database, &project, "agent");
        let service = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            SessionId("session".into()),
            Some("protocol-test".into()),
            Some(grant),
        );
        let work = match service
            .work_propose(root_input("Observe execution", "root"), at(0))
            .expect("root")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        let run_id = work.active_run_id.expect("active run");
        let observation = ExecutionObservation {
            schema_version: SCHEMA_VERSION,
            project_id: project.clone(),
            binding: crate::ControlWorkBinding {
                root_execution_id: crate::RootExecutionId::new(),
                work_id: work.work_id,
                run_id,
                work_revision: work.revision,
                claim_id: crate::WorkClaimId::new(),
                claim_fence: 1,
            },
            session_id: SessionId("session".into()),
            grant_id: "grant".into(),
            observation_id: "observation".into(),
            action_fingerprint: ObjectHash::from_canonical_bytes(b"write source"),
            effect: crate::EffectClass::MutateLocal,
            outcome: crate::ExecutionOutcome::Succeeded,
            source_changed: true,
            source_basis: Some(crate::ExecutionSourceBasis {
                workspace_id: "workspace-a".into(),
                source_revision: "revision-a".into(),
            }),
            observed_at: Some(at(1)),
            actor: ActorContext {
                actor_id: "host".into(),
                actor_kind: "host".into(),
                assurance: AssuranceLevel::Asserted,
                run_id: Some(run_id.0.to_string()),
                session_id: Some(SessionId("session".into())),
                source_tool: Some("host-control:turn_checkpoint".into()),
                source_skill: None,
                provenance_chain: Vec::new(),
                reason: "record execution fact".into(),
            },
            recorded_at: at(2),
        };
        let store = SqliteStore::open(database).expect("store");
        let projection = agent_change_object(
            &store,
            &project,
            Some(work.root_id),
            None,
            "execution_observation",
            serde_json::to_value(observation).expect("observation json"),
        )
        .expect("agent projection");
        let WorkChangeProjection::Visible(summary) = projection else {
            panic!("execution observation must remain visible");
        };
        assert_eq!(summary.work_id, Some(work.work_id));
        assert_eq!(summary.change_kind, "execution_observation");
        assert!(summary.summary.contains("MutateLocal Succeeded"));
        assert!(!summary.summary.contains("write source"));
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the exhaustive security regression enumerates every transition variant"
    )]
    fn agent_work_projections_exclude_authority_from_every_transition_and_waiver_receipt() {
        let grant = ObjectHash::from_canonical_bytes(b"host-only-grant");
        let work_id = WorkId::new();
        let run_id = crate::WorkRunId::new();
        let claim_id = crate::WorkClaimId::new();
        let offer_id = crate::WorkHandoffOfferId::new();
        let planning = WorkPlanningAuthority::Delegated {
            grant: grant.clone(),
        };
        let claim = WorkClaim {
            claim_id,
            work_id,
            run_id,
            accepted_work_revision: 1,
            holder: SessionId("holder".into()),
            expires_at: at(60),
            revision: 1,
            fence: 1,
            state: WorkClaimState::Active,
        };
        let transitions = vec![
            WorkTransition::Created {
                prerequisites: vec![WorkId::new()],
                authority_grant: grant.clone(),
            },
            WorkTransition::Decomposed {
                children: vec![WorkId::new()],
                authority: planning.clone(),
            },
            WorkTransition::Revised {
                authority: planning.clone(),
            },
            WorkTransition::PrerequisiteAdded {
                prerequisite_id: WorkId::new(),
                authority: planning.clone(),
            },
            WorkTransition::PrerequisiteRemoved {
                prerequisite_id: WorkId::new(),
                authority: planning,
            },
            WorkTransition::Blocked {
                blocker_id: "blocker".into(),
            },
            WorkTransition::Unblocked {
                blocker_id: "blocker".into(),
            },
            WorkTransition::Claimed {
                claim: claim.clone(),
                recovered: false,
                authority_grant: grant.clone(),
            },
            WorkTransition::Released {
                claim_id,
                fence: 2,
                reason: "released intentionally".into(),
            },
            WorkTransition::Checkpointed {
                checkpoint: ObjectHash::from_canonical_bytes(b"checkpoint"),
            },
            WorkTransition::HandoffOffered {
                offer_id,
                to: SessionId("recipient".into()),
                checkpoint: ObjectHash::from_canonical_bytes(b"handoff-checkpoint"),
                offer: ObjectHash::from_canonical_bytes(b"offer"),
            },
            WorkTransition::HandoffExpired {
                offer_id,
                offer: ObjectHash::from_canonical_bytes(b"expired-offer"),
            },
            WorkTransition::HandoffCancelled {
                offer_id,
                offer: ObjectHash::from_canonical_bytes(b"cancelled-offer"),
                reason: "recipient unavailable".into(),
            },
            WorkTransition::HandedOff {
                offer_id,
                claim_id,
                from: SessionId("holder".into()),
                to: SessionId("recipient".into()),
                fence: 2,
                checkpoint: ObjectHash::from_canonical_bytes(b"accepted-checkpoint"),
                authority_grant: grant.clone(),
                offer: ObjectHash::from_canonical_bytes(b"accepted-offer"),
            },
            WorkTransition::EvidenceAdded {
                evidence: ObjectHash::from_canonical_bytes(b"evidence"),
            },
            WorkTransition::TypedEvidenceAdded {
                evidence: ObjectHash::from_canonical_bytes(b"typed-evidence"),
                evidence_kind: WorkEvidenceKind::Verification,
            },
            WorkTransition::Completed {
                seal: ObjectHash::from_canonical_bytes(b"seal"),
            },
            WorkTransition::Disposed {
                lifecycle: WorkLifecycle::Cancelled,
                replacement_id: None,
                reason: "cancelled".into(),
                authority_grant: grant.clone(),
            },
            WorkTransition::RequiredChildWaived {
                child_id: WorkId::new(),
                child_revision: 2,
                reason: "waived".into(),
                authority_grant: grant.clone(),
            },
            WorkTransition::Reopened {
                run_id,
                generation: 2,
                authority: LifecycleAuthorityDecision {
                    grant: grant.clone(),
                },
                reason: "new execution generation".into(),
            },
        ];
        let actor = ActorContext {
            actor_id: "agent".into(),
            actor_kind: "coding_agent".into(),
            assurance: AssuranceLevel::Asserted,
            run_id: None,
            session_id: Some(SessionId("holder".into())),
            source_tool: Some("test".into()),
            source_skill: None,
            provenance_chain: Vec::new(),
            reason: "exercise agent projection".into(),
        };
        let work = WorkItem {
            schema_version: SCHEMA_VERSION,
            project_id: ProjectId("projection-project".into()),
            work_id,
            short_ref: "projection-ref".into(),
            root_id: work_id,
            parent_id: None,
            child_requirement: ChildRequirement::Required,
            title: "Projection test".into(),
            outcome: "No authority crosses the agent boundary".into(),
            acceptance: vec!["all transitions are covered".into()],
            kind: WorkItemKind::Task,
            priority: 1,
            labels: Vec::new(),
            assigned_to: None,
            deferred_until: None,
            origin: WorkOrigin::Local,
            source_snapshot_id: None,
            authority_policy_ref: "project-default".into(),
            lifecycle: WorkLifecycle::Open,
            revision: 1,
            active_run_id: Some(run_id),
            superseded_by: None,
            created_by: actor.clone(),
            created_at: at(0),
            updated_at: at(0),
        };
        let mut authority_bearing_transitions = 0;
        for transition in transitions {
            let raw = serde_json::to_string(&transition).expect("serialize raw transition");
            if raw.contains(grant.as_str()) {
                authority_bearing_transitions += 1;
            }
            let event = WorkEvent {
                schema_version: SCHEMA_VERSION,
                project_id: work.project_id.clone(),
                root_id: work.root_id,
                work_id,
                run_id: Some(run_id),
                revision: work.revision,
                work: work.clone(),
                run: None,
                root_execution: None,
                claim: Some(claim.clone()),
                handoff_offer: None,
                blocker: None,
                transition,
                actor: actor.clone(),
                created_at: at(1),
            };
            let projected = serde_json::to_string(&agent_work_event_summary(&event))
                .expect("serialize projected transition");
            assert!(!projected.contains(grant.as_str()));
            assert!(!projected.contains("authority_grant"));
            assert!(!projected.contains("\"authority\""));
        }
        assert!(authority_bearing_transitions > 0);

        let receipt = agent_update_receipt(
            "waive_required_child",
            serde_json::to_value(crate::RequiredChildWaiver {
                work_id,
                work_revision: 2,
                authority_grant: grant.clone(),
                waived_by: "host".into(),
                reason: "explicit omission".into(),
            })
            .expect("waiver receipt"),
        )
        .expect("agent waiver receipt");
        assert!(
            !serde_json::to_string(&receipt)
                .expect("serialize receipt")
                .contains(grant.as_str())
        );
    }

    #[test]
    fn oversized_ready_item_degrades_to_one_progress_making_summary() {
        let work_id = WorkId::new();
        let actor = ActorContext {
            actor_id: "agent".into(),
            actor_kind: "coding_agent".into(),
            assurance: AssuranceLevel::Asserted,
            run_id: None,
            session_id: Some(SessionId("session".into())),
            source_tool: Some("test".into()),
            source_skill: None,
            provenance_chain: Vec::new(),
            reason: "exercise bounded ready delivery".into(),
        };
        let work = WorkItem {
            schema_version: SCHEMA_VERSION,
            project_id: ProjectId("bounded-prefix".into()),
            work_id,
            short_ref: "bounded-ref".into(),
            root_id: work_id,
            parent_id: None,
            child_requirement: ChildRequirement::Required,
            title: "x".repeat(1_000),
            outcome: "x".repeat(1_000),
            acceptance: (0..MAX_ACCEPTANCE_ITEMS)
                .map(|_| "x".repeat(1_000))
                .collect(),
            kind: WorkItemKind::Task,
            priority: 1,
            labels: (0..MAX_LABEL_ITEMS).map(|_| "x".repeat(1_000)).collect(),
            assigned_to: Some("x".repeat(1_000)),
            deferred_until: None,
            origin: WorkOrigin::Local,
            source_snapshot_id: None,
            authority_policy_ref: "project-default".into(),
            lifecycle: WorkLifecycle::Open,
            revision: 1,
            active_run_id: None,
            superseded_by: None,
            created_by: actor,
            created_at: at(0),
            updated_at: at(0),
        };
        let source = vec![ReadyWorkSummary {
            work: work_item_summary(&work),
            availability: WorkAvailability::Ready,
            reason_codes: Vec::new(),
            why: vec!["x".repeat(1_000); MAX_FOCUS_RELATIONS],
            blocked_by: vec![WorkId::new(); MAX_FOCUS_RELATIONS],
            blocker_count: MAX_FOCUS_RELATIONS,
        }];
        assert!(
            serde_json::to_vec(&source).expect("serialize source").len() > MAX_READY_SECTION_BYTES
        );

        let bounded = bounded_ready_prefix(source, MAX_READY_SECTION_BYTES)
            .expect("degrade oversized ready summary");
        assert_eq!(bounded.len(), 1);
        assert!(
            serde_json::to_vec(&bounded)
                .expect("serialize bounded")
                .len()
                <= MAX_READY_SECTION_BYTES
        );
    }

    #[test]
    fn focus_exposes_blocker_ids_and_single_blocker_unblock_is_ambient() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("ambient-blockers".into());
        let grant = install_protocol_grant(&database, &project, "agent");
        let service = LocalWorkService::new(
            database,
            project,
            "agent".into(),
            SessionId("session".into()),
            Some("protocol-test".into()),
            Some(grant),
        );
        let root = match service
            .work_propose(root_input("Resolve blockers", "root"), at(0))
            .expect("root")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        for (key, detail) in [
            ("block-a", "first ".repeat(200)),
            ("block-b", "second ".repeat(200)),
        ] {
            service
                .work_update(
                    WorkUpdateInput::Block {
                        blocker_kind: WorkBlockerKind::ExternalInput,
                        detail,
                        idempotency_key: key.into(),
                    },
                    at(1),
                )
                .expect("block work");
        }
        let focus = service
            .work_focus(&root.short_ref, at(2))
            .expect("blocked focus");
        assert_eq!(focus.blockers.len(), 2);
        assert!(
            focus
                .blockers
                .iter()
                .all(|blocker| !blocker.blocker_id.is_empty())
        );
        assert!(
            serde_json::to_vec(&focus).expect("serialize focus").len()
                <= MAX_AGENT_WORK_RESPONSE_BYTES
        );

        let ambiguous = service.work_update(
            WorkUpdateInput::Unblock {
                blocker_id: None,
                idempotency_key: "ambiguous-unblock".into(),
            },
            at(3),
        );
        assert!(matches!(ambiguous, Err(StoreError::InvalidWork(_))));
        service
            .work_update(
                WorkUpdateInput::Unblock {
                    blocker_id: Some(focus.blockers[0].blocker_id.clone()),
                    idempotency_key: "explicit-unblock".into(),
                },
                at(4),
            )
            .expect("explicit unblock");
        service
            .work_update(
                WorkUpdateInput::Unblock {
                    blocker_id: None,
                    idempotency_key: "ambient-unblock".into(),
                },
                at(5),
            )
            .expect("infer sole blocker");
        assert!(
            service
                .work_focus(&root.short_ref, at(6))
                .expect("unblocked focus")
                .blockers
                .is_empty()
        );
    }

    #[test]
    fn interrupted_attempt_revalidates_authority() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");

        let revoked_project = ProjectId("revoked-attempt".into());
        let revoked_grant = install_protocol_grant(&database, &revoked_project, "agent");
        let revoked_service = LocalWorkService::new(
            database.clone(),
            revoked_project.clone(),
            "agent".into(),
            SessionId("revoked-session".into()),
            Some("protocol-test".into()),
            Some(revoked_grant.clone()),
        );
        let never_executed = root_input("Never executed", "interrupted-root");
        let mut store = SqliteStore::open(&database).expect("store");
        let basis = revoked_service
            .protocol_basis(&store, false, false, at(0))
            .expect("empty root basis");
        let intent = revoked_service.protocol_intent(&never_executed);
        store
            .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
                project_id: &revoked_project,
                session_id: &SessionId("revoked-session".into()),
                operation: "work_propose:root",
                idempotency_key: "interrupted-root",
                intent: &intent,
                basis: &basis,
                now: at(0),
            })
            .expect("persist interrupted attempt");
        store
            .revoke_work_authority_grant(
                &revoked_grant,
                &revoked_service.actor("test", "revoke interrupted authority"),
                "authority was withdrawn before execution",
                at(1),
                &DevelopmentNoopRedactor,
            )
            .expect("revoke grant");
        drop(store);
        assert!(matches!(
            revoked_service.work_propose(never_executed, at(2)),
            Err(StoreError::InvalidWork(_))
        ));
    }

    #[test]
    fn interrupted_attempt_cannot_follow_changed_focus() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let focus_project = ProjectId("focus-attempt".into());
        let focus_grant = install_protocol_grant(&database, &focus_project, "agent");
        let focus_service = LocalWorkService::new(
            database.clone(),
            focus_project.clone(),
            "agent".into(),
            SessionId("focus-session".into()),
            Some("protocol-test".into()),
            Some(focus_grant),
        );
        let first = match focus_service
            .work_propose(root_input("First target", "first-root"), at(3))
            .expect("first root")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        let revise = WorkUpdateInput::Revise {
            patch: WorkRevisionPatch {
                title: Some("Must stay on first target".into()),
                ..WorkRevisionPatch::default()
            },
            idempotency_key: "interrupted-revise".into(),
        };
        let mut store = SqliteStore::open(&database).expect("store");
        let basis = focus_service
            .protocol_basis(&store, true, false, at(4))
            .expect("bound first focus");
        assert_eq!(
            basis.focused_work.as_ref().map(|work| work.work_id),
            Some(first.work_id)
        );
        let intent = focus_service.protocol_intent(&revise);
        store
            .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
                project_id: &focus_project,
                session_id: &SessionId("focus-session".into()),
                operation: "work_update:revise",
                idempotency_key: "interrupted-revise",
                intent: &intent,
                basis: &basis,
                now: at(4),
            })
            .expect("persist focus-bound attempt");
        drop(store);
        let second = match focus_service
            .work_propose(root_input("Second target", "second-root"), at(5))
            .expect("second root")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        assert!(matches!(
            focus_service.work_update(revise, at(6)),
            Err(StoreError::WorkOperationIdempotencyConflict { .. })
        ));
        let unchanged = SqliteStore::open(&database)
            .expect("store")
            .get_work_item(second.work_id)
            .expect("second target");
        assert_eq!(unchanged.title, "Second target");
        assert_eq!(unchanged.revision, second.revision);
    }

    #[test]
    fn core_committed_update_recovery_uses_the_durable_focus_basis() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("committed-update-focus".into());
        let grant = install_protocol_grant(&database, &project, "agent");
        let session = SessionId("committed-update-session".into());
        let service = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            session.clone(),
            Some("protocol-test".into()),
            Some(grant),
        );
        let first = match service
            .work_propose(root_input("Original focus", "committed-first"), at(0))
            .expect("first root")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        let input = WorkUpdateInput::Revise {
            patch: WorkRevisionPatch {
                title: Some("Durably revised original focus".into()),
                ..WorkRevisionPatch::default()
            },
            idempotency_key: "committed-revise".into(),
        };

        let mut store = SqliteStore::open(&database).expect("store");
        let basis = service
            .protocol_basis(&store, true, false, at(1))
            .expect("original basis");
        let intent = service.protocol_intent(&input);
        store
            .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
                project_id: &project,
                session_id: &session,
                operation: "work_update:revise",
                idempotency_key: "committed-revise",
                intent: &intent,
                basis: &basis,
                now: at(1),
            })
            .expect("begin durable attempt");
        let original = basis.focused_work.clone().expect("original focus");
        let authority = service
            .planning_authority(basis.claim.as_ref(), &original, at(1))
            .expect("planning authority");
        store
            .revise_work(
                &ReviseWorkRequest {
                    work_id: original.work_id,
                    expected_revision: original.revision,
                    patch: WorkRevisionPatch {
                        title: Some("Durably revised original focus".into()),
                        ..WorkRevisionPatch::default()
                    },
                    authority,
                    actor: service.actor("test", "commit only the scoped update"),
                    idempotency_key: service
                        .core_operation_key("work_update:revise", "committed-revise", "revise_work")
                        .expect("scoped operation key"),
                    updated_at: at(1),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("commit core revise without protocol result");
        drop(store);

        let second = match service
            .work_propose(root_input("New live focus", "committed-second"), at(2))
            .expect("second root")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        let recovered = service
            .work_update(input.clone(), at(3))
            .expect("recover core-committed update");
        assert_eq!(recovered.receipt.work_id, first.work_id);
        assert_eq!(recovered.receipt.revision, first.revision + 1);
        assert_ne!(recovered.receipt.work_id, second.work_id);
        let replayed = service
            .work_update(input, at(4))
            .expect("replay exact protocol result");
        assert_eq!(
            serde_json::to_vec(&replayed).expect("serialize replay"),
            serde_json::to_vec(&recovered).expect("serialize recovery")
        );
        assert_eq!(
            SqliteStore::open(&database)
                .expect("store")
                .work_session_state(&project, &session, at(4))
                .expect("session")
                .focused_work_id,
            Some(second.work_id)
        );
    }

    #[test]
    fn core_committed_handoff_recovery_uses_the_durable_focus_basis() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("committed-handoff-focus".into());
        let grant = install_protocol_grant(&database, &project, "agent");
        let session = SessionId("committed-handoff-session".into());
        let service = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            session.clone(),
            Some("protocol-test".into()),
            Some(grant),
        );
        let first = match service
            .work_propose(root_input("Handoff original", "handoff-first"), at(0))
            .expect("first root")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        service
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(300),
                    recovery_reason: None,
                    idempotency_key: "handoff-claim".into(),
                },
                at(1),
            )
            .expect("claim original focus");
        let input = WorkHandoffInput::Offer {
            to: "peer".into(),
            ttl_seconds: Some(300),
            checkpoint_summary: "durable handoff checkpoint".into(),
            idempotency_key: "committed-offer".into(),
        };

        let mut store = SqliteStore::open(&database).expect("store");
        let basis = service
            .protocol_basis(&store, true, true, at(2))
            .expect("original handoff basis");
        let intent = service.protocol_intent(&input);
        store
            .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
                project_id: &project,
                session_id: &session,
                operation: "work_handoff:offer",
                idempotency_key: "committed-offer",
                intent: &intent,
                basis: &basis,
                now: at(2),
            })
            .expect("begin handoff attempt");
        let original = basis.focused_work.clone().expect("original focus");
        let scoped_key = service
            .core_operation_key(
                "work_handoff:offer",
                "committed-offer",
                "offer_work_handoff",
            )
            .expect("scoped operation key");
        service
            .execute_work_handoff(
                &mut store,
                &basis,
                &original,
                input.clone(),
                scoped_key,
                at(2),
            )
            .expect("commit core handoff without protocol result");
        drop(store);

        let second = match service
            .work_propose(root_input("Handoff new focus", "handoff-second"), at(3))
            .expect("second root")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        let recovered = service
            .work_handoff(input.clone(), at(4))
            .expect("recover core-committed handoff");
        assert_eq!(recovered.receipt.work_id, first.work_id);
        assert_ne!(recovered.receipt.work_id, second.work_id);
        let replayed = service
            .work_handoff(input, at(5))
            .expect("replay exact handoff result");
        assert_eq!(
            serde_json::to_vec(&replayed).expect("serialize replay"),
            serde_json::to_vec(&recovered).expect("serialize recovery")
        );
        assert_eq!(
            SqliteStore::open(&database)
                .expect("store")
                .work_session_state(&project, &session, at(5))
                .expect("session")
                .focused_work_id,
            Some(second.work_id)
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the lost-response regression keeps guessed acknowledgement, replay, and exact-token recovery in one auditable scenario"
    )]
    fn unseen_staged_delivery_must_be_replayed_before_focus_can_change() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("lost-delivery-focus".into());
        let grant = install_protocol_grant(&database, &project, "agent");
        let session = SessionId("lost-delivery-session".into());
        let service = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            session.clone(),
            Some("protocol-test".into()),
            Some(grant.clone()),
        );
        let peer = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            SessionId("lost-delivery-peer".into()),
            Some("protocol-test".into()),
            Some(grant),
        );
        service
            .work_propose(root_input("Original delivery focus", "lost-first"), at(0))
            .expect("original root");
        let target = match peer
            .work_propose(root_input("Later focus target", "lost-second"), at(1))
            .expect("target root")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };

        let unseen = service
            .work_next(
                20,
                WorkNextQuery {
                    sections: vec![WorkNextSection::Changes],
                    ..WorkNextQuery::default()
                },
                at(2),
            )
            .expect("stage page whose response is lost");
        let head = unseen.delivered_through.expect("staged page boundary");
        let expected_changes =
            serde_json::to_value(&unseen.changes).expect("serialize staged changes");
        let expected_token = unseen
            .delivery_token
            .clone()
            .expect("staged delivery token");
        drop(unseen);

        let mut rebound = SqliteStore::open(&database).expect("legacy task binding store");
        rebound
            .start_task(
                &project,
                "dummy:REBOUND-AFTER-STAGE",
                "Rebind after exact page staging",
                &session,
                service.actor("task_start", "exercise exact staged page replay"),
                at(3),
            )
            .expect("bind a legacy task after staging");
        drop(rebound);

        assert!(matches!(
            service.work_focus(&target.short_ref, at(3)),
            Err(StoreError::PendingWorkDelivery)
        ));
        let restarted = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            session.clone(),
            Some("protocol-test".into()),
            Some(install_protocol_grant(&database, &project, "agent")),
        );
        let replayed = restarted
            .work_next(
                20,
                WorkNextQuery {
                    sections: vec![WorkNextSection::Changes],
                    ..WorkNextQuery::default()
                },
                at(4),
            )
            .expect("replay the unseen staged page");
        assert_eq!(replayed.delivered_through, Some(head));
        assert_eq!(
            serde_json::to_value(&replayed.changes).expect("serialize replayed changes"),
            expected_changes
        );
        let delivery_token = replayed
            .delivery_token
            .clone()
            .expect("replayed delivery token");
        assert_eq!(delivery_token, expected_token);
        assert_eq!(replayed.session.confirmed_project_cursor, 0);
        let positions = replayed
            .changes
            .as_ref()
            .expect("replayed changes")
            .iter()
            .map(|change| change.entry.position.position)
            .collect::<Vec<_>>();
        assert_eq!(positions, (1..=head).collect::<Vec<_>>());

        let wrong_cursor = service
            .work_next_with_delivery_token(
                20,
                Some(head + 1_000),
                Some("wrong-token"),
                WorkNextQuery {
                    sections: vec![WorkNextSection::Focus],
                    ..WorkNextQuery::default()
                },
                at(5),
            )
            .expect_err("a guessed cursor and token cannot acknowledge a page");
        let wrong_cursor_message = wrong_cursor.to_string();
        assert!(!wrong_cursor_message.contains(&delivery_token));
        assert!(!wrong_cursor_message.contains(&(head + 1_000).to_string()));

        let wrong_token = service
            .work_next_with_delivery_token(
                20,
                Some(head),
                Some("wrong-token"),
                WorkNextQuery {
                    sections: vec![WorkNextSection::Focus],
                    ..WorkNextQuery::default()
                },
                at(5),
            )
            .expect_err("the staged cursor alone cannot acknowledge a page");
        assert!(!wrong_token.to_string().contains(&delivery_token));
        let still_pending = SqliteStore::open(&database)
            .expect("store after rejected acknowledgements")
            .work_session_state(&project, &session, at(5))
            .expect("pending state");
        assert_eq!(still_pending.project_cursor, 0);
        assert_eq!(still_pending.tentative_project_cursor, Some(head));
        assert_eq!(
            still_pending.tentative_delivery_token.as_deref(),
            Some(delivery_token.as_str())
        );

        let replayed_again = restarted
            .work_next(
                20,
                WorkNextQuery {
                    sections: vec![WorkNextSection::Changes],
                    ..WorkNextQuery::default()
                },
                at(5),
            )
            .expect("replay after rejected acknowledgement");
        assert_eq!(replayed_again.delivered_through, Some(head));
        assert_eq!(
            replayed_again.delivery_token.as_deref(),
            Some(delivery_token.as_str())
        );

        let cleared = service
            .work_next_with_delivery_token(
                20,
                Some(head),
                Some(delivery_token.as_str()),
                WorkNextQuery {
                    sections: vec![WorkNextSection::Focus],
                    ..WorkNextQuery::default()
                },
                at(6),
            )
            .expect("acknowledge only with the replayed cursor and token");
        assert_eq!(cleared.delivered_through, None);
        assert_eq!(cleared.session.confirmed_project_cursor, head);
        assert!(!cleared.session.pending_delivery);
        assert_eq!(
            service
                .work_focus(&target.short_ref, at(7))
                .expect("focus after safe acknowledgement")
                .status
                .work
                .work_id,
            target.work_id
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the two-thread regression keeps both competing pages and the durable winner visible"
    )]
    fn concurrent_same_session_delivery_returns_only_the_winning_exact_page() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("concurrent-delivery-cas".into());
        let grant = install_protocol_grant(&database, &project, "agent");
        let session = SessionId("shared-session".into());
        let service = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            session.clone(),
            Some("protocol-test".into()),
            Some(grant.clone()),
        );
        let peer = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            SessionId("seed-peer".into()),
            Some("protocol-test".into()),
            Some(grant),
        );
        service
            .work_propose(root_input("Shared focus", "shared-focus"), at(0))
            .expect("focused root");
        for index in 0..4 {
            peer.work_propose(
                root_input(
                    &format!("Concurrent source {index}"),
                    &format!("concurrent-source-{index}"),
                ),
                at(index + 1),
            )
            .expect("seed project event");
        }

        let entered = Arc::new(Barrier::new(3));
        let release = Arc::new(Barrier::new(3));
        let hook = DeliveryStageTestHook {
            entered: entered.clone(),
            release: release.clone(),
        };
        let mut short = service.clone();
        short.delivery_stage_hook = Some(hook.clone());
        let mut long = service.clone();
        long.delivery_stage_hook = Some(hook);
        let short_call = std::thread::spawn(move || {
            short.work_next(
                1,
                WorkNextQuery {
                    sections: vec![WorkNextSection::Changes],
                    ..WorkNextQuery::default()
                },
                at(10),
            )
        });
        let long_call = std::thread::spawn(move || {
            long.work_next(
                20,
                WorkNextQuery {
                    sections: vec![WorkNextSection::Changes],
                    ..WorkNextQuery::default()
                },
                at(10),
            )
        });
        entered.wait();
        release.wait();
        let short_response = short_call
            .join()
            .expect("short thread")
            .expect("short page");
        let long_response = long_call.join().expect("long thread").expect("long page");

        assert_eq!(
            short_response.delivered_through,
            long_response.delivered_through
        );
        assert_eq!(short_response.delivery_token, long_response.delivery_token);
        assert_eq!(
            serde_json::to_value(&short_response.changes).expect("short changes"),
            serde_json::to_value(&long_response.changes).expect("long changes")
        );
        let store = SqliteStore::open(&database).expect("durable delivery store");
        let state = store
            .work_session_state(&project, &session, at(11))
            .expect("durable delivery state");
        assert_eq!(
            state.tentative_project_cursor,
            short_response.delivered_through
        );
        assert_eq!(
            state.tentative_delivery_token,
            short_response.delivery_token
        );
        let durable: StagedWorkChangePage = store
            .staged_work_session_delivery_payload(&project, &session)
            .expect("durable payload")
            .expect("pending payload")
            .decode()
            .expect("decode durable payload");
        assert_eq!(
            serde_json::to_value(&durable.changes).expect("durable changes"),
            serde_json::to_value(&short_response.changes).expect("response changes")
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the deterministic focus race keeps the losing projection and durable reprojected page in one scenario"
    )]
    fn focus_winning_before_delivery_stage_forces_reprojection() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("focus-delivery-cas".into());
        let grant = install_protocol_grant(&database, &project, "agent");
        let session = SessionId("focus-race-session".into());
        let service = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            session.clone(),
            Some("protocol-test".into()),
            Some(grant.clone()),
        );
        let peer = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            SessionId("focus-race-peer".into()),
            Some("protocol-test".into()),
            Some(grant),
        );
        let original = match service
            .work_propose(root_input("Original focus", "focus-race-original"), at(0))
            .expect("original root")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        let replacement = match peer
            .work_propose(
                root_input("Replacement focus", "focus-race-replacement"),
                at(1),
            )
            .expect("replacement root")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        let mut memories = SqliteStore::open(&database).expect("memory store");
        for (work_id, prose, key, second, actor) in [
            (
                original.work_id,
                "original-root memory",
                "focus-race-original-memory",
                2,
                service.actor("memory_note", "seed original focus-sensitive delta"),
            ),
            (
                replacement.work_id,
                "replacement-root memory",
                "focus-race-replacement-memory",
                3,
                peer.actor("memory_note", "seed replacement focus-sensitive delta"),
            ),
        ] {
            memories
                .capture_note(
                    &crate::NoteRequest {
                        project_id: project.clone(),
                        task_id: None,
                        work_id: Some(work_id),
                        prose: prose.into(),
                        visibility: crate::NoteVisibility::Shared,
                        kind: None,
                        authority: None,
                        sensitivity: Some(Sensitivity::Internal),
                        title: None,
                        tags: Vec::new(),
                        evidence: Vec::new(),
                        refs: Vec::new(),
                        actor,
                        idempotency_key: key.into(),
                        created_at: at(second),
                    },
                    &DevelopmentNoopRedactor,
                )
                .expect("capture focus-sensitive memory");
        }
        drop(memories);

        let entered = Arc::new(Barrier::new(2));
        let release = Arc::new(Barrier::new(2));
        let mut delivery = service.clone();
        delivery.delivery_stage_hook = Some(DeliveryStageTestHook {
            entered: entered.clone(),
            release: release.clone(),
        });
        let delivery_call = std::thread::spawn(move || {
            delivery.work_next(
                20,
                WorkNextQuery {
                    sections: vec![WorkNextSection::Changes],
                    ..WorkNextQuery::default()
                },
                at(3),
            )
        });
        entered.wait();
        service
            .work_focus(&replacement.short_ref, at(3))
            .expect("focus wins before staging");
        release.wait();
        let response = delivery_call
            .join()
            .expect("delivery thread")
            .expect("delivery reprojects after focus CAS loss");
        assert_eq!(response.session.focused_work_id, Some(replacement.work_id));
        let memory_changes = response
            .changes
            .as_ref()
            .expect("changes")
            .iter()
            .filter(|change| change.entry.object_kind == "memory_version")
            .collect::<Vec<_>>();
        assert_eq!(memory_changes.len(), 2);
        assert!(matches!(
            &memory_changes[0].delivery,
            WorkChangeProjection::Omitted(WorkChangeOmission {
                omission: WorkChangeOmissionReason::OutsideFocusedRoot,
                ..
            })
        ));
        assert!(matches!(
            &memory_changes[1].delivery,
            WorkChangeProjection::Visible(summary)
                if summary.work_id == Some(replacement.work_id)
        ));

        let store = SqliteStore::open(&database).expect("durable delivery store");
        let state = store
            .work_session_state(&project, &session, at(4))
            .expect("durable state");
        assert_eq!(state.focused_work_id, Some(replacement.work_id));
        assert_eq!(state.tentative_project_cursor, response.delivered_through);
        assert_eq!(state.tentative_delivery_token, response.delivery_token);
        let durable: StagedWorkChangePage = store
            .staged_work_session_delivery_payload(&project, &session)
            .expect("durable payload")
            .expect("pending payload")
            .decode()
            .expect("decode durable payload");
        assert_eq!(
            serde_json::to_value(&durable.changes).expect("durable changes"),
            serde_json::to_value(&response.changes).expect("response changes")
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the table-driven failure-atomicity regression verifies every caller-controlled acceptance shape against all durable completion substeps"
    )]
    fn capture_completion_rejects_bad_acceptance_without_substeps() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("completion-prevalidation".into());
        let grant = install_protocol_grant(&database, &project, "agent");
        let service = LocalWorkService::new(
            database.clone(),
            project,
            "agent".into(),
            SessionId("completion-prevalidation-session".into()),
            Some("protocol-test".into()),
            Some(grant),
        );
        let root = match service
            .work_propose(
                WorkProposeInput::Root {
                    title: "Prevalidate completion".into(),
                    outcome: "Invalid acceptance never writes capture substeps".into(),
                    acceptance: vec!["criterion one".into(), "criterion two".into()],
                    work_kind: None,
                    priority: None,
                    labels: Vec::new(),
                    assigned_to: None,
                    deferred_until: None,
                    authority_policy_ref: None,
                    idempotency_key: "prevalidation-root".into(),
                },
                at(0),
            )
            .expect("root proposal")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        service
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(300),
                    recovery_reason: None,
                    idempotency_key: "prevalidation-claim".into(),
                },
                at(1),
            )
            .expect("claim focused work");
        let run_id = root.active_run_id.expect("active run");
        let accepted =
            |criterion: &str, satisfied: bool, evidence: Vec<String>| WorkAcceptanceInput {
                criterion: Some(criterion.into()),
                satisfied,
                evidence,
                note: "prevalidation fixture".into(),
            };
        let cases = vec![
            ("missing", vec![accepted("criterion one", true, Vec::new())]),
            (
                "duplicate",
                vec![
                    accepted("criterion one", true, Vec::new()),
                    accepted("criterion one", true, Vec::new()),
                ],
            ),
            (
                "unknown",
                vec![
                    accepted("criterion one", true, Vec::new()),
                    accepted("unknown criterion", true, Vec::new()),
                ],
            ),
            (
                "unsatisfied",
                vec![
                    accepted("criterion one", true, Vec::new()),
                    accepted("criterion two", false, Vec::new()),
                ],
            ),
            (
                "malformed-evidence",
                vec![
                    accepted("criterion one", true, vec!["not-a-hash".into()]),
                    accepted("criterion two", true, Vec::new()),
                ],
            ),
        ];

        for (index, (name, acceptance)) in cases.into_iter().enumerate() {
            let key = format!("prevalidation-{name}");
            let before = SqliteStore::open(&database).expect("before store");
            let before_evidence = before.work_run_evidence(run_id).expect("before evidence");
            let before_checkpoint = before
                .get_work_run(run_id)
                .expect("before run")
                .last_checkpoint;
            let before_head = before
                .work_feed_head(&FeedId::RunExecution(run_id))
                .expect("before feed head");
            drop(before);

            let input = WorkCompleteInput {
                capture: Some(WorkCompletionCaptureInput {
                    summary: format!("capture must not commit for {name}"),
                    refs: vec![format!("test:{name}")],
                }),
                evidence: Vec::new(),
                acceptance,
                idempotency_key: key.clone(),
            };
            assert!(
                service
                    .work_complete(
                        input.clone(),
                        at(2 + i64::try_from(index).expect("bounded case index")),
                    )
                    .is_err()
            );

            let after = SqliteStore::open(&database).expect("after store");
            assert_eq!(
                after.work_run_evidence(run_id).expect("after evidence"),
                before_evidence
            );
            assert_eq!(
                after
                    .get_work_run(run_id)
                    .expect("after run")
                    .last_checkpoint,
                before_checkpoint
            );
            assert_eq!(
                after
                    .work_feed_head(&FeedId::RunExecution(run_id))
                    .expect("after feed head"),
                before_head
            );
            for operation in ["record_work_evidence", "checkpoint_work"] {
                let scoped = service
                    .core_operation_key("work_complete", &key, operation)
                    .expect("scoped substep key");
                assert!(
                    after
                        .work_operation_result_value(operation, &scoped)
                        .expect("substep lookup")
                        .is_none()
                );
            }
        }
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the crash-replay regression seeds each committed completion substep under the exact durable protocol attempt"
    )]
    fn capture_completion_replays_after_evidence_or_checkpoint_commit() {
        for (scenario, checkpoint_committed, claim_expires_before_retry) in [
            ("evidence", false, false),
            ("checkpoint", true, false),
            ("expired-claim", false, true),
        ] {
            let directory = tempdir().expect("temp directory");
            let database = directory.path().join("engram.sqlite3");
            let project = ProjectId(format!("completion-replay-{scenario}"));
            let grant = install_protocol_grant(&database, &project, "agent");
            let session = SessionId("completion-session".into());
            let service = LocalWorkService::new(
                database.clone(),
                project.clone(),
                "agent".into(),
                session.clone(),
                Some("protocol-test".into()),
                Some(grant),
            );
            let root = match service
                .work_propose(
                    root_input("Crash-safe completion", "completion-root"),
                    at(0),
                )
                .expect("root proposal")
            {
                WorkProposeResult::Root { work, .. } => work,
                WorkProposeResult::Decomposition(_) => panic!("expected root"),
            };
            service
                .work_update(
                    WorkUpdateInput::Claim {
                        ttl_seconds: Some(if claim_expires_before_retry { 2 } else { 300 }),
                        recovery_reason: None,
                        idempotency_key: "completion-claim".into(),
                    },
                    at(1),
                )
                .expect("claim focused work");
            let input = WorkCompleteInput {
                capture: Some(WorkCompletionCaptureInput {
                    summary: "completion evidence was durably captured".into(),
                    refs: vec!["test:completion-replay".into()],
                }),
                evidence: Vec::new(),
                acceptance: vec![WorkAcceptanceInput {
                    criterion: None,
                    satisfied: true,
                    evidence: Vec::new(),
                    note: "the crash-replay path was verified".into(),
                }],
                idempotency_key: "crash-safe-completion".into(),
            };

            let mut store = SqliteStore::open(&database).expect("store");
            let basis = service
                .protocol_basis(&store, true, false, at(2))
                .expect("completion basis");
            let intent = service.protocol_intent(&input);
            store
                .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
                    project_id: &project,
                    session_id: &session,
                    operation: "work_complete",
                    idempotency_key: &input.idempotency_key,
                    intent: &intent,
                    basis: &basis,
                    now: at(2),
                })
                .expect("durable completion attempt");
            let work = basis.focused_work.clone().expect("focused work");
            assert_eq!(work.work_id, root.work_id);
            let claim = service
                .live_protocol_claim(&basis, &work, at(2))
                .expect("live completion claim");
            let capture = input.capture.as_ref().expect("completion capture");
            let evidence = store
                .record_work_evidence(
                    &RecordWorkEvidenceRequest {
                        work_id: work.work_id,
                        run_id: claim.run_id,
                        expected_work_revision: work.revision,
                        holder: session.clone(),
                        claim_id: claim.claim_id,
                        claim_fence: claim.fence,
                        summary: capture.summary.clone(),
                        refs: capture.refs.clone(),
                        actor: service.actor(
                            "work_complete",
                            "capture completion evidence for ambient local work",
                        ),
                        idempotency_key: service
                            .core_operation_key(
                                "work_complete",
                                &input.idempotency_key,
                                "record_work_evidence",
                            )
                            .expect("evidence key"),
                        recorded_at: at(2),
                    },
                    &DevelopmentNoopRedactor,
                )
                .expect("committed evidence substep");
            if checkpoint_committed {
                store
                    .checkpoint_work(
                        &CheckpointWorkRequest {
                            work_id: work.work_id,
                            run_id: claim.run_id,
                            expected_work_revision: work.revision,
                            holder: session.clone(),
                            claim_id: claim.claim_id,
                            claim_fence: claim.fence,
                            summary: capture.summary.clone(),
                            evidence: vec![evidence],
                            actor: service.actor(
                                "work_complete",
                                "checkpoint the exact completion evidence cut",
                            ),
                            idempotency_key: service
                                .core_operation_key(
                                    "work_complete",
                                    &input.idempotency_key,
                                    "checkpoint_work",
                                )
                                .expect("checkpoint key"),
                            checkpointed_at: at(2),
                        },
                        &DevelopmentNoopRedactor,
                    )
                    .expect("committed checkpoint substep");
            }
            drop(store);

            if claim_expires_before_retry {
                assert!(matches!(
                    service.work_complete(input.clone(), at(4)),
                    Err(StoreError::WorkClaimMismatch { .. })
                ));
                let store = SqliteStore::open(&database).expect("inspect refused retry");
                let checkpoint_key = service
                    .core_operation_key("work_complete", &input.idempotency_key, "checkpoint_work")
                    .expect("checkpoint key");
                assert!(
                    store
                        .work_operation_result_value("checkpoint_work", &checkpoint_key)
                        .expect("checkpoint lookup")
                        .is_none(),
                    "an expired retry must not commit its still-missing checkpoint"
                );
                continue;
            }

            let completed = service
                .work_complete(input.clone(), at(3))
                .expect("retry resumes the durable attempt");
            let WorkCompleteResult::Completed(completed) = completed else {
                panic!("retry must complete work");
            };
            assert_eq!(completed.work_id, root.work_id);
            assert_eq!(completed.completed_at, at(3));
            let replay = service
                .work_complete(input.clone(), at(4))
                .expect("completed outer attempt replays");
            let WorkCompleteResult::Completed(replay) = replay else {
                panic!("completed outer attempt must replay completion");
            };
            assert_eq!(replay.seal, completed.seal);
            assert_eq!(replay.completed_at, at(3));
        }
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the regression proves contradiction capture, delivery acknowledgement, and restart integrity as one scenario"
    )]
    fn work_scoped_contradiction_drains_through_work_next_and_doctor() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("contradiction-delivery".into());
        let grant = install_protocol_grant(&database, &project, "agent");
        let session = SessionId("contradiction-session".into());
        let service = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            session.clone(),
            Some("protocol-test".into()),
            Some(grant),
        );
        let root = match service
            .work_propose(
                root_input("Contradiction delivery", "contradiction-root"),
                at(0),
            )
            .expect("root proposal")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        let mut store = SqliteStore::open(&database).expect("store");
        let task = store
            .start_task(
                &project,
                "dummy:MIXED-CONTRADICTION",
                "Mixed contradiction applicability",
                &session,
                service.actor("task_start", "bind mixed contradiction task"),
                at(1),
            )
            .expect("task binding")
            .task;
        let left = store
            .capture_note(
                &crate::NoteRequest {
                    project_id: project.clone(),
                    task_id: None,
                    work_id: Some(root.work_id),
                    prose: "Constraint: use the first mutually exclusive work rule".into(),
                    visibility: crate::NoteVisibility::Shared,
                    kind: None,
                    authority: None,
                    sensitivity: None,
                    title: None,
                    tags: Vec::new(),
                    evidence: Vec::new(),
                    refs: Vec::new(),
                    actor: service.actor("memory_note", "capture first work rule"),
                    idempotency_key: "contradiction-left".into(),
                    created_at: at(1),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("left note");
        let right = store
            .capture_note(
                &crate::NoteRequest {
                    project_id: project.clone(),
                    task_id: None,
                    work_id: Some(root.work_id),
                    prose: "Constraint: use the second mutually exclusive work rule".into(),
                    visibility: crate::NoteVisibility::Shared,
                    kind: None,
                    authority: None,
                    sensitivity: None,
                    title: None,
                    tags: Vec::new(),
                    evidence: Vec::new(),
                    refs: Vec::new(),
                    actor: service.actor("memory_note", "capture second work rule"),
                    idempotency_key: "contradiction-right".into(),
                    created_at: at(2),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("right note");
        let project_memory = store
            .capture_note(
                &crate::NoteRequest {
                    project_id: project.clone(),
                    task_id: None,
                    work_id: None,
                    prose: "Project-wide constraint for mixed contradiction".into(),
                    visibility: crate::NoteVisibility::Shared,
                    kind: None,
                    authority: None,
                    sensitivity: None,
                    title: None,
                    tags: Vec::new(),
                    evidence: Vec::new(),
                    refs: Vec::new(),
                    actor: service.actor("memory_note", "capture project constraint"),
                    idempotency_key: "contradiction-project".into(),
                    created_at: at(3),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("project note");
        let task_memory = store
            .capture_note(
                &crate::NoteRequest {
                    project_id: project.clone(),
                    task_id: Some(task.task_id),
                    work_id: None,
                    prose: "Task constraint for mixed contradiction".into(),
                    visibility: crate::NoteVisibility::Shared,
                    kind: None,
                    authority: None,
                    sensitivity: None,
                    title: None,
                    tags: Vec::new(),
                    evidence: Vec::new(),
                    refs: Vec::new(),
                    actor: service.actor("memory_note", "capture task constraint"),
                    idempotency_key: "contradiction-task".into(),
                    created_at: at(4),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("task note");
        let contradiction = store
            .record_memory_contradiction(
                &project,
                None,
                Some(root.work_id),
                &session,
                "agent",
                &left.version,
                &right.version,
                "the two work rules cannot both guide execution",
                "contradiction-edge",
                service.actor("memory_contradict", "record explicit work contradiction"),
                at(5),
            )
            .expect("contradiction");
        let project_contradiction = store
            .record_memory_contradiction(
                &project,
                None,
                Some(root.work_id),
                &session,
                "agent",
                &left.version,
                &project_memory.version,
                "work and project guidance conflict",
                "contradiction-work-project",
                service.actor("memory_contradict", "record mixed project contradiction"),
                at(6),
            )
            .expect("work and project contradiction");
        let task_contradiction = store
            .record_memory_contradiction(
                &project,
                Some(task.task_id),
                Some(root.work_id),
                &session,
                "agent",
                &right.version,
                &task_memory.version,
                "work and task guidance conflict",
                "contradiction-work-task",
                service.actor("memory_contradict", "record mixed task contradiction"),
                at(7),
            )
            .expect("work and task contradiction");
        assert!(!contradiction.work_positions.is_empty());
        assert!(!project_contradiction.work_positions.is_empty());
        assert!(!task_contradiction.work_positions.is_empty());
        assert!(store.verify_all().expect("integrity report").is_healthy());
        drop(store);

        let page = service
            .work_next(100, WorkNextQuery::default(), at(8))
            .expect("deliver contradiction event");
        for hash in [
            &contradiction.contradiction,
            &project_contradiction.contradiction,
            &task_contradiction.contradiction,
        ] {
            assert!(
                page.changes
                    .as_ref()
                    .expect("changes")
                    .iter()
                    .any(|change| {
                        change.entry.object_kind == "memory_contradiction_event"
                            && &change.entry.object_hash == hash
                            && matches!(change.delivery, WorkChangeProjection::Visible(_))
                    })
            );
        }
        let delivered = page.delivered_through.expect("delivered cursor");
        let delivery_token = page.delivery_token.expect("delivery token");
        let acknowledged = service
            .work_next_with_delivery_token(
                100,
                Some(delivered),
                Some(delivery_token.as_str()),
                WorkNextQuery::default(),
                at(9),
            )
            .expect("acknowledge contradiction page");
        assert_eq!(acknowledged.session.confirmed_project_cursor, delivered);
        assert!(
            SqliteStore::open(&database)
                .expect("reopen store")
                .verify_all()
                .expect("integrity report after delivery")
                .is_healthy()
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the confidentiality regression covers visible, restricted, and cross-root memory feed pairs"
    )]
    fn work_next_redacts_restricted_and_out_of_root_memory_without_cursor_gaps() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("feed-boundary-project".into());
        let grant = install_protocol_grant(&database, &project, "agent");
        let focused = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            SessionId("focused-session".into()),
            Some("protocol-test".into()),
            Some(grant.clone()),
        );
        let peer = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            SessionId("peer-session".into()),
            Some("protocol-test".into()),
            Some(grant),
        );
        let focused_root = match focused
            .work_propose(root_input("Focused root", "focused-root"), at(0))
            .expect("focused root proposal")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected focused root"),
        };
        let peer_root = match peer
            .work_propose(root_input("Peer root", "peer-root"), at(1))
            .expect("peer root proposal")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected peer root"),
        };
        let mut store = SqliteStore::open(&database).expect("store");
        let (visible, restricted, outside, outside_second) = {
            let mut capture = |work_id: WorkId,
                               prose: &str,
                               sensitivity: Sensitivity,
                               key: &str,
                               actor: ActorContext,
                               captured_at: DateTime<Utc>| {
                store
                    .capture_note(
                        &crate::NoteRequest {
                            project_id: project.clone(),
                            task_id: None,
                            work_id: Some(work_id),
                            prose: prose.into(),
                            visibility: crate::NoteVisibility::Shared,
                            kind: None,
                            authority: None,
                            sensitivity: Some(sensitivity),
                            title: None,
                            tags: Vec::new(),
                            evidence: Vec::new(),
                            refs: Vec::new(),
                            actor,
                            idempotency_key: key.into(),
                            created_at: captured_at,
                        },
                        &DevelopmentNoopRedactor,
                    )
                    .expect("capture work memory")
            };
            let visible = capture(
                focused_root.work_id,
                "visible focused-root memory",
                Sensitivity::Internal,
                "visible-memory",
                focused.actor("memory_note", "capture focused-root memory"),
                at(2),
            );
            let restricted = capture(
                focused_root.work_id,
                "restricted focused-root secret",
                Sensitivity::Restricted,
                "restricted-memory",
                focused.actor("memory_note", "capture restricted focused-root memory"),
                at(3),
            );
            let outside = capture(
                peer_root.work_id,
                "unrelated root memory",
                Sensitivity::Internal,
                "outside-memory",
                peer.actor("memory_note", "capture peer-root memory"),
                at(4),
            );
            let outside_second = capture(
                peer_root.work_id,
                "second unrelated root memory",
                Sensitivity::Internal,
                "outside-memory-second",
                peer.actor("memory_note", "capture second peer-root memory"),
                at(5),
            );
            (visible, restricted, outside, outside_second)
        };
        let peer_contradiction = store
            .record_memory_contradiction(
                &project,
                None,
                Some(peer_root.work_id),
                &peer.session_id,
                "agent",
                &outside.version,
                &outside_second.version,
                "peer-root contradiction must remain outside focused delivery",
                "peer-root-contradiction",
                peer.actor("memory_contradict", "record peer-root contradiction"),
                at(6),
            )
            .expect("peer-root contradiction");
        let restricted_contradiction = MemoryContradictionEvent {
            schema_version: SCHEMA_VERSION,
            project_id: Some(project.clone()),
            task_id: None,
            work_root_id: Some(focused_root.root_id),
            left_version: visible.version.clone(),
            right_version: restricted.version.clone(),
            reason: "restricted contradiction payload".into(),
            actor: focused.actor("memory_contradict", "exercise restricted projection"),
            created_at: at(7),
        };
        assert!(matches!(
            agent_change_object(
                &store,
                &project,
                Some(focused_root.root_id),
                None,
                "memory_contradiction_event",
                serde_json::to_value(restricted_contradiction)
                    .expect("serialize restricted contradiction"),
            )
            .expect("restricted contradiction projection"),
            WorkChangeProjection::Omitted(WorkChangeOmission {
                omission: WorkChangeOmissionReason::RestrictedSensitivity,
                ..
            })
        ));
        drop(store);

        let page = focused
            .work_next(100, WorkNextQuery::default(), at(8))
            .expect("bounded project delta");
        let delivered = page.delivered_through.expect("delivered cursor");
        let changes = page.changes.as_ref().expect("changes section");
        assert_eq!(
            i64::try_from(changes.len()).expect("change count"),
            delivered - page.session.confirmed_project_cursor
        );
        let projection_for = |hash: &ObjectHash| {
            &changes
                .iter()
                .find(|change| &change.entry.object_hash == hash)
                .expect("feed object")
                .delivery
        };
        assert!(matches!(
            projection_for(&visible.version),
            WorkChangeProjection::Visible(value) if value.change_kind == "memory_version"
        ));
        for hash in [&restricted.version, &restricted.assertion] {
            assert!(matches!(
                projection_for(hash),
                WorkChangeProjection::Omitted(WorkChangeOmission {
                    omission: WorkChangeOmissionReason::RestrictedSensitivity,
                    ..
                })
            ));
        }
        for hash in [&outside.version, &outside.assertion] {
            assert!(matches!(
                projection_for(hash),
                WorkChangeProjection::Omitted(WorkChangeOmission {
                    omission: WorkChangeOmissionReason::OutsideFocusedRoot,
                    ..
                })
            ));
        }
        assert!(matches!(
            projection_for(&peer_contradiction.contradiction),
            WorkChangeProjection::Omitted(WorkChangeOmission {
                omission: WorkChangeOmissionReason::OutsideFocusedRoot,
                ..
            })
        ));
        let serialized = serde_json::to_string(&page).expect("serialize work_next page");
        assert!(serialized.contains("visible focused-root memory"));
        assert!(!serialized.contains("restricted focused-root secret"));
        assert!(!serialized.contains("unrelated root memory"));
        assert!(!serialized.contains("second unrelated root memory"));
        assert!(
            !serialized.contains("peer-root contradiction must remain outside focused delivery")
        );
        let acknowledged = focused
            .work_next_with_delivery_token(
                100,
                Some(delivered),
                page.delivery_token.as_deref(),
                WorkNextQuery::default(),
                at(9),
            )
            .expect("acknowledge protected delta");
        assert_eq!(acknowledged.session.confirmed_project_cursor, delivered);
        assert!(
            SqliteStore::open(&database)
                .expect("reopen store")
                .verify_all()
                .expect("integrity report")
                .is_healthy()
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one end-to-end scenario demonstrates that no lifecycle identifiers are shuttled between protocol calls"
    )]
    fn ambient_protocol_runs_root_claim_evidence_handoff_and_completion() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("protocol-project".into());
        let grant = install_protocol_grant(&database, &project, "agent");
        let a = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            SessionId("session-a".into()),
            Some("protocol-test".into()),
            Some(grant.clone()),
        );
        let b = LocalWorkService::new(
            database.clone(),
            project,
            "agent".into(),
            SessionId("session-b".into()),
            Some("protocol-test".into()),
            Some(grant.clone()),
        );

        let root = match a
            .work_propose(
                WorkProposeInput::Root {
                    title: "Ship ambient work".into(),
                    outcome: "The six-operation protocol works end to end".into(),
                    acceptance: vec!["handoff completion is sealed".into()],
                    work_kind: Some(WorkItemKind::Feature),
                    priority: Some(1),
                    labels: vec!["protocol".into()],
                    assigned_to: None,
                    deferred_until: None,
                    authority_policy_ref: None,
                    idempotency_key: "root".into(),
                },
                at(0),
            )
            .expect("root proposal")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        let first = a
            .work_next(20, WorkNextQuery::default(), at(1))
            .expect("first ambient page");
        assert_eq!(first.session.focused_work_id, Some(root.work_id));
        let first_delivered = first.delivered_through.expect("first delivered cursor");
        let first_delivery_token = first.delivery_token.clone().expect("first delivery token");
        assert!(first_delivered > 0);
        assert!(
            !serde_json::to_string(&first.changes)
                .expect("serialize agent changes")
                .contains(grant.as_str())
        );
        assert_eq!(first.session.confirmed_project_cursor, 0);
        assert!(first.session.pending_delivery);
        let concurrent = match b
            .work_propose(
                WorkProposeInput::Root {
                    title: "Concurrent project event".into(),
                    outcome: "Appending after delivery does not change the staged page".into(),
                    acceptance: vec!["event is durable".into()],
                    work_kind: None,
                    priority: None,
                    labels: Vec::new(),
                    assigned_to: None,
                    deferred_until: None,
                    authority_policy_ref: None,
                    idempotency_key: "concurrent-root".into(),
                },
                at(2),
            )
            .expect("append after another session staged a page")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected concurrent root"),
        };
        let blocked_focus_switch = a.work_focus(&concurrent.short_ref, at(3));
        assert!(matches!(
            blocked_focus_switch,
            Err(StoreError::PendingWorkDelivery)
        ));
        let replayed = a
            .work_next(20, WorkNextQuery::default(), at(3))
            .expect("unacknowledged page replays");
        assert_eq!(replayed.delivered_through, first.delivered_through);
        assert_eq!(replayed.delivery_token, first.delivery_token);
        assert_eq!(
            replayed
                .changes
                .as_ref()
                .expect("replayed changes")
                .iter()
                .map(|change| &change.entry.object_hash)
                .collect::<Vec<_>>(),
            first
                .changes
                .as_ref()
                .expect("first changes")
                .iter()
                .map(|change| &change.entry.object_hash)
                .collect::<Vec<_>>()
        );
        let acknowledged = a
            .work_next_with_delivery_token(
                20,
                Some(first_delivered),
                Some(first_delivery_token.as_str()),
                WorkNextQuery::default(),
                at(4),
            )
            .expect("delivery acknowledgement");
        assert_eq!(
            acknowledged.session.confirmed_project_cursor,
            first_delivered
        );
        let acknowledged_delivered = acknowledged
            .delivered_through
            .expect("acknowledged delivered cursor");
        assert!(acknowledged.session.pending_delivery);
        assert!(acknowledged_delivered > first_delivered);
        assert!(!acknowledged.changes.as_ref().expect("changes").is_empty());
        let acknowledgement_replay = a
            .work_next_with_delivery_token(
                20,
                Some(first_delivered),
                Some(first_delivery_token.as_str()),
                WorkNextQuery::default(),
                at(5),
            )
            .expect("lost ack-and-fetch response replays the newer staged page");
        assert_eq!(
            acknowledgement_replay.delivered_through,
            acknowledged.delivered_through
        );
        assert_eq!(
            acknowledgement_replay.delivery_token,
            acknowledged.delivery_token
        );
        assert_eq!(
            acknowledgement_replay
                .changes
                .as_ref()
                .expect("ack replay changes")
                .iter()
                .map(|change| &change.entry.object_hash)
                .collect::<Vec<_>>(),
            acknowledged
                .changes
                .as_ref()
                .expect("acknowledged changes")
                .iter()
                .map(|change| &change.entry.object_hash)
                .collect::<Vec<_>>()
        );
        assert!(
            first
                .focus
                .expect("focus")
                .allowed_next
                .contains(&"work_update:claim".into())
        );

        let claim_input = WorkUpdateInput::Claim {
            ttl_seconds: Some(300),
            recovery_reason: None,
            idempotency_key: "claim-a".into(),
        };
        let claimed = a.work_update(claim_input.clone(), at(4)).expect("claim");
        let control_binding = claimed
            .receipt
            .control_binding
            .as_ref()
            .expect("claim receipt exposes a paste-ready control binding");
        assert_eq!(control_binding.work_id, root.work_id);
        assert_eq!(control_binding.work_revision, claimed.receipt.revision);
        let claimed_focus = a
            .work_focus(&root.short_ref, at(4))
            .expect("claimed focus exposes the same control binding");
        assert_eq!(
            claimed_focus.control_binding.as_ref(),
            Some(control_binding)
        );
        assert_eq!(
            claimed_focus
                .run
                .as_ref()
                .expect("claimed run")
                .root_execution_id,
            control_binding.root_execution_id
        );
        assert_eq!(
            claimed_focus
                .claim
                .as_ref()
                .expect("claimed focus claim")
                .claim_id,
            control_binding.claim_id
        );
        a.work_next_with_delivery_token(
            20,
            Some(acknowledged_delivered),
            acknowledged.delivery_token.as_deref(),
            WorkNextQuery {
                sections: vec![WorkNextSection::Focus],
                ..WorkNextQuery::default()
            },
            at(6),
        )
        .expect("acknowledge pending delivery without staging another page");
        a.work_focus(&concurrent.short_ref, at(7))
            .expect("focus may change after delivery acknowledgement");
        let claim_replay = a
            .work_update(claim_input, at(40))
            .expect("lost-response claim replay");
        assert_eq!(
            serde_json::to_value(&claim_replay).expect("serialize replay"),
            serde_json::to_value(&claimed).expect("serialize original")
        );
        a.work_focus(&root.short_ref, at(41))
            .expect("restore original work focus");
        let attempt_connection = rusqlite::Connection::open(&database).expect("attempt store");
        let (basis_json, result_hash, result_json) = attempt_connection
            .query_row(
                "SELECT basis_json, result_hash, result_json FROM work_protocol_attempts
                 WHERE project_id = ?1 AND session_id = ?2
                   AND operation = 'work_update:claim' AND idempotency_key = 'claim-a'",
                rusqlite::params!["protocol-project", "session-a"],
                |row| {
                    Ok((
                        row.get::<_, Option<Vec<u8>>>(0)?,
                        row.get::<_, Option<String>>(1)?,
                        row.get::<_, Vec<u8>>(2)?,
                    ))
                },
            )
            .expect("compacted attempt");
        assert!(basis_json.is_none());
        assert!(result_hash.is_some());
        let exact_result: serde_json::Value =
            serde_json::from_slice(&result_json).expect("exact replay JSON");
        assert_eq!(
            exact_result,
            serde_json::to_value(&claimed).expect("serialize exact replay basis")
        );
        assert!(
            !serde_json::to_string(&exact_result)
                .expect("serialize exact result")
                .contains(grant.as_str())
        );
        drop(attempt_connection);
        let conflict = a.work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(301),
                recovery_reason: None,
                idempotency_key: "claim-a".into(),
            },
            at(41),
        );
        assert!(matches!(
            conflict,
            Err(StoreError::WorkOperationIdempotencyConflict { .. })
        ));
        let evidence = a
            .work_update(
                WorkUpdateInput::Evidence {
                    summary: "protocol lifecycle test passed".into(),
                    refs: vec!["test:ambient-protocol".into()],
                    attach: None,
                    idempotency_key: "evidence-a".into(),
                },
                at(42),
            )
            .expect("evidence")
            .receipt
            .result
            .as_str()
            .expect("evidence hash")
            .to_owned();
        a.work_handoff(
            WorkHandoffInput::Offer {
                to: "session-b".into(),
                ttl_seconds: Some(200),
                checkpoint_summary: "handoff after evidence capture".into(),
                idempotency_key: "offer-b".into(),
            },
            at(43),
        )
        .expect("offer");

        let focused = b
            .work_focus(&root.short_ref, at(44))
            .expect("recipient focus");
        assert!(focused.allowed_next.contains(&"work_handoff:accept".into()));
        b.work_handoff(
            WorkHandoffInput::Accept {
                idempotency_key: "accept-b".into(),
            },
            at(45),
        )
        .expect("accept");
        b.work_update(
            WorkUpdateInput::Checkpoint {
                summary: "recipient validated evidence and acceptance".into(),
                evidence: vec![evidence],
                idempotency_key: "checkpoint-b".into(),
            },
            at(46),
        )
        .expect("recipient checkpoint");
        let seal = b
            .work_complete(
                WorkCompleteInput {
                    capture: None,
                    evidence: Vec::new(),
                    acceptance: vec![WorkAcceptanceInput {
                        criterion: None,
                        satisfied: true,
                        evidence: Vec::new(),
                        note: "verified by the receiving session".into(),
                    }],
                    idempotency_key: "complete-b".into(),
                },
                at(47),
            )
            .expect("complete after handoff");
        let WorkCompleteResult::Completed(seal) = seal else {
            panic!("handoff completion must seal work");
        };
        assert_eq!(seal.work_id, root.work_id);
        let stored_seal = SqliteStore::open(&database)
            .expect("store")
            .get::<CompletionSeal>(&seal.seal)
            .expect("read seal")
            .expect("canonical completion seal");
        assert_eq!(stored_seal.expected_contributors.len(), 2);
        assert_eq!(
            stored_seal
                .contributions
                .iter()
                .map(|contribution| &contribution.participant)
                .collect::<std::collections::HashSet<_>>()
                .len(),
            2
        );
        let completed = b
            .work_focus(&root.short_ref, at(48))
            .expect("completed focus");
        assert_eq!(completed.status.work.lifecycle, WorkLifecycle::Completed);

        let tamper = rusqlite::Connection::open(&database).expect("tamper store");
        tamper
            .execute(
                "UPDATE work_protocol_attempts
                 SET result_json = CAST('{\"operation\":\"claim\",\"receipt\":{\"work_id\":\"00000000-0000-0000-0000-000000000000\"},\"obligations\":[],\"allowed_next\":[]}' AS BLOB)
                 WHERE project_id = 'protocol-project' AND session_id = 'session-a'
                   AND operation = 'work_update:claim' AND idempotency_key = 'claim-a'",
                [],
            )
            .expect("tamper compact replay bytes");
        let (projection_bytes, canonical_bytes) = tamper
            .query_row(
                "SELECT attempt.result_json, object.canonical_json
                 FROM work_protocol_attempts attempt
                 JOIN objects object ON object.object_hash = attempt.result_hash
                 WHERE attempt.project_id = 'protocol-project'
                   AND attempt.session_id = 'session-a'
                   AND attempt.operation = 'work_update:claim'
                   AND attempt.idempotency_key = 'claim-a'",
                [],
                |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .expect("tampered projection and canonical result");
        assert_ne!(projection_bytes, canonical_bytes);
        let canonical_replay: serde_json::Value =
            serde_json::from_slice(&canonical_bytes).expect("canonical replay JSON");
        assert_ne!(
            canonical_replay
                .pointer("/receipt/work_id")
                .and_then(serde_json::Value::as_str),
            Some("00000000-0000-0000-0000-000000000000")
        );
        drop(tamper);
        let tampered_replay = a.work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "claim-a".into(),
            },
            at(49),
        );
        assert!(
            matches!(tampered_replay, Err(StoreError::InvalidWorkProjection(_))),
            "unexpected tampered replay result: {tampered_replay:?}"
        );
        assert!(
            !SqliteStore::open(&database)
                .expect("doctor store")
                .verify_all()
                .expect("doctor")
                .invalid_work_records
                .is_empty()
        );
        let repair = rusqlite::Connection::open(&database).expect("repair store");
        repair
            .execute(
                "UPDATE work_protocol_attempts SET result_json = ?1
                 WHERE project_id = 'protocol-project' AND session_id = 'session-a'
                   AND operation = 'work_update:claim' AND idempotency_key = 'claim-a'",
                [canonical_bytes],
            )
            .expect("restore exact replay projection");
        drop(repair);
        assert!(
            SqliteStore::open(&database)
                .expect("repaired doctor store")
                .verify_all()
                .expect("repaired doctor")
                .is_healthy()
        );
        assert!(
            completed
                .allowed_next
                .contains(&"work_update:reopen".into())
        );
        b.work_update(
            WorkUpdateInput::Reopen {
                reason: "verify honest non-success disposition".into(),
                idempotency_key: "reopen-for-cancel".into(),
            },
            at(49),
        )
        .expect("reopen before cancellation");
        let cancelled = b
            .work_update(
                WorkUpdateInput::Cancel {
                    reason: "the reopened experiment is no longer needed".into(),
                    idempotency_key: "cancel-root".into(),
                },
                at(50),
            )
            .expect("cancel without false completion");
        assert_eq!(cancelled.receipt.work_id, root.work_id);
        assert_eq!(
            cancelled.receipt.result.get("lifecycle"),
            Some(&serde_json::json!("cancelled"))
        );

        let replacement = match b
            .work_propose(
                WorkProposeInput::Root {
                    title: "Replacement approach".into(),
                    outcome: "A better local execution plan is tracked".into(),
                    acceptance: vec!["replacement is evaluated".into()],
                    work_kind: Some(WorkItemKind::Research),
                    priority: Some(1),
                    labels: Vec::new(),
                    assigned_to: None,
                    deferred_until: None,
                    authority_policy_ref: None,
                    idempotency_key: "replacement-root".into(),
                },
                at(51),
            )
            .expect("replacement root")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected replacement root"),
        };
        let obsolete = match b
            .work_propose(
                WorkProposeInput::Root {
                    title: "Obsolete approach".into(),
                    outcome: "This plan is explicitly superseded".into(),
                    acceptance: vec!["obsolete plan is not completed".into()],
                    work_kind: Some(WorkItemKind::Research),
                    priority: Some(2),
                    labels: Vec::new(),
                    assigned_to: None,
                    deferred_until: None,
                    authority_policy_ref: None,
                    idempotency_key: "obsolete-root".into(),
                },
                at(52),
            )
            .expect("obsolete root")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected obsolete root"),
        };
        let superseded = b
            .work_update(
                WorkUpdateInput::Supersede {
                    replacement: replacement.short_ref,
                    reason: "replacement captures the revised plan".into(),
                    idempotency_key: "supersede-obsolete".into(),
                },
                at(53),
            )
            .expect("supersede without false completion");
        assert_eq!(superseded.receipt.work_id, obsolete.work_id);
        assert_eq!(
            superseded.receipt.result.get("lifecycle"),
            Some(&serde_json::json!("superseded"))
        );
        assert_eq!(
            superseded.receipt.result.get("superseded_by"),
            Some(&serde_json::json!(replacement.work_id))
        );
        let catalog = b
            .work_next(
                10,
                WorkNextQuery {
                    search: Some("obsolete".into()),
                    lifecycles: vec![WorkLifecycle::Superseded],
                    ..WorkNextQuery::default()
                },
                at(54),
            )
            .expect("search superseded work");
        let catalog_items = &catalog.catalog.as_ref().expect("catalog").items;
        assert_eq!(catalog_items.len(), 1);
        assert_eq!(catalog_items[0].work.work_id, obsolete.work_id);
        assert_eq!(
            catalog
                .focus
                .expect("query preserves ambient focus")
                .status
                .work
                .work_id,
            obsolete.work_id
        );
        let cancelled_catalog = b
            .work_next(
                10,
                WorkNextQuery {
                    lifecycles: vec![WorkLifecycle::Cancelled],
                    ..WorkNextQuery::default()
                },
                at(55),
            )
            .expect("list cancelled work");
        assert!(
            cancelled_catalog
                .catalog
                .as_ref()
                .expect("cancelled catalog")
                .items
                .iter()
                .any(|item| item.work.work_id == root.work_id)
        );
    }

    #[test]
    fn allowed_next_distinguishes_ordinary_claim_from_attributed_recovery() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("claim-recovery-guidance".into());
        let grant = install_protocol_grant(&database, &project, "agent");
        let first = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            SessionId("first-holder".into()),
            Some("protocol-test".into()),
            Some(grant.clone()),
        );
        let successor = LocalWorkService::new(
            database,
            project,
            "agent".into(),
            SessionId("successor".into()),
            Some("protocol-test".into()),
            Some(grant),
        );
        let root = match first
            .work_propose(
                root_input("Recovery guidance", "recovery-guidance-root"),
                at(0),
            )
            .expect("root proposal")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        first
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(2),
                    recovery_reason: None,
                    idempotency_key: "first-claim".into(),
                },
                at(1),
            )
            .expect("initial claim");

        let guidance = successor
            .work_focus(&root.short_ref, at(4))
            .expect("focus after prior claim expiry");
        assert!(
            guidance
                .allowed_next
                .contains(&"work_update:claim(recovery_reason_required)".into())
        );
        assert!(!guidance.allowed_next.contains(&"work_update:claim".into()));
        successor
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(60),
                    recovery_reason: Some("prior executor stopped before checkpointing".into()),
                    idempotency_key: "successor-recovery".into(),
                },
                at(4),
            )
            .expect("typed recovery guidance maps to an executable claim");
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the end-to-end regression keeps parent, child-scope, waiver, and refresh assertions in one lifecycle"
    )]
    fn required_child_waiver_guidance_is_exact_and_carries_an_actionable_child() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("waiver-guidance".into());
        let grant = install_protocol_grant(&database, &project, "agent");
        let service = LocalWorkService::new(
            database,
            project,
            "agent".into(),
            SessionId("waiver-session".into()),
            Some("protocol-test".into()),
            Some(grant),
        );
        let (root, fresh_focus) = match service
            .work_propose(root_input("Waiver guidance", "waiver-root"), at(0))
            .expect("root proposal")
        {
            WorkProposeResult::Root { work, focus } => (work, focus),
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        assert!(
            !fresh_focus
                .allowed_next
                .contains(&"work_update:waive_required_child".into())
        );
        assert!(fresh_focus.waivable_required_children.is_empty());

        let decomposition = service
            .work_propose(
                WorkProposeInput::Decompose {
                    children: ["disposed", "open"]
                        .into_iter()
                        .map(|key| WorkChildInput {
                            key: key.into(),
                            title: format!("{key} child"),
                            outcome: format!("{key} outcome"),
                            acceptance: vec![format!("{key} accepted")],
                            requirement: Some(ChildRequirement::Required),
                            kind: Some(WorkItemKind::Task),
                            priority: Some(1),
                            labels: Vec::new(),
                            assigned_to: None,
                            deferred_until: None,
                        })
                        .collect(),
                    prerequisites: Vec::new(),
                    idempotency_key: "waiver-decompose".into(),
                },
                at(1),
            )
            .expect("decompose root");
        let WorkProposeResult::Decomposition(decomposition) = decomposition else {
            panic!("expected decomposition");
        };
        let disposed = decomposition.children[0].clone();
        service
            .work_focus(&disposed.short_ref, at(2))
            .expect("focus required child");
        service
            .work_update(
                WorkUpdateInput::Cancel {
                    reason: "child outcome is deliberately omitted".into(),
                    idempotency_key: "cancel-required-child".into(),
                },
                at(3),
            )
            .expect("cancel required child");

        let install_scoped_waiver = |scope: WorkAuthorityScope| {
            SqliteStore::open(&service.database)
                .expect("scoped grant store")
                .install_work_authority_grant(
                    WorkAuthorityGrant {
                        schema_version: SCHEMA_VERSION,
                        project_id: service.project_id.clone(),
                        policy_ref: "project-default".into(),
                        subject_actor_id: "agent".into(),
                        issued_by: ActorContext {
                            actor_id: "test-host".into(),
                            actor_kind: "host_operator".into(),
                            assurance: AssuranceLevel::Asserted,
                            run_id: None,
                            session_id: None,
                            source_tool: Some("test".into()),
                            source_skill: None,
                            provenance_chain: Vec::new(),
                            reason: "issue scoped waiver authority".into(),
                        },
                        assurance: AssuranceLevel::Asserted,
                        operations: vec![WorkAuthorityOperation::CompletionWaiver],
                        scope,
                        planning_budget: None,
                        issued_at: at(0),
                        valid_until: at(3_600),
                        reason: "test exact child-scoped waiver guidance".into(),
                    },
                    &DevelopmentNoopRedactor,
                )
                .expect("install scoped waiver grant")
        };
        let parent_grant = install_scoped_waiver(WorkAuthorityScope::Work(root.work_id));
        let child_grant = install_scoped_waiver(WorkAuthorityScope::Work(disposed.work_id));
        let parent_scoped = LocalWorkService::new(
            service.database.clone(),
            service.project_id.clone(),
            "agent".into(),
            SessionId("parent-scope".into()),
            Some("protocol-test".into()),
            Some(parent_grant),
        )
        .work_focus(&root.short_ref, at(4))
        .expect("parent-scoped guidance");
        assert!(
            !parent_scoped
                .allowed_next
                .contains(&"work_update:waive_required_child".into())
        );
        let child_scoped = LocalWorkService::new(
            service.database.clone(),
            service.project_id.clone(),
            "agent".into(),
            SessionId("child-scope".into()),
            Some("protocol-test".into()),
            Some(child_grant),
        )
        .work_focus(&root.short_ref, at(4))
        .expect("child-scoped guidance");
        assert!(
            child_scoped
                .allowed_next
                .contains(&"work_update:waive_required_child".into())
        );
        assert_eq!(child_scoped.waivable_required_children.len(), 1);

        let parent = service
            .work_focus(&root.short_ref, at(4))
            .expect("focus parent with one waivable child");
        assert!(
            parent
                .allowed_next
                .contains(&"work_update:waive_required_child".into())
        );
        assert_eq!(parent.waivable_required_children.len(), 1);
        assert_eq!(
            parent.waivable_required_children[0].work_id,
            disposed.work_id
        );
        assert_eq!(
            parent.waivable_required_children[0].lifecycle,
            WorkLifecycle::Cancelled
        );
        service
            .work_update(
                WorkUpdateInput::WaiveRequiredChild {
                    child: disposed.short_ref,
                    reason: "the omission is explicit and accepted".into(),
                    idempotency_key: "waive-required-child".into(),
                },
                at(5),
            )
            .expect("execute advertised waiver");
        let refreshed = service
            .work_focus(&root.short_ref, at(6))
            .expect("refresh parent after waiver");
        assert!(
            !refreshed
                .allowed_next
                .contains(&"work_update:waive_required_child".into())
        );
        assert!(refreshed.waivable_required_children.is_empty());
    }

    #[test]
    fn maximum_fanout_decomposition_receipt_is_bounded_and_replays_exactly() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("maximum-fanout".into());
        let grant = install_protocol_grant_with_budget(
            &database,
            &project,
            "agent",
            WorkPlanningBudget {
                max_depth: 4,
                max_open_descendants: 64,
                max_children_per_decomposition: 64,
            },
        );
        let service = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            SessionId("fanout-session".into()),
            Some("protocol-test".into()),
            Some(grant.clone()),
        );
        service
            .work_propose(root_input("Maximum fanout", "fanout-root"), at(0))
            .expect("root proposal");
        let input = WorkProposeInput::Decompose {
            children: (0..64)
                .map(|index| WorkChildInput {
                    key: format!("child-{index:02}"),
                    title: format!("Child {index:02} {}", "x".repeat(256)),
                    outcome: format!("Outcome {index:02} {}", "y".repeat(256)),
                    acceptance: vec![format!("Acceptance {index:02} {}", "z".repeat(256))],
                    requirement: Some(ChildRequirement::Required),
                    kind: Some(WorkItemKind::Task),
                    priority: Some(1),
                    labels: vec![format!("label-{index:02}-{}", "q".repeat(128))],
                    assigned_to: None,
                    deferred_until: None,
                })
                .collect(),
            prerequisites: Vec::new(),
            idempotency_key: "fanout-decompose".into(),
        };
        let first = service
            .work_propose(input.clone(), at(1))
            .expect("maximum decomposition");
        let WorkProposeResult::Decomposition(summary) = &first else {
            panic!("expected decomposition");
        };
        assert_eq!(summary.child_count, 64);
        assert_eq!(summary.children.len(), 64);
        assert!(summary.details_omitted);
        assert_eq!(
            summary
                .children
                .iter()
                .map(|child| child.work_id)
                .collect::<std::collections::HashSet<_>>()
                .len(),
            64
        );
        assert!(
            serde_json::to_vec(&first)
                .expect("serialize first receipt")
                .len()
                <= MAX_AGENT_WORK_RESPONSE_BYTES
        );

        let restarted = LocalWorkService::new(
            database.clone(),
            project,
            "agent".into(),
            SessionId("fanout-session".into()),
            Some("protocol-test".into()),
            Some(grant),
        );
        let replay = restarted
            .work_propose(input, at(2))
            .expect("exact replay after restart");
        assert_eq!(
            serde_json::to_value(&replay).expect("replay JSON"),
            serde_json::to_value(&first).expect("first JSON")
        );
        let connection = rusqlite::Connection::open(database).expect("inspect replay store");
        let stored: Vec<u8> = connection
            .query_row(
                "SELECT result_json FROM work_protocol_attempts
                 WHERE project_id = 'maximum-fanout'
                   AND session_id = 'fanout-session'
                   AND operation = 'work_propose:decompose'
                   AND idempotency_key = 'fanout-decompose'",
                [],
                |row| row.get(0),
            )
            .expect("durable bounded result");
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&stored).expect("stored result JSON"),
            serde_json::to_value(first).expect("first result JSON")
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one scale regression creates, selects, replays, and densely drains the complete bounded protocol scenario"
    )]
    fn work_next_is_byte_bounded_dense_and_section_selective_at_project_scale() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("bounded-work-project".into());
        let grant = install_protocol_grant(&database, &project, "agent");
        let writer = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            SessionId("writer".into()),
            Some("protocol-test".into()),
            Some(grant.clone()),
        );
        let reader = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            SessionId("reader".into()),
            Some("protocol-test".into()),
            Some(grant),
        );

        let mut work_ids = Vec::new();
        for item_index in 0..50 {
            let work = match writer
                .work_propose(
                    root_input(
                        &format!("Bounded item {item_index:02}"),
                        &format!("bounded-root-{item_index:02}"),
                    ),
                    at(i64::from(item_index) * 10),
                )
                .expect("create bounded root")
            {
                WorkProposeResult::Root { work, .. } => work,
                WorkProposeResult::Decomposition(_) => panic!("expected root"),
            };
            work_ids.push(work.work_id);
        }
        let mut event_store = SqliteStore::open(&database).expect("event store");
        for (item_index, work_id) in work_ids.iter().enumerate() {
            let entry = event_store
                .work_event_tail(*work_id, 1)
                .expect("base event tail")
                .pop()
                .expect("base event");
            let base = event_store
                .get::<WorkEvent>(&entry.object_hash)
                .expect("load base event")
                .expect("base event object");
            for event_index in 0..9 {
                let mut event = base.clone();
                event.created_at = at(1_000
                    + i64::try_from(item_index).expect("item index") * 10
                    + i64::from(event_index));
                event.actor.reason = format!("bounded synthetic event {event_index}");
                event_store
                    .append_test_work_event(&event)
                    .expect("append canonical scale event");
            }
        }

        let initial_head = SqliteStore::open(&database)
            .expect("store")
            .work_feed_head(&FeedId::Project(project.clone()))
            .expect("project feed head");
        assert_eq!(initial_head, 500);

        crate::storage::reset_work_event_decode_count();
        let first = reader
            .work_next(1_000, WorkNextQuery::default(), at(600))
            .expect("bounded default work_next");
        let first_decode_count = crate::storage::work_event_decode_count();
        assert!(
            first_decode_count < 200,
            "bounded work_next decoded {first_decode_count} canonical work events"
        );
        assert!(
            serde_json::to_vec(&first)
                .expect("serialize first page")
                .len()
                <= MAX_AGENT_WORK_RESPONSE_BYTES
        );
        let first_cursor = first.delivered_through.expect("first delivery cursor");
        let first_hashes = first
            .changes
            .as_ref()
            .expect("default changes")
            .iter()
            .map(|change| change.entry.object_hash.clone())
            .collect::<Vec<_>>();

        crate::storage::reset_work_event_decode_count();
        let mutation = writer
            .work_update(
                WorkUpdateInput::Revise {
                    patch: WorkRevisionPatch {
                        title: Some(format!("Post-history mutation {}", "x".repeat(300))),
                        ..WorkRevisionPatch::default()
                    },
                    idempotency_key: "post-history-mutation".into(),
                },
                at(1_600),
            )
            .expect("mutation after long history");
        let mutation_decode_count = crate::storage::work_event_decode_count();
        assert!(
            mutation_decode_count < 50,
            "target mutation decoded {mutation_decode_count} canonical work events"
        );
        let mutation_bytes = serde_json::to_vec(&mutation).expect("serialize mutation result");
        assert!(
            mutation_bytes.len() < 2_048,
            "mutation response was {} bytes",
            mutation_bytes.len()
        );
        assert!(
            !String::from_utf8(mutation_bytes)
                .expect("UTF-8 response")
                .contains("history")
        );
        let head = initial_head + 1;

        let catalog_only = reader
            .work_next(
                50,
                WorkNextQuery {
                    sections: vec![WorkNextSection::Catalog],
                    ..WorkNextQuery::default()
                },
                at(601),
            )
            .expect("catalog-only page");
        assert!(catalog_only.changes.is_none());
        assert!(catalog_only.delivered_through.is_none());
        assert_eq!(catalog_only.session.confirmed_project_cursor, 0);
        assert!(catalog_only.session.pending_delivery);
        assert!(
            serde_json::to_vec(&catalog_only)
                .expect("serialize catalog page")
                .len()
                <= MAX_AGENT_WORK_RESPONSE_BYTES
        );

        let replay = reader
            .work_next(
                1_000,
                WorkNextQuery {
                    sections: vec![WorkNextSection::Changes],
                    ..WorkNextQuery::default()
                },
                at(602),
            )
            .expect("replay staged changes");
        assert_eq!(replay.delivered_through, Some(first_cursor));
        assert_eq!(
            replay
                .changes
                .as_ref()
                .expect("replayed changes")
                .iter()
                .map(|change| change.entry.object_hash.clone())
                .collect::<Vec<_>>(),
            first_hashes
        );

        let mut expected_position = 1_i64;
        let mut acknowledge = None;
        let mut acknowledge_token = None;
        loop {
            let page = reader
                .work_next_with_delivery_token(
                    1_000,
                    acknowledge,
                    acknowledge_token.as_deref(),
                    WorkNextQuery {
                        sections: vec![WorkNextSection::Changes],
                        ..WorkNextQuery::default()
                    },
                    at(603 + expected_position),
                )
                .expect("drain bounded changes");
            let delivered = page.delivered_through.expect("delivery cursor");
            let delivery_token = page.delivery_token.clone().expect("delivery token");
            let changes = page.changes.as_ref().expect("changes");
            assert!(
                serde_json::to_vec(&page)
                    .expect("serialize delta page")
                    .len()
                    <= MAX_AGENT_WORK_RESPONSE_BYTES
            );
            for change in changes {
                assert_eq!(change.entry.position.position, expected_position);
                expected_position += 1;
            }
            if delivered == head {
                reader
                    .work_next_with_delivery_token(
                        1,
                        Some(delivered),
                        Some(delivery_token.as_str()),
                        WorkNextQuery {
                            sections: vec![WorkNextSection::Catalog],
                            ..WorkNextQuery::default()
                        },
                        at(1_200),
                    )
                    .expect("acknowledge final page without staging more changes");
                break;
            }
            acknowledge = Some(delivered);
            acknowledge_token = Some(delivery_token);
        }
        assert_eq!(expected_position, head + 1);
    }
}
