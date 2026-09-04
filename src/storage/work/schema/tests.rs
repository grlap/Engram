use super::super::test_support::*;
use super::super::*;

#[test]
fn current_work_schema_has_no_agent_grant_tables() {
    let store = SqliteStore::open_in_memory().expect("current work schema");
    for name in ["work_authority_grants", "work_authority_revocations"] {
        let count: i64 = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master WHERE type = 'table' AND name = ?1",
                params![name],
                |row| row.get(0),
            )
            .expect("inspect current schema");
        assert_eq!(count, 0, "agent grant table {name} must stay absent");
    }
}

#[test]
fn doctor_binds_work_projections_to_canonical_events_and_scalar_columns() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("project-integrity", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let healthy = store.verify_all().expect("healthy work integrity report");
    assert!(healthy.is_healthy(), "{healthy:?}");
    assert!(healthy.checked_work_records > 1);

    store
        .connection
        .execute(
            "UPDATE work_items SET priority = 4 WHERE work_id = ?1",
            [root.work_id.0.to_string()],
        )
        .expect("corrupt scalar projection");
    let scalar_corruption = store.verify_all().expect("scalar corruption report");
    assert!(
        scalar_corruption
            .invalid_work_records
            .iter()
            .any(|record| record.contains("scalar_binding"))
    );

    store
        .connection
        .execute(
            "UPDATE work_items SET priority = ?2,
                 item_json = CAST(json_set(item_json, '$.title', 'tampered') AS BLOB)
             WHERE work_id = ?1",
            params![root.work_id.0.to_string(), root.priority],
        )
        .expect("corrupt JSON projection");
    let json_corruption = store.verify_all().expect("JSON corruption report");
    assert!(
        json_corruption
            .invalid_work_records
            .iter()
            .any(|record| record.starts_with("work_item:"))
    );
    let refused = store.revise_work(
        &ReviseWorkRequest {
            work_id: root.work_id,
            expected_revision: root.revision,
            patch: WorkRevisionPatch {
                title: Some("must not canonize corruption".into()),
                ..WorkRevisionPatch::default()
            },
            authority: delegated("project-integrity", "planner"),
            actor: actor("planner"),
            idempotency_key: "corrupt-revision".into(),
            updated_at: at(1),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(refused, Err(StoreError::InvalidWorkProjection(_))));
}

#[test]
fn current_schema_missing_state_tables_is_refused_before_repair_ddl() {
    for table in ["work_claims", "work_feed_entries"] {
        let directory = tempfile::tempdir().expect("temp directory");
        let database = directory.path().join(format!("missing-{table}.sqlite3"));
        let mut store = SqliteStore::open(&database).expect("initialize current schema");
        let root = store
            .create_work(
                &root_request("missing-current-table", "root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("nonempty work state");
        store
            .claim_work(
                &ClaimWorkRequest {
                    work_id: root.work_id,
                    expected_work_revision: root.revision,
                    expected_run_id: root.active_run_id.expect("active run"),
                    holder: SessionId("planner".into()),
                    ttl_seconds: 60,
                    recovery_reason: None,
                    actor: actor("planner"),
                    idempotency_key: format!("claim-before-dropping-{table}"),
                    claimed_at: at(1),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("live claim fixture");
        drop(store);

        let connection = Connection::open(&database).expect("damage fixture");
        match table {
            "work_claims" => connection
                .execute_batch("DROP TABLE work_claims")
                .expect("drop claims table"),
            "work_feed_entries" => connection
                .execute_batch("DROP TABLE work_feed_entries")
                .expect("drop feed entries table"),
            _ => unreachable!("fixed test table"),
        }
        let before = connection
            .prepare("SELECT name, type, sql FROM sqlite_master ORDER BY name")
            .expect("schema query")
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })
            .expect("schema rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("schema snapshot");
        drop(connection);

        let Err(error) = SqliteStore::open(&database) else {
            panic!("damaged current schema was accepted");
        };
        assert!(
            matches!(&error, StoreError::InvalidControlProjection(message)
                if message == crate::storage::DIFFERENT_BUILD_STORE_MESSAGE),
            "unexpected error for {table}: {error}"
        );
        let connection = Connection::open(&database).expect("inspect refused schema");
        let after = connection
            .prepare("SELECT name, type, sql FROM sqlite_master ORDER BY name")
            .expect("schema query")
            .query_map([], |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, Option<String>>(2)?,
                ))
            })
            .expect("schema rows")
            .collect::<Result<Vec<_>, _>>()
            .expect("schema snapshot");
        assert_eq!(after, before, "reopen must not recreate {table}");
    }
}

#[test]
fn explicit_projection_repair_rebuilds_missing_work_indexes_and_fts() {
    let directory = tempfile::tempdir().expect("temp directory");
    let database = directory.path().join("missing-indexes.sqlite3");
    let mut store = SqliteStore::open(&database).expect("initialize current schema");
    let root = store
        .create_work(
            &root_request("missing-current-index", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("nonempty work state");
    drop(store);
    let connection = Connection::open(&database).expect("index fixture");
    connection
        .execute_batch(
            "DROP INDEX work_items_ready;
             CREATE INDEX work_items_ready ON work_items(work_id);
             DROP INDEX work_run_active;
             CREATE UNIQUE INDEX work_run_active ON work_runs(run_id);
             DROP TABLE work_catalog_fts;
             CREATE TABLE work_catalog_fts (
                 work_id TEXT,
                 search_text TEXT
             ) STRICT;",
        )
        .expect("replace rebuildable projections with wrong definitions");
    drop(connection);

    let Err(error) = SqliteStore::open(&database) else {
        panic!("ordinary open must not repair");
    };
    assert!(
        matches!(error, StoreError::InvalidWorkProjection(message) if message.contains("--repair-projections")),
        "ordinary open should direct the operator to explicit repair"
    );
    let refused = Connection::open(&database).expect("inspect refused repair fixture");
    assert_eq!(
        refused
            .query_row(
                "SELECT COUNT(*) FROM sqlite_master
                 WHERE name IN ('work_items_ready', 'work_run_active', 'work_catalog_fts')",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("count missing projections"),
        3,
        "ordinary open must preserve the wrong definitions"
    );
    drop(refused);

    let report = SqliteStore::repair_rebuildable_projections(&database)
        .expect("explicitly rebuild work projections");
    assert!(report.is_healthy(), "{report:?}");
    let reopened = SqliteStore::open(&database).expect("open repaired schema");
    assert_eq!(
        reopened.get_work_item(root.work_id).expect("work survives"),
        root
    );
    for index in ["work_items_ready", "work_run_active"] {
        assert_eq!(
            reopened
                .connection
                .query_row(
                    "SELECT type FROM sqlite_master WHERE name = ?1",
                    [index],
                    |row| row.get::<_, String>(0),
                )
                .expect("rebuilt index"),
            "index"
        );
    }
    assert_eq!(
        reopened
            .connection
            .query_row(
                "SELECT COUNT(*) FROM work_catalog_fts WHERE work_id = ?1",
                [root.work_id.0.to_string()],
                |row| row.get::<_, i64>(0),
            )
            .expect("rebuilt catalog row"),
        1
    );
    reopened
        .connection
        .execute("DELETE FROM work_catalog_fts", [])
        .expect("drift existing work FTS content");
    let corrupt = reopened.verify_all().expect("diagnose work FTS drift");
    assert!(
        corrupt
            .invalid_work_records
            .iter()
            .any(|record| record.starts_with("work_catalog:")),
        "work FTS drift should be visible: {corrupt:?}"
    );
    drop(reopened);
    let repaired = SqliteStore::repair_rebuildable_projections(&database)
        .expect("repair existing work FTS content");
    assert!(repaired.is_healthy(), "{repaired:?}");
}

#[test]
fn projection_repair_rolls_back_when_durable_work_state_is_corrupt() {
    let directory = tempfile::tempdir().expect("temp directory");
    let database = directory.path().join("corrupt-work-repair.sqlite3");
    let mut store = SqliteStore::open(&database).expect("initialize current schema");
    let root = store
        .create_work(
            &root_request("corrupt-work-repair", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("create work fixture");
    store
        .connection
        .execute(
            "UPDATE work_items SET priority = priority + 1 WHERE work_id = ?1",
            [root.work_id.0.to_string()],
        )
        .expect("corrupt durable work scalar projection");
    store
        .connection
        .execute("DELETE FROM work_catalog_fts", [])
        .expect("drift rebuildable work projection");
    let before =
        test_database_shape_snapshot(&store.connection).expect("snapshot corrupt work state");
    drop(store);

    let error = SqliteStore::repair_rebuildable_projections(&database)
        .expect_err("repair must refuse corrupt durable work state");
    assert!(
        matches!(&error, StoreError::InvalidControlProjection(message) if message.contains("verification found")),
        "unexpected repair refusal: {error}"
    );
    let after = Connection::open(&database).expect("inspect rolled-back repair");
    assert_eq!(
        test_database_shape_snapshot(&after).expect("snapshot rolled-back repair"),
        before,
        "repair must roll back every projection change when durable work verification fails"
    );
}

#[test]
fn same_name_wrong_work_table_definition_is_refused_without_mutation() {
    let directory = tempfile::tempdir().expect("temp directory");
    let database = directory.path().join("wrong-work-table.sqlite3");
    drop(SqliteStore::open(&database).expect("initialize current schema"));
    let fixture = Connection::open(&database).expect("open work definition fixture");
    fixture
        .execute_batch(
            "PRAGMA foreign_keys = OFF;
             ALTER TABLE work_item_labels RENAME TO work_item_labels_old;
             CREATE TABLE work_item_labels (
                 work_id TEXT NOT NULL REFERENCES work_items(work_id) ON DELETE CASCADE,
                 label_key TEXT,
                 PRIMARY KEY(work_id, label_key)
             ) STRICT;
             DROP TABLE work_item_labels_old;
             PRAGMA foreign_keys = ON;",
        )
        .expect("replace work table with a weaker same-name definition");
    let before = test_database_shape_snapshot(&fixture).expect("snapshot malformed work schema");
    drop(fixture);

    for operation in [
        SqliteStore::open(&database).map(|_| ()),
        SqliteStore::repair_rebuildable_projections(&database).map(|_| ()),
    ] {
        let error = operation.expect_err("wrong durable work definition must be refused");
        assert!(
            matches!(&error, StoreError::InvalidControlProjection(_)),
            "unexpected exact work-schema diagnostic: {error}"
        );
    }
    let after = Connection::open(&database).expect("inspect refused work schema");
    assert_eq!(
        test_database_shape_snapshot(&after).expect("snapshot refused work schema"),
        before,
        "open and projection repair must not rewrite durable work definitions"
    );
}

#[test]
fn doctor_reconstructs_safety_rows_and_typed_feed_membership() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("project-safety-integrity", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let decomposition = store
        .decompose_work(
            &DecomposeWorkRequest {
                parent_id: root.work_id,
                expected_parent_revision: root.revision,
                children: vec![
                    child("first", ChildRequirement::Required, "First"),
                    child("second", ChildRequirement::Required, "Second"),
                ],
                prerequisites: vec![ChildWorkPrerequisite {
                    work_key: "second".into(),
                    prerequisite: WorkDependencyRef::Proposed("first".into()),
                }],
                authority: delegated(&root.project_id.0, "planner"),
                actor: actor("planner"),
                idempotency_key: "decompose".into(),
                created_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("decompose");
    let root = decomposition.parent;
    let first = decomposition.children[0].clone();
    let second = decomposition.children[1].clone();
    let blocker = store
        .add_work_blocker(
            &AddWorkBlockerRequest {
                work_id: root.work_id,
                expected_work_revision: root.revision,
                kind: crate::domain::WorkBlockerKind::Manual,
                detail: "exercise blocker projection".into(),
                authority: delegated(&root.project_id.0, "planner"),
                actor: actor("planner"),
                idempotency_key: "block".into(),
                blocked_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("block root");
    let blocked_root = store.get_work_item(root.work_id).expect("blocked root");
    store
        .clear_work_blocker(
            &ClearWorkBlockerRequest {
                work_id: root.work_id,
                expected_work_revision: blocked_root.revision,
                blocker_id: blocker.blocker_id.clone(),
                authority: delegated(&root.project_id.0, "planner"),
                actor: actor("planner"),
                idempotency_key: "clear".into(),
                cleared_at: at(3),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("clear root blocker");
    let claim = claim(&mut store, &first, "worker", "claim", 4, 100);
    let evidence = evidence(&mut store, &first, &claim, "worker", "evidence", 5);
    checkpoint(
        &mut store,
        &first,
        &claim,
        "worker",
        "checkpoint",
        6,
        std::slice::from_ref(&evidence),
    );
    complete(
        &mut store, &first, &claim, "worker", &evidence, "complete", 7,
    )
    .expect("complete first child");
    let healthy = store
        .verify_all()
        .expect("healthy safety projection report");
    assert!(healthy.is_healthy(), "{healthy:?}");

    let prerequisite_event = store
        .connection
        .query_row(
            "SELECT event_hash FROM work_prerequisites
             WHERE work_id = ?1 AND prerequisite_id = ?2",
            params![second.work_id.0.to_string(), first.work_id.0.to_string()],
            |row| row.get::<_, String>(0),
        )
        .expect("prerequisite event");
    let other_event = store
        .connection
        .query_row(
            "SELECT object_hash FROM objects
             WHERE object_kind = 'work_event' AND object_hash != ?1 LIMIT 1",
            [&prerequisite_event],
            |row| row.get::<_, String>(0),
        )
        .expect("other event");
    store
        .connection
        .execute_batch("SAVEPOINT corrupt")
        .expect("savepoint");
    store
        .connection
        .execute(
            "UPDATE work_prerequisites SET event_hash = ?3
             WHERE work_id = ?1 AND prerequisite_id = ?2",
            params![
                second.work_id.0.to_string(),
                first.work_id.0.to_string(),
                other_event
            ],
        )
        .expect("corrupt prerequisite binding");
    let report = store.verify_all().expect("prerequisite corruption report");
    assert!(
        report
            .invalid_work_records
            .iter()
            .any(|record| record.starts_with("work_prerequisite:"))
    );
    restore_savepoint(&store);

    store
        .connection
        .execute_batch("SAVEPOINT corrupt")
        .expect("savepoint");
    store
        .connection
        .execute(
            "UPDATE work_blockers SET state = 'active' WHERE blocker_id = ?1",
            [&blocker.blocker_id],
        )
        .expect("corrupt blocker state");
    let report = store.verify_all().expect("blocker corruption report");
    assert!(
        report
            .invalid_work_records
            .iter()
            .any(|record| record.contains("event_binding"))
    );
    restore_savepoint(&store);

    store
        .connection
        .execute_batch("SAVEPOINT corrupt")
        .expect("savepoint");
    store
        .connection
        .execute(
            "UPDATE work_run_evidence SET work_id = ?2, run_id = ?3
             WHERE evidence_hash = ?1",
            params![
                evidence.as_str(),
                root.work_id.0.to_string(),
                root.active_run_id.expect("root run").0.to_string()
            ],
        )
        .expect("move evidence binding");
    let report = store.verify_all().expect("evidence corruption report");
    assert!(
        report
            .invalid_work_records
            .iter()
            .any(|record| record.contains("run_binding"))
    );
    restore_savepoint(&store);

    store
        .connection
        .execute_batch("SAVEPOINT corrupt")
        .expect("savepoint");
    store
        .connection
        .execute(
            "DELETE FROM work_completion_seals WHERE run_id = ?1",
            [claim.run_id.0.to_string()],
        )
        .expect("delete expected seal");
    let report = store.verify_all().expect("seal corruption report");
    assert!(
        report
            .invalid_work_records
            .iter()
            .any(|record| record.starts_with("completion_seal:") && record.ends_with(":missing"))
    );
    restore_savepoint(&store);

    let moved_root = WorkId::new();
    store
        .connection
        .execute_batch("SAVEPOINT corrupt")
        .expect("savepoint");
    store
        .connection
        .execute(
            "INSERT INTO work_feed_heads (feed_kind, feed_id, position)
             SELECT feed_kind, ?2, position FROM work_feed_heads
             WHERE feed_kind = 'root_work' AND feed_id = ?1",
            params![root.root_id.0.to_string(), moved_root.0.to_string()],
        )
        .expect("copy root feed head");
    store
        .connection
        .execute(
            "UPDATE work_feed_entries SET feed_id = ?2
             WHERE feed_kind = 'root_work' AND feed_id = ?1",
            params![root.root_id.0.to_string(), moved_root.0.to_string()],
        )
        .expect("move root feed entries");
    store
        .connection
        .execute(
            "DELETE FROM work_feed_heads
             WHERE feed_kind = 'root_work' AND feed_id = ?1",
            [root.root_id.0.to_string()],
        )
        .expect("delete original root feed head");
    let report = store.verify_all().expect("feed corruption report");
    assert!(
        report.invalid_work_records.iter().any(|record| {
            record.contains("wrong_membership") || record.contains("occurrences")
        })
    );
    restore_savepoint(&store);
    assert!(store.verify_all().expect("restored report").is_healthy());
}
