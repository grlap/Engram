use std::{cell::Cell, rc::Rc};

use super::*;

// Clear the test-only callback even if verification returns before the seam.
struct ClearCallback;

impl Drop for ClearCallback {
    fn drop(&mut self) {
        AFTER_EVENT_SCAN.with(|callback| callback.borrow_mut().take());
    }
}

fn completion_during_verification(full: bool, existing_transaction: bool) {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("snapshot.db");
    let mut writer = SqliteStore::open(&database).unwrap();
    let work = writer
        .create_work(
            &root_request("snapshot", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let held = claim(&mut writer, &work, "holder", "claim", 1, 3600);
    let proof = evidence(&mut writer, &work, &held, "holder", "evidence", 2);
    checkpoint(
        &mut writer,
        &work,
        &held,
        "holder",
        "checkpoint",
        3,
        std::slice::from_ref(&proof),
    );
    let request = completion_request(&work, &held, "holder", &proof, "complete", 4);
    let reader = SqliteStore::open(&database).unwrap();
    let before = reader.verify_all().unwrap();
    assert!(before.is_healthy());
    let before_work = reader.verify_work_projections().unwrap();
    let transaction =
        existing_transaction.then(|| reader.connection.unchecked_transaction().unwrap());
    let committed = Rc::new(Cell::new(false));
    let observed = Rc::clone(&committed);
    let _clear = ClearCallback;
    AFTER_EVENT_SCAN.with(|callback| {
        assert!(callback.borrow().is_none());
        *callback.borrow_mut() = Some(Box::new(move || {
            writer
                .complete_work(&request, &DevelopmentNoopRedactor)
                .unwrap();
            observed.set(true);
        }));
    });

    if full {
        let report = reader.verify_all().unwrap();
        assert!(
            committed.get(),
            "completion committed at the event/projection boundary"
        );
        assert_eq!(report.invalid_work_records, Vec::<String>::new());
        assert_eq!(
            report, before,
            "every check must describe the original snapshot"
        );
    } else {
        let report = SqliteStore::verify_work_projections_on(&reader.connection).unwrap();
        assert!(
            committed.get(),
            "completion committed at the event/projection boundary"
        );
        assert_eq!(report.1, Vec::<String>::new());
        assert_eq!(report, before_work);
    }
    assert_eq!(reader.connection.is_autocommit(), !existing_transaction);
    if let Some(transaction) = transaction {
        assert_eq!(
            reader.verify_all().unwrap(),
            before,
            "reuse the caller's snapshot"
        );
        transaction.commit().unwrap();
    }
    let after = reader.verify_all().unwrap();
    assert!(
        after.is_healthy(),
        "completion before verification is healthy too: {after:?}"
    );
    assert!(after.checked_objects > before.checked_objects);
    assert!(after.snapshot.object_count > before.snapshot.object_count);
    assert!(
        after.snapshot.project_feed_heads[0].position
            > before.snapshot.project_feed_heads[0].position
    );
    assert!(reader.connection.is_autocommit());
}

#[test]
fn verify_all_keeps_one_snapshot_across_completion() {
    completion_during_verification(true, false);
}

#[test]
fn standalone_work_verifier_keeps_one_snapshot_across_completion() {
    completion_during_verification(false, false);
}

#[test]
fn integrity_verifiers_reuse_the_callers_snapshot() {
    completion_during_verification(true, true);
    completion_during_verification(false, true);
}

thread_local! {
    static SNAPSHOT_SQL: std::cell::RefCell<Vec<String>> = const {
        std::cell::RefCell::new(Vec::new())
    };
}

#[test]
fn integrity_entry_points_begin_before_the_first_read_and_reuse_savepoints() {
    use rusqlite::trace::{TraceEvent, TraceEventCodes};

    let store = SqliteStore::open_in_memory().unwrap();
    let checks: [fn(&SqliteStore); 5] = [
        |store| assert!(store.verify_all().unwrap().is_healthy()),
        |store| assert!(store.verify_work_projections().unwrap().1.is_empty()),
        |store| {
            SqliteStore::verify_control_policy_history(&store.connection).unwrap();
        },
        |store| {
            assert!(
                SqliteStore::diagnose_control_policy_records_on(&store.connection)
                    .unwrap()
                    .invalid_control_records
                    .is_empty()
            );
        },
        |store| {
            assert!(
                crate::storage::graph_snapshot::verify_work_graph_snapshot_saved_events_on(
                    &store.connection
                )
                .unwrap()
                .1
                .is_empty()
            );
        },
    ];
    for check in checks {
        for caller_owned in [false, true] {
            if caller_owned {
                store.connection.execute_batch("SAVEPOINT caller").unwrap();
            }
            SNAPSHOT_SQL.with(|sql| sql.borrow_mut().clear());
            store.connection.trace_v2(
                TraceEventCodes::SQLITE_TRACE_STMT,
                Some(|event| {
                    if let TraceEvent::Stmt(_, sql) = event {
                        SNAPSHOT_SQL
                            .with(|statements| statements.borrow_mut().push(sql.to_owned()));
                    }
                }),
            );
            check(&store);
            store.connection.trace_v2(TraceEventCodes::empty(), None);
            let sql = SNAPSHOT_SQL.with(std::cell::RefCell::take);
            assert!(sql.iter().any(|sql| sql.starts_with("SELECT")));
            let begins = sql.iter().filter(|sql| sql.starts_with("BEGIN")).count();
            let commits = sql.iter().filter(|sql| sql.starts_with("COMMIT")).count();
            if caller_owned {
                assert_eq!((begins, commits), (0, 0));
                assert!(!store.connection.is_autocommit());
                store
                    .connection
                    .execute_batch("ROLLBACK TO caller; RELEASE caller")
                    .unwrap();
            } else {
                assert_eq!((begins, commits), (1, 1));
                assert!(sql.first().unwrap().starts_with("BEGIN"));
                assert!(sql.last().unwrap().starts_with("COMMIT"));
            }
            assert!(store.connection.is_autocommit());
        }
    }
}
