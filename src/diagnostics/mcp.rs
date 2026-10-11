//! Fixed-size observation of decoded messages; the transport passes bytes unchanged.

mod transport;
use super::{
    Correlation, CorrelationLoss, Diagnostics, Event, Fault, Fingerprint,
    record::{label, millis},
};
use rmcp::model::{NumberOrString, RequestId};
use std::{
    cell::Cell,
    future::Future,
    sync::{Arc, Mutex},
    time::{Duration, Instant},
};
pub use transport::DiagnosticTransport;

tokio::task_local! { static FAULT: Cell<Option<Fault>>; }

/// Captures typed errors before their display text or tool result is rendered.
pub(crate) fn typed_fault(fault: Fault) {
    let _ = FAULT.try_with(|slot| slot.set(Some(fault)));
}

#[cfg(test)]
mod tests;

pub(crate) async fn scoped<T>(future: impl Future<Output = T>) -> (T, Option<Fault>) {
    FAULT
        .scope(Cell::new(None), async {
            let result = future.await;
            (result, FAULT.with(Cell::get))
        })
        .await
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
enum Key {
    Number(i64),
    String(Fingerprint),
}
impl Key {
    fn of(id: &RequestId) -> Self {
        match id {
            NumberOrString::Number(value) => Self::Number(*value),
            NumberOrString::String(value) => Self::String(Fingerprint::request(value)),
        }
    }
    fn number(self) -> Option<i64> {
        match self {
            Self::Number(value) => Some(value),
            Self::String(_) => None,
        }
    }
}

#[derive(Clone, Copy, Debug)]
struct Pending {
    key: Key,
    sequence: u64,
    operation: &'static str,
    received: Instant,
    started: Option<Instant>,
    settled: Option<Instant>,
    fault: Option<Fault>,
    uncorrelated: Option<CorrelationLoss>,
}
impl Pending {
    fn event(self, now: Instant, fault: Fault) -> Event {
        let start = self.started.unwrap_or(self.received);
        let mut event = Event::failure(
            self.operation,
            Correlation {
                request_id: self.key.number(),
                request_seq: self.uncorrelated.is_none().then_some(self.sequence),
                uncorrelated_reason: self.uncorrelated,
                ..Correlation::default()
            },
            now.saturating_duration_since(start),
            fault,
        );
        event.handler_ms = self
            .settled
            .map(|end| millis(end.saturating_duration_since(start)));
        event.dispatch_wait_ms = self
            .settled
            .map(|end| millis(now.saturating_duration_since(end)));
        event
    }
}

#[derive(Debug)]
struct State {
    pending: Box<[Option<Pending>]>,
    active: usize,
    quarantine: Box<[Option<Key>]>,
    quarantined: usize,
    quarantine_overflow: bool,
    sequence: u64,
    eof: Option<Instant>,
    stage: &'static str,
    overrun: bool,
    closed: bool,
}

impl State {
    fn uncorrelated(&self, key: Key) -> Option<CorrelationLoss> {
        if self.quarantine_overflow {
            Some(CorrelationLoss::QuarantineOverflow)
        } else if self.quarantine[..self.quarantined].contains(&Some(key)) {
            Some(CorrelationLoss::AmbiguousId)
        } else {
            None
        }
    }

    // rmcp does not expose whether it dropped a cancelled response. Never
    // restore precision for this key merely because its handler settled.
    fn quarantine(&mut self, key: Key) -> bool {
        if self.uncorrelated(key).is_some() {
            return false;
        }
        if self.quarantined < self.quarantine.len() {
            self.quarantine[self.quarantined] = Some(key);
            self.quarantined += 1;
            false
        } else {
            self.quarantine_overflow = true;
            true
        }
    }

    fn take(&mut self, index: usize) -> Option<Pending> {
        let item = self.pending[index].take()?;
        self.active -= 1;
        self.pending[index] = self.pending[self.active].take();
        Some(item)
    }
}

/// Shared front-door timing, with no request body or arbitrary identity retained.
#[derive(Debug)]
pub struct Observer {
    sink: Diagnostics,
    state: Mutex<State>,
    slow: Duration,
    shutdown: Duration,
}

/// A handler's local ordinal prevents a late completion overwriting a reused ID.
#[derive(Clone, Copy, Debug)]
pub struct Ticket(Option<u64>, Pending);

impl Observer {
    pub fn new(sink: Diagnostics) -> Arc<Self> {
        Self::thresholds(sink, super::SLOW_CALL, super::SHUTDOWN_OVERRUN)
    }

    fn thresholds(sink: Diagnostics, slow: Duration, shutdown: Duration) -> Arc<Self> {
        Arc::new(Self {
            sink,
            state: Mutex::new(State {
                pending: vec![None; super::QUEUE_CAPACITY].into_boxed_slice(),
                active: 0,
                quarantine: vec![None; super::QUEUE_CAPACITY].into_boxed_slice(),
                quarantined: 0,
                quarantine_overflow: false,
                sequence: 0,
                eof: None,
                stage: "running",
                overrun: false,
                closed: false,
            }),
            slow,
            shutdown,
        })
    }

    fn requested(&self, id: &RequestId, operation: &str, now: Instant) {
        let Ok(mut state) = self.state.lock() else {
            self.sink.tracking_lost();
            return;
        };
        let key = Key::of(id);
        state.sequence = state.sequence.saturating_add(1);
        let sequence = state.sequence;
        if state.uncorrelated(key).is_some() {
            self.sink.tracking_lost();
            return;
        }
        // Either handler or either response may arrive first. Neither may
        // borrow this receive generation once a duplicate has been observed.
        if let Some(index) = state.pending[..state.active]
            .iter()
            .position(|slot| slot.is_some_and(|item| item.key == key))
        {
            let _ = state.take(index);
            self.quarantine_on(&mut state, key);
            self.sink.tracking_lost();
            self.sink.tracking_lost();
            return;
        }
        if state.active < state.pending.len() {
            let index = state.active;
            state.pending[index] = Some(Pending {
                key,
                sequence,
                operation: label(operation),
                received: now,
                started: None,
                settled: None,
                fault: None,
                uncorrelated: None,
            });
            state.active += 1;
        } else {
            self.quarantine_on(&mut state, key);
            self.sink.tracking_lost();
        }
    }

    pub fn started(
        &self,
        context: &rmcp::service::RequestContext<rmcp::RoleServer>,
        operation: &str,
    ) -> Ticket {
        self.starting(&context.id, operation, || context.ct.is_cancelled())
    }

    fn starting(
        &self,
        id: &RequestId,
        operation: &str,
        cancelled: impl FnOnce() -> bool,
    ) -> Ticket {
        let key = Key::of(id);
        let mut fallback = Pending {
            key,
            sequence: 0,
            operation: label(operation),
            received: Instant::now(),
            started: Some(Instant::now()),
            settled: None,
            fault: None,
            uncorrelated: Some(CorrelationLoss::TrackingUnavailable),
        };
        let Ok(mut state) = self.state.lock() else {
            return Ticket(None, fallback);
        };
        // rmcp cancels the original context before receiving a replacement
        // request. Check under the slot lock: an old handler must not claim
        // that replacement even if cancellation preceded its first poll.
        if cancelled() {
            fallback.uncorrelated = Some(CorrelationLoss::AmbiguousId);
            return Ticket(None, fallback);
        }
        if let Some(reason) = state.uncorrelated(key) {
            fallback.uncorrelated = Some(reason);
            return Ticket(None, fallback);
        }
        let active = state.active;
        let item = state.pending[..active]
            .iter_mut()
            .flatten()
            .find(|item| item.key == key && item.started.is_none());
        if let Some(item) = item {
            item.started = Some(Instant::now());
            item.operation = label(operation);
            Ticket(Some(item.sequence), *item)
        } else {
            Ticket(None, fallback)
        }
    }

    pub fn settled(&self, ticket: Ticket, fault: Option<Fault>) {
        if ticket.0.is_none() {
            let now = Instant::now();
            if let Some(fault) = fault.or_else(|| {
                (now.saturating_duration_since(ticket.1.received) > self.slow)
                    .then_some(Fault::slow())
            }) {
                self.sink.emit(ticket.1.event(now, fault));
            }
            return;
        }
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        let active = state.active;
        if let Some(item) = state.pending[..active]
            .iter_mut()
            .flatten()
            .find(|item| Some(item.sequence) == ticket.0)
        {
            let now = Instant::now();
            item.settled = Some(now);
            item.fault = fault;
            if let Some(fault) = fault {
                self.sink.emit(item.event(now, fault));
            }
        } else if let Some(fault) = fault {
            // Cancellation retires the wire-ID slot immediately. The handler
            // ticket retains its original ordinal and timing for a late error.
            let mut item = ticket.1;
            item.settled = Some(Instant::now());
            self.sink.emit(item.event(Instant::now(), fault));
        }
    }

    fn dispatching(&self, id: &RequestId, rpc_error: bool) -> Option<Pending> {
        let key = Key::of(id);
        let Ok(mut state) = self.state.lock() else {
            return None;
        };
        if state.uncorrelated(key).is_some() {
            return None;
        }
        let index = state.pending[..state.active]
            .iter()
            .position(|slot| slot.is_some_and(|item| item.key == key))?;
        let item = state.take(index)?;
        if rpc_error && item.fault.is_none() {
            self.sink
                .emit(item.event(Instant::now(), Fault::rpc_error()));
        }
        Some(item)
    }

    fn sent(&self, item: Pending, dispatch: Instant, end: Instant, failed: bool) {
        let start = item.started.unwrap_or(item.received);
        if failed || end.saturating_duration_since(start) > self.slow {
            let mut event = item.event(
                end,
                if failed {
                    Fault::send_failed()
                } else {
                    Fault::slow()
                },
            );
            event.dispatch_wait_ms = item
                .settled
                .map(|time| millis(dispatch.saturating_duration_since(time)));
            event.send_ms = Some(millis(end.saturating_duration_since(dispatch)));
            self.sink.emit(event);
        }
    }

    fn cancelled(&self, id: &RequestId) {
        let key = Key::of(id);
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        if let Some(index) = state.pending[..state.active]
            .iter()
            .position(|slot| slot.is_some_and(|item| item.key == key))
            && let Some(item) = state.take(index)
        {
            self.sink
                .emit(item.event(Instant::now(), Fault::cancelled()));
        } else {
            self.sink.emit(Event::failure(
                "unknown",
                Correlation {
                    request_id: key.number(),
                    uncorrelated_reason: Some(
                        state
                            .uncorrelated(key)
                            .unwrap_or(CorrelationLoss::TrackingUnavailable),
                    ),
                    ..Correlation::default()
                },
                Duration::ZERO,
                Fault::cancelled(),
            ));
        }
        self.quarantine_on(&mut state, key);
    }

    fn quarantine_on(&self, state: &mut State, key: Key) {
        if state.quarantine(key) {
            self.sink.quarantine_overflow();
            // Future same-ID requests cannot be distinguished from any
            // existing slot once the fixed quarantine is full.
            for slot in &mut state.pending {
                if slot.take().is_some() {
                    self.sink.tracking_lost();
                }
            }
            state.active = 0;
        }
    }

    fn untracked(&self, id: &RequestId) -> Correlation {
        let key = Key::of(id);
        Correlation {
            request_id: key.number(),
            uncorrelated_reason: Some(
                self.state
                    .lock()
                    .ok()
                    .and_then(|state| state.uncorrelated(key))
                    .unwrap_or(CorrelationLoss::TrackingUnavailable),
            ),
            ..Correlation::default()
        }
    }

    fn input_ended(self: &Arc<Self>) {
        let start = Instant::now();
        if let Ok(mut state) = self.state.lock() {
            if state.eof.is_some() {
                return;
            }
            state.eof = Some(start);
            state.stage = "input_ended";
        }
        let observer = Arc::clone(self);
        if let Ok(runtime) = tokio::runtime::Handle::try_current() {
            runtime.spawn(async move {
                tokio::time::sleep(observer.shutdown).await;
                observer.check_shutdown(Instant::now());
            });
        }
    }

    fn check_shutdown(&self, now: Instant) {
        let Ok(mut state) = self.state.lock() else {
            return;
        };
        self.overrun_on(&mut state, now);
    }

    fn overrun_on(&self, state: &mut State, now: Instant) {
        if let Some(start) = state.eof
            && !state.closed
            && !state.overrun
            && now.saturating_duration_since(start) >= self.shutdown
        {
            state.overrun = true;
            self.sink.emit(Event::shutdown(
                state.stage,
                now.saturating_duration_since(start),
            ));
        }
    }

    /// Observes the existing opt-in trace milestones without changing its output.
    pub fn stage(&self, stage: crate::phase_trace::ShutdownStage) {
        if let Ok(mut state) = self.state.lock() {
            state.stage = match stage {
                crate::phase_trace::ShutdownStage::InputEnded => "input_ended",
                crate::phase_trace::ShutdownStage::TransportCloseStarted => {
                    "transport_close_started"
                }
                crate::phase_trace::ShutdownStage::TransportCloseFinished => {
                    "transport_close_finished"
                }
                crate::phase_trace::ShutdownStage::TraceFlushStarted => "trace_flush_started",
                crate::phase_trace::ShutdownStage::TraceFlushFinished => "trace_flush_finished",
                crate::phase_trace::ShutdownStage::ServiceWaitingReturned => {
                    "service_waiting_returned"
                }
            };
        }
    }

    pub fn finished(&self) {
        self.finished_at(Instant::now());
    }

    fn finished_at(&self, now: Instant) {
        if let Ok(mut state) = self.state.lock() {
            // The timer may be delayed by busy runtime workers. Closing is
            // another observation of the same clock and one-shot threshold.
            self.overrun_on(&mut state, now);
            state.closed = true;
            for slot in &mut state.pending {
                if let Some(item) = slot.take() {
                    self.sink
                        .emit(item.event(Instant::now(), Fault::incomplete()));
                }
            }
            state.active = 0;
        }
    }
}
