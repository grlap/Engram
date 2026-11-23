use super::*;

#[test]
fn legacy_completion_intent_replays_but_cannot_create_a_new_seal() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let work = store
        .create_work(
            &root_request("legacy-completion-replay", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let held = claim(&mut store, &work, "runner", "claim", 1, 300);
    let evidence = evidence(&mut store, &work, &held, "runner", "evidence", 2);
    checkpoint(
        &mut store,
        &work,
        &held,
        "runner",
        "checkpoint",
        3,
        std::slice::from_ref(&evidence),
    );
    let current = completion_request(&work, &held, "runner", &evidence, "complete", 4);

    // Construct the old wire intent independently of the current request type.
    let mut historical_json = serde_json::to_value(&current).unwrap();
    historical_json["source_fingerprint"] = serde_json::json!("historical-source");
    let historical_bytes = serde_json::to_vec(&historical_json).unwrap();
    let historical: CompleteWorkRequest = serde_json::from_slice(&historical_bytes).unwrap();
    let before = test_database_shape_snapshot(&store.connection).unwrap();
    assert!(matches!(
        store.complete_work(&historical, &DevelopmentNoopRedactor),
        Err(StoreError::WorkCompletionRefused { reason, .. })
            if reason.contains("source_fingerprint is retired")
    ));
    assert_eq!(
        test_database_shape_snapshot(&store.connection).unwrap(),
        before
    );

    let seal = store
        .complete_work(&current, &DevelopmentNoopRedactor)
        .unwrap();
    // Seed the receipt's original request hash as an earlier build stored it.
    // The completed seal and its evidence are ordinary production output.
    let historical_intent = CanonicalObject::freeze(&historical_json).unwrap();
    assert_eq!(
        store
            .connection
            .execute(
                "UPDATE work_operation_results SET request_hash = ?1
             WHERE operation = 'complete_work' AND idempotency_key = ?2",
                params![historical_intent.key().as_str(), current.idempotency_key],
            )
            .unwrap(),
        1
    );
    let before_replay = test_database_shape_snapshot(&store.connection).unwrap();
    assert_eq!(
        store
            .complete_work(&historical, &DevelopmentNoopRedactor)
            .unwrap(),
        seal
    );
    assert_eq!(
        test_database_shape_snapshot(&store.connection).unwrap(),
        before_replay
    );
    for altered in [Some("different-source".into()), None] {
        let mut changed = historical.clone();
        changed.source_fingerprint = altered;
        assert!(matches!(
            store.complete_work(&changed, &DevelopmentNoopRedactor),
            Err(StoreError::WorkOperationIdempotencyConflict { .. })
        ));
        assert_eq!(
            test_database_shape_snapshot(&store.connection).unwrap(),
            before_replay
        );
    }
    let mut new_intent = historical;
    new_intent.idempotency_key = "new-legacy-completion".into();
    assert!(matches!(
        store.complete_work(&new_intent, &DevelopmentNoopRedactor),
        Err(StoreError::WorkCompletionRefused { reason, .. })
            if reason.contains("source_fingerprint is retired")
    ));
    assert_eq!(
        test_database_shape_snapshot(&store.connection).unwrap(),
        before_replay
    );
}
