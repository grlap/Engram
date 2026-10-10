use super::*;

#[test]
fn task_sync_origin_link_is_local_provenance_and_other_markers_ignore_it() {
    let tree = candidate();
    let mut store = SqliteStore::open_in_memory().unwrap();
    import(&mut store, &tree);
    let expected = tree.documents().unwrap();
    for task in &tree.tasks {
        let item = store.get_work_item(task.id).unwrap();
        let id = origin_reference(&item.created_by).unwrap();
        let origin: ImportOrigin =
            SqliteStore::get_canonical_object_on(&store.connection, &id, ORIGIN_KIND)
                .unwrap()
                .unwrap()
                .decode()
                .unwrap();
        assert_eq!(item.created_by, linked_actor(&origin.actor, &id));
        assert_eq!(origin.task.id, task.id);
        assert_eq!(item.created_by.actor_id, origin.actor.actor_id);
        assert_eq!(store.detached_work_origin(&item).unwrap(), None);
        assert!(!item.created_by.actor_defaulted());
        assert_eq!(item.created_by.attribution_context(), None);
        assert_eq!(crate::domain::status_note_role(&item.created_by), None);
        assert_eq!(item.created_by.retry_stable(), item.created_by);
        let link = item
            .created_by
            .provenance_chain
            .iter()
            .find(|link| link.source == ORIGIN_KIND)
            .unwrap();
        assert!(!crate::domain::is_non_holder_note_marker(link));
        assert!(!crate::domain::is_peer_child_proposal_marker(link));
    }
    let actual = store
        .export_task_tree(&tree.project.project_id)
        .unwrap()
        .documents()
        .unwrap();
    assert_eq!(actual, expected);
    for bytes in actual.values() {
        let value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        assert!(!serde_json::to_string(&value).unwrap().contains(ORIGIN_KIND));
        assert!(
            !serde_json::to_string(&value)
                .unwrap()
                .contains("provenance_chain")
        );
    }
}

#[test]
fn task_sync_primary_key_lookup_refuses_wrong_missing_and_duplicate_links() {
    let tree = candidate();
    let mut store = SqliteStore::open_in_memory().unwrap();
    import(&mut store, &tree);
    let item = store.get_work_item(tree.tasks[0].id).unwrap();
    let other = store.get_work_item(tree.tasks[1].id).unwrap();
    let original = CanonicalObject::freeze(&item).unwrap();
    for case in ["other", "missing", "duplicate", "relation"] {
        let mut tampered = item.clone();
        let link = tampered
            .created_by
            .provenance_chain
            .iter_mut()
            .find(|link| link.source == ORIGIN_KIND)
            .unwrap();
        match case {
            "other" => {
                link.reference = Some(origin_reference(&other.created_by).unwrap().as_str().into());
            }
            "missing" => {
                link.reference = Some(
                    CanonicalObject::mint(&serde_json::json!({}))
                        .unwrap()
                        .key()
                        .as_str()
                        .into(),
                );
            }
            "relation" => link.relation = crate::domain::ProvenanceRelation::RelayedBy,
            _ => {
                let duplicate = link.clone();
                tampered.created_by.provenance_chain.push(duplicate);
            }
        }
        store
            .connection
            .execute(
                "UPDATE work_items SET item_json=?1 WHERE work_id=?2",
                params![
                    CanonicalObject::freeze(&tampered).unwrap().bytes(),
                    item.work_id.0.to_string()
                ],
            )
            .unwrap();
        assert!(store.get_work_item(item.work_id).is_err(), "{case}");
        assert!(!store.verify_all().unwrap().is_healthy(), "{case}");
        store
            .connection
            .execute(
                "UPDATE work_items SET item_json=?1 WHERE work_id=?2",
                params![original.bytes(), item.work_id.0.to_string()],
            )
            .unwrap();
    }
    // The exact object-id probe uses SQLite's existing primary-key search.
    let plan: String = store.connection.query_row(
        "EXPLAIN QUERY PLAN SELECT canonical_json FROM objects WHERE object_id=?1 AND object_kind=?2",
        params![origin_reference(&item.created_by).unwrap().as_str(), ORIGIN_KIND], |row| row.get(3)).unwrap();
    assert!(plan.contains("SEARCH objects"), "{plan}");
    assert!(!plan.contains("SCAN objects"), "{plan}");
}

#[test]
fn task_sync_global_audit_refuses_duplicate_and_incomplete_origin_graphs_on_import() {
    for case in ["duplicate", "closure"] {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let tree = candidate();
        import(&mut store, &tree);
        if case == "duplicate" {
            let id = origin_reference(&store.get_work_item(tree.tasks[0].id).unwrap().created_by)
                .unwrap();
            let origin = SqliteStore::get_canonical_object_on(&store.connection, &id, ORIGIN_KIND)
                .unwrap()
                .unwrap();
            let duplicate =
                CanonicalObject::mint(&origin.decode::<ImportOrigin>().unwrap()).unwrap();
            SqliteStore::insert_object(&store.connection, ORIGIN_KIND, &duplicate).unwrap();
        } else {
            let child = tree
                .tasks
                .iter()
                .find(|task| task.parent_id.is_some())
                .unwrap();
            let parent = store.get_work_item(child.parent_id.unwrap()).unwrap();
            let id = origin_reference(&parent.created_by).unwrap();
            store
                .connection
                .execute("DELETE FROM objects WHERE object_id=?1", [id.as_str()])
                .unwrap();
        }
        let before = state(&store);
        assert!(!store.verify_all().unwrap().is_healthy(), "{case}");
        let mut fresh = candidate();
        fresh.project.project_id = ProjectId("fresh-project".into());
        for dry_run in [true, false] {
            let error = store
                .import_task_tree(
                    &fresh.project.project_id,
                    &actor("importer"),
                    &fresh,
                    dry_run,
                    at(6),
                )
                .unwrap_err();
            assert!(
                error.to_string().contains("origin audit failed"),
                "{case}: {error}"
            );
            assert_eq!(state(&store), before);
        }
    }
}

#[test]
fn task_sync_misordered_scope_drop_cannot_remove_a_fresh_transaction_scope() {
    let mut first = SqliteStore::open_in_memory().unwrap();
    let tree = candidate();
    import(&mut first, &tree);
    let second = SqliteStore::open_in_memory().unwrap();
    first.connection.execute_batch("BEGIN").unwrap();
    second.connection.execute_batch("BEGIN").unwrap();
    let outer = origin::enter_origin_scope(&first.connection).unwrap();
    let stale = origin::enter_origin_scope(&second.connection).unwrap();
    drop(outer);
    first.connection.execute_batch("ROLLBACK; BEGIN").unwrap();
    let fresh = origin::enter_origin_scope(&first.connection).unwrap();
    origin::DECODES.with(|count| count.set(0));
    first.get_work_item(tree.tasks[0].id).unwrap();
    drop(stale);
    first.get_work_item(tree.tasks[0].id).unwrap();
    assert_eq!(origin::DECODES.with(std::cell::Cell::get), 1);
    drop(fresh);
    first.connection.execute_batch("ROLLBACK").unwrap();
    second.connection.execute_batch("ROLLBACK").unwrap();
    // Requiring a real transaction stays enforced after misordered cleanup.
    assert!(origin::enter_origin_scope(&first.connection).is_err());
}
