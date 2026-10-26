//! An opt-in phase trace for the MCP server: where each tool call's time went.
//!
//! It is enabled once, at `engram mcp` start, by `ENGRAM_MCP_PHASE_TRACE=1`.
//! Each tool call then runs with a per-call accumulator that the store hooks
//! fill. Bounded stage lines expose unfinished calls; a settled handler also
//! gets a final summary. Numeric IDs are associated only when unambiguous;
//! string IDs are never retained. Unset, no
//! accumulator is ever installed, so every hook returns at once without
//! taking a timestamp, no profile callback is registered, the transport is
//! not wrapped, and nothing is written.
//!
//! The record names phases, counts and durations only: never SQL, a path,
//! parameters or a request or response body. Durations are elapsed wall time
//! and overlap where a phase contains another; they are not summed. Tracing
//! never changes an operation's result: a failure to write a record is
//! dropped.
//! Shutdown milestones use the same best-effort queue and a separate record
//! kind. Their monotonic times measure when stages were reached, not when the
//! stderr reader received them; a missing line never proves a stalled stage.

use std::cell::RefCell;
use std::future::Future;
use std::io::Write as _;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use rmcp::model::{ClientNotification, ClientRequest, JsonRpcMessage, NumberOrString};
use rmcp::service::{RoleServer, RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;

mod call;

/// The environment variable that enables the trace at server start.
pub const PHASE_TRACE_ENV: &str = "ENGRAM_MCP_PHASE_TRACE";

/// Fixed shutdown boundaries observed only when the MCP trace is enabled.
#[derive(Clone, Copy, Debug)]
pub enum ShutdownStage {
    InputEnded,
    TransportCloseStarted,
    TransportCloseFinished,
    TraceFlushStarted,
    TraceFlushFinished,
    ServiceWaitingReturned,
    StoreOwnerDropStarted,
    StoreOwnerDropFinished,
}

impl ShutdownStage {
    const fn label(self) -> &'static str {
        match self {
            Self::InputEnded => "input_ended",
            Self::TransportCloseStarted => "transport_close_started",
            Self::TransportCloseFinished => "transport_close_finished",
            Self::TraceFlushStarted => "trace_flush_started",
            Self::TraceFlushFinished => "trace_flush_finished",
            Self::ServiceWaitingReturned => "service_waiting_returned",
            Self::StoreOwnerDropStarted => "store_owner_drop_started",
            Self::StoreOwnerDropFinished => "store_owner_drop_finished",
        }
    }
}

/// Waits for the service loop, then releases the retained store owner.
///
/// `RunningService::waiting` consumes its service. The caller keeps one
/// independent owner of the work service so that this trace separates the
/// completed service join from destruction of its cached SQLite connection.
/// The drop stays synchronous and on the ordinary shutdown path.
pub async fn wait_then_drop_store_owner<T, F>(
    trace: &PhaseTrace,
    store_owner: T,
    waiting: F,
) -> F::Output
where
    F: Future,
{
    let stopped = waiting.await;
    trace.shutdown_stage(ShutdownStage::ServiceWaitingReturned);
    trace.shutdown_stage(ShutdownStage::StoreOwnerDropStarted);
    drop(store_owner);
    trace.shutdown_stage(ShutdownStage::StoreOwnerDropFinished);
    stopped
}

/// The maximum shared receive, handler, settled-summary and quarantine slots.
const MAX_PENDING: usize = 256;

/// The most bytes one record line may take.
pub const MAX_RECORD_BYTES: usize = 4 * 1_024;

/// The tool labels a record may carry; any other name is recorded as
/// `unknown`, so a record never echoes caller-chosen text.
const TOOL_LABELS: &[&str] = &[
    "next", "ls", "show", "add", "claim", "update", "gate", "evaluate", "note", "done", "handoff",
    "remember", "memories", "forget", "search",
];

/// A phase the store and server hooks time inside one call.
/// When adding a variant, also extend `Phases` and the worker list in `merge`.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum Phase {
    /// Opening a store connection, schema checks included.
    StoreOpen,
    /// Waiting for the service's shared store connection.
    StoreMutexWait,
    /// `BEGIN IMMEDIATE`, including any wait for the write lock.
    BeginImmediate,
    /// `COMMIT`.
    Commit,
    /// Rendering the receipt into the tool result.
    ReceiptSerialize,
    /// A work read snapshot, including nested/reused snapshots.
    ReadSnapshot,
    /// Every statement covered by the existing SQLite profile callback.
    SqliteStatement,
    /// Project-home lookup and the local backup reminder's filesystem work.
    BackupReminder,
}

/// Count, total and longest duration of one phase within a call.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Summary {
    pub count: u32,
    pub total: Duration,
    pub max: Duration,
}

impl Summary {
    fn add(&mut self, elapsed: Duration) {
        self.count = self.count.saturating_add(1);
        self.total = self.total.saturating_add(elapsed);
        self.max = self.max.max(elapsed);
    }
}

/// What one call's hooks recorded.
#[derive(Clone, Copy, Debug, Default, Eq, PartialEq)]
pub struct Phases {
    pub store_open: Summary,
    pub store_mutex_wait: Summary,
    pub begin_immediate: Summary,
    pub commit: Summary,
    pub receipt_serialize: Summary,
    pub read_snapshot: Summary,
    pub sqlite_statement: Summary,
    pub backup_reminder: Summary,
}

impl Phases {
    fn summary(&mut self, phase: Phase) -> &mut Summary {
        match phase {
            Phase::StoreOpen => &mut self.store_open,
            Phase::StoreMutexWait => &mut self.store_mutex_wait,
            Phase::BeginImmediate => &mut self.begin_immediate,
            Phase::Commit => &mut self.commit,
            Phase::ReceiptSerialize => &mut self.receipt_serialize,
            Phase::ReadSnapshot => &mut self.read_snapshot,
            Phase::SqliteStatement => &mut self.sqlite_statement,
            Phase::BackupReminder => &mut self.backup_reminder,
        }
    }
}

thread_local! {
    /// The accumulator of the call being polled on this thread, if any.
    static CURRENT: RefCell<Option<Phases>> = const { RefCell::new(None) };
}

/// Whether a traced call is being polled on this thread.
pub(crate) fn active() -> bool {
    CURRENT.with(|current| current.borrow().is_some())
}

/// Capture synchronous work on a blocking thread using the existing accumulator.
pub(crate) fn capture<T>(read: impl FnOnce() -> T) -> (T, Phases) {
    let mut phases = Some(Phases::default());
    let previous = CURRENT.with(|current| current.replace(phases.take()));
    let installed = Installed {
        phases: &mut phases,
        previous,
    };
    let result = read();
    drop(installed);
    (result, phases.unwrap_or_default())
}

/// Merge a joined worker's phases into its awaiting request's accumulator.
pub(crate) fn merge(mut joined: Phases) {
    CURRENT.with_borrow_mut(|current| {
        if let Some(current) = current {
            for phase in [
                Phase::StoreOpen,
                Phase::StoreMutexWait,
                Phase::BeginImmediate,
                Phase::Commit,
                Phase::ReceiptSerialize,
                Phase::ReadSnapshot,
                Phase::SqliteStatement,
                Phase::BackupReminder,
            ] {
                let source = *joined.summary(phase);
                let target = current.summary(phase);
                target.count = target.count.saturating_add(source.count);
                target.total = target.total.saturating_add(source.total);
                target.max = target.max.max(source.max);
            }
        }
    });
}

/// The start of a timed phase, or `None` when no traced call is running, in
/// which case no timestamp is taken.
pub(crate) fn start() -> Option<Instant> {
    active().then(Instant::now)
}

/// Records the phase begun at `started`, when there was one.
pub(crate) fn finish(phase: Phase, started: Option<Instant>) {
    if let Some(started) = started {
        record(phase, started.elapsed());
    }
}

/// An empty guard outside a traced poll: no timestamp or allocation.
pub(crate) fn span(phase: Phase) -> Span {
    Span {
        phase,
        started: start(),
    }
}

pub(crate) struct Span {
    phase: Phase,
    started: Option<Instant>,
}

impl Drop for Span {
    fn drop(&mut self) {
        finish(self.phase, self.started);
    }
}

fn record(phase: Phase, elapsed: Duration) {
    CURRENT.with(|current| {
        if let Some(phases) = current.borrow_mut().as_mut() {
            phases.summary(phase).add(elapsed);
        }
    });
}

/// The SQLite profile callback: it classifies a finished statement by its
/// leading keywords and records only the category and its duration. The
/// statement text is never kept.
pub(crate) fn sql_profile(event: rusqlite::trace::TraceEvent<'_>) {
    #[cfg(test)]
    crate::storage::work_cost::sqlite::observe(&event);
    let rusqlite::trace::TraceEvent::Profile(statement, elapsed) = event else {
        return;
    };
    if !active() {
        return;
    }
    record(Phase::SqliteStatement, elapsed);
    let sql = statement.sql();
    let words: Vec<&str> = sql.split_ascii_whitespace().take(2).collect();
    let phase = match words.as_slice() {
        [begin, immediate]
            if begin.eq_ignore_ascii_case("BEGIN")
                && immediate
                    .trim_end_matches(';')
                    .eq_ignore_ascii_case("IMMEDIATE") =>
        {
            Phase::BeginImmediate
        }
        [commit, ..] if commit.trim_end_matches(';').eq_ignore_ascii_case("COMMIT") => {
            Phase::Commit
        }
        _ => return,
    };
    record(phase, elapsed);
}

/// Runs `inner` with its own accumulator installed on whichever thread polls
/// it, and returns its output with what the hooks recorded.
pub(crate) fn scoped<F: Future>(inner: F) -> Scoped<F> {
    Scoped {
        inner: Box::pin(inner),
        phases: Some(Phases::default()),
    }
}

/// A future that installs its call's accumulator only while it is polled, so
/// no record outlives a poll on a thread or leaks to another task.
pub(crate) struct Scoped<F> {
    inner: Pin<Box<F>>,
    phases: Option<Phases>,
}

/// Puts the previous accumulator back, even if the poll unwinds.
struct Installed<'a> {
    phases: &'a mut Option<Phases>,
    previous: Option<Phases>,
}

impl Drop for Installed<'_> {
    fn drop(&mut self) {
        let previous = self.previous.take();
        *self.phases = CURRENT.with(|current| current.replace(previous));
    }
}

impl<F: Future> Future for Scoped<F> {
    type Output = (F::Output, Phases);

    fn poll(self: Pin<&mut Self>, context: &mut Context<'_>) -> Poll<Self::Output> {
        let this = self.get_mut();
        let previous = CURRENT.with(|current| current.replace(this.phases.take()));
        let installed = Installed {
            phases: &mut this.phases,
            previous,
        };
        let polled = this.inner.as_mut().poll(context);
        drop(installed);
        match polled {
            Poll::Ready(output) => Poll::Ready((output, this.phases.take().unwrap_or_default())),
            Poll::Pending => Poll::Pending,
        }
    }
}

/// How a call's record ends.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Outcome {
    /// Its response was sent.
    Complete,
    /// Sending its response failed; nothing was delivered.
    SendFailed,
    /// The transport closed before its response was sent.
    Incomplete,
    /// The handler settled, but no safe receive/send association is available.
    Uncorrelated,
    /// The send future was destroyed without a result.
    SendDropped,
}

impl Outcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::SendFailed => "send_failed",
            Self::Incomplete => "incomplete",
            Self::Uncorrelated => "uncorrelated",
            Self::SendDropped => "send_dropped",
        }
    }
}

/// A completed handler: only fixed-size tracing metadata is retained.
#[derive(Clone, Debug)]
struct HandlerRecord {
    ticket: call::Ticket,
    tool: &'static str,
    handler: Duration,
    phases: Phases,
    cancel_requested: Option<bool>,
    dispatch_send: Option<Duration>,
}

/// The counters every record line reports.
#[derive(Clone, Copy, Debug, Default)]
struct Counters {
    evicted: u64,
    unmatched_sends: u64,
    dropped_lines: u64,
    unassociated: u64,
    association_disabled: bool,
}

/// Where record lines go: stderr for the server.
type Sink = Box<dyn std::io::Write + Send>;

/// The most record lines queued for the writer; a line that does not fit is
/// dropped and counted, never waited for.
const WRITER_CAPACITY: usize = 1_024;

/// How long close waits for queued lines to be written.
const CLOSE_FLUSH: Duration = Duration::from_secs(1);

enum Message {
    Line(String),
    Flush(std::sync::mpsc::SyncSender<()>),
}

/// One thread writing record lines to a sink through a bounded queue. A line
/// the queue cannot take is dropped and counted, never waited for, so a sink
/// nobody drains costs lines, never a stalled caller.
pub(crate) struct LineWriter {
    sender: Option<std::sync::mpsc::SyncSender<Message>>,
    dropped: std::sync::atomic::AtomicU64,
}

impl LineWriter {
    /// Starts the writer thread `name`. When the thread cannot start, every
    /// line is dropped and counted.
    pub(crate) fn new(sink: Sink, capacity: usize, name: &str) -> Self {
        let (sender, queue) = std::sync::mpsc::sync_channel::<Message>(capacity);
        let spawned = std::thread::Builder::new()
            .name(name.into())
            .spawn(move || {
                let mut sink = sink;
                for message in queue {
                    match message {
                        Message::Line(mut line) => {
                            // Submit the record and LF together, avoiding a separate
                            // newline write racing process exit. Delivery stays best-effort.
                            line.push('\n');
                            let _ = sink.write_all(line.as_bytes());
                            let _ = sink.flush();
                        }
                        Message::Flush(written) => {
                            let _ = written.send(());
                        }
                    }
                }
            });
        Self {
            sender: spawned.is_ok().then_some(sender),
            dropped: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// Queues one line. A full queue, or no writer, drops the line and
    /// counts it; this never blocks.
    pub(crate) fn emit(&self, line: String) {
        let queued = self
            .sender
            .as_ref()
            .is_some_and(|sender| sender.try_send(Message::Line(line)).is_ok());
        if !queued {
            self.dropped
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Waits up to `timeout` for the lines queued so far to be written.
    pub(crate) fn flush(&self, timeout: Duration) {
        let Some(sender) = self.sender.as_ref() else {
            return;
        };
        let (written, done) = std::sync::mpsc::sync_channel(1);
        if sender.try_send(Message::Flush(written)).is_ok() {
            let _ = done.recv_timeout(timeout);
        }
    }

    /// Lines dropped so far.
    pub(crate) fn dropped(&self) -> u64 {
        self.dropped.load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// The server's trace: the handler records awaiting their sends, bounded,
/// the counters a record reports, and the queue to the thread that writes
/// records. Writing never happens on a request or send path, so a reader
/// that stops draining stderr costs dropped lines, never a stalled server.
pub struct PhaseTrace {
    pending: Mutex<call::Pending>,
    lines: LineWriter,
    started: Instant,
    instance: uuid::Uuid,
    diagnostics: Mutex<Option<Arc<crate::diagnostics::mcp::Observer>>>,
}

impl std::fmt::Debug for PhaseTrace {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PhaseTrace")
            .field("pending", &self.pending)
            .finish_non_exhaustive()
    }
}

impl PhaseTrace {
    fn new(sink: Sink, capacity: usize) -> Self {
        Self {
            pending: Mutex::new(call::Pending::default()),
            lines: LineWriter::new(sink, capacity, "engram-phase-trace"),
            started: Instant::now(),
            instance: uuid::Uuid::new_v4(),
            diagnostics: Mutex::new(None),
        }
    }

    /// The trace when `ENGRAM_MCP_PHASE_TRACE=1` at start, otherwise none.
    #[must_use]
    pub fn from_env() -> Option<Arc<Self>> {
        (std::env::var(PHASE_TRACE_ENV).as_deref() == Ok("1")).then(|| {
            let trace = Arc::new(Self::new(Box::new(std::io::stderr()), WRITER_CAPACITY));
            trace.emit(
                serde_json::json!({
                    "engram_mcp_trace_instance": 1,
                    "trace_instance": trace.instance,
                    "pid": std::process::id(),
                    "server_elapsed_ms": millis(trace.started.elapsed()),
                })
                .to_string(),
            );
            trace
        })
    }

    /// Queues a fixed, content-free stage record without waiting for its sink.
    /// Records after the existing close flush are best effort too; this adds
    /// no flush wait, and an observer must treat missing stages as unknown.
    pub fn shutdown_stage(&self, stage: ShutdownStage) {
        if let Ok(observer) = self.diagnostics.lock()
            && let Some(observer) = &*observer
        {
            observer.stage(stage);
        }
        self.emit(
            serde_json::json!({
                "engram_mcp_shutdown_trace": 1,
                "stage": stage.label(),
                "server_elapsed_ms": millis(self.started.elapsed()),
                "dropped_lines": self.lines.dropped(),
            })
            .to_string(),
        );
    }

    /// Connects the already-observed shutdown milestones to the durable sink.
    pub fn observe_diagnostics(&self, observer: Arc<crate::diagnostics::mcp::Observer>) {
        if let Ok(mut slot) = self.diagnostics.lock() {
            *slot = Some(observer);
        }
    }

    fn counters(&self, pending: &call::Pending) -> Counters {
        Counters {
            evicted: pending.evicted,
            unmatched_sends: pending.unmatched_sends,
            dropped_lines: self.lines.dropped(),
            unassociated: pending.unassociated,
            association_disabled: pending.disabled,
        }
    }

    fn emit(&self, line: String) {
        self.lines.emit(line);
    }

    fn flush(&self, timeout: Duration) {
        self.lines.flush(timeout);
    }

    fn received(&self, id: &NumberOrString, tool: &str) {
        call::received(self, id, tool);
    }

    #[cfg(test)]
    fn requested(&self, id: &NumberOrString) {
        self.received(id, "unknown");
    }

    fn client_cancelled(&self, id: &NumberOrString) {
        call::cancelled(self, id);
    }

    pub(crate) fn handler_started(
        &self,
        id: &NumberOrString,
        tool: &str,
        cancelled: impl FnOnce() -> bool,
    ) -> call::Ticket {
        call::started(self, id, tool, cancelled)
    }

    pub(crate) fn handler_settled(
        &self,
        ticket: call::Ticket,
        tool: &str,
        handler: Duration,
        phases: &Phases,
    ) {
        call::settled(self, ticket, tool, handler, phases);
    }

    fn dispatching(&self, id: &NumberOrString) -> call::Dispatched {
        call::dispatching(self, id)
    }

    fn closed(&self) {
        call::closed(self);
        self.shutdown_stage(ShutdownStage::TraceFlushStarted);
        self.flush(CLOSE_FLUSH);
        self.shutdown_stage(ShutdownStage::TraceFlushFinished);
    }

    #[cfg(test)]
    fn pending_len(&self) -> usize {
        self.pending.lock().map_or(0, |pending| pending.records())
    }

    #[cfg(test)]
    fn dropped(&self) -> u64 {
        self.lines.dropped()
    }
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

fn summary_value(summary: &Summary) -> serde_json::Value {
    serde_json::json!({
        "count": summary.count,
        "total_ms": millis(summary.total),
        "max_ms": millis(summary.max),
    })
}

/// One record line. A numeric request id correlates it; a string id is never
/// echoed, and its correlation is reported unavailable.
fn render(
    record: &HandlerRecord,
    outcome: Outcome,
    send: Option<Duration>,
    counters: Counters,
) -> String {
    let id = record.ticket.id;
    let correlation = if record.ticket.loss.is_none() {
        "numeric"
    } else {
        "unavailable"
    };
    let value = serde_json::json!({
        "engram_mcp_phase_trace": 1,
        "id": id,
        "correlation": correlation,
        "tool": record.tool,
        "state": outcome.as_str(),
        "cancel_requested": record.cancel_requested,
        "call_seq": record.ticket.loss.is_none().then_some(record.ticket.sequence).flatten(),
        "uncorrelated_reason": record.ticket.loss,
        "receive_ms": record.ticket.received.map(millis),
        "handler_start_ms": millis(record.ticket.started),
        "handler_total_ms": millis(record.handler),
        "store_open_total": summary_value(&record.phases.store_open),
        "store_mutex_wait": summary_value(&record.phases.store_mutex_wait),
        "begin_immediate": summary_value(&record.phases.begin_immediate),
        "commit": summary_value(&record.phases.commit),
        "receipt_serialize": summary_value(&record.phases.receipt_serialize),
        "read_snapshot": summary_value(&record.phases.read_snapshot),
        "sqlite_statement": summary_value(&record.phases.sqlite_statement),
        "backup_reminder": summary_value(&record.phases.backup_reminder),
        "wire_encode_send_inclusive_ms": send.map(millis),
        "dispatch_send_inclusive_ms": record.dispatch_send.map(millis),
        "evicted": counters.evicted,
        "unmatched_sends": counters.unmatched_sends,
        "dropped_lines": counters.dropped_lines,
        "unassociated": counters.unassociated,
        "association_disabled": counters.association_disabled,
    });
    let mut line = value.to_string();
    if line.len() > MAX_RECORD_BYTES {
        // Every field is fixed in size, so this cannot happen; should it,
        // the record is replaced rather than written past its bound.
        line = serde_json::json!({
            "engram_mcp_phase_trace": 1,
            "correlation": correlation,
            "state": "oversized",
        })
        .to_string();
    }
    line
}

/// A transport that passes every message through unchanged and times each
/// response's send, the encode, write and flush together.
pub struct TracingTransport<T> {
    inner: T,
    trace: Arc<PhaseTrace>,
}

impl<T> TracingTransport<T> {
    #[must_use]
    pub const fn new(inner: T, trace: Arc<PhaseTrace>) -> Self {
        Self { inner, trace }
    }
}

impl<T> Transport<RoleServer> for TracingTransport<T>
where
    T: Transport<RoleServer>,
{
    type Error = T::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleServer>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let id = match &item {
            JsonRpcMessage::Response(response) => Some(&response.id),
            JsonRpcMessage::Error(error) => error.id.as_ref(),
            JsonRpcMessage::Request(_) | JsonRpcMessage::Notification(_) => None,
        };
        // Ownership begins synchronously, before inner.send or the first poll.
        let mut sending =
            id.map(|id| call::Sending::new(Arc::clone(&self.trace), self.trace.dispatching(id)));
        let future = self.inner.send(item);
        async move {
            if let Some(sending) = &mut sending {
                sending.polled();
            }
            let result = future.await;
            if let Some(sending) = sending.take() {
                sending.finished(result.is_ok());
            }
            result
        }
    }

    /// Passes each message on unchanged, noting a request's id and a client's
    /// cancel before rmcp's serve loop handles either; nothing is awaited
    /// between the inner receive and the return.
    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleServer>>> + Send {
        let trace = Arc::clone(&self.trace);
        let receiving = self.inner.receive();
        async move {
            let message = receiving.await;
            match &message {
                None => trace.shutdown_stage(ShutdownStage::InputEnded),
                Some(JsonRpcMessage::Request(request)) => {
                    if let ClientRequest::CallToolRequest(tool) = &request.request {
                        trace.received(&request.id, &tool.params.name);
                    } else {
                        // A metadata response carries the same wire-ID type.
                        // Observe it too, or it could claim a tool's slot.
                        call::metadata_received(&trace, &request.id);
                    }
                }
                Some(JsonRpcMessage::Notification(notification)) => {
                    if let ClientNotification::CancelledNotification(cancelled) =
                        &notification.notification
                        && let Some(id) = &cancelled.params.request_id
                    {
                        trace.client_cancelled(id);
                    }
                }
                _ => {}
            }
            message
        }
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        let trace = Arc::clone(&self.trace);
        let closing = self.inner.close();
        async move {
            trace.shutdown_stage(ShutdownStage::TransportCloseStarted);
            let result = closing.await;
            trace.shutdown_stage(ShutdownStage::TransportCloseFinished);
            trace.closed();
            result
        }
    }
}

pub mod control;

#[cfg(test)]
mod tests;
