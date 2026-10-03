//! Transient context captured at the existing admission decision points.

use super::{
    AcceptanceEvaluationMode, AcceptanceEvaluationPolicy, NamedEvaluationRoot, SessionId,
    StoreError, WorkId, WorkItem, WorkRunId,
};
use crate::domain::{
    AcceptanceEvaluationAdmissionCause, CitationAdmissionCause, EligibilityAdmissionCause,
    EvaluationAdmissionRemedy, EvaluationCitationMismatch, EvaluationEligibilityMismatch,
    EvaluationRootMismatch, SourceRootAdmissionCause,
};

pub(super) fn refusal(
    work: WorkId,
    reason: impl Into<String>,
    cause: AcceptanceEvaluationAdmissionCause,
) -> StoreError {
    StoreError::AcceptanceEvaluationAdmissionRefused {
        work,
        reason: reason.into(),
        cause: Box::new(cause),
    }
}

pub(super) struct EligibilityContext<'a> {
    pub item: &'a WorkItem,
    pub policy: &'a AcceptanceEvaluationPolicy,
    pub mode: AcceptanceEvaluationMode,
    pub evaluator: Option<&'a SessionId>,
    pub parent: Option<&'a SessionId>,
}

impl EligibilityContext<'_> {
    pub fn refused(
        &self,
        mismatch: EvaluationEligibilityMismatch,
        reason: impl Into<String>,
        mark_author: Option<SessionId>,
    ) -> StoreError {
        // Every mismatch is mapped here by name, so a new one must be given
        // its remedy deliberately.
        let remedy = match mismatch {
            EvaluationEligibilityMismatch::EvaluationDisabled => {
                EvaluationAdmissionRemedy::UseSelfAssertedCompletion
            }
            // The evaluator or its parent is bound wrongly to the run: the
            // binding is what to inspect.
            EvaluationEligibilityMismatch::SameSessionNotExecuting
            | EvaluationEligibilityMismatch::SubAgentParentNotExecuting
            | EvaluationEligibilityMismatch::IndependentEvaluatorAffiliated
            | EvaluationEligibilityMismatch::SubAgentEvaluatorAffiliated => {
                EvaluationAdmissionRemedy::InspectEvaluatorBinding
            }
            // The mode, the mark or who set it rules this evaluation out:
            // another, eligible evaluation is the way on.
            EvaluationEligibilityMismatch::ModeDisallowed
            | EvaluationEligibilityMismatch::TaskPinMismatch
            | EvaluationEligibilityMismatch::SameSessionUnmarked
            | EvaluationEligibilityMismatch::MarkAuthorUnrecorded
            | EvaluationEligibilityMismatch::MarkAuthorAffiliated => {
                EvaluationAdmissionRemedy::RequestEligibleEvaluation
            }
        };
        refusal(
            self.item.work_id,
            reason,
            AcceptanceEvaluationAdmissionCause::Eligibility(Box::new(EligibilityAdmissionCause {
                mismatch,
                requested_mode: self.mode,
                task_mark: self.item.evaluation_mode,
                admitted_modes: self.policy.allowed_modes.clone(),
                evaluator: self.evaluator.cloned(),
                parent: self.parent.cloned(),
                mark_author,
                remedy,
            })),
        )
    }
}

pub(super) struct SameSessionRefusal {
    pub reason: String,
    pub mismatch: EvaluationEligibilityMismatch,
    pub mark_author: Option<SessionId>,
}

pub(super) struct CitationContext<'a> {
    pub item: &'a WorkItem,
    pub run_id: WorkRunId,
    pub cut: i64,
    pub criterion: usize,
    pub citation: &'a str,
    pub position: Option<i64>,
}

impl CitationContext<'_> {
    pub fn cause(&self, mismatch: EvaluationCitationMismatch) -> CitationAdmissionCause {
        CitationAdmissionCause {
            mismatch,
            criterion: self.criterion,
            citation: self.citation.into(),
            run_id: self.run_id,
            evaluated_cut: self.cut,
            citation_position: self.position,
            requirement: self
                .item
                .acceptance_bindings
                .iter()
                .find(|binding| binding.criterion == self.criterion)
                .map(|binding| binding.requirement.clone()),
            checked_revision: None,
            judged_revision: None,
            producer_observation: None,
            // Every mismatch is mapped here by name, so a new one must be
            // given its remedy deliberately.
            remedy: match mismatch {
                EvaluationCitationMismatch::BeyondCut => EvaluationAdmissionRemedy::ReadCurrentCut,
                EvaluationCitationMismatch::WrongSource
                | EvaluationCitationMismatch::SourceMovedAfterCheck
                | EvaluationCitationMismatch::UnverifiableSource => {
                    EvaluationAdmissionRemedy::RunCurrentCheckAndEvaluate
                }
                EvaluationCitationMismatch::NotOnRun
                | EvaluationCitationMismatch::ObservedBasisRequired
                | EvaluationCitationMismatch::PassedVerificationRequired
                | EvaluationCitationMismatch::ObservedPolicyRequired
                | EvaluationCitationMismatch::PassingGateRequired
                | EvaluationCitationMismatch::BoundVerificationMismatch => {
                    EvaluationAdmissionRemedy::ReadRunEvidence
                }
            },
        }
    }

    pub fn refused(
        &self,
        mismatch: EvaluationCitationMismatch,
        reason: impl Into<String>,
    ) -> StoreError {
        refusal(
            self.item.work_id,
            reason,
            AcceptanceEvaluationAdmissionCause::Citation(Box::new(self.cause(mismatch))),
        )
    }

    /// A refusal whose deciding fault is the verdict's basis, not any record
    /// it cites: the cause names no citation, so a valid one is never read
    /// as the offender.
    pub fn basis_refused(
        &self,
        mismatch: EvaluationCitationMismatch,
        reason: impl Into<String>,
    ) -> StoreError {
        CitationContext {
            citation: "",
            position: None,
            ..*self
        }
        .refused(mismatch, reason)
    }
}

pub(super) fn root_refusal(
    work: WorkId,
    root: &NamedEvaluationRoot,
    cut: i64,
    mismatch: EvaluationRootMismatch,
    declaration: Option<&crate::domain::AcceptanceSourceBasis>,
    reported: Option<String>,
    reason: impl Into<String>,
) -> StoreError {
    refusal(
        work,
        reason,
        AcceptanceEvaluationAdmissionCause::SourceRoot(Box::new(SourceRootAdmissionCause {
            mismatch,
            root_binding: root.event_id.clone(),
            workspace_id: root.event.workspace_id.clone(),
            evaluated_cut: cut,
            declared_workspace_id: declaration.and_then(|basis| basis.workspace_id.clone()),
            declared_revision: declaration.map(|basis| basis.fingerprint.clone()),
            reported_revision: reported,
            remedy: if mismatch == EvaluationRootMismatch::NoInitialSighting {
                EvaluationAdmissionRemedy::CaptureRootAndEvaluate
            } else {
                EvaluationAdmissionRemedy::EvaluateNamedRoot
            },
        })),
    )
}
