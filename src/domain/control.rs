//! Project policy, execution observations, verification evidence and source identity.

use chrono::{DateTime, Utc};
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;

use crate::ObjectId;

mod named_root_state;

use super::{
    AcceptanceEvaluationPolicy, ActorContext, FeedPosition, ProjectId, RootExecutionId, SessionId,
    WorkClaimId, WorkClaimState, WorkId, WorkRunId, WorkRunState,
};

/// Monotonic invalidation epoch for the active project control policy.
#[derive(Clone, Copy, Debug, Default, Deserialize, Eq, PartialEq, Serialize)]
#[serde(transparent)]
pub struct ProjectPolicyEpoch(pub i64);

/// Historical assurance values preserved in immutable policy data. They do
/// not select an execution-admission mode in the current runtime.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Ord, PartialEq, PartialOrd, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ControlAssurance {
    Advisory,
    TurnGated,
    ActionGated,
}

/// Project-scoped administrative operation attributed to one immutable
/// control-policy authority decision.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum ProjectPolicyOperation {
    InitializeProjectPolicy,
    /// Decode historical policy attribution without restoring its operation.
    SetRequiredAssurance,
    SetObligationRuleSet,
    SetAcceptanceEvaluation,
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
    pub previous_policy: Option<ObjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_assurance: Option<ControlAssurance>,
    pub obligation_rule_set: ObjectId,
    /// Acceptance-evaluation policy in force after this decision; omitted
    /// bytes mean the self-asserted path.
    #[serde(
        default,
        skip_serializing_if = "AcceptanceEvaluationPolicy::is_self_asserted"
    )]
    pub acceptance_evaluation: AcceptanceEvaluationPolicy,
    pub authorized_by: ActorContext,
    pub decided_at: DateTime<Utc>,
}

/// Immutable project policy for obligations and acceptance evaluation.
/// Optional admission metadata is read from historical versions only.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ControlPolicy {
    pub schema_version: u16,
    pub control_schema_version: u16,
    pub policy_epoch: ProjectPolicyEpoch,
    pub previous_policy: Option<ObjectId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub required_assurance: Option<ControlAssurance>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supported_effects: Option<Vec<EffectClass>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub grant_ttl_seconds: Option<i64>,
    pub obligation_rule_set: ObjectId,
    /// Per-project acceptance-evaluation policy; omitted bytes mean the
    /// self-asserted completion path, so earlier policy objects keep
    /// their exact bytes and hashes.
    #[serde(
        default,
        skip_serializing_if = "AcceptanceEvaluationPolicy::is_self_asserted"
    )]
    pub acceptance_evaluation: AcceptanceEvaluationPolicy,
    pub authority: ObjectId,
    pub activated_at: DateTime<Utc>,
}

/// Material effect classes used by host capability mediation.
#[derive(Clone, Copy, Debug, Deserialize, Eq, Hash, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EffectClass {
    Observe,
    Communicate,
    /// Engram-internal coordination, separate from filesystem mutation.
    Coordinate,
    MutateLocal,
    MutateShared,
    ExternalSideEffect,
    Lifecycle,
}

/// Exact work, run and claim association captured by the embedding host.
/// Storage validates its holder, fence and source context for ordinary operations.
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

/// A host-reported transition in one claim's named source-root lifecycle.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NamedRootBindingKind {
    Bound,
    Ended,
}

/// Why an existing named source-root selection ended.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum NamedRootEndReason {
    ExplicitClear,
    SessionGoneAtRestore,
    RootInvalid,
}

/// State carried by a host sighting after its claim has had a named root.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceRootState {
    Named,
    Ended,
}

/// Host-recorded named-root selection or explicit end for one claim. The
/// source basis records the generation seen at capture, so a delayed
/// checkpoint cannot place an earlier sighting after a later name. Whether
/// that generation had ended or been released is read from the events
/// recorded before the sighting on the run feed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct NamedRootBindingEvent {
    pub schema_version: u16,
    pub project_id: ProjectId,
    pub root_execution_id: RootExecutionId,
    pub work_id: WorkId,
    pub run_id: WorkRunId,
    pub claim_id: WorkClaimId,
    /// Historical disclosure only; claim renewal does not invalidate a root.
    pub claim_fence: i64,
    pub session_id: SessionId,
    pub workspace_id: String,
    pub generation: i64,
    pub named_at: DateTime<Utc>,
    pub kind: NamedRootBindingKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_reason: Option<NamedRootEndReason>,
    pub actor: ActorContext,
    pub recorded_at: DateTime<Utc>,
}

/// Host-private receipt for one durable named-root selection.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NamedRootBindingReceipt {
    pub event: ObjectId,
    pub position: FeedPosition,
    pub workspace_id: String,
    pub generation: i64,
    pub kind: NamedRootBindingKind,
}

/// A claim's named-root binding as Engram derives it at a cut from the
/// recorded `named_root_bind` events and the claim's own lifecycle: the
/// host's authoritative read for when to name a root again. The variant set
/// is closed.
///
/// Decoding is variant-aware (see `named_root_state`): a field the state does
/// not name, or a repeated key, is refused rather than dropped.
#[derive(Clone, Debug, Eq, PartialEq, Serialize)]
#[serde(tag = "state", rename_all = "snake_case")]
pub enum NamedRootState {
    /// No root is bound: none was named, the latest one was ended, or the
    /// claim's run has completed or been disposed since.
    #[serde(rename = "none")]
    NoRoot,
    /// The claim's latest named root, still bound.
    Bound {
        workspace_id: String,
        generation: i64,
        named_at: DateTime<Utc>,
    },
    /// The claim was released after its latest name; a sighting stating that
    /// generation is unbound until a later name mints a fresh one.
    UnboundByRelease {
        last_generation: i64,
        /// Run-feed position of the release.
        released_at_position: i64,
    },
}

/// A host's read of one claim's named root on its run, taken from one
/// snapshot: the run and claim it names, their lifecycle, the named-root
/// state Engram derives for the claim, the claim's newest recorded root
/// event, and the run-feed cut all of it was read at. It needs no live
/// holder and changes nothing.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NamedRootRead {
    pub project_id: ProjectId,
    pub work_id: WorkId,
    pub root_execution_id: RootExecutionId,
    pub run_id: WorkRunId,
    pub claim_id: WorkClaimId,
    pub run: NamedRootReadRun,
    pub claim: NamedRootReadClaim,
    pub named_root: NamedRootState,
    /// The claim's newest recorded root event, bound or ended, at the cut;
    /// `None` only when the claim never had one. It is kept when the derived
    /// state is none, so a host can tell an ended or finished root from one
    /// never named.
    pub latest_event: Option<NamedRootEventReference>,
    pub read_cut: FeedPosition,
}

/// The run's lifecycle as the read saw it.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NamedRootReadRun {
    pub state: WorkRunState,
    pub generation: i64,
}

/// The claim's lifecycle as the read saw it. An expired claim still reads
/// as its stored state: expiry is disclosed, never derived into another state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NamedRootReadClaim {
    pub state: WorkClaimState,
    pub holder: SessionId,
    pub expires_at: DateTime<Utc>,
    pub revision: i64,
    pub fence: i64,
}

/// One recorded `named_root_bind` event, by its real record id and run-feed
/// position. A read, not a binding receipt.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct NamedRootEventReference {
    pub event: ObjectId,
    pub position: FeedPosition,
    pub generation: i64,
    pub kind: NamedRootBindingKind,
    pub workspace_id: String,
    pub named_at: DateTime<Utc>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub end_reason: Option<NamedRootEndReason>,
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

/// How the host established a source change it reports. It is the host's
/// assertion about its own method; the core cannot check it and draws no
/// conclusion from it: every value opens the obligations a change opens.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum SourceChangeDetection {
    /// Two content revisions were compared and differed.
    ContentComparison,
    /// The earlier revision was missing, so the change is assumed.
    AssumedMissingBaseline,
    /// No closing revision could be taken; file notifications decided. Such
    /// an observation carries no source basis.
    WatcherOnly,
}

impl SourceChangeDetection {
    /// The value as the host protocol spells it.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ContentComparison => "content_comparison",
            Self::AssumedMissingBaseline => "assumed_missing_baseline",
            Self::WatcherOnly => "watcher_only",
        }
    }
}

/// Host-selected source identity carried by an execution observation.
///
/// A0 preserves this optional basis without treating it as verification. A1
/// requires and validates it against the verification evidence state.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct ExecutionSourceBasis {
    /// Present only when the host's same capture measured a clean Git tree.
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "super::verification_applicability::present_git_tree"
    )]
    pub git_tree: Option<super::GitTreeId>,
    pub workspace_id: String,
    pub source_revision: String,
    /// Host capture-time generation and state of this claim's named root.
    /// Both are absent for a sighting taken before its first name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_root_generation: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_root_state: Option<SourceRootState>,
}

/// Whether `character` is refused in NEW host-supplied source text (a
/// workspace id or source revision): a C0 or C1 control character, or a
/// character of Unicode's `Bidi_Control` property. Other format characters,
/// such as the zero-width joiners, variation selectors and the soft hyphen,
/// stay accepted, and accepted text is kept byte for byte. Text already
/// stored is read as stored and never checked against this.
#[must_use]
pub fn is_refused_source_text_char(character: char) -> bool {
    character.is_control()
        || matches!(
            character,
            '\u{061C}' | '\u{200E}' | '\u{200F}' | '\u{202A}'..='\u{202E}' | '\u{2066}'..='\u{2069}'
        )
}

/// Whether `text` holds a character [`is_refused_source_text_char`] refuses.
#[must_use]
pub fn holds_refused_source_text(text: &str) -> bool {
    text.chars().any(is_refused_source_text_char)
}

/// Immutable historical execution fact on its exact bound run feed.
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
    pub action_fingerprint: ObjectId,
    pub effect: EffectClass,
    pub outcome: ExecutionOutcome,
    /// Whether the source changed: the core's reading of the host's report.
    /// It is true when the host reported a change, unless the reported
    /// revision equals the revision of the run's newest recorded source
    /// change and no host record on the run since that change (observation or
    /// environment evidence) carried another revision; such
    /// a report (as a host may send for writes under git-ignored paths) is
    /// recorded as no change. When the report or that newest change carries
    /// no revision, or the run has recorded no change, the host's report
    /// stands. Observations recorded before this rule keep the host's literal
    /// flag and are read as stored.
    pub source_changed: bool,
    /// How the host established the change it reported, as the host said
    /// it. It describes the host's report, so it stays when the core reads
    /// that report as no change. Absent when the host did not say, which is
    /// every observation recorded before hosts could; absence never means
    /// that the change was measured.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_source_change: Option<SourceChangeDetection>,
    /// Exact immutable rule set retained from the recorded policy basis.
    pub obligation_rule_set: ObjectId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_basis: Option<ExecutionSourceBasis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub observed_at: Option<DateTime<Utc>>,
    pub actor: ActorContext,
    pub recorded_at: DateTime<Utc>,
}

/// Host-declared class of one verification command or check.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
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

/// Immutable host-minted evidence that one check ran against an exact content
/// state. It is the only evidence kind that may satisfy a verification
/// requirement.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationEvidence {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "super::verification_applicability::present_applicability"
    )]
    pub applicability: Option<super::VerificationApplicability>,
    pub schema_version: u16,
    pub project_id: ProjectId,
    pub binding: ControlWorkBinding,
    pub session_id: SessionId,
    pub producer_observation: ObjectId,
    pub source_basis: super::VerificationSourceBasis,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub environment: Option<ObjectId>,
    pub check_kind: VerificationKind,
    pub check_fingerprint: ObjectId,
    pub result: VerificationResult,
    pub completed_at: DateTime<Utc>,
    pub summary: String,
    pub refs: Vec<String>,
    pub actor: ActorContext,
    pub recorded_at: DateTime<Utc>,
    /// Present only on a record that binds an earlier native check to this
    /// item: the check ran once, on the original's run, and this record
    /// credits it here. Absent on every native record, whose canonical bytes
    /// are therefore unchanged.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub bound_from: Option<BoundVerificationSource>,
}

impl VerificationEvidence {
    /// When this record stands on its own run: the check's completion for a
    /// native record, and the binding's time for a bound one, whose check
    /// completed earlier on another run. Comparisons with this run's admitted
    /// source changes use it, since a bound record was matched to their
    /// revision; an unadmitted change's time floor, like every other fact of
    /// the check itself, keeps `completed_at`.
    #[must_use]
    pub fn run_time(&self) -> DateTime<Utc> {
        if self.bound_from.is_some() {
            self.recorded_at
        } else {
            self.completed_at
        }
    }
}

/// Where a bound verification record's check ran and how it was bound here.
/// The binder is the record's `actor` and the binding's time its
/// `recorded_at`; the check's kind, fingerprint, result and completion time
/// are the original's, copied unchanged.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BoundVerificationSource {
    /// The original native verification record.
    pub verification: ObjectId,
    pub work_id: WorkId,
    pub run_id: WorkRunId,
    /// The original record's run-feed position on its own run, fixed when
    /// it was bound.
    pub original_position: i64,
    /// This item's newest source sighting when it was bound, whose revision
    /// equals the original check's.
    pub sighting: ObjectId,
    /// The host's measurement of the shared root when it bound the check.
    pub measurement: BindMeasurement,
    /// Criterion positions the binder intended the check for; provenance
    /// only, never a verdict.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub criteria: Vec<u32>,
}

/// A host-asserted measurement of a named root's content at bind time.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct BindMeasurement {
    pub workspace_id: String,
    pub source_revision: String,
    pub measured_at: DateTime<Utc>,
}

/// Exact verification property required by one immutable work obligation.
///
/// Builtin V1 rules require the verification kind while deliberately leaving
/// the command fingerprint open; a binding or rule may pin the fingerprint of
/// one exact check. A requirement cannot name an environment: the environment
/// evidence a verification links to belongs to one run and one source
/// revision, so a pin to it could not be satisfied anywhere else. Decoding is
/// strict, so a requirement that names any other member, such as an
/// environment, is refused by name rather than read without it.
#[derive(Clone, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub struct VerificationRequirement {
    pub check_kind: VerificationKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub check_fingerprint: Option<ObjectId>,
}

/// Immutable identity of one builtin obligation rule version.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct BuiltinObligationRuleRef {
    pub rule_id: String,
    pub rule_version: u16,
}

impl BuiltinObligationRuleRef {
    /// Whether `rule_id` is a rule id an obligation rule set admits:
    /// non-empty, at most 128 bytes, and equal to its own trim.
    #[must_use]
    pub fn admits_rule_id(rule_id: &str) -> bool {
        !rule_id.is_empty() && rule_id.len() <= 128 && rule_id.trim() == rule_id
    }
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
    pub environment_fingerprint: ObjectId,
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
    /// An explicit local use of retained proof, not a locally observed check.
    Imported,
}

/// Why one evidence object cannot satisfy an exact verification requirement.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum VerificationEvidenceMismatch {
    GitTreeMissing,
    GitTreeMismatch,
    WrongKind,
    CheckKindMismatch,
    WrongRun,
    StaleSourceRevision,
    CheckFingerprintMismatch,
    ResultNotPassed,
    InvalidTime,
    InvalidProducer,
    NotAfterMutation,
}

/// Which source record decided a `stale_source_revision` mismatch.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum StaleSourceDecider {
    /// The run's latest source change: without a named root, the newest
    /// flagged change, whose revision the check must carry.
    LatestChange,
    /// The active named root's newest sighting, whose revision the check
    /// must carry.
    RootSighting,
    /// The named root's binding: the check or its producer is not of the
    /// root's workspace and generation, or did not follow the binding.
    RootBinding,
    /// Without a named root, after an unadmitted change: the newest measured
    /// sighting in that change's workspace, whose revision the check must
    /// carry.
    MeasuredSighting,
}

/// The source record that decided a verification is stale, beside the
/// verification's own source, as the store recorded both. It is transient
/// read and refusal context, never stored. `None` means the record did not
/// carry that field; a binding names no revision and is no observation.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct StaleVerificationSource {
    pub decider: StaleSourceDecider,
    /// The deciding record's run-feed position.
    pub position: i64,
    /// For an observation, whether it reported a source change.
    pub source_changed: Option<bool>,
    pub workspace: Option<String>,
    pub revision: Option<String>,
    pub root_generation: Option<i64>,
    pub verification_workspace: String,
    pub verification_revision: String,
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

#[cfg(test)]
mod tests {
    use super::*;

    /// The named-root state's wire shape, which a host parses: an internally
    /// tagged `state` and fields in snake case.
    #[test]
    fn the_named_root_state_has_its_wire_shape() {
        let named_at = DateTime::parse_from_rfc3339("2026-09-29T01:00:00Z")
            .expect("time")
            .with_timezone(&Utc);
        for (state, json) in [
            (NamedRootState::NoRoot, serde_json::json!({"state": "none"})),
            (
                NamedRootState::Bound {
                    workspace_id: "workspace-B".into(),
                    generation: 9,
                    named_at,
                },
                serde_json::json!({
                    "state": "bound",
                    "workspace_id": "workspace-B",
                    "generation": 9,
                    "named_at": "2026-09-29T01:00:00Z",
                }),
            ),
            (
                NamedRootState::UnboundByRelease {
                    last_generation: 9,
                    released_at_position: 42,
                },
                serde_json::json!({
                    "state": "unbound_by_release",
                    "last_generation": 9,
                    "released_at_position": 42,
                }),
            ),
        ] {
            assert_eq!(serde_json::to_value(&state).expect("serialize"), json);
            assert_eq!(
                serde_json::from_value::<NamedRootState>(json).expect("deserialize"),
                state
            );
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
