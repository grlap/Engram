//! Shipped MCP process wiring, with explicit-home durable diagnostics.
use anyhow::{Context as _, Result};
use engram::{
    McpServer,
    diagnostics::{
        Correlation, Diagnostics, Event, Fault, Surface,
        mcp::{DiagnosticTransport, Observer},
    },
    phase_trace::{PhaseTrace, ShutdownStage, TracingTransport},
};
use rmcp::{ServiceExt, transport::stdio};
use std::{path::Path, sync::Arc, time::Duration};

pub(crate) async fn serve(server: McpServer, home: &Path, session: &str) -> Result<()> {
    let sink = Diagnostics::start(home, session, Surface::Mcp);
    let observer = Observer::new(sink.clone());
    let transport = rmcp::transport::IntoTransport::<
        rmcp::RoleServer,
        std::io::Error,
        rmcp::transport::async_rw::TransportAdapterAsyncRW,
    >::into_transport(stdio());
    let transport = DiagnosticTransport::new(transport, Arc::clone(&observer));
    let server = server.with_diagnostics(Arc::clone(&observer));
    let result = if let Some(trace) = PhaseTrace::from_env() {
        trace.observe_diagnostics(Arc::clone(&observer));
        let result = server
            .with_phase_trace(Some(Arc::clone(&trace)))
            .serve(TracingTransport::new(transport, Arc::clone(&trace)))
            .await;
        match result {
            Ok(server) => {
                let stopped = server.waiting().await;
                trace.shutdown_stage(ShutdownStage::ServiceWaitingReturned);
                stopped
                    .map(|_| ())
                    .context("Engram MCP stdio server stopped with an error")
            }
            Err(error) => Err(error).context("failed to start Engram MCP stdio server"),
        }
    } else {
        match server.serve(transport).await {
            Ok(server) => server
                .waiting()
                .await
                .map(|_| ())
                .context("Engram MCP stdio server stopped with an error"),
            Err(error) => Err(error).context("failed to start Engram MCP stdio server"),
        }
    };
    observer.finished();
    if result.is_err() {
        sink.emit(Event::failure(
            "shutdown",
            Correlation::default(),
            Duration::ZERO,
            Fault::transport_failed(),
        ));
    }
    let _ = sink.flush(engram::diagnostics::EXIT_FLUSH);
    result
}
