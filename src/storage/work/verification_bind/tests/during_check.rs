//! Supplied host facts with a historical producer and ordinary source captures. This does not
//! prove live `TermAl` observation emission or an actual subprocess start.

use super::*;
use crate::domain::{
    AcceptanceBasis, AcceptanceEvaluationMode, AcceptanceEvaluationPolicy, AcceptanceVerdict,
    MechanicalBasis, ObservationAccounting,
};
use crate::storage::test_support::{TestWorkHost, open_work_host};

struct Fixture {
    store: SqliteStore,
    a: Held,
    b: Held,
    host: TestWorkHost,
    capture: HistoricalCapture,
}

/// B has exactly one Test-bound criterion, so a cited pass is an overall pass.
fn target_item(store: &mut SqliteStore) -> Held {
    let mut input = root_request("project-a", "item-b", 4);
    input.acceptance = vec!["the tests pass".into()];
    input.acceptance_bindings = vec![AcceptanceBinding {
        criterion: 1,
        requirement: VerificationRequirement {
            check_kind: VerificationKind::Test,
            check_fingerprint: None,
        },
    }];
    let work = store.create_work(&input, &DevelopmentNoopRedactor).unwrap();
    let claim = claim(store, &work, HOLDER, "item-b-claim", 4, 36_000);
    let root = name_root(store, &work, &claim, 10, 4);
    let change = change(store, &work, &claim, "item-b-edit", "R0", 10, 5);
    Held {
        git_tree: None,
        work: store.get_work_item(work.work_id).unwrap(),
        claim,
        generation: 10,
        change,
        root_event: root.event,
        named_second: 4,
    }
}

fn fixture() -> Fixture {
    let mut store = SqliteStore::open_in_memory().unwrap();
    store
        .set_acceptance_evaluation_policy(
            &AcceptanceEvaluationPolicy {
                allowed_modes: vec![AcceptanceEvaluationMode::SameSession],
                mechanical_basis: MechanicalBasis::Observed,
                require_source_freshness: false,
            },
            &actor("policy-admin"),
            "enable",
            None,
            at(1),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let a = held(&mut store, "item-a", HOLDER, 9, "R1", 2);
    let b = target_item(&mut store);
    let host = open_work_host(&mut store, &a.claim.holder.0, at(7));
    let capture = HistoricalCapture {
        project: a.work.project_id.clone(),
        session: a.claim.holder.clone(),
        binding: binding_of(&store, &a),
        capture_id: "historical-check-a".into(),
    };
    Fixture {
        store,
        a,
        b,
        host,
        capture,
    }
}

impl Fixture {
    fn observe_b(&mut self, second: i64) -> crate::domain::ExecutionObservationReceipt {
        let receipt = unadmitted_change_through_host(
            &mut self.store,
            &self.b,
            &self.host,
            "R0",
            "R1",
            second,
        );
        assert_eq!(receipt.binding, binding_of(&self.store, &self.b));
        assert_eq!(receipt.recorded_at, at(second));
        assert!(matches!(
            receipt.accounting,
            ObservationAccounting::SourceChange { .. }
        ));
        assert_ne!(receipt.opened_obligations, [] as [ObjectId; 0]);
        let observation: crate::domain::UnadmittedExecutionObservation =
            crate::storage::work::feeds::load_typed_work_object(
                &self.store.connection,
                &receipt.observation,
                crate::storage::work::UNADMITTED_OBSERVATION_KIND,
            )
            .unwrap();
        assert_eq!(observation.binding, receipt.binding);
        assert_eq!(observation.recorded_at, at(second));
        let closing = observation
            .occurrence
            .source_change()
            .unwrap()
            .sighting()
            .unwrap();
        assert_eq!(closing.source_basis, basis("R1", self.b.generation).into());
        assert_eq!(closing.observed_at, at(second - 1));
        receipt
    }

    fn finish_a(&mut self) -> ObjectId {
        let receipt = append_historical_capture(
            &mut self.store,
            &self.capture,
            &[ExecutionObservationInput {
                observation_id: "check-a-finished".into(),
                action_fingerprint: check_fingerprint("suite"),
                effect: EffectClass::Observe,
                outcome: ExecutionOutcome::Succeeded,
                source_changed: false,
                reported_source_change: None,
                source_basis: Some(basis("R1", self.a.generation)),
                observed_at: Some(at(20)),
            }],
            &[VerificationEvidenceInput {
                applicability: None,
                producer_observation: ExecutionObservationReference::ObservationId {
                    observation_id: "check-a-finished".into(),
                },
                check_kind: VerificationKind::Test,
                environment: None,
                summary: Some("host observed the suite".into()),
                refs: vec!["command:suite".into()],
            }],
            &[],
            at(20),
        )
        .unwrap();
        assert_eq!(receipt.verification_evidence.len(), 1);
        let original = receipt.verification_evidence[0].clone();
        assert_eq!(stored(&self.store, &original).completed_at, at(20));
        original
    }

    fn bind_input(&self, original: &ObjectId, sighting: &ObjectId) -> StoredBind {
        let mut target = target(&self.store, &self.b, "R1", vec![1]);
        target.sighting.observation = sighting.clone();
        let mut input = request(original, "R1", vec![target]);
        input.measurement.measured_at = at(29);
        input
    }
}

/// A bound record of a check that was still running when the target was
/// observed satisfies the target's obligations and supports a cited pass.
#[test]
fn a_target_observed_during_a_check_passes_evaluation_on_its_bound_record() {
    let mut f = fixture();
    let sighting = f.observe_b(15);
    // The producer association stays on A while the same host observes B.
    let original = f.finish_a();
    let native = stored(&f.store, &original);
    let input = f.bind_input(&original, &sighting.observation);
    let receipt = store_bound(&mut f.store, &input, 30);
    let bound = &receipt.bound[0];

    let states = obligations(&f.store, &f.b);
    assert!(
        states
            .iter()
            .any(|(rule, _, _)| rule == "acceptance_criterion_requires_verification:1")
    );
    assert!(states.len() > sighting.opened_obligations.len());
    assert!(states.iter().all(
        |(_, state, evidence)| *state == WorkObligationState::Satisfied
            && evidence.as_deref() == Some(bound.verification.as_str())
    ));
    let head = run_head(&f.store, &f.b);

    let evaluation = evaluation(
        &f.store,
        &f.b,
        head,
        Some("R1"),
        vec![verdict(
            1,
            AcceptanceVerdict::Pass,
            AcceptanceBasis::Observed,
            std::slice::from_ref(&bound.verification),
        )],
        "cited-bound-check",
        40,
    );
    let admitted = f
        .store
        .record_acceptance_evaluation(&evaluation, &DevelopmentNoopRedactor)
        .unwrap();
    assert_eq!(admitted.record.verdicts.len(), 1);
    assert_eq!(admitted.record.verdicts[0].verdict, AcceptanceVerdict::Pass);
    assert!(admitted.record.evidence_basis.contains(&bound.verification));
    let evaluated = f
        .store
        .acceptance_evaluation_status(f.b.work.work_id, None)
        .unwrap()
        .unwrap();
    assert_eq!(evaluated.evaluation, admitted.evaluation);
    assert_eq!(evaluated.stale, None);
    assert_eq!(stored(&f.store, &original), native);
    let report = f.store.verify_all().unwrap();
    assert!(report.is_healthy(), "{report:?}");
}
