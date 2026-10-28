use super::*;
use crate::phase_trace::call::{self, Ticket};

mod routing;

fn begin(trace: &PhaseTrace, id: i64) -> Ticket {
    trace.received(&numeric(id), "next");
    trace.handler_started(&numeric(id), "next", || false)
}

fn finish(trace: &PhaseTrace, ticket: Ticket) {
    trace.handler_settled(ticket, "next", Duration::ZERO, &Phases::default());
}

fn assert_cancelled_summary(row: &serde_json::Value) {
    assert_eq!(row["state"], "uncorrelated");
    assert_eq!(row["cancel_requested"], true);
    assert!(row["call_seq"].is_null());
    assert!(row["wire_encode_send_inclusive_ms"].is_null());
    assert!(row["dispatch_send_inclusive_ms"].is_null());
}

#[test]
fn completed_cancel_cycles_do_not_exhaust_slots_in_either_order() {
    for settle_first in [false, true] {
        for reuse_id in [false, true] {
            let (trace, captured) = captured_trace();
            let cycles = MAX_PENDING + 20;
            for cycle in 0..cycles {
                let id = if reuse_id {
                    7
                } else {
                    i64::try_from(cycle).unwrap()
                };
                let ticket = begin(&trace, id);
                if settle_first {
                    finish(&trace, ticket);
                }
                trace.client_cancelled(&numeric(id));
                if !settle_first {
                    finish(&trace, ticket);
                }
                assert_eq!(trace.pending.lock().unwrap().retained(), 1);
                send(&trace, id, true);
                assert_eq!(trace.pending.lock().unwrap().retained(), 0);
                assert!(!trace.pending.lock().unwrap().disabled);
                if cycle % 20 == 19 {
                    assert_flushed(&trace.lines, Duration::from_secs(5));
                }
            }
            let rows = written(&trace, &captured);
            assert_eq!(rows.len(), cycles, "one summary per observed lifecycle");
            for row in &rows {
                assert_cancelled_summary(row);
            }
            trace.closed();
            assert_eq!(written(&trace, &captured).len(), cycles);
        }
    }
}

#[test]
fn held_cancelled_summary_is_emitted_at_dispatch_once() {
    for settle_first in [false, true] {
        let (trace, captured) = captured_trace();
        let ticket = begin(&trace, 7);
        if settle_first {
            finish(&trace, ticket);
        }
        trace.client_cancelled(&numeric(7));
        if !settle_first {
            finish(&trace, ticket);
        }
        assert_eq!(written(&trace, &captured).len(), usize::from(!settle_first));
        let data = trace.dispatching(&numeric(7));
        let rows = written(&trace, &captured);
        assert_eq!(rows.len(), 1, "dispatch releases any held summary");
        assert_cancelled_summary(&rows[0]);
        if settle_first {
            let lines = captured.lines();
            let dispatch = lines
                .iter()
                .position(|row| row["stage"] == "send_dispatch")
                .unwrap();
            let summary = lines
                .iter()
                .position(|row| row["engram_mcp_phase_trace"] == 1)
                .unwrap();
            assert!(dispatch < summary);
        }
        trace.closed();
        call::ended(&trace, data, None, None);
        assert_eq!(written(&trace, &captured).len(), 1);
    }
}

#[test]
fn cancelled_send_drop_or_finish_cannot_finalize_a_reused_id() {
    for polled in [false, true] {
        for drop_old in [false, true] {
            for new_first in [false, true] {
                let (trace, captured) = captured_trace();
                let trace = Arc::new(trace);
                let open = Arc::new(AtomicBool::new(false));
                let mut transport =
                    TracingTransport::new(GateTransport(Arc::clone(&open)), Arc::clone(&trace));
                settle(&trace, 7, "note");
                trace.client_cancelled(&numeric(7));
                let mut old = Box::pin(transport.send(response(7)));
                let mut context = Context::from_waker(std::task::Waker::noop());
                if polled {
                    assert!(old.as_mut().poll(&mut context).is_pending());
                }
                assert_eq!(trace.pending.lock().unwrap().retained(), 0);
                settle(&trace, 7, "show");
                let mut new = Box::pin(transport.send(response(7)));
                open.store(true, Ordering::Relaxed);
                if new_first {
                    assert!(matches!(
                        new.as_mut().poll(&mut context),
                        Poll::Ready(Ok(()))
                    ));
                }
                if drop_old {
                    drop(old);
                } else {
                    assert!(matches!(
                        old.as_mut().poll(&mut context),
                        Poll::Ready(Ok(()))
                    ));
                }
                if !new_first {
                    assert!(matches!(
                        new.as_mut().poll(&mut context),
                        Poll::Ready(Ok(()))
                    ));
                }
                let rows = written(&trace, &captured);
                assert_eq!(rows.len(), 2);
                assert_eq!(rows[0]["tool"], "note");
                assert_cancelled_summary(&rows[0]);
                assert_eq!(rows[1]["tool"], "show");
                assert_eq!(rows[1]["state"], "complete");
                assert!(rows[1]["call_seq"].is_number());
                assert_eq!(trace.pending.lock().unwrap().retained(), 0);
            }
        }
    }
}

#[test]
fn ambiguity_cannot_be_erased_by_a_later_cancellation_reason() {
    for settle_first in [false, true] {
        for cancel_first in [false, true] {
            for observation in ["duplicate", "metadata", "invocation", "token"] {
                let (trace, captured) = captured_trace();
                let original = begin(&trace, 7);
                if settle_first {
                    finish(&trace, original);
                }
                if cancel_first {
                    trace.client_cancelled(&numeric(7));
                }
                match observation {
                    "duplicate" => trace.received(&numeric(7), "show"),
                    "metadata" => call::metadata_received(&trace, &numeric(7)),
                    "invocation" => {
                        let _ = trace.handler_started(&numeric(7), "note", || false);
                    }
                    "token" => {
                        let _ = trace.handler_started(&numeric(7), "note", || true);
                    }
                    _ => unreachable!(),
                }
                trace.client_cancelled(&numeric(7));
                if !settle_first {
                    finish(&trace, original);
                    assert_eq!(
                        written(&trace, &captured)[0]["uncorrelated_reason"],
                        "cancel_requested"
                    );
                }
                send(&trace, 7, true);
                assert_eq!(trace.pending.lock().unwrap().retained(), 1, "{observation}");
                trace.received(&numeric(7), "show");
                let replacement = trace.handler_started(&numeric(7), "show", || false);
                assert!(replacement.sequence.is_none());
                trace.handler_settled(replacement, "show", Duration::ZERO, &Phases::default());
                send(&trace, 7, true);
                assert_eq!(
                    written(&trace, &captured).len(),
                    if settle_first { 1 } else { 2 }
                );
                trace.closed();
                let rows = written(&trace, &captured);
                assert_eq!(rows.len(), 2);
                assert!(rows.iter().all(|row| row["state"] == "uncorrelated"));
                assert!(rows.iter().all(|row| row["call_seq"].is_null()));
            }
        }
    }
}

#[test]
fn close_before_dispatch_or_settlement_emits_no_duplicate_summary() {
    for settle_first in [false, true] {
        let (trace, captured) = captured_trace();
        let ticket = begin(&trace, 7);
        if settle_first {
            finish(&trace, ticket);
        }
        trace.client_cancelled(&numeric(7));
        trace.closed();
        if !settle_first {
            finish(&trace, ticket);
        }
        send(&trace, 7, true);
        let rows = written(&trace, &captured);
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["state"], "uncorrelated");
        assert!(rows[0]["call_seq"].is_null());
        assert!(rows[0]["wire_encode_send_inclusive_ms"].is_null());
        assert_eq!(trace.pending.lock().unwrap().retained(), 0);
    }
}

#[test]
fn an_old_cancelled_context_cannot_borrow_a_reused_receive_generation() {
    let (trace, captured) = captured_trace();
    settle(&trace, 7, "note");
    trace.client_cancelled(&numeric(7));
    send(&trace, 7, true);
    trace.received(&numeric(7), "show");
    let old = trace.handler_started(&numeric(7), "note", || true);
    let replacement = trace.handler_started(&numeric(7), "show", || false);
    assert!(old.sequence.is_none());
    assert!(replacement.sequence.is_none());
    trace.handler_settled(old, "note", Duration::ZERO, &Phases::default());
    trace.handler_settled(replacement, "show", Duration::ZERO, &Phases::default());
    send(&trace, 7, true);
    send(&trace, 7, true);
    let rows = written(&trace, &captured);
    assert_eq!(rows.len(), 3);
    assert!(rows.iter().all(|row| row["state"] == "uncorrelated"));
    assert!(rows.iter().all(|row| row["call_seq"].is_null()));
    assert!(rows[1]["cancel_requested"].is_null());
    assert!(rows[2]["cancel_requested"].is_null());
    assert_eq!(trace.pending.lock().unwrap().retained(), 1);
}

#[test]
fn metadata_collision_is_unsafe_in_both_receive_and_response_orders() {
    for metadata_first in [false, true] {
        for metadata_response_first in [false, true] {
            let (trace, captured) = captured_trace();
            if metadata_first {
                call::metadata_received(&trace, &numeric(7));
            }
            trace.received(&numeric(7), "next");
            if !metadata_first {
                call::metadata_received(&trace, &numeric(7));
            }
            let ticket = trace.handler_started(&numeric(7), "next", || false);
            assert!(ticket.sequence.is_none());
            trace.client_cancelled(&numeric(7));
            if metadata_response_first {
                send(&trace, 7, true);
            }
            finish(&trace, ticket);
            send(&trace, 7, true);
            if !metadata_response_first {
                send(&trace, 7, true);
            }
            assert_eq!(trace.pending.lock().unwrap().retained(), 1);
            trace.received(&numeric(7), "show");
            assert!(
                trace
                    .handler_started(&numeric(7), "show", || false)
                    .sequence
                    .is_none()
            );
            let rows = written(&trace, &captured);
            assert_eq!(rows.len(), 1);
            assert_eq!(rows[0]["state"], "uncorrelated");
        }
    }
}

#[test]
fn missing_receive_token_and_premature_response_cannot_be_retired() {
    for observation in ["missing_receive", "token", "premature_response"] {
        let (trace, captured) = captured_trace();
        let ticket = match observation {
            "missing_receive" => trace.handler_started(&numeric(7), "next", || false),
            "token" => {
                trace.received(&numeric(7), "next");
                trace.handler_started(&numeric(7), "next", || true)
            }
            "premature_response" => begin(&trace, 7),
            _ => unreachable!(),
        };
        trace.client_cancelled(&numeric(7));
        if observation == "premature_response" {
            send(&trace, 7, true);
        }
        finish(&trace, ticket);
        send(&trace, 7, true);
        assert_eq!(trace.pending.lock().unwrap().retained(), 1);
        assert_eq!(written(&trace, &captured).len(), 1);
    }
}

#[test]
fn cancellation_without_observed_dispatch_stays_quarantined() {
    let (trace, captured) = captured_trace();
    let ticket = begin(&trace, 7);
    trace.client_cancelled(&numeric(7));
    finish(&trace, ticket);
    assert_eq!(trace.pending.lock().unwrap().retained(), 1);
    assert_cancelled_summary(&written(&trace, &captured)[0]);
    trace.received(&numeric(7), "show");
    assert!(
        trace
            .handler_started(&numeric(7), "show", || false)
            .sequence
            .is_none()
    );
}

#[test]
fn cancelled_dispatch_does_not_reenable_saturated_or_exhausted_tracking() {
    for exhaust_sequence in [false, true] {
        let (trace, captured) = captured_trace();
        let ticket = begin(&trace, 0);
        trace.client_cancelled(&numeric(0));
        if exhaust_sequence {
            trace.pending.lock().unwrap().exhaust_sequence();
            trace.received(&numeric(1), "next");
        } else {
            for id in 1..=i64::try_from(MAX_PENDING).unwrap() {
                trace.received(&numeric(id), "next");
            }
        }
        assert!(trace.pending.lock().unwrap().disabled);
        let retained = trace.pending.lock().unwrap().retained();
        finish(&trace, ticket);
        send(&trace, 0, true);
        assert!(trace.pending.lock().unwrap().disabled);
        assert_eq!(trace.pending.lock().unwrap().retained(), retained);
        assert_cancelled_summary(&written(&trace, &captured)[0]);
    }
}
