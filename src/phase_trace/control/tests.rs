use super::*;
use crate::HostControlServer;
use crate::domain::{ProjectId, SessionId};
use std::io::{Cursor, Read, Write};

/// A sink tests read back: every line written.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl Write for Captured {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        self.0.lock().expect("captured").extend_from_slice(bytes);
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

impl Captured {
    fn lines(&self) -> Vec<serde_json::Value> {
        let bytes = self.0.lock().expect("captured").clone();
        String::from_utf8(bytes)
            .expect("UTF-8")
            .lines()
            .map(|line| {
                assert!(line.len() < MAX_LINE_BYTES, "{line}");
                serde_json::from_str(line).expect("one JSON line")
            })
            .collect()
    }

    fn text(&self) -> String {
        String::from_utf8(self.0.lock().expect("captured").clone()).expect("UTF-8")
    }
}

/// Progress soon enough for a test. A descheduled frame can reach it in any
/// phase, so no test may take a line to mean a stall in one particular phase.
const FAST: ProgressTiming = ProgressTiming {
    first: Duration::from_millis(150),
    every: Duration::from_millis(50),
};

fn traced(timing: ProgressTiming) -> (Arc<ControlTrace>, Captured) {
    let captured = Captured::default();
    (
        ControlTrace::with_sink(Box::new(captured.clone()), 1_024, timing),
        captured,
    )
}

// This test drives the ordinary progress scheduler only after the actual
// statement starts, so slow startup cannot spend its ten progress records.
fn traced_without_monitor() -> (Arc<ControlTrace>, Captured) {
    let captured = Captured::default();
    let trace = Arc::new(ControlTrace {
        lines: LineWriter::new(Box::new(captured.clone()), 1_024, "control-test-trace"),
        pid: std::process::id(),
        timing: FAST,
        state: Mutex::new(State {
            slot: None,
            next_seq: 1,
            stopped: false,
        }),
        changed: Condvar::new(),
        encoded: EncodeCount::default(),
    });
    (trace, captured)
}

fn open_server(database: &std::path::Path) -> HostControlServer {
    HostControlServer::open(
        database,
        ProjectId("control-trace".into()),
        "agent".into(),
        SessionId("control-trace-session".into()),
        None,
    )
    .expect("control server")
}

#[test]
fn sqlite_metrics_and_control_phase_profile_share_one_callback() {
    let (trace, captured) = traced(ProgressTiming::DEFAULT);
    trace.install();
    let directory = crate::test_support::temp_home().unwrap();
    let scope = crate::storage::work_cost::sqlite::start();
    let _store =
        crate::SqliteStore::open_for_control_host(directory.path().join("shared.db"), None)
            .unwrap();
    let metrics = scope.finish();
    assert!(metrics.total.statements > 0);
    trace.startup_finished(None);
    let startup = terminal(&written(&trace, &captured), 0);
    assert!(
        startup["begin_immediate"]["count"].as_u64() >= Some(1),
        "{startup}"
    );
    assert!(startup["commit"]["count"].as_u64() >= Some(1), "{startup}");
}

#[test]
fn sqlite_metrics_do_not_enable_control_phases_on_a_raw_connection() {
    let (trace, captured) = traced(ProgressTiming::DEFAULT);
    trace.install();
    let connection = crate::storage::open_sqlite_file(
        std::path::Path::new(":memory:"),
        rusqlite::OpenFlags::default(),
    )
    .unwrap();
    let scope = crate::storage::work_cost::sqlite::start();
    connection
        .execute_batch("BEGIN IMMEDIATE; COMMIT;")
        .unwrap();
    let metrics = scope.finish();
    assert_eq!(metrics.total.statements, 2);
    trace.startup_finished(None);
    let startup = terminal(&written(&trace, &captured), 0);
    assert!(startup.get("begin_immediate").is_none(), "{startup}");
    assert!(startup.get("commit").is_none(), "{startup}");
}

/// The lines written so far, after `close`, once the writer has caught up:
/// close waits only briefly, so a loaded host is given longer here.
fn written(trace: &ControlTrace, captured: &Captured) -> Vec<serde_json::Value> {
    trace.close();
    crate::phase_trace::tests::assert_flushed(&trace.lines);
    captured.lines()
}

fn of_seq(lines: &[serde_json::Value], seq: u64) -> Vec<serde_json::Value> {
    lines
        .iter()
        .filter(|line| line["seq"] == seq)
        .cloned()
        .collect()
}

fn terminal(lines: &[serde_json::Value], seq: u64) -> serde_json::Value {
    let mut terminals = of_seq(lines, seq)
        .into_iter()
        .filter(|line| line["state"] != "in_flight")
        .collect::<Vec<_>>();
    assert_eq!(
        terminals.len(),
        1,
        "one terminal line for seq {seq}: {lines:?}"
    );
    terminals.remove(0)
}

// Frames are numbered by their first byte in input order, whatever they
// hold: a blank line, malformed JSON, an unknown operation, an oversize
// frame drained as one, and an unterminated last frame each take one, and
// a bare end of input takes none. The responses are those of the untraced
// server, byte for byte, and startup is traced from connection open through
// connection resume.
#[test]
fn frames_are_numbered_by_first_byte_and_responses_are_unchanged() {
    let home = crate::test_support::temp_home().expect("home");
    let database = home.path().join("control.sqlite3");
    let (trace, captured) = traced(ProgressTiming::DEFAULT);
    trace.install();
    let mut server = open_server(&database);
    trace.startup_finished(None);

    let mut input = b"\n{not json\n{\"operation\":\"unknown_operation\"}\n".to_vec();
    input.extend(vec![b'x'; crate::host::MAX_HOST_CONTROL_FRAME_BYTES + 1]);
    input.extend_from_slice(b"\n{\"operation\":\"work_session_read\"}");
    // A reader smaller than the oversize frame, so its remainder is drained
    // in the same frame.
    let reader = |input: Vec<u8>| std::io::BufReader::with_capacity(64, Cursor::new(input));
    let mut untraced = Vec::new();
    server
        .serve(reader(input.clone()), &mut untraced)
        .expect("untraced serve");
    let mut traced_output = Vec::new();
    server
        .serve_traced(reader(input), &mut traced_output, &trace)
        .expect("traced serve");
    assert_eq!(
        traced_output, untraced,
        "the trace changes no response byte"
    );
    assert_eq!(traced_output.split(|byte| *byte == b'\n').count() - 1, 5);

    let lines = written(&trace, &captured);
    let startup = terminal(&lines, 0);
    assert_eq!(startup["kind"], "startup");
    assert_eq!(startup["state"], "complete");
    assert_eq!(startup["outcome"], "ok");
    for phase in [
        "connection_open_ms",
        "schema_check_ms",
        "host_path_policy_ms",
        "policy_history_verify_ms",
        "schema_init_ms",
        "connection_resume_ms",
    ] {
        assert!(startup["phases"][phase].is_number(), "{phase}: {startup}");
    }
    assert!(
        startup["begin_immediate"]["count"].as_u64() >= Some(1),
        "{startup}"
    );
    let expected = [
        ("invalid", "invalid_request"),
        ("invalid", "invalid_request"),
        ("invalid", "invalid_request"),
        ("invalid", "invalid_request"),
        ("work_session_read", ""),
    ];
    for (index, (operation, outcome)) in expected.iter().enumerate() {
        let seq = u64::try_from(index).expect("small") + 1;
        let line = terminal(&lines, seq);
        assert_eq!(line["kind"], "frame");
        assert_eq!(line["state"], "complete", "{line}");
        assert_eq!(line["operation"], *operation, "{line}");
        if outcome.is_empty() {
            assert!(line["outcome"].is_string(), "{line}");
        } else {
            assert_eq!(line["outcome"], *outcome, "{line}");
        }
        for phase in [
            "handler_total_ms",
            "response_serialize_ms",
            "response_write_flush_ms",
        ] {
            assert!(line["phases"][phase].is_number(), "{phase}: {line}");
        }
        assert_eq!(line["pid"], std::process::id());
    }
    assert!(
        of_seq(&lines, 6).is_empty(),
        "a bare end of input takes no number"
    );
}

// A writer holding the lock while the control process starts shows up as
// the time of its connection-resume BEGIN IMMEDIATE, and while it waits, the
// in-flight lines name that phase and statement.
#[test]
fn a_contending_writer_raises_begin_immediate_at_startup() {
    let home = crate::test_support::temp_home().expect("home");
    let database = home.path().join("control.sqlite3");
    drop(open_server(&database));
    let blocker = rusqlite::Connection::open(&database).expect("blocker");
    blocker
        .execute_batch("BEGIN IMMEDIATE")
        .expect("hold the write lock");
    let (trace, captured) = traced_without_monitor();
    trace.install();
    let waiting = Arc::clone(&trace);
    let releaser = std::thread::spawn(move || -> Result<(), String> {
        let deadline = Instant::now() + Duration::from_secs(60);
        loop {
            let emitted = {
                let mut state = waiting.state.lock().expect("trace state");
                let Some(slot) = state.slot.as_mut() else {
                    return Err("startup ended before the contended BEGIN was observed".into());
                };
                if slot.startup
                    && slot.seq == 0
                    && slot.phase == ControlPhase::ConnectionResume
                    && matches!(slot.statement, Some((Statement::BeginImmediate, _)))
                    && let Some(line) = waiting.due_progress(slot, Instant::now())
                {
                    // Match the monitor's ordering, without waiting for its
                    // asynchronous writer or holding state during SQLite I/O.
                    waiting.lines.emit(line);
                    true
                } else {
                    false
                }
            };
            if emitted {
                break;
            }
            if Instant::now() >= deadline {
                return Err("timed out waiting for the contended BEGIN".into());
            }
            std::thread::sleep(Duration::from_millis(10));
        }
        std::thread::sleep(Duration::from_millis(300));
        blocker
            .execute_batch("COMMIT")
            .map_err(|error| format!("release the lock: {error}"))
    });

    // Exceed the old fixed hold and the autonomous progress budget before
    // opening, as a slow schema startup can do on a loaded host.
    std::thread::sleep(Duration::from_millis(1_000));
    let server = HostControlServer::open(
        &database,
        ProjectId("control-trace".into()),
        "agent".into(),
        SessionId("control-trace-session".into()),
        None,
    );
    trace.startup_finished(server.as_ref().err().map(crate::host::store_error_code));
    let thread_result = releaser.join();

    let lines = written(&trace, &captured);
    thread_result
        .expect("releaser")
        .unwrap_or_else(|error| panic!("{error}: {lines:?}"));
    drop(server.expect("control server"));
    let startup = terminal(&lines, 0);
    // Release follows the observed BEGIN, so schema startup cannot consume
    // the interval that must be measured as connection-resume contention.
    // Retained limit: the releaser must commit inside the product's 5 s busy
    // timeout, about 4.7 s after its hold, or startup is refused.
    assert!(
        startup["begin_immediate"]["max_ms"].as_f64() >= Some(250.0),
        "{startup}"
    );
    assert!(
        startup["phases"]["connection_resume_ms"].as_f64() >= Some(250.0),
        "{startup}"
    );
    assert!(
        lines.iter().any(|line| line["state"] == "in_flight"
            && line["phase"] == "connection_resume"
            && line["statement"] == "begin_immediate"),
        "{lines:?}"
    );
}

/// A response writer that blocks its first write, as a host that stops reading
/// stdout for a while does. The wait lasts `stall` plus all the time the frame
/// took to get here, so it exceeds any handler time however loaded the host,
/// and continues until the trace has written a line about it. It starts the
/// progress schedule over from this write, so a slow frame that already spent
/// its lines in an earlier phase still gets lines about this one.
struct Blocked {
    stall: Duration,
    frame_began: Instant,
    trace: Arc<ControlTrace>,
    captured: Captured,
    stalled: bool,
    /// The last line's ordinal when the schedule restarted: only a line above
    /// it counts against that schedule's limit.
    restarted_after: Arc<std::sync::atomic::AtomicU32>,
}

impl Blocked {
    fn progress_written(&self) -> bool {
        self.captured.text().lines().any(|line| {
            line.contains(r#""state":"in_flight""#)
                && line.contains(r#""phase":"response_write_flush""#)
        })
    }

    fn restart_progress(&self) {
        if let Some(slot) = self.trace.state.lock().expect("trace state").slot.as_mut() {
            slot.started = Instant::now();
            slot.progress = 0;
            self.restarted_after
                .store(slot.ordinal, std::sync::atomic::Ordering::SeqCst);
        }
        self.trace.changed.notify_all();
    }
}

impl Write for Blocked {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if !self.stalled {
            self.stalled = true;
            let stall = self.stall + self.frame_began.elapsed();
            self.restart_progress();
            let waiting = Instant::now();
            while waiting.elapsed() < crate::phase_trace::tests::HANG_GUARD
                && (waiting.elapsed() < stall || !self.progress_written())
            {
                std::thread::sleep(Duration::from_millis(10));
            }
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// A host that stops reading the response shows up as response_write_flush
// time, not handler time. While the frame waits, it leaves in-flight lines
// naming that phase, at most ten, in increasing order, and the terminal
// line comes last. A descheduled frame may leave earlier lines first, in
// earlier phases.
#[test]
fn a_blocked_reader_raises_response_write_flush_with_bounded_progress() {
    let home = crate::test_support::temp_home().expect("home");
    let database = home.path().join("control.sqlite3");
    let (trace, captured) = traced(FAST);
    trace.install();
    let mut server = open_server(&database);
    trace.startup_finished(None);
    let restarted_after = Arc::new(std::sync::atomic::AtomicU32::new(0));
    let blocked = Blocked {
        stall: Duration::from_millis(1_000),
        frame_began: Instant::now(),
        trace: Arc::clone(&trace),
        captured: captured.clone(),
        stalled: false,
        restarted_after: Arc::clone(&restarted_after),
    };
    server
        .serve_traced(Cursor::new(b"{}\n".to_vec()), blocked, &trace)
        .expect("traced serve");

    let lines = written(&trace, &captured);
    let frame = of_seq(&lines, 1);
    let progress = frame
        .iter()
        .filter(|line| line["state"] == "in_flight")
        .collect::<Vec<_>>();
    let in_order = |phase: &serde_json::Value| {
        [
            "handler_total",
            "response_serialize",
            "response_write_flush",
        ]
        .iter()
        .position(|known| phase == known)
        .unwrap_or_else(|| panic!("unexpected phase {phase}"))
    };
    let phases = progress
        .iter()
        .map(|line| in_order(&line["phase"]))
        .collect::<Vec<_>>();
    assert!(phases.is_sorted(), "{progress:?}");
    let waiting = progress
        .iter()
        .filter(|line| line["phase"] == "response_write_flush")
        .collect::<Vec<_>>();
    assert!(!waiting.is_empty(), "{frame:?}");
    let restarted_after = u64::from(restarted_after.load(std::sync::atomic::Ordering::SeqCst));
    let since_restart = waiting
        .iter()
        .filter(|line| line["ordinal"].as_u64().expect("ordinal") > restarted_after)
        .count();
    assert!(
        since_restart <= MAX_PROGRESS as usize,
        "{since_restart}: {waiting:?}"
    );
    assert!(
        waiting.iter().all(|line| line["operation"] == "invalid"),
        "{waiting:?}"
    );
    let ordinals = frame
        .iter()
        .map(|line| line["ordinal"].as_u64().expect("ordinal"))
        .collect::<Vec<_>>();
    assert!(
        ordinals.windows(2).all(|pair| pair[0] < pair[1]),
        "{ordinals:?}"
    );
    let last = frame.last().expect("lines");
    assert_eq!(last["state"], "complete", "the terminal line is last");
    assert!(
        last["phases"]["response_write_flush_ms"].as_f64() >= Some(990.0),
        "{last}"
    );
    // The wait is longer than all the frame's earlier time, so only a handler
    // charged with the wait could reach it.
    assert!(
        last["phases"]["handler_total_ms"].as_f64()
            < last["phases"]["response_write_flush_ms"].as_f64(),
        "{last}"
    );
}

// The same store work with the trace on and off: tracing adds no statement and
// no SQL step, at startup or per frame, so a trace that reads the store or
// repeats a statement fails here at any host speed.
#[test]
fn tracing_adds_no_sql_work_to_a_frame() {
    use crate::storage::work_cost::sqlite::{Metrics, start};

    const FRAMES: u64 = 20;

    fn work(database: &std::path::Path, trace: Option<&Arc<ControlTrace>>) -> [Metrics; 2] {
        let frame = b"{\"operation\":\"work_session_read\"}\n";
        let frames = frame.repeat(usize::try_from(FRAMES).expect("small"));
        let scope = start();
        if let Some(trace) = trace {
            trace.install();
        }
        let mut server = open_server(database);
        if let Some(trace) = trace {
            trace.startup_finished(None);
        }
        let startup = scope.finish();
        let scope = start();
        let mut responses = Vec::new();
        match trace {
            Some(trace) => {
                server
                    .serve_traced(Cursor::new(frames), &mut responses, trace)
                    .expect("traced serve");
                trace.close();
            }
            None => server
                .serve(Cursor::new(frames), &mut responses)
                .expect("untraced serve"),
        }
        let answering = scope.finish();
        assert_eq!(
            String::from_utf8(responses).expect("UTF-8").lines().count(),
            usize::try_from(FRAMES).expect("small")
        );
        [startup, answering]
    }

    let home = crate::test_support::temp_home().expect("home");
    // Work done once per process falls outside both measurements.
    work(&home.path().join("warmed.sqlite3"), None);
    let [plain_startup, plain_frames] = work(&home.path().join("plain.sqlite3"), None);
    let (trace, _captured) = traced(ProgressTiming::DEFAULT);
    let [startup, frames] = work(&home.path().join("traced.sqlite3"), Some(&trace));
    // Measured apart from startup, so these statements are the frames' own.
    assert!(plain_frames.total.statements >= FRAMES, "{plain_frames:?}");
    assert_eq!(frames.total, plain_frames.total);
    assert_eq!(frames.by_kind, plain_frames.by_kind);
    assert_eq!(startup.total, plain_startup.total);
    assert_eq!(startup.by_kind, plain_startup.by_kind);
}

// A line is encoded once, and every encoded byte reaches the sink, at any host
// speed: a trace that encodes a line repeatedly, or encodes lines it never
// emits, fails here. Not counted: building a line's fields, an encoding outside
// the trace's one encoder, and the writer thread's time.
#[test]
fn each_trace_line_is_encoded_exactly_once() {
    let home = crate::test_support::temp_home().expect("home");
    let (trace, captured) = traced(ProgressTiming::DEFAULT);
    trace.install();
    let mut server = open_server(&home.path().join("control.sqlite3"));
    trace.startup_finished(None);
    let frames = b"{\"operation\":\"work_session_read\"}\n".repeat(20);
    server
        .serve_traced(Cursor::new(frames), std::io::sink(), &trace)
        .expect("traced serve");
    let lines = written(&trace, &captured);
    assert!(lines.len() >= 21, "startup and 20 frames: {lines:?}");
    assert_eq!(trace.lines.dropped(), 0);
    let order = std::sync::atomic::Ordering::SeqCst;
    assert_eq!(
        trace.encoded.calls.load(order),
        u64::try_from(lines.len()).expect("small")
    );
    let text = captured.text();
    let without_newlines = text.len() - text.lines().count();
    assert_eq!(
        trace.encoded.bytes.load(order),
        u64::try_from(without_newlines).expect("small")
    );
}

// Tracing costs a frame no waiting on the trace's reader: with a stderr nobody
// drains, every frame is still answered. The sink stays shut until the frames
// have finished, so finishing at all proves none of them waited on it.
#[test]
fn an_undrained_sink_never_stalls_a_traced_frame() {
    use crate::phase_trace::tests::{HANG_GUARD, Undrained};

    let home = crate::test_support::temp_home().expect("home");
    let database = home.path().join("control.sqlite3");
    let sink = Undrained::new();
    let trace = ControlTrace::with_sink(Box::new(sink.clone()), 4, FAST);
    let (finished, done) = std::sync::mpsc::channel::<()>();
    let opener = std::thread::spawn(move || {
        let stalled = matches!(
            done.recv_timeout(HANG_GUARD),
            Err(std::sync::mpsc::RecvTimeoutError::Timeout)
        );
        sink.open();
        stalled
    });
    trace.install();
    let mut server = open_server(&database);
    trace.startup_finished(None);
    let mut responses = Vec::new();
    server
        .serve_traced(Cursor::new(b"{}\n".repeat(40)), &mut responses, &trace)
        .expect("traced serve");
    drop(finished);
    assert!(
        !opener.join().expect("opener"),
        "a frame waited on the sink"
    );
    assert_eq!(
        String::from_utf8(responses).expect("UTF-8").lines().count(),
        40
    );
    trace.close();
}

/// A response writer whose writes fail, as a closed pipe does.
struct Broken;

impl Write for Broken {
    fn write(&mut self, _: &[u8]) -> std::io::Result<usize> {
        Err(std::io::Error::new(
            std::io::ErrorKind::BrokenPipe,
            "closed",
        ))
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// A reader that yields some bytes of a frame, then fails.
struct Torn(Option<Vec<u8>>);

impl Read for Torn {
    fn read(&mut self, buffer: &mut [u8]) -> std::io::Result<usize> {
        match self.0.take() {
            Some(bytes) => {
                buffer[..bytes.len()].copy_from_slice(&bytes);
                Ok(bytes.len())
            }
            None => Err(std::io::Error::other("torn input")),
        }
    }
}

// A response that cannot be written reads write_failed with its outcome; a
// frame whose input fails after its first bytes reads incomplete with its
// number, and has no response.
#[test]
fn a_failed_write_and_a_torn_frame_each_end_in_their_state() {
    let home = crate::test_support::temp_home().expect("home");
    let database = home.path().join("control.sqlite3");
    let (trace, captured) = traced(ProgressTiming::DEFAULT);
    trace.install();
    let mut server = open_server(&database);
    trace.startup_finished(None);
    let failed = server.serve_traced(Cursor::new(b"{}\n".to_vec()), Broken, &trace);
    assert!(failed.is_err());
    let mut output = Vec::new();
    let torn = server.serve_traced(
        std::io::BufReader::new(Torn(Some(b"{\"operation\"".to_vec()))),
        &mut output,
        &trace,
    );
    assert!(torn.is_err());
    assert!(output.is_empty(), "{output:?}");

    let lines = written(&trace, &captured);
    let first = terminal(&lines, 1);
    assert_eq!(first["state"], "write_failed", "{first}");
    assert_eq!(first["outcome"], "invalid_request");
    let second = terminal(&lines, 2);
    assert_eq!(second["state"], "incomplete", "{second}");
    assert!(second.get("outcome").is_none(), "{second}");
}

// Unset, nothing is installed: the hooks see no trace on any thread, and the
// server serves as it always did.
#[test]
fn unset_installs_nothing() {
    assert!(!active());
    let home = crate::test_support::temp_home().expect("home");
    let mut server = open_server(&home.path().join("control.sqlite3"));
    assert!(!active());
    let mut output = Vec::new();
    server
        .serve(Cursor::new(b"{}\n".to_vec()), &mut output)
        .expect("serve");
    assert!(!output.is_empty(), "{output:?}");
}

// A line names fixed labels only: no request text reaches it, and every
// line is one bounded line.
#[test]
fn a_line_carries_no_request_text() {
    let home = crate::test_support::temp_home().expect("home");
    let database = home.path().join("control.sqlite3");
    let (trace, captured) = traced(ProgressTiming::DEFAULT);
    trace.install();
    let mut server = open_server(&database);
    trace.startup_finished(None);
    let secret = "SECRET-token-C:/private/path";
    // A parsed request carries the secret in its text fields; a second frame
    // carries it as an unknown operation name.
    let claim = uuid::Uuid::new_v4();
    let input = format!(
        "{{\"operation\":\"named_root_bind\",\"claim_id\":\"{claim}\",\"claim_fence\":1,\"workspace_id\":\"{secret}\",\"generation\":1,\"named_at\":\"2020-01-01T00:00:00Z\",\"kind\":\"bound\",\"idempotency_key\":\"{secret}\"}}\n{{\"operation\":\"{secret}\"}}\n"
    );
    let length = input.len();
    let mut cursor = Cursor::new(input.into_bytes());
    let mut output = Vec::new();
    server
        .serve_traced(&mut cursor, &mut output, &trace)
        .expect("serve");
    assert_eq!(cursor.position(), u64::try_from(length).expect("length"));
    // Every line, once the writer has caught up, is read from one snapshot.
    let lines = written(&trace, &captured);
    let text = captured.text();
    assert_eq!(text.lines().count(), lines.len());
    assert!(
        !text.contains("SECRET") && !text.contains("private"),
        "{text}"
    );
    for line in text.lines() {
        assert!(line.len() < MAX_LINE_BYTES, "{}", line.len());
    }
    assert_eq!(terminal(&lines, 1)["operation"], "named_root_bind");
    assert_eq!(terminal(&lines, 2)["operation"], "invalid");
}

// A control process that cannot open its store ends its startup record as
// incomplete, with the store error's fixed code, as `engram control` does.
#[test]
fn a_failed_startup_reads_incomplete_with_its_code() {
    let home = crate::test_support::temp_home().expect("home");
    let (trace, captured) = traced(ProgressTiming::DEFAULT);
    trace.install();
    // A directory is no store.
    let Err(error) = HostControlServer::open(
        home.path(),
        ProjectId("control-trace".into()),
        "agent".into(),
        SessionId("control-trace-session".into()),
        None,
    ) else {
        panic!("a directory is no store");
    };
    trace.startup_finished(Some(crate::host::store_error_code(&error)));
    let lines = written(&trace, &captured);
    let startup = terminal(&lines, 0);
    assert_eq!(startup["state"], "incomplete", "{startup}");
    assert!(startup["outcome"].is_string(), "{startup}");
    assert_ne!(startup["outcome"], "ok");
    assert!(
        startup["phases"]["connection_open_ms"].is_number(),
        "{startup}"
    );
}

// Startup is timed from the process's start: the time before the trace was
// installed counts as `starting`, never as no phase. A startup ended early
// reads incomplete with its code, and abandoning one already finished
// changes nothing.
#[test]
fn startup_counts_from_main_and_an_early_return_abandons_it() {
    let (trace, captured) = traced(ProgressTiming::DEFAULT);
    let main = Instant::now()
        .checked_sub(Duration::from_millis(300))
        .expect("an earlier instant");
    trace.install_from(main);
    trace.enter(ControlPhase::ProjectResolve);
    trace.abandon_startup("startup_failed");
    trace.abandon_startup("not_written");
    let lines = written(&trace, &captured);
    let startup = terminal(&lines, 0);
    assert_eq!(startup["state"], "incomplete", "{startup}");
    assert_eq!(startup["outcome"], "startup_failed");
    assert!(
        startup["phases"]["starting_ms"].as_f64() >= Some(290.0),
        "{startup}"
    );
    assert!(
        startup["phases"]["project_resolve_ms"].is_number(),
        "{startup}"
    );
    assert!(
        startup["phases"].get("connection_open_ms").is_none(),
        "{startup}"
    );
}
