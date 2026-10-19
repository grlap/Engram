//! Pure classification of a resolved task folder against the destination's
//! current shared planning.
//!
//! The planner reads three inputs and touches no store: the destination's
//! current export, the human-resolved tree, and the task ids of the last
//! applied exchange baseline. It decides only what the resolved tree asks to
//! change. Whether an admitted kind of change may apply to a given task here
//! (claims, runs, completion, provenance) is a separate admission question,
//! answered inside the applying transaction.
//!
//! Only scalar edits of a task both sides hold are applicable: its title,
//! outcome and priority. Every other requested delta is classified with a
//! named reason so that the whole folder can be refused, never applied in
//! part or dropped in silence.

use std::collections::{BTreeMap, HashSet};
use std::hash::BuildHasher;

use super::{TaskDocument, TaskSyncRefusal, TaskTree};
use crate::{WorkId, domain::WorkRevisionPatch, storage::StoreError};

/// What applying a resolved tree asks of the destination.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum FolderPlan {
    /// The destination shares no planning yet and the resolved tree holds
    /// tasks. The empty-graph import admits that tree whole, so no task is
    /// classified here.
    Bootstrap,
    /// The requested deltas against planning the destination already holds.
    Deltas(DeltaPlan),
}

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
    /// Only the title, outcome or priority differ.
    Scalar(ScalarChange),
    /// A delta this exchange does not apply.
    Unsupported(UnsupportedChange),
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

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum UnsupportedChange {
    /// The resolved tree adds a task the destination does not hold.
    Added,
    /// The resolved tree omits a task of the last applied exchange.
    Deleted,
    /// The resolved tree changes these fields beyond the scalar ones,
    /// in document order.
    Fields(Vec<TaskField>),
}

impl UnsupportedChange {
    /// A stable machine-readable name of the reason.
    #[must_use]
    pub const fn code(&self) -> &'static str {
        match self {
            Self::Added => "task_added",
            Self::Deleted => "task_deleted",
            Self::Fields(_) => "unsupported_fields",
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

    /// Whether every requested delta is an applicable scalar edit.
    #[must_use]
    pub fn is_applicable(&self) -> bool {
        self.project.is_empty() && self.unsupported().next().is_none()
    }

    /// The scalar edits, in task-id order.
    pub fn scalar_changes(&self) -> impl Iterator<Item = (WorkId, &ScalarChange)> {
        self.tasks.iter().filter_map(|task| match &task.delta {
            TaskDelta::Scalar(change) => Some((task.id, change)),
            _ => None,
        })
    }

    /// The unsupported task deltas, in task-id order.
    pub fn unsupported(&self) -> impl Iterator<Item = (WorkId, &UnsupportedChange)> {
        self.tasks.iter().filter_map(|task| match &task.delta {
            TaskDelta::Unsupported(change) => Some((task.id, change)),
            _ => None,
        })
    }
}

/// Classifies what `resolved` asks of a destination whose current shared
/// planning export is `current`, given the task ids of the destination's
/// last applied exchange baseline (empty before the first one).
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
) -> Result<FolderPlan, StoreError> {
    resolved
        .validate_stored()
        .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
    if resolved.project.project_id != current.project.project_id {
        return Err(TaskSyncRefusal::OriginMismatch
            .invalid("task folder project does not match the destination"));
    }
    if current.tasks.is_empty() && !resolved.tasks.is_empty() {
        return Ok(FolderPlan::Bootstrap);
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
            None if membership.contains(&held.id) => {
                TaskDelta::Unsupported(UnsupportedChange::Deleted)
            }
            None => TaskDelta::RetainedLocal,
        };
        deltas.insert(held.id.0, delta);
    }
    for id in incoming.keys() {
        deltas
            .entry(*id)
            .or_insert(TaskDelta::Unsupported(UnsupportedChange::Added));
    }
    let project = if current.project.notes_included == resolved.project.notes_included {
        Vec::new()
    } else {
        vec![ProjectChange::NotesProfile]
    };
    Ok(FolderPlan::Deltas(DeltaPlan {
        project,
        tasks: deltas
            .into_iter()
            .map(|(id, delta)| PlannedTask {
                id: WorkId(id),
                delta,
            })
            .collect(),
    }))
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
        prerequisites,
        blockers,
        superseded_by,
        disposal_reason,
        completion,
        notes,
        activity,
    } = held;
    let fields = changed_fields![
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
        prerequisites => Prerequisites,
        blockers => Blockers,
        superseded_by => SupersededBy,
        disposal_reason => DisposalReason,
        completion => Completion,
        notes => Notes,
        activity => Activity,
    ]
    .into_iter()
    .filter_map(|(field, changed)| changed.then_some(field))
    .collect::<Vec<_>>();
    if !fields.is_empty() {
        return Ok(TaskDelta::Unsupported(UnsupportedChange::Fields(fields)));
    }
    let change = ScalarChange {
        title: (*title != incoming.title).then(|| incoming.title.clone()),
        outcome: (*outcome != incoming.outcome).then(|| incoming.outcome.clone()),
        priority: (*priority != incoming.priority).then_some(incoming.priority),
    };
    if change.is_empty() {
        return Ok(TaskDelta::Unchanged);
    }
    // The ordinary revision rules, applied to exactly the values stored.
    crate::storage::validate_revision_patch(&change.patch())
        .map_err(|error| TaskSyncRefusal::InvalidGraph.classify(error))?;
    Ok(TaskDelta::Scalar(change))
}

#[cfg(test)]
mod tests;
