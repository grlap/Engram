use super::*;
use crate::storage::work::{
    reset_work_item_projection_decode_count, work_item_projection_decode_count,
};
use crate::task_sync::{CapturedTask, progress::ExportProfile};
use std::collections::{BTreeMap, BTreeSet};

#[test]
fn selected_documents_match_full_export_without_reading_the_other_two_thousand() {
    use super::feed_changes::{document, exchange, profile, project, tree};

    let mut store = SqliteStore::open_in_memory().unwrap();
    let enrolled = tree((0..2000).map(document).collect());
    exchange(&mut store, &enrolled, "enrol", 5);
    let mut next = enrolled.clone();
    let mut selected = BTreeSet::new();
    for task in next.tasks.iter_mut().take(5) {
        task.title.push_str(" edited");
        selected.insert(task.id);
    }
    let retired = next.tasks.pop().unwrap().id;
    selected.insert(retired);
    let added = document(2000);
    selected.insert(added.id);
    next.tasks.push(added);
    exchange(&mut store, &tree(next.tasks), "edit", 6);

    let expected = store
        .work_read_snapshot(|snapshot| {
            // Capture first so the full reference export cannot warm the
            // transaction's origin cache before the measured keyed read.
            reset_work_item_projection_decode_count();
            origin::DECODES.with(|count| count.set(0));
            origin::ENUMERATIONS.with(|count| count.set(0));
            crate::canonical::reset_canonical_decode_count();
            let captured =
                snapshot.capture_task_documents(&project(), &profile(), selected.clone())?;
            // Six present documents, with the seventh identity represented as
            // an explicit departure. No project-wide origin enumeration.
            assert_eq!(work_item_projection_decode_count(), 6);
            assert_eq!(origin::DECODES.with(std::cell::Cell::get), 6);
            assert_eq!(origin::ENUMERATIONS.with(std::cell::Cell::get), 0);
            let selected_decodes = crate::canonical::canonical_decode_count();
            assert!(selected_decodes > 0);

            crate::canonical::reset_canonical_decode_count();
            let full = snapshot.export_task_sync_folder(&project(), &profile())?;
            let full_decodes = crate::canonical::canonical_decode_count();
            let expected = full
                .tree
                .tasks
                .into_iter()
                .filter(|task| selected.contains(&task.id))
                .map(|task| (task.id, CapturedTask::Present(Box::new(task))))
                .chain([(retired, CapturedTask::Retired)])
                .collect::<BTreeMap<_, _>>();

            assert_eq!(captured.documents, expected);
            assert!(selected_decodes < full_decodes);
            Ok(expected)
        })
        .unwrap();

    let unselected = enrolled
        .tasks
        .iter()
        .find(|task| !selected.contains(&task.id))
        .unwrap()
        .id;
    store
        .connection
        .execute(
            "UPDATE work_items SET item_json = ?2 WHERE work_id = ?1",
            params![unselected.0.to_string(), b"{".as_slice()],
        )
        .unwrap();
    assert!(
        store
            .export_task_sync_folder(&project(), &profile())
            .is_err()
    );
    reset_work_item_projection_decode_count();
    let captured = store
        .capture_task_documents(&project(), &profile(), selected)
        .unwrap();
    assert_eq!(captured.documents, expected);
    assert_eq!(work_item_projection_decode_count(), 6);
    assert!(
        store
            .capture_task_documents(&project(), &profile(), [unselected])
            .is_err()
    );
}

#[test]
fn selected_document_keeps_references_to_unselected_tasks() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let tree = candidate();
    import(&mut store, &tree);
    let child = tree
        .tasks
        .iter()
        .find(|task| task.parent_id.is_some())
        .unwrap();
    let full = store.export_task_tree(&tree.project.project_id).unwrap();
    let expected = full.tasks.iter().find(|task| task.id == child.id).unwrap();
    let captured = store
        .capture_task_documents(
            &tree.project.project_id,
            &ExportProfile {
                include_notes: false,
                origin_machine: None,
            },
            [child.id],
        )
        .unwrap();
    assert_eq!(captured.documents.len(), 1);
    assert_eq!(
        captured.documents[&child.id],
        CapturedTask::Present(Box::new(expected.clone()))
    );
    assert!(expected.parent_id.is_some());
}

#[test]
fn empty_absent_and_duplicate_selections_are_explicit_and_keyed() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let work = store
        .create_work(
            &root_request("keyed", "one", 1),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let profile = ExportProfile {
        include_notes: false,
        origin_machine: None,
    };
    reset_work_item_projection_decode_count();
    crate::canonical::reset_canonical_decode_count();
    let empty = store
        .capture_task_documents(&work.project_id, &profile, [])
        .unwrap();
    assert!(empty.documents.is_empty());
    assert_eq!(work_item_projection_decode_count(), 0);
    assert_eq!(crate::canonical::canonical_decode_count(), 0);
    let absent = WorkId(uuid::Uuid::new_v4());
    let captured = store
        .capture_task_documents(
            &work.project_id,
            &profile,
            [work.work_id, absent, work.work_id],
        )
        .unwrap();
    assert_eq!(captured.documents.len(), 2);
    assert_eq!(captured.documents[&absent], CapturedTask::Absent);
    assert!(matches!(
        captured.documents[&work.work_id],
        CapturedTask::Present(_)
    ));
    assert_eq!(work_item_projection_decode_count(), 1);
}

#[test]
fn selected_capture_refuses_other_projects_and_invalid_profiles_without_writes() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let work = store
        .create_work(
            &root_request("keyed-owner", "one", 1),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let before = state(&store);
    let profile = ExportProfile {
        include_notes: false,
        origin_machine: None,
    };
    let error = store
        .capture_task_documents(
            &ProjectId("another-project".into()),
            &profile,
            [work.work_id],
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("selected task belongs to another project")
    );
    assert!(
        store
            .capture_task_documents(&ProjectId(" ".into()), &profile, [])
            .is_err()
    );
    assert!(
        store
            .capture_task_documents(
                &work.project_id,
                &ExportProfile {
                    include_notes: true,
                    origin_machine: None
                },
                [work.work_id],
            )
            .is_err()
    );
    assert_eq!(state(&store), before);
}

#[test]
fn selected_capture_refuses_baseline_note_selection_changes_before_reading_tasks() {
    use super::feed_changes::{document, tree};

    for include_notes in [false, true] {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let mut enrolled = tree(vec![document(0)]);
        enrolled.project.notes_included = include_notes;
        let profile = ExportProfile {
            include_notes,
            origin_machine: Some("host-A".into()),
        };
        let basis = store
            .export_task_sync_folder(&enrolled.project.project_id, &profile)
            .unwrap()
            .export_basis;
        store
            .apply_task_sync_folder(
                crate::storage::TaskSyncFolderApply {
                    project: &enrolled.project.project_id,
                    actor: &actor("folder-importer"),
                    tree: &enrolled,
                    export_basis: &basis,
                    profile: &profile,
                    complete: true,
                    now: at(5),
                },
                "enrol",
            )
            .unwrap();
        let project = &enrolled.project.project_id;
        let id = enrolled.tasks[0].id;
        let before = state(&store);
        let matching = store
            .capture_task_documents(project, &profile, [id])
            .unwrap();
        assert_eq!(
            matching.documents[&id],
            CapturedTask::Present(Box::new(
                store
                    .export_task_sync_folder(project, &profile)
                    .unwrap()
                    .tree
                    .tasks[0]
                    .clone()
            ))
        );
        let wrong = ExportProfile {
            include_notes: !include_notes,
            ..profile
        };
        reset_work_item_projection_decode_count();
        crate::canonical::reset_canonical_decode_count();
        let keyed = store
            .capture_task_documents(project, &wrong, [id])
            .unwrap_err();
        let complete = store.export_task_sync_folder(project, &wrong).unwrap_err();
        for error in [&keyed, &complete] {
            assert!(matches!(
                error,
                StoreError::TaskSyncRefused {
                    refusal: TaskSyncRefusal::DivergentPlanning,
                    ..
                }
            ));
        }
        assert_eq!(keyed.to_string(), complete.to_string());
        assert!(
            keyed
                .to_string()
                .contains("note selection differs from its complete baseline")
        );
        assert!(store.capture_task_documents(project, &wrong, []).is_err());
        assert_eq!(work_item_projection_decode_count(), 0);
        assert_eq!(crate::canonical::canonical_decode_count(), 0);
        assert_eq!(state(&store), before);
    }
}

#[test]
fn selected_capture_preserves_note_and_activity_profiles_and_omissions() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let mut request = root_request("keyed-profiles", "notes and activity", 0);
    request.notes = vec!["Authored initial note".into()];
    let work = store
        .create_work(&request, &crate::DevelopmentNoopRedactor)
        .unwrap();
    claim(&mut store, &work, "holder", "claim", 1, 100);
    let mut protected = root_request("keyed-profiles", "restricted notes and activity", 2);
    protected.labels.push("restricted".into());
    protected.notes = vec!["Private retained observation".into()];
    let protected = store
        .create_work(&protected, &crate::DevelopmentNoopRedactor)
        .unwrap();
    claim(
        &mut store,
        &protected,
        "private-holder",
        "private-claim",
        3,
        100,
    );
    let before = state(&store);
    for profile in [
        ExportProfile {
            include_notes: false,
            origin_machine: None,
        },
        ExportProfile {
            include_notes: false,
            origin_machine: Some("host-A".into()),
        },
        ExportProfile {
            include_notes: true,
            origin_machine: Some("host-A".into()),
        },
    ] {
        store
            .work_read_snapshot(|snapshot| {
                let full = snapshot.export_task_sync_folder(&work.project_id, &profile)?;
                let captured = snapshot.capture_task_documents(
                    &work.project_id,
                    &profile,
                    [work.work_id, protected.work_id],
                )?;
                let expected = full
                    .tree
                    .tasks
                    .iter()
                    .find(|task| task.id == work.work_id)
                    .unwrap();
                assert_eq!(
                    captured.documents,
                    full.tree
                        .tasks
                        .iter()
                        .map(|task| (task.id, CapturedTask::Present(Box::new(task.clone()))))
                        .collect::<BTreeMap<_, _>>()
                );
                assert_eq!(captured.omissions.counts, full.omissions.counts);
                let expected_notes = if profile.include_notes {
                    BTreeMap::from([("restricted task".into(), 1)])
                } else {
                    BTreeMap::new()
                };
                assert_eq!(captured.omissions.counts, expected_notes);
                assert_eq!(
                    captured.activity_omissions.counts,
                    full.activity_omissions.counts
                );
                let expected_activity = if profile.origin_machine.is_some() {
                    BTreeMap::from([("restricted-task".into(), 1)])
                } else {
                    BTreeMap::new()
                };
                assert_eq!(captured.activity_omissions.counts, expected_activity);
                let excluded = full
                    .tree
                    .tasks
                    .iter()
                    .find(|task| task.id == protected.work_id)
                    .unwrap();
                assert_eq!(excluded.notes, []);
                assert!(excluded.activity.is_empty());
                assert_eq!(!expected.notes.is_empty(), profile.include_notes);
                assert_eq!(
                    !expected.activity.is_empty(),
                    profile.origin_machine.is_some()
                );
                Ok(())
            })
            .unwrap();
    }
    assert_eq!(state(&store), before);
}
