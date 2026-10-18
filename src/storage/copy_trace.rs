//! A test-only record of when each sub-step of a store copy began and ended,
//! so a test that bounds a copy's time can name the step that took it.
//!
//! Recording is per thread and off until a test starts it, so marks made by
//! other tests' copies, and by every open that shares the admission code,
//! build no step strings and record nothing.

use std::cell::RefCell;
use std::time::{Duration, Instant};

use super::StoreError;

struct Record {
    began: Instant,
    steps: Vec<String>,
    busy: Busy,
}

/// Fixed-size observations; callback iterations never append to `steps`.
#[derive(Debug, Default)]
struct Busy {
    calls: u64,
    first_entry: Option<Duration>,
    last_entry: Option<Duration>,
    last_return: Option<Duration>,
    max_outside: Duration,
    sleeps: u64,
    max_sleep: Duration,
    final_refusal: Option<(Duration, &'static str)>,
}

impl Busy {
    fn enter(&mut self, at: Duration) {
        self.calls = self.calls.saturating_add(1);
        self.first_entry.get_or_insert(at);
        self.last_entry = Some(at);
        if let Some(previous) = self.last_return {
            self.max_outside = self.max_outside.max(at.saturating_sub(previous));
        }
    }

    fn sleep(&mut self, elapsed: Duration) {
        self.sleeps = self.sleeps.saturating_add(1);
        self.max_sleep = self.max_sleep.max(elapsed);
    }

    fn returned(&mut self, at: Duration, refusal: Option<&'static str>) {
        self.last_return = Some(at);
        if let Some(reason) = refusal {
            self.final_refusal = Some((at, reason));
        }
    }
}

thread_local! {
    static RECORD: RefCell<Option<Record>> = const { RefCell::new(None) };
}

/// Starts recording this thread's marks, discarding any earlier record.
pub(crate) fn start() {
    RECORD.with(|record| {
        *record.borrow_mut() = Some(Record {
            began: Instant::now(),
            steps: Vec::new(),
            busy: Busy::default(),
        });
    });
}

/// Records `step`, with the time since [`start`], when this thread records.
pub(crate) fn mark(step: impl FnOnce() -> String) {
    RECORD.with(|record| {
        if let Some(record) = record.borrow_mut().as_mut() {
            record
                .steps
                .push(format!("{:?} {}", record.began.elapsed(), step()));
        }
    });
}

pub(crate) fn recording() -> bool {
    RECORD.with(|record| record.borrow().is_some())
}

/// Stops recording on this thread and returns the marks in order.
pub(crate) fn take() -> Vec<String> {
    RECORD.with(|record| {
        record
            .borrow_mut()
            .take()
            .map(|mut record| {
                if record.busy.calls != 0 {
                    record
                        .steps
                        .push(format!("busy callbacks: {:?}", record.busy));
                }
                record.steps
            })
            .unwrap_or_default()
    })
}

/// Records a callback entry without allocating or writing a log.
pub(crate) fn busy_enter(now: Instant) {
    RECORD.with(|record| {
        if let Some(record) = record.borrow_mut().as_mut() {
            record
                .busy
                .enter(now.saturating_duration_since(record.began));
        }
    });
}

pub(crate) fn busy_sleep(elapsed: Duration) {
    RECORD.with(|record| {
        if let Some(record) = record.borrow_mut().as_mut() {
            record.busy.sleep(elapsed);
        }
    });
}

pub(crate) fn busy_return(now: Instant, refusal: Option<&'static str>) {
    RECORD.with(|record| {
        if let Some(record) = record.borrow_mut().as_mut() {
            record
                .busy
                .returned(now.saturating_duration_since(record.began), refusal);
        }
    });
}

/// Brackets disposal separately from the operation that produced an error.
pub(crate) fn dispose<T>(value: T, label: &str) {
    mark(|| format!("{label}: close begins"));
    drop(value);
    mark(|| format!("{label}: closed"));
}

/// Preserve SQLite's original primary and extended codes before classification.
pub(crate) fn store_error(error: &StoreError) -> String {
    let sqlite = match error {
        StoreError::Sqlite(source) => Some(source),
        StoreError::SqliteFile { source, .. } => Some(source.as_ref()),
        _ => None,
    };
    match sqlite {
        Some(rusqlite::Error::SqliteFailure(failure, _)) => format!(
            "{error}; sqlite code {:?}, extended {}",
            failure.code, failure.extended_code
        ),
        _ => error.to_string(),
    }
}

/// A short account of a step's result for a mark.
pub(crate) fn outcome<T, E: std::fmt::Display>(result: &Result<T, E>) -> String {
    match result {
        Ok(_) => "ok".into(),
        Err(error) => format!("error: {error}"),
    }
}

#[cfg(test)]
mod tests;
