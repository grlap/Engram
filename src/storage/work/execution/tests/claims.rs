use super::*;

#[test]
fn delegated_planning_cannot_revise_a_foreign_live_claim() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("project-claimed-planning", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let live_claim = claim(&mut store, &root, "holder", "holder-claim", 1, 300);
    let delegated_result = store.decompose_work(
        &DecomposeWorkRequest {
            parent_id: root.work_id,
            expected_parent_revision: root.revision,
            children: vec![
                child("first", ChildRequirement::Required, "First"),
                child("second", ChildRequirement::Required, "Second"),
            ],
            prerequisites: Vec::new(),
            authority: delegated(&root.project_id.0, "planner"),
            actor: actor("planner"),
            idempotency_key: "foreign-delegated-plan".into(),
            created_at: at(2),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(delegated_result, Err(StoreError::InvalidWork(_))));
    assert_eq!(store.get_work_item(root.work_id).unwrap(), root);
    assert_eq!(
        store.current_work_claim(root.work_id).unwrap(),
        Some(live_claim.clone())
    );

    let decomposition = store
        .decompose_work(
            &DecomposeWorkRequest {
                parent_id: root.work_id,
                expected_parent_revision: root.revision,
                children: vec![
                    child("first", ChildRequirement::Required, "First"),
                    child("second", ChildRequirement::Required, "Second"),
                ],
                prerequisites: Vec::new(),
                authority: WorkPlanningAuthority::Claim {
                    run_id: live_claim.run_id,
                    holder: live_claim.holder.clone(),
                    claim_id: live_claim.claim_id,
                    claim_fence: live_claim.fence,
                },
                actor: actor("holder"),
                idempotency_key: "holder-claim-plan".into(),
                created_at: at(3),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("claim holder can plan without stranding its claim");
    let rebased_claim = store
        .current_work_claim(root.work_id)
        .unwrap()
        .expect("claim remains active");
    assert_eq!(rebased_claim.claim_id, live_claim.claim_id);
    assert_eq!(rebased_claim.fence, live_claim.fence);
    assert_eq!(
        rebased_claim.accepted_work_revision,
        decomposition.parent.revision
    );
}

#[test]
fn submillisecond_work_and_claim_times_bind_to_millisecond_projections() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let created_at = at(0) + Duration::nanoseconds(999_999_999);
    let mut request = root_request("project-fractional-time", "root", 0);
    request.created_at = created_at;
    let root = store
        .create_work(&request, &DevelopmentNoopRedactor)
        .expect("create work with submillisecond timestamp");
    assert_eq!(store.get_work_item(root.work_id).unwrap(), root);

    let claimed_at = at(1) + Duration::nanoseconds(999_999_999);
    let claim = store
        .claim_work(
            &ClaimWorkRequest {
                work_id: root.work_id,
                expected_work_revision: root.revision,
                expected_run_id: Some(root.active_run_id.expect("active run")),
                holder: SessionId("holder".into()),
                ttl_seconds: 30,
                recovery_reason: None,
                actor: actor("holder"),
                idempotency_key: "fractional-claim".into(),
                claimed_at,
            },
            &DevelopmentNoopRedactor,
        )
        .expect("claim with submillisecond timestamp");
    assert_eq!(store.current_work_claim(root.work_id).unwrap(), Some(claim));
}

#[test]
fn claim_expiry_overflow_is_a_typed_refusal() {
    assert!(matches!(
        claim_expiry(DateTime::<Utc>::MAX_UTC, 1),
        Err(StoreError::InvalidWork(_))
    ));
}

#[test]
fn claims_recover_across_connections_and_handoff_fences_old_sessions() {
    let directory = tempfile::tempdir().expect("tempdir");
    let database = directory.path().join("work.db");
    let mut first = SqliteStore::open(&database).expect("first connection");
    let root = first
        .create_work(
            &root_request("project-b", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let mut second = SqliteStore::open(&database).expect("second connection");
    let initial = claim(&mut first, &root, "agent-a", "claim-a", 1, 10);
    let conflict = second.claim_work(
        &ClaimWorkRequest {
            work_id: root.work_id,
            expected_work_revision: root.revision,
            expected_run_id: Some(root.active_run_id.expect("active run")),
            holder: SessionId("agent-b".into()),
            ttl_seconds: 10,
            recovery_reason: None,
            actor: actor("agent-b"),
            idempotency_key: "claim-b-too-soon".into(),
            claimed_at: at(2),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(conflict, Err(StoreError::WorkClaimHeld { .. })));

    let missing_recovery = second.claim_work(
        &ClaimWorkRequest {
            work_id: root.work_id,
            expected_work_revision: root.revision,
            expected_run_id: Some(root.active_run_id.expect("active run")),
            holder: SessionId("agent-b".into()),
            ttl_seconds: 20,
            recovery_reason: None,
            actor: actor("agent-b"),
            idempotency_key: "claim-b-missing-recovery".into(),
            claimed_at: at(12),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(missing_recovery, Err(StoreError::InvalidWork(_))));
    assert_eq!(
        second.current_work_claim(root.work_id).unwrap(),
        Some(initial.clone())
    );

    let whitespace_recovery = second.claim_work(
        &ClaimWorkRequest {
            work_id: root.work_id,
            expected_work_revision: root.revision,
            expected_run_id: Some(root.active_run_id.expect("active run")),
            holder: SessionId("agent-b".into()),
            ttl_seconds: 20,
            recovery_reason: Some("   ".into()),
            actor: actor("agent-b"),
            idempotency_key: "claim-b-empty-recovery-reason".into(),
            claimed_at: at(12),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(
        whitespace_recovery,
        Err(StoreError::InvalidWork(_))
    ));
    assert_eq!(
        second.current_work_claim(root.work_id).unwrap(),
        Some(initial.clone())
    );

    let recovered = claim(&mut second, &root, "agent-b", "claim-b", 12, 20);
    assert_eq!(recovered.claim_id, initial.claim_id);
    assert!(recovered.fence > initial.fence);
    let stale = first.checkpoint_work(
        &CheckpointWorkRequest {
            work_id: root.work_id,
            run_id: initial.run_id,
            expected_work_revision: root.revision,
            holder: SessionId("agent-a".into()),
            claim_id: initial.claim_id,
            claim_fence: initial.fence,
            summary: "stale".into(),
            evidence: Some(Vec::new()),
            actor: actor("agent-a"),
            idempotency_key: "stale-checkpoint".into(),
            checkpointed_at: at(13),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(stale, Err(StoreError::WorkClaimMismatch { .. })));

    let offer = second
        .offer_work_handoff(
            &OfferWorkHandoffRequest {
                work_id: root.work_id,
                run_id: recovered.run_id,
                expected_work_revision: root.revision,
                from: recovered.holder.clone(),
                to: SessionId("agent-c".into()),
                claim_id: recovered.claim_id,
                claim_fence: recovered.fence,
                ttl_seconds: 30,
                checkpoint_summary: "ready for agent-c".into(),
                actor: actor("agent-b"),
                idempotency_key: "offer".into(),
                offered_at: at(14),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("offer handoff");
    second
        .connection
        .execute_batch("SAVEPOINT corrupt")
        .expect("handoff corruption savepoint");
    let mut tampered_offer = offer.clone();
    tampered_offer.to = SessionId("forged-recipient".into());
    second
        .connection
        .execute(
            "UPDATE work_handoff_offers SET offer_json = ?2 WHERE offer_id = ?1",
            params![
                offer.offer_id.0.to_string(),
                serde_json::to_vec(&tampered_offer).expect("tampered offer JSON")
            ],
        )
        .expect("tamper offer projection");
    assert!(matches!(
        second.work_handoff_offers(root.work_id),
        Err(StoreError::InvalidWorkProjection(_))
    ));
    let corrupted = second.verify_all().expect("inspect handoff corruption");
    assert!(
        corrupted
            .invalid_work_records
            .iter()
            .any(|record| record.contains("work_handoff_offer"))
    );
    restore_savepoint(&second);
    let blocked_while_pending = second.record_work_evidence(
        &RecordWorkEvidenceRequest {
            work_id: root.work_id,
            run_id: recovered.run_id,
            expected_work_revision: root.revision,
            holder: recovered.holder.clone(),
            claim_id: recovered.claim_id,
            claim_fence: recovered.fence,
            summary: "must not land".into(),
            refs: Vec::new(),
            actor: actor("agent-b"),
            idempotency_key: "pending-write".into(),
            recorded_at: at(15),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(
        blocked_while_pending,
        Err(StoreError::InvalidWork(_))
    ));
    let accepted = first
        .accept_work_handoff(
            &AcceptWorkHandoffRequest {
                work_id: root.work_id,
                offer_id: offer.offer_id,
                to: SessionId("agent-c".into()),
                actor: actor("agent-c"),
                idempotency_key: "accept".into(),
                accepted_at: at(16),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("accept handoff");
    assert_eq!(accepted.holder, SessionId("agent-c".into()));
    assert!(accepted.fence > recovered.fence);
    let replay = first
        .accept_work_handoff(
            &AcceptWorkHandoffRequest {
                work_id: root.work_id,
                offer_id: offer.offer_id,
                to: SessionId("agent-c".into()),
                actor: actor("agent-c"),
                idempotency_key: "accept".into(),
                accepted_at: at(16),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("idempotent accept");
    assert_eq!(replay, accepted);
    let accepted_evidence = evidence(
        &mut first,
        &root,
        &accepted,
        "agent-c",
        "accepted-evidence",
        17,
    );
    let predecessor_checkpoint = complete(
        &mut first,
        &root,
        &accepted,
        "agent-c",
        &accepted_evidence,
        "stale-handoff-complete",
        18,
    );
    assert!(matches!(
        predecessor_checkpoint,
        Err(StoreError::WorkCompletionRefused { .. })
    ));
    checkpoint(
        &mut first,
        &root,
        &accepted,
        "agent-c",
        "accepted-checkpoint",
        19,
        std::slice::from_ref(&accepted_evidence),
    );
    let seal = complete(
        &mut first,
        &root,
        &accepted,
        "agent-c",
        &accepted_evidence,
        "accepted-complete",
        20,
    )
    .expect("complete after current-fence checkpoint");
    assert_eq!(seal.waivers.len(), 1);
    assert_eq!(seal.expected_contributors.len(), 3);
    let run = second.get_work_run(accepted.run_id).expect("persisted run");
    assert_eq!(run.executor, Some(SessionId("agent-c".into())));
}

#[test]
fn same_holder_plain_claim_retakes_a_lapsed_claim_and_replays_exactly() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("same-holder-retake", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let initial = claim(&mut store, &root, "holder", "initial-claim", 1, 2);
    checkpoint(
        &mut store,
        &root,
        &initial,
        "holder",
        "activate-before-plain-retake",
        2,
        &[],
    );
    let active = store
        .current_work_claim(root.work_id)
        .expect("claim after checkpoint")
        .expect("active claim");
    let request = ClaimWorkRequest {
        work_id: root.work_id,
        expected_work_revision: root.revision,
        expected_run_id: Some(active.run_id),
        holder: active.holder.clone(),
        ttl_seconds: 60,
        recovery_reason: None,
        actor: actor("holder"),
        idempotency_key: "plain-same-holder-retake".into(),
        claimed_at: active.expires_at,
    };
    let retaken = store
        .claim_work(&request, &DevelopmentNoopRedactor)
        .expect("same holder retakes without recovery ceremony");
    assert_eq!(retaken.claim_id, active.claim_id);
    assert!(retaken.fence > active.fence);
    assert_eq!(
        retaken.expires_at,
        active.expires_at + Duration::seconds(60)
    );
    assert_eq!(
        store
            .get_work_run(retaken.run_id)
            .expect("retaken run")
            .state,
        WorkRunState::Active,
        "ordinary same-holder retake preserves checkpointed run state"
    );
    let events_before_replay =
        canonical_work_events_for_item(&store.connection, root.work_id).expect("claim history");
    assert!(matches!(
        events_before_replay.last().map(|event| &event.transition),
        Some(WorkTransition::Claimed {
            recovered: true,
            ..
        })
    ));

    let replay = store
        .claim_work(&request, &DevelopmentNoopRedactor)
        .expect("exact retake replay");
    assert_eq!(replay, retaken);
    assert_eq!(
        canonical_work_events_for_item(&store.connection, root.work_id)
            .expect("history after replay")
            .len(),
        events_before_replay.len(),
        "exact replay must not append a second retake event"
    );
    assert!(store.verify_all().expect("integrity").is_healthy());
}

#[test]
fn same_holder_plain_retake_refuses_blocked_and_deferred_work() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let blocked = store
        .create_work(
            &root_request("retake-readiness", "blocked-root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("blocked root");
    let blocked_claim = store
        .claim_work(
            &ClaimWorkRequest {
                work_id: blocked.work_id,
                expected_work_revision: blocked.revision,
                expected_run_id: Some(blocked.active_run_id.expect("active run")),
                holder: SessionId("holder".into()),
                ttl_seconds: 1,
                recovery_reason: None,
                actor: actor("holder"),
                idempotency_key: "blocked-claim".into(),
                claimed_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("blocked candidate claim");
    store
        .add_work_blocker(
            &AddWorkBlockerRequest {
                work_id: blocked.work_id,
                expected_work_revision: blocked.revision,
                kind: crate::domain::WorkBlockerKind::Manual,
                detail: "retake must respect this blocker".into(),
                authority: WorkPlanningAuthority::Claim {
                    run_id: blocked_claim.run_id,
                    holder: blocked_claim.holder.clone(),
                    claim_id: blocked_claim.claim_id,
                    claim_fence: blocked_claim.fence,
                },
                actor: actor("holder"),
                idempotency_key: "block-before-retake".into(),
                blocked_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("block claimed work");
    let blocked = store.get_work_item(blocked.work_id).expect("blocked work");
    let blocked_claim = store
        .current_work_claim(blocked.work_id)
        .expect("blocked claim")
        .expect("current blocked claim");
    let refused = store.claim_work(
        &ClaimWorkRequest {
            work_id: blocked.work_id,
            expected_work_revision: blocked.revision,
            expected_run_id: Some(blocked_claim.run_id),
            holder: blocked_claim.holder.clone(),
            ttl_seconds: 60,
            recovery_reason: None,
            actor: actor("holder"),
            idempotency_key: "blocked-plain-retake".into(),
            claimed_at: blocked_claim.expires_at,
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(refused, Err(StoreError::InvalidWork(reason)) if reason.contains("Blocked")));

    let deferred = store
        .create_work(
            &root_request("retake-readiness", "deferred-root", 10),
            &DevelopmentNoopRedactor,
        )
        .expect("deferred root");
    let deferred_claim = store
        .claim_work(
            &ClaimWorkRequest {
                work_id: deferred.work_id,
                expected_work_revision: deferred.revision,
                expected_run_id: Some(deferred.active_run_id.expect("active run")),
                holder: SessionId("holder".into()),
                ttl_seconds: 1,
                recovery_reason: None,
                actor: actor("holder"),
                idempotency_key: "deferred-claim".into(),
                claimed_at: at(11),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("deferred candidate claim");
    store
        .revise_work(
            &ReviseWorkRequest {
                work_id: deferred.work_id,
                expected_revision: deferred.revision,
                patch: WorkRevisionPatch {
                    deferred_until: Some(at(2_000_000)),
                    ..WorkRevisionPatch::default()
                },
                authority: WorkPlanningAuthority::Claim {
                    run_id: deferred_claim.run_id,
                    holder: deferred_claim.holder.clone(),
                    claim_id: deferred_claim.claim_id,
                    claim_fence: deferred_claim.fence,
                },
                actor: actor("holder"),
                idempotency_key: "defer-before-retake".into(),
                updated_at: at(11),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("defer claimed work");
    let deferred = store
        .get_work_item(deferred.work_id)
        .expect("deferred work");
    let deferred_claim = store
        .current_work_claim(deferred.work_id)
        .expect("deferred claim")
        .expect("current deferred claim");
    let refused = store.claim_work(
        &ClaimWorkRequest {
            work_id: deferred.work_id,
            expected_work_revision: deferred.revision,
            expected_run_id: Some(deferred_claim.run_id),
            holder: deferred_claim.holder.clone(),
            ttl_seconds: 60,
            recovery_reason: None,
            actor: actor("holder"),
            idempotency_key: "deferred-plain-retake".into(),
            claimed_at: deferred_claim.expires_at,
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(refused, Err(StoreError::InvalidWork(reason)) if reason.contains("Deferred")));
}

#[test]
fn same_holder_plain_retake_replays_across_connections() {
    let directory = tempfile::tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let mut first = SqliteStore::open(&database).expect("first connection");
    let root = first
        .create_work(
            &root_request("retake-contention", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let initial = claim(&mut first, &root, "holder", "initial", 1, 1);
    let request = ClaimWorkRequest {
        work_id: root.work_id,
        expected_work_revision: root.revision,
        expected_run_id: Some(initial.run_id),
        holder: initial.holder.clone(),
        ttl_seconds: 60,
        recovery_reason: None,
        actor: actor("holder"),
        idempotency_key: "retake-contention-key".into(),
        claimed_at: at(3),
    };
    let mut second = SqliteStore::open(&database).expect("second connection");
    let retaken = first
        .claim_work(&request, &DevelopmentNoopRedactor)
        .expect("first connection retakes");
    assert!(retaken.fence > initial.fence);
    let replay = second
        .claim_work(&request, &DevelopmentNoopRedactor)
        .expect("second connection replays the retake");
    assert_eq!(replay, retaken);
    assert_eq!(
        canonical_work_events_for_item(&first.connection, root.work_id)
            .expect("claim history")
            .into_iter()
            .filter(|event| matches!(
                event.transition,
                WorkTransition::Claimed {
                    recovered: true,
                    ..
                }
            ))
            .count(),
        1
    );
}

#[test]
fn holder_mutations_renew_refusals_do_not_and_plain_claim_retakes() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("project-renewal", "renewal-root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let initial = claim(&mut store, &root, "holder", "renewal-claim", 1, 10);
    assert_eq!(initial.expires_at, at(11));

    evidence(&mut store, &root, &initial, "holder", "renewal-evidence", 2);
    let after_evidence = store
        .current_work_claim(root.work_id)
        .expect("claim after evidence")
        .expect("live claim");
    assert_eq!(
        after_evidence.expires_at,
        at(2) + Duration::seconds(DEFAULT_WORK_CLAIM_TTL_SECONDS)
    );
    assert_eq!(after_evidence.revision, initial.revision + 1);

    checkpoint(
        &mut store,
        &root,
        &after_evidence,
        "holder",
        "renewal-checkpoint",
        3,
        &[],
    );
    let after_checkpoint = store
        .current_work_claim(root.work_id)
        .expect("claim after checkpoint")
        .expect("live claim");
    assert_eq!(
        after_checkpoint.expires_at,
        at(3) + Duration::seconds(DEFAULT_WORK_CLAIM_TTL_SECONDS)
    );
    assert_eq!(after_checkpoint.revision, after_evidence.revision + 1);

    let revised = store
        .revise_work(
            &ReviseWorkRequest {
                work_id: root.work_id,
                expected_revision: root.revision,
                patch: WorkRevisionPatch {
                    priority: Some(2),
                    ..WorkRevisionPatch::default()
                },
                authority: WorkPlanningAuthority::Claim {
                    run_id: after_checkpoint.run_id,
                    holder: after_checkpoint.holder.clone(),
                    claim_id: after_checkpoint.claim_id,
                    claim_fence: after_checkpoint.fence,
                },
                actor: actor("holder"),
                idempotency_key: "renewal-revise".into(),
                updated_at: at(4),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("claim-bound revision");
    let after_revision = store
        .current_work_claim(root.work_id)
        .expect("claim after revision")
        .expect("live claim");
    assert_eq!(
        after_revision.expires_at,
        at(4) + Duration::seconds(DEFAULT_WORK_CLAIM_TTL_SECONDS)
    );
    assert_eq!(after_revision.revision, after_checkpoint.revision + 1);
    assert_eq!(after_revision.accepted_work_revision, revised.revision);

    let offer = store
        .offer_work_handoff(
            &OfferWorkHandoffRequest {
                work_id: revised.work_id,
                run_id: after_revision.run_id,
                expected_work_revision: revised.revision,
                from: after_revision.holder.clone(),
                to: SessionId("next-holder".into()),
                claim_id: after_revision.claim_id,
                claim_fence: after_revision.fence,
                ttl_seconds: 30,
                checkpoint_summary: "handoff renewal checkpoint".into(),
                actor: actor("holder"),
                idempotency_key: "renewal-offer".into(),
                offered_at: at(5),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("handoff offer");
    let after_offer = store
        .current_work_claim(root.work_id)
        .expect("claim after offer")
        .expect("live claim");
    assert_eq!(
        after_offer.expires_at,
        at(5) + Duration::seconds(DEFAULT_WORK_CLAIM_TTL_SECONDS)
    );
    assert_eq!(after_offer.revision, after_revision.revision + 1);
    assert_eq!(offer.expires_at, at(35));

    store
        .cancel_work_handoff(
            &CancelWorkHandoffRequest {
                work_id: revised.work_id,
                run_id: after_offer.run_id,
                expected_work_revision: revised.revision,
                holder: after_offer.holder.clone(),
                offer_id: offer.offer_id,
                claim_id: after_offer.claim_id,
                claim_fence: after_offer.fence,
                reason: "continue with the current holder".into(),
                actor: actor("holder"),
                idempotency_key: "renewal-cancel".into(),
                cancelled_at: at(6),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("cancel handoff");
    let after_cancel = store
        .current_work_claim(root.work_id)
        .expect("claim after cancellation")
        .expect("live claim");
    assert_eq!(
        after_cancel.expires_at,
        at(6) + Duration::seconds(DEFAULT_WORK_CLAIM_TTL_SECONDS)
    );
    assert_eq!(after_cancel.revision, after_offer.revision + 1);

    let refused_at = at(7);
    assert!(matches!(
        store.checkpoint_work(
            &CheckpointWorkRequest {
                work_id: revised.work_id,
                run_id: after_cancel.run_id,
                expected_work_revision: revised.revision - 1,
                holder: after_cancel.holder.clone(),
                claim_id: after_cancel.claim_id,
                claim_fence: after_cancel.fence,
                summary: "stale revision must not renew".into(),
                evidence: Some(Vec::new()),
                actor: actor("holder"),
                idempotency_key: "refused-renewal".into(),
                checkpointed_at: refused_at,
            },
            &DevelopmentNoopRedactor,
        ),
        Err(StoreError::WorkRevisionConflict { .. })
    ));
    assert_eq!(
        store
            .current_work_claim(root.work_id)
            .expect("claim after refused checkpoint")
            .expect("live claim"),
        after_cancel,
        "a refused holder mutation must not renew the claim"
    );

    let retake_root = store
        .create_work(
            &root_request("project-renewal", "retake-renewal-root", 10),
            &DevelopmentNoopRedactor,
        )
        .expect("second root");
    let lapsed = claim(
        &mut store,
        &retake_root,
        "holder",
        "lapsed-renewal-claim",
        11,
        2,
    );
    checkpoint(
        &mut store,
        &retake_root,
        &lapsed,
        "holder",
        "activate-before-lapse",
        12,
        &[],
    );
    let active = store
        .current_work_claim(retake_root.work_id)
        .expect("claim after activating checkpoint")
        .expect("active claim");
    let retaken_at = active.expires_at;
    let retaken_claim = store
        .claim_work(
            &ClaimWorkRequest {
                work_id: retake_root.work_id,
                expected_work_revision: retake_root.revision,
                expected_run_id: Some(active.run_id),
                holder: active.holder.clone(),
                ttl_seconds: DEFAULT_WORK_CLAIM_TTL_SECONDS,
                recovery_reason: None,
                actor: ActorContext {
                    source_tool: Some("work_update".into()),
                    reason: "claim ambient local work".into(),
                    ..actor("holder")
                },
                idempotency_key: "plain-retake-before-checkpoint".into(),
                claimed_at: retaken_at,
            },
            &DevelopmentNoopRedactor,
        )
        .expect("holder plain retake");
    assert!(retaken_claim.fence > active.fence);
    assert_eq!(
        store
            .get_work_run(retaken_claim.run_id)
            .expect("run after retake")
            .state,
        WorkRunState::Active,
        "plain retake preserves checkpointed run state"
    );
    let retake_event = canonical_work_events_for_item(&store.connection, retake_root.work_id)
        .expect("retake history")
        .pop()
        .expect("retake event");
    assert_eq!(
        retake_event.actor.source_tool.as_deref(),
        Some("work_update")
    );
    assert_eq!(retake_event.actor.reason, "claim ambient local work");
    store
        .checkpoint_work(
            &CheckpointWorkRequest {
                work_id: retake_root.work_id,
                run_id: retaken_claim.run_id,
                expected_work_revision: retake_root.revision,
                holder: retaken_claim.holder.clone(),
                claim_id: retaken_claim.claim_id,
                claim_fence: retaken_claim.fence,
                summary: "resume after plain retake".into(),
                evidence: Some(Vec::new()),
                actor: actor("holder"),
                idempotency_key: "retaken-renewal".into(),
                checkpointed_at: retaken_at,
            },
            &DevelopmentNoopRedactor,
        )
        .expect("checkpoint revalidates the preflight fence");
    let after_retake_checkpoint = store
        .current_work_claim(retake_root.work_id)
        .expect("claim after retake")
        .expect("active retaken claim");
    assert_eq!(after_retake_checkpoint.fence, retaken_claim.fence);
    assert_eq!(
        after_retake_checkpoint.expires_at,
        retaken_at + Duration::seconds(DEFAULT_WORK_CLAIM_TTL_SECONDS)
    );
    assert!(store.verify_all().expect("integrity").is_healthy());
}

#[test]
fn shared_work_capture_requires_the_exact_live_holder_and_renews_once() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let project = ProjectId("project-work-note-claim".into());
    let root = store
        .create_work(
            &root_request(&project.0, "work-note-root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let initial = claim(&mut store, &root, "holder", "work-note-claim", 1, 10);
    let holder = SessionId("holder".into());
    let peer = SessionId("peer".into());
    store
        .focus_work_session(&project, &holder, root.work_id, at(1))
        .expect("holder focus");
    store
        .focus_work_session(&project, &peer, root.work_id, at(1))
        .expect("peer focus");

    let request = NoteRequest {
        project_id: project.clone(),
        task_id: None,
        work_id: Some(root.work_id),
        prose: "Evidence: the exact claim holder captured shared work memory".into(),
        visibility: NoteVisibility::Shared,
        kind: None,
        authority: None,
        sensitivity: None,
        title: None,
        tags: Vec::new(),
        evidence: Vec::new(),
        refs: vec!["test:work-note-holder".into()],
        actor: actor("holder"),
        idempotency_key: "work-note-holder".into(),
        created_at: at(2),
    };
    let receipt = store
        .capture_note(&request, &DevelopmentNoopRedactor)
        .expect("live holder capture");
    assert_eq!(receipt.work_positions.len(), 9);
    let renewed = store
        .current_work_claim(root.work_id)
        .expect("claim after capture")
        .expect("live claim");
    assert_eq!(renewed.revision, initial.revision + 1);
    assert_eq!(
        renewed.expires_at,
        at(2) + Duration::seconds(DEFAULT_WORK_CLAIM_TTL_SECONDS)
    );
    let latest = canonical_work_events_for_item(&store.connection, root.work_id)
        .expect("canonical work history")
        .pop()
        .expect("memory capture event");
    assert_eq!(latest.claim.as_ref(), Some(&renewed));
    assert!(matches!(
        latest.transition,
        WorkTransition::MemoryCaptured { version, assertion }
            if version == receipt.version && assertion == receipt.assertion
    ));

    let replay = store
        .capture_note(&request, &DevelopmentNoopRedactor)
        .expect("exact replay");
    assert!(replay.duplicate);
    assert_eq!(
        store
            .current_work_claim(root.work_id)
            .expect("claim after replay")
            .expect("live claim"),
        renewed
    );

    let mut foreign = request.clone();
    foreign.actor = actor("peer");
    foreign.idempotency_key = "work-note-peer".into();
    foreign.created_at = at(3);
    assert!(matches!(
        store.capture_note(&foreign, &DevelopmentNoopRedactor),
        Err(StoreError::WorkClaimMismatch { work }) if work == root.work_id
    ));
    assert_eq!(
        store
            .current_work_claim(root.work_id)
            .expect("claim after foreign refusal")
            .expect("live claim"),
        renewed
    );

    let mut lapsed = request;
    lapsed.idempotency_key = "work-note-lapsed".into();
    lapsed.created_at = renewed.expires_at;
    assert!(matches!(
        store.capture_note(&lapsed, &DevelopmentNoopRedactor),
        Err(StoreError::WorkClaimLapsed { work, .. }) if work == root.work_id
    ));
    let retaken_claim = store
        .claim_work(
            &ClaimWorkRequest {
                work_id: root.work_id,
                expected_work_revision: root.revision,
                expected_run_id: Some(renewed.run_id),
                holder: renewed.holder.clone(),
                ttl_seconds: DEFAULT_WORK_CLAIM_TTL_SECONDS,
                recovery_reason: None,
                actor: ActorContext {
                    source_tool: Some("work_update".into()),
                    reason: "claim ambient local work".into(),
                    ..actor("holder")
                },
                idempotency_key: "plain-retake-before-note".into(),
                claimed_at: lapsed.created_at,
            },
            &DevelopmentNoopRedactor,
        )
        .expect("own lapsed claim retake");
    let lapsed_receipt = store
        .capture_note(&lapsed, &DevelopmentNoopRedactor)
        .expect("note capture uses the retaken claim");
    let retaken = store
        .current_work_claim(root.work_id)
        .expect("claim after note capture")
        .expect("retaken claim");
    assert_eq!(retaken.fence, retaken_claim.fence);
    assert!(retaken.fence > renewed.fence);
    assert_eq!(
        retaken.expires_at,
        lapsed.created_at + Duration::seconds(DEFAULT_WORK_CLAIM_TTL_SECONDS)
    );
    let events_before_replay =
        canonical_work_events_for_item(&store.connection, root.work_id).expect("work history");
    assert!(matches!(
        events_before_replay
            .iter()
            .rev()
            .nth(1)
            .map(|event| &event.transition),
        Some(WorkTransition::Claimed {
            recovered: true,
            ..
        })
    ));
    let replay = store
        .capture_note(&lapsed, &DevelopmentNoopRedactor)
        .expect("exact post-retake note replay");
    assert!(replay.duplicate);
    assert_eq!(replay.version, lapsed_receipt.version);
    assert_eq!(
        canonical_work_events_for_item(&store.connection, root.work_id)
            .expect("history after replay")
            .len(),
        events_before_replay.len()
    );
    assert_eq!(
        store
            .current_work_claim(root.work_id)
            .expect("claim after replay")
            .expect("retaken claim"),
        retaken
    );
    assert!(store.verify_all().expect("integrity").is_healthy());
}

#[test]
fn release_requires_nonempty_waiver_reason_and_persists_audit_reasons() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("project-release-reason", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let claim = claim(&mut store, &root, "holder", "claim", 1, 100);
    let request = ReleaseWorkRequest {
        work_id: root.work_id,
        run_id: claim.run_id,
        expected_work_revision: root.revision,
        holder: claim.holder.clone(),
        claim_id: claim.claim_id,
        claim_fence: claim.fence,
        reason: "  planned pause  ".into(),
        waiver_reason: Some("   ".into()),
        actor: actor("holder"),
        idempotency_key: "release-empty-waiver-reason".into(),
        released_at: at(2),
    };
    assert!(matches!(
        store.release_work(&request, &DevelopmentNoopRedactor),
        Err(StoreError::InvalidWork(_))
    ));
    assert_eq!(
        store.current_work_claim(root.work_id).unwrap(),
        Some(claim.clone())
    );

    let released = store
        .release_work(
            &ReleaseWorkRequest {
                waiver_reason: Some("  holder left before contributing  ".into()),
                idempotency_key: "release-with-audit-reasons".into(),
                ..request
            },
            &DevelopmentNoopRedactor,
        )
        .expect("release with attributed waiver");
    assert_eq!(released.state, WorkClaimState::Released);
    let entry = store
        .work_event_tail(root.work_id, 1)
        .expect("release event")
        .pop()
        .expect("release tail");
    let event: WorkEvent =
        load_typed_work_object(&store.connection, &entry.object_hash, "work_event")
            .expect("canonical release event");
    assert!(matches!(
        event.transition,
        WorkTransition::Released { reason, .. } if reason == "planned pause"
    ));
    assert_eq!(
        event
            .root_execution
            .expect("release root execution")
            .waivers[0]
            .reason,
        "holder left before contributing"
    );
    let next_holder = SessionId("next-holder".into());
    assert!(
        !store
            .work_claim_recovery_required(root.work_id, &next_holder)
            .expect("waived holder is already accounted")
    );
    let successor = store
        .claim_work(
            &ClaimWorkRequest {
                work_id: root.work_id,
                expected_work_revision: root.revision,
                expected_run_id: Some(root.active_run_id.expect("active run")),
                holder: next_holder,
                ttl_seconds: 60,
                recovery_reason: None,
                actor: actor("next-holder"),
                idempotency_key: "ordinary-claim-after-waiver".into(),
                claimed_at: at(3),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("an accounted prior holder needs no second recovery waiver");
    assert_eq!(successor.holder, SessionId("next-holder".into()));
    assert!(store.verify_all().expect("integrity").is_healthy());
}
