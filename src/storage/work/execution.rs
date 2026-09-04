use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::Serialize;

use super::super::{
    BeginGateWorkProtocolAttempt, BeginWorkProtocolAttempt, SqliteStore, StoreError,
};
use super::completion::feed_head;
use super::feeds::{
    append_to_work_feeds, append_work_event, checkpoint_feed_end, expire_handoff_offers,
    inspect_work_request, load_handoff_offer_projection, load_typed_work_object, replay_operation,
    request_object, run_feed_position_for_object_on,
};
use super::integrity::{expected_environment_projection, expected_verification_projection};
use super::planning::{
    add_root_contribution, assert_actor_session, assert_revision, claim_expiry,
    expect_root_contributor, normalize_strings, normalize_text, persist_claim,
    persist_operation_result, persist_root_execution, persist_work_item, persist_work_run,
    renew_holder_claim, root_participant_is_accounted, unique_hashes,
    validate_live_claim_for_item_on, validate_live_claim_on, waive_root_contributor,
};
use super::query::{
    active_root_execution_optional, inspect_work_canonical_on, latest_restored_record,
    load_root_execution, load_work_claim_optional, load_work_item, load_work_run,
    work_completed_by_restored_record_on,
};
use super::session::begin_work_protocol_attempt_on;
use super::{
    EvidenceProjectionRow, GateWorkProtocolAttempt, GateWorkProtocolIntent,
    StoredWorkEvidenceSelectionRow, WorkEventDraft, WorkEvidenceProjectionSummary, WorkNoteCapture,
};
use crate::{
    CanonicalObject, ObjectHash, RestoredWorkEvidence, WorkId,
    domain::{
        AcceptWorkHandoffRequest, ActorContext, AppendRestoredWorkGateRequest,
        CancelWorkHandoffRequest, ClaimWorkRequest, CompletionSeal, EnvironmentEvidence, FeedId,
        FeedPosition, GATE_EVIDENCE_SUMMARY, GateEvidenceRecord, OfferWorkHandoffRequest,
        POST_COMPLETION_EVIDENCE_PROVENANCE_REFERENCE, POST_COMPLETION_EVIDENCE_PROVENANCE_SOURCE,
        RecordGateEvidenceRequest, RecordRestoredWorkEvidenceRequest, RecordWorkEvidenceRequest,
        RecordWorkNoteRequest, ReleaseWorkRequest, RestoredWorkEvidenceInput, RootExecution,
        RootExecutionId, RootExecutionState, SCHEMA_VERSION, VerificationEvidence,
        WorkAvailability, WorkCheckpoint, WorkClaim, WorkClaimId, WorkClaimState, WorkEvent,
        WorkEvidence, WorkEvidenceKind, WorkHandoffOffer, WorkHandoffOfferId, WorkHandoffState,
        WorkItem, WorkLifecycle, WorkRun, WorkRunId, WorkRunState, WorkTransition,
        normalize_gate_evidence_input, validate_gate_evidence_payload,
    },
    memory::Redactor,
};

#[cfg(test)]
mod tests;

pub(super) fn latest_canonical_handoff_offer(
    connection: &Connection,
    offer_id: &str,
    work_id: &str,
) -> Result<Option<WorkHandoffOffer>, StoreError> {
    let stored = connection
        .query_row(
            "SELECT entry.object_hash, object.object_kind, object.canonical_json
             FROM work_feed_entries entry
             JOIN objects object ON object.object_hash = entry.object_hash
             WHERE entry.feed_kind = 'project'
                AND entry.object_kind = 'work_event'
                AND entry.work_id = ?2
                AND json_extract(object.canonical_json, '$.handoff_offer.offer_id') = ?1
             ORDER BY entry.position DESC LIMIT 1",
            params![offer_id, work_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((stored_hash, object_kind, bytes)) = stored else {
        return Ok(None);
    };
    if object_kind != "work_event" {
        return Err(StoreError::InvalidWorkProjection(format!(
            "handoff offer {offer_id} is bound to a non-event canonical object"
        )));
    }
    let hash = ObjectHash::from_stored(stored_hash.clone())
        .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
    let event: WorkEvent = CanonicalObject::verify(&hash, bytes)?.decode()?;
    if event
        .handoff_offer
        .as_ref()
        .is_some_and(|offer| offer.offer_id.0.to_string() == offer_id)
    {
        Ok(event.handoff_offer)
    } else {
        Err(StoreError::InvalidWorkProjection(format!(
            "handoff offer {offer_id} is not bound to its canonical event"
        )))
    }
}

fn latest_restored_gate_evidence_on(
    connection: &Connection,
    work_id: WorkId,
    name: &str,
) -> Result<Option<ObjectHash>, StoreError> {
    let stored = connection
        .prepare(
            "SELECT evidence_hash, sequence
             FROM work_restored_evidence INDEXED BY work_restored_evidence_gate
             WHERE work_id = ?1 AND gate_name = ?2
             ORDER BY sequence, evidence_hash",
        )?
        .query_map(params![work_id.0.to_string(), name], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, i64>(1)?))
        })?
        .collect::<Result<Vec<_>, _>>()?;
    if stored.is_empty() {
        return Ok(None);
    }
    let mut evidence = HashMap::with_capacity(stored.len());
    let mut referenced = HashSet::with_capacity(stored.len());
    for (stored_hash, sequence) in stored {
        let hash = ObjectHash::from_stored(stored_hash.clone())
            .ok_or(StoreError::InvalidStoredHash(stored_hash))?;
        let value: RestoredWorkEvidence =
            load_typed_work_object(connection, &hash, "work_restored_evidence")?;
        if value.work_id != work_id
            || value.sequence != sequence
            || value.gate.as_ref().map(|gate| gate.name.as_str()) != Some(name)
        {
            return Err(StoreError::InvalidWorkProjection(
                "restored gate evidence differs from its projection binding".into(),
            ));
        }
        if let Some(previous) = value.gate.as_ref().and_then(|gate| gate.previous.clone())
            && !referenced.insert(previous)
        {
            return Err(StoreError::InvalidWorkProjection(
                "restored gate evidence chain branches".into(),
            ));
        }
        evidence.insert(hash, value);
    }
    let heads = evidence
        .keys()
        .filter(|hash| !referenced.contains(*hash))
        .cloned()
        .collect::<Vec<_>>();
    let [head] = heads.as_slice() else {
        return Err(StoreError::InvalidWorkProjection(
            "restored gate evidence does not form one chain".into(),
        ));
    };
    let mut cursor = Some(head.clone());
    let mut visited = HashSet::with_capacity(evidence.len());
    while let Some(hash) = cursor {
        if !visited.insert(hash.clone()) {
            return Err(StoreError::InvalidWorkProjection(
                "restored gate evidence chain contains a cycle".into(),
            ));
        }
        cursor = evidence
            .get(&hash)
            .and_then(|value| value.gate.as_ref())
            .and_then(|gate| gate.previous.clone());
    }
    if visited.len() != evidence.len() {
        return Err(StoreError::InvalidWorkProjection(
            "restored gate evidence chain is disconnected".into(),
        ));
    }
    Ok(Some(head.clone()))
}

pub(super) fn ensure_restored_execution_state(
    transaction: &Transaction<'_>,
    item: &mut WorkItem,
    now: DateTime<Utc>,
) -> Result<(RootExecution, WorkRun, bool), StoreError> {
    if let Some(run_id) = item.active_run_id {
        let run = load_work_run(transaction, run_id)?;
        let execution = load_root_execution(transaction, run.root_execution_id)?;
        return Ok((execution, run, false));
    }
    if !item.restored || item.lifecycle != WorkLifecycle::Open {
        return Err(StoreError::InvalidWorkProjection(
            "work has no active run for this operation".into(),
        ));
    }

    let (mut execution, created_execution) =
        if let Some(execution) = active_root_execution_optional(transaction, item.root_id)? {
            (execution, false)
        } else {
            (
                RootExecution {
                    schema_version: SCHEMA_VERSION,
                    root_execution_id: RootExecutionId::new(),
                    project_id: item.project_id.clone(),
                    root_id: item.root_id,
                    generation: 1,
                    state: RootExecutionState::Active,
                    revision: 1,
                    run_ids: Vec::new(),
                    required_child_seals: Vec::new(),
                    required_child_waivers: Vec::new(),
                    expected_contributors: Vec::new(),
                    contributions: Vec::new(),
                    waivers: Vec::new(),
                    created_at: now,
                    updated_at: now,
                },
                true,
            )
        };
    let generation = transaction.query_row(
        "SELECT COALESCE(MAX(generation), 0) + 1 FROM work_runs WHERE work_id = ?1",
        [item.work_id.0.to_string()],
        |row| row.get::<_, i64>(0),
    )?;
    let run = WorkRun {
        schema_version: SCHEMA_VERSION,
        run_id: WorkRunId::new(),
        root_execution_id: execution.root_execution_id,
        work_id: item.work_id,
        generation,
        executor: None,
        state: WorkRunState::Open,
        revision: 1,
        last_checkpoint: None,
        completion_seal: None,
        created_at: now,
        updated_at: now,
    };
    execution.run_ids.push(run.run_id);
    execution.run_ids.sort_by_key(|run_id| run_id.0);
    execution.run_ids.dedup();
    execution.updated_at = now;
    if created_execution {
        transaction.execute(
            "INSERT INTO work_root_executions (
                 root_execution_id, project_id, root_id, generation, state,
                 revision, created_at_ms, updated_at_ms, execution_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                execution.root_execution_id.0.to_string(),
                execution.project_id.0,
                execution.root_id.0.to_string(),
                execution.generation,
                "active",
                execution.revision,
                execution.created_at.timestamp_millis(),
                execution.updated_at.timestamp_millis(),
                serde_json::to_vec(&execution)?
            ],
        )?;
    } else {
        execution.revision += 1;
        persist_root_execution(transaction, &execution)?;
    }
    transaction.execute(
        "INSERT INTO work_runs (
             run_id, root_execution_id, work_id, generation,
             executor_session_id, state, revision, claim_fence_head,
             last_checkpoint_hash, completion_seal_hash,
             created_at_ms, updated_at_ms, run_json
         ) VALUES (?1, ?2, ?3, ?4, NULL, 'open', 1, 0, NULL, NULL, ?5, ?6, ?7)",
        params![
            run.run_id.0.to_string(),
            run.root_execution_id.0.to_string(),
            run.work_id.0.to_string(),
            run.generation,
            run.created_at.timestamp_millis(),
            run.updated_at.timestamp_millis(),
            serde_json::to_vec(&run)?
        ],
    )?;
    item.active_run_id = Some(run.run_id);
    item.updated_at = now;
    persist_work_item(transaction, item)?;
    Ok((execution, run, true))
}

fn append_restored_work_evidence_on(
    transaction: &Transaction<'_>,
    work_id: WorkId,
    expected_work_revision: i64,
    input: &RestoredWorkEvidenceInput,
    actor: &ActorContext,
    recorded_at: DateTime<Utc>,
) -> Result<ObjectHash, StoreError> {
    let item = load_work_item(transaction, work_id)?;
    assert_revision(&item, expected_work_revision)?;
    if !work_completed_by_restored_record_on(transaction, &item)? {
        return Err(StoreError::InvalidWork(
            "restored late findings require completed-by-record work".into(),
        ));
    }
    let (record_hash, record) =
        latest_restored_record(transaction, item.work_id)?.ok_or_else(|| {
            StoreError::InvalidWorkProjection(
                "restored completed work has no history record".into(),
            )
        })?;
    if record.history.completion.is_none() {
        return Err(StoreError::InvalidWorkProjection(
            "restored completed work has no completion proof".into(),
        ));
    }
    let (summary, refs, gate) = match input {
        RestoredWorkEvidenceInput::Note { summary, refs } => (
            normalize_text(summary, "note summary")?,
            normalize_strings(refs),
            None,
        ),
        RestoredWorkEvidenceInput::Gate {
            name,
            failed,
            evidence_ref,
        } => {
            let normalized = normalize_gate_evidence_input(name, failed, evidence_ref.as_deref())
                .map_err(StoreError::InvalidWork)?;
            let previous =
                latest_restored_gate_evidence_on(transaction, item.work_id, &normalized.name)?;
            (
                GATE_EVIDENCE_SUMMARY.into(),
                normalized.evidence_ref.into_iter().collect(),
                Some(GateEvidenceRecord {
                    schema_version: SCHEMA_VERSION,
                    name: normalized.name,
                    passed: normalized.failed.is_empty(),
                    failed: normalized.failed,
                    previous,
                }),
            )
        }
    };
    let sequence = transaction.query_row(
        "SELECT COALESCE(MAX(sequence), 0) + 1
         FROM work_restored_evidence WHERE work_id = ?1",
        [item.work_id.0.to_string()],
        |row| row.get::<_, i64>(0),
    )?;
    if sequence <= 0 {
        return Err(StoreError::InvalidWorkProjection(
            "restored evidence sequence exceeds SQLite range".into(),
        ));
    }
    let evidence = RestoredWorkEvidence {
        schema_version: SCHEMA_VERSION,
        work_id: item.work_id,
        restored_record: record_hash,
        sequence,
        summary,
        refs,
        gate,
        actor: actor.clone(),
        created_at: recorded_at,
    };
    let object = CanonicalObject::freeze(&evidence)?;
    SqliteStore::insert_object(transaction, "work_restored_evidence", &object)?;
    transaction.execute(
        "INSERT INTO work_restored_evidence (
              evidence_hash, work_id, record_hash, sequence, gate_name, created_at_ms
          ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            object.hash().as_str(),
            item.work_id.0.to_string(),
            evidence.restored_record.as_str(),
            evidence.sequence,
            evidence.gate.as_ref().map(|gate| gate.name.as_str()),
            evidence.created_at.timestamp_millis(),
        ],
    )?;
    append_to_work_feeds(
        transaction,
        &item.project_id,
        item.root_id,
        None,
        None,
        "work_restored_evidence",
        &object,
    )?;
    Ok(object.hash().clone())
}

impl SqliteStore {
    /// Appends one distinct restored gate observation without retry bookkeeping.
    pub(crate) fn append_restored_work_gate<R: Redactor>(
        &mut self,
        request: &AppendRestoredWorkGateRequest,
        redactor: &R,
    ) -> Result<ObjectHash, StoreError> {
        inspect_work_request(redactor, request, &request.actor)?;
        assert_actor_session(&request.actor, &request.holder)?;
        validate_evidence_phase_marker(WorkLifecycle::Completed, &request.actor)?;
        let transaction = self.begin_work_mutation()?;
        let hash = append_restored_work_evidence_on(
            &transaction,
            request.work_id,
            request.expected_work_revision,
            &RestoredWorkEvidenceInput::Gate {
                name: request.name.clone(),
                failed: request.failed.clone(),
                evidence_ref: request.evidence_ref.clone(),
            },
            &request.actor,
            request.recorded_at,
        )?;
        transaction.commit()?;
        Ok(hash)
    }

    /// Appends one note or gate whose immutable basis is the newest restored
    /// completion record. No run, seal, claim, or native work event is minted.
    pub(crate) fn record_restored_work_evidence<R: Redactor>(
        &mut self,
        request: &RecordRestoredWorkEvidenceRequest,
        redactor: &R,
    ) -> Result<ObjectHash, StoreError> {
        inspect_work_request(redactor, request, &request.actor)?;
        assert_actor_session(&request.actor, &request.holder)?;
        validate_evidence_phase_marker(WorkLifecycle::Completed, &request.actor)?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(hash) = replay_operation::<ObjectHash>(
            &transaction,
            "record_restored_work_evidence",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(hash);
        }
        let hash = append_restored_work_evidence_on(
            &transaction,
            request.work_id,
            request.expected_work_revision,
            &request.input,
            &request.actor,
            request.recorded_at,
        )?;
        persist_operation_result(
            &transaction,
            "record_restored_work_evidence",
            &request.idempotency_key,
            request_object.hash(),
            &hash,
        )?;
        transaction.commit()?;
        Ok(hash)
    }

    /// Atomically claims ready work or recovers an expired/released claim.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when work is not ready, another live claim wins,
    /// the request conflicts with an idempotent retry, or persistence fails.
    pub fn claim_work<R: Redactor>(
        &mut self,
        request: &ClaimWorkRequest,
        redactor: &R,
    ) -> Result<WorkClaim, StoreError> {
        inspect_work_request(redactor, request, &request.actor)?;
        assert_actor_session(&request.actor, &request.holder)?;
        let expires_at = claim_expiry(request.claimed_at, request.ttl_seconds)?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(claim) = replay_operation::<WorkClaim>(
            &transaction,
            "claim_work",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(claim);
        }
        let mut item = load_work_item(&transaction, request.work_id)?;
        assert_revision(&item, request.expected_work_revision)?;
        if item.active_run_id != request.expected_run_id {
            return Err(StoreError::InvalidWorkProjection(
                "claim request does not match the current active run".into(),
            ));
        }
        if let Some(run_id) = request.expected_run_id {
            expire_handoff_offers(&transaction, run_id, request.claimed_at, &request.actor)?;
        }
        if item.lifecycle != WorkLifecycle::Open {
            return Err(StoreError::WorkNotOpen(item.work_id));
        }
        let view = inspect_work_canonical_on(&transaction, item.work_id, request.claimed_at)?;
        if !matches!(view.availability, WorkAvailability::Ready) {
            if matches!(
                view.availability,
                WorkAvailability::Claimed | WorkAvailability::Active
            ) && let Some(run_id) = item.active_run_id
                && let Some(claim) = load_work_claim_optional(&transaction, run_id)?
            {
                // Claiming work this session already holds is a no-op that
                // returns the live claim; only another holder is a conflict.
                if claim.holder == request.holder
                    && claim.state == WorkClaimState::Active
                    && claim.expires_at > request.claimed_at
                {
                    transaction.commit()?;
                    return Ok(claim);
                }
                return Err(StoreError::WorkClaimHeld {
                    work: item.work_id,
                    holder: claim.holder.0,
                    expires_at: claim.expires_at.timestamp_millis(),
                });
            }
            return Err(StoreError::InvalidWork(format!(
                "work is not ready: {:?}",
                view.availability
            )));
        }
        let (mut root_execution, mut run, created_run) =
            ensure_restored_execution_state(&transaction, &mut item, request.claimed_at)?;
        let run_id = run.run_id;
        let prior = if created_run {
            None
        } else {
            load_work_claim_optional(&transaction, run_id)?
        };
        if let Some(claim) = prior.as_ref()
            && claim.state == WorkClaimState::Active
            && claim.expires_at > request.claimed_at
        {
            return Err(StoreError::WorkClaimHeld {
                work: item.work_id,
                holder: claim.holder.0.clone(),
                expires_at: claim.expires_at.timestamp_millis(),
            });
        }
        let fence_head = transaction.query_row(
            "SELECT claim_fence_head FROM work_runs WHERE run_id = ?1",
            [run_id.0.to_string()],
            |row| row.get::<_, i64>(0),
        )?;
        let recovered = prior.is_some();
        let claim = WorkClaim {
            claim_id: prior
                .as_ref()
                .map_or_else(WorkClaimId::new, |claim| claim.claim_id),
            work_id: item.work_id,
            run_id,
            accepted_work_revision: item.revision,
            holder: request.holder.clone(),
            expires_at,
            revision: prior.as_ref().map_or(1, |claim| claim.revision + 1),
            fence: fence_head + 1,
            state: WorkClaimState::Active,
        };
        let mut root_changed = expect_root_contributor(&mut root_execution, &request.holder);
        if let Some(prior_claim) = prior.as_ref()
            && prior_claim.holder != request.holder
            && !root_participant_is_accounted(&root_execution, &prior_claim.holder)
        {
            let reason = request.recovery_reason.as_deref().ok_or_else(|| {
                StoreError::InvalidWork(
                    "claim recovery requires an explicit attributed reason".into(),
                )
            })?;
            let reason = normalize_text(reason, "claim recovery reason")?;
            root_changed |= waive_root_contributor(
                &mut root_execution,
                &prior_claim.holder,
                &request.actor.actor_id,
                &reason,
            );
        }
        if root_changed {
            root_execution.revision += 1;
            root_execution.updated_at = request.claimed_at;
            persist_root_execution(&transaction, &root_execution)?;
        }
        let preserve_active_run = run.state == WorkRunState::Active
            && prior
                .as_ref()
                .is_some_and(|claim| claim.holder == request.holder);
        run.executor = Some(request.holder.clone());
        if !preserve_active_run {
            run.state = WorkRunState::Claimed;
        }
        run.revision += 1;
        run.updated_at = request.claimed_at;
        persist_claim(&transaction, &claim)?;
        persist_work_run(&transaction, &run, claim.fence)?;
        let event = WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: Some(run_id),
            revision: item.revision,
            work: item.clone(),
            run: Some(run.clone()),
            root_execution: Some(root_execution),
            claim: Some(claim.clone()),
            handoff_offer: None,
            blocker: None,
            transition: WorkTransition::Claimed {
                claim: claim.clone(),
                recovered,
            },
            actor: request.actor.clone(),
            created_at: request.claimed_at,
        };
        append_work_event(&transaction, &event)?;
        persist_operation_result(
            &transaction,
            "claim_work",
            &request.idempotency_key,
            request_object.hash(),
            &claim,
        )?;
        transaction.commit()?;
        Ok(claim)
    }

    /// Releases a live claim and advances its fence without reviving old authority.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the exact work, run, claim, revision, holder,
    /// or fence basis is stale, or persistence fails.
    pub fn release_work<R: Redactor>(
        &mut self,
        request: &ReleaseWorkRequest,
        redactor: &R,
    ) -> Result<WorkClaim, StoreError> {
        inspect_work_request(redactor, request, &request.actor)?;
        assert_actor_session(&request.actor, &request.holder)?;
        let reason = normalize_text(&request.reason, "release reason")?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(claim) = replay_operation::<WorkClaim>(
            &transaction,
            "release_work",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(claim);
        }
        expire_handoff_offers(
            &transaction,
            request.run_id,
            request.released_at,
            &request.actor,
        )?;
        let (item, mut run, mut claim) = validate_live_claim_on(
            &transaction,
            request.work_id,
            request.run_id,
            request.expected_work_revision,
            &request.holder,
            request.claim_id,
            request.claim_fence,
            request.released_at,
            false,
        )?;
        let mut root_execution = load_root_execution(&transaction, run.root_execution_id)?;
        if !root_participant_is_accounted(&root_execution, &claim.holder) {
            let reason = request.waiver_reason.as_deref().ok_or_else(|| {
                StoreError::InvalidWork(
                    "completion waiver requires an explicit attributed reason".into(),
                )
            })?;
            let reason = normalize_text(reason, "completion waiver reason")?;
            if waive_root_contributor(
                &mut root_execution,
                &claim.holder,
                &request.actor.actor_id,
                &reason,
            ) {
                root_execution.revision += 1;
                root_execution.updated_at = request.released_at;
                persist_root_execution(&transaction, &root_execution)?;
            }
        }
        claim.state = WorkClaimState::Released;
        claim.revision += 1;
        claim.fence += 1;
        claim.expires_at = request.released_at;
        run.executor = None;
        run.state = WorkRunState::Open;
        run.revision += 1;
        run.updated_at = request.released_at;
        persist_claim(&transaction, &claim)?;
        persist_work_run(&transaction, &run, claim.fence)?;
        let event = WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: Some(run.run_id),
            revision: item.revision,
            work: item.clone(),
            run: Some(run.clone()),
            root_execution: Some(root_execution),
            claim: Some(claim.clone()),
            handoff_offer: None,
            blocker: None,
            transition: WorkTransition::Released {
                claim_id: claim.claim_id,
                fence: claim.fence,
                reason,
            },
            actor: request.actor.clone(),
            created_at: request.released_at,
        };
        append_work_event(&transaction, &event)?;
        persist_operation_result(
            &transaction,
            "release_work",
            &request.idempotency_key,
            request_object.hash(),
            &claim,
        )?;
        transaction.commit()?;
        Ok(claim)
    }

    /// Captures a checkpoint under the exact work, run, claim, and fence basis.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when authority is stale, cited evidence is invalid,
    /// a handoff is pending, or persistence fails.
    pub fn checkpoint_work<R: Redactor>(
        &mut self,
        request: &crate::domain::CheckpointWorkRequest,
        redactor: &R,
    ) -> Result<ObjectHash, StoreError> {
        inspect_work_request(redactor, request, &request.actor)?;
        assert_actor_session(&request.actor, &request.holder)?;
        let summary = normalize_text(&request.summary, "checkpoint summary")?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(hash) = replay_operation::<ObjectHash>(
            &transaction,
            "checkpoint_work",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(hash);
        }
        expire_handoff_offers(
            &transaction,
            request.run_id,
            request.checkpointed_at,
            &request.actor,
        )?;
        let (item, run, mut claim) = validate_live_claim_on(
            &transaction,
            request.work_id,
            request.run_id,
            request.expected_work_revision,
            &request.holder,
            request.claim_id,
            request.claim_fence,
            request.checkpointed_at,
            false,
        )?;
        let evidence = match request.evidence.as_ref() {
            Some(evidence) => {
                let evidence = unique_hashes(evidence);
                ensure_run_evidence(&transaction, run.run_id, &evidence)?;
                evidence
            }
            None => work_run_evidence_on(&transaction, run.run_id)?,
        };
        renew_holder_claim(&transaction, &mut claim, request.checkpointed_at)?;
        let checkpoint = persist_work_checkpoint_on(
            &transaction,
            &item,
            run,
            claim,
            summary,
            evidence,
            &request.actor,
            request.checkpointed_at,
        )?;
        persist_operation_result(
            &transaction,
            "checkpoint_work",
            &request.idempotency_key,
            request_object.hash(),
            &checkpoint,
        )?;
        transaction.commit()?;
        Ok(checkpoint)
    }

    /// Captures or replays the completion checkpoint whose idempotency key is
    /// derived from the exact run-feed cut acknowledged by that checkpoint.
    /// Cut selection, replay proof, and checkpoint persistence share one
    /// immediate transaction, so another writer cannot advance the feed
    /// between the selected cut and the committed checkpoint.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when authority is stale, cited evidence is
    /// invalid, the derived key conflicts, or persistence fails.
    pub(crate) fn checkpoint_work_for_completion<R, F>(
        &mut self,
        request: &crate::domain::CheckpointWorkRequest,
        mut key_for_cut: F,
        redactor: &R,
    ) -> Result<(ObjectHash, FeedPosition), StoreError>
    where
        R: Redactor,
        F: FnMut(&FeedPosition) -> Result<String, StoreError>,
    {
        inspect_work_request(redactor, request, &request.actor)?;
        assert_actor_session(&request.actor, &request.holder)?;
        let summary = normalize_text(&request.summary, "checkpoint summary")?;
        let transaction = self.begin_work_mutation()?;
        expire_handoff_offers(
            &transaction,
            request.run_id,
            request.checkpointed_at,
            &request.actor,
        )?;
        let (item, run, mut claim) = validate_live_claim_on(
            &transaction,
            request.work_id,
            request.run_id,
            request.expected_work_revision,
            &request.holder,
            request.claim_id,
            request.claim_fence,
            request.checkpointed_at,
            false,
        )?;
        let evidence = match request.evidence.as_ref() {
            Some(evidence) => {
                let evidence = unique_hashes(evidence);
                ensure_run_evidence(&transaction, run.run_id, &evidence)?;
                evidence
            }
            None => work_run_evidence_on(&transaction, run.run_id)?,
        };
        let current_cut = FeedPosition {
            feed: FeedId::RunExecution(run.run_id),
            position: feed_head(&transaction, &FeedId::RunExecution(run.run_id))?,
        };

        if let Some(checkpoint_hash) = run.last_checkpoint.as_ref() {
            let checkpoint: WorkCheckpoint =
                load_typed_work_object(&transaction, checkpoint_hash, "work_checkpoint")?;
            let checkpoint_is_current = checkpoint.work_id == item.work_id
                && checkpoint.run_id == run.run_id
                && checkpoint.claim_id == claim.claim_id
                && checkpoint.claim_fence == claim.fence
                && checkpoint.evidence == evidence
                && checkpoint.acknowledged_run_position.feed == current_cut.feed
                && checkpoint_feed_end(checkpoint.acknowledged_run_position.position)?
                    == current_cut.position;
            if checkpoint_is_current {
                let key = key_for_cut(&checkpoint.acknowledged_run_position)?;
                let mut replay_request = request.clone();
                replay_request.evidence = Some(evidence.clone());
                replay_request.idempotency_key.clone_from(&key);
                replay_request.checkpointed_at = checkpoint.created_at;
                let request_object = request_object(&replay_request)?;
                let stored: Option<(String, Vec<u8>)> = transaction
                    .query_row(
                        "SELECT request_hash, result_json FROM work_operation_results
                         WHERE operation = 'checkpoint_work' AND idempotency_key = ?1",
                        [&key],
                        |row| Ok((row.get(0)?, row.get(1)?)),
                    )
                    .optional()?;
                if let Some((stored_request_hash, result_json)) = stored
                    && stored_request_hash == request_object.hash().as_str()
                {
                    let stored_checkpoint: ObjectHash = serde_json::from_slice(&result_json)?;
                    if &stored_checkpoint != checkpoint_hash {
                        return Err(StoreError::InvalidWorkProjection(
                            "completion checkpoint result does not name the current checkpoint"
                                .into(),
                        ));
                    }
                    transaction.commit()?;
                    return Ok((stored_checkpoint, checkpoint.acknowledged_run_position));
                }
            }
        }

        renew_holder_claim(&transaction, &mut claim, request.checkpointed_at)?;
        let key = key_for_cut(&current_cut)?;
        let mut persisted_request = request.clone();
        persisted_request.evidence = Some(evidence.clone());
        persisted_request.idempotency_key.clone_from(&key);
        let request_object = request_object(&persisted_request)?;
        if transaction
            .query_row(
                "SELECT 1 FROM work_operation_results
                 WHERE operation = 'checkpoint_work' AND idempotency_key = ?1",
                [&key],
                |_| Ok(()),
            )
            .optional()?
            .is_some()
        {
            return Err(StoreError::WorkOperationIdempotencyConflict {
                operation: "checkpoint_work".into(),
                key,
            });
        }
        let checkpoint = persist_work_checkpoint_on(
            &transaction,
            &item,
            run,
            claim,
            summary,
            evidence,
            &request.actor,
            request.checkpointed_at,
        )?;
        let persisted_checkpoint: WorkCheckpoint =
            load_typed_work_object(&transaction, &checkpoint, "work_checkpoint")?;
        if persisted_checkpoint.acknowledged_run_position != current_cut {
            return Err(StoreError::InvalidWorkProjection(
                "completion checkpoint did not retain its transaction-selected run-feed cut".into(),
            ));
        }
        persist_operation_result(
            &transaction,
            "checkpoint_work",
            &persisted_request.idempotency_key,
            request_object.hash(),
            &checkpoint,
        )?;
        transaction.commit()?;
        Ok((checkpoint, current_cut))
    }

    /// Adds immutable evidence under the exact live claim basis.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when authority is stale, a handoff is pending,
    /// content is invalid, or persistence fails.
    pub fn record_work_evidence<R: Redactor>(
        &mut self,
        request: &RecordWorkEvidenceRequest,
        redactor: &R,
    ) -> Result<ObjectHash, StoreError> {
        inspect_work_request(redactor, request, &request.actor)?;
        assert_actor_session(&request.actor, &request.holder)?;
        let summary = normalize_text(&request.summary, "evidence summary")?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(hash) = replay_operation::<ObjectHash>(
            &transaction,
            "record_work_evidence",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(hash);
        }
        expire_handoff_offers(
            &transaction,
            request.run_id,
            request.recorded_at,
            &request.actor,
        )?;
        let (item, run, mut claim) = validate_live_claim_on(
            &transaction,
            request.work_id,
            request.run_id,
            request.expected_work_revision,
            &request.holder,
            request.claim_id,
            request.claim_fence,
            request.recorded_at,
            false,
        )?;
        renew_holder_claim(&transaction, &mut claim, request.recorded_at)?;
        let evidence = WorkEvidence {
            schema_version: SCHEMA_VERSION,
            work_id: item.work_id,
            run_id: run.run_id,
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
            summary,
            refs: normalize_strings(&request.refs),
            gate: None,
            actor: request.actor.clone(),
            created_at: request.recorded_at,
        };
        let evidence_hash = persist_work_evidence_on(&transaction, &item, &run, claim, &evidence)?;
        persist_operation_result(
            &transaction,
            "record_work_evidence",
            &request.idempotency_key,
            request_object.hash(),
            &evidence_hash,
        )?;
        transaction.commit()?;
        Ok(evidence_hash)
    }

    /// Captures one note as evidence and the checkpoint that acknowledges the
    /// resulting run-evidence cut in one exact-claim transaction.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when authority is stale, content is invalid, a
    /// handoff is pending, or either immutable object cannot be persisted.
    pub(crate) fn record_work_note<R: Redactor>(
        &mut self,
        request: &RecordWorkNoteRequest,
        redactor: &R,
    ) -> Result<WorkNoteCapture, StoreError> {
        inspect_work_request(redactor, request, &request.actor)?;
        assert_actor_session(&request.actor, &request.holder)?;
        let summary = normalize_text(&request.summary, "note summary")?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(capture) = replay_operation::<WorkNoteCapture>(
            &transaction,
            "record_work_note",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(capture);
        }
        let item = load_work_item(&transaction, request.work_id)?;
        if item.lifecycle == WorkLifecycle::Completed {
            let (item, run, claim) = validate_post_completion_evidence_basis_on(
                &transaction,
                item,
                request.run_id,
                request.expected_work_revision,
                request.claim_id,
                request.claim_fence,
                request.recorded_at,
            )?;
            let evidence = WorkEvidence {
                schema_version: SCHEMA_VERSION,
                work_id: item.work_id,
                run_id: run.run_id,
                claim_id: request.claim_id,
                claim_fence: request.claim_fence,
                summary,
                refs: normalize_strings(&request.refs),
                gate: None,
                actor: request.actor.clone(),
                created_at: request.recorded_at,
            };
            let evidence_hash = persist_post_completion_work_evidence_on(
                &transaction,
                &item,
                &run,
                claim,
                &evidence,
            )?;
            let capture = WorkNoteCapture {
                evidence: evidence_hash,
                checkpoint: None,
            };
            persist_operation_result(
                &transaction,
                "record_work_note",
                &request.idempotency_key,
                request_object.hash(),
                &capture,
            )?;
            transaction.commit()?;
            return Ok(capture);
        }
        expire_handoff_offers(
            &transaction,
            request.run_id,
            request.recorded_at,
            &request.actor,
        )?;
        let (item, run, mut claim) = validate_live_claim_for_item_on(
            &transaction,
            item,
            request.run_id,
            request.expected_work_revision,
            &request.holder,
            request.claim_id,
            request.claim_fence,
            request.recorded_at,
            false,
        )?;
        renew_holder_claim(&transaction, &mut claim, request.recorded_at)?;
        let evidence = WorkEvidence {
            schema_version: SCHEMA_VERSION,
            work_id: item.work_id,
            run_id: run.run_id,
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
            summary: summary.clone(),
            refs: normalize_strings(&request.refs),
            gate: None,
            actor: request.actor.clone(),
            created_at: request.recorded_at,
        };
        let evidence_hash =
            persist_work_evidence_on(&transaction, &item, &run, claim.clone(), &evidence)?;
        let acknowledged_evidence = work_run_evidence_on(&transaction, run.run_id)?;
        let checkpoint_hash = persist_work_checkpoint_on(
            &transaction,
            &item,
            run,
            claim,
            summary,
            acknowledged_evidence,
            &request.actor,
            request.recorded_at,
        )?;
        let capture = WorkNoteCapture {
            evidence: evidence_hash,
            checkpoint: Some(checkpoint_hash),
        };
        persist_operation_result(
            &transaction,
            "record_work_note",
            &request.idempotency_key,
            request_object.hash(),
            &capture,
        )?;
        transaction.commit()?;
        Ok(capture)
    }

    /// Records a quality-gate transition, or replays the latest consecutive
    /// identical observation, under one SQLite write transaction.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when a new observation has a stale live-claim
    /// basis, the typed gate payload is inconsistent, or persistence fails.
    /// An exact consecutive replay is a read of the already-recorded fact and
    /// does not revalidate the historical claim.
    #[cfg(test)]
    pub(crate) fn record_gate_evidence<R: Redactor>(
        &mut self,
        request: &RecordGateEvidenceRequest,
        redactor: &R,
    ) -> Result<ObjectHash, StoreError> {
        inspect_work_request(redactor, request, &request.actor)?;
        assert_actor_session(&request.actor, &request.holder)?;
        let normalized = normalize_gate_evidence_input(
            &request.name,
            &request.failed,
            request.evidence_ref.as_deref(),
        )
        .map_err(StoreError::InvalidWork)?;
        let name = normalized.name;
        let failed = normalized.failed;
        let refs = normalized.evidence_ref.into_iter().collect::<Vec<_>>();
        let transaction = self.begin_work_mutation()?;
        let previous = latest_gate_evidence_on(&transaction, request.run_id, &name)?;
        if let Some((hash, evidence)) = previous.as_ref()
            && gate_observation_matches(evidence, request, &name, &failed, &refs)
        {
            transaction.commit()?;
            return Ok(hash.clone());
        }

        let evidence_hash = append_gate_evidence_on(
            &transaction,
            request,
            name,
            failed,
            refs,
            previous.as_ref().map(|(hash, _)| hash.clone()),
        )?;
        transaction.commit()?;
        Ok(evidence_hash)
    }

    /// Atomically reserves the gate transition's caller-visible protocol
    /// attempt and records (or reuses) its evidence object. The attempt key is
    /// derived from normalized input plus the previous distinct observation,
    /// so pass -> fail -> pass remains three transitions while an exact retry
    /// after a crash resumes the same pending attempt.
    pub(crate) fn record_gate_evidence_protocol<B: Serialize, R: Redactor>(
        &mut self,
        request: &RecordGateEvidenceRequest,
        protocol: &BeginGateWorkProtocolAttempt<'_, B>,
        redactor: &R,
    ) -> Result<GateWorkProtocolAttempt, StoreError> {
        inspect_work_request(redactor, request, &request.actor)?;
        assert_actor_session(&request.actor, &request.holder)?;
        let normalized = normalize_gate_evidence_input(
            &request.name,
            &request.failed,
            request.evidence_ref.as_deref(),
        )
        .map_err(StoreError::InvalidWork)?;
        let name = normalized.name;
        let failed = normalized.failed;
        let refs = normalized.evidence_ref.into_iter().collect::<Vec<_>>();
        let transaction = self.begin_work_mutation()?;
        let latest = latest_gate_evidence_on(&transaction, request.run_id, &name)?;
        let exact = latest.as_ref().is_some_and(|(_, evidence)| {
            gate_observation_matches(evidence, request, &name, &failed, &refs)
        });
        let previous = if exact {
            latest
                .as_ref()
                .and_then(|(_, evidence)| evidence.gate.as_ref())
                .and_then(|gate| gate.previous.as_ref())
        } else {
            latest.as_ref().map(|(hash, _)| hash)
        };
        let retry_actor = actor_without_optional_context(&request.actor);
        let intent = GateWorkProtocolIntent {
            schema_version: SCHEMA_VERSION,
            project_id: protocol.project_id,
            session_id: protocol.session_id,
            actor: &retry_actor,
            work_id: request.work_id,
            run_id: request.run_id,
            claim_id: request.claim_id,
            claim_fence: request.claim_fence,
            name: &name,
            failed: &failed,
            refs: &refs,
            previous,
        };
        let intent_object = CanonicalObject::freeze(&intent)?;
        let idempotency_key = format!("gate:{}", intent_object.hash().as_str());
        let attempt = begin_work_protocol_attempt_on(
            &transaction,
            &BeginWorkProtocolAttempt {
                project_id: protocol.project_id,
                session_id: protocol.session_id,
                operation: "work_update:gate",
                idempotency_key: &idempotency_key,
                intent: &intent,
                basis: protocol.basis,
                now: protocol.now,
            },
        )?;
        if attempt.result.is_some() && !exact {
            return Err(StoreError::InvalidWorkProjection(
                "completed gate protocol attempt disagrees with the latest same-name evidence"
                    .into(),
            ));
        }
        let evidence = if exact {
            latest
                .as_ref()
                .map(|(hash, _)| hash.clone())
                .ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "an exact gate replay has no latest evidence object".into(),
                    )
                })?
        } else {
            append_gate_evidence_on(
                &transaction,
                request,
                name,
                failed,
                refs,
                latest.as_ref().map(|(hash, _)| hash.clone()),
            )?
        };
        transaction.commit()?;
        Ok(GateWorkProtocolAttempt {
            evidence,
            idempotency_key,
            result: attempt.result,
        })
    }

    /// Offers a checkpoint-coupled handoff without transferring authority yet.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when authority is stale, a prior offer remains,
    /// the destination is invalid, or the atomic write fails.
    pub fn offer_work_handoff<R: Redactor>(
        &mut self,
        request: &OfferWorkHandoffRequest,
        redactor: &R,
    ) -> Result<WorkHandoffOffer, StoreError> {
        inspect_work_request(redactor, request, &request.actor)?;
        assert_actor_session(&request.actor, &request.from)?;
        validate_evidence_phase_marker(WorkLifecycle::Open, &request.actor)?;
        if request.from == request.to {
            return Err(StoreError::InvalidWork(
                "handoff source and destination must differ".into(),
            ));
        }
        let summary = normalize_text(&request.checkpoint_summary, "checkpoint summary")?;
        let requested_expiry = claim_expiry(request.offered_at, request.ttl_seconds)?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(offer) = replay_operation::<WorkHandoffOffer>(
            &transaction,
            "offer_work_handoff",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(offer);
        }
        expire_handoff_offers(
            &transaction,
            request.run_id,
            request.offered_at,
            &request.actor,
        )?;
        let (item, mut run, mut claim) = validate_live_claim_on(
            &transaction,
            request.work_id,
            request.run_id,
            request.expected_work_revision,
            &request.from,
            request.claim_id,
            request.claim_fence,
            request.offered_at,
            false,
        )?;
        renew_holder_claim(&transaction, &mut claim, request.offered_at)?;
        let acknowledged_run_position = FeedPosition {
            feed: FeedId::RunExecution(run.run_id),
            position: feed_head(&transaction, &FeedId::RunExecution(run.run_id))?,
        };
        let checkpoint = WorkCheckpoint {
            schema_version: SCHEMA_VERSION,
            work_id: item.work_id,
            run_id: run.run_id,
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
            acknowledged_run_position,
            summary,
            evidence: Vec::new(),
            actor: request.actor.clone(),
            created_at: request.offered_at,
        };
        let checkpoint_object = CanonicalObject::freeze(&checkpoint)?;
        SqliteStore::insert_object(&transaction, "work_checkpoint", &checkpoint_object)?;
        append_to_work_feeds(
            &transaction,
            &item.project_id,
            item.root_id,
            Some(run.run_id),
            None,
            "work_checkpoint",
            &checkpoint_object,
        )?;
        run.last_checkpoint = Some(checkpoint_object.hash().clone());
        run.state = WorkRunState::Active;
        run.revision += 1;
        run.updated_at = request.offered_at;
        persist_work_run(&transaction, &run, claim.fence)?;
        let mut root_execution = load_root_execution(&transaction, run.root_execution_id)?;
        let root_changed = expect_root_contributor(&mut root_execution, &claim.holder)
            | add_root_contribution(&mut root_execution, &claim.holder, checkpoint_object.hash());
        if root_changed {
            root_execution.revision += 1;
            root_execution.updated_at = request.offered_at;
            persist_root_execution(&transaction, &root_execution)?;
        }
        let offer = WorkHandoffOffer {
            offer_id: WorkHandoffOfferId::new(),
            work_id: item.work_id,
            run_id: run.run_id,
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
            work_revision: item.revision,
            from: request.from.clone(),
            to: request.to.clone(),
            checkpoint: checkpoint_object.hash().clone(),
            accepted_ttl_seconds: request.ttl_seconds,
            offered_at: request.offered_at,
            expires_at: claim.expires_at.min(requested_expiry),
            state: WorkHandoffState::Offered,
        };
        let offer_object = CanonicalObject::freeze(&offer)?;
        SqliteStore::insert_object(&transaction, "work_handoff_offer", &offer_object)?;
        transaction.execute(
            "INSERT INTO work_handoff_offers (
                 offer_id, run_id, work_id, state, expires_at_ms,
                 offer_hash, offer_json
             ) VALUES (?1, ?2, ?3, 'offered', ?4, ?5, ?6)",
            params![
                offer.offer_id.0.to_string(),
                offer.run_id.0.to_string(),
                offer.work_id.0.to_string(),
                offer.expires_at.timestamp_millis(),
                offer_object.hash().as_str(),
                serde_json::to_vec(&offer)?
            ],
        )?;
        let event = WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: Some(run.run_id),
            revision: item.revision,
            work: item.clone(),
            run: Some(run.clone()),
            root_execution: Some(root_execution),
            claim: Some(claim.clone()),
            handoff_offer: Some(offer.clone()),
            blocker: None,
            transition: WorkTransition::HandoffOffered {
                offer_id: offer.offer_id,
                to: offer.to.clone(),
                checkpoint: offer.checkpoint.clone(),
                offer: offer_object.hash().clone(),
            },
            actor: request.actor.clone(),
            created_at: request.offered_at,
        };
        append_work_event(&transaction, &event)?;
        persist_operation_result(
            &transaction,
            "offer_work_handoff",
            &request.idempotency_key,
            request_object.hash(),
            &offer,
        )?;
        transaction.commit()?;
        Ok(offer)
    }

    /// Accepts a pending handoff, transfers the claim, and advances its fence.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the offer expired or changed, its authority
    /// basis is stale, the destination differs, or persistence fails.
    pub fn accept_work_handoff<R: Redactor>(
        &mut self,
        request: &AcceptWorkHandoffRequest,
        redactor: &R,
    ) -> Result<WorkClaim, StoreError> {
        inspect_work_request(redactor, request, &request.actor)?;
        assert_actor_session(&request.actor, &request.to)?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(claim) = replay_operation::<WorkClaim>(
            &transaction,
            "accept_work_handoff",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(claim);
        }
        let offer_row: Option<(Option<String>, Vec<u8>)> = transaction
            .query_row(
                "SELECT offer_hash, offer_json FROM work_handoff_offers
                 WHERE offer_id = ?1",
                [request.offer_id.0.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let mut offer = offer_row
            .map(|row| load_handoff_offer_projection(&transaction, row))
            .transpose()?
            .ok_or_else(|| StoreError::InvalidWork("handoff offer is not active".into()))?;
        if offer.state != WorkHandoffState::Offered {
            return Err(StoreError::InvalidWork(
                "handoff offer is not active".into(),
            ));
        }
        if offer.work_id != request.work_id || offer.to != request.to {
            return Err(StoreError::InvalidWork(
                "handoff offer does not match this work or destination".into(),
            ));
        }
        if offer.expires_at <= request.accepted_at {
            expire_handoff_offers(
                &transaction,
                offer.run_id,
                request.accepted_at,
                &request.actor,
            )?;
            transaction.commit()?;
            return Err(StoreError::InvalidWork("handoff offer has expired".into()));
        }
        let (item, mut run, mut claim) = validate_live_claim_on(
            &transaction,
            offer.work_id,
            offer.run_id,
            offer.work_revision,
            &offer.from,
            offer.claim_id,
            offer.claim_fence,
            request.accepted_at,
            true,
        )?;
        claim.holder = request.to.clone();
        claim.fence += 1;
        claim.revision += 1;
        claim.expires_at = claim_expiry(request.accepted_at, offer.accepted_ttl_seconds)?;
        run.executor = Some(request.to.clone());
        run.state = WorkRunState::Active;
        run.revision += 1;
        run.updated_at = request.accepted_at;
        persist_claim(&transaction, &claim)?;
        persist_work_run(&transaction, &run, claim.fence)?;
        let mut root_execution = load_root_execution(&transaction, run.root_execution_id)?;
        if expect_root_contributor(&mut root_execution, &request.to) {
            root_execution.revision += 1;
            root_execution.updated_at = request.accepted_at;
            persist_root_execution(&transaction, &root_execution)?;
        }
        offer.state = WorkHandoffState::Accepted;
        let accepted_offer_object = CanonicalObject::freeze(&offer)?;
        SqliteStore::insert_object(&transaction, "work_handoff_offer", &accepted_offer_object)?;
        transaction.execute(
            "UPDATE work_handoff_offers
             SET state = 'accepted', offer_hash = ?2, offer_json = ?3
             WHERE offer_id = ?1",
            params![
                offer.offer_id.0.to_string(),
                accepted_offer_object.hash().as_str(),
                serde_json::to_vec(&offer)?
            ],
        )?;
        let event = WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: Some(run.run_id),
            revision: item.revision,
            work: item.clone(),
            run: Some(run.clone()),
            root_execution: Some(root_execution),
            claim: Some(claim.clone()),
            handoff_offer: Some(offer.clone()),
            blocker: None,
            transition: WorkTransition::HandedOff {
                offer_id: offer.offer_id,
                claim_id: claim.claim_id,
                from: offer.from,
                to: claim.holder.clone(),
                fence: claim.fence,
                checkpoint: offer.checkpoint,
                offer: accepted_offer_object.hash().clone(),
            },
            actor: request.actor.clone(),
            created_at: request.accepted_at,
        };
        append_work_event(&transaction, &event)?;
        persist_operation_result(
            &transaction,
            "accept_work_handoff",
            &request.idempotency_key,
            request_object.hash(),
            &claim,
        )?;
        transaction.commit()?;
        Ok(claim)
    }

    /// Cancels an unaccepted handoff while retaining the outgoing claim.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the offer, claim, holder, fence, or revision
    /// basis is stale, the offer expired, or persistence fails.
    pub fn cancel_work_handoff<R: Redactor>(
        &mut self,
        request: &CancelWorkHandoffRequest,
        redactor: &R,
    ) -> Result<WorkHandoffOffer, StoreError> {
        inspect_work_request(redactor, request, &request.actor)?;
        assert_actor_session(&request.actor, &request.holder)?;
        let reason = normalize_text(&request.reason, "handoff cancellation reason")?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(offer) = replay_operation::<WorkHandoffOffer>(
            &transaction,
            "cancel_work_handoff",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(offer);
        }
        let offer_row: Option<(Option<String>, Vec<u8>)> = transaction
            .query_row(
                "SELECT offer_hash, offer_json FROM work_handoff_offers
                 WHERE offer_id = ?1",
                [request.offer_id.0.to_string()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let mut offer = offer_row
            .map(|row| load_handoff_offer_projection(&transaction, row))
            .transpose()?
            .ok_or_else(|| StoreError::InvalidWork("handoff offer does not exist".into()))?;
        if offer.state != WorkHandoffState::Offered
            || offer.work_id != request.work_id
            || offer.run_id != request.run_id
            || offer.claim_id != request.claim_id
            || offer.claim_fence != request.claim_fence
            || offer.from != request.holder
        {
            return Err(StoreError::InvalidWork(
                "handoff offer does not match the live outgoing authority basis".into(),
            ));
        }
        if offer.expires_at <= request.cancelled_at {
            expire_handoff_offers(
                &transaction,
                offer.run_id,
                request.cancelled_at,
                &request.actor,
            )?;
            transaction.commit()?;
            return Err(StoreError::InvalidWork("handoff offer has expired".into()));
        }
        let (item, run, mut claim) = validate_live_claim_on(
            &transaction,
            request.work_id,
            request.run_id,
            request.expected_work_revision,
            &request.holder,
            request.claim_id,
            request.claim_fence,
            request.cancelled_at,
            true,
        )?;
        renew_holder_claim(&transaction, &mut claim, request.cancelled_at)?;
        offer.state = WorkHandoffState::Cancelled;
        let offer_object = CanonicalObject::freeze(&offer)?;
        SqliteStore::insert_object(&transaction, "work_handoff_offer", &offer_object)?;
        transaction.execute(
            "UPDATE work_handoff_offers
             SET state = 'cancelled', offer_hash = ?2, offer_json = ?3
             WHERE offer_id = ?1 AND state = 'offered'",
            params![
                offer.offer_id.0.to_string(),
                offer_object.hash().as_str(),
                serde_json::to_vec(&offer)?
            ],
        )?;
        let root_execution = load_root_execution(&transaction, run.root_execution_id)?;
        let event = WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: Some(run.run_id),
            revision: item.revision,
            work: item,
            run: Some(run),
            root_execution: Some(root_execution),
            claim: Some(claim),
            handoff_offer: Some(offer.clone()),
            blocker: None,
            transition: WorkTransition::HandoffCancelled {
                offer_id: offer.offer_id,
                offer: offer_object.hash().clone(),
                reason,
            },
            actor: request.actor.clone(),
            created_at: request.cancelled_at,
        };
        append_work_event(&transaction, &event)?;
        persist_operation_result(
            &transaction,
            "cancel_work_handoff",
            &request.idempotency_key,
            request_object.hash(),
            &offer,
        )?;
        transaction.commit()?;
        Ok(offer)
    }
}

pub(super) fn work_run_evidence_on(
    connection: &Connection,
    run_id: WorkRunId,
) -> Result<Vec<ObjectHash>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT evidence_hash FROM work_run_evidence
         WHERE run_id = ?1 ORDER BY evidence_hash",
    )?;
    statement
        .query_map([run_id.0.to_string()], |row| row.get::<_, String>(0))?
        .map(|row| {
            let value = row?;
            ObjectHash::from_stored(value.clone()).ok_or(StoreError::InvalidStoredHash(value))
        })
        .collect()
}

#[allow(
    clippy::too_many_arguments,
    reason = "the checkpoint object and event retain the complete exact claim basis"
)]
fn persist_work_checkpoint_on(
    transaction: &Transaction<'_>,
    item: &WorkItem,
    mut run: WorkRun,
    claim: WorkClaim,
    summary: String,
    evidence: Vec<ObjectHash>,
    actor: &crate::domain::ActorContext,
    checkpointed_at: DateTime<Utc>,
) -> Result<ObjectHash, StoreError> {
    validate_evidence_phase_marker(WorkLifecycle::Open, actor)?;
    let acknowledged_run_position = FeedPosition {
        feed: FeedId::RunExecution(run.run_id),
        position: feed_head(transaction, &FeedId::RunExecution(run.run_id))?,
    };
    let checkpoint = WorkCheckpoint {
        schema_version: SCHEMA_VERSION,
        work_id: item.work_id,
        run_id: run.run_id,
        claim_id: claim.claim_id,
        claim_fence: claim.fence,
        acknowledged_run_position,
        summary,
        evidence,
        actor: actor.clone(),
        created_at: checkpointed_at,
    };
    let object = CanonicalObject::freeze(&checkpoint)?;
    SqliteStore::insert_object(transaction, "work_checkpoint", &object)?;
    append_to_work_feeds(
        transaction,
        &item.project_id,
        item.root_id,
        Some(run.run_id),
        None,
        "work_checkpoint",
        &object,
    )?;
    run.last_checkpoint = Some(object.hash().clone());
    run.state = WorkRunState::Active;
    run.revision += 1;
    run.updated_at = checkpointed_at;
    persist_work_run(transaction, &run, claim.fence)?;
    let mut root_execution = load_root_execution(transaction, run.root_execution_id)?;
    let root_changed = expect_root_contributor(&mut root_execution, &claim.holder)
        | add_root_contribution(&mut root_execution, &claim.holder, object.hash());
    if root_changed {
        root_execution.revision += 1;
        root_execution.updated_at = checkpointed_at;
        persist_root_execution(transaction, &root_execution)?;
    }
    append_work_event(
        transaction,
        &WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: Some(run.run_id),
            revision: item.revision,
            work: item.clone(),
            run: Some(run),
            root_execution: Some(root_execution),
            claim: Some(claim),
            handoff_offer: None,
            blocker: None,
            transition: WorkTransition::Checkpointed {
                checkpoint: object.hash().clone(),
            },
            actor: actor.clone(),
            created_at: checkpointed_at,
        },
    )?;
    Ok(object.hash().clone())
}

fn persist_work_evidence_on(
    transaction: &Transaction<'_>,
    item: &WorkItem,
    run: &WorkRun,
    claim: WorkClaim,
    evidence: &WorkEvidence,
) -> Result<ObjectHash, StoreError> {
    validate_evidence_phase_marker(WorkLifecycle::Open, &evidence.actor)?;
    let object = CanonicalObject::freeze(evidence)?;
    SqliteStore::insert_object(transaction, "work_evidence", &object)?;
    transaction.execute(
        "INSERT INTO work_run_evidence (evidence_hash, work_id, run_id)
         VALUES (?1, ?2, ?3)",
        params![
            object.hash().as_str(),
            item.work_id.0.to_string(),
            run.run_id.0.to_string()
        ],
    )?;
    append_to_work_feeds(
        transaction,
        &item.project_id,
        item.root_id,
        Some(run.run_id),
        None,
        "work_evidence",
        &object,
    )?;
    let mut root_execution = load_root_execution(transaction, run.root_execution_id)?;
    let root_changed = expect_root_contributor(&mut root_execution, &claim.holder)
        | add_root_contribution(&mut root_execution, &claim.holder, object.hash());
    if root_changed {
        root_execution.revision += 1;
        root_execution.updated_at = evidence.created_at;
        persist_root_execution(transaction, &root_execution)?;
    }
    append_work_event(
        transaction,
        &WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: Some(run.run_id),
            revision: item.revision,
            work: item.clone(),
            run: Some(run.clone()),
            root_execution: Some(root_execution),
            claim: Some(claim),
            handoff_offer: None,
            blocker: None,
            transition: WorkTransition::EvidenceAdded {
                evidence: object.hash().clone(),
            },
            actor: evidence.actor.clone(),
            created_at: evidence.created_at,
        },
    )?;
    Ok(object.hash().clone())
}

fn append_gate_evidence_on(
    transaction: &Transaction<'_>,
    request: &RecordGateEvidenceRequest,
    name: String,
    failed: Vec<String>,
    refs: Vec<String>,
    previous: Option<ObjectHash>,
) -> Result<ObjectHash, StoreError> {
    let item = load_work_item(transaction, request.work_id)?;
    if item.lifecycle == WorkLifecycle::Completed {
        let (item, run, claim) = validate_post_completion_evidence_basis_on(
            transaction,
            item,
            request.run_id,
            request.expected_work_revision,
            request.claim_id,
            request.claim_fence,
            request.recorded_at,
        )?;
        let evidence = WorkEvidence {
            schema_version: SCHEMA_VERSION,
            work_id: item.work_id,
            run_id: run.run_id,
            claim_id: request.claim_id,
            claim_fence: request.claim_fence,
            summary: GATE_EVIDENCE_SUMMARY.into(),
            refs,
            gate: Some(GateEvidenceRecord {
                schema_version: SCHEMA_VERSION,
                name,
                passed: failed.is_empty(),
                failed,
                previous,
            }),
            actor: request.actor.clone(),
            created_at: request.recorded_at,
        };
        return persist_post_completion_work_evidence_on(
            transaction,
            &item,
            &run,
            claim,
            &evidence,
        );
    }
    expire_handoff_offers(
        transaction,
        request.run_id,
        request.recorded_at,
        &request.actor,
    )?;
    let (item, run, mut claim) = validate_live_claim_for_item_on(
        transaction,
        item,
        request.run_id,
        request.expected_work_revision,
        &request.holder,
        request.claim_id,
        request.claim_fence,
        request.recorded_at,
        false,
    )?;
    let gate = GateEvidenceRecord {
        schema_version: SCHEMA_VERSION,
        name,
        passed: failed.is_empty(),
        failed,
        previous,
    };
    renew_holder_claim(transaction, &mut claim, request.recorded_at)?;
    let evidence = WorkEvidence {
        schema_version: SCHEMA_VERSION,
        work_id: item.work_id,
        run_id: run.run_id,
        claim_id: claim.claim_id,
        claim_fence: claim.fence,
        summary: GATE_EVIDENCE_SUMMARY.into(),
        refs,
        gate: Some(gate),
        actor: request.actor.clone(),
        created_at: request.recorded_at,
    };
    persist_work_evidence_on(transaction, &item, &run, claim, &evidence)
}

fn post_completion_evidence_marker_count(actor: &ActorContext) -> usize {
    actor
        .provenance_chain
        .iter()
        .filter(|link| {
            link.relation == crate::domain::ProvenanceRelation::DerivedFrom
                && link.source == POST_COMPLETION_EVIDENCE_PROVENANCE_SOURCE
                && link.reference.as_deref() == Some(POST_COMPLETION_EVIDENCE_PROVENANCE_REFERENCE)
        })
        .count()
}

pub(super) fn validate_evidence_phase_marker(
    lifecycle: WorkLifecycle,
    actor: &ActorContext,
) -> Result<(), StoreError> {
    let marker_count = post_completion_evidence_marker_count(actor);
    match lifecycle {
        WorkLifecycle::Completed if marker_count == 1 => Ok(()),
        WorkLifecycle::Completed => Err(StoreError::InvalidWorkProjection(
            "post-completion evidence must carry exactly one late-finding provenance marker".into(),
        )),
        WorkLifecycle::Proposed
        | WorkLifecycle::Open
        | WorkLifecycle::Cancelled
        | WorkLifecycle::Superseded
            if marker_count == 0 =>
        {
            Ok(())
        }
        WorkLifecycle::Proposed
        | WorkLifecycle::Open
        | WorkLifecycle::Cancelled
        | WorkLifecycle::Superseded => Err(StoreError::InvalidWorkProjection(
            "late-finding provenance is reserved for evidence on completed work".into(),
        )),
    }
}

pub(super) fn validate_work_evidence_event_phase_on(
    connection: &Connection,
    evidence_hash: &ObjectHash,
    evidence: &WorkEvidence,
    event: &WorkEvent,
) -> Result<(), StoreError> {
    validate_evidence_phase_marker(event.work.lifecycle, &evidence.actor)?;
    if event.work.lifecycle != WorkLifecycle::Completed {
        return Ok(());
    }
    let run = event.run.as_ref().ok_or_else(|| {
        StoreError::InvalidWorkProjection(
            "post-completion evidence event has no completed run".into(),
        )
    })?;
    let claim = event.claim.as_ref().ok_or_else(|| {
        StoreError::InvalidWorkProjection(
            "post-completion evidence event has no historical claim".into(),
        )
    })?;
    let seal_hash = run.completion_seal.as_ref().ok_or_else(|| {
        StoreError::InvalidWorkProjection(
            "post-completion evidence event run has no completion seal".into(),
        )
    })?;
    let seal: CompletionSeal = load_typed_work_object(connection, seal_hash, "completion_seal")?;
    let evidence_position = run_feed_position_for_object_on(connection, run.run_id, evidence_hash)?;
    let completed_claim_fence = seal.claim_fence.checked_add(1).ok_or_else(|| {
        StoreError::InvalidWorkProjection(
            "completed claim fence overflowed its sealed basis".into(),
        )
    })?;
    let bound = event.work.active_run_id.is_none()
        && run.work_id == event.work_id
        && run.state == WorkRunState::Completed
        && run.completion_seal.as_ref() == Some(seal_hash)
        && claim.work_id == event.work_id
        && claim.run_id == run.run_id
        && claim.claim_id == seal.claim_id
        && claim.state == WorkClaimState::Completed
        && claim.fence == completed_claim_fence
        && evidence.claim_id == seal.claim_id
        && evidence.claim_fence == seal.claim_fence
        && evidence.created_at >= seal.completed_at
        && event.created_at == evidence.created_at
        && event.actor == evidence.actor
        && seal.work_id == event.work_id
        && seal.run_id == run.run_id
        && seal.completion_cut.feed == FeedId::RunExecution(run.run_id)
        && evidence_position.position > seal.completion_cut.position;
    if bound {
        Ok(())
    } else {
        Err(StoreError::InvalidWorkProjection(
            "post-completion evidence event does not bind the frozen completion basis".into(),
        ))
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the completed evidence basis is an exact immutable work/run/seal/claim cut"
)]
fn validate_post_completion_evidence_basis_on(
    connection: &Connection,
    item: WorkItem,
    run_id: WorkRunId,
    expected_work_revision: i64,
    claim_id: WorkClaimId,
    claim_fence: i64,
    recorded_at: DateTime<Utc>,
) -> Result<(WorkItem, WorkRun, WorkClaim), StoreError> {
    assert_revision(&item, expected_work_revision)?;
    if item.lifecycle != WorkLifecycle::Completed || item.active_run_id.is_some() {
        return Err(StoreError::WorkNotOpen(item.work_id));
    }
    let run = load_work_run(connection, run_id)?;
    if run.work_id != item.work_id || run.state != WorkRunState::Completed {
        return Err(StoreError::InvalidWorkProjection(
            "post-completion evidence does not bind the completed work run".into(),
        ));
    }
    let seal_hash = run.completion_seal.as_ref().ok_or_else(|| {
        StoreError::InvalidWorkProjection(
            "post-completion evidence run has no completion seal".into(),
        )
    })?;
    let seal: CompletionSeal = load_typed_work_object(connection, seal_hash, "completion_seal")?;
    if seal.work_id != item.work_id || seal.run_id != run.run_id {
        return Err(StoreError::InvalidWorkProjection(
            "post-completion evidence seal crosses its work or run binding".into(),
        ));
    }
    if seal.claim_id != claim_id || seal.claim_fence != claim_fence {
        return Err(StoreError::InvalidWorkProjection(
            "post-completion evidence does not bind the sealed claim".into(),
        ));
    }
    if seal.completion_cut.feed != FeedId::RunExecution(run.run_id) {
        return Err(StoreError::InvalidWorkProjection(
            "post-completion evidence seal has the wrong completion feed".into(),
        ));
    }
    if recorded_at < seal.completed_at {
        return Err(StoreError::InvalidWork(
            "post-completion evidence cannot precede the completed item".into(),
        ));
    }
    let claim = load_work_claim_optional(connection, run.run_id)?.ok_or_else(|| {
        StoreError::InvalidWorkProjection(
            "post-completion evidence run has no historical claim".into(),
        )
    })?;
    if claim.work_id != item.work_id
        || claim.run_id != run.run_id
        || claim.claim_id != seal.claim_id
        || claim.state != WorkClaimState::Completed
        || claim.fence
            != seal.claim_fence.checked_add(1).ok_or_else(|| {
                StoreError::InvalidWorkProjection(
                    "completed claim fence overflowed its sealed basis".into(),
                )
            })?
    {
        return Err(StoreError::InvalidWorkProjection(
            "post-completion evidence does not bind the historical completed claim".into(),
        ));
    }
    Ok((item, run, claim))
}

fn persist_post_completion_work_evidence_on(
    transaction: &Transaction<'_>,
    item: &WorkItem,
    run: &WorkRun,
    claim: WorkClaim,
    evidence: &WorkEvidence,
) -> Result<ObjectHash, StoreError> {
    validate_evidence_phase_marker(WorkLifecycle::Completed, &evidence.actor)?;
    let object = CanonicalObject::freeze(evidence)?;
    SqliteStore::insert_object(transaction, "work_evidence", &object)?;
    transaction.execute(
        "INSERT INTO work_run_evidence (evidence_hash, work_id, run_id)
         VALUES (?1, ?2, ?3)",
        params![
            object.hash().as_str(),
            item.work_id.0.to_string(),
            run.run_id.0.to_string()
        ],
    )?;
    append_to_work_feeds(
        transaction,
        &item.project_id,
        item.root_id,
        Some(run.run_id),
        None,
        "work_evidence",
        &object,
    )?;
    let root_execution = load_root_execution(transaction, run.root_execution_id)?;
    append_work_event(
        transaction,
        &WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: Some(run.run_id),
            revision: item.revision,
            work: item.clone(),
            run: Some(run.clone()),
            root_execution: Some(root_execution),
            claim: Some(claim),
            handoff_offer: None,
            blocker: None,
            transition: WorkTransition::EvidenceAdded {
                evidence: object.hash().clone(),
            },
            actor: evidence.actor.clone(),
            created_at: evidence.created_at,
        },
    )?;
    Ok(object.hash().clone())
}

const LATEST_GATE_EVIDENCE_SQL: &str = "SELECT entry.object_hash
     FROM work_feed_entries entry
     WHERE entry.feed_kind = 'run_execution'
       AND entry.feed_id = ?1
       AND entry.object_kind = 'work_evidence'
       AND entry.position = (
           SELECT MAX(candidate.position)
           FROM objects object INDEXED BY objects_work_evidence_gate_name
           JOIN work_run_evidence evidence
             ON evidence.run_id = ?1
            AND evidence.evidence_hash = object.object_hash
           JOIN work_feed_entries candidate
             ON candidate.feed_kind = 'run_execution'
            AND candidate.feed_id = evidence.run_id
            AND candidate.object_hash = evidence.evidence_hash
           WHERE object.object_kind = 'work_evidence'
             AND json_extract(object.canonical_json, '$.run_id') = ?1
             AND json_type(object.canonical_json, '$.gate') = 'object'
             AND json_extract(object.canonical_json, '$.gate.name') = ?2
       )";

fn latest_gate_evidence_on(
    connection: &Connection,
    run_id: WorkRunId,
    name: &str,
) -> Result<Option<(ObjectHash, WorkEvidence)>, StoreError> {
    // The canonical run feed is the sole source of the previous transition.
    // The rebuildable expression index narrows candidates, but a mutable head
    // must never redirect an immutable `previous` link.
    let stored = connection
        .query_row(
            LATEST_GATE_EVIDENCE_SQL,
            params![run_id.0.to_string(), name],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    stored
        .map(|stored| {
            let hash = ObjectHash::from_stored(stored.clone())
                .ok_or(StoreError::InvalidStoredHash(stored))?;
            let evidence =
                load_typed_work_object::<WorkEvidence>(connection, &hash, "work_evidence")?;
            let gate = evidence.gate.as_ref().ok_or_else(|| {
                StoreError::InvalidWorkProjection(format!(
                    "gate evidence {hash} has no typed gate payload"
                ))
            })?;
            if evidence.run_id != run_id || gate.name != name {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "gate evidence {hash} disagrees with its indexed identity"
                )));
            }
            validate_gate_evidence(&hash, &evidence)?;
            Ok((hash, evidence))
        })
        .transpose()
}

fn validate_gate_evidence(
    evidence_hash: &ObjectHash,
    evidence: &WorkEvidence,
) -> Result<(), StoreError> {
    validate_gate_evidence_payload(evidence).map_err(|detail| {
        StoreError::InvalidWorkProjection(format!(
            "gate evidence {evidence_hash} has an invalid typed payload: {detail}"
        ))
    })
}

pub(super) fn validate_gate_evidence_chain(
    evidence_hash: &ObjectHash,
    evidence: &WorkEvidence,
    gate_heads: &mut HashMap<(WorkRunId, String), ObjectHash>,
) -> Result<(), StoreError> {
    validate_gate_evidence(evidence_hash, evidence)?;
    let Some(gate) = &evidence.gate else {
        return Ok(());
    };
    let key = (evidence.run_id, gate.name.clone());
    if gate.previous.as_ref() != gate_heads.get(&key) {
        return Err(StoreError::InvalidWorkProjection(format!(
            "gate evidence {evidence_hash} does not name the prior same-run, same-name observation"
        )));
    }
    gate_heads.insert(key, evidence_hash.clone());
    Ok(())
}

fn gate_observation_matches(
    evidence: &WorkEvidence,
    request: &RecordGateEvidenceRequest,
    name: &str,
    failed: &[String],
    refs: &[String],
) -> bool {
    evidence.work_id == request.work_id
        && evidence.run_id == request.run_id
        && evidence.claim_id == request.claim_id
        && evidence.claim_fence == request.claim_fence
        && actor_matches_without_optional_context(&evidence.actor, &request.actor)
        && evidence.refs == refs
        && evidence.gate.as_ref().is_some_and(|gate| {
            gate.schema_version == SCHEMA_VERSION
                && gate.name == name
                && gate.passed == failed.is_empty()
                && gate.failed == failed
        })
}

fn actor_without_optional_context(actor: &ActorContext) -> ActorContext {
    let mut identity = actor.clone();
    identity
        .provenance_chain
        .retain(|link| !is_optional_actor_context_link(link.reference.as_deref()));
    identity
}

fn actor_matches_without_optional_context(left: &ActorContext, right: &ActorContext) -> bool {
    actor_without_optional_context(left) == actor_without_optional_context(right)
}

fn is_optional_actor_context_link(reference: Option<&str>) -> bool {
    matches!(
        reference,
        Some(
            crate::domain::ACTOR_CONTEXT_PROVENANCE_REFERENCE
                | crate::domain::ACTOR_CONTEXT_NORMALIZED_REFERENCE
        )
    )
}

pub(super) fn work_run_evidence_projection_on(
    connection: &Connection,
    run_id: WorkRunId,
    required_environments: &[ObjectHash],
    limit: usize,
) -> Result<Vec<WorkEvidenceProjectionSummary>, StoreError> {
    let mut selected =
        load_work_evidence_selection_rows_on(connection, run_id, required_environments, limit)?;
    for required in required_environments {
        if let Some(candidate) = selected
            .iter()
            .find(|candidate| candidate.hash == *required)
            && candidate.kind != WorkEvidenceKind::Environment
        {
            return Err(StoreError::InvalidWorkProjection(format!(
                "required focus environment {required} is not typed environment evidence for run {}",
                run_id.0
            )));
        }
    }
    let selected_hashes = selected
        .iter()
        .map(|candidate| candidate.hash.clone())
        .collect::<HashSet<_>>();
    let linked_environments = selected
        .iter()
        .filter_map(|candidate| candidate.environment.clone())
        .filter(|hash| !selected_hashes.contains(hash))
        .collect::<HashSet<_>>()
        .into_iter()
        .collect::<Vec<_>>();
    if !linked_environments.is_empty() {
        let closure =
            load_work_evidence_selection_rows_on(connection, run_id, &linked_environments, 0)?;
        for environment in &linked_environments {
            if !closure.iter().any(|candidate| {
                candidate.hash == *environment && candidate.kind == WorkEvidenceKind::Environment
            }) {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "verification evidence names non-environment evidence {environment} for run {}",
                    run_id.0
                )));
            }
        }
        selected.extend(closure);
    }
    selected.sort_by(|left, right| left.hash.as_str().cmp(right.hash.as_str()));
    selected.dedup_by(|left, right| left.hash == right.hash);
    Ok(selected)
}

fn load_work_evidence_selection_rows_on(
    connection: &Connection,
    run_id: WorkRunId,
    required: &[ObjectHash],
    limit: usize,
) -> Result<Vec<WorkEvidenceProjectionSummary>, StoreError> {
    let required_json =
        serde_json::to_string(&required.iter().map(ObjectHash::as_str).collect::<Vec<_>>())?;
    let limit = i64::try_from(limit).map_err(|_| {
        StoreError::InvalidWorkProjection("focus evidence limit does not fit SQLite".into())
    })?;
    let mut statement = connection.prepare(
        "WITH recent(evidence_hash) AS (
             SELECT evidence_hash FROM work_run_evidence
             WHERE run_id = ?1 ORDER BY evidence_hash DESC LIMIT ?2
         ), requested(evidence_hash) AS (
             SELECT value FROM json_each(?3)
         ), candidates(evidence_hash) AS (
             SELECT evidence_hash FROM recent
             UNION
             SELECT evidence_hash FROM requested
         )
         SELECT projection.evidence_hash, projection.work_id, projection.run_id,
                projection.evidence_kind, projection.environment_evidence_hash,
                object.object_kind, object.canonical_json
         FROM candidates candidate
         JOIN work_run_evidence projection
           ON projection.run_id = ?1
          AND projection.evidence_hash = candidate.evidence_hash
         LEFT JOIN objects object ON object.object_hash = projection.evidence_hash
         ORDER BY projection.evidence_hash",
    )?;
    let rows = statement
        .query_map(params![run_id.0.to_string(), limit, required_json], |row| {
            Ok(StoredWorkEvidenceSelectionRow {
                hash: row.get(0)?,
                projected_work: row.get(1)?,
                projected_run: row.get(2)?,
                projected_kind: row.get(3)?,
                projected_environment: row.get(4)?,
                object_kind: row.get(5)?,
                canonical_json: row.get(6)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(|row| {
            let hash = ObjectHash::from_stored(row.hash.clone())
                .ok_or(StoreError::InvalidStoredHash(row.hash))?;
            let object_kind = row.object_kind.ok_or_else(|| {
                StoreError::InvalidWorkProjection(format!(
                    "run evidence projection names missing object {hash}"
                ))
            })?;
            let bytes = row.canonical_json.ok_or_else(|| {
                StoreError::InvalidWorkProjection(format!(
                    "run evidence projection names missing canonical bytes {hash}"
                ))
            })?;
            let object = CanonicalObject::verify(&hash, bytes)?;
            let (kind, environment, canonical_work, canonical_run) = match object_kind.as_str() {
                "work_evidence" => {
                    let evidence: WorkEvidence = object.decode()?;
                    if evidence.schema_version != SCHEMA_VERSION {
                        return Err(StoreError::InvalidWorkProjection(format!(
                            "generic evidence {hash} has an unsupported schema"
                        )));
                    }
                    (
                        WorkEvidenceKind::Generic,
                        None,
                        evidence.work_id,
                        evidence.run_id,
                    )
                }
                "verification_evidence" => {
                    let evidence: VerificationEvidence = object.decode()?;
                    if evidence.schema_version != SCHEMA_VERSION {
                        return Err(StoreError::InvalidWorkProjection(format!(
                            "verification evidence {hash} has an unsupported schema"
                        )));
                    }
                    (
                        WorkEvidenceKind::Verification,
                        evidence.environment,
                        evidence.binding.work_id,
                        evidence.binding.run_id,
                    )
                }
                "environment_evidence" => {
                    let evidence: EnvironmentEvidence = object.decode()?;
                    if evidence.schema_version != SCHEMA_VERSION {
                        return Err(StoreError::InvalidWorkProjection(format!(
                            "environment evidence {hash} has an unsupported schema"
                        )));
                    }
                    (
                        WorkEvidenceKind::Environment,
                        None,
                        evidence.binding.work_id,
                        evidence.binding.run_id,
                    )
                }
                other => {
                    return Err(StoreError::InvalidWorkProjection(format!(
                        "run evidence object {hash} has unsupported kind {other:?}"
                    )));
                }
            };
            let projected_environment = row
                .projected_environment
                .map(|stored| {
                    ObjectHash::from_stored(stored.clone())
                        .ok_or(StoreError::InvalidStoredHash(stored))
                })
                .transpose()?;
            let expected_kind = match kind {
                WorkEvidenceKind::Generic => "generic",
                WorkEvidenceKind::Verification => "verification",
                WorkEvidenceKind::Environment => "environment",
            };
            if row.projected_kind != expected_kind
                || projected_environment != environment
                || row.projected_work != canonical_work.0.to_string()
                || row.projected_run != canonical_run.0.to_string()
                || canonical_run != run_id
            {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "evidence object {hash} disagrees with its selection projection"
                )));
            }
            Ok(WorkEvidenceProjectionSummary {
                hash,
                kind,
                environment,
            })
        })
        .collect()
}

pub(super) fn work_evidence_kind_on(
    connection: &Connection,
    run_id: WorkRunId,
    evidence_hash: &ObjectHash,
) -> Result<WorkEvidenceKind, StoreError> {
    let projected = connection
        .query_row(
            "SELECT work_id, run_id, evidence_kind,
                    workspace_id, source_revision, producer_session_id,
                    producer_observation_hash, check_fingerprint,
                    verification_result, observed_at_ms, environment_fingerprint,
                    environment_evidence_hash, components_json
             FROM work_run_evidence
             WHERE run_id = ?1 AND evidence_hash = ?2",
            params![run_id.0.to_string(), evidence_hash.as_str()],
            |row| {
                Ok(EvidenceProjectionRow {
                    work_id: row.get(0)?,
                    run_id: row.get(1)?,
                    evidence_kind: row.get(2)?,
                    workspace_id: row.get(3)?,
                    source_revision: row.get(4)?,
                    producer_session_id: row.get(5)?,
                    producer_observation_hash: row.get(6)?,
                    check_fingerprint: row.get(7)?,
                    verification_result: row.get(8)?,
                    observed_at_ms: row.get(9)?,
                    environment_fingerprint: row.get(10)?,
                    environment_evidence_hash: row.get(11)?,
                    components_json: row.get(12)?,
                })
            },
        )
        .optional()?
        .ok_or_else(|| {
            StoreError::InvalidWork(format!(
                "evidence object {evidence_hash} does not belong to run {run_id:?}"
            ))
        })?;
    let (kind, expected) = match projected.evidence_kind.as_str() {
        "generic" => {
            let evidence =
                load_typed_work_object::<WorkEvidence>(connection, evidence_hash, "work_evidence")?;
            if evidence.schema_version != SCHEMA_VERSION {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "generic evidence {evidence_hash} has an unsupported schema"
                )));
            }
            validate_gate_evidence(evidence_hash, &evidence)?;
            (
                WorkEvidenceKind::Generic,
                EvidenceProjectionRow {
                    work_id: evidence.work_id.0.to_string(),
                    run_id: evidence.run_id.0.to_string(),
                    evidence_kind: "generic".into(),
                    workspace_id: None,
                    source_revision: None,
                    producer_session_id: None,
                    producer_observation_hash: None,
                    check_fingerprint: None,
                    verification_result: None,
                    observed_at_ms: None,
                    environment_fingerprint: None,
                    environment_evidence_hash: None,
                    components_json: None,
                },
            )
        }
        "verification" => (
            WorkEvidenceKind::Verification,
            expected_verification_projection(connection, evidence_hash)?,
        ),
        "environment" => (
            WorkEvidenceKind::Environment,
            expected_environment_projection(connection, evidence_hash)?,
        ),
        kind => {
            return Err(StoreError::InvalidWorkProjection(format!(
                "evidence object {evidence_hash} has unknown kind {kind:?}"
            )));
        }
    };
    if projected != expected {
        return Err(StoreError::InvalidWorkProjection(format!(
            "evidence object {evidence_hash} disagrees with its redundant run projection"
        )));
    }
    Ok(kind)
}

pub(super) fn ensure_run_evidence(
    connection: &Connection,
    run_id: WorkRunId,
    evidence: &[ObjectHash],
) -> Result<(), StoreError> {
    // Evidence attach establishes the indexed run-membership fact in the same
    // transaction as its canonical object. Lifecycle mutations trust that
    // projection; operator-invoked integrity verification checks agreement.
    let evidence = unique_hashes(evidence);
    if evidence.is_empty() {
        return Ok(());
    }
    let evidence_json =
        serde_json::to_string(&evidence.iter().map(ObjectHash::as_str).collect::<Vec<_>>())?;
    let missing = connection
        .query_row(
            "WITH requested(evidence_hash) AS (
                 SELECT value FROM json_each(?2)
             )
             SELECT requested.evidence_hash
             FROM requested
             LEFT JOIN work_run_evidence projection
               ON projection.run_id = ?1
              AND projection.evidence_hash = requested.evidence_hash
             WHERE projection.evidence_hash IS NULL
             ORDER BY requested.evidence_hash
             LIMIT 1",
            params![run_id.0.to_string(), evidence_json],
            |row| row.get::<_, String>(0),
        )
        .optional()?;
    if let Some(hash) = missing {
        return Err(StoreError::InvalidWork(format!(
            "evidence object {hash} does not belong to run {}",
            run_id.0
        )));
    }
    Ok(())
}
