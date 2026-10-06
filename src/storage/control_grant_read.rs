//! Exact operational grant evidence, without admission or lazy expiry.

use chrono::{DateTime, Datelike, Utc};
use rusqlite::{Connection, OptionalExtension, Transaction, TransactionBehavior, types::Value};
use serde::Serialize;

use super::{CONTROL_SCHEMA_VERSION, SessionId, SqliteStore, StoreError, TurnGrantState};
use crate::ProjectId;

#[derive(Debug, Serialize)]
pub(crate) struct ControlTurnGrantRead {
    control_schema_version: u16,
    session_id: SessionId,
    grant_id: String,
    #[serde(flatten)]
    evidence: GrantEvidence,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum GrantEvidence {
    Found {
        state: TurnGrantState,
        begun_at: Option<DateTime<Utc>>,
        completed_at: Option<DateTime<Utc>>,
    },
    NotFound,
}

impl SqliteStore {
    pub(crate) fn read_control_turn_grant(
        &self,
        project: &ProjectId,
        session: &SessionId,
        connection_token: &str,
        routing_token: &str,
        grant_id: &str,
    ) -> Result<ControlTurnGrantRead, StoreError> {
        read_in_snapshot(
            &self.connection,
            project,
            session,
            connection_token,
            routing_token,
            grant_id,
        )
    }
}

fn read_in_snapshot(
    connection: &Connection,
    project: &ProjectId,
    session: &SessionId,
    connection_token: &str,
    routing_token: &str,
    grant_id: &str,
) -> Result<ControlTurnGrantRead, StoreError> {
    let snapshot = Transaction::new_unchecked(connection, TransactionBehavior::Deferred)?;
    SqliteStore::verify_control_connection(&snapshot, session, connection_token)?;
    let stored = SqliteStore::load_control_session_on(&snapshot, session)?
        .ok_or_else(|| StoreError::ControlSessionNotBound(session.0.clone()))?;
    SqliteStore::verify_control_session(&stored, project, routing_token)?;
    if grant_id.trim().is_empty() {
        return Err(StoreError::InvalidTurnGrantId);
    }
    let row = snapshot.query_row(
        "SELECT session_id, state, begun_at_ms, completed_at_ms FROM control_turn_grants WHERE grant_id = ?1",
        [grant_id],
        |row| Ok((row.get::<_, String>(0)?, row.get::<_, Value>(1)?, row.get::<_, Value>(2)?, row.get::<_, Value>(3)?)),
    ).optional()?;
    let evidence = match row {
        None => GrantEvidence::NotFound,
        Some((owner, state, begun, completed)) => {
            if owner != session.0 {
                return Err(StoreError::ControlTurnGrantSessionMismatch);
            }
            let state = match state {
                Value::Text(state) => match state.as_str() {
                    "issued" => TurnGrantState::Issued,
                    "begun" => TurnGrantState::Begun,
                    "completed" => TurnGrantState::Completed,
                    "expired" => TurnGrantState::Expired,
                    "superseded" => TurnGrantState::Superseded,
                    _ => return Err(invalid("unknown grant state")),
                },
                _ => return Err(invalid("grant state is not text")),
            };
            let begun_at = timestamp(&begun)?;
            let completed_at = timestamp(&completed)?;
            let coherent = match state {
                TurnGrantState::Issued | TurnGrantState::Expired | TurnGrantState::Superseded => {
                    begun_at.is_none() && completed_at.is_none()
                }
                TurnGrantState::Begun => begun_at.is_some() && completed_at.is_none(),
                TurnGrantState::Completed => begun_at.is_some() && completed_at.is_some(),
            };
            if !coherent {
                return Err(invalid(
                    "grant state and begin/checkpoint timestamps disagree",
                ));
            }
            GrantEvidence::Found {
                state,
                begun_at,
                completed_at,
            }
        }
    };
    snapshot.commit()?;
    Ok(ControlTurnGrantRead {
        control_schema_version: CONTROL_SCHEMA_VERSION,
        session_id: session.clone(),
        grant_id: grant_id.to_owned(),
        evidence,
    })
}

fn invalid(reason: &str) -> StoreError {
    StoreError::InvalidControlProjection(reason.to_owned())
}

fn timestamp(value: &Value) -> Result<Option<DateTime<Utc>>, StoreError> {
    match value {
        Value::Null => Ok(None),
        Value::Integer(ms) => DateTime::from_timestamp_millis(*ms)
            .filter(|timestamp| (0..=9999).contains(&timestamp.year()))
            .map(Some)
            .ok_or_else(|| invalid("grant timestamp is outside the supported range")),
        _ => Err(invalid("grant timestamp is not integer milliseconds")),
    }
}

#[cfg(test)]
mod tests;
