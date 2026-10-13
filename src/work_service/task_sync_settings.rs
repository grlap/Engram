//! Configuration does not acquire or initialize the SQLite store.

use super::LocalWorkService;
use crate::task_sync::settings::{self, Action, Error, Request, Status};
use chrono::{DateTime, Utc};

impl LocalWorkService {
    /// The same host-local setting operation used by CLI and MCP.
    ///
    /// # Errors
    /// Refuses mutations on a read-only service, invalid attribution or an
    /// unsuitable filesystem path. Malformed setting bytes may be replaced
    /// by an explicit attributed setter; concurrent setters use last writer wins.
    pub fn task_sync_settings(
        &self,
        request: &Request,
        now: DateTime<Utc>,
    ) -> Result<Status, Error> {
        if request.action == Action::Status {
            return Ok(settings::read(&self.database, &self.project_id));
        }
        let refuse = |code, message| {
            Error::new(
                Some(&self.project_id),
                request.action,
                code,
                message,
                Some(settings::read(&self.database, &self.project_id)),
            )
        };
        if self.read_only {
            return Err(refuse(
                "mcp_read_only_refused",
                "this connection is read-only",
            ));
        }
        if self.validate_read_attribution(now).is_err() {
            return Err(refuse(
                "invalid_attribution",
                "current actor or session attribution is invalid",
            ));
        }
        settings::set(
            &self.database,
            &self.project_id,
            request.action,
            request.user_instruction.as_deref(),
            &self.actor_id,
            &self.session_id.0,
            now,
        )
    }
}
