//! Grant-free settlement of a test run on another host.

use super::{CanonicalObject, SqliteStore, StoreError};
use crate::domain::{
    REMOTE_VERIFICATION_ASSOCIATION_KIND, RemoteVerificationAssociationRecord,
    RemoteVerificationReceipt, RemoteVerificationRequest, VerificationEvidence,
};
use crate::{ActorContext, ProjectId, SessionId};
use chrono::{DateTime, Utc};

pub(super) const REMOTE_VERIFICATION_OPERATION: &str = "remote_verification_record";

#[derive(serde::Serialize, serde::Deserialize)]
#[serde(deny_unknown_fields)]
struct Intent {
    control_schema_version: u16,
    session_id: SessionId,
    project_id: ProjectId,
    idempotency_key: String,
    request: RemoteVerificationRequest,
}

pub(super) fn row_matches(
    connection: &rusqlite::Connection,
    stored_session: &str,
    intent: serde_json::Value,
    result: serde_json::Value,
) -> Result<bool, StoreError> {
    let (Ok(intent), Ok(receipt)) = (
        serde_json::from_value::<Intent>(intent),
        serde_json::from_value::<RemoteVerificationReceipt>(result),
    ) else {
        return Ok(false);
    };
    if intent.control_schema_version != crate::CONTROL_SCHEMA_VERSION
        || intent.session_id.0 != stored_session
        || intent.idempotency_key != intent.request.operation_key
    {
        return Ok(false);
    }
    let record: RemoteVerificationAssociationRecord = super::work::load_typed_work_object(
        connection,
        &receipt.operation,
        REMOTE_VERIFICATION_ASSOCIATION_KIND,
    )?;
    // A repeated delivery under another key reports the delivery it repeats,
    // so the record's own key may differ; everything else is the same request.
    if record.project_id != intent.project_id
        || record.session_id != intent.session_id
        || record.request.completion != intent.request.completion
        || record.request.association != intent.request.association
        || record.origin != receipt.origin
        || record.qualification != receipt.qualification
        || receipt.verifications.len() != usize::from(record.qualification.credited)
    {
        return Ok(false);
    }
    for entry in &receipt.verifications {
        let evidence: VerificationEvidence = super::work::load_typed_work_object(
            connection,
            &entry.record_id,
            "verification_evidence",
        )?;
        if evidence.producer_observation != receipt.operation {
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
    pub(crate) fn record_remote_verification(
        &mut self,
        project_id: &ProjectId,
        session_id: &SessionId,
        connection_token: &str,
        authority: super::HostSessionAuthority,
        actor: &ActorContext,
        request: &RemoteVerificationRequest,
        now: DateTime<Utc>,
    ) -> Result<RemoteVerificationReceipt, StoreError> {
        let intent = CanonicalObject::freeze(&Intent {
            control_schema_version: crate::CONTROL_SCHEMA_VERSION,
            session_id: session_id.clone(),
            project_id: project_id.clone(),
            idempotency_key: request.operation_key.clone(),
            request: request.clone(),
        })?;
        super::control_runtime::control_lifecycle_transaction(&mut self.connection, |tx| {
            authority.verify_on(tx, project_id, session_id, connection_token)?;
            // An exact retry answers before its destination is judged again,
            // so moved authority cannot change an accepted receipt.
            if let Some(receipt) = Self::replay_host_operation(
                tx,
                project_id,
                session_id,
                REMOTE_VERIFICATION_OPERATION,
                &request.operation_key,
                intent.key(),
            )? {
                return Ok(receipt);
            }
            let receipt = super::work::record_remote_verification_on(
                tx, project_id, session_id, actor, request, now,
            )?;
            authority.persist(
                tx,
                project_id,
                session_id,
                REMOTE_VERIFICATION_OPERATION,
                &request.operation_key,
                &intent,
                &receipt,
                now,
            )?;
            Ok(receipt)
        })
    }
}
