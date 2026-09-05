use std::collections::HashSet;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params, types::Value};

use super::super::{SqliteStore, StoreError};
use super::completion::{
    ancestors_admit_execution, applicable_work_obligations_at_cut_on, feed_head,
    load_work_obligation_records_on, required_child_seals, validated_required_child_waivers,
    work_run_uses_active_root_execution,
};
use super::execution::{
    work_evidence_kind_on, work_run_evidence_on, work_run_evidence_projection_on,
};
use super::feeds::{
    checkpoint_feed_end, load_handoff_offer_projection, load_typed_work_object,
    run_feed_position_for_object_on,
};
use super::integrity::{expected_environment_projection, expected_verification_projection};
use super::planning::{
    encode_state, normalize_work_catalog_key, require_work_item_relation_integrity,
};
use super::{
    CompletionRecoverySnapshot, WorkEvidenceProjectionSummary, WorkObligationRecord,
    WorkPrerequisitePage,
};
use crate::{
    CanonicalObject, ObjectHash, RestoredRecord, RestoredWorkEvidence,
    domain::{
        CompletionSeal, EnvironmentEvidence, FeedId, FeedPosition, ReadyWork, RootExecution,
        RootExecutionId, SessionId, VerificationEvidence, WorkAvailability, WorkBlocker,
        WorkCatalogPage, WorkCatalogQuery, WorkCheckpoint, WorkClaim, WorkClaimState,
        WorkCompletionRecovery, WorkCompletionRecoveryCause, WorkEvent, WorkEvidenceKind,
        WorkFeedEntry, WorkHandoffOffer, WorkId, WorkItem, WorkLifecycle, WorkObligationId,
        WorkPrerequisiteState, WorkReadinessReason, WorkReferenceCandidate, WorkRun, WorkRunId,
        WorkRunState, WorkTransition,
    },
};

#[cfg(test)]
use super::feeds::append_work_event;
#[cfg(test)]
use super::planning::{expect_root_contributor, persist_root_execution};
#[cfg(test)]
use super::{WORK_EVENT_DECODE_COUNT, WORK_ITEM_PROJECTION_DECODE_COUNT, WorkEventDraft};

#[cfg(test)]
mod tests;

const PROJECTED_WORK_AVAILABILITY_SQL: &str = r"
    CASE
        WHEN candidate.lifecycle != 'open' THEN 'closed'
        WHEN (candidate.active_run_id IS NOT NULL AND NOT EXISTS (
            SELECT 1
            FROM work_runs run
            JOIN work_root_executions execution
              ON execution.root_execution_id = run.root_execution_id
            WHERE run.run_id = candidate.active_run_id
              AND run.work_id = candidate.work_id
              AND execution.project_id = candidate.project_id
              AND execution.root_id = candidate.root_id
              AND execution.state = 'active'
        )) OR (candidate.active_run_id IS NULL
               AND COALESCE(json_extract(candidate.item_json, '$.restored'), 0) != 1)
          OR EXISTS (
            WITH RECURSIVE ancestors(work_id, parent_id, lifecycle) AS (
                SELECT parent.work_id, parent.parent_id, parent.lifecycle
                FROM work_items parent
                WHERE parent.work_id = candidate.parent_id
                UNION ALL
                SELECT parent.work_id, parent.parent_id, parent.lifecycle
                FROM work_items parent
                JOIN ancestors ON parent.work_id = ancestors.parent_id
            )
            SELECT 1 FROM ancestors WHERE lifecycle != 'open'
        ) THEN 'blocked'
        WHEN candidate.deferred_until_ms IS NOT NULL
          AND candidate.deferred_until_ms > ?2 THEN 'deferred'
        WHEN EXISTS (
            SELECT 1 FROM work_blockers blocker
            WHERE blocker.work_id = candidate.work_id AND blocker.state = 'active'
        ) OR EXISTS (
            SELECT 1
            FROM work_prerequisites edge
            LEFT JOIN work_items prerequisite
              ON prerequisite.work_id = edge.prerequisite_id
            LEFT JOIN work_items replacement
              ON replacement.work_id = prerequisite.superseded_by
            WHERE edge.work_id = candidate.work_id
              AND (
                  prerequisite.work_id IS NULL
                  OR NOT (
                      prerequisite.lifecycle = 'completed'
                      OR (
                          prerequisite.lifecycle = 'superseded'
                          AND replacement.lifecycle = 'completed'
                      )
                  )
              )
        ) THEN 'blocked'
        WHEN EXISTS (
            SELECT 1 FROM work_claims claim
            WHERE claim.run_id = candidate.active_run_id
              AND claim.state = 'active'
              AND claim.expires_at_ms > ?2
        ) THEN CASE WHEN EXISTS (
            SELECT 1 FROM work_runs run
            WHERE run.run_id = candidate.active_run_id
              AND run.last_checkpoint_hash IS NOT NULL
        ) THEN 'active' ELSE 'claimed' END
        ELSE 'ready'
    END
";
const PROJECTED_WORK_HAS_BLOCKER_SQL: &str = r"
    EXISTS (
        SELECT 1 FROM work_blockers blocker
        WHERE blocker.work_id = candidate.work_id AND blocker.state = 'active'
    ) OR EXISTS (
        SELECT 1
        FROM work_prerequisites edge
        LEFT JOIN work_items prerequisite
          ON prerequisite.work_id = edge.prerequisite_id
        LEFT JOIN work_items replacement
          ON replacement.work_id = prerequisite.superseded_by
        WHERE edge.work_id = candidate.work_id
          AND (
              prerequisite.work_id IS NULL
              OR NOT (
                  prerequisite.lifecycle = 'completed'
                  OR (
                      prerequisite.lifecycle = 'superseded'
                      AND replacement.lifecycle = 'completed'
                  )
              )
          )
    )
";

impl SqliteStore {
    pub(crate) fn work_completed_by_restored_record(
        &self,
        work_id: WorkId,
    ) -> Result<bool, StoreError> {
        let item = load_work_item(&self.connection, work_id)?;
        work_completed_by_restored_record_on(&self.connection, &item)
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
    ) -> Result<Vec<(ObjectHash, RestoredWorkEvidence)>, StoreError> {
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
        let mut statement = self.connection.prepare(
            "SELECT offer_hash, offer_json FROM work_handoff_offers
             WHERE work_id = ?1 ORDER BY offer_id",
        )?;
        let rows = statement
            .query_map([work_id.0.to_string()], |row| {
                Ok((row.get::<_, Option<String>>(0)?, row.get::<_, Vec<u8>>(1)?))
            })?
            .collect::<Result<Vec<_>, _>>()?;
        rows.into_iter()
            .map(|row| load_handoff_offer_projection(&self.connection, row))
            .collect()
    }

    #[cfg(test)]
    pub(crate) fn add_expected_root_contributor_fixture(
        &mut self,
        work_id: WorkId,
        participant: &SessionId,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let transaction = self.begin_work_mutation()?;
        let item = load_work_item(&transaction, work_id)?;
        let run = active_run_snapshot(&transaction, &item)?.ok_or_else(|| {
            StoreError::InvalidWorkProjection("fixture work has no active run".into())
        })?;
        let mut root_execution = load_root_execution(&transaction, run.root_execution_id)?;
        if expect_root_contributor(&mut root_execution, participant) {
            root_execution.revision += 1;
            root_execution.updated_at = now;
            persist_root_execution(&transaction, &root_execution)?;
        }
        let mut event = latest_canonical_work_event_for_item(&transaction, work_id)?;
        event.root_execution = Some(root_execution);
        event.created_at = now;
        append_work_event(&transaction, &WorkEventDraft::from(&event))?;
        transaction.commit()?;
        Ok(())
    }

    /// Returns canonical evidence hashes recorded for one run.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when a stored hash is invalid.
    pub fn work_run_evidence(&self, run_id: WorkRunId) -> Result<Vec<ObjectHash>, StoreError> {
        work_run_evidence_on(&self.connection, run_id)
    }

    /// Returns a bounded, hash-verified selection basis for one focus page.
    /// Candidate identity is chosen only by the immutable evidence hash; the
    /// canonical kind and environment binding are verified before selection.
    pub(crate) fn work_run_evidence_projection(
        &self,
        run_id: WorkRunId,
        required_environments: &[ObjectHash],
        limit: usize,
    ) -> Result<Vec<WorkEvidenceProjectionSummary>, StoreError> {
        work_run_evidence_projection_on(&self.connection, run_id, required_environments, limit)
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
    ) -> Result<Option<ObjectHash>, StoreError> {
        let stored = self
            .connection
            .query_row(
                "SELECT object_hash
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
                ObjectHash::from_stored(stored_hash).ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "latest run evidence has an invalid object hash".into(),
                    )
                })
            })
            .transpose()
    }

    /// Returns hash-verified obligation definitions and terminal resolutions
    /// for one exact run in trigger order.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when canonical bytes, feed positions, redundant
    /// scalar bindings, or resolution authority do not agree.
    pub(crate) fn work_run_obligations(
        &self,
        run_id: WorkRunId,
    ) -> Result<Vec<WorkObligationRecord>, StoreError> {
        load_work_obligation_records_on(&self.connection, run_id, None)
    }

    /// Derives the obligations that were still open at one exact immutable
    /// run-feed cut. A3 consumes this helper when sealing completion.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the cut names another feed, exceeds the
    /// current head, splits an atomic observation/definition append, or any
    /// canonical obligation binding is invalid.
    #[allow(
        dead_code,
        reason = "A2 ships the verified cut query as the explicit basis for the A3 completion gate"
    )]
    pub(crate) fn open_work_obligations_at_cut(
        &self,
        run_id: WorkRunId,
        cut: &FeedPosition,
    ) -> Result<Vec<WorkObligationId>, StoreError> {
        let records = applicable_work_obligations_at_cut_on(&self.connection, run_id, cut)?;
        let mut open = Vec::new();
        for record in records {
            let terminal_at_cut = record
                .resolution_hash
                .as_ref()
                .map(|hash| run_feed_position_for_object_on(&self.connection, run_id, hash))
                .transpose()?
                .is_some_and(|position| position.position <= cut.position);
            if !terminal_at_cut {
                open.push(record.obligation.obligation_id);
            }
        }
        Ok(open)
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
        evidence_hash: &ObjectHash,
    ) -> Result<WorkEvidenceKind, StoreError> {
        work_evidence_kind_on(&self.connection, run_id, evidence_hash)
    }

    pub(crate) fn load_verification_evidence(
        &self,
        evidence_hash: &ObjectHash,
    ) -> Result<VerificationEvidence, StoreError> {
        expected_verification_projection(&self.connection, evidence_hash)?;
        load_typed_work_object(&self.connection, evidence_hash, "verification_evidence")
    }

    pub(crate) fn load_environment_evidence(
        &self,
        evidence_hash: &ObjectHash,
    ) -> Result<EnvironmentEvidence, StoreError> {
        expected_environment_projection(&self.connection, evidence_hash)?;
        load_typed_work_object(&self.connection, evidence_hash, "environment_evidence")
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
        inspect_work_on(&self.connection, work_id, now)
    }

    /// Whether current local projections satisfy every non-acceptance
    /// completion precondition for this session. The final call must still
    /// supply criterion results and pass authority revalidation atomically.
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
        let item = load_work_item(&self.connection, work_id)?;
        let claim = self.current_work_claim_for_item(&item)?;
        self.work_completion_readiness_for_item(&item, claim.as_ref(), session_id, now)
    }

    pub(crate) fn work_completion_readiness_for_item(
        &self,
        item: &WorkItem,
        claim: Option<&WorkClaim>,
        session_id: &SessionId,
        now: DateTime<Utc>,
    ) -> Result<(bool, bool), StoreError> {
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
        let live_handoff = self.connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM work_handoff_offers
                 WHERE run_id = ?1 AND state = 'offered' AND expires_at_ms > ?2
             )",
            params![run_id.0.to_string(), now.timestamp_millis()],
            |row| row.get::<_, bool>(0),
        )?;
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
        let required_child_count = self.connection.query_row(
            "SELECT COUNT(*) FROM work_items child
             WHERE child.parent_id = ?1
               AND child.child_requirement = 'required'",
            [work_id.0.to_string()],
            |row| row.get::<_, i64>(0),
        )?;
        if required_child_count != 0 {
            let required_child_seal_count =
                required_child_seals(&self.connection, work_id, run.root_execution_id)?.len();
            let root_execution = load_root_execution(&self.connection, run.root_execution_id)?;
            let required_child_waiver_count =
                validated_required_child_waivers(&self.connection, work_id, &root_execution)?.len();
            if usize::try_from(required_child_count).ok()
                != Some(required_child_seal_count + required_child_waiver_count)
            {
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

    /// Returns ready work ordered by priority, unblocking value, age, and stable id.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when readiness projections cannot be read or verified.
    pub fn ready_work(
        &self,
        project_id: &crate::domain::ProjectId,
        now: DateTime<Utc>,
        limit: u32,
    ) -> Result<Vec<ReadyWork>, StoreError> {
        let sql = format!(
            "WITH classified AS (
                 SELECT candidate.work_id, candidate.priority,
                        candidate.created_at_ms,
                        ({PROJECTED_WORK_AVAILABILITY_SQL}) AS availability
                 FROM work_items candidate
                 WHERE candidate.project_id = ?1
                   AND candidate.lifecycle = 'open'
             )
             SELECT work_id FROM classified
             WHERE availability = 'ready'
             ORDER BY priority,
                      (SELECT COUNT(*)
                       FROM work_prerequisites dependency
                       JOIN work_items dependant
                         ON dependant.work_id = dependency.work_id
                       WHERE dependency.prerequisite_id = classified.work_id
                         AND dependant.lifecycle = 'open') DESC,
                      created_at_ms, work_id
             LIMIT ?3"
        );
        let mut statement = self.connection.prepare(&sql)?;
        let ids = statement
            .query_map(
                params![
                    project_id.0.as_str(),
                    now.timestamp_millis(),
                    i64::from(limit.clamp(1, 1_000))
                ],
                |row| row.get::<_, String>(0),
            )?
            .map(|row| parse_work_id(&row?))
            .collect::<Result<Vec<_>, _>>()?;
        ids.into_iter()
            .map(|id| inspect_work_catalog_on(&self.connection, id, now))
            .filter_map(|result| match result {
                Ok(view) if view.availability == WorkAvailability::Ready => Some(Ok(view)),
                Ok(_) => None,
                Err(error) => Some(Err(error)),
            })
            .collect()
    }

    /// Queries every lifecycle/availability class without mutating focus.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when canonical work projections cannot be read.
    pub fn query_work_catalog(
        &self,
        project_id: &crate::domain::ProjectId,
        now: DateTime<Utc>,
        query: &WorkCatalogQuery,
    ) -> Result<WorkCatalogPage, StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        let page = work_catalog_page_on(&transaction, project_id, now, query)?;
        transaction.commit()?;
        Ok(page)
    }

    /// The counted agent list shares its count, page and displayed holders in
    /// one read snapshot. Ambient catalog callers never pay for this count.
    pub(crate) fn query_work_catalog_listing(
        &self,
        project_id: &crate::domain::ProjectId,
        now: DateTime<Utc>,
        query: &WorkCatalogQuery,
    ) -> Result<(WorkCatalogPage, usize, Vec<WorkClaim>), StoreError> {
        let transaction = self.connection.unchecked_transaction()?;
        let (sql, parameters) = work_catalog_sql(project_id, now, query, false)?;
        #[cfg(test)]
        super::WORK_CATALOG_COUNT_QUERIES.with(|count| count.set(count.get() + 1));
        let total: i64 =
            transaction.query_row(&sql, rusqlite::params_from_iter(parameters.iter()), |row| {
                row.get(0)
            })?;
        let total = usize::try_from(total).map_err(|_| {
            StoreError::InvalidWorkProjection("catalog count is outside the supported range".into())
        })?;
        #[cfg(test)]
        tests::after_catalog_count();
        let page = work_catalog_page_on(&transaction, project_id, now, query)?;
        let mut claims = Vec::new();
        for item in &page.items {
            if let Some(run_id) = item.work.active_run_id
                && let Some(claim) = load_work_claim_optional(&transaction, run_id)?
                && claim.state == WorkClaimState::Active
                && claim.expires_at > now
            {
                claims.push(claim);
            }
        }
        transaction.commit()?;
        Ok((page, total, claims))
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
            "SELECT position, object_kind, object_hash
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
            let object_hash =
                ObjectHash::from_stored(hash.clone()).ok_or(StoreError::InvalidStoredHash(hash))?;
            Ok(WorkFeedEntry {
                position: FeedPosition {
                    feed: feed.clone(),
                    position,
                },
                object_kind,
                object_hash,
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
            "SELECT position, object_kind, object_hash
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
                let object_hash = ObjectHash::from_stored(hash.clone())
                    .ok_or(StoreError::InvalidStoredHash(hash))?;
                Ok(WorkFeedEntry {
                    position: FeedPosition {
                        feed: feed.clone(),
                        position,
                    },
                    object_kind,
                    object_hash,
                })
            })
            .collect()
    }

    /// Reads the newest immutable entries in one exact feed, returned oldest
    /// to newest within the bounded tail.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the feed projection or an object hash is
    /// invalid.
    pub fn work_feed_tail(
        &self,
        feed: &FeedId,
        limit: u32,
    ) -> Result<Vec<WorkFeedEntry>, StoreError> {
        let (feed_kind, feed_id) = feed_parts(feed);
        let mut statement = self.connection.prepare(
            "SELECT position, object_kind, object_hash FROM (
                 SELECT position, object_kind, object_hash
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
                    object_hash: ObjectHash::from_stored(hash.clone())
                        .ok_or(StoreError::InvalidStoredHash(hash))?,
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
            "SELECT position, object_hash FROM (
                 SELECT entry.position, entry.object_hash
                 FROM work_feed_entries entry
                 JOIN objects object ON object.object_hash = entry.object_hash
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
                let object_hash = ObjectHash::from_stored(hash.clone())
                    .ok_or(StoreError::InvalidStoredHash(hash))?;
                Ok(WorkFeedEntry {
                    position: FeedPosition {
                        feed: FeedId::RootWork(item.root_id),
                        position,
                    },
                    object_kind: "work_event".into(),
                    object_hash,
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
                "SELECT object.object_hash, object.canonical_json
             FROM work_feed_entries entry
             JOIN objects object ON object.object_hash = entry.object_hash
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
             JOIN objects object ON object.object_hash = entry.object_hash
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

    #[cfg(test)]
    pub(crate) fn append_test_work_event(&mut self, event: &WorkEvent) -> Result<(), StoreError> {
        let transaction = self.begin_work_mutation()?;
        append_work_event(&transaction, &WorkEventDraft::from(event))?;
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
            append_work_event(&transaction, &WorkEventDraft::from(event))?;
        }
        transaction.commit()?;
        Ok(())
    }
}

const MAX_AMBIGUOUS_WORK_CANDIDATES: usize = 8;

pub(super) fn resolve_work_ref_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
    work_ref: &str,
) -> Result<WorkItem, StoreError> {
    let work_ref = work_ref.trim();
    if work_ref.is_empty() {
        return Err(StoreError::InvalidWork(
            "work reference must not be empty".into(),
        ));
    }
    let mut statement = connection.prepare(
        "SELECT work_id, COUNT(*) OVER()
         FROM work_items
         WHERE project_id = ?1 AND (short_ref = ?2 OR work_id = ?2)
         ORDER BY work_id LIMIT ?3",
    )?;
    let matches = statement
        .query_map(
            params![
                project_id.0,
                work_ref,
                i64::try_from(MAX_AMBIGUOUS_WORK_CANDIDATES).map_err(|_| {
                    StoreError::InvalidWorkProjection(
                        "ambiguous-work candidate limit overflowed SQLite".into(),
                    )
                })?
            ],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?)),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    match matches.as_slice() {
        [(work_id, _)] => load_work_item(connection, parse_work_id(work_id)?),
        [] => Err(StoreError::InvalidWork(format!(
            "work reference {work_ref:?} does not exist in project {:?}",
            project_id.0
        ))),
        _ => {
            let total = usize::try_from(matches[0].1).map_err(|_| {
                StoreError::InvalidWorkProjection(
                    "ambiguous-work candidate count overflowed usize".into(),
                )
            })?;
            let candidates = matches
                .iter()
                .map(|(work_id, _)| load_work_item(connection, parse_work_id(work_id)?))
                .collect::<Result<Vec<_>, StoreError>>()?;
            let more = total.saturating_sub(candidates.len());
            Err(ambiguous_work_reference_error(work_ref, candidates, more))
        }
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
    };
    Ok(WorkCompletionRecovery {
        cause,
        item,
        command,
    })
}

pub(super) fn completion_recovery_snapshot_on(
    connection: &Connection,
    work: &WorkItem,
    run_id: WorkRunId,
    cause: WorkCompletionRecoveryCause,
) -> Result<CompletionRecoverySnapshot, StoreError> {
    Ok(CompletionRecoverySnapshot {
        recovery: completion_recovery_on(connection, work, cause)?,
        obligations: load_work_obligation_records_on(connection, run_id, None)?,
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

fn push_catalog_parameter(parameters: &mut Vec<Value>, value: Value) -> String {
    parameters.push(value);
    format!("?{}", parameters.len())
}

pub(super) fn catalog_literal_fts_query(value: &str) -> String {
    format!("\"{}\"", value.replace('"', "\"\""))
}

fn work_catalog_page_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
    now: DateTime<Utc>,
    query: &WorkCatalogQuery,
) -> Result<WorkCatalogPage, StoreError> {
    let (sql, parameters) = work_catalog_sql(project_id, now, query, true)?;
    let mut statement = connection.prepare(&sql)?;
    let ids = statement
        .query_map(rusqlite::params_from_iter(parameters.iter()), |row| {
            row.get::<_, String>(0)
        })?
        .map(|row| parse_work_id(&row?))
        .collect::<Result<Vec<_>, StoreError>>()?;
    let limit = query.limit.clamp(1, 1_000) as usize;
    let mut items = Vec::with_capacity(ids.len());
    for work_id in ids {
        items.push(inspect_work_catalog_on(connection, work_id, now)?);
    }
    let next_after = (items.len() > limit).then(|| items[limit - 1].work.work_id);
    items.truncate(limit);
    Ok(WorkCatalogPage { items, next_after })
}

fn work_catalog_sql(
    project_id: &crate::domain::ProjectId,
    now: DateTime<Utc>,
    query: &WorkCatalogQuery,
    page: bool,
) -> Result<(String, Vec<Value>), StoreError> {
    let mut parameters = vec![
        Value::Text(project_id.0.clone()),
        Value::Integer(now.timestamp_millis()),
    ];
    let mut candidate_filters = vec!["candidate.project_id = ?1".to_owned()];
    if !query.lifecycles.is_empty() {
        let placeholders = query
            .lifecycles
            .iter()
            .map(|lifecycle| {
                Ok(push_catalog_parameter(
                    &mut parameters,
                    Value::Text(encode_state(*lifecycle)?),
                ))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        candidate_filters.push(format!(
            "candidate.lifecycle IN ({})",
            placeholders.join(", ")
        ));
    }
    let mut ownership_filters = Vec::new();
    if let Some(assigned_to) = query
        .assigned_to
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let parameter = push_catalog_parameter(
            &mut parameters,
            Value::Text(normalize_work_catalog_key(assigned_to)),
        );
        ownership_filters.push(format!("candidate.assigned_to_key = {parameter}"));
    }
    if let Some(session) = &query.held_by {
        let parameter = push_catalog_parameter(&mut parameters, Value::Text(session.0.clone()));
        ownership_filters.push(format!(
            "EXISTS (SELECT 1 FROM work_claims claim
                     WHERE claim.run_id = candidate.active_run_id
                       AND claim.holder_session_id = {parameter}
                       AND claim.state = 'active' AND claim.expires_at_ms > ?2)"
        ));
    }
    if !ownership_filters.is_empty() {
        candidate_filters.push(format!("({})", ownership_filters.join(" OR ")));
    }
    if let Some(label) = query
        .label
        .as_deref()
        .map(str::trim)
        .filter(|value| !value.is_empty())
    {
        let parameter = push_catalog_parameter(
            &mut parameters,
            Value::Text(normalize_work_catalog_key(label)),
        );
        candidate_filters.push(format!(
            "EXISTS (
                 SELECT 1 FROM work_item_labels label
                 WHERE label.work_id = candidate.work_id
                   AND label.label_key = {parameter}
             )"
        ));
    }
    if let Some(search) = query
        .search
        .as_deref()
        .map(normalize_work_catalog_key)
        .filter(|value| !value.is_empty())
    {
        let search_characters = search.chars().count();
        let parameter = push_catalog_parameter(
            &mut parameters,
            Value::Text(if search_characters >= 3 {
                catalog_literal_fts_query(&search)
            } else {
                search
            }),
        );
        if search_characters >= 3 {
            candidate_filters.push(format!(
                "candidate.work_id IN (
                     SELECT work_id FROM work_catalog_fts
                     WHERE work_catalog_fts MATCH {parameter}
                 )"
            ));
        } else {
            candidate_filters.push(format!("instr(candidate.search_text_key, {parameter}) > 0"));
        }
    }

    let mut classified_filters = Vec::new();
    if !query.availabilities.is_empty() {
        let placeholders = query
            .availabilities
            .iter()
            .map(|availability| {
                Ok(push_catalog_parameter(
                    &mut parameters,
                    Value::Text(encode_state(*availability)?),
                ))
            })
            .collect::<Result<Vec<_>, StoreError>>()?;
        classified_filters.push(format!("availability IN ({})", placeholders.join(", ")));
    }
    if query.blocked_only {
        candidate_filters.push(format!("({PROJECTED_WORK_HAS_BLOCKER_SQL})"));
    }
    if page && let Some(after) = query.after {
        let parameter = push_catalog_parameter(&mut parameters, Value::Text(after.0.to_string()));
        candidate_filters.push(format!("candidate.work_id > {parameter}"));
    }
    let classified_where = if classified_filters.is_empty() {
        String::new()
    } else {
        format!("WHERE {}", classified_filters.join(" AND "))
    };
    let selection = if page {
        let limit = i64::from(query.limit.clamp(1, 1_000)).saturating_add(1);
        let parameter = push_catalog_parameter(&mut parameters, Value::Integer(limit));
        format!("work_id FROM classified {classified_where} ORDER BY work_id LIMIT {parameter}")
    } else {
        format!("COUNT(*) FROM classified {classified_where}")
    };
    let sql = format!(
        "WITH classified AS (
             SELECT candidate.work_id,
                    ({PROJECTED_WORK_AVAILABILITY_SQL}) AS availability
             FROM work_items candidate
             WHERE {}
         )
         SELECT {selection}",
        candidate_filters.join(" AND ")
    );
    Ok((sql, parameters))
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

fn inspect_work_catalog_on(
    connection: &Connection,
    work_id: WorkId,
    now: DateTime<Utc>,
) -> Result<ReadyWork, StoreError> {
    let work = load_work_item_projection(connection, work_id)?;
    let blockers = load_active_blocker_catalog_rows(connection, work_id)?;
    let prerequisites = classified_prerequisite_catalog_rows(connection, work_id)?;
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
    let availability = if !matches!(work.lifecycle, WorkLifecycle::Open) {
        reason_codes.push(WorkReadinessReason::LifecycleClosed);
        why.push(format!("lifecycle is {:?}", work.lifecycle));
        WorkAvailability::Closed
    } else if !projected_ancestors_admit_execution(connection, &work)?
        || !projected_run_uses_active_root_execution(connection, &work)?
    {
        reason_codes.push(WorkReadinessReason::ParentDisallowsExecution);
        why.push("the ancestor or root-execution generation does not admit execution".into());
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
        why.push("open, admitted, unblocked, and unclaimed".into());
    }
    Ok(ReadyWork {
        work,
        availability,
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
    let availability = if !matches!(work.lifecycle, WorkLifecycle::Open) {
        reason_codes.push(WorkReadinessReason::LifecycleClosed);
        why.push(format!("lifecycle is {:?}", work.lifecycle));
        WorkAvailability::Closed
    } else if !ancestors_admit_execution(connection, &work)?
        || !((work.restored && work.active_run_id.is_none())
            || work_run_uses_active_root_execution(connection, &work)?)
    {
        reason_codes.push(WorkReadinessReason::ParentDisallowsExecution);
        why.push("the ancestor or root-execution generation does not admit execution".into());
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
        why.push("open, admitted, unblocked, and unclaimed".into());
    }
    Ok(ReadyWork {
        work,
        availability,
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

fn projected_ancestors_admit_execution(
    connection: &Connection,
    item: &WorkItem,
) -> Result<bool, StoreError> {
    let mut parent_id = item.parent_id;
    let mut visited = HashSet::new();
    let mut reached_root = item.work_id == item.root_id;
    while let Some(parent) = parent_id {
        if !visited.insert(parent) || visited.len() > 1_024 {
            return Err(StoreError::InvalidWorkProjection(
                "work hierarchy is cyclic or exceeds the corruption guard".into(),
            ));
        }
        let row: Option<(String, String, Option<String>, String)> = connection
            .query_row(
                "SELECT project_id, root_id, parent_id, lifecycle
                 FROM work_items WHERE work_id = ?1",
                [parent.0.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?)),
            )
            .optional()?;
        let (project_id, root_id, next_parent, lifecycle) = row.ok_or_else(|| {
            StoreError::InvalidWorkProjection(format!("work ancestor {parent:?} is missing"))
        })?;
        if project_id != item.project_id.0 || root_id != item.root_id.0.to_string() {
            return Err(StoreError::InvalidWorkProjection(format!(
                "work ancestor {parent:?} crosses its project or root boundary"
            )));
        }
        if lifecycle != "open" {
            return Ok(false);
        }
        reached_root |= parent == item.root_id;
        parent_id = next_parent.map(|value| parse_work_id(&value)).transpose()?;
    }
    if !reached_root {
        return Err(StoreError::InvalidWorkProjection(format!(
            "work {:?} does not reach its declared root {:?}",
            item.work_id, item.root_id
        )));
    }
    Ok(true)
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
            "SELECT run.last_checkpoint_hash, claim.state, claim.expires_at_ms
             FROM work_runs run
             LEFT JOIN work_claims claim ON claim.run_id = run.run_id
             WHERE run.run_id = ?1 AND run.work_id = ?2",
            params![run_id.0.to_string(), work.work_id.0.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
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

pub(super) fn latest_canonical_work_event_for_item_optional(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Option<WorkEvent>, StoreError> {
    let projected_hash = connection
        .query_row(
            "SELECT latest_event_hash FROM work_items WHERE work_id = ?1",
            [work_id.0.to_string()],
            |row| row.get::<_, Option<String>>(0),
        )
        .optional()?;
    let latest = connection
        .query_row(
            "SELECT object.object_hash, object.canonical_json
             FROM objects object
             JOIN work_feed_entries entry ON entry.object_hash = object.object_hash
             WHERE object.object_kind = 'work_event'
               AND entry.feed_kind = 'project'
               AND entry.object_kind = 'work_event'
               AND entry.work_id = ?1
             ORDER BY entry.position DESC LIMIT 1",
            [work_id.0.to_string()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?;
    match (projected_hash.flatten(), latest) {
        (None, None) => Ok(None),
        (Some(projected_hash), Some((stored_hash, bytes))) => {
            let projected_hash = ObjectHash::from_stored(projected_hash.clone())
                .ok_or(StoreError::InvalidStoredHash(projected_hash))?;
            let stored_hash_value = ObjectHash::from_stored(stored_hash.clone())
                .ok_or(StoreError::InvalidStoredHash(stored_hash.clone()))?;
            if projected_hash != stored_hash_value {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "work item {work_id:?} latest-event binding differs from its indexed feed head"
                )));
            }
            let event = decode_canonical_work_event((stored_hash, bytes))?;
            if event.work_id != work_id {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "work item {work_id:?} latest-event binding names another item"
                )));
            }
            Ok(Some(event))
        }
        _ => Err(StoreError::InvalidWorkProjection(format!(
            "work item {work_id:?} latest-event binding is incomplete"
        ))),
    }
}

pub(in crate::storage) fn canonical_work_events_for_item(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<WorkEvent>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT object.object_hash, object.canonical_json
         FROM objects object
         JOIN work_feed_entries entry ON entry.object_hash = object.object_hash
         WHERE object.object_kind = 'work_event'
           AND entry.feed_kind = 'project'
           AND entry.object_kind = 'work_event'
           AND entry.work_id = ?1
         ORDER BY entry.position",
    )?;
    statement
        .query_map([work_id.0.to_string()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })?
        .map(|row| decode_canonical_work_event(row?))
        .collect()
}

fn latest_canonical_work_event_on_feed(
    connection: &Connection,
    feed_kind: &str,
    feed_id: &str,
    required_snapshot: &str,
) -> Result<WorkEvent, StoreError> {
    let stored = connection
        .query_row(
            "SELECT object.object_hash, object.canonical_json
             FROM work_feed_entries entry
             JOIN objects object ON object.object_hash = entry.object_hash
             WHERE entry.feed_kind = ?1 AND entry.feed_id = ?2
               AND entry.object_kind = 'work_event'
               AND object.object_kind = 'work_event'
               AND json_type(object.canonical_json, ?3) IS NOT NULL
               AND json_type(object.canonical_json, ?3) != 'null'
             ORDER BY entry.position DESC LIMIT 1",
            params![feed_kind, feed_id, required_snapshot],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?
        .ok_or_else(|| {
            StoreError::InvalidWorkProjection(format!(
                "work feed {feed_kind}:{feed_id} has no canonical event"
            ))
        })?;
    decode_canonical_work_event(stored)
}

fn decode_canonical_work_event(stored: (String, Vec<u8>)) -> Result<WorkEvent, StoreError> {
    #[cfg(test)]
    WORK_EVENT_DECODE_COUNT.with(|count| count.set(count.get().saturating_add(1)));
    let (stored_hash, bytes) = stored;
    let hash = ObjectHash::from_stored(stored_hash.clone())
        .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
    CanonicalObject::verify(&hash, bytes)?.decode()
}

pub(super) fn load_work_item_projection(
    connection: &Connection,
    work_id: WorkId,
) -> Result<WorkItem, StoreError> {
    let row: Option<(Vec<u8>, bool)> = connection
        .query_row(
            "SELECT item_json,
                    work_id = json_extract(item_json, '$.work_id') AND
                    project_id = json_extract(item_json, '$.project_id') AND
                    short_ref = json_extract(item_json, '$.short_ref') AND
                    root_id = json_extract(item_json, '$.root_id') AND
                    COALESCE(parent_id, '') = COALESCE(json_extract(item_json, '$.parent_id'), '') AND
                    child_requirement = json_extract(item_json, '$.child_requirement') AND
                    lifecycle = json_extract(item_json, '$.lifecycle') AND
                    priority = json_extract(item_json, '$.priority') AND
                    COALESCE(assigned_to, '') = COALESCE(json_extract(item_json, '$.assigned_to'), '') AND
                    COALESCE(deferred_until_ms, -1) = COALESCE(
                        CAST(strftime('%s', json_extract(item_json, '$.deferred_until')) AS INTEGER) * 1000
                        + CASE WHEN instr(json_extract(item_json, '$.deferred_until'), '.') > 0
                            THEN CAST(substr(
                                substr(
                                    json_extract(item_json, '$.deferred_until'),
                                    instr(json_extract(item_json, '$.deferred_until'), '.') + 1,
                                    instr(json_extract(item_json, '$.deferred_until'), 'Z')
                                        - instr(json_extract(item_json, '$.deferred_until'), '.') - 1
                                ) || '000', 1, 3
                            ) AS INTEGER)
                            ELSE 0 END, -1) AND
                    revision = json_extract(item_json, '$.revision') AND
                    COALESCE(active_run_id, '') = COALESCE(json_extract(item_json, '$.active_run_id'), '') AND
                    COALESCE(superseded_by, '') = COALESCE(json_extract(item_json, '$.superseded_by'), '') AND
                    COALESCE(source_snapshot_hash, '') = COALESCE(json_extract(item_json, '$.source_snapshot_id'), '') AND
                    created_at_ms = CAST(strftime('%s', json_extract(item_json, '$.created_at')) AS INTEGER) * 1000
                        + CASE WHEN instr(json_extract(item_json, '$.created_at'), '.') > 0
                            THEN CAST(substr(
                                substr(
                                    json_extract(item_json, '$.created_at'),
                                    instr(json_extract(item_json, '$.created_at'), '.') + 1,
                                    instr(json_extract(item_json, '$.created_at'), 'Z')
                                        - instr(json_extract(item_json, '$.created_at'), '.') - 1
                                ) || '000', 1, 3
                            ) AS INTEGER)
                            ELSE 0 END AND
                    updated_at_ms = CAST(strftime('%s', json_extract(item_json, '$.updated_at')) AS INTEGER) * 1000
                        + CASE WHEN instr(json_extract(item_json, '$.updated_at'), '.') > 0
                            THEN CAST(substr(
                                substr(
                                    json_extract(item_json, '$.updated_at'),
                                    instr(json_extract(item_json, '$.updated_at'), '.') + 1,
                                    instr(json_extract(item_json, '$.updated_at'), 'Z')
                                        - instr(json_extract(item_json, '$.updated_at'), '.') - 1
                                ) || '000', 1, 3
                            ) AS INTEGER)
                            ELSE 0 END
             FROM work_items WHERE work_id = ?1",
            [work_id.0.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (bytes, scalar_bound) = row.ok_or(StoreError::WorkNotFound(work_id))?;
    #[cfg(test)]
    WORK_ITEM_PROJECTION_DECODE_COUNT.with(|count| count.set(count.get().saturating_add(1)));
    let item: WorkItem = serde_json::from_slice(&bytes)?;
    if !scalar_bound
        || item.work_id != work_id
        || item.schema_version != crate::schema::SCHEMA_VERSION
    {
        return Err(StoreError::InvalidWorkProjection(format!(
            "work item {work_id:?} differs from its scalar projection binding"
        )));
    }
    Ok(item)
}

pub(in crate::storage) fn load_work_item(
    connection: &Connection,
    work_id: WorkId,
) -> Result<WorkItem, StoreError> {
    let item = load_work_item_projection(connection, work_id)?;
    if let Some(event) = latest_canonical_work_event_for_item_optional(connection, work_id)? {
        if event.work != item {
            return Err(StoreError::InvalidWorkProjection(format!(
                "work item {work_id:?} differs from its latest canonical event"
            )));
        }
    } else {
        let Some((_, record)) = latest_restored_record(connection, work_id)? else {
            return Err(StoreError::InvalidWorkProjection(format!(
                "work item {work_id:?} has neither canonical native nor restored history"
            )));
        };
        if !crate::graph_snapshot::restored_item_basis_matches(
            &record.item,
            &record.project_id,
            &item,
        ) {
            return Err(StoreError::InvalidWorkProjection(format!(
                "work item {work_id:?} differs from its canonical restored record"
            )));
        }
        let load = super::super::graph_snapshot::work_graph_snapshot_load_origin_on(
            connection,
            &item.project_id,
        )?;
        if item.created_by != load.actor
            || item.created_at != load.loaded_at
            || item.updated_at != load.loaded_at
        {
            return Err(StoreError::InvalidWorkProjection(format!(
                "work item {work_id:?} differs from its canonical load attribution"
            )));
        }
        super::planning::validated_current_work_relation_basis(connection, work_id)?;
    }
    Ok(item)
}

pub(super) fn work_completed_by_restored_record_on(
    connection: &Connection,
    item: &WorkItem,
) -> Result<bool, StoreError> {
    if !item.restored || item.lifecycle != WorkLifecycle::Completed || item.active_run_id.is_some()
    {
        return Ok(false);
    }
    if let Some(event) = latest_canonical_work_event_for_item_optional(connection, item.work_id)? {
        let run = event.run.as_ref().ok_or_else(|| {
            StoreError::InvalidWorkProjection("native completed work has no canonical run".into())
        })?;
        let seal_hash = run.completion_seal.as_ref().ok_or_else(|| {
            StoreError::InvalidWorkProjection("native completed work has no canonical seal".into())
        })?;
        let projected = connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM work_completion_seals
                 WHERE work_id = ?1 AND run_id = ?2 AND seal_hash = ?3
             )",
            params![
                item.work_id.0.to_string(),
                run.run_id.0.to_string(),
                seal_hash.as_str()
            ],
            |row| row.get::<_, bool>(0),
        )?;
        let seal: CompletionSeal =
            load_typed_work_object(connection, seal_hash, "completion_seal")?;
        if event.work != *item
            || run.work_id != item.work_id
            || run.state != WorkRunState::Completed
            || !projected
            || seal.work_id != item.work_id
            || seal.run_id != run.run_id
            || seal.root_id != item.root_id
            || seal.root_execution_id != run.root_execution_id
        {
            return Err(StoreError::InvalidWorkProjection(
                "native completed work differs from its canonical seal or seal projection".into(),
            ));
        }
        return Ok(false);
    }
    let has_native_seal = connection.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM work_completion_seals WHERE work_id = ?1
         )",
        [item.work_id.0.to_string()],
        |row| row.get::<_, bool>(0),
    )?;
    let (_, record) = latest_restored_record(connection, item.work_id)?.ok_or_else(|| {
        StoreError::InvalidWorkProjection("completed-by-record work has no restored record".into())
    })?;
    if has_native_seal
        || record.history.completion.is_none()
        || !crate::graph_snapshot::restored_item_basis_matches(
            &record.item,
            &record.project_id,
            item,
        )
    {
        return Err(StoreError::InvalidWorkProjection(
            "completed-by-record work differs from its canonical restored completion".into(),
        ));
    }
    Ok(true)
}

pub(in crate::storage) fn latest_restored_record_hash(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Option<ObjectHash>, StoreError> {
    Ok(latest_restored_record(connection, work_id)?.map(|(hash, _)| hash))
}

pub(super) fn latest_restored_record(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Option<(ObjectHash, RestoredRecord)>, StoreError> {
    let row = connection
        .query_row(
            "SELECT generation_index, record_hash
             FROM work_restored_records WHERE work_id = ?1
             ORDER BY generation_index DESC LIMIT 1",
            [work_id.0.to_string()],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?)),
        )
        .optional()?;
    row.map(|(generation_index, stored_hash)| {
        let hash = ObjectHash::from_stored(stored_hash.clone())
            .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
        let record: RestoredRecord =
            load_typed_work_object(connection, &hash, "work_restored_record")?;
        if record.work_id != work_id
            || i64::try_from(record.generation_index).ok() != Some(generation_index)
        {
            return Err(StoreError::InvalidWorkProjection(format!(
                "restored record for {work_id:?} differs from its projection binding"
            )));
        }
        Ok((hash, record))
    })
    .transpose()
}

fn restored_records_for_item(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<RestoredRecord>, StoreError> {
    let rows = connection
        .prepare(
            "SELECT generation_index, record_hash FROM work_restored_records
             WHERE work_id = ?1 ORDER BY generation_index",
        )?
        .query_map([work_id.0.to_string()], |row| {
            Ok((row.get::<_, i64>(0)?, row.get::<_, String>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter()
        .enumerate()
        .map(|(expected, (generation, stored_hash))| {
            if i64::try_from(expected).ok() != Some(generation) {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "restored history for {work_id:?} is not dense"
                )));
            }
            let hash = ObjectHash::from_stored(stored_hash.clone())
                .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
            let record: RestoredRecord =
                load_typed_work_object(connection, &hash, "work_restored_record")?;
            if record.work_id != work_id || record.generation_index != expected {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "restored history for {work_id:?} differs from its projection binding"
                )));
            }
            Ok(record)
        })
        .collect()
}

fn restored_record_binds_work(
    connection: &Connection,
    record_hash: &ObjectHash,
    work_id: WorkId,
) -> Result<bool, StoreError> {
    let projected: bool = connection.query_row(
        "SELECT EXISTS(
             SELECT 1 FROM work_restored_records
             WHERE work_id = ?1 AND record_hash = ?2
         )",
        params![work_id.0.to_string(), record_hash.as_str()],
        |row| row.get(0),
    )?;
    if !projected {
        return Ok(false);
    }
    let record: RestoredRecord =
        load_typed_work_object(connection, record_hash, "work_restored_record")?;
    Ok(record.work_id == work_id)
}

fn native_work_event_optional(
    connection: &Connection,
    hash: &ObjectHash,
) -> Result<Option<WorkEvent>, StoreError> {
    let stored = connection
        .query_row(
            "SELECT object_kind, canonical_json FROM objects WHERE object_hash = ?1",
            [hash.as_str()],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .optional()?;
    let Some((kind, bytes)) = stored else {
        return Err(StoreError::InvalidWorkProjection(format!(
            "relation anchor {hash} is missing"
        )));
    };
    if kind != "work_event" {
        return Ok(None);
    }
    Ok(Some(CanonicalObject::verify(hash, bytes)?.decode()?))
}

fn restored_work_evidence_for_item(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<(ObjectHash, RestoredWorkEvidence)>, StoreError> {
    let rows = connection
        .prepare(
            "SELECT evidence_hash, record_hash, sequence, gate_name, created_at_ms
             FROM work_restored_evidence INDEXED BY work_restored_evidence_work
             WHERE work_id = ?1
             ORDER BY sequence, evidence_hash",
        )?
        .query_map([work_id.0.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, i64>(2)?,
                row.get::<_, Option<String>>(3)?,
                row.get::<_, i64>(4)?,
            ))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter()
        .enumerate()
        .map(
            |(index, (stored_hash, stored_record, sequence, gate_name, created_at_ms))| {
                let expected_sequence = i64::try_from(index)
                    .ok()
                    .and_then(|value| value.checked_add(1));
                let hash = ObjectHash::from_stored(stored_hash.clone())
                    .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
                let evidence: RestoredWorkEvidence =
                    load_typed_work_object(connection, &hash, "work_restored_evidence")?;
                let scalar_bound = evidence.work_id == work_id
                    && evidence.restored_record.as_str() == stored_record
                    && evidence.sequence == sequence
                    && Some(sequence) == expected_sequence
                    && evidence.gate.as_ref().map(|gate| gate.name.as_str())
                        == gate_name.as_deref()
                    && evidence.created_at.timestamp_millis() == created_at_ms
                    && restored_record_binds_work(connection, &evidence.restored_record, work_id)?;
                if !scalar_bound {
                    return Err(StoreError::InvalidWorkProjection(format!(
                        "restored evidence {hash} differs from its projection binding"
                    )));
                }
                Ok((hash, evidence))
            },
        )
        .collect()
}

pub(in crate::storage) fn verified_work_identity(
    connection: &Connection,
    work_id: WorkId,
) -> Result<(crate::domain::ProjectId, WorkId), StoreError> {
    let item = load_work_item(connection, work_id)?;
    Ok((item.project_id, item.root_id))
}

pub(in crate::storage) fn context_work_feed_heads(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<FeedPosition>, StoreError> {
    let item = load_work_item(connection, work_id)?;
    let mut feeds = vec![
        FeedId::Project(item.project_id),
        FeedId::RootWork(item.root_id),
    ];
    if let Some(run_id) = item.active_run_id {
        feeds.push(FeedId::RunExecution(run_id));
    }
    feeds
        .into_iter()
        .map(|feed| {
            Ok(FeedPosition {
                position: feed_head(connection, &feed)?,
                feed,
            })
        })
        .collect()
}

pub(super) fn load_work_items_query(
    connection: &Connection,
    query: &str,
    work_id: WorkId,
) -> Result<Vec<WorkItem>, StoreError> {
    let mut statement = connection.prepare(query)?;
    statement
        .query_map([work_id.0.to_string()], |row| row.get::<_, Vec<u8>>(0))?
        .map(|row| serde_json::from_slice(&row?).map_err(StoreError::from))
        .collect()
}

pub(super) fn load_work_run(
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
                    COALESCE(last_checkpoint_hash, '') = COALESCE(json_extract(run_json, '$.last_checkpoint'), '') AND
                    COALESCE(completion_seal_hash, '') = COALESCE(json_extract(run_json, '$.completion_seal'), '')
             FROM work_runs WHERE run_id = ?1",
            [run_id.0.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
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

pub(super) fn active_run_snapshot(
    connection: &Connection,
    item: &WorkItem,
) -> Result<Option<WorkRun>, StoreError> {
    item.active_run_id
        .map(|run_id| load_work_run(connection, run_id))
        .transpose()
}

pub(super) fn load_root_execution(
    connection: &Connection,
    root_execution_id: RootExecutionId,
) -> Result<RootExecution, StoreError> {
    let row: Option<(Vec<u8>, bool)> = connection
        .query_row(
            "SELECT execution_json,
                    root_execution_id = json_extract(execution_json, '$.root_execution_id') AND
                    project_id = json_extract(execution_json, '$.project_id') AND
                    root_id = json_extract(execution_json, '$.root_id') AND
                    generation = json_extract(execution_json, '$.generation') AND
                    state = json_extract(execution_json, '$.state') AND
                    revision = json_extract(execution_json, '$.revision')
             FROM work_root_executions WHERE root_execution_id = ?1",
            [root_execution_id.0.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let (bytes, scalar_bound) = row.ok_or_else(|| {
        StoreError::InvalidWorkProjection(format!(
            "root execution {root_execution_id:?} is missing"
        ))
    })?;
    let execution: RootExecution = serde_json::from_slice(&bytes)?;
    let event = latest_canonical_work_event_on_feed(
        connection,
        "root_work",
        &execution.root_id.0.to_string(),
        "$.root_execution",
    )?;
    if !scalar_bound
        || execution.root_execution_id != root_execution_id
        || event.root_execution.as_ref() != Some(&execution)
    {
        return Err(StoreError::InvalidWorkProjection(format!(
            "root execution {root_execution_id:?} differs from its scalar or canonical event binding"
        )));
    }
    Ok(execution)
}

pub(super) fn active_root_execution(
    connection: &Connection,
    root_id: WorkId,
) -> Result<RootExecution, StoreError> {
    active_root_execution_optional(connection, root_id)?.ok_or_else(|| {
        StoreError::InvalidWorkProjection(format!("root work {root_id:?} has no active execution"))
    })
}

pub(super) fn active_root_execution_optional(
    connection: &Connection,
    root_id: WorkId,
) -> Result<Option<RootExecution>, StoreError> {
    let row: Option<(Vec<u8>, bool)> = connection
        .query_row(
            "SELECT execution_json,
                    root_execution_id = json_extract(execution_json, '$.root_execution_id') AND
                    project_id = json_extract(execution_json, '$.project_id') AND
                    root_id = json_extract(execution_json, '$.root_id') AND
                    generation = json_extract(execution_json, '$.generation') AND
                    state = json_extract(execution_json, '$.state') AND
                    revision = json_extract(execution_json, '$.revision')
             FROM work_root_executions
             WHERE root_id = ?1 AND state = 'active'",
            [root_id.0.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .optional()?;
    let Some((bytes, scalar_bound)) = row else {
        return Ok(None);
    };
    let execution: RootExecution = serde_json::from_slice(&bytes)?;
    let event = latest_canonical_work_event_on_feed(
        connection,
        "root_work",
        &root_id.0.to_string(),
        "$.root_execution",
    )?;
    if !scalar_bound
        || execution.root_id != root_id
        || event.root_execution.as_ref() != Some(&execution)
    {
        return Err(StoreError::InvalidWorkProjection(format!(
            "active root execution for {root_id:?} differs from canonical history"
        )));
    }
    Ok(Some(execution))
}

pub(super) fn load_work_claim_optional(
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
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, bool>(1)?)),
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

pub(in crate::storage) fn load_active_blocker_projections(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<WorkBlocker>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT blocker_json, created_event_hash,
                blocker_id = json_extract(blocker_json, '$.blocker_id') AND
                work_id = json_extract(blocker_json, '$.work_id')
         FROM work_blockers
         WHERE work_id = ?1 AND state = 'active'
         ORDER BY blocker_id",
    )?;
    statement
        .query_map([work_id.0.to_string()], |row| {
            Ok((
                row.get::<_, Vec<u8>>(0)?,
                row.get::<_, String>(1)?,
                row.get::<_, bool>(2)?,
            ))
        })?
        .map(|row| {
            let (bytes, event_hash, scalar_bound) = row?;
            let blocker: WorkBlocker = serde_json::from_slice(&bytes)?;
            let event_hash = ObjectHash::from_stored(event_hash.clone())
                .ok_or(StoreError::InvalidStoredHash(event_hash))?;
            let native_binding =
                native_work_event_optional(connection, &event_hash)?.is_some_and(|event| {
                    event.work_id == work_id
                        && event.blocker.as_ref() == Some(&blocker)
                        && matches!(
                            event.transition,
                            WorkTransition::Blocked { ref blocker_id }
                                if blocker_id == &blocker.blocker_id
                        )
                });
            let restored_binding = restored_record_binds_work(connection, &event_hash, work_id)?;
            if !scalar_bound || blocker.work_id != work_id || !(native_binding || restored_binding)
            {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "active blocker {} differs from its scalar or event binding",
                    blocker.blocker_id
                )));
            }
            Ok(blocker)
        })
        .collect()
}

fn load_active_blocker_catalog_rows(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<WorkBlocker>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT blocker_json,
                blocker_id = json_extract(blocker_json, '$.blocker_id') AND
                work_id = json_extract(blocker_json, '$.work_id')
         FROM work_blockers
         WHERE work_id = ?1 AND state = 'active'
         ORDER BY blocker_id",
    )?;
    statement
        .query_map([work_id.0.to_string()], |row| {
            Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, bool>(1)?))
        })?
        .map(|row| {
            let (bytes, scalar_bound) = row?;
            let blocker: WorkBlocker = serde_json::from_slice(&bytes)?;
            if !scalar_bound || blocker.work_id != work_id {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "active blocker {} differs from its scalar binding",
                    blocker.blocker_id
                )));
            }
            Ok(blocker)
        })
        .collect()
}

fn classified_prerequisite_projections(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<(WorkId, WorkPrerequisiteState)>, StoreError> {
    let prerequisite_ids = load_prerequisite_projection_ids(connection, work_id)?;
    let mut classified = Vec::with_capacity(prerequisite_ids.len());
    for prerequisite_id in prerequisite_ids {
        let prerequisite = load_work_item(connection, prerequisite_id)?;
        classified.push((
            prerequisite_id,
            work_prerequisite_state(connection, &prerequisite)?,
        ));
    }
    Ok(classified)
}

pub(super) fn incomplete_prerequisite_projections(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<WorkId>, StoreError> {
    Ok(classified_prerequisite_projections(connection, work_id)?
        .into_iter()
        .filter_map(|(work_id, state)| {
            (state != WorkPrerequisiteState::Satisfied).then_some(work_id)
        })
        .collect())
}

pub(in crate::storage) fn load_prerequisite_projection_ids(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<WorkId>, StoreError> {
    let prerequisite_ids = {
        let mut statement = connection.prepare(
            "SELECT prerequisite_id, event_hash FROM work_prerequisites
             WHERE work_id = ?1 ORDER BY prerequisite_id",
        )?;
        statement
            .query_map([work_id.0.to_string()], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?))
            })?
            .map(|row| {
                let (prerequisite_id, event_hash) = row?;
                Ok((
                    parse_work_id(&prerequisite_id)?,
                    ObjectHash::from_stored(event_hash.clone())
                        .ok_or(StoreError::InvalidStoredHash(event_hash))?,
                ))
            })
            .collect::<Result<Vec<_>, StoreError>>()?
    };
    let mut bound = Vec::with_capacity(prerequisite_ids.len());
    for (prerequisite_id, event_hash) in prerequisite_ids {
        let event_binds_edge =
            native_work_event_optional(connection, &event_hash)?.is_some_and(|event| {
                event.work_id == work_id
                    && match &event.transition {
                        WorkTransition::Created { prerequisites, .. } => {
                            prerequisites.contains(&prerequisite_id)
                        }
                        WorkTransition::PrerequisiteAdded {
                            prerequisite_id: added,
                            ..
                        } => *added == prerequisite_id,
                        _ => false,
                    }
            }) || restored_record_binds_work(connection, &event_hash, work_id)?;
        if !event_binds_edge {
            return Err(StoreError::InvalidWorkProjection(format!(
                "prerequisite edge {work_id:?}->{prerequisite_id:?} differs from its event binding"
            )));
        }
        bound.push(prerequisite_id);
    }
    Ok(bound)
}

fn classified_prerequisite_catalog_rows(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<(WorkId, WorkPrerequisiteState)>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT edge.prerequisite_id, prerequisite.lifecycle, replacement.lifecycle
         FROM work_prerequisites edge
         LEFT JOIN work_items prerequisite
           ON prerequisite.work_id = edge.prerequisite_id
         LEFT JOIN work_items replacement
           ON replacement.work_id = prerequisite.superseded_by
         WHERE edge.work_id = ?1
         ORDER BY edge.prerequisite_id",
    )?;
    statement
        .query_map([work_id.0.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
            ))
        })?
        .map(|row| {
            let (prerequisite_id, lifecycle, replacement_lifecycle) = row?;
            let prerequisite_id = parse_work_id(&prerequisite_id)?;
            let lifecycle = lifecycle.ok_or_else(|| {
                StoreError::InvalidWorkProjection(format!(
                    "prerequisite {prerequisite_id:?} is missing"
                ))
            })?;
            let state = projected_prerequisite_state(
                &lifecycle,
                replacement_lifecycle.as_deref(),
                prerequisite_id,
            )?;
            Ok((prerequisite_id, state))
        })
        .collect()
}

pub(super) fn bounded_prerequisite_projection_rows(
    connection: &Connection,
    work_id: WorkId,
    limit: usize,
) -> Result<WorkPrerequisitePage, StoreError> {
    let mut statement = connection.prepare(
        "SELECT edge.prerequisite_id, prerequisite.short_ref,
                prerequisite.lifecycle, replacement.lifecycle
         FROM work_prerequisites edge
         LEFT JOIN work_items prerequisite
           ON prerequisite.work_id = edge.prerequisite_id
         LEFT JOIN work_items replacement
           ON replacement.work_id = prerequisite.superseded_by
         WHERE edge.work_id = ?1",
    )?;
    let mut classified = statement
        .query_map([work_id.0.to_string()], |row| {
            Ok((
                row.get::<_, String>(0)?,
                row.get::<_, Option<String>>(1)?,
                row.get::<_, Option<String>>(2)?,
                row.get::<_, Option<String>>(3)?,
            ))
        })?
        .map(|row| {
            let (stored_id, short_ref, lifecycle, replacement_lifecycle) = row?;
            let prerequisite_id = parse_work_id(&stored_id)?;
            let short_ref = short_ref.ok_or_else(|| {
                StoreError::InvalidWorkProjection(format!(
                    "prerequisite {prerequisite_id:?} is missing its catalog row"
                ))
            })?;
            let lifecycle = lifecycle.ok_or_else(|| {
                StoreError::InvalidWorkProjection(format!(
                    "prerequisite {prerequisite_id:?} is missing"
                ))
            })?;
            let state = projected_prerequisite_state(
                &lifecycle,
                replacement_lifecycle.as_deref(),
                prerequisite_id,
            )?;
            Ok((prerequisite_id, short_ref, state))
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    drop(statement);

    classified.sort_by(|left, right| {
        prerequisite_state_rank(left.2)
            .cmp(&prerequisite_state_rank(right.2))
            .then_with(|| left.1.cmp(&right.1))
    });
    let mut totals = [0_usize; 3];
    for (_, _, state) in &classified {
        totals[prerequisite_state_rank(*state)] += 1;
    }
    let selected = classified.into_iter().take(limit).collect::<Vec<_>>();
    let mut selected_counts = [0_usize; 3];
    let mut items = Vec::with_capacity(selected.len());
    for (prerequisite_id, _, state) in selected {
        selected_counts[prerequisite_state_rank(state)] += 1;
        items.push((load_work_item(connection, prerequisite_id)?, state));
    }
    Ok(WorkPrerequisitePage {
        items,
        omitted_by_state: std::array::from_fn(|index| totals[index] - selected_counts[index]),
    })
}

pub(super) const fn prerequisite_state_rank(state: WorkPrerequisiteState) -> usize {
    match state {
        WorkPrerequisiteState::Dead => 0,
        WorkPrerequisiteState::Pending => 1,
        WorkPrerequisiteState::Satisfied => 2,
    }
}

fn work_prerequisite_state(
    connection: &Connection,
    prerequisite: &WorkItem,
) -> Result<WorkPrerequisiteState, StoreError> {
    let replacement_lifecycle = prerequisite
        .superseded_by
        .map(|replacement| load_work_item(connection, replacement))
        .transpose()?
        .map(|replacement| replacement.lifecycle);
    classify_prerequisite_state(
        prerequisite.lifecycle,
        replacement_lifecycle,
        prerequisite.work_id,
    )
}

fn classify_prerequisite_state(
    lifecycle: WorkLifecycle,
    replacement_lifecycle: Option<WorkLifecycle>,
    prerequisite_id: WorkId,
) -> Result<WorkPrerequisiteState, StoreError> {
    match lifecycle {
        WorkLifecycle::Completed => Ok(WorkPrerequisiteState::Satisfied),
        WorkLifecycle::Cancelled => Ok(WorkPrerequisiteState::Dead),
        WorkLifecycle::Open | WorkLifecycle::Proposed => Ok(WorkPrerequisiteState::Pending),
        WorkLifecycle::Superseded => match replacement_lifecycle {
            Some(WorkLifecycle::Completed) => Ok(WorkPrerequisiteState::Satisfied),
            Some(WorkLifecycle::Open | WorkLifecycle::Proposed) => {
                Ok(WorkPrerequisiteState::Pending)
            }
            Some(WorkLifecycle::Cancelled | WorkLifecycle::Superseded) => {
                Ok(WorkPrerequisiteState::Dead)
            }
            None => Err(StoreError::InvalidWorkProjection(format!(
                "superseded prerequisite {prerequisite_id:?} has no replacement projection"
            ))),
        },
    }
}

fn projected_prerequisite_state(
    lifecycle: &str,
    replacement_lifecycle: Option<&str>,
    prerequisite_id: WorkId,
) -> Result<WorkPrerequisiteState, StoreError> {
    let lifecycle = match lifecycle {
        "proposed" => WorkLifecycle::Proposed,
        "open" => WorkLifecycle::Open,
        "completed" => WorkLifecycle::Completed,
        "cancelled" => WorkLifecycle::Cancelled,
        "superseded" => WorkLifecycle::Superseded,
        value => {
            return Err(StoreError::InvalidWorkProjection(format!(
                "prerequisite {prerequisite_id:?} has unknown lifecycle {value:?}"
            )));
        }
    };
    let replacement_lifecycle = replacement_lifecycle
        .map(|value| match value {
            "proposed" => Ok(WorkLifecycle::Proposed),
            "open" => Ok(WorkLifecycle::Open),
            "completed" => Ok(WorkLifecycle::Completed),
            "cancelled" => Ok(WorkLifecycle::Cancelled),
            "superseded" => Ok(WorkLifecycle::Superseded),
            value => Err(StoreError::InvalidWorkProjection(format!(
                "prerequisite {prerequisite_id:?} has replacement with unknown lifecycle {value:?}"
            ))),
        })
        .transpose()?;
    classify_prerequisite_state(lifecycle, replacement_lifecycle, prerequisite_id)
}

fn incomplete_prerequisites(
    connection: &Connection,
    work_id: WorkId,
) -> Result<Vec<WorkId>, StoreError> {
    require_work_item_relation_integrity(connection, work_id)?;
    incomplete_prerequisite_projections(connection, work_id)
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

pub(super) fn feed_parts(feed: &FeedId) -> (&'static str, String) {
    match feed {
        FeedId::Project(project) => ("project", project.0.clone()),
        FeedId::RootWork(root) => ("root_work", root.0.to_string()),
        FeedId::RunExecution(run) => ("run_execution", run.0.to_string()),
    }
}
