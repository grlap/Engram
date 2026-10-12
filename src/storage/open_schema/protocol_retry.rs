//! Recovering a store open from SQLite's locking-protocol refusal.
//!
//! SQLite returns `SQLITE_PROTOCOL` from one place: beginning a read on a
//! write-ahead log after losing the race for the log's index a hundred times
//! running, waiting a little longer each time, about ten seconds in all.
//! Concurrent first opens of a new store have met it on a loaded machine. An
//! open that meets it rolls back the transaction its attempt had open and
//! starts again from its first probe, at most once, so every admission check
//! runs again on what the store holds then, including anything an earlier
//! step of the attempt committed. An opener bound by a deadline of its own
//! does not start again, since another ten seconds of SQLite's waiting would
//! pass it; it returns the refusal at once. Either way the refusal names the
//! steps it ended, because the error alone cannot tell them apart.

use super::{Connection, StoreError};

#[cfg(test)]
use super::building_schema_reference;

#[cfg(test)]
mod tests;

/// Whether a store open starts again once after SQLite's locking-protocol
/// refusal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(super) enum ProtocolRetry {
    /// Ordinary opens: one more attempt, from the first probe.
    Once,
    /// Opens bound by a deadline of their own, such as a control host's call
    /// or a store copy: the refusal is returned at once.
    Never,
}

/// The step of an open attempt, named in a locking-protocol refusal.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub(crate) enum OpenPhase {
    /// The admission probes, all read in one snapshot.
    ProbeSnapshot,
    /// The connection settings applied after that snapshot.
    ConnectionSettings,
    /// Reading the store's journal mode.
    JournalQuery,
    /// Switching the store to write-ahead logging.
    JournalSwitch,
    /// Initializing a new store under its write lock.
    ColdInitialization,
    /// Settling an initialized store's policies and work schema.
    StoreSetup,
}

impl OpenPhase {
    pub(crate) const fn label(self) -> &'static str {
        match self {
            Self::ProbeSnapshot => "probe_snapshot",
            Self::ConnectionSettings => "connection_settings",
            Self::JournalQuery => "journal_query",
            Self::JournalSwitch => "journal_switch",
            Self::ColdInitialization => "cold_initialization",
            Self::StoreSetup => "store_setup",
        }
    }
}

/// Moves an attempt into `next`, so a refusal met there names it.
///
/// # Errors
///
/// Returns SQLite's locking-protocol refusal where a test injected one.
#[cfg_attr(
    not(test),
    allow(
        clippy::unnecessary_wraps,
        reason = "tests inject the refusal at this point; the callers propagate it either way"
    )
)]
pub(super) fn enter_phase(phase: &mut OpenPhase, next: OpenPhase) -> Result<(), StoreError> {
    *phase = next;
    #[cfg(test)]
    if !building_schema_reference() && take_injected(next) {
        return Err(StoreError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_PROTOCOL),
            None,
        )));
    }
    Ok(())
}

/// Whether `error` is SQLite's locking-protocol refusal.
pub(super) fn is_locking_protocol(error: &StoreError) -> bool {
    matches!(
        error,
        StoreError::Sqlite(source)
            if source.sqlite_error_code() == Some(rusqlite::ErrorCode::FileLockingProtocolFailed)
    )
}

/// Ends an attempt that met the refusal in `phase`, rolling back the
/// transaction it had open, so the connection holds no lock and no snapshot.
///
/// # Errors
///
/// Returns the refusal, naming the rollback's own failure, when the rollback
/// fails; the connection is then not used again.
pub(super) fn end_attempt(connection: &Connection, phase: OpenPhase) -> Result<(), StoreError> {
    if connection.is_autocommit() {
        return Ok(());
    }
    connection
        .execute_batch("ROLLBACK;")
        .map_err(|rollback| rollback_refusal(phase, &rollback))
}

/// The refusal returned when rolling back an attempt that met SQLite's
/// locking-protocol error in `phase` fails too. It keeps the locking-protocol
/// code, so it is classified as a busy store, and names both failures.
pub(super) fn rollback_refusal(phase: OpenPhase, rollback: &rusqlite::Error) -> StoreError {
    refusal(format!(
        "SQLite locking protocol failed while opening the store (phase: {}), and rolling back the attempt then failed: {rollback}",
        phase.label()
    ))
}

/// The refusal returned when SQLite's locking-protocol error ends an open's
/// last attempt in `phase`. `first` names the phase where it ended the first
/// attempt, when the open started again.
///
/// The refusal is transient: SQLite stopped waiting for another connection,
/// the attempt's open transaction was rolled back, and the store is safe to
/// open again. It keeps SQLite's locking-protocol code, so it is classified
/// as a busy store. A caller reports it and opens again later; it does not
/// loop on it, since the open has already started again where that was
/// allowed.
pub(super) fn locking_protocol_refusal(phase: OpenPhase, first: Option<OpenPhase>) -> StoreError {
    let message = match first {
        Some(first) => format!(
            "SQLite locking protocol failed while opening the store after one retry (phase: {}, first attempt in {}); open the store again",
            phase.label(),
            first.label()
        ),
        None => format!(
            "SQLite locking protocol failed while opening the store (phase: {}); open the store again",
            phase.label()
        ),
    };
    refusal(message)
}

fn refusal(message: String) -> StoreError {
    StoreError::Sqlite(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_PROTOCOL),
        Some(message),
    ))
}

#[cfg(test)]
thread_local! {
    static INJECTED: std::cell::Cell<Option<(OpenPhase, usize)>> =
        const { std::cell::Cell::new(None) };
}

#[cfg(test)]
fn take_injected(phase: OpenPhase) -> bool {
    INJECTED.with(|slot| match slot.get() {
        Some((at, remaining)) if at == phase && remaining > 0 => {
            slot.set(Some((at, remaining - 1)));
            true
        }
        _ => false,
    })
}

/// Makes this thread's store opens meet SQLite's locking-protocol refusal on
/// entering `phase`, the next `times` times, until the returned guard drops.
#[cfg(test)]
pub(crate) fn inject_locking_protocol(phase: OpenPhase, times: usize) -> InjectedProtocol {
    INJECTED.with(|slot| slot.set(Some((phase, times))));
    InjectedProtocol
}

/// Clears this thread's injected refusals when dropped.
#[cfg(test)]
pub(crate) struct InjectedProtocol;

#[cfg(test)]
impl InjectedProtocol {
    /// How many injected refusals no open has met yet.
    #[allow(
        clippy::unused_self,
        reason = "the count lives with this thread's injection, which the guard scopes"
    )]
    pub(crate) fn remaining(&self) -> usize {
        INJECTED.with(|slot| slot.get().map_or(0, |(_, remaining)| remaining))
    }
}

#[cfg(test)]
impl Drop for InjectedProtocol {
    fn drop(&mut self) {
        INJECTED.with(|slot| slot.set(None));
    }
}
