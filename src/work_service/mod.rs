//! Ambient six-operation protocol over the local work lifecycle.

const REJECT_PROTOCOL_OPERATION: &str = "work_update:reject";

#[cfg(test)]
pub(crate) use update::{before_note_capture, before_selected_clear};

pub(crate) const MULTIPLE_BLOCKERS_REFUSAL: &str =
    "focused work has multiple active blockers; supply blocker_id";
pub(crate) const INTERRUPTED_SELECTED_UNBLOCK_PREFIX: &str =
    "this selected unblock began earlier and was never answered";

use std::{
    fmt,
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
    ExecutionObservation, FeedId, FeedPosition, MemorySummary, MemoryVersion, ObjectId,
    OfferWorkHandoffRequest, ProjectId, ReadyWork, RecordWorkEvidenceRequest, ReleaseWorkRequest,
    ReopenWorkRequest, RestoredWorkEvidence, ReviseWorkRequest, SessionId, SqliteStore,
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
        POST_COMPLETION_EVIDENCE_PROVENANCE_REFERENCE, POST_COMPLETION_EVIDENCE_PROVENANCE_SOURCE,
        ProjectMemoryFull, ProjectMemoryList, ProjectMemoryMutationReceipt, ProvenanceLink,
        ProvenanceRelation, RecordGateEvidenceRequest, RecordRestoredWorkEvidenceRequest,
        RecordWorkNoteRequest, RememberProjectMemoryRequest, RestoredWorkEvidenceInput,
        SCHEMA_VERSION, Scope, Sensitivity, WorkCompletionRecoveryCause,
        is_unsafe_rendered_text_char, validate_gate_evidence_payload,
    },
    storage::{
        BeginGateWorkProtocolAttempt, BeginWorkProtocolAttempt, CompleteWorkStorageResult,
        CompletionRecoverySnapshot, PROCESS_DEFAULT_WORK_SESSION_NAMESPACE,
        PROCESS_DEFAULT_WORK_SESSION_PREFIX, PROCESS_DEFAULT_WORK_SESSION_RETENTION_SECONDS,
        PROCESS_DEFAULT_WORK_SESSION_REUSE_REFUSAL, ProjectMemoryAdvertisement,
        ProjectMemoryListingCut, StageWorkSessionDelivery, StoreError,
        WorkEvidenceProjectionSummary, WorkNoteCapture, normalize_completion_acceptance_shape,
    },
};

#[cfg(test)]
use crate::WorkReferenceCandidate;

mod terminal;
pub(crate) use completion::{POLICY_STALE_CAUSES, missing_evaluation_remedy};
#[cfg(test)]
use completion::{completion_attempt_key, completion_capture_key};
#[cfg(test)]
use delivery::{source_is_from_session, verify_staged_work_change_page};
pub(crate) use history_display::blocker_kind_word;
#[cfg(test)]
pub(crate) use memories::{ensure_project_memory_full_is_admissible, project_memory_full_response};
pub(crate) use memories::{retiring_target_dropped_reminder, retiring_target_lines};
use terminal::terminal_safe_data_block;
pub(crate) use terminal::{render_agent_receipt_text, terminal_safe_multiline};
pub use terminal::{terminal_error_command, terminal_error_line};
mod delivery;
pub(crate) use delivery::read_pending_delivery;
#[cfg(test)]
use delivery::source_display_producer;
use delivery::{
    decode_staged_work_change_page, verified_bounded_work_changes, work_delivery_boundary,
};
#[cfg(test)]
use history_display::agent_work_event_summary;
use history_display::{project_work_event, work_evidence_kind_word, work_lifecycle_word};
mod guidance;
use guidance::allowed_next;

mod acceptance;
pub(crate) use acceptance::{WorkAcceptanceEvidence, WorkAcceptanceLink};
pub(crate) mod blocker_selector;
mod bound_verification;
mod evaluation_admission;
pub(crate) use bound_verification::bound_verification_remedy;
pub(crate) use evaluation_admission::{READ_RUN_EVIDENCE_REMEDY, evaluation_admission_remedy};
mod catalog;
mod change_context;
mod completion;
pub(crate) use completion::validate_completion_request;
mod continuation;
mod criterion_links;
pub(crate) use criterion_links::{WorkCriterionLinkRow, WorkCriterionLinksWindow};
mod deciding;
mod source_recovery;
pub(crate) use source_recovery::{
    MEASURE_SOURCE_ID_REMEDY, MEASURE_SOURCE_REMEDY, shown_source_recovery, source_recovery_remedy,
};
mod evaluate;
pub(crate) use evaluate::validate_evaluation_words;
mod evaluation_windows;
pub(crate) use evaluation_windows::UNKNOWN_EVALUATION_REFUSAL;
mod focus;
pub(crate) use focus::WorkAuthoredContract;
mod handoff;
mod import;
pub use import::{MAX_WORK_IMPORT_INPUT_BYTES, parse_work_import_input};
mod history_display;
pub(crate) mod identity;
mod memories;
mod next;
pub(crate) use next::CLAIM_LAPSE_UNAVAILABLE;
#[cfg(test)]
pub(crate) use next::own_claim_lapse_reminder;
mod observation_windows;
mod operations;
mod projection;
mod sealed_source;
mod served_root;
pub(crate) use deciding::{ShownDecidingObservation, bounded as bounded_shown_field};
pub(crate) use projection::normalize_actor_context;
#[cfg(test)]
pub(crate) use served_root::FAIL_NEXT_SERVED_ROOT_READ;
mod propose;
mod record_windows;
pub(crate) use record_windows::ASSESSMENT_CONTINUATION_REFUSAL;
mod imported_evidence;
mod note_admission;
mod service;
mod status;
mod task_sync_audit;
mod task_sync_exchange;
mod task_sync_folder;
mod task_sync_origin;
mod task_sync_settings;
mod task_sync_setup;
pub(crate) mod unadmitted;
pub(crate) use projection::shed_work_next_focus;
pub use status::WorkCurrentStatus;
pub(crate) use status::shorten_status_previews;
mod update;
pub(crate) use update::committed::{CommittedMutation, WorkGateIntent};
mod views;

#[cfg(test)]
mod test_support;

pub(crate) use catalog::WorkListingPage;
#[cfg(test)]
pub(crate) use catalog::{encode_listing_cursor_json, listing_cursor_json};
pub use evaluate::{EVALUATE_WORD_ONLY_RESERVE, EVALUATE_WORD_RESERVE};
#[cfg(test)]
pub(crate) use evaluation_windows::MAX_ROW_VERDICTS;
pub(crate) use evaluation_windows::{
    WorkEvaluationDetail, WorkEvaluationRow, WorkEvaluationWindow,
};
pub(crate) use observation_windows::{
    UnadmittedObservationDetail, WorkObservationRow, WorkObservationWindow,
};
// Test-only access for the observation renderer's maximum-check stress test.
#[cfg(test)]
pub(crate) use observation_windows::ObservedCheckRow;
pub use operations::*;
pub(crate) use projection::*;
pub(crate) use record_windows::{
    AssessmentCount, AssessmentView, VerificationAssessmentPage, WorkRecordRow, WorkRecordWindow,
};
pub use views::*;

/// Ceiling for bounded agent work responses. Explicit single-note detail and
/// `show --full` authored-contract reads are complete-body exceptions;
/// note/history windows remain bounded.
pub const MAX_AGENT_WORK_RESPONSE_BYTES: usize = 12 * 1024;

/// Largest per-page byte budget a program reading `ls` JSON may request in
/// place of the agent protocol budget, which is also the smallest.
pub const MAX_LS_PAGE_BYTES: usize = 1024 * 1024;

const MAX_PROJECT_MEMORY_FULL_BYTES: usize = 12 * 1024;
const _: () = assert!(MAX_PROJECT_MEMORY_FULL_BYTES <= MAX_AGENT_WORK_RESPONSE_BYTES);

/// Most claims one `work core held` answer lists; the rest are counted as
/// omitted. Sixteen full rows, each with its binding, stay well within the
/// agent response budget.
pub const MAX_HELD_CLAIMS: usize = 16;

const MAX_CHANGE_SECTION_BYTES: usize = 4 * 1024;
const MAX_READY_SECTION_BYTES: usize = 2 * 1024;
const MAX_CATALOG_SECTION_BYTES: usize = 3 * 1024;
const MAX_OBLIGATION_PAGE_BYTES: usize = 4 * 1024;
const MAX_FOCUS_HISTORY: u32 = 4;
pub(crate) const MAX_FOCUS_RELATIONS: usize = 8;
pub(crate) const MAX_CHILD_OBLIGATION_REFS: usize = 5;
const MAX_FOCUS_MEMORIES: u32 = 8;
pub(crate) const MAX_SUMMARY_BYTES: usize = 192;
const MAX_HISTORY_TITLE_BYTES: usize = 72;
const MAX_HISTORY_DETAIL_BYTES: usize = 72;
/// A blocker's detail in history is bounded so its whole selector and kind
/// come first within the summary bound; a long title after them may still be
/// shortened.
const MAX_HISTORY_BLOCKER_DETAIL_BYTES: usize = 48;
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
/// The actor kind the agent words record their actions as, and so the kind a
/// record must carry for its reader's display to call it "you".
pub(crate) const WORD_ACTOR_KIND: &str = "agent";
/// The relation of a record made by this reader's own asserted actor in
/// another session: same-actor continuity, shown for navigation only.
pub const SAME_ACTOR_OTHER_SESSION: &str = "same_actor_other_session";
pub(crate) const COMPLETED_WORK_LATE_FINDING_REFUSAL: &str = "completed work cannot be mutated; use note or gate to record a late finding without reopening it";

/// Bytes a read reminder the agent words add to any read can take, in JSON
/// or terminal text: the own-claim lapse reminder or its unavailable notice.
/// New memories are admitted with this much room left, so their full read
/// still fits with the reminder.
pub(crate) const READ_REMINDER_RESERVE: usize = 160;

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
    /// Setup preserves this assertion under its own bounded validation contract.
    setup_actor_context: Option<String>,
    session_id: SessionId,
    attribution_defaults: WorkAttributionDefaults,
    source_skill: Option<String>,
    cached_store: OnceLock<Mutex<SqliteStore>>,
    process_default_session_initialized: OnceLock<()>,
    /// Fixed at construction: a read-only service never opens or returns the
    /// writable connection, so only the read words, which open the store
    /// read-only for each call, can run on it.
    read_only: bool,
    #[cfg(test)]
    delivery_stage_hook: Option<DeliveryStageTestHook>,
    #[cfg(test)]
    advisory_read_hook: Option<DeliveryStageTestHook>,
    #[cfg(test)]
    focus_children_hook: Option<DeliveryStageTestHook>,
}

impl Clone for LocalWorkService {
    fn clone(&self) -> Self {
        Self {
            database: self.database.clone(),
            project_id: self.project_id.clone(),
            actor_id: self.actor_id.clone(),
            actor_context: self.actor_context.clone(),
            actor_context_normalized: self.actor_context_normalized,
            setup_actor_context: self.setup_actor_context.clone(),
            session_id: self.session_id.clone(),
            attribution_defaults: self.attribution_defaults,
            source_skill: self.source_skill.clone(),
            read_only: self.read_only,
            // A clone is a separate protocol connection. Keeping its SQLite
            // handle independent preserves the real cross-connection CAS and
            // delivery-race semantics exercised by hosts and tests.
            cached_store: OnceLock::new(),
            process_default_session_initialized: OnceLock::new(),
            #[cfg(test)]
            delivery_stage_hook: self.delivery_stage_hook.clone(),
            #[cfg(test)]
            advisory_read_hook: self.advisory_read_hook.clone(),
            #[cfg(test)]
            focus_children_hook: self.focus_children_hook.clone(),
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
    evidence: Vec<ObjectId>,
    base_key: &'a str,
    now: DateTime<Utc>,
}

struct PreparedCompletionEvidence {
    evidence: Vec<ObjectId>,
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
pub(crate) struct WorkNoteIntent<'a> {
    #[serde(skip_serializing_if = "std::ops::Not::not")]
    pub(crate) status: bool,
    pub(crate) summary: &'a str,
    pub(crate) refs: &'a [String],
}

#[derive(Clone, Deserialize, Eq, PartialEq, Serialize)]
struct WorkProtocolBasis {
    focused_work: Option<WorkItem>,
    claim: Option<WorkClaim>,
    handoffs: Vec<WorkHandoffOffer>,
}

impl WorkProtocolBasis {
    fn completion_run_id(&self) -> Option<WorkRunId> {
        self.focused_work
            .as_ref()
            .and_then(|work| work.active_run_id)
            .or_else(|| self.claim.as_ref().map(|claim| claim.run_id))
    }

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
    basis: &'a ObjectId,
    /// Whether the basis claim is still live at call time, so a repeated
    /// call after expiry is a new attempt even though the projection bytes
    /// did not change.
    claim_live: Option<bool>,
    intent: &'a ObjectId,
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

fn update_metadata(input: &WorkUpdateInput) -> (&'static str, &'static str, &str) {
    match input {
        WorkUpdateInput::Claim {
            idempotency_key, ..
        } => ("claim", "claim_work", idempotency_key),
        WorkUpdateInput::ClaimNextReady {
            idempotency_key, ..
        } => (
            "claim_next_ready",
            "claim_next_ready_child",
            idempotency_key,
        ),
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
        WorkUpdateInput::Reject {
            idempotency_key, ..
        } => ("reject", "reject_required_child", idempotency_key),
        WorkUpdateInput::Supersede {
            idempotency_key, ..
        } => ("supersede", "dispose_work", idempotency_key),
        WorkUpdateInput::Detach {
            idempotency_key, ..
        } => ("detach", "detach_work", idempotency_key),
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

/// What the evaluation remedies of a completion refusal read: the task's
/// evaluation-mode mark and the modes the project admits.
pub(super) struct CompletionRemedyModes {
    pub(super) mark: Option<crate::domain::AcceptanceEvaluationMode>,
    pub(super) admitted: Vec<crate::domain::AcceptanceEvaluationMode>,
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

fn parse_record_ids(values: &[String]) -> Result<Vec<ObjectId>, StoreError> {
    values
        .iter()
        .map(|value| {
            ObjectId::from_str(value).map_err(|message| StoreError::InvalidWork(message.to_owned()))
        })
        .collect()
}

fn parse_record_id(value: &str) -> Result<ObjectId, StoreError> {
    ObjectId::from_str(value).map_err(|message| StoreError::InvalidWork(message.to_owned()))
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
    unlinked_keyless: bool,
) -> bool {
    if unlinked_keyless {
        let (Some(stored_work), Some(current_work), Some(current_claim)) = (
            stored.focused_work.as_ref(),
            current.focused_work.as_ref(),
            current.claim.as_ref(),
        ) else {
            return false;
        };
        // No authority is inferred from this key. Refresh only onto this
        // session's active claim in the same open run; the mutation still
        // validates the live fence, evidence, and current acceptance.
        // An earlier foreign or released claim could only have refused before
        // core execution. It must not poison this session's later valid retry.
        return stored_work.work_id == current_work.work_id
            && stored_work.lifecycle == WorkLifecycle::Open
            && current_work.lifecycle == WorkLifecycle::Open
            && stored.completion_run_id() == Some(current_claim.run_id)
            && current_work.active_run_id == Some(current_claim.run_id)
            && current_claim.work_id == current_work.work_id
            && current_claim.holder == *session_id
            && current_claim.state == WorkClaimState::Active
            && stored.claim.as_ref().is_none_or(|claim| {
                claim.work_id == stored_work.work_id && claim.run_id == current_claim.run_id
            });
    }
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
    object_kind: &str,
    object: serde_json::Value,
    position: Option<&FeedPosition>,
) -> Result<WorkChangeProjection, StoreError> {
    match object_kind {
        crate::task_sync::origin::recovery::RECOVERY_KIND => {
            let recovery: crate::task_sync::origin::recovery::RecoveryRecord =
                serde_json::from_value(object)?;
            recovery.validate()?;
            if &recovery.project_id != project_id {
                return Err(StoreError::InvalidWorkProjection(
                    "origin recovery belongs to another project".into(),
                ));
            }
            Ok(WorkChangeProjection::Visible(WorkChangeSummary {
                schema_version: SCHEMA_VERSION,
                object_kind: object_kind.into(),
                work_id: None,
                work_ref: None,
                revision: None,
                change_kind: "task_sync_origin_recovered".into(),
                summary: "New local task-sync origin recovered; predecessor history retained"
                    .into(),
                actor_id: Some(compact_text(&recovery.actor.actor_id)),
                actor_context: projected_actor_context(&recovery.actor),
                created_at: recovery.recovered_at,
            }))
        }
        crate::task_sync::origin::PRODUCING_ORIGIN_KIND => {
            let enrollment: crate::task_sync::origin::Enrollment = serde_json::from_value(object)?;
            if &enrollment.project_id != project_id {
                return Err(StoreError::InvalidWorkProjection(
                    "origin enrollment belongs to another project".into(),
                ));
            }
            Ok(WorkChangeProjection::Visible(WorkChangeSummary {
                schema_version: SCHEMA_VERSION,
                object_kind: object_kind.into(),
                work_id: None,
                work_ref: None,
                revision: None,
                change_kind: "task_sync_origin_enrolled".into(),
                summary: "Logical task-sync origin explicitly enrolled".into(),
                actor_id: Some(compact_text(&enrollment.actor.actor_id)),
                actor_context: projected_actor_context(&enrollment.actor),
                created_at: enrollment.enrolled_at,
            }))
        }
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
        "task_sync_reconciliation_hold" | "task_sync_shared_import_version" => {
            projection::project_reconciliation(store, project_id, object_kind, object)
        }
        "task_sync_import_planning" => {
            let (summary, created_at) =
                crate::storage::folder_apply_projection(object, project_id)?;
            Ok(WorkChangeProjection::Visible(WorkChangeSummary {
                schema_version: SCHEMA_VERSION,
                object_kind: object_kind.into(),
                work_id: None,
                work_ref: None,
                revision: None,
                change_kind: "task_sync_folder_apply".into(),
                summary: compact_text(&summary),
                actor_id: None,
                actor_context: None,
                created_at,
            }))
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
        "acceptance_evaluation" => {
            let evaluation = serde_json::from_value::<crate::domain::AcceptanceEvaluation>(object)?;
            let item = store.get_work_item(evaluation.work_id)?;
            if &evaluation.project_id != project_id || evaluation.root_id != item.root_id {
                return Err(StoreError::InvalidWorkProjection(
                    "acceptance evaluation crosses its work project".into(),
                ));
            }
            let passed = evaluation
                .verdicts
                .iter()
                .filter(|verdict| verdict.verdict == crate::domain::AcceptanceVerdict::Pass)
                .count();
            let summary = match evaluation.first_blocking() {
                None => format!(
                    "acceptance evaluated ({}): all {} criteria pass",
                    evaluation.mode.word(),
                    evaluation.verdicts.len()
                ),
                Some(blocking) => format!(
                    "acceptance evaluated ({}): {passed} of {} pass; {} \"{}\"",
                    evaluation.mode.word(),
                    evaluation.verdicts.len(),
                    blocking.verdict.word(),
                    compact_text(&blocking.criterion)
                ),
            };
            Ok(WorkChangeProjection::Visible(WorkChangeSummary {
                schema_version: evaluation.schema_version,
                object_kind: object_kind.into(),
                work_id: Some(evaluation.work_id),
                work_ref: Some(item.short_ref),
                revision: None,
                change_kind: "evaluated".into(),
                summary: compact_text(&summary),
                actor_id: Some(compact_text(&evaluation.evaluator.actor_id)),
                actor_context: projected_actor_context(&evaluation.evaluator),
                created_at: evaluation.created_at,
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
        "work_source_proposal" => {
            let proposal = serde_json::from_value::<crate::domain::WorkSourceProposal>(object)?;
            let item = store.get_work_item(proposal.work_id)?;
            if &proposal.project_id != project_id || proposal.root_id != item.root_id {
                return Err(StoreError::InvalidWorkProjection(
                    "source notice crosses its project".into(),
                ));
            }
            Ok(WorkChangeProjection::Visible(WorkChangeSummary {
                schema_version: proposal.schema_version,
                object_kind: object_kind.into(),
                work_id: Some(item.work_id),
                work_ref: Some(item.short_ref),
                revision: Some(proposal.notice.work_revision),
                change_kind: "source_changed".into(),
                summary: "external source changed; local work unchanged; inspect the item".into(),
                actor_id: Some(compact_text(&proposal.notice.actor.actor_id)),
                actor_context: projected_actor_context(&proposal.notice.actor),
                created_at: proposal.notice.recorded_at,
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
                    "{}; {:?} {:?}",
                    source_change_words(
                        observation.source_changed,
                        observation.reported_source_change
                    ),
                    observation.effect,
                    observation.outcome,
                )),
                actor_id: Some(compact_text(&observation.actor.actor_id)),
                actor_context: projected_actor_context(&observation.actor),
                created_at: observation.observed_at.unwrap_or(observation.recorded_at),
            }))
        }
        "unadmitted_execution_observation" => {
            let observation =
                serde_json::from_value::<crate::domain::UnadmittedExecutionObservation>(object)?;
            let item = store.get_work_item(observation.binding.work_id)?;
            if &observation.project_id != project_id {
                return Err(StoreError::InvalidWorkProjection(
                    "unadmitted observation is bound outside its project work item".into(),
                ));
            }
            Ok(WorkChangeProjection::Visible(WorkChangeSummary {
                schema_version: observation.schema_version,
                object_kind: object_kind.into(),
                work_id: Some(observation.binding.work_id),
                work_ref: Some(item.short_ref),
                revision: Some(observation.binding.work_revision),
                change_kind: unadmitted::UNADMITTED_CHANGE_KIND.into(),
                summary: compact_text(&unadmitted::delta_summary(&observation)),
                actor_id: Some(compact_text(&observation.observer.actor_id)),
                actor_context: projected_actor_context(&observation.observer),
                created_at: observation.recorded_at,
            }))
        }
        "registered_host_run" => {
            let producer = serde_json::from_value::<crate::domain::RegisteredHostRun>(object)?;
            let binding = &producer.request.registration.binding;
            let item = store.get_work_item(binding.work_id)?;
            if &producer.project_id != project_id || item.project_id != *project_id {
                return Err(StoreError::InvalidWorkProjection(
                    "registered host run is bound outside its project".into(),
                ));
            }
            Ok(WorkChangeProjection::Visible(WorkChangeSummary {
                schema_version: producer.schema_version,
                object_kind: object_kind.into(),
                work_id: Some(binding.work_id),
                work_ref: Some(item.short_ref),
                revision: Some(binding.work_revision),
                change_kind: "registered_host_run".into(),
                summary: compact_text(&format!(
                    "host-asserted registered run {:?}; input={}",
                    producer.request.run.outcome,
                    producer
                        .request
                        .input
                        .legacy_revision()
                        .unwrap_or_else(|| "source_id recorded".into())
                )),
                actor_id: Some(compact_text(&producer.actor.actor_id)),
                actor_context: projected_actor_context(&producer.actor),
                created_at: producer.recorded_at,
            }))
        }
        crate::domain::REMOTE_TERMINAL_ORIGIN_KIND => {
            let origin = serde_json::from_value::<crate::domain::RemoteTerminalOrigin>(object)?;
            if &origin.project_id != project_id {
                return Err(StoreError::InvalidWorkProjection(
                    "remote terminal origin is bound outside its project".into(),
                ));
            }
            Ok(WorkChangeProjection::Visible(WorkChangeSummary {
                schema_version: origin.schema_version,
                object_kind: object_kind.into(),
                work_id: None,
                work_ref: None,
                revision: None,
                change_kind: object_kind.into(),
                summary: compact_text(&origin.completion.headline()),
                actor_id: Some(compact_text(&origin.actor.actor_id)),
                actor_context: projected_actor_context(&origin.actor),
                created_at: origin.recorded_at,
            }))
        }
        crate::domain::REMOTE_VERIFICATION_ASSOCIATION_KIND => {
            let record = serde_json::from_value::<
                crate::domain::RemoteVerificationAssociationRecord,
            >(object)?;
            if &record.project_id != project_id {
                return Err(StoreError::InvalidWorkProjection(
                    "remote verification is bound outside its project".into(),
                ));
            }
            // A stale or unsupported delivery names a destination it never
            // reached, which may not even be an item of this project.
            let binding = &record.request.association.binding;
            let item = if record.is_in_item_scope() {
                Some(store.get_work_item(binding.work_id)?)
            } else {
                None
            };
            Ok(WorkChangeProjection::Visible(WorkChangeSummary {
                schema_version: record.schema_version,
                object_kind: object_kind.into(),
                work_id: item.as_ref().map(|item| item.work_id),
                work_ref: item.map(|item| item.short_ref),
                revision: record.is_in_item_scope().then_some(binding.work_revision),
                change_kind: object_kind.into(),
                summary: compact_text(&record.headline()),
                actor_id: Some(compact_text(&record.actor.actor_id)),
                actor_context: projected_actor_context(&record.actor),
                created_at: record.associated_at,
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
                    "{:?} {:?}; {}; environment={}",
                    evidence.check_kind,
                    evidence.result,
                    evidence.source_basis.legacy_revision().map_or_else(
                        || "source_id recorded".into(),
                        |revision| format!("source_revision={revision}")
                    ),
                    evidence
                        .environment
                        .as_ref()
                        .map_or("none", ObjectId::as_str)
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
            let (change_kind, summary) =
                if let WorkObligationResolution::Waived { waived_by, .. } = &event.resolution
                    && crate::control::is_stock_source_change_obligation(
                        &record.obligation.rule,
                        &record.obligation.requirement,
                    )
                {
                    untested_change_summary(store, &record.obligation, waived_by)?
                } else {
                    obligation_resolution_change_summary(
                        &record.obligation.rule.rule_id,
                        &event.resolution,
                    )
                };
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
        "named_root_binding" => {
            let event = serde_json::from_value::<crate::domain::NamedRootBindingEvent>(object)?;
            if &event.project_id != project_id {
                return Err(StoreError::InvalidWorkProjection(
                    "named-root event is bound outside its project".into(),
                ));
            }
            let item = store.get_work_item(event.work_id)?;
            let (change_kind, summary) = match event.kind {
                crate::domain::NamedRootBindingKind::Bound => (
                    "source_root_named",
                    format!(
                        "claim source root named in workspace {} at generation {}",
                        event.workspace_id, event.generation
                    ),
                ),
                crate::domain::NamedRootBindingKind::Ended => (
                    "source_root_ended",
                    format!(
                        "claim source root ended in workspace {} at generation {}",
                        event.workspace_id, event.generation
                    ),
                ),
            };
            Ok(WorkChangeProjection::Visible(WorkChangeSummary {
                schema_version: event.schema_version,
                object_kind: object_kind.into(),
                work_id: Some(event.work_id),
                work_ref: Some(item.short_ref),
                revision: None,
                change_kind: change_kind.into(),
                summary: compact_text(&summary),
                actor_id: Some(compact_text(&event.actor.actor_id)),
                actor_context: projected_actor_context(&event.actor),
                created_at: event.recorded_at,
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
        other => Err(StoreError::InvalidWorkProjection(format!(
            "project work feed contains unsupported agent object kind {other:?}"
        ))),
    }
}

/// The change an execution observation reports, first in a peer's delta so
/// that the bounded line keeps it: whether the source changed and, when the
/// host said, how it established that, in one word each (`compared`,
/// `assumed`, `watcher` for the protocol's three values). A report the core
/// read as a repeated revision keeps the host's word beside that reading.
fn source_change_words(
    source_changed: bool,
    reported: Option<crate::SourceChangeDetection>,
) -> String {
    let word = |detection: crate::SourceChangeDetection| match detection {
        crate::SourceChangeDetection::ContentComparison => "compared",
        crate::SourceChangeDetection::AssumedMissingBaseline => "assumed",
        crate::SourceChangeDetection::WatcherOnly => "watcher",
    };
    match (source_changed, reported) {
        (true, Some(detection)) => format!("changed: {}", word(detection)),
        (true, None) => "changed".into(),
        (false, Some(detection)) => format!("repeat; host: {}", word(detection)),
        (false, None) => "unchanged".into(),
    }
}

/// A waived obligation of the stock source-change rule records a change no
/// matching passing test followed; peers read it as that change, not as a
/// human exception.
fn untested_change_summary(
    store: &SqliteStore,
    obligation: &crate::WorkObligation,
    waived_by: &str,
) -> Result<(&'static str, String), StoreError> {
    let change = store.source_observation(&obligation.triggering_observation)?;
    let revision = change.source_basis.map_or_else(
        || "no recorded source revision".to_owned(),
        |basis| basis.identity.description(),
    );
    let said = change
        .reported_source_change
        .map_or_else(String::new, |detection| {
            format!("host said {}; ", detection.as_str())
        });
    Ok((
        "untested_source_change",
        format!(
            "{said}{} ({revision}); waiver attributed to {}",
            change.label,
            compact_text(waived_by)
        ),
    ))
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
        WorkObligationResolution::Displaced {
            trigger_workspace_id,
            ..
        } => (
            "foreign_workspace_change",
            format!(
                "{rule_id} change in workspace {} was displaced by a later named root; no test satisfaction or waiver was recorded",
                compact_text(trigger_workspace_id)
            ),
        ),
    }
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

fn agent_update_receipt(
    operation: &str,
    receipt: serde_json::Value,
) -> Result<serde_json::Value, StoreError> {
    if operation == "reject" {
        let rejected: crate::RejectRequiredChildReceipt = serde_json::from_value(receipt)?;
        return Ok(serde_json::json!({
            "work_id": rejected.child.work_id,
            "revision": rejected.child.revision,
            "lifecycle": rejected.child.lifecycle,
            "parent_ref": rejected.parent_ref,
            "required_child_waived": true,
        }));
    }
    if operation == "claim_next_ready" {
        // One flat claim receipt, shaped like an ordinary claim's, plus the
        // selection it came from.
        let selection: crate::domain::NextReadyChildClaim = serde_json::from_value(receipt)?;
        let mut flat = serde_json::to_value(&selection.claim)?;
        flat["position"] = serde_json::json!(selection.position);
        flat["ready_count"] = serde_json::json!(selection.ready_count);
        flat["renewed"] = serde_json::json!(selection.renewed);
        return Ok(flat);
    }
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
        _ => Err(StoreError::InvalidWork(MULTIPLE_BLOCKERS_REFUSAL.into())),
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
