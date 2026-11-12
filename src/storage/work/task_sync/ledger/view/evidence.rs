//! Exact evidence-family replay uses canonical links before current payloads.

use super::{Connection, ObjectId, SqliteStore, StoreError, read, require};
use crate::task_sync::ledger::{CompletedExchange, EvidenceIntent};

impl SqliteStore {
    /// Read the original evidence outcome without opening Git or pack files.
    /// This grants no destination verification, import or continuation authority.
    ///
    /// # Errors
    /// Refuses changed requests, broken canonical links or stale keyed views.
    pub fn completed_task_sync_evidence(
        &self,
        binding: &ObjectId,
        intent: &EvidenceIntent,
    ) -> Result<Option<CompletedExchange>, StoreError> {
        self.work_read_snapshot(|store| lookup(&store.connection, binding, intent))
    }
}

pub(in crate::storage::work::task_sync::ledger) fn lookup(
    connection: &Connection,
    binding_id: &ObjectId,
    intent: &EvidenceIntent,
) -> Result<Option<CompletedExchange>, StoreError> {
    let mut budget = read::ReadBudget::default();
    let binding = read::binding_on(connection, binding_id, &mut budget)?;
    intent.validate(binding_id, &binding)?;
    read::check_head(connection, binding_id)?;
    let Some(prepared) = read::request_by(connection, binding_id, intent.operation, &intent.key)?
    else {
        let orphan: bool = connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_task_sync_exchange_operations WHERE binding_id=?1 AND operation=?2)",
            rusqlite::params![binding_id.as_str(), intent.operation.to_string()], |row| row.get(0))?;
        require(
            !orphan,
            "orphan evidence operation projection; repair projections explicitly",
        )?;
        return Ok(None);
    };
    let operation = read::operation_on(connection, binding_id, &binding, &prepared, &mut budget)?;
    require(
        matches!(&operation.intent, read::CheckedRequest::Evidence(original) if original.as_ref() == intent),
        "evidence request differs from its recorded family or intent",
    )?;
    let Some(outcome) = operation.progress.completed else {
        return Ok(None);
    };
    let event = operation
        .events
        .into_iter()
        .find(|(id, _)| id == &outcome)
        .ok_or_else(|| super::super::super::invalid("completed evidence outcome is missing"))?
        .1;
    Ok(Some(CompletedExchange {
        prepared,
        outcome,
        event,
    }))
}
