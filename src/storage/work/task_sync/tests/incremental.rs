use super::*;
use crate::storage::work::test_support::evidence;
use crate::task_sync::{
    directory,
    progress::{ExportProfile, PrepareReceipt},
};
use std::{cell::RefCell, collections::BTreeMap, path::Path};

mod activity;

fn prepare(
    store: &SqliteStore,
    database: &Path,
    project: &ProjectId,
    profile: &ExportProfile,
    output: &Path,
    progress: &Path,
) -> (PrepareReceipt, BTreeMap<String, Vec<u8>>) {
    let full = RefCell::new(None);
    let result = crate::task_sync::progress::prepare(
        output,
        progress,
        database,
        project,
        profile,
        false,
        |previous| {
            let capture = store.capture_task_prepare(project, profile, previous)?;
            *full.borrow_mut() = Some(capture.tree.documents()?);
            Ok(capture)
        },
    )
    .unwrap();
    (result, full.into_inner().unwrap())
}

#[test]
fn task_prepare_actual_activity_and_note_mutations_equal_full_same_snapshot() {
    let home = crate::test_support::temp_home().unwrap();
    let database = home.path().join("store.db");
    let mut store = SqliteStore::open(&database).unwrap();
    let project = ProjectId("prepare-native".into());
    let profile = ExportProfile {
        include_notes: true,
        origin_machine: Some("host-a".into()),
    };
    let item = store
        .create_work(
            &root_request(&project.0, "native task", 0),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let progress = home.path().join("progress");
    let first = home.path().join("first");
    let (receipt, full) = prepare(&store, &database, &project, &profile, &first, &progress);
    assert_eq!(receipt.serialized_tasks, 1);
    assert_eq!(directory::read_documents(&first).unwrap(), full);
    let claimed = claim(&mut store, &item, "worker", "claim", 1, 100);
    let started = home.path().join("started");
    let (receipt, full) = prepare(&store, &database, &project, &profile, &started, &progress);
    assert_eq!(receipt.serialized_tasks, 1);
    assert_eq!(directory::read_documents(&started).unwrap(), full);
    let mut planning = TaskTree::from_stored_documents(&full).unwrap();
    assert_eq!(planning.tasks[0].activity["host-a"].len(), 1);
    planning.tasks[0].activity.clear();
    assert_eq!(
        planning.documents().unwrap(),
        directory::read_documents(&first).unwrap()
    );
    evidence(&mut store, &item, &claimed, "worker", "one-note", 2);
    let excluded = home.path().join("excluded");
    let (receipt, full) = prepare(&store, &database, &project, &profile, &excluded, &progress);
    assert_eq!(receipt.serialized_tasks, 0);
    assert_eq!(receipt.omissions.counts.get("references"), Some(&1));
    assert_eq!(directory::read_documents(&excluded).unwrap(), full);
    assert_eq!(full, directory::read_documents(&started).unwrap());
    let note = store
        .record_work_note(
            &crate::domain::RecordWorkNoteRequest {
                status: true,
                work_id: item.work_id,
                run_id: claimed.run_id,
                expected_work_revision: item.revision,
                holder: claimed.holder.clone(),
                claim_id: claimed.claim_id,
                claim_fence: claimed.fence,
                summary: "Captured progress".into(),
                refs: vec![],
                actor: actor("worker"),
                idempotency_key: "shared-status".into(),
                recorded_at: at(3),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let noted = home.path().join("noted");
    let (receipt, full) = prepare(&store, &database, &project, &profile, &noted, &progress);
    assert_eq!(receipt.serialized_tasks, 1);
    assert_eq!(directory::read_documents(&noted).unwrap(), full);
    let tree = TaskTree::from_stored_documents(&full).unwrap();
    assert_eq!(tree.tasks[0].notes.len(), 1);
    assert_eq!(tree.tasks[0].notes[0].id, note.evidence);
    assert_eq!(tree.tasks[0].notes[0].body, "Captured progress");
    let same_head = home.path().join("same-head");
    let (receipt, full) = prepare(&store, &database, &project, &profile, &same_head, &progress);
    assert_eq!(receipt.serialized_tasks, 0);
    assert_eq!(directory::read_documents(&same_head).unwrap(), full);
}

#[test]
fn task_prepare_feedless_import_inventory_and_lower_cursor_fall_back_without_skipping() {
    let home = crate::test_support::temp_home().unwrap();
    let database = home.path().join("store.db");
    let mut store = SqliteStore::open(&database).unwrap();
    let tree = candidate();
    let project = &tree.project.project_id;
    let profile = ExportProfile {
        include_notes: false,
        origin_machine: None,
    };
    let progress = home.path().join("progress");
    let first = home.path().join("empty");
    let (receipt, full) = prepare(&store, &database, project, &profile, &first, &progress);
    assert_eq!(receipt.project_position, 0);
    assert_eq!(directory::read_documents(&first).unwrap(), full);
    import(&mut store, &tree);
    let second = home.path().join("imported");
    let (receipt, full) = prepare(&store, &database, project, &profile, &second, &progress);
    assert_eq!(receipt.project_position, 0);
    assert_eq!(receipt.path, "incremental");
    assert_eq!(receipt.serialized_tasks, tree.tasks.len());
    assert_eq!(directory::read_documents(&second).unwrap(), full);
    let previous = crate::task_sync::progress::ExportCut {
        position: 1,
        anchor: Some(crate::task_sync::progress::FeedAnchor {
            object_kind: "work_event".into(),
            object_id: ObjectId::from_canonical_bytes(b"source basis"),
        }),
    };
    let captured = store
        .capture_task_prepare(project, &profile, Some(&previous))
        .unwrap();
    assert!(captured.full_reason.unwrap().contains("behind"));
    assert_eq!(captured.tree.documents().unwrap(), full);
}

#[test]
fn task_prepare_native_planning_routes_equal_full_at_each_cut() {
    use crate::domain::{ReviseWorkRequest, WorkPlanningAuthority, WorkRevisionPatch};
    let home = crate::test_support::temp_home().unwrap();
    let database = home.path().join("store.db");
    let mut store = SqliteStore::open(&database).unwrap();
    let project = ProjectId("prepare-planning".into());
    let redactor = crate::DevelopmentNoopRedactor;
    let mut item = store
        .create_work(&root_request(&project.0, "first", 0), &redactor)
        .unwrap();
    let profile = ExportProfile {
        include_notes: false,
        origin_machine: None,
    };
    let progress = home.path().join("progress");
    let check = |store: &SqliteStore, step, expected| {
        let output = home.path().join(format!("candidate-{step}"));
        let (receipt, full) = prepare(store, &database, &project, &profile, &output, &progress);
        assert_eq!(receipt.serialized_tasks, expected, "step {step}");
        assert_eq!(
            directory::read_documents(&output).unwrap(),
            full,
            "step {step}"
        );
    };
    check(&store, 0, 1);
    let patches = [
        WorkRevisionPatch {
            title: Some("revised title".into()),
            ..Default::default()
        },
        WorkRevisionPatch {
            outcome: Some("revised outcome".into()),
            ..Default::default()
        },
        WorkRevisionPatch {
            acceptance: Some(vec!["new criterion".into()]),
            ..Default::default()
        },
        WorkRevisionPatch {
            acceptance_bindings: Some(vec![AcceptanceBinding {
                criterion: 1,
                requirement: VerificationRequirement {
                    check_kind: VerificationKind::Test,
                    check_fingerprint: None,
                },
            }]),
            ..Default::default()
        },
        WorkRevisionPatch {
            kind: Some(WorkItemKind::Bug),
            priority: Some(2),
            ..Default::default()
        },
        WorkRevisionPatch {
            labels: Some(vec!["first".into()]),
            ..Default::default()
        },
        WorkRevisionPatch {
            add_labels: vec!["second".into()],
            remove_labels: vec!["first".into()],
            ..Default::default()
        },
        WorkRevisionPatch {
            assigned_to: Some("future-worker".into()),
            deferred_until: Some(at(100)),
            external_ref: Some("ticket".into()),
            evaluation_mode: Some(AcceptanceEvaluationMode::SameSession),
            ..Default::default()
        },
        WorkRevisionPatch {
            clear_assignment: true,
            clear_deferral: true,
            clear_external: true,
            clear_evaluation_mode: true,
            ..Default::default()
        },
    ];
    let mut step = 1;
    for patch in patches {
        item = store
            .revise_work(
                &ReviseWorkRequest {
                    work_id: item.work_id,
                    expected_revision: item.revision,
                    patch,
                    authority: WorkPlanningAuthority::Project,
                    actor: actor("planner"),
                    idempotency_key: format!("revision-{step}"),
                    updated_at: at(step),
                },
                &redactor,
            )
            .unwrap();
        check(&store, step, 1);
        step += 1;
    }
    let other = store
        .create_work(&root_request(&project.0, "prerequisite", step), &redactor)
        .unwrap();
    check(&store, step, 1);
    step += 1;
    for add in [true, false] {
        let request = crate::domain::ChangeWorkPrerequisiteRequest {
            work_id: item.work_id,
            prerequisite_id: other.work_id,
            expected_revision: item.revision,
            authority: WorkPlanningAuthority::Project,
            actor: actor("planner"),
            idempotency_key: format!("edge-{step}"),
            changed_at: at(step),
        };
        item = if add {
            store.add_work_prerequisite(&request, &redactor)
        } else {
            store.remove_work_prerequisite(&request, &redactor)
        }
        .unwrap();
        check(&store, step, 1);
        step += 1;
    }
    let blocker = store
        .add_work_blocker(
            &crate::domain::AddWorkBlockerRequest {
                work_id: item.work_id,
                expected_work_revision: item.revision,
                kind: WorkBlockerKind::ExternalInput,
                detail: "Input required".into(),
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner"),
                idempotency_key: "block".into(),
                blocked_at: at(step),
            },
            &redactor,
        )
        .unwrap();
    check(&store, step, 1);
    step += 1;
    item = query::load_work_item(&store.connection, item.work_id).unwrap();
    store
        .clear_work_blocker(
            &crate::domain::ClearWorkBlockerRequest {
                work_id: item.work_id,
                expected_work_revision: item.revision,
                blocker_id: blocker.blocker_id,
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner"),
                idempotency_key: "unblock".into(),
                cleared_at: at(step),
            },
            &redactor,
        )
        .unwrap();
    check(&store, step, 1);
    step += 1;
    item = query::load_work_item(&store.connection, item.work_id).unwrap();
    store
        .dispose_work(
            &crate::domain::DisposeWorkRequest {
                work_id: item.work_id,
                expected_work_revision: item.revision,
                disposition: crate::WorkDisposition::Cancelled,
                replacement_id: None,
                reason: "No longer needed".into(),
                actor: actor("planner"),
                idempotency_key: "cancel".into(),
                disposed_at: at(step),
            },
            &redactor,
        )
        .unwrap();
    check(&store, step, 1);
    assert!(store.verify_all().unwrap().is_healthy());
}
