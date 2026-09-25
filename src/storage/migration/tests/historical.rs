//! Records the live stores already hold, written by operations this build no
//! longer offers: they import unchanged and grant no authority.

use super::*;

/// A populated store with one bound control session, the moment it was bound.
fn bound_control(
    path: &Path,
) -> (
    crate::storage::test_support::TestControlBinding,
    chrono::DateTime<Utc>,
) {
    use crate::domain::EffectClass;
    populated(path);
    let mut store =
        SqliteStore::open_with_host_path_policy(path, crate::HostPathPolicy::host_default())
            .unwrap();
    let now = chrono::DateTime::parse_from_rfc3339("2026-09-20T10:00:00Z")
        .unwrap()
        .with_timezone(&Utc);
    let binding = crate::storage::test_support::bind_control_for(
        &mut store,
        "historical-session",
        "historical-bind",
        &[EffectClass::Observe, EffectClass::MutateLocal],
        now,
    );
    (binding, now)
}

#[test]
fn historical_control_operation_receipts_import_unchanged() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("source.db");
    let target = directory.path().join("target.db");
    let file = directory.path().join("store.jsonl");
    let (binding, _) = bound_control(&source);
    let connection = Connection::open(&source).unwrap();
    let bind_intent: String = connection
        .query_row(
            "SELECT bind_intent_hash FROM control_sessions WHERE session_id = ?1",
            [&binding.status.session_id.0],
            |row| row.get(0),
        )
        .unwrap();
    let definition = crate::ObjectId::mint();
    let obligation = uuid::Uuid::new_v4();
    // Receipts of operations this build no longer offers, retained as history
    // rather than as authority.
    for (operation, mut intent, result) in [
        (
            "lease_acquire",
            serde_json::json!({
                "fingerprint_schema_version": 1, "bind_intent_hash": bind_intent,
                "kind": "execution", "mode": "exclusive",
                "subject": {"kind": "logical", "namespace": "historical",
                    "segments": ["resource"], "coverage": "exact"}, "ttl_seconds": 60
            }),
            serde_json::json!({"decision": "refuse", "directive": {
                "directive_id": "historical-policy-refusal", "code": "capability_not_permitted",
                "target": "host", "satisfaction": "host_transition", "recovery_effects": ["observe"]
            }}),
        ),
        (
            "lease_release",
            serde_json::json!({
                "control_schema_version": crate::CONTROL_SCHEMA_VERSION,
                "lease_id": "historical-lease"
            }),
            serde_json::json!({
                "lease_id": "historical-lease", "task_id": binding.status.task_id,
                "holder": binding.status.session_id, "fence": 1, "cursor": 1,
                "released_at": "2026-09-20T10:00:00Z"
            }),
        ),
        (
            "obligation_waive",
            serde_json::json!({
                "control_schema_version": crate::CONTROL_SCHEMA_VERSION,
                "bind_intent_hash": bind_intent,
                "obligation_id": obligation, "expected_definition": definition,
                "waived_by": "historical-operator", "reason": "historical waiver request"
            }),
            serde_json::json!({
                "decision": "refused", "code": "waiver_not_admitted", "obligation_id": obligation,
                "current_definition": definition,
                "remedy": "bind the host control session to the live claim for this obligation run"
            }),
        ),
    ] {
        let key = format!("historical-{operation}");
        intent["session_id"] = serde_json::json!(binding.status.session_id);
        intent["idempotency_key"] = serde_json::json!(key);
        let frozen = crate::CanonicalObject::freeze(&intent).unwrap();
        connection
            .execute(
                "INSERT INTO control_operation_results (session_id, operation, idempotency_key,
                 intent_hash, intent_json, result_json, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    binding.status.session_id.0,
                    operation,
                    key,
                    frozen.key().as_str(),
                    frozen.bytes(),
                    crate::canonical::canonical_bytes(&result).unwrap(),
                    1_790_000_000_000_i64
                ],
            )
            .unwrap();
    }
    drop(connection);
    let before = rows(&source)["control_operation_results"].clone();
    assert_eq!(before.len(), 3, "the fixture holds each historical receipt");
    export_json(&source, &file).unwrap();
    import_json(&file, &target).unwrap();
    assert_eq!(rows(&target)["control_operation_results"], before);
    let imported =
        SqliteStore::open_with_host_path_policy(&target, crate::HostPathPolicy::host_default())
            .unwrap();
    assert!(imported.verify_all().unwrap().is_healthy());

    // A historical receipt is admitted by its shape, not by its name alone.
    let connection = Connection::open(&source).unwrap();
    let release: i64 = connection
        .query_row(
            "SELECT sequence FROM control_operation_results WHERE operation = 'lease_release'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    connection
        .execute(
            "UPDATE control_operation_results SET result_json = ?1
             WHERE operation = 'lease_release'",
            [crate::canonical::canonical_bytes(
                &serde_json::json!({"released_at": "2026-09-20T10:00:00Z"}),
            )
            .unwrap()],
        )
        .unwrap();
    drop(connection);
    let broken_file = directory.path().join("broken.jsonl");
    let broken_target = directory.path().join("broken.db");
    export_json(&source, &broken_file).unwrap();
    let error = import_json(&broken_file, &broken_target)
        .expect_err("a lease release receipt without its lease must refuse")
        .to_string();
    assert!(
        error.contains(&format!("invalid labels: control_operation:{release}")),
        "{error}"
    );
    assert!(!broken_target.exists(), "published an unhealthy store");
}

#[test]
fn historical_lease_refusal_imports_and_replays_without_lease_authority() {
    use crate::domain::{EffectClass, TurnIntent, TurnPurpose};
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("source.db");
    let target = directory.path().join("target.db");
    let file = directory.path().join("store.jsonl");
    let (binding, now) = bound_control(&source);
    let intent = TurnIntent {
        idempotency_key: "unleased-mutation".into(),
        intent_fingerprint: crate::ObjectId::mint(),
        purpose: Some(TurnPurpose::Ordinary),
        requested_effects: vec![EffectClass::MutateLocal],
        resource_intents: vec![],
    };
    // Written by an earlier evaluator; the current one never produces this code.
    let decision = serde_json::json!({"decision": "refuse", "directive": {
        "directive_id": "unleased-mutation:lease_required", "code": "lease_required",
        "target": "host", "satisfaction": "host_transition", "recovery_effects": ["observe"]
    }});
    let saved_intent = crate::CanonicalObject::freeze(&serde_json::json!({
        "control_schema_version": crate::CONTROL_SCHEMA_VERSION,
        "session_id": binding.status.session_id,
        "task_id": binding.status.task_id,
        "intent": intent
    }))
    .unwrap();
    let saved_decision = crate::CanonicalObject::freeze(&decision).unwrap();
    Connection::open(&source)
        .unwrap()
        .execute(
            "INSERT INTO control_turn_results (session_id, task_id, idempotency_key,
             intent_hash, intent_json, decision_hash, decision_json, created_at_ms)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            rusqlite::params![
                binding.status.session_id.0,
                binding.status.task_id.0.to_string(),
                intent.idempotency_key,
                saved_intent.key().as_str(),
                saved_intent.bytes(),
                saved_decision.key().as_str(),
                saved_decision.bytes(),
                now.timestamp_millis()
            ],
        )
        .unwrap();
    let before = rows(&source)["control_turn_results"].clone();
    export_json(&source, &file).unwrap();
    import_json(&file, &target).unwrap();
    assert_eq!(rows(&target)["control_turn_results"], before);
    let mut imported =
        SqliteStore::open_with_host_path_policy(&target, crate::HostPathPolicy::host_default())
            .unwrap();
    assert!(imported.verify_all().unwrap().is_healthy());
    let replay = imported
        .evaluate_control_turn(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &intent,
            now + chrono::Duration::seconds(1),
        )
        .unwrap();
    assert_eq!(serde_json::to_value(replay).unwrap(), decision);
    assert_eq!(rows(&target)["control_turn_results"], before);
}
