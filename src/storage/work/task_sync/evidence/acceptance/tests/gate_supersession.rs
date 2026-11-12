//! A newer record of a cited gate replaces that citation in retained reuse,
//! as it does natively, whatever cut the request names.
use super::*;

fn gate(fixture: &mut Fixture, name: &str, failed: bool, second: i64) -> ObjectId {
    let work = fixture
        .store
        .get_work_item(fixture.binding.work_id)
        .unwrap();
    let held = fixture.store.claim_fixture(work.work_id);
    fixture
        .store
        .record_gate_evidence(
            &RecordGateEvidenceRequest {
                work_id: work.work_id,
                run_id: held.run_id,
                expected_work_revision: work.revision,
                holder: session(),
                claim_id: held.claim_id,
                claim_fence: held.fence,
                name: name.into(),
                failed: if failed {
                    vec![format!("{name}::case")]
                } else {
                    vec![]
                },
                evidence_ref: None,
                actor: actor("runner"),
                recorded_at: at(second),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap()
}

fn stored(fixture: &Fixture, ids: &[&ObjectId]) -> Vec<Option<(String, Vec<u8>)>> {
    ids.iter()
        .map(|id| object_on(&fixture.store.connection, id).unwrap())
        .collect()
}

#[test]
fn a_newer_record_of_a_cited_gate_retires_a_retained_pass_that_an_older_cut_omits() {
    for (case, later, superseding) in [("unrelated", "clippy", false), ("same", "focused", true)] {
        let root = repository();
        let mut fixture = Fixture::with_bindings(root.path(), false);
        fixture.observe(root.path(), 5);
        let cited = gate(&mut fixture, "focused", false, 10);
        let evaluation =
            evaluate_basis(&mut fixture, root.path(), &cited, AcceptanceBasis::Judgment);
        let request = retained_evaluation(&mut fixture, &evaluation);
        let actor = fixture.local_actor();
        let id = fixture
            .store
            .associate_imported_acceptance(&project(), &session(), &actor, &request, at(22))
            .unwrap();
        let summary = fixture
            .store
            .imported_acceptance_summary(&id, at(23))
            .unwrap();
        assert!(
            summary.recorded_eligible && summary.currently_eligible,
            "{case}: {summary:?}"
        );

        let original = stored(&fixture, &[&cited, &evaluation.evaluation]);
        gate(&mut fixture, later, true, 30);
        let native = fixture
            .store
            .acceptance_evaluation_status(fixture.binding.work_id, None)
            .unwrap()
            .unwrap();
        assert_eq!(native.stale.is_some(), superseding, "{case}: native");
        // The newer record arrives on the current feed; the request keeps the
        // older cut, which does not contain it.
        let current = with_witnesses(&fixture, &request.origin, vec![]);
        retain(&mut fixture.store, &current);
        let expected = superseding.then_some(AcceptanceUseRefusal::EvaluationBasisMoved);
        let summary = fixture
            .store
            .imported_acceptance_summary(&id, at(31))
            .unwrap();
        assert!(summary.recorded_eligible, "{case}: {summary:?}");
        assert_eq!(
            summary.currently_eligible, !superseding,
            "{case}: {summary:?}"
        );
        assert_eq!(summary.refusal, expected, "{case}");
        let mut again = request.clone();
        again.decision = ObjectId::mint();
        let id = fixture
            .store
            .associate_imported_acceptance(&project(), &session(), &actor, &again, at(32))
            .unwrap();
        let summary = fixture
            .store
            .imported_acceptance_summary(&id, at(33))
            .unwrap();
        assert_eq!(
            summary.recorded_eligible, !superseding,
            "{case}: {summary:?}"
        );
        assert_eq!(summary.refusal, expected, "{case}");
        assert_eq!(
            stored(&fixture, &[&cited, &evaluation.evaluation]),
            original,
            "{case}: original history changed"
        );
        healthy(&fixture.store);
    }
}
