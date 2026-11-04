//! Execution, run, and claim state readers within their existing snapshot wrappers.

use super::{
    Connection, OptionalExtension, RootExecution, RootExecutionId, StoreError, WorkClaim,
    WorkEvent, WorkId, WorkItem, WorkRun, WorkRunId, decode_canonical_work_event,
    latest_canonical_work_event_on_feed, load_work_run, params, parse_root_execution_id,
};

pub(super) fn load_work_run_on_snapshot(
    connection: &Connection,
    run_id: WorkRunId,
) -> Result<WorkRun, StoreError> {
    let row: Option<(Vec<u8>, bool)> = connection
        .query_row(
            "SELECT run_json,
                    run_id = json_extract(run_json, '$.run_id') AND
                    root_execution_id = json_extract(run_json, '$.root_execution_id') AND
                    work_id = json_extract(run_json, '$.work_id') AND
                    generation = json_extract(run_json, '$.generation') AND
                    COALESCE(executor_session_id, '') = COALESCE(json_extract(run_json, '$.executor'), '') AND
                    state = json_extract(run_json, '$.state') AND
                    revision = json_extract(run_json, '$.revision') AND
                    COALESCE(last_checkpoint_id, '') = COALESCE(json_extract(run_json, '$.last_checkpoint'), '') AND
                    COALESCE(completion_seal_id, '') = COALESCE(json_extract(run_json, '$.completion_seal'), '')
             FROM work_runs WHERE run_id = ?1",
            [run_id.0.to_string()],
            |row| super::super::search_budget::bound_json_row(connection, row),
        )
        .optional()?;
    let (bytes, scalar_bound) =
        row.ok_or_else(|| StoreError::InvalidWorkProjection(format!("run {run_id:?} is missing")))?;
    let run: WorkRun = serde_json::from_slice(&bytes)?;
    let event = latest_canonical_work_event_on_feed(
        connection,
        "run_execution",
        &run_id.0.to_string(),
        "$.run",
    )?;
    if !scalar_bound || run.run_id != run_id || event.run.as_ref() != Some(&run) {
        return Err(StoreError::InvalidWorkProjection(format!(
            "work run {run_id:?} differs from its scalar or canonical event binding"
        )));
    }
    Ok(run)
}

pub(in crate::storage::work) fn active_run_snapshot(
    connection: &Connection,
    item: &WorkItem,
) -> Result<Option<WorkRun>, StoreError> {
    item.active_run_id
        .map(|run_id| load_work_run(connection, run_id))
        .transpose()
}

pub(in crate::storage::work) fn verify_root_execution_reference_on(
    connection: &Connection,
    address: &crate::domain::RootExecutionRef,
) -> Result<(), StoreError> {
    let event = latest_canonical_work_event_on_feed(
        connection,
        "root_work",
        &address.root_id.0.to_string(),
        "$.root_execution",
    )?;
    if event.root_execution.as_ref() != Some(address) {
        return Err(StoreError::InvalidWorkProjection(
            "root execution differs from its canonical event binding".into(),
        ));
    }
    Ok(())
}

pub(super) fn load_retained_root_execution_on_snapshot(
    connection: &Connection,
    root_execution_id: RootExecutionId,
) -> Result<RootExecution, StoreError> {
    let (execution, address) = super::super::root_state::projected(connection, root_execution_id)?;
    verify_retained_root_execution_reference_on(connection, &execution, &address)?;
    Ok(execution)
}

pub(in crate::storage::work) fn verify_retained_root_execution_reference_on(
    connection: &Connection,
    execution: &RootExecution,
    address: &crate::domain::RootExecutionRef,
) -> Result<(), StoreError> {
    let root_execution_id = execution.root_execution_id;
    let stored = connection
        .query_row(
            "SELECT object.object_id, object.canonical_json
             FROM work_feed_entries entry
             JOIN objects object ON object.object_id = entry.object_id
             WHERE entry.feed_kind = 'root_work' AND entry.feed_id = ?1
               AND entry.object_kind = 'work_event'
               AND object.object_kind = 'work_event'
               AND json_extract(object.canonical_json, '$.root_execution.root_execution_id') = ?2
               AND json_extract(object.canonical_json, '$.root_execution.generation') = ?3
             ORDER BY entry.position DESC LIMIT 1",
            params![
                execution.root_id.0.to_string(),
                root_execution_id.0.to_string(),
                execution.generation
            ],
            |row| {
                super::super::search_budget::record_row(connection, row)?;
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            },
        )
        .optional()?
        .ok_or_else(|| {
            StoreError::InvalidWorkProjection(
                "retained root execution has no generation-bound canonical event".into(),
            )
        })?;
    if decode_canonical_work_event(stored)?.root_execution.as_ref() != Some(address) {
        return Err(StoreError::InvalidWorkProjection(
            "retained root execution differs from its canonical event binding".into(),
        ));
    }
    Ok(())
}

fn bind_root_execution_event(
    execution: RootExecution,
    address: &crate::domain::RootExecutionRef,
    event: &WorkEvent,
) -> Result<RootExecution, StoreError> {
    if event.root_execution.as_ref() != Some(address) {
        return Err(StoreError::InvalidWorkProjection(format!(
            "root execution {:?} differs from its scalar or canonical event binding",
            execution.root_execution_id
        )));
    }
    Ok(execution)
}

pub(super) fn active_root_execution_on_snapshot(
    connection: &Connection,
    root_id: WorkId,
) -> Result<Option<RootExecution>, StoreError> {
    let row: Option<String> = connection
        .query_row(
            "SELECT root_execution_id
             FROM work_root_executions
             WHERE root_id = ?1 AND state = 'active'",
            [root_id.0.to_string()],
            |row| row.get(0),
        )
        .optional()?;
    let Some(id) = row else {
        return Ok(None);
    };
    let (execution, address) =
        super::super::root_state::projected(connection, parse_root_execution_id(&id)?)?;
    let event = latest_canonical_work_event_on_feed(
        connection,
        "root_work",
        &root_id.0.to_string(),
        "$.root_execution",
    )?;
    if execution.root_id != root_id {
        return Err(StoreError::InvalidWorkProjection(format!(
            "active root execution for {root_id:?} differs from canonical history"
        )));
    }
    Ok(Some(bind_root_execution_event(
        execution, &address, &event,
    )?))
}

pub(super) fn load_work_claim_on_snapshot(
    connection: &Connection,
    run_id: WorkRunId,
) -> Result<Option<WorkClaim>, StoreError> {
    let row = connection
        .query_row(
            "SELECT claim_json,
                    run_id = json_extract(claim_json, '$.run_id') AND
                    work_id = json_extract(claim_json, '$.work_id') AND
                    claim_id = json_extract(claim_json, '$.claim_id') AND
                    holder_session_id = json_extract(claim_json, '$.holder') AND
                    state = json_extract(claim_json, '$.state') AND
                    revision = json_extract(claim_json, '$.revision') AND
                    fence = json_extract(claim_json, '$.fence') AND
                    expires_at_ms = CAST(strftime('%s', json_extract(claim_json, '$.expires_at')) AS INTEGER) * 1000
                        + CASE WHEN instr(json_extract(claim_json, '$.expires_at'), '.') > 0
                            THEN CAST(substr(
                                substr(
                                    json_extract(claim_json, '$.expires_at'),
                                    instr(json_extract(claim_json, '$.expires_at'), '.') + 1,
                                    instr(json_extract(claim_json, '$.expires_at'), 'Z')
                                        - instr(json_extract(claim_json, '$.expires_at'), '.') - 1
                                ) || '000', 1, 3
                            ) AS INTEGER)
                            ELSE 0 END
             FROM work_claims WHERE run_id = ?1",
            [run_id.0.to_string()],
            |row| super::super::search_budget::bound_json_row(connection, row),
        )
        .optional()?;
    let required_snapshot = if row.is_some() { "$.claim" } else { "$.run" };
    let event = latest_canonical_work_event_on_feed(
        connection,
        "run_execution",
        &run_id.0.to_string(),
        required_snapshot,
    )?;
    match row {
        None if event.claim.is_none() => Ok(None),
        Some((bytes, scalar_bound)) => {
            let claim: WorkClaim = serde_json::from_slice(&bytes)?;
            if !scalar_bound || claim.run_id != run_id || event.claim.as_ref() != Some(&claim) {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "work claim for {run_id:?} differs from canonical history"
                )));
            }
            Ok(Some(claim))
        }
        None => Err(StoreError::InvalidWorkProjection(format!(
            "canonical run {run_id:?} has a missing claim projection"
        ))),
    }
}
