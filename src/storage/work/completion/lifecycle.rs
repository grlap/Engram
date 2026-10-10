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

use super::child_barriers::{ancestors_admit_execution, blocking_ancestor_on};
use crate::{RejectRequiredChildReceipt, RejectRequiredChildRequest};

impl SqliteStore {
    /// Cancels a required child and waives its parent's completion barrier atomically.
    /// Both events retain their ordinary authority checks and share the reason.
    ///
    /// # Errors
    /// Returns a typed rejection refusal for unsupported shapes, or the existing
    /// revision/authority/integrity error; neither transition commits on failure.
    pub fn reject_required_child<R: Redactor>(
        &mut self,
        request: &RejectRequiredChildRequest,
        redactor: &R,
    ) -> Result<RejectRequiredChildReceipt, StoreError> {
        inspect_work_request(redactor, request, &request.actor)?;
        let reason = normalize_text(
            &request.reason,
            crate::storage::refusal_labels::REQUIRED_CHILD_REJECTION_REASON,
        )?;
        let frozen = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(receipt) = replay_operation::<RejectRequiredChildReceipt>(
            &transaction,
            "reject_required_child",
            &request.idempotency_key,
            frozen.key(),
        )? {
            transaction.commit()?;
            return Ok(receipt);
        }
        let child = load_work_item(&transaction, request.work_id)?;
        assert_revision(&child, request.expected_work_revision)?;
        let parent = child
            .parent_id
            .map(|id| load_work_item(&transaction, id))
            .transpose()?;
        let refuse = |reason| reject_refusal(&child, parent.as_ref(), reason);
        if child.lifecycle != WorkLifecycle::Open {
            return Err(refuse("the child is not open"));
        }
        let Some(parent) = parent.as_ref() else {
            return Err(refuse("the item has no parent"));
        };
        if child.child_requirement != ChildRequirement::Required {
            return Err(refuse(
                "the child is optional and has no required-child barrier to waive",
            ));
        }
        if parent.lifecycle != WorkLifecycle::Open {
            return Err(refuse("the parent is not open"));
        }
        let expected_parent_revision = request.expected_parent_revision.ok_or_else(|| {
            StoreError::InvalidWork("rejection requires an expected parent revision".into())
        })?;
        assert_revision(parent, expected_parent_revision)?;
        let blocking_ancestor = blocking_ancestor_on(&transaction, parent)?;
        if let Some(root) = super::active_root_execution_optional(&transaction, parent.root_id)? {
            if root
                .required_child_waivers
                .iter()
                .any(|waiver| waiver.work_id == child.work_id)
            {
                return Err(refuse(
                    "the child already has a waiver in this root execution",
                ));
            }
        } else if let Some(run_id) = child.active_run_id {
            // A native child can remain open beneath an optional branch after
            // root completion. Validate its execution before classifying that
            // legitimate refusal; runless restored children still bootstrap.
            let run = load_work_run(&transaction, run_id)?;
            let root = load_root_execution(&transaction, run.root_execution_id)?;
            if root.state != RootExecutionState::Active {
                return Err(reject_refusal_with_ancestor(
                    &child,
                    Some(parent),
                    "the root execution is closed and cannot record a child waiver",
                    blocking_ancestor.as_ref(),
                ));
            }
            return Err(StoreError::InvalidWorkProjection(
                "child execution is active but absent from active root selection".into(),
            ));
        }
        if blocking_ancestor.is_some() {
            return Err(reject_refusal_with_ancestor(
                &child,
                Some(parent),
                "an ancestor is not open",
                blocking_ancestor.as_ref(),
            ));
        }
        let cancel = DisposeWorkRequest {
            work_id: child.work_id,
            expected_work_revision: request.expected_work_revision,
            disposition: WorkDisposition::Cancelled,
            replacement_id: None,
            reason: reason.clone(),
            actor: request.actor.clone(),
            idempotency_key: format!("{}:cancel", request.idempotency_key),
            disposed_at: request.rejected_at,
        };
        let cancelled = dispose_work_on(
            &transaction,
            &cancel,
            reason.clone(),
            &request_object(&cancel)?,
        )?
        .item;
        let waive = WaiveRequiredChildRequest {
            parent_id: parent.work_id,
            child_id: child.work_id,
            expected_parent_revision,
            reason: reason.clone(),
            actor: request.actor.clone(),
            idempotency_key: format!("{}:waive", request.idempotency_key),
            waived_at: request.rejected_at,
        };
        let waiver =
            waive_required_child_on(&transaction, &waive, reason, &request_object(&waive)?)?;
        let receipt = RejectRequiredChildReceipt {
            child: cancelled,
            parent_ref: parent.short_ref.clone(),
            waiver,
        };
        persist_operation_result(
            &transaction,
            "reject_required_child",
            &request.idempotency_key,
            frozen.key(),
            &receipt,
        )?;
        transaction.commit()?;
        journal_disposed(&request.actor, request.work_id);
        Ok(receipt)
    }

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
        let reason = normalize_text(
            &request.reason,
            crate::storage::refusal_labels::REOPEN_REASON,
        )?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(run) = replay_operation::<WorkRun>(
            &transaction,
            "reopen_work",
            &request.idempotency_key,
            request_object.key(),
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
                request_object.key(),
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
            super::super::root_state::initialize(&transaction, &execution)?;
            execution
        } else {
            let mut execution = active_root_execution(&transaction, item.root_id)?;
            if item.child_requirement == ChildRequirement::Required {
                let old_seal: Option<String> = transaction
                    .query_row(
                        "SELECT seal_id FROM work_completion_seals WHERE work_id = ?1
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
            root_execution
                .run_ids
                .sort_by(super::super::root_state::compare_runs);
            root_execution.revision += 1;
            root_execution.updated_at = request.reopened_at;
            persist_root_execution(&transaction, &root_execution)?;
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
            request_object.key(),
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
        let reason = normalize_text(
            &request.reason,
            crate::storage::refusal_labels::WORK_DISPOSAL_REASON,
        )?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        let disposal = dispose_work_on(&transaction, request, reason, &request_object)?;
        transaction.commit()?;
        // Only a fresh disposal ends a claim; a replay ended nothing now.
        if disposal.fresh {
            journal_disposed(&request.actor, request.work_id);
        }
        Ok(disposal.item)
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
        let reason = normalize_text(
            &request.reason,
            crate::storage::refusal_labels::REQUIRED_CHILD_WAIVER_REASON,
        )?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        let result = waive_required_child_on(&transaction, request, reason, &request_object)?;
        transaction.commit()?;
        Ok(result)
    }
}

/// A committed disposal: the disposed item is no longer open, so no claim on
/// it binds any more, whoever held it, and the acting session's word must not
/// disclose a binding it captured before.
fn journal_disposed(actor: &crate::domain::ActorContext, work: crate::domain::WorkId) {
    if let Some(session) = actor.session_id.as_ref() {
        super::super::focus_journal::record_ended(session, work);
    }
}

fn reject_refusal(child: &WorkItem, parent: Option<&WorkItem>, reason: &'static str) -> StoreError {
    let child_ref = child.short_ref.clone();
    let parent_ref = parent.map(|item| item.short_ref.clone());
    let waiver = parent_ref.as_ref().map_or_else(String::new,
        |parent| format!("; if still required and {parent} is open and waivable, use engram work update {parent} --waive {child_ref} --reason \"why\""),
    );
    StoreError::WorkRejectRefused {
        remedy: format!(
            "inspect with engram work show {child_ref}; if cancellation is admitted, use engram work update {child_ref} --cancel \"why\"{waiver}"
        ).into_boxed_str(),
        child_ref: child_ref.into_boxed_str(),
        parent_ref,
        blocking_ancestor_ref: None,
        reason,
    }
}

fn reject_refusal_with_ancestor(
    child: &WorkItem,
    parent: Option<&WorkItem>,
    reason: &'static str,
    ancestor: Option<&crate::domain::WorkBlockingAncestor>,
) -> StoreError {
    let Some(ancestor) = ancestor else {
        return reject_refusal(child, parent, reason);
    };
    StoreError::WorkRejectRefused {
        child_ref: child.short_ref.clone().into_boxed_str(),
        parent_ref: parent.map(|item| item.short_ref.clone()),
        blocking_ancestor_ref: Some(ancestor.short_ref.clone().into_boxed_str()),
        reason,
        remedy: format!(
            "execution blocked by ancestor {} ({}); inspect with engram work show {}; then inspect with engram work show {} and follow its admitted detach or resolve-first guidance, or file an independent root with engram work add \"Follow-up title\" --accept \"Delivery criterion\"",
            ancestor.short_ref, ancestor.lifecycle.word(), ancestor.short_ref, child.short_ref,
        ).into_boxed_str(),
    }
}

/// A disposal's item, and whether this call disposed of it rather than
/// replaying an earlier disposal.
struct Disposal {
    item: WorkItem,
    fresh: bool,
}

fn dispose_work_on(
    transaction: &rusqlite::Transaction<'_>,
    request: &DisposeWorkRequest,
    reason: String,
    request_object: &crate::CanonicalObject,
) -> Result<Disposal, StoreError> {
    if let Some(item) = replay_operation::<WorkItem>(
        transaction,
        "dispose_work",
        &request.idempotency_key,
        request_object.key(),
    )? {
        return Ok(Disposal { item, fresh: false });
    }
    let mut item = load_work_item(transaction, request.work_id)?;
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
                    crate::storage::refusal_labels::SELF_SUPERSEDE.into(),
                ));
            }
            let replacement = load_work_item(transaction, replacement_id)?;
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
                transaction,
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
            transaction,
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
            .map(|run_id| load_work_run(transaction, run_id))
            .transpose()?
    };
    let mut claim = if restored_execution.is_some() {
        None
    } else if let Some(run) = run.as_ref() {
        expire_handoff_offers(transaction, run.run_id, request.disposed_at, &request.actor)?;
        load_work_claim_optional(transaction, run.run_id)?
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
    if let Some(run) = run.as_ref() {
        let pending: bool = transaction.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_handoff_offers WHERE run_id = ?1 AND state = 'offered')",
            [run.run_id.0.to_string()],
            |row| row.get(0),
        )?;
        if pending {
            return Err(StoreError::InvalidWork(
                crate::storage::PENDING_HANDOFF_REFUSAL.into(),
            ));
        }
    }
    let claim_fence = if let Some(current) = claim.as_mut() {
        if current.state == WorkClaimState::Active {
            current.state = WorkClaimState::Released;
            current.revision += 1;
            current.fence += 1;
            current.expires_at = request.disposed_at;
            persist_claim(transaction, current)?;
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
        persist_work_run(transaction, current_run, claim_fence)?;
    }
    item.lifecycle = match request.disposition {
        WorkDisposition::Cancelled => WorkLifecycle::Cancelled,
        WorkDisposition::Superseded => WorkLifecycle::Superseded,
    };
    item.superseded_by = replacement.as_ref().map(|work| work.work_id);
    item.active_run_id = None;
    item.revision += 1;
    item.updated_at = request.disposed_at;
    persist_work_item(transaction, &item)?;
    let mut root_execution = if let Some((execution, _, _)) = restored_execution {
        execution
    } else if let Some(current_run) = run.as_ref() {
        load_root_execution(transaction, current_run.root_execution_id)?
    } else {
        active_root_execution(transaction, item.root_id)?
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
        persist_root_execution(transaction, &root_execution)?;
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
    append_work_event(transaction, &event)?;
    persist_operation_result(
        transaction,
        "dispose_work",
        &request.idempotency_key,
        request_object.key(),
        &item,
    )?;
    Ok(Disposal { item, fresh: true })
}

fn waive_required_child_on(
    transaction: &rusqlite::Transaction<'_>,
    request: &WaiveRequiredChildRequest,
    reason: String,
    request_object: &crate::CanonicalObject,
) -> Result<RequiredChildWaiver, StoreError> {
    if let Some(waiver) = replay_operation::<RequiredChildWaiver>(
        transaction,
        "waive_required_child",
        &request.idempotency_key,
        request_object.key(),
    )? {
        return Ok(waiver);
    }
    #[cfg(test)]
    super::super::cost::phase("waive.before_admission");
    let parent = load_work_item(transaction, request.parent_id)?;
    assert_revision(&parent, request.expected_parent_revision)?;
    if parent.lifecycle != WorkLifecycle::Open {
        return Err(StoreError::WorkNotOpen(parent.work_id));
    }
    let child = load_work_item(transaction, request.child_id)?;
    if child.parent_id != Some(parent.work_id)
        || child.child_requirement != ChildRequirement::Required
        || !matches!(
            child.lifecycle,
            WorkLifecycle::Cancelled | WorkLifecycle::Superseded
        )
    {
        return Err(StoreError::InvalidWork(
            crate::storage::refusal_labels::WAIVER_NEEDS_DISPOSED_REQUIRED_CHILD.into(),
        ));
    }
    if !ancestors_admit_execution(transaction, &parent)? {
        return Err(StoreError::InvalidWork(format!(
            "Cannot waive {} from {} because an ancestor is not open. Run engram work show {} and follow its admitted detach or resolve-first guidance. For work beneath a completed, cancelled, or superseded ancestor, continue through an admitted detach or file an independent root follow-up.",
            child.short_ref, parent.short_ref, parent.short_ref,
        )));
    }
    let execution_id: Option<String> = transaction.query_row(
        "SELECT root_execution_id FROM work_root_executions WHERE root_id = ?1 AND state = 'active'",
        [parent.root_id.0.to_string()], |row| row.get(0),
    ).optional()?;
    let execution_id = execution_id.ok_or_else(|| {
        StoreError::InvalidWorkProjection(format!(
            "root work {:?} has no active execution",
            parent.root_id
        ))
    })?;
    let waiver = RequiredChildWaiver {
        work_id: child.work_id,
        work_revision: child.revision,
        waived_by: request.actor.actor_id.clone(),
        reason: reason.clone(),
    };
    let root_execution = super::super::root_state::try_update(
        transaction,
        super::super::query::parse_root_execution_id(&execution_id)?,
        |root| {
            #[cfg(test)]
            super::super::cost::phase("waive.after_root_read");
            if root.root_id != parent.root_id || root.state != RootExecutionState::Active {
                return Err(StoreError::InvalidWorkProjection(
                    "active root execution differs from its root binding".into(),
                ));
            }
            if root
                .required_child_waivers
                .iter()
                .any(|waiver| waiver.work_id == child.work_id)
            {
                return Err(StoreError::InvalidWork(
                    "required child already has a completion waiver in this root execution".into(),
                ));
            }
            root.required_child_waivers.push(waiver.clone());
            root.required_child_waivers
                .sort_by(super::super::root_state::compare_child_waivers);
            root.revision += 1;
            root.updated_at = request.waived_at;
            Ok(())
        },
    )?;
    #[cfg(test)]
    super::super::cost::phase("waive.after_persist");
    let parent_run = parent
        .active_run_id
        .map(|run_id| load_work_run(transaction, run_id))
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
        root_execution: Some(root_execution.value().clone()),
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
    super::super::feeds::append_work_event_with_root(transaction, &event, &root_execution)?;
    #[cfg(test)]
    super::super::cost::phase("waive.after_event");
    persist_operation_result(
        transaction,
        "waive_required_child",
        &request.idempotency_key,
        request_object.key(),
        &waiver,
    )?;
    Ok(waiver)
}
