//! A run that moved past the evidence basis an evaluation read through: a
//! host check asks for a resubmission, a source change the evaluation did not
//! judge voids it, and a change to the revision it declared it judged does not
//! count, at recording or at completion.

use super::*;
use crate::{EvaluationBasisMove, SourceChangeDetection};

/// The class and stable code of a moved-basis refusal. A host may relay
/// only the error message to its evaluator, so the message itself must
/// carry the class's remedy, the same one the MCP details give.
fn moved(result: Result<AcceptanceEvaluationReceipt, StoreError>) -> (EvaluationBasisMove, String) {
    let Err(error) = result else {
        panic!("expected a moved basis, got {result:?}");
    };
    let value = crate::store_error_value(&error);
    let code = value["error"]["code"]
        .as_str()
        .expect("error code")
        .to_owned();
    let message = value["error"]["message"].as_str().expect("error message");
    match error {
        StoreError::AcceptanceEvaluationBasisMoved { moved, .. } => {
            assert!(message.contains(moved.remedy()), "{message}");
            assert_eq!(value["error"]["details"]["remedy"], moved.remedy());
            (moved, code)
        }
        other => panic!("expected a moved basis, got {other:?}"),
    }
}

/// A passing judgment of the fixture's criterion at `cut`, declaring
/// `source` as the revision it judged.
fn judged(
    work: &WorkItem,
    note: &ObjectId,
    cut: i64,
    source: Option<AcceptanceSourceBasis>,
    second: i64,
) -> RecordAcceptanceEvaluationRequest {
    let mut request = request(
        work,
        cut,
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
    request.source_basis = source;
    request
}

/// The ids of the run-feed entries of `kind` after `position`.
fn entries_after(store: &SqliteStore, work: &WorkItem, position: i64, kind: &str) -> Vec<ObjectId> {
    let run = work.active_run_id.expect("active run");
    let mut statement = store
        .connection
        .prepare(
            "SELECT object_id FROM work_feed_entries
             WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND position > ?2
               AND object_kind = ?3
             ORDER BY position",
        )
        .expect("feed query");
    statement
        .query_map(
            rusqlite::params![run.0.to_string(), position, kind],
            |row| row.get::<_, String>(0),
        )
        .expect("feed rows")
        .map(|stored| ObjectId::from_stored(stored.expect("feed row")).expect("stored id"))
        .collect()
}

fn revision(fingerprint: &str, workspace: Option<&str>) -> AcceptanceSourceBasis {
    AcceptanceSourceBasis {
        workspace_id: workspace.map(Into::into),
        fingerprint: fingerprint.into(),
    }
}

#[test]
fn a_host_check_after_the_basis_asks_for_a_resubmission() {
    let mut fixture = fixture("project-basis-check");
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
    let read = cut(store, &work);
    host.checkpoint(
        store,
        false,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        20,
    );
    assert_eq!(
        moved(record(store, &judged(&work, &note, read, None, 25))),
        (
            EvaluationBasisMove::CheckRecorded,
            "acceptance_evaluation_resubmit".into()
        )
    );
    record(store, &judged(&work, &note, cut(store, &work), None, 26))
        .expect("a re-read resubmission records");
}

#[test]
fn a_source_change_the_evaluation_did_not_judge_voids_it() {
    let mut fixture = fixture("project-basis-void");
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
    let read = cut(store, &work);
    // A check in the same turn does not soften it: the unseen change wins.
    host.checkpoint(
        store,
        true,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        20,
    );
    let void = (
        EvaluationBasisMove::SourceChanged,
        "acceptance_evaluation_void".to_owned(),
    );
    for (source, second) in [
        (None, 25),
        (Some(revision("another-revision", None)), 26),
        (
            Some(revision("content-revision-1", Some("another-workspace"))),
            27,
        ),
    ] {
        assert_eq!(
            moved(record(store, &judged(&work, &note, read, source, second))),
            void
        );
    }
}

#[test]
fn a_source_change_to_the_judged_revision_does_not_move_the_basis() {
    let mut fixture = fixture("project-basis-judged");
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable",
        5,
    );
    // Obligation rules stay on: the change opens a "tests have not run"
    // obligation, and that obligation was seen with it.
    let (work, note) = (fixture.work.clone(), fixture.evidence.clone());
    let mut host = HostSession::bind(store, &work, &fixture.claim, 10);
    let read = cut(store, &work);
    host.checkpoint(store, true, None, 20);
    assert!(cut(store, &work) > read, "the change reached the run feed");
    // The change opened one "tests have not run" obligation, triggered by
    // its own observation: the exemption must skip that obligation too.
    let changes = entries_after(store, &work, read, "execution_observation");
    let opened = entries_after(store, &work, read, "work_obligation");
    assert_eq!(opened.len(), 1, "the change opened one obligation");
    let obligation: WorkObligation =
        load_typed_work_object(&store.connection, &opened[0], "work_obligation")
            .expect("opened obligation");
    assert!(changes.contains(&obligation.triggering_observation));
    let recorded = record(
        store,
        &judged(
            &work,
            &note,
            read,
            Some(revision("content-revision-1", None)),
            25,
        ),
    )
    .expect("the evaluator judged the revision the host later reported");
    assert_eq!(
        store
            .acceptance_evaluation_status(work.work_id, None)
            .expect("status read")
            .map(|status| (status.evaluation, status.stale)),
        Some((recorded.evaluation, None))
    );

    // The passing test that resolves the obligation that change opened is a
    // check the evaluator has not seen: it asks for a resubmission, not a new
    // evaluation, and a re-read resubmission records.
    host.checkpoint(
        store,
        false,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        26,
    );
    assert_eq!(
        store
            .acceptance_evaluation_status(work.work_id, None)
            .expect("status read")
            .map(|status| status.stale),
        Some(Some(AcceptanceStaleReason::Mutation))
    );
    // Its own attempt key: the identical content would replay the committed
    // record instead of submitting anew.
    let mut unrefreshed = judged(
        &work,
        &note,
        read,
        Some(revision("content-revision-1", None)),
        27,
    );
    unrefreshed.attempt_key = Some("after-the-check".into());
    assert_eq!(
        moved(record(store, &unrefreshed)),
        (
            EvaluationBasisMove::CheckRecorded,
            "acceptance_evaluation_resubmit".into()
        )
    );
    let resubmitted = record(
        store,
        &judged(
            &work,
            &note,
            cut(store, &work),
            Some(revision("content-revision-1", None)),
            28,
        ),
    )
    .expect("the re-read resubmission records");
    assert_eq!(
        store
            .acceptance_evaluation_status(work.work_id, None)
            .expect("status read")
            .map(|status| (status.evaluation, status.stale)),
        Some((resubmitted.evaluation, None))
    );

    // A later change to another revision retires it at completion.
    host.basis.source_revision = "content-revision-2".into();
    host.checkpoint(store, true, None, 30);
    assert_eq!(
        store
            .acceptance_evaluation_status(work.work_id, None)
            .expect("status read")
            .map(|status| status.stale),
        Some(Some(AcceptanceStaleReason::Mutation))
    );
}

#[test]
fn a_reported_change_that_leaves_the_revision_unchanged_is_not_a_source_change() {
    let mut fixture = fixture("project-basis-same-revision");
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable",
        5,
    );
    // Obligation rules stay on: a real change opens a "tests have not run"
    // obligation, and a repeated revision must not.
    let (work, note) = (fixture.work.clone(), fixture.evidence.clone());
    let mut host = HostSession::bind(store, &work, &fixture.claim, 10);
    let before = cut(store, &work);
    host.checkpoint(store, true, None, 20);
    assert_eq!(
        entries_after(store, &work, before, "work_obligation").len(),
        1,
        "the first change to a revision opens the obligation"
    );
    let read = cut(store, &work);

    // The host reports a change again, but at the revision the run already
    // recorded, as it does for writes under git-ignored paths.
    host.checkpoint(store, true, None, 30);
    let repeated = entries_after(store, &work, read, "execution_observation");
    assert_eq!(repeated.len(), 1);
    let observation: ExecutionObservation =
        load_typed_work_object(&store.connection, &repeated[0], "execution_observation")
            .expect("repeated observation");
    assert!(
        !observation.source_changed,
        "an unchanged revision is not a source change"
    );
    assert!(
        entries_after(store, &work, read, "work_obligation").is_empty(),
        "an unchanged revision opens no obligation"
    );
    let recorded = record(store, &judged(&work, &note, read, None, 35))
        .expect("an unchanged revision neither voids nor asks for a resubmission");
    // An evaluation recorded before such a report stays fresh at completion.
    host.checkpoint(store, true, None, 36);
    assert_eq!(
        store
            .acceptance_evaluation_status(work.work_id, None)
            .expect("status read")
            .map(|status| (status.evaluation, status.stale)),
        Some((recorded.evaluation, None))
    );

    // The revision fingerprints the full content, so the same revision
    // reported from another workspace is still no change; the workspace is
    // kept for audit only. A new revision is a change.
    for (workspace, revision, changes, second) in [
        ("another-workspace", "content-revision-1", false, 40),
        ("workspace-evaluated", "content-revision-2", true, 50),
    ] {
        host.basis.workspace_id = workspace.into();
        host.basis.source_revision = revision.into();
        let position = cut(store, &work);
        host.checkpoint(store, true, None, second);
        let reported = entries_after(store, &work, position, "execution_observation");
        let observation: ExecutionObservation =
            load_typed_work_object(&store.connection, &reported[0], "execution_observation")
                .expect("reported observation");
        assert_eq!(
            observation.source_changed, changes,
            "{workspace} {revision}"
        );
        assert_eq!(
            entries_after(store, &work, position, "work_obligation").len(),
            usize::from(changes),
            "{workspace} {revision}"
        );
    }
    // Its own attempt key: the identical content would replay the committed
    // record instead of submitting anew.
    let mut unseen = judged(&work, &note, read, None, 55);
    unseen.attempt_key = Some("after-the-change".into());
    assert_eq!(
        moved(record(store, &unseen)),
        (
            EvaluationBasisMove::SourceChanged,
            "acceptance_evaluation_void".into()
        )
    );
}

/// Whether each execution observation recorded after `position` counts as a
/// source change, in feed order.
fn recorded_changes(store: &SqliteStore, work: &WorkItem, position: i64) -> Vec<bool> {
    entries_after(store, work, position, "execution_observation")
        .iter()
        .map(|id| {
            load_typed_work_object::<ExecutionObservation>(
                &store.connection,
                id,
                "execution_observation",
            )
            .expect("recorded observation")
            .source_changed
        })
        .collect()
}

/// Asserts the run has `count` obligations and a passing test left none open.
fn all_obligations_satisfied(store: &SqliteStore, run: crate::domain::WorkRunId, count: usize) {
    let obligations = store.work_run_obligations(run).expect("run obligations");
    assert_eq!(obligations.len(), count, "{obligations:?}");
    assert!(
        obligations
            .iter()
            .all(|record| record.state != crate::domain::WorkObligationState::Open),
        "{obligations:?}"
    );
}

#[test]
fn a_later_observation_in_the_same_checkpoint_sees_the_earlier_change() {
    let mut fixture = fixture("project-basis-one-checkpoint");
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable",
        5,
    );
    let work = fixture.work.clone();
    let mut host = HostSession::bind(store, &work, &fixture.claim, 10);
    let start = cut(store, &work);
    host.report(
        store,
        &[
            (true, Some("content-revision-1")),
            (true, Some("content-revision-1")),
        ],
        20,
    );
    assert_eq!(recorded_changes(store, &work, start), [true, false]);
    assert_eq!(
        entries_after(store, &work, start, "work_obligation").len(),
        1
    );
}

#[test]
fn after_a_revision_less_change_the_host_flag_stands_and_a_revision_re_anchors_tests() {
    let mut fixture = fixture("project-basis-revision-less");
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable",
        5,
    );
    let work = fixture.work.clone();
    let run = work.active_run_id.expect("active run");
    let mut host = HostSession::bind(store, &work, &fixture.claim, 10);
    let start = cut(store, &work);
    // A change at a known revision, one the host reported without a
    // revision, then one back at the known revision. The newest recorded
    // change carries no revision, so the last report is not compared away:
    // it stays a change and re-anchors the obligations at its revision.
    host.report(store, &[(true, Some("content-revision-1"))], 20);
    host.report(store, &[(true, None)], 30);
    host.report(store, &[(true, Some("content-revision-1"))], 40);
    assert_eq!(recorded_changes(store, &work, start), [true, true, true]);
    host.checkpoint(
        store,
        false,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        50,
    );
    all_obligations_satisfied(store, run, 3);
}

#[test]
fn a_revision_move_seen_only_on_an_observation_claiming_no_change_still_counts() {
    let mut fixture = fixture("project-basis-quiet-move");
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
    let run = work.active_run_id.expect("active run");
    let mut host = HostSession::bind(store, &work, &fixture.claim, 10);
    let start = cut(store, &work);
    host.report(store, &[(true, Some("content-revision-1"))], 20);
    let read = cut(store, &work);
    // The content moves to another revision on an observation that claims
    // no change, such as a check run after someone else's edit. The next
    // reported change at that revision is compared with the newest recorded
    // change, not with that observation, so it counts.
    host.report(store, &[(false, Some("content-revision-2"))], 30);
    host.report(store, &[(true, Some("content-revision-2"))], 40);
    assert_eq!(recorded_changes(store, &work, start), [true, false, true]);
    assert_eq!(
        moved(record(store, &judged(&work, &note, read, None, 45))),
        (
            EvaluationBasisMove::SourceChanged,
            "acceptance_evaluation_void".into()
        )
    );
    host.basis.source_revision = "content-revision-2".into();
    host.checkpoint(
        store,
        false,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        50,
    );
    all_obligations_satisfied(store, run, 2);
}

#[test]
fn a_revert_after_a_move_seen_only_on_an_observation_claiming_no_change_still_counts() {
    let mut fixture = fixture("project-basis-quiet-revert");
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
    let mut host = HostSession::bind(store, &work, &fixture.claim, 10);
    let start = cut(store, &work);
    host.report(store, &[(true, Some("content-revision-1"))], 20);
    // The content moves to another revision on an observation that claims
    // no change, and the evaluator judges that revision.
    host.report(store, &[(false, Some("content-revision-2"))], 30);
    let read = cut(store, &work);
    // A change back to the newest recorded change's revision is still a move
    // away from what the run last saw, so it counts.
    host.report(store, &[(true, Some("content-revision-1"))], 40);
    assert_eq!(recorded_changes(store, &work, start), [true, false, true]);
    assert_eq!(
        moved(record(
            store,
            &judged(
                &work,
                &note,
                read,
                Some(revision("content-revision-2", None)),
                45
            )
        )),
        (
            EvaluationBasisMove::SourceChanged,
            "acceptance_evaluation_void".into()
        )
    );
}

#[test]
fn a_change_after_the_content_went_away_and_came_back_still_counts() {
    let mut fixture = fixture("project-basis-away-and-back");
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
    let mut host = HostSession::bind(store, &work, &fixture.claim, 10);
    let start = cut(store, &work);
    host.report(store, &[(true, Some("content-revision-1"))], 20);
    // Two observations that claim no change see the content move away and
    // back; the evaluator judges the revision in between.
    host.report(store, &[(false, Some("content-revision-2"))], 30);
    let read = cut(store, &work);
    host.report(store, &[(false, Some("content-revision-1"))], 40);
    let before_last = cut(store, &work);
    host.report(store, &[(true, Some("content-revision-1"))], 50);
    assert_eq!(
        recorded_changes(store, &work, start),
        [true, false, false, true]
    );
    assert_eq!(
        entries_after(store, &work, before_last, "work_obligation").len(),
        1,
        "the change after the content came back opens an obligation"
    );
    assert_eq!(
        moved(record(
            store,
            &judged(
                &work,
                &note,
                read,
                Some(revision("content-revision-2", None)),
                55
            )
        )),
        (
            EvaluationBasisMove::SourceChanged,
            "acceptance_evaluation_void".into()
        )
    );
}

#[test]
fn a_move_seen_only_in_environment_evidence_still_counts_at_the_next_change() {
    let mut fixture = fixture("project-basis-environment-move");
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
    let mut host = HostSession::bind(store, &work, &fixture.claim, 10);
    let start = cut(store, &work);
    host.report(store, &[(true, Some("content-revision-1"))], 20);
    // Only environment evidence, which carries its own source basis, sees the
    // content at another revision; the evaluator judges that revision.
    host.capture_environment(store, "content-revision-2", 30);
    assert_eq!(
        entries_after(store, &work, start, "environment_evidence").len(),
        1
    );
    let read = cut(store, &work);
    host.report(store, &[(true, Some("content-revision-1"))], 40);
    assert_eq!(recorded_changes(store, &work, start), [true, true]);
    assert_eq!(
        entries_after(store, &work, read, "work_obligation").len(),
        1,
        "the change back opens an obligation"
    );
    assert_eq!(
        moved(record(
            store,
            &judged(
                &work,
                &note,
                read,
                Some(revision("content-revision-2", None)),
                45
            )
        )),
        (
            EvaluationBasisMove::SourceChanged,
            "acceptance_evaluation_void".into()
        )
    );
}

#[test]
fn a_late_verification_of_an_older_producer_is_not_a_move() {
    let mut fixture = fixture("project-basis-late-verification");
    let store = &mut fixture.store;
    enable(
        store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable",
        5,
    );
    let work = fixture.work.clone();
    let mut host = HostSession::bind(store, &work, &fixture.claim, 10);
    let start = cut(store, &work);
    // A build observed at R0, then a change to R1.
    host.report(store, &[(false, Some("content-revision-0"))], 20);
    let producer = entries_after(store, &work, start, "execution_observation")[0].clone();
    host.report(store, &[(true, Some("content-revision-1"))], 30);
    // A later turn records verification evidence for that earlier build: it
    // copies the producer's R0 at a feed position after the change, but the
    // producer itself is where that revision was seen.
    host.cite_earlier_producer(store, &producer, None, 40);
    let read = cut(store, &work);
    host.report(store, &[(true, Some("content-revision-1"))], 50);
    assert_eq!(recorded_changes(store, &work, start), [false, true, false]);
    assert!(
        entries_after(store, &work, read, "work_obligation").is_empty(),
        "a repeat after a late verification opens no obligation"
    );
}

#[test]
fn a_declared_workspace_must_match_the_reported_one() {
    let mut fixture = fixture("project-basis-workspace");
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
    let read = cut(store, &work);
    host.checkpoint(store, true, None, 20);
    record(
        store,
        &judged(
            &work,
            &note,
            read,
            Some(revision("content-revision-1", Some("workspace-evaluated"))),
            25,
        ),
    )
    .expect("the declared workspace and revision match the report");
}

/// The newest evaluation on the run and why completion would refuse it, if
/// it would.
fn freshness(
    store: &SqliteStore,
    work: &WorkItem,
) -> Option<(ObjectId, Option<AcceptanceStaleReason>)> {
    store
        .acceptance_evaluation_status(work.work_id, None)
        .expect("status read")
        .map(|status| (status.evaluation, status.stale))
}

/// A judgment at `cut` under its own attempt key, so that content identical
/// to an earlier submission is submitted anew instead of replayed.
fn judged_again(
    work: &WorkItem,
    note: &ObjectId,
    cut: i64,
    source: Option<AcceptanceSourceBasis>,
    attempt: &str,
    second: i64,
) -> RecordAcceptanceEvaluationRequest {
    let mut request = judged(work, note, cut, source, second);
    request.attempt_key = Some(attempt.into());
    request
}

// B20, B31: a check that sees the source at another revision, with no
// reported change, makes the evaluation stale at completion and void at
// recording; a late verification of an earlier check stays a check.
#[test]
fn a_check_at_another_revision_without_a_reported_change_voids_the_evaluation() {
    let mut fixture = fixture("project-basis-quiet-check");
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
    let first = cut(store, &work);
    let judged_first = record(store, &judged(&work, &note, first, None, 25))
        .expect("an evaluation of the changed source records");
    assert_eq!(
        freshness(store, &work),
        Some((judged_first.evaluation.clone(), None))
    );

    // A check that sees the judged revision is only a check: resubmit.
    host.checkpoint(
        store,
        false,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        26,
    );
    let producer = entries_after(store, &work, first, "execution_observation")[0].clone();
    assert_eq!(
        freshness(store, &work),
        Some((
            judged_first.evaluation.clone(),
            Some(AcceptanceStaleReason::Mutation)
        ))
    );
    assert_eq!(
        moved(record(
            store,
            &judged_again(&work, &note, first, None, "after-the-check", 27)
        )),
        (
            EvaluationBasisMove::CheckRecorded,
            "acceptance_evaluation_resubmit".into()
        )
    );
    let second = cut(store, &work);
    let judged_second = record(store, &judged(&work, &note, second, None, 28))
        .expect("a re-read resubmission records");

    // Someone else's edit moves the source; the host reports no change, but
    // the check it observed ran at another revision. The evaluation judged
    // content that is gone: it is stale at completion and void at recording.
    host.basis.source_revision = "content-revision-2".into();
    host.checkpoint(
        store,
        false,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        30,
    );
    assert_eq!(
        recorded_changes(store, &work, second),
        [false],
        "the host reported no change"
    );
    assert_eq!(
        freshness(store, &work),
        Some((
            judged_second.evaluation.clone(),
            Some(AcceptanceStaleReason::Mutation)
        ))
    );
    assert_eq!(
        moved(record(
            store,
            &judged_again(&work, &note, second, None, "after-the-quiet-move", 31)
        )),
        (
            EvaluationBasisMove::SourceChanged,
            "acceptance_evaluation_void".into()
        )
    );
    let third = cut(store, &work);
    let judged_third = record(store, &judged(&work, &note, third, None, 35))
        .expect("a new evaluation of the current source records");
    assert_eq!(
        freshness(store, &work),
        Some((judged_third.evaluation.clone(), None))
    );

    // A late verification of the earlier check, with the environment it ran
    // in, carries that check's older revision. Both describe the check, not
    // where the source is now: a check to resubmit for, never a move.
    host.cite_earlier_producer(store, &producer, Some("content-revision-1"), 36);
    assert_eq!(
        moved(record(
            store,
            &judged_again(&work, &note, third, None, "after-the-late-check", 39)
        )),
        (
            EvaluationBasisMove::CheckRecorded,
            "acceptance_evaluation_resubmit".into()
        )
    );
    // Nor is that environment where the run was last seen: an evaluation cut
    // after it judged R2, and a quiet observation at R2 leaves it fresh.
    let fourth = cut(store, &work);
    let judged_fourth = record(store, &judged(&work, &note, fourth, None, 40))
        .expect("a re-read resubmission records");
    host.report(store, &[(false, Some("content-revision-2"))], 41);
    assert_eq!(
        freshness(store, &work),
        Some((judged_fourth.evaluation.clone(), None))
    );
}

// B20, B31: a quiet observation alone at another revision voids the
// evaluation.
#[test]
fn an_observation_alone_at_another_revision_voids_the_evaluation() {
    let mut fixture = fixture("project-basis-quiet-record");
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
    let first = cut(store, &work);
    let judged_first = record(store, &judged(&work, &note, first, None, 25))
        .expect("an evaluation of the changed source records");

    // Observations at the judged revision, or with none, move nothing.
    host.report(
        store,
        &[(false, Some("content-revision-1")), (false, None)],
        26,
    );
    assert_eq!(
        freshness(store, &work),
        Some((judged_first.evaluation.clone(), None))
    );
    // One at another revision voids it, though it claims no change.
    host.report(store, &[(false, Some("content-revision-2"))], 30);
    assert_eq!(
        freshness(store, &work),
        Some((
            judged_first.evaluation.clone(),
            Some(AcceptanceStaleReason::Mutation)
        ))
    );
    assert_eq!(
        moved(record(
            store,
            &judged_again(&work, &note, first, None, "after-the-quiet-move", 31)
        )),
        (
            EvaluationBasisMove::SourceChanged,
            "acceptance_evaluation_void".into()
        )
    );

    // An environment record describes a check, at whatever revision that
    // check ran on: it asks for a resubmission and never voids.
    let second = cut(store, &work);
    record(store, &judged(&work, &note, second, None, 35))
        .expect("an evaluation of the moved source records");
    host.capture_environment(store, "content-revision-3", 36);
    assert_eq!(
        moved(record(
            store,
            &judged_again(&work, &note, second, None, "after-the-environment", 39)
        )),
        (
            EvaluationBasisMove::CheckRecorded,
            "acceptance_evaluation_resubmit".into()
        )
    );
    // Nor does it set the revision the run was last seen at: an evaluation
    // cut after it still judged R2.
    let third = cut(store, &work);
    let judged_third = record(store, &judged(&work, &note, third, None, 40))
        .expect("a re-read resubmission records");
    host.report(store, &[(false, Some("content-revision-2"))], 41);
    assert_eq!(
        freshness(store, &work),
        Some((judged_third.evaluation.clone(), None))
    );
}

// B20, B31: a quiet sighting is compared with the declared revision,
// whatever workspace reported it.
#[test]
fn a_quiet_record_is_compared_with_the_declared_revision_in_any_workspace() {
    let mut fixture = fixture("project-basis-quiet-declared");
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
    let read = cut(store, &work);
    // The evaluator declares it judged R2, which the host has not reported
    // yet; the run was last seen at R1.
    let declared = judged_again(
        &work,
        &note,
        read,
        Some(revision("content-revision-2", Some("workspace-evaluated"))),
        "declared",
        25,
    );
    let recorded = record(store, &declared).expect("the declared evaluation records");
    host.report(store, &[(false, Some("content-revision-2"))], 26);
    assert_eq!(
        freshness(store, &work),
        Some((recorded.evaluation.clone(), None)),
        "the declared revision is what was judged"
    );
    // Without that declaration, the same record moves the basis.
    assert_eq!(
        moved(record(
            store,
            &judged_again(&work, &note, read, None, "undeclared", 29)
        )),
        (
            EvaluationBasisMove::SourceChanged,
            "acceptance_evaluation_void".into()
        )
    );
    // The revision fingerprints the full content, so a record from another
    // workspace is compared too: at the declared revision it moves nothing.
    host.basis.workspace_id = "another-workspace".into();
    host.report(store, &[(false, Some("content-revision-2"))], 30);
    assert_eq!(
        freshness(store, &work),
        Some((recorded.evaluation.clone(), None))
    );
    // At R1 it is a move, in whatever workspace, though R1 is the revision
    // the run was last seen at when the basis was cut.
    host.report(store, &[(false, Some("content-revision-1"))], 34);
    assert_eq!(
        freshness(store, &work),
        Some((
            recorded.evaluation.clone(),
            Some(AcceptanceStaleReason::Mutation)
        ))
    );
}

// B20: with no declared revision, a quiet sighting is compared with the
// revision the run was last seen at when the cut was taken.
#[test]
fn a_quiet_record_is_compared_with_the_revision_last_seen_at_the_cut() {
    let mut fixture = fixture("project-basis-quiet-last-seen");
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
    // With no revision seen at the cut and none declared, there is nothing
    // to compare a quiet record with.
    let blind = cut(store, &work);
    let unanchored = record(store, &judged(&work, &note, blind, None, 15))
        .expect("an evaluation before any revision records");
    host.report(store, &[(false, Some("content-revision-1"))], 16);
    assert_eq!(
        freshness(store, &work),
        Some((unanchored.evaluation.clone(), None))
    );

    // A change to R1, then a quiet move to R2 before the cut: the evaluation
    // judged R2, the revision the run was last seen at, not the newest
    // reported change.
    host.report(store, &[(true, Some("content-revision-1"))], 20);
    host.report(store, &[(false, Some("content-revision-2"))], 24);
    let read = cut(store, &work);
    let recorded = record(
        store,
        &judged_again(&work, &note, read, None, "after-the-quiet-move", 28),
    )
    .expect("the evaluation records");
    host.report(store, &[(false, Some("content-revision-2"))], 30);
    assert_eq!(
        freshness(store, &work),
        Some((recorded.evaluation.clone(), None))
    );
    host.report(store, &[(false, Some("content-revision-1"))], 34);
    assert_eq!(
        freshness(store, &work),
        Some((
            recorded.evaluation.clone(),
            Some(AcceptanceStaleReason::Mutation)
        ))
    );
}

// B20, B31: a late report of a change to the declared revision re-anchors
// the source, so earlier sightings of another revision no longer count, and
// the same judgment still records at its cut.
#[test]
fn a_late_report_ending_at_the_declared_revision_re_anchors_earlier_sightings() {
    let mut fixture = fixture("project-basis-quiet-re-anchor");
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
    host.report(store, &[(true, Some("content-revision-0"))], 20);
    // Mid-turn, after a check at R0 and an edit to R1, the evaluator judges
    // R1 and declares it; the host reports that turn only when it ends.
    let read = cut(store, &work);
    let recorded = record(
        store,
        &judged_again(
            &work,
            &note,
            read,
            Some(revision("content-revision-1", None)),
            "declared",
            25,
        ),
    )
    .expect("the declared evaluation records");
    // The host lists the turn in the order it saw it: the check at R0, then
    // the change to R1. The earlier sighting is older than that change.
    host.report(
        store,
        &[
            (false, Some("content-revision-0")),
            (true, Some("content-revision-1")),
        ],
        26,
    );
    assert_eq!(
        freshness(store, &work),
        Some((recorded.evaluation.clone(), None))
    );
    let resubmitted = record(
        store,
        &judged_again(
            &work,
            &note,
            read,
            Some(revision("content-revision-1", None)),
            "declared-after-the-report",
            29,
        ),
    )
    .expect("the same judgment still records at its cut");
    // A sighting of R0 after that change is a move back.
    host.report(store, &[(false, Some("content-revision-0"))], 30);
    assert_eq!(
        freshness(store, &work),
        Some((
            resubmitted.evaluation.clone(),
            Some(AcceptanceStaleReason::Mutation)
        ))
    );
}

// B20: the newest sighting that carries a revision decides where the source
// is.
#[test]
fn the_newest_sighting_decides_where_the_source_is() {
    let mut fixture = fixture("project-basis-quiet-newest");
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
    host.report(store, &[(true, Some("content-revision-0"))], 20);
    // Someone else's edit to R1, which the host does not flag, and the
    // evaluator judges R1 and declares it.
    let read = cut(store, &work);
    let declared = record(
        store,
        &judged_again(
            &work,
            &note,
            read,
            Some(revision("content-revision-1", None)),
            "declared",
            25,
        ),
    )
    .expect("the declared evaluation records");
    // The turn lists a check before the edit and one after it: the source
    // ended where it was judged.
    host.report(
        store,
        &[
            (false, Some("content-revision-0")),
            (false, Some("content-revision-1")),
            (false, None),
        ],
        26,
    );
    assert_eq!(
        freshness(store, &work),
        Some((declared.evaluation.clone(), None))
    );
    host.report(store, &[(false, Some("content-revision-2"))], 30);
    assert_eq!(
        freshness(store, &work),
        Some((
            declared.evaluation.clone(),
            Some(AcceptanceStaleReason::Mutation)
        ))
    );

    // Undeclared, the source moved away and back to the revision last seen
    // at the cut: the newest sighting is where it was judged.
    let second = cut(store, &work);
    let undeclared = record(
        store,
        &judged_again(&work, &note, second, None, "undeclared", 35),
    )
    .expect("an evaluation of R2 records");
    host.report(
        store,
        &[
            (false, Some("content-revision-3")),
            (false, Some("content-revision-2")),
        ],
        36,
    );
    assert_eq!(
        freshness(store, &work),
        Some((undeclared.evaluation.clone(), None))
    );
    host.report(store, &[(false, Some("content-revision-3"))], 40);
    assert_eq!(
        freshness(store, &work),
        Some((
            undeclared.evaluation.clone(),
            Some(AcceptanceStaleReason::Mutation)
        ))
    );
}

/// One mutation turn that reports `reports` in order: whether the host
/// claims a change, the revision it reports, and how it says it established
/// the change.
fn report_detected(
    host: &mut HostSession,
    store: &mut SqliteStore,
    reports: &[(bool, Option<&str>, Option<SourceChangeDetection>)],
    second: i64,
) -> Result<ControlTurnCheckpointDecision, StoreError> {
    let grant = host.grant(store, &[EffectClass::MutateLocal], true, second);
    host.begin(store, &grant, second + 1);
    let observations: Vec<ExecutionObservationInput> = reports
        .iter()
        .enumerate()
        .map(
            |(index, (source_changed, revision, detection))| ExecutionObservationInput {
                observation_id: host.key(&format!("detected-{index}")),
                action_fingerprint: ObjectId::from_canonical_bytes(
                    host.key(&format!("detected action {index}")).as_bytes(),
                ),
                effect: EffectClass::MutateLocal,
                outcome: ExecutionOutcome::Succeeded,
                source_changed: *source_changed,
                reported_source_change: *detection,
                source_basis: revision.map(|revision| ExecutionSourceBasis {
                    workspace_id: host.basis.workspace_id.clone(),
                    source_revision: revision.into(),
                    source_root_generation: None,
                    source_root_state: None,
                }),
                observed_at: revision.map(|_| at(second + 1)),
            },
        )
        .collect();
    store.checkpoint_control_turn_with_evidence(
        &host.project_id,
        &host.session_id,
        &host.connection_token,
        &host.routing_token,
        &grant.grant_id,
        TurnNextIntent::Continue,
        &observations,
        &[],
        &[],
        &host.key("checkpoint"),
        at(second + 2),
    )
}

fn observations_after(
    store: &SqliteStore,
    work: &WorkItem,
    read: i64,
) -> Vec<ExecutionObservation> {
    entries_after(store, work, read, "execution_observation")
        .iter()
        .map(|entry| {
            load_typed_work_object(&store.connection, entry, "execution_observation")
                .expect("stored observation")
        })
        .collect()
}

#[test]
fn how_the_host_established_a_change_is_stored_as_said_and_changes_no_obligation() {
    let mut fixture = fixture("project-detection-values");
    let store = &mut fixture.store;
    let work = fixture.work.clone();
    let mut host = HostSession::bind(store, &work, &fixture.claim, 10);
    let said = [
        (
            Some("revision-a"),
            Some(SourceChangeDetection::ContentComparison),
        ),
        (
            Some("revision-b"),
            Some(SourceChangeDetection::AssumedMissingBaseline),
        ),
        (None, Some(SourceChangeDetection::WatcherOnly)),
        (Some("revision-d"), None),
    ];
    for (index, (revision, detection)) in said.iter().enumerate() {
        let second = 20 + 10 * i64::try_from(index).expect("small index");
        let read = cut(store, &work);
        let decision = report_detected(&mut host, store, &[(true, *revision, *detection)], second)
            .expect("a consistent report checkpoints");
        assert!(matches!(
            decision,
            ControlTurnCheckpointDecision::Checkpointed { .. }
        ));
        let stored = observations_after(store, &work, read);
        assert_eq!(stored.len(), 1);
        assert_eq!(stored[0].reported_source_change, *detection);
        assert!(stored[0].source_changed, "every way of knowing is a change");
        assert_eq!(
            entries_after(store, &work, read, "work_obligation").len(),
            1,
            "{detection:?} opens the obligation a change opens"
        );
        // The stored record names the value as the protocol spells it, and
        // leaves the field out when the host did not say.
        let value = serde_json::to_value(&stored[0]).expect("observation as JSON");
        assert_eq!(
            value
                .get("reported_source_change")
                .and_then(|value| value.as_str()),
            detection.map(SourceChangeDetection::as_str)
        );
    }
}

#[test]
fn a_repeated_revision_keeps_what_the_host_said_and_is_read_as_no_change() {
    let mut fixture = fixture("project-detection-repeated");
    let store = &mut fixture.store;
    let work = fixture.work.clone();
    let mut host = HostSession::bind(store, &work, &fixture.claim, 10);
    report_detected(
        &mut host,
        store,
        &[(
            true,
            Some("revision-a"),
            Some(SourceChangeDetection::ContentComparison),
        )],
        20,
    )
    .expect("first change");
    let read = cut(store, &work);
    report_detected(
        &mut host,
        store,
        &[(
            true,
            Some("revision-a"),
            Some(SourceChangeDetection::AssumedMissingBaseline),
        )],
        30,
    )
    .expect("the same revision again");
    let stored = observations_after(store, &work, read);
    assert_eq!(stored.len(), 1);
    assert!(!stored[0].source_changed);
    assert_eq!(
        stored[0].reported_source_change,
        Some(SourceChangeDetection::AssumedMissingBaseline)
    );
    assert!(entries_after(store, &work, read, "work_obligation").is_empty());
}

#[test]
fn a_report_that_contradicts_itself_is_refused_and_stores_nothing() {
    let contradictions = [
        (
            false,
            Some("revision-a"),
            SourceChangeDetection::ContentComparison,
            "reports no change",
        ),
        (
            false,
            None,
            SourceChangeDetection::WatcherOnly,
            "reports no change",
        ),
        (
            true,
            None,
            SourceChangeDetection::ContentComparison,
            "without the source basis",
        ),
        (
            true,
            None,
            SourceChangeDetection::AssumedMissingBaseline,
            "without the source basis",
        ),
        (
            true,
            Some("revision-a"),
            SourceChangeDetection::WatcherOnly,
            "with a source basis",
        ),
    ];
    for (index, (changed, revision, detection, why)) in contradictions.iter().enumerate() {
        // A refused checkpoint leaves its turn open, so each case has a
        // store of its own.
        let mut fixture = fixture(&format!("project-detection-refusal-{index}"));
        let store = &mut fixture.store;
        let work = fixture.work.clone();
        let mut host = HostSession::bind(store, &work, &fixture.claim, 10);
        let second = 20;
        let read = cut(store, &work);
        // A consistent report in the same checkpoint is refused with it.
        let refused = report_detected(
            &mut host,
            store,
            &[
                (
                    true,
                    Some("revision-good"),
                    Some(SourceChangeDetection::ContentComparison),
                ),
                (*changed, *revision, Some(*detection)),
            ],
            second,
        );
        let Err(StoreError::InvalidControlProjection(message)) = refused else {
            panic!("{detection:?} with changed={changed} must be refused: {refused:?}");
        };
        assert!(message.contains(why), "{message}");
        assert!(message.contains(detection.as_str()), "{message}");
        assert!(observations_after(store, &work, read).is_empty());
        assert!(entries_after(store, &work, read, "work_obligation").is_empty());
    }
}

#[test]
fn an_observation_without_the_field_reads_as_stored_and_an_unknown_value_is_refused_by_name() {
    let mut fixture = fixture("project-detection-old-bytes");
    let store = &mut fixture.store;
    let work = fixture.work.clone();
    let mut host = HostSession::bind(store, &work, &fixture.claim, 10);
    let read = cut(store, &work);
    report_detected(&mut host, store, &[(true, Some("revision-a"), None)], 20)
        .expect("a host that does not say");
    let entry = entries_after(store, &work, read, "execution_observation");
    let stored = observations_after(store, &work, read);
    let bytes: Vec<u8> = store
        .connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_id = ?1",
            [entry[0].as_str()],
            |row| row.get(0),
        )
        .expect("the stored canonical bytes");
    let without: serde_json::Value = serde_json::from_slice(&bytes).expect("stored bytes are JSON");
    assert!(without.get("reported_source_change").is_none());
    // Those are the bytes every earlier observation has: they decode to the
    // record and freeze back to the same bytes. The stored id is minted, not
    // derived from the bytes, so it is not compared.
    let decoded: ExecutionObservation =
        serde_json::from_slice(&bytes).expect("a record without the field decodes");
    assert_eq!(decoded, stored[0]);
    let frozen = crate::CanonicalObject::freeze(&decoded).expect("freeze the decoded record");
    assert_eq!(frozen.bytes(), bytes.as_slice());

    let mut input = serde_json::to_value(ExecutionObservationInput {
        observation_id: "unknown-value".into(),
        action_fingerprint: ObjectId::from_canonical_bytes(b"unknown value"),
        effect: EffectClass::MutateLocal,
        outcome: ExecutionOutcome::Succeeded,
        source_changed: true,
        reported_source_change: Some(SourceChangeDetection::WatcherOnly),
        source_basis: None,
        observed_at: None,
    })
    .expect("input as JSON");
    input["reported_source_change"] = "measured_between_turns".into();
    let refused = serde_json::from_value::<ExecutionObservationInput>(input)
        .expect_err("a value the protocol does not name");
    let message = refused.to_string();
    assert!(message.contains("measured_between_turns"), "{message}");
    for known in [
        SourceChangeDetection::ContentComparison,
        SourceChangeDetection::AssumedMissingBaseline,
        SourceChangeDetection::WatcherOnly,
    ] {
        assert!(message.contains(known.as_str()), "{message}");
    }
}
