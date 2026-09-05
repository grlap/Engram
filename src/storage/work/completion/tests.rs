use super::super::query::completion_recovery_on;
use super::super::test_support::*;
use super::super::*;
use super::*;

mod obligations;

#[test]
fn completed_gate_attempt_mismatch_refuses_before_appending() {
    let project = "completed-gate-attempt-mismatch";
    let holder = "gate-holder";
    let mut store = SqliteStore::open_in_memory().expect("gate attempt fixture");
    let work = store
        .create_work(
            &root_request(project, "gate-attempt-root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("gate attempt root");
    let claim = claim(&mut store, &work, holder, "gate-attempt-claim", 1, 300);
    let pass = RecordGateEvidenceRequest {
        work_id: work.work_id,
        run_id: claim.run_id,
        expected_work_revision: work.revision,
        holder: SessionId(holder.into()),
        claim_id: claim.claim_id,
        claim_fence: claim.fence,
        name: "cargo-test".into(),
        failed: Vec::new(),
        evidence_ref: None,
        actor: actor(holder),
        recorded_at: at(2),
    };
    let pass_hash = store
        .record_gate_evidence(&pass, &DevelopmentNoopRedactor)
        .expect("initial gate observation");
    let failed = vec!["suite::failure".into()];
    let request = RecordGateEvidenceRequest {
        failed: failed.clone(),
        recorded_at: at(3),
        ..pass
    };
    let project_id = crate::domain::ProjectId(project.into());
    let session_id = SessionId(holder.into());
    let basis = serde_json::json!({"test_basis": work.work_id});
    let intent = GateWorkProtocolIntent {
        schema_version: SCHEMA_VERSION,
        project_id: &project_id,
        session_id: &session_id,
        actor: &request.actor,
        work_id: work.work_id,
        run_id: claim.run_id,
        claim_id: claim.claim_id,
        claim_fence: claim.fence,
        name: "cargo-test",
        failed: &failed,
        refs: &[],
        previous: Some(&pass_hash),
    };
    let intent_object = CanonicalObject::freeze(&intent).expect("gate intent");
    let idempotency_key = format!("gate:{}", intent_object.hash().as_str());
    assert!(
        store
            .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
                project_id: &project_id,
                session_id: &session_id,
                operation: "work_update:gate",
                idempotency_key: &idempotency_key,
                intent: &intent,
                basis: &basis,
                now: at(3),
            })
            .expect("reserve synthetic completed attempt")
            .result
            .is_none()
    );
    store
        .finish_work_protocol_attempt(
            &project_id,
            &session_id,
            "work_update:gate",
            &idempotency_key,
            &serde_json::json!({"receipt": {"work_id": work.work_id}}),
        )
        .expect("complete synthetic attempt without evidence");

    assert!(matches!(
        store
            .record_gate_evidence_protocol(
                &request,
                &BeginGateWorkProtocolAttempt {
                    project_id: &project_id,
                    session_id: &session_id,
                    basis: &basis,
                    now: at(3),
                },
                &DevelopmentNoopRedactor,
            )
            .expect_err("a completed attempt cannot append disagreeing evidence"),
        StoreError::InvalidWorkProjection(detail)
            if detail.contains("disagrees with the latest same-name evidence")
    ));
    assert_eq!(
        store
            .work_run_evidence(claim.run_id)
            .expect("bounded gate evidence history"),
        vec![pass_hash]
    );
}

#[test]
fn completion_recovery_rejects_a_shell_unsafe_participant_id() {
    let mut store = SqliteStore::open_in_memory().expect("recovery fixture");
    let work = store
        .create_work(
            &root_request("unsafe-recovery-project", "unsafe-recovery", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root work");
    let error = completion_recovery_on(
        &store.connection,
        &work,
        WorkCompletionRecoveryCause::MissingContribution {
            participant: SessionId("peer; Remove-Item important".into()),
        },
    )
    .expect_err("shell metacharacters must not enter executable recovery guidance");
    assert!(
        matches!(error, StoreError::InvalidWorkProjection(reason) if reason.contains("shell-safe CLI argument"))
    );
}

#[test]
fn completion_checkpoint_holds_the_writer_slot_across_cut_selection_and_append() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("engram.sqlite3");
    let mut store = SqliteStore::open(&database).expect("store");
    let work = store
        .create_work(
            &root_request("completion-checkpoint-cut", "checkpoint-cut-root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("completion work");
    let claim = claim(&mut store, &work, "holder", "checkpoint-cut-claim", 1, 300);
    let evidence_hash = evidence(
        &mut store,
        &work,
        &claim,
        "holder",
        "checkpoint-cut-evidence",
        2,
    );
    let request = CheckpointWorkRequest {
        work_id: work.work_id,
        run_id: claim.run_id,
        expected_work_revision: work.revision,
        holder: SessionId("holder".into()),
        claim_id: claim.claim_id,
        claim_fence: claim.fence,
        summary: "atomic completion checkpoint".into(),
        evidence: Some(vec![evidence_hash]),
        actor: actor("holder"),
        idempotency_key: "completion-checkpoint-template".into(),
        checkpointed_at: at(3),
    };
    let mut contender = SqliteStore::open(&database).expect("contending store");
    contender
        .connection
        .busy_timeout(std::time::Duration::ZERO)
        .expect("zero contender busy timeout");
    let contender_request = RecordWorkEvidenceRequest {
        work_id: work.work_id,
        run_id: claim.run_id,
        expected_work_revision: work.revision,
        holder: SessionId("holder".into()),
        claim_id: claim.claim_id,
        claim_fence: claim.fence,
        summary: "must not interleave with the selected completion cut".into(),
        refs: Vec::new(),
        actor: actor("holder"),
        idempotency_key: "checkpoint-cut-contender".into(),
        recorded_at: at(3),
    };
    let mut probed = false;
    let (checkpoint_hash, selected_cut) = store
        .checkpoint_work_for_completion(
            &request,
            |cut| {
                probed = true;
                let Err(StoreError::Sqlite(error)) =
                    contender.record_work_evidence(&contender_request, &DevelopmentNoopRedactor)
                else {
                    panic!("a second writer must not enter after completion cut selection");
                };
                assert!(matches!(
                    error.sqlite_error_code(),
                    Some(rusqlite::ErrorCode::DatabaseBusy | rusqlite::ErrorCode::DatabaseLocked)
                ));
                Ok(format!("completion-cut-{}", cut.position))
            },
            &DevelopmentNoopRedactor,
        )
        .expect("atomic completion checkpoint");
    assert!(probed);
    let checkpoint: WorkCheckpoint = store
        .get(&checkpoint_hash)
        .expect("checkpoint read")
        .expect("canonical checkpoint");
    assert_eq!(checkpoint.acknowledged_run_position, selected_cut);
    assert_eq!(
        checkpoint_feed_end(selected_cut.position).expect("checkpoint feed end"),
        store
            .work_feed_head(&FeedId::RunExecution(claim.run_id))
            .expect("current run feed head")
    );
    assert!(
        store
            .verify_all()
            .expect("checkpoint integrity")
            .is_healthy()
    );
}

#[test]
fn expired_handoff_is_swept_before_progress_and_terminal_completion() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("project-terminal-expiry", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let claim = claim(&mut store, &root, "agent-a", "claim", 1, 100);
    let evidence_before_offer = evidence(
        &mut store,
        &root,
        &claim,
        "agent-a",
        "evidence-before-offer",
        2,
    );
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
                checkpoint_summary: "short-lived transfer".into(),
                actor: actor("agent-a"),
                idempotency_key: "offer".into(),
                offered_at: at(3),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("offer");
    assert_eq!(offer.expires_at, at(5));

    let blocked = store.record_work_evidence(
        &RecordWorkEvidenceRequest {
            work_id: root.work_id,
            run_id: claim.run_id,
            expected_work_revision: root.revision,
            holder: claim.holder.clone(),
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
            summary: "must wait".into(),
            refs: Vec::new(),
            actor: actor("agent-a"),
            idempotency_key: "blocked-evidence".into(),
            recorded_at: at(4),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(blocked, Err(StoreError::InvalidWork(_))));

    let refused_completion = store.complete_work_for_protocol(
        &completion_request(
            &root,
            &claim,
            "agent-a",
            &evidence_before_offer,
            "refused-expired-handoff-completion",
            6,
        ),
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(
        refused_completion,
        Err(StoreError::WorkCompletionRefused { .. })
    ));
    let offered_state = store
        .connection
        .query_row(
            "SELECT state FROM work_handoff_offers WHERE offer_id = ?1",
            [offer.offer_id.0.to_string()],
            |row| row.get::<_, String>(0),
        )
        .expect("rolled-back offer state");
    assert_eq!(offered_state, "offered");
    let expired_events_after_refusal = store
        .work_event_tail(root.work_id, 100)
        .expect("events after refused completion")
        .into_iter()
        .filter_map(|entry| {
            load_typed_work_object::<WorkEvent>(&store.connection, &entry.object_hash, "work_event")
                .ok()
        })
        .filter(|event| matches!(event.transition, WorkTransition::HandoffExpired { .. }))
        .count();
    assert_eq!(expired_events_after_refusal, 0);

    let evidence = evidence(
        &mut store,
        &root,
        &claim,
        "agent-a",
        "post-expiry-evidence",
        6,
    );
    let offer_state = store
        .connection
        .query_row(
            "SELECT state FROM work_handoff_offers WHERE offer_id = ?1",
            [offer.offer_id.0.to_string()],
            |row| row.get::<_, String>(0),
        )
        .expect("expired offer state");
    assert_eq!(offer_state, "expired");
    checkpoint(
        &mut store,
        &root,
        &claim,
        "agent-a",
        "post-expiry-checkpoint",
        7,
        std::slice::from_ref(&evidence),
    );
    complete(
        &mut store,
        &root,
        &claim,
        "agent-a",
        &evidence,
        "post-expiry-complete",
        8,
    )
    .expect("terminal completion after expired handoff sweep");
    let expired_events_after_completion = store
        .work_event_tail(root.work_id, 100)
        .expect("events after terminal completion")
        .into_iter()
        .filter_map(|entry| {
            load_typed_work_object::<WorkEvent>(&store.connection, &entry.object_hash, "work_event")
                .ok()
        })
        .filter(|event| matches!(event.transition, WorkTransition::HandoffExpired { .. }))
        .count();
    assert_eq!(expired_events_after_completion, 1);
}
