use super::*;

impl LocalWorkService {
    /// Creates a root or atomically decomposes ambient focused work.
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
    /// focus and the decomposition target inside the same call.
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
        let (protocol_operation, core_operation, raw_key) = propose_metadata(&input);
        let raw_key =
            self.effective_idempotency_key(raw_key, protocol_operation, &basis, &intent, now)?;
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
            let replay: WorkProposeResult = serde_json::from_value(result)?;
            ensure_agent_response_budget(&replay, "work_propose")?;
            return Ok(replay);
        }
        let basis_matches =
            retry_stable_basis_matches(attempt.basis_matches, attempt.basis.as_ref(), &basis)?;
        let scoped_key = self.core_operation_key(protocol_operation, &raw_key, core_operation)?;
        let core_result = store.work_operation_result_value(core_operation, &scoped_key)?;
        ensure_protocol_basis(
            basis_matches,
            protocol_operation,
            &raw_key,
            core_result.is_some(),
        )?;
        let result = match input {
            WorkProposeInput::Root {
                title,
                outcome,
                acceptance,
                work_kind,
                priority,
                labels,
                assigned_to,
                deferred_until,
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
