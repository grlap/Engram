//! A real retained successor may first execute a restored child below a runless root.

use super::*;
use crate::storage::work::{
    query::{active_root_execution_optional, load_root_execution, load_work_run},
    test_support::{checkpoint, complete, evidence},
};
use crate::{WorkId, WorkLifecycle};
use std::collections::BTreeSet;

mod controls;

#[derive(Clone, Copy, Debug)]
enum Shape {
    Single,
    Siblings,
    RootAndChild,
    ExistingRoot,
}

fn restored_child_fixture() -> (Fixture, WorkId, WorkId, WorkId) {
    restored_child_fixture_with(Shape::Single)
}

fn restored_child_fixture_with(shape: Shape) -> (Fixture, WorkId, WorkId, WorkId) {
    let mut tree = candidate();
    let mut root = tree
        .tasks
        .iter()
        .find(|task| task.lifecycle == WorkLifecycle::Open && task.parent_id.is_none())
        .unwrap()
        .clone();
    let mut child = tree
        .tasks
        .iter()
        .find(|task| task.lifecycle == WorkLifecycle::Completed)
        .unwrap()
        .clone();
    child.parent_id = Some(root.id);
    for task in [&mut root, &mut child] {
        task.external_ref = None;
        task.assigned_to = None;
        task.deferred_until = None;
        task.prerequisites.clear();
        task.blockers.clear();
        task.acceptance_bindings.clear();
        task.evaluation_mode = None;
        task.notes.clear();
        task.activity.clear();
    }
    let root_id = root.id;
    let child_id = child.id;
    // Restore the historical dependency as authored. Ordinary authoring
    // correctly refuses adding a new edge to already completed work.
    let mut dependent = root.clone();
    dependent.id = WorkId::new();
    dependent.title = "Separate dependent with local execution".into();
    dependent.prerequisites = vec![child_id];
    let dependent_id = dependent.id;
    let mut reopened = BTreeSet::from([child_id]);
    let mut siblings = Vec::new();
    if matches!(shape, Shape::Siblings) {
        let mut sibling = child.clone();
        sibling.id = WorkId::new();
        reopened.insert(sibling.id);
        siblings.push(sibling);
    }
    if matches!(shape, Shape::RootAndChild) {
        root.lifecycle = WorkLifecycle::Completed;
        root.completion = child.completion.clone();
        reopened.insert(root_id);
    }
    if matches!(shape, Shape::ExistingRoot) {
        dependent.parent_id = Some(root_id);
    }
    tree.tasks = vec![root, child, dependent];
    tree.tasks.extend(siblings);
    tree.tasks.sort_by_key(|task| task.id);
    let fixture = Fixture::with_setup(
        &tree,
        |tree| {
            tree.tasks
                .iter()
                .filter(|task| reopened.contains(&task.id))
                .cloned()
                .map(|mut task| {
                    task.lifecycle = WorkLifecycle::Open;
                    task.completion = None;
                    (task.id, Some(task))
                })
                .collect()
        },
        |store, selected| {
            assert!(reopened.contains(&selected));
            let item = store.get_work_item(dependent_id).unwrap();
            assert!(item.restored);
            assert_eq!(item.active_run_id, None);
            store
                .claim_work(
                    &crate::ClaimWorkRequest {
                        work_id: item.work_id,
                        expected_work_revision: item.revision,
                        expected_run_id: item.active_run_id,
                        holder: crate::SessionId("runner".into()),
                        ttl_seconds: 600,
                        recovery_reason: None,
                        actor: actor("runner"),
                        idempotency_key: "restored-successor-dependent-holder".into(),
                        claimed_at: at(33),
                    },
                    &DevelopmentNoopRedactor,
                )
                .unwrap();
        },
    );
    (fixture, root_id, child_id, dependent_id)
}

#[test]
fn retained_successor_reopens_restored_child_with_no_root_execution() {
    let (mut f, root, child, dependent) = restored_child_fixture();
    for id in [root, child] {
        let item = f.store.get_work_item(id).unwrap();
        assert!(item.restored);
        assert_eq!(item.active_run_id, None);
    }
    assert!(
        active_root_execution_optional(&f.store.connection, root)
            .unwrap()
            .is_none()
    );
    let original = f.apply().unwrap();
    let Transition::PlanningDeferred {
        hold,
        blocking_claims,
        ..
    } = &original.event.transition
    else {
        panic!("the native dependent must defer the whole scope")
    };
    assert_eq!(blocking_claims.len(), 1);
    let held: crate::task_sync::reconciliation::ReconciliationHold =
        SqliteStore::get_typed_object_on(
            &f.store.connection,
            hold,
            crate::task_sync::reconciliation::HOLD_KIND,
        )
        .unwrap()
        .unwrap();
    assert_eq!(
        held.affected
            .iter()
            .map(|basis| basis.work_id)
            .collect::<BTreeSet<_>>(),
        BTreeSet::from([root, child, dependent]),
    );
    assert!(
        held.affected
            .iter()
            .filter(|basis| basis.work_id != dependent)
            .all(|basis| basis.runs.is_empty())
    );
    let claim = f.store.claim_fixture(dependent);
    let item = f.store.get_work_item(dependent).unwrap();
    let proof = evidence(
        &mut f.store,
        &item,
        &claim,
        "runner",
        "restored-dependent-proof",
        41,
    );
    checkpoint(
        &mut f.store,
        &item,
        &claim,
        "runner",
        "restored-dependent-checkpoint",
        41,
        std::slice::from_ref(&proof),
    );
    let item = f.store.get_work_item(dependent).unwrap();
    let seal = complete(
        &mut f.store,
        &item,
        &claim,
        "runner",
        &proof,
        "restored-dependent-completion",
        42,
    )
    .unwrap();
    let old_run = load_work_run(&f.store.connection, claim.run_id).unwrap();
    assert_eq!(old_run.state, crate::WorkRunState::Completed);
    assert!(
        active_root_execution_optional(&f.store.connection, root)
            .unwrap()
            .is_none()
    );

    let mut request = successor_request(&f, &original);
    request.intent.candidate.directory = retention::delta::candidate_directory(
        &f.input.binding.repository.git_directory,
        request.intent.operation,
    );
    let mut payload = f.input.payload.clone();
    payload.operation = request.intent.operation;
    payload.output.inventory_root = InventoryRoot::parse(
        &crate::storage::work::task_sync::baseline::next_root(
            &f.store.connection,
            &f.input.binding.project,
            &payload.documents,
        )
        .unwrap(),
    )
    .unwrap();
    request.intent.candidate.output = payload.output.clone();
    request.intent.candidate.content_fingerprint =
        CanonicalObject::freeze(&payload).unwrap().key().clone();
    let before = state(&f.store);
    let inspected = f
        .store
        .inspect_task_sync_planning_successor(&f.input.binding_id, &request)
        .unwrap();
    assert_eq!(state(&f.store), before);
    assert!(
        inspected
            .iter()
            .filter(|basis| basis.work_id != dependent)
            .all(|basis| basis.runs.is_empty())
    );
    let retained = f
        .store
        .retain_task_sync_planning_successor(
            &f.input.binding_id,
            &request,
            &payload,
            at(70),
            &DevelopmentNoopRedactor,
        )
        .expect("the real retained successor must be admitted after normal completion");
    let selected_run = retained.recipe.new_runs[&child];
    let applied = f
        .store
        .apply_task_sync_planning_successor(&f.input.binding, &retained, &DevelopmentNoopRedactor)
        .unwrap_or_else(|error| {
            panic!("retained successor must reopen the runless restored child: {error}")
        });
    let reopened = f.store.get_work_item(child).unwrap();
    assert_eq!(reopened.lifecycle, WorkLifecycle::Open);
    assert_eq!(reopened.active_run_id, Some(selected_run));
    let run = load_work_run(&f.store.connection, selected_run).unwrap();
    assert_eq!(
        run.root_execution_id,
        retained.recipe.new_root_executions[&root]
    );
    let execution = load_root_execution(&f.store.connection, run.root_execution_id).unwrap();
    assert_eq!(execution.root_id, root);
    assert_eq!(execution.run_ids, vec![selected_run]);
    assert_eq!(f.store.get_work_item(root).unwrap().active_run_id, None);
    assert_eq!(
        load_work_run(&f.store.connection, claim.run_id).unwrap(),
        old_run
    );
    let preserved: crate::CompletionSeal = SqliteStore::get_typed_object_on(
        &f.store.connection,
        old_run.completion_seal.as_ref().unwrap(),
        "completion_seal",
    )
    .unwrap()
    .unwrap();
    assert_eq!(preserved, seal);
    let unchanged = state(&f.store);
    assert_eq!(
        f.store
            .apply_task_sync_planning_successor(
                &f.input.binding,
                &retained,
                &DevelopmentNoopRedactor
            )
            .unwrap(),
        applied
    );
    assert_eq!(f.apply().unwrap(), original);
    assert_eq!(state(&f.store), unchanged);
    let integrity = f.store.verify_all().unwrap();
    assert!(integrity.is_healthy(), "{integrity:#?}");
}
