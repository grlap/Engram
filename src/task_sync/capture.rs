//! Transient selected documents, distinct from a complete admitted task tree.

use std::collections::{BTreeMap, BTreeSet};

use super::{TaskDocument, notes::NoteOmissions};
use crate::{ObjectId, WorkId};

/// Shared graph namespaces with different retention contracts.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskSyncGraphView {
    /// Original immutable import documents, including retired owners.
    Origin,
    /// Current recorded exchange membership, excluding tombstones.
    Baseline,
}

/// Binding-scoped exchange membership, distinct from the local baseline.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum TaskSyncMembershipView {
    /// The binding's last admitted incoming source snapshot.
    Source,
    /// Prepared local export membership; this makes no publication claim.
    Export,
}

/// A selected connected component and its exact canonical membership head.
/// This is a read observation, without import or publication authority.
#[derive(Clone, Debug)]
pub struct TaskMembershipComponent {
    pub head: ObjectId,
    pub membership: super::delta::OutputMembership,
    pub component: TaskGraphComponent,
}

/// A connected selection captured on one snapshot, without admission or
/// application authority. Missing endpoints remain explicit; callers validate
/// document shape, graph, original roots and cross-component identities.
#[derive(Clone, Debug)]
pub struct TaskGraphComponent {
    pub documents: BTreeMap<WorkId, TaskDocument>,
    pub missing: BTreeSet<WorkId>,
    pub members: BTreeMap<WorkId, ObjectId>,
    pub expected_roots: BTreeMap<WorkId, WorkId>,
    pub notes_included: bool,
    pub baseline: Option<ObjectId>,
}

/// A closed component before and after a hypothetical membership delta.
/// Stored document fields and graph rules have been checked on both sides.
/// This is not a complete project inventory or application authority: the
/// caller still owns disclosure admission, global identities, eligibility and
/// transactional freshness checks.
#[derive(Clone, Debug)]
pub struct TaskGraphOverlay {
    pub before: TaskGraphComponent,
    pub after: BTreeMap<WorkId, TaskDocument>,
}

/// The selected identity's current shared-document state.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum CapturedTask {
    /// Current typed fields; references may name tasks outside the selection.
    Present(Box<TaskDocument>),
    /// No task or recorded exchange departure exists for this identity.
    Absent,
    /// The selected project's exchange baseline explicitly removed the task.
    Retired,
}

/// All fields are captured on one SQLite snapshot. This is not a stored record
/// or an admitted graph, and it does not authorize import or publication.
/// Callers must validate document shape and graph relationships before use in
/// an exchange.
#[derive(Clone, Debug)]
pub struct TaskDocumentCapture {
    pub documents: BTreeMap<WorkId, CapturedTask>,
    pub omissions: NoteOmissions,
    pub activity_omissions: NoteOmissions,
}
