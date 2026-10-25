use super::super::*;
use rmcp::ServiceExt as _;
use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .unwrap()
}

#[tokio::test]
async fn actual_rmcp_old_cancelled_response_first_cannot_claim_replacement() {
    ambiguous_order(true).await;
}

#[tokio::test]
async fn actual_rmcp_duplicate_handler_first_cannot_claim_original() {
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
        trace: Arc<PhaseTrace>,
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
                let ticket = self
                    .trace
                    .handler_started(&context.id, &request.name, || context.ct.is_cancelled());
                assert!(ticket.sequence.is_none());
                self.trace.handler_settled(
                    ticket,
                    &request.name,
                    Duration::ZERO,
                    &Phases::default(),
                );
                self.arrivals.send("old_settled").unwrap();
                bounded(self.old_return.acquire()).await.unwrap().forget();
            } else {
                self.arrivals.send("new_received").unwrap();
                bounded(self.new.acquire()).await.unwrap().forget();
                let ticket = self
                    .trace
                    .handler_started(&context.id, &request.name, || context.ct.is_cancelled());
                assert!(ticket.sequence.is_none());
                self.trace.handler_settled(
                    ticket,
                    &request.name,
                    Duration::ZERO,
                    &Phases::default(),
                );
                self.arrivals.send("new_settled").unwrap();
            }
            Err(ErrorData::invalid_request(request.name, None))
        }
    }
    let (trace, captured) = captured_trace();
    let trace = Arc::new(trace);
    let (arrivals, mut received) = tokio::sync::mpsc::unbounded_channel();
    let old = Arc::new(tokio::sync::Semaphore::new(0));
    let old_return = Arc::new(tokio::sync::Semaphore::new(0));
    let new = Arc::new(tokio::sync::Semaphore::new(0));
    let server = Delayed {
        trace: Arc::clone(&trace),
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
    let transport_trace = Arc::clone(&trace);
    let serving = tokio::spawn(async move {
        server
            .serve(TracingTransport::new(transport, transport_trace))
            .await
            .unwrap()
            .waiting()
            .await
            .unwrap();
    });
    let (reader, mut writer) = tokio::io::split(client);
    let mut reader = tokio::io::BufReader::new(reader).lines();
    write_line(&mut writer, &serde_json::json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{"protocolVersion":"2025-06-18","capabilities":{},"clientInfo":{"name":"test","version":"0"}}})).await;
    bounded(reader.next_line()).await.unwrap().unwrap();
    write_line(
        &mut writer,
        &serde_json::json!({"jsonrpc":"2.0","method":"notifications/initialized"}),
    )
    .await;
    write_line(&mut writer, &serde_json::json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"note","arguments":{}}})).await;
    assert_eq!(bounded(received.recv()).await, Some("old_received"));
    if cancel_old {
        write_line(&mut writer, &serde_json::json!({"jsonrpc":"2.0","method":"notifications/cancelled","params":{"requestId":7}})).await;
    }
    write_line(&mut writer, &serde_json::json!({"jsonrpc":"2.0","id":7,"method":"tools/call","params":{"name":"show","arguments":{}}})).await;
    assert_eq!(bounded(received.recv()).await, Some("new_received"));
    assert_eq!(trace.pending_len(), 0);
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
    let rows = written(&trace, &captured);
    assert_eq!(
        rows.len(),
        2,
        "both actual handlers produced final summaries"
    );
    for tool in ["note", "show"] {
        let row = rows.iter().find(|row| row["tool"] == tool).unwrap();
        assert_eq!(row["state"], "uncorrelated");
        assert!(row["call_seq"].is_null());
        assert!(
            row["cancel_requested"].is_null(),
            "a token or reused ID cannot prove a client cancel of this generation"
        );
        assert!(row["wire_encode_send_inclusive_ms"].is_null());
        assert!(matches!(
            row["uncorrelated_reason"].as_str(),
            Some("ambiguous_id" | "token_cancelled_before_start")
        ));
    }
}
