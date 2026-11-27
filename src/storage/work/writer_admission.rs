//! Bounded writer acquisition; transaction bodies and commits are never replayed.

use std::ops::{Deref, DerefMut};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use rusqlite::{Transaction, TransactionBehavior};
use serde::Serialize;

use super::schema::require_work_schema_version;
use crate::storage::{SqliteStore, StoreError};

const NOTE_ADMISSION_WINDOW: Duration = crate::operation_time::MINIMUM;

/// Keeps imported-origin reads shared within this mutation's transaction.
#[derive(Debug)]
pub(super) struct WorkMutation<'connection> {
    // Drop the scope before the transaction rolls back on an early return.
    origin_scope: super::task_sync::OriginScopeGuard,
    transaction: Transaction<'connection>,
    // Field order is significant: implicit rollback precedes observation.
    timing: crate::storage::transaction_timing::TransactionTiming<'connection>,
    // Last, so the write is named through its body and settlement. A note's
    // writes share their window's one warning instead, held by the window.
    warning: Option<crate::operation_time::Warning>,
}

impl<'connection> Deref for WorkMutation<'connection> {
    type Target = Transaction<'connection>;

    fn deref(&self) -> &Self::Target {
        &self.transaction
    }
}

impl<'connection> WorkMutation<'connection> {
    /// Borrow the already admitted writer for an atomic savepoint owner.
    pub(super) fn transaction_mut(&mut self) -> &mut Transaction<'connection> {
        &mut self.transaction
    }
    pub(super) fn commit(self) -> rusqlite::Result<()> {
        #[cfg(test)]
        test_policy::before_commit();
        // Nothing commits after its operation's deadline: by then the answer
        // is due, so the write is given back whole and refused as a busy
        // store. Once COMMIT starts, its outcome stands.
        if crate::storage::operation_deadline::current()
            .is_some_and(|deadline| Instant::now() >= deadline)
        {
            self.rollback()?;
            return Err(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
                Some("the operation's time limit ended before the write committed".into()),
            ));
        }
        let Self {
            origin_scope,
            transaction,
            mut timing,
            warning,
        } = self;
        drop(origin_scope);
        let result = transaction.commit();
        timing.committed(result.is_ok());
        drop(warning);
        #[cfg(test)]
        if result.is_ok() {
            test_policy::after_commit();
        }
        result
    }

    pub(super) fn rollback(self) -> rusqlite::Result<()> {
        let Self {
            origin_scope,
            transaction,
            mut timing,
            warning,
        } = self;
        drop(origin_scope);
        let result = transaction.rollback();
        timing.rolled_back(result.is_ok());
        drop(warning);
        result
    }
}

/// Why SQLite did not admit this one write transaction. Earlier transactions
/// of the same word may already have committed.
#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum WorkWriterAdmissionReason {
    /// The native busy handler returned BUSY after its bounded wait.
    BusyBudgetExhausted,
    /// SQLite returned LOCKED, which the busy handler cannot resolve.
    Locked,
    /// SQLite refused to promote a stale read snapshot.
    BusySnapshot,
    /// The caller already owns a transaction; it is left intact.
    NotAutocommit,
}

impl WorkWriterAdmissionReason {
    #[must_use]
    pub const fn as_str(self) -> &'static str {
        match self {
            Self::BusyBudgetExhausted => "busy_budget_exhausted",
            Self::Locked => "locked",
            Self::BusySnapshot => "busy_snapshot",
            Self::NotAutocommit => "not_autocommit",
        }
    }
}

impl std::fmt::Display for WorkWriterAdmissionReason {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.write_str(self.as_str())
    }
}

#[derive(Clone)]
pub(crate) struct WriterAdmissionWindow {
    started: Arc<OnceLock<Instant>>,
    budget: Duration,
    /// One ten-second warning for the whole window, which its admissions
    /// share, started at its first admission and ended with its last clone.
    warning: Arc<OnceLock<crate::operation_time::Warning>>,
}

impl WriterAdmissionWindow {
    pub(crate) fn note() -> Self {
        let budget = NOTE_ADMISSION_WINDOW;
        #[cfg(test)]
        let budget = test_policy::budget().unwrap_or(budget);
        Self::with_budget(budget)
    }

    fn with_budget(budget: Duration) -> Self {
        Self {
            started: Arc::new(OnceLock::new()),
            budget,
            warning: Arc::new(OnceLock::new()),
        }
    }

    fn start_warning(&self) {
        self.warning
            .get_or_init(|| crate::operation_time::Warning::start("note write admission"));
    }

    fn remaining(&self) -> Duration {
        #[cfg(test)]
        if let Some(now) = test_policy::now() {
            let started = *self.started.get_or_init(|| now);
            return self
                .budget
                .saturating_sub(now.saturating_duration_since(started));
        }
        self.budget
            .saturating_sub(self.started.get_or_init(Instant::now).elapsed())
    }
}

/// Keeps every acquisition of this note on one window, including early returns
/// and unwinding. It carries no authority and changes no caller timestamp.
pub(crate) struct NoteWriterAdmission<'a> {
    store: &'a mut SqliteStore,
    previous: Option<WriterAdmissionWindow>,
}

impl Deref for NoteWriterAdmission<'_> {
    type Target = SqliteStore;

    fn deref(&self) -> &Self::Target {
        self.store
    }
}

impl DerefMut for NoteWriterAdmission<'_> {
    fn deref_mut(&mut self) -> &mut Self::Target {
        self.store
    }
}

impl Drop for NoteWriterAdmission<'_> {
    fn drop(&mut self) {
        self.store.writer_admission = self.previous.take();
    }
}

impl SqliteStore {
    pub(crate) fn note_writer_admission(&mut self) -> NoteWriterAdmission<'_> {
        self.note_writer_admission_with_window(WriterAdmissionWindow::note())
    }

    #[cfg(test)]
    fn note_writer_admission_with_budget(&mut self, budget: Duration) -> NoteWriterAdmission<'_> {
        self.note_writer_admission_with_window(WriterAdmissionWindow::with_budget(budget))
    }

    pub(crate) fn note_writer_admission_with_window(
        &mut self,
        window: WriterAdmissionWindow,
    ) -> NoteWriterAdmission<'_> {
        let previous = self.writer_admission.clone();
        // A nested scope joins the existing window rather than resetting it.
        self.writer_admission.get_or_insert(window);
        NoteWriterAdmission {
            store: self,
            previous,
        }
    }

    pub(super) fn begin_work_mutation(&mut self) -> Result<WorkMutation<'_>, StoreError> {
        // Acquiring the writer can wait out the whole busy timeout or window.
        // A note's admissions share one window, and so its one warning.
        let warning = match &self.writer_admission {
            Some(window) => {
                window.start_warning();
                None
            }
            None => Some(crate::operation_time::Warning::start(
                "store write admission",
            )),
        };
        #[cfg(test)]
        test_policy::before_acquisition();
        let started = Instant::now();
        let ordinary_timeout = Duration::from_millis(u64::from(self.connection.query_row(
            "PRAGMA busy_timeout",
            [],
            |row| row.get::<_, u32>(0),
        )?));
        let window = self.writer_admission.as_ref();
        // An operation's deadline, shared with its open, ends this wait too.
        let deadline = crate::storage::operation_deadline::current();
        let uncapped = window.map_or(ordinary_timeout, WriterAdmissionWindow::remaining);
        let allowance = crate::storage::operation_deadline::cap(uncapped);
        #[cfg(test)]
        test_policy::observe_allowance(allowance);
        let autocommit = self.connection.is_autocommit();
        // A write never begins after its operation's deadline, even with the
        // writer free: by then the operation's answer is already due.
        let expired = || deadline.is_some_and(|deadline| Instant::now() >= deadline);
        if autocommit && expired() {
            return Err(deadline_passed(started.elapsed(), allowance));
        }
        let bounded = (window.is_some() || deadline.is_some()) && autocommit;
        // Inside a deadline the wait reads the clock: SQLite's busy timeout
        // counts the sleeps it asks for, which can overrun and admit the
        // writer after the deadline has passed.
        // The wait ends at the operation's own deadline, or sooner at the
        // ordinary or window limit, both read from one clock sample here.
        let clock_end = crate::storage::operation_deadline::earliest(
            deadline,
            Instant::now().checked_add(uncapped),
        );
        let _clock_wait = if let Some(end) = clock_end.filter(|_| bounded && deadline.is_some()) {
            Some(crate::storage::open_schema::wait_for_lock_until(
                &self.connection,
                end,
            )?)
        } else {
            if bounded {
                self.connection.busy_timeout(allowance)?;
            }
            None
        };
        // The shared connection borrow allows restoring the handler before
        // returning the guard. Autocommit is checked; a nested BEGIN refuses
        // without rolling back the caller's transaction.
        let mut timing = crate::storage::transaction_timing::TransactionTiming::start(
            &self.connection,
            crate::diagnostics::transaction::Boundary::WorkMutation,
            crate::diagnostics::transaction::Budget {
                configured_ms: Some(millis(ordinary_timeout)),
                allowance_ms: Some(millis(allowance)),
                shared_window_ms: window.map(|window| millis(window.budget)),
            },
        );
        let acquired = Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate);
        #[cfg(test)]
        test_policy::observe_acquisition(started, Instant::now());
        match &acquired {
            Ok(_) => timing.acquired(),
            Err(error) => timing.begin_failed(error, autocommit),
        }
        if bounded {
            // Restore on both acquisition outcomes, before any body runs.
            self.connection.busy_timeout(ordinary_timeout)?;
        }
        let transaction = acquired
            .map_err(|source| admission_error(source, autocommit, started.elapsed(), allowance))?;
        // Admitted only as the deadline passed: give the writer back before
        // anything is written.
        if expired() {
            let rolled_back = transaction.rollback();
            timing.rolled_back(rolled_back.is_ok());
            rolled_back?;
            return Err(deadline_passed(started.elapsed(), allowance));
        }
        require_work_schema_version(&transaction, self.work_schema_version)?;
        let origin_scope = super::task_sync::enter_origin_scope(&transaction)?;
        Ok(WorkMutation {
            origin_scope,
            transaction,
            timing,
            warning,
        })
    }
}

/// Begins a write transaction outside the work-mutation path, with the
/// warning that names the write once it has run ten seconds: the wait for the
/// write lock, the body and the commit. Bind the warning first, as
/// `let (_warning, transaction) = …`, so it is dropped after the transaction
/// settles, an implicit rollback included.
pub(crate) fn begin_immediate<'connection>(
    connection: &'connection mut rusqlite::Connection,
    operation: &'static str,
) -> rusqlite::Result<(crate::operation_time::Warning, Transaction<'connection>)> {
    let warning = crate::operation_time::Warning::start(operation);
    #[cfg(test)]
    test_policy::before_acquisition();
    let transaction = connection.transaction_with_behavior(TransactionBehavior::Immediate)?;
    Ok((warning, transaction))
}

/// The refusal of a write its operation's deadline has already passed: a busy
/// store, as a writer the deadline ended the wait for is.
fn deadline_passed(elapsed: Duration, allowance: Duration) -> StoreError {
    admission_error(
        rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            Some("the operation's time limit ended before the write began".into()),
        ),
        true,
        elapsed,
        allowance,
    )
}

fn admission_error(
    source: rusqlite::Error,
    autocommit: bool,
    elapsed: Duration,
    allowance: Duration,
) -> StoreError {
    let sqlite = source.sqlite_error();
    let extended = sqlite.map(|error| error.extended_code);
    let primary = extended.map(|code| code & 0xff);
    let reason = if !autocommit {
        Some(WorkWriterAdmissionReason::NotAutocommit)
    } else if extended == Some(rusqlite::ffi::SQLITE_BUSY_SNAPSHOT) {
        Some(WorkWriterAdmissionReason::BusySnapshot)
    } else {
        match primary {
            Some(rusqlite::ffi::SQLITE_BUSY) => {
                Some(WorkWriterAdmissionReason::BusyBudgetExhausted)
            }
            Some(rusqlite::ffi::SQLITE_LOCKED) => Some(WorkWriterAdmissionReason::Locked),
            _ => None,
        }
    };
    match reason {
        Some(reason) => StoreError::WorkWriterAdmissionRefused {
            reason,
            elapsed_ms: millis(elapsed),
            budget_ms: millis(allowance),
            sqlite_primary_code: primary,
            sqlite_extended_code: extended,
            source: Box::new(source),
        },
        None => StoreError::Sqlite(source),
    }
}

fn millis(duration: Duration) -> u64 {
    u64::try_from(duration.as_millis()).unwrap_or(u64::MAX)
}

#[cfg(test)]
mod test_policy;
#[cfg(test)]
mod tests;
#[cfg(test)]
pub(crate) use test_policy::advance_writer_admission_window;
#[cfg(test)]
pub(crate) use test_policy::last_writer_admission_allowance;
#[cfg(test)]
pub(crate) use test_policy::with_writer_admission_test_policy;
#[cfg(test)]
pub(crate) use test_policy::with_writer_commit_hook;
#[cfg(test)]
pub(crate) use test_policy::with_writer_settle_hook;
#[cfg(test)]
pub(crate) use test_policy::writer_admission_acquisitions;
