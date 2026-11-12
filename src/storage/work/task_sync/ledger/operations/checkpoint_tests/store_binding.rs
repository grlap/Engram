use super::*;

fn audit_alias(alias: impl FnOnce(&std::path::Path) -> std::path::PathBuf) {
    let (_home, mut store, mut binding, _) = initial_fixture();
    binding.store.database = alias(&binding.store.database);
    let opened = std::path::Path::new(store.connection.path().unwrap());
    assert_ne!(
        crate::storage::store_path_text(opened),
        crate::storage::store_path_text(&binding.store.database),
        "the fixture must exercise distinct path spellings",
    );
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let request = audit_request(&store, &binding, &id, CustodySeedSelection::CustodyOnly);
    let receipt = store
        .audit_task_sync_custody(&request, &binding.actor, at(30))
        .unwrap();
    assert!(receipt.acknowledged && !receipt.repeated);
    let before = state(&store);
    let replay = store
        .audit_task_sync_custody(&request, &binding.actor, at(31))
        .unwrap();
    assert!(replay.repeated && replay.acknowledged);
    assert_eq!(replay.checkpoint, receipt.checkpoint);
    assert_eq!(state(&store), before);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn custody_audit_accepts_an_existing_store_path_alias() {
    audit_alias(|database| {
        database
            .parent()
            .unwrap()
            .join(".")
            .join(database.file_name().unwrap())
    });
}

#[cfg(windows)]
#[test]
fn custody_audit_accepts_windows_store_path_spellings() {
    // Both spellings name the real opened fixture database. The audit must
    // accept them through its public boundary, retaining its exact receipt.
    audit_alias(|database| {
        let canonical = std::fs::canonicalize(database).unwrap();
        let plain = crate::task_sync::git::plain_path(&canonical).unwrap();
        let alias = plain
            .parent()
            .unwrap()
            .join(".")
            .join(plain.file_name().unwrap());
        std::path::PathBuf::from(alias.to_str().unwrap().replace('\\', "/"))
    });
}

#[test]
fn custody_audit_refuses_a_different_store_and_project_without_writes() {
    let (home, mut store, binding, _) = initial_fixture();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let request = audit_request(&store, &binding, &id, CustodySeedSelection::CustodyOnly);
    let before = state(&store);
    let private_before =
        crate::task_sync::origin::private::read(&binding.store.database, &binding.project).unwrap();
    let mut other_project = request.clone();
    other_project.expected_local_cut.position.feed =
        FeedId::Project(crate::ProjectId("another-project".into()));
    let error = store
        .audit_task_sync_custody(&other_project, &binding.actor, at(30))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("custody audit project binding differs"),
        "{error}"
    );
    assert_eq!(state(&store), before);

    // A real SQLite copy contains the exact binding and feed but is a
    // different file. It must not inherit admission for the original path.
    let copy = home.path().join("another-store.db");
    store
        .connection
        .execute("VACUUM INTO ?1", [copy.to_str().unwrap()])
        .unwrap();
    let mut other_store = SqliteStore::open(&copy).unwrap();
    let copy_before = state(&other_store);
    let error = other_store
        .audit_task_sync_custody(&request, &binding.actor, at(30))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("custody audit store binding differs"),
        "{error}"
    );
    assert_eq!(state(&other_store), copy_before);
    assert_eq!(state(&store), before);
    assert_eq!(
        crate::task_sync::origin::private::read(&binding.store.database, &binding.project).unwrap(),
        private_before,
    );
    assert!(
        crate::task_sync::retention::audit::inventory(&binding.repository.git_directory)
            .unwrap()
            .events
            .is_empty()
    );
}
