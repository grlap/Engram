//! Change capture names exactly the tasks a feed interval changed, from the
//! changed-task index, however large the exchange.

use std::collections::BTreeSet;

use super::*;
use crate::storage::TaskSyncFolderApply;
use crate::task_sync::progress::{ExportCut, ExportProfile, FeedAnchor, TaskChangeCapture};

pub(super) const PROJECT: &str = "feed-changes";

pub(super) fn project() -> ProjectId {
    ProjectId(PROJECT.into())
}

pub(super) fn profile() -> ExportProfile {
    ExportProfile {
        include_notes: false,
        origin_machine: None,
    }
}

pub(super) fn document(index: usize) -> TaskDocument {
    TaskDocument {
        id: WorkId(uuid::Uuid::new_v4()),
        external_ref: None,
        title: format!("task {index}"),
        outcome: format!("Deliver task {index}"),
        acceptance: vec!["delivered".into()],
        acceptance_bindings: Vec::new(),
        evaluation_mode: None,
        kind: WorkItemKind::Task,
        priority: 1,
        labels: Vec::new(),
        assigned_to: None,
        deferred_until: None,
        lifecycle: WorkLifecycle::Open,
        parent_id: None,
        child_requirement: ChildRequirement::Required,
        prerequisites: Vec::new(),
        blockers: Vec::new(),
        superseded_by: None,
        disposal_reason: None,
        completion: None,
        notes: Vec::new(),
        activity: std::collections::BTreeMap::default(),
    }
}

pub(super) fn tree(mut tasks: Vec<TaskDocument>) -> TaskTree {
    tasks.sort_by_key(|task| task.id.0);
    TaskTree {
        project: ProjectDocument {
            format: FORMAT.into(),
            project_id: project(),
            notes_included: false,
        },
        tasks,
    }
}

/// Applies `tree` against the destination's current basis.
pub(super) fn exchange(store: &mut SqliteStore, tree: &TaskTree, key: &str, second: i64) {
    let basis = store
        .export_task_sync_folder(&project(), &profile())
        .unwrap()
        .export_basis;
    store
        .apply_task_sync_folder(
            TaskSyncFolderApply {
                project: &project(),
                actor: &actor("folder-importer"),
                tree,
                export_basis: &basis,
                profile: &profile(),
                complete: true,
                now: at(second),
            },
            key,
        )
        .unwrap();
}

fn changes(store: &SqliteStore, after: &ExportCut) -> TaskChangeCapture {
    store.capture_task_changes(&project(), after).unwrap()
}

fn start() -> ExportCut {
    ExportCut {
        position: 0,
        anchor: None,
    }
}

fn rows(store: &SqliteStore) -> Vec<(String, i64, String)> {
    store
        .connection
        .prepare(
            "SELECT project_id, position, work_id FROM work_project_feed_tasks
             ORDER BY project_id, position, work_id",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn change_capture_names_exactly_the_changed_tasks_of_two_thousand() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let enrolled = tree((0..2000).map(document).collect());
    exchange(&mut store, &enrolled, "enrol", 5);

    // The first exchange adds every member.
    let first = changes(&store, &start());
    assert_eq!(
        first.changed,
        enrolled.tasks.iter().map(|task| task.id).collect()
    );
    // An interval with no change names nothing.
    let unchanged = changes(&store, &first.cut);
    assert!(unchanged.changed.is_empty());
    assert_eq!(unchanged.cut, first.cut);

    // Five edited titles, one added task and one omitted member.
    let mut next = enrolled.clone();
    let mut expected = BTreeSet::new();
    for task in next.tasks.iter_mut().take(5) {
        task.title.push_str(" edited");
        expected.insert(task.id);
    }
    expected.insert(next.tasks.pop().unwrap().id);
    let added = document(2000);
    expected.insert(added.id);
    next.tasks.push(added);
    let next = tree(next.tasks);
    exchange(&mut store, &next, "edit", 6);
    let second = changes(&store, &first.cut);
    assert_eq!(second.changed, expected);

    // A native task joins through its own events alone.
    let native = store
        .create_work(
            &root_request(PROJECT, "native", 7),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let third = changes(&store, &second.cut);
    assert_eq!(third.changed, BTreeSet::from([native.work_id]));

    // The rows written with each append are exactly the rows a rebuild
    // derives, and the doctor agrees.
    let written = rows(&store);
    assert_eq!(
        crate::storage::work::feed_tasks::rebuild(&store.connection).unwrap(),
        Vec::<String>::new()
    );
    assert_eq!(rows(&store), written);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn a_change_capture_decodes_no_record_however_long_its_interval() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &tree(vec![document(0)]), "enrol", 5);
    let cut = changes(&store, &start()).cut;
    // More entries than one interval page, none of them a planning record.
    let mut expected = BTreeSet::new();
    for index in 0..1200 {
        let work = store
            .create_work(
                &root_request(PROJECT, &format!("native-{index}"), 7),
                &crate::DevelopmentNoopRedactor,
            )
            .unwrap();
        expected.insert(work.work_id);
    }
    crate::canonical::reset_canonical_decode_count();
    let capture = changes(&store, &cut);
    assert_eq!(crate::canonical::canonical_decode_count(), 0);
    assert!(capture.cut.position - cut.position >= 1200);
    assert_eq!(capture.changed, expected);
}

#[test]
fn a_basis_that_does_not_continue_the_feed_refuses_by_name() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &tree(vec![document(0)]), "enrol", 5);
    let cut = changes(&store, &start()).cut;
    let ahead = ExportCut {
        position: cut.position + 1,
        anchor: cut.anchor.clone(),
    };
    let moved = ExportCut {
        position: cut.position,
        anchor: Some(FeedAnchor {
            object_kind: "work_event".into(),
            object_id: ObjectId::from_canonical_bytes(b"another object"),
        }),
    };
    for basis in [ahead, moved] {
        let error = store.capture_task_changes(&project(), &basis).unwrap_err();
        assert_eq!(
            refusal_code(&error),
            "task_sync_incremental_basis_unavailable",
            "{error:#}"
        );
    }
}

#[test]
fn the_doctor_names_a_changed_task_row_the_feed_does_not_derive() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &tree(vec![document(0)]), "enrol", 5);
    store
        .connection
        .execute("DELETE FROM work_project_feed_tasks", [])
        .unwrap();
    let report = store.verify_all().unwrap();
    assert!(!report.is_healthy());
    assert!(
        format!("{report:?}").contains("work_project_feed_tasks"),
        "{report:?}"
    );
}

#[test]
fn a_plain_import_refuses_once_the_feed_has_begun_so_no_capture_misses_it() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    // An empty enrolment starts the feed; a capture from zero takes its cut.
    exchange(&mut store, &tree(Vec::new()), "enrol-empty", 5);
    let cut = changes(&store, &start()).cut;
    assert!(cut.position > 0);
    let before = rows(&store);
    let error = store
        .import_task_tree(
            &project(),
            &actor("plain-importer"),
            &tree(vec![document(0)]),
            false,
            at(6),
        )
        .unwrap_err();
    assert_eq!(refusal_code(&error), "task_sync_native_state", "{error:#}");
    // Nothing was written, so the retained cut still names every change.
    assert_eq!(rows(&store), before);
    assert!(changes(&store, &cut).changed.is_empty());
}

#[test]
fn a_first_capture_refuses_tasks_that_no_feed_entry_names() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    // The plain import writes tasks without a project-feed entry, as a
    // graph load does.
    store
        .import_task_tree(
            &project(),
            &actor("plain-importer"),
            &tree(vec![document(0), document(1)]),
            false,
            at(5),
        )
        .unwrap();
    let error = store
        .capture_task_changes(&project(), &start())
        .unwrap_err();
    assert_eq!(
        refusal_code(&error),
        "task_sync_incremental_basis_unavailable",
        "{error:#}"
    );
}
