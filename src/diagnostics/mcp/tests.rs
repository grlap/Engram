use super::*;
use crate::diagnostics::{EXIT_FLUSH, Surface, tests::records};
mod routing;
mod transport;

#[test]
fn duplicate_handler_first_has_no_receive_generation() {
    let home = crate::test_support::temp_home().unwrap();
    let sink = Diagnostics::start(home.path(), "session-1", Surface::Mcp);
    let observer = Observer::new(sink.clone());
    let id = NumberOrString::Number(7);
    observer.requested(&id, "note", Instant::now());
    observer.requested(&id, "show", Instant::now());
    let replacement = observer.starting(&id, "show", || false);
    let original = observer.starting(&id, "note", || false);
    assert_eq!(replacement.0, None);
    assert_eq!(original.0, None);
    observer.settled(replacement, Some(Fault::tool_error()));
    observer.settled(original, Some(Fault::tool_error()));
    assert!(observer.dispatching(&id, true).is_none());
    observer.finished();
    assert!(sink.flush(EXIT_FLUSH));
    let rows = records(home.path());
    let faults = rows
        .iter()
        .filter(|row| row["event"]["code"] == "tool_error")
        .collect::<Vec<_>>();
    assert_eq!(faults.len(), 2);
    for row in faults {
        assert!(row["event"]["request_seq"].is_null());
        assert_eq!(row["event"]["uncorrelated_reason"], "ambiguous_id");
    }
    assert_eq!(rows.last().unwrap()["loss"]["tracking"], 2);
}

#[test]
fn quarantine_boundary_preserves_fresh_tracking_then_fails_closed() {
    let home = crate::test_support::temp_home().unwrap();
    let sink = Diagnostics::start(home.path(), "session-1", Surface::Mcp);
    let observer = Observer::new(sink.clone());
    for id in 0..i64::try_from(crate::diagnostics::QUEUE_CAPACITY).unwrap() {
        let id = NumberOrString::Number(id);
        observer.requested(&id, "note", Instant::now());
        observer.cancelled(&id);
        assert!(sink.flush(EXIT_FLUSH));
    }
    let fresh = NumberOrString::Number(1000);
    observer.requested(&fresh, "show", Instant::now());
    let ticket = observer.starting(&fresh, "show", || false);
    assert_eq!(ticket.0, Some(257));
    // An unknown cancellation also makes that key unsafe for future reuse.
    observer.cancelled(&NumberOrString::Number(999));
    assert!(observer.dispatching(&fresh, true).is_none());
    assert_eq!(observer.state.lock().unwrap().active, 0);
    observer.requested(&fresh, "note", Instant::now());
    let untracked = observer.starting(&fresh, "note", || false);
    assert_eq!(untracked.0, None);
    observer.settled(untracked, Some(Fault::tool_error()));
    // Already-started ticket provenance remains valid independently of sends.
    observer.settled(ticket, Some(Fault::tool_error()));
    observer.cancelled(&NumberOrString::Number(998));
    observer.finished();
    assert!(sink.flush(EXIT_FLUSH));
    let rows = records(home.path());
    let uncorrelated = rows
        .iter()
        .find(|row| row["event"]["code"] == "tool_error" && row["event"]["operation"] == "note")
        .unwrap();
    assert!(uncorrelated["event"]["request_seq"].is_null());
    assert_eq!(
        uncorrelated["event"]["uncorrelated_reason"],
        "quarantine_overflow"
    );
    let original = rows
        .iter()
        .find(|row| row["event"]["code"] == "tool_error" && row["event"]["operation"] == "show")
        .unwrap();
    assert_eq!(original["event"]["request_seq"], 257);
    assert_eq!(rows.last().unwrap()["loss"]["quarantine_overflow"], 1);
    assert_eq!(rows.last().unwrap()["loss"]["tracking"], 2);
}

#[test]
fn cancelled_before_start_cannot_claim_a_reused_id() {
    let home = crate::test_support::temp_home().unwrap();
    let sink = Diagnostics::start(home.path(), "session-1", Surface::Mcp);
    let observer = Observer::new(sink.clone());
    let id = NumberOrString::Number(7);
    observer.requested(&id, "note", Instant::now());
    observer.cancelled(&id);
    observer.requested(&id, "show", Instant::now());
    let old = observer.starting(&id, "note", || true);
    assert_eq!(old.0, None);
    observer.settled(old, Some(Fault::tool_error()));
    let new = observer.starting(&id, "show", || false);
    assert_eq!(new.0, None);
    assert!(observer.dispatching(&id, false).is_none());
    observer.settled(new, Some(Fault::tool_error()));
    observer.finished();
    assert!(sink.flush(EXIT_FLUSH));
    let rows = records(home.path());
    let late = rows
        .iter()
        .find(|row| row["event"]["code"] == "tool_error")
        .unwrap();
    assert!(late["event"]["request_seq"].is_null());
    assert_eq!(late["event"]["operation"], "note");
    assert_eq!(rows.last().unwrap()["loss"]["tracking"], 1);
    assert_eq!(late["event"]["uncorrelated_reason"], "ambiguous_id");
}

#[test]
fn receive_termination_without_a_runtime_is_checked_at_completion() {
    let home = crate::test_support::temp_home().unwrap();
    let sink = Diagnostics::start(home.path(), "session-1", Surface::Mcp);
    let observer = Observer::new(sink.clone());
    observer.input_ended();
    let eof = observer.state.lock().unwrap().eof.unwrap();
    observer.finished_at(eof + Duration::from_secs(11));
    assert!(sink.flush(EXIT_FLUSH));
    assert_eq!(
        records(home.path())
            .iter()
            .filter(|row| row["event"]["code"] == "shutdown_overrun")
            .count(),
        1
    );
}

#[test]
fn removing_a_middle_request_preserves_moved_tickets_and_reused_ids() {
    let home = crate::test_support::temp_home().unwrap();
    let sink = Diagnostics::start(home.path(), "session-1", Surface::Mcp);
    let observer = Observer::new(sink.clone());
    for id in 0..3 {
        observer.requested(&NumberOrString::Number(id), "show", Instant::now());
    }
    let moved = observer.starting(&NumberOrString::Number(2), "show", || false);
    observer.cancelled(&NumberOrString::Number(1));
    observer.requested(&NumberOrString::Number(3), "show", Instant::now());
    observer.settled(moved, None);
    let dispatched = observer
        .dispatching(&NumberOrString::Number(2), false)
        .unwrap();
    assert_eq!(Some(dispatched.sequence), moved.0);
    assert!(dispatched.settled.is_some());
    observer.requested(&NumberOrString::Number(1), "note", Instant::now());
    observer.requested(&NumberOrString::Number(3), "note", Instant::now());
    {
        let state = observer.state.lock().unwrap();
        assert_eq!(state.active, 1);
        assert!(state.pending[..state.active].iter().all(Option::is_some));
        assert!(state.pending[state.active..].iter().all(Option::is_none));
    }
    assert!(
        observer
            .dispatching(&NumberOrString::Number(1), false)
            .is_none()
    );
    assert!(
        observer
            .dispatching(&NumberOrString::Number(3), false)
            .is_none()
    );
    observer.finished();
    assert_eq!(observer.state.lock().unwrap().active, 0);
    assert!(sink.flush(EXIT_FLUSH));
    assert_eq!(records(home.path()).last().unwrap()["loss"]["tracking"], 3);
}

#[test]
fn handler_errors_are_retained_before_send_and_reused_ids_get_new_ordinals() {
    let home = crate::test_support::temp_home().unwrap();
    let sink = Diagnostics::start(home.path(), "session-11296", Surface::Mcp);
    let observer = Observer::new(sink.clone());
    let id = NumberOrString::Number(7);
    observer.requested(&id, "note", Instant::now());
    let ticket = observer.starting(&id, "note", || false);
    observer.settled(ticket, Some(Fault::tool_error()));
    assert!(sink.flush(EXIT_FLUSH));
    assert_eq!(
        records(home.path())
            .iter()
            .filter(|row| row["event"]["code"] == "tool_error")
            .count(),
        1
    );
    let item = observer.dispatching(&id, false).unwrap();
    observer.sent(item, Instant::now(), Instant::now(), false);
    observer.requested(&id, "show", Instant::now());
    let cancelled_ticket = observer.starting(&id, "show", || false);
    observer.cancelled(&id);
    assert!(observer.dispatching(&id, false).is_none());
    observer.requested(&id, "ls", Instant::now());
    let new = observer.starting(&id, "ls", || false);
    assert_ne!(ticket.0, new.0);
    observer.settled(
        cancelled_ticket,
        Some(Fault::store(&crate::StoreError::DifferentBuildSchema)),
    );
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
    observer.finished();
    assert!(sink.flush(EXIT_FLUSH));
    let rows = records(home.path());
    let cancelled = rows
        .iter()
        .find(|row| row["event"]["code"] == "cancel_requested")
        .unwrap();
    assert_eq!(cancelled["event"]["request_seq"], 2);
    assert_eq!(cancelled["event"]["request_id"], 7);
    let late = rows
        .iter()
        .find(|row| row["event"]["code"] == "control_projection_invalid")
        .unwrap();
    assert_eq!(late["event"]["request_seq"], 2);
    assert_eq!(late["event"]["operation"], "show");
    assert_eq!(
        rows.iter()
            .filter(|row| row["event"]["code"] == "incomplete")
            .count(),
        0
    );
}

#[test]
fn finishing_after_threshold_records_overrun_before_a_delayed_timer() {
    let home = crate::test_support::temp_home().unwrap();
    let sink = Diagnostics::start(home.path(), "session-1", Surface::Mcp);
    let observer = Observer::new(sink.clone());
    let start = Instant::now();
    {
        let mut state = observer.state.lock().unwrap();
        state.eof = Some(start);
        state.stage = "service_waiting_returned";
    }
    observer.finished_at(start + Duration::from_secs(11));
    observer.check_shutdown(start + Duration::from_secs(12));
    observer.finished_at(start + Duration::from_secs(13));
    assert!(sink.flush(EXIT_FLUSH));
    let rows = records(home.path());
    let overrun = rows
        .iter()
        .filter(|row| row["event"]["code"] == "shutdown_overrun")
        .collect::<Vec<_>>();
    assert_eq!(overrun.len(), 1);
    assert_eq!(overrun[0]["event"]["elapsed_ms"], 11000);
    assert_eq!(overrun[0]["event"]["stage"], "service_waiting_returned");
}

#[test]
fn slow_terminal_threshold_is_strict_and_includes_dispatch_and_send() {
    let home = crate::test_support::temp_home().unwrap();
    let sink = Diagnostics::start(home.path(), "session-1", Surface::Mcp);
    let observer = Observer::new(sink.clone());
    let start = Instant::now();
    let id = NumberOrString::Number(1);
    for (sequence, elapsed) in [
        (1, Duration::from_millis(2000)),
        (2, Duration::from_millis(2001)),
    ] {
        observer.requested(&id, "show", start);
        let mut item = observer.dispatching(&id, false).unwrap();
        item.started = Some(start);
        item.settled = Some(start + Duration::from_millis(100));
        assert_eq!(item.sequence, sequence);
        observer.sent(
            item,
            start + Duration::from_millis(500),
            start + elapsed,
            false,
        );
    }
    assert!(sink.flush(EXIT_FLUSH));
    let rows = records(home.path());
    let slow = rows
        .iter()
        .filter(|row| row["event"]["code"] == "slow_call")
        .collect::<Vec<_>>();
    assert_eq!(slow.len(), 1);
    assert_eq!(slow[0]["event"]["elapsed_ms"], 2001);
    assert_eq!(slow[0]["event"]["handler_ms"], 100);
    assert_eq!(slow[0]["event"]["dispatch_wait_ms"], 400);
    assert_eq!(slow[0]["event"]["send_ms"], 1501);
}

#[test]
fn actual_eof_clock_emits_once_only_while_shutdown_is_open() {
    let home = crate::test_support::temp_home().unwrap();
    let sink = Diagnostics::start(home.path(), "session-1", Surface::Mcp);
    let observer = Observer::new(sink.clone());
    let start = Instant::now();
    observer.check_shutdown(start + Duration::from_secs(100));
    {
        let mut state = observer.state.lock().unwrap();
        state.eof = Some(start);
        state.stage = "transport_close_started";
    }
    observer.check_shutdown(start + Duration::from_millis(9999));
    observer.check_shutdown(start + Duration::from_secs(10));
    observer.check_shutdown(start + Duration::from_secs(11));
    observer.finished();
    observer.check_shutdown(start + Duration::from_secs(12));
    assert!(sink.flush(EXIT_FLUSH));
    let rows = records(home.path());
    let overrun = rows
        .iter()
        .filter(|row| row["event"]["code"] == "shutdown_overrun")
        .collect::<Vec<_>>();
    assert_eq!(overrun.len(), 1);
    assert_eq!(overrun[0]["event"]["stage"], "transport_close_started");
    assert_eq!(overrun[0]["event"]["elapsed_ms"], 10000);
}

#[tokio::test]
async fn typed_fault_scope_is_private_and_does_not_enable_store_profiling() {
    let (result, fault) = scoped(async {
        assert!(!crate::phase_trace::active());
        typed_fault(Fault::tool_error());
        42
    })
    .await;
    assert_eq!(result, 42);
    assert!(fault.is_some());
    let ((), fault) = scoped(async {}).await;
    assert!(fault.is_none());
}
