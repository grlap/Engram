//! Agent-facing view and summary types projected from canonical local-work
//! state.

use super::{
    ActorContext, ChildRequirement, ControlWorkBinding, DateTime, Deserialize, FromStr, JsonSchema,
    ObjectHash, ProjectId, ProjectMemoryAdvertisement, Sensitivity, Serialize, SessionId, Utc,
    VerificationKind, VerificationResult, WorkAvailability, WorkBlockerKind, WorkClaim,
    WorkEvidenceKind, WorkFeedEntry, WorkHandoffState, WorkId, WorkItemKind, WorkLifecycle,
    WorkObligationState, WorkPrerequisiteState, WorkRunId, WorkRunState,
};

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
    pub memories: Option<ProjectMemorySignal>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivered_through: Option<i64>,
    /// Opaque capability required with `delivered_through` to acknowledge a
    /// staged page. Replay returns the same token; it is never exposed by an
    /// error or agent-session projection.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub delivery_token: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub omissions: Vec<WorkSectionOmission>,
    /// Exact advisory candidate retained only until the outer agent renderer
    /// confirms that the signal survived its tighter byte budget.
    #[serde(skip)]
    pub(crate) memory_advertisement: Option<ProjectMemoryAdvertisement>,
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
    /// Sections to return. Empty means the normal focus, ready, catalog,
    /// changes, and project-memory signal packet. A changes-free query never
    /// stages or advances delivery.
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
    /// Asserted host/client context generation. A changed value may reannounce
    /// the content-free project-memory signal without creating a delivery cursor.
    pub context_generation: Option<String>,
}

/// Advisory, content-free project-memory advertisement carried by next.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct ProjectMemorySignal {
    pub count: usize,
    pub changed: bool,
}

/// Selectable `work_next` response section.
#[derive(Clone, Copy, Debug, Deserialize, Eq, JsonSchema, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkNextSection {
    Focus,
    Ready,
    Catalog,
    Changes,
    Memories,
}

impl FromStr for WorkNextSection {
    type Err = &'static str;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "focus" => Ok(Self::Focus),
            "ready" => Ok(Self::Ready),
            "catalog" => Ok(Self::Catalog),
            "changes" => Ok(Self::Changes),
            "memories" => Ok(Self::Memories),
            _ => Err("expected focus, ready, catalog, changes, or memories"),
        }
    }
}

/// One hash-verified source object at an exact project-feed position, exposed
/// as an authority-redacted projection. `entry.object_hash` binds the original
/// canonical bytes; it intentionally does not hash the compact `delivery`.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkChange {
    pub entry: WorkFeedEntry,
    /// Derived from the verified source actor for this receiving session;
    /// persisted in the exact staged page without exposing session identity.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub from_current_session: bool,
    pub delivery: WorkChangeProjection,
}

/// Exact agent projection persisted beside an unacknowledged dense feed page.
/// Replays decode these bytes instead of rebuilding against mutable focus or
/// task bindings.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub(super) struct StagedWorkChangePage {
    pub(super) schema_version: u16,
    pub(super) changes: Vec<WorkChange>,
    pub(super) omitted_count: usize,
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
    /// Optional host-asserted execution context. Actor identity remains in
    /// `actor_id`; this field is attribution only.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_context: Option<String>,
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
    /// Dense change entries remain unconsumed for a later staged page.
    Staged,
    CountLimit,
    EvidenceCountLimit,
    UnfinishedChildCountLimit,
    TerminalChildCountLimit,
    DeadPrerequisiteCountLimit,
    PendingPrerequisiteCountLimit,
    SatisfiedPrerequisiteCountLimit,
}

/// Bounded work identity and planning fields used on the agent wire.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkItemSummary {
    pub work_id: WorkId,
    pub short_ref: String,
    pub root_id: WorkId,
    pub parent_id: Option<WorkId>,
    /// Present only when this item is an optional child. Required children
    /// and roots keep the common agent response compact.
    #[serde(
        default = "default_child_requirement",
        skip_serializing_if = "child_requirement_is_required"
    )]
    pub child_requirement: ChildRequirement,
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
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub restored: bool,
    pub revision: i64,
    pub active_run_id: Option<WorkRunId>,
    pub superseded_by: Option<WorkId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub prerequisite_state: Option<WorkPrerequisiteState>,
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

/// One compact inert history entry recreated from a work-graph snapshot.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct RestoredHistoryEntry {
    pub generation_index: usize,
    pub kind: String,
    pub summary: String,
    pub actor: ActorContext,
    pub created_at: DateTime<Utc>,
}

/// Bounded restored history with an exact source-entry count.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct RestoredHistoryView {
    pub total: usize,
    pub items: Vec<RestoredHistoryEntry>,
    pub omitted: usize,
}

/// Full bounded context for the ambient focused item.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkFocusView {
    pub session: AgentWorkSession,
    pub status: ReadyWorkSummary,
    /// True only while restored completion history, rather than a native
    /// completion seal, is the current completion authority.
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub completed_by_record: bool,
    /// The item's full outcome text; `status.work.outcome` is the compact
    /// summary used in lists.
    #[serde(default)]
    pub outcome: String,
    pub run: Option<WorkRunSummary>,
    pub claim: Option<WorkClaim>,
    /// Paste-ready native control binding for this session's live claim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub control_binding: Option<ControlWorkBinding>,
    pub children: Vec<WorkItemSummary>,
    /// Exact number of direct children before relation-count or byte-budget
    /// trimming. Unfinished children precede the terminal remainder in the
    /// bounded `children` prefix.
    #[serde(default)]
    pub child_count: usize,
    pub prerequisites: Vec<WorkItemSummary>,
    pub handoffs: Vec<WorkHandoffSummary>,
    pub blockers: Vec<WorkBlockerSummary>,
    pub evidence: Vec<ObjectHash>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub evidence_items: Vec<WorkEvidenceSummary>,
    /// Exact evidence membership count before the bounded focus selection.
    #[serde(default)]
    pub evidence_count: usize,
    /// Last evidence by authoritative order: native run-feed position when a
    /// run has evidence, otherwise restored per-item append position. Selected
    /// independently from the obligation-prioritized evidence page. Evidence
    /// timestamps are asserted metadata, never ordering authority. This
    /// fixed-size advisory is populated only for drill-down reads and retained
    /// while other focus rows trim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_evidence_item: Option<WorkEvidenceSummary>,
    pub obligation_page: WorkObligationPage,
    #[serde(default)]
    pub memories: Vec<WorkMemoryIndexEntry>,
    pub history: WorkHistoryView,
    /// Inert history imported from earlier stores. These entries never enter
    /// native feeds or completion authority.
    #[serde(default, skip_serializing_if = "restored_history_is_empty")]
    pub restored_history: RestoredHistoryView,
    /// Direct disposed required children for which the current project-bound
    /// caller can execute `work_update:waive_required_child` now.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub waivable_required_children: Vec<RequiredChildWaiverCandidate>,
    pub allowed_next: Vec<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub omissions: Vec<WorkSectionOmission>,
}

fn restored_history_is_empty(history: &RestoredHistoryView) -> bool {
    history.total == 0
}

/// Compact agent-facing summary of one canonical run evidence object.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkEvidenceSummary {
    pub evidence: ObjectHash,
    #[serde(default, skip_serializing_if = "std::ops::Not::not")]
    pub non_holder: bool,
    pub evidence_kind: WorkEvidenceKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub gate: Option<WorkGateEvidenceSummary>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub workspace_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_revision: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub producer_session_id: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub actor_context: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check_kind: Option<VerificationKind>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub check_fingerprint: Option<ObjectHash>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub verification_result: Option<VerificationResult>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment_fingerprint: Option<ObjectHash>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment: Option<ObjectHash>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub environment_components: Option<crate::EnvironmentComponents>,
    pub summary: String,
    pub created_at: DateTime<Utc>,
}

/// Typed gate discriminator retained beside the human-readable evidence line.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkGateEvidenceSummary {
    pub name: String,
    pub passed: bool,
    pub failed_count: usize,
}

/// Bounded agent-facing summary of one immutable run obligation.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkObligationSummary {
    pub obligation_id: crate::WorkObligationId,
    pub definition: ObjectHash,
    /// Exact immutable rule-set identity selected when the obligation opened.
    pub rule_set: ObjectHash,
    pub state: WorkObligationState,
    pub rule: crate::BuiltinObligationRuleRef,
    pub requirement: crate::VerificationRequirement,
    pub triggering_observation: ObjectHash,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub resolution: Option<ObjectHash>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub evidence: Option<ObjectHash>,
    /// Asserted human operator attribution for a waiver. The free-form waiver
    /// reason remains host-private.
    #[serde(skip_serializing_if = "Option::is_none")]
    pub waived_by: Option<String>,
    pub guidance: WorkObligationGuidance,
}

/// Count- and byte-bounded typed completion obligations. `omitted_count`
/// makes truncation explicit instead of overloading generic readiness prose.
#[derive(Clone, Debug, Default, Deserialize, Serialize)]
pub struct WorkObligationPage {
    pub items: Vec<WorkObligationSummary>,
    pub omitted_count: usize,
}

/// Deterministic next action derived from immutable obligation state and its
/// verification requirement. Waiver authority is intentionally absent.
#[derive(Clone, Debug, Deserialize, Serialize)]
#[serde(tag = "action", rename_all = "snake_case")]
pub enum WorkObligationGuidance {
    RecordVerificationThenCheckpoint {
        requirement: crate::VerificationRequirement,
        host_waiver_requestable: bool,
    },
    None,
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

fn default_child_requirement() -> ChildRequirement {
    ChildRequirement::Required
}

#[allow(
    clippy::trivially_copy_pass_by_ref,
    reason = "serde skip_serializing_if predicates receive a borrowed field"
)]
fn child_requirement_is_required(requirement: &ChildRequirement) -> bool {
    *requirement == ChildRequirement::Required
}
