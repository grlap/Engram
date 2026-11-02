use std::{path::PathBuf, time::Duration};

use rusqlite::Connection;

use super::*;

mod request_budget;

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
            .resume_work_connection(&session_id, Utc::now())
            .unwrap();
        let server = HostControlServer {
            store,
            diagnostic_store: crate::diagnostics::Fingerprint::store(&database),
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

    fn wire(&mut self, request: &Value) -> Value {
        let mut input = serde_json::to_vec(request).unwrap();
        input.push(b'\n');
        let mut output = Vec::new();
        self.server.serve(Cursor::new(input), &mut output).unwrap();
        serde_json::from_slice(&output).unwrap()
    }
}

/// Operations outside the certified busy set keep the generic storage error:
/// a busy BEGIN on the lifecycle `named_root_bind` makes no no-change claim
/// and applies nothing.
#[test]
fn lifecycle_begin_busy_keeps_the_generic_storage_error() {
    let mut fixture = Fixture::new();
    let name = serde_json::json!({
        "operation": "named_root_bind",
        "claim_id": uuid::Uuid::new_v4(), "claim_fence": 1,
        "workspace_id": "busy-workspace", "generation": 1,
        "named_at": "2026-01-01T00:00:00Z", "kind": "bound",
        "idempotency_key": "name"
    });
    let before = fixture.snapshot();
    assert!(fixture.server.store.test_is_autocommit());
    fixture.observer.execute_batch("BEGIN IMMEDIATE").unwrap();
    let reply = fixture.wire(&name);
    assert!(fixture.server.store.test_is_autocommit());
    fixture.observer.execute_batch("ROLLBACK").unwrap();
    assert_eq!(fixture.snapshot(), before);
    assert_eq!(reply["status"], "error", "{reply}");
    assert_eq!(reply["error"]["code"], "storage_error", "{reply}");
    let message = reply["error"]["message"].as_str().unwrap();
    // The generic error is the contended BEGIN itself, not some other failure.
    assert!(message.contains("database is locked"), "{message}");
    assert!(
        !message.contains("this attempt committed no changes"),
        "{message}"
    );
    assert_eq!(
        fixture.wire(&name)["error"]["code"],
        "named_root_binding_refused"
    );
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
        rusqlite::ffi::SQLITE_CORRUPT,
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
