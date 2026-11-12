//! Selection must include proof consumed by another connection before admission.

use super::*;
use std::cell::{Cell, RefCell};
use std::rc::Rc;

#[test]
fn completion_reselects_imported_satisfaction_added_before_writer_with_conflicting_terminal() {
    let mut data = selected(false);
    assert!(
        !data
            .target
            .store
            .acceptance_evaluation_policy()
            .unwrap()
            .is_evaluated()
    );
    assert!(
        data.target
            .store
            .work_run_obligations(data.target.binding.run_id)
            .unwrap()
            .iter()
            .any(|row| row.state == WorkObligationState::Open)
    );
    let item = data
        .target
        .store
        .get_work_item(data.target.binding.work_id)
        .unwrap();
    let held = data
        .target
        .store
        .current_work_claim(item.work_id)
        .unwrap()
        .unwrap();
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
                summary: "ordinary completion evidence before the competing consumption".into(),
                refs: vec![],
                actor: actor("runner"),
                idempotency_key: "writer-selection-note".into(),
                recorded_at: at(24),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let input = completion(&mut data.target, data.root.path(), &note.evidence, false);
    let database_home = crate::test_support::temp_home().unwrap();
    let database = database_home.path().join("writer-selection.sqlite3");
    data.target
        .store
        .connection
        .execute("VACUUM INTO ?1", [database.to_str().unwrap()])
        .unwrap();
    let mut first = SqliteStore::open(&database).unwrap();
    let mut second = SqliteStore::open(&database).unwrap();

    data.origin.request.operation_key = "writer-selection-conflict".into();
    data.origin.request.completion.result.verdict = RemoteRunVerdict::Fail;
    let terminal = data.origin.record(24);
    let conflict = pack(
        &data.acceptance.origin,
        vec![envelope(
            &data.origin.store,
            &data.acceptance.origin,
            &terminal.operation,
        )],
    );
    let key = crate::task_sync::evidence::verification::ExecutionKey::new(
        project(),
        &data.origin.request.completion,
    );
    let use_request = ConsumptionRequest {
        consumption: ObjectId::mint(),
        decision: data.verification.decision.clone(),
        binding: data.target.binding.clone(),
    };
    let local_actor = data.target.local_actor();
    let run = data.target.binding.run_id;
    let checkpoint_evidence = note.evidence;
    let fired = Rc::new(Cell::new(false));
    let post_second = Rc::new(RefCell::new(None));
    let called = fired.clone();
    let captured = post_second.clone();
    let result = crate::storage::with_writer_admission_test_policy(
        None,
        move || {
            assert!(
                !called.replace(true),
                "one deterministic writer interleaving"
            );
            // Ordinary typed mutations on a separate connection commit after
            // completion measured an empty imported set, before its writer.
            second
                .consume_imported_verification(
                    &project(),
                    &session(),
                    &local_actor,
                    &use_request,
                    at(27),
                )
                .unwrap();
            assert!(
                second
                    .work_run_obligations(run)
                    .unwrap()
                    .iter()
                    .all(|row| row.state == WorkObligationState::Satisfied)
            );
            retain(&mut second, &conflict);
            assert!(
                crate::storage::work::task_sync::evidence::verification::union_on(
                    &second.connection,
                    &key,
                )
                .unwrap()
                .conflicted
            );
            // The completing writer must still have a valid final checkpoint.
            // Use the same claim's ordinary API, rather than bypassing that
            // independent refusal with a projection edit or a stale cut.
            let current = second.get_work_item(use_request.binding.work_id).unwrap();
            let claim = second.claim_fixture(current.work_id);
            checkpoint(
                &mut second,
                &current,
                &claim,
                "runner",
                "writer-selection-competing-checkpoint",
                27,
                std::slice::from_ref(&checkpoint_evidence),
            );
            *captured.borrow_mut() =
                Some(crate::storage::test_database_shape_snapshot(&second.connection).unwrap());
        },
        || first.complete_work(&input, &DevelopmentNoopRedactor),
    );
    assert!(fired.get());
    assert!(
        result.is_err(),
        "completion sealed newly consumed, conflicted imported proof"
    );
    let error = result.unwrap_err();
    assert!(
        error.to_string().contains("selected imported uses changed"),
        "{error}"
    );
    let unchanged = post_second.borrow_mut().take().unwrap();
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&first.connection).unwrap(),
        unchanged
    );
    assert_eq!(count(&first, "completion_seal"), 0);
    assert_eq!(
        first.get_work_item(item.work_id).unwrap().active_run_id,
        Some(run)
    );
    assert!(!first.get_work_run(run).unwrap().is_finished());
    assert_eq!(
        first
            .current_work_claim(item.work_id)
            .unwrap()
            .unwrap()
            .claim_id,
        input.claim_id
    );

    // A retry captures the new use. It must reject the actual current terminal
    // conflict, rather than merely guarding the first empty-to-nonempty change.
    let retry = first
        .complete_work(&input, &DevelopmentNoopRedactor)
        .unwrap_err();
    assert!(
        retry.to_string().contains("terminal facts conflict"),
        "{retry}"
    );
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&first.connection).unwrap(),
        unchanged
    );
}
