use std::{cell::RefCell, io::Cursor, sync::mpsc, time::Duration};

use super::*;

thread_local! {
    static CONTENDED: RefCell<Option<StartupContention>> = const { RefCell::new(None) };
}

struct StartupContention {
    entered: mpsc::SyncSender<()>,
    released: mpsc::Receiver<()>,
}

fn signal_contention(_attempt: i32) -> bool {
    CONTENDED.with(|signal| {
        if let Some(signal) = signal.borrow_mut().take() {
            return signal.entered.send(()).is_ok()
                && signal
                    .released
                    .recv_timeout(Duration::from_secs(10))
                    .is_ok();
        }
        false
    })
}

#[test]
fn startup_resume_waits_for_a_contended_writer_then_succeeds() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("engram.db");
    let mut store = SqliteStore::open(&database).unwrap();
    let timeout: i64 = store
        .connection
        .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
        .unwrap();
    assert_eq!(timeout, 5000, "production wait remains bounded");
    let blocker = rusqlite::Connection::open(&database).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let (send, receive) = mpsc::sync_channel(0);
    let (release, released) = mpsc::sync_channel(0);
    let worker = std::thread::spawn(move || {
        CONTENDED.with(|signal| {
            *signal.borrow_mut() = Some(StartupContention {
                entered: send,
                released,
            });
        });
        store
            .connection
            .busy_handler(Some(signal_contention))
            .unwrap();
        let token = store
            .resume_control_connection(&SessionId("wait-session".into()), Utc::now())
            .unwrap();
        drop(store);
        token
    });
    receive
        .recv_timeout(Duration::from_secs(5))
        .expect("resume actually attempted a locked writer");
    blocker.execute_batch("ROLLBACK").unwrap();
    release
        .send(())
        .expect("release confirmed contention after rollback");
    assert_ne!(worker.join().unwrap(), "");
    drop(blocker);
}

#[test]
fn startup_resume_busy_preserves_existing_grants_and_does_not_dispatch_first_frame() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("engram.db");
    let mut store = SqliteStore::open(&database).unwrap();
    let now = Utc::now();
    let mut sessions = Vec::new();
    for (session, begun) in [("issued-startup", false), ("begun-startup", true)] {
        let binding = bind_control_for(
            &mut store,
            session,
            &format!("bind-{session}"),
            &[EffectClass::Observe],
            now,
        );
        complete_control_turn(
            &mut store,
            &binding,
            &format!("sync-{session}"),
            vec![EffectClass::Observe],
            Vec::new(),
            now,
        );
        let ControlTurnDecision::Grant { grant } = store
            .evaluate_control_turn(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &TurnIntent {
                    idempotency_key: format!("issue-{session}"),
                    intent_fingerprint: ObjectId::from_canonical_bytes(session.as_bytes()),
                    purpose: Some(TurnPurpose::Ordinary),
                    requested_effects: vec![EffectClass::Observe],
                    resource_intents: Vec::new(),
                },
                now + TimeDelta::seconds(1),
            )
            .unwrap()
        else {
            panic!("grant");
        };
        if begun {
            assert!(matches!(
                store
                    .begin_control_turn(
                        &ProjectId("project-a".into()),
                        &binding.status.session_id,
                        &binding.connection_token,
                        &binding.routing_token,
                        &grant.grant_id,
                        &[],
                        &format!("begin-{session}"),
                        now + TimeDelta::seconds(2),
                    )
                    .unwrap(),
                ControlTurnBeginDecision::Begin { .. }
            ));
        }
        sessions.push(binding.status.session_id.clone());
    }
    let before = crate::storage::test_database_shape_snapshot(&store.connection).unwrap();
    let blocker = rusqlite::Connection::open(&database).unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    store
        .connection
        .busy_timeout(Duration::from_millis(20))
        .unwrap();
    let request = serde_json::to_vec(&serde_json::json!({
        "operation": "session_bind", "external_ref": "dummy:STARTUP",
        "title": "Must not be created", "assurance": "turn_gated", "mediated_effects": ["observe"],
        "capability_map_revision": 1, "idempotency_key": "must-not-dispatch"
    }))
    .unwrap();
    // This is a valid mutating request; the failed startup responder never dispatches it.
    serde_json::from_slice::<crate::host::HostControlRequest>(&request).unwrap();
    for session in sessions {
        let error = store
            .resume_control_connection(&session, now + TimeDelta::seconds(3))
            .unwrap_err();
        assert!(
            matches!(&error, StoreError::Sqlite(error) if error.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy))
        );
        let mut output = Vec::new();
        crate::HostControlServer::serve_startup_failure(&error, Cursor::new(&request), &mut output)
            .unwrap();
        assert_eq!(
            serde_json::from_slice::<serde_json::Value>(&output).unwrap()["error"]["code"],
            "control_startup_busy"
        );
    }
    blocker.execute_batch("ROLLBACK").unwrap();
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&store.connection).unwrap(),
        before
    );
    drop(blocker);
    drop(store);
}
