//! Best-effort observed transaction intervals, never a lock-owner oracle.
//! Slow pairs are retrospective; only full capture can retain an acquisition
//! whose process never settles. Missing records never establish absence.
use super::{Correlation, Diagnostics, Event, Fault, Fingerprint, record};
use chrono::{DateTime, Utc};
use serde::Serialize;
use std::{
    cell::RefCell,
    future::Future,
    marker::PhantomData,
    rc::Rc,
    sync::{
        Arc,
        atomic::{AtomicU64, Ordering},
    },
    time::{Duration, Instant},
};

pub(crate) const SLOW_TRANSACTION: Duration = Duration::from_secs(1);
tokio::task_local! { static ASYNC_CONTEXT: Context; }
thread_local! { static SYNC_CONTEXT: RefCell<Option<Context>> = const { RefCell::new(None) }; }

#[derive(Clone, Debug)]
pub(crate) struct Context {
    sink: Diagnostics,
    operation: &'static str,
    correlation: Correlation,
    store: Fingerprint,
    mutex_ms: Arc<AtomicU64>,
}
impl Context {
    #[cfg(test)]
    pub(crate) fn capture_all_for_test(mut self) -> Self {
        self.sink = self.sink.capture_transactions_for_test();
        self
    }

    pub(crate) fn new(
        sink: Diagnostics,
        operation: &str,
        correlation: Correlation,
        store: Fingerprint,
    ) -> Self {
        Self {
            sink,
            operation: record::label(operation),
            correlation,
            store,
            mutex_ms: Arc::new(AtomicU64::new(0)),
        }
    }
    fn event(&self, stage: &'static str, elapsed: Duration) -> Event {
        let mut event = Event::failure(
            self.operation,
            self.correlation,
            elapsed,
            Fault::transaction_observation(),
        );
        event.kind = record::EventKind::StoreTransaction;
        event.stage = Some(stage);
        event
    }
}
fn current() -> Option<Context> {
    ASYNC_CONTEXT
        .try_with(Clone::clone)
        .ok()
        .or_else(|| SYNC_CONTEXT.with(|slot| slot.borrow().clone()))
}
pub(crate) async fn scoped<T>(context: Context, future: impl Future<Output = T>) -> T {
    ASYNC_CONTEXT.scope(context, future).await
}
/// Synchronous frames restore the predecessor on unwind. Async handlers use
/// the task-local scope instead; this guard cannot cross threads.
pub(crate) struct Scope {
    previous: Option<Context>,
    _thread: PhantomData<Rc<()>>,
}
impl Scope {
    pub(crate) fn enter(context: Context) -> Self {
        Self {
            previous: SYNC_CONTEXT.with(|slot| slot.replace(Some(context))),
            _thread: PhantomData,
        }
    }
}
impl Drop for Scope {
    fn drop(&mut self) {
        SYNC_CONTEXT.with(|slot| {
            slot.replace(self.previous.take());
        });
    }
}

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Boundary {
    WorkMutation,
    ControlMutation,
}
#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Disposition {
    Acquired,
    Committed,
    CommitFailed,
    RolledBack,
    RollbackFailed,
    ScopeExit,
    AdmissionRefused,
    BeginFailed,
}
#[derive(Clone, Copy, Debug, Default, Serialize)]
#[expect(
    clippy::struct_field_names,
    reason = "serialized diagnostic allowances explicitly name their millisecond units"
)]
pub(crate) struct Budget {
    pub(crate) configured_ms: Option<u64>,
    pub(crate) allowance_ms: Option<u64>,
    pub(crate) shared_window_ms: Option<u64>,
}
#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct Record {
    store_path_fingerprint: Fingerprint,
    transaction_seq: u64,
    boundary: Boundary,
    attempt_at: DateTime<Utc>,
    acquired_at: Option<DateTime<Utc>>,
    admission_ms: u64,
    acquired_interval_ms: Option<u64>,
    disposition: Disposition,
    /// Observed connection state after cleanup, not an OS lock timestamp.
    connection_autocommit: Option<bool>,
    retrospective: bool,
    budget: Budget,
    store_mutex_wait_ms: u64,
}
#[derive(Clone, Copy, Debug, Serialize)]
pub(crate) struct MutexRecord {
    store_path_fingerprint: Fingerprint,
    wait_ms: u64,
}
#[derive(Debug)]
pub(crate) struct Observation {
    context: Context,
    record: Record,
    started: Instant,
    acquired: Option<Instant>,
    acquisition_emitted: bool,
}
impl Observation {
    pub(crate) fn start(boundary: Boundary, budget: Budget) -> Option<Self> {
        Self::start_at(current()?, boundary, budget, Instant::now())
    }
    fn start_at(
        context: Context,
        boundary: Boundary,
        budget: Budget,
        started: Instant,
    ) -> Option<Self> {
        let sequence = context.sink.transaction_sequence.try_update(
            Ordering::Relaxed,
            Ordering::Relaxed,
            |value| value.checked_add(1),
        );
        let Ok(sequence) = sequence else {
            context.sink.tracking_lost();
            return None;
        };
        let record = Record {
            store_path_fingerprint: context.store,
            transaction_seq: sequence + 1,
            boundary,
            attempt_at: Utc::now(),
            acquired_at: None,
            admission_ms: 0,
            acquired_interval_ms: None,
            disposition: Disposition::BeginFailed,
            connection_autocommit: None,
            retrospective: false,
            budget,
            store_mutex_wait_ms: context.mutex_ms.load(Ordering::Relaxed),
        };
        Some(Self {
            context,
            record,
            started,
            acquired: None,
            acquisition_emitted: false,
        })
    }
    pub(crate) fn acquired(&mut self) {
        self.acquired_at(Instant::now());
    }
    fn acquired_at(&mut self, now: Instant) {
        self.acquired = Some(now);
        self.record.admission_ms = record::millis(now.saturating_duration_since(self.started));
        self.record.acquired_at = Some(Utc::now());
        if self.context.sink.full_transactions {
            self.emit_acquisition(false);
        }
    }
    fn emit_acquisition(&mut self, retrospective: bool) {
        let mut record = self.record;
        record.disposition = Disposition::Acquired;
        record.retrospective = retrospective;
        let mut event = self
            .context
            .event("acquired", Duration::from_millis(record.admission_ms));
        event.observed_at = record.acquired_at.unwrap_or(record.attempt_at);
        event.store_transaction = Some(record);
        self.context.sink.emit(event);
        self.acquisition_emitted = true;
    }
    pub(crate) fn finish(self, disposition: Disposition, autocommit: Option<bool>) {
        self.finish_at(disposition, autocommit, Instant::now());
    }
    fn finish_at(mut self, disposition: Disposition, autocommit: Option<bool>, now: Instant) {
        let elapsed = now.saturating_duration_since(self.started);
        let admission = self.acquired.map_or(elapsed, |acquired| {
            acquired.saturating_duration_since(self.started)
        });
        let acquired_interval = self
            .acquired
            .map(|acquired| now.saturating_duration_since(acquired));
        let refusal = matches!(
            disposition,
            Disposition::AdmissionRefused | Disposition::BeginFailed
        );
        if !self.context.sink.full_transactions
            && !refusal
            && admission <= SLOW_TRANSACTION
            && acquired_interval.is_none_or(|value| value <= SLOW_TRANSACTION)
        {
            return;
        }
        if self.acquired.is_some() && !self.acquisition_emitted {
            self.emit_acquisition(true);
        }
        self.record.admission_ms = record::millis(admission);
        self.record.acquired_interval_ms = acquired_interval.map(record::millis);
        self.record.disposition = disposition;
        self.record.connection_autocommit = autocommit;
        self.record.retrospective = !self.context.sink.full_transactions;
        let stage = if self.acquired.is_some() {
            "settled"
        } else {
            "acquisition_failed"
        };
        let mut event = self.context.event(stage, elapsed);
        event.store_transaction = Some(self.record);
        self.context.sink.emit(event);
    }
}
pub(crate) fn mutex_start() -> Option<Instant> {
    current().map(|_| Instant::now())
}
pub(crate) fn mutex_finished(started: Option<Instant>) {
    let Some(started) = started else {
        return;
    };
    let Some(context) = current() else {
        return;
    };
    let elapsed = started.elapsed();
    let value = record::millis(elapsed);
    let _ = context
        .mutex_ms
        .try_update(Ordering::Relaxed, Ordering::Relaxed, |previous| {
            Some(previous.saturating_add(value))
        });
    if context.sink.full_transactions || elapsed > SLOW_TRANSACTION {
        let mut event = context.event("mutex_wait", elapsed);
        event.kind = record::EventKind::StoreMutex;
        event.store_mutex = Some(MutexRecord {
            store_path_fingerprint: context.store,
            wait_ms: value,
        });
        context.sink.emit(event);
    }
}
#[cfg(test)]
mod tests;
