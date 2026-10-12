use super::*;
use crate::storage::work::test_support::checkpoint;
use crate::task_sync::{
    SharedActor,
    activity::{ActivityNotice, ActivityState},
};

#[test]
fn task_prepare_without_native_alias_preserves_bytes_after_a_claim() {
    let home = crate::test_support::temp_home().unwrap();
    let database = home.path().join("store.db");
    let mut store = SqliteStore::open(&database).unwrap();
    let project = ProjectId("prepare-runtime-only".into());
    let item = store
        .create_work(
            &root_request(&project.0, "Planning only", 0),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let profile = ExportProfile {
        include_notes: false,
        origin_machine: None,
    };
    let progress = home.path().join("progress");
    let (_, original) = prepare(
        &store,
        &database,
        &project,
        &profile,
        &home.path().join("first"),
        &progress,
    );
    claim(&mut store, &item, "worker", "claim", 1, 100);
    let output = home.path().join("claimed");
    let (receipt, full) = prepare(&store, &database, &project, &profile, &output, &progress);
    assert_eq!(receipt.serialized_tasks, 0);
    assert_eq!(directory::read_documents(&output).unwrap(), original);
    assert_eq!(full, original);
    assert!(
        TaskTree::from_stored_documents(&full).unwrap().tasks[0]
            .activity
            .is_empty()
    );
}

#[test]
fn task_prepare_activity_profiles_renew_release_and_omissions_equal_full_snapshot() {
    for include_notes in [false, true] {
        let home = crate::test_support::temp_home().unwrap();
        let database = home.path().join("store.db");
        let mut store = SqliteStore::open(&database).unwrap();
        let project = ProjectId("prepared-activity".into());
        let mut item = store
            .create_work(
                &root_request(&project.0, "Shared activity", 0),
                &crate::DevelopmentNoopRedactor,
            )
            .unwrap();
        let profile = ExportProfile {
            include_notes,
            origin_machine: Some("host-a".into()),
        };
        let progress = home.path().join("progress");
        let first = home.path().join("first");
        prepare(&store, &database, &project, &profile, &first, &progress);
        claim(&mut store, &item, "worker", "start", 1, 100);
        item = store.get_work_item(item.work_id).unwrap();
        let started = home.path().join("started");
        let (receipt, original) =
            prepare(&store, &database, &project, &profile, &started, &progress);
        assert_eq!(receipt.serialized_tasks, 1);
        assert_eq!(directory::read_documents(&started).unwrap(), original);
        let tree = TaskTree::from_stored_documents(&original).unwrap();
        let notice = tree.tasks[0].activity["host-a"]
            .values()
            .next()
            .unwrap()
            .clone();
        assert_eq!(notice.state, ActivityState::Started);
        assert_eq!(notice.activity_id, notice.event_id);
        assert_eq!(tree.project.notes_included, include_notes);

        let renewed = claim(&mut store, &item, "worker", "renew", 2, 100);
        checkpoint(&mut store, &item, &renewed, "worker", "checkpoint", 3, &[]);
        let unchanged = home.path().join("unchanged");
        let (receipt, full) = prepare(&store, &database, &project, &profile, &unchanged, &progress);
        assert_eq!(receipt.serialized_tasks, 0);
        assert_eq!(full, original, "renewal and checkpoint are runtime-only");
        assert_eq!(directory::read_documents(&unchanged).unwrap(), full);

        store
            .release_work(
                &crate::domain::ReleaseWorkRequest {
                    work_id: item.work_id,
                    run_id: renewed.run_id,
                    expected_work_revision: item.revision,
                    holder: renewed.holder.clone(),
                    claim_id: renewed.claim_id,
                    claim_fence: renewed.fence,
                    reason: "local execution reason".into(),
                    waiver_reason: Some("participant left".into()),
                    actor: actor("worker"),
                    idempotency_key: "release".into(),
                    released_at: at(4),
                },
                &crate::DevelopmentNoopRedactor,
            )
            .unwrap();
        let released = home.path().join("released");
        let (receipt, full) = prepare(&store, &database, &project, &profile, &released, &progress);
        assert_eq!(receipt.serialized_tasks, 1);
        assert_eq!(directory::read_documents(&released).unwrap(), full);
        let tree = TaskTree::from_stored_documents(&full).unwrap();
        let ended = &tree.tasks[0].activity["host-a"][&notice.activity_id];
        assert_eq!(ended.state, ActivityState::Released);
        assert_eq!(ended.state_revision, 2);
        assert_eq!(ended.activity_id, notice.activity_id);
        assert_ne!(ended.event_id, notice.event_id);

        item = store.get_work_item(item.work_id).unwrap();
        store
            .revise_work(
                &crate::domain::ReviseWorkRequest {
                    work_id: item.work_id,
                    expected_revision: item.revision,
                    patch: crate::domain::WorkRevisionPatch {
                        add_labels: vec!["secret".into()],
                        ..Default::default()
                    },
                    authority: crate::domain::WorkPlanningAuthority::Project,
                    actor: actor("planner"),
                    idempotency_key: "restrict".into(),
                    updated_at: at(5),
                },
                &crate::DevelopmentNoopRedactor,
            )
            .unwrap();
        let restricted = home.path().join("restricted");
        let (receipt, full) = prepare(
            &store,
            &database,
            &project,
            &profile,
            &restricted,
            &progress,
        );
        assert_eq!(receipt.serialized_tasks, 1);
        assert_eq!(
            receipt.activity_omissions.counts.get("restricted-task"),
            Some(&1)
        );
        assert!(receipt.omissions.counts.is_empty());
        assert_eq!(directory::read_documents(&restricted).unwrap(), full);
        assert!(
            TaskTree::from_stored_documents(&full).unwrap().tasks[0]
                .activity
                .is_empty()
        );
    }
}

#[test]
fn task_prepare_feedless_import_retains_activity_without_a_native_alias() {
    let home = crate::test_support::temp_home().unwrap();
    let database = home.path().join("store.db");
    let mut store = SqliteStore::open(&database).unwrap();
    let mut imported = candidate();
    let task = &mut imported.tasks[0];
    let activity_id = CanonicalObject::mint(&"foreign start")
        .unwrap()
        .key()
        .clone();
    let notice = ActivityNotice {
        activity_id: activity_id.clone(),
        task_id: task.id,
        origin_machine: "host-other".into(),
        actor: SharedActor::from_actor(&actor("foreign")),
        state: ActivityState::Started,
        state_revision: 1,
        event_id: activity_id.clone(),
    };
    task.activity
        .entry("host-other".into())
        .or_default()
        .insert(activity_id.clone(), notice.clone());
    let project = &imported.project.project_id;
    let profile = ExportProfile {
        include_notes: false,
        origin_machine: None,
    };
    let progress = home.path().join("progress");
    prepare(
        &store,
        &database,
        project,
        &profile,
        &home.path().join("empty"),
        &progress,
    );
    import(&mut store, &imported);
    let output = home.path().join("imported");
    let (receipt, full) = prepare(&store, &database, project, &profile, &output, &progress);
    assert_eq!(receipt.project_position, 0);
    assert_eq!(receipt.serialized_tasks, imported.tasks.len());
    assert_eq!(directory::read_documents(&output).unwrap(), full);
    assert_eq!(
        TaskTree::from_stored_documents(&full).unwrap().tasks[0].activity["host-other"]
            [&activity_id],
        notice
    );
}
