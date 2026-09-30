//! A turn that changes the source, runs its tests and asks for an evaluation:
//! the host starts the evaluator at once and reports the turn after the
//! evaluator's cut. A passed host check on the revision the evaluation
//! declared, with its own environment record and the obligation resolution
//! it satisfied, asks for no resubmission; anything else after the cut still
//! does.

use super::basis_moves::entries_after;
use super::*;

const JUDGED: &str = "content-revision-2";

/// A pass citing `note`, declaring `revision` (and `workspace`) as the
/// source it judged, read through `through`.
fn declared_pass(
    work: &WorkItem,
    note: &ObjectId,
    through: i64,
    revision: &str,
    workspace: Option<&str>,
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
            AcceptanceVerdict::Pass,
            AcceptanceBasis::Judgment,
            std::slice::from_ref(note),
        )],
        second,
    );
    request.source_basis = Some(AcceptanceSourceBasis {
        workspace_id: workspace.map(Into::into),
        fingerprint: revision.into(),
    });
    request.attempt_key = Some(key.into());
    request
}

fn stale(store: &SqliteStore, work: &WorkItem) -> Option<AcceptanceStaleReason> {
    store
        .acceptance_evaluation_status(work.work_id, None)
        .expect("status read")
        .expect("an evaluation")
        .stale
}

/// A project whose run was last reported at revision 1, tested there, with
/// the host bound; obligation rules on.
fn tested_at_revision_one(name: &str) -> (Fixture, HostSession) {
    let mut fixture = fixture(name);
    enable(
        &mut fixture.store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable",
        5,
    );
    let (work, claim) = (fixture.work.clone(), fixture.claim.clone());
    let mut host = HostSession::bind(&mut fixture.store, &work, &claim, 10);
    host.checkpoint(
        &mut fixture.store,
        true,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        20,
    );
    (fixture, host)
}

// B73: the requesting turn changes the source to revision 2, runs its test
// there, and asks. The evaluator declares revision 2 and submits before the
// turn's report lands, or after it on the same earlier cut. Either way the
// report (the change, the obligation it opened, the environment, the passed
// test and the resolution it made) asks for nothing, and done seals on it.
#[test]
fn a_same_turn_change_and_passed_test_leave_the_evaluation_fresh_and_it_seals() {
    for submitted_before_the_report in [true, false] {
        let (mut fixture, mut host) = tested_at_revision_one(if submitted_before_the_report {
            "project-same-turn-before"
        } else {
            "project-same-turn-after"
        });
        let store = &mut fixture.store;
        let (work, claim, note) = (
            fixture.work.clone(),
            fixture.claim.clone(),
            fixture.evidence.clone(),
        );
        let started_at = cut(store, &work);
        let request = declared_pass(&work, &note, started_at, JUDGED, None, "evaluate", 30);
        let recorded = if submitted_before_the_report {
            Some(record(store, &request).expect("the evaluation records"))
        } else {
            None
        };
        // The requesting turn's report: the change to revision 2 and its test.
        host.basis.source_revision = JUDGED.into();
        host.checkpoint(
            store,
            true,
            Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
            40,
        );
        for kind in [
            "execution_observation",
            "work_obligation",
            "environment_evidence",
            "verification_evidence",
            "work_obligation_resolution",
        ] {
            assert!(
                !entries_after(store, &work, started_at, kind).is_empty(),
                "the report landed a {kind} after the evaluator's cut"
            );
        }
        let recorded = match recorded {
            Some(recorded) => recorded,
            None => record(store, &request).expect("a late submission on the earlier cut records"),
        };
        assert_eq!(stale(store, &work), None);
        let seal = complete_evaluated(store, &work, &claim, "runner", &note, None, "complete", 50)
            .expect("done seals on the evaluation");
        assert_eq!(seal.acceptance_evaluation, Some(recorded.evaluation));
        let report = store.verify_all().expect("doctor");
        assert!(report.is_healthy(), "{report:?}");
    }
}

// B74: a failed or indeterminate check on the judged revision still asks for
// a resubmission, alone or beside a passed one in the same report, in either
// order, although they share one environment record.
#[test]
fn a_failed_or_indeterminate_check_in_the_same_report_still_asks_for_a_resubmission() {
    for (name, checks) in [
        (
            "passed-then-failed",
            vec![
                (VerificationKind::Test, ExecutionOutcome::Succeeded),
                (VerificationKind::Test, ExecutionOutcome::Failed),
            ],
        ),
        (
            "failed-then-passed",
            vec![
                (VerificationKind::Test, ExecutionOutcome::Failed),
                (VerificationKind::Test, ExecutionOutcome::Succeeded),
            ],
        ),
        (
            "indeterminate",
            vec![(VerificationKind::Build, ExecutionOutcome::Unknown)],
        ),
    ] {
        let (mut fixture, mut host) = tested_at_revision_one(&format!("project-mixed-{name}"));
        let store = &mut fixture.store;
        let (work, note) = (fixture.work.clone(), fixture.evidence.clone());
        let started_at = cut(store, &work);
        record(
            store,
            &declared_pass(&work, &note, started_at, JUDGED, None, "evaluate", 30),
        )
        .expect("the evaluation records");
        host.basis.source_revision = JUDGED.into();
        let minted = host.checkpoint_checks(store, true, &checks, 40);
        assert_eq!(minted.len(), checks.len(), "{name}");
        assert_eq!(
            stale(store, &work),
            Some(AcceptanceStaleReason::Mutation),
            "{name}"
        );
        let refused = record(
            store,
            &declared_pass(&work, &note, started_at, JUDGED, None, "resubmit", 45),
        );
        assert!(
            matches!(
                refused,
                Err(StoreError::AcceptanceEvaluationBasisMoved {
                    moved: EvaluationBasisMove::CheckRecorded,
                    ..
                })
            ),
            "{name}: {refused:?}"
        );
    }
}

// B75: the exemption needs the declared revision. Without a declaration, or
// when the declared workspace is another one, a passed check after the cut
// still asks for a resubmission.
#[test]
fn a_passed_check_is_exempt_only_on_the_declared_revision() {
    // No declaration: as before.
    let (mut fixture, mut host) = tested_at_revision_one("project-undeclared");
    let store = &mut fixture.store;
    let (work, note) = (fixture.work.clone(), fixture.evidence.clone());
    let started_at = cut(store, &work);
    let mut undeclared = declared_pass(&work, &note, started_at, JUDGED, None, "undeclared", 30);
    undeclared.source_basis = None;
    record(store, &undeclared).expect("the evaluation records");
    host.checkpoint(
        store,
        false,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        40,
    );
    assert_eq!(stale(store, &work), Some(AcceptanceStaleReason::Mutation));

    // Another declared workspace: the check ran elsewhere.
    let (mut fixture, mut host) = tested_at_revision_one("project-other-workspace");
    let store = &mut fixture.store;
    let (work, note) = (fixture.work.clone(), fixture.evidence.clone());
    let started_at = cut(store, &work);
    let revision = host.basis.source_revision.clone();
    record(
        store,
        &declared_pass(
            &work,
            &note,
            started_at,
            &revision,
            Some("another-workspace"),
            "elsewhere",
            30,
        ),
    )
    .expect("the evaluation records");
    host.checkpoint(
        store,
        false,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        40,
    );
    assert_eq!(stale(store, &work), Some(AcceptanceStaleReason::Mutation));
}

// B76: an exempt passed check is still new evidence for a later evaluation
// that includes it, and only for one that does: after a blocking record, a
// replacement on its own cut is a re-roll, and one whose cut includes the
// check records.
#[test]
fn an_exempt_check_is_new_evidence_only_within_the_next_basis() {
    let (mut fixture, mut host) = tested_at_revision_one("project-exempt-reroll");
    let store = &mut fixture.store;
    let (work, note) = (fixture.work.clone(), fixture.evidence.clone());
    let started_at = cut(store, &work);
    let mut failing = declared_pass(&work, &note, started_at, JUDGED, None, "fail", 30);
    failing.verdicts = vec![verdict(
        1,
        AcceptanceVerdict::Fail,
        AcceptanceBasis::Judgment,
        &[],
    )];
    record(store, &failing).expect("the failing evaluation records");
    host.basis.source_revision = JUDGED.into();
    host.checkpoint(
        store,
        true,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        40,
    );
    let rerolled = record(
        store,
        &declared_pass(&work, &note, started_at, JUDGED, None, "same-cut", 45),
    );
    assert!(
        matches!(
            &rerolled,
            Err(StoreError::AcceptanceEvaluationRefused { reason, .. })
                if reason.contains("nothing that could change it was recorded")
        ),
        "{rerolled:?}"
    );
    record(
        store,
        &declared_pass(
            &work,
            &note,
            cut(store, &work),
            JUDGED,
            None,
            "after-check",
            50,
        ),
    )
    .expect("an evaluation whose basis includes the check replaces the failure");
}

// B80: a passed check on the declared revision is exempt only while the run
// was last sighted there. A late verification of an earlier run of the
// declared revision, after the run moved on, still leaves the evaluation
// stale.
#[test]
fn a_late_check_of_the_declared_revision_after_the_run_moved_on_still_counts() {
    let mut fixture = fixture("project-late-producer");
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
    let declared = host.basis.source_revision.clone();
    let earlier = host.checkpoint(
        store,
        false,
        Some((VerificationKind::Build, ExecutionOutcome::Succeeded)),
        20,
    );
    let producer = load_typed_work_object::<VerificationEvidence>(
        &store.connection,
        &earlier[0],
        "verification_evidence",
    )
    .expect("earlier verification")
    .producer_observation;
    // The run moves on to another revision.
    host.basis.source_revision = JUDGED.into();
    host.checkpoint(store, true, None, 30);
    record(
        store,
        &declared_pass(
            &work,
            &note,
            cut(store, &work),
            &declared,
            None,
            "earlier",
            40,
        ),
    )
    .expect("the evaluation records");
    // A late verification of the earlier run on the declared revision.
    host.cite_earlier_producer(store, &producer, None, 50);
    assert_eq!(stale(store, &work), Some(AcceptanceStaleReason::Mutation));
}

// B81: the mirror case. The evaluation declares the revision the run is at,
// and a passed check of an earlier revision is verified late, after its cut:
// the check did not run on the judged source, so it still leaves the
// evaluation stale although the run's newest sighting is the declared one.
#[test]
fn a_late_passed_check_on_another_revision_than_the_declared_one_still_counts() {
    let mut fixture = fixture("project-other-revision");
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
    let earlier = host.checkpoint(
        store,
        false,
        Some((VerificationKind::Build, ExecutionOutcome::Succeeded)),
        20,
    );
    let producer = load_typed_work_object::<VerificationEvidence>(
        &store.connection,
        &earlier[0],
        "verification_evidence",
    )
    .expect("earlier verification")
    .producer_observation;
    // The run moves on, and the evaluation judges where it is now.
    host.basis.source_revision = JUDGED.into();
    host.checkpoint(store, true, None, 30);
    record(
        store,
        &declared_pass(&work, &note, cut(store, &work), JUDGED, None, "current", 40),
    )
    .expect("the evaluation records");
    assert_eq!(stale(store, &work), None);
    // A late verification of the earlier revision's build.
    host.cite_earlier_producer(store, &producer, None, 50);
    assert_eq!(stale(store, &work), Some(AcceptanceStaleReason::Mutation));
}
