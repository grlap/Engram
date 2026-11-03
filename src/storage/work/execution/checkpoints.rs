//! Checkpoint capture and persistence within the caller transaction.

use super::{
    CanonicalObject, DateTime, FeedId, FeedPosition, ObjectId, OptionalExtension, Redactor,
    SCHEMA_VERSION, SqliteStore, StoreError, Transaction, Utc, WorkCheckpoint, WorkClaim,
    WorkEventDraft, WorkItem, WorkLifecycle, WorkRun, WorkRunState, WorkTransition,
    add_root_contribution, append_to_work_feeds, assert_actor_session, checkpoint_feed_end,
    ensure_run_evidence, expect_root_contributor, expire_handoff_offers, feed_head,
    inspect_work_request, load_typed_work_object, normalize_text, persist_operation_result,
    persist_work_run, renew_holder_claim, replay_operation, request_object, unique_hashes,
    validate_evidence_phase_marker, validate_live_claim_on, work_run_evidence_on,
};

impl SqliteStore {
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
    ) -> Result<ObjectId, StoreError> {
        inspect_work_request(redactor, request, &request.actor)?;
        assert_actor_session(&request.actor, &request.holder)?;
        let summary = normalize_text(
            &request.summary,
            crate::storage::refusal_labels::CHECKPOINT_SUMMARY,
        )?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(hash) = replay_operation::<ObjectId>(
            &transaction,
            "checkpoint_work",
            &request.idempotency_key,
            request_object.key(),
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
            request_object.key(),
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
    ) -> Result<(ObjectId, FeedPosition), StoreError>
    where
        R: Redactor,
        F: FnMut(&FeedPosition) -> Result<String, StoreError>,
    {
        inspect_work_request(redactor, request, &request.actor)?;
        assert_actor_session(&request.actor, &request.holder)?;
        let summary = normalize_text(
            &request.summary,
            crate::storage::refusal_labels::CHECKPOINT_SUMMARY,
        )?;
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
                    && stored_request_hash == request_object.key().as_str()
                {
                    let stored_checkpoint: ObjectId = serde_json::from_slice(&result_json)?;
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
            request_object.key(),
            &checkpoint,
        )?;
        transaction.commit()?;
        Ok((checkpoint, current_cut))
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the checkpoint object and event retain the complete exact claim basis"
)]
pub(super) fn persist_work_checkpoint_on(
    transaction: &Transaction<'_>,
    item: &WorkItem,
    mut run: WorkRun,
    claim: WorkClaim,
    summary: String,
    evidence: Vec<ObjectId>,
    actor: &crate::domain::ActorContext,
    checkpointed_at: DateTime<Utc>,
) -> Result<ObjectId, StoreError> {
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
    let object = CanonicalObject::mint(&checkpoint)?;
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
    run.last_checkpoint = Some(object.key().clone());
    run.state = WorkRunState::Active;
    run.revision += 1;
    run.updated_at = checkpointed_at;
    persist_work_run(transaction, &run, claim.fence)?;
    let root_execution =
        super::super::root_state::update(transaction, run.root_execution_id, |root| {
            let changed = expect_root_contributor(root, &claim.holder)
                | add_root_contribution(root, &claim.holder, object.key());
            if changed {
                root.revision += 1;
                root.updated_at = checkpointed_at;
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
            run: Some(run),
            root_execution: Some(root_execution.value().clone()),
            claim: Some(claim),
            handoff_offer: None,
            blocker: None,
            transition: WorkTransition::Checkpointed {
                checkpoint: object.key().clone(),
            },
            actor: actor.clone(),
            created_at: checkpointed_at,
        },
        &root_execution,
    )?;
    Ok(object.key().clone())
}
