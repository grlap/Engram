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
}

impl HostControlRequest {
    /// The protocol operation this request names, as a fixed label.
    #[must_use]
    pub const fn operation(&self) -> &'static str {
        match self {
            Self::SessionBind { .. } => "session_bind",
            Self::SessionStatus { .. } => "session_status",
            Self::NamedRootBind { .. } => "named_root_bind",
            Self::NamedRootRead { .. } => "named_root_read",
            Self::NamedRootSightingRead { .. } => "named_root_sighting_read",
            Self::AcceptanceBindingRead { .. } => "acceptance_binding_read",
            Self::AcceptanceVerificationRead { .. } => "acceptance_verification_read",
            Self::TurnEvaluate { .. } => "turn_evaluate",
            Self::TurnBegin { .. } => "turn_begin",
            Self::ExecutionObserve { .. } => "execution_observe",
            Self::TurnCheckpoint { .. } => "turn_checkpoint",
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
        StoreError::AcceptanceEvaluationBasisMoved { moved, .. } => {
            crate::verbs::error_rendering::evaluation_basis_move_code(*moved)
        }
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
        StoreError::WorkCatalogCursorInvalid { .. } => "work_catalog_cursor_invalid",
        StoreError::WorkShowCursorInvalid { .. } => "work_show_cursor_invalid",
        StoreError::WorkNoteReferenceInvalid { .. } => "work_note_reference_invalid",
        StoreError::WorkCriterionLinkInvalid { .. } => "work_criterion_link_invalid",
        StoreError::WorkNoteTooLarge { .. } => "work_note_too_large",
        StoreError::Json(_)
        | StoreError::Sqlite(_)
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
mod tests {
    use std::io::Cursor;

    use super::*;

    #[test]
    fn different_build_refusal_preserves_host_wire_code() {
        let error = StoreError::DifferentBuildSchema;
        assert_eq!(store_error_code(&error), "control_projection_invalid");
        assert!(!error.to_string().contains("invalid data"));
    }

    #[test]
    fn moved_evaluation_basis_preserves_both_host_wire_codes() {
        for (moved, expected) in [
            (
                crate::EvaluationBasisMove::CheckRecorded,
                "acceptance_evaluation_resubmit",
            ),
            (
                crate::EvaluationBasisMove::SourceChanged,
                "acceptance_evaluation_void",
            ),
        ] {
            let error = StoreError::AcceptanceEvaluationBasisMoved {
                work: crate::WorkId::new(),
                moved,
                reason: "evaluation basis moved".into(),
                observation: None,
            };
            let response = HostControlResponse::Error {
                error: HostControlErrorBody {
                    code: store_error_code(&error),
                    message: error.to_string(),
                },
            };
            let wire = serde_json::to_value(response).expect("host response");
            assert_eq!(wire["error"]["code"], expected);
        }
    }

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
            actor_context: None,
            actor_context_normalized: false,
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

    #[test]
    fn removed_host_obligation_waiver_is_refused() {
        let clean = serde_json::json!({
            "operation": "obligation_waive",
            "routing_token": "routing-token",
            "obligation_id": uuid::Uuid::nil().to_string(),
            "expected_definition": "a".repeat(64),
            "waived_by": "operator",
            "reason": "reviewed exception",
            "idempotency_key": "waive-once"
        });
        let error = parse_host_control_request(&serde_json::to_vec(&clean).expect("encode frame"))
            .expect_err("removed operation must fail closed");
        assert!(error.contains("obligation_waive"));
        assert!(error.contains("unknown variant"));
    }

    #[test]
    fn typed_evaluation_admission_preserves_host_private_error_code() {
        let cause = serde_json::from_value(serde_json::json!({
            "kind": "eligibility",
            "mismatch": "mode_disallowed",
            "requested_mode": "same_session",
            "task_mark": null,
            "admitted_modes": ["independent_session"],
            "remedy": "request_eligible_evaluation",
        }))
        .expect("typed admission cause");
        let error = StoreError::AcceptanceEvaluationAdmissionRefused {
            work: crate::domain::WorkId::new(),
            reason: "unchanged admission reason".into(),
            cause: Box::new(cause),
        };
        assert_eq!(store_error_code(&error), "storage_error");
        // A standing blocking evaluation answers as an evaluation refusal.
        let reroll = serde_json::from_value(serde_json::json!({
            "kind": "reroll",
            "mismatch": "blocking_evaluation_stands",
            "evaluation": "0".repeat(32),
            "feed": {"kind": "run_execution", "id": uuid::Uuid::nil()},
            "after_position": 3,
            "through_position": 5,
            "criterion": 1,
            "verdict": "fail",
            "remedy": "record_new_evidence_then_evaluate",
        }))
        .expect("typed re-roll cause");
        let error = StoreError::AcceptanceEvaluationAdmissionRefused {
            work: crate::domain::WorkId::new(),
            reason: "unchanged re-roll reason".into(),
            cause: Box::new(reroll),
        };
        assert_eq!(store_error_code(&error), "acceptance_evaluation_refused");
    }

    /// The documented `execution_observe` frame: an unadmitted turn with a
    /// source change and one check, its cause asserted by the host.
    fn execution_observe_frame() -> Value {
        serde_json::json!({
            "operation": "execution_observe",
            "routing_token": "routing-token",
            "idempotency_key": "termal-turn-17-observed",
            "binding": {
                "root_execution_id": uuid::Uuid::new_v4(),
                "work_id": uuid::Uuid::new_v4(),
                "run_id": uuid::Uuid::new_v4(),
                "work_revision": 3,
                "claim_id": uuid::Uuid::new_v4(),
                "claim_fence": 2
            },
            "root_basis": {
                "capture_run_cut": 12,
                "latest_event": null,
                "state": {"state": "none"}
            },
            "observed_interval": {
                "from": "2026-10-02T03:00:00Z",
                "through": "2026-10-02T03:05:00Z"
            },
            "occurrence": {
                "kind": "unadmitted_turn",
                "host_turn_ref": "termal-turn-17",
                "source_change": {
                    "detection": "content_comparison",
                    "workspace_id": "workspace-A",
                    "baseline": {
                        "workspace_id": "workspace-A",
                        "source_revision": "rev-a",
                        "observed_at": "2026-10-02T03:00:00Z"
                    },
                    "sighting": {
                        "source_basis": {
                            "workspace_id": "workspace-A",
                            "source_revision": "rev-b"
                        },
                        "observed_at": "2026-10-02T03:04:00Z"
                    }
                },
                "observed_checks": [{
                    "host_check_id": "cargo-test",
                    "check_kind": "test",
                    "observed_result": "passed",
                    "started_at": "2026-10-02T03:01:00Z",
                    "finished_at": "2026-10-02T03:03:00Z",
                    "observed_at": "2026-10-02T03:03:00Z",
                    "host_evidence_ref": "termal://check-log/17"
                }]
            },
            "causality": {
                "kind": "host_assertion",
                "claimed_actor": {
                    "actor_id": "greg/claude",
                    "actor_kind": "agent",
                    "assurance": "asserted",
                    "run_id": null,
                    "session_id": "session-7284",
                    "source_tool": null,
                    "source_skill": null,
                    "provenance_chain": [],
                    "reason": "the host saw the session's terminal"
                },
                "basis": "terminal ownership"
            },
            "policy_basis": {"mode": "audit_only"}
        })
    }

    #[test]
    fn execution_observe_frame_parses_strictly() {
        let frame = execution_observe_frame();
        let parsed = parse_host_control_request(&serde_json::to_vec(&frame).expect("frame"))
            .expect("the documented frame parses");
        assert_eq!(parsed.operation(), "execution_observe");
        assert!(matches!(
            parsed,
            HostControlRequest::ExecutionObserve {
                ref occurrence,
                causality: crate::domain::ObservationCausality::HostAssertion { .. },
                ref policy_basis,
                ..
            } if matches!(**occurrence, crate::domain::ObservedOccurrence::UnadmittedTurn { .. })
                && **policy_basis == crate::domain::ObservationPolicyBasis::AuditOnly {}
        ));
        // Nothing a caller sends can claim credit, a grant or verified cause.
        for (pointer, field, value) in [
            (
                "/occurrence/observed_checks/0",
                "credit",
                serde_json::json!("credited"),
            ),
            ("", "grant_id", serde_json::json!("grant-1")),
            ("/occurrence", "turn_status", serde_json::json!("succeeded")),
        ] {
            let mut altered = frame.clone();
            altered
                .pointer_mut(pointer)
                .and_then(Value::as_object_mut)
                .expect("object")
                .insert(field.into(), value);
            let error = parse_host_control_request(&serde_json::to_vec(&altered).expect("frame"))
                .expect_err("an unknown field is refused");
            assert!(error.contains(field), "{error}");
        }
        let mut verified = frame;
        verified["causality"] = serde_json::json!({"kind": "verified"});
        assert!(
            parse_host_control_request(&serde_json::to_vec(&verified).expect("frame")).is_err()
        );
    }

    // Fields the request types reuse are as strict as the rest: an unknown
    // field inside the root state, the asserted actor or one of its
    // provenance links is refused, never dropped.
    #[test]
    fn execution_observe_refuses_unknown_nested_fields() {
        let frame = execution_observe_frame();
        for (pointer, field) in [
            ("/root_basis/state", "generation_hint"),
            ("/causality/claimed_actor", "verified_by"),
        ] {
            let mut altered = frame.clone();
            altered
                .pointer_mut(pointer)
                .and_then(Value::as_object_mut)
                .expect("object")
                .insert(field.into(), serde_json::json!("x"));
            let error = parse_host_control_request(&serde_json::to_vec(&altered).expect("frame"))
                .expect_err("an unknown nested field is refused");
            assert!(error.contains(field), "{error}");
        }
        let mut linked = frame;
        linked["causality"]["claimed_actor"]["provenance_chain"] = serde_json::json!([
            {"relation": "asserted_by", "source": "host", "reference": null, "weight": 1}
        ]);
        let error = parse_host_control_request(&serde_json::to_vec(&linked).expect("frame"))
            .expect_err("an unknown provenance field is refused");
        assert!(error.contains("weight"), "{error}");
    }

    // A duplicate key anywhere in the request is refused, as it is for typed
    // fields, never collapsed to its last value.
    #[test]
    fn execution_observe_refuses_duplicate_nested_keys() {
        let mut frame = execution_observe_frame();
        frame["causality"]["claimed_actor"]["provenance_chain"] = serde_json::json!([
            {"relation": "asserted_by", "source": "host", "reference": null}
        ]);
        let text = serde_json::to_string(&frame).expect("frame");
        for (needle, doubled) in [
            (
                r#""state":{"state":"none"}"#,
                r#""state":{"state":"none","state":"none"}"#,
            ),
            (
                r#""actor_id":"greg/claude""#,
                r#""actor_id":"someone-else","actor_id":"greg/claude""#,
            ),
            (
                r#""relation":"asserted_by""#,
                r#""relation":"asserted_by","relation":"asserted_by""#,
            ),
        ] {
            assert_eq!(text.matches(needle).count(), 1, "{needle}");
            let altered = text.replacen(needle, doubled, 1);
            let error = parse_host_control_request(altered.as_bytes())
                .expect_err("a duplicate key is refused");
            assert!(error.contains("duplicate"), "{needle}: {error}");
        }
    }

    // A field beside a field-less tag is refused too, and the field-less
    // shapes serialize exactly as their tag.
    #[test]
    fn execution_observe_refuses_fields_beside_a_bare_tag() {
        let frame = execution_observe_frame();
        for (pointer, bare, extra) in [
            (
                "/causality",
                serde_json::json!({"kind": "unknown"}),
                ("basis", serde_json::json!("dropped cause")),
            ),
            (
                "/policy_basis",
                serde_json::json!({"mode": "audit_only"}),
                ("project_policy_epoch", serde_json::json!(1)),
            ),
        ] {
            let mut accepted = frame.clone();
            *accepted.pointer_mut(pointer).expect("field") = bare.clone();
            parse_host_control_request(&serde_json::to_vec(&accepted).expect("frame"))
                .expect("the bare tag parses");
            let mut altered = accepted;
            altered
                .pointer_mut(pointer)
                .and_then(Value::as_object_mut)
                .expect("object")
                .insert(extra.0.into(), extra.1);
            let error = parse_host_control_request(&serde_json::to_vec(&altered).expect("frame"))
                .expect_err("a field beside a bare tag is refused");
            assert!(error.contains(extra.0), "{error}");
        }
        assert_eq!(
            serde_json::to_value(crate::domain::ObservationCausality::Unknown {}).expect("json"),
            serde_json::json!({"kind": "unknown"})
        );
        assert_eq!(
            serde_json::to_value(crate::domain::ObservationPolicyBasis::AuditOnly {})
                .expect("json"),
            serde_json::json!({"mode": "audit_only"})
        );
    }

    #[test]
    fn the_frame_discriminator_reads_only_the_operation() {
        let frame = serde_json::to_vec(&execution_observe_frame()).expect("frame");
        assert_eq!(
            frame_operation(&frame).as_deref(),
            Some("execution_observe")
        );
        assert_eq!(frame_operation(b"[1, 2]"), None);
        assert_eq!(frame_operation(b"{\"routing_token\": \"r\"}"), None);
        assert_eq!(frame_operation(b"{\"operation\": "), None);
    }

    #[test]
    fn an_oversized_execution_observe_frame_is_refused_before_decoding() {
        let mut frame = execution_observe_frame();
        frame["occurrence"]["host_turn_ref"] =
            Value::String("x".repeat(crate::domain::MAX_EXECUTION_OBSERVE_REQUEST_BYTES));
        let bytes = serde_json::to_vec(&frame).expect("frame");
        assert!(bytes.len() < MAX_HOST_CONTROL_FRAME_BYTES);
        let refused = parse_frame(Ok(bytes)).expect_err("refused");
        let HostControlResponse::Error { error } = refused else {
            panic!("an error response");
        };
        assert_eq!(error.code, "invalid_request");
        assert!(
            error.message.contains("execution_observe"),
            "{}",
            error.message
        );
        assert!(error.message.contains("65536"), "{}", error.message);
    }

    #[test]
    fn execution_observation_refusals_answer_with_distinct_codes() {
        for (error, code) in [
            (
                StoreError::ExecutionObservationInvalid("shape".into()),
                "execution_observation_invalid",
            ),
            (
                StoreError::ExecutionObservationBasisMismatch("basis".into()),
                "execution_observation_basis_mismatch",
            ),
            (
                StoreError::ExecutionObservationPolicyBasisMismatch("policy".into()),
                "execution_observation_policy_basis_mismatch",
            ),
        ] {
            assert_eq!(store_error_code(&error), code);
        }
    }

    #[test]
    fn named_root_binding_frame_carries_claim_and_workspace_identity() {
        let frame = serde_json::json!({
            "operation": "named_root_bind",
            "routing_token": "routing-token",
            "claim_id": uuid::Uuid::new_v4(),
            "claim_fence": 4,
            "workspace_id": r"\\?\C:\source-root",
            "generation": 7,
            "named_at": "2026-09-28T00:00:00Z",
            "kind": "bound",
            "idempotency_key": "claim-generation-7-bound"
        });
        let parsed = parse_host_control_request(&serde_json::to_vec(&frame).expect("frame"))
            .expect("host accepts the claim-scoped frame");
        assert!(matches!(
            parsed,
            HostControlRequest::NamedRootBind { workspace_id, generation: 7, .. }
                if workspace_id == r"\\?\C:\source-root"
        ));
        let mut altered = frame;
        altered["source_path"] = serde_json::json!("C:\\source-root");
        let error = parse_host_control_request(&serde_json::to_vec(&altered).expect("frame"))
            .expect_err("a path alias cannot stand in for the host workspace identity");
        assert!(error.contains("source_path"));
    }

    #[test]
    fn acceptance_binding_read_frame_takes_no_caller_cut() {
        let frame = serde_json::json!({
            "operation": "acceptance_binding_read",
            "routing_token": "routing-token",
            "work_id": uuid::Uuid::new_v4(),
            "expected_work_revision": 3,
            "run_id": uuid::Uuid::new_v4(),
        });
        let parsed = parse_host_control_request(&serde_json::to_vec(&frame).expect("frame"))
            .expect("a first page names no continuation");
        assert!(matches!(
            parsed,
            HostControlRequest::AcceptanceBindingRead {
                expected_work_revision: 3,
                after: None,
                ..
            }
        ));
        let mut continued = frame.clone();
        continued["after"] = serde_json::json!("abr1-00");
        assert!(matches!(
            parse_host_control_request(&serde_json::to_vec(&continued).expect("frame"))
                .expect("a continuation"),
            HostControlRequest::AcceptanceBindingRead { after: Some(ref token), .. }
                if token == "abr1-00"
        ));
        let mut cut = frame;
        cut["run_cut"] = serde_json::json!(12);
        let error = parse_host_control_request(&serde_json::to_vec(&cut).expect("frame"))
            .expect_err("the first page captures the cut; a caller cannot name one");
        assert!(error.contains("run_cut"), "{error}");
    }

    #[test]
    fn named_root_sighting_read_frame_needs_no_routing_token() {
        let frame = serde_json::json!({
            "operation": "named_root_sighting_read",
            "work_ref": "w-0123456789ab",
            "run_id": uuid::Uuid::new_v4(),
        });
        let parsed = parse_host_control_request(&serde_json::to_vec(&frame).expect("frame"))
            .expect("the read takes no routing token");
        assert!(matches!(
            parsed,
            HostControlRequest::NamedRootSightingRead { ref work_ref, run_cut: None, .. }
                if work_ref == "w-0123456789ab"
        ));
        assert_eq!(parsed.operation(), "named_root_sighting_read");
        let mut at_cut = frame.clone();
        at_cut["run_cut"] = serde_json::json!(12);
        assert!(matches!(
            parse_host_control_request(&serde_json::to_vec(&at_cut).expect("frame"))
                .expect("a caller cut"),
            HostControlRequest::NamedRootSightingRead {
                run_cut: Some(12),
                ..
            }
        ));
        let mut token = frame;
        token["routing_token"] = serde_json::json!("routing-token");
        let error = parse_host_control_request(&serde_json::to_vec(&token).expect("frame"))
            .expect_err("an unknown field is refused");
        assert!(error.contains("routing_token"), "{error}");
    }

    #[test]
    fn named_root_sighting_read_refusals_answer_with_distinct_codes() {
        use crate::domain::NamedRootSightingReadRefusal as Refusal;
        let refusals = [
            Refusal::InvalidWorkRef,
            Refusal::WrongRun,
            Refusal::InvalidCut,
            Refusal::ResponseTooLarge,
        ];
        let codes: std::collections::BTreeSet<&str> = refusals
            .iter()
            .map(|refusal| {
                store_error_code(&StoreError::NamedRootSightingReadRefused {
                    refusal: *refusal,
                    reason: "reason".into(),
                })
            })
            .collect();
        assert_eq!(codes.len(), refusals.len());
        assert!(
            codes
                .iter()
                .all(|code| code.starts_with("named_root_sighting_read_"))
        );
    }

    #[test]
    fn acceptance_binding_read_refusals_answer_with_distinct_codes() {
        use crate::domain::AcceptanceBindingReadRefusal as Refusal;
        let refusals = [
            Refusal::UnknownWork,
            Refusal::WrongProject,
            Refusal::WrongRevision,
            Refusal::WrongRun,
            Refusal::StaleCut,
            Refusal::InvalidCursor,
            Refusal::CursorBasisMismatch,
            Refusal::PageTooLarge,
        ];
        let codes: std::collections::BTreeSet<&str> = refusals
            .iter()
            .map(|refusal| {
                store_error_code(&StoreError::AcceptanceBindingReadRefused {
                    refusal: *refusal,
                    reason: "reason".into(),
                })
            })
            .collect();
        assert_eq!(codes.len(), refusals.len());
        assert!(
            codes
                .iter()
                .all(|code| code.starts_with("acceptance_binding_read_"))
        );
    }

    #[test]
    fn acceptance_verification_read_frame_names_its_cut_and_criterion_strictly() {
        let frame = serde_json::json!({
            "operation": "acceptance_verification_read",
            "routing_token": "routing-token",
            "work_id": uuid::Uuid::new_v4(),
            "expected_work_revision": 3,
            "run_id": uuid::Uuid::new_v4(),
            "run_cut": 12,
            "criterion": 2,
        });
        let parsed = parse_host_control_request(&serde_json::to_vec(&frame).expect("frame"))
            .expect("a first page names no continuation");
        assert_eq!(parsed.operation(), "acceptance_verification_read");
        assert!(matches!(
            parsed,
            HostControlRequest::AcceptanceVerificationRead {
                expected_work_revision: 3,
                run_cut: 12,
                criterion: 2,
                after: None,
                ..
            }
        ));
        let mut continued = frame.clone();
        continued["after"] = serde_json::json!("avr1-00");
        assert!(matches!(
            parse_host_control_request(&serde_json::to_vec(&continued).expect("frame"))
                .expect("a continuation"),
            HostControlRequest::AcceptanceVerificationRead { after: Some(ref token), .. }
                if token == "avr1-00"
        ));
        // The cut and criterion are required; a negative criterion, a
        // caller-chosen kind and a caller-chosen project are wire errors.
        let mut negative = frame.clone();
        negative["criterion"] = serde_json::json!(-1);
        let error = parse_host_control_request(&serde_json::to_vec(&negative).expect("frame"))
            .expect_err("a negative criterion");
        assert!(error.contains("expected usize"), "{error}");
        for field in ["check_kind", "project_id"] {
            let mut altered = frame.clone();
            altered[field] = serde_json::json!("caller-chosen");
            let error = parse_host_control_request(&serde_json::to_vec(&altered).expect("frame"))
                .expect_err("an unknown field");
            assert!(error.contains(field), "{field}: {error}");
        }
        for missing in ["run_cut", "criterion"] {
            let mut altered = frame.clone();
            altered
                .as_object_mut()
                .expect("frame object")
                .remove(missing);
            let error = parse_host_control_request(&serde_json::to_vec(&altered).expect("frame"))
                .expect_err("a frame without its cut or criterion");
            assert!(error.contains(missing), "{missing}: {error}");
        }
    }

    #[test]
    fn acceptance_verification_read_refusals_answer_with_distinct_codes() {
        use crate::domain::AcceptanceVerificationReadRefusal as Refusal;
        let refusals = [
            Refusal::UnknownWork,
            Refusal::WrongProject,
            Refusal::WrongRevision,
            Refusal::WrongRun,
            Refusal::StaleCut,
            Refusal::InvalidCriterion,
            Refusal::InvalidCursor,
            Refusal::CursorBasisMismatch,
            Refusal::PageTooLarge,
        ];
        let codes: std::collections::BTreeSet<&str> = refusals
            .iter()
            .map(|refusal| {
                store_error_code(&StoreError::AcceptanceVerificationReadRefused {
                    refusal: *refusal,
                    reason: "reason".into(),
                })
            })
            .collect();
        assert_eq!(codes.len(), refusals.len());
        assert!(
            codes
                .iter()
                .all(|code| code.starts_with("acceptance_verification_read_"))
        );
    }

    #[test]
    fn host_control_frames_reject_duplicate_fields_without_unbounded_diagnostics() {
        let duplicate_top_level = br#"{
            "operation":"session_status",
            "operation":"session_status",
            "routing_token":"routing-token"
        }"#;
        let error = parse_host_control_request(duplicate_top_level)
            .expect_err("duplicate top-level field must fail closed");
        assert!(error.contains("duplicate field"));

        let duplicate_nested = br#"{
            "operation":"turn_evaluate",
            "routing_token":"routing-token",
            "purpose":"ordinary",
            "intent_fingerprint":"unused",
            "requested_effects":["observe"],
            "resource_intents":[{
                "kind":"logical",
                "namespace":"workspace",
                "namespace":"other-workspace",
                "segments":["src"],
                "coverage":"exact"
            }],
            "idempotency_key":"turn-once"
        }"#;
        let error = parse_host_control_request(duplicate_nested)
            .expect_err("duplicate nested field must fail closed");
        assert!(error.contains("duplicate field"));

        let oversized_operation = serde_json::json!({
            "operation": "x".repeat(MAX_HOST_CONTROL_OPERATION_BYTES + 1),
        });
        let error = parse_host_control_request(
            &serde_json::to_vec(&oversized_operation).expect("encode oversized operation"),
        )
        .expect_err("unknown oversized operation must fail closed");
        assert!(error.contains("<invalid>"));
        assert!(error.len() < 512);
    }

    #[test]
    fn host_control_frames_reject_unknown_nested_fields() {
        let misspelled_components = serde_json::json!({
            "operation": "turn_checkpoint",
            "routing_token": "routing-token",
            "grant_id": "grant-id",
            "next_intent": "continue",
            "observations": [],
            "verification_evidence": [],
            "environment_evidence": [{
                "source_basis": {
                    "workspace_id": "workspace",
                    "source_revision": "revision"
                },
                "environment_fingerprint": "a".repeat(64),
                "componentz": {
                    "toolchain": "stable",
                    "workspace_id": "workspace",
                    "capability_map_revision": 1
                },
                "observed_at": "2026-09-02T00:00:00Z"
            }],
            "idempotency_key": "checkpoint-once"
        });
        let error = parse_host_control_request(
            &serde_json::to_vec(&misspelled_components).expect("encode nested typo"),
        )
        .expect_err("misspelled optional components field must fail closed");
        assert!(error.contains("turn_checkpoint"));
        assert!(error.contains("componentz"));

        let extra_resource_field = serde_json::json!({
            "operation": "turn_evaluate",
            "routing_token": "routing-token",
            "purpose": "ordinary",
            "intent_fingerprint": "a".repeat(64),
            "requested_effects": ["observe"],
            "resource_intents": [{
                "kind": "logical",
                "namespace": "workspace",
                "segments": ["src"],
                "coverage": "exact",
                "unexpected": true
            }],
            "idempotency_key": "turn-once"
        });
        let error = parse_host_control_request(
            &serde_json::to_vec(&extra_resource_field).expect("encode extra resource field"),
        )
        .expect_err("unknown resource-subject field must fail closed");
        assert!(error.contains("turn_evaluate"));
        assert!(error.contains("unexpected"));
    }

    fn turn_evaluate_frame(purpose: Option<&str>, key: &str) -> Value {
        let mut frame = serde_json::json!({
            "operation": "turn_evaluate",
            "routing_token": "routing-token",
            "idempotency_key": key,
            "intent_fingerprint": ObjectId::from_canonical_bytes(key.as_bytes()).as_str(),
            "requested_effects": ["observe"],
        });
        if let Some(purpose) = purpose {
            frame["purpose"] = purpose.into();
        }
        frame
    }

    // While hosts move off the removed fields, a turn request may still name
    // the only purpose; a recovery turn no longer parses.
    #[test]
    fn turn_evaluate_takes_an_ordinary_or_absent_purpose_and_refuses_recovery() {
        for purpose in [None, Some("ordinary")] {
            let frame = turn_evaluate_frame(purpose, "turn-once");
            assert!(
                matches!(
                    parse_host_control_request(&serde_json::to_vec(&frame).expect("encode")),
                    Ok(HostControlRequest::TurnEvaluate { .. })
                ),
                "purpose {purpose:?} must parse"
            );
        }
        let frame = turn_evaluate_frame(Some("recovery"), "turn-once");
        let error = parse_host_control_request(&serde_json::to_vec(&frame).expect("encode"))
            .expect_err("a recovery turn must be refused");
        assert!(error.contains("turn_evaluate"));
        assert!(error.contains("recovery"));
    }

    // Grants carry no delivery page. A begin may omit the tokens or send
    // none; a begin naming a token is refused as outside the grant.
    #[test]
    fn turn_begin_takes_no_delivery_tokens_and_refuses_any() {
        let directory = crate::test_support::temp_home().expect("temp");
        let mut server = HostControlServer::open_with_host_path_identity(
            directory.path().join("control.sqlite3"),
            None,
            ProjectId("wire-transition".into()),
            "agent".into(),
            SessionId("wire-session".into()),
            None,
        )
        .expect("open the control connection");
        let mut call = |frame: Value| {
            let request = parse_host_control_request(&serde_json::to_vec(&frame).expect("encode"))
                .expect("a valid frame");
            server.handle(request).expect("handled")
        };
        let bound = call(serde_json::json!({
            "operation": "session_bind",
            "external_ref": "dummy:WIRE",
            "title": "Wire transition",
            "assurance": "turn_gated",
            "mediated_effects": ["observe", "communicate"],
            "capability_map_revision": 1,
            "idempotency_key": "bind-wire",
        }));
        assert_eq!(bound["status"]["phase"], "ready");
        assert!(bound["status"].get("confirmed_cursor").is_none());
        let routing_token = bound["routing_token"].as_str().expect("token").to_owned();
        let grant_for = |key: &str, call: &mut dyn FnMut(Value) -> Value| {
            let mut frame = turn_evaluate_frame(None, key);
            frame["routing_token"] = routing_token.clone().into();
            let decision = call(frame);
            assert_eq!(decision["decision"], "grant", "{decision}");
            assert!(decision["grant"].get("delivery").is_none());
            decision["grant"]["grant_id"]
                .as_str()
                .expect("grant id")
                .to_owned()
        };
        let begin = |grant_id: &str, tokens: Option<Value>, key: &str| {
            let mut frame = serde_json::json!({
                "operation": "turn_begin",
                "routing_token": routing_token.clone(),
                "grant_id": grant_id,
                "idempotency_key": key,
            });
            if let Some(tokens) = tokens {
                frame["delivery_tokens"] = tokens;
            }
            frame
        };

        let first = grant_for("turn-echoes-a-token", &mut call);
        let refused = call(begin(
            &first,
            Some(serde_json::json!(["token-from-an-old-page"])),
            "begin-with-a-token",
        ));
        assert_eq!(refused["decision"], "refuse");
        assert_eq!(refused["code"], "grant_scope_mismatch");

        for (key, tokens) in [("absent", None), ("empty", Some(serde_json::json!([])))] {
            let grant = grant_for(&format!("turn-{key}-tokens"), &mut call);
            let started = call(begin(&grant, tokens, &format!("begin-{key}-tokens")));
            assert_eq!(started["decision"], "begin", "{started}");
            let reported = call(serde_json::json!({
                "operation": "turn_checkpoint",
                "routing_token": routing_token.clone(),
                "grant_id": grant,
                "next_intent": "continue",
                "idempotency_key": format!("checkpoint-{key}-tokens"),
            }));
            assert_eq!(reported["decision"], "checkpointed", "{reported}");
            assert_eq!(
                reported["receipt"]["confirmed_cursor"],
                reported["receipt"]["cursor"]
            );
        }
    }

    #[test]
    fn host_control_open_refuses_an_oversized_session_before_store_open() {
        let directory = crate::test_support::temp_home().expect("temp");
        let path = directory.path().join("control.sqlite3");
        let giant = SessionId("h".repeat(65));
        let Err(error) = HostControlServer::open_with_host_path_identity(
            &path,
            None,
            ProjectId("control-admission".into()),
            "actor".into(),
            giant.clone(),
            None,
        ) else {
            panic!("oversized control session must be refused")
        };
        assert!(matches!(
            error,
            StoreError::InvalidWork(ref reason)
                if reason == crate::SessionIdAdmissionError::TooLong.as_str()
        ));
        assert!(!error.to_string().contains(&giant.0));
        assert!(!path.exists());
    }

    // The host passes one execution context to every channel of a session. The
    // control connection attributes its records to it exactly as the work
    // words do, and an unsafe value is normalized rather than refused.
    #[test]
    fn host_control_attributes_its_records_to_the_supplied_actor_context() {
        let directory = crate::test_support::temp_home().expect("temp");
        let open = |name: &str| {
            HostControlServer::open_with_host_path_identity(
                directory.path().join(name),
                None,
                ProjectId("control-context".into()),
                "greg/claude".into(),
                SessionId("session-context".into()),
                None,
            )
            .expect("open the control connection")
        };
        let context_of = |actor: &ActorContext| {
            actor
                .provenance_chain
                .iter()
                .find(|link| {
                    link.reference.as_deref()
                        == Some(crate::domain::ACTOR_CONTEXT_PROVENANCE_REFERENCE)
                })
                .map(|link| link.source.clone())
        };

        let plain = open("plain.sqlite3").actor("turn_begin", "begin");
        assert_eq!(context_of(&plain), None);

        let attributed = open("attributed.sqlite3")
            .with_actor_context(Some("agent=claude;model=fable;reasoning=high".into()))
            .actor("turn_begin", "begin");
        assert_eq!(
            context_of(&attributed).as_deref(),
            Some("agent=claude;model=fable;reasoning=high")
        );
        assert_eq!(attributed.actor_id, "greg/claude");
        attributed
            .validate_attribution_context()
            .expect("a valid attribution");

        let normalized = open("normalized.sqlite3")
            .with_actor_context(Some("agent=claude\u{7};model=fable".into()))
            .actor("turn_begin", "begin");
        assert_eq!(
            context_of(&normalized).as_deref(),
            Some("agent=claude ;model=fable")
        );
        assert!(normalized.provenance_chain.iter().any(|link| {
            link.reference.as_deref() == Some(crate::domain::ACTOR_CONTEXT_NORMALIZED_REFERENCE)
        }));
        normalized
            .validate_attribution_context()
            .expect("a normalized attribution stays valid");
    }
}
