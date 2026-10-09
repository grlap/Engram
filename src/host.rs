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
    ExecutionObservationInput, HostPathPolicy, NamedRootBindingKind, NamedRootEndReason, ObjectId,
    ProjectId, SessionId, SqliteStore, TurnIntent, TurnPurpose, VerificationEvidenceInput,
    WorkClaimId, WorkRunId,
    domain::{AssuranceLevel, ProvenanceLink, ProvenanceRelation, TurnNextIntent},
    storage::StoreError,
};

pub(crate) const MAX_HOST_CONTROL_FRAME_BYTES: usize = 256 * 1_024;
const MAX_HOST_CONTROL_OPERATION_BYTES: usize = 64;
const MAX_HOST_CONTROL_ERROR_DETAIL_BYTES: usize = 384;

/// One host-private control operation. The runtime session and asserted actor
/// are fixed by process arguments instead of repeated in agent-controlled
/// request payloads.
#[derive(Debug, Deserialize)]
#[serde(tag = "operation", rename_all = "snake_case", deny_unknown_fields)]
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
    TurnGrantRead {
        routing_token: String,
        grant_id: String,
    },
    NamedRootBind {
        routing_token: String,
        claim_id: WorkClaimId,
        claim_fence: i64,
        workspace_id: String,
        generation: i64,
        named_at: chrono::DateTime<Utc>,
        kind: NamedRootBindingKind,
        #[serde(default)]
        end_reason: Option<NamedRootEndReason>,
        idempotency_key: String,
    },
    /// Reads one claim's named-root lifecycle on its run, for any run and
    /// claim of the project; it writes nothing and needs no live holder.
    NamedRootRead {
        routing_token: String,
        run_id: WorkRunId,
        claim_id: WorkClaimId,
    },
    /// Reads whether a run's named source root has the initial sighting that
    /// recording an evaluation at `run_cut` requires, or at the head when it
    /// is absent. It needs no routing token or live holder and writes
    /// nothing.
    NamedRootSightingRead {
        work_ref: String,
        run_id: WorkRunId,
        #[serde(default)]
        run_cut: Option<i64>,
    },
    /// Reads, for one item on its active run, what satisfied each bound
    /// acceptance criterion: the first page captures the run's feed head as
    /// its cut, and `after` continues at that cut. It writes nothing.
    AcceptanceBindingRead {
        routing_token: String,
        work_id: crate::WorkId,
        expected_work_revision: i64,
        run_id: WorkRunId,
        #[serde(default)]
        after: Option<String>,
    },
    /// Reads, for one criterion of an item on its active run, every host
    /// verification of the criterion's bound kind on that run up to
    /// `run_cut`, which must be the run's feed head; `after` continues at
    /// that cut. It computes no freshness and writes nothing.
    AcceptanceVerificationRead {
        routing_token: String,
        work_id: crate::WorkId,
        expected_work_revision: i64,
        run_id: WorkRunId,
        run_cut: i64,
        criterion: usize,
        #[serde(default)]
        after: Option<String>,
    },
    TurnEvaluate {
        routing_token: String,
        idempotency_key: String,
        intent_fingerprint: String,
        /// Optional while hosts still send it; only `ordinary` parses.
        #[serde(default)]
        purpose: Option<TurnPurpose>,
        requested_effects: Vec<EffectClass>,
        #[serde(default)]
        resource_intents: Vec<crate::ResourceSubject>,
    },
    TurnBegin {
        routing_token: String,
        grant_id: String,
        /// Optional while hosts still send it; grants carry no delivery
        /// page, so any token refuses the begin.
        #[serde(default)]
        delivery_tokens: Vec<String>,
        idempotency_key: String,
    },
    /// Records execution the host observed without admission: a turn seen
    /// after it started, a workspace change between turns, or a check inside
    /// such a turn. It records a fact, never a grant, a begin or a turn
    /// result.
    ExecutionObserve {
        routing_token: String,
        idempotency_key: String,
        binding: Box<ControlWorkBinding>,
        root_basis: Box<crate::domain::ObservationRootBasis>,
        observed_interval: crate::domain::ObservedInterval,
        occurrence: Box<crate::domain::ObservedOccurrence>,
        causality: crate::domain::ObservationCausality,
        policy_basis: Box<crate::domain::ObservationPolicyBasis>,
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
    /// Binds one native passed check to other items this session holds whose
    /// named roots hold the same content: one record per target, all or
    /// nothing. It creates no observation, turn or focus change.
    VerificationBind {
        routing_token: String,
        idempotency_key: String,
        original: crate::ObjectId,
        measurement: crate::domain::BindMeasurement,
        targets: Vec<crate::domain::VerificationBindTarget>,
    },
}

impl HostControlRequest {
    /// The protocol operation this request names, as a fixed label.
    #[must_use]
    pub const fn operation(&self) -> &'static str {
        match self {
            Self::SessionBind { .. } => "session_bind",
            Self::SessionStatus { .. } => "session_status",
            Self::TurnGrantRead { .. } => "turn_grant_read",
            Self::NamedRootBind { .. } => "named_root_bind",
            Self::NamedRootRead { .. } => "named_root_read",
            Self::NamedRootSightingRead { .. } => "named_root_sighting_read",
            Self::AcceptanceBindingRead { .. } => "acceptance_binding_read",
            Self::AcceptanceVerificationRead { .. } => "acceptance_verification_read",
            Self::TurnEvaluate { .. } => "turn_evaluate",
            Self::TurnBegin { .. } => "turn_begin",
            Self::ExecutionObserve { .. } => "execution_observe",
            Self::TurnCheckpoint { .. } => "turn_checkpoint",
            Self::VerificationBind { .. } => "verification_bind",
        }
    }
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
    /// The typed parts of a refusal that names several, when it has them;
    /// absent for every other error.
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<Value>,
}

/// The typed parts of `error` a host reads as data, when it has them.
fn store_error_details(error: &StoreError) -> Option<Value> {
    match error {
        StoreError::VerificationBindRefused(refusal) => serde_json::to_value(refusal).ok(),
        _ => None,
    }
}

/// Long-lived host-private service over one project-local store.
pub struct HostControlServer {
    store: SqliteStore,
    project_id: ProjectId,
    actor_id: String,
    session_id: SessionId,
    connection_token: String,
    source_skill: Option<String>,
    actor_context: Option<String>,
    actor_context_normalized: bool,
}

impl HostControlServer {
    /// Attributes this connection's records to the host-asserted execution
    /// context (agent, model, reasoning), normalized exactly as the work
    /// words normalize it. It describes the actor; it never changes the
    /// principal used for assignment or authority.
    #[must_use]
    pub fn with_actor_context(mut self, actor_context: Option<String>) -> Self {
        let (actor_context, normalized) =
            crate::work_service::normalize_actor_context(actor_context);
        self.actor_context = actor_context;
        self.actor_context_normalized = normalized;
        self
    }

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
        Self::open_with_host_path_identity(
            database,
            Some(HostPathPolicy::host_default()),
            project_id,
            actor_id,
            session_id,
            source_skill,
        )
    }

    /// Opens the project store with the project root's resolved filesystem
    /// identity (`None` when unresolved: path intents then fail closed) and
    /// fixes asserted host context for this connection.
    ///
    /// # Errors
    ///
    /// Returns [`StoreError`] when the SQLite store cannot be opened or its
    /// persisted path policy differs from the resolved one.
    pub fn open_with_host_path_identity(
        database: impl AsRef<Path>,
        identity: Option<HostPathPolicy>,
        project_id: ProjectId,
        actor_id: String,
        session_id: SessionId,
        source_skill: Option<String>,
    ) -> Result<Self, StoreError> {
        crate::storage::admit_session_id(&session_id)?;
        let mut store = SqliteStore::open_with_host_path_identity(database, identity)?;
        crate::phase_trace::control::enter(
            crate::phase_trace::control::ControlPhase::ConnectionResume,
        );
        let connection_token = store.resume_control_connection(&session_id, Utc::now())?;
        Ok(Self {
            store,
            project_id,
            actor_id,
            session_id,
            connection_token,
            source_skill,
            actor_context: None,
            actor_context_normalized: false,
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
            HostControlRequest::TurnGrantRead {
                routing_token,
                grant_id,
            } => serde_json::to_value(self.store.read_control_turn_grant(
                &self.project_id,
                &self.session_id,
                &self.connection_token,
                &routing_token,
                &grant_id,
            )?)
            .map_err(StoreError::Json),
            HostControlRequest::NamedRootBind {
                routing_token,
                claim_id,
                claim_fence,
                workspace_id,
                generation,
                named_at,
                kind,
                end_reason,
                idempotency_key,
            } => {
                let mut actor = self.actor("named_root_bind", "bind the run's named source root");
                serde_json::to_value(self.store.bind_named_root(
                    &self.project_id,
                    &self.session_id,
                    &self.connection_token,
                    &routing_token,
                    claim_id,
                    claim_fence,
                    &workspace_id,
                    generation,
                    named_at,
                    kind,
                    end_reason,
                    &mut actor,
                    &idempotency_key,
                    now,
                )?)
                .map_err(StoreError::Json)
            }
            HostControlRequest::NamedRootRead {
                routing_token,
                run_id,
                claim_id,
            } => serde_json::to_value(self.store.read_named_root(
                &self.project_id,
                &self.session_id,
                &self.connection_token,
                &routing_token,
                run_id,
                claim_id,
            )?)
            .map_err(StoreError::Json),
            HostControlRequest::NamedRootSightingRead {
                work_ref,
                run_id,
                run_cut,
            } => serde_json::to_value(self.store.read_named_root_sighting(
                &self.project_id,
                &work_ref,
                run_id,
                run_cut,
            )?)
            .map_err(StoreError::Json),
            HostControlRequest::AcceptanceBindingRead {
                routing_token,
                work_id,
                expected_work_revision,
                run_id,
                after,
            } => serde_json::to_value(self.store.read_acceptance_bindings(
                &self.project_id,
                &self.session_id,
                &self.connection_token,
                &routing_token,
                &crate::storage::BindingReadRequest {
                    work_id,
                    expected_work_revision,
                    run_id,
                    after: after.as_deref(),
                },
            )?)
            .map_err(StoreError::Json),
            HostControlRequest::AcceptanceVerificationRead {
                routing_token,
                work_id,
                expected_work_revision,
                run_id,
                run_cut,
                criterion,
                after,
            } => serde_json::to_value(self.store.read_acceptance_verifications(
                &self.project_id,
                &self.session_id,
                &self.connection_token,
                &routing_token,
                &crate::storage::VerificationReadRequest {
                    work_id,
                    expected_work_revision,
                    run_id,
                    run_cut,
                    criterion,
                    after: after.as_deref(),
                },
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
                let intent_fingerprint = ObjectId::from_str(&intent_fingerprint).map_err(|_| {
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
            HostControlRequest::ExecutionObserve {
                routing_token,
                idempotency_key,
                binding,
                root_basis,
                observed_interval,
                occurrence,
                causality,
                policy_basis,
            } => {
                let mut observer = self.actor(
                    "execution_observe",
                    "record execution observed without admission",
                );
                observer.run_id = Some(binding.run_id.0.to_string());
                serde_json::to_value(self.store.record_unadmitted_execution_observation(
                    &self.project_id,
                    &self.session_id,
                    &self.connection_token,
                    &routing_token,
                    &observer,
                    crate::domain::ExecutionObserveInput {
                        idempotency_key,
                        binding: *binding,
                        root_basis: *root_basis,
                        observed_interval,
                        occurrence: *occurrence,
                        causality,
                        policy_basis: *policy_basis,
                    },
                    now,
                )?)
                .map_err(StoreError::Json)
            }
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
            HostControlRequest::VerificationBind {
                routing_token,
                idempotency_key,
                original,
                measurement,
                targets,
            } => {
                let binder = self.actor(
                    "verification_bind",
                    "bind one passed check to other held items of the same content",
                );
                serde_json::to_value(self.store.bind_verification(
                    &self.project_id,
                    &self.session_id,
                    &self.connection_token,
                    &routing_token,
                    &binder,
                    &crate::domain::VerificationBindInput {
                        idempotency_key,
                        original,
                        measurement,
                        targets,
                    },
                    now,
                )?)
                .map_err(StoreError::Json)
            }
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
            let response = match parse_frame(frame) {
                Ok(request) => self.respond(request),
                Err(refusal) => refusal,
            };
            serde_json::to_writer(&mut writer, &response)?;
            writer.write_all(b"\n")?;
            writer.flush()?;
        }
    }

    /// [`Self::serve`] with the opt-in phase trace: the same responses, byte
    /// for byte, and for each frame, numbered when its first byte arrives,
    /// a trace of where its time went. Waiting idle for a frame is not part
    /// of one.
    ///
    /// # Errors
    ///
    /// Returns an I/O error when the transport itself cannot read or write,
    /// after the frame's terminal trace line.
    pub fn serve_traced(
        &mut self,
        mut reader: impl BufRead,
        mut writer: impl Write,
        trace: &crate::phase_trace::control::ControlTrace,
    ) -> std::io::Result<()> {
        use crate::phase_trace::control::{ControlPhase, Terminal};
        loop {
            if reader.fill_buf()?.is_empty() {
                return Ok(());
            }
            trace.begin_frame();
            let frame = match read_control_frame(&mut reader) {
                Ok(Some(frame)) => frame,
                Ok(None) => {
                    trace.finish(Terminal::Incomplete, None);
                    return Ok(());
                }
                Err(error) => {
                    trace.finish(Terminal::Incomplete, None);
                    return Err(error);
                }
            };
            // The frame has been in handler_total since its first byte. The
            // operation is named before the handler runs, so a frame
            // that stalls in it says which operation it is.
            let response = match parse_frame(frame) {
                Ok(request) => {
                    trace.set_operation(request.operation());
                    self.respond(request)
                }
                Err(refusal) => {
                    trace.set_operation("invalid");
                    refusal
                }
            };
            let outcome = match &response {
                HostControlResponse::Ok { .. } => "ok",
                HostControlResponse::Error { error } => error.code,
            };
            trace.enter(ControlPhase::ResponseSerialize);
            let bytes = match serde_json::to_vec(&response) {
                Ok(bytes) => bytes,
                Err(error) => {
                    trace.finish(Terminal::Incomplete, Some(outcome));
                    return Err(error.into());
                }
            };
            trace.enter(ControlPhase::ResponseWriteFlush);
            let written = writer
                .write_all(&bytes)
                .and_then(|()| writer.write_all(b"\n"))
                .and_then(|()| writer.flush());
            if let Err(error) = written {
                trace.finish(Terminal::WriteFailed, Some(outcome));
                return Err(error);
            }
            trace.finish(Terminal::Complete, Some(outcome));
        }
    }

    /// The response to one parsed request.
    fn respond(&mut self, request: HostControlRequest) -> HostControlResponse {
        match self.handle(request) {
            Ok(result) => HostControlResponse::Ok { result },
            Err(error) => HostControlResponse::Error {
                error: HostControlErrorBody {
                    code: store_error_code(&error),
                    message: error.to_string(),
                    details: store_error_details(&error),
                },
            },
        }
    }

    fn actor(&self, operation: &str, reason: &str) -> ActorContext {
        let mut provenance_chain = vec![ProvenanceLink {
            relation: ProvenanceRelation::AssertedBy,
            source: self.actor_id.clone(),
            reference: Some(self.session_id.0.clone()),
        }];
        // The same two links the work words record, so one session's control
        // and work records carry one attribution.
        if let Some(actor_context) = &self.actor_context {
            provenance_chain.push(ProvenanceLink {
                relation: ProvenanceRelation::DerivedFrom,
                source: actor_context.clone(),
                reference: Some(crate::domain::ACTOR_CONTEXT_PROVENANCE_REFERENCE.into()),
            });
        }
        if self.actor_context_normalized {
            provenance_chain.push(ProvenanceLink {
                relation: ProvenanceRelation::DerivedFrom,
                source: "actor_context:normalized".into(),
                reference: Some(crate::domain::ACTOR_CONTEXT_NORMALIZED_REFERENCE.into()),
            });
        }
        ActorContext {
            actor_id: self.actor_id.clone(),
            // The kind the work words record as, so the words call these
            // records this session's own.
            actor_kind: crate::work_service::WORD_ACTOR_KIND.into(),
            assurance: AssuranceLevel::Asserted,
            run_id: None,
            session_id: Some(self.session_id.clone()),
            source_tool: Some(format!("host-control:{operation}")),
            source_skill: self.source_skill.clone(),
            provenance_chain,
            reason: reason.into(),
        }
    }
}

/// The request one frame holds, or the response refusing it: an oversize
/// frame, or one that does not parse.
fn parse_frame(frame: Result<Vec<u8>, ()>) -> Result<HostControlRequest, HostControlResponse> {
    let refuse = |message: String| HostControlResponse::Error {
        error: HostControlErrorBody {
            code: "invalid_request",
            message,
            details: None,
        },
    };
    match frame {
        Err(()) => Err(refuse(format!(
            "host control frame exceeds {MAX_HOST_CONTROL_FRAME_BYTES} bytes"
        ))),
        Ok(frame) => {
            // An observation request has its own, smaller bound, checked
            // before the request is decoded into its typed shape.
            if frame.len() > crate::domain::MAX_EXECUTION_OBSERVE_REQUEST_BYTES
                && frame_operation(&frame).as_deref() == Some("execution_observe")
            {
                return Err(refuse(format!(
                    "host control request \"execution_observe\" exceeds {} bytes",
                    crate::domain::MAX_EXECUTION_OBSERVE_REQUEST_BYTES
                )));
            }
            parse_host_control_request(&frame).map_err(refuse)
        }
    }
}

/// A frame's top-level `operation`, read without building the rest of the
/// request: every other member is skipped as it is parsed, so an oversized
/// body is never decoded into values. `None` for a frame that is not a JSON
/// object or names no operation.
fn frame_operation(frame: &[u8]) -> Option<String> {
    struct Operation;
    impl<'de> serde::de::Visitor<'de> for Operation {
        type Value = Option<String>;

        fn expecting(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
            formatter.write_str("a host control request object")
        }

        fn visit_map<A: serde::de::MapAccess<'de>>(
            self,
            mut map: A,
        ) -> Result<Self::Value, A::Error> {
            let mut operation = None;
            while let Some(key) = map.next_key::<std::borrow::Cow<'de, str>>()? {
                if key == "operation" && operation.is_none() {
                    operation = Some(map.next_value::<String>()?);
                } else {
                    map.next_value::<serde::de::IgnoredAny>()?;
                }
            }
            Ok(operation)
        }
    }
    let mut deserializer = serde_json::Deserializer::from_slice(frame);
    serde::Deserializer::deserialize_map(&mut deserializer, Operation)
        .ok()
        .flatten()
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

fn parse_host_control_request(frame: &[u8]) -> Result<HostControlRequest, String> {
    serde_json::from_slice(frame).map_err(|error| {
        let operation = host_control_operation_hint(frame);
        let detail = bounded_host_control_error_detail(&error.to_string());
        format!("host control request {operation:?} is invalid: {detail}")
    })
}

fn host_control_operation_hint(frame: &[u8]) -> String {
    let Ok(value) = serde_json::from_slice::<Value>(frame) else {
        return "<invalid-json>".into();
    };
    let Some(operation) = value.get("operation").and_then(Value::as_str) else {
        return "<missing>".into();
    };
    if operation.is_empty()
        || operation.len() > MAX_HOST_CONTROL_OPERATION_BYTES
        || !operation
            .bytes()
            .all(|byte| byte.is_ascii_lowercase() || byte.is_ascii_digit() || byte == b'_')
    {
        return "<invalid>".into();
    }
    operation.to_owned()
}

fn bounded_host_control_error_detail(detail: &str) -> String {
    if detail.len() <= MAX_HOST_CONTROL_ERROR_DETAIL_BYTES {
        return detail.to_owned();
    }
    let mut end = MAX_HOST_CONTROL_ERROR_DETAIL_BYTES.saturating_sub('…'.len_utf8());
    while !detail.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &detail[..end])
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

/// The fixed wire code of a store error.
#[must_use]
pub fn store_error_code(error: &StoreError) -> &'static str {
    match error {
        StoreError::StoreNotInitialized => "store_not_initialized",
        StoreError::InvalidControlSession(_) => "invalid_control_session",
        StoreError::NamedRootBindingRefused(_) => "named_root_binding_refused",
        StoreError::SourceBasisTextRefused { .. } => "source_basis_text_refused",
        StoreError::NamedRootReadRefused(_) => "named_root_read_refused",
        StoreError::ExecutionObservationInvalid(_) => "execution_observation_invalid",
        StoreError::VerificationBindRefused(_) => "verification_bind_refused",
        StoreError::ExecutionObservationBasisMismatch(_) => "execution_observation_basis_mismatch",
        StoreError::ExecutionObservationPolicyBasisMismatch(_) => {
            "execution_observation_policy_basis_mismatch"
        }
        StoreError::AcceptanceBindingReadRefused { refusal, .. } => refusal.code(),
        StoreError::NamedRootSightingReadRefused { refusal, .. } => refusal.code(),
        StoreError::AcceptanceVerificationReadRefused { refusal, .. } => refusal.code(),
        StoreError::HostPathIdentityUnresolved => "host_path_identity_unresolved",
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
        StoreError::ControlObservationScopeMismatch { .. } => "observation_scope_mismatch",
        StoreError::VerificationProducerObservationNotFound(_) => "verification_producer_not_found",
        StoreError::EnvironmentFingerprintMismatch => "environment_fingerprint_mismatch",
        StoreError::EnvironmentEvidenceNotFound(_) => "environment_evidence_not_found",
        StoreError::EnvironmentBasisMismatch(_) => "environment_basis_mismatch",
        StoreError::ControlTurnGrantNotFound(_) => "turn_grant_not_found",
        StoreError::ControlTurnGrantSessionMismatch => "turn_grant_session_mismatch",
        StoreError::InvalidTurnGrantId => "invalid_turn_grant_id",
        StoreError::AcceptanceEvaluationRefused { .. }
        | StoreError::AcceptanceEvaluationCarriedFailure { .. } => "acceptance_evaluation_refused",
        // A standing blocking evaluation answers as an evaluation refusal;
        // the other typed admission causes stay storage errors below.
        StoreError::AcceptanceEvaluationAdmissionRefused { cause, .. }
            if matches!(
                **cause,
                crate::domain::AcceptanceEvaluationAdmissionCause::Reroll(_)
            ) =>
        {
            "acceptance_evaluation_refused"
        }
        StoreError::AcceptanceEvaluationBasisMoved { moved, .. } => moved.code(),
        StoreError::DifferentBuildSchema | StoreError::InvalidControlProjection(_) => {
            "control_projection_invalid"
        }
        StoreError::TaskAccessDenied { .. } => "task_access_denied",
        StoreError::ProjectMemoryExists(_) => "memory_exists",
        StoreError::ProjectMemoryRevisionConflict { .. } => "memory_revision_conflict",
        StoreError::ProjectMemoryRevisionNotFound { .. } => "memory_revision_not_found",
        StoreError::ProjectMemorySectionNotFound(_) => "memory_section_not_found",
        StoreError::ProjectMemoryRetired(_) => "memory_retired",
        StoreError::ProjectMemoryNotFound(_) => "memory_not_found",
        StoreError::ProjectMemoryBindingInvalid => "memory_binding_invalid",
        StoreError::InvalidProjectMemory(_) => "memory_invalid",
        StoreError::WorkClaimMismatch { .. } => "work_claim_mismatch",
        StoreError::WorkClaimLapsed { .. } => "work_claim_lapsed",
        StoreError::WorkCompletionRecoveryRequired { .. } => "work_completion_recovery_required",
        StoreError::WorkReferenceAmbiguous { .. } => "work_reference_ambiguous",
        StoreError::WorkImplicitTargetConflict(_) => "work_implicit_target_conflict",
        StoreError::WorkBareTargetAmbiguous(_) => "work_bare_target_ambiguous",
        StoreError::WorkCatalogCursorInvalid { .. } => "work_catalog_cursor_invalid",
        StoreError::WorkShowCursorInvalid { .. } => "work_show_cursor_invalid",
        StoreError::WorkNoteReferenceInvalid { .. } => "work_note_reference_invalid",
        StoreError::WorkCriterionLinkInvalid { .. } => "work_criterion_link_invalid",
        StoreError::WorkNoteTooLarge { .. } => "work_note_too_large",
        StoreError::Json(_)
        | StoreError::Sqlite(_)
        | StoreError::WorkWriterAdmissionRefused { .. }
        | StoreError::SqlitePath { .. }
        | StoreError::SqliteFile { .. }
        | StoreError::StoreFileIo { .. }
        | StoreError::ImmutableCollision(_)
        | StoreError::ObjectKindMismatch { .. }
        | StoreError::InvalidStoredKey(_)
        | StoreError::NoteIdempotencyConflict(_)
        | StoreError::EmptyNote
        | StoreError::RedactionRefused(_)
        | StoreError::InvalidMemoryProjection(_)
        | StoreError::InvalidTaskProjection(_)
        | StoreError::NoActiveTask(_)
        | StoreError::MemoryNotFound(_)
        | StoreError::MemoryAccessDenied(_)
        | StoreError::ControlPolicyConflict { .. }
        | StoreError::WorkNotFound(_)
        | StoreError::InvalidWork(_)
        | StoreError::InvalidWorkProjection(_)
        | StoreError::WorkRevisionConflict { .. }
        | StoreError::WorkOperationIdempotencyConflict { .. }
        | StoreError::WorkDecompositionRetryConflict { .. }
        | StoreError::WorkDependencyCycle
        | StoreError::WorkPrerequisiteAlreadySatisfied(_)
        | StoreError::WorkNotOpen(_)
        | StoreError::WorkParentNotOpen { .. }
        | StoreError::WorkAncestorNotOpen { .. }
        | StoreError::WorkDetachRefused { .. }
        | StoreError::WorkRejectRefused { .. }
        | StoreError::WorkPeerDecompositionRefused { .. }
        | StoreError::WorkClaimHeld { .. }
        | StoreError::WorkReleaseWaiverRequired { .. }
        | StoreError::WorkCompletionRefused { .. }
        | StoreError::WorkBoundVerificationRefused { .. }
        | StoreError::AcceptanceEvaluationAdmissionRefused { .. }
        | StoreError::AcceptanceCriteriaRequired { .. }
        | StoreError::GraphDestinationNotEmpty
        | StoreError::GraphProjectMismatch { .. }
        | StoreError::GraphDifferentBuild
        | StoreError::InvalidGraphSnapshot(_)
        | StoreError::OpenWorkObligations { .. } => "storage_error",
    }
}

#[cfg(test)]
mod tests;
