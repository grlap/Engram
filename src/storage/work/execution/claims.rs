//! Claim selection, fencing, renewal, and release.

use super::{
    BTreeMap, ClaimNextReadyChildRequest, ClaimWorkRequest, DateTime, FeedId, NextReadyChildClaim,
    Redactor, ReleaseWorkRequest, SCHEMA_VERSION, SqliteStore, StoreError, Transaction, Utc,
    WorkAvailability, WorkClaim, WorkClaimId, WorkClaimState, WorkEventDraft, WorkId, WorkItem,
    WorkLifecycle, WorkRelease, WorkRunState, WorkTransition, append_work_event,
    assert_actor_session, assert_revision, claim_expiry, ensure_restored_execution_state,
    expect_root_contributor, expire_handoff_offers, held_open_child_on, inspect_work_canonical_on,
    inspect_work_request, load_root_execution, load_work_claim_optional, load_work_item,
    load_work_run, normalize_text, open_children_by_ready_order_on, persist_claim,
    persist_operation_result, persist_root_execution, persist_work_run, replay_operation,
    request_object, root_participant_is_accounted, validate_live_claim_for_item_on,
    validate_live_claim_on, waive_root_contributor,
};

impl SqliteStore {
    /// Atomically claims ready work, renews a live claim held by the caller,
    /// or recovers an expired/released claim. Only a nonempty explicit key
    /// requests exact replay; a keyless call is a fresh claim/renewal.
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
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        let keyed = !request.idempotency_key.is_empty();
        if keyed
            && let Some(claim) = replay_operation::<WorkClaim>(
                &transaction,
                "claim_work",
                &request.idempotency_key,
                request_object.key(),
            )?
        {
            transaction.commit()?;
            return Ok(claim);
        }
        let claim = Self::claim_work_on(&transaction, request)?;
        if keyed {
            persist_operation_result(
                &transaction,
                "claim_work",
                &request.idempotency_key,
                request_object.key(),
                &claim,
            )?;
        }
        let journaled = Self::claimed_binding_on(
            &transaction,
            &request.holder,
            claim.work_id,
            request.claimed_at,
        );
        transaction.commit()?;
        if let Some((project_id, binding)) = journaled {
            super::super::focus_journal::record_binding(
                &project_id,
                &request.holder,
                claim.work_id,
                binding,
            );
        }
        Ok(claim)
    }

    /// The binding a claim just taken, renewed or recovered leaves on
    /// `work_id`, read in the claim's transaction for a word that discloses
    /// its focus change; `None` when no journal is installed. Like every
    /// disclosure read, it never refuses the claim: a failed read records
    /// nothing.
    pub(super) fn claimed_binding_on(
        connection: &rusqlite::Connection,
        holder: &crate::domain::SessionId,
        work_id: WorkId,
        now: DateTime<Utc>,
    ) -> Option<(crate::domain::ProjectId, Option<super::super::FocusBinding>)> {
        if !super::super::focus_journal::recording() {
            return None;
        }
        let item = load_work_item(connection, work_id).ok()?;
        let binding = super::super::focus_journal::focus_binding_on(
            connection,
            &item.project_id,
            holder,
            &item,
            now,
        );
        Some((item.project_id, binding))
    }

    /// Selects the next ready direct child of one parent and claims it in the
    /// same write transaction. Candidates are the parent's open children in
    /// the `ls --ready` order (priority, then work id) whose canonical basis
    /// is ready; blocked, deferred, held and terminal children are never
    /// selected. A child this holder already holds under the parent is the
    /// answer to a repeat: the call renews it instead of taking a second one.
    /// A ready child whose prior claim lapsed under an unaccounted holder is
    /// passed over unless the request carries a recovery reason. A nonempty
    /// key replays the original selection exactly.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the parent is unknown, no child can be
    /// claimed, the request conflicts with an idempotent retry, or
    /// persistence fails. A refusal claims nothing.
    pub fn claim_next_ready_child<R: Redactor>(
        &mut self,
        request: &ClaimNextReadyChildRequest,
        redactor: &R,
    ) -> Result<NextReadyChildClaim, StoreError> {
        inspect_work_request(redactor, request, &request.actor)?;
        assert_actor_session(&request.actor, &request.holder)?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        let keyed = !request.idempotency_key.is_empty();
        if keyed
            && let Some(selection) = replay_operation::<NextReadyChildClaim>(
                &transaction,
                "claim_next_ready_child",
                &request.idempotency_key,
                request_object.key(),
            )?
        {
            transaction.commit()?;
            return Ok(selection);
        }
        let parent = load_work_item(&transaction, request.parent_id)?;
        // The caller's own child is found from claim metadata alone, under
        // whatever word the projection gives it: a renewal keeps a live claim
        // alive and must not depend on reading any sibling.
        let held = held_open_child_on(
            &transaction,
            parent.work_id,
            &request.holder,
            request.claimed_at,
        )?;
        let mut candidates = Vec::new();
        let mut others = BTreeMap::new();
        for (child_id, availability) in
            open_children_by_ready_order_on(&transaction, parent.work_id, request.claimed_at)?
        {
            if availability == "ready" {
                candidates.push(child_id);
            } else {
                *others.entry(availability).or_insert(0) += 1;
            }
        }
        let claim_request = |item: &WorkItem, recovery_reason: Option<String>| ClaimWorkRequest {
            work_id: item.work_id,
            expected_work_revision: item.revision,
            expected_run_id: item.active_run_id,
            holder: request.holder.clone(),
            ttl_seconds: request.ttl_seconds,
            recovery_reason,
            actor: request.actor.clone(),
            idempotency_key: String::new(),
            claimed_at: request.claimed_at,
        };
        let selection = if let Some(child_id) = held {
            // The claim body revalidates the child's canonical authority. No
            // sibling is inspected, so the count is the projection's.
            let item = load_work_item(&transaction, child_id)?;
            let claim = Self::claim_work_on(&transaction, &claim_request(&item, None))?;
            NextReadyChildClaim {
                parent_id: parent.work_id,
                work_id: item.work_id,
                claim,
                position: None,
                ready_count: candidates.len(),
                renewed: true,
            }
        } else {
            let ready = Self::verified_ready_children_on(
                &transaction,
                candidates,
                request.claimed_at,
                &mut others,
            )?;
            let (item, position) =
                Self::select_next_ready_child_on(&transaction, &ready, request, &others)?;
            let claim = Self::claim_work_on(
                &transaction,
                &claim_request(&item, request.recovery_reason.clone()),
            )?;
            NextReadyChildClaim {
                parent_id: parent.work_id,
                work_id: item.work_id,
                claim,
                position: Some(position),
                ready_count: ready.len(),
                renewed: false,
            }
        };
        if keyed {
            persist_operation_result(
                &transaction,
                "claim_next_ready_child",
                &request.idempotency_key,
                request_object.key(),
                &selection,
            )?;
        }
        let journaled = Self::claimed_binding_on(
            &transaction,
            &request.holder,
            selection.work_id,
            request.claimed_at,
        );
        transaction.commit()?;
        if let Some((project_id, binding)) = journaled {
            super::super::focus_journal::record_binding(
                &project_id,
                &request.holder,
                selection.work_id,
                binding,
            );
        }
        Ok(selection)
    }

    /// The projection narrows the candidates; the canonical basis decides, so
    /// an entry it does not confirm is counted as not ready and left out.
    /// Returns the verified ready children's ids in their order. A fresh
    /// selection is strict: a candidate whose basis cannot be read fails it.
    fn verified_ready_children_on(
        transaction: &Transaction<'_>,
        candidates: Vec<WorkId>,
        now: DateTime<Utc>,
        others: &mut BTreeMap<String, usize>,
    ) -> Result<Vec<WorkId>, StoreError> {
        let mut verified = Vec::new();
        for child_id in candidates {
            let view = inspect_work_canonical_on(transaction, child_id, now)?;
            if matches!(view.availability, WorkAvailability::Ready) {
                verified.push(child_id);
            } else {
                *others.entry("not ready".to_owned()).or_insert(0) += 1;
            }
        }
        Ok(verified)
    }

    /// Returns the first verified ready child this request may claim and its
    /// one-based place in that order; only a child it considers is loaded.
    fn select_next_ready_child_on(
        transaction: &Transaction<'_>,
        ready: &[WorkId],
        request: &ClaimNextReadyChildRequest,
        others: &BTreeMap<String, usize>,
    ) -> Result<(WorkItem, usize), StoreError> {
        let mut lapsed = 0;
        for (index, child_id) in ready.iter().enumerate() {
            let item = load_work_item(transaction, *child_id)?;
            if request.recovery_reason.is_none()
                && Self::lapsed_under_unaccounted_holder_on(transaction, &item, &request.holder)?
            {
                lapsed += 1;
                continue;
            }
            return Ok((item, index + 1));
        }
        Err(StoreError::InvalidWork(Self::no_ready_child_reason(
            others, lapsed,
        )))
    }

    /// A ready child whose last claim belongs to a different holder that the
    /// root execution has neither credited nor waived needs attributed
    /// recovery, exactly as an explicit claim of it would.
    fn lapsed_under_unaccounted_holder_on(
        transaction: &Transaction<'_>,
        item: &WorkItem,
        holder: &crate::domain::SessionId,
    ) -> Result<bool, StoreError> {
        let Some(run_id) = item.active_run_id else {
            return Ok(false);
        };
        let Some(prior) = load_work_claim_optional(transaction, run_id)? else {
            return Ok(false);
        };
        if prior.holder == *holder {
            return Ok(false);
        }
        let run = load_work_run(transaction, run_id)?;
        let execution = load_root_execution(transaction, run.root_execution_id)?;
        Ok(!root_participant_is_accounted(&execution, &prior.holder))
    }

    fn no_ready_child_reason(others: &BTreeMap<String, usize>, lapsed: usize) -> String {
        let mut parts = others
            .iter()
            .map(|(availability, count)| format!("{count} {availability}"))
            .collect::<Vec<_>>();
        if lapsed > 0 {
            parts.push(format!(
                "{lapsed} ready but lapsed under another holder; pass a recovery reason to take one over"
            ));
        }
        if parts.is_empty() {
            return "no ready child to claim: the item has no open children".into();
        }
        format!("no ready child to claim: {}", parts.join(", "))
    }

    /// The claim body shared by an explicit claim and a next-ready selection;
    /// the caller owns replay and commit.
    fn claim_work_on(
        transaction: &Transaction<'_>,
        request: &ClaimWorkRequest,
    ) -> Result<WorkClaim, StoreError> {
        let expires_at = claim_expiry(request.claimed_at, request.ttl_seconds)?;
        let mut item = load_work_item(transaction, request.work_id)?;
        assert_revision(&item, request.expected_work_revision)?;
        if item.active_run_id != request.expected_run_id {
            return Err(StoreError::InvalidWorkProjection(
                "claim request does not match the current active run".into(),
            ));
        }
        if let Some(run_id) = request.expected_run_id {
            expire_handoff_offers(transaction, run_id, request.claimed_at, &request.actor)?;
        }
        if item.lifecycle != WorkLifecycle::Open {
            return Err(StoreError::WorkNotOpen(item.work_id));
        }
        if let Some(run_id) = item.active_run_id
            && let Some(claim) = load_work_claim_optional(transaction, run_id)?
            && claim.holder == request.holder
            && claim.state == WorkClaimState::Active
            && claim.expires_at > request.claimed_at
        {
            let (item, run, mut claim) = validate_live_claim_for_item_on(
                transaction,
                item,
                run_id,
                request.expected_work_revision,
                &request.holder,
                claim.claim_id,
                claim.fence,
                request.claimed_at,
                false,
            )?;
            let root_execution = load_root_execution(transaction, run.root_execution_id)?;
            claim.expires_at = claim.expires_at.max(expires_at);
            claim.revision += 1;
            persist_claim(transaction, &claim)?;
            append_work_event(
                transaction,
                &WorkEventDraft {
                    schema_version: SCHEMA_VERSION,
                    project_id: item.project_id.clone(),
                    root_id: item.root_id,
                    work_id: item.work_id,
                    run_id: Some(run_id),
                    revision: item.revision,
                    work: item,
                    run: Some(run),
                    root_execution: Some(root_execution),
                    claim: Some(claim.clone()),
                    handoff_offer: None,
                    blocker: None,
                    transition: WorkTransition::ClaimRenewed {
                        claim: claim.clone(),
                    },
                    actor: request.actor.clone(),
                    created_at: request.claimed_at,
                },
            )?;
            return Ok(claim);
        }
        super::super::task_sync::reconciliation::refuse_held_on(transaction, item.work_id)?;
        let view = inspect_work_canonical_on(transaction, item.work_id, request.claimed_at)?;
        if !matches!(view.availability, WorkAvailability::Ready) {
            if matches!(
                view.availability,
                WorkAvailability::Claimed | WorkAvailability::Active
            ) && let Some(run_id) = item.active_run_id
                && let Some(claim) = load_work_claim_optional(transaction, run_id)?
            {
                return Err(StoreError::WorkClaimHeld {
                    work: item.work_id,
                    holder: claim.holder.0,
                    expires_at: claim.expires_at.timestamp_millis(),
                });
            }
            if let Some(ancestor) = view.blocking_ancestor {
                return Err(StoreError::WorkAncestorNotOpen {
                    work: item.work_id,
                    ancestor,
                });
            }
            return Err(StoreError::InvalidWork(format!(
                "work is not ready: {:?}",
                view.availability
            )));
        }
        let (mut root_execution, mut run, created_run) =
            ensure_restored_execution_state(transaction, &mut item, request.claimed_at)?;
        let run_id = run.run_id;
        let prior = if created_run {
            None
        } else {
            load_work_claim_optional(transaction, run_id)?
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
            persist_root_execution(transaction, &root_execution)?;
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
        persist_claim(transaction, &claim)?;
        persist_work_run(transaction, &run, claim.fence)?;
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
        let (event_id, positions) = append_work_event(transaction, &event)?;
        // A run this claim is the first to hold may still owe the obligations
        // its bound criteria open; a run that already holds them keeps them.
        if let Some(position) = positions
            .iter()
            .find(|position| position.feed == FeedId::RunExecution(run_id))
        {
            super::super::completion::open_binding_obligations_on(
                transaction,
                &item,
                &run,
                &event_id,
                position,
                &[],
                request.claimed_at,
            )?;
        }
        Ok(claim)
    }

    /// Releases a live claim and advances its fence without reviving old authority.
    /// A holder with neither a contribution nor a waiver under the root
    /// execution needs `waiver_reason`, recorded as its participant waiver;
    /// the result says whether this release recorded one.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the exact work, run, claim, revision, holder,
    /// or fence basis is stale, or persistence fails.
    pub fn release_work<R: Redactor>(
        &mut self,
        request: &ReleaseWorkRequest,
        redactor: &R,
    ) -> Result<WorkRelease, StoreError> {
        inspect_work_request(redactor, request, &request.actor)?;
        assert_actor_session(&request.actor, &request.holder)?;
        let reason = normalize_text(
            &request.reason,
            crate::storage::refusal_labels::RELEASE_REASON,
        )?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(release) = replay_operation::<WorkRelease>(
            &transaction,
            "release_work",
            &request.idempotency_key,
            request_object.key(),
        )? {
            transaction.commit()?;
            return Ok(release);
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
        let mut waiver_recorded = false;
        if !root_participant_is_accounted(&root_execution, &claim.holder) {
            let reason =
                request
                    .waiver_reason
                    .as_deref()
                    .ok_or(StoreError::WorkReleaseWaiverRequired {
                        work: request.work_id,
                    })?;
            let reason = normalize_text(reason, "completion waiver reason")?;
            if waive_root_contributor(
                &mut root_execution,
                &claim.holder,
                &request.actor.actor_id,
                &reason,
            ) {
                waiver_recorded = true;
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
        let release = WorkRelease {
            claim,
            waiver_recorded: Some(waiver_recorded),
        };
        persist_operation_result(
            &transaction,
            "release_work",
            &request.idempotency_key,
            request_object.key(),
            &release,
        )?;
        transaction.commit()?;
        // The released claim no longer binds; a replay above ended nothing.
        super::super::focus_journal::record_ended(&release.claim.holder, release.claim.work_id);
        Ok(release)
    }
}
