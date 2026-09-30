//! Every evaluation record of a run, for the explicit history window and the
//! complete detail of one record. While the run is the item's active run, each
//! record is judged the way the newest one is, at the same read and under the
//! current policy with the source unmeasured, so an older record carries only
//! its own stale reason, never one merely because a later record exists. Once
//! the run has ended, its records are not judged: completion, cancellation and
//! supersession revise the item, and comparing a record with the item after
//! its run ended would call every record stale, including the one a seal
//! consumed. Completion still consults only the newest record.

use super::{
    AcceptanceEvaluation, AcceptanceStaleReason, DecidingObservation, KIND, ObjectId, SourceCheck,
    SqliteStore, StoreError, WorkId, WorkItem, WorkRunId, latest_on, load_typed_work_object,
    load_work_item, on_one_snapshot, params, staleness_named,
};
use rusqlite::OptionalExtension;

/// One evaluation record on a run feed, at its dense run-feed position.
#[derive(Clone, Debug, Eq, PartialEq)]
pub(crate) struct AcceptanceEvaluationEntry {
    pub position: i64,
    pub evaluation: ObjectId,
}

/// The run an item's source observations were read from, and each
/// observation's run-feed position and record id, in position order.
pub(crate) type SourceObservationEntries = (WorkRunId, Vec<(i64, ObjectId)>);

/// The run an item's evaluation history covers, and its records.
#[derive(Clone, Debug)]
pub(crate) struct AcceptanceEvaluationHistory {
    pub run_id: WorkRunId,
    /// Whether the run is still the item's active run. Records of an ended
    /// run are listed but not judged.
    pub active: bool,
    pub entries: Vec<AcceptanceEvaluationEntry>,
}

/// A decoded record with the stale reason it has at this read.
#[derive(Clone, Debug)]
pub(crate) struct AssessedAcceptanceEvaluation {
    pub position: i64,
    pub evaluation: ObjectId,
    pub record: AcceptanceEvaluation,
    /// `None` when fresh or when not judged.
    pub stale: Option<AcceptanceStaleReason>,
    /// The source observation that decided the move the record reads stale
    /// for, when an observation decided it.
    pub stale_observation: Option<DecidingObservation>,
    /// Whether the record was judged at this read: false once its run ended.
    pub judged: bool,
    /// Whether it is the newest record on the history run, the one completion
    /// reads.
    pub newest: bool,
}

impl SqliteStore {
    /// The run an item's evaluation history covers, and every evaluation on
    /// that run's feed in position order. An item with an active run reads
    /// that run; otherwise, such as once it is completed, its latest run.
    /// `None` when the item has no run.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the item, its runs, or the feed cannot be
    /// read.
    pub(crate) fn acceptance_evaluation_entries(
        &self,
        work_id: WorkId,
    ) -> Result<Option<AcceptanceEvaluationHistory>, StoreError> {
        on_one_snapshot(&self.connection, |connection| {
            let item = load_work_item(connection, work_id)?;
            let Some(run_id) = self.history_run(&item)? else {
                return Ok(None);
            };
            let rows = connection
                .prepare(
                    "SELECT position, object_id FROM work_feed_entries
                     WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND object_kind = ?2
                     ORDER BY position",
                )?
                .query_map(params![run_id.0.to_string(), KIND], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let entries = rows
                .into_iter()
                .map(|(position, stored)| {
                    let evaluation = ObjectId::from_stored(stored.clone())
                        .ok_or(StoreError::InvalidStoredKey(stored))?;
                    Ok(AcceptanceEvaluationEntry {
                        position,
                        evaluation,
                    })
                })
                .collect::<Result<Vec<_>, StoreError>>()?;
            Ok(Some(AcceptanceEvaluationHistory {
                run_id,
                active: item.active_run_id == Some(run_id),
                entries,
            }))
        })
    }

    /// Decodes the selected records of `run_id`, the run
    /// [`Self::acceptance_evaluation_entries`] chose, and judges each at this
    /// read while that run is still active. `newest` names the run's newest
    /// record.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when a record is missing, bound to another item
    /// or run, or cannot be judged.
    pub(crate) fn assess_acceptance_evaluations(
        &self,
        work_id: WorkId,
        run_id: WorkRunId,
        entries: &[AcceptanceEvaluationEntry],
        newest: &ObjectId,
    ) -> Result<Vec<AssessedAcceptanceEvaluation>, StoreError> {
        on_one_snapshot(&self.connection, |connection| {
            let item = load_work_item(connection, work_id)?;
            let judged = item.active_run_id == Some(run_id);
            let policy = SqliteStore::load_acceptance_evaluation_policy_on(connection)?;
            entries
                .iter()
                .map(|entry| {
                    let record: AcceptanceEvaluation =
                        load_typed_work_object(connection, &entry.evaluation, KIND)?;
                    if record.run_id != run_id || record.work_id != work_id {
                        return Err(StoreError::InvalidWorkProjection(
                            "acceptance evaluation on a run feed is bound to another item or run"
                                .into(),
                        ));
                    }
                    let (stale, stale_observation) = if judged {
                        staleness_named(
                            connection,
                            &item,
                            run_id,
                            &policy,
                            &record,
                            SourceCheck::Unmeasured,
                        )?
                    } else {
                        (None, None)
                    };
                    Ok(AssessedAcceptanceEvaluation {
                        position: entry.position,
                        newest: entry.evaluation == *newest,
                        evaluation: entry.evaluation.clone(),
                        record,
                        stale,
                        stale_observation,
                        judged,
                    })
                })
                .collect()
        })
    }

    /// One evaluation record of this item, from any of its runs. While the
    /// item has an active run, the record is judged against that run, so a
    /// record of an earlier run is stale for that reason; once no run is
    /// active, it is not judged. `None` when no evaluation of this item has
    /// that id.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the item or the record cannot be read.
    pub(crate) fn acceptance_evaluation_of_item(
        &self,
        work_id: WorkId,
        evaluation: &ObjectId,
    ) -> Result<Option<AssessedAcceptanceEvaluation>, StoreError> {
        on_one_snapshot(&self.connection, |connection| {
            // An id of any other kind, such as a note or gate locator, is no
            // evaluation of this item: answered as absent, never as a store
            // fault that would name what the id is.
            let object = match SqliteStore::get_canonical_object_on(connection, evaluation, KIND) {
                Ok(Some(object)) => object,
                Ok(None) | Err(StoreError::ObjectKindMismatch { .. }) => return Ok(None),
                Err(error) => return Err(error),
            };
            let record: AcceptanceEvaluation = object.decode()?;
            if record.work_id != work_id {
                return Ok(None);
            }
            let Some(position) = connection
                .query_row(
                    "SELECT position FROM work_feed_entries
                     WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND object_id = ?2",
                    params![record.run_id.0.to_string(), evaluation.as_str()],
                    |row| row.get::<_, i64>(0),
                )
                .optional()?
            else {
                return Err(StoreError::InvalidWorkProjection(
                    "acceptance evaluation is missing from its run feed".into(),
                ));
            };
            let item = load_work_item(connection, work_id)?;
            let history_run = self.history_run(&item)?;
            let (stale, stale_observation, judged) = match item.active_run_id {
                Some(run_id) => {
                    let policy = SqliteStore::load_acceptance_evaluation_policy_on(connection)?;
                    let (stale, stale_observation) = staleness_named(
                        connection,
                        &item,
                        run_id,
                        &policy,
                        &record,
                        SourceCheck::Unmeasured,
                    )?;
                    (stale, stale_observation, true)
                }
                None => (None, None, false),
            };
            let newest = match history_run {
                Some(run_id) => {
                    latest_on(connection, run_id)?.is_some_and(|(latest, _)| latest == *evaluation)
                }
                None => false,
            };
            Ok(Some(AssessedAcceptanceEvaluation {
                position,
                evaluation: evaluation.clone(),
                record,
                stale,
                stale_observation,
                judged,
                newest,
            }))
        })
    }

    /// The source observations on the run an item's evaluation history
    /// covers, each at its run-feed position, in position order: every
    /// execution observation, whatever workspace reported it. `None` when the
    /// item has no run.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the item, its runs, or the feed cannot be
    /// read.
    pub(crate) fn source_observation_entries(
        &self,
        work_id: WorkId,
    ) -> Result<Option<SourceObservationEntries>, StoreError> {
        on_one_snapshot(&self.connection, |connection| {
            let item = load_work_item(connection, work_id)?;
            let Some(run_id) = self.history_run(&item)? else {
                return Ok(None);
            };
            let rows = connection
                .prepare(
                    "SELECT position, object_id FROM work_feed_entries
                     WHERE feed_kind = 'run_execution' AND feed_id = ?1
                       AND object_kind = 'execution_observation'
                     ORDER BY position",
                )?
                .query_map(params![run_id.0.to_string()], |row| {
                    Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            let entries = rows
                .into_iter()
                .map(|(position, stored)| {
                    let observation = ObjectId::from_stored(stored.clone())
                        .ok_or(StoreError::InvalidStoredKey(stored))?;
                    Ok((position, observation))
                })
                .collect::<Result<Vec<_>, StoreError>>()?;
            Ok(Some((run_id, entries)))
        })
    }

    /// Decodes the selected source observations, keeping each position.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when an observation is missing or invalid.
    pub(crate) fn source_observations(
        &self,
        entries: &[(i64, ObjectId)],
    ) -> Result<Vec<(i64, ObjectId, crate::domain::ExecutionObservation)>, StoreError> {
        on_one_snapshot(&self.connection, |connection| {
            entries
                .iter()
                .map(|(position, hash)| {
                    let observation =
                        load_typed_work_object(connection, hash, "execution_observation")?;
                    Ok((*position, hash.clone(), observation))
                })
                .collect()
        })
    }

    /// The run an item's evaluation history covers: its active run, or its
    /// latest run when none is active.
    fn history_run(&self, item: &WorkItem) -> Result<Option<WorkRunId>, StoreError> {
        match item.active_run_id {
            Some(run_id) => Ok(Some(run_id)),
            None => Ok(self.latest_work_run(item.work_id)?.map(|run| run.run_id)),
        }
    }
}
