//! Reuse the real named-root/producer fixture; no parallel source qualifier.
use super::*;
use crate::task_sync::evidence::acceptance::{
    AcceptanceUseRefusal, AcceptanceUseRequest, HistoricalProof,
};
use crate::task_sync::evidence::witnesses::{
    GoverningBasis, GoverningSnapshotWitness, SnapshotAvailability,
};

#[path = "tests/movement.rs"]
mod movement;
#[path = "tests/seals.rs"]
mod seals;

#[path = "tests/concurrent.rs"]
mod concurrent;
#[path = "tests/gate_supersession.rs"]
mod gate_supersession;
#[path = "tests/typed_dependencies.rs"]
mod typed_dependencies;

fn enable(store: &mut SqliteStore) {
    store
        .set_acceptance_evaluation_policy(
            &AcceptanceEvaluationPolicy {
                allowed_modes: vec![AcceptanceEvaluationMode::IndependentSession],
                mechanical_basis: MechanicalBasis::Observed,
                require_source_freshness: true,
            },
            &actor("policy-admin"),
            "historical-policy",
            None,
            at(6),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
}

fn evaluate(
    fixture: &mut Fixture,
    root: &std::path::Path,
    verification: &ObjectId,
) -> crate::storage::work::acceptance_evaluation::AcceptanceEvaluationReceipt {
    evaluate_basis(fixture, root, verification, AcceptanceBasis::Observed)
}

fn evaluate_basis(
    fixture: &mut Fixture,
    root: &std::path::Path,
    verification: &ObjectId,
    basis: AcceptanceBasis,
) -> crate::storage::work::acceptance_evaluation::AcceptanceEvaluationReceipt {
    enable(&mut fixture.store);
    let cut = crate::storage::work::feeds::current_run_feed_cut_on(
        &fixture.store.connection,
        fixture.binding.run_id,
    )
    .unwrap();
    fixture
        .store
        .record_acceptance_evaluation_registering(
            &RecordAcceptanceEvaluationRequest {
                project_id: project(),
                work_id: fixture.binding.work_id,
                expected_work_revision: fixture.binding.work_revision,
                evaluated_through: cut.position,
                mode: AcceptanceEvaluationMode::IndependentSession,
                execution_identity: None,
                parent_session: None,
                evaluator_model: None,
                source_basis: Some(AcceptanceSourceBasis {
                    workspace_id: None,
                    identity: VerificationSourceIdentity::SourceId {
                        source_id: source(root),
                    },
                }),
                verdicts: vec![CriterionVerdictInput {
                    criterion: 1,
                    verdict: AcceptanceVerdict::Pass,
                    basis,
                    rationale: "original judged evidence".into(),
                    evidence: vec![verification.clone()],
                }],
                evaluator: actor("reviewer"),
                attempt_key: Some("original-evaluation".into()),
                recorded_at: at(12),
                supersedes: None,
            },
            &DevelopmentNoopRedactor,
            Some((&SessionId("reviewer".into()), at(11))),
        )
        .unwrap()
}

/// Retain a native evaluation and its whole run feed under a newly enrolled
/// origin, as a peer exporting that history would, and request its reuse.
fn retained_evaluation(
    fixture: &mut Fixture,
    evaluation: &crate::storage::work::acceptance_evaluation::AcceptanceEvaluationReceipt,
) -> AcceptanceUseRequest {
    let work = fixture
        .store
        .get_work_item(fixture.binding.work_id)
        .unwrap();
    let (origin, enrollment) = enrollment();
    let witness_id = ObjectId::mint();
    let witness = GoverningSnapshotWitness {
        basis: GoverningBasis {
            evaluation: evaluation.evaluation.clone(),
            project_id: project(),
            work_id: work.work_id,
            work_revision: evaluation.record.work_revision,
            content_fingerprint: evaluation.record.work_revision_hash.clone(),
        },
        snapshot: SnapshotAvailability::Exact {
            snapshot: Box::new(work),
            source_snapshot: None,
        },
    };
    let pack = with_witnesses(
        fixture,
        &origin,
        vec![
            enrollment,
            RecordEnvelope::from_canonical(
                origin.clone(),
                witness_id.clone(),
                RecordKind::GoverningSnapshotWitness.as_str(),
                serde_json_canonicalizer::to_vec(&witness).unwrap(),
            )
            .unwrap(),
        ],
    );
    let cut = pack
        .records()
        .iter()
        .find(|record| record.kind() == RecordKind::OriginFeedCut)
        .unwrap()
        .object_id()
        .clone();
    retain(&mut fixture.store, &pack);
    AcceptanceUseRequest {
        decision: ObjectId::mint(),
        origin,
        proof: HistoricalProof::Evaluation {
            evaluation: evaluation.evaluation.clone(),
            governing_witness: witness_id,
        },
        origin_cuts: vec![cut],
        binding: fixture.binding.clone(),
        verification_decisions: vec![],
    }
}

fn prepare(
    fixture: &mut Fixture,
    root: &std::path::Path,
    unavailable: bool,
) -> AcceptanceUseRequest {
    fixture.observe(root, 5);
    let receipt = fixture.record(10);
    let original = &receipt.verifications[0].record_id;
    let evaluation = evaluate(fixture, root, original);
    let governing = fixture
        .store
        .get_work_item(fixture.binding.work_id)
        .unwrap();
    let witness_id = ObjectId::mint();
    let witness = GoverningSnapshotWitness {
        basis: GoverningBasis {
            evaluation: evaluation.evaluation.clone(),
            project_id: project(),
            work_id: governing.work_id,
            work_revision: governing.revision,
            content_fingerprint: evaluation.record.work_revision_hash.clone(),
        },
        snapshot: if unavailable {
            SnapshotAvailability::GoverningSnapshotUnavailable
        } else {
            SnapshotAvailability::Exact {
                snapshot: Box::new(governing),
                source_snapshot: None,
            }
        },
    };
    let (origin, enrollment) = enrollment();
    let witness = RecordEnvelope::from_canonical(
        origin.clone(),
        witness_id.clone(),
        RecordKind::GoverningSnapshotWitness.as_str(),
        serde_json_canonicalizer::to_vec(&witness).unwrap(),
    )
    .unwrap();
    let pack = with_witnesses(
        fixture,
        &origin,
        vec![
            enrollment,
            witness,
            envelope(&fixture.store, &origin, receipt.origin.as_ref().unwrap()),
            envelope(&fixture.store, &origin, &receipt.operation),
        ],
    );
    let cut = pack
        .records()
        .iter()
        .find(|record| record.kind() == RecordKind::OriginFeedCut)
        .unwrap()
        .object_id()
        .clone();
    retain(&mut fixture.store, &pack);
    let mut verification = intent(fixture, original.clone(), origin.clone());
    verification.criterion = Some(1);
    let actor = fixture.local_actor();
    fixture
        .store
        .associate_imported_verification(&project(), &session(), &actor, &verification, at(21))
        .unwrap();
    AcceptanceUseRequest {
        decision: ObjectId::mint(),
        origin,
        proof: HistoricalProof::Evaluation {
            evaluation: evaluation.evaluation,
            governing_witness: witness_id,
        },
        origin_cuts: vec![cut],
        binding: fixture.binding.clone(),
        verification_decisions: vec![verification.decision],
    }
}

#[test]
fn original_evaluation_reuses_exact_governing_and_eligible_citation_without_a_verdict_write() {
    let root = repository();
    let mut fixture = Fixture::new(root.path());
    let request = prepare(&mut fixture, root.path(), false);
    let before:i64=fixture.store.connection.query_row("SELECT count(*) FROM objects WHERE object_kind IN ('acceptance_evaluation','completion_seal')",[],|row|row.get(0)).unwrap();
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
        "{summary:?}"
    );
    assert_eq!(
        fixture
            .store
            .associate_imported_acceptance(&project(), &session(), &actor, &request, at(24))
            .unwrap(),
        id
    );
    let after:i64=fixture.store.connection.query_row("SELECT count(*) FROM objects WHERE object_kind IN ('acceptance_evaluation','completion_seal')",[],|row|row.get(0)).unwrap();
    assert_eq!(before, after);
    std::fs::write(root.path().join("file"), "different\n").unwrap();
    assert!(
        !fixture
            .store
            .imported_acceptance_summary(&id, at(25))
            .unwrap()
            .currently_eligible
    );
    // Exact uncertain retry is independent of the later filesystem state.
    assert_eq!(
        fixture
            .store
            .associate_imported_acceptance(&project(), &session(), &actor, &request, at(26))
            .unwrap(),
        id
    );
    healthy(&fixture.store);
}

#[test]
fn unavailable_governing_is_a_retained_refusal_and_missing_context_rolls_back() {
    let root = repository();
    let mut fixture = Fixture::new(root.path());
    let request = prepare(&mut fixture, root.path(), true);
    let actor = fixture.local_actor();
    let id = fixture
        .store
        .associate_imported_acceptance(&project(), &session(), &actor, &request, at(22))
        .unwrap();
    assert_eq!(
        fixture
            .store
            .imported_acceptance_summary(&id, at(23))
            .unwrap()
            .refusal,
        Some(AcceptanceUseRefusal::GoverningSnapshotUnavailable)
    );
    let mut bad = request;
    bad.decision = ObjectId::mint();
    bad.origin_cuts = vec![ObjectId::mint()];
    let before = crate::storage::test_database_shape_snapshot(&fixture.store.connection).unwrap();
    assert!(
        fixture
            .store
            .associate_imported_acceptance(&project(), &session(), &actor, &bad, at(24))
            .is_err()
    );
    assert_eq!(
        before,
        crate::storage::test_database_shape_snapshot(&fixture.store.connection).unwrap()
    );
    healthy(&fixture.store);
}

fn current_binding(fixture: &Fixture) -> ControlWorkBinding {
    let work = fixture
        .store
        .get_work_item(fixture.binding.work_id)
        .unwrap();
    let claim = fixture.store.claim_fixture(work.work_id);
    ControlWorkBinding {
        work_revision: work.revision,
        claim_id: claim.claim_id,
        claim_fence: claim.fence,
        ..fixture.binding.clone()
    }
}

fn later_evaluation(
    fixture: &Fixture,
    request: &AcceptanceUseRequest,
    mutate: impl FnOnce(&mut AcceptanceEvaluation),
) -> (EvidencePack, AcceptanceUseRequest) {
    use crate::task_sync::evidence::witnesses::{FeedMember, OriginFeedCut, OriginFeedSegment};
    let HistoricalProof::Evaluation { evaluation, .. } = &request.proof else {
        panic!("evaluation fixture")
    };
    let (_, bytes) = object_on(&fixture.store.connection, evaluation)
        .unwrap()
        .unwrap();
    let mut record: AcceptanceEvaluation = serde_json::from_slice(&bytes).unwrap();
    mutate(&mut record);
    let id = ObjectId::mint();
    let witness_id = ObjectId::mint();
    let segment_id = ObjectId::mint();
    let cut_id = ObjectId::mint();
    let (_, cut_bytes) = object_on(&fixture.store.connection, &request.origin_cuts[0])
        .unwrap()
        .unwrap();
    let prior: OriginFeedCut = serde_json::from_slice(&cut_bytes).unwrap();
    let member = FeedMember {
        position: prior.through + 1,
        object_kind: "acceptance_evaluation".into(),
        object_id: id.clone(),
    };
    let segment = OriginFeedSegment {
        source: prior.source.clone(),
        from_exclusive: prior.through,
        through: member.position,
        predecessor: prior.segments.last().cloned(),
        anchor: prior.endpoint,
        entries: vec![member.clone()],
    };
    let cut = OriginFeedCut {
        source: prior.source,
        through: member.position,
        endpoint: Some(member),
        segments: vec![segment_id.clone()],
    };
    let witness = GoverningSnapshotWitness {
        basis: GoverningBasis {
            evaluation: id.clone(),
            project_id: project(),
            work_id: record.work_id,
            work_revision: record.work_revision,
            content_fingerprint: record.work_revision_hash.clone(),
        },
        snapshot: SnapshotAvailability::Exact {
            snapshot: Box::new(fixture.store.get_work_item(record.work_id).unwrap()),
            source_snapshot: None,
        },
    };
    let records = [
        (
            id.clone(),
            RecordKind::AcceptanceEvaluation,
            serde_json_canonicalizer::to_vec(&record).unwrap(),
        ),
        (
            witness_id.clone(),
            RecordKind::GoverningSnapshotWitness,
            serde_json_canonicalizer::to_vec(&witness).unwrap(),
        ),
        (
            segment_id,
            RecordKind::OriginFeedSegment,
            serde_json_canonicalizer::to_vec(&segment).unwrap(),
        ),
        (
            cut_id.clone(),
            RecordKind::OriginFeedCut,
            serde_json_canonicalizer::to_vec(&cut).unwrap(),
        ),
    ]
    .into_iter()
    .map(|(id, kind, bytes)| {
        RecordEnvelope::from_canonical(request.origin.clone(), id, kind.as_str(), bytes).unwrap()
    })
    .collect();
    let mut next = request.clone();
    next.decision = ObjectId::mint();
    next.proof = HistoricalProof::Evaluation {
        evaluation: id,
        governing_witness: witness_id,
    };
    next.origin_cuts = vec![cut_id];
    (pack(&request.origin, records), next)
}

#[test]
fn latest_origin_position_supersedes_a_pass_even_with_an_older_original_clock() {
    let root = repository();
    let mut fixture = Fixture::new(root.path());
    let request = prepare(&mut fixture, root.path(), false);
    let actor = fixture.local_actor();
    let id = fixture
        .store
        .associate_imported_acceptance(&project(), &session(), &actor, &request, at(22))
        .unwrap();
    let (pack, next) = later_evaluation(&fixture, &request, |record| {
        record.created_at = at(1);
        record.verdicts[0].verdict = AcceptanceVerdict::Fail;
    });
    retain(&mut fixture.store, &pack);
    assert_eq!(
        fixture
            .store
            .imported_acceptance_summary(&id, at(23))
            .unwrap()
            .refusal,
        Some(AcceptanceUseRefusal::SupersededEvaluation)
    );
    let id = fixture
        .store
        .associate_imported_acceptance(&project(), &session(), &actor, &next, at(24))
        .unwrap();
    assert_eq!(
        fixture
            .store
            .imported_acceptance_summary(&id, at(25))
            .unwrap()
            .refusal,
        Some(AcceptanceUseRefusal::BlockingVerdict)
    );
    healthy(&fixture.store);
}

#[test]
fn original_holder_is_not_an_independent_imported_evaluator() {
    let root = repository();
    let mut fixture = Fixture::new(root.path());
    let request = prepare(&mut fixture, root.path(), false);
    let actor = fixture.local_actor();
    let (pack, next) = later_evaluation(&fixture, &request, |record| {
        record.evaluator = actor.clone();
    });
    retain(&mut fixture.store, &pack);
    let id = fixture
        .store
        .associate_imported_acceptance(&project(), &session(), &actor, &next, at(23))
        .unwrap();
    assert_eq!(
        fixture
            .store
            .imported_acceptance_summary(&id, at(24))
            .unwrap()
            .refusal,
        Some(AcceptanceUseRefusal::EvaluatorAffiliated)
    );
    healthy(&fixture.store);
}

#[test]
fn a_governing_content_fingerprint_cannot_be_replaced_with_a_record_link() {
    let root = repository();
    let mut fixture = Fixture::new(root.path());
    let request = prepare(&mut fixture, root.path(), false);
    let (pack, _) = later_evaluation(&fixture, &request, |record| {
        record.work_revision_hash = ObjectId::mint();
    });
    let before = crate::storage::test_database_shape_snapshot(&fixture.store.connection).unwrap();
    assert!(
        fixture
            .store
            .retain_evidence_pack(&project(), &pack, &ObjectId::mint())
            .is_err()
    );
    assert_eq!(
        before,
        crate::storage::test_database_shape_snapshot(&fixture.store.connection).unwrap()
    );
    healthy(&fixture.store);
}

#[test]
fn shared_destination_keeps_the_named_root_through_recovery_and_holder_change() {
    let root = repository();
    let mut fixture = Fixture::new(root.path());
    fixture.observe(root.path(), 5);
    let work = fixture
        .store
        .get_work_item(fixture.binding.work_id)
        .unwrap();
    let recovered = claim(
        &mut fixture.store,
        &work,
        "runner",
        "recover-kept-root",
        4000,
        3600,
    );
    assert_eq!(recovered.claim_id, fixture.binding.claim_id);
    assert!(recovered.fence > fixture.binding.claim_fence);
    let binding = current_binding(&fixture);
    let measured =
        applicability::measure_binding_destination(&fixture.store.connection, &binding).unwrap();
    applicability::binding_destination_on(
        &fixture.store.connection,
        &project(),
        &session(),
        &binding,
        &measured,
        at(4001),
        None,
    )
    .unwrap();
    let work = fixture.store.get_work_item(binding.work_id).unwrap();
    let offer = fixture
        .store
        .offer_work_handoff(
            &OfferWorkHandoffRequest {
                work_id: work.work_id,
                run_id: binding.run_id,
                expected_work_revision: work.revision,
                from: session(),
                to: SessionId("second".into()),
                claim_id: binding.claim_id,
                claim_fence: binding.claim_fence,
                ttl_seconds: 300,
                checkpoint_summary: "keep named root through handoff".into(),
                actor: actor("runner"),
                idempotency_key: "offer-kept-root".into(),
                offered_at: at(4002),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    fixture
        .store
        .accept_work_handoff(
            &AcceptWorkHandoffRequest {
                work_id: work.work_id,
                offer_id: offer.offer_id,
                to: SessionId("second".into()),
                actor: actor("second"),
                idempotency_key: "accept-kept-root".into(),
                accepted_at: at(4003),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let binding = current_binding(&fixture);
    let measured =
        applicability::measure_binding_destination(&fixture.store.connection, &binding).unwrap();
    applicability::binding_destination_on(
        &fixture.store.connection,
        &project(),
        &SessionId("second".into()),
        &binding,
        &measured,
        at(4004),
        None,
    )
    .unwrap();
    assert!(
        applicability::binding_destination_on(
            &fixture.store.connection,
            &project(),
            &session(),
            &binding,
            &measured,
            at(4004),
            None
        )
        .is_err()
    );
    healthy(&fixture.store);
}

#[test]
fn shared_destination_release_and_reclaim_still_requires_a_new_name() {
    let root = repository();
    let mut fixture = Fixture::new(root.path());
    fixture.observe(root.path(), 5);
    let work = fixture
        .store
        .get_work_item(fixture.binding.work_id)
        .unwrap();
    fixture
        .store
        .release_work(
            &ReleaseWorkRequest {
                work_id: work.work_id,
                run_id: fixture.binding.run_id,
                expected_work_revision: work.revision,
                holder: session(),
                claim_id: fixture.binding.claim_id,
                claim_fence: fixture.binding.claim_fence,
                reason: "end named-root responsibility".into(),
                waiver_reason: Some("fixture has no contribution".into()),
                actor: actor("runner"),
                idempotency_key: "release-kept-root".into(),
                released_at: at(6),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let work = fixture.store.get_work_item(work.work_id).unwrap();
    let recovered = claim(
        &mut fixture.store,
        &work,
        "runner",
        "reclaim-ended-root",
        7,
        3600,
    );
    assert_eq!(recovered.claim_id, fixture.binding.claim_id);
    let binding = current_binding(&fixture);
    assert!(
        applicability::measure_binding_destination(&fixture.store.connection, &binding).is_err()
    );
    healthy(&fixture.store);
}

#[test]
fn a_new_local_governing_revision_does_not_reuse_the_original_verdict() {
    let root = repository();
    let mut fixture = Fixture::new(root.path());
    let mut request = prepare(&mut fixture, root.path(), false);
    let work = fixture
        .store
        .get_work_item(fixture.binding.work_id)
        .unwrap();
    let actor = fixture.local_actor();
    let revised = fixture
        .store
        .revise_work(
            &ReviseWorkRequest {
                work_id: work.work_id,
                expected_revision: work.revision,
                patch: WorkRevisionPatch {
                    acceptance: Some(vec!["a changed local criterion".into()]),
                    ..WorkRevisionPatch::default()
                },
                authority: WorkPlanningAuthority::Claim {
                    run_id: fixture.binding.run_id,
                    holder: session(),
                    claim_id: fixture.binding.claim_id,
                    claim_fence: fixture.binding.claim_fence,
                },
                actor: actor.clone(),
                idempotency_key: "change-local-governing".into(),
                updated_at: at(30),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    request.binding.work_revision = revised.revision;
    request.decision = ObjectId::mint();
    let id = fixture
        .store
        .associate_imported_acceptance(&project(), &session(), &actor, &request, at(31))
        .unwrap();
    let summary = fixture
        .store
        .imported_acceptance_summary(&id, at(32))
        .unwrap();
    assert!(!summary.recorded_eligible);
    assert_eq!(
        summary.refusal,
        Some(AcceptanceUseRefusal::GoverningContentDiffers)
    );
    healthy(&fixture.store);
}
