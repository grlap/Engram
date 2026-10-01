//! The host-control command: serving `engram control` over stdio, with its
//! opt-in phase trace.

use anyhow::{Context as _, Result};
use engram::{HostControlServer, HostPathPolicy, ProjectId, SessionId};
use std::io::{self, BufReader, BufWriter};
use std::path::PathBuf;

/// An opted-in control process's trace while it runs. Dropped on any
/// return, it ends a startup still open as incomplete and closes the trace.
pub(crate) struct ControlStartup(
    pub(crate) std::sync::Arc<engram::phase_trace::control::ControlTrace>,
);

impl Drop for ControlStartup {
    fn drop(&mut self) {
        self.0.abandon_startup("startup_failed");
        self.0.close();
    }
}

#[allow(
    clippy::too_many_arguments,
    reason = "the control command's resolved context, plus its opt-in trace"
)]
pub(crate) fn serve_control(
    database: PathBuf,
    identity: Option<HostPathPolicy>,
    project_id: ProjectId,
    actor_id: String,
    session_id: String,
    actor_context: Option<String>,
    source_skill: Option<String>,
    trace: Option<&std::sync::Arc<engram::phase_trace::control::ControlTrace>>,
) -> Result<()> {
    // ENGRAM_MCP_PHASE_TRACE=1 at start traces startup and each frame to
    // stderr; unset, the server is untouched.
    if let Err(error) = crate::validate_session_id_length(&session_id) {
        if let Some(trace) = trace {
            trace.abandon_startup("invalid_session_id");
        }
        return Err(error.into());
    }
    let opened = HostControlServer::open_with_host_path_identity(
        database,
        identity,
        project_id,
        actor_id,
        SessionId(session_id),
        source_skill,
    );
    let mut server = match opened {
        Ok(server) => server.with_actor_context(actor_context),
        Err(error) => {
            if let Some(trace) = trace {
                trace.startup_finished(Some(engram::host::store_error_code(&error)));
            }
            return Err(error).context("failed to start Engram host-control service");
        }
    };
    let reader = BufReader::new(io::stdin().lock());
    let writer = BufWriter::new(io::stdout().lock());
    let stopped = match trace {
        Some(trace) => {
            trace.startup_finished(None);
            server.serve_traced(reader, writer, trace)
        }
        None => server.serve(reader, writer),
    };
    stopped.context("Engram host-control stdio service stopped with an error")
}
