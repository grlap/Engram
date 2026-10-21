use super::*;

fn label_rows(store: &SqliteStore) -> Vec<(String, String)> {
    store
        .connection
        .prepare("SELECT work_id, label_key FROM work_item_labels ORDER BY work_id, label_key")
        .expect("prepare labels")
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("query labels")
        .collect::<Result<_, _>>()
        .expect("read labels")
}

#[test]
fn restore_refuses_orphan_label_collision_without_clearing_rows() {
    let mut source = SqliteStore::open_in_memory().expect("source");
    let project = ProjectId("snapshot-orphan-label".into());
    let item = create_root(
        &mut source,
        &project,
        "Restore labels",
        "orphan-label-source",
    );
    let snapshot = source
        .save_work_graph_snapshot(
            &project,
            &actor("orphan-label-save"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(2),
            &DevelopmentNoopRedactor,
        )
        .expect("save snapshot");
    let mut destination = SqliteStore::open_in_memory().expect("destination");
    // Model an inconsistent destination, without changing restore admission.
    destination
        .connection
        .pragma_update(None, "foreign_keys", false)
        .expect("allow orphan fixture");
    for key in ["snapshot", "stale-label"] {
        destination
            .connection
            .execute(
                "INSERT INTO work_item_labels (work_id, label_key) VALUES (?1, ?2)",
                rusqlite::params![item.work_id.0.to_string(), key],
            )
            .expect("seed orphan label");
    }
    destination
        .connection
        .pragma_update(None, "foreign_keys", true)
        .expect("restore foreign-key enforcement");
    let before = label_rows(&destination);
    assert_eq!(before.len(), 2);
    let object_count: i64 = destination
        .connection
        .query_row("SELECT COUNT(*) FROM objects", [], |row| row.get(0))
        .expect("count prior objects");

    let error = destination
        .load_work_graph_snapshot(
            &project,
            &actor("orphan-label-load"),
            &snapshot_bytes(&snapshot.document),
            false,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .expect_err("restore must not erase the conflicting orphan label");
    assert!(
        matches!(&error, StoreError::Sqlite(rusqlite::Error::SqliteFailure(failure, _))
            if failure.extended_code == rusqlite::ffi::SQLITE_CONSTRAINT_PRIMARYKEY),
        "{error:?}"
    );
    assert_eq!(label_rows(&destination), before);
    for table in ["work_items", "work_catalog_fts", "work_restored_records"] {
        let count: i64 = destination
            .connection
            .query_row(&format!("SELECT COUNT(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .expect("count rolled-back rows");
        assert_eq!(count, 0, "{table} was rolled back");
    }
    let after_objects: i64 = destination
        .connection
        .query_row("SELECT COUNT(*) FROM objects", [], |row| row.get(0))
        .expect("count remaining objects");
    assert_eq!(after_objects, object_count);
}
