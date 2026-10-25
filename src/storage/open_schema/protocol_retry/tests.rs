//! A store open that meets SQLite's locking-protocol refusal rolls back its
//! attempt and starts again from the top once, or returns the refusal at once
//! where its opener has a deadline of its own; the refusal names its phase
//! and is classified as a busy store.

use rusqlite::Connection;

use super::{OpenPhase, inject_locking_protocol};
use crate::domain::ControlAssurance;
use crate::storage::test_support::open_with_assurance;
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
/// one retry whose first attempt ended in `first`, or after none, and is
/// classified as a busy store.
fn assert_refusal(refusal: &StoreError, phase: OpenPhase, first: Option<OpenPhase>) {
    let text = refusal.to_string();
    assert!(
        text.contains(&format!("(phase: {}", phase.label())),
        "{text}"
    );
    assert_eq!(text.contains("after one retry"), first.is_some(), "{text}");
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

/// A new store's bootstrap policy survives a retry. When the refusal ends the
/// first attempt before its initialization commits, the attempt that starts
/// again installs the requested assurance, not the default. When it ends the
/// first attempt after that commit, the attempt that starts again admits the
/// store the first one initialized, with the same requested assurance.
#[test]
fn a_recovered_open_installs_the_requested_bootstrap_policy() {
    for phase in [OpenPhase::ColdInitialization, OpenPhase::StoreSetup] {
        let directory = crate::test_support::temp_home().expect("temp home");
        let database = directory.path().join("work.sqlite3");
        let injected = inject_locking_protocol(phase, 1);
        drop(
            open_with_assurance(&database, ControlAssurance::ActionGated)
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
        drop(raw);
        drop(open_with_assurance(&database, ControlAssurance::ActionGated).expect("same policy"));
        assert!(
            open_with_assurance(&database, ControlAssurance::Advisory).is_err(),
            "{}: the store holds the requested assurance, not another",
            phase.label()
        );
    }
}

/// Met again in the attempt that started again, the refusal ends the open:
/// one retry, never a third attempt. The refusal names where it ended both
/// attempts. The rolled-back attempts leave no part of a new store, and a
/// later open initializes it whole.
#[test]
fn a_second_refusal_ends_the_open_after_one_retry() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("work.sqlite3");
    let injected = inject_locking_protocol(OpenPhase::ColdInitialization, 3);
    let refusal = SqliteStore::open(&database)
        .map(drop)
        .expect_err("refused after one retry");
    assert_eq!(injected.remaining(), 1, "two attempts, no third");
    drop(injected);
    assert_refusal(
        &refusal,
        OpenPhase::ColdInitialization,
        Some(OpenPhase::ColdInitialization),
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

/// A control host's open is bounded by its host's deadline, so it returns the
/// refusal at once instead of starting again.
#[test]
fn a_control_host_open_returns_the_refusal_without_a_retry() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("work.sqlite3");
    let injected = inject_locking_protocol(OpenPhase::JournalSwitch, 2);
    let refusal =
        SqliteStore::open_for_control_host(&database, Some(HostPathPolicy::host_default()))
            .map(drop)
            .expect_err("refused at once");
    assert_eq!(injected.remaining(), 1, "one attempt only");
    drop(injected);
    assert_refusal(&refusal, OpenPhase::JournalSwitch, None);
    drop(
        SqliteStore::open_for_control_host(&database, Some(HostPathPolicy::host_default()))
            .expect("a later open"),
    );
}

/// A store copy runs under its own deadline, so its open of the source
/// returns the refusal at once too.
#[test]
fn a_store_copy_returns_the_refusal_without_a_retry() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("work.sqlite3");
    drop(SqliteStore::open(&database).expect("initialize"));
    let target = directory.path().join("copy.sqlite3");
    let injected = inject_locking_protocol(OpenPhase::ProbeSnapshot, 2);
    let refusal = SqliteStore::copy_existing_read_only(
        &database,
        &target,
        &CopyInterrupt::after(std::time::Duration::from_secs(60)),
    )
    .expect_err("refused at once");
    assert_eq!(injected.remaining(), 1, "one attempt only");
    assert_refusal(&refusal, OpenPhase::ProbeSnapshot, None);
}

/// A host runs a control-session inspection under its own deadline, so the
/// inspection's read-only open returns the refusal at once too.
#[test]
fn a_control_session_inspection_returns_the_refusal_without_a_retry() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("work.sqlite3");
    drop(SqliteStore::open(&database).expect("initialize"));
    let injected = inject_locking_protocol(OpenPhase::ProbeSnapshot, 2);
    let refusal = SqliteStore::inspect_control_session(
        &database,
        Some(HostPathPolicy::host_default()),
        "session",
        "grant",
        &crate::ProjectId("project".into()),
    )
    .map(drop)
    .expect_err("refused at once");
    assert_eq!(injected.remaining(), 1, "one attempt only");
    assert_refusal(&refusal, OpenPhase::ProbeSnapshot, None);
}

/// When rolling back the ended attempt fails too, the refusal names both
/// failures and is still classified as a busy store.
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
