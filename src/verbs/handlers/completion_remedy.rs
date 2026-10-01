//! The words a completion refusal gives: what is owed and, for a missing
//! acceptance evaluation, where one comes from.

use super::{lifecycle_word, short};

/// What a missing-evaluation reminder needs besides the recovery: the task's
/// evaluation-mode mark and the modes the project admits (empty when unread).
#[derive(Clone, Debug, Default)]
pub(in crate::verbs) struct EvaluationRemedy {
    pub(in crate::verbs) mark: Option<crate::domain::AcceptanceEvaluationMode>,
    pub(in crate::verbs) admitted: Vec<crate::domain::AcceptanceEvaluationMode>,
}

pub(in crate::verbs) fn completion_recovery_reminder(
    recovery: &crate::WorkCompletionRecovery,
    include_title: bool,
    evaluation: &EvaluationRemedy,
) -> String {
    let item = &recovery.item;
    let label = if include_title {
        format!("{} \"{}\"", item.short_ref, short(&item.title))
    } else {
        item.short_ref.clone()
    };
    let reminder = match &recovery.cause {
        crate::WorkCompletionRecoveryCause::OpenObligation {
            obligation_id,
            required_check,
            ..
        } => format!(
            "{label} still owes {required_check:?} for obligation {}",
            obligation_id.0
        ),
        crate::WorkCompletionRecoveryCause::RequiredChildUnsealed { .. } => format!(
            "required child {label} is {} without a completion seal or waiver",
            lifecycle_word(item.lifecycle)
        ),
        crate::WorkCompletionRecoveryCause::MissingContribution { participant } => format!(
            "{label} is missing the contribution or waiver for participant {}",
            participant.0
        ),
        crate::WorkCompletionRecoveryCause::MissingAcceptance { criterion } => {
            format!("{label} is missing acceptance for \"{}\"", short(criterion))
        }
        crate::WorkCompletionRecoveryCause::MissingAcceptanceEvaluation { criterion } => {
            format!(
                "{label} has no acceptance evaluation for \"{}\"; {}",
                short(criterion),
                crate::work_service::missing_evaluation_remedy(
                    evaluation.mark,
                    &evaluation.admitted
                )
            )
        }
        crate::WorkCompletionRecoveryCause::AcceptanceEvaluationStale { reason } => match reason {
            crate::AcceptanceStaleReason::Source => format!(
                "{label} acceptance evaluation is stale (source): {}",
                recovery.source.as_deref().map_or("read the current source and request a fresh acceptance evaluation, then retry done", crate::work_service::source_recovery_remedy)
            ),
            // Two causes share this reason and the refusal does not say which,
            // so the remedy is the one a missing evaluation of this task gets.
            crate::AcceptanceStaleReason::Identity => format!(
                "{label} acceptance evaluation is stale (identity): its independent evaluator has since held this run, or the record lacks a session its mode requires; {}",
                crate::work_service::missing_evaluation_remedy(
                    evaluation.mark,
                    &evaluation.admitted
                )
            ),
            crate::AcceptanceStaleReason::VerificationSource => format!(
                "{label} acceptance evaluation is stale (verification_source): a pass on a bound criterion cites a check that ran on another source than the one evaluated, or before a later change to it; run the check on the current source, then evaluate again citing it, declaring the source revision the host reports"
            ),
            crate::AcceptanceStaleReason::Policy => format!(
                "{label} acceptance evaluation is stale (policy): the project's policy or the task's mark no longer admits the mode or session it was recorded in; {}",
                crate::work_service::missing_evaluation_remedy(
                    evaluation.mark,
                    &evaluation.admitted
                )
            ),
            reason => format!(
                "{label} acceptance evaluation is stale ({}); evaluate again",
                reason.word()
            ),
        },
        crate::WorkCompletionRecoveryCause::AcceptanceFailed { criterion } => format!(
            "{label} failed acceptance for \"{}\"; correct the work, then evaluate again",
            short(criterion)
        ),
        crate::WorkCompletionRecoveryCause::AcceptanceInsufficientEvidence { criterion } => {
            format!(
                "{label} lacks sufficient evidence for \"{}\"; record evidence, then evaluate again",
                short(criterion)
            )
        }
        crate::WorkCompletionRecoveryCause::AcceptanceNeedsHuman { criterion } => format!(
            "{label} needs a human decision on \"{}\"; revise the criteria or cancel",
            short(criterion)
        ),
    };
    // The cause's words stay as they are; the observation that decided a
    // stale evaluation's source move follows them as one escaped sentence.
    match &recovery.deciding_observation {
        Some(observation) => format!("{reminder}. {}", observation.sentence()),
        None => reminder,
    }
}
