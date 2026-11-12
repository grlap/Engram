//! Restored execution initialization and late evidence.

use super::{
    ActorContext, AppendRestoredWorkGateRequest, CanonicalObject, Connection, DateTime,
    GATE_EVIDENCE_SUMMARY, GateEvidenceRecord, HashMap, HashSet, ObjectId,
    RecordRestoredWorkEvidenceRequest, Redactor, RestoredWorkEvidence, RestoredWorkEvidenceInput,
    RootExecution, RootExecutionState, SCHEMA_VERSION, Serialize, SqliteStore, StoreError,
    Transaction, Utc, WorkId, WorkItem, WorkLifecycle, WorkRun, WorkRunState,
    active_root_execution_optional, append_to_work_feeds, assert_actor_session, assert_revision,
    inspect_work_request, latest_restored_record, load_root_execution, load_typed_work_object,
    load_work_item, load_work_run, normalize_gate_evidence_input, normalize_strings, params,
    persist_operation_result, persist_root_execution, persist_work_item, replay_operation,
    request_object, validate_evidence_phase_marker, work_completed_by_restored_record_on,
};

fn latest_restored_gate_evidence_on(
    connection: &Connection,
    work_id: WorkId,
    name: &str,
) -> Result<Option<ObjectId>, StoreError> {
    let stored = connection
        .prepare(
            "SELECT evidence_id, sequence
             FROM work_restored_evidence INDEXED BY work_restored_evidence_gate
             WHERE work_id = ?1 AND gate_name = ?2
             ORDER BY sequence, evidence_id",
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
        let hash = ObjectId::from_stored(stored_hash.clone())
            .ok_or(StoreError::InvalidStoredKey(stored_hash))?;
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

pub(in crate::storage::work) fn ensure_restored_execution_state(
    transaction: &Transaction<'_>,
    item: &mut WorkItem,
    now: DateTime<Utc>,
) -> Result<(RootExecution, WorkRun, bool), StoreError> {
    ensure_restored_execution_state_for_event(
        transaction,
        item,
        now,
        &super::super::planning::successor::NativePlanningEvent::Ordinary,
    )
}

pub(in crate::storage::work) fn ensure_restored_execution_state_for_event(
    transaction: &Transaction<'_>,
    item: &mut WorkItem,
    now: DateTime<Utc>,
    mutation: &super::super::planning::successor::NativePlanningEvent<'_, '_, '_, '_>,
) -> Result<(RootExecution, WorkRun, bool), StoreError> {
    mutation.check(transaction, item.work_id)?;
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
                    root_execution_id: mutation.root_id(item.root_id)?,
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
        run_id: mutation.run_id(item.work_id)?,
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
    execution
        .run_ids
        .sort_by(super::super::root_state::compare_runs);
    execution.run_ids.dedup();
    execution.updated_at = now;
    if created_execution {
        super::super::root_state::initialize(transaction, &execution)?;
    } else {
        execution.revision += 1;
        persist_root_execution(transaction, &execution)?;
    }
    transaction.execute(
        "INSERT INTO work_runs (
             run_id, root_execution_id, work_id, generation,
             executor_session_id, state, revision, claim_fence_head,
             last_checkpoint_id, completion_seal_id,
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
) -> Result<ObjectId, StoreError> {
    let item = load_work_item(transaction, work_id)?;
    assert_revision(&item, expected_work_revision)?;
    if !work_completed_by_restored_record_on(transaction, &item)? {
        return Err(StoreError::InvalidWork(
            "restored late findings require completed-by-record work".into(),
        ));
    }
    let (record_id, record) =
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
        RestoredWorkEvidenceInput::Note { summary, refs, .. } => (
            super::super::planning::normalize_note_text(
                summary,
                crate::storage::refusal_labels::NOTE_SUMMARY,
            )?,
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
        restored_record: record_id,
        sequence,
        summary,
        refs,
        gate,
        actor: crate::domain::captured_note_actor(
            actor,
            matches!(input, RestoredWorkEvidenceInput::Note { status: true, .. }),
            item.assigned_to.as_deref() == Some(actor.actor_id.as_str()),
        ),
        created_at: recorded_at,
    };
    let object = CanonicalObject::mint(&evidence)?;
    SqliteStore::insert_object(transaction, "work_restored_evidence", &object)?;
    transaction.execute(
        "INSERT INTO work_restored_evidence (
              evidence_id, work_id, record_id, sequence, gate_name, created_at_ms
          ) VALUES (?1, ?2, ?3, ?4, ?5, ?6)",
        params![
            object.key().as_str(),
            item.work_id.0.to_string(),
            evidence.restored_record.as_str(),
            evidence.sequence,
            evidence.gate.as_ref().map(|gate| gate.name.as_str()),
            evidence.created_at.timestamp_millis(),
        ],
    )?;
    let positions = append_to_work_feeds(
        transaction,
        &item.project_id,
        item.root_id,
        None,
        None,
        "work_restored_evidence",
        &object,
    )?;
    super::super::note_index::append_native(
        transaction,
        &positions,
        object.key(),
        &super::super::note_index::NativeNote::Restored(&evidence),
    )?;
    Ok(object.key().clone())
}

impl SqliteStore {
    /// Appends one distinct restored gate observation without retry bookkeeping.
    pub(crate) fn append_restored_work_gate<R: Redactor>(
        &mut self,
        request: &AppendRestoredWorkGateRequest,
        redactor: &R,
    ) -> Result<ObjectId, StoreError> {
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
    ) -> Result<ObjectId, StoreError> {
        inspect_work_request(redactor, request, &request.actor)?;
        assert_actor_session(&request.actor, &request.holder)?;
        validate_evidence_phase_marker(WorkLifecycle::Completed, &request.actor)?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(hash) = replay_operation::<ObjectId>(
            &transaction,
            "record_restored_work_evidence",
            &request.idempotency_key,
            request_object.key(),
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
            request_object.key(),
            &hash,
        )?;
        transaction.commit()?;
        Ok(hash)
    }

    /// The restored late note already committed under `idempotency_key`, for
    /// an attempt interrupted before its response. The stored request also
    /// holds the original time, so the note is matched by its content
    /// instead: the same item, status, summary and refs as the retry, both
    /// normalized as a capture stores them. A note under that key with other
    /// content is refused, never adopted.
    pub(crate) fn recover_restored_work_note(
        &self,
        idempotency_key: &str,
        work_id: WorkId,
        status: bool,
        summary: &str,
        refs: &[String],
    ) -> Result<Option<ObjectId>, StoreError> {
        const OPERATION: &str = "record_restored_work_evidence";
        /// What a restored note holds that its retry must repeat.
        #[derive(Serialize)]
        struct NoteContent<'a> {
            work_id: WorkId,
            gate: bool,
            status: bool,
            summary: &'a str,
            refs: &'a [String],
        }
        let Some(result) = self.work_operation_result_value(OPERATION, idempotency_key)? else {
            return Ok(None);
        };
        let evidence_id: ObjectId = serde_json::from_value(result)?;
        let evidence: RestoredWorkEvidence = super::super::feeds::load_typed_work_object(
            &self.connection,
            &evidence_id,
            "work_restored_evidence",
        )?;
        let fingerprint = |content: &NoteContent<'_>| {
            CanonicalObject::freeze(content).map(|object| object.key().clone())
        };
        let retry_summary = super::super::planning::normalize_note_text(
            summary,
            crate::storage::refusal_labels::NOTE_SUMMARY,
        )?;
        let retry_refs = normalize_strings(refs);
        let stored_fingerprint = fingerprint(&NoteContent {
            work_id: evidence.work_id,
            gate: evidence.gate.is_some(),
            status: crate::domain::status_note_role(&evidence.actor).is_some(),
            summary: &evidence.summary,
            refs: &evidence.refs,
        })?;
        let retry_fingerprint = fingerprint(&NoteContent {
            work_id,
            gate: false,
            status,
            summary: &retry_summary,
            refs: &retry_refs,
        })?;
        if stored_fingerprint != retry_fingerprint {
            return Err(StoreError::WorkOperationIdempotencyConflict {
                operation: OPERATION.into(),
                key: idempotency_key.into(),
            });
        }
        Ok(Some(evidence_id))
    }
}
