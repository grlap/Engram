//! A carried failure survives the executor's ordinary next step: a host check
//! after the revision retires the failing record for another reason too, but
//! the failure it recorded is still carried to the next evaluation.

use super::*;

// Every revision of an item with an active run lands on that run's feed, so
// criteria that no revision there explains mean a damaged projection. Reading
// nothing carried would silently lift the requirement to name the failure.
#[test]
fn criteria_no_revision_explains_are_a_damaged_projection() {
    let mut fixture = fixture("project-carried-damaged");
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable-damaged",
        5,
    );
    disable_obligation_rules(store, 6);
    let work = fixture.work.clone();
    let claim = fixture.claim.clone();
    record(
        store,
        &request(
            &work,
            cut(store, &work),
            "runner",
            Mode::SameSession,
            vec![verdict(
                1,
                AcceptanceVerdict::Fail,
                AcceptanceBasis::Judgment,
                &[],
            )],
            15,
        ),
    )
    .expect("failing evaluation");
    revise(
        store,
        &work,
        &claim,
        WorkRevisionPatch {
            acceptance: Some(vec!["An easier criterion".into()]),
            ..empty_patch()
        },
        "reword-after-failure",
        16,
    )
    .expect("the holder rewords the failed criterion");
    // The revision's event stays the item's latest, but its entry on the run
    // feed is lost.
    let removed = store
        .connection
        .execute(
            "DELETE FROM work_feed_entries
             WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND object_kind = 'work_event'
               AND position = (
                   SELECT MAX(position) FROM work_feed_entries
                   WHERE feed_kind = 'run_execution' AND feed_id = ?1
                     AND object_kind = 'work_event')",
            rusqlite::params![claim.run_id.0.to_string()],
        )
        .expect("lose the revision's run-feed entry");
    assert_eq!(removed, 1);
    let status = store.acceptance_evaluation_status(work.work_id, None);
    assert!(
        matches!(
            &status,
            Err(StoreError::InvalidWorkProjection(message))
                if message.contains("do not lead to the item's current criteria")
        ),
        "{status:?}"
    );
}

#[test]
fn a_host_check_after_the_revision_does_not_end_the_carry() {
    let mut fixture = fixture("project-carried-failure");
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession, Mode::IndependentSession],
        MechanicalBasis::Asserted,
        false,
        "enable-carried",
        5,
    );
    disable_obligation_rules(store, 6);
    let work = fixture.work.clone();
    let claim = fixture.claim.clone();
    let note = fixture.evidence.clone();
    let failed = record(
        store,
        &request(
            &work,
            cut(store, &work),
            // A failing independent judgment by a session that never held the run.
            "judge",
            Mode::IndependentSession,
            vec![verdict(
                1,
                AcceptanceVerdict::Fail,
                AcceptanceBasis::Judgment,
                &[],
            )],
            15,
        ),
    )
    .expect("failing evaluation");
    let revised = revise(
        store,
        &work,
        &claim,
        WorkRevisionPatch {
            acceptance: Some(vec!["An easier criterion".into()]),
            ..empty_patch()
        },
        "reword-after-failure",
        16,
    )
    .expect("the holder rewords the failed criterion");
    // The executor's next step: a host-observed test after the rewording,
    // which also retires the failing record as overtaken.
    host_verification(
        store,
        &revised,
        &claim,
        "runner",
        "test-after-rewording",
        VerificationKind::Test,
        VerificationResult::Passed,
        20,
    );
    let status = store
        .acceptance_evaluation_status(work.work_id, None)
        .expect("status")
        .expect("newest evaluation");
    assert_eq!(status.evaluation, failed.evaluation);
    let carried = status
        .carried_failure
        .expect("the failure is still carried after the check");
    assert_eq!(carried.evaluation, failed.evaluation);
    assert_eq!(
        carried.revised_by,
        crate::domain::CarriedFailureReviser::Executor
    );

    let current = cut(store, &revised);
    let pass = |session: &str, mode: Mode, supersedes: Option<ObjectId>, second: i64| {
        RecordAcceptanceEvaluationRequest {
            supersedes,
            ..request(
                &revised,
                current,
                session,
                mode,
                vec![verdict(
                    1,
                    AcceptanceVerdict::Pass,
                    AcceptanceBasis::Judgment,
                    std::slice::from_ref(&note),
                )],
                second,
            )
        }
    };
    let unacknowledged = pass("runner", Mode::SameSession, None, 25);
    assert!(
        matches!(
            record(store, &unacknowledged),
            Err(StoreError::AcceptanceEvaluationCarriedFailure {
                refusal: crate::CarriedFailureRefusal::Unacknowledged,
                ..
            })
        ),
        "a pass that ignores the failure is refused"
    );
    let self_named = pass(
        "runner",
        Mode::SameSession,
        Some(failed.evaluation.clone()),
        26,
    );
    assert!(
        matches!(
            record(store, &self_named),
            Err(StoreError::AcceptanceEvaluationCarriedFailure {
                refusal: crate::CarriedFailureRefusal::SelfAcknowledged,
                ..
            })
        ),
        "the executor may not acknowledge its own failure"
    );
    let acknowledged = pass(
        "reviewer",
        Mode::IndependentSession,
        Some(failed.evaluation.clone()),
        27,
    );
    let recorded = record(store, &acknowledged).expect("the reviewer's acknowledged pass records");
    assert_eq!(recorded.record.supersedes, Some(failed.evaluation));
}

// Criteria keep the order typed, so a revision that only reorders them
// changes the criteria an evaluation judged: a failure recorded before it is
// carried, and a pass after an executor's reorder must name it.
#[test]
fn a_reorder_after_a_failed_evaluation_carries_the_failure() {
    let mut fixture = fixture("project-carried-reorder");
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession, Mode::IndependentSession],
        MechanicalBasis::Asserted,
        false,
        "enable-carried-reorder",
        5,
    );
    disable_obligation_rules(store, 6);
    let claim = fixture.claim.clone();
    let note = fixture.evidence.clone();
    let typed = revise(
        store,
        &fixture.work,
        &claim,
        WorkRevisionPatch {
            acceptance: Some(vec!["Zeta holds".into(), "Alpha holds".into()]),
            ..empty_patch()
        },
        "two-criteria",
        7,
    )
    .expect("two criteria, in the order typed");
    assert_eq!(typed.acceptance, vec!["Zeta holds", "Alpha holds"]);
    let failed = record(
        store,
        &request(
            &typed,
            cut(store, &typed),
            "judge",
            Mode::IndependentSession,
            vec![
                verdict(1, AcceptanceVerdict::Fail, AcceptanceBasis::Judgment, &[]),
                verdict(2, AcceptanceVerdict::Fail, AcceptanceBasis::Judgment, &[]),
            ],
            15,
        ),
    )
    .expect("failing evaluation");
    let reordered = revise(
        store,
        &typed,
        &claim,
        WorkRevisionPatch {
            acceptance: Some(vec!["Alpha holds".into(), "Zeta holds".into()]),
            ..empty_patch()
        },
        "reorder-after-failure",
        16,
    )
    .expect("the holder only reorders the criteria");
    assert_eq!(reordered.revision, typed.revision + 1);
    assert_eq!(reordered.acceptance, vec!["Alpha holds", "Zeta holds"]);
    let carried = store
        .acceptance_evaluation_status(reordered.work_id, None)
        .expect("status")
        .expect("newest evaluation")
        .carried_failure
        .expect("the reorder carries the failure");
    assert_eq!(carried.evaluation, failed.evaluation);
    assert_eq!(
        carried.revised_by,
        crate::domain::CarriedFailureReviser::Executor
    );
    let current = cut(store, &reordered);
    let pass = |supersedes: Option<ObjectId>, second: i64| RecordAcceptanceEvaluationRequest {
        supersedes,
        ..request(
            &reordered,
            current,
            "reviewer",
            Mode::IndependentSession,
            vec![
                verdict(
                    1,
                    AcceptanceVerdict::Pass,
                    AcceptanceBasis::Judgment,
                    std::slice::from_ref(&note),
                ),
                verdict(
                    2,
                    AcceptanceVerdict::Pass,
                    AcceptanceBasis::Judgment,
                    std::slice::from_ref(&note),
                ),
            ],
            second,
        )
    };
    let unacknowledged = pass(None, 25);
    assert!(
        matches!(
            record(store, &unacknowledged),
            Err(StoreError::AcceptanceEvaluationCarriedFailure {
                refusal: crate::CarriedFailureRefusal::Unacknowledged,
                ..
            })
        ),
        "a pass that ignores the failure is refused"
    );
    let acknowledged = pass(Some(failed.evaluation.clone()), 26);
    let recorded = record(store, &acknowledged).expect("the reviewer's acknowledged pass records");
    assert_eq!(recorded.record.supersedes, Some(failed.evaluation));
}

// Every arm of the executor standing counts on its own. In a store the
// holder and the run's executor are always in the holder history too, so
// only a direct check can tell the arms apart.
#[test]
fn each_arm_of_the_executor_standing_counts() {
    let me = SessionId("me".into());
    let other = SessionId("other".into());
    let standing = |mode: Mode,
                    holder: Option<&SessionId>,
                    executor: Option<&SessionId>,
                    history: &[SessionId]| {
        super::super::EvaluatorStanding {
            mode,
            session: &me,
            holder,
            executor,
            history,
        }
        .is_an_executor()
    };
    let others = [other.clone()];
    assert!(!standing(
        Mode::IndependentSession,
        Some(&other),
        Some(&other),
        &others
    ));
    assert!(standing(Mode::SameSession, None, None, &[]), "the mode");
    assert!(
        standing(Mode::SubAgent, Some(&me), None, &[]),
        "the current holder"
    );
    assert!(
        standing(Mode::SubAgent, None, Some(&me), &[]),
        "the run's executor"
    );
    assert!(
        standing(Mode::SubAgent, None, None, std::slice::from_ref(&me)),
        "a former holder"
    );
}
