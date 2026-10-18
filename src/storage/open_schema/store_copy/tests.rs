use super::*;
use crate::storage::copy_trace;

#[test]
fn copy_boundary_expiry_releases_the_connection_without_starting_later_work() {
    for point in [
        CopyProbePoint::Opened,
        CopyProbePoint::BeforeAdmission,
        CopyProbePoint::BeforeCopy,
    ] {
        let directory = crate::test_support::temp_home().unwrap();
        let source = directory.path().join("engram.db");
        drop(SqliteStore::open(&source).unwrap());
        let target = directory.path().join("copy.db");
        let interrupt =
            CopyInterrupt::after(Duration::MAX).with_probe(Arc::new(move |at| at == point));
        copy_trace::start();
        let error = SqliteStore::copy_existing_read_only(&source, &target, &interrupt)
            .expect_err("the selected boundary fires the interrupt");
        let trace = copy_trace::take();
        assert!(interrupt.fired(), "{point:?}");
        assert!(
            matches!(error, StoreError::InvalidWork(ref message) if message == "the copy passed its deadline"),
            "{point:?}: {error}"
        );
        assert!(!target.exists(), "{point:?}: no copy statement ran");
        assert!(
            !trace
                .iter()
                .any(|step| step.contains("copy: vacuum begins")),
            "{point:?}: {trace:?}"
        );
        if point != CopyProbePoint::BeforeCopy {
            assert!(
                !trace
                    .iter()
                    .any(|step| step.ends_with("copy: admission begins")),
                "{point:?}: {trace:?}"
            );
        }
        assert!(!super::super::lock_wait::in_copy_scope());
        assert!(trace.iter().any(|step| step.ends_with(": close begins")));
        assert!(trace.iter().any(|step| step.ends_with(": closed")));
        // A leaked read transaction would prevent this exclusive lock.
        let reopened = Connection::open(&source).unwrap();
        reopened.busy_timeout(Duration::ZERO).unwrap();
        reopened
            .execute_batch(
                "PRAGMA locking_mode = EXCLUSIVE;
                 BEGIN EXCLUSIVE;
                 CREATE TABLE release_probe(value INTEGER);
                 DROP TABLE release_probe;
                 ROLLBACK;",
            )
            .unwrap();
    }
}

#[test]
fn failed_admission_records_the_original_sqlite_error_before_disposal() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("engram.db");
    drop(SqliteStore::open(&source).unwrap());
    let holder = Connection::open(&source).unwrap();
    holder
        .execute_batch(
            "PRAGMA locking_mode = EXCLUSIVE;
             BEGIN EXCLUSIVE;
             CREATE TABLE lock_probe(value INTEGER);
             DROP TABLE lock_probe;
             COMMIT;",
        )
        .unwrap();
    let connection = SqliteStore::open_existing_read_only_connection(&source).unwrap();
    copy_trace::start();
    // Without a copy scope the callback refuses immediately, without a sleep.
    let Err(error) = SqliteStore::from_connection_with_lock_wait(
        connection,
        None,
        None,
        LockWait::CopyDeadline,
        ProtocolRetry::Never,
    ) else {
        panic!("the locked store must refuse admission");
    };
    let trace = copy_trace::take();
    assert!(
        matches!(error, StoreError::Sqlite(rusqlite::Error::SqliteFailure(ref failure, _))
            if failure.code == rusqlite::ErrorCode::DatabaseBusy),
        "{error}"
    );
    let produced = trace
        .iter()
        .position(|step| step.contains("admission error produced:"))
        .unwrap();
    let closing = trace
        .iter()
        .position(|step| step.ends_with("open admission failure: close begins"))
        .unwrap();
    let closed = trace
        .iter()
        .position(|step| step.ends_with("open admission failure: closed"))
        .unwrap();
    assert!(produced < closing && closing < closed, "{trace:?}");
    assert!(
        trace[produced].contains("sqlite code DatabaseBusy"),
        "{trace:?}"
    );
    assert!(trace.last().unwrap().contains("no copy scope"), "{trace:?}");
    assert!(!super::super::lock_wait::in_copy_scope());
}

#[test]
fn a_copy_waits_for_locks_through_its_deadline_handler_and_leaves_no_scope() {
    let directory = crate::test_support::temp_home().expect("temporary store directory");
    let source = directory.path().join("engram.db");
    drop(SqliteStore::open(&source).expect("initialize current store"));
    let target = directory.path().join("copy.db");
    let interrupt = CopyInterrupt::after(Duration::from_secs(60));

    copy_trace::start();
    let copied = SqliteStore::copy_existing_read_only(&source, &target, &interrupt);
    let trace = copy_trace::take();
    copied.expect("copy the store");
    assert!(
        trace
            .iter()
            .any(|step| step.ends_with("open: lock wait CopyDeadline set")),
        "the admission waits through the copy's handler: {trace:?}"
    );
    // SQLite reads a busy timeout of 0 while a custom busy handler is set.
    assert!(
        trace
            .iter()
            .any(|step| step.ends_with("copy: vacuum begins, busy timeout 0 ms")),
        "the copy statement waits through the copy's handler: {trace:?}"
    );
    assert!(
        !super::super::lock_wait::in_copy_scope(),
        "the copy's scope ends with it"
    );

    SqliteStore::settle_store_copy(&target, &interrupt).expect("settle the copy");
    assert!(
        !super::super::lock_wait::in_copy_scope(),
        "the settlement's scope ends with it"
    );
}
