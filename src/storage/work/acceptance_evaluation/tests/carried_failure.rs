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
        &[Mode::SameSession],
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
    let pass = |supersedes: Option<ObjectId>, second: i64| RecordAcceptanceEvaluationRequest {
        supersedes,
        ..request(
            &revised,
            current,
            "runner",
            Mode::SameSession,
            vec![verdict(
                1,
                AcceptanceVerdict::Pass,
                AcceptanceBasis::Judgment,
                std::slice::from_ref(&note),
            )],
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
    let recorded = record(store, &acknowledged).expect("the acknowledged pass records");
    assert_eq!(recorded.record.supersedes, Some(failed.evaluation));
}
