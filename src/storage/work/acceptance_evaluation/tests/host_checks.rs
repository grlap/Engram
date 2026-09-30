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
