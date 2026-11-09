//! The record an earlier build's `verification_bind` operation left: its
//! stored intent and receipt, which doctor still checks. No request writes
//! one now; the operation is gone and these rows are history.

use serde::{Deserialize, Serialize};

use super::{StoreError, work};
use crate::ObjectId;
use crate::domain::{
    BindMeasurement, CONTROL_SCHEMA_VERSION, ProjectId, SessionId, VerificationBindReceipt,
    VerificationBindTarget,
};

/// The operation name a stored bind row carries.
pub(crate) const VERIFICATION_BIND_OPERATION: &str = "verification_bind";

/// A stored bind's intent: the session and project it acted for and every
/// semantic field of the request. The session and key sit at the top level,
/// where doctor binds every host operation to its row.
#[derive(Deserialize, Serialize)]
#[serde(deny_unknown_fields)]
pub(super) struct VerificationBindIntent {
    control_schema_version: u16,
    operation: String,
    project_id: ProjectId,
    session_id: SessionId,
    idempotency_key: String,
    original: ObjectId,
    measurement: BindMeasurement,
    targets: Vec<VerificationBindTarget>,
}

#[cfg(test)]
impl VerificationBindIntent {
    pub(super) fn historical(
        project_id: &ProjectId,
        session_id: &SessionId,
        idempotency_key: &str,
        original: &ObjectId,
        measurement: &BindMeasurement,
        targets: &[VerificationBindTarget],
    ) -> Self {
        Self {
            control_schema_version: CONTROL_SCHEMA_VERSION,
            operation: VERIFICATION_BIND_OPERATION.into(),
            project_id: project_id.clone(),
            session_id: session_id.clone(),
            idempotency_key: idempotency_key.to_owned(),
            original: original.clone(),
            measurement: measurement.clone(),
            targets: targets.to_vec(),
        }
    }
}

/// Doctor's check of a stored bind: the intent is a bind of this row's
/// session, and the receipt names, per target in order, the record the bind
/// wrote for it, which carries that target's binding, sighting and criteria
/// and the request's original and measurement at the receipt's position.
pub(super) fn verification_bind_row_matches(
    connection: &rusqlite::Connection,
    stored_session: &str,
    intent: serde_json::Value,
    result: serde_json::Value,
) -> Result<bool, StoreError> {
    let (Ok(intent), Ok(receipt)) = (
        serde_json::from_value::<VerificationBindIntent>(intent),
        serde_json::from_value::<VerificationBindReceipt>(result),
    ) else {
        return Ok(false);
    };
    if intent.control_schema_version != CONTROL_SCHEMA_VERSION
        || intent.operation != VERIFICATION_BIND_OPERATION
        || intent.session_id.0 != stored_session
        || receipt.replayed
        || receipt.original != intent.original
    {
        return Ok(false);
    }
    work::bound_receipt_matches_on(
        connection,
        &intent.session_id,
        &intent.original,
        &intent.measurement,
        &intent.targets,
        &receipt.bound,
    )
}
