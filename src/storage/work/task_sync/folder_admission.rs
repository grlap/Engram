//! Admission and application of the changes a resolved folder asks of an
//! enrolled destination: additions, scalar edits and execution-free removals.
//!
//! Admission runs in two stages before any write. The first judges each
//! task on its own: what the planner found it asks, and whether this store
//! lets that task take it. The second judges each addition in context: its
//! ancestors, its root's execution and the store's whole import-origin
//! history. Every conflict is collected, one per task. Complete folders apply
//! the admitted tasks and report the others; strict delta candidates retain
//! their whole-candidate admission. A task's first-stage reason takes precedence
//! over a second-stage one; within the second stage, an addition's own
//! ancestor chain is judged first, then whether an addition it depends on
//! was itself refused, then the graph the import would leave, and last the
//! origin history. A graph failure no single addition accounts for refuses
//! at folder level, naming no task. For deletion, a retained incoming edge
//! precedes ancestor/root binding, after the target's own eligibility.
//!
//! Admission builds every index once; a held
//! ancestor's eligibility, a root's engaged execution and
//! a completion's provenance are each read once, however many additions
//! share them. The mixed-graph fixpoint checks the selected graph again after
//! each refusal cascade; every retry sets aside at least one requested change,
//! naming the tasks the located failure involves.
//! Each removal is independently rechecked at its writer.

use std::collections::{BTreeMap, HashMap, HashSet};

mod relations;

use super::{origin, query};
use crate::{
    ActorContext, ProjectId, WorkId, WorkLifecycle,
    domain::WorkTransition,
    storage::StoreError,
    task_sync::{
        FORMAT, ProjectDocument, TaskDocument, TaskSyncRefusal, TaskTree,
        folder_plan::{DeltaPlan, ProjectChange, TaskChange, TaskDelta, TaskField},
    },
};

/// How many refused tasks a refusal names before it counts the rest.
pub(super) const NAMED_CONFLICT_LIMIT: usize = 20;

#[cfg(test)]
thread_local! {
    /// How many held tasks admission read for eligibility, in this thread.
    pub(super) static ELIGIBILITY_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// How many prospective origin histories admission validated, in this thread.
    pub(super) static ORIGIN_VALIDATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// How many prospective destination graphs admission validated, in this thread.
    pub(super) static RETAINED_VALIDATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Why one task the folder would change cannot take that change here.
pub(super) struct Conflict {
    pub(super) id: WorkId,
    pub(super) reason: &'static str,
    pub(super) detail: String,
}

#[derive(Debug)]
pub(super) struct Application {
    pub(super) plan: DeltaPlan,
    pub(super) tasks: Vec<super::TaskSyncTaskResult>,
}

/// The folder's prospective shared planning: the destination's current
/// documents beside the resolved ones, so that a change is judged in the
/// context of the whole tree it would produce. The current documents are
/// kept as they are, not patched, so a later kind of change can compare the
/// two sides of any field.
pub(super) struct Prospective<'a> {
    pub(super) project: &'a ProjectId,
    project_document: &'a ProjectDocument,
    /// The destination's current exported documents, by id.
    pub(super) current: HashMap<WorkId, &'a TaskDocument>,
    /// The resolved documents, by id.
    pub(super) resolved: HashMap<WorkId, &'a TaskDocument>,
    /// Each resolved task's root, computed from the whole resolved tree.
    pub(super) roots: HashMap<WorkId, WorkId>,
    /// Current roots include tasks that the resolved folder removes.
    current_roots: HashMap<WorkId, WorkId>,
    /// Snapshots of the validated resolved scope, including added tasks.
    snapshots: HashMap<WorkId, crate::WorkGraphSnapshotItem>,
    notes_included: bool,
}

impl<'a> Prospective<'a> {
    pub(super) fn new(current: &'a TaskTree, resolved: &'a TaskTree) -> Result<Self, StoreError> {
        Ok(Self::from_documents(
            &resolved.project,
            current.tasks.iter().map(|task| (task.id, task)).collect(),
            resolved.tasks.iter().map(|task| (task.id, task)).collect(),
            current
                .snapshots()?
                .into_iter()
                .map(|item| (item.work_id, item.root_id))
                .collect(),
            resolved
                .snapshots()?
                .into_iter()
                .map(|item| (item.work_id, item))
                .collect(),
        ))
    }

    /// Both maps are complete connected scopes with already validated graph
    /// snapshots. The atomic owner captures them afresh inside its writer;
    /// this private constructor grants no admission to a partial task folder.
    pub(super) fn from_documents(
        project: &'a ProjectDocument,
        current: HashMap<WorkId, &'a TaskDocument>,
        resolved: HashMap<WorkId, &'a TaskDocument>,
        current_roots: HashMap<WorkId, WorkId>,
        snapshots: HashMap<WorkId, crate::WorkGraphSnapshotItem>,
    ) -> Self {
        Self {
            project: &project.project_id,
            project_document: project,
            current,
            resolved,
            roots: snapshots
                .values()
                .map(|item| (item.work_id, item.root_id))
                .collect(),
            current_roots,
            snapshots,
            notes_included: project.notes_included,
        }
    }

    /// The resolved task's ancestors, nearest first. The resolved tree is a
    /// valid graph, so every parent is in it and the chain ends.
    fn ancestors(&self, id: WorkId) -> Vec<WorkId> {
        let mut chain = Vec::new();
        let mut cursor = self.resolved.get(&id).and_then(|task| task.parent_id);
        while let Some(parent) = cursor {
            chain.push(parent);
            cursor = self.resolved.get(&parent).and_then(|task| task.parent_id);
        }
        chain
    }

    fn resolved_task(&self, id: WorkId) -> Result<&'a TaskDocument, StoreError> {
        self.resolved.get(&id).copied().ok_or_else(|| {
            StoreError::InvalidWorkProjection("a planned task is missing from the folder".into())
        })
    }

    fn root(&self, id: WorkId) -> Result<WorkId, StoreError> {
        self.roots.get(&id).copied().ok_or_else(|| {
            StoreError::InvalidWorkProjection("an added task has no computed root".into())
        })
    }
}

/// Complete immutable-origin components selected by the applying owner.
/// Identity collisions use their separate global keyed lookup.
pub(super) struct OriginGraph {
    pub(super) documents: Vec<TaskDocument>,
    pub(super) notes_included: bool,
}

impl OriginGraph {
    fn read(
        transaction: &rusqlite::Transaction<'_>,
        project: &ProjectId,
    ) -> Result<Self, StoreError> {
        let origins = origin::all_origins(transaction)?;
        let mut notes_included = false;
        let mut documents = origins
            .into_iter()
            .filter(|origin| origin.project_id == *project)
            .map(|origin| {
                notes_included |= origin.notes_included;
                origin.task
            })
            .collect::<Vec<_>>();
        let endpoints = origin::native_endpoints(transaction, project, &documents)?;
        documents.extend(endpoints.into_iter().map(|endpoint| endpoint.task));
        Ok(Self {
            documents,
            notes_included,
        })
    }
}

/// Reads about held tasks, each made once per admission.
struct HeldReads {
    now: chrono::DateTime<chrono::Utc>,
    eligibility: HashMap<WorkId, Option<(&'static str, String)>>,
    removal_eligibility: HashMap<WorkId, Option<(&'static str, String)>>,
    executions: HashMap<WorkId, Option<String>>,
    restored_completions: HashMap<WorkId, bool>,
}

impl HeldReads {
    fn new(now: chrono::DateTime<chrono::Utc>) -> Self {
        Self {
            now,
            eligibility: HashMap::new(),
            removal_eligibility: HashMap::new(),
            executions: HashMap::new(),
            restored_completions: HashMap::new(),
        }
    }

    fn eligibility(
        &mut self,
        transaction: &rusqlite::Transaction<'_>,
        id: WorkId,
    ) -> Result<Option<(&'static str, String)>, StoreError> {
        if let Some(known) = self.eligibility.get(&id) {
            return Ok(known.clone());
        }
        let found =
            eligible(transaction, id, self.now)?.map(|conflict| (conflict.reason, conflict.detail));
        self.eligibility.insert(id, found.clone());
        Ok(found)
    }

    /// An active root execution holds planning only while a run is engaged.
    fn execution(
        &mut self,
        transaction: &rusqlite::Transaction<'_>,
        root: WorkId,
    ) -> Result<Option<String>, StoreError> {
        if let Some(known) = self.executions.get(&root) {
            return Ok(known.clone());
        }
        let mut found = None;
        if let Some(execution) = query::active_root_execution_optional(transaction, root)? {
            let mut statement = transaction.prepare(
                "SELECT work_id FROM work_runs WHERE root_execution_id = ?1
                 AND state NOT IN ('completed', 'cancelled', 'superseded_by_import')
                 ORDER BY work_id",
            )?;
            let work = statement
                .query_map([execution.root_execution_id.0.to_string()], |row| {
                    row.get::<_, String>(0)
                })?
                .collect::<Result<Vec<_>, _>>()?;
            for id in work {
                if engaged_run(transaction, query::parse_work_id(&id)?, self.now)?.is_some() {
                    found = Some(execution.root_execution_id.0.to_string());
                    break;
                }
            }
        }
        self.executions.insert(root, found.clone());
        Ok(found)
    }

    /// Whether the completion is restored history, including a later report,
    /// rather than a local seal that accounts for the task's children.
    fn restored_completion(
        &mut self,
        transaction: &rusqlite::Transaction<'_>,
        id: WorkId,
    ) -> Result<bool, StoreError> {
        if let Some(&known) = self.restored_completions.get(&id) {
            return Ok(known);
        }
        let item = query::load_work_item(transaction, id).map_err(origin::classify_binding)?;
        let restored = query::work_completed_by_restored_record_on(transaction, &item)?;
        self.restored_completions.insert(id, restored);
        Ok(restored)
    }
}

fn activity_keys(task: &TaskDocument) -> impl Iterator<Item = (String, String)> + '_ {
    task.activity.iter().flat_map(|(machine, entries)| {
        entries
            .keys()
            .map(move |id| (machine.clone(), id.as_str().to_owned()))
    })
}

/// Applies the admitted tasks of a classified folder. Every
/// requested delta is judged first, in two stages: each task on its own,
/// then each addition in the context of the prospective tree and the origin
/// history. One conflict is kept per task, in the precedence the module
/// describes. Every unapplied task is returned with its reason; only an
/// admitted task advances the caller's exchange baseline.
///
/// Admitted changes remove old relations, restore additions, apply scalars
/// and new relations, then dispose removals in hierarchy postorder. The caller checks
/// the result against the new membership and records it.
pub(super) fn apply_plan(
    transaction: &rusqlite::Transaction<'_>,
    plan: &DeltaPlan,
    current: &TaskTree,
    resolved: &TaskTree,
    actor: &ActorContext,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Application, StoreError> {
    let prospective = Prospective::new(current, resolved)?;
    let origins = if plan.additions().next().is_some() {
        Some(OriginGraph::read(transaction, prospective.project)?)
    } else {
        None
    };
    apply_on(
        transaction,
        plan,
        &prospective,
        origins.as_ref(),
        || {
            Ok(super::export_on(transaction, prospective.project)?
                .tasks
                .into_iter()
                .map(|task| (task.id, task))
                .collect())
        },
        || {
            if super::super::integrity::combined_graph_is_acyclic(
                transaction,
                &prospective.project.0,
            )? {
                Ok(())
            } else {
                Err(StoreError::InvalidWorkProjection(
                    "a folder relation change left a durable dependency cycle".into(),
                ))
            }
        },
        actor,
        now,
        true,
    )
}

/// One admission and ordered application implementation for full folders and
/// fresh connected scopes. The caller owns graph capture, snapshot validation
/// and the transaction; these inputs are not public admission observations.
/// Graph readers remain lazy so earlier per-task refusals keep their ordering.
#[allow(
    clippy::too_many_arguments,
    reason = "the applying owner supplies its two graph checks beside captured planning"
)]
pub(super) fn apply_on(
    transaction: &rusqlite::Transaction<'_>,
    plan: &DeltaPlan,
    prospective: &Prospective<'_>,
    origins: Option<&OriginGraph>,
    durable: impl FnOnce() -> Result<HashMap<WorkId, TaskDocument>, StoreError>,
    final_graph: impl FnOnce() -> Result<(), StoreError>,
    actor: &ActorContext,
    now: chrono::DateTime<chrono::Utc>,
    allow_partial: bool,
) -> Result<Application, StoreError> {
    let mut conflicts = BTreeMap::new();
    let mut batch = Vec::new();
    let mut reads = HeldReads::new(now);
    let additions = plan.additions().collect::<Vec<_>>();
    if !additions.is_empty() && origins.is_none() {
        return Err(StoreError::InvalidWorkProjection(
            "addition admission requires captured immutable-origin components".into(),
        ));
    }
    for task in &plan.tasks {
        if !matches!(task.delta, TaskDelta::Unchanged | TaskDelta::RetainedLocal) {
            let mut affected = HashSet::new();
            for document in prospective
                .current
                .get(&task.id)
                .into_iter()
                .chain(prospective.resolved.get(&task.id))
            {
                affected.insert(document.id);
                affected.extend(document.parent_id);
                affected.extend(document.prerequisites.iter().copied());
                affected.extend(document.superseded_by);
            }
            let mut affected = affected
                .into_iter()
                .filter(|id| prospective.current.contains_key(id))
                .collect::<Vec<_>>();
            affected.sort_by_key(|id| id.0);
            let mut held = None;
            for id in affected {
                if let Some(conflict) = reconciliation_conflict(transaction, id)? {
                    held = Some(Conflict {
                        id: task.id,
                        reason: conflict.reason,
                        detail: format!("affected task {}: {}", id.0, conflict.detail),
                    });
                    break;
                }
            }
            if let Some(conflict) = held {
                conflicts.insert(task.id.0, conflict);
                continue;
            }
        }
        let conflict = match &task.delta {
            TaskDelta::Deleted => removal_eligibility(transaction, &mut reads, task.id)?,
            TaskDelta::Changed(change) if !change.is_applicable() => {
                Some(unsupported_fields(task.id, change))
            }
            TaskDelta::Changed(change) if !change.relations.is_empty() => relations::admit_owner(
                transaction,
                prospective,
                &mut reads,
                task.id,
                &change.relations,
            )?,
            TaskDelta::Changed(change) if change.scalar.is_some() || change.reported_completion => {
                reads
                    .eligibility(transaction, task.id)?
                    .map(|(reason, detail)| Conflict {
                        id: task.id,
                        reason,
                        detail,
                    })
            }
            TaskDelta::Added => match origins {
                Some(_) => admit_addition(transaction, prospective, task.id)?,
                None => None,
            },
            TaskDelta::Unchanged | TaskDelta::RetainedLocal | TaskDelta::Changed(_) => None,
        };
        let conflict = if conflict.is_none()
            && matches!(&task.delta, TaskDelta::Changed(change) if change.reported_completion)
            && super::super::completion::live_descendant_execution_authority(
                transaction,
                task.id,
                now,
            )? {
            Some(Conflict {
                id: task.id,
                reason: "active_run",
                detail: "a descendant holds live execution authority".into(),
            })
        } else {
            conflict
        };
        let conflict = if conflict.is_none() {
            if let TaskDelta::Changed(change) = &task.delta
                && let Some(bindings) = change
                    .scalar
                    .as_ref()
                    .and_then(|change| change.acceptance_bindings.as_ref())
                && let Err(error) =
                    super::super::planning::refuse_record_id_pins_on(transaction, bindings)
            {
                Some(Conflict {
                    id: task.id,
                    reason: "acceptance_pin",
                    detail: error.to_string(),
                })
            } else {
                None
            }
        } else {
            conflict
        };
        if let Some(conflict) = conflict {
            conflicts.insert(conflict.id.0, conflict);
        }
    }
    let removals = plan
        .tasks
        .iter()
        .filter_map(|task| matches!(task.delta, TaskDelta::Deleted).then_some(task.id))
        .collect::<HashSet<_>>();
    // Apply admitted owner relation edits to the retained-reference view.
    // Unsupported changes must not hide a retained reference.
    // Keep ancestor reasons aside so a retained edge stays the more precise
    // graph-stage reason. A target's own first-stage refusal still wins.
    let mut bindings = BTreeMap::new();
    for &id in &removals {
        if !conflicts.contains_key(&id.0)
            && let Some(conflict) =
                removal_ancestor_binding(transaction, prospective, &mut reads, id)?
        {
            bindings.insert(id.0, conflict);
        }
    }
    let mut admitted_removals = removals
        .iter()
        .copied()
        .filter(|id| !conflicts.contains_key(&id.0) && !bindings.contains_key(&id.0))
        .collect::<HashSet<_>>();
    let mut retained = prospective
        .current
        .keys()
        .copied()
        .filter(|id| !admitted_removals.contains(id))
        .collect::<Vec<_>>();
    // Traverse retained seeds by task UUID, then newly retained removals
    // in FIFO discovery order. Reference fields/sets have stable order, so
    // the first encountered retained edge deterministically owns the detail.
    retained.sort_by_key(|id| id.0);
    let relation_owners = plan
        .relation_changes()
        .map(|(id, _)| id)
        .collect::<HashSet<_>>();
    let mut retained = std::collections::VecDeque::from(retained);
    while let Some(referrer) = retained.pop_front() {
        let task = if relation_owners.contains(&referrer) && !conflicts.contains_key(&referrer.0) {
            prospective.resolved_task(referrer)?
        } else {
            prospective.current[&referrer]
        };
        let references = task
            .parent_id
            .map(|id| ("parent_id", id))
            .into_iter()
            .chain(
                task.prerequisites
                    .iter()
                    .copied()
                    .map(|id| ("prerequisites", id)),
            )
            .chain(task.superseded_by.map(|id| ("superseded_by", id)));
        for (field, target) in references {
            if removals.contains(&target) {
                conflicts.entry(target.0).or_insert_with(|| Conflict {
                    id: target,
                    reason: "retained_reference",
                    detail: format!(
                        "retained task {} {field} references {}",
                        task.id.0, target.0
                    ),
                });
                // Refusing this removal retains its outgoing edges too.
                // Enqueue each newly retained task once, so chains of
                // refused deletions cannot strand another retained task.
                if admitted_removals.remove(&target) {
                    retained.push_back(target);
                }
            }
        }
    }
    for (id, conflict) in bindings {
        conflicts.entry(id).or_insert(conflict);
    }
    if origins.is_some() {
        for &id in &additions {
            if !conflicts.contains_key(&id.0)
                && let Some(conflict) = ancestor_binding(
                    transaction,
                    prospective,
                    &mut reads,
                    &relation_owners,
                    &conflicts,
                    id,
                )?
            {
                conflicts.insert(id.0, conflict);
            }
        }
        name_dependents_of_refusals(prospective, &additions, &mut conflicts);
    }
    let admitted = |conflicts: &BTreeMap<uuid::Uuid, Conflict>| {
        additions
            .iter()
            .copied()
            .filter(|id| !conflicts.contains_key(&id.0))
            .collect::<Vec<_>>()
    };
    if plan.relation_changes().next().is_none() && (!additions.is_empty() || !removals.is_empty()) {
        let retained = retained_graph(prospective, &admitted_removals, &admitted(&conflicts))?;
        absorb(retained, &mut conflicts, &mut batch);
        name_dependents_of_refusals(prospective, &additions, &mut conflicts);
    }
    if batch.is_empty()
        && let Some(origins) = origins
    {
        let origins = origin_graph(transaction, prospective, origins, &admitted(&conflicts))?;
        absorb(origins, &mut conflicts, &mut batch);
        name_dependents_of_refusals(prospective, &additions, &mut conflicts);
    }
    for (id, _) in plan.relation_changes() {
        if !conflicts.contains_key(&id.0)
            && let Some(conflict) =
                relations::ancestor_binding(transaction, prospective, &mut reads, id)?
        {
            conflicts.insert(id.0, conflict);
        }
    }
    let durable = if plan.relation_changes().next().is_some() {
        Some(durable()?)
    } else {
        None
    };
    if !plan.project.is_empty() || !batch.is_empty() || (!allow_partial && !conflicts.is_empty()) {
        let conflicts = conflicts.into_values().collect::<Vec<_>>();
        return Err(
            TaskSyncRefusal::ImportConflict.invalid(conflict_message_with(
                &plan.project,
                &batch,
                &conflicts,
            )),
        );
    }
    // Refused values never become the basis of an admitted dependent. Check
    // the actual mixed graph, retaining each refused task's local document.
    // Each pass sets aside at least one requested change, so this terminates.
    let candidates = plan
        .tasks
        .iter()
        .filter(|task| !task.is_unchanged())
        .map(|task| task.id)
        .collect::<Vec<_>>();
    loop {
        name_dependents_of_refusals(prospective, &candidates, &mut conflicts);
        let selected = selected_plan(plan, &conflicts);
        let effective = effective_documents(&selected, prospective)?;
        let mut refused_endpoint = false;
        for (id, change) in selected.relation_changes() {
            for target in &change.prerequisite_additions {
                if let Some(endpoint) = effective.get(target) {
                    // A shared edge can have been added while its target was
                    // Open, then returned after that target ended. Preserve
                    // that history only when the incoming and effective
                    // lifecycles agree; a refused promotion cannot supply an
                    // Open endpoint that this store still holds terminal.
                    let terminal_history = matches!(
                        endpoint.lifecycle,
                        WorkLifecycle::Completed
                            | WorkLifecycle::Cancelled
                            | WorkLifecycle::Superseded
                    ) && prospective.resolved_task(*target)?.lifecycle
                        == endpoint.lifecycle;
                    if endpoint.lifecycle == WorkLifecycle::Open || terminal_history {
                        continue;
                    }
                    conflicts.insert(
                        id.0,
                        Conflict {
                            id,
                            reason: if endpoint.lifecycle == WorkLifecycle::Completed {
                                "already_satisfied"
                            } else {
                                "prerequisite_lifecycle"
                            },
                            detail: format!(
                                "prerequisite {} is {:?}",
                                target.0, endpoint.lifecycle
                            ),
                        },
                    );
                    refused_endpoint = true;
                    break;
                }
            }
        }
        if refused_endpoint {
            continue;
        }
        let mut unfinished_child = false;
        for task in &selected.tasks {
            if !matches!(&task.delta, TaskDelta::Changed(change) if change.reported_completion) {
                continue;
            }
            if let Some(child) = effective
                .values()
                .filter(|child| {
                    child.parent_id == Some(task.id)
                        && child.child_requirement == crate::ChildRequirement::Required
                        && matches!(
                            child.lifecycle,
                            WorkLifecycle::Open | WorkLifecycle::Proposed
                        )
                })
                .min_by_key(|child| child.id.0)
            {
                conflicts.insert(
                    task.id.0,
                    Conflict {
                        id: task.id,
                        reason: "required_child_unfinished",
                        detail: format!("required child {} remains unfinished", child.id.0),
                    },
                );
                unfinished_child = true;
            }
        }
        if unfinished_child {
            continue;
        }
        // A later owner refusal restores its current edges. Recheck removal
        // targets against that actual mixed graph, including chains of targets
        // retained by an earlier pass, before locating any remaining fault.
        let deleted = selected
            .tasks
            .iter()
            .filter_map(|task| matches!(task.delta, TaskDelta::Deleted).then_some(task.id))
            .collect::<HashSet<_>>();
        let mut retained_removal = false;
        for task in effective.values() {
            for target in origin::native_references(task).filter(|id| deleted.contains(id)) {
                conflicts.entry(target.0).or_insert_with(|| Conflict {
                    id: target,
                    reason: "retained_reference",
                    detail: format!("retained task {} references {}", task.id.0, target.0),
                });
                retained_removal = true;
            }
        }
        if retained_removal {
            continue;
        }
        // A refused move retains its current ancestry. Compare incoming
        // roots only for tasks whose changes will actually be applied.
        let mut expected_roots = prospective.current_roots.clone();
        for task in selected.tasks.iter().filter(|task| !task.is_unchanged()) {
            if let Some(root) = prospective.roots.get(&task.id) {
                expected_roots.insert(task.id, *root);
            }
        }
        let Some(fault) = locate_fault(
            prospective.project,
            prospective.notes_included,
            effective.into_values().collect(),
            &expected_roots,
            count_retained_validation,
        )?
        else {
            // Name visible retained-graph faults before checking additional
            // paths through retired work in the durable graph.
            if let Some(durable) = &durable {
                let before = conflicts.len();
                relations::graphs(&selected, prospective, &mut conflicts, &mut batch, durable)?;
                if !batch.is_empty() {
                    return Err(
                        TaskSyncRefusal::ImportConflict.invalid(conflict_message_with(
                            &plan.project,
                            &batch,
                            &conflicts.into_values().collect::<Vec<_>>(),
                        )),
                    );
                }
                if conflicts.len() != before {
                    continue;
                }
            }
            break;
        };
        let mut named = false;
        if let Some(located) = fault.located {
            for task in selected.tasks.iter().filter(|task| !task.is_unchanged()) {
                if located.groups.contains_key(&task.id) {
                    conflicts.insert(
                        task.id.0,
                        Conflict {
                            id: task.id,
                            reason: "retained_graph",
                            detail: fault.diagnostic.clone(),
                        },
                    );
                    named = true;
                }
            }
        }
        if !named {
            return Err(StoreError::InvalidWorkProjection(format!(
                "cannot locate a partial import graph refusal: {}",
                fault.diagnostic
            )));
        }
    }
    if !allow_partial && !conflicts.is_empty() {
        return Err(
            TaskSyncRefusal::ImportConflict.invalid(conflict_message_with(
                &plan.project,
                &[],
                &conflicts.into_values().collect::<Vec<_>>(),
            )),
        );
    }
    let selected = selected_plan(plan, &conflicts);
    let tasks = plan
        .tasks
        .iter()
        .filter(|task| !task.is_unchanged())
        .map(|task| {
            let conflict = conflicts.get(&task.id.0);
            super::TaskSyncTaskResult {
                task: task.id,
                status: match conflict {
                    None => super::TaskSyncTaskStatus::Applied,
                    Some(conflict)
                        if matches!(
                            conflict.reason,
                            "active_run"
                                | "task_sync_reconciliation_held"
                                | "ancestor_binding"
                                | "refused_reference"
                        ) =>
                    {
                        super::TaskSyncTaskStatus::Held
                    }
                    Some(_) => super::TaskSyncTaskStatus::Refused,
                },
                reason: conflict.map(|conflict| conflict.reason.to_owned()),
                detail: conflict.map(|conflict| conflict.detail.clone()),
            }
        })
        .collect();
    let plan = &selected;
    let additions = plan.additions().collect::<Vec<_>>();
    let removals = plan
        .tasks
        .iter()
        .filter_map(|task| matches!(task.delta, TaskDelta::Deleted).then_some(task.id))
        .collect::<HashSet<_>>();
    relations::apply_removals(transaction, plan, actor, now)?;
    if !additions.is_empty() {
        // Additions may reference one another in any order; the caller
        // checks every foreign key once they are all in place.
        transaction.execute_batch("PRAGMA defer_foreign_keys = ON")?;
        for &id in &additions {
            let task = prospective.resolved_task(id)?;
            let snapshot = prospective.snapshots.get(&id).cloned().ok_or_else(|| {
                StoreError::InvalidWorkProjection("an added task has no snapshot".into())
            })?;
            super::insert_task(
                transaction,
                prospective.project_document,
                task,
                snapshot,
                actor,
                now,
            )?;
        }
    }
    for (id, change) in plan.scalar_changes() {
        super::super::planning::apply_folder_scalar_revision_on(
            transaction,
            id,
            change,
            actor,
            now,
        )?;
    }
    relations::apply_additions(transaction, plan, actor, now)?;
    if plan.relation_changes().next().is_some() {
        final_graph()?;
    }
    // Children close before their parent; no local child seal is fabricated.
    let mut completions = plan
        .tasks
        .iter()
        .filter_map(|task| match &task.delta {
            TaskDelta::Changed(change) if change.reported_completion => Some(task.id),
            _ => None,
        })
        .collect::<Vec<_>>();
    completions.sort_by_key(|id| (std::cmp::Reverse(prospective.ancestors(*id).len()), id.0));
    for id in completions {
        super::reported::apply_on(
            transaction,
            prospective.resolved_task(id)?,
            prospective.snapshots.get(&id).cloned().ok_or_else(|| {
                StoreError::InvalidWorkProjection("reported task has no snapshot".into())
            })?,
            actor,
            now,
        )?;
    }
    let mut ordered = removals
        .into_iter()
        .map(|id| {
            let mut depth = 0_usize;
            let mut cursor = prospective.current.get(&id).and_then(|task| task.parent_id);
            while let Some(parent) = cursor {
                depth += 1;
                cursor = prospective
                    .current
                    .get(&parent)
                    .and_then(|task| task.parent_id);
            }
            (std::cmp::Reverse(depth), id)
        })
        .collect::<Vec<_>>();
    ordered.sort_by_key(|&(depth, id)| (depth, id.0));
    for (_, id) in ordered {
        // No earlier phase mutates this disjoint set. Recheck at the writer
        // boundary rather than relying on a run-free field alone.
        let admitted = removal_item(transaction, id, now)?;
        super::super::planning::apply_folder_disposal_on(transaction, admitted, actor, now)?;
    }
    Ok(Application {
        plan: selected,
        tasks,
    })
}

fn selected_plan(plan: &DeltaPlan, conflicts: &BTreeMap<uuid::Uuid, Conflict>) -> DeltaPlan {
    let mut selected = plan.clone();
    for task in &mut selected.tasks {
        if conflicts.contains_key(&task.id.0) {
            task.delta = TaskDelta::Unchanged;
        }
    }
    selected
}

fn effective_documents(
    plan: &DeltaPlan,
    prospective: &Prospective<'_>,
) -> Result<HashMap<WorkId, TaskDocument>, StoreError> {
    let mut effective = prospective
        .current
        .iter()
        .map(|(&id, &task)| (id, task.clone()))
        .collect::<HashMap<_, _>>();
    for task in &plan.tasks {
        match task.delta {
            TaskDelta::Changed(_) | TaskDelta::Added => {
                effective.insert(task.id, prospective.resolved_task(task.id)?.clone());
            }
            TaskDelta::Deleted => {
                effective.remove(&task.id);
            }
            TaskDelta::Unchanged | TaskDelta::RetainedLocal => {}
        }
    }
    Ok(effective)
}

/// Deletion requires Open work and adds stricter history checks without
/// changing scalar admission of Proposed work.
fn removal_eligibility(
    transaction: &rusqlite::Transaction<'_>,
    reads: &mut HeldReads,
    id: WorkId,
) -> Result<Option<Conflict>, StoreError> {
    if let Some(known) = reads.removal_eligibility.get(&id) {
        return Ok(known
            .clone()
            .map(|(reason, detail)| Conflict { id, reason, detail }));
    }
    let found = if let Some((mut reason, mut detail)) = reads.eligibility(transaction, id)? {
        if reason == "restored_completion" {
            let sealed: bool = transaction.query_row(
                "SELECT EXISTS(SELECT 1 FROM work_completion_seals WHERE work_id = ?1)",
                [id.0.to_string()],
                |row| row.get(0),
            )?;
            if sealed {
                reason = "sealed_completion";
                detail = "the task was completed and sealed on this host".into();
            }
        }
        Some(Conflict { id, reason, detail })
    } else {
        removal_history(transaction, id)?
    };
    reads.removal_eligibility.insert(
        id,
        found
            .as_ref()
            .map(|conflict| (conflict.reason, conflict.detail.clone())),
    );
    Ok(found)
}

fn removal_history(
    transaction: &rusqlite::Transaction<'_>,
    id: WorkId,
) -> Result<Option<Conflict>, StoreError> {
    let (lifecycle, prior): (String, bool) = transaction.query_row(
        "SELECT lifecycle, EXISTS(SELECT 1 FROM work_runs WHERE work_id = ?1)
         FROM work_items WHERE work_id = ?1",
        [id.0.to_string()],
        |row| Ok((row.get(0)?, row.get(1)?)),
    )?;
    if lifecycle != "open" {
        return Ok(Some(Conflict {
            id,
            reason: "lifecycle",
            detail: "only Open tasks can be removed".into(),
        }));
    }
    if prior {
        return Ok(Some(Conflict {
            id,
            reason: "execution_history",
            detail: "a persisted run exists".into(),
        }));
    }
    Ok(None)
}

fn removal_item(
    transaction: &rusqlite::Transaction<'_>,
    id: WorkId,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<crate::WorkItem, StoreError> {
    if let Some(conflict) = removal_eligibility(transaction, &mut HeldReads::new(now), id)? {
        return Err(StoreError::InvalidWorkProjection(format!(
            "a folder removal lost admission: {} ({})",
            conflict.reason, conflict.detail
        )));
    }
    query::load_work_item(transaction, id)
}

fn removal_ancestor_binding(
    transaction: &rusqlite::Transaction<'_>,
    prospective: &Prospective<'_>,
    reads: &mut HeldReads,
    id: WorkId,
) -> Result<Option<Conflict>, StoreError> {
    let mut cursor = prospective.current.get(&id).and_then(|task| task.parent_id);
    let mut seen = HashSet::new();
    while let Some(parent) = cursor {
        if !seen.insert(parent) {
            return Err(StoreError::InvalidWorkProjection(
                "cyclic removal ancestry".into(),
            ));
        }
        if let Some(conflict) = removal_eligibility(transaction, reads, parent)? {
            return Ok(Some(Conflict {
                id,
                reason: "ancestor_binding",
                detail: format!(
                    "ancestor {}: {} ({})",
                    parent.0, conflict.reason, conflict.detail
                ),
            }));
        }
        cursor = prospective
            .current
            .get(&parent)
            .and_then(|task| task.parent_id);
    }
    let root = prospective.current_roots.get(&id).copied().ok_or_else(|| {
        StoreError::InvalidWorkProjection("a removed task has no computed current root".into())
    })?;
    if let Some(execution) = reads.execution(transaction, root)? {
        return Ok(Some(Conflict {
            id,
            reason: "ancestor_binding",
            detail: format!("root {} has active root execution {}", root.0, execution),
        }));
    }
    Ok(None)
}

fn unsupported_fields(id: WorkId, change: &TaskChange) -> Conflict {
    Conflict {
        id,
        reason: "unsupported_fields",
        detail: change
            .unsupported
            .iter()
            .copied()
            .map(TaskField::as_str)
            .collect::<Vec<_>>()
            .join(", "),
    }
}

/// Descriptions belong to the shared task, regardless of its creation host
/// or past execution. Only engaged local execution holds an incoming change.
fn eligible(
    transaction: &rusqlite::Transaction<'_>,
    id: WorkId,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Option<Conflict>, StoreError> {
    #[cfg(test)]
    ELIGIBILITY_READS.with(|count| count.set(count.get() + 1));
    let conflict = |reason: &'static str, detail: String| -> Result<Option<Conflict>, StoreError> {
        Ok(Some(Conflict { id, reason, detail }))
    };
    let item = query::load_work_item(transaction, id).map_err(origin::classify_binding)?;
    if let Some(held) = reconciliation_conflict(transaction, id)? {
        return Ok(Some(held));
    }
    match item.lifecycle {
        WorkLifecycle::Open | WorkLifecycle::Proposed => {}
        WorkLifecycle::Completed => {
            return conflict("restored_completion", "the task is completed".into());
        }
        other => {
            return conflict("lifecycle", format!("the task is {other:?}"));
        }
    }
    if let Some(run) = engaged_run(transaction, id, now)? {
        return conflict("active_run", format!("run {}", run.0));
    }
    Ok(None)
}

pub(super) fn engaged_run(
    connection: &rusqlite::Connection,
    id: WorkId,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<Option<crate::WorkRunId>, StoreError> {
    let item = query::load_work_item(connection, id)?;
    let Some(run) = item.active_run_id else {
        return Ok(None);
    };
    if super::super::execution::live_handoff_on(connection, run, now)?.is_some() {
        return Ok(Some(run));
    }
    if let Some(claim) = query::load_work_claim_optional(connection, run)? {
        return Ok(
            (claim.state == crate::WorkClaimState::Active && claim.expires_at > now).then_some(run),
        );
    }
    Ok(
        query::canonical_work_event_records_for_item(connection, id)?
            .into_iter()
            .any(|(_, event)| event.run_id == Some(run) && !is_planning(&event.transition))
            .then_some(run),
    )
}

fn reconciliation_conflict(
    connection: &rusqlite::Connection,
    id: WorkId,
) -> Result<Option<Conflict>, StoreError> {
    Ok(
        super::reconciliation::holds::held_by_on(connection, id)?.map(|candidate| Conflict {
            id,
            reason: "task_sync_reconciliation_held",
            detail: format!("held by reconciliation candidate {candidate}"),
        }),
    )
}

/// Whether a transition changes only planning, never execution. Every
/// variant is named, so a new one cannot compile until it is classified.
const fn is_planning(transition: &WorkTransition) -> bool {
    match transition {
        WorkTransition::Created { .. }
        | WorkTransition::Reopened { .. }
        | WorkTransition::RequiredChildWaived { .. }
        | WorkTransition::Decomposed { .. }
        | WorkTransition::Revised { .. }
        | WorkTransition::PrerequisiteAdded { .. }
        | WorkTransition::PrerequisiteRemoved { .. }
        | WorkTransition::Blocked { .. }
        | WorkTransition::Unblocked { .. }
        | WorkTransition::Disposed { .. } => true,
        // Execution transitions are distinct from administrative planning.
        WorkTransition::Claimed { .. }
        | WorkTransition::ClaimRenewed { .. }
        | WorkTransition::Released { .. }
        | WorkTransition::Checkpointed { .. }
        | WorkTransition::HandoffOffered { .. }
        | WorkTransition::HandoffExpired { .. }
        | WorkTransition::HandoffCancelled { .. }
        | WorkTransition::HandedOff { .. }
        | WorkTransition::EvidenceAdded { .. }
        | WorkTransition::MemoryCaptured { .. }
        | WorkTransition::TypedEvidenceAdded { .. }
        | WorkTransition::Completed { .. }
        | WorkTransition::CompletionReported { .. }
        | WorkTransition::SupersededByImport { .. }
        | WorkTransition::ReconciliationHoldAbandoned { .. } => false,
    }
}

/// Whether a task in this lifecycle is work still to be done. Every
/// lifecycle is named, so a new one cannot compile until it is classified.
const fn unfinished(lifecycle: WorkLifecycle) -> bool {
    match lifecycle {
        WorkLifecycle::Proposed | WorkLifecycle::Open => true,
        WorkLifecycle::Completed | WorkLifecycle::Cancelled | WorkLifecycle::Superseded => false,
    }
}

/// The first stage for an addition: every identity it brings is new to this
/// store and its record-id pins are refused here rather than during insertion.
fn admit_addition(
    transaction: &rusqlite::Transaction<'_>,
    prospective: &Prospective<'_>,
    id: WorkId,
) -> Result<Option<Conflict>, StoreError> {
    let collision = |detail: String| -> Result<Option<Conflict>, StoreError> {
        Ok(Some(Conflict {
            id,
            reason: "id_collision",
            detail,
        }))
    };
    let task = prospective.resolved_task(id)?;
    let held = |sql: &str, value: &str, extra: Option<&str>| -> Result<bool, StoreError> {
        Ok(match extra {
            Some(project) => transaction.query_row(sql, [value, project], |row| row.get(0))?,
            None => transaction.query_row(sql, [value], |row| row.get(0))?,
        })
    };
    if held(
        "SELECT EXISTS(SELECT 1 FROM work_items WHERE work_id = ?1)",
        &id.0.to_string(),
        None,
    )? {
        return collision("the work id is already held by this store".into());
    }
    let short_ref = task.snapshot_with_root(prospective.root(id)?).short_ref;
    if held(
        "SELECT EXISTS(SELECT 1 FROM work_items WHERE short_ref = ?1 AND project_id = ?2)",
        &short_ref,
        Some(prospective.project.0.as_str()),
    )? {
        return collision(format!(
            "short ref {short_ref} is already held in this project"
        ));
    }
    for blocker in &task.blockers {
        if held(
            "SELECT EXISTS(SELECT 1 FROM work_blockers WHERE blocker_id = ?1)",
            &blocker.blocker_id,
            None,
        )? {
            return collision(format!("blocker {} is already held", blocker.blocker_id));
        }
    }
    for note in &task.notes {
        if super::closure::graph::identities::original_held(
            transaction,
            "note",
            "",
            note.id.as_str(),
        )? || held(
            "SELECT EXISTS(SELECT 1 FROM objects WHERE object_id = ?1)",
            note.id.as_str(),
            None,
        )? {
            return collision(format!("note {} is already held", note.id.as_str()));
        }
    }
    for key in activity_keys(task) {
        // This host reports its native activity, in any project and under
        // whatever alias an export asserts, by the id of the event that
        // recorded the claim; like a note id, an activity id that names a
        // record this store holds is held.
        if super::closure::graph::identities::original_held(
            transaction,
            "activity",
            &key.0,
            &key.1,
        )? || held(
            "SELECT EXISTS(SELECT 1 FROM objects WHERE object_id = ?1)",
            &key.1,
            None,
        )? {
            return collision(format!("activity {}/{} is already held", key.0, key.1));
        }
    }
    if let Err(error) =
        super::super::planning::refuse_record_id_pins_on(transaction, &task.acceptance_bindings)
    {
        return Ok(Some(Conflict {
            id,
            reason: "acceptance_pin",
            detail: error.to_string(),
        }));
    }
    Ok(None)
}

/// The second stage for an addition: no ancestor in its prospective chain is
/// bound against taking new work, and its root has no active execution.
/// Unfinished work needs every ancestor Open, and a held Open ancestor must
/// itself be free to change. A finished addition, such as a completed
/// subtree restored as history, may sit under a completed ancestor only when
/// that completion was itself restored: a completion sealed on this host
/// accounts for exactly the children it named, so it takes no new one. A
/// held ancestor whose own change in the folder is already refused, such as
/// a move to another parent, refuses the addition too: its chain and root
/// rest on that change.
fn ancestor_binding(
    transaction: &rusqlite::Transaction<'_>,
    prospective: &Prospective<'_>,
    reads: &mut HeldReads,
    relation_owners: &HashSet<WorkId>,
    conflicts: &BTreeMap<uuid::Uuid, Conflict>,
    id: WorkId,
) -> Result<Option<Conflict>, StoreError> {
    let binding = |detail: String| -> Result<Option<Conflict>, StoreError> {
        Ok(Some(Conflict {
            id,
            reason: "ancestor_binding",
            detail,
        }))
    };
    let unfinished = unfinished(prospective.resolved_task(id)?.lifecycle);
    for ancestor in prospective.ancestors(id) {
        if let Some(held) = prospective.current.get(&ancestor) {
            if let Some(refused) = conflicts.get(&ancestor.0) {
                return binding(format!(
                    "held ancestor {} is refused ({})",
                    ancestor.0, refused.reason
                ));
            }
            if relation_owners.contains(&ancestor) {
                return binding(format!(
                    "ancestor {} has a relation change in this folder",
                    ancestor.0
                ));
            }
            if held.lifecycle == WorkLifecycle::Completed {
                if unfinished {
                    return binding(format!("ancestor {} is completed", ancestor.0));
                }
                if !reads.restored_completion(transaction, ancestor)? {
                    return binding(format!(
                        "ancestor {} was completed on this host",
                        ancestor.0
                    ));
                }
                continue;
            }
            if let Some((reason, detail)) = reads.eligibility(transaction, ancestor)? {
                return binding(format!("ancestor {}: {reason} ({detail})", ancestor.0));
            }
        }
        // An entirely new incoming subtree is reported state, including an
        // open descendant of a cancelled or completed remote ancestor. It
        // creates no execution authority. Existing local ancestors retain
        // their lifecycle and execution guards above.
    }
    let root = prospective.root(id)?;
    if prospective.current.contains_key(&root)
        && let Some(execution) = reads.execution(transaction, root)?
    {
        return binding(format!(
            "root {} has active root execution {execution}",
            root.0
        ));
    }
    Ok(None)
}

/// Names each addition that depends on a refused addition: through its
/// parent chain as `ancestor_binding`, through a prerequisite or replacement
/// as `refused_reference`. Refusal spreads along those references once, in
/// one breadth-first pass over the additions.
fn name_dependents_of_refusals(
    prospective: &Prospective<'_>,
    additions: &[WorkId],
    conflicts: &mut BTreeMap<uuid::Uuid, Conflict>,
) {
    let dependents = dependents_among(prospective, additions);
    let mut queue = conflicts
        .values()
        // A refused edit/deletion leaves a real local endpoint in place.
        // It does not make every task referencing that endpoint unimportable.
        // New identities that were never admitted do require their dependents
        // to wait. Ancestry changes have their separate binding check above.
        .filter(|conflict| !prospective.current.contains_key(&conflict.id))
        .map(|conflict| conflict.id)
        .collect::<std::collections::VecDeque<_>>();
    while let Some(refused) = queue.pop_front() {
        for &(dependent, through_parent) in dependents.get(&refused).into_iter().flatten() {
            if conflicts.contains_key(&dependent.0) {
                continue;
            }
            let conflict = if through_parent {
                Conflict {
                    id: dependent,
                    reason: "ancestor_binding",
                    detail: format!("added ancestor {} is refused", refused.0),
                }
            } else {
                Conflict {
                    id: dependent,
                    reason: "refused_reference",
                    detail: format!("it references added task {}, which is refused", refused.0),
                }
            };
            conflicts.insert(dependent.0, conflict);
            if !prospective.current.contains_key(&dependent) {
                queue.push_back(dependent);
            }
        }
    }
}

/// What a graph check found: the additions it names, and the refusals it
/// could not pin on any addition, each as a folder-level reason.
#[derive(Default)]
struct GraphFindings {
    conflicts: Vec<Conflict>,
    batch: Vec<(&'static str, String)>,
}

/// Each addition another addition depends on, with those dependents: `true`
/// for a child through its parent, `false` for a prerequisite or a
/// replacement.
fn dependents_among(
    prospective: &Prospective<'_>,
    additions: &[WorkId],
) -> HashMap<WorkId, Vec<(WorkId, bool)>> {
    let mut dependents = HashMap::<WorkId, Vec<(WorkId, bool)>>::new();
    for &id in additions {
        let Some(task) = prospective.resolved.get(&id) else {
            continue;
        };
        if let Some(parent) = task.parent_id {
            dependents.entry(parent).or_default().push((id, true));
        }
        for reference in task.prerequisites.iter().copied().chain(task.superseded_by) {
            dependents.entry(reference).or_default().push((id, false));
        }
    }
    dependents
}

/// The kinds of failure a graph check can pin on tasks, in the order the
/// validator checks them.
#[derive(Clone, Copy)]
enum FaultClass {
    MissingReference,
    OpenDescendantLimit,
    DependencyCycle,
    DifferingRoot,
}

impl FaultClass {
    /// Why a named addition fails in `what`, alone or `joint`ly with the
    /// other named additions that share its fault. A differing root is found
    /// only in a graph that is otherwise valid, so it never claims an
    /// invalid graph.
    fn detail(self, what: &str, joint: bool) -> String {
        let invalid = |reason: &str| {
            if joint {
                format!(
                    "{what} with this and the other named additions would not be a valid task graph: {reason}"
                )
            } else {
                format!("{what} with this addition would not be a valid task graph: {reason}")
            }
        };
        match (self, joint) {
            (Self::MissingReference, false) => invalid("it references a missing task"),
            (Self::MissingReference, true) => invalid("each references a missing task"),
            (Self::OpenDescendantLimit, false) => {
                invalid("it takes its root over the open-descendant limit")
            }
            (Self::OpenDescendantLimit, true) => {
                invalid("together they take their root over the open-descendant limit")
            }
            (Self::DependencyCycle, false) => invalid("it closes a dependency cycle"),
            (Self::DependencyCycle, true) => invalid("together they close a dependency cycle"),
            (Self::DifferingRoot, _) => format!(
                "{what} with this addition is a valid task graph, but this addition's computed root there differs from the folder's"
            ),
        }
    }
}

/// The tasks a located failure involves, each with the group of tasks that
/// share one fault: a dependency cycle's members, or a root's open
/// descendants. A task whose fault is its own has a group of its own.
struct Located {
    class: FaultClass,
    groups: HashMap<WorkId, usize>,
}

/// Where a graph's validation failed: the tasks the failure involves, when
/// they can be located, and the validator's own diagnostic.
struct Fault {
    located: Option<Located>,
    diagnostic: String,
}

/// A group of its own for each task.
fn own_groups(tasks: impl IntoIterator<Item = WorkId>) -> HashMap<WorkId, usize> {
    tasks
        .into_iter()
        .enumerate()
        .map(|(group, id)| (id, group))
        .collect()
}

/// Validates the documents as one task graph and, when it is not valid,
/// locates the tasks the failure involves, trying each kind of failure in
/// the validator's order: the holders of a reference to a missing task; the
/// open descendants of a root over its limit; the members of a dependency
/// cycle (a required child for its parent, a prerequisite, a replacement).
/// A valid graph can still fail when an added task's computed root differs
/// from the folder's. A failure none of these explain is not located.
fn locate_fault(
    project: &ProjectId,
    notes_included: bool,
    mut tasks: Vec<TaskDocument>,
    expected_roots: &HashMap<WorkId, WorkId>,
    count: fn(),
) -> Result<Option<Fault>, StoreError> {
    count();
    tasks.sort_by_key(|task| task.id.0);
    let tree = TaskTree {
        project: ProjectDocument {
            format: FORMAT.into(),
            project_id: project.clone(),
            notes_included,
        },
        tasks,
    };
    let diagnostic = match tree.validate_stored() {
        Ok(()) => {
            let items = tree.snapshots()?;
            let differing = items
                .iter()
                .filter(|item| {
                    expected_roots
                        .get(&item.work_id)
                        .is_some_and(|root| *root != item.root_id)
                })
                .map(|item| item.work_id)
                .collect::<HashSet<_>>();
            if differing.is_empty() {
                return Ok(None);
            }
            return Ok(Some(Fault {
                located: Some(Located {
                    class: FaultClass::DifferingRoot,
                    groups: own_groups(differing),
                }),
                diagnostic: "an added task's root differs from the folder's".into(),
            }));
        }
        Err(error) => error.to_string(),
    };
    let located = [
        (
            FaultClass::MissingReference,
            missing_references(&tree.tasks),
        ),
        (
            FaultClass::OpenDescendantLimit,
            over_limit_descendants(&tree),
        ),
        (FaultClass::DependencyCycle, dependency_cycles(&tree.tasks)),
    ]
    .into_iter()
    .find(|(_, groups)| !groups.is_empty())
    .map(|(class, groups)| Located { class, groups });
    Ok(Some(Fault {
        located,
        diagnostic,
    }))
}

/// The tasks holding a parent, prerequisite or replacement reference to a
/// task the documents do not hold, each in a group of its own.
fn missing_references(tasks: &[TaskDocument]) -> HashMap<WorkId, usize> {
    let ids = tasks.iter().map(|task| task.id).collect::<HashSet<_>>();
    own_groups(
        tasks
            .iter()
            .filter(|task| {
                task.parent_id
                    .into_iter()
                    .chain(task.prerequisites.iter().copied())
                    .chain(task.superseded_by)
                    .any(|reference| !ids.contains(&reference))
            })
            .map(|task| task.id),
    )
}

/// The open descendants of every root whose open descendants exceed the
/// work limit, grouped by root.
fn over_limit_descendants(tree: &TaskTree) -> HashMap<WorkId, usize> {
    let Ok(items) = tree.snapshots() else {
        return HashMap::new();
    };
    let mut open = HashMap::<WorkId, Vec<WorkId>>::new();
    for item in &items {
        if item.work_id != item.root_id && unfinished(item.lifecycle) {
            open.entry(item.root_id).or_default().push(item.work_id);
        }
    }
    open.into_values()
        .filter(|descendants| {
            descendants.len() > crate::storage::work::MAX_OPEN_WORK_DESCENDANTS as usize
        })
        .enumerate()
        .flat_map(|(group, descendants)| descendants.into_iter().map(move |id| (id, group)))
        .collect()
}

/// The members of every dependency cycle of the combined graph, grouped by
/// strongly connected component: a component of more than one task, or a
/// task that depends on itself. A task on a path between two cycles belongs
/// to neither. The components come from one iterative pass of Tarjan's
/// algorithm, linear in the tasks and their references.
pub(super) fn dependency_cycles(tasks: &[TaskDocument]) -> HashMap<WorkId, usize> {
    const UNVISITED: usize = usize::MAX;
    let position = tasks
        .iter()
        .enumerate()
        .map(|(index, task)| (task.id, index))
        .collect::<HashMap<_, _>>();
    let mut edges = vec![Vec::new(); tasks.len()];
    for (index, task) in tasks.iter().enumerate() {
        if task.child_requirement == crate::ChildRequirement::Required
            && let Some(&parent) = task.parent_id.as_ref().and_then(|id| position.get(id))
        {
            edges[parent].push(index);
        }
        edges[index].extend(
            task.prerequisites
                .iter()
                .chain(task.superseded_by.as_ref())
                .filter_map(|id| position.get(id).copied()),
        );
    }
    let mut order = vec![UNVISITED; tasks.len()];
    let mut low = vec![0; tasks.len()];
    let mut on_stack = vec![false; tasks.len()];
    let mut stack = Vec::new();
    let mut calls = Vec::<(usize, usize)>::new();
    let mut visited = 0;
    let mut groups = HashMap::new();
    let mut group = 0;
    for start in 0..tasks.len() {
        if order[start] != UNVISITED {
            continue;
        }
        order[start] = visited;
        low[start] = visited;
        visited += 1;
        stack.push(start);
        on_stack[start] = true;
        calls.push((start, 0));
        while let Some(frame) = calls.last_mut() {
            let node = frame.0;
            if let Some(&next) = edges[node].get(frame.1) {
                frame.1 += 1;
                if order[next] == UNVISITED {
                    order[next] = visited;
                    low[next] = visited;
                    visited += 1;
                    stack.push(next);
                    on_stack[next] = true;
                    calls.push((next, 0));
                } else if on_stack[next] {
                    low[node] = low[node].min(order[next]);
                }
                continue;
            }
            calls.pop();
            if let Some(&(caller, _)) = calls.last() {
                low[caller] = low[caller].min(low[node]);
            }
            if low[node] != order[node] {
                continue;
            }
            let mut component = Vec::new();
            while let Some(member) = stack.pop() {
                on_stack[member] = false;
                component.push(member);
                if member == node {
                    break;
                }
            }
            if component.len() > 1 || edges[node].contains(&node) {
                for member in component {
                    groups.insert(tasks[member].id, group);
                }
                group += 1;
            }
        }
    }
    groups
}

/// How many times one graph check may locate a fault and set its additions
/// aside before naming the rest at folder level.
const FAULT_PASSES: usize = 3;

/// Checks a base graph with the admitted additions, in bounded work. The
/// base alone is validated first; a base that fails without any addition is
/// a folder-level refusal that names no addition. Otherwise each pass
/// locates the fault, names the additions among its tasks, and sets them
/// aside together with every addition that depends on one of them, which the
/// caller then names for that dependence. A fault that involves no addition,
/// or remains after the last pass, is a folder-level refusal naming the
/// additions together.
#[allow(
    clippy::too_many_arguments,
    reason = "a graph check takes its base, its context and its reason"
)]
fn check_graph(
    prospective: &Prospective<'_>,
    project: &ProjectId,
    notes_included: bool,
    base: &[&TaskDocument],
    additions: &[WorkId],
    reason: &'static str,
    what: &str,
    count: fn(),
    native_connection: Option<&rusqlite::Connection>,
) -> Result<GraphFindings, StoreError> {
    let mut findings = GraphFindings::default();
    let base_documents = || base.iter().map(|&task| task.clone()).collect::<Vec<_>>();
    if let Some(fault) = locate_fault(
        project,
        notes_included,
        base_documents(),
        &HashMap::new(),
        count,
    )? {
        findings.batch.push((
            reason,
            format!(
                "{what} is not a valid task graph even without the folder's additions: {}",
                fault.diagnostic
            ),
        ));
        return Ok(findings);
    }
    if additions.is_empty() {
        return Ok(findings);
    }
    let dependents = dependents_among(prospective, additions);
    let expand = |tasks: &mut Vec<TaskDocument>| -> Result<Vec<TaskDocument>, StoreError> {
        let support = if let Some(connection) = native_connection {
            origin::native_endpoints(connection, project, tasks)?
                .into_iter()
                .map(|endpoint| endpoint.task)
                .collect::<Vec<_>>()
        } else {
            Vec::new()
        };
        tasks.extend(support.iter().cloned());
        Ok(support)
    };
    let mut remaining = additions.to_vec();
    for _ in 0..FAULT_PASSES {
        let mut tasks = base_documents();
        for &id in &remaining {
            tasks.push(prospective.resolved_task(id)?.clone());
        }
        let support = expand(&mut tasks)?;
        let expected = remaining
            .iter()
            .filter_map(|id| prospective.roots.get(id).map(|root| (*id, *root)))
            .collect::<HashMap<_, _>>();
        let Some(fault) = locate_fault(project, notes_included, tasks, &expected, count)? else {
            return Ok(findings);
        };
        let Some(located) = fault
            .located
            .map(|mut located| {
                name_native_support_users(&mut located, &support, prospective, &remaining);
                located
            })
            .filter(|located| remaining.iter().any(|id| located.groups.contains_key(id)))
        else {
            findings.batch.push((
                reason,
                format!(
                    "the additions together would make {what} an invalid task graph: {}",
                    fault.diagnostic
                ),
            ));
            return Ok(findings);
        };
        let mut named_in_group = HashMap::<usize, usize>::new();
        for id in &remaining {
            if let Some(&group) = located.groups.get(id) {
                *named_in_group.entry(group).or_default() += 1;
            }
        }
        let mut removed = HashSet::new();
        for &id in &remaining {
            let Some(group) = located.groups.get(&id) else {
                continue;
            };
            let detail = located
                .class
                .detail(what, named_in_group.get(group).copied().unwrap_or(0) > 1);
            findings.conflicts.push(Conflict { id, reason, detail });
            removed.insert(id);
        }
        // An addition that depends on a named one is set aside unnamed, so
        // that its reference to the named one does not fail the next pass;
        // the caller names it for that dependence.
        let mut queue = removed.iter().copied().collect::<Vec<_>>();
        while let Some(id) = queue.pop() {
            for &(dependent, _) in dependents.get(&id).into_iter().flatten() {
                if removed.insert(dependent) {
                    queue.push(dependent);
                }
            }
        }
        remaining.retain(|id| !removed.contains(id));
        if remaining.is_empty() {
            return Ok(findings);
        }
    }
    // A last check of the additions still admitted.
    let mut tasks = base_documents();
    for &id in &remaining {
        tasks.push(prospective.resolved_task(id)?.clone());
    }
    expand(&mut tasks)?;
    let expected = remaining
        .iter()
        .filter_map(|id| prospective.roots.get(id).map(|root| (*id, *root)))
        .collect::<HashMap<_, _>>();
    if let Some(fault) = locate_fault(project, notes_included, tasks, &expected, count)? {
        findings.batch.push((
            reason,
            format!(
                "the remaining additions together would make {what} an invalid task graph: {}",
                fault.diagnostic
            ),
        ));
    }
    Ok(findings)
}

/// A new reference can retain a native creation component whose old edges
/// close an origin cycle without the referencing task itself being on that
/// cycle. Attribute that support to its incoming users, not to unrelated
/// native work. The reverse walk is linear in the selected support graph.
fn name_native_support_users(
    located: &mut Located,
    support: &[TaskDocument],
    prospective: &Prospective<'_>,
    additions: &[WorkId],
) {
    let mut dependents = HashMap::<WorkId, Vec<WorkId>>::new();
    let directly_named = additions
        .iter()
        .filter_map(|id| located.groups.get(id).copied())
        .collect::<HashSet<_>>();
    for task in support.iter().chain(
        additions
            .iter()
            .filter_map(|id| prospective.resolved.get(id).copied()),
    ) {
        for reference in origin::native_references(task) {
            dependents.entry(reference).or_default().push(task.id);
        }
    }
    let mut queue = support
        .iter()
        .filter_map(|task| {
            located
                .groups
                .get(&task.id)
                .filter(|group| !directly_named.contains(*group))
                .map(|group| (task.id, *group))
        })
        .collect::<std::collections::VecDeque<_>>();
    while let Some((id, group)) = queue.pop_front() {
        for &dependent in dependents.get(&id).into_iter().flatten() {
            if let std::collections::hash_map::Entry::Vacant(entry) =
                located.groups.entry(dependent)
            {
                entry.insert(group);
                queue.push_back((dependent, group));
            }
        }
    }
}

/// The second stage across the admitted additions, first check: the graph
/// that would exist after the import, the current documents of every held
/// task the import keeps (scalar edits do not change its structure) plus the
/// additions, must be a valid task graph. A host-local task the folder omits
/// can close a cycle that neither the folder nor the origin history holds.
///
/// `removed` names held tasks the import would take out of the planning;
/// only admitted execution-free removals belong in this set.
fn retained_graph(
    prospective: &Prospective<'_>,
    removed: &HashSet<WorkId>,
    additions: &[WorkId],
) -> Result<GraphFindings, StoreError> {
    let mut base = prospective
        .current
        .iter()
        .filter(|(id, _)| !removed.contains(id))
        .map(|(_, &task)| task)
        .collect::<Vec<_>>();
    base.sort_by_key(|task| task.id.0);
    check_graph(
        prospective,
        prospective.project,
        prospective.notes_included,
        &base,
        additions,
        "retained_graph",
        "the destination's planning",
        count_retained_validation,
        None,
    )
}

/// The second stage across the admitted additions, last check: the
/// import-origin history, every origin document of the project plus the
/// additions, must stay a valid task graph whose computed roots match. An
/// origin keeps the structure its task was imported with, so an edge the
/// current planning no longer holds can still close a cycle there.
fn origin_graph(
    connection: &rusqlite::Connection,
    prospective: &Prospective<'_>,
    origins: &OriginGraph,
    additions: &[WorkId],
) -> Result<GraphFindings, StoreError> {
    let project = prospective.project;
    let base = origins.documents.iter().collect::<Vec<_>>();
    let notes_included = prospective.notes_included || origins.notes_included;
    check_graph(
        prospective,
        project,
        notes_included,
        &base,
        additions,
        "origin_graph",
        "the import-origin history",
        count_origin_validation,
        Some(connection),
    )
}

fn count_origin_validation() {
    #[cfg(test)]
    ORIGIN_VALIDATIONS.with(|count| count.set(count.get() + 1));
}

fn count_retained_validation() {
    #[cfg(test)]
    RETAINED_VALIDATIONS.with(|count| count.set(count.get() + 1));
}

/// Takes a graph check's findings: its named additions, unless an earlier
/// reason already names them, and its folder-level refusals.
fn absorb(
    findings: GraphFindings,
    conflicts: &mut BTreeMap<uuid::Uuid, Conflict>,
    batch: &mut Vec<(&'static str, String)>,
) {
    for conflict in findings.conflicts {
        conflicts.entry(conflict.id.0).or_insert(conflict);
    }
    batch.extend(findings.batch);
}

/// The refusal text: every project change, then the first refused tasks in
/// the order given, up to the limit, then the exact count of the rest.
#[cfg(test)]
pub(super) fn conflict_message(project: &[ProjectChange], conflicts: &[Conflict]) -> String {
    conflict_message_with(project, &[], conflicts)
}

/// The refusal text with folder-level refusals, which no single task
/// accounts for, after the project changes.
fn conflict_message_with(
    project: &[ProjectChange],
    batch: &[(&'static str, String)],
    conflicts: &[Conflict],
) -> String {
    let mut entries = project
        .iter()
        .map(|change| format!("project: {}", change.code()))
        .chain(
            batch
                .iter()
                .map(|(reason, detail)| format!("folder: {reason} ({detail})")),
        )
        .chain(conflicts.iter().take(NAMED_CONFLICT_LIMIT).map(|conflict| {
            format!(
                "{}: {} ({})",
                conflict.id.0, conflict.reason, conflict.detail
            )
        }))
        .collect::<Vec<_>>();
    match conflicts.len().saturating_sub(NAMED_CONFLICT_LIMIT) {
        0 => {}
        1 => entries.push("and 1 more refused task".into()),
        rest => entries.push(format!("and {rest} more refused tasks")),
    }
    format!(
        "task folder import refuses the selected changes: {}",
        entries.join("; ")
    )
}

#[cfg(test)]
mod tests;
