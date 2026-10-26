use super::*;
use crate::WorkCatalogQuery;
use crate::storage::SqliteStore;
use crate::storage::work::test_support::*;
use std::time::Duration;

mod guidance;
mod hydration;
mod restored;

fn fixture() -> (SqliteStore, ProjectId, crate::ObjectId) {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let item = store
        .create_work(
            &root_request("search-budget", "Subject", 0),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let held = claim(&mut store, &item, "holder", "held", 1, 600);
    let item = store.get_work_item(item.work_id).unwrap();
    let mut evidence = None;
    for index in 0..8 {
        evidence = Some(
            store
                .record_work_evidence(
                    &RecordWorkEvidenceRequest {
                        work_id: item.work_id,
                        run_id: held.run_id,
                        expected_work_revision: item.revision,
                        holder: held.holder.clone(),
                        claim_id: held.claim_id,
                        claim_fence: held.fence,
                        summary: format!("Straße /literal/path/{}", "retained text ".repeat(64)),
                        refs: vec![format!("ref-{index}")],
                        actor: actor("holder"),
                        idempotency_key: format!("note-{index}"),
                        recorded_at: at(2),
                    },
                    &DevelopmentNoopRedactor,
                )
                .unwrap(),
        );
    }
    (store, item.project_id, evidence.unwrap())
}

#[test]
fn note_search_budget_is_shared_by_plain_and_counted_catalog_readers() {
    let (store, project, _) = fixture();
    let query = WorkCatalogQuery {
        search: Some("STRASSE".into()),
        limit: 1,
        ..WorkCatalogQuery::default()
    };
    let before = store.connection.limit(Limit::SQLITE_LIMIT_LENGTH).unwrap();
    let limits = Limits {
        records: 0,
        ..Limits::default()
    };
    for counted in [false, true] {
        let result = with_limits(limits, || {
            if counted {
                store
                    .query_work_catalog_listing(&project, at(3), &query)
                    .map(|value| value.0)
            } else {
                store.query_work_catalog(&project, at(3), &query)
            }
        });
        let error = result.unwrap_err();
        assert!(matches!(
            error,
            StoreError::SearchBudgetExhausted {
                dimension: "record_visits",
                ..
            }
        ));
        let rendered = crate::verbs::store_error_value(&error);
        assert_eq!(rendered["error"]["code"], "search_budget_exhausted");
        assert!(rendered.get("items").is_none());
        assert!(rendered.get("total").is_none());
        assert_eq!(
            store.connection.limit(Limit::SQLITE_LIMIT_LENGTH).unwrap(),
            before
        );
    }
    let (page, total, _, _, _, matches) = store
        .query_work_catalog_continuation(&project, at(3), &query, None)
        .unwrap();
    assert_eq!(total, 1);
    assert_eq!(page.items.len(), 1);
    assert_eq!(matches.len(), 1);
    assert_eq!(
        store
            .query_work_catalog(
                &project,
                at(3),
                &WorkCatalogQuery {
                    search: None,
                    ..query
                }
            )
            .unwrap()
            .items
            .len(),
        1
    );
}

#[test]
fn search_budget_interrupts_sql_and_does_not_leak_into_the_next_call() {
    let store = SqliteStore::open_in_memory().unwrap();
    let error = with_limits(Limits { vm_steps: 0, ..Limits::default() }, || {
        run(&store.connection, Some("absent"), || {
            Ok(store.connection.query_row(
                "WITH RECURSIVE numbers(n) AS (VALUES(1) UNION ALL SELECT n+1 FROM numbers WHERE n<10000) SELECT sum(n) FROM numbers",
                [], |row| row.get::<_, i64>(0),
            )?)
        })
    }).unwrap_err();
    assert!(matches!(
        error,
        StoreError::SearchBudgetExhausted {
            dimension: "sql_vm_steps",
            ..
        }
    ));
    let value: i64 = store
        .connection
        .query_row("SELECT 42", [], |row| row.get(0))
        .unwrap();
    assert_eq!(value, 42);
    assert!(active(&store.connection).is_none());
}

#[test]
fn search_budget_refuses_before_copying_and_decoding_canonical_bytes() {
    let (store, _, id) = fixture();
    crate::canonical::reset_canonical_decode_count();
    let error = with_limits(
        Limits {
            bytes: 128,
            ..Limits::default()
        },
        || {
            run(&store.connection, Some("text"), || {
                super::super::feeds::load_typed_work_object::<crate::domain::WorkEvidence>(
                    &store.connection,
                    &id,
                    "work_evidence",
                )
            })
        },
    )
    .unwrap_err();
    assert!(matches!(
        error,
        StoreError::SearchBudgetExhausted {
            dimension: "work_bytes",
            ..
        }
    ));
    assert_eq!(crate::canonical::canonical_decode_count(), 0);
}

#[test]
fn search_budget_refuses_oversized_sql_units_before_decode_and_restores_limit() {
    let store = SqliteStore::open_in_memory().unwrap();
    let object =
        crate::CanonicalObject::mint(&serde_json::json!({"large": "x".repeat(4096)})).unwrap();
    SqliteStore::insert_object(&store.connection, "synthetic", &object).unwrap();
    let before = store.connection.limit(Limit::SQLITE_LIMIT_LENGTH).unwrap();
    crate::canonical::reset_canonical_decode_count();
    let error = with_limits(
        Limits {
            unit_bytes: 1024,
            ..Limits::default()
        },
        || {
            run(&store.connection, Some("text"), || {
                super::super::feeds::load_typed_work_object::<serde_json::Value>(
                    &store.connection,
                    object.key(),
                    "synthetic",
                )
            })
        },
    )
    .unwrap_err();
    assert!(matches!(
        error,
        StoreError::SearchBudgetExhausted {
            dimension: "unit_bytes",
            ..
        }
    ));
    assert_eq!(crate::canonical::canonical_decode_count(), 0);
    assert_eq!(
        store.connection.limit(Limit::SQLITE_LIMIT_LENGTH).unwrap(),
        before
    );
    let value: serde_json::Value =
        super::super::feeds::load_typed_work_object(&store.connection, object.key(), "synthetic")
            .unwrap();
    assert_eq!(value["large"].as_str().unwrap().len(), 4096);
}

#[test]
fn search_budget_checks_text_work_before_normalization() {
    let store = SqliteStore::open_in_memory().unwrap();
    let error = with_limits(
        Limits {
            bytes: 16,
            ..Limits::default()
        },
        || {
            run(&store.connection, Some("text"), || {
                contains(&store.connection, &"ß".repeat(128), "ss")
            })
        },
    )
    .unwrap_err();
    assert!(matches!(
        error,
        StoreError::SearchBudgetExhausted {
            dimension: "work_bytes",
            ..
        }
    ));
}

#[test]
fn search_budget_expired_allowance_refuses_without_starting_read() {
    let store = SqliteStore::open_in_memory().unwrap();
    let called = std::cell::Cell::new(false);
    let error = with_limits(
        Limits {
            elapsed: Duration::ZERO,
            ..Limits::default()
        },
        || {
            run(&store.connection, Some("text"), || {
                called.set(true);
                Ok(())
            })
        },
    )
    .unwrap_err();
    assert!(matches!(
        error,
        StoreError::SearchBudgetExhausted {
            dimension: "elapsed_ms",
            ..
        }
    ));
    assert!(!called.get());
}

#[test]
fn search_budget_restores_its_thread_and_connection_state_after_unwind() {
    let store = SqliteStore::open_in_memory().unwrap();
    let before = store.connection.limit(Limit::SQLITE_LIMIT_LENGTH).unwrap();
    let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        let _: Result<(), StoreError> = run(&store.connection, Some("text"), || {
            panic!("injected unwind")
        });
    }));
    assert!(result.is_err());
    assert!(active(&store.connection).is_none());
    assert_eq!(
        store.connection.limit(Limit::SQLITE_LIMIT_LENGTH).unwrap(),
        before
    );
    assert_eq!(run(&store.connection, Some("text"), || Ok(7)).unwrap(), 7);
}

#[test]
fn search_request_checks_the_same_allowance_before_and_after_work() {
    thread_local! {
        static CLOCK: std::cell::Cell<std::time::Instant> = std::cell::Cell::new(std::time::Instant::now());
    }
    fn now() -> std::time::Instant {
        CLOCK.with(std::cell::Cell::get)
    }
    let mut request = Request::new(CancellationToken::new());
    request.allowance = Allowance::with_clock(request.limits.elapsed, now);
    let elapsed = request.limits.elapsed;
    let result = request.run(|| {
        CLOCK.with(|clock| clock.set(clock.get() + elapsed));
        Ok(42)
    });
    assert!(matches!(
        result.unwrap_err().error,
        StoreError::SearchBudgetExhausted {
            dimension: "elapsed_ms",
            ..
        }
    ));
    assert!(REQUEST.with_borrow(Option::is_none));
}

#[test]
fn search_request_cancellation_interrupts_sql_and_restores_the_connection() {
    let store = SqliteStore::open_in_memory().unwrap();
    let cancel = CancellationToken::new();
    let request = Request::new(cancel.clone());
    let result = request.run(|| {
        Ok(run(&store.connection, Some("text"), || {
            cancel.cancel();
            store.connection.query_row("WITH RECURSIVE n(x) AS (VALUES(1) UNION ALL SELECT x+1 FROM n WHERE x<1000000) SELECT sum(x) FROM n", [], |row| row.get::<_, i64>(0))?;
            Ok(())
        })?)
    });
    assert!(matches!(
        result.unwrap_err().error,
        StoreError::SearchCancelled
    ));
    assert!(active(&store.connection).is_none());
    assert!(REQUEST.with_borrow(Option::is_none));
    assert_eq!(
        store
            .connection
            .query_row("SELECT 42", [], |row| row.get::<_, i64>(0))
            .unwrap(),
        42
    );
}
