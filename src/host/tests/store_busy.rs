use std::{path::PathBuf, time::Duration};

use rusqlite::Connection;

use super::*;

struct Fixture {
    server: HostControlServer,
    observer: Connection,
    _directory: crate::test_support::TempHome,
}

impl Fixture {
    fn new() -> Self {
        let directory = crate::test_support::temp_home().unwrap();
        let database: PathBuf = directory.path().join("control.db");
        let mut store = SqliteStore::open(&database).unwrap();
        store.test_set_busy_timeout(Duration::ZERO).unwrap();
        let session_id = SessionId("busy-host".into());
        let connection_token = store
            .resume_control_connection(&session_id, Utc::now())
            .unwrap();
        let server = HostControlServer {
            store,
            project_id: ProjectId("busy-project".into()),
            actor_id: "busy-host".into(),
            session_id,
            connection_token,
            source_skill: None,
            actor_context: None,
            actor_context_normalized: false,
            diagnostics: None,
            diagnostic_frame: None,
            diagnostic_sequence: 0,
        };
        let observer = Connection::open(&database).unwrap();
        Self {
            server,
            observer,
            _directory: directory,
        }
    }

    fn snapshot(&self) -> crate::storage::TestDatabaseShapeSnapshot {
        crate::storage::test_database_shape_snapshot(&self.observer).unwrap()
    }

    fn bind(&mut self) -> String {
        self.success(&bind_frame("bind"))["routing_token"]
            .as_str()
            .unwrap()
            .into()
    }

    fn issue(&mut self, routing: &str) -> String {
        self.success(&evaluate_frame(routing, "issued"))["grant"]["grant_id"]
            .as_str()
            .unwrap()
            .into()
    }

    fn wire(&mut self, request: &Value) -> Value {
        let mut input = serde_json::to_vec(request).unwrap();
        input.push(b'\n');
        let mut output = Vec::new();
        self.server.serve(Cursor::new(input), &mut output).unwrap();
        serde_json::from_slice(&output).unwrap()
    }

    fn success(&mut self, request: &Value) -> Value {
        let reply = self.wire(request);
        assert_eq!(reply["status"], "ok", "{reply}");
        reply["result"].clone()
    }

    fn blocked_attempt(&mut self, request: &Value) {
        let before = self.snapshot();
        self.observer.execute_batch("BEGIN IMMEDIATE").unwrap();
        let reply = self.wire(request);
        self.observer.execute_batch("ROLLBACK").unwrap();
        assert_eq!(self.snapshot(), before);
        assert_eq!(reply["status"], "error", "{reply}");
        assert_eq!(reply["error"]["code"], "control_store_busy", "{reply}");
        let message = reply["error"]["message"].as_str().unwrap();
        assert!(message.contains("this attempt committed no changes"));
        assert!(message.contains("the same idempotency key"));
        assert!(message.contains("the same control connection"));
        assert!(message.contains("prior replay or reconciliation uncertainty"));
    }

    fn blocked_then_exact_retry(&mut self, request: &Value) {
        self.blocked_attempt(request);
        let before = self.snapshot();
        let first = self.success(request);
        let applied = self.snapshot();
        assert_ne!(applied, before);
        assert_eq!(self.success(request), first);
        assert_eq!(self.snapshot(), applied);
    }
}

fn bind_frame(key: &str) -> Value {
    serde_json::json!({
        "operation": "session_bind", "external_ref": "busy-anchor", "title": "busy",
        "assurance": "turn_gated", "mediated_effects": ["observe"],
        "capability_map_revision": 1, "idempotency_key": key
    })
}

fn evaluate_frame(routing: &str, key: &str) -> Value {
    serde_json::json!({
        "operation": "turn_evaluate", "routing_token": routing, "idempotency_key": key,
        "intent_fingerprint": ObjectId::from_canonical_bytes(key.as_bytes()).as_str(),
        "requested_effects": ["observe"], "resource_intents": []
    })
}

fn begin_frame(routing: &str, grant: &str) -> Value {
    serde_json::json!({
        "operation": "turn_begin", "routing_token": routing,
        "grant_id": grant, "idempotency_key": "begin"
    })
}

#[test]
fn control_store_busy_session_bind_preserves_unbound_and_existing_grants() {
    let mut fixture = Fixture::new();
    fixture.blocked_then_exact_retry(&bind_frame("bind"));
    let routing = fixture.success(&bind_frame("bind"))["routing_token"]
        .as_str()
        .unwrap()
        .to_owned();
    fixture.issue(&routing);
    fixture.blocked_then_exact_retry(&bind_frame("rebind"));
}

#[test]
fn control_store_busy_evaluate_preserves_the_issued_grant() {
    let mut fixture = Fixture::new();
    let routing = fixture.bind();
    fixture.issue(&routing);
    fixture.blocked_then_exact_retry(&evaluate_frame(&routing, "replacement"));
}

#[test]
fn control_store_busy_begin_preserves_the_exact_grant() {
    let mut fixture = Fixture::new();
    let routing = fixture.bind();
    let grant = fixture.issue(&routing);
    fixture.blocked_then_exact_retry(&begin_frame(&routing, &grant));
}

#[test]
fn control_store_busy_checkpoint_preserves_the_begun_turn() {
    let mut fixture = Fixture::new();
    let routing = fixture.bind();
    let grant = fixture.issue(&routing);
    fixture.success(&begin_frame(&routing, &grant));
    let request = serde_json::json!({
        "operation": "turn_checkpoint", "routing_token": routing, "grant_id": grant,
        "next_intent": "continue", "idempotency_key": "checkpoint"
    });
    fixture.blocked_then_exact_retry(&request);
}

#[test]
fn control_store_busy_replay_does_not_disprove_an_earlier_begin() {
    let mut fixture = Fixture::new();
    let routing = fixture.bind();
    let grant = fixture.issue(&routing);
    let request = begin_frame(&routing, &grant);
    let begun = fixture.success(&request);
    let before = fixture.snapshot();
    fixture.blocked_attempt(&request);
    assert_eq!(fixture.success(&request), begun);
    assert_eq!(fixture.snapshot(), before);
}

#[test]
fn control_store_busy_does_not_reclassify_other_sqlite_errors() {
    for code in [
        rusqlite::ffi::SQLITE_BUSY,
        rusqlite::ffi::SQLITE_BUSY_RECOVERY,
        rusqlite::ffi::SQLITE_LOCKED,
        rusqlite::ffi::SQLITE_LOCKED_SHAREDCACHE,
        rusqlite::ffi::SQLITE_IOERR,
        rusqlite::ffi::SQLITE_CONSTRAINT,
        rusqlite::ffi::SQLITE_READONLY,
    ] {
        let error = StoreError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(code),
            None,
        ));
        assert_eq!(store_error_code(&error), "storage_error");
    }
}

#[test]
fn control_store_busy_checkpoint_nonbusy_failure_rolls_back_without_transient_code() {
    let mut fixture = Fixture::new();
    let routing = fixture.bind();
    let grant = fixture.issue(&routing);
    fixture.success(&begin_frame(&routing, &grant));
    fixture
        .observer
        .execute_batch(
            "CREATE TRIGGER reject_checkpoint_result
         BEFORE INSERT ON control_operation_results
         WHEN NEW.operation = 'turn_checkpoint'
         BEGIN SELECT RAISE(ABORT, 'checkpoint fixture failure'); END;",
        )
        .unwrap();
    let before = fixture.snapshot();
    let request = serde_json::json!({
        "operation": "turn_checkpoint", "routing_token": routing, "grant_id": grant,
        "next_intent": "continue", "idempotency_key": "checkpoint"
    });
    let reply = fixture.wire(&request);
    assert_eq!(reply["status"], "error");
    assert_eq!(reply["error"]["code"], "storage_error");
    assert!(
        reply["error"]["message"]
            .as_str()
            .unwrap()
            .contains("checkpoint fixture failure")
    );
    assert_eq!(fixture.snapshot(), before);
    fixture
        .observer
        .execute_batch("DROP TRIGGER reject_checkpoint_result")
        .unwrap();
    let completed = fixture.success(&request);
    let after = fixture.snapshot();
    assert_eq!(fixture.success(&request), completed);
    assert_eq!(fixture.snapshot(), after);
}
