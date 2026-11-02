use super::*;

#[test]
fn invalid_import_is_refused_before_contended_writer_admission() {
    let directory = crate::test_support::temp_home().unwrap();
    let path = directory.path().join("import-contention.db");
    let mut store = SqliteStore::open(&path).unwrap();
    store
        .connection
        .busy_timeout(std::time::Duration::ZERO)
        .unwrap();
    let blocker =
        crate::storage::open_sqlite_file(&path, rusqlite::OpenFlags::SQLITE_OPEN_READ_WRITE)
            .unwrap();
    blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
    let tree = candidate();
    let mut bad_tree = tree.clone();
    bad_tree.project.format = "unsupported".into();
    let mut bad_actor = actor("importer");
    bad_actor.actor_id.clear();
    for (project, author, candidate) in [
        (tree.project.project_id.clone(), actor("importer"), bad_tree),
        (
            ProjectId("other-project".into()),
            actor("importer"),
            tree.clone(),
        ),
        (tree.project.project_id.clone(), bad_actor, tree.clone()),
    ] {
        let error = store
            .import_task_tree(&project, &author, &candidate, false, at(5))
            .unwrap_err();
        assert!(
            !matches!(error, StoreError::WorkWriterAdmissionRefused { .. }),
            "invalid input must retain its validation refusal: {error}"
        );
    }
    // Prove the competing write is real: valid input reaches admission and fails.
    assert!(matches!(
        store.import_task_tree(
            &tree.project.project_id,
            &actor("importer"),
            &tree,
            false,
            at(5),
        ),
        Err(StoreError::WorkWriterAdmissionRefused { .. })
    ));
    blocker.execute_batch("ROLLBACK").unwrap();
    assert_eq!(
        store
            .export_task_tree(&tree.project.project_id)
            .unwrap()
            .tasks,
        [] as [crate::task_sync::TaskDocument; 0]
    );
}

#[test]
fn staged_import_is_rolled_back_when_the_outer_operation_fails() {
    let tree = candidate();
    let mut store = SqliteStore::open_in_memory().unwrap();
    let before = state(&store);
    let result = (|| -> Result<(), StoreError> {
        let transaction = store.begin_work_mutation()?;
        let imported = import_task_tree_on(
            &transaction,
            &tree.project.project_id,
            &actor("importer"),
            &tree,
            false,
            at(5),
        )?;
        assert!(imported.changed);
        assert!(query::load_work_item(&transaction, tree.tasks[0].id).is_ok());
        // Failure after all import writes must still leave no partial graph.
        Err(invalid("injected failure before the exchange receipt"))
    })();
    assert!(result.unwrap_err().to_string().contains("injected failure"));
    assert_eq!(state(&store), before);
    assert!(store.verify_all().unwrap().is_healthy());
}
