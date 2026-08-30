//! Domain records shared by storage, context assembly, and tracker adapters.

use std::collections::BTreeMap;

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use serde_json::Value;
use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;
use uuid::Uuid;

use crate::ObjectHash;
pub use crate::schema::{
    COMPLETION_ENVIRONMENT_SCHEMA_VERSION, COMPLETION_OBLIGATION_SCHEMA_VERSION,
    CONTROL_SCHEMA_VERSION, OBLIGATION_RULE_SET_SCHEMA_VERSION, SCHEMA_VERSION,
};
/// Sliding lease applied after every successful claim-holder work mutation.
pub const DEFAULT_WORK_CLAIM_TTL_SECONDS: i64 = 3_600;

/// Stable host-local project identity shared by every session and worktree.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ProjectId(pub String);

/// Runtime session identity asserted by the host integration.
#[derive(Clone, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct SessionId(pub String);

/// Monotonic position in a task's durable change feed.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(transparent)]
pub struct ChangeCursor(pub i64);

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
    UpgradeBuiltinEnvelope,
    UpgradeBuiltinObligationRules,
    SetObligationRuleSet,
    #[serde(other)]
    Unknown,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub obligation_rule_set: Option<ObjectHash>,
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
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub obligation_rule_set: Option<ObjectHash>,
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

/// Exact local-work authority basis selected by the embedding host for one
/// control session.
///
/// The binding is a frozen reference to a live claim. Storage revalidates it
/// before granting or beginning ordinary work and never derives it from a
/// legacy task id or ambient focus.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
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
pub struct ExecutionSourceBasis {
    pub workspace_id: String,
    pub source_revision: String,
}

/// Host-supplied portion of one execution observation recorded at turn
/// checkpoint. Storage supplies the bound run, claim, session, grant, actor,
/// and recording timestamp.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
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
    /// Legacy observations omit this field and retain the stock V1 meaning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub obligation_rule_set: Option<ObjectHash>,
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
#[serde(tag = "kind", rename_all = "snake_case")]
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
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum EnvironmentEvidenceReference {
    ObjectHash { object_hash: ObjectHash },
    Index { index: usize },
}

/// Host-private request to capture the environment identity used for one exact
/// run and content state. When `components` is present, storage derives and
/// compares `environment_fingerprint`; absence preserves the legacy opaque
/// fingerprint contract.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
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
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
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
#[serde(tag = "kind", rename_all = "snake_case")]
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

/// Stable identifier for a memory across immutable versions.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct MemoryId(pub Uuid);

impl MemoryId {
    /// Creates a time-sortable identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for MemoryId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable identifier for a local operational task.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct TaskId(pub Uuid);

impl TaskId {
    /// Creates a time-sortable identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for TaskId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable planning identity for first-class local work.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WorkId(pub Uuid);

impl WorkId {
    /// Creates a time-sortable identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for WorkId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable identity of one root execution generation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct RootExecutionId(pub Uuid);

impl RootExecutionId {
    /// Creates a time-sortable identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for RootExecutionId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable identity of one execution generation for one work item.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WorkRunId(pub Uuid);

impl WorkRunId {
    /// Creates a time-sortable identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for WorkRunId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable identity of one immutable execution obligation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WorkObligationId(pub Uuid);

impl WorkObligationId {
    /// Creates a time-sortable identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for WorkObligationId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable identity of a fenced work claim across renewal and handoff.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WorkClaimId(pub Uuid);

impl WorkClaimId {
    /// Creates a time-sortable identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for WorkClaimId {
    fn default() -> Self {
        Self::new()
    }
}

/// Stable identity of one pending handoff offer.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(transparent)]
pub struct WorkHandoffOfferId(pub Uuid);

impl WorkHandoffOfferId {
    /// Creates a time-sortable identifier.
    #[must_use]
    pub fn new() -> Self {
        Self(Uuid::now_v7())
    }
}

impl Default for WorkHandoffOfferId {
    fn default() -> Self {
        Self::new()
    }
}

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
/// Authority is deliberately absent: a host binds authority to the service
/// process and every mutation resolves that immutable grant again.
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
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
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

/// Stable machine-facing reasons behind a derived work availability.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkReadinessReason {
    LifecycleClosed,
    DeferredUntil,
    PrerequisiteIncomplete,
    TypedBlockerActive,
    ParentDisallowsExecution,
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
    pub authority_policy_ref: String,
    pub lifecycle: WorkLifecycle,
    pub revision: i64,
    pub active_run_id: Option<WorkRunId>,
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
    LapsedClaim {
        expired_at: DateTime<Utc>,
    },
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

/// Exact affected item and one executable command captured with a completion
/// refusal. Writers freeze this value in the same transaction that decides
/// the refusal so retries cannot observe a different recovery target.
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

/// Evidence captured under the live work claim and later consumed by completion.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkEvidence {
    pub schema_version: u16,
    pub work_id: WorkId,
    pub run_id: WorkRunId,
    pub claim_id: WorkClaimId,
    pub claim_fence: i64,
    pub summary: String,
    pub refs: Vec<String>,
    pub actor: ActorContext,
    pub created_at: DateTime<Utc>,
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
    pub authority_grant: ObjectHash,
    pub waived_by: String,
    pub reason: String,
}

/// Attributed authority decision accounting for one required child that was
/// deliberately cancelled instead of completed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RequiredChildWaiver {
    pub work_id: WorkId,
    pub work_revision: i64,
    pub authority_grant: ObjectHash,
    pub waived_by: String,
    pub reason: String,
}

/// Bounded decomposition envelope carried by planning authority.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkPlanningBudget {
    pub max_depth: u32,
    pub max_open_descendants: u32,
    pub max_children_per_decomposition: u32,
}

/// Operation admitted by one durable work-authority grant.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkAuthorityOperation {
    RootCreate,
    Plan,
    Claim,
    Dispose,
    RootComplete,
    Reopen,
    ClaimRecovery,
    CompletionWaiver,
    CompletionDrain,
    ObligationWaiver,
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
    /// Exact rule-set identity selected by the triggering observation. Legacy
    /// definitions omit it and retain the stock V1 rule meaning.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub rule_set: Option<ObjectHash>,
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
        authority_grant: ObjectHash,
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

/// Durable scope within which a work-authority grant may be consumed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", content = "id", rename_all = "snake_case")]
pub enum WorkAuthorityScope {
    Project,
    Root(WorkId),
    Work(WorkId),
    Run(WorkRunId),
}

/// Canonical host-issued authority resolved by hash inside lifecycle transactions.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkAuthorityGrant {
    pub schema_version: u16,
    pub project_id: ProjectId,
    pub policy_ref: String,
    pub subject_actor_id: String,
    /// Host/operator that issued the decision. This is attribution, not an
    /// authenticated identity claim unless its assurance says otherwise.
    pub issued_by: ActorContext,
    pub assurance: AssuranceLevel,
    pub operations: Vec<WorkAuthorityOperation>,
    pub scope: WorkAuthorityScope,
    pub planning_budget: Option<WorkPlanningBudget>,
    pub issued_at: DateTime<Utc>,
    pub valid_until: DateTime<Utc>,
    pub reason: String,
}

/// Immutable host-attributed revocation of one durable work-authority grant.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkAuthorityRevocation {
    pub schema_version: u16,
    pub grant: ObjectHash,
    pub revoked_by: ActorContext,
    pub reason: String,
    pub revoked_at: DateTime<Utc>,
}

/// Reference to a canonical, persisted lifecycle authority grant.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LifecycleAuthorityDecision {
    pub grant: ObjectHash,
}

/// Planning authority is either the exact live claim or an explicit delegation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum WorkPlanningAuthority {
    Claim {
        run_id: WorkRunId,
        holder: SessionId,
        claim_id: WorkClaimId,
        claim_fence: i64,
        grant: ObjectHash,
    },
    Delegated {
        grant: ObjectHash,
    },
}

/// Attributed host statement that action and resource authority is drained.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CompletionDrainAttestation {
    pub reconciled_action_outcomes: Vec<ObjectHash>,
    pub released_resource_leases: Vec<String>,
    pub decision: LifecycleAuthorityDecision,
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
    /// Absent only on immutable pre-A3 seals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub obligation_schema_version: Option<u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub obligations: Vec<CompletionObligationBinding>,
    /// Absent only on immutable pre-B seals.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment_schema_version: Option<u16>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub environment: Vec<ObjectHash>,
    pub required_child_seals: Vec<ObjectHash>,
    #[serde(default)]
    pub required_child_waivers: Vec<RequiredChildWaiver>,
    pub unfinished_optional_children: Vec<WorkId>,
    pub expected_contributors: Vec<SessionId>,
    pub contributions: Vec<RootContribution>,
    pub waivers: Vec<CompletionWaiver>,
    pub root_authority: Option<LifecycleAuthorityDecision>,
    pub drain: CompletionDrainAttestation,
    pub actor: ActorContext,
    pub completed_at: DateTime<Utc>,
}

/// Compact candidate returned by readiness queries.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReadyWork {
    pub work: WorkItem,
    pub availability: WorkAvailability,
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
    /// transition. Older events omit it and require full-history validation.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub relation_fingerprint: Option<ObjectHash>,
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
        authority_grant: ObjectHash,
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
        authority_grant: ObjectHash,
    },
    Released {
        claim_id: WorkClaimId,
        fence: i64,
        #[serde(default)]
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
        #[serde(default)]
        reason: String,
    },
    HandedOff {
        offer_id: WorkHandoffOfferId,
        claim_id: WorkClaimId,
        from: SessionId,
        to: SessionId,
        fence: i64,
        checkpoint: ObjectHash,
        authority_grant: ObjectHash,
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
        authority_grant: ObjectHash,
    },
    RequiredChildWaived {
        child_id: WorkId,
        child_revision: i64,
        reason: String,
        authority_grant: ObjectHash,
    },
    Reopened {
        run_id: WorkRunId,
        generation: i64,
        authority: LifecycleAuthorityDecision,
        #[serde(default)]
        reason: String,
    },
}

/// Request to create a root or child work item.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CreateWorkRequest {
    pub project_id: ProjectId,
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
    pub authority_policy_ref: String,
    pub authority: LifecycleAuthorityDecision,
    pub actor: ActorContext,
    pub idempotency_key: String,
    pub created_at: DateTime<Utc>,
}

/// One direct child proposed during an atomic decomposition.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ChildWorkDraft {
    pub local_key: String,
    pub child_requirement: ChildRequirement,
    pub title: String,
    pub outcome: String,
    pub acceptance: Vec<String>,
    pub kind: WorkItemKind,
    pub priority: i32,
    pub labels: Vec<String>,
    pub assigned_to: Option<String>,
    pub deferred_until: Option<DateTime<Utc>>,
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
pub struct WorkRevisionPatch {
    pub title: Option<String>,
    pub outcome: Option<String>,
    pub acceptance: Option<Vec<String>>,
    pub priority: Option<i32>,
    pub labels: Option<Vec<String>>,
    pub assigned_to: Option<String>,
    #[serde(default)]
    pub clear_assignment: bool,
    pub deferred_until: Option<DateTime<Utc>>,
    #[serde(default)]
    pub clear_deferral: bool,
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
    pub expected_run_id: WorkRunId,
    pub holder: SessionId,
    pub ttl_seconds: i64,
    pub authority: LifecycleAuthorityDecision,
    pub recovery_authority: Option<LifecycleAuthorityDecision>,
    pub recovery_reason: Option<String>,
    pub actor: ActorContext,
    pub idempotency_key: String,
    pub claimed_at: DateTime<Utc>,
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
    pub waiver_authority: Option<LifecycleAuthorityDecision>,
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
    pub evidence: Option<Vec<ObjectHash>>,
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
    pub authority: LifecycleAuthorityDecision,
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

/// Request to seal a run at a fenced, evidence-backed completion cut.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CompleteWorkRequest {
    pub work_id: WorkId,
    pub run_id: WorkRunId,
    pub holder: SessionId,
    pub expected_work_revision: i64,
    pub claim_id: WorkClaimId,
    pub claim_fence: i64,
    pub evidence: Vec<ObjectHash>,
    pub acceptance: Vec<AcceptanceResult>,
    pub drain: CompletionDrainAttestation,
    pub root_authority: Option<LifecycleAuthorityDecision>,
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
    pub authority: LifecycleAuthorityDecision,
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
    pub authority: LifecycleAuthorityDecision,
    pub actor: ActorContext,
    pub idempotency_key: String,
    pub disposed_at: DateTime<Utc>,
}

/// Explicit completion-barrier waiver for a cancelled required child.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WaiveRequiredChildRequest {
    pub parent_id: WorkId,
    pub child_id: WorkId,
    pub expected_parent_revision: i64,
    pub reason: String,
    pub authority: LifecycleAuthorityDecision,
    pub actor: ActorContext,
    pub idempotency_key: String,
    pub waived_at: DateTime<Utc>,
}

/// Host/operator-private request to waive one exact open work obligation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WaiveWorkObligationRequest {
    pub obligation_id: WorkObligationId,
    pub expected_definition: ObjectHash,
    /// Human operator identity asserted by the host. The request actor remains
    /// the server-fixed control session that presented this authority.
    pub waived_by: String,
    pub reason: String,
    pub authority: LifecycleAuthorityDecision,
    pub actor: ActorContext,
    pub idempotency_key: String,
    pub waived_at: DateTime<Utc>,
}

/// Stable host-private policy refusal for an obligation waiver request.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkObligationWaiverRefusalCode {
    WaiverNotAdmitted,
    ObligationNotOpen,
    DefinitionChanged,
}

/// Redacted durable receipt for a host-authorized obligation waiver.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkObligationWaiverReceipt {
    pub obligation_id: WorkObligationId,
    pub definition: ObjectHash,
    pub resolution: ObjectHash,
    pub state: WorkObligationState,
    pub waived_by: String,
    pub waived_at: DateTime<Utc>,
}

/// Host-private result. Policy outcomes are replayable typed values; routing,
/// token, and idempotency faults remain transport errors.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "decision", rename_all = "snake_case")]
pub enum WorkObligationWaiverDecision {
    Waived {
        receipt: WorkObligationWaiverReceipt,
    },
    Refused {
        code: WorkObligationWaiverRefusalCode,
        obligation_id: WorkObligationId,
        #[serde(skip_serializing_if = "Option::is_none")]
        current_definition: Option<ObjectHash>,
        remedy: String,
    },
}

/// What a memory means independently of how it is delivered.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryKind {
    Constraint,
    Decision,
    Convention,
    Fact,
    Preference,
    Episode,
}

/// Strength of the memory's instruction or assertion.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Authority {
    Hard,
    Firm,
    Soft,
}

/// Default context-delivery behavior; policy may override it with a reason.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Delivery {
    Pinned,
    Index,
    OnDemand,
    Suppressed,
}

/// Scope supported by the local V1 backend.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Scope {
    Project {
        project: ProjectId,
    },
    Task {
        project: ProjectId,
        task: TaskId,
    },
    Work {
        project: ProjectId,
        work: WorkId,
    },
    Agent {
        project: ProjectId,
        task: Option<TaskId>,
        #[serde(default)]
        work: Option<WorkId>,
        agent: String,
    },
}

impl Scope {
    /// Returns the task whose working set this scope belongs to, if any.
    #[must_use]
    pub fn task_id(&self) -> Option<TaskId> {
        match self {
            Self::Task { task, .. }
            | Self::Agent {
                task: Some(task), ..
            } => Some(*task),
            Self::Project { .. } | Self::Work { .. } | Self::Agent { task: None, .. } => None,
        }
    }

    /// Returns the local work identity this memory belongs to, if any.
    #[must_use]
    pub fn work_id(&self) -> Option<WorkId> {
        match self {
            Self::Work { work, .. }
            | Self::Agent {
                work: Some(work), ..
            } => Some(*work),
            Self::Project { .. } | Self::Task { .. } | Self::Agent { work: None, .. } => None,
        }
    }

    /// Whether the scope is visible to every participant of its task.
    #[must_use]
    pub fn is_task_shared(&self) -> bool {
        matches!(self, Self::Task { .. })
    }

    /// Whether the scope is visible to every participant of local work.
    #[must_use]
    pub fn is_work_shared(&self) -> bool {
        matches!(self, Self::Work { .. })
    }
}

/// Lifecycle state for a memory head.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryStatus {
    Proposed,
    Active,
    Contested,
    Stale,
    Retracted,
    Expired,
    Tombstoned,
}

/// Assurance attached to actor and authority text supplied by the host.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum AssuranceLevel {
    Asserted,
    Authenticated,
    Signed,
}

/// Retrieval classification applied before context is assembled.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum Sensitivity {
    Public,
    Internal,
    Restricted,
    SecretRef,
}

/// How an assertion reached the actor recording it.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProvenanceRelation {
    AssertedBy,
    RelayedBy,
    DerivedFrom,
}

/// One retained hop in an assertion's provenance chain.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProvenanceLink {
    pub relation: ProvenanceRelation,
    pub source: String,
    pub reference: Option<String>,
}

/// Fingerprint of mutable source material as it was observed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SourceSnapshot {
    pub source_ref: String,
    pub fingerprint: String,
    pub observed_at: DateTime<Utc>,
}

/// Selected external fields retained when organizational work is imported.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkSourceProjection {
    pub title: Option<String>,
    pub body: Option<String>,
    pub status: Option<String>,
    pub owner: Option<String>,
}

/// Immutable, backend-neutral provenance for one explicit work import.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct WorkSourceSnapshot {
    pub schema_version: u16,
    pub adapter_kind: String,
    pub canonical_ref: String,
    pub projected: WorkSourceProjection,
    pub captured_at: DateTime<Utc>,
    pub source_revision: Option<String>,
    pub fingerprint: String,
    pub canonical_url: Option<String>,
    pub payload_hash: ObjectHash,
    #[serde(default)]
    pub raw: BTreeMap<String, Value>,
}

/// Attribution retained on every durable object.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ActorContext {
    pub actor_id: String,
    pub actor_kind: String,
    pub assurance: AssuranceLevel,
    pub run_id: Option<String>,
    pub session_id: Option<SessionId>,
    pub source_tool: Option<String>,
    pub source_skill: Option<String>,
    #[serde(default)]
    pub provenance_chain: Vec<ProvenanceLink>,
    pub reason: String,
}

/// Immutable content of one memory version. Its object hash is stored outside
/// this payload so identity is computed over canonical content only.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MemoryVersion {
    pub schema_version: u16,
    pub memory_id: MemoryId,
    pub parents: Vec<ObjectHash>,
    pub kind: MemoryKind,
    pub authority: Authority,
    pub delivery: Delivery,
    pub scope: Scope,
    pub title: String,
    pub body: String,
    pub structured_value: Option<Value>,
    pub tags: Vec<String>,
    pub evidence: Vec<ObjectHash>,
    pub refs: Vec<String>,
    pub source_snapshot: Option<SourceSnapshot>,
    pub confidence: Option<f64>,
    pub sensitivity: Sensitivity,
    pub classification_reason: String,
    pub delivery_override_reason: Option<String>,
    pub valid_from: Option<DateTime<Utc>>,
    pub valid_until: Option<DateTime<Utc>>,
    pub review_by: Option<DateTime<Utc>>,
    pub last_verified: Option<DateTime<Utc>>,
    pub actor: ActorContext,
    pub created_at: DateTime<Utc>,
}

/// Initial activation decision for a memory version. Status remains derived
/// from immutable events; this object is the first event in that history.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MemoryAssertionEvent {
    pub schema_version: u16,
    pub memory_id: MemoryId,
    pub version: ObjectHash,
    pub status: MemoryStatus,
    pub policy_reason: String,
    pub actor: ActorContext,
    pub created_at: DateTime<Utc>,
}

/// Visibility override for low-friction prose capture.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NoteVisibility {
    #[default]
    Shared,
    Private,
}

/// Common capture request used by the CLI and MCP surface.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct NoteRequest {
    pub project_id: ProjectId,
    pub task_id: Option<TaskId>,
    #[serde(default)]
    pub work_id: Option<WorkId>,
    pub prose: String,
    #[serde(default)]
    pub visibility: NoteVisibility,
    pub kind: Option<MemoryKind>,
    pub authority: Option<Authority>,
    pub sensitivity: Option<Sensitivity>,
    pub title: Option<String>,
    #[serde(default)]
    pub tags: Vec<String>,
    #[serde(default)]
    pub evidence: Vec<ObjectHash>,
    #[serde(default)]
    pub refs: Vec<String>,
    pub actor: ActorContext,
    pub idempotency_key: String,
    pub created_at: DateTime<Utc>,
}

/// Explainable receipt returned after prose capture.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NoteReceipt {
    pub idempotency_key: String,
    pub memory_id: MemoryId,
    pub version: ObjectHash,
    pub assertion: ObjectHash,
    pub status: MemoryStatus,
    pub kind: MemoryKind,
    pub authority: Authority,
    pub delivery: Delivery,
    pub scope: Scope,
    pub cursor: Option<ChangeCursor>,
    #[serde(default)]
    pub work_positions: Vec<FeedPosition>,
    pub classification_reason: String,
    pub policy_reason: String,
    pub duplicate: bool,
}

/// Immutable declaration that two memory versions cannot both guide action.
/// Hash ordering is canonicalized before this object is frozen.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MemoryContradictionEvent {
    pub schema_version: u16,
    #[serde(default)]
    pub project_id: Option<ProjectId>,
    #[serde(default)]
    pub task_id: Option<TaskId>,
    #[serde(default)]
    pub work_root_id: Option<WorkId>,
    pub left_version: ObjectHash,
    pub right_version: ObjectHash,
    pub reason: String,
    pub actor: ActorContext,
    pub created_at: DateTime<Utc>,
}

/// Idempotent result of declaring an explicit contradiction.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MemoryContradictionReceipt {
    pub idempotency_key: String,
    pub contradiction: ObjectHash,
    pub left_version: ObjectHash,
    pub right_version: ObjectHash,
    #[serde(default)]
    pub cursor: Option<ChangeCursor>,
    #[serde(default)]
    pub work_positions: Vec<FeedPosition>,
    pub duplicate: bool,
}

/// Compact, explainable memory view used by search and context indexes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct MemorySummary {
    pub memory_id: MemoryId,
    pub version: ObjectHash,
    pub status: MemoryStatus,
    pub kind: MemoryKind,
    pub authority: Authority,
    pub delivery: Delivery,
    pub scope: Scope,
    pub title: String,
    pub body: String,
    pub sensitivity: Sensitivity,
    pub created_at: DateTime<Utc>,
}

/// Local task lifecycle. External trackers remain authoritative for the
/// organizational work item referenced by `external_ref`.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum TaskState {
    Active,
    Quiescing,
    FinalizationPending,
    ReportReady,
    Publishing,
    Published,
}

/// One participant's state at the task finalization barrier.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum ParticipantReadiness {
    Working {
        participant: String,
    },
    Ready {
        participant: String,
        contribution: ObjectHash,
    },
    Waived {
        participant: String,
        waived_by: String,
        reason: String,
    },
}

impl ParticipantReadiness {
    /// Returns the participant represented by this barrier entry.
    #[must_use]
    pub fn participant(&self) -> &str {
        match self {
            Self::Working { participant }
            | Self::Ready { participant, .. }
            | Self::Waived { participant, .. } => participant,
        }
    }

    /// Whether this participant is accounted for before report freeze.
    #[must_use]
    pub fn is_accounted_for(&self) -> bool {
        matches!(self, Self::Ready { .. } | Self::Waived { .. })
    }
}

/// Barrier that prevents a coordinator from freezing a report while a peer
/// is still contributing, unless that omission is explicitly waived.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FinalizationBarrier {
    pub task_id: TaskId,
    pub participants: Vec<ParticipantReadiness>,
}

impl FinalizationBarrier {
    /// Whether every expected participant contributed or was explicitly
    /// waived. An empty participant list is invalid and never ready.
    #[must_use]
    pub fn is_satisfied(&self) -> bool {
        !self.participants.is_empty()
            && self
                .participants
                .iter()
                .all(ParticipantReadiness::is_accounted_for)
    }

    /// Participants whose contribution still blocks report freeze.
    #[must_use]
    pub fn waiting_on(&self) -> Vec<&str> {
        self.participants
            .iter()
            .filter(|participant| !participant.is_accounted_for())
            .map(ParticipantReadiness::participant)
            .collect()
    }
}

/// Current exclusive execution claim. The mutable row is a coordination
/// projection; every transition also emits an immutable task event.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskLease {
    pub task_id: TaskId,
    pub lease_id: String,
    pub holder: SessionId,
    pub idempotency_key: String,
    /// Original requested duration. It fingerprints a retry independently of
    /// the wall-clock instant at which the transport repeats the call.
    #[serde(default)]
    pub ttl_seconds: i64,
    pub expires_at: DateTime<Utc>,
    pub revision: i64,
}

/// Immutable audit event for task ownership transitions.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskClaimEvent {
    pub schema_version: u16,
    pub lease: TaskLease,
    pub previous_holder: Option<SessionId>,
    pub actor: ActorContext,
    pub created_at: DateTime<Utc>,
}

/// Header returned with a context packet. The hash reproduces content; the
/// cursor orders later peer changes.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextPacketHeader {
    pub project_id: ProjectId,
    pub task_id: Option<TaskId>,
    #[serde(default)]
    pub work_id: Option<WorkId>,
    #[serde(default)]
    pub work_feed_heads: Vec<FeedPosition>,
    /// Monotonic fence for project-visible memory that is not ordered by a
    /// task or work feed.
    #[serde(default)]
    pub project_context_revision: i64,
    /// Monotonic, owner-private fence. The revision reveals no private object
    /// identity and is scoped to the packet's project and agent.
    #[serde(default)]
    pub private_context_revision: i64,
    pub packet_hash: ObjectHash,
    pub event_cursor: ChangeCursor,
    pub proposed_count: u32,
    pub stale_count: u32,
}

/// One memory included in a context packet with an auditable retrieval basis.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextItem {
    pub memory_id: MemoryId,
    pub version: ObjectHash,
    pub kind: MemoryKind,
    pub authority: Authority,
    pub status: MemoryStatus,
    pub title: String,
    pub body: Option<String>,
    pub retrieval_reason: String,
}

/// Visible record excluded from a bounded packet.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextOmission {
    pub memory_id: MemoryId,
    pub version: ObjectHash,
    pub reason: String,
}

/// Count of additional omitted memories after the exact manifest is bounded.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextOmissionSummary {
    pub reason: String,
    pub count: u32,
}

/// Canonical packet content stored under the hash returned in the header.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextPacketPayload {
    pub schema_version: u16,
    pub project_id: ProjectId,
    pub task_id: Option<TaskId>,
    #[serde(default)]
    pub work_id: Option<WorkId>,
    #[serde(default)]
    pub work_feed_heads: Vec<FeedPosition>,
    #[serde(default)]
    pub project_context_revision: i64,
    #[serde(default)]
    pub private_context_revision: i64,
    pub agent_id: String,
    pub event_cursor: ChangeCursor,
    pub pinned: Vec<ContextItem>,
    pub index: Vec<ContextItem>,
    pub omissions: Vec<ContextOmission>,
    #[serde(default)]
    pub omission_summaries: Vec<ContextOmissionSummary>,
    pub proposed_count: u32,
    pub stale_count: u32,
    pub created_at: DateTime<Utc>,
}

/// Context result returned by CLI and MCP.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ContextPacket {
    pub header: ContextPacketHeader,
    pub pinned: Vec<ContextItem>,
    pub index: Vec<ContextItem>,
    pub omissions: Vec<ContextOmission>,
    #[serde(default)]
    pub omission_summaries: Vec<ContextOmissionSummary>,
}

/// Authorized full-memory view with its initial activation event.
#[derive(Clone, Debug, Deserialize, PartialEq, Serialize)]
pub struct MemoryRecord {
    pub version_hash: ObjectHash,
    pub assertion_hash: ObjectHash,
    pub version: MemoryVersion,
    pub assertion: MemoryAssertionEvent,
}

/// One ordered task-feed item, decoded enough for an agent to act on it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct DeltaItem {
    pub cursor: ChangeCursor,
    pub object_kind: String,
    pub object_hash: ObjectHash,
    pub memory: Option<MemorySummary>,
    pub object: Value,
}

/// Deterministic task delta; repeating the same request after restart returns
/// the same bytes while the underlying feed is unchanged.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskDelta {
    pub task_id: TaskId,
    pub after: ChangeCursor,
    pub cursor: ChangeCursor,
    pub changes: Vec<DeltaItem>,
}

/// Local operational task; this is a reference to, never a mirror of, an
/// external ticket.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct LocalTask {
    pub schema_version: u16,
    pub project_id: ProjectId,
    pub task_id: TaskId,
    pub title: String,
    pub external_ref: Option<String>,
    pub participants: Vec<SessionId>,
    pub state: TaskState,
    pub event_cursor: ChangeCursor,
    pub created_at: DateTime<Utc>,
    pub updated_at: DateTime<Utc>,
}

/// Immutable creation event for a local task bound to an external reference.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskStartedEvent {
    pub schema_version: u16,
    pub task_id: TaskId,
    pub project_id: ProjectId,
    pub title: String,
    pub external_ref: String,
    pub participant: SessionId,
    pub actor: ActorContext,
    pub created_at: DateTime<Utc>,
}

/// Immutable event emitted the first time another session joins a task.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskJoinedEvent {
    pub schema_version: u16,
    pub task_id: TaskId,
    pub participant: SessionId,
    pub actor: ActorContext,
    pub created_at: DateTime<Utc>,
}

/// Ref-bound task result shared by CLI and MCP.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct TaskBindReceipt {
    pub task: LocalTask,
    pub joined: bool,
    pub cursor: ChangeCursor,
}

/// Structured report sections frozen before any publication attempt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ReportSections {
    pub outcome: String,
    pub work_performed: String,
    pub decisions: String,
    pub constraints_and_conventions: String,
    pub validation_and_evidence: String,
    pub unresolved_follow_ups: String,
    pub promotion_candidates: String,
    pub provenance: String,
}

/// Immutable report payload. Publication binds the resulting object hash to
/// exactly one idempotency key.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct FrozenReport {
    pub schema_version: u16,
    pub report_id: Uuid,
    pub task_id: TaskId,
    pub supersedes: Option<ObjectHash>,
    pub source_memory_versions: Vec<ObjectHash>,
    pub participant_contributions: Vec<ObjectHash>,
    pub waived_participants: Vec<String>,
    pub sections: ReportSections,
    pub actor: ActorContext,
    pub created_at: DateTime<Utc>,
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash(seed: &str) -> ObjectHash {
        ObjectHash::from_canonical_bytes(seed.as_bytes())
    }

    #[test]
    fn finalization_waits_for_every_participant() {
        let task_id = TaskId::new();
        let barrier = FinalizationBarrier {
            task_id,
            participants: vec![
                ParticipantReadiness::Ready {
                    participant: "codex".into(),
                    contribution: hash("codex contribution"),
                },
                ParticipantReadiness::Working {
                    participant: "fable".into(),
                },
            ],
        };

        assert!(!barrier.is_satisfied());
        assert_eq!(barrier.waiting_on(), vec!["fable"]);
    }

    #[test]
    fn explicit_waiver_satisfies_the_barrier() {
        let barrier = FinalizationBarrier {
            task_id: TaskId::new(),
            participants: vec![
                ParticipantReadiness::Ready {
                    participant: "codex".into(),
                    contribution: hash("codex contribution"),
                },
                ParticipantReadiness::Waived {
                    participant: "fable".into(),
                    waived_by: "coordinator".into(),
                    reason: "session ended before contributing".into(),
                },
            ],
        };

        assert!(barrier.is_satisfied());
        assert!(barrier.waiting_on().is_empty());
    }

    #[test]
    fn legacy_work_transitions_without_audit_reasons_still_decode() {
        let released = WorkTransition::Released {
            claim_id: WorkClaimId::new(),
            fence: 2,
            reason: "intentional release".into(),
        };
        let cancelled = WorkTransition::HandoffCancelled {
            offer_id: WorkHandoffOfferId::new(),
            offer: hash("cancelled offer"),
            reason: "recipient unavailable".into(),
        };
        let reopened = WorkTransition::Reopened {
            run_id: WorkRunId::new(),
            generation: 2,
            authority: LifecycleAuthorityDecision {
                grant: hash("reopen authority"),
            },
            reason: "new generation".into(),
        };
        for transition in [released, cancelled, reopened] {
            let mut value = serde_json::to_value(transition).expect("serialize transition");
            value
                .as_object_mut()
                .expect("tagged transition object")
                .remove("reason");
            let decoded: WorkTransition =
                serde_json::from_value(value).expect("decode legacy transition");
            match decoded {
                WorkTransition::Released { reason, .. }
                | WorkTransition::HandoffCancelled { reason, .. }
                | WorkTransition::Reopened { reason, .. } => assert!(reason.is_empty()),
                _ => panic!("unexpected legacy transition"),
            }
        }
    }

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
