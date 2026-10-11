use super::*;

#[test]
fn task_sync_notes_retained_history_uses_structure_not_current_admission_policy() {
    let mut tree = note_candidate();
    let task = &mut tree.tasks[0];
    task.notes[0].body = "root=/private".into();
    task.notes[0].author.actor_id = "cache=%CUSTOM_HOME%".into();
    assert!(tree.validate().is_err());
    tree.validate_stored().unwrap();
    let documents = tree.documents().unwrap();
    assert!(TaskTree::from_documents(&documents).is_err());

    // Seed the immutable records an earlier admission policy could have written.
    let mut store = SqliteStore::open_in_memory().unwrap();
    let transaction = store.begin_work_mutation().unwrap();
    transaction
        .execute_batch("PRAGMA defer_foreign_keys = ON")
        .unwrap();
    for (task, snapshot) in tree.tasks.iter().zip(tree.snapshots().unwrap()) {
        insert_task(
            &transaction,
            &tree,
            task,
            snapshot,
            &actor("importer"),
            at(5),
        )
        .unwrap();
    }
    transaction.commit().unwrap();
    let before = state(&store);
    store.get_work_item(tree.tasks[0].id).unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
    let exported = store
        .export_task_tree_with_notes(&tree.project.project_id, "host-B")
        .unwrap();
    assert!(exported.omissions.counts.is_empty());
    assert_eq!(exported.tree.documents().unwrap(), documents);
    assert_eq!(state(&store), before);

    let fixture = crate::test_support::temp_home().unwrap();
    let path = fixture.path().join("retained-candidate");
    crate::task_sync::directory::write(&path, &exported.tree).unwrap();
    crate::task_sync::directory::write(&path, &exported.tree).unwrap();
    for (relative, bytes) in &documents {
        assert_eq!(std::fs::read(path.join(relative)).unwrap(), *bytes);
    }
    assert!(crate::task_sync::directory::read(&path).is_err());
    let mut target = SqliteStore::open_in_memory().unwrap();
    let target_before = state(&target);
    for dry in [true, false] {
        assert!(
            target
                .import_task_tree(
                    &tree.project.project_id,
                    &actor("importer"),
                    &exported.tree,
                    dry,
                    at(10)
                )
                .is_err()
        );
        assert_eq!(state(&target), target_before);
    }
    store
        .save_work_graph_snapshot(
            &tree.project.project_id,
            &actor("saver"),
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(10),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
}

#[test]
fn task_sync_notes_coordinator_author_and_prose_labels_survive_current_admission() {
    let mut tree = note_candidate();
    tree.tasks[0].notes[0].author.actor_id = "Engram::Fable".into();
    tree.tasks[0].notes[0].body =
        "Decision: ok Note: fixed Result: pass pass/fail greg/codex".into();
    tree.validate().unwrap();
    let mut store = SqliteStore::open_in_memory().unwrap();
    import(&mut store, &tree);
    assert_eq!(
        store
            .export_task_tree_with_notes(&tree.project.project_id, "host-B")
            .unwrap()
            .tree
            .documents()
            .unwrap(),
        tree.documents().unwrap()
    );
}

#[test]
fn task_sync_notes_current_restricted_labels_omit_retained_notes_without_changing_history() {
    let mut source = SqliteStore::open_in_memory().unwrap();
    let mut request = root_request("relabel-notes", "notes", 0);
    request.notes = vec!["Retained progress".into()];
    let item = source
        .create_work(&request, &crate::DevelopmentNoopRedactor)
        .unwrap();
    let original = source
        .export_task_tree_with_notes(&item.project_id, "host-A")
        .unwrap()
        .tree;
    let original_note = original.tasks[0].notes[0].clone();
    for label in ["secret", "restricted", "secret-ref"] {
        let mut store = SqliteStore::open_in_memory().unwrap();
        import(&mut store, &original);
        let imported = store.get_work_item(item.work_id).unwrap();
        let restricted = store
            .revise_work(
                &crate::domain::ReviseWorkRequest {
                    work_id: item.work_id,
                    expected_revision: imported.revision,
                    patch: crate::domain::WorkRevisionPatch {
                        add_labels: vec![label.into()],
                        ..Default::default()
                    },
                    authority: crate::domain::WorkPlanningAuthority::Project,
                    actor: actor("operator"),
                    idempotency_key: "restrict-notes".into(),
                    updated_at: at(6),
                },
                &crate::DevelopmentNoopRedactor,
            )
            .unwrap();
        let before = state(&store);
        let exported = store
            .export_task_tree_with_notes(&item.project_id, "host-B")
            .unwrap();
        assert_eq!(exported.omissions.counts["restricted task"], 1);
        assert_eq!(exported.tree.tasks[0].notes, [] as [SharedNote; 0]);
        exported.tree.validate().unwrap();
        let bytes = serde_json::to_string(&exported.tree).unwrap();
        assert!(!bytes.contains(original_note.id.as_str()));
        assert!(!bytes.contains(&original_note.body));
        assert_eq!(state(&store), before);
        assert!(store.verify_all().unwrap().is_healthy());
        let retained = store
            .work_read_snapshot(|store| {
                crate::storage::work::task_sync::notes::capture(
                    &store.connection,
                    &restricted,
                    None,
                    false,
                    true,
                    &mut crate::task_sync::notes::NoteOmissions::default(),
                )
            })
            .unwrap();
        assert_eq!(retained.0, vec![original_note.clone()]);
        assert!(retained.1);
        let mut target = SqliteStore::open_in_memory().unwrap();
        import(&mut target, &exported.tree);
        store
            .save_work_graph_snapshot(
                &item.project_id,
                &actor("saver"),
                None,
                crate::WorkGraphSnapshotDestinationKind::Stdout,
                at(7),
                &crate::DevelopmentNoopRedactor,
            )
            .unwrap();
        store
            .revise_work(
                &crate::domain::ReviseWorkRequest {
                    work_id: item.work_id,
                    expected_revision: restricted.revision,
                    patch: crate::domain::WorkRevisionPatch {
                        remove_labels: vec![label.into()],
                        ..Default::default()
                    },
                    authority: crate::domain::WorkPlanningAuthority::Project,
                    actor: actor("operator"),
                    idempotency_key: "unrestrict-notes".into(),
                    updated_at: at(8),
                },
                &crate::DevelopmentNoopRedactor,
            )
            .unwrap();
        let restored = store
            .export_task_tree_with_notes(&item.project_id, "host-C")
            .unwrap();
        assert!(restored.omissions.counts.is_empty());
        assert_eq!(restored.tree.tasks[0].notes, vec![original_note.clone()]);
        assert!(store.verify_all().unwrap().is_healthy());
    }
}
