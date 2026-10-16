use std::{cell::Cell, time::Duration};

use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};

use super::*;

fn busy_error(code: i32) -> StoreError {
    StoreError::Sqlite(rusqlite::Error::SqliteFailure(
        rusqlite::ffi::Error::new(code),
        Some("private statement detail".into()),
    ))
}

fn row_count(connection: &Connection) -> i64 {
    connection
        .query_row("SELECT count(*) FROM payload", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn control_store_busy_precommit_failure_requires_successful_rollback() {
    for code in [
        rusqlite::ffi::SQLITE_BUSY,
        rusqlite::ffi::SQLITE_BUSY_SNAPSHOT,
        rusqlite::ffi::SQLITE_LOCKED,
        rusqlite::ffi::SQLITE_LOCKED_SHAREDCACHE,
    ] {
        let mut connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("CREATE TABLE payload (value INTEGER)")
            .unwrap();
        let error = control_transaction::<()>(&mut connection, |transaction| {
            transaction.execute("INSERT INTO payload VALUES (1)", [])?;
            Err(busy_error(code))
        })
        .unwrap_err();
        assert!(connection.is_autocommit());
        assert_eq!(row_count(&connection), 0);
        let StoreError::ControlStoreBusy { source } = &error else {
            panic!("expected certified contention, got {error:?}");
        };
        let rusqlite::Error::SqliteFailure(native, detail) = source.as_ref() else {
            panic!("lost SQLite source");
        };
        assert_eq!(native.extended_code, code);
        assert_eq!(detail.as_deref(), Some("private statement detail"));
        assert_eq!(crate::host::store_error_code(&error), "control_store_busy");
        assert_eq!(
            crate::store_error_value(&error)["error"]["code"],
            "engram_store_error"
        );
        assert!(!error.to_string().contains("private statement detail"));
    }
}

#[test]
fn control_store_busy_rollback_failure_keeps_generic_storage_error() {
    let mut connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch("CREATE TABLE payload (value INTEGER)")
        .unwrap();
    connection
        .authorizer(Some(|context: AuthContext<'_>| {
            if matches!(
                context.action,
                AuthAction::Transaction {
                    operation: TransactionOperation::Rollback
                }
            ) {
                Authorization::Deny
            } else {
                Authorization::Allow
            }
        }))
        .unwrap();
    let error = control_transaction::<()>(&mut connection, |transaction| {
        transaction.execute("INSERT INTO payload VALUES (1)", [])?;
        Err(busy_error(rusqlite::ffi::SQLITE_BUSY))
    })
    .unwrap_err();
    connection
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .unwrap();
    assert_eq!(crate::host::store_error_code(&error), "storage_error");
    assert!(matches!(error, StoreError::Sqlite(_)));
    assert!(!connection.is_autocommit());
    connection.execute_batch("ROLLBACK").unwrap();
    assert!(connection.is_autocommit());
    assert_eq!(row_count(&connection), 0);
}

#[test]
fn control_store_busy_real_commit_contention_keeps_generic_storage_error() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("commit.db");
    let mut connection = Connection::open(&database).unwrap();
    connection.busy_timeout(Duration::ZERO).unwrap();
    connection
        .execute_batch(
            "PRAGMA journal_mode=DELETE; PRAGMA cache_spill=OFF;
         CREATE TABLE payload (value INTEGER);",
        )
        .unwrap();
    let reader = Connection::open(&database).unwrap();
    reader
        .execute_batch("BEGIN; SELECT value FROM payload;")
        .unwrap();
    let body_completed = Cell::new(false);
    let error = control_transaction(&mut connection, |transaction| {
        transaction.execute("INSERT INTO payload VALUES (1)", [])?;
        body_completed.set(true);
        Ok(())
    })
    .unwrap_err();
    assert!(body_completed.get(), "the write must reach COMMIT");
    assert!(matches!(
        &error,
        StoreError::Sqlite(source)
            if source.sqlite_error_code() == Some(rusqlite::ErrorCode::DatabaseBusy)
    ));
    assert_eq!(crate::host::store_error_code(&error), "storage_error");
    reader.execute_batch("ROLLBACK").unwrap();
    assert!(connection.is_autocommit());
    assert_eq!(row_count(&connection), 0);
    control_transaction(&mut connection, |transaction| {
        transaction.execute("INSERT INTO payload VALUES (1)", [])?;
        Ok(())
    })
    .unwrap();
    assert_eq!(row_count(&connection), 1);
}
