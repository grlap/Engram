//! Host-private newline-delimited JSON transport for Execution Observability.
//!
//! This surface is separate from agent-facing MCP. The embedding host supplies
//! asserted session and actor context; it does not authenticate their identity.

use std::{
    io::{BufRead, Write},
    path::Path,
};

use chrono::Utc;
use serde::{Deserialize, Serialize};
use serde_json::Value;

use crate::{
    ActorContext, ControlWorkBinding, HostPathPolicy, NamedRootBindingKind, NamedRootEndReason,
    ProjectId, SessionId, SqliteStore, WorkClaimId, WorkRunId,
    domain::{AssuranceLevel, ProvenanceLink, ProvenanceRelation},
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
    /// Read the ordinary host channel context and supported evidence protocol.
    WorkSessionRead {},
    /// Asks which store this serving connection opened. It needs no routing
    /// token, grant or bind and writes nothing to the store. It refuses a
    /// superseded connection, and otherwise always refuses with
    /// `control_connection_identity_unavailable`: no platform can yet prove
    /// which file the connection holds, so it never returns a path.
    ControlConnectionRead {},
    NamedRootBind {
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
        work_id: crate::WorkId,
        expected_work_revision: i64,
        run_id: WorkRunId,
        run_cut: i64,
        criterion: usize,
        #[serde(default)]
        after: Option<String>,
    },
    /// Records execution the host observed without admission: a turn seen
    /// after it started, a workspace change between turns, or a check inside
    /// such a turn. It records a fact, never a grant, a begin or a turn
    /// result.
    ExecutionObserve {
        idempotency_key: String,
        binding: Box<ControlWorkBinding>,
        root_basis: Box<crate::domain::ObservationRootBasis>,
        observed_interval: crate::domain::ObservedInterval,
        occurrence: Box<crate::domain::ObservedOccurrence>,
        causality: crate::domain::ObservationCausality,
        policy_basis: Box<crate::domain::ObservationPolicyBasis>,
    },
    /// Binds one native passed check to other items this session holds whose
    /// named roots hold the same content: one record per target, all or
    /// nothing. It creates no observation, turn or focus change.
    VerificationBind {
        idempotency_key: String,
        original: crate::ObjectId,
        measurement: crate::domain::BindMeasurement,
        targets: Vec<crate::domain::VerificationBindTarget>,
    },
    VerificationRecord {
        operation_key: String,
        registration: Box<crate::domain::RegisteredVerificationAssociation>,
        input: crate::domain::RegisteredVerificationInput,
        run: crate::domain::RegisteredVerificationRun,
        checks: Vec<crate::domain::RegisteredVerificationCheck>,
        post_run: crate::domain::RegisteredVerificationPostRun,
    },
    /// Records one delivery of a test result from another host: its immutable
    /// terminal facts, and the destination and use the host states. Only a
    /// qualifying delivery mints verification.
    RemoteVerificationRecord {
        operation_key: String,
        completion: Box<crate::domain::RemoteRunCompletion>,
        association: Box<crate::domain::RemoteVerificationAssociation>,
    },
}

impl HostControlRequest {
    /// The protocol operation this request names, as a fixed label.
    #[must_use]
    pub const fn operation(&self) -> &'static str {
        match self {
            Self::WorkSessionRead {} => "work_session_read",
            Self::ControlConnectionRead {} => "control_connection_read",
            Self::NamedRootBind { .. } => "named_root_bind",
            Self::NamedRootRead { .. } => "named_root_read",
            Self::NamedRootSightingRead { .. } => "named_root_sighting_read",
            Self::AcceptanceBindingRead { .. } => "acceptance_binding_read",
            Self::AcceptanceVerificationRead { .. } => "acceptance_verification_read",
            Self::ExecutionObserve { .. } => "execution_observe",
            Self::VerificationBind { .. } => "verification_bind",
            Self::VerificationRecord { .. } => "verification_record",
            Self::RemoteVerificationRecord { .. } => "remote_verification_record",
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
    /// A stable named-root read refusal reason; absent for other errors.
    #[serde(skip_serializing_if = "Option::is_none")]
    reason: Option<crate::storage::NamedRootReadRefusal>,
    /// The typed parts of a refusal that names several, when it has them;
    /// absent for every other error.
    #[serde(skip_serializing_if = "Option::is_none")]
    details: Option<Value>,
}

fn store_error_reason(error: &StoreError) -> Option<crate::storage::NamedRootReadRefusal> {
    match error {
        StoreError::NamedRootReadRefused(reason) => Some(*reason),
        _ => None,
    }
}

/// The typed parts of `error` a host reads as data, when it has them.
fn store_error_details(error: &StoreError) -> Option<Value> {
    match error {
        StoreError::RegisteredVerificationRefused { field, reason }
        | StoreError::RemoteVerificationInvalid { field, reason } => {
            Some(serde_json::json!({"field": field, "reason": reason}))
        }
        StoreError::VerificationBindRefused(refusal) => serde_json::to_value(refusal).ok(),
        _ => None,
    }
}

/// A terminal unwind at the control boundary; no application outcome is implied.
#[derive(Debug, thiserror::Error)]
#[error("Engram control stopped after an internal panic; the operation outcome is unknown")]
pub struct ControlPanic;

/// Long-lived host-private service over one project-local store.
pub struct HostControlServer {
    store: SqliteStore,
    diagnostic_store: crate::diagnostics::Fingerprint,
    project_id: ProjectId,
    actor_id: String,
    session_id: SessionId,
    connection_token: String,
    source_skill: Option<String>,
    actor_context: Option<String>,
    actor_context_normalized: bool,
    diagnostics: Option<crate::diagnostics::Diagnostics>,
    diagnostic_frame: Option<crate::diagnostics::control::Frame>,
    diagnostic_sequence: u64,
    terminal: bool,
}

impl HostControlServer {
    #[cfg(test)]
    pub(crate) fn from_test_store(
        store: SqliteStore,
        project_id: ProjectId,
        session_id: SessionId,
        connection_token: String,
    ) -> Self {
        Self {
            store,
            diagnostic_store: crate::diagnostics::Fingerprint::store(Path::new(":memory:")),
            project_id,
            actor_id: "runner".into(),
            session_id,
            connection_token,
            source_skill: None,
            actor_context: None,
            actor_context_normalized: false,
            diagnostics: None,
            diagnostic_frame: None,
            diagnostic_sequence: 0,
            terminal: false,
        }
    }

    /// Answers at most one bounded frame after startup has failed, without
    /// decoding or dispatching its operation. The caller must then exit with
    /// a failure status and retain the original diagnostic on stderr.
    ///
    /// These codes are terminal startup refusals, never live-server errors.
    /// They do not imply that earlier startup work made no durable changes.
    /// The embedding host bounds the wait for input with its request deadline.
    ///
    /// # Errors
    ///
    /// Returns an I/O error on transport failure or oversized input. Empty
    /// EOF produces no response; bounded partial EOF still produces a refusal.
    pub fn serve_startup_failure(
        error: &StoreError,
        mut reader: impl BufRead,
        mut writer: impl Write,
    ) -> std::io::Result<()> {
        match read_control_frame_with_policy(&mut reader, OversizedFramePolicy::Stop)? {
            None => return Ok(()),
            Some(Err(())) => {
                return Err(std::io::Error::new(
                    std::io::ErrorKind::InvalidData,
                    "host control startup frame exceeds the maximum size",
                ));
            }
            Some(Ok(_)) => {}
        }
        let (code, message) = if matches!(
            error,
            StoreError::Sqlite(error)
                if error.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy)
        ) {
            (
                "control_startup_busy",
                "Engram control startup could not acquire the store.",
            )
        } else {
            (
                "control_startup_refused",
                "Engram control startup was refused; inspect the startup diagnostic.",
            )
        };
        let response = HostControlResponse::Error {
            error: HostControlErrorBody {
                code,
                message: message.into(),
                reason: None,
                details: None,
            },
        };
        serde_json::to_writer(&mut writer, &response)?;
        writer.write_all(b"\n")?;
        writer.flush()
    }

    /// Adds a bounded diagnostic sink selected by the process's explicit home.
    #[must_use]
    pub fn with_diagnostics(mut self, sink: crate::diagnostics::Diagnostics) -> Self {
        self.diagnostics = Some(sink);
        self
    }

    fn observe_fault(&self, fault: crate::diagnostics::Fault) {
        if let (Some(sink), Some(frame)) = (&self.diagnostics, &self.diagnostic_frame) {
            frame.emit(sink, fault);
        }
    }
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
        Self::open_host_session(
            database,
            identity,
            project_id,
            actor_id,
            session_id,
            source_skill,
        )
    }

    /// Opens the ordinary host evidence channel for Execution Observability.
    ///
    /// # Errors
    /// Returns a storage error when the store or connection fence cannot be opened.
    pub fn open_work_session_with_host_path_identity(
        database: impl AsRef<Path>,
        identity: Option<HostPathPolicy>,
        project_id: ProjectId,
        actor_id: String,
        session_id: SessionId,
        source_skill: Option<String>,
    ) -> Result<Self, StoreError> {
        Self::open_host_session(
            database,
            identity,
            project_id,
            actor_id,
            session_id,
            source_skill,
        )
    }

    fn open_host_session(
        database: impl AsRef<Path>,
        identity: Option<HostPathPolicy>,
        project_id: ProjectId,
        actor_id: String,
        session_id: SessionId,
        source_skill: Option<String>,
    ) -> Result<Self, StoreError> {
        crate::storage::admit_session_id(&session_id)?;
        let diagnostic_store = crate::diagnostics::Fingerprint::store(database.as_ref());
        let mut store = SqliteStore::open_for_control_host(database, identity)?;
        crate::phase_trace::control::enter(
            crate::phase_trace::control::ControlPhase::ConnectionResume,
        );
        let connection_token = store.resume_work_connection(&session_id, Utc::now())?;
        Ok(Self {
            store,
            diagnostic_store,
            project_id,
            actor_id,
            session_id,
            connection_token,
            source_skill,
            actor_context: None,
            actor_context_normalized: false,
            diagnostics: None,
            diagnostic_frame: None,
            diagnostic_sequence: 0,
            terminal: false,
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
        if self.terminal {
            return Err(StoreError::InvalidHostContext(
                "control service has terminated; open a fresh connection".into(),
            ));
        }
        let _budget = crate::storage::ControlRequestScope::ensure()?;
        let now = Utc::now();
        match request {
            HostControlRequest::WorkSessionRead {} => {
                self.store
                    .verify_current_control_connection(&self.session_id, &self.connection_token)?;
                self.with_verification_applicability(serde_json::json!({
                    "project_id": self.project_id,
                    "session_id": self.session_id,
                    "work_session": true,
                }))
            }
            HostControlRequest::VerificationRecord {
                operation_key,
                registration,
                input,
                run,
                checks,
                post_run,
            } => {
                let actor = self.actor("verification_record", "record a registered host run");
                serde_json::to_value(self.store.record_registered_verification(
                    &self.project_id,
                    &self.session_id,
                    &self.connection_token,
                    crate::storage::HostSessionAuthority::Work,
                    &actor,
                    &crate::domain::RegisteredVerificationRequest {
                        operation_key,
                        registration: *registration,
                        input,
                        run,
                        checks,
                        post_run,
                    },
                    now,
                )?)
                .map_err(StoreError::Json)
            }
            HostControlRequest::RemoteVerificationRecord {
                operation_key,
                completion,
                association,
            } => {
                let actor = self.actor("remote_verification_record", "record a remote test result");
                serde_json::to_value(self.store.record_remote_verification(
                    &self.project_id,
                    &self.session_id,
                    &self.connection_token,
                    crate::storage::HostSessionAuthority::Work,
                    &actor,
                    &crate::domain::RemoteVerificationRequest {
                        operation_key,
                        completion: *completion,
                        association: *association,
                    },
                    now,
                )?)
                .map_err(StoreError::Json)
            }
            HostControlRequest::ControlConnectionRead {} => self.control_connection_read(),
            HostControlRequest::NamedRootBind {
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
                    crate::storage::HostSessionAuthority::Work,
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
            HostControlRequest::NamedRootRead { run_id, claim_id } => {
                serde_json::to_value(self.store.read_named_root(
                    &self.project_id,
                    &self.session_id,
                    &self.connection_token,
                    crate::storage::HostSessionAuthority::Work,
                    run_id,
                    claim_id,
                )?)
                .map_err(StoreError::Json)
            }
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
                work_id,
                expected_work_revision,
                run_id,
                after,
            } => serde_json::to_value(self.store.read_acceptance_bindings(
                &self.project_id,
                &self.session_id,
                &self.connection_token,
                crate::storage::HostSessionAuthority::Work,
                &crate::storage::BindingReadRequest {
                    work_id,
                    expected_work_revision,
                    run_id,
                    after: after.as_deref(),
                },
            )?)
            .map_err(StoreError::Json),
            HostControlRequest::AcceptanceVerificationRead {
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
                crate::storage::HostSessionAuthority::Work,
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
            HostControlRequest::ExecutionObserve {
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
                    crate::storage::HostSessionAuthority::Work,
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
            HostControlRequest::VerificationBind {
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
                    crate::storage::HostSessionAuthority::Work,
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
    pub fn serve(&mut self, reader: impl BufRead, writer: impl Write) -> std::io::Result<()> {
        self.serve_observed(reader, writer, None)
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
        reader: impl BufRead,
        writer: impl Write,
        trace: &crate::phase_trace::control::ControlTrace,
    ) -> std::io::Result<()> {
        self.serve_observed(reader, writer, Some(trace))
    }

    fn serve_observed(
        &mut self,
        reader: impl BufRead,
        writer: impl Write,
        trace: Option<&crate::phase_trace::control::ControlTrace>,
    ) -> std::io::Result<()> {
        if self.terminal {
            return Err(std::io::Error::other(
                "Engram control service has terminated; open a fresh connection",
            ));
        }
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            self.serve_frames(reader, writer, trace)
        }));
        let frame = self.diagnostic_frame.take();
        if matches!(result, Ok(Ok(()))) {
            return Ok(());
        }
        // An unwind can leave handler state unsafe to reuse. Ordinary I/O
        // errors end this transport invocation; fresh transport may retry.
        self.terminal = result.is_err();
        if let Some(sink) = &self.diagnostics {
            let frame = frame.as_ref();
            let persisted = sink.control_abnormal_exit(
                if result.is_err() {
                    "panic"
                } else {
                    "transport_failure"
                },
                frame.map_or("unknown", |frame| frame.operation),
                frame.map_or(crate::diagnostics::Correlation::default(), |frame| {
                    frame.correlation
                }),
                frame.map_or("idle_read", |frame| frame.phase),
                frame.map_or(std::time::Duration::ZERO, |frame| {
                    crate::storage::control_diagnostic_now()
                        .saturating_duration_since(frame.started)
                }),
            );
            if persisted.is_err() {
                eprintln!("Engram control abnormal-exit diagnostic could not be persisted");
            }
        }
        match result {
            Ok(result) => result,
            Err(_) => Err(std::io::Error::other(ControlPanic)),
        }
    }

    fn serve_frames(
        &mut self,
        mut reader: impl BufRead,
        mut writer: impl Write,
        trace: Option<&crate::phase_trace::control::ControlTrace>,
    ) -> std::io::Result<()> {
        use crate::phase_trace::control::{ControlPhase, Terminal};
        loop {
            let available = match reader.fill_buf() {
                Ok(bytes) => !bytes.is_empty(),
                Err(error) => {
                    if let Some(sink) = &self.diagnostics {
                        sink.emit(crate::diagnostics::Event::failure(
                            "unknown",
                            crate::diagnostics::Correlation::default(),
                            std::time::Duration::ZERO,
                            crate::diagnostics::Fault::transport_failed(),
                        ));
                    }
                    return Err(error);
                }
            };
            if !available {
                self.diagnostic_frame = None;
                return Ok(());
            }
            self.diagnostic_sequence = self.diagnostic_sequence.saturating_add(1);
            // The first byte is available. Keep parsing and all transaction
            // admission on this frame's allowance; do not reset it at dispatch.
            let _budget = crate::storage::ControlRequestScope::frame();
            let diagnostic_timing = self
                .diagnostics
                .as_ref()
                .map(|_| crate::storage::ControlDiagnosticScope::enter());
            self.diagnostic_frame = self
                .diagnostics
                .as_ref()
                .map(|_| crate::diagnostics::control::Frame::new(self.diagnostic_sequence));
            if let Some(trace) = trace {
                trace.begin_frame();
            }
            let frame = match read_control_frame(&mut reader) {
                Ok(Some(frame)) => frame,
                Ok(None) => {
                    if let Some(trace) = trace {
                        trace.finish(Terminal::Incomplete, None);
                    }
                    self.observe_fault(crate::diagnostics::Fault::incomplete());
                    return Ok(());
                }
                Err(error) => {
                    if let Some(trace) = trace {
                        trace.finish(Terminal::Incomplete, None);
                    }
                    self.observe_fault(crate::diagnostics::Fault::transport_failed());
                    return Err(error);
                }
            };
            // The frame has been in handler_total since its first byte. The
            // operation is named before the handler runs, so a frame
            // that stalls in it says which operation it is.
            let response = match parse_frame(frame) {
                Ok(request) => {
                    if let Some(trace) = trace {
                        trace.set_operation(request.operation());
                    }
                    if let Some(frame) = &mut self.diagnostic_frame {
                        frame.request(&request);
                        frame.phase = "handler";
                    }
                    let _transaction_scope = self.diagnostics.as_ref().and_then(|sink| {
                        self.diagnostic_frame.as_ref().map(|frame| {
                            crate::diagnostics::transaction::Scope::enter(
                                crate::diagnostics::transaction::Context::new(
                                    sink.clone(),
                                    frame.operation,
                                    frame.correlation,
                                    self.diagnostic_store,
                                ),
                            )
                        })
                    });
                    self.respond(request)
                }
                Err(refusal) => {
                    if let Some(trace) = trace {
                        trace.set_operation("invalid");
                    }
                    self.observe_fault(crate::diagnostics::Fault::invalid_request());
                    refusal
                }
            };
            let outcome = match &response {
                HostControlResponse::Ok { .. } => "ok",
                HostControlResponse::Error { error } => error.code,
            };
            if let (Some(frame), Some(timing)) = (&mut self.diagnostic_frame, &diagnostic_timing) {
                frame.handler_finished(timing.snapshot());
            }
            if let Some(frame) = &mut self.diagnostic_frame {
                frame.phase = "response_write";
            }
            if let Some(trace) = trace {
                trace.enter(ControlPhase::ResponseSerialize);
            }
            if let Some(trace) = trace {
                let bytes = match serde_json::to_vec(&response) {
                    Ok(bytes) => bytes,
                    Err(error) => {
                        trace.finish(Terminal::Incomplete, Some(outcome));
                        self.observe_fault(crate::diagnostics::Fault::incomplete());
                        return Err(error.into());
                    }
                };
                trace.enter(ControlPhase::ResponseWriteFlush);
                if let Err(error) = writer.write_all(&bytes) {
                    trace.finish(Terminal::WriteFailed, Some(outcome));
                    self.observe_fault(crate::diagnostics::Fault::send_failed());
                    return Err(error);
                }
            } else if let Err(error) = stream_control_response(&mut writer, &response) {
                self.observe_fault(if error.is_io() {
                    crate::diagnostics::Fault::send_failed()
                } else {
                    crate::diagnostics::Fault::incomplete()
                });
                return Err(error.into());
            }
            if let Some(frame) = &mut self.diagnostic_frame {
                frame.phase = "response_flush";
            }
            let written = writer.write_all(b"\n").and_then(|()| writer.flush());
            if let Err(error) = written {
                if let Some(trace) = trace {
                    trace.finish(Terminal::WriteFailed, Some(outcome));
                }
                self.observe_fault(crate::diagnostics::Fault::send_failed());
                return Err(error);
            }
            if let Some(trace) = trace {
                trace.finish(Terminal::Complete, Some(outcome));
            }
            if let (Some(sink), Some(frame)) = (&self.diagnostics, self.diagnostic_frame.take()) {
                frame.complete(sink);
            }
        }
    }

    /// Which store this serving connection opened. It first checks, on one
    /// read snapshot, that the connection is still the session's current one.
    /// No platform can yet prove that the file at the store's path is the
    /// file SQLite holds: a path observed around the open can be redirected
    /// during it. So it then always refuses, rather than return a path that
    /// might name another file. It writes nothing to the store.
    fn control_connection_read(&self) -> Result<Value, StoreError> {
        self.store
            .verify_current_control_connection(&self.session_id, &self.connection_token)?;
        Err(StoreError::ControlConnectionIdentityUnavailable(
            "no platform can yet prove which file this connection opened".to_owned(),
        ))
    }

    /// The response to one parsed request.
    fn with_verification_applicability(&self, mut value: Value) -> Result<Value, StoreError> {
        value["capabilities"] = serde_json::json!([
            "host_work_v1",
            "verification_record_v1",
            "remote_verification_record_v1"
        ]);
        if self.store.verification_git_tree_applicability_available()? {
            value["verification_evidence_git_tree_applicability_v1"] = Value::Bool(true);
        }
        Ok(value)
    }

    fn respond(&mut self, request: HostControlRequest) -> HostControlResponse {
        match self.handle(request) {
            Ok(result) => HostControlResponse::Ok { result },
            Err(error) => {
                self.observe_fault(crate::diagnostics::Fault::store(&error));
                HostControlResponse::Error {
                    error: HostControlErrorBody {
                        code: store_error_code(&error),
                        message: error.to_string(),
                        reason: store_error_reason(&error),
                        details: store_error_details(&error),
                    },
                }
            }
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

/// The default path streams JSON directly, without a response-sized buffer.
fn stream_control_response(
    writer: &mut impl Write,
    response: &HostControlResponse,
) -> serde_json::Result<()> {
    serde_json::to_writer(writer, response)
}

/// The request one frame holds, or the response refusing it: an oversize
/// frame, or one that does not parse.
fn parse_frame(frame: Result<Vec<u8>, ()>) -> Result<HostControlRequest, HostControlResponse> {
    let refuse = |message: String| HostControlResponse::Error {
        error: HostControlErrorBody {
            code: "invalid_request",
            message,
            reason: None,
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
    read_control_frame_with_policy(reader, OversizedFramePolicy::Drain)
}

#[derive(Clone, Copy)]
enum OversizedFramePolicy {
    Drain,
    Stop,
}

fn read_control_frame_with_policy(
    reader: &mut impl BufRead,
    policy: OversizedFramePolicy,
) -> std::io::Result<Option<Result<Vec<u8>, ()>>> {
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
        // A failed startup cannot recover another frame. Stop after the
        // first excess byte rather than draining an arbitrarily long line.
        let available = match policy {
            OversizedFramePolicy::Drain => available,
            OversizedFramePolicy::Stop => {
                &available[..available
                    .len()
                    .min(MAX_HOST_CONTROL_FRAME_BYTES + 1 - frame.len())]
            }
        };
        let newline = available.iter().position(|byte| *byte == b'\n');
        let consumed = newline.map_or(available.len(), |position| position + 1);
        let oversized = frame.len().saturating_add(consumed) > MAX_HOST_CONTROL_FRAME_BYTES;
        if !oversized {
            frame.extend_from_slice(&available[..consumed]);
        }
        reader.consume(consumed);
        if oversized {
            if newline.is_none() && matches!(policy, OversizedFramePolicy::Drain) {
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
        StoreError::RegisteredVerificationRefused { .. } => "verification_record_refused",
        StoreError::RemoteVerificationInvalid { .. } => "invalid_request",
        StoreError::ControlRequestExpired => "control_request_expired",
        StoreError::SearchBudgetExhausted { .. } => "search_budget_exhausted",
        StoreError::SearchCancelled => "search_cancelled",
        StoreError::StoreNotInitialized => "store_not_initialized",
        StoreError::InvalidHostContext(_) => "invalid_host_context",
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
        StoreError::ControlConnectionSuperseded(_) => "control_connection_superseded",
        StoreError::ControlConnectionIdentityUnavailable(_) => {
            "control_connection_identity_unavailable"
        }
        StoreError::ControlOperationIdempotencyConflict { .. } => {
            "control_operation_idempotency_conflict"
        }
        StoreError::ControlWorkBindingStale { .. } => "stale_fence",
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
        StoreError::RedactionRefused(_) => "redaction_refused",
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
        // The host wire keeps the classified refusal's own code; the family
        // is reported only by the CLI's structured --json rendering.
        StoreError::TaskSyncRefused { inner, .. } => store_error_code(inner),
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
        | StoreError::InvalidMemoryProjection(_)
        | StoreError::InvalidTaskProjection(_)
        | StoreError::MemoryNotFound(_)
        | StoreError::MemoryAccessDenied(_)
        | StoreError::ControlPolicyConflict { .. }
        | StoreError::WorkNotFound(_)
        | StoreError::InvalidWork(_)
        | StoreError::WorkHandoffPending { .. }
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
pub(crate) mod id_contract;
#[cfg(test)]
mod tests;
