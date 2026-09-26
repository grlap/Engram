//! Engram's local working-memory core.
//!
//! The crate deliberately separates local operational state from publication:
//! agents work against local immutable records, while a frozen report crosses
//! the external tracker boundary only through a receipted adapter call.

pub mod build_identity;
pub mod canonical;
pub mod control;
pub mod domain;
pub mod graph_snapshot;
pub mod host;
pub mod mcp;
pub mod memory;
pub mod project;
pub mod schema;
pub mod storage;
pub mod verbs;
pub mod work_service;

#[cfg(test)]
mod test_support;

pub use canonical::{CanonicalObject, ObjectId};
pub use control::{
    ObligationSatisfactionInput, VerificationEvidenceMatchInput, builtin_obligation_rule_set,
    evaluate_obligation_rules, evaluate_obligation_satisfaction, evaluate_turn_begin,
    evaluate_turn_checkpoint, match_verification_evidence, observe_turn,
};
pub use domain::{
    AcceptWorkHandoffRequest, AcceptanceBasis, AcceptanceEvaluation, AcceptanceEvaluationMode,
    AcceptanceEvaluationPolicy, AcceptanceResult, AcceptanceSourceBasis, AcceptanceStaleReason,
    AcceptanceVerdict, ActorContext, AddWorkBlockerRequest, Authority, BuiltinObligationRuleRef,
    BuiltinObligationTrigger, COMPLETION_ENVIRONMENT_SCHEMA_VERSION,
    COMPLETION_OBLIGATION_SCHEMA_VERSION, CONTROL_SCHEMA_VERSION, CancelWorkHandoffRequest,
    ChangeCursor, ChangeWorkPrerequisiteRequest, CheckpointWorkRequest, ChildRequirement,
    ChildWorkDraft, ChildWorkPrerequisite, ClaimWorkRequest, ClearWorkBlockerRequest,
    CompleteWorkRequest, CompletionDrainAttestation, CompletionObligationBinding, CompletionSeal,
    CompletionWaiver, ContextItem, ContextOmission, ContextOmissionSummary, ContextPacket,
    ContextPacketHeader, ContextPacketPayload, ControlAssurance, ControlDelivery, ControlDirective,
    ControlEpochs, ControlPolicy, ControlRefusalCode, ControlSessionBinding, ControlSessionStatus,
    ControlTurnBeginDecision, ControlTurnCheckpointDecision, ControlTurnDecision,
    ControlWorkBinding, CreateWorkRequest, DEFAULT_WORK_CLAIM_TTL_SECONDS, DecomposeWorkRequest,
    Delivery, DeliveryPage, DeltaItem, DetachWorkRequest, DirectiveSatisfaction, DirectiveTarget,
    DisposeWorkRequest, EffectClass, EnvironmentComponents, EnvironmentEvidence,
    EnvironmentEvidenceInput, EnvironmentEvidenceReference, ExecutionObservation,
    ExecutionObservationInput, ExecutionObservationReference, ExecutionOutcome,
    ExecutionSourceBasis, FeedId, FeedPosition, ForgetProjectMemoryRequest, GateEvidenceRecord,
    HostPathPolicy, IssuedTurnGrant, MAX_SESSION_ID_BYTES, MemoryId, MemoryKind, MemoryRecord,
    MemoryStatus, MemorySummary, MemoryVersion, NoteReceipt, NoteRequest, NoteVisibility,
    OBLIGATION_RULE_SET_SCHEMA_VERSION, ObligationRuleDefinition, ObligationRuleSet,
    ObservedTurnDecision, OfferWorkHandoffRequest, OpenWorkObligation, PLAIN_READY_REASON,
    ParticipantMembership, ProjectId, ProjectMemoryFull, ProjectMemoryList, ProjectMemoryListRow,
    ProjectMemoryMutationReceipt, ProjectPolicyAuthorityDecision, ProjectPolicyEpoch,
    ProjectPolicyOperation, ReadyWork, RecordWorkEvidenceRequest, RejectRequiredChildReceipt,
    RejectRequiredChildRequest, ReleaseWorkRequest, RememberProjectMemoryRequest,
    ReopenWorkRequest, RequiredChildResolution, RequiredChildWaiver, ResourceCoverage,
    ResourceSubject, ReviseWorkRequest, RootContribution, RootExecution, RootExecutionId,
    RootExecutionState, Scope, Sensitivity, SessionId, SessionIdAdmissionError, SessionPhase,
    TaskAdmissionEpoch, TaskDelta, TaskId, TurnBeginDecision, TurnBeginReceipt, TurnBeginSnapshot,
    TurnCheckpointDecision, TurnCheckpointEvent, TurnCheckpointReceipt, TurnCheckpointSnapshot,
    TurnDecision, TurnEvaluationInput, TurnGrantBasis, TurnGrantState, TurnGrantSupersession,
    TurnGrantSupersessionReason, TurnIntent, TurnNextIntent, TurnPurpose, VerificationEvidence,
    VerificationEvidenceInput, VerificationEvidenceMismatch, VerificationKind,
    VerificationRequirement, VerificationResult, WaiveRequiredChildRequest,
    WaiveWorkObligationRequest, WorkAvailability, WorkBlocker, WorkBlockerKind, WorkCatalogPage,
    WorkCatalogQuery, WorkCheckpoint, WorkClaim, WorkClaimId, WorkClaimState,
    WorkCompletionRecovery, WorkCompletionRecoveryCause, WorkDecomposition, WorkDependencyRef,
    WorkDisposition, WorkEvent, WorkEvidence, WorkEvidenceKind, WorkFeedEntry, WorkHandoffOffer,
    WorkHandoffOfferId, WorkHandoffState, WorkId, WorkItem, WorkItemKind, WorkLifecycle,
    WorkObligation, WorkObligationId, WorkObligationResolution, WorkObligationResolutionEvent,
    WorkObligationState, WorkOrigin, WorkPlanningAuthority, WorkPrerequisiteState,
    WorkReadinessReason, WorkReferenceCandidate, WorkRevisionPatch, WorkRun, WorkRunId,
    WorkRunState, WorkSessionState, WorkSourceProjection, WorkSourceSnapshot, WorkTransition,
    validate_session_id_length,
};
pub use graph_snapshot::{
    RestoredRecord, RestoredRelationBasis, RestoredWorkEvidence,
    WORK_GRAPH_SNAPSHOT_SCHEMA_VERSION, WorkGraphSnapshotBlocker, WorkGraphSnapshotBody,
    WorkGraphSnapshotCompletion, WorkGraphSnapshotCut, WorkGraphSnapshotDestinationKind,
    WorkGraphSnapshotDocument, WorkGraphSnapshotEvent, WorkGraphSnapshotExport,
    WorkGraphSnapshotGate, WorkGraphSnapshotHistory, WorkGraphSnapshotItem,
    WorkGraphSnapshotLifecycleCounts, WorkGraphSnapshotLoadPreview, WorkGraphSnapshotLoadResult,
    WorkGraphSnapshotLoadedEvent, WorkGraphSnapshotManifest, WorkGraphSnapshotMemory,
    WorkGraphSnapshotMemoryState, WorkGraphSnapshotNote, WorkGraphSnapshotRecord,
    WorkGraphSnapshotRecordPayload, WorkGraphSnapshotRedactedCounts, WorkGraphSnapshotSavedEvent,
    WorkGraphSnapshotSectionCounts, WorkGraphSnapshotSource, WorkGraphSnapshotSummary,
    WorkGraphSnapshotText, graph_snapshot_files_are_equivalent, parse_work_graph_snapshot_document,
    work_graph_snapshot_exporting_build, work_graph_snapshot_format_fingerprint,
};
pub use host::{HostControlRequest, HostControlServer};
pub use mcp::{McpServer, store_error_value};
pub use memory::{DevelopmentNoopRedactor, Redactor};
pub use project::{
    HostPathProbeError, parse_host_path_policy, probe_host_path_policy, project_database_path,
};
pub use storage::{
    AcceptanceEvaluationPolicyUpdateReceipt, AcceptanceEvaluationReceipt,
    AcceptanceEvaluationStatus, BackupManifest, ControlDiagnostics, ControlPolicyRecoveryFinding,
    ControlPolicyRecoveryReport, ControlPolicyUpdateReceipt, EvaluationBasisMove, IntegrityReport,
    ObligationRuleSetUpdateReceipt, SqliteStore, StoreError, describe_host_path_policy,
    install_store_copy_without_replacing,
};
pub use verbs::{
    AddInput, AgentVerbs, ClaimInput, ClaimUnderInput, DoneInput, EvaluateInput, ForgetInput,
    GateInput, Guidance, HandoffAction, HandoffInput, LsInput, MemoriesInput, NextInput, NoteInput,
    Receipt, RememberInput, UpdateAction, UpdateInput, VerbError, looks_like_work_ref,
    parse_defer_date,
};
pub use work_service::{
    LocalWorkService, ProjectMemorySignal, UntestedSourceChange, WorkAcceptanceInput,
    WorkActorDefaultSource, WorkAttributionDefaults, WorkChange, WorkChangeOmission,
    WorkChangeOmissionReason, WorkChangeProjection, WorkChildInput, WorkCompleteInput,
    WorkCompleteRefusal, WorkCompleteResult, WorkCompletedReceipt, WorkCompletionCaptureInput,
    WorkCriterionVerdictInput, WorkEvaluateInput, WorkEvaluateResult, WorkEvidenceAttachInput,
    WorkEvidenceSummary, WorkFocusView, WorkHandoffInput, WorkHandoffResult, WorkHeldClaim,
    WorkHeldView, WorkInspectView, WorkNextQuery, WorkNextSection, WorkNextView,
    WorkObligationGuidance, WorkObligationPage, WorkObligationSummary, WorkPrerequisiteInput,
    WorkProposeInput, WorkProposeResult, WorkUpdateInput, WorkUpdateResult,
    new_process_default_work_session_id, terminal_error_command, terminal_error_line,
};
