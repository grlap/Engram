use super::*;

#[test]
fn kept_root_recovery_preserves_a_new_registered_input_witness() {
    let mut f = prepared(false, true);
    let original = f.request.registration.binding.clone();
    let work = f.store.get_work_item(original.work_id).unwrap();
    let held = claim(&mut f.store, &work, "runner", "recover-witness", 4000, 3600);
    assert_eq!(held.claim_id, original.claim_id);
    assert!(held.fence > original.claim_fence);
    f.request.registration.binding.claim_fence = held.fence;
    f.request.registration.registered_at = at(4001);
    f.request.run.started_at = at(4001);
    f.request.run.finished_at = at(4005);
    f.request.checks[0].started_at = Some(at(4002));
    f.request.checks[0].completed_at = Some(at(4005));
    let check = f.record_at(at(4006)).unwrap().verifications[0]
        .record_id
        .clone();
    let note = evidence(
        &mut f.store,
        &work,
        &held,
        "runner",
        "kept-root-review",
        4007,
    );
    quiet_sighting(&mut f, "kept-root-quiet", 4007);
    let mut input = judgment(&f, &note);
    input.recorded_at = at(4008);
    let evaluated = record_evaluation(&mut f, &input).unwrap();
    assert_eq!(
        evaluated.record.registered_input_witness.as_ref(),
        Some(&check)
    );
    assert!(
        !input.verdicts[0].evidence.contains(&check),
        "the witness is independent of the cited review note"
    );
    checkpoint(
        &mut f.store,
        &work,
        &held,
        "runner",
        "kept-root-witness-checkpoint",
        4009,
        &[check.clone(), note.clone()],
    );
    let mut complete = completion_request(
        &work,
        &held,
        "runner",
        &check,
        "kept-root-witness-done",
        4010,
    );
    complete.acceptance.clear();
    complete.evidence.push(note);
    complete.source_fingerprint = Some(f.request.input.revision());
    let sealed = f
        .store
        .complete_work(&complete, &DevelopmentNoopRedactor)
        .unwrap();
    assert_eq!(sealed.acceptance_evaluation, Some(evaluated.evaluation));
    let report = f.store.verify_all().unwrap();
    assert!(report.is_healthy(), "{report:?}");
}
