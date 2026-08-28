//! Host-private newline-delimited JSON control transport.
//!
//! This surface is intentionally separate from agent-facing MCP. Possessing a
//! routing token prevents accidental session mix-ups but is not authentication;
//! the embedding host remains the policy-enforcement point.

use std::{
    io::{BufRead, Write},
    path::Path,
    str::FromStr,
};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    ActorContext, ControlAssurance, ControlWorkBinding, EffectClass, EnvironmentEvidenceInput,
    ExecutionObservationInput, LeaseKind, LeaseMode, ObjectHash, ProjectId, ResourceSubject,
    SessionId, SqliteStore, TurnIntent, TurnPurpose, VerificationEvidenceInput,
    domain::{AssuranceLevel, ProvenanceLink, ProvenanceRelation, TurnNextIntent},
    storage::StoreError,
};

const MAX_HOST_CONTROL_FRAME_BYTES: usize = 256 * 1_024;

/// One host-private control operation. The runtime session and asserted actor
/// are fixed by process arguments instead of repeated in agent-controlled
/// request payloads.
#[derive(Debug, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case")]
pub enum HostControlRequest {
    SessionBind {
        external_ref: String,
        title: String,
        assurance: ControlAssurance,
        mediated_effects: Vec<EffectClass>,
        #[serde(default)]
        work_binding: Option<ControlWorkBinding>,
        capability_map_revision: i64,
        idempotency_key: String,
    },
    SessionStatus {
        routing_token: String,
    },
    LeaseAcquire {
        routing_token: String,
        kind: LeaseKind,
        mode: LeaseMode,
        subject: ResourceSubject,
        ttl_seconds: i64,
        idempotency_key: String,
    },
    LeaseRelease {
        routing_token: String,
        lease_id: String,
        idempotency_key: String,
    },
    TurnEvaluate {
        routing_token: String,
        idempotency_key: String,
        intent_fingerprint: String,
        purpose: TurnPurpose,
        requested_effects: Vec<EffectClass>,
        #[serde(default)]
        resource_intents: Vec<crate::ResourceSubject>,
    },
    TurnBegin {
        routing_token: String,
        grant_id: String,
        delivery_tokens: Vec<String>,
        idempotency_key: String,
    },
    TurnCheckpoint {
        routing_token: String,
        grant_id: String,
        next_intent: TurnNextIntent,
        #[serde(default)]
        observations: Vec<ExecutionObservationInput>,
        #[serde(default)]
        verification_evidence: Vec<VerificationEvidenceInput>,
        #[serde(default)]
        environment_evidence: Vec<EnvironmentEvidenceInput>,
        idempotency_key: String,
    },
}

#[derive(Debug, Serialize)]
#[serde(tag = "status", rename_all = "snake_case")]
enum HostControlResponse {
    Ok { result: Value },
    Error { error: HostControlErrorBody },
}

#[derive(Debug, Serialize)]
struct HostControlErrorBody {
    code: &'static str,
    message: String,
}

/// Long-lived host-private service over one project-local store.
pub struct HostControlServer {
    store: SqliteStore,
    project_id: ProjectId,
    actor_id: String,
    session_id: SessionId,
    connection_token: String,
    source_skill: Option<String>,
}

impl HostControlServer {
    /// Opens the project store and fixes asserted host context for this
    /// connection.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the SQLite store cannot be opened.
    pub fn open(
        database: impl AsRef<Path>,
        project_id: ProjectId,
        actor_id: String,
        session_id: SessionId,
        source_skill: Option<String>,
    ) -> Result<Self, StoreError> {
        let mut store = SqliteStore::open(database)?;
        let connection_token = store.resume_control_connection(&session_id, Utc::now())?;
        Ok(Self {
            store,
            project_id,
            actor_id,
            session_id,
            connection_token,
            source_skill,
        })
    }

    /// Handles one decoded request and returns its typed result as JSON.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] for invalid protocol data or failed durable
    /// transitions.
    #[allow(
        clippy::too_many_lines,
        reason = "the tagged host protocol stays auditable as one exhaustive dispatch"
    )]
    pub fn handle(&mut self, request: HostControlRequest) -> Result<Value, StoreError> {
        let now = Utc::now();
        match request {
            HostControlRequest::SessionBind {
                external_ref,
                title,
                assurance,
                mediated_effects,
                work_binding,
                capability_map_revision,
                idempotency_key,
            } => {
                let mut actor = self.actor("session_bind", "bind the host control session");
                actor.run_id = work_binding
                    .as_ref()
                    .map(|binding| binding.run_id.0.to_string());
                serde_json::to_value(self.store.bind_control_session_with_work(
                    &self.project_id,
                    &external_ref,
                    &title,
                    &self.session_id,
                    &self.connection_token,
                    &actor,
                    work_binding.as_ref(),
                    assurance,
                    &mediated_effects,
                    capability_map_revision,
                    &idempotency_key,
                    now,
                )?)
                .map_err(StoreError::Json)
            }
            HostControlRequest::SessionStatus { routing_token } => {
                serde_json::to_value(self.store.control_status(
                    &self.project_id,
                    &self.session_id,
                    &self.connection_token,
                    &routing_token,
                    now,
                )?)
                .map_err(StoreError::Json)
            }
            HostControlRequest::LeaseAcquire {
                routing_token,
                kind,
                mode,
                subject,
                ttl_seconds,
                idempotency_key,
            } => serde_json::to_value(self.store.acquire_work_lease(
                &self.project_id,
                &self.session_id,
                &self.connection_token,
                &routing_token,
                kind,
                mode,
                &subject,
                ttl_seconds,
                &idempotency_key,
                now,
            )?)
            .map_err(StoreError::Json),
            HostControlRequest::LeaseRelease {
                routing_token,
                lease_id,
                idempotency_key,
            } => serde_json::to_value(self.store.release_work_lease(
                &self.project_id,
                &self.session_id,
                &self.connection_token,
                &routing_token,
                &lease_id,
                &idempotency_key,
                now,
            )?)
            .map_err(StoreError::Json),
            HostControlRequest::TurnEvaluate {
                routing_token,
                idempotency_key,
                intent_fingerprint,
                purpose,
                requested_effects,
                resource_intents,
            } => {
                let intent_fingerprint =
                    ObjectHash::from_str(&intent_fingerprint).map_err(|_| {
                        StoreError::InvalidControlSession(
                            "intent_fingerprint must be a lowercase SHA-256 digest".into(),
                        )
                    })?;
                serde_json::to_value(self.store.evaluate_control_turn(
                    &self.project_id,
                    &self.session_id,
                    &self.connection_token,
                    &routing_token,
                    &TurnIntent {
                        idempotency_key,
                        intent_fingerprint,
                        purpose,
                        requested_effects,
                        resource_intents,
                    },
                    now,
                )?)
                .map_err(StoreError::Json)
            }
            HostControlRequest::TurnBegin {
                routing_token,
                grant_id,
                delivery_tokens,
                idempotency_key,
            } => serde_json::to_value(self.store.begin_control_turn(
                &self.project_id,
                &self.session_id,
                &self.connection_token,
                &routing_token,
                &grant_id,
                &delivery_tokens,
                &idempotency_key,
                now,
            )?)
            .map_err(StoreError::Json),
            HostControlRequest::TurnCheckpoint {
                routing_token,
                grant_id,
                next_intent,
                observations,
                verification_evidence,
                environment_evidence,
                idempotency_key,
            } => serde_json::to_value(self.store.checkpoint_control_turn_with_evidence(
                &self.project_id,
                &self.session_id,
                &self.connection_token,
                &routing_token,
                &grant_id,
                next_intent,
                &observations,
                &verification_evidence,
                &environment_evidence,
                &idempotency_key,
                now,
            )?)
            .map_err(StoreError::Json),
        }
    }

    /// Serves newline-delimited JSON until EOF. Request failures are returned
    /// as one error line and do not terminate the host connection.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the transport itself cannot read or write.
    pub fn serve(
        &mut self,
        mut reader: impl BufRead,
        mut writer: impl Write,
    ) -> std::io::Result<()> {
        loop {
            let Some(frame) = read_control_frame(&mut reader)? else {
                return Ok(());
            };
            let response = match frame {
                Err(()) => HostControlResponse::Error {
                    error: HostControlErrorBody {
                        code: "invalid_request",
                        message: format!(
                            "host control frame exceeds {MAX_HOST_CONTROL_FRAME_BYTES} bytes"
                        ),
                    },
                },
                Ok(frame) => match serde_json::from_slice::<HostControlRequest>(&frame) {
                    Ok(request) => match self.handle(request) {
                        Ok(result) => HostControlResponse::Ok { result },
                        Err(error) => HostControlResponse::Error {
                            error: HostControlErrorBody {
                                code: store_error_code(&error),
                                message: error.to_string(),
                            },
                        },
                    },
                    Err(error) => HostControlResponse::Error {
                        error: HostControlErrorBody {
                            code: "invalid_request",
                            message: error.to_string(),
                        },
                    },
                },
            };
            serde_json::to_writer(&mut writer, &response)?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }
    }

    fn actor(&self, operation: &str, reason: &str) -> ActorContext {
        ActorContext {
            actor_id: self.actor_id.clone(),
            actor_kind: "agent".into(),
            assurance: AssuranceLevel::Asserted,
            run_id: None,
            session_id: Some(self.session_id.clone()),
            source_tool: Some(format!("host-control:{operation}")),
            source_skill: self.source_skill.clone(),
            provenance_chain: vec![ProvenanceLink {
                relation: ProvenanceRelation::AssertedBy,
                source: self.actor_id.clone(),
                reference: Some(self.session_id.0.clone()),
            }],
            reason: reason.into(),
        }
    }
}

fn read_control_frame(reader: &mut impl BufRead) -> std::io::Result<Option<Result<Vec<u8>, ()>>> {
    let mut frame = Vec::new();
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return if frame.is_empty() {
                Ok(None)
            } else {
                Ok(Some(Ok(frame)))
            };
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |position| position + 1);
        let oversized = frame.len().saturating_add(consumed) > MAX_HOST_CONTROL_FRAME_BYTES;
        if !oversized {
            frame.extend_from_slice(&available[..consumed]);
        }
        reader.consume(consumed);
        if oversized {
            if newline.is_none() {
                drain_control_frame(reader)?;
            }
            return Ok(Some(Err(())));
        }
        if newline.is_some() {
            return Ok(Some(Ok(frame)));
        }
    }
}

fn drain_control_frame(reader: &mut impl BufRead) -> std::io::Result<()> {
    loop {
        let available = reader.fill_buf()?;
        if available.is_empty() {
            return Ok(());
        }
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |position| position + 1);
        reader.consume(consumed);
        if newline.is_some() {
            return Ok(());
        }
    }
}

const fn store_error_code(error: &StoreError) -> &'static str {
    match error {
        StoreError::InvalidControlSession(_) => "invalid_control_session",
        StoreError::ControlSessionNotBound(_) => "control_session_not_bound",
        StoreError::ControlSessionTokenMismatch(_) => "control_session_token_mismatch",
        StoreError::ControlConnectionSuperseded(_) => "control_connection_superseded",
        StoreError::ControlSessionBindConflict(_) => "control_session_bind_conflict",
        StoreError::ControlTurnIdempotencyConflict(_) => "turn_idempotency_conflict",
        StoreError::ControlOperationIdempotencyConflict { .. } => {
            "control_operation_idempotency_conflict"
        }
        StoreError::ControlWorkBindingStale { .. } => "stale_fence",
        StoreError::ControlGrantScopeMismatch { .. } => "grant_scope_mismatch",
        StoreError::VerificationProducerObservationNotFound(_) => "verification_producer_not_found",
        StoreError::ControlTurnGrantNotFound(_) => "turn_grant_not_found",
        StoreError::WorkLeaseNotFound(_) => "work_lease_not_found",
        StoreError::WorkLeaseNotHeld { .. } => "work_lease_not_held",
        StoreError::WorkLeaseExpired { .. } => "work_lease_expired",
        StoreError::InvalidControlProjection(_) | StoreError::InvalidControlObservation(_) => {
            "control_projection_invalid"
        }
        StoreError::PinnedContradiction { .. } => "pinned_contradiction",
        StoreError::PinnedBudgetExceeded { .. } => "pinned_budget_exceeded",
        StoreError::TaskAccessDenied { .. } => "task_access_denied",
        StoreError::WorkClaimMismatch { .. } => "work_claim_mismatch",
        _ => "storage_error",
    }
}

#[cfg(test)]
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn oversized_control_frame_is_rejected_and_drained() {
        let mut input = vec![b'x'; MAX_HOST_CONTROL_FRAME_BYTES + 1];
        input.extend_from_slice(b"\n{}\n");
        let mut output = Vec::new();
        let mut server = HostControlServer {
            store: SqliteStore::open_in_memory().expect("store"),
            project_id: ProjectId("frame-project".into()),
            actor_id: "frame-agent".into(),
            session_id: SessionId("frame-session".into()),
            connection_token: "frame-connection".into(),
            source_skill: None,
        };

        server
            .serve(Cursor::new(input), &mut output)
            .expect("serve bounded frames");
        let responses = String::from_utf8(output)
            .expect("response UTF-8")
            .lines()
            .map(|line| serde_json::from_str::<Value>(line).expect("response JSON"))
            .collect::<Vec<_>>();
        assert_eq!(responses.len(), 2);
        assert_eq!(responses[0]["error"]["code"], "invalid_request");
        assert!(
            responses[0]["error"]["message"]
                .as_str()
                .is_some_and(|message| message.contains("exceeds"))
        );
        assert_eq!(responses[1]["error"]["code"], "invalid_request");
    }
}
