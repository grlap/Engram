use super::*;
use crate::storage::control_runtime::request_budget::test_clock;

#[test]
fn control_request_expiry_rolls_back_binding_and_keeps_prior_replay_uncertain() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let a = held(&mut store, "item-a", HOLDER, 9, "R1", 2);
    let b = held(&mut store, "item-b", HOLDER, 10, "R1", 4);
    let original = native_check(&mut store, &a, "R1", 20);
    let input = request(&original, "R1", vec![target(&store, &b, "R1", vec![1])]);
    let host = bind_control_for(
        &mut store,
        HOLDER,
        "budget-host",
        &[EffectClass::Observe],
        at(25),
    );
    let before = crate::storage::test_database_shape_snapshot(&store.connection).unwrap();
    let _scope = test_clock::scope();
    test_clock::expire_at_operation_write(&store);
    let error = bind_through_host(&mut store, &host, &input, 100).unwrap_err();
    assert!(matches!(error, StoreError::ControlRequestExpired));
    assert!(store.connection.is_autocommit());
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&store.connection).unwrap(),
        before
    );
    test_clock::clear_hook(&store);
    let _fresh = test_clock::scope();
    let committed = bind_through_host(&mut store, &host, &input, 100).unwrap();
    assert!(!committed.replayed);
    let after = crate::storage::test_database_shape_snapshot(&store.connection).unwrap();
    assert_ne!(after, before);

    test_clock::expire();
    assert!(matches!(
        bind_through_host(&mut store, &host, &input, 101),
        Err(StoreError::ControlRequestExpired)
    ));
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&store.connection).unwrap(),
        after
    );
    let _retry = test_clock::scope();
    let replay = bind_through_host(&mut store, &host, &input, 102).unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.original, committed.original);
    assert_eq!(replay.bound, committed.bound);
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&store.connection).unwrap(),
        after
    );
}
