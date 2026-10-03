//! A host's read of whether a run's named source root has its initial
//! sighting at a cut, for the host process's routed project. It needs no
//! bound session, routing token or live holder, reads one snapshot and
//! writes nothing.

use super::{SqliteStore, StoreError, work};
use crate::domain::{NamedRootSightingRead, ProjectId, WorkRunId};

impl SqliteStore {
    /// Reads the root recording an evaluation would select for `run_id` at
    /// `run_cut`, or at the head when `None`, and whether it has a sighting
    /// there.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::NamedRootSightingReadRefused`] when the work
    /// reference names no item of the project, the run is unknown or not
    /// the item's, the cut is outside the run feed, or the result is too
    /// large; [`StoreError::WorkReferenceAmbiguous`] for an ambiguous
    /// reference; and other [`StoreError`] values when the store cannot be
    /// read. A refusal is never a finding about the root.
    pub(crate) fn read_named_root_sighting(
        &self,
        project_id: &ProjectId,
        work_ref: &str,
        run_id: WorkRunId,
        run_cut: Option<i64>,
    ) -> Result<NamedRootSightingRead, StoreError> {
        work::on_one_snapshot(&self.connection, |connection| {
            work::read_named_root_sighting_on(connection, project_id, work_ref, run_id, run_cut)
        })
    }
}
