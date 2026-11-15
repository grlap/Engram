use super::*;

#[test]
fn shared_updates_additions_refuse_late_note_identity_reuse_across_projects() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let mut initial = tree_with_shared();
    apply(&mut store, &initial, None, "initial", 2);
    let late = note("Original late shared note", "remote");
    initial.tasks[0].notes.push(late.clone());
    initial.tasks[0]
        .notes
        .sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    apply(&mut store, &initial, None, "late", 3);
    let other = ProjectId("another-project".into());
    let mut added = document("Conflicting new task", None);
    let mut copied = late;
    copied.body = "Different prose on the same original id".into();
    added.notes = vec![copied];
    let mut incoming = TaskTree {
        project: crate::task_sync::ProjectDocument {
            format: crate::task_sync::FORMAT.into(),
            project_id: other.clone(),
            notes_included: true,
        },
        tasks: vec![added.clone()],
    };
    let error = store
        .import_task_tree(&other, &actor("receiver"), &incoming, false, at(4))
        .unwrap_err();
    assert_eq!(
        crate::store_error_value(&error)["error"]["code"],
        "task_sync_id_collision"
    );
    let apply_other = |store: &mut SqliteStore, tree: &TaskTree, key: &str| {
        let profile = shared_profile(None, true);
        let basis = store
            .export_task_sync_folder(&other, &profile)
            .unwrap()
            .export_basis;
        store
            .apply_task_sync_folder(
                TaskSyncFolderApply {
                    project: &other,
                    actor: &actor("receiver"),
                    tree,
                    export_basis: &basis,
                    profile: &profile,
                    complete: true,
                    now: at(5),
                },
                key,
            )
            .unwrap()
    };
    for key in ["first-addition", "existing-baseline-addition"] {
        if key == "existing-baseline-addition" {
            let seed = document("Existing independent member", None);
            let seeded = TaskTree {
                project: incoming.project.clone(),
                tasks: vec![seed.clone()],
            };
            assert_applied(&apply_other(&mut store, &seeded, "seed-other"));
            incoming.tasks.push(seed);
            incoming.tasks.sort_by_key(|task| task.id);
        }
        let result = apply_other(&mut store, &incoming, key);
        let refused = result
            .tasks
            .iter()
            .find(|task| task.task == added.id)
            .unwrap();
        assert_eq!(refused.status, crate::storage::TaskSyncTaskStatus::Refused);
        assert_eq!(refused.reason.as_deref(), Some("id_collision"));
        assert!(store.get_work_item(added.id).is_err());
        assert_eq!(capture(&store, None, true), initial);
    }
    initial.tasks[0]
        .notes
        .push(note("Original owner can still advance", "remote"));
    initial.tasks[0]
        .notes
        .sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    assert_applied(&apply(&mut store, &initial, None, "owner-continues", 6));
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn shared_updates_native_note_with_references_cannot_be_admitted_and_reexported() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let work = store
        .create_work(
            &root_request(PROJECT, "native", 0),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let receipt = store
        .record_work_observation(
            &crate::domain::RecordWorkObservationRequest {
                status: false,
                project_id: project(),
                work_id: work.work_id,
                expected_work_revision: work.revision,
                session_id: SessionId("peer".into()),
                summary: "Native note with a private reference".into(),
                refs: vec!["https://example.invalid/private-context".into()],
                actor: crate::domain::non_holder_note_actor(actor("peer")),
                idempotency_key: "reference-note".into(),
                recorded_at: at(1),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let original: crate::domain::WorkObservation = store.get(&receipt.evidence).unwrap().unwrap();
    let before = capture(&store, Some("local"), true);
    assert!(
        before.tasks[0]
            .notes
            .iter()
            .all(|note| note.id != receipt.evidence)
    );
    let mut incoming = before.clone();
    incoming.tasks[0].notes.push(SharedNote {
        id: receipt.evidence.clone(),
        kind: SharedNoteKind::Generic,
        body: original.summary,
        author: SharedActor::from_actor(&original.actor),
        recorded_at: original.created_at,
        origin_machine: "local".into(),
    });
    incoming.tasks[0]
        .notes
        .sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    let item = store.get_work_item(work.work_id).unwrap();
    let result = apply(
        &mut store,
        &incoming,
        Some("local"),
        "reject-reference-echo",
        2,
    );
    let refused = result
        .tasks
        .iter()
        .find(|task| task.task == work.work_id)
        .unwrap();
    assert_eq!(refused.status, crate::storage::TaskSyncTaskStatus::Refused);
    assert_eq!(refused.reason.as_deref(), Some("id_collision"));
    assert_eq!(store.get_work_item(work.work_id).unwrap(), item);
    assert_eq!(capture(&store, Some("local"), true), before);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn shared_updates_earlier_native_echo_preserves_unexported_release_with_remote_planning() {
    let mut source = SqliteStore::open_in_memory().unwrap();
    let work = source
        .create_work(
            &root_request(PROJECT, "native", 0),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let live = claim(&mut source, &work, "executor", "claim", 1, 100);
    let published = capture(&source, Some("host-A"), false);
    let mut receiver = SqliteStore::open_in_memory().unwrap();
    apply(&mut receiver, &published, None, "receive", 2);
    let item = source.get_work_item(work.work_id).unwrap();
    source
        .release_work(
            &ReleaseWorkRequest {
                work_id: work.work_id,
                run_id: live.run_id,
                expected_work_revision: item.revision,
                holder: live.holder,
                claim_id: live.claim_id,
                claim_fence: live.fence,
                reason: "Local episode ended".into(),
                waiver_reason: Some("No contribution".into()),
                actor: actor("executor"),
                idempotency_key: "release".into(),
                released_at: at(3),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let local = capture(&source, Some("host-A"), false);
    let mut incoming = capture(&receiver, None, false);
    incoming.tasks[0].title = "Remote planning edit".into();
    assert_applied(&apply(
        &mut source,
        &incoming,
        Some("host-A"),
        "return-stale-native",
        4,
    ));
    let after = capture(&source, Some("host-A"), false);
    assert_eq!(after.tasks[0].title, "Remote planning edit");
    assert_eq!(after.tasks[0].activity, local.tasks[0].activity);
    assert!(
        after.tasks[0].activity["host-A"]
            .values()
            .all(|notice| notice.state == ActivityState::Released)
    );
    assert_eq!(
        super::super::super::super::baseline::load(&source.connection, &project())
            .unwrap()
            .unwrap()
            .tree,
        incoming
    );
    assert!(source.verify_all().unwrap().is_healthy());
}

#[test]
fn shared_updates_membership_removal_keeps_late_note_detail_and_graph_history() {
    let initial = tree_with_shared();
    let id = initial.tasks[0].id;
    let mut store = SqliteStore::open_in_memory().unwrap();
    apply(&mut store, &initial, None, "initial", 2);
    let mut next = initial;
    next.tasks[0].notes.push(note(
        "Late shared history survives membership retirement",
        "remote",
    ));
    next.tasks[0]
        .notes
        .sort_by(|a, b| a.id.as_str().cmp(b.id.as_str()));
    apply(&mut store, &next, None, "late", 3);
    let rows = store
        .work_record_index(&project(), id, WorkRecordKind::Notes)
        .unwrap();
    let row = rows
        .into_iter()
        .find(|row| row.locator.contains(":shared-note-"))
        .unwrap();
    let WorkRecordContent::Note(before) = store.work_record_content(&project(), id, &row).unwrap()
    else {
        panic!("shared note expected");
    };
    let mut removed = next;
    removed.tasks.retain(|task| task.id != id);
    assert_applied(&apply(&mut store, &removed, None, "remove-member", 4));
    let after = store
        .work_record_index(&project(), id, WorkRecordKind::Notes)
        .unwrap();
    let retained = after
        .iter()
        .find(|candidate| candidate.locator == row.locator)
        .unwrap();
    let WorkRecordContent::Note(detail) =
        store.work_record_content(&project(), id, retained).unwrap()
    else {
        panic!("shared note expected");
    };
    assert_eq!(detail.summary, before.summary);
    let graph = store
        .save_work_graph_snapshot(
            &project(),
            &actor("reader"),
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(5),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    assert_eq!(
        serde_json::to_string(&graph.document)
            .unwrap()
            .matches(&before.summary)
            .count(),
        1
    );
    assert!(store.verify_all().unwrap().is_healthy());
}
