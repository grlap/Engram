//! Original successor facts are shared by inspection and the atomic writer.

use super::{
    Binding, Event, ObjectId, SqliteStore, StoreError, Transition, invalid, local_store,
    producing_origin, reconciliation, require, view,
};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use crate::task_sync::{
    delta::{PlanningSuccessorRecipe, PlanningSuccessorRequest},
    ledger::EVENT_KIND,
    progress::ExportCut,
    reconciliation::{HOLD_KIND, ReconciliationHold, ReconciliationWorkBasis},
};

mod writer;

impl SqliteStore {
    /// Inspect a new successor's original deferred candidate and fresh local
    /// readset. This performs no Git I/O, creates no records and resolves no
    /// hold. The atomic writer separately checks that local execution has ended.
    ///
    /// # Errors
    /// Refuses another original event, candidate, binding, resolved hold or
    /// changed local cut. Returned observations are not admission authority.
    pub fn inspect_task_sync_planning_successor(
        &self,
        binding_id: &ObjectId,
        request: &PlanningSuccessorRequest,
    ) -> Result<Vec<ReconciliationWorkBasis>, StoreError> {
        self.work_read_snapshot(|store| {
            original_on(&store.connection, binding_id, request).map(|(_, basis)| basis)
        })
    }
}

fn original_facts_on(
    connection: &Connection,
    binding_id: &ObjectId,
    request: &PlanningSuccessorRequest,
) -> Result<(Binding, ReconciliationHold), StoreError> {
    let binding = view::read::binding_on(
        connection,
        binding_id,
        &mut view::read::ReadBudget::default(),
    )?;
    let deferred: Event =
        SqliteStore::get_typed_object_on(connection, &request.deferred, EVENT_KIND)?
            .ok_or_else(|| invalid("planning successor deferred event is missing"))?;
    require(
        deferred.binding == *binding_id
            && matches!(&deferred.transition,
            Transition::PlanningDeferred { prepared, hold, candidate_fingerprint, .. }
            if prepared == &request.original_prepared
                && hold == &request.hold
                && candidate_fingerprint == &request.original_candidate),
        "planning successor original deferred selection differs",
    )?;
    let prepared: Event =
        SqliteStore::get_typed_object_on(connection, &request.original_prepared, EVENT_KIND)?
            .ok_or_else(|| invalid("planning successor original preparation is missing"))?;
    let Transition::DeltaPrepared { intent, payload } = &prepared.transition else {
        return Err(invalid(
            "planning successor original preparation is not a delta",
        ));
    };
    require(
        prepared.binding == *binding_id,
        "planning successor preparation belongs to another binding",
    )?;
    payload.validate_historical(binding_id, &binding, intent)?;
    request.validate(binding_id, &binding, intent)?;
    super::deferred::validate_on(connection, &deferred, intent)?;
    let hold: ReconciliationHold =
        SqliteStore::get_typed_object_on(connection, &request.hold, HOLD_KIND)?
            .ok_or_else(|| invalid("planning successor hold is missing"))?;
    Ok((binding, hold))
}

fn original_on(
    connection: &Connection,
    binding_id: &ObjectId,
    request: &PlanningSuccessorRequest,
) -> Result<(ReconciliationHold, Vec<ReconciliationWorkBasis>), StoreError> {
    let (binding, hold) = original_facts_on(connection, binding_id, request)?;
    local_store(connection, &binding)?;
    for basis in &hold.affected {
        require(
            reconciliation::holds::held_by_on(connection, basis.work_id)?.as_ref()
                == Some(&request.original_prepared),
            "planning successor hold is no longer active for its complete scope",
        )?;
    }
    let basis = reconciliation::capture_affected_on(connection, &binding.project, &hold.seeds)?;
    super::readset::unchanged_on(
        connection,
        &binding.project,
        &request.intent.local_cut,
        basis.iter().map(|basis| basis.work_id),
    )?;
    Ok((hold, basis))
}

/// Resolve the original immutable request, then check the fresh destination in
/// this same writer transaction. Adapter-supplied ids cannot stand in for the
/// canonical hold, and the returned check cannot leave this transaction.
pub(in crate::storage::work::task_sync::ledger) fn admit_on<'tx, 'conn, 'recipe>(
    transaction: &'tx Transaction<'conn>,
    binding_id: &ObjectId,
    recipe: &'recipe PlanningSuccessorRecipe,
) -> Result<reconciliation::successor::CheckedPlanningSuccessor<'tx, 'conn, 'recipe>, StoreError> {
    let (hold, _) = original_on(transaction, binding_id, &recipe.request)?;
    let check = reconciliation::holds::load_reconciliation_hold_on(
        transaction,
        &reconciliation::holds::HoldExpectation {
            project: &hold.project_id,
            hold: &recipe.request.hold,
            prepared: &hold.prepared,
            scope: &recipe.request.hold,
            association: &hold.association,
            candidate_revision: hold.candidate_revision,
            hold_revision: hold.hold_revision,
            scope_revision: hold.scope_revision,
            owner_set_revision: hold.owner_set_revision,
        },
    )?;
    let reconciliation::holds::ReconciliationHoldCheck::Ready(check) = check else {
        return Err(invalid("planning successor hold changed before admission"));
    };
    reconciliation::successor::admit_on(check, recipe)
}

pub(in crate::storage::work::task_sync::ledger) fn validate_prepared_on(
    connection: &Connection,
    binding: &Binding,
    id: &ObjectId,
    event: &Event,
    recipe: &PlanningSuccessorRecipe,
) -> Result<(), StoreError> {
    let (original_binding, hold) = original_facts_on(connection, &event.binding, &recipe.request)?;
    recipe.validate_historical_prepared(event, binding)?;
    require(
        &original_binding == binding && id == &recipe.prepared && recipe.retirements.is_empty(),
        "canonical successor preparation differs from its original selection",
    )?;
    let original: Event =
        SqliteStore::get_typed_object_on(connection, &recipe.request.deferred, EVENT_KIND)?
            .ok_or_else(|| invalid("successor original deferred outcome is missing"))?;
    require(
        original.sequence < event.sequence,
        "successor precedes its deferred request",
    )?;
    producing_origin::check_cut_on(
        connection,
        &binding.project,
        &ExportCut {
            position: recipe.request.intent.local_cut.position.position,
            anchor: recipe.request.intent.local_cut.anchor.clone(),
        },
    )?;
    require(
        hold.affected.len() == recipe.before.len(),
        "successor historical affected scope differs",
    )?;
    for basis in &recipe.before {
        reconciliation::validate_work_basis_links(connection, &binding.project, basis)?;
        let original_basis = hold
            .affected
            .iter()
            .find(|old| old.work_id == basis.work_id)
            .ok_or_else(|| invalid("successor historical task is outside its hold"))?;
        require(
            original_basis.imported_head == basis.imported_head
                && original_basis
                    .runs
                    .iter()
                    .map(|run| (run.run_id, run.generation))
                    .collect::<Vec<_>>()
                    == basis
                        .runs
                        .iter()
                        .map(|run| (run.run_id, run.generation))
                        .collect::<Vec<_>>(),
            "successor changes held generation or imported-head membership",
        )?;
        for run in &basis.runs {
            terminal_at_cut(
                connection,
                binding,
                basis.work_id,
                run,
                recipe.request.intent.local_cut.position.position,
            )?;
        }
    }
    Ok(())
}

/// Historical validation uses native snapshots admitted at the selected cut,
/// never today's live state or the successor's later reopened generation.
fn terminal_at_cut(
    connection: &Connection,
    binding: &Binding,
    work: crate::WorkId,
    basis: &crate::task_sync::reconciliation::ReconciliationRunBasis,
    cut: i64,
) -> Result<(), StoreError> {
    let event_id: Option<String> = connection
        .query_row(
            "SELECT object_id FROM objects INDEXED BY objects_work_event_work_id
         WHERE object_kind='work_event'
           AND json_extract(canonical_json,'$.work_id')=?1
           AND json_extract(canonical_json,'$.run.run_id')=?2
           AND json_extract(canonical_json,'$.run.revision')=?3
           AND EXISTS(SELECT 1 FROM work_feed_entries feed
               WHERE feed.feed_kind='project' AND feed.feed_id=?4
                 AND feed.object_id=objects.object_id AND feed.position<=?5)
         ORDER BY json_extract(canonical_json,'$.revision') DESC LIMIT 1",
            params![
                work.0.to_string(),
                basis.run_id.0.to_string(),
                basis.revision,
                binding.project.0,
                cut
            ],
            |row| row.get(0),
        )
        .optional()?;
    let event_id = event_id
        .and_then(ObjectId::from_stored)
        .ok_or_else(|| invalid("successor has no terminal local run snapshot at its cut"))?;
    let event: crate::WorkEvent =
        SqliteStore::get_typed_object_on(connection, &event_id, "work_event")?
            .ok_or_else(|| invalid("successor terminal run snapshot is missing"))?;
    require(
        event.project_id == binding.project
            && event.work_id == work
            && event.run.as_ref().is_some_and(|run| {
                run.run_id == basis.run_id
                    && run.generation == basis.generation
                    && run.revision == basis.revision
                    && matches!(
                        run.state,
                        crate::WorkRunState::Completed
                            | crate::WorkRunState::Cancelled
                            | crate::WorkRunState::SupersededByImport
                    )
            }),
        "successor captured an unfinished local generation",
    )?;
    if let Some(claim) = basis.claim_id {
        let claim_event: Option<String> = connection
            .query_row(
                "SELECT object_id FROM objects INDEXED BY objects_work_event_work_id
             WHERE object_kind='work_event'
               AND json_extract(canonical_json,'$.work_id')=?1
               AND json_extract(canonical_json,'$.claim.claim_id')=?2
               AND EXISTS(SELECT 1 FROM work_feed_entries feed
                   WHERE feed.feed_kind='project' AND feed.feed_id=?3
                     AND feed.object_id=objects.object_id AND feed.position<=?4)
             ORDER BY json_extract(canonical_json,'$.claim.revision') DESC LIMIT 1",
                params![
                    work.0.to_string(),
                    claim.0.to_string(),
                    binding.project.0,
                    cut
                ],
                |row| row.get(0),
            )
            .optional()?;
        let id = claim_event
            .and_then(ObjectId::from_stored)
            .ok_or_else(|| invalid("successor has no ended local claim snapshot at its cut"))?;
        let event: crate::WorkEvent =
            SqliteStore::get_typed_object_on(connection, &id, "work_event")?
                .ok_or_else(|| invalid("successor claim snapshot is missing"))?;
        require(
            event.project_id == binding.project
                && event.work_id == work
                && event.claim.as_ref().is_some_and(|value| {
                    value.claim_id == claim
                        && value.run_id == basis.run_id
                        && value.fence <= basis.claim_fence
                        && value.state != crate::WorkClaimState::Active
                }),
            "successor captured an active local claim",
        )?;
    }
    Ok(())
}

pub(in crate::storage::work::task_sync::ledger) fn validate_receipt_on(
    connection: &Connection,
    binding: &Binding,
    id: &ObjectId,
    event: &Event,
    recipe: &PlanningSuccessorRecipe,
    receipt: &crate::task_sync::delta::DeltaImportReceipt,
) -> Result<(), StoreError> {
    require(
        id == &recipe.outcome
            && event == &recipe.applied_event(&event.binding, binding, receipt.clone())?,
        "canonical successor outcome differs from retained construction",
    )?;
    let preparation: Event =
        SqliteStore::get_typed_object_on(connection, &recipe.prepared, EVENT_KIND)?
            .ok_or_else(|| invalid("successor canonical preparation is missing"))?;
    let Transition::PlanningSuccessorPrepared {
        recipe: stored,
        payload,
    } = &preparation.transition
    else {
        return Err(invalid("successor preparation belongs to another family"));
    };
    require(
        stored.as_ref() == recipe,
        "successor recipe changed after preparation",
    )?;
    validate_prepared_on(connection, binding, &recipe.prepared, &preparation, recipe)?;
    super::receipt::validate_planning_effects(
        connection,
        binding,
        &recipe.request.intent,
        payload,
        event,
        receipt,
    )?;
    for task in &receipt.tasks {
        require(
            recipe.task_events.get(&task.task) == Some(&task.local_record_ids),
            "successor local event identities differ from retained construction",
        )?;
    }
    let planning: crate::task_sync::delta::DeltaPlanning = SqliteStore::get_typed_object_on(
        connection,
        &recipe.planning,
        crate::task_sync::delta::DELTA_PLANNING_KIND,
    )?
    .ok_or_else(|| invalid("successor planning record is missing"))?;
    planning.validate(&binding.project, &recipe.prepared, &recipe.request.intent)?;
    require(
        planning.local_planning == recipe.baseline,
        "successor local baseline identity differs from retained construction",
    )?;
    Ok(())
}
