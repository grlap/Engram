use super::*;

impl LocalWorkService {
    /// Applies one typed update to ambient focused work.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when focus/authority/fences are absent or stale.
    pub fn work_update(
        &self,
        input: WorkUpdateInput,
        now: DateTime<Utc>,
    ) -> Result<WorkUpdateResult, StoreError> {
        self.work_update_on(None, input, now)
    }

    /// Records one typed gate transition through the evidence path. Storage
    /// serializes the latest same-name observation with the append, so an
    /// exact consecutive retry is atomic across sessions and processes.
    #[cfg(test)]
    pub(crate) fn work_gate(
        &self,
        name: &str,
        failed: &[String],
        evidence_ref: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<WorkUpdateResult, StoreError> {
        self.work_gate_on(None, name, failed, evidence_ref, now)
    }

    /// Like [`Self::work_gate`], but binds an explicit target through the same
    /// storage operation so concurrent focus changes cannot redirect evidence.
    pub(crate) fn work_gate_on(
        &self,
        work_ref: Option<&str>,
        name: &str,
        failed: &[String],
        evidence_ref: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<WorkUpdateResult, StoreError> {
        let mut store = self.store_at(now)?;
        let target = self.bind_target(&mut store, work_ref, now)?;
        let basis = self.protocol_basis(&store, true, false, target, now)?;
        let work = basis.focused_work.clone().ok_or_else(|| {
            StoreError::InvalidWorkProjection("gate attempt has no bound focused work".into())
        })?;
        let (run_id, claim_id, claim_fence, actor) = if work.lifecycle == WorkLifecycle::Completed {
            let (run, seal) = Self::completed_evidence_basis(&store, &work)?;
            (
                run.run_id,
                seal.claim_id,
                seal.claim_fence,
                self.post_completion_actor(
                    "work_update",
                    "record post-completion gate evidence for ambient work",
                ),
            )
        } else {
            let claim = basis
                .claim
                .clone()
                .ok_or(StoreError::WorkClaimMismatch { work: work.work_id })?;
            if claim.work_id != work.work_id
                || claim.state != WorkClaimState::Active
                || claim.holder != self.session_id
            {
                return Err(StoreError::WorkClaimMismatch { work: work.work_id });
            }
            if work.lifecycle != WorkLifecycle::Open {
                return Err(StoreError::WorkNotOpen(work.work_id));
            }
            (
                claim.run_id,
                claim.claim_id,
                claim.fence,
                self.actor("work_update", "record gate evidence for ambient work"),
            )
        };
        let attempt = store.record_gate_evidence_protocol(
            &RecordGateEvidenceRequest {
                work_id: work.work_id,
                run_id,
                expected_work_revision: work.revision,
                holder: self.session_id.clone(),
                claim_id,
                claim_fence,
                name: name.to_owned(),
                failed: failed.to_owned(),
                evidence_ref: evidence_ref.map(str::to_owned),
                actor,
                recorded_at: now,
            },
            &BeginGateWorkProtocolAttempt {
                project_id: &self.project_id,
                session_id: &self.session_id,
                basis: &basis,
                now,
            },
            &DevelopmentNoopRedactor,
        )?;
        let protocol_operation = "work_update:gate";
        // Gate does not use `basis_matches`: its protocol key already binds
        // work, run, claim id/fence, normalized observation, and the canonical
        // previous transition. A new append revalidates either the live claim
        // or the immutable completed-seal basis inside the same transaction.
        if let Some(result) = attempt.result {
            return serde_json::from_value(result).map_err(StoreError::from);
        }
        let result = self.work_update_result(
            &store,
            "evidence",
            work.work_id,
            serde_json::to_value(&attempt.evidence)?,
            now,
        )?;
        store.finish_work_protocol_attempt(
            &self.project_id,
            &self.session_id,
            protocol_operation,
            &attempt.idempotency_key,
            &result,
        )?;
        Ok(result)
    }

    /// Captures one note under one explicit work target and one atomic storage
    /// operation. Open work also receives an acknowledging checkpoint; a
    /// completed item receives only evidence after its frozen seal cut.
    pub(crate) fn work_note_on(
        &self,
        work_ref: Option<&str>,
        summary: &str,
        refs: &[String],
        now: DateTime<Utc>,
    ) -> Result<WorkNoteResult, StoreError> {
        let mut store = self.store_at(now)?;
        let target = self.bind_target(&mut store, work_ref, now)?;
        let basis = self.protocol_basis(&store, true, false, target, now)?;
        let note = WorkNoteIntent { summary, refs };
        let intent = self.protocol_intent(&note);
        let protocol_operation = "work_update:note";
        let raw_key =
            self.effective_idempotency_key("", protocol_operation, &basis, &intent, now)?;
        let attempt = store.begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
            project_id: &self.project_id,
            session_id: &self.session_id,
            operation: protocol_operation,
            idempotency_key: &raw_key,
            intent: &intent,
            basis: &basis,
            now,
        })?;
        if let Some(result) = attempt.result {
            return serde_json::from_value(result).map_err(StoreError::from);
        }
        let basis_matches =
            retry_stable_basis_matches(attempt.basis_matches, attempt.basis.as_ref(), &basis)?;
        let scoped_key =
            self.core_operation_key(protocol_operation, &raw_key, "record_work_note")?;
        if let Some(value) = store.work_operation_result_value("record_work_note", &scoped_key)? {
            let capture: WorkNoteCapture = serde_json::from_value(value)?;
            let durable_basis: WorkProtocolBasis =
                serde_json::from_value(attempt.basis.ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "committed note has no durable attempt basis".into(),
                    )
                })?)?;
            let work_id = durable_basis
                .focused_work
                .map(|work| work.work_id)
                .ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "committed note basis has no focused work".into(),
                    )
                })?;
            let result = self.work_note_result(&store, work_id, &capture, now)?;
            store.finish_work_protocol_attempt(
                &self.project_id,
                &self.session_id,
                protocol_operation,
                &raw_key,
                &result,
            )?;
            return Ok(result);
        }
        ensure_protocol_basis(basis_matches, protocol_operation, &raw_key, false)?;
        let work = basis.focused_work.clone().ok_or_else(|| {
            StoreError::InvalidWorkProjection("note attempt has no bound focused work".into())
        })?;
        let (run_id, claim_id, claim_fence, actor) =
            self.work_note_evidence_basis(&store, &basis, &work, now)?;
        let capture = store.record_work_note(
            &RecordWorkNoteRequest {
                work_id: work.work_id,
                run_id,
                expected_work_revision: work.revision,
                holder: self.session_id.clone(),
                claim_id,
                claim_fence,
                summary: summary.to_owned(),
                refs: refs.to_owned(),
                actor,
                idempotency_key: scoped_key,
                recorded_at: now,
            },
            &DevelopmentNoopRedactor,
        )?;
        let result = self.work_note_result(&store, work.work_id, &capture, now)?;
        store.finish_work_protocol_attempt(
            &self.project_id,
            &self.session_id,
            protocol_operation,
            &raw_key,
            &result,
        )?;
        Ok(result)
    }

    fn work_note_result(
        &self,
        store: &SqliteStore,
        work_id: WorkId,
        capture: &WorkNoteCapture,
        now: DateTime<Utc>,
    ) -> Result<WorkNoteResult, StoreError> {
        let guidance = self.work_guidance(store, work_id, now)?;
        let evidence = compact_mutation_receipt(
            &guidance.status.work,
            None,
            serde_json::to_value(&capture.evidence)?,
        );
        let primary = capture
            .checkpoint
            .as_ref()
            .unwrap_or(&capture.evidence)
            .clone();
        let result = WorkNoteResult {
            operation: "note".into(),
            receipt: compact_mutation_receipt(
                &guidance.status.work,
                None,
                serde_json::to_value(primary)?,
            ),
            obligations: compact_obligations(&guidance.status),
            obligation_page: work_obligation_page(store, work_id)?,
            allowed_next: guidance.allowed_next,
            evidence,
        };
        ensure_agent_response_budget(&result, "work_update")?;
        Ok(result)
    }

    fn work_note_evidence_basis(
        &self,
        store: &SqliteStore,
        basis: &WorkProtocolBasis,
        work: &WorkItem,
        now: DateTime<Utc>,
    ) -> Result<(WorkRunId, WorkClaimId, i64, ActorContext), StoreError> {
        match work.lifecycle {
            WorkLifecycle::Open => {
                let claim = self.live_protocol_claim(basis, work, now)?;
                Ok((
                    claim.run_id,
                    claim.claim_id,
                    claim.fence,
                    self.actor(
                        "work_update",
                        "record note evidence and checkpoint ambient local work",
                    ),
                ))
            }
            WorkLifecycle::Completed => {
                let (run, seal) = Self::completed_evidence_basis(store, work)?;
                Ok((
                    run.run_id,
                    seal.claim_id,
                    seal.claim_fence,
                    self.post_completion_actor(
                        "work_update",
                        "record post-completion note evidence for ambient work",
                    ),
                ))
            }
            WorkLifecycle::Proposed | WorkLifecycle::Cancelled | WorkLifecycle::Superseded => {
                self.live_protocol_claim(basis, work, now).map(|claim| {
                    (
                        claim.run_id,
                        claim.claim_id,
                        claim.fence,
                        self.actor(
                            "work_update",
                            "record note evidence and checkpoint ambient local work",
                        ),
                    )
                })
            }
        }
    }

    fn completed_evidence_basis(
        store: &SqliteStore,
        work: &WorkItem,
    ) -> Result<(WorkRun, CompletionSeal), StoreError> {
        let run = store.latest_work_run(work.work_id)?.ok_or_else(|| {
            StoreError::InvalidWorkProjection(
                "completed work has no historical execution run".into(),
            )
        })?;
        let seal_hash = run.completion_seal.as_ref().ok_or_else(|| {
            StoreError::InvalidWorkProjection("completed work has no completion seal".into())
        })?;
        let seal: CompletionSeal = store.get(seal_hash)?.ok_or_else(|| {
            StoreError::InvalidWorkProjection(
                "completed work has no canonical completion seal".into(),
            )
        })?;
        if run.work_id != work.work_id
            || run.state != WorkRunState::Completed
            || seal.work_id != work.work_id
            || seal.run_id != run.run_id
        {
            return Err(StoreError::InvalidWorkProjection(
                "completed evidence basis crosses its work or run binding".into(),
            ));
        }
        Ok((run, seal))
    }

    /// Like [`Self::work_update`], but first binds `work_ref` as the ambient
    /// focus and the mutation target inside the same call.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] under the same conditions as [`Self::work_update`],
    /// or when `work_ref` does not resolve inside the project.
    #[allow(
        clippy::too_many_lines,
        reason = "the tagged update union is translated in one exhaustive match so new variants cannot bypass ambient fence inference"
    )]
    pub fn work_update_on(
        &self,
        work_ref: Option<&str>,
        input: WorkUpdateInput,
        now: DateTime<Utc>,
    ) -> Result<WorkUpdateResult, StoreError> {
        let mut store = self.store_at(now)?;
        let target = self.bind_target(&mut store, work_ref, now)?;
        let basis = self.protocol_basis(&store, true, false, target, now)?;
        let intent = self.protocol_intent(&input);
        let (operation, core_operation, raw_key) = update_metadata(&input);
        let protocol_operation = format!("work_update:{operation}");
        let raw_key =
            self.effective_idempotency_key(raw_key, &protocol_operation, &basis, &intent, now)?;
        let attempt = store.begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
            project_id: &self.project_id,
            session_id: &self.session_id,
            operation: &protocol_operation,
            idempotency_key: &raw_key,
            intent: &intent,
            basis: &basis,
            now,
        })?;
        if let Some(result) = attempt.result {
            return serde_json::from_value(result).map_err(StoreError::from);
        }
        let basis_matches =
            retry_stable_basis_matches(attempt.basis_matches, attempt.basis.as_ref(), &basis)?;
        let scoped_key = self.core_operation_key(&protocol_operation, &raw_key, core_operation)?;
        if let Some(receipt) = store.work_operation_result_value(core_operation, &scoped_key)? {
            let durable_basis: WorkProtocolBasis =
                serde_json::from_value(attempt.basis.ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "core-committed update has no durable attempt basis".into(),
                    )
                })?)?;
            let current = durable_basis
                .focused_work
                .map(|work| work.work_id)
                .ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "core-committed update basis has no focused work".into(),
                    )
                })?;
            let result = self.work_update_result(
                &store,
                operation,
                current,
                agent_update_receipt(operation, receipt)?,
                now,
            )?;
            store.finish_work_protocol_attempt(
                &self.project_id,
                &self.session_id,
                &protocol_operation,
                &raw_key,
                &result,
            )?;
            return Ok(result);
        }
        ensure_protocol_basis(basis_matches, &protocol_operation, &raw_key, false)?;
        let work = basis.focused_work.clone().ok_or_else(|| {
            StoreError::InvalidWorkProjection("update attempt has no bound focused work".into())
        })?;
        if work.lifecycle == WorkLifecycle::Completed
            && !matches!(&input, WorkUpdateInput::Reopen { .. })
        {
            return Err(StoreError::InvalidWork(
                COMPLETED_WORK_LATE_FINDING_REFUSAL.into(),
            ));
        }
        let (_, receipt) = match input {
            WorkUpdateInput::Claim {
                ttl_seconds,
                recovery_reason,
                idempotency_key: _,
            } => {
                let run_id = active_run_id(&work)?;
                let claim = store.claim_work(
                    &ClaimWorkRequest {
                        work_id: work.work_id,
                        expected_work_revision: work.revision,
                        expected_run_id: run_id,
                        holder: self.session_id.clone(),
                        ttl_seconds: ttl_seconds.unwrap_or(DEFAULT_WORK_CLAIM_TTL_SECONDS),
                        recovery_reason,
                        actor: self.actor("work_update", "claim ambient local work"),
                        idempotency_key: scoped_key,
                        claimed_at: now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                ("claim", serde_json::to_value(claim)?)
            }
            WorkUpdateInput::Release {
                reason,
                waiver_reason,
                idempotency_key: _,
            } => {
                let claim = self.live_protocol_claim(&basis, &work, now)?;
                let released = store.release_work(
                    &ReleaseWorkRequest {
                        work_id: work.work_id,
                        run_id: claim.run_id,
                        expected_work_revision: work.revision,
                        holder: self.session_id.clone(),
                        claim_id: claim.claim_id,
                        claim_fence: claim.fence,
                        reason,
                        waiver_reason,
                        actor: self.actor("work_update", "release ambient local work"),
                        idempotency_key: scoped_key,
                        released_at: now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                ("release", serde_json::to_value(released)?)
            }
            WorkUpdateInput::Checkpoint {
                summary,
                evidence,
                idempotency_key: _,
            } => {
                let claim = self.live_protocol_claim(&basis, &work, now)?;
                let checkpoint = store.checkpoint_work(
                    &CheckpointWorkRequest {
                        work_id: work.work_id,
                        run_id: claim.run_id,
                        expected_work_revision: work.revision,
                        holder: self.session_id.clone(),
                        claim_id: claim.claim_id,
                        claim_fence: claim.fence,
                        summary,
                        evidence: evidence.as_deref().map(parse_hashes).transpose()?,
                        actor: self.actor("work_update", "checkpoint ambient local work"),
                        idempotency_key: scoped_key,
                        checkpointed_at: now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                ("checkpoint", serde_json::to_value(checkpoint)?)
            }
            WorkUpdateInput::Evidence {
                summary,
                refs,
                attach,
                idempotency_key: _,
            } => {
                let claim = self.live_protocol_claim(&basis, &work, now)?;
                if let Some(attach) = attach {
                    if !summary.trim().is_empty() || !refs.is_empty() {
                        return Err(StoreError::InvalidWork(
                            "typed evidence attach cannot also supply generic summary or refs"
                                .into(),
                        ));
                    }
                    let evidence = parse_hash(&attach.evidence)?;
                    let evidence_kind = store.work_evidence_kind(claim.run_id, &evidence)?;
                    if evidence_kind == WorkEvidenceKind::Generic {
                        return Err(StoreError::InvalidWork(
                            "typed evidence attach requires verification or environment evidence"
                                .into(),
                        ));
                    }
                    (
                        "evidence",
                        serde_json::json!({
                            "attached": true,
                            "evidence": evidence,
                            "evidence_kind": evidence_kind,
                        }),
                    )
                } else {
                    let evidence = store.record_work_evidence(
                        &RecordWorkEvidenceRequest {
                            work_id: work.work_id,
                            run_id: claim.run_id,
                            expected_work_revision: work.revision,
                            holder: self.session_id.clone(),
                            claim_id: claim.claim_id,
                            claim_fence: claim.fence,
                            summary,
                            refs,
                            actor: self.actor("work_update", "record evidence for ambient work"),
                            idempotency_key: scoped_key,
                            recorded_at: now,
                        },
                        &DevelopmentNoopRedactor,
                    )?;
                    ("evidence", serde_json::to_value(evidence)?)
                }
            }
            WorkUpdateInput::Block {
                blocker_kind,
                detail,
                idempotency_key: _,
            } => {
                let authority = self.planning_authority(basis.claim.as_ref(), &work, now);
                let blocker = store.add_work_blocker(
                    &AddWorkBlockerRequest {
                        work_id: work.work_id,
                        expected_work_revision: work.revision,
                        kind: blocker_kind,
                        detail,
                        authority,
                        actor: self.actor("work_update", "block ambient local work"),
                        idempotency_key: scoped_key,
                        blocked_at: now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                ("block", serde_json::to_value(blocker)?)
            }
            WorkUpdateInput::Unblock {
                blocker_id,
                idempotency_key: _,
            } => {
                let authority = self.planning_authority(basis.claim.as_ref(), &work, now);
                let blocker_id = match blocker_id {
                    Some(blocker_id) if !blocker_id.trim().is_empty() => blocker_id,
                    Some(_) => {
                        return Err(StoreError::InvalidWork(
                            "blocker_id must not be empty; omit it to infer one active blocker"
                                .into(),
                        ));
                    }
                    None => unique_blocker_id(&store.inspect_work(work.work_id, now)?.blockers)?,
                };
                let item = store.clear_work_blocker(
                    &ClearWorkBlockerRequest {
                        work_id: work.work_id,
                        expected_work_revision: work.revision,
                        blocker_id,
                        authority,
                        actor: self.actor("work_update", "clear an ambient work blocker"),
                        idempotency_key: scoped_key,
                        cleared_at: now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                ("unblock", serde_json::to_value(item)?)
            }
            WorkUpdateInput::Revise {
                patch,
                idempotency_key: _,
            } => {
                let authority = self.planning_authority(basis.claim.as_ref(), &work, now);
                let item = store.revise_work(
                    &ReviseWorkRequest {
                        work_id: work.work_id,
                        expected_revision: work.revision,
                        patch,
                        authority,
                        actor: self.actor("work_update", "revise ambient local work"),
                        idempotency_key: scoped_key,
                        updated_at: now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                ("revise", serde_json::to_value(item)?)
            }
            WorkUpdateInput::AddPrerequisite {
                prerequisite,
                idempotency_key: _,
            } => {
                let prerequisite = store.resolve_work_ref(&self.project_id, &prerequisite)?;
                let authority = self.planning_authority(basis.claim.as_ref(), &work, now);
                let item = store.add_work_prerequisite(
                    &ChangeWorkPrerequisiteRequest {
                        work_id: work.work_id,
                        prerequisite_id: prerequisite.work_id,
                        expected_revision: work.revision,
                        authority,
                        actor: self.actor("work_update", "add an ambient work prerequisite"),
                        idempotency_key: scoped_key,
                        changed_at: now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                ("add_prerequisite", serde_json::to_value(item)?)
            }
            WorkUpdateInput::RemovePrerequisite {
                prerequisite,
                idempotency_key: _,
            } => {
                let prerequisite = store.resolve_work_ref(&self.project_id, &prerequisite)?;
                let authority = self.planning_authority(basis.claim.as_ref(), &work, now);
                let item = store.remove_work_prerequisite(
                    &ChangeWorkPrerequisiteRequest {
                        work_id: work.work_id,
                        prerequisite_id: prerequisite.work_id,
                        expected_revision: work.revision,
                        authority,
                        actor: self.actor("work_update", "remove an ambient work prerequisite"),
                        idempotency_key: scoped_key,
                        changed_at: now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                ("remove_prerequisite", serde_json::to_value(item)?)
            }
            WorkUpdateInput::Reopen {
                reason,
                idempotency_key: _,
            } => {
                let item = store.reopen_work(
                    &ReopenWorkRequest {
                        work_id: work.work_id,
                        expected_work_revision: work.revision,
                        reason,
                        actor: self.actor("work_update", "reopen ambient completed work"),
                        idempotency_key: scoped_key,
                        reopened_at: now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                ("reopen", serde_json::to_value(item)?)
            }
            WorkUpdateInput::Cancel {
                reason,
                idempotency_key: _,
            } => {
                let item = store.dispose_work(
                    &DisposeWorkRequest {
                        work_id: work.work_id,
                        expected_work_revision: work.revision,
                        disposition: WorkDisposition::Cancelled,
                        replacement_id: None,
                        reason,
                        actor: self.actor("work_update", "cancel ambient local work"),
                        idempotency_key: scoped_key,
                        disposed_at: now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                ("cancel", serde_json::to_value(item)?)
            }
            WorkUpdateInput::Supersede {
                replacement,
                reason,
                idempotency_key: _,
            } => {
                let replacement = store.resolve_work_ref(&self.project_id, &replacement)?;
                let item = store.dispose_work(
                    &DisposeWorkRequest {
                        work_id: work.work_id,
                        expected_work_revision: work.revision,
                        disposition: WorkDisposition::Superseded,
                        replacement_id: Some(replacement.work_id),
                        reason,
                        actor: self.actor("work_update", "supersede ambient local work"),
                        idempotency_key: scoped_key,
                        disposed_at: now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                ("supersede", serde_json::to_value(item)?)
            }
            WorkUpdateInput::WaiveRequiredChild {
                child,
                reason,
                idempotency_key: _,
            } => {
                let child = store.resolve_work_ref(&self.project_id, &child)?;
                let waiver = store.waive_required_child(
                    &WaiveRequiredChildRequest {
                        parent_id: work.work_id,
                        child_id: child.work_id,
                        expected_parent_revision: work.revision,
                        reason,
                        actor: self.actor(
                            "work_update",
                            "waive a disposed required child from the completion barrier",
                        ),
                        idempotency_key: scoped_key,
                        waived_at: now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                ("waive_required_child", serde_json::to_value(waiver)?)
            }
        };
        let receipt = agent_update_receipt(operation, receipt)?;
        let result = self.work_update_result(&store, operation, work.work_id, receipt, now)?;
        store.finish_work_protocol_attempt(
            &self.project_id,
            &self.session_id,
            &protocol_operation,
            &raw_key,
            &result,
        )?;
        Ok(result)
    }

    fn work_update_result(
        &self,
        store: &SqliteStore,
        operation: &str,
        work_id: WorkId,
        receipt: serde_json::Value,
        now: DateTime<Utc>,
    ) -> Result<WorkUpdateResult, StoreError> {
        let guidance = self.work_guidance(store, work_id, now)?;
        let control_binding = if operation == "claim" {
            guidance
                .claim
                .as_ref()
                .map(|claim| store.get_work_run(claim.run_id))
                .transpose()?
                .as_ref()
                .and_then(|run| {
                    owned_control_work_binding(
                        &guidance.status.work,
                        run,
                        guidance.claim.as_ref(),
                        &self.session_id,
                        now,
                    )
                })
        } else {
            None
        };
        let result = WorkUpdateResult {
            operation: operation.to_owned(),
            receipt: compact_mutation_receipt(&guidance.status.work, control_binding, receipt),
            obligations: compact_obligations(&guidance.status),
            obligation_page: work_obligation_page(store, work_id)?,
            allowed_next: guidance.allowed_next,
        };
        ensure_agent_response_budget(&result, "work_update")?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests;
