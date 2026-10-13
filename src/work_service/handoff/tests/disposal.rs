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
    assert!(matches!(&error, StoreError::WorkHandoffPending { .. }));
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

#[test]
fn pending_handoff_notes_and_renewals_refuse_without_execution_effects() {
    let (directory, owner, recipient, work) = fixture();
    let store = SqliteStore::open(directory.path().join("handoff.sqlite3")).unwrap();
    let claim = store.current_work_claim(work.work_id).unwrap().unwrap();
    let run = store.get_work_run(claim.run_id).unwrap();
    let offers = store.work_handoff_offers(work.work_id).unwrap();
    let evidence = store.work_run_evidence(claim.run_id).unwrap();
    let events = store.work_event_count(work.work_id).unwrap();
    for operation in ["note", "status", "claim-keyless", "claim-keyed"] {
        let error = if operation.starts_with("claim") {
            owner
                .work_update(
                    WorkUpdateInput::Claim {
                        ttl_seconds: Some(7_200),
                        recovery_reason: None,
                        idempotency_key: if operation == "claim-keyed" {
                            "fresh-renewal".into()
                        } else {
                            String::new()
                        },
                    },
                    at(3),
                )
                .unwrap_err()
        } else {
            owner
                .work_note_with_status_on(
                    Some(&work.short_ref),
                    "Waiting for recipient",
                    &[],
                    operation == "status",
                    at(3),
                )
                .unwrap_err()
        };
        assert!(
            matches!(error, StoreError::WorkHandoffPending { offer_id } if offer_id == offers[0].offer_id),
            "{error:?}"
        );
        let value = crate::store_error_value(&error);
        assert_eq!(value["error"]["code"], "work_invalid");
        assert_eq!(
            value["error"]["details"]["offer_id"],
            serde_json::to_value(offers[0].offer_id).unwrap()
        );
        assert_eq!(
            value["error"]["details"]["reason"],
            crate::storage::PENDING_HANDOFF_REFUSAL
        );
        assert_eq!(
            store.current_work_claim(work.work_id).unwrap().unwrap(),
            claim
        );
        assert_eq!(store.get_work_run(claim.run_id).unwrap(), run);
        assert_eq!(store.work_handoff_offers(work.work_id).unwrap(), offers);
        assert_eq!(store.work_run_evidence(claim.run_id).unwrap(), evidence);
        assert_eq!(store.work_event_count(work.work_id).unwrap(), events);
    }
    let observation = recipient
        .work_note_with_status_on(
            Some(&work.short_ref),
            "Recipient is waiting",
            &[],
            true,
            at(4),
        )
        .unwrap();
    assert!(observation.non_holder);
    assert_eq!(store.get_work_run(claim.run_id).unwrap(), run);
    assert_eq!(
        store.current_work_claim(work.work_id).unwrap().unwrap(),
        claim
    );
    assert_eq!(store.work_handoff_offers(work.work_id).unwrap(), offers);
}

#[test]
fn pending_handoff_same_holder_note_is_admitted_after_cancel() {
    for status in [false, true] {
        let (directory, owner, _, work) = fixture();
        let store = SqliteStore::open(directory.path().join("handoff.sqlite3")).unwrap();
        let before = store.current_work_claim(work.work_id).unwrap().unwrap();
        let checkpoint = store.get_work_run(before.run_id).unwrap().last_checkpoint;
        let text = "Continue local work after recipient wait";
        let error = owner
            .work_note_with_status_on(Some(&work.short_ref), text, &[], status, at(3))
            .unwrap_err();
        assert!(matches!(error, StoreError::WorkHandoffPending { .. }));
        owner
            .work_handoff(
                WorkHandoffInput::Cancel {
                    reason: "Resume local work".into(),
                    idempotency_key: "cancel-for-note".into(),
                },
                at(4),
            )
            .unwrap();
        let note = owner
            .work_note_with_status_on(Some(&work.short_ref), text, &[], status, at(5))
            .unwrap();
        assert!(!note.non_holder);
        assert_ne!(
            store.get_work_run(before.run_id).unwrap().last_checkpoint,
            checkpoint
        );
        let after = store.current_work_claim(work.work_id).unwrap().unwrap();
        assert_eq!(after.claim_id, before.claim_id);
        assert_eq!(after.fence, before.fence);
        assert_eq!(
            store.work_handoff_offers(work.work_id).unwrap()[0].state,
            WorkHandoffState::Cancelled
        );
    }
}

#[test]
fn pending_handoff_waivable_child_is_advertised_and_admitted_for_each_session() {
    for caller in ["owner", "recipient", "observer"] {
        let (directory, owner, _, work) = fixture();
        owner
            .work_handoff(
                WorkHandoffInput::Cancel {
                    reason: "Prepare required child".into(),
                    idempotency_key: "cancel-first".into(),
                },
                at(3),
            )
            .unwrap();
        let WorkProposeResult::Decomposition(children) = owner
            .work_propose(
                WorkProposeInput::Decompose {
                    children: vec![WorkChildInput {
                        key: "omitted".into(),
                        title: "Omitted child".into(),
                        outcome: "Child outcome".into(),
                        acceptance: vec!["Child accepted".into()],
                        requirement: Some(ChildRequirement::Required),
                        kind: Some(WorkItemKind::Task),
                        priority: None,
                        labels: Vec::new(),
                        assigned_to: None,
                        deferred_until: None,
                        acceptance_bindings: Vec::new(),
                        evaluation_mode: None,
                        external_ref: None,
                        notes: Vec::new(),
                    }],
                    prerequisites: Vec::new(),
                    idempotency_key: "decompose".into(),
                },
                at(4),
            )
            .unwrap()
        else {
            panic!("expected decomposition")
        };
        let child = &children.children[0];
        owner.work_focus(&child.short_ref, at(5)).unwrap();
        owner
            .work_update_on(
                Some(&child.short_ref),
                WorkUpdateInput::Cancel {
                    reason: "Omit child outcome".into(),
                    idempotency_key: "dispose-child".into(),
                },
                at(6),
            )
            .unwrap();
        owner.work_focus(&work.short_ref, at(7)).unwrap();
        owner
            .work_handoff(
                WorkHandoffInput::Offer {
                    to: "recipient".into(),
                    ttl_seconds: Some(30),
                    checkpoint_summary: "Transfer parent".into(),
                    idempotency_key: "second-offer".into(),
                },
                at(8),
            )
            .unwrap();
        let service = LocalWorkService::new(
            directory.path().join("handoff.sqlite3"),
            owner.project_id.clone(),
            caller.into(),
            SessionId(caller.into()),
            None,
        );
        let focus = service.work_focus(&work.short_ref, at(9)).unwrap();
        let live_offers = focus
            .handoffs
            .iter()
            .filter(|offer| offer.state == WorkHandoffState::Offered)
            .collect::<Vec<_>>();
        let [live_offer] = live_offers.as_slice() else {
            panic!("expected exactly one live parent offer");
        };
        assert_eq!(focus.waivable_required_children.len(), 1);
        assert_eq!(focus.waivable_required_children[0].work_id, child.work_id);
        assert!(
            focus
                .allowed_next
                .contains(&"work_update:waive_required_child".into()),
            "{caller}"
        );
        service
            .work_update(
                WorkUpdateInput::WaiveRequiredChild {
                    child: child.short_ref.clone(),
                    reason: "Explicit omission".into(),
                    idempotency_key: "waive-under-offer".into(),
                },
                at(10),
            )
            .unwrap();
        let refreshed = service.work_focus(&work.short_ref, at(11)).unwrap();
        assert!(refreshed.waivable_required_children.is_empty());
        assert!(
            !refreshed
                .allowed_next
                .contains(&"work_update:waive_required_child".into())
        );
        let retained = refreshed
            .handoffs
            .iter()
            .find(|offer| offer.offer_id == live_offer.offer_id)
            .expect("waiver retains the same live offer");
        assert_eq!(
            serde_json::to_value(retained).unwrap(),
            serde_json::to_value(live_offer).unwrap()
        );
    }
}

#[test]
fn pending_handoff_every_advertised_holder_action_is_admitted() {
    let (_directory, owner, _, work) = fixture();
    let inspected = owner.work_inspect(&work.short_ref, at(3)).unwrap();
    assert!(inspected.view().control_binding.is_none());
    let actions = inspected.view().allowed_next.clone();
    assert_eq!(actions, ["work_focus", "work_handoff:cancel"]);
    for action in actions {
        let (_directory, owner, _, work) = fixture();
        match action.as_str() {
            "work_focus" => {
                owner.work_focus(&work.short_ref, at(3)).unwrap();
            }
            "work_handoff:cancel" => {
                owner
                    .work_handoff(
                        WorkHandoffInput::Cancel {
                            reason: "Resume holder work".into(),
                            idempotency_key: "cancel".into(),
                        },
                        at(3),
                    )
                    .unwrap();
            }
            other => panic!("advertised action has no admission proof: {other}"),
        }
    }
}

#[test]
fn pending_handoff_does_not_rewrite_successful_claim_replays() {
    let (directory, owner, _, work) = fixture();
    let store = SqliteStore::open(directory.path().join("handoff.sqlite3")).unwrap();
    let claim = store.current_work_claim(work.work_id).unwrap().unwrap();
    let offers = store.work_handoff_offers(work.work_id).unwrap();
    let input = WorkUpdateInput::Claim {
        ttl_seconds: Some(300),
        recovery_reason: None,
        idempotency_key: "claim".into(),
    };
    let first = owner.work_update(input.clone(), at(3)).unwrap();
    let second = owner.work_update(input, at(4)).unwrap();
    assert_eq!(
        serde_json::to_value(&first).unwrap(),
        serde_json::to_value(&second).unwrap()
    );
    assert!(first.allowed_next.contains(&"work_update:note".into()));
    assert_eq!(
        store.current_work_claim(work.work_id).unwrap().unwrap(),
        claim
    );
    assert_eq!(store.work_handoff_offers(work.work_id).unwrap(), offers);
}

#[test]
fn pending_handoff_guidance_matches_current_run_and_strict_expiry() {
    let (_directory, owner, _, work) = fixture();
    let store = owner.store().unwrap();
    let view = owner.work_guidance(&store, work.work_id, at(3)).unwrap();
    let offer = view.handoffs[0].clone();
    let actions = |offer: &WorkHandoffOffer, now| {
        allowed_next(
            &view.status,
            AllowedNextContext {
                claim: view.claim.as_ref(),
                handoffs: std::slice::from_ref(offer),
                session: &owner.session_id,
                now,
                can_waive_required_child: false,
                claim_recovery_required: false,
                completion_capture_ready: false,
                completion_preflight_ready: false,
            },
        )
    };
    assert!(
        !actions(&offer, offer.expires_at - chrono::Duration::milliseconds(1))
            .contains(&"work_update:note".into())
    );
    for now in [
        offer.expires_at,
        offer.expires_at + chrono::Duration::milliseconds(1),
    ] {
        assert!(actions(&offer, now).contains(&"work_update:note".into()));
    }
    for state in [
        WorkHandoffState::Accepted,
        WorkHandoffState::Cancelled,
        WorkHandoffState::Expired,
    ] {
        let mut resolved = offer.clone();
        resolved.state = state;
        assert!(actions(&resolved, at(3)).contains(&"work_update:note".into()));
    }
    let mut historical = offer;
    historical.run_id = WorkRunId::new();
    for tag in [
        "work_update:note",
        "work_update:checkpoint",
        "work_update:evidence",
        "work_update:release",
    ] {
        assert!(actions(&historical, at(3)).contains(&tag.into()));
    }
}
