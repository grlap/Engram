//! A record without a session its mode requires cannot complete work. Such a
//! record is never admitted, so each case writes an admitted record and then
//! rewrites its stored bytes under the same id, as an import or an edit
//! could.

use super::review::{handoff, pass_judgment};
use super::*;

/// Rewrites the stored evaluation through `edit`, keeping its id, so the run
/// feed still names it.
fn malform(
    store: &SqliteStore,
    evaluation: &ObjectId,
    edit: impl FnOnce(&mut AcceptanceEvaluation),
) {
    let mut stored: AcceptanceEvaluation = store
        .get(evaluation)
        .expect("read the stored evaluation")
        .expect("canonical evaluation");
    edit(&mut stored);
    let object = CanonicalObject::identified(evaluation, &stored).expect("rewritten evaluation");
    let changed = store
        .connection
        .execute(
            "UPDATE objects SET canonical_json = ?2 WHERE object_id = ?1",
            params![evaluation.as_str(), object.bytes()],
        )
        .expect("rewrite the stored evaluation");
    assert_eq!(changed, 1);
}

/// The newest record status reads, and why it is stale.
fn newest(store: &SqliteStore, work: &WorkItem) -> (ObjectId, Option<AcceptanceStaleReason>) {
    let status = store
        .acceptance_evaluation_status(work.work_id, None)
        .expect("status read")
        .expect("newest evaluation");
    (status.evaluation, status.stale)
}

/// `done` refuses with stale `Identity`, and the item stays open.
fn refused_for_identity(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    note: &ObjectId,
    key: &str,
    second: i64,
) {
    let cause = recovery_cause(complete_evaluated(
        store,
        work,
        claim,
        &claim.holder.0,
        note,
        None,
        key,
        second,
    ));
    assert!(
        matches!(
            cause,
            WorkCompletionRecoveryCause::AcceptanceEvaluationStale {
                reason: AcceptanceStaleReason::Identity
            }
        ),
        "{cause:?}"
    );
    let after = store
        .get_work_item(work.work_id)
        .expect("item after refusal");
    assert_eq!(after.lifecycle, WorkLifecycle::Open);
}

/// A task an outside planner marked for `same_session`, claimed by `runner`
/// with one evidence note, under a policy that admits only that mode.
fn marked_same_session(project: &str) -> (Fixture, WorkItem, WorkClaim, ObjectId) {
    let mut fx = fixture(project);
    enable(
        &mut fx.store,
        &[Mode::SameSession],
        MechanicalBasis::Asserted,
        false,
        "enable-same-session",
        5,
    );
    let mut create = root_request(project, "create-marked", 6);
    create.evaluation_mode = Some(Mode::SameSession);
    let work = fx
        .store
        .create_work(&create, &DevelopmentNoopRedactor)
        .expect("outside-authored mark");
    let held = claim(&mut fx.store, &work, "runner", "claim-marked", 7, 3_600);
    let note = evidence(&mut fx.store, &work, &held, "runner", "marked-evidence", 8);
    (fx, work, held, note)
}

#[test]
fn a_same_session_record_without_its_evaluator_cannot_complete() {
    let (mut fx, work, held, note) = marked_same_session("project-shape-same-session");
    let store = &mut fx.store;
    // An older valid pass, then the newest one, which loses its evaluator.
    let older = record(
        store,
        &RecordAcceptanceEvaluationRequest {
            attempt_key: Some("older-pass".into()),
            ..request(
                &work,
                cut(store, &work),
                "runner",
                Mode::SameSession,
                pass_judgment(&note),
                9,
            )
        },
    )
    .expect("older valid pass");
    let newer = record(
        store,
        &RecordAcceptanceEvaluationRequest {
            attempt_key: Some("newer-pass".into()),
            ..request(
                &work,
                cut(store, &work),
                "runner",
                Mode::SameSession,
                pass_judgment(&note),
                10,
            )
        },
    )
    .expect("newer valid pass");
    assert_eq!(newest(store, &work), (newer.evaluation.clone(), None));
    malform(store, &newer.evaluation, |record| {
        record.evaluator.session_id = None;
    });
    // Status reads the newest record stale; the older pass does not stand in.
    assert_eq!(
        newest(store, &work),
        (
            newer.evaluation.clone(),
            Some(AcceptanceStaleReason::Identity)
        )
    );
    assert_ne!(older.evaluation, newer.evaluation);
    refused_for_identity(store, &work, &held, &note, "complete-without-evaluator", 11);
    // A fresh admitted evaluation completes the item.
    let fresh = record(
        store,
        &RecordAcceptanceEvaluationRequest {
            attempt_key: Some("fresh-pass".into()),
            ..request(
                &work,
                cut(store, &work),
                "runner",
                Mode::SameSession,
                pass_judgment(&note),
                13,
            )
        },
    )
    .expect("fresh valid pass");
    let seal = complete_evaluated(
        store,
        &work,
        &held,
        "runner",
        &note,
        None,
        "complete-fresh",
        14,
    )
    .expect("the fresh pass completes the item");
    assert_eq!(seal.acceptance_evaluation, Some(fresh.evaluation));
}

#[test]
fn a_sub_agent_record_without_its_parent_or_evaluator_cannot_complete() {
    type Edit = fn(&mut AcceptanceEvaluation);
    let shapes: [(&str, Edit); 3] = [
        ("parent", |record| record.parent_session = None),
        ("evaluator", |record| record.evaluator.session_id = None),
        ("both", |record| {
            record.parent_session = None;
            record.evaluator.session_id = None;
        }),
    ];
    for (missing, edit) in shapes {
        let mut fx = fixture(&format!("project-shape-child-{missing}"));
        let store = &mut fx.store;
        enable(
            store,
            &[Mode::SubAgent, Mode::IndependentSession],
            MechanicalBasis::Asserted,
            false,
            "enable-child",
            5,
        );
        let work = fx.work.clone();
        let runner = fx.claim.clone();
        let note = fx.evidence.clone();
        let child =
            |store: &SqliteStore, attempt: &str, second| RecordAcceptanceEvaluationRequest {
                execution_identity: Some("child-execution".into()),
                parent_session: Some(runner.holder.clone()),
                attempt_key: Some(attempt.into()),
                ..request(
                    &work,
                    cut(store, &work),
                    "child",
                    Mode::SubAgent,
                    pass_judgment(&note),
                    second,
                )
            };
        let admitted = record(store, &child(store, "child-pass", 6)).expect("admitted child pass");
        assert_eq!(
            newest(store, &work),
            (admitted.evaluation.clone(), None),
            "{missing}"
        );
        malform(store, &admitted.evaluation, edit);
        assert_eq!(
            newest(store, &work),
            (
                admitted.evaluation.clone(),
                Some(AcceptanceStaleReason::Identity)
            ),
            "{missing}"
        );
        refused_for_identity(store, &work, &runner, &note, "complete-malformed-child", 7);
        let fresh = record(store, &child(store, "fresh-child-pass", 9)).expect("fresh child pass");
        let seal = complete_evaluated(
            store,
            &work,
            &runner,
            "runner",
            &note,
            None,
            "complete-fresh",
            10,
        )
        .expect("the fresh child pass completes the item");
        assert_eq!(
            seal.acceptance_evaluation,
            Some(fresh.evaluation),
            "{missing}"
        );
    }
}

#[test]
fn an_independent_record_without_its_evaluator_cannot_complete() {
    let mut fx = fixture("project-shape-independent");
    let store = &mut fx.store;
    enable(
        store,
        &[Mode::IndependentSession],
        MechanicalBasis::Asserted,
        false,
        "enable-independent",
        5,
    );
    let work = fx.work.clone();
    let runner = fx.claim.clone();
    let note = fx.evidence.clone();
    let admitted = record(
        store,
        &request(
            &work,
            cut(store, &work),
            "judge",
            Mode::IndependentSession,
            pass_judgment(&note),
            6,
        ),
    )
    .expect("independent pass");
    malform(store, &admitted.evaluation, |record| {
        record.evaluator.session_id = None;
    });
    assert_eq!(
        newest(store, &work),
        (admitted.evaluation, Some(AcceptanceStaleReason::Identity))
    );
    refused_for_identity(store, &work, &runner, &note, "complete-without-judge", 7);
}

// The check asks only for presence: a same-session evaluator need not still
// execute the run when its record is consumed.
#[test]
fn a_same_session_pass_outlives_its_evaluator_handing_off_the_run() {
    let (mut fx, work, held, note) = marked_same_session("project-shape-same-session-handoff");
    let store = &mut fx.store;
    let admitted = record(
        store,
        &request(
            &work,
            cut(store, &work),
            "runner",
            Mode::SameSession,
            pass_judgment(&note),
            9,
        ),
    )
    .expect("same-session pass");
    let second = handoff(store, &work, &held, "second", 10);
    assert_eq!(newest(store, &work), (admitted.evaluation.clone(), None));
    let seal = complete_evaluated(
        store,
        &work,
        &second,
        "second",
        &note,
        None,
        "complete-after-handoff",
        12,
    )
    .expect("the new holder consumes the pass");
    assert_eq!(seal.acceptance_evaluation, Some(admitted.evaluation));
}

// Earlier reasons keep their precedence over a missing session, and the
// missing session precedes the later ones. Assessment inputs, as in the
// policy matrix; nothing here is written to the store.
#[test]
fn a_missing_session_reads_after_earlier_reasons_and_before_later_ones() {
    let (fx, work, held, recorded) = {
        let (mut fx, work, held, note) = marked_same_session("project-shape-precedence");
        let through = cut(&fx.store, &work);
        let recorded = record(
            &mut fx.store,
            &request(
                &work,
                through,
                "runner",
                Mode::SameSession,
                pass_judgment(&note),
                9,
            ),
        )
        .expect("same-session pass");
        (fx, work, held, recorded)
    };
    let assess = |item: &WorkItem,
                  run: WorkRunId,
                  policy: &AcceptanceEvaluationPolicy,
                  record: &AcceptanceEvaluation,
                  source| {
        staleness_named(
            &fx.store.connection,
            item,
            run,
            policy,
            &recorded.evaluation,
            record,
            source,
        )
        .expect("assessment")
        .0
    };
    let admitted = policy(&[Mode::SameSession], MechanicalBasis::Asserted, false);
    let mut missing = recorded.record.clone();
    missing.evaluator.session_id = None;
    assert_eq!(
        assess(
            &work,
            held.run_id,
            &admitted,
            &recorded.record,
            SourceCheck::Unmeasured
        ),
        None
    );
    assert_eq!(
        assess(
            &work,
            held.run_id,
            &admitted,
            &missing,
            SourceCheck::Unmeasured
        ),
        Some(AcceptanceStaleReason::Identity)
    );
    // Earlier: another run, another revision, a mode the policy no longer
    // admits, a same-session record on a task no one marked.
    assert_eq!(
        assess(
            &work,
            fx.claim.run_id,
            &admitted,
            &missing,
            SourceCheck::Unmeasured
        ),
        Some(AcceptanceStaleReason::Run)
    );
    let mut revised = work.clone();
    revised.revision += 1;
    assert_eq!(
        assess(
            &revised,
            held.run_id,
            &admitted,
            &missing,
            SourceCheck::Unmeasured
        ),
        Some(AcceptanceStaleReason::Revision)
    );
    let independent_only = policy(
        &[Mode::IndependentSession],
        MechanicalBasis::Asserted,
        false,
    );
    assert_eq!(
        assess(
            &work,
            held.run_id,
            &independent_only,
            &missing,
            SourceCheck::Unmeasured
        ),
        Some(AcceptanceStaleReason::Policy)
    );
    let mut unmarked = work.clone();
    unmarked.evaluation_mode = None;
    let mut unmarked_missing = missing.clone();
    unmarked_missing.work_revision_hash = CanonicalObject::freeze(&unmarked)
        .expect("unmarked revision")
        .key()
        .clone();
    let both = policy(
        &[Mode::SameSession, Mode::IndependentSession],
        MechanicalBasis::Asserted,
        false,
    );
    assert_eq!(
        assess(
            &unmarked,
            held.run_id,
            &both,
            &unmarked_missing,
            SourceCheck::Unmeasured
        ),
        Some(AcceptanceStaleReason::Policy)
    );
    // Later: a source fingerprint that does not match.
    let fresh_source = policy(&[Mode::SameSession], MechanicalBasis::Asserted, true);
    let presented = SourceCheck::AtCompletion(Some("another-revision"));
    assert_eq!(
        assess(
            &work,
            held.run_id,
            &fresh_source,
            &recorded.record,
            presented
        ),
        Some(AcceptanceStaleReason::Source)
    );
    assert_eq!(
        assess(
            &work,
            held.run_id,
            &fresh_source,
            &missing,
            SourceCheck::AtCompletion(Some("another-revision"))
        ),
        Some(AcceptanceStaleReason::Identity)
    );
}
