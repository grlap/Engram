//! Grant-free settlement of the embedding host's registered test run.

use super::{CanonicalObject, SqliteStore, StoreError};
use crate::domain::{RegisteredVerificationReceipt, RegisteredVerificationRequest};
use crate::{ActorContext, ProjectId, SessionId};
use chrono::{DateTime, Utc};

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    control_schema_version: u16,
    session_id: SessionId,
    project_id: ProjectId,
    idempotency_key: String,
    request: RegisteredVerificationRequest,
}

pub(super) fn row_matches(
    connection: &rusqlite::Connection,
    stored_session: &str,
    intent: serde_json::Value,
    result: serde_json::Value,
) -> Result<bool, StoreError> {
    let (Ok(intent), Ok(receipt)) = (
        serde_json::from_value::<Intent>(intent),
        serde_json::from_value::<RegisteredVerificationReceipt>(result),
    ) else {
        return Ok(false);
    };
    if intent.control_schema_version != crate::CONTROL_SCHEMA_VERSION
        || intent.session_id.0 != stored_session
        || intent.idempotency_key != intent.request.operation_key
        || receipt.operation != receipt.producer
        || receipt.run_outcome != intent.request.run.outcome
    {
        return Ok(false);
    }
    let producer: crate::domain::RegisteredHostRun =
        super::work::load_typed_work_object(connection, &receipt.producer, "registered_host_run")?;
    if producer.project_id != intent.project_id
        || producer.session_id != intent.session_id
        || producer.request != intent.request
    {
        return Ok(false);
    }
    let expected: Vec<_> = producer
        .request
        .checks
        .iter()
        .filter(|check| {
            check.completed_at.is_some()
                && check.outcome != crate::domain::RegisteredVerificationOutcome::NotRun
        })
        .collect();
    if receipt.verifications.len() != expected.len() {
        return Ok(false);
    }
    let mut ids = std::collections::HashSet::new();
    for (entry, check) in receipt.verifications.iter().zip(expected) {
        if entry.stage != check.stage || !ids.insert(&entry.record_id) {
            return Ok(false);
        }
        let evidence: crate::domain::VerificationEvidence = super::work::load_typed_work_object(
            connection,
            &entry.record_id,
            "verification_evidence",
        )?;
        if evidence.producer_observation != receipt.producer
            || evidence.check_kind != check.check_kind
            || evidence.check_fingerprint != check.check_fingerprint
            || evidence.completed_at != check.completed_at.unwrap_or(producer.recorded_at)
        {
            return Ok(false);
        }
        super::work::load_verification_producer_on(connection, &evidence)?;
    }
    Ok(true)
}

impl SqliteStore {
    #[allow(
        clippy::too_many_arguments,
        reason = "routing and asserted producer context stay explicit"
    )]
    pub(crate) fn record_registered_verification(
        &mut self,
        project_id: &ProjectId,
        session_id: &SessionId,
        connection_token: &str,
        routing_token: &str,
        actor: &ActorContext,
        request: &RegisteredVerificationRequest,
        now: DateTime<Utc>,
    ) -> Result<RegisteredVerificationReceipt, StoreError> {
        let intent = CanonicalObject::freeze(&Intent {
            control_schema_version: crate::CONTROL_SCHEMA_VERSION,
            session_id: session_id.clone(),
            project_id: project_id.clone(),
            idempotency_key: request.operation_key.clone(),
            request: request.clone(),
        })?;
        super::control_runtime::control_lifecycle_transaction(&mut self.connection, |tx| {
            Self::verify_control_connection(tx, session_id, connection_token)?;
            let session = Self::load_control_session_on(tx, session_id)?
                .ok_or_else(|| StoreError::ControlSessionNotBound(session_id.0.clone()))?;
            Self::verify_control_session(&session, project_id, routing_token)?;
            if let Some(receipt) = Self::replay_control_operation(
                tx,
                session_id,
                "verification_record",
                &request.operation_key,
                intent.key(),
            )? {
                return Ok(receipt);
            }
            let receipt = super::work::record_registered_verification_on(
                tx, project_id, session_id, actor, request, now,
            )?;
            Self::persist_control_operation(
                tx,
                session_id,
                "verification_record",
                &request.operation_key,
                &intent,
                &receipt,
                now,
            )?;
            Ok(receipt)
        })
    }
}
