//! Ambient six-operation protocol over the local work lifecycle.

use std::{
    fmt::{self, Write as _},
    path::PathBuf,
    str::FromStr,
    sync::{Mutex, MutexGuard, OnceLock},
};

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
    CompletionSeal, ControlWorkBinding, CreateWorkRequest, DEFAULT_WORK_CLAIM_TTL_SECONDS,
    DecomposeWorkRequest, DevelopmentNoopRedactor, DisposeWorkRequest, EnvironmentEvidence,
    ExecutionObservation, FeedId, FeedPosition, MemorySummary, MemoryVersion, ObjectHash,
    OfferWorkHandoffRequest, ProjectId, ReadyWork, RecordWorkEvidenceRequest, ReleaseWorkRequest,
    ReopenWorkRequest, ReviseWorkRequest, SessionId, SqliteStore, TaskId, VerificationEvidence,
    VerificationKind, VerificationResult, WaiveRequiredChildRequest, WorkAvailability,
    WorkBlockerKind, WorkCatalogQuery, WorkCheckpoint, WorkClaim, WorkClaimId, WorkClaimState,
    WorkCompletionRecovery, WorkDecomposition, WorkDependencyRef, WorkDisposition, WorkEvent,
    WorkEvidence, WorkEvidenceKind, WorkFeedEntry, WorkHandoffOffer, WorkHandoffState, WorkId,
    WorkItem, WorkItemKind, WorkLifecycle, WorkObligation, WorkObligationResolution,
    WorkObligationResolutionEvent, WorkObligationState, WorkOrigin, WorkPlanningAuthority,
    WorkPrerequisiteState, WorkRevisionPatch, WorkRun, WorkRunId, WorkRunState, WorkSessionState,
    WorkTransition,
    domain::{
        ACTOR_CONTEXT_NORMALIZED_REFERENCE, ACTOR_CONTEXT_PROVENANCE_REFERENCE, AssuranceLevel,
        ForgetProjectMemoryRequest, MAX_ACTOR_CONTEXT_BYTES, MemoryAssertionEvent,
        MemoryContradictionEvent, ProjectMemoryFull, ProjectMemoryList,
        ProjectMemoryMutationReceipt, ProvenanceLink, ProvenanceRelation,
        RecordGateEvidenceRequest, RecordWorkNoteRequest, RememberProjectMemoryRequest,
        SCHEMA_VERSION, Scope, Sensitivity, WorkCompletionRecoveryCause,
        is_unsafe_rendered_text_char, validate_gate_evidence_payload,
    },
    storage::{
        BeginGateWorkProtocolAttempt, BeginWorkProtocolAttempt, CompleteWorkStorageResult,
        CompletionRecoverySnapshot, ProjectMemoryAdvertisement, StageWorkSessionDelivery,
        StoreError, WorkEvidenceProjectionSummary, WorkNoteCapture,
        normalize_completion_acceptance_shape,
    },
};

#[cfg(test)]
use crate::WorkReferenceCandidate;

/// Hard ceiling for every successful agent-facing work response.
pub const MAX_AGENT_WORK_RESPONSE_BYTES: usize = 12 * 1024;

const MAX_PROJECT_MEMORY_FULL_BYTES: usize = 12 * 1024;
const _: () = assert!(MAX_PROJECT_MEMORY_FULL_BYTES <= MAX_AGENT_WORK_RESPONSE_BYTES);

const MAX_CHANGE_SECTION_BYTES: usize = 4 * 1024;
const MAX_READY_SECTION_BYTES: usize = 2 * 1024;
const MAX_CATALOG_SECTION_BYTES: usize = 3 * 1024;
const MAX_OBLIGATION_PAGE_BYTES: usize = 4 * 1024;
const MAX_FOCUS_HISTORY: u32 = 4;
const MAX_FOCUS_RELATIONS: usize = 8;
const MAX_FOCUS_MEMORIES: u32 = 8;
const MAX_SUMMARY_BYTES: usize = 192;
const MAX_HISTORY_TITLE_BYTES: usize = 72;
const MAX_HISTORY_DETAIL_BYTES: usize = 72;
const MAX_ACCEPTANCE_ITEMS: usize = 6;
const MAX_LABEL_ITEMS: usize = 8;
const MAX_DELIVERY_STAGE_RETRIES: usize = 8;
/// Maximum runnable recovery commands rendered in one text receipt.
pub(crate) const MAX_TEXT_NEXT_COMMANDS: usize = 4;
/// The recovery tag is the sole agent-facing signal that `recovery_reason`
/// is mandatory; consumers must not infer that requirement from readiness.
pub(crate) const WORK_UPDATE_CLAIM_ACTION: &str = "work_update:claim";
pub(crate) const WORK_UPDATE_CLAIM_RECOVERY_ACTION: &str =
    "work_update:claim(recovery_reason_required)";

/// Exact structured agent response for one full project-memory read.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub(crate) struct ProjectMemoryFullResponse {
    #[serde(flatten)]
    pub memory: ProjectMemoryFull,
    pub reminders: Vec<String>,
    pub next: Vec<String>,
}

impl ProjectMemoryFullResponse {
    #[must_use]
    pub(crate) fn new(memory: ProjectMemoryFull) -> Self {
        Self {
            memory,
            reminders: Vec::new(),
            next: vec!["engram work memories".into()],
        }
    }

    #[must_use]
    pub(crate) fn terminal_lines(&self) -> Vec<String> {
        vec![
            format!("memory {}:", self.memory.key),
            format!(
                "by {}",
                terminal_safe_actor_label(
                    &self.memory.actor_id,
                    self.memory.actor_context.as_deref()
                )
            ),
            terminal_safe_data_block(&self.memory.body),
        ]
    }
}

pub(crate) fn project_memory_full_response(
    memory: ProjectMemoryFull,
) -> Result<ProjectMemoryFullResponse, StoreError> {
    let response = ProjectMemoryFullResponse::new(memory);
    let bytes = serde_json::to_vec(&response)?.len();
    if bytes > MAX_PROJECT_MEMORY_FULL_BYTES {
        return Err(StoreError::InvalidProjectMemory(format!(
            "serialized full memory response requires {bytes} bytes, exceeding the {MAX_PROJECT_MEMORY_FULL_BYTES}-byte limit"
        )));
    }
    let terminal_bytes = render_agent_receipt_text(
        &response.terminal_lines(),
        &response.reminders,
        &response.next,
    )
    .len();
    if terminal_bytes > MAX_PROJECT_MEMORY_FULL_BYTES {
        return Err(StoreError::InvalidProjectMemory(format!(
            "terminal-safe full memory response requires {terminal_bytes} bytes, exceeding the {MAX_PROJECT_MEMORY_FULL_BYTES}-byte limit"
        )));
    }
    Ok(response)
}

pub(crate) fn render_agent_receipt_text(
    lines: &[String],
    reminders: &[String],
    next: &[String],
) -> String {
    let mut out = lines.join("\n");
    out.push_str("\nreminders:");
    if reminders.is_empty() {
        out.push_str(" none");
    }
    for reminder in reminders {
        out.push_str("\n  - ");
        out.push_str(reminder);
    }
    out.push_str("\nnext:");
    if next.is_empty() {
        out.push_str(" none");
    }
    for command in next.iter().take(MAX_TEXT_NEXT_COMMANDS) {
        out.push_str("\n  ");
        out.push_str(command);
    }
    if next.len() > MAX_TEXT_NEXT_COMMANDS {
        let _ = write!(
            &mut out,
            "\n  (+{} more)",
            next.len() - MAX_TEXT_NEXT_COMMANDS
        );
    }
    out
}

pub(crate) fn terminal_safe_multiline(text: &str) -> String {
    let mut safe = String::with_capacity(text.len());
    for character in text.chars() {
        if character == '\n' || character == '\t' || !is_unsafe_rendered_text_char(character) {
            safe.push(character);
        } else {
            safe.extend(character.escape_default());
        }
    }
    safe
}

fn terminal_safe_data_block(text: &str) -> String {
    terminal_safe_multiline(text)
        .split('\n')
        .map(|line| format!("  | {line}"))
        .collect::<Vec<_>>()
        .join("\n")
}

fn ensure_project_memory_full_is_admissible(memory: &ProjectMemoryFull) -> Result<(), StoreError> {
    project_memory_full_response(memory.clone()).map(drop)
}

/// Locally derived source used when a shell omits its asserted actor id.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum WorkActorDefaultSource {
    /// A conventional OS-user environment variable supplied the value.
    OsUserEnvironment,
    /// No conventional user variable existed, so this process supplied a
    /// synthetic non-empty actor id rather than refusing the shell word.
    ProcessFallback,
}

/// Typed audit origins for shell attribution omitted by the caller.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct WorkAttributionDefaults {
    pub actor: Option<WorkActorDefaultSource>,
    pub session: bool,
}

/// Immutable host context for one CLI or MCP work-service connection.
pub struct LocalWorkService {
    database: PathBuf,
    project_id: ProjectId,
    actor_id: String,
    actor_context: Option<String>,
    actor_context_normalized: bool,
    session_id: SessionId,
    attribution_defaults: WorkAttributionDefaults,
    source_skill: Option<String>,
    cached_store: OnceLock<Mutex<SqliteStore>>,
    #[cfg(test)]
    delivery_stage_hook: Option<DeliveryStageTestHook>,
}

impl Clone for LocalWorkService {
    fn clone(&self) -> Self {
        Self {
            database: self.database.clone(),
            project_id: self.project_id.clone(),
            actor_id: self.actor_id.clone(),
            actor_context: self.actor_context.clone(),
            actor_context_normalized: self.actor_context_normalized,
            session_id: self.session_id.clone(),
            attribution_defaults: self.attribution_defaults,
            source_skill: self.source_skill.clone(),
            // A clone is a separate protocol connection. Keeping its SQLite
            // handle independent preserves the real cross-connection CAS and
            // delivery-race semantics exercised by hosts and tests.
            cached_store: OnceLock::new(),
            #[cfg(test)]
            delivery_stage_hook: self.delivery_stage_hook.clone(),
        }
    }
}

impl fmt::Debug for LocalWorkService {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter
            .debug_struct("LocalWorkService")
            .field("database", &self.database)
            .field("project_id", &self.project_id)
            .field("actor_id", &self.actor_id)
            .field("actor_context_present", &self.actor_context.is_some())
            .field("actor_context_normalized", &self.actor_context_normalized)
            .field("session_id", &self.session_id)
            .field("attribution_defaults", &self.attribution_defaults)
            .field("source_skill", &self.source_skill)
            .field("store_initialized", &self.cached_store.get().is_some())
            .finish_non_exhaustive()
    }
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
    base_key: &'a str,
    now: DateTime<Utc>,
}

struct PreparedCompletionEvidence {
    evidence: Vec<ObjectHash>,
    attempt_key: String,
}

#[derive(Serialize)]
struct WorkProtocolIntent<'a, T> {
    project_id: &'a ProjectId,
    session_id: &'a SessionId,
    actor_id: &'a str,
    source_skill: Option<&'a str>,
    input: &'a T,
}

#[derive(Serialize)]
struct WorkNoteIntent<'a> {
    summary: &'a str,
    refs: &'a [String],
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
struct WorkProtocolBasis {
    focused_work: Option<WorkItem>,
    claim: Option<WorkClaim>,
    handoffs: Vec<WorkHandoffOffer>,
}

impl WorkProtocolBasis {
    fn retry_stable(&self) -> Self {
        let mut stable = self.clone();
        if let Some(claim) = stable.claim.as_mut() {
            // Holder activity slides these projection fields without changing
            // claim identity or authority. The fence remains part of retry
            // identity because it distinguishes claim epochs.
            claim.expires_at = DateTime::<Utc>::UNIX_EPOCH;
            claim.revision = 0;
        }
        stable
    }
}

#[derive(Serialize)]
struct WorkCoreOperationKey<'a> {
    project_id: &'a ProjectId,
    session_id: &'a SessionId,
    protocol_operation: &'a str,
    caller_key: &'a str,
    core_operation: &'a str,
}

#[derive(Serialize)]
struct WorkCompletionAttemptKey<'a> {
    base_key: &'a str,
    /// Dense positions are unique only inside one active store lineage. The
    /// enclosing core-operation key supplies project, session, and protocol
    /// identity, while this run-tagged position supplies the run identity.
    run_feed_cut: &'a FeedPosition,
}

#[derive(Serialize)]
struct WorkCompletionCaptureKey<'a> {
    base_key: &'a str,
    work_id: WorkId,
    work_revision: i64,
    run_id: WorkRunId,
    claim_id: WorkClaimId,
    claim_fence: i64,
}

/// Server-derived idempotency identity for a call that supplied no key: the
/// same session repeating the same operation with the same canonical intent
/// against the same focused work replays instead of duplicating.
#[derive(Serialize)]
struct WorkDerivedKey<'a> {
    project_id: &'a ProjectId,
    session_id: &'a SessionId,
    protocol_operation: &'a str,
    focused_work_id: Option<WorkId>,
    /// Hash of the focused work/claim/handoff basis, excluding sliding claim
    /// expiry and claim revision, so renewal does not defeat exact replay.
    basis: &'a ObjectHash,
    /// Whether the basis claim is still live at call time, so a repeated
    /// call after expiry is a new attempt even though the projection bytes
    /// did not change.
    claim_live: Option<bool>,
    intent: &'a ObjectHash,
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
    pub delivery: WorkChangeProjection,
}

/// Exact agent projection persisted beside an unacknowledged dense feed page.
/// Replays decode these bytes instead of rebuilding against mutable focus or
/// task bindings.
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

/// Full bounded context for the ambient focused item.
#[derive(Clone, Debug, Deserialize, Serialize)]
pub struct WorkFocusView {
    pub session: AgentWorkSession,
    pub status: ReadyWorkSummary,
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
    pub obligation_page: WorkObligationPage,
    #[serde(default)]
    pub memories: Vec<WorkMemoryIndexEntry>,
    pub history: WorkHistoryView,
    /// Direct disposed required children for which the current project-bound
    /// caller can execute `work_update:waive_required_child` now.
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

impl LocalWorkService {
    /// Constructs a project-bound local-work service.
    #[must_use]
    pub fn new(
        database: PathBuf,
        project_id: ProjectId,
        actor_id: String,
        session_id: SessionId,
        source_skill: Option<String>,
    ) -> Self {
        Self::new_with_attribution(
            database,
            project_id,
            actor_id,
            session_id,
            source_skill,
            None,
            WorkAttributionDefaults::default(),
        )
    }

    /// Constructs a project-bound service with optional host-asserted actor
    /// context and explicit local-attribution defaults.
    #[must_use]
    pub fn new_with_attribution(
        database: PathBuf,
        project_id: ProjectId,
        actor_id: String,
        session_id: SessionId,
        source_skill: Option<String>,
        actor_context: Option<String>,
        attribution_defaults: WorkAttributionDefaults,
    ) -> Self {
        let (actor_context, actor_context_normalized) = normalize_actor_context(actor_context);
        Self {
            database,
            project_id,
            actor_id,
            actor_context,
            actor_context_normalized,
            session_id,
            attribution_defaults,
            source_skill,
            cached_store: OnceLock::new(),
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
    pub fn work_next_with_delivery_token(
        &self,
        limit: u32,
        acknowledge_through: Option<i64>,
        acknowledge_token: Option<&str>,
        query: WorkNextQuery,
        now: DateTime<Utc>,
    ) -> Result<WorkNextView, StoreError> {
        self.work_next_internal(
            limit,
            acknowledge_through,
            acknowledge_token,
            query,
            now,
            false,
        )
    }

    /// Builds an agent-rendered view while deferring the memory-signal
    /// acknowledgement until the outer renderer proves that it was delivered.
    pub(crate) fn work_next_for_agent(
        &self,
        limit: u32,
        query: WorkNextQuery,
        now: DateTime<Utc>,
    ) -> Result<WorkNextView, StoreError> {
        self.work_next_internal(limit, None, None, query, now, true)
    }

    #[allow(
        clippy::too_many_lines,
        reason = "section selection, exact delivery staging, and final byte fitting stay together so cursor advancement is auditable"
    )]
    fn work_next_internal(
        &self,
        limit: u32,
        acknowledge_through: Option<i64>,
        acknowledge_token: Option<&str>,
        query: WorkNextQuery,
        now: DateTime<Utc>,
        defer_memory_acknowledgement: bool,
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
        let wants_memories = sections.contains(&WorkNextSection::Memories);
        // Validate and read the advisory memory signal before changing the
        // exact work-change delivery state. An advisory refusal must not make
        // an unseen tentative page look delivered on the caller's next try.
        let memory_advertisement = if wants_memories {
            Some(store.project_memory_advertisement_candidate(
                &self.project_id,
                &self.session_id,
                query.context_generation.as_deref(),
            )?)
        } else {
            None
        };
        let project_feed = FeedId::Project(self.project_id.clone());
        if acknowledge_through.is_none() && wants_changes {
            // The page returned by the previous call counts as delivered once
            // this session asks for the next one; an agent never acknowledges.
            let previous = store.work_session_state(&self.project_id, &self.session_id, now)?;
            if let Some(through) = previous.tentative_project_cursor {
                store.acknowledge_work_session_delivery(
                    &self.project_id,
                    &self.session_id,
                    through,
                    previous.tentative_delivery_token.as_deref(),
                    now,
                )?;
            }
        }
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
                            reason: WorkSectionOmissionReason::Staged,
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
                            reason: WorkSectionOmissionReason::Staged,
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
                .map(|work_id| self.focus_view(&store, work_id, true, now))
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
        let memories = memory_advertisement
            .as_ref()
            .map(|advertisement| ProjectMemorySignal {
                count: advertisement.count,
                changed: advertisement.changed,
            });
        let mut response = WorkNextView {
            session: agent_work_session(&session),
            focus,
            ready,
            catalog,
            changes,
            memories,
            delivered_through: wants_changes.then_some(delivered_through),
            delivery_token: wants_changes
                .then(|| session.tentative_delivery_token.clone())
                .flatten(),
            omissions,
            memory_advertisement: None,
        };
        fit_work_next_response(&mut response)?;
        ensure_agent_response_budget(&response, "work_next")?;
        if response.memories.is_some()
            && let Some(advertisement) = memory_advertisement
            && advertisement.changed
        {
            if defer_memory_acknowledgement {
                response.memory_advertisement = Some(advertisement);
            } else {
                acknowledge_project_memory_advertisement_best_effort(
                    &mut store,
                    &self.project_id,
                    &self.session_id,
                    &advertisement,
                );
            }
        }
        Ok(response)
    }

    /// Acknowledges the exact project-memory advisory candidate retained in an
    /// agent response after its final byte shedding and rendering pass.
    pub(crate) fn acknowledge_work_next_memories(&self, view: &WorkNextView) {
        let Some(advertisement) = &view.memory_advertisement else {
            return;
        };
        let Ok(mut store) = self.store() else {
            return;
        };
        acknowledge_project_memory_advertisement_best_effort(
            &mut store,
            &self.project_id,
            &self.session_id,
            advertisement,
        );
    }

    /// Creates one attributed project memory without changing work focus or
    /// renewing a work claim.
    ///
    /// # Errors
    ///
    /// Returns a typed storage refusal when authorization, normalization,
    /// size, redaction, or create-only lifecycle admission fails.
    pub fn remember_project_memory(
        &self,
        body: String,
        key: Option<String>,
        now: DateTime<Utc>,
    ) -> Result<ProjectMemoryMutationReceipt, StoreError> {
        self.store()?.remember_project_memory_with_admission(
            &RememberProjectMemoryRequest {
                project_id: self.project_id.clone(),
                session_id: self.session_id.clone(),
                key,
                body,
                actor: self.actor("remember", "record attributed project memory"),
                created_at: now,
            },
            &DevelopmentNoopRedactor,
            ensure_project_memory_full_is_admissible,
        )
    }

    /// Lists live project memories without exposing body text.
    ///
    /// # Errors
    ///
    /// Returns a typed storage refusal when authorization, query, cursor, or
    /// stored-projection validation fails.
    pub fn project_memories(
        &self,
        query: Option<&str>,
        after: Option<&str>,
    ) -> Result<ProjectMemoryList, StoreError> {
        self.store()?.project_memories(
            &self.project_id,
            &self.session_id,
            &self.actor("memories", "list attributed project memories"),
            query,
            after,
        )
    }

    /// Reads one live project memory through its dedicated bounded envelope.
    ///
    /// # Errors
    ///
    /// Returns a typed storage refusal when authorization, key resolution,
    /// lifecycle, or stored-envelope validation fails.
    pub(crate) fn project_memory_full(
        &self,
        key: &str,
    ) -> Result<ProjectMemoryFullResponse, StoreError> {
        let full = self.store()?.project_memory_full(
            &self.project_id,
            &self.session_id,
            &self.actor("memories", "read attributed project memory"),
            key,
        )?;
        project_memory_full_response(full).map_err(|error| match error {
            StoreError::InvalidProjectMemory(detail) => StoreError::InvalidMemoryProjection(detail),
            other => other,
        })
    }

    /// Appends an attributed terminal project-memory tombstone.
    ///
    /// # Errors
    ///
    /// Returns a typed storage refusal when authorization, key resolution, or
    /// terminal lifecycle validation fails.
    pub fn forget_project_memory(
        &self,
        key: String,
        now: DateTime<Utc>,
    ) -> Result<ProjectMemoryMutationReceipt, StoreError> {
        self.store()?.forget_project_memory(
            &ForgetProjectMemoryRequest {
                project_id: self.project_id.clone(),
                session_id: self.session_id.clone(),
                key,
                actor: self.actor("forget", "retire attributed project memory"),
                created_at: now,
            },
            &DevelopmentNoopRedactor,
        )
    }

    /// Makes `work_ref` the session's ambient focus without inspecting it, so a
    /// mutation can name its target in the same call.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the reference is absent or outside the project.
    pub fn select_work(&self, work_ref: &str, now: DateTime<Utc>) -> Result<(), StoreError> {
        let mut store = self.store()?;
        self.bind_target(&mut store, Some(work_ref), now)?;
        Ok(())
    }

    /// The work this session holds under a live claim, with expiry, read from
    /// the claim projection without building any focus view.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the store cannot be read.
    pub fn held_work(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Vec<(WorkId, DateTime<Utc>)>, StoreError> {
        let store = self.store()?;
        store.work_held_by(&self.session_id, now)
    }

    /// Every live claim in this project, used only to annotate compact agent
    /// catalog rows without constructing one focus packet per item.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the live-claim projection is invalid.
    pub fn live_work_claims(
        &self,
        now: DateTime<Utc>,
    ) -> Result<Vec<(WorkId, SessionId, DateTime<Utc>)>, StoreError> {
        let store = self.store()?;
        store.live_work_claims(&self.project_id, now)
    }

    /// Inspects work by reference without changing ambient focus or staging
    /// any delivery.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the reference is absent or projections are invalid.
    pub fn inspect_work(
        &self,
        work_ref: &str,
        now: DateTime<Utc>,
    ) -> Result<WorkFocusView, StoreError> {
        let store = self.store()?;
        let work = store.resolve_work_ref(&self.project_id, work_ref)?;
        self.focus_view(&store, work.work_id, false, now)
    }

    /// Resolves one work reference without projecting or changing ambient
    /// focus. Agent translations use this only to attribute core refusals.
    pub(crate) fn resolve_work_reference(&self, work_ref: &str) -> Result<WorkItem, StoreError> {
        self.store()?.resolve_work_ref(&self.project_id, work_ref)
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
        self.focus_view(&store, item.work_id, true, now)
    }

    /// Creates a root or atomically decomposes ambient focused work.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when project binding or lifecycle admission is
    /// invalid, or the underlying transaction refuses the request.
    pub fn work_propose(
        &self,
        input: WorkProposeInput,
        now: DateTime<Utc>,
    ) -> Result<WorkProposeResult, StoreError> {
        self.work_propose_on(None, input, now)
    }

    /// Like [`Self::work_propose`], but first binds `work_ref` as the ambient
    /// focus and the decomposition target inside the same call.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] under the same conditions as [`Self::work_propose`],
    /// or when `work_ref` does not resolve inside the project.
    #[allow(
        clippy::too_many_lines,
        reason = "root and decomposition translations remain together so the six-operation boundary is auditable"
    )]
    pub fn work_propose_on(
        &self,
        work_ref: Option<&str>,
        input: WorkProposeInput,
        now: DateTime<Utc>,
    ) -> Result<WorkProposeResult, StoreError> {
        let mut store = self.store()?;
        let target = self.bind_target(&mut store, work_ref, now)?;
        let basis = self.protocol_basis(
            &store,
            matches!(input, WorkProposeInput::Decompose { .. }),
            false,
            target,
            now,
        )?;
        let intent = self.protocol_intent(&input);
        let (protocol_operation, core_operation, raw_key) = propose_metadata(&input);
        let raw_key =
            self.effective_idempotency_key(raw_key, protocol_operation, &basis, &intent, now)?;
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
        let basis_matches =
            retry_stable_basis_matches(attempt.basis_matches, attempt.basis.as_ref(), &basis)?;
        let scoped_key = self.core_operation_key(protocol_operation, &raw_key, core_operation)?;
        let core_result = store.work_operation_result_value(core_operation, &scoped_key)?;
        ensure_protocol_basis(
            basis_matches,
            protocol_operation,
            &raw_key,
            core_result.is_some(),
        )?;
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
                    let focus = self.focus_view(&store, work.work_id, true, now)?;
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
                        actor: self.actor("work_propose", "create local root work"),
                        idempotency_key: scoped_key,
                        created_at: now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                store.focus_work_session(&self.project_id, &self.session_id, work.work_id, now)?;
                let focus = self.focus_view(&store, work.work_id, true, now)?;
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
                    let authority = self.planning_authority(basis.claim.as_ref(), &parent, now);
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
    pub fn work_update(
        &self,
        input: WorkUpdateInput,
        now: DateTime<Utc>,
    ) -> Result<WorkUpdateResult, StoreError> {
        self.work_update_on(None, input, now)
    }

    /// Records one typed gate transition through the evidence path. Storage
    /// serializes the latest same-name observation with the append, so an
    /// exact consecutive retry is atomic across sessions and processes.
    #[cfg(test)]
    pub(crate) fn work_gate(
        &self,
        name: &str,
        failed: &[String],
        evidence_ref: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<WorkUpdateResult, StoreError> {
        self.work_gate_on(None, name, failed, evidence_ref, now)
    }

    /// Like [`Self::work_gate`], but binds an explicit target through the same
    /// storage operation so concurrent focus changes cannot redirect evidence.
    pub(crate) fn work_gate_on(
        &self,
        work_ref: Option<&str>,
        name: &str,
        failed: &[String],
        evidence_ref: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<WorkUpdateResult, StoreError> {
        let mut store = self.store()?;
        let target = self.bind_target(&mut store, work_ref, now)?;
        let basis = self.protocol_basis(&store, true, false, target, now)?;
        let work = basis.focused_work.clone().ok_or_else(|| {
            StoreError::InvalidWorkProjection("gate attempt has no bound focused work".into())
        })?;
        let claim = basis
            .claim
            .clone()
            .ok_or(StoreError::WorkClaimMismatch { work: work.work_id })?;
        if claim.work_id != work.work_id
            || claim.state != WorkClaimState::Active
            || claim.holder != self.session_id
        {
            return Err(StoreError::WorkClaimMismatch { work: work.work_id });
        }
        if work.lifecycle != WorkLifecycle::Open {
            return Err(StoreError::WorkNotOpen(work.work_id));
        }
        let attempt = store.record_gate_evidence_protocol(
            &RecordGateEvidenceRequest {
                work_id: work.work_id,
                run_id: claim.run_id,
                expected_work_revision: work.revision,
                holder: self.session_id.clone(),
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                name: name.to_owned(),
                failed: failed.to_owned(),
                evidence_ref: evidence_ref.map(str::to_owned),
                actor: self.actor("work_update", "record gate evidence for ambient work"),
                recorded_at: now,
            },
            &BeginGateWorkProtocolAttempt {
                project_id: &self.project_id,
                session_id: &self.session_id,
                basis: &basis,
                now,
            },
            &DevelopmentNoopRedactor,
        )?;
        let protocol_operation = "work_update:gate";
        // Gate does not use `basis_matches`: its protocol key already binds
        // work, run, claim id/fence, normalized observation, and the canonical
        // previous transition. A new append revalidates the live claim inside
        // the same storage transaction.
        if let Some(result) = attempt.result {
            return serde_json::from_value(result).map_err(StoreError::from);
        }
        let result = self.work_update_result(
            &store,
            "evidence",
            work.work_id,
            serde_json::to_value(&attempt.evidence)?,
            now,
        )?;
        store.finish_work_protocol_attempt(
            &self.project_id,
            &self.session_id,
            protocol_operation,
            &attempt.idempotency_key,
            &result,
        )?;
        Ok(result)
    }

    /// Captures one note as evidence plus its acknowledging checkpoint under
    /// one explicit work target and one atomic storage operation.
    pub(crate) fn work_note_on(
        &self,
        work_ref: Option<&str>,
        summary: &str,
        refs: &[String],
        now: DateTime<Utc>,
    ) -> Result<WorkNoteResult, StoreError> {
        let mut store = self.store()?;
        let target = self.bind_target(&mut store, work_ref, now)?;
        let basis = self.protocol_basis(&store, true, false, target, now)?;
        let note = WorkNoteIntent { summary, refs };
        let intent = self.protocol_intent(&note);
        let protocol_operation = "work_update:note";
        let raw_key =
            self.effective_idempotency_key("", protocol_operation, &basis, &intent, now)?;
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
            return serde_json::from_value(result).map_err(StoreError::from);
        }
        let basis_matches =
            retry_stable_basis_matches(attempt.basis_matches, attempt.basis.as_ref(), &basis)?;
        let scoped_key =
            self.core_operation_key(protocol_operation, &raw_key, "record_work_note")?;
        if let Some(value) = store.work_operation_result_value("record_work_note", &scoped_key)? {
            let capture: WorkNoteCapture = serde_json::from_value(value)?;
            let durable_basis: WorkProtocolBasis =
                serde_json::from_value(attempt.basis.ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "committed note has no durable attempt basis".into(),
                    )
                })?)?;
            let work_id = durable_basis
                .focused_work
                .map(|work| work.work_id)
                .ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "committed note basis has no focused work".into(),
                    )
                })?;
            let result = self.work_note_result(&store, work_id, capture, now)?;
            store.finish_work_protocol_attempt(
                &self.project_id,
                &self.session_id,
                protocol_operation,
                &raw_key,
                &result,
            )?;
            return Ok(result);
        }
        ensure_protocol_basis(basis_matches, protocol_operation, &raw_key, false)?;
        let work = basis.focused_work.clone().ok_or_else(|| {
            StoreError::InvalidWorkProjection("note attempt has no bound focused work".into())
        })?;
        let claim = self.live_protocol_claim(&basis, &work, now)?;
        let capture = store.record_work_note(
            &RecordWorkNoteRequest {
                work_id: work.work_id,
                run_id: claim.run_id,
                expected_work_revision: work.revision,
                holder: self.session_id.clone(),
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                summary: summary.to_owned(),
                refs: refs.to_owned(),
                actor: self.actor(
                    "work_update",
                    "record note evidence and checkpoint ambient local work",
                ),
                idempotency_key: scoped_key,
                recorded_at: now,
            },
            &DevelopmentNoopRedactor,
        )?;
        let result = self.work_note_result(&store, work.work_id, capture, now)?;
        store.finish_work_protocol_attempt(
            &self.project_id,
            &self.session_id,
            protocol_operation,
            &raw_key,
            &result,
        )?;
        Ok(result)
    }

    fn work_note_result(
        &self,
        store: &SqliteStore,
        work_id: WorkId,
        capture: WorkNoteCapture,
        now: DateTime<Utc>,
    ) -> Result<WorkNoteResult, StoreError> {
        let guidance = self.work_guidance(store, work_id, now)?;
        let evidence = compact_mutation_receipt(
            &guidance.status.work,
            None,
            serde_json::to_value(capture.evidence)?,
        );
        let result = WorkNoteResult {
            operation: "note".into(),
            receipt: compact_mutation_receipt(
                &guidance.status.work,
                None,
                serde_json::to_value(capture.checkpoint)?,
            ),
            obligations: compact_obligations(&guidance.status),
            obligation_page: work_obligation_page(store, work_id)?,
            allowed_next: guidance.allowed_next,
            evidence,
        };
        ensure_agent_response_budget(&result, "work_update")?;
        Ok(result)
    }

    /// Like [`Self::work_update`], but first binds `work_ref` as the ambient
    /// focus and the mutation target inside the same call.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] under the same conditions as [`Self::work_update`],
    /// or when `work_ref` does not resolve inside the project.
    #[allow(
        clippy::too_many_lines,
        reason = "the tagged update union is translated in one exhaustive match so new variants cannot bypass ambient fence inference"
    )]
    pub fn work_update_on(
        &self,
        work_ref: Option<&str>,
        input: WorkUpdateInput,
        now: DateTime<Utc>,
    ) -> Result<WorkUpdateResult, StoreError> {
        let mut store = self.store()?;
        let target = self.bind_target(&mut store, work_ref, now)?;
        let basis = self.protocol_basis(&store, true, false, target, now)?;
        let intent = self.protocol_intent(&input);
        let (operation, core_operation, raw_key) = update_metadata(&input);
        let protocol_operation = format!("work_update:{operation}");
        let raw_key =
            self.effective_idempotency_key(raw_key, &protocol_operation, &basis, &intent, now)?;
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
        let basis_matches =
            retry_stable_basis_matches(attempt.basis_matches, attempt.basis.as_ref(), &basis)?;
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
        ensure_protocol_basis(basis_matches, &protocol_operation, &raw_key, false)?;
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
                        ttl_seconds: ttl_seconds.unwrap_or(DEFAULT_WORK_CLAIM_TTL_SECONDS),
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
                        evidence: evidence.as_deref().map(parse_hashes).transpose()?,
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
                let authority = self.planning_authority(basis.claim.as_ref(), &work, now);
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
                let authority = self.planning_authority(basis.claim.as_ref(), &work, now);
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
                let authority = self.planning_authority(basis.claim.as_ref(), &work, now);
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
                let authority = self.planning_authority(basis.claim.as_ref(), &work, now);
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
                let authority = self.planning_authority(basis.claim.as_ref(), &work, now);
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
                        actor: self.actor(
                            "work_update",
                            "waive a disposed required child from the completion barrier",
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
            obligation_page: work_obligation_page(store, work_id)?,
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
        self.work_complete_on(None, input, now)
    }

    /// Like [`Self::work_complete`], but first binds `work_ref` as the ambient
    /// focus and the completion target inside the same call.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] under the same conditions as
    /// [`Self::work_complete`], or when `work_ref` does not resolve inside the
    /// project.
    #[allow(
        clippy::too_many_lines,
        reason = "capture, evidence closure, acceptance, checkpoint, and seal stay in one auditable completion path"
    )]
    pub fn work_complete_on(
        &self,
        work_ref: Option<&str>,
        input: WorkCompleteInput,
        now: DateTime<Utc>,
    ) -> Result<WorkCompleteResult, StoreError> {
        let mut store = self.store()?;
        let target = self.bind_target(&mut store, work_ref, now)?;
        let basis = self.protocol_basis(&store, true, false, target, now)?;
        let intent = self.protocol_intent(&input);
        let raw_key = self.effective_idempotency_key(
            &input.idempotency_key,
            "work_complete",
            &basis,
            &intent,
            now,
        )?;
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
            let result: WorkCompleteResult = serde_json::from_value(result)?;
            match &result {
                WorkCompleteResult::Completed(receipt) => {
                    ensure_completion_replay_target(&basis, receipt.work_id, &raw_key)?;
                    return Ok(result);
                }
                WorkCompleteResult::Refused(_) => {
                    return Err(StoreError::InvalidWorkProjection(
                        "stored work_complete refusal belongs to an incompatible prerelease build"
                            .into(),
                    ));
                }
            }
        }
        let stored_basis = attempt
            .basis
            .clone()
            .map(serde_json::from_value::<WorkProtocolBasis>)
            .transpose()?;
        if let Some(stored_basis) = stored_basis.as_ref() {
            let stored_work = stored_basis.focused_work.as_ref().ok_or_else(|| {
                StoreError::InvalidWorkProjection(
                    "pending completion attempt has no bound focused work".into(),
                )
            })?;
            ensure_completion_replay_target(&basis, stored_work.work_id, &raw_key)?;
            let stored_run_id = if let Some(claim) = stored_basis.claim.as_ref() {
                if claim.work_id != stored_work.work_id {
                    return Err(StoreError::InvalidWorkProjection(
                        "pending completion claim crosses its focused work binding".into(),
                    ));
                }
                Some(claim.run_id)
            } else {
                stored_work.active_run_id
            };
            if let Some(run_id) = stored_run_id {
                let run = store.get_work_run(run_id)?;
                if run.work_id != stored_work.work_id {
                    return Err(StoreError::InvalidWorkProjection(
                        "pending completion run crosses its focused work binding".into(),
                    ));
                }
                if let Some(seal_hash) = run.completion_seal {
                    let seal: CompletionSeal = store.get(&seal_hash)?.ok_or_else(|| {
                        StoreError::InvalidWorkProjection(
                            "completed pending run has no canonical completion seal".into(),
                        )
                    })?;
                    if seal.work_id != stored_work.work_id || seal.run_id != run_id {
                        return Err(StoreError::InvalidWorkProjection(
                            "pending completion seal crosses its original work or run binding"
                                .into(),
                        ));
                    }
                    let result = completion_result(&store, &seal)?;
                    store.finish_work_protocol_attempt(
                        &self.project_id,
                        &self.session_id,
                        "work_complete",
                        &raw_key,
                        &result,
                    )?;
                    return Ok(result);
                }
            }
        }
        let mut basis_matches =
            retry_stable_basis_matches(attempt.basis_matches, attempt.basis.as_ref(), &basis)?;
        if !basis_matches
            && stored_basis.as_ref().is_some_and(|stored| {
                completion_basis_refresh_is_safe(stored, &basis, &self.session_id)
            })
        {
            let expected_basis = attempt.basis.as_ref().ok_or_else(|| {
                StoreError::InvalidWorkProjection(
                    "pending completion basis refresh has no durable source basis".into(),
                )
            })?;
            store.refresh_pending_work_protocol_attempt_basis(
                &self.project_id,
                &self.session_id,
                "work_complete",
                &raw_key,
                expected_basis,
                &basis,
            )?;
            basis_matches = true;
        }
        // A fresh attempt against work that was already sealed has no claim in
        // its basis. Only use the latest run while that exact completed basis
        // still matches; interrupted core completion above is bound to its
        // original claimed run instead.
        if basis_matches
            && let Some(work) = basis.focused_work.as_ref()
            && work.lifecycle == WorkLifecycle::Completed
            && let Some(run) = store.latest_work_run(work.work_id)?
            && let Some(seal_hash) = run.completion_seal
        {
            let seal: CompletionSeal = store.get(&seal_hash)?.ok_or_else(|| {
                StoreError::InvalidWorkProjection(
                    "completed work has no canonical completion seal".into(),
                )
            })?;
            let result = completion_result(&store, &seal)?;
            store.finish_work_protocol_attempt(
                &self.project_id,
                &self.session_id,
                "work_complete",
                &raw_key,
                &result,
            )?;
            return Ok(result);
        }
        ensure_protocol_basis(basis_matches, "work_complete", &raw_key, false)?;
        let WorkCompleteInput {
            capture,
            evidence: supplied_evidence,
            acceptance: supplied_acceptance,
            note,
            idempotency_key: _,
        } = input;
        let work = basis.focused_work.clone().ok_or_else(|| {
            StoreError::InvalidWorkProjection("completion attempt has no bound focused work".into())
        })?;
        let actor = self.actor("work_complete", "complete ambient local work");
        let claim = self.live_protocol_claim(&basis, &work, now)?;
        let evidence_basis = Self::completion_evidence_basis(&store, &claim, &supplied_evidence)?;
        let acceptance = match Self::prevalidate_completion_acceptance(
            &work,
            supplied_acceptance.as_deref(),
            note.as_deref(),
            &evidence_basis,
            actor.assurance,
            &actor.actor_id,
        ) {
            Ok(acceptance) => acceptance,
            Err(StoreError::WorkCompletionRecoveryRequired { cause, .. }) => {
                let snapshot = store.work_completion_recovery(&work, &claim, now, &cause)?;
                let obligation_page = work_completion_recovery_page(&snapshot)?;
                let result =
                    completion_recovery_result(work.work_id, snapshot.recovery, obligation_page);
                return Ok(result);
            }
            Err(error) => return Err(error),
        };
        let prepared = self.prepare_completion_evidence(
            &mut store,
            CompletionEvidencePlan {
                work: &work,
                claim: &claim,
                capture: capture.as_ref(),
                evidence: evidence_basis,
                base_key: &raw_key,
                now,
            },
        )?;
        let scoped_key =
            self.core_operation_key("work_complete", &prepared.attempt_key, "complete_work")?;
        let evidence = prepared.evidence;
        let acceptance = bind_completion_acceptance_evidence(acceptance, &evidence);
        let completion = store.complete_work_for_protocol(
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
                },
                actor,
                idempotency_key: scoped_key,
                completed_at: now,
            },
            &DevelopmentNoopRedactor,
        );
        let result = match completion? {
            CompleteWorkStorageResult::Completed(seal) => completion_result(&store, &seal)?,
            CompleteWorkStorageResult::Recovery(snapshot) => {
                let obligation_page = work_completion_recovery_page(&snapshot)?;
                let result =
                    completion_recovery_result(work.work_id, snapshot.recovery, obligation_page);
                return Ok(result);
            }
        };
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
    ) -> Result<PreparedCompletionEvidence, StoreError> {
        let CompletionEvidencePlan {
            work,
            claim,
            capture,
            mut evidence,
            base_key,
            now,
        } = plan;
        if let Some(capture) = capture {
            let capture_key = completion_capture_key(base_key, work, claim)?;
            let evidence_key =
                self.core_operation_key("work_complete", &capture_key, "record_work_evidence")?;
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
        }
        evidence.sort();
        evidence.dedup();
        let run_feed_cut = if let Some(capture) = capture {
            let (_, cut) = store.checkpoint_work_for_completion(
                &CheckpointWorkRequest {
                    work_id: work.work_id,
                    run_id: claim.run_id,
                    expected_work_revision: work.revision,
                    holder: self.session_id.clone(),
                    claim_id: claim.claim_id,
                    claim_fence: claim.fence,
                    summary: capture.summary.clone(),
                    evidence: Some(evidence.clone()),
                    actor: self.actor(
                        "work_complete",
                        "checkpoint the exact completion evidence cut",
                    ),
                    idempotency_key: base_key.to_owned(),
                    checkpointed_at: now,
                },
                |cut| {
                    let attempt_key = completion_attempt_key(base_key, cut)?;
                    self.core_operation_key("work_complete", &attempt_key, "checkpoint_work")
                },
                &DevelopmentNoopRedactor,
            )?;
            cut
        } else {
            FeedPosition {
                feed: FeedId::RunExecution(claim.run_id),
                position: store.work_feed_head(&FeedId::RunExecution(claim.run_id))?,
            }
        };
        let attempt_key = completion_attempt_key(base_key, &run_feed_cut)?;
        Ok(PreparedCompletionEvidence {
            evidence,
            attempt_key,
        })
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
        supplied: Option<&[WorkAcceptanceInput]>,
        note: Option<&str>,
        evidence_basis: &[ObjectHash],
        assurance: AssuranceLevel,
        actor_id: &str,
    ) -> Result<Vec<AcceptanceResult>, StoreError> {
        let translated = if let Some(supplied) = supplied {
            if note.is_some() {
                return Err(StoreError::InvalidWork(
                    "completion note may be supplied only when acceptance is omitted".into(),
                ));
            }
            supplied
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
                .collect::<Result<Vec<_>, StoreError>>()?
        } else {
            let note = note
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map_or_else(
                    || format!("accepted by {actor_id} via work done"),
                    str::to_owned,
                );
            work.acceptance
                .iter()
                .map(|criterion| AcceptanceResult {
                    criterion: criterion.clone(),
                    satisfied: true,
                    evidence: Vec::new(),
                    assurance,
                    note: note.clone(),
                })
                .collect()
        };
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
        self.work_handoff_on(None, input, now)
    }

    /// Like [`Self::work_handoff`], but first binds `work_ref` as the ambient
    /// focus and the handoff target inside the same call.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] under the same conditions as
    /// [`Self::work_handoff`], or when `work_ref` does not resolve inside the
    /// project.
    pub fn work_handoff_on(
        &self,
        work_ref: Option<&str>,
        input: WorkHandoffInput,
        now: DateTime<Utc>,
    ) -> Result<WorkHandoffResult, StoreError> {
        let mut store = self.store()?;
        let target = self.bind_target(&mut store, work_ref, now)?;
        let basis = self.protocol_basis(&store, true, true, target, now)?;
        let intent = self.protocol_intent(&input);
        let (operation, core_operation, raw_key) = handoff_metadata(&input);
        let protocol_operation = format!("work_handoff:{operation}");
        let raw_key =
            self.effective_idempotency_key(raw_key, &protocol_operation, &basis, &intent, now)?;
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
        let basis_matches =
            retry_stable_basis_matches(attempt.basis_matches, attempt.basis.as_ref(), &basis)?;
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
        ensure_protocol_basis(basis_matches, &protocol_operation, &raw_key, false)?;
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
                        ttl_seconds: ttl_seconds.unwrap_or(DEFAULT_WORK_CLAIM_TTL_SECONDS),
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
                let claim = self.live_protocol_claim(basis, work, now)?;
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

    fn store(&self) -> Result<MutexGuard<'_, SqliteStore>, StoreError> {
        if self.actor_id.trim().is_empty() || self.session_id.0.trim().is_empty() {
            return Err(StoreError::InvalidWork(
                "local work requires a non-empty asserted actor and session binding".into(),
            ));
        }
        if self.cached_store.get().is_none() {
            let opened = SqliteStore::open_unresolved(&self.database)?;
            // A simultaneous first call may win initialization. Dropping this
            // redundant opener is safe; both opened the same canonical store.
            let _ = self.cached_store.set(Mutex::new(opened));
        }
        let cached = self.cached_store.get().ok_or_else(|| {
            StoreError::InvalidWorkProjection(
                "local work service could not initialize its SQLite connection".into(),
            )
        })?;
        cached.lock().map_err(|_| {
            StoreError::InvalidWorkProjection(
                "local work service SQLite connection lock is poisoned".into(),
            )
        })
    }

    fn protocol_intent<'a, T>(&'a self, input: &'a T) -> WorkProtocolIntent<'a, T> {
        WorkProtocolIntent {
            project_id: &self.project_id,
            session_id: &self.session_id,
            actor_id: &self.actor_id,
            source_skill: self.source_skill.as_deref(),
            input,
        }
    }

    fn protocol_basis(
        &self,
        store: &SqliteStore,
        bind_focus: bool,
        include_handoffs: bool,
        target: Option<WorkId>,
        now: DateTime<Utc>,
    ) -> Result<WorkProtocolBasis, StoreError> {
        if !bind_focus {
            return Ok(WorkProtocolBasis {
                focused_work: None,
                claim: None,
                handoffs: Vec::new(),
            });
        }
        let work = self.focused_item(store, target, now)?;
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

    /// Uses the caller's key when one was supplied; otherwise derives one from
    /// the session, operation, focused work, and canonical intent, so an
    /// identical call replays and a different call is a new attempt.
    fn effective_idempotency_key<T: Serialize>(
        &self,
        caller_key: &str,
        protocol_operation: &str,
        basis: &WorkProtocolBasis,
        intent: &WorkProtocolIntent<'_, T>,
        now: DateTime<Utc>,
    ) -> Result<String, StoreError> {
        let caller_key = caller_key.trim();
        if !caller_key.is_empty() {
            return Ok(caller_key.to_owned());
        }
        let intent = CanonicalObject::freeze(intent)?;
        let basis_object = CanonicalObject::freeze(&basis.retry_stable())?;
        let object = CanonicalObject::freeze(&WorkDerivedKey {
            project_id: &self.project_id,
            session_id: &self.session_id,
            protocol_operation,
            focused_work_id: basis.focused_work.as_ref().map(|work| work.work_id),
            basis: basis_object.hash(),
            claim_live: basis
                .claim
                .as_ref()
                .map(|claim| claim.state == WorkClaimState::Active && claim.expires_at > now),
            intent: intent.hash(),
        })?;
        Ok(format!("auto:{}", object.hash().as_str()))
    }

    /// Resolves an optional caller-supplied target, makes it the ambient
    /// focus on this connection, and returns its id so the mutation binds to
    /// it regardless of any concurrent focus change by the same session.
    fn bind_target(
        &self,
        store: &mut SqliteStore,
        work_ref: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<Option<WorkId>, StoreError> {
        let Some(work_ref) = work_ref else {
            return Ok(None);
        };
        let work = store.resolve_work_ref(&self.project_id, work_ref)?;
        store.focus_work_session(&self.project_id, &self.session_id, work.work_id, now)?;
        Ok(Some(work.work_id))
    }

    fn actor(&self, tool_name: &str, reason: &str) -> ActorContext {
        let mut provenance_chain = vec![ProvenanceLink {
            relation: ProvenanceRelation::AssertedBy,
            source: self.actor_id.clone(),
            reference: Some(self.session_id.0.clone()),
        }];
        if let Some(source) = self.attribution_defaults.actor {
            provenance_chain.push(ProvenanceLink {
                relation: ProvenanceRelation::DerivedFrom,
                source: match source {
                    WorkActorDefaultSource::OsUserEnvironment => "defaulted:os_user_environment",
                    WorkActorDefaultSource::ProcessFallback => "defaulted:process_actor",
                }
                .into(),
                reference: Some("actor_id".into()),
            });
        }
        if self.attribution_defaults.session {
            provenance_chain.push(ProvenanceLink {
                relation: ProvenanceRelation::DerivedFrom,
                source: "defaulted:process_session".into(),
                reference: Some("session_id".into()),
            });
        }
        if let Some(actor_context) = &self.actor_context {
            provenance_chain.push(ProvenanceLink {
                relation: ProvenanceRelation::DerivedFrom,
                source: actor_context.clone(),
                reference: Some(ACTOR_CONTEXT_PROVENANCE_REFERENCE.into()),
            });
        }
        if self.actor_context_normalized {
            provenance_chain.push(ProvenanceLink {
                relation: ProvenanceRelation::DerivedFrom,
                source: "actor_context:normalized".into(),
                reference: Some(ACTOR_CONTEXT_NORMALIZED_REFERENCE.into()),
            });
        }
        ActorContext {
            actor_id: self.actor_id.clone(),
            actor_kind: "agent".into(),
            assurance: AssuranceLevel::Asserted,
            run_id: None,
            session_id: Some(self.session_id.clone()),
            source_tool: Some(tool_name.into()),
            source_skill: self.source_skill.clone(),
            provenance_chain,
            reason: reason.into(),
        }
    }

    fn focused_item(
        &self,
        store: &SqliteStore,
        target: Option<WorkId>,
        now: DateTime<Utc>,
    ) -> Result<WorkItem, StoreError> {
        let focused = match target {
            Some(work_id) => Some(work_id),
            None => {
                store
                    .work_session_state(&self.project_id, &self.session_id, now)?
                    .focused_work_id
            }
        };
        focused
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
        if claim.work_id == work.work_id
            && claim.state == WorkClaimState::Active
            && claim.holder == self.session_id
            && claim.expires_at <= now
        {
            return Err(StoreError::WorkClaimLapsed {
                work: work.work_id,
                expired_at: claim.expires_at,
            });
        }
        if claim.work_id != work.work_id
            || claim.state != WorkClaimState::Active
            || claim.holder != self.session_id
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
    ) -> WorkPlanningAuthority {
        if let Some(claim) = claim
            && claim.work_id == work.work_id
            && claim.state == WorkClaimState::Active
            && claim.holder == self.session_id
            && claim.expires_at > now
        {
            return WorkPlanningAuthority::Claim {
                run_id: claim.run_id,
                holder: claim.holder.clone(),
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
            };
        }
        WorkPlanningAuthority::Project
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the bounded focus packet is assembled in one place so every relation and omission limit is visible"
    )]
    fn focus_view(
        &self,
        store: &SqliteStore,
        work_id: WorkId,
        with_memories: bool,
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
        let obligation_records = run
            .as_ref()
            .map(|run| store.work_run_obligations(run.run_id))
            .transpose()?
            .unwrap_or_default();
        let obligation_page = work_obligation_page_from_records(obligation_records)?;
        let required_environments = obligation_page
            .items
            .iter()
            .filter(|obligation| obligation.state == WorkObligationState::Open)
            .filter_map(|obligation| obligation.requirement.required_environment.clone())
            .collect::<Vec<_>>();
        let evidence_total = run
            .as_ref()
            .map(|run| store.work_run_evidence_count(run.run_id))
            .transpose()?
            .unwrap_or_default();
        let evidence_candidates = run
            .as_ref()
            .map(|run| {
                store.work_run_evidence_projection(
                    run.run_id,
                    &required_environments,
                    MAX_FOCUS_RELATIONS,
                )
            })
            .transpose()?
            .unwrap_or_default();
        let evidence = prioritized_focus_evidence(evidence_candidates, &obligation_page);
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
        let mut children = store.work_children(work_id)?;
        // Put unfinished children first inside the bounded relation prefix so
        // terminal history cannot hide work that still needs attention.
        // Stable sorting retains the store's stable id order within each
        // lifecycle group.
        children.sort_by_key(|child| child_lifecycle_priority(child.lifecycle));
        let child_count = children.len();
        let unfinished_child_count = children
            .iter()
            .take_while(|child| child_lifecycle_is_unfinished(child.lifecycle))
            .count();
        let visible_child_count = child_count.min(MAX_FOCUS_RELATIONS);
        let visible_unfinished_child_count = unfinished_child_count.min(visible_child_count);
        let terminal_child_count = child_count - unfinished_child_count;
        let visible_terminal_child_count = visible_child_count - visible_unfinished_child_count;
        let prerequisite_page =
            store.work_prerequisites_with_state(work_id, MAX_FOCUS_RELATIONS)?;
        // The work-memory index is bound to the session's persisted focus; an
        // inspection of another item carries no memory index.
        let memories = if with_memories {
            store.search_work_memories(
                &self.project_id,
                work_id,
                &self.session_id,
                &self.actor_id,
                None,
                Some(MAX_FOCUS_MEMORIES + 1),
            )?
        } else {
            Vec::new()
        };
        let mut omissions = Vec::new();
        let blockers = status.blockers.clone();
        if unfinished_child_count > visible_unfinished_child_count {
            omissions.push(WorkSectionOmission {
                section: WorkNextSection::Focus,
                reason: WorkSectionOmissionReason::UnfinishedChildCountLimit,
                omitted_count: unfinished_child_count - visible_unfinished_child_count,
            });
        }
        if terminal_child_count > visible_terminal_child_count {
            omissions.push(WorkSectionOmission {
                section: WorkNextSection::Focus,
                reason: WorkSectionOmissionReason::TerminalChildCountLimit,
                omitted_count: terminal_child_count - visible_terminal_child_count,
            });
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
        if memories.len() > usize::try_from(MAX_FOCUS_MEMORIES).unwrap_or(usize::MAX) {
            omissions.push(count_omission(
                WorkNextSection::Focus,
                memories.len() - usize::try_from(MAX_FOCUS_MEMORIES).unwrap_or(usize::MAX),
            ));
        }
        let control_binding = run.as_ref().and_then(|run| {
            owned_control_work_binding(&status.work, run, claim.as_ref(), &self.session_id, now)
        });
        let outcome = status.work.outcome.clone();
        let (prerequisites, prerequisite_omissions) = bounded_prerequisite_summaries(
            prerequisite_page.items,
            prerequisite_page.omitted_by_state,
        );
        omissions.extend(prerequisite_omissions);
        let mut view = WorkFocusView {
            session: agent_work_session(&session),
            status: ready_work_summary(status),
            outcome,
            run: run.as_ref().map(work_run_summary),
            claim,
            control_binding,
            children: children
                .into_iter()
                .take(visible_child_count)
                .map(|work| work_item_summary(&work))
                .collect(),
            child_count,
            prerequisites,
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
            obligation_page,
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
        let claim = store.current_work_claim_for_item(&status.work)?;
        let handoffs = store.work_handoff_offers(work_id)?;
        let waivable_required_children = store
            .waivable_required_children(&status.work, MAX_FOCUS_RELATIONS)?
            .into_iter()
            .map(required_child_waiver_candidate)
            .collect::<Vec<_>>();
        let (completion_capture_ready, completion_preflight_ready) = store
            .work_completion_readiness_for_item(
                &status.work,
                claim.as_ref(),
                &self.session_id,
                now,
            )?;
        let claim_recovery_required = store.work_claim_recovery_required_for_item(
            &status.work,
            claim.as_ref(),
            &self.session_id,
        )?;
        let next = allowed_next(
            &status,
            AllowedNextContext {
                claim: claim.as_ref(),
                handoffs: &handoffs,
                session: &self.session_id,
                now,
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

fn acknowledge_project_memory_advertisement_best_effort(
    store: &mut SqliteStore,
    project_id: &ProjectId,
    session_id: &SessionId,
    advertisement: &ProjectMemoryAdvertisement,
) {
    // Delivery is advisory. Any failure leaves the candidate unacknowledged,
    // so it safely reannounces without turning a staged work page into an
    // error that the next call would mistake for a delivered page.
    ignore_project_memory_advertisement_acknowledgement(|| {
        store.acknowledge_project_memory_advertisement(project_id, session_id, advertisement)
    });
}

fn ignore_project_memory_advertisement_acknowledgement(
    acknowledge: impl FnOnce() -> Result<(), StoreError>,
) {
    let _ = acknowledge();
}

fn selected_work_next_sections(requested: &[WorkNextSection]) -> Vec<WorkNextSection> {
    let mut sections = if requested.is_empty() {
        vec![
            WorkNextSection::Focus,
            WorkNextSection::Ready,
            WorkNextSection::Catalog,
            WorkNextSection::Changes,
            WorkNextSection::Memories,
        ]
    } else {
        requested.to_vec()
    };
    sections.sort_by_key(|section| match section {
        WorkNextSection::Focus => 0,
        WorkNextSection::Ready => 1,
        WorkNextSection::Catalog => 2,
        WorkNextSection::Changes => 3,
        WorkNextSection::Memories => 4,
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

fn projected_actor_context(actor: &ActorContext) -> Option<String> {
    actor.attribution_context().map(str::to_owned)
}

pub(crate) fn actor_label(actor_id: &str, actor_context: Option<&str>) -> String {
    actor_context.map_or_else(
        || actor_id.to_owned(),
        |actor_context| format!("{actor_id} ({actor_context})"),
    )
}

pub(crate) fn terminal_safe_actor_label(actor_id: &str, actor_context: Option<&str>) -> String {
    let label = actor_label(actor_id, actor_context);
    let mut safe = String::with_capacity(label.len());
    for character in label.chars() {
        if is_unsafe_rendered_text_char(character) {
            safe.extend(character.escape_default());
        } else {
            safe.push(character);
        }
    }
    safe
}

fn normalize_actor_context(actor_context: Option<String>) -> (Option<String>, bool) {
    let Some(original) = actor_context else {
        return (None, false);
    };
    let mut stripped = String::with_capacity(original.len());
    let mut stripped_unsafe_run = false;
    for character in original.chars() {
        if is_unsafe_rendered_text_char(character) {
            stripped_unsafe_run = true;
            continue;
        }
        if stripped_unsafe_run {
            let previous_is_space = stripped
                .chars()
                .next_back()
                .is_some_and(char::is_whitespace);
            if !stripped.is_empty() && !previous_is_space && !character.is_whitespace() {
                stripped.push(' ');
            }
            stripped_unsafe_run = false;
        }
        stripped.push(character);
    }
    let trimmed = stripped.trim();
    let mut end = trimmed.len().min(MAX_ACTOR_CONTEXT_BYTES);
    while !trimmed.is_char_boundary(end) {
        end = end.saturating_sub(1);
    }
    let normalized = trimmed[..end].trim_end();
    let changed = normalized != original;
    (
        (!normalized.is_empty()).then(|| normalized.to_owned()),
        changed,
    )
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
        child_requirement: work.child_requirement,
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
        prerequisite_state: None,
        updated_at: work.updated_at,
    }
}

const fn child_lifecycle_is_unfinished(lifecycle: WorkLifecycle) -> bool {
    match lifecycle {
        WorkLifecycle::Open | WorkLifecycle::Proposed => true,
        WorkLifecycle::Completed | WorkLifecycle::Cancelled | WorkLifecycle::Superseded => false,
    }
}

const fn child_lifecycle_priority(lifecycle: WorkLifecycle) -> u8 {
    if child_lifecycle_is_unfinished(lifecycle) {
        0
    } else {
        1
    }
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

fn work_item_summary_with_prerequisite_state(
    work: &WorkItem,
    state: WorkPrerequisiteState,
) -> WorkItemSummary {
    let mut summary = work_item_summary(work);
    summary.prerequisite_state = Some(state);
    summary
}

fn bounded_prerequisite_summaries(
    prerequisites: Vec<(WorkItem, WorkPrerequisiteState)>,
    omitted_by_state: [usize; 3],
) -> (Vec<WorkItemSummary>, Vec<WorkSectionOmission>) {
    let reasons = [
        WorkSectionOmissionReason::DeadPrerequisiteCountLimit,
        WorkSectionOmissionReason::PendingPrerequisiteCountLimit,
        WorkSectionOmissionReason::SatisfiedPrerequisiteCountLimit,
    ];
    let omissions = reasons
        .into_iter()
        .zip(omitted_by_state)
        .filter_map(|(reason, omitted_count)| {
            (omitted_count != 0).then_some(WorkSectionOmission {
                section: WorkNextSection::Focus,
                reason,
                omitted_count,
            })
        })
        .collect();
    let summaries = prerequisites
        .into_iter()
        .map(|(work, state)| work_item_summary_with_prerequisite_state(&work, state))
        .collect();
    (summaries, omissions)
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

fn prioritized_focus_evidence(
    candidates: Vec<WorkEvidenceProjectionSummary>,
    obligation_page: &WorkObligationPage,
) -> Vec<ObjectHash> {
    let required_environments = obligation_page
        .items
        .iter()
        .filter(|obligation| obligation.state == WorkObligationState::Open)
        .filter_map(|obligation| obligation.requirement.required_environment.clone())
        .collect();
    prioritized_focus_evidence_hashes(candidates, required_environments)
}

fn prioritized_focus_evidence_hashes(
    mut candidates: Vec<WorkEvidenceProjectionSummary>,
    mut required_environments: Vec<ObjectHash>,
) -> Vec<ObjectHash> {
    candidates.sort_by(|left, right| left.hash.as_str().cmp(right.hash.as_str()));
    required_environments.sort_by(|left, right| left.as_str().cmp(right.as_str()));
    required_environments.dedup();
    let environment_hashes = candidates
        .iter()
        .filter(|candidate| candidate.kind == WorkEvidenceKind::Environment)
        .map(|candidate| candidate.hash.clone())
        .collect::<std::collections::HashSet<_>>();
    let mut selected = Vec::new();
    for environment in required_environments {
        if environment_hashes.contains(&environment) {
            push_focus_evidence(&mut selected, &environment);
        }
        if selected.len() == MAX_FOCUS_RELATIONS {
            return selected;
        }
    }
    for candidate in candidates
        .iter()
        .filter(|candidate| candidate.kind == WorkEvidenceKind::Verification)
    {
        match candidate.environment.as_ref() {
            Some(environment) if environment_hashes.contains(environment) => {
                let environment_is_visible = selected.contains(environment);
                let needed = usize::from(!environment_is_visible) + 1;
                if selected.len() + needed > MAX_FOCUS_RELATIONS {
                    continue;
                }
                // Environment first means byte trimming pops its dependent
                // verification before it can break visible typed closure.
                push_focus_evidence(&mut selected, environment);
                push_focus_evidence(&mut selected, &candidate.hash);
            }
            _ => push_focus_evidence(&mut selected, &candidate.hash),
        }
        if selected.len() == MAX_FOCUS_RELATIONS {
            return selected;
        }
    }
    for candidate in candidates {
        if candidate.kind != WorkEvidenceKind::Verification {
            push_focus_evidence(&mut selected, &candidate.hash);
        }
        if selected.len() == MAX_FOCUS_RELATIONS {
            break;
        }
    }
    selected
}

fn push_focus_evidence(selected: &mut Vec<ObjectHash>, hash: &ObjectHash) {
    if selected.len() < MAX_FOCUS_RELATIONS && !selected.contains(hash) {
        selected.push(hash.clone());
    }
}

fn compact_work_evidence(evidence: &WorkEvidence) -> Result<String, StoreError> {
    let Some(gate) = evidence.gate.as_ref() else {
        return Ok(compact_text(&evidence.summary));
    };
    validate_gate_evidence_payload(evidence).map_err(StoreError::InvalidWorkProjection)?;
    if gate.passed {
        return Ok(compact_text(&format!("gate {} passed", gate.name)));
    }
    let count = gate.failed.len();
    let listed = gate
        .failed
        .iter()
        .take(2)
        .map(|failure| compact_text(failure))
        .collect::<Vec<_>>()
        .join(", ");
    let omitted = count.saturating_sub(2);
    let suffix = if omitted == 0 {
        String::new()
    } else {
        format!(" (+{omitted} more)")
    };
    Ok(compact_text(&format!(
        "gate {} failed ({count} failures): {listed}{suffix}",
        gate.name
    )))
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
            let summary = compact_work_evidence(&evidence)?;
            let gate = evidence.gate.as_ref().map(|gate| WorkGateEvidenceSummary {
                name: gate.name.clone(),
                passed: gate.passed,
                failed_count: gate.failed.len(),
            });
            Ok(WorkEvidenceSummary {
                evidence: hash.clone(),
                evidence_kind: WorkEvidenceKind::Generic,
                gate,
                workspace_id: None,
                source_revision: None,
                producer_session_id: evidence.actor.session_id.clone(),
                actor_id: Some(compact_text(&evidence.actor.actor_id)),
                actor_context: projected_actor_context(&evidence.actor),
                check_kind: None,
                check_fingerprint: None,
                verification_result: None,
                environment_fingerprint: None,
                environment: None,
                environment_components: None,
                summary,
                created_at: evidence.created_at,
            })
        }
        WorkEvidenceKind::Verification => {
            let evidence = store.load_verification_evidence(hash)?;
            Ok(WorkEvidenceSummary {
                evidence: hash.clone(),
                evidence_kind: WorkEvidenceKind::Verification,
                gate: None,
                workspace_id: Some(compact_text(&evidence.source_basis.workspace_id)),
                source_revision: Some(compact_text(&evidence.source_basis.source_revision)),
                producer_session_id: Some(evidence.session_id),
                actor_id: Some(compact_text(&evidence.actor.actor_id)),
                actor_context: projected_actor_context(&evidence.actor),
                check_kind: Some(evidence.check_kind),
                check_fingerprint: Some(evidence.check_fingerprint),
                verification_result: Some(evidence.result),
                environment_fingerprint: None,
                environment: evidence.environment,
                environment_components: None,
                summary: compact_text(&evidence.summary),
                created_at: evidence.completed_at,
            })
        }
        WorkEvidenceKind::Environment => {
            let evidence = store.load_environment_evidence(hash)?;
            Ok(WorkEvidenceSummary {
                evidence: hash.clone(),
                evidence_kind: WorkEvidenceKind::Environment,
                gate: None,
                workspace_id: Some(compact_text(&evidence.source_basis.workspace_id)),
                source_revision: Some(compact_text(&evidence.source_basis.source_revision)),
                producer_session_id: Some(evidence.session_id),
                actor_id: Some(compact_text(&evidence.actor.actor_id)),
                actor_context: projected_actor_context(&evidence.actor),
                check_kind: None,
                check_fingerprint: None,
                verification_result: None,
                environment_fingerprint: Some(evidence.environment_fingerprint),
                environment: None,
                environment_components: evidence.components,
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
    let waived_by = record.resolution.as_ref().and_then(|event| {
        if let WorkObligationResolution::Waived { waived_by, .. } = &event.resolution {
            Some(compact_text(waived_by))
        } else {
            None
        }
    });
    let guidance = if record.state == WorkObligationState::Open {
        WorkObligationGuidance::RecordVerificationThenCheckpoint {
            requirement: record.obligation.requirement.clone(),
            host_waiver_requestable: true,
        }
    } else {
        WorkObligationGuidance::None
    };
    WorkObligationSummary {
        obligation_id: record.obligation.obligation_id,
        definition: record.definition_hash.clone(),
        rule_set: record.obligation.rule_set.clone(),
        state: record.state,
        rule: record.obligation.rule.clone(),
        requirement: record.obligation.requirement.clone(),
        triggering_observation: record.obligation.triggering_observation.clone(),
        resolution: record.resolution_hash.clone(),
        evidence,
        waived_by,
        guidance,
    }
}

fn work_obligation_page(
    store: &SqliteStore,
    work_id: WorkId,
) -> Result<WorkObligationPage, StoreError> {
    let Some(run) = store.latest_work_run(work_id)? else {
        return Ok(WorkObligationPage::default());
    };
    work_obligation_page_from_records(store.work_run_obligations(run.run_id)?)
}

fn work_completion_recovery_page(
    snapshot: &CompletionRecoverySnapshot,
) -> Result<WorkObligationPage, StoreError> {
    let state = matches!(
        &snapshot.recovery.cause,
        WorkCompletionRecoveryCause::OpenObligation { .. }
    )
    .then_some(WorkObligationState::Open);
    let records = snapshot
        .obligations
        .iter()
        .filter(|record| state.is_none_or(|expected| record.state == expected))
        .cloned()
        .collect();
    work_obligation_page_from_records(records)
}

fn sealed_work_obligation_page(
    store: &SqliteStore,
    seal: &CompletionSeal,
) -> Result<WorkObligationPage, StoreError> {
    let records = store.work_run_obligations(seal.run_id)?;
    let mut bindings = records
        .iter()
        .map(|record| {
            let resolution = record.resolution_hash.clone().ok_or_else(|| {
                StoreError::InvalidWorkProjection(format!(
                    "sealed obligation {} has no terminal resolution",
                    record.obligation.obligation_id.0
                ))
            })?;
            Ok(crate::CompletionObligationBinding {
                obligation_id: record.obligation.obligation_id,
                definition: record.definition_hash.clone(),
                resolution,
            })
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    bindings.sort_by(|left, right| {
        left.obligation_id
            .0
            .as_bytes()
            .cmp(right.obligation_id.0.as_bytes())
            .then_with(|| left.definition.as_str().cmp(right.definition.as_str()))
    });
    if bindings != seal.obligations {
        return Err(StoreError::InvalidWorkProjection(format!(
            "completion seal for run {:?} does not match its canonical obligation closure",
            seal.run_id
        )));
    }
    work_obligation_page_from_records(records)
}

fn work_obligation_page_from_records(
    records: Vec<crate::storage::WorkObligationRecord>,
) -> Result<WorkObligationPage, StoreError> {
    let mut page = count_bounded_work_obligation_page(records);
    while serde_json::to_vec(&page)?.len() > MAX_OBLIGATION_PAGE_BYTES
        && trim_obligation_page_once(&mut page)
    {}
    Ok(page)
}

fn count_bounded_work_obligation_page(
    mut records: Vec<crate::storage::WorkObligationRecord>,
) -> WorkObligationPage {
    records.sort_by(|left, right| {
        match (
            left.state == WorkObligationState::Open,
            right.state == WorkObligationState::Open,
        ) {
            (true, true) => left
                .obligation
                .trigger_position
                .position
                .cmp(&right.obligation.trigger_position.position)
                .then_with(|| {
                    left.obligation
                        .obligation_id
                        .0
                        .as_bytes()
                        .cmp(right.obligation.obligation_id.0.as_bytes())
                }),
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            (false, false) => right
                .resolution_position
                .as_ref()
                .map(|position| position.position)
                .cmp(
                    &left
                        .resolution_position
                        .as_ref()
                        .map(|position| position.position),
                )
                .then_with(|| {
                    left.obligation
                        .obligation_id
                        .0
                        .as_bytes()
                        .cmp(right.obligation.obligation_id.0.as_bytes())
                }),
        }
    });
    let omitted_count = records.len().saturating_sub(MAX_FOCUS_RELATIONS);
    if omitted_count > 0 {
        records.truncate(MAX_FOCUS_RELATIONS);
    }
    WorkObligationPage {
        items: records.iter().map(work_obligation_summary).collect(),
        omitted_count,
    }
}

fn trim_obligation_page_once(page: &mut WorkObligationPage) -> bool {
    if page.items.is_empty() {
        return false;
    }
    page.items.pop();
    page.omitted_count = page.omitted_count.saturating_add(1);
    true
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
        if response.memories.take().is_some() {
            // The fixed-size signal is advisory and reannounces until it is
            // delivered. Recording a larger omission row here would increase
            // the response that this pass is trying to fit.
            continue;
        }
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
    if focus.memories.pop().is_some() {
        return true;
    }
    if focus.children.pop().is_some() {
        return true;
    }
    focus.prerequisites.pop().is_some()
        || focus.handoffs.pop().is_some()
        || trim_obligation_page_once(&mut focus.obligation_page)
        || trim_focus_evidence_once(focus)
}

fn trim_focus_evidence_once(focus: &mut WorkFocusView) -> bool {
    if focus.evidence_items.pop().is_some() {
        focus.evidence.pop();
        true
    } else {
        focus.evidence.pop().is_some()
    }
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

fn completion_result(
    store: &SqliteStore,
    seal: &CompletionSeal,
) -> Result<WorkCompleteResult, StoreError> {
    Ok(WorkCompleteResult::Completed(WorkCompletedReceipt {
        seal: crate::CanonicalObject::freeze(seal)?.hash().clone(),
        work_id: seal.work_id,
        run_id: seal.run_id,
        completed_at: seal.completed_at,
        obligation_page: sealed_work_obligation_page(store, seal)?,
    }))
}

fn completion_attempt_key(
    base_key: &str,
    run_feed_cut: &FeedPosition,
) -> Result<String, StoreError> {
    let key = CanonicalObject::freeze(&WorkCompletionAttemptKey {
        base_key,
        run_feed_cut,
    })?;
    Ok(format!("attempt:{}", key.hash().as_str()))
}

fn completion_capture_key(
    base_key: &str,
    work: &WorkItem,
    claim: &WorkClaim,
) -> Result<String, StoreError> {
    let key = CanonicalObject::freeze(&WorkCompletionCaptureKey {
        base_key,
        work_id: work.work_id,
        work_revision: work.revision,
        run_id: claim.run_id,
        claim_id: claim.claim_id,
        claim_fence: claim.fence,
    })?;
    Ok(format!("capture:{}", key.hash().as_str()))
}

fn ensure_completion_replay_target(
    basis: &WorkProtocolBasis,
    actual: WorkId,
    key: &str,
) -> Result<(), StoreError> {
    if basis
        .focused_work
        .as_ref()
        .is_some_and(|work| work.work_id == actual)
    {
        return Ok(());
    }
    Err(StoreError::WorkOperationIdempotencyConflict {
        operation: "work_complete".into(),
        key: key.into(),
    })
}

fn completion_recovery_result(
    work_id: WorkId,
    recovery: WorkCompletionRecovery,
    obligation_page: WorkObligationPage,
) -> WorkCompleteResult {
    let code = match &recovery.cause {
        WorkCompletionRecoveryCause::OpenObligation { .. } => "open_work_obligations",
        WorkCompletionRecoveryCause::RequiredChildUnsealed { .. } => "required_child_unsealed",
        WorkCompletionRecoveryCause::MissingContribution { .. } => "missing_contribution",
        WorkCompletionRecoveryCause::MissingAcceptance { .. } => "missing_acceptance",
    };
    let remedy = if matches!(
        &recovery.cause,
        WorkCompletionRecoveryCause::OpenObligation { .. }
    ) {
        "record the matching host verification, then checkpoint_work acknowledging it, then complete; or request a host/operator waiver"
            .into()
    } else {
        format!(
            "resolve {} for {} {:?}, then retry completion",
            code.replace('_', " "),
            recovery.item.short_ref,
            recovery.item.title
        )
    };
    WorkCompleteResult::Refused(WorkCompleteRefusal {
        code: code.into(),
        work_id,
        obligation_page,
        remedy,
        recovery,
    })
}

#[cfg(test)]
fn completion_command_ref_from_resolution(
    item: &WorkReferenceCandidate,
    resolution: Result<WorkItem, StoreError>,
) -> Result<String, StoreError> {
    match resolution {
        Ok(resolved) if resolved.work_id == item.work_id => Ok(item.short_ref.clone()),
        Ok(resolved) => Err(StoreError::InvalidWorkProjection(format!(
            "short reference {:?} resolved to work {:?} instead of {:?}",
            item.short_ref, resolved.work_id.0, item.work_id.0
        ))),
        Err(StoreError::WorkReferenceAmbiguous { .. }) => Ok(item.work_id.0.to_string()),
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

fn retry_stable_basis_matches(
    exact_matches: bool,
    stored: Option<&serde_json::Value>,
    current: &WorkProtocolBasis,
) -> Result<bool, StoreError> {
    if exact_matches {
        return Ok(true);
    }
    stored
        .cloned()
        .map(serde_json::from_value::<WorkProtocolBasis>)
        .transpose()
        .map(|stored| stored.is_some_and(|stored| stored.retry_stable() == current.retry_stable()))
        .map_err(StoreError::from)
}

fn completion_basis_refresh_is_safe(
    stored: &WorkProtocolBasis,
    current: &WorkProtocolBasis,
    session_id: &SessionId,
) -> bool {
    let (Some(stored_work), Some(current_work), Some(stored_claim), Some(current_claim)) = (
        stored.focused_work.as_ref(),
        current.focused_work.as_ref(),
        stored.claim.as_ref(),
        current.claim.as_ref(),
    ) else {
        return false;
    };

    stored_work == current_work
        && current_work.lifecycle == WorkLifecycle::Open
        && stored_claim.work_id == current_work.work_id
        && current_claim.work_id == current_work.work_id
        && stored_claim.run_id == current_claim.run_id
        && stored_claim.holder == *session_id
        && current_claim.holder == *session_id
        && stored_claim.state == WorkClaimState::Active
        && current_claim.state == WorkClaimState::Active
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
                actor_context: projected_actor_context(&checkpoint.actor),
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
                summary: compact_work_evidence(&evidence)?,
                actor_id: Some(compact_text(&evidence.actor.actor_id)),
                actor_context: projected_actor_context(&evidence.actor),
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
                actor_context: projected_actor_context(&observation.actor),
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
                    "{:?} {:?}; source_revision={}; environment={}",
                    evidence.check_kind,
                    evidence.result,
                    evidence.source_basis.source_revision,
                    evidence
                        .environment
                        .as_ref()
                        .map_or("none", ObjectHash::as_str)
                )),
                actor_id: Some(compact_text(&evidence.actor.actor_id)),
                actor_context: projected_actor_context(&evidence.actor),
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
            let component_summary = evidence.components.as_ref().map_or_else(
                || "opaque_components".into(),
                |components| {
                    format!(
                        "toolchain={}; sandbox={}; workspace={}; capability_map_revision={}",
                        components.toolchain,
                        components.sandbox.as_deref().unwrap_or("none"),
                        components.workspace_id,
                        components.capability_map_revision
                    )
                },
            );
            Ok(WorkChangeProjection::Visible(WorkChangeSummary {
                schema_version: evidence.schema_version,
                object_kind: object_kind.into(),
                work_id: Some(evidence.binding.work_id),
                work_ref: Some(item.short_ref),
                revision: Some(evidence.binding.work_revision),
                change_kind: "environment_evidence".into(),
                summary: compact_text(&format!(
                    "environment {}; source_revision={}; {}",
                    evidence.environment_fingerprint,
                    evidence.source_basis.source_revision,
                    component_summary
                )),
                actor_id: Some(compact_text(&evidence.actor.actor_id)),
                actor_context: projected_actor_context(&evidence.actor),
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
                actor_context: None,
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
            let (change_kind, summary) = obligation_resolution_change_summary(
                &record.obligation.rule.rule_id,
                &event.resolution,
            );
            Ok(WorkChangeProjection::Visible(WorkChangeSummary {
                schema_version: event.schema_version,
                object_kind: object_kind.into(),
                work_id: Some(record.obligation.work_id),
                work_ref: Some(item.short_ref),
                revision: Some(record.obligation.work_revision),
                change_kind: change_kind.into(),
                summary: compact_text(&summary),
                actor_id: Some(compact_text(&event.actor.actor_id)),
                actor_context: projected_actor_context(&event.actor),
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
                        actor_context: projected_actor_context(&version.actor),
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
                        actor_context: projected_actor_context(&assertion.actor),
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
            if event.project_id != *project_id || event.work_root_id != focused_root_id {
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
                        actor_context: projected_actor_context(&event.actor),
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

fn obligation_resolution_change_summary(
    rule_id: &str,
    resolution: &WorkObligationResolution,
) -> (&'static str, String) {
    match resolution {
        WorkObligationResolution::Satisfied { evidence, .. } => (
            "obligation_satisfied",
            format!("{rule_id} satisfied by {evidence}"),
        ),
        WorkObligationResolution::Waived { waived_by, .. } => (
            "obligation_waived",
            format!("{rule_id} waiver attributed to {}", compact_text(waived_by)),
        ),
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
        summary: compact_text(&format!(
            "{change_kind}: {}",
            work_transition_summary(event)
        )),
        actor_id: Some(compact_text(&event.actor.actor_id)),
        actor_context: projected_actor_context(&event.actor),
        created_at: event.created_at,
    }
}

fn work_transition_summary(event: &WorkEvent) -> String {
    let title = compact_text_to(&event.work.title, MAX_HISTORY_TITLE_BYTES);
    let detail = |value: &str| compact_text_to(value, MAX_HISTORY_DETAIL_BYTES);
    match &event.transition {
        WorkTransition::Created { prerequisites } if prerequisites.is_empty() => {
            format!("without prerequisites: \"{title}\"")
        }
        WorkTransition::Created { prerequisites } => {
            format!("with {} prerequisite(s): \"{title}\"", prerequisites.len())
        }
        WorkTransition::Decomposed { children, .. } => {
            format!("added {} child item(s): \"{title}\"", children.len())
        }
        WorkTransition::Revised { .. } => format!("planning fields: \"{title}\""),
        WorkTransition::PrerequisiteAdded { .. } => {
            format!("added one prerequisite: \"{title}\"")
        }
        WorkTransition::PrerequisiteRemoved { .. } => {
            format!("removed one prerequisite: \"{title}\"")
        }
        WorkTransition::Blocked { .. } => event.blocker.as_ref().map_or_else(
            || format!("\"{title}\""),
            |blocker| format!("{}: \"{title}\"", detail(&blocker.detail)),
        ),
        WorkTransition::Unblocked { .. } => format!("removed one blocker: \"{title}\""),
        WorkTransition::Claimed {
            recovered: true, ..
        } => format!("after recovery by a session: \"{title}\""),
        WorkTransition::Claimed {
            recovered: false, ..
        } => format!("by a session: \"{title}\""),
        WorkTransition::Released { reason, .. }
        | WorkTransition::HandoffCancelled { reason, .. }
        | WorkTransition::Reopened { reason, .. } => {
            format!("because {}: \"{title}\"", detail(reason))
        }
        WorkTransition::Checkpointed { .. } => format!("progress: \"{title}\""),
        WorkTransition::HandoffOffered { .. } => {
            format!("to another session: \"{title}\"")
        }
        WorkTransition::HandoffExpired { .. } => format!("expired: \"{title}\""),
        WorkTransition::HandedOff { .. } => {
            format!("from one session to another: \"{title}\"")
        }
        WorkTransition::EvidenceAdded { .. } => format!("for: \"{title}\""),
        WorkTransition::MemoryCaptured { .. } => {
            format!("shared work memory: \"{title}\"")
        }
        WorkTransition::TypedEvidenceAdded { evidence_kind, .. } => {
            format!(
                "{} evidence: \"{title}\"",
                work_evidence_kind_word(*evidence_kind)
            )
        }
        WorkTransition::Completed { .. } => format!("\"{title}\""),
        WorkTransition::Disposed {
            lifecycle, reason, ..
        } => format!(
            "to {} because {}: \"{title}\"",
            work_lifecycle_word(*lifecycle),
            detail(reason)
        ),
        WorkTransition::RequiredChildWaived { reason, .. } => {
            format!(
                "waived one required child because {}: \"{title}\"",
                detail(reason)
            )
        }
    }
}

fn work_evidence_kind_word(kind: WorkEvidenceKind) -> &'static str {
    match kind {
        WorkEvidenceKind::Generic => "generic",
        WorkEvidenceKind::Verification => "verification",
        WorkEvidenceKind::Environment => "environment",
    }
}

fn work_lifecycle_word(lifecycle: WorkLifecycle) -> &'static str {
    match lifecycle {
        WorkLifecycle::Proposed => "proposed",
        WorkLifecycle::Open => "open",
        WorkLifecycle::Completed => "completed",
        WorkLifecycle::Cancelled => "cancelled",
        WorkLifecycle::Superseded => "superseded",
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
        WorkTransition::MemoryCaptured { .. } => "memory_captured",
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
    can_waive_required_child: bool,
    claim_recovery_required: bool,
    completion_capture_ready: bool,
    completion_preflight_ready: bool,
}

fn append_holder_execution_actions(
    allowed: &mut Vec<String>,
    completion_capture_ready: bool,
    completion_preflight_ready: bool,
    handoff_action: &str,
) {
    allowed.extend([
        "work_update:checkpoint".into(),
        "work_update:evidence".into(),
        "work_update:release".into(),
        handoff_action.into(),
    ]);
    if completion_capture_ready || completion_preflight_ready {
        allowed.push("work_complete".into());
    }
}

fn append_claim_actions(
    allowed: &mut Vec<String>,
    status: &ReadyWork,
    context: AllowedNextContext<'_>,
) {
    let AllowedNextContext {
        claim,
        handoffs,
        session,
        now,
        claim_recovery_required,
        completion_capture_ready,
        completion_preflight_ready,
        ..
    } = context;
    match claim {
        Some(claim)
            if claim.state == WorkClaimState::Active
                && claim.holder == *session
                && claim.expires_at > now =>
        {
            let outgoing_offer = handoffs.iter().any(|offer| {
                offer.state == WorkHandoffState::Offered
                    && offer.from == *session
                    && offer.expires_at > now
            });
            append_holder_execution_actions(
                allowed,
                completion_capture_ready,
                completion_preflight_ready,
                if outgoing_offer {
                    "work_handoff:cancel"
                } else {
                    "work_handoff:offer"
                },
            );
        }
        Some(claim)
            if claim.state == WorkClaimState::Active
                && claim.holder == *session
                && claim.expires_at <= now
                && status.availability == WorkAvailability::Ready =>
        {
            allowed.push(WORK_UPDATE_CLAIM_ACTION.into());
        }
        Some(claim)
            if claim.state == WorkClaimState::Active
                && claim.holder == *session
                && claim.expires_at <= now => {}
        Some(claim)
            if claim.state == WorkClaimState::Active
                && claim.holder != *session
                && claim.expires_at > now => {}
        _ if status.availability == WorkAvailability::Ready => {
            if claim_recovery_required {
                allowed.push(WORK_UPDATE_CLAIM_RECOVERY_ACTION.into());
            } else {
                allowed.push(WORK_UPDATE_CLAIM_ACTION.into());
            }
        }
        _ => {}
    }
    if handoffs.iter().any(|offer| {
        offer.state == WorkHandoffState::Offered && offer.to == *session && offer.expires_at > now
    }) {
        allowed.push("work_handoff:accept".into());
    }
}

fn allowed_next(status: &ReadyWork, context: AllowedNextContext<'_>) -> Vec<String> {
    let mut allowed = vec!["work_focus".into()];
    if status.work.lifecycle == WorkLifecycle::Completed {
        allowed.push("work_update:reopen".into());
        return allowed;
    }
    if status.work.lifecycle != WorkLifecycle::Open {
        return allowed;
    }
    let another_session_holds_live_claim = context.claim.is_some_and(|claim| {
        claim.state == WorkClaimState::Active
            && claim.holder != *context.session
            && claim.expires_at > context.now
    });
    if !another_session_holds_live_claim {
        allowed.extend([
            "work_update:revise".into(),
            "work_update:block".into(),
            "work_update:unblock".into(),
            "work_update:add_prerequisite".into(),
            "work_update:remove_prerequisite".into(),
            "work_propose:decompose".into(),
            "work_update:cancel".into(),
            "work_update:supersede".into(),
        ]);
    }
    if context.can_waive_required_child {
        allowed.push("work_update:waive_required_child".into());
    }
    append_claim_actions(&mut allowed, status, context);
    allowed.sort();
    allowed.dedup();
    allowed
}

#[cfg(test)]
mod tests {
    use chrono::{Duration, TimeZone};
    use tempfile::tempdir;

    use super::*;
    use crate::domain::{GATE_EVIDENCE_SUMMARY, SCHEMA_VERSION};
    use crate::verbs::{AgentVerbs, DoneInput, UpdateAction, UpdateInput};

    fn at(second: i64) -> DateTime<Utc> {
        Utc.with_ymd_and_hms(2026, 8, 27, 3, 0, 0)
            .single()
            .expect("fixed timestamp")
            + Duration::seconds(second)
    }

    #[test]
    fn child_lifecycle_priority_keeps_every_unfinished_state_first() {
        assert_eq!(child_lifecycle_priority(WorkLifecycle::Open), 0);
        assert_eq!(child_lifecycle_priority(WorkLifecycle::Proposed), 0);
        assert_eq!(child_lifecycle_priority(WorkLifecycle::Completed), 1);
        assert_eq!(child_lifecycle_priority(WorkLifecycle::Cancelled), 1);
        assert_eq!(child_lifecycle_priority(WorkLifecycle::Superseded), 1);
    }

    #[test]
    fn advisory_memory_acknowledgement_swallows_every_failure_class() {
        for error in [
            StoreError::Sqlite(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                None,
            )),
            StoreError::InvalidProjectMemory("non-contention refusal".into()),
        ] {
            let mut attempted = false;
            ignore_project_memory_advertisement_acknowledgement(|| {
                attempted = true;
                Err(error)
            });
            assert!(attempted);
        }
    }

    #[test]
    fn obligation_waiver_projection_names_asserted_attribution_not_authority() {
        for waived_by in ["shell-operator", "bound-host-session"] {
            let (kind, summary) = obligation_resolution_change_summary(
                "required-check",
                &WorkObligationResolution::Waived {
                    waived_by: waived_by.into(),
                    reason: "explicit exception".into(),
                },
            );
            assert_eq!(kind, "obligation_waived");
            assert_eq!(
                summary,
                format!("required-check waiver attributed to {waived_by}")
            );
            assert!(!summary.contains("authority"));
        }
    }

    #[test]
    fn gate_evidence_projection_uses_bounded_words() {
        let gate = crate::GateEvidenceRecord {
            schema_version: crate::domain::SCHEMA_VERSION,
            name: "cargo-test".into(),
            passed: false,
            failed: ["suite::first", "suite::second", "suite::third"]
                .map(String::from)
                .to_vec(),
            previous: None,
        };
        let evidence = WorkEvidence {
            schema_version: crate::domain::SCHEMA_VERSION,
            work_id: WorkId(uuid::Uuid::from_u128(1)),
            run_id: WorkRunId(uuid::Uuid::from_u128(2)),
            claim_id: crate::WorkClaimId(uuid::Uuid::from_u128(3)),
            claim_fence: 1,
            summary: GATE_EVIDENCE_SUMMARY.into(),
            refs: Vec::new(),
            gate: Some(gate),
            actor: ActorContext {
                actor_id: "agent".into(),
                actor_kind: "agent".into(),
                assurance: AssuranceLevel::Asserted,
                run_id: None,
                session_id: Some(SessionId("session".into())),
                source_tool: Some("gate".into()),
                source_skill: None,
                provenance_chain: Vec::new(),
                reason: "test gate projection".into(),
            },
            created_at: DateTime::<Utc>::UNIX_EPOCH,
        };

        assert_eq!(
            compact_work_evidence(&evidence).expect("typed gate projection"),
            "gate cargo-test failed (3 failures): suite::first, suite::second (+1 more)"
        );
        let mut generic = evidence;
        generic.gate = None;
        assert_eq!(
            compact_work_evidence(&generic).expect("generic evidence projection"),
            generic.summary
        );
        let mut invalid = generic;
        invalid.gate = Some(crate::GateEvidenceRecord {
            schema_version: SCHEMA_VERSION,
            name: "cargo-test".into(),
            passed: true,
            failed: vec!["suite::failed".into()],
            previous: None,
        });
        assert!(matches!(
            compact_work_evidence(&invalid),
            Err(StoreError::InvalidWorkProjection(_))
        ));
    }

    #[test]
    fn failing_gate_evidence_does_not_create_a_completion_barrier() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let service = LocalWorkService::new(
            database.clone(),
            ProjectId("failing-gate-completion".into()),
            "agent".into(),
            SessionId("failing-gate-session".into()),
            Some("protocol-test".into()),
        );
        let work = proposed_root(
            service
                .work_propose(
                    root_input("Failed gate is evidence", "failed-gate-root"),
                    at(0),
                )
                .expect("root proposal"),
        );
        service
            .work_focus(&work.short_ref, at(1))
            .expect("focus root");
        service
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(300),
                    recovery_reason: None,
                    idempotency_key: "failed-gate-claim".into(),
                },
                at(2),
            )
            .expect("claim root");
        let gate = service
            .work_gate(
                "cargo-test",
                &["suite::failure".into()],
                Some("test:failing-gate"),
                at(3),
            )
            .expect("record failing gate");
        let gate_evidence =
            serde_json::from_value::<ObjectHash>(gate.receipt.result).expect("gate evidence hash");

        let completed = service
            .work_complete(
                completion_input(
                    "completion remains independent from gate result",
                    "failed-gate-completion",
                ),
                at(4),
            )
            .expect("failing gate does not block completion");
        let WorkCompleteResult::Completed(completed) = completed else {
            panic!("failing gate evidence must not create a completion refusal");
        };
        let store = SqliteStore::open(&database).expect("store");
        let evidence = store
            .get::<WorkEvidence>(&gate_evidence)
            .expect("read failing gate evidence")
            .expect("canonical failing gate evidence");
        let gate = evidence.gate.expect("typed gate evidence");
        assert!(!gate.passed);
        assert_eq!(gate.failed, ["suite::failure"]);
        let seal = store
            .get::<CompletionSeal>(&completed.seal)
            .expect("read completion seal")
            .expect("canonical completion seal");
        assert!(seal.evidence.contains(&gate_evidence));
        assert!(seal.obligations.is_empty());
        assert!(seal.waivers.is_empty());
    }

    fn obligation_record(
        identity: i64,
        state: WorkObligationState,
        trigger_position: i64,
        resolution_position: Option<i64>,
        rule_padding: usize,
    ) -> crate::storage::WorkObligationRecord {
        let run_id = WorkRunId(uuid::Uuid::from_u128(10));
        crate::storage::WorkObligationRecord {
            definition_hash: ObjectHash::from_canonical_bytes(
                format!("definition-{identity}").as_bytes(),
            ),
            obligation: WorkObligation {
                schema_version: SCHEMA_VERSION,
                obligation_id: crate::WorkObligationId(uuid::Uuid::from_u128(
                    u128::try_from(identity).expect("positive test identity"),
                )),
                project_id: ProjectId("obligation-page-project".into()),
                root_execution_id: crate::RootExecutionId(uuid::Uuid::from_u128(20)),
                root_id: WorkId(uuid::Uuid::from_u128(30)),
                work_id: WorkId(uuid::Uuid::from_u128(31)),
                run_id,
                work_revision: 1,
                rule_set: ObjectHash::from_canonical_bytes(b"obligation-rule-set"),
                rule: crate::BuiltinObligationRuleRef {
                    rule_id: format!("rule-{identity}-{}", "x".repeat(rule_padding)),
                    rule_version: 1,
                },
                triggering_observation: ObjectHash::from_canonical_bytes(
                    format!("observation-{identity}").as_bytes(),
                ),
                trigger_position: crate::FeedPosition {
                    feed: FeedId::RunExecution(run_id),
                    position: trigger_position,
                },
                requirement: crate::VerificationRequirement {
                    check_kind: VerificationKind::Test,
                    check_fingerprint: None,
                    required_environment: None,
                },
                opened_at: at(trigger_position),
            },
            state,
            resolution_hash: (state != WorkObligationState::Open).then(|| {
                ObjectHash::from_canonical_bytes(format!("resolution-{identity}").as_bytes())
            }),
            resolution: None,
            resolution_position: resolution_position.map(|position| crate::FeedPosition {
                feed: FeedId::RunExecution(run_id),
                position,
            }),
        }
    }

    #[test]
    fn obligation_page_keeps_open_items_first_under_count_trimming() {
        let mut records = Vec::new();
        for identity in 1..=5_i64 {
            records.push(obligation_record(
                identity,
                WorkObligationState::Open,
                10 - identity,
                None,
                0,
            ));
        }
        for identity in 6..=12_i64 {
            records.push(obligation_record(
                identity,
                WorkObligationState::Satisfied,
                identity,
                Some(identity),
                0,
            ));
        }
        records.reverse();

        let page = work_obligation_page_from_records(records).expect("bounded obligation page");
        assert!(page.items.len() <= MAX_FOCUS_RELATIONS);
        assert_eq!(page.omitted_count, 12 - page.items.len());
        assert!(
            page.items[..5]
                .iter()
                .all(|item| item.state == WorkObligationState::Open)
        );
        assert!(
            page.items[5..]
                .iter()
                .all(|item| item.state == WorkObligationState::Satisfied)
        );
        let open_ids = page.items[..5]
            .iter()
            .map(|item| item.obligation_id.0.as_u128())
            .collect::<Vec<_>>();
        assert_eq!(open_ids, vec![5, 4, 3, 2, 1]);
        let terminal_ids = page.items[5..]
            .iter()
            .map(|item| item.obligation_id.0.as_u128())
            .collect::<Vec<_>>();
        let expected_terminal = (6_u128..=12)
            .rev()
            .take(page.items.len() - 5)
            .collect::<Vec<_>>();
        assert_eq!(terminal_ids, expected_terminal);
    }

    #[test]
    fn obligation_page_keeps_every_open_item_that_fits_under_byte_trimming() {
        let mut records = (1..=4_i64)
            .map(|identity| {
                obligation_record(identity, WorkObligationState::Open, identity, None, 0)
            })
            .chain((5..=8_i64).map(|identity| {
                obligation_record(
                    identity,
                    WorkObligationState::Waived,
                    identity,
                    Some(identity),
                    3_000,
                )
            }))
            .collect::<Vec<_>>();
        let expected = work_obligation_page_from_records(records.clone())
            .expect("byte-bounded obligation page");
        records.reverse();
        let reversed = work_obligation_page_from_records(records)
            .expect("deterministic byte-bounded obligation page");

        assert_eq!(
            serde_json::to_vec(&expected).expect("serialize expected page"),
            serde_json::to_vec(&reversed).expect("serialize reversed page")
        );
        assert!(serde_json::to_vec(&expected).unwrap().len() <= MAX_OBLIGATION_PAGE_BYTES);
        assert!(expected.omitted_count > 0);
        assert_eq!(expected.omitted_count, 8 - expected.items.len());
        assert!(
            expected.items[..4]
                .iter()
                .all(|item| item.state == WorkObligationState::Open)
        );
        assert_eq!(
            expected.items[..4]
                .iter()
                .map(|item| item.obligation_id.0.as_u128())
                .collect::<Vec<_>>(),
            vec![1, 2, 3, 4]
        );
    }

    #[test]
    fn focus_evidence_keeps_required_environment_and_verification_closure() {
        for prefix in ["fixture-a", "fixture-b"] {
            let hash = |label: &str| {
                ObjectHash::from_canonical_bytes(format!("{prefix}:{label}").as_bytes())
            };
            let required = hash("required-environment");
            let environment_a = hash("environment-a");
            let environment_b = hash("environment-b");
            let mut candidates = vec![
                WorkEvidenceProjectionSummary {
                    hash: required.clone(),
                    kind: WorkEvidenceKind::Environment,
                    environment: None,
                },
                WorkEvidenceProjectionSummary {
                    hash: environment_a.clone(),
                    kind: WorkEvidenceKind::Environment,
                    environment: None,
                },
                WorkEvidenceProjectionSummary {
                    hash: environment_b.clone(),
                    kind: WorkEvidenceKind::Environment,
                    environment: None,
                },
                WorkEvidenceProjectionSummary {
                    hash: hash("verification-a"),
                    kind: WorkEvidenceKind::Verification,
                    environment: Some(environment_a.clone()),
                },
                WorkEvidenceProjectionSummary {
                    hash: hash("verification-b"),
                    kind: WorkEvidenceKind::Verification,
                    environment: Some(environment_b.clone()),
                },
                WorkEvidenceProjectionSummary {
                    hash: hash("verification-without-environment"),
                    kind: WorkEvidenceKind::Verification,
                    environment: None,
                },
            ];
            candidates.extend((0..6).map(|index| WorkEvidenceProjectionSummary {
                hash: hash(&format!("generic-{index}")),
                kind: WorkEvidenceKind::Generic,
                environment: None,
            }));
            let expected =
                prioritized_focus_evidence_hashes(candidates.clone(), vec![required.clone()]);
            candidates.reverse();
            let reversed =
                prioritized_focus_evidence_hashes(candidates.clone(), vec![required.clone()]);

            assert_eq!(expected, reversed);
            assert_eq!(expected.len(), MAX_FOCUS_RELATIONS);
            assert_eq!(expected.first(), Some(&required));
            for candidate in candidates
                .iter()
                .filter(|candidate| candidate.kind == WorkEvidenceKind::Verification)
            {
                let Some(verification_index) =
                    expected.iter().position(|hash| hash == &candidate.hash)
                else {
                    continue;
                };
                if let Some(environment) = candidate.environment.as_ref() {
                    let environment_index = expected
                        .iter()
                        .position(|hash| hash == environment)
                        .expect("visible verification retains its environment");
                    assert!(environment_index < verification_index);
                }
            }
        }
    }

    #[test]
    fn focus_evidence_prioritizes_environments_from_the_visible_obligation_page() {
        let environment_hash = |identity: i64| {
            let value = if identity <= 8 {
                100 + identity
            } else {
                identity - 8
            };
            ObjectHash::from_stored(format!("{value:064x}")).expect("valid environment hash")
        };
        let count_records = (1..=10_i64)
            .rev()
            .map(|identity| {
                obligation_record(identity, WorkObligationState::Open, identity, None, 0)
            })
            .collect::<Vec<_>>();
        let mut count_page = count_bounded_work_obligation_page(count_records);
        assert_eq!(count_page.items.len(), MAX_FOCUS_RELATIONS);
        assert_eq!(count_page.omitted_count, 2);
        for item in &mut count_page.items {
            item.requirement.required_environment = Some(environment_hash(
                i64::try_from(item.obligation_id.0.as_u128()).expect("small fixture identity"),
            ));
        }

        let mut byte_records = (1..=10_i64)
            .map(|identity| {
                let mut record =
                    obligation_record(identity, WorkObligationState::Open, identity, None, 0);
                record.obligation.requirement.required_environment =
                    Some(environment_hash(identity));
                record
            })
            .collect::<Vec<_>>();
        byte_records.reverse();
        let byte_page = work_obligation_page_from_records(byte_records).expect("byte-bounded page");
        assert!(byte_page.items.len() < MAX_FOCUS_RELATIONS);
        assert_eq!(byte_page.omitted_count, 10 - byte_page.items.len());

        let candidates = (1..=10_i64)
            .rev()
            .map(|identity| WorkEvidenceProjectionSummary {
                hash: environment_hash(identity),
                kind: WorkEvidenceKind::Environment,
                environment: None,
            })
            .collect::<Vec<_>>();
        let selected = prioritized_focus_evidence(candidates.clone(), &count_page);
        for visible in &count_page.items {
            let required = visible
                .requirement
                .required_environment
                .as_ref()
                .expect("visible obligation requires an environment");
            assert!(selected.contains(required));
        }
        assert!(!selected.contains(&environment_hash(9)));
        assert!(!selected.contains(&environment_hash(10)));

        let selected_after_byte_trim = prioritized_focus_evidence(candidates, &byte_page);
        for visible in &byte_page.items {
            let required = visible
                .requirement
                .required_environment
                .as_ref()
                .expect("visible obligation requires an environment");
            assert!(selected_after_byte_trim.contains(required));
        }
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
            idempotency_key: key.into(),
        }
    }

    fn proposed_root(result: WorkProposeResult) -> WorkItemSummary {
        match result {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        }
    }

    fn completion_input(summary: &str, key: &str) -> WorkCompleteInput {
        WorkCompleteInput {
            capture: Some(WorkCompletionCaptureInput {
                summary: summary.into(),
                refs: Vec::new(),
            }),
            evidence: Vec::new(),
            acceptance: None,
            note: None,
            idempotency_key: key.into(),
        }
    }

    fn commit_completion_core_without_finishing(
        service: &LocalWorkService,
        input: &WorkCompleteInput,
        now: DateTime<Utc>,
    ) -> CompletionSeal {
        let mut store = service.store().expect("completion store");
        let basis = service
            .protocol_basis(&store, true, false, None, now)
            .expect("completion basis");
        let intent = service.protocol_intent(input);
        let raw_key = service
            .effective_idempotency_key(
                &input.idempotency_key,
                "work_complete",
                &basis,
                &intent,
                now,
            )
            .expect("completion key");
        store
            .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
                project_id: &service.project_id,
                session_id: &service.session_id,
                operation: "work_complete",
                idempotency_key: &raw_key,
                intent: &intent,
                basis: &basis,
                now,
            })
            .expect("pending completion attempt");
        let work = basis.focused_work.clone().expect("focused completion work");
        let claim = service
            .live_protocol_claim(&basis, &work, now)
            .expect("completion claim");
        let actor = service.actor("work_complete", "complete ambient local work");
        let evidence_basis =
            LocalWorkService::completion_evidence_basis(&store, &claim, &input.evidence)
                .expect("completion evidence basis");
        let acceptance = LocalWorkService::prevalidate_completion_acceptance(
            &work,
            input.acceptance.as_deref(),
            input.note.as_deref(),
            &evidence_basis,
            actor.assurance,
            &actor.actor_id,
        )
        .expect("completion acceptance");
        let prepared = service
            .prepare_completion_evidence(
                &mut store,
                CompletionEvidencePlan {
                    work: &work,
                    claim: &claim,
                    capture: input.capture.as_ref(),
                    evidence: evidence_basis,
                    base_key: &raw_key,
                    now,
                },
            )
            .expect("completion substeps");
        let scoped_key = service
            .core_operation_key("work_complete", &prepared.attempt_key, "complete_work")
            .expect("completion core key");
        let evidence = prepared.evidence;
        let acceptance = bind_completion_acceptance_evidence(acceptance, &evidence);
        match store
            .complete_work_for_protocol(
                &CompleteWorkRequest {
                    work_id: work.work_id,
                    run_id: claim.run_id,
                    holder: service.session_id.clone(),
                    expected_work_revision: work.revision,
                    claim_id: claim.claim_id,
                    claim_fence: claim.fence,
                    evidence,
                    acceptance,
                    drain: CompletionDrainAttestation {
                        reconciled_action_outcomes: Vec::new(),
                        released_resource_leases: Vec::new(),
                    },
                    actor,
                    idempotency_key: scoped_key,
                    completed_at: now,
                },
                &DevelopmentNoopRedactor,
            )
            .expect("completion core commits")
        {
            CompleteWorkStorageResult::Completed(seal) => *seal,
            CompleteWorkStorageResult::Recovery(_) => {
                panic!("completion fixture must cross every barrier")
            }
        }
    }

    fn completion_run_feed_head(service: &LocalWorkService, work_id: WorkId) -> i64 {
        let store = service.store().expect("completion-basis store");
        let run_id = store
            .latest_work_run(work_id)
            .expect("latest work run")
            .expect("completion-basis run")
            .run_id;
        store
            .work_feed_head(&FeedId::RunExecution(run_id))
            .expect("completion run-feed head")
    }

    #[test]
    fn prerequisite_summary_preserves_states_and_public_omission_reasons() {
        let actor = ActorContext {
            actor_id: "agent".into(),
            actor_kind: "test_agent".into(),
            assurance: AssuranceLevel::Asserted,
            run_id: None,
            session_id: Some(SessionId("session".into())),
            source_tool: Some("test".into()),
            source_skill: None,
            provenance_chain: Vec::new(),
            reason: "test prerequisite summary translation".into(),
        };
        let item = |index: u128| {
            let work_id = WorkId(uuid::Uuid::from_u128(index));
            WorkItem {
                schema_version: SCHEMA_VERSION,
                project_id: ProjectId("prerequisite-summary".into()),
                work_id,
                short_ref: format!("w-{index:012x}"),
                root_id: work_id,
                parent_id: None,
                child_requirement: ChildRequirement::Optional,
                title: format!("Prerequisite {index}"),
                outcome: "Translated prerequisite".into(),
                acceptance: Vec::new(),
                kind: WorkItemKind::Task,
                priority: 2,
                labels: Vec::new(),
                assigned_to: None,
                deferred_until: None,
                origin: WorkOrigin::Local,
                source_snapshot_id: None,
                lifecycle: WorkLifecycle::Open,
                revision: 1,
                active_run_id: None,
                superseded_by: None,
                created_by: actor.clone(),
                created_at: at(0),
                updated_at: at(0),
            }
        };
        let states = [
            WorkPrerequisiteState::Dead,
            WorkPrerequisiteState::Pending,
            WorkPrerequisiteState::Satisfied,
        ];
        let prerequisites = states
            .into_iter()
            .enumerate()
            .map(|(index, state)| (item(index as u128), state))
            .collect();
        let (summaries, omissions) = bounded_prerequisite_summaries(prerequisites, [3, 2, 1]);

        assert_eq!(
            summaries
                .iter()
                .map(|summary| summary.prerequisite_state)
                .collect::<Vec<_>>(),
            states.map(Some)
        );
        assert_eq!(
            omissions
                .iter()
                .map(|omission| (omission.reason, omission.omitted_count))
                .collect::<Vec<_>>(),
            vec![
                (WorkSectionOmissionReason::DeadPrerequisiteCountLimit, 3),
                (WorkSectionOmissionReason::PendingPrerequisiteCountLimit, 2,),
                (
                    WorkSectionOmissionReason::SatisfiedPrerequisiteCountLimit,
                    1,
                ),
            ]
        );
    }

    #[derive(Clone, Copy, Debug)]
    struct ScaleSample {
        elapsed_us: u128,
        canonical_decodes: usize,
        work_event_decodes: usize,
        item_decodes: usize,
    }

    fn measure_scale_operation<T>(
        samples: &mut Vec<ScaleSample>,
        operation: impl FnOnce() -> T,
    ) -> T {
        crate::canonical::reset_canonical_decode_count();
        crate::storage::reset_work_event_decode_count();
        crate::storage::reset_work_item_projection_decode_count();
        let started = std::time::Instant::now();
        let result = operation();
        samples.push(ScaleSample {
            elapsed_us: started.elapsed().as_micros(),
            canonical_decodes: crate::canonical::canonical_decode_count(),
            work_event_decodes: crate::storage::work_event_decode_count(),
            item_decodes: crate::storage::work_item_projection_decode_count(),
        });
        result
    }

    fn scale_p95<T: Copy + Ord>(values: impl Iterator<Item = T>) -> T {
        let mut values = values.collect::<Vec<_>>();
        assert!(!values.is_empty(), "scale percentile needs samples");
        values.sort_unstable();
        values[(values.len() * 95).div_ceil(100) - 1]
    }

    fn report_scale_samples(operation: &str, samples: &[ScaleSample]) {
        eprintln!(
            "claim mutation scale {operation}: samples={} p95_us={} canonical_decodes_p95={} canonical_decodes_max={} work_event_decodes_p95={} work_event_decodes_max={} item_decodes_p95={} item_decodes_max={}",
            samples.len(),
            scale_p95(samples.iter().map(|sample| sample.elapsed_us)),
            scale_p95(samples.iter().map(|sample| sample.canonical_decodes)),
            samples
                .iter()
                .map(|sample| sample.canonical_decodes)
                .max()
                .expect("scale samples"),
            scale_p95(samples.iter().map(|sample| sample.work_event_decodes)),
            samples
                .iter()
                .map(|sample| sample.work_event_decodes)
                .max()
                .expect("scale samples"),
            scale_p95(samples.iter().map(|sample| sample.item_decodes)),
            samples
                .iter()
                .map(|sample| sample.item_decodes)
                .max()
                .expect("scale samples"),
        );
    }

    #[test]
    fn core_operation_keys_separate_protocol_variants_and_suboperations() {
        let service = LocalWorkService::new(
            PathBuf::from("unused.sqlite3"),
            ProjectId("key-project".into()),
            "agent".into(),
            SessionId("key-session".into()),
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
    fn local_work_service_rejects_blank_asserted_identity() {
        let directory = tempdir().expect("temporary directory");
        for (actor_id, session_id) in [("   ", "session"), ("agent", "\t")] {
            let service = LocalWorkService::new(
                directory
                    .path()
                    .join(format!("{}.sqlite3", session_id.len())),
                ProjectId("blank-identity-project".into()),
                actor_id.into(),
                SessionId(session_id.into()),
                None,
            );
            assert!(matches!(
                service.work_next(1, WorkNextQuery::default(), at(0)),
                Err(StoreError::InvalidWork(detail))
                    if detail.contains("non-empty asserted actor and session")
            ));
        }
    }

    #[test]
    fn local_work_service_normalizes_actor_context_without_refusing_words() {
        let directory = tempdir().expect("temporary directory");
        for (index, (actor_context, expected)) in [
            (
                format!("  model=codex\n{}  ", "🙂".repeat(100)),
                format!("model=codex {}", "🙂".repeat(61)),
            ),
            ("\n\t".into(), String::new()),
        ]
        .into_iter()
        .enumerate()
        {
            let service = LocalWorkService::new_with_attribution(
                directory
                    .path()
                    .join(format!("actor-context-{index}.sqlite3")),
                ProjectId("actor-context-bound-project".into()),
                "agent".into(),
                SessionId("session".into()),
                None,
                Some(actor_context),
                WorkAttributionDefaults::default(),
            );
            service
                .work_next(1, WorkNextQuery::default(), at(0))
                .expect("normalized context must not refuse a word");
            let actor = service.actor("work_next", "test normalized actor context");
            if expected.is_empty() {
                assert!(actor.attribution_context().is_none());
                assert!(!actor.provenance_chain.iter().any(|link| {
                    link.reference.as_deref() == Some(ACTOR_CONTEXT_PROVENANCE_REFERENCE)
                }));
            } else {
                assert_eq!(actor.attribution_context(), Some(expected.as_str()));
            }
            assert!(actor.provenance_chain.contains(&ProvenanceLink {
                relation: ProvenanceRelation::DerivedFrom,
                source: "actor_context:normalized".into(),
                reference: Some(ACTOR_CONTEXT_NORMALIZED_REFERENCE.into()),
            }));
            assert!(!format!("{service:?}").contains("model=codex"));
        }
    }

    #[test]
    fn terminal_actor_labels_escape_asserted_identity_and_context() {
        let safe = terminal_safe_actor_label("agent\nname", Some("model=codex\u{202e}"));
        assert!(!safe.chars().any(is_unsafe_rendered_text_char));
        assert!(safe.contains("agent\\nname"));
        assert!(safe.contains("model=codex\\u{202e}"));
    }

    #[test]
    fn shell_attribution_defaults_are_explicit_in_actor_provenance() {
        let directory = tempdir().expect("temporary directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("defaulted-attribution-project".into());
        let service = LocalWorkService::new_with_attribution(
            database.clone(),
            project,
            "os-user".into(),
            SessionId("process-session".into()),
            None,
            None,
            WorkAttributionDefaults {
                actor: Some(WorkActorDefaultSource::OsUserEnvironment),
                session: true,
            },
        );
        let work = proposed_root(
            service
                .work_propose(
                    root_input("Defaulted attribution", "defaulted-attribution"),
                    at(0),
                )
                .expect("persist defaulted attribution"),
        );
        let store = SqliteStore::open(&database).expect("defaulted attribution store");
        let entry = store
            .work_event_tail(work.work_id, 1)
            .expect("defaulted attribution event")
            .pop()
            .expect("created event");
        let event = store
            .get::<WorkEvent>(&entry.object_hash)
            .expect("read defaulted attribution event")
            .expect("canonical defaulted attribution event");
        let actor = event.actor;
        assert_eq!(actor.actor_id, "os-user");
        assert_eq!(actor.session_id, Some(SessionId("process-session".into())));
        assert!(actor.provenance_chain.contains(&ProvenanceLink {
            relation: ProvenanceRelation::DerivedFrom,
            source: "defaulted:os_user_environment".into(),
            reference: Some("actor_id".into()),
        }));
        assert!(actor.provenance_chain.contains(&ProvenanceLink {
            relation: ProvenanceRelation::DerivedFrom,
            source: "defaulted:process_session".into(),
            reference: Some("session_id".into()),
        }));

        let fallback = LocalWorkService::new_with_attribution(
            PathBuf::from("unused.sqlite3"),
            ProjectId("fallback-attribution-project".into()),
            "local-user-1".into(),
            SessionId("process-session".into()),
            None,
            None,
            WorkAttributionDefaults {
                actor: Some(WorkActorDefaultSource::ProcessFallback),
                session: false,
            },
        )
        .actor("work_next", "test fallback attribution");
        assert!(fallback.provenance_chain.contains(&ProvenanceLink {
            relation: ProvenanceRelation::DerivedFrom,
            source: "defaulted:process_actor".into(),
            reference: Some("actor_id".into()),
        }));

        let injected = LocalWorkService::new(
            PathBuf::from("unused.sqlite3"),
            ProjectId("injected-attribution-project".into()),
            " injected actor ".into(),
            SessionId(" injected session ".into()),
            None,
        )
        .actor("work_next", "test injected attribution");
        assert_eq!(injected.actor_id, " injected actor ");
        assert_eq!(
            injected.session_id,
            Some(SessionId(" injected session ".into()))
        );
        assert_eq!(injected.provenance_chain.len(), 1);

        let contextual = LocalWorkService::new_with_attribution(
            PathBuf::from("unused.sqlite3"),
            ProjectId("contextual-attribution-project".into()),
            "greg/codex".into(),
            SessionId("contextual-session".into()),
            None,
            Some("model=opus-4.1;reasoning=high".into()),
            WorkAttributionDefaults::default(),
        )
        .actor("work_next", "test contextual attribution");
        assert_eq!(
            contextual.attribution_context(),
            Some("model=opus-4.1;reasoning=high")
        );
        assert_eq!(contextual.actor_id, "greg/codex");
        assert!(
            !contextual.provenance_chain.iter().any(|link| {
                link.reference.as_deref() == Some(ACTOR_CONTEXT_NORMALIZED_REFERENCE)
            })
        );
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
        let service = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            SessionId("session".into()),
            Some("protocol-test".into()),
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
            obligation_rule_set: ObjectHash::from_canonical_bytes(b"obligation-rule-set"),
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
        reason = "one canonical event fixture exercises the complete agent projection boundary"
    )]
    fn work_event_projection_does_not_expose_transition_fences_or_hashes() {
        let work_id = WorkId(uuid::Uuid::from_u128(11));
        let run_id = WorkRunId(uuid::Uuid::from_u128(12));
        let claim_id = crate::WorkClaimId(uuid::Uuid::from_u128(13));
        let actor = ActorContext {
            actor_id: "agent".into(),
            actor_kind: "agent".into(),
            assurance: AssuranceLevel::Asserted,
            run_id: Some(run_id.0.to_string()),
            session_id: Some(SessionId("projection-session".into())),
            source_tool: Some("test".into()),
            source_skill: None,
            provenance_chain: Vec::new(),
            reason: "pin the agent projection boundary".into(),
        };
        let work = WorkItem {
            schema_version: SCHEMA_VERSION,
            project_id: ProjectId("projection-project".into()),
            work_id,
            short_ref: "w-projection".into(),
            root_id: work_id,
            parent_id: None,
            child_requirement: ChildRequirement::Required,
            title: "Projection boundary".into(),
            outcome: "No transition secrets".into(),
            acceptance: vec!["Only compact fields are visible".into()],
            kind: WorkItemKind::Task,
            priority: 1,
            labels: Vec::new(),
            assigned_to: None,
            deferred_until: None,
            origin: WorkOrigin::Local,
            source_snapshot_id: None,
            lifecycle: WorkLifecycle::Open,
            revision: 1,
            active_run_id: Some(run_id),
            superseded_by: None,
            created_by: actor.clone(),
            created_at: at(0),
            updated_at: at(0),
        };
        let claim = WorkClaim {
            claim_id,
            work_id,
            run_id,
            accepted_work_revision: 1,
            holder: SessionId("local-process-42-123e4567-e89b-42d3-a456-426614174000".into()),
            expires_at: at(60),
            revision: 1,
            fence: 77,
            state: WorkClaimState::Active,
        };
        let mut event = WorkEvent {
            schema_version: SCHEMA_VERSION,
            project_id: work.project_id.clone(),
            root_id: work_id,
            work_id,
            run_id: Some(run_id),
            revision: 1,
            work,
            run: None,
            root_execution: None,
            claim: Some(claim.clone()),
            handoff_offer: None,
            blocker: None,
            relation_fingerprint: ObjectHash::from_canonical_bytes(b"relations"),
            transition: WorkTransition::Claimed {
                claim: claim.clone(),
                recovered: false,
            },
            actor,
            created_at: at(1),
        };
        let claimed_summary = agent_work_event_summary(&event);
        assert_eq!(
            claimed_summary.summary,
            "claimed: by a session: \"Projection boundary\""
        );
        let claimed = serde_json::to_string(&claimed_summary).expect("serialize claimed summary");
        assert!(!claimed.contains(&claim_id.0.to_string()));
        assert!(!claimed.contains("123e4567-e89b-42d3-a456-426614174000"));
        assert!(!claimed.contains("\"fence\""));

        let checkpoint = ObjectHash::from_canonical_bytes(b"private-checkpoint-marker");
        let offer = ObjectHash::from_canonical_bytes(b"private-offer-marker");
        event.transition = WorkTransition::HandoffOffered {
            offer_id: crate::WorkHandoffOfferId(uuid::Uuid::from_u128(14)),
            to: SessionId("next-session".into()),
            checkpoint: checkpoint.clone(),
            offer: offer.clone(),
        };
        let offered_summary = agent_work_event_summary(&event);
        assert_eq!(
            offered_summary.summary,
            "handoff_offered: to another session: \"Projection boundary\""
        );
        let offered = serde_json::to_string(&offered_summary).expect("serialize handoff summary");
        assert!(!offered.contains(&checkpoint.to_string()));
        assert!(!offered.contains(&offer.to_string()));
        assert!(!offered.contains("offer_id"));

        event.work.title = "long title ".repeat(80);
        event.transition = WorkTransition::Claimed {
            claim,
            recovered: true,
        };
        let long_claim = agent_work_event_summary(&event).summary;
        assert!(long_claim.starts_with("claimed: after recovery by a session: \""));
        assert!(long_claim.len() <= MAX_SUMMARY_BYTES);

        event.transition = WorkTransition::TypedEvidenceAdded {
            evidence: ObjectHash::from_canonical_bytes(b"verification"),
            evidence_kind: WorkEvidenceKind::Verification,
        };
        assert!(
            agent_work_event_summary(&event)
                .summary
                .starts_with("typed_evidence_added: verification evidence: \"")
        );

        event.transition = WorkTransition::Disposed {
            lifecycle: WorkLifecycle::Cancelled,
            replacement_id: None,
            reason: "bounded reason ".repeat(40),
        };
        let disposed = agent_work_event_summary(&event).summary;
        assert!(disposed.starts_with("disposed: to cancelled because bounded reason"));
        assert!(disposed.len() <= MAX_SUMMARY_BYTES);
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
        let service = LocalWorkService::new(
            database,
            project,
            "agent".into(),
            SessionId("session".into()),
            Some("protocol-test".into()),
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
    fn interrupted_attempt_cannot_follow_changed_focus() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let focus_project = ProjectId("focus-attempt".into());
        let focus_service = LocalWorkService::new(
            database.clone(),
            focus_project.clone(),
            "agent".into(),
            SessionId("focus-session".into()),
            Some("protocol-test".into()),
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
            .protocol_basis(&store, true, false, None, at(4))
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
        let session = SessionId("committed-update-session".into());
        let service = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            session.clone(),
            Some("protocol-test".into()),
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
            .protocol_basis(&store, true, false, None, at(1))
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
        let authority = service.planning_authority(basis.claim.as_ref(), &original, at(1));
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
        let session = SessionId("committed-handoff-session".into());
        let service = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            session.clone(),
            Some("protocol-test".into()),
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
            .protocol_basis(&store, true, true, None, at(2))
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
        reason = "one scenario shows discard-on-focus-change, implicit delivery, and dense continuation in order"
    )]
    fn staged_page_never_blocks_focus_and_is_delivered_by_the_next_call() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("implicit-delivery".into());
        let session = SessionId("implicit-delivery-session".into());
        let service = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            session.clone(),
            Some("protocol-test".into()),
        );
        let peer = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            SessionId("implicit-delivery-peer".into()),
            Some("protocol-test".into()),
        );
        service
            .work_propose(root_input("First root", "implicit-first"), at(0))
            .expect("first root");
        let target = match peer
            .work_propose(root_input("Second root", "implicit-second"), at(1))
            .expect("second root")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        let changes_only = || WorkNextQuery {
            sections: vec![WorkNextSection::Changes],
            ..WorkNextQuery::default()
        };
        let first = service
            .work_next(20, changes_only(), at(2))
            .expect("first page");
        let first_head = first.delivered_through.expect("first boundary");
        assert!(first_head > 0);
        assert_eq!(first.session.confirmed_project_cursor, 0);

        // Focus changes while the page is still staged; the page projected
        // under the old focus is discarded and nothing is confirmed.
        let focused = service
            .work_focus(&target.short_ref, at(3))
            .expect("focus while a page is pending");
        assert_eq!(focused.status.work.work_id, target.work_id);
        let discarded = SqliteStore::open(&database)
            .expect("store")
            .work_session_state(&project, &session, at(3))
            .expect("session state");
        assert_eq!(discarded.tentative_project_cursor, None);
        assert_eq!(discarded.project_cursor, 0);
        assert_eq!(discarded.focused_work_id, Some(target.work_id));

        peer.work_propose(root_input("Third root", "implicit-third"), at(4))
            .expect("append after the first page was staged");
        // The next call recomputes the interval under the new focus from the
        // confirmed cursor, densely through the new head.
        let second = service
            .work_next(20, changes_only(), at(5))
            .expect("second page");
        assert_eq!(second.session.confirmed_project_cursor, 0);
        let second_head = second.delivered_through.expect("second boundary");
        assert!(second_head > first_head);
        let positions = second
            .changes
            .as_ref()
            .expect("second changes")
            .iter()
            .map(|change| change.entry.position.position)
            .collect::<Vec<_>>();
        assert_eq!(positions, (1..=second_head).collect::<Vec<_>>());
        // Sections without changes neither deliver nor stage.
        let focus_only = service
            .work_next(
                20,
                WorkNextQuery {
                    sections: vec![WorkNextSection::Focus],
                    ..WorkNextQuery::default()
                },
                at(6),
            )
            .expect("focus-only view");
        assert_eq!(focus_only.session.confirmed_project_cursor, 0);
        assert_eq!(focus_only.delivered_through, None);
        // Without a focus change, the next call delivers the previous page
        // implicitly and continues densely from its boundary.
        peer.work_propose(root_input("Fourth root", "implicit-fourth"), at(6))
            .expect("append after the second page was staged");
        let third = service
            .work_next(20, changes_only(), at(7))
            .expect("third page");
        assert_eq!(third.session.confirmed_project_cursor, second_head);
        let third_head = third.delivered_through.expect("third boundary");
        assert!(third_head > second_head);
        let positions = third
            .changes
            .as_ref()
            .expect("third changes")
            .iter()
            .map(|change| change.entry.position.position)
            .collect::<Vec<_>>();
        assert_eq!(
            positions,
            (second_head + 1..=third_head).collect::<Vec<_>>()
        );
        let idle = service
            .work_next(20, changes_only(), at(8))
            .expect("idle page");
        assert_eq!(idle.session.confirmed_project_cursor, third_head);
        assert!(idle.changes.as_ref().expect("no new changes").is_empty());
    }

    #[test]
    fn omitted_idempotency_key_replays_identical_calls_and_separates_different_ones() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("derived-keys".into());
        let service = LocalWorkService::new(
            database,
            project,
            "agent".into(),
            SessionId("derived-keys-session".into()),
            Some("protocol-test".into()),
        );
        let root_of = |result: WorkProposeResult| match result {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        let first = root_of(
            service
                .work_propose(root_input("Keyless root", ""), at(0))
                .expect("keyless root"),
        );
        let replayed = root_of(
            service
                .work_propose(root_input("Keyless root", ""), at(1))
                .expect("identical keyless call replays"),
        );
        assert_eq!(replayed.work_id, first.work_id);
        let other = root_of(
            service
                .work_propose(root_input("Different keyless root", ""), at(2))
                .expect("different keyless call creates"),
        );
        assert_ne!(other.work_id, first.work_id);

        service
            .select_work(&first.short_ref, at(3))
            .expect("select the first root");
        let claim = WorkUpdateInput::Claim {
            ttl_seconds: Some(300),
            recovery_reason: None,
            idempotency_key: String::new(),
        };
        let claimed = service.work_update(claim.clone(), at(3)).expect("claim");
        assert_eq!(claimed.receipt.work_id, first.work_id);
        // The item moved (it is now claimed), so the identical keyless call is
        // a new attempt; claiming work this session already holds returns the
        // same live claim instead of a refusal.
        let claimed_again = service
            .work_update(claim, at(4))
            .expect("re-claiming held work returns the live claim");
        assert_eq!(
            serde_json::to_value(&claimed_again.receipt).expect("receipt"),
            serde_json::to_value(&claimed.receipt).expect("receipt")
        );
        let checkpoint = |summary: &str| WorkUpdateInput::Checkpoint {
            summary: summary.into(),
            evidence: None,
            idempotency_key: String::new(),
        };
        let noted = service
            .work_update(checkpoint("found the cause"), at(5))
            .expect("first checkpoint");
        // A checkpoint does not move the focused work/claim basis, so the
        // identical keyless note replays instead of duplicating.
        let noted_again = service
            .work_update(checkpoint("found the cause"), at(6))
            .expect("identical checkpoint replays");
        assert_eq!(
            serde_json::to_value(&noted_again.receipt).expect("receipt"),
            serde_json::to_value(&noted.receipt).expect("receipt")
        );
        // A refused attempt leaves the basis unchanged, so its exact retry
        // replays the refusal rather than inventing a new attempt.
        let stale_release = WorkUpdateInput::Release {
            reason: String::new(),
            waiver_reason: None,
            idempotency_key: String::new(),
        };
        let first_refusal = service
            .work_update(stale_release.clone(), at(7))
            .expect_err("an empty release reason is refused");
        let second_refusal = service
            .work_update(stale_release, at(8))
            .expect_err("the identical retry is refused the same way");
        assert_eq!(first_refusal.to_string(), second_refusal.to_string());
    }

    #[test]
    fn actor_context_does_not_change_work_protocol_identity() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("context-independent-intent".into());
        let service = |context: &str| {
            LocalWorkService::new_with_attribution(
                database.clone(),
                project.clone(),
                "agent".into(),
                SessionId("stable-session".into()),
                Some("protocol-test".into()),
                Some(context.into()),
                WorkAttributionDefaults::default(),
            )
        };
        let first = service("model=first")
            .work_propose(root_input("Context-independent retry", "stable-key"), at(0))
            .expect("first operation");
        let replay = service("model=second")
            .work_propose(root_input("Context-independent retry", "stable-key"), at(1))
            .expect("context change must replay instead of conflicting");
        let WorkProposeResult::Root {
            work: first_work, ..
        } = first
        else {
            panic!("expected root");
        };
        let WorkProposeResult::Root {
            work: replayed_work,
            ..
        } = replay
        else {
            panic!("expected replayed root");
        };
        assert_eq!(replayed_work.work_id, first_work.work_id);
    }

    #[test]
    fn select_work_sets_focus_for_the_next_mutation() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("select-work".into());
        let service = LocalWorkService::new(
            database,
            project,
            "agent".into(),
            SessionId("select-work-session".into()),
            Some("protocol-test".into()),
        );
        let first = match service
            .work_propose(root_input("Select first", "select-first"), at(0))
            .expect("first root")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        service
            .work_propose(root_input("Select second", "select-second"), at(1))
            .expect("second root becomes focus");
        service
            .select_work(&first.short_ref, at(2))
            .expect("select the first root by short ref");
        let claimed = service
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(300),
                    recovery_reason: None,
                    idempotency_key: "select-claim".into(),
                },
                at(3),
            )
            .expect("claim the selected root");
        assert_eq!(claimed.receipt.work_id, first.work_id);
        assert!(matches!(
            service.select_work("no-such-ref", at(4)),
            Err(StoreError::WorkNotFound(_) | StoreError::InvalidWork(_))
        ));
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one scenario shows the omitted, explicit-empty, and synthesized defaults side by side"
    )]
    fn omitted_checkpoint_evidence_and_acceptance_take_safe_defaults() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("safe-defaults".into());
        let service = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            SessionId("safe-defaults-session".into()),
            Some("protocol-test".into()),
        );
        service
            .work_propose(
                WorkProposeInput::Root {
                    title: "Safe defaults".into(),
                    outcome: "omitted fields do the safe thing".into(),
                    acceptance: vec!["first criterion".into(), "second criterion".into()],
                    work_kind: None,
                    priority: None,
                    labels: Vec::new(),
                    assigned_to: None,
                    deferred_until: None,
                    idempotency_key: "defaults-root".into(),
                },
                at(0),
            )
            .expect("root");
        service
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(300),
                    recovery_reason: None,
                    idempotency_key: "defaults-claim".into(),
                },
                at(1),
            )
            .expect("claim");
        let evidence = |summary: &str, key: &str| WorkUpdateInput::Evidence {
            summary: summary.into(),
            refs: vec![format!("test:{key}")],
            attach: None,
            idempotency_key: key.into(),
        };
        let first_evidence: ObjectHash = serde_json::from_value(
            service
                .work_update(evidence("first finding", "defaults-evidence-1"), at(2))
                .expect("first evidence")
                .receipt
                .result,
        )
        .expect("evidence hash");
        let second_evidence: ObjectHash = serde_json::from_value(
            service
                .work_update(evidence("second finding", "defaults-evidence-2"), at(3))
                .expect("second evidence")
                .receipt
                .result,
        )
        .expect("evidence hash");

        // Omitted evidence snapshots everything already on the run.
        let checkpoint: ObjectHash = serde_json::from_value(
            service
                .work_update(
                    WorkUpdateInput::Checkpoint {
                        summary: "progress".into(),
                        evidence: None,
                        idempotency_key: "defaults-checkpoint".into(),
                    },
                    at(4),
                )
                .expect("checkpoint")
                .receipt
                .result,
        )
        .expect("checkpoint hash");
        let stored_checkpoint = SqliteStore::open(&database)
            .expect("store")
            .get::<WorkCheckpoint>(&checkpoint)
            .expect("read checkpoint")
            .expect("canonical checkpoint");
        let mut expected = vec![first_evidence.clone(), second_evidence.clone()];
        expected.sort_by(|left, right| left.as_str().cmp(right.as_str()));
        assert_eq!(stored_checkpoint.evidence, expected);

        // Explicit empty still acknowledges none.
        let empty: ObjectHash = serde_json::from_value(
            service
                .work_update(
                    WorkUpdateInput::Checkpoint {
                        summary: "explicitly none".into(),
                        evidence: Some(Vec::new()),
                        idempotency_key: "defaults-checkpoint-empty".into(),
                    },
                    at(5),
                )
                .expect("empty checkpoint")
                .receipt
                .result,
        )
        .expect("checkpoint hash");
        assert!(
            SqliteStore::open(&database)
                .expect("store")
                .get::<WorkCheckpoint>(&empty)
                .expect("read checkpoint")
                .expect("canonical checkpoint")
                .evidence
                .is_empty()
        );

        // Omitted acceptance asserts every criterion with the server note.
        let completed = service
            .work_complete(
                WorkCompleteInput {
                    capture: Some(WorkCompletionCaptureInput {
                        summary: "delivered".into(),
                        refs: Vec::new(),
                    }),
                    evidence: Vec::new(),
                    acceptance: None,
                    note: None,
                    idempotency_key: "defaults-complete".into(),
                },
                at(6),
            )
            .expect("complete");
        let WorkCompleteResult::Completed(receipt) = completed else {
            panic!("completion must seal");
        };
        let seal = SqliteStore::open(&database)
            .expect("store")
            .get::<CompletionSeal>(&receipt.seal)
            .expect("read seal")
            .expect("canonical seal");
        assert_eq!(
            seal.acceptance
                .iter()
                .map(|result| (
                    result.criterion.as_str(),
                    result.satisfied,
                    result.note.as_str()
                ))
                .collect::<Vec<_>>(),
            vec![
                ("first criterion", true, "accepted by agent via work done"),
                ("second criterion", true, "accepted by agent via work done"),
            ]
        );
    }

    #[test]
    fn explicit_empty_acceptance_still_fails_and_note_needs_omitted_acceptance() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("strict-acceptance".into());
        let service = LocalWorkService::new(
            database,
            project,
            "agent".into(),
            SessionId("strict-acceptance-session".into()),
            Some("protocol-test".into()),
        );
        service
            .work_propose(root_input("Strict acceptance", "strict-root"), at(0))
            .expect("root");
        service
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(300),
                    recovery_reason: None,
                    idempotency_key: "strict-claim".into(),
                },
                at(1),
            )
            .expect("claim");
        let complete =
            |acceptance: Option<Vec<WorkAcceptanceInput>>, note: Option<&str>, key: &str| {
                WorkCompleteInput {
                    capture: Some(WorkCompletionCaptureInput {
                        summary: "delivered".into(),
                        refs: Vec::new(),
                    }),
                    evidence: Vec::new(),
                    acceptance,
                    note: note.map(str::to_owned),
                    idempotency_key: key.into(),
                }
            };
        let refused = service
            .work_complete(complete(Some(Vec::new()), None, "strict-empty"), at(2))
            .expect("missing acceptance is a typed protocol refusal");
        let WorkCompleteResult::Refused(refusal) = refused else {
            panic!("explicit empty acceptance must not complete work with criteria");
        };
        assert_eq!(refusal.code, "missing_acceptance");
        let recovery = refusal.recovery;
        assert!(matches!(
            recovery.cause,
            WorkCompletionRecoveryCause::MissingAcceptance { ref criterion }
                if criterion == "Strict acceptance accepted"
        ));
        assert_eq!(recovery.item.title, "Strict acceptance");
        assert!(recovery.command.starts_with("engram work done "));
        assert!(matches!(
            service.work_complete(
                complete(
                    Some(vec![WorkAcceptanceInput {
                        criterion: None,
                        satisfied: true,
                        evidence: Vec::new(),
                        note: "explicit".into(),
                    }]),
                    Some("stray note"),
                    "strict-note-conflict",
                ),
                at(3),
            ),
            Err(StoreError::InvalidWork(_))
        ));
        let WorkCompleteResult::Completed(_) = service
            .work_complete(complete(None, Some("reviewed by hand"), "strict-ok"), at(4))
            .expect("omitted acceptance with a note completes")
        else {
            panic!("completion must seal");
        };
    }

    #[test]
    fn completion_on_a_lapsed_claim_refuses_without_retaking() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("completion-lapsed-claim".into());
        let service = LocalWorkService::new(
            database,
            project,
            "agent".into(),
            SessionId("completion-lapsed-session".into()),
            Some("protocol-test".into()),
        );
        let work = match service
            .work_propose(
                root_input("Lapsed completion", "lapsed-completion-root"),
                at(0),
            )
            .expect("root")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        service
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(1),
                    recovery_reason: None,
                    idempotency_key: "lapsed-completion-claim".into(),
                },
                at(1),
            )
            .expect("claim");

        let input = WorkCompleteInput {
            capture: Some(WorkCompletionCaptureInput {
                summary: "delivered".into(),
                refs: Vec::new(),
            }),
            evidence: Vec::new(),
            acceptance: None,
            note: None,
            idempotency_key: "lapsed-completion".into(),
        };
        let store = service.store().expect("store before refusal");
        let claim = store
            .current_work_claim(work.work_id)
            .expect("claim projection")
            .expect("lapsed claim");
        let event_count = store
            .work_event_tail(work.work_id, 64)
            .expect("events")
            .len();
        drop(store);

        assert!(matches!(
            service.work_complete(input, at(3)),
            Err(StoreError::WorkClaimLapsed { work: refused, .. }) if refused == work.work_id
        ));
        let store = service.store().expect("store after refusal");
        assert_eq!(
            store
                .current_work_claim(work.work_id)
                .expect("claim projection after refusal"),
            Some(claim),
            "a lapsed completion refusal must not renew or retake the claim"
        );
        assert_eq!(
            store
                .work_event_tail(work.work_id, 64)
                .expect("events after refusal")
                .len(),
            event_count,
            "a lapsed completion refusal must not append a claim event"
        );
    }

    #[test]
    fn completed_explicit_update_replays_after_expiry_without_retaking() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("completed-update-after-expiry".into());
        let service = LocalWorkService::new(
            database,
            project,
            "agent".into(),
            SessionId("completed-update-session".into()),
            Some("protocol-test".into()),
        );
        let work = match service
            .work_propose(
                root_input("Completed update replay", "completed-update-root"),
                at(0),
            )
            .expect("root")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        service
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(60),
                    recovery_reason: None,
                    idempotency_key: "completed-update-claim".into(),
                },
                at(1),
            )
            .expect("claim");
        let input = WorkUpdateInput::Checkpoint {
            summary: "checkpoint once".into(),
            evidence: Some(Vec::new()),
            idempotency_key: "completed-update-key".into(),
        };
        let completed = service
            .work_update(input.clone(), at(2))
            .expect("checkpoint");
        let store = service.store().expect("store after checkpoint");
        let claim = store
            .current_work_claim(work.work_id)
            .expect("claim projection")
            .expect("live claim");
        let event_count = store
            .work_event_tail(work.work_id, 64)
            .expect("events")
            .len();
        drop(store);

        let replay = service
            .work_update(input, at(4_000))
            .expect("completed explicit key replays after claim expiry");
        assert_eq!(
            serde_json::to_value(replay).expect("replay JSON"),
            serde_json::to_value(completed).expect("completed JSON")
        );
        let store = service.store().expect("store after replay");
        assert_eq!(
            store
                .current_work_claim(work.work_id)
                .expect("claim projection")
                .expect("retained claim"),
            claim,
            "a completed replay must not advance or renew claim authority"
        );
        assert_eq!(
            store
                .work_event_tail(work.work_id, 64)
                .expect("events after replay")
                .len(),
            event_count,
            "a completed replay must not append a retake event"
        );
    }

    #[test]
    fn outgoing_handoff_expires_no_later_than_its_source_claim() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("lapsed-cancel".into());
        let service = LocalWorkService::new(
            database,
            project,
            "agent".into(),
            SessionId("lapsed-cancel-session".into()),
            Some("protocol-test".into()),
        );
        let work = match service
            .work_propose(root_input("Lapsed cancel", "lapsed-cancel-root"), at(0))
            .expect("root")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        service
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(60),
                    recovery_reason: None,
                    idempotency_key: "lapsed-cancel-claim".into(),
                },
                at(1),
            )
            .expect("claim");
        service
            .work_handoff(
                WorkHandoffInput::Offer {
                    to: "successor".into(),
                    ttl_seconds: Some(7_200),
                    checkpoint_summary: "handoff is bounded by claim expiry".into(),
                    idempotency_key: "lapsed-cancel-offer".into(),
                },
                at(2),
            )
            .expect("offer");
        let store = service.store().expect("store after offer");
        let claim = store
            .current_work_claim(work.work_id)
            .expect("claim projection")
            .expect("offering claim");
        let offer = store
            .work_handoff_offers(work.work_id)
            .expect("handoffs")
            .into_iter()
            .find(|offer| offer.state == WorkHandoffState::Offered)
            .expect("stored offer");
        assert_eq!(
            offer.expires_at, claim.expires_at,
            "an outgoing offer cannot outlive its source claim"
        );
        let event_count = store
            .work_event_tail(work.work_id, 64)
            .expect("events")
            .len();
        drop(store);

        let focus = service
            .work_focus(&work.short_ref, at(4_000))
            .expect("focus after claim and offer expiry");
        assert!(!focus.allowed_next.contains(&"work_handoff:cancel".into()));
        let refused = service
            .work_handoff(
                WorkHandoffInput::Cancel {
                    reason: "cancel after lapse".into(),
                    idempotency_key: "lapsed-cancel-attempt".into(),
                },
                at(4_000),
            )
            .expect_err("expired offer is not cancellable");
        assert!(matches!(
            &refused,
            StoreError::InvalidWork(reason)
                if reason == "ambient work has no live outgoing handoff offer"
        ));
        let store = service.store().expect("store after refusal");
        assert_eq!(
            store
                .current_work_claim(work.work_id)
                .expect("claim projection")
                .expect("retained claim"),
            claim
        );
        assert_eq!(
            store
                .work_event_tail(work.work_id, 64)
                .expect("events after refusal")
                .len(),
            event_count,
            "cancel refusal must not append a retake event"
        );
    }

    fn assert_lapsed_completion_refuses_without_mutation(
        service: &LocalWorkService,
        work: &WorkItemSummary,
        input: &WorkCompleteInput,
        now: DateTime<Utc>,
    ) {
        let store = service.store().expect("store before refusal");
        let claim = store
            .current_work_claim(work.work_id)
            .expect("claim projection")
            .expect("lapsed claim");
        let events = store
            .work_event_tail(work.work_id, 64)
            .expect("events before refusal")
            .len();
        let evidence = store
            .work_run_evidence(claim.run_id)
            .expect("evidence before refusal");
        drop(store);

        assert!(matches!(
            service.work_complete(input.clone(), now),
            Err(StoreError::WorkClaimLapsed { work: refused, .. }) if refused == work.work_id
        ));
        let store = service.store().expect("store after refusal");
        assert_eq!(
            store
                .current_work_claim(work.work_id)
                .expect("claim after refusal"),
            Some(claim.clone())
        );
        assert_eq!(
            store
                .work_event_tail(work.work_id, 64)
                .expect("events after refusal")
                .len(),
            events
        );
        assert_eq!(
            store
                .work_run_evidence(claim.run_id)
                .expect("evidence after refusal"),
            evidence
        );
    }

    #[test]
    fn lapsed_completion_refuses_before_capture_for_explicit_and_derived_keys() {
        for (case, caller_key) in [("explicit", "lapsed-explicit"), ("derived", "")] {
            let directory = tempdir().expect("temp directory");
            let database = directory.path().join("engram.sqlite3");
            let project = ProjectId(format!("completion-lapsed-{case}"));
            let service = LocalWorkService::new(
                database,
                project,
                "agent".into(),
                SessionId(format!("completion-lapsed-{case}-session")),
                Some("protocol-test".into()),
            );
            let work = match service
                .work_propose(
                    root_input(
                        &format!("Lapsed completion {case}"),
                        &format!("lapsed-completion-{case}-root"),
                    ),
                    at(0),
                )
                .expect("root")
            {
                WorkProposeResult::Root { work, .. } => work,
                WorkProposeResult::Decomposition(_) => panic!("expected root"),
            };
            service
                .work_update(
                    WorkUpdateInput::Claim {
                        ttl_seconds: Some(1),
                        recovery_reason: None,
                        idempotency_key: format!("lapsed-completion-{case}-claim"),
                    },
                    at(1),
                )
                .expect("claim");
            let input = WorkCompleteInput {
                capture: Some(WorkCompletionCaptureInput {
                    summary: "delivered once".into(),
                    refs: Vec::new(),
                }),
                evidence: Vec::new(),
                acceptance: None,
                note: None,
                idempotency_key: caller_key.into(),
            };

            assert_lapsed_completion_refuses_without_mutation(&service, &work, &input, at(3));
        }
    }

    #[test]
    fn completion_recovery_command_uses_full_id_when_target_is_beyond_ambiguity_page() {
        let work_id = WorkId::new();
        let item = WorkReferenceCandidate {
            work_id,
            short_ref: "w-collision".into(),
            title: "Ambiguous recovery target".into(),
            lifecycle: WorkLifecycle::Open,
        };
        let resolution = Err(StoreError::WorkReferenceAmbiguous {
            reference: item.short_ref.clone(),
            candidates: vec![WorkReferenceCandidate {
                work_id: WorkId::new(),
                short_ref: item.short_ref.clone(),
                title: "Earlier target".into(),
                lifecycle: WorkLifecycle::Open,
            }],
            more: 1,
        });

        assert_eq!(
            completion_command_ref_from_resolution(&item, resolution)
                .expect("ambiguous recovery target uses its full id"),
            work_id.0.to_string()
        );
    }

    #[test]
    fn missing_contribution_recovery_names_the_participant_and_root() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("completion-missing-contribution".into());
        let service = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            SessionId("completion-contribution-session".into()),
            Some("protocol-test".into()),
        );
        let work = match service
            .work_propose(
                root_input("Contribution barrier", "contribution-barrier-root"),
                at(0),
            )
            .expect("root")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        let participant = SessionId("missing-participant".into());
        service
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(300),
                    recovery_reason: None,
                    idempotency_key: "contribution-claim".into(),
                },
                at(1),
            )
            .expect("claim root");
        SqliteStore::open(&database)
            .expect("fixture store")
            .add_expected_root_contributor_fixture(work.work_id, &participant, at(2))
            .expect("seed an unaccounted expected participant");
        let completion = service
            .work_complete(
                WorkCompleteInput {
                    capture: Some(WorkCompletionCaptureInput {
                        summary: "root implementation complete".into(),
                        refs: Vec::new(),
                    }),
                    evidence: Vec::new(),
                    acceptance: None,
                    note: None,
                    idempotency_key: "contribution-completion".into(),
                },
                at(3),
            )
            .expect("missing contribution is a typed refusal");
        let WorkCompleteResult::Refused(refusal) = completion else {
            panic!("missing participant must block completion");
        };
        let recovery = refusal.recovery;

        assert_eq!(recovery.item.work_id, work.work_id);
        assert!(matches!(
            recovery.cause,
            WorkCompletionRecoveryCause::MissingContribution { participant: missing }
                if missing == participant
        ));
        assert_eq!(
            recovery.command,
            format!(
                "engram work handoff {} --to {} --summary \"transfer root so the missing participant can contribute\"",
                work.short_ref, participant.0
            )
        );
        let handoff = service
            .work_handoff(
                WorkHandoffInput::Offer {
                    to: participant.0.clone(),
                    ttl_seconds: None,
                    checkpoint_summary: "transfer root so the missing participant can contribute"
                        .into(),
                    idempotency_key: "contribution-recovery-handoff".into(),
                },
                at(4),
            )
            .expect("recovery command maps to a real handoff operation");
        assert_eq!(handoff.operation, "offer");
        let store = SqliteStore::open(&database).expect("inspect offered handoff");
        let offers = store
            .work_handoff_offers(work.work_id)
            .expect("load handoff history");
        assert!(
            offers.iter().any(|offer| {
                offer.state == WorkHandoffState::Offered && offer.to == participant
            })
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one retry chain covers live, cancelled, waived, sealed, and unrelated child states"
    )]
    fn keyless_completion_rechecks_required_children_until_the_parent_seals() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("completion-unsealed-child".into());
        let service = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            SessionId("completion-unsealed-session".into()),
            Some("protocol-test".into()),
        );
        let root = match service
            .work_propose(root_input("Parent barrier", "parent-barrier-root"), at(0))
            .expect("root")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        let decomposition = service
            .work_propose(
                WorkProposeInput::Decompose {
                    children: [
                        ("waived-child", ChildRequirement::Required),
                        ("sealed-child", ChildRequirement::Required),
                        ("optional-sibling", ChildRequirement::Optional),
                    ]
                    .into_iter()
                    .map(|(key, requirement)| WorkChildInput {
                        key: key.into(),
                        title: key.replace('-', " "),
                        outcome: format!("{key} outcome"),
                        acceptance: vec![format!("{key} accepted")],
                        requirement: Some(requirement),
                        kind: None,
                        priority: None,
                        labels: Vec::new(),
                        assigned_to: None,
                        deferred_until: None,
                    })
                    .collect(),
                    prerequisites: Vec::new(),
                    idempotency_key: "parent-decomposition".into(),
                },
                at(1),
            )
            .expect("decompose");
        let WorkProposeResult::Decomposition(decomposition) = decomposition else {
            panic!("expected decomposition");
        };
        let required = decomposition.children[..2].to_vec();
        let sibling = decomposition.children[2].clone();
        service
            .work_focus(&root.work_id.0.to_string(), at(2))
            .expect("refocus parent");
        service
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(300),
                    recovery_reason: None,
                    idempotency_key: "parent-claim".into(),
                },
                at(3),
            )
            .expect("claim parent");

        let parent_completion = completion_input("parent delivered", "");
        let first = service
            .work_complete(parent_completion.clone(), at(4))
            .expect("unsealed child is a typed completion refusal");
        let WorkCompleteResult::Refused(refusal) = first else {
            panic!("required child must block completion");
        };
        assert_eq!(refusal.code, "required_child_unsealed");
        let waived = required
            .iter()
            .find(|child| child.work_id == refusal.recovery.item.work_id)
            .expect("refusal names one required child")
            .clone();
        assert!(matches!(
            &refusal.recovery.cause,
            WorkCompletionRecoveryCause::RequiredChildUnsealed { child }
                if *child == waived.work_id
        ));
        let waived_item = SqliteStore::open(&database)
            .expect("required-child store")
            .get_work_item(waived.work_id)
            .expect("required child");
        assert_eq!(refusal.recovery.item.title, waived_item.title);
        assert_eq!(
            refusal.recovery.command,
            format!("engram work show {}", waived.short_ref)
        );
        let sealed = required
            .into_iter()
            .find(|child| child.work_id != waived.work_id)
            .expect("second required child");

        service
            .work_focus(&waived.short_ref, at(5))
            .expect("focus child to waive");
        service
            .work_update(
                WorkUpdateInput::Cancel {
                    reason: "the child outcome is no longer required".into(),
                    idempotency_key: "cancel-required-child".into(),
                },
                at(6),
            )
            .expect("cancel required child");
        service
            .work_focus(&root.short_ref, at(7))
            .expect("refocus parent after cancellation");
        let verbs = AgentVerbs::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            SessionId("completion-unsealed-session".into()),
            Some("protocol-test".into()),
        );
        let cancelled = verbs
            .done(
                DoneInput {
                    work_ref: Some(root.short_ref.clone()),
                    summary: Some("parent delivered".into()),
                    note: None,
                },
                at(8),
            )
            .expect("completion refusal is rendered from current state");
        assert!(cancelled.owed);
        let cancelled_text = cancelled.text();
        assert!(cancelled_text.contains(&format!("required child {} \"", waived.short_ref)));
        assert!(cancelled_text.contains("is cancelled without a completion seal or waiver"));
        let recovery_command = cancelled.next.first().expect("recovery command");
        assert_eq!(
            recovery_command,
            &format!(
                "engram work update {} --waive {} --reason \"account for disposed required child\"",
                root.short_ref, waived.short_ref
            )
        );
        verbs
            .update(
                UpdateInput {
                    work_ref: Some(root.short_ref.clone()),
                    action: UpdateAction::WaiveRequiredChild {
                        child: waived.short_ref,
                        reason: "the cancelled child is explicitly accounted for".into(),
                    },
                },
                at(9),
            )
            .expect("waive cancelled child");
        let next = service
            .work_complete(parent_completion.clone(), at(10))
            .expect("completion advances to the remaining child");
        assert!(matches!(
            next,
            WorkCompleteResult::Refused(WorkCompleteRefusal {
                recovery: WorkCompletionRecovery {
                    cause: WorkCompletionRecoveryCause::RequiredChildUnsealed { child },
                    ..
                },
                ..
            }) if child == sealed.work_id
        ));

        let before_sibling_activity = completion_run_feed_head(&service, root.work_id);
        let peer = LocalWorkService::new(
            database,
            project,
            "peer".into(),
            SessionId("completion-sibling-session".into()),
            Some("protocol-test".into()),
        );
        peer.work_focus(&sibling.short_ref, at(11))
            .expect("peer focuses optional sibling");
        peer.work_update(
            WorkUpdateInput::Cancel {
                reason: "optional sibling activity".into(),
                idempotency_key: "cancel-optional-sibling".into(),
            },
            at(12),
        )
        .expect("peer changes optional sibling");
        assert_eq!(
            completion_run_feed_head(&service, root.work_id),
            before_sibling_activity,
            "optional sibling activity does not advance the parent run feed"
        );
        service
            .work_focus(&sealed.short_ref, at(13))
            .expect("focus remaining required child");
        service
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(300),
                    recovery_reason: None,
                    idempotency_key: "claim-required-child".into(),
                },
                at(14),
            )
            .expect("claim remaining required child");
        assert!(matches!(
            service
                .work_complete(completion_input("required child delivered", ""), at(15))
                .expect("seal required child"),
            WorkCompleteResult::Completed(_)
        ));
        service
            .work_focus(&root.short_ref, at(16))
            .expect("refocus parent after child seal");
        assert!(matches!(
            service
                .work_complete(parent_completion, at(17))
                .expect("parent completion rechecks current barriers"),
            WorkCompleteResult::Completed(_)
        ));
    }

    #[test]
    fn explicit_completion_target_is_checked_before_replay() {
        let directory = tempdir().expect("temp directory");
        let service = LocalWorkService::new(
            directory.path().join("engram.sqlite3"),
            ProjectId("completion-replay-target".into()),
            "agent".into(),
            SessionId("completion-replay-target-session".into()),
            Some("protocol-test".into()),
        );
        let first = proposed_root(
            service
                .work_propose(root_input("First target", "first-target"), at(0))
                .expect("first root"),
        );
        service
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(300),
                    recovery_reason: None,
                    idempotency_key: "claim-first-target".into(),
                },
                at(1),
            )
            .expect("claim first target");
        let input = completion_input("shared completion intent", "shared-completion-key");
        assert!(matches!(
            service
                .work_complete_on(Some(&first.short_ref), input.clone(), at(2))
                .expect("complete first target"),
            WorkCompleteResult::Completed(_)
        ));
        let second = proposed_root(
            service
                .work_propose(root_input("Second target", "second-target"), at(3))
                .expect("second root"),
        );
        assert!(matches!(
            service.work_complete_on(Some(&second.short_ref), input, at(4)),
            Err(StoreError::WorkOperationIdempotencyConflict { operation, key })
                if operation == "work_complete" && key == "shared-completion-key"
        ));
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one scenario proves target binding and same-holder claim-epoch recovery"
    )]
    fn refused_explicit_completion_stays_target_bound_and_rotates_with_holder_claim_epoch() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("completion-refusal-target-binding".into());
        let session = SessionId("completion-refusal-session".into());
        let service = LocalWorkService::new(
            database.clone(),
            project,
            "agent".into(),
            session,
            Some("protocol-test".into()),
        );
        let parent = proposed_root(
            service
                .work_propose(
                    root_input("Refused completion target", "refusal-target-root"),
                    at(0),
                )
                .expect("parent root"),
        );
        service
            .work_propose(
                WorkProposeInput::Decompose {
                    children: vec![WorkChildInput {
                        key: "required-child".into(),
                        title: "Required child".into(),
                        outcome: "Required child outcome".into(),
                        acceptance: vec!["Required child accepted".into()],
                        requirement: Some(ChildRequirement::Required),
                        kind: None,
                        priority: None,
                        labels: Vec::new(),
                        assigned_to: None,
                        deferred_until: None,
                    }],
                    prerequisites: Vec::new(),
                    idempotency_key: "refusal-target-decomposition".into(),
                },
                at(1),
            )
            .expect("required child");
        service
            .work_focus(&parent.short_ref, at(2))
            .expect("focus parent");
        service
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(300),
                    recovery_reason: None,
                    idempotency_key: "refusal-target-claim".into(),
                },
                at(3),
            )
            .expect("claim parent");
        let input = completion_input("parent completion capture", "refusal-target-completion");
        assert!(matches!(
            service
                .work_complete(input.clone(), at(4))
                .expect("required child refusal"),
            WorkCompleteResult::Refused(WorkCompleteRefusal {
                recovery: WorkCompletionRecovery {
                    cause: WorkCompletionRecoveryCause::RequiredChildUnsealed { .. },
                    ..
                },
                ..
            })
        ));

        let other = proposed_root(
            service
                .work_propose(root_input("Other focus", "refusal-other-root"), at(5))
                .expect("other root"),
        );
        assert!(matches!(
            service.work_complete(input.clone(), at(6)),
            Err(StoreError::WorkOperationIdempotencyConflict { operation, key })
                if operation == "work_complete" && key == "refusal-target-completion"
        ));
        service
            .work_focus(&parent.short_ref, at(7))
            .expect("restore refused target");
        service
            .work_update(
                WorkUpdateInput::Release {
                    reason: "rotate the holder claim epoch".into(),
                    waiver_reason: None,
                    idempotency_key: "release-refused-target".into(),
                },
                at(8),
            )
            .expect("release original claim");
        service
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(300),
                    recovery_reason: None,
                    idempotency_key: "reclaim-refused-target".into(),
                },
                at(9),
            )
            .expect("reclaim target");
        assert!(matches!(
            service
                .work_complete(input, at(10))
                .expect("same caller key advances under the new holder claim epoch"),
            WorkCompleteResult::Refused(WorkCompleteRefusal {
                recovery: WorkCompletionRecovery {
                    cause: WorkCompletionRecoveryCause::RequiredChildUnsealed { .. },
                    ..
                },
                ..
            })
        ));
        assert_ne!(other.work_id, parent.work_id);

        let store = SqliteStore::open(&database).expect("refusal binding store");
        assert!(
            store
                .verify_all()
                .expect("refusal binding integrity")
                .is_healthy()
        );
    }

    #[test]
    fn refused_explicit_completion_cannot_refresh_across_work_revision() {
        let directory = tempdir().expect("temp directory");
        let service = LocalWorkService::new(
            directory.path().join("engram.sqlite3"),
            ProjectId("completion-refusal-revision-binding".into()),
            "agent".into(),
            SessionId("completion-refusal-revision-session".into()),
            Some("protocol-test".into()),
        );
        let parent = proposed_root(
            service
                .work_propose(
                    root_input("Revision-bound completion", "revision-bound-root"),
                    at(0),
                )
                .expect("parent root"),
        );
        service
            .work_propose(
                WorkProposeInput::Decompose {
                    children: vec![WorkChildInput {
                        key: "required-child".into(),
                        title: "Required child".into(),
                        outcome: "Required child outcome".into(),
                        acceptance: vec!["Required child accepted".into()],
                        requirement: Some(ChildRequirement::Required),
                        kind: None,
                        priority: None,
                        labels: Vec::new(),
                        assigned_to: None,
                        deferred_until: None,
                    }],
                    prerequisites: Vec::new(),
                    idempotency_key: "revision-bound-decomposition".into(),
                },
                at(1),
            )
            .expect("required child");
        service
            .work_focus(&parent.short_ref, at(2))
            .expect("focus parent");
        service
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(300),
                    recovery_reason: None,
                    idempotency_key: "revision-bound-claim".into(),
                },
                at(3),
            )
            .expect("claim parent");
        let input = completion_input(
            "completion against the original acceptance",
            "revision-bound-completion",
        );
        assert!(matches!(
            service
                .work_complete(input.clone(), at(4))
                .expect("required child refusal"),
            WorkCompleteResult::Refused(WorkCompleteRefusal {
                recovery: WorkCompletionRecovery {
                    cause: WorkCompletionRecoveryCause::RequiredChildUnsealed { .. },
                    ..
                },
                ..
            })
        ));

        service
            .work_update(
                WorkUpdateInput::Revise {
                    patch: WorkRevisionPatch {
                        acceptance: Some(vec!["Revised acceptance must be assessed anew".into()]),
                        ..WorkRevisionPatch::default()
                    },
                    idempotency_key: "revise-after-completion-refusal".into(),
                },
                at(5),
            )
            .expect("revise refused target");

        assert!(matches!(
            service.work_complete(input, at(6)),
            Err(StoreError::WorkOperationIdempotencyConflict { operation, key })
                if operation == "work_complete" && key == "revision-bound-completion"
        ));
        let store = service.store().expect("revision-bound store");
        let revised = store
            .get_work_item(parent.work_id)
            .expect("revised parent projection");
        assert_eq!(
            revised.acceptance,
            vec!["Revised acceptance must be assessed anew"]
        );
        assert_eq!(revised.lifecycle, WorkLifecycle::Open);
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
        let session = SessionId("shared-session".into());
        let service = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            session.clone(),
            Some("protocol-test".into()),
        );
        let peer = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            SessionId("seed-peer".into()),
            Some("protocol-test".into()),
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
        let session = SessionId("focus-race-session".into());
        let service = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            session.clone(),
            Some("protocol-test".into()),
        );
        let peer = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            SessionId("focus-race-peer".into()),
            Some("protocol-test".into()),
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
        service
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: None,
                    recovery_reason: None,
                    idempotency_key: "focus-race-original-claim".into(),
                },
                at(1),
            )
            .expect("claim original focus");
        peer.work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: None,
                recovery_reason: None,
                idempotency_key: "focus-race-replacement-claim".into(),
            },
            at(1),
        )
        .expect("claim replacement focus");
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
        let service = LocalWorkService::new(
            database.clone(),
            project,
            "agent".into(),
            SessionId("completion-prevalidation-session".into()),
            Some("protocol-test".into()),
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
                acceptance: Some(acceptance),
                note: None,
                idempotency_key: key.clone(),
            };
            let completion = service.work_complete(
                input.clone(),
                at(2 + i64::try_from(index).expect("bounded case index")),
            );
            if matches!(name, "missing" | "unknown" | "unsatisfied") {
                let WorkCompleteResult::Refused(refusal) =
                    completion.expect("missing acceptance is a typed refusal")
                else {
                    panic!("{name} acceptance must not complete work");
                };
                assert_eq!(refusal.code, "missing_acceptance");
                assert!(matches!(
                    refusal.recovery.cause,
                    WorkCompletionRecoveryCause::MissingAcceptance { .. }
                ));
            } else {
                assert!(completion.is_err(), "{name} must be rejected");
            }

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
        for (scenario, checkpoint_committed, claim_ttl, retry_at) in [
            ("evidence", false, 300, at(3)),
            ("checkpoint", true, 300, at(3)),
            ("short-claim-renewed", false, 2, at(4)),
        ] {
            let directory = tempdir().expect("temp directory");
            let database = directory.path().join("engram.sqlite3");
            let project = ProjectId(format!("completion-replay-{scenario}"));
            let session = SessionId("completion-session".into());
            let service = LocalWorkService::new(
                database.clone(),
                project.clone(),
                "agent".into(),
                session.clone(),
                Some("protocol-test".into()),
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
                        ttl_seconds: Some(claim_ttl),
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
                acceptance: Some(vec![WorkAcceptanceInput {
                    criterion: None,
                    satisfied: true,
                    evidence: Vec::new(),
                    note: "the crash-replay path was verified".into(),
                }]),
                note: None,
                idempotency_key: "crash-safe-completion".into(),
            };

            let mut store = SqliteStore::open(&database).expect("store");
            let basis = service
                .protocol_basis(&store, true, false, None, at(2))
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
                                &completion_capture_key(&input.idempotency_key, &work, &claim)
                                    .expect("completion capture key"),
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
                    .checkpoint_work_for_completion(
                        &CheckpointWorkRequest {
                            work_id: work.work_id,
                            run_id: claim.run_id,
                            expected_work_revision: work.revision,
                            holder: session.clone(),
                            claim_id: claim.claim_id,
                            claim_fence: claim.fence,
                            summary: capture.summary.clone(),
                            evidence: Some(vec![evidence]),
                            actor: service.actor(
                                "work_complete",
                                "checkpoint the exact completion evidence cut",
                            ),
                            idempotency_key: input.idempotency_key.clone(),
                            checkpointed_at: at(2),
                        },
                        |cut| {
                            let attempt_key = completion_attempt_key(&input.idempotency_key, cut)?;
                            service.core_operation_key(
                                "work_complete",
                                &attempt_key,
                                "checkpoint_work",
                            )
                        },
                        &DevelopmentNoopRedactor,
                    )
                    .expect("committed checkpoint substep");
            }
            let checkpoint_count_before_retry = store
                .work_feed_after(&FeedId::RunExecution(claim.run_id), 0, 100)
                .expect("run feed before retry")
                .into_iter()
                .filter(|entry| entry.object_kind == "work_checkpoint")
                .count();
            drop(store);

            let completed = service
                .work_complete(input.clone(), retry_at)
                .expect("retry resumes the durable attempt");
            let WorkCompleteResult::Completed(completed) = completed else {
                panic!("retry must complete work");
            };
            assert_eq!(completed.work_id, root.work_id);
            assert_eq!(completed.completed_at, retry_at);
            let checkpoint_count_after_retry = SqliteStore::open(&database)
                .expect("store after retry")
                .work_feed_after(&FeedId::RunExecution(claim.run_id), 0, 100)
                .expect("run feed after retry")
                .into_iter()
                .filter(|entry| entry.object_kind == "work_checkpoint")
                .count();
            assert_eq!(
                checkpoint_count_after_retry, 1,
                "a retry writes or reuses exactly one completion checkpoint"
            );
            assert_eq!(
                checkpoint_count_before_retry,
                usize::from(checkpoint_committed)
            );
            let replay = service
                .work_complete(input.clone(), retry_at + Duration::seconds(1))
                .expect("completed outer attempt replays");
            let WorkCompleteResult::Completed(replay) = replay else {
                panic!("completed outer attempt must replay completion");
            };
            assert_eq!(replay.seal, completed.seal);
            assert_eq!(replay.completed_at, retry_at);
        }
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one interrupted-success fixture covers focus changes and later work generations"
    )]
    fn interrupted_completion_replays_the_original_work_and_run() {
        for scenario in ["focus-change", "reopen", "recomplete"] {
            let directory = tempdir().expect("temp directory");
            let database = directory.path().join("engram.sqlite3");
            let project = ProjectId(format!("interrupted-completion-{scenario}"));
            let session = SessionId("interrupted-completion-session".into());
            let service = LocalWorkService::new(
                database.clone(),
                project.clone(),
                "agent".into(),
                session,
                Some("protocol-test".into()),
            );
            let original = proposed_root(
                service
                    .work_propose(
                        root_input("Original completion target", "original-target"),
                        at(0),
                    )
                    .expect("original root"),
            );
            service
                .work_update(
                    WorkUpdateInput::Claim {
                        ttl_seconds: Some(300),
                        recovery_reason: None,
                        idempotency_key: "original-claim".into(),
                    },
                    at(1),
                )
                .expect("claim original work");
            let input = completion_input("original run completed", "interrupted-completion");
            let original_seal = commit_completion_core_without_finishing(&service, &input, at(2));
            let original_seal_hash = CanonicalObject::freeze(&original_seal)
                .expect("original seal object")
                .hash()
                .clone();

            match scenario {
                "focus-change" => {
                    let peer = LocalWorkService::new(
                        database.clone(),
                        project.clone(),
                        "peer".into(),
                        SessionId("interrupted-completion-peer".into()),
                        Some("protocol-test".into()),
                    );
                    let other = proposed_root(
                        peer.work_propose(
                            root_input("Other completed work", "other-target"),
                            at(3),
                        )
                        .expect("other root"),
                    );
                    peer.work_update(
                        WorkUpdateInput::Claim {
                            ttl_seconds: Some(300),
                            recovery_reason: None,
                            idempotency_key: "other-claim".into(),
                        },
                        at(4),
                    )
                    .expect("claim other work");
                    assert!(matches!(
                        peer.work_complete(
                            completion_input("other work completed", "other-completion"),
                            at(5)
                        )
                        .expect("complete other work"),
                        WorkCompleteResult::Completed(_)
                    ));
                    service
                        .work_focus(&other.short_ref, at(6))
                        .expect("move focus to other completed work");
                    assert!(matches!(
                        service.work_complete(input.clone(), at(7)),
                        Err(StoreError::WorkOperationIdempotencyConflict { .. })
                    ));
                    service
                        .work_focus(&original.short_ref, at(8))
                        .expect("restore original focus");
                }
                "reopen" => {
                    service
                        .work_update(
                            WorkUpdateInput::Reopen {
                                reason: "exercise interrupted replay after reopen".into(),
                                idempotency_key: "reopen-original".into(),
                            },
                            at(3),
                        )
                        .expect("reopen original work");
                }
                "recomplete" => {
                    service
                        .work_update(
                            WorkUpdateInput::Reopen {
                                reason: "exercise interrupted replay after a later generation"
                                    .into(),
                                idempotency_key: "reopen-original".into(),
                            },
                            at(3),
                        )
                        .expect("reopen original work");
                    service
                        .work_update(
                            WorkUpdateInput::Claim {
                                ttl_seconds: Some(300),
                                recovery_reason: None,
                                idempotency_key: "later-generation-claim".into(),
                            },
                            at(4),
                        )
                        .expect("claim later generation");
                    let later = service
                        .work_complete(
                            completion_input("later generation completed", "later-completion"),
                            at(5),
                        )
                        .expect("complete later generation");
                    let WorkCompleteResult::Completed(later) = later else {
                        panic!("later generation must complete");
                    };
                    assert_ne!(later.run_id, original_seal.run_id);
                    assert_ne!(later.seal, original_seal_hash);
                }
                _ => unreachable!("fixture scenario is exhaustive"),
            }

            let replay = service
                .work_complete(input.clone(), at(20))
                .expect("recover interrupted completion");
            let WorkCompleteResult::Completed(replay) = replay else {
                panic!("interrupted success must replay");
            };
            assert_eq!(replay.work_id, original.work_id);
            assert_eq!(replay.run_id, original_seal.run_id);
            assert_eq!(replay.seal, original_seal_hash);
            assert_eq!(replay.completed_at, original_seal.completed_at);
            let second = service
                .work_complete(input, at(21))
                .expect("finished interrupted replay is stable");
            let WorkCompleteResult::Completed(second) = second else {
                panic!("finished outer attempt must replay");
            };
            assert_eq!(second.seal, original_seal_hash);
        }
    }

    #[test]
    fn pending_completion_resumes_after_holder_evidence_and_seals_the_current_set() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("completion-retry-current-evidence".into());
        let session = SessionId("completion-current-evidence-session".into());
        let service = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            session.clone(),
            Some("protocol-test".into()),
        );
        let root = match service
            .work_propose(
                root_input("Seal current evidence", "current-evidence-root"),
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
                    ttl_seconds: None,
                    recovery_reason: None,
                    idempotency_key: "current-evidence-claim".into(),
                },
                at(1),
            )
            .expect("claim focused work");
        let input = WorkCompleteInput {
            capture: Some(WorkCompletionCaptureInput {
                summary: "completion checkpoint includes current evidence".into(),
                refs: vec!["test:current-evidence-completion".into()],
            }),
            evidence: Vec::new(),
            acceptance: Some(vec![WorkAcceptanceInput {
                criterion: None,
                satisfied: true,
                evidence: Vec::new(),
                note: "the current evidence set is sealed".into(),
            }]),
            note: None,
            idempotency_key: "current-evidence-completion".into(),
        };

        let mut store = SqliteStore::open(&database).expect("store");
        let basis = service
            .protocol_basis(&store, true, false, None, at(2))
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
            .expect("pending completion attempt");
        let claim = basis.claim.as_ref().expect("completion claim");
        let unrelated = store
            .record_work_evidence(
                &RecordWorkEvidenceRequest {
                    work_id: root.work_id,
                    run_id: claim.run_id,
                    expected_work_revision: root.revision,
                    holder: session.clone(),
                    claim_id: claim.claim_id,
                    claim_fence: claim.fence,
                    summary: "independent holder evidence committed after attempt start".into(),
                    refs: vec!["test:independent-evidence".into()],
                    actor: service.actor("work_update", "record independent holder evidence"),
                    idempotency_key: "independent-holder-evidence".into(),
                    recorded_at: at(3),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("independent evidence");
        drop(store);

        let completed = service
            .work_complete(input, at(4))
            .expect("same-key retry resumes against current evidence");
        let WorkCompleteResult::Completed(receipt) = completed else {
            panic!("completion must seal");
        };
        let store = SqliteStore::open(&database).expect("sealed store");
        let seal: CompletionSeal = store
            .get(&receipt.seal)
            .expect("load seal")
            .expect("canonical seal");
        assert!(seal.evidence.contains(&unrelated));
        assert!(store.verify_all().expect("integrity").is_healthy());
    }

    #[test]
    fn pending_completion_conflicts_after_foreign_claim_fence_change() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("completion-retry-foreign-fence".into());
        let session = SessionId("completion-original-session".into());
        let service = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            session.clone(),
            Some("protocol-test".into()),
        );
        let root = match service
            .work_propose(
                root_input("Reject foreign retry", "foreign-fence-root"),
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
                    ttl_seconds: Some(2),
                    recovery_reason: None,
                    idempotency_key: "foreign-fence-original-claim".into(),
                },
                at(1),
            )
            .expect("short original claim");
        let input = WorkCompleteInput {
            capture: Some(WorkCompletionCaptureInput {
                summary: "this stale attempt must never commit".into(),
                refs: Vec::new(),
            }),
            evidence: Vec::new(),
            acceptance: Some(vec![WorkAcceptanceInput {
                criterion: None,
                satisfied: true,
                evidence: Vec::new(),
                note: "stale completion".into(),
            }]),
            note: None,
            idempotency_key: "foreign-fence-completion".into(),
        };

        let mut store = SqliteStore::open(&database).expect("store");
        let basis = service
            .protocol_basis(&store, true, false, None, at(2))
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
            .expect("pending completion attempt");
        let run_id = root.active_run_id.expect("active run");
        let peer = SessionId("completion-peer-session".into());
        let mut peer_actor = service.actor("work_update", "recover expired foreign claim");
        peer_actor.session_id = Some(peer.clone());
        let recovered = store
            .claim_work(
                &ClaimWorkRequest {
                    work_id: root.work_id,
                    expected_work_revision: root.revision,
                    expected_run_id: run_id,
                    holder: peer,
                    ttl_seconds: 300,
                    recovery_reason: Some("the original holder claim expired".into()),
                    actor: peer_actor,
                    idempotency_key: "foreign-fence-recovery".into(),
                    claimed_at: at(4),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("peer recovers claim");
        assert_ne!(
            recovered.fence,
            basis.claim.as_ref().expect("old claim").fence
        );
        drop(store);

        assert!(matches!(
            service.work_complete(input, at(5)),
            Err(StoreError::WorkOperationIdempotencyConflict { operation, key })
                if operation == "work_complete" && key == "foreign-fence-completion"
        ));
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
        let session = SessionId("contradiction-session".into());
        let service = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            session.clone(),
            Some("protocol-test".into()),
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
        service
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: None,
                    recovery_reason: None,
                    idempotency_key: "contradiction-claim".into(),
                },
                at(1),
            )
            .expect("claim contradiction work");
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
                &DevelopmentNoopRedactor,
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
                &DevelopmentNoopRedactor,
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
                &DevelopmentNoopRedactor,
            )
            .expect("work and task contradiction");
        assert!(!contradiction.work_positions.is_empty());
        assert!(!project_contradiction.work_positions.is_empty());
        assert!(!task_contradiction.work_positions.is_empty());
        assert!(store.verify_all().expect("integrity report").is_healthy());
        drop(store);

        let mut page = service
            .work_next(100, WorkNextQuery::default(), at(8))
            .expect("deliver contradiction event");
        let expected = [
            contradiction.contradiction,
            project_contradiction.contradiction,
            task_contradiction.contradiction,
        ];
        let mut visible = std::collections::HashSet::new();
        let mut confirmed = 0;
        for offset in 0..8 {
            for change in page.changes.as_deref().unwrap_or_default() {
                if change.entry.object_kind == "memory_contradiction_event"
                    && matches!(change.delivery, WorkChangeProjection::Visible(_))
                {
                    visible.insert(change.entry.object_hash.clone());
                }
            }
            let delivered = page.delivered_through.expect("delivered cursor");
            let delivery_token = page.delivery_token.as_deref().expect("delivery token");
            page = service
                .work_next_with_delivery_token(
                    100,
                    Some(delivered),
                    Some(delivery_token),
                    WorkNextQuery::default(),
                    at(9 + offset),
                )
                .expect("acknowledge contradiction page");
            confirmed = page.session.confirmed_project_cursor;
            if expected.iter().all(|hash| visible.contains(hash)) {
                break;
            }
        }
        assert!(expected.iter().all(|hash| visible.contains(hash)));
        assert!(confirmed > 0);
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
        let focused = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            SessionId("focused-session".into()),
            Some("protocol-test".into()),
        );
        let peer = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            SessionId("peer-session".into()),
            Some("protocol-test".into()),
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
        focused
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: None,
                    recovery_reason: None,
                    idempotency_key: "focused-memory-claim".into(),
                },
                at(1),
            )
            .expect("claim focused root");
        peer.work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: None,
                recovery_reason: None,
                idempotency_key: "peer-memory-claim".into(),
            },
            at(1),
        )
        .expect("claim peer root");
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
                &DevelopmentNoopRedactor,
            )
            .expect("peer-root contradiction");
        let restricted_contradiction = MemoryContradictionEvent {
            schema_version: SCHEMA_VERSION,
            project_id: project.clone(),
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

        let mut page = focused
            .work_next(100, WorkNextQuery::default(), at(8))
            .expect("bounded project delta");
        let expected_hashes = [
            &visible.version,
            &restricted.version,
            &restricted.assertion,
            &outside.version,
            &outside.assertion,
            &peer_contradiction.contradiction,
        ];
        let mut changes = Vec::new();
        let mut final_confirmed = 0;
        for offset in 0..8 {
            let delivered = page.delivered_through.expect("delivered cursor");
            let page_changes = page.changes.as_ref().expect("changes section");
            assert_eq!(
                i64::try_from(page_changes.len()).expect("change count"),
                delivered - page.session.confirmed_project_cursor
            );
            changes.extend(page_changes.iter().cloned());
            let delivery_token = page.delivery_token.as_deref().expect("delivery token");
            page = focused
                .work_next_with_delivery_token(
                    100,
                    Some(delivered),
                    Some(delivery_token),
                    WorkNextQuery::default(),
                    at(9 + offset),
                )
                .expect("acknowledge protected delta page");
            final_confirmed = page.session.confirmed_project_cursor;
            if expected_hashes.iter().all(|hash| {
                changes
                    .iter()
                    .any(|change| &change.entry.object_hash == *hash)
            }) {
                break;
            }
        }
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
        let serialized = serde_json::to_string(&changes).expect("serialize work_next changes");
        assert!(serialized.contains("visible focused-root memory"));
        assert!(!serialized.contains("restricted focused-root secret"));
        assert!(!serialized.contains("unrelated root memory"));
        assert!(!serialized.contains("second unrelated root memory"));
        assert!(
            !serialized.contains("peer-root contradiction must remain outside focused delivery")
        );
        assert!(final_confirmed > 0);
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
        reason = "concurrency, exact replay, and a large unrelated evidence history form one gate-transition regression"
    )]
    fn concurrent_gate_transitions_serialize_and_history_lookup_stays_bounded() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("concurrent-gate".into());
        let session = SessionId("gate-session".into());
        let service = LocalWorkService::new(
            database.clone(),
            project,
            "agent".into(),
            session,
            Some("gate-test".into()),
        );
        let work = match service
            .work_propose(root_input("Concurrent gate", "concurrent-gate-root"), at(0))
            .expect("root")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        service
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(300),
                    recovery_reason: None,
                    idempotency_key: "concurrent-gate-claim".into(),
                },
                at(1),
            )
            .expect("claim");

        let barrier = Arc::new(Barrier::new(3));
        let first_service = service.clone();
        let first_barrier = barrier.clone();
        let first = std::thread::spawn(move || {
            first_barrier.wait();
            first_service.work_gate("cargo-test", &["first".into()], None, at(2))
        });
        let second_service = service.clone();
        let second_barrier = barrier.clone();
        let second = std::thread::spawn(move || {
            second_barrier.wait();
            second_service.work_gate("cargo-test", &["second".into()], None, at(2))
        });
        barrier.wait();
        let first = first.join().expect("first thread").expect("first gate");
        let second = second.join().expect("second thread").expect("second gate");
        let first_hash: ObjectHash =
            serde_json::from_value(first.receipt.result).expect("first evidence hash");
        let second_hash: ObjectHash =
            serde_json::from_value(second.receipt.result).expect("second evidence hash");
        let store = SqliteStore::open(&database).expect("store");
        let first_evidence = store
            .get::<WorkEvidence>(&first_hash)
            .expect("first evidence read")
            .expect("first evidence");
        let second_evidence = store
            .get::<WorkEvidence>(&second_hash)
            .expect("second evidence read")
            .expect("second evidence");
        let (latest_hash, latest_failed) = if first_evidence
            .gate
            .as_ref()
            .and_then(|gate| gate.previous.as_ref())
            == Some(&second_hash)
        {
            (first_hash, vec!["first".into()])
        } else {
            assert_eq!(
                second_evidence
                    .gate
                    .as_ref()
                    .and_then(|gate| gate.previous.as_ref()),
                Some(&first_hash)
            );
            (second_hash, vec!["second".into()])
        };
        drop(store);
        let replay = service
            .work_gate("cargo-test", &latest_failed, None, at(3))
            .expect("replay latest transition");
        assert_eq!(
            serde_json::from_value::<ObjectHash>(replay.receipt.result)
                .expect("replayed evidence hash"),
            latest_hash
        );
        assert_eq!(
            SqliteStore::open(&database)
                .expect("store")
                .work_run_evidence(work.active_run_id.expect("active run"))
                .expect("run evidence")
                .len(),
            2
        );

        for index in 0..64 {
            service
                .work_update(
                    WorkUpdateInput::Evidence {
                        summary: format!("unrelated evidence {index}"),
                        refs: Vec::new(),
                        attach: None,
                        idempotency_key: format!("unrelated-evidence-{index}"),
                    },
                    at(4 + index),
                )
                .expect("unrelated evidence");
        }
        crate::canonical::reset_canonical_decode_count();
        service
            .work_gate("bounded-history", &[], None, at(100))
            .expect("bounded gate lookup");
        assert!(
            crate::canonical::canonical_decode_count() <= 24,
            "gate lookup decoded an unbounded evidence history"
        );
        let lapsed_replay = service
            .work_gate("bounded-history", &[], None, at(4_000))
            .expect("a committed exact gate retry replays after claim lapse");
        assert_eq!(lapsed_replay.operation, "evidence");
    }

    #[test]
    fn explicit_update_target_wins_after_same_session_focus_change() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("explicit-update-target".into());
        let service = LocalWorkService::new(
            database,
            project,
            "agent".into(),
            SessionId("shared-session".into()),
            Some("explicit-target-test".into()),
        );
        let create = |title: &str, key: &str| match service
            .work_propose(root_input(title, key), at(0))
            .expect("root")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        let target = create("Explicit target", "explicit-target");
        let other = create("Concurrent focus", "concurrent-focus");
        let prerequisite = create("Prerequisite", "explicit-prerequisite");

        service
            .work_focus(&target.short_ref, at(1))
            .expect("initial target focus");
        service
            .work_focus(&other.short_ref, at(2))
            .expect("same session changes focus before mutation");
        service
            .work_update_on(
                Some(&target.work_id.0.to_string()),
                WorkUpdateInput::AddPrerequisite {
                    prerequisite: prerequisite.short_ref.clone(),
                    idempotency_key: String::new(),
                },
                at(3),
            )
            .expect("explicit update remains bound to its target");

        let store = SqliteStore::open(&service.database).expect("store");
        assert_eq!(
            store
                .work_prerequisites(target.work_id)
                .expect("target prerequisites")
                .into_iter()
                .map(|item| item.work_id)
                .collect::<Vec<_>>(),
            vec![prerequisite.work_id]
        );
        assert!(
            store
                .work_prerequisites(other.work_id)
                .expect("other prerequisites")
                .is_empty()
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "the regression keeps pending-attempt setup, atomic capture, and recovery assertions together"
    )]
    fn pending_note_attempt_recovers_the_atomic_evidence_checkpoint_pair() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("pending-note-attempt".into());
        let service = LocalWorkService::new(
            database.clone(),
            project,
            "agent".into(),
            SessionId("pending-note-session".into()),
            Some("pending-note-test".into()),
        );
        let work = match service
            .work_propose(root_input("Pending note", "pending-note-root"), at(0))
            .expect("root")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        service
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(300),
                    recovery_reason: None,
                    idempotency_key: "pending-note-claim".into(),
                },
                at(1),
            )
            .expect("claim");
        let summary = "atomic note survives a lost response";
        let refs = vec!["test:pending-note".into()];
        let committed = {
            let mut store = service.store().expect("store");
            let basis = service
                .protocol_basis(&store, true, false, Some(work.work_id), at(2))
                .expect("note basis");
            let note = WorkNoteIntent {
                summary,
                refs: &refs,
            };
            let intent = service.protocol_intent(&note);
            let raw_key = service
                .effective_idempotency_key("", "work_update:note", &basis, &intent, at(2))
                .expect("derived note key");
            store
                .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
                    project_id: &service.project_id,
                    session_id: &service.session_id,
                    operation: "work_update:note",
                    idempotency_key: &raw_key,
                    intent: &intent,
                    basis: &basis,
                    now: at(2),
                })
                .expect("pending note attempt");
            let claim = basis.claim.expect("claim basis");
            let scoped_key = service
                .core_operation_key("work_update:note", &raw_key, "record_work_note")
                .expect("note core key");
            store
                .record_work_note(
                    &RecordWorkNoteRequest {
                        work_id: work.work_id,
                        run_id: claim.run_id,
                        expected_work_revision: work.revision,
                        holder: service.session_id.clone(),
                        claim_id: claim.claim_id,
                        claim_fence: claim.fence,
                        summary: summary.into(),
                        refs: refs.clone(),
                        actor: service.actor(
                            "work_update",
                            "simulate a lost note response after atomic capture",
                        ),
                        idempotency_key: scoped_key,
                        recorded_at: at(2),
                    },
                    &DevelopmentNoopRedactor,
                )
                .expect("atomic note capture")
        };

        let recovered = service
            .work_note_on(Some(&work.work_id.0.to_string()), summary, &refs, at(3))
            .expect("recover pending note");
        assert_eq!(
            recovered.evidence.result,
            serde_json::to_value(&committed.evidence).expect("evidence value")
        );
        assert_eq!(
            recovered.receipt.result,
            serde_json::to_value(&committed.checkpoint).expect("checkpoint value")
        );
        let store = SqliteStore::open(&database).expect("store");
        assert_eq!(
            store
                .work_run_evidence(work.active_run_id.expect("active run"))
                .expect("run evidence"),
            vec![committed.evidence]
        );
        assert_eq!(
            store
                .latest_work_run(work.work_id)
                .expect("run read")
                .expect("run")
                .last_checkpoint,
            Some(committed.checkpoint)
        );
    }

    #[test]
    fn pending_gate_attempt_recovers_without_appending_again() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("pending-gate-attempt".into());
        let service = LocalWorkService::new(
            database.clone(),
            project,
            "agent".into(),
            SessionId("pending-gate-session".into()),
            Some("pending-gate-test".into()),
        );
        let work = match service
            .work_propose(root_input("Pending gate", "pending-gate-root"), at(0))
            .expect("root")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        service
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(300),
                    recovery_reason: None,
                    idempotency_key: "pending-gate-claim".into(),
                },
                at(1),
            )
            .expect("claim");

        let pending = {
            let mut store = service.store().expect("store");
            let basis = service
                .protocol_basis(&store, true, false, Some(work.work_id), at(2))
                .expect("gate basis");
            let claim = basis.claim.clone().expect("claim basis");
            store
                .record_gate_evidence_protocol(
                    &RecordGateEvidenceRequest {
                        work_id: work.work_id,
                        run_id: claim.run_id,
                        expected_work_revision: work.revision,
                        holder: service.session_id.clone(),
                        claim_id: claim.claim_id,
                        claim_fence: claim.fence,
                        name: "cargo-test".into(),
                        failed: vec!["one failure".into()],
                        evidence_ref: None,
                        actor: service
                            .actor("work_update", "record gate evidence for ambient work"),
                        recorded_at: at(2),
                    },
                    &BeginGateWorkProtocolAttempt {
                        project_id: &service.project_id,
                        session_id: &service.session_id,
                        basis: &basis,
                        now: at(2),
                    },
                    &DevelopmentNoopRedactor,
                )
                .expect("atomic gate append")
        };
        assert!(pending.result.is_none());

        let recovered = service
            .work_gate_on(
                Some(&work.work_id.0.to_string()),
                "cargo-test",
                &["one failure".into()],
                None,
                at(3),
            )
            .expect("recover pending gate attempt");
        assert_eq!(
            serde_json::from_value::<ObjectHash>(recovered.receipt.result)
                .expect("recovered evidence hash"),
            pending.evidence
        );
        assert_eq!(
            SqliteStore::open(&database)
                .expect("store")
                .work_run_evidence(work.active_run_id.expect("active run"))
                .expect("run evidence")
                .len(),
            1
        );
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one scenario covers handoff and same-session reclaim gate replay authority"
    )]
    fn identical_gate_after_handoff_or_reclaim_is_a_new_claim_observation() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("handoff-gate-observation".into());
        let first = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            SessionId("gate-holder-a".into()),
            Some("gate-handoff-test".into()),
        );
        let second = LocalWorkService::new(
            database.clone(),
            project,
            "agent".into(),
            SessionId("gate-holder-b".into()),
            Some("gate-handoff-test".into()),
        );
        let work = match first
            .work_propose(root_input("Handoff gate", "handoff-gate-root"), at(0))
            .expect("root")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        first
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(300),
                    recovery_reason: None,
                    idempotency_key: "gate-holder-a-claim".into(),
                },
                at(1),
            )
            .expect("first claim");
        let first_gate = first
            .work_gate("cargo-test", &[], None, at(2))
            .expect("first holder gate");
        let first_hash: ObjectHash =
            serde_json::from_value(first_gate.receipt.result).expect("first gate hash");
        second
            .work_focus(&work.short_ref, at(3))
            .expect("non-holder focus");
        assert!(matches!(
            second
                .work_gate("cargo-test", &[], None, at(3))
                .expect_err("a non-holder cannot reuse the holder's gate observation"),
            StoreError::WorkClaimMismatch { .. }
        ));
        first
            .work_handoff(
                WorkHandoffInput::Offer {
                    to: "gate-holder-b".into(),
                    ttl_seconds: Some(300),
                    checkpoint_summary: "handoff after the first gate observation".into(),
                    idempotency_key: "gate-handoff-offer".into(),
                },
                at(4),
            )
            .expect("offer handoff");
        second
            .work_focus(&work.short_ref, at(5))
            .expect("second holder focus");
        second
            .work_handoff(
                WorkHandoffInput::Accept {
                    idempotency_key: "gate-handoff-accept".into(),
                },
                at(6),
            )
            .expect("accept handoff");
        assert!(matches!(
            first
                .work_gate("cargo-test", &[], None, at(7))
                .expect_err("the outgoing holder cannot replay after handoff acceptance"),
            StoreError::WorkClaimMismatch { .. }
        ));

        let second_gate = second
            .work_gate("cargo-test", &[], None, at(8))
            .expect("second holder records the same result");
        let second_hash: ObjectHash =
            serde_json::from_value(second_gate.receipt.result).expect("second gate hash");
        assert_ne!(second_hash, first_hash);
        let evidence = SqliteStore::open(&database)
            .expect("store")
            .get::<WorkEvidence>(&second_hash)
            .expect("second evidence read")
            .expect("second evidence");
        assert_eq!(
            evidence
                .gate
                .as_ref()
                .and_then(|gate| gate.previous.as_ref()),
            Some(&first_hash)
        );
        assert_eq!(
            evidence.actor.session_id.as_ref(),
            Some(&SessionId("gate-holder-b".into()))
        );
        second
            .work_update(
                WorkUpdateInput::Release {
                    reason: "pause after verification".into(),
                    waiver_reason: None,
                    idempotency_key: "gate-holder-b-release".into(),
                },
                at(9),
            )
            .expect("release second holder claim");
        assert!(matches!(
            second
                .work_gate("cargo-test", &[], None, at(10))
                .expect_err("a released holder cannot replay gate evidence"),
            StoreError::WorkClaimMismatch { .. }
        ));
        second
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(300),
                    recovery_reason: None,
                    idempotency_key: "gate-holder-b-reclaim".into(),
                },
                at(11),
            )
            .expect("same session reclaims the run");
        let reclaimed = second
            .work_gate("cargo-test", &[], None, at(12))
            .expect("same result under a new claim is a fresh observation");
        let reclaimed_hash: ObjectHash =
            serde_json::from_value(reclaimed.receipt.result).expect("reclaimed gate hash");
        assert_ne!(reclaimed_hash, second_hash);
        let reclaimed_evidence = SqliteStore::open(&database)
            .expect("store")
            .get::<WorkEvidence>(&reclaimed_hash)
            .expect("reclaimed evidence read")
            .expect("reclaimed evidence");
        assert_eq!(
            reclaimed_evidence
                .gate
                .as_ref()
                .and_then(|gate| gate.previous.as_ref()),
            Some(&second_hash)
        );
        assert!(reclaimed_evidence.claim_fence > evidence.claim_fence);
    }

    #[test]
    fn explicit_gate_target_wins_after_same_session_focus_change() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("explicit-gate-target".into());
        let service = LocalWorkService::new(
            database.clone(),
            project,
            "agent".into(),
            SessionId("shared-gate-session".into()),
            Some("explicit-gate-test".into()),
        );
        let create = |title: &str, key: &str| match service
            .work_propose(root_input(title, key), at(0))
            .expect("root")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        let target = create("Explicit gate target", "explicit-gate-target");
        let other = create("Concurrent gate focus", "concurrent-gate-focus");
        service
            .work_update_on(
                Some(&target.work_id.0.to_string()),
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(300),
                    recovery_reason: None,
                    idempotency_key: "explicit-gate-claim".into(),
                },
                at(1),
            )
            .expect("claim explicit target");
        service
            .work_focus(&other.short_ref, at(2))
            .expect("same session changes focus before gate");

        let result = service
            .work_gate_on(
                Some(&target.work_id.0.to_string()),
                "cargo-test",
                &[],
                None,
                at(3),
            )
            .expect("explicit gate remains bound to target");
        let evidence_hash: ObjectHash =
            serde_json::from_value(result.receipt.result).expect("evidence hash");
        let evidence = SqliteStore::open(&database)
            .expect("store")
            .get::<WorkEvidence>(&evidence_hash)
            .expect("evidence read")
            .expect("evidence");
        assert_eq!(evidence.work_id, target.work_id);
        assert_ne!(evidence.work_id, other.work_id);
    }

    #[test]
    #[allow(
        clippy::too_many_lines,
        reason = "one boundary test covers normalization, typed projection, and direct-storage refusal"
    )]
    fn gate_storage_owns_normalization_and_bounds() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("gate-storage-boundary".into());
        let service = LocalWorkService::new(
            database,
            project,
            "agent".into(),
            SessionId("gate-storage-session".into()),
            Some("gate-storage-test".into()),
        );
        let work = match service
            .work_propose(root_input("Gate storage", "gate-storage-root"), at(0))
            .expect("root")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        service
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(300),
                    recovery_reason: None,
                    idempotency_key: "gate-storage-claim".into(),
                },
                at(1),
            )
            .expect("claim");
        let claim = service
            .store()
            .expect("store")
            .current_work_claim(work.work_id)
            .expect("claim projection")
            .expect("live claim");
        let request = RecordGateEvidenceRequest {
            work_id: work.work_id,
            run_id: claim.run_id,
            expected_work_revision: work.revision,
            holder: service.session_id.clone(),
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
            name: "  CARGO-TEST  ".into(),
            failed: vec![" suite::b ".into(), "suite::a".into(), "suite::a".into()],
            evidence_ref: Some(" logs/gate.txt ".into()),
            actor: service.actor("work_update", "exercise the gate storage boundary"),
            recorded_at: at(2),
        };
        let evidence_hash = service
            .store()
            .expect("store")
            .record_gate_evidence(&request, &DevelopmentNoopRedactor)
            .expect("normalized gate evidence");
        let evidence = service
            .store()
            .expect("store")
            .get::<WorkEvidence>(&evidence_hash)
            .expect("evidence read")
            .expect("evidence");
        let gate = evidence.gate.expect("typed gate payload");
        assert_eq!(gate.name, "cargo-test");
        assert_eq!(gate.failed, vec!["suite::a", "suite::b"]);
        assert_eq!(evidence.refs, vec!["logs/gate.txt"]);
        assert_eq!(evidence.summary, GATE_EVIDENCE_SUMMARY);

        let mimicking_hash: ObjectHash = serde_json::from_value(
            service
                .work_update(
                    WorkUpdateInput::Evidence {
                        summary: "gate cargo-test failed (2 failures): suite::a, suite::b".into(),
                        refs: Vec::new(),
                        attach: None,
                        idempotency_key: "gate-shaped-note".into(),
                    },
                    at(3),
                )
                .expect("gate-shaped generic evidence")
                .receipt
                .result,
        )
        .expect("generic evidence hash");
        let focus = service
            .inspect_work(&work.short_ref, at(4))
            .expect("projected evidence");
        let projected_gate = focus
            .evidence_items
            .iter()
            .find(|item| item.evidence == evidence_hash)
            .expect("typed gate projection")
            .gate
            .as_ref()
            .expect("typed gate discriminator");
        assert_eq!(projected_gate.name, "cargo-test");
        assert!(!projected_gate.passed);
        assert_eq!(projected_gate.failed_count, 2);
        assert!(
            focus
                .evidence_items
                .iter()
                .find(|item| item.evidence == mimicking_hash)
                .expect("gate-shaped generic projection")
                .gate
                .is_none(),
            "generic prose must not acquire the typed gate discriminator"
        );

        let mut oversized = request;
        oversized.name = "x".repeat(crate::domain::MAX_GATE_NAME_BYTES + 1);
        oversized.recorded_at = at(3);
        assert!(matches!(
            service
                .store()
                .expect("store")
                .record_gate_evidence(&oversized, &DevelopmentNoopRedactor)
                .expect_err("storage rejects oversized gate identity"),
            StoreError::InvalidWork(detail) if detail.contains("gate_input_too_large")
        ));
        oversized.name = "cargo\u{e0020}test".into();
        oversized.recorded_at = at(4);
        assert!(matches!(
            service
                .store()
                .expect("store")
                .record_gate_evidence(&oversized, &DevelopmentNoopRedactor)
                .expect_err("storage rejects invisible gate identity"),
            StoreError::InvalidWork(detail) if detail.contains("control or format")
        ));
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
        let a = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            SessionId("session-a".into()),
            Some("protocol-test".into()),
        );
        let b = LocalWorkService::new(
            database.clone(),
            project,
            "agent".into(),
            SessionId("session-b".into()),
            Some("protocol-test".into()),
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
                    idempotency_key: "concurrent-root".into(),
                },
                at(2),
            )
            .expect("append after another session staged a page")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected concurrent root"),
        };
        // A staged page never blocks a focus change.
        let switched = a
            .work_focus(&concurrent.short_ref, at(3))
            .expect("focus changes while a page is staged");
        assert_eq!(switched.status.work.work_id, concurrent.work_id);
        a.work_focus(&root.short_ref, at(3))
            .expect("focus returns to the root");
        // The focus change discarded the un-delivered page, so the next call
        // recomputes the interval from the confirmed cursor through the
        // concurrent append.
        let second = a
            .work_next(20, WorkNextQuery::default(), at(4))
            .expect("second page after the focus change");
        assert_eq!(second.session.confirmed_project_cursor, 0);
        let second_delivered = second.delivered_through.expect("second delivered cursor");
        assert!(second_delivered > first_delivered);
        assert!(second.session.pending_delivery);
        let second_positions = second
            .changes
            .as_ref()
            .expect("second changes")
            .iter()
            .map(|change| change.entry.position.position)
            .collect::<Vec<_>>();
        assert_eq!(second_positions, (1..=second_delivered).collect::<Vec<_>>());
        assert_ne!(second.delivery_token, Some(first_delivery_token.clone()));
        // A host may still acknowledge explicitly, but only the exact current
        // pair; the discarded page's pair is refused without disclosure.
        let stale = a
            .work_next_with_delivery_token(
                20,
                Some(first_delivered),
                Some(first_delivery_token.as_str()),
                WorkNextQuery::default(),
                at(5),
            )
            .expect_err("a discarded page cannot be acknowledged");
        assert!(!stale.to_string().contains(&first_delivery_token));
        let explicit = a
            .work_next_with_delivery_token(
                20,
                Some(second_delivered),
                second.delivery_token.as_deref(),
                WorkNextQuery::default(),
                at(5),
            )
            .expect("explicit acknowledgement of the current page");
        assert_eq!(explicit.session.confirmed_project_cursor, second_delivered);
        assert!(
            explicit
                .changes
                .as_ref()
                .expect("no new changes")
                .is_empty()
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
        a.work_focus(&concurrent.short_ref, at(7))
            .expect("focus changes freely between calls");
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
                evidence: Some(vec![evidence]),
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
                    acceptance: Some(vec![WorkAcceptanceInput {
                        criterion: None,
                        satisfied: true,
                        evidence: Vec::new(),
                        note: "verified by the receiving session".into(),
                    }]),
                    note: None,
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
        let first = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            SessionId("first-holder".into()),
            Some("protocol-test".into()),
        );
        let successor = LocalWorkService::new(
            database,
            project,
            "agent".into(),
            SessionId("successor".into()),
            Some("protocol-test".into()),
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

        let live_foreign_guidance = successor
            .work_focus(&root.short_ref, at(2))
            .expect("focus while another session holds the live claim");
        assert_eq!(live_foreign_guidance.allowed_next, vec!["work_focus"]);

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
    fn allowed_next_advertises_plain_claim_without_recovery_for_a_ready_lapsed_holder() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("retake-readiness-guidance".into());
        let service = LocalWorkService::new(
            database,
            project,
            "agent".into(),
            SessionId("retake-readiness-session".into()),
            Some("protocol-test".into()),
        );
        let work = match service
            .work_propose(
                root_input(
                    "Retake readiness guidance",
                    "retake-readiness-guidance-root",
                ),
                at(0),
            )
            .expect("root")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        service
            .work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(1),
                    recovery_reason: None,
                    idempotency_key: "retake-readiness-guidance-claim".into(),
                },
                at(1),
            )
            .expect("claim");
        let store = service.store().expect("store");
        let guidance = service
            .work_guidance(&store, work.work_id, at(3))
            .expect("lapsed holder guidance basis");
        let claim = guidance.claim.as_ref().expect("lapsed holder claim");
        let ready = allowed_next(
            &guidance.status,
            AllowedNextContext {
                claim: Some(claim),
                handoffs: &[],
                session: &service.session_id,
                now: at(3),
                can_waive_required_child: false,
                claim_recovery_required: false,
                completion_capture_ready: true,
                completion_preflight_ready: true,
            },
        );
        let without_claim = vec![
            "work_focus",
            "work_propose:decompose",
            "work_update:add_prerequisite",
            "work_update:block",
            "work_update:cancel",
            "work_update:remove_prerequisite",
            "work_update:revise",
            "work_update:supersede",
            "work_update:unblock",
        ];
        let mut with_claim = without_claim.clone();
        with_claim.push("work_update:claim");
        with_claim.sort_unstable();
        assert_eq!(ready, with_claim);
        for availability in [
            WorkAvailability::Blocked,
            WorkAvailability::Deferred,
            WorkAvailability::Waiting,
        ] {
            let mut status = guidance.status.clone();
            status.availability = availability;
            for claim in [Some(claim), None] {
                let next = allowed_next(
                    &status,
                    AllowedNextContext {
                        claim,
                        handoffs: &[],
                        session: &service.session_id,
                        now: at(3),
                        can_waive_required_child: false,
                        claim_recovery_required: false,
                        completion_capture_ready: true,
                        completion_preflight_ready: true,
                    },
                );
                assert_eq!(next, without_claim);
            }
        }
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
        let service = LocalWorkService::new(
            database,
            project,
            "agent".into(),
            SessionId("waiver-session".into()),
            Some("protocol-test".into()),
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
    fn compact_agent_memory_signal_is_acknowledged_only_after_delivery() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("deferred-memory-advertisement".into());
        let service = LocalWorkService::new(
            database,
            project,
            "agent".into(),
            SessionId("deferred-memory-session".into()),
            Some("memory-advertisement-test".into()),
        );
        service
            .remember_project_memory("retained fact".into(), Some("retained-fact".into()), at(0))
            .expect("remember fixture");
        let query = WorkNextQuery {
            sections: vec![WorkNextSection::Memories],
            ..WorkNextQuery::default()
        };
        let first = service
            .work_next_for_agent(20, query.clone(), at(1))
            .expect("first deferred signal");
        assert!(first.memories.as_ref().is_some_and(|signal| signal.changed));
        assert!(first.memory_advertisement.is_some());
        let repeated = service
            .work_next_for_agent(20, query.clone(), at(2))
            .expect("unacknowledged signal repeats");
        assert!(
            repeated
                .memories
                .as_ref()
                .is_some_and(|signal| signal.changed)
        );
        service.acknowledge_work_next_memories(&first);
        let stable = service
            .work_next_for_agent(20, query, at(3))
            .expect("acknowledged signal is stable");
        assert!(
            stable
                .memories
                .as_ref()
                .is_some_and(|signal| !signal.changed)
        );
        assert!(stable.memory_advertisement.is_none());
    }

    #[test]
    fn rejected_memory_advisory_cannot_consume_an_unseen_work_change_page() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("memory-advisory-delivery-order".into());
        let session = SessionId("memory-advisory-delivery-session".into());
        let reader = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "reader".into(),
            session.clone(),
            Some("memory-advisory-test".into()),
        );
        let writer = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "writer".into(),
            SessionId("memory-advisory-writer".into()),
            Some("memory-advisory-test".into()),
        );
        let created = match writer
            .work_propose(
                root_input("Peer change", "memory-advisory-peer-change"),
                at(0),
            )
            .expect("create peer change")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };

        assert!(matches!(
            reader.work_next_for_agent(
                20,
                WorkNextQuery {
                    context_generation: Some("invalid\ncontext".into()),
                    ..WorkNextQuery::default()
                },
                at(1),
            ),
            Err(StoreError::InvalidProjectMemory(_))
        ));
        let after_refusal = SqliteStore::open(&database)
            .expect("store")
            .work_session_state(&project, &session, at(1))
            .expect("session state after refusal");
        assert_eq!(after_refusal.project_cursor, 0);
        assert_eq!(after_refusal.tentative_project_cursor, None);

        let replayed = reader
            .work_next_for_agent(20, WorkNextQuery::default(), at(2))
            .expect("corrected call delivers unseen page");
        assert!(replayed.changes.as_ref().is_some_and(|changes| {
            changes.iter().any(|change| {
                matches!(
                    &change.delivery,
                    WorkChangeProjection::Visible(summary)
                        if summary.work_id == Some(created.work_id)
                )
            })
        }));
        assert!(replayed.delivered_through.is_some());
    }

    #[test]
    fn project_memory_advisory_is_constant_decode_at_scale() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("project-memory-advisory-scale".into());
        let service = LocalWorkService::new(
            database,
            project,
            "agent".into(),
            SessionId("memory-scale-session".into()),
            Some("project-memory-scale-test".into()),
        );
        for index in 0..256 {
            service
                .remember_project_memory(
                    format!("retained project observation {index}"),
                    Some(format!("memory-{index:03}")),
                    at(index),
                )
                .expect("remember scale fixture");
        }
        crate::canonical::reset_canonical_decode_count();
        let next = service
            .work_next_for_agent(
                20,
                WorkNextQuery {
                    sections: vec![WorkNextSection::Memories],
                    ..WorkNextQuery::default()
                },
                at(300),
            )
            .expect("read O(1) advisory state");
        assert_eq!(next.memories.as_ref().map(|signal| signal.count), Some(256));
        assert_eq!(
            crate::canonical::canonical_decode_count(),
            0,
            "the advisory hot path must not walk canonical memory history"
        );
    }

    #[test]
    fn maximum_default_fanout_decomposition_receipt_is_bounded_and_replays_exactly() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("maximum-fanout".into());
        let service = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            SessionId("fanout-session".into()),
            Some("protocol-test".into()),
        );
        service
            .work_propose(root_input("Maximum fanout", "fanout-root"), at(0))
            .expect("root proposal");
        let input = WorkProposeInput::Decompose {
            children: (0..16)
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
        assert_eq!(summary.child_count, 16);
        assert_eq!(summary.children.len(), 16);
        assert!(summary.details_omitted);
        assert_eq!(
            summary
                .children
                .iter()
                .map(|child| child.work_id)
                .collect::<std::collections::HashSet<_>>()
                .len(),
            16
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
    fn focus_bounds_repeated_direct_decomposition_at_the_root_open_work_limit() {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let service = LocalWorkService::new(
            database,
            ProjectId("repeated-direct-fanout".into()),
            "agent".into(),
            SessionId("repeated-direct-fanout-session".into()),
            Some("protocol-test".into()),
        );
        let root = proposed_root(
            service
                .work_propose(root_input("Repeated direct fanout", "fanout-root"), at(0))
                .expect("root proposal"),
        );

        for batch in 0..8 {
            let second = 1 + i64::from(batch) * 2;
            service
                .work_focus(&root.short_ref, at(second))
                .expect("refocus current parent revision");
            service
                .work_propose(
                    WorkProposeInput::Decompose {
                        children: (0..16)
                            .map(|index| WorkChildInput {
                                key: format!("batch-{batch}-child-{index}"),
                                title: format!("Open child {batch}-{index} {}", "x".repeat(256)),
                                outcome: format!("Open child {batch}-{index} outcome"),
                                acceptance: vec![format!("Open child {batch}-{index} accepted")],
                                requirement: Some(ChildRequirement::Required),
                                kind: Some(WorkItemKind::Task),
                                priority: Some(1),
                                labels: Vec::new(),
                                assigned_to: None,
                                deferred_until: None,
                            })
                            .collect(),
                        prerequisites: Vec::new(),
                        idempotency_key: format!("fanout-batch-{batch}"),
                    },
                    at(second + 1),
                )
                .expect("add one direct-child batch");
        }

        let focus = service
            .work_focus(&root.short_ref, at(20))
            .expect("bounded focus at the root open-work limit");
        assert_eq!(focus.child_count, 128);
        assert_eq!(focus.children.len(), MAX_FOCUS_RELATIONS);
        assert!(
            focus
                .children
                .iter()
                .all(|child| child.lifecycle == WorkLifecycle::Open)
        );
        assert!(focus.omissions.iter().any(|omission| {
            omission.reason == WorkSectionOmissionReason::UnfinishedChildCountLimit
                && omission.omitted_count == 128 - MAX_FOCUS_RELATIONS
        }));
        assert!(focus.omissions.iter().all(|omission| {
            omission.reason != WorkSectionOmissionReason::TerminalChildCountLimit
        }));
        assert!(
            serde_json::to_vec(&focus)
                .expect("serialize bounded focus")
                .len()
                <= MAX_AGENT_WORK_RESPONSE_BYTES
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
        let writer = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            SessionId("writer".into()),
            Some("protocol-test".into()),
        );
        let reader = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            SessionId("reader".into()),
            Some("protocol-test".into()),
        );

        let mut work_ids = Vec::new();
        for item_index in 0..500 {
            let work = match writer
                .work_propose(
                    root_input(
                        &format!("Bounded item {item_index:03}"),
                        &format!("bounded-root-{item_index:03}"),
                    ),
                    at(i64::from(item_index)),
                )
                .expect("create bounded root")
            {
                WorkProposeResult::Root { work, .. } => work,
                WorkProposeResult::Decomposition(_) => panic!("expected root"),
            };
            work_ids.push(work.work_id);
        }
        let mut event_store = SqliteStore::open(&database).expect("event store");
        let last_work_id = *work_ids.last().expect("last scale work item");
        let entry = event_store
            .work_event_tail(last_work_id, 1)
            .expect("base event tail")
            .pop()
            .expect("base event");
        let base = event_store
            .get::<WorkEvent>(&entry.object_hash)
            .expect("load base event")
            .expect("base event object");
        for event_index in 0..9 {
            let mut event = base.clone();
            event.created_at = at(1_000 + i64::from(event_index));
            event.actor.reason = format!("bounded synthetic event {event_index}");
            event_store
                .append_test_work_event(&event)
                .expect("append canonical scale event");
        }

        let initial_head = SqliteStore::open(&database)
            .expect("store")
            .work_feed_head(&FeedId::Project(project.clone()))
            .expect("project feed head");
        assert_eq!(initial_head, 509);

        crate::storage::reset_work_event_decode_count();
        crate::storage::reset_work_item_projection_decode_count();
        let ready_started = std::time::Instant::now();
        reader
            .work_next(
                50,
                WorkNextQuery {
                    sections: vec![WorkNextSection::Ready],
                    ..WorkNextQuery::default()
                },
                at(1_100),
            )
            .expect("ready-only scale query");
        let ready_elapsed = ready_started.elapsed();
        let ready_event_decodes = crate::storage::work_event_decode_count();
        let ready_item_decodes = crate::storage::work_item_projection_decode_count();
        eprintln!(
            "work_next scale ready: elapsed_us={} event_decodes={} item_decodes={}",
            ready_elapsed.as_micros(),
            ready_event_decodes,
            ready_item_decodes
        );
        assert_eq!(ready_event_decodes, 0);
        assert!(ready_item_decodes <= 50);

        crate::storage::reset_work_event_decode_count();
        crate::storage::reset_work_item_projection_decode_count();
        let catalog_started = std::time::Instant::now();
        reader
            .work_next(
                50,
                WorkNextQuery {
                    sections: vec![WorkNextSection::Catalog],
                    ..WorkNextQuery::default()
                },
                at(1_101),
            )
            .expect("catalog-only scale query");
        let catalog_elapsed = catalog_started.elapsed();
        let catalog_event_decodes = crate::storage::work_event_decode_count();
        let catalog_item_decodes = crate::storage::work_item_projection_decode_count();
        eprintln!(
            "work_next scale catalog: elapsed_us={} event_decodes={} item_decodes={}",
            catalog_elapsed.as_micros(),
            catalog_event_decodes,
            catalog_item_decodes
        );
        assert_eq!(catalog_event_decodes, 0);
        assert!(catalog_item_decodes <= 51);

        crate::storage::reset_work_event_decode_count();
        crate::storage::reset_work_item_projection_decode_count();
        let selective_catalog_started = std::time::Instant::now();
        reader
            .work_next(
                50,
                WorkNextQuery {
                    sections: vec![WorkNextSection::Catalog],
                    search: Some("Bounded item 499".into()),
                    ..WorkNextQuery::default()
                },
                at(1_102),
            )
            .expect("selective catalog scale query");
        let selective_catalog_elapsed = selective_catalog_started.elapsed();
        let selective_catalog_event_decodes = crate::storage::work_event_decode_count();
        let selective_catalog_item_decodes = crate::storage::work_item_projection_decode_count();
        eprintln!(
            "work_next scale selective_catalog: elapsed_us={} event_decodes={} item_decodes={}",
            selective_catalog_elapsed.as_micros(),
            selective_catalog_event_decodes,
            selective_catalog_item_decodes
        );
        assert_eq!(selective_catalog_event_decodes, 0);
        assert_eq!(selective_catalog_item_decodes, 1);

        crate::storage::reset_work_event_decode_count();
        crate::storage::reset_work_item_projection_decode_count();
        let first = reader
            .work_next(1_000, WorkNextQuery::default(), at(1_103))
            .expect("bounded default work_next");
        let first_decode_count = crate::storage::work_event_decode_count();
        let first_item_decode_count = crate::storage::work_item_projection_decode_count();
        eprintln!(
            "work_next scale default: event_decodes={first_decode_count} item_decodes={first_item_decode_count}"
        );
        assert_eq!(first_decode_count, 0);
        assert!(first_item_decode_count <= 1_000);
        assert!(
            serde_json::to_vec(&first)
                .expect("serialize first page")
                .len()
                <= MAX_AGENT_WORK_RESPONSE_BYTES
        );
        assert!(first.omissions.iter().any(|omission| {
            omission.section == WorkNextSection::Changes
                && omission.reason == WorkSectionOmissionReason::Staged
                && omission.omitted_count > 0
        }));
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

        // The next changes call delivers the first page implicitly and
        // continues densely from its boundary.
        let following = reader
            .work_next(
                1_000,
                WorkNextQuery {
                    sections: vec![WorkNextSection::Changes],
                    ..WorkNextQuery::default()
                },
                at(602),
            )
            .expect("following staged changes");
        assert_eq!(following.session.confirmed_project_cursor, first_cursor);
        let following_cursor = following
            .delivered_through
            .expect("following delivery cursor");
        assert!(following_cursor > first_cursor);
        let following_changes = following.changes.as_ref().expect("following changes");
        assert_ne!(
            following_changes
                .iter()
                .map(|change| change.entry.object_hash.clone())
                .collect::<Vec<_>>(),
            first_hashes
        );
        for (offset, change) in following_changes.iter().enumerate() {
            assert_eq!(
                change.entry.position.position,
                first_cursor + 1 + i64::try_from(offset).expect("offset")
            );
        }

        let mut expected_position = following_cursor + 1;
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

    #[test]
    #[ignore = "runs separately so the project-scale fixture and decode samples stay out of the ordinary suite"]
    #[allow(
        clippy::too_many_lines,
        reason = "one scale regression measures the complete claim-validated mutation family against one fixed project fixture"
    )]
    fn claim_validated_mutations_are_bounded_at_project_scale() {
        // The long-lived MCP server retains one service for its process
        // lifetime, so these samples include exactly the production warm-call
        // lifecycle rather than silently omitting a per-request reopen.
        const ITEM_COUNT: usize = 500;
        const TOTAL_EVENT_COUNT: usize = 5_000;
        const DEEP_EVENT_COUNT: usize = 500;
        const SAMPLE_COUNT: usize = 20;

        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let project = ProjectId("claim-mutation-scale".into());
        let writer = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            SessionId("mutation-writer".into()),
            Some("protocol-test".into()),
        );
        let reader = LocalWorkService::new(
            database.clone(),
            project.clone(),
            "agent".into(),
            SessionId("mutation-reader".into()),
            Some("protocol-test".into()),
        );

        let mut work_items = Vec::with_capacity(ITEM_COUNT);
        for item_index in 0..ITEM_COUNT {
            let work = match writer
                .work_propose(
                    root_input(
                        &format!("Claim mutation item {item_index:03}"),
                        &format!("claim-mutation-root-{item_index:03}"),
                    ),
                    at(i64::try_from(item_index).expect("item timestamp")),
                )
                .expect("create scale root")
            {
                WorkProposeResult::Root { work, .. } => work,
                WorkProposeResult::Decomposition(_) => panic!("expected root"),
            };
            work_items.push(work);
        }

        let mut event_store = SqliteStore::open(&database).expect("event store");
        let mut synthetic_events = Vec::with_capacity(TOTAL_EVENT_COUNT - ITEM_COUNT);
        for (item_index, work) in work_items.iter().enumerate() {
            let entry = event_store
                .work_event_tail(work.work_id, 1)
                .expect("base event tail")
                .pop()
                .expect("base event");
            let base = event_store
                .get::<WorkEvent>(&entry.object_hash)
                .expect("load base event")
                .expect("base event object");
            let event_count = if item_index == ITEM_COUNT - 1 {
                DEEP_EVENT_COUNT
            } else if item_index < 9 {
                10
            } else {
                9
            };
            for event_index in 1..event_count {
                let mut event = base.clone();
                event.created_at = at(600 + i64::try_from(event_index).expect("event timestamp"));
                event.actor.reason =
                    format!("claim mutation scale item {item_index:03} event {event_index:02}");
                synthetic_events.push(event);
            }
        }
        event_store
            .append_test_work_events(&synthetic_events)
            .expect("append scale event history");
        assert_eq!(
            event_store
                .work_feed_head(&FeedId::Project(project.clone()))
                .expect("scale project feed head"),
            i64::try_from(TOTAL_EVENT_COUNT).expect("scale feed head")
        );
        drop(event_store);

        let sampled_work = &work_items[ITEM_COUNT - SAMPLE_COUNT..];
        let mut claim_samples = Vec::with_capacity(SAMPLE_COUNT);
        for (sample_index, work) in sampled_work.iter().enumerate() {
            writer
                .select_work(
                    &work.short_ref,
                    at(1_100 + i64::try_from(sample_index).expect("select timestamp")),
                )
                .expect("select claim target");
            measure_scale_operation(&mut claim_samples, || {
                writer.work_update(
                    WorkUpdateInput::Claim {
                        ttl_seconds: Some(3_600),
                        recovery_reason: None,
                        idempotency_key: format!("scale-claim-{sample_index:02}"),
                    },
                    at(1_120 + i64::try_from(sample_index).expect("claim timestamp")),
                )
            })
            .expect("claim scale target");
        }

        let mut work_next_samples = Vec::with_capacity(SAMPLE_COUNT);
        for sample_index in 0..SAMPLE_COUNT {
            measure_scale_operation(&mut work_next_samples, || {
                reader.work_next(
                    50,
                    WorkNextQuery {
                        sections: vec![WorkNextSection::Ready],
                        ..WorkNextQuery::default()
                    },
                    at(1_150 + i64::try_from(sample_index).expect("work_next timestamp")),
                )
            })
            .expect("measure ready work_next");
        }

        let mut evidence_samples = Vec::with_capacity(SAMPLE_COUNT);
        for (sample_index, work) in sampled_work.iter().enumerate() {
            writer
                .select_work(
                    &work.short_ref,
                    at(1_170 + i64::try_from(sample_index * 2).expect("select timestamp")),
                )
                .expect("select evidence target");
            measure_scale_operation(&mut evidence_samples, || {
                writer.work_update(
                    WorkUpdateInput::Evidence {
                        summary: format!("scale evidence {sample_index:02}"),
                        refs: vec![format!("test:claim-mutation-scale:{sample_index:02}")],
                        attach: None,
                        idempotency_key: format!("scale-evidence-{sample_index:02}"),
                    },
                    at(1_171 + i64::try_from(sample_index * 2).expect("evidence timestamp")),
                )
            })
            .expect("record scale evidence");
        }

        let mut gate_samples = Vec::with_capacity(SAMPLE_COUNT);
        let gate_target = sampled_work.last().expect("sampled gate target");
        for sample_index in 0..SAMPLE_COUNT {
            let failed = if sample_index % 2 == 0 {
                Vec::new()
            } else {
                vec!["scale alternating failure".to_owned()]
            };
            measure_scale_operation(&mut gate_samples, || {
                writer.work_gate_on(
                    Some(&gate_target.short_ref),
                    "claim-mutation-scale",
                    &failed,
                    None,
                    at(1_200 + i64::try_from(sample_index).expect("gate timestamp")),
                )
            })
            .expect("record scale gate transition");
        }

        let mut checkpoint_samples = Vec::with_capacity(SAMPLE_COUNT);
        for (sample_index, work) in sampled_work.iter().enumerate() {
            writer
                .select_work(
                    &work.short_ref,
                    at(1_220 + i64::try_from(sample_index * 2).expect("select timestamp")),
                )
                .expect("select checkpoint target");
            measure_scale_operation(&mut checkpoint_samples, || {
                writer.work_update(
                    WorkUpdateInput::Checkpoint {
                        summary: format!("scale checkpoint {sample_index:02}"),
                        evidence: None,
                        idempotency_key: format!("scale-checkpoint-{sample_index:02}"),
                    },
                    at(1_221 + i64::try_from(sample_index * 2).expect("checkpoint timestamp")),
                )
            })
            .expect("record scale checkpoint");
        }

        let mut revise_samples = Vec::with_capacity(SAMPLE_COUNT);
        for (sample_index, work) in sampled_work.iter().enumerate() {
            writer
                .select_work(
                    &work.short_ref,
                    at(1_270 + i64::try_from(sample_index * 2).expect("select timestamp")),
                )
                .expect("select revision target");
            measure_scale_operation(&mut revise_samples, || {
                writer.work_update(
                    WorkUpdateInput::Revise {
                        patch: WorkRevisionPatch {
                            title: Some(format!(
                                "Claim mutation target revision {sample_index:02}"
                            )),
                            ..WorkRevisionPatch::default()
                        },
                        idempotency_key: format!("scale-revise-{sample_index:02}"),
                    },
                    at(1_271 + i64::try_from(sample_index * 2).expect("revise timestamp")),
                )
            })
            .expect("revise scale target");
        }

        let mut block_samples = Vec::with_capacity(SAMPLE_COUNT);
        let mut unblock_samples = Vec::with_capacity(SAMPLE_COUNT);
        for (sample_index, work) in sampled_work.iter().enumerate() {
            writer
                .select_work(
                    &work.short_ref,
                    at(1_320 + i64::try_from(sample_index * 3).expect("select timestamp")),
                )
                .expect("select blocker target");
            let blocked = measure_scale_operation(&mut block_samples, || {
                writer.work_update(
                    WorkUpdateInput::Block {
                        blocker_kind: WorkBlockerKind::Manual,
                        detail: format!("scale blocker {sample_index:02}"),
                        idempotency_key: format!("scale-block-{sample_index:02}"),
                    },
                    at(1_321 + i64::try_from(sample_index * 3).expect("block timestamp")),
                )
            })
            .expect("block scale target");
            let blocker_id = blocked
                .receipt
                .result
                .get("blocker_id")
                .and_then(serde_json::Value::as_str)
                .expect("blocker receipt id")
                .to_owned();
            measure_scale_operation(&mut unblock_samples, || {
                writer.work_update(
                    WorkUpdateInput::Unblock {
                        blocker_id: Some(blocker_id),
                        idempotency_key: format!("scale-unblock-{sample_index:02}"),
                    },
                    at(1_322 + i64::try_from(sample_index * 3).expect("unblock timestamp")),
                )
            })
            .expect("unblock scale target");
        }

        let mut handoff_samples = Vec::with_capacity(SAMPLE_COUNT);
        for (sample_index, work) in sampled_work.iter().enumerate() {
            writer
                .select_work(
                    &work.short_ref,
                    at(1_400 + i64::try_from(sample_index * 4).expect("select timestamp")),
                )
                .expect("select handoff target");
            measure_scale_operation(&mut handoff_samples, || {
                writer.work_handoff(
                    WorkHandoffInput::Offer {
                        to: "handoff-peer".into(),
                        ttl_seconds: Some(300),
                        checkpoint_summary: format!("scale handoff checkpoint {sample_index:02}"),
                        idempotency_key: format!("scale-handoff-offer-{sample_index:02}"),
                    },
                    at(1_401 + i64::try_from(sample_index * 4).expect("offer timestamp")),
                )
            })
            .expect("offer scale handoff");
            writer
                .work_handoff(
                    WorkHandoffInput::Cancel {
                        reason: "restore benchmark executor".into(),
                        idempotency_key: format!("scale-handoff-cancel-{sample_index:02}"),
                    },
                    at(1_402 + i64::try_from(sample_index * 4).expect("cancel timestamp")),
                )
                .expect("cancel scale handoff");
            writer
                .work_update(
                    WorkUpdateInput::Checkpoint {
                        summary: format!("post-handoff checkpoint {sample_index:02}"),
                        evidence: None,
                        idempotency_key: format!("scale-post-handoff-checkpoint-{sample_index:02}"),
                    },
                    at(1_403 + i64::try_from(sample_index * 4).expect("checkpoint timestamp")),
                )
                .expect("checkpoint after scale handoff");
        }

        let mut complete_samples = Vec::with_capacity(SAMPLE_COUNT);
        for (sample_index, work) in sampled_work.iter().enumerate() {
            writer
                .select_work(
                    &work.short_ref,
                    at(1_500 + i64::try_from(sample_index * 2).expect("select timestamp")),
                )
                .expect("select completion target");
            let completed = measure_scale_operation(&mut complete_samples, || {
                writer.work_complete(
                    WorkCompleteInput {
                        capture: None,
                        evidence: Vec::new(),
                        acceptance: None,
                        note: Some(format!("scale completion {sample_index:02}")),
                        idempotency_key: format!("scale-complete-{sample_index:02}"),
                    },
                    at(1_501 + i64::try_from(sample_index * 2).expect("complete timestamp")),
                )
            })
            .expect("complete scale target");
            assert!(matches!(completed, WorkCompleteResult::Completed(_)));
        }

        for (operation, samples) in [
            ("claim", &claim_samples),
            ("evidence", &evidence_samples),
            ("gate", &gate_samples),
            ("checkpoint", &checkpoint_samples),
            ("revise", &revise_samples),
            ("block", &block_samples),
            ("unblock", &unblock_samples),
            ("handoff", &handoff_samples),
            ("complete", &complete_samples),
            ("work_next", &work_next_samples),
        ] {
            assert_eq!(samples.len(), SAMPLE_COUNT);
            report_scale_samples(operation, samples);
            let (canonical_budget, work_event_budget, item_budget) = if operation == "work_next" {
                (16, 0, 64)
            } else {
                (64, 64, 16)
            };
            for (kind, actual, budget) in [
                (
                    "canonical-decode",
                    samples
                        .iter()
                        .map(|sample| sample.canonical_decodes)
                        .max()
                        .expect("scale samples"),
                    canonical_budget,
                ),
                (
                    "work-event-decode",
                    samples
                        .iter()
                        .map(|sample| sample.work_event_decodes)
                        .max()
                        .expect("scale samples"),
                    work_event_budget,
                ),
                (
                    "item-decode",
                    samples
                        .iter()
                        .map(|sample| sample.item_decodes)
                        .max()
                        .expect("scale samples"),
                    item_budget,
                ),
            ] {
                assert!(
                    actual <= budget,
                    "{operation} exceeded its bounded {kind} budget of {budget}: {actual}"
                );
            }
        }
    }
}
