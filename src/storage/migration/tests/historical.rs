//! Records the live stores already hold, written by operations this build no
//! longer offers: they import unchanged and grant no authority.

use super::*;

mod policy_chain;

#[test]
fn historical_control_operation_receipts_import_unchanged() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("source.db");
    let target = directory.path().join("target.db");
    let file = directory.path().join("store.jsonl");
    populated(&source);
    let connection = Connection::open(&source).unwrap();
    let task_id = crate::TaskId(uuid::Uuid::new_v4());
    let session_id = crate::SessionId("historical-session".into());
    let bind_intent = crate::CanonicalObject::freeze(&serde_json::json!({"historical": "bind"}))
        .unwrap()
        .key()
        .clone();
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
                "lease_id": "historical-lease", "task_id": task_id,
                "holder": session_id, "fence": 1, "cursor": 1,
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
        intent["session_id"] = serde_json::json!(session_id);
        intent["idempotency_key"] = serde_json::json!(key);
        let frozen = crate::CanonicalObject::freeze(&intent).unwrap();
        connection
            .execute(
                "INSERT INTO control_operation_results (session_id, operation, idempotency_key,
                 intent_hash, intent_json, result_json, created_at_ms)
                 VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7)",
                rusqlite::params![
                    session_id.0,
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

/// A replay receipt of a work operation this build no longer runs is read as
/// stored: it is never decoded, so a shape no current type reads imports
/// unchanged, and the published store is healthy.
#[test]
fn a_retired_work_operation_receipt_imports_unchanged() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("source.db");
    let target = directory.path().join("target.db");
    let file = directory.path().join("store.jsonl");
    populated(&source);
    let connection = Connection::open(&source).unwrap();
    connection
        .execute(
            "INSERT INTO work_operation_results (
                 operation, idempotency_key, request_hash, result_json
             ) VALUES ('complete_work_recovery', 'historical-recovery', ?1, ?2)",
            rusqlite::params![
                crate::ObjectId::from_canonical_bytes(b"historical recovery").as_str(),
                br#"{"recovered":"in a shape no current type reads"}"#.as_slice()
            ],
        )
        .unwrap();
    drop(connection);
    let before = rows(&source)["work_operation_results"].clone();
    assert!(before.len() > 1, "the fixture holds current receipts too");
    export_json(&source, &file).unwrap();
    import_json(&file, &target).unwrap();
    assert_eq!(rows(&target)["work_operation_results"], before);
    let imported = SqliteStore::open(&target).unwrap();
    assert!(imported.verify_all().unwrap().is_healthy());
}
