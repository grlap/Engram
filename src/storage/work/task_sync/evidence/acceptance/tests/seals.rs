use super::*;
use crate::storage::work::test_support::{checkpoint, completion_request};

#[path = "seals/closure.rs"]
mod closure;
#[path = "seals/consumption.rs"]
mod consumption;
#[path = "seals/onward.rs"]
mod onward;

#[path = "seals/mark_history.rs"]
mod mark_history;

fn destination(origin: &mut Fixture, root: &std::path::Path) -> Fixture {
    destination_at(origin, root, 11)
}

fn destination_at(origin: &mut Fixture, root: &std::path::Path, start: i64) -> Fixture {
    let snapshot = origin
        .store
        .save_work_graph_snapshot(
            &project(),
            &actor("snapshot-author"),
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(start),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let mut store = SqliteStore::open_in_memory().unwrap();
    store
        .load_work_graph_snapshot(
            &project(),
            &actor("restore-author"),
            &serde_json::to_vec(&snapshot.document).unwrap(),
            false,
            at(start + 1),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let work = store.get_work_item(origin.binding.work_id).unwrap();
    assert!(work.restored);
    assert!(work.active_run_id.is_none());
    assert_eq!(work.acceptance_bindings.len(), 0);
    // Planning restore is inert. Author the actual destination requirement
    // through its ordinary revision API before starting its local run.
    let bindings = origin
        .store
        .get_work_item(origin.binding.work_id)
        .unwrap()
        .acceptance_bindings;
    let work = store
        .revise_work(
            &ReviseWorkRequest {
                work_id: work.work_id,
                expected_revision: work.revision,
                patch: WorkRevisionPatch {
                    acceptance_bindings: Some(bindings),
                    ..WorkRevisionPatch::default()
                },
                authority: WorkPlanningAuthority::Project,
                actor: actor("destination-author"),
                idempotency_key: "destination-bindings".into(),
                updated_at: at(start + 1),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    assert_ne!(
        work.created_at,
        origin.store.get_work_item(work.work_id).unwrap().created_at
    );
    let held = store
        .claim_work(
            &ClaimWorkRequest {
                work_id: work.work_id,
                expected_work_revision: work.revision,
                expected_run_id: None,
                holder: session(),
                ttl_seconds: 3600,
                recovery_reason: None,
                actor: actor("runner"),
                idempotency_key: "destination-claim".into(),
                claimed_at: at(start + 2),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let work = store.get_work_item(work.work_id).unwrap();
    let binding = ControlWorkBinding {
        root_execution_id: store.get_work_run(held.run_id).unwrap().root_execution_id,
        work_id: work.work_id,
        run_id: held.run_id,
        work_revision: work.revision,
        claim_id: held.claim_id,
        claim_fence: held.fence,
    };
    assert_ne!(binding.run_id, origin.binding.run_id);
    let token = store
        .resume_work_connection(&session(), at(start + 3))
        .unwrap();
    let workspace = root.to_string_lossy().into_owned();
    store
        .bind_named_root(
            &project(),
            &session(),
            &token,
            HostSessionAuthority::Work,
            binding.claim_id,
            binding.claim_fence,
            &workspace,
            1,
            at(start + 3),
            NamedRootBindingKind::Bound,
            None,
            &mut actor("runner"),
            "destination-root",
            at(start + 3),
        )
        .unwrap();
    let mut request = origin.request.clone();
    request.association.binding = binding.clone();
    Fixture {
        store,
        binding,
        request,
        token,
        workspace,
    }
}

fn seal_pack(
    origin: &Fixture,
    seal_id: &ObjectId,
    seal: &CompletionSeal,
    reverse: bool,
) -> (EvidencePack, ObjectId, Vec<ObjectId>) {
    let (id, enrollment) = enrollment();
    let mut records = vec![
        enrollment,
        envelope(&origin.store, &id, seal_id),
        envelope(&origin.store, &id, &seal.accepted_work_revision_hash),
    ];
    let (_, root) = crate::storage::work::query::load_root_execution_with_ref(
        &origin.store.connection,
        seal.root_execution_id,
    )
    .unwrap();
    let mut cursor = Some(root.head);
    while let Some(head) = cursor {
        let (_, bytes) = object_on(&origin.store.connection, &head).unwrap().unwrap();
        let delta: RootExecutionDelta = serde_json::from_slice(&bytes).unwrap();
        records.push(envelope(&origin.store, &id, &head));
        cursor = delta.predecessor;
    }
    // Remote producer bodies are not substituted by their carrier or association.
    for evidence in &seal.evidence {
        let (kind, bytes) = object_on(&origin.store.connection, evidence)
            .unwrap()
            .unwrap();
        if kind == "verification_evidence" {
            let verification: VerificationEvidence = serde_json::from_slice(&bytes).unwrap();
            let producer = envelope(&origin.store, &id, &verification.producer_observation);
            let record: RemoteVerificationAssociationRecord =
                serde_json::from_slice(producer.canonical_bytes()).unwrap();
            records.push(producer);
            records.push(envelope(
                &origin.store,
                &id,
                record.origin.as_ref().unwrap(),
            ));
        }
    }
    let mut records = with_feed_witnesses(
        origin,
        &id,
        &crate::FeedId::RunExecution(seal.run_id),
        records,
    );
    let mut children = seal.required_child_seals.clone();
    let mut visited = std::collections::BTreeSet::new();
    while let Some(child_id) = children.pop() {
        if !visited.insert(child_id.clone()) {
            continue;
        }
        let (_, bytes) = object_on(&origin.store.connection, &child_id)
            .unwrap()
            .unwrap();
        let child: CompletionSeal = serde_json::from_slice(&bytes).unwrap();
        records.push(envelope(&origin.store, &id, &child_id));
        records.push(envelope(
            &origin.store,
            &id,
            &child.accepted_work_revision_hash,
        ));
        records = with_feed_witnesses(origin, &id, &FeedId::RunExecution(child.run_id), records);
        children.extend(child.required_child_seals);
    }
    let rule_sets: Vec<_> = records
        .iter()
        .filter(|record| record.kind() == RecordKind::WorkObligation)
        .map(|record| {
            serde_json::from_slice::<WorkObligation>(record.canonical_bytes())
                .unwrap()
                .rule_set
        })
        .collect();
    for rule_set in rule_sets {
        if !records.iter().any(|record| record.object_id() == &rule_set) {
            records.push(envelope(&origin.store, &id, &rule_set));
        }
    }
    let mut records =
        with_feed_witnesses(origin, &id, &crate::FeedId::RootWork(seal.root_id), records);
    assert!(records.iter().any(|record| record.object_id() == seal_id));
    let cuts = records
        .iter()
        .filter(|record| record.kind() == RecordKind::OriginFeedCut)
        .map(|record| record.object_id().clone())
        .collect();
    if reverse {
        records.reverse();
    }
    (pack(&id, records), id, cuts)
}

#[test]
fn native_foreign_seal_is_applicable_without_completing_the_destination_run() {
    for reverse in [false, true] {
        let root = repository();
        let mut origin = Fixture::new(root.path());
        origin.observe(root.path(), 5);
        let receipt = origin.record(10);
        let mut target = destination(&mut origin, root.path());
        target.observe(root.path(), 16);
        let work = origin.store.get_work_item(origin.binding.work_id).unwrap();
        let held = origin.store.claim_fixture(work.work_id);
        let check = &receipt.verifications[0].record_id;
        checkpoint(
            &mut origin.store,
            &work,
            &held,
            "runner",
            "original-completion-checkpoint",
            12,
            std::slice::from_ref(check),
        );
        let mut input = completion_request(
            &work,
            &held,
            "runner",
            check,
            "original-native-completion",
            15,
        );
        input.source_id = Some(source(root.path()));
        let seal = origin
            .store
            .complete_work(&input, &DevelopmentNoopRedactor)
            .unwrap();
        let seal_id = origin
            .store
            .get_work_run(seal.run_id)
            .unwrap()
            .completion_seal
            .unwrap();
        let original_bytes = object_on(&origin.store.connection, &seal_id)
            .unwrap()
            .unwrap()
            .1;
        let (pack, origin_id, cuts) = seal_pack(&origin, &seal_id, &seal, reverse);
        retain(&mut target.store, &pack);
        let mut verification = intent(&target, check.clone(), origin_id.clone());
        verification.origin_cut = cuts
            .iter()
            .find(|id| {
                let (_, bytes) = object_on(&target.store.connection, id).unwrap().unwrap();
                let cut: crate::task_sync::evidence::witnesses::OriginFeedCut =
                    serde_json::from_slice(&bytes).unwrap();
                cut.source.feed == FeedId::RunExecution(seal.run_id)
            })
            .unwrap()
            .clone();
        verification.criterion = Some(1);
        let actor = target.local_actor();
        target
            .store
            .associate_imported_verification(&project(), &session(), &actor, &verification, at(21))
            .unwrap();
        let request = AcceptanceUseRequest {
            decision: ObjectId::mint(),
            origin: origin_id,
            proof: HistoricalProof::NativeCompletion {
                seal: seal_id.clone(),
                governing_witness: None,
            },
            origin_cuts: cuts,
            binding: target.binding.clone(),
            verification_decisions: vec![verification.decision],
        };
        let id = target
            .store
            .associate_imported_acceptance(&project(), &session(), &actor, &request, at(22))
            .unwrap();
        let summary = target
            .store
            .imported_acceptance_summary(&id, at(23))
            .unwrap();
        assert!(summary.currently_eligible, "{summary:?}");
        assert!(
            !target
                .store
                .get_work_run(target.binding.run_id)
                .unwrap()
                .is_finished()
        );
        assert_eq!(
            target
                .store
                .get_work_item(target.binding.work_id)
                .unwrap()
                .lifecycle,
            WorkLifecycle::Open
        );
        assert_eq!(
            object_on(&target.store.connection, &seal_id)
                .unwrap()
                .unwrap()
                .1,
            original_bytes
        );
        assert_eq!(
            target
                .store
                .associate_imported_acceptance(&project(), &session(), &actor, &request, at(24))
                .unwrap(),
            id
        );
        assert!(
            target
                .store
                .acceptance_evaluation_policy()
                .unwrap()
                .is_self_asserted()
        );
        let mut missing = request.clone();
        missing.decision = ObjectId::mint();
        missing.origin_cuts.retain(|id| {
            let (_, bytes) = object_on(&target.store.connection, id).unwrap().unwrap();
            let cut: crate::task_sync::evidence::witnesses::OriginFeedCut =
                serde_json::from_slice(&bytes).unwrap();
            matches!(cut.source.feed, crate::FeedId::RunExecution(_))
        });
        let refused = target
            .store
            .associate_imported_acceptance(&project(), &session(), &actor, &missing, at(25))
            .unwrap();
        assert_eq!(
            target
                .store
                .imported_acceptance_summary(&refused, at(26))
                .unwrap()
                .refusal,
            Some(AcceptanceUseRefusal::OriginContextUnavailable)
        );
        let before =
            crate::storage::test_database_shape_snapshot(&target.store.connection).unwrap();
        for wrong_generation in [false, true] {
            let mut wrong = seal.root_execution.clone();
            if wrong_generation {
                wrong.generation += 1;
            } else {
                wrong.root_id = crate::WorkId(uuid::Uuid::new_v4());
            }
            assert!(
                crate::storage::work::root_state::resolve_retained(&wrong, |id| {
                    let (kind, bytes) = object_on(&target.store.connection, id).unwrap().unwrap();
                    assert_eq!(kind, "work_root_delta");
                    Ok(serde_json::from_slice(&bytes)?)
                })
                .is_err()
            );
        }
        assert!(
            crate::storage::work::root_state::resolve_retained(&seal.root_execution, |id| {
                let (_, bytes) = object_on(&target.store.connection, id).unwrap().unwrap();
                let mut value: serde_json::Value = serde_json::from_slice(&bytes)?;
                value
                    .as_object_mut()
                    .unwrap()
                    .insert("unexpected".into(), serde_json::json!(true));
                Ok(value)
            })
            .is_err()
        );
        assert_eq!(
            before,
            crate::storage::test_database_shape_snapshot(&target.store.connection).unwrap()
        );
        // A reported checkpoint cannot stand in for the original native seal.
        let mut reported = request.clone();
        reported.decision = ObjectId::mint();
        reported.proof = HistoricalProof::NativeCompletion {
            seal: seal.checkpoint.clone().unwrap(),
            governing_witness: None,
        };
        let refused = target
            .store
            .associate_imported_acceptance(&project(), &session(), &actor, &reported, at(27))
            .unwrap();
        assert_eq!(
            target
                .store
                .imported_acceptance_summary(&refused, at(28))
                .unwrap()
                .refusal,
            Some(AcceptanceUseRefusal::ReportedCompletion)
        );
        let reopened = complete_and_reopen(&mut target, root.path());
        assert_ne!(reopened.run_id, request.binding.run_id);
        assert!(reopened.generation > seal.run_generation);
        let stale = target
            .store
            .imported_acceptance_summary(&id, at(38))
            .unwrap();
        assert!(!stale.currently_eligible);
        assert_eq!(
            stale.refusal,
            Some(AcceptanceUseRefusal::LocalGenerationDiffers)
        );
        // Historical proof and exact repeat stay unchanged after local reopen.
        assert_eq!(
            target
                .store
                .associate_imported_acceptance(&project(), &session(), &actor, &request, at(39))
                .unwrap(),
            id
        );
        assert_eq!(
            object_on(&target.store.connection, &seal_id)
                .unwrap()
                .unwrap()
                .1,
            original_bytes
        );
        healthy(&target.store);
        healthy(&origin.store);
    }
}

fn complete_and_reopen(target: &mut Fixture, root: &std::path::Path) -> WorkRun {
    target.request.operation_key = "destination-native-check".into();
    target.request.completion.run_id = "test-destination-generation".into();
    target.request.completion.finished_at = Some(at(29));
    let receipt = target.record(30);
    let evidence = &receipt.verifications[0].record_id;
    let work = target.store.get_work_item(target.binding.work_id).unwrap();
    let held = target.store.claim_fixture(work.work_id);
    checkpoint(
        &mut target.store,
        &work,
        &held,
        "runner",
        "destination-checkpoint",
        31,
        std::slice::from_ref(evidence),
    );
    let mut input = completion_request(
        &work,
        &held,
        "runner",
        evidence,
        "destination-completion",
        33,
    );
    input.source_id = Some(source(root));
    target
        .store
        .complete_work(&input, &DevelopmentNoopRedactor)
        .unwrap();
    let work = target.store.get_work_item(work.work_id).unwrap();
    target
        .store
        .reopen_work(
            &ReopenWorkRequest {
                work_id: work.work_id,
                expected_work_revision: work.revision,
                reason: "begin a fresh local generation".into(),
                actor: actor("human"),
                idempotency_key: "destination-reopen".into(),
                reopened_at: at(36),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap()
}
