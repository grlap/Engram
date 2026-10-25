use super::*;
use chrono::{TimeDelta, TimeZone};
use serde_json::{Value as Json, json};

use crate::storage::{SqliteStore, test_support::*};
use crate::{
    ControlTurnBeginDecision, ControlTurnCheckpointDecision, ControlTurnDecision, EffectClass,
    HostPathPolicy, IssuedTurnGrant, ObjectId, TurnIntent, TurnNextIntent,
};

fn now() -> DateTime<Utc> {
    Utc.timestamp_millis_opt(1_700_000_000_000).unwrap()
}

fn project() -> ProjectId {
    ProjectId("project-a".into())
}

fn issue(store: &mut SqliteStore, binding: &TestControlBinding, key: &str) -> IssuedTurnGrant {
    let result = store
        .evaluate_control_turn(
            &project(),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &TurnIntent {
                idempotency_key: key.into(),
                intent_fingerprint: ObjectId::from_canonical_bytes(key.as_bytes()),
                purpose: None,
                requested_effects: vec![EffectClass::Observe],
                resource_intents: vec![],
            },
            now(),
        )
        .unwrap();
    let ControlTurnDecision::Grant { grant } = result else {
        panic!("fixture must grant");
    };
    *grant
}

fn view(store: &SqliteStore, binding: &TestControlBinding, grant: &str) -> Json {
    let changes = store.connection.total_changes();
    let before = crate::storage::test_database_shape_snapshot(&store.connection).unwrap();
    let details = read(
        &store.connection,
        &project(),
        &binding.status.session_id.0,
        grant,
    )
    .unwrap();
    assert_eq!(store.connection.total_changes(), changes);
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&store.connection).unwrap(),
        before
    );
    serde_json::to_value(details).unwrap()
}

#[test]
fn control_inspection_details_read_real_issued_begun_and_checkpointed_states() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let binding = bind_control(&mut store, now());
    let grant = issue(&mut store, &binding, "inspect-lifecycle");
    let issued = view(&store, &binding, &grant.grant_id);
    assert_eq!(issued["schema_version"], 1);
    assert_eq!(issued["session"]["phase"], "turn_open");
    assert_eq!(issued["session_begun_grant_present"], false);
    assert_eq!(issued["retained_grant"]["state"], "issued");
    assert_eq!(
        issued["retained_grant"]["issued_at"],
        json!(grant.issued_at)
    );
    assert_eq!(
        issued["retained_grant"]["expires_at"],
        json!(grant.basis.expires_at)
    );
    assert_eq!(
        issued["retained_grant"]["session_id"],
        binding.status.session_id.0
    );
    assert_eq!(issued["retained_grant"]["session_relation"], "target");
    assert_eq!(issued["retained_grant"]["matches_current_task"], true);
    assert!(issued["retained_grant"]["begun_at"].is_null());
    assert!(matches!(
        store
            .begin_control_turn(
                &project(),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &grant.grant_id,
                &[],
                "inspect-begin",
                now() + TimeDelta::milliseconds(1),
            )
            .unwrap(),
        ControlTurnBeginDecision::Begin { .. }
    ));
    let begun = view(&store, &binding, &grant.grant_id);
    assert_eq!(begun["session"]["phase"], "turn_open");
    assert_eq!(begun["session_begun_grant_present"], true);
    assert_eq!(begun["retained_grant"]["state"], "begun");
    assert_eq!(
        begun["retained_grant"]["begun_at"],
        json!(now() + TimeDelta::milliseconds(1))
    );
    assert!(matches!(
        store
            .checkpoint_control_turn(
                &project(),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &grant.grant_id,
                TurnNextIntent::Exit,
                "inspect-checkpoint",
                now() + TimeDelta::milliseconds(2),
            )
            .unwrap(),
        ControlTurnCheckpointDecision::Checkpointed { .. }
    ));
    let completed = view(&store, &binding, &grant.grant_id);
    assert_eq!(completed["session"]["phase"], "exited");
    assert_eq!(completed["session_begun_grant_present"], false);
    assert_eq!(completed["retained_grant"]["state"], "completed");
    assert_eq!(
        completed["retained_grant"]["completed_at"],
        json!(now() + TimeDelta::milliseconds(2))
    );
    assert_eq!(completed["retained_grant"]["timestamps_status"], "valid");
    let encoded = serde_json::to_string(&completed).unwrap();
    for private in [&binding.routing_token, &binding.connection_token] {
        assert!(!encoded.contains(private));
    }
    assert!(!encoded.contains("was_replayed"));
    assert!(!encoded.contains("refusal_code"));
}

#[test]
fn control_inspection_details_never_lazily_expire_an_overdue_issued_grant() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let binding = bind_control(&mut store, now());
    let first = issue(&mut store, &binding, "inspect-old");
    assert!(first.basis.expires_at < Utc::now());
    assert_eq!(
        view(&store, &binding, &first.grant_id)["retained_grant"]["state"],
        "issued"
    );
    let second = issue(&mut store, &binding, "inspect-replacement");
    assert_eq!(
        view(&store, &binding, &first.grant_id)["retained_grant"]["state"],
        "superseded"
    );
    store
        .control_status(
            &project(),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            now() + TimeDelta::minutes(1),
        )
        .unwrap();
    let expired = view(&store, &binding, &second.grant_id);
    assert_eq!(expired["retained_grant"]["state"], "expired");
    assert_eq!(expired["session_begun_grant_present"], false);
    assert!(expired["retained_grant"]["begun_at"].is_null());
}

#[test]
fn control_inspection_details_public_snapshot_keeps_database_and_wal_bytes() {
    let home = crate::test_support::temp_home().unwrap();
    let path = home.path().join("inspect.db");
    let mut store = SqliteStore::open(&path).unwrap();
    let binding = bind_control(&mut store, now());
    let grant = issue(&mut store, &binding, "inspect-file");
    let bytes = || {
        ["", "-wal"].map(
            |suffix| match std::fs::read(format!("{}{suffix}", path.display())) {
                Ok(bytes) => Some(bytes),
                Err(error) if error.kind() == std::io::ErrorKind::NotFound => None,
                Err(error) => panic!("fixture read: {error}"),
            },
        )
    };
    let before = bytes();
    let report = SqliteStore::inspect_control_session(
        &path,
        Some(HostPathPolicy::host_default()),
        &binding.status.session_id.0,
        &grant.grant_id,
        &project(),
    )
    .unwrap();
    assert!(
        report.session_present && report.session_grants_present && report.retained_grant_present
    );
    assert_eq!(
        serde_json::to_value(report.details).unwrap()["retained_grant"]["state"],
        "issued"
    );
    assert_eq!(bytes(), before);
}

#[test]
fn control_inspection_details_preserve_grant_project_after_session_rebind() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let binding = bind_control(&mut store, now());
    let grant = complete_control_turn(
        &mut store,
        &binding,
        "inspect-history",
        vec![EffectClass::Observe],
        vec![],
        now(),
    );
    let foreign = ProjectId("PRIVATE-other-project".into());
    store
        .bind_control_session(
            &foreign,
            "PRIVATE-other-anchor",
            "PRIVATE-title",
            &binding.status.session_id,
            &binding.connection_token,
            &actor(&binding.status.session_id.0),
            crate::ControlAssurance::TurnGated,
            &[EffectClass::Observe],
            1,
            "PRIVATE-rebind",
            now() + TimeDelta::seconds(1),
        )
        .unwrap();
    let own = view(&store, &binding, &grant.grant_id);
    assert_eq!(own["session"]["status"], "withheld");
    assert!(own["session"]["phase"].is_null());
    assert!(own["session_begun_grant_present"].is_null());
    assert_eq!(own["retained_grant"]["status"], "found");
    assert_eq!(own["retained_grant"]["project_match"], true);
    assert_eq!(own["retained_grant"]["matches_current_task"], false);
    let other = serde_json::to_value(
        read(
            &store.connection,
            &foreign,
            &binding.status.session_id.0,
            &grant.grant_id,
        )
        .unwrap(),
    )
    .unwrap();
    assert_eq!(other["session"]["status"], "found");
    assert_eq!(other["retained_grant"]["status"], "withheld");
    assert_eq!(other["retained_grant"]["project_match"], false);
    assert!(other["retained_grant"]["session_id"].is_null());
    assert!(other["retained_grant"]["state"].is_null());
    assert!(!serde_json::to_string(&other).unwrap().contains("PRIVATE"));
}

#[test]
fn control_inspection_details_do_not_echo_unknown_scalars_or_hide_presence() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let binding = bind_control(&mut store, now());
    let grant = issue(&mut store, &binding, "inspect-malformed");
    store
        .connection
        .execute("UPDATE control_sessions SET phase='PRIVATE-phase'", [])
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE control_turn_grants SET state='PRIVATE-state',grant_json=x'ff',issued_at_ms=?1",
            [i64::MAX],
        )
        .unwrap();
    let facts = view(&store, &binding, &grant.grant_id);
    assert_eq!(facts["session"]["status"], "found");
    assert!(facts["session"]["phase"].is_null());
    assert_eq!(facts["session"]["phase_status"], "unrecognized");
    assert!(facts["retained_grant"]["state"].is_null());
    assert_eq!(facts["retained_grant"]["state_status"], "unrecognized");
    assert_eq!(facts["retained_grant"]["timestamps_status"], "invalid");
    assert!(!serde_json::to_string(&facts).unwrap().contains("PRIVATE"));
    store
        .connection
        .execute(
            "UPDATE control_turn_grants SET state='begun',issued_at_ms=0,begun_at_ms=NULL",
            [],
        )
        .unwrap();
    assert_eq!(
        view(&store, &binding, &grant.grant_id)["retained_grant"]["timestamps_status"],
        "inconsistent"
    );
    store
        .connection
        .execute_batch("PRAGMA foreign_keys=OFF")
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE control_turn_grants SET task_id='orphan',session_id='PRIVATE-orphan'",
            [],
        )
        .unwrap();
    let report = SqliteStore::control_session_presence_on(
        &store.connection,
        &binding.status.session_id.0,
        &grant.grant_id,
        HostPathPolicy::host_default(),
        &project(),
    )
    .unwrap();
    assert!(report.session_present && report.retained_grant_present);
    let orphan = serde_json::to_value(report.details).unwrap();
    assert_eq!(orphan["retained_grant"]["status"], "withheld");
    assert!(orphan["retained_grant"]["project_match"].is_null());
    assert!(!serde_json::to_string(&orphan).unwrap().contains("PRIVATE"));
}

#[test]
fn control_inspection_details_keep_phase_separate_from_begun_row_presence() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let binding = bind_control(&mut store, now());
    let grant = issue(&mut store, &binding, "inspect-phase");
    for phase in ["ready", "sync_required", "turn_open", "exited"] {
        store
            .connection
            .execute("UPDATE control_sessions SET phase=?1", [phase])
            .unwrap();
        let facts = view(&store, &binding, &grant.grant_id);
        assert_eq!(facts["session"]["phase"], phase);
        assert_eq!(facts["session"]["phase_status"], "recognized");
        assert_eq!(facts["session_begun_grant_present"], false);
    }
    store
        .connection
        .execute(
            "UPDATE control_turn_grants SET state='begun',begun_at_ms=0",
            [],
        )
        .unwrap();
    let facts = view(&store, &binding, &grant.grant_id);
    assert_eq!(facts["session"]["phase"], "exited");
    assert_eq!(facts["session_begun_grant_present"], true);
    store
        .connection
        .execute_batch("PRAGMA foreign_keys=OFF")
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE control_turn_grants SET session_id='another-session'",
            [],
        )
        .unwrap();
    let facts = view(&store, &binding, &grant.grant_id);
    assert_eq!(facts["session_begun_grant_present"], false);
    assert_eq!(facts["retained_grant"]["status"], "found");
    assert_eq!(facts["retained_grant"]["session_relation"], "other");
    assert_eq!(facts["retained_grant"]["owner_session_present"], false);
    assert_eq!(facts["retained_grant"]["matches_current_task"], false);
}

#[test]
fn control_inspection_details_scan_budget_refuses_and_is_removed_after_error() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let binding = bind_control(&mut store, now());
    let grant = issue(&mut store, &binding, "inspect-budget");
    let changes = store.connection.total_changes();
    let result = read_with_budget(
        &store.connection,
        &project(),
        &binding.status.session_id.0,
        &grant.grant_id,
        1,
    );
    assert!(
        matches!(result, Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(error, _))) if error.code == rusqlite::ErrorCode::OperationInterrupted)
    );
    assert_eq!(store.connection.total_changes(), changes);
    assert_eq!(
        view(&store, &binding, &grant.grant_id)["retained_grant"]["state"],
        "issued"
    );
    store.connection.execute_batch("PRAGMA foreign_keys=OFF;
        WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<25000)
        INSERT INTO control_turn_grants(grant_id,session_id,task_id,request_key,grant_json,state,issued_at_ms,expires_at_ms)
        SELECT 'history-'||x,'control-session','orphan','history-'||x,x'ff','completed',0,1 FROM n;").unwrap();
    assert!(
        matches!(read(&store.connection, &project(), &binding.status.session_id.0, &grant.grant_id), Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(error, _))) if error.code == rusqlite::ErrorCode::OperationInterrupted)
    );
    let one: i64 = store
        .connection
        .query_row("SELECT 1", [], |row| row.get(0))
        .unwrap();
    assert_eq!(one, 1);
}
