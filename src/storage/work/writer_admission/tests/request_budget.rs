//! Phase time is injected; no wall-clock contention threshold decides the test.

use super::*;
use crate::storage::ControlRequestScope;
use std::cell::Cell;

thread_local! {
    static CLOCK: Cell<Instant> = Cell::new(Instant::now());
}

fn now() -> Instant {
    CLOCK.with(Cell::get)
}

fn advance(seconds: u64) {
    CLOCK.with(|clock| clock.set(clock.get() + Duration::from_secs(seconds)));
}

#[test]
fn opener_and_writer_spend_one_request_budget_and_expiry_rolls_back() {
    let home = crate::test_support::temp_home().unwrap();
    let database = home.path().join("composed.sqlite3");
    drop(SqliteStore::open(&database).unwrap());
    let _request = ControlRequestScope::with_clock(now);
    // Stage the opener phase at the real sidecar acquisition boundary. The
    // operation still has twenty seconds when the opened store is returned.
    crate::storage::BEFORE_OPEN_LOCK.with(|slot| {
        *slot.borrow_mut() = Some(Box::new(|| advance(40)));
    });
    let mut store = SqliteStore::open(&database).unwrap();
    store
        .connection
        .execute_batch("CREATE TEMP TABLE payload(value)")
        .unwrap();
    with_writer_admission_test_policy(
        None,
        || advance(15),
        || {
            let transaction = store.begin_work_mutation().unwrap();
            assert_eq!(
                last_writer_admission_allowance(),
                Some(Duration::from_secs(5)),
                "writer admission must spend the budget left after the opener, without resetting it"
            );
            transaction
                .execute("INSERT INTO payload VALUES (1)", [])
                .unwrap();
            advance(5);
            assert!(
                transaction.commit().is_err(),
                "expiry after the opener and writer must roll back the transaction"
            );
        },
    );
    assert!(store.connection.is_autocommit());
    let count: i64 = store
        .connection
        .query_row("SELECT count(*) FROM payload", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 0);
    // A fresh invocation gets a fresh allowance; the expired scope cannot
    // reset itself merely because it reaches another acquisition.
    assert!(store.begin_work_mutation().is_err());
    let _retry = ControlRequestScope::with_clock(now);
    let transaction = store.begin_work_mutation().unwrap();
    transaction
        .execute("INSERT INTO payload VALUES (1)", [])
        .unwrap();
    transaction.commit().unwrap();
    let count: i64 = store
        .connection
        .query_row("SELECT count(*) FROM payload", [], |row| row.get(0))
        .unwrap();
    assert_eq!(count, 1);
}

#[test]
fn a_blocked_opener_warns_once_and_finishes_without_an_observation_timeout() {
    let home = crate::test_support::temp_home().unwrap();
    let database = home.path().join("warned.sqlite3");
    let probe = std::rc::Rc::new(crate::operation_time::test_clock::Probe::install());
    let warning = crate::operation_time::Warning::start("store open operation");
    let inside = std::rc::Rc::clone(&probe);
    crate::storage::before_next_open_lock(move || {
        assert!(inside.active("store open operation"));
        inside.advance(Duration::from_secs(10));
        inside.advance(Duration::from_secs(20));
        assert!(inside.active("store open operation"));
    });
    let store = SqliteStore::open(&database).unwrap();
    drop(warning);
    assert!(store.connection.is_autocommit());
    let events = probe.events();
    assert_eq!(events.len(), 1);
    assert_eq!(events[0].0, "store open operation");
    assert_eq!(events[0].2, Duration::from_secs(10));
    assert!(!probe.active("store open operation"));
    probe.advance(Duration::from_secs(60));
    assert_eq!(probe.events(), events);
}
