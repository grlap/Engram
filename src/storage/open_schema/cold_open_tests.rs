//! Several first opens of a new store at once: each decides the store is new
//! before it takes the write lock, and exactly one initializes it.

use std::sync::Arc;

use crate::storage::{
    ColdOpenGate, FAIL_COLD_SCHEMA_BEFORE_COMMIT, SqliteStore, StoreError, before_cold_lock,
    between_work_schema_probes, hold_cold_open_at,
};
use rusqlite::Connection;

fn count(connection: &Connection, sql: &str) -> i64 {
    connection
        .query_row(sql, [], |row| row.get(0))
        .expect("count rows")
}

/// Opens `database` from `openers` threads, every one held after it has
/// decided the store is new and before it takes the write lock, so all of
/// them decide before any initializes.
fn open_together(database: &std::path::Path, openers: usize) -> Vec<Result<(), StoreError>> {
    let gate = ColdOpenGate::new(openers);
    std::thread::scope(|scope| {
        let handles = (0..openers)
            .map(|_| {
                let gate = Arc::clone(&gate);
                scope.spawn(move || {
                    hold_cold_open_at(gate);
                    SqliteStore::open(database).map(drop)
                })
            })
            .collect::<Vec<_>>();
        handles
            .into_iter()
            .map(|handle| handle.join().expect("opener thread"))
            .collect()
    })
}

/// Two and three openers that all decided the store was new: every open
/// succeeds, the store holds one initial policy, and its work schema came
/// with it.
#[test]
fn first_opens_of_a_new_store_together_all_succeed() {
    for openers in [2, 3] {
        let directory = crate::test_support::temp_home().expect("temp home");
        let database = directory.path().join("work.sqlite3");
        for result in open_together(&database, openers) {
            result.unwrap_or_else(|error| panic!("{openers} openers: {error}"));
        }
        let raw = Connection::open(&database).expect("inspect store");
        assert_eq!(
            count(&raw, "SELECT COUNT(*) FROM control_policy_versions"),
            1,
            "one initial policy with {openers} openers"
        );
        assert_eq!(count(&raw, "SELECT COUNT(*) FROM control_policy_state"), 1);
        assert_eq!(count(&raw, "SELECT COUNT(*) FROM work_schema_metadata"), 1);
        drop(raw);
        drop(SqliteStore::open(&database).expect("a later open"));
    }
}

/// Another opener commits the whole new store between two of this open's
/// probes: after the one that finds no work schema marker and before the one
/// that counts work tables. The probes read one snapshot, so they agree that
/// the store is new; the open then finds it initialized under its write lock,
/// starts again once and opens it, instead of refusing it as a different build
/// because one probe saw an empty store and the next a complete one. The file
/// starts empty but in write-ahead-log mode, so the other opener can commit
/// while this open's snapshot is read.
#[test]
fn a_store_initialized_between_the_probes_opens_after_one_restart() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("work.sqlite3");
    let mode: String = Connection::open(&database)
        .expect("empty store")
        .query_row("PRAGMA journal_mode = WAL", [], |row| row.get(0))
        .expect("write-ahead log");
    assert_eq!(mode, "wal");
    let other = database.clone();
    between_work_schema_probes(move || {
        drop(SqliteStore::open(&other).expect("the other opener initializes the store"));
    });
    drop(SqliteStore::open(&database).expect("the open sees one consistent store"));
    let raw = Connection::open(&database).expect("inspect store");
    assert_eq!(
        count(&raw, "SELECT COUNT(*) FROM control_policy_versions"),
        1
    );
}

/// The refusal for a second restart, which cannot happen in practice, is
/// typed as a busy store to open again, never as a corrupt one.
#[test]
fn a_second_restart_refuses_as_a_busy_store() {
    let refusal = super::cold_open_restart_refusal();
    let kind = crate::storage::store_open_refusal_kind(&refusal);
    assert_eq!(kind, crate::storage::StoreOpenRefusalKind::Busy);
    assert_ne!(kind, crate::storage::StoreOpenRefusalKind::CorruptStore);
    assert!(
        refusal
            .to_string()
            .contains("initialized by another opener twice"),
        "{refusal}"
    );
}

/// The work schema is created in the transaction that creates the core
/// schema: a failure before that commit leaves neither, so no opener can see
/// one without the other; the next open initializes the store whole.
#[test]
fn a_new_store_commits_its_core_and_work_schemas_together() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("work.sqlite3");
    FAIL_COLD_SCHEMA_BEFORE_COMMIT.set(true);
    let refused = SqliteStore::open(&database)
        .map(drop)
        .expect_err("the injected failure refuses");
    assert!(
        refused
            .to_string()
            .contains("injected cold-schema failure before commit"),
        "{refused}"
    );
    let raw = Connection::open(&database).expect("inspect rolled-back store");
    for table in [
        "objects",
        "control_policy_versions",
        "work_schema_metadata",
        "work_items",
    ] {
        assert!(
            !SqliteStore::sqlite_table_exists(&raw, table).expect("inspect table"),
            "{table} was rolled back"
        );
    }
    drop(raw);
    drop(SqliteStore::open(&database).expect("a whole initialization"));
    let raw = Connection::open(&database).expect("inspect store");
    assert_eq!(count(&raw, "SELECT COUNT(*) FROM work_schema_metadata"), 1);
}

/// An opener that decided the store was new, and finds under the lock that
/// something other than Engram wrote a table meanwhile, starts again and is
/// refused as a different build: the restart never masks a store that does
/// not match. The same file opened afterwards is refused the same way.
#[test]
fn a_store_changed_to_a_foreign_schema_meanwhile_is_refused_as_a_different_build() {
    let directory = crate::test_support::temp_home().expect("temp home");
    let database = directory.path().join("work.sqlite3");
    let foreign = database.clone();
    before_cold_lock(move || {
        Connection::open(&foreign)
            .expect("foreign writer")
            .execute_batch("CREATE TABLE foreign_records (value TEXT);")
            .expect("foreign table");
    });
    let result = SqliteStore::open(&database).map(drop);
    assert!(
        matches!(result, Err(StoreError::DifferentBuildSchema)),
        "{result:?}"
    );
    assert!(matches!(
        SqliteStore::open(&database).map(drop),
        Err(StoreError::DifferentBuildSchema)
    ));
}
