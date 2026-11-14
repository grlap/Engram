use super::*;

fn tree(tasks: Vec<TaskDocument>) -> TaskTree {
    let mut tree = fixture().tree;
    tree.tasks = tasks;
    tree.tasks.sort_by_key(|task| task.id.0);
    tree
}

#[test]
fn a_wholly_refused_first_import_has_no_baseline_write_or_changing_replay() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let foreign = store
        .create_work(
            &root_request("other-project", "existing identity", 0),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let mut collision = document("cannot replace a foreign identity", None);
    collision.id = foreign.work_id;
    let incoming = tree(vec![collision]);
    let before = state(&store);
    let first = exchange(&mut store, &incoming, "all-refused-first-import", 5).unwrap();
    assert!(!first.changed);
    assert_eq!(first.tasks[0].status, TaskSyncTaskStatus::Refused);
    assert_eq!(state(&store), before);
    assert!(
        super::super::super::super::baseline::load(&store.connection, &project())
            .unwrap()
            .is_none()
    );
    let repeated = exchange(&mut store, &incoming, "all-refused-first-import", 6).unwrap();
    assert_eq!(repeated, first);
    assert_eq!(state(&store), before);
}

#[test]
fn partial_baselines_use_local_relations_for_held_and_refused_members() {
    for held in [false, true] {
        for transitive in [false, true] {
            let mut u = document("unapplied owner", None);
            let mut v = document("middle", None);
            let w = document("last", None);
            let z = document("independent", None);
            u.prerequisites.push(v.id);
            if transitive {
                v.prerequisites.push(w.id);
            }
            let mut store = SqliteStore::open_in_memory().unwrap();
            exchange(
                &mut store,
                &tree(vec![u.clone(), v.clone(), w.clone(), z.clone()]),
                "initial-edges",
                5,
            )
            .unwrap();
            let item = store.get_work_item(u.id).unwrap();
            store
                .remove_work_prerequisite(
                    &crate::domain::ChangeWorkPrerequisiteRequest {
                        work_id: u.id,
                        prerequisite_id: v.id,
                        expected_revision: item.revision,
                        authority: WorkPlanningAuthority::Project,
                        actor: actor("planner"),
                        idempotency_key: "local-edge-removal".into(),
                        changed_at: at(6),
                    },
                    &crate::DevelopmentNoopRedactor,
                )
                .unwrap();
            if held {
                claim_restored(&mut store, u.id, "local-executor", 7);
            }
            let incoming = edited(&exported(&store), u.id, |task| {
                if held {
                    task.title = "Pending title".into();
                } else {
                    task.kind = WorkItemKind::Feature;
                }
            });
            let last = if transitive { w.id } else { v.id };
            let incoming = edited(&incoming, last, |task| {
                task.prerequisites.push(u.id);
            });
            let incoming = edited(&incoming, z.id, |task| {
                task.title = "Independent change applied".into();
            });
            let result = exchange(&mut store, &incoming, "mixed-old-baseline", 8).unwrap();
            let owner = result.tasks.iter().find(|task| task.task == u.id).unwrap();
            assert_eq!(
                owner.status,
                if held {
                    TaskSyncTaskStatus::Held
                } else {
                    TaskSyncTaskStatus::Refused
                }
            );
            for id in [last, z.id] {
                assert_eq!(
                    result
                        .tasks
                        .iter()
                        .find(|task| task.task == id)
                        .unwrap()
                        .status,
                    TaskSyncTaskStatus::Applied
                );
            }
            let baseline =
                super::super::super::super::baseline::load(&store.connection, &project())
                    .unwrap()
                    .unwrap();
            assert_eq!(
                baseline
                    .tree
                    .tasks
                    .iter()
                    .find(|task| task.id == u.id)
                    .unwrap()
                    .prerequisites,
                []
            );
            baseline.tree.validate_folder().unwrap();
            assert_eq!(
                store.get_work_item(z.id).unwrap().title,
                "Independent change applied"
            );
            if held {
                // The local support document did not consume the pending edit.
                let retry = exchange(&mut store, &incoming, "fresh-after-expiry", 4000).unwrap();
                assert_eq!(
                    retry
                        .tasks
                        .iter()
                        .find(|task| task.task == u.id)
                        .unwrap()
                        .status,
                    TaskSyncTaskStatus::Applied
                );
                assert_eq!(store.get_work_item(u.id).unwrap().title, "Pending title");
            }
            let health = store.verify_all().unwrap();
            assert!(health.is_healthy(), "{health:?}");
        }
    }
}

#[test]
fn prerequisite_additions_use_the_effective_endpoint_lifecycle() {
    for lifecycle in [
        WorkLifecycle::Proposed,
        WorkLifecycle::Completed,
        WorkLifecycle::Cancelled,
    ] {
        let mut target = document("non-open local endpoint", None);
        target.lifecycle = lifecycle;
        if lifecycle == WorkLifecycle::Completed {
            target.completion = Some(SharedCompletion {
                summary: "Already completed".into(),
                completed_at: at(2),
                actor: SharedActor::from_actor(&actor("reporter")),
            });
        } else if lifecycle == WorkLifecycle::Cancelled {
            target.disposal_reason = Some("No longer needed".into());
        }
        let owner = document("relation owner", None);
        let independent = document("independent", None);
        let mut store = SqliteStore::open_in_memory().unwrap();
        exchange(
            &mut store,
            &tree(vec![target.clone(), owner.clone(), independent.clone()]),
            "initial-lifecycles",
            5,
        )
        .unwrap();
        let incoming = edited(&exported(&store), target.id, |task| {
            task.lifecycle = WorkLifecycle::Open;
            task.completion = None;
            task.disposal_reason = None;
        });
        let incoming = edited(&incoming, owner.id, |task| {
            task.prerequisites.push(target.id);
        });
        let incoming = edited(&incoming, independent.id, |task| {
            task.title = "Applied beside endpoint refusal".into();
        });
        let result = exchange(&mut store, &incoming, "effective-endpoint", 6).unwrap();
        assert_eq!(
            result
                .tasks
                .iter()
                .find(|task| task.task == owner.id)
                .unwrap()
                .status,
            TaskSyncTaskStatus::Refused
        );
        assert_eq!(store.get_work_item(target.id).unwrap().lifecycle, lifecycle);
        assert_eq!(
            store.get_work_item(independent.id).unwrap().title,
            "Applied beside endpoint refusal"
        );
        assert_eq!(
            exported(&store)
                .tasks
                .iter()
                .find(|task| task.id == owner.id)
                .unwrap()
                .prerequisites,
            []
        );
        let health = store.verify_all().unwrap();
        assert!(health.is_healthy(), "{lifecycle:?}: {health:?}");
    }
}

#[test]
fn a_required_child_waiver_on_a_dormant_run_does_not_hold_planning() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let parent = store
        .create_work(
            &root_request(PROJECT, "native parent", 0),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    assert!(parent.active_run_id.is_some());
    let decomposition = store
        .decompose_work(
            &crate::domain::DecomposeWorkRequest {
                parent_id: parent.work_id,
                expected_parent_revision: parent.revision,
                children: vec![crate::storage::work::test_support::child(
                    "child",
                    ChildRequirement::Required,
                    "required child",
                )],
                prerequisites: Vec::new(),
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner"),
                idempotency_key: "decompose-dormant-parent".into(),
                created_at: at(1),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let child = store
        .get_work_item(decomposition.children[0].work_id)
        .unwrap();
    let parent = store.get_work_item(parent.work_id).unwrap();
    store
        .reject_required_child(
            &crate::domain::RejectRequiredChildRequest {
                work_id: child.work_id,
                expected_work_revision: child.revision,
                expected_parent_revision: Some(parent.revision),
                reason: "Not needed".into(),
                actor: actor("planner"),
                idempotency_key: "reject-dormant-child".into(),
                rejected_at: at(2),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let history = events(&store, parent.work_id);
    let waiver = history
        .iter()
        .find(|event| matches!(event.transition, WorkTransition::RequiredChildWaived { .. }))
        .unwrap();
    assert_eq!(waiver.run_id, parent.active_run_id);
    let claim_count: i64 = store
        .connection
        .query_row("SELECT COUNT(*) FROM work_claims", [], |row| row.get(0))
        .unwrap();
    assert_eq!(claim_count, 0);
    let incoming = edited(&exported(&store), parent.work_id, |task| {
        task.title = "Administrative waiver permits editing".into();
    });
    let added = document("new child after waiver", Some(parent.work_id));
    let mut incoming = incoming;
    incoming.tasks.push(added.clone());
    incoming.tasks.sort_by_key(|task| task.id.0);
    let result = exchange(&mut store, &incoming, "after-administrative-waiver", 5).unwrap();
    assert!(
        result
            .tasks
            .iter()
            .all(|task| task.status == TaskSyncTaskStatus::Applied),
        "{result:?}"
    );
    assert!(store.get_work_item(added.id).is_ok());
    assert_eq!(
        store.get_work_item(parent.work_id).unwrap().title,
        "Administrative waiver permits editing"
    );
    let health = store.verify_all().unwrap();
    assert!(health.is_healthy(), "{health:?}");
}

#[test]
fn folder_blockers_refuse_proposed_owners_and_bind_dormant_run_events() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let native = store
        .create_work(
            &root_request(PROJECT, "native owner", 0),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let mut proposed = document("proposed owner", None);
    proposed.lifecycle = WorkLifecycle::Proposed;
    let independent = document("independent", None);
    let mut initial = exported(&store);
    initial
        .tasks
        .extend([proposed.clone(), independent.clone()]);
    initial.tasks.sort_by_key(|task| task.id.0);
    exchange(&mut store, &initial, "initial-blocker-owners", 5).unwrap();
    let blocker = |detail: &str| SharedBlocker {
        blocker_id: uuid::Uuid::new_v4().to_string(),
        kind: WorkBlockerKind::Manual,
        detail: detail.into(),
        created_by: SharedActor::from_actor(&actor("reporter")),
        created_at: at(1),
    };
    let incoming = edited(&exported(&store), proposed.id, |task| {
        task.blockers.push(blocker("not open"));
    });
    let incoming = edited(&incoming, native.work_id, |task| {
        task.blockers.push(blocker("dormant owner"));
    });
    let incoming = edited(&incoming, independent.id, |task| {
        task.title = "Applied beside blocker refusal".into();
    });
    let result = exchange(&mut store, &incoming, "incoming-blockers", 6).unwrap();
    assert_eq!(
        result
            .tasks
            .iter()
            .find(|task| task.task == proposed.id)
            .unwrap()
            .status,
        TaskSyncTaskStatus::Refused
    );
    assert_eq!(
        store.get_work_item(independent.id).unwrap().title,
        "Applied beside blocker refusal"
    );
    let addition_event = events(&store, native.work_id)
        .into_iter()
        .find(|event| matches!(event.transition, WorkTransition::Blocked { .. }))
        .unwrap();
    assert_eq!(addition_event.run_id, native.active_run_id);
    assert_eq!(
        addition_event.run.as_ref().map(|run| run.run_id),
        native.active_run_id
    );
    let incoming = edited(&exported(&store), native.work_id, |task| {
        task.blockers.clear();
    });
    exchange(&mut store, &incoming, "clear-dormant-blocker", 7).unwrap();
    let cleared = events(&store, native.work_id)
        .into_iter()
        .find(|event| matches!(event.transition, WorkTransition::Unblocked { .. }))
        .unwrap();
    assert_eq!(cleared.run_id, native.active_run_id);
    assert_eq!(
        cleared.run.as_ref().map(|run| run.run_id),
        native.active_run_id
    );
    let health = store.verify_all().unwrap();
    assert!(health.is_healthy(), "{health:?}");
}
