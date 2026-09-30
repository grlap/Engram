//! One formatter for cause-derived evaluation admission advice.

use crate::domain::{
    AcceptanceEvaluationAdmissionCause, EvaluationAdmissionRemedy, EvaluationEligibilityMismatch,
};

pub(crate) fn evaluation_admission_remedy(cause: &AcceptanceEvaluationAdmissionCause) -> String {
    let action = match cause {
        AcceptanceEvaluationAdmissionCause::Eligibility(context) => context.remedy,
        AcceptanceEvaluationAdmissionCause::SourceRoot(context) => context.remedy,
        AcceptanceEvaluationAdmissionCause::Citation(context) => context.remedy,
    };
    match action {
        EvaluationAdmissionRemedy::UseSelfAssertedCompletion => "the project does not enable acceptance evaluation; satisfy its current acceptance and complete under its self-asserted policy".into(),
        EvaluationAdmissionRemedy::RequestEligibleEvaluation => {
            let AcceptanceEvaluationAdmissionCause::Eligibility(context) = cause else {
                return "request an evaluation in a mode admitted by the project and the task's mark".into();
            };
            if matches!(context.mismatch, EvaluationEligibilityMismatch::MarkAuthorUnrecorded | EvaluationEligibilityMismatch::MarkAuthorAffiliated) {
                return "have a planning session that never held or executed this run clear the task's evaluation mark and set an admitted mark again, then request an evaluation admitted by that mark and the project policy".into();
            }
            super::missing_evaluation_remedy(context.task_mark, &context.admitted_modes).into()
        }
        EvaluationAdmissionRemedy::InspectEvaluatorBinding => "inspect the evaluator's session and parent binding; submit from a session eligible for the selected mode".into(),
        EvaluationAdmissionRemedy::CaptureRootAndEvaluate => "end the turn so the host can capture the named root, read the run again, then evaluate that root; a declaration alone does not supply an initial sighting".into(),
        EvaluationAdmissionRemedy::EvaluateNamedRoot => "read the named root and its reported source, then evaluate that root with a matching source declaration".into(),
        EvaluationAdmissionRemedy::ReadRunEvidence => "read this run's show --notes --gates and cite its own admissible evidence; a bound pass needs matching passed host verification".into(),
        EvaluationAdmissionRemedy::ReadCurrentCut => "read show again for the evidence cut that includes the citation, then judge that evidence and submit with that cut".into(),
        EvaluationAdmissionRemedy::RunCurrentCheckAndEvaluate => "run the required check on the current named root or run source, have the host record it, then evaluate again citing that check within the evaluated cut".into(),
    }
}
