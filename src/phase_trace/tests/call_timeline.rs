use super::*;
use std::sync::atomic::{AtomicBool, Ordering};

mod retirement;
mod routing;

#[test]
fn token_cancellation_does_not_prove_a_client_notification() {
    let (trace, captured) = captured_trace();
    trace.received(&numeric(1), "next");
    let ticket = trace.handler_started(&numeric(1), "next", || true);
    trace.handler_settled(ticket, "next", Duration::ZERO, &Phases::default());
    let rows = written(&trace, &captured);
    assert_eq!(rows.len(), 1);
    assert!(rows[0]["cancel_requested"].is_null());
    assert_eq!(
        rows[0]["uncorrelated_reason"],
        "token_cancelled_before_start"
    );
    assert!(rows[0]["call_seq"].is_null());
}

#[test]
fn explicit_client_cancel_survives_duplicate_and_saturation_loss() {
    for duplicate in [false, true] {
        let (trace, captured) = captured_trace();
        trace.received(&numeric(1), "next");
        let ticket = trace.handler_started(&numeric(1), "next", || false);
        trace.client_cancelled(&numeric(1));
        if duplicate {
            trace.received(&numeric(1), "show");
        } else {
            for id in 2..=i64::try_from(MAX_PENDING + 1).unwrap() {
                trace.received(&numeric(id), "next");
            }
            assert!(trace.pending.lock().unwrap().disabled);
        }
        trace.handler_settled(ticket, "next", Duration::ZERO, &Phases::default());
        let rows = written(&trace, &captured);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["cancel_requested"], true);
        assert_eq!(rows[0]["state"], "uncorrelated");
        assert_eq!(
            rows[0]["uncorrelated_reason"],
            if duplicate {
                "ambiguous_id"
            } else {
                "saturated"
            }
        );
    }
}

#[test]
fn a_cancel_after_duplicate_has_no_guessed_generation() {
    let (trace, captured) = captured_trace();
    trace.received(&numeric(1), "next");
    let ticket = trace.handler_started(&numeric(1), "next", || false);
    trace.received(&numeric(1), "show");
    trace.client_cancelled(&numeric(1));
    trace.handler_settled(ticket, "next", Duration::ZERO, &Phases::default());
    let rows = written(&trace, &captured);
    assert_eq!(rows.len(), 1);
    assert!(rows[0]["cancel_requested"].is_null());
}

#[test]
fn unknown_and_dispatched_cancels_do_not_consume_the_pool() {
    let (trace, captured) = captured_trace();
    for id in 0..300 {
        trace.client_cancelled(&numeric(id));
    }
    assert_eq!(trace.pending.lock().unwrap().retained(), 0);
    assert!(!trace.pending.lock().unwrap().disabled);
    settle(&trace, 1, "next");
    send(&trace, 1, true);
    for id in 0..300 {
        trace.client_cancelled(&numeric(id));
    }
    assert_eq!(trace.pending.lock().unwrap().retained(), 0);
    assert!(!trace.pending.lock().unwrap().disabled);
    let rows = written(&trace, &captured);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["state"], "complete");
    assert_eq!(rows[0]["cancel_requested"], false);
}

fn stages(trace: &PhaseTrace, captured: &Captured) -> Vec<serde_json::Value> {
    trace.flush(Duration::from_secs(5));
    captured
        .lines()
        .into_iter()
        .filter(|line| line["engram_mcp_call_stage"] == 1)
        .collect()
}

fn names(rows: &[serde_json::Value]) -> Vec<&str> {
    rows.iter()
        .map(|row| row["stage"].as_str().unwrap())
        .collect()
}

#[test]
fn completed_call_has_correlated_monotonic_boundaries() {
    let (trace, captured) = captured_trace();
    settle(&trace, 1, "next");
    send(&trace, 1, true);
    let rows = stages(&trace, &captured);
    assert_eq!(
        names(&rows),
        [
            "receive",
            "handler_start",
            "handler_settle",
            "send_dispatch",
            "send_finish"
        ]
    );
    assert!(
        rows.iter()
            .all(|row| row["call_seq"] == rows[0]["call_seq"])
    );
    assert!(
        rows.iter()
            .all(|row| row["trace_instance"] == rows[0]["trace_instance"])
    );
    assert!(
        rows.windows(2)
            .all(|pair| pair[0]["server_elapsed_ms"].as_f64().unwrap()
                <= pair[1]["server_elapsed_ms"].as_f64().unwrap())
    );
    assert_eq!(rows[4]["send_succeeded"], true);
}

struct GateTransport(Arc<AtomicBool>);

impl Transport<RoleServer> for GateTransport {
    type Error = std::io::Error;

    fn send(
        &mut self,
        _: TxJsonRpcMessage<RoleServer>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let open = Arc::clone(&self.0);
        std::future::poll_fn(move |_| {
            if open.load(Ordering::Relaxed) {
                Poll::Ready(Ok(()))
            } else {
                Poll::Pending
            }
        })
    }

    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleServer>>> + Send {
        std::future::ready(None)
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        std::future::ready(Ok(()))
    }
}

#[test]
fn unpolled_and_pending_send_drop_keep_their_owned_instance_after_close() {
    for poll in [false, true] {
        let (trace, captured) = captured_trace();
        let trace = Arc::new(trace);
        let mut transport = TracingTransport::new(
            GateTransport(Arc::new(AtomicBool::new(false))),
            Arc::clone(&trace),
        );
        settle(&trace, 2, "show");
        let mut future = Box::pin(transport.send(response(2)));
        assert!(!names(&stages(&trace, &captured)).contains(&"send_finish"));
        if poll {
            let mut context = Context::from_waker(std::task::Waker::noop());
            assert!(future.as_mut().poll(&mut context).is_pending());
        }
        trace.closed();
        drop(future);
        let rows = stages(&trace, &captured);
        assert_eq!(names(&rows).last(), Some(&"send_dropped"));
        assert!(!names(&rows).contains(&"send_finish"));
        let records = written(&trace, &captured);
        assert_eq!(records.len(), 1);
        assert_eq!(records[0]["state"], "send_dropped");
        assert!(records[0]["wire_encode_send_inclusive_ms"].is_null());
    }
}

#[test]
fn concurrent_sends_with_reused_id_do_not_finish_each_others_record() {
    let (trace, captured) = captured_trace();
    let trace = Arc::new(trace);
    let open = Arc::new(AtomicBool::new(false));
    let mut transport = TracingTransport::new(GateTransport(Arc::clone(&open)), Arc::clone(&trace));
    settle(&trace, 3, "note");
    let old = transport.send(response(3)); // Owns and removes the original slot now.
    settle(&trace, 3, "show");
    let new = transport.send(response(3));
    drop(old);
    open.store(true, Ordering::Relaxed);
    let mut new = Box::pin(new);
    let mut context = Context::from_waker(std::task::Waker::noop());
    assert!(matches!(
        new.as_mut().poll(&mut context),
        Poll::Ready(Ok(()))
    ));
    let rows = written(&trace, &captured);
    assert_eq!(rows.len(), 2);
    assert_eq!(rows[0]["state"], "send_dropped");
    assert_eq!(rows[0]["tool"], "note");
    assert_eq!(rows[1]["state"], "complete");
    assert_eq!(rows[1]["tool"], "show");
    assert_ne!(rows[0]["call_seq"], rows[1]["call_seq"]);
}

#[test]
fn strings_and_unobserved_calls_are_unassociated_without_retaining_keys() {
    let (trace, captured) = captured_trace();
    let secret = NumberOrString::String("SELECT PRIVATE C:\\secret ".repeat(10000).into());
    trace.received(&secret, "PRIVATE-tool");
    let ticket = trace.handler_started(&secret, "PRIVATE-tool", || false);
    trace.handler_settled(ticket, "PRIVATE-tool", Duration::ZERO, &Phases::default());
    trace.client_cancelled(&secret);
    let data = trace.dispatching(&secret);
    super::super::call::ended(&trace, data, Some(Instant::now()), Some(false));
    assert_eq!(trace.pending.lock().unwrap().retained(), 0);
    let missing = trace.handler_started(&numeric(99), "next", || false);
    trace.handler_settled(missing, "next", Duration::ZERO, &Phases::default());
    trace.closed();
    let rows = captured.lines();
    assert!(!serde_json::to_string(&rows).unwrap().contains("PRIVATE"));
    assert!(!serde_json::to_string(&rows).unwrap().contains("SELECT"));
    assert!(rows.iter().all(|row| row["call_seq"].is_null()));
    let records = written(&trace, &captured);
    assert!(records.iter().all(|row| row["state"] == "uncorrelated"));
    assert_eq!(records[0]["id"], serde_json::Value::Null);
    assert_eq!(records[1]["uncorrelated_reason"], "missing_receive");
}

#[test]
fn early_cancel_and_nested_error_guards_do_not_borrow_a_replacement() {
    let (trace, captured) = captured_trace();
    trace.received(&numeric(5), "next");
    let ticket = trace.handler_started(&numeric(5), "next", || true);
    trace.handler_settled(ticket, "next", Duration::ZERO, &Phases::default());
    trace.requested(&numeric(5));
    settle(&trace, 5, "show");
    let rows = written(&trace, &captured);
    assert!(rows.iter().all(|row| row["call_seq"].is_null()));
    assert!(rows.iter().all(|row| row["state"] == "uncorrelated"));

    let (result, phases) = block_on(scoped(async {
        let _snapshot = span(Phase::ReadSnapshot);
        let ((), nested) = scoped(async {
            let _reminder = span(Phase::BackupReminder);
        })
        .await;
        assert_eq!(nested.backup_reminder.count, 1);
        Err::<(), _>("unchanged error")
    }));
    assert_eq!(result, Err("unchanged error"));
    assert_eq!(phases.read_snapshot.count, 1);
    assert_eq!(phases.backup_reminder.count, 0);
    assert!(!active());
    assert!(span(Phase::ReadSnapshot).started.is_none());
}

#[test]
fn missing_receive_handler_cannot_claim_a_later_calls_send() {
    for retained in [0, MAX_PENDING] {
        let (trace, captured) = captured_trace();
        for offset in 0..retained {
            trace.received(&numeric(1000 + i64::try_from(offset).unwrap()), "next");
        }
        let id = numeric(77);
        let old = trace.handler_started(&id, "note", || false);
        assert!(old.sequence.is_none());
        assert_eq!(
            trace.pending.lock().unwrap().retained(),
            (retained + 1).min(MAX_PENDING)
        );
        assert_eq!(
            trace.pending.lock().unwrap().disabled,
            retained == MAX_PENDING
        );

        trace.received(&id, "show");
        let new = trace.handler_started(&id, "show", || false);
        trace.handler_settled(new, "show", Duration::ZERO, &Phases::default());
        trace.handler_settled(old, "note", Duration::ZERO, &Phases::default());
        // The old response is dispatched after the newer handler settles.
        // Neither this send nor the following one may claim the newer record.
        send(&trace, 77, true);
        send(&trace, 77, true);
        trace.closed();
        let rows = written(&trace, &captured);
        assert_eq!(rows.len(), 2);
        assert!(rows.iter().all(|row| row["state"] == "uncorrelated"));
        assert!(rows.iter().all(|row| row["call_seq"].is_null()));
        assert!(
            rows.iter()
                .all(|row| row["wire_encode_send_inclusive_ms"].is_null())
        );
        assert!(
            stages(&trace, &captured)
                .iter()
                .filter(|row| row["stage"] == "send_dispatch" || row["stage"] == "send_finish")
                .all(|row| row["call_seq"].is_null())
        );
    }
}

#[test]
fn sequence_exhaustion_cannot_reuse_an_old_ticket() {
    let (trace, captured) = captured_trace();
    trace.pending.lock().unwrap().exhaust_sequence();
    settle(&trace, 6, "next");
    assert!(trace.pending.lock().unwrap().disabled);
    let rows = written(&trace, &captured);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["state"], "uncorrelated");
    assert_eq!(rows[0]["uncorrelated_reason"], "saturated");
    assert!(rows[0]["call_seq"].is_null());
}

#[test]
fn metadata_and_tool_ids_share_one_duplicate_guard_in_both_receive_orders() {
    for metadata_first in [true, false] {
        let (trace, captured) = captured_trace();
        let trace = Arc::new(trace);
        let metadata = client_message(serde_json::json!({
            "jsonrpc": "2.0", "id": 8, "method": "tools/list", "params": {}
        }));
        let tool = client_message(serde_json::json!({
            "jsonrpc": "2.0", "id": 8, "method": "tools/call",
            "params": { "name": "next", "arguments": { "peek": true } }
        }));
        let incoming = if metadata_first {
            [metadata, tool]
        } else {
            [tool, metadata]
        };
        let mut transport = TracingTransport::new(
            SlowTransport {
                delay: Duration::ZERO,
                fail: false,
                closed: false,
                incoming: incoming.into(),
            },
            Arc::clone(&trace),
        );
        assert!(block_on(transport.receive()).is_some());
        assert!(block_on(transport.receive()).is_some());
        let ticket = trace.handler_started(&numeric(8), "next", || false);
        trace.handler_settled(ticket, "next", Duration::ZERO, &Phases::default());
        block_on(transport.send(response(8))).unwrap();
        trace.closed();
        let rows = written(&trace, &captured);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["state"], "uncorrelated");
        assert_eq!(rows[0]["uncorrelated_reason"], "ambiguous_id");
        assert!(rows[0]["wire_encode_send_inclusive_ms"].is_null());
    }
}
