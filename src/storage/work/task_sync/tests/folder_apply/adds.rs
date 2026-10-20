//! Tasks a resolved folder adds to an enrolled destination.

use super::*;
use crate::{
    WorkItem,
    domain::{AcceptanceBinding, VerificationKind, VerificationRequirement},
    task_sync::{
        SharedBlocker,
        notes::{SharedNote, SharedNoteKind},
    },
};

fn with(tree: &TaskTree, added: Vec<TaskDocument>) -> TaskTree {
    let mut tree = tree.clone();
    tree.tasks.extend(added);
    tree.tasks.sort_by_key(|task| task.id.0);
    tree
}

fn item(store: &SqliteStore, id: WorkId) -> WorkItem {
    query::load_work_item(&store.connection, id).unwrap()
}

fn completed_document(title: &str, parent: Option<WorkId>) -> TaskDocument {
    let mut task = document(title, parent);
    task.lifecycle = WorkLifecycle::Completed;
    task.completion = Some(SharedCompletion {
        summary: format!("{title} delivered elsewhere"),
        completed_at: at(3),
        actor: SharedActor::from_actor(&actor("remote-author")),
    });
    task
}

fn native_root(store: &mut SqliteStore, project: &str, key: &str, second: i64) -> WorkItem {
    store
        .create_work(
            &root_request(project, key, second),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap()
}

#[test]
fn added_tasks_restore_like_an_import_beside_an_edit() {
    let (mut store, fixture) = enrolled();
    let durable_before = durable(&store).len();
    let root = document("added root", None);
    let child = document("added child", Some(fixture.parent));
    let (root_id, child_id) = (root.id, child.id);
    let resolved = with(
        &edited(&fixture.tree, fixture.first, |task| {
            task.title = "Renamed first".into();
        }),
        vec![root, child],
    );
    let result = exchange(&mut store, &resolved, "add-1", 6).unwrap();
    assert!(result.changed && !result.dry_run);
    for id in [root_id, child_id] {
        let item = item(&store, id);
        assert!(item.restored, "{id:?}");
        assert_eq!(item.active_run_id, None);
        assert_eq!(item.lifecycle, WorkLifecycle::Open);
        assert_eq!(item.created_by.actor_id, "folder-importer");
        assert_eq!(events(&store, id), Vec::new());
    }
    assert_eq!(item(&store, child_id).root_id, fixture.parent);
    for root in [root_id, fixture.parent] {
        assert!(
            query::active_root_execution_optional(&store.connection, root)
                .unwrap()
                .is_none()
        );
    }
    // The edit applied in the same transaction.
    assert_eq!(events(&store, fixture.first).len(), 1);
    // Each addition has its own import origin and generation-zero record.
    assert_eq!(durable(&store).len(), durable_before + 4);
    assert_eq!(exported(&store), resolved);
    assert!(store.verify_all().unwrap().is_healthy());

    // The additions joined the baseline: the same folder again changes
    // nothing, and a later edit of an added task applies.
    assert!(!exchange(&mut store, &resolved, "add-2", 7).unwrap().changed);
    let renamed = edited(&resolved, child_id, |task| {
        task.title = "Renamed added child".into();
    });
    assert!(
        exchange(&mut store, &renamed, "edit-added", 8)
            .unwrap()
            .changed
    );
    assert_eq!(events(&store, child_id).len(), 1);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn a_completed_addition_keeps_its_reported_completion_without_a_seal() {
    let (mut store, fixture) = enrolled();
    let done = completed_document("reported done", None);
    let history = completed_document("completed history", Some(fixture.completed));
    let ids = [done.id, history.id];
    let resolved = with(&fixture.tree, vec![done, history]);
    assert!(exchange(&mut store, &resolved, "add-1", 6).unwrap().changed);
    for id in ids {
        let item = item(&store, id);
        assert_eq!(item.lifecycle, WorkLifecycle::Completed);
        assert_eq!(item.active_run_id, None);
        assert_eq!(events(&store, id), Vec::new());
    }
    assert_eq!(exported(&store), resolved);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn unfinished_work_under_a_completed_ancestor_refuses() {
    let (mut store, fixture) = enrolled();
    let before = state(&store);
    let late = document("late work", Some(fixture.completed));
    let late_id = late.id;
    let resolved = with(&fixture.tree, vec![late]);
    let message = conflict(&exchange(&mut store, &resolved, "add-1", 6).unwrap_err());
    assert!(
        message.contains(&format!(
            "{}: ancestor_binding (ancestor {} is completed)",
            late_id.0, fixture.completed.0
        )),
        "{message}"
    );
    assert_eq!(state(&store), before);
}

#[test]
fn an_addition_under_a_claimed_parent_refuses_and_a_dry_run_refuses_alike() {
    let (mut store, fixture) = enrolled();
    let claim = claim_restored(&mut store, fixture.parent, "worker", 6);
    let child = document("added child", Some(fixture.parent));
    let child_id = child.id;
    let resolved = with(&fixture.tree, vec![child]);
    let basis = basis(&store);
    let project = project();
    let importer = actor("folder-importer");
    let profile = profile();
    let request = TaskSyncFolderApply {
        project: &project,
        actor: &importer,
        tree: &resolved,
        export_basis: &basis,
        profile: &profile,
        complete: true,
        now: at(7),
    };
    let before = state(&store);
    let dry = store
        .dry_run_task_sync_folder(request, "add-1")
        .unwrap_err();
    let real = store.apply_task_sync_folder(request, "add-1").unwrap_err();
    assert_eq!(dry.to_string(), real.to_string());
    let message = conflict(&real);
    assert!(
        message.contains(&format!(
            "{}: ancestor_binding (ancestor {}: active_run (run {}))",
            child_id.0, fixture.parent.0, claim.run_id.0
        )),
        "{message}"
    );
    assert_eq!(state(&store), before);
}

#[test]
fn an_executing_root_refuses_an_addition_though_its_ancestors_are_run_free() {
    // Claiming one child starts the root's execution while the parent and
    // the other child stay run-free.
    let (mut store, fixture) = enrolled();
    claim_restored(&mut store, fixture.first, "worker", 6);
    let grandchild = document("added grandchild", Some(fixture.second));
    let grandchild_id = grandchild.id;
    let resolved = with(&fixture.tree, vec![grandchild]);
    let before = state(&store);
    let message = conflict(&exchange(&mut store, &resolved, "add-1", 7).unwrap_err());
    assert!(
        message.contains(&format!(
            "{}: ancestor_binding (root {} has active root execution ",
            grandchild_id.0, fixture.parent.0
        )),
        "{message}"
    );
    assert_eq!(state(&store), before);

    // A new root is outside that execution and is restored.
    let unrelated = with(&fixture.tree, vec![document("unrelated root", None)]);
    assert!(
        exchange(&mut store, &unrelated, "add-2", 8)
            .unwrap()
            .changed
    );
}

#[test]
fn blocked_additions_and_edits_are_named_together() {
    let (mut store, fixture) = enrolled();
    let claim = claim_restored(&mut store, fixture.first, "worker", 6);
    let grandchild = document("added grandchild", Some(fixture.second));
    let grandchild_id = grandchild.id;
    let resolved = with(
        &edited(&fixture.tree, fixture.first, |task| {
            task.title = "Renamed first".into();
        }),
        vec![grandchild],
    );
    let before = state(&store);
    let message = conflict(&exchange(&mut store, &resolved, "add-1", 7).unwrap_err());
    let edit = format!("{}: active_run (run {})", fixture.first.0, claim.run_id.0);
    let addition = format!("{}: ancestor_binding (root ", grandchild_id.0);
    let (Some(edit_at), Some(addition_at)) = (message.find(&edit), message.find(&addition)) else {
        panic!("both conflicts are named: {message}");
    };
    assert_eq!(
        edit_at < addition_at,
        fixture.first.0 < grandchild_id.0,
        "{message}"
    );
    assert_eq!(state(&store), before);
}

#[test]
fn an_addition_referencing_a_native_task_refuses() {
    let (mut store, _fixture) = enrolled();
    let native = native_root(&mut store, PROJECT, "native-task", 6);
    let child = document("under native", Some(native.work_id));
    let mut dependent = document("needs native", None);
    dependent.prerequisites = vec![native.work_id];
    let (child_id, dependent_id) = (child.id, dependent.id);
    let resolved = with(&exported(&store), vec![child, dependent]);
    let before = state(&store);
    let message = conflict(&exchange(&mut store, &resolved, "add-1", 7).unwrap_err());
    for id in [child_id, dependent_id] {
        assert!(
            message.contains(&format!(
                "{}: unsupported_origin (it references task {}",
                id.0, native.work_id.0
            )),
            "{message}"
        );
    }
    assert_eq!(state(&store), before);
}

#[test]
fn identities_the_store_already_holds_refuse_as_collisions() {
    let (mut store, fixture) = enrolled();
    let elsewhere = native_root(&mut store, "other-project", "elsewhere", 6);
    let native = native_root(&mut store, PROJECT, "blocked-native", 7);
    let blocker = store
        .add_work_blocker(
            &crate::domain::AddWorkBlockerRequest {
                work_id: native.work_id,
                expected_work_revision: native.revision,
                kind: WorkBlockerKind::ExternalInput,
                detail: "Input required".into(),
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner"),
                idempotency_key: "block".into(),
                blocked_at: at(8),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let after_block = item(&store, native.work_id);
    store
        .clear_work_blocker(
            &crate::domain::ClearWorkBlockerRequest {
                work_id: native.work_id,
                expected_work_revision: after_block.revision,
                blocker_id: blocker.blocker_id.clone(),
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner"),
                idempotency_key: "unblock".into(),
                cleared_at: at(9),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();

    // A work id another project holds.
    let mut taken = document("collides by id", None);
    taken.id = elsewhere.work_id;
    // A cleared blocker id stays reserved.
    let mut reused = document("reuses a blocker", None);
    reused.blockers = vec![SharedBlocker {
        blocker_id: blocker.blocker_id.clone(),
        kind: WorkBlockerKind::Manual,
        detail: "Waiting".into(),
        created_by: SharedActor::from_actor(&actor("remote-author")),
        created_at: at(2),
    }];
    let reused_id = reused.id;
    // A record-id pin is refused before insertion, naming the task.
    let mut pinned = document("pins a record", None);
    pinned.acceptance_bindings = vec![AcceptanceBinding {
        criterion: 1,
        requirement: VerificationRequirement {
            check_kind: VerificationKind::Test,
            check_fingerprint: Some(ObjectId::from_stored(durable(&store).remove(0).0).unwrap()),
        },
    }];
    let pinned_id = pinned.id;
    let resolved = with(&exported(&store), vec![taken, reused, pinned]);
    let before = state(&store);
    let message = conflict(&exchange(&mut store, &resolved, "add-1", 10).unwrap_err());
    for expected in [
        format!(
            "{}: id_collision (the work id is already held by this store)",
            elsewhere.work_id.0
        ),
        format!(
            "{}: id_collision (blocker {} is already held)",
            reused_id.0, blocker.blocker_id
        ),
        format!("{}: acceptance_pin (", pinned_id.0),
    ] {
        assert!(message.contains(&expected), "{expected}\n{message}");
    }
    assert_eq!(state(&store), before);
    assert_eq!(events(&store, fixture.parent), Vec::new());
}

#[test]
fn a_task_the_exchange_retired_cannot_be_added_again() {
    let (mut store, fixture) = enrolled();
    let mut without = fixture.tree.clone();
    without.tasks.retain(|task| task.id != fixture.completed);
    super::super::super::folder::record_complete_baseline_for_test(&mut store, &without).unwrap();
    assert!(
        exported(&store)
            .tasks
            .iter()
            .all(|task| task.id != fixture.completed)
    );
    let before = state(&store);
    let message = conflict(&exchange(&mut store, &fixture.tree, "re-add", 6).unwrap_err());
    assert!(
        message.contains(&format!(
            "{}: id_collision (the work id is already held by this store)",
            fixture.completed.0
        )),
        "{message}"
    );
    assert_eq!(state(&store), before);
}

#[test]
fn a_short_ref_another_project_holds_does_not_collide() {
    let (mut store, fixture) = enrolled();
    let elsewhere = native_root(&mut store, "other-project", "elsewhere", 6);
    let mut hex = elsewhere.work_id.0.simple().to_string();
    let first = if hex.starts_with('0') { "1" } else { "0" };
    hex.replace_range(0..1, first);
    let mut task = document("shares a short ref", None);
    task.id = WorkId(uuid::Uuid::parse_str(&hex).unwrap());
    let id = task.id;
    let resolved = with(&fixture.tree, vec![task]);
    assert!(exchange(&mut store, &resolved, "add-1", 7).unwrap().changed);
    assert_eq!(item(&store, id).short_ref, elsewhere.short_ref);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn an_addition_the_origin_history_cannot_take_refuses_as_origin_graph() {
    // The origin of q records that it requires p; its current state no
    // longer does. An added required child x of p that requires q is valid
    // now, but the origin history would hold the cycle q -> p -> x -> q.
    let p = document("p", None);
    let mut q = document("q", None);
    q.prerequisites = vec![p.id];
    let (p_id, q_id) = (p.id, q.id);
    let mut tasks = vec![p, q];
    tasks.sort_by_key(|task| task.id.0);
    let tree = TaskTree {
        project: ProjectDocument {
            format: FORMAT.into(),
            project_id: project(),
            notes_included: false,
        },
        tasks,
    };
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &tree, "enrol", 5).unwrap();
    let held = item(&store, q_id);
    store
        .remove_work_prerequisite(
            &crate::domain::ChangeWorkPrerequisiteRequest {
                work_id: q_id,
                prerequisite_id: p_id,
                expected_revision: held.revision,
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner"),
                idempotency_key: "drop-edge".into(),
                changed_at: at(6),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let mut x = document("x", Some(p_id));
    x.prerequisites = vec![q_id];
    let x_id = x.id;
    let resolved = with(&exported(&store), vec![x]);
    let before = state(&store);
    let message = conflict(&exchange(&mut store, &resolved, "add-1", 7).unwrap_err());
    assert!(
        message.contains(&format!("{}: origin_graph (", x_id.0)),
        "{message}"
    );
    assert_eq!(state(&store), before);
}

#[test]
fn an_enrolled_empty_destination_admits_additions_as_deltas() {
    let empty = TaskTree {
        project: ProjectDocument {
            format: FORMAT.into(),
            project_id: project(),
            notes_included: false,
        },
        tasks: Vec::new(),
    };
    let mut store = SqliteStore::open_in_memory().unwrap();
    assert!(
        exchange(&mut store, &empty, "enrol-empty", 5)
            .unwrap()
            .changed
    );
    let first = document("first task", None);
    let id = first.id;
    let added = with(&empty, vec![first]);
    assert!(exchange(&mut store, &added, "add-1", 6).unwrap().changed);
    assert!(item(&store, id).restored);
    assert_eq!(events(&store, id), Vec::new());
    assert_eq!(exported(&store), added);

    // Later folders keep the delta path: another addition joins, and the
    // first one's omission is an admitted removal, not a fresh bootstrap.
    let again = with(&added, vec![document("second task", None)]);
    assert!(exchange(&mut store, &again, "add-2", 7).unwrap().changed);
    assert!(store.verify_all().unwrap().is_healthy());
    let mut omitted = again.clone();
    omitted.tasks.retain(|task| task.id != id);
    assert!(exchange(&mut store, &omitted, "omit-1", 8).unwrap().changed);
    assert_eq!(item(&store, id).lifecycle, WorkLifecycle::Cancelled);
    assert_eq!(exported(&store), omitted);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn an_exact_repeat_of_an_addition_replays_its_receipt() {
    let (mut store, fixture) = enrolled();
    let resolved = with(&fixture.tree, vec![document("added root", None)]);
    let basis = basis(&store);
    let project = project();
    let importer = actor("folder-importer");
    let profile = profile();
    let request = TaskSyncFolderApply {
        project: &project,
        actor: &importer,
        tree: &resolved,
        export_basis: &basis,
        profile: &profile,
        complete: true,
        now: at(6),
    };
    let before = state(&store);
    let dry = store.dry_run_task_sync_folder(request, "add-1").unwrap();
    assert!(dry.changed && dry.dry_run);
    assert_eq!(state(&store), before);
    let first = store.apply_task_sync_folder(request, "add-1").unwrap();
    let committed = state(&store);
    assert_eq!(
        store.apply_task_sync_folder(request, "add-1").unwrap(),
        first
    );
    assert_eq!(state(&store), committed);
}

#[test]
fn shared_notes_on_an_addition_restore_and_a_held_note_id_collides() {
    // Shared notes are exported under this host's machine alias.
    let notes = ExportProfile {
        include_notes: true,
        origin_machine: Some("host-a".into()),
    };
    let apply = |store: &mut SqliteStore, tree: &TaskTree, key: &str, second: i64| {
        let basis = store
            .export_task_sync_folder(&project(), &notes)
            .unwrap()
            .export_basis;
        let project = project();
        store.apply_task_sync_folder(
            TaskSyncFolderApply {
                project: &project,
                actor: &actor("folder-importer"),
                tree,
                export_basis: &basis,
                profile: &notes,
                complete: true,
                now: at(second),
            },
            key,
        )
    };
    let mut fixture = fixture();
    fixture.tree.project.notes_included = true;
    let mut store = SqliteStore::open_in_memory().unwrap();
    apply(&mut store, &fixture.tree, "enrol", 5).unwrap();
    let note = |id: ObjectId| SharedNote {
        id,
        kind: SharedNoteKind::Generic,
        body: "Found the cause".into(),
        author: SharedActor::from_actor(&actor("remote-author")),
        recorded_at: at(2),
        origin_machine: "laptop".into(),
    };

    let mut fresh = document("with a note", None);
    fresh.notes = vec![note(ObjectId::mint())];
    let resolved = with(&fixture.tree, vec![fresh]);
    assert!(apply(&mut store, &resolved, "add-1", 6).unwrap().changed);
    assert_eq!(
        store
            .export_task_sync_folder(&project(), &notes)
            .unwrap()
            .tree,
        resolved
    );
    assert!(store.verify_all().unwrap().is_healthy());

    let mut colliding = document("reuses a record id", None);
    colliding.notes = vec![note(
        ObjectId::from_stored(durable(&store).remove(0).0).unwrap(),
    )];
    let colliding_id = colliding.id;
    let before = state(&store);
    let error = apply(&mut store, &with(&resolved, vec![colliding]), "add-2", 7).unwrap_err();
    let message = conflict(&error);
    assert!(
        message.contains(&format!("{}: id_collision (note ", colliding_id.0)),
        "{message}"
    );
    assert_eq!(state(&store), before);
}

#[test]
fn projection_repair_after_additions_preserves_every_durable_row() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("folder-adds.db");
    let fixture = fixture();
    let read_rows = |store: &SqliteStore| {
        store
            .connection
            .prepare("SELECT object_id, canonical_json FROM objects ORDER BY object_id")
            .unwrap()
            .query_map([], |row| {
                Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
            })
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap()
    };
    let rows = {
        let mut store = SqliteStore::open(&database).unwrap();
        exchange(&mut store, &fixture.tree, "enrol", 5).unwrap();
        let resolved = with(
            &fixture.tree,
            vec![
                document("added root", None),
                document("added child", Some(fixture.parent)),
            ],
        );
        exchange(&mut store, &resolved, "add-1", 6).unwrap();
        assert!(store.verify_all().unwrap().is_healthy());
        read_rows(&store)
    };
    let report = SqliteStore::repair_rebuildable_projections(&database).unwrap();
    assert!(report.is_healthy());
    let store = SqliteStore::open(&database).unwrap();
    assert_eq!(read_rows(&store), rows);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn a_failed_final_origin_audit_after_admission_is_a_projection_bug() {
    // Another project's import origin still decodes, but the task it
    // restored no longer carries that origin's attribution. Neither this
    // folder's admission nor this project's export reads that project, so
    // only the final audit, which checks every origin the store holds, finds
    // it: after the addition is admitted and inserted.
    let (mut store, fixture) = enrolled();
    let other = ProjectId("folder-apply-audit-other".into());
    let held = document("held by another project", None);
    let held_id = held.id;
    let tree = TaskTree {
        project: ProjectDocument {
            format: FORMAT.into(),
            project_id: other.clone(),
            notes_included: false,
        },
        tasks: vec![held],
    };
    let basis = store
        .export_task_sync_folder(&other, &profile())
        .unwrap()
        .export_basis;
    store
        .apply_task_sync_folder(
            TaskSyncFolderApply {
                project: &other,
                actor: &actor("folder-importer"),
                tree: &tree,
                export_basis: &basis,
                profile: &profile(),
                complete: true,
                now: at(6),
            },
            "enrol-other",
        )
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE work_items SET item_json = CAST(json_set(CAST(item_json AS TEXT), \
             '$.created_by.actor_id', 'someone-else') AS BLOB) WHERE work_id = ?1",
            [held_id.0.to_string()],
        )
        .unwrap();
    let resolved = with(&fixture.tree, vec![document("an unrelated root", None)]);
    let before = state(&store);
    let error = exchange(&mut store, &resolved, "add-1", 7).unwrap_err();
    assert!(
        matches!(
            &error,
            StoreError::InvalidWorkProjection(message)
                if message.starts_with(
                    "a folder addition failed the import origin audit: task_sync_import_origin:"
                )
        ),
        "{error:#}"
    );
    assert_eq!(state(&store), before);
}

mod refusals;
