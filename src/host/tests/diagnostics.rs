use super::*;
use crate::diagnostics::{
    Diagnostics, Fingerprint, Surface,
    tests::{drain, records},
};
use std::io::{self, Write};
use std::time::Duration;

struct LostControlReply;
impl Write for LostControlReply {
    fn write(&mut self, _: &[u8]) -> io::Result<usize> {
        panic!("PRIVATE panic payload must not enter the diagnostic");
    }
    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn control_unwind_records_the_lost_reply_and_exact_retry_replays_once() {
    let _ = crate::build_identity::current();
    let home = crate::test_support::temp_home().unwrap();
    let sink = Diagnostics::start(home.path(), "session-11296", Surface::Control);
    let (mut server, request) = diagnostic_server(home.path(), &sink);
    server = server.with_diagnostics(sink.clone());
    sink.emit(crate::diagnostics::Event::failure(
        "work_session_read",
        crate::diagnostics::Correlation::default(),
        Duration::ZERO,
        crate::diagnostics::Fault::slow(),
    ));
    drain(&sink);
    let input = request.to_string() + "\n";
    let failure = server
        .serve(Cursor::new(input.clone()), LostControlReply)
        .unwrap_err();
    assert!(failure.get_ref().unwrap().is::<ControlPanic>());
    let mut refused_output = Vec::new();
    assert!(
        server
            .serve(Cursor::new(input.clone()), &mut refused_output)
            .is_err()
    );
    assert!(refused_output.is_empty(), "an unwound server never resumes");
    assert!(matches!(
        server.handle(HostControlRequest::WorkSessionRead {}),
        Err(StoreError::InvalidHostContext(_))
    ));
    // The async drain precedes the synced append; neither uses serving SQLite.
    drain(&sink);
    let observed = records(home.path());
    let terminal = observed
        .iter()
        .find(|row| row["event"]["code"] == "control_abnormal_exit")
        .unwrap();
    assert_eq!(terminal["event"]["reason"], "panic");
    assert_eq!(terminal["event"]["operation"], "named_root_bind");
    assert_eq!(terminal["event"]["phase"], "response_write");
    assert_eq!(terminal["event"]["frame_seq"], 1);
    assert_eq!(
        terminal["event"]["attempt_fingerprint"],
        serde_json::to_value(Fingerprint::attempt(
            request["idempotency_key"].as_str().unwrap()
        ))
        .unwrap()
    );
    assert_eq!(terminal["producer"]["pid"], std::process::id());
    let queued = observed
        .iter()
        .find(|row| row["event"]["operation"] == "work_session_read")
        .unwrap();
    assert_eq!(
        terminal["producer"], queued["producer"],
        "queued and fatal records share one producer"
    );
    assert!(terminal["loss"].is_object());
    assert!(!terminal.to_string().contains("PRIVATE"));
    drop(server);
    let mut replacement = HostControlServer::open_work_session_with_host_path_identity(
        home.path().join("store.sqlite3"),
        None,
        ProjectId("slow-control".into()),
        "PRIVATE-actor".into(),
        SessionId("session-11296".into()),
        None,
    )
    .unwrap();
    let audit_connection = rusqlite::Connection::open_with_flags(
        home.path().join("store.sqlite3"),
        rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY,
    )
    .unwrap();
    let count: i64 = audit_connection
        .query_row("SELECT count(*) FROM objects", [], |row| row.get(0))
        .unwrap();
    let mut first = Vec::new();
    replacement
        .serve(Cursor::new(input.clone()), &mut first)
        .unwrap();
    let mut second = Vec::new();
    replacement.serve(Cursor::new(input), &mut second).unwrap();
    assert_eq!(first, second, "exact retry returns the original receipt");
    assert_eq!(
        serde_json::from_slice::<Value>(&first).unwrap()["status"],
        "ok"
    );
    let after: i64 = audit_connection
        .query_row("SELECT count(*) FROM objects", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, after, "exact retries add no canonical objects");
}

struct DiagnosticWriter {
    bytes: Vec<u8>,
    flush_delay: Duration,
    fail_write: bool,
    fail_flush: bool,
}

#[test]
fn control_io_failure_is_durable_and_fresh_transport_can_recover() {
    let home = crate::test_support::temp_home().unwrap();
    let sink = Diagnostics::start(home.path(), "session-11296", Surface::Control);
    let (mut server, _) = diagnostic_server(home.path(), &sink);
    let _clock = crate::storage::control_diagnostic_test_clock::start();
    let input = "{\"operation\":\"work_session_read\"}\n";
    let mut writer = DiagnosticWriter::new(Duration::ZERO);
    writer.fail_flush = true;
    assert_eq!(
        server
            .serve(Cursor::new(input), &mut writer)
            .unwrap_err()
            .kind(),
        io::ErrorKind::BrokenPipe
    );
    assert!(server.diagnostic_frame.is_none());
    let mut unreadable_output = Vec::new();
    assert_eq!(
        server
            .serve(FailedInitialRead, &mut unreadable_output)
            .unwrap_err()
            .kind(),
        io::ErrorKind::PermissionDenied
    );
    assert_eq!(unreadable_output, [] as [u8; 0]);
    drain(&sink);
    let rows = records(home.path());
    let terminal = rows
        .iter()
        .find(|row| row["event"]["kind"] == "control_exit")
        .unwrap();
    assert_eq!(terminal["event"]["reason"], "transport_failure");
    assert_eq!(terminal["event"]["phase"], "response_flush");
    assert_eq!(terminal["event"]["frame_seq"], 1);
    assert!(terminal["loss"].is_object());
    let idle = rows
        .iter()
        .filter(|row| row["event"]["kind"] == "control_exit")
        .nth(1)
        .unwrap();
    assert_eq!(idle["event"]["reason"], "transport_failure");
    assert_eq!(idle["event"]["operation"], "unknown");
    assert_eq!(idle["event"]["phase"], "idle_read");
    assert!(idle["event"]["frame_seq"].is_null());
    assert!(idle["event"]["attempt_fingerprint"].is_null());
    assert!(
        server
            .handle(HostControlRequest::WorkSessionRead {})
            .is_ok()
    );
    let mut next_output = Vec::new();
    server.serve(Cursor::new(input), &mut next_output).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&next_output).unwrap()["status"],
        "ok"
    );
}

struct FailedInitialRead;

impl io::Read for FailedInitialRead {
    fn read(&mut self, _: &mut [u8]) -> io::Result<usize> {
        Err(io::ErrorKind::PermissionDenied.into())
    }
}

impl io::BufRead for FailedInitialRead {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        Err(io::ErrorKind::PermissionDenied.into())
    }

    fn consume(&mut self, _: usize) {}
}

#[derive(Default)]
struct PartialControlReply {
    bytes: Vec<u8>,
    failures: usize,
}

impl Write for PartialControlReply {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        if bytes.is_empty() {
            return Ok(0);
        }
        let remaining = 7 - self.bytes.len();
        if remaining == 0 {
            self.failures += 1;
            return Err(io::ErrorKind::BrokenPipe.into());
        }
        let accepted = bytes.len().min(remaining);
        self.bytes.extend_from_slice(&bytes[..accepted]);
        Ok(accepted)
    }

    fn flush(&mut self) -> io::Result<()> {
        Ok(())
    }
}

#[test]
fn control_partial_write_stops_before_the_next_frame_and_exact_retry_replays() {
    let home = crate::test_support::temp_home().unwrap();
    let sink = Diagnostics::start(home.path(), "session-11296", Surface::Control);
    let (mut server, request) = diagnostic_server(home.path(), &sink);
    let input = request.to_string() + "\n";
    let second = "{\"operation\":\"work_session_read\"}\n";
    let mut reader = Cursor::new(input.clone() + second);
    let mut writer = PartialControlReply::default();
    assert_eq!(
        server.serve(&mut reader, &mut writer).unwrap_err().kind(),
        io::ErrorKind::BrokenPipe
    );
    assert_eq!(reader.position(), u64::try_from(input.len()).unwrap());
    assert_eq!(writer.bytes.len(), 7);
    assert_eq!(writer.failures, 1, "no replacement response is attempted");
    assert!(!writer.bytes.contains(&b'\n'));
    let observer = rusqlite::Connection::open(home.path().join("store.sqlite3")).unwrap();
    let committed = crate::storage::test_database_shape_snapshot(&observer).unwrap();
    let mut replay = Vec::new();
    server.serve(Cursor::new(input), &mut replay).unwrap();
    assert_eq!(
        serde_json::from_slice::<Value>(&replay).unwrap()["status"],
        "ok"
    );
    assert_eq!(writer.bytes, replay[..7]);
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&observer).unwrap(),
        committed,
        "fresh transport replays the committed receipt without another write"
    );
    drain(&sink);
    let rows = records(home.path());
    let failure = rows
        .iter()
        .find(|row| row["event"]["kind"] == "control_exit")
        .unwrap();
    assert_eq!(failure["event"]["operation"], "named_root_bind");
    assert_eq!(failure["event"]["phase"], "response_write");
    assert_eq!(failure["event"]["frame_seq"], 1);
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

fn diagnostic_server(home: &std::path::Path, sink: &Diagnostics) -> (HostControlServer, Value) {
    let mut server = HostControlServer::open_work_session_with_host_path_identity(
        home.join("store.sqlite3"),
        None,
        ProjectId("slow-control".into()),
        "PRIVATE-actor".into(),
        SessionId("session-11296".into()),
        None,
    )
    .unwrap();
    let binding = claimed_binding(&mut server);
    let request = root_bind_frame(&binding);
    (
        server.with_diagnostics(sink.clone().capture_transactions_for_test()),
        request,
    )
}

#[test]
fn diagnostics_control_slow_write_and_read_have_exact_isolated_timings() {
    let home = crate::test_support::temp_home().unwrap();
    let sink = Diagnostics::start(home.path(), "session-11296", Surface::Control);
    let (mut server, root_request) = diagnostic_server(home.path(), &sink);
    server
        .store
        .test_control_diagnostic_delays(Duration::from_millis(1_000), Duration::from_millis(1_100));
    let _clock = crate::storage::control_diagnostic_test_clock::start();
    let evaluate = root_request.to_string() + "\n";
    let mut output = DiagnosticWriter::new(Duration::from_millis(200));
    server.serve(Cursor::new(evaluate), &mut output).unwrap();
    let response: serde_json::Value = serde_json::from_slice(&output.bytes).unwrap();
    assert_eq!(response["status"], "ok");
    // The next frame's read snapshot also uses BEGIN; inject no delay there.
    server
        .store
        .test_control_diagnostic_delays(Duration::ZERO, Duration::ZERO);
    let read = serde_json::json!({"operation":"work_session_read"}).to_string() + "\n";
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
    assert_eq!(first["operation"], "named_root_bind");
    assert_eq!(first["frame_seq"], 1);
    assert_eq!(first["elapsed_ms"], 2_300);
    assert_eq!(first["handler_ms"], 2_100);
    assert_eq!(
        first["attempt_fingerprint"],
        serde_json::to_value(Fingerprint::attempt("PRIVATE-root-key")).unwrap()
    );
    assert_eq!(
        first["store_write"],
        serde_json::json!({"attempted":1,"acquired":1,"wait_ms":1000,"hold_ms":1100})
    );
    let second = &events[1]["event"];
    assert_eq!(second["frame_seq"], 2);
    assert_eq!(second["operation"], "work_session_read");
    assert_eq!(second["elapsed_ms"], 2_001);
    assert_eq!(second["handler_ms"], 0);
    assert_eq!(
        second["store_write"],
        serde_json::json!({"attempted":0,"acquired":0,"wait_ms":0,"hold_ms":0})
    );
    let text = serde_json::to_string(&rows).unwrap();
    assert!(!text.contains("PRIVATE"));
}

#[test]
fn diagnostics_control_fast_and_exact_threshold_frames_are_silent() {
    let home = crate::test_support::temp_home().unwrap();
    let sink = Diagnostics::start(home.path(), "session-11296", Surface::Control);
    let (mut server, _root_request) = diagnostic_server(home.path(), &sink);
    let _clock = crate::storage::control_diagnostic_test_clock::start();
    let read = serde_json::json!({"operation":"work_session_read"}).to_string() + "\n";
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
        let (mut server, root_request) = diagnostic_server(home.path(), &sink);
        server
            .store
            .test_control_diagnostic_delays(Duration::from_millis(2_101), Duration::ZERO);
        let _clock = crate::storage::control_diagnostic_test_clock::start();
        let mut request = root_request;
        if store_error {
            request["claim_id"] = serde_json::json!(uuid::Uuid::new_v4());
        } else {
            request["generation"] = 0.into();
        }
        let request = request.to_string() + "\n";
        let mut writer = DiagnosticWriter::new(Duration::from_millis(2_001));
        server.serve(Cursor::new(request), &mut writer).unwrap();
        let response: serde_json::Value = serde_json::from_slice(&writer.bytes).unwrap();
        assert_eq!(response["status"], "error");
        drain(&sink);
        let rows = records(home.path());
        let events = rows
            .iter()
            .filter(|row| row["event"]["kind"] == "failure")
            .filter(|row| row["event"]["frame_seq"] == 1)
            .collect::<Vec<_>>();
        assert_eq!(events.len(), 1, "no extra slow success: {rows:?}");
        let event = &events[0]["event"];
        assert_eq!(event["code"], "named_root_binding_refused");
        assert_eq!(event["elapsed_ms"], if store_error { 2_101 } else { 0 });
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
        let (mut server, root_request) = diagnostic_server(home.path(), &sink);
        let _clock = crate::storage::control_diagnostic_test_clock::start();
        let mut request = root_request;
        request["generation"] = 0.into();
        let request = request.to_string() + "\n";
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
        assert_eq!(codes, ["named_root_binding_refused", "send_failed"]);
    }
}

#[test]
fn diagnostics_control_successful_frame_failed_flush_keeps_only_send_failure() {
    let home = crate::test_support::temp_home().unwrap();
    let sink = Diagnostics::start(home.path(), "session-11296", Surface::Control);
    let (mut server, _root_request) = diagnostic_server(home.path(), &sink);
    let _clock = crate::storage::control_diagnostic_test_clock::start();
    let request = serde_json::json!({"operation":"work_session_read"}).to_string() + "\n";
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
    assert_eq!(event["operation"], "work_session_read");
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
    let input = serde_json::json!({"operation":"work_session_read"}).to_string() + "\n";
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
    assert_eq!(event["operation"], "work_session_read");
    assert_eq!(event["elapsed_ms"], 2_001);
    assert_eq!(event["handler_ms"], 0);
    assert_eq!(event["store_write"]["attempted"], 0);
    assert_eq!(event["store_write"]["acquired"], 0);
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
    let input = format!(
        "{}\n{}\n",
        serde_json::json!({"operation":"named_root_bind", "claim_id":uuid::Uuid::new_v4(),
            "claim_fence":1, "workspace_id":"PRIVATE-workspace", "generation":1,
            "named_at":"2020-01-01T00:00:00Z", "kind":"bound",
            "idempotency_key":"PRIVATE-key"}),
        serde_json::json!({"operation":"named_root_read", "run_id":uuid::Uuid::new_v4(),
            "claim_id":uuid::Uuid::new_v4()})
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
    assert_eq!(error_response["error"]["code"], "named_root_read_refused");
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
    // A frame that parses and is then refused names its operation and its
    // attempt, never its body.
    let refused = rows
        .iter()
        .find(|row| row["event"]["code"] == "named_root_binding_refused")
        .unwrap();
    assert_eq!(refused["event"]["operation"], "named_root_bind");
    assert_eq!(refused["event"]["frame_seq"], 3);
    assert_eq!(
        refused["event"]["attempt_fingerprint"],
        serde_json::to_value(Fingerprint::attempt("PRIVATE-key")).unwrap()
    );
    let error = rows
        .iter()
        .find(|row| row["event"]["code"] == "named_root_read_refused")
        .unwrap();
    assert_eq!(error["event"]["frame_seq"], 4);
    let text = serde_json::to_string(&rows).unwrap();
    assert!(!text.contains("PRIVATE"));
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
    let binding = claimed_binding(&mut server);
    let input = serde_json::to_vec(&root_bind_frame(&binding)).unwrap();
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
        assert_eq!(event["operation"], "named_root_bind");
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
