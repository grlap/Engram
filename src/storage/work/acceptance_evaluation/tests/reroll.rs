//! A blocking evaluation stands until something that could change it lies
//! within a later evaluation's basis: a note, a gate, a host check, or an
//! observation that the source changed to another revision. A later record
//! cut at the same position, or one whose cut advanced only by evaluation or
//! checkpoint entries, or by a repeat sighting of the source it judged, does
//! not replace it.
//!
//! Every record in these tests goes through a wrapper that first reads the
//! evaluation status: at the run's head, the status's `reroll` must predict
//! this rule's answer exactly, so the status and the record transaction are
//! shown to share one assessment rather than said to.

use super::*;
use crate::domain::{EvaluationRerollMismatch, RerollAdmissionCause};

/// Records as the shared helper does, after reading the status at the same
/// moment. When the request is cut at the run's head, an admitted record had
/// no standing cause in the status, and a refusal for the standing
/// evaluation carries exactly the cause the status showed. A refusal by an
/// earlier rule says nothing of this one, since the record stops before it.
fn record(
    store: &mut SqliteStore,
    request: &RecordAcceptanceEvaluationRequest,
) -> Result<AcceptanceEvaluationReceipt, StoreError> {
    let item = store.get_work_item(request.work_id).expect("item");
    let at_head = item.active_run_id.is_some_and(|run| {
        store
            .work_feed_head(&FeedId::RunExecution(run))
            .expect("run feed head")
            == request.evaluated_through
    });
    let standing = store
        .acceptance_evaluation_status(request.work_id, None)
        .expect("status read")
        .and_then(|status| status.reroll);
    let result = super::record(store, request);
    let answered = match &result {
        Ok(receipt) if !receipt.replayed => Some(None),
        Err(StoreError::AcceptanceEvaluationAdmissionRefused { cause, .. }) => match cause.as_ref()
        {
            AcceptanceEvaluationAdmissionCause::Reroll(cause) => Some(Some(cause.clone())),
            _ => None,
        },
        _ => None,
    };
    if at_head && let Some(answered) = answered {
        assert_eq!(
            answered, standing,
            "the status read at the head predicts the record's re-roll answer"
        );
    }
    result
}

/// The standing cause the status shows now.
fn standing(store: &SqliteStore, work: &WorkItem) -> Option<Box<RerollAdmissionCause>> {
    store
        .acceptance_evaluation_status(work.work_id, None)
        .expect("status read")
        .and_then(|status| status.reroll)
}

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

/// The refusal a re-roll on the same evidence gets: the typed cause, with the
/// shared CLI/MCP error shape and unchanged words.
fn rerolled(result: Result<AcceptanceEvaluationReceipt, StoreError>, case: &str) -> String {
    assert!(
        matches!(
            &result,
            Err(StoreError::AcceptanceEvaluationAdmissionRefused { cause, .. })
                if matches!(**cause, AcceptanceEvaluationAdmissionCause::Reroll(_))
        ),
        "{case}: a re-roll must be refused, got {result:?}"
    );
    let reason = typed_cause(
        result,
        Typed::Reroll(
            EvaluationRerollMismatch::BlockingEvaluationStands,
            Remedy::RecordNewEvidenceThenEvaluate,
        ),
    );
    assert!(
        reason.contains("nothing that could change it was recorded"),
        "{case}: {reason}"
    );
    reason
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

// An accounted unadmitted change after a blocking evaluation's cut voids that
// evaluation even when it reports the revision the evaluation judged, so it is
// new evidence for the next one: a fail, an insufficient-evidence verdict or
// one that needs a human is replaced by an evaluation whose basis includes the
// change, and the refusal naming no new evidence never appears while the
// blocking record reads stale for it. A basis that stops before the change is
// refused as void.
#[test]
fn an_accounted_unadmitted_change_is_new_evidence_whatever_revision_it_reports() {
    for blocking in [
        AcceptanceVerdict::Fail,
        AcceptanceVerdict::InsufficientEvidence,
        AcceptanceVerdict::NeedsHuman,
    ] {
        let case = blocking.word();
        let mut fixture = fixture(&format!("project-reroll-unadmitted-{case}"));
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
        let (work, claim, note) = (
            fixture.work.clone(),
            fixture.claim.clone(),
            fixture.evidence.clone(),
        );
        let mut host = HostSession::bind(store, &work, &claim, 10);
        // A sighting, not a change, so the later report is accounted as a
        // change of its own rather than a repeat of this one.
        host.checkpoint(store, false, None, 20);
        let blocked_at = cut(store, &work);
        let blocked = record(
            store,
            &judged(&work, blocking, &[], blocked_at, "blocking", 30),
        )
        .expect("the blocking evaluation");
        // The change reports the very revision the blocking evaluation judged.
        let judged_revision = host.basis.source_revision.clone();
        let change = super::citation_sources::unadmitted_change(
            store,
            &host,
            &claim,
            &judged_revision,
            "late-report",
            40,
        );
        assert!(
            matches!(
                change.accounting,
                crate::domain::ObservationAccounting::SourceChange { .. }
            ),
            "{case}: {:?}",
            change.accounting
        );
        let status = store
            .acceptance_evaluation_status(work.work_id, None)
            .expect("status read")
            .expect("newest evaluation");
        assert_eq!(status.evaluation, blocked.evaluation, "{case}");
        // Named as the barrier it is, not as a content mutation.
        assert_eq!(
            status.stale,
            Some(AcceptanceStaleReason::UnadmittedChange),
            "{case}"
        );
        assert_eq!(
            status
                .stale_observation
                .as_ref()
                .map(|deciding| deciding.observation.clone()),
            Some(change.observation.clone()),
            "{case}"
        );
        // A basis that stops before the change is refused: the change voids
        // it as it voids the blocking record.
        let before = record(
            store,
            &judged(
                &work,
                AcceptanceVerdict::Pass,
                std::slice::from_ref(&note),
                blocked_at,
                "before-the-change",
                45,
            ),
        );
        assert!(
            matches!(
                &before,
                Err(StoreError::AcceptanceEvaluationBasisMoved { .. })
            ),
            "{case}: {before:?}"
        );
        // One whose basis includes the change replaces the blocking record.
        let replacement = record(
            store,
            &judged(
                &work,
                AcceptanceVerdict::Pass,
                std::slice::from_ref(&note),
                cut(store, &work),
                "after-the-change",
                50,
            ),
        )
        .unwrap_or_else(|error| panic!("{case}: the change is new evidence: {error:?}"));
        let status = store
            .acceptance_evaluation_status(work.work_id, None)
            .expect("status read")
            .expect("newest evaluation");
        assert_eq!(
            (status.evaluation, status.stale),
            (replacement.evaluation, None),
            "{case}"
        );
    }
}

// The status shows a standing blocking evaluation with the refusal's own
// fields before any evaluator starts. A release and a re-claim keep the run,
// so the evaluation still stands for the new claim, as the record transaction
// finds too. A note recorded after a read that showed the standing cause lies
// within the next basis, and that record is admitted: the transaction
// assesses again, so the earlier read is no token either way.
#[test]
fn the_status_shows_the_standing_cause_and_the_record_assesses_it_again() {
    let mut fixture = fixture("project-reroll-status");
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable",
        5,
    );
    let (work, held, note) = (
        fixture.work.clone(),
        fixture.claim.clone(),
        fixture.evidence.clone(),
    );
    let run = work.active_run_id.expect("active run");
    assert!(
        store
            .acceptance_evaluation_status(work.work_id, None)
            .expect("status read")
            .is_none(),
        "no evaluation, no status"
    );
    let failed_at = cut(store, &work);
    let failed = record(
        store,
        &judged(&work, AcceptanceVerdict::Fail, &[], failed_at, "fail", 10),
    )
    .expect("the failing evaluation");
    let expected = RerollAdmissionCause {
        mismatch: EvaluationRerollMismatch::BlockingEvaluationStands,
        evaluation: failed.evaluation.clone(),
        feed: FeedId::RunExecution(run),
        after_position: failed_at,
        through_position: cut(store, &work),
        criterion: 1,
        verdict: AcceptanceVerdict::Fail,
        remedy: Remedy::RecordNewEvidenceThenEvaluate,
    };
    assert_eq!(standing(store, &work).as_deref(), Some(&expected));
    let refused = record(
        store,
        &judged(
            &work,
            AcceptanceVerdict::Pass,
            std::slice::from_ref(&note),
            cut(store, &work),
            "same-evidence",
            11,
        ),
    );
    assert!(
        matches!(
            &refused,
            Err(StoreError::AcceptanceEvaluationAdmissionRefused { cause, .. })
                if **cause == AcceptanceEvaluationAdmissionCause::Reroll(Box::new(expected.clone()))
        ),
        "{refused:?}"
    );

    // Release and re-claim: the run stays, and so does the standing cause.
    let current = store.get_work_item(work.work_id).expect("item");
    store
        .release_work(
            &crate::domain::ReleaseWorkRequest {
                work_id: current.work_id,
                run_id: held.run_id,
                expected_work_revision: current.revision,
                holder: held.holder.clone(),
                claim_id: held.claim_id,
                claim_fence: held.fence,
                reason: "stepping away".into(),
                waiver_reason: None,
                actor: actor("runner"),
                idempotency_key: "release-runner".into(),
                released_at: at(20),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("release");
    let released = store.get_work_item(work.work_id).expect("item");
    let reclaim = claim(store, &released, "runner", "reclaim", 21, 3_600);
    let current = store.get_work_item(work.work_id).expect("item");
    assert_eq!(
        current.active_run_id,
        Some(run),
        "the re-claim keeps the run"
    );
    let read = standing(store, &current).expect("the evaluation still stands");
    assert_eq!(read.evaluation, failed.evaluation);
    assert_eq!(read.through_position, cut(store, &current));
    rerolled(
        record(
            store,
            &judged(
                &current,
                AcceptanceVerdict::Pass,
                std::slice::from_ref(&note),
                cut(store, &current),
                "after-reclaim",
                22,
            ),
        ),
        "after a release and a re-claim",
    );

    // The read above showed the standing cause; a correction recorded after
    // it lies within the next basis, and the record is admitted.
    let correction = gate(store, &current, &reclaim, "runner", "correction", &[], 23);
    record(
        store,
        &judged(
            &current,
            AcceptanceVerdict::Pass,
            std::slice::from_ref(&correction),
            cut(store, &current),
            "after-correction",
            24,
        ),
    )
    .expect("an evaluation after the correction replaces the failure");
    assert!(
        standing(store, &current).is_none(),
        "a passing newest evaluation leaves nothing standing"
    );
}

// A read that showed nothing standing admits nothing: when a newer blocking
// evaluation lands after it, a record is refused against that evaluation,
// whether it is cut where the read was or at the new head.
#[test]
fn a_permissive_read_before_a_newer_blocking_evaluation_admits_nothing() {
    let mut fixture = fixture("project-reroll-race");
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable",
        5,
    );
    let (work, note) = (fixture.work.clone(), fixture.evidence.clone());
    record(
        store,
        &judged(
            &work,
            AcceptanceVerdict::Pass,
            std::slice::from_ref(&note),
            cut(store, &work),
            "pass",
            10,
        ),
    )
    .expect("a passing evaluation");
    assert!(
        standing(store, &work).is_none(),
        "a pass leaves nothing standing"
    );
    let read_at = cut(store, &work);
    let newer = record(
        store,
        &judged(
            &work,
            AcceptanceVerdict::Fail,
            &[],
            read_at,
            "newer-fail",
            11,
        ),
    )
    .expect("a newer blocking evaluation");
    for (case, through, key) in [
        ("cut where the permissive read was", read_at, "stale-read"),
        ("cut at the new head", cut(store, &work), "new-head"),
    ] {
        let reason = rerolled(
            record(
                store,
                &judged(
                    &work,
                    AcceptanceVerdict::Pass,
                    std::slice::from_ref(&note),
                    through,
                    key,
                    12,
                ),
            ),
            case,
        );
        assert!(
            reason.contains(newer.evaluation.as_str()),
            "{case}: {reason}"
        );
    }
}
