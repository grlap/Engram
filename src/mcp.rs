//! MCP stdio surface for fourteen work words, `search`, and the separate
//! `task_sync_settings` configuration tool.

mod arguments;
#[cfg(test)]
mod assessment_tests;
mod parameters;
#[cfg(test)]
pub(crate) mod prose_sweep;
mod read_only;
mod search;
#[cfg(test)]
mod tests;
mod tools;

use crate::{AgentVerbs, LocalWorkService, ProjectId, Receipt, SessionId, VerbError};
use rmcp::{
    ErrorData, RoleServer, ServerHandler,
    handler::server::{router::tool::ToolRouter, tool::ToolCallContext},
    model::{CallToolRequestParams, CallToolResponse, CallToolResult},
    service::RequestContext,
    tool_handler,
};
use std::{path::PathBuf, sync::Arc};

/// Immutable host context asserted for one MCP connection.
#[derive(Clone, Debug)]
pub struct McpServer {
    actor_id: String,
    session_id: SessionId,
    work_service: Arc<LocalWorkService>,
    tool_router: ToolRouter<Self>,
    /// Fixed at construction: only the read words, in their reading forms.
    read_only: bool,
    /// The opt-in phase trace, when enabled at server start.
    phase_trace: Option<Arc<crate::phase_trace::PhaseTrace>>,
    diagnostics: Option<Arc<crate::diagnostics::mcp::Observer>>,
    searches: Arc<search::Searches>,
}

impl McpServer {
    /// Creates an MCP service with optional host-asserted actor attribution
    /// context. The context never participates in actor/session authority.
    #[must_use]
    pub fn new_with_actor_context(
        database: PathBuf,
        project_id: ProjectId,
        actor_id: String,
        session_id: SessionId,
        source_skill: Option<String>,
        actor_context: Option<String>,
    ) -> Self {
        Self::with_mode(
            database,
            project_id,
            actor_id,
            session_id,
            source_skill,
            actor_context,
            false,
        )
    }

    /// The same server in read-only mode: it lists only the read words and
    /// refuses any other call, and any writing form of a read word, as an
    /// MCP tool error with a stable code. Its service never opens the store
    /// for writing.
    #[must_use]
    pub fn new_read_only_with_actor_context(
        database: PathBuf,
        project_id: ProjectId,
        actor_id: String,
        session_id: SessionId,
        source_skill: Option<String>,
        actor_context: Option<String>,
    ) -> Self {
        Self::with_mode(
            database,
            project_id,
            actor_id,
            session_id,
            source_skill,
            actor_context,
            true,
        )
    }

    fn with_mode(
        database: PathBuf,
        project_id: ProjectId,
        actor_id: String,
        session_id: SessionId,
        source_skill: Option<String>,
        actor_context: Option<String>,
        read_only: bool,
    ) -> Self {
        // Capture before this long-lived server can outlive an executable install.
        let _ = crate::build_identity::current();
        let service = LocalWorkService::new_with_attribution(
            database,
            project_id,
            actor_id.clone(),
            session_id.clone(),
            source_skill,
            actor_context,
            crate::WorkAttributionDefaults::default(),
        );
        let work_service = Arc::new(if read_only {
            service.into_read_only()
        } else {
            service
        });
        let mut tool_router = Self::agent_tool_router();
        if read_only {
            // Every route the router holds, so a tool added later is
            // refused unless it is made a read word here.
            for tool in tool_router.list_all() {
                if !read_only::READ_TOOLS.contains(&tool.name.as_ref()) {
                    tool_router.remove_route(&tool.name);
                }
            }
        }
        Self {
            actor_id,
            session_id,
            work_service,
            tool_router,
            read_only,
            phase_trace: None,
            diagnostics: None,
            searches: Arc::default(),
        }
    }

    /// The same server, recording each tool call's phases into `trace`.
    #[must_use]
    pub fn with_phase_trace(mut self, trace: Option<Arc<crate::phase_trace::PhaseTrace>>) -> Self {
        self.phase_trace = trace;
        self
    }

    /// Enables the bounded durable observer for this explicitly configured process.
    #[must_use]
    pub fn with_diagnostics(mut self, observer: Arc<crate::diagnostics::mcp::Observer>) -> Self {
        self.diagnostics = Some(observer);
        self
    }

    /// Retains the service's cached store until the MCP loop has joined.
    #[must_use]
    pub fn shutdown_store_owner(&self) -> Arc<LocalWorkService> {
        Arc::clone(&self.work_service)
    }

    /// Close search admission, cancel and join any worker before store teardown.
    pub async fn shutdown_searches(&self) {
        self.searches.shutdown().await;
    }

    /// One tool call: the read-only admission, then the routed tool.
    async fn call_tool_untraced(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        // The read-only mode decides on the raw call, before any tool runs.
        if self.read_only
            && let Err(restriction) = read_only::admit(&request.name, request.arguments.as_ref())
        {
            return Ok(CallToolResult::structured_error(read_only::refusal(
                &request.name,
                restriction,
            ))
            .into());
        }
        self.tool_router
            .call(ToolCallContext::new(self, request, context))
            .await
    }

    fn verb(&self, outcome: Result<Receipt, VerbError>) -> CallToolResult {
        if let Err(error) = &outcome {
            crate::diagnostics::mcp::typed_fault(crate::diagnostics::Fault::mcp_verb(error));
        }
        tools::verb(outcome, &self.verbs())
    }

    fn verbs(&self) -> AgentVerbs {
        AgentVerbs::with_shared_service(
            Arc::clone(&self.work_service),
            self.actor_id.clone(),
            self.session_id.clone(),
        )
        .with_mcp_argument_names()
    }
}

/// What `initialize` tells an ordinary connection.
const INSTRUCTIONS: &str = "Fourteen words: next, ls, show, add, claim, update, gate, evaluate, note, done, handoff, remember, memories, forget (plus search). add needs only a title; claim before execution; evaluate records attributed acceptance verdicts that an evaluated project policy consumes at done; note accepts project-bound non-holders on open/blocked work without granting execution authority; completed note/gate append late evidence without reopening; remember stores attributed project notes; memories is their source of truth; forget tombstones rather than erases. Every answer a word gives ends with reminders and runnable next commands; arguments a word's input schema rejects (an undeclared field, a wrong type, an unknown action) are refused before the word runs, as a text-only tool error that names the field and describes the problem. Keyless same-holder claim calls renew without shortening expiry. A restored completed gate always appends: inspect show before repeating an uncertain call. Other identical calls retain exact retry semantics. task_sync_settings is a separate host-local configuration tool outside the work words: status reads without creating a store; enable requires user_instruction quoting the user's explicit request; disable needs no quote.";

#[allow(
    clippy::unused_async_trait_impl,
    reason = "the rmcp handler macro emits required async trait methods"
)]
#[tool_handler(router = self.tool_router)]
impl ServerHandler for McpServer {
    fn get_info(&self) -> rmcp::model::ServerInfo {
        rmcp::model::ServerInfo::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
        .with_server_info(rmcp::model::Implementation::new("engram", "0.1.0"))
        .with_instructions(if self.read_only {
            read_only::INSTRUCTIONS.to_string()
        } else {
            INSTRUCTIONS.to_string()
        })
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        if let Some(observer) = &self.diagnostics {
            let ticket = observer.started(&context, &request.name);
            let transaction_context = observer
                .transaction_context(ticket, self.work_service.diagnostic_store_fingerprint());
            let (result, fault) = crate::diagnostics::transaction::scoped(
                transaction_context,
                crate::diagnostics::mcp::scoped(self.call_tool_traced(request, context)),
            )
            .await;
            let fault = fault.or_else(|| match &result {
                Err(_) => Some(crate::diagnostics::Fault::rpc_error()),
                Ok(CallToolResponse::Complete(response)) if response.is_error == Some(true) => {
                    Some(crate::diagnostics::Fault::tool_error())
                }
                Ok(_) => None,
            });
            observer.settled(ticket, fault);
            return result;
        }
        self.call_tool_traced(request, context).await
    }
}

impl McpServer {
    async fn call_tool_traced(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let Some(trace) = self.phase_trace.as_ref() else {
            return self.call_tool_untraced(request, context).await;
        };
        let tool = request.name.to_string();
        let ticket = trace.handler_started(&context.id, &tool, || context.ct.is_cancelled());
        let started = std::time::Instant::now();
        let (result, phases) =
            crate::phase_trace::scoped(self.call_tool_untraced(request, context)).await;
        trace.handler_settled(ticket, &tool, started.elapsed(), &phases);
        result
    }
}
