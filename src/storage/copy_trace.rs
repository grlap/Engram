//! A test-only record of when each sub-step of a store copy began and ended,
//! so a test that bounds a copy's time can name the step that took it.
//!
//! Recording is per thread and off until a test starts it, so marks made by
//! other tests' copies, and by every open that shares the admission code, cost
//! nothing and record nothing.

use std::cell::RefCell;
use std::time::Instant;

thread_local! {
    static RECORD: RefCell<Option<(Instant, Vec<String>)>> = const { RefCell::new(None) };
}

/// Starts recording this thread's marks, discarding any earlier record.
pub(crate) fn start() {
    RECORD.with(|record| *record.borrow_mut() = Some((Instant::now(), Vec::new())));
}

/// Records `step`, with the time since [`start`], when this thread records.
pub(crate) fn mark(step: impl FnOnce() -> String) {
    RECORD.with(|record| {
        if let Some((began, steps)) = record.borrow_mut().as_mut() {
            steps.push(format!("{:?} {}", began.elapsed(), step()));
        }
    });
}

/// Stops recording on this thread and returns the marks in order.
pub(crate) fn take() -> Vec<String> {
    RECORD.with(|record| {
        record
            .borrow_mut()
            .take()
            .map(|(_, steps)| steps)
            .unwrap_or_default()
    })
}

/// A short account of a step's result for a mark.
pub(crate) fn outcome<T, E: std::fmt::Display>(result: &Result<T, E>) -> String {
    match result {
        Ok(_) => "ok".into(),
        Err(error) => format!("error: {error}"),
    }
}
