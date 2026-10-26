//! One bounded blocking search job, owned until joined even if its caller drops.
use super::McpServer;
use crate::storage::search_budget::Request;
use crate::{AgentVerbs, Receipt, VerbError};
use rmcp::model::CallToolResult;
use tokio::{sync::Mutex, task::JoinHandle};
use tokio_util::sync::CancellationToken;

type Output = (
    Result<Receipt, VerbError>,
    Option<crate::phase_trace::Phases>,
);

#[derive(Debug, Default)]
pub(super) struct Searches {
    closed: CancellationToken,
    job: Mutex<Option<JoinHandle<Output>>>,
    #[cfg(test)]
    pub(super) pause:
        std::sync::Mutex<Option<std::sync::Arc<crate::storage::search_budget::TestPause>>>,
}

impl Drop for Searches {
    fn drop(&mut self) {
        self.closed.cancel();
    }
}

impl Searches {
    pub(super) async fn shutdown(&self) {
        self.closed.cancel();
        let mut job = self.job.lock().await;
        if let Some(job) = job.take() {
            let _ = job.await;
        }
    }
}

fn refusal(code: &'static str, message: &'static str) -> CallToolResult {
    crate::diagnostics::mcp::typed_fault(crate::diagnostics::Fault::tool_error());
    CallToolResult::structured_error(serde_json::json!({
        "error": { "code": code, "message": message }
    }))
}

impl McpServer {
    pub(super) async fn blocking_search(
        &self,
        request_cancel: CancellationToken,
        read: impl FnOnce(AgentVerbs) -> Result<Receipt, VerbError> + Send + 'static,
    ) -> CallToolResult {
        let Ok(mut slot) = self.searches.job.try_lock() else {
            return refusal(
                "search_busy",
                "another search is still running in this MCP server",
            );
        };
        if slot.as_ref().is_some_and(|job| !job.is_finished()) {
            return refusal(
                "search_busy",
                "another search is still running in this MCP server",
            );
        }
        // A dropped request leaves its handle here. Reap it before reusing the slot.
        if let Some(previous) = slot.take() {
            let _ = previous.await;
        }
        if self.searches.closed.is_cancelled() {
            return refusal("search_cancelled", "MCP server is shutting down");
        }
        let cancel = self.searches.closed.child_token();
        if request_cancel.is_cancelled() {
            cancel.cancel();
        }
        let _cancel_on_drop = cancel.clone().drop_guard();
        let request = Request::new(cancel.clone());
        #[cfg(test)]
        let request = request.with_pause(self.searches.pause.lock().unwrap().clone());
        let verbs = self.verbs();
        let diagnostic = crate::diagnostics::transaction::current();
        let traced = crate::phase_trace::active();
        *slot = Some(tokio::task::spawn_blocking(move || {
            let _diagnostic = diagnostic.map(crate::diagnostics::transaction::Scope::enter);
            let run = || request.run(|| read(verbs));
            if traced {
                let (output, phases) = crate::phase_trace::capture(run);
                (output, Some(phases))
            } else {
                (run(), None)
            }
        }));
        let job = slot.as_mut().expect("search handle was installed");
        let joined = tokio::select! {
            joined = &mut *job => joined,
            () = request_cancel.cancelled() => {
                cancel.cancel();
                job.await
            }
        };
        *slot = None;
        match joined {
            Ok((output, phases)) => {
                if let Some(phases) = phases {
                    crate::phase_trace::merge(phases);
                }
                self.verb(output)
            }
            Err(_) => refusal(
                "search_worker_failed",
                "search worker failed; no partial result is returned",
            ),
        }
    }
}

#[cfg(test)]
mod tests;
