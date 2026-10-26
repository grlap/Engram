//! Exact recorded outcomes, never proof that a request replayed.

use chrono::{DateTime, Utc};
use rusqlite::{Connection, OptionalExtension, params, types::Value};
use serde::{Deserialize, Serialize};

use crate::{
    ControlRefusalCode, ControlTurnBeginDecision, ControlTurnCheckpointDecision,
    EnvironmentEvidenceInput, ExecutionObservationInput, ProjectId, SessionId, TurnNextIntent,
    VerificationEvidenceInput,
};

use super::{StoreError, details::timestamp};

/// Diagnostic selector bound only; producers retain their own key contract.
pub const MAX_INSPECTION_KEY_BYTES: usize = 4 * 1024;
const MAX_PAYLOAD_BYTES: i64 = 1024 * 1024;

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
/// Operations with retained outcomes supported by this diagnostic.
pub enum ControlInspectionOperation {
    TurnBegin,
    TurnCheckpoint,
}

impl ControlInspectionOperation {
    fn as_str(self) -> &'static str {
        match self {
            Self::TurnBegin => "turn_begin",
            Self::TurnCheckpoint => "turn_checkpoint",
        }
    }
}

/// Exact selection; validated before any store open and never serialized.
#[derive(Clone, Copy)]
pub struct ControlOperationSelector<'a> {
    operation: ControlInspectionOperation,
    key: &'a str,
}

impl<'a> ControlOperationSelector<'a> {
    /// Does not normalize or include the supplied value in errors.
    ///
    /// # Errors
    /// Refuses unsupported operations and invalid diagnostic keys.
    pub fn new(operation: &str, key: &'a str) -> Result<Self, StoreError> {
        let operation = match operation {
            "turn_begin" => ControlInspectionOperation::TurnBegin,
            "turn_checkpoint" => ControlInspectionOperation::TurnCheckpoint,
            _ => {
                return Err(invalid(
                    "inspection operation must be turn_begin or turn_checkpoint",
                ));
            }
        };
        if key.trim().is_empty()
            || key.len() > MAX_INSPECTION_KEY_BYTES
            || key.chars().any(char::is_control)
        {
            return Err(invalid(
                "inspection key must be nonblank, control-free and at most 4096 UTF-8 bytes",
            ));
        }
        Ok(Self { operation, key })
    }
}

#[derive(Debug, Serialize)]
pub(super) struct OperationResult {
    schema_version: u16,
    operation: ControlInspectionOperation,
    #[serde(flatten)]
    outcome: Outcome,
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum Outcome {
    NotFound,
    Found {
        recorded_at: DateTime<Utc>,
        decision: &'static str,
        #[serde(skip_serializing_if = "Option::is_none")]
        refusal_code: Option<ControlRefusalCode>,
    },
}

// The producer's fingerprint structs borrow their inputs and serialize only.
// This owned read shape validates that existing format without changing replay.
// Derived named-field decoding rejects duplicate identity fields as well.
#[derive(Deserialize)]
#[serde(deny_unknown_fields)]
struct RecordedIntent {
    control_schema_version: u16,
    session_id: SessionId,
    grant_id: String,
    idempotency_key: String,
    #[serde(default, deserialize_with = "present")]
    delivery_tokens: Option<Vec<String>>,
    #[serde(default, deserialize_with = "present")]
    next_intent: Option<TurnNextIntent>,
    #[serde(default, deserialize_with = "present")]
    observations: Option<Vec<ExecutionObservationInput>>,
    #[serde(default, deserialize_with = "present")]
    verification_evidence: Option<Vec<VerificationEvidenceInput>>,
    #[serde(default, deserialize_with = "present")]
    environment_evidence: Option<Vec<EnvironmentEvidenceInput>>,
}

fn present<'de, D, T>(deserializer: D) -> Result<Option<T>, D::Error>
where
    D: serde::Deserializer<'de>,
    T: Deserialize<'de>,
{
    T::deserialize(deserializer).map(Some)
}

pub(super) fn read(
    connection: &Connection,
    project: &ProjectId,
    session: &str,
    grant: &str,
    selector: ControlOperationSelector<'_>,
) -> Result<OperationResult, StoreError> {
    // Do not consult the candidate key until grant attribution is established.
    // A historical grant follows its anchor, even after owner-session rebind.
    let task: Option<String> = connection
        .query_row(
            "SELECT g.task_id FROM control_turn_grants g
         JOIN control_anchors a ON a.task_id = g.task_id
         WHERE g.grant_id = ?1 AND g.session_id = ?2 AND a.project_id = ?3",
            params![grant, session, project.0],
            |row| row.get(0),
        )
        .optional()?;
    let task = task.ok_or_else(|| {
        invalid("inspection grant is not attributable to the selected session and project")
    })?;
    let row = connection
        .query_row(
            "SELECT CASE WHEN length(intent_json) <= ?4 THEN intent_json END,
                CASE WHEN length(result_json) <= ?4 THEN result_json END,
                created_at_ms
         FROM control_operation_results
         WHERE session_id = ?1 AND operation = ?2 AND idempotency_key = ?3",
            params![
                session,
                selector.operation.as_str(),
                selector.key,
                MAX_PAYLOAD_BYTES
            ],
            |row| {
                Ok((
                    row.get::<_, Option<Vec<u8>>>(0)?,
                    row.get::<_, Option<Vec<u8>>>(1)?,
                    row.get::<_, Value>(2)?,
                ))
            },
        )
        .optional()?;
    let outcome = if let Some((intent, result, recorded_at)) = row {
        let intent = intent
            .ok_or_else(|| invalid("recorded operation intent exceeds inspection byte bound"))?;
        let result = result
            .ok_or_else(|| invalid("recorded operation result exceeds inspection byte bound"))?;
        let intent: RecordedIntent = decode(&intent)?;
        if intent.control_schema_version != crate::CONTROL_SCHEMA_VERSION
            || intent.session_id.0 != session
            || intent.grant_id != grant
            || intent.idempotency_key != selector.key
        {
            return Err(invalid(
                "recorded operation intent does not match inspection selectors",
            ));
        }
        let (decision, refusal_code) = match selector.operation {
            ControlInspectionOperation::TurnBegin => {
                if intent.delivery_tokens.is_none()
                    || intent.next_intent.is_some()
                    || intent.observations.is_some()
                    || intent.verification_evidence.is_some()
                    || intent.environment_evidence.is_some()
                {
                    return Err(invalid("recorded operation intent has invalid begin shape"));
                }
                match decode::<ControlTurnBeginDecision>(&result)? {
                    ControlTurnBeginDecision::Begin { receipt } => {
                        if receipt.grant_id != grant
                            || receipt.session_id.0 != session
                            || receipt.task_id.0.to_string() != task
                        {
                            return Err(invalid(
                                "recorded begin receipt does not match selected grant",
                            ));
                        }
                        ("begin", None)
                    }
                    ControlTurnBeginDecision::Refuse { code } => ("refuse", Some(code)),
                }
            }
            ControlInspectionOperation::TurnCheckpoint => {
                if intent.next_intent.is_none() || intent.delivery_tokens.is_some() {
                    return Err(invalid(
                        "recorded operation intent has invalid checkpoint shape",
                    ));
                }
                match decode::<ControlTurnCheckpointDecision>(&result)? {
                    ControlTurnCheckpointDecision::Checkpointed { receipt } => {
                        if receipt.grant_id != grant {
                            return Err(invalid(
                                "recorded checkpoint receipt does not match selected grant",
                            ));
                        }
                        ("checkpointed", None)
                    }
                    ControlTurnCheckpointDecision::Refuse { code, .. } => ("refuse", Some(code)),
                }
            }
        };
        let recorded_at = timestamp(&recorded_at, false)
            .0
            .ok_or_else(|| invalid("recorded operation timestamp is outside supported range"))?;
        Outcome::Found {
            recorded_at,
            decision,
            refusal_code,
        }
    } else {
        Outcome::NotFound
    };
    Ok(OperationResult {
        schema_version: 1,
        operation: selector.operation,
        outcome,
    })
}

fn decode<T: serde::de::DeserializeOwned>(bytes: &[u8]) -> Result<T, StoreError> {
    // Raw serde errors may contain keys, stored strings or payload excerpts.
    serde_json::from_slice(bytes).map_err(|_| invalid("invalid recorded operation payload"))
}

fn invalid(reason: &str) -> StoreError {
    StoreError::InvalidControlSession(reason.into())
}

#[cfg(test)]
mod tests;
