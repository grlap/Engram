//! Qualified retry and evaluation share the same writer-selection boundary.

use super::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

fn connections(data: &mut Selected) -> (crate::test_support::TempHome, SqliteStore) {
    let home = crate::test_support::temp_home().unwrap();
    let database = home.path().join("selection-controls.sqlite3");
    data.target
        .store
        .connection
        .execute("VACUUM INTO ?1", [database.to_str().unwrap()])
        .unwrap();
    data.target.store = SqliteStore::open(&database).unwrap();
    (home, SqliteStore::open(&database).unwrap())
}

fn ordinary_completion(data: &mut Selected) -> (CompleteWorkRequest, ObjectId) {
    let item = data
        .target
        .store
        .get_work_item(data.target.binding.work_id)
        .unwrap();
    let held = data.target.store.claim_fixture(item.work_id);
    let note = data
        .target
        .store
        .record_work_note(
            &RecordWorkNoteRequest {
                status: false,
                work_id: item.work_id,
                run_id: held.run_id,
                expected_work_revision: item.revision,
                holder: held.holder,
                claim_id: held.claim_id,
                claim_fence: held.fence,
                summary: "ordinary evidence for selected-use retry".into(),
                refs: vec![],
                actor: actor("runner"),
                idempotency_key: "selection-control-note".into(),
                recorded_at: at(24),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    (
        completion(&mut data.target, data.root.path(), &note.evidence, false),
        note.evidence,
    )
}

fn conflict(data: &mut Selected) -> EvidencePack {
    data.origin.request.operation_key = "selection-control-conflict".into();
    data.origin.request.completion.result.verdict = RemoteRunVerdict::Fail;
    let terminal = data.origin.record(29);
    pack(
        &data.acceptance.origin,
        vec![envelope(
            &data.origin.store,
            &data.acceptance.origin,
            &terminal.operation,
        )],
    )
}

#[test]
fn completion_reselects_imported_consume_only_then_retry_and_exact_replay() {
    let mut data = selected(false);
    let (input, note) = ordinary_completion(&mut data);
    let (_home, mut second) = connections(&mut data);
    let consume = ConsumptionRequest {
        consumption: ObjectId::mint(),
        decision: data.verification.decision.clone(),
        binding: data.target.binding.clone(),
    };
    let local_actor = data.target.local_actor();
    let post_second = Rc::new(RefCell::new(None));
    let snapshot = post_second.clone();
    let error = crate::storage::with_writer_admission_test_policy(
        None,
        move || {
            second
                .consume_imported_verification(
                    &project(),
                    &session(),
                    &local_actor,
                    &consume,
                    at(27),
                )
                .unwrap();
            let item = second.get_work_item(consume.binding.work_id).unwrap();
            let held = second.claim_fixture(item.work_id);
            checkpoint(
                &mut second,
                &item,
                &held,
                "runner",
                "selection-control-checkpoint",
                27,
                std::slice::from_ref(&note),
            );
            *snapshot.borrow_mut() =
                Some(crate::storage::test_database_shape_snapshot(&second.connection).unwrap());
        },
        || {
            data.target
                .store
                .complete_work(&input, &DevelopmentNoopRedactor)
        },
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("selected imported uses changed"),
        "{error}"
    );
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&data.target.store.connection).unwrap(),
        post_second.borrow_mut().take().unwrap()
    );
    let seal = data
        .target
        .store
        .complete_work(&input, &DevelopmentNoopRedactor)
        .unwrap();
    assert_eq!(count(&data.target.store, "completion_seal"), 1);
    let later = conflict(&mut data);
    retain(&mut data.target.store, &later);
    let before =
        crate::storage::test_database_shape_snapshot(&data.target.store.connection).unwrap();
    let replay = data
        .target
        .store
        .complete_work(&input, &DevelopmentNoopRedactor)
        .unwrap();
    assert_eq!(replay, seal);
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&data.target.store.connection).unwrap(),
        before
    );
    healthy(&data.target.store);
    drop(data.target.store);
}

#[test]
fn evaluation_reselects_imported_citation_added_before_writer() {
    let mut data = selected(false);
    enable(&mut data.target.store);
    let before = crate::storage::work::feeds::current_run_feed_cut_on(
        &data.target.store.connection,
        data.target.binding.run_id,
    )
    .unwrap();
    let consume = ConsumptionRequest {
        consumption: ObjectId::mint(),
        decision: data.verification.decision.clone(),
        binding: data.target.binding.clone(),
    };
    // This fixture's typed consumption publishes its record, work event, and
    // one bound-obligation resolution. Assert the real cut in the callback so
    // an invalid future citation cut cannot masquerade as selection refusal.
    let evaluated_through = before.position + 3;
    let input = RecordAcceptanceEvaluationRequest {
        project_id: project(),
        work_id: data.target.binding.work_id,
        expected_work_revision: data.target.binding.work_revision,
        evaluated_through,
        mode: AcceptanceEvaluationMode::IndependentSession,
        execution_identity: None,
        parent_session: None,
        evaluator_model: None,
        source_basis: Some(AcceptanceSourceBasis {
            workspace_id: None,
            identity: VerificationSourceIdentity::SourceId {
                source_id: source(data.root.path()),
            },
        }),
        verdicts: vec![CriterionVerdictInput {
            criterion: 1,
            verdict: AcceptanceVerdict::Pass,
            basis: AcceptanceBasis::Observed,
            rationale: "selected consumption at the admitted cut".into(),
            evidence: vec![consume.consumption.clone()],
        }],
        evaluator: actor("reviewer"),
        attempt_key: Some("selection-control-evaluation".into()),
        recorded_at: at(29),
        supersedes: None,
    };
    let (_home, mut second) = connections(&mut data);
    let local_actor = data.target.local_actor();
    let fired = Rc::new(Cell::new(false));
    let called = fired.clone();
    let post_second = Rc::new(RefCell::new(None));
    let snapshot = post_second.clone();
    let error = crate::storage::with_writer_admission_test_policy(
        None,
        move || {
            assert!(!called.replace(true));
            second
                .consume_imported_verification(
                    &project(),
                    &session(),
                    &local_actor,
                    &consume,
                    at(27),
                )
                .unwrap();
            assert_eq!(
                crate::storage::work::feeds::current_run_feed_cut_on(
                    &second.connection,
                    consume.binding.run_id
                )
                .unwrap()
                .position,
                evaluated_through
            );
            *snapshot.borrow_mut() =
                Some(crate::storage::test_database_shape_snapshot(&second.connection).unwrap());
        },
        || {
            data.target.store.record_acceptance_evaluation_registering(
                &input,
                &DevelopmentNoopRedactor,
                Some((&SessionId("reviewer".into()), at(28))),
            )
        },
    )
    .unwrap_err();
    assert!(fired.get());
    assert!(
        error.to_string().contains("selected imported uses changed"),
        "{error}"
    );
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&data.target.store.connection).unwrap(),
        post_second.borrow_mut().take().unwrap()
    );
    let receipt = data
        .target
        .store
        .record_acceptance_evaluation_registering(
            &input,
            &DevelopmentNoopRedactor,
            Some((&SessionId("reviewer".into()), at(28))),
        )
        .unwrap();
    let before =
        crate::storage::test_database_shape_snapshot(&data.target.store.connection).unwrap();
    let replay = data
        .target
        .store
        .record_acceptance_evaluation_registering(
            &input,
            &DevelopmentNoopRedactor,
            Some((&SessionId("reviewer".into()), at(28))),
        )
        .unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.evaluation, receipt.evaluation);
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&data.target.store.connection).unwrap(),
        before
    );
    healthy(&data.target.store);
    drop(data.target.store);
}
