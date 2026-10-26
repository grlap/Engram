use super::*;
use crate::storage::{SqliteStore, test_support::*};
use crate::{ControlTurnDecision, EffectClass, IssuedTurnGrant, ObjectId, TurnIntent};
use chrono::{TimeDelta, TimeZone};
use serde_json::{Value as Json, json};
use std::fmt::Write;

const KEY: &str = " PRIVATE-INSPECTION-KEY ";

fn now() -> DateTime<Utc> {
    Utc.timestamp_millis_opt(1_700_000_000_000).unwrap()
}

fn project() -> ProjectId {
    ProjectId("project-a".into())
}

fn fixture() -> (SqliteStore, TestControlBinding, IssuedTurnGrant) {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let binding = bind_control(&mut store, now());
    let result = store
        .evaluate_control_turn(
            &project(),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &TurnIntent {
                idempotency_key: "evaluate".into(),
                intent_fingerprint: ObjectId::from_canonical_bytes(b"fixture"),
                purpose: None,
                requested_effects: vec![EffectClass::Observe],
                resource_intents: vec![],
            },
            now(),
        )
        .unwrap();
    let ControlTurnDecision::Grant { grant } = result else {
        panic!("fixture grant")
    };
    (store, binding, *grant)
}

fn checkpoint(store: &mut SqliteStore, binding: &TestControlBinding, grant: &str) {
    store
        .checkpoint_control_turn(
            &project(),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            grant,
            TurnNextIntent::Exit,
            KEY,
            now() + TimeDelta::milliseconds(3),
        )
        .unwrap();
}

fn view(
    store: &SqliteStore,
    binding: &TestControlBinding,
    grant: &str,
    operation: &str,
    key: &str,
) -> Result<Json, StoreError> {
    let before = store.connection.total_changes();
    let result = read(
        &store.connection,
        &project(),
        &binding.status.session_id.0,
        grant,
        ControlOperationSelector::new(operation, key)?,
    );
    assert_eq!(store.connection.total_changes(), before);
    result.map(|value| serde_json::to_value(value).unwrap())
}

fn replace(store: &SqliteStore, intent: &[u8], result: &[u8], time: i64) {
    store
        .connection
        .execute(
            "UPDATE control_operation_results SET intent_json=?1,result_json=?2,created_at_ms=?3",
            params![intent, result, time],
        )
        .unwrap();
}

fn payloads(store: &SqliteStore) -> (Vec<u8>, Vec<u8>) {
    store
        .connection
        .query_row(
            "SELECT intent_json,result_json FROM control_operation_results",
            [],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap()
}

fn safe_error(result: Result<Json, StoreError>) -> String {
    let text = result.unwrap_err().to_string();
    assert!(!text.contains(KEY));
    assert!(!text.contains("PRIVATE-PAYLOAD"));
    text
}

#[test]
fn control_inspection_operation_real_outcomes_and_exact_tuple_isolation() {
    let (mut store, binding, grant) = fixture();
    // Same key for both operations: checkpoint refusal exists before begin.
    checkpoint(&mut store, &binding, &grant.grant_id);
    let refused = view(&store, &binding, &grant.grant_id, "turn_checkpoint", KEY).unwrap();
    assert_eq!(refused["status"], "found");
    assert_eq!(refused["decision"], "refuse");
    assert_eq!(refused["refusal_code"], "grant_not_begun");
    assert_eq!(
        refused["recorded_at"],
        json!(now() + TimeDelta::milliseconds(3))
    );
    assert_eq!(
        view(&store, &binding, &grant.grant_id, "turn_begin", KEY).unwrap()["status"],
        "not_found"
    );
    assert_eq!(
        view(
            &store,
            &binding,
            &grant.grant_id,
            "turn_checkpoint",
            KEY.trim()
        )
        .unwrap()["status"],
        "not_found"
    );
    store
        .begin_control_turn(
            &project(),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &grant.grant_id,
            &[],
            KEY,
            now() + TimeDelta::milliseconds(4),
        )
        .unwrap();
    let begin = view(&store, &binding, &grant.grant_id, "turn_begin", KEY).unwrap();
    assert_eq!(begin["decision"], "begin");
    assert!(begin.get("refusal_code").is_none());
    // Later lifecycle changes must not rewrite the original recorded outcome.
    store
        .checkpoint_control_turn(
            &project(),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &grant.grant_id,
            TurnNextIntent::Exit,
            "different-checkpoint",
            now() + TimeDelta::milliseconds(5),
        )
        .unwrap();
    let completed = view(
        &store,
        &binding,
        &grant.grant_id,
        "turn_checkpoint",
        "different-checkpoint",
    )
    .unwrap();
    assert_eq!(completed["decision"], "checkpointed");
    assert_eq!(
        view(&store, &binding, &grant.grant_id, "turn_checkpoint", KEY).unwrap(),
        refused
    );
    for value in [begin, completed, refused] {
        let encoded = value.to_string();
        for private in [
            KEY,
            "directive",
            "was_replayed",
            "intent_json",
            "receipt",
            &binding.routing_token,
        ] {
            assert!(!encoded.contains(private), "{private}");
        }
    }
}

#[test]
fn control_inspection_operation_begin_refusal_and_checkpoint_directive_are_safe() {
    let (mut store, binding, grant) = fixture();
    store
        .begin_control_turn(
            &project(),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &grant.grant_id,
            &["PRIVATE-PAYLOAD".into()],
            KEY,
            now(),
        )
        .unwrap();
    let refusal = view(&store, &binding, &grant.grant_id, "turn_begin", KEY).unwrap();
    assert_eq!(refusal["decision"], "refuse");
    // The current producer rejects echoed delivery tokens as a scope mismatch.
    assert_eq!(refusal["refusal_code"], "grant_scope_mismatch");
    // Projection uses the typed code, even for historical saved vocabulary.
    let (intent, _) = payloads(&store);
    replace(
        &store,
        &intent,
        br#"{"decision":"refuse","code":"lease_required"}"#,
        0,
    );
    assert_eq!(
        view(&store, &binding, &grant.grant_id, "turn_begin", KEY).unwrap()["refusal_code"],
        "lease_required"
    );
    let (mut checkpoint_store, checkpoint_binding, checkpoint_grant) = fixture();
    checkpoint(
        &mut checkpoint_store,
        &checkpoint_binding,
        &checkpoint_grant.grant_id,
    );
    let (checkpoint_intent, _) = payloads(&checkpoint_store);
    let with_directive = json!({"decision":"refuse", "code":"grant_not_begun",
        "directive": {"directive_id":"PRIVATE-PAYLOAD", "code":"grant_not_begun",
            "target":"agent", "satisfaction":"recovery_checkpoint", "recovery_effects":[]}});
    replace(
        &checkpoint_store,
        &checkpoint_intent,
        &serde_json::to_vec(&with_directive).unwrap(),
        0,
    );
    let safe = view(
        &checkpoint_store,
        &checkpoint_binding,
        &checkpoint_grant.grant_id,
        "turn_checkpoint",
        KEY,
    )
    .unwrap();
    assert_eq!(safe["refusal_code"], "grant_not_begun");
    assert!(!safe.to_string().contains("PRIVATE-PAYLOAD"));
    assert!(!safe.to_string().contains("directive"));
}

#[test]
fn control_inspection_operation_attribution_precedes_key_lookup() {
    for mutation in [
        "UPDATE control_anchors SET project_id='foreign'",
        "DELETE FROM control_anchors",
        "UPDATE control_turn_grants SET session_id='other-session'",
        "DELETE FROM control_turn_grants",
    ] {
        let (mut store, binding, grant) = fixture();
        checkpoint(&mut store, &binding, &grant.grant_id);
        store
            .connection
            .execute_batch("PRAGMA foreign_keys=OFF")
            .unwrap();
        store.connection.execute_batch(mutation).unwrap();
        let present = safe_error(view(
            &store,
            &binding,
            &grant.grant_id,
            "turn_checkpoint",
            KEY,
        ));
        let absent = safe_error(view(
            &store,
            &binding,
            &grant.grant_id,
            "turn_checkpoint",
            "not-found",
        ));
        assert_eq!(present, absent, "{mutation}");
    }
}

#[test]
fn control_inspection_operation_uses_historical_anchor_after_rebind_or_orphan_owner() {
    let (mut store, binding, grant) = fixture();
    checkpoint(&mut store, &binding, &grant.grant_id);
    let original = view(&store, &binding, &grant.grant_id, "turn_checkpoint", KEY).unwrap();
    store
        .connection
        .execute_batch(
            "PRAGMA foreign_keys=OFF;
        UPDATE control_sessions SET project_id='new-project',task_id='new-task'",
        )
        .unwrap();
    assert_eq!(
        view(&store, &binding, &grant.grant_id, "turn_checkpoint", KEY).unwrap(),
        original
    );
    store
        .connection
        .execute("DELETE FROM control_sessions", [])
        .unwrap();
    assert_eq!(
        view(&store, &binding, &grant.grant_id, "turn_checkpoint", KEY).unwrap(),
        original
    );
}

#[test]
fn control_inspection_operation_rejects_malformed_and_mismatched_intents_without_echo() {
    let (mut store, binding, grant) = fixture();
    checkpoint(&mut store, &binding, &grant.grant_id);
    let (intent, result) = payloads(&store);
    let original: Json = serde_json::from_slice(&intent).unwrap();
    let mut duplicate = String::from_utf8(intent.clone()).unwrap();
    duplicate.pop();
    write!(
        duplicate,
        ",\"grant_id\":{}}}",
        serde_json::to_string(&grant.grant_id).unwrap()
    )
    .unwrap();
    replace(&store, duplicate.as_bytes(), &result, 0);
    safe_error(view(
        &store,
        &binding,
        &grant.grant_id,
        "turn_checkpoint",
        KEY,
    ));
    for (field, value) in [
        ("grant_id", json!("another-grant")),
        ("session_id", json!("other-session")),
        ("idempotency_key", json!("PRIVATE-PAYLOAD")),
        ("control_schema_version", json!(2)),
        ("next_intent", json!("PRIVATE-PAYLOAD")),
        ("delivery_tokens", json!([])),
        ("observations", json!([{"invalid":"PRIVATE-PAYLOAD"}])),
    ] {
        let mut changed = original.clone();
        changed[field] = value;
        replace(&store, &serde_json::to_vec(&changed).unwrap(), &result, 0);
        safe_error(view(
            &store,
            &binding,
            &grant.grant_id,
            "turn_checkpoint",
            KEY,
        ));
    }
    for bytes in [
        b"PRIVATE-PAYLOAD".as_slice(),
        b"\xff",
        b"{}",
        br#"{"session_id":"a","session_id":"b"}"#,
    ] {
        replace(&store, bytes, &result, 0);
        safe_error(view(
            &store,
            &binding,
            &grant.grant_id,
            "turn_checkpoint",
            KEY,
        ));
    }
}

#[test]
fn control_inspection_operation_rejects_invalid_results_and_preserves_original_time() {
    let (mut store, binding, grant) = fixture();
    checkpoint(&mut store, &binding, &grant.grant_id);
    let (intent, result) = payloads(&store);
    for bad in [
        br#"{"decision":"PRIVATE-PAYLOAD"}"#.as_slice(),
        br#"{"decision":"refuse","code":"PRIVATE-PAYLOAD"}"#,
        br#"{"decision":"begin","receipt":{}}"#,
        br#"{"decision":"checkpointed","receipt":{}}"#,
        br#"{"decision":"refuse","code":"grant_not_begun","directive":"PRIVATE-PAYLOAD"}"#,
        b"\xff",
        b"{}",
    ] {
        replace(&store, &intent, bad, 0);
        safe_error(view(
            &store,
            &binding,
            &grant.grant_id,
            "turn_checkpoint",
            KEY,
        ));
    }
    for time in [-62_167_219_200_000_i64, 253_402_300_799_999] {
        replace(&store, &intent, &result, time);
        assert_eq!(
            view(&store, &binding, &grant.grant_id, "turn_checkpoint", KEY).unwrap()["recorded_at"],
            json!(DateTime::from_timestamp_millis(time).unwrap())
        );
    }
    for time in [i64::MIN, i64::MAX, 253_402_300_800_000] {
        replace(&store, &intent, &result, time);
        assert!(
            safe_error(view(
                &store,
                &binding,
                &grant.grant_id,
                "turn_checkpoint",
                KEY
            ))
            .contains("timestamp")
        );
    }
}

#[test]
fn control_inspection_operation_bounds_payloads_before_decode_and_preserves_exact_keys() {
    let (mut store, binding, grant) = fixture();
    checkpoint(&mut store, &binding, &grant.grant_id);
    let (intent, result) = payloads(&store);
    for which in [true, false] {
        let mut padded = if which {
            intent.clone()
        } else {
            result.clone()
        };
        padded.resize(usize::try_from(MAX_PAYLOAD_BYTES).unwrap(), b' ');
        replace(
            &store,
            if which { &padded } else { &intent },
            if which { &result } else { &padded },
            0,
        );
        assert_eq!(
            view(&store, &binding, &grant.grant_id, "turn_checkpoint", KEY).unwrap()["status"],
            "found"
        );
        padded.push(b' ');
        replace(
            &store,
            if which { &padded } else { &intent },
            if which { &result } else { &padded },
            0,
        );
        assert!(
            safe_error(view(
                &store,
                &binding,
                &grant.grant_id,
                "turn_checkpoint",
                KEY
            ))
            .contains("byte bound")
        );
    }
    for key in [
        "-leading".to_string(),
        "é".repeat(MAX_INSPECTION_KEY_BYTES / 2),
    ] {
        let selector = ControlOperationSelector::new("turn_checkpoint", &key).unwrap();
        assert_eq!(selector.key, key);
        assert!(
            ControlOperationSelector::new(
                "turn_checkpoint",
                &format!("{key}{}", "a".repeat(MAX_INSPECTION_KEY_BYTES + 1))
            )
            .is_err()
        );
    }
    for key in ["", "  ", "PRIVATE-PAYLOAD\n"] {
        let error = ControlOperationSelector::new("turn_begin", key)
            .err()
            .unwrap();
        assert!(!error.to_string().contains("PRIVATE-PAYLOAD"));
    }
}

#[test]
fn control_inspection_operation_is_inside_the_existing_admitted_snapshot() {
    let home = crate::test_support::temp_home().unwrap();
    let path = home.path().join("store.db");
    let mut writer = SqliteStore::open(&path).unwrap();
    let binding = bind_control(&mut writer, now());
    let grant = writer
        .evaluate_control_turn(
            &project(),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &TurnIntent {
                idempotency_key: "snapshot".into(),
                intent_fingerprint: ObjectId::mint(),
                purpose: None,
                requested_effects: vec![EffectClass::Observe],
                resource_intents: vec![],
            },
            now(),
        )
        .unwrap();
    let ControlTurnDecision::Grant { grant } = grant else {
        panic!("fixture grant")
    };
    checkpoint(&mut writer, &binding, &grant.grant_id);
    let reader = SqliteStore::open_existing_read_only(&path).unwrap();
    let snapshot = reader.connection.unchecked_transaction().unwrap();
    let inspect = || {
        SqliteStore::inspect_control_session_on(
            &snapshot,
            crate::HostPathPolicy::host_default(),
            &binding.status.session_id.0,
            &grant.grant_id,
            &project(),
            Some(ControlOperationSelector::new("turn_checkpoint", KEY).unwrap()),
        )
        .unwrap()
    };
    let original = serde_json::to_value(inspect().details).unwrap();
    writer
        .connection
        .execute("DELETE FROM control_operation_results", [])
        .unwrap();
    writer
        .connection
        .execute("UPDATE control_anchors SET project_id='other'", [])
        .unwrap();
    assert_eq!(serde_json::to_value(inspect().details).unwrap(), original);
    snapshot.commit().unwrap();
    assert!(
        SqliteStore::inspect_control_session(
            &path,
            Some(crate::HostPathPolicy::host_default()),
            &binding.status.session_id.0,
            &grant.grant_id,
            &project(),
            Some(ControlOperationSelector::new("turn_checkpoint", KEY).unwrap())
        )
        .is_err()
    );
}
