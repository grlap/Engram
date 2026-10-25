use super::*;
use crate::StoreError;
use crate::storage::control_runtime::{control_lifecycle_transaction, control_transaction};
use rusqlite::{
    Connection,
    hooks::{AuthAction, AuthContext, Authorization, TransactionOperation},
};

fn connection() -> Connection {
    let connection = Connection::open_in_memory().unwrap();
    connection
        .execute_batch("CREATE TABLE payload(value INTEGER)")
        .unwrap();
    connection
}

fn hooks(connection: &Connection, deny: Option<TransactionOperation>) {
    connection
        .authorizer(Some(move |context: AuthContext<'_>| {
            if let AuthAction::Transaction { operation } = context.action {
                let delay = match operation {
                    TransactionOperation::Begin => 3,
                    TransactionOperation::Rollback => 7,
                    _ => 0,
                };
                test_clock::advance(Duration::from_millis(delay));
                if Some(operation) == deny {
                    return Authorization::Deny;
                }
            }
            Authorization::Allow
        }))
        .unwrap();
    connection
        .commit_hook(Some(|| {
            test_clock::advance(Duration::from_millis(5));
            false
        }))
        .unwrap();
}

#[test]
fn control_diagnostic_timing_aggregates_both_transaction_wrappers() {
    let _clock = test_clock::start();
    let scope = ControlDiagnosticScope::enter();
    let mut connection = connection();
    hooks(&connection, None);
    for lifecycle in [false, true] {
        let apply = |transaction: &rusqlite::Transaction<'_>| {
            test_clock::advance(Duration::from_millis(11));
            transaction.execute("INSERT INTO payload VALUES(1)", [])?;
            Ok(())
        };
        if lifecycle {
            control_lifecycle_transaction(&mut connection, apply).unwrap();
        } else {
            control_transaction(&mut connection, apply).unwrap();
        }
    }
    assert_eq!(
        scope.snapshot(),
        ControlWriteTiming {
            attempted: 2,
            acquired: 2,
            wait: Duration::from_millis(6),
            hold: Duration::from_millis(32),
        }
    );
    assert!(connection.is_autocommit());
}

#[test]
fn control_diagnostic_timing_failed_begin_has_no_acquired_interval() {
    let _clock = test_clock::start();
    let scope = ControlDiagnosticScope::enter();
    let mut connection = connection();
    hooks(&connection, Some(TransactionOperation::Begin));
    control_transaction::<()>(&mut connection, |_| panic!("BEGIN was refused")).unwrap_err();
    assert_eq!(
        scope.snapshot(),
        ControlWriteTiming {
            attempted: 1,
            acquired: 0,
            wait: Duration::from_millis(3),
            hold: Duration::ZERO,
        }
    );
    assert!(connection.is_autocommit());
}

#[test]
fn control_diagnostic_timing_includes_explicit_implicit_and_unwind_rollback() {
    for kind in [0, 1, 2] {
        let _clock = test_clock::start();
        let scope = ControlDiagnosticScope::enter();
        let mut connection = connection();
        hooks(&connection, None);
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            control_transaction::<()>(&mut connection, |transaction| {
                transaction.execute("INSERT INTO payload VALUES(1)", [])?;
                test_clock::advance(Duration::from_millis(11));
                match kind {
                    0 => Err(StoreError::ControlRequestExpired), // explicit rollback
                    1 => Err(StoreError::InvalidWork("private".into())), // Drop rollback
                    _ => panic!("unwind through transaction cleanup"),
                }
            })
        }));
        if kind == 2 {
            assert!(result.is_err());
        } else {
            assert!(result.unwrap().is_err());
        }
        assert_eq!(
            scope.snapshot(),
            ControlWriteTiming {
                attempted: 1,
                acquired: 1,
                wait: Duration::from_millis(3),
                hold: Duration::from_millis(18),
            }
        );
        assert!(connection.is_autocommit());
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM payload", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }
}

#[test]
fn control_diagnostic_timing_failed_commit_is_timed_and_settled() {
    let _clock = test_clock::start();
    let scope = ControlDiagnosticScope::enter();
    let mut connection = connection();
    hooks(&connection, None);
    connection
        .commit_hook(Some(|| {
            test_clock::advance(Duration::from_millis(5));
            true // SQLite rolls back a commit vetoed by its hook.
        }))
        .unwrap();
    control_transaction(&mut connection, |transaction| {
        transaction.execute("INSERT INTO payload VALUES(1)", [])?;
        test_clock::advance(Duration::from_millis(11));
        Ok(())
    })
    .unwrap_err();
    assert_eq!(
        scope.snapshot(),
        ControlWriteTiming {
            attempted: 1,
            acquired: 1,
            wait: Duration::from_millis(3),
            hold: Duration::from_millis(16),
        }
    );
    assert!(connection.is_autocommit());
    assert_eq!(
        connection
            .query_row("SELECT count(*) FROM payload", [], |row| row
                .get::<_, i64>(0))
            .unwrap(),
        0
    );
}

#[test]
fn control_diagnostic_scope_restores_on_unwind_and_is_thread_local() {
    assert!(Observation::start().is_none());
    let _clock = test_clock::start();
    let outer = ControlDiagnosticScope::enter();
    let mut connection = connection();
    hooks(&connection, None);
    control_transaction(&mut connection, |_| Ok(())).unwrap();
    let before = outer.snapshot();
    assert!(
        std::panic::catch_unwind(|| {
            let inner = ControlDiagnosticScope::enter();
            assert_eq!(inner.snapshot(), ControlWriteTiming::default());
            panic!("restore observer");
        })
        .is_err()
    );
    assert_eq!(outer.snapshot(), before);
    std::thread::spawn(|| assert!(Observation::start().is_none()))
        .join()
        .unwrap();
    drop(outer);
    assert!(Observation::start().is_none());
    assert_eq!(
        ControlDiagnosticScope::enter().snapshot(),
        ControlWriteTiming::default()
    );
}
