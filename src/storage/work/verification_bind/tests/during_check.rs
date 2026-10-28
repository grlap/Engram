//! Supplied host facts, ordered through a begun control turn. This does not
//! prove live `TermAl` observation emission or an actual subprocess start.

use super::*;
use crate::domain::{
    AcceptanceBasis, AcceptanceEvaluationMode, AcceptanceEvaluationPolicy, AcceptanceVerdict,
    IssuedTurnGrant, MechanicalBasis, ObservationAccounting,
};
use crate::storage::test_support::TestControlBinding;

struct Fixture {
    store: SqliteStore,
    a: Held,
    b: Held,
    host: TestControlBinding,
    grant: IssuedTurnGrant,
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
    let connection_token = store
        .resume_control_connection(&a.claim.holder, at(7))
        .unwrap();
    let binding = binding_of(&store, &a);
    let mut host_actor = actor(HOLDER);
    host_actor.run_id = Some(a.claim.run_id.0.to_string());
    let control = store
        .bind_control_session_with_work(
            &a.work.project_id,
            "local-work:check-in-progress",
            "Check A while observing B",
            &a.claim.holder,
            &connection_token,
            &host_actor,
            Some(&binding),
            ControlAssurance::TurnGated,
            &[EffectClass::Observe],
            1,
            "host-on-a",
            at(8),
        )
        .unwrap();
    let host = TestControlBinding {
        binding: control,
        connection_token,
    };
    let decision = store
        .evaluate_control_turn(
            &a.work.project_id,
            &host.status.session_id,
            &host.connection_token,
            &host.routing_token,
            &TurnIntent {
                idempotency_key: "check-a".into(),
                intent_fingerprint: check_fingerprint("check-a"),
                purpose: Some(TurnPurpose::Ordinary),
                requested_effects: vec![EffectClass::Observe],
                resource_intents: vec![],
            },
            at(9),
        )
        .unwrap();
    let ControlTurnDecision::Grant { grant } = decision else {
        panic!("A must receive a grant: {decision:?}");
    };
    assert!(matches!(
        store
            .begin_control_turn(
                &a.work.project_id,
                &host.status.session_id,
                &host.connection_token,
                &host.routing_token,
                &grant.grant_id,
                &[],
                "begin-a",
                at(10),
            )
            .unwrap(),
        ControlTurnBeginDecision::Begin { .. }
    ));
    Fixture {
        store,
        a,
        b,
        host,
        grant: *grant,
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
        assert_eq!(closing.source_basis, basis("R1", self.b.generation));
        assert_eq!(closing.observed_at, at(second - 1));
        receipt
    }

    fn finish_a(&mut self) -> ObjectId {
        let outcome = self
            .store
            .checkpoint_control_turn_with_evidence(
                &self.a.work.project_id,
                &self.host.status.session_id,
                &self.host.connection_token,
                &self.host.routing_token,
                &self.grant.grant_id,
                TurnNextIntent::Continue,
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
                "finish-a",
                at(20),
            )
            .unwrap();
        let ControlTurnCheckpointDecision::Checkpointed { receipt } = outcome else {
            panic!("A must complete without switching to B: {outcome:?}");
        };
        assert_eq!(receipt.verification_evidence.len(), 1);
        let original = receipt.verification_evidence[0].clone();
        assert_eq!(stored(&self.store, &original).completed_at, at(20));
        original
    }

    fn bind_input(&self, original: &ObjectId, sighting: &ObjectId) -> VerificationBindInput {
        let mut target = target(&self.store, &self.b, "R1", vec![1]);
        target.sighting.observation = sighting.clone();
        let mut input = request(original, "R1", vec![target]);
        input.measurement.measured_at = at(29);
        input
    }
}

#[test]
fn a_target_observed_during_a_check_binds_replays_and_passes_evaluation() {
    let mut f = fixture();
    let sighting = f.observe_b(15);
    // The same host remains bound to A with its turn begun during B's observation.
    let original = f.finish_a();
    let native = stored(&f.store, &original);
    // The checkpoint also appends resolutions after the verification: its
    // run's head is not the verification's own provenance position.
    let original_position: i64 = f.store.connection.query_row(
        "SELECT position FROM work_feed_entries WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND object_id = ?2",
        rusqlite::params![f.a.claim.run_id.0.to_string(), original.as_str()],
        |row| row.get(0),
    ).unwrap();
    let input = f.bind_input(&original, &sighting.observation);
    let receipt = bind_through_host(&mut f.store, &f.host, &input, 30).unwrap();
    assert!(!receipt.replayed);
    assert_eq!(receipt.bound.len(), 1);
    let bound = &receipt.bound[0];
    assert_eq!(bound.work_id, f.b.work.work_id);
    assert_eq!(bound.run_id, f.b.claim.run_id);
    assert_eq!(bound.criteria.len(), 1);
    assert!(bound.criteria[0].eligible);
    let record = stored(&f.store, &bound.verification);
    assert_eq!(record.binding, binding_of(&f.store, &f.b));
    assert_eq!(record.source_basis, basis("R1", f.b.generation));
    assert_eq!(record.producer_observation, native.producer_observation);
    assert_eq!(record.check_kind, native.check_kind);
    assert_eq!(record.check_fingerprint, native.check_fingerprint);
    assert_eq!(record.result, native.result);
    assert_eq!(record.environment, native.environment);
    assert_eq!(record.completed_at, native.completed_at);
    assert_eq!(record.recorded_at, at(30));
    let provenance = record.bound_from.unwrap();
    assert_eq!(provenance.verification, original);
    assert_eq!(provenance.work_id, f.a.work.work_id);
    assert_eq!(provenance.run_id, f.a.claim.run_id);
    assert_eq!(provenance.original_position, original_position);
    assert_eq!(provenance.sighting, sighting.observation);
    assert_eq!(provenance.measurement, input.measurement);
    assert_eq!(provenance.criteria, vec![1]);

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
    for id in &sighting.opened_obligations {
        let state: (String, String) = f.store.connection.query_row(
            "SELECT state, evidence_id FROM work_run_obligations WHERE definition_id = ?1 AND run_id = ?2",
            rusqlite::params![id.as_str(), f.b.claim.run_id.0.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        ).unwrap();
        assert_eq!(state, ("satisfied".into(), bound.verification.to_string()));
    }
    let counts = (evidence_rows(&f.store), control_operations(&f.store));
    let head = run_head(&f.store, &f.b);
    let replay = bind_through_host(&mut f.store, &f.host, &input, 31).unwrap();
    assert!(replay.replayed);
    assert_eq!(replay.original, receipt.original);
    assert_eq!(replay.bound, receipt.bound);
    assert_eq!(
        (evidence_rows(&f.store), control_operations(&f.store)),
        counts
    );
    assert_eq!(run_head(&f.store, &f.b), head);
    assert_eq!(obligations(&f.store, &f.b), states);

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

#[test]
fn a_target_observed_after_check_completion_refuses_without_writes() {
    let mut f = fixture();
    let original = f.finish_a();
    let sighting = f.observe_b(25);
    let input = f.bind_input(&original, &sighting.observation);
    let before = (
        evidence_rows(&f.store),
        control_operations(&f.store),
        run_head(&f.store, &f.a),
        run_head(&f.store, &f.b),
        obligations(&f.store, &f.b),
    );
    let Err(StoreError::VerificationBindRefused(refusal)) =
        bind_through_host(&mut f.store, &f.host, &input, 30)
    else {
        panic!("the older check must not bind");
    };
    assert_eq!(refusal.original, None);
    assert_eq!(refusal.request, None);
    assert_eq!(refusal.targets.len(), 1);
    assert_eq!(refusal.targets[0].work_id, f.b.work.work_id);
    assert_eq!(
        refusal.targets[0].reason,
        Reason::CheckPredatesUnadmittedChange
    );
    assert!(refusal.targets[0].remedy.is_some());
    assert_eq!(
        (
            evidence_rows(&f.store),
            control_operations(&f.store),
            run_head(&f.store, &f.a),
            run_head(&f.store, &f.b),
            obligations(&f.store, &f.b)
        ),
        before
    );
    let report = f.store.verify_all().unwrap();
    assert!(report.is_healthy(), "{report:?}");
}
