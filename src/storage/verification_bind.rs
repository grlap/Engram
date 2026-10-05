//! The host's request to bind one native passed check to several items the
//! session holds. Routing, replay and the control-operation record live
//! here; the validation and the writes live in the work store.

use chrono::{DateTime, Utc};
use rusqlite::TransactionBehavior;
use serde::{Deserialize, Serialize};

use super::{SqliteStore, StoreError, work};
use crate::ObjectId;
use crate::canonical::CanonicalObject;
use crate::domain::{
    ActorContext, BindMeasurement, CONTROL_SCHEMA_VERSION, ProjectId, SessionId,
    VerificationBindInput, VerificationBindReceipt, VerificationBindTarget,
};

/// The control operation a bind is recorded and replayed under.
pub(crate) const VERIFICATION_BIND_OPERATION: &str = "verification_bind";

/// What a retry must repeat exactly: the session and project it acts for and
/// every semantic field of the request. The routing token is not part of
/// it, so a retry under refreshed credentials still replays. The session and
/// key sit at the top level, where doctor binds every control operation to
/// its row.
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

impl VerificationBindIntent {
    fn new(project_id: &ProjectId, session_id: &SessionId, input: &VerificationBindInput) -> Self {
        Self {
            control_schema_version: CONTROL_SCHEMA_VERSION,
            operation: VERIFICATION_BIND_OPERATION.into(),
            project_id: project_id.clone(),
            session_id: session_id.clone(),
            idempotency_key: input.idempotency_key.clone(),
            original: input.original.clone(),
            measurement: input.measurement.clone(),
            targets: input.targets.clone(),
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

impl SqliteStore {
    /// Binds `input.original` to every target for the bound control session.
    ///
    /// A request committed earlier with the same key and intent is replayed
    /// before anything mutable is consulted, even when a claim it named has
    /// since ended; its receipt says it was replayed.
    ///
    /// # Errors
    ///
    /// The control session's own refusals for a superseded connection, an
    /// unbound session or a mismatched project or routing token;
    /// [`StoreError::VerificationBindRefused`] naming every failing part
    /// when the request does not hold, with nothing written;
    /// [`StoreError::ControlOperationIdempotencyConflict`] when the key was
    /// used for a different request.
    #[allow(
        clippy::too_many_arguments,
        reason = "the host record keeps routing, attribution, request and time explicit"
    )]
    pub(crate) fn bind_verification(
        &mut self,
        project_id: &ProjectId,
        session_id: &SessionId,
        connection_token: &str,
        routing_token: &str,
        binder: &ActorContext,
        input: &VerificationBindInput,
        now: DateTime<Utc>,
    ) -> Result<VerificationBindReceipt, StoreError> {
        let intent =
            CanonicalObject::freeze(&VerificationBindIntent::new(project_id, session_id, input))?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        Self::verify_control_connection(&transaction, session_id, connection_token)?;
        let session = Self::load_control_session_on(&transaction, session_id)?
            .ok_or_else(|| StoreError::ControlSessionNotBound(session_id.0.clone()))?;
        Self::verify_control_session(&session, project_id, routing_token)?;
        if let Some(mut replay) = Self::replay_control_operation::<VerificationBindReceipt>(
            &transaction,
            session_id,
            VERIFICATION_BIND_OPERATION,
            &input.idempotency_key,
            intent.key(),
        )? {
            transaction.commit()?;
            replay.replayed = true;
            return Ok(replay);
        }
        match work::bind_verification_on(&transaction, project_id, session_id, binder, input, now)?
        {
            work::VerificationBindOutcome::Bound(receipt) => {
                Self::persist_control_operation(
                    &transaction,
                    session_id,
                    VERIFICATION_BIND_OPERATION,
                    &input.idempotency_key,
                    &intent,
                    &receipt,
                    now,
                )?;
                transaction.commit()?;
                Ok(receipt)
            }
            // Dropping the transaction rolls back; nothing was written.
            work::VerificationBindOutcome::Refused(refusal) => {
                Err(StoreError::VerificationBindRefused(Box::new(refusal)))
            }
        }
    }
}
