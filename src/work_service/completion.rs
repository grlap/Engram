use super::*;

mod links;

/// A sealed completion's landing is frozen with it: naming a landing its seal
/// does not already record is a late finding, refused like any other change
/// to completed work, so a later push is recorded in a note.
fn landing_frozen(input: &WorkCompleteInput, seal: &CompletionSeal) -> Result<(), StoreError> {
    match &input.landing {
        Some(landing) if seal.landing.as_ref() != Some(landing) => Err(StoreError::InvalidWork(
            COMPLETED_WORK_LATE_FINDING_REFUSAL.into(),
        )),
        _ => Ok(()),
    }
}

/// Decodes a stored completion result by its discriminant, so that a result
/// this build cannot read is refused with the reason of the one shape it
/// claims, not the untagged union's "matches no variant". A present `seal`
/// selects the receipt, even when null or malformed; otherwise a `code`
/// selects the refusal. The reason names an unknown or missing member and
/// otherwise only the kind of problem, never a stored value.
fn replayed_completion_result(result: serde_json::Value) -> Result<WorkCompleteResult, StoreError> {
    let refused = |reason: &str| {
        StoreError::Json(<serde_json::Error as serde::de::Error>::custom(format!(
            "stored work_complete result: {reason}"
        )))
    };
    let decoded = if result.get("seal").is_some() {
        serde_json::from_value(result).map(WorkCompleteResult::Completed)
    } else if result.get("code").is_some() {
        serde_json::from_value(result).map(WorkCompleteResult::Refused)
    } else {
        return Err(refused("neither a `seal` nor a refusal `code`"));
    };
    decoded.map_err(|error| refused(&crate::storage::undecodable_json_reason(&error)))
}

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
        links::validate_shape(&input)?;
        // A malformed landing is refused before anything is recorded.
        if let Some(landing) = &input.landing {
            landing.validate().map_err(StoreError::InvalidWork)?;
        }
        let mut store = self.store_at(now)?;
        let target = self.bind_target(&mut store, work_ref, now)?;
        // The work and retained claim jointly identify the run, including
        // after sealing. Do not combine two cuts across a concurrent reopen.
        let basis = store
            .work_read_snapshot(|store| self.protocol_basis(store, true, false, target, now))?;
        let intent = self.protocol_intent(&input);
        let raw_key = if !input.links.is_empty() && input.idempotency_key.trim().is_empty() {
            // A positional link already carries an explicit read basis. Its
            // exact intent must retain its identity across the sealed revision;
            // a different intent still cannot amend an existing seal.
            let identity = CanonicalObject::freeze(&serde_json::json!({
                "project": self.project_id, "session": self.session_id,
                "operation": "work_complete", "work": basis.focused_work.as_ref().map(|work| work.work_id),
                "intent": intent,
            }))?;
            format!("linked-completion:{}", identity.key())
        } else {
            self.effective_idempotency_key(
                &input.idempotency_key,
                "work_complete",
                &basis,
                &intent,
                now,
            )?
        };
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
            let mut result = replayed_completion_result(result)?;
            match &mut result {
                WorkCompleteResult::Completed(receipt) => {
                    ensure_completion_replay_target(&basis, receipt.work_id, &raw_key)?;
                    // The canonical replay receipt already proves completion.
                    // An advisory reload failure must not turn it into refusal;
                    // the agent renderer explicitly discloses unavailable facts.
                    match acceptance::for_seal(
                        &store,
                        &receipt.seal,
                        receipt.work_id,
                        receipt.run_id,
                    ) {
                        Ok(facts) => receipt.acceptance_evidence = Some(facts),
                        Err(error) => {
                            receipt.acceptance_evidence_error_class =
                                Some(advisory_error_class(&error));
                        }
                    }
                    // Provenance is reloaded from the frozen seal the same
                    // way, so a replay discloses what the first receipt did.
                    receipt.acceptance_provenance = acceptance::provenance_for_seal(
                        &store,
                        &receipt.seal,
                        receipt.work_id,
                        receipt.run_id,
                    )
                    .ok();
                    // A landing that cannot be read back is disclosed as
                    // unavailable, never silently dropped from the receipt.
                    match acceptance::bound_seal(
                        &store,
                        &receipt.seal,
                        receipt.work_id,
                        receipt.run_id,
                    ) {
                        Ok(seal) => receipt.landing = seal.landing,
                        Err(error) => {
                            receipt.landing_unavailable = Some(advisory_error_class(&error));
                        }
                    }
                    // A receipt stored before pages carried their run's
                    // history reads as history now that its run is sealed.
                    // Like the reloads above, this is advisory: a page whose
                    // history cannot be read is returned as recorded, never
                    // turned into a refusal of a proven completion.
                    let _advisory = super::projection::replayed_obligation_page(
                        &store,
                        Some(receipt.run_id),
                        &mut receipt.obligation_page,
                    );
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
            if stored_work.lifecycle == WorkLifecycle::Completed {
                links::frozen(&input)?;
            }
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
                if let Some(seal_id) = run.completion_seal {
                    let seal: CompletionSeal = store.get(&seal_id)?.ok_or_else(|| {
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
                    landing_frozen(&input, &seal)?;
                    if !input.links.is_empty() {
                        // Mirror prepare_completion_evidence: capture keys its
                        // pre-checkpoint cut; no capture keys the head it read,
                        // which completion requires to be where the checkpoint
                        // ends. The seal's cut can lie past it: completion
                        // appends its untested-change waivers first.
                        let checkpoint = seal
                            .checkpoint
                            .as_ref()
                            .map(|checkpoint_id| {
                                store
                                    .get::<crate::domain::WorkCheckpoint>(checkpoint_id)?
                                    .ok_or_else(|| {
                                        StoreError::InvalidWorkProjection(
                                            "completed pending run has no canonical checkpoint"
                                                .into(),
                                        )
                                    })
                            })
                            .transpose()?;
                        let attempt_cut = match (input.capture.is_some(), checkpoint) {
                            (true, Some(checkpoint)) => checkpoint.acknowledged_run_position,
                            (true, None) => {
                                return Err(StoreError::InvalidWorkProjection(
                                    "captured completion has no checkpoint binding".into(),
                                ));
                            }
                            (false, Some(checkpoint)) => {
                                crate::storage::checkpoint_run_feed_end(&checkpoint)?
                            }
                            (false, None) => seal.completion_cut.clone(),
                        };
                        let attempt_key = completion_attempt_key(&raw_key, &attempt_cut)?;
                        let core_key = self.core_operation_key(
                            "work_complete",
                            &attempt_key,
                            "complete_work",
                        )?;
                        let committed = store
                            .work_operation_result_value("complete_work", &core_key)?
                            .map(serde_json::from_value::<CompletionSeal>)
                            .transpose()?;
                        if committed.as_ref() != Some(&seal) {
                            links::frozen(&input)?;
                        }
                    }
                    links::validate_recovered_seal(
                        stored_basis,
                        &input,
                        &seal,
                        &self.actor("work_complete", "complete ambient local work"),
                    )?;
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
                completion_basis_refresh_is_safe(
                    stored,
                    &basis,
                    &self.session_id,
                    input.links.is_empty() && input.idempotency_key.trim().is_empty(),
                )
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
            && let Some(seal_id) = run.completion_seal
        {
            links::frozen(&input)?;
            let seal: CompletionSeal = store.get(&seal_id)?.ok_or_else(|| {
                StoreError::InvalidWorkProjection(
                    "completed work has no canonical completion seal".into(),
                )
            })?;
            landing_frozen(&input, &seal)?;
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
        if !basis_matches && !input.links.is_empty() {
            return Err(StoreError::WorkCriterionLinkInvalid {
                criterion: None,
                reason: "the pending completion basis changed; read show and reconcile the links before a new intent",
            });
        }
        if !basis_matches
            && input.links.is_empty()
            && input.idempotency_key.trim().is_empty()
            && let Some(work) = basis.focused_work.as_ref()
        {
            // A refused refresh grants no authority and changes no pending row.
            // Preserve the claim guidance a fresh keyless attempt would return,
            // rather than asking its caller to supply a different derived key.
            self.live_protocol_claim(&basis, work, now)?;
        }
        ensure_protocol_basis(basis_matches, "work_complete", &raw_key, false)?;
        let work = basis.focused_work.clone().ok_or_else(|| {
            StoreError::InvalidWorkProjection("completion attempt has no bound focused work".into())
        })?;
        let actor = self.actor("work_complete", "complete ambient local work");
        let claim = self.live_protocol_claim(&basis, &work, now)?;
        let remedy_modes = CompletionRemedyModes {
            mark: work.evaluation_mode,
            admitted: store.acceptance_evaluation_policy()?.allowed_modes,
        };
        let mut evidence_basis = Self::completion_evidence_basis(&store, &claim, &input.evidence)?;
        let validated_acceptance =
            links::validated_acceptance(&store, &work, &claim, &input, &actor, &evidence_basis);
        let acceptance = match validated_acceptance {
            Ok(acceptance) => acceptance,
            Err(StoreError::WorkCompletionRecoveryRequired { cause, context, .. }) => {
                let snapshot =
                    store.work_completion_recovery(&work, &claim, now, &cause, *context)?;
                let obligation_page = work_completion_recovery_page(&store, &snapshot)?;
                let result = completion_recovery_result(
                    work.work_id,
                    snapshot.recovery,
                    obligation_page,
                    snapshot.required_child_successor,
                    &remedy_modes,
                );
                return Ok(result);
            }
            Err(error) => return Err(error),
        };
        // Under an evaluated policy the readiness decision is taken before
        // any capture is recorded: the shared assessment answers with the
        // same typed recovery the storage completion would, and the storage
        // completion still repeats it inside its transaction. A ready
        // evaluation's citations join the completion evidence set, so the
        // capture checkpoint acknowledges them and the seal names them.
        match store.acceptance_evaluation_readiness(
            work.work_id,
            claim.run_id,
            input.source_fingerprint.as_deref(),
        )? {
            crate::storage::AcceptanceEvaluationReadiness::Blocked(cause, context) => {
                let snapshot =
                    store.work_completion_recovery(&work, &claim, now, &cause, context)?;
                let obligation_page = work_completion_recovery_page(&store, &snapshot)?;
                return Ok(completion_recovery_result(
                    work.work_id,
                    snapshot.recovery,
                    obligation_page,
                    snapshot.required_child_successor,
                    &remedy_modes,
                ));
            }
            crate::storage::AcceptanceEvaluationReadiness::Ready(evaluation) => {
                evidence_basis.extend(
                    evaluation
                        .verdicts
                        .iter()
                        .flat_map(|verdict| verdict.evidence.iter().cloned()),
                );
            }
            crate::storage::AcceptanceEvaluationReadiness::SelfAsserted => {}
        }
        let capture = input.capture;
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
                source_fingerprint: input.source_fingerprint.clone(),
                landing: input.landing.clone(),
                actor,
                idempotency_key: scoped_key,
                completed_at: now,
            },
            &DevelopmentNoopRedactor,
        );
        let result = match completion? {
            CompleteWorkStorageResult::Completed(seal) => completion_result(&store, &seal)?,
            CompleteWorkStorageResult::Recovery(snapshot) => {
                let obligation_page = work_completion_recovery_page(&store, &snapshot)?;
                let result = completion_recovery_result(
                    work.work_id,
                    snapshot.recovery,
                    obligation_page,
                    snapshot.required_child_successor,
                    &remedy_modes,
                );
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
                        super::change_context::COMPLETION_CAPTURE_REASON,
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
                        super::change_context::COMPLETION_CHECKPOINT_REASON,
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
    ) -> Result<Vec<ObjectId>, StoreError> {
        let available = store.work_run_evidence(claim.run_id)?;
        let mut requested = parse_record_ids(supplied)?;
        if requested.is_empty() {
            return Ok(available);
        }
        let available = available.iter().collect::<std::collections::HashSet<_>>();
        if let Some(evidence_id) = requested
            .iter()
            .find(|evidence_id| !available.contains(evidence_id))
        {
            return Err(StoreError::InvalidWork(format!(
                "evidence object {evidence_id} does not belong to the focused run"
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
        evidence_basis: &[ObjectId],
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
                        evidence: parse_record_ids(&result.evidence)?,
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
            if let Some(evidence_id) = result
                .evidence
                .iter()
                .find(|evidence_id| !evidence_basis.contains(evidence_id))
            {
                return Err(StoreError::WorkCompletionRefused {
                    work: work.work_id,
                    reason: format!(
                        "acceptance criterion {:?} cites evidence {evidence_id} outside the requested completion basis",
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
