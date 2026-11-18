//! The real service opens lazily, then acquires its writer in the same request.

use super::*;
use crate::storage::control_request_test_clock as test_clock;
use crate::storage::{last_writer_admission_allowance, with_writer_admission_test_policy};
use std::time::Duration;

#[test]
fn an_evaluation_keeps_its_budget_through_opening_and_replays_the_same_attempt() {
    let case = scenario("composed-budget");
    let evaluator = case.executor_agent();
    let input = WorkEvaluateInput {
        mode: "sub_agent".into(),
        execution_identity: Some("executor-agent".into()),
        parent_session: Some(case.service.session_id.0.clone()),
        attempt: Some("composed-budget-evaluation".into()),
        ..evaluate_input(
            &case.root.short_ref,
            case.revision(),
            completion_run_feed_head(&case.service, case.root.work_id),
            vec![verdict(1, "pass", "judgment", &case.citation)],
        )
    };
    let _request = test_clock::scope();
    crate::storage::before_next_open_lock(|| test_clock::advance(Duration::from_secs(40)));
    let mut first = true;
    let recorded = with_writer_admission_test_policy(
        None,
        move || {
            if first {
                first = false;
                test_clock::advance(Duration::from_secs(15));
            }
        },
        || {
            let recorded = evaluator.work_evaluate_on(&input, at(5)).unwrap();
            assert_eq!(
                last_writer_admission_allowance(),
                Some(Duration::from_secs(5))
            );
            recorded
        },
    );
    assert!(!recorded.replayed);
    test_clock::advance(Duration::from_secs(5));
    assert!(matches!(
        evaluator.work_evaluate_on(&input, at(6)),
        Err(StoreError::ControlRequestExpired)
    ));
    // Expiry of a replay says nothing about whether the earlier attempt wrote.
    // The fresh exact retry reads and returns that original immutable record.
    let _retry = test_clock::scope();
    let replay = evaluator.work_evaluate_on(&input, at(7)).unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.evaluation, recorded.evaluation);
    let history = case
        .store()
        .acceptance_evaluation_entries(case.root.work_id)
        .unwrap()
        .unwrap();
    assert_eq!(history.entries.len(), 1);
    assert_eq!(history.entries[0].evaluation, recorded.evaluation);
}
