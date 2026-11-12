//! Mark authorship belongs to the item; holder affiliation belongs to one run.
use super::*;
use crate::storage::work::task_sync::evidence::native_capture::{
    NativeEvidenceCaptureRequest, NativeEvidenceFeed,
};

fn marked(root: &std::path::Path, home: &std::path::Path) -> Fixture {
    let mut store =
        crate::storage::work::task_sync::evidence::test_support::project_store(home, &project());
    let mut request = root_request(&project().0, "outside-mark", 1);
    request.evaluation_mode = Some(AcceptanceEvaluationMode::SameSession);
    request.actor = actor("outside-planner");
    let work = store
        .create_work(&request, &DevelopmentNoopRedactor)
        .unwrap();
    let mut fixture = Fixture::with_work(root, store, &work);
    fixture
        .store
        .set_acceptance_evaluation_policy(
            &AcceptanceEvaluationPolicy {
                allowed_modes: vec![
                    AcceptanceEvaluationMode::SameSession,
                    AcceptanceEvaluationMode::IndependentSession,
                ],
                mechanical_basis: MechanicalBasis::Observed,
                require_source_freshness: true,
            },
            &actor("policy-admin"),
            "marked-policy",
            None,
            at(6),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    fixture
}

fn evaluate_marked(fixture: &mut Fixture, root: &std::path::Path, second: i64) -> ObjectId {
    fixture.observe(root, second);
    fixture.request.operation_key = format!("check-{second}");
    fixture.request.completion.run_id = format!("test-check-{second}");
    fixture.request.completion.finished_at = Some(at(second + 1));
    let receipt = fixture.record(second + 2);
    let check = &receipt.verifications[0].record_id;
    let work = fixture
        .store
        .get_work_item(fixture.binding.work_id)
        .unwrap();
    let held = fixture.store.claim_fixture(work.work_id);
    checkpoint(
        &mut fixture.store,
        &work,
        &held,
        "runner",
        &format!("checkpoint-{second}"),
        second + 3,
        std::slice::from_ref(check),
    );
    let cut = fixture
        .store
        .work_feed_head(&FeedId::RunExecution(fixture.binding.run_id))
        .unwrap();
    fixture
        .store
        .record_acceptance_evaluation_registering(
            &RecordAcceptanceEvaluationRequest {
                project_id: project(),
                work_id: work.work_id,
                expected_work_revision: work.revision,
                evaluated_through: cut,
                mode: AcceptanceEvaluationMode::SameSession,
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
                    rationale: "original marked evaluation".into(),
                    evidence: vec![check.clone()],
                }],
                evaluator: actor("runner"),
                attempt_key: Some(format!("marked-evaluation-{second}")),
                recorded_at: at(second + 4),
                supersedes: None,
            },
            &DevelopmentNoopRedactor,
            Some((&session(), at(second + 4))),
        )
        .expect("native evaluation accepts the outside author's unchanged mark")
        .evaluation
}

fn captured_use(
    fixture: &mut Fixture,
    evaluation: &ObjectId,
    runs: &[WorkRunId],
    origin: &ObjectId,
) -> AcceptanceUseRequest {
    let feeds = std::iter::once(FeedId::Project(project()))
        .chain(std::iter::once(FeedId::RootWork(fixture.binding.work_id)))
        .chain(runs.iter().copied().map(FeedId::RunExecution))
        .map(|feed| NativeEvidenceFeed {
            through: fixture.store.work_feed_head(&feed).unwrap(),
            feed,
        })
        .collect();
    let request = NativeEvidenceCaptureRequest {
        capture: ObjectId::mint(),
        origin: origin.clone(),
        profile: crate::task_sync::progress::ExportProfile {
            include_notes: true,
            origin_machine: Some("mark-history".into()),
        },
        roots: vec![evaluation.clone()],
        feeds,
        maximum_pack_records: 128,
    };
    let captured = fixture
        .store
        .capture_native_evidence(
            &project(),
            &SessionId("collector".into()),
            &actor("collector"),
            &request,
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let page = fixture
        .store
        .export_native_evidence_capture(&project(), &captured.cursor, 128, &request.profile)
        .unwrap();
    assert!(page.next.is_none());
    let witness = page
        .packs
        .iter()
        .flat_map(EvidencePack::records)
        .find(|record| {
            record.kind() == RecordKind::GoverningSnapshotWitness
                && serde_json::from_slice::<GoverningSnapshotWitness>(record.canonical_bytes())
                    .unwrap()
                    .basis
                    .evaluation
                    == *evaluation
        })
        .unwrap()
        .object_id()
        .clone();
    let admissions = page
        .packs
        .iter()
        .map(|_| ObjectId::mint())
        .collect::<Vec<_>>();
    fixture
        .store
        .retain_evidence_batch(
            &project(),
            &page.packs.iter().zip(&admissions).collect::<Vec<_>>(),
        )
        .unwrap();
    AcceptanceUseRequest {
        decision: ObjectId::mint(),
        origin: origin.clone(),
        proof: HistoricalProof::Evaluation {
            evaluation: evaluation.clone(),
            governing_witness: witness,
        },
        origin_cuts: captured.feed_cuts,
        binding: fixture.binding.clone(),
        verification_decisions: Vec::new(),
    }
}

fn summary(
    fixture: &mut Fixture,
    request: &AcceptanceUseRequest,
    second: i64,
) -> crate::task_sync::evidence::acceptance::AcceptanceUseSummary {
    let id = fixture
        .store
        .associate_imported_acceptance(
            &project(),
            &session(),
            &fixture.local_actor(),
            request,
            at(second),
        )
        .unwrap();
    fixture
        .store
        .imported_acceptance_summary(&id, at(second + 1))
        .unwrap()
}

fn reopen(fixture: &mut Fixture, root: &std::path::Path, evaluation: &ObjectId) -> WorkRun {
    let work = fixture
        .store
        .get_work_item(fixture.binding.work_id)
        .unwrap();
    let held = fixture.store.claim_fixture(work.work_id);
    let (_, bytes) = object_on(&fixture.store.connection, evaluation)
        .unwrap()
        .unwrap();
    let first_record: AcceptanceEvaluation = serde_json::from_slice(&bytes).unwrap();
    checkpoint(
        &mut fixture.store,
        &work,
        &held,
        "runner",
        "final-marked-checkpoint",
        24,
        &first_record.verdicts[0].evidence,
    );
    let mut completion = completion_request(
        &work,
        &held,
        "runner",
        &first_record.verdicts[0].evidence[0],
        "complete-marked",
        25,
    );
    completion.source_id = Some(source(root));
    completion.acceptance.clear();
    let sealed = fixture
        .store
        .complete_work(&completion, &DevelopmentNoopRedactor)
        .unwrap();
    assert_eq!(sealed.acceptance_evaluation.as_ref(), Some(evaluation));
    let work = fixture.store.get_work_item(work.work_id).unwrap();
    let reopened = fixture
        .store
        .reopen_work(
            &ReopenWorkRequest {
                work_id: work.work_id,
                expected_work_revision: work.revision,
                reason: "another run with the same mark".into(),
                actor: actor("reopener"),
                idempotency_key: "reopen-marked".into(),
                reopened_at: at(30),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    assert_ne!(fixture.binding.run_id, reopened.run_id);
    let work = fixture.store.get_work_item(work.work_id).unwrap();
    let held = claim(
        &mut fixture.store,
        &work,
        "runner",
        "claim-reopened",
        31,
        3600,
    );
    fixture.binding = ControlWorkBinding {
        root_execution_id: reopened.root_execution_id,
        work_id: work.work_id,
        run_id: reopened.run_id,
        work_revision: work.revision,
        claim_id: held.claim_id,
        claim_fence: held.fence,
    };
    fixture
        .store
        .bind_named_root(
            &project(),
            &session(),
            &fixture.token,
            HostSessionAuthority::Work,
            held.claim_id,
            held.fence,
            &fixture.workspace,
            2,
            at(32),
            NamedRootBindingKind::Bound,
            None,
            &mut actor("runner"),
            "named-reopened",
            at(32),
        )
        .unwrap();
    fixture.request.association.binding = fixture.binding.clone();
    fixture.request.association.root.generation = 2;
    reopened
}

#[test]
fn an_unchanged_mark_keeps_its_outside_author_when_an_evaluated_task_reopens() {
    let root = repository();
    let home = crate::test_support::temp_home().unwrap();
    let mut fixture = marked(root.path(), home.path());
    let enrolled = fixture
        .store
        .enroll_task_sync_origin(
            &project(),
            &actor("collector"),
            &crate::task_sync::origin::EnrollRequest {
                operation: uuid::Uuid::new_v4(),
                predecessor: None,
            },
            at(7),
        )
        .unwrap();
    let first_run = fixture.binding.run_id;
    let first = evaluate_marked(&mut fixture, root.path(), 10);
    let before = captured_use(&mut fixture, &first, &[first_run], &enrolled.origin_id);
    let accepted = summary(&mut fixture, &before, 22);
    assert!(accepted.currently_eligible, "before reopen: {accepted:?}");
    let reopened = reopen(&mut fixture, root.path(), &first);
    let second = evaluate_marked(&mut fixture, root.path(), 40);
    let after = captured_use(
        &mut fixture,
        &second,
        &[first_run, reopened.run_id],
        &enrolled.origin_id,
    );

    // A run witness alone cannot prove who set a mark carried from an earlier
    // run. Native objects and other retained origins are no substitute.
    let mut missing = after.clone();
    missing.decision = ObjectId::mint();
    missing.origin_cuts.retain(|id| {
        let (_, bytes) = object_on(&fixture.store.connection, id).unwrap().unwrap();
        let cut: crate::task_sync::evidence::witnesses::OriginFeedCut =
            serde_json::from_slice(&bytes).unwrap();
        cut.source.feed != FeedId::Project(project())
    });
    assert!(
        !summary(&mut fixture, &missing, 62).currently_eligible,
        "missing origin-qualified item history must refuse"
    );
    let accepted = summary(&mut fixture, &after, 64);
    assert!(
        accepted.currently_eligible,
        "reopened evaluation must retain the outside mark author: {accepted:?}"
    );
    healthy(&fixture.store);
}
