use std::cell::Cell;
use std::sync::{
    Arc,
    atomic::{AtomicUsize, Ordering},
};
use std::time::Instant;

use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};

use super::super::request_budget::{CONTROL_REQUEST_ALLOWANCE, ControlRequestScope};
use super::*;

thread_local! {
    static CLOCK: Cell<Instant> = Cell::new(Instant::now());
}

fn now() -> Instant {
    CLOCK.with(Cell::get)
}
fn expire() {
    CLOCK.with(|clock| clock.set(clock.get() + CONTROL_REQUEST_ALLOWANCE));
}

fn fixture() -> (Connection, ControlRequestScope) {
    CLOCK.with(|clock| clock.set(Instant::now()));
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch("CREATE TABLE payload (value INTEGER)")
        .unwrap();
    connection.busy_timeout(Duration::from_secs(20)).unwrap();
    (connection, ControlRequestScope::with_clock(now))
}

fn count(connection: &Connection) -> i64 {
    connection
        .query_row("SELECT count(*) FROM payload", [], |row| row.get(0))
        .unwrap()
}

fn timeout(connection: &Connection) -> u32 {
    connection
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn expired_request_never_enters_begin_or_the_body() {
    let (mut connection, _scope) = fixture();
    expire();
    let called = Cell::new(false);
    let error = control_transaction(&mut connection, |_| {
        called.set(true);
        Ok(())
    })
    .unwrap_err();
    assert!(matches!(error, StoreError::ControlRequestExpired));
    assert!(!called.get());
    assert!(connection.is_autocommit());
    assert_eq!(count(&connection), 0);
    assert_eq!(timeout(&connection), 20_000);
}

#[test]
fn delayed_begin_rolls_back_before_the_body_and_restores_timeout() {
    let (mut connection, _scope) = fixture();
    let rollbacks = Arc::new(AtomicUsize::new(0));
    let seen = rollbacks.clone();
    connection
        .authorizer(Some(move |context: AuthContext<'_>| {
            match context.action {
                AuthAction::Transaction {
                    operation: TransactionOperation::Begin,
                } => expire(),
                AuthAction::Transaction {
                    operation: TransactionOperation::Rollback,
                } => {
                    seen.fetch_add(1, Ordering::SeqCst);
                }
                _ => {}
            }
            Authorization::Allow
        }))
        .unwrap();
    let called = Cell::new(false);
    let error = control_transaction(&mut connection, |_| {
        called.set(true);
        Ok(())
    })
    .unwrap_err();
    assert!(matches!(error, StoreError::ControlRequestExpired));
    assert!(!called.get());
    assert_eq!(rollbacks.load(Ordering::SeqCst), 1);
    assert!(connection.is_autocommit());
    assert_eq!(timeout(&connection), 20_000);
}

#[test]
fn expired_body_rolls_back_its_write_and_the_same_connection_can_retry() {
    let (mut connection, _scope) = fixture();
    let error = control_transaction(&mut connection, |transaction| {
        assert_eq!(timeout(transaction), 15_000);
        transaction.execute("INSERT INTO payload VALUES (1)", [])?;
        expire();
        Ok(())
    })
    .unwrap_err();
    assert!(matches!(error, StoreError::ControlRequestExpired));
    assert_eq!(count(&connection), 0);
    assert!(connection.is_autocommit());
    assert_eq!(timeout(&connection), 20_000);
    let _fresh = ControlRequestScope::with_clock(now);
    control_transaction(&mut connection, |transaction| {
        transaction.execute("INSERT INTO payload VALUES (1)", [])?;
        Ok(())
    })
    .unwrap();
    assert_eq!(count(&connection), 1);
    assert_eq!(timeout(&connection), 20_000);
}

#[test]
fn expired_body_with_failed_rollback_is_not_certified() {
    let (mut connection, _scope) = fixture();
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
    let error = control_transaction(&mut connection, |transaction| {
        transaction.execute("INSERT INTO payload VALUES (1)", [])?;
        expire();
        Ok(())
    })
    .unwrap_err();
    assert_eq!(crate::host::store_error_code(&error), "storage_error");
    assert!(!connection.is_autocommit());
    assert_eq!(timeout(&connection), 20_000);
    connection
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .unwrap();
    connection.execute_batch("ROLLBACK").unwrap();
    assert_eq!(count(&connection), 0);
}

#[test]
fn commit_that_finishes_after_the_allowance_remains_successful() {
    let (mut connection, _scope) = fixture();
    connection
        .commit_hook(Some(|| {
            expire();
            false
        }))
        .unwrap();
    let result = control_transaction(&mut connection, |transaction| {
        transaction.execute("INSERT INTO payload VALUES (1)", [])?;
        Ok("settled")
    })
    .unwrap();
    assert_eq!(result, "settled");
    assert_eq!(count(&connection), 1);
    assert_eq!(timeout(&connection), 20_000);
}

#[test]
fn unwind_restores_the_busy_policy_and_discards_uncommitted_changes() {
    let (mut connection, _scope) = fixture();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _ = control_transaction::<()>(&mut connection, |transaction| {
            transaction.execute("INSERT INTO payload VALUES (1)", [])?;
            panic!("injected body panic");
        });
    }));
    assert!(result.is_err());
    assert!(connection.is_autocommit());
    assert_eq!(count(&connection), 0);
    assert_eq!(timeout(&connection), 20_000);
}
