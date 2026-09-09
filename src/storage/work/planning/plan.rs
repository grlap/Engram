//! Whole-plan admission shares the ordinary planning transitions and one commit.

use super::{
    CanonicalObject, ChangeWorkPrerequisiteRequest, CreateWorkRequest, DecomposeWorkRequest,
    HashMap, HashSet, MAX_OPEN_WORK_DESCENDANTS, MAX_WORK_DEPTH, PlanRelationBasis,
    PlanningValidation, Redactor, SessionId, SqliteStore, StoreError, Transaction, WorkId,
    WorkItem, WorkLifecycle, WorkOrigin, WorkPlanningAuthority,
    change_work_prerequisite_with_validation_on, combined_graph_is_acyclic,
    create_root_with_validation_on, decompose_work_with_validation_on, inspect_work_request,
    normalize_optional, normalize_strings, normalize_text, persist_operation_result,
    replay_operation, require_work_item_relation_integrity, root_open_descendant_count,
    validated_current_work_relation_basis,
};
use crate::domain::{
    ChildRequirement, ChildWorkDraft, MAX_WORK_PLAN_BYTES, MAX_WORK_PLAN_EDGES,
    MAX_WORK_PLAN_KEY_BYTES, MAX_WORK_PLAN_TASKS, ProjectId, ProposeWorkPlanRequest,
    WorkPlanDependency, WorkPlanInput, WorkPlanMapping, WorkPlanReceipt,
};

#[cfg(test)]
mod tests;

struct ValidatedPlan {
    drafts: Vec<ChildWorkDraft>,
    notes: Vec<Vec<String>>,
    parents: Vec<Option<usize>>,
    order: Vec<usize>,
    edges: Vec<(usize, WorkPlanDependency)>,
}

impl SqliteStore {
    /// Admits a complete authored forest and dependency graph atomically.
    ///
    /// # Errors
    /// Returns a typed work refusal for invalid input or conflicting replay,
    /// or a storage error. No partial plan or replay result is committed.
    pub fn propose_work_plan<R: Redactor>(
        &mut self,
        request: &ProposeWorkPlanRequest,
        redactor: &R,
    ) -> Result<WorkPlanReceipt, StoreError> {
        self.propose_work_plan_with_admission(request, redactor, |_| Ok(()))
    }

    pub(crate) fn propose_work_plan_with_admission<R: Redactor>(
        &mut self,
        request: &ProposeWorkPlanRequest,
        redactor: &R,
        admit: impl Fn(&WorkPlanReceipt) -> Result<(), StoreError>,
    ) -> Result<WorkPlanReceipt, StoreError> {
        let plan = validate_plan(&request.plan)?;
        normalize_text(&request.project_id.0, "plan project")?;
        inspect_work_request(redactor, request, &request.actor)?;
        let session = request.actor.session_id.as_ref().ok_or_else(|| {
            StoreError::InvalidWork("plan admission requires an asserted session".into())
        })?;
        normalize_text(&session.0, "plan session")?;
        // Match the protocol's stable intent fields. Host context, provenance,
        // reason and retry time do not re-author an already committed operation.
        // Full asserted attribution was inspected above and is recorded on the
        // first admission; a changed plan, actor, session or skill never replays.
        let intent = CanonicalObject::freeze(&(
            &request.project_id,
            session,
            &request.actor.actor_id,
            &request.actor.source_skill,
            &request.plan,
        ))?;
        let scoped_key =
            plan_operation_key(&request.project_id, session, &request.plan.idempotency_key)?;
        let transaction = self.begin_work_mutation()?;
        if let Some(receipt) = replay_operation::<WorkPlanReceipt>(
            &transaction,
            "propose_work_plan",
            &scoped_key,
            intent.hash(),
        )? {
            admit(&receipt)?;
            transaction.commit()?;
            return Ok(receipt);
        }
        let existing = validate_existing_on(&transaction, request)?;
        let receipt = admit_plan_on(&transaction, request, &plan, &existing, redactor)?;
        for (index, parent) in plan.parents.iter().enumerate() {
            if parent.is_none() {
                let root = receipt.tasks.get(index).ok_or_else(|| {
                    StoreError::InvalidWorkProjection("validated plan lost a created task".into())
                })?;
                validate_plan_root_budget(&transaction, root)?;
            }
        }
        // Shared transitions deferred their full-project scan. Verify the
        // complete old/new union exactly once under this same writer lock,
        // including unrelated pre-existing cycles, before any commit/receipt.
        if !combined_graph_is_acyclic(&transaction, &request.project_id.0)? {
            return Err(StoreError::WorkDependencyCycle);
        }
        admit(&receipt)?;
        persist_operation_result(
            &transaction,
            "propose_work_plan",
            &scoped_key,
            intent.hash(),
            &receipt,
        )?;
        transaction.commit()?;
        Ok(receipt)
    }
}

fn validate_plan_root_budget(
    connection: &super::Connection,
    root: &WorkPlanMapping,
) -> Result<(), StoreError> {
    let descendants = root_open_descendant_count(connection, root.work_id)?;
    if descendants > i64::from(MAX_OPEN_WORK_DESCENDANTS) {
        return Err(StoreError::InvalidWork(format!(
            "plan: root '{}' has {descendants} open descendants; at most {MAX_OPEN_WORK_DESCENDANTS} are allowed ({} tasks including the root)",
            root.key,
            MAX_OPEN_WORK_DESCENDANTS + 1
        )));
    }
    Ok(())
}

fn plan_operation_key(
    project: &ProjectId,
    session: &SessionId,
    key: &str,
) -> Result<String, StoreError> {
    Ok(
        CanonicalObject::freeze(&("work_propose:plan", project, session, key))?
            .hash()
            .as_str()
            .to_owned(),
    )
}

pub(crate) fn validate_work_plan(input: &WorkPlanInput) -> Result<(), StoreError> {
    validate_plan(input).map(|_| ())
}

fn validate_existing_on(
    transaction: &Transaction<'_>,
    request: &ProposeWorkPlanRequest,
) -> Result<HashMap<String, WorkId>, StoreError> {
    // All parents and dependants are new payload-local items. Existing items
    // cannot point back to these not-yet-created IDs, so outgoing prerequisites
    // cannot close a mixed old/new cycle. Attaching to an existing parent would
    // invalidate this argument and requires additional union-graph validation.
    let mut existing = HashMap::new();
    let mut distinct = HashSet::new();
    for edge in &request.plan.prerequisites {
        let WorkPlanDependency::Existing(reference) = &edge.prerequisite else {
            continue;
        };
        let item =
            super::super::query::resolve_work_ref_on(transaction, &request.project_id, reference)?;
        if item.lifecycle == WorkLifecycle::Completed {
            return Err(StoreError::WorkPrerequisiteAlreadySatisfied(item.work_id));
        }
        if item.lifecycle != WorkLifecycle::Open {
            return Err(StoreError::WorkNotOpen(item.work_id));
        }
        require_work_item_relation_integrity(transaction, item.work_id)?;
        if !distinct.insert((&edge.work_key, item.work_id)) {
            return Err(StoreError::InvalidWork(
                "plan has duplicate resolved prerequisite edges".into(),
            ));
        }
        existing.insert(reference.clone(), item.work_id);
    }
    Ok(existing)
}

fn valid_key(value: &str) -> bool {
    !value.is_empty()
        && value.len() <= MAX_WORK_PLAN_KEY_BYTES
        && value
            .bytes()
            .all(|byte| byte.is_ascii_alphanumeric() || b"._-".contains(&byte))
        && value.as_bytes()[0].is_ascii_alphanumeric()
}

#[allow(
    clippy::too_many_lines,
    reason = "whole-input validation precedes every planning write"
)]
fn validate_plan(input: &WorkPlanInput) -> Result<ValidatedPlan, StoreError> {
    let invalid = |message: &str| StoreError::InvalidWork(format!("plan: {message}"));
    if input.tasks.is_empty() || input.tasks.len() > MAX_WORK_PLAN_TASKS {
        return Err(invalid(&format!(
            "expected 1 through {MAX_WORK_PLAN_TASKS} tasks"
        )));
    }
    if input.prerequisites.len() > MAX_WORK_PLAN_EDGES {
        return Err(invalid(&format!(
            "at most {MAX_WORK_PLAN_EDGES} prerequisite edges are allowed"
        )));
    }
    if !valid_key(&input.idempotency_key) {
        return Err(invalid("idempotency key must be a 1..64 byte ASCII token"));
    }
    if serde_json::to_vec(input)?.len() > MAX_WORK_PLAN_BYTES {
        return Err(invalid(&format!(
            "serialized plan exceeds {MAX_WORK_PLAN_BYTES} bytes"
        )));
    }
    let mut keys = HashMap::new();
    let mut drafts = Vec::new();
    let mut notes = Vec::new();
    let mut note_count = 0_usize;
    for (index, task) in input.tasks.iter().enumerate() {
        if !valid_key(&task.key) || keys.insert(task.key.as_str(), index).is_some() {
            return Err(invalid("task keys must be unique 1..64 byte ASCII tokens"));
        }
        if task.acceptance.len() > 64 || task.labels.len() > 64 {
            return Err(invalid(
                "at most 64 acceptance entries and 64 labels per task",
            ));
        }
        if task.acceptance.iter().any(|text| text.trim().is_empty()) {
            return Err(invalid("acceptance entries must not be blank"));
        }
        if task
            .priority
            .is_some_and(|priority| !(0..=4).contains(&priority))
        {
            return Err(invalid("priority must be from 0 through 4"));
        }
        if task.parent_key.is_none() && task.requirement == Some(ChildRequirement::Optional) {
            return Err(invalid("an optional child requirement needs a parent key"));
        }
        note_count = note_count.saturating_add(task.notes.len());
        crate::domain::validate_initial_work_note_count(note_count)
            .map_err(StoreError::InvalidWork)?;
        notes.push(
            crate::domain::normalize_initial_work_notes(&task.notes)
                .map_err(StoreError::InvalidWork)?,
        );
        drafts.push(ChildWorkDraft {
            local_key: task.key.clone(),
            external_ref: crate::domain::normalize_external_reference(task.external_ref.as_deref())
                .map_err(StoreError::InvalidWork)?,
            title: normalize_text(&task.title, "plan task title")?,
            outcome: normalize_text(&task.outcome, "plan task outcome")?,
            acceptance: normalize_strings(&task.acceptance),
            kind: task.kind.unwrap_or(crate::domain::WorkItemKind::Task),
            priority: task.priority.unwrap_or(1),
            child_requirement: task.requirement.unwrap_or(ChildRequirement::Required),
            labels: normalize_strings(&task.labels),
            assigned_to: normalize_optional(task.assigned_to.clone()),
            deferred_until: task.deferred_until,
            notes: task.notes.clone(),
        });
    }
    let resolve = |key: &str| {
        keys.get(key)
            .copied()
            .ok_or_else(|| invalid("relationship names an unknown payload-local key"))
    };
    let parents = input
        .tasks
        .iter()
        .map(|task| task.parent_key.as_deref().map(&resolve).transpose())
        .collect::<Result<Vec<_>, _>>()?;
    let mut depths = Vec::new();
    for index in 0..input.tasks.len() {
        let mut depth = 0_u32;
        let mut parent = parents[index];
        let mut visited = HashSet::from([index]);
        while let Some(ancestor) = parent {
            if !visited.insert(ancestor) {
                return Err(StoreError::WorkDependencyCycle);
            }
            depth += 1;
            parent = parents[ancestor];
        }
        if depth > MAX_WORK_DEPTH {
            return Err(invalid(&format!(
                "hierarchy depth exceeds {MAX_WORK_DEPTH} (root depth is zero)"
            )));
        }
        depths.push(depth);
    }
    let mut order = (0..input.tasks.len()).collect::<Vec<_>>();
    order.sort_by_key(|index| depths[*index]);
    let mut adjacency = vec![Vec::new(); input.tasks.len()];
    for (index, parent) in parents.iter().enumerate() {
        if let Some(parent) = parent
            && drafts[index].child_requirement == ChildRequirement::Required
        {
            adjacency[*parent].push(index);
        }
    }
    let mut edges = Vec::new();
    let mut distinct = HashSet::new();
    for edge in &input.prerequisites {
        let work = resolve(&edge.work_key)?;
        let spelling = match &edge.prerequisite {
            WorkPlanDependency::Local(key) => format!("local:{key}"),
            WorkPlanDependency::Existing(reference) => {
                if reference.trim().is_empty() || reference.len() > 128 {
                    return Err(invalid("existing prerequisite ref must have 1..128 bytes"));
                }
                format!("existing:{reference}")
            }
        };
        if !distinct.insert((work, spelling)) {
            return Err(invalid("duplicate prerequisite edge"));
        }
        edges.push((work, edge.prerequisite.clone()));
        let WorkPlanDependency::Local(key) = &edge.prerequisite else {
            continue;
        };
        let prerequisite = resolve(key)?;
        let mut parent = parents[work];
        while let Some(ancestor) = parent {
            if ancestor == prerequisite {
                return Err(StoreError::WorkDependencyCycle);
            }
            parent = parents[ancestor];
        }
        adjacency[work].push(prerequisite);
    }
    if !plan_graph_is_acyclic(&adjacency) {
        return Err(StoreError::WorkDependencyCycle);
    }
    Ok(ValidatedPlan {
        drafts,
        notes,
        parents,
        order,
        edges,
    })
}

// Kahn's algorithm visits every vertex and edge once. Parallel explicit and
// implicit edges are counted and removed equally; no reachability matrix or
// recursion grows with plan size.
fn plan_graph_is_acyclic(adjacency: &[Vec<usize>]) -> bool {
    let mut incoming = vec![0_usize; adjacency.len()];
    for targets in adjacency {
        for &target in targets {
            incoming[target] += 1;
        }
    }
    let mut ready = incoming
        .iter()
        .enumerate()
        .filter_map(|(index, count)| (*count == 0).then_some(index))
        .collect::<Vec<_>>();
    let mut visited = 0;
    while let Some(index) = ready.pop() {
        visited += 1;
        for &target in &adjacency[index] {
            incoming[target] -= 1;
            if incoming[target] == 0 {
                ready.push(target);
            }
        }
    }
    visited == adjacency.len()
}

fn item_at(items: &[Option<WorkItem>], index: usize) -> Result<&WorkItem, StoreError> {
    items.get(index).and_then(Option::as_ref).ok_or_else(|| {
        StoreError::InvalidWorkProjection("validated plan lost a created task".into())
    })
}

#[allow(
    clippy::too_many_lines,
    reason = "one transaction composes existing root, child, and edge transitions"
)]
fn admit_plan_on<R: Redactor>(
    transaction: &Transaction<'_>,
    request: &ProposeWorkPlanRequest,
    plan: &ValidatedPlan,
    existing: &HashMap<String, WorkId>,
    redactor: &R,
) -> Result<WorkPlanReceipt, StoreError> {
    let mut items = vec![None; plan.drafts.len()];
    for &index in &plan.order {
        if plan.parents[index].is_none() {
            let draft = &plan.drafts[index];
            items[index] = Some(create_root_with_validation_on(
                transaction,
                &CreateWorkRequest {
                    project_id: request.project_id.clone(),
                    parent_id: None,
                    child_requirement: ChildRequirement::Required,
                    title: draft.title.clone(),
                    outcome: draft.outcome.clone(),
                    acceptance: draft.acceptance.clone(),
                    kind: draft.kind,
                    priority: draft.priority,
                    labels: draft.labels.clone(),
                    external_ref: draft.external_ref.clone(),
                    assigned_to: draft.assigned_to.clone(),
                    deferred_until: draft.deferred_until,
                    notes: draft.notes.clone(),
                    origin: WorkOrigin::Local,
                    source_snapshot_id: None,
                    actor: request.actor.clone(),
                    idempotency_key: String::new(),
                    created_at: request.created_at,
                },
                &plan.notes[index],
                redactor,
                PlanningValidation::AtomicPlan,
            )?);
        }
        let child_indices = plan
            .parents
            .iter()
            .enumerate()
            .filter_map(|(child, parent)| (*parent == Some(index)).then_some(child))
            .collect::<Vec<_>>();
        if child_indices.is_empty() {
            continue;
        }
        let parent = item_at(&items, index)?;
        let children = child_indices
            .iter()
            .map(|child| {
                let mut draft = plan.drafts[*child].clone();
                if request.plan.tasks[*child].priority.is_none() {
                    draft.priority = parent.priority;
                }
                draft
            })
            .collect();
        let initial_notes = child_indices
            .iter()
            .map(|child| plan.notes[*child].clone())
            .collect::<Vec<_>>();
        let decomposition = decompose_work_with_validation_on(
            transaction,
            &DecomposeWorkRequest {
                parent_id: parent.work_id,
                expected_parent_revision: parent.revision,
                children,
                prerequisites: Vec::new(),
                authority: WorkPlanningAuthority::Project,
                actor: request.actor.clone(),
                idempotency_key: String::new(),
                created_at: request.created_at,
            },
            &initial_notes,
            redactor,
            PlanningValidation::AtomicPlan,
        )?;
        items[index] = Some(decomposition.parent);
        for (child, item) in child_indices.into_iter().zip(decomposition.children) {
            items[child] = Some(item);
        }
    }
    let mut relations = HashMap::new();
    for (work, prerequisite) in &plan.edges {
        let prerequisite_id = match prerequisite {
            WorkPlanDependency::Existing(reference) => {
                *existing.get(reference).ok_or_else(|| {
                    StoreError::InvalidWorkProjection(
                        "validated plan lost existing prerequisite".into(),
                    )
                })?
            }
            WorkPlanDependency::Local(key) => {
                let index = plan
                    .drafts
                    .iter()
                    .position(|draft| draft.local_key == *key)
                    .ok_or_else(|| {
                        StoreError::InvalidWorkProjection(
                            "validated plan lost prerequisite key".into(),
                        )
                    })?;
                item_at(&items, index)?.work_id
            }
        };
        let item = item_at(&items, *work)?;
        let basis = match relations.entry(item.work_id) {
            std::collections::hash_map::Entry::Occupied(entry) => entry.into_mut(),
            std::collections::hash_map::Entry::Vacant(entry) => entry.insert(PlanRelationBasis {
                work_id: item.work_id,
                basis: validated_current_work_relation_basis(transaction, item.work_id)?,
            }),
        };
        let changed = change_work_prerequisite_with_validation_on(
            transaction,
            &ChangeWorkPrerequisiteRequest {
                work_id: item.work_id,
                prerequisite_id,
                expected_revision: item.revision,
                authority: WorkPlanningAuthority::Project,
                actor: request.actor.clone(),
                idempotency_key: String::new(),
                changed_at: request.created_at,
            },
            true,
            PlanningValidation::AtomicPlan,
            Some(basis),
        )?;
        items[*work] = Some(changed);
    }
    // Validate every final edge's canonical proof and the complete relation
    // fingerprint once per touched item. The transaction-local bases above
    // cannot escape this call or substitute for the final stored-state audit.
    for work_id in relations.keys() {
        require_work_item_relation_integrity(transaction, *work_id)?;
    }
    let tasks = plan
        .drafts
        .iter()
        .enumerate()
        .map(|(index, draft)| {
            let item = item_at(&items, index)?;
            Ok(WorkPlanMapping {
                key: draft.local_key.clone(),
                work_id: item.work_id,
                short_ref: item.short_ref.clone(),
                revision: item.revision,
            })
        })
        .collect::<Result<Vec<_>, StoreError>>()?;
    Ok(WorkPlanReceipt { tasks })
}
