//! Behavioral-control types: policy, session phases, leases, verification
//! evidence, obligation rules, turn and action grants, and resource subjects.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;

use crate::ObjectHash;

use super::{
    ActorContext, ChangeCursor, ContextPacket, ProjectId, RootExecutionId, SessionId, TaskDelta,
    TaskId, TaskState, WorkClaimId, WorkId, WorkRunId,
};

/// Monotonic invalidation epoch for the active project control policy.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ProjectPolicyEpoch(pub i64);

/// Monotonic invalidation epoch for task-applicable admission state.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct TaskAdmissionEpoch(pub i64);

/// Independent invalidation epochs captured by a control decision.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
pub struct ControlEpochs {
    pub project_policy: ProjectPolicyEpoch,
    pub task_admission: TaskAdmissionEpoch,
}

/// How completely the host mediates the behavior Engram evaluates.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlAssurance {
    Advisory,
    TurnGated,
    ActionGated,
}

impl ControlAssurance {
    /// Whether this assurance is at least as strong as a policy requirement.
    #[must_use]
    pub fn covers(self, required: Self) -> bool {
        self >= required
    }
}

/// Project-scoped administrative operation attributed to one immutable
/// control-policy authority decision.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectPolicyOperation {
    SetRequiredAssurance,
    SetObligationRuleSet,
}

/// Immutable operator/host attribution authorizing one project policy change.
///
/// V1 records asserted host context honestly; this object is durable audit
/// evidence, not cryptographic authentication.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectPolicyAuthorityDecision {
    pub schema_version: u16,
    pub operation: ProjectPolicyOperation,
    pub policy_epoch: ProjectPolicyEpoch,
    pub previous_policy: Option<ObjectHash>,
    pub required_assurance: ControlAssurance,
    pub obligation_rule_set: ObjectHash,
    pub authorized_by: ActorContext,
    pub reason: String,
    pub decided_at: DateTime<Utc>,
}

/// Immutable version of the project-scoped behavioral-control policy.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ControlPolicy {
    pub schema_version: u16,
    pub control_schema_version: u16,
    pub policy_epoch: ProjectPolicyEpoch,
    pub previous_policy: Option<ObjectHash>,
    pub required_assurance: ControlAssurance,
    pub supported_effects: Vec<EffectClass>,
    pub grant_ttl_seconds: i64,
    pub obligation_rule_set: ObjectHash,
    pub authority: ObjectHash,
    pub activated_at: DateTime<Utc>,
}

/// Durable execution phase for a task-bound host session.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SessionPhase {
    Unbound,
    SyncRequired,
    Ready,
    TurnOpen,
    CheckpointRequired,
    RecoveryOpen,
    HandoffPending,
    ContributionRequired,
    ParticipantReady,
    FinalizerOpen,
    Exited,
}

/// Scope of a model turn admitted by the control plane.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnPurpose {
    Ordinary,
    Recovery,
    Finalizer,
}

/// Material effect classes used by host capability mediation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    Observe,
    Communicate,
    /// Engram-internal coordination such as fenced task-level leases.
    Coordinate,
    MutateLocal,
    MutateShared,
    ExternalSideEffect,
    Lifecycle,
}

/// Health state supplied to the deterministic control evaluator.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlHealth {
    Healthy,
    Unavailable,
    Corrupt,
    UnknownSchema,
}

/// Safety result from transactional context assembly.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum PacketSafety {
    Safe,
    PinnedContradiction,
    PinnedBudgetExceeded,
    DeliveryBudgetExceeded,
}

/// Durable membership result for the bound session and task.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ParticipantMembership {
    Member,
    NotMember,
}

/// Stable machine-readable reason why a turn cannot be admitted.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlRefusalCode {
    ControlUnavailable,
    StoreCorrupt,
    UnknownControlSchema,
    ControlPolicyMissing,
    ControlAssuranceInsufficient,
    CapabilityNotPermitted,
    TaskUnbound,
    TaskAccessDenied,
    PolicyEpochChanged,
    TaskAdmissionEpochChanged,
    PinnedContradiction,
    PinnedBudgetExceeded,
    LeaseRequired,
    ContextRequired,
    DeltaRequired,
    DeliveryInvalid,
    CheckpointRequired,
    RecoveryRequired,
    TurnAlreadyOpen,
    TurnPurposeMismatch,
    LifecycleHold,
    ParticipantNotReady,
    ActionOutcomeUnknown,
    MissingAuthority,
    GrantExpired,
    GrantNotBegun,
    GrantScopeMismatch,
    StaleFence,
    ResourceRemapped,
    SessionExited,
}

impl ControlRefusalCode {
    /// Stable protocol spelling used in directive identifiers and transports.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ControlUnavailable => "control_unavailable",
            Self::StoreCorrupt => "store_corrupt",
            Self::UnknownControlSchema => "unknown_control_schema",
            Self::ControlPolicyMissing => "control_policy_missing",
            Self::ControlAssuranceInsufficient => "control_assurance_insufficient",
            Self::CapabilityNotPermitted => "capability_not_permitted",
            Self::TaskUnbound => "task_unbound",
            Self::TaskAccessDenied => "task_access_denied",
            Self::PolicyEpochChanged => "policy_epoch_changed",
            Self::TaskAdmissionEpochChanged => "task_admission_epoch_changed",
            Self::PinnedContradiction => "pinned_contradiction",
            Self::PinnedBudgetExceeded => "pinned_budget_exceeded",
            Self::LeaseRequired => "lease_required",
            Self::ContextRequired => "context_required",
            Self::DeltaRequired => "delta_required",
            Self::DeliveryInvalid => "delivery_invalid",
            Self::CheckpointRequired => "checkpoint_required",
            Self::RecoveryRequired => "recovery_required",
            Self::TurnAlreadyOpen => "turn_already_open",
            Self::TurnPurposeMismatch => "turn_purpose_mismatch",
            Self::LifecycleHold => "lifecycle_hold",
            Self::ParticipantNotReady => "participant_not_ready",
            Self::ActionOutcomeUnknown => "action_outcome_unknown",
            Self::MissingAuthority => "missing_authority",
            Self::GrantExpired => "grant_expired",
            Self::GrantNotBegun => "grant_not_begun",
            Self::GrantScopeMismatch => "grant_scope_mismatch",
            Self::StaleFence => "stale_fence",
            Self::ResourceRemapped => "resource_remapped",
            Self::SessionExited => "session_exited",
        }
    }
}

/// Party capable of satisfying a typed control directive.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectiveTarget {
    Host,
    Agent,
    Human,
}

/// Evidence required before the evaluator may clear a directive.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum DirectiveSatisfaction {
    HostTransition,
    RecoveryCheckpoint,
    HumanAuthority,
}

/// Exact bounded packet or delta page proposed for prompt injection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeliveryPage {
    pub from_cursor: ChangeCursor,
    pub to_cursor: ChangeCursor,
    pub head_cursor: ChangeCursor,
    pub has_more: bool,
    pub content_digest: ObjectHash,
    pub delivery_token: String,
}

/// Purpose of a lease in the task control protocol.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseKind {
    Execution,
    Coordination,
}

/// Whether a lease reserves intent or exclusively authorizes mutation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum LeaseMode {
    Intent,
    Exclusive,
}

/// Complete lease basis captured by a turn or action grant.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LeaseBasis {
    pub lease_id: String,
    pub holder: SessionId,
    pub kind: LeaseKind,
    pub mode: LeaseMode,
    pub subject: ResourceSubject,
    pub fence: i64,
    pub expires_at: DateTime<Utc>,
}

/// Durable resource-scoped lease used by the host turn envelope.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkLease {
    pub control_schema_version: u16,
    pub lease_id: String,
    pub task_id: TaskId,
    pub holder: SessionId,
    pub kind: LeaseKind,
    pub mode: LeaseMode,
    pub subject: ResourceSubject,
    pub fence: i64,
    pub revision: i64,
    pub idempotency_key: String,
    pub expires_at: DateTime<Utc>,
}

impl WorkLease {
    /// Converts the current lease projection into grant-bound authority.
    #[must_use]
    pub fn basis(&self) -> LeaseBasis {
        LeaseBasis {
            lease_id: self.lease_id.clone(),
            holder: self.holder.clone(),
            kind: self.kind,
            mode: self.mode,
            subject: self.subject.clone(),
            fence: self.fence,
            expires_at: self.expires_at,
        }
    }
}

/// Result of one atomic resource claim.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum WorkLeaseDecision {
    Granted {
        lease: WorkLease,
    },
    Refuse {
        directive: ControlDirective,
    },
    Defer {
        holder: SessionId,
        conflicting_lease_id: String,
        expires_at: DateTime<Utc>,
        /// The conflicting lease is pinned by a begun turn. Expiry alone
        /// cannot transfer its fence until that turn is checkpointed.
        #[serde(default)]
        checkpoint_required: bool,
    },
}

/// Immutable task-feed event for resource lease acquisition or release.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkLeaseTransition {
    Acquired,
    Released,
    Expired,
}

/// Immutable task-feed event for resource lease acquisition or release.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkLeaseEvent {
    pub schema_version: u16,
    pub task_id: TaskId,
    pub lease: WorkLease,
    pub transition: WorkLeaseTransition,
    pub actor: ActorContext,
    pub created_at: DateTime<Utc>,
}

/// Idempotent receipt after releasing one held resource lease.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkLeaseReleaseReceipt {
    pub lease_id: String,
    pub task_id: TaskId,
    pub holder: SessionId,
    pub fence: i64,
    pub cursor: ChangeCursor,
    pub released_at: DateTime<Utc>,
}

/// Canonical intent supplied by the host before one model turn.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TurnIntent {
    pub idempotency_key: String,
    pub intent_fingerprint: ObjectHash,
    pub purpose: TurnPurpose,
    pub requested_effects: Vec<EffectClass>,
    #[serde(default)]
    pub resource_intents: Vec<ResourceSubject>,
}

/// Exact local-work claim basis selected by the embedding host for one control
/// session.
///
/// The binding is a frozen reference to a live claim. Storage revalidates it
/// before granting or beginning ordinary work and never derives it from a
/// task id or ambient focus.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ControlWorkBinding {
    pub root_execution_id: RootExecutionId,
    pub work_id: WorkId,
    pub run_id: WorkRunId,
    pub work_revision: i64,
    pub claim_id: WorkClaimId,
    pub claim_fence: i64,
}

/// Host-observed outcome for one material action performed during a begun
/// turn. This is asserted execution evidence, never execution authority.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ExecutionOutcome {
    Succeeded,
    Failed,
    Unknown,
}

/// Host-selected source identity carried by an execution observation.
///
/// A0 preserves this optional basis without treating it as verification. A1
/// requires and validates it against the verification evidence state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionSourceBasis {
    pub workspace_id: String,
    pub source_revision: String,
}

/// Host-supplied portion of one execution observation recorded at turn
/// checkpoint. Storage supplies the bound run, claim, session, grant, actor,
/// and recording timestamp.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionObservationInput {
    pub observation_id: String,
    pub action_fingerprint: ObjectHash,
    pub effect: EffectClass,
    pub outcome: ExecutionOutcome,
    pub source_changed: bool,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_basis: Option<ExecutionSourceBasis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<DateTime<Utc>>,
}

/// Immutable host-captured execution fact routed to the exact bound run feed.
///
/// The host may preserve workspace and full-content source-state basis on the
/// observation. Observations may trigger obligations and may be referenced by
/// host-minted typed evidence, but cannot satisfy verification requirements by
/// themselves.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ExecutionObservation {
    pub schema_version: u16,
    pub project_id: ProjectId,
    pub binding: ControlWorkBinding,
    pub session_id: SessionId,
    pub grant_id: String,
    pub observation_id: String,
    pub action_fingerprint: ObjectHash,
    pub effect: EffectClass,
    pub outcome: ExecutionOutcome,
    pub source_changed: bool,
    /// Exact immutable rule set selected by the frozen turn-policy basis.
    pub obligation_rule_set: ObjectHash,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_basis: Option<ExecutionSourceBasis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<DateTime<Utc>>,
    pub actor: ActorContext,
    pub recorded_at: DateTime<Utc>,
}

/// Stable host-private reference to a previously recorded execution
/// observation or to one observation in the same checkpoint request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ExecutionObservationReference {
    ObjectHash { object_hash: ObjectHash },
    ObservationId { observation_id: String },
}

/// Host-declared class of one verification command or check.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationKind {
    Test,
    Build,
    Lint,
    Review,
    Acceptance,
}

/// Host-observed result of one verification command or check.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationResult {
    Passed,
    Failed,
    Indeterminate,
}

/// Host-private request to mint typed verification evidence from an exact
/// host-captured execution observation.
///
/// The caller supplies classification and presentation only. Storage derives
/// the run, workspace/source basis, command fingerprint, result, producer
/// session, and timestamps from `producer_observation`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationEvidenceInput {
    pub producer_observation: ExecutionObservationReference,
    pub check_kind: VerificationKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<EnvironmentEvidenceReference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub refs: Vec<String>,
}

/// Host-declared components whose canonical bytes identify one execution
/// environment without embedding image, sandbox, or toolchain payload bytes.
/// These are asserted audit labels, not authenticated attestation, and must
/// never contain credentials or other secret material.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentComponents {
    pub toolchain: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub sandbox: Option<String>,
    pub workspace_id: String,
    pub capability_map_revision: i64,
}

/// Host-private reference to environment evidence already stored or supplied
/// earlier in the same checkpoint request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum EnvironmentEvidenceReference {
    ObjectHash { object_hash: ObjectHash },
    Index { index: usize },
}

/// Host-private request to capture the environment identity used for one exact
/// run and content state. When `components` is present, storage derives and
/// compares `environment_fingerprint`; absence selects the opaque fingerprint
/// contract.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct EnvironmentEvidenceInput {
    pub source_basis: ExecutionSourceBasis,
    pub environment_fingerprint: ObjectHash,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub components: Option<EnvironmentComponents>,
    pub observed_at: DateTime<Utc>,
}

/// Immutable host-minted evidence that one check ran against an exact content
/// state. It is the only evidence kind that may satisfy a verification
/// requirement.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct VerificationEvidence {
    pub schema_version: u16,
    pub project_id: ProjectId,
    pub binding: ControlWorkBinding,
    pub session_id: SessionId,
    pub producer_observation: ObjectHash,
    pub source_basis: ExecutionSourceBasis,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<ObjectHash>,
    pub check_kind: VerificationKind,
    pub check_fingerprint: ObjectHash,
    pub result: VerificationResult,
    pub completed_at: DateTime<Utc>,
    pub summary: String,
    pub refs: Vec<String>,
    pub actor: ActorContext,
    pub recorded_at: DateTime<Utc>,
}

/// Exact verification property required by one immutable work obligation.
///
/// Builtin V1 rules require the verification kind while deliberately leaving
/// the command fingerprint and environment open. Future immutable rules may
/// pin an exact fingerprint and environment without allowing candidate
/// evidence to define its own requirement.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct VerificationRequirement {
    pub check_kind: VerificationKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_fingerprint: Option<ObjectHash>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_environment: Option<ObjectHash>,
}

/// Immutable identity of one builtin obligation rule version.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BuiltinObligationRuleRef {
    pub rule_id: String,
    pub rule_version: u16,
}

/// Closed trigger vocabulary understood by the immutable V1 obligation rules.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum BuiltinObligationTrigger {
    SourceChanged,
    #[serde(other)]
    Unknown,
}

/// One immutable typed rule definition in a canonical obligation rule set.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ObligationRuleDefinition {
    pub rule: BuiltinObligationRuleRef,
    pub trigger: BuiltinObligationTrigger,
    pub requirement: VerificationRequirement,
}

/// Canonical rule table selected by one immutable project policy version.
///
/// The rule set is intentionally small and typed. Policy activation may select
/// a different immutable set for future observations, while obligations already
/// opened from an older set keep their recorded definition and requirement.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ObligationRuleSet {
    pub schema_version: u16,
    pub rules: Vec<ObligationRuleDefinition>,
}

/// Immutable host-minted identity of the environment used for one exact run
/// and source-content fingerprint.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EnvironmentEvidence {
    pub schema_version: u16,
    pub project_id: ProjectId,
    pub binding: ControlWorkBinding,
    pub session_id: SessionId,
    pub source_basis: ExecutionSourceBasis,
    pub environment_fingerprint: ObjectHash,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub components: Option<EnvironmentComponents>,
    pub observed_at: DateTime<Utc>,
    pub actor: ActorContext,
    pub recorded_at: DateTime<Utc>,
}

/// Typed evidence category retained by the run-evidence projection.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkEvidenceKind {
    Generic,
    Verification,
    Environment,
}

/// Why one evidence object cannot satisfy an exact verification requirement.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationEvidenceMismatch {
    WrongKind,
    CheckKindMismatch,
    WrongRun,
    StaleSourceRevision,
    CheckFingerprintMismatch,
    EnvironmentMismatch,
    ResultNotPassed,
    InvalidTime,
    InvalidProducer,
    NotAfterMutation,
}

const fn default_work_binding_current() -> bool {
    true
}

#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if callbacks receive a reference to the field"
)]
const fn work_binding_is_current(value: &bool) -> bool {
    *value
}

/// Complete explicitly supplied state used to evaluate one turn.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[allow(
    clippy::struct_excessive_bools,
    reason = "the evaluator input records independent fail-closed facts rather than interchangeable flags"
)]
pub struct TurnEvaluationInput {
    pub control_schema_version: u16,
    pub session_id: SessionId,
    pub task_id: Option<TaskId>,
    pub participant_membership: ParticipantMembership,
    pub task_state: Option<TaskState>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_binding: Option<ControlWorkBinding>,
    /// Whether storage revalidated `work_binding` against the current work,
    /// run, claim, holder, fence, root execution, and expiry in this snapshot.
    #[serde(
        default = "default_work_binding_current",
        skip_serializing_if = "work_binding_is_current"
    )]
    pub work_binding_current: bool,
    pub phase: SessionPhase,
    pub health: ControlHealth,
    pub active_policy_known: bool,
    pub host_assurance: ControlAssurance,
    pub required_assurance: ControlAssurance,
    #[serde(default)]
    pub policy_effects: Vec<EffectClass>,
    #[serde(default)]
    pub mediated_effects: Vec<EffectClass>,
    pub current_epochs: ControlEpochs,
    pub session_epochs: ControlEpochs,
    pub confirmed_cursor: ChangeCursor,
    pub head_cursor: ChangeCursor,
    pub pending_delivery: Option<DeliveryPage>,
    pub packet_safety: PacketSafety,
    pub blocking_watermark: ChangeCursor,
    pub acknowledged_blocking_watermark: ChangeCursor,
    pub has_unknown_action_outcome: bool,
    pub authority_satisfied: bool,
    pub capability_map_revision: i64,
    pub leases: Vec<LeaseBasis>,
    pub intent: TurnIntent,
    pub evaluated_at: DateTime<Utc>,
    pub grant_ttl_seconds: i64,
}

/// Repair obligation returned with a refused observed decision.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ControlDirective {
    pub directive_id: String,
    pub code: ControlRefusalCode,
    /// Effect whose intrinsic assurance or mediation rule caused this refusal.
    /// Project-wide assurance refusals leave this unset.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effect: Option<EffectClass>,
    /// Assurance required by the failed project-wide or effect-specific rule.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_assurance: Option<ControlAssurance>,
    /// Effects the host declared it mediates for this session, when the
    /// refusal is tied to the mediation envelope.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared_mediated_effects: Option<Vec<EffectClass>>,
    /// Declared effects that remain credible after the host assurance cap.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub effective_mediated_effects: Option<Vec<EffectClass>>,
    pub target: DirectiveTarget,
    pub satisfaction: DirectiveSatisfaction,
    pub recovery_effects: Vec<EffectClass>,
}

/// Exact basis a storage layer would bind into a short-lived turn grant.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TurnGrantBasis {
    pub session_id: SessionId,
    pub task_id: TaskId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_binding: Option<ControlWorkBinding>,
    pub purpose: TurnPurpose,
    pub intent_fingerprint: ObjectHash,
    pub project_policy_epoch: ProjectPolicyEpoch,
    pub task_admission_epoch: TaskAdmissionEpoch,
    pub confirmed_cursor: ChangeCursor,
    pub delivery_cursor: ChangeCursor,
    pub blocking_watermark: ChangeCursor,
    pub inline_delivery: Option<DeliveryPage>,
    pub capability_map_revision: i64,
    pub requested_effects: Vec<EffectClass>,
    #[serde(default)]
    pub resource_intents: Vec<ResourceSubject>,
    pub leases: Vec<LeaseBasis>,
    pub expires_at: DateTime<Utc>,
}

/// Deterministic result the observe-only evaluator says enforcement would use.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum TurnDecision {
    Grant { basis: Box<TurnGrantBasis> },
    Refuse { directive: ControlDirective },
    Defer { deferral: ControlDeferral },
}

/// Stable reason why admission should be retried rather than repaired.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlDeferCode {
    LeaseConflict,
    DecisionBusy,
}

/// Bounded retry/wake guidance for ordinary contention.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ControlDeferral {
    pub code: ControlDeferCode,
    pub retry_after_ms: Option<u64>,
    pub wake_condition: String,
}

/// Persistable shadow result. It is evidence, never execution authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ObservedTurnDecision {
    pub control_schema_version: u16,
    pub request_key: String,
    pub observed_at: DateTime<Utc>,
    pub decision: TurnDecision,
}

/// Durable host-control session state returned without exposing SQLite rows.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ControlSessionStatus {
    pub control_schema_version: u16,
    pub project_id: ProjectId,
    pub task_id: TaskId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_binding: Option<ControlWorkBinding>,
    pub session_id: SessionId,
    pub phase: SessionPhase,
    pub assurance: ControlAssurance,
    pub mediated_effects: Vec<EffectClass>,
    pub confirmed_cursor: ChangeCursor,
    pub tentative_cursor: Option<ChangeCursor>,
    pub epochs: ControlEpochs,
    pub blocking_watermark: ChangeCursor,
    pub capability_map_revision: i64,
    pub revision: i64,
    pub open_grant_id: Option<String>,
    /// Durable state of `open_grant_id`, when one is present.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub open_grant_state: Option<TurnGrantState>,
    /// Exact frozen grant that a replacement host may safely redeliver.
    ///
    /// This is present only for an already-begun, partial recovery page whose
    /// effects are observe-only. Other uncertain begun turns expose only
    /// `open_grant_id` and remain checkpoint/reconciliation required.
    #[serde(default)]
    pub recoverable_grant: Option<Box<IssuedTurnGrant>>,
}

/// Result of binding or safely rebinding a host-private control session.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ControlSessionBinding {
    pub routing_token: String,
    /// Declared effects capped to those this session's assurance may mediate.
    pub effective_mediated_effects: Vec<EffectClass>,
    pub status: ControlSessionStatus,
}

/// Exact prompt payload attached to a turn grant that advances synchronization.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ControlDelivery {
    pub page: DeliveryPage,
    /// Present on the final page only. Partial recovery pages contain exact
    /// deltas without exposing state beyond their cursor.
    pub context: Option<ContextPacket>,
    /// Exact verified task-feed interval covered by `page`.
    pub delta: TaskDelta,
}

/// Live turn authority issued to a host policy-enforcement point.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct IssuedTurnGrant {
    pub control_schema_version: u16,
    pub grant_id: String,
    pub request_key: String,
    pub basis: TurnGrantBasis,
    pub delivery: Option<ControlDelivery>,
    pub issued_at: DateTime<Utc>,
}

/// Enforced host-facing turn decision. Unlike [`ObservedTurnDecision`], a
/// grant here has a persisted identity and can be begun exactly once.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum ControlTurnDecision {
    Grant { grant: Box<IssuedTurnGrant> },
    Refuse { directive: ControlDirective },
    Defer { deferral: ControlDeferral },
}

/// Operational state of a persisted turn grant.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnGrantState {
    Issued,
    Begun,
    Completed,
    Expired,
    Superseded,
}

/// Immutable audit record for replacing issued-but-unbegun turn authority.
///
/// The mutable grant row makes the old authority unusable. This record binds
/// that terminalization to the exact replacement decision so the transition
/// can be reconstructed and integrity-checked after restart.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnGrantSupersessionReason {
    FreshEvaluation,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TurnGrantSupersession {
    pub control_schema_version: u16,
    pub session_id: SessionId,
    pub task_id: TaskId,
    pub superseded_grant_id: String,
    pub superseded_request_key: String,
    pub replacement_request_key: String,
    pub replacement_decision: ObjectHash,
    pub reason: TurnGrantSupersessionReason,
    pub superseded_at: DateTime<Utc>,
}

/// Durable facts rechecked immediately before the host dispatches a prompt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TurnBeginSnapshot {
    pub control_schema_version: u16,
    pub session_id: SessionId,
    pub task_id: TaskId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_binding: Option<ControlWorkBinding>,
    #[serde(
        default = "default_work_binding_current",
        skip_serializing_if = "work_binding_is_current"
    )]
    pub work_binding_current: bool,
    pub phase: SessionPhase,
    pub participant_membership: ParticipantMembership,
    pub task_state: Option<TaskState>,
    pub grant_state: TurnGrantState,
    pub current_epochs: ControlEpochs,
    pub current_head: ChangeCursor,
    pub context_current: bool,
    pub capability_map_revision: i64,
    pub delivery_tokens: Vec<String>,
    pub leases: Vec<LeaseBasis>,
    pub observed_at: DateTime<Utc>,
}

/// Pure begin-time result before any storage transition is committed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum TurnBeginDecision {
    Begin,
    Refuse { code: ControlRefusalCode },
}

/// Receipt proving that the host may dispatch the exact granted prompt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TurnBeginReceipt {
    pub grant_id: String,
    pub session_id: SessionId,
    pub task_id: TaskId,
    pub phase: SessionPhase,
    pub tentative_cursor: ChangeCursor,
    pub session_revision: i64,
    pub begun_at: DateTime<Utc>,
}

/// Host decision after a begin-time state and delivery recheck.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum ControlTurnBeginDecision {
    Begin { receipt: TurnBeginReceipt },
    Refuse { code: ControlRefusalCode },
}

/// Bounded lifecycle choice made after a model turn.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TurnNextIntent {
    Continue,
    Wait,
    Exit,
}

/// Durable facts checked before a begun turn can be checkpointed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TurnCheckpointSnapshot {
    pub control_schema_version: u16,
    pub session_id: SessionId,
    pub task_id: TaskId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub work_binding: Option<ControlWorkBinding>,
    pub phase: SessionPhase,
    pub grant_state: TurnGrantState,
}

/// Pure checkpoint eligibility result before the transaction emits an event.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum TurnCheckpointDecision {
    Checkpoint,
    Refuse { code: ControlRefusalCode },
}

/// Immutable audit event emitted by a successful turn checkpoint.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TurnCheckpointEvent {
    pub schema_version: u16,
    pub task_id: TaskId,
    pub session_id: SessionId,
    pub grant_id: String,
    pub delivered_cursor: ChangeCursor,
    pub next_intent: TurnNextIntent,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub execution_observations: Vec<ObjectHash>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verification_evidence: Vec<ObjectHash>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub environment_evidence: Vec<ObjectHash>,
    pub actor: ActorContext,
    pub created_at: DateTime<Utc>,
}

/// Restart-safe receipt for closing one begun model turn.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TurnCheckpointReceipt {
    pub grant_id: String,
    pub checkpoint: ObjectHash,
    pub cursor: ChangeCursor,
    pub confirmed_cursor: ChangeCursor,
    pub phase: SessionPhase,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub execution_observations: Vec<ObjectHash>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub verification_evidence: Vec<ObjectHash>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub environment_evidence: Vec<ObjectHash>,
    pub session_revision: i64,
    pub checkpointed_at: DateTime<Utc>,
}

/// Host result when checkpoint preconditions are evaluated atomically.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum ControlTurnCheckpointDecision {
    Checkpointed {
        receipt: TurnCheckpointReceipt,
    },
    Refuse {
        code: ControlRefusalCode,
        /// Additive host transition/recovery guidance for this refusal.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        directive: Option<ControlDirective>,
    },
}

/// Component-boundary coverage of a canonical resource subject.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResourceCoverage {
    Exact,
    Tree,
}

/// Host-selected filesystem identity rules persisted with a local store.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct HostPathPolicy {
    pub case_fold_paths: bool,
    pub windows_alias_rules: bool,
}

impl HostPathPolicy {
    /// Conservative policy for the host target running this Engram process.
    #[must_use]
    pub const fn host_default() -> Self {
        Self {
            case_fold_paths: cfg!(any(target_os = "windows", target_os = "macos")),
            windows_alias_rules: cfg!(target_os = "windows"),
        }
    }
}

/// Substrate-neutral resource identity used for conflicts and action grants.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub enum ResourceSubject {
    Path {
        project_id: ProjectId,
        segments: Vec<String>,
        coverage: ResourceCoverage,
    },
    Logical {
        namespace: String,
        segments: Vec<String>,
        coverage: ResourceCoverage,
    },
}

impl ResourceSubject {
    /// Normalizes an asserted subject for one project and host path policy.
    /// Path subjects are project-bound; logical namespaces remain
    /// case-sensitive. `case_fold_paths` must reflect the host filesystem
    /// policy selected by the embedding core.
    #[must_use]
    pub fn normalized_for_project(
        &self,
        expected_project: &ProjectId,
        case_fold_paths: bool,
    ) -> Option<Self> {
        self.normalized_for_project_with_policy(
            expected_project,
            HostPathPolicy {
                case_fold_paths,
                windows_alias_rules: false,
            },
        )
    }

    /// Normalizes a subject with the complete persisted host path policy.
    #[must_use]
    pub fn normalized_for_project_with_policy(
        &self,
        expected_project: &ProjectId,
        policy: HostPathPolicy,
    ) -> Option<Self> {
        let normalize = |value: &str, fold_case: bool| {
            let nfc = value.nfc().collect::<String>();
            if fold_case {
                nfc.as_str().case_fold().collect::<String>().nfc().collect()
            } else {
                nfc
            }
        };
        let normalized = match self {
            Self::Path {
                project_id,
                segments,
                coverage,
            } if project_id == expected_project => Self::Path {
                project_id: expected_project.clone(),
                segments: segments
                    .iter()
                    .map(|segment| normalize(segment, policy.case_fold_paths))
                    .collect(),
                coverage: *coverage,
            },
            Self::Path { .. } => return None,
            Self::Logical {
                namespace,
                segments,
                coverage,
            } => Self::Logical {
                namespace: normalize(namespace, false),
                segments: segments
                    .iter()
                    .map(|segment| normalize(segment, false))
                    .collect(),
                coverage: *coverage,
            },
        };
        let aliases_are_safe = !policy.windows_alias_rules
            || match &normalized {
                Self::Path { segments, .. } => segments
                    .iter()
                    .all(|segment| windows_path_segment_is_unambiguous(segment)),
                Self::Logical { .. } => true,
            };
        (normalized.has_valid_shape() && aliases_are_safe).then_some(normalized)
    }

    /// Whether this subject requires execution-bound filesystem resolution.
    #[must_use]
    pub fn is_path(&self) -> bool {
        matches!(self, Self::Path { .. })
    }

    /// Whether the already-normalized subject has a structurally safe shape.
    ///
    /// Unicode normalization and project case policy belong to the host/core
    /// mapping boundary; this check rejects shapes that are unsafe under every
    /// supported policy.
    #[must_use]
    pub fn has_valid_shape(&self) -> bool {
        let (prefix_valid, segments, coverage) = match self {
            Self::Path {
                project_id,
                segments,
                coverage,
            } => (!project_id.0.trim().is_empty(), segments, coverage),
            Self::Logical {
                namespace,
                segments,
                coverage,
            } => (!namespace.trim().is_empty(), segments, coverage),
        };
        prefix_valid
            && (!segments.is_empty() || matches!(coverage, ResourceCoverage::Tree))
            && segments.iter().all(|segment| {
                !segment.is_empty()
                    && segment != "."
                    && segment != ".."
                    && !segment.contains(['/', '\\', '\0'])
            })
    }

    /// Whether this lease subject fully covers a requested action subject.
    #[must_use]
    pub fn covers(&self, requested: &Self) -> bool {
        let (same_root, lease_segments, lease_coverage, requested_segments, requested_coverage) =
            match (self, requested) {
                (
                    Self::Path {
                        project_id: lease_project,
                        segments: lease_segments,
                        coverage: lease_coverage,
                    },
                    Self::Path {
                        project_id: requested_project,
                        segments: requested_segments,
                        coverage: requested_coverage,
                    },
                ) => (
                    lease_project == requested_project,
                    lease_segments,
                    lease_coverage,
                    requested_segments,
                    requested_coverage,
                ),
                (
                    Self::Logical {
                        namespace: lease_namespace,
                        segments: lease_segments,
                        coverage: lease_coverage,
                    },
                    Self::Logical {
                        namespace: requested_namespace,
                        segments: requested_segments,
                        coverage: requested_coverage,
                    },
                ) => (
                    lease_namespace == requested_namespace,
                    lease_segments,
                    lease_coverage,
                    requested_segments,
                    requested_coverage,
                ),
                (Self::Path { .. }, Self::Logical { .. })
                | (Self::Logical { .. }, Self::Path { .. }) => return false,
            };

        same_root
            && match lease_coverage {
                ResourceCoverage::Exact => {
                    matches!(requested_coverage, ResourceCoverage::Exact)
                        && lease_segments == requested_segments
                }
                ResourceCoverage::Tree => requested_segments.starts_with(lease_segments),
            }
    }
}

fn windows_path_segment_is_unambiguous(segment: &str) -> bool {
    if segment.ends_with(['.', ' '])
        || segment.chars().any(|character| {
            character <= '\u{1f}' || matches!(character, '<' | '>' | '"' | '|' | '?' | '*' | ':')
        })
    {
        return false;
    }
    let folded = segment.to_ascii_uppercase();
    let stem = folded.split('.').next().unwrap_or_default();
    let reserved = matches!(stem, "CON" | "PRN" | "AUX" | "NUL" | "CONIN$" | "CONOUT$")
        || stem
            .strip_prefix("COM")
            .or_else(|| stem.strip_prefix("LPT"))
            .is_some_and(|suffix| {
                matches!(
                    suffix,
                    "1" | "2" | "3" | "4" | "5" | "6" | "7" | "8" | "9" | "¹" | "²" | "³"
                )
            });
    let short_alias = stem.rsplit_once('~').is_some_and(|(_, suffix)| {
        !suffix.is_empty() && suffix.len() <= 6 && suffix.bytes().all(|byte| byte.is_ascii_digit())
    });
    !reserved && !short_alias
}

/// Complete basis captured when an action grant is authorized.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ActionGrantBasis {
    pub control_schema_version: u16,
    pub grant_id: String,
    pub parent_turn_id: String,
    pub session_id: SessionId,
    pub task_id: TaskId,
    pub turn_purpose: TurnPurpose,
    pub effect: EffectClass,
    pub resource_subjects: Vec<ResourceSubject>,
    pub request_fingerprint: ObjectHash,
    pub authority_references: Vec<String>,
    pub epochs: ControlEpochs,
    pub blocking_watermark: ChangeCursor,
    pub capability_map_revision: i64,
    pub leases: Vec<LeaseBasis>,
    pub resolution_binding_digest: Option<ObjectHash>,
    pub expires_at: DateTime<Utc>,
}

/// Begin-time state of the parent turn.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ParentTurnState {
    Open,
    Closed,
}

/// Whether the single-use action grant is still available.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ActionGrantState {
    Available,
    Consumed,
}

/// Verification result for the durable authority references.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AuthorityState {
    Valid,
    Invalid,
}

/// Filesystem mapping assurance supplied by the host mediator.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ResolutionAssurance {
    PinnedThroughInvocation,
    DetectionOnly,
}

/// Current state atomically compared with an action grant at begin time.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ActionBeginSnapshot {
    pub control_schema_version: u16,
    pub parent_turn_id: String,
    pub parent_turn_state: ParentTurnState,
    pub grant_state: ActionGrantState,
    pub session_id: SessionId,
    pub task_id: TaskId,
    pub phase: SessionPhase,
    pub task_state: TaskState,
    pub turn_purpose: TurnPurpose,
    pub effect: EffectClass,
    pub resource_subjects: Vec<ResourceSubject>,
    pub request_fingerprint: ObjectHash,
    pub authority_references: Vec<String>,
    pub authority_state: AuthorityState,
    pub current_epochs: ControlEpochs,
    pub acknowledged_blocking_watermark: ChangeCursor,
    pub capability_map_revision: i64,
    pub leases: Vec<LeaseBasis>,
    pub resolution_binding_digest: Option<ObjectHash>,
    pub resolution_assurance: ResolutionAssurance,
    pub observed_at: DateTime<Utc>,
}

/// Shadow result of the complete begin-time authorization recheck.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum ActionBeginDecision {
    Begin { grant_id: String },
    Refuse { code: ControlRefusalCode },
}

/// Persistable action-begin observation. It never consumes a real grant.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ObservedActionBeginDecision {
    pub control_schema_version: u16,
    pub grant_id: String,
    pub observed_at: DateTime<Utc>,
    pub decision: ActionBeginDecision,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resource_subject_shape_rejects_ambiguous_paths() {
        let project_id = ProjectId("project-a".into());
        assert!(
            ResourceSubject::Path {
                project_id: project_id.clone(),
                segments: Vec::new(),
                coverage: ResourceCoverage::Tree,
            }
            .has_valid_shape()
        );
        for segments in [
            Vec::new(),
            vec![".".into()],
            vec!["..".into()],
            vec!["nested/name".into()],
            vec!["nested\\name".into()],
        ] {
            assert!(
                !ResourceSubject::Path {
                    project_id: project_id.clone(),
                    segments,
                    coverage: ResourceCoverage::Exact,
                }
                .has_valid_shape()
            );
        }
    }

    #[test]
    fn tree_resource_subject_covers_only_component_descendants() {
        let project_id = ProjectId("project-a".into());
        let tree = ResourceSubject::Path {
            project_id: project_id.clone(),
            segments: vec!["src".into()],
            coverage: ResourceCoverage::Tree,
        };
        let child = ResourceSubject::Path {
            project_id: project_id.clone(),
            segments: vec!["src".into(), "control.rs".into()],
            coverage: ResourceCoverage::Exact,
        };
        let sibling = ResourceSubject::Path {
            project_id,
            segments: vec!["src-old".into()],
            coverage: ResourceCoverage::Exact,
        };

        assert!(tree.covers(&child));
        assert!(!tree.covers(&sibling));
        assert!(!child.covers(&tree));
    }

    #[test]
    fn resource_normalization_binds_project_case_policy_and_unicode() {
        let project_id = ProjectId("project-a".into());
        let alias = ResourceSubject::Path {
            project_id: project_id.clone(),
            segments: vec!["SRC".into(), "cafe\u{301}.rs".into()],
            coverage: ResourceCoverage::Exact,
        };
        assert_eq!(
            alias.normalized_for_project(&project_id, true),
            Some(ResourceSubject::Path {
                project_id: project_id.clone(),
                segments: vec!["src".into(), "caf\u{e9}.rs".into()],
                coverage: ResourceCoverage::Exact,
            })
        );
        assert!(
            alias
                .normalized_for_project(&ProjectId("project-b".into()), true)
                .is_none()
        );
        let long_s = ResourceSubject::Path {
            project_id: project_id.clone(),
            segments: vec!["\u{17f}ource".into()],
            coverage: ResourceCoverage::Tree,
        };
        let plain_s = ResourceSubject::Path {
            project_id: project_id.clone(),
            segments: vec!["source".into()],
            coverage: ResourceCoverage::Tree,
        };
        assert_eq!(
            long_s.normalized_for_project(&project_id, true),
            plain_s.normalized_for_project(&project_id, true)
        );
    }

    #[test]
    fn windows_resource_policy_rejects_filename_aliases() {
        let project_id = ProjectId("project-a".into());
        let policy = HostPathPolicy {
            case_fold_paths: true,
            windows_alias_rules: true,
        };
        for segment in [
            "readme.md.",
            "readme.md ",
            "file.txt:secret",
            "CON",
            "com1.log",
            "COM¹.log",
            "lpt²",
            "CONIN$",
            "CONOUT$.txt",
            "READM~1.TXT",
            "bad<name",
            "bad>name",
            "bad\"name",
            "bad|name",
            "bad?name",
            "bad*name",
            "bad\u{1f}name",
        ] {
            let subject = ResourceSubject::Path {
                project_id: project_id.clone(),
                segments: vec![segment.into()],
                coverage: ResourceCoverage::Exact,
            };
            assert_eq!(
                subject.normalized_for_project_with_policy(&project_id, policy),
                None,
                "{segment:?} must not be admitted under Windows identity rules"
            );
        }
        let safe = ResourceSubject::Path {
            project_id: project_id.clone(),
            segments: vec!["src".into(), "control.rs".into()],
            coverage: ResourceCoverage::Exact,
        };
        assert!(
            safe.normalized_for_project_with_policy(&project_id, policy)
                .is_some()
        );
    }
}
