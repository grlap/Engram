use super::*;
use crate::storage::work::test_support::*;
use crate::storage::work::*;

fn fractional_handoff() -> (SqliteStore, WorkItem, WorkClaim, WorkHandoffOffer) {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let root = store
        .create_work(
            &root_request("fractional-handoff", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let held = claim(&mut store, &root, "holder", "claim", 1, 7_200);
    let offer = store
        .offer_work_handoff(
            &OfferWorkHandoffRequest {
                work_id: root.work_id,
                run_id: held.run_id,
                expected_work_revision: root.revision,
                from: held.holder.clone(),
                to: SessionId("recipient".into()),
                claim_id: held.claim_id,
                claim_fence: held.fence,
                ttl_seconds: 30,
                checkpoint_summary: "fractional expiry".into(),
                actor: actor("holder"),
                idempotency_key: "offer".into(),
                offered_at: at(2) + Duration::microseconds(500),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let held = store.current_work_claim(root.work_id).unwrap().unwrap();
    assert_eq!(offer.expires_at, at(32) + Duration::microseconds(500));
    assert!(held.expires_at > offer.expires_at);
    (store, root, held, offer)
}

fn expiry_events(store: &SqliteStore, work: &WorkItem) -> usize {
    store
        .work_event_tail(work.work_id, 100)
        .unwrap()
        .into_iter()
        .filter(|entry| {
            let event: WorkEvent =
                load_typed_work_object(&store.connection, &entry.object_id, "work_event").unwrap();
            matches!(event.transition, WorkTransition::HandoffExpired { .. })
        })
        .count()
}

#[test]
fn damaged_handoff_index_refuses_a_cancelled_offer_expiry_sweep() {
    for damaged_index in [false, true] {
        let (mut store, root, held, offer) = fractional_handoff();
        store
            .cancel_work_handoff(
                &CancelWorkHandoffRequest {
                    work_id: root.work_id,
                    run_id: held.run_id,
                    expected_work_revision: root.revision,
                    holder: held.holder.clone(),
                    offer_id: offer.offer_id,
                    claim_id: held.claim_id,
                    claim_fence: held.fence,
                    reason: "cancel before the expiry sweep".into(),
                    actor: actor("holder"),
                    idempotency_key: "cancel".into(),
                    cancelled_at: at(3),
                },
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        if damaged_index {
            assert_eq!(
                store
                    .connection
                    .execute(
                        "UPDATE work_handoff_offers SET state = 'offered' WHERE offer_id = ?1",
                        [offer.offer_id.0.to_string()],
                    )
                    .unwrap(),
                1
            );
        }
        let before = test_database_shape_snapshot(&store.connection).unwrap();
        let result = store.record_work_note(
            &crate::domain::RecordWorkNoteRequest {
                status: false,
                work_id: root.work_id,
                run_id: held.run_id,
                expected_work_revision: root.revision,
                holder: held.holder.clone(),
                claim_id: held.claim_id,
                claim_fence: held.fence,
                summary: "holder progress after cancellation".into(),
                refs: Vec::new(),
                actor: actor("holder"),
                idempotency_key: "note-after-cancel".into(),
                recorded_at: at(33),
            },
            &DevelopmentNoopRedactor,
        );
        if damaged_index {
            let error = result.unwrap_err();
            let expected = format!(
                "handoff offer {:?} expiry index differs from its canonical state, run or expiry",
                offer.offer_id
            );
            assert!(matches!(
                &error,
                StoreError::InvalidWorkProjection(reason) if reason == &expected
            ));
            assert_eq!(
                error.to_string(),
                format!("work projection contains invalid data: {expected}")
            );
            assert_eq!(
                test_database_shape_snapshot(&store.connection).unwrap(),
                before
            );
        } else {
            result.unwrap();
            assert_eq!(
                store.work_handoff_offers(root.work_id).unwrap()[0].state,
                WorkHandoffState::Cancelled
            );
            assert!(store.verify_all().unwrap().is_healthy());
        }
        assert_eq!(expiry_events(&store, &root), 0);
    }
}

#[test]
fn fractional_handoff_notes_and_renewals_use_the_exact_expiry() {
    for operation in ["note", "status", "keyless-renewal", "keyed-renewal"] {
        for delta in [-1, 0, 1] {
            let (mut store, root, held, offer) = fractional_handoff();
            let now = offer.expires_at + Duration::nanoseconds(delta);
            let before = test_database_shape_snapshot(&store.connection).unwrap();
            assert_eq!(
                live_handoff_on(&store.connection, held.run_id, now)
                    .unwrap()
                    .is_some(),
                delta < 0,
            );
            assert_eq!(
                store
                    .work_completion_readiness(root.work_id, &held.holder, now)
                    .unwrap()
                    .0,
                delta >= 0,
            );
            let result = if operation.ends_with("renewal") {
                store
                    .claim_work(
                        &ClaimWorkRequest {
                            work_id: root.work_id,
                            expected_work_revision: root.revision,
                            expected_run_id: Some(held.run_id),
                            holder: held.holder.clone(),
                            ttl_seconds: 7_200,
                            recovery_reason: None,
                            actor: actor("holder"),
                            idempotency_key: if operation == "keyless-renewal" {
                                String::new()
                            } else {
                                "renew".into()
                            },
                            claimed_at: now,
                        },
                        &DevelopmentNoopRedactor,
                    )
                    .map(|_| ())
            } else {
                store
                    .record_work_note(
                        &crate::domain::RecordWorkNoteRequest {
                            status: operation == "status",
                            work_id: root.work_id,
                            run_id: held.run_id,
                            expected_work_revision: root.revision,
                            holder: held.holder.clone(),
                            claim_id: held.claim_id,
                            claim_fence: held.fence,
                            summary: "resume progress".into(),
                            refs: Vec::new(),
                            actor: actor("holder"),
                            idempotency_key: "note".into(),
                            recorded_at: now,
                        },
                        &DevelopmentNoopRedactor,
                    )
                    .map(|_| ())
            };
            if delta < 0 {
                assert!(
                    matches!(
                        result, Err(StoreError::WorkHandoffPending { offer_id })
                            if offer_id == offer.offer_id
                    ),
                    "{operation}: {result:?}"
                );
                assert_eq!(
                    test_database_shape_snapshot(&store.connection).unwrap(),
                    before
                );
                assert_eq!(expiry_events(&store, &root), 0);
            } else {
                result.unwrap();
                assert_eq!(
                    store.work_handoff_offers(root.work_id).unwrap()[0].state,
                    WorkHandoffState::Expired
                );
                assert_eq!(expiry_events(&store, &root), 1);
                evidence(&mut store, &root, &held, "holder", "later-evidence", 33);
                assert_eq!(expiry_events(&store, &root), 1);
            }
            assert!(store.verify_all().unwrap().is_healthy());
        }
    }
}

#[test]
fn fractional_handoff_accept_and_cancel_observe_the_exact_boundary() {
    for operation in ["accept", "cancel"] {
        for delta in [-1, 0] {
            let (mut store, root, held, offer) = fractional_handoff();
            let now = offer.expires_at + Duration::nanoseconds(delta);
            let result = if operation == "accept" {
                store
                    .accept_work_handoff(
                        &AcceptWorkHandoffRequest {
                            work_id: root.work_id,
                            offer_id: offer.offer_id,
                            to: offer.to.clone(),
                            actor: actor("recipient"),
                            idempotency_key: "accept".into(),
                            accepted_at: now,
                        },
                        &DevelopmentNoopRedactor,
                    )
                    .map(|_| ())
            } else {
                store
                    .cancel_work_handoff(
                        &CancelWorkHandoffRequest {
                            work_id: root.work_id,
                            run_id: held.run_id,
                            expected_work_revision: root.revision,
                            holder: held.holder.clone(),
                            offer_id: offer.offer_id,
                            claim_id: held.claim_id,
                            claim_fence: held.fence,
                            reason: "retain ownership".into(),
                            actor: actor("holder"),
                            idempotency_key: "cancel".into(),
                            cancelled_at: now,
                        },
                        &DevelopmentNoopRedactor,
                    )
                    .map(|_| ())
            };
            if delta < 0 {
                result.unwrap();
                assert_eq!(expiry_events(&store, &root), 0);
            } else {
                assert!(
                    matches!(result, Err(StoreError::InvalidWork(ref reason))
                    if reason == "handoff offer has expired"),
                    "{result:?}"
                );
                assert_eq!(
                    store.work_handoff_offers(root.work_id).unwrap()[0].state,
                    WorkHandoffState::Expired
                );
                assert_eq!(expiry_events(&store, &root), 1);
            }
            assert!(store.verify_all().unwrap().is_healthy());
        }
    }
}

#[test]
fn fractional_handoff_failed_authority_rolls_back_expiry_at_the_exact_boundary() {
    let (mut store, root, held, offer) = fractional_handoff();
    let before = test_database_shape_snapshot(&store.connection).unwrap();
    let result = store.record_work_note(
        &crate::domain::RecordWorkNoteRequest {
            status: false,
            work_id: root.work_id,
            run_id: held.run_id,
            expected_work_revision: root.revision,
            holder: held.holder.clone(),
            claim_id: held.claim_id,
            claim_fence: held.fence + 1,
            summary: "stale fence must not commit expiry".into(),
            refs: Vec::new(),
            actor: actor("holder"),
            idempotency_key: "stale-note".into(),
            recorded_at: offer.expires_at,
        },
        &DevelopmentNoopRedactor,
    );
    assert!(result.is_err());
    assert_eq!(
        test_database_shape_snapshot(&store.connection).unwrap(),
        before
    );
    assert_eq!(expiry_events(&store, &root), 0);
}

#[test]
fn fractional_handoff_exact_boundary_exclusion_does_not_hide_index_damage() {
    for damage in ["state", "run", "expiry"] {
        let (mut store, root, held, offer) = fractional_handoff();
        let mut selected_run = held.run_id;
        match damage {
            "state" => {
                store
                    .cancel_work_handoff(
                        &CancelWorkHandoffRequest {
                            work_id: root.work_id,
                            run_id: held.run_id,
                            expected_work_revision: root.revision,
                            holder: held.holder.clone(),
                            offer_id: offer.offer_id,
                            claim_id: held.claim_id,
                            claim_fence: held.fence,
                            reason: "cancel before damaging the index".into(),
                            actor: actor("holder"),
                            idempotency_key: "cancel".into(),
                            cancelled_at: at(3),
                        },
                        &DevelopmentNoopRedactor,
                    )
                    .unwrap();
                store
                    .connection
                    .execute(
                        "UPDATE work_handoff_offers SET state = 'offered' WHERE offer_id = ?1",
                        [offer.offer_id.0.to_string()],
                    )
                    .unwrap();
            }
            "run" => {
                let other = store
                    .create_work(
                        &root_request("fractional-handoff", "other", 0),
                        &DevelopmentNoopRedactor,
                    )
                    .unwrap();
                selected_run = other.active_run_id.unwrap();
                store
                    .connection
                    .execute(
                        "UPDATE work_handoff_offers SET run_id = ?2 WHERE offer_id = ?1",
                        params![offer.offer_id.0.to_string(), selected_run.0.to_string()],
                    )
                    .unwrap();
            }
            _ => {
                store.connection.execute(
                    "UPDATE work_handoff_offers SET expires_at_ms = expires_at_ms + 1 WHERE offer_id = ?1",
                    [offer.offer_id.0.to_string()],
                ).unwrap();
            }
        }
        let before = test_database_shape_snapshot(&store.connection).unwrap();
        assert!(matches!(
            live_handoff_on(&store.connection, selected_run, offer.expires_at),
            Err(StoreError::InvalidWorkProjection(_))
        ));
        assert_eq!(
            test_database_shape_snapshot(&store.connection).unwrap(),
            before
        );
    }
}

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
            load_typed_work_object::<WorkEvent>(&store.connection, &entry.object_id, "work_event")
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
    let cancelled_event: WorkEvent =
        load_typed_work_object(&store.connection, &cancelled_entry.object_id, "work_event")
            .expect("canonical handoff cancellation event");
    assert!(matches!(
        cancelled_event.transition,
        WorkTransition::HandoffCancelled { reason, .. }
            if reason == "destination did not accept"
    ));
    evidence(&mut store, &root, &claim, "agent-a", "resumed-evidence", 4);

    store
        .connection
        .execute(
            "UPDATE work_handoff_offers SET state = 'offered' WHERE offer_id = ?1",
            [offer.offer_id.0.to_string()],
        )
        .expect("damage only the indexed state of the canonical cancelled offer");
    let before = crate::storage::test_database_shape_snapshot(&store.connection).unwrap();
    let error = store
        .record_work_evidence(
            &RecordWorkEvidenceRequest {
                work_id: root.work_id,
                run_id: claim.run_id,
                expected_work_revision: root.revision,
                holder: claim.holder.clone(),
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                summary: "must not bypass damaged handoff index".into(),
                refs: Vec::new(),
                actor: actor("agent-a"),
                idempotency_key: "damaged-index-evidence".into(),
                recorded_at: at(5),
            },
            &DevelopmentNoopRedactor,
        )
        .expect_err("indexed live state cannot bypass the canonical cancelled offer");
    assert!(
        matches!(error, StoreError::InvalidWorkProjection(_)),
        "{error:?}"
    );
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&store.connection).unwrap(),
        before
    );
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

#[test]
fn offer_work_handoff_refuses_an_oversized_recipient_before_tx() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("project-handoff-admission", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let initial = claim(&mut store, &root, "agent-a", "claim-a", 1, 10);
    let offers_before: i64 = store
        .connection
        .query_row("SELECT COUNT(*) FROM work_handoff_offers", [], |row| {
            row.get(0)
        })
        .expect("count");
    let giant = SessionId("r".repeat(65));
    let error = store
        .offer_work_handoff(
            &OfferWorkHandoffRequest {
                work_id: root.work_id,
                run_id: initial.run_id,
                expected_work_revision: root.revision,
                from: initial.holder.clone(),
                to: giant.clone(),
                claim_id: initial.claim_id,
                claim_fence: initial.fence,
                ttl_seconds: 10,
                checkpoint_summary: "should not persist".into(),
                actor: actor("agent-a"),
                idempotency_key: "oversized-recipient".into(),
                offered_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect_err("oversized recipient");
    assert!(matches!(
        error,
        StoreError::InvalidWork(ref reason) if reason == crate::SessionIdAdmissionError::TooLong.as_str()
    ));
    assert!(!error.to_string().contains(&giant.0));
    let offers_after: i64 = store
        .connection
        .query_row("SELECT COUNT(*) FROM work_handoff_offers", [], |row| {
            row.get(0)
        })
        .expect("count");
    assert_eq!(offers_after, offers_before);
}
