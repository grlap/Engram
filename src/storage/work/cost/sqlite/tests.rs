use super::*;

const SORTED_SCAN: &str = "SELECT value FROM sample ORDER BY value DESC";

fn fixture() -> Connection {
    let connection = Connection::open_in_memory().unwrap();
    install(&connection);
    populate(&connection);
    connection
}

fn populate(connection: &Connection) {
    connection
        .execute_batch(
            "CREATE TABLE sample(value INTEGER NOT NULL);
         WITH RECURSIVE values_to_insert(value) AS (
             SELECT 0 UNION ALL SELECT value + 1 FROM values_to_insert WHERE value < 127
         ) INSERT INTO sample SELECT value FROM values_to_insert;",
        )
        .unwrap();
}

fn scan(connection: &Connection) {
    let mut statement = connection.prepare_cached(SORTED_SCAN).unwrap();
    let values = statement
        .query_map([], |row| row.get::<_, i64>(0))
        .unwrap()
        .collect::<rusqlite::Result<Vec<_>>>()
        .unwrap();
    assert_eq!(values, (0..128).rev().collect::<Vec<_>>());
}

#[test]
fn cached_executions_report_deltas_and_exclude_preparation() {
    calibrate();
}

pub(super) fn calibrate() {
    let connection = fixture();
    scan(&connection); // A prior execution must not inflate the next sample.
    let scope = start();
    scan(&connection);
    scan(&connection);
    let metrics = scope.finish();
    assert_eq!(metrics.total.fullscan_steps, 254);
    assert_eq!(metrics.total.sorts, 2);
    assert_eq!(metrics.total.statements, 2);
    assert_eq!(metrics.by_kind["select"], metrics.total);
    let scope = start();
    scan(&connection);
    let single = scope.finish();
    assert_eq!(single.total.fullscan_steps, 127);
    assert_eq!(single.total.sorts, 1);
    assert_eq!(single.total.statements, 1);
    assert!(single.total.vm_steps > 0);
    assert_eq!(start().finish().total, Counters::default());
}

#[test]
fn early_dropped_rows_are_profiled_without_draining() {
    let connection = fixture();
    let mut statement = connection.prepare("SELECT value FROM sample").unwrap();
    let scope = start();
    {
        let mut rows = statement.query([]).unwrap();
        for expected in 0..10 {
            assert_eq!(
                rows.next().unwrap().unwrap().get::<_, i64>(0).unwrap(),
                expected
            );
        }
    }
    let metrics = scope.finish();
    assert_eq!(metrics.total.statements, 1);
    assert_eq!(metrics.total.sorts, 0);
    assert_eq!(
        metrics.total.fullscan_steps,
        u64::try_from(statement.get_status(StatementStatus::FullscanStep)).unwrap()
    );
    assert!(metrics.total.fullscan_steps > 0 && metrics.total.fullscan_steps < 127);
}

#[test]
fn trigger_subprograms_preserve_the_outer_baseline_and_count_once() {
    let connection = fixture();
    connection
        .execute_batch(
            "CREATE TABLE input(value INTEGER); CREATE TABLE audit(value INTEGER);
         CREATE TRIGGER audit_insert AFTER INSERT ON input BEGIN
             INSERT INTO audit SELECT value FROM sample ORDER BY value DESC;
         END;",
        )
        .unwrap();
    let mut statement = connection
        .prepare_cached("INSERT INTO input VALUES (?1)")
        .unwrap();
    let scope = start();
    statement.execute([1]).unwrap();
    statement.execute([2]).unwrap();
    let metrics = scope.finish();
    assert_eq!(metrics.total.statements, 2);
    assert_eq!(metrics.total.fullscan_steps, 254);
    assert_eq!(metrics.total.sorts, 2);
    assert_eq!(
        metrics.total.fullscan_steps,
        u64::try_from(statement.get_status(StatementStatus::FullscanStep)).unwrap()
    );
    assert_eq!(metrics.by_kind["insert"], metrics.total);
}

#[test]
fn identical_sql_on_two_connections_can_finish_out_of_order() {
    let first = fixture();
    let second = fixture();
    let mut first_query = first.prepare(SORTED_SCAN).unwrap();
    let mut second_query = second.prepare(SORTED_SCAN).unwrap();
    let scope = start();
    {
        let mut first_rows = first_query.query([]).unwrap();
        let mut second_rows = second_query.query([]).unwrap();
        assert!(first_rows.next().unwrap().is_some());
        assert!(second_rows.next().unwrap().is_some());
        // Drop the older execution first: attribution must not be a SQL-text
        // map or a LIFO stack shared by these identical statements.
        drop(first_rows);
        drop(second_rows);
    }
    let metrics = scope.finish();
    assert_eq!(metrics.total.statements, 2);
    assert_eq!(metrics.total.fullscan_steps, 254);
    assert_eq!(metrics.total.sorts, 2);
}

#[test]
fn failed_execution_and_unwind_do_not_leak_into_the_next_scope() {
    let connection = fixture();
    connection
        .execute_batch("CREATE UNIQUE INDEX sample_unique ON sample(value)")
        .unwrap();
    let scope = start();
    assert!(
        connection
            .execute("INSERT INTO sample VALUES (1)", [])
            .is_err()
    );
    assert_eq!(scope.finish().total.statements, 1);
    let unwound = std::panic::catch_unwind(|| {
        let _scope = start();
        panic!("leave the measurement before finish");
    });
    assert!(unwound.is_err());
    assert_eq!(start().finish().total, Counters::default());
}

#[test]
fn transaction_statement_categories_ignore_sql_terminators() {
    let connection = fixture();
    let scope = start();
    connection
        .execute_batch("BEGIN; ROLLBACK; BEGIN; COMMIT;")
        .unwrap();
    let metrics = scope.finish();
    assert_eq!(metrics.total.statements, 4);
    assert_eq!(metrics.by_kind["begin"].statements, 2);
    assert_eq!(metrics.by_kind["rollback"].statements, 1);
    assert_eq!(metrics.by_kind["commit"].statements, 1);
    assert!(!metrics.by_kind.contains_key("other"));
}

#[test]
fn a_statement_started_before_the_scope_is_rejected_on_reset() {
    let connection = fixture();
    let mut statement = connection.prepare("SELECT value FROM sample").unwrap();
    let mut rows = statement.query([]).unwrap();
    assert!(rows.next().unwrap().is_some());
    let scope = start();
    drop(rows);
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scope.finish())).is_err());
    assert_eq!(start().finish().total, Counters::default());
}

#[test]
fn retained_memory_and_new_file_connections_are_observed_automatically() {
    let retained = crate::SqliteStore::open_in_memory().unwrap();
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("cost.db");
    let writer =
        crate::storage::open_sqlite_file(&database, rusqlite::OpenFlags::default()).unwrap();
    populate(&writer);
    let scope = start();
    retained
        .connection
        .query_row("SELECT 1", [], |_| Ok(()))
        .unwrap();
    scan(&writer);
    let reader =
        crate::storage::open_sqlite_file(&database, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    scan(&reader);
    let metrics = scope.finish();
    assert_eq!(metrics.total.statements, 3);
    assert_eq!(metrics.total.fullscan_steps, 254);
    assert_eq!(metrics.total.sorts, 2);
}

#[tokio::test(flavor = "current_thread")]
async fn sqlite_metrics_and_mcp_phase_profile_share_one_callback() {
    let directory = crate::test_support::temp_home().unwrap();
    let (metrics, phases) = crate::phase_trace::scoped(async {
        let store =
            crate::SqliteStore::open_unresolved(directory.path().join("shared.db")).unwrap();
        let scope = start();
        store
            .connection
            .execute_batch("BEGIN IMMEDIATE; COMMIT;")
            .unwrap();
        scope.finish()
    })
    .await;
    assert_eq!(metrics.total.statements, 2);
    assert_eq!(metrics.by_kind["commit"].statements, 1);
    assert_eq!(metrics.total.fullscan_steps, 0);
    assert_eq!(metrics.total.sorts, 0);
    assert_eq!(phases.begin_immediate.count, 1);
    assert_eq!(phases.commit.count, 1);
}

#[tokio::test(flavor = "current_thread")]
async fn sqlite_metrics_do_not_enable_mcp_phases_on_an_untraced_connection() {
    let directory = crate::test_support::temp_home().unwrap();
    let store = crate::SqliteStore::open_unresolved(directory.path().join("untraced.db")).unwrap();
    let (metrics, phases) = crate::phase_trace::scoped(async {
        let scope = start();
        store
            .connection
            .execute_batch("BEGIN IMMEDIATE; COMMIT;")
            .unwrap();
        scope.finish()
    })
    .await;
    assert_eq!(metrics.total.statements, 2);
    assert_eq!(phases.begin_immediate.count, 0);
    assert_eq!(phases.commit.count, 0);
}

#[test]
fn statements_must_settle_inside_the_scope() {
    let connection = fixture();
    let mut statement = connection.prepare("SELECT value FROM sample").unwrap();
    let scope = start();
    let mut rows = statement.query([]).unwrap();
    assert!(rows.next().unwrap().is_some());
    assert!(std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| scope.finish())).is_err());
    drop(rows);
    assert_eq!(start().finish().total, Counters::default());
}

#[test]
fn first_empty_note_fts_write_is_fully_attributed_through_reprepare() {
    let connection = Connection::open_in_memory().unwrap();
    install(&connection);
    crate::storage::work::note_index::create_schema(&connection).unwrap();
    let scope = start();
    connection
        .execute(
            "INSERT INTO work_note_fts
             (locator, work_id, family, layer, generation, position, fields_json, search_text)
             VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8)",
            [
                "locator",
                "work",
                "notes",
                "1",
                "0",
                "1",
                "[]",
                "public note",
            ],
        )
        .unwrap();
    let category = CURRENT.with_borrow(|current| {
        current
            .as_ref()
            .unwrap()
            .profiled
            .values()
            .find(|(status, _)| status.reprepares > 0)
            .map(|(_, category)| *category)
            .expect("first note insert automatically reprepares an internal statement")
    });
    let first = scope.finish();
    assert!(first.total.vm_steps > 0);
    assert!(first.by_kind[category].vm_steps > 0);
    assert_eq!(start().finish().total, Counters::default());
}

#[test]
fn reprepare_profiles_charge_both_deltas_and_other_unmatched_profiles_fail() {
    let before = Status {
        fullscan_steps: 1,
        sorts: 1,
        vm_steps: 10,
        reprepares: 0,
        runs: 0,
    };
    let first = Status {
        fullscan_steps: 4,
        sorts: 2,
        vm_steps: 15,
        runs: 1,
        ..before
    };
    let retry = Status {
        fullscan_steps: 9,
        sorts: 4,
        vm_steps: 25,
        reprepares: 1,
        runs: 2,
    };
    let observed_first = || {
        let mut capture = Capture::default();
        capture.pending.insert(1, (before, "replace"));
        capture.profile(1, first);
        capture
    };
    let mut capture = observed_first();
    capture.profile(1, retry);
    assert!(!capture.invalid);
    assert_eq!(
        capture.metrics.total,
        Counters {
            statements: 1,
            fullscan_steps: 8,
            sorts: 3,
            vm_steps: 15,
        }
    );
    assert_eq!(capture.metrics.total, capture.metrics.by_kind["replace"]);
    for forged in [
        first, // Another PROFILE without an automatic reprepare.
        Status {
            reprepares: 0,
            ..retry
        },
        Status { runs: 1, ..retry },
        Status {
            vm_steps: 0,
            ..retry
        },
        Status {
            fullscan_steps: 0,
            ..retry
        },
        Status { sorts: 0, ..retry },
        Status {
            vm_steps: -1,
            ..retry
        },
    ] {
        let mut invalid = observed_first();
        let retained = invalid.metrics.total;
        invalid.profile(1, forged);
        assert!(invalid.invalid);
        assert_eq!(invalid.metrics.total, retained);
    }
    let mut unknown = Capture::default();
    unknown.profile(1, retry);
    assert!(unknown.invalid);
    assert_eq!(unknown.metrics.total, Counters::default());
}
