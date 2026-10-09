use super::*;

fn timeout(store: &SqliteStore) -> u32 {
    store
        .connection
        .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
        .unwrap()
}

#[test]
fn writer_admission_nested_transaction_is_untouched() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    store
        .connection
        .execute_batch(
            "BEGIN; CREATE TEMP TABLE caller_owned(value); INSERT INTO caller_owned VALUES (1)",
        )
        .unwrap();
    let mut scoped = store.note_writer_admission();
    assert!(matches!(
        scoped.begin_work_mutation(),
        Err(StoreError::WorkWriterAdmissionRefused {
            reason: WorkWriterAdmissionReason::NotAutocommit,
            ..
        })
    ));
    assert!(!scoped.connection.is_autocommit());
    assert_eq!(
        scoped
            .connection
            .query_row("SELECT value FROM caller_owned", [], |row| row
                .get::<_, i32>(0))
            .unwrap(),
        1
    );
    assert_eq!(timeout(&scoped), 5000);
    scoped.connection.execute_batch("ROLLBACK").unwrap();
}

#[test]
fn writer_admission_restores_timeout_and_scope_on_schema_failure_and_unwind() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    store.work_schema_version += 1;
    {
        let mut scoped = store.note_writer_admission_with_budget(Duration::from_millis(20));
        assert!(scoped.begin_work_mutation().is_err());
        assert!(scoped.connection.is_autocommit());
        assert_eq!(timeout(&scoped), 5000);
    }
    assert!(store.writer_admission.is_none());
    let panic = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _scope = store.note_writer_admission();
        panic!("scope unwinding");
    }));
    assert!(panic.is_err());
    assert!(store.writer_admission.is_none());
}

#[test]
fn writer_admission_classifies_extended_codes_without_retrying() {
    for (code, expected) in [
        (
            rusqlite::ffi::SQLITE_BUSY,
            WorkWriterAdmissionReason::BusyBudgetExhausted,
        ),
        (
            rusqlite::ffi::SQLITE_BUSY_SNAPSHOT,
            WorkWriterAdmissionReason::BusySnapshot,
        ),
        (
            rusqlite::ffi::SQLITE_LOCKED_SHAREDCACHE,
            WorkWriterAdmissionReason::Locked,
        ),
    ] {
        let source = rusqlite::Error::SqliteFailure(rusqlite::ffi::Error::new(code), None);
        let error = admission_error(
            source,
            true,
            Duration::from_millis(3),
            Duration::from_millis(20),
        );
        let StoreError::WorkWriterAdmissionRefused {
            reason,
            sqlite_primary_code,
            sqlite_extended_code,
            source,
            ..
        } = error
        else {
            panic!("typed refusal")
        };
        assert_eq!(reason, expected);
        assert_eq!(sqlite_primary_code, Some(code & 0xff));
        assert_eq!(sqlite_extended_code, Some(code));
        assert_eq!(source.sqlite_error().unwrap().extended_code, code);
    }
}

#[test]
fn writer_admission_preserves_custom_timeout_and_reports_ordinary_allowance() {
    let directory = crate::test_support::temp_home().unwrap();
    let path = directory.path().join("custom-timeout.db");
    let mut store = SqliteStore::open(&path).unwrap();
    let blocker = rusqlite::Connection::open(&path).unwrap();
    for ordinary_ms in [0, 7] {
        store
            .connection
            .busy_timeout(Duration::from_millis(ordinary_ms))
            .unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        assert!(matches!(
            store.begin_work_mutation(),
            Err(StoreError::WorkWriterAdmissionRefused {
                reason: WorkWriterAdmissionReason::BusyBudgetExhausted,
                budget_ms,
                ..
            }) if budget_ms == ordinary_ms
        ));
        {
            let mut scoped = store.note_writer_admission_with_budget(Duration::from_millis(20));
            assert!(matches!(
                scoped.begin_work_mutation(),
                Err(StoreError::WorkWriterAdmissionRefused {
                    reason: WorkWriterAdmissionReason::BusyBudgetExhausted,
                    budget_ms,
                    ..
                }) if budget_ms <= 20
            ));
            assert_eq!(u64::from(timeout(&scoped)), ordinary_ms);
            blocker.execute_batch("COMMIT").unwrap();
            // Restoration precedes the body even after a successful BEGIN.
            let transaction = scoped.begin_work_mutation().unwrap();
            let restored: u32 = transaction
                .query_row("PRAGMA busy_timeout", [], |row| row.get(0))
                .unwrap();
            assert_eq!(u64::from(restored), ordinary_ms);
            transaction.commit().unwrap();
        }
        assert_eq!(u64::from(timeout(&store)), ordinary_ms);
        assert!(store.writer_admission.is_none());
    }
}

#[test]
fn writer_admission_two_acquisitions_share_the_short_window() {
    let directory = crate::test_support::temp_home().unwrap();
    let path = directory.path().join("admission.db");
    let mut store = SqliteStore::open(&path).unwrap();
    let contender = rusqlite::Connection::open(&path).unwrap();
    contender.execute_batch("BEGIN IMMEDIATE").unwrap();
    let mut scoped = store.note_writer_admission_with_budget(Duration::from_millis(30));
    let first = scoped.begin_work_mutation().unwrap_err();
    let second = scoped.begin_work_mutation().unwrap_err();
    let StoreError::WorkWriterAdmissionRefused {
        budget_ms: first_budget,
        ..
    } = first
    else {
        panic!("first refusal")
    };
    let StoreError::WorkWriterAdmissionRefused {
        budget_ms: second_budget,
        ..
    } = second
    else {
        panic!("second refusal")
    };
    assert!(first_budget <= 30 && first_budget > 0);
    assert!(second_budget < first_budget);
    assert_eq!(timeout(&scoped), 5000);
    contender.execute_batch("COMMIT").unwrap();
    // An exhausted contention allowance still permits an immediate acquisition.
    scoped.begin_work_mutation().unwrap().commit().unwrap();
    assert_eq!(timeout(&scoped), 5000);
    drop(scoped);
    assert!(store.writer_admission.is_none());
}
