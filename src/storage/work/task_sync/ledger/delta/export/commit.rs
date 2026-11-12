//! Record the actual retained commit without exporting or publishing again.

use super::{
    ObjectId, SqliteStore, StoreError, Transition, invalid, producing_origin, require, view,
};
use crate::task_sync::{delta::DeltaCommitSelection, exchange::delta::InspectedDelta, retention};
use chrono::{DateTime, Utc};

impl SqliteStore {
    pub(crate) fn record_task_sync_delta_export_commit(
        &mut self,
        input: &InspectedDelta,
        selection: &DeltaCommitSelection,
        now: DateTime<Utc>,
    ) -> Result<ObjectId, StoreError> {
        crate::storage::admit_live_actor_session(&input.intent.actor)?;
        selection.validate(
            &input.binding_id,
            &input.binding,
            &input.intent,
            &selection.prepared,
        )?;
        let request = retention::delta::commit_request_fingerprint(
            &input.binding_id,
            &input.intent,
            selection,
        )
        .map_err(|error| invalid(error.to_string()))?;
        producing_origin::with_exchange(
            self,
            &input.binding.project,
            &input.binding.repository.git_directory,
            Some(input.intent.operation),
            &request,
            |store, guard| {
                let transaction = store.begin_work_mutation()?;
                require(
                    guard.is_some(),
                    "delta commit requires private origin continuity",
                )?;
                producing_origin::check_exchange_on(
                    &transaction,
                    &input.binding.project,
                    guard,
                    &request,
                )?;
                let mut ledger = checked_on(&transaction, input, selection, guard)?;
                let transition = Transition::DeltaLocalCommitRecorded {
                    prepared: selection.prepared.clone(),
                    commit: selection.commit.clone(),
                };
                if let Some((id, _)) = ledger.events.iter().find(|(_, event)| {
                    event.binding == input.binding_id && event.transition == transition
                }) {
                    return Ok(id.clone());
                }
                let object =
                    super::super::append(&transaction, &mut ledger, input, transition, now)?;
                transaction.commit()?;
                Ok(object.key().clone())
            },
        )
    }
}

/// Both commit binding and publication use the same exact preparation,
/// retained bytes, origin, supersession and exhaustive custody admission.
pub(super) fn checked_on(
    connection: &rusqlite::Connection,
    input: &InspectedDelta,
    selection: &DeltaCommitSelection,
    guard: Option<&crate::task_sync::origin::continuity::PreparedContinuity>,
) -> Result<super::super::super::Ledger, StoreError> {
    checked_with_resolution_on(connection, input, selection, None, guard)
}

pub(super) fn checked_with_resolution_on(
    connection: &rusqlite::Connection,
    input: &InspectedDelta,
    selection: &DeltaCommitSelection,
    resolution: Option<&crate::task_sync::exchange::InspectedDeltaResolution>,
    guard: Option<&crate::task_sync::origin::continuity::PreparedContinuity>,
) -> Result<super::super::super::Ledger, StoreError> {
    super::super::local_store(connection, &input.binding)?;
    let actual = super::prepare::input_on(connection, &input.binding_id, &input.intent)?;
    require(
        actual.binding == input.binding && actual.payload == input.payload,
        "delta commit differs from its original preparation",
    )?;
    let prepared = view::read::request_by(
        connection,
        &input.binding_id,
        input.intent.operation,
        &input.intent.key,
    )?;
    require(
        prepared.as_ref() == Some(&selection.prepared),
        "delta commit has no exact canonical preparation",
    )?;
    input
        .recheck_candidate()
        .map_err(|error| invalid(error.to_string()))?;
    require(
        retention::commit_selection(
            &input.binding.repository.git_directory,
            input.intent.operation,
        )
        .map_err(|error| invalid(error.to_string()))?
        .as_ref()
            == Some(&selection.commit),
        "delta commit selection is missing or changed",
    )?;
    // Keep the existing exhaustive custody/capacity contract explicit.
    let ledger = super::super::super::Ledger::load(connection)?;
    let root = &input.binding.repository.git_directory;
    let observed = guard
        .and_then(|guard| guard.custody().get(root))
        .ok_or_else(|| invalid("delta custody observation missing before writer admission"))?;
    let operation = if let Some(resolution) = resolution {
        resolution
            .operation()
            .map_err(|error| invalid(error.to_string()))?
    } else {
        input.intent.operation
    };
    super::super::super::custody::check(
        &ledger,
        root,
        observed,
        Some(operation),
        super::super::super::same_directory,
    )?;
    require(
        !ledger
            .bindings
            .values()
            .any(|binding| binding.supersedes.as_ref() == Some(&input.binding_id)),
        "delta commit binding was superseded",
    )?;
    require(
        !super::super::super::recovery::quarantined(connection, &input.binding_id, &input.binding)?,
        "delta commit binding was quarantined",
    )?;
    Ok(ledger)
}
