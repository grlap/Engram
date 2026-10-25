//! Bounded, conservative association of decoded calls and their send futures.
//!
//! rmcp returns only a wire ID at dispatch. An ambiguous ID therefore stays
//! quarantined; a handler finishing later cannot restore its association.

use super::{
    Counters, Duration, HandlerRecord, Instant, MAX_PENDING, MAX_RECORD_BYTES, NumberOrString,
    Outcome, PhaseTrace, Phases, TOOL_LABELS, millis, render,
};
use std::sync::Arc;

#[derive(Clone, Copy, Debug)]
pub(crate) struct Ticket {
    pub(super) id: Option<i64>,
    pub(super) sequence: Option<u64>,
    pub(super) received: Option<Duration>,
    pub(super) started: Duration,
    pub(super) loss: Option<&'static str>,
}

impl Ticket {
    fn unknown(id: &NumberOrString, now: Duration, reason: &'static str) -> Self {
        Self {
            id: number(id),
            sequence: None,
            received: None,
            started: now,
            loss: Some(reason),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Lifecycle {
    SingleGeneration,
    Unsafe,
}

#[derive(Debug)]
struct Entry {
    ticket: Ticket,
    // A client notification attributed to this generation, independently of
    // why correlation was lost. None means the generation was ambiguous.
    cancel_requested: Option<bool>,
    // Once an additional or unowned response is possible, dispatch cannot
    // prove that this key is safe to reuse. Displayed loss reasons may change.
    lifecycle: Lifecycle,
    tool_call: bool,
    started: bool,
    settled: bool,
    record: Option<HandlerRecord>,
}

/// One pool holds receive, handler, settled-summary and quarantine state.
/// Every retained key and field is fixed-size; no request string is cloned.
#[derive(Debug, Default)]
pub(super) struct Pending {
    entries: Vec<Entry>,
    sequence: u64,
    pub(super) disabled: bool,
    pub(super) closed: bool,
    pub(super) evicted: u64,
    pub(super) unmatched_sends: u64,
    pub(super) unassociated: u64,
}

impl Pending {
    #[cfg(test)]
    pub(super) fn observed(&self, id: &NumberOrString) -> bool {
        number(id).is_some()
            && self
                .entries
                .iter()
                .any(|entry| entry.ticket.id == number(id))
    }

    #[cfg(test)]
    pub(super) fn retained(&self) -> usize {
        self.entries.len()
    }

    #[cfg(test)]
    pub(super) fn exhaust_sequence(&mut self) {
        self.sequence = u64::MAX;
    }

    #[cfg(test)]
    pub(super) fn records(&self) -> usize {
        self.entries
            .iter()
            .filter(|entry| entry.record.is_some())
            .count()
    }

    fn reason(&self, id: &NumberOrString) -> &'static str {
        if self.closed {
            "closed"
        } else if self.disabled {
            "saturated"
        } else if number(id).is_none() {
            "string_id"
        } else {
            self.entries
                .iter()
                .find(|entry| entry.ticket.id == number(id))
                .and_then(|entry| entry.ticket.loss)
                .unwrap_or("missing_receive")
        }
    }

    fn disable(&mut self) {
        self.disabled = true;
        for entry in &mut self.entries {
            entry.ticket.loss = Some("saturated");
            if let Some(record) = &mut entry.record {
                record.ticket.loss = Some("saturated");
            }
        }
    }

    fn quarantine(&mut self, id: i64, reason: &'static str, now: Duration) {
        if let Some(entry) = self
            .entries
            .iter_mut()
            .find(|entry| entry.ticket.id == Some(id))
        {
            if reason != "cancel_requested" {
                entry.lifecycle = Lifecycle::Unsafe;
            }
            entry.ticket.loss = Some(reason);
            if let Some(record) = &mut entry.record {
                record.ticket.loss = Some(reason);
            }
        } else if self.entries.len() < MAX_PENDING && !self.disabled && !self.closed {
            self.entries.push(Entry {
                ticket: Ticket::unknown(&NumberOrString::Number(id), now, reason),
                cancel_requested: None,
                lifecycle: Lifecycle::Unsafe,
                tool_call: true,
                started: false,
                settled: false,
                record: None,
            });
        } else if !self.closed {
            // Forgetting this tombstone could let an old response claim a new
            // call. Sacrifice association for this process instead.
            self.disable();
        }
    }
}

fn number(id: &NumberOrString) -> Option<i64> {
    match id {
        NumberOrString::Number(id) => Some(*id),
        NumberOrString::String(_) => None,
    }
}

fn label(tool: &str) -> &'static str {
    TOOL_LABELS
        .iter()
        .copied()
        .find(|name| *name == tool)
        .unwrap_or("unknown")
}

fn stage(trace: &PhaseTrace, ticket: Ticket, tool: &str, stage: &'static str, now: Duration) {
    stage_result(trace, ticket, tool, stage, now, None);
}

fn stage_result(
    trace: &PhaseTrace,
    ticket: Ticket,
    tool: &str,
    stage: &'static str,
    now: Duration,
    delivered: Option<bool>,
) {
    let line = serde_json::json!({
        "engram_mcp_call_stage": 1,
        "trace_instance": trace.instance,
        "id": ticket.id,
        "call_seq": if ticket.loss.is_none() { ticket.sequence } else { None },
        "correlation": if ticket.loss.is_none() { "numeric" } else { "unavailable" },
        "uncorrelated_reason": ticket.loss,
        "tool": label(tool),
        "stage": stage,
        "send_succeeded": delivered,
        "server_elapsed_ms": millis(now),
        "dropped_lines": trace.lines.dropped(),
    })
    .to_string();
    if line.len() <= MAX_RECORD_BYTES {
        trace.emit(line);
    }
}

pub(super) fn received(trace: &PhaseTrace, id: &NumberOrString, tool: &str) {
    received_kind(trace, id, tool, true);
}

pub(super) fn metadata_received(trace: &PhaseTrace, id: &NumberOrString) {
    received_kind(trace, id, "unknown", false);
}

fn received_kind(trace: &PhaseTrace, id: &NumberOrString, tool: &str, tool_call: bool) {
    let now = trace.started.elapsed();
    let ticket = {
        let Ok(mut pending) = trace.pending.lock() else {
            return;
        };
        let mut ticket = Ticket::unknown(id, now, pending.reason(id));
        if let Some(id) = number(id)
            && !pending.disabled
            && !pending.closed
        {
            if pending
                .entries
                .iter()
                .any(|entry| entry.ticket.id == Some(id))
            {
                pending.quarantine(id, "ambiguous_id", now);
                ticket.loss = Some(pending.reason(&NumberOrString::Number(id)));
            } else if pending.entries.len() >= MAX_PENDING || pending.sequence == u64::MAX {
                pending.disable();
                ticket.loss = Some("saturated");
            } else {
                pending.sequence += 1;
                ticket = Ticket {
                    id: Some(id),
                    sequence: Some(pending.sequence),
                    received: Some(now),
                    started: now,
                    loss: None,
                };
                pending.entries.push(Entry {
                    ticket,
                    cancel_requested: Some(false),
                    lifecycle: Lifecycle::SingleGeneration,
                    tool_call,
                    started: false,
                    settled: false,
                    record: None,
                });
            }
        }
        if ticket.loss.is_some() {
            pending.unassociated = pending.unassociated.saturating_add(1);
        }
        ticket
    };
    stage(trace, ticket, tool, "receive", now);
}

pub(super) fn started(
    trace: &PhaseTrace,
    id: &NumberOrString,
    tool: &str,
    cancelled: impl FnOnce() -> bool,
) -> Ticket {
    let now = trace.started.elapsed();
    let ticket = {
        let Ok(mut pending) = trace.pending.lock() else {
            return Ticket::unknown(id, now, "tracking_unavailable");
        };
        // In particular, an old context cancelled before its first poll must
        // not borrow a replacement request's receive slot.
        if cancelled()
            && let Some(id) = number(id)
        {
            pending.quarantine(id, "token_cancelled_before_start", now);
        }
        let mut ticket = Ticket::unknown(id, now, pending.reason(id));
        if !pending.disabled
            && !pending.closed
            && let Some(id) = number(id)
        {
            if let Some(entry) = pending
                .entries
                .iter_mut()
                .find(|entry| entry.ticket.id == Some(id))
            {
                if entry.ticket.loss.is_none()
                    && entry.lifecycle == Lifecycle::SingleGeneration
                    && entry.tool_call
                    && !entry.started
                {
                    entry.started = true;
                    entry.ticket.started = now;
                    ticket = entry.ticket;
                } else {
                    // A second invocation can still answer under this ID,
                    // including after a client cancellation supplied a loss.
                    entry.lifecycle = Lifecycle::Unsafe;
                    if entry.ticket.loss.is_none() {
                        entry.ticket.loss = Some("ambiguous_id");
                        if let Some(record) = &mut entry.record {
                            record.ticket.loss = Some("ambiguous_id");
                        }
                        ticket.loss = Some("ambiguous_id");
                    }
                }
            } else {
                // This handler may still produce a response. Keep its ID
                // unavailable so that response cannot claim a later call.
                pending.quarantine(id, "missing_receive", now);
                ticket.loss = Some(pending.reason(&NumberOrString::Number(id)));
            }
        }
        if ticket.loss.is_some() {
            pending.unassociated = pending.unassociated.saturating_add(1);
        }
        ticket
    };
    stage(trace, ticket, tool, "handler_start", now);
    ticket
}

pub(super) fn settled(
    trace: &PhaseTrace,
    mut ticket: Ticket,
    tool: &str,
    handler: Duration,
    phases: &Phases,
) {
    let now = trace.started.elapsed();
    let line = {
        let Ok(mut pending) = trace.pending.lock() else {
            return;
        };
        let mut record = HandlerRecord {
            ticket,
            tool: label(tool),
            handler,
            phases: *phases,
            cancel_requested: None,
            dispatch_send: None,
        };
        let index = ticket.sequence.and_then(|sequence| {
            pending
                .entries
                .iter()
                .position(|entry| entry.ticket.sequence == Some(sequence))
        });
        if let Some(index) = index {
            ticket.loss = pending.entries[index].ticket.loss;
            record.cancel_requested = pending.entries[index].cancel_requested;
            // A cancelled handler can emit its summary now, but its unique
            // response may still be dispatched later (notably during drain).
            pending.entries[index].settled = true;
        } else if ticket.loss.is_none() {
            ticket.loss = Some(if pending.closed {
                "closed"
            } else {
                "tracking_unavailable"
            });
        }
        record.ticket = ticket;
        if ticket.loss.is_none() && !pending.closed && !pending.disabled {
            if let Some(index) = index {
                pending.entries[index].record = Some(record);
            }
            None
        } else {
            pending.unassociated = pending.unassociated.saturating_add(1);
            Some(render(
                &record,
                Outcome::Uncorrelated,
                None,
                trace.counters(&pending),
            ))
        }
    };
    stage(trace, ticket, tool, "handler_settle", now);
    if let Some(line) = line {
        trace.emit(line);
    }
}

pub(super) fn cancelled(trace: &PhaseTrace, id: &NumberOrString) {
    let now = trace.started.elapsed();
    let ticket = {
        let Ok(mut pending) = trace.pending.lock() else {
            return;
        };
        if let Some(id) = number(id)
            && let Some(entry) = pending
                .entries
                .iter_mut()
                .find(|entry| entry.ticket.id == Some(id))
        {
            // A notification after a duplicate cannot identify a generation.
            // Never erase a notification already attributed before that loss.
            if entry.cancel_requested != Some(true) {
                entry.cancel_requested = (entry.ticket.loss.is_none()
                    && entry.lifecycle == Lifecycle::SingleGeneration)
                    .then_some(true);
            }
            if let Some(record) = &mut entry.record {
                record.cancel_requested = entry.cancel_requested;
            }
            pending.quarantine(id, "cancel_requested", now);
        }
        // Unknown or already-dispatched IDs have no outstanding observed call
        // to mark. They cannot contribute a response that steals a later slot.
        Ticket::unknown(id, now, pending.reason(id))
    };
    stage(trace, ticket, "unknown", "cancel_requested", now);
}

/// Removed from shared state at dispatch. Completion never looks up a wire ID.
pub(super) struct Dispatched {
    record: Option<HandlerRecord>,
    ticket: Ticket,
    tool: &'static str,
    dispatch: Instant,
}

pub(super) fn dispatching(trace: &PhaseTrace, id: &NumberOrString) -> Dispatched {
    let dispatch = Instant::now();
    let now = dispatch.saturating_duration_since(trace.started);
    let (data, retired_line) = {
        let mut ticket = Ticket::unknown(id, now, "tracking_unavailable");
        let mut record = None;
        let mut retired = None;
        let mut retired_line = None;
        if let Ok(mut pending) = trace.pending.lock() {
            ticket.loss = Some(pending.reason(id));
            if !pending.disabled && !pending.closed && number(id).is_some() {
                if let Some(index) = pending.entries.iter().position(|entry| {
                    entry.ticket.id == number(id)
                        && entry.lifecycle == Lifecycle::SingleGeneration
                        && ((entry.ticket.loss.is_none()
                            && (!entry.tool_call || (entry.settled && entry.record.is_some())))
                            || (entry.ticket.sequence.is_some()
                                && entry.tool_call
                                && entry.started
                                && entry.settled
                                && entry.cancel_requested == Some(true)))
                }) {
                    let entry = pending.entries.swap_remove(index);
                    ticket = entry.ticket;
                    if ticket.loss.is_some() {
                        // Dispatch proves this generation's only response is
                        // owned by a send attempt, not that it was delivered.
                        retired = entry.record;
                    } else {
                        record = entry.record;
                    }
                } else if let Some(id) = number(id) {
                    // Dispatch cannot clear unsafe quarantine or replace its
                    // latest observed loss reason just by seeing a response.
                    let quarantined = pending
                        .entries
                        .iter_mut()
                        .find(|entry| entry.ticket.id == Some(id))
                        .is_some_and(|entry| {
                            entry.lifecycle = Lifecycle::Unsafe;
                            entry.ticket.loss.is_some()
                        });
                    if !quarantined {
                        pending.quarantine(id, "unmatched_send", now);
                    }
                    ticket.loss = Some(pending.reason(&NumberOrString::Number(id)));
                }
            }
            if record.is_none() {
                pending.unmatched_sends = pending.unmatched_sends.saturating_add(1);
            }
            retired_line = retired.as_ref().map(|record| {
                render(
                    record,
                    Outcome::Uncorrelated,
                    None,
                    trace.counters(&pending),
                )
            });
        }
        let tool = record
            .as_ref()
            .or(retired.as_ref())
            .map_or("unknown", |record| record.tool);
        (
            Dispatched {
                record,
                ticket,
                tool,
                dispatch,
            },
            retired_line,
        )
    };
    stage(trace, data.ticket, data.tool, "send_dispatch", now);
    if let Some(line) = retired_line {
        trace.emit(line);
    }
    data
}

pub(super) fn ended(
    trace: &PhaseTrace,
    mut data: Dispatched,
    polled: Option<Instant>,
    delivered: Option<bool>,
) {
    let now = Instant::now();
    stage_result(
        trace,
        data.ticket,
        data.tool,
        if delivered.is_some() {
            "send_finish"
        } else {
            "send_dropped"
        },
        now.saturating_duration_since(trace.started),
        delivered,
    );
    if let Some(mut record) = data.record.take() {
        record.dispatch_send = Some(now.saturating_duration_since(data.dispatch));
        let outcome = match delivered {
            Some(true) => Outcome::Complete,
            Some(false) => Outcome::SendFailed,
            None => Outcome::SendDropped,
        };
        let counters = trace
            .pending
            .lock()
            .ok()
            .map_or_else(Counters::default, |pending| trace.counters(&pending));
        trace.emit(render(
            &record,
            outcome,
            delivered
                .and(polled)
                .map(|start| now.saturating_duration_since(start)),
            counters,
        ));
    }
}

/// This guard must exist before inner.send, including before its first poll.
pub(super) struct Sending {
    trace: Arc<PhaseTrace>,
    data: Option<Dispatched>,
    polled: Option<Instant>,
}

impl Sending {
    pub(super) fn new(trace: Arc<PhaseTrace>, data: Dispatched) -> Self {
        Self {
            trace,
            data: Some(data),
            polled: None,
        }
    }

    pub(super) fn polled(&mut self) {
        self.polled = Some(Instant::now());
    }

    pub(super) fn finished(mut self, delivered: bool) {
        if let Some(data) = self.data.take() {
            ended(&self.trace, data, self.polled, Some(delivered));
        }
    }
}

impl Drop for Sending {
    fn drop(&mut self) {
        if let Some(data) = self.data.take() {
            ended(&self.trace, data, self.polled, None);
        }
    }
}

pub(super) fn closed(trace: &PhaseTrace) {
    let lines = {
        let Ok(mut pending) = trace.pending.lock() else {
            return;
        };
        pending.closed = true;
        let counters = trace.counters(&pending);
        pending
            .entries
            .drain(..)
            .filter_map(|entry| {
                entry.record.map(|mut record| {
                    record.ticket.loss = entry.ticket.loss;
                    let outcome = if record.ticket.loss.is_some() {
                        Outcome::Uncorrelated
                    } else {
                        Outcome::Incomplete
                    };
                    render(&record, outcome, None, counters)
                })
            })
            .collect::<Vec<_>>()
    };
    for line in lines {
        trace.emit(line);
    }
}
