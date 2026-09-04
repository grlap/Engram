use super::super::test_support::*;
use super::super::*;
use tempfile::tempdir;

#[test]
fn core_committed_handoff_recovery_uses_the_durable_focus_basis() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("committed-handoff-focus".into());
    let session = SessionId("committed-handoff-session".into());
    let service = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        session.clone(),
        Some("protocol-test".into()),
    );
    let first = match service
        .work_propose(root_input("Handoff original", "handoff-first"), at(0))
        .expect("first root")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "handoff-claim".into(),
            },
            at(1),
        )
        .expect("claim original focus");
    let input = WorkHandoffInput::Offer {
        to: "peer".into(),
        ttl_seconds: Some(300),
        checkpoint_summary: "durable handoff checkpoint".into(),
        idempotency_key: "committed-offer".into(),
    };

    let mut store = SqliteStore::open(&database).expect("store");
    let basis = service
        .protocol_basis(&store, true, true, None, at(2))
        .expect("original handoff basis");
    let intent = service.protocol_intent(&input);
    store
        .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
            project_id: &project,
            session_id: &session,
            operation: "work_handoff:offer",
            idempotency_key: "committed-offer",
            intent: &intent,
            basis: &basis,
            now: at(2),
        })
        .expect("begin handoff attempt");
    let original = basis.focused_work.clone().expect("original focus");
    let scoped_key = service
        .core_operation_key(
            "work_handoff:offer",
            "committed-offer",
            "offer_work_handoff",
        )
        .expect("scoped operation key");
    service
        .execute_work_handoff(
            &mut store,
            &basis,
            &original,
            input.clone(),
            scoped_key,
            at(2),
        )
        .expect("commit core handoff without protocol result");
    drop(store);

    let second = match service
        .work_propose(root_input("Handoff new focus", "handoff-second"), at(3))
        .expect("second root")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    let recovered = service
        .work_handoff(input.clone(), at(4))
        .expect("recover core-committed handoff");
    assert_eq!(recovered.receipt.work_id, first.work_id);
    assert_ne!(recovered.receipt.work_id, second.work_id);
    let replayed = service
        .work_handoff(input, at(5))
        .expect("replay exact handoff result");
    assert_eq!(
        serde_json::to_vec(&replayed).expect("serialize replay"),
        serde_json::to_vec(&recovered).expect("serialize recovery")
    );
    assert_eq!(
        SqliteStore::open(&database)
            .expect("store")
            .work_session_state(&project, &session, at(5))
            .expect("session")
            .focused_work_id,
        Some(second.work_id)
    );
}

#[test]
fn outgoing_handoff_expires_no_later_than_its_source_claim() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("lapsed-cancel".into());
    let service = LocalWorkService::new(
        database,
        project,
        "agent".into(),
        SessionId("lapsed-cancel-session".into()),
        Some("protocol-test".into()),
    );
    let work = match service
        .work_propose(root_input("Lapsed cancel", "lapsed-cancel-root"), at(0))
        .expect("root")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(60),
                recovery_reason: None,
                idempotency_key: "lapsed-cancel-claim".into(),
            },
            at(1),
        )
        .expect("claim");
    service
        .work_handoff(
            WorkHandoffInput::Offer {
                to: "successor".into(),
                ttl_seconds: Some(7_200),
                checkpoint_summary: "handoff is bounded by claim expiry".into(),
                idempotency_key: "lapsed-cancel-offer".into(),
            },
            at(2),
        )
        .expect("offer");
    let store = service.store().expect("store after offer");
    let claim = store
        .current_work_claim(work.work_id)
        .expect("claim projection")
        .expect("offering claim");
    let offer = store
        .work_handoff_offers(work.work_id)
        .expect("handoffs")
        .into_iter()
        .find(|offer| offer.state == WorkHandoffState::Offered)
        .expect("stored offer");
    assert_eq!(
        offer.expires_at, claim.expires_at,
        "an outgoing offer cannot outlive its source claim"
    );
    let event_count = store
        .work_event_tail(work.work_id, 64)
        .expect("events")
        .len();
    drop(store);

    let focus = service
        .work_focus(&work.short_ref, at(4_000))
        .expect("focus after claim and offer expiry");
    assert!(!focus.allowed_next.contains(&"work_handoff:cancel".into()));
    let refused = service
        .work_handoff(
            WorkHandoffInput::Cancel {
                reason: "cancel after lapse".into(),
                idempotency_key: "lapsed-cancel-attempt".into(),
            },
            at(4_000),
        )
        .expect_err("expired offer is not cancellable");
    assert!(matches!(
        &refused,
        StoreError::InvalidWork(reason)
            if reason == "ambient work has no live outgoing handoff offer"
    ));
    let store = service.store().expect("store after refusal");
    assert_eq!(
        store
            .current_work_claim(work.work_id)
            .expect("claim projection")
            .expect("retained claim"),
        claim
    );
    assert_eq!(
        store
            .work_event_tail(work.work_id, 64)
            .expect("events after refusal")
            .len(),
        event_count,
        "cancel refusal must not append a retake event"
    );
}
