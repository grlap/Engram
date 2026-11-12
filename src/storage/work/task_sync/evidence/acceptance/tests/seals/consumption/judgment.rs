//! A native judged pass that cites an imported consumption stays reusable after
//! the next exchange, with the consumption's provenance still required.
use super::*;
use crate::task_sync::evidence::consumption::{
    CONSUMPTION_KIND, ConsumedProof, ImportedEvidenceConsumption,
};

fn judge(
    target: &mut Fixture,
    root: &std::path::Path,
    cited: &ObjectId,
) -> crate::storage::work::acceptance_evaluation::AcceptanceEvaluationReceipt {
    enable(&mut target.store);
    let cut = crate::storage::work::feeds::current_run_feed_cut_on(
        &target.store.connection,
        target.binding.run_id,
    )
    .unwrap();
    target
        .store
        .record_acceptance_evaluation_registering(
            &RecordAcceptanceEvaluationRequest {
                project_id: project(),
                work_id: target.binding.work_id,
                expected_work_revision: target.binding.work_revision,
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
                    basis: AcceptanceBasis::Judgment,
                    rationale: "judged the consumed imported check".into(),
                    evidence: vec![cited.clone()],
                }],
                evaluator: actor("reviewer"),
                attempt_key: Some("judged-consumption".into()),
                recorded_at: at(30),
                supersedes: None,
            },
            &DevelopmentNoopRedactor,
            Some((&SessionId("reviewer".into()), at(29))),
        )
        .unwrap()
}

/// Carry a later retained export of the same origin: a changed copy of the
/// consumption, then an evaluation that cites that copy instead.
fn forged_citation(
    target: &Fixture,
    request: &AcceptanceUseRequest,
    consumption: &ObjectId,
    change: impl FnOnce(&mut ImportedEvidenceConsumption),
) -> (EvidencePack, AcceptanceUseRequest) {
    use crate::task_sync::evidence::witnesses::{FeedMember, OriginFeedCut, OriginFeedSegment};
    let load = |id: &ObjectId| object_on(&target.store.connection, id).unwrap().unwrap().1;
    let HistoricalProof::Evaluation { evaluation, .. } = &request.proof else {
        panic!("evaluation fixture")
    };
    let prior: OriginFeedCut = serde_json::from_slice(&load(&request.origin_cuts[0])).unwrap();
    let mut copy: ImportedEvidenceConsumption = serde_json::from_slice(&load(consumption)).unwrap();
    let copy_id = ObjectId::mint();
    copy.request.consumption = copy_id.clone();
    change(&mut copy);
    let mut record: AcceptanceEvaluation = serde_json::from_slice(&load(evaluation)).unwrap();
    record.verdicts[0].evidence = vec![copy_id.clone()];
    record.evidence_basis.push(copy_id.clone());
    record.evaluated_cut.position = prior.through + 1;
    let id = ObjectId::mint();
    let entries = vec![
        FeedMember {
            position: prior.through + 1,
            object_kind: CONSUMPTION_KIND.into(),
            object_id: copy_id.clone(),
        },
        FeedMember {
            position: prior.through + 2,
            object_kind: "acceptance_evaluation".into(),
            object_id: id.clone(),
        },
    ];
    let segment_id = ObjectId::mint();
    let segment = OriginFeedSegment {
        source: prior.source.clone(),
        from_exclusive: prior.through,
        through: prior.through + 2,
        predecessor: prior.segments.last().cloned(),
        anchor: prior.endpoint,
        entries: entries.clone(),
    };
    let cut_id = ObjectId::mint();
    let cut = OriginFeedCut {
        source: prior.source,
        through: prior.through + 2,
        endpoint: entries.last().cloned(),
        segments: vec![segment_id.clone()],
    };
    let witness_id = ObjectId::mint();
    let witness = GoverningSnapshotWitness {
        basis: GoverningBasis {
            evaluation: id.clone(),
            project_id: project(),
            work_id: record.work_id,
            work_revision: record.work_revision,
            content_fingerprint: record.work_revision_hash.clone(),
        },
        snapshot: SnapshotAvailability::Exact {
            snapshot: Box::new(target.store.get_work_item(record.work_id).unwrap()),
            source_snapshot: None,
        },
    };
    let records = [
        (
            copy_id,
            CONSUMPTION_KIND,
            serde_json_canonicalizer::to_vec(&copy).unwrap(),
        ),
        (
            id.clone(),
            RecordKind::AcceptanceEvaluation.as_str(),
            serde_json_canonicalizer::to_vec(&record).unwrap(),
        ),
        (
            witness_id.clone(),
            RecordKind::GoverningSnapshotWitness.as_str(),
            serde_json_canonicalizer::to_vec(&witness).unwrap(),
        ),
        (
            segment_id,
            RecordKind::OriginFeedSegment.as_str(),
            serde_json_canonicalizer::to_vec(&segment).unwrap(),
        ),
        (
            cut_id.clone(),
            RecordKind::OriginFeedCut.as_str(),
            serde_json_canonicalizer::to_vec(&cut).unwrap(),
        ),
    ]
    .into_iter()
    .map(|(id, kind, bytes)| {
        RecordEnvelope::from_canonical(request.origin.clone(), id, kind, bytes).unwrap()
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

fn assert_not_reused(data: &mut Selected, request: &AcceptanceUseRequest, case: &str) {
    let actor = data.target.local_actor();
    let before =
        crate::storage::test_database_shape_snapshot(&data.target.store.connection).unwrap();
    match data.target.store.associate_imported_acceptance(
        &project(),
        &session(),
        &actor,
        request,
        at(33),
    ) {
        Ok(id) => {
            let summary = data
                .target
                .store
                .imported_acceptance_summary(&id, at(34))
                .unwrap();
            assert!(!summary.recorded_eligible, "{case}: {summary:?}");
            assert_eq!(
                summary.refusal,
                Some(AcceptanceUseRefusal::CitationIneligible),
                "{case}"
            );
        }
        // A refused association leaves the store as it was.
        Err(_) => assert_eq!(
            before,
            crate::storage::test_database_shape_snapshot(&data.target.store.connection).unwrap(),
            "{case}"
        ),
    }
}

#[test]
fn a_judged_pass_citing_an_imported_consumption_is_reused_after_exchange() {
    for case in ["valid", "missing provenance", "other run", "foreign cut"] {
        let mut data = selected_bindings(false, false);
        let consumed = consume(&mut data.target, &data.verification.decision, false);
        let evaluation = judge(&mut data.target, data.root.path(), &consumed.consumption);
        let request = retained_evaluation(&mut data.target, &evaluation);
        let original = object_on(&data.target.store.connection, &consumed.consumption).unwrap();
        match case {
            "valid" => {
                let actor = data.target.local_actor();
                let id = data
                    .target
                    .store
                    .associate_imported_acceptance(&project(), &session(), &actor, &request, at(31))
                    .unwrap();
                let summary = data
                    .target
                    .store
                    .imported_acceptance_summary(&id, at(32))
                    .unwrap();
                assert!(
                    summary.recorded_eligible && summary.currently_eligible,
                    "{summary:?}"
                );
                // The reused judged pass is consumed after the exchange.
                let used = consume(&mut data.target, &id, true);
                let (kind, _) = object_on(&data.target.store.connection, &used.consumption)
                    .unwrap()
                    .unwrap();
                assert_eq!(kind, CONSUMPTION_KIND);
                healthy(&data.target.store);
            }
            "missing provenance" => {
                // The consumed original loses its witnessed origin membership.
                data.target
                    .store
                    .connection
                    .execute(
                        "DELETE FROM work_task_sync_evidence_feed_members WHERE object_id=?1",
                        [data.verification.original.as_str()],
                    )
                    .unwrap();
                assert_not_reused(&mut data, &request, case);
            }
            _ => {
                let other_run = case == "other run";
                let scope = request.origin_cuts[0].clone();
                let (pack, forged) =
                    forged_citation(&data.target, &request, &consumed.consumption, |copy| {
                        if other_run {
                            copy.request.binding.run_id = WorkRunId(uuid::Uuid::new_v4());
                        } else if let ConsumedProof::Verification { origin_cut, .. } =
                            &mut copy.proof
                        {
                            // A real cut, but of another origin's feed.
                            *origin_cut = scope;
                        }
                    });
                retain(&mut data.target.store, &pack);
                assert_not_reused(&mut data, &forged, case);
            }
        }
        assert_eq!(
            object_on(&data.target.store.connection, &consumed.consumption).unwrap(),
            original,
            "{case}"
        );
    }
}
