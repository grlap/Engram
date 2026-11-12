//! Native pure predicates applied to origin-qualified historical context.

use super::{
    AcceptanceBasis, AcceptanceEvaluation, AcceptanceEvaluationPolicy, AcceptanceStaleReason,
    AcceptanceVerdict, EligibilityContext, IdentityShape, MechanicalBasis,
    RecordAcceptanceEvaluationRequest, SessionId, SessionStanding, WorkEvent, WorkItem,
    assess_mode_policy, eligibility, record_verdicts_malformed,
};

pub(crate) fn historical_evaluation_mark_author(
    events: impl IntoIterator<Item = WorkEvent>,
) -> Option<SessionId> {
    eligibility::historical_mark_author(events)
}

/// Historical exchange supplies origin-qualified standing. This shares native
/// policy and identity rules without consulting native session-name indexes.
pub(crate) fn historical_evaluation_fault(
    item: &WorkItem,
    policy: &AcceptanceEvaluationPolicy,
    record: &AcceptanceEvaluation,
    holder: Option<&SessionId>,
    executor: Option<&SessionId>,
    history: &[SessionId],
    mark_author: Option<SessionId>,
) -> Option<AcceptanceStaleReason> {
    if assess_mode_policy(item, policy, record.mode).is_err()
        || (policy.mechanical_basis == MechanicalBasis::Observed
            && record.verdicts.iter().any(|verdict| {
                verdict.verdict == AcceptanceVerdict::Pass
                    && verdict.basis == AcceptanceBasis::Asserted
            }))
    {
        return Some(AcceptanceStaleReason::Policy);
    }
    let shape = IdentityShape::of_record(record);
    if shape.identity_defect() {
        return Some(AcceptanceStaleReason::Identity);
    }
    if shape.stray_child_metadata() || record_verdicts_malformed(record) {
        return Some(AcceptanceStaleReason::RecordShape);
    }
    let standing = SessionStanding {
        evaluator: record.evaluator.session_id.as_ref(),
        holder,
        executor,
        history,
    };
    if eligibility::same_session_ineligibility_with_author(
        item,
        policy,
        record.mode,
        &standing,
        mark_author,
    )
    .is_some()
    {
        return Some(AcceptanceStaleReason::Policy);
    }
    let request = RecordAcceptanceEvaluationRequest {
        project_id: record.project_id.clone(),
        work_id: record.work_id,
        expected_work_revision: record.work_revision,
        evaluated_through: record.evaluated_cut.position,
        mode: record.mode,
        execution_identity: record.execution_identity.clone(),
        parent_session: record.parent_session.clone(),
        evaluator_model: record.evaluator_model.clone(),
        source_basis: record.source_basis.clone(),
        verdicts: Vec::new(),
        evaluator: record.evaluator.clone(),
        attempt_key: None,
        recorded_at: record.created_at,
        supersedes: record.supersedes.clone(),
    };
    let context = EligibilityContext {
        item,
        policy,
        mode: record.mode,
        evaluator: record.evaluator.session_id.as_ref(),
        parent: record.parent_session.as_ref(),
    };
    if record
        .evaluator_model
        .as_ref()
        .is_some_and(|model| model.validate().is_err())
        || record
            .evaluator
            .session_id
            .as_ref()
            .is_none_or(|evaluator| {
                eligibility::admit_identity(
                    &context, &request, evaluator, holder, executor, history,
                )
                .is_err()
            })
    {
        return Some(AcceptanceStaleReason::Identity);
    }
    None
}
