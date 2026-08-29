//! Engram's local working-memory core.
//!
//! The crate deliberately separates local operational state from publication:
//! agents work against local immutable records, while a frozen report crosses
//! the external tracker boundary only through a receipted adapter call.

pub mod canonical;
pub mod control;
pub mod domain;
pub mod host;
pub mod mcp;
pub mod memory;
pub mod project;
pub mod storage;
pub mod tracker;
pub mod work_service;

pub use canonical::{CanonicalObject, ObjectHash};
pub use control::{
    ObligationSatisfactionInput, VerificationEvidenceMatchInput, evaluate_builtin_obligation_rules,
    evaluate_obligation_satisfaction, evaluate_turn_begin, evaluate_turn_checkpoint,
    match_verification_evidence, observe_action_begin, observe_turn,
};
pub use domain::{
    AcceptWorkHandoffRequest, AcceptanceResult, ActionBeginDecision, ActionBeginSnapshot,
    ActionGrantBasis, ActionGrantState, ActorContext, AddWorkBlockerRequest, Authority,
    AuthorityState, BuiltinObligationRuleRef, COMPLETION_OBLIGATION_SCHEMA_VERSION,
    CONTROL_SCHEMA_VERSION, CancelWorkHandoffRequest, ChangeCursor, ChangeWorkPrerequisiteRequest,
    CheckpointWorkRequest, ChildRequirement, ChildWorkDraft, ChildWorkPrerequisite,
    ClaimWorkRequest, ClearWorkBlockerRequest, CompleteWorkRequest, CompletionDrainAttestation,
    CompletionObligationBinding, CompletionSeal, CompletionWaiver, ContextItem, ContextOmission,
    ContextOmissionSummary, ContextPacket, ContextPacketHeader, ContextPacketPayload,
    ControlAssurance, ControlDeferCode, ControlDeferral, ControlDelivery, ControlDirective,
    ControlEpochs, ControlHealth, ControlPolicy, ControlRefusalCode, ControlSessionBinding,
    ControlSessionStatus, ControlTurnBeginDecision, ControlTurnCheckpointDecision,
    ControlTurnDecision, ControlWorkBinding, CreateWorkRequest, DecomposeWorkRequest, Delivery,
    DeliveryPage, DeltaItem, DirectiveSatisfaction, DirectiveTarget, DisposeWorkRequest,
    EffectClass, EnvironmentEvidence, EnvironmentEvidenceInput, ExecutionObservation,
    ExecutionObservationInput, ExecutionObservationReference, ExecutionOutcome,
    ExecutionSourceBasis, FeedId, FeedPosition, FinalizationBarrier, FrozenReport, HostPathPolicy,
    IssuedTurnGrant, LeaseBasis, LeaseKind, LeaseMode, LifecycleAuthorityDecision, LocalTask,
    MemoryContradictionEvent, MemoryContradictionReceipt, MemoryId, MemoryKind, MemoryRecord,
    MemoryStatus, MemorySummary, MemoryVersion, NoteReceipt, NoteRequest, NoteVisibility,
    ObservedActionBeginDecision, ObservedTurnDecision, OfferWorkHandoffRequest, OpenWorkObligation,
    PacketSafety, ParentTurnState, ParticipantMembership, ParticipantReadiness, ProjectId,
    ProjectPolicyAuthorityDecision, ProjectPolicyEpoch, ProjectPolicyOperation, ReadyWork,
    RecordWorkEvidenceRequest, ReleaseWorkRequest, ReopenWorkRequest, RequiredChildWaiver,
    ResolutionAssurance, ResourceCoverage, ResourceSubject, ReviseWorkRequest, RootContribution,
    RootExecution, RootExecutionId, RootExecutionState, Scope, Sensitivity, SessionId,
    SessionPhase, TaskAdmissionEpoch, TaskBindReceipt, TaskDelta, TaskId, TaskLease, TaskState,
    TurnBeginDecision, TurnBeginReceipt, TurnBeginSnapshot, TurnCheckpointDecision,
    TurnCheckpointEvent, TurnCheckpointReceipt, TurnCheckpointSnapshot, TurnDecision,
    TurnEvaluationInput, TurnGrantBasis, TurnGrantState, TurnIntent, TurnNextIntent, TurnPurpose,
    VerificationEvidence, VerificationEvidenceInput, VerificationEvidenceMismatch,
    VerificationKind, VerificationRequirement, VerificationResult, WaiveRequiredChildRequest,
    WaiveWorkObligationRequest, WorkAuthorityGrant, WorkAuthorityOperation,
    WorkAuthorityRevocation, WorkAuthorityScope, WorkAvailability, WorkBlocker, WorkBlockerKind,
    WorkCatalogPage, WorkCatalogQuery, WorkCheckpoint, WorkClaim, WorkClaimId, WorkClaimState,
    WorkDecomposition, WorkDependencyRef, WorkDisposition, WorkEvent, WorkEvidence,
    WorkEvidenceKind, WorkFeedEntry, WorkHandoffOffer, WorkHandoffOfferId, WorkHandoffState,
    WorkId, WorkItem, WorkItemKind, WorkLease, WorkLeaseDecision, WorkLeaseEvent,
    WorkLeaseReleaseReceipt, WorkLeaseTransition, WorkLifecycle, WorkObligation, WorkObligationId,
    WorkObligationResolution, WorkObligationResolutionEvent, WorkObligationState, WorkOrigin,
    WorkPlanningAuthority, WorkPlanningBudget, WorkReadinessReason, WorkRevisionPatch, WorkRun,
    WorkRunId, WorkRunState, WorkSessionState, WorkSourceProjection, WorkSourceSnapshot,
    WorkTransition,
};
pub use host::{HostControlRequest, HostControlServer};
pub use mcp::McpServer;
pub use memory::{DevelopmentNoopRedactor, Redactor};
pub use project::project_database_path;
pub use storage::{
    ControlDiagnostics, ControlPolicyUpdateReceipt, IntegrityReport, SqliteStore, TaskChange,
};
pub use tracker::{DummyTrackerAdapter, PublicationReceipt, TrackerAdapter};
pub use work_service::{
    LocalWorkService, WorkAcceptanceInput, WorkChange, WorkChangeOmission,
    WorkChangeOmissionReason, WorkChangeProjection, WorkChildInput, WorkCompleteInput,
    WorkCompleteRefusal, WorkCompleteResult, WorkCompletedReceipt, WorkCompletionCaptureInput,
    WorkEvidenceAttachInput, WorkEvidenceSummary, WorkFocusView, WorkHandoffInput,
    WorkHandoffResult, WorkNextQuery, WorkNextSection, WorkNextView, WorkPrerequisiteInput,
    WorkProposeInput, WorkProposeResult, WorkUpdateInput, WorkUpdateResult,
};
