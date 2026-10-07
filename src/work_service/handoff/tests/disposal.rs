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
