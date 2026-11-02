//! Stored historical evidence fixtures. This installs canonical data directly;
//! it implements no host admission, grant, checkpoint or recovery operation.

use super::*;
use crate::domain::{
    ControlWorkBinding, EnvironmentEvidence, ExecutionObservation, VerificationEvidence,
};

#[derive(Clone, Debug)]
pub(crate) struct HistoricalCapture {
    pub(crate) project: ProjectId,
    pub(crate) session: SessionId,
    pub(crate) binding: ControlWorkBinding,
    pub(crate) capture_id: String,
}

#[derive(Clone, Debug, Eq, PartialEq, serde::Serialize)]
pub(crate) struct HistoricalEvidenceBatch {
    pub(crate) execution_observations: Vec<ObjectId>,
    pub(crate) verification_evidence: Vec<ObjectId>,
    pub(crate) environment_evidence: Vec<ObjectId>,
}

/// Install one historical batch without creating any operational turn state.
pub(crate) fn append_historical_capture(
    store: &mut SqliteStore,
    capture: &HistoricalCapture,
    observations: &[ExecutionObservationInput],
    verifications: &[VerificationEvidenceInput],
    environments: &[EnvironmentEvidenceInput],
    now: chrono::DateTime<chrono::Utc>,
) -> Result<HistoricalEvidenceBatch, StoreError> {
    let rules = active_rule_set_id(&store.connection);
    let tx = store.connection.transaction()?;
    let mut actor = actor(&capture.session.0);
    actor.run_id = Some(capture.binding.run_id.0.to_string());
    let mut result = HistoricalEvidenceBatch {
        execution_observations: vec![],
        verification_evidence: vec![],
        environment_evidence: vec![],
    };
    let mut stored = Vec::new();
    for input in observations {
        let observation = ExecutionObservation {
            schema_version: SCHEMA_VERSION,
            project_id: capture.project.clone(),
            binding: capture.binding.clone(),
            session_id: capture.session.clone(),
            grant_id: capture.capture_id.clone(),
            observation_id: input.observation_id.clone(),
            action_fingerprint: input.action_fingerprint.clone(),
            effect: input.effect,
            outcome: input.outcome,
            source_changed: input.source_changed
                && !super::super::completion::source_revision_repeats_on(
                    &tx,
                    capture.binding.run_id,
                    capture.binding.claim_id,
                    input.source_basis.as_ref(),
                )?,
            reported_source_change: input.reported_source_change,
            obligation_rule_set: rules.clone(),
            source_basis: input.source_basis.clone(),
            observed_at: input.observed_at,
            actor: actor.clone(),
            recorded_at: now,
        };
        let id =
            super::super::completion::append_control_execution_observation_on(&tx, &observation)?;
        result.execution_observations.push(id.clone());
        stored.push((id, observation));
    }
    let mut environment_records = Vec::new();
    for input in environments {
        let environment = EnvironmentEvidence {
            schema_version: SCHEMA_VERSION,
            project_id: capture.project.clone(),
            binding: capture.binding.clone(),
            session_id: capture.session.clone(),
            source_basis: input.source_basis.clone(),
            environment_fingerprint: input.environment_fingerprint.clone(),
            components: input.components.clone(),
            observed_at: input.observed_at,
            actor: actor.clone(),
            recorded_at: now,
        };
        let id =
            super::super::completion::append_control_environment_evidence_on(&tx, &environment)?;
        result.environment_evidence.push(id.clone());
        environment_records.push((id, environment));
    }
    for input in verifications {
        let (producer, observation) = match &input.producer_observation {
            ExecutionObservationReference::ObjectId { object_id } => (
                object_id.clone(),
                super::super::feeds::load_typed_work_object::<ExecutionObservation>(
                    &tx,
                    object_id,
                    "execution_observation",
                )?,
            ),
            ExecutionObservationReference::ObservationId { observation_id } => stored
                .iter()
                .find(|(_, observation)| &observation.observation_id == observation_id)
                .cloned()
                .expect("historical producer fixture"),
        };
        let source_basis = observation
            .source_basis
            .clone()
            .expect("historical measured check");
        let environment = match &input.environment {
            None => None,
            Some(EnvironmentEvidenceReference::ObjectId { object_id }) => Some(object_id.clone()),
            Some(EnvironmentEvidenceReference::Index { index }) => {
                Some(environment_records[*index].0.clone())
            }
        };
        let evidence = VerificationEvidence {
            schema_version: SCHEMA_VERSION,
            applicability: input.applicability.clone(),
            project_id: capture.project.clone(),
            binding: capture.binding.clone(),
            session_id: capture.session.clone(),
            producer_observation: producer,
            source_basis,
            environment,
            check_kind: input.check_kind,
            check_fingerprint: observation.action_fingerprint,
            result: match observation.outcome {
                ExecutionOutcome::Succeeded => VerificationResult::Passed,
                ExecutionOutcome::Failed => VerificationResult::Failed,
                ExecutionOutcome::Unknown => VerificationResult::Indeterminate,
            },
            completed_at: observation.observed_at.unwrap_or(observation.recorded_at),
            summary: input.summary.clone().unwrap_or_default(),
            refs: input.refs.clone(),
            actor: actor.clone(),
            recorded_at: now,
            bound_from: None,
        };
        result.verification_evidence.push(
            super::super::completion::append_control_verification_evidence_on(&tx, &evidence)?,
        );
    }
    tx.commit()?;
    Ok(result)
}
