use super::*;
use crate::{CancelWorkHandoffRequest, WorkHandoffState};

fn fixture() -> (
    SqliteStore,
    WorkItem,
    WorkItem,
    WorkClaim,
    WorkHandoffOffer,
    WorkItem,
) {
    fixture_on(SqliteStore::open_in_memory().unwrap())
}

fn fixture_on(
    mut store: SqliteStore,
) -> (
    SqliteStore,
    WorkItem,
    WorkItem,
    WorkClaim,
    WorkHandoffOffer,
    WorkItem,
) {
    let root = store
        .create_work(
            &root_request("dispose-handoff", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let children = store
        .decompose_work(
            &DecomposeWorkRequest {
                parent_id: root.work_id,
                expected_parent_revision: root.revision,
                children: vec![child("required", ChildRequirement::Required, "Child")],
                prerequisites: vec![],
                authority: delegated("dispose-handoff", "owner"),
                actor: actor("owner"),
                idempotency_key: "children".into(),
                created_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let work = children.children[0].clone();
    let held = claim(&mut store, &work, "owner", "claim", 2, 100);
    let offer = store
        .offer_work_handoff(
            &OfferWorkHandoffRequest {
                work_id: work.work_id,
                run_id: held.run_id,
                expected_work_revision: work.revision,
                from: held.holder.clone(),
                to: SessionId("recipient".into()),
                claim_id: held.claim_id,
                claim_fence: held.fence,
                ttl_seconds: 30,
                checkpoint_summary: "Transfer".into(),
                actor: actor("owner"),
                idempotency_key: "offer".into(),
                offered_at: at(3),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let replacement = store
        .create_work(
            &root_request("dispose-handoff", "replacement", 4),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    (store, children.parent, work, held, offer, replacement)
}

fn disposal(
    work: &WorkItem,
    replacement: &WorkItem,
    disposition: WorkDisposition,
    second: i64,
) -> DisposeWorkRequest {
    DisposeWorkRequest {
        work_id: work.work_id,
        expected_work_revision: work.revision,
        disposition,
        replacement_id: (disposition == WorkDisposition::Superseded).then_some(replacement.work_id),
        reason: "Outcome changed".into(),
        actor: actor("owner"),
        idempotency_key: "dispose".into(),
        disposed_at: at(second),
    }
}

fn cancel_offer(
    store: &mut SqliteStore,
    work: &WorkItem,
    held: &WorkClaim,
    offer: &WorkHandoffOffer,
) {
    store
        .cancel_work_handoff(
            &CancelWorkHandoffRequest {
                work_id: work.work_id,
                run_id: held.run_id,
                expected_work_revision: work.revision,
                holder: held.holder.clone(),
                offer_id: offer.offer_id,
                claim_id: held.claim_id,
                claim_fence: held.fence,
                reason: "Keep ownership".into(),
                actor: actor("owner"),
                idempotency_key: "cancel-offer".into(),
                cancelled_at: at(6),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
}

#[test]
fn disposal_handoff_cancel_and_supersede_refuse_without_effects_then_retry() {
    for disposition in [WorkDisposition::Cancelled, WorkDisposition::Superseded] {
        let (mut store, _, work, held, offer, replacement) = fixture();
        let request = disposal(&work, &replacement, disposition, 7);
        let before = test_database_shape_snapshot(&store.connection).unwrap();
        let error = store
            .dispose_work(&request, &DevelopmentNoopRedactor)
            .unwrap_err();
        assert!(
            matches!(error, StoreError::InvalidWork(ref reason) if reason == crate::storage::PENDING_HANDOFF_REFUSAL)
        );
        assert_eq!(
            test_database_shape_snapshot(&store.connection).unwrap(),
            before
        );
        let mut foreign = request.clone();
        foreign.actor = actor("stranger");
        assert!(
            matches!(store.dispose_work(&foreign, &DevelopmentNoopRedactor), Err(StoreError::InvalidWork(ref reason)) if reason.contains("does not match lifecycle holder"))
        );
        assert_eq!(
            test_database_shape_snapshot(&store.connection).unwrap(),
            before
        );
        cancel_offer(&mut store, &work, &held, &offer);
        let disposed = store
            .dispose_work(&request, &DevelopmentNoopRedactor)
            .unwrap();
        let after = test_database_shape_snapshot(&store.connection).unwrap();
        assert_eq!(
            store
                .dispose_work(&request, &DevelopmentNoopRedactor)
                .unwrap(),
            disposed
        );
        assert_eq!(
            test_database_shape_snapshot(&store.connection).unwrap(),
            after
        );
        assert!(store.verify_all().unwrap().is_healthy());
    }
}

#[test]
fn disposal_handoff_reject_refusal_and_failed_waiver_roll_back_everything() {
    let (mut store, parent, work, held, offer, _) = fixture();
    let mut request = RejectRequiredChildRequest {
        work_id: work.work_id,
        expected_work_revision: work.revision,
        expected_parent_revision: Some(parent.revision),
        reason: "Disproved".into(),
        actor: actor("owner"),
        idempotency_key: "reject".into(),
        rejected_at: at(7),
    };
    let before = test_database_shape_snapshot(&store.connection).unwrap();
    assert!(
        matches!(store.reject_required_child(&request, &DevelopmentNoopRedactor), Err(StoreError::InvalidWork(ref reason)) if reason == crate::storage::PENDING_HANDOFF_REFUSAL)
    );
    assert_eq!(
        test_database_shape_snapshot(&store.connection).unwrap(),
        before
    );
    store.connection.execute_batch("CREATE TEMP TRIGGER fail_waiver BEFORE INSERT ON objects WHEN NEW.object_kind = 'work_event' AND json_extract(NEW.canonical_json, '$.transition.kind') = 'required_child_waived' BEGIN SELECT RAISE(ABORT, 'test waiver failure'); END;").unwrap();
    request.rejected_at = offer.expires_at;
    let before = test_database_shape_snapshot(&store.connection).unwrap();
    assert!(
        store
            .reject_required_child(&request, &DevelopmentNoopRedactor)
            .unwrap_err()
            .to_string()
            .contains("test waiver failure")
    );
    assert_eq!(
        test_database_shape_snapshot(&store.connection).unwrap(),
        before,
        "expiry sweep and disposal roll back with waiver"
    );
    store
        .connection
        .execute_batch("DROP TRIGGER fail_waiver")
        .unwrap();
    request.rejected_at = at(7);
    cancel_offer(&mut store, &work, &held, &offer);
    let result = store
        .reject_required_child(&request, &DevelopmentNoopRedactor)
        .unwrap();
    assert_eq!(result.child.lifecycle, WorkLifecycle::Cancelled);
    let after = test_database_shape_snapshot(&store.connection).unwrap();
    assert_eq!(
        store
            .reject_required_child(&request, &DevelopmentNoopRedactor)
            .unwrap(),
        result
    );
    assert_eq!(
        test_database_shape_snapshot(&store.connection).unwrap(),
        after
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn disposal_handoff_expiry_boundary_allows_disposal_and_preserves_audit() {
    for second in [33, 34] {
        let (mut store, _, work, _, offer, replacement) = fixture();
        store
            .dispose_work(
                &disposal(&work, &replacement, WorkDisposition::Cancelled, second),
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        let offers = store.work_handoff_offers(work.work_id).unwrap();
        assert_eq!(offers[0].offer_id, offer.offer_id);
        assert_eq!(offers[0].state, WorkHandoffState::Expired);
        assert!(store.verify_all().unwrap().is_healthy());
    }
}

#[test]
fn disposal_handoff_historical_terminal_accept_and_cancel_preserve_bytes() {
    for disposition in [WorkDisposition::Cancelled, WorkDisposition::Superseded] {
        let (mut store, _, work, held, offer, replacement) = fixture();
        let disposed = store.test_dispose_with_historical_offer(&disposal(
            &work,
            &replacement,
            disposition,
            7,
        ));
        let before = test_database_shape_snapshot(&store.connection).unwrap();
        for second in [8, 34] {
            let accept = store
                .accept_work_handoff(
                    &AcceptWorkHandoffRequest {
                        work_id: work.work_id,
                        offer_id: offer.offer_id,
                        to: offer.to.clone(),
                        actor: actor("recipient"),
                        idempotency_key: format!("accept-{second}"),
                        accepted_at: at(second),
                    },
                    &DevelopmentNoopRedactor,
                )
                .unwrap_err();
            let cancel = store
                .cancel_work_handoff(
                    &CancelWorkHandoffRequest {
                        work_id: work.work_id,
                        run_id: held.run_id,
                        expected_work_revision: work.revision,
                        holder: held.holder.clone(),
                        offer_id: offer.offer_id,
                        claim_id: held.claim_id,
                        claim_fence: held.fence,
                        reason: "Historical".into(),
                        actor: actor("owner"),
                        idempotency_key: format!("cancel-{second}"),
                        cancelled_at: at(second),
                    },
                    &DevelopmentNoopRedactor,
                )
                .unwrap_err();
            for error in [accept, cancel] {
                let message = error.to_string();
                assert!(message.contains(&disposed.short_ref));
                assert!(
                    message.contains(if disposition == WorkDisposition::Cancelled {
                        "cancelled"
                    } else {
                        "superseded"
                    })
                );
                assert!(message.contains(&format!("engram work show {}", disposed.short_ref)));
            }
            assert_eq!(
                test_database_shape_snapshot(&store.connection).unwrap(),
                before
            );
        }
        assert_eq!(store.work_handoff_offers(work.work_id).unwrap()[0], offer);
    }
}

#[test]
fn disposal_handoff_accept_replay_survives_later_disposal() {
    let (mut store, _, work, _, offer, replacement) = fixture();
    let accept = AcceptWorkHandoffRequest {
        work_id: work.work_id,
        offer_id: offer.offer_id,
        to: offer.to.clone(),
        actor: actor("recipient"),
        idempotency_key: "accepted".into(),
        accepted_at: at(6),
    };
    let accepted = store
        .accept_work_handoff(&accept, &DevelopmentNoopRedactor)
        .unwrap();
    let mut request = disposal(&work, &replacement, WorkDisposition::Cancelled, 7);
    request.actor = actor("recipient");
    store
        .dispose_work(&request, &DevelopmentNoopRedactor)
        .unwrap();
    let before = test_database_shape_snapshot(&store.connection).unwrap();
    assert_eq!(
        store
            .accept_work_handoff(&accept, &DevelopmentNoopRedactor)
            .unwrap(),
        accepted
    );
    assert_eq!(
        test_database_shape_snapshot(&store.connection).unwrap(),
        before
    );
    let historical = store.current_work_claim(work.work_id).unwrap().unwrap();
    assert_eq!(historical.state, WorkClaimState::Released);
    assert_eq!(historical.fence, accepted.fence + 1);
    assert_eq!(historical.expires_at, request.disposed_at);
    assert_eq!(
        store.get_work_run(accepted.run_id).unwrap().state,
        WorkRunState::Cancelled
    );
}

#[test]
fn disposal_handoff_accept_and_dispose_serialize_across_connections() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("race.sqlite3");
    let (store, _, work, _, offer, replacement) = fixture_on(SqliteStore::open(&database).unwrap());
    let mut accept_store = SqliteStore::open(&database).unwrap();
    let mut dispose_store = SqliteStore::open(&database).unwrap();
    let start = std::sync::Barrier::new(2);
    std::thread::scope(|scope| {
        let accepted = scope.spawn(|| {
            start.wait();
            accept_store.accept_work_handoff(
                &AcceptWorkHandoffRequest {
                    work_id: work.work_id,
                    offer_id: offer.offer_id,
                    to: offer.to.clone(),
                    actor: actor("recipient"),
                    idempotency_key: "race-accept".into(),
                    accepted_at: at(7),
                },
                &DevelopmentNoopRedactor,
            )
        });
        let disposed = scope.spawn(|| {
            start.wait();
            dispose_store.dispose_work(
                &disposal(&work, &replacement, WorkDisposition::Cancelled, 7),
                &DevelopmentNoopRedactor,
            )
        });
        assert_eq!(accepted.join().unwrap().unwrap().holder, offer.to);
        let error = disposed.join().unwrap().unwrap_err();
        assert!(
            matches!(error, StoreError::InvalidWork(ref reason) if reason == crate::storage::PENDING_HANDOFF_REFUSAL || reason.contains("does not match lifecycle holder"))
        );
    });
    assert_eq!(
        store.get_work_item(work.work_id).unwrap().lifecycle,
        WorkLifecycle::Open
    );
    assert_eq!(
        store
            .current_work_claim(work.work_id)
            .unwrap()
            .unwrap()
            .holder,
        offer.to
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn disposal_handoff_offer_and_dispose_serialize_across_connections() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("race.sqlite3");
    let (mut store, _, work, held, offer, replacement) =
        fixture_on(SqliteStore::open(&database).unwrap());
    cancel_offer(&mut store, &work, &held, &offer);
    let mut offer_store = SqliteStore::open(&database).unwrap();
    let mut dispose_store = SqliteStore::open(&database).unwrap();
    let start = std::sync::Barrier::new(2);
    let (offered, disposed) = std::thread::scope(|scope| {
        let offered = scope.spawn(|| {
            start.wait();
            offer_store.offer_work_handoff(
                &OfferWorkHandoffRequest {
                    work_id: work.work_id,
                    run_id: held.run_id,
                    expected_work_revision: work.revision,
                    from: held.holder.clone(),
                    to: offer.to.clone(),
                    claim_id: held.claim_id,
                    claim_fence: held.fence,
                    ttl_seconds: 30,
                    checkpoint_summary: "Concurrent transfer".into(),
                    actor: actor("owner"),
                    idempotency_key: "race-offer".into(),
                    offered_at: at(7),
                },
                &DevelopmentNoopRedactor,
            )
        });
        let disposed = scope.spawn(|| {
            start.wait();
            dispose_store.dispose_work(
                &disposal(&work, &replacement, WorkDisposition::Cancelled, 7),
                &DevelopmentNoopRedactor,
            )
        });
        (offered.join().unwrap(), disposed.join().unwrap())
    });
    match (offered, disposed) {
        (Ok(_), Err(StoreError::InvalidWork(reason))) => {
            assert_eq!(reason, crate::storage::PENDING_HANDOFF_REFUSAL);
            assert_eq!(
                store.get_work_item(work.work_id).unwrap().lifecycle,
                WorkLifecycle::Open
            );
        }
        (
            Err(StoreError::WorkRevisionConflict { .. } | StoreError::WorkClaimMismatch { .. }),
            Ok(item),
        ) => {
            assert_eq!(item.lifecycle, WorkLifecycle::Cancelled);
            assert!(
                store
                    .work_handoff_offers(work.work_id)
                    .unwrap()
                    .iter()
                    .all(|offer| offer.state != WorkHandoffState::Offered)
            );
        }
        results => panic!("offer/dispose did not serialize: {results:?}"),
    }
    assert!(store.verify_all().unwrap().is_healthy());
}
