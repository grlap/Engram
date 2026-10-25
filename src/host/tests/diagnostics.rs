use super::*;
use crate::diagnostics::{
    Diagnostics, Fingerprint, Surface,
    tests::{drain, records},
};
use std::io::{self, Write};
use std::time::Duration;

struct DiagnosticWriter {
    bytes: Vec<u8>,
    flush_delay: Duration,
    fail_write: bool,
    fail_flush: bool,
}

impl DiagnosticWriter {
    fn new(flush_delay: Duration) -> Self {
        Self {
            bytes: Vec::new(),
            flush_delay,
            fail_write: false,
            fail_flush: false,
        }
    }
}

impl Write for DiagnosticWriter {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if self.fail_write {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }
    fn flush(&mut self) -> io::Result<()> {
        crate::storage::control_diagnostic_test_clock::advance(self.flush_delay);
        if self.fail_flush {
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        Ok(())
    }
}

fn diagnostic_server(
    home: &std::path::Path,
    sink: &Diagnostics,
) -> (HostControlServer, String, String) {
    let mut server = HostControlServer::open_with_host_path_identity(
        home.join("store.sqlite3"),
        None,
        ProjectId("slow-control".into()),
        "PRIVATE-actor".into(),
        SessionId("session-11296".into()),
        None,
    )
    .unwrap();
    let bound = server
        .handle(HostControlRequest::SessionBind {
            external_ref: "PRIVATE-anchor".into(),
            title: "PRIVATE-title".into(),
            assurance: ControlAssurance::TurnGated,
            mediated_effects: vec![EffectClass::Observe],
            work_binding: None,
            capability_map_revision: 1,
            idempotency_key: "PRIVATE-bind".into(),
        })
        .unwrap();
    let routing = bound["routing_token"].as_str().unwrap().to_owned();
    let granted = server
        .handle(HostControlRequest::TurnEvaluate {
            routing_token: routing.clone(),
            idempotency_key: "PRIVATE-setup".into(),
            intent_fingerprint: ObjectId::from_canonical_bytes(b"diagnostic-fixture").to_string(),
            purpose: None,
            requested_effects: vec![EffectClass::Observe],
            resource_intents: vec![],
        })
        .unwrap();
    let grant = granted["grant"]["grant_id"].as_str().unwrap().to_owned();
    // Exercise frame assertions alongside fully captured transaction records.
    (
        server.with_diagnostics(sink.clone().capture_transactions_for_test()),
        routing,
        grant,
    )
}

#[test]
fn diagnostics_control_slow_evaluate_and_read_have_exact_isolated_timings() {
    let home = crate::test_support::temp_home().unwrap();
    let sink = Diagnostics::start(home.path(), "session-11296", Surface::Control);
    let (mut server, routing, grant) = diagnostic_server(home.path(), &sink);
    server
        .store
        .test_control_diagnostic_delays(Duration::from_millis(1_000), Duration::from_millis(1_100));
    let _clock = crate::storage::control_diagnostic_test_clock::start();
    let evaluate = serde_json::json!({
        "operation":"turn_evaluate", "routing_token":routing,
        "idempotency_key":"PRIVATE-evaluate", "intent_fingerprint":ObjectId::from_canonical_bytes(b"slow-evaluate"),
        "purpose":null, "requested_effects":["observe"], "resource_intents":[]
    }).to_string() + "\n";
    let mut output = DiagnosticWriter::new(Duration::from_millis(200));
    server.serve(Cursor::new(evaluate), &mut output).unwrap();
    let response: serde_json::Value = serde_json::from_slice(&output.bytes).unwrap();
    assert_eq!(response["status"], "ok");
    // The next frame's read snapshot also uses BEGIN; inject no delay there.
    server
        .store
        .test_control_diagnostic_delays(Duration::ZERO, Duration::ZERO);
    let read = serde_json::json!({"operation":"turn_grant_read", "routing_token":routing, "grant_id":grant}).to_string() + "\n";
    let mut read_output = DiagnosticWriter::new(Duration::from_millis(2_001));
    server.serve(Cursor::new(read), &mut read_output).unwrap();
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&read_output.bytes).unwrap()["status"],
        "ok"
    );
    drain(&sink);
    let rows = records(home.path());
    assert!(
        rows.iter()
            .any(|row| row["event"]["kind"] == "store_transaction")
    );
    let events = rows
        .iter()
        .filter(|row| row["event"]["kind"] == "failure")
        .filter(|row| !row["event"]["frame_seq"].is_null())
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 2, "{rows:?}");
    let first = &events[0]["event"];
    assert_eq!(first["code"], "slow_call");
    assert_eq!(first["operation"], "turn_evaluate");
    assert_eq!(first["frame_seq"], 1);
    assert_eq!(first["elapsed_ms"], 2_300);
    assert_eq!(first["handler_ms"], 2_100);
    assert_eq!(
        first["attempt_fingerprint"],
        serde_json::to_value(Fingerprint::attempt("PRIVATE-evaluate")).unwrap()
    );
    assert_eq!(
        first["store_write"],
        serde_json::json!({"attempted":1,"acquired":1,"wait_ms":1000,"hold_ms":1100})
    );
    let second = &events[1]["event"];
    assert_eq!(second["frame_seq"], 2);
    assert_eq!(second["operation"], "turn_grant_read");
    assert_eq!(second["elapsed_ms"], 2_001);
    assert_eq!(second["handler_ms"], 0);
    assert_eq!(
        second["store_write"],
        serde_json::json!({"attempted":0,"acquired":0,"wait_ms":0,"hold_ms":0})
    );
    let text = serde_json::to_string(&rows).unwrap();
    assert!(!text.contains("PRIVATE"));
    assert!(!text.contains(&routing));
}

#[test]
fn diagnostics_control_fast_and_exact_threshold_frames_are_silent() {
    let home = crate::test_support::temp_home().unwrap();
    let sink = Diagnostics::start(home.path(), "session-11296", Surface::Control);
    let (mut server, routing, grant) = diagnostic_server(home.path(), &sink);
    let _clock = crate::storage::control_diagnostic_test_clock::start();
    let read = serde_json::json!({"operation":"turn_grant_read", "routing_token":routing, "grant_id":grant}).to_string() + "\n";
    for delay in [
        Duration::ZERO,
        crate::diagnostics::SLOW_CALL,
        crate::diagnostics::SLOW_CALL + Duration::from_nanos(1),
    ] {
        server
            .serve(Cursor::new(&read), DiagnosticWriter::new(delay))
            .unwrap();
    }
    drain(&sink);
    let rows = records(home.path());
    let events = rows
        .iter()
        .filter(|row| row["event"]["kind"] == "failure")
        .filter(|row| !row["event"]["frame_seq"].is_null())
        .collect::<Vec<_>>();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0]["event"]["frame_seq"], 3);
    assert_eq!(events[0]["event"]["code"], "slow_call");
    assert_eq!(
        events[0]["event"]["elapsed_ms"], 2_000,
        "comparison precedes ms rounding"
    );
}

#[test]
fn diagnostics_control_slow_typed_and_store_refusals_keep_fault_records_only() {
    for store_error in [false, true] {
        let home = crate::test_support::temp_home().unwrap();
        let sink = Diagnostics::start(home.path(), "session-11296", Surface::Control);
        let (mut server, routing, grant) = diagnostic_server(home.path(), &sink);
        server
            .store
            .test_control_diagnostic_delays(Duration::from_millis(2_101), Duration::ZERO);
        let _clock = crate::storage::control_diagnostic_test_clock::start();
        let request = serde_json::json!({
            "operation":"turn_begin", "routing_token":routing,
            "grant_id":if store_error { uuid::Uuid::new_v4().to_string() } else { grant },
            "delivery_tokens":["PRIVATE-delivery"], "idempotency_key":"PRIVATE-refusal"
        })
        .to_string()
            + "\n";
        let mut writer = DiagnosticWriter::new(Duration::from_millis(2_001));
        server.serve(Cursor::new(request), &mut writer).unwrap();
        let response: serde_json::Value = serde_json::from_slice(&writer.bytes).unwrap();
        assert_eq!(response["status"], if store_error { "error" } else { "ok" });
        drain(&sink);
        let rows = records(home.path());
        let events = rows
            .iter()
            .filter(|row| row["event"]["kind"] == "failure")
            .filter(|row| row["event"]["frame_seq"] == 1)
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 1, "no extra slow success: {rows:?}");
        let event = &events[0]["event"];
        assert_eq!(
            event["code"],
            if store_error {
                "turn_grant_not_found"
            } else {
                "grant_scope_mismatch"
            }
        );
        assert_eq!(event["elapsed_ms"], 2_101);
        assert!(event["handler_ms"].is_null());
        assert!(!event.as_object().unwrap().contains_key("store_write"));
        assert!(!serde_json::to_string(&rows).unwrap().contains("PRIVATE"));
    }
}

#[test]
fn diagnostics_control_failed_delivery_never_adds_a_slow_success() {
    for write_failure in [false, true] {
        let home = crate::test_support::temp_home().unwrap();
        let sink = Diagnostics::start(home.path(), "session-11296", Surface::Control);
        let (mut server, routing, grant) = diagnostic_server(home.path(), &sink);
        let _clock = crate::storage::control_diagnostic_test_clock::start();
        let request =
            serde_json::json!({"operation":"turn_begin", "routing_token":routing, "grant_id":grant,
            "delivery_tokens":["PRIVATE-delivery"], "idempotency_key":"PRIVATE-refusal"})
            .to_string()
                + "\n";
        let mut writer = DiagnosticWriter::new(Duration::from_millis(2_001));
        writer.fail_write = write_failure;
        writer.fail_flush = !write_failure;
        assert_eq!(
            server
                .serve(Cursor::new(request), writer)
                .unwrap_err()
                .kind(),
            io::ErrorKind::BrokenPipe
        );
        drain(&sink);
        let rows = records(home.path());
        let codes = rows
            .iter()
            .filter(|row| row["event"]["kind"] == "failure")
            .filter(|row| row["event"]["frame_seq"] == 1)
            .map(|row| row["event"]["code"].as_str().unwrap())
            .collect::<Vec<_>>();
        assert_eq!(codes, ["grant_scope_mismatch", "send_failed"]);
    }
}

#[test]
fn diagnostics_control_successful_frame_failed_flush_keeps_only_send_failure() {
    let home = crate::test_support::temp_home().unwrap();
    let sink = Diagnostics::start(home.path(), "session-11296", Surface::Control);
    let (mut server, routing, grant) = diagnostic_server(home.path(), &sink);
    let _clock = crate::storage::control_diagnostic_test_clock::start();
    let request = serde_json::json!({
        "operation":"turn_grant_read", "routing_token":routing, "grant_id":grant
    })
    .to_string()
        + "\n";
    let mut writer = DiagnosticWriter::new(Duration::from_millis(2_001));
    writer.fail_flush = true;
    assert_eq!(
        server
            .serve(Cursor::new(request), &mut writer)
            .unwrap_err()
            .kind(),
        io::ErrorKind::BrokenPipe
    );
    let response: serde_json::Value = serde_json::from_slice(&writer.bytes).unwrap();
    assert_eq!(response["status"], "ok");
    drain(&sink);
    let rows = records(home.path());
    let events = rows
        .iter()
        .filter(|row| row["event"]["kind"] == "failure")
        .filter(|row| row["event"]["frame_seq"] == 1)
        .collect::<Vec<_>>();
    assert_eq!(
        events.len(),
        1,
        "no slow success after failed flush: {rows:?}"
    );
    let event = &events[0]["event"];
    assert_eq!(event["operation"], "turn_grant_read");
    assert_eq!(event["code"], "send_failed");
    assert_eq!(event["elapsed_ms"], 2_001);
    assert!(event["handler_ms"].is_null());
    assert!(!event.as_object().unwrap().contains_key("store_write"));
}

#[test]
fn diagnostics_control_slow_success_is_retained_after_response_flush() {
    struct SlowFlush(Vec<u8>);
    impl Write for SlowFlush {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.0.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            crate::storage::control_diagnostic_test_clock::advance(Duration::from_millis(2_001));
            Ok(())
        }
    }
    let home = crate::test_support::temp_home().unwrap();
    let sink = Diagnostics::start(home.path(), "session-11296", Surface::Control);
    let mut server = HostControlServer::open_with_host_path_identity(
        home.path().join("store.sqlite3"),
        None,
        ProjectId("slow-success".into()),
        "PRIVATE-actor".into(),
        SessionId("session-11296".into()),
        None,
    )
    .unwrap()
    .with_diagnostics(sink.clone());
    let input = serde_json::json!({
        "operation": "session_bind", "external_ref": "PRIVATE-anchor",
        "title": "PRIVATE-title", "assurance": "turn_gated",
        "mediated_effects": ["observe"], "work_binding": null,
        "capability_map_revision": 1, "idempotency_key": "PRIVATE-key"
    })
    .to_string()
        + "\n";
    let _clock = crate::storage::control_diagnostic_test_clock::start();
    let mut output = SlowFlush(Vec::new());
    server.serve(Cursor::new(input), &mut output).unwrap();
    let response: serde_json::Value = serde_json::from_slice(&output.0).unwrap();
    assert_eq!(response["status"], "ok");
    drain(&sink);
    let rows = records(home.path());
    let events = rows
        .iter()
        .filter(|row| row["event"]["kind"] == "failure")
        .filter(|row| row["event"]["frame_seq"] == 1)
        .collect::<Vec<_>>();
    assert_eq!(
        events.len(),
        1,
        "slow success must emit one record: {rows:?}"
    );
    let event = &events[0]["event"];
    assert_eq!(event["code"], "slow_call");
    assert_eq!(event["operation"], "session_bind");
    assert_eq!(event["elapsed_ms"], 2_001);
    assert_eq!(event["handler_ms"], 0);
    assert_eq!(event["store_write"]["attempted"], 1);
    assert_eq!(event["store_write"]["acquired"], 1);
    assert!(!serde_json::to_string(&rows).unwrap().contains("PRIVATE"));
}

#[test]
fn diagnostics_default_large_control_response_streams_without_one_body_write() {
    struct Chunks {
        bytes: Vec<u8>,
        largest: usize,
    }
    impl Write for Chunks {
        fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
            self.largest = self.largest.max(bytes.len());
            self.bytes.extend_from_slice(bytes);
            Ok(bytes.len())
        }
        fn flush(&mut self) -> io::Result<()> {
            Ok(())
        }
    }
    let response = HostControlResponse::Ok {
        result: serde_json::json!({"entries": vec![123; 20_000]}),
    };
    let expected = serde_json::to_vec(&response).unwrap();
    let mut writer = Chunks {
        bytes: Vec::new(),
        largest: 0,
    };
    stream_control_response(&mut writer, &response).unwrap();
    assert_eq!(writer.bytes, expected);
    assert!(writer.largest < 128);
    assert!(expected.len() > 80_000);
}

struct Failed<'a> {
    sink: &'a Diagnostics,
    home: &'a std::path::Path,
}

#[test]
fn diagnostics_control_read_failure_before_a_frame_is_retained() {
    struct Unreadable;
    impl io::Read for Unreadable {
        fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
            Err(io::ErrorKind::PermissionDenied.into())
        }
    }
    impl io::BufRead for Unreadable {
        fn fill_buf(&mut self) -> io::Result<&[u8]> {
            Err(io::ErrorKind::PermissionDenied.into())
        }
        fn consume(&mut self, _: usize) {}
    }
    let home = crate::test_support::temp_home().unwrap();
    let sink = Diagnostics::start(home.path(), "session-1", Surface::Control);
    let mut server = HostControlServer::open_with_host_path_identity(
        home.path().join("store.sqlite3"),
        None,
        ProjectId("read-error".into()),
        "actor".into(),
        SessionId("session-1".into()),
        None,
    )
    .unwrap()
    .with_diagnostics(sink.clone());
    let mut output = Vec::new();
    assert_eq!(
        server.serve(Unreadable, &mut output).unwrap_err().kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(output.len(), 0);
    drain(&sink);
    let rows = records(home.path());
    let fault = rows
        .iter()
        .find(|row| row["event"]["code"] == "transport_failed")
        .unwrap();
    assert!(fault["event"]["frame_seq"].is_null());
    assert_eq!(fault["event"]["operation"], "unknown");
}
impl Write for Failed<'_> {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        drain(self.sink);
        assert_eq!(
            records(self.home)
                .iter()
                .filter(|row| row["event"]["code"] == "invalid_request")
                .count(),
            1
        );
        Err(io::ErrorKind::BrokenPipe.into())
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn diagnostics_control_refusals_keep_wire_bytes_and_safe_frame_correlation() {
    let home = crate::test_support::temp_home().unwrap();
    let sink = Diagnostics::start(home.path(), "session-11296", Surface::Control);
    let mut server = HostControlServer::open_with_host_path_identity(
        home.path().join("store.sqlite3"),
        None,
        ProjectId("diagnostics-control".into()),
        "PRIVATE-actor".into(),
        SessionId("session-11296".into()),
        None,
    )
    .unwrap();
    let bound = server
        .handle(HostControlRequest::SessionBind {
            external_ref: "PRIVATE-anchor".into(),
            title: "PRIVATE-title".into(),
            assurance: ControlAssurance::TurnGated,
            mediated_effects: vec![EffectClass::Observe],
            work_binding: None,
            capability_map_revision: 1,
            idempotency_key: "PRIVATE-bind".into(),
        })
        .unwrap();
    let routing = bound["routing_token"].as_str().unwrap();
    let grant = server
        .handle(HostControlRequest::TurnEvaluate {
            routing_token: routing.into(),
            idempotency_key: "PRIVATE-eval".into(),
            intent_fingerprint: ObjectId::from_canonical_bytes(b"diagnostics").to_string(),
            purpose: None,
            requested_effects: vec![EffectClass::Observe],
            resource_intents: vec![],
        })
        .unwrap();
    let grant = grant["grant"]["grant_id"].as_str().unwrap();
    let input = format!(
        "{}\n{}\n",
        serde_json::json!({"operation":"turn_begin","routing_token":routing,"grant_id":grant,"delivery_tokens":["PRIVATE-delivery"],"idempotency_key":"PRIVATE-key"}),
        serde_json::json!({"operation":"turn_begin","routing_token":routing,"grant_id":uuid::Uuid::new_v4(),"delivery_tokens":[],"idempotency_key":"PRIVATE-missing-grant"})
    );
    let mut baseline = Vec::new();
    server.serve(Cursor::new(&input), &mut baseline).unwrap();
    let error_response: serde_json::Value = serde_json::from_str(
        std::str::from_utf8(&baseline)
            .unwrap()
            .lines()
            .nth(1)
            .unwrap(),
    )
    .unwrap();
    assert_eq!(error_response["error"]["code"], "turn_grant_not_found");
    server = server.with_diagnostics(sink.clone());
    let mut observed = Vec::new();
    server.serve(Cursor::new(&input), &mut observed).unwrap();
    assert_eq!(baseline, observed);
    let trace = crate::phase_trace::control::ControlTrace::with_sink(
        Box::new(io::sink()),
        1024,
        crate::phase_trace::control::ProgressTiming::DEFAULT,
    );
    trace.startup_finished(None);
    let mut traced_bytes = Vec::new();
    server
        .serve_traced(Cursor::new(&input), &mut traced_bytes, &trace)
        .unwrap();
    trace.close();
    assert_eq!(baseline, traced_bytes);
    drain(&sink);
    let rows = records(home.path());
    let refused = rows
        .iter()
        .find(|row| row["event"]["code"] == "grant_scope_mismatch")
        .unwrap();
    assert_eq!(refused["event"]["operation"], "turn_begin");
    assert_eq!(refused["event"]["frame_seq"], 3);
    assert_eq!(refused["event"]["grant_id"], grant);
    assert_eq!(
        refused["event"]["attempt_fingerprint"],
        serde_json::to_value(Fingerprint::attempt("PRIVATE-key")).unwrap()
    );
    let error = rows
        .iter()
        .find(|row| row["event"]["code"] == "turn_grant_not_found")
        .unwrap();
    assert_eq!(error["event"]["frame_seq"], 4);
    let text = serde_json::to_string(&rows).unwrap();
    assert!(!text.contains("PRIVATE"));
    assert!(!text.contains(routing));
    assert!(refused["event"]["observed_at"].as_str().is_some());
}

#[test]
fn diagnostics_control_transaction_scope_keeps_exact_frame_and_wire_replay() {
    let home = crate::test_support::temp_home().unwrap();
    let database = home.path().join("PRIVATE-store.sqlite3");
    let sink = Diagnostics::start(home.path(), "session-1", Surface::Control)
        .capture_transactions_for_test();
    let mut server = HostControlServer::open_with_host_path_identity(
        &database,
        None,
        ProjectId("transaction-wire".into()),
        "PRIVATE-actor".into(),
        SessionId("session-1".into()),
        None,
    )
    .unwrap();
    let input = serde_json::to_vec(&serde_json::json!({
        "operation":"session_bind", "external_ref":"PRIVATE-anchor", "title":"PRIVATE-title",
        "assurance":"turn_gated", "mediated_effects":["observe"], "capability_map_revision":1,
        "idempotency_key":"PRIVATE-bind"
    }))
    .unwrap();
    let wire = [input, vec![b'\n']].concat();
    let mut baseline = Vec::new();
    server.serve(Cursor::new(&wire), &mut baseline).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&baseline).unwrap()["status"],
        "ok"
    );
    server = server.with_diagnostics(sink.clone());
    let mut replay = Vec::new();
    server.serve(Cursor::new(&wire), &mut replay).unwrap();
    assert_eq!(baseline, replay);
    drain(&sink);
    let rows = records(home.path());
    let transactions: Vec<_> = rows
        .iter()
        .filter(|r| r["event"]["kind"] == "store_transaction")
        .collect();
    assert_ne!(transactions, Vec::<&Value>::new());
    for row in transactions {
        let event = &row["event"];
        assert_eq!(event["operation"], "session_bind");
        assert_eq!(event["frame_seq"], 2);
        assert_eq!(event["store_transaction"]["boundary"], "control_mutation");
        assert_eq!(
            event["store_transaction"]["store_path_fingerprint"],
            serde_json::to_value(Fingerprint::store(&database)).unwrap()
        );
    }
    assert!(!serde_json::to_string(&rows).unwrap().contains("PRIVATE"));
}

#[test]
fn diagnostics_control_parse_refusal_is_queued_before_failed_response_delivery() {
    let home = crate::test_support::temp_home().unwrap();
    let sink = Diagnostics::start(home.path(), "session-1", Surface::Control);
    let mut server = HostControlServer::open_with_host_path_identity(
        home.path().join("store.sqlite3"),
        None,
        ProjectId("diagnostics-send".into()),
        "actor".into(),
        SessionId("session-1".into()),
        None,
    )
    .unwrap()
    .with_diagnostics(sink.clone());
    assert_eq!(
        server
            .serve(
                Cursor::new(b"{PRIVATE malformed input}\n"),
                Failed {
                    sink: &sink,
                    home: home.path()
                }
            )
            .unwrap_err()
            .kind(),
        io::ErrorKind::BrokenPipe
    );
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
