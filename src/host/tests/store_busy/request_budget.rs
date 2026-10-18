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
