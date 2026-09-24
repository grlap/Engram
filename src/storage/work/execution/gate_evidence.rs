//! Records and queries live quality-gate evidence transitions for a claimed
//! run, including canonical-history lookup and protocol-attempt reservation.

use super::{
    ActorContext, BeginGateWorkProtocolAttempt, BeginWorkProtocolAttempt, CanonicalObject,
    Connection, GATE_EVIDENCE_SUMMARY, GateEvidenceRecord, GateWorkProtocolAttempt,
    GateWorkProtocolIntent, HashMap, ObjectId, OptionalExtension, RecordGateEvidenceRequest,
    Redactor, SCHEMA_VERSION, Serialize, SqliteStore, StoreError, Transaction, WorkEvidence,
    WorkLifecycle, WorkRunId, assert_actor_session, begin_work_protocol_attempt_on,
    expire_handoff_offers, inspect_work_request, load_typed_work_object, load_work_item,
    normalize_gate_evidence_input, params, persist_post_completion_work_evidence_on,
    persist_work_evidence_on, renew_holder_claim, validate_gate_evidence_payload,
    validate_live_claim_for_item_on, validate_post_completion_evidence_basis_on,
};

impl SqliteStore {
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
    ) -> Result<ObjectId, StoreError> {
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

        let evidence_id = append_gate_evidence_on(
            &transaction,
            request,
            name,
            failed,
            refs,
            previous.as_ref().map(|(hash, _)| hash.clone()),
        )?;
        transaction.commit()?;
        Ok(evidence_id)
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
        let idempotency_key = format!("gate:{}", intent_object.key().as_str());
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
}

fn append_gate_evidence_on(
    transaction: &Transaction<'_>,
    request: &RecordGateEvidenceRequest,
    name: String,
    failed: Vec<String>,
    refs: Vec<String>,
    previous: Option<ObjectId>,
) -> Result<ObjectId, StoreError> {
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

pub(super) const LATEST_GATE_EVIDENCE_SQL: &str = "SELECT entry.object_id
     FROM work_feed_entries entry
     WHERE entry.feed_kind = 'run_execution'
       AND entry.feed_id = ?1
       AND entry.object_kind = 'work_evidence'
       AND entry.position = (
           SELECT MAX(candidate.position)
           FROM objects object INDEXED BY objects_work_evidence_gate_name
           JOIN work_run_evidence evidence
             ON evidence.run_id = ?1
            AND evidence.evidence_id = object.object_id
           JOIN work_feed_entries candidate
             ON candidate.feed_kind = 'run_execution'
            AND candidate.feed_id = evidence.run_id
            AND candidate.object_id = evidence.evidence_id
           WHERE object.object_kind = 'work_evidence'
             AND json_extract(object.canonical_json, '$.run_id') = ?1
             AND json_type(object.canonical_json, '$.gate') = 'object'
             AND json_extract(object.canonical_json, '$.gate.name') = ?2
       )";

pub(super) fn latest_gate_evidence_on(
    connection: &Connection,
    run_id: WorkRunId,
    name: &str,
) -> Result<Option<(ObjectId, WorkEvidence)>, StoreError> {
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
            let hash = ObjectId::from_stored(stored.clone())
                .ok_or(StoreError::InvalidStoredKey(stored))?;
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

pub(super) fn validate_gate_evidence(
    evidence_id: &ObjectId,
    evidence: &WorkEvidence,
) -> Result<(), StoreError> {
    validate_gate_evidence_payload(evidence).map_err(|detail| {
        StoreError::InvalidWorkProjection(format!(
            "gate evidence {evidence_id} has an invalid typed payload: {detail}"
        ))
    })
}

pub(in crate::storage::work) fn validate_gate_evidence_chain(
    evidence_id: &ObjectId,
    evidence: &WorkEvidence,
    gate_heads: &mut HashMap<(WorkRunId, String), ObjectId>,
) -> Result<(), StoreError> {
    validate_gate_evidence(evidence_id, evidence)?;
    let Some(gate) = &evidence.gate else {
        return Ok(());
    };
    let key = (evidence.run_id, gate.name.clone());
    if gate.previous.as_ref() != gate_heads.get(&key) {
        return Err(StoreError::InvalidWorkProjection(format!(
            "gate evidence {evidence_id} does not name the prior same-run, same-name observation"
        )));
    }
    gate_heads.insert(key, evidence_id.clone());
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
