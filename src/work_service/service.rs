use super::{
    ACTOR_CONTEXT_NORMALIZED_REFERENCE, ACTOR_CONTEXT_PROVENANCE_REFERENCE, ActorContext,
    AllowedNextContext, AssuranceLevel, CanonicalObject, DateTime, LocalWorkService,
    MAX_FOCUS_HISTORY, MAX_FOCUS_MEMORIES, MAX_FOCUS_RELATIONS, Mutex, MutexGuard, OnceLock,
    POST_COMPLETION_EVIDENCE_PROVENANCE_REFERENCE, POST_COMPLETION_EVIDENCE_PROVENANCE_SOURCE,
    PROCESS_DEFAULT_WORK_SESSION_NAMESPACE, PathBuf, ProjectId, ProvenanceLink, ProvenanceRelation,
    REJECT_PROTOCOL_OPERATION, RestoredHistoryEntry, RestoredHistoryView, Serialize, SessionId,
    SqliteStore, StoreError, Utc, WorkActorDefaultSource, WorkAttributionDefaults,
    WorkBlockerSummary, WorkChange, WorkChangeProjection, WorkClaim, WorkClaimState,
    WorkCoreOperationKey, WorkDerivedKey, WorkFocusView, WorkGraphSnapshotDestinationKind,
    WorkGraphSnapshotExport, WorkGraphSnapshotLoadResult, WorkGuidance, WorkHistoryView, WorkId,
    WorkItem, WorkNextSection, WorkPlanningAuthority, WorkProtocolBasis, WorkProtocolIntent,
    WorkSectionOmission, WorkSectionOmissionReason, agent_work_session, allowed_next,
    bindable_control_work_binding, bounded_prerequisite_summaries, child_lifecycle_is_unfinished,
    child_lifecycle_priority, compact_text, count_omission, disclosed_work_obligation_page,
    ensure_agent_response_budget, fit_focus_response, normalize_actor_context,
    prioritized_focus_evidence, ready_work_summary, required_child_waiver_candidate,
    restored_work_evidence_summary, validate_process_default_work_session, work_evidence_kind_word,
    work_evidence_summary, work_handoff_summary, work_item_summary, work_lifecycle_word,
    work_memory_index, work_observation_summary, work_run_summary,
};

use std::cell::RefCell;
use std::rc::Rc;

thread_local! {
    /// One read word's read-only connection, while that word runs on this
    /// thread; see [`LocalWorkService::one_read_connection`].
    static READ_SCOPE: RefCell<Option<ReadScope>> = const { RefCell::new(None) };
}

struct ReadScope {
    /// The service whose word opened the scope; another service's reads on
    /// this thread, such as a test's second session, keep their own.
    owner: usize,
    store: Option<Rc<SqliteStore>>,
}

/// A read-only store connection, shared by the reads of one read word.
pub(crate) struct ReadStore(Rc<SqliteStore>);

impl std::ops::Deref for ReadStore {
    type Target = SqliteStore;

    fn deref(&self) -> &SqliteStore {
        &self.0
    }
}

/// Only safe agent detail requests full contract text. Core/list projections
/// retain their existing summary shape and field bounds.
#[derive(Clone, Copy)]
pub(super) enum FocusText {
    Summary,
    Full,
}

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
            read_only: false,
            #[cfg(test)]
            delivery_stage_hook: None,
            #[cfg(test)]
            advisory_read_hook: None,
            #[cfg(test)]
            focus_children_hook: None,
        }
    }
    /// The same service in read-only mode, for a connection that must not
    /// write: it never opens or returns the writable connection. Made from a
    /// service that has not yet opened one.
    #[must_use]
    pub fn into_read_only(mut self) -> Self {
        self.read_only = true;
        self
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

    /// Read words validate attribution, then open the existing store
    /// read-only for this one call, as peek does: they record nothing, never
    /// register a process-default session (its first stateful operation still
    /// does that in `store_at`), and never create or initialize a store; a
    /// path without one refuses with `store_not_initialized`. A refusal is
    /// never retried through the writable connection.
    pub(super) fn read_store_at(&self, now: DateTime<Utc>) -> Result<ReadStore, StoreError> {
        self.validate_read_attribution(now)?;
        let owner = self.read_scope_owner();
        let scoped = READ_SCOPE.with(|scope| -> Result<_, StoreError> {
            let mut scope = scope.borrow_mut();
            let Some(scope) = scope.as_mut().filter(|scope| scope.owner == owner) else {
                return Ok(None);
            };
            if scope.store.is_none() {
                let store = SqliteStore::open_existing_read_only(&self.database)?;
                // Every read of this word joins one transaction, so the
                // word's parts share a single snapshot.
                store.begin_held_read()?;
                scope.store = Some(Rc::new(store));
            }
            Ok(scope.store.clone())
        })?;
        match scoped {
            Some(store) => Ok(ReadStore(store)),
            None => Ok(ReadStore(Rc::new(SqliteStore::open_existing_read_only(
                &self.database,
            )?))),
        }
    }

    fn read_scope_owner(&self) -> usize {
        std::ptr::from_ref(self) as usize
    }

    /// Runs one read word so that every read it makes on this thread shares
    /// a single read-only connection and one read transaction, opened by its
    /// first read and ended when the word returns: a word opens its store
    /// once and sees one snapshot, however many reads it makes. A word
    /// already inside such a scope keeps it.
    pub(crate) fn one_read_connection<T>(&self, read: impl FnOnce() -> T) -> T {
        struct Restore(Option<ReadScope>);
        impl Drop for Restore {
            fn drop(&mut self) {
                let ended = READ_SCOPE
                    .with(|scope| std::mem::replace(&mut *scope.borrow_mut(), self.0.take()));
                if let Some(store) = ended.and_then(|scope| scope.store) {
                    // A read-only transaction holds nothing to keep; ending
                    // it only releases the snapshot.
                    let _ = store.end_held_read();
                }
            }
        }
        let owner = self.read_scope_owner();
        let nested = READ_SCOPE.with(|scope| {
            scope
                .borrow()
                .as_ref()
                .is_some_and(|scope| scope.owner == owner)
        });
        if nested {
            return read();
        }
        let outer =
            READ_SCOPE.with(|scope| scope.borrow_mut().replace(ReadScope { owner, store: None }));
        let _restore = Restore(outer);
        read()
    }

    /// The cached writable connection for the one advisory record a read
    /// word makes: the memories listing that carries a context generation.
    /// Like a read, it does not register a process-default session.
    pub(super) fn record_store_at(
        &self,
        now: DateTime<Utc>,
    ) -> Result<MutexGuard<'_, SqliteStore>, StoreError> {
        self.lock_store_at(now)
    }

    fn lock_store_at(&self, now: DateTime<Utc>) -> Result<MutexGuard<'_, SqliteStore>, StoreError> {
        self.validate_read_attribution(now)?;
        self.lock_validated_store()
    }

    /// Shared admission for both cached work calls and non-registering peek.
    pub(super) fn validate_read_attribution(&self, now: DateTime<Utc>) -> Result<(), StoreError> {
        if self.actor_id.trim().is_empty() || self.session_id.0.trim().is_empty() {
            return Err(StoreError::InvalidWork(
                "local work requires a non-empty asserted actor and session binding".into(),
            ));
        }
        crate::storage::admit_session_id(&self.session_id)?;
        validate_process_default_work_session(
            &self.session_id,
            self.attribution_defaults.session,
            now,
        )
    }

    fn lock_validated_store(&self) -> Result<MutexGuard<'_, SqliteStore>, StoreError> {
        // Before opening, and before returning one already open.
        if self.read_only {
            return Err(StoreError::InvalidWork(
                "this connection is read-only and never opens the store for writing".into(),
            ));
        }
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
        let started = crate::phase_trace::start();
        let store = cached.lock();
        crate::phase_trace::finish(crate::phase_trace::Phase::StoreMutexWait, started);
        let store = store.map_err(|_| {
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
        // The item, its claim and its handoff offers are compared as one
        // basis, so they come from one commit even before a write opens.
        store.work_read_snapshot(|store| {
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
        })
    }

    /// The protocol basis of a core operation that may act on the ambient
    /// focus: [`Self::protocol_basis`] for a named target. With none, the
    /// operation follows the agent words' implicit-target rule: when the
    /// focus is not an item this session holds while it holds other live
    /// claims in the project, it is refused with
    /// [`StoreError::WorkImplicitTargetConflict`] and nothing is recorded.
    /// The focus, its claim and the session's held claims come from one
    /// snapshot, and the basis returned is the one checked, so a concurrent
    /// focus change cannot pass the check and then redirect the write.
    ///
    /// A retry of an act already admitted is exempt, so a lost response is
    /// answered even after the focus moved: a caller key under which this
    /// session began an attempt of the same intent that finished, or whose
    /// core write `core_operation` committed before the attempt could finish.
    /// Any other request with that key, and an attempt that wrote nothing,
    /// is checked like a new act.
    #[allow(
        clippy::too_many_arguments,
        reason = "the retry exemption needs the operation, its key, its intent and its core write"
    )]
    pub(super) fn ambient_protocol_basis<T: Serialize>(
        &self,
        store: &SqliteStore,
        target: Option<WorkId>,
        operation: &str,
        core_operation: &str,
        caller_key: &str,
        intent: &WorkProtocolIntent<'_, T>,
        now: DateTime<Utc>,
    ) -> Result<WorkProtocolBasis, StoreError> {
        if target.is_some()
            || self.admitted_retry(store, operation, core_operation, caller_key, intent)?
        {
            return self.protocol_basis(store, true, false, target, now);
        }
        store.work_read_snapshot(|store| {
            let work = self.focused_item(store, None, now)?;
            let claim = store.current_work_claim(work.work_id)?;
            let held = store.work_held_refs_in_project(&self.project_id, &self.session_id, now)?;
            match self.implicit_target_refusal(
                operation,
                ImplicitTargetRule::FocusHeld,
                &work,
                claim.as_ref(),
                held,
                now,
            ) {
                Some(refusal) => Err(refusal),
                None => Ok(WorkProtocolBasis {
                    focused_work: Some(work),
                    claim,
                    handoffs: Vec::new(),
                }),
            }
        })
    }

    /// The refusal a core `operation` that named no item meets on the focus
    /// `work`, read with its `claim` and this session's live claims `held`
    /// (in ref order) in one snapshot, or `None` when it acts on the focus.
    /// A focus the session does not hold is refused while it holds other
    /// work, as the agent words refuse it; a held focus is refused only under
    /// [`ImplicitTargetRule::SoleClaim`] while another claim is live beside
    /// it, as a bare `done` or `evaluate` is.
    pub(super) fn implicit_target_refusal(
        &self,
        operation: &str,
        rule: ImplicitTargetRule,
        work: &WorkItem,
        claim: Option<&WorkClaim>,
        held: Vec<(WorkId, String)>,
        now: DateTime<Utc>,
    ) -> Option<StoreError> {
        if held.iter().any(|(work_id, _)| *work_id == work.work_id) {
            if rule == ImplicitTargetRule::SoleClaim && held.len() > 1 {
                let held: Vec<String> = held.into_iter().map(|(_, short_ref)| short_ref).collect();
                return Some(StoreError::WorkBareTargetAmbiguous(Box::new(
                    crate::storage::BareTargetAmbiguity::new(operation, &work.short_ref, &held),
                )));
            }
            return None;
        }
        let others: Vec<String> = held
            .into_iter()
            .filter(|(work_id, _)| *work_id != work.work_id)
            .map(|(_, short_ref)| short_ref)
            .collect();
        if others.is_empty() {
            return None;
        }
        let held_elsewhere = claim.is_some_and(|claim| {
            claim.state == crate::domain::WorkClaimState::Active
                && claim.expires_at > now
                && claim.holder != self.session_id
        });
        let focus_state = if held_elsewhere {
            crate::storage::ImplicitFocusState::HeldElsewhere
        } else if work.lifecycle != crate::domain::WorkLifecycle::Open {
            crate::storage::ImplicitFocusState::NotOpen
        } else {
            crate::storage::ImplicitFocusState::Unclaimed
        };
        let more = others
            .len()
            .saturating_sub(crate::storage::IMPLICIT_TARGET_HELD_SHOWN);
        Some(StoreError::WorkImplicitTargetConflict(Box::new(
            crate::storage::ImplicitTargetConflict {
                operation: operation.to_owned(),
                focus: work.short_ref.clone(),
                focus_state,
                focus_lifecycle: work.lifecycle,
                held: others
                    .into_iter()
                    .take(crate::storage::IMPLICIT_TARGET_HELD_SHOWN)
                    .collect(),
                more,
            },
        )))
    }

    /// Whether a keyed request repeats an act this session already admitted:
    /// its attempt for the same intent finished, or its core write committed.
    fn admitted_retry<T: Serialize>(
        &self,
        store: &SqliteStore,
        operation: &str,
        core_operation: &str,
        caller_key: &str,
        intent: &WorkProtocolIntent<'_, T>,
    ) -> Result<bool, StoreError> {
        let caller_key = caller_key.trim();
        if caller_key.is_empty() {
            return Ok(false);
        }
        Ok(
            match store.work_protocol_attempt_finished(
                &self.project_id,
                &self.session_id,
                operation,
                caller_key,
                CanonicalObject::freeze(intent)?.key(),
            )? {
                None => false,
                Some(true) => true,
                Some(false) => store
                    .work_operation_result_value(
                        core_operation,
                        &self.core_operation_key(operation, caller_key, core_operation)?,
                    )?
                    .is_some(),
            },
        )
    }

    /// The core idempotency key of every service operation except a plan:
    /// proposals of a root or a decomposition, updates, completion and
    /// handoff. Plans never use it: storage derives a plan's key itself, from
    /// a different tuple with no `work:` prefix (see `plan_operation_key`),
    /// and the two must stay apart or stored plans stop replaying.
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
        Ok(format!("work:{}", object.key().as_str()))
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
        if protocol_operation == crate::storage::DECOMPOSE_PROTOCOL_OPERATION {
            return self.decomposition_idempotency_key(basis, intent.key());
        }
        if protocol_operation == REJECT_PROTOCOL_OPERATION {
            return self.rejection_idempotency_key(basis, intent.key());
        }
        if protocol_operation == "work_complete" {
            // Unlinked keyless completion belongs to one run: sealing keeps
            // its identity, while reopen/new run makes the same intent fresh.
            // A runless restored item uses None until a claim bootstraps it.
            let identity = CanonicalObject::freeze(&serde_json::json!({
                "project": self.project_id,
                "session": self.session_id,
                "operation": protocol_operation,
                "work": basis.focused_work.as_ref().map(|work| work.work_id),
                "run": basis.completion_run_id(),
                "intent": intent.key(),
            }))?;
            return Ok(format!("completion:{}", identity.key()));
        }
        let basis_object = CanonicalObject::freeze(&basis.retry_stable())?;
        let object = CanonicalObject::freeze(&WorkDerivedKey {
            project_id: &self.project_id,
            session_id: &self.session_id,
            protocol_operation,
            focused_work_id: basis.focused_work.as_ref().map(|work| work.work_id),
            basis: basis_object.key(),
            claim_live: basis
                .claim
                .as_ref()
                .map(|claim| claim.state == WorkClaimState::Active && claim.expires_at > now),
            intent: intent.key(),
        })?;
        Ok(format!("auto:{}", object.key().as_str()))
    }

    /// Starts a journal of this connection's focus moves for one word; the
    /// word takes the net change when it finishes.
    pub(crate) fn focus_journal(&self) -> crate::storage::FocusJournal {
        crate::storage::FocusJournal::begin(&self.project_id, &self.session_id)
    }

    /// The project's active acceptance-evaluation policy.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the store or its control policy cannot be
    /// read.
    pub(crate) fn acceptance_evaluation_policy(
        &self,
        now: DateTime<Utc>,
    ) -> Result<crate::domain::AcceptanceEvaluationPolicy, StoreError> {
        self.store_at(now)?.acceptance_evaluation_policy()
    }

    /// The keyless attempt identity of clearing one named blocker: this
    /// session's clear of that blocker on that item, with the same canonical
    /// intent. It leaves out the item's revision, which the clear itself
    /// bumps, so repeating the call finds the attempt it made. The attempt's
    /// recorded basis still refuses an unfinished retry after the item
    /// changed.
    pub(super) fn selected_unblock_idempotency_key<T: Serialize>(
        &self,
        basis: &WorkProtocolBasis,
        blocker_id: &str,
        intent: &WorkProtocolIntent<'_, T>,
    ) -> Result<String, StoreError> {
        let work = basis.focused_work.as_ref().ok_or_else(|| {
            StoreError::InvalidWorkProjection("a selected unblock has no bound item".into())
        })?;
        let intent = CanonicalObject::freeze(intent)?;
        let key = CanonicalObject::freeze(&serde_json::json!({
            "project": self.project_id,
            "session": self.session_id,
            "operation": "work_update:unblock",
            "work": work.work_id,
            "blocker": blocker_id,
            "intent": intent.key(),
        }))?;
        Ok(format!("auto:{}", key.key().as_str()))
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

    /// Resolves an optional caller-supplied target without moving focus.
    pub(super) fn resolve_target(
        &self,
        store: &SqliteStore,
        work_ref: Option<&str>,
    ) -> Result<Option<WorkId>, StoreError> {
        work_ref
            .map(|work_ref| {
                store
                    .resolve_work_ref(&self.project_id, work_ref)
                    .map(|work| work.work_id)
            })
            .transpose()
    }

    /// Makes `target` the ambient focus only when this session holds its live
    /// claim. A word acting on someone else's item (a peer's note, a late
    /// gate, an independent evaluation) leaves focus, and so the claim the
    /// session's next host turn binds, where it was.
    pub(super) fn focus_if_held(
        &self,
        store: &mut SqliteStore,
        target: Option<WorkId>,
        claim: Option<&WorkClaim>,
        now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        if let Some(target) = target
            && claim.is_some_and(|claim| {
                claim.holder == self.session_id
                    && claim.state == WorkClaimState::Active
                    && claim.expires_at > now
            })
        {
            store.focus_work_session(&self.project_id, &self.session_id, target, now)?;
        }
        Ok(())
    }

    /// Whether a record was made by this reader's own actor in another
    /// session, with both sides asserted rather than defaulted: the same
    /// byte-exact actor id, recorded as the agent words record, at asserted
    /// assurance, with no shell-default marker on either side. A defaulted
    /// actor id names a machine user, not an agent, so it never relates
    /// sessions. The relation is for navigation and display only; it grants
    /// nothing.
    pub(crate) fn same_asserted_actor_elsewhere(&self, actor: &ActorContext) -> bool {
        self.attribution_defaults.actor.is_none()
            && actor.actor_id == self.actor_id
            && actor.actor_kind == super::WORD_ACTOR_KIND
            && actor.assurance == AssuranceLevel::Asserted
            && !actor.actor_defaulted()
            && actor
                .session_id
                .as_ref()
                .is_some_and(|session| *session != self.session_id)
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
                    WorkActorDefaultSource::OsUserEnvironment => {
                        crate::domain::DEFAULTED_OS_USER_ACTOR_SOURCE
                    }
                    WorkActorDefaultSource::ProcessFallback => {
                        crate::domain::DEFAULTED_PROCESS_ACTOR_SOURCE
                    }
                }
                .into(),
                reference: Some(crate::domain::DEFAULTED_ACTOR_REFERENCE.into()),
            });
        }
        if self.attribution_defaults.session {
            provenance_chain.push(ProvenanceLink {
                relation: ProvenanceRelation::DerivedFrom,
                source: crate::domain::DEFAULTED_PROCESS_SESSION_SOURCE.into(),
                reference: Some(crate::domain::DEFAULTED_SESSION_REFERENCE.into()),
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
            actor_kind: super::WORD_ACTOR_KIND.into(),
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
        crate::domain::non_holder_note_actor(
            self.actor("work_update", "record a non-holder work observation"),
        )
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

    pub(super) fn focus_view(
        &self,
        store: &SqliteStore,
        work_id: WorkId,
        with_memories: bool,
        with_latest_evidence: bool,
        now: DateTime<Utc>,
    ) -> Result<WorkFocusView, StoreError> {
        // One view, one commit: its rows are cross-checked against each other.
        let mut view = store.work_read_snapshot(|store| {
            self.focus_view_for_projection(
                store,
                work_id,
                with_memories,
                with_latest_evidence,
                FocusText::Summary,
                now,
            )
        })?;
        fit_focus_response(&mut view)?;
        ensure_agent_response_budget(&view, "work_focus")?;
        Ok(view)
    }

    // Count and field bounds apply here; only the emitted representation may
    // decide whether whole rows need shedding for bytes.
    #[allow(
        clippy::too_many_lines,
        reason = "the bounded focus packet is assembled in one place so every relation and omission limit is visible"
    )]
    pub(super) fn focus_view_for_projection(
        &self,
        store: &SqliteStore,
        work_id: WorkId,
        with_memories: bool,
        with_latest_evidence: bool,
        text: FocusText,
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
        let (acceptance_evidence, acceptance_evidence_error_class) =
            if matches!(text, FocusText::Full)
                && status.work.lifecycle == crate::WorkLifecycle::Completed
                && !completed_by_record
            {
                match super::acceptance::for_completed_run(store, run.as_ref(), work_id) {
                    Ok(facts) => (Some(facts), None),
                    Err(error) => (None, Some(super::advisory_error_class(&error))),
                }
            } else {
                (None, None)
            };
        let obligation_records = run
            .as_ref()
            .map(|run| store.work_run_obligations(run.run_id))
            .transpose()?
            .unwrap_or_default();
        let obligation_page = disclosed_work_obligation_page(
            store,
            &obligation_records,
            run.as_ref()
                .is_some_and(super::projection::obligations_are_historical),
        )?;
        // A historical page owes nothing, so it shows no evaluation rows.
        let evaluation_obligation_rows_visible = obligation_page
            .items
            .iter()
            .filter(|item| {
                !obligation_page.historical && item.state == crate::WorkObligationState::Open
            })
            .count();
        let mut evidence_count = run
            .as_ref()
            .map(|run| store.work_run_evidence_count(run.run_id))
            .transpose()?
            .unwrap_or_default();
        let evidence_candidates = run
            .as_ref()
            .map(|run| store.work_run_evidence_projection(run.run_id, MAX_FOCUS_RELATIONS))
            .transpose()?
            .unwrap_or_default();
        let evidence = prioritized_focus_evidence(evidence_candidates);
        let native_evidence_items = run
            .as_ref()
            .map(|run| {
                evidence
                    .iter()
                    .map(|evidence_id| work_evidence_summary(store, run.run_id, evidence_id))
                    .collect::<Result<Vec<_>, _>>()
            })
            .transpose()?
            .unwrap_or_default();
        let native_latest_evidence_item = if with_latest_evidence {
            run.as_ref()
                .map(|run| {
                    store
                        .latest_work_run_evidence(run.run_id)?
                        .map(|evidence_id| work_evidence_summary(store, run.run_id, &evidence_id))
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
        for (evidence_id, evidence) in restored_evidence {
            evidence_items.push(restored_work_evidence_summary(evidence_id, &evidence)?);
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
        for (index, (observation_id, observation)) in observations.into_iter().enumerate() {
            let summary = work_observation_summary(observation_id, &observation);
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
        if with_latest_evidence {
            // Show emits its page in the item's dense root-work feed order,
            // which every row has; asserted timestamps stay metadata. Only the
            // position is recorded here, so selection and priority are unchanged.
            for item in evidence_items
                .iter_mut()
                .chain(latest_evidence_item.as_mut())
            {
                item.root_position =
                    Some(store.work_root_object_position(status.work.root_id, &item.evidence)?);
            }
        }
        let history_total = store.work_event_count(work_id)?;
        let mut history = Vec::new();
        for entry in store.work_event_tail(work_id, MAX_FOCUS_HISTORY)? {
            let event = store
                .get::<crate::WorkEvent>(&entry.object_id)?
                .ok_or_else(|| {
                    StoreError::InvalidWorkProjection(format!(
                        "root-work feed object {} is missing",
                        entry.object_id
                    ))
                })?;
            if event.work_id != work_id {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "targeted history returned event for {} while loading {}",
                    event.work_id.0, work_id.0
                )));
            }
            // One read of the event's facts serves the stored summary and the
            // transient display.
            let display =
                super::history_display::HistoryDisplay::load(store, &event, &entry.position)?;
            let summary = display.stored_summary();
            history.push(WorkChange {
                history_display: Some(display),
                capture: None,
                completion_checkpoint: None,
                display_producer: Some(super::views::DisplayProducer::of(&event.actor)),
                from_current_session: event.actor.session_id.as_ref() == Some(&self.session_id),
                entry,
                delivery: WorkChangeProjection::Visible(summary),
            });
        }
        let restored_history = restored_history_view(store.work_restored_records(work_id)?);
        #[cfg(test)]
        if matches!(text, FocusText::Full)
            && let Some(hook) = &self.focus_children_hook
        {
            hook.entered.wait();
            hook.release.wait();
        }
        let mut children = store.work_children(work_id)?;
        let child_obligations = if matches!(text, FocusText::Full) && !children.is_empty() {
            Some(super::focus::child_obligations(
                store,
                &status.work,
                run.as_ref(),
                &children,
            )?)
        } else {
            None
        };
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
        let control_binding = run
            .as_ref()
            .map(|run| {
                bindable_control_work_binding(
                    store,
                    &self.project_id,
                    &self.session_id,
                    &status.work,
                    run,
                    claim.as_ref(),
                    now,
                )
            })
            .transpose()?
            .flatten();
        let outcome = match text {
            FocusText::Summary => compact_text(&status.work.outcome),
            FocusText::Full => status.work.outcome.clone(),
        };
        let (prerequisites, prerequisite_omissions) = bounded_prerequisite_summaries(
            prerequisite_page.items,
            prerequisite_page.omitted_by_state,
        );
        omissions.extend(prerequisite_omissions);
        let full_acceptance =
            matches!(text, FocusText::Full).then(|| status.work.acceptance.clone());
        let detached_from = store
            .detached_work_origin(&status.work)?
            .map(|(source, reason)| {
                let bounded = match text {
                    FocusText::Summary => compact_text(&reason),
                    FocusText::Full => reason.clone(),
                };
                super::WorkDetachedFrom {
                    work_ref: source,
                    reason_truncated: bounded != reason,
                    reason: bounded,
                }
            });
        let successor = if matches!(text, FocusText::Full) {
            store.required_child_successor(&status.work)?
        } else {
            None
        };
        let parent = if matches!(text, FocusText::Full) {
            status
                .work
                .parent_id
                .map(|parent_id| {
                    let parent = store.get_work_item(parent_id)?;
                    if parent.project_id != status.work.project_id
                        || parent.root_id != status.work.root_id
                    {
                        return Err(StoreError::InvalidWorkProjection(
                            "focused work parent crosses its project or root boundary".into(),
                        ));
                    }
                    Ok(super::WorkParentSummary {
                        short_ref: parent.short_ref,
                        title: compact_text(&parent.title),
                        lifecycle: parent.lifecycle,
                    })
                })
                .transpose()?
        } else {
            None
        };
        // Degrade only advisory fields whose absence can be stated truthfully;
        // unreadable item/history context must refuse, not imply completeness.
        let (source, source_error_class) = if matches!(text, FocusText::Full) {
            match store.work_source_for_item(status.work.work_id) {
                Ok(source) => (source, None),
                Err(error) => (None, Some(super::advisory_error_class(&error))),
            }
        } else {
            (None, None)
        };
        // The newest evaluation is agent-show detail for open items; it is
        // read with the freshness completion would apply at this moment.
        // A completed item discloses where its sealed acceptance came from,
        // read from the frozen seal through the shared binding check.
        // A readable seal whose evaluation binding fails that check is
        // disclosed by class rather than swallowed, so a broken evaluated
        // binding never reads like self-assertion. Missing provenance is
        // reported as unavailable; the evidence read supplies diagnostics.
        // The landing the seal records is read from the same bound seal.
        let (mut acceptance_provenance, mut acceptance_provenance_error_class) = (None, None);
        let (mut landing, mut landing_unavailable) = (None, None);
        if matches!(text, FocusText::Full)
            && status.work.lifecycle == crate::WorkLifecycle::Completed
        {
            let sealed = run
                .as_ref()
                .and_then(|run| {
                    run.completion_seal
                        .as_ref()
                        .map(|seal_id| (run.run_id, seal_id))
                })
                .filter(|_| !completed_by_record)
                .map(|(run_id, seal_id)| {
                    super::acceptance::bound_seal(store, seal_id, work_id, run_id)
                });
            // A seal the run names but that cannot be read or bound is
            // classed too, so it never reads like an item with no seal.
            match &sealed {
                Some(Ok(seal)) => match super::acceptance::provenance(store, seal) {
                    Ok(provenance) => acceptance_provenance = Some(provenance),
                    Err(error) => {
                        acceptance_provenance_error_class =
                            Some(super::advisory_error_class(&error));
                    }
                },
                Some(Err(error)) => {
                    acceptance_provenance_error_class = Some(super::advisory_error_class(error));
                }
                None => {}
            }
            (landing, landing_unavailable) =
                completed_landing(completed_by_record, sealed.as_ref());
        }
        // Agent detail for an open item under an evaluated policy: the
        // evidence basis an evaluator passes back, and the newest record with
        // the freshness completion would apply now. Self-asserted projects keep their
        // unchanged show shape.
        let evaluated_policy = status.work.lifecycle == crate::domain::WorkLifecycle::Open
            && store.acceptance_evaluation_policy()?.is_evaluated();
        let (acceptance_evaluation, evidence_basis) = if evaluated_policy {
            let evidence_basis = status
                .work
                .active_run_id
                .map(|run_id| store.work_feed_head(&crate::domain::FeedId::RunExecution(run_id)))
                .transpose()?;
            (
                store.acceptance_evaluation_status(status.work.work_id, None)?,
                evidence_basis,
            )
        } else {
            (None, None)
        };
        let acceptance_placeholder = acceptance_placeholder(store, &status.work)?;
        // Under an evaluated policy the seal cites the evaluation's citations,
        // and a pass cannot cite nothing, so only self-asserted work asks.
        // An open item without an active run, such as one restored and not
        // yet claimed, has no obligation that could link a criterion.
        let unlinked_records: &[_] = if status.work.active_run_id.is_some() {
            obligation_records.as_slice()
        } else {
            &[]
        };
        let unlinked_criteria = (status.work.lifecycle == crate::domain::WorkLifecycle::Open
            && !evaluated_policy)
            .then(|| {
                let positions =
                    crate::storage::criteria_without_evidence_link(&status.work, unlinked_records);
                super::views::UnlinkedCriteriaView {
                    bound_check_open: crate::storage::unlinked_criteria_owe_bound_check(
                        &status.work,
                        unlinked_records,
                        &positions,
                    ),
                    positions,
                }
            });
        let title_stored_bytes = status.work.title.len();
        let title_truncated = matches!(text, FocusText::Full)
            && compact_text(&status.work.title) != status.work.title;
        let outcome_stored_bytes = status.work.outcome.len();
        let mut status = ready_work_summary(status);
        if matches!(text, FocusText::Full) {
            (status.work.current_status, status.work.status_observation) =
                self.status_for_item(store, status.work.work_id, now)?;
        }
        status.work.required_child_successor = successor;
        if let Some(acceptance) = full_acceptance {
            status.work.acceptance = acceptance;
        }
        let view = WorkFocusView {
            session_focus: None,
            source,
            source_error_class,
            acceptance_evidence,
            acceptance_evidence_error_class,
            acceptance_provenance_error_class,
            evaluation_rows_visible: acceptance_evaluation
                .as_ref()
                .map_or(0, |status| status.record.verdicts.len()),
            acceptance_evaluation,
            evaluated_policy,
            acceptance_placeholder,
            unlinked_criteria,
            evaluation_obligation_rows_visible,
            evidence_basis,
            acceptance_provenance,
            landing,
            landing_unavailable,
            session: agent_work_session(&session),
            detached_from,
            status,
            parent,
            completed_by_record,
            outcome,
            title_stored_bytes,
            title_truncated,
            outcome_stored_bytes,
            outcome_omitted_bytes: None,
            run: run.as_ref().map(work_run_summary),
            claim,
            control_binding,
            children: children
                .into_iter()
                .take(visible_child_count)
                .map(|work| {
                    let mut summary = work_item_summary(&work);
                    if matches!(text, FocusText::Full) {
                        summary.required_child_successor = store.required_child_successor(&work)?;
                    }
                    Ok(summary)
                })
                .collect::<Result<_, StoreError>>()?,
            child_count,
            child_obligations,
            prerequisites,
            handoffs: handoffs
                .iter()
                .take(MAX_FOCUS_RELATIONS)
                .map(work_handoff_summary)
                .collect(),
            blocker_count: blockers.len(),
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
        Ok(view)
    }

    /// The item's status, claim, handoff offers and next moves, read from one
    /// commit so the rows compared among them cannot straddle another
    /// connection's write.
    pub(super) fn work_guidance(
        &self,
        store: &SqliteStore,
        work_id: WorkId,
        now: DateTime<Utc>,
    ) -> Result<WorkGuidance, StoreError> {
        store.work_read_snapshot(|store| self.work_guidance_on_snapshot(store, work_id, now))
    }

    fn work_guidance_on_snapshot(
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
        let mut next = allowed_next(
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
        if status
            .reason_codes
            .contains(&crate::WorkReadinessReason::DetachAvailable)
        {
            next.push("work_update:detach".into());
        }
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
    let carried_disposals = crate::graph_snapshot::carried_disposal_layers(&records);
    for (record, carried_disposal) in records.into_iter().zip(carried_disposals) {
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
        entries.extend(
            record
                .history
                .events
                .into_iter()
                .filter(|_| !carried_disposal)
                .map(|event| {
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
                }),
        );
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
    // A deterministic presentation order, not a chronology: by generation,
    // then notes, events and completion in the order each record stores
    // them. The stable sort keeps that order; carried timestamps are shown
    // as data and never reorder entries.
    entries.sort_by_key(|entry| entry.generation_index);
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

/// The landing a completed item's `show` discloses: the record its native
/// seal holds, or why none can be read. A completion restored from history,
/// or a run without a seal, has no native seal to read, and a seal that
/// cannot be read is named by class; either way the landing is unavailable,
/// never "no landing recorded".
pub(super) fn completed_landing(
    completed_by_record: bool,
    sealed: Option<&Result<crate::CompletionSeal, StoreError>>,
) -> (
    Option<crate::domain::CompletionLanding>,
    Option<&'static str>,
) {
    match (completed_by_record, sealed) {
        (true, _) => (None, Some("restored completion")),
        (false, None) => (None, Some("no completion seal")),
        (false, Some(Ok(seal))) => (seal.landing.clone(), None),
        (false, Some(Err(error))) => (None, Some(super::advisory_error_class(error))),
    }
}

/// The open item's only criterion while it is still the placeholder the item
/// was created with, `"<creation title> is done"`, and its list has never
/// been revised to anything else. A title revision keeps it; a revision of
/// the list to other criteria drops it for good, even if a later revision
/// restores the sentence. A replacement with the identical text leaves no
/// trace in the store, so it reads like no revision.
pub(super) fn acceptance_placeholder(
    store: &SqliteStore,
    work: &crate::WorkItem,
) -> Result<Option<String>, StoreError> {
    let [criterion] = work.acceptance.as_slice() else {
        return Ok(None);
    };
    // Every placeholder ends so; this keeps real criteria off the store.
    if work.lifecycle != crate::WorkLifecycle::Open || !criterion.ends_with(" is done") {
        return Ok(None);
    }
    Ok(store
        .work_creation_placeholder(work.work_id)?
        .filter(|placeholder| placeholder == criterion))
}

/// What a core operation that named no item may do on a focus this session
/// holds, under the agent words' implicit-target rule.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ImplicitTargetRule {
    /// It acts on a held focus whatever else the session holds: decomposition
    /// and every update form.
    FocusHeld,
    /// It acts on a held focus only while no other claim is live beside it:
    /// completion records a verdict on one item, so the item must be certain.
    SoleClaim,
}

#[cfg(test)]
mod tests;
