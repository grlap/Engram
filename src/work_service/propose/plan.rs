use super::super::{
    BeginWorkProtocolAttempt, DateTime, DevelopmentNoopRedactor, LocalWorkService, StoreError, Utc,
    WorkId, WorkProposeInput, WorkProposeResult,
};
use crate::domain::{ProposeWorkPlanRequest, WorkPlanInput, WorkPlanMapping, WorkPlanReceipt};

/// Complete host/operator mapping, never injected as a compact agent receipt.
/// Checked before any graph commit, including recovery of an existing receipt.
const MAX_WORK_PLAN_RESPONSE_BYTES: usize = 64 * 1024;

impl LocalWorkService {
    pub(super) fn work_propose_plan(
        &self,
        work_ref: Option<&str>,
        plan: &WorkPlanInput,
        now: DateTime<Utc>,
    ) -> Result<WorkProposeResult, StoreError> {
        self.work_propose_plan_with_budget(work_ref, plan, now, MAX_WORK_PLAN_RESPONSE_BYTES)
    }

    fn work_propose_plan_with_budget(
        &self,
        work_ref: Option<&str>,
        plan: &WorkPlanInput,
        now: DateTime<Utc>,
        budget: usize,
    ) -> Result<WorkProposeResult, StoreError> {
        if work_ref.is_some() {
            return Err(StoreError::InvalidWork(
                "plan parents must use payload-local keys; omit work_ref".into(),
            ));
        }
        crate::storage::validate_work_plan(plan)?;
        // Full typed envelope with worst-width generated identities and revisions.
        // ASCII-only keys need no escaping, and the exact receipt is also checked
        // by the storage callback before its transaction can commit.
        let bound = WorkPlanReceipt {
            tasks: plan
                .tasks
                .iter()
                .map(|task| WorkPlanMapping {
                    key: task.key.clone(),
                    work_id: WorkId::new(),
                    short_ref: "w-ffffffffffff".into(),
                    revision: i64::MAX,
                })
                .collect(),
        };
        admit_plan_response(
            &WorkProposeResult::Plan(bound),
            MAX_WORK_PLAN_RESPONSE_BYTES,
        )?;
        let mut store = self.store_at(now)?;
        let input = WorkProposeInput::Plan { plan: plan.clone() };
        let basis = (); // New roots have no ambient target or claim basis.
        let attempt = store.begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
            project_id: &self.project_id,
            session_id: &self.session_id,
            operation: "work_propose:plan",
            idempotency_key: &plan.idempotency_key,
            intent: &self.protocol_intent(&input),
            basis: &basis,
            now,
        })?;
        if let Some(result) = attempt.result {
            let result: WorkProposeResult = serde_json::from_value(result)?;
            admit_plan_response(&result, budget)?;
            return Ok(result);
        }
        // Always replay through storage's in-transaction intent check. A core
        // receipt may predate this protocol attempt and belong to another input.
        let receipt = store.propose_work_plan_with_admission(
            &ProposeWorkPlanRequest {
                project_id: self.project_id.clone(),
                plan: plan.clone(),
                actor: self.actor("work_propose", "atomically admit an authored local plan"),
                created_at: now,
            },
            &DevelopmentNoopRedactor,
            |receipt| admit_plan_response(&WorkProposeResult::Plan(receipt.clone()), budget),
        )?;
        let result = WorkProposeResult::Plan(receipt);
        store.finish_work_protocol_attempt(
            &self.project_id,
            &self.session_id,
            "work_propose:plan",
            &plan.idempotency_key,
            &result,
        )?;
        Ok(result)
    }
}

fn admit_plan_response(result: &WorkProposeResult, budget: usize) -> Result<(), StoreError> {
    let bytes = serde_json::to_vec(result)?.len();
    if bytes > budget {
        return Err(StoreError::InvalidWorkProjection(format!(
            "work_propose plan response is {bytes} bytes, exceeding the {budget}-byte limit"
        )));
    }
    Ok(())
}

#[cfg(test)]
mod tests;
