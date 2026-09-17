use std::fs;

use crate::test_support::temp_home;
use rusqlite::Connection;

use super::{
    ExportManifest, MigrationProfile, TableDisposition, compare_export_to_source, export_store,
    import_archive, restore_source_layout, verify_export,
};
use crate::work_service::{
    LocalWorkService, WorkCompleteInput, WorkCompleteResult, WorkCompletionCaptureInput,
    WorkNextQuery, WorkNextSection, WorkNextView, WorkProposeInput, WorkProposeResult,
};

#[test]
fn migration_export_preserves_unknown_empty_tables_and_raw_sqlite_cells() {
    let directory = temp_home().expect("directory");
    let source = directory.path().join("source.db");
    let connection = Connection::open(&source).expect("source");
    connection.execute_batch("
        PRAGMA user_version = 73;
        CREATE TABLE unknown_empty (x BLOB);
        CREATE TABLE data (id INTEGER PRIMARY KEY, n, i, r, t, b);
        INSERT INTO data VALUES (8, NULL, -9223372036854775808, 1.25, CAST(x'ff00' AS TEXT), x'ff00');
        CREATE TABLE keyed (k TEXT PRIMARY KEY, v BLOB) WITHOUT ROWID;
        INSERT INTO keyed VALUES ('z', x''), ('a', x'00');
        CREATE VIEW visible AS SELECT id FROM data;
        CREATE TRIGGER keep_empty AFTER DELETE ON data BEGIN INSERT INTO unknown_empty VALUES (OLD.b); END;
    ").expect("fixture");
    drop(connection);
    let before = fs::read(&source).expect("source bytes");
    let output = directory.path().join("export.db");
    let manifest = export_store(&source, &output).expect("export old/unknown schema");
    assert_eq!(manifest.source_user_version, 73);
    assert_eq!(manifest.total_rows, 3);
    assert_eq!(manifest.empty_tables, ["unknown_empty"]);
    assert_eq!(manifest.tables.len(), 3);
    assert!(manifest.schema.iter().any(|entry| entry.kind == "trigger"));
    assert!(manifest.schema.iter().any(|entry| entry.kind == "view"));
    let archive = Connection::open(&output).expect("archive");
    let cells: Vec<u8> = archive
        .query_row(
            "SELECT cells FROM migration_rows WHERE table_name = 'data'",
            [],
            |row| row.get(0),
        )
        .expect("cells");
    let mut expected = vec![1];
    expected.extend_from_slice(&8_i64.to_be_bytes()); // rowid
    expected.push(1);
    expected.extend_from_slice(&8_i64.to_be_bytes()); // declared id
    expected.push(0);
    expected.push(1);
    expected.extend_from_slice(&i64::MIN.to_be_bytes());
    expected.push(2);
    expected.extend_from_slice(&1.25_f64.to_bits().to_be_bytes());
    for tag in [3, 4] {
        expected.push(tag);
        expected.extend_from_slice(&2_u64.to_be_bytes());
        expected.extend_from_slice(&[255, 0]);
    }
    assert_eq!(cells, expected);
    let document: Vec<u8> = archive
        .query_row("SELECT document FROM migration_manifest", [], |row| {
            row.get(0)
        })
        .expect("manifest");
    assert_eq!(
        serde_json::from_slice::<ExportManifest>(&document).expect("decode"),
        manifest
    );
    let second = export_store(&source, &directory.path().join("second.db")).expect("repeat");
    assert_eq!(second, manifest);
    assert_eq!(fs::read(&source).expect("source unchanged"), before);
}

#[test]
fn migration_export_reads_committed_wal_under_writer_and_never_overwrites() {
    let directory = temp_home().expect("directory");
    let source = directory.path().join("source.db");
    let connection = Connection::open(&source).expect("source");
    connection.execute_batch("PRAGMA journal_mode = WAL; CREATE TABLE t(x); INSERT INTO t VALUES ('committed'); BEGIN IMMEDIATE; INSERT INTO t VALUES ('pending');").expect("writer");
    let output = directory.path().join("archive.db");
    assert_eq!(
        export_store(&source, &output)
            .expect("read under writer")
            .total_rows,
        1
    );
    let before = fs::read(&output).expect("archive");
    assert!(export_store(&source, &output).is_err());
    assert_eq!(fs::read(&output).expect("unchanged"), before);
    assert!(export_store(&source, &source).is_err());
    connection.execute_batch("ROLLBACK").expect("rollback");
    assert!(!fs::read_dir(directory.path()).expect("list").any(|entry| {
        entry
            .expect("entry")
            .file_name()
            .to_string_lossy()
            .starts_with(".engram-migration-")
    }));
}

#[test]
fn migration_export_covers_current_store_including_fts_shadow_tables() {
    let directory = temp_home().expect("directory");
    let source = directory.path().join("source.db");
    let store = crate::SqliteStore::open_unresolved(&source).expect("current store");
    drop(store);
    let manifest =
        export_store(&source, &directory.path().join("archive.db")).expect("full export");
    assert!(manifest.tables.iter().any(|table| table.kind == "shadow"));
    assert!(
        manifest
            .tables
            .iter()
            .any(|table| table.name == "objects" && table.rows > 0)
    );
    let source = Connection::open(&source).expect("source");
    let names: Vec<String> = source
        .prepare("SELECT name FROM sqlite_schema WHERE type = 'table' ORDER BY name")
        .expect("query")
        .query_map([], |row| row.get(0))
        .expect("names")
        .collect::<Result<_, _>>()
        .expect("collect");
    assert_eq!(
        manifest
            .tables
            .iter()
            .map(|table| table.name.clone())
            .collect::<Vec<_>>(),
        names
    );
}

#[test]
fn migration_export_rejects_damage_and_self_consistent_truncation_against_source() {
    use sha2::{Digest, Sha256};
    let directory = temp_home().expect("directory");
    let source = directory.path().join("source.db");
    let connection = Connection::open(&source).expect("source");
    connection.execute_batch("CREATE TABLE a(x); CREATE TABLE b(x); CREATE TABLE empty(x); INSERT INTO a VALUES (1), (2); INSERT INTO b VALUES (3);").expect("fixture");
    let output = directory.path().join("archive.db");
    let original = export_store(&source, &output).expect("export");
    assert_eq!(
        compare_export_to_source(&source, &output).expect("compare"),
        original
    );
    let archive = Connection::open(&output).expect("archive");
    archive
        .execute("DELETE FROM migration_rows WHERE table_name = 'b'", [])
        .expect("truncate");
    assert!(verify_export(&output).is_err());
    let mut forged = original;
    forged
        .schema
        .retain(|entry| entry.table != "b" && entry.table != "empty");
    forged
        .tables
        .retain(|table| table.name != "b" && table.name != "empty");
    forged.total_rows -= 1;
    forged.empty_tables.clear();
    let document = serde_json::to_vec(&forged).expect("document");
    archive
        .execute(
            "UPDATE migration_manifest SET document = ?1, document_sha256 = ?2",
            rusqlite::params![document, format!("{:x}", Sha256::digest(&document))],
        )
        .expect("rewrite manifest");
    assert_eq!(
        verify_export(&output).expect("self-consistent is not complete"),
        forged
    );
    assert!(compare_export_to_source(&source, &output).is_err());
}

#[test]
fn migration_export_refuses_unrepresentable_rowids_without_publishing_output() {
    let directory = temp_home().expect("directory");
    let source = directory.path().join("source.db");
    let connection = Connection::open(&source).expect("source");
    connection
        .execute_batch("CREATE TABLE t(rowid, _rowid_, oid); INSERT INTO t VALUES (1, 2, 3);")
        .expect("fixture");
    let output = directory.path().join("archive.db");
    assert!(export_store(&source, &output).is_err());
    assert!(!output.exists());
    assert_eq!(
        fs::read_dir(directory.path()).expect("directory").count(),
        1
    );
}

#[test]
fn migration_restore_current_layout_preserves_all_rows_and_sequence_high_water() {
    let directory = temp_home().expect("directory");
    let source = directory.path().join("source.db");
    let store = crate::SqliteStore::open_unresolved(&source).expect("source");
    store
        .connection
        .execute(
            "INSERT INTO sqlite_sequence(name, seq) VALUES ('control_turn_results', 99)",
            [],
        )
        .expect("high water above all surviving rows");
    let before = crate::storage::test_database_shape_snapshot(&store.connection).expect("before");
    drop(store);
    let archive = directory.path().join("archive.db");
    let exported = export_store(&source, &archive).expect("export");
    let restored = directory.path().join("restored.db");
    assert_eq!(
        restore_source_layout(&archive, &restored).expect("raw restore"),
        exported
    );
    let store = crate::SqliteStore::open_unresolved(&restored).expect("ordinary current open");
    assert!(store.verify_all().expect("doctor").is_healthy());
    let after = crate::storage::test_database_shape_snapshot(&store.connection).expect("after");
    assert_eq!(before.rows, after.rows);
    assert_eq!(before.table_info, after.table_info);
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT seq FROM sqlite_sequence WHERE name = 'control_turn_results'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .expect("sequence"),
        99
    );
}

#[test]
fn migration_restore_refuses_unknown_profile_without_output() {
    let directory = temp_home().expect("directory");
    let source = directory.path().join("source.db");
    let connection = Connection::open(&source).expect("source");
    connection
        .execute_batch("CREATE TABLE unrecognized(x); INSERT INTO unrecognized VALUES (1);")
        .expect("fixture");
    let archive = directory.path().join("archive.db");
    export_store(&source, &archive).expect("generic export");
    let output = directory.path().join("restored.db");
    assert!(restore_source_layout(&archive, &output).is_err());
    assert!(!output.exists());
}

#[test]
fn migration_restore_aggregate_without_intake_indexes() {
    let directory = temp_home().expect("directory");
    let source = directory.path().join("source.db");
    aggregate_profile(&source);
    let archive = directory.path().join("archive.db");
    let exported = export_store(&source, &archive).expect("export");
    let output = directory.path().join("restored.db");
    assert_eq!(
        restore_source_layout(&archive, &output).expect("exact source layout"),
        exported
    );
    assert!(
        crate::SqliteStore::open_unresolved(&output).is_err(),
        "raw unpack must not claim an upgrade"
    );
}

pub(in crate::storage::migration) fn aggregate_profile(source: &std::path::Path) {
    let store = crate::SqliteStore::open_unresolved(source).expect("source");
    store
        .connection
        .execute_batch(
            "PRAGMA foreign_keys = OFF;
        DROP TABLE work_root_members;
        DROP INDEX work_root_execution_active;
        DROP TABLE work_root_executions;
        CREATE TABLE work_root_executions (
             root_execution_id TEXT PRIMARY KEY,
             project_id TEXT NOT NULL,
             root_id TEXT NOT NULL REFERENCES work_items(work_id),
             generation INTEGER NOT NULL,
             state TEXT NOT NULL,
             revision INTEGER NOT NULL,
             created_at_ms INTEGER NOT NULL,
             updated_at_ms INTEGER NOT NULL,
             execution_json BLOB NOT NULL,
             UNIQUE(root_id, generation)
         ) STRICT;
        CREATE UNIQUE INDEX work_root_execution_active
             ON work_root_executions(root_id) WHERE state = 'active';
        DROP INDEX objects_work_source_key;
        DROP INDEX objects_work_source_proposal_work;
        DROP INDEX work_items_source_snapshot;
        DROP INDEX control_work_leases_task_state;
        CREATE INDEX control_work_leases_task_state
                  ON control_work_leases(task_id, state, expires_at_ms);
    ",
        )
        .expect("source predecessor schema");
    drop(store);
}

#[test]
fn migration_import_empty_work_profile_opens_current_without_repair_and_never_overwrites() {
    let directory = temp_home().expect("fixture");
    let source = directory.path().join("source.db");
    aggregate_profile(&source);
    let archive = directory.path().join("archive.db");
    let manifest = export_store(&source, &archive).expect("export");
    let output = directory.path().join("current.db");
    let report = super::import_archive(&archive, &output).expect("complete bootstrap import");
    assert_eq!(report.profile, MigrationProfile::AggregateRootV1);
    assert!(report.dispositions.iter().any(|table| {
        table.name == "objects" && table.disposition == TableDisposition::Transform
    }));
    assert!(report.dispositions.iter().any(|table| {
        table.name == "work_root_executions" && table.disposition == TableDisposition::Transform
    }));
    assert_eq!(report.source, manifest);
    assert!(!report.installed);
    assert_eq!(report.conversion.changed_objects, 0);
    let store = crate::SqliteStore::open_unresolved(&output).expect("current format");
    assert!(store.verify_all().expect("doctor").is_healthy());
    let before =
        crate::storage::test_database_shape_snapshot(&store.connection).expect("before fault");
    store
        .connection
        .execute_batch("SAVEPOINT missing_original")
        .expect("savepoint");
    let removed = store.connection.execute("DELETE FROM migration_original_rows WHERE (source_id,table_name,row_number) IN (SELECT source_id,table_name,row_number FROM migration_original_rows ORDER BY source_id,table_name,row_number LIMIT 1)", []).expect("remove one retained row");
    assert_eq!(removed, 1, "the negative control must remove actual data");
    assert!(
        store.verify_all().is_err(),
        "doctor must audit retained rows, not only active projections"
    );
    store
        .connection
        .execute_batch("ROLLBACK TO missing_original; RELEASE missing_original")
        .expect("restore fixture");
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&store.connection).expect("after fault"),
        before
    );
    assert!(store.verify_all().expect("restored doctor").is_healthy());
    assert!(super::import_archive(&archive, &output).is_err());
}

fn assert_core_complete_result_is_mapped_seal(connection: &Connection, mapped: &str) {
    let count: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM work_operation_results WHERE operation = 'complete_work'",
            [],
            |row| row.get(0),
        )
        .expect("core complete_work rows");
    assert_eq!(count, 1, "one complete_work result");
    let result: Vec<u8> = connection
        .query_row(
            "SELECT result_json FROM work_operation_results WHERE operation = 'complete_work'",
            [],
            |row| row.get(0),
        )
        .expect("core complete_work bytes");
    let seal: Vec<u8> = connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_hash = ?1",
            [mapped],
            |row| row.get(0),
        )
        .expect("mapped seal bytes");
    assert_eq!(result, seal);
}

fn assert_same_durable(left: &Connection, right: &Connection) {
    let left_rows = durable_rows(left);
    let right_rows = durable_rows(right);
    assert_eq!(
        left_rows
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        right_rows
            .iter()
            .map(|(name, _)| name.as_str())
            .collect::<Vec<_>>(),
        "durable table names"
    );
    for ((name, left), (_, right)) in left_rows.iter().zip(right_rows.iter()) {
        assert_eq!(left.len(), right.len(), "{name} row count");
        assert_eq!(left, right, "{name} row bytes");
    }
}

fn durable_rows(connection: &Connection) -> Vec<(String, Vec<Vec<u8>>)> {
    let names: Vec<String> = connection
        .prepare("SELECT name FROM pragma_table_list WHERE type = 'table' ORDER BY name")
        .expect("table list")
        .query_map([], |row| row.get(0))
        .expect("names")
        .collect::<Result<_, _>>()
        .expect("collect");
    names
        .into_iter()
        .filter(|name| name != "sqlite_schema" && name != "sqlite_temp_schema")
        .map(|name| {
            let quoted = name.replace('"', "\"\"");
            let mut statement = connection
                .prepare(&format!("SELECT * FROM \"{quoted}\""))
                .expect("rows");
            let count = statement.column_count();
            let mut query = statement.query([]).expect("query");
            let mut rows = Vec::new();
            while let Some(row) = query.next().expect("row") {
                rows.push(super::rows::encode(row, count).expect("encode"));
            }
            rows.sort();
            (name, rows)
        })
        .collect()
}

fn logical_fts(connection: &Connection) -> (Vec<Vec<u8>>, Vec<Vec<u8>>) {
    let encode = |sql: &str| {
        let mut statement = connection.prepare(sql).expect("fts");
        let count = statement.column_count();
        let mut query = statement.query([]).expect("query");
        let mut rows = Vec::new();
        while let Some(row) = query.next().expect("row") {
            rows.push(super::rows::encode(row, count).expect("encode"));
        }
        rows
    };
    (
        encode("SELECT object_hash,title,body FROM object_fts ORDER BY object_hash"),
        encode("SELECT work_id,search_text FROM work_catalog_fts ORDER BY work_id"),
    )
}

fn current_root_input() -> WorkProposeInput {
    WorkProposeInput::Root {
        evaluation_mode: None,
        title: "Current import fixture".into(),
        outcome: "Keep every durable row".into(),
        acceptance: vec!["round-trip".into()],
        external_ref: None,
        notes: vec!["seed note".into()],
        work_kind: None,
        priority: Some(1),
        labels: vec!["migration".into()],
        assigned_to: None,
        deferred_until: None,
        idempotency_key: "current-root".into(),
    }
}

fn populate_current(path: &std::path::Path) -> (WorkProposeResult, WorkNextView) {
    let now = chrono::Utc::now();
    let service = LocalWorkService::new(
        path.to_path_buf(),
        crate::ProjectId("migration-current".into()),
        "author".into(),
        crate::SessionId("session".into()),
        None,
    );
    let first = service
        .work_propose(current_root_input(), now)
        .expect("root");
    let replay = service
        .work_propose(current_root_input(), now)
        .expect("replay same propose");
    assert_eq!(
        serde_json::to_value(&replay).expect("replay JSON"),
        serde_json::to_value(&first).expect("first JSON"),
        "source idempotent propose must be exact and have no extra effects"
    );
    let pending = service
        .work_next(
            20,
            WorkNextQuery {
                sections: vec![WorkNextSection::Changes],
                ..WorkNextQuery::default()
            },
            now,
        )
        .expect("pending delivery");
    assert!(pending.delivery_token.is_some());
    assert!(pending.delivered_through.is_some());
    let store = crate::SqliteStore::open_unresolved(path).expect("open");
    let updated = store
        .connection
        .execute(
            "UPDATE sqlite_sequence SET seq = MAX(seq, 77) WHERE name = 'control_turn_results'",
            [],
        )
        .expect("bump sequence");
    if updated == 0 {
        store
            .connection
            .execute(
                "INSERT INTO sqlite_sequence(name, seq) VALUES ('control_turn_results', 77)",
                [],
            )
            .expect("sequence high water");
    }
    drop(store);
    (first, pending)
}

#[test]
fn migration_current_import_twice_preserves_rows_provenance_and_replay() {
    let directory = temp_home().expect("directory");
    let source = directory.path().join("source.db");
    let (original_propose, pending) = populate_current(&source);
    let first_archive = directory.path().join("first.db");
    let first = directory.path().join("imported.db");
    let exported = export_store(&source, &first_archive).expect("export");
    let report = import_archive(&first_archive, &first).expect("current import");
    assert_eq!(report.profile, MigrationProfile::Current);
    assert!(report.dispositions.iter().any(|table| {
        table.name == "objects" && table.disposition == TableDisposition::Unchanged
    }));
    assert!(report.dispositions.iter().any(|table| {
        table.name == "object_fts" && table.disposition == TableDisposition::Rebuild
    }));
    assert!(
        report
            .dispositions
            .iter()
            .all(|table| table.disposition != TableDisposition::Transform)
    );
    assert_eq!(report.source, exported);
    assert!(!report.installed);
    assert_eq!(report.conversion.changed_objects, 0);
    assert_eq!(report.reexpressed_completion_results, 0);
    let source_store = crate::SqliteStore::open_unresolved(&source).expect("source");
    let imported = crate::SqliteStore::open_unresolved(&first).expect("imported");
    assert!(imported.verify_all().expect("doctor").is_healthy());
    assert_same_durable(&source_store.connection, &imported.connection);
    assert_eq!(
        logical_fts(&source_store.connection),
        logical_fts(&imported.connection)
    );
    drop(source_store);
    drop(imported);
    let second_archive = directory.path().join("second.db");
    let second = directory.path().join("imported-again.db");
    export_store(&first, &second_archive).expect("re-export");
    let again = import_archive(&second_archive, &second).expect("second current import");
    assert_eq!(again.profile, MigrationProfile::Current);
    let first_store = crate::SqliteStore::open_unresolved(&first).expect("first");
    let second_store = crate::SqliteStore::open_unresolved(&second).expect("second");
    assert_same_durable(&first_store.connection, &second_store.connection);
    let maps: i64 = second_store
        .connection
        .query_row("SELECT COUNT(*) FROM migration_object_map", [], |row| {
            row.get(0)
        })
        .expect("maps");
    assert_eq!(maps, 0, "current no-op must not invent remappings");
    drop(first_store);
    drop(second_store);
    let imported = LocalWorkService::new(
        second.clone(),
        crate::ProjectId("migration-current".into()),
        "author".into(),
        crate::SessionId("session".into()),
        None,
    );
    let roots_before: i64 = {
        let connection = Connection::open(&second).expect("count");
        connection
            .query_row(
                "SELECT COUNT(*) FROM work_items WHERE parent_id IS NULL",
                [],
                |row| row.get(0),
            )
            .expect("roots")
    };
    let attempts_before: i64 = {
        let connection = Connection::open(&second).expect("attempts");
        connection
            .query_row(
                "SELECT COUNT(*) FROM work_protocol_attempts WHERE idempotency_key = 'current-root'",
                [],
                |row| row.get(0),
            )
            .expect("attempts")
    };
    let replayed = imported
        .work_propose(current_root_input(), chrono::Utc::now())
        .expect("operation replay after current import");
    assert_eq!(
        serde_json::to_value(&replayed).expect("imported replay JSON"),
        serde_json::to_value(&original_propose).expect("original propose JSON"),
        "imported propose replay must return the original exact result"
    );
    let connection = Connection::open(&second).expect("after replay");
    let roots_after: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM work_items WHERE parent_id IS NULL",
            [],
            |row| row.get(0),
        )
        .expect("roots after");
    let attempts_after: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM work_protocol_attempts WHERE idempotency_key = 'current-root'",
            [],
            |row| row.get(0),
        )
        .expect("attempts after");
    assert_eq!(roots_before, 1);
    assert_eq!(roots_after, roots_before);
    assert_eq!(attempts_after, attempts_before);
    drop(connection);
    let query = WorkNextQuery {
        sections: vec![WorkNextSection::Changes],
        ..WorkNextQuery::default()
    };
    let now = chrono::Utc::now();
    assert!(
        imported
            .work_next_with_delivery_token(
                20,
                pending.delivered_through,
                Some("wrong-token"),
                query.clone(),
                now,
            )
            .is_err(),
        "imported pending page must still require the original token"
    );
    imported
        .work_next_with_delivery_token(
            20,
            pending.delivered_through,
            pending.delivery_token.as_deref(),
            query,
            now,
        )
        .expect("pending delivery token replay after current import");
}

#[test]
fn migration_current_import_preserves_prior_aggregate_provenance() {
    let directory = temp_home().expect("directory");
    let source = directory.path().join("source.db");
    aggregate_profile(&source);
    let archive = directory.path().join("aggregate.db");
    export_store(&source, &archive).expect("export");
    let converted = directory.path().join("converted.db");
    let first = import_archive(&archive, &converted).expect("aggregate");
    assert_eq!(first.profile, MigrationProfile::AggregateRootV1);
    assert!(first.dispositions.iter().any(|table| {
        table.name == "objects" && table.disposition == TableDisposition::Transform
    }));
    let converted_store = crate::SqliteStore::open_unresolved(&converted).expect("converted");
    let maps_before: i64 = converted_store
        .connection
        .query_row("SELECT COUNT(*) FROM migration_object_map", [], |row| {
            row.get(0)
        })
        .expect("maps");
    assert!(maps_before > 0);
    drop(converted_store);
    let current_archive = directory.path().join("current.db");
    export_store(&converted, &current_archive).expect("export converted");
    let round = directory.path().join("round.db");
    let report = import_archive(&current_archive, &round).expect("current after aggregate");
    assert_eq!(report.profile, MigrationProfile::Current);
    let before = crate::SqliteStore::open_unresolved(&converted).expect("before");
    let after = crate::SqliteStore::open_unresolved(&round).expect("after");
    assert_same_durable(&before.connection, &after.connection);
    let maps_after: i64 = after
        .connection
        .query_row("SELECT COUNT(*) FROM migration_object_map", [], |row| {
            row.get(0)
        })
        .expect("maps");
    assert_eq!(maps_before, maps_after);
    assert_eq!(report.reexpressed_completion_results, 0);
    let audits: i64 = after
        .connection
        .query_row(
            "SELECT COUNT(*) FROM migration_reexpressed_results",
            [],
            |row| row.get(0),
        )
        .expect("audits");
    assert_eq!(
        audits, 0,
        "empty-work predecessor has no completion rewrites"
    );
}

#[test]
fn migration_import_refuses_corruption_unknown_profile_and_existing_destination() {
    let directory = temp_home().expect("directory");
    let source = directory.path().join("source.db");
    let _ = populate_current(&source);
    let archive = directory.path().join("archive.db");
    export_store(&source, &archive).expect("export");
    let output = directory.path().join("out.db");
    import_archive(&archive, &output).expect("first");
    assert!(import_archive(&archive, &output).is_err());
    assert!(output.exists());

    let unknown = directory.path().join("unknown.db");
    let connection = Connection::open(&unknown).expect("unknown");
    connection
        .execute_batch("CREATE TABLE unrecognized(x); INSERT INTO unrecognized VALUES (1);")
        .expect("fixture");
    drop(connection);
    let unknown_archive = directory.path().join("unknown-archive.db");
    export_store(&unknown, &unknown_archive).expect("generic export");
    let unknown_out = directory.path().join("unknown-out.db");
    assert!(import_archive(&unknown_archive, &unknown_out).is_err());
    assert!(!unknown_out.exists());

    let damaged = directory.path().join("damaged.db");
    fs::copy(&archive, &damaged).expect("copy");
    let archive_connection = Connection::open(&damaged).expect("archive");
    archive_connection
        .execute("DELETE FROM migration_rows", [])
        .expect("corrupt");
    drop(archive_connection);
    let damaged_out = directory.path().join("damaged-out.db");
    assert!(import_archive(&damaged, &damaged_out).is_err());
    assert!(!damaged_out.exists());
}

#[test]
fn migration_import_refuses_current_layout_without_all_provenance_tables() {
    let directory = temp_home().expect("directory");
    let source = directory.path().join("source.db");
    drop(crate::SqliteStore::open_unresolved(&source).expect("current"));
    let connection = Connection::open(&source).expect("open");
    connection
        .execute_batch("PRAGMA foreign_keys=OFF")
        .expect("fk off");
    for table in super::schema::TABLES {
        connection
            .execute_batch(&format!("DROP TABLE {table}"))
            .expect("drop provenance");
    }
    drop(connection);
    let archive = directory.path().join("archive.db");
    export_store(&source, &archive).expect("export current-shaped without provenance tables");
    let restored = directory.path().join("restored.db");
    restore_source_layout(&archive, &restored)
        .expect("unpack still restores the all-absent provenance layout");
    let imported = directory.path().join("imported.db");
    let error = import_archive(&archive, &imported).expect_err("not a current import profile");
    assert!(
        error
            .to_string()
            .contains("current import requires all six migration provenance tables"),
        "{error}"
    );
    assert!(!imported.exists());

    let partial = directory.path().join("partial.db");
    drop(crate::SqliteStore::open_unresolved(&partial).expect("current"));
    let connection = Connection::open(&partial).expect("partial");
    connection
        .execute_batch("PRAGMA foreign_keys=OFF; DROP TABLE migration_object_map")
        .expect("drop one provenance table");
    drop(connection);
    let partial_archive = directory.path().join("partial-archive.db");
    export_store(&partial, &partial_archive).expect("export partial provenance");
    let partial_restored = directory.path().join("partial-restored.db");
    assert!(restore_source_layout(&partial_archive, &partial_restored).is_err());
    assert!(!partial_restored.exists());
    let partial_imported = directory.path().join("partial-imported.db");
    assert!(import_archive(&partial_archive, &partial_imported).is_err());
    assert!(!partial_imported.exists());
}

#[test]
fn migration_aggregate_import_remaps_completed_seal_run_and_replays() {
    let directory = temp_home().expect("directory");
    let source = directory.path().join("source.db");
    let source_seal = super::aggregate_lifecycle::populate_completed_aggregate(&source);
    {
        let source = Connection::open(&source).expect("predecessor source");
        let seals: i64 = source
            .query_row("SELECT COUNT(*) FROM work_completion_seals", [], |row| {
                row.get(0)
            })
            .expect("source seals");
        assert_eq!(
            seals, 1,
            "lifecycle helper builds one completed root, not parallel child seals"
        );
    }
    let archive = directory.path().join("archive.db");
    export_store(&source, &archive).expect("export");
    let output = directory.path().join("imported.db");
    let report = import_archive(&archive, &output).expect("aggregate completed import");
    assert_eq!(report.profile, MigrationProfile::AggregateRootV1);
    assert!(report.conversion.changed_objects > 0);
    assert!(report.reexpressed_completion_results > 0);
    for name in [
        "objects",
        "work_root_executions",
        "work_runs",
        "work_completion_seals",
        "work_operation_results",
    ] {
        assert!(
            report.dispositions.iter().any(|table| {
                table.name == name && table.disposition == TableDisposition::Transform
            }),
            "{name}"
        );
    }
    let store = crate::SqliteStore::open_unresolved(&output).expect("imported");
    assert!(store.verify_all().expect("doctor").is_healthy());
    let mapped: String = store
        .connection
        .query_row(
            "SELECT target_hash FROM migration_object_map WHERE source_hash = ?1",
            [source_seal.as_str()],
            |row| row.get(0),
        )
        .expect("mapped seal");
    assert_ne!(mapped, source_seal.as_str());
    let run_seal: String = store
        .connection
        .query_row("SELECT completion_seal_hash FROM work_runs", [], |row| {
            row.get(0)
        })
        .expect("run seal");
    let projection_seal: String = store
        .connection
        .query_row("SELECT seal_hash FROM work_completion_seals", [], |row| {
            row.get(0)
        })
        .expect("projection seal");
    assert_eq!(run_seal, mapped);
    assert_eq!(projection_seal, mapped);
    let imported = LocalWorkService::new(
        output.clone(),
        crate::ProjectId("migration-aggregate-lifecycle".into()),
        "agent".into(),
        crate::SessionId("migration-aggregate-session".into()),
        Some("protocol-test".into()),
    );
    let before = crate::storage::test_database_shape_snapshot(&store.connection).expect("before");
    let replayed = imported
        .work_complete(
            WorkCompleteInput {
                source_fingerprint: None,
                links: Vec::new(),
                link_basis: None,
                capture: Some(WorkCompletionCaptureInput {
                    summary: "delivered".into(),
                    refs: Vec::new(),
                }),
                evidence: Vec::new(),
                acceptance: None,
                note: None,
                idempotency_key: "aggregate-lifecycle-complete".into(),
            },
            chrono::DateTime::parse_from_rfc3339("2026-08-27T03:00:05Z")
                .expect("ts")
                .with_timezone(&chrono::Utc),
        )
        .expect("remapped complete_work replay");
    let WorkCompleteResult::Completed(receipt) = replayed else {
        panic!("expected completed replay");
    };
    assert_eq!(receipt.seal.as_str(), source_seal.as_str());
    assert_eq!(
        store
            .resolve_migrated_reference(&receipt.seal)
            .expect("resolve replayed seal")
            .as_str(),
        mapped
    );
    let first_receipt = serde_json::to_value(&receipt).expect("first ambient receipt");
    assert_core_complete_result_is_mapped_seal(&store.connection, &mapped);
    drop(imported);
    let after = crate::SqliteStore::open_unresolved(&output).expect("after replay");
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&after.connection).expect("after"),
        before
    );
    drop(after);
    let converted_archive = directory.path().join("converted-archive.db");
    export_store(&output, &converted_archive).expect("export converted");
    let current = directory.path().join("current.db");
    let current_report =
        import_archive(&converted_archive, &current).expect("current after populated aggregate");
    assert_eq!(current_report.profile, MigrationProfile::Current);
    assert_eq!(current_report.reexpressed_completion_results, 0);
    let converted_store = crate::SqliteStore::open_unresolved(&output).expect("converted");
    let current_store = crate::SqliteStore::open_unresolved(&current).expect("current");
    assert_same_durable(&converted_store.connection, &current_store.connection);
    let audits: i64 = current_store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM migration_reexpressed_results",
            [],
            |row| row.get(0),
        )
        .expect("preserved audits");
    assert!(audits > 0, "prior aggregate rewrites stay inspectable");
    drop(converted_store);
    let before_current = crate::storage::test_database_shape_snapshot(&current_store.connection)
        .expect("before current replay");
    drop(current_store);
    let current_service = LocalWorkService::new(
        current.clone(),
        crate::ProjectId("migration-aggregate-lifecycle".into()),
        "agent".into(),
        crate::SessionId("migration-aggregate-session".into()),
        Some("protocol-test".into()),
    );
    let replayed_current = current_service
        .work_complete(
            WorkCompleteInput {
                source_fingerprint: None,
                links: Vec::new(),
                link_basis: None,
                capture: Some(WorkCompletionCaptureInput {
                    summary: "delivered".into(),
                    refs: Vec::new(),
                }),
                evidence: Vec::new(),
                acceptance: None,
                note: None,
                idempotency_key: "aggregate-lifecycle-complete".into(),
            },
            chrono::DateTime::parse_from_rfc3339("2026-08-27T03:00:05Z")
                .expect("ts")
                .with_timezone(&chrono::Utc),
        )
        .expect("complete_work replay after current import");
    let WorkCompleteResult::Completed(current_receipt) = replayed_current else {
        panic!("expected completed replay after current import");
    };
    assert_eq!(current_receipt.seal.as_str(), source_seal.as_str());
    assert_eq!(
        crate::SqliteStore::open_unresolved(&current)
            .expect("resolve store")
            .resolve_migrated_reference(&current_receipt.seal)
            .expect("resolve current replayed seal")
            .as_str(),
        mapped
    );
    assert_eq!(
        serde_json::to_value(&current_receipt).expect("current ambient receipt"),
        first_receipt
    );
    assert_core_complete_result_is_mapped_seal(
        &crate::SqliteStore::open_unresolved(&current)
            .expect("core after current")
            .connection,
        &mapped,
    );
    drop(current_service);
    let after_current =
        crate::SqliteStore::open_unresolved(&current).expect("after current replay");
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&after_current.connection)
            .expect("after current"),
        before_current
    );
}
