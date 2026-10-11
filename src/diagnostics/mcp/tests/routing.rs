use super::*;
use crate::{McpServer, ProjectId, SessionId};
use rmcp::ServiceExt as _;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .unwrap()
}

async fn line(writer: &mut (impl tokio::io::AsyncWrite + Unpin), value: &serde_json::Value) {
    writer
        .write_all(format!("{value}\n").as_bytes())
        .await
        .unwrap();
}

async fn routed(server: McpServer, observer: Arc<Observer>) -> Vec<String> {
    let (client, server_io) = tokio::io::duplex(1 << 16);
    let transport = rmcp::transport::IntoTransport::<
        rmcp::RoleServer,
        std::io::Error,
        rmcp::transport::async_rw::TransportAdapterAsyncRW,
    >::into_transport(tokio::io::split(server_io));
    let transport = DiagnosticTransport::new(transport, Arc::clone(&observer));
    let serving = tokio::spawn(async move {
        server
            .serve(transport)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap();
    });
    let (reader, mut writer) = tokio::io::split(client);
    let mut reader = tokio::io::BufReader::new(reader).lines();
    line(&mut writer, &serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"diagnostics-test","version":"0"}}})).await;
    let _initialized = tokio::time::timeout(Duration::from_secs(10), reader.next_line())
        .await
        .unwrap()
        .unwrap()
        .unwrap();
    line(
        &mut writer,
        &serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await;
    let mut replies = Vec::new();
    for (id, name, arguments) in [
        (2, "show", serde_json::json!({"work_ref":"PRIVATE-missing"})),
        (
            3,
            "next",
            serde_json::json!({"PRIVATE-unknown-field":"PRIVATE-argument"}),
        ),
        (4, "PRIVATE-unknown-word", serde_json::json!({})),
    ] {
        line(&mut writer, &serde_json::json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":name,"arguments":arguments}})).await;
        replies.push(
            tokio::time::timeout(Duration::from_secs(10), reader.next_line())
                .await
                .unwrap()
                .unwrap()
                .unwrap(),
        );
    }
    writer.shutdown().await.unwrap();
    tokio::time::timeout(Duration::from_secs(10), serving)
        .await
        .unwrap()
        .unwrap();
    observer.finished();
    replies
}

#[tokio::test]
async fn actual_mcp_routing_errors_keep_response_bytes_and_typed_faults() {
    let home = crate::test_support::temp_home().unwrap();
    let database = home.path().join("store.sqlite3");
    let _ = crate::SqliteStore::open(&database).unwrap();
    let sink = Diagnostics::start(home.path(), "session-11296", Surface::Mcp);
    let make = || {
        McpServer::new_with_actor_context(
            database.clone(),
            ProjectId("diagnostics-mcp".into()),
            "PRIVATE-actor".into(),
            SessionId("session-11296".into()),
            None,
            Some("PRIVATE-context".into()),
        )
    };
    let baseline_observer = Observer::new(sink.clone());
    let baseline = routed(make(), baseline_observer).await;
    let observer = Observer::new(sink.clone());
    let candidate = routed(make().with_diagnostics(Arc::clone(&observer)), observer).await;
    assert_eq!(baseline, candidate);
    assert!(sink.flush(EXIT_FLUSH));
    let rows = records(home.path());
    assert!(rows.iter().any(|row| row["event"]["operation"] == "show"));
    assert!(rows.iter().any(|row| row["event"]["code"] == "rpc_error"));
    assert!(!serde_json::to_string(&rows).unwrap().contains("PRIVATE"));
}

#[tokio::test]
async fn actual_rmcp_cancellation_releases_slots_and_quarantine_overflow_fails_closed() {
    use rmcp::{
        RoleServer, ServerHandler,
        model::{CallToolRequestParams, CallToolResponse, ErrorData},
        service::RequestContext,
    };
    #[derive(Clone)]
    struct Cancellable {
        observer: Arc<Observer>,
        started: tokio::sync::mpsc::UnboundedSender<()>,
        settled: tokio::sync::mpsc::UnboundedSender<()>,
    }
    impl ServerHandler for Cancellable {
        async fn call_tool(
            &self,
            request: CallToolRequestParams,
            context: RequestContext<RoleServer>,
        ) -> Result<CallToolResponse, ErrorData> {
            let ticket = self.observer.started(&context, &request.name);
            if request.name.as_ref() == "note" {
                self.started.send(()).unwrap();
                context.ct.cancelled().await;
            }
            self.observer.settled(ticket, Some(Fault::tool_error()));
            if request.name.as_ref() == "note" {
                self.settled.send(()).unwrap();
            }
            Err(ErrorData::invalid_request("test response", None))
        }
    }
    let home = crate::test_support::temp_home().unwrap();
    let sink = Diagnostics::start(home.path(), "session-1", Surface::Mcp);
    let observer = Observer::new(sink.clone());
    let (started, mut starts) = tokio::sync::mpsc::unbounded_channel();
    let (settled, mut settles) = tokio::sync::mpsc::unbounded_channel();
    let server = Cancellable {
        observer: Arc::clone(&observer),
        started,
        settled,
    };
    let (client, server_io) = tokio::io::duplex(1 << 16);
    let transport = rmcp::transport::IntoTransport::<
        rmcp::RoleServer,
        std::io::Error,
        rmcp::transport::async_rw::TransportAdapterAsyncRW,
    >::into_transport(tokio::io::split(server_io));
    let transport = DiagnosticTransport::new(transport, Arc::clone(&observer));
    let serving = tokio::spawn(async move {
        server
            .serve(transport)
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap();
    });
    let (reader, mut writer) = tokio::io::split(client);
    let mut reader = tokio::io::BufReader::new(reader).lines();
    line(&mut writer, &serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}})).await;
    bounded(reader.next_line()).await.unwrap().unwrap();
    line(
        &mut writer,
        &serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await;
    for id in 2..=(2 + crate::diagnostics::QUEUE_CAPACITY) {
        line(&mut writer, &serde_json::json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"note","arguments":{}}})).await;
        bounded(starts.recv()).await.unwrap();
        line(&mut writer, &serde_json::json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":id}})).await;
        bounded(settles.recv()).await.unwrap();
        assert_eq!(
            observer
                .state
                .lock()
                .unwrap()
                .pending
                .iter()
                .flatten()
                .count(),
            0
        );
        assert!(sink.flush(EXIT_FLUSH));
    }
    // rmcp drops cancelled responses instead of calling the transport's send.
    // Even a settled handler may have an old response in rmcp's queue.
    // Quarantine overflow sacrifices precision, never capacity or fault capture.
    line(&mut writer, &serde_json::json!({"jsonrpc":"2.0","id":2,"method":"tools/call","params":{"name":"show","arguments":{}}})).await;
    let reply: serde_json::Value =
        serde_json::from_str(&bounded(reader.next_line()).await.unwrap().unwrap()).unwrap();
    assert_eq!(reply["id"], 2);
    assert_eq!(
        observer
            .state
            .lock()
            .unwrap()
            .pending
            .iter()
            .flatten()
            .count(),
        0
    );
    writer.shutdown().await.unwrap();
    bounded(serving).await.unwrap();
    observer.finished();
    assert!(sink.flush(EXIT_FLUSH));
    let rows = records(home.path());
    assert_eq!(
        rows.iter()
            .filter(|row| row["event"]["code"] == "cancel_requested")
            .count(),
        crate::diagnostics::QUEUE_CAPACITY + 1
    );
    assert_eq!(
        rows.iter()
            .filter(|row| row["event"]["code"] == "incomplete")
            .count(),
        0
    );
    let reused = rows
        .iter()
        .find(|row| row["event"]["code"] == "tool_error" && row["event"]["operation"] == "show")
        .unwrap();
    assert_eq!(reused["event"]["request_id"], 2);
    assert!(reused["event"]["request_seq"].is_null());
    assert_eq!(
        reused["event"]["uncorrelated_reason"],
        "quarantine_overflow"
    );
    assert_eq!(rows.last().unwrap()["loss"]["tracking"], 1);
    assert_eq!(rows.last().unwrap()["loss"]["quarantine_overflow"], 1);
}

#[tokio::test]
async fn actual_rmcp_old_cancelled_response_first_never_claims_replacement() {
    ambiguous_order(true).await;
}

#[tokio::test]
async fn actual_rmcp_duplicate_handler_first_never_claims_original() {
    ambiguous_order(false).await;
}

async fn ambiguous_order(cancel_old: bool) {
    use rmcp::{
        RoleServer, ServerHandler,
        model::{CallToolRequestParams, CallToolResponse, ErrorData},
        service::RequestContext,
    };
    #[derive(Clone)]
    struct Delayed {
        observer: Arc<Observer>,
        arrivals: tokio::sync::mpsc::UnboundedSender<&'static str>,
        old: Arc<tokio::sync::Semaphore>,
        old_return: Arc<tokio::sync::Semaphore>,
        new: Arc<tokio::sync::Semaphore>,
        cancel_old: bool,
    }
    impl ServerHandler for Delayed {
        async fn call_tool(
            &self,
            request: CallToolRequestParams,
            context: RequestContext<RoleServer>,
        ) -> Result<CallToolResponse, ErrorData> {
            if request.name.as_ref() == "note" {
                self.arrivals.send("old_received").unwrap();
                bounded(self.old.acquire()).await.unwrap().forget();
                assert_eq!(context.ct.is_cancelled(), self.cancel_old);
                let ticket = self.observer.started(&context, &request.name);
                assert_eq!(ticket.0, None);
                self.observer.settled(ticket, Some(Fault::tool_error()));
                self.arrivals.send("old_settled").unwrap();
                bounded(self.old_return.acquire()).await.unwrap().forget();
            } else {
                self.arrivals.send("new_received").unwrap();
                bounded(self.new.acquire()).await.unwrap().forget();
                let ticket = self.observer.started(&context, &request.name);
                assert_eq!(ticket.0, None);
                self.observer.settled(ticket, Some(Fault::tool_error()));
                self.arrivals.send("new_settled").unwrap();
            }
            Err(ErrorData::invalid_request(request.name, None))
        }
    }
    let home = crate::test_support::temp_home().unwrap();
    let sink = Diagnostics::start(home.path(), "session-1", Surface::Mcp);
    let observer = Observer::new(sink.clone());
    let (arrivals, mut received) = tokio::sync::mpsc::unbounded_channel();
    let old = Arc::new(tokio::sync::Semaphore::new(0));
    let old_return = Arc::new(tokio::sync::Semaphore::new(0));
    let new = Arc::new(tokio::sync::Semaphore::new(0));
    let server = Delayed {
        observer: Arc::clone(&observer),
        arrivals,
        old: Arc::clone(&old),
        old_return: Arc::clone(&old_return),
        new: Arc::clone(&new),
        cancel_old,
    };
    let (client, server_io) = tokio::io::duplex(1 << 16);
    let transport = rmcp::transport::IntoTransport::<
        rmcp::RoleServer,
        std::io::Error,
        rmcp::transport::async_rw::TransportAdapterAsyncRW,
    >::into_transport(tokio::io::split(server_io));
    let transport_observer = Arc::clone(&observer);
    let serving = tokio::spawn(async move {
        server
            .serve(DiagnosticTransport::new(transport, transport_observer))
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap();
    });
    let (reader, mut writer) = tokio::io::split(client);
    let mut reader = tokio::io::BufReader::new(reader).lines();
    line(&mut writer, &serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}})).await;
    bounded(reader.next_line()).await.unwrap().unwrap();
    line(
        &mut writer,
        &serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await;
    line(&mut writer, &serde_json::json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"note","arguments":{}}})).await;
    assert_eq!(bounded(received.recv()).await, Some("old_received"));
    if cancel_old {
        line(&mut writer, &serde_json::json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":7}})).await;
    }
    line(&mut writer, &serde_json::json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"show","arguments":{}}})).await;
    assert_eq!(bounded(received.recv()).await, Some("new_received"));
    assert_eq!(observer.state.lock().unwrap().active, 0);
    if cancel_old {
        // A returns while B is still blocked. rmcp forwards A under their
        // shared ID and cancels B's token; diagnostics must not attribute it to B.
        old.add_permits(1);
        assert_eq!(bounded(received.recv()).await, Some("old_settled"));
        old_return.add_permits(1);
    } else {
        // B starts first despite A having been received first.
        new.add_permits(1);
        assert_eq!(bounded(received.recv()).await, Some("new_settled"));
    }
    let reply: serde_json::Value =
        serde_json::from_str(&bounded(reader.next_line()).await.unwrap().unwrap()).unwrap();
    assert_eq!(reply["id"], 7);
    assert_eq!(
        reply["error"]["message"],
        if cancel_old { "note" } else { "show" }
    );
    if cancel_old {
        new.add_permits(1);
        assert_eq!(bounded(received.recv()).await, Some("new_settled"));
    } else {
        old.add_permits(1);
        assert_eq!(bounded(received.recv()).await, Some("old_settled"));
        old_return.add_permits(1);
    }
    bounded(writer.shutdown()).await.unwrap();
    bounded(serving).await.unwrap();
    observer.finished();
    assert!(sink.flush(EXIT_FLUSH));
    let rows = records(home.path());
    let old_fault = rows
        .iter()
        .find(|row| row["event"]["code"] == "tool_error" && row["event"]["operation"] == "note")
        .unwrap();
    assert!(old_fault["event"]["request_seq"].is_null());
    let new_fault = rows
        .iter()
        .find(|row| row["event"]["code"] == "tool_error" && row["event"]["operation"] == "show")
        .unwrap();
    assert!(new_fault["event"]["request_seq"].is_null());
    for row in rows.iter().filter(|row| {
        row["event"]["code"] != "loss_summary" && row["event"]["code"] != "cancel_requested"
    }) {
        assert!(row["event"]["request_seq"].is_null());
        assert_eq!(row["event"]["uncorrelated_reason"], "ambiguous_id");
    }
}
