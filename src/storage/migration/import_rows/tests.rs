use std::collections::HashMap;

use rusqlite::{Connection, params};

use super::CopyContext;
use crate::{CanonicalObject, RootExecution, storage::test_database_shape_snapshot};

// Exercises the actual row copier after canonical conversion, independently of
// full doctor. The existing phase fixture is not a complete executable store.
fn copy_case(table: &str, mutation: Option<&str>) -> Result<(), super::MigrationError> {
    let directory = crate::test_support::temp_home().expect("fixture");
    let source_path = directory.path().join("source.db");
    let (source, seal, _, _) = super::super::convert::tests::source(&source_path);
    let mut target = super::super::convert::tests::target();
    super::super::convert_aggregate_store_objects(&source_path, &mut target)
        .expect("canonical conversion");
    source.execute_batch("ALTER TABLE work_root_executions ADD COLUMN project_id TEXT;
        ALTER TABLE work_root_executions ADD COLUMN root_id TEXT;
        ALTER TABLE work_root_executions ADD COLUMN generation INTEGER;
        ALTER TABLE work_root_executions ADD COLUMN state TEXT;
        ALTER TABLE work_root_executions ADD COLUMN revision INTEGER;
        ALTER TABLE work_root_executions ADD COLUMN created_at_ms INTEGER;
        ALTER TABLE work_root_executions ADD COLUMN updated_at_ms INTEGER;
        CREATE TABLE work_completion_seals(seal_hash TEXT REFERENCES objects(object_hash), seal_json BLOB);").expect("source projection schema");
    let bytes: Vec<u8> = source
        .query_row("SELECT execution_json FROM work_root_executions", [], |r| {
            r.get(0)
        })
        .expect("root");
    let root: RootExecution = serde_json::from_slice(&bytes).expect("root");
    source.execute("UPDATE work_root_executions SET project_id=?1,root_id=?2,generation=?3,state='completed',revision=?4,created_at_ms=?5,updated_at_ms=?6", params![root.project_id.0,root.root_id.0.to_string(),root.generation,root.revision,root.created_at.timestamp_millis(),root.updated_at.timestamp_millis()]).expect("consistent columns");
    source.execute("INSERT INTO work_completion_seals SELECT object_hash,canonical_json FROM objects WHERE object_hash=?1",[seal.as_str()]).expect("consistent seal");
    if let Some(sql) = mutation {
        source.execute_batch(sql).expect("one corrupt projection");
    }
    let before_source = test_database_shape_snapshot(&source).expect("source before");
    let archive_path = directory.path().join("archive.db");
    let manifest = super::super::export_store(&source_path, &archive_path)
        .expect("self-consistent raw archive");
    let archive = Connection::open(&archive_path).expect("archive");
    target.execute_batch("CREATE TABLE work_completion_seals(seal_hash TEXT,seal_json BLOB);
        CREATE TABLE work_root_executions(root_execution_id TEXT,project_id TEXT,root_id TEXT,generation INTEGER,state TEXT,revision INTEGER,created_at_ms INTEGER,updated_at_ms INTEGER,header_json BLOB,head_hash TEXT);
        CREATE TABLE work_root_members(root_execution_id TEXT,member_hash TEXT,member_json BLOB);").expect("target projection schema");
    let mapping = target
        .prepare("SELECT source_hash,target_hash FROM migration_object_map")
        .expect("map query")
        .query_map([], |r| Ok((r.get::<_, String>(0)?, r.get::<_, String>(1)?)))
        .expect("map rows")
        .collect::<Result<HashMap<_, _>, _>>()
        .expect("map");
    let (hash, bytes): (String,Vec<u8>) = target.query_row("SELECT object_hash,canonical_json FROM objects WHERE object_kind=?1 ORDER BY json_extract(canonical_json,'$.sequence') DESC LIMIT 1",[crate::storage::work::ROOT_DELTA_KIND],|r|Ok((r.get(0)?,r.get(1)?))).expect("latest converted head");
    let delta: crate::domain::RootExecutionDelta = serde_json::from_slice(&bytes).expect("delta");
    let heads = HashMap::from([(delta.header.root_execution_id.0.to_string(), (hash, delta))]);
    let before_target = test_database_shape_snapshot(&target).expect("target before");
    let mut context = CopyContext {
        archive: &archive,
        target: &target,
        mapping,
        heads,
        reexpressed: 0,
    };
    let result = context.copy_table(
        manifest
            .tables
            .iter()
            .find(|t| t.name == table)
            .expect("table"),
        crate::storage::migration::TableDisposition::Transform,
    );
    assert_eq!(
        test_database_shape_snapshot(&source).expect("source after"),
        before_source
    );
    if result.is_err() {
        assert_eq!(
            test_database_shape_snapshot(&target).expect("target after"),
            before_target,
            "refusal before replacement writes"
        );
    }
    result
}

#[test]
fn migration_row_copy_refuses_inconsistent_source_seal_json() {
    copy_case("work_completion_seals", None).expect("valid source projection");
    let error = copy_case("work_completion_seals", Some("UPDATE work_completion_seals SET seal_json=CAST(json_set(seal_json,'$.actor.reason','corrupt projection only') AS BLOB)")).expect_err("must not silently repair source seal projection");
    assert!(
        error
            .to_string()
            .contains("source completion seal projection differs"),
        "{error}"
    );
}

#[test]
fn migration_row_copy_disposition_governs_object_foreign_key() {
    let directory = crate::test_support::temp_home().expect("fixture");
    let old = CanonicalObject::freeze(&serde_json::json!({"fixture":"cite-old"}))
        .expect("old fixture")
        .hash()
        .to_string();
    let new = CanonicalObject::freeze(&serde_json::json!({"fixture":"cite-new"}))
        .expect("new fixture")
        .hash()
        .to_string();
    assert_ne!(old, new);
    let source_path = directory.path().join("source.db");
    let source = Connection::open(&source_path).expect("source");
    source
        .execute_batch(
            "CREATE TABLE objects(object_hash TEXT PRIMARY KEY);
             CREATE TABLE cite(
                 object_hash TEXT NOT NULL REFERENCES objects(object_hash)
             );",
        )
        .expect("cite schema");
    source
        .execute("INSERT INTO objects(object_hash) VALUES (?1)", [&old])
        .expect("old object");
    source
        .execute("INSERT INTO cite(object_hash) VALUES (?1)", [&old])
        .expect("old cite");
    drop(source);
    let archive_path = directory.path().join("archive.db");
    let manifest = super::super::export_store(&source_path, &archive_path).expect("archive");
    let table = manifest
        .tables
        .iter()
        .find(|table| table.name == "cite")
        .expect("cite");
    assert!(
        table
            .foreign_keys
            .iter()
            .any(|foreign| foreign.target_table == "objects"),
        "discriminator requires an objects foreign key"
    );
    let archive = Connection::open(&archive_path).expect("archive");
    let mapping = HashMap::from([(old.clone(), new.clone())]);

    let unchanged_path = directory.path().join("unchanged.db");
    let unchanged = open_cite_target(&unchanged_path, &old, &new);
    let before_unchanged = test_database_shape_snapshot(&unchanged).expect("unchanged before");
    let mut unchanged_context = CopyContext {
        archive: &archive,
        target: &unchanged,
        mapping: mapping.clone(),
        heads: HashMap::new(),
        reexpressed: 0,
    };
    unchanged_context
        .copy_table(
            table,
            crate::storage::migration::TableDisposition::Unchanged,
        )
        .expect("unchanged copy");
    let stored_old: String = unchanged
        .query_row("SELECT object_hash FROM cite", [], |row| row.get(0))
        .expect("unchanged cell");
    assert_eq!(
        stored_old, old,
        "Unchanged must keep the source object hash"
    );
    assert_ne!(
        test_database_shape_snapshot(&unchanged).expect("unchanged after"),
        before_unchanged
    );

    let transformed_path = directory.path().join("transformed.db");
    let transformed = open_cite_target(&transformed_path, &old, &new);
    let mut transformed_context = CopyContext {
        archive: &archive,
        target: &transformed,
        mapping,
        heads: HashMap::new(),
        reexpressed: 0,
    };
    transformed_context
        .copy_table(
            table,
            crate::storage::migration::TableDisposition::Transform,
        )
        .expect("transform copy");
    let stored_new: String = transformed
        .query_row("SELECT object_hash FROM cite", [], |row| row.get(0))
        .expect("transformed cell");
    assert_eq!(
        stored_new, new,
        "Transform must apply the canonical mapping"
    );

    let refused_path = directory.path().join("refused.db");
    let refused = open_cite_target(&refused_path, &old, &new);
    let before_refused = test_database_shape_snapshot(&refused).expect("refused before");
    let mut refused_context = CopyContext {
        archive: &archive,
        target: &refused,
        mapping: HashMap::new(),
        heads: HashMap::new(),
        reexpressed: 0,
    };
    let error = refused_context
        .copy_table(
            table,
            crate::storage::migration::TableDisposition::Transform,
        )
        .expect_err("Transform without mappings must refuse");
    assert!(
        error
            .to_string()
            .contains("transform declared without canonical mappings for cite"),
        "{error}"
    );
    assert_eq!(
        test_database_shape_snapshot(&refused).expect("refused after"),
        before_refused,
        "refusal before target effects"
    );
    let refused_rows: i64 = refused
        .query_row("SELECT COUNT(*) FROM cite", [], |row| row.get(0))
        .expect("refused rows");
    assert_eq!(refused_rows, 0);
}

fn open_cite_target(path: &std::path::Path, old: &str, new: &str) -> Connection {
    let target = Connection::open(path).expect("target");
    target
        .execute_batch(
            "CREATE TABLE objects(object_hash TEXT PRIMARY KEY);
             CREATE TABLE cite(object_hash TEXT NOT NULL REFERENCES objects(object_hash));",
        )
        .expect("target schema");
    target
        .execute(
            "INSERT INTO objects(object_hash) VALUES (?1), (?2)",
            [old, new],
        )
        .expect("both addresses");
    target
}

#[test]
fn migration_row_copy_refuses_inconsistent_source_root_columns() {
    copy_case("work_root_executions", None).expect("valid source projection");
    for assignment in [
        "root_execution_id='different'",
        "project_id='different'",
        "root_id='different'",
        "generation=generation+1",
        "revision=revision+1",
        "state='active'",
        "created_at_ms=created_at_ms+1",
        "updated_at_ms=updated_at_ms+1",
    ] {
        let error = copy_case(
            "work_root_executions",
            Some(&format!("UPDATE work_root_executions SET {assignment}")),
        )
        .expect_err("must not silently repair source root columns");
        assert!(
            error
                .to_string()
                .contains("source root projection column differs"),
            "{assignment}: {error}"
        );
    }
}
