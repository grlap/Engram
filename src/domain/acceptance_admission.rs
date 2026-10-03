//! Transient, core-decided evaluation admission failures; never stored verdicts.

use serde::{Deserialize, Serialize};

use super::{
    AcceptanceEvaluationMode, AcceptanceVerdict, FeedId, SessionId, VerificationRequirement,
    WorkRunId,
};
use crate::ObjectId;

/// Action selected by the deciding admission rule, not inferred from its prose.
#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationAdmissionRemedy {
    UseSelfAssertedCompletion,
    RequestEligibleEvaluation,
    InspectEvaluatorBinding,
    CaptureRootAndEvaluate,
    EvaluateNamedRoot,
    ReadRunEvidence,
    ReadCurrentCut,
    RunCurrentCheckAndEvaluate,
    RecordNewEvidenceThenEvaluate,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationEligibilityMismatch {
    EvaluationDisabled,
    ModeDisallowed,
    TaskPinMismatch,
    SameSessionNotExecuting,
    SubAgentParentNotExecuting,
    IndependentEvaluatorAffiliated,
    SubAgentEvaluatorAffiliated,
    SameSessionUnmarked,
    MarkAuthorUnrecorded,
    MarkAuthorAffiliated,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationRootMismatch {
    NoInitialSighting,
    DeclaredWorkspaceMismatch,
    JudgedSourceMismatch,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationCitationMismatch {
    NotOnRun,
    BeyondCut,
    ObservedBasisRequired,
    PassedVerificationRequired,
    ObservedPolicyRequired,
    PassingGateRequired,
    BoundVerificationMismatch,
    WrongSource,
    SourceMovedAfterCheck,
    UnverifiableSource,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct EligibilityAdmissionCause {
    pub mismatch: EvaluationEligibilityMismatch,
    pub requested_mode: AcceptanceEvaluationMode,
    pub task_mark: Option<AcceptanceEvaluationMode>,
    pub admitted_modes: Vec<AcceptanceEvaluationMode>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub evaluator: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub parent: Option<SessionId>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub mark_author: Option<SessionId>,
    pub remedy: EvaluationAdmissionRemedy,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct SourceRootAdmissionCause {
    pub mismatch: EvaluationRootMismatch,
    pub root_binding: ObjectId,
    pub workspace_id: String,
    pub evaluated_cut: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared_workspace_id: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub declared_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub reported_revision: Option<String>,
    pub remedy: EvaluationAdmissionRemedy,
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct CitationAdmissionCause {
    pub mismatch: EvaluationCitationMismatch,
    pub criterion: usize,
    /// Submitted locator or record id, not proof of evidence on another run.
    /// Empty when the deciding fault is the verdict's basis
    /// (`observed_basis_required`, `observed_policy_required`): no citation
    /// was at fault.
    pub citation: String,
    pub run_id: WorkRunId,
    pub evaluated_cut: i64,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub citation_position: Option<i64>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requirement: Option<VerificationRequirement>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub checked_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub judged_revision: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub producer_observation: Option<ObjectId>,
    pub remedy: EvaluationAdmissionRemedy,
}

#[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum EvaluationRerollMismatch {
    BlockingEvaluationStands,
}

/// A blocking evaluation that stands on its run: nothing that could change
/// it was recorded after its evidence basis and within the one assessed.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
pub struct RerollAdmissionCause {
    pub mismatch: EvaluationRerollMismatch,
    /// Record id of the newest evaluation on the run, the one that blocks.
    pub evaluation: ObjectId,
    /// The run feed the positions belong to.
    pub feed: FeedId,
    /// The blocking evaluation's evidence basis; evidence must lie after it.
    pub after_position: i64,
    /// The last position assessed, inclusive: the submitted evidence basis
    /// at record time, the run feed's head in status.
    pub through_position: i64,
    /// One-based position of the criterion that blocks: the first that
    /// failed, else the first with insufficient evidence, else the first that
    /// needs a human.
    pub criterion: usize,
    pub verdict: AcceptanceVerdict,
    pub remedy: EvaluationAdmissionRemedy,
}

impl RerollAdmissionCause {
    /// What clears a standing blocking evaluation, and what does not.
    pub const REMEDY: &str = "record the correction or the new evidence first (a note, a gate, a host check or a source change), then evaluate on a basis that includes it; another evaluation, a claim, a checkpoint, or an edit of the title, the mode or the policy does not count";
}

/// The family and its deciding context, beside unchanged human refusal text.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AcceptanceEvaluationAdmissionCause {
    Eligibility(Box<EligibilityAdmissionCause>),
    SourceRoot(Box<SourceRootAdmissionCause>),
    Citation(Box<CitationAdmissionCause>),
    Reroll(Box<RerollAdmissionCause>),
}
