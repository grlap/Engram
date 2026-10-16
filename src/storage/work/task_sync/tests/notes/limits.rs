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
        let error = target
            .import_task_tree(&item.project_id, &actor("importer"), &oversized, dry, at(5))
            .unwrap_err();
        // Import classifies the oversized note as an invalid candidate; the
        // native refusal, its byte counts and its wire code stay inside.
        assert_eq!(
            crate::store_error_value(&error)["error"]["code"],
            "task_sync_invalid_graph"
        );
        let StoreError::TaskSyncRefused { inner, .. } = &error else {
            panic!("an unclassified import refusal: {error:?}");
        };
        assert!(matches!(
            **inner,
            StoreError::WorkNoteTooLarge { bytes, limit: actual }
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

#[test]
fn task_sync_export_classifies_a_retained_oversized_native_note() {
    let limit = crate::domain::MAX_WORK_NOTE_BYTES;
    let marker = "retained-progress-marker";
    let mut store = SqliteStore::open_in_memory().unwrap();
    let mut request = root_request("note-retained", "notes", 0);
    request.notes = vec![marker.into()];
    let item = store
        .create_work(&request, &crate::DevelopmentNoopRedactor)
        .unwrap();
    // A store may retain a note written before today's limit: rewrite the
    // stored note body past it.
    let rows = store
        .connection
        .prepare(
            "SELECT object_id, canonical_json FROM objects
             WHERE CAST(canonical_json AS TEXT) LIKE '%' || ?1 || '%'",
        )
        .unwrap()
        .query_map([marker], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_ne!(rows.len(), 0, "the note body is stored");
    let oversized = "x".repeat(limit + 1);
    for (id, bytes) in rows {
        let text = String::from_utf8(bytes)
            .unwrap()
            .replace(marker, &oversized);
        store
            .connection
            .execute(
                "UPDATE objects SET canonical_json = ?1 WHERE object_id = ?2",
                params![text.into_bytes(), id],
            )
            .unwrap();
    }
    let error = store
        .export_task_tree_with_notes(&item.project_id, "host-A")
        .unwrap_err();
    assert_eq!(
        crate::store_error_value(&error)["error"]["code"],
        "task_sync_invalid_graph",
        "{error}"
    );
    let StoreError::TaskSyncRefused { inner, .. } = &error else {
        panic!("an unclassified export refusal: {error:?}");
    };
    assert!(matches!(
        **inner,
        StoreError::WorkNoteTooLarge { bytes, limit: actual }
            if bytes == limit + 1 && actual == limit
    ));
}
