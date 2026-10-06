use chrono::{TimeDelta, TimeZone};
use rusqlite::params;
use serde_json::{Value as Json, json};

use super::*;
use crate::storage::{test_database_shape_snapshot, test_support::*};
use crate::{ControlAssurance, EffectClass};

fn fixture() -> (SqliteStore, TestControlBinding, String) {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let (binding, grant) = seed(&mut store);
    (store, binding, grant)
}

fn seed(store: &mut SqliteStore) -> (TestControlBinding, String) {
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let binding = bind_control(store, now);
    let grant = complete_control_turn(
        store,
        &binding,
        "read-fixture",
        vec![EffectClass::Observe],
        vec![],
        now + TimeDelta::seconds(1),
    );
    (binding, grant.grant_id)
}

fn read_unchanged(
    store: &SqliteStore,
    binding: &TestControlBinding,
    project: &str,
    connection: &str,
    routing: &str,
    grant: &str,
) -> Result<Json, StoreError> {
    let before = test_database_shape_snapshot(&store.connection).unwrap();
    let changes = store.connection.total_changes();
    let result = store.read_control_turn_grant(
        &ProjectId(project.into()),
        &binding.status.session_id,
        connection,
        routing,
        grant,
    );
    assert_eq!(
        store.connection.total_changes(),
        changes,
        "even rolled-back writes are forbidden"
    );
    assert_eq!(
        test_database_shape_snapshot(&store.connection).unwrap(),
        before
    );
    assert!(store.connection.is_autocommit());
    result.map(|r| serde_json::to_value(r).unwrap())
}

fn own_read(
    store: &SqliteStore,
    binding: &TestControlBinding,
    grant: &str,
) -> Result<Json, StoreError> {
    read_unchanged(
        store,
        binding,
        "project-a",
        &binding.connection_token,
        &binding.routing_token,
        grant,
    )
}

fn set_facts(
    store: &SqliteStore,
    grant: &str,
    state: &str,
    begun: Option<i64>,
    completed: Option<i64>,
) {
    store.connection.execute("UPDATE control_turn_grants SET state=?2, begun_at_ms=?3, completed_at_ms=?4 WHERE grant_id=?1", params![grant, state, begun, completed]).unwrap();
}

#[test]
fn grant_read_states_preserve_exact_evidence_without_writes_or_expiry() {
    let (mut store, binding, grant) = fixture();
    for (state, begun, completed) in [
        ("issued", None, None),
        ("expired", None, None),
        ("superseded", None, None),
        ("begun", Some(-1), None),
        ("completed", Some(0), Some(-1)),
    ] {
        set_facts(&store, &grant, state, begun, completed);
        let result = own_read(&store, &binding, &grant).unwrap();
        let date =
            |ms: Option<i64>| ms.map(|ms| DateTime::<Utc>::from_timestamp_millis(ms).unwrap());
        assert_eq!(
            result,
            json!({"control_schema_version": CONTROL_SCHEMA_VERSION, "session_id":binding.status.session_id, "grant_id":grant, "status":"found", "state":state, "begun_at":date(begun), "completed_at":date(completed)})
        );
    }
    assert_eq!(
        own_read(&store, &binding, "unknown").unwrap(),
        json!({"control_schema_version":CONTROL_SCHEMA_VERSION,"session_id":binding.status.session_id,"grant_id":"unknown","status":"not_found"})
    );
    // The old status operation still lazily expires an overdue issued row.
    set_facts(&store, &grant, "issued", None, None);
    assert_eq!(
        own_read(&store, &binding, &grant).unwrap()["state"],
        "issued"
    );
    store
        .control_status(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            Utc::now(),
        )
        .unwrap();
    assert_eq!(
        own_read(&store, &binding, &grant).unwrap()["state"],
        "expired"
    );
}

#[test]
fn grant_read_refuses_every_incoherent_shape_and_bad_timestamp() {
    let (store, binding, grant) = fixture();
    for state in ["issued", "expired", "superseded", "begun", "completed"] {
        for begun in [None, Some(0)] {
            for completed in [None, Some(-1)] {
                let valid = match state {
                    "begun" => begun.is_some() && completed.is_none(),
                    "completed" => begun.is_some() && completed.is_some(),
                    _ => begun.is_none() && completed.is_none(),
                };
                set_facts(&store, &grant, state, begun, completed);
                let result = own_read(&store, &binding, &grant);
                if valid {
                    assert!(result.is_ok());
                } else {
                    assert!(
                        matches!(result, Err(StoreError::InvalidControlProjection(_))),
                        "{state}/{begun:?}/{completed:?}"
                    );
                }
            }
        }
    }
    for (state, begun, completed) in [
        ("unknown", None, None),
        ("begun", Some(i64::MAX), None),
        ("completed", Some(0), Some(i64::MIN)),
    ] {
        set_facts(&store, &grant, state, begun, completed);
        assert!(matches!(
            own_read(&store, &binding, &grant),
            Err(StoreError::InvalidControlProjection(_))
        ));
    }
}

#[test]
fn grant_read_checks_credentials_before_absence_or_foreign_details() {
    let (mut store, binding, grant) = fixture();
    let foreign = bind_control_for(
        &mut store,
        "foreign-session",
        "foreign-bind",
        &[EffectClass::Observe],
        Utc::now(),
    );
    store.connection.execute("UPDATE control_turn_grants SET session_id=?2, state='malformed foreign state', begun_at_ms=?3 WHERE grant_id=?1",params![grant,foreign.status.session_id.0,i64::MAX]).unwrap();
    let error = own_read(&store, &binding, &grant).unwrap_err();
    assert!(matches!(error, StoreError::ControlTurnGrantSessionMismatch));
    assert_eq!(error.to_string(), "turn grant belongs to another session");
    for id in [grant.as_str(), "missing"] {
        for token in ["", "wrong"] {
            assert!(matches!(
                read_unchanged(
                    &store,
                    &binding,
                    "project-a",
                    token,
                    &binding.routing_token,
                    id
                ),
                Err(StoreError::ControlConnectionSuperseded(_))
            ));
            assert!(matches!(
                read_unchanged(
                    &store,
                    &binding,
                    "project-a",
                    &binding.connection_token,
                    token,
                    id
                ),
                Err(StoreError::ControlSessionTokenMismatch(_))
            ));
        }
        assert!(matches!(
            read_unchanged(
                &store,
                &binding,
                "wrong-project",
                &binding.connection_token,
                &binding.routing_token,
                id
            ),
            Err(StoreError::ControlSessionNotBound(_))
        ));
    }
    for id in ["", " \t\r\n"] {
        assert!(matches!(
            own_read(&store, &binding, id),
            Err(StoreError::InvalidTurnGrantId)
        ));
    }
    assert_eq!(
        own_read(&store, &binding, " missing ").unwrap()["grant_id"],
        " missing "
    );
    let old = binding.connection_token.clone();
    store
        .resume_control_connection(&binding.status.session_id, Utc::now())
        .unwrap();
    assert!(matches!(
        read_unchanged(
            &store,
            &binding,
            "project-a",
            &old,
            &binding.routing_token,
            "missing"
        ),
        Err(StoreError::ControlConnectionSuperseded(_))
    ));
}

#[test]
fn grant_read_keeps_session_history_after_exit_newer_turn_and_rebind() {
    let (mut store, binding, grant) = fixture();
    let later = complete_control_turn(
        &mut store,
        &binding,
        "newer",
        vec![EffectClass::Observe],
        vec![],
        Utc::now(),
    );
    let before = own_read(&store, &binding, &grant).unwrap();
    // Its unsupported payload is deliberately not part of the diagnostic read.
    let mut payload: Json = store
        .connection
        .query_row(
            "SELECT grant_json FROM control_turn_grants WHERE grant_id=?1",
            [&grant],
            |r| r.get::<_, Vec<u8>>(0),
        )
        .map(|bytes| serde_json::from_slice(&bytes).unwrap())
        .unwrap();
    payload["control_schema_version"] = json!(999);
    payload["task_id"] = json!("stale-task");
    store
        .connection
        .execute(
            "UPDATE control_turn_grants SET grant_json=?2 WHERE grant_id=?1",
            params![grant, serde_json::to_vec(&payload).unwrap()],
        )
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE control_sessions SET phase='exited' WHERE session_id=?1",
            [&binding.status.session_id.0],
        )
        .unwrap();
    assert_eq!(own_read(&store, &binding, &grant).unwrap(), before);
    let rebound = store
        .bind_control_session(
            &ProjectId("project-b".into()),
            "rebound-anchor",
            "rebound",
            &binding.status.session_id,
            &binding.connection_token,
            &actor("control-session"),
            ControlAssurance::TurnGated,
            &[EffectClass::Observe],
            2,
            "rebind",
            Utc::now(),
        )
        .unwrap();
    assert_eq!(
        read_unchanged(
            &store,
            &binding,
            "project-b",
            &binding.connection_token,
            &rebound.routing_token,
            &grant
        )
        .unwrap(),
        before
    );
    assert_eq!(
        read_unchanged(
            &store,
            &binding,
            "project-b",
            &binding.connection_token,
            &rebound.routing_token,
            &later.grant_id
        )
        .unwrap()["state"],
        "completed"
    );
}

#[test]
fn grant_read_uses_one_snapshot_across_credentials_and_grant_lookup() {
    let home = crate::test_support::temp_home().unwrap();
    let path = home.path().join("snapshot.db");
    let mut store = SqliteStore::open(&path).unwrap();
    let (binding, grant) = seed(&mut store);
    let writer = SqliteStore::open(&path).unwrap();
    let worker_grant = grant.clone();
    let session = binding.status.session_id.clone();
    let changes = store.connection.total_changes();
    let result = crate::storage::concurrent_commit::read_across_a_concurrent_commit(
        &store,
        |reader| {
            reader.read_control_turn_grant(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &grant,
            )
        },
        // The connection credential SELECT has already established the snapshot.
        &["FROM control_sessions WHERE session_id = ?1"],
        move || {
            let tx = writer
                .connection
                .unchecked_transaction()
                .map_err(|e| e.to_string())?;
            tx.execute(
                "UPDATE control_connections SET connection_token='replacement' WHERE session_id=?1",
                [&session.0],
            )
            .map_err(|e| e.to_string())?;
            tx.execute(
                "UPDATE control_sessions SET routing_token='replacement' WHERE session_id=?1",
                [&session.0],
            )
            .map_err(|e| e.to_string())?;
            tx.execute(
            "UPDATE control_turn_grants SET state='begun', completed_at_ms=NULL WHERE grant_id=?1",
            [&worker_grant],
        )
        .map_err(|e| e.to_string())?;
            tx.commit().map_err(|e| e.to_string())
        },
    )
    .unwrap();
    assert_eq!(serde_json::to_value(result).unwrap()["state"], "completed");
    assert_eq!(store.connection.total_changes(), changes);
    assert!(matches!(
        own_read(&store, &binding, &grant),
        Err(StoreError::ControlConnectionSuperseded(_))
    ));
}
