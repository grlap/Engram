use super::*;

fn fixture() -> (
    crate::test_support::TempHome,
    LocalWorkService,
    LocalWorkService,
    WorkItemSummary,
) {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("handoff.sqlite3");
    let project = ProjectId("service-disposal".into());
    let owner = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "owner".into(),
        SessionId("owner".into()),
        None,
    );
    let recipient = LocalWorkService::new(
        database,
        project,
        "recipient".into(),
        SessionId("recipient".into()),
        None,
    );
    let work = proposed_root(
        owner
            .work_propose(root_input("Transfer", "root"), at(0))
            .unwrap(),
    );
    owner
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "claim".into(),
            },
            at(1),
        )
        .unwrap();
    owner
        .work_handoff(
            WorkHandoffInput::Offer {
                to: "recipient".into(),
                ttl_seconds: Some(30),
                checkpoint_summary: "Transfer".into(),
                idempotency_key: "offer".into(),
            },
            at(2),
        )
        .unwrap();
    (directory, owner, recipient, work)
}

#[test]
fn disposal_handoff_guidance_ignores_noncurrent_or_resolved_offers() {
    let (_directory, owner, _, work) = fixture();
    let store = owner.store().unwrap();
    let guidance = owner.work_guidance(&store, work.work_id, at(3)).unwrap();
    let offer = guidance.handoffs[0].clone();
    let claim = guidance.claim.as_ref();
    let actions = |status: &ReadyWork, offers: &[WorkHandoffOffer], now| {
        allowed_next(
            status,
            AllowedNextContext {
                claim,
                handoffs: offers,
                session: &owner.session_id,
                now,
                can_waive_required_child: false,
                claim_recovery_required: false,
                completion_capture_ready: false,
                completion_preflight_ready: false,
            },
        )
    };
    let admitted = |next: Vec<String>| {
        for action in ["work_update:cancel", "work_update:supersede"] {
            assert!(next.contains(&action.to_owned()));
        }
    };
    let mut historic = offer.clone();
    historic.run_id = WorkRunId::new();
    admitted(actions(&guidance.status, &[historic], at(3)));
    let mut without_run = guidance.status.clone();
    without_run.work.active_run_id = None;
    admitted(actions(&without_run, std::slice::from_ref(&offer), at(3)));
    for state in [
        WorkHandoffState::Accepted,
        WorkHandoffState::Cancelled,
        WorkHandoffState::Expired,
    ] {
        let mut resolved = offer.clone();
        resolved.state = state;
        admitted(actions(&guidance.status, &[resolved], at(3)));
    }
    for now in [
        offer.expires_at,
        offer.expires_at + chrono::Duration::seconds(1),
    ] {
        admitted(actions(&guidance.status, std::slice::from_ref(&offer), now));
    }
    // Disposal is blocked by the current run's offer regardless of its sender.
    let mut other_sender = offer;
    other_sender.from = SessionId("earlier-holder".into());
    let next = actions(&guidance.status, &[other_sender], at(3));
    for action in ["work_update:cancel", "work_update:supersede"] {
        assert!(!next.contains(&action.to_owned()));
    }
}

#[test]
fn disposal_handoff_service_retry_after_authorized_cancel_is_admitted() {
    let (_directory, owner, _, work) = fixture();
    let cancel = WorkUpdateInput::Cancel {
        reason: "Finish elsewhere".into(),
        idempotency_key: "dispose".into(),
    };
    let error = owner.work_update(cancel.clone(), at(3)).unwrap_err();
    assert!(
        matches!(&error, StoreError::InvalidWork(reason) if reason == crate::storage::PENDING_HANDOFF_REFUSAL)
    );
    let guidance = crate::verbs::VerbError::from(error).guidance();
    assert_eq!(
        guidance.next,
        vec!["engram work handoff <ref> --cancel \"…\"".to_owned()]
    );
    let before = owner
        .store()
        .unwrap()
        .work_handoff_offers(work.work_id)
        .unwrap();
    assert_eq!(before[0].state, WorkHandoffState::Offered);
    owner
        .work_handoff(
            WorkHandoffInput::Cancel {
                reason: "Retain ownership".into(),
                idempotency_key: "cancel-offer".into(),
            },
            at(4),
        )
        .unwrap();
    owner.work_update(cancel.clone(), at(5)).unwrap();
    owner.work_update(cancel, at(6)).unwrap();
    assert_eq!(
        owner
            .store()
            .unwrap()
            .get_work_item(work.work_id)
            .unwrap()
            .lifecycle,
        WorkLifecycle::Cancelled
    );
}

#[test]
fn disposal_handoff_service_accept_replay_precedes_terminal_guard() {
    let (directory, _, recipient, work) = fixture();
    let accept = WorkHandoffInput::Accept {
        idempotency_key: "accept".into(),
    };
    let receipt = recipient
        .work_handoff_on(Some(&work.short_ref), accept.clone(), at(3))
        .unwrap();
    recipient
        .work_update(
            WorkUpdateInput::Cancel {
                reason: "No longer needed".into(),
                idempotency_key: "dispose".into(),
            },
            at(4),
        )
        .unwrap();
    let store = recipient.store().unwrap();
    let released = store.current_work_claim(work.work_id).unwrap().unwrap();
    assert_eq!(released.state, WorkClaimState::Released);
    assert_eq!(released.expires_at, at(4));
    let connection = rusqlite::Connection::open(directory.path().join("handoff.sqlite3")).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
    drop(store);
    let replay = recipient.work_handoff(accept, at(5)).unwrap();
    assert_eq!(
        serde_json::to_value(replay).unwrap(),
        serde_json::to_value(receipt).unwrap()
    );
    let store = recipient.store().unwrap();
    assert_eq!(
        store.current_work_claim(work.work_id).unwrap().unwrap(),
        released
    );
    assert_eq!(
        store.get_work_run(released.run_id).unwrap().state,
        WorkRunState::Cancelled
    );
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&connection).unwrap(),
        before
    );
}
