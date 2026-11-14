use super::*;

fn report(tree: &TaskTree, ids: &[WorkId]) -> TaskTree {
    let mut incoming = tree.clone();
    for task in incoming
        .tasks
        .iter_mut()
        .filter(|task| ids.contains(&task.id))
    {
        task.lifecycle = WorkLifecycle::Completed;
        task.completion = Some(SharedCompletion {
            summary: "Completed on another host".into(),
            completed_at: at(3),
            actor: SharedActor::from_actor(&actor("remote-executor")),
        });
    }
    incoming
}

fn release(store: &mut SqliteStore, id: WorkId, claim: &WorkClaim, second: i64) {
    let item = store.get_work_item(id).unwrap();
    store
        .release_work(
            &ReleaseWorkRequest {
                work_id: id,
                run_id: claim.run_id,
                claim_id: claim.claim_id,
                expected_work_revision: item.revision,
                holder: claim.holder.clone(),
                claim_fence: claim.fence,
                waiver_reason: Some("No contribution".into()),
                actor: actor(&claim.holder.0),
                reason: "Apply returned completion".into(),
                idempotency_key: format!("release-{}-{second}", id.0),
                released_at: at(second),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
}

fn tree(tasks: Vec<TaskDocument>) -> TaskTree {
    let mut tree = fixture().tree;
    tree.tasks = tasks;
    tree.tasks.sort_by_key(|task| task.id.0);
    tree
}

#[test]
fn parent_seals_keep_reported_child_history_after_reopen_and_another_report() {
    use crate::storage::work::test_support::{checkpoint, complete, evidence};

    let mut store = SqliteStore::open_in_memory().unwrap();
    let parent = store
        .create_work(
            &root_request(PROJECT, "native parent", 0),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let children = store
        .decompose_work(
            &crate::domain::DecomposeWorkRequest {
                parent_id: parent.work_id,
                expected_parent_revision: parent.revision,
                children: vec![crate::storage::work::test_support::child(
                    "child",
                    ChildRequirement::Required,
                    "native required child",
                )],
                prerequisites: Vec::new(),
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner"),
                idempotency_key: "decompose-reported-child".into(),
                created_at: at(1),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let child_id = children.children[0].work_id;
    let incoming = report(&exported(&store), &[child_id]);
    exchange(&mut store, &incoming, "first-child-report", 4).unwrap();
    let parent = store.get_work_item(parent.work_id).unwrap();
    let claim = claim_restored(&mut store, parent.work_id, "executor", 5);
    let proof = evidence(&mut store, &parent, &claim, "executor", "proof", 6);
    checkpoint(
        &mut store,
        &parent,
        &claim,
        "executor",
        "checkpoint-parent",
        7,
        std::slice::from_ref(&proof),
    );
    let seal = complete(
        &mut store,
        &parent,
        &claim,
        "executor",
        &proof,
        "seal-parent",
        8,
    )
    .unwrap();
    assert!(seal.restored);
    assert_eq!(seal.restored_child_completions.len(), 1);
    let record_id = seal.restored_child_completions[0].clone();
    let seal_id = query::load_work_run(&store.connection, seal.run_id)
        .unwrap()
        .completion_seal
        .unwrap();
    let health = store.verify_all().unwrap();
    assert!(health.is_healthy(), "before reopen: {health:?}");

    for (id, second) in [(parent.work_id, 9), (child_id, 10)] {
        let item = store.get_work_item(id).unwrap();
        store
            .reopen_work(
                &crate::ReopenWorkRequest {
                    work_id: id,
                    expected_work_revision: item.revision,
                    reason: "Another iteration".into(),
                    actor: actor("planner"),
                    idempotency_key: format!("reopen-{second}"),
                    reopened_at: at(second),
                },
                &crate::DevelopmentNoopRedactor,
            )
            .unwrap();
    }
    let health = store.verify_all().unwrap();
    assert!(health.is_healthy(), "after child reopen: {health:?}");

    let incoming = report(&exported(&store), &[child_id]);
    exchange(&mut store, &incoming, "second-child-report", 11).unwrap();
    assert_ne!(
        query::latest_restored_record_hash(&store.connection, child_id).unwrap(),
        Some(record_id)
    );
    let health = store.verify_all().unwrap();
    assert!(health.is_healthy(), "after second child report: {health:?}");
    let historical: crate::CompletionSeal = store.get(&seal_id).unwrap().unwrap();
    assert_eq!(historical, seal);
}

#[test]
fn restored_child_citations_require_the_exact_projection_and_parent() {
    use crate::storage::work::test_support::{checkpoint, complete, evidence};

    let (mut store, fixture) = enrolled();
    let incoming = report(&exported(&store), &[fixture.first, fixture.second]);
    exchange(&mut store, &incoming, "report-required-children", 6).unwrap();
    let claim = claim_restored(&mut store, fixture.parent, "executor", 7);
    let parent = store.get_work_item(fixture.parent).unwrap();
    let proof = evidence(&mut store, &parent, &claim, "executor", "proof", 8);
    checkpoint(
        &mut store,
        &parent,
        &claim,
        "executor",
        "checkpoint-parent",
        9,
        std::slice::from_ref(&proof),
    );
    let seal = complete(
        &mut store,
        &parent,
        &claim,
        "executor",
        &proof,
        "seal-parent",
        10,
    )
    .unwrap();
    assert_historical_child_citation_is_bound(&mut store, &seal);
}

fn assert_historical_child_citation_is_bound(
    store: &mut SqliteStore,
    seal: &crate::CompletionSeal,
) {
    use crate::storage::work::completion::validate_completion_seal_children_on;

    let record_id = &seal.restored_child_completions[0];
    let mut unrelated = seal.clone();
    unrelated.work_id = WorkId(uuid::Uuid::new_v4());
    assert!(validate_completion_seal_children_on(&store.connection, &unrelated, 0).is_err());
    for damage in [
        "DELETE FROM work_restored_records WHERE record_id = ?1",
        "UPDATE work_restored_records SET generation_index = generation_index + 1000
         WHERE record_id = ?1",
    ] {
        let transaction = store.connection.transaction().unwrap();
        assert_eq!(
            transaction.execute(damage, [record_id.as_str()]).unwrap(),
            1
        );
        assert!(validate_completion_seal_children_on(&transaction, seal, 0).is_err());
        transaction.rollback().unwrap();
    }
    let health = store.verify_all().unwrap();
    assert!(
        health.is_healthy(),
        "after restoring citation bindings: {health:?}"
    );
}

#[test]
fn finished_children_can_arrive_after_their_ancestors_reported_completion() {
    let root = document("reported ancestor", None);
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &tree(vec![root.clone()]), "initial-ancestor", 5).unwrap();
    let incoming = report(&exported(&store), &[root.id]);
    exchange(&mut store, &incoming, "report-ancestor", 6).unwrap();
    assert!(matches!(
        events(&store, root.id).last().unwrap().transition,
        WorkTransition::CompletionReported { .. }
    ));

    let child = document("later finished child", Some(root.id));
    let mut incoming = exported(&store);
    incoming.tasks.push(child.clone());
    incoming.tasks.sort_by_key(|task| task.id.0);
    let incoming = report(&incoming, &[child.id]);
    let result = exchange(&mut store, &incoming, "later-finished-child", 7).unwrap();
    assert_eq!(result.tasks.len(), 1);
    assert_eq!(result.tasks[0].task, child.id);
    assert_eq!(result.tasks[0].status, TaskSyncTaskStatus::Applied);
    let item = store.get_work_item(child.id).unwrap();
    assert_eq!(item.lifecycle, WorkLifecycle::Completed);
    assert!(item.active_run_id.is_none());
    assert_eq!(events(&store, child.id), []);
    let health = store.verify_all().unwrap();
    assert!(health.is_healthy(), "{health:?}");
}

#[test]
fn a_shared_prerequisite_added_before_completion_imports_together_or_later() {
    for completion_first in [false, true] {
        let owner = document("owner", None);
        let target = document("completed prerequisite", None);
        let mut store = SqliteStore::open_in_memory().unwrap();
        exchange(
            &mut store,
            &tree(vec![owner.clone(), target.clone()]),
            "initial-edge-history",
            5,
        )
        .unwrap();
        let completed = report(&exported(&store), &[target.id]);
        if completion_first {
            exchange(&mut store, &completed, "completion-before-edge", 6).unwrap();
        }
        let incoming = edited(&completed, owner.id, |task| {
            task.prerequisites.push(target.id);
        });
        let result = exchange(&mut store, &incoming, "returned-edge-history", 7).unwrap();
        assert!(
            result
                .tasks
                .iter()
                .all(|task| task.status == TaskSyncTaskStatus::Applied)
        );
        assert_eq!(result.tasks.len(), if completion_first { 1 } else { 2 });
        assert_eq!(exported(&store), incoming);
        let history = events(&store, owner.id);
        assert!(
            matches!(history.last().unwrap().transition, WorkTransition::PrerequisiteAdded { prerequisite_id, .. } if prerequisite_id == target.id)
        );
        let repeated = exchange(&mut store, &incoming, "retry-returned-edge-history", 8).unwrap();
        assert_eq!(repeated.tasks, []);
        assert_eq!(events(&store, owner.id), history);
        assert!(store.verify_all().unwrap().is_healthy());
    }
}

#[test]
fn reported_child_completion_keeps_siblings_claimable_and_the_report_reopenable() {
    let (mut store, fixture) = enrolled();
    let claim = claim_restored(&mut store, fixture.first, "executor", 6);
    let execution = query::load_work_run(&store.connection, claim.run_id)
        .unwrap()
        .root_execution_id;
    release(&mut store, fixture.first, &claim, 7);
    let incoming = report(&exported(&store), &[fixture.first]);
    assert_eq!(
        exchange(&mut store, &incoming, "reported-child", 8)
            .unwrap()
            .tasks[0]
            .status,
        TaskSyncTaskStatus::Applied
    );
    assert_eq!(
        query::load_root_execution(&store.connection, execution)
            .unwrap()
            .state,
        crate::RootExecutionState::Active
    );
    let sibling = claim_restored(&mut store, fixture.second, "sibling", 9);
    assert_eq!(
        query::load_work_run(&store.connection, sibling.run_id)
            .unwrap()
            .root_execution_id,
        execution
    );
    release(&mut store, fixture.second, &sibling, 10);
    let item = store.get_work_item(fixture.first).unwrap();
    store
        .reopen_work(
            &crate::ReopenWorkRequest {
                work_id: item.work_id,
                expected_work_revision: item.revision,
                reason: "Another iteration".into(),
                actor: actor("planner"),
                idempotency_key: "reopen-reported-child".into(),
                reopened_at: at(11),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    // Reopen itself does not constitute live execution of the new run.
    let incoming = edited(&exported(&store), fixture.first, |task| {
        task.title = "Fresh dormant run".into();
    });
    let result = exchange(&mut store, &incoming, "edit-reopened-child", 12).unwrap();
    assert_eq!(result.tasks[0].status, TaskSyncTaskStatus::Applied);
    let health = store.verify_all().unwrap();
    assert!(health.is_healthy(), "{health:?}");
}

#[test]
fn a_reported_root_can_reopen_with_the_next_execution_generation() {
    let root = document("root", None);
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &tree(vec![root.clone()]), "initial-root", 5).unwrap();
    let claim = claim_restored(&mut store, root.id, "executor", 6);
    release(&mut store, root.id, &claim, 7);
    let incoming = report(&exported(&store), &[root.id]);
    exchange(&mut store, &incoming, "reported-root", 8).unwrap();
    let item = store.get_work_item(root.id).unwrap();
    let run = store
        .reopen_work(
            &crate::ReopenWorkRequest {
                work_id: item.work_id,
                expected_work_revision: item.revision,
                reason: "Another iteration".into(),
                actor: actor("planner"),
                idempotency_key: "reopen-reported-root".into(),
                reopened_at: at(9),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    assert_eq!(
        query::load_root_execution(&store.connection, run.root_execution_id)
            .unwrap()
            .generation,
        2
    );
    let health = store.verify_all().unwrap();
    assert!(health.is_healthy(), "{health:?}");
}

#[test]
fn reporting_an_ancestor_does_not_revoke_a_descendants_live_claim() {
    let (mut store, fixture) = enrolled();
    let claim = claim_restored(&mut store, fixture.first, "local-child", 6);
    let incoming = report(&exported(&store), &[fixture.parent, fixture.first]);
    let incoming = edited(&incoming, fixture.second, |task| {
        task.title = "Independent planning".into();
    });
    let result = exchange(&mut store, &incoming, "reports-over-live-child", 7).unwrap();
    for id in [fixture.parent, fixture.first] {
        let outcome = result.tasks.iter().find(|task| task.task == id).unwrap();
        assert_eq!(outcome.status, TaskSyncTaskStatus::Held);
        assert_eq!(outcome.reason.as_deref(), Some("active_run"));
        assert_eq!(
            store.get_work_item(id).unwrap().lifecycle,
            WorkLifecycle::Open
        );
    }
    assert_eq!(
        store.get_work_item(fixture.second).unwrap().title,
        "Independent planning"
    );
    release(&mut store, fixture.first, &claim, 8);
    let health = store.verify_all().unwrap();
    assert!(health.is_healthy(), "{health:?}");
}

#[test]
fn reporting_a_run_free_root_closes_its_child_execution_before_reopen() {
    let root = document("root without its own local run", None);
    let child = document("locally executed child", Some(root.id));
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(
        &mut store,
        &tree(vec![root.clone(), child.clone()]),
        "initial-child-execution",
        5,
    )
    .unwrap();
    let claim = claim_restored(&mut store, child.id, "child-executor", 6);
    let previous_execution = query::load_work_run(&store.connection, claim.run_id)
        .unwrap()
        .root_execution_id;
    release(&mut store, child.id, &claim, 7);
    assert_eq!(store.get_work_item(root.id).unwrap().active_run_id, None);
    let incoming = report(&exported(&store), &[root.id, child.id]);
    let result = exchange(&mut store, &incoming, "reported-run-free-root", 8).unwrap();
    assert_eq!(result.tasks.len(), 2);
    assert!(
        result
            .tasks
            .iter()
            .all(|task| task.status == TaskSyncTaskStatus::Applied)
    );
    assert_eq!(
        query::load_root_execution(&store.connection, previous_execution)
            .unwrap()
            .state,
        crate::RootExecutionState::Cancelled
    );
    let report_event = events(&store, root.id)
        .into_iter()
        .find(|event| matches!(event.transition, WorkTransition::CompletionReported { .. }))
        .unwrap();
    assert_eq!(report_event.run_id, None);
    let bound = report_event.root_execution.unwrap();
    assert_eq!(bound.root_execution_id, previous_execution);
    let event_execution =
        crate::storage::work::root_state::resolve(&store.connection, &bound).unwrap();
    assert_eq!(event_execution.state, crate::RootExecutionState::Cancelled);
    let health = store.verify_all().unwrap();
    assert!(health.is_healthy(), "{health:?}");
    let current = store.get_work_item(root.id).unwrap();
    let reopened = store
        .reopen_work(
            &crate::ReopenWorkRequest {
                work_id: root.id,
                expected_work_revision: current.revision,
                reason: "New root iteration".into(),
                actor: actor("planner"),
                idempotency_key: "reopen-after-child-only-execution".into(),
                reopened_at: at(9),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    assert_ne!(reopened.root_execution_id, previous_execution);
    assert_eq!(
        query::load_root_execution(&store.connection, reopened.root_execution_id)
            .unwrap()
            .generation,
        2
    );
    let health = store.verify_all().unwrap();
    assert!(health.is_healthy(), "{health:?}");
}

#[test]
fn reporting_a_root_with_an_optional_dormant_child_ends_its_execution_generation() {
    for released in [false, true] {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let root = store
            .create_work(
                &root_request(PROJECT, "optional-root", 0),
                &crate::DevelopmentNoopRedactor,
            )
            .unwrap();
        let decomposition = store
            .decompose_work(
                &crate::domain::DecomposeWorkRequest {
                    parent_id: root.work_id,
                    expected_parent_revision: root.revision,
                    children: vec![crate::storage::work::test_support::child(
                        "optional",
                        ChildRequirement::Optional,
                        "optional dormant child",
                    )],
                    prerequisites: Vec::new(),
                    authority: WorkPlanningAuthority::Project,
                    actor: actor("planner"),
                    idempotency_key: "optional-decomposition".into(),
                    created_at: at(1),
                },
                &crate::DevelopmentNoopRedactor,
            )
            .unwrap();
        let child = decomposition.children[0].work_id;
        let child_run = store.get_work_item(child).unwrap().active_run_id.unwrap();
        let previous_execution = query::load_work_run(&store.connection, child_run)
            .unwrap()
            .root_execution_id;
        if released {
            let claim = claim_restored(&mut store, child, "child-executor", 2);
            release(&mut store, child, &claim, 3);
        }
        let incoming = report(&exported(&store), &[root.work_id]);
        let result = exchange(&mut store, &incoming, "report-optional-root", 4).unwrap();
        assert_eq!(result.tasks.len(), 1);
        assert_eq!(result.tasks[0].status, TaskSyncTaskStatus::Applied);
        assert_eq!(
            query::load_root_execution(&store.connection, previous_execution)
                .unwrap()
                .state,
            crate::RootExecutionState::Cancelled
        );
        assert_eq!(
            query::load_work_run(&store.connection, child_run)
                .unwrap()
                .state,
            crate::WorkRunState::Open
        );
        let health = store.verify_all().unwrap();
        assert!(health.is_healthy(), "{health:?}");
        let child = store.get_work_item(child).unwrap();
        store
            .dispose_work(
                &crate::domain::DisposeWorkRequest {
                    work_id: child.work_id,
                    expected_work_revision: child.revision,
                    disposition: crate::WorkDisposition::Cancelled,
                    replacement_id: None,
                    reason: "Finish the old iteration".into(),
                    actor: actor("planner"),
                    idempotency_key: "dispose-optional-child".into(),
                    disposed_at: at(5),
                },
                &crate::DevelopmentNoopRedactor,
            )
            .unwrap();
        let root = store.get_work_item(root.work_id).unwrap();
        let reopened = store
            .reopen_work(
                &crate::ReopenWorkRequest {
                    work_id: root.work_id,
                    expected_work_revision: root.revision,
                    reason: "Next iteration".into(),
                    actor: actor("planner"),
                    idempotency_key: "reopen-optional-root".into(),
                    reopened_at: at(6),
                },
                &crate::DevelopmentNoopRedactor,
            )
            .unwrap();
        assert_ne!(reopened.root_execution_id, previous_execution);
        let next =
            query::load_root_execution(&store.connection, reopened.root_execution_id).unwrap();
        assert_eq!(next.generation, 2);
        assert_eq!(next.run_ids, [reopened.run_id]);
        assert_eq!(next.expected_contributors, []);
        assert_eq!(next.contributions, []);
        assert_eq!(next.required_child_seals, []);
        assert_eq!(next.required_child_waivers, []);
        assert_eq!(next.waivers, []);
        let health = store.verify_all().unwrap();
        assert!(health.is_healthy(), "{health:?}");
    }
}

#[test]
fn reporting_a_parent_refuses_while_a_required_child_would_remain_open() {
    let (mut store, fixture) = enrolled();
    let incoming = report(&exported(&store), &[fixture.parent, fixture.first]);
    let result = exchange(&mut store, &incoming, "incomplete-required-children", 6).unwrap();
    assert_eq!(
        result
            .tasks
            .iter()
            .find(|task| task.task == fixture.parent)
            .unwrap()
            .reason
            .as_deref(),
        Some("required_child_unfinished")
    );
    assert_eq!(
        store.get_work_item(fixture.parent).unwrap().lifecycle,
        WorkLifecycle::Open
    );
    assert_eq!(
        store.get_work_item(fixture.first).unwrap().lifecycle,
        WorkLifecycle::Completed
    );
    let health = store.verify_all().unwrap();
    assert!(health.is_healthy(), "{health:?}");
}

#[test]
fn proposed_prerequisite_changes_refuse_before_independent_writes() {
    for removal in [false, true] {
        let target = document("target", None);
        let mut owner = document("proposed owner", None);
        owner.lifecycle = WorkLifecycle::Proposed;
        if removal {
            owner.prerequisites.push(target.id);
        }
        let independent = document("independent", None);
        let initial = tree(vec![owner.clone(), target.clone(), independent.clone()]);
        let mut store = SqliteStore::open_in_memory().unwrap();
        exchange(&mut store, &initial, "initial-proposed", 5).unwrap();
        let incoming = edited(&initial, owner.id, |task| {
            if removal {
                task.prerequisites.clear();
            } else {
                task.prerequisites.push(target.id);
            }
        });
        let incoming = edited(&incoming, independent.id, |task| {
            task.title = "Applied beside proposed refusal".into();
        });
        let result = exchange(&mut store, &incoming, "proposed-relation", 6).unwrap();
        assert_eq!(
            result
                .tasks
                .iter()
                .find(|task| task.task == owner.id)
                .unwrap()
                .status,
            TaskSyncTaskStatus::Refused
        );
        assert_eq!(
            store.get_work_item(independent.id).unwrap().title,
            "Applied beside proposed refusal"
        );
        let health = store.verify_all().unwrap();
        assert!(health.is_healthy(), "{health:?}");
    }
}

#[test]
fn a_refused_edit_does_not_spread_through_an_existing_endpoint() {
    let x = document("existing endpoint", None);
    let mut z = document("existing dependent", None);
    z.prerequisites.push(x.id);
    let initial = tree(vec![x.clone(), z.clone()]);
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &initial, "initial-endpoints", 5).unwrap();
    let collision = store
        .create_work(
            &root_request("other-project", "collision", 6),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let mut n = document("refused new identity", None);
    n.id = collision.work_id;
    let mut incoming = edited(&initial, x.id, |task| {
        task.prerequisites.push(n.id);
    });
    incoming.tasks.push(n);
    incoming.tasks.sort_by_key(|task| task.id.0);
    let incoming = edited(&incoming, z.id, |task| {
        task.title = "Independent existing dependent edit".into();
    });
    let result = exchange(&mut store, &incoming, "do-not-spread-existing", 7).unwrap();
    assert_eq!(
        result
            .tasks
            .iter()
            .find(|task| task.task == x.id)
            .unwrap()
            .reason
            .as_deref(),
        Some("refused_reference")
    );
    assert_eq!(
        result
            .tasks
            .iter()
            .find(|task| task.task == z.id)
            .unwrap()
            .status,
        TaskSyncTaskStatus::Applied
    );
    assert_eq!(
        store.get_work_item(z.id).unwrap().title,
        "Independent existing dependent edit"
    );
    let health = store.verify_all().unwrap();
    assert!(health.is_healthy(), "{health:?}");
}

#[test]
fn a_refused_relation_removal_does_not_hide_a_cycle_through_retired_work() {
    let a = document("A", None);
    let y = document("Y", None);
    let mut b = document("B", None);
    b.prerequisites.push(y.id);
    let mut r = document("retired required child", Some(a.id));
    r.prerequisites.push(b.id);
    let independent = document("independent", None);
    let initial = tree(vec![
        a.clone(),
        b.clone(),
        r.clone(),
        y.clone(),
        independent.clone(),
    ]);
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &initial, "initial-durable-chain", 5).unwrap();
    let mut current = exported(&store);
    current.tasks.retain(|task| task.id != r.id);
    exchange(&mut store, &current, "retire-chain-child", 6).unwrap();
    let incoming = edited(&current, b.id, |task| {
        task.kind = WorkItemKind::Feature;
        task.prerequisites.clear();
    });
    let incoming = edited(&incoming, y.id, |task| {
        task.prerequisites.push(a.id);
    });
    let incoming = edited(&incoming, independent.id, |task| {
        task.title = "Applied beside durable cycle refusal".into();
    });
    let result = exchange(&mut store, &incoming, "mixed-durable-cycle", 7).unwrap();
    assert_eq!(
        result
            .tasks
            .iter()
            .find(|task| task.task == b.id)
            .unwrap()
            .reason
            .as_deref(),
        Some("unsupported_fields")
    );
    assert_eq!(
        result
            .tasks
            .iter()
            .find(|task| task.task == y.id)
            .unwrap()
            .reason
            .as_deref(),
        Some("durable_graph")
    );
    assert_eq!(
        store.get_work_item(independent.id).unwrap().title,
        "Applied beside durable cycle refusal"
    );
    let health = store.verify_all().unwrap();
    assert!(health.is_healthy(), "{health:?}");
}
