use super::*;

fn claimed_service(database: std::path::PathBuf) -> LocalWorkService {
    let service = LocalWorkService::new(
        database,
        ProjectId("completion-identity".into()),
        "agent".into(),
        SessionId("completion-identity-session".into()),
        None,
    );
    service
        .work_propose(root_input("Stable completion", "root"), at(0))
        .expect("root");
    claim(&service, at(1));
    service
}

fn claim(service: &LocalWorkService, now: DateTime<Utc>) {
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: String::new(),
            },
            now,
        )
        .expect("claim");
}

fn identity(service: &LocalWorkService, input: &WorkCompleteInput, now: DateTime<Utc>) -> String {
    let store = service.store().expect("store");
    let basis = service
        .protocol_basis(&store, true, false, None, now)
        .expect("basis");
    service
        .effective_idempotency_key(
            "",
            "work_complete",
            &basis,
            &service.protocol_intent(input),
            now,
        )
        .expect("derived identity")
}

fn attempts(service: &LocalWorkService) -> Vec<(String, bool)> {
    let connection = rusqlite::Connection::open(&service.database).expect("fixture connection");
    connection
        .prepare("SELECT idempotency_key, result_json IS NOT NULL FROM work_protocol_attempts WHERE operation = 'work_complete' ORDER BY idempotency_key")
        .expect("attempt query")
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .expect("attempt rows")
        .collect::<Result<_, _>>()
        .expect("attempt values")
}

fn release(service: &LocalWorkService, now: DateTime<Utc>) {
    service
        .work_update(
            WorkUpdateInput::Release {
                reason: "release fixture claim".into(),
                waiver_reason: Some("fixture holder releases its contribution".into()),
                idempotency_key: String::new(),
            },
            now,
        )
        .expect("release");
}

fn assert_reclaimed_attempt_completes(
    service: &LocalWorkService,
    input: WorkCompleteInput,
    original: &WorkProtocolBasis,
    key: String,
    now: DateTime<Utc>,
) {
    let current = service
        .protocol_basis(&service.store().unwrap(), true, false, None, now)
        .unwrap();
    let old_claim = original.claim.as_ref().unwrap();
    let new_claim = current.claim.as_ref().unwrap();
    assert_eq!(old_claim.run_id, new_claim.run_id);
    assert_eq!(old_claim.claim_id, new_claim.claim_id);
    assert!(new_claim.fence > old_claim.fence);
    assert_eq!(new_claim.holder, service.session_id);
    assert_eq!(new_claim.state, WorkClaimState::Active);
    assert_eq!(key, identity(service, &input, now));
    assert_eq!(attempts(service), vec![(key.clone(), false)]);
    let result = service.work_complete(input.clone(), now).expect("retry");
    match &result {
        WorkCompleteResult::Completed(receipt) => assert_eq!(receipt.run_id, old_claim.run_id),
        WorkCompleteResult::Refused(_) => panic!("reclaimed completion refused"),
    }
    assert_eq!(attempts(service), vec![(key.clone(), true)]);
    let replay = service.clone().work_complete(input, now).expect("replay");
    assert_eq!(
        serde_json::to_value(result).unwrap(),
        serde_json::to_value(replay).unwrap()
    );
    assert_eq!(attempts(service), vec![(key, true)]);
}

#[test]
fn keyless_completion_retries_after_its_released_claim_is_reclaimed() {
    let directory = crate::test_support::temp_home().expect("temp");
    let service = claimed_service(directory.path().join("store.db"));
    release(&service, at(2));
    let input = completion_input("delivered", "");
    let original = service
        .protocol_basis(&service.store().unwrap(), true, false, None, at(3))
        .unwrap();
    assert_eq!(
        original.claim.as_ref().unwrap().state,
        WorkClaimState::Released
    );
    let key = identity(&service, &input, at(3));
    assert!(matches!(
        service.work_complete(input.clone(), at(3)),
        Err(StoreError::WorkClaimMismatch { .. })
    ));
    claim(&service, at(4));
    assert_reclaimed_attempt_completes(&service, input, &original, key, at(5));
}

#[test]
fn keyless_completion_retries_after_a_foreign_claim_is_reclaimed() {
    let directory = crate::test_support::temp_home().expect("temp");
    let service = claimed_service(directory.path().join("store.db"));
    release(&service, at(2));
    let work = service
        .protocol_basis(&service.store().unwrap(), true, false, None, at(2))
        .unwrap()
        .focused_work
        .unwrap();
    let peer = LocalWorkService::new(
        service.database.clone(),
        service.project_id.clone(),
        "peer".into(),
        SessionId("prior-holder".into()),
        None,
    );
    peer.work_focus(&work.short_ref, at(3)).unwrap();
    peer.work_update(
        WorkUpdateInput::Claim {
            ttl_seconds: Some(300),
            recovery_reason: Some("take released fixture".into()),
            idempotency_key: String::new(),
        },
        at(4),
    )
    .expect("peer claim");
    let input = completion_input("delivered", "");
    let original = service
        .protocol_basis(&service.store().unwrap(), true, false, None, at(5))
        .unwrap();
    assert_eq!(original.claim.as_ref().unwrap().holder, peer.session_id);
    let key = identity(&service, &input, at(5));
    assert!(matches!(
        service.work_complete(input.clone(), at(5)),
        Err(StoreError::WorkClaimMismatch { .. })
    ));
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: Some("recover expired peer fixture".into()),
                idempotency_key: String::new(),
            },
            at(400),
        )
        .expect("own recovery");
    assert_reclaimed_attempt_completes(&service, input, &original, key, at(401));
}

#[test]
fn keyless_completion_retries_after_its_lapsed_claim_is_reclaimed() {
    let directory = crate::test_support::temp_home().expect("temp");
    let service = claimed_service(directory.path().join("store.db"));
    let input = completion_input("delivered", "");
    let original = service
        .protocol_basis(&service.store().unwrap(), true, false, None, at(302))
        .unwrap();
    assert_eq!(
        original.claim.as_ref().unwrap().state,
        WorkClaimState::Active
    );
    assert!(original.claim.as_ref().unwrap().expires_at < at(302));
    let key = identity(&service, &input, at(302));
    assert!(matches!(
        service.work_complete(input.clone(), at(302)),
        Err(StoreError::WorkClaimLapsed { .. })
    ));
    claim(&service, at(303));
    assert_reclaimed_attempt_completes(&service, input, &original, key, at(304));
}

#[test]
fn keyless_completion_identity_survives_its_terminal_transition() {
    let directory = crate::test_support::temp_home().expect("temp");
    let service = claimed_service(directory.path().join("store.db"));
    let input = completion_input("delivered", "");
    let before = identity(&service, &input, at(2));
    let original = service
        .protocol_basis(&service.store().unwrap(), true, false, None, at(2))
        .unwrap();
    assert!(matches!(
        service
            .work_complete(input.clone(), at(2))
            .expect("complete"),
        WorkCompleteResult::Completed(_)
    ));
    let terminal = service
        .protocol_basis(&service.store().unwrap(), true, false, None, at(3))
        .unwrap();
    let original_work = original.focused_work.unwrap();
    let terminal_work = terminal.focused_work.unwrap();
    assert_eq!(original_work.lifecycle, WorkLifecycle::Open);
    assert_eq!(terminal_work.lifecycle, WorkLifecycle::Completed);
    assert!(terminal_work.revision > original_work.revision);
    assert!(original_work.active_run_id.is_some());
    assert!(terminal_work.active_run_id.is_none());
    let original_claim = original.claim.unwrap();
    let terminal_claim = terminal.claim.unwrap();
    assert_eq!(original_claim.run_id, terminal_claim.run_id);
    assert_eq!(original_claim.state, WorkClaimState::Active);
    assert_eq!(terminal_claim.state, WorkClaimState::Completed);
    assert!(terminal_claim.fence > original_claim.fence);
    assert!(terminal_claim.revision > original_claim.revision);
    assert_eq!(before, identity(&service, &input, at(3)));
    assert_eq!(attempts(&service), vec![(before, true)]);
}

#[test]
fn keyless_completion_replay_uses_the_original_attempt_not_a_fresh_fallback() {
    let directory = crate::test_support::temp_home().expect("temp");
    let service = claimed_service(directory.path().join("store.db"));
    let input = completion_input("delivered", "");
    let first = service
        .work_complete(input.clone(), at(2))
        .expect("complete");
    let before = attempts(&service);
    let connection = rusqlite::Connection::open(&service.database).expect("fixture connection");
    let snapshot = crate::storage::test_database_shape_snapshot(&connection).expect("snapshot");
    let restarted = service.clone();
    let replay = restarted
        .work_complete(input, at(3))
        .expect("lost response retry");
    assert_eq!(
        serde_json::to_value(first).unwrap(),
        serde_json::to_value(replay).unwrap()
    );
    assert_eq!(attempts(&service), before, "no second fallback attempt");
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&connection).unwrap(),
        snapshot
    );
}

#[test]
fn keyless_completion_finishes_its_original_pending_attempt_after_core_commit() {
    let directory = crate::test_support::temp_home().expect("temp");
    let service = claimed_service(directory.path().join("store.db"));
    let input = completion_input("delivered", "");
    let key = identity(&service, &input, at(2));
    let seal = commit_completion_core_without_finishing(&service, &input, at(2));
    assert_eq!(attempts(&service), vec![(key.clone(), false)]);
    let replay = service
        .clone()
        .work_complete(input, at(3))
        .expect("recover core commit");
    let WorkCompleteResult::Completed(replay) = replay else {
        panic!("completed")
    };
    assert_eq!(replay.run_id, seal.run_id);
    assert_eq!(replay.completed_at, seal.completed_at);
    assert_eq!(replay.seal, *CanonicalObject::freeze(&seal).unwrap().hash());
    assert_eq!(
        attempts(&service),
        vec![(key, true)],
        "finish the pending row, not a new fallback row"
    );
}

#[test]
fn keyless_completion_reopen_means_a_new_attempt_under_new_run_authority() {
    let directory = crate::test_support::temp_home().expect("temp");
    let service = claimed_service(directory.path().join("store.db"));
    let input = completion_input("delivered", "");
    let old_key = identity(&service, &input, at(2));
    let WorkCompleteResult::Completed(first) = service.work_complete(input.clone(), at(2)).unwrap()
    else {
        panic!("completed")
    };
    service
        .work_update(
            WorkUpdateInput::Reopen {
                reason: "new execution".into(),
                idempotency_key: "reopen".into(),
            },
            at(3),
        )
        .expect("reopen");
    assert_ne!(identity(&service, &input, at(4)), old_key);
    assert!(
        matches!(
            service.work_complete(input.clone(), at(4)),
            Err(StoreError::WorkClaimMismatch { .. })
        ),
        "old completion is not new authority"
    );
    let unclaimed_key = identity(&service, &input, at(4));
    assert!(attempts(&service).contains(&(unclaimed_key.clone(), false)));
    claim(&service, at(5));
    let key = identity(&service, &input, at(6));
    assert_eq!(key, unclaimed_key);
    let WorkCompleteResult::Completed(second) = service
        .work_complete(input.clone(), at(6))
        .expect("new completion")
    else {
        panic!("completed")
    };
    assert_ne!(second.run_id, first.run_id);
    assert_ne!(second.seal, first.seal);
    assert_eq!(second.completed_at, at(6));
    let replay = service
        .clone()
        .work_complete(input, at(7))
        .expect("new run retry");
    assert_eq!(
        serde_json::to_value(&second).unwrap(),
        match replay {
            WorkCompleteResult::Completed(receipt) => serde_json::to_value(receipt).unwrap(),
            WorkCompleteResult::Refused(_) => panic!("completed"),
        }
    );
    assert!(attempts(&service).contains(&(key, true)));
    assert_eq!(attempts(&service).len(), 2, "one attempt per run");
}

#[test]
fn keyless_completion_without_a_native_run_refuses_until_claim_bootstraps_one() {
    let directory = crate::test_support::temp_home().expect("temp");
    let source = claimed_service(directory.path().join("source.db"));
    let saved = source
        .save_work_graph_snapshot(None, WorkGraphSnapshotDestinationKind::Stdout, at(2))
        .expect("snapshot");
    let service = LocalWorkService::new(
        directory.path().join("restored.db"),
        source.project_id.clone(),
        "agent".into(),
        source.session_id.clone(),
        None,
    );
    service
        .load_work_graph_snapshot(&serde_json::to_vec(&saved.document).unwrap(), false, at(3))
        .expect("restore");
    let work = saved.document.body.items[0].work_id;
    service
        .work_focus(&work.0.to_string(), at(4))
        .expect("focus restored work");
    let basis = service
        .protocol_basis(&service.store().unwrap(), true, false, None, at(4))
        .unwrap();
    assert!(basis.focused_work.unwrap().active_run_id.is_none());
    assert!(basis.claim.is_none());
    let input = completion_input("delivered", "");
    let key = identity(&service, &input, at(4));
    assert!(matches!(
        service.work_complete(input.clone(), at(4)),
        Err(StoreError::WorkClaimMismatch { .. })
    ));
    assert_eq!(key, identity(&service, &input, at(5)));
    claim(&service, at(6));
    assert_ne!(key, identity(&service, &input, at(7)));
    assert!(matches!(
        service
            .work_complete(input, at(7))
            .expect("bootstrapped completion"),
        WorkCompleteResult::Completed(_)
    ));
}

#[test]
fn keyless_completion_pending_attempt_does_not_refresh_to_a_foreign_holder() {
    let directory = crate::test_support::temp_home().expect("temp");
    let service = claimed_service(directory.path().join("store.db"));
    let input = completion_input("delivered", "");
    let key = identity(&service, &input, at(2));
    let original_basis = {
        let mut store = service.store().unwrap();
        let basis = service
            .protocol_basis(&store, true, false, None, at(2))
            .unwrap();
        store
            .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
                project_id: &service.project_id,
                session_id: &service.session_id,
                operation: "work_complete",
                idempotency_key: &key,
                intent: &service.protocol_intent(&input),
                basis: &basis,
                now: at(2),
            })
            .unwrap();
        basis
    };
    let peer = LocalWorkService::new(
        service.database.clone(),
        service.project_id.clone(),
        "peer".into(),
        SessionId("peer-session".into()),
        None,
    );
    peer.work_focus(
        &original_basis.focused_work.as_ref().unwrap().short_ref,
        at(400),
    )
    .unwrap();
    peer.work_update(
        WorkUpdateInput::Claim {
            ttl_seconds: Some(300),
            recovery_reason: Some("recover expired claim".into()),
            idempotency_key: "peer-claim".into(),
        },
        at(401),
    )
    .expect("peer takeover");
    assert!(matches!(
        service.work_complete(input.clone(), at(402)),
        Err(StoreError::WorkClaimMismatch { .. })
    ));
    let attempt = service
        .store()
        .unwrap()
        .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
            project_id: &service.project_id,
            session_id: &service.session_id,
            operation: "work_complete",
            idempotency_key: &key,
            intent: &service.protocol_intent(&input),
            basis: &original_basis,
            now: at(403),
        })
        .unwrap();
    assert!(attempt.result.is_none());
    assert_eq!(
        attempt.basis.unwrap(),
        serde_json::to_value(original_basis).unwrap()
    );
}

#[test]
fn keyless_completion_unclaimed_pending_cannot_refresh_to_a_peer_claim() {
    let directory = crate::test_support::temp_home().expect("temp");
    let service = claimed_service(directory.path().join("store.db"));
    service
        .work_complete(completion_input("first", ""), at(2))
        .unwrap();
    service
        .work_update(
            WorkUpdateInput::Reopen {
                reason: "new run".into(),
                idempotency_key: "reopen".into(),
            },
            at(3),
        )
        .unwrap();
    let input = completion_input("new run", "");
    assert!(matches!(
        service.work_complete(input.clone(), at(4)),
        Err(StoreError::WorkClaimMismatch { .. })
    ));
    let original_basis = service
        .protocol_basis(&service.store().unwrap(), true, false, None, at(4))
        .unwrap();
    assert!(original_basis.claim.is_none());
    let key = identity(&service, &input, at(4));
    let peer = LocalWorkService::new(
        service.database.clone(),
        service.project_id.clone(),
        "peer".into(),
        SessionId("new-holder".into()),
        None,
    );
    peer.work_focus(
        &original_basis.focused_work.as_ref().unwrap().short_ref,
        at(5),
    )
    .unwrap();
    claim(&peer, at(6));
    assert!(matches!(
        service.work_complete(input.clone(), at(7)),
        Err(StoreError::WorkClaimMismatch { .. })
    ));
    let attempt = service
        .store()
        .unwrap()
        .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
            project_id: &service.project_id,
            session_id: &service.session_id,
            operation: "work_complete",
            idempotency_key: &key,
            intent: &service.protocol_intent(&input),
            basis: &original_basis,
            now: at(8),
        })
        .unwrap();
    assert!(attempt.result.is_none());
    assert_eq!(
        attempt.basis.unwrap(),
        serde_json::to_value(original_basis).unwrap()
    );
}

#[test]
fn keyless_completion_pending_refusal_preserves_fresh_authority_guidance() {
    for transition in ["peer recovery", "release", "cancel"] {
        let directory = crate::test_support::temp_home().expect("temp");
        let service = claimed_service(directory.path().join("store.db"));
        let input = completion_input("delivered", "");
        let key = identity(&service, &input, at(302));
        assert!(matches!(
            service.work_complete(input.clone(), at(302)),
            Err(StoreError::WorkClaimLapsed { .. })
        ));
        let original = service
            .protocol_basis(&service.store().unwrap(), true, false, None, at(302))
            .unwrap();
        let work = original.focused_work.as_ref().unwrap();
        match transition {
            "peer recovery" => {
                let peer = LocalWorkService::new(
                    service.database.clone(),
                    service.project_id.clone(),
                    "peer".into(),
                    SessionId("recovered-holder".into()),
                    None,
                );
                peer.work_focus(&work.short_ref, at(303)).unwrap();
                peer.work_update(
                    WorkUpdateInput::Claim {
                        ttl_seconds: Some(300),
                        recovery_reason: Some("recover expired fixture".into()),
                        idempotency_key: String::new(),
                    },
                    at(304),
                )
                .expect("peer recovery");
            }
            "release" => {
                claim(&service, at(303));
                release(&service, at(304));
            }
            "cancel" => {
                claim(&service, at(303));
                service
                    .work_update(
                        WorkUpdateInput::Cancel {
                            reason: "cancel fixture".into(),
                            idempotency_key: String::new(),
                        },
                        at(304),
                    )
                    .expect("cancel");
            }
            _ => unreachable!(),
        }
        assert_eq!(key, identity(&service, &input, at(305)), "{transition}");
        let connection = rusqlite::Connection::open(&service.database).unwrap();
        let snapshot = crate::storage::test_database_shape_snapshot(&connection).unwrap();
        let retry_error = service.work_complete(input, at(305)).expect_err(transition);
        assert_eq!(
            snapshot,
            crate::storage::test_database_shape_snapshot(&connection).unwrap(),
            "pending refusal must change neither rows nor schema: {transition}"
        );
        assert_eq!(attempts(&service), vec![(key, false)]);
        let fresh_error = service
            .work_complete(completion_input("different intent", ""), at(305))
            .expect_err("fresh attempt must also refuse");
        assert!(matches!(fresh_error, StoreError::WorkClaimMismatch { .. }));
        assert_eq!(
            std::mem::discriminant(&retry_error),
            std::mem::discriminant(&fresh_error),
            "pending attempt must preserve the fresh refusal class: {transition}"
        );
        let mut word_error = crate::verbs::VerbError::from(retry_error);
        word_error.work_ref = Some(work.short_ref.clone());
        let guidance = word_error.guidance();
        assert_eq!(
            guidance.reminders,
            vec!["this operation needs current claim authority; show the item before retrying"]
        );
        assert_eq!(
            guidance.next,
            vec![format!("engram work show {}", work.short_ref)]
        );
    }
}
