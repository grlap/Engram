//! A host's read, for one acceptance criterion of an item on its active run,
//! of every verification of the criterion's bound kind up to a pinned cut.
//! It reads one snapshot and writes nothing.

use super::{SessionId, SqliteStore, StoreError, work};
use crate::domain::{AcceptanceVerificationPage, ProjectId};

impl SqliteStore {
    /// Reads one page of `request`'s criterion for the bound host session,
    /// at the cut the request names, which must be the run's feed head.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError::ControlConnectionSuperseded`] when the work
    /// session's connection fence is missing or no longer current;
    /// [`StoreError::AcceptanceVerificationReadRefused`] with its
    /// typed reason when the item, revision, run, cut, criterion or
    /// continuation does not hold or one row does not fit a page; and other
    /// [`StoreError`] values when a record fails its canonical association or
    /// cannot be read.
    pub(crate) fn read_acceptance_verifications(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
        connection_token: &str,
        authority: super::HostSessionAuthority,
        request: &super::VerificationReadRequest<'_>,
    ) -> Result<AcceptanceVerificationPage, StoreError> {
        work::on_one_snapshot(&self.connection, |connection| {
            authority.verify_on(connection, project_id, session_id, connection_token)?;
            work::read_acceptance_verifications_on(connection, project_id, request)
        })
    }
}
