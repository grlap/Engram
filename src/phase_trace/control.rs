//! The opt-in phase trace of the host-control transport: where a control
//! process's startup and each input frame spent their time.
//!
//! With `ENGRAM_MCP_PHASE_TRACE=1` when `engram control` starts, the process
//! writes bounded JSON lines to stderr, each keyed `engram_control_phase_trace`
//! with its `pid` and `seq`. `seq` 0 is startup; `seq` N is the Nth input
//! frame, numbered when its first byte arrives, which its Nth response line
//! answers when one is written. A frame still running after a second gets
//! `in_flight` progress lines naming the phase in progress, so a process
//! killed at a host's deadline leaves its last observed phase behind; each
//! startup or frame then gets at most one terminal line. Lines go through a
//! bounded queue to their own writer thread, so a stderr nobody drains costs
//! dropped lines, never a stalled request. Unset, nothing is installed.
//!
//! A line names fixed labels, counts and durations only: never a request or
//! response body, an error message, SQL, a path or a token.

use std::cell::RefCell;
use std::sync::{Arc, Condvar, Mutex, OnceLock};
use std::time::{Duration, Instant};

use super::{LineWriter, PHASE_TRACE_ENV, Sink, Summary};

/// The bytes one line may take, its newline included.
pub const MAX_LINE_BYTES: usize = 4_096;

/// The most lines queued for the writer.
const WRITER_CAPACITY: usize = 1_024;

/// The most progress lines one startup or frame gets.
pub const MAX_PROGRESS: u32 = 10;

/// How long the exit waits for queued lines to be written.
const EXIT_FLUSH: Duration = Duration::from_millis(500);

/// The instant `main` began, taken only when the trace is opted into.
static PROCESS_START: OnceLock<Instant> = OnceLock::new();

/// The trace whose writer owns this process's stderr, once one is made from
/// the environment. Every later stderr line goes through its queue, so a
/// host that stops draining stderr can never block the process on a write.
static STDERR_OWNER: OnceLock<Arc<ControlTrace>> = OnceLock::new();

/// Writes `lines` to stderr after the trace lines already queued, when a
/// control trace owns stderr: they join its bounded queue, and the call waits
/// at most briefly for them to be written, so a stderr nobody drains costs
/// the lines, never a stalled exit. Returns false, writing nothing, when no
/// trace owns stderr.
pub fn emit_diagnostic_lines(lines: &[String]) -> bool {
    let Some(trace) = STDERR_OWNER.get() else {
        return false;
    };
    for line in lines {
        trace.lines.emit(line.clone());
    }
    trace.lines.flush(EXIT_FLUSH);
    true
}

/// Takes the process's start instant when the trace is opted into, so the
/// startup record measures from `main`; otherwise it takes nothing. Call it
/// first in `main`.
pub fn mark_process_start() {
    if enabled_by_env() {
        let _ = PROCESS_START.set(Instant::now());
    }
}

fn enabled_by_env() -> bool {
    std::env::var(PHASE_TRACE_ENV).as_deref() == Ok("1")
}

thread_local! {
    /// The control trace of the thread serving the control transport, if
    /// any; every hook on another thread returns at once.
    static ACTIVE: RefCell<Option<Arc<ControlTrace>>> = const { RefCell::new(None) };
}

/// Whether this thread serves a traced control transport.
pub(crate) fn active() -> bool {
    ACTIVE.with(|active| active.borrow().is_some())
}

fn with_active(action: impl FnOnce(&ControlTrace)) {
    ACTIVE.with(|active| {
        if let Some(trace) = active.borrow().as_ref() {
            action(trace);
        }
    });
}

/// A phase of a control process's startup or of one input frame.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ControlPhase {
    /// Startup from `main` until the control command is known.
    Starting,
    /// Resolving the project file and its store.
    ProjectResolve,
    /// Probing the project root's filesystem identity.
    HostPathProbe,
    ConnectionOpen,
    SchemaCheck,
    HostPathPolicy,
    PolicyHistoryVerify,
    SchemaInit,
    ConnectionResume,
    /// The frame from its first byte until its response is ready: reading
    /// the rest of it, parsing and handling it.
    Handler,
    ResponseSerialize,
    ResponseWriteFlush,
}

const PHASES: usize = 12;

impl ControlPhase {
    const fn index(self) -> usize {
        self as usize
    }

    /// The label a line carries.
    #[must_use]
    pub const fn word(self) -> &'static str {
        match self {
            Self::Starting => "starting",
            Self::ProjectResolve => "project_resolve",
            Self::HostPathProbe => "host_path_probe",
            Self::ConnectionOpen => "connection_open",
            Self::SchemaCheck => "schema_check",
            Self::HostPathPolicy => "host_path_policy",
            Self::PolicyHistoryVerify => "policy_history_verify",
            Self::SchemaInit => "schema_init",
            Self::ConnectionResume => "connection_resume",
            Self::Handler => "handler_total",
            Self::ResponseSerialize => "response_serialize",
            Self::ResponseWriteFlush => "response_write_flush",
        }
    }

    const ALL: [Self; PHASES] = [
        Self::Starting,
        Self::ProjectResolve,
        Self::HostPathProbe,
        Self::ConnectionOpen,
        Self::SchemaCheck,
        Self::HostPathPolicy,
        Self::PolicyHistoryVerify,
        Self::SchemaInit,
        Self::ConnectionResume,
        Self::Handler,
        Self::ResponseSerialize,
        Self::ResponseWriteFlush,
    ];
}

/// A SQL statement whose running time a line names: its call can include a
/// wait for the write lock, I/O and other work, never only one of them.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Statement {
    BeginImmediate,
    Commit,
}

impl Statement {
    const fn word(self) -> &'static str {
        match self {
            Self::BeginImmediate => "begin_immediate",
            Self::Commit => "commit",
        }
    }

    fn classify(sql: &str) -> Option<Self> {
        let mut words = sql.split_ascii_whitespace();
        let first = words.next()?.trim_end_matches(';');
        if first.eq_ignore_ascii_case("COMMIT") {
            return Some(Self::Commit);
        }
        let second = words.next()?.trim_end_matches(';');
        (first.eq_ignore_ascii_case("BEGIN") && second.eq_ignore_ascii_case("IMMEDIATE"))
            .then_some(Self::BeginImmediate)
    }
}

/// How a startup or a frame ended.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum Terminal {
    /// Startup reached ready, or the response's newline was written and
    /// flushed; a protocol error is still complete, with its outcome.
    Complete,
    /// Writing or flushing the response failed.
    WriteFailed,
    /// Processing stopped before it finished, where the process could say
    /// so.
    Incomplete,
}

impl Terminal {
    const fn word(self) -> &'static str {
        match self {
            Self::Complete => "complete",
            Self::WriteFailed => "write_failed",
            Self::Incomplete => "incomplete",
        }
    }
}

/// When progress lines fall due.
#[derive(Clone, Copy, Debug)]
pub struct ProgressTiming {
    pub first: Duration,
    pub every: Duration,
}

impl ProgressTiming {
    /// After a second, then every two seconds.
    pub const DEFAULT: Self = Self {
        first: Duration::from_secs(1),
        every: Duration::from_secs(2),
    };
}

/// The startup or frame in flight.
struct Slot {
    seq: u64,
    startup: bool,
    started: Instant,
    phase: ControlPhase,
    phase_started: Instant,
    statement: Option<(Statement, Instant)>,
    completed: [Option<Duration>; PHASES],
    begin_immediate: Summary,
    commit: Summary,
    operation: Option<&'static str>,
    ordinal: u32,
    progress: u32,
}

impl Slot {
    fn new(seq: u64, startup: bool, started: Instant, phase: ControlPhase) -> Self {
        Self {
            seq,
            startup,
            started,
            phase,
            phase_started: started,
            statement: None,
            completed: [None; PHASES],
            begin_immediate: Summary::default(),
            commit: Summary::default(),
            operation: None,
            ordinal: 0,
            progress: 0,
        }
    }

    /// Closes the phase in progress, adding its time, and opens `phase`.
    fn enter(&mut self, phase: ControlPhase) {
        let now = Instant::now();
        let spent = now.saturating_duration_since(self.phase_started);
        let slot = &mut self.completed[self.phase.index()];
        *slot = Some(slot.unwrap_or_default().saturating_add(spent));
        self.phase = phase;
        self.phase_started = now;
    }
}

struct State {
    slot: Option<Slot>,
    next_seq: u64,
    stopped: bool,
}

/// One control process's trace: the startup or frame in flight, the monitor
/// that writes its progress, and the queue to the writer thread.
pub struct ControlTrace {
    lines: LineWriter,
    pid: u32,
    timing: ProgressTiming,
    state: Mutex<State>,
    changed: Condvar,
}

impl std::fmt::Debug for ControlTrace {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("ControlTrace")
            .field("pid", &self.pid)
            .finish_non_exhaustive()
    }
}

impl ControlTrace {
    /// The trace when `ENGRAM_MCP_PHASE_TRACE=1` at start, writing to stderr,
    /// otherwise none.
    #[must_use]
    pub fn from_env() -> Option<Arc<Self>> {
        enabled_by_env().then(|| {
            let trace = Self::with_sink(
                Box::new(std::io::stderr()),
                WRITER_CAPACITY,
                ProgressTiming::DEFAULT,
            );
            let _ = STDERR_OWNER.set(Arc::clone(&trace));
            trace
        })
    }

    /// A trace writing to `sink` with `timing`, its monitor running.
    pub(crate) fn with_sink(sink: Sink, capacity: usize, timing: ProgressTiming) -> Arc<Self> {
        let trace = Arc::new(Self {
            lines: LineWriter::new(sink, capacity, "engram-control-trace"),
            pid: std::process::id(),
            timing,
            state: Mutex::new(State {
                slot: None,
                next_seq: 1,
                stopped: false,
            }),
            changed: Condvar::new(),
        });
        // Without a monitor there are no progress lines; terminal lines
        // still come.
        let monitor = Arc::downgrade(&trace);
        let _ = std::thread::Builder::new()
            .name("engram-control-trace-monitor".into())
            .spawn(move || monitor_loop(&monitor));
        trace
    }

    /// Makes this the calling thread's control trace and opens the startup
    /// record, measured from `main` when its instant was taken.
    pub fn install(self: &Arc<Self>) {
        self.install_from(PROCESS_START.get().copied().unwrap_or_else(Instant::now));
    }

    /// [`Self::install`], with startup measured from `started`.
    pub(crate) fn install_from(self: &Arc<Self>, started: Instant) {
        ACTIVE.with(|active| *active.borrow_mut() = Some(Arc::clone(self)));
        self.open(Slot::new(0, true, started, ControlPhase::Starting));
    }

    fn open(&self, slot: Slot) {
        if let Ok(mut state) = self.state.lock() {
            state.slot = Some(slot);
        }
        self.changed.notify_all();
    }

    /// Ends startup: ready, or failed with a fixed `outcome` code.
    pub fn startup_finished(&self, outcome: Option<&'static str>) {
        let state = if outcome.is_none() {
            Terminal::Complete
        } else {
            Terminal::Incomplete
        };
        self.finish(state, Some(outcome.unwrap_or("ok")));
    }

    /// Ends startup as incomplete with `outcome` when it is still open, as
    /// every early return of a failed startup must; otherwise does nothing.
    pub fn abandon_startup(&self, outcome: &'static str) {
        let open = self
            .state
            .lock()
            .is_ok_and(|state| state.slot.as_ref().is_some_and(|slot| slot.startup));
        if open {
            self.finish(Terminal::Incomplete, Some(outcome));
        }
    }

    /// Opens the next frame, numbered when its first byte has arrived.
    pub fn begin_frame(&self) -> u64 {
        let seq = match self.state.lock() {
            Ok(mut state) => {
                let seq = state.next_seq;
                state.next_seq += 1;
                state.slot = Some(Slot::new(seq, false, Instant::now(), ControlPhase::Handler));
                seq
            }
            Err(_) => 0,
        };
        self.changed.notify_all();
        seq
    }

    /// Moves the startup or frame in flight into `phase`.
    pub fn enter(&self, phase: ControlPhase) {
        if let Ok(mut state) = self.state.lock()
            && let Some(slot) = state.slot.as_mut()
        {
            slot.enter(phase);
        }
    }

    /// Names the frame's operation: an allowlisted protocol label.
    pub fn set_operation(&self, operation: &'static str) {
        if let Ok(mut state) = self.state.lock()
            && let Some(slot) = state.slot.as_mut()
        {
            slot.operation = Some(operation);
        }
    }

    /// Writes the terminal line of the startup or frame in flight and
    /// closes it. The line is queued under the same lock the monitor
    /// queues progress under, so no stale progress line follows it.
    pub fn finish(&self, terminal: Terminal, outcome: Option<&'static str>) {
        if let Ok(mut state) = self.state.lock()
            && let Some(mut slot) = state.slot.take()
        {
            let phase = slot.phase;
            slot.enter(phase);
            slot.ordinal += 1;
            self.lines
                .emit(self.render_terminal(&slot, terminal, outcome));
        }
        self.changed.notify_all();
    }

    /// Stops the monitor and waits briefly for queued lines; it never waits
    /// for a stalled sink longer than that.
    pub fn close(&self) {
        if let Ok(mut state) = self.state.lock() {
            state.stopped = true;
        }
        self.changed.notify_all();
        ACTIVE.with(|active| {
            let mut active = active.borrow_mut();
            if active
                .as_ref()
                .is_some_and(|trace| std::ptr::eq(Arc::as_ptr(trace), self))
            {
                *active = None;
            }
        });
        self.lines.flush(EXIT_FLUSH);
    }

    fn statement_started(&self, statement: Statement) {
        if let Ok(mut state) = self.state.lock()
            && let Some(slot) = state.slot.as_mut()
        {
            slot.statement = Some((statement, Instant::now()));
        }
    }

    fn statement_finished(&self, statement: Statement, elapsed: Duration) {
        if let Ok(mut state) = self.state.lock()
            && let Some(slot) = state.slot.as_mut()
        {
            match statement {
                Statement::BeginImmediate => slot.begin_immediate.add(elapsed),
                Statement::Commit => slot.commit.add(elapsed),
            }
            slot.statement = None;
        }
    }

    /// The progress line due now for the slot, if one is due, counted.
    fn due_progress(&self, slot: &mut Slot, now: Instant) -> Option<String> {
        if slot.progress >= MAX_PROGRESS {
            return None;
        }
        let due = self.timing.first + self.timing.every * slot.progress;
        if now.saturating_duration_since(slot.started) < due {
            return None;
        }
        slot.progress += 1;
        slot.ordinal += 1;
        Some(self.render_progress(slot, now))
    }

    /// How long the monitor may sleep before the slot's next progress line.
    fn until_due(&self, slot: Option<&Slot>, now: Instant) -> Duration {
        slot.filter(|slot| slot.progress < MAX_PROGRESS).map_or(
            Duration::from_secs(3_600),
            |slot| {
                let due = self.timing.first + self.timing.every * slot.progress;
                due.saturating_sub(now.saturating_duration_since(slot.started))
            },
        )
    }

    fn base(&self, slot: &Slot, state: &str) -> serde_json::Map<String, serde_json::Value> {
        let mut line = serde_json::Map::new();
        line.insert("engram_control_phase_trace".into(), 1.into());
        line.insert("pid".into(), self.pid.into());
        line.insert("seq".into(), slot.seq.into());
        line.insert("ordinal".into(), slot.ordinal.into());
        line.insert(
            "kind".into(),
            if slot.startup { "startup" } else { "frame" }.into(),
        );
        line.insert("state".into(), state.into());
        if let Some(operation) = slot.operation {
            line.insert("operation".into(), operation.into());
        }
        line
    }

    fn render_progress(&self, slot: &Slot, now: Instant) -> String {
        let mut line = self.base(slot, "in_flight");
        line.insert("phase".into(), slot.phase.word().into());
        line.insert(
            "elapsed_ms".into(),
            millis(now.saturating_duration_since(slot.started)).into(),
        );
        line.insert(
            "phase_elapsed_ms".into(),
            millis(now.saturating_duration_since(slot.phase_started)).into(),
        );
        if let Some((statement, started)) = slot.statement {
            line.insert("statement".into(), statement.word().into());
            line.insert(
                "statement_elapsed_ms".into(),
                millis(now.saturating_duration_since(started)).into(),
            );
        }
        line.insert("dropped_lines".into(), self.lines.dropped().into());
        bounded(&line)
    }

    fn render_terminal(
        &self,
        slot: &Slot,
        terminal: Terminal,
        outcome: Option<&'static str>,
    ) -> String {
        let mut line = self.base(slot, terminal.word());
        if let Some(outcome) = outcome {
            line.insert("outcome".into(), outcome.into());
        }
        line.insert(
            "total_ms".into(),
            millis(Instant::now().saturating_duration_since(slot.started)).into(),
        );
        // Phases reached, each with its time; a phase not reached is left
        // out, never shown as zero. Phases nest, so they are not summed.
        let mut phases = serde_json::Map::new();
        for phase in ControlPhase::ALL {
            if let Some(spent) = slot.completed[phase.index()] {
                phases.insert(format!("{}_ms", phase.word()), millis(spent).into());
            }
        }
        line.insert("phases".into(), phases.into());
        for (statement, summary) in [
            (Statement::BeginImmediate, &slot.begin_immediate),
            (Statement::Commit, &slot.commit),
        ] {
            if summary.count > 0 {
                line.insert(
                    statement.word().into(),
                    serde_json::json!({
                        "count": summary.count,
                        "total_ms": millis(summary.total),
                        "max_ms": millis(summary.max),
                    }),
                );
            }
        }
        line.insert("dropped_lines".into(), self.lines.dropped().into());
        bounded(&line)
    }
}

fn millis(duration: Duration) -> f64 {
    duration.as_secs_f64() * 1_000.0
}

/// One line within the bound, its newline included. Every field is fixed in
/// size, so the fallback never fires in practice.
fn bounded(line: &serde_json::Map<String, serde_json::Value>) -> String {
    let text = serde_json::Value::Object(line.clone()).to_string();
    if text.len() < MAX_LINE_BYTES {
        return text;
    }
    let mut short = serde_json::Map::new();
    for key in [
        "engram_control_phase_trace",
        "pid",
        "seq",
        "ordinal",
        "kind",
        "state",
    ] {
        if let Some(value) = line.get(key) {
            short.insert(key.into(), value.clone());
        }
    }
    short.insert("oversized".into(), true.into());
    serde_json::Value::Object(short).to_string()
}

/// Writes each progress line when it falls due, under the state lock, until
/// [`ControlTrace::close`] stops it; the monitor holds the trace while it
/// waits, so a trace that is never closed keeps its monitor.
fn monitor_loop(trace: &std::sync::Weak<ControlTrace>) {
    loop {
        let Some(trace) = trace.upgrade() else {
            return;
        };
        let Ok(state) = trace.state.lock() else {
            return;
        };
        if state.stopped {
            return;
        }
        let wait = trace.until_due(state.slot.as_ref(), Instant::now());
        let Ok((mut state, _)) = trace.changed.wait_timeout(state, wait) else {
            return;
        };
        if state.stopped {
            return;
        }
        let now = Instant::now();
        if let Some(slot) = state.slot.as_mut()
            && let Some(line) = trace.due_progress(slot, now)
        {
            trace.lines.emit(line);
        }
        drop(state);
        drop(trace);
    }
}

/// The SQLite trace callback of a traced control connection: it notes when
/// `BEGIN IMMEDIATE` or `COMMIT` starts and how long it ran, and records
/// nothing of any statement's text.
pub(crate) fn sql_event(event: rusqlite::trace::TraceEvent<'_>) {
    match event {
        rusqlite::trace::TraceEvent::Stmt(_, sql) => {
            if let Some(statement) = Statement::classify(sql) {
                with_active(|trace| trace.statement_started(statement));
            }
        }
        rusqlite::trace::TraceEvent::Profile(statement, elapsed) => {
            if let Some(kind) = Statement::classify(&statement.sql()) {
                with_active(|trace| trace.statement_finished(kind, elapsed));
            }
        }
        _ => {}
    }
}

/// Moves this thread's traced startup into `phase`; on another thread it
/// does nothing.
pub(crate) fn enter(phase: ControlPhase) {
    with_active(|trace| trace.enter(phase));
}

#[cfg(test)]
mod tests;
