use super::*;
use crate::diagnostics::{Diagnostics, EXIT_FLUSH, Fingerprint, Surface, tests::records};
use std::io::{self, Write};

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
    assert!(sink.flush(EXIT_FLUSH));
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
        assert!(self.sink.flush(EXIT_FLUSH));
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
    assert!(sink.flush(EXIT_FLUSH));
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
    assert!(sink.flush(EXIT_FLUSH));
    let rows = records(home.path());
    assert_eq!(
        rows.iter()
            .filter(|row| row["event"]["code"] == "send_failed")
            .count(),
        1
    );
    assert!(!serde_json::to_string(&rows).unwrap().contains("PRIVATE"));
}
