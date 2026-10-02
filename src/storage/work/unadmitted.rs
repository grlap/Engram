//! Work-side checks and feed placement for execution a host observed without
//! admission. The binding is checked against the store's history at the
//! host's capture cut, never against the live claim: a host records what it
//! saw, and a claim that has since expired or moved on keeps that history
//! valid. Nothing here renews a claim, opens a grant or reads authority.

use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::feeds::append_to_work_feeds;
use super::query::load_root_execution;
use super::{
    current_run_feed_cut_on, latest_named_root_event_record_on, load_work_item, load_work_run,
    named_root_state_on,
};
use crate::ObjectId;
use crate::canonical::CanonicalObject;
use crate::domain::{
    ControlWorkBinding, ExecutionSourceBasis, FeedId, FeedPosition, ProjectId, SourceRootState,
    UnadmittedExecutionObservation, WorkClaimState, WorkEvent, WorkLifecycle, WorkRunState,
};
use crate::storage::{SqliteStore, StoreError};

#[cfg(test)]
mod tests;

/// The record kind an unadmitted observation is stored and fed under.
pub(in crate::storage) const UNADMITTED_OBSERVATION_KIND: &str = "unadmitted_execution_observation";

fn mismatch(detail: impl Into<String>) -> StoreError {
    StoreError::ExecutionObservationBasisMismatch(detail.into())
}

/// Checks `binding` and the capture cut against the store's history and
/// returns the canonical work event that records the bound claim epoch at or
/// before the cut. The claim's holder need not be the observing session.
///
/// # Errors
///
/// [`StoreError::ExecutionObservationBasisMismatch`] when the item, run,
/// root execution, claim epoch or cut is not what the store holds; other
/// [`StoreError`] values when a record cannot be read.
pub(in crate::storage) fn historical_claim_epoch_on(
    connection: &Connection,
    project_id: &ProjectId,
    binding: &ControlWorkBinding,
    capture_run_cut: i64,
) -> Result<ObjectId, StoreError> {
    let item = match load_work_item(connection, binding.work_id) {
        Ok(item) => item,
        Err(StoreError::WorkNotFound(_)) => return Err(mismatch("the work item is unknown")),
        Err(error) => return Err(error),
    };
    if item.project_id != *project_id {
        return Err(mismatch("the work item belongs to another project"));
    }
    let run_known: bool = connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_runs WHERE run_id = ?1)",
        [binding.run_id.0.to_string()],
        |row| row.get(0),
    )?;
    if !run_known {
        return Err(mismatch("the run is unknown"));
    }
    let run = load_work_run(connection, binding.run_id)?;
    if run.work_id != binding.work_id || run.root_execution_id != binding.root_execution_id {
        return Err(mismatch(
            "the run does not belong to the bound item and root execution",
        ));
    }
    let head = current_run_feed_cut_on(connection, binding.run_id)?;
    if capture_run_cut > head.position {
        return Err(mismatch(format!(
            "capture_run_cut {capture_run_cut} is beyond the run's feed head {}",
            head.position
        )));
    }
    let mut statement = connection.prepare(
        "SELECT object.object_id, object.canonical_json
         FROM work_feed_entries entry
         JOIN objects object ON object.object_id = entry.object_id
         WHERE entry.feed_kind = 'run_execution' AND entry.feed_id = ?1
           AND entry.position <= ?2
           AND entry.object_kind = 'work_event'
           AND object.object_kind = 'work_event'
           AND json_extract(object.canonical_json, '$.work.revision') = ?3
           AND json_extract(object.canonical_json, '$.claim.claim_id') = ?4
           AND json_extract(object.canonical_json, '$.claim.fence') = ?5
         ORDER BY entry.position DESC",
    )?;
    let mut rows = statement.query(params![
        binding.run_id.0.to_string(),
        capture_run_cut,
        binding.work_revision,
        binding.claim_id.0.to_string(),
        binding.claim_fence,
    ])?;
    while let Some(row) = rows.next()? {
        let stored_id: String = row.get(0)?;
        let id = ObjectId::from_stored(stored_id.clone())
            .ok_or(StoreError::InvalidStoredKey(stored_id))?;
        let event: WorkEvent = CanonicalObject::stored(&id, row.get(1)?)?.decode()?;
        if records_claim_epoch(&event, project_id, binding) {
            return Ok(id);
        }
    }
    Err(mismatch(
        "no canonical event at or before the capture cut records this claim, fence and work revision on the bound run",
    ))
}

/// Whether `event` records `binding`'s claim epoch as an open item's active
/// claim on its live run. The holder is whoever held it; expiry after the
/// event does not matter to history.
fn records_claim_epoch(
    event: &WorkEvent,
    project_id: &ProjectId,
    binding: &ControlWorkBinding,
) -> bool {
    event.project_id == *project_id
        && event.work_id == binding.work_id
        && event.work.lifecycle == WorkLifecycle::Open
        && event.work.revision == binding.work_revision
        && event.work.active_run_id == Some(binding.run_id)
        && event.run.as_ref().is_some_and(|run| {
            run.run_id == binding.run_id
                && run.work_id == binding.work_id
                && run.root_execution_id == binding.root_execution_id
                && matches!(run.state, WorkRunState::Claimed | WorkRunState::Active)
        })
        && event.claim.as_ref().is_some_and(|claim| {
            claim.work_id == binding.work_id
                && claim.run_id == binding.run_id
                && claim.claim_id == binding.claim_id
                && claim.accepted_work_revision == binding.work_revision
                && claim.fence == binding.claim_fence
                && claim.state == WorkClaimState::Active
                && claim.expires_at > event.created_at
        })
}

/// Checks the host's root basis against the claim's named-root history at
/// the capture cut: the derived state and the newest root event must be
/// exactly what the store holds there.
///
/// # Errors
///
/// [`StoreError::ExecutionObservationBasisMismatch`] when either differs.
pub(in crate::storage) fn check_root_basis_on(
    connection: &Connection,
    binding: &ControlWorkBinding,
    root_basis: &crate::domain::ObservationRootBasis,
) -> Result<(), StoreError> {
    let cut = root_basis.capture_run_cut;
    let state = named_root_state_on(connection, binding.run_id, binding.claim_id, cut)?;
    if state != root_basis.state {
        return Err(mismatch(
            "root_basis.state is not the claim's named-root state at the capture cut",
        ));
    }
    let latest =
        latest_named_root_event_record_on(connection, binding.run_id, binding.claim_id, cut)?
            .map(|(_, id, _)| id);
    if latest != root_basis.latest_event {
        return Err(mismatch(
            "root_basis.latest_event is not the claim's newest root event at the capture cut",
        ));
    }
    Ok(())
}

/// Checks the closing sighting of a source change against the root basis it
/// was captured under. A sighting stated `named` at generation g needs the
/// claim bound at exactly g at the cut; one stated `ended` at g needs the
/// root unbound there with g's end as the claim's newest root event. A
/// sighting with no generation states nothing and passes. Checks that ran
/// earlier keep the historical rule in [`check_source_root_at_cut_on`].
///
/// # Errors
///
/// [`StoreError::ExecutionObservationBasisMismatch`] when they disagree.
pub(in crate::storage) fn check_sighting_matches_root_basis_on(
    connection: &Connection,
    binding: &ControlWorkBinding,
    root_basis: &crate::domain::ObservationRootBasis,
    basis: &ExecutionSourceBasis,
) -> Result<(), StoreError> {
    use crate::domain::{NamedRootBindingKind, NamedRootState};
    let (Some(generation), Some(state)) = (basis.source_root_generation, basis.source_root_state)
    else {
        return Ok(());
    };
    let agrees = match state {
        SourceRootState::Named => matches!(
            root_basis.state,
            NamedRootState::Bound { generation: bound, .. } if bound == generation
        ),
        SourceRootState::Ended => {
            root_basis.state == NamedRootState::NoRoot
                && latest_named_root_event_record_on(
                    connection,
                    binding.run_id,
                    binding.claim_id,
                    root_basis.capture_run_cut,
                )?
                .is_some_and(|(_, _, event)| {
                    event.generation == generation && event.kind == NamedRootBindingKind::Ended
                })
        }
    };
    if !agrees {
        return Err(mismatch(format!(
            "the closing sighting states root generation {generation} {}, which is not the root basis at the capture cut",
            match state {
                SourceRootState::Named => "named",
                SourceRootState::Ended => "ended",
            }
        )));
    }
    Ok(())
}

/// Checks that a source basis naming a root generation names one the claim
/// had recorded, in that state, by the capture cut. A basis with no
/// generation names none and passes.
///
/// # Errors
///
/// [`StoreError::ExecutionObservationBasisMismatch`] when no such event is
/// recorded by the cut.
pub(in crate::storage) fn check_source_root_at_cut_on(
    connection: &Connection,
    binding: &ControlWorkBinding,
    basis: &ExecutionSourceBasis,
    capture_run_cut: i64,
    label: &str,
) -> Result<(), StoreError> {
    let (Some(generation), Some(state)) = (basis.source_root_generation, basis.source_root_state)
    else {
        return Ok(());
    };
    let kind = match state {
        SourceRootState::Named => "bound",
        SourceRootState::Ended => "ended",
    };
    let recorded: bool = connection.query_row(
        "SELECT EXISTS (
             SELECT 1 FROM work_feed_entries entry
             JOIN objects object ON object.object_id = entry.object_id
             WHERE entry.feed_kind = 'run_execution' AND entry.feed_id = ?1
               AND entry.position <= ?5
               AND entry.object_kind = 'named_root_binding'
               AND json_extract(object.canonical_json, '$.claim_id') = ?2
               AND json_extract(object.canonical_json, '$.generation') = ?3
               AND json_extract(object.canonical_json, '$.kind') = ?4
         )",
        params![
            binding.run_id.0.to_string(),
            binding.claim_id.0.to_string(),
            generation,
            kind,
            capture_run_cut
        ],
        |row| row.get(0),
    )?;
    if !recorded {
        return Err(mismatch(format!(
            "{label} names {kind} root generation {generation}, which this claim had not recorded by the capture cut"
        )));
    }
    Ok(())
}

/// Whether a stored record still is what its request and the store's history
/// at its capture cut admit: its own shape, the only accounting this build
/// records, the claim epoch event it names, its root basis and every root
/// generation it states, and a cut before its own run-feed position.
///
/// # Errors
///
/// Returns [`StoreError`] only when a record cannot be read.
pub(in crate::storage) fn unadmitted_observation_is_consistent_on(
    connection: &Connection,
    observation: &UnadmittedExecutionObservation,
    object_id: &ObjectId,
) -> Result<bool, StoreError> {
    use crate::domain::{
        ObservationAccounting, ObservationAdmission, ObservationAuditReason, ObservationPolicyBasis,
    };
    if observation.validate_recorded_shape().is_err()
        || observation.admission != ObservationAdmission::Unadmitted
        || !matches!(
            observation.policy_basis,
            ObservationPolicyBasis::AuditOnly {}
        )
        || observation.accounting
            != (ObservationAccounting::AuditOnly {
                reason: ObservationAuditReason::ExplicitAudit,
            })
    {
        return Ok(false);
    }
    let binding = &observation.binding;
    let cut = observation.root_basis.capture_run_cut;
    let own_position: Option<i64> = connection
        .query_row(
            "SELECT position FROM work_feed_entries
             WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND object_id = ?2",
            params![binding.run_id.0.to_string(), object_id.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    if own_position.is_none_or(|position| cut >= position) {
        return Ok(false);
    }
    let epoch_holds = connection
        .query_row(
            "SELECT object.canonical_json FROM work_feed_entries entry
             JOIN objects object ON object.object_id = entry.object_id
             WHERE entry.feed_kind = 'run_execution' AND entry.feed_id = ?1
               AND entry.object_id = ?2 AND entry.position <= ?3
               AND entry.object_kind = 'work_event' AND object.object_kind = 'work_event'",
            params![
                binding.run_id.0.to_string(),
                observation.claim_epoch_event.as_str(),
                cut
            ],
            |row| row.get::<_, Vec<u8>>(0),
        )
        .optional()?
        .and_then(|bytes| {
            CanonicalObject::stored(&observation.claim_epoch_event, bytes)
                .and_then(|object| object.decode::<WorkEvent>())
                .ok()
        })
        .is_some_and(|event| records_claim_epoch(&event, &observation.project_id, binding));
    if !epoch_holds {
        return Ok(false);
    }
    let history_holds = || -> Result<(), StoreError> {
        // The write path names the newest event recording the epoch at the
        // cut, and only that one is consistent.
        let newest = historical_claim_epoch_on(connection, &observation.project_id, binding, cut)?;
        if newest != observation.claim_epoch_event {
            return Err(mismatch(
                "the record names an older claim-epoch event than the newest at its cut",
            ));
        }
        check_root_basis_on(connection, binding, &observation.root_basis)?;
        if let Some(sighting) = observation
            .occurrence
            .source_change()
            .and_then(|change| change.sighting())
        {
            check_sighting_matches_root_basis_on(
                connection,
                binding,
                &observation.root_basis,
                &sighting.source_basis,
            )?;
        }
        for check in observation.occurrence.checks() {
            if let Some(basis) = &check.check.source_basis {
                check_source_root_at_cut_on(connection, binding, basis, cut, "source basis")?;
            }
        }
        Ok(())
    };
    match history_holds() {
        Ok(()) => Ok(true),
        Err(StoreError::ExecutionObservationBasisMismatch(_)) => Ok(false),
        Err(error) => Err(error),
    }
}

/// Stores `observation` and places it on its project, root and run feeds.
/// It opens no obligation and touches no claim, grant or session.
///
/// # Errors
///
/// Returns [`StoreError`] when the binding crosses its canonical run or a
/// write fails.
pub(in crate::storage) fn append_unadmitted_observation_on(
    transaction: &Transaction<'_>,
    observation: &UnadmittedExecutionObservation,
) -> Result<(ObjectId, FeedPosition), StoreError> {
    let item = load_work_item(transaction, observation.binding.work_id)?;
    let run = load_work_run(transaction, observation.binding.run_id)?;
    let root_execution = load_root_execution(transaction, observation.binding.root_execution_id)?;
    if item.project_id != observation.project_id
        || item.root_id != root_execution.root_id
        || run.work_id != item.work_id
        || run.root_execution_id != root_execution.root_execution_id
    {
        return Err(StoreError::InvalidWorkProjection(
            "unadmitted observation binding does not match canonical work state".into(),
        ));
    }
    let object = CanonicalObject::mint(observation)?;
    SqliteStore::insert_object(transaction, UNADMITTED_OBSERVATION_KIND, &object)?;
    let position = append_to_work_feeds(
        transaction,
        &item.project_id,
        item.root_id,
        Some(run.run_id),
        None,
        UNADMITTED_OBSERVATION_KIND,
        &object,
    )?
    .into_iter()
    .find(|position| position.feed == FeedId::RunExecution(run.run_id))
    .ok_or_else(|| {
        StoreError::InvalidWorkProjection(
            "unadmitted observation did not receive a run-feed position".into(),
        )
    })?;
    Ok((object.key().clone(), position))
}
