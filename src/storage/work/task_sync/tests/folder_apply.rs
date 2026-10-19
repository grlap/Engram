use super::super::folder::TaskSyncFolderApply;
use super::super::folder_admission::{Conflict, NAMED_CONFLICT_LIMIT, conflict_message};
use super::*;
use crate::{
    ClaimWorkRequest, ReleaseWorkRequest, SessionId, WorkClaim,
    domain::{WorkPlanningAuthority, WorkTransition},
    task_sync::{folder_plan::ProjectChange, progress::ExportProfile},
};

const PROJECT: &str = "folder-apply";

fn profile() -> ExportProfile {
    ExportProfile {
        include_notes: false,
        origin_machine: None,
    }
}

fn document(title: &str, parent: Option<WorkId>) -> TaskDocument {
    TaskDocument {
        id: WorkId(uuid::Uuid::new_v4()),
        external_ref: None,
        title: title.into(),
        outcome: format!("Deliver {title}"),
        acceptance: vec!["delivered".into()],
        acceptance_bindings: Vec::new(),
        evaluation_mode: None,
        kind: WorkItemKind::Task,
        priority: 1,
        labels: Vec::new(),
        assigned_to: None,
        deferred_until: None,
        lifecycle: WorkLifecycle::Open,
        parent_id: parent,
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

/// A parent with two open children, and a completed root.
struct Fixture {
    tree: TaskTree,
    parent: WorkId,
    first: WorkId,
    second: WorkId,
    completed: WorkId,
}

fn fixture() -> Fixture {
    let parent = document("parent", None);
    let first = document("first child", Some(parent.id));
    let second = document("second child", Some(parent.id));
    let mut completed = document("reported complete", None);
    completed.lifecycle = WorkLifecycle::Completed;
    completed.completion = Some(SharedCompletion {
        summary: "reported result".into(),
        completed_at: at(2),
        actor: SharedActor::from_actor(&actor("reported-author")),
    });
    let ids = (parent.id, first.id, second.id, completed.id);
    let mut tasks = vec![parent, first, second, completed];
    tasks.sort_by_key(|task| task.id.0);
    Fixture {
        tree: TaskTree {
            project: ProjectDocument {
                format: FORMAT.into(),
                project_id: ProjectId(PROJECT.into()),
                notes_included: false,
            },
            tasks,
        },
        parent: ids.0,
        first: ids.1,
        second: ids.2,
        completed: ids.3,
    }
}

fn project() -> ProjectId {
    ProjectId(PROJECT.into())
}

fn basis(store: &SqliteStore) -> String {
    store
        .export_task_sync_folder(&project(), &profile())
        .unwrap()
        .export_basis
}

/// Applies `tree` against the destination's current basis, as a folder import
/// after an unchanged export would.
fn exchange(
    store: &mut SqliteStore,
    tree: &TaskTree,
    key: &str,
    second: i64,
) -> Result<TaskSyncImportResult, StoreError> {
    let basis = basis(store);
    let project = project();
    store.apply_task_sync_folder(
        TaskSyncFolderApply {
            project: &project,
            actor: &actor("folder-importer"),
            tree,
            export_basis: &basis,
            profile: &profile(),
            complete: true,
            now: at(second),
        },
        key,
    )
}

fn enrolled() -> (SqliteStore, Fixture) {
    let fixture = fixture();
    let mut store = SqliteStore::open_in_memory().unwrap();
    assert!(
        exchange(&mut store, &fixture.tree, "enrol", 5)
            .unwrap()
            .changed
    );
    (store, fixture)
}

fn edited(tree: &TaskTree, id: WorkId, edit: impl FnOnce(&mut TaskDocument)) -> TaskTree {
    let mut tree = tree.clone();
    edit(tree.tasks.iter_mut().find(|task| task.id == id).unwrap());
    tree
}

fn events(store: &SqliteStore, id: WorkId) -> Vec<crate::WorkEvent> {
    query::canonical_work_event_records_for_item(&store.connection, id)
        .unwrap()
        .into_iter()
        .map(|(_, event)| event)
        .collect()
}

fn exported(store: &SqliteStore) -> TaskTree {
    store
        .export_task_sync_folder(&project(), &profile())
        .unwrap()
        .tree
}

fn conflict(error: &StoreError) -> String {
    assert_eq!(
        refusal_code(error),
        "task_sync_import_conflict",
        "{error:#}"
    );
    error.to_string()
}

fn claim_restored(store: &mut SqliteStore, id: WorkId, holder: &str, second: i64) -> WorkClaim {
    let item = query::load_work_item(&store.connection, id).unwrap();
    store
        .claim_work(
            &ClaimWorkRequest {
                work_id: id,
                expected_work_revision: item.revision,
                expected_run_id: item.active_run_id,
                holder: SessionId(holder.into()),
                ttl_seconds: 3600,
                recovery_reason: None,
                actor: actor(holder),
                idempotency_key: format!("claim-{holder}-{second}"),
                claimed_at: at(second),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap()
}

/// The durable records an import must never rewrite: origins, restored
/// records and work events, by id and bytes.
fn durable(store: &SqliteStore) -> Vec<(String, Vec<u8>)> {
    store
        .connection
        .prepare(
            "SELECT object_id, canonical_json FROM objects
             WHERE object_kind IN ('task_sync_import_origin', 'work_restored_record')
             ORDER BY object_id",
        )
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn scalar_edits_apply_as_run_free_project_revisions_and_a_second_edit_follows() {
    let (mut store, fixture) = enrolled();
    let origins = durable(&store);
    let resolved = edited(
        &edited(&fixture.tree, fixture.parent, |task| {
            task.title = "Renamed parent".into();
            task.outcome = "Deliver the renamed parent".into();
        }),
        fixture.first,
        |task| task.priority = 3,
    );
    let result = exchange(&mut store, &resolved, "edit-1", 6).unwrap();
    assert!(result.changed && !result.dry_run);
    for id in [fixture.parent, fixture.first] {
        let history = events(&store, id);
        assert_eq!(history.len(), 1, "{id:?}");
        let event = &history[0];
        assert_eq!(
            event.transition,
            WorkTransition::Revised {
                authority: WorkPlanningAuthority::Project
            }
        );
        assert!(event.run_id.is_none() && event.run.is_none() && event.claim.is_none());
        assert_eq!(event.work.revision, 2);
        assert_eq!(event.actor.actor_id, "folder-importer");
    }
    assert_eq!(events(&store, fixture.second), Vec::new());
    assert_eq!(exported(&store), resolved);
    assert_eq!(durable(&store), origins);
    assert!(store.verify_all().unwrap().is_healthy());

    // A second exchange edits a task whose history now holds a run-free
    // revision; the S1 empty-graph path would refuse this store as native.
    let again = edited(&resolved, fixture.parent, |task| {
        task.title = "Renamed parent again".into();
    });
    assert!(exchange(&mut store, &again, "edit-2", 7).unwrap().changed);
    assert_eq!(events(&store, fixture.parent).len(), 2);
    assert_eq!(exported(&store), again);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn a_restored_completion_stays_event_free_and_an_edit_of_it_refuses() {
    let (mut store, fixture) = enrolled();
    let resolved = edited(&fixture.tree, fixture.parent, |task| {
        task.title = "Renamed parent".into();
    });
    exchange(&mut store, &resolved, "edit-1", 6).unwrap();
    assert_eq!(events(&store, fixture.completed), Vec::new());

    let before = state(&store);
    let completed = edited(&resolved, fixture.completed, |task| {
        task.title = "Renamed completion".into();
    });
    let message = conflict(&exchange(&mut store, &completed, "edit-2", 7).unwrap_err());
    assert!(
        message.contains(&format!("{}: restored_completion", fixture.completed.0)),
        "{message}"
    );
    assert_eq!(state(&store), before);
}

#[test]
fn an_active_run_refuses_the_whole_folder_naming_only_the_blocked_task() {
    let (mut store, fixture) = enrolled();
    let claim = claim_restored(&mut store, fixture.first, "worker", 6);
    let before = state(&store);
    let resolved = edited(
        &edited(&fixture.tree, fixture.first, |task| {
            task.title = "Renamed first".into();
        }),
        fixture.second,
        |task| task.title = "Renamed second".into(),
    );
    let message = conflict(&exchange(&mut store, &resolved, "edit-1", 7).unwrap_err());
    assert!(
        message.contains(&format!(
            "{}: active_run (run {})",
            fixture.first.0, claim.run_id.0
        )),
        "{message}"
    );
    assert!(
        !message.contains(&fixture.second.0.to_string()),
        "{message}"
    );
    assert_eq!(state(&store), before);
}

#[test]
fn a_released_prior_run_still_refuses_as_its_active_run() {
    let (mut store, fixture) = enrolled();
    let claim = claim_restored(&mut store, fixture.first, "worker", 6);
    let item = query::load_work_item(&store.connection, fixture.first).unwrap();
    store
        .release_work(
            &ReleaseWorkRequest {
                work_id: fixture.first,
                run_id: claim.run_id,
                expected_work_revision: item.revision,
                holder: SessionId("worker".into()),
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                reason: "hand back".into(),
                waiver_reason: Some("no contribution".into()),
                actor: actor("worker"),
                idempotency_key: "release-worker".into(),
                released_at: at(7),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let before = state(&store);
    let resolved = edited(&fixture.tree, fixture.first, |task| {
        task.title = "Renamed first".into();
    });
    // A release keeps the run as the task's active run.
    let message = conflict(&exchange(&mut store, &resolved, "edit-1", 8).unwrap_err());
    assert!(
        message.contains(&format!(
            "{}: active_run (run {})",
            fixture.first.0, claim.run_id.0
        )),
        "{message}"
    );
    assert_eq!(state(&store), before);
}

#[test]
fn unsupported_deltas_and_blocked_edits_refuse_together_in_task_id_order() {
    let (mut store, fixture) = enrolled();
    let claim = claim_restored(&mut store, fixture.first, "worker", 6);
    let before = state(&store);
    let resolved = edited(
        &edited(&fixture.tree, fixture.first, |task| {
            task.title = "Renamed first".into();
        }),
        fixture.second,
        |task| task.labels = vec!["sync".into()],
    );
    let message = conflict(&exchange(&mut store, &resolved, "edit-1", 7).unwrap_err());
    let blocked = format!("{}: active_run (run {})", fixture.first.0, claim.run_id.0);
    let unsupported = format!("{}: unsupported_fields (labels)", fixture.second.0);
    let (Some(blocked_at), Some(unsupported_at)) =
        (message.find(&blocked), message.find(&unsupported))
    else {
        panic!("both conflicts are named: {message}");
    };
    assert_eq!(
        blocked_at < unsupported_at,
        fixture.first.0 < fixture.second.0,
        "{message}"
    );
    assert_eq!(state(&store), before);
}

#[test]
fn run_free_planning_history_refuses_as_planning_history() {
    let (mut store, fixture) = enrolled();
    let redactor = crate::DevelopmentNoopRedactor;
    let other = store
        .create_work(&root_request(PROJECT, "prerequisite", 6), &redactor)
        .unwrap();
    for (step, add) in [(7, true), (8, false)] {
        let item = query::load_work_item(&store.connection, fixture.first).unwrap();
        let request = crate::domain::ChangeWorkPrerequisiteRequest {
            work_id: fixture.first,
            prerequisite_id: other.work_id,
            expected_revision: item.revision,
            authority: WorkPlanningAuthority::Project,
            actor: actor("planner"),
            idempotency_key: format!("edge-{step}"),
            changed_at: at(step),
        };
        if add {
            store.add_work_prerequisite(&request, &redactor).unwrap();
        } else {
            store.remove_work_prerequisite(&request, &redactor).unwrap();
        }
    }
    let before = state(&store);
    let resolved = edited(&exported(&store), fixture.first, |task| {
        task.title = "Renamed first".into();
    });
    let message = conflict(&exchange(&mut store, &resolved, "edit-1", 9).unwrap_err());
    assert!(
        message.contains(&format!("{}: planning_history (event ", fixture.first.0)),
        "{message}"
    );
    assert_eq!(state(&store), before);
}

#[test]
fn a_run_free_execution_event_refuses_as_execution_history() {
    // No public operation leaves an Open task run-free once it has executed,
    // so the guard is checked with a directly appended run-free event.
    let (mut store, fixture) = enrolled();
    let evidence = ObjectId::from_stored(durable(&store).remove(0).0).unwrap();
    let item = query::load_work_item(&store.connection, fixture.second).unwrap();
    let mut draft = crate::storage::work::planning::revised_event(
        &item,
        None,
        None,
        WorkPlanningAuthority::Project,
        actor("recorder"),
        at(6),
    );
    draft.transition = WorkTransition::EvidenceAdded { evidence };
    let transaction = store.begin_work_mutation().unwrap();
    crate::storage::work::feeds::append_work_event(&transaction, &draft).unwrap();
    transaction.commit().unwrap();

    let before = state(&store);
    let resolved = edited(&fixture.tree, fixture.second, |task| {
        task.title = "Renamed second".into();
    });
    let message = conflict(&exchange(&mut store, &resolved, "edit-1", 7).unwrap_err());
    assert!(
        message.contains(&format!("{}: execution_history (event ", fixture.second.0)),
        "{message}"
    );
    assert_eq!(state(&store), before);
}

#[test]
fn a_cancelled_restored_task_refuses_as_lifecycle() {
    let mut fixture = fixture();
    let standalone = document("standalone", None);
    let id = standalone.id;
    fixture.tree.tasks.push(standalone);
    fixture.tree.tasks.sort_by_key(|task| task.id.0);
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &fixture.tree, "enrol", 5).unwrap();
    let item = query::load_work_item(&store.connection, id).unwrap();
    store
        .dispose_work(
            &crate::domain::DisposeWorkRequest {
                work_id: id,
                expected_work_revision: item.revision,
                disposition: crate::WorkDisposition::Cancelled,
                replacement_id: None,
                reason: "No longer needed".into(),
                actor: actor("planner"),
                idempotency_key: "cancel".into(),
                disposed_at: at(6),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();

    let before = state(&store);
    let resolved = edited(&exported(&store), id, |task| {
        task.title = "Renamed standalone".into();
    });
    let message = conflict(&exchange(&mut store, &resolved, "edit-1", 7).unwrap_err());
    assert!(
        message.contains(&format!("{}: lifecycle (the task is Cancelled)", id.0)),
        "{message}"
    );
    assert_eq!(state(&store), before);
}

#[test]
fn execution_elsewhere_in_the_hierarchy_does_not_block_an_edit() {
    // A claimed child does not bind its parent's revision.
    let (mut store, fixture) = enrolled();
    claim_restored(&mut store, fixture.first, "worker", 6);
    let parent = edited(&fixture.tree, fixture.parent, |task| {
        task.title = "Renamed parent".into();
    });
    assert!(
        exchange(&mut store, &parent, "edit-parent", 7)
            .unwrap()
            .changed
    );

    // A claimed sibling does not bind the other child's revision.
    let sibling = edited(&parent, fixture.second, |task| {
        task.title = "Renamed second".into();
    });
    assert!(
        exchange(&mut store, &sibling, "edit-sibling", 8)
            .unwrap()
            .changed
    );

    // A claimed parent does not bind its child's revision.
    let (mut store, fixture) = enrolled();
    claim_restored(&mut store, fixture.parent, "worker", 6);
    let child = edited(&fixture.tree, fixture.second, |task| {
        task.title = "Renamed second".into();
    });
    assert!(
        exchange(&mut store, &child, "edit-child", 7)
            .unwrap()
            .changed
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn any_unsupported_delta_refuses_the_whole_folder_by_name() {
    let (mut store, fixture) = enrolled();
    let before = state(&store);
    let resolved = edited(
        &edited(&fixture.tree, fixture.parent, |task| {
            task.title = "Renamed parent".into();
        }),
        fixture.first,
        |task| task.labels = vec!["sync".into()],
    );
    let message = conflict(&exchange(&mut store, &resolved, "edit-1", 6).unwrap_err());
    assert!(
        message.contains(&format!("{}: unsupported_fields (labels)", fixture.first.0)),
        "{message}"
    );
    assert_eq!(state(&store), before);

    // An admissible addition beside an unsupported change is not applied
    // on its own: the whole folder refuses.
    let mut added = resolved.clone();
    let addition = document("added elsewhere", None);
    let addition_id = addition.id;
    added.tasks.push(addition);
    added.tasks.sort_by_key(|task| task.id.0);
    let message = conflict(&exchange(&mut store, &added, "edit-2", 7).unwrap_err());
    assert!(message.contains("unsupported_fields"), "{message}");
    assert!(!message.contains(&addition_id.0.to_string()), "{message}");
    assert_eq!(state(&store), before);
}

#[test]
fn a_native_task_carried_by_the_folder_refuses_its_edit() {
    let (mut store, fixture) = enrolled();
    let native = store
        .create_work(
            &root_request(PROJECT, "native-task", 6),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let carried = exported(&store);
    let resolved = edited(&carried, native.work_id, |task| {
        task.title = "Renamed native".into();
    });
    let before = state(&store);
    let message = conflict(&exchange(&mut store, &resolved, "edit-1", 7).unwrap_err());
    assert!(
        message.contains(&format!("{}: unsupported_origin", native.work_id.0)),
        "{message}"
    );
    assert_eq!(state(&store), before);
    assert_eq!(events(&store, fixture.parent), Vec::new());
}

#[test]
fn a_host_local_task_stays_until_enrolled_then_its_omission_is_a_refused_deletion() {
    let (mut store, fixture) = enrolled();
    let local = store
        .create_work(
            &root_request(PROJECT, "host-local", 6),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();

    // The folder omits the host-local task: it is retained, nothing changes.
    let retained = exchange(&mut store, &fixture.tree, "omit-local", 7).unwrap();
    assert!(!retained.changed);
    assert!(
        exported(&store)
            .tasks
            .iter()
            .any(|task| task.id == local.work_id)
    );

    // A folder that carries it unchanged enrols it in the baseline.
    let carried = exported(&store);
    assert!(
        exchange(&mut store, &carried, "carry-local", 8)
            .unwrap()
            .changed
    );
    assert_eq!(events(&store, fixture.parent), Vec::new());

    // Its later omission is now a deletion, which this exchange refuses.
    let before = state(&store);
    let mut omitted = carried.clone();
    omitted.tasks.retain(|task| task.id != local.work_id);
    let message = conflict(&exchange(&mut store, &omitted, "omit-enrolled", 9).unwrap_err());
    assert!(
        message.contains(&format!("{}: task_deleted", local.work_id.0)),
        "{message}"
    );
    assert_eq!(state(&store), before);
}

#[test]
fn an_exact_repeat_replays_and_a_dry_run_writes_nothing() {
    let (mut store, fixture) = enrolled();
    let resolved = edited(&fixture.tree, fixture.parent, |task| {
        task.title = "Renamed parent".into();
    });
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
    let dry = store.dry_run_task_sync_folder(request, "edit-1").unwrap();
    assert!(dry.changed && dry.dry_run);
    assert!(store.connection.is_autocommit());
    assert_eq!(state(&store), before);

    let first = store.apply_task_sync_folder(request, "edit-1").unwrap();
    let committed = state(&store);
    assert_eq!(
        store.apply_task_sync_folder(request, "edit-1").unwrap(),
        first
    );
    assert_eq!(state(&store), committed);
    assert_eq!(events(&store, fixture.parent).len(), 1);
}

#[test]
fn projection_repair_after_an_edit_preserves_every_durable_row() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("folder-apply.db");
    let fixture = fixture();
    let rows = {
        let mut store = SqliteStore::open(&database).unwrap();
        exchange(&mut store, &fixture.tree, "enrol", 5).unwrap();
        let resolved = edited(&fixture.tree, fixture.parent, |task| {
            task.title = "Renamed parent".into();
        });
        exchange(&mut store, &resolved, "edit-1", 6).unwrap();
        assert!(store.verify_all().unwrap().is_healthy());
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
    let report = SqliteStore::repair_rebuildable_projections(&database).unwrap();
    assert!(report.is_healthy());
    let store = SqliteStore::open(&database).unwrap();
    let after = store
        .connection
        .prepare("SELECT object_id, canonical_json FROM objects ORDER BY object_id")
        .unwrap()
        .query_map([], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        })
        .unwrap()
        .collect::<Result<Vec<_>, _>>()
        .unwrap();
    assert_eq!(after, rows);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn a_required_child_waiver_on_the_parent_refuses_its_edit_as_execution_history() {
    // A waiver accounts for the root's execution: with only a child claimed,
    // the parent records it without a run of its own.
    let (mut store, fixture) = enrolled();
    claim_restored(&mut store, fixture.first, "worker", 6);
    let child = query::load_work_item(&store.connection, fixture.second).unwrap();
    let parent = query::load_work_item(&store.connection, fixture.parent).unwrap();
    store
        .reject_required_child(
            &crate::domain::RejectRequiredChildRequest {
                work_id: fixture.second,
                expected_work_revision: child.revision,
                expected_parent_revision: Some(parent.revision),
                reason: "No longer needed".into(),
                actor: actor("planner"),
                idempotency_key: "reject-second".into(),
                rejected_at: at(7),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let parent = query::load_work_item(&store.connection, fixture.parent).unwrap();
    assert_eq!(parent.active_run_id, None);

    let before = state(&store);
    let resolved = edited(&exported(&store), fixture.parent, |task| {
        task.title = "Renamed parent".into();
    });
    let message = conflict(&exchange(&mut store, &resolved, "edit-1", 8).unwrap_err());
    assert!(
        message.contains(&format!("{}: execution_history (event ", fixture.parent.0)),
        "{message}"
    );
    assert_eq!(state(&store), before);
}

fn deleted_conflicts(count: usize) -> Vec<Conflict> {
    (1_u128..)
        .take(count)
        .map(|value| Conflict {
            id: WorkId(uuid::Uuid::from_u128(value)),
            reason: "task_deleted",
            detail: "omitted from the folder".into(),
        })
        .collect()
}

#[test]
fn a_refusal_names_at_most_the_limit_and_counts_the_rest_exactly() {
    let named = |message: &str| message.matches(": task_deleted").count();

    let at_limit = conflict_message(&[], &deleted_conflicts(NAMED_CONFLICT_LIMIT));
    assert_eq!(named(&at_limit), NAMED_CONFLICT_LIMIT, "{at_limit}");
    assert!(!at_limit.contains("more refused"), "{at_limit}");
    assert!(!at_limit.ends_with(';'), "{at_limit}");

    let one_over = conflict_message(&[], &deleted_conflicts(NAMED_CONFLICT_LIMIT + 1));
    assert_eq!(named(&one_over), NAMED_CONFLICT_LIMIT, "{one_over}");
    assert!(
        one_over.ends_with("; and 1 more refused task"),
        "{one_over}"
    );

    let conflicts = deleted_conflicts(NAMED_CONFLICT_LIMIT + 3);
    let with_project = conflict_message(&[ProjectChange::NotesProfile], &conflicts);
    assert!(
        with_project.starts_with(
            "task folder import refuses the selected changes: project: notes_profile; "
        ),
        "{with_project}"
    );
    assert_eq!(named(&with_project), NAMED_CONFLICT_LIMIT, "{with_project}");
    assert!(
        with_project.contains(&conflicts[NAMED_CONFLICT_LIMIT - 1].id.0.to_string())
            && !with_project.contains(&conflicts[NAMED_CONFLICT_LIMIT].id.0.to_string()),
        "{with_project}"
    );
    assert!(
        with_project.ends_with("; and 3 more refused tasks"),
        "{with_project}"
    );
}

#[test]
fn a_refusal_beyond_the_limit_names_the_lowest_ids_and_counts_the_rest() {
    let mut tasks = (0..NAMED_CONFLICT_LIMIT + 5)
        .map(|index| document(&format!("root {index}"), None))
        .collect::<Vec<_>>();
    tasks.sort_by_key(|task| task.id.0);
    let ids = tasks.iter().map(|task| task.id).collect::<Vec<_>>();
    let project_document = ProjectDocument {
        format: FORMAT.into(),
        project_id: project(),
        notes_included: false,
    };
    let enrolled = TaskTree {
        project: project_document.clone(),
        tasks,
    };
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &enrolled, "enrol", 5).unwrap();

    // An empty folder against a destination that holds the exchange is a
    // deletion of every enrolled task, not a bootstrap.
    let empty = TaskTree {
        project: project_document,
        tasks: Vec::new(),
    };
    let basis = basis(&store);
    let project = project();
    let importer = actor("folder-importer");
    let profile = profile();
    let request = TaskSyncFolderApply {
        project: &project,
        actor: &importer,
        tree: &empty,
        export_basis: &basis,
        profile: &profile,
        complete: true,
        now: at(6),
    };
    let before = state(&store);
    let dry = store
        .dry_run_task_sync_folder(request, "empty")
        .unwrap_err();
    let real = store.apply_task_sync_folder(request, "empty").unwrap_err();
    assert_eq!(dry.to_string(), real.to_string());
    assert_eq!(state(&store), before);

    let message = conflict(&real);
    let mut previous = 0;
    for id in &ids[..NAMED_CONFLICT_LIMIT] {
        let entry = format!("{}: task_deleted", id.0);
        let position = message
            .find(&entry)
            .unwrap_or_else(|| panic!("{entry} is named: {message}"));
        assert!(position > previous, "{message}");
        previous = position;
    }
    for id in &ids[NAMED_CONFLICT_LIMIT..] {
        assert!(!message.contains(&id.0.to_string()), "{message}");
    }
    assert!(message.ends_with("; and 5 more refused tasks"), "{message}");
}

mod adds;
