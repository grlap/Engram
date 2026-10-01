//! A host's read of what satisfied each bound acceptance criterion of one
//! item on its active run. It reads one snapshot and writes nothing.

use super::{SessionId, SqliteStore, StoreError, work};
use crate::domain::{AcceptanceBindingPage, ProjectId};

impl SqliteStore {
    /// Reads one page of `request`'s item for the bound host session. The
    /// first page captures the run's feed head as its cut; a continuation
    /// reads at that cut, or is refused once the run's feed has moved.
    ///
    /// # Errors
    ///
    /// Returns the control session's own refusals for a superseded
    /// connection, an unbound session or a mismatched project or routing
    /// token; [`StoreError::AcceptanceBindingReadRefused`] with its typed
    /// reason when the item, revision, run or continuation does not hold or
    /// one row does not fit a page; and other [`StoreError`] values when a
    /// record fails its canonical association or cannot be read.
    pub(crate) fn read_acceptance_bindings(
        &self,
        project_id: &ProjectId,
        session_id: &SessionId,
        connection_token: &str,
        routing_token: &str,
        request: &super::BindingReadRequest<'_>,
    ) -> Result<AcceptanceBindingPage, StoreError> {
        work::on_one_snapshot(&self.connection, |connection| {
            Self::verify_control_connection(connection, session_id, connection_token)?;
            let session = Self::load_control_session_on(connection, session_id)?
                .ok_or_else(|| StoreError::ControlSessionNotBound(session_id.0.clone()))?;
            Self::verify_control_session(&session, project_id, routing_token)?;
            work::read_acceptance_bindings_on(connection, project_id, request)
        })
    }
}
