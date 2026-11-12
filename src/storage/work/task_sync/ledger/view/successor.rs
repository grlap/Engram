//! Exact successor-family replay uses canonical links before current payloads.

use super::{Connection, ObjectId, SqliteStore, StoreError, read, require};
use crate::task_sync::{delta::PlanningSuccessorRequest, ledger::CompletedExchange};

impl SqliteStore {
    /// Read the original planning successor outcome without opening Git or pack files.
    /// This grants no destination execution or continuation authority.
    ///
    /// # Errors
    /// Refuses changed requests, broken canonical links or stale keyed views.
    pub fn completed_task_sync_planning_successor(
        &self,
        binding: &ObjectId,
        request: &PlanningSuccessorRequest,
    ) -> Result<Option<CompletedExchange>, StoreError> {
        self.work_read_snapshot(|store| lookup(&store.connection, binding, request))
    }
}

pub(in crate::storage::work::task_sync::ledger) fn lookup(
    connection: &Connection,
    binding_id: &ObjectId,
    request: &PlanningSuccessorRequest,
) -> Result<Option<CompletedExchange>, StoreError> {
    let mut budget = read::ReadBudget::default();
    let binding = read::binding_on(connection, binding_id, &mut budget)?;
    request.intent.validate_successor(binding_id, &binding)?;
    let intent = &request.intent;
    read::check_head(connection, binding_id)?;
    let Some(prepared) = read::request_by(connection, binding_id, intent.operation, &intent.key)?
    else {
        let orphan: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_task_sync_exchange_operations WHERE binding_id=?1 AND operation=?2)",
            rusqlite::params![binding_id.as_str(), intent.operation.to_string()], |row| row.get(0))?;
        require(
            !orphan,
            "orphan planning successor operation projection; repair projections explicitly",
        )?;
        return Ok(None);
    };
    let operation = read::operation_on(connection, binding_id, &binding, &prepared, &mut budget)?;
    require(
        matches!(&operation.intent, read::CheckedRequest::Successor(original) if &original.request == request),
        "planning successor request differs from its recorded family or intent",
    )?;
    let Some(outcome) = operation.progress.completed else {
        return Ok(None);
    };
    let event = operation
        .events
        .into_iter()
        .find(|(id, _)| id == &outcome)
        .ok_or_else(|| {
            super::super::super::invalid("completed planning successor outcome is missing")
        })?
        .1;
    Ok(Some(CompletedExchange {
        prepared,
        outcome,
        event,
    }))
}
