use super::*;

#[test]
fn expired_handoff_is_audited_and_does_not_block_a_new_offer() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("project-expiry", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let initial = claim(&mut store, &root, "agent-a", "claim-a", 1, 10);
    let expired_offer = store
        .offer_work_handoff(
            &OfferWorkHandoffRequest {
                work_id: root.work_id,
                run_id: initial.run_id,
                expected_work_revision: root.revision,
                from: initial.holder.clone(),
                to: SessionId("agent-b".into()),
                claim_id: initial.claim_id,
                claim_fence: initial.fence,
                ttl_seconds: 10,
                checkpoint_summary: "handoff that will expire".into(),
                actor: actor("agent-a"),
                idempotency_key: "first-offer".into(),
                offered_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("first offer");
    let renewed = store
        .current_work_claim(root.work_id)
        .expect("renewed claim")
        .expect("current holder remains authoritative");
    let replacement = store
        .offer_work_handoff(
            &OfferWorkHandoffRequest {
                work_id: root.work_id,
                run_id: renewed.run_id,
                expected_work_revision: root.revision,
                from: renewed.holder.clone(),
                to: SessionId("agent-d".into()),
                claim_id: renewed.claim_id,
                claim_fence: renewed.fence,
                ttl_seconds: 20,
                checkpoint_summary: "replacement handoff".into(),
                actor: actor("agent-a"),
                idempotency_key: "replacement-offer".into(),
                offered_at: at(13),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("replacement offer");
    assert_ne!(replacement.offer_id, expired_offer.offer_id);
    let expired_state = store
        .connection
        .query_row(
            "SELECT state FROM work_handoff_offers WHERE offer_id = ?1",
            [expired_offer.offer_id.0.to_string()],
            |row| row.get::<_, String>(0),
        )
        .expect("expired state");
    assert_eq!(expired_state, "expired");
    let expired_events = store
        .work_feed_after(&FeedId::RunExecution(initial.run_id), 0, 100)
        .expect("run feed")
        .into_iter()
        .filter(|entry| entry.object_kind == "work_event")
        .filter_map(|entry| {
            load_typed_work_object::<WorkEvent>(&store.connection, &entry.object_hash, "work_event")
                .ok()
        })
        .filter(|event| matches!(event.transition, WorkTransition::HandoffExpired { .. }))
        .count();
    assert_eq!(expired_events, 1);
}

#[test]
fn outgoing_holder_can_cancel_a_handoff_and_resume_progress() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("project-cancel", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let claim = claim(&mut store, &root, "agent-a", "claim", 1, 100);
    let offer = store
        .offer_work_handoff(
            &OfferWorkHandoffRequest {
                work_id: root.work_id,
                run_id: claim.run_id,
                expected_work_revision: root.revision,
                from: claim.holder.clone(),
                to: SessionId("agent-b".into()),
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                ttl_seconds: 30,
                checkpoint_summary: "possible transfer".into(),
                actor: actor("agent-a"),
                idempotency_key: "offer".into(),
                offered_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("offer");
    let cancelled = store
        .cancel_work_handoff(
            &CancelWorkHandoffRequest {
                work_id: root.work_id,
                run_id: claim.run_id,
                expected_work_revision: root.revision,
                holder: claim.holder.clone(),
                offer_id: offer.offer_id,
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                reason: "  destination did not accept  ".into(),
                actor: actor("agent-a"),
                idempotency_key: "cancel".into(),
                cancelled_at: at(3),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("cancel");
    assert_eq!(cancelled.state, WorkHandoffState::Cancelled);
    let cancelled_entry = store
        .work_event_tail(root.work_id, 1)
        .expect("handoff cancellation event")
        .pop()
        .expect("handoff cancellation tail");
    let cancelled_event: WorkEvent = load_typed_work_object(
        &store.connection,
        &cancelled_entry.object_hash,
        "work_event",
    )
    .expect("canonical handoff cancellation event");
    assert!(matches!(
        cancelled_event.transition,
        WorkTransition::HandoffCancelled { reason, .. }
            if reason == "destination did not accept"
    ));
    evidence(&mut store, &root, &claim, "agent-a", "resumed-evidence", 4);
}

#[test]
fn foreign_holder_cannot_commit_an_expired_handoff_sweep() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("project-cancel-auth", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let claim = claim(&mut store, &root, "agent-a", "claim", 1, 100);
    let offer = store
        .offer_work_handoff(
            &OfferWorkHandoffRequest {
                work_id: root.work_id,
                run_id: claim.run_id,
                expected_work_revision: root.revision,
                from: claim.holder.clone(),
                to: SessionId("agent-b".into()),
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                ttl_seconds: 2,
                checkpoint_summary: "short transfer window".into(),
                actor: actor("agent-a"),
                idempotency_key: "offer".into(),
                offered_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("offer");
    let events_before = store
        .work_event_tail(root.work_id, 100)
        .expect("event tail")
        .len();

    let refused = store.cancel_work_handoff(
        &CancelWorkHandoffRequest {
            work_id: root.work_id,
            run_id: claim.run_id,
            expected_work_revision: root.revision,
            holder: SessionId("intruder".into()),
            offer_id: offer.offer_id,
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
            reason: "expire another holder's offer".into(),
            actor: actor("intruder"),
            idempotency_key: "foreign-cancel".into(),
            cancelled_at: at(5),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(refused, Err(StoreError::InvalidWork(_))));
    let state: String = store
        .connection
        .query_row(
            "SELECT state FROM work_handoff_offers WHERE offer_id = ?1",
            [offer.offer_id.0.to_string()],
            |row| row.get(0),
        )
        .expect("offer state");
    assert_eq!(state, "offered");
    assert_eq!(
        store
            .work_event_tail(root.work_id, 100)
            .expect("event tail")
            .len(),
        events_before
    );
}
