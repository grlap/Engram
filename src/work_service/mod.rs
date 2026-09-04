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
    WorkEvidence, WorkEvidenceKind, WorkFeedEntry, WorkGraphSnapshotDestinationKind,
    WorkGraphSnapshotExport, WorkHandoffOffer, WorkHandoffState, WorkId, WorkItem, WorkItemKind,
    WorkLifecycle, WorkObligation, WorkObligationResolution, WorkObligationResolutionEvent,
    WorkObligationState, WorkOrigin, WorkPlanningAuthority, WorkPrerequisiteState,
    WorkRevisionPatch, WorkRun, WorkRunId, WorkRunState, WorkSessionState, WorkTransition,
    domain::{
        ACTOR_CONTEXT_NORMALIZED_REFERENCE, ACTOR_CONTEXT_PROVENANCE_REFERENCE, AssuranceLevel,
        ForgetProjectMemoryRequest, MAX_ACTOR_CONTEXT_BYTES, MemoryAssertionEvent,
        MemoryContradictionEvent, POST_COMPLETION_EVIDENCE_PROVENANCE_REFERENCE,
        POST_COMPLETION_EVIDENCE_PROVENANCE_SOURCE, ProjectMemoryFull, ProjectMemoryList,
        ProjectMemoryMutationReceipt, ProvenanceLink, ProvenanceRelation,
        RecordGateEvidenceRequest, RecordWorkNoteRequest, RememberProjectMemoryRequest,
        SCHEMA_VERSION, Scope, Sensitivity, WorkCompletionRecoveryCause,
        is_unsafe_rendered_text_char, validate_gate_evidence_payload,
    },
    storage::{
        BeginGateWorkProtocolAttempt, BeginWorkProtocolAttempt, CompleteWorkStorageResult,
        CompletionRecoverySnapshot, PROCESS_DEFAULT_WORK_SESSION_NAMESPACE,
        PROCESS_DEFAULT_WORK_SESSION_PREFIX, PROCESS_DEFAULT_WORK_SESSION_RETENTION_SECONDS,
        PROCESS_DEFAULT_WORK_SESSION_REUSE_REFUSAL, ProjectMemoryAdvertisement,
        StageWorkSessionDelivery, StoreError, WorkEvidenceProjectionSummary, WorkNoteCapture,
        normalize_completion_acceptance_shape,
    },
};

#[cfg(test)]
use crate::WorkReferenceCandidate;

mod completion;
mod focus;
mod handoff;
mod memories;
mod next;
mod propose;
mod service;
mod update;

#[cfg(test)]
mod test_support;

/// Hard ceiling for every successful agent-facing work response.
pub const MAX_AGENT_WORK_RESPONSE_BYTES: usize = 12 * 1024;

const MAX_PROJECT_MEMORY_FULL_BYTES: usize = 12 * 1024;
const _: () = assert!(MAX_PROJECT_MEMORY_FULL_BYTES <= MAX_AGENT_WORK_RESPONSE_BYTES);

const MAX_CHANGE_SECTION_BYTES: usize = 4 * 1024;
const MAX_READY_SECTION_BYTES: usize = 2 * 1024;
const MAX_CATALOG_SECTION_BYTES: usize = 3 * 1024;
const MAX_OBLIGATION_PAGE_BYTES: usize = 4 * 1024;
const MAX_FOCUS_HISTORY: u32 = 4;
pub(crate) const MAX_FOCUS_RELATIONS: usize = 8;
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
pub(crate) const COMPLETED_WORK_LATE_FINDING_REFUSAL: &str = "completed work cannot be mutated; use note or gate to record a late finding without reopening it";

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
    process_default_session_initialized: OnceLock<()>,
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
            process_default_session_initialized: OnceLock::new(),
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
            .field(
                "process_default_session_initialized",
                &self.process_default_session_initialized.get().is_some(),
            )
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
    /// Exact evidence membership count before the bounded focus selection.
    #[serde(default)]
    pub evidence_count: usize,
    /// Last run evidence by dense run-feed position, selected independently
    /// from the obligation-prioritized evidence page. Evidence timestamps are
    /// asserted metadata, never ordering authority. This fixed-size advisory
    /// is populated only for drill-down reads and retained while other focus
    /// rows trim.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub latest_evidence_item: Option<WorkEvidenceSummary>,
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

/// Generates the reserved, time-bearing identity used when the shell did not
/// receive a durable session from its host or operator.
#[must_use]
pub fn new_process_default_work_session_id() -> String {
    format!(
        "{PROCESS_DEFAULT_WORK_SESSION_PREFIX}{}-{}",
        std::process::id(),
        uuid::Uuid::now_v7()
    )
}

fn process_default_work_session_started_at(
    session_id: &SessionId,
) -> Result<Option<DateTime<Utc>>, StoreError> {
    if !session_id
        .0
        .starts_with(PROCESS_DEFAULT_WORK_SESSION_NAMESPACE)
    {
        return Ok(None);
    }
    let encoded = session_id
        .0
        .strip_prefix(PROCESS_DEFAULT_WORK_SESSION_PREFIX)
        .ok_or_else(process_default_work_session_reuse_refusal)?;
    let (process_id, uuid) = encoded
        .split_once('-')
        .ok_or_else(process_default_work_session_reuse_refusal)?;
    process_id
        .parse::<u32>()
        .map_err(|_| process_default_work_session_reuse_refusal())?;
    let uuid =
        uuid::Uuid::parse_str(uuid).map_err(|_| process_default_work_session_reuse_refusal())?;
    if uuid.get_version_num() != 7 {
        return Err(process_default_work_session_reuse_refusal());
    }
    let (seconds, nanos) = uuid
        .get_timestamp()
        .ok_or_else(|| {
            StoreError::InvalidWorkProjection(
                "version 7 process-default session has no creation timestamp".into(),
            )
        })?
        .to_unix();
    let seconds = i64::try_from(seconds).map_err(|_| {
        StoreError::InvalidWorkProjection(
            "process-default session creation timestamp overflowed".into(),
        )
    })?;
    DateTime::from_timestamp(seconds, nanos)
        .map(Some)
        .ok_or_else(|| {
            StoreError::InvalidWorkProjection(
                "process-default session creation timestamp is invalid".into(),
            )
        })
}

fn process_default_work_session_reuse_refusal() -> StoreError {
    StoreError::InvalidWork(PROCESS_DEFAULT_WORK_SESSION_REUSE_REFUSAL.into())
}

fn validate_process_default_work_session(
    session_id: &SessionId,
    was_defaulted: bool,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    let started_at = process_default_work_session_started_at(session_id)?;
    if was_defaulted && started_at.is_none() {
        return Err(process_default_work_session_reuse_refusal());
    }
    if started_at.is_some_and(|started_at| {
        started_at > now
            || now.signed_duration_since(started_at).num_seconds()
                >= PROCESS_DEFAULT_WORK_SESSION_RETENTION_SECONDS
    }) {
        return Err(process_default_work_session_reuse_refusal());
    }
    Ok(())
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
        allowed.extend([
            "work_update:gate".into(),
            "work_update:note".into(),
            "work_update:reopen".into(),
        ]);
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
