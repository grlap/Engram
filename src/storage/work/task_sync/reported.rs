//! Imported completion is an inert report, never a local successful run.

use chrono::{DateTime, Utc};
use rusqlite::{Connection, Transaction, params};

use super::super::{WorkEventDraft, feeds, planning, query};
use crate::{
    ActorContext, CanonicalObject, RestoredRecord, RestoredRelationBasis, RootExecutionState,
    SqliteStore, StoreError, WorkEvent, WorkGraphSnapshotItem, WorkLifecycle, WorkRunState,
    WorkTransition, task_sync::TaskDocument,
};

pub(super) fn apply_on(
    transaction: &Transaction<'_>,
    task: &TaskDocument,
    mut snapshot: WorkGraphSnapshotItem,
    actor: &ActorContext,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    let mut item = query::load_work_item(transaction, task.id)?;
    if !matches!(
        item.lifecycle,
        WorkLifecycle::Open | WorkLifecycle::Proposed
    ) || super::folder_admission::engaged_run(transaction, task.id, now)?.is_some()
        || super::super::completion::live_descendant_execution_authority(transaction, task.id, now)?
        || task.lifecycle != WorkLifecycle::Completed
        || task.completion.is_none()
    {
        return Err(invalid("reported completion lost admission"));
    }
    let mut run = item
        .active_run_id
        .map(|id| query::load_work_run(transaction, id))
        .transpose()?;
    let mut claim = run
        .as_ref()
        .map(|run| query::load_work_claim_optional(transaction, run.run_id))
        .transpose()?
        .flatten();
    if let Some(claim) = &mut claim
        && claim.state == crate::WorkClaimState::Active
    {
        claim.state = crate::WorkClaimState::Released;
        claim.revision += 1;
        claim.fence += 1;
        claim.expires_at = now;
        planning::persist_claim(transaction, claim)?;
    }
    // A restored root need never have had its own run: a child's local
    // execution can still have created the root aggregate that this report
    // must close and bind in its canonical event.
    let mut execution = if item.work_id == item.root_id {
        query::active_root_execution_optional(transaction, item.root_id)?
    } else {
        run.as_ref()
            .map(|run| query::load_root_execution(transaction, run.root_execution_id))
            .transpose()?
    };
    if let Some(run) = &mut run {
        let fence = if let Some(claim) = &claim {
            claim.fence
        } else {
            transaction.query_row(
                "SELECT claim_fence_head FROM work_runs WHERE run_id = ?1",
                [run.run_id.0.to_string()],
                |row| row.get::<_, i64>(0),
            )?
        };
        run.state = WorkRunState::Cancelled;
        run.executor = None;
        run.revision += 1;
        run.updated_at = now;
        planning::persist_work_run(transaction, run, fence)?;
    }
    // As for native root completion, end this iteration even if optional
    // descendants retain dormant runs. Admission already excluded live
    // descendant authority; those old runs must not keep the next iteration
    // on this generation's contributors, waivers and child seals.
    if let Some(execution) = &mut execution
        && item.work_id == item.root_id
        && execution.state == RootExecutionState::Active
    {
        execution.state = RootExecutionState::Cancelled;
        execution.revision += 1;
        execution.updated_at = now;
        planning::persist_root_execution(transaction, execution)?;
    }
    snapshot.origin = item.origin;
    snapshot
        .source_snapshot_id
        .clone_from(&item.source_snapshot_id);
    let generation_index = query::restored_records_with_id_for_item(transaction, task.id)?.len();
    // Shared notes retain their original native/import identities. A completion
    // report adds only the outcome, without inheriting those notes a second time.
    let mut history = super::history(task, actor, now);
    history.notes.clear();
    history.events.clear();
    let record = RestoredRecord {
        schema_version: crate::WORK_GRAPH_SNAPSHOT_SCHEMA_VERSION,
        project_id: item.project_id.clone(),
        work_id: task.id,
        generation_index,
        item: snapshot,
        relations: RestoredRelationBasis {
            prerequisites: task.prerequisites.clone(),
            blockers: task.snapshot_blockers(),
        },
        history,
    };
    let object = CanonicalObject::mint(&record)?;
    SqliteStore::insert_object(transaction, "work_restored_record", &object)?;
    transaction.execute(
        "INSERT INTO work_restored_records(work_id, generation_index, record_id) VALUES (?1, ?2, ?3)",
        params![task.id.0.to_string(), i64::try_from(generation_index)
            .map_err(|_| invalid("reported generation exceeds storage bound"))?, object.key().as_str()],
    )?;
    item.lifecycle = WorkLifecycle::Completed;
    item.active_run_id = None;
    item.restored = true;
    item.revision += 1;
    item.updated_at = now;
    planning::persist_work_item(transaction, &item)?;
    feeds::append_work_event(
        transaction,
        &WorkEventDraft {
            schema_version: crate::schema::SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: run.as_ref().map(|run| run.run_id),
            revision: item.revision,
            work: item,
            run,
            root_execution: execution,
            claim,
            handoff_offer: None,
            blocker: None,
            transition: WorkTransition::CompletionReported {
                record: object.key().clone(),
            },
            actor: actor.clone(),
            created_at: now,
        },
    )?;
    Ok(())
}

/// Check the exact report named by a native event, independently of current
/// projections and without treating an imported result as successful proof.
pub(in crate::storage) fn validate_on(
    connection: &Connection,
    event: &WorkEvent,
) -> Result<Option<RestoredRecord>, StoreError> {
    let WorkTransition::CompletionReported { record: id } = &event.transition else {
        return Ok(None);
    };
    let record: RestoredRecord =
        feeds::load_typed_work_object(connection, id, "work_restored_record")?;
    let mut basis = event.work.clone();
    basis.revision = 1;
    if record.schema_version != crate::WORK_GRAPH_SNAPSHOT_SCHEMA_VERSION
        || record.work_id != event.work_id
        || record.project_id != event.project_id
        || record.history.completion.is_none()
        || !crate::graph_snapshot::restored_item_basis_matches(
            &record.item,
            &record.project_id,
            &basis,
        )
        || event.work.lifecycle != WorkLifecycle::Completed
        || event.work.active_run_id.is_some()
        || event.run.as_ref().is_some_and(|run| {
            run.state != WorkRunState::Cancelled
                || run.completion_seal.is_some()
                || run.executor.is_some()
        })
        || event
            .claim
            .as_ref()
            .is_some_and(|claim| claim.state == crate::WorkClaimState::Active)
    {
        return Err(invalid(
            "reported completion differs from its exact inert record",
        ));
    }
    let projected: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_restored_records WHERE work_id=?1
         AND generation_index=?2 AND record_id=?3)",
        params![
            event.work_id.0.to_string(),
            i64::try_from(record.generation_index)
                .map_err(|_| invalid("reported generation exceeds storage bound"))?,
            id.as_str()
        ],
        |row| row.get(0),
    )?;
    if !projected {
        return Err(invalid("reported completion record projection is missing"));
    }
    Ok(Some(record))
}

fn invalid(message: &str) -> StoreError {
    StoreError::InvalidWorkProjection(message.into())
}
