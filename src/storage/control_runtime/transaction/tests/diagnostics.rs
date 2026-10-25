use super::*;
use crate::diagnostics::{
    Correlation, Diagnostics, Fingerprint, Surface,
    tests::{drain, records},
    transaction::{Context, Scope},
};

#[test]
fn transaction_budget_reports_the_installed_native_cap_and_restores_configuration() {
    for (configured, effective) in [(250_u64, 250_u64), (20000, 15000)] {
        let home = crate::test_support::temp_home().unwrap();
        let mut connection = Connection::open_in_memory().unwrap();
        connection
            .busy_timeout(Duration::from_millis(configured))
            .unwrap();
        let _request = crate::storage::control_runtime::request_budget::test_clock::scope();
        let aggregate = crate::storage::ControlDiagnosticScope::enter();
        let sink = Diagnostics::start(home.path(), "synthetic-control", Surface::Control);
        let context = Scope::enter(
            Context::new(
                sink.clone(),
                "turn_checkpoint",
                Correlation::default(),
                Fingerprint::store(&home.path().join("control.db")),
            )
            .capture_all_for_test(),
        );
        control_transaction(&mut connection, |transaction| {
            let installed: u32 = transaction.query_row("PRAGMA busy_timeout", [], |r| r.get(0))?;
            assert_eq!(u64::from(installed), effective);
            Ok(())
        })
        .unwrap();
        assert_eq!(aggregate.snapshot().attempted, 1);
        assert_eq!(aggregate.snapshot().acquired, 1);
        let restored: u32 = connection
            .query_row("PRAGMA busy_timeout", [], |r| r.get(0))
            .unwrap();
        assert_eq!(u64::from(restored), configured);
        drop(context);
        drain(&sink);
        let rows = records(home.path());
        let settled = rows
            .iter()
            .find(|r| r["event"]["stage"] == "settled")
            .unwrap();
        let budget = &settled["event"]["store_transaction"]["budget"];
        assert_eq!(budget["configured_ms"], configured);
        assert_eq!(budget["allowance_ms"], effective);
        assert!(budget["shared_window_ms"].is_null());
    }
}

#[test]
fn control_cleanup_evidence_preserves_results_and_observes_actual_autocommit() {
    for mode in [
        "commit",
        "rollback",
        "implicit",
        "unwind",
        "commit_failed",
        "rollback_failed",
    ] {
        let home = crate::test_support::temp_home().unwrap();
        let mut connection = Connection::open_in_memory().unwrap();
        connection
            .execute_batch("CREATE TABLE payload(value)")
            .unwrap();
        if mode == "commit_failed" {
            connection.commit_hook(Some(|| true)).unwrap();
        }
        if mode == "rollback_failed" {
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
        }
        let sink = Diagnostics::start(home.path(), "session-12329", Surface::Control);
        let scope = Scope::enter(
            Context::new(
                sink.clone(),
                "turn_checkpoint",
                Correlation {
                    frame_seq: Some(3),
                    ..Correlation::default()
                },
                Fingerprint::store(&home.path().join("control.db")),
            )
            .capture_all_for_test(),
        );
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            control_transaction(&mut connection, |transaction| {
                transaction.execute("INSERT INTO payload VALUES(1)", [])?;
                match mode {
                    "rollback" | "rollback_failed" => Err(busy_error(rusqlite::ffi::SQLITE_BUSY)),
                    "implicit" => Err(StoreError::InvalidWork("synthetic body refusal".into())),
                    "unwind" => panic!("injected control body unwind"),
                    _ => Ok(()),
                }
            })
        }));
        assert_eq!(result.is_err(), mode == "unwind");
        if let Ok(result) = result {
            assert_eq!(result.is_ok(), mode == "commit");
        }
        drop(scope);
        connection.commit_hook(None::<fn() -> bool>).unwrap();
        assert_eq!(connection.is_autocommit(), mode != "rollback_failed");
        drain(&sink);
        let rows = records(home.path())
            .into_iter()
            .filter(|row| {
                row["event"]["kind"] == "store_transaction" && row["event"]["stage"] == "settled"
            })
            .collect::<Vec<_>>();
        assert_eq!(rows.len(), 1, "{mode}: {rows:?}");
        let event = &rows[0]["event"];
        assert_eq!(event["frame_seq"], 3);
        assert_eq!(event["store_transaction"]["boundary"], "control_mutation");
        assert_eq!(
            event["store_transaction"]["connection_autocommit"],
            mode != "rollback_failed"
        );
        let expected = match mode {
            "commit" => "committed",
            "commit_failed" => "commit_failed",
            "rollback" => "rolled_back",
            "rollback_failed" => "rollback_failed",
            _ => "scope_exit",
        };
        assert_eq!(
            event["store_transaction"]["disposition"], expected,
            "{mode}"
        );
        if mode == "rollback_failed" {
            connection
                .authorizer(None::<fn(AuthContext<'_>) -> Authorization>)
                .unwrap();
            connection.execute_batch("ROLLBACK").unwrap();
        }
        assert_eq!(row_count(&connection), i64::from(mode == "commit"));
    }
}
