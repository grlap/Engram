//! Local-work planning and execution records: feeds, items, runs, claims,
//! handoffs, evidence, obligations, completion seals, and audited transitions.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use unicode_casefold::UnicodeCaseFold;
use unicode_general_category::{GeneralCategory, get_general_category};
use unicode_normalization::UnicodeNormalization;

use crate::ObjectHash;
use crate::schema::SCHEMA_VERSION;

use super::{
    ActorContext, AssuranceLevel, BuiltinObligationRuleRef, ProjectId, RootExecutionId, SessionId,
    VerificationKind, VerificationRequirement, WorkClaimId, WorkEvidenceKind, WorkHandoffOfferId,
    WorkId, WorkObligationId, WorkRunId,
};

/// Sliding lease applied after every successful claim-holder work mutation.
pub const DEFAULT_WORK_CLAIM_TTL_SECONDS: i64 = 3_600;
pub(crate) const GATE_EVIDENCE_SUMMARY: &str = "typed gate evidence";
pub(crate) const MAX_GATE_NAME_BYTES: usize = 128;
pub(crate) const MAX_GATE_FAILURE_INPUTS: usize = 256;
pub(crate) const MAX_GATE_FAILURES: usize = 64;
pub(crate) const MAX_GATE_FAILURE_BYTES: usize = 256;
pub(crate) const MAX_GATE_FAILURE_TOTAL_BYTES: usize = 4 * 1024;
pub(crate) const MAX_GATE_REF_BYTES: usize = 2 * 1024;
const MAX_GATE_RAW_EXPANSION: usize = 4;

/// Named source feed. Positions are dense only within this exact identity.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum FeedId {
    Project(ProjectId),
    RootWork(WorkId),
    RunExecution(WorkRunId),
}

/// Monotonic position in one named source feed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FeedPosition {
    pub feed: FeedId,
    pub position: i64,
}

/// One immutable object reference in a named dense source feed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkFeedEntry {
    pub position: FeedPosition,
    pub object_kind: String,
    pub object_hash: ObjectHash,
}

/// Mutable, host-local navigation state for one agent session.
///
/// Authority is deliberately absent: local work uses the stable project plus
/// asserted actor/session binding, while lifecycle mutations recheck their
/// current item and fenced claim basis.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkSessionState {
    pub project_id: ProjectId,
    pub session_id: SessionId,
    pub focused_work_id: Option<WorkId>,
    /// Last project-feed position explicitly acknowledged by the caller.
    pub project_cursor: i64,
    /// Highest position in the currently staged, replayable delivery page.
    pub tentative_project_cursor: Option<i64>,
    /// Opaque acknowledgement capability emitted only with the staged page.
    pub tentative_delivery_token: Option<String>,
    pub updated_at: DateTime<Utc>,
}

/// Locally indexed category used for planning and filtering work.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkItemKind {
    Task,
    Bug,
    Feature,
    Epic,
    Chore,
    Research,
}

/// Whether a child contributes to the parent's completion barrier.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ChildRequirement {
    Required,
    Optional,
}

/// How a local work item entered Engram.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkOrigin {
    Local,
    Imported,
}

/// Durable planning lifecycle, separate from derived execution availability.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkLifecycle {
    Proposed,
    Open,
    Completed,
    Cancelled,
    Superseded,
}

/// One-hop completion state of an explicit prerequisite edge.
///
/// `Dead` means the edge cannot become satisfied without removing it. A
/// superseded prerequisite is classified from its immediate replacement; the
/// V1 readiness contract deliberately does not chase successor chains.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkPrerequisiteState {
    Satisfied,
    Pending,
    Dead,
}

/// Current execution state of one work-run generation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkRunState {
    Open,
    Claimed,
    Active,
    Completed,
    Cancelled,
}

/// Mutable claim projection state; immutable events retain every transition.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkClaimState {
    Active,
    Released,
    Completed,
}

/// Lifecycle of a checkpoint-coupled claim handoff offer.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkHandoffState {
    Offered,
    Accepted,
    Cancelled,
    Expired,
}

/// Lifecycle of one root execution aggregate.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum RootExecutionState {
    Active,
    Completed,
    Cancelled,
}

/// Typed reason that open work is not presently ready.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkBlockerKind {
    Manual,
    HumanDecision,
    ExternalInput,
    Policy,
}

/// Manually managed blocker projected from immutable lifecycle events.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkBlocker {
    pub blocker_id: String,
    pub work_id: WorkId,
    pub kind: WorkBlockerKind,
    pub detail: String,
    pub created_by: ActorContext,
    pub created_at: DateTime<Utc>,
}

/// Derived availability over planning, graph, blocker, deferral, run, and claim state.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkAvailability {
    Ready,
    Claimed,
    Active,
    Blocked,
    Deferred,
    Waiting,
    Closed,
}

/// Machine-facing availability reasons and supported recovery affordances.
/// These codes need not correspond one-to-one with human-readable explanations.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkReadinessReason {
    LifecycleClosed,
    DeferredUntil,
    PrerequisiteIncomplete,
    TypedBlockerActive,
    ParentDisallowsExecution,
    DetachAvailable,
    PriorClaimRecoverable,
    LiveClaimWithoutCheckpoint,
    LiveClaimWithCheckpoint,
    ReadyUnclaimed,
}

/// Current durable projection of one local planning identity.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkItem {
    pub schema_version: u16,
    pub project_id: ProjectId,
    pub work_id: WorkId,
    pub short_ref: String,
    pub root_id: WorkId,
    pub parent_id: Option<WorkId>,
    pub child_requirement: ChildRequirement,
    pub title: String,
    pub outcome: String,
    pub acceptance: Vec<String>,
    pub kind: WorkItemKind,
    pub priority: i32,
    pub labels: Vec<String>,
    pub assigned_to: Option<String>,
    pub deferred_until: Option<DateTime<Utc>>,
    pub origin: WorkOrigin,
    pub source_snapshot_id: Option<ObjectHash>,
    pub lifecycle: WorkLifecycle,
    pub revision: i64,
    pub active_run_id: Option<WorkRunId>,
    /// True when planning state was recreated from an inert work-graph
    /// snapshot rather than produced by this store's native event feed.
    #[serde(default)]
    pub restored: bool,
    #[serde(default)]
    pub superseded_by: Option<WorkId>,
    pub created_by: ActorContext,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Bounded identity shown when a human-facing work reference is ambiguous.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkReferenceCandidate {
    pub work_id: WorkId,
    #[serde(rename = "ref")]
    pub short_ref: String,
    pub title: String,
    #[serde(rename = "state")]
    pub lifecycle: WorkLifecycle,
}

/// Exact condition that prevents a work run from crossing its completion
/// barrier. Agent-facing receipts pair this typed cause with the affected
/// item's bounded identity and one concrete recovery command.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkCompletionRecoveryCause {
    OpenObligation {
        obligation_id: WorkObligationId,
        definition: ObjectHash,
        required_check: VerificationKind,
    },
    RequiredChildUnsealed {
        child: WorkId,
    },
    MissingContribution {
        participant: SessionId,
    },
    MissingAcceptance {
        criterion: String,
    },
}

/// Exact affected item and one executable command derived from one coherent
/// current snapshot for a completion refusal. The guidance is not persisted;
/// a retry recomputes it so a moved barrier yields its current recovery target.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkCompletionRecovery {
    pub cause: WorkCompletionRecoveryCause,
    pub item: WorkReferenceCandidate,
    pub command: String,
}

/// Aggregate generation that owns the root completion barrier.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RootExecution {
    pub schema_version: u16,
    pub root_execution_id: RootExecutionId,
    pub project_id: ProjectId,
    pub root_id: WorkId,
    pub generation: i64,
    pub state: RootExecutionState,
    pub revision: i64,
    pub run_ids: Vec<WorkRunId>,
    pub required_child_seals: Vec<ObjectHash>,
    #[serde(default)]
    pub required_child_waivers: Vec<RequiredChildWaiver>,
    pub expected_contributors: Vec<SessionId>,
    pub contributions: Vec<RootContribution>,
    pub waivers: Vec<CompletionWaiver>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// One ordinary-executor generation for a work item.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkRun {
    pub schema_version: u16,
    pub run_id: WorkRunId,
    pub root_execution_id: RootExecutionId,
    pub work_id: WorkId,
    pub generation: i64,
    pub executor: Option<SessionId>,
    pub state: WorkRunState,
    pub revision: i64,
    pub last_checkpoint: Option<ObjectHash>,
    pub completion_seal: Option<ObjectHash>,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Fenced, expiring responsibility for one work run.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkClaim {
    pub claim_id: WorkClaimId,
    pub work_id: WorkId,
    pub run_id: WorkRunId,
    pub accepted_work_revision: i64,
    pub holder: SessionId,
    pub expires_at: DateTime<Utc>,
    pub revision: i64,
    pub fence: i64,
    pub state: WorkClaimState,
}

/// Pending transfer that keeps the old claim authoritative until acceptance.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkHandoffOffer {
    pub offer_id: WorkHandoffOfferId,
    pub work_id: WorkId,
    pub run_id: WorkRunId,
    pub claim_id: WorkClaimId,
    pub claim_fence: i64,
    pub work_revision: i64,
    pub from: SessionId,
    pub to: SessionId,
    pub checkpoint: ObjectHash,
    pub accepted_ttl_seconds: i64,
    pub offered_at: DateTime<Utc>,
    pub expires_at: DateTime<Utc>,
    pub state: WorkHandoffState,
}

/// Structurally typed payload for one agent-reported quality-gate observation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct GateEvidenceRecord {
    pub schema_version: u16,
    pub name: String,
    pub passed: bool,
    pub failed: Vec<String>,
    /// Previous observation for this gate name. This makes a later return to
    /// the same result a distinct immutable transition even at one timestamp.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub previous: Option<ObjectHash>,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct NormalizedGateEvidenceInput {
    pub name: String,
    pub failed: Vec<String>,
    pub evidence_ref: Option<String>,
}

pub(crate) fn normalize_gate_evidence_input(
    name: &str,
    failed: &[String],
    evidence_ref: Option<&str>,
) -> Result<NormalizedGateEvidenceInput, String> {
    if name.len() > MAX_GATE_NAME_BYTES * MAX_GATE_RAW_EXPANSION {
        return Err(gate_input_too_large(
            "the raw gate name exceeds the normalization input ceiling",
        ));
    }
    if failed.len() > MAX_GATE_FAILURE_INPUTS {
        return Err(gate_input_too_large(&format!(
            "more than {MAX_GATE_FAILURE_INPUTS} gate failure labels were supplied"
        )));
    }
    let nfc: String = name.trim().nfc().collect();
    let name: String = nfc.as_str().case_fold().collect::<String>().nfc().collect();
    if name.is_empty() {
        return Err("gate name must not be empty".into());
    }
    if name.len() > MAX_GATE_NAME_BYTES {
        return Err(gate_input_too_large(&format!(
            "gate name exceeds {MAX_GATE_NAME_BYTES} UTF-8 bytes"
        )));
    }
    if name.chars().any(is_unsafe_rendered_text_char) {
        return Err("gate name must not contain control or format characters".into());
    }

    let mut normalized_failed = Vec::with_capacity(failed.len());
    for failure in failed {
        if failure.len() > MAX_GATE_FAILURE_BYTES * MAX_GATE_RAW_EXPANSION {
            return Err(gate_input_too_large(
                "one raw gate failure label exceeds the normalization input ceiling",
            ));
        }
        let failure: String = failure.trim().nfc().collect();
        if failure.is_empty() {
            return Err("gate failure labels must not be empty".into());
        }
        if failure.len() > MAX_GATE_FAILURE_BYTES {
            return Err(gate_input_too_large(&format!(
                "one gate failure label exceeds {MAX_GATE_FAILURE_BYTES} UTF-8 bytes"
            )));
        }
        if failure.chars().any(is_unsafe_rendered_text_char) {
            return Err("gate failure labels must not contain control or format characters".into());
        }
        normalized_failed.push(failure);
    }
    normalized_failed.sort();
    normalized_failed.dedup();
    if normalized_failed.len() > MAX_GATE_FAILURES {
        return Err(gate_input_too_large(&format!(
            "more than {MAX_GATE_FAILURES} distinct gate failure labels were supplied"
        )));
    }
    if normalized_failed.iter().map(String::len).sum::<usize>() > MAX_GATE_FAILURE_TOTAL_BYTES {
        return Err(gate_input_too_large(&format!(
            "the normalized gate failure-label list exceeds {MAX_GATE_FAILURE_TOTAL_BYTES} UTF-8 bytes"
        )));
    }

    if evidence_ref.is_some_and(|value| value.len() > MAX_GATE_REF_BYTES * MAX_GATE_RAW_EXPANSION) {
        return Err(gate_input_too_large(
            "the raw gate reference exceeds the normalization input ceiling",
        ));
    }
    let evidence_ref = evidence_ref.and_then(|value| {
        let value = value.trim().nfc().collect::<String>();
        (!value.is_empty()).then_some(value)
    });
    if let Some(value) = evidence_ref.as_deref()
        && (value.len() > MAX_GATE_REF_BYTES || value.chars().any(is_unsafe_rendered_text_char))
    {
        return Err(format!(
            "gate --ref must be a control- and format-free opaque reference of at most {MAX_GATE_REF_BYTES} UTF-8 bytes"
        ));
    }

    Ok(NormalizedGateEvidenceInput {
        name,
        failed: normalized_failed,
        evidence_ref,
    })
}

pub(crate) fn is_unsafe_rendered_text_char(ch: char) -> bool {
    matches!(
        get_general_category(ch),
        GeneralCategory::Control
            | GeneralCategory::Format
            | GeneralCategory::LineSeparator
            | GeneralCategory::ParagraphSeparator
            | GeneralCategory::PrivateUse
    ) || matches!(
        ch,
        '\u{034f}'
            | '\u{115f}'..='\u{1160}'
            | '\u{17b4}'..='\u{17b5}'
            | '\u{180b}'..='\u{180f}'
            | '\u{3164}'
            | '\u{fe00}'..='\u{fe0f}'
            | '\u{ffa0}'
            | '\u{fff0}'..='\u{fff8}'
            | '\u{1bca0}'..='\u{1bca3}'
            | '\u{e0100}'..='\u{e0fff}'
    )
}

fn gate_input_too_large(detail: &str) -> String {
    format!(
        "gate_input_too_large: {detail}; rerun with one aggregate --failed entry and --ref OPAQUE_REFERENCE"
    )
}

/// Evidence captured under a live work claim or appended as an attributed late
/// finding after completion. Post-completion evidence retains the completed
/// seal's historical claim basis and is marked in the actor provenance chain;
/// it is never part of that already-frozen seal.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkEvidence {
    pub schema_version: u16,
    pub work_id: WorkId,
    pub run_id: WorkRunId,
    pub claim_id: WorkClaimId,
    pub claim_fence: i64,
    pub summary: String,
    pub refs: Vec<String>,
    /// Present only for the typed `gate` word. Generic note/evidence prose can
    /// never acquire gate semantics by resembling its serialized form.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<GateEvidenceRecord>,
    pub actor: ActorContext,
    pub created_at: DateTime<Utc>,
}

pub(crate) fn validate_gate_evidence_payload(evidence: &WorkEvidence) -> Result<(), String> {
    let Some(gate) = &evidence.gate else {
        return Ok(());
    };
    gate.validate(&evidence.refs)
}

impl GateEvidenceRecord {
    pub(crate) fn validate(&self, refs: &[String]) -> Result<(), String> {
        let evidence_ref = match refs {
            [] => None,
            [evidence_ref] => Some(evidence_ref.as_str()),
            _ => return Err("more than one evidence reference".into()),
        };
        validate_stored_gate_evidence_fields(&self.name, &self.failed, evidence_ref)?;
        if self.schema_version != SCHEMA_VERSION || self.passed != self.failed.is_empty() {
            return Err("inconsistent normalized gate fields".into());
        }
        Ok(())
    }
}

fn validate_stored_gate_evidence_fields(
    name: &str,
    failed: &[String],
    evidence_ref: Option<&str>,
) -> Result<(), String> {
    if name.is_empty() || name.len() > MAX_GATE_NAME_BYTES {
        return Err("stored gate name is empty or oversized".into());
    }
    if failed.len() > MAX_GATE_FAILURES
        || failed.iter().map(String::len).sum::<usize>() > MAX_GATE_FAILURE_TOTAL_BYTES
    {
        return Err("stored gate failure-label list exceeds its count or byte bound".into());
    }
    let mut previous: Option<&str> = None;
    for failure in failed {
        if failure.is_empty()
            || failure.len() > MAX_GATE_FAILURE_BYTES
            || previous.is_some_and(|prior| prior >= failure.as_str())
        {
            return Err("stored gate failure labels are not bounded and strictly sorted".into());
        }
        previous = Some(failure);
    }
    if let Some(value) = evidence_ref
        && (value.is_empty() || value.len() > MAX_GATE_REF_BYTES)
    {
        return Err("stored gate reference is empty or oversized".into());
    }
    Ok(())
}

/// Checkpoint captured before continuing, releasing, or handing off work.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkCheckpoint {
    pub schema_version: u16,
    pub work_id: WorkId,
    pub run_id: WorkRunId,
    pub claim_id: WorkClaimId,
    pub claim_fence: i64,
    pub acknowledged_run_position: FeedPosition,
    pub summary: String,
    pub evidence: Vec<ObjectHash>,
    pub actor: ActorContext,
    pub created_at: DateTime<Utc>,
}

/// One participant's immutable contribution to the root completion barrier.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RootContribution {
    pub participant: SessionId,
    pub object: ObjectHash,
}

/// Attributed authority decision accounting for an expected participant omission.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CompletionWaiver {
    pub participant: SessionId,
    pub waived_by: String,
    pub reason: String,
}

/// Attributed authority decision accounting for one required child that was
/// deliberately cancelled or superseded instead of completed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RequiredChildWaiver {
    pub work_id: WorkId,
    pub work_revision: i64,
    pub waived_by: String,
    pub reason: String,
}

/// Immutable requirement opened by one exact run-bound execution fact.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkObligation {
    pub schema_version: u16,
    pub obligation_id: WorkObligationId,
    pub project_id: ProjectId,
    pub root_execution_id: RootExecutionId,
    pub root_id: WorkId,
    pub work_id: WorkId,
    pub run_id: WorkRunId,
    pub work_revision: i64,
    /// Exact rule-set identity selected by the triggering observation.
    pub rule_set: ObjectHash,
    pub rule: BuiltinObligationRuleRef,
    pub triggering_observation: ObjectHash,
    pub trigger_position: FeedPosition,
    pub requirement: VerificationRequirement,
    pub opened_at: DateTime<Utc>,
}

/// Bounded identity returned when completion is blocked by an open obligation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct OpenWorkObligation {
    pub obligation_id: WorkObligationId,
    pub definition: ObjectHash,
    pub required_check: VerificationKind,
}

/// Append-only terminal result for one immutable work obligation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkObligationResolution {
    Satisfied {
        evidence: ObjectHash,
        evaluated_cut: FeedPosition,
    },
    Waived {
        waived_by: String,
        reason: String,
    },
}

/// Immutable terminal event for one work obligation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkObligationResolutionEvent {
    pub schema_version: u16,
    pub project_id: ProjectId,
    pub obligation_id: WorkObligationId,
    pub definition: ObjectHash,
    pub run_id: WorkRunId,
    pub resolution: WorkObligationResolution,
    pub actor: ActorContext,
    pub created_at: DateTime<Utc>,
}

/// Rebuildable current state of one immutable obligation definition.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkObligationState {
    Open,
    Satisfied,
    Waived,
}

/// Planning context is either the exact live claim or the project binding.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkPlanningAuthority {
    Claim {
        run_id: WorkRunId,
        holder: SessionId,
        claim_id: WorkClaimId,
        claim_fence: i64,
    },
    Project,
}

/// Attributed host statement that action and resource authority is drained.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CompletionDrainAttestation {
    pub reconciled_action_outcomes: Vec<ObjectHash>,
    pub released_resource_leases: Vec<String>,
}

/// One acceptance criterion evaluated at the immutable completion cut.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct AcceptanceResult {
    pub criterion: String,
    pub satisfied: bool,
    pub evidence: Vec<ObjectHash>,
    pub assurance: AssuranceLevel,
    pub note: String,
}

/// Exact immutable obligation definition and resolution admitted by one seal.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CompletionObligationBinding {
    pub obligation_id: WorkObligationId,
    pub definition: ObjectHash,
    pub resolution: ObjectHash,
}

/// Immutable proof that one run completed under current work and claim fences.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CompletionSeal {
    pub schema_version: u16,
    pub work_id: WorkId,
    pub root_id: WorkId,
    pub root_execution_id: RootExecutionId,
    pub run_id: WorkRunId,
    pub run_generation: i64,
    pub accepted_work_revision: i64,
    pub accepted_work_revision_hash: ObjectHash,
    pub claim_id: WorkClaimId,
    pub claim_fence: i64,
    pub completion_cut: FeedPosition,
    pub checkpoint: Option<ObjectHash>,
    pub evidence: Vec<ObjectHash>,
    pub acceptance: Vec<AcceptanceResult>,
    pub obligation_schema_version: u16,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub obligations: Vec<CompletionObligationBinding>,
    pub environment_schema_version: u16,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub environment: Vec<ObjectHash>,
    pub required_child_seals: Vec<ObjectHash>,
    #[serde(default)]
    pub required_child_waivers: Vec<RequiredChildWaiver>,
    /// Completion proofs inherited from inert restored records rather than
    /// from live child execution in this store.
    #[serde(default)]
    pub restored_child_completions: Vec<ObjectHash>,
    /// Materialized transitive marker used to refuse report assembly without
    /// recursively walking child seals.
    #[serde(default)]
    pub restored: bool,
    pub unfinished_optional_children: Vec<WorkId>,
    pub expected_contributors: Vec<SessionId>,
    pub contributions: Vec<RootContribution>,
    pub waivers: Vec<CompletionWaiver>,
    pub drain: CompletionDrainAttestation,
    pub actor: ActorContext,
    pub completed_at: DateTime<Utc>,
}

/// Compact candidate returned by readiness queries.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReadyWork {
    pub work: WorkItem,
    pub availability: WorkAvailability,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub blocking_parent: Option<WorkLifecycle>,
    pub reason_codes: Vec<WorkReadinessReason>,
    pub why: Vec<String>,
    pub blocked_by: Vec<WorkId>,
    pub blockers: Vec<WorkBlocker>,
}

/// Bounded project-wide work query over canonical local projections.
#[derive(Clone, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkCatalogQuery {
    pub search: Option<String>,
    pub lifecycles: Vec<WorkLifecycle>,
    pub availabilities: Vec<WorkAvailability>,
    pub blocked_only: bool,
    pub assigned_to: Option<String>,
    /// Include this session's live claims in a union with assignment when both are set.
    pub held_by: Option<SessionId>,
    pub label: Option<String>,
    pub after: Option<WorkId>,
    pub limit: u32,
}

/// Stable page of project work, including non-ready and terminal items.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkCatalogPage {
    pub items: Vec<ReadyWork>,
    pub next_after: Option<WorkId>,
}

/// Immutable event shared by work feeds and audit/history views.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkEvent {
    pub schema_version: u16,
    pub project_id: ProjectId,
    pub root_id: WorkId,
    pub work_id: WorkId,
    pub run_id: Option<WorkRunId>,
    pub revision: i64,
    pub work: WorkItem,
    pub run: Option<WorkRun>,
    pub root_execution: Option<RootExecution>,
    pub claim: Option<WorkClaim>,
    pub handoff_offer: Option<WorkHandoffOffer>,
    pub blocker: Option<WorkBlocker>,
    /// Hash of the exact prerequisite and active-blocker projection after this
    /// transition.
    pub relation_fingerprint: ObjectHash,
    pub transition: WorkTransition,
    pub actor: ActorContext,
    pub created_at: DateTime<Utc>,
}

/// Audited work lifecycle transition.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkTransition {
    Created {
        prerequisites: Vec<WorkId>,
    },
    Decomposed {
        children: Vec<WorkId>,
        authority: WorkPlanningAuthority,
    },
    Revised {
        authority: WorkPlanningAuthority,
    },
    PrerequisiteAdded {
        prerequisite_id: WorkId,
        authority: WorkPlanningAuthority,
    },
    PrerequisiteRemoved {
        prerequisite_id: WorkId,
        authority: WorkPlanningAuthority,
    },
    Blocked {
        blocker_id: String,
    },
    Unblocked {
        blocker_id: String,
    },
    Claimed {
        claim: WorkClaim,
        recovered: bool,
    },
    ClaimRenewed {
        claim: WorkClaim,
    },
    Released {
        claim_id: WorkClaimId,
        fence: i64,
        reason: String,
    },
    Checkpointed {
        checkpoint: ObjectHash,
    },
    HandoffOffered {
        offer_id: WorkHandoffOfferId,
        to: SessionId,
        checkpoint: ObjectHash,
        offer: ObjectHash,
    },
    HandoffExpired {
        offer_id: WorkHandoffOfferId,
        offer: ObjectHash,
    },
    HandoffCancelled {
        offer_id: WorkHandoffOfferId,
        offer: ObjectHash,
        reason: String,
    },
    HandedOff {
        offer_id: WorkHandoffOfferId,
        claim_id: WorkClaimId,
        from: SessionId,
        to: SessionId,
        fence: i64,
        checkpoint: ObjectHash,
        offer: ObjectHash,
    },
    EvidenceAdded {
        evidence: ObjectHash,
    },
    MemoryCaptured {
        version: ObjectHash,
        assertion: ObjectHash,
    },
    TypedEvidenceAdded {
        evidence: ObjectHash,
        evidence_kind: WorkEvidenceKind,
    },
    Completed {
        seal: ObjectHash,
    },
    Disposed {
        lifecycle: WorkLifecycle,
        replacement_id: Option<WorkId>,
        reason: String,
    },
    RequiredChildWaived {
        child_id: WorkId,
        child_revision: i64,
        reason: String,
    },
    Reopened {
        run_id: WorkRunId,
        generation: i64,
        reason: String,
    },
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn gate_names_apply_canonicalization_once_and_stored_validation_does_not_rewrite_them() {
        for raw in [
            "\u{0345}",
            "\u{1f88}",
            "\u{1e9e}",
            "\u{0130}",
            "A\u{0315}\u{0300}",
            "\u{1f00}\u{0345}",
        ] {
            let normalized = normalize_gate_evidence_input(raw, &[], None)
                .unwrap_or_else(|error| panic!("normalize {raw:?}: {error}"));
            let repeated = normalize_gate_evidence_input(
                &normalized.name,
                &normalized.failed,
                normalized.evidence_ref.as_deref(),
            )
            .unwrap_or_else(|error| panic!("repeat normalization for {raw:?}: {error}"));
            assert_eq!(
                repeated, normalized,
                "gate normalization must be a fixed point"
            );
            assert!(
                validate_stored_gate_evidence_fields(&normalized.name, &normalized.failed, None)
                    .is_ok(),
                "stored canonical gate name {raw:?} must validate without a second fold"
            );
        }
        assert!(
            validate_stored_gate_evidence_fields("gate\u{e0020}", &[], None).is_ok(),
            "stored shape validation must not depend on mutable Unicode category tables"
        );
    }

    #[test]
    fn gate_input_bound_messages_derive_numbers_from_the_enforced_constants() {
        let oversized_name = "x".repeat(MAX_GATE_NAME_BYTES + 1);
        assert_eq!(
            normalize_gate_evidence_input(&oversized_name, &[], None)
                .expect_err("oversized gate name"),
            format!(
                "gate_input_too_large: gate name exceeds {MAX_GATE_NAME_BYTES} UTF-8 bytes; rerun with one aggregate --failed entry and --ref OPAQUE_REFERENCE"
            )
        );

        let too_many_inputs = vec!["same".into(); MAX_GATE_FAILURE_INPUTS + 1];
        assert_eq!(
            normalize_gate_evidence_input("gate", &too_many_inputs, None)
                .expect_err("too many raw failure inputs"),
            format!(
                "gate_input_too_large: more than {MAX_GATE_FAILURE_INPUTS} gate failure labels were supplied; rerun with one aggregate --failed entry and --ref OPAQUE_REFERENCE"
            )
        );

        let oversized_failure = vec!["x".repeat(MAX_GATE_FAILURE_BYTES + 1)];
        assert_eq!(
            normalize_gate_evidence_input("gate", &oversized_failure, None)
                .expect_err("oversized normalized failure"),
            format!(
                "gate_input_too_large: one gate failure label exceeds {MAX_GATE_FAILURE_BYTES} UTF-8 bytes; rerun with one aggregate --failed entry and --ref OPAQUE_REFERENCE"
            )
        );

        let too_many_distinct = (0..=MAX_GATE_FAILURES)
            .map(|index| format!("failure-{index}"))
            .collect::<Vec<_>>();
        assert_eq!(
            normalize_gate_evidence_input("gate", &too_many_distinct, None)
                .expect_err("too many distinct failures"),
            format!(
                "gate_input_too_large: more than {MAX_GATE_FAILURES} distinct gate failure labels were supplied; rerun with one aggregate --failed entry and --ref OPAQUE_REFERENCE"
            )
        );

        let oversized_total = (0..=MAX_GATE_FAILURE_TOTAL_BYTES / MAX_GATE_FAILURE_BYTES)
            .map(|index| format!("{index:02}{}", "x".repeat(MAX_GATE_FAILURE_BYTES - 2)))
            .collect::<Vec<_>>();
        assert_eq!(
            normalize_gate_evidence_input("gate", &oversized_total, None)
                .expect_err("oversized failure-label total"),
            format!(
                "gate_input_too_large: the normalized gate failure-label list exceeds {MAX_GATE_FAILURE_TOTAL_BYTES} UTF-8 bytes; rerun with one aggregate --failed entry and --ref OPAQUE_REFERENCE"
            )
        );

        let oversized_ref = "x".repeat(MAX_GATE_REF_BYTES + 1);
        assert_eq!(
            normalize_gate_evidence_input("gate", &[], Some(&oversized_ref))
                .expect_err("oversized gate reference"),
            format!(
                "gate --ref must be a control- and format-free opaque reference of at most {MAX_GATE_REF_BYTES} UTF-8 bytes"
            )
        );
    }
}
