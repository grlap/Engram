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
    /// Returns the control session's own refusals for a superseded
    /// connection, an unbound session or a mismatched project or routing
    /// token; [`StoreError::AcceptanceVerificationReadRefused`] with its
    /// typed reason when the item, revision, run, cut, criterion or
    /// continuation does not hold or one row does not fit a page; and other
    /// [`StoreError`] values when a record fails its canonical association or
    /// cannot be read.
    pub(crate) fn read_acceptance_verifications<'a>(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
        connection_token: &str,
        authority: impl Into<super::HostSessionAuthority<'a>>,
        request: &super::VerificationReadRequest<'_>,
    ) -> Result<AcceptanceVerificationPage, StoreError> {
        let authority = authority.into();
        work::on_one_snapshot(&self.connection, |connection| {
            let _ = authority.verify_on(connection, project_id, session_id, connection_token)?;
            work::read_acceptance_verifications_on(connection, project_id, request)
        })
    }
}
