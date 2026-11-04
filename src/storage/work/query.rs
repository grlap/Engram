use chrono::{DateTime, Utc};
mod activity;

mod blockers;
mod canonical;
mod execution_state;
mod restored;
pub(in crate::storage) use blockers::{
    load_active_blocker_projections, load_blocker_cleared_at, load_cleared_blocker_projection,
};
#[cfg(test)]
use canonical::CLAIM_EPOCH_EVENTS_SQL;
pub(in crate::storage) use canonical::{
    any_canonical_claim_epoch_event, canonical_work_events_for_item,
    canonical_work_mark_events_for_item, verified_work_identity,
};
use canonical::{
    decode_canonical_work_event, latest_canonical_work_event_on_feed,
    latest_canonical_work_event_on_snapshot, load_work_item_on_snapshot,
    native_work_event_optional,
};
pub(super) use canonical::{load_work_item_projection, load_work_items_query};
use execution_state::{
    active_root_execution_on_snapshot, load_retained_root_execution_on_snapshot,
    load_work_claim_on_snapshot, load_work_run_on_snapshot,
};
pub(super) use execution_state::{
    active_run_snapshot, verify_retained_root_execution_reference_on,
    verify_root_execution_reference_on,
};
pub(in crate::storage) use restored::latest_restored_record_hash;
pub(super) use restored::{
    latest_restored_record, restored_records_for_item, restored_records_with_id_for_item,
    work_completed_by_restored_record_on,
};
use restored::{restored_record_binds_work, restored_work_evidence_for_item};
mod references;
pub(in crate::storage) use activity::canonical_work_event_records_for_item;
pub(in crate::storage) use references::resolve_work_ref_on;
/// Test builds observe the executed statement, rather than estimating SQL work.
pub(super) fn query_row_with_cost<T, P, F>(
    connection: &rusqlite::Connection,
    label: &'static str,
    sql: &str,
    parameters: P,
    mapper: F,
) -> rusqlite::Result<T>
where
    P: rusqlite::Params,
    F: FnOnce(&rusqlite::Row<'_>) -> rusqlite::Result<T>,
{
    let mapper = |row: &rusqlite::Row<'_>| {
        super::search_budget::record(connection).map_err(|_| rusqlite::Error::InvalidQuery)?;
        mapper(row)
    };
    #[cfg(not(test))]
    {
        let _ = label;
        connection.query_row(sql, parameters, mapper)
    }
    #[cfg(test)]
    {
        let mut statement = connection.prepare(sql)?;
        let result = statement.query_row(parameters, mapper);
        super::cost::sql(label, &statement);
        result
    }
}
pub(crate) use catalog::WorkNoteSearchMatch;
use rusqlite::{Connection, OptionalExtension, params, types::Value};

use super::super::{SqliteStore, StaleRecoveryContext, StoreError};
use super::completion::{
    blocking_ancestor_on, feed_head, load_work_obligation_records_on,
    work_run_uses_active_root_execution,
};
use super::execution::{
    live_handoff_on, work_evidence_kind_on, work_run_evidence_on, work_run_evidence_projection_on,
};
use super::feeds::{checkpoint_feed_end, load_handoff_offer_projection, load_typed_work_object};
use super::integrity::{expected_environment_projection, expected_verification_projection};
use super::planning::{
    encode_state, normalize_work_catalog_key, require_work_item_relation_integrity,
};
use super::{CompletionRecoverySnapshot, WorkEvidenceProjectionSummary, WorkObligationRecord};
use crate::{
    CanonicalObject, ObjectId, RestoredRecord, RestoredWorkEvidence,
    domain::{
        CompletionSeal, EnvironmentEvidence, FeedId, FeedPosition, ReadyWork, RootExecution,
        RootExecutionId, SessionId, VerificationEvidence, WorkAvailability, WorkBlocker,
        WorkCatalogPage, WorkCatalogQuery, WorkCheckpoint, WorkClaim, WorkClaimState,
        WorkCompletionRecovery, WorkCompletionRecoveryCause, WorkEvent, WorkEvidenceKind,
        WorkFeedEntry, WorkHandoffOffer, WorkId, WorkItem, WorkLifecycle, WorkPrerequisiteState,
        WorkReadinessReason, WorkReferenceCandidate, WorkRun, WorkRunId, WorkRunState,
        WorkTransition,
    },
};

#[cfg(test)]
use super::WorkEventDraft;
#[cfg(test)]
use super::feeds::append_work_event;

#[cfg(test)]
mod cost_fixture;
#[cfg(test)]
mod tests;

mod ancestor;
mod stranded;
use ancestor::parent_execution_guidance;
pub(super) use ancestor::projected_blocking_ancestor;

mod catalog;
pub(crate) use catalog::{CHILD_REQUIREMENT_NEEDS_PARENT_REFUSAL, ListingExpectation};
pub(super) use catalog::{held_open_child_on, open_children_by_ready_order_on};

mod prerequisites;
pub(in crate::storage) use prerequisites::load_prerequisite_projection_ids;
pub(super) use prerequisites::{
    bounded_prerequisite_projection_rows, classified_prerequisite_projections,
    incomplete_prerequisite_projections,
};
use prerequisites::{incomplete_prerequisites, projected_prerequisite_state};

mod readiness;
pub(super) use readiness::current_required_child_waivers;

impl SqliteStore {
    pub(crate) fn work_completed_by_restored_record(
        &self,
        work_id: WorkId,
    ) -> Result<bool, StoreError> {
        // The item and the event it is checked against come from one commit.
        self.work_read_snapshot(|store| {
            let item = load_work_item(&store.connection, work_id)?;
            work_completed_by_restored_record_on(&store.connection, &item)
        })
    }

    /// Returns inert restored history generations in their dense order.
    pub(crate) fn work_restored_records(
        &self,
        work_id: WorkId,
    ) -> Result<Vec<RestoredRecord>, StoreError> {
        restored_records_for_item(&self.connection, work_id)
    }

    /// Returns late findings bound to restored completion history.
    pub(crate) fn restored_work_evidence(
        &self,
        work_id: WorkId,
    ) -> Result<Vec<(ObjectId, RestoredWorkEvidence)>, StoreError> {
        restored_work_evidence_for_item(&self.connection, work_id)
    }

    /// Returns every handoff offer for work in stable offer order.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when an offer projection cannot be decoded.
    pub fn work_handoff_offers(
        &self,
        work_id: WorkId,
    ) -> Result<Vec<WorkHandoffOffer>, StoreError> {
        // Each offer row is checked against its latest offer event, so the
        // rows and the events come from one commit.
        on_one_snapshot(&self.connection, |connection| {
            let mut statement = connection.prepare(
                "SELECT offer_object_id, offer_json FROM work_handoff_offers
                 WHERE work_id = ?1 ORDER BY offer_id",
            )?;
            let rows = statement
                .query_map([work_id.0.to_string()], |row| {
                    Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Vec<u8>>(1)?))
                })?
                .collect::<Result<Vec<_>, _>>()?;
            rows.into_iter()
                .map(|row| load_handoff_offer_projection(connection, row))
                .collect()
        })
    }

    /// Returns canonical evidence ides recorded for one run.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when a stored hash is invalid.
    pub fn work_run_evidence(&self, run_id: WorkRunId) -> Result<Vec<ObjectId>, StoreError> {
        work_run_evidence_on(&self.connection, run_id)
    }

    /// Returns a bounded selection basis for one focus page: the run's newest
    /// evidence records by run-feed position, plus the environments their
    /// verifications link to. Each record's canonical kind and environment
    /// binding are checked against the run projection before selection.
    pub(crate) fn work_run_evidence_projection(
        &self,
        run_id: WorkRunId,
        limit: usize,
    ) -> Result<Vec<WorkEvidenceProjectionSummary>, StoreError> {
        work_run_evidence_projection_on(&self.connection, run_id, limit)
    }

    pub(crate) fn work_run_evidence_count(&self, run_id: WorkRunId) -> Result<usize, StoreError> {
        let count = self.connection.query_row(
            "SELECT COUNT(*) FROM work_run_evidence WHERE run_id = ?1",
            [run_id.0.to_string()],
            |row| row.get::<_, i64>(0),
        )?;
        usize::try_from(count).map_err(|_| {
            StoreError::InvalidWorkProjection("run evidence count does not fit usize".into())
        })
    }

    /// Returns the last evidence object appended to one dense run feed.
    /// Evidence timestamps are asserted metadata and do not order delivery.
    pub(crate) fn latest_work_run_evidence(
        &self,
        run_id: WorkRunId,
    ) -> Result<Option<ObjectId>, StoreError> {
        let stored = self
            .connection
            .query_row(
                "SELECT object_id
                 FROM work_feed_entries
                 WHERE feed_kind = 'run_execution'
                   AND feed_id = ?1
                   AND object_kind IN (
                       'work_evidence',
                       'verification_evidence',
                       'environment_evidence'
                   )
                 ORDER BY position DESC
                 LIMIT 1",
                [run_id.0.to_string()],
                |row| row.get::<_, String>(0),
            )
            .optional()?;
        stored
            .map(|stored_hash| {
                ObjectId::from_stored(stored_hash).ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "latest run evidence has an invalid object id".into(),
                    )
                })
            })
            .transpose()
    }

    /// Returns obligation definitions and terminal resolutions for one exact
    /// run in trigger order, each decoded from its record and checked against
    /// the run's durable projection.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when canonical bytes, feed positions, redundant
    /// scalar bindings, or resolution authority do not agree.
    pub(crate) fn work_run_obligations(
        &self,
        run_id: WorkRunId,
    ) -> Result<Vec<WorkObligationRecord>, StoreError> {
        // The records are checked against the run feed's observations, so
        // both come from one commit.
        on_one_snapshot(&self.connection, |connection| {
            load_work_obligation_records_on(connection, run_id, None)
        })
    }

    /// Resolves and validates the typed category of one run evidence object.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the hash is absent, belongs to another run,
    /// or its canonical object disagrees with the redundant projection.
    pub fn work_evidence_kind(
        &self,
        run_id: WorkRunId,
        evidence_id: &ObjectId,
    ) -> Result<WorkEvidenceKind, StoreError> {
        work_evidence_kind_on(&self.connection, run_id, evidence_id)
    }

    pub(crate) fn load_verification_evidence(
        &self,
        evidence_id: &ObjectId,
    ) -> Result<VerificationEvidence, StoreError> {
        expected_verification_projection(&self.connection, evidence_id)?;
        load_typed_work_object(&self.connection, evidence_id, "verification_evidence")
    }

    pub(crate) fn load_environment_evidence(
        &self,
        evidence_id: &ObjectId,
    ) -> Result<EnvironmentEvidence, StoreError> {
        expected_environment_projection(&self.connection, evidence_id)?;
        load_typed_work_object(&self.connection, evidence_id, "environment_evidence")
    }

    /// Reads the current head for an exact work feed.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the head projection cannot be read.
    pub fn work_feed_head(&self, feed: &FeedId) -> Result<i64, StoreError> {
        feed_head(&self.connection, feed)
    }

    /// Returns a deterministic derived status with exact blocker reasons.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when graph or projection data cannot be verified.
    pub fn inspect_work(
        &self,
        work_id: WorkId,
        now: DateTime<Utc>,
    ) -> Result<ReadyWork, StoreError> {
        // The item, its relations and their event basis are compared, so they
        // come from one commit.
        on_one_snapshot(&self.connection, |connection| {
            inspect_work_on(connection, work_id, now)
        })
    }

    /// The blocker a clear removed from `work_id`, as it was recorded when it
    /// was raised, checked against the event that raised it. `None` when the
    /// item holds no cleared blocker of that id.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the retained row disagrees with its own
    /// columns or with the event that raised it, or SQLite fails.
    pub fn cleared_work_blocker(
        &self,
        work_id: WorkId,
        blocker_id: &str,
    ) -> Result<Option<WorkBlocker>, StoreError> {
        load_cleared_blocker_projection(&self.connection, work_id, blocker_id)
    }

    /// The blocker whose committed clear moved `work_id` to `revision`, as it
    /// was recorded when it was raised; `None` when that change cleared no
    /// blocker.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when a clear event or its retained row cannot
    /// be read or disagrees with its binding, or SQLite fails.
    pub fn blocker_cleared_at(
        &self,
        work_id: WorkId,
        revision: i64,
    ) -> Result<Option<WorkBlocker>, StoreError> {
        on_one_snapshot(&self.connection, |connection| {
            load_blocker_cleared_at(connection, work_id, revision)
        })
    }

    /// Advisory completion readiness from current local bindings for this
    /// session. Child seals and waivers are not replayed as historical proof
    /// while shaping guidance. The final call must supply criterion results
    /// and verify all canonical proofs and current authority atomically.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when canonical checkpoint/evidence projections
    /// are corrupt or SQLite cannot evaluate the graph.
    pub fn work_completion_readiness(
        &self,
        work_id: WorkId,
        session_id: &SessionId,
        now: DateTime<Utc>,
    ) -> Result<(bool, bool), StoreError> {
        // The item, claim, run, checkpoint and feed head are compared, so
        // they come from one commit.
        on_one_snapshot(&self.connection, |_| {
            let item = load_work_item(&self.connection, work_id)?;
            let claim = self.current_work_claim_for_item(&item)?;
            self.work_completion_readiness_for_item(&item, claim.as_ref(), session_id, now)
        })
    }

    pub(crate) fn work_completion_readiness_for_item(
        &self,
        item: &WorkItem,
        claim: Option<&WorkClaim>,
        session_id: &SessionId,
        now: DateTime<Utc>,
    ) -> Result<(bool, bool), StoreError> {
        self.work_completion_readiness_in_scope(item, claim, session_id, now, None)
    }

    pub(crate) fn work_completion_readiness_in_scope(
        &self,
        item: &WorkItem,
        claim: Option<&WorkClaim>,
        session_id: &SessionId,
        now: DateTime<Utc>,
        scope: Option<&super::RootReadScope<'_>>,
    ) -> Result<(bool, bool), StoreError> {
        if let Some(scope) = scope {
            scope.check_connection(&self.connection)?;
        }
        let work_id = item.work_id;
        if item.lifecycle != WorkLifecycle::Open {
            return Ok((false, false));
        }
        let Some(run_id) = item.active_run_id else {
            return Ok((false, false));
        };
        let run = load_work_run(&self.connection, run_id)?;
        let Some(claim) = claim else {
            return Ok((false, false));
        };
        if claim.work_id != item.work_id
            || claim.run_id != run_id
            || claim.state != WorkClaimState::Active
            || claim.holder != *session_id
            || claim.expires_at <= now
        {
            return Ok((false, false));
        }
        let live_handoff = live_handoff_on(&self.connection, run_id, now)?.is_some();
        let active_blocker = self.connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM work_blockers WHERE work_id = ?1 AND state = 'active'
             )",
            [work_id.0.to_string()],
            |row| row.get::<_, bool>(0),
        )?;
        if live_handoff
            || active_blocker
            || !incomplete_prerequisites(&self.connection, work_id)?.is_empty()
        {
            return Ok((false, false));
        }
        let required_child_count = query_row_with_cost(
            &self.connection,
            "required_child_count",
            "SELECT COUNT(*) FROM work_items child
             WHERE child.parent_id = ?1
               AND child.child_requirement = 'required'",
            [work_id.0.to_string()],
            |row| row.get::<_, i64>(0),
        )?;
        if required_child_count != 0 {
            let root_execution = match scope {
                Some(scope) => scope.current(run.root_execution_id)?,
                None => std::rc::Rc::new(load_root_execution(
                    &self.connection,
                    run.root_execution_id,
                )?),
            };
            if !readiness::required_children_ready(&self.connection, work_id, &root_execution)? {
                return Ok((false, false));
            }
        }
        let evidence = self.work_run_evidence(run_id)?;
        let Some(checkpoint_hash) = run.last_checkpoint else {
            return Ok((true, false));
        };
        if evidence.is_empty() {
            return Ok((true, false));
        }
        let checkpoint: WorkCheckpoint =
            load_typed_work_object(&self.connection, &checkpoint_hash, "work_checkpoint")?;
        let current_cut = feed_head(&self.connection, &FeedId::RunExecution(run_id))?;
        let checkpoint_cut = checkpoint_feed_end(checkpoint.acknowledged_run_position.position)?;
        Ok((
            true,
            checkpoint.work_id == work_id
                && checkpoint.run_id == run_id
                && checkpoint.claim_id == claim.claim_id
                && checkpoint.claim_fence == claim.fence
                && evidence
                    .iter()
                    .all(|hash| checkpoint.evidence.contains(hash))
                && checkpoint.acknowledged_run_position.feed == FeedId::RunExecution(run_id)
                && checkpoint_cut == current_cut,
        ))
    }

    /// Reads immutable entries after a dense position in one exact feed.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the feed contains invalid hashes or cannot be read.
    pub fn work_feed_after(
        &self,
        feed: &FeedId,
        after: i64,
        limit: u32,
    ) -> Result<Vec<WorkFeedEntry>, StoreError> {
        let (feed_kind, feed_id) = feed_parts(feed);
        let mut statement = self.connection.prepare(
            "SELECT position, object_kind, object_id
             FROM work_feed_entries
             WHERE feed_kind = ?1 AND feed_id = ?2 AND position > ?3
             ORDER BY position LIMIT ?4",
        )?;
        let rows = statement.query_map(
            params![
                feed_kind,
                feed_id,
                after.max(0),
                i64::from(limit.clamp(1, 1_000))
            ],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            },
        )?;
        rows.map(|row| {
            let (position, object_kind, hash) = row?;
            let object_id =
                ObjectId::from_stored(hash.clone()).ok_or(StoreError::InvalidStoredKey(hash))?;
            Ok(WorkFeedEntry {
                position: FeedPosition {
                    feed: feed.clone(),
                    position,
                },
                object_kind,
                object_id,
            })
        })
        .collect()
    }

    /// Replays one exact staged feed interval, inclusive of its upper bound.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the interval or stored hashes are invalid.
    pub fn work_feed_between(
        &self,
        feed: &FeedId,
        after: i64,
        through: i64,
    ) -> Result<Vec<WorkFeedEntry>, StoreError> {
        let distance = through.checked_sub(after).ok_or_else(|| {
            StoreError::InvalidWorkProjection(format!(
                "staged feed interval ({after}, {through}] overflowed"
            ))
        })?;
        if !(0..=1_000).contains(&distance) {
            return Err(StoreError::InvalidWorkProjection(format!(
                "invalid staged feed interval ({after}, {through}]"
            )));
        }
        let (feed_kind, feed_id) = feed_parts(feed);
        let mut statement = self.connection.prepare(
            "SELECT position, object_kind, object_id
             FROM work_feed_entries
             WHERE feed_kind = ?1 AND feed_id = ?2
               AND position > ?3 AND position <= ?4
             ORDER BY position",
        )?;
        let rows = statement
            .query_map(params![feed_kind, feed_id, after, through], |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                ))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        let expected_count = usize::try_from(distance).map_err(|_| {
            StoreError::InvalidWorkProjection("staged feed interval size overflowed".into())
        })?;
        if rows.len() != expected_count {
            return Err(StoreError::InvalidWorkProjection(format!(
                "staged feed interval ({after}, {through}] is not dense"
            )));
        }
        for (offset, row) in rows.iter().enumerate() {
            let offset = i64::try_from(offset).map_err(|_| {
                StoreError::InvalidWorkProjection(
                    "staged feed dense-position offset overflowed".into(),
                )
            })?;
            let expected_position = after
                .checked_add(offset)
                .and_then(|position| position.checked_add(1))
                .ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "staged feed dense-position arithmetic overflowed".into(),
                    )
                })?;
            if row.0 != expected_position {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "staged feed interval ({after}, {through}] is not dense"
                )));
            }
        }
        rows.into_iter()
            .map(|(position, object_kind, hash)| {
                let object_id = ObjectId::from_stored(hash.clone())
                    .ok_or(StoreError::InvalidStoredKey(hash))?;
                Ok(WorkFeedEntry {
                    position: FeedPosition {
                        feed: feed.clone(),
                        position,
                    },
                    object_kind,
                    object_id,
                })
            })
            .collect()
    }

    /// Reads the newest immutable entries in one exact feed, returned oldest
    /// to newest within the bounded tail.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the feed projection or an object id is
    /// invalid.
    pub fn work_feed_tail(
        &self,
        feed: &FeedId,
        limit: u32,
    ) -> Result<Vec<WorkFeedEntry>, StoreError> {
        let (feed_kind, feed_id) = feed_parts(feed);
        let mut statement = self.connection.prepare(
            "SELECT position, object_kind, object_id FROM (
                 SELECT position, object_kind, object_id
                 FROM work_feed_entries
                 WHERE feed_kind = ?1 AND feed_id = ?2
                 ORDER BY position DESC LIMIT ?3
             ) ORDER BY position",
        )?;
        statement
            .query_map(
                params![feed_kind, feed_id, i64::from(limit.clamp(1, 10_000))],
                |row| {
                    Ok((
                        row.get::<_, i64>(0)?,
                        row.get::<_, String>(1)?,
                        row.get::<_, String>(2)?,
                    ))
                },
            )?
            .map(|row| {
                let (position, object_kind, hash) = row?;
                Ok(WorkFeedEntry {
                    position: FeedPosition {
                        feed: feed.clone(),
                        position,
                    },
                    object_kind,
                    object_id: ObjectId::from_stored(hash.clone())
                        .ok_or(StoreError::InvalidStoredKey(hash))?,
                })
            })
            .collect()
    }

    /// Returns the newest work events for one exact item without applying a
    /// root-wide pre-limit that could hide older item history.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when item/feed identities or hashes are invalid.
    pub fn work_event_tail(
        &self,
        work_id: WorkId,
        limit: u32,
    ) -> Result<Vec<WorkFeedEntry>, StoreError> {
        let item = self.get_work_item(work_id)?;
        let mut statement = self.connection.prepare(
            "SELECT position, object_id FROM (
                 SELECT entry.position, entry.object_id
                 FROM work_feed_entries entry
                 JOIN objects object ON object.object_id = entry.object_id
                 WHERE entry.feed_kind = 'root_work'
                   AND entry.feed_id = ?1
                   AND entry.object_kind = 'work_event'
                   AND entry.work_id = ?2
                 ORDER BY entry.position DESC LIMIT ?3
             ) ORDER BY position",
        )?;
        statement
            .query_map(
                params![
                    item.root_id.0.to_string(),
                    work_id.0.to_string(),
                    i64::from(limit.clamp(1, 1_000))
                ],
                |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
            )?
            .map(|row| {
                let (position, hash) = row?;
                let object_id = ObjectId::from_stored(hash.clone())
                    .ok_or(StoreError::InvalidStoredKey(hash))?;
                Ok(WorkFeedEntry {
                    position: FeedPosition {
                        feed: FeedId::RootWork(item.root_id),
                        position,
                    },
                    object_kind: "work_event".into(),
                    object_id,
                })
            })
            .collect()
    }

    /// Reads the adjacent canonical planning snapshot, even outside a displayed
    /// history page. Restored work starts from its immutable restore record.
    pub(crate) fn work_planning_before(
        &self,
        position: &FeedPosition,
        event: &WorkEvent,
    ) -> Result<serde_json::Value, StoreError> {
        let (kind, feed_id) = feed_parts(&position.feed);
        if position.feed != FeedId::Project(event.project_id.clone())
            && position.feed != FeedId::RootWork(event.root_id)
        {
            return Err(StoreError::InvalidWorkProjection(
                "revision history feed does not bind the work item".into(),
            ));
        }
        let row = self
            .connection
            .query_row(
                "SELECT object.object_id, object.canonical_json
             FROM work_feed_entries entry
             JOIN objects object ON object.object_id = entry.object_id
             WHERE entry.feed_kind = ?1 AND entry.feed_id = ?2
               AND entry.work_id = ?3 AND entry.object_kind = 'work_event'
               AND object.object_kind = 'work_event' AND entry.position < ?4
             ORDER BY entry.position DESC LIMIT 1",
                params![
                    kind,
                    feed_id,
                    event.work_id.0.to_string(),
                    position.position
                ],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .optional()?;
        if let Some(row) = row {
            let previous = decode_canonical_work_event(row)?;
            if previous.project_id != event.project_id
                || previous.root_id != event.root_id
                || previous.work_id != event.work_id
                || previous.revision >= event.revision
            {
                return Err(StoreError::InvalidWorkProjection(
                    "adjacent revision snapshot has invalid identity or order".into(),
                ));
            }
            return Ok(serde_json::to_value(previous.work)?);
        }
        if event.work.restored
            && let Some((_, record)) = latest_restored_record(&self.connection, event.work_id)?
            && record.project_id == event.project_id
            && record.item.root_id == event.root_id
        {
            return Ok(serde_json::to_value(record.item)?);
        }
        Err(StoreError::InvalidWorkProjection(
            "revision history has no preceding canonical planning snapshot".into(),
        ))
    }

    /// Counts canonical lifecycle events for one exact work item.
    pub(crate) fn work_event_count(&self, work_id: WorkId) -> Result<usize, StoreError> {
        let item = self.get_work_item(work_id)?;
        let count: i64 = self.connection.query_row(
            "SELECT COUNT(*)
             FROM work_feed_entries entry
             JOIN objects object ON object.object_id = entry.object_id
             WHERE entry.feed_kind = 'root_work'
               AND entry.feed_id = ?1
               AND entry.object_kind = 'work_event'
               AND entry.work_id = ?2",
            params![item.root_id.0.to_string(), work_id.0.to_string()],
            |row| row.get(0),
        )?;
        usize::try_from(count).map_err(|_| {
            StoreError::InvalidWorkProjection("work event count overflowed usize".into())
        })
    }

    /// The placeholder `"<title> is done"` an item was created with as its
    /// only criterion, with its creation title, while none of its events
    /// carries another acceptance list. Events are read without decoding, and
    /// the history is scanned only for an item created with the placeholder.
    /// `None` otherwise, and when the first event is not a creation, as for
    /// restored history.
    pub(crate) fn work_creation_placeholder(
        &self,
        work_id: WorkId,
    ) -> Result<Option<String>, StoreError> {
        on_one_snapshot(&self.connection, |connection| {
            let first = connection
                .query_row(
                    "SELECT json_extract(object.canonical_json, '$.transition.kind'),
                            json_extract(object.canonical_json, '$.work.title'),
                            json_extract(object.canonical_json, '$.work.acceptance')
                     FROM work_feed_entries entry
                     JOIN objects object ON object.object_id = entry.object_id
                     WHERE entry.feed_kind = 'project' AND entry.object_kind = 'work_event'
                       AND entry.work_id = ?1
                     ORDER BY entry.position LIMIT 1",
                    [work_id.0.to_string()],
                    |row| {
                        Ok((
                            row.get::<_, String>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )
                .optional()?;
            let Some((_, title, acceptance)) = first.filter(|(kind, ..)| kind == "created") else {
                return Ok(None);
            };
            let placeholder = format!("{title} is done");
            let created_with: Vec<String> = serde_json::from_str(&acceptance)?;
            if created_with != [placeholder.as_str()] {
                return Ok(None);
            }
            let revised = connection.query_row(
                "SELECT EXISTS(
                     SELECT 1 FROM work_feed_entries entry
                     JOIN objects object ON object.object_id = entry.object_id
                     WHERE entry.feed_kind = 'project' AND entry.object_kind = 'work_event'
                       AND entry.work_id = ?1
                       AND json_extract(object.canonical_json, '$.work.acceptance') IS NOT ?2
                 )",
                params![work_id.0.to_string(), acceptance],
                |row| row.get::<_, bool>(0),
            )?;
            Ok((!revised).then_some(placeholder))
        })
    }

    #[cfg(test)]
    pub(crate) fn append_test_work_event(&mut self, event: &WorkEvent) -> Result<(), StoreError> {
        let transaction = self.begin_work_mutation()?;
        let root = event
            .root_execution
            .as_ref()
            .map(|address| super::root_state::resolve(&transaction, address))
            .transpose()?;
        append_work_event(&transaction, &WorkEventDraft::with_root_state(event, root))?;
        transaction.commit()?;
        Ok(())
    }

    #[cfg(test)]
    pub(crate) fn append_test_work_events(
        &mut self,
        events: &[WorkEvent],
    ) -> Result<(), StoreError> {
        let transaction = self.begin_work_mutation()?;
        for event in events {
            let root = event
                .root_execution
                .as_ref()
                .map(|address| super::root_state::resolve(&transaction, address))
                .transpose()?;
            append_work_event(&transaction, &WorkEventDraft::with_root_state(event, root))?;
        }
        transaction.commit()?;
        Ok(())
    }
}

fn command_work_ref_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
    item: &WorkReferenceCandidate,
) -> Result<String, StoreError> {
    match resolve_work_ref_on(connection, project_id, &item.short_ref) {
        Ok(resolved) if resolved.work_id == item.work_id => Ok(item.short_ref.clone()),
        Ok(resolved) => Err(StoreError::InvalidWorkProjection(format!(
            "short reference {:?} resolved to work {:?} instead of {:?}",
            item.short_ref, resolved.work_id.0, item.work_id.0
        ))),
        Err(StoreError::WorkReferenceAmbiguous { .. }) => Ok(item.work_id.0.to_string()),
        Err(error) => Err(error),
    }
}

pub(super) fn completion_recovery_on(
    connection: &Connection,
    work: &WorkItem,
    cause: WorkCompletionRecoveryCause,
) -> Result<WorkCompletionRecovery, StoreError> {
    let affected_id = match &cause {
        WorkCompletionRecoveryCause::RequiredChildUnsealed { child } => *child,
        _ => work.work_id,
    };
    let affected = if affected_id == work.work_id {
        work.clone()
    } else {
        load_work_item(connection, affected_id)?
    };
    let item = WorkReferenceCandidate {
        work_id: affected.work_id,
        short_ref: affected.short_ref.clone(),
        title: affected.title,
        lifecycle: affected.lifecycle,
    };
    let command_ref = command_work_ref_on(connection, &affected.project_id, &item)?;
    let command = match &cause {
        WorkCompletionRecoveryCause::OpenObligation { obligation_id, .. } => format!(
            "engram work done {command_ref} --note \"retry after host verification for obligation {}\"",
            obligation_id.0
        ),
        WorkCompletionRecoveryCause::RequiredChildUnsealed { .. }
            if item.lifecycle == WorkLifecycle::Open =>
        {
            format!("engram work show {command_ref}")
        }
        WorkCompletionRecoveryCause::RequiredChildUnsealed { .. } => {
            let parent = WorkReferenceCandidate {
                work_id: work.work_id,
                short_ref: work.short_ref.clone(),
                title: work.title.clone(),
                lifecycle: work.lifecycle,
            };
            let parent_ref = command_work_ref_on(connection, &work.project_id, &parent)?;
            format!(
                "engram work update {parent_ref} --waive {command_ref} --reason \"account for disposed required child\""
            )
        }
        WorkCompletionRecoveryCause::MissingContribution { participant } => {
            let participant = recovery_command_atom(&participant.0)?;
            format!(
                "engram work handoff {command_ref} --to {participant} --summary \"transfer root so the missing participant can contribute\""
            )
        }
        WorkCompletionRecoveryCause::MissingAcceptance { .. } => {
            format!("engram work done {command_ref} --note \"acceptance verified\"")
        }
        WorkCompletionRecoveryCause::MissingAcceptanceEvaluation { .. }
        | WorkCompletionRecoveryCause::AcceptanceEvaluationStale { .. } => {
            // Runnable navigation to the notes and gate locators an evaluator
            // reads before recording (the criteria sit in the same receipt;
            // `--full` is a separate, exclusive mode); the evaluation itself
            // is judgment, so no template pre-fills a verdict.
            format!("engram work show {command_ref} --notes --gates")
        }
        WorkCompletionRecoveryCause::AcceptanceFailed { .. }
        | WorkCompletionRecoveryCause::AcceptanceInsufficientEvidence { .. } => {
            format!("engram work show {command_ref} --notes --gates")
        }
        WorkCompletionRecoveryCause::AcceptanceNeedsHuman { .. } => {
            format!("engram work show {command_ref} --full")
        }
    };
    Ok(WorkCompletionRecovery {
        cause,
        item,
        command,
        deciding_observation: None,
        source: None,
        open_obligation_check: None,
    })
}

pub(super) fn completion_recovery_snapshot_on(
    connection: &Connection,
    work: &WorkItem,
    run_id: WorkRunId,
    cause: WorkCompletionRecoveryCause,
    context: StaleRecoveryContext,
) -> Result<CompletionRecoverySnapshot, StoreError> {
    let required_child_successor = match &cause {
        WorkCompletionRecoveryCause::RequiredChildUnsealed { child } => {
            let child = load_work_item(connection, *child)?;
            let run = load_work_run(connection, run_id)?;
            super::child_resolution::required_child_successor_on(
                connection,
                &child,
                Some(run.root_execution_id),
            )?
        }
        WorkCompletionRecoveryCause::OpenObligation { .. }
        | WorkCompletionRecoveryCause::MissingContribution { .. }
        | WorkCompletionRecoveryCause::MissingAcceptance { .. }
        | WorkCompletionRecoveryCause::MissingAcceptanceEvaluation { .. }
        | WorkCompletionRecoveryCause::AcceptanceEvaluationStale { .. }
        | WorkCompletionRecoveryCause::AcceptanceFailed { .. }
        | WorkCompletionRecoveryCause::AcceptanceInsufficientEvidence { .. }
        | WorkCompletionRecoveryCause::AcceptanceNeedsHuman { .. } => None,
    };
    let mut recovery = completion_recovery_on(connection, work, cause)?;
    recovery.deciding_observation = context.deciding_observation;
    recovery.source = context.source;
    Ok(CompletionRecoverySnapshot {
        recovery,
        obligations: load_work_obligation_records_on(connection, run_id, None)?,
        required_child_successor: required_child_successor.map(Box::new),
    })
}

fn recovery_command_atom(value: &str) -> Result<&str, StoreError> {
    if !value.is_empty()
        && value.bytes().all(|byte| {
            byte.is_ascii_alphanumeric() || matches!(byte, b'-' | b'_' | b'.' | b':' | b'@')
        })
    {
        return Ok(value);
    }
    Err(StoreError::InvalidWorkProjection(
        "completion recovery target contains characters that cannot be rendered as a shell-safe CLI argument"
            .into(),
    ))
}

fn ambiguous_work_reference_error(
    reference: &str,
    mut items: Vec<WorkItem>,
    more: usize,
) -> StoreError {
    items.sort_by(|left, right| left.work_id.0.as_bytes().cmp(right.work_id.0.as_bytes()));
    StoreError::WorkReferenceAmbiguous {
        reference: reference.to_owned(),
        candidates: items
            .into_iter()
            .map(|item| WorkReferenceCandidate {
                work_id: item.work_id,
                short_ref: item.short_ref,
                title: item.title,
                lifecycle: item.lifecycle,
            })
            .collect(),
        more,
    }
}

pub(super) fn inspect_work_on(
    connection: &Connection,
    work_id: WorkId,
    now: DateTime<Utc>,
) -> Result<ReadyWork, StoreError> {
    let work = load_work_item(connection, work_id)?;
    require_work_item_relation_integrity(connection, work_id)?;
    let blockers = load_active_blocker_projections(connection, work_id)?;
    let prerequisites = classified_prerequisite_projections(connection, work_id)?;
    derive_projected_work_availability(connection, work, blockers, prerequisites, now)
}

fn derive_projected_work_availability(
    connection: &Connection,
    work: WorkItem,
    blockers: Vec<WorkBlocker>,
    prerequisites: Vec<(WorkId, WorkPrerequisiteState)>,
    now: DateTime<Utc>,
) -> Result<ReadyWork, StoreError> {
    let (blocked_by, has_dead_prerequisite) = prerequisite_readiness(prerequisites);
    let mut why = Vec::new();
    let mut reason_codes = Vec::new();
    let blocking_ancestor = if work.lifecycle == WorkLifecycle::Open {
        projected_blocking_ancestor(connection, &work)?
    } else {
        None
    };
    let availability = if !matches!(work.lifecycle, WorkLifecycle::Open) {
        reason_codes.push(WorkReadinessReason::LifecycleClosed);
        why.push(format!("lifecycle is {:?}", work.lifecycle));
        WorkAvailability::Closed
    } else if readiness::import_retired_on(connection, &work)? {
        reason_codes.push(WorkReadinessReason::ImportRetired);
        why.push(
            "retired by import without successful completion; explicitly reopen a fresh generation"
                .into(),
        );
        WorkAvailability::Waiting
    } else if blocking_ancestor.is_some()
        || !projected_run_uses_active_root_execution(connection, &work)?
    {
        reason_codes.push(WorkReadinessReason::ParentDisallowsExecution);
        parent_execution_guidance(
            connection,
            &work,
            blocking_ancestor.as_ref(),
            now,
            blockers.is_empty() && blocked_by.is_empty(),
            &mut reason_codes,
            &mut why,
        )?;
        WorkAvailability::Blocked
    } else if work.deferred_until.is_some_and(|until| until > now) {
        reason_codes.push(WorkReadinessReason::DeferredUntil);
        why.push("deferred wake time has not arrived".into());
        WorkAvailability::Deferred
    } else if !blockers.is_empty() || !blocked_by.is_empty() {
        if !blocked_by.is_empty() {
            reason_codes.push(WorkReadinessReason::PrerequisiteIncomplete);
            why.push(if has_dead_prerequisite {
                "one or more prerequisites are dead and must be removed".into()
            } else {
                "one or more prerequisites are incomplete".into()
            });
        }
        if !blockers.is_empty() {
            reason_codes.push(WorkReadinessReason::TypedBlockerActive);
            why.push("one or more typed blockers remain active".into());
        }
        WorkAvailability::Blocked
    } else {
        projected_claim_availability(connection, &work, now, &mut reason_codes, &mut why)?
    };
    if availability == WorkAvailability::Ready {
        reason_codes.push(WorkReadinessReason::ReadyUnclaimed);
        why.push(crate::PLAIN_READY_REASON.into());
    }
    Ok(ReadyWork {
        work,
        availability,
        blocking_parent: blocking_ancestor
            .as_ref()
            .map(|ancestor| ancestor.lifecycle),
        blocking_ancestor,
        reason_codes,
        why,
        blocked_by,
        blockers,
    })
}

pub(super) fn inspect_work_canonical_on(
    connection: &Connection,
    work_id: WorkId,
    now: DateTime<Utc>,
) -> Result<ReadyWork, StoreError> {
    let work = load_work_item(connection, work_id)?;
    require_work_item_relation_integrity(connection, work_id)?;
    let blockers = load_active_blocker_projections(connection, work_id)?;
    let prerequisites = classified_prerequisite_projections(connection, work_id)?;
    derive_work_availability(connection, work, blockers, prerequisites, now)
}

fn derive_work_availability(
    connection: &Connection,
    work: WorkItem,
    blockers: Vec<WorkBlocker>,
    prerequisites: Vec<(WorkId, WorkPrerequisiteState)>,
    now: DateTime<Utc>,
) -> Result<ReadyWork, StoreError> {
    let (blocked_by, has_dead_prerequisite) = prerequisite_readiness(prerequisites);
    let mut why = Vec::new();
    let mut reason_codes = Vec::new();
    let blocking_ancestor = if work.lifecycle == WorkLifecycle::Open {
        blocking_ancestor_on(connection, &work)?
    } else {
        None
    };
    let availability = if !matches!(work.lifecycle, WorkLifecycle::Open) {
        reason_codes.push(WorkReadinessReason::LifecycleClosed);
        why.push(format!("lifecycle is {:?}", work.lifecycle));
        WorkAvailability::Closed
    } else if readiness::import_retired_on(connection, &work)? {
        reason_codes.push(WorkReadinessReason::ImportRetired);
        why.push(
            "retired by import without successful completion; explicitly reopen a fresh generation"
                .into(),
        );
        WorkAvailability::Waiting
    } else if blocking_ancestor.is_some()
        || !((work.restored && work.active_run_id.is_none())
            || work_run_uses_active_root_execution(connection, &work)?)
    {
        reason_codes.push(WorkReadinessReason::ParentDisallowsExecution);
        parent_execution_guidance(
            connection,
            &work,
            blocking_ancestor.as_ref(),
            now,
            blockers.is_empty() && blocked_by.is_empty(),
            &mut reason_codes,
            &mut why,
        )?;
        WorkAvailability::Blocked
    } else if work.deferred_until.is_some_and(|until| until > now) {
        reason_codes.push(WorkReadinessReason::DeferredUntil);
        why.push("deferred wake time has not arrived".into());
        WorkAvailability::Deferred
    } else if !blockers.is_empty() || !blocked_by.is_empty() {
        if !blocked_by.is_empty() {
            reason_codes.push(WorkReadinessReason::PrerequisiteIncomplete);
            why.push(if has_dead_prerequisite {
                "one or more prerequisites are dead and must be removed".into()
            } else {
                "one or more prerequisites are incomplete".into()
            });
        }
        if !blockers.is_empty() {
            reason_codes.push(WorkReadinessReason::TypedBlockerActive);
            why.push("one or more typed blockers remain active".into());
        }
        WorkAvailability::Blocked
    } else {
        claim_availability(connection, &work, now, &mut reason_codes, &mut why)?
    };
    if availability == WorkAvailability::Ready {
        reason_codes.push(WorkReadinessReason::ReadyUnclaimed);
        why.push(crate::PLAIN_READY_REASON.into());
    }
    Ok(ReadyWork {
        work,
        availability,
        blocking_parent: blocking_ancestor
            .as_ref()
            .map(|ancestor| ancestor.lifecycle),
        blocking_ancestor,
        reason_codes,
        why,
        blocked_by,
        blockers,
    })
}

fn prerequisite_readiness(
    prerequisites: Vec<(WorkId, WorkPrerequisiteState)>,
) -> (Vec<WorkId>, bool) {
    let has_dead = prerequisites
        .iter()
        .any(|(_, state)| *state == WorkPrerequisiteState::Dead);
    let blocked_by = prerequisites
        .into_iter()
        .filter_map(|(work_id, state)| {
            (state != WorkPrerequisiteState::Satisfied).then_some(work_id)
        })
        .collect();
    (blocked_by, has_dead)
}

fn claim_availability(
    connection: &Connection,
    work: &WorkItem,
    now: DateTime<Utc>,
    reason_codes: &mut Vec<WorkReadinessReason>,
    why: &mut Vec<String>,
) -> Result<WorkAvailability, StoreError> {
    let Some(run_id) = work.active_run_id else {
        if work.restored {
            return Ok(WorkAvailability::Ready);
        }
        return Err(StoreError::InvalidWorkProjection(format!(
            "open work {:?} has no active run",
            work.work_id
        )));
    };
    let run = load_work_run(connection, run_id)?;
    let Some(claim) = load_work_claim_optional(connection, run_id)? else {
        return Ok(WorkAvailability::Ready);
    };
    if claim.state != WorkClaimState::Active || claim.expires_at <= now {
        reason_codes.push(WorkReadinessReason::PriorClaimRecoverable);
        why.push("prior claim is recoverable".into());
        return Ok(WorkAvailability::Ready);
    }
    if run.last_checkpoint.is_some() {
        reason_codes.push(WorkReadinessReason::LiveClaimWithCheckpoint);
        why.push("live claim has checkpointed progress".into());
        Ok(WorkAvailability::Active)
    } else {
        reason_codes.push(WorkReadinessReason::LiveClaimWithoutCheckpoint);
        why.push("live claim has not checkpointed progress".into());
        Ok(WorkAvailability::Claimed)
    }
}

fn projected_run_uses_active_root_execution(
    connection: &Connection,
    item: &WorkItem,
) -> Result<bool, StoreError> {
    if item.restored && item.active_run_id.is_none() {
        return Ok(true);
    }
    let run_id = item.active_run_id.ok_or_else(|| {
        StoreError::InvalidWorkProjection(format!("open work {:?} has no active run", item.work_id))
    })?;
    connection
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM work_runs run
                 JOIN work_root_executions execution
                   ON execution.root_execution_id = run.root_execution_id
                 WHERE run.run_id = ?1 AND run.work_id = ?2
                   AND execution.project_id = ?3 AND execution.root_id = ?4
                   AND execution.state = 'active'
             )",
            params![
                run_id.0.to_string(),
                item.work_id.0.to_string(),
                item.project_id.0,
                item.root_id.0.to_string()
            ],
            |row| row.get(0),
        )
        .map_err(StoreError::from)
}

fn projected_claim_availability(
    connection: &Connection,
    work: &WorkItem,
    now: DateTime<Utc>,
    reason_codes: &mut Vec<WorkReadinessReason>,
    why: &mut Vec<String>,
) -> Result<WorkAvailability, StoreError> {
    if work.restored && work.active_run_id.is_none() {
        return Ok(WorkAvailability::Ready);
    }
    let run_id = work.active_run_id.ok_or_else(|| {
        StoreError::InvalidWorkProjection(format!("open work {:?} has no active run", work.work_id))
    })?;
    let row: Option<(Option<String>, Option<String>, Option<i64>)> = connection
        .query_row(
            "SELECT run.last_checkpoint_id, claim.state, claim.expires_at_ms
             FROM work_runs run
             LEFT JOIN work_claims claim ON claim.run_id = run.run_id
             WHERE run.run_id = ?1 AND run.work_id = ?2",
            params![run_id.0.to_string(), work.work_id.0.to_string()],
            |row| {
                super::search_budget::record_row(connection, row)?;
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            },
        )
        .optional()?;
    let (checkpoint, claim_state, expires_at) =
        row.ok_or_else(|| StoreError::InvalidWorkProjection(format!("run {run_id:?} is missing")))?;
    if claim_state.as_deref() != Some("active")
        || expires_at.is_none_or(|expires_at| expires_at <= now.timestamp_millis())
    {
        if claim_state.is_some() {
            reason_codes.push(WorkReadinessReason::PriorClaimRecoverable);
            why.push("prior claim is recoverable".into());
        }
        return Ok(WorkAvailability::Ready);
    }
    if checkpoint.is_some() {
        reason_codes.push(WorkReadinessReason::LiveClaimWithCheckpoint);
        why.push("live claim has checkpointed progress".into());
        Ok(WorkAvailability::Active)
    } else {
        reason_codes.push(WorkReadinessReason::LiveClaimWithoutCheckpoint);
        why.push("live claim has not checkpointed progress".into());
        Ok(WorkAvailability::Claimed)
    }
}

#[cfg(test)]
pub(super) fn latest_canonical_work_event_for_item(
    connection: &Connection,
    work_id: WorkId,
) -> Result<WorkEvent, StoreError> {
    latest_canonical_work_event_for_item_optional(connection, work_id)?.ok_or_else(|| {
        StoreError::InvalidWorkProjection(format!("work item {work_id:?} has no canonical event"))
    })
}

/// Runs `read` on one commit state of `connection`: inside the caller's
/// transaction when there is one, otherwise in a short deferred read
/// transaction. In autocommit each statement sees the newest commit, so a
/// check that compares several statements could otherwise compare two
/// moments and report another connection's commit as corruption.
pub(in crate::storage) fn on_one_snapshot<T>(
    connection: &Connection,
    read: impl FnOnce(&Connection) -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    if !connection.is_autocommit() {
        return super::task_sync::with_origin_scope(connection, || read(connection));
    }
    let transaction = connection.unchecked_transaction()?;
    let value = super::task_sync::with_origin_scope(&transaction, || read(&transaction))?;
    transaction.commit()?;
    Ok(value)
}

pub(super) fn latest_canonical_work_event_for_item_optional(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Option<WorkEvent>, StoreError> {
    on_one_snapshot(connection, |connection| {
        latest_canonical_work_event_on_snapshot(connection, work_id)
    })
}

/// The work item, checked against its latest canonical event or, for a
/// restored item, its restored record. Every read is on one commit state.
pub(in crate::storage) fn load_work_item(
    connection: &Connection,
    work_id: WorkId,
) -> Result<WorkItem, StoreError> {
    on_one_snapshot(connection, |connection| {
        load_work_item_on_snapshot(connection, work_id)
    })
}

/// The run row and its latest run-feed event are compared, so they are read
/// from one commit.
pub(in crate::storage) fn load_work_run(
    connection: &Connection,
    run_id: WorkRunId,
) -> Result<WorkRun, StoreError> {
    on_one_snapshot(connection, |connection| {
        load_work_run_on_snapshot(connection, run_id)
    })
}

pub(super) fn load_root_execution(
    connection: &Connection,
    root_execution_id: RootExecutionId,
) -> Result<RootExecution, StoreError> {
    load_root_execution_with_ref(connection, root_execution_id).map(|(execution, _)| execution)
}

/// The root state and its latest root-feed event are compared, so they are
/// read from one commit.
pub(super) fn load_root_execution_with_ref(
    connection: &Connection,
    root_execution_id: RootExecutionId,
) -> Result<(RootExecution, crate::domain::RootExecutionRef), StoreError> {
    on_one_snapshot(connection, |connection| {
        let (execution, address) = super::root_state::projected(connection, root_execution_id)?;
        verify_root_execution_reference_on(connection, &address)?;
        Ok((execution, address))
    })
}

/// Advisory inspection of a retained generation, not selection of live authority.
/// Its latest snapshot must bind the exact execution id and generation; a newer
/// root generation must neither replace its waivers nor invalidate its history.
pub(super) fn load_retained_root_execution(
    connection: &Connection,
    root_execution_id: RootExecutionId,
) -> Result<RootExecution, StoreError> {
    on_one_snapshot(connection, |connection| {
        load_retained_root_execution_on_snapshot(connection, root_execution_id)
    })
}

pub(super) fn active_root_execution(
    connection: &Connection,
    root_id: WorkId,
) -> Result<RootExecution, StoreError> {
    active_root_execution_optional(connection, root_id)?.ok_or_else(|| {
        StoreError::InvalidWorkProjection(format!("root work {root_id:?} has no active execution"))
    })
}

/// The active-execution row, the root state and the latest root-feed event
/// are compared, so they are read from one commit.
pub(super) fn active_root_execution_optional(
    connection: &Connection,
    root_id: WorkId,
) -> Result<Option<RootExecution>, StoreError> {
    on_one_snapshot(connection, |connection| {
        active_root_execution_on_snapshot(connection, root_id)
    })
}

/// The claim row and its latest run-feed event are compared, so they are
/// read from one commit.
pub(in crate::storage) fn load_work_claim_optional(
    connection: &Connection,
    run_id: WorkRunId,
) -> Result<Option<WorkClaim>, StoreError> {
    on_one_snapshot(connection, |connection| {
        load_work_claim_on_snapshot(connection, run_id)
    })
}

pub(super) fn parse_work_id(value: &str) -> Result<WorkId, StoreError> {
    uuid::Uuid::parse_str(value).map(WorkId).map_err(|error| {
        StoreError::InvalidWorkProjection(format!("invalid work id {value:?}: {error}"))
    })
}

pub(super) fn parse_work_run_id(value: &str) -> Result<WorkRunId, StoreError> {
    uuid::Uuid::parse_str(value)
        .map(WorkRunId)
        .map_err(|error| {
            StoreError::InvalidWorkProjection(format!("invalid work run id {value:?}: {error}"))
        })
}

pub(super) fn parse_root_execution_id(value: &str) -> Result<RootExecutionId, StoreError> {
    uuid::Uuid::parse_str(value)
        .map(RootExecutionId)
        .map_err(|error| {
            StoreError::InvalidWorkProjection(format!(
                "invalid root execution id {value:?}: {error}"
            ))
        })
}

pub(super) fn feed_parts(feed: &FeedId) -> (&'static str, String) {
    match feed {
        FeedId::Project(project) => ("project", project.0.clone()),
        FeedId::RootWork(root) => ("root_work", root.0.to_string()),
        FeedId::RunExecution(run) => ("run_execution", run.0.to_string()),
    }
}
