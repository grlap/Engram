use super::*;
use crate::domain::{ProjectId, SessionId};
use crate::verbs::{AddInput, AgentVerbs};
use chrono::{TimeZone, Utc};

fn at(second: i64) -> chrono::DateTime<Utc> {
    Utc.timestamp_opt(1_790_000_000 + second, 0)
        .single()
        .expect("timestamp")
}

// Field order matters: the verbs close their store before the home goes.
struct Agent {
    verbs: AgentVerbs,
    database: std::path::PathBuf,
    _home: crate::test_support::TempHome,
}

fn agent(project: &str) -> Agent {
    let home = crate::test_support::temp_home().expect("temporary home");
    let database = home.path().join("engram.sqlite3");
    Agent {
        verbs: AgentVerbs::new(
            database.clone(),
            ProjectId(project.into()),
            "agent".into(),
            SessionId("phase-trace-session".into()),
            Some("phase-trace-test".into()),
        ),
        database,
        _home: home,
    }
}

fn add(agent: &Agent, title: &str, second: i64) -> String {
    let receipt = agent
        .verbs
        .add(
            AddInput {
                external: None,
                notes: Vec::new(),
                title: title.into(),
                outcome: None,
                acceptance: vec![format!("{title} is delivered")],
                bindings: Vec::new(),
                under: None,
                optional: false,
                priority: None,
                labels: Vec::new(),
                assignee: None,
                kind: None,
                evaluation_mode: None,
            },
            at(second),
        )
        .expect("add");
    receipt.value["work"]["short_ref"]
        .as_str()
        .expect("ref")
        .to_owned()
}

/// Polls `future` to completion on this thread; the futures here never wait.
fn block_on<F: Future>(future: F) -> F::Output {
    let mut future = std::pin::pin!(future);
    let waker = std::task::Waker::noop();
    let mut context = Context::from_waker(waker);
    loop {
        if let Poll::Ready(output) = future.as_mut().poll(&mut context) {
            return output;
        }
    }
}

// Hooks record only inside a traced call's poll: outside one they take no
// timestamp and record nothing, and the accumulator never leaks past it.
#[test]
fn hooks_record_only_inside_a_traced_call() {
    assert!(!active());
    assert_eq!(start(), None);
    finish(Phase::Commit, start());
    let ((), phases) = block_on(scoped(async {
        assert!(active());
        let started = start();
        assert!(started.is_some());
        finish(Phase::ReceiptSerialize, started);
        finish(Phase::ReceiptSerialize, start());
    }));
    assert!(!active(), "the accumulator leaked past the call");
    assert_eq!(phases.receipt_serialize.count, 2);
    assert_eq!(phases.commit.count, 0);
    assert_eq!(phases.store_open.count, 0);
}

// A traced call that writes opens the store, takes BEGIN IMMEDIATE and
// commits; the trace times each. Results do not change with the trace: the
// same read inside and outside a traced call returns the same receipt.
#[test]
fn a_traced_call_times_the_store_and_returns_the_same_result() {
    let agent = agent("phase-trace-results");
    let ((), opened) = block_on(scoped(async {
        add(&agent, "First traced write", 1);
    }));
    assert_eq!(opened.store_open.count, 1, "{opened:?}");
    assert!(opened.store_mutex_wait.count >= 1, "{opened:?}");
    assert!(opened.begin_immediate.count >= 1, "{opened:?}");
    assert!(opened.commit.count >= 1, "{opened:?}");

    let work = add(&agent, "Read back", 2);
    let untraced = agent.verbs.show(&work, at(3)).expect("untraced show");
    let (traced, phases) = block_on(scoped(async { agent.verbs.show(&work, at(3)) }));
    let traced = traced.expect("traced show");
    assert_eq!(traced.value, untraced.value);
    assert_eq!(traced.text(), untraced.text());
    // An error is unchanged too: the same refusal, message and guidance.
    let untraced = agent
        .verbs
        .show("w-000000000000", at(4))
        .expect_err("untraced refusal");
    let (traced, _) = block_on(scoped(async { agent.verbs.show("w-000000000000", at(4)) }));
    let traced = traced.expect_err("traced refusal");
    assert_eq!(traced.error.to_string(), untraced.error.to_string());
    assert_eq!(
        format!("{:?}", traced.guidance()),
        format!("{:?}", untraced.guidance())
    );
    // A read word opens its own read-only connection for each call, takes
    // no write transaction, and its read snapshot's COMMIT is timed too.
    assert_eq!(phases.store_open.count, 1, "{phases:?}");
    assert_eq!(phases.begin_immediate.count, 0, "{phases:?}");
    assert!(phases.commit.count >= 1, "{phases:?}");

    // Peek opens its own read-only connection as well.
    let ((), peeked) = block_on(scoped(async {
        agent
            .verbs
            .next(
                &crate::verbs::NextInput {
                    limit: None,
                    peek: true,
                    verbose: false,
                    context_generation: None,
                },
                at(5),
            )
            .expect("peek");
    }));
    assert_eq!(peeked.store_open.count, 1, "{peeked:?}");
    assert!(peeked.commit.count >= 1, "{peeked:?}");
}

// A writer holding the lock shows up in begin_immediate, not elsewhere: the
// call waits in BEGIN IMMEDIATE until the other writer commits.
#[test]
fn a_contending_writer_raises_begin_immediate() {
    let agent = agent("phase-trace-contention");
    block_on(scoped(async {
        add(&agent, "Opens the traced store", 1);
    }));
    let blocker = rusqlite::Connection::open(&agent.database).expect("second connection");
    blocker
        .execute_batch("BEGIN IMMEDIATE")
        .expect("hold the write lock");
    let held = Duration::from_millis(400);
    let releaser = std::thread::spawn(move || {
        std::thread::sleep(held);
        blocker
            .execute_batch("COMMIT")
            .expect("release the write lock");
    });
    let ((), phases) = block_on(scoped(async {
        add(&agent, "Waits for the lock", 2);
    }));
    releaser.join().expect("releaser");
    assert!(
        phases.begin_immediate.max >= Duration::from_millis(250),
        "{phases:?}"
    );
    assert!(phases.commit.count >= 1, "{phases:?}");
}

/// A sink tests read back: every record line written.
#[derive(Clone, Default)]
struct Captured(Arc<Mutex<Vec<u8>>>);

impl std::io::Write for Captured {
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
                assert!(line.len() <= MAX_RECORD_BYTES, "{line}");
                serde_json::from_str(line).expect("one JSON record per line")
            })
            .collect()
    }
}

fn captured_trace() -> (PhaseTrace, Captured) {
    let captured = Captured::default();
    (
        PhaseTrace::new(Box::new(captured.clone()), WRITER_CAPACITY),
        captured,
    )
}

#[test]
fn line_writer_submits_each_record_with_its_newline() {
    #[derive(Clone, Default)]
    struct Writes(Arc<Mutex<Vec<Vec<u8>>>>);

    impl std::io::Write for Writes {
        fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
            self.0.lock().expect("writes").push(bytes.to_vec());
            Ok(bytes.len())
        }

        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    let writes = Writes::default();
    let record_sink = LineWriter::new(Box::new(writes.clone()), 4, "trace-record-write-test");
    let record = serde_json::json!({ "stage": "trace_flush_finished" }).to_string();
    record_sink.emit(record.clone());
    record_sink.flush(Duration::from_secs(5));
    assert_eq!(
        *writes.0.lock().expect("writes"),
        vec![format!("{record}\n").into_bytes()]
    );
}

/// The lines written so far, once the writer has caught up.
fn written(trace: &PhaseTrace, captured: &Captured) -> Vec<serde_json::Value> {
    trace.flush(Duration::from_secs(5));
    captured
        .lines()
        .into_iter()
        .filter(|line| line["engram_mcp_phase_trace"] == 1)
        .collect()
}

fn numeric(id: i64) -> NumberOrString {
    NumberOrString::Number(id)
}

fn some_phases() -> Phases {
    let mut phases = Phases::default();
    phases.begin_immediate.add(Duration::from_millis(3));
    phases.commit.add(Duration::from_millis(1));
    phases
}

fn settle(trace: &PhaseTrace, id: i64, tool: &str) {
    trace.handler_settled(numeric(id), tool, Duration::from_millis(1), some_phases());
}

fn states(lines: &[serde_json::Value]) -> Vec<(serde_json::Value, serde_json::Value)> {
    lines
        .iter()
        .map(|line| (line["id"].clone(), line["state"].clone()))
        .collect()
}

// The pending records are bounded: the oldest is written as evicted and
// counted, a send with no record is counted, and close writes what is left as
// incomplete. Every settled call ends in exactly one line.
#[test]
fn every_record_ends_in_one_line_and_the_wait_is_bounded() {
    let (trace, captured) = captured_trace();
    for id in 0..300 {
        settle(&trace, id, "note");
    }
    assert_eq!(trace.pending_len(), MAX_PENDING);
    // The evicted call's response finds no record.
    trace.dispatching(&numeric(0));
    trace.sent(&numeric(0), Duration::from_millis(1), true);
    trace.dispatching(&numeric(299));
    trace.sent(&numeric(299), Duration::from_millis(1), true);
    assert_eq!(trace.pending_len(), MAX_PENDING - 1);
    trace.closed();
    assert_eq!(trace.pending_len(), 0);
    let lines = written(&trace, &captured);
    assert_eq!(lines.len(), 300, "one line for each settled call");
    let state = |id: i64| {
        lines
            .iter()
            .find(|line| line["id"] == id)
            .map(|line| line["state"].clone())
    };
    assert_eq!(state(0), Some(serde_json::json!("evicted")));
    assert_eq!(state(43), Some(serde_json::json!("evicted")));
    assert_eq!(state(299), Some(serde_json::json!("complete")));
    assert_eq!(state(44), Some(serde_json::json!("incomplete")));
    let last = lines.last().expect("a line");
    assert_eq!(last["evicted"], 300 - MAX_PENDING as u64);
    assert_eq!(last["unmatched_sends"], 1);
    assert_eq!(last["dropped_lines"], 0);
}

// A call its client cancelled before its response was handed over is only
// marked: rmcp may still send that response while it drains at shutdown. At
// close a marked call never sent reads cancelled, and a call settling after
// close is written at once. A failed send is never reported as delivered.
#[test]
fn cancelled_late_and_failed_calls_are_written_with_their_state() {
    let (trace, captured) = captured_trace();
    trace.client_cancelled(&numeric(1));
    settle(&trace, 1, "note");
    assert_eq!(trace.pending_len(), 1, "a cancelled call waits for close");
    settle(&trace, 2, "gate");
    trace.dispatching(&numeric(2));
    trace.sent(&numeric(2), Duration::from_millis(1), false);
    trace.closed();
    trace.client_cancelled(&numeric(4));
    settle(&trace, 3, "done");
    settle(&trace, 4, "show");
    assert_eq!(trace.pending_len(), 0, "a settle after close does not wait");
    let lines = written(&trace, &captured);
    assert_eq!(
        states(&lines),
        vec![
            (serde_json::json!(2), serde_json::json!("send_failed")),
            (serde_json::json!(1), serde_json::json!("cancelled")),
            (serde_json::json!(3), serde_json::json!("incomplete")),
            (serde_json::json!(4), serde_json::json!("cancelled")),
        ]
    );
    assert!(lines[0]["wire_encode_send_inclusive_ms"].is_number());
    assert_eq!(lines[0]["cancel_requested"], false);
    assert_eq!(
        lines[1]["wire_encode_send_inclusive_ms"],
        serde_json::Value::Null
    );
    assert_eq!(lines[1]["cancel_requested"], true);
}

// A cancel reaching the server after the handler settled but before its
// response was handed over marks the record, so it never reads incomplete:
// unsent at close, it reads cancelled. When rmcp's shutdown drain sends the
// response after all, the record completes as sent, with the cancel noted.
#[test]
fn a_cancel_before_dispatch_reads_cancelled_unless_the_response_is_sent() {
    let (trace, captured) = captured_trace();
    settle(&trace, 1, "note");
    settle(&trace, 2, "show");
    trace.client_cancelled(&numeric(1));
    assert_eq!(trace.pending_len(), 2, "the cancelled record waits");
    settle(&trace, 5, "gate");
    trace.client_cancelled(&numeric(5));
    trace.dispatching(&numeric(5));
    trace.sent(&numeric(5), Duration::from_millis(1), true);
    trace.closed();
    let lines = written(&trace, &captured);
    assert_eq!(
        states(&lines),
        vec![
            (serde_json::json!(5), serde_json::json!("complete")),
            (serde_json::json!(1), serde_json::json!("cancelled")),
            (serde_json::json!(2), serde_json::json!("incomplete")),
        ]
    );
    assert_eq!(lines[0]["cancel_requested"], true);
    assert_eq!(lines[0]["unmatched_sends"], 0);
    assert_eq!(lines[2]["cancel_requested"], false);
}

// Only a client's cancel marks a call cancelled. rmcp also cancels a request
// token on its normal path, just before it hands the response over, and other
// calls may settle in between; none of that is taken for a cancel. A cancel
// arriving after the response was handed over leaves it complete, and the next
// request reusing that id clears it, so the new call waits for its own send.
#[test]
fn a_normal_response_is_never_taken_for_cancelled() {
    let (trace, captured) = captured_trace();
    settle(&trace, 7, "note");
    settle(&trace, 8, "show");
    settle(&trace, 9, "show");
    trace.dispatching(&numeric(7));
    trace.client_cancelled(&numeric(7));
    settle(&trace, 10, "show");
    trace.sent(&numeric(7), Duration::from_millis(1), true);
    trace.requested(&numeric(7));
    settle(&trace, 7, "gate");
    assert_eq!(trace.pending_len(), 4, "8, 9, 10 and the new 7 wait");
    let lines = written(&trace, &captured);
    assert_eq!(
        states(&lines),
        vec![(serde_json::json!(7), serde_json::json!("complete"))]
    );
    assert_eq!(lines[0]["tool"], "note");
    assert_eq!(lines[0]["cancel_requested"], false);
}

// Cancels naming no waiting call are remembered within the same bound as the
// records, so cancels for requests that were no tool call cannot grow it.
#[test]
fn remembered_cancels_are_bounded() {
    let (trace, captured) = captured_trace();
    for id in 0..300 {
        trace.client_cancelled(&numeric(id));
    }
    assert_eq!(
        trace.pending.lock().expect("pending").cancels.len(),
        MAX_PENDING
    );
    // The oldest were let go: that call is no longer marked.
    settle(&trace, 0, "note");
    settle(&trace, 299, "note");
    trace.closed();
    let lines = written(&trace, &captured);
    assert_eq!(
        states(&lines),
        vec![
            (serde_json::json!(0), serde_json::json!("incomplete")),
            (serde_json::json!(299), serde_json::json!("cancelled")),
        ]
    );
}

// A reused request id completes the newest record carrying it, so a send
// never pairs with an older call's phases.
#[test]
fn a_reused_id_completes_the_newest_record() {
    let (trace, captured) = captured_trace();
    trace.handler_settled(
        numeric(9),
        "note",
        Duration::from_millis(100),
        Phases::default(),
    );
    trace.handler_settled(
        numeric(9),
        "gate",
        Duration::from_millis(1),
        Phases::default(),
    );
    trace.dispatching(&numeric(9));
    trace.sent(&numeric(9), Duration::from_millis(1), true);
    let lines = written(&trace, &captured);
    assert_eq!(lines.len(), 1);
    assert_eq!(lines[0]["tool"], "gate");
    assert_eq!(trace.pending_len(), 1);
}

/// A sink that blocks every write until it is opened, as a stderr nobody
/// drains does.
#[derive(Clone)]
struct Undrained(Arc<(Mutex<bool>, std::sync::Condvar)>);

impl Undrained {
    fn new() -> Self {
        Self(Arc::new((Mutex::new(false), std::sync::Condvar::new())))
    }

    fn open(&self) {
        let (open, changed) = &*self.0;
        *open.lock().expect("gate") = true;
        changed.notify_all();
    }
}

impl std::io::Write for Undrained {
    fn write(&mut self, bytes: &[u8]) -> std::io::Result<usize> {
        let (open, changed) = &*self.0;
        let mut open = open.lock().expect("gate");
        while !*open {
            open = changed.wait(open).expect("gate");
        }
        Ok(bytes.len())
    }

    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

// A stderr nobody drains never stalls the server: settling and sending return
// at once, the queue stays bounded, and the lines that do not fit are dropped
// and counted.
#[test]
fn an_undrained_sink_drops_and_counts_lines_without_blocking() {
    let sink = Undrained::new();
    let trace = PhaseTrace::new(Box::new(sink.clone()), 4);
    let started = Instant::now();
    for id in 0..40 {
        settle(&trace, id, "note");
        trace.dispatching(&numeric(id));
        trace.sent(&numeric(id), Duration::from_millis(1), true);
    }
    assert!(
        started.elapsed() < Duration::from_secs(2),
        "{:?}",
        started.elapsed()
    );
    // One line may be held by the blocked writer and four queued.
    assert!(trace.dropped() >= 35, "{}", trace.dropped());
    sink.open();
    trace.flush(Duration::from_secs(5));
}

// A record is one bounded line naming phases only. A string request id is
// never echoed; its correlation is reported unavailable. An unknown tool
// name is recorded as unknown.
#[test]
fn a_record_is_bounded_and_carries_no_caller_text() {
    let record = HandlerRecord {
        id: NumberOrString::String("SELECT secret FROM C:\\private".into()),
        tool: "unknown",
        handler: Duration::from_millis(12),
        phases: some_phases(),
        dispatched: false,
        cancel_requested: false,
    };
    let line = render(
        &record,
        Outcome::Complete,
        Some(Duration::from_millis(2)),
        Counters::default(),
    );
    assert!(line.len() <= MAX_RECORD_BYTES);
    assert!(
        !line.contains("SELECT") && !line.contains("private"),
        "{line}"
    );
    let value: serde_json::Value = serde_json::from_str(&line).expect("one JSON line");
    assert_eq!(value["correlation"], "unavailable");
    assert_eq!(value["id"], serde_json::Value::Null);
    assert_eq!(value["state"], "complete");
    assert_eq!(value["begin_immediate"]["count"], 1);
    assert_eq!(value["tool"], "unknown");

    let (trace, _) = captured_trace();
    trace.handler_settled(numeric(1), "drop table", Duration::ZERO, Phases::default());
    assert_eq!(
        trace.pending.lock().expect("pending").records[0].tool,
        "unknown"
    );
}

/// A transport whose send takes `delay`, or fails, standing in for a pipe.
struct SlowTransport {
    delay: Duration,
    fail: bool,
    closed: bool,
    incoming: VecDeque<RxJsonRpcMessage<RoleServer>>,
}

impl Transport<RoleServer> for SlowTransport {
    type Error = std::io::Error;

    fn send(
        &mut self,
        _item: TxJsonRpcMessage<RoleServer>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        let delay = self.delay;
        let fail = self.fail;
        async move {
            std::thread::sleep(delay);
            if fail {
                Err(std::io::Error::new(
                    std::io::ErrorKind::BrokenPipe,
                    "pipe closed",
                ))
            } else {
                Ok(())
            }
        }
    }

    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleServer>>> + Send {
        std::future::ready(self.incoming.pop_front())
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.closed = true;
        async { Ok(()) }
    }
}

fn response(id: i64) -> TxJsonRpcMessage<RoleServer> {
    JsonRpcMessage::Response(rmcp::model::JsonRpcResponse {
        jsonrpc: rmcp::model::JsonRpcVersion2_0,
        id: numeric(id),
        result: rmcp::model::ServerResult::EmptyResult(rmcp::model::EmptyObject {}),
    })
}

// A pending close must expose the entered stage before it can complete.
#[test]
fn shutdown_milestones_distinguish_input_end_and_a_pending_transport_close() {
    struct HeldClose(Arc<std::sync::atomic::AtomicBool>);
    impl Transport<RoleServer> for HeldClose {
        type Error = std::io::Error;
        fn send(
            &mut self,
            _: TxJsonRpcMessage<RoleServer>,
        ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
            std::future::ready(Ok(()))
        }
        fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleServer>>> + Send {
            std::future::ready(None)
        }
        fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
            let released = Arc::clone(&self.0);
            std::future::poll_fn(move |_| {
                if released.load(std::sync::atomic::Ordering::Relaxed) {
                    Poll::Ready(Ok(()))
                } else {
                    Poll::Pending
                }
            })
        }
    }
    let (trace, captured) = captured_trace();
    let trace = Arc::new(trace);
    let released = Arc::new(std::sync::atomic::AtomicBool::new(false));
    let mut transport = TracingTransport::new(HeldClose(Arc::clone(&released)), Arc::clone(&trace));
    assert!(block_on(transport.receive()).is_none());
    let mut closing = std::pin::pin!(transport.close());
    assert!(
        closing
            .as_mut()
            .poll(&mut Context::from_waker(std::task::Waker::noop()))
            .is_pending()
    );
    trace.flush(Duration::from_secs(5));
    let before = captured.lines();
    assert_eq!(before.len(), 2);
    assert_eq!(before[0]["stage"], "input_ended");
    assert_eq!(before[1]["stage"], "transport_close_started");
    released.store(true, std::sync::atomic::Ordering::Relaxed);
    block_on(closing).expect("released close");
    trace.shutdown_stage(ShutdownStage::ServiceWaitingReturned);
    trace.flush(Duration::from_secs(5));
    let lines = captured.lines();
    let stages: Vec<_> = lines
        .iter()
        .map(|line| line["stage"].as_str().expect("stage"))
        .collect();
    assert_eq!(
        stages,
        vec![
            "input_ended",
            "transport_close_started",
            "transport_close_finished",
            "trace_flush_started",
            "trace_flush_finished",
            "service_waiting_returned"
        ]
    );
    let times: Vec<_> = lines
        .iter()
        .map(|line| line["server_elapsed_ms"].as_f64().expect("elapsed"))
        .collect();
    assert!(times.windows(2).all(|pair| pair[0] <= pair[1]));
    assert!(
        lines
            .iter()
            .all(|line| line["engram_mcp_shutdown_trace"] == 1
                && line.as_object().expect("object").len() == 4)
    );
}

// The transport passes messages through and completes the record of the call
// its response answers, timing the send; a failed send returns its error
// unchanged and is recorded as failed; close flushes what was never sent.
#[test]
fn the_transport_times_the_send_and_flushes_on_close() {
    let (trace, captured) = captured_trace();
    let trace = Arc::new(trace);
    let mut transport = TracingTransport::new(
        SlowTransport {
            delay: Duration::from_millis(30),
            fail: false,
            closed: false,
            incoming: VecDeque::new(),
        },
        Arc::clone(&trace),
    );
    settle(&trace, 5, "note");
    settle(&trace, 6, "gate");
    settle(&trace, 7, "done");
    block_on(transport.send(response(5))).expect("send");
    assert_eq!(
        trace.pending_len(),
        2,
        "the answered call's record was written"
    );
    transport.inner.fail = true;
    let error = block_on(transport.send(response(7))).expect_err("the send fails");
    assert_eq!(error.kind(), std::io::ErrorKind::BrokenPipe);
    assert_eq!(error.to_string(), "pipe closed");
    block_on(transport.close()).expect("close");
    assert!(transport.inner.closed);
    assert_eq!(trace.pending_len(), 0, "close flushed the unsent record");
    let lines = written(&trace, &captured);
    assert_eq!(
        states(&lines),
        vec![
            (serde_json::json!(5), serde_json::json!("complete")),
            (serde_json::json!(7), serde_json::json!("send_failed")),
            (serde_json::json!(6), serde_json::json!("incomplete")),
        ]
    );
    assert!(
        lines[0]["wire_encode_send_inclusive_ms"]
            .as_f64()
            .expect("send time")
            >= 25.0
    );
}

fn client_message(value: serde_json::Value) -> RxJsonRpcMessage<RoleServer> {
    serde_json::from_value(value).expect("a client message")
}

// The transport passes client messages through unchanged and reads a client's
// cancel as rmcp's serve loop is about to handle it: a call settled but not
// yet answered, or one still running, is marked. A response sent all the same
// completes its record; one never sent reads cancelled at close. A request
// reusing an id clears an earlier cancel of it.
#[test]
fn the_transport_reads_a_client_cancel_on_receive() {
    let (trace, captured) = captured_trace();
    let trace = Arc::new(trace);
    let cancel = |id: i64| {
        client_message(serde_json::json!({
            "jsonrpc": "2.0",
            "method": "notifications/cancelled",
            "params": { "requestId": id, "reason": "client gave up" },
        }))
    };
    let request = |id: i64| {
        client_message(serde_json::json!({
            "jsonrpc": "2.0",
            "id": id,
            "method": "tools/call",
            "params": { "name": "note", "arguments": {} },
        }))
    };
    let sent_cancel = cancel(3);
    let mut transport = TracingTransport::new(
        SlowTransport {
            delay: Duration::ZERO,
            fail: false,
            closed: false,
            incoming: VecDeque::from([
                request(3),
                sent_cancel.clone(),
                cancel(4),
                request(5),
                cancel(5),
                request(5),
            ]),
        },
        Arc::clone(&trace),
    );
    let received = block_on(transport.receive()).expect("request 3");
    assert!(matches!(received, JsonRpcMessage::Request(_)));
    settle(&trace, 3, "note");
    let received = block_on(transport.receive()).expect("cancel 3");
    assert_eq!(
        serde_json::to_value(&received).expect("encode"),
        serde_json::to_value(&sent_cancel).expect("encode"),
        "the cancel passes through unchanged"
    );
    assert_eq!(trace.pending_len(), 1, "the settled call is only marked");
    block_on(transport.send(response(3))).expect("send");
    block_on(transport.receive()).expect("cancel 4");
    settle(&trace, 4, "gate");
    // A cancel for 5 is cleared by the next request with that id.
    for _ in 0..3 {
        block_on(transport.receive()).expect("message");
    }
    settle(&trace, 5, "done");
    assert_eq!(trace.pending_len(), 2, "4 and the new 5 wait");
    assert!(block_on(transport.receive()).is_none());
    block_on(transport.close()).expect("close");
    let lines = written(&trace, &captured);
    assert_eq!(
        states(&lines),
        vec![
            (serde_json::json!(3), serde_json::json!("complete")),
            (serde_json::json!(4), serde_json::json!("cancelled")),
            (serde_json::json!(5), serde_json::json!("incomplete")),
        ]
    );
    assert_eq!(lines[0]["cancel_requested"], true);
    assert_eq!(lines[2]["cancel_requested"], false);
}

/// A transport that says when its input ended: rmcp's serve loop breaks in
/// the same poll in which the end is seen.
struct EndSignal<T> {
    inner: T,
    ended: Arc<tokio::sync::Notify>,
}

impl<T: Transport<RoleServer>> Transport<RoleServer> for EndSignal<T> {
    type Error = T::Error;

    fn send(
        &mut self,
        item: TxJsonRpcMessage<RoleServer>,
    ) -> impl Future<Output = Result<(), Self::Error>> + Send + 'static {
        self.inner.send(item)
    }

    fn receive(&mut self) -> impl Future<Output = Option<RxJsonRpcMessage<RoleServer>>> + Send {
        let ended = Arc::clone(&self.ended);
        let receiving = self.inner.receive();
        async move {
            let message = receiving.await;
            if message.is_none() {
                ended.notify_one();
            }
            message
        }
    }

    fn close(&mut self) -> impl Future<Output = Result<(), Self::Error>> + Send {
        self.inner.close()
    }
}

/// A tool server whose one call waits for its client's cancel, says so,
/// waits for the input to end, and then settles into the trace as the MCP
/// server's traced call does.
struct HeldUntilEnd {
    trace: Arc<PhaseTrace>,
    cancel_seen: Arc<tokio::sync::Notify>,
    ended: Arc<tokio::sync::Notify>,
}

impl rmcp::ServerHandler for HeldUntilEnd {
    fn get_info(&self) -> rmcp::model::ServerInfo {
        rmcp::model::ServerInfo::new(
            rmcp::model::ServerCapabilities::builder()
                .enable_tools()
                .build(),
        )
    }

    async fn call_tool(
        &self,
        request: rmcp::model::CallToolRequestParams,
        context: rmcp::service::RequestContext<RoleServer>,
    ) -> Result<rmcp::model::CallToolResponse, rmcp::ErrorData> {
        context.ct.cancelled().await;
        self.cancel_seen.notify_one();
        self.ended.notified().await;
        self.trace.handler_settled(
            context.id.clone(),
            &request.name,
            Duration::from_millis(1),
            Phases::default(),
        );
        Ok(rmcp::model::CallToolResult::success(Vec::new()).into())
    }
}

async fn write_line(writer: &mut (impl tokio::io::AsyncWrite + Unpin), value: &serde_json::Value) {
    use tokio::io::AsyncWriteExt as _;
    let mut line = value.to_string();
    line.push('\n');
    writer
        .write_all(line.as_bytes())
        .await
        .expect("write to the server");
}

// Over rmcp's real serve loop: rmcp drains the responses still queued after
// its input ends straight to the transport, a cancelled call's included. A
// call its client cancelled, whose handler finishes during that drain, has
// its response delivered, so its record reads complete with the cancel noted,
// never cancelled beside an unmatched send.
#[test]
fn a_cancelled_call_drained_after_input_ends_reads_complete() {
    use rmcp::ServiceExt as _;
    use tokio::io::{AsyncBufReadExt as _, AsyncWriteExt as _};

    let (trace, captured) = captured_trace();
    let trace = Arc::new(trace);
    let ended = Arc::new(tokio::sync::Notify::new());
    let cancel_seen = Arc::new(tokio::sync::Notify::new());
    let handler = HeldUntilEnd {
        trace: Arc::clone(&trace),
        cancel_seen: Arc::clone(&cancel_seen),
        ended: Arc::clone(&ended),
    };
    let runtime = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .expect("runtime");
    let output = runtime.block_on(async {
        let (client, server) = tokio::io::duplex(1 << 16);
        let transport = rmcp::transport::IntoTransport::<
            RoleServer,
            std::io::Error,
            rmcp::transport::async_rw::TransportAdapterAsyncRW,
        >::into_transport(tokio::io::split(server));
        let transport = TracingTransport::new(
            EndSignal {
                inner: transport,
                ended,
            },
            Arc::clone(&trace),
        );
        let serving = tokio::spawn(async move {
            let running = handler.serve(transport).await.expect("serve");
            running.waiting().await.expect("serve loop")
        });
        let (reader, mut writer) = tokio::io::split(client);
        let mut lines = tokio::io::BufReader::new(reader).lines();
        let bound = Duration::from_secs(20);
        write_line(
            &mut writer,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": "2025-06-18",
                    "capabilities": {},
                    "clientInfo": { "name": "phase-trace-test", "version": "0" },
                },
            }),
        )
        .await;
        let initialized = tokio::time::timeout(bound, lines.next_line())
            .await
            .expect("initialize answered in time")
            .expect("read")
            .expect("an initialize response");
        assert!(initialized.contains("\"id\":1"), "{initialized}");
        write_line(
            &mut writer,
            &serde_json::json!({ "jsonrpc": "2.0", "method": "notifications/initialized" }),
        )
        .await;
        write_line(
            &mut writer,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "id": 2,
                "method": "tools/call",
                "params": { "name": "note", "arguments": {} },
            }),
        )
        .await;
        write_line(
            &mut writer,
            &serde_json::json!({
                "jsonrpc": "2.0",
                "method": "notifications/cancelled",
                "params": { "requestId": 2, "reason": "client gave up" },
            }),
        )
        .await;
        tokio::time::timeout(bound, cancel_seen.notified())
            .await
            .expect("rmcp handled the cancel in time");
        // The input ends; the handler finishes only once the serve loop saw it.
        writer.shutdown().await.expect("end the input");
        let mut output = Vec::new();
        while let Some(line) = tokio::time::timeout(bound, lines.next_line())
            .await
            .expect("the server finished in time")
            .expect("read")
        {
            output.push(line);
        }
        tokio::time::timeout(bound, serving)
            .await
            .expect("the serve loop ended in time")
            .expect("serve task");
        output
    });
    assert!(
        output.iter().any(|line| line.contains("\"id\":2")),
        "the drain delivered the cancelled call's response: {output:?}"
    );
    let lines = written(&trace, &captured);
    assert_eq!(
        states(&lines),
        vec![(serde_json::json!(2), serde_json::json!("complete"))]
    );
    assert_eq!(lines[0]["cancel_requested"], true);
    assert_eq!(lines[0]["unmatched_sends"], 1, "only initialize's");
}

fn verbs_on(database: &std::path::Path, session: &str) -> AgentVerbs {
    AgentVerbs::new(
        database.to_path_buf(),
        ProjectId("phase-trace-timeout".into()),
        "agent".into(),
        SessionId(session.into()),
        Some("phase-trace-test".into()),
    )
}

fn try_add(
    verbs: &AgentVerbs,
    title: &str,
    second: i64,
) -> Result<crate::verbs::Receipt, crate::verbs::VerbError> {
    verbs.add(
        AddInput {
            external: None,
            notes: Vec::new(),
            title: title.into(),
            outcome: None,
            acceptance: vec![format!("{title} is delivered")],
            bindings: Vec::new(),
            under: None,
            optional: false,
            priority: None,
            labels: Vec::new(),
            assignee: None,
            kind: None,
            evaluation_mode: None,
        },
        at(second),
    )
}

// The trace changes no timeout. A traced and an untraced write, run together
// against a writer that holds the lock past the store's busy timeout, wait
// the same bound and are refused the same way. The traced store is opened
// inside a traced call, as the MCP server's first call opens it, so its
// connection carries the profile; SQLite profiles a statement that ends in
// SQLITE_BUSY too, so the timed-out BEGIN IMMEDIATE shows its wait.
#[test]
fn a_busy_timeout_is_unchanged_by_the_trace() {
    let home = crate::test_support::temp_home().expect("temporary home");
    let database = home.path().join("engram.sqlite3");
    let traced_verbs = verbs_on(&database, "phase-trace-traced");
    let untraced_verbs = verbs_on(&database, "phase-trace-untraced");
    let (opened, warmed) = block_on(scoped(async {
        try_add(&traced_verbs, "Opens the traced store", 1)
    }));
    opened.expect("first traced write");
    // The positive control: the traced connection is profiled.
    assert_eq!(warmed.store_open.count, 1, "{warmed:?}");
    assert!(warmed.begin_immediate.count >= 1, "{warmed:?}");
    assert!(warmed.commit.count >= 1, "{warmed:?}");
    try_add(&untraced_verbs, "Opens the untraced store", 2).expect("first untraced write");
    let blocker = rusqlite::Connection::open(&database).expect("second connection");
    blocker
        .execute_batch("BEGIN IMMEDIATE")
        .expect("hold the write lock");
    let ((traced, phases, traced_elapsed), (untraced, untraced_elapsed)) =
        std::thread::scope(|scope| {
            let traced = scope.spawn(|| {
                let started = Instant::now();
                let (result, phases) = block_on(scoped(async {
                    try_add(&traced_verbs, "Times out traced", 3)
                }));
                (result, phases, started.elapsed())
            });
            let untraced = scope.spawn(|| {
                let started = Instant::now();
                let result = try_add(&untraced_verbs, "Times out untraced", 4);
                (result, started.elapsed())
            });
            (
                traced.join().expect("traced writer"),
                untraced.join().expect("untraced writer"),
            )
        });
    blocker
        .execute_batch("COMMIT")
        .expect("release the write lock");
    let traced = traced.expect_err("the traced write times out");
    let untraced = untraced.expect_err("the untraced write times out");
    assert_eq!(traced.error.to_string(), untraced.error.to_string());
    assert_eq!(
        format!("{:?}", traced.guidance()),
        format!("{:?}", untraced.guidance())
    );
    // Both waited the store's busy timeout, and about equally long.
    let bound = Duration::from_secs(4);
    assert!(traced_elapsed >= bound, "{traced_elapsed:?}");
    assert!(untraced_elapsed >= bound, "{untraced_elapsed:?}");
    let difference = traced_elapsed.abs_diff(untraced_elapsed);
    assert!(
        difference < Duration::from_secs(2),
        "traced {traced_elapsed:?}, untraced {untraced_elapsed:?}"
    );
    assert_eq!(
        phases.store_open.count, 0,
        "the cached connection: {phases:?}"
    );
    assert!(phases.begin_immediate.count >= 1, "{phases:?}");
    assert!(phases.begin_immediate.max >= bound, "{phases:?}");
    // Every connection closes before the home goes: locals drop in reverse
    // order, the blocker and both stores first, the home last.
}
