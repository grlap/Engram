use super::*;
use crate::task_sync::retention::successor::RetainedSuccessor;

fn prepared(shape: Shape) -> (Fixture, WorkId, RetainedSuccessor) {
    let (mut f, root, _, dependent) = restored_child_fixture_with(shape);
    let original = f.apply().unwrap();
    assert!(matches!(
        original.event.transition,
        Transition::PlanningDeferred { .. }
    ));
    let claim = f.store.claim_fixture(dependent);
    let item = f.store.get_work_item(dependent).unwrap();
    let proof = evidence(&mut f.store, &item, &claim, "runner", "control-proof", 41);
    checkpoint(
        &mut f.store,
        &item,
        &claim,
        "runner",
        "control-checkpoint",
        41,
        std::slice::from_ref(&proof),
    );
    let item = f.store.get_work_item(dependent).unwrap();
    complete(
        &mut f.store,
        &item,
        &claim,
        "runner",
        &proof,
        "control-completion",
        42,
    )
    .unwrap();
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
    let retained = f
        .store
        .retain_task_sync_planning_successor(
            &f.input.binding_id,
            &request,
            &payload,
            at(70),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    (f, root, retained)
}

#[test]
fn restored_successor_shares_one_root_and_reuses_an_existing_aggregate() {
    for shape in [Shape::Siblings, Shape::RootAndChild, Shape::ExistingRoot] {
        let (mut f, root, retained) = prepared(shape);
        let existing = active_root_execution_optional(&f.store.connection, root).unwrap();
        let expected = if matches!(shape, Shape::ExistingRoot) {
            assert!(retained.recipe.new_root_executions.is_empty());
            existing.as_ref().unwrap().root_execution_id
        } else {
            assert!(existing.is_none());
            assert_eq!(retained.recipe.new_root_executions.len(), 1);
            retained.recipe.new_root_executions[&root]
        };
        let before = state(&f.store);
        let read = retention::successor::read(
            &f.input.binding_id,
            &f.input.binding,
            &retained.recipe.request,
        )
        .unwrap()
        .unwrap();
        assert_eq!(read, retained);
        assert_eq!(state(&f.store), before);
        f.store
            .apply_task_sync_planning_successor(&f.input.binding, &read, &DevelopmentNoopRedactor)
            .unwrap();
        let mut expected_runs = existing.map_or_else(Vec::new, |execution| execution.run_ids);
        for (&work, &run) in &retained.recipe.new_runs {
            let item = f.store.get_work_item(work).unwrap();
            assert_eq!(item.active_run_id, Some(run), "{shape:?}");
            let run = load_work_run(&f.store.connection, run).unwrap();
            assert_eq!(run.root_execution_id, expected, "{shape:?}");
            assert!(run.executor.is_none());
            expected_runs.push(run.run_id);
        }
        expected_runs.sort_by_key(|run| run.0);
        let execution = load_root_execution(&f.store.connection, expected).unwrap();
        assert_eq!(execution.root_id, root);
        assert_eq!(execution.run_ids, expected_runs);
        let integrity = f.store.verify_all().unwrap();
        assert!(integrity.is_healthy(), "{shape:?}: {integrity:#?}");
    }
}

#[test]
fn restored_successor_refuses_inexact_root_reservations_without_writes() {
    let (mut f, root, retained) = prepared(Shape::Single);
    let before = state(&f.store);
    for fault in ["missing", "wrong_root", "extra", "overlapping_id"] {
        let mut recipe = retained.recipe.clone();
        let child = *recipe.new_runs.keys().next().unwrap();
        let identity = recipe.new_root_executions[&root];
        match fault {
            "missing" => {
                recipe.new_root_executions.remove(&root);
            }
            "wrong_root" => {
                recipe.new_root_executions.remove(&root);
                recipe.new_root_executions.insert(child, identity);
            }
            "extra" => {
                recipe
                    .new_root_executions
                    .insert(child, crate::RootExecutionId::new());
            }
            "overlapping_id" => {
                recipe
                    .new_root_executions
                    .insert(root, crate::RootExecutionId(recipe.new_runs[&child].0));
            }
            _ => unreachable!(),
        }
        {
            let tx = f.store.begin_work_mutation().unwrap();
            let error = crate::storage::work::task_sync::ledger::delta::successor::admit_on(
                &tx,
                &f.input.binding_id,
                &recipe,
            )
            .err()
            .expect(fault);
            assert!(
                error.to_string().contains(if fault == "overlapping_id" {
                    "repeats an identity"
                } else {
                    "root selection differs"
                }),
                "{fault}: {error}"
            );
        }
        assert_eq!(state(&f.store), before, "{fault}");
        assert!(
            active_root_execution_optional(&f.store.connection, root)
                .unwrap()
                .is_none()
        );
    }
    assert_eq!(
        retention::successor::read(
            &f.input.binding_id,
            &f.input.binding,
            &retained.recipe.request,
        )
        .unwrap()
        .unwrap(),
        retained
    );
}

#[test]
fn restored_successor_rolls_back_new_aggregate_and_retries_the_retained_ids() {
    let (mut f, root, retained) = prepared(Shape::Single);
    let before = state(&f.store);
    f.store
        .connection
        .execute_batch(
            "CREATE TEMP TRIGGER restored_successor_failure
        BEFORE INSERT ON objects WHEN NEW.object_kind='task_sync_import_planning'
        BEGIN SELECT RAISE(ABORT,'injected restored successor failure'); END;",
        )
        .unwrap();
    let error = f
        .store
        .apply_task_sync_planning_successor(&f.input.binding, &retained, &DevelopmentNoopRedactor)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected restored successor failure"),
        "{error}"
    );
    assert_eq!(state(&f.store), before);
    assert!(
        active_root_execution_optional(&f.store.connection, root)
            .unwrap()
            .is_none()
    );
    for &work in retained.recipe.new_runs.keys() {
        assert_eq!(f.store.get_work_item(work).unwrap().active_run_id, None);
    }
    f.store
        .connection
        .execute_batch("DROP TRIGGER restored_successor_failure")
        .unwrap();
    let retry = retention::successor::read(
        &f.input.binding_id,
        &f.input.binding,
        &retained.recipe.request,
    )
    .unwrap()
    .unwrap();
    assert_eq!(retry, retained);
    f.store
        .apply_task_sync_planning_successor(&f.input.binding, &retry, &DevelopmentNoopRedactor)
        .unwrap();
    let execution = load_root_execution(
        &f.store.connection,
        retained.recipe.new_root_executions[&root],
    )
    .unwrap();
    assert_eq!(
        execution.run_ids,
        retained
            .recipe
            .new_runs
            .values()
            .copied()
            .collect::<Vec<_>>()
    );
    assert!(f.store.verify_all().unwrap().is_healthy());
}
