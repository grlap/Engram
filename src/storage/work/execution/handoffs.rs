use super::{
    AcceptWorkHandoffRequest, CancelWorkHandoffRequest, CanonicalObject, Connection, FeedId,
    FeedPosition, ObjectHash, OfferWorkHandoffRequest, OptionalExtension, Redactor, SCHEMA_VERSION,
    SqliteStore, StoreError, WorkCheckpoint, WorkClaim, WorkEvent, WorkEventDraft,
    WorkHandoffOffer, WorkHandoffOfferId, WorkHandoffState, WorkLifecycle, WorkRunState,
    WorkTransition, add_root_contribution, append_to_work_feeds, append_work_event,
    assert_actor_session, claim_expiry, expect_root_contributor, expire_handoff_offers, feed_head,
    inspect_work_request, load_handoff_offer_projection, load_root_execution, normalize_text,
    params, persist_claim, persist_operation_result, persist_root_execution, persist_work_run,
    renew_holder_claim, replay_operation, request_object, validate_evidence_phase_marker,
    validate_live_claim_on,
};

#[cfg(test)]
mod tests;

pub(in crate::storage::work) fn latest_canonical_handoff_offer(
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

impl SqliteStore {
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
