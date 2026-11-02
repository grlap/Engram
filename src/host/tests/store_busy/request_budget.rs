use std::{
    cell::Cell,
    io::{self, BufRead, Read},
    time::{Duration, Instant},
};

use super::*;
use crate::storage::ControlRequestScope;

thread_local! { static CLOCK: Cell<Instant> = Cell::new(Instant::now()); }
fn now() -> Instant {
    CLOCK.with(Cell::get)
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

/// Lifecycle and read operations share the same allowance. Expiry precedes
/// request-specific validation and leaves no mutation, even before a bad name.
#[test]
fn expired_lifecycle_and_read_frames_are_refused_before_dispatch() {
    for request in [
        serde_json::json!({"operation":"named_root_bind", "claim_id":uuid::Uuid::new_v4(),
            "claim_fence":1, "workspace_id":"workspace", "generation":0,
            "named_at":"2026-01-01T00:00:00Z", "kind":"bound", "idempotency_key":"name"}),
        serde_json::json!({"operation":"work_session_read"}),
    ] {
        CLOCK.with(|clock| clock.set(Instant::now()));
        let _clock = ControlRequestScope::with_clock(now);
        let mut fixture = Fixture::new();
        let before = fixture.snapshot();
        let refusal = serve_expired(&mut fixture, &request);
        assert_eq!(
            refusal["error"]["code"], "control_request_expired",
            "{refusal}"
        );
        assert_eq!(fixture.snapshot(), before);
        let reply = fixture.wire(&request);
        if request["operation"] == "named_root_bind" {
            assert_eq!(
                reply["error"]["code"], "named_root_binding_refused",
                "{reply}"
            );
        } else {
            assert_eq!(reply["status"], "ok", "{reply}");
            assert_eq!(reply["result"]["work_session"], true);
        }
        assert_eq!(fixture.snapshot(), before);
    }
}
