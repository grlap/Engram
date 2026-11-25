//! A store open that meets SQLite's locking-protocol refusal rolls back its
//! attempt and starts again from the top until its own lock wait ends; only
//! then is the refusal returned. It names its phase and is classified as a
//! busy store.

use rusqlite::Connection;

use super::{OpenPhase, inject_locking_protocol};
use crate::storage::{
    CopyInterrupt, HostPathPolicy, SqliteStore, StoreError, StoreOpenRefusalKind,
    between_work_schema_probes, store_open_refusal_kind,
};

fn count(connection: &Connection, sql: &str) -> i64 {
    connection
        .query_row(sql, [], |row| row.get(0))
        .expect("count rows")
}

/// Asserts that `refusal` is the locking-protocol refusal for `phase`, after
/// starting again from a first attempt that ended in `first`, or after none,
/// and is classified as a busy store.
fn assert_refusal(refusal: &StoreError, phase: OpenPhase, first: Option<OpenPhase>) {
    let text = refusal.to_string();
    assert!(
        text.contains(&format!("(phase: {}", phase.label())),
        "{text}"
    );
    assert_eq!(text.contains("starting again"), first.is_some(), "{text}");
    if let Some(first) = first {
        assert!(
            text.contains(&format!("first attempt in {})", first.label())),
            "{text}"
        );
    }
    assert_eq!(store_open_refusal_kind(refusal), StoreOpenRefusalKind::Busy);
}

/// The refusal met once, in any step of a new store's open, ends that
/// attempt; the open starts again and initializes the store whole.
#[test]
fn a_refusal_met_once_while_a_new_store_opens_is_recovered() {
    for phase in [
        OpenPhase::ProbeSnapshot,
        OpenPhase::ConnectionSettings,
        OpenPhase::JournalQuery,
        OpenPhase::JournalSwitch,
        OpenPhase::ColdInitialization,
        OpenPhase::StoreSetup,
    ] {
        let directory = crate::test_support::temp_home().expect("temp home");
        let database = directory.path().join("work.sqlite3");
        let injected = inject_locking_protocol(phase, 1);
        drop(
            SqliteStore::open(&database)
                .unwrap_or_else(|error| panic!("{}: {error}", phase.label())),
        );
        assert_eq!(injected.remaining(), 0, "{} was met", phase.label());
        drop(injected);
        let raw = Connection::open(&database).expect("inspect store");
        assert_eq!(
            count(&raw, "SELECT COUNT(*) FROM control_policy_versions"),
            1,
            "{}",
            phase.label()
        );
        assert_eq!(count(&raw, "SELECT COUNT(*) FROM work_schema_metadata"), 1);
        let mode: String = raw
            .query_row("PRAGMA journal_mode", [], |row| row.get(0))
            .expect("journal mode");
        assert_eq!(mode, "wal", "{}", phase.label());
    }
}

/// The refusal met once while an initialized store settles is recovered too.
#[test]
fn a_refusal_met_once_while_an_initialized_store_opens_is_recovered() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("work.sqlite3");
    drop(SqliteStore::open(&database).expect("initialize"));
    let injected = inject_locking_protocol(OpenPhase::StoreSetup, 1);
    drop(SqliteStore::open(&database).expect("reopen"));
    assert_eq!(injected.remaining(), 0);
}

/// Met again and again while its lock wait lasts, the refusal does not end the
/// open: it starts again each time, here three times, and then initializes
/// the store whole.
#[test]
fn a_refusal_met_again_and_again_within_the_wait_is_recovered() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("work.sqlite3");
    let injected = inject_locking_protocol(OpenPhase::ColdInitialization, 3);
    drop(SqliteStore::open(&database).expect("opened after starting again three times"));
    assert_eq!(injected.remaining(), 0, "all three refusals were met");
    drop(injected);
    let raw = Connection::open(&database).expect("inspect store");
    assert_eq!(
        count(&raw, "SELECT COUNT(*) FROM control_policy_versions"),
        1
    );
    assert_eq!(count(&raw, "SELECT COUNT(*) FROM work_schema_metadata"), 1);
}

/// A refusal that keeps coming ends the open only once its lock wait has
/// ended, never before; how many attempts fit in the wait depends on the
/// host. The refusal names the step it ended, and the first attempt's too when
/// the open started again. The rolled-back attempts leave no part of a new
/// store, and a later open initializes it whole.
#[test]
fn a_refusal_that_keeps_coming_ends_the_open_only_once_its_wait_has_ended() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("work.sqlite3");
    let wait = std::time::Duration::from_millis(200);
    let injected = inject_locking_protocol(OpenPhase::ColdInitialization, usize::MAX);
    let started = std::time::Instant::now();
    let refusal = super::super::lock_wait::with_store_lock_wait(wait, || {
        SqliteStore::open(&database).map(drop)
    })
    .expect_err("refused once the wait has ended");
    assert!(started.elapsed() >= wait, "refused before its wait ended");
    let started_again = injected.remaining() < usize::MAX - 1;
    drop(injected);
    assert_refusal(
        &refusal,
        OpenPhase::ColdInitialization,
        started_again.then_some(OpenPhase::ColdInitialization),
    );
    let raw = Connection::open(&database).expect("inspect rolled-back store");
    for table in ["objects", "control_policy_versions", "work_schema_metadata"] {
        assert!(
            !SqliteStore::sqlite_table_exists(&raw, table).expect("inspect table"),
            "{table} was rolled back"
        );
    }
    drop(raw);
    drop(SqliteStore::open(&database).expect("a later open"));
}

/// A control host's open, inside its call's allowance, starts again after the
/// refusal while that allowance lasts, rather than ending the call at SQLite's
/// first ten seconds.
#[test]
fn a_control_host_open_starts_again_while_its_time_lasts() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("work.sqlite3");
    let _call = crate::storage::ControlRequestScope::frame();
    let injected = inject_locking_protocol(OpenPhase::JournalSwitch, 2);
    drop(
        SqliteStore::open_for_control_host(&database, Some(HostPathPolicy::host_default()))
            .expect("opened after starting again"),
    );
    assert_eq!(injected.remaining(), 0, "both refusals were met");
}

/// A store copy's open of the source starts again too, within the copy's own
/// deadline.
#[test]
fn a_store_copy_starts_again_while_its_deadline_lasts() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("work.sqlite3");
    drop(SqliteStore::open(&database).expect("initialize"));
    let target = directory.path().join("copy.sqlite3");
    let injected = inject_locking_protocol(OpenPhase::ProbeSnapshot, 2);
    SqliteStore::copy_existing_read_only(
        &database,
        &target,
        &CopyInterrupt::after(std::time::Duration::from_secs(60)),
    )
    .expect("copied after starting again");
    assert_eq!(injected.remaining(), 0, "both refusals were met");
}

#[test]
fn a_failed_rollback_still_refuses_as_a_busy_store() {
    let rollback = rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR),
        Some("disk I/O error".into()),
    );
    let refusal = super::rollback_refusal(OpenPhase::ColdInitialization, &rollback);
    let text = refusal.to_string();
    assert!(text.contains("(phase: cold_initialization)"), "{text}");
    assert!(text.contains("disk I/O error"), "{text}");
    assert_eq!(
        store_open_refusal_kind(&refusal),
        StoreOpenRefusalKind::Busy
    );
}

/// The retry and the restart after another opener initialized the store keep
/// separate allowances: one open may use both.
#[test]
fn a_retry_and_a_restart_keep_separate_allowances() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("work.sqlite3");
    let mode: String = Connection::open(&database)
        .expect("empty store")
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .expect("write-ahead log");
    assert_eq!(mode, "wal");
    let other = database.clone();
    // The first attempt meets the refusal before its work-schema probes; the
    // second finds the store initialized meanwhile and restarts; the third
    // opens it.
    let injected = inject_locking_protocol(OpenPhase::ProbeSnapshot, 1);
    between_work_schema_probes(move || {
        drop(SqliteStore::open(&other).expect("the other opener initializes the store"));
    });
    drop(SqliteStore::open(&database).expect("both allowances used"));
    assert_eq!(injected.remaining(), 0);
    let raw = Connection::open(&database).expect("inspect store");
    assert_eq!(
        count(&raw, "SELECT COUNT(*) FROM control_policy_versions"),
        1
    );
}
