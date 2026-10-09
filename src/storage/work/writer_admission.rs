//! Bounded writer acquisition; transaction bodies and commits are never replayed.

use std::ops::{Deref, DerefMut};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use rusqlite::{Transaction, TransactionBehavior};
use serde::Serialize;

use super::schema::require_work_schema_version;
use crate::storage::{SqliteStore, StoreError};

const NOTE_ADMISSION_WINDOW: Duration = Duration::from_secs(10);

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
        }
    }

    fn remaining(&self) -> Duration {
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

    pub(super) fn begin_work_mutation(&mut self) -> Result<Transaction<'_>, StoreError> {
        #[cfg(test)]
        test_policy::before_acquisition();
        let started = Instant::now();
        let ordinary_timeout = Duration::from_millis(u64::from(self.connection.query_row(
            "PRAGMA busy_timeout",
            [],
            |row| row.get::<_, u32>(0),
        )?));
        let window = self.writer_admission.as_ref();
        let allowance = window.map_or(ordinary_timeout, WriterAdmissionWindow::remaining);
        #[cfg(test)]
        test_policy::observe_allowance(allowance);
        let autocommit = self.connection.is_autocommit();
        if window.is_some() && autocommit {
            self.connection.busy_timeout(allowance)?;
        }
        // The shared connection borrow allows restoring the handler before
        // returning the guard. Autocommit is checked; a nested BEGIN refuses
        // without rolling back the caller's transaction.
        let acquired = Transaction::new_unchecked(&self.connection, TransactionBehavior::Immediate);
        if window.is_some() && autocommit {
            // Restore on both acquisition outcomes, before any body runs.
            self.connection.busy_timeout(ordinary_timeout)?;
        }
        let transaction = acquired
            .map_err(|source| admission_error(source, autocommit, started.elapsed(), allowance))?;
        require_work_schema_version(&transaction, self.work_schema_version)?;
        Ok(transaction)
    }
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
pub(crate) use test_policy::last_writer_admission_allowance;
#[cfg(test)]
pub(crate) use test_policy::with_writer_admission_test_policy;
