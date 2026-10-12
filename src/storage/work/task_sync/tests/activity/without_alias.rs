use super::*;

#[test]
fn task_sync_activity_export_without_alias_omits_native_release_and_completion() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let released = store
        .create_work(
            &root_request("no-alias-activity", "released", 0),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let finished = store
        .create_work(
            &root_request("no-alias-activity", "finished", 0),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let project = released.project_id.clone();

    let first = claim(&mut store, &released, "first", "claim-released", 1, 100);
    let released = store.get_work_item(released.work_id).unwrap();
    release(&mut store, &released, &first, 2);

    let second = claim(&mut store, &finished, "second", "claim-finished", 3, 100);
    let finished = store.get_work_item(finished.work_id).unwrap();
    let proof = evidence(&mut store, &finished, &second, "second", "evidence", 4);
    checkpoint(
        &mut store,
        &finished,
        &second,
        "second",
        "checkpoint",
        5,
        std::slice::from_ref(&proof),
    );
    complete(
        &mut store, &finished, &second, "second", &proof, "complete", 6,
    )
    .unwrap();

    // With an alias, the same history exports one closed notice per task.
    let aliased = export(&store, &project, "machine-A");
    let ids = aliased.tasks.iter().map(|task| task.id).collect::<Vec<_>>();
    assert_eq!(ids.len(), 2);
    assert!(ids.contains(&released.work_id) && ids.contains(&finished.work_id));
    for (work, expected) in [
        (released.work_id, ActivityState::Released),
        (finished.work_id, ActivityState::Finished),
    ] {
        let task = aliased.tasks.iter().find(|task| task.id == work).unwrap();
        let entries = task.activity.values().flat_map(|map| map.values());
        assert_eq!(
            entries.map(|notice| notice.state).collect::<Vec<_>>(),
            [expected]
        );
    }

    let before = state(&store);
    let result = store
        .export_task_tree_with_options(
            &project,
            &TaskSyncExportOptions {
                include_notes: false,
                origin_machine: None,
            },
        )
        .unwrap();
    assert_eq!(state(&store), before);
    assert!(result.activity_omissions.counts.is_empty());
    assert_eq!(result.tree.tasks.len(), 2);
    assert!(
        result
            .tree
            .tasks
            .iter()
            .all(|task| task.activity.is_empty())
    );
    let documents = result.tree.documents().unwrap();
    let mut task_documents = 0;
    for (path, bytes) in &documents {
        if !path.starts_with("tasks/") {
            continue;
        }
        task_documents += 1;
        let value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        assert!(value.get("activity").is_none(), "{path} kept activity");
    }
    assert_eq!(task_documents, 2);
    assert_eq!(
        store
            .export_task_tree(&project)
            .unwrap()
            .documents()
            .unwrap(),
        documents
    );
}
