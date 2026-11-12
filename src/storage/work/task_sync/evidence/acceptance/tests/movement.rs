use super::*;

fn retain_current(
    fixture: &mut Fixture,
    request: &AcceptanceUseRequest,
    records: Vec<RecordEnvelope>,
) {
    let pack = with_witnesses(fixture, &request.origin, records);
    retain(&mut fixture.store, &pack);
}

fn request_at_current_tail(
    fixture: &mut Fixture,
    request: &AcceptanceUseRequest,
    producer: &ObjectId,
) -> AcceptanceUseRequest {
    let pack = with_witnesses(
        fixture,
        &request.origin,
        vec![envelope(&fixture.store, &request.origin, producer)],
    );
    let cut = pack
        .records()
        .iter()
        .find(|record| record.kind() == RecordKind::OriginFeedCut)
        .unwrap()
        .object_id()
        .clone();
    retain(&mut fixture.store, &pack);
    let mut request = request.clone();
    request.origin_cuts = vec![cut];
    request
}

#[test]
fn historical_input_decision_cannot_hide_a_newer_failed_check_inside_its_cut() {
    let root = repository();
    let mut fixture = Fixture::new(root.path());
    let request = prepare(&mut fixture, root.path(), false);
    let HistoricalProof::Evaluation { evaluation, .. } = &request.proof else {
        panic!("evaluation fixture")
    };
    let original: AcceptanceEvaluation = history_object(&fixture, evaluation);
    let selected: VerificationEvidence = history_object(
        &fixture,
        &original.registered_input_decision.unwrap().verification,
    );
    let failed = record_later_check_with_fingerprint(
        &mut fixture,
        root.path(),
        true,
        selected.check_fingerprint,
    );
    let request = request_at_current_tail(&mut fixture, &request, &failed.operation);
    let cut = crate::storage::work::feeds::current_run_feed_cut_on(
        &fixture.store.connection,
        fixture.binding.run_id,
    )
    .unwrap();
    // A transported record cannot claim its old passing decision remains
    // valid at a cut that already contains the later failed check.
    let (pack, request) = later_evaluation(&fixture, &request, |record| {
        assert!(record.registered_input_decision.is_some());
        record.evaluated_cut = cut;
    });
    retain(&mut fixture.store, &pack);
    let before = crate::storage::test_database_shape_snapshot(&fixture.store.connection).unwrap();
    let actor = fixture.local_actor();
    let error = fixture
        .store
        .associate_imported_acceptance(&project(), &session(), &actor, &request, at(32))
        .unwrap_err();
    assert!(
        error.to_string().contains("failed registered check"),
        "{error}"
    );
    assert_eq!(
        before,
        crate::storage::test_database_shape_snapshot(&fixture.store.connection).unwrap()
    );
}

#[test]
fn retained_input_witness_requires_the_check_at_its_own_original_cut() {
    for after_cut in [false, true] {
        let root = repository();
        let mut fixture = Fixture::new(root.path());
        let mut request = prepare(&mut fixture, root.path(), false);
        let HistoricalProof::Evaluation { evaluation, .. } = &request.proof else {
            panic!("evaluation fixture")
        };
        let (_, original) = object_on(&fixture.store.connection, evaluation)
            .unwrap()
            .unwrap();
        let original: AcceptanceEvaluation = serde_json::from_slice(&original).unwrap();
        let mut witness = original.registered_input_decision.unwrap().verification;
        if after_cut {
            let later = record_later_check(&mut fixture, root.path(), false);
            witness = later.verifications[0].record_id.clone();
            request = request_at_current_tail(&mut fixture, &request, &later.operation);
        }
        let (pack, request) = later_evaluation(&fixture, &request, |record| {
            record.registered_input_decision = None;
            record.registered_input_witness = Some(witness.clone());
            if !record.evidence_basis.contains(&witness) {
                record.evidence_basis.push(witness);
            }
        });
        retain(&mut fixture.store, &pack);
        let before =
            crate::storage::test_database_shape_snapshot(&fixture.store.connection).unwrap();
        let actor = fixture.local_actor();
        let result = fixture.store.associate_imported_acceptance(
            &project(),
            &session(),
            &actor,
            &request,
            at(32),
        );
        if after_cut {
            assert!(result.is_err());
            assert_eq!(
                before,
                crate::storage::test_database_shape_snapshot(&fixture.store.connection).unwrap()
            );
        } else {
            let summary = fixture
                .store
                .imported_acceptance_summary(&result.unwrap(), at(33))
                .unwrap();
            assert!(
                summary.recorded_eligible && summary.currently_eligible,
                "{summary:?}"
            );
        }
    }
}

#[test]
fn later_shared_checks_follow_native_pass_exemptions_and_failed_check_barriers() {
    for failed in [false, true] {
        let root = repository();
        let mut fixture = Fixture::new(root.path());
        let request = prepare(&mut fixture, root.path(), false);
        let receipt = record_later_check(&mut fixture, root.path(), failed);
        assert_ne!(receipt.verifications.len(), 0);
        let native = fixture
            .store
            .acceptance_evaluation_status(fixture.binding.work_id, None)
            .unwrap()
            .unwrap();
        assert_eq!(native.stale.is_some(), failed, "native failed={failed}");
        let records = vec![envelope(
            &fixture.store,
            &request.origin,
            &receipt.operation,
        )];
        retain_current(&mut fixture, &request, records);
        // The original requested cut must not hide later retained execution.
        let actor = fixture.local_actor();
        let id = fixture
            .store
            .associate_imported_acceptance(&project(), &session(), &actor, &request, at(31))
            .unwrap();
        let summary = fixture
            .store
            .imported_acceptance_summary(&id, at(32))
            .unwrap();
        assert_eq!(summary.recorded_eligible, !failed, "{summary:?}");
        assert_eq!(
            summary.refusal,
            failed.then_some(AcceptanceUseRefusal::EvaluationBasisMoved)
        );
        healthy(&fixture.store);
    }
}

#[test]
fn committed_input_currency_survives_a_reported_change_then_revert() {
    let root = repository();
    let mut fixture = Fixture::new(root.path());
    let request = prepare(&mut fixture, root.path(), false);
    std::fs::write(root.path().join("file"), "changed source\n").unwrap();
    fixture.report_source(root.path(), 30, true);
    std::fs::write(root.path().join("file"), "initial\n").unwrap();
    fixture.observe(root.path(), 31);
    let native = fixture
        .store
        .acceptance_evaluation_status(fixture.binding.work_id, None)
        .unwrap()
        .unwrap();
    assert!(native.stale.is_none());
    retain_current(&mut fixture, &request, vec![]);
    let actor = fixture.local_actor();
    let id = fixture
        .store
        .associate_imported_acceptance(&project(), &session(), &actor, &request, at(32))
        .unwrap();
    let summary = fixture
        .store
        .imported_acceptance_summary(&id, at(33))
        .unwrap();
    assert!(summary.recorded_eligible, "{summary:?}");
    assert_eq!(summary.refusal, None);
    healthy(&fixture.store);
}

fn record_later_check(
    fixture: &mut Fixture,
    root: &std::path::Path,
    failed: bool,
) -> crate::domain::RegisteredVerificationReceipt {
    record_later_check_with_fingerprint(
        fixture,
        root,
        failed,
        ObjectId::from_canonical_bytes(b"cargo test --lib"),
    )
}

fn history_object<T: serde::de::DeserializeOwned>(fixture: &Fixture, id: &ObjectId) -> T {
    let (_, bytes) = object_on(&fixture.store.connection, id).unwrap().unwrap();
    serde_json::from_slice(&bytes).unwrap()
}

fn record_later_check_with_fingerprint(
    fixture: &mut Fixture,
    root: &std::path::Path,
    failed: bool,
    fingerprint: ObjectId,
) -> crate::domain::RegisteredVerificationReceipt {
    let outcome = if failed {
        RegisteredVerificationOutcome::Failed
    } else {
        RegisteredVerificationOutcome::Passed
    };
    let source_id = source(root);
    let request = RegisteredVerificationRequest {
        operation_key: "termal-run:test-later-native-check".into(),
        registration: RegisteredVerificationAssociation {
            host_run_id: "test-later-native-check".into(),
            request_identity: "later-native-request".into(),
            registered_at: at(26),
            initiating_session_id: session(),
            owning_session_id: None,
            binding: fixture.binding.clone(),
            root: RegisteredVerificationRoot {
                workspace_id: fixture.workspace.clone(),
                generation: 1,
                state: SourceRootState::Named,
            },
        },
        input: RegisteredVerificationInput::SourceId {
            source_id: source_id.clone(),
        },
        run: RegisteredVerificationRun {
            mode: "focused".into(),
            command: "cargo test --lib".into(),
            started_at: at(26),
            finished_at: at(29),
            outcome,
        },
        checks: vec![RegisteredVerificationCheck {
            stage: "focused".into(),
            check_kind: VerificationKind::Test,
            command: "cargo test --lib".into(),
            check_fingerprint: fingerprint,
            started_at: Some(at(27)),
            completed_at: Some(at(29)),
            outcome,
            summary: Some("later native check".into()),
            refs: vec![],
            counts: None,
        }],
        post_run: RegisteredVerificationPostRun::SourceId { source_id },
    };
    let actor = fixture.local_actor();
    fixture
        .store
        .record_registered_verification(
            &project(),
            &session(),
            &fixture.token,
            HostSessionAuthority::Work,
            &actor,
            &request,
            at(30),
        )
        .unwrap()
}

#[test]
fn report_based_shared_evaluation_keeps_an_accounted_change_barrier_after_revert() {
    let root = repository();
    let mut fixture = Fixture::with_bindings(root.path(), false);
    fixture.observe(root.path(), 5);
    let work = fixture
        .store
        .get_work_item(fixture.binding.work_id)
        .unwrap();
    let held = fixture.store.claim_fixture(work.work_id);
    let note = crate::storage::work::test_support::evidence(
        &mut fixture.store,
        &work,
        &held,
        "runner",
        "judged-note",
        10,
    );
    let evaluation = evaluate_basis(&mut fixture, root.path(), &note, AcceptanceBasis::Judgment);
    assert!(evaluation.record.registered_input_decision.is_none());
    assert!(evaluation.record.registered_input_witness.is_none());
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
    std::fs::write(root.path().join("file"), "changed source\n").unwrap();
    fixture.report_source(root.path(), 30, true);
    std::fs::write(root.path().join("file"), "initial\n").unwrap();
    fixture.observe(root.path(), 31);
    assert!(
        fixture
            .store
            .acceptance_evaluation_status(fixture.binding.work_id, None)
            .unwrap()
            .unwrap()
            .stale
            .is_some()
    );
    let pack = with_witnesses(
        &fixture,
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
    let request = AcceptanceUseRequest {
        decision: ObjectId::mint(),
        origin,
        proof: HistoricalProof::Evaluation {
            evaluation: evaluation.evaluation,
            governing_witness: witness_id,
        },
        origin_cuts: vec![cut],
        binding: fixture.binding.clone(),
        verification_decisions: vec![],
    };
    let actor = fixture.local_actor();
    let id = fixture
        .store
        .associate_imported_acceptance(&project(), &session(), &actor, &request, at(32))
        .unwrap();
    let summary = fixture
        .store
        .imported_acceptance_summary(&id, at(33))
        .unwrap();
    assert!(!summary.recorded_eligible);
    assert_eq!(summary.refusal, Some(AcceptanceUseRefusal::SourceDiffers));
    healthy(&fixture.store);
}
