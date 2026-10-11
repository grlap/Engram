use super::*;

#[test]
fn task_sync_shared_note_size_matches_native_utf8_write_limit() {
    let limit = crate::domain::MAX_WORK_NOTE_BYTES;
    let body = "Ω".repeat(limit / "Ω".len());
    assert_eq!(body.len(), limit);
    let mut source = SqliteStore::open_in_memory().unwrap();
    let mut request = root_request("note-size", "notes", 0);
    request.notes = vec![body.clone()];
    let item = source
        .create_work(&request, &crate::DevelopmentNoopRedactor)
        .unwrap();
    let exported = source
        .export_task_tree_with_notes(&item.project_id, "host-A")
        .unwrap();
    assert_eq!(exported.tree.tasks[0].notes[0].body, body);
    let documents = exported.tree.documents().unwrap();
    let decoded = TaskTree::from_documents(&documents).unwrap();
    let mut target = SqliteStore::open_in_memory().unwrap();
    import(&mut target, &decoded);
    assert_eq!(
        target
            .export_task_tree_with_notes(&item.project_id, "host-B")
            .unwrap()
            .tree
            .documents()
            .unwrap(),
        documents
    );

    let mut oversized = exported.tree.clone();
    oversized.tasks[0].notes[0].body.push('x');
    assert!(matches!(
        oversized.documents(),
        Err(StoreError::WorkNoteTooLarge { bytes, limit: actual })
            if bytes == limit + 1 && actual == limit
    ));
    let mut target = SqliteStore::open_in_memory().unwrap();
    let before = state(&target);
    for dry in [true, false] {
        assert!(matches!(
            target.import_task_tree(
                &item.project_id,
                &actor("importer"),
                &oversized,
                dry,
                at(5)
            ),
            Err(StoreError::WorkNoteTooLarge { bytes, limit: actual })
                if bytes == limit + 1 && actual == limit
        ));
        assert_eq!(state(&target), before);
    }
    request.idempotency_key = "oversized-native".into();
    request.notes[0].push('x');
    assert!(matches!(
        source.create_work(&request, &crate::DevelopmentNoopRedactor),
        Err(StoreError::WorkNoteTooLarge { bytes, limit: actual })
            if bytes == limit + 1 && actual == limit
    ));
    assert!(source.verify_all().unwrap().is_healthy());
}
