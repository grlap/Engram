use std::fs;

use crate::test_support::temp_home;
use rusqlite::Connection;

use super::{
    ExportManifest, compare_export_to_source, export_store, restore_source_layout, verify_export,
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

fn aggregate_profile(source: &std::path::Path) {
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
    let report =
        super::import_aggregate_archive(&archive, &output).expect("complete bootstrap import");
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
    assert!(super::import_aggregate_archive(&archive, &output).is_err());
}
