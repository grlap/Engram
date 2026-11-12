use super::*;
use crate::storage::work::task_sync::evidence::test_support::{
    capture_roundtrip, dependency_refusals, project_store,
};
use crate::task_sync::evidence::{EvidencePack, RecordKind};

#[test]
fn native_typed_dependencies_preserve_historical_registered_input_witness() {
    let home = crate::test_support::temp_home().unwrap();
    let store = project_store(home.path(), &ProjectId("project-a".into()));
    let mut f = prepared_in(store, false, true);
    let recorded = f.record().unwrap();
    let check = recorded.verifications[0].record_id.clone();
    let work = f
        .store
        .get_work_item(f.request.registration.binding.work_id)
        .unwrap();
    let held = f.store.claim_fixture(work.work_id);
    let note = f
        .store
        .record_work_evidence(
            &crate::domain::RecordWorkEvidenceRequest {
                work_id: work.work_id,
                run_id: held.run_id,
                expected_work_revision: work.revision,
                holder: SessionId("runner".into()),
                claim_id: held.claim_id,
                claim_fence: held.fence,
                summary: "focused validation passed".into(),
                refs: vec![],
                actor: actor("runner"),
                idempotency_key: "review-note".into(),
                recorded_at: at(10),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    quiet_sighting(&mut f, "capture-quiet-input", 11);
    let input = judgment(&f, &note);
    let evaluated = record_evaluation(&mut f, &input).unwrap();
    assert_eq!(
        evaluated.record.registered_input_witness.as_ref(),
        Some(&check)
    );
    assert!(!input.verdicts[0].evidence.contains(&check));
    let packs = capture_roundtrip(
        &mut f.store,
        &f.project,
        f.request.registration.binding.run_id,
        vec![evaluated.evaluation.clone()],
    );
    let witness = packs
        .iter()
        .flat_map(EvidencePack::records)
        .find(|record| record.object_id() == &check)
        .unwrap();
    assert_eq!(witness.kind(), RecordKind::VerificationEvidence);
    let evidence: VerificationEvidence = serde_json::from_slice(witness.canonical_bytes()).unwrap();
    let producer = packs
        .iter()
        .flat_map(EvidencePack::records)
        .find(|record| record.object_id() == &evidence.producer_observation)
        .unwrap();
    assert_eq!(producer.kind(), RecordKind::RegisteredHostRun);
    dependency_refusals(&packs, &check);
}
