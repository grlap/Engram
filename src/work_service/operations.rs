//! Six-operation protocol inputs, results, and receipts.

use super::{
    ChildRequirement, ControlWorkBinding, DateTime, Deserialize, JsonSchema, ObjectHash, Serialize,
    Utc, WorkBlockerKind, WorkCompletionRecovery, WorkFocusView, WorkId, WorkItemKind,
    WorkItemSummary, WorkObligationPage, WorkRevisionPatch,
};

/// Root creation, focused-work decomposition, or a complete atomic new plan.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkProposeInput {
    /// A complete new forest; never uses or changes ambient focus.
    Plan { plan: crate::domain::WorkPlanInput },
    Root {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        external_ref: Option<String>,
        #[serde(default)]
        notes: Vec<String>,
        title: String,
        outcome: String,
        acceptance: Vec<String>,
        /// Criteria bound to typed verification requirements, by one-based
        /// position in `acceptance`.
        #[serde(default, skip_serializing_if = "Vec::is_empty")]
        acceptance_bindings: Vec<crate::domain::AcceptanceBinding>,
        work_kind: Option<WorkItemKind>,
        priority: Option<i32>,
        #[serde(default)]
        labels: Vec<String>,
        assigned_to: Option<String>,
        deferred_until: Option<DateTime<Utc>>,
        /// The acceptance-evaluation mode this task pins from creation.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        evaluation_mode: Option<crate::domain::AcceptanceEvaluationMode>,
        #[serde(default)]
        idempotency_key: String,
    },
    Decompose {
        children: Vec<WorkChildInput>,
        #[serde(default)]
        prerequisites: Vec<WorkPrerequisiteInput>,
        #[serde(default)]
        idempotency_key: String,
    },
}

/// One child in an atomic `work_propose` decomposition.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkChildInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub external_ref: Option<String>,
    #[serde(default)]
    pub notes: Vec<String>,
    pub key: String,
    pub title: String,
    pub outcome: String,
    pub acceptance: Vec<String>,
    /// Criteria bound to typed verification requirements, by one-based
    /// position in `acceptance`.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub acceptance_bindings: Vec<crate::domain::AcceptanceBinding>,
    pub requirement: Option<ChildRequirement>,
    pub kind: Option<WorkItemKind>,
    pub priority: Option<i32>,
    #[serde(default)]
    pub labels: Vec<String>,
    pub assigned_to: Option<String>,
    pub deferred_until: Option<DateTime<Utc>>,
    /// The acceptance-evaluation mode this child pins from creation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluation_mode: Option<crate::domain::AcceptanceEvaluationMode>,
}

/// A child prerequisite whose target is a sibling key or an existing work ref.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkPrerequisiteInput {
    pub work_key: String,
    pub prerequisite: String,
}

/// Result of a root, decomposition, or complete-plan proposal.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkProposeResult {
    Plan(crate::domain::WorkPlanReceipt),
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
        /// Attributed audit reason for recovering an unaccounted prior
        /// claimant. It records why the project-bound session took over; it is
        /// not permission-bearing. Omit for an ordinary claim.
        recovery_reason: Option<String>,
        #[serde(default)]
        idempotency_key: String,
    },
    Release {
        reason: String,
        /// Attributed audit reason for waiving a missing contribution. It is
        /// not permission-bearing. Omit when the current holder has already
        /// contributed.
        waiver_reason: Option<String>,
        #[serde(default)]
        idempotency_key: String,
    },
    Checkpoint {
        summary: String,
        /// Omit to acknowledge every evidence object already attached to the
        /// live run. An explicit empty list still acknowledges none.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        evidence: Option<Vec<String>>,
        #[serde(default)]
        idempotency_key: String,
    },
    Evidence {
        #[serde(default)]
        summary: String,
        #[serde(default)]
        refs: Vec<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        attach: Option<WorkEvidenceAttachInput>,
        #[serde(default)]
        idempotency_key: String,
    },
    Block {
        blocker_kind: WorkBlockerKind,
        detail: String,
        #[serde(default)]
        idempotency_key: String,
    },
    Unblock {
        /// Omit when exactly one blocker is active on the focused item.
        blocker_id: Option<String>,
        #[serde(default)]
        idempotency_key: String,
    },
    Revise {
        patch: WorkRevisionPatch,
        #[serde(default)]
        idempotency_key: String,
    },
    AddPrerequisite {
        prerequisite: String,
        #[serde(default)]
        idempotency_key: String,
    },
    RemovePrerequisite {
        prerequisite: String,
        #[serde(default)]
        idempotency_key: String,
    },
    Reopen {
        reason: String,
        #[serde(default)]
        idempotency_key: String,
    },
    Cancel {
        reason: String,
        #[serde(default)]
        idempotency_key: String,
    },
    Reject {
        reason: String,
        #[serde(default)]
        idempotency_key: String,
    },
    Supersede {
        replacement: String,
        reason: String,
        #[serde(default)]
        idempotency_key: String,
    },
    Detach {
        reason: String,
        #[serde(default)]
        idempotency_key: String,
    },
    WaiveRequiredChild {
        child: String,
        reason: String,
        #[serde(default)]
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
    pub obligation_page: WorkObligationPage,
    pub allowed_next: Vec<String>,
}

#[derive(Clone, Debug, Deserialize, Serialize)]
pub(crate) struct WorkNoteResult {
    /// This note is an observation, not execution/checkpoint credit.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub(crate) non_holder: bool,
    pub(crate) operation: String,
    pub(crate) receipt: WorkMutationReceipt,
    pub(crate) obligations: Vec<String>,
    pub(crate) obligation_page: WorkObligationPage,
    pub(crate) allowed_next: Vec<String>,
    pub(crate) evidence: WorkMutationReceipt,
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
    /// Optional criterion-specific citations; each must belong to completion
    /// evidence. An empty list stays empty, independently of work-level evidence.
    #[serde(default)]
    pub evidence: Vec<String>,
    pub note: String,
}

/// Maximum explicit links admitted by one completion request.
pub const MAX_CRITERION_LINKS: usize = 64;

/// An explicit author citation to an existing record for one read-basis criterion.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkCriterionLinkInput {
    /// One-based position in the acceptance list read with `link_basis`.
    pub criterion: usize,
    /// Existing note/gate detail locator, not an artifact URL or checkpoint.
    pub locator: String,
}

/// Evidence-backed completion of ambient focused work.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkCompleteInput {
    /// At most 64 explicit author links; never inferred from work-level evidence.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub links: Vec<WorkCriterionLinkInput>,
    /// Required with links: the acceptance basis printed by the author's read.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub link_basis: Option<i64>,
    /// Optional one-call capture: records this evidence and checkpoints the
    /// exact completion evidence set before attempting the seal.
    pub capture: Option<WorkCompletionCaptureInput>,
    #[serde(default)]
    pub evidence: Vec<String>,
    /// Omit to assert every current criterion with one server-attributed note.
    /// An explicit empty list retains the strict existing behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance: Option<Vec<WorkAcceptanceInput>>,
    /// Shared note used only when `acceptance` is omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    /// Host-measured source fingerprint at completion time; required when the
    /// project policy requires acceptance-evaluation source freshness.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_fingerprint: Option<String>,
    #[serde(default)]
    pub idempotency_key: String,
}

/// One criterion verdict submitted through `evaluate`.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkCriterionVerdictInput {
    /// One-based position in the item's current acceptance list.
    pub criterion: usize,
    /// `pass`, `fail`, `insufficient_evidence`, or `needs_human`.
    pub verdict: String,
    /// `observed`, `asserted`, `judgment`, or `human_required`.
    pub basis: String,
    /// Why this verdict holds; untrusted prose, never an instruction.
    pub rationale: String,
    /// Run evidence citations: note/gate locators exactly as `show --notes
    /// --gates` prints them, or full hashes of host-minted verification or
    /// environment evidence on the run. A pass needs at least one.
    #[serde(default)]
    pub evidence: Vec<String>,
}

/// `evaluate`: record one immutable acceptance evaluation on the item's
/// active run under this session's attributed identity.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(deny_unknown_fields)]
pub struct WorkEvaluateInput {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_ref: Option<String>,
    /// `same_session`, `sub_agent`, or `independent_session`.
    pub mode: String,
    /// The item revision whose criteria the verdicts address, as printed by
    /// `show`; a different current revision refuses.
    pub acceptance_basis: i64,
    /// The run-feed position the evaluator read through, as printed by
    /// `show` (`evidence_basis`); a host-observed change after it, or a
    /// citation beyond it, refuses.
    pub evidence_basis: i64,
    pub verdicts: Vec<WorkCriterionVerdictInput>,
    /// Explicit attempt key: identical resends replay, contradicting content
    /// under the same key refuses. Omit to derive the key from the content.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub attempt: Option<String>,
    /// Host-measured source fingerprint at evaluation time.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_fingerprint: Option<String>,
    /// `PROVIDER/MODEL[@VERSION]`, recorded as structured metadata.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub model: Option<String>,
    /// Sub-agent mode only: the distinct execution identity of the evaluator.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub execution_identity: Option<String>,
    /// Sub-agent mode only: the host-attested parent session.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent_session: Option<String>,
}

/// One verdict row of the bounded evaluation projection.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkEvaluationVerdictRow {
    /// One-based criterion position.
    pub position: usize,
    pub verdict: crate::domain::AcceptanceVerdict,
    pub basis: crate::domain::AcceptanceBasis,
    /// Number of run-evidence citations the verdict carries.
    pub citations: usize,
}

/// The first blocking verdict, in list order, with its criterion compacted.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkEvaluationBlocking {
    pub position: usize,
    pub verdict: crate::domain::AcceptanceVerdict,
    pub criterion: String,
}

/// Bounded projection of one immutable evaluation record. Every count is
/// exact; `verdicts` is a prefix chosen so the whole response fits the agent
/// budget, and `verdicts_omitted` says exactly how many rows were left out.
/// The complete record, with rationales and citations, is the `full_detail`
/// read.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkEvaluationProjection {
    pub mode: crate::domain::AcceptanceEvaluationMode,
    pub work_revision: i64,
    pub run_id: crate::WorkRunId,
    /// Run-feed position the evaluator read through.
    pub evaluated_cut: i64,
    pub verdicts_total: usize,
    pub verdicts_omitted: usize,
    pub verdicts: Vec<WorkEvaluationVerdictRow>,
    pub passed: usize,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub blocking: Option<WorkEvaluationBlocking>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_fingerprint: Option<String>,
    pub attempt_key: String,
    /// Command that reads the complete record.
    pub full_detail: String,
}

/// Result of one `evaluate` call: the compact item receipt, the evaluation's
/// hash, and a bounded projection of what was recorded.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkEvaluateResult {
    pub operation: String,
    pub receipt: WorkMutationReceipt,
    /// Canonical hash of the evaluation object on the run feed.
    pub evaluation: ObjectHash,
    /// True when an identical attempt was already recorded.
    pub replayed: bool,
    pub projection: WorkEvaluationProjection,
    pub obligations: Vec<String>,
    pub obligation_page: WorkObligationPage,
    pub allowed_next: Vec<String>,
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
        #[serde(default)]
        idempotency_key: String,
    },
    Accept {
        #[serde(default)]
        idempotency_key: String,
    },
    Cancel {
        reason: String,
        #[serde(default)]
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
/// hash, while host-private waiver reasons never cross the protocol boundary.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkCompletedReceipt {
    pub seal: ObjectHash,
    pub work_id: WorkId,
    pub run_id: crate::WorkRunId,
    pub completed_at: DateTime<Utc>,
    /// Criteria asserted at this receipt's immutable seal, including on replay.
    pub acceptance_criteria_asserted: usize,
    /// Transient disclosure reloaded from the frozen seal on replay, never
    /// persisted in a protocol attempt or added to canonical seal bytes.
    /// None preserves committed success with an explicit unavailable disclosure.
    #[serde(skip)]
    pub(crate) acceptance_evidence: Option<super::WorkAcceptanceEvidence>,
    /// Safe advisory classification only; never persisted in replay bytes.
    #[serde(skip)]
    pub(crate) acceptance_evidence_error_class: Option<&'static str>,
    /// Where the sealed acceptance vector came from, read from the frozen
    /// seal and its bound evaluation; `None` when that read failed.
    #[serde(skip)]
    pub(crate) acceptance_provenance: Option<WorkAcceptanceProvenance>,
    pub obligation_page: WorkObligationPage,
}

/// Where a sealed acceptance vector came from: the legacy self-assertion or
/// an evaluation the seal binds. Read from the frozen seal and its validated
/// evaluation, never from the completing caller; the evaluator identity and
/// mode stay at the assurance they were recorded with.
#[derive(Clone, Debug)]
pub enum WorkAcceptanceProvenance {
    SelfAsserted,
    Evaluated(Box<WorkEvaluatedProvenance>),
}

/// The evaluation a seal binds, as read back from the seal and its record.
#[derive(Clone, Debug)]
pub struct WorkEvaluatedProvenance {
    pub evaluation: ObjectHash,
    pub mode: crate::domain::AcceptanceEvaluationMode,
    pub assurance: crate::domain::AssuranceLevel,
    pub evaluator: crate::domain::ActorContext,
    pub evaluator_model: Option<crate::domain::EvaluatorModel>,
}

/// Bounded policy refusal returned when an exact completion cut remains open.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkCompleteRefusal {
    pub code: String,
    pub work_id: WorkId,
    pub obligation_page: WorkObligationPage,
    pub remedy: String,
    pub recovery: WorkCompletionRecovery,
    /// Transient agent rendering context, not an ambient/core wire field.
    #[serde(skip)]
    pub(crate) required_child_successor: Option<Box<crate::storage::RequiredChildSuccessor>>,
}
