use super::*;

#[test]
fn imported_planning_can_produce_an_onward_native_seal_without_promoting_restored_proofs() {
    for fault in ["none", "restored-seal", "restored-child", "native-event"] {
        let root = repository();
        let mut seed = Fixture::new(root.path());
        let mut origin = destination(&mut seed, root.path());
        assert!(
            origin
                .store
                .get_work_item(origin.binding.work_id)
                .unwrap()
                .restored
        );
        origin.observe(root.path(), 16);
        origin.request.completion.finished_at = Some(at(19));
        let receipt = origin.record(20);
        let check = &receipt.verifications[0].record_id;
        let work = origin.store.get_work_item(origin.binding.work_id).unwrap();
        let held = origin.store.claim_fixture(work.work_id);
        checkpoint(
            &mut origin.store,
            &work,
            &held,
            "runner",
            "onward-checkpoint",
            22,
            std::slice::from_ref(check),
        );
        // Exchange open planning first. A completed planning snapshot would
        // correctly refuse the destination's ordinary revision and claim.
        assert_eq!(work.lifecycle, WorkLifecycle::Open);
        let mut target = destination_at(&mut origin, root.path(), 31);
        target.observe(root.path(), 36);
        let mut completion =
            completion_request(&work, &held, "runner", check, "onward-completion", 25);
        completion.source_id = Some(source(root.path()));
        let seal = origin
            .store
            .complete_work(&completion, &DevelopmentNoopRedactor)
            .unwrap();
        assert!(!seal.restored);
        let seal_id = origin
            .store
            .get_work_run(seal.run_id)
            .unwrap()
            .completion_seal
            .unwrap();
        let original = object_on(&origin.store.connection, &seal_id)
            .unwrap()
            .unwrap()
            .1;
        let (packed, origin_id, cuts) = seal_pack(&origin, &seal_id, &seal, false);
        let records = packed
            .records()
            .iter()
            .map(|record| {
                let mut value: serde_json::Value =
                    serde_json::from_slice(record.canonical_bytes()).unwrap();
                if record.object_id() == &seal_id {
                    if fault == "restored-seal" {
                        value["restored"] = true.into();
                    }
                    if fault == "restored-child" {
                        value["restored_child_completions"] = serde_json::json!([seal_id]);
                    }
                }
                if fault == "native-event" && record.kind() == RecordKind::WorkEvent {
                    let event: WorkEvent = serde_json::from_value(value.clone()).unwrap();
                    if matches!(event.transition, WorkTransition::Completed { .. }) {
                        value["run_id"] = serde_json::json!(uuid::Uuid::new_v4());
                    }
                }
                RecordEnvelope::from_canonical(
                    record.origin().clone(),
                    record.object_id().clone(),
                    record.kind().as_str(),
                    serde_json_canonicalizer::to_vec(&value).unwrap(),
                )
                .unwrap()
            })
            .collect();
        retain(&mut target.store, &pack(&origin_id, records));
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
            .associate_imported_verification(&project(), &session(), &actor, &verification, at(40))
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
        let result = target.store.associate_imported_acceptance(
            &project(),
            &session(),
            &actor,
            &request,
            at(41),
        );
        if fault == "none" {
            let id = result.unwrap();
            let summary = target
                .store
                .imported_acceptance_summary(&id, at(42))
                .unwrap();
            assert!(summary.currently_eligible, "{summary:?}");
            assert_eq!(
                object_on(&target.store.connection, &seal_id)
                    .unwrap()
                    .unwrap()
                    .1,
                original
            );
            healthy(&target.store);
        } else if let Ok(id) = result {
            assert!(
                !target
                    .store
                    .imported_acceptance_summary(&id, at(42))
                    .unwrap()
                    .currently_eligible,
                "{fault}"
            );
        }
        assert!(
            !target
                .store
                .get_work_run(target.binding.run_id)
                .unwrap()
                .is_finished()
        );
    }
}
