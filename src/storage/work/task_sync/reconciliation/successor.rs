//! Admission of a planning successor after local execution has ended normally.

use super::super::producing_origin;
use super::{ObjectId, StoreError, Transaction, capture_affected_on, holds, query, require};
use crate::{WorkClaimState, WorkRunState, task_sync::delta::PlanningSuccessorRecipe};

/// The writer keeps the checked hold and exact recipe in their original SQL
/// transaction. This value cannot be constructed from adapter observations.
pub(in crate::storage::work) struct CheckedPlanningSuccessor<'tx, 'conn, 'recipe> {
    hold: holds::CheckedReconciliationHold<'tx, 'conn>,
    recipe: &'recipe PlanningSuccessorRecipe,
}

impl CheckedPlanningSuccessor<'_, '_, '_> {
    pub(in crate::storage::work) fn project(&self) -> &crate::ProjectId {
        &self.hold.hold().project_id
    }

    pub(in crate::storage::work) fn require_selected_event(
        &self,
        work: crate::WorkId,
        event: &ObjectId,
    ) -> Result<(), StoreError> {
        // Admission has already checked this complete scope in our transaction.
        // Event append occurs between persisting the new projection and its
        // canonical event, so it must not reread that intermediate projection.
        self.require_scope(work)?;
        require(
            self.recipe
                .task_events
                .get(&work)
                .is_some_and(|ids| ids.contains(event)),
            "successor event identity differs from its retained task construction",
        )
    }

    pub(in crate::storage::work) fn require_affected(
        &self,
        work: crate::WorkId,
    ) -> Result<(), StoreError> {
        self.require_scope(work)?;
        match query::load_work_item(self.transaction(), work) {
            Ok(item) => require(
                &item.project_id == self.project(),
                "successor endpoint belongs to another project",
            )?,
            Err(StoreError::WorkNotFound(_)) => (),
            Err(error) => return Err(error),
        }
        Ok(())
    }

    fn require_scope(&self, work: crate::WorkId) -> Result<(), StoreError> {
        require(
            self.recipe.before.iter().any(|basis| basis.work_id == work)
                || self.recipe.task_events.contains_key(&work),
            "successor endpoint is outside its checked affected scope",
        )
    }

    pub(in crate::storage::work) fn transaction(&self) -> &Transaction<'_> {
        self.hold.transaction()
    }

    #[cfg(test)]
    pub(in crate::storage::work::task_sync) fn hold_id(&self) -> &ObjectId {
        self.hold.scope_ref()
    }

    pub(in crate::storage::work) fn recipe(&self) -> &PlanningSuccessorRecipe {
        self.recipe
    }
}

/// Recheck the original hold, fresh complete readset and every local generation
/// inside the writer's transaction. Releasing a claim leaves an unfinished run
/// ineligible; elapsed claim time supplies no permission to change that run.
pub(in crate::storage::work::task_sync) fn admit_on<'tx, 'conn, 'recipe>(
    checked: holds::CheckedReconciliationHold<'tx, 'conn>,
    recipe: &'recipe PlanningSuccessorRecipe,
) -> Result<CheckedPlanningSuccessor<'tx, 'conn, 'recipe>, StoreError> {
    recipe.validate()?;
    let hold = checked.hold();
    require(
        checked.scope_ref() == &recipe.request.hold
            && hold.prepared == recipe.request.original_prepared
            && hold.candidate_fingerprint == recipe.request.original_candidate,
        "planning successor checked hold differs from its retained request",
    )?;
    require(
        recipe.retirements.is_empty(),
        "planning successor cannot retire local execution",
    )?;
    let transaction = checked.transaction();
    require(
        recipe.request.intent.local_cut.binding == hold.binding
            && recipe.request.intent.local_cut.position.feed
                == crate::FeedId::Project(hold.project_id.clone()),
        "planning successor destination scope changed",
    )?;
    producing_origin::check_cut_on(
        transaction,
        &hold.project_id,
        &crate::task_sync::progress::ExportCut {
            position: recipe.request.intent.local_cut.position.position,
            anchor: recipe.request.intent.local_cut.anchor.clone(),
        },
    )?;
    let current = capture_affected_on(transaction, &hold.project_id, &hold.seeds)?;
    require(
        current == recipe.before,
        "planning successor local readset changed",
    )?;
    require_ended_on(transaction, &current)?;
    super::super::super::planning::successor::require_successor_roots_on(transaction, recipe)?;
    for basis in &current {
        require(
            holds::held_by_on(transaction, basis.work_id)?.as_ref() == Some(&hold.prepared),
            "planning successor hold no longer covers its complete scope",
        )?;
        for basis in &basis.runs {
            let run = query::load_work_run(transaction, basis.run_id)?;
            require(
                recipe
                    .new_root_executions
                    .values()
                    .all(|planned| planned != &run.root_execution_id),
                "planning successor reuses an original root execution identity",
            )?;
        }
    }
    Ok(CheckedPlanningSuccessor {
        hold: checked,
        recipe,
    })
}

/// Selection and application use the same local end condition. A released or
/// expired claim does not end its run, and retaining a recipe grants no grace.
pub(in crate::storage::work::task_sync) fn require_ended_on(
    connection: &rusqlite::Connection,
    before: &[crate::task_sync::reconciliation::ReconciliationWorkBasis],
) -> Result<(), StoreError> {
    for basis in before.iter().flat_map(|work| &work.runs) {
        let run = query::load_work_run(connection, basis.run_id)?;
        let claim = query::load_work_claim_optional(connection, basis.run_id)?;
        require(
            claim.is_none_or(|claim| claim.state != WorkClaimState::Active),
            "planning successor waits for an active local claim",
        )?;
        require(
            matches!(
                run.state,
                WorkRunState::Completed
                    | WorkRunState::Cancelled
                    | WorkRunState::SupersededByImport
            ),
            "planning successor waits for an unfinished local run",
        )?;
    }
    Ok(())
}
