use super::*;
use crate::storage::control_runtime::request_budget::test_clock;

#[test]
fn control_request_expiry_rolls_back_observation_and_keeps_prior_replay_uncertain() {
    let mut fixture = fixture();
    let input = inter_turn_change(&fixture, "delayed-change");
    let before = crate::storage::test_database_shape_snapshot(&fixture.store.connection).unwrap();
    let _scope = test_clock::scope();
    test_clock::expire_at_operation_write(&fixture.store);
    let error = observe(&mut fixture, input.clone(), 7).unwrap_err();
    assert!(matches!(error, StoreError::ControlRequestExpired));
    assert!(fixture.store.connection.is_autocommit());
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&fixture.store.connection).unwrap(),
        before
    );
    test_clock::clear_hook(&fixture.store);
    let _fresh = test_clock::scope();
    let committed = observe(&mut fixture, input.clone(), 7).unwrap();
    let after = crate::storage::test_database_shape_snapshot(&fixture.store.connection).unwrap();
    assert_ne!(after, before);

    // A refusal on a later replay certifies only that replay's invocation.
    test_clock::expire();
    assert!(matches!(
        observe(&mut fixture, input.clone(), 8),
        Err(StoreError::ControlRequestExpired)
    ));
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&fixture.store.connection).unwrap(),
        after
    );
    let _retry = test_clock::scope();
    let replay = observe(&mut fixture, input, 9).unwrap();
    assert_eq!(
        serde_json::to_value(replay).unwrap(),
        serde_json::to_value(committed).unwrap()
    );
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&fixture.store.connection).unwrap(),
        after
    );
}
