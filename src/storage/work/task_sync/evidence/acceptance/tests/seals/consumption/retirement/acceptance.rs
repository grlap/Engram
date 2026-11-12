//! Stale implicit acceptance yields to current native checks, preserving history.
use super::*;

fn captured_history(data: &Selected) -> Vec<(String, Vec<u8>)> {
    let mut statement = data
        .target
        .store
        .connection
        .prepare(
            "SELECT object_id,canonical_json FROM objects WHERE object_kind IN
         ('imported_evidence_consumption','work_obligation_resolution') ORDER BY object_id",
        )
        .unwrap();
    statement
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<std::result::Result<_, _>>()
        .unwrap()
}

#[test]
fn stale_uncited_acceptance_retires_after_each_local_movement() {
    for fault in ["source", "revision", "root", "recovery", "handoff"] {
        let mut data = selected(false);
        let used = consume(&mut data.target, &data.acceptance.decision, true);
        let satisfied: bool = data.target.store.connection.query_row(
            "SELECT EXISTS(SELECT 1 FROM work_run_obligations WHERE evidence_id=?1 AND state='satisfied')",
            [used.consumption.as_str()], |row| row.get(0),
        ).unwrap();
        assert!(
            satisfied,
            "fixture must exercise implicit obligation credit"
        );
        let history = captured_history(&data);
        assert_ne!(history.len(), 0);
        let (holder, now) = move_destination(&mut data, fault);
        let fresh = fresh_native(&mut data, &holder, now);
        let seal = finish(&mut data, &holder, &fresh, now + 1)
            .unwrap_or_else(|error| panic!("{fault}: {error}"));
        assert!(!seal.evidence.contains(&used.consumption), "{fault}");
        assert_eq!(seal.acceptance[0].evidence, vec![fresh], "{fault}");
        let after = captured_history(&data);
        for original in history {
            assert!(
                after.contains(&original),
                "{fault}: original history changed"
            );
        }
        healthy(&data.target.store);
        crate::storage::work::task_sync::evidence::consumption::rebuild(
            &data.target.store.connection,
        )
        .unwrap();
        healthy(&data.target.store);
    }
}

#[test]
fn explicitly_cited_stale_acceptance_still_refuses_after_each_movement() {
    for fault in ["source", "revision", "root", "recovery", "handoff"] {
        let mut data = selected(false);
        let used = consume(&mut data.target, &data.acceptance.decision, true);
        let (holder, now) = move_destination(&mut data, fault);
        fresh_native(&mut data, &holder, now);
        let history = captured_history(&data);
        assert!(
            finish(&mut data, &holder, &used.consumption, now + 1).is_err(),
            "{fault}: explicit stale citation must refuse"
        );
        assert_eq!(captured_history(&data), history, "{fault}");
        healthy(&data.target.store);
    }
}

#[test]
fn implicit_acceptance_retirement_preserves_terminal_and_provenance_refusals() {
    for fault in ["terminal", "provenance"] {
        let mut data = selected(false);
        consume(&mut data.target, &data.acceptance.decision, true);
        let (holder, now) = move_destination(&mut data, "source");
        let fresh = fresh_native(&mut data, &holder, now);
        if fault == "terminal" {
            data.origin.request.operation_key = "retired-acceptance-conflict".into();
            data.origin.request.completion.result.verdict = RemoteRunVerdict::Fail;
            let record = data.origin.record(24);
            let conflicting = pack(
                &data.acceptance.origin,
                vec![envelope(
                    &data.origin.store,
                    &data.acceptance.origin,
                    &record.operation,
                )],
            );
            retain(&mut data.target.store, &conflicting);
        } else {
            data.target
                .store
                .connection
                .execute(
                    "DELETE FROM work_task_sync_evidence_feed_members WHERE object_id=?1",
                    [data.verification.original.as_str()],
                )
                .unwrap();
        }
        assert!(
            finish(&mut data, &holder, &fresh, now + 1).is_err(),
            "{fault}"
        );
    }
}

#[test]
fn retired_acceptance_cannot_carry_a_native_check_across_a_later_source_change() {
    let mut data = selected(false);
    consume(&mut data.target, &data.acceptance.decision, true);
    let (holder, now) = move_destination(&mut data, "source");
    let fresh = fresh_native(&mut data, &holder, now);
    // The native-check helper resumed this session's control connection.
    // Source observation uses the fixture token, so renew it before reporting.
    data.target.token = data
        .target
        .store
        .resume_work_connection(&SessionId(holder.clone()), at(now + 1))
        .unwrap();
    std::fs::write(data.root.path().join("file"), "moved after native check\n").unwrap();
    data.target.report_source(data.root.path(), now + 1, true);
    assert!(finish(&mut data, &holder, &fresh, now + 2).is_err());
    healthy(&data.target.store);
}

#[test]
fn retired_acceptance_does_not_bypass_native_check_or_evaluation_policy() {
    for policy in [false, true] {
        let mut data = selected(false);
        consume(&mut data.target, &data.acceptance.decision, true);
        let (holder, now) = move_destination(&mut data, "source");
        if policy {
            let fresh = fresh_native(&mut data, &holder, now);
            enable(&mut data.target.store);
            let error = finish(&mut data, &holder, &fresh, now + 1).unwrap_err();
            assert!(
                error.to_string().contains("evaluated acceptance policy"),
                "{error}"
            );
        } else {
            let work = data
                .target
                .store
                .get_work_item(data.target.binding.work_id)
                .unwrap();
            let claim = data
                .target
                .store
                .current_work_claim(work.work_id)
                .unwrap()
                .unwrap();
            let note = data
                .target
                .store
                .record_work_note(
                    &RecordWorkNoteRequest {
                        status: false,
                        work_id: work.work_id,
                        run_id: claim.run_id,
                        expected_work_revision: work.revision,
                        holder: claim.holder,
                        claim_id: claim.claim_id,
                        claim_fence: claim.fence,
                        summary: "ordinary note, no current native check".into(),
                        refs: vec![],
                        actor: actor(&holder),
                        idempotency_key: "acceptance-retirement-note".into(),
                        recorded_at: at(now),
                    },
                    &DevelopmentNoopRedactor,
                )
                .unwrap();
            let error = finish(&mut data, &holder, &note.evidence, now + 1).unwrap_err();
            assert!(error.to_string().contains("passing check"), "{error}");
        }
        healthy(&data.target.store);
    }
}
