use super::*;

fn retain_late_note_in_other_project(
    store: &mut SqliteStore,
    template: &crate::task_sync::TaskDocument,
) -> crate::task_sync::notes::SharedNote {
    let project = crate::ProjectId("other-project".into());
    let mut task = template.clone();
    task.id = crate::WorkId(uuid::Uuid::new_v4());
    task.external_ref = None;
    task.notes.clear();
    task.activity.clear();
    let mut tree = crate::task_sync::TaskTree {
        project: crate::task_sync::ProjectDocument {
            format: crate::task_sync::FORMAT.into(),
            project_id: project.clone(),
            notes_included: true,
        },
        tasks: vec![task],
    };
    store
        .import_task_tree(&project, &actor("receiver"), &tree, false, at(31))
        .unwrap();
    let profile = crate::task_sync::progress::ExportProfile {
        include_notes: true,
        origin_machine: Some("fixture-host".into()),
    };
    let basis = store
        .export_task_sync_folder(&project, &profile)
        .unwrap()
        .export_basis;
    let late = crate::task_sync::notes::SharedNote {
        id: ObjectId::mint(),
        kind: crate::task_sync::notes::SharedNoteKind::Generic,
        body: "Late note retained by its original project".into(),
        author: crate::task_sync::SharedActor::from_actor(&actor("peer")),
        recorded_at: at(32),
        origin_machine: "remote".into(),
    };
    tree.tasks[0].notes.push(late.clone());
    let result = store
        .apply_task_sync_folder(
            crate::storage::TaskSyncFolderApply {
                project: &project,
                actor: &actor("receiver"),
                tree: &tree,
                export_basis: &basis,
                profile: &profile,
                complete: true,
                now: at(33),
            },
            "retain-late-note",
        )
        .unwrap();
    assert!(
        result
            .tasks
            .iter()
            .all(|task| { task.status == crate::storage::TaskSyncTaskStatus::Applied })
    );
    late
}

#[test]
fn shared_updates_strict_addition_refuses_late_identity_before_deferring() {
    let mut tree = claimable();
    tree.project.notes_included = true;
    tree.tasks
        .retain(|task| task.parent_id.is_none() && task.lifecycle == crate::WorkLifecycle::Open);
    tree.tasks.truncate(1);
    let governing = tree.tasks[0].id;
    let mut f = Fixture::with_store_changes(
        &tree,
        |store, tree| {
            let mut task = tree.tasks[0].clone();
            let mut late = retain_late_note_in_other_project(store, &task);
            late.body = "Different prose claiming the retained original id".into();
            let mut added = task.clone();
            added.id = crate::WorkId(uuid::Uuid::new_v4());
            added.external_ref = None;
            added.notes = vec![late];
            task.title.push_str(" incoming edit");
            BTreeMap::from([(task.id, Some(task)), (added.id, Some(added))])
        },
        |store, _| {
            claim_local(store, governing, 34);
        },
        |_, _, _| {},
    );
    let before = f.store.get_work_item(governing).unwrap();
    let objects: i64 = f
        .store
        .connection
        .query_row("SELECT COUNT(*) FROM objects", [], |row| row.get(0))
        .unwrap();
    let error = f.apply().unwrap_err();
    assert_eq!(
        crate::store_error_value(&error)["error"]["code"],
        "task_sync_id_collision"
    );
    assert_eq!(f.store.get_work_item(governing).unwrap(), before);
    assert_eq!(
        f.store
            .connection
            .query_row("SELECT COUNT(*) FROM objects", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        objects
    );
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn shared_updates_strict_delta_refuses_reference_bearing_native_note_echo() {
    let mut tree = claimable();
    tree.project.notes_included = true;
    let mut f = Fixture::with_store_changes(
        &tree,
        |store, tree| {
            let mut task = tree
                .tasks
                .iter()
                .find(|task| {
                    task.parent_id.is_none() && task.lifecycle == crate::WorkLifecycle::Open
                })
                .unwrap()
                .clone();
            let item = store.get_work_item(task.id).unwrap();
            let receipt = store
                .record_work_observation(
                    &crate::domain::RecordWorkObservationRequest {
                        status: false,
                        project_id: item.project_id,
                        work_id: item.work_id,
                        expected_work_revision: item.revision,
                        session_id: crate::SessionId("peer".into()),
                        summary: "Native reference-bearing prose".into(),
                        refs: vec!["https://example.invalid/private-context".into()],
                        actor: crate::domain::non_holder_note_actor(actor("peer")),
                        idempotency_key: "reference-note".into(),
                        recorded_at: at(31),
                    },
                    &DevelopmentNoopRedactor,
                )
                .unwrap();
            let original: crate::domain::WorkObservation =
                store.get(&receipt.evidence).unwrap().unwrap();
            task.notes.push(crate::task_sync::notes::SharedNote {
                id: receipt.evidence,
                kind: crate::task_sync::notes::SharedNoteKind::Generic,
                body: original.summary,
                author: crate::task_sync::SharedActor::from_actor(&original.actor),
                recorded_at: original.created_at,
                origin_machine: "fixture-host".into(),
            });
            task.notes.sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
            BTreeMap::from([(task.id, Some(task))])
        },
        |_, _| {},
        |_, _, _| {},
    );
    let before = f
        .store
        .export_task_sync_folder(&f.input.binding.project, &f.input.binding.profile)
        .unwrap()
        .tree;
    let item = f.store.get_work_item(f.selected).unwrap();
    assert!(f.apply().is_err());
    assert_eq!(f.store.get_work_item(f.selected).unwrap(), item);
    assert_eq!(
        f.store
            .export_task_sync_folder(&f.input.binding.project, &f.input.binding.profile,)
            .unwrap()
            .tree,
        before
    );
    assert!(f.store.verify_all().unwrap().is_healthy());
}
