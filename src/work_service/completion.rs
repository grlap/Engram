use super::*;

impl LocalWorkService {
    /// Completes ambient focused work under inferred run/claim/fence state.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when evidence/acceptance is incomplete, authority
    /// is absent, or any current lifecycle fence changed.
    pub fn work_complete(
        &self,
        input: WorkCompleteInput,
        now: DateTime<Utc>,
    ) -> Result<WorkCompleteResult, StoreError> {
        self.work_complete_on(None, input, now)
    }

    /// Like [`Self::work_complete`], but first binds `work_ref` as the ambient
    /// focus and the completion target inside the same call.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] under the same conditions as
    /// [`Self::work_complete`], or when `work_ref` does not resolve inside the
    /// project.
    #[allow(
        clippy::too_many_lines,
        reason = "capture, evidence closure, acceptance, checkpoint, and seal stay in one auditable completion path"
    )]
    pub fn work_complete_on(
        &self,
        work_ref: Option<&str>,
        input: WorkCompleteInput,
        now: DateTime<Utc>,
    ) -> Result<WorkCompleteResult, StoreError> {
        let mut store = self.store_at(now)?;
        let target = self.bind_target(&mut store, work_ref, now)?;
        let basis = self.protocol_basis(&store, true, false, target, now)?;
        let intent = self.protocol_intent(&input);
        let raw_key = self.effective_idempotency_key(
            &input.idempotency_key,
            "work_complete",
            &basis,
            &intent,
            now,
        )?;
        let attempt = store.begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
            project_id: &self.project_id,
            session_id: &self.session_id,
            operation: "work_complete",
            idempotency_key: &raw_key,
            intent: &intent,
            basis: &basis,
            now,
        })?;
        if let Some(result) = attempt.result {
            let result: WorkCompleteResult = serde_json::from_value(result)?;
            match &result {
                WorkCompleteResult::Completed(receipt) => {
                    ensure_completion_replay_target(&basis, receipt.work_id, &raw_key)?;
                    return Ok(result);
                }
                WorkCompleteResult::Refused(_) => {
                    return Err(StoreError::InvalidWorkProjection(
                        "stored work_complete attempt contains a refusal result".into(),
                    ));
                }
            }
        }
        let stored_basis = attempt
            .basis
            .clone()
            .map(serde_json::from_value::<WorkProtocolBasis>)
            .transpose()?;
        if let Some(stored_basis) = stored_basis.as_ref() {
            let stored_work = stored_basis.focused_work.as_ref().ok_or_else(|| {
                StoreError::InvalidWorkProjection(
                    "pending completion attempt has no bound focused work".into(),
                )
            })?;
            ensure_completion_replay_target(&basis, stored_work.work_id, &raw_key)?;
            let stored_run_id = if let Some(claim) = stored_basis.claim.as_ref() {
                if claim.work_id != stored_work.work_id {
                    return Err(StoreError::InvalidWorkProjection(
                        "pending completion claim crosses its focused work binding".into(),
                    ));
                }
                Some(claim.run_id)
            } else {
                stored_work.active_run_id
            };
            if let Some(run_id) = stored_run_id {
                let run = store.get_work_run(run_id)?;
                if run.work_id != stored_work.work_id {
                    return Err(StoreError::InvalidWorkProjection(
                        "pending completion run crosses its focused work binding".into(),
                    ));
                }
                if let Some(seal_hash) = run.completion_seal {
                    let seal: CompletionSeal = store.get(&seal_hash)?.ok_or_else(|| {
                        StoreError::InvalidWorkProjection(
                            "completed pending run has no canonical completion seal".into(),
                        )
                    })?;
                    if seal.work_id != stored_work.work_id || seal.run_id != run_id {
                        return Err(StoreError::InvalidWorkProjection(
                            "pending completion seal crosses its original work or run binding"
                                .into(),
                        ));
                    }
                    let result = completion_result(&store, &seal)?;
                    store.finish_work_protocol_attempt(
                        &self.project_id,
                        &self.session_id,
                        "work_complete",
                        &raw_key,
                        &result,
                    )?;
                    return Ok(result);
                }
            }
        }
        let mut basis_matches =
            retry_stable_basis_matches(attempt.basis_matches, attempt.basis.as_ref(), &basis)?;
        if !basis_matches
            && stored_basis.as_ref().is_some_and(|stored| {
                completion_basis_refresh_is_safe(stored, &basis, &self.session_id)
            })
        {
            let expected_basis = attempt.basis.as_ref().ok_or_else(|| {
                StoreError::InvalidWorkProjection(
                    "pending completion basis refresh has no durable source basis".into(),
                )
            })?;
            store.refresh_pending_work_protocol_attempt_basis(
                &self.project_id,
                &self.session_id,
                "work_complete",
                &raw_key,
                expected_basis,
                &basis,
            )?;
            basis_matches = true;
        }
        // A fresh attempt against work that was already sealed has no claim in
        // its basis. Only use the latest run while that exact completed basis
        // still matches; interrupted core completion above is bound to its
        // original claimed run instead.
        if basis_matches
            && let Some(work) = basis.focused_work.as_ref()
            && work.lifecycle == WorkLifecycle::Completed
            && let Some(run) = store.latest_work_run(work.work_id)?
            && let Some(seal_hash) = run.completion_seal
        {
            let seal: CompletionSeal = store.get(&seal_hash)?.ok_or_else(|| {
                StoreError::InvalidWorkProjection(
                    "completed work has no canonical completion seal".into(),
                )
            })?;
            let result = completion_result(&store, &seal)?;
            store.finish_work_protocol_attempt(
                &self.project_id,
                &self.session_id,
                "work_complete",
                &raw_key,
                &result,
            )?;
            return Ok(result);
        }
        ensure_protocol_basis(basis_matches, "work_complete", &raw_key, false)?;
        let WorkCompleteInput {
            capture,
            evidence: supplied_evidence,
            acceptance: supplied_acceptance,
            note,
            idempotency_key: _,
        } = input;
        let work = basis.focused_work.clone().ok_or_else(|| {
            StoreError::InvalidWorkProjection("completion attempt has no bound focused work".into())
        })?;
        let actor = self.actor("work_complete", "complete ambient local work");
        let claim = self.live_protocol_claim(&basis, &work, now)?;
        let evidence_basis = Self::completion_evidence_basis(&store, &claim, &supplied_evidence)?;
        let acceptance = match Self::prevalidate_completion_acceptance(
            &work,
            supplied_acceptance.as_deref(),
            note.as_deref(),
            &evidence_basis,
            actor.assurance,
            &actor.actor_id,
        ) {
            Ok(acceptance) => acceptance,
            Err(StoreError::WorkCompletionRecoveryRequired { cause, .. }) => {
                let snapshot = store.work_completion_recovery(&work, &claim, now, &cause)?;
                let obligation_page = work_completion_recovery_page(&snapshot)?;
                let result =
                    completion_recovery_result(work.work_id, snapshot.recovery, obligation_page);
                return Ok(result);
            }
            Err(error) => return Err(error),
        };
        let prepared = self.prepare_completion_evidence(
            &mut store,
            CompletionEvidencePlan {
                work: &work,
                claim: &claim,
                capture: capture.as_ref(),
                evidence: evidence_basis,
                base_key: &raw_key,
                now,
            },
        )?;
        let scoped_key =
            self.core_operation_key("work_complete", &prepared.attempt_key, "complete_work")?;
        let evidence = prepared.evidence;
        let acceptance = bind_completion_acceptance_evidence(acceptance, &evidence);
        let completion = store.complete_work_for_protocol(
            &CompleteWorkRequest {
                work_id: work.work_id,
                run_id: claim.run_id,
                holder: self.session_id.clone(),
                expected_work_revision: work.revision,
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                evidence,
                acceptance,
                drain: CompletionDrainAttestation {
                    reconciled_action_outcomes: Vec::new(),
                    released_resource_leases: Vec::new(),
                },
                actor,
                idempotency_key: scoped_key,
                completed_at: now,
            },
            &DevelopmentNoopRedactor,
        );
        let result = match completion? {
            CompleteWorkStorageResult::Completed(seal) => completion_result(&store, &seal)?,
            CompleteWorkStorageResult::Recovery(snapshot) => {
                let obligation_page = work_completion_recovery_page(&snapshot)?;
                let result =
                    completion_recovery_result(work.work_id, snapshot.recovery, obligation_page);
                return Ok(result);
            }
        };
        store.finish_work_protocol_attempt(
            &self.project_id,
            &self.session_id,
            "work_complete",
            &raw_key,
            &result,
        )?;
        Ok(result)
    }

    pub(super) fn prepare_completion_evidence(
        &self,
        store: &mut SqliteStore,
        plan: CompletionEvidencePlan<'_>,
    ) -> Result<PreparedCompletionEvidence, StoreError> {
        let CompletionEvidencePlan {
            work,
            claim,
            capture,
            mut evidence,
            base_key,
            now,
        } = plan;
        if let Some(capture) = capture {
            let capture_key = completion_capture_key(base_key, work, claim)?;
            let evidence_key =
                self.core_operation_key("work_complete", &capture_key, "record_work_evidence")?;
            let recorded_at = store
                .work_operation_result_object::<WorkEvidence>(
                    "record_work_evidence",
                    &evidence_key,
                    "work_evidence",
                )?
                .map_or(now, |committed| committed.created_at);
            let captured = store.record_work_evidence(
                &RecordWorkEvidenceRequest {
                    work_id: work.work_id,
                    run_id: claim.run_id,
                    expected_work_revision: work.revision,
                    holder: self.session_id.clone(),
                    claim_id: claim.claim_id,
                    claim_fence: claim.fence,
                    summary: capture.summary.clone(),
                    refs: capture.refs.clone(),
                    actor: self.actor(
                        "work_complete",
                        "capture completion evidence for ambient local work",
                    ),
                    idempotency_key: evidence_key,
                    recorded_at,
                },
                &DevelopmentNoopRedactor,
            )?;
            evidence.push(captured);
        }
        evidence.sort();
        evidence.dedup();
        let run_feed_cut = if let Some(capture) = capture {
            let (_, cut) = store.checkpoint_work_for_completion(
                &CheckpointWorkRequest {
                    work_id: work.work_id,
                    run_id: claim.run_id,
                    expected_work_revision: work.revision,
                    holder: self.session_id.clone(),
                    claim_id: claim.claim_id,
                    claim_fence: claim.fence,
                    summary: capture.summary.clone(),
                    evidence: Some(evidence.clone()),
                    actor: self.actor(
                        "work_complete",
                        "checkpoint the exact completion evidence cut",
                    ),
                    idempotency_key: base_key.to_owned(),
                    checkpointed_at: now,
                },
                |cut| {
                    let attempt_key = completion_attempt_key(base_key, cut)?;
                    self.core_operation_key("work_complete", &attempt_key, "checkpoint_work")
                },
                &DevelopmentNoopRedactor,
            )?;
            cut
        } else {
            FeedPosition {
                feed: FeedId::RunExecution(claim.run_id),
                position: store.work_feed_head(&FeedId::RunExecution(claim.run_id))?,
            }
        };
        let attempt_key = completion_attempt_key(base_key, &run_feed_cut)?;
        Ok(PreparedCompletionEvidence {
            evidence,
            attempt_key,
        })
    }

    pub(super) fn completion_evidence_basis(
        store: &SqliteStore,
        claim: &WorkClaim,
        supplied: &[String],
    ) -> Result<Vec<ObjectHash>, StoreError> {
        let available = store.work_run_evidence(claim.run_id)?;
        let mut requested = parse_hashes(supplied)?;
        if requested.is_empty() {
            return Ok(available);
        }
        let available = available.iter().collect::<std::collections::HashSet<_>>();
        if let Some(hash) = requested.iter().find(|hash| !available.contains(hash)) {
            return Err(StoreError::InvalidWork(format!(
                "evidence object {hash} does not belong to the focused run"
            )));
        }
        requested.sort();
        requested.dedup();
        Ok(requested)
    }

    pub(super) fn prevalidate_completion_acceptance(
        work: &WorkItem,
        supplied: Option<&[WorkAcceptanceInput]>,
        note: Option<&str>,
        evidence_basis: &[ObjectHash],
        assurance: AssuranceLevel,
        actor_id: &str,
    ) -> Result<Vec<AcceptanceResult>, StoreError> {
        let translated = if let Some(supplied) = supplied {
            if note.is_some() {
                return Err(StoreError::InvalidWork(
                    "completion note may be supplied only when acceptance is omitted".into(),
                ));
            }
            supplied
                .iter()
                .map(|result| {
                    let criterion = match result.criterion.as_deref() {
                        Some(value) => value.trim().to_owned(),
                        None if work.acceptance.len() == 1 => work.acceptance[0].clone(),
                        None => {
                            return Err(StoreError::InvalidWork(
                                "criterion is required when work has multiple acceptance criteria"
                                    .into(),
                            ));
                        }
                    };
                    Ok(AcceptanceResult {
                        criterion,
                        satisfied: result.satisfied,
                        evidence: parse_hashes(&result.evidence)?,
                        assurance,
                        note: result.note.clone(),
                    })
                })
                .collect::<Result<Vec<_>, StoreError>>()?
        } else {
            let note = note
                .map(str::trim)
                .filter(|value| !value.is_empty())
                .map_or_else(
                    || format!("accepted by {actor_id} via work done"),
                    str::to_owned,
                );
            work.acceptance
                .iter()
                .map(|criterion| AcceptanceResult {
                    criterion: criterion.clone(),
                    satisfied: true,
                    evidence: Vec::new(),
                    assurance,
                    note: note.clone(),
                })
                .collect()
        };
        let normalized = normalize_completion_acceptance_shape(work, &translated, assurance)?;
        let evidence_basis = evidence_basis
            .iter()
            .collect::<std::collections::HashSet<_>>();
        for result in &normalized {
            if let Some(hash) = result
                .evidence
                .iter()
                .find(|hash| !evidence_basis.contains(hash))
            {
                return Err(StoreError::WorkCompletionRefused {
                    work: work.work_id,
                    reason: format!(
                        "acceptance criterion {:?} cites evidence {hash} outside the requested completion basis",
                        result.criterion
                    ),
                });
            }
        }
        Ok(normalized)
    }
}

#[cfg(test)]
mod tests;
