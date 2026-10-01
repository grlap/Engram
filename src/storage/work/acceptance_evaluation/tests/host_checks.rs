//! The host test session's check turns: one checkpoint carrying an optional
//! source change and host-observed checks.

use super::*;

impl HostSession {
    /// One mutation turn: an optional source change plus an optional
    /// host-observed check. Returns the minted verification evidence ides.
    pub(super) fn checkpoint(
        &mut self,
        store: &mut SqliteStore,
        source_changed: bool,
        check: Option<(VerificationKind, ExecutionOutcome)>,
        second: i64,
    ) -> Vec<ObjectId> {
        self.checkpoint_checks(store, source_changed, check.as_slice(), second)
    }

    /// One mutation turn: an optional source change plus host-observed
    /// checks in order, all on the current basis and sharing one environment
    /// record, as a host reports several checks of one turn. Returns the
    /// minted verification evidence ids in order.
    pub(super) fn checkpoint_checks(
        &mut self,
        store: &mut SqliteStore,
        source_changed: bool,
        checks: &[(VerificationKind, ExecutionOutcome)],
        second: i64,
    ) -> Vec<ObjectId> {
        let grant = self.grant(store, &[EffectClass::MutateLocal], true, second);
        self.begin(store, &grant, second + 1);
        let mut observations = Vec::new();
        if source_changed {
            observations.push(ExecutionObservationInput {
                observation_id: self.key("source-mutation"),
                action_fingerprint: ObjectId::from_canonical_bytes(
                    self.key("write src").as_bytes(),
                ),
                effect: EffectClass::MutateLocal,
                outcome: ExecutionOutcome::Succeeded,
                source_changed: true,
                reported_source_change: None,
                source_basis: Some(self.basis.clone()),
                observed_at: Some(at(second + 1)),
            });
        }
        let mut verifications = Vec::new();
        let mut environments = Vec::new();
        for (index, (kind, outcome)) in checks.iter().copied().enumerate() {
            let observation_id = if index == 0 {
                self.key("check")
            } else {
                self.key(&format!("check-{index}"))
            };
            observations.push(ExecutionObservationInput {
                observation_id: observation_id.clone(),
                action_fingerprint: ObjectId::from_canonical_bytes(
                    self.key(&format!("run check {index}")).as_bytes(),
                ),
                effect: EffectClass::MutateLocal,
                outcome,
                source_changed: false,
                reported_source_change: None,
                source_basis: Some(self.basis.clone()),
                observed_at: Some(at(second + 1)),
            });
            verifications.push(VerificationEvidenceInput {
                producer_observation: ExecutionObservationReference::ObservationId {
                    observation_id,
                },
                check_kind: kind,
                environment: Some(EnvironmentEvidenceReference::Index { index: 0 }),
                summary: Some("host observed the check".into()),
                refs: vec!["command:check".into()],
            });
        }
        if !checks.is_empty() {
            let components = EnvironmentComponents {
                toolchain: "rustc-test".into(),
                sandbox: Some("test-host-sandbox".into()),
                workspace_id: self.basis.workspace_id.clone(),
                capability_map_revision: 1,
            };
            environments.push(EnvironmentEvidenceInput {
                source_basis: self.basis.clone(),
                environment_fingerprint: CanonicalObject::freeze(&components)
                    .expect("freeze environment components")
                    .key()
                    .clone(),
                components: Some(components),
                observed_at: at(second + 1),
            });
        }
        let checkpointed = store
            .checkpoint_control_turn_with_evidence(
                &self.project_id,
                &self.session_id,
                &self.connection_token,
                &self.routing_token,
                &grant.grant_id,
                TurnNextIntent::Continue,
                &observations,
                &verifications,
                &environments,
                &self.key("checkpoint"),
                at(second + 2),
            )
            .expect("checkpoint host turn with evidence");
        let ControlTurnCheckpointDecision::Checkpointed { receipt } = checkpointed else {
            panic!("host turn must checkpoint: {checkpointed:?}");
        };
        receipt.verification_evidence.clone()
    }
}

/// Every obligation row on `run`, every contribution of its root, and the
/// bytes of `seal`, as stored: what a late record must leave unchanged.
fn closed_state(
    store: &SqliteStore,
    run: WorkRunId,
    seal: &ObjectId,
) -> (Vec<crate::storage::WorkObligationRecord>, String, Vec<u8>) {
    let obligations = store.work_run_obligations(run).expect("stored obligations");
    let run = load_work_run(&store.connection, run).expect("run");
    let root =
        crate::storage::work::query::load_root_execution(&store.connection, run.root_execution_id)
            .expect("root execution");
    let seal_bytes: Vec<u8> = store
        .connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_id = ?1",
            [seal.as_str()],
            |row| row.get(0),
        )
        .expect("seal bytes");
    (obligations, format!("{root:?}"), seal_bytes)
}

// A turn begun while the run was live and checkpointed after the run
// completed reports a source change and a check. The late records are kept
// under their original binding and the turn closes, but they open no
// obligation, satisfy none and change neither the root's contributors nor its
// contributions; the seal is untouched and an exact replay returns the same
// receipt. A source change on the live run, before completion, still opens
// its obligation.
#[test]
fn a_late_checkpoint_on_a_completed_run_is_audit_only() {
    let mut fixture = fixture("project-late-checkpoint");
    let claim = fixture.claim.clone();
    let mut host = HostSession::bind(&mut fixture.store, &fixture.work.clone(), &claim, 5);
    // On the live run, a source change opens its obligation.
    let opened_before = fixture
        .store
        .work_run_obligations(claim.run_id)
        .expect("obligations")
        .len();
    host.checkpoint(&mut fixture.store, true, None, 10);
    let live = fixture
        .store
        .work_run_obligations(claim.run_id)
        .expect("obligations");
    assert_eq!(
        live.len(),
        opened_before + 1,
        "a live source change opens an obligation"
    );

    // A turn begun while the run is live...
    let grant = host.grant(&mut fixture.store, &[EffectClass::MutateLocal], true, 20);
    host.begin(&mut fixture.store, &grant, 21);
    // ...outlives the run's completion.
    let current = fixture
        .store
        .get_work_item(fixture.work.work_id)
        .expect("item");
    let sealed = checkpoint_then_complete(
        &mut fixture.store,
        &current,
        &claim,
        "runner",
        std::slice::from_ref(&fixture.evidence),
        true,
        None,
        "complete-before-late-turn",
        22,
    )
    .expect("complete");
    let seal = load_work_run(&fixture.store.connection, claim.run_id)
        .expect("run")
        .completion_seal
        .expect("a sealed run");
    // A store written before this guard may already hold an obligation a
    // late change opened on the finished run; a late check must not satisfy
    // it either.
    fixture.store.append_late_source_change_fixture(
        fixture.work.work_id,
        &claim,
        "historical-change",
        at(22),
        &host.basis.source_revision.clone(),
    );
    let before = closed_state(&fixture.store, claim.run_id, &seal);
    assert!(
        before
            .0
            .iter()
            .any(|row| row.state == crate::domain::WorkObligationState::Open),
        "an open historical obligation is present"
    );

    // The begun turn now reports a genuine source change, to a revision the
    // run has not seen, and a check of it.
    let late_basis = ExecutionSourceBasis {
        source_revision: "content-revision-late".into(),
        ..host.basis.clone()
    };
    let observations = vec![
        ExecutionObservationInput {
            observation_id: host.key("late-change"),
            action_fingerprint: ObjectId::from_canonical_bytes(b"write src late"),
            effect: EffectClass::MutateLocal,
            outcome: ExecutionOutcome::Succeeded,
            source_changed: true,
            reported_source_change: None,
            source_basis: Some(late_basis.clone()),
            observed_at: Some(at(23)),
        },
        ExecutionObservationInput {
            observation_id: host.key("late-check"),
            action_fingerprint: ObjectId::from_canonical_bytes(b"run tests late"),
            effect: EffectClass::MutateLocal,
            outcome: ExecutionOutcome::Succeeded,
            source_changed: false,
            reported_source_change: None,
            source_basis: Some(late_basis.clone()),
            observed_at: Some(at(23)),
        },
    ];
    let verifications = vec![VerificationEvidenceInput {
        producer_observation: ExecutionObservationReference::ObservationId {
            observation_id: host.key("late-check"),
        },
        check_kind: VerificationKind::Test,
        environment: Some(EnvironmentEvidenceReference::Index { index: 0 }),
        summary: Some("host observed a late check".into()),
        refs: vec!["command:test".into()],
    }];
    let environments = vec![host.environment(&late_basis.source_revision, 22)];
    let checkpoint = |store: &mut SqliteStore, key: &str| {
        store
            .checkpoint_control_turn_with_evidence(
                &host.project_id,
                &host.session_id,
                &host.connection_token,
                &host.routing_token,
                &grant.grant_id,
                TurnNextIntent::Continue,
                &observations,
                &verifications,
                &environments,
                key,
                at(24),
            )
            .expect("checkpoint the late turn")
    };
    let late_key = host.key("late-checkpoint");
    let first = checkpoint(&mut fixture.store, &late_key);
    let ControlTurnCheckpointDecision::Checkpointed { receipt } = &first else {
        panic!("the late turn must checkpoint: {first:?}");
    };
    assert_eq!(receipt.execution_observations.len(), 2);
    assert_eq!(receipt.verification_evidence.len(), 1);
    assert_eq!(receipt.environment_evidence.len(), 1);

    // Audit kept under the original binding, never rebound to another run,
    // with the source change recorded as a change at its new revision.
    let stored: Vec<crate::domain::ExecutionObservation> = receipt
        .execution_observations
        .iter()
        .map(|observation| {
            crate::storage::work::load_typed_work_object(
                &fixture.store.connection,
                observation,
                "execution_observation",
            )
            .expect("the late observation")
        })
        .collect();
    for observation in &stored {
        assert_eq!(observation.binding.run_id, claim.run_id);
        assert_eq!(observation.grant_id, grant.grant_id);
    }
    let change = stored
        .iter()
        .find(|observation| observation.observation_id == host.key("late-change"))
        .expect("the late change");
    assert!(
        change.source_changed,
        "the late change is recorded as a change"
    );
    assert_eq!(change.source_basis.as_ref(), Some(&late_basis));
    let check: VerificationEvidence = crate::storage::work::load_typed_work_object(
        &fixture.store.connection,
        &receipt.verification_evidence[0],
        "verification_evidence",
    )
    .expect("the late check");
    assert_eq!(check.binding.run_id, claim.run_id);

    // No obligation opened or satisfied, no root accounting changed, and the
    // seal is untouched.
    assert_eq!(closed_state(&fixture.store, claim.run_id, &seal), before);
    assert_eq!(
        load_work_run(&fixture.store.connection, claim.run_id)
            .expect("run")
            .completion_seal,
        Some(seal.clone())
    );
    let _ = sealed;
    // The integrity check accepts the late change without an obligation,
    // and the historical one with its full set.
    let report = fixture.store.verify_all().expect("integrity report");
    assert!(
        report.invalid_work_records.is_empty(),
        "{:?}",
        report.invalid_work_records
    );

    // Core inspect lists the run's obligations as before: none from the late
    // turn.
    let service = crate::work_service::LocalWorkService::new(
        fixture.directory.path().join("engram.sqlite3"),
        fixture.work.project_id.clone(),
        "inspector".into(),
        SessionId("late-checkpoint-inspector".into()),
        Some("protocol-test".into()),
    );
    let inspected = service
        .work_inspect(&fixture.work.work_id.0.to_string(), at(25))
        .expect("inspect");
    let listed: Vec<_> = inspected
        .view()
        .obligation_page
        .items
        .iter()
        .map(|item| item.obligation_id)
        .collect();
    let stored: Vec<_> = before
        .0
        .iter()
        .map(|row| row.obligation.obligation_id)
        .collect();
    assert!(
        listed.iter().all(|id| stored.contains(id)),
        "inspect lists no obligation the late turn opened: {listed:?}"
    );
    assert!(inspected.view().obligation_page.historical);
    assert_eq!(inspected.view().obligation_page.open_total, Some(0));

    // An exact replay returns the same receipt and writes nothing more.
    let replay = checkpoint(&mut fixture.store, &late_key);
    assert_eq!(replay, first);
    assert_eq!(closed_state(&fixture.store, claim.run_id, &seal), before);
    // The turn closed: the grant checkpoints nothing more.
    let again = checkpoint(&mut fixture.store, &host.key("late-checkpoint-again"));
    assert!(
        !matches!(again, ControlTurnCheckpointDecision::Checkpointed { .. }),
        "{again:?}"
    );

    // The allowance covers only what was recorded after the seal: the live
    // change's obligation lost from the projection is still a damaged store.
    let live_obligation = live.last().expect("the live obligation");
    fixture
        .store
        .connection
        .execute(
            "DELETE FROM work_run_obligations WHERE obligation_id = ?1",
            [live_obligation.obligation.obligation_id.0.to_string()],
        )
        .expect("drop the live obligation row");
    let damaged = fixture.store.work_run_obligations(claim.run_id);
    assert!(
        matches!(&damaged, Err(StoreError::InvalidWorkProjection(message)) if message.contains("matching builtin obligation definitions")),
        "{damaged:?}"
    );
    let report = fixture.store.verify_all().expect("integrity report");
    assert!(
        report
            .invalid_work_records
            .iter()
            .any(|record| record.ends_with(":missing_definition")),
        "{:?}",
        report.invalid_work_records
    );
}
