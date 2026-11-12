//! A completed non-fast-forward refusal stays known across a lost fetch, a
//! reopen and ordinary repeats, and it never permits a second push.
use super::*;

struct Started {
    _home: crate::test_support::TempHome,
    binding: Binding,
    selected: InspectedExchange,
    prepared: ObjectId,
    request: CompletedExchangeRequest,
}

fn started() -> Started {
    let home = crate::test_support::temp_home().unwrap();
    let binding = binding(home.path());
    let mut store = SqliteStore::open(&binding.store.database).unwrap();
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let selected = input(&store, &id, &binding);
    let prepared = prepare(&mut store, &selected);
    let request = CompletedExchangeRequest {
        binding: id,
        intent: selected.intent.clone(),
    };
    Started {
        _home: home,
        binding,
        selected,
        prepared,
        request,
    }
}

fn latest(store: &SqliteStore, request: &CompletedExchangeRequest) -> Transition {
    store
        .task_sync_outgoing_events(request)
        .unwrap()
        .pop()
        .unwrap()
        .1
        .transition
}

fn observed(store: &SqliteStore, request: &CompletedExchangeRequest) -> bool {
    store
        .task_sync_outgoing_events(request)
        .unwrap()
        .iter()
        .any(|(_, event)| matches!(event.transition, Transition::PushObserved { .. }))
}

/// Every row of the ledger projections that repair rebuilds.
fn ledger_rows(database: &Path) -> Vec<Vec<rusqlite::types::Value>> {
    let connection = rusqlite::Connection::open(database).unwrap();
    let mut rows = Vec::new();
    for table in [
        "work_task_sync_exchange_operations",
        "work_task_sync_exchange_heads",
    ] {
        let mut statement = connection
            .prepare(&format!("SELECT * FROM {table} ORDER BY 1"))
            .unwrap();
        let width = statement.column_count();
        rows.extend(
            statement
                .query_map([], |row| {
                    (0..width)
                        .map(|index| row.get::<_, rusqlite::types::Value>(index))
                        .collect::<rusqlite::Result<Vec<_>>>()
                })
                .unwrap()
                .map(|row| row.unwrap()),
        );
    }
    rows
}

#[tokio::test]
async fn a_refusal_with_a_lost_fetch_survives_reopen_and_repeats_and_still_reconciles() {
    let data = started();
    let mut store = SqliteStore::open(&data.binding.store.database).unwrap();
    let tip = fingerprint(&"competing data tip").to_string();
    let mut io = Io {
        tips: VecDeque::from([None, Some(tip.clone())]),
        results: VecDeque::from([Err(RunError::NonFastForward)]),
        fetch_failures: 1,
        ..Io::default()
    };
    let error = publish_with(&mut store, &data.selected, &data.request, &mut io)
        .await
        .unwrap_err();
    assert!(error.to_string().contains(REFUSED), "{error:#}");
    assert_eq!(io.pushes, 1);
    let Transition::PushRejected { push_started, .. } = latest(&store, &data.request) else {
        panic!("the refusal must be recorded before the fetch");
    };
    assert!(!observed(&store, &data.request));
    assert!(store.verify_all().unwrap().is_healthy());
    drop(store);

    let mut store = SqliteStore::open(&data.binding.store.database).unwrap();
    for _ in 0..2 {
        io.tips.push_back(Some(tip.clone()));
        let PublicationAttempt::ReconciliationRequired { failure, .. } =
            publish_with(&mut store, &data.selected, &data.request, &mut io)
                .await
                .unwrap()
        else {
            panic!("a recorded refusal must request reconciliation again");
        };
        assert_eq!(
            failure,
            OutgoingFailure::NonFastForward {
                push_started: push_started.clone(),
                fetched_tip: tip.clone(),
            }
        );
        assert_eq!(io.pushes, 1);
    }
    assert!(!observed(&store, &data.request));

    let mut resolved = input(&store, &data.request.binding, &data.binding);
    resolved.intent.expected_head = Some(data.selected.intent.commit.oid.clone());
    resolved.intent.commit.parents = vec![data.selected.intent.commit.oid.clone(), tip.clone()];
    save_commit(&resolved);
    store
        .record_task_sync_exchange_progress(
            &resolved,
            OutgoingProgress::Reconciled {
                prepared: data.prepared.clone(),
                reconciliation: Box::new(OutgoingReconciliation::Resolved {
                    failure: OutgoingFailure::NonFastForward {
                        push_started,
                        fetched_tip: tip,
                    },
                    resolved: Box::new(resolved.intent.clone()),
                }),
            },
            chrono::Utc::now(),
        )
        .unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
}

#[tokio::test]
async fn a_refusal_recorded_before_a_crash_resumes_only_the_fetch() {
    let data = started();
    let mut store = SqliteStore::open(&data.binding.store.database).unwrap();
    let (start, fresh) = store
        .record_task_sync_exchange_progress_once(
            &data.selected,
            OutgoingProgress::PushStarted,
            chrono::Utc::now(),
        )
        .unwrap();
    assert!(fresh);
    let refusal = OutgoingProgress::PushRejected {
        push_started: start.clone(),
    };
    let (rejected, fresh) = store
        .record_task_sync_exchange_progress_once(
            &data.selected,
            refusal.clone(),
            chrono::Utc::now(),
        )
        .unwrap();
    assert!(fresh);
    assert_eq!(
        store
            .record_task_sync_exchange_progress_once(&data.selected, refusal, chrono::Utc::now())
            .unwrap(),
        (rejected, false)
    );
    drop(store);

    let mut store = SqliteStore::open(&data.binding.store.database).unwrap();
    let tip = fingerprint(&"tip after the crash").to_string();
    let mut io = Io {
        tips: VecDeque::from([Some(tip.clone())]),
        ..Io::default()
    };
    let PublicationAttempt::ReconciliationRequired { failure, .. } =
        publish_with(&mut store, &data.selected, &data.request, &mut io)
            .await
            .unwrap()
    else {
        panic!("a recorded refusal must resume to reconciliation");
    };
    assert_eq!(
        failure,
        OutgoingFailure::NonFastForward {
            push_started: start,
            fetched_tip: tip.clone(),
        }
    );
    assert_eq!(io.pushes, 0);
    assert_eq!(io.fetched, vec![tip]);
    assert!(!observed(&store, &data.request));
    assert!(store.verify_all().unwrap().is_healthy());
}

#[tokio::test]
async fn a_refusal_admits_no_later_observation_and_no_second_refusal() {
    let data = started();
    let mut store = SqliteStore::open(&data.binding.store.database).unwrap();
    let (start, _) = store
        .record_task_sync_exchange_progress_once(
            &data.selected,
            OutgoingProgress::PushStarted,
            chrono::Utc::now(),
        )
        .unwrap();
    // Only the current start can be refused.
    assert!(
        store
            .record_task_sync_exchange_progress_once(
                &data.selected,
                OutgoingProgress::PushRejected {
                    push_started: data.prepared.clone(),
                },
                chrono::Utc::now(),
            )
            .is_err()
    );
    store
        .record_task_sync_exchange_progress_once(
            &data.selected,
            OutgoingProgress::PushRejected {
                push_started: start,
            },
            chrono::Utc::now(),
        )
        .unwrap();
    // A negative observation can't replace the known refusal.
    assert!(
        store
            .record_task_sync_exchange_progress_once(
                &data.selected,
                OutgoingProgress::Observed(PublicationObservation::Unknown { observed_tip: None }),
                chrono::Utc::now(),
            )
            .is_err()
    );
    assert!(matches!(
        latest(&store, &data.request),
        Transition::PushRejected { .. }
    ));
    assert!(store.verify_all().unwrap().is_healthy());
}

#[tokio::test]
async fn repair_rebuilds_identical_rows_for_old_unknown_and_new_refusal_history() {
    for refused in [false, true] {
        let data = started();
        let mut store = SqliteStore::open(&data.binding.store.database).unwrap();
        let mut io = Io {
            results: VecDeque::from([Err(if refused {
                RunError::NonFastForward
            } else {
                RunError::Deadline
            })]),
            fetch_failures: usize::from(refused),
            tips: VecDeque::from([None, Some(fingerprint(&"remote tip").to_string())]),
            ..Io::default()
        };
        let outcome = publish_with(&mut store, &data.selected, &data.request, &mut io).await;
        assert_eq!(outcome.is_err(), refused);
        assert_eq!(
            matches!(
                latest(&store, &data.request),
                Transition::PushRejected { .. }
            ),
            refused
        );
        assert_eq!(observed(&store, &data.request), !refused);
        assert!(store.verify_all().unwrap().is_healthy());
        drop(store);
        let before = ledger_rows(&data.binding.store.database);
        // One operation row and one binding head at least.
        assert!(before.len() >= 2, "{before:?}");
        let report =
            SqliteStore::repair_rebuildable_projections(&data.binding.store.database).unwrap();
        assert!(report.is_healthy(), "{report:?}");
        assert_eq!(ledger_rows(&data.binding.store.database), before);
        let store = SqliteStore::open(&data.binding.store.database).unwrap();
        assert!(store.verify_all().unwrap().is_healthy());
    }
}
