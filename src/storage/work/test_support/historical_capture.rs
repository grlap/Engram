//! Stored historical evidence fixtures. This installs canonical data directly;
//! it implements no host admission, grant, checkpoint or recovery operation.

use super::*;
use crate::domain::{
    ControlWorkBinding, EnvironmentEvidence, ExecutionObservation, SourceChangeDetection,
    VerificationApplicability, VerificationEvidence,
};

use serde::{Deserialize, Deserializer, Serialize};

/// Input used only to construct historical execution-observation fixtures.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct ExecutionObservationInput {
    pub(crate) observation_id: String,
    pub(crate) action_fingerprint: ObjectId,
    pub(crate) effect: EffectClass,
    pub(crate) outcome: ExecutionOutcome,
    pub(crate) source_changed: bool,
    /// How the host established the change it reports; only with
    /// `source_changed`. Absent when the host does not say.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) reported_source_change: Option<SourceChangeDetection>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) source_basis: Option<ExecutionSourceBasis>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) observed_at: Option<DateTime<Utc>>,
}

/// Stable host-private reference to a previously recorded execution
/// observation or to one observation in the same checkpoint request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum ExecutionObservationReference {
    ObjectId { object_id: ObjectId },
    ObservationId { observation_id: String },
}

/// Historical verification fixture input referencing a stored producer.
///
/// The caller supplies classification and presentation only. Storage derives
/// the run, workspace/source basis, command fingerprint, result, producer
/// session, and timestamps from `producer_observation`.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VerificationEvidenceInput {
    #[serde(
        default,
        skip_serializing_if = "Option::is_none",
        deserialize_with = "present_applicability"
    )]
    pub(crate) applicability: Option<VerificationApplicability>,
    pub(crate) producer_observation: ExecutionObservationReference,
    pub(crate) check_kind: VerificationKind,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) environment: Option<EnvironmentEvidenceReference>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) summary: Option<String>,
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub(crate) refs: Vec<String>,
}

/// Host-private reference to environment evidence already stored or supplied
/// earlier in the same checkpoint request.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(tag = "kind", rename_all = "snake_case", deny_unknown_fields)]
pub(crate) enum EnvironmentEvidenceReference {
    ObjectId { object_id: ObjectId },
    Index { index: usize },
}

/// Host-private request to capture the environment identity used for one exact
/// run and content state. When `components` is present, storage derives and
/// compares `environment_fingerprint`; absence selects the opaque fingerprint
/// contract.
#[derive(Clone, Debug, Deserialize, Eq, PartialEq, Serialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct EnvironmentEvidenceInput {
    pub(crate) source_basis: ExecutionSourceBasis,
    pub(crate) environment_fingerprint: ObjectId,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub(crate) components: Option<EnvironmentComponents>,
    pub(crate) observed_at: DateTime<Utc>,
}

fn present_applicability<'de, D: Deserializer<'de>>(
    deserializer: D,
) -> Result<Option<VerificationApplicability>, D::Error> {
    VerificationApplicability::deserialize(deserializer).map(Some)
}

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

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn git_tree_optional_restriction_distinguishes_omission_from_present_null() {
        let old = json!({"producer_observation":{"kind":"observation_id","observation_id":"check"},"check_kind":"test"});
        let decoded: VerificationEvidenceInput = serde_json::from_value(old.clone()).unwrap();
        assert_eq!(
            crate::CanonicalObject::freeze(&old).unwrap().bytes(),
            crate::CanonicalObject::freeze(&decoded).unwrap().bytes()
        );
        let mut null = old.clone();
        null["applicability"] = serde_json::Value::Null;
        let error = serde_json::from_value::<VerificationEvidenceInput>(null)
            .unwrap_err()
            .to_string();
        assert!(error.contains("applicability"), "{error}");
        let duplicate = format!(
            "{{\"kind\":\"git_tree\",\"kind\":\"git_tree\",\"tree\":\"{}\"}}",
            "a".repeat(40)
        );
        assert!(
            serde_json::from_str::<VerificationApplicability>(&duplicate)
                .unwrap_err()
                .to_string()
                .contains("duplicate field")
        );
    }
}
