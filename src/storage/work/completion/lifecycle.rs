use super::{
    ChildRequirement, DisposeWorkRequest, OptionalExtension, Redactor, ReopenWorkRequest,
    RequiredChildWaiver, RootExecution, RootExecutionId, RootExecutionState, SCHEMA_VERSION,
    SqliteStore, StoreError, WaiveRequiredChildRequest, WorkClaimState, WorkDisposition,
    WorkEventDraft, WorkItem, WorkLifecycle, WorkRun, WorkRunId, WorkRunState, WorkTransition,
    active_root_execution, append_work_event, assert_actor_session, assert_revision,
    combined_graph_is_acyclic_with_dependency, ensure_restored_execution_state,
    expire_handoff_offers, inspect_work_request, load_root_execution, load_work_claim_optional,
    load_work_item, load_work_run, normalize_text, params, persist_claim, persist_operation_result,
    persist_root_execution, persist_work_item, persist_work_run, refuse_completed_ancestor,
    replay_operation, request_object, waive_root_contributor, work_completed_by_restored_record_on,
};

#[cfg(test)]
mod tests;

impl SqliteStore {
    /// Reopens completed work as a clean run generation without reviving authority.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the expected revision changed, an ancestor
    /// already consumed the child seal, or the new generation cannot be persisted.
    pub fn reopen_work<R: Redactor>(
        &mut self,
        request: &ReopenWorkRequest,
        redactor: &R,
    ) -> Result<WorkRun, StoreError> {
        inspect_work_request(redactor, request, &request.actor)?;
        let reason = normalize_text(&request.reason, "reopen reason")?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(run) = replay_operation::<WorkRun>(
            &transaction,
            "reopen_work",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(run);
        }
        let mut item = load_work_item(&transaction, request.work_id)?;
        assert_revision(&item, request.expected_work_revision)?;
        if item.lifecycle != WorkLifecycle::Completed {
            return Err(StoreError::InvalidWork(
                "only completed work can be reopened".into(),
            ));
        }
        if item.parent_id.is_some() {
            refuse_completed_ancestor(&transaction, &item)?;
        } else {
            let open_descendants = transaction.query_row(
                "WITH RECURSIVE descendants(work_id) AS (
                     SELECT work_id FROM work_items WHERE parent_id = ?1
                     UNION
                     SELECT child.work_id FROM work_items child
                     JOIN descendants parent ON child.parent_id = parent.work_id
                 )
                 SELECT COUNT(*) FROM descendants
                 JOIN work_items item USING(work_id)
                 WHERE item.lifecycle IN ('proposed', 'open')",
                [item.work_id.0.to_string()],
                |row| row.get::<_, i64>(0),
            )?;
            if open_descendants != 0 {
                return Err(StoreError::InvalidWork(
                    "dispose unfinished descendants before reopening a completed root execution"
                        .into(),
                ));
            }
        }
        if work_completed_by_restored_record_on(&transaction, &item)? {
            item.lifecycle = WorkLifecycle::Open;
            item.revision += 1;
            item.updated_at = request.reopened_at;
            let (root_execution, run, created) =
                ensure_restored_execution_state(&transaction, &mut item, request.reopened_at)?;
            if !created {
                return Err(StoreError::InvalidWorkProjection(
                    "restored reopen did not create a fresh run".into(),
                ));
            }
            let event = WorkEventDraft {
                schema_version: SCHEMA_VERSION,
                project_id: item.project_id.clone(),
                root_id: item.root_id,
                work_id: item.work_id,
                run_id: Some(run.run_id),
                revision: item.revision,
                work: item,
                run: Some(run.clone()),
                root_execution: Some(root_execution),
                claim: None,
                handoff_offer: None,
                blocker: None,
                transition: WorkTransition::Reopened {
                    run_id: run.run_id,
                    generation: run.generation,
                    reason,
                },
                actor: request.actor.clone(),
                created_at: request.reopened_at,
            };
            append_work_event(&transaction, &event)?;
            persist_operation_result(
                &transaction,
                "reopen_work",
                &request.idempotency_key,
                request_object.hash(),
                &run,
            )?;
            transaction.commit()?;
            return Ok(run);
        }
        let generation = transaction.query_row(
            "SELECT COALESCE(MAX(generation), 0) + 1 FROM work_runs WHERE work_id = ?1",
            [item.work_id.0.to_string()],
            |row| row.get::<_, i64>(0),
        )?;
        let root_execution = if item.work_id == item.root_id {
            let root_generation = transaction.query_row(
                "SELECT COALESCE(MAX(generation), 0) + 1
                 FROM work_root_executions WHERE root_id = ?1",
                [item.root_id.0.to_string()],
                |row| row.get::<_, i64>(0),
            )?;
            let execution = RootExecution {
                schema_version: SCHEMA_VERSION,
                root_execution_id: RootExecutionId::new(),
                project_id: item.project_id.clone(),
                root_id: item.root_id,
                generation: root_generation,
                state: RootExecutionState::Active,
                revision: 1,
                run_ids: Vec::new(),
                required_child_seals: Vec::new(),
                required_child_waivers: Vec::new(),
                expected_contributors: Vec::new(),
                contributions: Vec::new(),
                waivers: Vec::new(),
                created_at: request.reopened_at,
                updated_at: request.reopened_at,
            };
            transaction.execute(
                "INSERT INTO work_root_executions (
                     root_execution_id, project_id, root_id, generation, state,
                     revision, created_at_ms, updated_at_ms, execution_json
                 ) VALUES (?1, ?2, ?3, ?4, 'active', 1, ?5, ?6, ?7)",
                params![
                    execution.root_execution_id.0.to_string(),
                    execution.project_id.0,
                    execution.root_id.0.to_string(),
                    execution.generation,
                    execution.created_at.timestamp_millis(),
                    execution.updated_at.timestamp_millis(),
                    serde_json::to_vec(&execution)?
                ],
            )?;
            execution
        } else {
            let mut execution = active_root_execution(&transaction, item.root_id)?;
            if item.child_requirement == ChildRequirement::Required {
                let old_seal: Option<String> = transaction
                    .query_row(
                        "SELECT seal_hash FROM work_completion_seals WHERE work_id = ?1
                         ORDER BY rowid DESC LIMIT 1",
                        [item.work_id.0.to_string()],
                        |row| row.get(0),
                    )
                    .optional()?;
                if let Some(old_seal) = old_seal {
                    execution
                        .required_child_seals
                        .retain(|hash| hash.as_str() != old_seal);
                    execution.revision += 1;
                    execution.updated_at = request.reopened_at;
                    persist_root_execution(&transaction, &execution)?;
                }
            }
            execution
        };
        let run = WorkRun {
            schema_version: SCHEMA_VERSION,
            run_id: WorkRunId::new(),
            root_execution_id: root_execution.root_execution_id,
            work_id: item.work_id,
            generation,
            executor: None,
            state: WorkRunState::Open,
            revision: 1,
            last_checkpoint: None,
            completion_seal: None,
            created_at: request.reopened_at,
            updated_at: request.reopened_at,
        };
        let mut root_execution = root_execution;
        if !root_execution.run_ids.contains(&run.run_id) {
            root_execution.run_ids.push(run.run_id);
            root_execution.run_ids.sort_by_key(|run_id| run_id.0);
            root_execution.revision += 1;
            root_execution.updated_at = request.reopened_at;
            persist_root_execution(&transaction, &root_execution)?;
        }
        transaction.execute(
            "INSERT INTO work_runs (
                 run_id, root_execution_id, work_id, generation,
                 executor_session_id, state, revision, claim_fence_head,
                 last_checkpoint_hash, completion_seal_hash,
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
        item.lifecycle = WorkLifecycle::Open;
        item.active_run_id = Some(run.run_id);
        item.revision += 1;
        item.updated_at = request.reopened_at;
        persist_work_item(&transaction, &item)?;
        let event = WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: Some(run.run_id),
            revision: item.revision,
            work: item.clone(),
            run: Some(run.clone()),
            root_execution: Some(root_execution.clone()),
            claim: None,
            handoff_offer: None,
            blocker: None,
            transition: WorkTransition::Reopened {
                run_id: run.run_id,
                generation: run.generation,
                reason,
            },
            actor: request.actor.clone(),
            created_at: request.reopened_at,
        };
        append_work_event(&transaction, &event)?;
        persist_operation_result(
            &transaction,
            "reopen_work",
            &request.idempotency_key,
            request_object.hash(),
            &run,
        )?;
        transaction.commit()?;
        Ok(run)
    }

    /// Cancels or supersedes open work without recording false completion.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when authority, revision, claim ownership,
    /// replacement linkage, or descendant-drain invariants are not satisfied.
    pub fn dispose_work<R: Redactor>(
        &mut self,
        request: &DisposeWorkRequest,
        redactor: &R,
    ) -> Result<WorkItem, StoreError> {
        inspect_work_request(redactor, request, &request.actor)?;
        let reason = normalize_text(&request.reason, "work disposal reason")?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(item) = replay_operation::<WorkItem>(
            &transaction,
            "dispose_work",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(item);
        }
        let mut item = load_work_item(&transaction, request.work_id)?;
        assert_revision(&item, request.expected_work_revision)?;
        if item.lifecycle != WorkLifecycle::Open {
            return Err(StoreError::WorkNotOpen(item.work_id));
        }
        let open_descendants = transaction.query_row(
            "WITH RECURSIVE descendants(work_id) AS (
                 SELECT work_id FROM work_items WHERE parent_id = ?1
                 UNION
                 SELECT child.work_id FROM work_items child
                 JOIN descendants parent ON child.parent_id = parent.work_id
             )
             SELECT COUNT(*) FROM descendants
             JOIN work_items item USING(work_id)
             WHERE item.lifecycle IN ('proposed', 'open')",
            [item.work_id.0.to_string()],
            |row| row.get::<_, i64>(0),
        )?;
        if open_descendants != 0 {
            return Err(StoreError::InvalidWork(
                "dispose open descendants before disposing their parent".into(),
            ));
        }
        let replacement = match (request.disposition, request.replacement_id) {
            (WorkDisposition::Cancelled, None) => None,
            (WorkDisposition::Cancelled, Some(_)) => {
                return Err(StoreError::InvalidWork(
                    "cancelled work must not name a replacement".into(),
                ));
            }
            (WorkDisposition::Superseded, None) => {
                return Err(StoreError::InvalidWork(
                    "superseded work requires a replacement".into(),
                ));
            }
            (WorkDisposition::Superseded, Some(replacement_id)) => {
                if replacement_id == item.work_id {
                    return Err(StoreError::InvalidWork(
                        "work cannot supersede itself".into(),
                    ));
                }
                let replacement = load_work_item(&transaction, replacement_id)?;
                if replacement.project_id != item.project_id
                    || matches!(
                        replacement.lifecycle,
                        WorkLifecycle::Cancelled | WorkLifecycle::Superseded
                    )
                {
                    return Err(StoreError::InvalidWork(
                        "replacement must be live or completed work in the same project".into(),
                    ));
                }
                if !combined_graph_is_acyclic_with_dependency(
                    &transaction,
                    &item.project_id.0,
                    Some((item.work_id, replacement.work_id)),
                )? {
                    return Err(StoreError::WorkDependencyCycle);
                }
                Some(replacement)
            }
        };
        let restored_execution = if item.active_run_id.is_none() {
            Some(ensure_restored_execution_state(
                &transaction,
                &mut item,
                request.disposed_at,
            )?)
        } else {
            None
        };
        let mut run = if let Some((_, run, _)) = restored_execution.as_ref() {
            Some(run.clone())
        } else {
            item.active_run_id
                .map(|run_id| load_work_run(&transaction, run_id))
                .transpose()?
        };
        let mut claim = if restored_execution.is_some() {
            None
        } else if let Some(run) = run.as_ref() {
            expire_handoff_offers(
                &transaction,
                run.run_id,
                request.disposed_at,
                &request.actor,
            )?;
            load_work_claim_optional(&transaction, run.run_id)?
        } else {
            None
        };
        let unaccounted_holder = claim
            .as_ref()
            .filter(|claim| claim.state == WorkClaimState::Active)
            .map(|claim| claim.holder.clone());
        if let Some(current) = claim.as_ref()
            && current.state == WorkClaimState::Active
            && current.expires_at > request.disposed_at
        {
            assert_actor_session(&request.actor, &current.holder)?;
        }
        let claim_fence = if let Some(current) = claim.as_mut() {
            if current.state == WorkClaimState::Active {
                current.state = WorkClaimState::Released;
                current.revision += 1;
                current.fence += 1;
                current.expires_at = request.disposed_at;
                persist_claim(&transaction, current)?;
            }
            current.fence
        } else if let Some(run) = run.as_ref() {
            transaction.query_row(
                "SELECT claim_fence_head FROM work_runs WHERE run_id = ?1",
                [run.run_id.0.to_string()],
                |row| row.get::<_, i64>(0),
            )?
        } else {
            0
        };
        if let Some(current_run) = run.as_mut() {
            current_run.executor = None;
            current_run.state = WorkRunState::Cancelled;
            current_run.revision += 1;
            current_run.updated_at = request.disposed_at;
            persist_work_run(&transaction, current_run, claim_fence)?;
        }
        item.lifecycle = match request.disposition {
            WorkDisposition::Cancelled => WorkLifecycle::Cancelled,
            WorkDisposition::Superseded => WorkLifecycle::Superseded,
        };
        item.superseded_by = replacement.as_ref().map(|work| work.work_id);
        item.active_run_id = None;
        item.revision += 1;
        item.updated_at = request.disposed_at;
        persist_work_item(&transaction, &item)?;
        let mut root_execution = if let Some((execution, _, _)) = restored_execution {
            execution
        } else if let Some(current_run) = run.as_ref() {
            load_root_execution(&transaction, current_run.root_execution_id)?
        } else {
            active_root_execution(&transaction, item.root_id)?
        };
        let mut root_changed = false;
        if item.work_id != item.root_id
            && let Some(holder) = unaccounted_holder
            && !root_execution
                .contributions
                .iter()
                .any(|contribution| contribution.participant == holder)
            && !root_execution
                .waivers
                .iter()
                .any(|waiver| waiver.participant == holder)
        {
            root_changed |= waive_root_contributor(
                &mut root_execution,
                &holder,
                &request.actor.actor_id,
                &reason,
            );
        }
        if item.work_id == item.root_id {
            root_execution.state = RootExecutionState::Cancelled;
            root_changed = true;
        }
        if root_changed {
            root_execution.revision += 1;
            root_execution.updated_at = request.disposed_at;
            persist_root_execution(&transaction, &root_execution)?;
        }
        let event = WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: run.as_ref().map(|run| run.run_id),
            revision: item.revision,
            work: item.clone(),
            run,
            root_execution: Some(root_execution),
            claim,
            handoff_offer: None,
            blocker: None,
            transition: WorkTransition::Disposed {
                lifecycle: item.lifecycle,
                replacement_id: item.superseded_by,
                reason,
            },
            actor: request.actor.clone(),
            created_at: request.disposed_at,
        };
        append_work_event(&transaction, &event)?;
        persist_operation_result(
            &transaction,
            "dispose_work",
            &request.idempotency_key,
            request_object.hash(),
            &item,
        )?;
        transaction.commit()?;
        Ok(item)
    }

    /// Accounts for one deliberately cancelled or superseded required child
    /// with an attributed, audited reason from the project-bound session.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the parent revision changed, the child is
    /// not a directly required disposed child, the asserted project binding
    /// is invalid, or the waiver conflicts with an earlier request.
    pub fn waive_required_child<R: Redactor>(
        &mut self,
        request: &WaiveRequiredChildRequest,
        redactor: &R,
    ) -> Result<RequiredChildWaiver, StoreError> {
        inspect_work_request(redactor, request, &request.actor)?;
        let reason = normalize_text(&request.reason, "required-child waiver reason")?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(waiver) = replay_operation::<RequiredChildWaiver>(
            &transaction,
            "waive_required_child",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(waiver);
        }
        let parent = load_work_item(&transaction, request.parent_id)?;
        assert_revision(&parent, request.expected_parent_revision)?;
        if parent.lifecycle != WorkLifecycle::Open {
            return Err(StoreError::WorkNotOpen(parent.work_id));
        }
        let child = load_work_item(&transaction, request.child_id)?;
        if child.parent_id != Some(parent.work_id)
            || child.child_requirement != ChildRequirement::Required
            || !matches!(
                child.lifecycle,
                WorkLifecycle::Cancelled | WorkLifecycle::Superseded
            )
        {
            return Err(StoreError::InvalidWork(
                "completion waiver requires a directly required cancelled or superseded child"
                    .into(),
            ));
        }
        let mut root_execution = active_root_execution(&transaction, parent.root_id)?;
        if root_execution
            .required_child_waivers
            .iter()
            .any(|waiver| waiver.work_id == child.work_id)
        {
            return Err(StoreError::InvalidWork(
                "required child already has a completion waiver in this root execution".into(),
            ));
        }
        let waiver = RequiredChildWaiver {
            work_id: child.work_id,
            work_revision: child.revision,
            waived_by: request.actor.actor_id.clone(),
            reason: reason.clone(),
        };
        root_execution.required_child_waivers.push(waiver.clone());
        root_execution
            .required_child_waivers
            .sort_by(|left, right| left.work_id.0.as_bytes().cmp(right.work_id.0.as_bytes()));
        root_execution.revision += 1;
        root_execution.updated_at = request.waived_at;
        persist_root_execution(&transaction, &root_execution)?;
        let parent_run = parent
            .active_run_id
            .map(|run_id| load_work_run(&transaction, run_id))
            .transpose()?;
        let event = WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: parent.project_id.clone(),
            root_id: parent.root_id,
            work_id: parent.work_id,
            run_id: parent.active_run_id,
            revision: parent.revision,
            work: parent,
            run: parent_run,
            root_execution: Some(root_execution),
            claim: None,
            handoff_offer: None,
            blocker: None,
            transition: WorkTransition::RequiredChildWaived {
                child_id: child.work_id,
                child_revision: child.revision,
                reason,
            },
            actor: request.actor.clone(),
            created_at: request.waived_at,
        };
        append_work_event(&transaction, &event)?;
        persist_operation_result(
            &transaction,
            "waive_required_child",
            &request.idempotency_key,
            request_object.hash(),
            &waiver,
        )?;
        transaction.commit()?;
        Ok(waiver)
    }
}
