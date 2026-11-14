use super::*;

fn complete(task: &mut TaskDocument) {
    task.lifecycle = WorkLifecycle::Completed;
    task.completion = Some(SharedCompletion {
        summary: "Reported by another host".into(),
        completed_at: at(3),
        actor: SharedActor::from_actor(&actor("remote-executor")),
    });
}

#[test]
fn an_expired_unreleased_claim_keeps_its_cancelled_run_fence_in_sync() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let original = store
        .create_work(
            &root_request(PROJECT, "creator", 0),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let claim = crate::storage::work::test_support::claim(
        &mut store,
        &original,
        "expired",
        "expired-claim",
        1,
        60,
    );
    let incoming = edited(&exported(&store), original.work_id, complete);
    let result = exchange(&mut store, &incoming, "completion-after-expiry", 62).unwrap();
    assert_eq!(result.tasks[0].status, TaskSyncTaskStatus::Applied);
    let retained = query::load_work_claim_optional(&store.connection, claim.run_id)
        .unwrap()
        .unwrap();
    assert_eq!(retained.state, crate::WorkClaimState::Released);
    assert_eq!(retained.fence, claim.fence + 1);
    let head: i64 = store
        .connection
        .query_row(
            "SELECT claim_fence_head FROM work_runs WHERE run_id=?1",
            [claim.run_id.0.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(head, retained.fence);
    let run = query::load_work_run(&store.connection, claim.run_id).unwrap();
    assert_eq!(run.state, crate::WorkRunState::Cancelled);
    assert!(run.completion_seal.is_none());
    let health = store.verify_all().unwrap();
    assert!(health.is_healthy(), "{health:?}");
}

#[test]
fn reported_completion_history_round_trips_through_graph_snapshot_save_and_load() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let native = store
        .create_work(
            &root_request(PROJECT, "snapshot-source", 0),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let incoming = edited(&exported(&store), native.work_id, complete);
    exchange(&mut store, &incoming, "snapshot-reported-completion", 5).unwrap();
    let saved = store
        .save_work_graph_snapshot(
            &project(),
            &actor("snapshot-saver"),
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(6),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let bytes = serde_json::to_vec(&saved.document).unwrap();
    let mut restored = SqliteStore::open_in_memory().unwrap();
    restored
        .load_work_graph_snapshot(
            &project(),
            &actor("snapshot-loader"),
            &bytes,
            false,
            at(7),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let item = restored.get_work_item(native.work_id).unwrap();
    assert_eq!(item.lifecycle, WorkLifecycle::Completed);
    assert!(item.restored && item.active_run_id.is_none());
    assert_eq!(exported(&restored), incoming);
    let health = restored.verify_all().unwrap();
    assert!(health.is_healthy(), "{health:?}");
    let resaved = restored
        .save_work_graph_snapshot(
            &project(),
            &actor("snapshot-resaver"),
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(8),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let mut second_restore = SqliteStore::open_in_memory().unwrap();
    second_restore
        .load_work_graph_snapshot(
            &project(),
            &actor("snapshot-second-loader"),
            &serde_json::to_vec(&resaved.document).unwrap(),
            false,
            at(9),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    assert_eq!(exported(&second_restore), incoming);
    let seals: i64 = second_restore
        .connection
        .query_row(
            "SELECT COUNT(*) FROM work_completion_seals WHERE work_id=?1",
            [native.work_id.0.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(seals, 0);
}

#[test]
fn reported_completion_closes_local_activity_without_rolling_back_independent_edits() {
    use crate::task_sync::activity::ActivityState;

    let mut store = SqliteStore::open_in_memory().unwrap();
    let original = store
        .create_work(
            &root_request(PROJECT, "creator", 0),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let independent = store
        .create_work(
            &root_request(PROJECT, "other-creator", 0),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    crate::storage::work::test_support::claim(
        &mut store,
        &original,
        "expired",
        "expired-activity-claim",
        1,
        60,
    );
    let profile = ExportProfile {
        include_notes: false,
        origin_machine: Some("host-a".into()),
    };
    let before = store.export_task_sync_folder(&project(), &profile).unwrap();
    let episode = before
        .tree
        .tasks
        .iter()
        .find(|task| task.id == original.work_id)
        .unwrap()
        .activity["host-a"]
        .values()
        .next()
        .unwrap()
        .clone();
    assert_eq!(episode.state, ActivityState::Started);
    let incoming = edited(&before.tree, original.work_id, complete);
    let incoming = edited(&incoming, independent.work_id, |task| {
        task.title = "Independent incoming edit".into();
    });
    let result = store
        .apply_task_sync_folder(
            TaskSyncFolderApply {
                project: &project(),
                actor: &actor("folder-importer"),
                tree: &incoming,
                export_basis: &before.export_basis,
                profile: &profile,
                complete: true,
                now: at(62),
            },
            "report-with-activity",
        )
        .unwrap();
    assert_eq!(result.tasks.len(), 2);
    assert!(
        result
            .tasks
            .iter()
            .all(|task| task.status == TaskSyncTaskStatus::Applied)
    );
    assert_eq!(
        store.get_work_item(independent.work_id).unwrap().title,
        "Independent incoming edit"
    );
    let after = store.export_task_sync_folder(&project(), &profile).unwrap();
    let completed = after
        .tree
        .tasks
        .iter()
        .find(|task| task.id == original.work_id)
        .unwrap();
    let closed = &completed.activity["host-a"][&episode.activity_id];
    assert_eq!(closed.state, ActivityState::Abandoned);
    assert_eq!(closed.state_revision, episode.state_revision + 1);
    assert_eq!(closed.actor, episode.actor);
    let records =
        query::canonical_work_event_records_for_item(&store.connection, original.work_id).unwrap();
    let (event_id, event) = records.last().unwrap();
    assert!(matches!(
        event.transition,
        WorkTransition::CompletionReported { .. }
    ));
    assert_eq!(&closed.event_id, event_id);
    let baseline = super::super::super::super::baseline::load(&store.connection, &project())
        .unwrap()
        .unwrap();
    assert_eq!(baseline.tree, after.tree);
    let retry = store
        .apply_task_sync_folder(
            TaskSyncFolderApply {
                project: &project(),
                actor: &actor("folder-importer"),
                tree: &incoming,
                export_basis: &before.export_basis,
                profile: &profile,
                complete: true,
                now: at(62),
            },
            "report-with-activity",
        )
        .unwrap();
    assert_eq!(retry, result);
    let health = store.verify_all().unwrap();
    assert!(health.is_healthy(), "{health:?}");
}

#[test]
fn partial_membership_retains_unenrolled_local_endpoints_without_consuming_their_edit() {
    for empty_baseline in [false, true] {
        for held in [false, true] {
            let mut store = SqliteStore::open_in_memory().unwrap();
            if empty_baseline {
                let empty = exported(&store);
                exchange(&mut store, &empty, "empty-baseline", 0).unwrap();
            }
            let native = store
                .create_work(
                    &root_request(PROJECT, "creator", 1),
                    &crate::DevelopmentNoopRedactor,
                )
                .unwrap();
            let claim =
                held.then(|| claim_restored(&mut store, native.work_id, "local-executor", 2));
            let current = exported(&store);
            let mut incoming = edited(&current, native.work_id, |task| {
                task.title = "Unapplied incoming title".into();
                if !held {
                    task.kind = if task.kind == WorkItemKind::Task {
                        WorkItemKind::Feature
                    } else {
                        WorkItemKind::Task
                    };
                }
            });
            let mut dependent = document("independent root using the existing endpoint", None);
            dependent.prerequisites.push(native.work_id);
            incoming.tasks.push(dependent.clone());
            incoming.tasks.sort_by_key(|task| task.id.0);
            let result = exchange(&mut store, &incoming, "partial-reference-closure", 3).unwrap();
            let outcome = result
                .tasks
                .iter()
                .find(|task| task.task == native.work_id)
                .unwrap();
            assert_eq!(
                outcome.status,
                if held {
                    TaskSyncTaskStatus::Held
                } else {
                    TaskSyncTaskStatus::Refused
                }
            );
            assert_eq!(
                result
                    .tasks
                    .iter()
                    .find(|task| task.task == dependent.id)
                    .unwrap()
                    .status,
                TaskSyncTaskStatus::Applied
            );
            let baseline =
                super::super::super::super::baseline::load(&store.connection, &project())
                    .unwrap()
                    .unwrap();
            baseline.tree.folder_documents().unwrap();
            assert_eq!(
                baseline
                    .tree
                    .tasks
                    .iter()
                    .find(|task| task.id == native.work_id)
                    .unwrap(),
                current
                    .tasks
                    .iter()
                    .find(|task| task.id == native.work_id)
                    .unwrap()
            );
            assert_eq!(
                store.get_work_item(dependent.id).unwrap().title,
                dependent.title
            );
            if let Some(claim) = claim {
                let item = store.get_work_item(native.work_id).unwrap();
                store
                    .release_work(
                        &ReleaseWorkRequest {
                            work_id: item.work_id,
                            run_id: claim.run_id,
                            claim_id: claim.claim_id,
                            expected_work_revision: item.revision,
                            holder: claim.holder,
                            claim_fence: claim.fence,
                            waiver_reason: Some("No contribution".into()),
                            actor: actor("local-executor"),
                            reason: "Apply incoming planning".into(),
                            idempotency_key: "release-local-endpoint".into(),
                            released_at: at(4),
                        },
                        &crate::DevelopmentNoopRedactor,
                    )
                    .unwrap();
                let retry = exchange(&mut store, &incoming, "retry-local-endpoint", 5).unwrap();
                assert_eq!(
                    retry
                        .tasks
                        .iter()
                        .find(|task| task.task == native.work_id)
                        .unwrap()
                        .status,
                    TaskSyncTaskStatus::Applied
                );
                assert_eq!(
                    store.get_work_item(native.work_id).unwrap().title,
                    "Unapplied incoming title"
                );
            }
            let health = store.verify_all().unwrap();
            assert!(health.is_healthy(), "{health:?}");
        }
    }
}

#[test]
fn a_late_relation_owner_hold_retains_its_prerequisite_and_allows_an_independent_root() {
    let fixture = fixture();
    let target = document("removal target", None);
    let mut initial = fixture.tree;
    initial
        .tasks
        .iter_mut()
        .find(|task| task.id == fixture.first)
        .unwrap()
        .prerequisites
        .push(target.id);
    initial.tasks.push(target.clone());
    initial.tasks.sort_by_key(|task| task.id.0);
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &initial, "initial-relation", 5).unwrap();
    claim_restored(&mut store, fixture.parent, "engaged-parent", 6);
    let mut incoming = edited(&exported(&store), fixture.first, |task| {
        task.prerequisites.clear();
    });
    incoming.tasks.retain(|task| task.id != target.id);
    let independent = document("independent accepted root", None);
    incoming.tasks.push(independent.clone());
    incoming.tasks.sort_by_key(|task| task.id.0);
    let result = exchange(&mut store, &incoming, "late-owner-hold", 7).unwrap();
    let owner = result
        .tasks
        .iter()
        .find(|task| task.task == fixture.first)
        .unwrap();
    assert_eq!(owner.status, TaskSyncTaskStatus::Held);
    let target_result = result
        .tasks
        .iter()
        .find(|task| task.task == target.id)
        .unwrap();
    assert_eq!(target_result.status, TaskSyncTaskStatus::Refused);
    assert_eq!(target_result.reason.as_deref(), Some("retained_reference"));
    assert_eq!(
        result
            .tasks
            .iter()
            .find(|task| task.task == independent.id)
            .unwrap()
            .status,
        TaskSyncTaskStatus::Applied
    );
    assert_eq!(
        exported(&store)
            .tasks
            .iter()
            .find(|task| task.id == fixture.first)
            .unwrap()
            .prerequisites,
        [target.id]
    );
    assert!(store.get_work_item(target.id).is_ok());
    let health = store.verify_all().unwrap();
    assert!(health.is_healthy(), "{health:?}");
}

#[test]
fn reported_completion_keeps_existing_shared_notes_once_without_false_omissions() {
    let mut task = document("reported completion with shared notes", None);
    task.notes.push(crate::task_sync::notes::SharedNote {
        id: crate::CanonicalObject::mint(&"shared original note")
            .unwrap()
            .key()
            .clone(),
        kind: crate::task_sync::notes::SharedNoteKind::Generic,
        body: "A shared note that already exists".into(),
        author: SharedActor::from_actor(&actor("original-author")),
        recorded_at: at(1),
        origin_machine: "A".into(),
    });
    let mut initial = fixture().tree;
    initial.project.notes_included = true;
    initial.tasks = vec![task.clone()];
    let selection = ExportProfile {
        include_notes: true,
        origin_machine: Some("B".into()),
    };
    let mut store = SqliteStore::open_in_memory().unwrap();
    for (tree, key, second) in [
        (&initial, "initial-notes", 2),
        (&edited(&initial, task.id, complete), "report-notes", 4),
    ] {
        let basis = store
            .export_task_sync_folder(&project(), &selection)
            .unwrap()
            .export_basis;
        store
            .apply_task_sync_folder(
                TaskSyncFolderApply {
                    project: &project(),
                    actor: &actor("importer"),
                    tree,
                    export_basis: &basis,
                    profile: &selection,
                    complete: true,
                    now: at(second),
                },
                key,
            )
            .unwrap();
    }
    let exported = store
        .export_task_sync_folder(&project(), &selection)
        .unwrap();
    assert_eq!(exported.tree.tasks[0].notes, task.notes);
    assert_eq!(exported.omissions.counts, std::collections::BTreeMap::new());
    let count: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM work_note_fts WHERE work_id=?1",
            [task.id.0.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(count, 1);
    let health = store.verify_all().unwrap();
    assert!(health.is_healthy(), "{health:?}");
}
