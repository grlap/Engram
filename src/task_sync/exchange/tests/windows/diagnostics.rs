//! Diagnostics for explicitly owned test children, never production telemetry.

use super::{Cancellation, RunError, RunLimits, RunOutput, process};
use std::{cell::Cell, fmt::Write as _, io::Write, process::Command, time::Instant};

const PREFIX: &str = "exchange-launch ";
const MAX_LAUNCHES: usize = 64;
const LABELS: &[&str] = &[
    "version",
    "remote-fetch",
    "remote-push",
    "remote-mutation",
    "rewrites",
    "bundle-uri",
    "push-options",
    "helpers",
    "config",
    "init",
    "fixture",
    "other",
];

pub(super) fn label(command: &Command) -> &'static str {
    let has = |value: &str| command.get_args().any(|arg| arg == value);
    if has("--exact") {
        "fixture"
    } else if has("--version") {
        "version"
    } else if has("remote") && has("get-url") {
        if has("--push") {
            "remote-push"
        } else {
            "remote-fetch"
        }
    } else if has("remote") {
        "remote-mutation"
    } else if has("config") {
        if has("^url\\.") {
            "rewrites"
        } else if has("fetch.bundleURI") {
            "bundle-uri"
        } else if has("push.pushOption") {
            "push-options"
        } else if has("^credential\\.(.+\\.)?helper$") {
            "helpers"
        } else {
            "config"
        }
    } else if has("init") {
        "init"
    } else {
        "other"
    }
}

#[derive(Clone, Copy)]
struct State {
    started: Instant,
    next: usize,
}
std::thread_local! { static STATE: Cell<Option<State>> = const { Cell::new(None) }; }

pub(super) struct Scope;
pub(super) fn begin() -> Scope {
    STATE.with(|slot| {
        assert!(slot.get().is_none(), "one trace per owned child");
        slot.set(Some(State {
            started: Instant::now(),
            next: 1,
        }));
    });
    Scope
}
impl Drop for Scope {
    fn drop(&mut self) {
        STATE.with(|slot| slot.set(None));
    }
}

struct Started {
    id: usize,
    kind: &'static str,
    label: &'static str,
    began_at: Instant,
}
pub(super) struct Launch(Option<Started>);

fn emit(record: &str) {
    // One locked write, with a leading newline for libtest's unfinished prefix.
    let mut stdout = std::io::stdout().lock();
    writeln!(stdout, "\n{PREFIX}{record}").unwrap();
    stdout.flush().unwrap();
}

fn start(kind: &'static str, label: &'static str) -> Launch {
    let start = STATE.with(|slot| {
        let mut state = slot.get()?;
        let id = state.next;
        state.next += 1;
        slot.set(Some(state));
        Some((id, state.started.elapsed().as_micros()))
    });
    let Some((id, since)) = start else {
        return Launch(None);
    };
    assert!(
        id <= MAX_LAUNCHES,
        "owned diagnostic launch ceiling exceeded"
    );
    emit(&format!("begin {id} {kind} {label} {since}"));
    Launch(Some(Started {
        id,
        kind,
        label,
        began_at: Instant::now(),
    }))
}
pub(super) fn product(label: &'static str) -> Launch {
    start("product", label)
}
pub(super) fn direct(command: &Command) -> Launch {
    start("direct", label(command))
}

impl Launch {
    pub(super) fn finish(mut self, error: Option<&RunError>) {
        let outcome = match error {
            None => "ok",
            Some(RunError::Deadline) => "deadline",
            Some(RunError::Cancelled) => "cancelled",
            Some(RunError::OutputLimit) => "output-limit",
            Some(RunError::Spawn) => "spawn",
            Some(RunError::Process) => "process",
            Some(RunError::Git) => "git",
            Some(_) => "refused",
        };
        self.end(outcome);
    }
    fn end(&mut self, outcome: &str) {
        if let Some(start) = self.0.take() {
            emit(&format!(
                "end {} {} {} {} {outcome}",
                start.id,
                start.kind,
                start.label,
                start.began_at.elapsed().as_micros()
            ));
        }
    }
}
impl Drop for Launch {
    fn drop(&mut self) {
        self.end("dropped");
    }
}

#[derive(Debug)]
pub(super) struct Entry {
    pub(super) id: usize,
    pub(super) kind: String,
    pub(super) label: String,
    pub(super) since_us: u128,
    pub(super) end: Option<(u128, String)>,
}
#[derive(Debug, Default)]
pub(super) struct Trace {
    pub(super) entries: Vec<Entry>,
    pub(super) incomplete: bool,
}
pub(super) fn parse(stdout: &[u8]) -> Trace {
    let mut trace = Trace::default();
    for raw_line in stdout.split_inclusive(|byte| *byte == b'\n') {
        let line = raw_line.strip_suffix(b"\n").unwrap_or(raw_line);
        let line = line.strip_suffix(b"\r").unwrap_or(line);
        if !raw_line.ends_with(b"\n") && !line.is_empty() {
            trace.incomplete = true;
        }
        if !line.starts_with(PREFIX.as_bytes()) {
            continue;
        }
        if !raw_line.ends_with(b"\n") || !line.is_ascii() {
            trace.incomplete = true;
            break;
        }
        let fields: Vec<_> = std::str::from_utf8(line)
            .unwrap()
            .split_ascii_whitespace()
            .collect();
        let valid = (|| {
            let id: usize = fields.get(2)?.parse().ok()?;
            let kind = *fields.get(3)?;
            let label = *fields.get(4)?;
            let micros: u128 = fields.get(5)?.parse().ok()?;
            if !matches!(kind, "direct" | "product") || !LABELS.contains(&label) {
                return None;
            }
            match (fields.get(1).copied(), fields.len()) {
                (Some("begin"), 6) if id == trace.entries.len() + 1 && id <= MAX_LAUNCHES => {
                    if trace
                        .entries
                        .last()
                        .is_some_and(|entry| entry.end.is_none())
                    {
                        return None;
                    }
                    trace.entries.push(Entry {
                        id,
                        kind: kind.into(),
                        label: label.into(),
                        since_us: micros,
                        end: None,
                    });
                }
                (Some("end"), 7) => {
                    let entry = trace.entries.last_mut()?;
                    if entry.id != id
                        || entry.kind != kind
                        || entry.label != label
                        || entry.end.is_some()
                        || !matches!(
                            fields[6],
                            "ok" | "deadline"
                                | "cancelled"
                                | "output-limit"
                                | "spawn"
                                | "process"
                                | "git"
                                | "refused"
                                | "dropped"
                        )
                    {
                        return None;
                    }
                    entry.end = Some((micros, fields[6].into()));
                }
                _ => return None,
            }
            Some(())
        })();
        if valid.is_none() {
            trace.incomplete = true;
            break;
        }
    }
    trace
}

pub(super) fn assert_complete_trace(stdout: &[u8], direct: usize, product: usize) {
    let trace = parse(stdout);
    assert!(!trace.incomplete, "{trace:?}");
    assert_eq!(trace.entries.len(), direct + product, "{trace:?}");
    assert_eq!(
        trace
            .entries
            .iter()
            .filter(|entry| entry.kind == "direct")
            .count(),
        direct,
        "{trace:?}"
    );
    assert!(
        trace.entries.iter().all(|entry| entry.end.is_some()),
        "{trace:?}"
    );
}

pub(super) fn report(output: &process::DiagnosticOutput) -> String {
    let trace = parse(&output.stdout);
    let mut text = format!(
        "owned fixture result={:?}; trace_incomplete={}\n",
        output.result, trace.incomplete
    );
    for entry in &trace.entries {
        match &entry.end {
            Some((micros, status)) => writeln!(
                text,
                "completed launch={} kind={} label={} elapsed_us={} status={}",
                entry.id, entry.kind, entry.label, micros, status
            )
            .unwrap(),
            None if trace.incomplete => writeln!(
                text,
                "last captured unmatched BEGIN launch={} kind={} label={} began_at_group_us={}",
                entry.id, entry.kind, entry.label, entry.since_us
            )
            .unwrap(),
            None => writeln!(
                text,
                "active launch={} kind={} label={} began_at_group_us={}; last captured open BEGIN, END may be unread",
                entry.id, entry.kind, entry.label, entry.since_us
            )
            .unwrap(),
        }
    }
    if trace.incomplete {
        text.push_str(
            "active launch unknown: trace incomplete; captured records may omit later activity\n",
        );
    } else if trace.entries.last().is_none_or(|entry| entry.end.is_some()) {
        text.push_str(
            "active launch unknown: no unmatched BEGIN was captured; a later BEGIN may be unread\n",
        );
    }
    text.push_str("captured stdout:\n");
    text.push_str(&String::from_utf8_lossy(&output.stdout));
    text.push_str("\ncaptured stderr:\n");
    text.push_str(&String::from_utf8_lossy(&output.stderr));
    text
}

pub(super) async fn supervise(
    command: Command,
    limits: RunLimits,
    cancel: &Cancellation,
) -> Result<RunOutput, String> {
    let output = process::execute_diagnostic(command, limits, cancel, None).await;
    if output.result.is_err() {
        return Err(report(&output));
    }
    Ok(RunOutput {
        stdout: output.stdout,
        stderr_bytes: output.stderr.len(),
    })
}
