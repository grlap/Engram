use super::*;

mod plan;
mod replay;

impl LocalWorkService {
    /// Creates a root, decomposes focused work, or atomically admits a new plan.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when project binding or lifecycle admission is
    /// invalid, or the underlying transaction refuses the request.
    pub fn work_propose(
        &self,
        input: WorkProposeInput,
        now: DateTime<Utc>,
    ) -> Result<WorkProposeResult, StoreError> {
        self.work_propose_on(None, input, now)
    }

    /// Like [`Self::work_propose`], but first binds `work_ref` as the ambient
    /// focus and the decomposition target inside the same call. A complete new
    /// plan refuses `work_ref` and preserves the existing focus.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] under the same conditions as [`Self::work_propose`],
    /// or when `work_ref` does not resolve inside the project.
    #[allow(
        clippy::too_many_lines,
        reason = "root and decomposition translations remain together so the six-operation boundary is auditable"
    )]
    pub fn work_propose_on(
        &self,
        work_ref: Option<&str>,
        input: WorkProposeInput,
        now: DateTime<Utc>,
    ) -> Result<WorkProposeResult, StoreError> {
        // The one place a plan is dispatched. A plan binds no ambient focus, so
        // it leaves before the target binding below; everything after this
        // match handles only a root or a decomposition.
        let (protocol_operation, core_operation, raw_key) = match &input {
            WorkProposeInput::Plan { plan } => return self.work_propose_plan(work_ref, plan, now),
            WorkProposeInput::Root {
                idempotency_key, ..
            } => ("work_propose:root", "create_work", idempotency_key.as_str()),
            WorkProposeInput::Decompose {
                idempotency_key, ..
            } => (
                crate::storage::DECOMPOSE_PROTOCOL_OPERATION,
                "decompose_work",
                idempotency_key.as_str(),
            ),
        };
        let mut store = self.store_at(now)?;
        let target = self.bind_target(&mut store, work_ref, now)?;
        let basis = self.protocol_basis(
            &store,
            matches!(input, WorkProposeInput::Decompose { .. }),
            false,
            target,
            now,
        )?;
        let intent = self.protocol_intent(&input);
        let auto_decomposition = protocol_operation == crate::storage::DECOMPOSE_PROTOCOL_OPERATION
            && raw_key.trim().is_empty();
        let raw_key =
            self.effective_idempotency_key(raw_key, protocol_operation, &basis, &intent, now)?;
        let attempt = store
            .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
                project_id: &self.project_id,
                session_id: &self.session_id,
                operation: protocol_operation,
                idempotency_key: &raw_key,
                intent: &intent,
                basis: &basis,
                now,
            })
            .map_err(|error| {
                if auto_decomposition
                    && matches!(error, StoreError::WorkOperationIdempotencyConflict { .. })
                {
                    replay::retry_conflict(
                        &basis,
                        "the original attempt has no replayable retained basis",
                    )
                } else {
                    error
                }
            })?;
        let scoped_key = self.core_operation_key(protocol_operation, &raw_key, core_operation)?;
        let core_result = if auto_decomposition || attempt.result.is_none() {
            store.work_operation_result_value(core_operation, &scoped_key)?
        } else {
            None
        };
        if auto_decomposition {
            let stored = attempt.basis.as_ref().ok_or_else(|| {
                StoreError::InvalidWorkProjection(
                    "decomposition attempt has no retained basis".into(),
                )
            })?;
            replay::guard_decomposition_retry(stored, &basis, core_result.as_ref())?;
            if !attempt.basis_matches && attempt.result.is_none() && core_result.is_none() {
                // The basis hash and bytes are the pending attempt's CAS
                // revision. Decomposition still checks the live parent
                // revision and authority inside its mutation transaction.
                self.refresh_decomposition_retry_basis(&mut store, &raw_key, stored, &basis)?;
            }
        }
        if let Some(result) = attempt.result {
            let mut replay: WorkProposeResult = serde_json::from_value(result)?;
            // A root's receipt carries its focus page. Once the run its
            // obligations belong to has finished, that page is history. The
            // receipt proves the item was created, so a page whose history
            // cannot be read is returned as recorded rather than refused.
            // The focus names the run its page was built on, which decides a
            // page whose rows were all trimmed away; its rows decide otherwise.
            if let WorkProposeResult::Root { focus, .. } = &mut replay {
                let run_hint = focus.run.as_ref().map(|run| run.run_id);
                let _advisory = super::projection::replayed_obligation_page(
                    &store,
                    run_hint,
                    &mut focus.obligation_page,
                );
            }
            fit_replayed_root(&mut replay)?;
            ensure_agent_response_budget(&replay, "work_propose")?;
            return Ok(replay);
        }
        let basis_matches = auto_decomposition
            || retry_stable_basis_matches(attempt.basis_matches, attempt.basis.as_ref(), &basis)?;
        ensure_protocol_basis(
            basis_matches,
            protocol_operation,
            &raw_key,
            core_result.is_some(),
        )?;
        if matches!(&input, WorkProposeInput::Decompose { .. })
            && core_result.is_none()
            && basis
                .focused_work
                .as_ref()
                .is_some_and(|work| work.lifecycle != WorkLifecycle::Open)
        {
            let parent = basis.focused_work.as_ref().ok_or_else(|| {
                StoreError::InvalidWorkProjection("decomposition has no parent".into())
            })?;
            return Err(StoreError::WorkParentNotOpen {
                parent: parent.work_id,
                lifecycle: parent.lifecycle,
            });
        }
        let result = match input {
            // Dispatched by the opening match, so this cannot happen; should a
            // later edit break that, refuse rather than panic the server.
            WorkProposeInput::Plan { .. } => {
                return Err(StoreError::InvalidWorkProjection(
                    "a plan reached root or decomposition translation; plans are dispatched before ambient binding".into(),
                ));
            }
            WorkProposeInput::Root {
                external_ref,
                notes,
                title,
                outcome,
                acceptance,
                acceptance_bindings,
                work_kind,
                priority,
                labels,
                assigned_to,
                deferred_until,
                evaluation_mode,
                idempotency_key: _,
            } => {
                if let Some(value) = core_result {
                    let work: WorkItem = serde_json::from_value(value)?;
                    store.focus_work_session(
                        &self.project_id,
                        &self.session_id,
                        work.work_id,
                        now,
                    )?;
                    let focus = self.focus_view(&store, work.work_id, true, false, now)?;
                    let result = WorkProposeResult::Root {
                        work: work_item_summary(&work),
                        focus: Box::new(focus),
                    };
                    ensure_agent_response_budget(&result, "work_propose")?;
                    store.finish_work_protocol_attempt(
                        &self.project_id,
                        &self.session_id,
                        protocol_operation,
                        &raw_key,
                        &result,
                    )?;
                    return Ok(result);
                }
                let work = store.create_work(
                    &CreateWorkRequest {
                        acceptance_bindings,
                        external_ref,
                        notes,
                        project_id: self.project_id.clone(),
                        parent_id: None,
                        child_requirement: ChildRequirement::Required,
                        title,
                        outcome,
                        acceptance,
                        kind: work_kind.unwrap_or(WorkItemKind::Task),
                        priority: priority.unwrap_or(1),
                        labels,
                        assigned_to,
                        deferred_until,
                        evaluation_mode,
                        origin: WorkOrigin::Local,
                        source_snapshot_id: None,
                        actor: self.actor("work_propose", "create local root work"),
                        idempotency_key: scoped_key,
                        created_at: now,
                    },
                    &DevelopmentNoopRedactor,
                )?;
                store.focus_work_session(&self.project_id, &self.session_id, work.work_id, now)?;
                let focus = self.focus_view(&store, work.work_id, true, false, now)?;
                WorkProposeResult::Root {
                    work: work_item_summary(&work),
                    focus: Box::new(focus),
                }
            }
            WorkProposeInput::Decompose {
                children,
                prerequisites,
                idempotency_key: _,
            } => {
                if let Some(value) = core_result {
                    let decomposition: WorkDecomposition = serde_json::from_value(value)?;
                    WorkProposeResult::Decomposition(work_decomposition_summary(&decomposition))
                } else {
                    let parent = basis.focused_work.clone().ok_or_else(|| {
                        StoreError::InvalidWorkProjection(
                            "decomposition attempt has no bound focused work".into(),
                        )
                    })?;
                    let local_keys = children
                        .iter()
                        .map(|child| child.key.trim().to_owned())
                        .collect::<Vec<_>>();
                    let children = children
                        .into_iter()
                        .map(|child| ChildWorkDraft {
                            acceptance_bindings: child.acceptance_bindings,
                            external_ref: child.external_ref,
                            notes: child.notes,
                            local_key: child.key,
                            child_requirement: child
                                .requirement
                                .unwrap_or(ChildRequirement::Required),
                            title: child.title,
                            outcome: child.outcome,
                            acceptance: child.acceptance,
                            kind: child.kind.unwrap_or(WorkItemKind::Task),
                            priority: child.priority.unwrap_or(parent.priority),
                            labels: child.labels,
                            assigned_to: child.assigned_to,
                            deferred_until: child.deferred_until,
                            evaluation_mode: child.evaluation_mode,
                        })
                        .collect();
                    let mut resolved = Vec::with_capacity(prerequisites.len());
                    for edge in prerequisites {
                        let prerequisite =
                            if local_keys.iter().any(|key| key == edge.prerequisite.trim()) {
                                WorkDependencyRef::Proposed(edge.prerequisite)
                            } else {
                                WorkDependencyRef::Existing(
                                    store
                                        .resolve_work_ref(&self.project_id, &edge.prerequisite)?
                                        .work_id,
                                )
                            };
                        resolved.push(ChildWorkPrerequisite {
                            work_key: edge.work_key,
                            prerequisite,
                        });
                    }
                    let authority = self.planning_authority(basis.claim.as_ref(), &parent, now);
                    let decomposition = store.decompose_work(
                        &DecomposeWorkRequest {
                            parent_id: parent.work_id,
                            expected_parent_revision: parent.revision,
                            children,
                            prerequisites: resolved,
                            authority,
                            actor: self
                                .actor("work_propose", "atomically decompose ambient local work"),
                            idempotency_key: scoped_key,
                            created_at: now,
                        },
                        &DevelopmentNoopRedactor,
                    )?;
                    WorkProposeResult::Decomposition(work_decomposition_summary(&decomposition))
                }
            }
        };
        ensure_agent_response_budget(&result, "work_propose")?;
        store.finish_work_protocol_attempt(
            &self.project_id,
            &self.session_id,
            protocol_operation,
            &raw_key,
            &result,
        )?;
        Ok(result)
    }
}

#[cfg(test)]
mod tests;

/// Fits a replayed root's response within the agent budget after its page
/// was marked as history, which adds a few bytes. Recoverable focus context
/// is shed, as any focus is fitted, so the history mark stays: restoring the
/// recorded page would offer a finished run's obligations as owed again.
pub(super) fn fit_replayed_root(replay: &mut WorkProposeResult) -> Result<(), StoreError> {
    let overflow = serde_json::to_vec(&*replay)?
        .len()
        .saturating_sub(super::MAX_AGENT_WORK_RESPONSE_BYTES);
    if overflow > 0
        && let WorkProposeResult::Root { focus, .. } = replay
    {
        let focus_bytes = serde_json::to_vec(&**focus)?.len();
        let reserved = super::MAX_AGENT_WORK_RESPONSE_BYTES
            .saturating_sub(focus_bytes.saturating_sub(overflow));
        super::projection::fit_focus_response_reserving(focus, reserved)?;
    }
    Ok(())
}
