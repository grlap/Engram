//! Transient selected documents, distinct from a complete admitted task tree.

use std::collections::BTreeMap;

use super::{TaskDocument, notes::NoteOmissions};
use crate::WorkId;

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
