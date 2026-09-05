use super::*;

#[test]
fn phoenix_claim_renewal_refuses_pending_handoff_and_completed_work() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("work.db");
    let service = LocalWorkService::new(
        database.clone(),
        ProjectId("renewal-handoff".into()),
        "owner".into(),
        SessionId("holder".into()),
        None,
    );
    let root = proposed_root(
        service
            .work_propose(root_input("Renewal handoff", "root"), at(0))
            .unwrap(),
    );
    let claim = || WorkUpdateInput::Claim {
        ttl_seconds: Some(7_200),
        recovery_reason: None,
        idempotency_key: String::new(),
    };
    service.work_update(claim(), at(1)).unwrap();
    service
        .work_handoff(
            WorkHandoffInput::Offer {
                to: "recipient".into(),
                ttl_seconds: Some(120),
                checkpoint_summary: "ready for transfer".into(),
                idempotency_key: "offer".into(),
            },
            at(2),
        )
        .unwrap();
    let store = SqliteStore::open(&database).unwrap();
    let before = store.current_work_claim(root.work_id).unwrap().unwrap();
    let event_count = store.work_event_count(root.work_id).unwrap();
    let error = service.work_update(claim(), at(3)).unwrap_err();
    let structured = crate::mcp::store_error_value(&error);
    assert_eq!(
        structured["error"]["details"]["remedy"],
        "cancel the handoff offer, or let it be accepted or expire before retrying"
    );
    assert!(matches!(error, StoreError::InvalidWork(ref message)
        if message.contains("cancel the offer") && message.contains("accepted or expire")));
    let error: crate::verbs::VerbError = error.into();
    let guidance = error.guidance();
    assert!(
        guidance
            .next
            .iter()
            .any(|command| command.contains("handoff") && command.contains("--cancel"))
    );
    assert_eq!(
        store.current_work_claim(root.work_id).unwrap().unwrap(),
        before
    );
    assert_eq!(store.work_event_count(root.work_id).unwrap(), event_count);
    service
        .work_handoff(
            WorkHandoffInput::Cancel {
                reason: "continuing locally".into(),
                idempotency_key: "cancel".into(),
            },
            at(4),
        )
        .unwrap();
    service.work_update(claim(), at(5)).unwrap();
    let renewed = store.current_work_claim(root.work_id).unwrap().unwrap();
    assert_eq!(renewed.claim_id, before.claim_id);
    assert_eq!(renewed.fence, before.fence);
    assert_eq!(renewed.expires_at, at(7_205));
    service
        .work_complete(completion_input("delivered", "done"), at(6))
        .unwrap();
    assert!(matches!(service.work_update(claim(), at(7)),
        Err(StoreError::InvalidWork(message)) if message == COMPLETED_WORK_LATE_FINDING_REFUSAL));
}

#[test]
fn phoenix_keyless_claim_renews_but_explicit_key_replays_without_shortening() {
    let directory = tempdir().unwrap();
    let database = directory.path().join("work.db");
    let project = ProjectId("claim-renewal".into());
    let service = LocalWorkService::new(
        database.clone(),
        project,
        "owner".into(),
        SessionId("holder".into()),
        None,
    );
    let root = proposed_root(
        service
            .work_propose(root_input("Renewal", "root"), at(0))
            .unwrap(),
    );
    let input = |ttl_seconds, key: &str| WorkUpdateInput::Claim {
        ttl_seconds,
        recovery_reason: None,
        idempotency_key: key.into(),
    };
    service.work_update(input(Some(120), ""), at(1)).unwrap();
    let first = SqliteStore::open(&database)
        .unwrap()
        .current_work_claim(root.work_id)
        .unwrap()
        .unwrap();
    service.work_update(input(None, ""), at(2)).unwrap();
    let renewed = SqliteStore::open(&database)
        .unwrap()
        .current_work_claim(root.work_id)
        .unwrap()
        .unwrap();
    assert_eq!(renewed.claim_id, first.claim_id);
    assert_eq!(renewed.fence, first.fence);
    assert_eq!(renewed.expires_at, at(2 + DEFAULT_WORK_CLAIM_TTL_SECONDS));
    assert_eq!(renewed.revision, first.revision + 1);
    service.work_update(input(Some(60), ""), at(3)).unwrap();
    let shorter = SqliteStore::open(&database)
        .unwrap()
        .current_work_claim(root.work_id)
        .unwrap()
        .unwrap();
    assert_eq!(shorter.expires_at, renewed.expires_at);
    assert_eq!(shorter.fence, first.fence);
    let keyed = service
        .work_update(input(Some(7_200), "explicit-renewal"), at(4))
        .unwrap();
    let replay = service
        .work_update(input(Some(7_200), "explicit-renewal"), at(30))
        .unwrap();
    assert_eq!(
        serde_json::to_value(keyed).unwrap(),
        serde_json::to_value(replay).unwrap()
    );
    let store = SqliteStore::open(&database).unwrap();
    let final_claim = store.current_work_claim(root.work_id).unwrap().unwrap();
    assert_eq!(final_claim.expires_at, at(7_204));
    assert_eq!(final_claim.revision, shorter.revision + 1);
    let events = store.work_event_tail(root.work_id, 50).unwrap();
    let renewals = events
        .iter()
        .filter(|entry| {
            matches!(
                store
                    .get::<WorkEvent>(&entry.object_hash)
                    .unwrap()
                    .unwrap()
                    .transition,
                WorkTransition::ClaimRenewed { .. }
            )
        })
        .count();
    assert_eq!(renewals, 3);
    let invalid = store.verify_all().unwrap().invalid_work_records;
    assert!(invalid.is_empty(), "{invalid:?}");
    let saved = service
        .save_work_graph_snapshot(None, WorkGraphSnapshotDestinationKind::Stdout, at(31))
        .unwrap();
    let destination = LocalWorkService::new(
        directory.path().join("restored.db"),
        service.project_id.clone(),
        "reader".into(),
        SessionId("reader-session".into()),
        None,
    );
    destination
        .load_work_graph_snapshot(&serde_json::to_vec(&saved.document).unwrap(), false, at(32))
        .unwrap();
    assert!(
        destination
            .work_focus(&root.short_ref, at(33))
            .unwrap()
            .restored_history
            .items
            .iter()
            .any(
                |event| event.kind == "claimed" && event.summary.contains("renewed existing claim")
            )
    );
}
