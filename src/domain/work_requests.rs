//! Request records for creating, planning, claiming, checkpointing, handing
//! off, evidencing, completing, and disposing of local work.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};

use crate::ObjectId;

use super::{
    AcceptanceResult, ActorContext, ChildRequirement, CompletionDrainAttestation, ProjectId,
    SessionId, WorkBlockerKind, WorkClaim, WorkClaimId, WorkHandoffOfferId, WorkId, WorkItem,
    WorkItemKind, WorkObligationId, WorkOrigin, WorkPlanningAuthority, WorkRunId,
};

/// Request to create a root or child work item.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CreateWorkRequest {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_ref: Option<String>,
    /// Ordered initial observations, committed atomically with creation.
    pub notes: Vec<String>,
    pub project_id: ProjectId,
    pub parent_id: Option<WorkId>,
    pub child_requirement: ChildRequirement,
    pub title: String,
    pub outcome: String,
    pub acceptance: Vec<String>,
    /// Criteria bound to typed verification requirements, by one-based position.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acceptance_bindings: Vec<super::AcceptanceBinding>,
    pub kind: WorkItemKind,
    pub priority: i32,
    pub labels: Vec<String>,
    pub assigned_to: Option<String>,
    pub deferred_until: Option<DateTime<Utc>>,
    /// The acceptance-evaluation mode this task pins from creation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluation_mode: Option<super::AcceptanceEvaluationMode>,
    pub origin: WorkOrigin,
    pub source_snapshot_id: Option<ObjectId>,
    pub actor: ActorContext,
    pub idempotency_key: String,
    pub created_at: DateTime<Utc>,
}

/// One direct child proposed during an atomic decomposition.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChildWorkDraft {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_ref: Option<String>,
    /// Ordered initial observations, committed atomically with decomposition.
    pub notes: Vec<String>,
    pub local_key: String,
    pub child_requirement: ChildRequirement,
    pub title: String,
    pub outcome: String,
    pub acceptance: Vec<String>,
    /// Criteria bound to typed verification requirements, by one-based position.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acceptance_bindings: Vec<super::AcceptanceBinding>,
    pub kind: WorkItemKind,
    pub priority: i32,
    pub labels: Vec<String>,
    pub assigned_to: Option<String>,
    pub deferred_until: Option<DateTime<Utc>>,
    /// The acceptance-evaluation mode this child pins from creation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluation_mode: Option<super::AcceptanceEvaluationMode>,
}

/// Reference to existing work or another child in the same decomposition.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "value", rename_all = "snake_case")]
pub enum WorkDependencyRef {
    Existing(WorkId),
    Proposed(String),
}

/// One prerequisite edge admitted atomically with proposed children.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChildWorkPrerequisite {
    pub work_key: String,
    pub prerequisite: WorkDependencyRef,
}

/// Bounded direct-child decomposition request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DecomposeWorkRequest {
    pub parent_id: WorkId,
    pub expected_parent_revision: i64,
    pub children: Vec<ChildWorkDraft>,
    pub prerequisites: Vec<ChildWorkPrerequisite>,
    pub authority: WorkPlanningAuthority,
    pub actor: ActorContext,
    pub idempotency_key: String,
    pub created_at: DateTime<Utc>,
}

/// Atomic decomposition result.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkDecomposition {
    pub parent: WorkItem,
    pub children: Vec<WorkItem>,
}

/// Optimistic patch for durable planning fields.
#[derive(Clone, Debug, Default, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "each clear flag pairs with one optional field of the same audited revision patch"
)]
pub struct WorkRevisionPatch {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_ref: Option<String>,
    /// Remove planning linkage through the ordinary audited revision path.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_external: bool,
    pub title: Option<String>,
    pub outcome: Option<String>,
    pub acceptance: Option<Vec<String>>,
    /// Replace the criteria bound to typed verification requirements. Omitted
    /// with `acceptance` replaced, the bindings are cleared, since positions
    /// name the old list; omitted otherwise, they are unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance_bindings: Option<Vec<super::AcceptanceBinding>>,
    pub kind: Option<WorkItemKind>,
    pub priority: Option<i32>,
    pub labels: Option<Vec<String>>,
    #[serde(default)]
    pub add_labels: Vec<String>,
    #[serde(default)]
    pub remove_labels: Vec<String>,
    pub assigned_to: Option<String>,
    #[serde(default)]
    pub clear_assignment: bool,
    pub deferred_until: Option<DateTime<Utc>>,
    #[serde(default)]
    pub clear_deferral: bool,
    /// Select the acceptance-evaluation mode this task requires.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluation_mode: Option<super::AcceptanceEvaluationMode>,
    /// Return the task to "any policy-allowed mode".
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub clear_evaluation_mode: bool,
}

/// Optimistic work revision request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReviseWorkRequest {
    pub work_id: WorkId,
    pub expected_revision: i64,
    pub patch: WorkRevisionPatch,
    pub authority: WorkPlanningAuthority,
    pub actor: ActorContext,
    pub idempotency_key: String,
    pub updated_at: DateTime<Utc>,
}

/// Optimistic request to add or remove one explicit completion prerequisite.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChangeWorkPrerequisiteRequest {
    pub work_id: WorkId,
    pub prerequisite_id: WorkId,
    pub expected_revision: i64,
    pub authority: WorkPlanningAuthority,
    pub actor: ActorContext,
    pub idempotency_key: String,
    pub changed_at: DateTime<Utc>,
}

/// Request to add one typed manual blocker.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AddWorkBlockerRequest {
    pub work_id: WorkId,
    pub expected_work_revision: i64,
    pub kind: WorkBlockerKind,
    pub detail: String,
    pub authority: WorkPlanningAuthority,
    pub actor: ActorContext,
    pub idempotency_key: String,
    pub blocked_at: DateTime<Utc>,
}

/// Request to resolve a live manual blocker.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ClearWorkBlockerRequest {
    pub work_id: WorkId,
    pub expected_work_revision: i64,
    pub blocker_id: String,
    pub authority: WorkPlanningAuthority,
    pub actor: ActorContext,
    pub idempotency_key: String,
    pub cleared_at: DateTime<Utc>,
}

/// Request to acquire or recover the current run claim.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ClaimWorkRequest {
    pub work_id: WorkId,
    pub expected_work_revision: i64,
    /// Existing active run expected by the caller. Restored work has no live
    /// run until its first post-load claim and therefore expects `None`.
    pub expected_run_id: Option<WorkRunId>,
    pub holder: SessionId,
    pub ttl_seconds: i64,
    pub recovery_reason: Option<String>,
    pub actor: ActorContext,
    pub idempotency_key: String,
    pub claimed_at: DateTime<Utc>,
}

/// Request to select one parent's next ready direct child by derived
/// readiness and the priority-then-id order of `ls --ready`, and claim it in
/// the same write transaction.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ClaimNextReadyChildRequest {
    pub parent_id: WorkId,
    pub holder: SessionId,
    pub ttl_seconds: i64,
    /// Attributed reason that lets the selection take over a ready child
    /// whose prior claim lapsed under an unaccounted holder; without it such
    /// a child is passed over.
    pub recovery_reason: Option<String>,
    pub actor: ActorContext,
    pub idempotency_key: String,
    pub claimed_at: DateTime<Utc>,
}

/// One child selected and claimed under its parent.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NextReadyChildClaim {
    pub parent_id: WorkId,
    pub work_id: WorkId,
    pub claim: WorkClaim,
    /// One-based place of the claimed child in the parent's verified ready
    /// order at selection; absent when the call renewed a child this holder
    /// already held.
    pub position: Option<usize>,
    /// Ready direct children at the call. A fresh selection reports the
    /// canonically verified ones, the child it claimed included. A renewal
    /// reports the projection's count of ready siblings, which is advisory: a
    /// renewal keeps a live claim alive and reads no sibling to do it.
    pub ready_count: usize,
    pub renewed: bool,
}

/// Request to release live responsibility without completing the run.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReleaseWorkRequest {
    pub work_id: WorkId,
    pub run_id: WorkRunId,
    pub expected_work_revision: i64,
    pub holder: SessionId,
    pub claim_id: WorkClaimId,
    pub claim_fence: i64,
    pub reason: String,
    pub waiver_reason: Option<String>,
    pub actor: ActorContext,
    pub idempotency_key: String,
    pub released_at: DateTime<Utc>,
}

/// Request to capture execution progress under an exact claim fence.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CheckpointWorkRequest {
    pub work_id: WorkId,
    pub run_id: WorkRunId,
    pub expected_work_revision: i64,
    pub holder: SessionId,
    pub claim_id: WorkClaimId,
    pub claim_fence: i64,
    pub summary: String,
    /// Explicit evidence selection. `None` snapshots every evidence object
    /// already attached to the run inside the checkpoint transaction, while
    /// `Some([])` deliberately acknowledges none.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evidence: Option<Vec<ObjectId>>,
    pub actor: ActorContext,
    pub idempotency_key: String,
    pub checkpointed_at: DateTime<Utc>,
}

/// Offer a handoff while coupling it to the outgoing holder's checkpoint.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OfferWorkHandoffRequest {
    pub work_id: WorkId,
    pub run_id: WorkRunId,
    pub expected_work_revision: i64,
    pub from: SessionId,
    pub to: SessionId,
    pub claim_id: WorkClaimId,
    pub claim_fence: i64,
    pub ttl_seconds: i64,
    pub checkpoint_summary: String,
    pub actor: ActorContext,
    pub idempotency_key: String,
    pub offered_at: DateTime<Utc>,
}

/// Accept a pending handoff and advance the claim fence atomically.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AcceptWorkHandoffRequest {
    pub work_id: WorkId,
    pub offer_id: WorkHandoffOfferId,
    pub to: SessionId,
    pub actor: ActorContext,
    pub idempotency_key: String,
    pub accepted_at: DateTime<Utc>,
}

/// Withdraw a pending handoff while the outgoing claim is still authoritative.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CancelWorkHandoffRequest {
    pub work_id: WorkId,
    pub run_id: WorkRunId,
    pub expected_work_revision: i64,
    pub holder: SessionId,
    pub offer_id: WorkHandoffOfferId,
    pub claim_id: WorkClaimId,
    pub claim_fence: i64,
    pub reason: String,
    pub actor: ActorContext,
    pub idempotency_key: String,
    pub cancelled_at: DateTime<Utc>,
}

/// Request to attach one immutable evidence object to the live run.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RecordWorkEvidenceRequest {
    pub work_id: WorkId,
    pub run_id: WorkRunId,
    pub expected_work_revision: i64,
    pub holder: SessionId,
    pub claim_id: WorkClaimId,
    pub claim_fence: i64,
    pub summary: String,
    pub refs: Vec<String>,
    pub actor: ActorContext,
    pub idempotency_key: String,
    pub recorded_at: DateTime<Utc>,
}

/// Request to capture one note as evidence plus the checkpoint that
/// acknowledges it. Storage commits both immutable objects atomically.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct RecordWorkNoteRequest {
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub status: bool,
    pub work_id: WorkId,
    pub run_id: WorkRunId,
    pub expected_work_revision: i64,
    pub holder: SessionId,
    pub claim_id: WorkClaimId,
    pub claim_fence: i64,
    pub summary: String,
    pub refs: Vec<String>,
    pub actor: ActorContext,
    pub idempotency_key: String,
    pub recorded_at: DateTime<Utc>,
}

/// Request to record or replay one consecutive quality-gate transition.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct RecordGateEvidenceRequest {
    pub work_id: WorkId,
    pub run_id: WorkRunId,
    pub expected_work_revision: i64,
    pub holder: SessionId,
    pub claim_id: WorkClaimId,
    pub claim_fence: i64,
    pub name: String,
    pub failed: Vec<String>,
    pub evidence_ref: Option<String>,
    pub actor: ActorContext,
    pub recorded_at: DateTime<Utc>,
}

/// Late-finding content bound to an inert restored completion record.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub(crate) enum RestoredWorkEvidenceInput {
    Note {
        #[serde(default, skip_serializing_if = "std::ops::Not::not")]
        status: bool,
        summary: String,
        refs: Vec<String>,
    },
    Gate {
        name: String,
        failed: Vec<String>,
        evidence_ref: Option<String>,
    },
}

/// Request to append one late finding to restored completed work.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct RecordRestoredWorkEvidenceRequest {
    pub work_id: WorkId,
    pub expected_work_revision: i64,
    pub holder: SessionId,
    pub input: RestoredWorkEvidenceInput,
    pub actor: ActorContext,
    pub idempotency_key: String,
    pub recorded_at: DateTime<Utc>,
}

/// Append-only gate observation against a verified restored completion record.
/// Each call records a distinct observation; this is not a retryable command.
#[derive(Clone, Debug, Serialize)]
pub(crate) struct AppendRestoredWorkGateRequest {
    pub work_id: WorkId,
    pub expected_work_revision: i64,
    pub holder: SessionId,
    pub name: String,
    pub failed: Vec<String>,
    pub evidence_ref: Option<String>,
    pub actor: ActorContext,
    pub recorded_at: DateTime<Utc>,
}

/// Request to seal a run at a fenced, evidence-backed completion cut.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CompleteWorkRequest {
    pub work_id: WorkId,
    pub run_id: WorkRunId,
    pub holder: SessionId,
    pub expected_work_revision: i64,
    pub claim_id: WorkClaimId,
    pub claim_fence: i64,
    pub evidence: Vec<ObjectId>,
    pub acceptance: Vec<AcceptanceResult>,
    pub drain: CompletionDrainAttestation,
    /// Host-measured source fingerprint at completion time; required when the
    /// project policy requires source freshness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_fingerprint: Option<String>,
    pub actor: ActorContext,
    pub idempotency_key: String,
    pub completed_at: DateTime<Utc>,
}

/// Human-authorized request to reopen completed work as a fresh run generation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReopenWorkRequest {
    pub work_id: WorkId,
    pub expected_work_revision: i64,
    pub reason: String,
    pub actor: ActorContext,
    pub idempotency_key: String,
    pub reopened_at: DateTime<Utc>,
}

/// Explicit non-success terminal disposition for open local work.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkDisposition {
    Cancelled,
    Superseded,
}

/// Audited request to dispose of work without laundering it as completion.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DisposeWorkRequest {
    pub work_id: WorkId,
    pub expected_work_revision: i64,
    pub disposition: WorkDisposition,
    pub replacement_id: Option<WorkId>,
    pub reason: String,
    pub actor: ActorContext,
    pub idempotency_key: String,
    pub disposed_at: DateTime<Utc>,
}

/// Audited composition of cancellation and the direct parent's required-child waiver.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RejectRequiredChildRequest {
    pub work_id: WorkId,
    pub expected_work_revision: i64,
    pub expected_parent_revision: Option<i64>,
    pub reason: String,
    pub actor: ActorContext,
    pub idempotency_key: String,
    pub rejected_at: DateTime<Utc>,
}

/// Replay-stable result of the two existing transitions committed together.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RejectRequiredChildReceipt {
    pub child: WorkItem,
    pub parent_ref: String,
    pub waiver: super::RequiredChildWaiver,
}

/// Atomically supersedes stranded open work with an independent root.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DetachWorkRequest {
    pub project_id: ProjectId,
    pub work_id: WorkId,
    pub expected_work_revision: i64,
    pub reason: String,
    pub actor: ActorContext,
    pub idempotency_key: String,
    pub detached_at: DateTime<Utc>,
}

/// Explicit completion-barrier waiver for a cancelled or superseded required child.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WaiveRequiredChildRequest {
    pub parent_id: WorkId,
    pub child_id: WorkId,
    pub expected_parent_revision: i64,
    pub reason: String,
    pub actor: ActorContext,
    pub idempotency_key: String,
    pub waived_at: DateTime<Utc>,
}

/// Host/operator-private request to waive one exact open work obligation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WaiveWorkObligationRequest {
    pub obligation_id: WorkObligationId,
    pub expected_definition: ObjectId,
    /// Human/operator identity asserted for immutable audit attribution.
    /// This text is not authenticated and does not itself grant permission.
    pub waived_by: String,
    pub reason: String,
    pub actor: ActorContext,
    pub idempotency_key: String,
    pub waived_at: DateTime<Utc>,
}
