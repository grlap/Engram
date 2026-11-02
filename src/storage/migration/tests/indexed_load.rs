//! Scratch experiment: indexed deferred-FK loading keeps admission unchanged.

use super::*;
use std::sync::{
    Arc,
    atomic::{AtomicU64, Ordering},
};
use std::time::{Duration, Instant};

fn populated_with_memory(path: &Path) {
    populated(path);
    let mut store = SqliteStore::open_unresolved(path).unwrap();
    store
        .remember_project_memory(
            &crate::domain::RememberProjectMemoryRequest {
                project_id: ProjectId("project-json-transfer".into()),
                session_id: crate::SessionId("author".into()),
                key: Some("indexed-transfer".into()),
                revise: false,
                expected_revision: None,
                retiring_target: crate::domain::ProjectMemoryRetiringTargetChange::Keep,
                body: "Synthetic memory for foreign-key admission".into(),
                actor: actor("author"),
                created_at: Utc::now(),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
}

fn assert_unpublished(directory: &Path, target: &Path) {
    assert!(!target.exists(), "an invalid import was published");
    for sidecar in crate::storage::store_sidecars(target) {
        assert!(!sidecar.exists(), "a destination sidecar was left behind");
    }
    assert_eq!(staging_leftovers(directory), 0);
}

#[test]
fn indexed_load_still_refuses_orphans_in_each_indexed_foreign_key() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("source.db");
    populated_with_memory(&source);
    for (index, (table, column)) in [
        ("memory_heads", "version_id"),
        ("memory_heads", "assertion_id"),
        ("control_changes", "object_id"),
        ("control_policy_versions", "authority_id"),
    ]
    .into_iter()
    .enumerate()
    {
        let file = directory.path().join(format!("orphan-{index}.jsonl"));
        let target = directory.path().join(format!("orphan-{index}.db"));
        export_json(&source, &file).unwrap();
        let missing = uuid::Uuid::new_v4().simple().to_string();
        let mut document: Vec<Json> = fs::read_to_string(&file)
            .unwrap()
            .lines()
            .map(|line| serde_json::from_str(line).unwrap())
            .collect();
        if let Some(row) = document
            .iter_mut()
            .find(|line| line["row"]["table"] == table)
        {
            row["row"]["values"][column] = Json::from(missing);
        } else {
            // The ordinary fixture has no legacy control changes. Add one
            // structurally valid row so this FK must still fail at commit.
            assert_eq!(table, "control_changes");
            let task_id = uuid::Uuid::new_v4().to_string();
            let declaration = document[0]["engram_export"]["tables"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|entry| entry["name"] == table)
                .unwrap();
            assert_eq!(declaration["rows"], 0);
            declaration["rows"] = Json::from(1);
            let anchors = document[0]["engram_export"]["tables"]
                .as_array_mut()
                .unwrap()
                .iter_mut()
                .find(|entry| entry["name"] == "control_anchors")
                .unwrap();
            anchors["rows"] = Json::from(anchors["rows"].as_u64().unwrap() + 1);
            let end = document.last_mut().unwrap();
            end["end"]["rows"] = Json::from(end["end"]["rows"].as_u64().unwrap() + 2);
            // Supply the task parent too, leaving object_id as the only
            // dangling reference introduced by this case.
            document.insert(
                document.len() - 1,
                serde_json::json!({"row": {"table": "control_anchors", "values": {
                    "task_id": task_id,
                    "project_id": "project-json-transfer",
                    "external_ref": uuid::Uuid::new_v4().to_string(),
                    "title": "Synthetic orphan fixture anchor",
                    "admission_epoch": 1,
                }}}),
            );
            document.insert(
                document.len() - 1,
                serde_json::json!({"row": {"table": table, "values": {
                    "sequence": 1,
                    "task_id": task_id,
                    "task_cursor": 1,
                    "object_kind": "memory_version",
                    "object_id": missing,
                }}}),
            );
        }
        fs::write(
            &file,
            document
                .iter()
                .map(Json::to_string)
                .collect::<Vec<_>>()
                .join("\n")
                + "\n",
        )
        .unwrap();
        let error = import_json(&file, &target).unwrap_err();
        assert!(
            matches!(&error, MigrationError::Sqlite(rusqlite::Error::SqliteFailure(detail, _))
                if detail.code == rusqlite::ErrorCode::ConstraintViolation),
            "expected SQLite FK refusal for {table}.{column}: {error}"
        );
        assert!(error.to_string().contains("FOREIGN KEY"), "{error}");
        assert_unpublished(directory.path(), &target);
    }
}

#[test]
fn indexed_load_leaves_the_exact_current_schema_and_preserves_rows() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("source.db");
    let fresh = directory.path().join("fresh.db");
    let file = directory.path().join("store.jsonl");
    let target = directory.path().join("target.db");
    populated_with_memory(&source);
    drop(SqliteStore::open_unresolved(&fresh).unwrap());
    export_json(&source, &file).unwrap();
    let report = import_json(&file, &target).unwrap();
    assert!(report.checked_objects > 0);
    assert!(report.checked_work_records > 0);
    assert_eq!(rows(&source), rows(&target));

    let expected =
        crate::storage::stored_schema_definitions(&Connection::open(&fresh).unwrap()).unwrap();
    let actual =
        crate::storage::stored_schema_definitions(&Connection::open(&target).unwrap()).unwrap();
    // Compare every actual definition, including every index, rather than
    // recognizing temporary indexes by a guessed naming convention.
    assert_eq!(actual, expected, "import-only DDL survived publication");
    let reference = crate::storage::running_schema_reference().unwrap();
    assert_eq!(
        crate::storage::store_schema_reference(&fresh).unwrap(),
        reference
    );
    assert_eq!(
        crate::storage::store_schema_reference(&target).unwrap(),
        reference
    );
    assert!(
        SqliteStore::open_unresolved(&target)
            .unwrap()
            .verify_all()
            .unwrap()
            .is_healthy()
    );
    assert_eq!(staging_leftovers(directory.path()), 0);
}

#[test]
fn indexed_load_still_runs_full_doctor_for_semantically_invalid_receipts() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("source.db");
    let file = directory.path().join("bad-receipt.jsonl");
    let target = directory.path().join("bad-receipt.db");
    populated_with_memory(&source);
    let connection = Connection::open(&source).unwrap();
    assert_eq!(
        connection
            .execute(
                "UPDATE work_operation_results SET result_json = ?1
                 WHERE operation = 'create_work'",
                [br"{}".as_slice()],
            )
            .unwrap(),
        1
    );
    let violations: i64 = connection
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(violations, 0, "the bad receipt preserves all foreign keys");
    drop(connection);
    export_json(&source, &file).unwrap();
    let error = import_json(&file, &target).unwrap_err().to_string();
    assert!(
        error.contains("projection repair refused because verification found"),
        "{error}"
    );
    assert!(
        error.contains("work_operation_result:create_work:create:"),
        "{error}"
    );
    assert_unpublished(directory.path(), &target);
}

struct LoadMeasurement {
    vm_steps_lower_bound: u64,
    elapsed: Duration,
}

fn synthetic_fk_load(indexed: bool, count: usize, ids: &[String]) -> LoadMeasurement {
    const PROGRESS_INTERVAL: i32 = 1_000;
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch(
            "PRAGMA foreign_keys = ON;
             CREATE TABLE objects (object_id TEXT PRIMARY KEY) STRICT;
             CREATE TABLE memory_heads (
                 memory_id INTEGER PRIMARY KEY,
                 version_id TEXT NOT NULL REFERENCES objects(object_id),
                 assertion_id TEXT NOT NULL REFERENCES objects(object_id)
             ) STRICT;
             CREATE TABLE control_changes (
                 sequence INTEGER PRIMARY KEY,
                 task_id TEXT NOT NULL,
                 object_id TEXT NOT NULL REFERENCES objects(object_id),
                 UNIQUE(task_id, object_id)
             ) STRICT;
             CREATE TABLE control_policy_versions (
                 policy_id TEXT PRIMARY KEY REFERENCES objects(object_id),
                 authority_id TEXT NOT NULL REFERENCES objects(object_id)
             ) STRICT;",
        )
        .unwrap();
    if indexed {
        connection
            .execute_batch(
                "CREATE INDEX load_memory_version ON memory_heads(version_id);
                 CREATE INDEX load_memory_assertion ON memory_heads(assertion_id);
                 CREATE INDEX load_control_object ON control_changes(object_id);
                 CREATE INDEX load_policy_authority ON control_policy_versions(authority_id);",
            )
            .unwrap();
    }
    let callbacks = Arc::new(AtomicU64::new(0));
    let observed = Arc::clone(&callbacks);
    connection
        .progress_handler(
            PROGRESS_INTERVAL,
            Some(move || {
                observed.fetch_add(1, Ordering::Relaxed);
                false
            }),
        )
        .unwrap();
    let started = Instant::now();
    connection
        .execute_batch("BEGIN; PRAGMA defer_foreign_keys = ON;")
        .unwrap();
    {
        let mut memory = connection
            .prepare("INSERT INTO memory_heads VALUES (?1, ?2, ?3)")
            .unwrap();
        let mut changes = connection
            .prepare("INSERT INTO control_changes VALUES (?1, ?2, ?3)")
            .unwrap();
        let mut policies = connection
            .prepare("INSERT INTO control_policy_versions VALUES (?1, ?2)")
            .unwrap();
        // Exactly the child-before-parent mechanism used by alphabetical
        // transfers. Both variants use identical statements and generated ids.
        for index in 0..count {
            let row = i64::try_from(index).unwrap();
            memory
                .execute(rusqlite::params![row, &ids[3 * index], &ids[3 * index + 1]])
                .unwrap();
            changes
                .execute(rusqlite::params![
                    row,
                    format!("task-{index}"),
                    &ids[3 * index + 2]
                ])
                .unwrap();
            policies
                .execute(rusqlite::params![&ids[3 * index], &ids[3 * index + 1]])
                .unwrap();
        }
        let mut objects = connection
            .prepare("INSERT INTO objects VALUES (?1)")
            .unwrap();
        for id in ids {
            objects.execute([id]).unwrap();
        }
    }
    connection.execute_batch("COMMIT;").unwrap();
    let elapsed = started.elapsed();
    connection
        .progress_handler(0, None::<fn() -> bool>)
        .unwrap();
    let violations: i64 = connection
        .query_row("SELECT count(*) FROM pragma_foreign_key_check", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(violations, 0);
    let parents: i64 = connection
        .query_row("SELECT count(*) FROM objects", [], |row| row.get(0))
        .unwrap();
    assert_eq!(parents, i64::try_from(3 * count).unwrap());
    LoadMeasurement {
        vm_steps_lower_bound: callbacks.load(Ordering::Relaxed)
            * u64::try_from(PROGRESS_INTERVAL).unwrap(),
        elapsed,
    }
}

#[test]
#[ignore = "Synthetic deferred-FK mechanism benchmark; run explicitly on the remote runner"]
fn synthetic_child_before_parent_fk_vm_counts() {
    for count in [256, 512, 1_024] {
        let ids = (0..3 * count)
            .map(|_| uuid::Uuid::new_v4().simple().to_string())
            .collect::<Vec<_>>();
        let plain = synthetic_fk_load(false, count, &ids);
        let indexed = synthetic_fk_load(true, count, &ids);
        println!(
            "{}",
            serde_json::json!({
                "synthetic_deferred_fk": {
                    "child_rows_per_table": count,
                    "parent_rows": ids.len(),
                    "progress_interval": 1_000,
                    "unindexed_vm_steps_lower_bound": plain.vm_steps_lower_bound,
                    "indexed_vm_steps_lower_bound": indexed.vm_steps_lower_bound,
                    "unindexed_elapsed_ms": plain.elapsed.as_millis(),
                    "indexed_elapsed_ms": indexed.elapsed.as_millis(),
                }
            })
        );
        assert!(
            indexed.vm_steps_lower_bound < plain.vm_steps_lower_bound,
            "indexed FK lookups should use less VM work; elapsed time is diagnostic only"
        );
    }
}
