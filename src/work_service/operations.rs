//! Six-operation protocol inputs, results, and receipts.

use super::{
    ChildRequirement, ControlWorkBinding, DateTime, Deserialize, JsonSchema, ObjectHash, Serialize,
    Utc, WorkBlockerKind, WorkCompletionRecovery, WorkFocusView, WorkId, WorkItemKind,
    WorkItemSummary, WorkObligationPage, WorkRevisionPatch,
};

/// Low-ceremony root creation or atomic focused-work decomposition.
#[derive(Clone, Debug, Deserialize, JsonSchema, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum WorkProposeInput {
    Root {
        #[serde(default)]
        notes: Vec<String>,
        title: String,
        outcome: String,
        acceptance: Vec<String>,
        work_kind: Option<WorkItemKind>,
        priority: Option<i32>,
        #[serde(default)]
        labels: Vec<String>,
        assigned_to: Option<String>,
        deferred_until: Option<DateTime<Utc>>,
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
    #[serde(default)]
    pub notes: Vec<String>,
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
    Supersede {
        replacement: String,
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
    /// Omit to assert every current criterion with one server-attributed note.
    /// An explicit empty list retains the strict existing behavior.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub acceptance: Option<Vec<WorkAcceptanceInput>>,
    /// Shared note used only when `acceptance` is omitted.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    #[serde(default)]
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
    pub obligation_page: WorkObligationPage,
}

/// Bounded policy refusal returned when an exact completion cut remains open.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkCompleteRefusal {
    pub code: String,
    pub work_id: WorkId,
    pub obligation_page: WorkObligationPage,
    pub remedy: String,
    pub recovery: WorkCompletionRecovery,
}
