use super::super::folder::TaskSyncFolderApply;
use super::*;
use crate::task_sync::notes::{SharedNote, SharedNoteKind};
use crate::task_sync::progress::documents_fingerprint;

fn profile() -> crate::task_sync::progress::ExportProfile {
    crate::task_sync::progress::ExportProfile {
        include_notes: false,
        origin_machine: None,
    }
}

fn destination_basis(store: &SqliteStore, project: &ProjectId) -> String {
    store
        .work_read_snapshot(|store| {
            let current = super::super::export_on(&store.connection, project)?;
            Ok(documents_fingerprint(&current.documents()?))
        })
        .unwrap()
}

fn folder_apply_request<'a>(
    project: &'a ProjectId,
    actor: &'a crate::ActorContext,
    tree: &'a TaskTree,
    export_basis: &'a str,
    profile: &'a crate::task_sync::progress::ExportProfile,
    complete: bool,
    now: chrono::DateTime<chrono::Utc>,
) -> TaskSyncFolderApply<'a> {
    TaskSyncFolderApply {
        project,
        actor,
        tree,
        export_basis,
        profile,
        complete,
        now,
    }
}

fn apply_folder(
    store: &mut SqliteStore,
    tree: &TaskTree,
    key: &str,
    now: chrono::DateTime<chrono::Utc>,
) -> TaskSyncImportResult {
    let basis = destination_basis(store, &tree.project.project_id);
    store
        .apply_task_sync_folder(
            folder_apply_request(
                &tree.project.project_id,
                &actor("folder-importer"),
                tree,
                &basis,
                &profile(),
                true,
                now,
            ),
            key,
        )
        .unwrap()
}

#[test]
fn task_sync_folder_apply_records_the_complete_baseline_and_exact_repeat_is_a_noop() {
    let tree = candidate();
    let mut store = SqliteStore::open_in_memory().unwrap();
    let basis = destination_basis(&store, &tree.project.project_id);
    let first = store
        .apply_task_sync_folder(
            folder_apply_request(
                &tree.project.project_id,
                &actor("folder-importer"),
                &tree,
                &basis,
                &profile(),
                true,
                at(5),
            ),
            "folder-import-1",
        )
        .unwrap();
    assert!(first.changed);

    let exported = store
        .export_task_sync_folder(&tree.project.project_id, &profile())
        .unwrap();
    assert_eq!(
        exported.tree.documents().unwrap(),
        tree.documents().unwrap()
    );
    assert_eq!(
        exported.export_basis,
        documents_fingerprint(&tree.documents().unwrap())
    );
    assert!(store.verify_all().unwrap().is_healthy());

    let before = state(&store);
    let repeated = store
        .apply_task_sync_folder(
            folder_apply_request(
                &tree.project.project_id,
                &actor("folder-importer"),
                &tree,
                &basis,
                &profile(),
                true,
                at(6),
            ),
            "folder-import-1",
        )
        .unwrap();
    assert_eq!(repeated, first);
    assert_eq!(state(&store), before);
}

#[test]
fn task_sync_folder_reuses_a_key_only_for_the_identical_request() {
    let tree = candidate();
    let mut store = SqliteStore::open_in_memory().unwrap();
    let basis = destination_basis(&store, &tree.project.project_id);
    apply_folder(&mut store, &tree, "folder-key-conflict", at(5));
    let before = state(&store);
    let mut changed = tree.clone();
    changed.tasks[0].title = "different request under the same key".into();
    let error = store
        .apply_task_sync_folder(
            folder_apply_request(
                &tree.project.project_id,
                &actor("folder-importer"),
                &changed,
                &basis,
                &profile(),
                true,
                at(6),
            ),
            "folder-key-conflict",
        )
        .unwrap_err();
    assert!(matches!(
        error,
        StoreError::WorkOperationIdempotencyConflict { .. }
    ));
    assert_eq!(state(&store), before);
}

#[test]
fn task_sync_folder_fresh_key_noop_does_not_append_a_planning_baseline() {
    let tree = candidate();
    let mut store = SqliteStore::open_in_memory().unwrap();
    let basis = destination_basis(&store, &tree.project.project_id);
    let first = store
        .apply_task_sync_folder(
            folder_apply_request(
                &tree.project.project_id,
                &actor("folder-importer"),
                &tree,
                &basis,
                &profile(),
                true,
                at(5),
            ),
            "folder-fresh-noop-first",
        )
        .unwrap();
    assert!(first.changed);
    let planning_before: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM work_feed_entries
         WHERE feed_kind = 'project' AND feed_id = ?1 AND object_kind = ?2",
            rusqlite::params![
                tree.project.project_id.0,
                super::super::folder::IMPORT_PLANNING_KIND
            ],
            |row| row.get(0),
        )
        .unwrap();
    let result = store
        .apply_task_sync_folder(
            folder_apply_request(
                &tree.project.project_id,
                &actor("folder-importer"),
                &tree,
                &basis,
                &profile(),
                true,
                at(6),
            ),
            "folder-fresh-noop-second",
        )
        .unwrap();
    assert!(!result.changed);
    let planning_after: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM work_feed_entries
         WHERE feed_kind = 'project' AND feed_id = ?1 AND object_kind = ?2",
            rusqlite::params![
                tree.project.project_id.0,
                super::super::folder::IMPORT_PLANNING_KIND
            ],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(planning_after, planning_before);
}

#[test]
fn task_sync_folder_notes_from_another_machine_round_trip_and_fresh_key_is_a_noop() {
    let mut tree = candidate();
    tree.project.notes_included = true;
    tree.tasks[0].notes = vec![SharedNote {
        id: CanonicalObject::mint(&"source-host status")
            .unwrap()
            .key()
            .clone(),
        kind: SharedNoteKind::Status,
        body: "Source-host status".into(),
        author: SharedActor::from_actor(&actor("source-author")),
        recorded_at: at(1),
        origin_machine: "source-host".into(),
    }];
    let profile = crate::task_sync::progress::ExportProfile {
        include_notes: true,
        origin_machine: Some("destination-host".into()),
    };
    let mut store = SqliteStore::open_in_memory().unwrap();
    let basis = store
        .export_task_sync_folder(&tree.project.project_id, &profile)
        .unwrap()
        .export_basis;
    let first = store
        .apply_task_sync_folder(
            folder_apply_request(
                &tree.project.project_id,
                &actor("folder-importer"),
                &tree,
                &basis,
                &profile,
                true,
                at(5),
            ),
            "folder-notes-origin-first",
        )
        .unwrap();
    assert!(first.changed);

    let exported = store
        .export_task_sync_folder(&tree.project.project_id, &profile)
        .unwrap();
    assert_eq!(
        exported.tree.folder_documents().unwrap(),
        tree.folder_documents().unwrap()
    );
    assert_eq!(
        exported.tree.tasks[0].notes[0].origin_machine,
        "source-host"
    );

    let planning_before: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM work_feed_entries
         WHERE feed_kind = 'project' AND feed_id = ?1 AND object_kind = ?2",
            rusqlite::params![
                tree.project.project_id.0,
                super::super::folder::IMPORT_PLANNING_KIND
            ],
            |row| row.get(0),
        )
        .unwrap();
    let before = state(&store);
    let operation_results_before: i64 = store
        .connection
        .query_row("SELECT COUNT(*) FROM work_operation_results", [], |row| {
            row.get(0)
        })
        .unwrap();
    let repeated = store
        .apply_task_sync_folder(
            folder_apply_request(
                &tree.project.project_id,
                &actor("folder-importer"),
                &tree,
                &basis,
                &profile,
                true,
                at(6),
            ),
            "folder-notes-origin-second",
        )
        .unwrap();
    assert!(!repeated.changed);
    let receipt: Vec<u8> = store
        .connection
        .query_row(
            "SELECT result_json FROM work_operation_results
             WHERE operation = ?1 AND idempotency_key = ?2",
            rusqlite::params!["apply_task_sync_folder", "folder-notes-origin-second"],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        serde_json::from_slice::<TaskSyncImportResult>(&receipt).unwrap(),
        repeated
    );
    let operation_results_after: i64 = store
        .connection
        .query_row("SELECT COUNT(*) FROM work_operation_results", [], |row| {
            row.get(0)
        })
        .unwrap();
    assert_eq!(operation_results_after, operation_results_before + 1);
    let planning_after: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM work_feed_entries
         WHERE feed_kind = 'project' AND feed_id = ?1 AND object_kind = ?2",
            rusqlite::params![
                tree.project.project_id.0,
                super::super::folder::IMPORT_PLANNING_KIND
            ],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(planning_after, planning_before);
    assert_eq!(
        store
            .connection
            .execute(
                "DELETE FROM work_operation_results
                 WHERE operation = ?1 AND idempotency_key = ?2",
                rusqlite::params!["apply_task_sync_folder", "folder-notes-origin-second"],
            )
            .unwrap(),
        1
    );
    assert_eq!(state(&store), before);
}

#[test]
fn task_sync_folder_fresh_key_noop_requires_admitted_actor() {
    let tree = candidate();
    let mut store = SqliteStore::open_in_memory().unwrap();
    let basis = destination_basis(&store, &tree.project.project_id);
    apply_folder(
        &mut store,
        &tree,
        "folder-fresh-noop-admission-first",
        at(5),
    );

    let mut invalid_actor = actor("folder-importer");
    invalid_actor.actor_id.clear();
    let before = state(&store);
    let error = store
        .apply_task_sync_folder(
            folder_apply_request(
                &tree.project.project_id,
                &invalid_actor,
                &tree,
                &basis,
                &profile(),
                true,
                at(6),
            ),
            "folder-fresh-noop-invalid-actor",
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("attribution is required"),
        "{error:#}"
    );
    assert_eq!(state(&store), before);
}

#[test]
fn task_sync_folder_apply_refuses_missing_envelope_guards_without_writes() {
    let tree = candidate();
    let mut wrong_project_tree = tree.clone();
    wrong_project_tree.project.project_id = ProjectId("wrong-project".into());
    let mut store = SqliteStore::open_in_memory().unwrap();
    let basis = destination_basis(&store, &tree.project.project_id);
    let before = state(&store);
    let actor = actor("folder-importer");
    let profile = profile();
    let cases = [
        (
            "folder-incomplete-envelope",
            TaskSyncFolderApply {
                project: &tree.project.project_id,
                actor: &actor,
                tree: &tree,
                export_basis: &basis,
                profile: &profile,
                complete: false,
                now: at(5),
            },
            "task_sync_invalid_graph",
        ),
        (
            "folder-empty-basis",
            TaskSyncFolderApply {
                project: &tree.project.project_id,
                actor: &actor,
                tree: &tree,
                export_basis: "",
                profile: &profile,
                complete: true,
                now: at(5),
            },
            "task_sync_divergent_planning",
        ),
        (
            "folder-wrong-project",
            TaskSyncFolderApply {
                project: &tree.project.project_id,
                actor: &actor,
                tree: &wrong_project_tree,
                export_basis: &basis,
                profile: &profile,
                complete: true,
                now: at(5),
            },
            "task_sync_origin_mismatch",
        ),
    ];
    for (key, request, code) in cases {
        let error = store.apply_task_sync_folder(request, key).unwrap_err();
        assert_eq!(refusal_code(&error), code, "{error:#}");
        assert_eq!(state(&store), before);
    }
    let error = store
        .apply_task_sync_folder(
            folder_apply_request(
                &tree.project.project_id,
                &actor,
                &tree,
                &basis,
                &profile,
                true,
                at(5),
            ),
            "",
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("idempotency key is required"),
        "{error:#}"
    );
    assert_eq!(state(&store), before);
}

#[test]
fn task_sync_folder_refuses_a_profile_that_does_not_match_the_tree() {
    let tree = candidate();
    let mut store = SqliteStore::open_in_memory().unwrap();
    let before = state(&store);
    let basis = destination_basis(&store, &tree.project.project_id);
    let error = store
        .apply_task_sync_folder(
            folder_apply_request(
                &tree.project.project_id,
                &actor("folder-importer"),
                &tree,
                &basis,
                &crate::task_sync::progress::ExportProfile {
                    include_notes: true,
                    origin_machine: Some("host-a".into()),
                },
                true,
                at(5),
            ),
            "folder-import-profile-mismatch",
        )
        .unwrap_err();
    assert!(error.to_string().contains("profile and tree disclosure"));
    assert_eq!(state(&store), before);
}

#[test]
fn task_sync_folder_empty_complete_tree_enrolls_a_baseline() {
    let mut tree = candidate();
    tree.tasks.clear();
    let mut store = SqliteStore::open_in_memory().unwrap();

    let basis = destination_basis(&store, &tree.project.project_id);
    let first = store
        .apply_task_sync_folder(
            folder_apply_request(
                &tree.project.project_id,
                &actor("folder-importer"),
                &tree,
                &basis,
                &profile(),
                true,
                at(5),
            ),
            "empty-folder-import-1",
        )
        .unwrap();
    assert!(first.changed);
    assert_eq!(
        store
            .export_task_sync_folder(&tree.project.project_id, &profile())
            .unwrap()
            .tree
            .documents()
            .unwrap(),
        tree.documents().unwrap()
    );

    let before = state(&store);
    assert_eq!(
        store
            .apply_task_sync_folder(
                folder_apply_request(
                    &tree.project.project_id,
                    &actor("folder-importer"),
                    &tree,
                    &basis,
                    &profile(),
                    true,
                    at(5),
                ),
                "empty-folder-import-1",
            )
            .unwrap(),
        first
    );
    assert_eq!(state(&store), before);
}

#[test]
fn task_sync_folder_empty_note_profile_exports_bootstraps_and_replays() {
    let mut tree = candidate();
    tree.tasks.clear();
    tree.project.notes_included = true;
    let profile = crate::task_sync::progress::ExportProfile {
        include_notes: true,
        origin_machine: Some("host-a".into()),
    };
    let mut store = SqliteStore::open_in_memory().unwrap();

    let basis = documents_fingerprint(&tree.folder_documents().unwrap());
    let empty = store
        .export_task_sync_folder(&tree.project.project_id, &profile)
        .unwrap();
    assert_eq!(
        empty.tree.folder_documents().unwrap(),
        tree.folder_documents().unwrap()
    );
    assert_eq!(empty.export_basis, basis);
    assert!(empty.omissions.counts.is_empty());
    assert!(empty.activity_omissions.counts.is_empty());

    let first = store
        .apply_task_sync_folder(
            folder_apply_request(
                &tree.project.project_id,
                &actor("folder-importer"),
                &tree,
                &basis,
                &profile,
                true,
                at(5),
            ),
            "empty-note-folder-import-1",
        )
        .unwrap();
    assert!(first.changed);

    let planning_count_before: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM work_feed_entries
             WHERE feed_kind = 'project' AND feed_id = ?1 AND object_kind = ?2",
            rusqlite::params![
                tree.project.project_id.0,
                super::super::folder::IMPORT_PLANNING_KIND,
            ],
            |row| row.get(0),
        )
        .unwrap();
    let repeated = store
        .apply_task_sync_folder(
            folder_apply_request(
                &tree.project.project_id,
                &actor("folder-importer"),
                &tree,
                &basis,
                &profile,
                true,
                at(6),
            ),
            "empty-note-folder-import-2",
        )
        .unwrap();
    assert!(!repeated.changed);
    let planning_count_after: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM work_feed_entries
             WHERE feed_kind = 'project' AND feed_id = ?1 AND object_kind = ?2",
            rusqlite::params![
                tree.project.project_id.0,
                super::super::folder::IMPORT_PLANNING_KIND,
            ],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(planning_count_after, planning_count_before);
}

#[test]
fn task_sync_folder_export_retains_visible_note_omissions() {
    use crate::domain::{RecordWorkObservationRequest, SessionId};

    let mut store = SqliteStore::open_in_memory().unwrap();
    let work = store
        .create_work(
            &root_request("folder-omissions", "capture omissions", 0),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    store
        .record_work_observation(
            &RecordWorkObservationRequest {
                status: true,
                project_id: work.project_id.clone(),
                work_id: work.work_id,
                expected_work_revision: work.revision,
                session_id: SessionId("folder-observer".into()),
                summary: "Local reference is deliberately omitted".into(),
                refs: vec!["local-evidence".into()],
                actor: crate::domain::non_holder_note_actor(actor("folder-observer")),
                idempotency_key: "folder-omission-note".into(),
                recorded_at: at(1),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let profile = crate::task_sync::progress::ExportProfile {
        include_notes: true,
        origin_machine: Some("host-a".into()),
    };

    let export = store
        .export_task_sync_folder(&work.project_id, &profile)
        .unwrap();
    assert_eq!(export.omissions.counts["references"], 1);
    assert!(export.activity_omissions.counts.is_empty());
    assert_eq!(export.tree.tasks[0].notes, Vec::new());
}

#[test]
fn task_sync_folder_exports_a_no_history_source_and_round_trips_its_inventory() {
    let tree = candidate();
    let mut source = SqliteStore::open_in_memory().unwrap();
    source
        .import_task_tree(
            &tree.project.project_id,
            &actor("legacy-source-importer"),
            &tree,
            false,
            at(5),
        )
        .unwrap();

    let exported = source
        .export_task_sync_folder(&tree.project.project_id, &profile())
        .unwrap();
    assert_eq!(
        exported.tree.documents().unwrap(),
        tree.documents().unwrap()
    );
    assert_eq!(
        exported.export_basis,
        documents_fingerprint(&exported.tree.documents().unwrap())
    );

    let mut destination = SqliteStore::open_in_memory().unwrap();
    let basis = destination_basis(&destination, &tree.project.project_id);
    destination
        .apply_task_sync_folder(
            folder_apply_request(
                &tree.project.project_id,
                &actor("folder-destination-importer"),
                &exported.tree,
                &basis,
                &exported.profile,
                true,
                at(6),
            ),
            "folder-no-history-round-trip",
        )
        .unwrap();
    assert_eq!(
        destination
            .export_task_sync_folder(&tree.project.project_id, &profile())
            .unwrap()
            .tree
            .documents()
            .unwrap(),
        exported.tree.documents().unwrap()
    );
}

#[test]
fn task_sync_folder_refuses_a_corrupt_history_source() {
    let tree = candidate();
    let mut store = SqliteStore::open_in_memory().unwrap();
    apply_folder(&mut store, &tree, "folder-import-1", at(5));
    store
        .connection
        .execute(
            "UPDATE objects SET canonical_json = ?1
             WHERE object_id = (
                 SELECT object_id FROM work_feed_entries
                 WHERE feed_kind = 'project' AND feed_id = ?2
                   AND object_kind = 'task_sync_import_planning'
                 ORDER BY position DESC LIMIT 1
             )",
            rusqlite::params![b"not valid JSON".to_vec(), tree.project.project_id.0],
        )
        .unwrap();

    let error = store
        .export_task_sync_folder(&tree.project.project_id, &profile())
        .unwrap_err();
    assert!(matches!(error, StoreError::Json(_)), "{error:#}");
}

#[test]
fn task_sync_folder_integrity_rejects_a_planning_record_on_the_wrong_project_feed() {
    let tree = candidate();
    let mut store = SqliteStore::open_in_memory().unwrap();
    apply_folder(&mut store, &tree, "folder-import-1", at(5));
    store
        .connection
        .execute(
            "INSERT INTO work_feed_heads (feed_kind, feed_id, position)
             VALUES ('project', 'wrong-project', 0)",
            [],
        )
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE work_feed_entries SET feed_id = 'wrong-project'
             WHERE feed_kind = 'project' AND feed_id = ?1 AND object_kind = ?2",
            rusqlite::params![
                tree.project.project_id.0,
                super::super::folder::IMPORT_PLANNING_KIND,
            ],
        )
        .unwrap();
    let report = store.verify_all().unwrap();
    assert!(!report.is_healthy(), "{report:?}");
    assert!(
        format!("{report:?}").contains("wrong_membership"),
        "{report:?}"
    );
}

#[test]
fn task_sync_folder_integrity_rejects_an_unfed_planning_record() {
    let tree = candidate();
    let mut store = SqliteStore::open_in_memory().unwrap();
    apply_folder(&mut store, &tree, "folder-import-1", at(5));
    store
        .connection
        .execute(
            "DELETE FROM work_feed_entries
             WHERE feed_kind = 'project' AND feed_id = ?1 AND object_kind = ?2",
            rusqlite::params![
                tree.project.project_id.0,
                super::super::folder::IMPORT_PLANNING_KIND,
            ],
        )
        .unwrap();
    let report = store.verify_all().unwrap();
    assert!(!report.is_healthy(), "{report:?}");
    assert!(
        format!("{report:?}").contains(super::super::folder::IMPORT_PLANNING_KIND),
        "{report:?}"
    );
}

#[test]
fn task_sync_folder_export_projects_current_planning_and_local_additions() {
    let tree = candidate();
    let mut store = SqliteStore::open_in_memory().unwrap();
    apply_folder(&mut store, &tree, "folder-import-1", at(5));

    let member = tree
        .tasks
        .iter()
        .find(|task| task.lifecycle == WorkLifecycle::Open)
        .unwrap();
    let current = query::load_work_item(&store.connection, member.id).unwrap();
    store
        .revise_work(
            &crate::ReviseWorkRequest {
                work_id: current.work_id,
                expected_revision: current.revision,
                patch: crate::WorkRevisionPatch {
                    title: Some("Local planning edit".into()),
                    ..Default::default()
                },
                authority: crate::WorkPlanningAuthority::Project,
                actor: actor("local-planner"),
                idempotency_key: "local-folder-edit".into(),
                updated_at: at(6),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let mut local_request = root_request(
        &tree.project.project_id.0,
        "Local collaboration addition",
        6,
    );
    local_request.title = "Local collaboration addition".into();
    let local = store
        .create_work(&local_request, &crate::DevelopmentNoopRedactor)
        .unwrap();

    let exported = store
        .export_task_sync_folder(&tree.project.project_id, &profile())
        .unwrap();
    assert_eq!(exported.tree.tasks.len(), tree.tasks.len() + 1);
    assert_eq!(
        exported
            .tree
            .tasks
            .iter()
            .find(|task| task.id == member.id)
            .unwrap()
            .title,
        "Local planning edit"
    );
    assert!(
        exported.tree.tasks.iter().any(|task| {
            task.id == local.work_id && task.title == "Local collaboration addition"
        })
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn task_sync_folder_apply_refuses_a_local_edit_after_export() {
    let tree = candidate();
    let mut store = SqliteStore::open_in_memory().unwrap();
    apply_folder(&mut store, &tree, "folder-import-1", at(5));
    let item = query::load_work_item(
        &store.connection,
        tree.tasks
            .iter()
            .find(|task| task.lifecycle == WorkLifecycle::Open)
            .expect("fixture includes an open task")
            .id,
    )
    .unwrap();
    let export_basis = destination_basis(&store, &tree.project.project_id);
    store
        .revise_work(
            &crate::ReviseWorkRequest {
                work_id: item.work_id,
                expected_revision: item.revision,
                patch: crate::WorkRevisionPatch {
                    title: Some("Local edit after export".into()),
                    ..Default::default()
                },
                authority: crate::WorkPlanningAuthority::Project,
                actor: actor("local-planner"),
                idempotency_key: "local-edit-after-export".into(),
                updated_at: at(6),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let before = state(&store);
    let error = store
        .apply_task_sync_folder(
            folder_apply_request(
                &tree.project.project_id,
                &actor("folder-importer"),
                &tree,
                &export_basis,
                &profile(),
                true,
                at(7),
            ),
            "folder-import-2",
        )
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("locally changed planning inventory")
    );
    assert_eq!(refusal_code(&error), "task_sync_divergent_planning");
    assert_eq!(state(&store), before);
}

#[test]
fn task_sync_folder_export_captures_a_local_edit_without_self_applying() {
    let tree = candidate();
    let mut store = SqliteStore::open_in_memory().unwrap();
    apply_folder(&mut store, &tree, "folder-import-1", at(5));
    let item = query::load_work_item(
        &store.connection,
        tree.tasks
            .iter()
            .find(|task| task.lifecycle == WorkLifecycle::Open)
            .expect("fixture includes an open task")
            .id,
    )
    .unwrap();
    store
        .revise_work(
            &crate::ReviseWorkRequest {
                work_id: item.work_id,
                expected_revision: item.revision,
                patch: crate::WorkRevisionPatch {
                    title: Some("Local edit captured before export".into()),
                    ..Default::default()
                },
                authority: crate::WorkPlanningAuthority::Project,
                actor: actor("local-planner"),
                idempotency_key: "local-edit-before-export".into(),
                updated_at: at(6),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let export = store
        .export_task_sync_folder(&tree.project.project_id, &profile())
        .unwrap();
    assert_eq!(
        export
            .tree
            .tasks
            .iter()
            .find(|task| task.id == item.work_id)
            .expect("export retains the edited task")
            .title,
        "Local edit captured before export"
    );

    // An enrolled store reconciles its own export as a folder that changes no
    // task: the edited task gains no event, and the baseline now carries the
    // local edit, so the next exchange compares against it.
    let events_before =
        query::canonical_work_event_records_for_item(&store.connection, item.work_id)
            .unwrap()
            .len();
    let result = store
        .apply_task_sync_folder(
            folder_apply_request(
                &tree.project.project_id,
                &actor("folder-importer"),
                &export.tree,
                &export.export_basis,
                &export.profile,
                true,
                at(7),
            ),
            "folder-import-pre-export-edit",
        )
        .unwrap();
    assert!(result.changed);
    assert_eq!(
        query::canonical_work_event_records_for_item(&store.connection, item.work_id)
            .unwrap()
            .len(),
        events_before
    );
    assert_eq!(
        store
            .export_task_sync_folder(&tree.project.project_id, &profile())
            .unwrap()
            .tree,
        export.tree
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn task_sync_folder_refuses_a_deletion_that_leaves_a_dangling_relation() {
    let tree = candidate();
    let removed = tree
        .tasks
        .iter()
        .find(|task| task.lifecycle == WorkLifecycle::Completed)
        .expect("fixture includes a completed prerequisite")
        .id;
    let mut store = SqliteStore::open_in_memory().unwrap();
    apply_folder(&mut store, &tree, "folder-import-1", at(5));

    let mut resolved = tree.clone();
    resolved.tasks.retain(|task| task.id != removed);
    let before = state(&store);
    let basis = destination_basis(&store, &tree.project.project_id);
    let error = store
        .apply_task_sync_folder(
            folder_apply_request(
                &tree.project.project_id,
                &actor("folder-importer"),
                &resolved,
                &basis,
                &profile(),
                true,
                at(6),
            ),
            "folder-import-dangling-deletion",
        )
        .unwrap_err();
    assert!(
        error.to_string().contains("dangling work relation"),
        "{error:#}"
    );
    assert_eq!(refusal_code(&error), "task_sync_invalid_graph");
    assert_eq!(state(&store), before);
}

#[test]
fn task_sync_folder_seeded_retirement_filter_omits_deleted_member_but_keeps_local_work() {
    let tree = candidate();
    let removed = tree
        .tasks
        .iter()
        .find(|task| task.parent_id.is_some())
        .expect("fixture includes a child without incoming references")
        .id;
    let retained = tree
        .tasks
        .iter()
        .find(|task| task.id != removed)
        .expect("fixture includes a retained task")
        .id;
    let mut store = SqliteStore::open_in_memory().unwrap();
    apply_folder(&mut store, &tree, "folder-import-1", at(5));

    let mut resolved = tree.clone();
    resolved.tasks.retain(|task| task.id != removed);
    // A synthetic complete baseline models an already accepted retirement.
    // Deletion admission happens in a later exchange stage; this verifies
    // export filtering only.
    super::super::folder::record_complete_baseline_for_test(&mut store, &resolved).unwrap();

    let mut request = root_request(
        &tree.project.project_id.0,
        "Local-first collaboration task",
        6,
    );
    request.title = "Local-first collaboration task".into();
    let local = store
        .create_work(&request, &crate::DevelopmentNoopRedactor)
        .unwrap();
    let exported = store
        .export_task_sync_folder(&tree.project.project_id, &profile())
        .unwrap();
    assert!(exported.tree.tasks.iter().any(|task| task.id == retained));
    assert!(!exported.tree.tasks.iter().any(|task| task.id == removed));
    assert!(
        exported
            .tree
            .tasks
            .iter()
            .any(|task| task.id == local.work_id)
    );
}

#[test]
fn task_sync_folder_apply_refuses_a_partial_baseline_without_deleting_local_work() {
    let tree = candidate();
    let mut store = SqliteStore::open_in_memory().unwrap();
    apply_folder(&mut store, &tree, "folder-import-1", at(5));

    // Refresh the synthetic baseline so this fixture reaches the partial-input
    // guard rather than the earlier freshness guard.
    let baseline = store
        .export_task_sync_folder(&tree.project.project_id, &profile())
        .unwrap();
    super::super::folder::record_complete_baseline_for_test(&mut store, &baseline.tree).unwrap();

    let before = state(&store);
    let mut partial = tree;
    // Keep the graph valid so the partial-inventory guard, rather than
    // structural validation, is the behavior under test. `candidate()` sorts
    // random UUIDs, so `pop()` could remove a referenced root or prerequisite.
    partial
        .tasks
        .retain(|task| task.lifecycle != WorkLifecycle::Proposed);
    let basis = destination_basis(&store, &partial.project.project_id);
    let error = store
        .apply_task_sync_folder(
            folder_apply_request(
                &partial.project.project_id,
                &actor("folder-importer"),
                &partial,
                &basis,
                &profile(),
                true,
                at(6),
            ),
            "folder-import-2",
        )
        .unwrap_err();
    // An enrolled store sees the omitted members as deletions, which this
    // exchange refuses by name rather than applying the rest.
    assert!(error.to_string().contains("task_deleted"), "{error:#}");
    assert_eq!(refusal_code(&error), "task_sync_import_conflict");
    assert_eq!(state(&store), before);
}
