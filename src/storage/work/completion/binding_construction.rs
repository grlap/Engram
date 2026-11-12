//! Shared acceptance-binding selection for ordinary and retained construction.

use super::{
    Connection, DateTime, FeedId, FeedPosition, ObjectId, SCHEMA_VERSION, SqliteStore, StoreError,
    Transaction, Utc, WorkEventDraft, WorkItem, WorkObligation, WorkObligationId,
    WorkObligationRecord, WorkObligationResolution, WorkObligationResolutionEvent,
    WorkObligationState, WorkRun, WorkRunId, append_obligation_resolution_with_id_on, binding_rule,
    binding_rule_criterion, load_complete_work_obligation_records_on,
    load_work_obligation_records_on, persist_obligation_with_id_on,
};
use crate::task_sync::delta::{PlannedSuccessorObligation, PlannedSuccessorResolution};

fn definitions_to_open_on<'a>(
    connection: &Connection,
    item: &'a WorkItem,
    run: WorkRunId,
    reauthored: &[usize],
) -> Result<Vec<&'a crate::domain::AcceptanceBinding>, StoreError> {
    if item.acceptance_bindings.is_empty() {
        return Ok(Vec::new());
    }
    let existing = load_work_obligation_records_on(connection, run, None)?;
    Ok(item
        .acceptance_bindings
        .iter()
        .filter(|binding| {
            let rule = binding_rule(binding.criterion);
            !existing.iter().any(|record| {
                record.obligation.rule == rule
                    && record.obligation.requirement == binding.requirement
                    && record.state != WorkObligationState::Waived
                    && !(reauthored.contains(&binding.criterion)
                        && record.obligation.work_revision < item.revision)
            })
        })
        .collect())
}

fn resolutions_to_waive_on(
    connection: &Connection,
    item: &WorkItem,
    run: WorkRunId,
    reauthored: &[usize],
) -> Result<Vec<WorkObligationRecord>, StoreError> {
    Ok(load_complete_work_obligation_records_on(connection, run)?
        .into_iter()
        .filter(|record| record.state == WorkObligationState::Open)
        .filter(|record| {
            let Some(criterion) = binding_rule_criterion(&record.obligation.rule) else {
                return false;
            };
            let rewritten =
                reauthored.contains(&criterion) && record.obligation.work_revision < item.revision;
            let still_bound = !rewritten
                && item.acceptance_bindings.iter().any(|binding| {
                    binding.criterion == criterion
                        && binding.requirement == record.obligation.requirement
                });
            !still_bound
        })
        .collect())
}

/// Read-only construction planning, not write admission. A future run id may
/// have no rows yet; the checked mutating core selects again against reality.
pub(in crate::storage::work) fn plan_binding_obligation_identities_on(
    connection: &Connection,
    item: &WorkItem,
    run: WorkRunId,
    reauthored: &[usize],
) -> Result<
    (
        Vec<PlannedSuccessorObligation>,
        Vec<PlannedSuccessorResolution>,
    ),
    StoreError,
> {
    let definitions = definitions_to_open_on(connection, item, run, reauthored)?
        .into_iter()
        .map(|_| PlannedSuccessorObligation {
            obligation_id: WorkObligationId::new(),
            record: ObjectId::mint(),
        })
        .collect();
    let resolutions = resolutions_to_waive_on(connection, item, run, reauthored)?
        .into_iter()
        .map(|record| PlannedSuccessorResolution {
            obligation_id: record.obligation.obligation_id,
            record: ObjectId::mint(),
        })
        .collect();
    Ok((definitions, resolutions))
}

#[allow(
    clippy::too_many_arguments,
    reason = "native obligation construction retains its trigger and optional planned identities"
)]
pub(super) fn open_on(
    transaction: &Transaction<'_>,
    item: &WorkItem,
    run: &WorkRun,
    trigger: &ObjectId,
    trigger_position: &FeedPosition,
    reauthored: &[usize],
    now: DateTime<Utc>,
    planned: Option<&[PlannedSuccessorObligation]>,
) -> Result<Vec<ObjectId>, StoreError> {
    let selected = definitions_to_open_on(transaction, item, run.run_id, reauthored)?;
    if planned.is_some_and(|planned| planned.len() != selected.len()) {
        return Err(StoreError::InvalidWorkProjection(
            "successor obligation definitions differ from native selection".into(),
        ));
    }
    if item.acceptance_bindings.is_empty() {
        return Ok(Vec::new());
    }
    let (_, policy, _) = SqliteStore::load_control_policy_head(transaction)?;
    let mut definitions = Vec::with_capacity(selected.len());
    for (index, binding) in selected.into_iter().enumerate() {
        let identity = planned.map(|planned| &planned[index]);
        let obligation = WorkObligation {
            schema_version: SCHEMA_VERSION,
            obligation_id: identity.map_or_else(WorkObligationId::new, |id| id.obligation_id),
            project_id: item.project_id.clone(),
            root_execution_id: run.root_execution_id,
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: run.run_id,
            work_revision: item.revision,
            rule_set: policy.obligation_rule_set.clone(),
            rule: binding_rule(binding.criterion),
            triggering_observation: trigger.clone(),
            trigger_position: trigger_position.clone(),
            requirement: binding.requirement.clone(),
            opened_at: now,
        };
        definitions.push(persist_obligation_with_id_on(
            transaction,
            &obligation,
            identity.map(|id| &id.record),
        )?);
    }
    Ok(definitions)
}

pub(super) fn waive_on(
    transaction: &Transaction<'_>,
    item: &WorkItem,
    run: WorkRunId,
    reauthored: &[usize],
    actor: &crate::ActorContext,
    now: DateTime<Utc>,
    planned: Option<&[PlannedSuccessorResolution]>,
) -> Result<Vec<ObjectId>, StoreError> {
    let selected = resolutions_to_waive_on(transaction, item, run, reauthored)?;
    if let Some(planned) = planned
        && (planned.len() != selected.len()
            || planned
                .iter()
                .zip(&selected)
                .any(|(id, record)| id.obligation_id != record.obligation.obligation_id))
    {
        return Err(StoreError::InvalidWorkProjection(
            "successor obligation resolutions differ from native selection".into(),
        ));
    }
    let mut resolutions = Vec::with_capacity(selected.len());
    for (index, record) in selected.into_iter().enumerate() {
        let criterion = binding_rule_criterion(&record.obligation.rule).ok_or_else(|| {
            StoreError::InvalidWorkProjection("selected binding obligation has no criterion".into())
        })?;
        let rewritten =
            reauthored.contains(&criterion) && record.obligation.work_revision < item.revision;
        let change = if rewritten {
            "was rewritten, and its verification is owed again"
        } else {
            "no longer requires this verification"
        };
        let event = WorkObligationResolutionEvent {
            registered_input_decision: None,
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            obligation_id: record.obligation.obligation_id,
            definition: record.definition_id.clone(),
            run_id: run,
            resolution: WorkObligationResolution::Waived {
                waived_by: actor.actor_id.clone(),
                reason: format!(
                    "acceptance revised at revision {}: criterion {criterion} {change}",
                    item.revision
                ),
            },
            actor: actor.clone(),
            created_at: now,
        };
        resolutions.push(append_obligation_resolution_with_id_on(
            transaction,
            &record,
            &event,
            planned.map(|ids| &ids[index].record),
        )?);
    }
    Ok(resolutions)
}

/// The capability supplies all construction ids. No old run or obligation is
/// reinterpreted as evidence for a newly opened generation.
#[allow(
    clippy::too_many_arguments,
    reason = "revision bookkeeping uses the native event and its exact feed positions"
)]
pub(in crate::storage::work) fn apply_successor_binding_obligations_on(
    checked: &super::super::task_sync::reconciliation::successor::CheckedPlanningSuccessor<
        '_,
        '_,
        '_,
    >,
    item: &WorkItem,
    event: &WorkEventDraft,
    event_id: &ObjectId,
    positions: &[FeedPosition],
    reauthored: &[usize],
    actor: &crate::ActorContext,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    checked.require_selected_event(item.work_id, event_id)?;
    let definitions = checked
        .recipe()
        .obligation_definitions
        .get(&item.work_id)
        .map_or(&[][..], Vec::as_slice);
    let resolutions = checked
        .recipe()
        .obligation_resolutions
        .get(&item.work_id)
        .map_or(&[][..], Vec::as_slice);
    let Some(run_id) = item.active_run_id else {
        if !definitions.is_empty() || !resolutions.is_empty() {
            return Err(StoreError::InvalidWorkProjection(
                "successor obligations have no active native generation".into(),
            ));
        }
        return Ok(());
    };
    waive_on(
        checked.transaction(),
        item,
        run_id,
        reauthored,
        actor,
        now,
        Some(resolutions),
    )?;
    if let Some(run) = event.run.as_ref()
        && let Some(position) = positions
            .iter()
            .find(|position| position.feed == FeedId::RunExecution(run_id))
    {
        open_on(
            checked.transaction(),
            item,
            run,
            event_id,
            position,
            reauthored,
            now,
            Some(definitions),
        )?;
    } else if !definitions.is_empty() {
        return Err(StoreError::InvalidWorkProjection(
            "successor obligation definitions have no native trigger".into(),
        ));
    }
    Ok(())
}
