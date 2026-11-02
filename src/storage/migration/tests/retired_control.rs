use super::*;

const RETIRED: &[&str] = &[
    "control_sessions",
    "control_turn_results",
    "control_turn_grants",
    "control_turn_grant_supersessions",
];

fn former_store(path: &Path) {
    populated(path);
    let connection = Connection::open(path).unwrap();
    connection
        .execute_batch(include_str!("retired_control.sql"))
        .unwrap();
    let task_id = uuid::Uuid::new_v4();
    connection
        .execute(
            "INSERT INTO control_anchors (task_id, project_id, external_ref, title)
             VALUES (?1, 'project-json-transfer', 'historical:retired-turns', 'Former turn fixture')",
            [task_id.to_string()],
        )
        .unwrap();
    connection.execute_batch(&format!(
        "ALTER TABLE control_policy_state ADD COLUMN required_assurance TEXT NOT NULL DEFAULT 'turn_gated';
         ALTER TABLE control_policy_state ADD COLUMN supported_effects_json TEXT NOT NULL DEFAULT '[\"observe\"]';
         ALTER TABLE control_policy_state ADD COLUMN grant_ttl_seconds INTEGER NOT NULL DEFAULT 30;
         INSERT INTO control_sessions (session_id, project_id, task_id, routing_token,
             actor_json, bind_key, bind_intent_hash, bind_intent_json, phase, assurance,
             mediated_effects_json, confirmed_cursor, project_policy_epoch, task_admission_epoch,
             blocking_watermark, capability_map_revision, revision, updated_at_ms)
         VALUES ('s', 'project-json-transfer', '{task_id}', 'route', X'7b7d', 'bind', 'intent', X'7b7d', 'ready',
             'turn_gated', '[\"observe\"]', 0, 1, 1, 0, 1, 1, 1);
         INSERT INTO control_turn_results (session_id, task_id, idempotency_key, intent_hash,
             intent_json, decision_hash, decision_json, created_at_ms)
         VALUES ('s', '{task_id}', 'request', 'intent', X'7b7d', 'decision', X'7b7d', 1);
         INSERT INTO control_turn_grants (grant_id, session_id, task_id, request_key,
             grant_json, state, issued_at_ms, expires_at_ms)
         VALUES ('grant', 's', '{task_id}', 'request', X'7b7d', 'issued', 1, 2);
         INSERT INTO control_turn_grant_supersessions (superseded_grant_id, session_id,
             task_id, replacement_request_key, replacement_decision_hash,
             supersession_json, superseded_at_ms)
         VALUES ('grant', 's', '{task_id}', 'request', 'decision', X'7b7d', 2);"
    )).unwrap();
    let foreign_key_violations: i64 = connection
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(foreign_key_violations, 0);
}

#[test]
fn former_turn_tables_and_columns_are_reported_and_survivors_are_unchanged() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("former.db");
    let file = directory.path().join("former.jsonl");
    let target = directory.path().join("current.db");
    former_store(&source);
    let mut expected = rows(&source);
    for name in RETIRED {
        expected.remove(*name).unwrap();
    }
    // Added columns follow the current four-column selector in this fixture.
    for row in expected.get_mut("control_policy_state").unwrap() {
        row.truncate(4);
    }
    export_json(&source, &file).unwrap();
    let report = import_json(&file, &target).unwrap();
    for name in RETIRED {
        let retired = report
            .left_out
            .iter()
            .find(|entry| entry.name == *name)
            .unwrap();
        assert_eq!(retired.rows, 1);
        assert_eq!(retired.reason, "retired turn-admission table");
    }
    for column in super::super::retired_control::POLICY_COLUMNS {
        let name = format!("control_policy_state.{column}");
        assert_eq!(
            report
                .left_out
                .iter()
                .find(|entry| entry.name == name)
                .unwrap()
                .rows,
            1
        );
    }
    assert_eq!(rows(&target), expected);
    assert!(
        SqliteStore::open_unresolved(&target)
            .unwrap()
            .verify_all()
            .unwrap()
            .is_healthy()
    );
}

#[test]
fn ordinary_open_refuses_the_former_schema_without_changing_it() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("former.db");
    former_store(&source);
    let before = rows(&source);
    assert!(SqliteStore::open_unresolved(&source).is_err());
    assert_eq!(rows(&source), before);
}

#[test]
fn retirement_refuses_unknown_columns_and_malformed_retired_values() {
    for sql in [
        "ALTER TABLE control_sessions ADD COLUMN unknown_input TEXT",
        "UPDATE control_turn_results SET intent_json = X'ff'",
        "ALTER TABLE control_policy_state ADD COLUMN unknown_input TEXT",
        "ALTER TABLE control_policy_state DROP COLUMN grant_ttl_seconds",
        "ALTER TABLE control_sessions DROP COLUMN tentative_cursor",
        "CREATE TABLE control_turn_results_notes (body TEXT) STRICT",
    ] {
        let directory = crate::test_support::temp_home().unwrap();
        let source = directory.path().join("former.db");
        let file = directory.path().join("former.jsonl");
        let target = directory.path().join("current.db");
        former_store(&source);
        Connection::open(&source)
            .unwrap()
            .execute_batch(sql)
            .unwrap();
        export_json(&source, &file).unwrap();
        assert!(import_json(&file, &target).is_err());
        assert!(!target.exists());
    }
}
