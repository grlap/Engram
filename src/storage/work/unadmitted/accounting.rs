//! Source-change accounting for an unadmitted observation sent under
//! `account_if_eligible`. The decision reads the store's history through the
//! run-feed position just before the record's own, so the write path and
//! doctor reach the same answer: the write path reads at the feed head, and
//! doctor reconstructs at the position before the stored record.

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::super::completion::{
    append_builtin_obligations_on, finished_run_cut_on, newest_change_repeated_on,
};
use super::super::{latest_named_root_event_record_on, named_root_state_on};
use crate::ObjectId;
use crate::canonical::CanonicalObject;
use crate::domain::{
    ControlWorkBinding, FeedPosition, ObservationAccounting, ObservationAuditReason,
    ObservationRootBasis, ObservedSourceChange, SourceObservation, UnadmittedExecutionObservation,
    WorkClaimState, WorkEvent, WorkLifecycle, WorkRunState,
};
use crate::storage::StoreError;

/// How a record bound to `binding`, captured under `root_basis` and
/// reporting `change`, enters accounting on the history through `through`.
///
/// The lifecycle decides first: a run finished by then keeps the record as
/// `finished_run`; a binding that is no longer the run's newest claim epoch
/// (a newer claim or fence, a released claim, or a revised or reassigned
/// item) as `historical_binding`, while an epoch that merely expired still
/// accounts; and a root lifecycle that moved since the capture as
/// `root_basis_moved`. Then no reported change is `no_source_change`, a
/// measured revision that repeats the newest change in the same claim and
/// workspace with no other revision since is a repeat of that change, and
/// anything else is a new change.
///
/// # Errors
///
/// Returns [`StoreError`] when a record cannot be read.
pub(in crate::storage) fn decide_accounting_on(
    connection: &Connection,
    binding: &ControlWorkBinding,
    root_basis: &ObservationRootBasis,
    change: Option<&ObservedSourceChange>,
    through: i64,
) -> Result<ObservationAccounting, StoreError> {
    let audit = |reason| Ok(ObservationAccounting::AuditOnly { reason });
    if finished_run_cut_on(connection, binding.run_id)?.is_some_and(|cut| cut <= through) {
        return audit(ObservationAuditReason::FinishedRun);
    }
    if !claim_epoch_is_latest_on(connection, binding, through)? {
        return audit(ObservationAuditReason::HistoricalBinding);
    }
    if root_basis_moved_on(connection, binding, root_basis, through)? {
        return audit(ObservationAuditReason::RootBasisMoved);
    }
    let Some(change) = change else {
        return Ok(ObservationAccounting::NoSourceChange {});
    };
    if let Some(anchor) = repeated_change_on(connection, binding, change, through)? {
        return Ok(ObservationAccounting::Repeat {
            source_change: anchor,
        });
    }
    Ok(ObservationAccounting::SourceChange {
        source_change: None,
    })
}

/// Whether `binding` is still the run's newest claim epoch through
/// `through`: the newest event records the item open at the bound revision
/// with the run live, and the newest event that records a claim records this
/// claim, fence and revision as active. Expiry does not matter.
fn claim_epoch_is_latest_on(
    connection: &Connection,
    binding: &ControlWorkBinding,
    through: i64,
) -> Result<bool, StoreError> {
    let newest = |with_claim: bool| -> Result<Option<WorkEvent>, StoreError> {
        connection
            .query_row(
                "SELECT object.object_id, object.canonical_json
                 FROM work_feed_entries entry
                 JOIN objects object ON object.object_id = entry.object_id
                 WHERE entry.feed_kind = 'run_execution' AND entry.feed_id = ?1
                   AND entry.position <= ?2
                   AND entry.object_kind = 'work_event' AND object.object_kind = 'work_event'
                   AND (?3 = 0 OR json_type(object.canonical_json, '$.claim') = 'object')
                 ORDER BY entry.position DESC LIMIT 1",
                params![binding.run_id.0.to_string(), through, with_claim],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?
            .map(|(stored, bytes)| {
                let id = ObjectId::from_stored(stored.clone())
                    .ok_or(StoreError::InvalidStoredKey(stored))?;
                CanonicalObject::stored(&id, bytes)?.decode::<WorkEvent>()
            })
            .transpose()
    };
    let item_holds = newest(false)?.is_some_and(|event| {
        event.work.lifecycle == WorkLifecycle::Open
            && event.work.revision == binding.work_revision
            && event.work.active_run_id == Some(binding.run_id)
            && event.run.as_ref().is_some_and(|run| {
                run.run_id == binding.run_id
                    && matches!(run.state, WorkRunState::Claimed | WorkRunState::Active)
            })
    });
    let claim_holds = newest(true)?.is_some_and(|event| {
        event.claim.as_ref().is_some_and(|claim| {
            claim.run_id == binding.run_id
                && claim.claim_id == binding.claim_id
                && claim.fence == binding.claim_fence
                && claim.accepted_work_revision == binding.work_revision
                && claim.state == WorkClaimState::Active
        })
    });
    Ok(item_holds && claim_holds)
}

/// Whether the claim's named-root lifecycle through `through` is no longer
/// the one the host captured: another derived state or newest root event.
fn root_basis_moved_on(
    connection: &Connection,
    binding: &ControlWorkBinding,
    root_basis: &ObservationRootBasis,
    through: i64,
) -> Result<bool, StoreError> {
    let state = named_root_state_on(connection, binding.run_id, binding.claim_id, through)?;
    let latest =
        latest_named_root_event_record_on(connection, binding.run_id, binding.claim_id, through)?
            .map(|(_, id, _)| id);
    Ok(state != root_basis.state || latest != root_basis.latest_event)
}

/// The change a measured report repeats: the newest recorded change in the
/// report's workspace, when it carries the same revision, was recorded under
/// the same claim, and no other revision was seen in that workspace since
/// (the run's repeat rule, scoped to the workspace even without a named
/// root). A watcher-only report carries no revision and never repeats.
fn repeated_change_on(
    connection: &Connection,
    binding: &ControlWorkBinding,
    change: &ObservedSourceChange,
    through: i64,
) -> Result<Option<ObjectId>, StoreError> {
    let Some(sighting) = change.sighting() else {
        return Ok(None);
    };
    let basis = &sighting.source_basis;
    Ok(newest_change_repeated_on(
        connection,
        binding.run_id,
        binding.claim_id,
        basis,
        through,
        true,
    )?
    .filter(|(_, anchor)| {
        anchor.binding.claim_id == binding.claim_id
            && anchor
                .source_basis
                .as_ref()
                .is_some_and(|anchored| anchored.workspace_id == basis.workspace_id)
    })
    .map(|(_, anchor)| anchor.record))
}

/// Opens the selected rule set's obligations for a record accounted as a new
/// change, triggered by the record at its own run-feed position, and returns
/// their definition ids. A record accounted any other way opens nothing.
///
/// # Errors
///
/// Returns [`StoreError`] when a write fails.
pub(in crate::storage) fn open_unadmitted_obligations_on(
    transaction: &Transaction<'_>,
    record: &ObjectId,
    observation: &UnadmittedExecutionObservation,
    position: &FeedPosition,
) -> Result<Vec<ObjectId>, StoreError> {
    if !matches!(
        observation.accounting,
        ObservationAccounting::SourceChange { .. }
    ) {
        return Ok(Vec::new());
    }
    let Some(view) = SourceObservation::unadmitted(record.clone(), observation) else {
        return Ok(Vec::new());
    };
    append_builtin_obligations_on(transaction, &view, position)
}

/// The definition ids of the obligations `record` opened, in run-feed order.
///
/// # Errors
///
/// Returns [`StoreError`] when a row cannot be read.
pub(in crate::storage) fn opened_obligations_of_on(
    connection: &Connection,
    run_id: crate::domain::WorkRunId,
    record: &ObjectId,
) -> Result<Vec<ObjectId>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT entry.object_id FROM work_feed_entries entry
         JOIN objects object ON object.object_id = entry.object_id
         WHERE entry.feed_kind = 'run_execution' AND entry.feed_id = ?1
           AND entry.object_kind = 'work_obligation'
           AND json_extract(object.canonical_json, '$.triggering_observation') = ?2
         ORDER BY entry.position",
    )?;
    statement
        .query_map(params![run_id.0.to_string(), record.as_str()], |row| {
            row.get::<_, String>(0)
        })?
        .map(|stored| {
            let stored = stored?;
            ObjectId::from_stored(stored.clone()).ok_or(StoreError::InvalidStoredKey(stored))
        })
        .collect()
}
