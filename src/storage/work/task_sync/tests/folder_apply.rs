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
fn ordinary_run_free_project_relation_history_keeps_later_scalar_import_admissible() {
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
    let resolved = edited(&exported(&store), fixture.first, |task| {
        task.title = "Renamed first".into();
    });
    exchange(&mut store, &resolved, "edit-1", 9).unwrap();
    assert_eq!(exported(&store), resolved);
    assert!(store.verify_all().unwrap().is_healthy());
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
        message.contains(&format!("{}: unsupported_origin", local.work_id.0)),
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

fn lifecycle_conflicts(count: usize) -> Vec<Conflict> {
    (1_u128..)
        .take(count)
        .map(|value| Conflict {
            id: WorkId(uuid::Uuid::from_u128(value)),
            reason: "lifecycle",
            detail: "the task is Cancelled".into(),
        })
        .collect()
}

#[test]
fn a_refusal_names_at_most_the_limit_and_counts_the_rest_exactly() {
    let named = |message: &str| message.matches(": lifecycle").count();

    let at_limit = conflict_message(&[], &lifecycle_conflicts(NAMED_CONFLICT_LIMIT));
    assert_eq!(named(&at_limit), NAMED_CONFLICT_LIMIT, "{at_limit}");
    assert!(!at_limit.contains("more refused"), "{at_limit}");
    assert!(!at_limit.ends_with(';'), "{at_limit}");

    let one_over = conflict_message(&[], &lifecycle_conflicts(NAMED_CONFLICT_LIMIT + 1));
    assert_eq!(named(&one_over), NAMED_CONFLICT_LIMIT, "{one_over}");
    assert!(
        one_over.ends_with("; and 1 more refused task"),
        "{one_over}"
    );

    let conflicts = lifecycle_conflicts(NAMED_CONFLICT_LIMIT + 3);
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
    // Terminal members still refuse retirement; use a graph-valid fixture
    // whose tasks all exercise the same lifecycle boundary.
    for task in &mut tasks {
        task.lifecycle = WorkLifecycle::Cancelled;
        task.disposal_reason = Some("already cancelled".into());
    }
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
        let entry = format!("{}: lifecycle", id.0);
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
mod relations;

/// Omission of a baseline member, unlike omission of never-enrolled local work.
fn without(tree: &TaskTree, ids: &[WorkId]) -> TaskTree {
    let mut selected = tree.clone();
    selected.tasks.retain(|task| !ids.contains(&task.id));
    selected
}

#[test]
fn folder_removal_is_one_run_free_disposal_and_preserves_history() {
    let (mut store, fixture) = enrolled();
    let origins = durable(&store);
    let tables_before = state(&store).rows;
    let before = query::load_work_item(&store.connection, fixture.first).unwrap();
    let resolved = without(&fixture.tree, &[fixture.first]);
    super::super::folder_admission::RETAINED_VALIDATIONS.with(|count| count.set(0));
    let result = exchange(&mut store, &resolved, "remove-leaf", 6).unwrap();
    // A deletion-only import validates C minus D even with no additions.
    assert_eq!(
        super::super::folder_admission::RETAINED_VALIDATIONS.with(std::cell::Cell::get),
        1
    );
    assert!(result.changed);
    let removed = query::load_work_item(&store.connection, fixture.first).unwrap();
    assert_eq!(removed.lifecycle, WorkLifecycle::Cancelled);
    assert_eq!(removed.revision, before.revision + 1);
    assert_eq!(removed.parent_id, before.parent_id);
    assert_eq!(removed.root_id, before.root_id);
    assert_eq!(removed.active_run_id, None);
    let history = events(&store, fixture.first);
    assert_eq!(history.len(), 1);
    let event = &history[0];
    assert!(matches!(
        event.transition,
        WorkTransition::Disposed {
            lifecycle: WorkLifecycle::Cancelled,
            replacement_id: None,
            ..
        }
    ));
    assert!(event.run_id.is_none() && event.run.is_none());
    assert!(event.root_execution.is_none() && event.claim.is_none());
    assert!(event.handoff_offer.is_none() && event.blocker.is_none());
    assert_eq!(event.actor.actor_id, "folder-importer");
    assert_eq!(event.created_at, at(6));
    for table in [
        "work_runs",
        "work_claims",
        "work_root_executions",
        "work_root_members",
    ] {
        let count: i64 = store
            .connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "{table}");
    }
    let run_feed: i64 = store
        .connection
        .query_row(
            "SELECT count(*) FROM work_feed_entries WHERE feed_kind = 'run_execution'",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(run_feed, 0);
    let tables_after = state(&store).rows;
    for (name, rows) in tables_before {
        if matches!(
            name.as_str(),
            "objects"
                | "work_items"
                | "work_feed_entries"
                | "work_feed_heads"
                | "work_operation_results"
        ) || name.starts_with("work_catalog_fts")
        {
            continue;
        }
        assert_eq!(
            tables_after
                .iter()
                .find(|(table, _)| table == &name)
                .map(|(_, rows)| rows),
            Some(&rows),
            "unchanged table {name}"
        );
    }
    assert_eq!(durable(&store), origins);
    assert_eq!(exported(&store), resolved);
    assert_eq!(events(&store, fixture.completed), []);
    assert!(store.verify_all().unwrap().is_healthy());
    assert!(
        !exchange(&mut store, &resolved, "remove-repeat", 7)
            .unwrap()
            .changed
    );
    assert_eq!(events(&store, fixture.first).len(), 1);
}

#[test]
fn folder_removal_preserves_outgoing_edges_and_sequential_retirement() {
    let mut fixture = fixture();
    fixture.tree = edited(&fixture.tree, fixture.first, |task| {
        task.prerequisites = vec![fixture.completed];
        task.blockers.push(SharedBlocker {
            blocker_id: uuid::Uuid::new_v4().to_string(),
            kind: WorkBlockerKind::Manual,
            detail: "historical input".into(),
            created_by: SharedActor::from_actor(&actor("author")),
            created_at: at(1),
        });
    });
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &fixture.tree, "enrol", 5).unwrap();
    let origins = durable(&store);
    let children_removed = without(&fixture.tree, &[fixture.first, fixture.second]);
    exchange(&mut store, &children_removed, "children", 6).unwrap();
    let parent_removed = without(&children_removed, &[fixture.parent]);
    exchange(&mut store, &parent_removed, "parent", 7).unwrap();
    let graph = store.export_task_tree(&project()).unwrap();
    let retired_child = graph
        .tasks
        .iter()
        .find(|task| task.id == fixture.first)
        .unwrap();
    assert_eq!(retired_child.parent_id, Some(fixture.parent));
    assert_eq!(retired_child.prerequisites, vec![fixture.completed]);
    assert_eq!(retired_child.blockers.len(), 1);
    assert_eq!(durable(&store), origins);
    assert_eq!(exported(&store), parent_removed);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn folder_removal_refuses_hidden_local_incoming_references_without_writes() {
    let (mut store, fixture) = enrolled();
    let local = store
        .create_work(
            &root_request(PROJECT, "local-root", 6),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    store
        .add_work_prerequisite(
            &crate::domain::ChangeWorkPrerequisiteRequest {
                work_id: local.work_id,
                prerequisite_id: fixture.first,
                expected_revision: local.revision,
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner"),
                idempotency_key: "local-needs-member".into(),
                changed_at: at(7),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    // R omits native L, but current L still requires the removed member.
    let resolved = without(&fixture.tree, &[fixture.first]);
    let before = state(&store);
    let message = conflict(&exchange(&mut store, &resolved, "remove-member", 8).unwrap_err());
    assert!(
        message.contains(&format!("{}: retained_reference", fixture.first.0)),
        "{message}"
    );
    assert!(message.contains(&local.work_id.0.to_string()), "{message}");
    assert_eq!(state(&store), before);
}

#[test]
fn folder_removal_of_a_parent_refuses_when_its_claimed_child_stays() {
    for claim_lower_id in [true, false] {
        let (mut store, fixture) = enrolled();
        let mut children = [fixture.first, fixture.second];
        children.sort_by_key(|id| id.0);
        let claimed = children[usize::from(!claim_lower_id)];
        let sibling = children[usize::from(claim_lower_id)];
        let claim = claim_restored(&mut store, claimed, "child-worker", 6);
        let selected = without(
            &exported(&store),
            &[fixture.parent, fixture.first, fixture.second],
        );
        selected.validate_folder().unwrap();
        let before = state(&store);
        let message =
            conflict(&exchange(&mut store, &selected, "remove-claimed-subtree", 7).unwrap_err());
        assert!(
            message.contains(&format!(
                "{}: active_run (run {})",
                claimed.0, claim.run_id.0
            )),
            "{message}"
        );
        assert!(
            message.contains(&format!("{}: ancestor_binding", sibling.0)),
            "{message}"
        );
        assert!(
            message.contains(&format!("{}: retained_reference", fixture.parent.0)),
            "{message}"
        );
        // Both refused children retain their parent edge. The diagnostic
        // names the lower id, whether or not that child holds the claim.
        assert!(
            message.contains(&format!(
                "retained task {} parent_id references {}",
                children[0].0, fixture.parent.0
            )),
            "{message}"
        );
        assert_eq!(state(&store), before);
    }
}

#[test]
fn folder_removal_of_a_native_sealed_member_refuses_without_changing_its_seal() {
    use crate::storage::work::test_support::{checkpoint, complete, evidence};
    let root = document("root completed here", None);
    let mut initial = fixture().tree;
    initial.tasks = vec![root.clone()];
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &initial, "enrol", 5).unwrap();
    let claim = claim_restored(&mut store, root.id, "worker", 6);
    let current = query::load_work_item(&store.connection, root.id).unwrap();
    let proof = evidence(&mut store, &current, &claim, "worker", "proof", 7);
    checkpoint(
        &mut store,
        &current,
        &claim,
        "worker",
        "checkpoint",
        8,
        std::slice::from_ref(&proof),
    );
    let seal = complete(
        &mut store, &current, &claim, "worker", &proof, "complete", 9,
    )
    .unwrap();
    assert_eq!(seal.work_id, root.id);
    let selected = without(&exported(&store), &[root.id]);
    let before = state(&store);
    let message = conflict(&exchange(&mut store, &selected, "remove-sealed", 10).unwrap_err());
    assert!(
        message.contains(&format!("{}: sealed_completion", root.id.0)),
        "{message}"
    );
    assert_eq!(state(&store), before);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn folder_removal_refuses_a_run_free_revision_with_root_bound_history() {
    let (mut store, fixture) = enrolled();
    claim_restored(&mut store, fixture.first, "sibling-worker", 6);
    let root = query::active_root_execution(&store.connection, fixture.parent).unwrap();
    let mut item = query::load_work_item(&store.connection, fixture.second).unwrap();
    assert_eq!(item.active_run_id, None);
    item.revision += 1;
    item.updated_at = at(7);
    // Exercise conservative historical admission directly: public scalar
    // revision currently writes no root snapshot, but retained history may
    // carry one. The event and root reference are stored canonically.
    let transaction = store.connection.transaction().unwrap();
    crate::storage::work::planning::persist_work_item(&transaction, &item).unwrap();
    let mut event = crate::storage::work::planning::revised_event(
        &item,
        None,
        None,
        WorkPlanningAuthority::Project,
        actor("planner"),
        at(7),
    );
    event.root_execution = Some(root);
    let (event_id, _) =
        crate::storage::work::feeds::append_work_event(&transaction, &event).unwrap();
    transaction.commit().unwrap();
    let selected = without(&exported(&store), &[item.work_id]);
    let before = state(&store);
    let message = conflict(&exchange(&mut store, &selected, "remove-root-bound", 8).unwrap_err());
    // Shared eligibility now rejects the root snapshot before the removal-only guards.
    assert!(
        message.contains(&format!(
            "{}: execution_history (event {})",
            item.work_id.0,
            event_id.as_str()
        )),
        "{message}"
    );
    assert_eq!(state(&store), before);
}

#[test]
fn folder_removal_of_an_optional_child_keeps_ordinary_parent_completion() {
    use crate::storage::work::test_support::{checkpoint, complete, evidence};
    let parent = document("parent", None);
    let mut child = document("optional child", Some(parent.id));
    child.child_requirement = ChildRequirement::Optional;
    let mut initial = fixture().tree;
    initial.tasks = vec![parent.clone(), child.clone()];
    initial.tasks.sort_by_key(|task| task.id.0);
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &initial, "enrol", 5).unwrap();
    exchange(
        &mut store,
        &without(&initial, &[child.id]),
        "remove-optional",
        6,
    )
    .unwrap();
    let claim = claim_restored(&mut store, parent.id, "parent-worker", 7);
    let current = query::load_work_item(&store.connection, parent.id).unwrap();
    let proof = evidence(&mut store, &current, &claim, "parent-worker", "proof", 8);
    checkpoint(
        &mut store,
        &current,
        &claim,
        "parent-worker",
        "checkpoint",
        9,
        std::slice::from_ref(&proof),
    );
    let seal = complete(
        &mut store,
        &current,
        &claim,
        "parent-worker",
        &proof,
        "complete-parent",
        10,
    )
    .unwrap();
    assert_eq!(seal.required_child_seals, []);
    assert_eq!(seal.required_child_waivers, []);
    // Ordinary completion lists optional children without a completed seal,
    // including the retained Cancelled child; no waiver is manufactured.
    assert_eq!(seal.unfinished_optional_children, vec![child.id]);
    let retained = query::load_work_item(&store.connection, child.id).unwrap();
    assert_eq!(retained.lifecycle, WorkLifecycle::Cancelled);
    assert_eq!(retained.parent_id, Some(parent.id));
    assert_eq!(retained.active_run_id, None);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn folder_removal_uses_current_references_despite_refused_folder_edits() {
    let mut fixture = fixture();
    fixture.tree = edited(&fixture.tree, fixture.first, |task| {
        task.prerequisites = vec![fixture.second];
    });
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &fixture.tree, "enrol", 5).unwrap();
    let selected = edited(
        &without(&fixture.tree, &[fixture.second]),
        fixture.first,
        |task| {
            task.prerequisites.clear();
            task.labels.push("unsupported-edit".into());
        },
    );
    selected.validate_folder().unwrap();
    let before = state(&store);
    let message = conflict(&exchange(&mut store, &selected, "remove-referenced", 6).unwrap_err());
    assert!(
        message.contains(&format!("{}: retained_reference", fixture.second.0)),
        "{message}"
    );
    assert!(
        message.contains(&format!("{}: unsupported_fields (labels)", fixture.first.0)),
        "{message}"
    );
    assert_eq!(state(&store), before);
}

#[test]
fn folder_removal_refusal_does_not_blame_an_added_indirect_dependent() {
    let mut fixture = fixture();
    fixture.tree = edited(&fixture.tree, fixture.first, |task| {
        task.prerequisites = vec![fixture.second];
    });
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &fixture.tree, "enrol", 5).unwrap();
    // Current H -> D survives: H also asks for an unsupported label edit, and the
    // requested deletion of D must retain it. X -> H is valid in both R
    // and the graph the import would leave, so X is no graph culprit.
    let mut selected = edited(
        &without(&fixture.tree, &[fixture.second]),
        fixture.first,
        |task| {
            task.prerequisites.clear();
            task.labels.push("unsupported-edit".into());
        },
    );
    let mut added = document("indirect dependent", None);
    added.prerequisites = vec![fixture.first];
    let added_id = added.id;
    selected.tasks.push(added);
    selected.tasks.sort_by_key(|task| task.id.0);
    selected.validate_folder().unwrap();
    let before = state(&store);
    let message =
        conflict(&exchange(&mut store, &selected, "mixed-retained-reference", 6).unwrap_err());
    assert!(
        message.contains(&format!("{}: unsupported_fields", fixture.first.0)),
        "{message}"
    );
    assert!(
        message.contains(&format!("{}: retained_reference", fixture.second.0)),
        "{message}"
    );
    assert!(!message.contains(&added_id.0.to_string()), "{message}");
    assert!(!message.contains("folder:"), "{message}");
    assert_eq!(state(&store), before);
}

#[test]
fn folder_removal_keeps_references_of_every_refused_deletion() {
    let mut first = document("retained cancellation", None);
    let mut second = document("middle root", None);
    let third = document("last root", None);
    first.prerequisites = vec![second.id];
    first.lifecycle = WorkLifecycle::Cancelled;
    first.disposal_reason = Some("cancelled by the source author".into());
    second.prerequisites = vec![third.id];
    let mut initial = fixture().tree;
    initial.tasks = vec![first.clone(), second.clone(), third.clone()];
    initial.tasks.sort_by_key(|task| task.id.0);
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &initial, "enrol", 5).unwrap();
    let selected = without(&exported(&store), &[first.id, second.id, third.id]);
    let before = state(&store);
    let message = conflict(&exchange(&mut store, &selected, "remove-chain", 7).unwrap_err());
    assert!(
        message.contains(&format!("{}: lifecycle", first.id.0)),
        "{message}"
    );
    assert!(
        message.contains(&format!("{}: retained_reference", second.id.0)),
        "{message}"
    );
    assert!(
        message.contains(&format!("{}: retained_reference", third.id.0)),
        "{message}"
    );
    assert_eq!(state(&store), before);
}

#[test]
fn folder_removal_checks_sibling_root_execution_but_not_unrelated_roots() {
    let (mut store, fixture) = enrolled();
    claim_restored(&mut store, fixture.first, "worker", 6);
    let resolved = without(&exported(&store), &[fixture.second]);
    let before = state(&store);
    let message = conflict(&exchange(&mut store, &resolved, "remove-second", 7).unwrap_err());
    assert!(message.contains("ancestor_binding"), "{message}");
    assert!(message.contains("root execution"), "{message}");
    assert_eq!(state(&store), before);

    let unrelated = document("separate root", None);
    let mut extended = exported(&store);
    extended.tasks.push(unrelated.clone());
    extended.tasks.sort_by_key(|task| task.id.0);
    exchange(&mut store, &extended, "add-unrelated", 8).unwrap();
    exchange(
        &mut store,
        &without(&extended, &[unrelated.id]),
        "remove-unrelated",
        9,
    )
    .unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn folder_removal_delete_all_stays_enrolled_and_reserves_retired_ids() {
    let root = document("only task", None);
    let mut initial = fixture().tree;
    initial.tasks = vec![root.clone()];
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &initial, "enrol", 5).unwrap();
    let empty = without(&initial, &[root.id]);
    exchange(&mut store, &empty, "delete-all", 6).unwrap();
    assert_eq!(exported(&store), empty);
    let mut added = empty.clone();
    added.tasks.push(document("new task", None));
    exchange(&mut store, &added, "add-after-empty", 7).unwrap();
    let mut resurrection = added.clone();
    resurrection.tasks.push(root.clone());
    resurrection.tasks.sort_by_key(|task| task.id.0);
    let before = state(&store);
    let message = conflict(&exchange(&mut store, &resurrection, "resurrection", 8).unwrap_err());
    assert!(message.contains("id_collision"), "{message}");
    assert_eq!(state(&store), before);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn folder_removal_dry_run_replay_and_late_failure_are_atomic() {
    let (mut store, fixture) = enrolled();
    let resolved = without(&fixture.tree, &[fixture.first]);
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
    assert!(
        store
            .dry_run_task_sync_folder(request, "remove")
            .unwrap()
            .dry_run
    );
    assert_eq!(state(&store), before);
    // Failing baseline persistence is after item/events but before the new
    // membership can affect the export check.
    store.connection.execute_batch("CREATE TEMP TRIGGER refuse_folder_baseline BEFORE INSERT ON objects WHEN NEW.object_kind = 'task_sync_import_planning' BEGIN SELECT RAISE(ABORT, 'baseline failure'); END;").unwrap();
    let before_baseline = state(&store);
    assert!(store.apply_task_sync_folder(request, "remove").is_err());
    assert_eq!(state(&store), before_baseline);
    store
        .connection
        .execute_batch("DROP TRIGGER refuse_folder_baseline")
        .unwrap();
    // Failure at receipt persistence is later than the new membership/event writes.
    store.connection.execute_batch("CREATE TEMP TRIGGER refuse_folder_result BEFORE INSERT ON work_operation_results WHEN NEW.operation = 'apply_task_sync_folder' BEGIN SELECT RAISE(ABORT, 'late result failure'); END;").unwrap();
    let before_failure = state(&store);
    assert!(store.apply_task_sync_folder(request, "remove").is_err());
    assert_eq!(state(&store), before_failure);
    store
        .connection
        .execute_batch("DROP TRIGGER refuse_folder_result")
        .unwrap();
    let first = store.apply_task_sync_folder(request, "remove").unwrap();
    let committed = state(&store);
    assert_eq!(
        store.apply_task_sync_folder(request, "remove").unwrap(),
        first
    );
    assert_eq!(state(&store), committed);
    // A later legitimate local edit makes the old request's basis stale.
    // Exact receipt replay must still precede freshness/admission checks.
    let survivor = query::load_work_item(&store.connection, fixture.second).unwrap();
    store
        .revise_work(
            &crate::domain::ReviseWorkRequest {
                work_id: survivor.work_id,
                expected_revision: survivor.revision,
                patch: crate::domain::WorkRevisionPatch {
                    title: Some("later local planning".into()),
                    ..Default::default()
                },
                authority: WorkPlanningAuthority::Project,
                actor: actor("local-planner"),
                idempotency_key: "later-local-revision".into(),
                updated_at: at(7),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let later = state(&store);
    assert_eq!(
        store.apply_task_sync_folder(request, "remove").unwrap(),
        first
    );
    assert_eq!(state(&store), later);
    let changed_payload = edited(&resolved, fixture.second, |task| {
        task.title = "different intent".into();
    });
    let changed_request = TaskSyncFolderApply {
        tree: &changed_payload,
        ..request
    };
    assert!(matches!(
        store.apply_task_sync_folder(changed_request, "remove"),
        Err(StoreError::WorkOperationIdempotencyConflict { .. })
    ));
    assert_eq!(state(&store), later);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn folder_removal_reopen_and_projection_repair_preserve_durable_rows() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("folder-removal.db");
    let fixture = fixture();
    let selected = without(&fixture.tree, &[fixture.first]);
    let read_durable = |store: &SqliteStore| {
        state(store)
            .rows
            .into_iter()
            .filter(|(name, _)| {
                !super::super::super::is_rebuilt_projection_table(name)
                    && !name.starts_with("work_catalog_fts")
            })
            .collect::<Vec<_>>()
    };
    let before = {
        let mut store = SqliteStore::open(&database).unwrap();
        exchange(&mut store, &fixture.tree, "enrol", 5).unwrap();
        exchange(&mut store, &selected, "remove", 6).unwrap();
        assert!(store.verify_all().unwrap().is_healthy());
        read_durable(&store)
    };
    assert!(
        SqliteStore::repair_rebuildable_projections(&database)
            .unwrap()
            .is_healthy()
    );
    let store = SqliteStore::open(&database).unwrap();
    assert_eq!(exported(&store), selected);
    assert_eq!(read_durable(&store), before);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn folder_removal_retains_required_child_barrier_until_explicit_waiver() {
    use crate::storage::work::test_support::{checkpoint, complete, evidence};
    let parent = document("parent", None);
    let child = document("required child", Some(parent.id));
    let mut initial = fixture().tree;
    initial.tasks = vec![parent.clone(), child.clone()];
    initial.tasks.sort_by_key(|task| task.id.0);
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &initial, "enrol", 5).unwrap();
    exchange(
        &mut store,
        &without(&initial, &[child.id]),
        "remove-child",
        6,
    )
    .unwrap();
    assert_eq!(events(&store, parent.id), []);
    let claim = claim_restored(&mut store, parent.id, "parent-worker", 7);
    let current = query::load_work_item(&store.connection, parent.id).unwrap();
    let proof = evidence(&mut store, &current, &claim, "parent-worker", "proof", 8);
    checkpoint(
        &mut store,
        &current,
        &claim,
        "parent-worker",
        "checkpoint",
        9,
        std::slice::from_ref(&proof),
    );
    assert!(matches!(
        complete(&mut store, &current, &claim, "parent-worker", &proof, "before-waiver", 10),
        Err(StoreError::WorkCompletionRecoveryRequired {
            cause: crate::WorkCompletionRecoveryCause::RequiredChildUnsealed { child: blocked }, ..
        }) if blocked == child.id
    ));
    let waiver = store
        .waive_required_child(
            &crate::domain::WaiveRequiredChildRequest {
                parent_id: parent.id,
                child_id: child.id,
                expected_parent_revision: current.revision,
                reason: "The removed child is explicitly accepted as omitted".into(),
                actor: actor("parent-worker"),
                idempotency_key: "explicit-waiver".into(),
                waived_at: at(11),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    assert_eq!(waiver.waived_by, "parent-worker");
    let current = query::load_work_item(&store.connection, parent.id).unwrap();
    checkpoint(
        &mut store,
        &current,
        &claim,
        "parent-worker",
        "checkpoint-after-waiver",
        12,
        std::slice::from_ref(&proof),
    );
    let seal = complete(
        &mut store,
        &current,
        &claim,
        "parent-worker",
        &proof,
        "after-waiver",
        13,
    )
    .unwrap();
    assert_eq!(seal.required_child_waivers.len(), 1);
    assert_eq!(
        query::load_work_item(&store.connection, child.id)
            .unwrap()
            .lifecycle,
        WorkLifecycle::Cancelled
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn folder_removal_subtree_is_postorder_beside_additions_and_scalar_edits() {
    let mut fixture = fixture();
    let mut grandchild = document("grandchild", Some(fixture.first));
    grandchild.child_requirement = ChildRequirement::Optional;
    let grandchild_id = grandchild.id;
    fixture.tree.tasks.push(grandchild);
    fixture.tree.tasks.sort_by_key(|task| task.id.0);
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &fixture.tree, "enrol", 5).unwrap();
    let mut resolved = without(&fixture.tree, &[fixture.first, grandchild_id]);
    resolved = edited(&resolved, fixture.second, |task| {
        task.title = "edited sibling".into();
    });
    let added = document("new unrelated root", None);
    let added_id = added.id;
    resolved.tasks.push(added);
    resolved.tasks.sort_by_key(|task| task.id.0);
    exchange(&mut store, &resolved, "mixed", 6).unwrap();
    let order = store.connection.prepare(
        "SELECT work_id FROM work_feed_entries WHERE feed_kind = 'project' AND object_kind = 'work_event' ORDER BY position"
    ).unwrap().query_map([], |row| row.get::<_, String>(0)).unwrap()
        .collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(
        order,
        vec![
            fixture.second.0.to_string(),
            grandchild_id.0.to_string(),
            fixture.first.0.to_string()
        ]
    );
    assert_eq!(events(&store, added_id), []);
    assert_eq!(exported(&store), resolved);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn folder_removal_refusals_collect_with_other_deltas_and_roll_back_everything() {
    let (mut store, fixture) = enrolled();
    let claim = claim_restored(&mut store, fixture.first, "worker", 6);
    let mut resolved = without(&exported(&store), &[fixture.first, fixture.completed]);
    resolved = edited(&resolved, fixture.second, |task| {
        task.labels = vec!["unsupported".into()];
    });
    resolved.tasks.push(document("safe addition", None));
    resolved.tasks.sort_by_key(|task| task.id.0);
    let before = state(&store);
    let message = conflict(&exchange(&mut store, &resolved, "blocked-mixed", 7).unwrap_err());
    for expected in [
        format!("{}: active_run (run {})", fixture.first.0, claim.run_id.0),
        format!("{}: restored_completion", fixture.completed.0),
        format!("{}: unsupported_fields", fixture.second.0),
    ] {
        assert!(message.contains(&expected), "{expected}: {message}");
    }
    assert_eq!(state(&store), before);
}

#[test]
fn folder_removal_preserves_notes_and_retained_cancelled_members() {
    use crate::task_sync::notes::{SharedNote, SharedNoteKind};
    let mut fixture = fixture();
    fixture.tree.project.notes_included = true;
    let note = SharedNote {
        id: ObjectId::mint(),
        kind: SharedNoteKind::Generic,
        body: "historical observation".into(),
        author: SharedActor::from_actor(&actor("author")),
        recorded_at: at(2),
        origin_machine: "remote".into(),
    };
    fixture.tree = edited(&fixture.tree, fixture.first, |task| task.notes.push(note));
    let mut cancelled = document("retained cancellation", None);
    cancelled.lifecycle = WorkLifecycle::Cancelled;
    cancelled.disposal_reason = Some("already retired by its author".into());
    let cancelled_id = cancelled.id;
    fixture.tree.tasks.push(cancelled);
    fixture.tree.tasks.sort_by_key(|task| task.id.0);
    let mut store = SqliteStore::open_in_memory().unwrap();
    let profile = ExportProfile {
        include_notes: true,
        origin_machine: Some("host".into()),
    };
    let apply = |store: &mut SqliteStore, tree: &TaskTree, key: &str, second: i64| {
        let own = store.export_task_sync_folder(&project(), &profile).unwrap();
        store.apply_task_sync_folder(
            TaskSyncFolderApply {
                project: &project(),
                actor: &actor("folder-importer"),
                tree,
                export_basis: &own.export_basis,
                profile: &profile,
                complete: true,
                now: at(second),
            },
            key,
        )
    };
    apply(&mut store, &fixture.tree, "enrol", 5).unwrap();
    let origins = durable(&store);
    let resolved = without(&fixture.tree, &[fixture.first]);
    apply(&mut store, &resolved, "remove", 6).unwrap();
    assert_eq!(
        store
            .export_task_sync_folder(&project(), &profile)
            .unwrap()
            .tree,
        resolved
    );
    assert_eq!(durable(&store), origins);
    assert_eq!(events(&store, cancelled_id), []);
    let all = store
        .export_task_tree_with_options(&project(), &profile.options())
        .unwrap();
    assert_eq!(
        all.tree
            .tasks
            .iter()
            .find(|task| task.id == fixture.first)
            .unwrap()
            .notes
            .len(),
        1
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn folder_removal_preserves_imported_started_activity_after_reopen_and_repair() {
    use crate::task_sync::activity::{ActivityMap, ActivityNotice, ActivityState};

    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("folder-removal-activity.db");
    let mut fixture = fixture();
    fixture.tree.project.notes_included = true;
    let activity_id = ObjectId::mint();
    let notice = ActivityNotice {
        activity_id: activity_id.clone(),
        task_id: fixture.first,
        origin_machine: "remote".into(),
        actor: SharedActor::from_actor(&actor("remote-worker")),
        state: ActivityState::Started,
        state_revision: 1,
        event_id: activity_id.clone(),
    };
    let activity: ActivityMap = std::iter::once((
        "remote".into(),
        std::iter::once((activity_id, notice)).collect(),
    ))
    .collect();
    fixture.tree = edited(&fixture.tree, fixture.first, |task| {
        task.activity = activity.clone();
    });
    let selected = without(&fixture.tree, &[fixture.first]);
    let profile = ExportProfile {
        include_notes: true,
        origin_machine: Some("host".into()),
    };
    let assert_notices = |store: &SqliteStore| {
        assert_eq!(
            store
                .export_task_sync_folder(&project(), &profile)
                .unwrap()
                .tree,
            selected
        );
        let all = store
            .export_task_tree_with_options(&project(), &profile.options())
            .unwrap();
        let retired = all
            .tree
            .tasks
            .iter()
            .find(|task| task.id == fixture.first)
            .unwrap();
        assert_eq!(retired.lifecycle, WorkLifecycle::Cancelled);
        assert_eq!(retired.activity, activity);
        assert!(
            all.tree
                .tasks
                .iter()
                .all(|task| !task.activity.contains_key("host"))
        );
        assert!(store.verify_all().unwrap().is_healthy());
    };
    let read_durable = |store: &SqliteStore| {
        state(store)
            .rows
            .into_iter()
            .filter(|(name, _)| {
                !super::super::super::is_rebuilt_projection_table(name)
                    && !name.starts_with("work_catalog_fts")
            })
            .collect::<Vec<_>>()
    };
    let before = {
        let mut store = SqliteStore::open(&database).unwrap();
        for (tree, key, second) in [
            (&fixture.tree, "enrol-activity", 5),
            (&selected, "remove-activity", 6),
        ] {
            let own = store.export_task_sync_folder(&project(), &profile).unwrap();
            store
                .apply_task_sync_folder(
                    TaskSyncFolderApply {
                        project: &project(),
                        actor: &actor("folder-importer"),
                        tree,
                        export_basis: &own.export_basis,
                        profile: &profile,
                        complete: true,
                        now: at(second),
                    },
                    key,
                )
                .unwrap();
        }
        assert_notices(&store);
        read_durable(&store)
    };
    {
        let store = SqliteStore::open(&database).unwrap();
        assert_notices(&store);
        assert_eq!(read_durable(&store), before);
    }
    assert!(
        SqliteStore::repair_rebuildable_projections(&database)
            .unwrap()
            .is_healthy()
    );
    let store = SqliteStore::open(&database).unwrap();
    assert_notices(&store);
    assert_eq!(read_durable(&store), before);
}

#[test]
fn folder_removal_of_a_released_prior_run_refuses_without_expiring_authority() {
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
                reason: "hand back before folder exchange".into(),
                waiver_reason: Some("no contribution".into()),
                actor: actor("worker"),
                idempotency_key: "release-worker".into(),
                released_at: at(7),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let selected = without(&exported(&store), &[fixture.first]);
    let before = state(&store);
    let message = conflict(&exchange(&mut store, &selected, "remove-released", 8).unwrap_err());
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
fn folder_removal_shared_ancestors_are_read_once_per_admission() {
    use super::super::folder_admission::ELIGIBILITY_READS;
    let root = document("shared root", None);
    let children = (0..40)
        .map(|index| document(&format!("child {index}"), Some(root.id)))
        .collect::<Vec<_>>();
    let mut initial = fixture().tree;
    initial.tasks = vec![root.clone()];
    initial.tasks.extend(children.iter().cloned());
    initial.tasks.sort_by_key(|task| task.id.0);
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &initial, "enrol", 5).unwrap();
    let selected = without(
        &initial,
        &children.iter().map(|task| task.id).collect::<Vec<_>>(),
    );
    ELIGIBILITY_READS.with(|count| count.set(0));
    exchange(&mut store, &selected, "remove-children", 6).unwrap();
    // One eligibility read for each child and their shared ancestor in
    // preflight, and one deliberate write-boundary recheck per child.
    assert_eq!(
        ELIGIBILITY_READS.with(std::cell::Cell::get),
        2 * children.len() + 1
    );
    assert_eq!(exported(&store), selected);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn folder_removal_mixed_with_addition_validates_the_retained_local_graph() {
    let p = document("p", None);
    let q = document("q", None);
    let removed = document("disjoint removal", None);
    let mut initial = fixture().tree;
    initial.tasks = vec![p.clone(), q.clone(), removed.clone()];
    initial.tasks.sort_by_key(|task| task.id.0);
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &initial, "enrol", 5).unwrap();
    let parent = query::load_work_item(&store.connection, p.id).unwrap();
    let decomposition = store
        .decompose_work(
            &crate::domain::DecomposeWorkRequest {
                parent_id: p.id,
                expected_parent_revision: parent.revision,
                children: vec![crate::storage::work::test_support::child(
                    "h",
                    ChildRequirement::Required,
                    "local-required-child",
                )],
                prerequisites: Vec::new(),
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner"),
                idempotency_key: "decompose-p".into(),
                created_at: at(6),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let local =
        query::load_work_item(&store.connection, decomposition.children[0].work_id).unwrap();
    store
        .add_work_prerequisite(
            &crate::domain::ChangeWorkPrerequisiteRequest {
                work_id: local.work_id,
                prerequisite_id: q.id,
                expected_revision: local.revision,
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner"),
                idempotency_key: "local-needs-q".into(),
                changed_at: at(7),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let mut added = document("x", Some(q.id));
    added.prerequisites = vec![p.id];
    let added_id = added.id;
    let mut selected = without(&initial, &[removed.id]);
    selected.tasks.push(added);
    selected.tasks.sort_by_key(|task| task.id.0);
    selected.validate_folder().unwrap();
    // R and the original import union omit native H. Only C minus D plus R
    // sees the cycle P -> H -> Q -> X -> P. The safe disjoint removal must
    // remain unapplied when the addition is refused.
    let before = state(&store);
    let message = conflict(&exchange(&mut store, &selected, "mixed-cycle", 8).unwrap_err());
    assert!(
        message.contains(&format!("{}: retained_graph", added_id.0)),
        "{message}"
    );
    assert!(
        !message.contains(&format!("{}:", removed.id.0)),
        "{message}"
    );
    assert_eq!(state(&store), before);
}
