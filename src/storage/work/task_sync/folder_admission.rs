//! Admission and application of the changes a resolved folder asks of an
//! enrolled destination: the tasks it adds, and scalar edits of tasks both
//! sides hold.
//!
//! Admission runs in two stages before any write. The first judges each
//! task on its own: what the planner found it asks, and whether this store
//! lets that task take it. The second judges each addition in context: its
//! ancestors, its root's execution and the store's whole import-origin
//! history. Every conflict is collected, one per task, and any conflict
//! refuses the whole folder. A task's first-stage reason takes precedence
//! over a second-stage one; within the second stage, an addition's own
//! ancestor chain is judged first, then whether an addition it depends on
//! was itself refused, then the graph the import would leave, and last the
//! origin history. A graph failure no single addition accounts for refuses
//! at folder level, naming no task.
//!
//! The work is near-linear in the folder: every index is built once; a held
//! ancestor's eligibility, a referenced task's origin, a root's execution and
//! a completion's provenance are each read once, however many additions
//! share them; and each graph check validates its graph a fixed number of
//! times, naming the additions the located failure involves.

use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use super::{ImportOrigin, origin, query};
use crate::{
    ActorContext, ProjectId, WorkId, WorkLifecycle, WorkOrigin,
    domain::{WorkPlanningAuthority, WorkTransition},
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
    /// How many held references admission read for their origin, in this thread.
    pub(super) static REFERENCE_READS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
    /// How many prospective destination graphs admission validated, in this thread.
    pub(super) static RETAINED_VALIDATIONS: std::cell::Cell<usize> = const { std::cell::Cell::new(0) };
}

/// Why one task the folder would change cannot take that change here.
pub(super) struct Conflict {
    pub(super) id: WorkId,
    pub(super) reason: &'static str,
    pub(super) detail: String,
}

/// The folder's prospective shared planning: the destination's current
/// documents beside the resolved ones, so that a change is judged in the
/// context of the whole tree it would produce. The current documents are
/// kept as they are, not patched, so a later kind of change can compare the
/// two sides of any field.
pub(super) struct Prospective<'a> {
    pub(super) project: &'a ProjectId,
    /// The destination's current exported documents, by id.
    pub(super) current: HashMap<WorkId, &'a TaskDocument>,
    /// The resolved documents, by id.
    pub(super) resolved: HashMap<WorkId, &'a TaskDocument>,
    /// Each resolved task's root, computed from the whole resolved tree.
    pub(super) roots: HashMap<WorkId, WorkId>,
    /// Every origin-qualified activity identity the current documents hold,
    /// including host-local tasks the folder omits.
    current_activity: BTreeSet<(String, String)>,
}

impl<'a> Prospective<'a> {
    pub(super) fn new(current: &'a TaskTree, resolved: &'a TaskTree) -> Result<Self, StoreError> {
        Ok(Self {
            project: &resolved.project.project_id,
            current: current.tasks.iter().map(|task| (task.id, task)).collect(),
            resolved: resolved.tasks.iter().map(|task| (task.id, task)).collect(),
            roots: resolved
                .snapshots()?
                .into_iter()
                .map(|item| (item.work_id, item.root_id))
                .collect(),
            current_activity: current.tasks.iter().flat_map(activity_keys).collect(),
        })
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

/// Identities the store's import origins already hold, read once per
/// admission.
struct OriginIdentities {
    origins: Vec<ImportOrigin>,
    notes: HashSet<String>,
    activity: BTreeSet<(String, String)>,
}

impl OriginIdentities {
    fn read(transaction: &rusqlite::Transaction<'_>) -> Result<Self, StoreError> {
        let origins = origin::all_origins(transaction)?;
        let notes = origins
            .iter()
            .flat_map(|origin| origin.task.notes.iter())
            .map(|note| note.id.as_str().to_owned())
            .collect();
        let activity = origins
            .iter()
            .flat_map(|origin| activity_keys(&origin.task))
            .collect();
        Ok(Self {
            origins,
            notes,
            activity,
        })
    }
}

/// Reads about held tasks, each made once per admission.
#[derive(Default)]
struct HeldReads {
    eligibility: HashMap<WorkId, Option<(&'static str, String)>>,
    executions: HashMap<WorkId, Option<String>>,
    restored_completions: HashMap<WorkId, bool>,
    supported_origins: HashMap<WorkId, bool>,
}

impl HeldReads {
    /// Whether a held task was restored by a task-sync import, which the
    /// import-origin history requires of every task an addition references.
    fn supported_origin(
        &mut self,
        transaction: &rusqlite::Transaction<'_>,
        id: WorkId,
    ) -> Result<bool, StoreError> {
        if let Some(&known) = self.supported_origins.get(&id) {
            return Ok(known);
        }
        #[cfg(test)]
        REFERENCE_READS.with(|count| count.set(count.get() + 1));
        let item = query::load_work_item(transaction, id).map_err(origin::classify_binding)?;
        let supported = item.restored
            && item.origin == WorkOrigin::Local
            && item.source_snapshot_id.is_none()
            && origin::for_item(transaction, &item)
                .map_err(origin::classify_binding)?
                .is_some();
        self.supported_origins.insert(id, supported);
        Ok(supported)
    }

    fn eligibility(
        &mut self,
        transaction: &rusqlite::Transaction<'_>,
        id: WorkId,
    ) -> Result<Option<(&'static str, String)>, StoreError> {
        if let Some(known) = self.eligibility.get(&id) {
            return Ok(known.clone());
        }
        let found = eligible(transaction, id)?.map(|conflict| (conflict.reason, conflict.detail));
        self.eligibility.insert(id, found.clone());
        Ok(found)
    }

    /// The root's active execution, by id, when it has one.
    fn execution(
        &mut self,
        transaction: &rusqlite::Transaction<'_>,
        root: WorkId,
    ) -> Result<Option<String>, StoreError> {
        if let Some(known) = self.executions.get(&root) {
            return Ok(known.clone());
        }
        let found = query::active_root_execution_optional(transaction, root)?
            .map(|execution| execution.root_execution_id.0.to_string());
        self.executions.insert(root, found.clone());
        Ok(found)
    }

    /// Whether a completed held task is a completion this store only
    /// restored: no run and no history of its own here, so no seal or
    /// completed run accounts for its children.
    fn restored_completion(
        &mut self,
        transaction: &rusqlite::Transaction<'_>,
        id: WorkId,
    ) -> Result<bool, StoreError> {
        if let Some(&known) = self.restored_completions.get(&id) {
            return Ok(known);
        }
        let item = query::load_work_item(transaction, id).map_err(origin::classify_binding)?;
        let restored = item.restored
            && item.active_run_id.is_none()
            && query::canonical_work_event_records_for_item(transaction, id)?.is_empty();
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

/// Applies a classified folder whole, or refuses it before any write. Every
/// requested delta is judged first, in two stages: each task on its own,
/// then each addition in the context of the prospective tree and the origin
/// history. One conflict is kept per task, in the precedence the module
/// describes. The refusal names the first conflicts in task-id order, up to
/// a fixed limit, and gives the exact count of the rest.
///
/// Admitted changes then apply in a fixed order of phases: additions, then
/// scalar edits. The caller checks the result and records it.
pub(super) fn apply_plan(
    transaction: &rusqlite::Transaction<'_>,
    plan: &DeltaPlan,
    current: &TaskTree,
    resolved: &TaskTree,
    actor: &ActorContext,
    now: chrono::DateTime<chrono::Utc>,
) -> Result<(), StoreError> {
    let prospective = Prospective::new(current, resolved)?;
    let mut conflicts = BTreeMap::new();
    let mut batch = Vec::new();
    let mut reads = HeldReads::default();
    let additions = plan.additions().collect::<Vec<_>>();
    let identities = if additions.is_empty() {
        None
    } else {
        Some(OriginIdentities::read(transaction)?)
    };
    for task in &plan.tasks {
        let conflict = match &task.delta {
            TaskDelta::Deleted => Some(Conflict {
                id: task.id,
                reason: "task_deleted",
                detail: "omitted from the folder".into(),
            }),
            TaskDelta::Changed(change) if !change.is_applicable() => {
                Some(unsupported_fields(task.id, change))
            }
            TaskDelta::Changed(TaskChange {
                scalar: Some(_), ..
            }) => reads
                .eligibility(transaction, task.id)?
                .map(|(reason, detail)| Conflict {
                    id: task.id,
                    reason,
                    detail,
                }),
            TaskDelta::Added => match &identities {
                Some(identities) => {
                    admit_addition(transaction, &prospective, identities, &mut reads, task.id)?
                }
                None => None,
            },
            TaskDelta::Unchanged | TaskDelta::RetainedLocal | TaskDelta::Changed(_) => None,
        };
        if let Some(conflict) = conflict {
            conflicts.insert(conflict.id.0, conflict);
        }
    }
    if let Some(identities) = &identities {
        for &id in &additions {
            if !conflicts.contains_key(&id.0)
                && let Some(conflict) = ancestor_binding(transaction, &prospective, &mut reads, id)?
            {
                conflicts.insert(id.0, conflict);
            }
        }
        name_dependents_of_refusals(&prospective, &additions, &mut conflicts);
        let admitted = |conflicts: &BTreeMap<uuid::Uuid, Conflict>| {
            additions
                .iter()
                .copied()
                .filter(|id| !conflicts.contains_key(&id.0))
                .collect::<Vec<_>>()
        };
        // Nothing is removed from the planning by this import.
        let retained = retained_graph(
            &prospective,
            resolved,
            &HashSet::new(),
            &admitted(&conflicts),
        )?;
        absorb(retained, &mut conflicts, &mut batch);
        name_dependents_of_refusals(&prospective, &additions, &mut conflicts);
        if batch.is_empty() {
            let origins = origin_graph(&prospective, resolved, identities, &admitted(&conflicts))?;
            absorb(origins, &mut conflicts, &mut batch);
            name_dependents_of_refusals(&prospective, &additions, &mut conflicts);
        }
    }
    if !plan.project.is_empty() || !conflicts.is_empty() || !batch.is_empty() {
        let conflicts = conflicts.into_values().collect::<Vec<_>>();
        return Err(
            TaskSyncRefusal::ImportConflict.invalid(conflict_message_with(
                &plan.project,
                &batch,
                &conflicts,
            )),
        );
    }
    if !additions.is_empty() {
        // Additions may reference one another in any order; the caller
        // checks every foreign key once they are all in place.
        transaction.execute_batch("PRAGMA defer_foreign_keys = ON")?;
        let mut snapshots = resolved
            .snapshots()?
            .into_iter()
            .map(|item| (item.work_id, item))
            .collect::<HashMap<_, _>>();
        for &id in &additions {
            let task = prospective.resolved_task(id)?;
            let snapshot = snapshots.remove(&id).ok_or_else(|| {
                StoreError::InvalidWorkProjection("an added task has no snapshot".into())
            })?;
            super::insert_task(transaction, resolved, task, snapshot, actor, now)?;
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
    Ok(())
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

/// Whether a held task is free to take a planning change: restored by a
/// task-sync import, Open, with no run, and with only admitted run-free
/// history. Claims, handoffs, evaluations, obligations and seals all attach
/// to a run of the task itself, so a run-free task carries none; other
/// tasks' execution does not bind it.
fn eligible(
    transaction: &rusqlite::Transaction<'_>,
    id: WorkId,
) -> Result<Option<Conflict>, StoreError> {
    #[cfg(test)]
    ELIGIBILITY_READS.with(|count| count.set(count.get() + 1));
    let conflict = |reason: &'static str, detail: String| -> Result<Option<Conflict>, StoreError> {
        Ok(Some(Conflict { id, reason, detail }))
    };
    let item = query::load_work_item(transaction, id).map_err(origin::classify_binding)?;
    if !item.restored
        || item.origin != WorkOrigin::Local
        || item.source_snapshot_id.is_some()
        || origin::for_item(transaction, &item)
            .map_err(origin::classify_binding)?
            .is_none()
    {
        return conflict(
            "unsupported_origin",
            "not a task restored by a task-sync import".into(),
        );
    }
    match item.lifecycle {
        WorkLifecycle::Open => {}
        WorkLifecycle::Completed => {
            return conflict("restored_completion", "the task is completed".into());
        }
        other => {
            return conflict("lifecycle", format!("the task is {other:?}"));
        }
    }
    if let Some(run) = item.active_run_id {
        return conflict("active_run", format!("run {}", run.0));
    }
    for (event_id, event) in query::canonical_work_event_records_for_item(transaction, id)? {
        let run_bound = event.run_id.is_some()
            || event.run.is_some()
            || event.claim.is_some()
            || event.handoff_offer.is_some();
        if run_bound || !is_planning(&event.transition) {
            return conflict("execution_history", format!("event {}", event_id.as_str()));
        }
        if !admitted_history(&event.transition) {
            return conflict("planning_history", format!("event {}", event_id.as_str()));
        }
    }
    Ok(None)
}

/// The run-free planning transitions a task's history may hold while it
/// stays free to take a folder change. Each later kind of folder change that
/// records its own planning transition adds it here.
const fn admitted_history(transition: &WorkTransition) -> bool {
    matches!(
        transition,
        WorkTransition::Revised {
            authority: WorkPlanningAuthority::Project
        }
    )
}

/// Whether a transition changes only planning, never execution. Every
/// variant is named, so a new one cannot compile until it is classified.
const fn is_planning(transition: &WorkTransition) -> bool {
    match transition {
        WorkTransition::Created { .. }
        | WorkTransition::Decomposed { .. }
        | WorkTransition::Revised { .. }
        | WorkTransition::PrerequisiteAdded { .. }
        | WorkTransition::PrerequisiteRemoved { .. }
        | WorkTransition::Blocked { .. }
        | WorkTransition::Unblocked { .. }
        | WorkTransition::Disposed { .. } => true,
        // Execution, including a required-child waiver: it accounts for the
        // root's execution and can be recorded without a run of its own.
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
        | WorkTransition::RequiredChildWaived { .. }
        | WorkTransition::Reopened { .. } => false,
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
/// store, its record-id pins are refused here rather than during insertion,
/// and every task it references that this destination holds was restored by
/// a task-sync import, as the import-origin history requires.
fn admit_addition(
    transaction: &rusqlite::Transaction<'_>,
    prospective: &Prospective<'_>,
    identities: &OriginIdentities,
    reads: &mut HeldReads,
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
        if identities.notes.contains(note.id.as_str())
            || held(
                "SELECT EXISTS(SELECT 1 FROM objects WHERE object_id = ?1)",
                note.id.as_str(),
                None,
            )?
        {
            return collision(format!("note {} is already held", note.id.as_str()));
        }
    }
    for key in activity_keys(task) {
        // This host reports its native activity, in any project and under
        // whatever alias an export asserts, by the id of the event that
        // recorded the claim; like a note id, an activity id that names a
        // record this store holds is held.
        if identities.activity.contains(&key)
            || prospective.current_activity.contains(&key)
            || held(
                "SELECT EXISTS(SELECT 1 FROM objects WHERE object_id = ?1)",
                &key.1,
                None,
            )?
        {
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
    let references = task
        .parent_id
        .into_iter()
        .chain(task.prerequisites.iter().copied())
        .chain(task.superseded_by);
    for reference in references {
        if !prospective.current.contains_key(&reference) {
            // Another addition, admitted on its own.
            continue;
        }
        if !reads.supported_origin(transaction, reference)? {
            return Ok(Some(Conflict {
                id,
                reason: "unsupported_origin",
                detail: format!(
                    "it references task {}, which was not restored by a task-sync import",
                    reference.0
                ),
            }));
        }
    }
    Ok(None)
}

/// The second stage for an addition: no ancestor in its prospective chain is
/// bound against taking new work, and its root has no active execution.
/// Unfinished work needs every ancestor Open, and a held Open ancestor must
/// itself be free to change. A finished addition, such as a completed
/// subtree restored as history, may sit under a completed ancestor only when
/// that completion was itself restored: a completion sealed on this host
/// accounts for exactly the children it named, so it takes no new one.
fn ancestor_binding(
    transaction: &rusqlite::Transaction<'_>,
    prospective: &Prospective<'_>,
    reads: &mut HeldReads,
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
        let document = prospective.resolved_task(ancestor)?;
        if let Some(held) = prospective.current.get(&ancestor) {
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
        } else if unfinished && document.lifecycle != WorkLifecycle::Open {
            return binding(format!(
                "ancestor {} is {:?}",
                ancestor.0, document.lifecycle
            ));
        }
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
    let mut queue = additions
        .iter()
        .copied()
        .filter(|id| conflicts.contains_key(&id.0))
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
            queue.push_back(dependent);
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
    let added = additions.iter().copied().collect::<HashSet<_>>();
    let mut dependents = HashMap::<WorkId, Vec<(WorkId, bool)>>::new();
    for &id in additions {
        let Some(task) = prospective.resolved.get(&id) else {
            continue;
        };
        if let Some(parent) = task.parent_id.filter(|parent| added.contains(parent)) {
            dependents.entry(parent).or_default().push((id, true));
        }
        for reference in task
            .prerequisites
            .iter()
            .copied()
            .chain(task.superseded_by)
            .filter(|reference| added.contains(reference))
        {
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
    /// Why a named addition fails when no other named addition shares its
    /// fault.
    const fn alone(self) -> &'static str {
        match self {
            Self::MissingReference => "it references a missing task",
            Self::OpenDescendantLimit => "it takes its root over the open-descendant limit",
            Self::DependencyCycle => "it closes a dependency cycle",
            Self::DifferingRoot => "its root differs from the folder's",
        }
    }

    /// Why named additions fail when they share one fault.
    const fn together(self) -> &'static str {
        match self {
            Self::MissingReference => "each references a missing task",
            Self::OpenDescendantLimit => {
                "together they take their root over the open-descendant limit"
            }
            Self::DependencyCycle => "together they close a dependency cycle",
            Self::DifferingRoot => "each one's root differs from the folder's",
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
fn dependency_cycles(tasks: &[TaskDocument]) -> HashMap<WorkId, usize> {
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
) -> Result<GraphFindings, StoreError> {
    let mut findings = GraphFindings::default();
    if additions.is_empty() {
        return Ok(findings);
    }
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
    let dependents = dependents_among(prospective, additions);
    let mut remaining = additions.to_vec();
    for _ in 0..FAULT_PASSES {
        let mut tasks = base_documents();
        for &id in &remaining {
            tasks.push(prospective.resolved_task(id)?.clone());
        }
        let expected = remaining
            .iter()
            .filter_map(|id| prospective.roots.get(id).map(|root| (*id, *root)))
            .collect::<HashMap<_, _>>();
        let Some(fault) = locate_fault(project, notes_included, tasks, &expected, count)? else {
            return Ok(findings);
        };
        let Some(located) = fault
            .located
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
            let detail = if named_in_group.get(group).copied().unwrap_or(0) > 1 {
                format!(
                    "{what} with this and the other named additions would not be a valid task graph: {}",
                    located.class.together()
                )
            } else {
                format!(
                    "{what} with this addition would not be a valid task graph: {}",
                    located.class.alone()
                )
            };
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

/// The second stage across the admitted additions, first check: the graph
/// that would exist after the import, the current documents of every held
/// task the import keeps (scalar edits do not change its structure) plus the
/// additions, must be a valid task graph. A host-local task the folder omits
/// can close a cycle that neither the folder nor the origin history holds.
///
/// `removed` names held tasks the import would take out of the planning;
/// none does today.
fn retained_graph(
    prospective: &Prospective<'_>,
    resolved: &TaskTree,
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
        &resolved.project.project_id,
        resolved.project.notes_included,
        &base,
        additions,
        "retained_graph",
        "the destination's planning",
        count_retained_validation,
    )
}

/// The second stage across the admitted additions, last check: the
/// import-origin history, every origin document of the project plus the
/// additions, must stay a valid task graph whose computed roots match. An
/// origin keeps the structure its task was imported with, so an edge the
/// current planning no longer holds can still close a cycle there.
fn origin_graph(
    prospective: &Prospective<'_>,
    resolved: &TaskTree,
    identities: &OriginIdentities,
    additions: &[WorkId],
) -> Result<GraphFindings, StoreError> {
    let project = &resolved.project.project_id;
    let base = identities
        .origins
        .iter()
        .filter(|origin| origin.project_id == *project)
        .map(|origin| &origin.task)
        .collect::<Vec<_>>();
    let notes_included = resolved.project.notes_included
        || identities
            .origins
            .iter()
            .any(|origin| origin.project_id == *project && origin.notes_included);
    check_graph(
        prospective,
        project,
        notes_included,
        &base,
        additions,
        "origin_graph",
        "the import-origin history",
        count_origin_validation,
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
