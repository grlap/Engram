use rusqlite::{Connection, params};

#[test]
fn migration_original_replay_rows_require_exact_per_key_audits() {
    let source = Connection::open_in_memory().expect("source fixture");
    source.execute_batch("CREATE TABLE work_operation_results(operation TEXT,idempotency_key TEXT,request_hash TEXT,result_json BLOB)").expect("table");
    let original = br#"{ "historical": true }"#;
    source
        .execute(
            "INSERT INTO work_operation_results VALUES ('complete_work','key','intent',?1)",
            [original.as_slice()],
        )
        .expect("original response");
    let schema = super::super::export::schema_on(&source).expect("schema");
    let manifest = super::super::export::export_on(&source, None, schema).expect("manifest");
    let table = manifest
        .tables
        .iter()
        .find(|table| table.name == "work_operation_results")
        .expect("table");
    let cells = source
        .query_row("SELECT rowid,* FROM work_operation_results", [], |row| {
            Ok(super::rows::encode(row, 5).expect("frame"))
        })
        .expect("row");
    let target = Connection::open_in_memory().expect("audit fixture");
    target.execute_batch("CREATE TABLE migration_reexpressed_results(operation TEXT,idempotency_key TEXT,source_result BLOB)").expect("audit table");
    assert!(
        super::verify_original_completion(&target, table, &cells, 5).is_err(),
        "missing audit must refuse"
    );
    target
        .execute(
            "INSERT INTO migration_reexpressed_results VALUES ('complete_work','key',?1)",
            [original.as_slice()],
        )
        .expect("exact audit");
    assert!(
        super::verify_original_completion(&target, table, &cells, 5).expect("exact original bytes")
    );
    target
        .execute(
            "UPDATE migration_reexpressed_results SET source_result=?1",
            params![br#"{"historical":true}"#.as_slice()],
        )
        .expect("same JSON but different bytes");
    assert!(
        super::verify_original_completion(&target, table, &cells, 5).is_err(),
        "semantic equality cannot replace original response bytes"
    );
}
