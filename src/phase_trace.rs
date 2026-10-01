//! An opt-in phase trace for the MCP server: where each tool call's time went.
//!
//! It is enabled once, at `engram mcp` start, by `ENGRAM_MCP_PHASE_TRACE=1`.
//! Each tool call then runs with a per-call accumulator that the store hooks
//! fill, and after its response is sent the server writes one bounded JSON
//! line to stderr, correlated by the call's numeric request id. Unset, no
//! accumulator is ever installed, so every hook returns at once without
//! taking a timestamp, no profile callback is registered, the transport is
//! not wrapped, and nothing is written.
//!
//! The record names phases, counts and durations only: never SQL, a path,
//! parameters or a request or response body. Durations are elapsed wall time
//! and overlap where a phase contains another; they are not summed. Tracing
//! never changes an operation's result: a failure to write a record is
//! dropped.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::future::Future;
use std::io::Write as _;
use std::pin::Pin;
use std::sync::{Arc, Mutex};
use std::task::{Context, Poll};
use std::time::{Duration, Instant};

use rmcp::model::{ClientNotification, JsonRpcMessage, NumberOrString};
use rmcp::service::{RoleServer, RxJsonRpcMessage, TxJsonRpcMessage};
use rmcp::transport::Transport;

/// The environment variable that enables the trace at server start.
pub const PHASE_TRACE_ENV: &str = "ENGRAM_MCP_PHASE_TRACE";

/// The most handler records kept while their responses are being sent.
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
}

impl Phases {
    fn summary(&mut self, phase: Phase) -> &mut Summary {
        match phase {
            Phase::StoreOpen => &mut self.store_open,
            Phase::StoreMutexWait => &mut self.store_mutex_wait,
            Phase::BeginImmediate => &mut self.begin_immediate,
            Phase::Commit => &mut self.commit,
            Phase::ReceiptSerialize => &mut self.receipt_serialize,
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
    let rusqlite::trace::TraceEvent::Profile(statement, elapsed) = event else {
        return;
    };
    if !active() {
        return;
    }
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
    /// The client cancelled it before its response was handed over, and by
    /// close no response had been sent.
    Cancelled,
    /// The transport closed before its response was sent.
    Incomplete,
    /// Too many records waited for their sends; this, the oldest, was let go.
    Evicted,
}

impl Outcome {
    const fn as_str(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::SendFailed => "send_failed",
            Self::Cancelled => "cancelled",
            Self::Incomplete => "incomplete",
            Self::Evicted => "evicted",
        }
    }
}

/// One tool call's handler record, waiting for its response to be sent.
///
/// rmcp's serve loop handles a client's `notifications/cancelled` and hands
/// a response to the transport on one task, one event at a time. The trace
/// sees the cancel as the transport yields it, before the loop handles it,
/// and sees a response handed over (`dispatched`) in the same synchronous
/// step in which the loop hands it over. So a cancel seen while a call's
/// response is not yet dispatched (`cancel_requested`) is one the serve loop
/// honours by dropping that response, and a cancel seen after dispatch comes
/// too late to stop it. After input ends, though, rmcp drains the responses
/// still queued straight to the transport, a cancelled call's included. So a
/// cancel only marks the record: a later send completes it as usual, and
/// close writes a record still unsent as cancelled.
#[derive(Clone, Debug)]
struct HandlerRecord {
    id: NumberOrString,
    tool: &'static str,
    handler: Duration,
    phases: Phases,
    dispatched: bool,
    cancel_requested: bool,
}

#[derive(Debug, Default)]
struct Pending {
    records: VecDeque<HandlerRecord>,
    /// Calls their clients cancelled before they settled, newest last and
    /// bounded; a call settling with its id here is marked cancelled.
    cancels: VecDeque<NumberOrString>,
    evicted: u64,
    unmatched_sends: u64,
    /// Set once the transport closed: a record settling later is written at
    /// once, since no send or close will come for it.
    closed: bool,
}

/// The counters every record line reports.
#[derive(Clone, Copy, Debug, Default)]
struct Counters {
    evicted: u64,
    unmatched_sends: u64,
    dropped_lines: u64,
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

/// The server's trace: the handler records awaiting their sends, bounded,
/// the counters a record reports, and the queue to the thread that writes
/// records. Writing never happens on a request or send path, so a reader
/// that stops draining stderr costs dropped lines, never a stalled server.
pub struct PhaseTrace {
    pending: Mutex<Pending>,
    writer: Option<std::sync::mpsc::SyncSender<Message>>,
    dropped_lines: std::sync::atomic::AtomicU64,
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
        let (writer, queue) = std::sync::mpsc::sync_channel::<Message>(capacity);
        let spawned = std::thread::Builder::new()
            .name("engram-phase-trace".into())
            .spawn(move || {
                let mut sink = sink;
                for message in queue {
                    match message {
                        Message::Line(line) => {
                            let _ = writeln!(sink, "{line}");
                            let _ = sink.flush();
                        }
                        Message::Flush(written) => {
                            let _ = written.send(());
                        }
                    }
                }
            });
        Self {
            pending: Mutex::new(Pending::default()),
            writer: spawned.is_ok().then_some(writer),
            dropped_lines: std::sync::atomic::AtomicU64::new(0),
        }
    }

    /// The trace when `ENGRAM_MCP_PHASE_TRACE=1` at start, otherwise none.
    #[must_use]
    pub fn from_env() -> Option<Arc<Self>> {
        (std::env::var(PHASE_TRACE_ENV).as_deref() == Ok("1"))
            .then(|| Arc::new(Self::new(Box::new(std::io::stderr()), WRITER_CAPACITY)))
    }

    fn counters(&self, pending: &Pending) -> Counters {
        Counters {
            evicted: pending.evicted,
            unmatched_sends: pending.unmatched_sends,
            dropped_lines: self
                .dropped_lines
                .load(std::sync::atomic::Ordering::Relaxed),
        }
    }

    /// Queues one record line for the writer. A full queue, or no writer,
    /// drops the line and counts it; this never blocks.
    fn emit(&self, line: String) {
        let queued = self
            .writer
            .as_ref()
            .is_some_and(|writer| writer.try_send(Message::Line(line)).is_ok());
        if !queued {
            self.dropped_lines
                .fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        }
    }

    /// Waits up to `timeout` for the lines queued so far to be written.
    fn flush(&self, timeout: Duration) {
        let Some(writer) = self.writer.as_ref() else {
            return;
        };
        let (written, done) = std::sync::mpsc::sync_channel(1);
        if writer.try_send(Message::Flush(written)).is_ok() {
            let _ = done.recv_timeout(timeout);
        }
    }

    /// A request arrived: an earlier cancel naming its id was for an older
    /// call, so it no longer applies.
    fn requested(&self, id: &NumberOrString) {
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        pending.cancels.retain(|cancelled| cancelled != id);
    }

    /// The client cancelled the call `id`. When a settled call's response
    /// was not yet handed to the transport, its record is marked. Otherwise
    /// the id is remembered, so a call with it that is still running is
    /// marked when it settles. A cancel arriving after its response was
    /// dispatched, or naming a request that was no tool call, never reaches
    /// a record: the next request with that id clears it.
    fn client_cancelled(&self, id: &NumberOrString) {
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        if let Some(record) = pending
            .records
            .iter_mut()
            .rev()
            .find(|record| record.id == *id && !record.dispatched)
        {
            record.cancel_requested = true;
            return;
        }
        if pending.cancels.len() >= MAX_PENDING {
            pending.cancels.pop_front();
        }
        pending.cancels.push_back(id.clone());
    }

    /// Takes a settled call's record, which waits for its response's send. A
    /// call settling after close gets no send, so it is written at once: as
    /// cancelled when its client cancelled it, otherwise as incomplete. When
    /// the bound is reached the oldest waiting record is written as evicted.
    pub(crate) fn handler_settled(
        &self,
        id: NumberOrString,
        tool: &str,
        handler: Duration,
        phases: Phases,
    ) {
        let tool = TOOL_LABELS
            .iter()
            .copied()
            .find(|label| *label == tool)
            .unwrap_or("unknown");
        let mut record = HandlerRecord {
            id,
            tool,
            handler,
            phases,
            dispatched: false,
            cancel_requested: false,
        };
        let lines: Vec<String> = {
            let Ok(mut pending) = self.pending.lock() else {
                return;
            };
            let mut lines = Vec::new();
            record.cancel_requested = pending
                .cancels
                .iter()
                .position(|cancelled| *cancelled == record.id)
                .and_then(|index| pending.cancels.remove(index))
                .is_some();
            if pending.closed {
                lines.push(render(
                    &record,
                    unsent(&record),
                    None,
                    self.counters(&pending),
                ));
            } else {
                if pending.records.len() >= MAX_PENDING
                    && let Some(oldest) = pending.records.pop_front()
                {
                    pending.evicted = pending.evicted.saturating_add(1);
                    lines.push(render(
                        &oldest,
                        Outcome::Evicted,
                        None,
                        self.counters(&pending),
                    ));
                }
                pending.records.push_back(record);
            }
            lines
        };
        for line in lines {
            self.emit(line);
        }
    }

    /// Marks the newest waiting record of the call `id` answers as handed to
    /// the transport. It runs in the same synchronous step in which rmcp
    /// hands the response over, before the send is awaited.
    pub(crate) fn dispatching(&self, id: &NumberOrString) {
        let Ok(mut pending) = self.pending.lock() else {
            return;
        };
        if let Some(record) = pending
            .records
            .iter_mut()
            .rev()
            .find(|record| record.id == *id && !record.dispatched)
        {
            record.dispatched = true;
        }
    }

    /// Completes the record of the call `id` answered, by the newest record
    /// with that id, and writes it: sent, or the send failed. A send matching
    /// no record, such as initialize's or tools/list's, is counted.
    pub(crate) fn sent(&self, id: &NumberOrString, send: Duration, delivered: bool) {
        let line = {
            let Ok(mut pending) = self.pending.lock() else {
                return;
            };
            let index = pending
                .records
                .iter()
                .rposition(|record| record.id == *id && record.dispatched)
                .or_else(|| pending.records.iter().rposition(|record| record.id == *id));
            if let Some(record) = index.and_then(|index| pending.records.remove(index)) {
                let outcome = if delivered {
                    Outcome::Complete
                } else {
                    Outcome::SendFailed
                };
                Some(render(
                    &record,
                    outcome,
                    Some(send),
                    self.counters(&pending),
                ))
            } else {
                pending.unmatched_sends = pending.unmatched_sends.saturating_add(1);
                None
            }
        };
        if let Some(line) = line {
            self.emit(line);
        }
    }

    /// Writes every record still waiting: as cancelled when its client
    /// cancelled it before its response was handed over, otherwise as
    /// incomplete. rmcp closes the transport only after its shutdown drain,
    /// so no send can follow. It marks the trace closed so a later settle is
    /// written at once, and waits briefly for the queued lines to reach the
    /// sink. Lines queued at or after close are best effort: the process may
    /// exit before the writer reaches them.
    pub(crate) fn closed(&self) {
        let lines: Vec<String> = {
            let Ok(mut pending) = self.pending.lock() else {
                return;
            };
            pending.closed = true;
            let records: Vec<HandlerRecord> = pending.records.drain(..).collect();
            let counters = self.counters(&pending);
            records
                .iter()
                .map(|record| render(record, unsent(record), None, counters))
                .collect()
        };
        for line in lines {
            self.emit(line);
        }
        self.flush(CLOSE_FLUSH);
    }

    #[cfg(test)]
    fn pending_len(&self) -> usize {
        self.pending
            .lock()
            .map_or(0, |pending| pending.records.len())
    }

    #[cfg(test)]
    fn dropped(&self) -> u64 {
        self.dropped_lines
            .load(std::sync::atomic::Ordering::Relaxed)
    }
}

/// How a record whose response was never sent ends: cancelled when its
/// client cancelled it before the response was handed over, otherwise
/// incomplete.
const fn unsent(record: &HandlerRecord) -> Outcome {
    if record.cancel_requested && !record.dispatched {
        Outcome::Cancelled
    } else {
        Outcome::Incomplete
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
    let (id, correlation) = match &record.id {
        NumberOrString::Number(number) => (serde_json::json!(number), "numeric"),
        NumberOrString::String(_) => (serde_json::Value::Null, "unavailable"),
    };
    let value = serde_json::json!({
        "engram_mcp_phase_trace": 1,
        "id": id,
        "correlation": correlation,
        "tool": record.tool,
        "state": outcome.as_str(),
        "cancel_requested": record.cancel_requested,
        "handler_total_ms": millis(record.handler),
        "store_open_total": summary_value(&record.phases.store_open),
        "store_mutex_wait": summary_value(&record.phases.store_mutex_wait),
        "begin_immediate": summary_value(&record.phases.begin_immediate),
        "commit": summary_value(&record.phases.commit),
        "receipt_serialize": summary_value(&record.phases.receipt_serialize),
        "wire_encode_send_inclusive_ms": send.map(millis),
        "evicted": counters.evicted,
        "unmatched_sends": counters.unmatched_sends,
        "dropped_lines": counters.dropped_lines,
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
            JsonRpcMessage::Response(response) => Some(response.id.clone()),
            JsonRpcMessage::Error(error) => error.id.clone(),
            JsonRpcMessage::Request(_) | JsonRpcMessage::Notification(_) => None,
        };
        let trace = Arc::clone(&self.trace);
        if let Some(id) = &id {
            trace.dispatching(id);
        }
        let sending = self.inner.send(item);
        async move {
            let started = Instant::now();
            let result = sending.await;
            if let Some(id) = id {
                trace.sent(&id, started.elapsed(), result.is_ok());
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
                Some(JsonRpcMessage::Request(request)) => trace.requested(&request.id),
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
            let result = closing.await;
            trace.closed();
            result
        }
    }
}

#[cfg(test)]
mod tests;
