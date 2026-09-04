use std::collections::HashSet;

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, params};
use serde::{Serialize, de::DeserializeOwned};

use super::super::{BeginWorkProtocolAttempt, SqliteStore, StoreError};
use super::completion::{feed_head, validated_required_child_waivers};
use super::feeds::{
    load_typed_work_object, require_work_protocol_result_object,
    validate_work_protocol_result_binding,
};
use super::planning::{normalize_text, root_participant_is_accounted, validate_live_claim_on};
use super::query::{
    active_root_execution, bounded_prerequisite_projection_rows, completion_recovery_snapshot_on,
    feed_parts, load_root_execution, load_work_claim_optional, load_work_item,
    load_work_items_query, load_work_run, parse_work_id, parse_work_run_id, resolve_work_ref_on,
};
use super::schema::require_work_schema_version;
use super::{
    CompletionRecoverySnapshot, StageWorkSessionDelivery, WorkPrerequisitePage, WorkProtocolAttempt,
};
use crate::{
    CanonicalObject, ObjectHash,
    domain::{
        FeedId, SessionId, TaskId, WorkClaim, WorkCompletionRecoveryCause, WorkId, WorkItem,
        WorkLifecycle, WorkRun, WorkRunId, WorkSessionState,
    },
};

#[cfg(test)]
mod tests;

struct WorkProtocolAttemptRow {
    request_hash: String,
    basis_hash: Option<String>,
    basis_json: Option<Vec<u8>>,
    result_hash: Option<String>,
    result_json: Option<Vec<u8>>,
}

pub(super) const MAX_PROCESS_DEFAULT_SESSION_RECLAIMS_PER_CREATION: usize = 64;
pub(super) const PROCESS_DEFAULT_SESSION_RECLAMATION_CANDIDATES_SQL: &str = r"
    SELECT stale.session_id
    FROM work_session_state AS stale INDEXED BY work_session_state_retention
    WHERE stale.project_id = ?1
      AND stale.updated_at_ms <= ?2
      AND stale.session_id GLOB ?3
      AND stale.session_id != ?4
      AND stale.tentative_delivery_token IS NULL
      AND NOT EXISTS (
          SELECT 1 FROM session_bindings AS binding
          WHERE binding.session_id = stale.session_id
      )
      AND NOT EXISTS (
          SELECT 1 FROM work_protocol_attempts AS recent
          WHERE recent.project_id = ?1
            AND recent.session_id = stale.session_id
            AND recent.initiated_at_ms > ?2
      )
      AND NOT EXISTS (
          SELECT 1 FROM work_protocol_attempts AS pending
          WHERE pending.project_id = ?1
            AND pending.session_id = stale.session_id
            AND (pending.result_hash IS NULL OR pending.result_json IS NULL)
      )
      AND NOT EXISTS (
          SELECT 1
          FROM work_claims AS claim INDEXED BY work_claims_holder_live
          JOIN work_items AS item ON item.work_id = claim.work_id
          WHERE claim.holder_session_id = stale.session_id
            AND claim.state = 'active'
            AND claim.expires_at_ms > ?5
            AND item.project_id = ?1
      )
      AND NOT EXISTS (
          SELECT 1
          FROM work_handoff_offers AS offer INDEXED BY work_handoff_offer_from_live
          JOIN work_items AS item ON item.work_id = offer.work_id
          WHERE json_extract(offer.offer_json, '$.from') = stale.session_id
            AND offer.state = 'offered'
            AND offer.expires_at_ms > ?5
            AND item.project_id = ?1
      )
      AND NOT EXISTS (
          SELECT 1
          FROM work_handoff_offers AS offer INDEXED BY work_handoff_offer_to_live
          JOIN work_items AS item ON item.work_id = offer.work_id
          WHERE json_extract(offer.offer_json, '$.to') = stale.session_id
            AND offer.state = 'offered'
            AND offer.expires_at_ms > ?5
            AND item.project_id = ?1
      )
    ORDER BY stale.updated_at_ms, stale.session_id
    LIMIT ?6
";

/// Reclaims one bounded page of inactive CLI-generated session projections.
///
/// This runs only inside the transaction that creates another process-default
/// session row. The row being created, every previously bound session,
/// staged deliveries, pending protocol attempts, and live claim or handoff
/// authority are retained fail-closed.
fn reclaim_inactive_process_default_work_sessions_on(
    transaction: &Transaction<'_>,
    project_id: &crate::domain::ProjectId,
    creating_session_id: &SessionId,
    retained_since: DateTime<Utc>,
    now: DateTime<Utc>,
) -> Result<usize, StoreError> {
    let process_default_glob = format!("{}*", super::super::PROCESS_DEFAULT_WORK_SESSION_NAMESPACE);
    let mut statement = transaction.prepare(PROCESS_DEFAULT_SESSION_RECLAMATION_CANDIDATES_SQL)?;
    let candidates = statement
        .query_map(
            params![
                project_id.0,
                retained_since.timestamp_millis(),
                process_default_glob,
                creating_session_id.0,
                now.timestamp_millis(),
                i64::try_from(MAX_PROCESS_DEFAULT_SESSION_RECLAIMS_PER_CREATION).map_err(|_| {
                    StoreError::InvalidWorkProjection(
                        "process-default session reclamation bound overflowed".into(),
                    )
                })?
            ],
            |row| row.get::<_, String>(0),
        )?
        .collect::<Result<Vec<_>, _>>()?;
    drop(statement);

    let mut reclaimed = 0;
    for session_id in candidates {
        transaction.execute(
            "DELETE FROM work_protocol_attempts
             WHERE project_id = ?1 AND session_id = ?2",
            params![project_id.0, session_id],
        )?;
        transaction.execute(
            "DELETE FROM work_session_state
             WHERE project_id = ?1 AND session_id = ?2",
            params![project_id.0, session_id],
        )?;
        reclaimed += 1;
    }
    Ok(reclaimed)
}

pub(super) fn begin_work_protocol_attempt_on<T: Serialize, B: Serialize>(
    connection: &Connection,
    request: &BeginWorkProtocolAttempt<'_, T, B>,
) -> Result<WorkProtocolAttempt, StoreError> {
    let project_id = request.project_id;
    let session_id = request.session_id;
    let operation = request.operation;
    let idempotency_key = normalize_text(request.idempotency_key, "work idempotency key")?;
    let request_object = CanonicalObject::freeze(request.intent)?;
    let basis_object = CanonicalObject::freeze(request.basis)?;
    connection.execute(
        "INSERT INTO work_protocol_attempts (
             project_id, session_id, operation, idempotency_key,
             request_hash, basis_hash, basis_json, initiated_at_ms,
             result_hash, result_json
          ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, NULL, NULL)
         ON CONFLICT(project_id, session_id, operation, idempotency_key)
         DO NOTHING",
        params![
            project_id.0,
            session_id.0,
            operation,
            idempotency_key,
            request_object.hash().as_str(),
            basis_object.hash().as_str(),
            basis_object.bytes(),
            request.now.timestamp_millis()
        ],
    )?;
    let stored = connection.query_row(
        "SELECT request_hash, basis_hash, basis_json, result_hash, result_json
         FROM work_protocol_attempts
         WHERE project_id = ?1 AND session_id = ?2
           AND operation = ?3 AND idempotency_key = ?4",
        params![project_id.0, session_id.0, operation, idempotency_key],
        |row| {
            Ok(WorkProtocolAttemptRow {
                request_hash: row.get(0)?,
                basis_hash: row.get(1)?,
                basis_json: row.get(2)?,
                result_hash: row.get(3)?,
                result_json: row.get(4)?,
            })
        },
    )?;
    if stored.request_hash != request_object.hash().as_str() {
        return Err(StoreError::WorkOperationIdempotencyConflict {
            operation: operation.to_owned(),
            key: idempotency_key,
        });
    }
    let basis_matches = stored.basis_hash.as_deref() == Some(basis_object.hash().as_str())
        && stored.basis_json.as_deref() == Some(basis_object.bytes());
    let stored_basis = match (&stored.basis_hash, &stored.basis_json) {
        (Some(stored_hash), Some(bytes)) => {
            let hash = ObjectHash::from_stored(stored_hash.clone())
                .ok_or_else(|| StoreError::InvalidStoredHash(stored_hash.clone()))?;
            Some(CanonicalObject::verify(&hash, bytes.clone())?.decode()?)
        }
        (_, None) if stored.result_hash.is_some() && stored.result_json.is_some() => None,
        _ => {
            return Err(StoreError::InvalidWorkProjection(
                "pending work-protocol attempt has no verified durable basis".into(),
            ));
        }
    };
    let result = match (stored.result_hash, stored.result_json) {
        (None, None) => None,
        (Some(stored_hash), Some(bytes)) => {
            let hash = ObjectHash::from_stored(stored_hash.clone())
                .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
            let value = load_typed_work_object::<serde_json::Value>(
                connection,
                &hash,
                "work_protocol_result",
            )?;
            let object = CanonicalObject::freeze(&value)?;
            if object.hash() != &hash || object.bytes() != bytes {
                return Err(StoreError::InvalidWorkProjection(
                    "work-protocol replay bytes differ from their canonical result".into(),
                ));
            }
            validate_work_protocol_result_binding(connection, &project_id.0, operation, &value)?;
            Some(value)
        }
        _ => {
            return Err(StoreError::InvalidWorkProjection(
                "work-protocol result hash and bytes must be present together".into(),
            ));
        }
    };
    Ok(WorkProtocolAttempt {
        result,
        basis_matches,
        basis: stored_basis,
    })
}

impl SqliteStore {
    pub(super) fn begin_work_mutation(&mut self) -> Result<Transaction<'_>, StoreError> {
        let expected_version = self.work_schema_version;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_work_schema_version(&transaction, expected_version)?;
        Ok(transaction)
    }

    /// Returns one current work projection.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the item is absent, corrupt, or unreadable.
    pub fn get_work_item(&self, work_id: WorkId) -> Result<WorkItem, StoreError> {
        load_work_item(&self.connection, work_id)
    }

    /// Returns the active or historical run by stable identity.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the run is absent, corrupt, or unreadable.
    pub fn get_work_run(&self, run_id: WorkRunId) -> Result<WorkRun, StoreError> {
        load_work_run(&self.connection, run_id)
    }

    /// Returns the newest run generation for work, including a terminal run.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when a persisted run identity or projection is invalid.
    pub fn latest_work_run(&self, work_id: WorkId) -> Result<Option<WorkRun>, StoreError> {
        let run_id: Option<String> = self
            .connection
            .query_row(
                "SELECT run_id FROM work_runs WHERE work_id = ?1
                 ORDER BY generation DESC LIMIT 1",
                [work_id.0.to_string()],
                |row| row.get(0),
            )
            .optional()?;
        run_id
            .map(|value| parse_work_run_id(&value).and_then(|run_id| self.get_work_run(run_id)))
            .transpose()
    }

    /// Resolves a model-facing short reference or full UUID inside one project.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the reference is empty, absent, ambiguous,
    /// or its stored projection is invalid.
    pub fn resolve_work_ref(
        &self,
        project_id: &crate::domain::ProjectId,
        work_ref: &str,
    ) -> Result<WorkItem, StoreError> {
        resolve_work_ref_on(&self.connection, project_id, work_ref)
    }

    /// Builds current completion-recovery guidance without persisting a
    /// replayable refusal.
    pub(crate) fn work_completion_recovery(
        &self,
        expected_work: &WorkItem,
        claim: &WorkClaim,
        now: DateTime<Utc>,
        cause: &WorkCompletionRecoveryCause,
    ) -> Result<CompletionRecoverySnapshot, StoreError> {
        debug_assert!(self.connection.is_autocommit());
        let transaction = self.connection.unchecked_transaction()?;
        let (work, _, _) = validate_live_claim_on(
            &transaction,
            expected_work.work_id,
            claim.run_id,
            expected_work.revision,
            &claim.holder,
            claim.claim_id,
            claim.fence,
            now,
            false,
        )?;
        let WorkCompletionRecoveryCause::MissingAcceptance { criterion } = cause else {
            return Err(StoreError::InvalidWorkProjection(
                "preflight completion recovery accepts only missing acceptance".into(),
            ));
        };
        if !work.acceptance.contains(criterion) {
            return Err(StoreError::InvalidWorkProjection(
                "preflight completion recovery criterion is absent from the bound work revision"
                    .into(),
            ));
        }
        let recovery =
            completion_recovery_snapshot_on(&transaction, &work, claim.run_id, cause.clone())?;
        transaction.commit()?;
        Ok(recovery)
    }

    /// Creates the operational row for one validated process-default session.
    ///
    /// Only the creator runs one bounded reclamation page. Existing rows take
    /// the primary-key read path and never scan the retention index again.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the row or reclamation projection cannot be
    /// read or written atomically.
    pub(crate) fn initialize_process_default_work_session(
        &mut self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        now: DateTime<Utc>,
    ) -> Result<bool, StoreError> {
        let exists = self.connection.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM work_session_state
                 WHERE project_id = ?1 AND session_id = ?2
             )",
            params![project_id.0, session_id.0],
            |row| row.get::<_, i64>(0),
        )? == 1;
        if exists {
            return Ok(false);
        }

        let retained_since = now
            .checked_sub_signed(chrono::TimeDelta::seconds(
                super::super::PROCESS_DEFAULT_WORK_SESSION_RETENTION_SECONDS,
            ))
            .ok_or_else(|| {
                StoreError::InvalidWorkProjection(
                    "process-default session retention boundary overflowed".into(),
                )
            })?;
        let transaction = self.begin_work_mutation()?;
        let exists = transaction.query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM work_session_state
                 WHERE project_id = ?1 AND session_id = ?2
             )",
            params![project_id.0, session_id.0],
            |row| row.get::<_, i64>(0),
        )? == 1;
        if exists {
            transaction.commit()?;
            return Ok(false);
        }
        reclaim_inactive_process_default_work_sessions_on(
            &transaction,
            project_id,
            session_id,
            retained_since,
            now,
        )?;
        transaction.execute(
            "INSERT INTO work_session_state (
                 project_id, session_id, focused_work_id, project_cursor, updated_at_ms
             ) VALUES (?1, ?2, NULL, 0, ?3)",
            params![project_id.0, session_id.0, now.timestamp_millis()],
        )?;
        transaction.commit()?;
        Ok(true)
    }

    /// Returns ambient navigation state, defaulting to no focus and cursor zero.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the projection cannot be read.
    pub fn work_session_state(
        &self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        now: DateTime<Utc>,
    ) -> Result<WorkSessionState, StoreError> {
        let row = self
            .connection
            .query_row(
                "SELECT focused_work_id, project_cursor,
                        tentative_project_cursor, tentative_delivery_token,
                        tentative_delivery_payload_hash,
                        tentative_delivery_payload, updated_at_ms
                 FROM work_session_state WHERE project_id = ?1 AND session_id = ?2",
                params![project_id.0, session_id.0],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, i64>(1)?,
                        row.get::<_, Option<i64>>(2)?,
                        row.get::<_, Option<String>>(3)?,
                        row.get::<_, Option<String>>(4)?,
                        row.get::<_, Option<Vec<u8>>>(5)?,
                        row.get::<_, i64>(6)?,
                    ))
                },
            )
            .optional()?;
        let Some((
            focused_work_id,
            project_cursor,
            tentative_project_cursor,
            tentative_delivery_token,
            tentative_delivery_payload_hash,
            tentative_delivery_payload,
            updated_at_ms,
        )) = row
        else {
            return Ok(WorkSessionState {
                project_id: project_id.clone(),
                session_id: session_id.clone(),
                focused_work_id: None,
                project_cursor: 0,
                tentative_project_cursor: None,
                tentative_delivery_token: None,
                updated_at: now,
            });
        };
        let pending_fields = [
            tentative_project_cursor.is_some(),
            tentative_delivery_token.is_some(),
            tentative_delivery_payload_hash.is_some(),
            tentative_delivery_payload.is_some(),
        ];
        if pending_fields
            .iter()
            .any(|present| *present != pending_fields[0])
        {
            return Err(StoreError::InvalidWorkProjection(
                "staged work delivery cursor, token, payload hash, and payload must be present together"
                    .into(),
            ));
        }
        if let (Some(hash), Some(payload)) =
            (tentative_delivery_payload_hash, tentative_delivery_payload)
        {
            let hash =
                ObjectHash::from_stored(hash.clone()).ok_or(StoreError::InvalidStoredHash(hash))?;
            CanonicalObject::verify(&hash, payload)?;
        }
        Ok(WorkSessionState {
            project_id: project_id.clone(),
            session_id: session_id.clone(),
            focused_work_id: focused_work_id
                .map(|value| parse_work_id(&value))
                .transpose()?,
            project_cursor,
            tentative_project_cursor,
            tentative_delivery_token,
            updated_at: DateTime::from_timestamp_millis(updated_at_ms).ok_or_else(|| {
                StoreError::InvalidWorkProjection(format!(
                    "invalid work-session timestamp {updated_at_ms}"
                ))
            })?,
        })
    }

    /// Returns direct required children that can be waived from this parent's
    /// completion barrier under the local project's binding-only policy.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the parent/root projections or canonical
    /// waiver history are invalid, or when candidate children cannot be read.
    pub fn waivable_required_children(
        &self,
        parent: &WorkItem,
        limit: usize,
    ) -> Result<Vec<WorkItem>, StoreError> {
        if limit == 0 || parent.lifecycle != WorkLifecycle::Open {
            return Ok(Vec::new());
        }
        let mut statement = self.connection.prepare(
            "SELECT work_id FROM work_items
             WHERE parent_id = ?1
               AND child_requirement = 'required'
               AND lifecycle IN ('cancelled', 'superseded')
             ORDER BY created_at_ms, work_id",
        )?;
        let child_ids = statement
            .query_map([parent.work_id.0.to_string()], |row| {
                row.get::<_, String>(0)
            })?
            .collect::<Result<Vec<_>, _>>()?;
        drop(statement);
        if child_ids.is_empty() {
            return Ok(Vec::new());
        }
        let root_execution = active_root_execution(&self.connection, parent.root_id)?;
        let waived =
            validated_required_child_waivers(&self.connection, parent.work_id, &root_execution)?
                .into_iter()
                .map(|waiver| waiver.work_id)
                .collect::<HashSet<_>>();

        let mut eligible = Vec::new();
        for stored_id in child_ids {
            let child_id = parse_work_id(&stored_id)?;
            if waived.contains(&child_id) {
                continue;
            }
            let child = load_work_item(&self.connection, child_id)?;
            eligible.push(child);
            if eligible.len() == limit {
                break;
            }
        }
        Ok(eligible)
    }

    /// Starts or replays one caller-visible ambient protocol intent before any
    /// mutable focus, claim, revision, or handoff state is inferred.
    pub(crate) fn begin_work_protocol_attempt<T: Serialize, B: Serialize>(
        &mut self,
        request: &BeginWorkProtocolAttempt<'_, T, B>,
    ) -> Result<WorkProtocolAttempt, StoreError> {
        let transaction = self.begin_work_mutation()?;
        let attempt = begin_work_protocol_attempt_on(&transaction, request)?;
        transaction.commit()?;
        Ok(attempt)
    }

    /// Persists the caller-visible result for exact lost-response replay.
    pub(crate) fn finish_work_protocol_attempt<T: Serialize>(
        &mut self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        operation: &str,
        idempotency_key: &str,
        result: &T,
    ) -> Result<(), StoreError> {
        let compact_result = require_work_protocol_result_object(serde_json::to_value(result)?)?;
        let transaction = self.begin_work_mutation()?;
        validate_work_protocol_result_binding(
            &transaction,
            &project_id.0,
            operation,
            &compact_result,
        )?;
        let result_object = CanonicalObject::freeze(&compact_result)?;
        Self::insert_object(&transaction, "work_protocol_result", &result_object)?;
        let changed = transaction.execute(
            "UPDATE work_protocol_attempts
             SET basis_json = NULL, result_hash = ?5, result_json = ?6
             WHERE project_id = ?1 AND session_id = ?2
               AND operation = ?3 AND idempotency_key = ?4
               AND result_hash IS NULL AND result_json IS NULL",
            params![
                project_id.0,
                session_id.0,
                operation,
                idempotency_key,
                result_object.hash().as_str(),
                result_object.bytes()
            ],
        )?;
        if changed == 0 {
            let stored = transaction
                .query_row(
                    "SELECT result_hash, result_json FROM work_protocol_attempts
                     WHERE project_id = ?1 AND session_id = ?2
                       AND operation = ?3 AND idempotency_key = ?4",
                    params![project_id.0, session_id.0, operation, idempotency_key],
                    |row| {
                        Ok((
                            row.get::<_, Option<String>>(0)?,
                            row.get::<_, Option<Vec<u8>>>(1)?,
                        ))
                    },
                )
                .optional()?;
            if stored
                != Some((
                    Some(result_object.hash().as_str().to_owned()),
                    Some(result_object.bytes().to_vec()),
                ))
            {
                return Err(StoreError::InvalidWorkProjection(
                    "work-protocol result completion conflicted with its durable attempt".into(),
                ));
            }
        } else if changed != 1 {
            return Err(StoreError::InvalidWorkProjection(
                "work-protocol result updated more than one durable attempt".into(),
            ));
        }
        transaction.commit()?;
        Ok(())
    }

    /// Refreshes one pending completion attempt to a newer live basis without
    /// losing its caller-key, request, or target binding. The caller first
    /// verifies that both bases name the same work item.
    pub(crate) fn refresh_pending_work_protocol_attempt_basis<B: Serialize>(
        &mut self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        operation: &str,
        idempotency_key: &str,
        expected_basis: &serde_json::Value,
        current_basis: &B,
    ) -> Result<(), StoreError> {
        let expected = CanonicalObject::freeze(expected_basis)?;
        let current = CanonicalObject::freeze(current_basis)?;
        let transaction = self.begin_work_mutation()?;
        let changed = transaction.execute(
            "UPDATE work_protocol_attempts
             SET basis_hash = ?7, basis_json = ?8
             WHERE project_id = ?1 AND session_id = ?2
               AND operation = ?3 AND idempotency_key = ?4
               AND basis_hash = ?5 AND basis_json = ?6
               AND result_hash IS NULL AND result_json IS NULL",
            params![
                project_id.0,
                session_id.0,
                operation,
                idempotency_key,
                expected.hash().as_str(),
                expected.bytes(),
                current.hash().as_str(),
                current.bytes()
            ],
        )?;
        if changed == 0 {
            let matches_current = transaction
                .query_row(
                    "SELECT basis_hash, basis_json
                     FROM work_protocol_attempts
                     WHERE project_id = ?1 AND session_id = ?2
                       AND operation = ?3 AND idempotency_key = ?4
                       AND result_hash IS NULL AND result_json IS NULL",
                    params![project_id.0, session_id.0, operation, idempotency_key],
                    |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
                )
                .optional()?
                .is_some_and(|(hash, bytes)| {
                    hash == current.hash().as_str() && bytes == current.bytes()
                });
            if !matches_current {
                return Err(StoreError::WorkOperationIdempotencyConflict {
                    operation: operation.to_owned(),
                    key: idempotency_key.to_owned(),
                });
            }
        } else if changed != 1 {
            return Err(StoreError::InvalidWorkProjection(
                "pending work-protocol basis refresh updated more than one attempt".into(),
            ));
        }
        transaction.commit()?;
        Ok(())
    }

    /// Returns a committed core-operation result after an interrupted ambient
    /// wrapper. The already-matched protocol intent makes this lookup safe.
    pub(crate) fn work_operation_result_value(
        &self,
        operation: &str,
        idempotency_key: &str,
    ) -> Result<Option<serde_json::Value>, StoreError> {
        let bytes: Option<Vec<u8>> = self
            .connection
            .query_row(
                "SELECT result_json FROM work_operation_results
                 WHERE operation = ?1 AND idempotency_key = ?2",
                params![operation, idempotency_key],
                |row| row.get(0),
            )
            .optional()?;
        bytes
            .map(|value| serde_json::from_slice(&value).map_err(StoreError::from))
            .transpose()
    }

    /// Loads the canonical object named by a committed hash-valued core
    /// operation result. The caller still replays the operation so its stored
    /// request hash verifies the reconstructed request exactly.
    pub(crate) fn work_operation_result_object<T: DeserializeOwned>(
        &self,
        operation: &str,
        idempotency_key: &str,
        object_kind: &str,
    ) -> Result<Option<T>, StoreError> {
        let Some(value) = self.work_operation_result_value(operation, idempotency_key)? else {
            return Ok(None);
        };
        let hash: ObjectHash = serde_json::from_value(value)?;
        load_typed_work_object(&self.connection, &hash, object_kind).map(Some)
    }

    /// Changes ambient focus without claiming or releasing work.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the target is outside the project or the
    /// mutable navigation projection cannot be written.
    pub fn focus_work_session(
        &mut self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        work_id: WorkId,
        now: DateTime<Utc>,
    ) -> Result<WorkSessionState, StoreError> {
        let transaction = self.begin_work_mutation()?;
        let item = load_work_item(&transaction, work_id)?;
        if item.project_id != *project_id {
            return Err(StoreError::InvalidWork(
                "focused work must belong to the bound project".into(),
            ));
        }
        // A staged change page was projected under the previous focus. A
        // different focus discards it so the next call recomputes the same
        // interval under the new visibility basis; nothing is confirmed here.
        transaction.execute(
            "UPDATE work_session_state SET
                 tentative_project_cursor = NULL,
                 tentative_delivery_token = NULL,
                 tentative_delivery_payload_hash = NULL,
                 tentative_delivery_payload = NULL
             WHERE project_id = ?1 AND session_id = ?2
               AND focused_work_id IS NOT ?3",
            params![project_id.0, session_id.0, work_id.0.to_string()],
        )?;
        transaction.execute(
            "INSERT INTO work_session_state (
                 project_id, session_id, focused_work_id, project_cursor, updated_at_ms
             ) VALUES (?1, ?2, ?3, 0, ?4)
             ON CONFLICT(project_id, session_id) DO UPDATE SET
                 focused_work_id = excluded.focused_work_id,
                 updated_at_ms = excluded.updated_at_ms",
            params![
                project_id.0,
                session_id.0,
                work_id.0.to_string(),
                now.timestamp_millis()
            ],
        )?;
        transaction.commit()?;
        self.work_session_state(project_id, session_id, now)
    }

    /// Stages one replayable delivery without acknowledging it.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the cursor is invalid or cannot be stored.
    pub(crate) fn stage_work_session_delivery(
        &mut self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        request: StageWorkSessionDelivery<'_>,
    ) -> Result<Option<WorkSessionState>, StoreError> {
        let StageWorkSessionDelivery {
            expected_confirmed_through,
            expected_focused_work_id,
            expected_bound_task_id,
            delivered_through,
            delivered_entries,
            delivery_payload,
            now,
        } = request;
        if expected_confirmed_through < 0 || delivered_through < 0 {
            return Err(StoreError::InvalidWork(
                "work delivery cursor must not be negative".into(),
            ));
        }
        let transaction = self.begin_work_mutation()?;
        transaction.execute(
            "INSERT INTO work_session_state (
                 project_id, session_id, focused_work_id, project_cursor,
                 updated_at_ms
              ) VALUES (?1, ?2, NULL, 0, ?3)
              ON CONFLICT(project_id, session_id) DO NOTHING",
            params![project_id.0, session_id.0, now.timestamp_millis()],
        )?;
        let (focused_work_id, confirmed, pending): (Option<String>, i64, Option<i64>) = transaction
            .query_row(
                "SELECT focused_work_id, project_cursor, tentative_project_cursor
             FROM work_session_state WHERE project_id = ?1 AND session_id = ?2",
                params![project_id.0, session_id.0],
                |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
            )?;
        let focused_work_id = focused_work_id
            .map(|value| parse_work_id(&value))
            .transpose()?;
        let bound_task_id = transaction
            .query_row(
                "SELECT b.task_id FROM session_bindings b JOIN tasks t
                   ON t.task_id = b.task_id
                 WHERE b.session_id = ?1 AND t.project_id = ?2",
                params![session_id.0, project_id.0],
                |row| row.get::<_, String>(0),
            )
            .optional()?
            .map(|stored| {
                uuid::Uuid::parse_str(&stored)
                    .map(TaskId)
                    .map_err(|error| StoreError::InvalidTaskProjection(error.to_string()))
            })
            .transpose()?;
        if confirmed != expected_confirmed_through
            || focused_work_id != expected_focused_work_id
            || bound_task_id != expected_bound_task_id
            || pending.is_some()
        {
            return Ok(None);
        }
        let feed = FeedId::Project(project_id.clone());
        let head = feed_head(&transaction, &feed)?;
        if delivered_through < confirmed || delivered_through > head {
            return Err(StoreError::InvalidWork(format!(
                "work delivery cursor {delivered_through} must be within [{confirmed}, {head}]"
            )));
        }
        if delivered_through > confirmed {
            let expected_count = usize::try_from(delivered_through - confirmed).map_err(|_| {
                StoreError::InvalidWork("work delivery interval size overflowed".into())
            })?;
            if delivered_entries.len() != expected_count
                || delivered_entries.iter().enumerate().any(|(offset, entry)| {
                    entry.position.feed != feed
                        || i64::try_from(offset)
                            .ok()
                            .is_none_or(|offset| entry.position.position != confirmed + offset + 1)
                })
            {
                return Err(StoreError::InvalidWork(
                    "work delivery payload does not bind the exact dense interval".into(),
                ));
            }
            let (feed_kind, feed_id) = feed_parts(&feed);
            let stored_entries = transaction
                .prepare(
                    "SELECT position, object_kind, object_hash
                 FROM work_feed_entries
                 WHERE feed_kind = ?1 AND feed_id = ?2
                   AND position > ?3 AND position <= ?4
                 ORDER BY position",
                )?
                .query_map(
                    params![feed_kind, feed_id, confirmed, delivered_through],
                    |row| {
                        Ok((
                            row.get::<_, i64>(0)?,
                            row.get::<_, String>(1)?,
                            row.get::<_, String>(2)?,
                        ))
                    },
                )?
                .collect::<Result<Vec<_>, _>>()?;
            let stored_matches = stored_entries.len() == expected_count
                && stored_entries.iter().zip(delivered_entries).all(
                    |((position, object_kind, object_hash), delivered)| {
                        *position == delivered.position.position
                            && object_kind == &delivered.object_kind
                            && object_hash == delivered.object_hash.as_str()
                    },
                );
            if !stored_matches {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "work delivery interval ({confirmed}, {delivered_through}] is not dense"
                )));
            }
        } else if !delivered_entries.is_empty() {
            return Err(StoreError::InvalidWork(
                "an empty work delivery interval cannot bind feed entries".into(),
            ));
        }
        let new_delivery_token = uuid::Uuid::new_v4().to_string();
        let tentative = (delivered_through > confirmed).then_some(delivered_through);
        let tentative_delivery_token = tentative.map(|_| new_delivery_token);
        let changed = transaction.execute(
            "UPDATE work_session_state SET
                 tentative_project_cursor = ?3,
                 tentative_delivery_token = ?4,
                 tentative_delivery_payload_hash = ?5,
                 tentative_delivery_payload = ?6,
                 updated_at_ms = ?7
             WHERE project_id = ?1 AND session_id = ?2
               AND project_cursor = ?8
               AND tentative_project_cursor IS NULL
               AND focused_work_id IS ?9",
            params![
                project_id.0,
                session_id.0,
                tentative,
                tentative_delivery_token.as_deref(),
                tentative.map(|_| delivery_payload.hash().as_str()),
                tentative.map(|_| delivery_payload.bytes()),
                now.timestamp_millis(),
                expected_confirmed_through,
                expected_focused_work_id.map(|work_id| work_id.0.to_string())
            ],
        )?;
        if changed != 1 {
            return Ok(None);
        }
        let staged = WorkSessionState {
            project_id: project_id.clone(),
            session_id: session_id.clone(),
            focused_work_id,
            project_cursor: confirmed,
            tentative_project_cursor: tentative,
            tentative_delivery_token,
            updated_at: now,
        };
        transaction.commit()?;
        Ok(Some(staged))
    }

    /// Loads the exact canonical agent page bound to a pending delivery.
    pub(crate) fn staged_work_session_delivery_payload(
        &self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
    ) -> Result<Option<CanonicalObject>, StoreError> {
        let row = self
            .connection
            .query_row(
                "SELECT tentative_delivery_payload_hash, tentative_delivery_payload
                 FROM work_session_state
                 WHERE project_id = ?1 AND session_id = ?2",
                params![project_id.0, session_id.0],
                |row| {
                    Ok((
                        row.get::<_, Option<String>>(0)?,
                        row.get::<_, Option<Vec<u8>>>(1)?,
                    ))
                },
            )
            .optional()?;
        match row {
            None | Some((None, None)) => Ok(None),
            Some((Some(hash), Some(bytes))) => {
                let hash = ObjectHash::from_stored(hash.clone())
                    .ok_or(StoreError::InvalidStoredHash(hash))?;
                Ok(Some(CanonicalObject::verify(&hash, bytes)?))
            }
            Some(_) => Err(StoreError::InvalidWorkProjection(
                "staged work delivery payload hash and bytes must be present together".into(),
            )),
        }
    }

    /// Acknowledges the exact staged delivery using compare-and-swap semantics.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the acknowledgement is stale, was never
    /// staged, or cannot be persisted.
    pub fn acknowledge_work_session_delivery(
        &mut self,
        project_id: &crate::domain::ProjectId,
        session_id: &SessionId,
        through: i64,
        delivery_token: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<WorkSessionState, StoreError> {
        if through < 0 {
            return Err(StoreError::InvalidWork(
                "work acknowledgement cursor must not be negative".into(),
            ));
        }
        let transaction = self.begin_work_mutation()?;
        let changed = transaction.execute(
            "UPDATE work_session_state SET
                 project_cursor = ?3,
                 tentative_project_cursor = NULL,
                 tentative_delivery_token = NULL,
                 tentative_delivery_payload_hash = NULL,
                 tentative_delivery_payload = NULL,
                 updated_at_ms = ?5
             WHERE project_id = ?1 AND session_id = ?2
               AND tentative_project_cursor = ?3
               AND tentative_delivery_token = ?4",
            params![
                project_id.0,
                session_id.0,
                through,
                delivery_token,
                now.timestamp_millis()
            ],
        )?;
        transaction.commit()?;
        let state = self.work_session_state(project_id, session_id, now)?;
        if changed == 0 {
            if state.project_cursor == through {
                return Ok(state);
            }
            return Err(StoreError::InvalidWork(
                "work delivery acknowledgement does not match the pending page; replay it with work_next (changes selected, no acknowledgement) and acknowledge the delivered_through and delivery_token you receive"
                    .into(),
            ));
        }
        Ok(state)
    }

    /// Lists direct children in stable creation order.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when a child projection cannot be decoded.
    pub fn work_children(&self, work_id: WorkId) -> Result<Vec<WorkItem>, StoreError> {
        load_work_items_query(
            &self.connection,
            "SELECT item_json FROM work_items
             WHERE parent_id = ?1 ORDER BY created_at_ms, work_id",
            work_id,
        )
    }

    /// Lists explicit prerequisites in stable id order.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when a prerequisite projection cannot be decoded.
    pub fn work_prerequisites(&self, work_id: WorkId) -> Result<Vec<WorkItem>, StoreError> {
        load_work_items_query(
            &self.connection,
            "SELECT prerequisite.item_json
             FROM work_prerequisites edge
             JOIN work_items prerequisite ON prerequisite.work_id = edge.prerequisite_id
             WHERE edge.work_id = ?1 ORDER BY prerequisite.work_id",
            work_id,
        )
    }

    pub(crate) fn work_prerequisites_with_state(
        &self,
        work_id: WorkId,
        limit: usize,
    ) -> Result<WorkPrerequisitePage, StoreError> {
        bounded_prerequisite_projection_rows(&self.connection, work_id, limit)
    }

    /// Returns the projected claim for the active run, or the latest
    /// historical run after a terminal transition clears the active run.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when work or claim projections are invalid.
    pub fn current_work_claim(&self, work_id: WorkId) -> Result<Option<WorkClaim>, StoreError> {
        let item = load_work_item(&self.connection, work_id)?;
        self.current_work_claim_for_item(&item)
    }

    pub(crate) fn current_work_claim_for_item(
        &self,
        item: &WorkItem,
    ) -> Result<Option<WorkClaim>, StoreError> {
        item.active_run_id
            .or(self.latest_work_run(item.work_id)?.map(|run| run.run_id))
            .map(|run_id| load_work_claim_optional(&self.connection, run_id))
            .transpose()
            .map(Option::flatten)
    }

    /// Lists the work this session holds under a live claim, with each claim's
    /// expiry, from the claim projection alone.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when a stored work id is invalid.
    pub fn work_held_by(
        &self,
        holder: &SessionId,
        now: DateTime<Utc>,
    ) -> Result<Vec<(WorkId, DateTime<Utc>)>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT work_id, expires_at_ms FROM work_claims
             WHERE holder_session_id = ?1 AND state = 'active' AND expires_at_ms > ?2
             ORDER BY expires_at_ms, work_id",
        )?;
        let rows = statement.query_map(params![holder.0, now.timestamp_millis()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?;
        let mut held = Vec::new();
        for row in rows {
            let (work_id, expires_at_ms) = row?;
            let work_id = uuid::Uuid::parse_str(&work_id).map(WorkId).map_err(|_| {
                StoreError::InvalidWorkProjection(format!("claim has invalid work id {work_id}"))
            })?;
            let expires_at =
                DateTime::<Utc>::from_timestamp_millis(expires_at_ms).ok_or_else(|| {
                    StoreError::InvalidWorkProjection("claim has an invalid expiry".into())
                })?;
            held.push((work_id, expires_at));
        }
        Ok(held)
    }

    /// Lists every live project claim needed to render compact catalog rows.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when a projected work id or expiry is invalid.
    pub fn live_work_claims(
        &self,
        project_id: &crate::domain::ProjectId,
        now: DateTime<Utc>,
    ) -> Result<Vec<(WorkId, SessionId, DateTime<Utc>)>, StoreError> {
        let mut statement = self.connection.prepare(
            "SELECT claim.work_id, claim.holder_session_id, claim.expires_at_ms
             FROM work_claims claim
             JOIN work_items item ON item.work_id = claim.work_id
             WHERE item.project_id = ?1
               AND claim.state = 'active' AND claim.expires_at_ms > ?2
             ORDER BY claim.work_id",
        )?;
        let rows = statement.query_map(
            params![project_id.0.as_str(), now.timestamp_millis()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, i64>(2)?,
                ))
            },
        )?;
        let mut claims = Vec::new();
        for row in rows {
            let (work_id, holder, expires_at_ms) = row?;
            let work_id = uuid::Uuid::parse_str(&work_id).map(WorkId).map_err(|_| {
                StoreError::InvalidWorkProjection(format!("claim has invalid work id {work_id}"))
            })?;
            let expires_at =
                DateTime::<Utc>::from_timestamp_millis(expires_at_ms).ok_or_else(|| {
                    StoreError::InvalidWorkProjection("claim has an invalid expiry".into())
                })?;
            claims.push((work_id, SessionId(holder), expires_at));
        }
        Ok(claims)
    }

    /// Reports whether taking the current run from another prior holder needs
    /// an attributed claim-recovery waiver.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the work, run, claim, or root projection is
    /// invalid.
    pub fn work_claim_recovery_required(
        &self,
        work_id: WorkId,
        claimant: &SessionId,
    ) -> Result<bool, StoreError> {
        let item = load_work_item(&self.connection, work_id)?;
        let claim = self.current_work_claim_for_item(&item)?;
        self.work_claim_recovery_required_for_item(&item, claim.as_ref(), claimant)
    }

    pub(crate) fn work_claim_recovery_required_for_item(
        &self,
        item: &WorkItem,
        claim: Option<&WorkClaim>,
        claimant: &SessionId,
    ) -> Result<bool, StoreError> {
        if claim.is_none_or(|claim| claim.holder == *claimant) {
            return Ok(false);
        }
        let Some(run_id) = item.active_run_id else {
            return Ok(false);
        };
        let run = load_work_run(&self.connection, run_id)?;
        let execution = load_root_execution(&self.connection, run.root_execution_id)?;
        Ok(claim.is_some_and(|claim| {
            claim.run_id == run_id
                && claim.holder != *claimant
                && !root_participant_is_accounted(&execution, &claim.holder)
        }))
    }
}
