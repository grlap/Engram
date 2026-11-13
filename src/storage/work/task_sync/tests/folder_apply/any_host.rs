use super::*;
use crate::storage::TaskSyncTaskStatus;

mod repair;
mod repair2;
mod repair3;

#[test]
fn unrelated_native_history_does_not_join_the_import_origin_graph() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let p = document("original prerequisite", None);
    let mut q = document("original dependent", None);
    q.prerequisites.push(p.id);
    let mut initial = fixture().tree;
    initial.tasks = vec![p.clone(), q.clone()];
    initial.tasks.sort_by_key(|task| task.id.0);
    exchange(&mut store, &initial, "initial-origins", 5).unwrap();
    let q_item = store.get_work_item(q.id).unwrap();
    store
        .remove_work_prerequisite(
            &crate::domain::ChangeWorkPrerequisiteRequest {
                work_id: q.id,
                prerequisite_id: p.id,
                expected_revision: q_item.revision,
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner"),
                idempotency_key: "remove-original-edge".into(),
                changed_at: at(6),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let p_item = store.get_work_item(p.id).unwrap();
    let decomposition = store
        .decompose_work(
            &crate::domain::DecomposeWorkRequest {
                parent_id: p.id,
                expected_parent_revision: p_item.revision,
                children: vec![crate::storage::work::test_support::child(
                    "native-child",
                    ChildRequirement::Required,
                    "native child",
                )],
                prerequisites: vec![crate::domain::ChildWorkPrerequisite {
                    work_key: "native-child".into(),
                    prerequisite: crate::domain::WorkDependencyRef::Existing(q.id),
                }],
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner"),
                idempotency_key: "native-decomposition".into(),
                created_at: at(7),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let native = decomposition.children[0].work_id;
    // Q's immutable origin still requires P; native H requires Q and is
    // required by P. That old union is cyclic, but current native planning
    // is valid and no retained import refers to H.
    let health = store.verify_all().unwrap();
    assert!(health.is_healthy(), "{health:?}");
    // Strict delta admission seeds the origin component with every edited
    // payload identity. A native seed alone must not pull its creation
    // closure into the retained import origins.
    let origin = store
        .capture_task_sync_graph_component(
            &project(),
            crate::task_sync::TaskSyncGraphView::Origin,
            [native],
        )
        .unwrap();
    assert!(origin.missing.contains(&native));
    assert!(!origin.documents.contains_key(&native));
    let safe = document("unrelated incoming task", None);
    let mut incoming = exported(&store);
    incoming.tasks.push(safe.clone());
    incoming.tasks.sort_by_key(|task| task.id.0);
    assert!(
        exchange(&mut store, &incoming, "safe-independent-import", 8)
            .unwrap()
            .changed
    );
    assert!(store.verify_all().unwrap().is_healthy());
    // Retaining H as a new immutable endpoint would close that origin
    // cycle. Refuse its incoming user, while admitting an independent task.
    let mut bad_reference = document("incoming user of incompatible history", None);
    bad_reference.prerequisites.push(native);
    let independent = document("another independent task", None);
    let mut incoming = exported(&store);
    incoming
        .tasks
        .extend([bad_reference.clone(), independent.clone()]);
    incoming.tasks.sort_by_key(|task| task.id.0);
    let result = exchange(&mut store, &incoming, "selected-native-history", 9).unwrap();
    let refused = result
        .tasks
        .iter()
        .find(|task| task.task == bad_reference.id)
        .unwrap();
    assert_eq!(refused.status, TaskSyncTaskStatus::Refused);
    assert_eq!(refused.reason.as_deref(), Some("origin_graph"));
    assert!(store.get_work_item(bad_reference.id).is_err());
    assert!(store.get_work_item(independent.id).is_ok());
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn first_folder_restores_terminal_ancestors_and_their_open_descendants() {
    for lifecycle in [
        WorkLifecycle::Cancelled,
        WorkLifecycle::Completed,
        WorkLifecycle::Superseded,
    ] {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let mut ancestor = document("remote terminal ancestor", None);
        ancestor.lifecycle = lifecycle;
        let child = document("remote open child", Some(ancestor.id));
        let grandchild = document("remote open grandchild", Some(child.id));
        let replacement = document("remote replacement", None);
        match lifecycle {
            WorkLifecycle::Completed => {
                ancestor.completion = Some(SharedCompletion {
                    summary: "Reported remotely".into(),
                    completed_at: at(2),
                    actor: SharedActor::from_actor(&actor("remote-author")),
                });
            }
            WorkLifecycle::Cancelled | WorkLifecycle::Superseded => {
                ancestor.disposal_reason = Some("Reported remotely".into());
                if lifecycle == WorkLifecycle::Superseded {
                    ancestor.superseded_by = Some(replacement.id);
                }
            }
            _ => unreachable!(),
        }
        let mut incoming = fixture().tree;
        incoming.tasks = vec![ancestor.clone(), child.clone(), grandchild.clone()];
        if lifecycle == WorkLifecycle::Superseded {
            incoming.tasks.push(replacement);
        }
        incoming.tasks.sort_by_key(|task| task.id.0);
        let result = exchange(&mut store, &incoming, "initial-terminal-subtree", 5).unwrap();
        assert_eq!(result.tasks.len(), incoming.tasks.len());
        assert!(
            result
                .tasks
                .iter()
                .all(|task| task.status == TaskSyncTaskStatus::Applied),
            "{lifecycle:?}: {result:?}"
        );
        assert_eq!(exported(&store), incoming);
        assert_eq!(
            store.get_work_item(ancestor.id).unwrap().lifecycle,
            lifecycle
        );
        for id in [ancestor.id, child.id, grandchild.id] {
            let item = store.get_work_item(id).unwrap();
            assert!(item.restored && item.active_run_id.is_none());
            assert_eq!(events(&store, id), []);
        }
        let counts: (i64, i64, i64) = store.connection.query_row(
            "SELECT (SELECT COUNT(*) FROM work_runs), (SELECT COUNT(*) FROM work_claims), (SELECT COUNT(*) FROM work_completion_seals)",
            [], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)),
        ).unwrap();
        assert_eq!(counts, (0, 0, 0));
        let health = store.verify_all().unwrap();
        assert!(health.is_healthy(), "{lifecycle:?}: {health:?}");
    }
}

#[test]
fn first_folder_applies_independent_tasks_and_keeps_refused_ids_out_of_its_baseline() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let other = store
        .create_work(
            &root_request("other-project", "held native identity", 0),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let mut collision = document("cannot replace another project's identity", None);
    collision.id = other.work_id;
    let child = document("depends on the refused addition", Some(collision.id));
    let safe = document("independent task", None);
    let mut incoming = fixture().tree;
    incoming.tasks = vec![collision.clone(), child.clone(), safe.clone()];
    incoming.tasks.sort_by_key(|task| task.id.0);
    assert!(
        super::super::super::baseline::load(&store.connection, &project())
            .unwrap()
            .is_none()
    );
    let before = state(&store);
    let own = basis(&store);
    let importer = actor("folder-importer");
    let project_id = project();
    let selection = profile();
    let dry = store
        .dry_run_task_sync_folder(
            TaskSyncFolderApply {
                project: &project_id,
                actor: &importer,
                tree: &incoming,
                export_basis: &own,
                profile: &selection,
                complete: true,
                now: at(5),
            },
            "initial-partial",
        )
        .unwrap();
    assert!(dry.dry_run && dry.changed);
    assert_eq!(state(&store), before);
    let result = exchange(&mut store, &incoming, "initial-partial", 5).unwrap();
    assert_eq!(result.tasks, dry.tasks);
    assert!(result.changed);
    for (id, status, reason) in [
        (
            collision.id,
            TaskSyncTaskStatus::Refused,
            Some("id_collision"),
        ),
        (child.id, TaskSyncTaskStatus::Held, Some("ancestor_binding")),
        (safe.id, TaskSyncTaskStatus::Applied, None),
    ] {
        let outcome = result.tasks.iter().find(|task| task.task == id).unwrap();
        assert_eq!(outcome.status, status);
        assert_eq!(outcome.reason.as_deref(), reason);
    }
    assert_eq!(store.get_work_item(other.work_id).unwrap(), other);
    assert_eq!(exported(&store).tasks, vec![safe.clone()]);
    assert!(store.get_work_item(child.id).is_err());
    let baseline = super::super::super::baseline::load(&store.connection, &project())
        .unwrap()
        .unwrap();
    assert_eq!(baseline.tree.tasks, vec![safe]);
    let after = state(&store);
    // A lost-response retry with the original key and basis recovers the
    // partial receipt even though applying the safe task changed the basis.
    let replay = store
        .apply_task_sync_folder(
            TaskSyncFolderApply {
                project: &project_id,
                actor: &importer,
                tree: &incoming,
                export_basis: &own,
                profile: &selection,
                complete: true,
                now: at(6),
            },
            "initial-partial",
        )
        .unwrap();
    assert_eq!(replay, result);
    assert_eq!(state(&store), after);
    // A fresh-basis retry gets a fresh operation key and reconsiders the
    // held/refused tasks rather than permanently caching their outcome.
    let retry = exchange(&mut store, &incoming, "initial-partial-fresh-retry", 7).unwrap();
    assert!(!retry.changed && retry.tasks.len() == 2);
    assert_eq!(state(&store), after);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn imported_children_and_prerequisites_can_reference_a_native_creation() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let native = store
        .create_work(
            &root_request(PROJECT, "creator", 0),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let child = document("Child created on B", Some(native.work_id));
    let mut dependent = document("Dependent created on B", None);
    dependent.prerequisites.push(native.work_id);
    let mut incoming = exported(&store);
    incoming.tasks.extend([child.clone(), dependent.clone()]);
    incoming.tasks.sort_by_key(|task| task.id.0);
    let result = exchange(&mut store, &incoming, "native-endpoints", 5).unwrap();
    assert!(
        result
            .tasks
            .iter()
            .all(|task| task.status == TaskSyncTaskStatus::Applied)
    );
    assert_eq!(
        store.get_work_item(child.id).unwrap().parent_id,
        Some(native.work_id)
    );
    assert_eq!(exported(&store), incoming);
    let health = store.verify_all().unwrap();
    assert!(health.is_healthy(), "{health:?}");
    let original = store
        .capture_task_sync_graph_component(
            &project(),
            crate::task_sync::TaskSyncGraphView::Origin,
            [child.id, dependent.id],
        )
        .unwrap();
    assert!(original.missing.is_empty());
    assert_eq!(original.documents.len(), 3);
    let initial_title = original.documents[&native.work_id].title.clone();
    let changed = edited(&incoming, native.work_id, |task| {
        task.title = "Mutable new title".into();
    });
    exchange(&mut store, &changed, "change-current-not-origin", 6).unwrap();
    let original = store
        .capture_task_sync_graph_component(
            &project(),
            crate::task_sync::TaskSyncGraphView::Origin,
            [child.id],
        )
        .unwrap();
    assert_eq!(original.documents[&native.work_id].title, initial_title);
}

#[test]
fn native_creator_accepts_returned_planning_without_an_execution_history_ban() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let original = store
        .create_work(
            &root_request(PROJECT, "creator", 0),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let mut incoming = exported(&store);
    let task = incoming
        .tasks
        .iter_mut()
        .find(|task| task.id == original.work_id)
        .unwrap();
    task.title = "Implemented on another host".into();
    task.outcome = "Return the implementation result".into();
    task.acceptance = vec!["Works after a round trip".into()];
    task.labels = vec!["remote-work".into()];
    task.assigned_to = Some("remote-implementer".into());
    let result = exchange(&mut store, &incoming, "returned-planning", 5).unwrap();
    assert!(result.changed);
    assert_eq!(result.tasks.len(), 1);
    assert_eq!(result.tasks[0].status, TaskSyncTaskStatus::Applied);
    let current = store.get_work_item(original.work_id).unwrap();
    assert_eq!(current.created_by, original.created_by);
    assert_eq!(current.active_run_id, original.active_run_id);
    assert_eq!(current.acceptance, ["Works after a round trip"]);
    assert_eq!(exported(&store), incoming);
    let health = store.verify_all().unwrap();
    assert!(health.is_healthy(), "{health:?}");
}

#[test]
fn native_dormant_run_closes_without_a_seal_when_completion_is_reported() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("reported.db");
    let mut store = SqliteStore::open(&database).unwrap();
    let original = store
        .create_work(
            &root_request(PROJECT, "creator", 0),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let run_id = original.active_run_id.unwrap();
    let obligations_before: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM work_run_obligations WHERE run_id=?1",
            [run_id.0.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    let mut incoming = exported(&store);
    let task = incoming
        .tasks
        .iter_mut()
        .find(|task| task.id == original.work_id)
        .unwrap();
    task.lifecycle = WorkLifecycle::Completed;
    task.completion = Some(SharedCompletion {
        summary: "Completed while disconnected on B".into(),
        completed_at: at(3),
        actor: SharedActor::from_actor(&actor("implementer-b")),
    });
    let result = exchange(&mut store, &incoming, "returned-completion", 5).unwrap();
    assert_eq!(result.tasks[0].status, TaskSyncTaskStatus::Applied);
    let current = store.get_work_item(original.work_id).unwrap();
    assert_eq!(current.created_by, original.created_by);
    assert_eq!(current.lifecycle, WorkLifecycle::Completed);
    assert!(current.active_run_id.is_none());
    assert!(
        store
            .work_completed_by_restored_record(original.work_id)
            .unwrap()
    );
    let run = query::load_work_run(&store.connection, run_id).unwrap();
    assert_eq!(run.state, crate::WorkRunState::Cancelled);
    assert!(run.completion_seal.is_none());
    let seals: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM work_completion_seals WHERE work_id=?1",
            [original.work_id.0.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(seals, 0);
    let obligations_after: i64 = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM work_run_obligations WHERE run_id=?1",
            [run_id.0.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(obligations_after, obligations_before);
    assert_eq!(exported(&store), incoming);
    let health = store.verify_all().unwrap();
    assert!(health.is_healthy(), "{health:?}");
    let canonical = objects(&store);
    let items = item_bytes(&store);
    drop(store);
    let repaired = SqliteStore::repair_rebuildable_projections(&database).unwrap();
    assert!(repaired.is_healthy(), "{repaired:?}");
    let migration = directory.path().join("reported-migration.json");
    crate::storage::migration::export_json(&database, &migration).unwrap();
    let target = directory.path().join("migrated.db");
    crate::storage::migration::import_json(&migration, &target).unwrap();
    let migrated = SqliteStore::open(&target).unwrap();
    assert_eq!(
        objects(&migrated),
        canonical,
        "the event and report ids survive migration"
    );
    assert_eq!(item_bytes(&migrated), items);
    assert!(
        migrated
            .work_completed_by_restored_record(original.work_id)
            .unwrap()
    );
    let health = migrated.verify_all().unwrap();
    assert!(health.is_healthy(), "{health:?}");
}

#[test]
fn a_held_task_does_not_consume_its_change_or_block_an_independent_edit() {
    let (mut store, fixture) = enrolled();
    let claim = claim_restored(&mut store, fixture.first, "local-executor", 6);
    let incoming = edited(
        &edited(&exported(&store), fixture.first, |task| {
            task.title = "Held incoming title".into();
        }),
        fixture.completed,
        |task| task.title = "Refused terminal edit".into(),
    );
    let incoming = edited(&incoming, fixture.second, |task| {
        task.title = "Independent edit".into();
    });
    let result = exchange(&mut store, &incoming, "mixed-outcomes", 7).unwrap();
    let status = |id| {
        result
            .tasks
            .iter()
            .find(|task| task.task == id)
            .unwrap()
            .status
    };
    assert_eq!(status(fixture.first), TaskSyncTaskStatus::Held);
    assert_eq!(status(fixture.completed), TaskSyncTaskStatus::Refused);
    assert_eq!(status(fixture.second), TaskSyncTaskStatus::Applied);
    assert_eq!(
        store.get_work_item(fixture.second).unwrap().title,
        "Independent edit"
    );
    assert_ne!(
        store.get_work_item(fixture.first).unwrap().title,
        "Held incoming title"
    );
    let item = store.get_work_item(fixture.first).unwrap();
    store
        .release_work(
            &ReleaseWorkRequest {
                work_id: item.work_id,
                run_id: claim.run_id,
                claim_id: claim.claim_id,
                expected_work_revision: item.revision,
                holder: SessionId("local-executor".into()),
                claim_fence: claim.fence,
                waiver_reason: Some("No contribution yet".into()),
                actor: actor("local-executor"),
                reason: "Continue after incoming planning".into(),
                idempotency_key: "release-for-import".into(),
                released_at: at(8),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let result = exchange(&mut store, &incoming, "retry-held-planning", 9).unwrap();
    assert_eq!(
        result
            .tasks
            .iter()
            .find(|task| task.task == fixture.first)
            .unwrap()
            .status,
        TaskSyncTaskStatus::Applied
    );
    assert_eq!(
        store.get_work_item(fixture.first).unwrap().title,
        "Held incoming title"
    );
    let health = store.verify_all().unwrap();
    assert!(health.is_healthy(), "{health:?}");
}
