use super::*;
use crate::diagnostics::{
    Correlation, Diagnostics, Fingerprint, Surface,
    tests::{drain, records},
    transaction::{Context, Scope},
};
use rusqlite::hooks::{AuthAction, AuthContext, Authorization, TransactionOperation};

fn observe(home: &std::path::Path) -> (Diagnostics, Scope) {
    let sink = Diagnostics::start(home, "session-12329", Surface::Mcp);
    let context = Context::new(
        sink.clone(),
        "note",
        Correlation::default(),
        Fingerprint::store(&home.join("store.db")),
    )
    .capture_all_for_test();
    (sink, Scope::enter(context))
}
fn settled(home: &std::path::Path) -> Vec<serde_json::Value> {
    records(home)
        .into_iter()
        .filter(|row| {
            row["event"]["kind"] == "store_transaction" && row["event"]["stage"] == "settled"
        })
        .collect()
}
fn count(store: &SqliteStore) -> i64 {
    store
        .connection
        .query_row("SELECT count(*) FROM diagnostic_payload", [], |r| r.get(0))
        .unwrap()
}

#[test]
fn work_cleanup_is_observed_after_commit_rollback_drop_and_unwind() {
    for mode in [
        "commit",
        "rollback",
        "drop",
        "unwind",
        "commit_failed",
        "schema_failed",
    ] {
        let home = crate::test_support::temp_home().unwrap();
        let mut store = SqliteStore::open_in_memory().unwrap();
        store
            .connection
            .execute_batch("CREATE TABLE diagnostic_payload(value)")
            .unwrap();
        if mode == "commit_failed" {
            store.connection.commit_hook(Some(|| true)).unwrap();
        }
        if mode == "schema_failed" {
            store.work_schema_version += 1;
        }
        let (sink, scope) = observe(home.path());
        if mode == "schema_failed" {
            assert!(store.begin_work_mutation().is_err());
        } else {
            let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                let transaction = store.begin_work_mutation().unwrap();
                transaction
                    .execute("INSERT INTO diagnostic_payload VALUES(1)", [])
                    .unwrap();
                match mode {
                    "commit" => transaction.commit().unwrap(),
                    "rollback" => transaction.rollback().unwrap(),
                    "commit_failed" => assert!(transaction.commit().is_err()),
                    "unwind" => panic!("injected transaction body unwind"),
                    _ => drop(transaction),
                }
            }));
            assert_eq!(result.is_err(), mode == "unwind");
        }
        drop(scope);
        store.connection.commit_hook(None::<fn() -> bool>).unwrap();
        assert!(store.connection.is_autocommit(), "{mode}");
        assert_eq!(timeout(&store), 5000);
        assert_eq!(count(&store), i64::from(mode == "commit"));
        drain(&sink);
        let rows = settled(home.path());
        assert_eq!(rows.len(), 1, "{mode}: {rows:?}");
        let record = &rows[0]["event"]["store_transaction"];
        assert_eq!(record["connection_autocommit"], true, "{mode}");
        assert!(record["acquired_interval_ms"].is_number());
        let expected = match mode {
            "commit" => "committed",
            "rollback" => "rolled_back",
            "commit_failed" => "commit_failed",
            _ => "scope_exit",
        };
        assert_eq!(record["disposition"], expected, "{mode}");
    }
}

#[test]
fn failed_work_rollback_does_not_certify_release() {
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open_in_memory().unwrap();
    store
        .connection
        .execute_batch("CREATE TABLE diagnostic_payload(value)")
        .unwrap();
    store
        .connection
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
    let (sink, scope) = observe(home.path());
    let transaction = store.begin_work_mutation().unwrap();
    transaction
        .execute("INSERT INTO diagnostic_payload VALUES(1)", [])
        .unwrap();
    assert!(transaction.rollback().is_err());
    drop(scope);
    assert!(!store.connection.is_autocommit());
    drain(&sink);
    let rows = settled(home.path());
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0]["event"]["store_transaction"]["disposition"],
        "rollback_failed"
    );
    assert_eq!(
        rows[0]["event"]["store_transaction"]["connection_autocommit"],
        false
    );
    store
        .connection
        .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
        .unwrap();
    store.connection.execute_batch("ROLLBACK").unwrap();
    assert_eq!(count(&store), 0);
}

#[test]
fn nested_work_refusal_preserves_caller_and_reports_no_acquired_interval() {
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open_in_memory().unwrap();
    store
        .connection
        .execute_batch(
            "BEGIN; CREATE TEMP TABLE caller_payload(value); INSERT INTO caller_payload VALUES(7)",
        )
        .unwrap();
    let (sink, scope) = observe(home.path());
    assert!(store.begin_work_mutation().is_err());
    drop(scope);
    assert!(!store.connection.is_autocommit());
    assert_eq!(
        store
            .connection
            .query_row("SELECT value FROM caller_payload", [], |r| r
                .get::<_, i64>(0))
            .unwrap(),
        7
    );
    drain(&sink);
    let rows = records(home.path())
        .into_iter()
        .filter(|r| r["event"]["stage"] == "acquisition_failed")
        .collect::<Vec<_>>();
    assert_eq!(rows.len(), 1);
    assert!(rows[0]["event"]["store_transaction"]["acquired_interval_ms"].is_null());
    assert!(rows[0]["event"]["store_transaction"]["connection_autocommit"].is_null());
    store.connection.execute_batch("ROLLBACK").unwrap();
}
