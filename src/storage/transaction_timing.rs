//! A guard must precede its SQLite transaction so implicit cleanup finishes
//! before its connection state is observed. No timing result grants authority.
use crate::diagnostics::transaction::{Boundary, Budget, Disposition, Observation};
use rusqlite::Connection;
pub(super) mod control;

#[derive(Debug)]
pub(super) struct TransactionTiming<'a> {
    connection: &'a Connection,
    observation: Option<Observation>,
    control: Option<control::Observation>,
    disposition: Disposition,
    acquired: bool,
}
impl<'a> TransactionTiming<'a> {
    pub(super) fn start(connection: &'a Connection, boundary: Boundary, budget: Budget) -> Self {
        Self {
            connection,
            observation: Observation::start(boundary, budget),
            control: if matches!(boundary, Boundary::ControlMutation) {
                control::Observation::start()
            } else {
                None
            },
            disposition: Disposition::BeginFailed,
            acquired: false,
        }
    }
    pub(super) fn acquired(&mut self) {
        self.acquired = true;
        self.disposition = Disposition::ScopeExit;
        if let Some(control) = &mut self.control {
            control.acquired();
        }
        if let Some(observation) = &mut self.observation {
            observation.acquired();
        }
    }
    pub(super) fn begin_failed(&mut self, error: &rusqlite::Error, autocommit: bool) {
        self.disposition = if !autocommit
            || matches!(
                error.sqlite_error_code(),
                Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
            ) {
            Disposition::AdmissionRefused
        } else {
            Disposition::BeginFailed
        };
    }
    pub(super) fn committed(&mut self, succeeded: bool) {
        self.disposition = if succeeded {
            Disposition::Committed
        } else {
            Disposition::CommitFailed
        };
    }
    pub(super) fn rolled_back(&mut self, succeeded: bool) {
        self.disposition = if succeeded {
            Disposition::RolledBack
        } else {
            Disposition::RollbackFailed
        };
    }
}
impl Drop for TransactionTiming<'_> {
    fn drop(&mut self) {
        if let Some(control) = self.control.take() {
            control.finish();
        }
        if let Some(observation) = self.observation.take() {
            observation.finish(
                self.disposition,
                self.acquired.then(|| self.connection.is_autocommit()),
            );
        }
    }
}
