use super::*;

/// Every exchange writer holds the origin reservation for its whole effect. A
/// fresh successor construction beside one in flight refuses with nothing
/// retained, rather than binding a chain position that writer then replaces;
/// once the reservation is free it binds the current tail and applies.
#[test]
fn successor_construction_refuses_beside_an_in_flight_writer_and_retains_nothing() {
    let (mut f, original, _) = ended_fixture();
    let (request, payload) = resolved(&f, &original);
    let database = std::path::PathBuf::from(f.store.connection.path().unwrap());
    let in_flight = crate::task_sync::origin::continuity::PreparedContinuity::acquire(
        &database,
        &f.input.binding.project,
    )
    .unwrap();
    let files = retention::inventory(&f.input.binding.repository.git_directory).unwrap();
    let before = state(&f.store);
    let error = f
        .store
        .retain_task_sync_planning_successor(
            &f.input.binding_id,
            &request,
            &payload,
            at(70),
            &DevelopmentNoopRedactor,
        )
        .unwrap_err();
    assert!(error.to_string().contains("busy"), "{error}");
    assert_eq!(
        retention::inventory(&f.input.binding.repository.git_directory).unwrap(),
        files
    );
    assert!(
        retention::successor::read(&f.input.binding_id, &f.input.binding, &request)
            .unwrap()
            .is_none()
    );
    assert_eq!(state(&f.store), before);
    drop(in_flight);
    let applied = apply_planning_successor(
        &mut f.store,
        &f.input.binding_id,
        &f.input.binding,
        &request,
        &payload,
        at(71),
        &DevelopmentNoopRedactor,
    )
    .unwrap();
    let retained = retention::successor::read(&f.input.binding_id, &f.input.binding, &request)
        .unwrap()
        .unwrap();
    assert_eq!(applied.prepared, retained.recipe.prepared);
    crate::storage::work::task_sync::ledger::operations::checkpoint_tests::audit_healthy(
        &mut f.store,
        &f.input.binding,
        &f.input.binding_id,
    );
}

#[test]
fn retained_successor_survives_unrelated_feed_movement_with_original_ids() {
    let (mut f, original, old_run) = ended_fixture();
    let (request, payload) = resolved(&f, &original);
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
    crate::storage::work::task_sync::ledger::operations::checkpoint_tests::unrelated(
        &mut f.store,
        &f.input.binding.project,
    );
    let applied = apply_planning_successor(
        &mut f.store,
        &f.input.binding_id,
        &f.input.binding,
        &request,
        &payload,
        at(90),
        &DevelopmentNoopRedactor,
    )
    .unwrap();
    assert_eq!(applied.prepared, retained.recipe.prepared);
    assert_eq!(applied.outcome, retained.recipe.outcome);
    assert_eq!(
        retention::successor::read(&f.input.binding_id, &f.input.binding, &request).unwrap(),
        Some(retained)
    );
    assert_eq!(
        crate::storage::work::query::load_work_run(&f.store.connection, old_run.run_id).unwrap(),
        old_run
    );
    assert_eq!(f.apply().unwrap(), original);
    crate::storage::work::task_sync::ledger::operations::checkpoint_tests::audit_healthy(
        &mut f.store,
        &f.input.binding,
        &f.input.binding_id,
    );
}
