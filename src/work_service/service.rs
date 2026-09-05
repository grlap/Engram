use super::{
    ACTOR_CONTEXT_NORMALIZED_REFERENCE, ACTOR_CONTEXT_PROVENANCE_REFERENCE, ActorContext,
    AllowedNextContext, AssuranceLevel, CanonicalObject, DateTime, LocalWorkService,
    MAX_FOCUS_HISTORY, MAX_FOCUS_MEMORIES, MAX_FOCUS_RELATIONS, Mutex, MutexGuard, OnceLock,
    POST_COMPLETION_EVIDENCE_PROVENANCE_REFERENCE, POST_COMPLETION_EVIDENCE_PROVENANCE_SOURCE,
    PROCESS_DEFAULT_WORK_SESSION_NAMESPACE, PathBuf, ProjectId, ProvenanceLink, ProvenanceRelation,
    RestoredHistoryEntry, RestoredHistoryView, Serialize, SessionId, SqliteStore, StoreError, Utc,
    WorkActorDefaultSource, WorkAttributionDefaults, WorkBlockerSummary, WorkChange,
    WorkChangeProjection, WorkClaim, WorkClaimState, WorkCoreOperationKey, WorkDerivedKey,
    WorkFocusView, WorkGraphSnapshotDestinationKind, WorkGraphSnapshotExport,
    WorkGraphSnapshotLoadResult, WorkGuidance, WorkHistoryView, WorkId, WorkItem, WorkNextSection,
    WorkObligationState, WorkPlanningAuthority, WorkProtocolBasis, WorkProtocolIntent,
    WorkSectionOmission, WorkSectionOmissionReason, agent_work_event_summary, agent_work_session,
    allowed_next, bounded_prerequisite_summaries, child_lifecycle_is_unfinished,
    child_lifecycle_priority, compact_text, count_omission, ensure_agent_response_budget,
    fit_focus_response, normalize_actor_context, owned_control_work_binding,
    prioritized_focus_evidence, ready_work_summary, required_child_waiver_candidate,
    restored_work_evidence_summary, validate_process_default_work_session, work_evidence_kind_word,
    work_evidence_summary, work_handoff_summary, work_item_summary, work_lifecycle_word,
    work_memory_index, work_obligation_page_from_records, work_observation_summary,
    work_run_summary,
};

impl LocalWorkService {
    /// Validates and optionally recreates a saved planning/history graph.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the file or destination violates the graph
    /// snapshot contract.
    pub fn load_work_graph_snapshot(
        &self,
        bytes: &[u8],
        dry_run: bool,
        loaded_at: DateTime<Utc>,
    ) -> Result<WorkGraphSnapshotLoadResult, StoreError> {
        let mut store = self.lock_store_at(loaded_at)?;
        store.load_work_graph_snapshot(
            &self.project_id,
            &self.actor("graph:load", "recreate the project work graph"),
            bytes,
            dry_run,
            loaded_at,
            &crate::DevelopmentNoopRedactor,
        )
    }

    /// Saves one deterministic planning/history snapshot and returns only
    /// after its source-store disclosure audit has committed.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the store, canonical snapshot, redactor, or
    /// audit transaction is invalid.
    pub fn save_work_graph_snapshot(
        &self,
        widening_reason: Option<&str>,
        destination_kind: WorkGraphSnapshotDestinationKind,
        exported_at: DateTime<Utc>,
    ) -> Result<WorkGraphSnapshotExport, StoreError> {
        let mut store = self.lock_store_at(exported_at)?;
        store.save_work_graph_snapshot(
            &self.project_id,
            &self.actor("graph:save", "save the project work graph"),
            widening_reason,
            destination_kind,
            exported_at,
            &crate::DevelopmentNoopRedactor,
        )
    }

    /// Constructs a project-bound local-work service.
    #[must_use]
    pub fn new(
        database: PathBuf,
        project_id: ProjectId,
        actor_id: String,
        session_id: SessionId,
        source_skill: Option<String>,
    ) -> Self {
        Self::new_with_attribution(
            database,
            project_id,
            actor_id,
            session_id,
            source_skill,
            None,
            WorkAttributionDefaults::default(),
        )
    }

    /// Constructs a project-bound service with optional host-asserted actor
    /// context and explicit local-attribution defaults.
    #[must_use]
    pub fn new_with_attribution(
        database: PathBuf,
        project_id: ProjectId,
        actor_id: String,
        session_id: SessionId,
        source_skill: Option<String>,
        actor_context: Option<String>,
        attribution_defaults: WorkAttributionDefaults,
    ) -> Self {
        let (actor_context, actor_context_normalized) = normalize_actor_context(actor_context);
        Self {
            database,
            project_id,
            actor_id,
            actor_context,
            actor_context_normalized,
            session_id,
            attribution_defaults,
            source_skill,
            cached_store: OnceLock::new(),
            process_default_session_initialized: OnceLock::new(),
            #[cfg(test)]
            delivery_stage_hook: None,
        }
    }
    pub(super) fn store_at(
        &self,
        now: DateTime<Utc>,
    ) -> Result<MutexGuard<'_, SqliteStore>, StoreError> {
        let mut store = self.lock_store_at(now)?;
        if self
            .session_id
            .0
            .starts_with(PROCESS_DEFAULT_WORK_SESSION_NAMESPACE)
            && self.process_default_session_initialized.get().is_none()
        {
            store.initialize_process_default_work_session(
                &self.project_id,
                &self.session_id,
                now,
            )?;
            let _ = self.process_default_session_initialized.set(());
        }
        Ok(store)
    }

    fn lock_store_at(&self, now: DateTime<Utc>) -> Result<MutexGuard<'_, SqliteStore>, StoreError> {
        if self.actor_id.trim().is_empty() || self.session_id.0.trim().is_empty() {
            return Err(StoreError::InvalidWork(
                "local work requires a non-empty asserted actor and session binding".into(),
            ));
        }
        validate_process_default_work_session(
            &self.session_id,
            self.attribution_defaults.session,
            now,
        )?;
        if self.cached_store.get().is_none() {
            let opened = SqliteStore::open_unresolved(&self.database)?;
            // A simultaneous first call may win initialization. Dropping this
            // redundant opener is safe; both opened the same canonical store.
            let _ = self.cached_store.set(Mutex::new(opened));
        }
        let cached = self.cached_store.get().ok_or_else(|| {
            StoreError::InvalidWorkProjection(
                "local work service could not initialize its SQLite connection".into(),
            )
        })?;
        let store = cached.lock().map_err(|_| {
            StoreError::InvalidWorkProjection(
                "local work service SQLite connection lock is poisoned".into(),
            )
        })?;
        Ok(store)
    }

    #[cfg(test)]
    pub(super) fn store(&self) -> Result<MutexGuard<'_, SqliteStore>, StoreError> {
        self.store_at(Utc::now())
    }

    pub(super) fn protocol_intent<'a, T>(&'a self, input: &'a T) -> WorkProtocolIntent<'a, T> {
        WorkProtocolIntent {
            project_id: &self.project_id,
            session_id: &self.session_id,
            actor_id: &self.actor_id,
            source_skill: self.source_skill.as_deref(),
            input,
        }
    }

    pub(super) fn protocol_basis(
        &self,
        store: &SqliteStore,
        bind_focus: bool,
        include_handoffs: bool,
        target: Option<WorkId>,
        now: DateTime<Utc>,
    ) -> Result<WorkProtocolBasis, StoreError> {
        if !bind_focus {
            return Ok(WorkProtocolBasis {
                focused_work: None,
                claim: None,
                handoffs: Vec::new(),
            });
        }
        let work = self.focused_item(store, target, now)?;
        Ok(WorkProtocolBasis {
            claim: store.current_work_claim(work.work_id)?,
            handoffs: if include_handoffs {
                store.work_handoff_offers(work.work_id)?
            } else {
                Vec::new()
            },
            focused_work: Some(work),
        })
    }

    pub(super) fn core_operation_key(
        &self,
        protocol_operation: &str,
        caller_key: &str,
        core_operation: &str,
    ) -> Result<String, StoreError> {
        let object = CanonicalObject::freeze(&WorkCoreOperationKey {
            project_id: &self.project_id,
            session_id: &self.session_id,
            protocol_operation,
            caller_key,
            core_operation,
        })?;
        Ok(format!("work:{}", object.hash().as_str()))
    }

    /// Uses the caller's key when one was supplied; otherwise derives one from
    /// the session, operation, focused work, and canonical intent, so an
    /// identical call replays and a different call is a new attempt.
    pub(super) fn effective_idempotency_key<T: Serialize>(
        &self,
        caller_key: &str,
        protocol_operation: &str,
        basis: &WorkProtocolBasis,
        intent: &WorkProtocolIntent<'_, T>,
        now: DateTime<Utc>,
    ) -> Result<String, StoreError> {
        let caller_key = caller_key.trim();
        if !caller_key.is_empty() {
            return Ok(caller_key.to_owned());
        }
        let intent = CanonicalObject::freeze(intent)?;
        let basis_object = CanonicalObject::freeze(&basis.retry_stable())?;
        let object = CanonicalObject::freeze(&WorkDerivedKey {
            project_id: &self.project_id,
            session_id: &self.session_id,
            protocol_operation,
            focused_work_id: basis.focused_work.as_ref().map(|work| work.work_id),
            basis: basis_object.hash(),
            claim_live: basis
                .claim
                .as_ref()
                .map(|claim| claim.state == WorkClaimState::Active && claim.expires_at > now),
            intent: intent.hash(),
        })?;
        Ok(format!("auto:{}", object.hash().as_str()))
    }

    /// Resolves an optional caller-supplied target, makes it the ambient
    /// focus on this connection, and returns its id so the mutation binds to
    /// it regardless of any concurrent focus change by the same session.
    pub(super) fn bind_target(
        &self,
        store: &mut SqliteStore,
        work_ref: Option<&str>,
        now: DateTime<Utc>,
    ) -> Result<Option<WorkId>, StoreError> {
        let Some(work_ref) = work_ref else {
            return Ok(None);
        };
        let work = store.resolve_work_ref(&self.project_id, work_ref)?;
        store.focus_work_session(&self.project_id, &self.session_id, work.work_id, now)?;
        Ok(Some(work.work_id))
    }

    pub(super) fn actor(&self, tool_name: &str, reason: &str) -> ActorContext {
        let mut provenance_chain = vec![ProvenanceLink {
            relation: ProvenanceRelation::AssertedBy,
            source: self.actor_id.clone(),
            reference: Some(self.session_id.0.clone()),
        }];
        if let Some(source) = self.attribution_defaults.actor {
            provenance_chain.push(ProvenanceLink {
                relation: ProvenanceRelation::DerivedFrom,
                source: match source {
                    WorkActorDefaultSource::OsUserEnvironment => "defaulted:os_user_environment",
                    WorkActorDefaultSource::ProcessFallback => "defaulted:process_actor",
                }
                .into(),
                reference: Some("actor_id".into()),
            });
        }
        if self.attribution_defaults.session {
            provenance_chain.push(ProvenanceLink {
                relation: ProvenanceRelation::DerivedFrom,
                source: "defaulted:process_session".into(),
                reference: Some("session_id".into()),
            });
        }
        if let Some(actor_context) = &self.actor_context {
            provenance_chain.push(ProvenanceLink {
                relation: ProvenanceRelation::DerivedFrom,
                source: actor_context.clone(),
                reference: Some(ACTOR_CONTEXT_PROVENANCE_REFERENCE.into()),
            });
        }
        if self.actor_context_normalized {
            provenance_chain.push(ProvenanceLink {
                relation: ProvenanceRelation::DerivedFrom,
                source: "actor_context:normalized".into(),
                reference: Some(ACTOR_CONTEXT_NORMALIZED_REFERENCE.into()),
            });
        }
        ActorContext {
            actor_id: self.actor_id.clone(),
            actor_kind: "agent".into(),
            assurance: AssuranceLevel::Asserted,
            run_id: None,
            session_id: Some(self.session_id.clone()),
            source_tool: Some(tool_name.into()),
            source_skill: self.source_skill.clone(),
            provenance_chain,
            reason: reason.into(),
        }
    }

    pub(super) fn post_completion_actor(&self, tool_name: &str, reason: &str) -> ActorContext {
        let mut actor = self.actor(tool_name, reason);
        actor.provenance_chain.push(ProvenanceLink {
            relation: ProvenanceRelation::DerivedFrom,
            source: POST_COMPLETION_EVIDENCE_PROVENANCE_SOURCE.into(),
            reference: Some(POST_COMPLETION_EVIDENCE_PROVENANCE_REFERENCE.into()),
        });
        actor
    }

    pub(super) fn non_holder_note_actor(&self) -> ActorContext {
        let mut actor = self.actor("work_update", "record a non-holder work observation");
        actor.provenance_chain.push(ProvenanceLink {
            relation: ProvenanceRelation::DerivedFrom,
            source: crate::domain::NON_HOLDER_NOTE_SOURCE.into(),
            reference: Some(crate::domain::NON_HOLDER_NOTE_REFERENCE.into()),
        });
        actor
    }

    pub(super) fn focused_item(
        &self,
        store: &SqliteStore,
        target: Option<WorkId>,
        now: DateTime<Utc>,
    ) -> Result<WorkItem, StoreError> {
        let focused = match target {
            Some(work_id) => Some(work_id),
            None => {
                store
                    .work_session_state(&self.project_id, &self.session_id, now)?
                    .focused_work_id
            }
        };
        focused
            .map(|work_id| store.get_work_item(work_id))
            .transpose()?
            .ok_or_else(|| {
                StoreError::InvalidWork(
                    "this session has no focused work; call work_focus first".into(),
                )
            })
    }

    pub(super) fn live_protocol_claim(
        &self,
        basis: &WorkProtocolBasis,
        work: &WorkItem,
        now: DateTime<Utc>,
    ) -> Result<WorkClaim, StoreError> {
        let claim = basis
            .claim
            .clone()
            .ok_or(StoreError::WorkClaimMismatch { work: work.work_id })?;
        if claim.work_id == work.work_id
            && claim.state == WorkClaimState::Active
            && claim.holder == self.session_id
            && claim.expires_at <= now
        {
            return Err(StoreError::WorkClaimLapsed {
                work: work.work_id,
                expired_at: claim.expires_at,
            });
        }
        if claim.work_id != work.work_id
            || claim.state != WorkClaimState::Active
            || claim.holder != self.session_id
        {
            return Err(StoreError::WorkClaimMismatch { work: work.work_id });
        }
        Ok(claim)
    }

    pub(super) fn planning_authority(
        &self,
        claim: Option<&WorkClaim>,
        work: &WorkItem,
        now: DateTime<Utc>,
    ) -> WorkPlanningAuthority {
        if let Some(claim) = claim
            && claim.work_id == work.work_id
            && claim.state == WorkClaimState::Active
            && claim.holder == self.session_id
            && claim.expires_at > now
        {
            return WorkPlanningAuthority::Claim {
                run_id: claim.run_id,
                holder: claim.holder.clone(),
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
            };
        }
        WorkPlanningAuthority::Project
    }

    #[allow(
        clippy::too_many_lines,
        reason = "the bounded focus packet is assembled in one place so every relation and omission limit is visible"
    )]
    pub(super) fn focus_view(
        &self,
        store: &SqliteStore,
        work_id: WorkId,
        with_memories: bool,
        with_latest_evidence: bool,
        now: DateTime<Utc>,
    ) -> Result<WorkFocusView, StoreError> {
        let session = store.work_session_state(&self.project_id, &self.session_id, now)?;
        let WorkGuidance {
            status,
            allowed_next,
            waivable_required_children,
            claim,
            handoffs,
        } = self.work_guidance(store, work_id, now)?;
        let run = if let Some(run_id) = status.work.active_run_id {
            Some(store.get_work_run(run_id)?)
        } else {
            store.latest_work_run(work_id)?
        };
        let completed_by_record = store.work_completed_by_restored_record(work_id)?;
        let obligation_records = run
            .as_ref()
            .map(|run| store.work_run_obligations(run.run_id))
            .transpose()?
            .unwrap_or_default();
        let obligation_page = work_obligation_page_from_records(obligation_records)?;
        let required_environments = obligation_page
            .items
            .iter()
            .filter(|obligation| obligation.state == WorkObligationState::Open)
            .filter_map(|obligation| obligation.requirement.required_environment.clone())
            .collect::<Vec<_>>();
        let mut evidence_count = run
            .as_ref()
            .map(|run| store.work_run_evidence_count(run.run_id))
            .transpose()?
            .unwrap_or_default();
        let evidence_candidates = run
            .as_ref()
            .map(|run| {
                store.work_run_evidence_projection(
                    run.run_id,
                    &required_environments,
                    MAX_FOCUS_RELATIONS,
                )
            })
            .transpose()?
            .unwrap_or_default();
        let evidence = prioritized_focus_evidence(evidence_candidates, &obligation_page);
        let native_evidence_items = run
            .as_ref()
            .map(|run| {
                evidence
                    .iter()
                    .map(|hash| work_evidence_summary(store, run.run_id, hash))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default();
        let native_latest_evidence_item = if with_latest_evidence {
            run.as_ref()
                .map(|run| {
                    store
                        .latest_work_run_evidence(run.run_id)?
                        .map(|hash| work_evidence_summary(store, run.run_id, &hash))
                        .transpose()
                })
                .transpose()?
                .flatten()
        } else {
            None
        };
        let restored_evidence = store.restored_work_evidence(work_id)?;
        evidence_count = evidence_count.saturating_add(restored_evidence.len());
        let mut evidence_items = Vec::with_capacity(
            restored_evidence
                .len()
                .saturating_add(native_evidence_items.len()),
        );
        for (hash, evidence) in restored_evidence {
            evidence_items.push(restored_work_evidence_summary(hash, &evidence)?);
        }
        let restored_latest_evidence_item = with_latest_evidence
            .then(|| evidence_items.last().cloned())
            .flatten();
        evidence_items.extend(native_evidence_items);
        if evidence_items.len() > MAX_FOCUS_RELATIONS {
            evidence_items.drain(..evidence_items.len() - MAX_FOCUS_RELATIONS);
        }
        let mut latest_evidence_item = if completed_by_record {
            restored_latest_evidence_item.or(native_latest_evidence_item)
        } else {
            native_latest_evidence_item.or(restored_latest_evidence_item)
        };
        let (observation_count, observations) =
            store.work_observation_tail(work_id, MAX_FOCUS_RELATIONS)?;
        evidence_count = evidence_count.saturating_add(observation_count);
        let mut latest_position = if with_latest_evidence && !observations.is_empty() {
            latest_evidence_item
                .as_ref()
                .map(|latest| {
                    store.work_root_object_position(status.work.root_id, &latest.evidence)
                })
                .transpose()?
        } else {
            None
        };
        let observation_slots = MAX_FOCUS_RELATIONS.saturating_sub(evidence_items.len());
        let omitted_observations = observations.len().saturating_sub(observation_slots);
        for (index, (hash, observation)) in observations.into_iter().enumerate() {
            let summary = work_observation_summary(hash, &observation);
            if with_latest_evidence {
                let position =
                    store.work_root_object_position(status.work.root_id, &summary.evidence)?;
                if latest_position.is_none_or(|latest| latest < position) {
                    latest_evidence_item = Some(summary.clone());
                    latest_position = Some(position);
                }
            }
            // Keep the existing native/restored evidence priority. Peer notes
            // fill spare slots; the latest note is also exposed separately.
            if index >= omitted_observations {
                evidence_items.push(summary);
            }
        }
        let history_total = store.work_event_count(work_id)?;
        let mut history = Vec::new();
        for entry in store.work_event_tail(work_id, MAX_FOCUS_HISTORY)? {
            let event = store
                .get::<crate::WorkEvent>(&entry.object_hash)?
                .ok_or_else(|| {
                    StoreError::InvalidWorkProjection(format!(
                        "root-work feed object {} is missing",
                        entry.object_hash
                    ))
                })?;
            if event.work_id != work_id {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "targeted history returned event for {} while loading {}",
                    event.work_id.0, work_id.0
                )));
            }
            history.push(WorkChange {
                from_current_session: event.actor.session_id.as_ref() == Some(&self.session_id),
                entry,
                delivery: WorkChangeProjection::Visible(agent_work_event_summary(&event)),
            });
        }
        let restored_history = restored_history_view(store.work_restored_records(work_id)?);
        let mut children = store.work_children(work_id)?;
        // Put unfinished children first inside the bounded relation prefix so
        // terminal history cannot hide work that still needs attention.
        // Stable sorting retains the store's stable id order within each
        // lifecycle group.
        children.sort_by_key(|child| child_lifecycle_priority(child.lifecycle));
        let child_count = children.len();
        let unfinished_child_count = children
            .iter()
            .take_while(|child| child_lifecycle_is_unfinished(child.lifecycle))
            .count();
        let visible_child_count = child_count.min(MAX_FOCUS_RELATIONS);
        let visible_unfinished_child_count = unfinished_child_count.min(visible_child_count);
        let terminal_child_count = child_count - unfinished_child_count;
        let visible_terminal_child_count = visible_child_count - visible_unfinished_child_count;
        let prerequisite_page =
            store.work_prerequisites_with_state(work_id, MAX_FOCUS_RELATIONS)?;
        // The work-memory index is bound to the session's persisted focus; an
        // inspection of another item carries no memory index.
        let memories = if with_memories {
            store.search_work_memories(
                &self.project_id,
                work_id,
                &self.session_id,
                &self.actor_id,
                None,
                Some(MAX_FOCUS_MEMORIES + 1),
            )?
        } else {
            Vec::new()
        };
        let mut omissions = Vec::new();
        let blockers = status.blockers.clone();
        if unfinished_child_count > visible_unfinished_child_count {
            omissions.push(WorkSectionOmission {
                section: WorkNextSection::Focus,
                reason: WorkSectionOmissionReason::UnfinishedChildCountLimit,
                omitted_count: unfinished_child_count - visible_unfinished_child_count,
            });
        }
        if terminal_child_count > visible_terminal_child_count {
            omissions.push(WorkSectionOmission {
                section: WorkNextSection::Focus,
                reason: WorkSectionOmissionReason::TerminalChildCountLimit,
                omitted_count: terminal_child_count - visible_terminal_child_count,
            });
        }
        if handoffs.len() > MAX_FOCUS_RELATIONS {
            omissions.push(count_omission(
                WorkNextSection::Focus,
                handoffs.len() - MAX_FOCUS_RELATIONS,
            ));
        }
        if blockers.len() > MAX_FOCUS_RELATIONS {
            omissions.push(count_omission(
                WorkNextSection::Focus,
                blockers.len() - MAX_FOCUS_RELATIONS,
            ));
        }
        if evidence_count > evidence_items.len() {
            omissions.push(WorkSectionOmission {
                section: WorkNextSection::Focus,
                reason: WorkSectionOmissionReason::EvidenceCountLimit,
                omitted_count: evidence_count - evidence_items.len(),
            });
        }
        if memories.len() > usize::try_from(MAX_FOCUS_MEMORIES).unwrap_or(usize::MAX) {
            omissions.push(count_omission(
                WorkNextSection::Focus,
                memories.len() - usize::try_from(MAX_FOCUS_MEMORIES).unwrap_or(usize::MAX),
            ));
        }
        let control_binding = run.as_ref().and_then(|run| {
            owned_control_work_binding(&status.work, run, claim.as_ref(), &self.session_id, now)
        });
        let outcome = status.work.outcome.clone();
        let (prerequisites, prerequisite_omissions) = bounded_prerequisite_summaries(
            prerequisite_page.items,
            prerequisite_page.omitted_by_state,
        );
        omissions.extend(prerequisite_omissions);
        let mut view = WorkFocusView {
            session: agent_work_session(&session),
            status: ready_work_summary(status),
            completed_by_record,
            outcome,
            run: run.as_ref().map(work_run_summary),
            claim,
            control_binding,
            children: children
                .into_iter()
                .take(visible_child_count)
                .map(|work| work_item_summary(&work))
                .collect(),
            child_count,
            prerequisites,
            handoffs: handoffs
                .iter()
                .take(MAX_FOCUS_RELATIONS)
                .map(work_handoff_summary)
                .collect(),
            blockers: blockers
                .into_iter()
                .take(MAX_FOCUS_RELATIONS)
                .map(|blocker| WorkBlockerSummary {
                    blocker_id: blocker.blocker_id,
                    kind: blocker.kind,
                    detail: compact_text(&blocker.detail),
                })
                .collect(),
            evidence,
            evidence_items,
            evidence_count,
            latest_evidence_item,
            obligation_page,
            memories: memories
                .into_iter()
                .take(usize::try_from(MAX_FOCUS_MEMORIES).unwrap_or(usize::MAX))
                .map(work_memory_index)
                .collect(),
            history: WorkHistoryView {
                total: history_total,
                omitted: history_total.saturating_sub(history.len()),
                items: history,
            },
            restored_history,
            waivable_required_children,
            allowed_next,
            omissions,
        };
        fit_focus_response(&mut view)?;
        ensure_agent_response_budget(&view, "work_focus")?;
        Ok(view)
    }

    pub(super) fn work_guidance(
        &self,
        store: &SqliteStore,
        work_id: WorkId,
        now: DateTime<Utc>,
    ) -> Result<WorkGuidance, StoreError> {
        let status = store.inspect_work(work_id, now)?;
        let claim = store.current_work_claim_for_item(&status.work)?;
        let handoffs = store.work_handoff_offers(work_id)?;
        let waivable_required_children = store
            .waivable_required_children(&status.work, MAX_FOCUS_RELATIONS)?
            .into_iter()
            .map(required_child_waiver_candidate)
            .collect::<Vec<_>>();
        let (completion_capture_ready, completion_preflight_ready) = store
            .work_completion_readiness_for_item(
                &status.work,
                claim.as_ref(),
                &self.session_id,
                now,
            )?;
        let claim_recovery_required = store.work_claim_recovery_required_for_item(
            &status.work,
            claim.as_ref(),
            &self.session_id,
        )?;
        let next = allowed_next(
            &status,
            AllowedNextContext {
                claim: claim.as_ref(),
                handoffs: &handoffs,
                session: &self.session_id,
                now,
                can_waive_required_child: !waivable_required_children.is_empty(),
                claim_recovery_required,
                completion_capture_ready,
                completion_preflight_ready,
            },
        );
        Ok(WorkGuidance {
            status,
            allowed_next: next,
            waivable_required_children,
            claim,
            handoffs,
        })
    }
}

fn restored_history_view(records: Vec<crate::RestoredRecord>) -> RestoredHistoryView {
    let mut entries = Vec::new();
    for record in records {
        let generation_index = record.generation_index;
        entries.extend(record.history.notes.into_iter().map(|note| {
            RestoredHistoryEntry {
                generation_index,
                kind: if note
                    .actor
                    .provenance_chain
                    .iter()
                    .any(crate::domain::is_non_holder_note_marker)
                {
                    "non_holder_note".to_owned()
                } else {
                    work_evidence_kind_word(note.evidence_kind).to_owned()
                },
                summary: compact_text(&note.summary),
                actor: note.actor,
                created_at: note.recorded_at,
            }
        }));
        entries.extend(record.history.events.into_iter().map(|event| {
            let summary = event.reason.unwrap_or_else(|| {
                event.lifecycle.map_or_else(
                    || event.kind.clone(),
                    |lifecycle| work_lifecycle_word(lifecycle).to_owned(),
                )
            });
            RestoredHistoryEntry {
                generation_index,
                kind: event.kind,
                summary: compact_text(&summary),
                actor: event.actor,
                created_at: event.occurred_at,
            }
        }));
        if let Some(completion) = record.history.completion {
            entries.push(RestoredHistoryEntry {
                generation_index,
                kind: "completed".into(),
                summary: compact_text(&completion.summary),
                actor: completion.actor,
                created_at: completion.completed_at,
            });
        }
    }
    entries.sort_by_key(|entry| (entry.generation_index, entry.created_at));
    let total = entries.len();
    let keep = usize::try_from(MAX_FOCUS_HISTORY).unwrap_or(usize::MAX);
    let omitted = total.saturating_sub(keep);
    if omitted > 0 {
        entries.drain(..omitted);
    }
    RestoredHistoryView {
        total,
        items: entries,
        omitted,
    }
}

#[cfg(test)]
mod tests;
