use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};

use super::super::{SqliteStore, StoreError};
use super::completion::{ancestors_admit_execution, work_is_ancestor_of};
use super::execution::ensure_restored_execution_state;
use super::feeds::{
    append_work_event, expire_handoff_offers, inspect_work_request, load_typed_work_object,
    replay_operation, request_object, validate_work_source_snapshot,
};
use super::integrity::combined_graph_is_acyclic;
use super::observation::append_initial_notes_on;
#[cfg(test)]
use super::query::latest_canonical_work_event_for_item;
use super::query::{
    active_root_execution, active_run_snapshot, any_canonical_claim_epoch_event,
    load_active_blocker_projections, load_work_claim_optional, load_work_item, load_work_run,
};
use super::{
    MAX_CHILDREN_PER_DECOMPOSITION, MAX_OPEN_WORK_DESCENDANTS, MAX_WORK_DEPTH,
    MAX_WORK_PREREQUISITES_PER_ITEM, MAX_WORK_TTL_SECONDS, WorkEventDraft, WorkRelationBasis,
};
use crate::{
    CanonicalObject, ObjectId,
    domain::{
        AddWorkBlockerRequest, ChangeWorkPrerequisiteRequest, ClearWorkBlockerRequest,
        CompletionWaiver, ControlWorkBinding, CreateWorkRequest, DEFAULT_WORK_CLAIM_TTL_SECONDS,
        DecomposeWorkRequest, ReviseWorkRequest, RootContribution, RootExecution, RootExecutionId,
        RootExecutionState, SCHEMA_VERSION, SessionId, WorkBlocker, WorkClaim, WorkClaimId,
        WorkClaimState, WorkDecomposition, WorkDependencyRef, WorkId, WorkItem, WorkLifecycle,
        WorkOrigin, WorkPlanningAuthority, WorkRun, WorkRunId, WorkRunState, WorkSourceSnapshot,
        WorkTransition,
    },
    memory::Redactor,
};

#[cfg(test)]
mod tests;

mod authority;
mod contributors;
mod normalization;
mod persistence;

pub(crate) use authority::claim_expiry;
pub(super) use authority::{
    assert_actor_session, assert_revision, renew_holder_claim,
    validate_live_claim_for_item_in_scope, validate_live_claim_for_item_on, validate_live_claim_on,
};
use authority::{
    new_root_open_descendant_count, peer_decomposition_admitted, rebase_planning_claim,
    validate_decomposition_budget, validate_planning_authority,
};
#[cfg(test)]
use authority::{root_open_descendant_count, validate_root_descendant_budget};
pub(in crate::storage) use authority::{
    validate_control_work_binding_in_scope, validate_control_work_binding_on,
};
pub(super) use contributors::{
    add_root_contribution, expect_root_contributor, first_unaccounted_root_contributor,
    root_participant_is_accounted, unique_hashes, waive_root_contributor,
};
pub(crate) use normalization::{normalize_acceptance, normalize_note_text, normalize_text};
pub(in crate::storage) use normalization::{normalize_acceptance_criteria, short_ref};
use normalization::{
    normalize_optional, validate_child_count, validate_child_inputs, validate_priority,
    validate_root_input,
};
pub(super) use normalization::{normalize_strings, refuse_record_id_pins_on};
use persistence::{
    blocker_json_search_can_read, initialize_new_work_catalog_projection,
    refuse_longer_text_search_cannot_read,
};
pub(in crate::storage) use persistence::{
    encode_state, initialize_work_catalog_projection, persist_operation_result,
    refresh_work_catalog_projection,
};
pub(super) use persistence::{
    normalize_work_catalog_key, persist_claim, persist_root_execution, persist_work_item,
    persist_work_run, work_catalog_search_text,
};

mod catalog_fts;
mod detach;
mod folder_relations;
mod plan;
mod relations;
mod shape;
pub(in crate::storage::work) mod successor;
pub(super) use folder_relations::{apply_folder_blocker_on, apply_folder_prerequisite_on};
#[cfg(test)]
pub(crate) use plan::plan_validations;
pub(crate) use plan::validate_work_plan;
#[cfg(test)]
pub(super) use relations::relation_basis_validations;
pub(super) use relations::{
    CheckedRelationBasis, apply_work_relation_transition, projected_work_relation_basis,
    require_work_item_relation_integrity, validated_current_work_relation_basis,
    work_relation_fingerprint,
};
use relations::{
    change_work_prerequisite_on, change_work_prerequisite_with_validation_on,
    validate_prerequisite_in_degree, validate_prerequisite_input,
};
pub(crate) use shape::{
    DraftPrerequisiteEdge, RootDraftShape, validate_decomposition_drafts, validate_revision_patch,
    validate_root_draft,
};
use successor::NativePlanningEvent;

/// Atomic plans defer whole-project cycle and root-size scans to their final
/// commit checks and use their own child-count bound. Every other check stays
/// in the shared transitions. Never commit an `AtomicPlan` transition alone.
#[derive(Clone, Copy)]
enum PlanningValidation {
    Immediate,
    AtomicPlan,
}

/// Ephemeral proof for one new item, scoped to a single atomic-plan transaction.
/// Every appended event advances this basis. A final canonical edge audit is
/// mandatory before the owning plan can commit; ordinary mutations never use it.
struct PlanRelationBasis {
    work_id: WorkId,
    basis: WorkRelationBasis,
}

#[cfg(test)]
thread_local! {
    static DESCENDANT_SCAN_COUNT: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    static DESCENDANT_SCAN_VM_STEPS: std::cell::Cell<i32> = const { std::cell::Cell::new(0) };
    static WORK_CATALOG_FTS_DELETE_STATEMENTS: std::cell::Cell<usize> =
        const { std::cell::Cell::new(0) };
}

#[cfg(test)]
pub(super) fn take_work_catalog_fts_delete_statements() -> usize {
    WORK_CATALOG_FTS_DELETE_STATEMENTS.with(|count| count.replace(0))
}

#[cfg(test)]
fn take_descendant_scan_count() -> usize {
    DESCENDANT_SCAN_COUNT.with(|count| count.replace(0))
}

#[cfg(test)]
fn take_descendant_scan_vm_steps() -> i32 {
    DESCENDANT_SCAN_VM_STEPS.with(|steps| steps.replace(0))
}

impl PlanningValidation {
    fn check_graph(self, transaction: &Transaction<'_>, project: &str) -> Result<(), StoreError> {
        if matches!(self, Self::Immediate) && !combined_graph_is_acyclic(transaction, project)? {
            return Err(StoreError::WorkDependencyCycle);
        }
        Ok(())
    }

    fn child_budget(self) -> usize {
        match self {
            Self::Immediate => MAX_CHILDREN_PER_DECOMPOSITION,
            Self::AtomicPlan => crate::domain::MAX_WORK_PLAN_TASKS,
        }
    }
}

/// Creates a root within the caller's write transaction, enforcing root input guards.
/// The caller owns operation replay and commit, including coupled graph changes.
pub(super) fn create_root_on<R: Redactor>(
    transaction: &Transaction<'_>,
    request: &CreateWorkRequest,
    initial_notes: &[String],
    redactor: &R,
) -> Result<WorkItem, StoreError> {
    create_root_with_validation_on(
        transaction,
        request,
        initial_notes,
        redactor,
        PlanningValidation::Immediate,
    )
}

fn create_root_with_validation_on<R: Redactor>(
    transaction: &Transaction<'_>,
    request: &CreateWorkRequest,
    initial_notes: &[String],
    redactor: &R,
    validation: PlanningValidation,
) -> Result<WorkItem, StoreError> {
    validate_root_input(request)?;
    if let Some(snapshot) = request.source_snapshot_id.as_ref() {
        let source = load_typed_work_object::<WorkSourceSnapshot>(
            transaction,
            snapshot,
            "work_source_snapshot",
        )
        .map_err(|error| {
            StoreError::InvalidWork(format!(
                "import source {snapshot} is not a verified work_source_snapshot object: {error}"
            ))
        })?;
        validate_work_source_snapshot(&source, request.created_at)?;
    }

    let title = normalize_text(&request.title, crate::storage::refusal_labels::TITLE)?;
    let outcome = normalize_text(&request.outcome, crate::storage::refusal_labels::OUTCOME)?;
    let work_id = WorkId::new();
    let run_id = WorkRunId::new();
    let root_id = work_id;
    let root_execution = RootExecution {
        schema_version: SCHEMA_VERSION,
        root_execution_id: RootExecutionId::new(),
        project_id: request.project_id.clone(),
        root_id,
        generation: 1,
        state: RootExecutionState::Active,
        revision: 1,
        run_ids: vec![run_id],
        required_child_seals: Vec::new(),
        required_child_waivers: Vec::new(),
        expected_contributors: Vec::new(),
        contributions: Vec::new(),
        waivers: Vec::new(),
        created_at: request.created_at,
        updated_at: request.created_at,
    };
    let (acceptance, acceptance_bindings) =
        normalize_acceptance(&request.acceptance, &request.acceptance_bindings)?;
    refuse_record_id_pins_on(transaction, &acceptance_bindings)?;
    let item = WorkItem {
        schema_version: SCHEMA_VERSION,
        project_id: request.project_id.clone(),
        work_id,
        short_ref: short_ref(work_id),
        root_id,
        parent_id: None,
        child_requirement: request.child_requirement,
        title,
        outcome,
        acceptance,
        acceptance_bindings,
        kind: request.kind,
        priority: request.priority,
        labels: normalize_strings(&request.labels),
        external_ref: crate::domain::normalize_external_reference(request.external_ref.as_deref())
            .map_err(StoreError::InvalidWork)?,
        assigned_to: normalize_optional(request.assigned_to.clone()),
        deferred_until: request.deferred_until,
        origin: request.origin,
        source_snapshot_id: request.source_snapshot_id.clone(),
        lifecycle: WorkLifecycle::Open,
        revision: 1,
        active_run_id: Some(run_id),
        restored: false,
        superseded_by: None,
        evaluation_mode: request.evaluation_mode,
        created_by: request.actor.clone(),
        created_at: request.created_at,
        updated_at: request.created_at,
    };
    transaction.execute(
        "INSERT INTO work_items (
             work_id, project_id, short_ref, root_id, parent_id,
             child_requirement, lifecycle, priority, assigned_to,
             deferred_until_ms, revision, active_run_id, source_snapshot_id,
             created_at_ms, updated_at_ms, item_json
         ) VALUES (
             ?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12, ?13,
             ?14, ?15, ?16
         )",
        params![
            item.work_id.0.to_string(),
            item.project_id.0,
            item.short_ref,
            item.root_id.0.to_string(),
            item.parent_id.map(|value| value.0.to_string()),
            encode_state(item.child_requirement)?,
            encode_state(item.lifecycle)?,
            item.priority,
            item.assigned_to,
            item.deferred_until.map(|value| value.timestamp_millis()),
            item.revision,
            run_id.0.to_string(),
            item.source_snapshot_id.as_ref().map(ObjectId::as_str),
            item.created_at.timestamp_millis(),
            item.updated_at.timestamp_millis(),
            serde_json::to_vec(&item)?
        ],
    )?;
    initialize_new_work_catalog_projection(transaction, &item)?;
    super::root_state::initialize(transaction, &root_execution)?;
    let run = WorkRun {
        schema_version: SCHEMA_VERSION,
        run_id,
        root_execution_id: root_execution.root_execution_id,
        work_id,
        generation: 1,
        executor: None,
        state: WorkRunState::Open,
        revision: 1,
        last_checkpoint: None,
        completion_seal: None,
        created_at: request.created_at,
        updated_at: request.created_at,
    };
    transaction.execute(
        "INSERT INTO work_runs (
             run_id, root_execution_id, work_id, generation,
             executor_session_id, state, revision, claim_fence_head,
             last_checkpoint_id, completion_seal_id,
             created_at_ms, updated_at_ms, run_json
         ) VALUES (?1, ?2, ?3, ?4, NULL, ?5, ?6, 0, NULL, NULL, ?7, ?8, ?9)",
        params![
            run.run_id.0.to_string(),
            run.root_execution_id.0.to_string(),
            run.work_id.0.to_string(),
            run.generation,
            encode_state(run.state)?,
            run.revision,
            run.created_at.timestamp_millis(),
            run.updated_at.timestamp_millis(),
            serde_json::to_vec(&run)?
        ],
    )?;
    validation.check_graph(transaction, &item.project_id.0)?;
    let event = WorkEventDraft {
        schema_version: SCHEMA_VERSION,
        project_id: item.project_id.clone(),
        root_id: item.root_id,
        work_id,
        run_id: Some(run_id),
        revision: item.revision,
        work: item.clone(),
        run: Some(run.clone()),
        root_execution: Some(root_execution.clone()),
        claim: None,
        handoff_offer: None,
        blocker: None,
        transition: WorkTransition::Created {
            prerequisites: Vec::new(),
        },
        actor: request.actor.clone(),
        created_at: request.created_at,
    };
    let (event_id, positions) = append_work_event(transaction, &event)?;
    if let Some(position) = positions
        .iter()
        .find(|position| position.feed == crate::domain::FeedId::RunExecution(run_id))
    {
        super::completion::open_binding_obligations_on(
            transaction,
            &item,
            &run,
            &event_id,
            position,
            &[],
            request.created_at,
        )?;
    }
    append_initial_notes_on(transaction, &item, initial_notes, redactor)?;
    Ok(item)
}

impl SqliteStore {
    /// Creates a local root with an initial run and immutable event.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when input, source provenance, idempotency, or
    /// persistence validation fails.
    pub fn create_work<R: Redactor>(
        &mut self,
        request: &CreateWorkRequest,
        redactor: &R,
    ) -> Result<WorkItem, StoreError> {
        let initial_notes = crate::domain::normalize_initial_work_notes(&request.notes)
            .map_err(StoreError::InvalidWork)?;
        inspect_work_request(redactor, request, &request.actor)?;
        validate_root_input(request)?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(item) = replay_operation::<WorkItem>(
            &transaction,
            "create_work",
            &request.idempotency_key,
            request_object.key(),
        )? {
            transaction.commit()?;
            return Ok(item);
        }
        let item = create_root_on(&transaction, request, &initial_notes, redactor)?;
        persist_operation_result(
            &transaction,
            "create_work",
            &request.idempotency_key,
            request_object.key(),
            &item,
        )?;
        transaction.commit()?;
        Ok(item)
    }

    /// Atomically creates a bounded set of direct children and prerequisites.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the parent revision changed, a child or edge
    /// is invalid, the combined graph cycles, or the transaction cannot commit.
    pub fn decompose_work<R: Redactor>(
        &mut self,
        request: &DecomposeWorkRequest,
        redactor: &R,
    ) -> Result<WorkDecomposition, StoreError> {
        validate_child_count(request.children.len(), PlanningValidation::Immediate)?;
        let count = request.children.iter().fold(0_usize, |count, child| {
            count.saturating_add(child.notes.len())
        });
        crate::domain::validate_initial_work_note_count(count).map_err(StoreError::InvalidWork)?;
        let initial_notes = request
            .children
            .iter()
            .enumerate()
            .map(|(index, child)| {
                crate::domain::normalize_initial_work_notes(&child.notes).map_err(|reason| {
                    StoreError::InvalidWork(format!("child {}: {reason}", index + 1))
                })
            })
            .collect::<Result<Vec<_>, _>>()?;
        inspect_work_request(redactor, request, &request.actor)?;
        validate_child_inputs(&request.children)?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(decomposition) = replay_operation::<WorkDecomposition>(
            &transaction,
            "decompose_work",
            &request.idempotency_key,
            request_object.key(),
        )? {
            transaction.commit()?;
            return Ok(decomposition);
        }
        let decomposition = decompose_work_on(&transaction, request, &initial_notes, redactor)?;
        persist_operation_result(
            &transaction,
            "decompose_work",
            &request.idempotency_key,
            request_object.key(),
            &decomposition,
        )?;
        transaction.commit()?;
        Ok(decomposition)
    }

    /// Revises planning fields under optimistic work revision control.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the patch is invalid, the revision changed,
    /// work is not open, idempotency conflicts, or persistence fails.
    pub fn revise_work<R: Redactor>(
        &mut self,
        request: &ReviseWorkRequest,
        redactor: &R,
    ) -> Result<WorkItem, StoreError> {
        inspect_work_request(redactor, request, &request.actor)?;
        let _ = validate_revision_patch(&request.patch)?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(item) = replay_operation::<WorkItem>(
            &transaction,
            "revise_work",
            &request.idempotency_key,
            request_object.key(),
        )? {
            transaction.commit()?;
            return Ok(item);
        }
        let item = revise_work_on(&transaction, request, &NativePlanningEvent::Ordinary)?;
        persist_operation_result(
            &transaction,
            "revise_work",
            &request.idempotency_key,
            request_object.key(),
            &item,
        )?;
        transaction.commit()?;
        Ok(item)
    }

    /// Adds an explicit prerequisite and rejects cycles in the combined graph.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when work is absent or stale, projects differ,
    /// the combined graph cycles, or persistence fails.
    pub fn add_work_prerequisite<R: Redactor>(
        &mut self,
        request: &ChangeWorkPrerequisiteRequest,
        redactor: &R,
    ) -> Result<WorkItem, StoreError> {
        self.change_work_prerequisite(request, redactor, true)
    }

    /// Removes an explicit prerequisite under optimistic revision control.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when work is absent or stale, projects differ,
    /// idempotency conflicts, or persistence fails.
    pub fn remove_work_prerequisite<R: Redactor>(
        &mut self,
        request: &ChangeWorkPrerequisiteRequest,
        redactor: &R,
    ) -> Result<WorkItem, StoreError> {
        self.change_work_prerequisite(request, redactor, false)
    }

    fn change_work_prerequisite<R: Redactor>(
        &mut self,
        request: &ChangeWorkPrerequisiteRequest,
        redactor: &R,
        add: bool,
    ) -> Result<WorkItem, StoreError> {
        inspect_work_request(redactor, request, &request.actor)?;
        validate_prerequisite_input(request)?;
        let operation = if add {
            "add_work_prerequisite"
        } else {
            "remove_work_prerequisite"
        };
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(item) = replay_operation::<WorkItem>(
            &transaction,
            operation,
            &request.idempotency_key,
            request_object.key(),
        )? {
            transaction.commit()?;
            return Ok(item);
        }
        let item = change_work_prerequisite_on(&transaction, request, add)?;
        persist_operation_result(
            &transaction,
            operation,
            &request.idempotency_key,
            request_object.key(),
            &item,
        )?;
        transaction.commit()?;
        Ok(item)
    }

    /// Adds a typed blocker that participates in derived readiness.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when work is not open, blocker content is invalid,
    /// idempotency conflicts, or persistence fails.
    pub fn add_work_blocker<R: Redactor>(
        &mut self,
        request: &AddWorkBlockerRequest,
        redactor: &R,
    ) -> Result<WorkBlocker, StoreError> {
        inspect_work_request(redactor, request, &request.actor)?;
        let _detail = normalize_text(
            &request.detail,
            crate::storage::refusal_labels::BLOCKER_DETAIL,
        )?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(blocker) = replay_operation::<WorkBlocker>(
            &transaction,
            "add_work_blocker",
            &request.idempotency_key,
            request_object.key(),
        )? {
            transaction.commit()?;
            return Ok(blocker);
        }
        let blocker =
            add_work_blocker_on(&transaction, request, &NativePlanningEvent::Ordinary, None)?;
        persist_operation_result(
            &transaction,
            "add_work_blocker",
            &request.idempotency_key,
            request_object.key(),
            &blocker,
        )?;
        transaction.commit()?;
        Ok(blocker)
    }

    /// Resolves a blocker through an immutable event and revision bump.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the blocker is absent or belongs elsewhere,
    /// idempotency conflicts, or persistence fails.
    pub fn clear_work_blocker<R: Redactor>(
        &mut self,
        request: &ClearWorkBlockerRequest,
        redactor: &R,
    ) -> Result<WorkItem, StoreError> {
        self.clear_work_blocker_with_attempt(request, redactor, None)
    }

    /// The selected, keyless wrapper checks its admitted attempt under the
    /// same write lock as a new clear. Committed core replay precedes the guard.
    pub(crate) fn clear_work_blocker_with_attempt<R: Redactor>(
        &mut self,
        request: &ClearWorkBlockerRequest,
        redactor: &R,
        attempt: Option<&super::PendingWorkProtocolAttempt>,
    ) -> Result<WorkItem, StoreError> {
        inspect_work_request(redactor, request, &request.actor)?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(item) = replay_operation::<WorkItem>(
            &transaction,
            "clear_work_blocker",
            &request.idempotency_key,
            request_object.key(),
        )? {
            transaction.commit()?;
            return Ok(item);
        }
        if let Some(attempt) = attempt {
            attempt.require_pending_on(&transaction)?;
        }
        let item = clear_work_blocker_on(&transaction, request, &NativePlanningEvent::Ordinary)?;
        persist_operation_result(
            &transaction,
            "clear_work_blocker",
            &request.idempotency_key,
            request_object.key(),
            &item,
        )?;
        transaction.commit()?;
        Ok(item)
    }
}

/// Reuses ordinary decomposition admission inside a caller-owned plan transaction.
fn decompose_work_on<R: Redactor>(
    transaction: &Transaction<'_>,
    request: &DecomposeWorkRequest,
    initial_notes: &[Vec<String>],
    redactor: &R,
) -> Result<WorkDecomposition, StoreError> {
    decompose_work_with_validation_on(
        transaction,
        request,
        initial_notes,
        redactor,
        PlanningValidation::Immediate,
    )
}

#[allow(clippy::too_many_lines, reason = "one atomic decomposition transition")]
fn decompose_work_with_validation_on<R: Redactor>(
    transaction: &Transaction<'_>,
    request: &DecomposeWorkRequest,
    initial_notes: &[Vec<String>],
    redactor: &R,
    validation: PlanningValidation,
) -> Result<WorkDecomposition, StoreError> {
    validate_child_count(request.children.len(), validation)?;
    validate_child_inputs(&request.children)?;
    let mut parent = load_work_item(transaction, request.parent_id)?;
    super::task_sync::reconciliation::refuse_held_on(transaction, parent.work_id)?;
    assert_revision(&parent, request.expected_parent_revision)?;
    if parent.lifecycle != WorkLifecycle::Open {
        return Err(StoreError::WorkParentNotOpen {
            parent: parent.work_id,
            lifecycle: parent.lifecycle,
        });
    }
    let peer_proposal = peer_decomposition_admitted(transaction, &parent, request)?;
    if !peer_proposal {
        validate_planning_authority(
            transaction,
            &parent,
            &request.authority,
            &request.actor,
            request.created_at,
        )?;
    }
    let child_actor = if peer_proposal {
        crate::domain::peer_child_proposal_actor(request.actor.clone())
    } else {
        request.actor.clone()
    };
    validate_decomposition_budget(transaction, &parent, request.children.len(), validation)?;
    for edge in &request.prerequisites {
        if let WorkDependencyRef::Existing(work_id) = &edge.prerequisite {
            super::task_sync::reconciliation::refuse_held_on(transaction, *work_id)?;
        }
    }
    let restored_execution = if parent.active_run_id.is_none() {
        Some(ensure_restored_execution_state(
            transaction,
            &mut parent,
            request.created_at,
        )?)
    } else {
        None
    };
    let (mut root_execution, restored_run) = match restored_execution {
        Some((execution, run, _)) => (execution, Some(run)),
        None => (active_root_execution(transaction, parent.root_id)?, None),
    };
    let mut ids = HashMap::new();
    for child in &request.children {
        ids.insert(child.local_key.trim().to_owned(), WorkId::new());
    }
    let mut prerequisites: HashMap<String, Vec<WorkId>> = HashMap::new();
    for edge in &request.prerequisites {
        let work_key = edge.work_key.trim();
        if !ids.contains_key(work_key) {
            return Err(StoreError::InvalidWork(
                crate::storage::refusal_labels::unknown_child_edge(work_key),
            ));
        }
        let prerequisite = match &edge.prerequisite {
            WorkDependencyRef::Existing(work_id) => {
                if *work_id == parent.work_id {
                    return Err(StoreError::WorkDependencyCycle);
                }
                let existing = load_work_item(transaction, *work_id)?;
                if existing.project_id != parent.project_id {
                    return Err(StoreError::InvalidWork(
                        "prerequisite edges cannot cross projects".into(),
                    ));
                }
                if existing.lifecycle == WorkLifecycle::Completed {
                    return Err(StoreError::WorkPrerequisiteAlreadySatisfied(
                        existing.work_id,
                    ));
                }
                if existing.lifecycle != WorkLifecycle::Open {
                    return Err(StoreError::WorkNotOpen(existing.work_id));
                }
                if work_is_ancestor_of(transaction, existing.work_id, &parent)? {
                    return Err(StoreError::WorkDependencyCycle);
                }
                *work_id
            }
            WorkDependencyRef::Proposed(key) => ids.get(key.trim()).copied().ok_or_else(|| {
                StoreError::InvalidWork(format!(
                    "prerequisite edge references unknown proposed child {key:?}"
                ))
            })?,
        };
        if ids[work_key] == prerequisite {
            return Err(StoreError::WorkDependencyCycle);
        }
        prerequisites
            .entry(work_key.to_owned())
            .or_default()
            .push(prerequisite);
    }
    for values in prerequisites.values_mut() {
        values.sort_by_key(|value| value.0);
        values.dedup();
        validate_prerequisite_in_degree(values.len())?;
    }

    let mut children = Vec::with_capacity(request.children.len());
    let mut runs = HashMap::new();
    for draft in &request.children {
        let key = draft.local_key.trim();
        let work_id = ids[key];
        let run_id = WorkRunId::new();
        let mut labels = parent.labels.clone();
        labels.extend(draft.labels.clone());
        let (acceptance, acceptance_bindings) =
            normalize_acceptance(&draft.acceptance, &draft.acceptance_bindings)?;
        refuse_record_id_pins_on(transaction, &acceptance_bindings)?;
        let item = WorkItem {
            external_ref: crate::domain::normalize_external_reference(
                draft.external_ref.as_deref(),
            )
            .map_err(StoreError::InvalidWork)?,
            schema_version: SCHEMA_VERSION,
            project_id: parent.project_id.clone(),
            work_id,
            short_ref: short_ref(work_id),
            root_id: parent.root_id,
            parent_id: Some(parent.work_id),
            child_requirement: draft.child_requirement,
            title: normalize_text(&draft.title, crate::storage::refusal_labels::CHILD_TITLE)?,
            outcome: normalize_text(
                &draft.outcome,
                crate::storage::refusal_labels::CHILD_OUTCOME,
            )?,
            acceptance,
            acceptance_bindings,
            kind: draft.kind,
            priority: draft.priority,
            labels: normalize_strings(&labels),
            assigned_to: normalize_optional(draft.assigned_to.clone()),
            deferred_until: draft.deferred_until,
            origin: WorkOrigin::Local,
            source_snapshot_id: None,
            lifecycle: WorkLifecycle::Open,
            revision: 1,
            active_run_id: Some(run_id),
            restored: false,
            superseded_by: None,
            evaluation_mode: draft.evaluation_mode,
            created_by: child_actor.clone(),
            created_at: request.created_at,
            updated_at: request.created_at,
        };
        transaction.execute(
            "INSERT INTO work_items (
                 work_id, project_id, short_ref, root_id, parent_id,
                 child_requirement, lifecycle, priority, assigned_to,
                 deferred_until_ms, revision, active_run_id, source_snapshot_id,
                 created_at_ms, updated_at_ms, item_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, 'open', ?7, ?8, ?9, 1, ?10,
                       NULL, ?11, ?12, ?13)",
            params![
                item.work_id.0.to_string(),
                item.project_id.0,
                item.short_ref,
                item.root_id.0.to_string(),
                parent.work_id.0.to_string(),
                encode_state(item.child_requirement)?,
                item.priority,
                item.assigned_to,
                item.deferred_until.map(|value| value.timestamp_millis()),
                run_id.0.to_string(),
                item.created_at.timestamp_millis(),
                item.updated_at.timestamp_millis(),
                serde_json::to_vec(&item)?
            ],
        )?;
        initialize_new_work_catalog_projection(transaction, &item)?;
        let run = WorkRun {
            schema_version: SCHEMA_VERSION,
            run_id,
            root_execution_id: root_execution.root_execution_id,
            work_id,
            generation: 1,
            executor: None,
            state: WorkRunState::Open,
            revision: 1,
            last_checkpoint: None,
            completion_seal: None,
            created_at: request.created_at,
            updated_at: request.created_at,
        };
        transaction.execute(
            "INSERT INTO work_runs (
                 run_id, root_execution_id, work_id, generation,
                 executor_session_id, state, revision, claim_fence_head,
                 last_checkpoint_id, completion_seal_id,
                 created_at_ms, updated_at_ms, run_json
             ) VALUES (?1, ?2, ?3, 1, NULL, 'open', 1, 0, NULL, NULL, ?4, ?5, ?6)",
            params![
                run.run_id.0.to_string(),
                run.root_execution_id.0.to_string(),
                run.work_id.0.to_string(),
                run.created_at.timestamp_millis(),
                run.updated_at.timestamp_millis(),
                serde_json::to_vec(&run)?
            ],
        )?;
        runs.insert(key.to_owned(), run);
        children.push(item);
    }
    root_execution
        .run_ids
        .extend(runs.values().map(|run| run.run_id));
    root_execution
        .run_ids
        .sort_by(super::root_state::compare_runs);
    root_execution.run_ids.dedup();
    root_execution.revision += 1;
    root_execution.updated_at = request.created_at;
    persist_root_execution(transaction, &root_execution)?;
    for ((draft, item), notes) in request.children.iter().zip(&children).zip(initial_notes) {
        let item_prerequisites = prerequisites
            .get(draft.local_key.trim())
            .cloned()
            .unwrap_or_default();
        let run = &runs[draft.local_key.trim()];
        let run_id = run.run_id;
        let event = WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: Some(run_id),
            revision: item.revision,
            work: item.clone(),
            run: Some(run.clone()),
            root_execution: Some(root_execution.clone()),
            claim: None,
            handoff_offer: None,
            blocker: None,
            transition: WorkTransition::Created {
                prerequisites: item_prerequisites.clone(),
            },
            actor: child_actor.clone(),
            created_at: request.created_at,
        };
        let (event_id, positions) = append_work_event(transaction, &event)?;
        for prerequisite in item_prerequisites {
            transaction.execute(
                "INSERT INTO work_prerequisites (work_id, prerequisite_id, event_id)
                 VALUES (?1, ?2, ?3)",
                params![
                    item.work_id.0.to_string(),
                    prerequisite.0.to_string(),
                    event_id.as_str()
                ],
            )?;
        }
        if let Some(position) = positions
            .iter()
            .find(|position| position.feed == crate::domain::FeedId::RunExecution(run_id))
        {
            super::completion::open_binding_obligations_on(
                transaction,
                item,
                run,
                &event_id,
                position,
                &[],
                request.created_at,
            )?;
        }
        append_initial_notes_on(transaction, item, notes, redactor)?;
    }
    validation.check_graph(transaction, &parent.project_id.0)?;
    // A peer proposal creates optional child runs, not a mutation of the
    // holder's planning revision, run feed, or fenced authority.
    if !peer_proposal {
        parent.revision += 1;
        parent.updated_at = request.created_at;
        persist_work_item(transaction, &parent)?;
        let (claim_snapshot, rebased_run) =
            rebase_planning_claim(transaction, &parent, &request.authority, request.created_at)?;
        let event = WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: parent.project_id.clone(),
            root_id: parent.root_id,
            work_id: parent.work_id,
            run_id: parent.active_run_id,
            revision: parent.revision,
            work: parent.clone(),
            run: match rebased_run {
                Some(run) => Some(run),
                None => match restored_run {
                    Some(run) => Some(run),
                    None => active_run_snapshot(transaction, &parent)?,
                },
            },
            root_execution: Some(root_execution.clone()),
            claim: claim_snapshot,
            handoff_offer: None,
            blocker: None,
            transition: WorkTransition::Decomposed {
                children: children.iter().map(|child| child.work_id).collect(),
                authority: request.authority.clone(),
            },
            actor: request.actor.clone(),
            created_at: request.created_at,
        };
        append_work_event(transaction, &event)?;
    }
    Ok(WorkDecomposition { parent, children })
}

/// The `Revised` event of a planning revision already persisted in the
/// current transaction. `run` and `claim` are the run and claim the revision
/// leaves on the item; a revision never carries a root execution, handoff
/// offer or blocker.
pub(super) fn revised_event(
    item: &WorkItem,
    run: Option<WorkRun>,
    claim: Option<WorkClaim>,
    authority: WorkPlanningAuthority,
    actor: crate::domain::ActorContext,
    created_at: DateTime<Utc>,
) -> WorkEventDraft {
    WorkEventDraft {
        schema_version: SCHEMA_VERSION,
        project_id: item.project_id.clone(),
        root_id: item.root_id,
        work_id: item.work_id,
        run_id: item.active_run_id,
        revision: item.revision,
        work: item.clone(),
        run,
        root_execution: None,
        claim,
        handoff_offer: None,
        blocker: None,
        transition: WorkTransition::Revised { authority },
        actor,
        created_at,
    }
}

/// Applies an admitted scalar edit from a resolved task folder as an ordinary
/// planning revision under project authority, on the caller's transaction.
/// Origin does not change planning authority. The ordinary writer owns field
/// validation, canonical events, claim rebasing and binding obligations.
pub(super) fn apply_folder_scalar_revision_on(
    transaction: &Transaction<'_>,
    work_id: WorkId,
    change: &crate::task_sync::folder_plan::ScalarChange,
    actor: &crate::domain::ActorContext,
    at: DateTime<Utc>,
) -> Result<WorkItem, StoreError> {
    let item = load_work_item(transaction, work_id)?;
    revise_work_on(
        transaction,
        &ReviseWorkRequest {
            work_id,
            expected_revision: item.revision,
            patch: change.patch(),
            authority: WorkPlanningAuthority::Project,
            actor: actor.clone(),
            // The folder owner owns operation replay for the complete import.
            idempotency_key: String::new(),
            updated_at: at,
        },
        &NativePlanningEvent::Ordinary,
    )
}

/// Materializes an admitted folder removal without creating execution state.
/// The caller owns the fresh eligibility check in this transaction.
/// Ordinary disposal retains its separate runtime reconciliation contract.
pub(super) fn apply_folder_disposal_on(
    transaction: &Transaction<'_>,
    mut item: WorkItem,
    actor: &crate::domain::ActorContext,
    at: DateTime<Utc>,
) -> Result<WorkItem, StoreError> {
    // Bind and validate retained relations before persisting the snapshot.
    validated_current_work_relation_basis(transaction, item.work_id)?;
    item.lifecycle = WorkLifecycle::Cancelled;
    item.superseded_by = None;
    item.revision += 1;
    item.updated_at = at;
    persist_work_item(transaction, &item)?;
    append_work_event(
        transaction,
        &WorkEventDraft {
            schema_version: SCHEMA_VERSION,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: None,
            revision: item.revision,
            work: item.clone(),
            run: None,
            root_execution: None,
            claim: None,
            handoff_offer: None,
            blocker: None,
            transition: WorkTransition::Disposed {
                lifecycle: WorkLifecycle::Cancelled,
                replacement_id: None,
                reason: "Removed by resolved task folder import".into(),
            },
            actor: actor.clone(),
            created_at: at,
        },
    )?;
    Ok(item)
}

fn revise_work_on(
    transaction: &Transaction<'_>,
    request: &ReviseWorkRequest,
    mutation: &NativePlanningEvent<'_, '_, '_, '_>,
) -> Result<WorkItem, StoreError> {
    mutation.check(transaction, request.work_id)?;
    let _ = validate_revision_patch(&request.patch)?;
    let mut item = load_work_item(transaction, request.work_id)?;
    assert_revision(&item, request.expected_revision)?;
    mutation.authority(
        transaction,
        &item,
        &request.authority,
        &request.actor,
        request.updated_at,
    )?;
    if !matches!(
        item.lifecycle,
        WorkLifecycle::Open | WorkLifecycle::Proposed
    ) {
        return Err(StoreError::WorkNotOpen(item.work_id));
    }
    let reauthored = apply_revision_fields_on(transaction, &mut item, &request.patch)?;
    item.revision += 1;
    item.updated_at = request.updated_at;
    if matches!(mutation, NativePlanningEvent::Ordinary) {
        refuse_longer_text_search_cannot_read(transaction, &item)?;
    }
    persist_work_item(transaction, &item)?;
    let (claim_snapshot, rebased_run) =
        rebase_planning_claim(transaction, &item, &request.authority, request.updated_at)?;
    let run = match rebased_run {
        Some(run) => Some(run),
        None => active_run_snapshot(transaction, &item)?,
    };
    let event = revised_event(
        &item,
        run,
        claim_snapshot,
        request.authority.clone(),
        request.actor.clone(),
        request.updated_at,
    );
    let (event_id, positions) = mutation.append(transaction, &event)?;
    if let NativePlanningEvent::Successor { checked, .. } = mutation {
        super::completion::apply_successor_binding_obligations_on(
            checked,
            &item,
            &event,
            &event_id,
            &positions,
            &reauthored,
            &request.actor,
            request.updated_at,
        )?;
    } else if let Some(run_id) = item.active_run_id {
        // A binding the revision removed retires its obligation in the
        // revising actor's name; one it added opens from this revision.
        super::completion::waive_unbound_obligations_on(
            transaction,
            &item,
            run_id,
            &reauthored,
            &request.actor,
            request.updated_at,
        )?;
        if let Some(run) = event.run.as_ref()
            && let Some(position) = positions
                .iter()
                .find(|position| position.feed == crate::domain::FeedId::RunExecution(run_id))
        {
            super::completion::open_binding_obligations_on(
                transaction,
                &item,
                run,
                &event_id,
                position,
                &reauthored,
                request.updated_at,
            )?;
        }
    }
    Ok(item)
}

fn add_work_blocker_on(
    transaction: &Transaction<'_>,
    request: &AddWorkBlockerRequest,
    mutation: &NativePlanningEvent<'_, '_, '_, '_>,
    incoming: Option<(&WorkBlocker, &ObjectId)>,
) -> Result<WorkBlocker, StoreError> {
    mutation.check(transaction, request.work_id)?;
    let detail = normalize_text(
        &request.detail,
        crate::storage::refusal_labels::BLOCKER_DETAIL,
    )?;
    let mut item = load_work_item(transaction, request.work_id)?;
    require_work_item_relation_integrity(transaction, item.work_id)?;
    assert_revision(&item, request.expected_work_revision)?;
    mutation.authority(
        transaction,
        &item,
        &request.authority,
        &request.actor,
        request.blocked_at,
    )?;
    if item.lifecycle != WorkLifecycle::Open {
        return Err(StoreError::WorkNotOpen(item.work_id));
    }
    let (blocker, blocker_object) = if let Some((blocker, id)) = incoming {
        if blocker.work_id != item.work_id
            || blocker.kind != request.kind
            || blocker.detail != detail
            || blocker.blocker_id.trim().is_empty()
            || blocker.blocker_id.len() > 4096
        {
            return Err(StoreError::InvalidWorkProjection(
                "successor blocker differs from the admitted native transition".into(),
            ));
        }
        (blocker.clone(), CanonicalObject::identified(id, blocker)?)
    } else {
        let blocker = WorkBlocker {
            blocker_id: uuid::Uuid::now_v7().to_string(),
            work_id: item.work_id,
            kind: request.kind,
            detail,
            created_by: request.actor.clone(),
            created_at: request.blocked_at,
        };
        let blocker_object = CanonicalObject::mint(&blocker)?;
        (blocker, blocker_object)
    };
    SqliteStore::insert_object(transaction, "work_blocker", &blocker_object)?;
    item.revision += 1;
    item.updated_at = request.blocked_at;
    persist_work_item(transaction, &item)?;
    let (claim_snapshot, rebased_run) =
        rebase_planning_claim(transaction, &item, &request.authority, request.blocked_at)?;
    let event = WorkEventDraft {
        schema_version: SCHEMA_VERSION,
        project_id: item.project_id.clone(),
        root_id: item.root_id,
        work_id: item.work_id,
        run_id: item.active_run_id,
        revision: item.revision,
        work: item.clone(),
        run: match rebased_run {
            Some(run) => Some(run),
            None => active_run_snapshot(transaction, &item)?,
        },
        root_execution: None,
        claim: claim_snapshot,
        handoff_offer: None,
        blocker: Some(blocker.clone()),
        transition: WorkTransition::Blocked {
            blocker_id: blocker.blocker_id.clone(),
        },
        actor: request.actor.clone(),
        created_at: request.blocked_at,
    };
    let (event_id, _) = mutation.append(transaction, &event)?;
    let blocker_json = match mutation {
        NativePlanningEvent::Ordinary => blocker_json_search_can_read(&item, &blocker)?,
        NativePlanningEvent::Successor { .. } => serde_json::to_vec(&blocker)?,
    };
    transaction.execute(
        "INSERT INTO work_blockers (
             blocker_id, work_id, state, blocker_json, created_event_id
         ) VALUES (?1, ?2, 'active', ?3, ?4)",
        params![
            blocker.blocker_id,
            blocker.work_id.0.to_string(),
            blocker_json,
            event_id.as_str()
        ],
    )?;
    refresh_work_catalog_projection(transaction, &item)?;
    Ok(blocker)
}

fn clear_work_blocker_on(
    transaction: &Transaction<'_>,
    request: &ClearWorkBlockerRequest,
    mutation: &NativePlanningEvent<'_, '_, '_, '_>,
) -> Result<WorkItem, StoreError> {
    mutation.check(transaction, request.work_id)?;
    let mut item = load_work_item(transaction, request.work_id)?;
    require_work_item_relation_integrity(transaction, item.work_id)?;
    assert_revision(&item, request.expected_work_revision)?;
    mutation.authority(
        transaction,
        &item,
        &request.authority,
        &request.actor,
        request.cleared_at,
    )?;
    if item.lifecycle != WorkLifecycle::Open {
        return Err(StoreError::WorkNotOpen(item.work_id));
    }
    let blocker_work: Option<String> = transaction
        .query_row(
            "SELECT work_id FROM work_blockers
             WHERE blocker_id = ?1 AND state = 'active'",
            [&request.blocker_id],
            |row| row.get(0),
        )
        .optional()?;
    if blocker_work.as_deref() != Some(&item.work_id.0.to_string()) {
        return Err(StoreError::InvalidWork(
            crate::storage::UNKNOWN_BLOCKER_REFUSAL.into(),
        ));
    }
    item.revision += 1;
    item.updated_at = request.cleared_at;
    persist_work_item(transaction, &item)?;
    let (claim_snapshot, rebased_run) =
        rebase_planning_claim(transaction, &item, &request.authority, request.cleared_at)?;
    let event = WorkEventDraft {
        schema_version: SCHEMA_VERSION,
        project_id: item.project_id.clone(),
        root_id: item.root_id,
        work_id: item.work_id,
        run_id: item.active_run_id,
        revision: item.revision,
        work: item.clone(),
        run: match rebased_run {
            Some(run) => Some(run),
            None => active_run_snapshot(transaction, &item)?,
        },
        root_execution: None,
        claim: claim_snapshot,
        handoff_offer: None,
        blocker: None,
        transition: WorkTransition::Unblocked {
            blocker_id: request.blocker_id.clone(),
        },
        actor: request.actor.clone(),
        created_at: request.cleared_at,
    };
    let (event_id, _) = mutation.append(transaction, &event)?;
    transaction.execute(
        "UPDATE work_blockers SET state = 'cleared', cleared_event_id = ?2
         WHERE blocker_id = ?1",
        params![request.blocker_id, event_id.as_str()],
    )?;
    refresh_work_catalog_projection(transaction, &item)?;
    Ok(item)
}

/// Normalize the proposed planning fields without writing. Retained successor
/// construction and the mutating revision share this exact acceptance logic.
fn apply_revision_fields_on(
    transaction: &Connection,
    item: &mut WorkItem,
    patch: &crate::WorkRevisionPatch,
) -> Result<Vec<usize>, StoreError> {
    let (add_labels, remove_labels) = validate_revision_patch(patch)?;
    if let Some(title) = patch.title.as_deref() {
        item.title = normalize_text(title, crate::storage::refusal_labels::TITLE)?;
    }
    if let Some(external) = patch.external_ref.as_deref() {
        item.external_ref = crate::domain::normalize_external_reference(Some(external))
            .map_err(StoreError::InvalidWork)?;
    }
    if patch.clear_external {
        item.external_ref = None;
    }
    if let Some(outcome) = patch.outcome.as_deref() {
        item.outcome = normalize_text(outcome, crate::storage::refusal_labels::OUTCOME)?;
    }
    let authored_before = (item.acceptance.clone(), item.acceptance_bindings.clone());
    match (
        patch.acceptance.as_ref(),
        patch.acceptance_bindings.as_ref(),
    ) {
        // Bindings authored with a new list name its positions as typed.
        (Some(acceptance), Some(bindings)) => {
            let (acceptance, bindings) = normalize_acceptance(acceptance, bindings)?;
            item.acceptance = acceptance;
            item.acceptance_bindings = bindings;
        }
        // Positions name the list that was replaced; nothing is carried
        // over to a list they were never authored against.
        (Some(acceptance), None) => {
            item.acceptance = normalize_acceptance_criteria(acceptance);
            item.acceptance_bindings.clear();
        }
        // Bindings alone name the stored list, as `show` numbers it.
        (None, Some(bindings)) => {
            item.acceptance_bindings =
                crate::domain::normalize_acceptance_bindings(item.acceptance.len(), bindings)
                    .map_err(StoreError::InvalidWork)?;
        }
        (None, None) => {}
    }
    if patch.acceptance_bindings.is_some() {
        refuse_record_id_pins_on(transaction, &item.acceptance_bindings)?;
    }
    // A binding the preceding revision did not carry unchanged (same
    // position, same requirement, same sentence) is authored by this
    // revision and owes its verification from here: an obligation an
    // earlier revision resolved answered an earlier authoring, whether the
    // sentence was rewritten or the binding was dropped and added again.
    let reauthored = item
        .acceptance_bindings
        .iter()
        .filter(|binding| {
            let carried = authored_before.1.iter().any(|before| {
                before.criterion == binding.criterion && before.requirement == binding.requirement
            }) && binding
                .criterion
                .checked_sub(1)
                .is_some_and(|index| authored_before.0.get(index) == item.acceptance.get(index));
            !carried
        })
        .map(|binding| binding.criterion)
        .collect::<Vec<_>>();
    if let Some(kind) = patch.kind {
        item.kind = kind;
    }
    if let Some(priority) = patch.priority {
        item.priority = priority;
    }
    if let Some(labels) = patch.labels.as_ref() {
        item.labels = normalize_strings(labels);
    } else if !add_labels.is_empty() || !remove_labels.is_empty() {
        let remove_label_keys = remove_labels
            .iter()
            .map(|label| normalize_work_catalog_key(label))
            .collect::<HashSet<_>>();
        item.labels
            .retain(|current| !remove_label_keys.contains(&normalize_work_catalog_key(current)));
        let mut current_label_keys = item
            .labels
            .iter()
            .map(|label| normalize_work_catalog_key(label))
            .collect::<HashSet<_>>();
        for label in add_labels {
            if current_label_keys.insert(normalize_work_catalog_key(&label)) {
                item.labels.push(label);
            }
        }
        item.labels = normalize_strings(&item.labels);
    }
    if patch.clear_assignment {
        item.assigned_to = None;
    } else if patch.assigned_to.is_some() {
        item.assigned_to = normalize_optional(patch.assigned_to.clone());
    }
    if patch.clear_deferral {
        item.deferred_until = None;
    } else if let Some(deferred_until) = patch.deferred_until {
        item.deferred_until = Some(deferred_until);
    }
    if patch.clear_evaluation_mode {
        item.evaluation_mode = None;
    } else if let Some(mode) = patch.evaluation_mode {
        item.evaluation_mode = Some(mode);
    }
    Ok(reauthored)
}
