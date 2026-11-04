use super::*;

#[test]
fn restore_initializes_catalog_once_after_relations_without_fts_deletes() {
    let directory = crate::test_support::temp_home().expect("tempdir");
    let project = ProjectId("snapshot-catalog-insert-only".into());
    let mut source =
        SqliteStore::open(directory.path().join("catalog-source.db")).expect("source store");
    let item = create_root(
        &mut source,
        &project,
        "Distinctive restored title fragment",
        "catalog-restore-item",
    );
    source
        .add_work_blocker(
            &AddWorkBlockerRequest {
                work_id: item.work_id,
                expected_work_revision: item.revision,
                kind: WorkBlockerKind::Manual,
                detail: "distinctive relation blocker fragment".into(),
                authority: WorkPlanningAuthority::Project,
                actor: actor("catalog-blocker"),
                idempotency_key: "catalog-blocker".into(),
                blocked_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("add blocker");
    let snapshot = source
        .save_work_graph_snapshot(
            &project,
            &actor("catalog-save"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .expect("save snapshot");

    let mut destination = SqliteStore::open_in_memory().expect("destination");
    let other_project = ProjectId("snapshot-catalog-other-project".into());
    let other_item = create_root(
        &mut destination,
        &other_project,
        "Existing other-project catalog row",
        "catalog-other-item",
    );
    let other_fts_before: String = destination
        .connection
        .query_row(
            "SELECT search_text FROM work_catalog_fts WHERE work_id = ?1",
            [other_item.work_id.0.to_string()],
            |row| row.get(0),
        )
        .expect("original other-project FTS text");
    destination
        .load_work_graph_snapshot(
            &project,
            &actor("catalog-dry-run"),
            &snapshot_bytes(&snapshot.document),
            true,
            at(4),
            &DevelopmentNoopRedactor,
        )
        .expect("unrelated project FTS row does not collide in dry run");
    crate::storage::take_work_catalog_fts_delete_statements();
    destination
        .load_work_graph_snapshot(
            &project,
            &actor("catalog-load"),
            &snapshot_bytes(&snapshot.document),
            false,
            at(4),
            &DevelopmentNoopRedactor,
        )
        .expect("load snapshot");

    assert_eq!(
        crate::storage::take_work_catalog_fts_delete_statements(),
        0,
        "restoring new items should not issue FTS deletes"
    );
    let item_count = i64::try_from(snapshot.document.body.items.len()).expect("small item count");
    let project_fts_count: i64 = destination
        .connection
        .query_row(
            "SELECT COUNT(*) FROM work_catalog_fts
             WHERE work_id IN (SELECT work_id FROM work_items WHERE project_id = ?1)",
            [&project.0],
            |row| row.get(0),
        )
        .expect("count FTS rows");
    assert_eq!(
        project_fts_count, item_count,
        "one FTS row per restored item"
    );
    let other_fts_count: i64 = destination
        .connection
        .query_row(
            "SELECT COUNT(*) FROM work_catalog_fts WHERE work_id = ?1",
            [other_item.work_id.0.to_string()],
            |row| row.get(0),
        )
        .expect("count other-project FTS row");
    assert_eq!(other_fts_count, 1, "other-project row remains intact");
    let other_fts_after: String = destination
        .connection
        .query_row(
            "SELECT search_text FROM work_catalog_fts WHERE work_id = ?1",
            [other_item.work_id.0.to_string()],
            |row| row.get(0),
        )
        .expect("preserved other-project FTS text");
    assert_eq!(other_fts_after, other_fts_before);
    assert_eq!(
        destination.get_work_item(other_item.work_id).unwrap(),
        other_item
    );
    let page = destination
        .query_work_catalog(
            &project,
            at(5),
            &crate::WorkCatalogQuery {
                search: Some("distinctive restored title fragment".into()),
                limit: 10,
                ..crate::WorkCatalogQuery::default()
            },
        )
        .expect("search restored title");
    assert_eq!(page.items.len(), 1);
    assert_eq!(page.items[0].work.work_id, item.work_id);
    let blocker_page = destination
        .query_work_catalog(
            &project,
            at(5),
            &crate::WorkCatalogQuery {
                search: Some("distinctive relation blocker fragment".into()),
                limit: 10,
                ..crate::WorkCatalogQuery::default()
            },
        )
        .expect("search restored blocker detail");
    assert_eq!(blocker_page.items.len(), 1);
    assert_eq!(blocker_page.items[0].work.work_id, item.work_id);
    let search_text: String = destination
        .connection
        .query_row(
            "SELECT search_text FROM work_catalog_fts WHERE work_id = ?1",
            [item.work_id.0.to_string()],
            |row| row.get(0),
        )
        .expect("read restored FTS text");
    assert!(search_text.contains("distinctive relation blocker fragment"));
}

#[test]
fn restore_refuses_an_existing_fts_row_for_an_incoming_work_id() {
    let directory = crate::test_support::temp_home().expect("tempdir");
    let project = ProjectId("snapshot-catalog-collision".into());
    let mut source =
        SqliteStore::open(directory.path().join("collision-source.db")).expect("source store");
    let item = create_root(
        &mut source,
        &project,
        "Incoming catalog collision",
        "catalog-collision-item",
    );
    let snapshot = source
        .save_work_graph_snapshot(
            &project,
            &actor("collision-save"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .expect("save snapshot");
    let mut destination = SqliteStore::open_in_memory().expect("destination");
    destination
        .connection
        .execute(
            "INSERT INTO work_catalog_fts (work_id, search_text) VALUES (?1, 'orphaned catalog row')",
            [item.work_id.0.to_string()],
        )
        .expect("seed conflicting orphan row");
    let before = crate::storage::test_database_shape_snapshot(&destination.connection)
        .expect("destination before refusal");

    let dry_run_error = destination
        .load_work_graph_snapshot(
            &project,
            &actor("collision-dry-run"),
            &snapshot_bytes(&snapshot.document),
            true,
            at(4),
            &DevelopmentNoopRedactor,
        )
        .expect_err("dry run must reject an incoming id already present in FTS");
    assert!(
        matches!(dry_run_error, StoreError::InvalidWorkProjection(ref detail)
            if detail.contains("destination work_catalog_fts")
                && detail.contains("orphan row")
                && detail.contains("engram doctor --repair-projections")),
        "{dry_run_error:?}"
    );
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&destination.connection).unwrap(),
        before,
        "dry-run refusal leaves all destination rows and schema unchanged"
    );

    let error = destination
        .load_work_graph_snapshot(
            &project,
            &actor("collision-load"),
            &snapshot_bytes(&snapshot.document),
            false,
            at(4),
            &DevelopmentNoopRedactor,
        )
        .expect_err("must reject an incoming id already present in FTS");
    assert!(
        matches!(error, StoreError::InvalidWorkProjection(ref detail)
            if detail.contains("destination work_catalog_fts")
                && detail.contains("orphan row")
                && detail.contains("engram doctor --repair-projections")),
        "{error:?}"
    );
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&destination.connection).unwrap(),
        before,
        "real-load refusal leaves all destination rows and schema unchanged"
    );
    let work_count: i64 = destination
        .connection
        .query_row("SELECT COUNT(*) FROM work_items", [], |row| row.get(0))
        .expect("count work items");
    let fts_count: i64 = destination
        .connection
        .query_row("SELECT COUNT(*) FROM work_catalog_fts", [], |row| {
            row.get(0)
        })
        .expect("count FTS rows");
    assert_eq!(work_count, 0, "refusal leaves catalog empty");
    assert_eq!(fts_count, 1, "refusal preserves the preexisting row");
}

#[test]
fn restore_keeps_cross_project_work_id_collisions_as_snapshot_conflicts() {
    let mut destination = SqliteStore::open_in_memory().expect("destination");
    let existing_project = ProjectId("snapshot-existing-project".into());
    let incoming_project = ProjectId("snapshot-incoming-project".into());
    create_root(
        &mut destination,
        &existing_project,
        "Existing item retains its identity",
        "cross-project-existing-root",
    );
    let mut snapshot = destination
        .save_work_graph_snapshot(
            &existing_project,
            &actor("cross-project-save"),
            None,
            WorkGraphSnapshotDestinationKind::Stdout,
            at(3),
            &DevelopmentNoopRedactor,
        )
        .expect("save native-only snapshot");
    snapshot.document.body.summary.project_id = incoming_project.clone();
    rebind_snapshot_body(&mut snapshot.document);
    let bytes = snapshot_bytes(&snapshot.document);

    // Prove the rebinding made a valid snapshot for the empty target project;
    // its refusal below must be the existing destination identity collision.
    let mut empty = SqliteStore::open_in_memory().expect("empty control destination");
    empty
        .load_work_graph_snapshot(
            &incoming_project,
            &actor("cross-project-control"),
            &bytes,
            true,
            at(4),
            &DevelopmentNoopRedactor,
        )
        .expect("snapshot is valid against an empty destination");
    let before = crate::storage::test_database_shape_snapshot(&destination.connection)
        .expect("destination before refusal");
    for dry_run in [true, false] {
        let error = destination
            .load_work_graph_snapshot(
                &incoming_project,
                &actor("cross-project-collision"),
                &bytes,
                dry_run,
                at(4),
                &DevelopmentNoopRedactor,
            )
            .expect_err("an existing global work ID is a snapshot conflict");
        assert!(
            matches!(error, StoreError::InvalidGraphSnapshot(ref detail)
                if detail.contains("already has a catalog FTS row")
                    && !detail.contains("repair-projections")),
            "{error:?}"
        );
        assert_eq!(
            crate::storage::test_database_shape_snapshot(&destination.connection).unwrap(),
            before,
            "snapshot-conflict refusal leaves the other project unchanged"
        );
    }
}
