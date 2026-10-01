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

/// Progress soon enough for a test, but well after a frame's read and
/// handler, so only a stall writes it.
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

/// The lines written so far, after `close`, once the writer has caught up:
/// close waits only briefly, so a loaded host is given longer here.
fn written(trace: &ControlTrace, captured: &Captured) -> Vec<serde_json::Value> {
    trace.close();
    trace.lines.flush(Duration::from_secs(10));
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
    input.extend_from_slice(b"\n{\"operation\":\"session_status\",\"routing_token\":\"rt\"}");
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
        ("session_status", ""),
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
    let held = Duration::from_millis(1_000);
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(held);
        blocker.execute_batch("COMMIT").expect("release the lock");
    });
    let (trace, captured) = traced(FAST);
    trace.install();
    let server = open_server(&database);
    trace.startup_finished(None);
    releaser.join().expect("releaser");
    drop(server);

    let lines = written(&trace, &captured);
    let startup = terminal(&lines, 0);
    // The lock is held a second from before startup; the resume waits for
    // most of it, however slowly the store opens on a loaded host.
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

/// A response writer that blocks its first write for `stall`, measured from
/// that write, as a host that stops reading stdout for a while does.
struct Blocked {
    stall: Duration,
    stalled: bool,
}

impl Write for Blocked {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        if !self.stalled {
            self.stalled = true;
            std::thread::sleep(self.stall);
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
// line comes last.
#[test]
fn a_blocked_reader_raises_response_write_flush_with_bounded_progress() {
    let home = crate::test_support::temp_home().expect("home");
    let database = home.path().join("control.sqlite3");
    let (trace, captured) = traced(FAST);
    trace.install();
    let mut server = open_server(&database);
    trace.startup_finished(None);
    let blocked = Blocked {
        stall: Duration::from_millis(1_000),
        stalled: false,
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
    assert!(!progress.is_empty(), "{frame:?}");
    assert!(
        progress.len() <= MAX_PROGRESS as usize,
        "{}",
        progress.len()
    );
    assert!(
        progress
            .iter()
            .all(|line| line["phase"] == "response_write_flush" && line["operation"] == "invalid"),
        "{progress:?}"
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
    assert!(
        last["phases"]["handler_total_ms"].as_f64() < Some(300.0),
        "{last}"
    );
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
    assert!(output.is_empty());

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
    assert!(!output.is_empty());
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
    let input = format!(
        "{{\"operation\":\"session_status\",\"routing_token\":\"{secret}\"}}\n{{\"operation\":\"{secret}\"}}\n"
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
