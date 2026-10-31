//! One typed reader for the two genuine verification producer kinds.

use super::feeds::load_typed_work_object;
use crate::domain::{
    ExecutionObservation, ExecutionOutcome, ExecutionSourceBasis, RegisteredHostRun,
    RegisteredVerificationInput, RegisteredVerificationOutcome, VerificationEvidence,
    VerificationProducer, VerificationProducerFacts, VerificationResult,
};
use crate::storage::StoreError;
use rusqlite::{Connection, params};

pub(in crate::storage) fn load_verification_producer_on(
    connection: &Connection,
    evidence: &VerificationEvidence,
) -> Result<VerificationProducerFacts, StoreError> {
    let id = &evidence.producer_observation;
    let kind: String = connection.query_row(
        "SELECT object_kind FROM objects WHERE object_id = ?1",
        params![id.as_str()],
        |row| row.get(0),
    )?;
    match kind.as_str() {
        "execution_observation" => Ok(load_typed_work_object::<ExecutionObservation>(
            connection,
            id,
            "execution_observation",
        )?
        .verification_facts()),
        "registered_host_run" => {
            let producer: RegisteredHostRun =
                load_typed_work_object(connection, id, "registered_host_run")?;
            let check = producer
                .request
                .checks
                .iter()
                .find(|check| {
                    check.check_kind == evidence.check_kind
                        && check.check_fingerprint == evidence.check_fingerprint
                        && check.completed_at == Some(evidence.completed_at)
                        && matches!(
                            (check.outcome, evidence.result),
                            (
                                RegisteredVerificationOutcome::Passed,
                                VerificationResult::Passed
                            ) | (
                                RegisteredVerificationOutcome::Failed,
                                VerificationResult::Failed
                            ) | (
                                RegisteredVerificationOutcome::Indeterminate,
                                VerificationResult::Indeterminate
                            )
                        )
                })
                .ok_or_else(|| {
                    StoreError::InvalidWorkProjection(format!(
                        "verification has no matching terminal check in producer {id}"
                    ))
                })?;
            let outcome = match check.outcome {
                RegisteredVerificationOutcome::Passed => ExecutionOutcome::Succeeded,
                RegisteredVerificationOutcome::Failed => ExecutionOutcome::Failed,
                RegisteredVerificationOutcome::Indeterminate => ExecutionOutcome::Unknown,
                RegisteredVerificationOutcome::NotRun => {
                    return Err(StoreError::InvalidWorkProjection(
                        "a not-run check cannot be verification evidence".into(),
                    ));
                }
            };
            let root = &producer.request.registration.root;
            Ok(VerificationProducerFacts {
                project_id: producer.project_id,
                binding: producer.request.registration.binding,
                session_id: producer.session_id,
                action_fingerprint: check.check_fingerprint.clone(),
                outcome,
                run_outcome: Some(match producer.request.run.outcome {
                    RegisteredVerificationOutcome::Passed => ExecutionOutcome::Succeeded,
                    RegisteredVerificationOutcome::Failed => ExecutionOutcome::Failed,
                    RegisteredVerificationOutcome::Indeterminate
                    | RegisteredVerificationOutcome::NotRun => ExecutionOutcome::Unknown,
                }),
                source_basis: Some(ExecutionSourceBasis {
                    git_tree: match &producer.request.input {
                        RegisteredVerificationInput::StagedTree { tree, .. } => Some(tree.clone()),
                        RegisteredVerificationInput::Commit { .. } => None,
                    },
                    workspace_id: root.workspace_id.clone(),
                    source_revision: producer.request.input.revision(),
                    source_root_generation: Some(root.generation),
                    source_root_state: Some(root.state),
                }),
                observed_at: check.completed_at,
                actor: producer.actor,
                recorded_at: producer.recorded_at,
            })
        }
        _ => Err(StoreError::InvalidWorkProjection(format!(
            "verification producer {id} has unsupported kind {kind}"
        ))),
    }
}
