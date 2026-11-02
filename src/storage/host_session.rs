//! Authority of an embedding host's project-local evidence channel.

use super::{CanonicalObject, ObjectId, StoredControlOperation, work};
use super::{Connection, SessionId, SqliteStore, StoreError};
use crate::ProjectId;
use chrono::{DateTime, Utc};
use rusqlite::{OptionalExtension, Transaction, params};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// New ordinary receipts use the existing work-result owner. The external
/// receipt and immutable intent are unchanged; this envelope supplies the
/// project/session scope formerly supplied by the control-session foreign key.
#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(super) struct WorkHostReceipt {
    project_id: ProjectId,
    session_id: SessionId,
    operation: String,
    idempotency_key: String,
    intent: serde_json::Value,
    result: serde_json::Value,
}

fn work_operation(operation: &str) -> Result<&'static str, StoreError> {
    match operation {
        "named_root_bind" => Ok("host_named_root_bind"),
        "execution_observe" => Ok("host_execution_observe"),
        "verification_record" => Ok("host_verification_record"),
        "verification_bind" => Ok("host_verification_bind"),
        _ => Err(StoreError::InvalidWorkProjection(
            "unknown ordinary host operation".into(),
        )),
    }
}

fn scoped_key(project: &ProjectId, session: &SessionId, key: &str) -> Result<String, StoreError> {
    serde_json::to_string(&(project, session, key)).map_err(StoreError::Json)
}

pub(super) fn verify_work_receipt_on(
    connection: &Connection,
    operation: &str,
    key: &str,
    request_hash: &str,
    bytes: &[u8],
) -> Result<(), StoreError> {
    let receipt: WorkHostReceipt = serde_json::from_slice(bytes)?;
    let intent = CanonicalObject::freeze(&receipt.intent)?;
    if work_operation(&receipt.operation)? != operation
        || scoped_key(
            &receipt.project_id,
            &receipt.session_id,
            &receipt.idempotency_key,
        )? != key
        || intent.key().as_str() != request_hash
    {
        return Err(StoreError::InvalidWorkProjection(
            "ordinary host receipt scope or intent differs".into(),
        ));
    }
    let field = match receipt.operation.as_str() {
        "named_root_bind" => "event",
        "execution_observe" => "observation",
        "verification_record" => "producer",
        "verification_bind" => "original",
        _ => {
            return Err(StoreError::InvalidWorkProjection(
                "unknown ordinary host operation".into(),
            ));
        }
    };
    let record_id = receipt
        .result
        .get(field)
        .and_then(serde_json::Value::as_str)
        .ok_or_else(|| {
            StoreError::InvalidWorkProjection("ordinary host receipt has no record".into())
        })?;
    let record: Vec<u8> = connection.query_row(
        "SELECT canonical_json FROM objects WHERE object_id = ?1",
        [record_id],
        |row| row.get(0),
    )?;
    let record: serde_json::Value = serde_json::from_slice(&record)?;
    if record.get("project_id") != Some(&serde_json::to_value(&receipt.project_id)?) {
        return Err(StoreError::InvalidWorkProjection(
            "ordinary host receipt crosses its project".into(),
        ));
    }
    SqliteStore::verify_control_operation(
        connection,
        &StoredControlOperation {
            sequence: 0,
            session_id: receipt.session_id.0,
            operation: receipt.operation,
            idempotency_key: receipt.idempotency_key,
            intent_hash: request_hash.into(),
            intent_json: intent.bytes().to_vec(),
            result_json: crate::canonical::canonical_bytes(&receipt.result)?,
        },
    )
}

#[derive(Clone, Copy)]
pub(crate) enum HostSessionAuthority {
    Work,
}

impl HostSessionAuthority {
    #[allow(
        clippy::too_many_arguments,
        reason = "receipt persistence retains each scoped retry input"
    )]
    pub(super) fn persist<T: Serialize>(
        self,
        tx: &Transaction<'_>,
        project: &ProjectId,
        session: &SessionId,
        operation: &str,
        key: &str,
        intent: &CanonicalObject,
        result: &T,
        _now: DateTime<Utc>,
    ) -> Result<(), StoreError> {
        let Self::Work = self;
        if key.trim().is_empty() {
            return Err(StoreError::InvalidWork(
                "host operation idempotency key is empty".into(),
            ));
        }
        let stored = WorkHostReceipt {
            project_id: project.clone(),
            session_id: session.clone(),
            operation: operation.into(),
            idempotency_key: key.into(),
            intent: serde_json::from_slice(intent.bytes())?,
            result: serde_json::to_value(result)?,
        };
        let operation = work_operation(operation)?;
        let key = scoped_key(project, session, key)?;
        let bytes = serde_json::to_vec(&stored)?;
        verify_work_receipt_on(tx, operation, &key, intent.key().as_str(), &bytes)?;
        work::persist_operation_result(tx, operation, &key, intent.key(), &stored)
    }

    pub(super) fn verify_on(
        self,
        connection: &Connection,
        _project: &ProjectId,
        session: &SessionId,
        connection_token: &str,
    ) -> Result<(), StoreError> {
        let Self::Work = self;
        SqliteStore::verify_control_connection(connection, session, connection_token)
    }
}

impl SqliteStore {
    /// Read an exact preserved historical receipt or an ordinary work receipt.
    /// No historical operation is executed or settled by this lookup.
    pub(super) fn replay_host_operation<T: DeserializeOwned>(
        tx: &Transaction<'_>,
        project: &ProjectId,
        session: &SessionId,
        operation: &str,
        key: &str,
        intent_hash: &ObjectId,
    ) -> Result<Option<T>, StoreError> {
        if let Some(receipt) =
            SqliteStore::replay_control_operation(tx, session, operation, key, intent_hash)?
        {
            return Ok(Some(receipt));
        }
        let work_operation = work_operation(operation)?;
        let original_key = key;
        let key = scoped_key(project, session, key)?;
        let stored: Option<(String, Vec<u8>)> = tx
            .query_row(
                "SELECT request_hash, result_json FROM work_operation_results
             WHERE operation = ?1 AND idempotency_key = ?2",
                params![work_operation, key],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?;
        let Some((hash, bytes)) = stored else {
            return Ok(None);
        };
        if hash != intent_hash.as_str() {
            return Err(StoreError::ControlOperationIdempotencyConflict {
                operation: operation.into(),
                key: original_key.into(),
            });
        }
        verify_work_receipt_on(tx, work_operation, &key, &hash, &bytes)?;
        let receipt: WorkHostReceipt = serde_json::from_slice(&bytes)?;
        serde_json::from_value(receipt.result)
            .map(Some)
            .map_err(StoreError::Json)
    }

    pub(crate) fn resume_work_connection(
        &mut self,
        session: &SessionId,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<String, StoreError> {
        super::admit_session_id(session)?;
        super::control_runtime::control_lifecycle_transaction(&mut self.connection, |tx| {
            Self::rotate_host_connection_on(tx, session, now)
        })
    }
    pub(super) fn rotate_host_connection_on(
        tx: &rusqlite::Transaction<'_>,
        session: &SessionId,
        now: chrono::DateTime<chrono::Utc>,
    ) -> Result<String, StoreError> {
        let token = uuid::Uuid::now_v7().to_string();
        tx.execute(
            "INSERT INTO control_connections (session_id, connection_token, opened_at_ms)
                 VALUES (?1, ?2, ?3)
                 ON CONFLICT(session_id) DO UPDATE SET
                    connection_token = excluded.connection_token,
                    opened_at_ms = excluded.opened_at_ms",
            rusqlite::params![session.0, token, now.timestamp_millis()],
        )?;
        Ok(token)
    }
}
