use super::*;

impl LocalWorkService {
    /// Offers, accepts, or cancels a checkpoint-coupled ambient handoff.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when no unique matching offer exists or claim
    /// fences changed.
    pub fn work_handoff(
        &self,
        input: WorkHandoffInput,
        now: DateTime<Utc>,
    ) -> Result<WorkHandoffResult, StoreError> {
        self.work_handoff_on(None, input, now)
    }

    /// Like [`Self::work_handoff`], but first binds `work_ref` as the ambient
    /// focus and the handoff target inside the same call.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] under the same conditions as
    /// [`Self::work_handoff`], or when `work_ref` does not resolve inside the
    /// project.
    pub fn work_handoff_on(
        &self,
        work_ref: Option<&str>,
        input: WorkHandoffInput,
        now: DateTime<Utc>,
    ) -> Result<WorkHandoffResult, StoreError> {
        let mut store = self.store_at(now)?;
        let target = self.bind_target(&mut store, work_ref, now)?;
        let basis = self.protocol_basis(&store, true, true, target, now)?;
        let intent = self.protocol_intent(&input);
        let (operation, core_operation, raw_key) = handoff_metadata(&input);
        let protocol_operation = format!("work_handoff:{operation}");
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
                        "core-committed handoff has no durable attempt basis".into(),
                    )
                })?)?;
            let current = durable_basis
                .focused_work
                .map(|work| work.work_id)
                .ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "core-committed handoff basis has no focused work".into(),
                    )
                })?;
            let result = self.work_handoff_result(&store, operation, current, receipt, now)?;
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
            StoreError::InvalidWorkProjection("handoff attempt has no bound focused work".into())
        })?;
        let receipt =
            self.execute_work_handoff(&mut store, &basis, &work, input, scoped_key, now)?;
        let result = self.work_handoff_result(&store, operation, work.work_id, receipt, now)?;
        store.finish_work_protocol_attempt(
            &self.project_id,
            &self.session_id,
            &protocol_operation,
            &raw_key,
            &result,
        )?;
        Ok(result)
    }

    fn work_handoff_result(
        &self,
        store: &SqliteStore,
        operation: &str,
        work_id: WorkId,
        receipt: serde_json::Value,
        now: DateTime<Utc>,
    ) -> Result<WorkHandoffResult, StoreError> {
        let guidance = self.work_guidance(store, work_id, now)?;
        let result = WorkHandoffResult {
            operation: operation.to_owned(),
            receipt: compact_mutation_receipt(&guidance.status.work, None, receipt),
            obligations: compact_obligations(&guidance.status),
            allowed_next: guidance.allowed_next,
        };
        ensure_agent_response_budget(&result, "work_handoff")?;
        Ok(result)
    }

    fn execute_work_handoff(
        &self,
        store: &mut SqliteStore,
        basis: &WorkProtocolBasis,
        work: &WorkItem,
        input: WorkHandoffInput,
        scoped_key: String,
        now: DateTime<Utc>,
    ) -> Result<serde_json::Value, StoreError> {
        match input {
            WorkHandoffInput::Offer {
                to,
                ttl_seconds,
                checkpoint_summary,
                idempotency_key: _,
            } => {
                let claim = self.live_protocol_claim(basis, work, now)?;
                let offer = store.offer_work_handoff(
                    &OfferWorkHandoffRequest {
                        work_id: work.work_id,
                        run_id: claim.run_id,
                        expected_work_revision: work.revision,
                        from: self.session_id.clone(),
                        to: SessionId(to),
                        claim_id: claim.claim_id,
                        claim_fence: claim.fence,
                        ttl_seconds: ttl_seconds.unwrap_or(DEFAULT_WORK_CLAIM_TTL_SECONDS),
                        checkpoint_summary,
                        actor: self.actor("work_handoff", "offer ambient work handoff"),
                        idempotency_key: scoped_key,
                        offered_at: now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                serde_json::to_value(offer).map_err(StoreError::from)
            }
            WorkHandoffInput::Accept { idempotency_key: _ } => {
                let offer = unique_offer(
                    basis
                        .handoffs
                        .iter()
                        .filter(|offer| {
                            offer.state == WorkHandoffState::Offered
                                && offer.to == self.session_id
                                && offer.expires_at > now
                        })
                        .cloned(),
                    "incoming",
                )?;
                let claim = store.accept_work_handoff(
                    &AcceptWorkHandoffRequest {
                        work_id: work.work_id,
                        offer_id: offer.offer_id,
                        to: self.session_id.clone(),
                        actor: self.actor("work_handoff", "accept ambient work handoff"),
                        idempotency_key: scoped_key,
                        accepted_at: now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                serde_json::to_value(claim).map_err(StoreError::from)
            }
            WorkHandoffInput::Cancel {
                reason,
                idempotency_key: _,
            } => {
                let offer = unique_offer(
                    basis
                        .handoffs
                        .iter()
                        .filter(|offer| {
                            offer.state == WorkHandoffState::Offered
                                && offer.from == self.session_id
                                && offer.expires_at > now
                        })
                        .cloned(),
                    "outgoing",
                )?;
                let claim = self.live_protocol_claim(basis, work, now)?;
                let offer = store.cancel_work_handoff(
                    &CancelWorkHandoffRequest {
                        work_id: work.work_id,
                        run_id: claim.run_id,
                        expected_work_revision: work.revision,
                        holder: self.session_id.clone(),
                        offer_id: offer.offer_id,
                        claim_id: claim.claim_id,
                        claim_fence: claim.fence,
                        reason,
                        actor: self.actor("work_handoff", "cancel ambient work handoff"),
                        idempotency_key: scoped_key,
                        cancelled_at: now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                serde_json::to_value(offer).map_err(StoreError::from)
            }
        }
    }
}

#[cfg(test)]
mod tests;
