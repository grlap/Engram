//! Evidence capture, phase validation, membership, and ordered selection.

use super::{
    ActorContext, CanonicalObject, CompletionSeal, Connection, DateTime, EnvironmentEvidence,
    EvidenceProjectionRow, FeedId, HashSet, ObjectId, OptionalExtension,
    POST_COMPLETION_EVIDENCE_PROVENANCE_REFERENCE, POST_COMPLETION_EVIDENCE_PROVENANCE_SOURCE,
    RecordWorkEvidenceRequest, RecordWorkNoteRequest, Redactor, SCHEMA_VERSION, SqliteStore,
    StoreError, StoredWorkEvidenceSelectionRow, Transaction, Utc, VerificationEvidence, WorkClaim,
    WorkClaimId, WorkClaimState, WorkEvent, WorkEventDraft, WorkEvidence, WorkEvidenceKind,
    WorkEvidenceProjectionSummary, WorkItem, WorkLifecycle, WorkNoteCapture, WorkRun, WorkRunId,
    WorkRunState, WorkTransition, add_root_contribution, append_to_work_feeds, append_work_event,
    assert_actor_session, assert_revision, expect_root_contributor,
    expected_environment_projection, expected_verification_projection, expire_handoff_offers,
    inspect_work_request, load_root_execution, load_typed_work_object, load_work_claim_optional,
    load_work_item, load_work_run, normalize_strings, params, persist_operation_result,
    persist_work_checkpoint_on, renew_holder_claim, replay_operation, request_object,
    run_feed_position_for_object_on, unique_hashes, validate_gate_evidence,
    validate_live_claim_for_item_on, validate_live_claim_on,
};

impl SqliteStore {
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
    ) -> Result<ObjectId, StoreError> {
        inspect_work_request(redactor, request, &request.actor)?;
        assert_actor_session(&request.actor, &request.holder)?;
        let summary = super::super::planning::normalize_note_text(
            &request.summary,
            crate::storage::refusal_labels::EVIDENCE_SUMMARY,
        )?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(hash) = replay_operation::<ObjectId>(
            &transaction,
            "record_work_evidence",
            &request.idempotency_key,
            request_object.key(),
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
        let evidence_id = persist_work_evidence_on(&transaction, &item, &run, claim, &evidence)?;
        persist_operation_result(
            &transaction,
            "record_work_evidence",
            &request.idempotency_key,
            request_object.key(),
            &evidence_id,
        )?;
        transaction.commit()?;
        Ok(evidence_id)
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
        let summary = super::super::planning::normalize_note_text(
            &request.summary,
            crate::storage::refusal_labels::NOTE_SUMMARY,
        )?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(capture) = replay_operation::<WorkNoteCapture>(
            &transaction,
            "record_work_note",
            &request.idempotency_key,
            request_object.key(),
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
                actor: crate::domain::captured_note_actor(
                    &request.actor,
                    request.status,
                    item.assigned_to.as_deref() == Some(request.actor.actor_id.as_str()),
                ),
                created_at: request.recorded_at,
            };
            let evidence_id = persist_post_completion_work_evidence_on(
                &transaction,
                &item,
                &run,
                claim,
                &evidence,
            )?;
            let capture = WorkNoteCapture {
                non_holder: false,
                evidence: evidence_id,
                checkpoint: None,
            };
            persist_operation_result(
                &transaction,
                "record_work_note",
                &request.idempotency_key,
                request_object.key(),
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
            actor: crate::domain::captured_note_actor(&request.actor, request.status, true),
            created_at: request.recorded_at,
        };
        let evidence_id =
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
            non_holder: false,
            evidence: evidence_id,
            checkpoint: Some(checkpoint_hash),
        };
        persist_operation_result(
            &transaction,
            "record_work_note",
            &request.idempotency_key,
            request_object.key(),
            &capture,
        )?;
        transaction.commit()?;
        Ok(capture)
    }
}

pub(in crate::storage::work) fn work_run_evidence_on(
    connection: &Connection,
    run_id: WorkRunId,
) -> Result<Vec<ObjectId>, StoreError> {
    let mut statement = connection.prepare(
        "SELECT evidence_id FROM work_run_evidence
         WHERE run_id = ?1 ORDER BY evidence_id",
    )?;
    statement
        .query_map([run_id.0.to_string()], |row| row.get::<_, String>(0))?
        .map(|row| {
            let value = row?;
            ObjectId::from_stored(value.clone()).ok_or(StoreError::InvalidStoredKey(value))
        })
        .collect()
}

pub(super) fn persist_work_evidence_on(
    transaction: &Transaction<'_>,
    item: &WorkItem,
    run: &WorkRun,
    claim: WorkClaim,
    evidence: &WorkEvidence,
) -> Result<ObjectId, StoreError> {
    validate_evidence_phase_marker(WorkLifecycle::Open, &evidence.actor)?;
    let object = CanonicalObject::mint(evidence)?;
    SqliteStore::insert_object(transaction, "work_evidence", &object)?;
    transaction.execute(
        "INSERT INTO work_run_evidence (evidence_id, work_id, run_id)
         VALUES (?1, ?2, ?3)",
        params![
            object.key().as_str(),
            item.work_id.0.to_string(),
            run.run_id.0.to_string()
        ],
    )?;
    let positions = append_to_work_feeds(
        transaction,
        &item.project_id,
        item.root_id,
        Some(run.run_id),
        None,
        "work_evidence",
        &object,
    )?;
    super::super::note_index::append_native(
        transaction,
        &positions,
        object.key(),
        &super::super::note_index::NativeNote::Evidence(evidence),
    )?;
    let root_execution =
        super::super::root_state::update(transaction, run.root_execution_id, |root| {
            let changed = expect_root_contributor(root, &claim.holder)
                | add_root_contribution(root, &claim.holder, object.key());
            if changed {
                root.revision += 1;
                root.updated_at = evidence.created_at;
            }
        })?;
    super::super::feeds::append_work_event_with_root(
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
            root_execution: Some(root_execution.value().clone()),
            claim: Some(claim),
            handoff_offer: None,
            blocker: None,
            transition: WorkTransition::EvidenceAdded {
                evidence: object.key().clone(),
            },
            actor: evidence.actor.clone(),
            created_at: evidence.created_at,
        },
        &root_execution,
    )?;
    Ok(object.key().clone())
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

pub(in crate::storage::work) fn validate_evidence_phase_marker(
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

pub(in crate::storage::work) fn validate_work_evidence_event_phase_on(
    connection: &Connection,
    evidence_id: &ObjectId,
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
    let seal_id = run.completion_seal.as_ref().ok_or_else(|| {
        StoreError::InvalidWorkProjection(
            "post-completion evidence event run has no completion seal".into(),
        )
    })?;
    let seal: CompletionSeal = load_typed_work_object(connection, seal_id, "completion_seal")?;
    let evidence_position = run_feed_position_for_object_on(connection, run.run_id, evidence_id)?;
    let completed_claim_fence = seal.completed_claim_fence().ok_or_else(|| {
        StoreError::InvalidWorkProjection(
            "completed claim fence overflowed its sealed basis".into(),
        )
    })?;
    let bound = event.work.active_run_id.is_none()
        && run.work_id == event.work_id
        && run.state == WorkRunState::Completed
        && run.completion_seal.as_ref() == Some(seal_id)
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
pub(super) fn validate_post_completion_evidence_basis_on(
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
    let seal_id = run.completion_seal.as_ref().ok_or_else(|| {
        StoreError::InvalidWorkProjection(
            "post-completion evidence run has no completion seal".into(),
        )
    })?;
    let seal: CompletionSeal = load_typed_work_object(connection, seal_id, "completion_seal")?;
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
            != seal.completed_claim_fence().ok_or_else(|| {
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

pub(super) fn persist_post_completion_work_evidence_on(
    transaction: &Transaction<'_>,
    item: &WorkItem,
    run: &WorkRun,
    claim: WorkClaim,
    evidence: &WorkEvidence,
) -> Result<ObjectId, StoreError> {
    validate_evidence_phase_marker(WorkLifecycle::Completed, &evidence.actor)?;
    let object = CanonicalObject::mint(evidence)?;
    SqliteStore::insert_object(transaction, "work_evidence", &object)?;
    transaction.execute(
        "INSERT INTO work_run_evidence (evidence_id, work_id, run_id)
         VALUES (?1, ?2, ?3)",
        params![
            object.key().as_str(),
            item.work_id.0.to_string(),
            run.run_id.0.to_string()
        ],
    )?;
    let positions = append_to_work_feeds(
        transaction,
        &item.project_id,
        item.root_id,
        Some(run.run_id),
        None,
        "work_evidence",
        &object,
    )?;
    super::super::note_index::append_native(
        transaction,
        &positions,
        object.key(),
        &super::super::note_index::NativeNote::Evidence(evidence),
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
                evidence: object.key().clone(),
            },
            actor: evidence.actor.clone(),
            created_at: evidence.created_at,
        },
    )?;
    Ok(object.key().clone())
}

pub(in crate::storage::work) fn work_run_evidence_projection_on(
    connection: &Connection,
    run_id: WorkRunId,
    limit: usize,
) -> Result<Vec<WorkEvidenceProjectionSummary>, StoreError> {
    let mut selected = load_work_evidence_selection_rows_on(connection, run_id, &[], limit)?;
    let selected_hashes = selected
        .iter()
        .map(|candidate| candidate.hash.clone())
        .collect::<HashSet<_>>();
    // A verification bound from another item's check links the original's
    // environment, which lies on the original's run, not this one; doctor
    // holds it equal to the original's. Only this run's environments join
    // the closure.
    let mut linked_environments = HashSet::new();
    for candidate in &selected {
        let Some(environment) = candidate.environment.clone() else {
            continue;
        };
        if selected_hashes.contains(&environment)
            || bound_environment_elsewhere(connection, &candidate.hash, &environment)?
        {
            continue;
        }
        linked_environments.insert(environment);
    }
    let linked_environments = linked_environments.into_iter().collect::<Vec<_>>();
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
    // One record has one run-feed position, so equal positions are the same
    // record and adjacent after sorting.
    selected.sort_by_key(|candidate| candidate.run_position);
    selected.dedup_by(|left, right| left.hash == right.hash);
    Ok(selected)
}

/// Whether `verification` is a bound record whose linked `environment` is
/// the environment record on its original's run.
fn bound_environment_elsewhere(
    connection: &Connection,
    verification: &ObjectId,
    environment: &ObjectId,
) -> Result<bool, StoreError> {
    let kind: Option<String> = connection
        .query_row(
            "SELECT object_kind FROM objects WHERE object_id = ?1",
            [verification.as_str()],
            |row| row.get(0),
        )
        .optional()?;
    if kind.as_deref() != Some("verification_evidence") {
        return Ok(false);
    }
    let evidence = load_typed_work_object::<VerificationEvidence>(
        connection,
        verification,
        "verification_evidence",
    )?;
    let Some(source) = evidence.bound_from else {
        return Ok(false);
    };
    Ok(connection.query_row(
        "SELECT EXISTS(SELECT 1 FROM work_run_evidence
         WHERE run_id = ?1 AND evidence_id = ?2 AND evidence_kind = 'environment')",
        params![source.run_id.0.to_string(), environment.as_str()],
        |row| row.get(0),
    )?)
}

/// The run's `limit` newest evidence rows by dense run-execution feed
/// position, plus the rows named in `required`: the environment evidence the
/// selected verifications link to, wherever it lies. A projected row with no
/// run-feed entry sorts first so that its missing position is refused rather
/// than silently left out.
fn load_work_evidence_selection_rows_on(
    connection: &Connection,
    run_id: WorkRunId,
    required: &[ObjectId],
    limit: usize,
) -> Result<Vec<WorkEvidenceProjectionSummary>, StoreError> {
    let required_json =
        serde_json::to_string(&required.iter().map(ObjectId::as_str).collect::<Vec<_>>())?;
    let limit = i64::try_from(limit).map_err(|_| {
        StoreError::InvalidWorkProjection("focus evidence limit does not fit SQLite".into())
    })?;
    let mut statement = connection.prepare(
        "WITH bounded(evidence_id) AS (
             SELECT evidence.evidence_id FROM work_run_evidence evidence
             LEFT JOIN work_feed_entries entry
               ON entry.feed_kind = 'run_execution'
              AND entry.feed_id = ?1
              AND entry.object_id = evidence.evidence_id
             WHERE evidence.run_id = ?1
             ORDER BY entry.position DESC NULLS FIRST LIMIT ?2
         ), requested(evidence_id) AS (
             SELECT value FROM json_each(?3)
         ), candidates(evidence_id) AS (
             SELECT evidence_id FROM bounded
             UNION
             SELECT evidence_id FROM requested
         )
         SELECT projection.evidence_id, projection.work_id, projection.run_id,
                projection.evidence_kind, projection.environment_evidence_id,
                object.object_kind, object.canonical_json, entry.position
         FROM candidates candidate
         JOIN work_run_evidence projection
           ON projection.run_id = ?1
          AND projection.evidence_id = candidate.evidence_id
         LEFT JOIN objects object ON object.object_id = projection.evidence_id
         LEFT JOIN work_feed_entries entry
           ON entry.feed_kind = 'run_execution'
          AND entry.feed_id = ?1
          AND entry.object_id = projection.evidence_id
         ORDER BY entry.position",
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
                run_position: row.get(7)?,
            })
        })?
        .collect::<Result<Vec<_>, _>>()?;
    rows.into_iter()
        .map(|row| {
            let hash = ObjectId::from_stored(row.hash.clone())
                .ok_or(StoreError::InvalidStoredKey(row.hash))?;
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
            let object = CanonicalObject::stored(&hash, bytes)?;
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
                "imported_evidence_consumption" => {
                    let evidence: crate::task_sync::evidence::consumption::ImportedEvidenceConsumption = object.decode()?;
                    (
                        WorkEvidenceKind::Imported,
                        None,
                        evidence.request.binding.work_id,
                        evidence.request.binding.run_id,
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
                    ObjectId::from_stored(stored.clone())
                        .ok_or(StoreError::InvalidStoredKey(stored))
                })
                .transpose()?;
            let expected_kind = match kind {
                WorkEvidenceKind::Generic => "generic",
                WorkEvidenceKind::Verification => "verification",
                WorkEvidenceKind::Environment => "environment",
                WorkEvidenceKind::Imported => "imported",
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
            let run_position = row.run_position.ok_or_else(|| {
                StoreError::InvalidWorkProjection(format!(
                    "run evidence {hash} has no entry in its run-execution feed"
                ))
            })?;
            Ok(WorkEvidenceProjectionSummary {
                hash,
                kind,
                environment,
                run_position,
            })
        })
        .collect()
}

pub(in crate::storage::work) fn work_evidence_kind_on(
    connection: &Connection,
    run_id: WorkRunId,
    evidence_id: &ObjectId,
) -> Result<WorkEvidenceKind, StoreError> {
    let projected = connection
        .query_row(
            "SELECT work_id, run_id, evidence_kind,
                    workspace_id, source_revision, producer_session_id,
                    producer_observation_id, check_fingerprint,
                    verification_result, observed_at_ms, environment_fingerprint,
                    environment_evidence_id, components_json
             FROM work_run_evidence
             WHERE run_id = ?1 AND evidence_id = ?2",
            params![run_id.0.to_string(), evidence_id.as_str()],
            |row| {
                super::super::search_budget::record_row(connection, row)?;
                Ok(EvidenceProjectionRow {
                    work_id: row.get(0)?,
                    run_id: row.get(1)?,
                    evidence_kind: row.get(2)?,
                    workspace_id: row.get(3)?,
                    source_revision: row.get(4)?,
                    producer_session_id: row.get(5)?,
                    producer_observation_id: row.get(6)?,
                    check_fingerprint: row.get(7)?,
                    verification_result: row.get(8)?,
                    observed_at_ms: row.get(9)?,
                    environment_fingerprint: row.get(10)?,
                    environment_evidence_id: row.get(11)?,
                    components_json: row.get(12)?,
                })
            },
        )
        .optional()?
        .ok_or_else(|| {
            StoreError::InvalidWork(format!(
                "evidence object {evidence_id} does not belong to run {run_id:?}"
            ))
        })?;
    let (kind, expected) = match projected.evidence_kind.as_str() {
        "imported" => (
            WorkEvidenceKind::Imported,
            crate::storage::work::task_sync::evidence::consumption::expected_projection_on(
                connection,
                evidence_id,
            )?,
        ),
        "generic" => {
            let evidence =
                load_typed_work_object::<WorkEvidence>(connection, evidence_id, "work_evidence")?;
            if evidence.schema_version != SCHEMA_VERSION {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "generic evidence {evidence_id} has an unsupported schema"
                )));
            }
            validate_gate_evidence(evidence_id, &evidence)?;
            (
                WorkEvidenceKind::Generic,
                EvidenceProjectionRow {
                    work_id: evidence.work_id.0.to_string(),
                    run_id: evidence.run_id.0.to_string(),
                    evidence_kind: "generic".into(),
                    workspace_id: None,
                    source_revision: None,
                    producer_session_id: None,
                    producer_observation_id: None,
                    check_fingerprint: None,
                    verification_result: None,
                    observed_at_ms: None,
                    environment_fingerprint: None,
                    environment_evidence_id: None,
                    components_json: None,
                },
            )
        }
        "verification" => (
            WorkEvidenceKind::Verification,
            expected_verification_projection(connection, evidence_id)?,
        ),
        "environment" => (
            WorkEvidenceKind::Environment,
            expected_environment_projection(connection, evidence_id)?,
        ),
        kind => {
            return Err(StoreError::InvalidWorkProjection(format!(
                "evidence object {evidence_id} has unknown kind {kind:?}"
            )));
        }
    };
    if projected != expected {
        return Err(StoreError::InvalidWorkProjection(format!(
            "evidence object {evidence_id} disagrees with its redundant run projection"
        )));
    }
    Ok(kind)
}

pub(in crate::storage::work) fn ensure_run_evidence(
    connection: &Connection,
    run_id: WorkRunId,
    evidence: &[ObjectId],
) -> Result<(), StoreError> {
    // Evidence attach establishes the indexed run-membership fact in the same
    // transaction as its canonical object. Lifecycle mutations trust that
    // projection; operator-invoked integrity verification checks agreement.
    let evidence = unique_hashes(evidence);
    if evidence.is_empty() {
        return Ok(());
    }
    let evidence_json =
        serde_json::to_string(&evidence.iter().map(ObjectId::as_str).collect::<Vec<_>>())?;
    let missing = connection
        .query_row(
            "WITH requested(evidence_id) AS (
                 SELECT value FROM json_each(?2)
             )
             SELECT requested.evidence_id
             FROM requested
             LEFT JOIN work_run_evidence projection
               ON projection.run_id = ?1
              AND projection.evidence_id = requested.evidence_id
             WHERE projection.evidence_id IS NULL
             ORDER BY requested.evidence_id
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
