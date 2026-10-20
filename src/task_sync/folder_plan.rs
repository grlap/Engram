//! Pure classification of a resolved task folder against an enrolled
//! destination's current shared planning.
//!
//! The planner reads three inputs and touches no store: the destination's
//! current export, the human-resolved tree, and the task ids of the last
//! applied exchange baseline. It decides only what the resolved tree asks to
//! change. Whether an admitted kind of change may apply to a given task here
//! (claims, runs, completion, provenance, identity) is a separate admission
//! question, answered inside the applying transaction.
//!
//! Applicable deltas are additions, omissions of enrolled members, and
//! scalar and relation edits of tasks both sides hold. Every other
//! requested delta is classified with a named reason so that the whole folder
//! can be refused, never applied in part or dropped in silence.

use std::collections::{BTreeMap, BTreeSet, HashSet};
use std::hash::BuildHasher;

use super::{SharedBlocker, TaskDocument, TaskSyncRefusal, TaskTree};
use crate::{WorkId, domain::WorkRevisionPatch, storage::StoreError};

/// Every requested delta, classified.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct DeltaPlan {
    /// Requested changes to the project document. None is applicable.
    pub project: Vec<ProjectChange>,
    /// One entry for each task id the destination or the resolved tree
    /// holds, in task-id order.
    pub tasks: Vec<PlannedTask>,
}

/// One task's classification.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct PlannedTask {
    pub id: WorkId,
    pub delta: TaskDelta,
}

impl PlannedTask {
    /// Whether the task stays exactly as the destination holds it.
    #[must_use]
    pub fn is_unchanged(&self) -> bool {
        matches!(self.delta, TaskDelta::Unchanged | TaskDelta::RetainedLocal)
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum TaskDelta {
    /// Both sides hold the same document.
    Unchanged,
    /// A task the destination holds outside the last applied exchange,
    /// which the resolved tree omits. Omission is not deletion: the task
    /// stays as it is and does not join the exchange.
    RetainedLocal,
    /// Both sides hold the task and its document differs.
    Changed(TaskChange),
    /// The resolved tree adds a task the destination does not hold.
    Added,
    /// The resolved tree omits a task of the last applied exchange.
    /// Storage admission decides whether it may be retired here.
    Deleted,
}

/// What a resolved document changes in a task both sides hold: its scalar
/// fields, net relation sets, and unsupported changed fields. The parts stay
/// together so that a
/// change of several kinds is judged once, as one change to one task.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct TaskChange {
    /// The new title, outcome or priority, when any of them changes.
    pub scalar: Option<ScalarChange>,
    /// Net changes to prerequisite and blocker sets, preserving blocker ids.
    pub relations: RelationChange,
    /// The other changed fields, in document order.
    pub unsupported: Vec<TaskField>,
}

/// An ephemeral plan only; original imported documents are never rewritten.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct RelationChange {
    pub prerequisite_additions: Vec<WorkId>,
    pub prerequisite_removals: Vec<WorkId>,
    pub blocker_additions: Vec<SharedBlocker>,
    pub blocker_removals: Vec<String>,
    pub blocker_rewrites: Vec<String>,
}

impl RelationChange {
    #[must_use]
    pub fn is_empty(&self) -> bool {
        self.prerequisite_additions.is_empty()
            && self.prerequisite_removals.is_empty()
            && self.blocker_additions.is_empty()
            && self.blocker_removals.is_empty()
            && self.blocker_rewrites.is_empty()
    }
}

impl TaskChange {
    /// Whether every changed field is one this exchange applies.
    #[must_use]
    pub fn is_applicable(&self) -> bool {
        self.unsupported.is_empty()
    }
}

/// The scalar fields a resolved task changes, each carrying its new value.
/// A field left unchanged is `None`.
#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct ScalarChange {
    pub title: Option<String>,
    pub outcome: Option<String>,
    pub priority: Option<i32>,
}

impl ScalarChange {
    fn is_empty(&self) -> bool {
        self.title.is_none() && self.outcome.is_none() && self.priority.is_none()
    }

    /// The same change as an ordinary planning revision patch.
    #[must_use]
    pub fn patch(&self) -> WorkRevisionPatch {
        WorkRevisionPatch {
            title: self.title.clone(),
            outcome: self.outcome.clone(),
            priority: self.priority,
            ..WorkRevisionPatch::default()
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ProjectChange {
    /// The resolved tree changes whether shared notes are included.
    NotesProfile,
}

impl ProjectChange {
    /// A stable machine-readable name of the change.
    #[must_use]
    pub const fn code(self) -> &'static str {
        match self {
            Self::NotesProfile => "notes_profile",
        }
    }
}

/// A task document field outside the scalar ones, named as its document
/// spells it.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskField {
    ExternalRef,
    Acceptance,
    AcceptanceBindings,
    EvaluationMode,
    Kind,
    Labels,
    AssignedTo,
    DeferredUntil,
    Lifecycle,
    ParentId,
    ChildRequirement,
    Prerequisites,
    Blockers,
    SupersededBy,
    DisposalReason,
    Completion,
    Notes,
    Activity,
}

impl TaskField {
    /// The field's name in the task document.
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::ExternalRef => "external_ref",
            Self::Acceptance => "acceptance",
            Self::AcceptanceBindings => "acceptance_bindings",
            Self::EvaluationMode => "evaluation_mode",
            Self::Kind => "kind",
            Self::Labels => "labels",
            Self::AssignedTo => "assigned_to",
            Self::DeferredUntil => "deferred_until",
            Self::Lifecycle => "lifecycle",
            Self::ParentId => "parent_id",
            Self::ChildRequirement => "child_requirement",
            Self::Prerequisites => "prerequisites",
            Self::Blockers => "blockers",
            Self::SupersededBy => "superseded_by",
            Self::DisposalReason => "disposal_reason",
            Self::Completion => "completion",
            Self::Notes => "notes",
            Self::Activity => "activity",
        }
    }
}

impl DeltaPlan {
    /// Whether nothing is requested: no project change and no task delta.
    #[must_use]
    pub fn is_unchanged(&self) -> bool {
        self.project.is_empty() && self.tasks.iter().all(PlannedTask::is_unchanged)
    }

    /// Whether every requested delta is of a kind this exchange applies:
    /// an addition, enrolled removal, scalar or relation edit. Admission may still refuse it.
    #[must_use]
    pub fn is_applicable(&self) -> bool {
        self.project.is_empty()
            && self.tasks.iter().all(|task| match &task.delta {
                TaskDelta::Unchanged
                | TaskDelta::RetainedLocal
                | TaskDelta::Added
                | TaskDelta::Deleted => true,
                TaskDelta::Changed(change) => change.is_applicable(),
            })
    }

    /// The applicable scalar edits, in task-id order.
    pub fn scalar_changes(&self) -> impl Iterator<Item = (WorkId, &ScalarChange)> {
        self.tasks.iter().filter_map(|task| match &task.delta {
            TaskDelta::Changed(TaskChange {
                scalar: Some(change),
                unsupported,
                ..
            }) if unsupported.is_empty() => Some((task.id, change)),
            _ => None,
        })
    }

    /// The tasks the resolved tree adds, in task-id order.
    pub fn additions(&self) -> impl Iterator<Item = WorkId> + '_ {
        self.tasks
            .iter()
            .filter(|task| task.delta == TaskDelta::Added)
            .map(|task| task.id)
    }

    /// Existing owners with net relation changes, in task-id order.
    pub fn relation_changes(&self) -> impl Iterator<Item = (WorkId, &RelationChange)> {
        self.tasks.iter().filter_map(|task| match &task.delta {
            TaskDelta::Changed(change)
                if change.is_applicable() && !change.relations.is_empty() =>
            {
                Some((task.id, &change.relations))
            }
            _ => None,
        })
    }

    pub fn changed_ids(&self) -> impl Iterator<Item = WorkId> + '_ {
        self.tasks.iter().filter_map(|task| match &task.delta {
            TaskDelta::Changed(change) if change.is_applicable() => Some(task.id),
            _ => None,
        })
    }
}

/// Classifies what `resolved` asks of an enrolled destination whose current
/// shared planning export is `current`, given the task ids of the
/// destination's last applied exchange baseline.
///
/// The destination holds a baseline, so even an empty current export is
/// compared task by task: every task the resolved tree holds is then an
/// addition. Only a destination with no baseline takes the empty-graph
/// import, and it never reaches this planner.
///
/// A destination task the resolved tree omits is a deletion only when the
/// last applied exchange held it; otherwise it is a host-local task and is
/// retained. `resolved` must pass the stored-tree rules, which already
/// require normalized title and outcome text and a priority from 0 through
/// 4, so each scalar value it carries is exactly the value an applied
/// revision stores.
///
/// # Errors
///
/// Refuses a resolved tree that is not a valid task graph, a scalar value
/// the ordinary revision rules refuse, and a resolved tree for another
/// project.
pub fn plan<S: BuildHasher>(
    current: &TaskTree,
    resolved: &TaskTree,
    membership: &HashSet<WorkId, S>,
) -> Result<DeltaPlan, StoreError> {
    resolved
        .validate_stored()
        .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
    if resolved.project.project_id != current.project.project_id {
        return Err(TaskSyncRefusal::OriginMismatch
            .invalid("task folder project does not match the destination"));
    }
    let incoming = resolved
        .tasks
        .iter()
        .map(|task| (task.id.0, task))
        .collect::<BTreeMap<_, _>>();
    let mut deltas = BTreeMap::new();
    for held in &current.tasks {
        let delta = match incoming.get(&held.id.0) {
            Some(incoming) => compare(held, incoming)?,
            None if membership.contains(&held.id) => TaskDelta::Deleted,
            None => TaskDelta::RetainedLocal,
        };
        deltas.insert(held.id.0, delta);
    }
    for id in incoming.keys() {
        deltas.entry(*id).or_insert(TaskDelta::Added);
    }
    let project = if current.project.notes_included == resolved.project.notes_included {
        Vec::new()
    } else {
        vec![ProjectChange::NotesProfile]
    };
    Ok(DeltaPlan {
        project,
        tasks: deltas
            .into_iter()
            .map(|(id, delta)| PlannedTask {
                id: WorkId(id),
                delta,
            })
            .collect(),
    })
}

fn compare(held: &TaskDocument, incoming: &TaskDocument) -> Result<TaskDelta, StoreError> {
    macro_rules! changed_fields {
        ($($field:ident => $name:ident),+ $(,)?) => {
            [$((TaskField::$name, *$field != incoming.$field)),+]
        };
    }
    // Destructured whole, so a new document field cannot compile until it is
    // classified here.
    let TaskDocument {
        id: _,
        external_ref,
        title,
        outcome,
        acceptance,
        acceptance_bindings,
        evaluation_mode,
        kind,
        priority,
        labels,
        assigned_to,
        deferred_until,
        lifecycle,
        parent_id,
        child_requirement,
        prerequisites: _,
        blockers: _,
        superseded_by,
        disposal_reason,
        completion,
        notes,
        activity,
    } = held;
    let unsupported = changed_fields![
        external_ref => ExternalRef,
        acceptance => Acceptance,
        acceptance_bindings => AcceptanceBindings,
        evaluation_mode => EvaluationMode,
        kind => Kind,
        labels => Labels,
        assigned_to => AssignedTo,
        deferred_until => DeferredUntil,
        lifecycle => Lifecycle,
        parent_id => ParentId,
        child_requirement => ChildRequirement,
        superseded_by => SupersededBy,
        disposal_reason => DisposalReason,
        completion => Completion,
        notes => Notes,
        activity => Activity,
    ]
    .into_iter()
    .filter_map(|(field, changed)| changed.then_some(field))
    .collect::<Vec<_>>();
    let scalar = ScalarChange {
        title: (*title != incoming.title).then(|| incoming.title.clone()),
        outcome: (*outcome != incoming.outcome).then(|| incoming.outcome.clone()),
        priority: (*priority != incoming.priority).then_some(incoming.priority),
    };
    let relations = compare_relations(held, incoming);
    if unsupported.is_empty() && scalar.is_empty() && relations.is_empty() {
        return Ok(TaskDelta::Unchanged);
    }
    let scalar = (!scalar.is_empty()).then_some(scalar);
    if unsupported.is_empty()
        && let Some(change) = &scalar
    {
        // The ordinary revision rules, applied to exactly the values stored.
        crate::storage::validate_revision_patch(&change.patch())
            .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
    }
    Ok(TaskDelta::Changed(TaskChange {
        scalar,
        relations,
        unsupported,
    }))
}

fn compare_relations(held: &TaskDocument, incoming: &TaskDocument) -> RelationChange {
    let before = held
        .prerequisites
        .iter()
        .map(|id| id.0)
        .collect::<BTreeSet<_>>();
    let after = incoming
        .prerequisites
        .iter()
        .map(|id| id.0)
        .collect::<BTreeSet<_>>();
    let old_blockers = held
        .blockers
        .iter()
        .map(|blocker| (blocker.blocker_id.as_str(), blocker))
        .collect::<BTreeMap<_, _>>();
    let mut change = RelationChange {
        prerequisite_additions: after.difference(&before).copied().map(WorkId).collect(),
        prerequisite_removals: before.difference(&after).copied().map(WorkId).collect(),
        ..RelationChange::default()
    };
    let mut incoming_ids = HashSet::new();
    for blocker in &incoming.blockers {
        incoming_ids.insert(blocker.blocker_id.as_str());
        match old_blockers.get(blocker.blocker_id.as_str()) {
            Some(old) if *old != blocker => {
                change.blocker_rewrites.push(blocker.blocker_id.clone());
            }
            None => change.blocker_additions.push(blocker.clone()),
            Some(_) => {}
        }
    }
    for id in old_blockers.keys() {
        if !incoming_ids.contains(id) {
            change.blocker_removals.push((*id).to_owned());
        }
    }
    change
        .blocker_additions
        .sort_by(|left, right| left.blocker_id.cmp(&right.blocker_id));
    change.blocker_rewrites.sort();
    change
}

#[cfg(test)]
mod tests;
