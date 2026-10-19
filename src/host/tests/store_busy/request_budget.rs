use std::{
    cell::Cell,
    io::{self, BufRead, Read, Write},
    time::{Duration, Instant},
};

use super::*;
use crate::storage::ControlRequestScope;

thread_local! { static CLOCK: Cell<Instant> = Cell::new(Instant::now()); }
fn now() -> Instant {
    CLOCK.with(Cell::get)
}
fn slow_phase() {
    CLOCK.with(|clock| clock.set(clock.get() + Duration::from_secs(21)));
}
fn pre_apply_delay() {
    CLOCK.with(|clock| clock.set(clock.get() + Duration::from_millis(15_001)));
}

/// Input is available before the frame starts; the remaining read then stalls.
struct DelayedFrame {
    input: Cursor<Vec<u8>>,
    first_byte_seen: bool,
    delayed: bool,
}

impl Read for DelayedFrame {
    fn read(&mut self, bytes: &mut [u8]) -> io::Result<usize> {
        self.input.read(bytes)
    }
}

impl BufRead for DelayedFrame {
    fn fill_buf(&mut self) -> io::Result<&[u8]> {
        if self.first_byte_seen && !self.delayed {
            pre_apply_delay();
            self.delayed = true;
        }
        self.first_byte_seen = true;
        self.input.fill_buf()
    }

    fn consume(&mut self, count: usize) {
        self.input.consume(count);
    }
}

#[test]
fn expired_control_frame_applies_nothing_and_exact_retry_preserves_authority() {
    for operation in [
        "session_bind",
        "turn_evaluate",
        "turn_begin",
        "turn_checkpoint",
    ] {
        CLOCK.with(|clock| clock.set(Instant::now()));
        let _clock = ControlRequestScope::with_clock(now);
        let mut fixture = Fixture::new();
        let request = if operation == "session_bind" {
            bind_frame("bind")
        } else {
            let routing = fixture.bind();
            let grant = fixture.issue(&routing);
            match operation {
                "turn_evaluate" => evaluate_frame(&routing, "replacement"),
                "turn_begin" => begin_frame(&routing, &grant),
                _ => {
                    fixture.success(&begin_frame(&routing, &grant));
                    serde_json::json!({
                        "operation":"turn_checkpoint", "routing_token":routing,
                        "grant_id":grant, "next_intent":"continue", "idempotency_key":"checkpoint"
                    })
                }
            }
        };
        let before = fixture.snapshot();
        let mut bytes = serde_json::to_vec(&request).unwrap();
        bytes.push(b'\n');
        let reader = DelayedFrame {
            input: Cursor::new(bytes),
            first_byte_seen: false,
            delayed: false,
        };
        let mut output = Vec::new();
        let started = now();
        fixture.server.serve(reader, &mut output).unwrap();
        assert_eq!(now().duration_since(started), Duration::from_millis(15_001));
        let refusal: Value = serde_json::from_slice(&output).unwrap();
        assert_eq!(
            refusal["error"]["code"], "control_request_expired",
            "{refusal}"
        );
        let message = refusal["error"]["message"].as_str().unwrap();
        assert!(message.contains("this attempt committed no changes"));
        assert!(message.contains("Earlier uncertain attempts remain unresolved"));
        assert_eq!(fixture.snapshot(), before);
        let first = fixture.success(&request);
        let applied = fixture.snapshot();
        assert_ne!(applied, before);
        assert_eq!(fixture.success(&request), first);
        assert_eq!(fixture.snapshot(), applied);
    }
}

/// Serves one frame whose input stalls past the allowance after its first byte.
fn serve_expired(fixture: &mut Fixture, request: &Value) -> Value {
    let mut bytes = serde_json::to_vec(request).unwrap();
    bytes.push(b'\n');
    let reader = DelayedFrame {
        input: Cursor::new(bytes),
        first_byte_seen: false,
        delayed: false,
    };
    let mut output = Vec::new();
    let started = now();
    fixture.server.serve(reader, &mut output).unwrap();
    assert_eq!(now().duration_since(started), Duration::from_millis(15_001));
    serde_json::from_slice(&output).unwrap()
}

/// Lifecycle and read operations take the same frame allowance as turn
/// operations. An expired frame gets the typed expiry refusal and leaves the
/// store unchanged; in time, the same frame reaches its operation.
///
/// `named_root_bind` and `turn_grant_read` show the refusal comes before
/// dispatch: the bind frame fails validation that runs before any
/// transaction, and the grant read takes no allowance of its own, so only
/// the frame check can answer them with an expiry. `session_status` runs
/// nothing before its transaction, so for it the test shows the refusal
/// comes before that transaction opens and applies nothing.
#[test]
fn expired_lifecycle_and_read_frames_are_refused_before_dispatch() {
    for operation in ["named_root_bind", "session_status", "turn_grant_read"] {
        CLOCK.with(|clock| clock.set(Instant::now()));
        let _clock = ControlRequestScope::with_clock(now);
        let mut fixture = Fixture::new();
        let routing = fixture.bind();
        let request = match operation {
            "named_root_bind" => serde_json::json!({
                "operation": "named_root_bind", "routing_token": routing,
                "claim_id": uuid::Uuid::new_v4(), "claim_fence": 1,
                "workspace_id": "expiry-workspace", "generation": 0,
                "named_at": "2026-01-01T00:00:00Z", "kind": "bound",
                "idempotency_key": "name"
            }),
            "session_status" => {
                serde_json::json!({"operation": "session_status", "routing_token": routing})
            }
            _ => {
                let grant = fixture.issue(&routing);
                serde_json::json!({
                    "operation": "turn_grant_read", "routing_token": routing, "grant_id": grant
                })
            }
        };
        let before = fixture.snapshot();
        let refusal = serve_expired(&mut fixture, &request);
        assert_eq!(
            refusal["error"]["code"], "control_request_expired",
            "{operation}: {refusal}"
        );
        let message = refusal["error"]["message"].as_str().unwrap();
        assert!(message.contains("this attempt committed no changes"));
        assert!(message.contains("Earlier uncertain attempts remain unresolved"));
        assert_eq!(fixture.snapshot(), before, "{operation}");

        // In time, the same frame reaches the operation and gets its own answer.
        let reply = fixture.wire(&request);
        match operation {
            "named_root_bind" => {
                assert_eq!(
                    reply["error"]["code"], "named_root_binding_refused",
                    "{reply}"
                );
                let message = reply["error"]["message"].as_str().unwrap();
                assert!(
                    message.contains("positive generation and fence"),
                    "{message}"
                );
            }
            "session_status" => {
                assert_eq!(reply["status"], "ok", "{reply}");
                assert_eq!(reply["result"]["phase"], "ready", "{reply}");
                assert!(reply["result"]["open_grant_id"].is_null(), "{reply}");
            }
            _ => {
                assert_eq!(reply["status"], "ok", "{reply}");
                assert_eq!(reply["result"]["status"], "found", "{reply}");
                assert_eq!(reply["result"]["state"], "issued", "{reply}");
            }
        }
        assert_eq!(fixture.snapshot(), before, "{operation}");
    }
}

#[test]
fn expired_status_frame_preserves_due_grant_until_in_time_retry() {
    CLOCK.with(|clock| clock.set(Instant::now()));
    let _clock = ControlRequestScope::with_clock(now);
    let mut fixture = Fixture::new();

    // Seed through the store's UTC clock; only the frame clock is delayed.
    let issued_at = Utc::now() - chrono::TimeDelta::days(1);
    let actor = fixture
        .server
        .actor("session_bind", "bind an expiry fixture");
    let bound = fixture
        .server
        .store
        .bind_control_session(
            &fixture.server.project_id,
            "busy-anchor",
            "busy",
            &fixture.server.session_id,
            &fixture.server.connection_token,
            &actor,
            ControlAssurance::TurnGated,
            &[EffectClass::Observe],
            1,
            "bind",
            issued_at,
        )
        .unwrap();
    let intent = TurnIntent {
        idempotency_key: "expired".into(),
        intent_fingerprint: ObjectId::from_canonical_bytes(b"expired"),
        purpose: None,
        requested_effects: vec![EffectClass::Observe],
        resource_intents: vec![],
    };
    let decision = fixture
        .server
        .store
        .evaluate_control_turn(
            &fixture.server.project_id,
            &fixture.server.session_id,
            &fixture.server.connection_token,
            &bound.routing_token,
            &intent,
            issued_at,
        )
        .unwrap();
    let crate::domain::ControlTurnDecision::Grant { grant } = decision else {
        panic!("expected an issued historical grant");
    };
    assert!(grant.basis.expires_at < Utc::now());

    let read_grant = serde_json::json!({
        "operation": "turn_grant_read", "routing_token": bound.routing_token,
        "grant_id": grant.grant_id
    });
    let issued = fixture.success(&read_grant);
    assert_eq!(issued["status"], "found");
    assert_eq!(issued["state"], "issued");
    assert!(issued["begun_at"].is_null());
    assert!(issued["completed_at"].is_null());

    let request = status_frame(&bound.routing_token);
    let before = fixture.snapshot();
    let refusal = serve_expired(&mut fixture, &request);
    assert_eq!(refusal["status"], "error", "{refusal}");
    assert_eq!(
        refusal["error"]["code"], "control_request_expired",
        "{refusal}"
    );
    let message = refusal["error"]["message"].as_str().unwrap();
    assert!(message.contains("this attempt committed no changes"));
    assert!(message.contains("Earlier uncertain attempts remain unresolved"));
    assert_eq!(fixture.snapshot(), before);
    assert_eq!(fixture.success(&read_grant), issued);
    assert_eq!(fixture.snapshot(), before);

    let status = fixture.success(&request);
    assert_eq!(status["phase"], "ready", "{status}");
    assert!(status["open_grant_id"].is_null(), "{status}");
    assert!(status["open_grant_state"].is_null(), "{status}");
    let applied = fixture.snapshot();
    assert_ne!(applied, before);

    let expired = fixture.success(&read_grant);
    assert_eq!(expired["status"], "found");
    assert_eq!(expired["state"], "expired");
    assert!(expired["begun_at"].is_null());
    assert!(expired["completed_at"].is_null());
    assert_eq!(fixture.snapshot(), applied);

    assert_eq!(fixture.success(&request), status);
    assert_eq!(fixture.snapshot(), applied);
}

#[derive(Default)]
struct LostFlush {
    bytes: Vec<u8>,
}

impl Write for LostFlush {
    fn write(&mut self, bytes: &[u8]) -> io::Result<usize> {
        self.bytes.extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> io::Result<()> {
        Err(io::ErrorKind::BrokenPipe.into())
    }
}

#[test]
fn late_commit_with_lost_response_is_settled_by_exact_key_replay() {
    CLOCK.with(|clock| clock.set(Instant::now()));
    let _clock = ControlRequestScope::with_clock(now);
    let mut fixture = Fixture::new();
    let routing = fixture.bind();
    let grant = fixture.issue(&routing);
    let request = begin_frame(&routing, &grant);
    fixture.server.store.test_control_commit_clock(slow_phase);
    let mut input = serde_json::to_vec(&request).unwrap();
    input.push(b'\n');
    let mut lost = LostFlush::default();
    let error = fixture
        .server
        .serve(Cursor::new(input), &mut lost)
        .unwrap_err();
    assert_eq!(error.kind(), io::ErrorKind::BrokenPipe);
    let reply: Value = serde_json::from_slice(&lost.bytes).unwrap();
    assert_eq!(reply["status"], "ok", "{reply}");
    let settled = fixture.snapshot();
    assert_eq!(fixture.success(&request), reply["result"]);
    assert_eq!(fixture.snapshot(), settled);
}
