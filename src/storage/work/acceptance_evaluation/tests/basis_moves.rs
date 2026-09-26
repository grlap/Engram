//! A run that moved past the evidence basis an evaluation read through: a
//! host check asks for a resubmission, a source change the evaluation did not
//! judge voids it, and a change to the revision it declared it judged does not
//! count, at recording or at completion.

use super::*;
use crate::EvaluationBasisMove;

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
