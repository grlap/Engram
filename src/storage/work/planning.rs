use std::collections::{HashMap, HashSet};

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, params};
use serde::Serialize;
use unicode_casefold::UnicodeCaseFold;
use unicode_normalization::UnicodeNormalization;

use super::super::{SqliteStore, StoreError};
use super::completion::{
    ancestors_admit_execution, run_uses_active_root_execution, work_is_ancestor_of,
};
use super::execution::ensure_restored_execution_state;
use super::feeds::{
    append_work_event, expire_handoff_offers, inspect_work_request, load_typed_work_object,
    replay_operation, request_object, validate_work_source_snapshot,
};
use super::integrity::combined_graph_is_acyclic;
#[cfg(test)]
use super::query::latest_canonical_work_event_for_item;
use super::query::{
    active_root_execution, active_run_snapshot, canonical_work_events_for_item,
    load_active_blocker_projections, load_prerequisite_projection_ids, load_work_claim_optional,
    load_work_item, load_work_run,
};
use super::{
    MAX_CHILDREN_PER_DECOMPOSITION, MAX_OPEN_WORK_DESCENDANTS, MAX_WORK_DEPTH,
    MAX_WORK_TTL_SECONDS, WorkEventDraft, WorkRelationBasis, WorkRelationBlockerBasis,
};
use crate::{
    CanonicalObject, ObjectHash,
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
        inspect_work_request(redactor, request, &request.actor)?;
        if request.parent_id.is_some() {
            return Err(StoreError::InvalidWork(
                "direct child creation is not allowed; use decompose_work with the parent revision"
                    .into(),
            ));
        }
        if !(0..=4).contains(&request.priority) {
            return Err(StoreError::InvalidWork(
                "priority must be an integer from 0 through 4".into(),
            ));
        }
        match (request.origin, request.source_snapshot_id.as_ref()) {
            (WorkOrigin::Local, None) | (WorkOrigin::Imported, Some(_)) => {}
            (WorkOrigin::Local, Some(_)) => {
                return Err(StoreError::InvalidWork(
                    "local work cannot carry an imported source snapshot".into(),
                ));
            }
            (WorkOrigin::Imported, None) => {
                return Err(StoreError::InvalidWork(
                    "imported work requires a source snapshot".into(),
                ));
            }
        }
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(item) = replay_operation::<WorkItem>(
            &transaction,
            "create_work",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(item);
        }
        if let Some(snapshot) = request.source_snapshot_id.as_ref() {
            let source = load_typed_work_object::<WorkSourceSnapshot>(
                &transaction,
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

        let title = normalize_text(&request.title, "title")?;
        let outcome = normalize_text(&request.outcome, "outcome")?;
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
            acceptance: normalize_strings(&request.acceptance),
            kind: request.kind,
            priority: request.priority,
            labels: normalize_strings(&request.labels),
            assigned_to: normalize_optional(request.assigned_to.clone()),
            deferred_until: request.deferred_until,
            origin: request.origin,
            source_snapshot_id: request.source_snapshot_id.clone(),
            lifecycle: WorkLifecycle::Open,
            revision: 1,
            active_run_id: Some(run_id),
            restored: false,
            superseded_by: None,
            created_by: request.actor.clone(),
            created_at: request.created_at,
            updated_at: request.created_at,
        };
        transaction.execute(
            "INSERT INTO work_items (
                 work_id, project_id, short_ref, root_id, parent_id,
                 child_requirement, lifecycle, priority, assigned_to,
                 deferred_until_ms, revision, active_run_id, source_snapshot_hash,
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
                item.source_snapshot_id.as_ref().map(ObjectHash::as_str),
                item.created_at.timestamp_millis(),
                item.updated_at.timestamp_millis(),
                serde_json::to_vec(&item)?
            ],
        )?;
        refresh_work_catalog_projection(&transaction, &item)?;
        transaction.execute(
            "INSERT INTO work_root_executions (
                 root_execution_id, project_id, root_id, generation, state,
                 revision, created_at_ms, updated_at_ms, execution_json
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                root_execution.root_execution_id.0.to_string(),
                root_execution.project_id.0,
                root_execution.root_id.0.to_string(),
                root_execution.generation,
                encode_state(root_execution.state)?,
                root_execution.revision,
                root_execution.created_at.timestamp_millis(),
                root_execution.updated_at.timestamp_millis(),
                serde_json::to_vec(&root_execution)?
            ],
        )?;
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
                 last_checkpoint_hash, completion_seal_hash,
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
        if !combined_graph_is_acyclic(&transaction, &item.project_id.0)? {
            return Err(StoreError::WorkDependencyCycle);
        }
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
        append_work_event(&transaction, &event)?;
        persist_operation_result(
            &transaction,
            "create_work",
            &request.idempotency_key,
            request_object.hash(),
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
        inspect_work_request(redactor, request, &request.actor)?;
        if request.children.is_empty() || request.children.len() > MAX_CHILDREN_PER_DECOMPOSITION {
            return Err(StoreError::InvalidWork(format!(
                "decomposition must contain from 1 through {MAX_CHILDREN_PER_DECOMPOSITION} children"
            )));
        }
        let mut keys = HashSet::new();
        for child in &request.children {
            let key = normalize_text(&child.local_key, "child local key")?;
            if !keys.insert(key) {
                return Err(StoreError::InvalidWork(
                    "child local keys must be unique".into(),
                ));
            }
            if !(0..=4).contains(&child.priority) {
                return Err(StoreError::InvalidWork(
                    "child priority must be an integer from 0 through 4".into(),
                ));
            }
        }
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(decomposition) = replay_operation::<WorkDecomposition>(
            &transaction,
            "decompose_work",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(decomposition);
        }
        let mut parent = load_work_item(&transaction, request.parent_id)?;
        assert_revision(&parent, request.expected_parent_revision)?;
        validate_planning_authority(
            &transaction,
            &parent,
            &request.authority,
            &request.actor,
            request.created_at,
        )?;
        validate_decomposition_budget(&transaction, &parent, request.children.len())?;
        if parent.lifecycle != WorkLifecycle::Open {
            return Err(StoreError::WorkNotOpen(parent.work_id));
        }
        let restored_execution = if parent.active_run_id.is_none() {
            Some(ensure_restored_execution_state(
                &transaction,
                &mut parent,
                request.created_at,
            )?)
        } else {
            None
        };
        let (mut root_execution, restored_run) = match restored_execution {
            Some((execution, run, _)) => (execution, Some(run)),
            None => (active_root_execution(&transaction, parent.root_id)?, None),
        };
        let mut ids = HashMap::new();
        for child in &request.children {
            ids.insert(child.local_key.trim().to_owned(), WorkId::new());
        }
        let mut prerequisites: HashMap<String, Vec<WorkId>> = HashMap::new();
        for edge in &request.prerequisites {
            let work_key = edge.work_key.trim();
            if !ids.contains_key(work_key) {
                return Err(StoreError::InvalidWork(format!(
                    "prerequisite edge references unknown child {work_key:?}"
                )));
            }
            let prerequisite = match &edge.prerequisite {
                WorkDependencyRef::Existing(work_id) => {
                    if *work_id == parent.work_id {
                        return Err(StoreError::WorkDependencyCycle);
                    }
                    let existing = load_work_item(&transaction, *work_id)?;
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
                    if work_is_ancestor_of(&transaction, existing.work_id, &parent)? {
                        return Err(StoreError::WorkDependencyCycle);
                    }
                    *work_id
                }
                WorkDependencyRef::Proposed(key) => {
                    ids.get(key.trim()).copied().ok_or_else(|| {
                        StoreError::InvalidWork(format!(
                            "prerequisite edge references unknown proposed child {key:?}"
                        ))
                    })?
                }
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
        }

        let mut children = Vec::with_capacity(request.children.len());
        let mut runs = HashMap::new();
        for draft in &request.children {
            let key = draft.local_key.trim();
            let work_id = ids[key];
            let run_id = WorkRunId::new();
            let mut labels = parent.labels.clone();
            labels.extend(draft.labels.clone());
            let item = WorkItem {
                schema_version: SCHEMA_VERSION,
                project_id: parent.project_id.clone(),
                work_id,
                short_ref: short_ref(work_id),
                root_id: parent.root_id,
                parent_id: Some(parent.work_id),
                child_requirement: draft.child_requirement,
                title: normalize_text(&draft.title, "child title")?,
                outcome: normalize_text(&draft.outcome, "child outcome")?,
                acceptance: normalize_strings(&draft.acceptance),
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
                created_by: request.actor.clone(),
                created_at: request.created_at,
                updated_at: request.created_at,
            };
            transaction.execute(
                "INSERT INTO work_items (
                     work_id, project_id, short_ref, root_id, parent_id,
                     child_requirement, lifecycle, priority, assigned_to,
                     deferred_until_ms, revision, active_run_id, source_snapshot_hash,
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
            refresh_work_catalog_projection(&transaction, &item)?;
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
                     last_checkpoint_hash, completion_seal_hash,
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
        root_execution.run_ids.sort_by_key(|run_id| run_id.0);
        root_execution.run_ids.dedup();
        root_execution.revision += 1;
        root_execution.updated_at = request.created_at;
        persist_root_execution(&transaction, &root_execution)?;
        for (draft, item) in request.children.iter().zip(&children) {
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
                actor: request.actor.clone(),
                created_at: request.created_at,
            };
            let (event_hash, _) = append_work_event(&transaction, &event)?;
            for prerequisite in item_prerequisites {
                transaction.execute(
                    "INSERT INTO work_prerequisites (work_id, prerequisite_id, event_hash)
                     VALUES (?1, ?2, ?3)",
                    params![
                        item.work_id.0.to_string(),
                        prerequisite.0.to_string(),
                        event_hash.as_str()
                    ],
                )?;
            }
        }
        if !combined_graph_is_acyclic(&transaction, &parent.project_id.0)? {
            return Err(StoreError::WorkDependencyCycle);
        }
        parent.revision += 1;
        parent.updated_at = request.created_at;
        persist_work_item(&transaction, &parent)?;
        let (claim_snapshot, rebased_run) = rebase_planning_claim(
            &transaction,
            &parent,
            &request.authority,
            request.created_at,
        )?;
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
                    None => active_run_snapshot(&transaction, &parent)?,
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
        append_work_event(&transaction, &event)?;
        let decomposition = WorkDecomposition { parent, children };
        persist_operation_result(
            &transaction,
            "decompose_work",
            &request.idempotency_key,
            request_object.hash(),
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
        if request.patch.clear_assignment && request.patch.assigned_to.is_some() {
            return Err(StoreError::InvalidWork(
                "assignment cannot be set and cleared in one revision".into(),
            ));
        }
        if request.patch.clear_deferral && request.patch.deferred_until.is_some() {
            return Err(StoreError::InvalidWork(
                "deferral cannot be set and cleared in one revision".into(),
            ));
        }
        if let Some(acceptance) = &request.patch.acceptance
            && (acceptance.is_empty() || acceptance.iter().any(|value| value.trim().is_empty()))
        {
            return Err(StoreError::InvalidWork(
                "acceptance replacement needs at least one nonblank criterion; omit acceptance to leave it unchanged".into(),
            ));
        }
        if request.patch.labels.is_some()
            && (!request.patch.add_labels.is_empty() || !request.patch.remove_labels.is_empty())
        {
            return Err(StoreError::InvalidWork(
                "labels cannot be replaced and incrementally changed in one revision".into(),
            ));
        }
        let add_labels = normalize_strings(&request.patch.add_labels);
        let remove_labels = normalize_strings(&request.patch.remove_labels);
        let add_label_keys = add_labels
            .iter()
            .map(|label| normalize_work_catalog_key(label))
            .collect::<HashSet<_>>();
        let remove_label_keys = remove_labels
            .iter()
            .map(|label| normalize_work_catalog_key(label))
            .collect::<HashSet<_>>();
        if !add_label_keys.is_disjoint(&remove_label_keys) {
            return Err(StoreError::InvalidWork(
                "the same label cannot be added and removed in one revision".into(),
            ));
        }
        if request
            .patch
            .priority
            .is_some_and(|priority| !(0..=4).contains(&priority))
        {
            return Err(StoreError::InvalidWork(
                "priority must be an integer from 0 through 4".into(),
            ));
        }
        let changed = request.patch.title.is_some()
            || request.patch.outcome.is_some()
            || request.patch.acceptance.is_some()
            || request.patch.kind.is_some()
            || request.patch.priority.is_some()
            || request.patch.labels.is_some()
            || !add_labels.is_empty()
            || !remove_labels.is_empty()
            || request.patch.assigned_to.is_some()
            || request.patch.clear_assignment
            || request.patch.deferred_until.is_some()
            || request.patch.clear_deferral;
        if !changed {
            return Err(StoreError::InvalidWork("revision patch is empty".into()));
        }
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(item) = replay_operation::<WorkItem>(
            &transaction,
            "revise_work",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(item);
        }
        let mut item = load_work_item(&transaction, request.work_id)?;
        assert_revision(&item, request.expected_revision)?;
        validate_planning_authority(
            &transaction,
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
        if let Some(title) = request.patch.title.as_deref() {
            item.title = normalize_text(title, "title")?;
        }
        if let Some(outcome) = request.patch.outcome.as_deref() {
            item.outcome = normalize_text(outcome, "outcome")?;
        }
        if let Some(acceptance) = request.patch.acceptance.as_ref() {
            item.acceptance = normalize_strings(acceptance);
        }
        if let Some(kind) = request.patch.kind {
            item.kind = kind;
        }
        if let Some(priority) = request.patch.priority {
            item.priority = priority;
        }
        if let Some(labels) = request.patch.labels.as_ref() {
            item.labels = normalize_strings(labels);
        } else if !add_labels.is_empty() || !remove_labels.is_empty() {
            item.labels.retain(|current| {
                !remove_label_keys.contains(&normalize_work_catalog_key(current))
            });
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
        if request.patch.clear_assignment {
            item.assigned_to = None;
        } else if request.patch.assigned_to.is_some() {
            item.assigned_to = normalize_optional(request.patch.assigned_to.clone());
        }
        if request.patch.clear_deferral {
            item.deferred_until = None;
        } else if let Some(deferred_until) = request.patch.deferred_until {
            item.deferred_until = Some(deferred_until);
        }
        item.revision += 1;
        item.updated_at = request.updated_at;
        persist_work_item(&transaction, &item)?;
        let (claim_snapshot, rebased_run) =
            rebase_planning_claim(&transaction, &item, &request.authority, request.updated_at)?;
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
                None => active_run_snapshot(&transaction, &item)?,
            },
            root_execution: None,
            claim: claim_snapshot,
            handoff_offer: None,
            blocker: None,
            transition: WorkTransition::Revised {
                authority: request.authority.clone(),
            },
            actor: request.actor.clone(),
            created_at: request.updated_at,
        };
        append_work_event(&transaction, &event)?;
        persist_operation_result(
            &transaction,
            "revise_work",
            &request.idempotency_key,
            request_object.hash(),
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
        if request.work_id == request.prerequisite_id {
            return Err(StoreError::WorkDependencyCycle);
        }
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
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(item);
        }
        let mut item = load_work_item(&transaction, request.work_id)?;
        let prerequisite = load_work_item(&transaction, request.prerequisite_id)?;
        require_work_item_relation_integrity(&transaction, item.work_id)?;
        assert_revision(&item, request.expected_revision)?;
        validate_planning_authority(
            &transaction,
            &item,
            &request.authority,
            &request.actor,
            request.changed_at,
        )?;
        if item.project_id != prerequisite.project_id {
            return Err(StoreError::InvalidWork(
                "prerequisite edges cannot cross projects".into(),
            ));
        }
        if item.lifecycle != WorkLifecycle::Open {
            return Err(StoreError::WorkNotOpen(item.work_id));
        }
        if add {
            if prerequisite.lifecycle == WorkLifecycle::Completed {
                return Err(StoreError::WorkPrerequisiteAlreadySatisfied(
                    prerequisite.work_id,
                ));
            }
            if prerequisite.lifecycle != WorkLifecycle::Open {
                return Err(StoreError::WorkNotOpen(prerequisite.work_id));
            }
            if work_is_ancestor_of(&transaction, prerequisite.work_id, &item)? {
                return Err(StoreError::WorkDependencyCycle);
            }
        }
        let exists: Option<String> = transaction
            .query_row(
                "SELECT event_hash FROM work_prerequisites
                 WHERE work_id = ?1 AND prerequisite_id = ?2",
                params![
                    item.work_id.0.to_string(),
                    prerequisite.work_id.0.to_string()
                ],
                |row| row.get(0),
            )
            .optional()?;
        if add == exists.is_some() {
            persist_operation_result(
                &transaction,
                operation,
                &request.idempotency_key,
                request_object.hash(),
                &item,
            )?;
            transaction.commit()?;
            return Ok(item);
        }
        item.revision += 1;
        item.updated_at = request.changed_at;
        let (claim_snapshot, rebased_run) =
            rebase_planning_claim(&transaction, &item, &request.authority, request.changed_at)?;
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
                None => active_run_snapshot(&transaction, &item)?,
            },
            root_execution: None,
            claim: claim_snapshot,
            handoff_offer: None,
            blocker: None,
            transition: if add {
                WorkTransition::PrerequisiteAdded {
                    prerequisite_id: prerequisite.work_id,
                    authority: request.authority.clone(),
                }
            } else {
                WorkTransition::PrerequisiteRemoved {
                    prerequisite_id: prerequisite.work_id,
                    authority: request.authority.clone(),
                }
            },
            actor: request.actor.clone(),
            created_at: request.changed_at,
        };
        let (event_hash, _) = append_work_event(&transaction, &event)?;
        if add {
            transaction.execute(
                "INSERT INTO work_prerequisites (work_id, prerequisite_id, event_hash)
                 VALUES (?1, ?2, ?3)",
                params![
                    item.work_id.0.to_string(),
                    prerequisite.work_id.0.to_string(),
                    event_hash.as_str()
                ],
            )?;
            if !combined_graph_is_acyclic(&transaction, &item.project_id.0)? {
                return Err(StoreError::WorkDependencyCycle);
            }
        } else {
            transaction.execute(
                "DELETE FROM work_prerequisites
                 WHERE work_id = ?1 AND prerequisite_id = ?2",
                params![
                    item.work_id.0.to_string(),
                    prerequisite.work_id.0.to_string()
                ],
            )?;
        }
        persist_work_item(&transaction, &item)?;
        persist_operation_result(
            &transaction,
            operation,
            &request.idempotency_key,
            request_object.hash(),
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
        let detail = normalize_text(&request.detail, "blocker detail")?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(blocker) = replay_operation::<WorkBlocker>(
            &transaction,
            "add_work_blocker",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(blocker);
        }
        let mut item = load_work_item(&transaction, request.work_id)?;
        require_work_item_relation_integrity(&transaction, item.work_id)?;
        assert_revision(&item, request.expected_work_revision)?;
        validate_planning_authority(
            &transaction,
            &item,
            &request.authority,
            &request.actor,
            request.blocked_at,
        )?;
        if item.lifecycle != WorkLifecycle::Open {
            return Err(StoreError::WorkNotOpen(item.work_id));
        }
        let blocker = WorkBlocker {
            blocker_id: uuid::Uuid::now_v7().to_string(),
            work_id: item.work_id,
            kind: request.kind,
            detail,
            created_by: request.actor.clone(),
            created_at: request.blocked_at,
        };
        let blocker_object = CanonicalObject::freeze(&blocker)?;
        SqliteStore::insert_object(&transaction, "work_blocker", &blocker_object)?;
        item.revision += 1;
        item.updated_at = request.blocked_at;
        persist_work_item(&transaction, &item)?;
        let (claim_snapshot, rebased_run) =
            rebase_planning_claim(&transaction, &item, &request.authority, request.blocked_at)?;
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
                None => active_run_snapshot(&transaction, &item)?,
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
        let (event_hash, _) = append_work_event(&transaction, &event)?;
        transaction.execute(
            "INSERT INTO work_blockers (
                 blocker_id, work_id, state, blocker_json, created_event_hash
             ) VALUES (?1, ?2, 'active', ?3, ?4)",
            params![
                blocker.blocker_id,
                blocker.work_id.0.to_string(),
                serde_json::to_vec(&blocker)?,
                event_hash.as_str()
            ],
        )?;
        refresh_work_catalog_projection(&transaction, &item)?;
        persist_operation_result(
            &transaction,
            "add_work_blocker",
            &request.idempotency_key,
            request_object.hash(),
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
        inspect_work_request(redactor, request, &request.actor)?;
        let request_object = request_object(request)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(item) = replay_operation::<WorkItem>(
            &transaction,
            "clear_work_blocker",
            &request.idempotency_key,
            request_object.hash(),
        )? {
            transaction.commit()?;
            return Ok(item);
        }
        let mut item = load_work_item(&transaction, request.work_id)?;
        require_work_item_relation_integrity(&transaction, item.work_id)?;
        assert_revision(&item, request.expected_work_revision)?;
        validate_planning_authority(
            &transaction,
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
                "unknown blocker id for this work item".into(),
            ));
        }
        item.revision += 1;
        item.updated_at = request.cleared_at;
        persist_work_item(&transaction, &item)?;
        let (claim_snapshot, rebased_run) =
            rebase_planning_claim(&transaction, &item, &request.authority, request.cleared_at)?;
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
                None => active_run_snapshot(&transaction, &item)?,
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
        let (event_hash, _) = append_work_event(&transaction, &event)?;
        transaction.execute(
            "UPDATE work_blockers SET state = 'cleared', cleared_event_hash = ?2
             WHERE blocker_id = ?1",
            params![request.blocker_id, event_hash.as_str()],
        )?;
        refresh_work_catalog_projection(&transaction, &item)?;
        persist_operation_result(
            &transaction,
            "clear_work_blocker",
            &request.idempotency_key,
            request_object.hash(),
            &item,
        )?;
        transaction.commit()?;
        Ok(item)
    }
}

pub(super) fn projected_work_relation_basis(
    connection: &Connection,
    work_id: WorkId,
) -> Result<WorkRelationBasis, StoreError> {
    let mut prerequisite_ids = load_prerequisite_projection_ids(connection, work_id)?;
    prerequisite_ids.sort_by_key(|prerequisite_id| prerequisite_id.0);
    let mut active_blockers = load_active_blocker_projections(connection, work_id)?
        .into_iter()
        .map(|blocker| {
            let blocker_id = blocker.blocker_id.clone();
            let blocker_hash = CanonicalObject::freeze(&blocker)?.hash().clone();
            Ok(WorkRelationBlockerBasis {
                blocker_id,
                blocker_hash,
            })
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    active_blockers.sort_by(|left, right| left.blocker_id.cmp(&right.blocker_id));
    Ok(WorkRelationBasis {
        schema_version: SCHEMA_VERSION,
        prerequisite_ids,
        active_blockers,
    })
}

pub(super) fn work_relation_fingerprint(
    basis: &WorkRelationBasis,
) -> Result<ObjectHash, StoreError> {
    Ok(CanonicalObject::freeze(basis)?.hash().clone())
}

pub(super) fn validated_current_work_relation_basis(
    connection: &Connection,
    work_id: WorkId,
) -> Result<WorkRelationBasis, StoreError> {
    let basis = projected_work_relation_basis(connection, work_id)?;
    let actual = work_relation_fingerprint(&basis)?;
    let expected = if let Some(latest) =
        super::query::latest_canonical_work_event_for_item_optional(connection, work_id)?
    {
        latest.relation_fingerprint
    } else {
        let (_, restored) =
            super::query::latest_restored_record(connection, work_id)?.ok_or_else(|| {
                StoreError::InvalidWorkProjection(format!(
                    "relations for {work_id:?} have no canonical history anchor"
                ))
            })?;
        let mut restored_basis = WorkRelationBasis {
            schema_version: SCHEMA_VERSION,
            prerequisite_ids: restored.relations.prerequisites,
            active_blockers: restored
                .relations
                .blockers
                .into_iter()
                .map(|blocker| {
                    let blocker = WorkBlocker {
                        blocker_id: blocker.blocker_id,
                        work_id: blocker.work_id,
                        kind: blocker.kind,
                        detail: blocker.detail,
                        created_by: blocker.created_by,
                        created_at: blocker.created_at,
                    };
                    Ok(WorkRelationBlockerBasis {
                        blocker_id: blocker.blocker_id.clone(),
                        blocker_hash: CanonicalObject::freeze(&blocker)?.hash().clone(),
                    })
                })
                .collect::<Result<Vec<_>, StoreError>>()?,
        };
        restored_basis.prerequisite_ids.sort_by_key(|id| id.0);
        restored_basis
            .active_blockers
            .sort_by(|left, right| left.blocker_id.cmp(&right.blocker_id));
        work_relation_fingerprint(&restored_basis)?
    };
    if actual != expected {
        return Err(StoreError::InvalidWorkProjection(format!(
            "relations for {work_id:?} differ from the latest canonical fingerprint"
        )));
    }
    Ok(basis)
}

pub(super) fn apply_work_relation_transition(
    basis: &mut WorkRelationBasis,
    transition: &WorkTransition,
    blocker: Option<&WorkBlocker>,
) -> Result<(), StoreError> {
    match transition {
        WorkTransition::Created { prerequisites, .. } => {
            basis.prerequisite_ids.clone_from(prerequisites);
            basis
                .prerequisite_ids
                .sort_by_key(|prerequisite_id| prerequisite_id.0);
            basis.prerequisite_ids.dedup();
        }
        WorkTransition::PrerequisiteAdded {
            prerequisite_id, ..
        } => {
            if basis.prerequisite_ids.contains(prerequisite_id) {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "prerequisite {prerequisite_id:?} is already active"
                )));
            }
            basis.prerequisite_ids.push(*prerequisite_id);
            basis
                .prerequisite_ids
                .sort_by_key(|prerequisite_id| prerequisite_id.0);
        }
        WorkTransition::PrerequisiteRemoved {
            prerequisite_id, ..
        } => {
            let previous = basis.prerequisite_ids.len();
            basis
                .prerequisite_ids
                .retain(|candidate| candidate != prerequisite_id);
            if basis.prerequisite_ids.len() == previous {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "prerequisite {prerequisite_id:?} is not active"
                )));
            }
        }
        WorkTransition::Blocked { blocker_id } => {
            let blocker = blocker.ok_or_else(|| {
                StoreError::InvalidWorkProjection(format!(
                    "block event {blocker_id} has no blocker snapshot"
                ))
            })?;
            if blocker.blocker_id != *blocker_id
                || basis
                    .active_blockers
                    .iter()
                    .any(|candidate| candidate.blocker_id == *blocker_id)
            {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "block event {blocker_id} has an invalid relation transition"
                )));
            }
            basis.active_blockers.push(WorkRelationBlockerBasis {
                blocker_id: blocker_id.clone(),
                blocker_hash: CanonicalObject::freeze(blocker)?.hash().clone(),
            });
            basis
                .active_blockers
                .sort_by(|left, right| left.blocker_id.cmp(&right.blocker_id));
        }
        WorkTransition::Unblocked { blocker_id } => {
            let previous = basis.active_blockers.len();
            basis
                .active_blockers
                .retain(|candidate| candidate.blocker_id != *blocker_id);
            if basis.active_blockers.len() == previous {
                return Err(StoreError::InvalidWorkProjection(format!(
                    "blocker {blocker_id} is not active"
                )));
            }
        }
        _ => {}
    }
    Ok(())
}

pub(super) fn require_work_item_relation_integrity(
    connection: &Connection,
    work_id: WorkId,
) -> Result<(), StoreError> {
    validated_current_work_relation_basis(connection, work_id)?;
    Ok(())
}

pub(super) fn persist_operation_result<T: Serialize>(
    transaction: &Transaction<'_>,
    operation: &str,
    key: &str,
    request_hash: &ObjectHash,
    result: &T,
) -> Result<(), StoreError> {
    transaction.execute(
        "INSERT INTO work_operation_results (
             operation, idempotency_key, request_hash, result_json
         ) VALUES (?1, ?2, ?3, ?4)",
        params![
            operation,
            key,
            request_hash.as_str(),
            serde_json::to_vec(result)?
        ],
    )?;
    Ok(())
}

pub(super) fn normalize_text(value: &str, label: &str) -> Result<String, StoreError> {
    let trimmed = value.trim();
    if trimmed.is_empty() {
        return Err(StoreError::InvalidWork(format!(
            "{label} must not be empty"
        )));
    }
    Ok(trimmed.to_owned())
}

fn normalize_optional(value: Option<String>) -> Option<String> {
    value.and_then(|value| {
        let trimmed = value.trim();
        (!trimmed.is_empty()).then(|| trimmed.to_owned())
    })
}

pub(super) fn normalize_strings(values: &[String]) -> Vec<String> {
    let mut normalized = values
        .iter()
        .map(|value| value.trim())
        .filter(|value| !value.is_empty())
        .map(str::to_owned)
        .collect::<Vec<_>>();
    normalized.sort();
    normalized.dedup();
    normalized
}

fn short_ref(work_id: WorkId) -> String {
    let simple = work_id.0.simple().to_string();
    format!("w-{}", simple.get(20..).unwrap_or(&simple))
}

pub(super) fn claim_expiry(
    now: DateTime<Utc>,
    ttl_seconds: i64,
) -> Result<DateTime<Utc>, StoreError> {
    if !(1..=MAX_WORK_TTL_SECONDS).contains(&ttl_seconds) {
        return Err(StoreError::InvalidWork(format!(
            "claim TTL must be from 1 through {MAX_WORK_TTL_SECONDS} seconds"
        )));
    }
    now.checked_add_signed(chrono::TimeDelta::seconds(ttl_seconds))
        .ok_or_else(|| StoreError::InvalidWork("claim expiry exceeds the supported clock".into()))
}

pub(in crate::storage) fn encode_state<T: Serialize>(value: T) -> Result<String, StoreError> {
    let value = serde_json::to_value(value)?;
    value.as_str().map(str::to_owned).ok_or_else(|| {
        StoreError::InvalidWorkProjection("enum did not serialize as a string".into())
    })
}

pub(super) fn assert_revision(item: &WorkItem, expected: i64) -> Result<(), StoreError> {
    if item.revision == expected {
        Ok(())
    } else {
        Err(StoreError::WorkRevisionConflict {
            work: item.work_id,
            expected,
            current: item.revision,
        })
    }
}

pub(super) fn assert_actor_session(
    actor: &crate::domain::ActorContext,
    expected: &SessionId,
) -> Result<(), StoreError> {
    if actor.session_id.as_ref() == Some(expected) {
        Ok(())
    } else {
        Err(StoreError::InvalidWork(format!(
            "actor session {:?} does not match lifecycle holder {:?}",
            actor.session_id.as_ref().map(|session| &session.0),
            expected.0
        )))
    }
}

fn validate_planning_authority(
    transaction: &Transaction<'_>,
    item: &WorkItem,
    authority: &WorkPlanningAuthority,
    actor: &crate::domain::ActorContext,
    at: DateTime<Utc>,
) -> Result<(), StoreError> {
    if let Some(run_id) = item.active_run_id {
        expire_handoff_offers(transaction, run_id, at, actor)?;
    }
    match authority {
        WorkPlanningAuthority::Project => {
            if let Some(run_id) = item.active_run_id
                && load_work_claim_optional(transaction, run_id)?.is_some_and(|claim| {
                    claim.state == WorkClaimState::Active && claim.expires_at > at
                })
            {
                return Err(StoreError::InvalidWork(
                    "project planning cannot revise work held by a live claim; use the holder's claim-bound planning context or wait for recovery"
                        .into(),
                ));
            }
        }
        WorkPlanningAuthority::Claim {
            run_id,
            holder,
            claim_id,
            claim_fence,
            ..
        } => {
            if actor.session_id.as_ref() != Some(holder) {
                return Err(StoreError::InvalidWork(
                    "planning claim holder must match the attributed actor session".into(),
                ));
            }
            validate_live_claim_on(
                transaction,
                item.work_id,
                *run_id,
                item.revision,
                holder,
                *claim_id,
                *claim_fence,
                at,
                false,
            )?;
        }
    }
    Ok(())
}

fn validate_decomposition_budget(
    connection: &Connection,
    parent: &WorkItem,
    proposed_children: usize,
) -> Result<(), StoreError> {
    if proposed_children > MAX_CHILDREN_PER_DECOMPOSITION {
        return Err(StoreError::InvalidWork(
            "decomposition exceeds the project per-operation child budget".into(),
        ));
    }
    let proposed = i64::try_from(proposed_children)
        .map_err(|_| StoreError::InvalidWork("decomposition size overflow".into()))?;
    let depth = work_depth(connection, parent.work_id)? + 1;
    if depth > i64::from(MAX_WORK_DEPTH) {
        return Err(StoreError::InvalidWork(
            "decomposition exceeds the project hierarchy depth".into(),
        ));
    }
    let open_descendants = connection.query_row(
        "WITH RECURSIVE descendants(work_id) AS (
             SELECT work_id FROM work_items WHERE parent_id = ?1
             UNION
             SELECT child.work_id FROM work_items child
             JOIN descendants parent ON child.parent_id = parent.work_id
         )
         SELECT COUNT(*) FROM descendants
         JOIN work_items item USING(work_id)
         WHERE item.lifecycle IN ('proposed', 'open')",
        [parent.root_id.0.to_string()],
        |row| row.get::<_, i64>(0),
    )?;
    if open_descendants + proposed > i64::from(MAX_OPEN_WORK_DESCENDANTS) {
        return Err(StoreError::InvalidWork(
            "decomposition exceeds the root open-descendant budget".into(),
        ));
    }
    Ok(())
}

fn work_depth(connection: &Connection, work_id: WorkId) -> Result<i64, StoreError> {
    let mut depth = 0_i64;
    let mut current = load_work_item(connection, work_id)?;
    while let Some(parent_id) = current.parent_id {
        depth += 1;
        if depth > 1_024 {
            return Err(StoreError::InvalidWorkProjection(
                "work hierarchy depth exceeds the corruption guard".into(),
            ));
        }
        current = load_work_item(connection, parent_id)?;
    }
    Ok(depth)
}

fn rebase_planning_claim(
    transaction: &Transaction<'_>,
    item: &WorkItem,
    authority: &WorkPlanningAuthority,
    at: DateTime<Utc>,
) -> Result<(Option<WorkClaim>, Option<WorkRun>), StoreError> {
    let WorkPlanningAuthority::Claim { run_id, .. } = authority else {
        return Ok((None, None));
    };
    let mut claim = load_work_claim_optional(transaction, *run_id)?
        .ok_or(StoreError::WorkClaimMismatch { work: item.work_id })?;
    let mut run = load_work_run(transaction, *run_id)?;
    claim.accepted_work_revision = item.revision;
    renew_holder_claim(transaction, &mut claim, at)?;
    run.revision += 1;
    run.updated_at = at;
    persist_work_run(transaction, &run, claim.fence)?;
    Ok((Some(claim), Some(run)))
}

pub(super) fn renew_holder_claim(
    transaction: &Transaction<'_>,
    claim: &mut WorkClaim,
    at: DateTime<Utc>,
) -> Result<(), StoreError> {
    claim.expires_at = claim
        .expires_at
        .max(claim_expiry(at, DEFAULT_WORK_CLAIM_TTL_SECONDS)?);
    claim.revision += 1;
    persist_claim(transaction, claim)
}

pub(super) fn normalize_work_catalog_key(value: &str) -> String {
    let nfc = value.trim().nfc().collect::<String>();
    nfc.as_str().case_fold().collect::<String>().nfc().collect()
}

pub(super) fn work_catalog_search_text(
    connection: &Connection,
    item: &WorkItem,
) -> Result<String, StoreError> {
    let mut parts = vec![
        item.short_ref.clone(),
        item.title.clone(),
        item.outcome.clone(),
    ];
    parts.extend(item.labels.iter().cloned());
    let mut statement = connection.prepare(
        "SELECT blocker_json FROM work_blockers
         WHERE work_id = ?1 AND state = 'active' ORDER BY blocker_id",
    )?;
    let blocker_details = statement
        .query_map([item.work_id.0.to_string()], |row| row.get::<_, Vec<u8>>(0))?
        .map(|row| {
            serde_json::from_slice::<WorkBlocker>(&row?)
                .map(|blocker| blocker.detail)
                .map_err(StoreError::from)
        })
        .collect::<Result<Vec<_>, _>>()?;
    parts.extend(blocker_details);
    Ok(normalize_work_catalog_key(&parts.join("\n")))
}

pub(in crate::storage) fn refresh_work_catalog_projection(
    connection: &Connection,
    item: &WorkItem,
) -> Result<(), StoreError> {
    let assigned_to_key = item.assigned_to.as_deref().map(normalize_work_catalog_key);
    let search_text_key = work_catalog_search_text(connection, item)?;
    let changed = connection.execute(
        "UPDATE work_items
         SET assigned_to_key = ?2, search_text_key = ?3
         WHERE work_id = ?1",
        params![item.work_id.0.to_string(), assigned_to_key, search_text_key],
    )?;
    if changed != 1 {
        return Err(StoreError::InvalidWorkProjection(format!(
            "work catalog refresh lost item {:?}",
            item.work_id
        )));
    }
    connection.execute(
        "DELETE FROM work_item_labels WHERE work_id = ?1",
        [item.work_id.0.to_string()],
    )?;
    let mut label_keys = item
        .labels
        .iter()
        .map(|label| normalize_work_catalog_key(label))
        .collect::<Vec<_>>();
    label_keys.sort();
    label_keys.dedup();
    for label_key in label_keys {
        connection.execute(
            "INSERT INTO work_item_labels (work_id, label_key) VALUES (?1, ?2)",
            params![item.work_id.0.to_string(), label_key],
        )?;
    }
    connection.execute(
        "DELETE FROM work_catalog_fts WHERE work_id = ?1",
        [item.work_id.0.to_string()],
    )?;
    connection.execute(
        "INSERT INTO work_catalog_fts (work_id, search_text) VALUES (?1, ?2)",
        params![item.work_id.0.to_string(), search_text_key],
    )?;
    Ok(())
}

pub(super) fn persist_work_item(
    transaction: &Transaction<'_>,
    item: &WorkItem,
) -> Result<(), StoreError> {
    transaction.execute(
        "UPDATE work_items SET
             lifecycle = ?2, priority = ?3, assigned_to = ?4,
             deferred_until_ms = ?5, revision = ?6, active_run_id = ?7,
             superseded_by = ?8, updated_at_ms = ?9, item_json = ?10
         WHERE work_id = ?1",
        params![
            item.work_id.0.to_string(),
            encode_state(item.lifecycle)?,
            item.priority,
            item.assigned_to,
            item.deferred_until.map(|value| value.timestamp_millis()),
            item.revision,
            item.active_run_id.map(|value| value.0.to_string()),
            item.superseded_by.map(|value| value.0.to_string()),
            item.updated_at.timestamp_millis(),
            serde_json::to_vec(item)?
        ],
    )?;
    refresh_work_catalog_projection(transaction, item)
}

pub(super) fn persist_work_run(
    transaction: &Transaction<'_>,
    run: &WorkRun,
    claim_fence_head: i64,
) -> Result<(), StoreError> {
    transaction.execute(
        "UPDATE work_runs SET
             executor_session_id = ?2, state = ?3, revision = ?4,
             claim_fence_head = ?5, last_checkpoint_hash = ?6,
             completion_seal_hash = ?7, updated_at_ms = ?8, run_json = ?9
         WHERE run_id = ?1",
        params![
            run.run_id.0.to_string(),
            run.executor.as_ref().map(|value| value.0.as_str()),
            encode_state(run.state)?,
            run.revision,
            claim_fence_head,
            run.last_checkpoint.as_ref().map(ObjectHash::as_str),
            run.completion_seal.as_ref().map(ObjectHash::as_str),
            run.updated_at.timestamp_millis(),
            serde_json::to_vec(run)?
        ],
    )?;
    Ok(())
}

pub(super) fn persist_claim(
    transaction: &Transaction<'_>,
    claim: &WorkClaim,
) -> Result<(), StoreError> {
    transaction.execute(
        "INSERT INTO work_claims (
             run_id, work_id, claim_id, holder_session_id, state,
             expires_at_ms, revision, fence, claim_json
         ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)
         ON CONFLICT(run_id) DO UPDATE SET
             claim_id = excluded.claim_id,
             holder_session_id = excluded.holder_session_id,
             state = excluded.state,
             expires_at_ms = excluded.expires_at_ms,
             revision = excluded.revision,
             fence = excluded.fence,
             claim_json = excluded.claim_json",
        params![
            claim.run_id.0.to_string(),
            claim.work_id.0.to_string(),
            claim.claim_id.0.to_string(),
            claim.holder.0,
            encode_state(claim.state)?,
            claim.expires_at.timestamp_millis(),
            claim.revision,
            claim.fence,
            serde_json::to_vec(claim)?
        ],
    )?;
    Ok(())
}

pub(super) fn persist_root_execution(
    transaction: &Transaction<'_>,
    execution: &RootExecution,
) -> Result<(), StoreError> {
    transaction.execute(
        "UPDATE work_root_executions SET
             state = ?2, revision = ?3, updated_at_ms = ?4, execution_json = ?5
         WHERE root_execution_id = ?1",
        params![
            execution.root_execution_id.0.to_string(),
            encode_state(execution.state)?,
            execution.revision,
            execution.updated_at.timestamp_millis(),
            serde_json::to_vec(execution)?
        ],
    )?;
    Ok(())
}

#[allow(
    clippy::too_many_arguments,
    reason = "the exact authority basis is intentionally explicit at the storage boundary"
)]
pub(super) fn validate_live_claim_on(
    connection: &Connection,
    work_id: WorkId,
    run_id: WorkRunId,
    expected_work_revision: i64,
    holder: &SessionId,
    claim_id: WorkClaimId,
    claim_fence: i64,
    now: DateTime<Utc>,
    allow_pending_handoff: bool,
) -> Result<(WorkItem, WorkRun, WorkClaim), StoreError> {
    let item = load_work_item(connection, work_id)?;
    validate_live_claim_for_item_on(
        connection,
        item,
        run_id,
        expected_work_revision,
        holder,
        claim_id,
        claim_fence,
        now,
        allow_pending_handoff,
    )
}

#[allow(
    clippy::too_many_arguments,
    reason = "the exact authority basis is intentionally explicit at the storage boundary"
)]
pub(super) fn validate_live_claim_for_item_on(
    connection: &Connection,
    item: WorkItem,
    run_id: WorkRunId,
    expected_work_revision: i64,
    holder: &SessionId,
    claim_id: WorkClaimId,
    claim_fence: i64,
    now: DateTime<Utc>,
    allow_pending_handoff: bool,
) -> Result<(WorkItem, WorkRun, WorkClaim), StoreError> {
    let work_id = item.work_id;
    assert_revision(&item, expected_work_revision)?;
    if item.lifecycle != WorkLifecycle::Open || item.active_run_id != Some(run_id) {
        return Err(StoreError::WorkClaimMismatch { work: work_id });
    }
    let run = load_work_run(connection, run_id)?;
    if run.work_id != work_id
        || !matches!(run.state, WorkRunState::Claimed | WorkRunState::Active)
        || !ancestors_admit_execution(connection, &item)?
        || !run_uses_active_root_execution(connection, &item, &run)?
    {
        return Err(StoreError::WorkClaimMismatch { work: work_id });
    }
    let claim = load_work_claim_optional(connection, run_id)?
        .ok_or(StoreError::WorkClaimMismatch { work: work_id })?;
    let exact_authority_basis = claim.work_id == work_id
        && claim.run_id == run_id
        && claim.claim_id == claim_id
        && claim.accepted_work_revision == expected_work_revision
        && claim.fence == claim_fence
        && &claim.holder == holder
        && claim.state == WorkClaimState::Active;
    if exact_authority_basis && claim.expires_at <= now {
        return Err(StoreError::WorkClaimLapsed {
            work: work_id,
            expired_at: claim.expires_at,
        });
    }
    if !exact_authority_basis {
        return Err(StoreError::WorkClaimMismatch { work: work_id });
    }
    if !allow_pending_handoff {
        let pending: Option<i64> = connection
            .query_row(
                "SELECT 1 FROM work_handoff_offers
                 WHERE run_id = ?1 AND state = 'offered' AND expires_at_ms > ?2",
                params![run_id.0.to_string(), now.timestamp_millis()],
                |row| row.get(0),
            )
            .optional()?;
        if pending.is_some() {
            return Err(StoreError::InvalidWork(
                super::super::PENDING_HANDOFF_REFUSAL.into(),
            ));
        }
    }
    Ok((item, run, claim))
}

pub(in crate::storage) fn validate_control_work_binding_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
    session_id: &SessionId,
    binding: &ControlWorkBinding,
    now: DateTime<Utc>,
) -> Result<(), StoreError> {
    let item = match load_work_item(connection, binding.work_id) {
        Ok(item) => item,
        Err(StoreError::WorkNotFound(_)) => {
            return Err(StoreError::WorkClaimMismatch {
                work: binding.work_id,
            });
        }
        Err(error) => return Err(error),
    };
    if &item.project_id != project_id
        || !control_work_binding_was_valid_on(connection, project_id, session_id, binding)?
    {
        return Err(StoreError::WorkClaimMismatch {
            work: binding.work_id,
        });
    }
    match validate_live_claim_on(
        connection,
        binding.work_id,
        binding.run_id,
        binding.work_revision,
        session_id,
        binding.claim_id,
        binding.claim_fence,
        now,
        false,
    ) {
        Ok(_) => Ok(()),
        Err(
            StoreError::WorkRevisionConflict { .. }
            | StoreError::WorkClaimMismatch { .. }
            | StoreError::WorkClaimLapsed { .. }
            | StoreError::InvalidWork(_),
        ) => Err(StoreError::ControlWorkBindingStale {
            work: binding.work_id,
        }),
        Err(error) => Err(error),
    }
}

fn control_work_binding_was_valid_on(
    connection: &Connection,
    project_id: &crate::domain::ProjectId,
    session_id: &SessionId,
    binding: &ControlWorkBinding,
) -> Result<bool, StoreError> {
    Ok(canonical_work_events_for_item(connection, binding.work_id)?
        .iter()
        .any(|event| {
            event.project_id == *project_id
                && event.work_id == binding.work_id
                && event.work.lifecycle == WorkLifecycle::Open
                && event.work.revision == binding.work_revision
                && event.work.active_run_id == Some(binding.run_id)
                && event.run.as_ref().is_some_and(|run| {
                    run.run_id == binding.run_id
                        && run.work_id == binding.work_id
                        && run.root_execution_id == binding.root_execution_id
                        && matches!(run.state, WorkRunState::Claimed | WorkRunState::Active)
                })
                && event.claim.as_ref().is_some_and(|claim| {
                    claim.work_id == binding.work_id
                        && claim.run_id == binding.run_id
                        && claim.claim_id == binding.claim_id
                        && claim.accepted_work_revision == binding.work_revision
                        && claim.fence == binding.claim_fence
                        && claim.holder == *session_id
                        && claim.state == WorkClaimState::Active
                        && claim.expires_at > event.created_at
                })
        }))
}

pub(super) fn unique_hashes(values: &[ObjectHash]) -> Vec<ObjectHash> {
    let mut seen = HashSet::new();
    values
        .iter()
        .filter(|value| seen.insert((*value).clone()))
        .cloned()
        .collect()
}

pub(super) fn expect_root_contributor(
    execution: &mut RootExecution,
    participant: &SessionId,
) -> bool {
    if execution.expected_contributors.contains(participant) {
        return false;
    }
    execution.expected_contributors.push(participant.clone());
    execution
        .expected_contributors
        .sort_by(|left, right| left.0.cmp(&right.0));
    true
}

pub(super) fn add_root_contribution(
    execution: &mut RootExecution,
    participant: &SessionId,
    object: &ObjectHash,
) -> bool {
    let contribution = RootContribution {
        participant: participant.clone(),
        object: object.clone(),
    };
    if execution.contributions.contains(&contribution) {
        return false;
    }
    execution.contributions.push(contribution);
    execution.contributions.sort_by(|left, right| {
        left.participant
            .0
            .cmp(&right.participant.0)
            .then_with(|| left.object.as_str().cmp(right.object.as_str()))
    });
    true
}

pub(super) fn waive_root_contributor(
    execution: &mut RootExecution,
    participant: &SessionId,
    waived_by: &str,
    reason: &str,
) -> bool {
    if execution
        .contributions
        .iter()
        .any(|contribution| &contribution.participant == participant)
        || execution
            .waivers
            .iter()
            .any(|waiver| &waiver.participant == participant)
    {
        return false;
    }
    execution.waivers.push(CompletionWaiver {
        participant: participant.clone(),
        waived_by: waived_by.to_owned(),
        reason: reason.trim().to_owned(),
    });
    execution
        .waivers
        .sort_by(|left, right| left.participant.0.cmp(&right.participant.0));
    true
}

pub(super) fn first_unaccounted_root_contributor(execution: &RootExecution) -> Option<&SessionId> {
    execution
        .expected_contributors
        .iter()
        .find(|participant| !root_participant_is_accounted(execution, participant))
}

pub(super) fn root_participant_is_accounted(
    execution: &RootExecution,
    participant: &SessionId,
) -> bool {
    execution
        .contributions
        .iter()
        .any(|contribution| &contribution.participant == participant)
        || execution
            .waivers
            .iter()
            .any(|waiver| &waiver.participant == participant)
}
