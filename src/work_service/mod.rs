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
    ReopenWorkRequest, RestoredWorkEvidence, ReviseWorkRequest, SessionId, SqliteStore, TaskId,
    VerificationEvidence, VerificationKind, VerificationResult, WaiveRequiredChildRequest,
    WorkAvailability, WorkBlockerKind, WorkCatalogQuery, WorkCheckpoint, WorkClaim, WorkClaimId,
    WorkClaimState, WorkCompletionRecovery, WorkDecomposition, WorkDependencyRef, WorkDisposition,
    WorkEvent, WorkEvidence, WorkEvidenceKind, WorkFeedEntry, WorkGraphSnapshotDestinationKind,
    WorkGraphSnapshotExport, WorkGraphSnapshotLoadResult, WorkHandoffOffer, WorkHandoffState,
    WorkId, WorkItem, WorkItemKind, WorkLifecycle, WorkObligation, WorkObligationResolution,
    WorkObligationResolutionEvent, WorkObligationState, WorkOrigin, WorkPlanningAuthority,
    WorkPrerequisiteState, WorkRevisionPatch, WorkRun, WorkRunId, WorkRunState, WorkSessionState,
    WorkTransition,
    domain::{
        ACTOR_CONTEXT_NORMALIZED_REFERENCE, ACTOR_CONTEXT_PROVENANCE_REFERENCE, AssuranceLevel,
        ForgetProjectMemoryRequest, MAX_ACTOR_CONTEXT_BYTES, MemoryAssertionEvent,
        MemoryContradictionEvent, POST_COMPLETION_EVIDENCE_PROVENANCE_REFERENCE,
        POST_COMPLETION_EVIDENCE_PROVENANCE_SOURCE, ProjectMemoryFull, ProjectMemoryList,
        ProjectMemoryMutationReceipt, ProvenanceLink, ProvenanceRelation,
        RecordGateEvidenceRequest, RecordRestoredWorkEvidenceRequest, RecordWorkNoteRequest,
        RememberProjectMemoryRequest, RestoredWorkEvidenceInput, SCHEMA_VERSION, Scope,
        Sensitivity, WorkCompletionRecoveryCause, is_unsafe_rendered_text_char,
        validate_gate_evidence_payload,
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
mod operations;
mod projection;
mod propose;
mod service;
mod update;
mod views;

#[cfg(test)]
mod test_support;

pub use operations::*;
pub(crate) use projection::*;
pub use views::*;

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

#[allow(
    clippy::too_many_arguments,
    reason = "the receiving session and visibility boundary bind the exact budgeted delivery page"
)]
fn verified_bounded_work_changes(
    store: &SqliteStore,
    project_id: &ProjectId,
    session_id: &SessionId,
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
        let from_current_session = source_is_from_session(&object, session_id);
        let delivery = agent_change_object(
            store,
            project_id,
            focused_root_id,
            bound_task_id,
            &entry.object_kind,
            object,
            Some(&entry.position),
        )?;
        changes.push(WorkChange {
            from_current_session: matches!(&delivery, WorkChangeProjection::Visible(_))
                && from_current_session,
            delivery,
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
    session_id: &SessionId,
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
    for (entry, change) in entries.into_iter().zip(&page.changes) {
        let object = store
            .get::<serde_json::Value>(&entry.object_hash)?
            .ok_or_else(|| {
                StoreError::InvalidWorkProjection(format!(
                    "staged project-feed object {} is missing",
                    entry.object_hash
                ))
            })?;
        let expected = matches!(&change.delivery, WorkChangeProjection::Visible(_))
            && source_is_from_session(&object, session_id);
        if change.from_current_session != expected {
            return Err(StoreError::InvalidWorkProjection(
                "staged work attribution differs from the receiving session".into(),
            ));
        }
    }
    Ok(())
}

fn source_is_from_session(object: &serde_json::Value, session_id: &SessionId) -> bool {
    object
        .get("actor")
        .and_then(|actor| actor.get("session_id"))
        .and_then(serde_json::Value::as_str)
        == Some(session_id.0.as_str())
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
    position: Option<&FeedPosition>,
) -> Result<WorkChangeProjection, StoreError> {
    match object_kind {
        "work_event" => {
            let event = serde_json::from_value::<WorkEvent>(object)?;
            let position = position.ok_or_else(|| {
                StoreError::InvalidWorkProjection(
                    "work event delivery is missing its feed position".into(),
                )
            })?;
            Ok(WorkChangeProjection::Visible(project_work_event(
                store, &event, position,
            )?))
        }
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
        "work_observation" => {
            let observation = serde_json::from_value::<crate::domain::WorkObservation>(object)?;
            let item = store.get_work_item(observation.work_id)?;
            if &observation.project_id != project_id || observation.root_id != item.root_id {
                return Err(StoreError::InvalidWorkProjection(
                    "observation crosses its work project".into(),
                ));
            }
            Ok(WorkChangeProjection::Visible(WorkChangeSummary {
                schema_version: observation.schema_version,
                object_kind: object_kind.into(),
                work_id: Some(observation.work_id),
                work_ref: Some(item.short_ref),
                revision: None,
                change_kind: "evidence".into(),
                summary: compact_text(&format!("non-holder: {}", observation.summary)),
                actor_id: Some(compact_text(&observation.actor.actor_id)),
                actor_context: projected_actor_context(&observation.actor),
                created_at: observation.created_at,
            }))
        }
        "work_restored_evidence" => {
            let evidence = serde_json::from_value::<RestoredWorkEvidence>(object)?;
            let item = store.get_work_item(evidence.work_id)?;
            Ok(WorkChangeProjection::Visible(WorkChangeSummary {
                schema_version: evidence.schema_version,
                object_kind: object_kind.into(),
                work_id: Some(evidence.work_id),
                work_ref: Some(item.short_ref),
                revision: None,
                change_kind: "evidence".into(),
                summary: compact_restored_work_evidence(&evidence)?,
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

fn project_work_event(
    store: &SqliteStore,
    event: &WorkEvent,
    position: &FeedPosition,
) -> Result<WorkChangeSummary, StoreError> {
    let fields = if matches!(event.transition, WorkTransition::Revised { .. }) {
        let previous = store.work_planning_before(position, event)?;
        let current = serde_json::to_value(&event.work)?;
        [
            ("title", "title"),
            ("outcome", "outcome"),
            ("acceptance", "acceptance"),
            ("kind", "kind"),
            ("priority", "priority"),
            ("labels", "labels"),
            ("assigned_to", "assignment"),
            ("deferred_until", "deferral"),
        ]
        .into_iter()
        .filter_map(|(key, word)| (previous[key] != current[key]).then_some(word))
        .collect::<Vec<_>>()
    } else {
        Vec::new()
    };
    Ok(agent_work_event_summary(event, &fields))
}

fn agent_work_event_summary(event: &WorkEvent, revised_fields: &[&str]) -> WorkChangeSummary {
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
            work_transition_summary(event, revised_fields)
        )),
        actor_id: Some(compact_text(&event.actor.actor_id)),
        actor_context: projected_actor_context(&event.actor),
        created_at: event.created_at,
    }
}

fn work_transition_summary(event: &WorkEvent, revised_fields: &[&str]) -> String {
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
        WorkTransition::Revised { .. } => {
            let fields = if revised_fields.is_empty() {
                "no planning change".into()
            } else {
                revised_fields.join(", ")
            };
            format!("{fields}: \"{title}\"")
        }
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
        WorkTransition::ClaimRenewed { .. } => format!("renewed by its holder: \"{title}\""),
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
        WorkTransition::ClaimRenewed { .. } => "claim_renewed",
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
    allowed.push("work_update:note".into());
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
