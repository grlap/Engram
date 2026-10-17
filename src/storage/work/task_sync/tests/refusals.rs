use super::*;
use crate::task_sync::{TaskSyncExportOptions, notes::NoteOmissions};

#[test]
fn task_sync_import_record_id_pin_refuses_without_writes_but_command_fingerprint_imports() {
    let mut target = SqliteStore::open_in_memory().unwrap();
    let object = CanonicalObject::mint(&serde_json::json!({"unrelated": "record"})).unwrap();
    SqliteStore::insert_object(&target.connection, "test_record", &object).unwrap();
    let mut tree = candidate();
    tree.tasks[0].acceptance_bindings[0]
        .requirement
        .check_fingerprint = Some(object.key().clone());
    let before = state(&target);
    for dry_run in [true, false] {
        let error = target
            .import_task_tree(
                &tree.project.project_id,
                &actor("importer"),
                &tree,
                dry_run,
                at(5),
            )
            .unwrap_err();
        assert_eq!(refusal_code(&error), "task_sync_invalid_graph");
        assert!(
            error.to_string().contains("names a stored record"),
            "{error}"
        );
        assert_eq!(state(&target), before);
    }
    let command = ObjectId::from_canonical_bytes(b"cargo test --lib refusal_boundary");
    assert!(target.get::<serde_json::Value>(&command).unwrap().is_none());
    tree.tasks[0].acceptance_bindings[0]
        .requirement
        .check_fingerprint = Some(command);
    assert!(import(&mut target, &tree).changed);
    assert!(!import(&mut target, &tree).changed);
}

#[test]
fn task_sync_export_activity_conflict_producer_preserves_the_retained_origin() {
    let mut source = SqliteStore::open_in_memory().unwrap();
    let item = source
        .create_work(
            &root_request("activity-conflict", "create", 0),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    claim(&mut source, &item, "worker", "claim", 1, 100);
    let item = source.get_work_item(item.work_id).unwrap();
    let original = source
        .export_task_tree_with_options(
            &item.project_id,
            &TaskSyncExportOptions {
                include_notes: false,
                origin_machine: Some("machine-A".into()),
            },
        )
        .unwrap()
        .tree;
    let events =
        query::canonical_work_event_records_for_item(&source.connection, item.work_id).unwrap();
    for changed in [false, true] {
        let mut tree = original.clone();
        if changed {
            tree.tasks[0]
                .activity
                .get_mut("machine-A")
                .unwrap()
                .values_mut()
                .next()
                .unwrap()
                .actor
                .actor_id = "other-worker".into();
        }
        let mut target = SqliteStore::open_in_memory().unwrap();
        import(&mut target, &tree);
        let restored = target.get_work_item(item.work_id).unwrap();
        let before = state(&target);
        // Genuine source events collide with retained import notices here. Normal
        // exports do not let a fixture choose a future randomly minted event id.
        let result = target.work_read_snapshot(|store| {
            with_origin_scope(&store.connection, || {
                super::super::activity::capture(
                    &store.connection,
                    &restored,
                    &events,
                    Some("machine-A"),
                    false,
                    &mut NoteOmissions::default(),
                )
            })
        });
        if changed {
            let error = result.unwrap_err();
            assert_eq!(refusal_code(&error), "task_sync_divergent_planning");
            assert!(
                error
                    .to_string()
                    .contains("native activity conflicts with retained origin entry")
            );
        } else {
            assert_eq!(result.unwrap(), original.tasks[0].activity);
        }
        let separate = target
            .work_read_snapshot(|store| {
                with_origin_scope(&store.connection, || {
                    super::super::activity::capture(
                        &store.connection,
                        &restored,
                        &events,
                        Some("machine-B"),
                        false,
                        &mut NoteOmissions::default(),
                    )
                })
            })
            .unwrap();
        assert_eq!(separate["machine-A"], tree.tasks[0].activity["machine-A"]);
        assert_eq!(separate["machine-B"].len(), 1);
        assert_eq!(state(&target), before);
    }
}
