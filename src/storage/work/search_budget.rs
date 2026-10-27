//! Cooperative limits for one storage catalog search, across every scan caller.
//! SQL stepping and bounded Rust units check one sticky allowance. This is not
//! a hard real-time guarantee: native/filesystem stalls and an existing SQLite
//! busy wait must return before the next cooperative check. No worker is left
//! running when this synchronous scope returns.
use crate::storage::control_runtime::request_budget::Allowance;
use std::cell::RefCell;
use std::marker::PhantomData;
use std::rc::Rc;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;
use tokio_util::sync::CancellationToken;

use rusqlite::{Connection, ErrorCode, Row, limits::Limit};

use crate::storage::StoreError;

const PROGRESS_STEPS: i32 = 1;

#[derive(Clone, Copy)]
pub(crate) struct Limits {
    pub vm_steps: u64,
    pub records: u64,
    pub bytes: u64,
    pub unit_bytes: i32,
    pub elapsed: Duration,
}

impl Default for Limits {
    fn default() -> Self {
        Self {
            vm_steps: 10_000_000,
            records: 100_000,
            bytes: 384 * 1024 * 1024,
            unit_bytes: 1024 * 1024,
            elapsed: Duration::from_secs(60),
        }
    }
}

struct State {
    connection: usize,
    limits: Limits,
    request: Request,
    steps: AtomicU64,
    records: AtomicU64,
    bytes: AtomicU64,
    #[cfg(test)]
    max_unit_bytes: AtomicU64,
    failed: AtomicBool,
    failure: std::sync::Mutex<Option<(&'static str, u64, u64)>>,
}

impl State {
    fn refuse(&self, dimension: &'static str, limit: u64, used: u64) {
        self.failed.store(true, Ordering::Relaxed);
        if let Ok(mut failure) = self.failure.lock() {
            failure.get_or_insert((dimension, limit, used));
        }
    }

    fn progress(&self) -> bool {
        let used = self.steps.fetch_add(1, Ordering::Relaxed).saturating_add(1);
        if used > self.limits.vm_steps {
            self.refuse("sql_vm_steps", self.limits.vm_steps, used);
            return true;
        }
        // Short statements must still consume the SQL allowance. Poll the
        // clock less often than the counter to keep the VM hook inexpensive.
        (used.is_multiple_of(1024) || self.failed.load(Ordering::Relaxed)) && self.check().is_err()
    }

    fn check(&self) -> Result<(), StoreError> {
        if self.request.cancel.is_cancelled() {
            return Err(StoreError::SearchCancelled);
        }
        if self.request.allowance.remaining().is_zero() {
            self.refuse(
                "elapsed_ms",
                u64::try_from(self.limits.elapsed.as_millis()).unwrap_or(u64::MAX),
                u64::try_from(self.request.allowance.elapsed().as_millis()).unwrap_or(u64::MAX),
            );
        }
        match *self.failure.lock().map_err(|_| {
            StoreError::InvalidWorkProjection("search budget state lock is poisoned".into())
        })? {
            Some((dimension, limit, used_at_least)) => Err(StoreError::SearchBudgetExhausted {
                dimension,
                limit,
                used_at_least,
            }),
            None => Ok(()),
        }
    }

    fn charge(
        &self,
        counter: &AtomicU64,
        amount: u64,
        limit: u64,
        dimension: &'static str,
    ) -> Result<(), StoreError> {
        self.check()?;
        let previous = counter
            .try_update(Ordering::Relaxed, Ordering::Relaxed, |value| {
                Some(value.saturating_add(amount))
            })
            .unwrap_or(u64::MAX);
        let used = previous.saturating_add(amount);
        if used > limit {
            self.refuse(dimension, limit, used);
        }
        self.check()
    }

    fn bytes(&self, bytes: usize) -> Result<(), StoreError> {
        let bytes = u64::try_from(bytes).unwrap_or(u64::MAX);
        #[cfg(test)]
        self.max_unit_bytes.fetch_max(bytes, Ordering::Relaxed);
        let unit_bytes = u64::try_from(self.limits.unit_bytes).unwrap_or(0);
        if bytes > unit_bytes {
            self.refuse("unit_bytes", unit_bytes, bytes);
        }
        self.charge(&self.bytes, bytes, self.limits.bytes, "work_bytes")
    }
}

thread_local! {
    static CURRENT: RefCell<Option<Arc<State>>> = const { RefCell::new(None) };
    static REQUEST: RefCell<Option<Request>> = const { RefCell::new(None) };
    #[cfg(test)]
    static TEST_LIMITS: std::cell::Cell<Option<Limits>> = const { std::cell::Cell::new(None) };
}

/// The same admission allowance follows the request into its blocking worker.
#[derive(Clone)]
pub(crate) struct Request {
    allowance: Allowance,
    cancel: CancellationToken,
    limits: Limits,
    #[cfg(test)]
    pause: Option<Arc<TestPause>>,
}

impl Request {
    pub(crate) fn new(cancel: CancellationToken) -> Self {
        #[cfg(not(test))]
        let limits = Limits::default();
        #[cfg(test)]
        let limits = TEST_LIMITS.with(std::cell::Cell::get).unwrap_or_default();
        Self {
            allowance: Allowance::new(limits.elapsed),
            cancel,
            limits,
            #[cfg(test)]
            pause: None,
        }
    }

    pub(crate) fn check(&self) -> Result<(), StoreError> {
        if self.cancel.is_cancelled() {
            return Err(StoreError::SearchCancelled);
        }
        if self.allowance.remaining().is_zero() {
            return Err(StoreError::SearchBudgetExhausted {
                dimension: "elapsed_ms",
                limit: u64::try_from(self.limits.elapsed.as_millis()).unwrap_or(u64::MAX),
                used_at_least: u64::try_from(self.allowance.elapsed().as_millis())
                    .unwrap_or(u64::MAX),
            });
        }
        Ok(())
    }

    pub(crate) fn run<T>(
        self,
        read: impl FnOnce() -> Result<T, crate::VerbError>,
    ) -> Result<T, crate::VerbError> {
        struct Restore(Option<Request>);
        impl Drop for Restore {
            fn drop(&mut self) {
                REQUEST.with_borrow_mut(|slot| *slot = self.0.take());
            }
        }
        self.check()?;
        let _restore = Restore(REQUEST.with_borrow_mut(|slot| slot.replace(self.clone())));
        let result = read();
        self.check()?;
        result
    }
}

#[cfg(test)]
#[derive(Debug)]
pub(crate) struct TestPause {
    entered: std::sync::Mutex<Option<tokio::sync::oneshot::Sender<()>>>,
    pub(crate) release: tokio::sync::Notify,
}

#[cfg(test)]
impl TestPause {
    pub(crate) fn new() -> (Arc<Self>, tokio::sync::oneshot::Receiver<()>) {
        let (entered, receiver) = tokio::sync::oneshot::channel();
        (
            Arc::new(Self {
                entered: std::sync::Mutex::new(Some(entered)),
                release: tokio::sync::Notify::new(),
            }),
            receiver,
        )
    }
}

#[cfg(test)]
impl Request {
    pub(crate) fn with_pause(mut self, pause: Option<Arc<TestPause>>) -> Self {
        self.pause = pause;
        self
    }
}

#[cfg(test)]
pub(crate) fn pause(connection: &Connection) -> Result<(), StoreError> {
    if let Some(state) = active(connection)
        && let Some(pause) = &state.request.pause
        && let Some(entered) = pause.entered.lock().unwrap().take()
    {
        let _ = entered.send(());
        tokio::runtime::Handle::current().block_on(async {
            tokio::select! {
                () = pause.release.notified() => {},
                () = state.request.cancel.cancelled() => {},
            }
        });
        state.check()?;
    }
    Ok(())
}

fn active(connection: &Connection) -> Option<Arc<State>> {
    let key = std::ptr::from_ref(connection) as usize;
    CURRENT.with_borrow(|state| {
        state
            .as_ref()
            .filter(|state| state.connection == key)
            .cloned()
    })
}

struct Scope<'a> {
    connection: &'a Connection,
    prior_length: i32,
    previous: Option<Arc<State>>,
    _thread: PhantomData<Rc<()>>,
}

impl crate::storage::SqliteStore {
    /// Share one connection budget across catalog selection and its enrichment.
    /// Nested storage entrypoints join this scope without resetting counters.
    pub(crate) fn with_search_budget<T>(
        &self,
        search: Option<&str>,
        read: impl FnOnce() -> Result<T, StoreError>,
    ) -> Result<T, StoreError> {
        run(&self.connection, search, read)
    }
}

impl Drop for Scope<'_> {
    fn drop(&mut self) {
        #[cfg(test)]
        if let Some(state) = active(self.connection) {
            super::cost::search_budget_finished(
                state.steps.load(Ordering::Relaxed),
                state.records.load(Ordering::Relaxed),
                state.bytes.load(Ordering::Relaxed),
                state.max_unit_bytes.load(Ordering::Relaxed),
            );
        }
        let _ = self.connection.progress_handler(0, None::<fn() -> bool>);
        let _ = self
            .connection
            .set_limit(Limit::SQLITE_LIMIT_LENGTH, self.prior_length);
        CURRENT.with_borrow_mut(|state| *state = self.previous.take());
    }
}

pub(crate) fn run<T>(
    connection: &Connection,
    search: Option<&str>,
    read: impl FnOnce() -> Result<T, StoreError>,
) -> Result<T, StoreError> {
    let Some(search) = search else {
        return read();
    };
    if let Some(state) = active(connection) {
        state.check()?;
        let result = read();
        state.check()?;
        return result;
    }
    let request = REQUEST
        .with_borrow(Clone::clone)
        .unwrap_or_else(|| Request::new(CancellationToken::new()));
    let limits = request.limits;
    let state = Arc::new(State {
        connection: std::ptr::from_ref(connection) as usize,
        limits,
        request,
        steps: AtomicU64::new(0),
        records: AtomicU64::new(0),
        bytes: AtomicU64::new(0),
        #[cfg(test)]
        max_unit_bytes: AtomicU64::new(0),
        failed: AtomicBool::new(false),
        failure: std::sync::Mutex::new(None),
    });
    state.bytes(search.len())?;
    if search.trim().is_empty() {
        return read();
    }
    state.check()?;
    let prior_length = connection.limit(Limit::SQLITE_LIMIT_LENGTH)?;
    connection.set_limit(
        Limit::SQLITE_LIMIT_LENGTH,
        prior_length.min(limits.unit_bytes),
    )?;
    let previous = CURRENT.with_borrow_mut(|slot| slot.replace(Arc::clone(&state)));
    let _scope = Scope {
        connection,
        prior_length,
        previous,
        _thread: PhantomData,
    };
    let progress = Arc::clone(&state);
    connection.progress_handler(PROGRESS_STEPS, Some(move || progress.progress()))?;
    let result = read();
    if result.as_ref().err().is_some_and(|error| {
        matches!(error,
        StoreError::Sqlite(rusqlite::Error::SqliteFailure(code, _))
        if code.code == ErrorCode::TooBig)
    }) {
        state.refuse(
            "unit_bytes",
            u64::try_from(limits.unit_bytes).unwrap_or(0),
            u64::try_from(limits.unit_bytes)
                .unwrap_or(0)
                .saturating_add(1),
        );
    }
    state.check()?;
    result
}

/// Charge before a row/index allocation, rather than after collecting a vector.
pub(crate) fn record(connection: &Connection) -> Result<(), StoreError> {
    if let Some(state) = active(connection) {
        state.charge(&state.records, 1, state.limits.records, "record_visits")?;
        // Conservative accounting for retained index/match identifiers and nodes.
        state.charge(&state.bytes, 512, state.limits.bytes, "work_bytes")?;
    }
    Ok(())
}

/// Count selected text/blob lengths before retaining a materialized row.
pub(crate) fn record_row(connection: &Connection, row: &Row<'_>) -> rusqlite::Result<()> {
    record(connection).map_err(|_| rusqlite::Error::InvalidQuery)?;
    if let Some(state) = active(connection) {
        for index in 0..row.as_ref().column_count() {
            match row.get_ref(index)? {
                rusqlite::types::ValueRef::Text(bytes) | rusqlite::types::ValueRef::Blob(bytes) => {
                    state
                        .bytes(bytes.len())
                        .map_err(|_| rusqlite::Error::InvalidQuery)?;
                }
                _ => {}
            }
        }
    }
    Ok(())
}

/// Retain a JSON projection and its SQL binding check after charging its row.
pub(crate) fn bound_json_row(
    connection: &Connection,
    row: &Row<'_>,
) -> rusqlite::Result<(Vec<u8>, bool)> {
    record_row(connection, row)?;
    Ok((row.get(0)?, row.get(1)?))
}

/// Check length and charge bytes before cloning the SQLite-owned blob.
pub(crate) fn blob(
    connection: &Connection,
    row: &Row<'_>,
    column: usize,
) -> rusqlite::Result<Vec<u8>> {
    let bytes = row.get_ref(column)?.as_blob()?;
    if let Some(state) = active(connection) {
        state
            .bytes(bytes.len())
            .map_err(|_| rusqlite::Error::InvalidQuery)?;
    }
    Ok(bytes.to_vec())
}

pub(crate) fn contains(
    connection: &Connection,
    text: &str,
    needle: &str,
) -> Result<bool, StoreError> {
    if let Some(state) = active(connection) {
        state.bytes(text.len())?;
    }
    let found = super::planning::normalize_work_catalog_key(text).contains(needle);
    if let Some(state) = active(connection) {
        state.check()?;
    }
    Ok(found)
}

#[cfg(test)]
pub(crate) fn with_limits<T>(limits: Limits, read: impl FnOnce() -> T) -> T {
    struct Restore(Option<Limits>);
    impl Drop for Restore {
        fn drop(&mut self) {
            TEST_LIMITS.with(|slot| slot.set(self.0));
        }
    }
    let _restore = Restore(TEST_LIMITS.with(|slot| slot.replace(Some(limits))));
    read()
}

#[cfg(test)]
mod tests;
