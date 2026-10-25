use super::*;
use rmcp::ServiceExt as _;
use tokio::sync::{Notify, Semaphore, mpsc};

async fn bounded<T>(future: impl Future<Output = T>) -> T {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .unwrap()
}

// Report each new receive attempt and how many input messages were consumed.
// With the handler blocked and no other input or sends, the receive attempt
// after its completion proves the serve loop processed its queued response.
struct ObservedTransport {
    incoming: mpsc::UnboundedReceiver<RxJsonRpcMessage<RoleServer>>,
    outgoing: mpsc::UnboundedSender<TxJsonRpcMessage<RoleServer>>,
    attempts: mpsc::UnboundedSender<(u64, usize)>,
    epoch: u64,
    consumed: usize,
}

impl Transport<RoleServer> for ObservedTransport {
    type Error = std::io::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleServer>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        std::future::ready(
            self.outgoing
                .send(item)
                .map_err(|_| std::io::Error::other("test client closed")),
        )
    }

    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleServer>>> + Send {
        self.epoch += 1;
        self.attempts.send((self.epoch, self.consumed)).unwrap();
        async move {
            let message = self.incoming.recv().await;
            self.consumed += usize::from(message.is_some());
            message
        }
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        std::future::ready(Ok(()))
    }
}

#[derive(Clone)]
struct CancelledHandler {
    trace: Arc<PhaseTrace>,
    started: Arc<Notify>,
    cancelled: Arc<Notify>,
    finish: Arc<Semaphore>,
}

impl rmcp::ServerHandler for CancelledHandler {
    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
        let ticket = self
            .trace
            .handler_started(&context.id, &request.name, || context.ct.is_cancelled());
        assert!(ticket.sequence.is_some());
        self.started.notify_one();
        context.ct.cancelled().await;
        self.cancelled.notify_one();
        bounded(self.finish.acquire()).await.unwrap().forget();
        self.trace
            .handler_settled(ticket, &request.name, Duration::ZERO, &Phases::default());
        Ok(rmcp::model::CallToolResult::success(Vec::new()).into())
    }
}

async fn receive_after(attempts: &mut mpsc::UnboundedReceiver<(u64, usize)>, previous: u64) -> u64 {
    loop {
        let (epoch, consumed) = bounded(attempts.recv()).await.unwrap();
        if epoch > previous && consumed == 4 {
            return epoch;
        }
    }
}

#[tokio::test]
async fn actual_rmcp_cancelled_response_without_dispatch_stays_quarantined() {
    let (trace, captured) = captured_trace();
    let trace = Arc::new(trace);
    let started = Arc::new(Notify::new());
    let cancelled = Arc::new(Notify::new());
    let finish = Arc::new(Semaphore::new(0));
    let server = CancelledHandler {
        trace: Arc::clone(&trace),
        started: Arc::clone(&started),
        cancelled: Arc::clone(&cancelled),
        finish: Arc::clone(&finish),
    };
    let (input, incoming) = mpsc::unbounded_channel();
    let (outgoing, mut output) = mpsc::unbounded_channel();
    let (attempts, mut receives) = mpsc::unbounded_channel();
    let transport = ObservedTransport {
        incoming,
        outgoing,
        attempts,
        epoch: 0,
        consumed: 0,
    };
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
    input.send(client_message(serde_json::json!({
        "jsonrpc": "2.0", "id": 1, "method": "initialize",
        "params": { "protocolVersion": "2025-06-18", "capabilities": {}, "clientInfo": { "name": "test", "version": "0" } }
    }))).unwrap();
    let initialized = bounded(output.recv()).await.unwrap();
    assert_eq!(serde_json::to_value(initialized).unwrap()["id"], 1);
    input
        .send(client_message(
            serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        ))
        .unwrap();
    input.send(client_message(serde_json::json!({
        "jsonrpc": "2.0", "id": 7, "method": "tools/call", "params": { "name": "next", "arguments": {} }
    }))).unwrap();
    bounded(started.notified()).await;
    input
        .send(client_message(serde_json::json!({
            "jsonrpc": "2.0", "method": "notifications/cancelled", "params": { "requestId": 7 }
        })))
        .unwrap();
    bounded(cancelled.notified()).await;
    // Wait until the serve loop processed the cancellation and is receiving
    // again. The only next event is this handler's queued response.
    let after_cancel = receive_after(&mut receives, 0).await;
    finish.add_permits(1);
    receive_after(&mut receives, after_cancel).await;
    assert!(matches!(
        output.try_recv(),
        Err(mpsc::error::TryRecvError::Empty)
    ));
    assert_eq!(trace.pending.lock().unwrap().retained(), 1);
    let rows = written(&trace, &captured);
    assert_eq!(rows.len(), 1);
    assert_cancelled_summary(&rows[0]);
    assert!(
        !stages(&trace, &captured)
            .iter()
            .any(|row| row["id"] == 7 && row["stage"] == "send_dispatch")
    );
    drop(input);
    bounded(serving).await.unwrap();
}
