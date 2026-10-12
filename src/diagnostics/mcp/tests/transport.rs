use super::*;
use rmcp::{
    RoleServer,
    model::JsonRpcMessage,
    service::{RxJsonRpcMessage, TxJsonRpcMessage},
    transport::Transport,
};
use std::{collections::VecDeque, future::Future, io};

struct Fake {
    messages: VecDeque<RxJsonRpcMessage<RoleServer>>,
    fail: bool,
    close: Option<Arc<tokio::sync::Notify>>,
}
impl Transport<RoleServer> for Fake {
    type Error = io::Error;
    fn send(
        &mut self,
        _: TxJsonRpcMessage<RoleServer>,
    ) -> impl Future<Output = io::Result<()>> + Send + 'static {
        let fail = self.fail;
        async move {
            if fail {
                Err(io::ErrorKind::BrokenPipe.into())
            } else {
                Ok(())
            }
        }
    }
    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleServer>>> + Send {
        let message = self.messages.pop_front();
        async move { message }
    }
    fn close(&mut self) -> impl Future<Output = io::Result<()>> + Send {
        let close = self.close.clone();
        async move {
            if let Some(close) = close {
                close.notified().await;
            }
            Ok(())
        }
    }
}
fn request(id: i64) -> RxJsonRpcMessage<RoleServer> {
    serde_json::from_value(serde_json::json!({"jsonrpc":"2.0","id":id,"method":"tools/call","params":{"name":"note","arguments":{}}})).unwrap()
}
fn response(id: i64, error: bool) -> TxJsonRpcMessage<RoleServer> {
    serde_json::from_value(if error { serde_json::json!({"jsonrpc":"2.0","id":id,"error":{"code":-32602,"message":"PRIVATE-error-text"}}) } else { serde_json::json!({"jsonrpc":"2.0","id":id,"result":{}}) }).unwrap()
}

#[tokio::test]
async fn error_is_queued_before_send_poll_and_delivery_failures_remain_errors() {
    let home = crate::test_support::temp_home().unwrap();
    let sink = Diagnostics::start(home.path(), "session-1", Surface::Mcp);
    let observer = Observer::new(sink.clone());
    let original = request(2);
    let expected = serde_json::to_value(&original).unwrap();
    let mut transport = DiagnosticTransport::new(
        Fake {
            messages: VecDeque::from([original]),
            fail: true,
            close: None,
        },
        Arc::clone(&observer),
    );
    assert_eq!(
        serde_json::to_value(transport.receive().await.unwrap()).unwrap(),
        expected
    );
    let sending = transport.send(response(2, true));
    drain(&sink);
    assert_eq!(
        records(home.path())
            .iter()
            .filter(|row| row["event"]["code"] == "rpc_error")
            .count(),
        1
    );
    assert_eq!(sending.await.unwrap_err().kind(), io::ErrorKind::BrokenPipe);
    drain(&sink);
    let rows = records(home.path());
    assert_eq!(
        rows.iter()
            .filter(|row| row["event"]["code"] == "send_failed")
            .count(),
        1
    );
    assert!(!serde_json::to_string(&rows).unwrap().contains("PRIVATE"));
}

#[tokio::test]
async fn abandoned_send_is_incomplete_and_string_id_never_leaks() {
    let home = crate::test_support::temp_home().unwrap();
    let sink = Diagnostics::start(home.path(), "session-1", Surface::Mcp);
    let observer = Observer::new(sink.clone());
    let mut transport = DiagnosticTransport::new(
        Fake {
            messages: VecDeque::new(),
            fail: false,
            close: None,
        },
        Arc::clone(&observer),
    );
    observer.requested(&NumberOrString::Number(3), "show", Instant::now());
    drop(transport.send(response(3, false)));
    let private_id = NumberOrString::String(Arc::from("PRIVATE-token"));
    observer.requested(&private_id, "PRIVATE-word", Instant::now());
    observer.cancelled(&private_id);
    observer.finished();
    drain(&sink);
    let rows = records(home.path());
    assert_eq!(
        rows.iter()
            .filter(|row| row["event"]["code"] == "incomplete")
            .count(),
        1
    );
    let cancellations = rows
        .iter()
        .filter(|row| row["event"]["code"] == "cancel_requested")
        .collect::<Vec<_>>();
    assert_eq!(cancellations.len(), 1);
    assert!(cancellations[0]["event"]["request_id"].is_null());
    assert_eq!(cancellations[0]["event"]["request_seq"], 2);
    assert!(!serde_json::to_string(&rows).unwrap().contains("PRIVATE"));
}

#[tokio::test]
async fn observed_eof_and_open_close_report_overrun_without_completing_the_close() {
    let home = crate::test_support::temp_home().unwrap();
    let sink = Diagnostics::start(home.path(), "session-1", Surface::Mcp);
    let observer =
        Observer::thresholds(sink.clone(), super::super::super::SLOW_CALL, Duration::ZERO);
    let release = Arc::new(tokio::sync::Notify::new());
    let mut transport = DiagnosticTransport::new(
        Fake {
            messages: VecDeque::new(),
            fail: false,
            close: Some(Arc::clone(&release)),
        },
        Arc::clone(&observer),
    );
    assert!(transport.receive().await.is_none());
    let mut closing = std::pin::pin!(transport.close());
    std::future::poll_fn(|context| {
        assert!(closing.as_mut().poll(context).is_pending());
        std::task::Poll::Ready(())
    })
    .await;
    observer.check_shutdown(Instant::now());
    drain(&sink);
    let rows = records(home.path());
    let overrun = rows
        .iter()
        .filter(|row| row["event"]["code"] == "shutdown_overrun")
        .collect::<Vec<_>>();
    assert_eq!(overrun.len(), 1);
    assert_eq!(overrun[0]["event"]["stage"], "transport_close_started");
    release.notify_one();
    closing.await.unwrap();
    observer.finished();
    drain(&sink);
    assert_eq!(
        records(home.path())
            .iter()
            .filter(|row| row["event"]["code"] == "shutdown_overrun")
            .count(),
        1
    );
}

#[test]
fn pending_tracking_is_bounded_and_discloses_overflow() {
    let home = crate::test_support::temp_home().unwrap();
    let sink = Diagnostics::start(home.path(), "session-1", Surface::Mcp);
    let observer = Observer::new(sink.clone());
    for id in 0..=i64::try_from(super::super::super::QUEUE_CAPACITY).unwrap() {
        observer.requested(&NumberOrString::Number(id), "note", Instant::now());
    }
    assert_eq!(
        observer
            .state
            .lock()
            .unwrap()
            .pending
            .iter()
            .flatten()
            .count(),
        super::super::super::QUEUE_CAPACITY
    );
    let lost = observer.starting(&NumberOrString::Number(999), "show", || false);
    observer.settled(lost, Some(Fault::tool_error()));
    drain(&sink);
    let rows = records(home.path());
    assert_eq!(rows.last().unwrap()["loss"]["tracking"], 1);
    assert!(rows[0]["event"]["request_seq"].is_null());
    // Drop without emitting the remaining pending records: overflow itself is
    // covered without turning this into a queue-saturation timing test.
    assert!(matches!(response(1, false), JsonRpcMessage::Response(_)));
}
