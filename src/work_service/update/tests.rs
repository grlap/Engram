use super::super::test_support::*;
use super::super::*;
use crate::domain::GATE_EVIDENCE_SUMMARY;
use tempfile::tempdir;

mod observations;
mod renewal;

#[test]
fn work_update_does_not_admit_obligation_waivers() {
    let attempted = serde_json::json!({
        "kind": "waive_obligation",
        "obligation_id": uuid::Uuid::now_v7(),
        "reason": "agent attempted to waive a host obligation",
        "idempotency_key": "agent-waiver"
    });

    assert!(serde_json::from_value::<WorkUpdateInput>(attempted).is_err());
}

#[test]
fn core_committed_update_recovery_uses_the_durable_focus_basis() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("committed-update-focus".into());
    let session = SessionId("committed-update-session".into());
    let service = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        session.clone(),
        Some("protocol-test".into()),
    );
    let first = match service
        .work_propose(root_input("Original focus", "committed-first"), at(0))
        .expect("first root")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    let input = WorkUpdateInput::Revise {
        patch: WorkRevisionPatch {
            title: Some("Durably revised original focus".into()),
            ..WorkRevisionPatch::default()
        },
        idempotency_key: "committed-revise".into(),
    };

    let mut store = SqliteStore::open(&database).expect("store");
    let basis = service
        .protocol_basis(&store, true, false, None, at(1))
        .expect("original basis");
    let intent = service.protocol_intent(&input);
    store
        .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
            project_id: &project,
            session_id: &session,
            operation: "work_update:revise",
            idempotency_key: "committed-revise",
            intent: &intent,
            basis: &basis,
            now: at(1),
        })
        .expect("begin durable attempt");
    let original = basis.focused_work.clone().expect("original focus");
    let authority = service.planning_authority(basis.claim.as_ref(), &original, at(1));
    store
        .revise_work(
            &ReviseWorkRequest {
                work_id: original.work_id,
                expected_revision: original.revision,
                patch: WorkRevisionPatch {
                    title: Some("Durably revised original focus".into()),
                    ..WorkRevisionPatch::default()
                },
                authority,
                actor: service.actor("test", "commit only the scoped update"),
                idempotency_key: service
                    .core_operation_key("work_update:revise", "committed-revise", "revise_work")
                    .expect("scoped operation key"),
                updated_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("commit core revise without protocol result");
    drop(store);

    let second = match service
        .work_propose(root_input("New live focus", "committed-second"), at(2))
        .expect("second root")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    let recovered = service
        .work_update(input.clone(), at(3))
        .expect("recover core-committed update");
    assert_eq!(recovered.receipt.work_id, first.work_id);
    assert_eq!(recovered.receipt.revision, first.revision + 1);
    assert_ne!(recovered.receipt.work_id, second.work_id);
    let replayed = service
        .work_update(input, at(4))
        .expect("replay exact protocol result");
    assert_eq!(
        serde_json::to_vec(&replayed).expect("serialize replay"),
        serde_json::to_vec(&recovered).expect("serialize recovery")
    );
    assert_eq!(
        SqliteStore::open(&database)
            .expect("store")
            .work_session_state(&project, &session, at(4))
            .expect("session")
            .focused_work_id,
        Some(second.work_id)
    );
}

#[test]
fn omitted_idempotency_key_replays_identical_calls_and_separates_different_ones() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("derived-keys".into());
    let service = LocalWorkService::new(
        database,
        project,
        "agent".into(),
        SessionId("derived-keys-session".into()),
        Some("protocol-test".into()),
    );
    let root_of = |result: WorkProposeResult| match result {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    let first = root_of(
        service
            .work_propose(root_input("Keyless root", ""), at(0))
            .expect("keyless root"),
    );
    let replayed = root_of(
        service
            .work_propose(root_input("Keyless root", ""), at(1))
            .expect("identical keyless call replays"),
    );
    assert_eq!(replayed.work_id, first.work_id);
    let other = root_of(
        service
            .work_propose(root_input("Different keyless root", ""), at(2))
            .expect("different keyless call creates"),
    );
    assert_ne!(other.work_id, first.work_id);

    service
        .select_work(&first.short_ref, at(3))
        .expect("select the first root");
    let claim = WorkUpdateInput::Claim {
        ttl_seconds: Some(300),
        recovery_reason: None,
        idempotency_key: String::new(),
    };
    let claimed = service.work_update(claim.clone(), at(3)).expect("claim");
    assert_eq!(claimed.receipt.work_id, first.work_id);
    // Keyless claims renew while keeping the existing claim identity/fence.
    let claimed_again = service
        .work_update(claim, at(4))
        .expect("re-claiming held work renews the live claim");
    assert_eq!(
        claimed_again.receipt.result["claim_id"],
        claimed.receipt.result["claim_id"]
    );
    assert_eq!(
        claimed_again.receipt.result["fence"],
        claimed.receipt.result["fence"]
    );
    assert_eq!(
        claimed_again.receipt.result["expires_at"],
        serde_json::json!(at(304))
    );
    let checkpoint = |summary: &str| WorkUpdateInput::Checkpoint {
        summary: summary.into(),
        evidence: None,
        idempotency_key: String::new(),
    };
    let noted = service
        .work_update(checkpoint("found the cause"), at(5))
        .expect("first checkpoint");
    // A checkpoint does not move the focused work/claim basis, so the
    // identical keyless note replays instead of duplicating.
    let noted_again = service
        .work_update(checkpoint("found the cause"), at(6))
        .expect("identical checkpoint replays");
    assert_eq!(
        serde_json::to_value(&noted_again.receipt).expect("receipt"),
        serde_json::to_value(&noted.receipt).expect("receipt")
    );
    // A refused attempt leaves the basis unchanged, so its exact retry
    // replays the refusal rather than inventing a new attempt.
    let stale_release = WorkUpdateInput::Release {
        reason: String::new(),
        waiver_reason: None,
        idempotency_key: String::new(),
    };
    let first_refusal = service
        .work_update(stale_release.clone(), at(7))
        .expect_err("an empty release reason is refused");
    let second_refusal = service
        .work_update(stale_release, at(8))
        .expect_err("the identical retry is refused the same way");
    assert_eq!(first_refusal.to_string(), second_refusal.to_string());
}

#[test]
fn completed_explicit_update_replays_after_expiry_without_retaking() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("completed-update-after-expiry".into());
    let service = LocalWorkService::new(
        database,
        project,
        "agent".into(),
        SessionId("completed-update-session".into()),
        Some("protocol-test".into()),
    );
    let work = match service
        .work_propose(
            root_input("Completed update replay", "completed-update-root"),
            at(0),
        )
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
                idempotency_key: "completed-update-claim".into(),
            },
            at(1),
        )
        .expect("claim");
    let input = WorkUpdateInput::Checkpoint {
        summary: "checkpoint once".into(),
        evidence: Some(Vec::new()),
        idempotency_key: "completed-update-key".into(),
    };
    let completed = service
        .work_update(input.clone(), at(2))
        .expect("checkpoint");
    let store = service.store().expect("store after checkpoint");
    let claim = store
        .current_work_claim(work.work_id)
        .expect("claim projection")
        .expect("live claim");
    let event_count = store
        .work_event_tail(work.work_id, 64)
        .expect("events")
        .len();
    drop(store);

    let replay = service
        .work_update(input, at(4_000))
        .expect("completed explicit key replays after claim expiry");
    assert_eq!(
        serde_json::to_value(replay).expect("replay JSON"),
        serde_json::to_value(completed).expect("completed JSON")
    );
    let store = service.store().expect("store after replay");
    assert_eq!(
        store
            .current_work_claim(work.work_id)
            .expect("claim projection")
            .expect("retained claim"),
        claim,
        "a completed replay must not advance or renew claim authority"
    );
    assert_eq!(
        store
            .work_event_tail(work.work_id, 64)
            .expect("events after replay")
            .len(),
        event_count,
        "a completed replay must not append a retake event"
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "concurrency, exact replay, and a large unrelated evidence history form one gate-transition regression"
)]
fn concurrent_gate_transitions_serialize_and_history_lookup_stays_bounded() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("concurrent-gate".into());
    let session = SessionId("gate-session".into());
    let service = LocalWorkService::new(
        database.clone(),
        project,
        "agent".into(),
        session,
        Some("gate-test".into()),
    );
    let work = match service
        .work_propose(root_input("Concurrent gate", "concurrent-gate-root"), at(0))
        .expect("root")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "concurrent-gate-claim".into(),
            },
            at(1),
        )
        .expect("claim");

    let barrier = Arc::new(Barrier::new(3));
    let first_service = service.clone();
    let first_barrier = barrier.clone();
    let first = std::thread::spawn(move || {
        first_barrier.wait();
        first_service.work_gate("cargo-test", &["first".into()], None, at(2))
    });
    let second_service = service.clone();
    let second_barrier = barrier.clone();
    let second = std::thread::spawn(move || {
        second_barrier.wait();
        second_service.work_gate("cargo-test", &["second".into()], None, at(2))
    });
    barrier.wait();
    let first = first.join().expect("first thread").expect("first gate");
    let second = second.join().expect("second thread").expect("second gate");
    let first_hash: ObjectHash =
        serde_json::from_value(first.receipt.result).expect("first evidence hash");
    let second_hash: ObjectHash =
        serde_json::from_value(second.receipt.result).expect("second evidence hash");
    let store = SqliteStore::open(&database).expect("store");
    let first_evidence = store
        .get::<WorkEvidence>(&first_hash)
        .expect("first evidence read")
        .expect("first evidence");
    let second_evidence = store
        .get::<WorkEvidence>(&second_hash)
        .expect("second evidence read")
        .expect("second evidence");
    let (latest_hash, latest_failed) = if first_evidence
        .gate
        .as_ref()
        .and_then(|gate| gate.previous.as_ref())
        == Some(&second_hash)
    {
        (first_hash, vec!["first".into()])
    } else {
        assert_eq!(
            second_evidence
                .gate
                .as_ref()
                .and_then(|gate| gate.previous.as_ref()),
            Some(&first_hash)
        );
        (second_hash, vec!["second".into()])
    };
    drop(store);
    let replay = service
        .work_gate("cargo-test", &latest_failed, None, at(3))
        .expect("replay latest transition");
    assert_eq!(
        serde_json::from_value::<ObjectHash>(replay.receipt.result)
            .expect("replayed evidence hash"),
        latest_hash
    );
    assert_eq!(
        SqliteStore::open(&database)
            .expect("store")
            .work_run_evidence(work.active_run_id.expect("active run"))
            .expect("run evidence")
            .len(),
        2
    );

    for index in 0..64 {
        service
            .work_update(
                WorkUpdateInput::Evidence {
                    summary: format!("unrelated evidence {index}"),
                    refs: Vec::new(),
                    attach: None,
                    idempotency_key: format!("unrelated-evidence-{index}"),
                },
                at(4 + index),
            )
            .expect("unrelated evidence");
    }
    crate::canonical::reset_canonical_decode_count();
    service
        .work_gate("bounded-history", &[], None, at(100))
        .expect("bounded gate lookup");
    assert!(
        crate::canonical::canonical_decode_count() <= 24,
        "gate lookup decoded an unbounded evidence history"
    );
    let lapsed_replay = service
        .work_gate("bounded-history", &[], None, at(4_000))
        .expect("a committed exact gate retry replays after claim lapse");
    assert_eq!(lapsed_replay.operation, "evidence");
}

#[test]
fn explicit_update_target_wins_after_same_session_focus_change() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("explicit-update-target".into());
    let service = LocalWorkService::new(
        database,
        project,
        "agent".into(),
        SessionId("shared-session".into()),
        Some("explicit-target-test".into()),
    );
    let create = |title: &str, key: &str| match service
        .work_propose(root_input(title, key), at(0))
        .expect("root")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    let target = create("Explicit target", "explicit-target");
    let other = create("Concurrent focus", "concurrent-focus");
    let prerequisite = create("Prerequisite", "explicit-prerequisite");

    service
        .work_focus(&target.short_ref, at(1))
        .expect("initial target focus");
    service
        .work_focus(&other.short_ref, at(2))
        .expect("same session changes focus before mutation");
    service
        .work_update_on(
            Some(&target.work_id.0.to_string()),
            WorkUpdateInput::AddPrerequisite {
                prerequisite: prerequisite.short_ref.clone(),
                idempotency_key: String::new(),
            },
            at(3),
        )
        .expect("explicit update remains bound to its target");

    let store = SqliteStore::open(&service.database).expect("store");
    assert_eq!(
        store
            .work_prerequisites(target.work_id)
            .expect("target prerequisites")
            .into_iter()
            .map(|item| item.work_id)
            .collect::<Vec<_>>(),
        vec![prerequisite.work_id]
    );
    assert!(
        store
            .work_prerequisites(other.work_id)
            .expect("other prerequisites")
            .is_empty()
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the regression keeps pending-attempt setup, atomic capture, and recovery assertions together"
)]
fn pending_note_attempt_recovers_the_atomic_evidence_checkpoint_pair() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("pending-note-attempt".into());
    let service = LocalWorkService::new(
        database.clone(),
        project,
        "agent".into(),
        SessionId("pending-note-session".into()),
        Some("pending-note-test".into()),
    );
    let work = match service
        .work_propose(root_input("Pending note", "pending-note-root"), at(0))
        .expect("root")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "pending-note-claim".into(),
            },
            at(1),
        )
        .expect("claim");
    let summary = "atomic note survives a lost response";
    let refs = vec!["test:pending-note".into()];
    let committed = {
        let mut store = service.store().expect("store");
        let basis = service
            .protocol_basis(&store, true, false, Some(work.work_id), at(2))
            .expect("note basis");
        let note = WorkNoteIntent {
            summary,
            refs: &refs,
        };
        let intent = service.protocol_intent(&note);
        let raw_key = service
            .effective_idempotency_key("", "work_update:note", &basis, &intent, at(2))
            .expect("derived note key");
        store
            .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
                project_id: &service.project_id,
                session_id: &service.session_id,
                operation: "work_update:note",
                idempotency_key: &raw_key,
                intent: &intent,
                basis: &basis,
                now: at(2),
            })
            .expect("pending note attempt");
        let claim = basis.claim.expect("claim basis");
        let scoped_key = service
            .core_operation_key("work_update:note", &raw_key, "record_work_note")
            .expect("note core key");
        store
            .record_work_note(
                &RecordWorkNoteRequest {
                    work_id: work.work_id,
                    run_id: claim.run_id,
                    expected_work_revision: work.revision,
                    holder: service.session_id.clone(),
                    claim_id: claim.claim_id,
                    claim_fence: claim.fence,
                    summary: summary.into(),
                    refs: refs.clone(),
                    actor: service.actor(
                        "work_update",
                        "simulate a lost note response after atomic capture",
                    ),
                    idempotency_key: scoped_key,
                    recorded_at: at(2),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("atomic note capture")
    };

    let recovered = service
        .work_note_on(Some(&work.work_id.0.to_string()), summary, &refs, at(3))
        .expect("recover pending note");
    assert_eq!(
        recovered.evidence.result,
        serde_json::to_value(&committed.evidence).expect("evidence value")
    );
    assert_eq!(
        recovered.receipt.result,
        serde_json::to_value(
            committed
                .checkpoint
                .as_ref()
                .expect("open note checkpoint value"),
        )
        .expect("checkpoint value")
    );
    let store = SqliteStore::open(&database).expect("store");
    assert_eq!(
        store
            .work_run_evidence(work.active_run_id.expect("active run"))
            .expect("run evidence"),
        vec![committed.evidence]
    );
    assert_eq!(
        store
            .latest_work_run(work.work_id)
            .expect("run read")
            .expect("run")
            .last_checkpoint,
        committed.checkpoint
    );
}

#[test]
fn pending_gate_attempt_recovers_without_appending_again() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("pending-gate-attempt".into());
    let service = LocalWorkService::new(
        database.clone(),
        project,
        "agent".into(),
        SessionId("pending-gate-session".into()),
        Some("pending-gate-test".into()),
    );
    let work = match service
        .work_propose(root_input("Pending gate", "pending-gate-root"), at(0))
        .expect("root")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "pending-gate-claim".into(),
            },
            at(1),
        )
        .expect("claim");

    let pending = {
        let mut store = service.store().expect("store");
        let basis = service
            .protocol_basis(&store, true, false, Some(work.work_id), at(2))
            .expect("gate basis");
        let claim = basis.claim.clone().expect("claim basis");
        store
            .record_gate_evidence_protocol(
                &RecordGateEvidenceRequest {
                    work_id: work.work_id,
                    run_id: claim.run_id,
                    expected_work_revision: work.revision,
                    holder: service.session_id.clone(),
                    claim_id: claim.claim_id,
                    claim_fence: claim.fence,
                    name: "cargo-test".into(),
                    failed: vec!["one failure".into()],
                    evidence_ref: None,
                    actor: service.actor("work_update", "record gate evidence for ambient work"),
                    recorded_at: at(2),
                },
                &BeginGateWorkProtocolAttempt {
                    project_id: &service.project_id,
                    session_id: &service.session_id,
                    basis: &basis,
                    now: at(2),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("atomic gate append")
    };
    assert!(pending.result.is_none());

    let recovered = service
        .work_gate_on(
            Some(&work.work_id.0.to_string()),
            "cargo-test",
            &["one failure".into()],
            None,
            at(3),
        )
        .expect("recover pending gate attempt");
    assert_eq!(
        serde_json::from_value::<ObjectHash>(recovered.receipt.result)
            .expect("recovered evidence hash"),
        pending.evidence
    );
    assert_eq!(
        SqliteStore::open(&database)
            .expect("store")
            .work_run_evidence(work.active_run_id.expect("active run"))
            .expect("run evidence")
            .len(),
        1
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one lifecycle regression proves the completed seal, late evidence, peer feed, and holder-word boundary together"
)]
fn project_bound_peers_append_late_notes_and_gates_after_the_frozen_completion_cut() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("post-completion-evidence".into());
    let owner = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "owner".into(),
        SessionId("completion-owner".into()),
        Some("protocol-test".into()),
    );
    let peer = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "peer".into(),
        SessionId("late-finding-peer".into()),
        Some("protocol-test".into()),
    );
    let observer = LocalWorkService::new(
        database.clone(),
        project,
        "observer".into(),
        SessionId("late-finding-observer".into()),
        Some("protocol-test".into()),
    );
    let work = proposed_root(
        owner
            .work_propose(
                root_input("Completed item with a late finding", "late-finding-root"),
                at(0),
            )
            .expect("root"),
    );
    owner
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "late-finding-claim".into(),
            },
            at(1),
        )
        .expect("claim");
    owner
        .work_gate(
            "cargo-test",
            &["late::regression".into()],
            Some("review:late-gate"),
            at(2),
        )
        .expect("pre-completion gate establishes the same-name chain head");
    let completed = owner
        .work_complete(
            completion_input("completed before the late finding", "late-finding-complete"),
            at(3),
        )
        .expect("complete");
    let WorkCompleteResult::Completed(completed) = completed else {
        panic!("completion must seal");
    };
    let mut store = SqliteStore::open(&database).expect("store after completion");
    let seal_before = store
        .get::<CompletionSeal>(&completed.seal)
        .expect("completion seal read")
        .expect("completion seal");
    let run_before = store
        .latest_work_run(work.work_id)
        .expect("latest run read")
        .expect("completed run");
    let evidence_before = store
        .work_run_evidence(completed.run_id)
        .expect("sealed evidence membership");
    let completed_work = store
        .get_work_item(work.work_id)
        .expect("completed work item");
    assert!(matches!(
        store.record_work_note(
            &RecordWorkNoteRequest {
                work_id: work.work_id,
                run_id: completed.run_id,
                expected_work_revision: completed_work.revision,
                holder: owner.session_id.clone(),
                claim_id: seal_before.claim_id,
                claim_fence: seal_before.claim_fence,
                summary: "unmarked completed evidence must be refused".into(),
                refs: Vec::new(),
                actor: owner.actor("test", "attempt unmarked post-completion evidence"),
                idempotency_key: "unmarked-post-completion-note".into(),
                recorded_at: at(3),
            },
            &DevelopmentNoopRedactor,
        ),
        Err(StoreError::InvalidWorkProjection(message))
            if message.contains("exactly one late-finding provenance marker")
    ));
    drop(store);

    observer
        .work_next(
            100,
            WorkNextQuery {
                sections: vec![WorkNextSection::Changes],
                ..WorkNextQuery::default()
            },
            at(3),
        )
        .expect("observer establishes its pre-finding project cursor");
    peer.work_focus(&work.short_ref, at(3))
        .expect("project-bound peer focuses completed work");
    let late_note = peer
        .work_note_on(
            Some(&work.short_ref),
            "late review found a documentation mismatch",
            &["review:late-note".into()],
            at(4),
        )
        .expect("peer appends a late note without a claim or reopen");
    let note_hash: ObjectHash =
        serde_json::from_value(late_note.evidence.result.clone()).expect("late note hash");
    assert_eq!(
        serde_json::from_value::<ObjectHash>(late_note.receipt.result.clone())
            .expect("late note primary hash"),
        note_hash,
        "a late note has no post-completion checkpoint"
    );
    let replayed_note = peer
        .work_note_on(
            Some(&work.short_ref),
            "late review found a documentation mismatch",
            &["review:late-note".into()],
            at(5),
        )
        .expect("an identical late note replays");
    assert_eq!(replayed_note.evidence.result, late_note.evidence.result);

    let late_gate = peer
        .work_gate_on(
            Some(&work.short_ref),
            "cargo-test",
            &["late::regression".into()],
            Some("review:late-gate"),
            at(6),
        )
        .expect("peer appends failed late gate without a claim or reopen");
    let gate_hash: ObjectHash =
        serde_json::from_value(late_gate.receipt.result).expect("late gate hash");

    let store = SqliteStore::open(&database).expect("store after late findings");
    let seal_after = store
        .get::<CompletionSeal>(&completed.seal)
        .expect("completion seal reread")
        .expect("completion seal remains");
    assert_eq!(seal_after, seal_before);
    assert_eq!(
        store
            .latest_work_run(work.work_id)
            .expect("latest run reread")
            .expect("completed run after late findings"),
        run_before
    );
    assert_eq!(seal_after.evidence, evidence_before);
    assert!(!seal_after.evidence.contains(&note_hash));
    assert!(!seal_after.evidence.contains(&gate_hash));
    let run_entries = store
        .work_feed_after(&FeedId::RunExecution(completed.run_id), 0, 100)
        .expect("run feed");
    for late_hash in [&note_hash, &gate_hash] {
        let position = run_entries
            .iter()
            .find(|entry| &entry.object_hash == late_hash)
            .expect("late evidence is in the completed run feed")
            .position
            .position;
        assert!(position > seal_after.completion_cut.position);
    }
    for late_hash in [&note_hash, &gate_hash] {
        let evidence = store
            .get::<WorkEvidence>(late_hash)
            .expect("late evidence read")
            .expect("late evidence");
        assert_eq!(evidence.actor.actor_id, "peer");
        assert_eq!(
            evidence.actor.session_id,
            Some(SessionId("late-finding-peer".into()))
        );
        assert_eq!(evidence.claim_id, seal_after.claim_id);
        assert_eq!(evidence.claim_fence, seal_after.claim_fence);
        assert!(evidence.actor.provenance_chain.iter().any(|link| {
            link.relation == ProvenanceRelation::DerivedFrom
                && link.source == POST_COMPLETION_EVIDENCE_PROVENANCE_SOURCE
                && link.reference.as_deref() == Some(POST_COMPLETION_EVIDENCE_PROVENANCE_REFERENCE)
        }));
    }
    assert!(store.verify_all().expect("integrity report").is_healthy());
    drop(store);

    let focus = peer
        .inspect_work(&work.short_ref, at(7))
        .expect("show projection after late findings");
    assert_eq!(focus.status.work.lifecycle, WorkLifecycle::Completed);
    assert!(focus.allowed_next.contains(&"work_update:note".into()));
    assert!(focus.allowed_next.contains(&"work_update:gate".into()));
    assert!(focus.allowed_next.contains(&"work_update:reopen".into()));
    assert!(focus.evidence_items.iter().any(|item| {
        item.evidence == note_hash && item.summary == "late review found a documentation mismatch"
    }));
    assert!(focus.evidence_items.iter().any(|item| {
        item.evidence == gate_hash
            && item
                .gate
                .as_ref()
                .is_some_and(|gate| gate.name == "cargo-test" && !gate.passed)
    }));
    assert_eq!(
        focus
            .latest_evidence_item
            .as_ref()
            .map(|item| &item.evidence),
        Some(&gate_hash)
    );

    let changes = observer
        .work_next(
            100,
            WorkNextQuery {
                sections: vec![WorkNextSection::Changes],
                ..WorkNextQuery::default()
            },
            at(8),
        )
        .expect("observer receives late findings");
    let delivered_changes = changes.changes.expect("changes");
    let late_change_count = delivered_changes
        .iter()
        .filter(|change| {
            matches!(
                &change.delivery,
                WorkChangeProjection::Visible(summary)
                    if summary.work_id == Some(work.work_id)
                        && summary.change_kind == "evidence_added"
            )
        })
        .count();
    assert_eq!(late_change_count, 2);

    assert!(matches!(
        peer.work_update_on(
            Some(&work.short_ref),
            WorkUpdateInput::Revise {
                patch: WorkRevisionPatch {
                    title: Some("completed work must stay frozen".into()),
                    ..WorkRevisionPatch::default()
                },
                idempotency_key: "late-finding-revise".into(),
            },
            at(9),
        ),
        Err(StoreError::InvalidWork(message))
            if message == COMPLETED_WORK_LATE_FINDING_REFUSAL
    ));
    assert!(matches!(
        peer.work_handoff_on(
            Some(&work.short_ref),
            WorkHandoffInput::Offer {
                to: "late-finding-observer".into(),
                ttl_seconds: Some(300),
                checkpoint_summary: "completed work cannot be handed off".into(),
                idempotency_key: "late-finding-handoff".into(),
            },
            at(10),
        ),
        Err(StoreError::InvalidWork(message))
            if message == COMPLETED_WORK_LATE_FINDING_REFUSAL
    ));
    assert!(matches!(
        peer.work_propose(
            WorkProposeInput::Decompose {
                children: vec![WorkChildInput {
                    notes: Vec::new(),
                    key: "late-child".into(),
                    title: "completed work cannot gain a child".into(),
                    outcome: "no child is created".into(),
                    acceptance: vec!["no child exists".into()],
                    requirement: Some(ChildRequirement::Required),
                    kind: None,
                    priority: None,
                    labels: Vec::new(),
                    assigned_to: None,
                    deferred_until: None,
                }],
                prerequisites: Vec::new(),
                idempotency_key: "late-finding-decompose".into(),
            },
            at(11),
        ),
        Err(StoreError::WorkParentNotOpen { parent, lifecycle: WorkLifecycle::Completed })
            if parent == work.work_id
    ));
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one scenario covers handoff and same-session reclaim gate replay authority"
)]
fn identical_gate_after_handoff_or_reclaim_is_a_new_claim_observation() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("handoff-gate-observation".into());
    let first = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("gate-holder-a".into()),
        Some("gate-handoff-test".into()),
    );
    let second = LocalWorkService::new(
        database.clone(),
        project,
        "agent".into(),
        SessionId("gate-holder-b".into()),
        Some("gate-handoff-test".into()),
    );
    let work = match first
        .work_propose(root_input("Handoff gate", "handoff-gate-root"), at(0))
        .expect("root")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    first
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "gate-holder-a-claim".into(),
            },
            at(1),
        )
        .expect("first claim");
    let first_gate = first
        .work_gate("cargo-test", &[], None, at(2))
        .expect("first holder gate");
    let first_hash: ObjectHash =
        serde_json::from_value(first_gate.receipt.result).expect("first gate hash");
    second
        .work_focus(&work.short_ref, at(3))
        .expect("non-holder focus");
    assert!(matches!(
        second
            .work_gate("cargo-test", &[], None, at(3))
            .expect_err("a non-holder cannot reuse the holder's gate observation"),
        StoreError::WorkClaimMismatch { .. }
    ));
    first
        .work_handoff(
            WorkHandoffInput::Offer {
                to: "gate-holder-b".into(),
                ttl_seconds: Some(300),
                checkpoint_summary: "handoff after the first gate observation".into(),
                idempotency_key: "gate-handoff-offer".into(),
            },
            at(4),
        )
        .expect("offer handoff");
    second
        .work_focus(&work.short_ref, at(5))
        .expect("second holder focus");
    second
        .work_handoff(
            WorkHandoffInput::Accept {
                idempotency_key: "gate-handoff-accept".into(),
            },
            at(6),
        )
        .expect("accept handoff");
    assert!(matches!(
        first
            .work_gate("cargo-test", &[], None, at(7))
            .expect_err("the outgoing holder cannot replay after handoff acceptance"),
        StoreError::WorkClaimMismatch { .. }
    ));

    let second_gate = second
        .work_gate("cargo-test", &[], None, at(8))
        .expect("second holder records the same result");
    let second_hash: ObjectHash =
        serde_json::from_value(second_gate.receipt.result).expect("second gate hash");
    assert_ne!(second_hash, first_hash);
    let evidence = SqliteStore::open(&database)
        .expect("store")
        .get::<WorkEvidence>(&second_hash)
        .expect("second evidence read")
        .expect("second evidence");
    assert_eq!(
        evidence
            .gate
            .as_ref()
            .and_then(|gate| gate.previous.as_ref()),
        Some(&first_hash)
    );
    assert_eq!(
        evidence.actor.session_id.as_ref(),
        Some(&SessionId("gate-holder-b".into()))
    );
    second
        .work_update(
            WorkUpdateInput::Release {
                reason: "pause after verification".into(),
                waiver_reason: None,
                idempotency_key: "gate-holder-b-release".into(),
            },
            at(9),
        )
        .expect("release second holder claim");
    assert!(matches!(
        second
            .work_gate("cargo-test", &[], None, at(10))
            .expect_err("a released holder cannot replay gate evidence"),
        StoreError::WorkClaimMismatch { .. }
    ));
    second
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "gate-holder-b-reclaim".into(),
            },
            at(11),
        )
        .expect("same session reclaims the run");
    let reclaimed = second
        .work_gate("cargo-test", &[], None, at(12))
        .expect("same result under a new claim is a fresh observation");
    let reclaimed_hash: ObjectHash =
        serde_json::from_value(reclaimed.receipt.result).expect("reclaimed gate hash");
    assert_ne!(reclaimed_hash, second_hash);
    let reclaimed_evidence = SqliteStore::open(&database)
        .expect("store")
        .get::<WorkEvidence>(&reclaimed_hash)
        .expect("reclaimed evidence read")
        .expect("reclaimed evidence");
    assert_eq!(
        reclaimed_evidence
            .gate
            .as_ref()
            .and_then(|gate| gate.previous.as_ref()),
        Some(&second_hash)
    );
    assert!(reclaimed_evidence.claim_fence > evidence.claim_fence);
}

#[test]
fn explicit_gate_target_wins_after_same_session_focus_change() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("explicit-gate-target".into());
    let service = LocalWorkService::new(
        database.clone(),
        project,
        "agent".into(),
        SessionId("shared-gate-session".into()),
        Some("explicit-gate-test".into()),
    );
    let create = |title: &str, key: &str| match service
        .work_propose(root_input(title, key), at(0))
        .expect("root")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    let target = create("Explicit gate target", "explicit-gate-target");
    let other = create("Concurrent gate focus", "concurrent-gate-focus");
    service
        .work_update_on(
            Some(&target.work_id.0.to_string()),
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "explicit-gate-claim".into(),
            },
            at(1),
        )
        .expect("claim explicit target");
    service
        .work_focus(&other.short_ref, at(2))
        .expect("same session changes focus before gate");

    let result = service
        .work_gate_on(
            Some(&target.work_id.0.to_string()),
            "cargo-test",
            &[],
            None,
            at(3),
        )
        .expect("explicit gate remains bound to target");
    let evidence_hash: ObjectHash =
        serde_json::from_value(result.receipt.result).expect("evidence hash");
    let evidence = SqliteStore::open(&database)
        .expect("store")
        .get::<WorkEvidence>(&evidence_hash)
        .expect("evidence read")
        .expect("evidence");
    assert_eq!(evidence.work_id, target.work_id);
    assert_ne!(evidence.work_id, other.work_id);
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one boundary test covers normalization, typed projection, and direct-storage refusal"
)]
fn gate_storage_owns_normalization_and_bounds() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("gate-storage-boundary".into());
    let service = LocalWorkService::new(
        database,
        project,
        "agent".into(),
        SessionId("gate-storage-session".into()),
        Some("gate-storage-test".into()),
    );
    let work = match service
        .work_propose(root_input("Gate storage", "gate-storage-root"), at(0))
        .expect("root")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: "gate-storage-claim".into(),
            },
            at(1),
        )
        .expect("claim");
    let claim = service
        .store()
        .expect("store")
        .current_work_claim(work.work_id)
        .expect("claim projection")
        .expect("live claim");
    let request = RecordGateEvidenceRequest {
        work_id: work.work_id,
        run_id: claim.run_id,
        expected_work_revision: work.revision,
        holder: service.session_id.clone(),
        claim_id: claim.claim_id,
        claim_fence: claim.fence,
        name: "  CARGO-TEST  ".into(),
        failed: vec![" suite::b ".into(), "suite::a".into(), "suite::a".into()],
        evidence_ref: Some(" logs/gate.txt ".into()),
        actor: service.actor("work_update", "exercise the gate storage boundary"),
        recorded_at: at(2),
    };
    let evidence_hash = service
        .store()
        .expect("store")
        .record_gate_evidence(&request, &DevelopmentNoopRedactor)
        .expect("normalized gate evidence");
    let evidence = service
        .store()
        .expect("store")
        .get::<WorkEvidence>(&evidence_hash)
        .expect("evidence read")
        .expect("evidence");
    let gate = evidence.gate.expect("typed gate payload");
    assert_eq!(gate.name, "cargo-test");
    assert_eq!(gate.failed, vec!["suite::a", "suite::b"]);
    assert_eq!(evidence.refs, vec!["logs/gate.txt"]);
    assert_eq!(evidence.summary, GATE_EVIDENCE_SUMMARY);

    let mimicking_hash: ObjectHash = serde_json::from_value(
        service
            .work_update(
                WorkUpdateInput::Evidence {
                    summary: "gate cargo-test failed (2 failures): suite::a, suite::b".into(),
                    refs: Vec::new(),
                    attach: None,
                    idempotency_key: "gate-shaped-note".into(),
                },
                at(3),
            )
            .expect("gate-shaped generic evidence")
            .receipt
            .result,
    )
    .expect("generic evidence hash");
    let focus = service
        .inspect_work(&work.short_ref, at(4))
        .expect("projected evidence");
    let projected_gate = focus
        .evidence_items
        .iter()
        .find(|item| item.evidence == evidence_hash)
        .expect("typed gate projection")
        .gate
        .as_ref()
        .expect("typed gate discriminator");
    assert_eq!(projected_gate.name, "cargo-test");
    assert!(!projected_gate.passed);
    assert_eq!(projected_gate.failed_count, 2);
    assert!(
        focus
            .evidence_items
            .iter()
            .find(|item| item.evidence == mimicking_hash)
            .expect("gate-shaped generic projection")
            .gate
            .is_none(),
        "generic prose must not acquire the typed gate discriminator"
    );

    let mut oversized = request;
    oversized.name = "x".repeat(crate::domain::MAX_GATE_NAME_BYTES + 1);
    oversized.recorded_at = at(3);
    assert!(matches!(
        service
            .store()
            .expect("store")
            .record_gate_evidence(&oversized, &DevelopmentNoopRedactor)
            .expect_err("storage rejects oversized gate identity"),
        StoreError::InvalidWork(detail) if detail.contains("gate_input_too_large")
    ));
    oversized.name = "cargo\u{e0020}test".into();
    oversized.recorded_at = at(4);
    assert!(matches!(
        service
            .store()
            .expect("store")
            .record_gate_evidence(&oversized, &DevelopmentNoopRedactor)
            .expect_err("storage rejects invisible gate identity"),
        StoreError::InvalidWork(detail) if detail.contains("control or format")
    ));
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the two-depth regression keeps identical setup and three measured lifecycle mutations together"
)]
fn gate_heavy_evidence_membership_has_constant_decode_cost() {
    const FIRST_GATE_TRANSITION_COUNT: usize = 64;
    const SECOND_GATE_TRANSITION_COUNT: usize = FIRST_GATE_TRANSITION_COUNT * 2;
    const CANONICAL_DECODE_BUDGET: usize = 64;

    fn measure(gate_transition_count: usize) -> [usize; 3] {
        let directory = tempdir().expect("temp directory");
        let database = directory.path().join("engram.sqlite3");
        let service = LocalWorkService::new(
            database,
            ProjectId(format!(
                "gate-heavy-evidence-membership-{gate_transition_count}"
            )),
            "agent".into(),
            SessionId(format!("gate-heavy-session-{gate_transition_count}")),
            Some("protocol-test".into()),
        );
        let work = proposed_root(
            service
                .work_propose(
                    root_input("Gate-heavy evidence membership", "gate-heavy-root"),
                    at(0),
                )
                .expect("create gate-heavy work"),
        );
        service
            .work_update_on(
                Some(&work.short_ref),
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(3_600),
                    recovery_reason: None,
                    idempotency_key: "gate-heavy-claim".into(),
                },
                at(1),
            )
            .expect("claim gate-heavy work");

        for index in 0..gate_transition_count {
            let failed = if index % 2 == 0 {
                Vec::new()
            } else {
                vec!["alternating failure".to_owned()]
            };
            service
                .work_gate_on(
                    Some(&work.short_ref),
                    "gate-heavy",
                    &failed,
                    None,
                    at(2 + i64::try_from(index).expect("gate timestamp")),
                )
                .expect("record gate transition");
        }
        let after_gates = 2 + i64::try_from(gate_transition_count).expect("gate count");

        crate::canonical::reset_canonical_decode_count();
        service
            .work_note_on(
                Some(&work.short_ref),
                "gate-heavy note",
                &[],
                at(after_gates),
            )
            .expect("record gate-heavy note");
        let note_decodes = crate::canonical::canonical_decode_count();

        crate::canonical::reset_canonical_decode_count();
        service
            .work_update_on(
                Some(&work.short_ref),
                WorkUpdateInput::Checkpoint {
                    summary: "gate-heavy explicit checkpoint".into(),
                    evidence: None,
                    idempotency_key: "gate-heavy-checkpoint".into(),
                },
                at(after_gates + 1),
            )
            .expect("record gate-heavy explicit checkpoint");
        let checkpoint_decodes = crate::canonical::canonical_decode_count();

        crate::canonical::reset_canonical_decode_count();
        let completed = service
            .work_complete_on(
                Some(&work.short_ref),
                WorkCompleteInput {
                    capture: None,
                    evidence: Vec::new(),
                    acceptance: None,
                    note: Some("gate-heavy completion".into()),
                    idempotency_key: "gate-heavy-complete".into(),
                },
                at(after_gates + 2),
            )
            .expect("complete gate-heavy work");
        assert!(matches!(completed, WorkCompleteResult::Completed(_)));
        let completion_decodes = crate::canonical::canonical_decode_count();

        [note_decodes, checkpoint_decodes, completion_decodes]
    }

    let first = measure(FIRST_GATE_TRANSITION_COUNT);
    let second = measure(SECOND_GATE_TRANSITION_COUNT);
    for (operation, first_count, second_count) in [
        ("note", first[0], second[0]),
        ("checkpoint", first[1], second[1]),
        ("completion", first[2], second[2]),
    ] {
        assert_eq!(
            first_count, second_count,
            "{operation} canonical decode cost grew between {FIRST_GATE_TRANSITION_COUNT} and {SECOND_GATE_TRANSITION_COUNT} gate transitions"
        );
        assert!(
            second_count <= CANONICAL_DECODE_BUDGET,
            "{operation} decoded {second_count} canonical objects after {SECOND_GATE_TRANSITION_COUNT} gate transitions; budget is {CANONICAL_DECODE_BUDGET}"
        );
    }
}

#[test]
#[ignore = "runs separately so the project-scale fixture and decode samples stay out of the ordinary suite"]
#[allow(
    clippy::too_many_lines,
    reason = "one scale regression measures the complete claim-validated mutation family against one fixed project fixture"
)]
fn claim_validated_mutations_are_bounded_at_project_scale() {
    // The long-lived MCP server retains one service for its process
    // lifetime, so these samples include exactly the production warm-call
    // lifecycle rather than silently omitting a per-request reopen.
    const ITEM_COUNT: usize = 500;
    const TOTAL_EVENT_COUNT: usize = 5_000;
    const DEEP_EVENT_COUNT: usize = 500;
    const SAMPLE_COUNT: usize = 20;
    const GATE_TRANSITION_COUNT: usize = 128;

    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("claim-mutation-scale".into());
    let writer = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("mutation-writer".into()),
        Some("protocol-test".into()),
    );
    let reader = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("mutation-reader".into()),
        Some("protocol-test".into()),
    );

    let mut work_items = Vec::with_capacity(ITEM_COUNT);
    for item_index in 0..ITEM_COUNT {
        let work = match writer
            .work_propose(
                root_input(
                    &format!("Claim mutation item {item_index:03}"),
                    &format!("claim-mutation-root-{item_index:03}"),
                ),
                at(i64::try_from(item_index).expect("item timestamp")),
            )
            .expect("create scale root")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        work_items.push(work);
    }

    let mut event_store = SqliteStore::open(&database).expect("event store");
    let mut synthetic_events = Vec::with_capacity(TOTAL_EVENT_COUNT - ITEM_COUNT);
    for (item_index, work) in work_items.iter().enumerate() {
        let entry = event_store
            .work_event_tail(work.work_id, 1)
            .expect("base event tail")
            .pop()
            .expect("base event");
        let base = event_store
            .get::<WorkEvent>(&entry.object_hash)
            .expect("load base event")
            .expect("base event object");
        let event_count = if item_index == ITEM_COUNT - 1 {
            DEEP_EVENT_COUNT
        } else if item_index < 9 {
            10
        } else {
            9
        };
        for event_index in 1..event_count {
            let mut event = base.clone();
            event.created_at = at(600 + i64::try_from(event_index).expect("event timestamp"));
            event.actor.reason =
                format!("claim mutation scale item {item_index:03} event {event_index:02}");
            synthetic_events.push(event);
        }
    }
    event_store
        .append_test_work_events(&synthetic_events)
        .expect("append scale event history");
    assert_eq!(
        event_store
            .work_feed_head(&FeedId::Project(project.clone()))
            .expect("scale project feed head"),
        i64::try_from(TOTAL_EVENT_COUNT).expect("scale feed head")
    );
    drop(event_store);

    let sampled_work = &work_items[ITEM_COUNT - SAMPLE_COUNT..];
    let mut claim_samples = Vec::with_capacity(SAMPLE_COUNT);
    for (sample_index, work) in sampled_work.iter().enumerate() {
        writer
            .select_work(
                &work.short_ref,
                at(1_100 + i64::try_from(sample_index).expect("select timestamp")),
            )
            .expect("select claim target");
        measure_scale_operation(&mut claim_samples, || {
            writer.work_update(
                WorkUpdateInput::Claim {
                    ttl_seconds: Some(3_600),
                    recovery_reason: None,
                    idempotency_key: format!("scale-claim-{sample_index:02}"),
                },
                at(1_120 + i64::try_from(sample_index).expect("claim timestamp")),
            )
        })
        .expect("claim scale target");
    }

    let mut work_next_samples = Vec::with_capacity(SAMPLE_COUNT);
    for sample_index in 0..SAMPLE_COUNT {
        measure_scale_operation(&mut work_next_samples, || {
            reader.work_next(
                50,
                WorkNextQuery {
                    sections: vec![
                        WorkNextSection::Ready,
                        WorkNextSection::Assigned,
                        WorkNextSection::Participated,
                    ],
                    ..WorkNextQuery::default()
                },
                at(1_150 + i64::try_from(sample_index).expect("work_next timestamp")),
            )
        })
        .expect("measure ready and claimless discovery work_next");
    }

    let mut evidence_samples = Vec::with_capacity(SAMPLE_COUNT);
    for (sample_index, work) in sampled_work.iter().enumerate() {
        writer
            .select_work(
                &work.short_ref,
                at(1_170 + i64::try_from(sample_index * 2).expect("select timestamp")),
            )
            .expect("select evidence target");
        measure_scale_operation(&mut evidence_samples, || {
            writer.work_update(
                WorkUpdateInput::Evidence {
                    summary: format!("scale evidence {sample_index:02}"),
                    refs: vec![format!("test:claim-mutation-scale:{sample_index:02}")],
                    attach: None,
                    idempotency_key: format!("scale-evidence-{sample_index:02}"),
                },
                at(1_171 + i64::try_from(sample_index * 2).expect("evidence timestamp")),
            )
        })
        .expect("record scale evidence");
    }

    let mut gate_samples = Vec::with_capacity(GATE_TRANSITION_COUNT);
    let gate_target = sampled_work.last().expect("sampled gate target");
    for sample_index in 0..GATE_TRANSITION_COUNT {
        let failed = if sample_index % 2 == 0 {
            Vec::new()
        } else {
            vec!["scale alternating failure".to_owned()]
        };
        measure_scale_operation(&mut gate_samples, || {
            writer.work_gate_on(
                Some(&gate_target.short_ref),
                "claim-mutation-scale",
                &failed,
                None,
                at(1_200 + i64::try_from(sample_index).expect("gate timestamp")),
            )
        })
        .expect("record scale gate transition");
    }

    let mut note_samples = Vec::with_capacity(1);
    measure_scale_operation(&mut note_samples, || {
        writer.work_note_on(
            Some(&gate_target.short_ref),
            "scale gate-heavy note",
            &[],
            at(1_350),
        )
    })
    .expect("record scale gate-heavy note");

    let mut checkpoint_samples = Vec::with_capacity(SAMPLE_COUNT);
    for (sample_index, work) in sampled_work.iter().enumerate() {
        writer
            .select_work(
                &work.short_ref,
                at(1_400 + i64::try_from(sample_index * 2).expect("select timestamp")),
            )
            .expect("select checkpoint target");
        measure_scale_operation(&mut checkpoint_samples, || {
            writer.work_update(
                WorkUpdateInput::Checkpoint {
                    summary: format!("scale checkpoint {sample_index:02}"),
                    evidence: None,
                    idempotency_key: format!("scale-checkpoint-{sample_index:02}"),
                },
                at(1_401 + i64::try_from(sample_index * 2).expect("checkpoint timestamp")),
            )
        })
        .expect("record scale checkpoint");
    }

    let mut revise_samples = Vec::with_capacity(SAMPLE_COUNT);
    for (sample_index, work) in sampled_work.iter().enumerate() {
        writer
            .select_work(
                &work.short_ref,
                at(1_450 + i64::try_from(sample_index * 2).expect("select timestamp")),
            )
            .expect("select revision target");
        measure_scale_operation(&mut revise_samples, || {
            writer.work_update(
                WorkUpdateInput::Revise {
                    patch: WorkRevisionPatch {
                        title: Some(format!("Claim mutation target revision {sample_index:02}")),
                        ..WorkRevisionPatch::default()
                    },
                    idempotency_key: format!("scale-revise-{sample_index:02}"),
                },
                at(1_451 + i64::try_from(sample_index * 2).expect("revise timestamp")),
            )
        })
        .expect("revise scale target");
    }

    let mut block_samples = Vec::with_capacity(SAMPLE_COUNT);
    let mut unblock_samples = Vec::with_capacity(SAMPLE_COUNT);
    for (sample_index, work) in sampled_work.iter().enumerate() {
        writer
            .select_work(
                &work.short_ref,
                at(1_500 + i64::try_from(sample_index * 3).expect("select timestamp")),
            )
            .expect("select blocker target");
        let blocked = measure_scale_operation(&mut block_samples, || {
            writer.work_update(
                WorkUpdateInput::Block {
                    blocker_kind: WorkBlockerKind::Manual,
                    detail: format!("scale blocker {sample_index:02}"),
                    idempotency_key: format!("scale-block-{sample_index:02}"),
                },
                at(1_501 + i64::try_from(sample_index * 3).expect("block timestamp")),
            )
        })
        .expect("block scale target");
        let blocker_id = blocked
            .receipt
            .result
            .get("blocker_id")
            .and_then(serde_json::Value::as_str)
            .expect("blocker receipt id")
            .to_owned();
        measure_scale_operation(&mut unblock_samples, || {
            writer.work_update(
                WorkUpdateInput::Unblock {
                    blocker_id: Some(blocker_id),
                    idempotency_key: format!("scale-unblock-{sample_index:02}"),
                },
                at(1_502 + i64::try_from(sample_index * 3).expect("unblock timestamp")),
            )
        })
        .expect("unblock scale target");
    }

    let mut handoff_samples = Vec::with_capacity(SAMPLE_COUNT);
    for (sample_index, work) in sampled_work.iter().enumerate() {
        writer
            .select_work(
                &work.short_ref,
                at(1_600 + i64::try_from(sample_index * 4).expect("select timestamp")),
            )
            .expect("select handoff target");
        measure_scale_operation(&mut handoff_samples, || {
            writer.work_handoff(
                WorkHandoffInput::Offer {
                    to: "handoff-peer".into(),
                    ttl_seconds: Some(300),
                    checkpoint_summary: format!("scale handoff checkpoint {sample_index:02}"),
                    idempotency_key: format!("scale-handoff-offer-{sample_index:02}"),
                },
                at(1_601 + i64::try_from(sample_index * 4).expect("offer timestamp")),
            )
        })
        .expect("offer scale handoff");
        writer
            .work_handoff(
                WorkHandoffInput::Cancel {
                    reason: "restore benchmark executor".into(),
                    idempotency_key: format!("scale-handoff-cancel-{sample_index:02}"),
                },
                at(1_602 + i64::try_from(sample_index * 4).expect("cancel timestamp")),
            )
            .expect("cancel scale handoff");
        writer
            .work_update(
                WorkUpdateInput::Checkpoint {
                    summary: format!("post-handoff checkpoint {sample_index:02}"),
                    evidence: None,
                    idempotency_key: format!("scale-post-handoff-checkpoint-{sample_index:02}"),
                },
                at(1_603 + i64::try_from(sample_index * 4).expect("checkpoint timestamp")),
            )
            .expect("checkpoint after scale handoff");
    }

    let mut complete_samples = Vec::with_capacity(SAMPLE_COUNT);
    for (sample_index, work) in sampled_work.iter().enumerate() {
        writer
            .select_work(
                &work.short_ref,
                at(1_700 + i64::try_from(sample_index * 2).expect("select timestamp")),
            )
            .expect("select completion target");
        let completed = measure_scale_operation(&mut complete_samples, || {
            writer.work_complete(
                WorkCompleteInput {
                    capture: None,
                    evidence: Vec::new(),
                    acceptance: None,
                    note: Some(format!("scale completion {sample_index:02}")),
                    idempotency_key: format!("scale-complete-{sample_index:02}"),
                },
                at(1_701 + i64::try_from(sample_index * 2).expect("complete timestamp")),
            )
        })
        .expect("complete scale target");
        assert!(matches!(completed, WorkCompleteResult::Completed(_)));
    }

    for (operation, samples, expected_samples) in [
        ("claim", &claim_samples, SAMPLE_COUNT),
        ("evidence", &evidence_samples, SAMPLE_COUNT),
        ("gate", &gate_samples, GATE_TRANSITION_COUNT),
        ("note", &note_samples, 1),
        ("checkpoint", &checkpoint_samples, SAMPLE_COUNT),
        ("revise", &revise_samples, SAMPLE_COUNT),
        ("block", &block_samples, SAMPLE_COUNT),
        ("unblock", &unblock_samples, SAMPLE_COUNT),
        ("handoff", &handoff_samples, SAMPLE_COUNT),
        ("complete", &complete_samples, SAMPLE_COUNT),
        ("work_next", &work_next_samples, SAMPLE_COUNT),
    ] {
        assert_eq!(samples.len(), expected_samples);
        report_scale_samples(operation, samples);
        let (canonical_budget, work_event_budget, item_budget) = if operation == "work_next" {
            (16, 0, 64)
        } else {
            (64, 64, 16)
        };
        for (kind, actual, budget) in [
            (
                "canonical-decode",
                samples
                    .iter()
                    .map(|sample| sample.canonical_decodes)
                    .max()
                    .expect("scale samples"),
                canonical_budget,
            ),
            (
                "work-event-decode",
                samples
                    .iter()
                    .map(|sample| sample.work_event_decodes)
                    .max()
                    .expect("scale samples"),
                work_event_budget,
            ),
            (
                "item-decode",
                samples
                    .iter()
                    .map(|sample| sample.item_decodes)
                    .max()
                    .expect("scale samples"),
                item_budget,
            ),
        ] {
            assert!(
                actual <= budget,
                "{operation} exceeded its bounded {kind} budget of {budget}: {actual}"
            );
        }
    }
}
