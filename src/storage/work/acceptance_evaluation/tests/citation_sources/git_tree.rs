use super::*;
use crate::domain::{GitTreeId, VerificationApplicability};

fn set_tree(host: &mut HostSession, tree: Option<char>, restrict: bool) {
    host.basis.git_tree = tree.map(|c| GitTreeId::parse(c.to_string().repeat(40)).unwrap());
    host.applicability = restrict.then(|| VerificationApplicability::GitTree {
        tree: host.basis.git_tree.clone().unwrap(),
    });
}

#[test]
fn git_tree_evaluation_checks_every_selected_citation_and_keeps_unused_history_inert() {
    for missing in [false, true] {
        let (mut fixture, work, _, mut host) = bound_fixture("tree-evaluation", false);
        let note = fixture.evidence.clone();
        let store = &mut fixture.store;
        set_tree(&mut host, Some('a'), true);
        let old = one_check(host.checkpoint(
            store,
            true,
            Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
            20,
        ));
        set_tree(&mut host, (!missing).then_some('b'), !missing);
        let local = one_check(host.checkpoint(
            store,
            false,
            Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
            30,
        ));
        let source = declared(&host.basis.source_revision, Some(&host.basis.workspace_id));
        let mut input = evaluation(
            store,
            &work,
            &[old.clone(), local.clone()],
            &note,
            Some(source),
            40,
        );
        let (_, cause) = admission::typed_refusal(record(store, &input));
        let AcceptanceEvaluationAdmissionCause::Citation(cause) = cause else {
            panic!("citation cause");
        };
        assert_eq!(
            cause.mismatch,
            if missing {
                EvaluationCitationMismatch::GitTreeMissing
            } else {
                EvaluationCitationMismatch::GitTreeMismatch
            }
        );
        // The same old record is harmless when it is not selected.
        input.verdicts[0].evidence = vec![local];
        record(store, &input).unwrap();
        assert!(
            store
                .load_verification_evidence(&old)
                .unwrap()
                .applicability
                .is_some()
        );
    }
}

#[test]
fn git_tree_consumption_rechecks_the_current_quiet_sighting_even_for_a_waived_binding() {
    let (mut fixture, work, claim, mut host) = bound_fixture("tree-consumption", false);
    let note = fixture.evidence.clone();
    let store = &mut fixture.store;
    let binding = store
        .work_run_obligations(claim.run_id)
        .unwrap()
        .into_iter()
        .find(|record| record.state == WorkObligationState::Open)
        .unwrap();
    store
        .waive_work_obligation(
            &WaiveWorkObligationRequest {
                obligation_id: binding.obligation.obligation_id,
                expected_definition: binding.definition_id,
                waived_by: "operator".into(),
                reason: "manual verification".into(),
                actor: actor("operator"),
                idempotency_key: "waive-tree-binding".into(),
                waived_at: at(15),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    set_tree(&mut host, Some('a'), true);
    let check = one_check(host.checkpoint(
        store,
        true,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        20,
    ));
    let input = evaluation(
        store,
        &work,
        std::slice::from_ref(&check),
        &note,
        Some(declared(
            &host.basis.source_revision,
            Some(&host.basis.workspace_id),
        )),
        30,
    );
    record(store, &input).unwrap();
    // A quiet same-content move cannot borrow the check's tree identity.
    set_tree(&mut host, Some('b'), false);
    host.report(store, &[(false, Some("revision-d"))], 40);
    stale_verification_source(done(
        store,
        &work,
        &claim,
        &[note, check],
        Some("revision-d"),
        "done-tree",
        50,
    ));
}

#[test]
fn git_tree_checkpoint_producer_mismatch_rolls_back_for_both_reference_forms() {
    for existing in [false, true] {
        for actual in [None, Some('b'), Some('a')] {
            let (mut fixture, work, _, mut host) = bound_fixture("tree-producer", false);
            let store = &mut fixture.store;
            set_tree(&mut host, actual, false);
            let previous = one_check(host.checkpoint(
                store,
                false,
                Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
                20,
            ));
            let previous = store.load_verification_evidence(&previous).unwrap();
            let grant = host.grant(store, &[EffectClass::Observe], false, 30);
            host.begin(store, &grant, 31);
            let observation = ExecutionObservationInput {
                observation_id: "tree-producer".into(),
                action_fingerprint: check_fingerprint("tree-producer"),
                effect: EffectClass::Observe,
                outcome: ExecutionOutcome::Succeeded,
                source_changed: false,
                reported_source_change: None,
                source_basis: Some(host.basis.clone()),
                observed_at: Some(at(31)),
            };
            let producer_observation = if existing {
                ExecutionObservationReference::ObjectId {
                    object_id: previous.producer_observation,
                }
            } else {
                ExecutionObservationReference::ObservationId {
                    observation_id: observation.observation_id.clone(),
                }
            };
            let verification = VerificationEvidenceInput {
                applicability: Some(VerificationApplicability::GitTree {
                    tree: GitTreeId::parse("a".repeat(40)).unwrap(),
                }),
                producer_observation,
                check_kind: VerificationKind::Test,
                environment: None,
                summary: None,
                refs: vec![],
            };
            let before = test_database_shape_snapshot(&store.connection).unwrap();
            let result = store.checkpoint_control_turn_with_evidence(
                &work.project_id,
                &host.session_id,
                &host.connection_token,
                &host.routing_token,
                &grant.grant_id,
                TurnNextIntent::Continue,
                &[observation],
                &[verification],
                &[],
                "restricted-producer-checkpoint",
                at(32),
            );
            if actual == Some('a') {
                assert!(result.is_ok(), "{result:?}");
            } else {
                let error = result.unwrap_err();
                assert_eq!(
                    crate::host::store_error_code(&error),
                    "invalid_control_session"
                );
                assert!(error.to_string().contains("applicability.tree"));
                assert!(error.to_string().contains("source_basis.git_tree"));
                assert_eq!(
                    test_database_shape_snapshot(&store.connection).unwrap(),
                    before
                );
            }
        }
    }
}

#[test]
fn git_tree_unbound_observed_mechanical_pass_also_requires_applicability() {
    let mut fixture = fixture("unbound-tree");
    let work = fixture.work.clone();
    enable(
        &mut fixture.store,
        &[Mode::SameSession],
        MechanicalBasis::Observed,
        false,
        "policy",
        6,
    );
    let mut host = HostSession::bind(&mut fixture.store, &work, &fixture.claim, 10);
    set_tree(&mut host, Some('a'), true);
    let check = one_check(host.checkpoint(
        &mut fixture.store,
        false,
        Some((VerificationKind::Test, ExecutionOutcome::Succeeded)),
        20,
    ));
    set_tree(&mut host, Some('b'), false);
    host.report(
        &mut fixture.store,
        &[(false, Some("content-revision-1"))],
        30,
    );
    let mut input = request(
        &work,
        cut(&fixture.store, &work),
        "runner",
        Mode::SameSession,
        vec![verdict(
            1,
            AcceptanceVerdict::Pass,
            AcceptanceBasis::Observed,
            &[check],
        )],
        40,
    );
    input.source_basis = Some(declared(
        &host.basis.source_revision,
        Some(&host.basis.workspace_id),
    ));
    let (_, cause) = admission::typed_refusal(record(&mut fixture.store, &input));
    assert!(
        matches!(cause, AcceptanceEvaluationAdmissionCause::Citation(cause) if cause.mismatch == EvaluationCitationMismatch::GitTreeMismatch)
    );
}
