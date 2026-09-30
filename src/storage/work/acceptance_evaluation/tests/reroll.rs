//! A blocking evaluation stands until something that could change it lies
//! within a later evaluation's basis: a note, a gate, a host check, or an
//! observation that the source changed to another revision. A later record
//! cut at the same position, or one whose cut advanced only by evaluation or
//! checkpoint entries, or by a repeat sighting of the source it judged, does
//! not replace it.

use super::*;

fn judged(
    work: &WorkItem,
    verdict_word: AcceptanceVerdict,
    evidence: &[ObjectId],
    through: i64,
    key: &str,
    second: i64,
) -> RecordAcceptanceEvaluationRequest {
    let mut request = request(
        work,
        through,
        "runner",
        Mode::SameSession,
        vec![verdict(
            1,
            verdict_word,
            AcceptanceBasis::Judgment,
            evidence,
        )],
        second,
    );
    request.attempt_key = Some(key.into());
    request
}

/// The refusal a re-roll on the same evidence gets.
fn rerolled(result: Result<AcceptanceEvaluationReceipt, StoreError>, case: &str) -> String {
    match result {
        Err(StoreError::AcceptanceEvaluationRefused { reason, .. })
            if reason.contains("nothing that could change it was recorded") =>
        {
            reason
        }
        other => panic!("{case}: a re-roll must be refused, got {other:?}"),
    }
}

// B65: the same cut, a cut advanced only by the failing record itself, and one
// advanced only by a checkpoint are re-rolls; a note after the failure lets a
// new evaluation replace it.
#[test]
fn a_fail_stands_until_new_evidence_lies_within_the_next_basis() {
    let mut fixture = fixture("project-reroll-bookkeeping");
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable",
        5,
    );
    let (work, claim, note) = (
        fixture.work.clone(),
        fixture.claim.clone(),
        fixture.evidence.clone(),
    );
    let failed_at = cut(store, &work);
    let failed = record(
        store,
        &judged(&work, AcceptanceVerdict::Fail, &[], failed_at, "fail", 10),
    )
    .expect("the failing evaluation");

    // The same cut the failure read through.
    let reason = rerolled(
        record(
            store,
            &judged(
                &work,
                AcceptanceVerdict::Pass,
                std::slice::from_ref(&note),
                failed_at,
                "same-cut",
                11,
            ),
        ),
        "same cut",
    );
    assert!(
        reason.contains(failed.evaluation.as_str()) && reason.contains("gave fail on criterion 1"),
        "{reason}"
    );
    // A cut advanced only by the failing record itself.
    let after_record = cut(store, &work);
    assert!(after_record > failed_at);
    rerolled(
        record(
            store,
            &judged(
                &work,
                AcceptanceVerdict::Pass,
                std::slice::from_ref(&note),
                after_record,
                "after-record",
                12,
            ),
        ),
        "advanced by the evaluation record",
    );
    // A cut advanced by a checkpoint as well.
    checkpoint(
        store,
        &work,
        &claim,
        "runner",
        "bookkeeping",
        13,
        std::slice::from_ref(&note),
    );
    let after_checkpoint = cut(store, &work);
    assert!(after_checkpoint > after_record);
    rerolled(
        record(
            store,
            &judged(
                &work,
                AcceptanceVerdict::Pass,
                std::slice::from_ref(&note),
                after_checkpoint,
                "after-checkpoint",
                14,
            ),
        ),
        "advanced by a checkpoint",
    );
    // A failing re-roll is refused alike: the verdict does not matter.
    rerolled(
        record(
            store,
            &judged(
                &work,
                AcceptanceVerdict::Fail,
                &[],
                after_checkpoint,
                "fail-again",
                15,
            ),
        ),
        "a failing re-roll",
    );
    // Nothing was recorded by the refusals.
    assert_eq!(
        store
            .acceptance_evaluation_status(work.work_id, None)
            .expect("status read")
            .map(|status| status.evaluation),
        Some(failed.evaluation.clone())
    );

    // A correction after the failure, within the next basis, replaces it.
    let correction = gate(store, &work, &claim, "runner", "correction", &[], 16);
    // A basis cut before the correction still re-rolls.
    rerolled(
        record(
            store,
            &judged(
                &work,
                AcceptanceVerdict::Pass,
                std::slice::from_ref(&note),
                after_checkpoint,
                "before-correction",
                17,
            ),
        ),
        "cut before the correction",
    );
    let passed = record(
        store,
        &judged(
            &work,
            AcceptanceVerdict::Pass,
            std::slice::from_ref(&correction),
            cut(store, &work),
            "after-correction",
            18,
        ),
    )
    .expect("an evaluation after the correction replaces the failure");
    assert_ne!(passed.evaluation, failed.evaluation);
    // A pass is not blocking: the next evaluation needs nothing new.
    record(
        store,
        &judged(
            &work,
            AcceptanceVerdict::Pass,
            std::slice::from_ref(&correction),
            cut(store, &work),
            "after-pass",
            19,
        ),
    )
    .expect("a pass does not hold later evaluations back");
}

// B66: an observed change of the source to another revision is new evidence;
// a repeat sighting of the revision the failure judged is not.
#[test]
fn a_source_change_is_new_evidence_and_a_repeat_sighting_is_not() {
    let mut fixture = fixture("project-reroll-source");
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable",
        5,
    );
    disable_obligation_rules(store, 6);
    let (work, note) = (fixture.work.clone(), fixture.evidence.clone());
    let mut host = HostSession::bind(store, &work, &fixture.claim, 10);
    host.checkpoint(store, true, None, 20);
    record(
        store,
        &judged(
            &work,
            AcceptanceVerdict::Fail,
            &[],
            cut(store, &work),
            "fail",
            30,
        ),
    )
    .expect("the failing evaluation");

    // The host reports a change again, at the revision already judged.
    host.checkpoint(store, true, None, 40);
    rerolled(
        record(
            store,
            &judged(
                &work,
                AcceptanceVerdict::Pass,
                std::slice::from_ref(&note),
                cut(store, &work),
                "repeat",
                50,
            ),
        ),
        "a repeat sighting",
    );

    // A change to another revision is new evidence.
    host.basis.source_revision = "content-revision-2".into();
    host.checkpoint(store, true, None, 60);
    record(
        store,
        &judged(
            &work,
            AcceptanceVerdict::Pass,
            std::slice::from_ref(&note),
            cut(store, &work),
            "changed",
            70,
        ),
    )
    .expect("an evaluation after a source change replaces the failure");
}

// B69: a failure that declared the revision it judged stands through the
// host's later report of the change to that revision; a change to another
// revision is new evidence.
#[test]
fn a_reported_change_to_the_judged_revision_is_no_new_evidence() {
    let mut fixture = fixture("project-reroll-declared");
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable",
        5,
    );
    disable_obligation_rules(store, 6);
    let (work, note) = (fixture.work.clone(), fixture.evidence.clone());
    let mut host = HostSession::bind(store, &work, &fixture.claim, 10);
    host.checkpoint(store, true, None, 20);
    // Judged mid-turn: the evaluator saw revision 2 before the host reported it.
    let mut failing = judged(
        &work,
        AcceptanceVerdict::Fail,
        &[],
        cut(store, &work),
        "fail",
        30,
    );
    failing.source_basis = Some(AcceptanceSourceBasis {
        workspace_id: None,
        fingerprint: "content-revision-2".into(),
    });
    record(store, &failing).expect("the failing evaluation");
    // The host reports that change at the end of the turn.
    host.basis.source_revision = "content-revision-2".into();
    host.checkpoint(store, true, None, 40);
    rerolled(
        record(
            store,
            &judged(
                &work,
                AcceptanceVerdict::Pass,
                std::slice::from_ref(&note),
                cut(store, &work),
                "reported",
                50,
            ),
        ),
        "the reported change to the judged revision",
    );
    host.basis.source_revision = "content-revision-3".into();
    host.checkpoint(store, true, None, 60);
    record(
        store,
        &judged(
            &work,
            AcceptanceVerdict::Pass,
            std::slice::from_ref(&note),
            cut(store, &work),
            "changed",
            70,
        ),
    )
    .expect("a change to another revision is new evidence");
}

// B70: insufficient_evidence and needs_human stand like a fail, and so does
// a blocking record through a policy edit.
#[test]
fn every_blocking_verdict_stands_and_a_policy_edit_is_no_evidence() {
    let mut fixture = fixture("project-reroll-verdicts");
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable",
        5,
    );
    let (work, claim, note) = (
        fixture.work.clone(),
        fixture.claim.clone(),
        fixture.evidence.clone(),
    );
    for (index, (blocking, basis)) in [
        (
            AcceptanceVerdict::InsufficientEvidence,
            AcceptanceBasis::Judgment,
        ),
        (
            AcceptanceVerdict::NeedsHuman,
            AcceptanceBasis::HumanRequired,
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let second = 10 * (i64::try_from(index).expect("index") + 1);
        if index > 0 {
            gate(
                store,
                &work,
                &claim,
                "runner",
                &format!("correction-{index}"),
                &[],
                second,
            );
        }
        let mut input = request(
            &work,
            cut(store, &work),
            "runner",
            Mode::SameSession,
            vec![verdict(1, blocking, basis, &[])],
            second,
        );
        input.attempt_key = Some(format!("blocking-{index}"));
        record(store, &input).expect("the blocking evaluation");
        rerolled(
            record(
                store,
                &judged(
                    &work,
                    AcceptanceVerdict::Pass,
                    std::slice::from_ref(&note),
                    cut(store, &work),
                    &format!("reroll-{index}"),
                    second + 1,
                ),
            ),
            blocking.word(),
        );
    }
    // A policy edit stales the needs_human record but is no evidence.
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        true,
        "stricter",
        40,
    );
    rerolled(
        record(
            store,
            &judged(
                &work,
                AcceptanceVerdict::Pass,
                std::slice::from_ref(&note),
                cut(store, &work),
                "after-policy",
                41,
            ),
        ),
        "after a policy edit",
    );
}

// B71: the observation rule reads the change flag and the revision last seen.
// A quiet sighting at another revision is no new evidence, nor is a flagged
// change to a revision already sighted since the failure; a flagged change
// that carries no revision is.
#[test]
fn the_observation_rule_reads_the_change_flag_and_the_revision_last_seen() {
    let mut fixture = fixture("project-reroll-observations");
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable",
        5,
    );
    disable_obligation_rules(store, 6);
    let (work, note) = (fixture.work.clone(), fixture.evidence.clone());
    let mut host = HostSession::bind(store, &work, &fixture.claim, 10);
    host.checkpoint(store, true, None, 20);
    record(
        store,
        &judged(
            &work,
            AcceptanceVerdict::Fail,
            &[],
            cut(store, &work),
            "fail",
            30,
        ),
    )
    .expect("the failing evaluation");
    let pass = |store: &SqliteStore, key: &str, second: i64| {
        judged(
            &work,
            AcceptanceVerdict::Pass,
            std::slice::from_ref(&note),
            cut(store, &work),
            key,
            second,
        )
    };

    host.report(store, &[(false, Some("content-revision-2"))], 40);
    let request = pass(store, "quiet", 45);
    rerolled(
        record(store, &request),
        "a quiet sighting at another revision",
    );

    host.report(store, &[(true, Some("content-revision-2"))], 50);
    let request = pass(store, "flagged-after-quiet", 55);
    rerolled(
        record(store, &request),
        "a flagged change to a revision sighted since the failure",
    );

    host.report(store, &[(true, None)], 60);
    let request = pass(store, "flagged-without-revision", 65);
    record(store, &request).expect("a flagged change with no revision is new evidence");
}
