//! Read-only classification of what completion can do with an open obligation,
//! and the check that each recorded source change holds the obligations its
//! rules call for.

use rusqlite::Connection;

use super::super::WorkObligationCompletionAction;
use super::named_root::{ForeignChange, classify_change_on};
use super::{
    CompletionSeal, FeedId, NamedRootContext, ObjectId, SqliteStore, StoreError, WorkObligation,
    WorkObligationRecord, WorkRunId, latest_source_mutation_on, load_typed_work_object,
    load_work_claim_optional, load_work_run, named_root_context_on,
    obligation_rule_set_for_observation_on,
};

/// One-based positions of the criteria a self-asserted completion would seal
/// with no evidence link unless the completing call links evidence to them:
/// every unbound criterion, and every bound one whose newest binding
/// obligation is not satisfied. A satisfied binding is the only link
/// completion adds on its own, decided by the same predicate the seal uses.
/// It reports what is linked now, not a forecast: completion checks a
/// satisfied binding again and may refuse it.
pub(crate) fn criteria_without_evidence_link(
    item: &crate::WorkItem,
    records: &[WorkObligationRecord],
) -> Vec<usize> {
    (1..=item.acceptance.len())
        .filter(|position| {
            !item.acceptance_bindings.iter().any(|binding| {
                binding.criterion == *position
                    && super::satisfied_binding(records, binding).is_some()
            })
        })
        .collect()
}

/// Whether any of `positions` is a criterion whose binding still owes its
/// check: the binding's newest obligation, matched as completion matches it,
/// is open. A waived or satisfied one owes nothing, and an unbound criterion
/// has no check to pass.
pub(crate) fn unlinked_criteria_owe_bound_check(
    item: &crate::WorkItem,
    records: &[WorkObligationRecord],
    positions: &[usize],
) -> bool {
    item.acceptance_bindings.iter().any(|binding| {
        positions.contains(&binding.criterion)
            && super::binding_obligation(records, binding)
                .is_some_and(|record| record.state == crate::domain::WorkObligationState::Open)
    })
}

impl SqliteStore {
    /// Guidance at the current claim's named-root cut; completion classifies
    /// again. A bounded page loads each run's claim and root once.
    pub(crate) fn work_obligation_completion_actions(
        &self,
        obligations: &[&WorkObligation],
    ) -> Result<Vec<WorkObligationCompletionAction>, StoreError> {
        let mut context: Option<(WorkRunId, bool, Option<NamedRootContext>, bool)> = None;
        let mut actions = Vec::with_capacity(obligations.len());
        for obligation in obligations {
            if !crate::control::is_source_change_obligation(&obligation.rule) {
                actions.push(WorkObligationCompletionAction::CheckOrWaiver);
                continue;
            }
            if context
                .as_ref()
                .is_none_or(|(run_id, _, _, _)| *run_id != obligation.run_id)
            {
                let claim = load_work_claim_optional(&self.connection, obligation.run_id)?;
                let root = claim
                    .as_ref()
                    .map(|claim| {
                        named_root_context_on(
                            &self.connection,
                            obligation.run_id,
                            claim.claim_id,
                            i64::MAX,
                        )
                    })
                    .transpose()?
                    .flatten();
                let latest_unverifiable = if claim.is_some() && root.is_none() {
                    latest_source_mutation_on(&self.connection, obligation.run_id, i64::MAX)?
                        .is_some_and(|(_, mutation)| {
                            mutation.source_basis.is_none() || mutation.observed_at.is_none()
                        })
                } else {
                    false
                };
                context = Some((
                    obligation.run_id,
                    claim.is_some(),
                    root,
                    latest_unverifiable,
                ));
            }
            let (_, has_claim, root, latest_unverifiable) =
                context.as_ref().expect("source context initialized");
            if !has_claim {
                actions.push(WorkObligationCompletionAction::CheckOrWaiver);
                continue;
            }
            let change = super::super::feeds::load_source_observation_on(
                &self.connection,
                &obligation.triggering_observation,
            )?;
            let class = classify_change_on(
                &self.connection,
                root.as_ref(),
                &change,
                obligation.trigger_position.position,
            )?;
            actions.push(match class {
                ForeignChange::Displaced => WorkObligationCompletionAction::DoneDisplaces,
                ForeignChange::Open => WorkObligationCompletionAction::WaiverOnly,
                ForeignChange::Unlocated if root.is_none() => {
                    WorkObligationCompletionAction::NameRootCheckOrWaiver
                }
                ForeignChange::Unlocated => WorkObligationCompletionAction::CheckOrWaiver,
                ForeignChange::NotForeign | ForeignChange::Unbound
                    if crate::control::is_stock_source_change_obligation(
                        &obligation.rule,
                        &obligation.requirement,
                    ) =>
                {
                    WorkObligationCompletionAction::DoneWaives
                }
                ForeignChange::NotForeign | ForeignChange::Unbound if *latest_unverifiable => {
                    WorkObligationCompletionAction::NameRootCheckOrWaiver
                }
                ForeignChange::NotForeign | ForeignChange::Unbound => {
                    WorkObligationCompletionAction::CheckOrWaiver
                }
            });
        }
        Ok(actions)
    }
}

pub(super) fn require_expected_obligations_on(
    connection: &Connection,
    run_id: WorkRunId,
    records: &[WorkObligationRecord],
) -> Result<(), StoreError> {
    let expected = connection
        .prepare(&format!(
            "SELECT entry.position, entry.object_id
             FROM work_feed_entries entry
             JOIN objects object ON object.object_id = entry.object_id
             WHERE entry.feed_kind = 'run_execution' AND entry.feed_id = ?1
               AND {} AND {}
             ORDER BY entry.position",
            super::super::feeds::SOURCE_RECORD_SQL,
            super::super::feeds::SOURCE_CHANGED_SQL
        ))?
        .query_map([run_id.0.to_string()], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    let finished_at = finished_run_cut_on(connection, run_id)?;
    for (position, stored_hash) in expected {
        let hash = ObjectId::from_stored(stored_hash.clone())
            .ok_or(StoreError::InvalidStoredKey(stored_hash))?;
        if finished_at.is_some_and(|cut| position > cut)
            && !records
                .iter()
                .any(|record| record.obligation.triggering_observation == hash)
        {
            continue;
        }
        let observation = super::super::feeds::load_source_observation_on(connection, &hash)?;
        let rule_set = obligation_rule_set_for_observation_on(connection, &observation)?;
        for (rule, requirement) in
            crate::control::evaluate_obligation_rules(&rule_set, observation.source_changed)
        {
            let matches = records
                .iter()
                .filter(|record| {
                    record.obligation.run_id == run_id
                        && record.obligation.triggering_observation == hash
                        && record.obligation.trigger_position.position == position
                        && record.obligation.rule_set == observation.obligation_rule_set
                        && record.obligation.rule == rule
                        && record.obligation.requirement == requirement
                })
                .count();
            if matches != 1 {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "run {run_id:?} source mutation {hash} has {matches} matching builtin obligation definitions"
                )));
            }
        }
    }
    Ok(())
}

/// The run-feed position at which a finished run was sealed. A source change
/// recorded after it opens no obligation, so the checks that expect one per
/// evaluated rule accept none for it, or the full set an older build opened.
/// A run or seal that is missing or cannot be decoded gives no cut: the checks
/// stay strict, and the damage is reported where the run or seal is read. A
/// SQLite failure while reading either is returned as itself, so it is never
/// reported as a missing obligation.
pub(in crate::storage::work) fn finished_run_cut_on(
    connection: &Connection,
    run_id: WorkRunId,
) -> Result<Option<i64>, StoreError> {
    let Some(run) = readable(load_work_run(connection, run_id))? else {
        return Ok(None);
    };
    let Some(seal_id) = run.completion_seal.as_ref().filter(|_| run.is_finished()) else {
        return Ok(None);
    };
    let Some(seal) = readable::<CompletionSeal>(load_typed_work_object(
        connection,
        seal_id,
        "completion_seal",
    ))?
    else {
        return Ok(None);
    };
    Ok(
        (seal.run_id == run_id && seal.completion_cut.feed == FeedId::RunExecution(run_id))
            .then_some(seal.completion_cut.position),
    )
}

/// A read's value, `None` when the record is missing or undecodable, or the
/// SQLite failure that kept it from being read. A column that holds a value of
/// the wrong type or range, or text that is not UTF-8, is a damaged row, so it
/// reads as undecodable. SQLite's own error for malformed JSON in an
/// expression cannot be told apart from other SQLite failures, so it is
/// returned as one.
fn readable<T>(read: Result<T, StoreError>) -> Result<Option<T>, StoreError> {
    match read {
        Ok(value) => Ok(Some(value)),
        Err(StoreError::Sqlite(
            rusqlite::Error::InvalidColumnType(..)
            | rusqlite::Error::FromSqlConversionFailure(..)
            | rusqlite::Error::IntegralValueOutOfRange(..)
            | rusqlite::Error::Utf8Error(..),
        )) => Ok(None),
        Err(error @ StoreError::Sqlite(_)) => Err(error),
        Err(_) => Ok(None),
    }
}
