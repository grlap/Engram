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
pub mod tracker;
pub mod verbs;
pub mod work_service;

pub use canonical::{CanonicalObject, ObjectHash};
pub use control::{
    ObligationSatisfactionInput, VerificationEvidenceMatchInput, builtin_obligation_rule_set,
    evaluate_obligation_rules, evaluate_obligation_satisfaction, evaluate_turn_begin,
    evaluate_turn_checkpoint, match_verification_evidence, observe_action_begin, observe_turn,
};
pub use domain::{
    AcceptWorkHandoffRequest, AcceptanceResult, ActionBeginDecision, ActionBeginSnapshot,
    ActionGrantBasis, ActionGrantState, ActorContext, AddWorkBlockerRequest, Authority,
    AuthorityState, BuiltinObligationRuleRef, BuiltinObligationTrigger,
    COMPLETION_ENVIRONMENT_SCHEMA_VERSION, COMPLETION_OBLIGATION_SCHEMA_VERSION,
    CONTROL_SCHEMA_VERSION, CancelWorkHandoffRequest, ChangeCursor, ChangeWorkPrerequisiteRequest,
    CheckpointWorkRequest, ChildRequirement, ChildWorkDraft, ChildWorkPrerequisite,
    ClaimWorkRequest, ClearWorkBlockerRequest, CompleteWorkRequest, CompletionDrainAttestation,
    CompletionObligationBinding, CompletionSeal, CompletionWaiver, ContextItem, ContextOmission,
    ContextOmissionSummary, ContextPacket, ContextPacketHeader, ContextPacketPayload,
    ControlAssurance, ControlDeferCode, ControlDeferral, ControlDelivery, ControlDirective,
    ControlEpochs, ControlHealth, ControlPolicy, ControlRefusalCode, ControlSessionBinding,
    ControlSessionStatus, ControlTurnBeginDecision, ControlTurnCheckpointDecision,
    ControlTurnDecision, ControlWorkBinding, CreateWorkRequest, DEFAULT_WORK_CLAIM_TTL_SECONDS,
    DecomposeWorkRequest, Delivery, DeliveryPage, DeltaItem, DirectiveSatisfaction,
    DirectiveTarget, DisposeWorkRequest, EffectClass, EnvironmentComponents, EnvironmentEvidence,
    EnvironmentEvidenceInput, EnvironmentEvidenceReference, ExecutionObservation,
    ExecutionObservationInput, ExecutionObservationReference, ExecutionOutcome,
    ExecutionSourceBasis, FeedId, FeedPosition, FinalizationBarrier, ForgetProjectMemoryRequest,
    FrozenReport, GateEvidenceRecord, HostPathPolicy, IssuedTurnGrant, LeaseBasis, LeaseKind,
    LeaseMode, LocalTask, MemoryContradictionEvent, MemoryContradictionReceipt, MemoryId,
    MemoryKind, MemoryRecord, MemoryStatus, MemorySummary, MemoryVersion, NoteReceipt, NoteRequest,
    NoteVisibility, OBLIGATION_RULE_SET_SCHEMA_VERSION, ObligationRuleDefinition,
    ObligationRuleSet, ObservedActionBeginDecision, ObservedTurnDecision, OfferWorkHandoffRequest,
    OpenWorkObligation, PacketSafety, ParentTurnState, ParticipantMembership, ParticipantReadiness,
    ProjectId, ProjectMemoryFull, ProjectMemoryList, ProjectMemoryListRow,
    ProjectMemoryMutationReceipt, ProjectPolicyAuthorityDecision, ProjectPolicyEpoch,
    ProjectPolicyOperation, ReadyWork, RecordWorkEvidenceRequest, ReleaseWorkRequest,
    RememberProjectMemoryRequest, ReopenWorkRequest, RequiredChildWaiver, ResolutionAssurance,
    ResourceCoverage, ResourceSubject, ReviseWorkRequest, RootContribution, RootExecution,
    RootExecutionId, RootExecutionState, Scope, Sensitivity, SessionId, SessionPhase,
    TaskAdmissionEpoch, TaskBindReceipt, TaskDelta, TaskId, TaskLease, TaskState,
    TurnBeginDecision, TurnBeginReceipt, TurnBeginSnapshot, TurnCheckpointDecision,
    TurnCheckpointEvent, TurnCheckpointReceipt, TurnCheckpointSnapshot, TurnDecision,
    TurnEvaluationInput, TurnGrantBasis, TurnGrantState, TurnGrantSupersession,
    TurnGrantSupersessionReason, TurnIntent, TurnNextIntent, TurnPurpose, VerificationEvidence,
    VerificationEvidenceInput, VerificationEvidenceMismatch, VerificationKind,
    VerificationRequirement, VerificationResult, WaiveRequiredChildRequest,
    WaiveWorkObligationRequest, WorkAvailability, WorkBlocker, WorkBlockerKind, WorkCatalogPage,
    WorkCatalogQuery, WorkCheckpoint, WorkClaim, WorkClaimId, WorkClaimState,
    WorkCompletionRecovery, WorkCompletionRecoveryCause, WorkDecomposition, WorkDependencyRef,
    WorkDisposition, WorkEvent, WorkEvidence, WorkEvidenceKind, WorkFeedEntry, WorkHandoffOffer,
    WorkHandoffOfferId, WorkHandoffState, WorkId, WorkItem, WorkItemKind, WorkLease,
    WorkLeaseDecision, WorkLeaseEvent, WorkLeaseReleaseReceipt, WorkLeaseTransition, WorkLifecycle,
    WorkObligation, WorkObligationId, WorkObligationResolution, WorkObligationResolutionEvent,
    WorkObligationState, WorkObligationWaiverDecision, WorkObligationWaiverReceipt,
    WorkObligationWaiverRefusalCode, WorkOrigin, WorkPlanningAuthority, WorkPrerequisiteState,
    WorkReadinessReason, WorkReferenceCandidate, WorkRevisionPatch, WorkRun, WorkRunId,
    WorkRunState, WorkSessionState, WorkSourceProjection, WorkSourceSnapshot, WorkTransition,
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
    BackupManifest, ControlDiagnostics, ControlPolicyRecoveryFinding, ControlPolicyRecoveryReport,
    ControlPolicyUpdateReceipt, IntegrityReport, ObligationRuleSetUpdateReceipt, SqliteStore,
    StoreError, TaskChange, describe_host_path_policy, install_store_copy_without_replacing,
};
pub use tracker::{DummyTrackerAdapter, PublicationReceipt, TrackerAdapter};
pub use verbs::{
    AddInput, AgentVerbs, ClaimInput, DoneInput, ForgetInput, GateInput, Guidance, HandoffAction,
    HandoffInput, LsInput, MemoriesInput, NextInput, NoteInput, Receipt, RememberInput,
    UpdateAction, UpdateInput, VerbError, looks_like_work_ref, parse_defer_date,
};
pub use work_service::{
    LocalWorkService, ProjectMemorySignal, WorkAcceptanceInput, WorkActorDefaultSource,
    WorkAttributionDefaults, WorkChange, WorkChangeOmission, WorkChangeOmissionReason,
    WorkChangeProjection, WorkChildInput, WorkCompleteInput, WorkCompleteRefusal,
    WorkCompleteResult, WorkCompletedReceipt, WorkCompletionCaptureInput, WorkEvidenceAttachInput,
    WorkEvidenceSummary, WorkFocusView, WorkHandoffInput, WorkHandoffResult, WorkNextQuery,
    WorkNextSection, WorkNextView, WorkObligationGuidance, WorkObligationPage,
    WorkObligationSummary, WorkPrerequisiteInput, WorkProposeInput, WorkProposeResult,
    WorkUpdateInput, WorkUpdateResult, new_process_default_work_session_id,
};
