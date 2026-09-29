//! Read-only classification of what completion can do with an open obligation.

use super::super::WorkObligationCompletionAction;
use super::named_root::{ForeignChange, classify_change_on};
use super::{
    ExecutionObservation, NamedRootContext, SqliteStore, StoreError, WorkObligation, WorkRunId,
    latest_source_mutation_on, load_typed_work_object, load_work_claim_optional,
    named_root_context_on,
};

impl SqliteStore {
    /// Guidance at the current claim's named-root cut; completion classifies
    /// again. A bounded page loads each run's claim and root once.
    pub(crate) fn work_obligation_completion_actions(
        &self,
        obligations: &[&WorkObligation],
    ) -> Result<Vec<WorkObligationCompletionAction>, StoreError> {
        let mut context: Option<(WorkRunId, bool, Option<NamedRootContext>, bool)> = None;
        let mut actions = Vec::with_capacity(obligations.len());
        for obligation in obligations {
            if !crate::control::is_source_change_obligation(&obligation.rule) {
                actions.push(WorkObligationCompletionAction::CheckOrWaiver);
                continue;
            }
            if context
                .as_ref()
                .is_none_or(|(run_id, _, _, _)| *run_id != obligation.run_id)
            {
                let claim = load_work_claim_optional(&self.connection, obligation.run_id)?;
                let root = claim
                    .as_ref()
                    .map(|claim| {
                        named_root_context_on(
                            &self.connection,
                            obligation.run_id,
                            claim.claim_id,
                            i64::MAX,
                        )
                    })
                    .transpose()?
                    .flatten();
                let latest_unverifiable = if claim.is_some() && root.is_none() {
                    latest_source_mutation_on(&self.connection, obligation.run_id, i64::MAX)?
                        .is_some_and(|(_, mutation)| {
                            mutation.source_basis.is_none() || mutation.observed_at.is_none()
                        })
                } else {
                    false
                };
                context = Some((
                    obligation.run_id,
                    claim.is_some(),
                    root,
                    latest_unverifiable,
                ));
            }
            let (_, has_claim, root, latest_unverifiable) =
                context.as_ref().expect("source context initialized");
            if !has_claim {
                actions.push(WorkObligationCompletionAction::CheckOrWaiver);
                continue;
            }
            let change: ExecutionObservation = load_typed_work_object(
                &self.connection,
                &obligation.triggering_observation,
                "execution_observation",
            )?;
            let class = classify_change_on(
                &self.connection,
                root.as_ref(),
                &change,
                obligation.trigger_position.position,
            )?;
            actions.push(match class {
                ForeignChange::Displaced => WorkObligationCompletionAction::DoneDisplaces,
                ForeignChange::Open => WorkObligationCompletionAction::WaiverOnly,
                ForeignChange::Unlocated if root.is_none() => {
                    WorkObligationCompletionAction::NameRootCheckOrWaiver
                }
                ForeignChange::Unlocated => WorkObligationCompletionAction::CheckOrWaiver,
                ForeignChange::NotForeign | ForeignChange::Unbound
                    if crate::control::is_stock_source_change_obligation(
                        &obligation.rule,
                        &obligation.requirement,
                    ) =>
                {
                    WorkObligationCompletionAction::DoneWaives
                }
                ForeignChange::NotForeign | ForeignChange::Unbound if *latest_unverifiable => {
                    WorkObligationCompletionAction::NameRootCheckOrWaiver
                }
                ForeignChange::NotForeign | ForeignChange::Unbound => {
                    WorkObligationCompletionAction::CheckOrWaiver
                }
            });
        }
        Ok(actions)
    }
}
