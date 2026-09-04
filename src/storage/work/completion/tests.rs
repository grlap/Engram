use super::super::query::{completion_recovery_on, inspect_work_on};
use super::super::test_support::*;
use super::super::*;
use super::*;

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
fn disposing_claimed_child_records_an_attributed_participant_waiver() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("project-dispose-claim", "dispose-claim-root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let decomposition = store
        .decompose_work(
            &DecomposeWorkRequest {
                parent_id: root.work_id,
                expected_parent_revision: root.revision,
                children: vec![
                    child(
                        "optional-child",
                        ChildRequirement::Optional,
                        "Optional child",
                    ),
                    child("unused-child", ChildRequirement::Optional, "Unused child"),
                ],
                prerequisites: Vec::new(),
                authority: delegated("project-dispose-claim", "planner"),
                actor: actor("planner"),
                idempotency_key: "dispose-claim-decompose".into(),
                created_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("decompose");
    let child = decomposition.children[0].clone();

    let child_claim = store
        .claim_work(
            &ClaimWorkRequest {
                work_id: child.work_id,
                expected_work_revision: child.revision,
                expected_run_id: child.active_run_id.expect("child run"),
                holder: SessionId("child-agent".into()),
                ttl_seconds: 100,
                recovery_reason: None,
                actor: actor("child-agent"),
                idempotency_key: "dispose-claim-child-claim".into(),
                claimed_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("claim child");
    store
        .dispose_work(
            &DisposeWorkRequest {
                work_id: child.work_id,
                expected_work_revision: child.revision,
                disposition: WorkDisposition::Cancelled,
                replacement_id: None,
                reason: "optional path was abandoned".into(),
                actor: actor("child-agent"),
                idempotency_key: "dispose-claim-with-waiver".into(),
                disposed_at: at(3),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("dispose with participant waiver");
    let unused_child = &decomposition.children[1];
    store
        .dispose_work(
            &DisposeWorkRequest {
                work_id: unused_child.work_id,
                expected_work_revision: unused_child.revision,
                disposition: WorkDisposition::Cancelled,
                replacement_id: None,
                reason: "unused optional path".into(),
                actor: actor("planner"),
                idempotency_key: "dispose-unused-child".into(),
                disposed_at: at(4),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("dispose unused child");

    let root_claim = claim(
        &mut store,
        &decomposition.parent,
        "root-agent",
        "dispose-claim-root-claim",
        5,
        100,
    );
    let root_evidence = evidence(
        &mut store,
        &decomposition.parent,
        &root_claim,
        "root-agent",
        "dispose-claim-root-evidence",
        6,
    );
    checkpoint(
        &mut store,
        &decomposition.parent,
        &root_claim,
        "root-agent",
        "dispose-claim-root-checkpoint",
        7,
        std::slice::from_ref(&root_evidence),
    );
    let seal = complete(
        &mut store,
        &decomposition.parent,
        &root_claim,
        "root-agent",
        &root_evidence,
        "dispose-claim-root-complete",
        8,
    )
    .expect("root completes with child participant accounted");
    assert!(seal.waivers.iter().any(|waiver| {
        waiver.participant == child_claim.holder && waiver.reason == "optional path was abandoned"
    }));
}

#[test]
fn cancelled_required_child_blocks_completion_until_an_attributed_waiver() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("project-waiver", "waiver-root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let decomposition = store
        .decompose_work(
            &DecomposeWorkRequest {
                parent_id: root.work_id,
                expected_parent_revision: root.revision,
                children: vec![
                    child(
                        "required-cancelled",
                        ChildRequirement::Required,
                        "Required but deliberately omitted",
                    ),
                    child(
                        "optional-open",
                        ChildRequirement::Optional,
                        "Optional work may remain open",
                    ),
                ],
                prerequisites: Vec::new(),
                authority: delegated("project-waiver", "planner"),
                actor: actor("planner"),
                idempotency_key: "waiver-decompose".into(),
                created_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("decompose");
    let root = decomposition.parent;
    let child = decomposition.children[0].clone();
    store
        .dispose_work(
            &DisposeWorkRequest {
                work_id: child.work_id,
                expected_work_revision: child.revision,
                disposition: WorkDisposition::Cancelled,
                replacement_id: None,
                reason: "child is no longer required for the accepted outcome".into(),
                actor: actor("planner"),
                idempotency_key: "cancel-required-child".into(),
                disposed_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("cancel required child");

    let root_claim = claim(&mut store, &root, "root-agent", "waiver-root-claim", 3, 100);
    let root_evidence = evidence(
        &mut store,
        &root,
        &root_claim,
        "root-agent",
        "waiver-root-evidence",
        4,
    );
    checkpoint(
        &mut store,
        &root,
        &root_claim,
        "root-agent",
        "waiver-root-checkpoint-before",
        5,
        std::slice::from_ref(&root_evidence),
    );
    assert!(
        !store
            .work_completion_readiness(root.work_id, &root_claim.holder, at(6))
            .expect("completion readiness")
            .0
    );
    assert!(matches!(
        complete(
            &mut store,
            &root,
            &root_claim,
            "root-agent",
            &root_evidence,
            "root-without-waiver",
            6,
        ),
        Err(StoreError::WorkCompletionRecoveryRequired {
            cause: WorkCompletionRecoveryCause::RequiredChildUnsealed {
                child: blocked_child,
            },
            ..
        }) if blocked_child == child.work_id
    ));

    let waiver_request = WaiveRequiredChildRequest {
        parent_id: root.work_id,
        child_id: child.work_id,
        expected_parent_revision: root.revision,
        reason: "the omission is explicit, attributed, and accepted".into(),
        actor: actor("planner"),
        idempotency_key: "waive-required-child".into(),
        waived_at: at(7),
    };
    let waiver = store
        .waive_required_child(&waiver_request, &DevelopmentNoopRedactor)
        .expect("authorized waiver");
    assert_eq!(
        store
            .waive_required_child(&waiver_request, &DevelopmentNoopRedactor)
            .expect("idempotent waiver"),
        waiver
    );
    let root_execution_id = store
        .get_work_run(root.active_run_id.expect("root run"))
        .expect("root run projection")
        .root_execution_id;
    let original_execution_json: Vec<u8> = store
        .connection
        .query_row(
            "SELECT execution_json FROM work_root_executions
             WHERE root_execution_id = ?1",
            [root_execution_id.0.to_string()],
            |row| row.get(0),
        )
        .expect("root execution bytes");
    let mut corrupted_execution: RootExecution =
        serde_json::from_slice(&original_execution_json).expect("root execution");
    corrupted_execution
        .required_child_waivers
        .push(waiver.clone());
    store
        .connection
        .execute(
            "UPDATE work_root_executions SET execution_json = ?2
             WHERE root_execution_id = ?1",
            params![
                root_execution_id.0.to_string(),
                serde_json::to_vec(&corrupted_execution).expect("corrupt execution JSON")
            ],
        )
        .expect("inject duplicate waiver");
    let corrupted = store.verify_all().expect("waiver integrity report");
    assert!(
        corrupted
            .invalid_work_records
            .iter()
            .any(|record| { record.ends_with(":invalid_required_child_waivers") })
    );
    assert!(matches!(
        store.work_completion_readiness(root.work_id, &root_claim.holder, at(8)),
        Err(StoreError::InvalidWorkProjection(_))
    ));
    store
        .connection
        .execute(
            "UPDATE work_root_executions SET execution_json = ?2
             WHERE root_execution_id = ?1",
            params![root_execution_id.0.to_string(), original_execution_json],
        )
        .expect("restore root execution");
    checkpoint(
        &mut store,
        &root,
        &root_claim,
        "root-agent",
        "waiver-root-checkpoint-after",
        8,
        std::slice::from_ref(&root_evidence),
    );
    let seal = complete(
        &mut store,
        &root,
        &root_claim,
        "root-agent",
        &root_evidence,
        "root-with-waiver",
        9,
    )
    .expect("complete with explicit waiver");
    assert!(seal.required_child_seals.is_empty());
    assert_eq!(seal.required_child_waivers, vec![waiver]);
}

#[test]
fn superseding_required_work_with_a_completed_optional_child_still_requires_a_waiver() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("project-supersede", "supersede-root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let decomposition = store
        .decompose_work(
            &DecomposeWorkRequest {
                parent_id: root.work_id,
                expected_parent_revision: root.revision,
                children: vec![
                    child(
                        "required-superseded",
                        ChildRequirement::Required,
                        "Required work",
                    ),
                    child(
                        "optional-replacement",
                        ChildRequirement::Optional,
                        "Unrelated optional work",
                    ),
                ],
                prerequisites: Vec::new(),
                authority: delegated("project-supersede", "planner"),
                actor: actor("planner"),
                idempotency_key: "supersede-decompose".into(),
                created_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("decompose");
    let root = decomposition.parent;
    let required = decomposition.children[0].clone();
    let optional = decomposition.children[1].clone();
    let optional_claim = claim(
        &mut store,
        &optional,
        "optional-agent",
        "optional-claim",
        2,
        100,
    );
    let optional_evidence = evidence(
        &mut store,
        &optional,
        &optional_claim,
        "optional-agent",
        "optional-evidence",
        3,
    );
    checkpoint(
        &mut store,
        &optional,
        &optional_claim,
        "optional-agent",
        "optional-checkpoint",
        4,
        std::slice::from_ref(&optional_evidence),
    );
    complete(
        &mut store,
        &optional,
        &optional_claim,
        "optional-agent",
        &optional_evidence,
        "optional-complete",
        5,
    )
    .expect("complete optional replacement");
    store
        .dispose_work(
            &DisposeWorkRequest {
                work_id: required.work_id,
                expected_work_revision: required.revision,
                disposition: WorkDisposition::Superseded,
                replacement_id: Some(optional.work_id),
                reason: "attempt to substitute unrelated completed optional work".into(),
                actor: actor("planner"),
                idempotency_key: "supersede-required".into(),
                disposed_at: at(6),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("supersede required child");

    let root_claim = claim(
        &mut store,
        &root,
        "root-agent",
        "supersede-root-claim",
        7,
        100,
    );
    let root_evidence = evidence(
        &mut store,
        &root,
        &root_claim,
        "root-agent",
        "supersede-root-evidence",
        8,
    );
    checkpoint(
        &mut store,
        &root,
        &root_claim,
        "root-agent",
        "supersede-root-checkpoint-before",
        9,
        std::slice::from_ref(&root_evidence),
    );
    assert!(matches!(
        complete(
            &mut store,
            &root,
            &root_claim,
            "root-agent",
            &root_evidence,
            "supersede-root-without-waiver",
            10,
        ),
        Err(StoreError::WorkCompletionRecoveryRequired {
            cause: WorkCompletionRecoveryCause::RequiredChildUnsealed { child },
            ..
        }) if child == required.work_id
    ));
    let waiver = store
        .waive_required_child(
            &WaiveRequiredChildRequest {
                parent_id: root.work_id,
                child_id: required.work_id,
                expected_parent_revision: root.revision,
                reason: "explicitly accept the superseded required outcome".into(),
                actor: actor("planner"),
                idempotency_key: "waive-superseded-required".into(),
                waived_at: at(11),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("waive superseded required child");
    checkpoint(
        &mut store,
        &root,
        &root_claim,
        "root-agent",
        "supersede-root-checkpoint-after",
        12,
        std::slice::from_ref(&root_evidence),
    );
    let seal = complete(
        &mut store,
        &root,
        &root_claim,
        "root-agent",
        &root_evidence,
        "supersede-root-with-waiver",
        13,
    )
    .expect("complete after explicit waiver");
    assert_eq!(seal.required_child_waivers, vec![waiver]);
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

#[test]
fn completion_seals_required_children_and_reopen_starts_a_clean_generation() {
    let directory = tempfile::tempdir().expect("tempdir");
    let database = directory.path().join("work.db");
    let (root_id, old_run, reopened_run) = {
        let mut store = SqliteStore::open(&database).expect("store");
        let root = store
            .create_work(
                &root_request("project-c", "root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("root");
        let decomposition = store
            .decompose_work(
                &DecomposeWorkRequest {
                    parent_id: root.work_id,
                    expected_parent_revision: root.revision,
                    children: vec![
                        child("required", ChildRequirement::Required, "Required child"),
                        child("optional", ChildRequirement::Optional, "Optional child"),
                    ],
                    prerequisites: Vec::new(),
                    authority: delegated(&root.project_id.0, "planner"),
                    actor: actor("planner"),
                    idempotency_key: "decompose".into(),
                    created_at: at(1),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("decompose");
        let root = decomposition.parent;
        let required = decomposition.children[0].clone();
        let optional = decomposition.children[1].clone();

        let root_claim = claim(&mut store, &root, "root-agent", "root-claim", 2, 100);
        let root_evidence = evidence(
            &mut store,
            &root,
            &root_claim,
            "root-agent",
            "root-evidence",
            3,
        );
        checkpoint(
            &mut store,
            &root,
            &root_claim,
            "root-agent",
            "root-cp",
            4,
            std::slice::from_ref(&root_evidence),
        );
        let early = complete(
            &mut store,
            &root,
            &root_claim,
            "root-agent",
            &root_evidence,
            "root-too-early",
            5,
        );
        assert!(matches!(
            early,
            Err(StoreError::WorkCompletionRecoveryRequired {
                cause: WorkCompletionRecoveryCause::RequiredChildUnsealed { child },
                ..
            }) if child == required.work_id
        ));

        let child_claim = claim(&mut store, &required, "child-agent", "child-claim", 6, 100);
        let child_run =
            load_work_run(&store.connection, child_claim.run_id).expect("required child run");
        let child_binding = ControlWorkBinding {
            root_execution_id: child_run.root_execution_id,
            work_id: required.work_id,
            run_id: child_run.run_id,
            work_revision: child_claim.accepted_work_revision,
            claim_id: child_claim.claim_id,
            claim_fence: child_claim.fence,
        };
        let mut child_actor = actor("child-agent");
        child_actor.run_id = Some(child_run.run_id.0.to_string());
        let child_basis = ExecutionSourceBasis {
            workspace_id: "workspace-child".into(),
            source_revision: "required-child-revision".into(),
        };
        let child_verification = {
            let transaction = store
                .connection
                .transaction_with_behavior(TransactionBehavior::Immediate)
                .expect("child obligation transaction");
            append_control_execution_observation_on(
                &transaction,
                &ExecutionObservation {
                    schema_version: SCHEMA_VERSION,
                    project_id: required.project_id.clone(),
                    binding: child_binding.clone(),
                    session_id: SessionId("child-agent".into()),
                    grant_id: "child-obligation-grant".into(),
                    observation_id: "child-source-mutation".into(),
                    action_fingerprint: ObjectHash::from_canonical_bytes(b"write required child"),
                    effect: EffectClass::MutateLocal,
                    outcome: ExecutionOutcome::Succeeded,
                    source_changed: true,
                    obligation_rule_set: builtin_rule_set_hash(),
                    source_basis: Some(child_basis.clone()),
                    observed_at: Some(at(7)),
                    actor: child_actor.clone(),
                    recorded_at: at(7),
                },
            )
            .expect("append child mutation");
            let producer = append_control_execution_observation_on(
                &transaction,
                &ExecutionObservation {
                    schema_version: SCHEMA_VERSION,
                    project_id: required.project_id.clone(),
                    binding: child_binding.clone(),
                    session_id: SessionId("child-agent".into()),
                    grant_id: "child-obligation-grant".into(),
                    observation_id: "child-verification".into(),
                    action_fingerprint: ObjectHash::from_canonical_bytes(
                        b"cargo test required child",
                    ),
                    effect: EffectClass::Observe,
                    outcome: ExecutionOutcome::Succeeded,
                    source_changed: false,
                    obligation_rule_set: builtin_rule_set_hash(),
                    source_basis: Some(child_basis.clone()),
                    observed_at: Some(at(7)),
                    actor: child_actor.clone(),
                    recorded_at: at(7),
                },
            )
            .expect("append child verification producer");
            let verification = append_control_verification_evidence_on(
                &transaction,
                &VerificationEvidence {
                    schema_version: SCHEMA_VERSION,
                    project_id: required.project_id.clone(),
                    binding: child_binding,
                    session_id: SessionId("child-agent".into()),
                    producer_observation: producer,
                    source_basis: child_basis,
                    environment: None,
                    check_kind: VerificationKind::Test,
                    check_fingerprint: ObjectHash::from_canonical_bytes(
                        b"cargo test required child",
                    ),
                    result: VerificationResult::Passed,
                    completed_at: at(7),
                    summary: "required-child tests passed on the latest source basis".into(),
                    refs: Vec::new(),
                    actor: child_actor,
                    recorded_at: at(7),
                },
            )
            .expect("append child verification evidence");
            transaction.commit().expect("commit child obligation");
            verification
        };
        let child_evidence = evidence(
            &mut store,
            &required,
            &child_claim,
            "child-agent",
            "child-evidence",
            7,
        );
        checkpoint(
            &mut store,
            &required,
            &child_claim,
            "child-agent",
            "child-cp",
            8,
            &[child_evidence.clone(), child_verification],
        );
        let child_seal = complete(
            &mut store,
            &required,
            &child_claim,
            "child-agent",
            &child_evidence,
            "child-complete",
            9,
        )
        .expect("complete required child");
        assert_eq!(
            child_seal.obligation_schema_version,
            COMPLETION_OBLIGATION_SCHEMA_VERSION
        );
        assert_eq!(child_seal.obligations.len(), 1);
        let child_seal_object =
            CanonicalObject::freeze(&child_seal).expect("freeze valid child completion seal");
        let mut forged_child_seal = child_seal.clone();
        forged_child_seal.environment_schema_version += 1;
        let forged_child_object = CanonicalObject::freeze(&forged_child_seal)
            .expect("freeze child seal with invalid environment basis");
        SqliteStore::insert_object(&store.connection, "completion_seal", &forged_child_object)
            .expect("insert forged child seal fixture");
        store
            .connection
            .execute(
                "UPDATE work_completion_seals SET seal_hash = ?1, seal_json = ?2
                 WHERE run_id = ?3",
                params![
                    forged_child_object.hash().as_str(),
                    forged_child_object.bytes(),
                    child_seal.run_id.0.to_string(),
                ],
            )
            .expect("bind forged child seal projection");
        store
            .connection
            .execute(
                "UPDATE work_runs SET completion_seal_hash = ?1 WHERE run_id = ?2",
                params![
                    forged_child_object.hash().as_str(),
                    child_seal.run_id.0.to_string(),
                ],
            )
            .expect("bind forged child seal to its run");
        let refused_corrupt_child = complete(
            &mut store,
            &root,
            &root_claim,
            "root-agent",
            &root_evidence,
            "root-corrupt-child-environment",
            10,
        );
        assert!(matches!(
            refused_corrupt_child,
            Err(StoreError::InvalidWorkProjection(_))
        ));
        assert!(
            store
                .verify_all()
                .expect("corrupt child environment report")
                .invalid_work_records
                .iter()
                .any(|record| record.contains("completion_seal"))
        );
        store
            .connection
            .execute(
                "UPDATE work_completion_seals SET seal_hash = ?1, seal_json = ?2
                 WHERE run_id = ?3",
                params![
                    child_seal_object.hash().as_str(),
                    child_seal_object.bytes(),
                    child_seal.run_id.0.to_string(),
                ],
            )
            .expect("restore valid child seal projection");
        store
            .connection
            .execute(
                "UPDATE work_runs SET completion_seal_hash = ?1 WHERE run_id = ?2",
                params![
                    child_seal_object.hash().as_str(),
                    child_seal.run_id.0.to_string(),
                ],
            )
            .expect("restore valid child seal run binding");
        store
            .connection
            .execute(
                "DELETE FROM objects WHERE object_hash = ?1",
                [forged_child_object.hash().as_str()],
            )
            .expect("remove forged child seal fixture");
        let root_seal = complete(
            &mut store,
            &root,
            &root_claim,
            "root-agent",
            &root_evidence,
            "root-complete",
            10,
        )
        .expect("complete root");
        assert_eq!(root_seal.required_child_seals.len(), 1);
        assert_eq!(
            root_seal.required_child_seals[0],
            CanonicalObject::freeze(&child_seal)
                .expect("freeze child seal")
                .hash()
                .clone()
        );
        assert_eq!(
            root_seal.obligation_schema_version,
            COMPLETION_OBLIGATION_SCHEMA_VERSION
        );
        assert!(root_seal.obligations.is_empty());
        assert_eq!(
            store
                .get_work_item(optional.work_id)
                .expect("optional")
                .lifecycle,
            WorkLifecycle::Open
        );
        let completion_tail = store
            .work_feed_after(
                &FeedId::RunExecution(root_claim.run_id),
                root_seal.completion_cut.position,
                10,
            )
            .expect("completion tail");
        assert_eq!(completion_tail.len(), 1);
        assert_eq!(
            completion_tail[0].position.position,
            root_seal.completion_cut.position + 1
        );

        let required_current = store.get_work_item(required.work_id).expect("required");
        let child_reopen = store.reopen_work(
            &ReopenWorkRequest {
                work_id: required.work_id,
                expected_work_revision: required_current.revision,
                reason: "invalidate completed child".into(),
                actor: actor("human"),
                idempotency_key: "child-reopen".into(),
                reopened_at: at(11),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(child_reopen, Err(StoreError::InvalidWork(_))));

        let root_current = store.get_work_item(root.work_id).expect("completed root");
        let blocked_root_reopen = store.reopen_work(
            &ReopenWorkRequest {
                work_id: root.work_id,
                expected_work_revision: root_current.revision,
                reason: "unfinished optional child still belongs to the sealed execution".into(),
                actor: actor("human"),
                idempotency_key: "root-reopen-before-optional-disposal".into(),
                reopened_at: at(12),
            },
            &DevelopmentNoopRedactor,
        );
        assert!(matches!(
            blocked_root_reopen,
            Err(StoreError::InvalidWork(_))
        ));
        store
            .dispose_work(
                &DisposeWorkRequest {
                    work_id: optional.work_id,
                    expected_work_revision: optional.revision,
                    disposition: WorkDisposition::Cancelled,
                    replacement_id: None,
                    reason: "retire optional work omitted by the sealed execution".into(),
                    actor: actor("human"),
                    idempotency_key: "dispose-optional-before-root-reopen".into(),
                    disposed_at: at(13),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("dispose unfinished optional child");
        let reopened = store
            .reopen_work(
                &ReopenWorkRequest {
                    work_id: root.work_id,
                    expected_work_revision: root_current.revision,
                    reason: "  new root execution generation  ".into(),
                    actor: actor("human"),
                    idempotency_key: "root-reopen".into(),
                    reopened_at: at(14),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("reopen root");
        assert_eq!(reopened.generation, 2);
        assert_ne!(reopened.run_id, root_claim.run_id);
        let reopened_entry = store
            .work_event_tail(root.work_id, 1)
            .expect("reopen event")
            .pop()
            .expect("reopen event tail");
        let reopened_event: WorkEvent =
            load_typed_work_object(&store.connection, &reopened_entry.object_hash, "work_event")
                .expect("canonical reopen event");
        assert!(matches!(
            reopened_event.transition,
            WorkTransition::Reopened { reason, .. }
                if reason == "new root execution generation"
        ));
        (root.work_id, root_claim.run_id, reopened.run_id)
    };

    let reopened_store = SqliteStore::open(&database).expect("reopen database");
    let item = reopened_store
        .get_work_item(root_id)
        .expect("persisted item");
    assert_eq!(item.lifecycle, WorkLifecycle::Open);
    assert_eq!(item.active_run_id, Some(reopened_run));
    assert_eq!(
        reopened_store.get_work_run(old_run).expect("old run").state,
        WorkRunState::Completed
    );
    assert_eq!(
        reopened_store
            .get_work_run(reopened_run)
            .expect("new run")
            .state,
        WorkRunState::Open
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the regression pins root sealing, stale authority, descendant disposal, and clean-generation reopen as one lifecycle"
)]
fn root_completion_fences_live_optional_descendants_and_old_generations() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("project-optional-fence", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let decomposition = store
        .decompose_work(
            &DecomposeWorkRequest {
                parent_id: root.work_id,
                expected_parent_revision: root.revision,
                children: vec![
                    child("live-optional", ChildRequirement::Optional, "Live optional"),
                    child("idle-optional", ChildRequirement::Optional, "Idle optional"),
                ],
                prerequisites: Vec::new(),
                authority: delegated(&root.project_id.0, "planner"),
                actor: actor("planner"),
                idempotency_key: "decompose-optionals".into(),
                created_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("decompose optionals");
    let root = decomposition.parent;
    let live_optional = decomposition.children[0].clone();
    let idle_optional = decomposition.children[1].clone();

    let root_claim = claim(&mut store, &root, "root-agent", "root-claim", 2, 100);
    let root_evidence = evidence(
        &mut store,
        &root,
        &root_claim,
        "root-agent",
        "root-evidence",
        3,
    );
    checkpoint(
        &mut store,
        &root,
        &root_claim,
        "root-agent",
        "root-checkpoint",
        4,
        std::slice::from_ref(&root_evidence),
    );
    let optional_claim = claim(
        &mut store,
        &live_optional,
        "optional-agent",
        "optional-claim",
        5,
        100,
    );
    let optional_evidence = evidence(
        &mut store,
        &live_optional,
        &optional_claim,
        "optional-agent",
        "optional-evidence",
        6,
    );
    checkpoint(
        &mut store,
        &live_optional,
        &optional_claim,
        "optional-agent",
        "optional-checkpoint",
        7,
        std::slice::from_ref(&optional_evidence),
    );
    let expiring_claim = claim(
        &mut store,
        &idle_optional,
        "expiring-agent",
        "expiring-claim",
        5,
        3,
    );
    let expiring_offer = store
        .offer_work_handoff(
            &OfferWorkHandoffRequest {
                work_id: idle_optional.work_id,
                run_id: expiring_claim.run_id,
                expected_work_revision: idle_optional.revision,
                from: expiring_claim.holder.clone(),
                to: SessionId("late-recipient".into()),
                claim_id: expiring_claim.claim_id,
                claim_fence: expiring_claim.fence,
                ttl_seconds: 2,
                checkpoint_summary: "offer expires before root completion".into(),
                actor: actor("expiring-agent"),
                idempotency_key: "expiring-offer".into(),
                offered_at: at(6),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("offer expiring optional handoff");

    let live_descendant = complete(
        &mut store,
        &root,
        &root_claim,
        "root-agent",
        &root_evidence,
        "root-complete-with-live-optional",
        8,
    );
    assert!(matches!(
        live_descendant,
        Err(StoreError::WorkCompletionRefused { .. })
    ));

    store
        .release_work(
            &ReleaseWorkRequest {
                work_id: live_optional.work_id,
                run_id: optional_claim.run_id,
                expected_work_revision: live_optional.revision,
                holder: optional_claim.holder.clone(),
                claim_id: optional_claim.claim_id,
                claim_fence: optional_claim.fence,
                reason: "  root is sealing without this optional child  ".into(),
                waiver_reason: None,
                actor: actor("optional-agent"),
                idempotency_key: "release-optional".into(),
                released_at: at(9),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("release optional claim");
    store
        .release_work(
            &ReleaseWorkRequest {
                work_id: idle_optional.work_id,
                run_id: expiring_claim.run_id,
                expected_work_revision: idle_optional.revision,
                holder: expiring_claim.holder.clone(),
                claim_id: expiring_claim.claim_id,
                claim_fence: expiring_claim.fence,
                reason: "handoff expired before root sealing".into(),
                waiver_reason: None,
                actor: actor("expiring-agent"),
                idempotency_key: "release-expiring-optional".into(),
                released_at: at(9),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("release renewed claim after handoff expiry");
    let release_entry = store
        .work_event_tail(live_optional.work_id, 1)
        .expect("release event")
        .pop()
        .expect("release event tail");
    let release_event: WorkEvent =
        load_typed_work_object(&store.connection, &release_entry.object_hash, "work_event")
            .expect("canonical release event");
    assert!(matches!(
        release_event.transition,
        WorkTransition::Released { reason, .. }
            if reason == "root is sealing without this optional child"
    ));
    let seal = complete(
        &mut store,
        &root,
        &root_claim,
        "root-agent",
        &root_evidence,
        "root-complete-after-release",
        10,
    )
    .expect("complete root after descendant release");
    let mut expected_unfinished = vec![idle_optional.work_id, live_optional.work_id];
    expected_unfinished.sort_by_key(|work_id| work_id.0);
    assert_eq!(seal.unfinished_optional_children, expected_unfinished);

    let backdated_accept = store.accept_work_handoff(
        &AcceptWorkHandoffRequest {
            work_id: idle_optional.work_id,
            offer_id: expiring_offer.offer_id,
            to: SessionId("late-recipient".into()),
            actor: actor("late-recipient"),
            idempotency_key: "backdated-post-root-accept".into(),
            accepted_at: at(7),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(backdated_accept, Err(StoreError::InvalidWork(_))));

    let stale_checkpoint = store.checkpoint_work(
        &CheckpointWorkRequest {
            work_id: live_optional.work_id,
            run_id: optional_claim.run_id,
            expected_work_revision: live_optional.revision,
            holder: optional_claim.holder.clone(),
            claim_id: optional_claim.claim_id,
            claim_fence: optional_claim.fence,
            summary: "must remain fenced after root completion".into(),
            evidence: Some(vec![optional_evidence]),
            actor: actor("optional-agent"),
            idempotency_key: "stale-post-root-checkpoint".into(),
            checkpointed_at: at(11),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(
        stale_checkpoint,
        Err(StoreError::WorkClaimMismatch { .. })
    ));
    let blocked = inspect_work_on(&store.connection, live_optional.work_id, at(11))
        .expect("inspect blocked optional");
    assert_eq!(blocked.availability, WorkAvailability::Blocked);
    assert!(
        blocked
            .reason_codes
            .contains(&WorkReadinessReason::ParentDisallowsExecution)
    );

    let root_current = store.get_work_item(root.work_id).expect("completed root");
    let premature_reopen = store.reopen_work(
        &ReopenWorkRequest {
            work_id: root.work_id,
            expected_work_revision: root_current.revision,
            reason: "unfinished descendants must be resolved first".into(),
            actor: actor("human"),
            idempotency_key: "premature-root-reopen".into(),
            reopened_at: at(12),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(premature_reopen, Err(StoreError::InvalidWork(_))));

    for (item, key, second) in [
        (&live_optional, "dispose-live-optional", 13),
        (&idle_optional, "dispose-idle-optional", 14),
    ] {
        store
            .dispose_work(
                &DisposeWorkRequest {
                    work_id: item.work_id,
                    expected_work_revision: item.revision,
                    disposition: WorkDisposition::Cancelled,
                    replacement_id: None,
                    reason: "retire optional work omitted by the completed root".into(),
                    actor: actor("human"),
                    idempotency_key: key.into(),
                    disposed_at: at(second),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("dispose optional descendant");
    }
    let reopened = store
        .reopen_work(
            &ReopenWorkRequest {
                work_id: root.work_id,
                expected_work_revision: root_current.revision,
                reason: "start a clean root generation".into(),
                actor: actor("human"),
                idempotency_key: "reopen-clean-root".into(),
                reopened_at: at(15),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("reopen clean root");
    let old_optional_run =
        load_work_run(&store.connection, optional_claim.run_id).expect("old optional run");
    assert_ne!(
        old_optional_run.root_execution_id,
        reopened.root_execution_id
    );
    assert_eq!(old_optional_run.state, WorkRunState::Cancelled);
    assert_eq!(
        store
            .get_work_item(live_optional.work_id)
            .expect("disposed optional")
            .lifecycle,
        WorkLifecycle::Cancelled
    );
    let final_report = store.verify_all().expect("integrity report");
    assert!(final_report.is_healthy(), "{final_report:?}");
}

#[test]
fn completion_refuses_open_obligations_then_seals_the_exact_terminal_basis() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("engram.sqlite3");
    let mut store = SqliteStore::open(&database).expect("store");
    let work = store
        .create_work(
            &root_request(
                "project-completion-obligations",
                "create-completion-obligation-work",
                1,
            ),
            &DevelopmentNoopRedactor,
        )
        .expect("create local work");
    let claim = claim(
        &mut store,
        &work,
        "runner",
        "claim-completion-obligation-work",
        2,
        300,
    );
    let run = load_work_run(&store.connection, claim.run_id).expect("claimed run");
    let binding = ControlWorkBinding {
        root_execution_id: run.root_execution_id,
        work_id: work.work_id,
        run_id: run.run_id,
        work_revision: claim.accepted_work_revision,
        claim_id: claim.claim_id,
        claim_fence: claim.fence,
    };
    let mut run_actor = actor("runner");
    run_actor.run_id = Some(run.run_id.0.to_string());
    let source_basis = ExecutionSourceBasis {
        workspace_id: "workspace-completion".into(),
        source_revision: "revision-after-mutation".into(),
    };
    let mutation = ExecutionObservation {
        schema_version: SCHEMA_VERSION,
        project_id: work.project_id.clone(),
        binding: binding.clone(),
        session_id: SessionId("runner".into()),
        grant_id: "completion-obligation-grant".into(),
        observation_id: "completion-source-mutation".into(),
        action_fingerprint: ObjectHash::from_canonical_bytes(b"write src/lib.rs"),
        effect: EffectClass::MutateLocal,
        outcome: ExecutionOutcome::Succeeded,
        source_changed: true,
        obligation_rule_set: builtin_rule_set_hash(),
        source_basis: Some(source_basis.clone()),
        observed_at: Some(at(3)),
        actor: run_actor.clone(),
        recorded_at: at(3),
    };
    {
        let transaction = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("mutation transaction");
        append_control_execution_observation_on(&transaction, &mutation)
            .expect("append mutation and obligation");
        transaction.commit().expect("commit mutation");
    }
    let opened = store
        .work_run_obligations(run.run_id)
        .expect("open obligation");
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0].state, WorkObligationState::Open);

    let generic_evidence = evidence(
        &mut store,
        &work,
        &claim,
        "runner",
        "completion-generic-evidence",
        4,
    );
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "completion-before-verification",
        5,
        std::slice::from_ref(&generic_evidence),
    );
    let refused = complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &generic_evidence,
        "completion-open-obligation",
        6,
    );
    let Err(StoreError::OpenWorkObligations {
        work: refused_work,
        obligations,
        omitted_count,
    }) = refused
    else {
        panic!("completion must return the typed open-obligation refusal");
    };
    assert_eq!(refused_work, work.work_id);
    assert_eq!(omitted_count, 0);
    assert_eq!(obligations.len(), 1);
    assert_eq!(
        obligations[0].obligation_id,
        opened[0].obligation.obligation_id
    );
    assert_eq!(obligations[0].definition, opened[0].definition_hash);
    assert_eq!(obligations[0].required_check, VerificationKind::Test);

    let verification_observation = ExecutionObservation {
        schema_version: SCHEMA_VERSION,
        project_id: work.project_id.clone(),
        binding,
        session_id: SessionId("runner".into()),
        grant_id: "completion-obligation-grant".into(),
        observation_id: "completion-verification".into(),
        action_fingerprint: ObjectHash::from_canonical_bytes(b"cargo test --workspace"),
        effect: EffectClass::Observe,
        outcome: ExecutionOutcome::Succeeded,
        source_changed: false,
        obligation_rule_set: builtin_rule_set_hash(),
        source_basis: Some(source_basis.clone()),
        observed_at: Some(at(7)),
        actor: run_actor.clone(),
        recorded_at: at(7),
    };
    let environment_components = EnvironmentComponents {
        toolchain: "rustc-1.89.0".into(),
        sandbox: Some("completion-sandbox-v1".into()),
        workspace_id: source_basis.workspace_id.clone(),
        capability_map_revision: 1,
    };
    let environment_fingerprint = CanonicalObject::freeze(&environment_components)
        .expect("freeze completion environment components")
        .hash()
        .clone();
    let (verification_hash, environment_hash) = {
        let transaction = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("verification transaction");
        let producer =
            append_control_execution_observation_on(&transaction, &verification_observation)
                .expect("append verification producer");
        let environment_hash = append_control_environment_evidence_on(
            &transaction,
            &EnvironmentEvidence {
                schema_version: SCHEMA_VERSION,
                project_id: work.project_id.clone(),
                binding: verification_observation.binding.clone(),
                session_id: SessionId("runner".into()),
                source_basis: source_basis.clone(),
                environment_fingerprint,
                components: Some(environment_components.clone()),
                observed_at: at(7),
                actor: run_actor.clone(),
                recorded_at: at(7),
            },
        )
        .expect("append completion environment evidence");
        let hash = append_control_verification_evidence_on(
            &transaction,
            &VerificationEvidence {
                schema_version: SCHEMA_VERSION,
                project_id: work.project_id.clone(),
                binding: verification_observation.binding.clone(),
                session_id: SessionId("runner".into()),
                producer_observation: producer,
                source_basis,
                environment: Some(environment_hash.clone()),
                check_kind: VerificationKind::Test,
                check_fingerprint: verification_observation.action_fingerprint.clone(),
                result: VerificationResult::Passed,
                completed_at: at(7),
                summary: "host observed tests on the latest source basis".into(),
                refs: vec!["command:cargo-test-workspace".into()],
                actor: run_actor,
                recorded_at: at(7),
            },
        )
        .expect("append matching verification evidence");
        transaction.commit().expect("commit verification");
        (hash, environment_hash)
    };
    let terminal = store
        .work_run_obligations(run.run_id)
        .expect("terminal obligation");
    assert_eq!(terminal.len(), 1);
    assert_eq!(terminal[0].state, WorkObligationState::Satisfied);
    assert!(matches!(
        terminal[0]
            .resolution
            .as_ref()
            .map(|resolution| &resolution.resolution),
        Some(WorkObligationResolution::Satisfied { evidence, .. })
            if evidence == &verification_hash
    ));

    let all_evidence = store
        .work_run_evidence(run.run_id)
        .expect("all completion evidence");
    assert_eq!(all_evidence.len(), 3);
    assert!(all_evidence.contains(&generic_evidence));
    assert!(all_evidence.contains(&verification_hash));
    assert!(all_evidence.contains(&environment_hash));
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "completion-after-verification",
        8,
        &all_evidence,
    );
    let seal = complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &generic_evidence,
        "completion-after-obligation",
        9,
    )
    .expect("complete after terminal obligation and acknowledging checkpoint");
    assert_eq!(seal.schema_version, crate::schema::SCHEMA_VERSION);
    assert_eq!(
        seal.obligation_schema_version,
        crate::schema::COMPLETION_OBLIGATION_SCHEMA_VERSION
    );
    assert_eq!(
        seal.obligations,
        vec![CompletionObligationBinding {
            obligation_id: terminal[0].obligation.obligation_id,
            definition: terminal[0].definition_hash.clone(),
            resolution: terminal[0]
                .resolution_hash
                .clone()
                .expect("terminal resolution hash"),
        }]
    );
    assert_eq!(
        seal.environment_schema_version,
        crate::schema::COMPLETION_ENVIRONMENT_SCHEMA_VERSION
    );
    assert_eq!(seal.environment, vec![environment_hash.clone()]);
    validate_completion_seal_environment_basis_on(&store.connection, &seal)
        .expect("reconstruct exact completion environment basis");
    let mut forged_environment_basis = seal.clone();
    forged_environment_basis.environment.clear();
    assert!(
        validate_completion_seal_environment_basis_on(
            &store.connection,
            &forged_environment_basis,
        )
        .is_err(),
        "completion accepted a seal that omitted environment evidence"
    );
    validate_completion_seal_obligation_basis_on(&store.connection, &seal)
        .expect("reconstruct exact completion basis");
    let report = store.verify_all().expect("integrity report");
    assert!(report.is_healthy(), "{report:?}");
    let seal_hash = CanonicalObject::freeze(&seal)
        .expect("freeze sealed obligation basis")
        .hash()
        .clone();
    let mut forged_seal = seal.clone();
    forged_seal.obligations.clear();
    store
        .connection
        .execute_batch("SAVEPOINT corrupt_completion_obligations")
        .expect("start completion-seal corruption fixture");
    store
        .connection
        .execute(
            "UPDATE work_completion_seals SET seal_json = ?2 WHERE seal_hash = ?1",
            params![
                seal_hash.as_str(),
                serde_json::to_vec(&forged_seal).expect("forged seal JSON")
            ],
        )
        .expect("corrupt completion obligation projection");
    let corrupt_report = store.verify_all().expect("corrupt integrity report");
    assert!(
        corrupt_report
            .invalid_work_records
            .iter()
            .any(|record| record.contains("completion_seal")),
        "{corrupt_report:?}"
    );
    store
        .connection
        .execute_batch(
            "ROLLBACK TO corrupt_completion_obligations; RELEASE corrupt_completion_obligations",
        )
        .expect("restore completion-seal projection");
    assert!(
        store
            .verify_all()
            .expect("restored integrity report")
            .is_healthy()
    );
}

#[test]
fn completion_refuses_more_than_the_bounded_environment_basis() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let work = store
        .create_work(
            &root_request(
                "project-bounded-environment",
                "create-bounded-environment-work",
                1,
            ),
            &DevelopmentNoopRedactor,
        )
        .expect("create local work");
    let claim = claim(
        &mut store,
        &work,
        "runner",
        "claim-bounded-environment-work",
        2,
        300,
    );
    let run = load_work_run(&store.connection, claim.run_id).expect("claimed run");
    let binding = ControlWorkBinding {
        root_execution_id: run.root_execution_id,
        work_id: work.work_id,
        run_id: run.run_id,
        work_revision: claim.accepted_work_revision,
        claim_id: claim.claim_id,
        claim_fence: claim.fence,
    };
    let mut run_actor = actor("runner");
    run_actor.run_id = Some(run.run_id.0.to_string());
    let environment_hashes = {
        let transaction = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("environment transaction");
        let mut hashes = Vec::new();
        for index in 0..=MAX_COMPLETION_ENVIRONMENT_EVIDENCE {
            let components = EnvironmentComponents {
                toolchain: format!("toolchain-{index}"),
                sandbox: Some("bounded-environment-sandbox".into()),
                workspace_id: "workspace-bounded-environment".into(),
                capability_map_revision: 1,
            };
            let environment_fingerprint = CanonicalObject::freeze(&components)
                .expect("freeze bounded environment components")
                .hash()
                .clone();
            hashes.push(
                append_control_environment_evidence_on(
                    &transaction,
                    &EnvironmentEvidence {
                        schema_version: SCHEMA_VERSION,
                        project_id: work.project_id.clone(),
                        binding: binding.clone(),
                        session_id: SessionId("runner".into()),
                        source_basis: ExecutionSourceBasis {
                            workspace_id: components.workspace_id.clone(),
                            source_revision: "bounded-environment-revision".into(),
                        },
                        environment_fingerprint,
                        components: Some(components),
                        observed_at: at(3),
                        actor: run_actor.clone(),
                        recorded_at: at(3),
                    },
                )
                .expect("append bounded environment evidence"),
            );
        }
        transaction.commit().expect("commit environment evidence");
        hashes
    };
    assert_eq!(
        environment_hashes.len(),
        MAX_COMPLETION_ENVIRONMENT_EVIDENCE + 1
    );
    let generic_evidence = evidence(
        &mut store,
        &work,
        &claim,
        "runner",
        "bounded-environment-generic-evidence",
        4,
    );
    let mut acknowledged = environment_hashes;
    acknowledged.push(generic_evidence.clone());
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "bounded-environment-checkpoint",
        5,
        &acknowledged,
    );
    let result = complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &generic_evidence,
        "bounded-environment-completion",
        6,
    );
    assert!(
        matches!(
            result,
            Err(StoreError::WorkCompletionRefused { ref reason, .. })
                if reason.contains("maximum 64")
        ),
        "completion did not refuse an oversized environment basis: {result:?}"
    );
}

#[test]
fn open_completion_obligation_refusal_is_bounded_and_counts_omissions() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let work = store
        .create_work(
            &root_request(
                "project-bounded-obligations",
                "create-bounded-obligation-work",
                1,
            ),
            &DevelopmentNoopRedactor,
        )
        .expect("create local work");
    let claim = claim(
        &mut store,
        &work,
        "runner",
        "claim-bounded-obligation-work",
        2,
        300,
    );
    let run = load_work_run(&store.connection, claim.run_id).expect("claimed run");
    let binding = ControlWorkBinding {
        root_execution_id: run.root_execution_id,
        work_id: work.work_id,
        run_id: run.run_id,
        work_revision: claim.accepted_work_revision,
        claim_id: claim.claim_id,
        claim_fence: claim.fence,
    };
    let mut run_actor = actor("runner");
    run_actor.run_id = Some(run.run_id.0.to_string());
    let transaction = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("bounded-obligation transaction");
    for index in 0..=MAX_OPEN_COMPLETION_OBLIGATIONS {
        append_control_execution_observation_on(
            &transaction,
            &ExecutionObservation {
                schema_version: SCHEMA_VERSION,
                project_id: work.project_id.clone(),
                binding: binding.clone(),
                session_id: SessionId("runner".into()),
                grant_id: "bounded-obligation-grant".into(),
                observation_id: format!("bounded-source-mutation-{index}"),
                action_fingerprint: ObjectHash::from_canonical_bytes(
                    format!("write source {index}").as_bytes(),
                ),
                effect: EffectClass::MutateLocal,
                outcome: ExecutionOutcome::Succeeded,
                source_changed: true,
                obligation_rule_set: builtin_rule_set_hash(),
                source_basis: Some(ExecutionSourceBasis {
                    workspace_id: "workspace-bounded".into(),
                    source_revision: format!("revision-{index}"),
                }),
                observed_at: Some(at(3)),
                actor: run_actor.clone(),
                recorded_at: at(3),
            },
        )
        .expect("append bounded mutation obligation");
    }
    transaction.commit().expect("commit bounded obligations");
    let cut = FeedPosition {
        feed: FeedId::RunExecution(run.run_id),
        position: feed_head(&store.connection, &FeedId::RunExecution(run.run_id))
            .expect("run head"),
    };
    let Err(StoreError::OpenWorkObligations {
        work: refused_work,
        obligations,
        omitted_count,
    }) = completion_obligation_basis_on(&store.connection, work.work_id, run.run_id, &cut)
    else {
        panic!("the exact cut must refuse its open obligations");
    };
    assert_eq!(refused_work, work.work_id);
    assert_eq!(obligations.len(), MAX_OPEN_COMPLETION_OBLIGATIONS);
    assert_eq!(omitted_count, 1);
    assert!(
        obligations
            .iter()
            .all(|obligation| obligation.required_check == VerificationKind::Test)
    );
    assert!(obligations.windows(2).all(|window| {
        window[0].obligation_id.0.as_bytes() < window[1].obligation_id.0.as_bytes()
    }));
}

#[test]
fn ambient_completion_recomputes_a_typed_open_obligation_result() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("project-protocol-obligations".into());
    let session = SessionId("runner".into());
    let (work, expected_obligation, binding, run_actor, source_basis) = {
        let mut store = SqliteStore::open(&database).expect("store");
        let work = store
            .create_work(
                &root_request(&project.0, "create-protocol-obligation-work", 1),
                &DevelopmentNoopRedactor,
            )
            .expect("create local work");
        store
            .focus_work_session(&project, &session, work.work_id, at(2))
            .expect("focus work session");
        let claim = claim(
            &mut store,
            &work,
            &session.0,
            "claim-protocol-obligation-work",
            2,
            300,
        );
        let run = load_work_run(&store.connection, claim.run_id).expect("claimed run");
        let binding = ControlWorkBinding {
            root_execution_id: run.root_execution_id,
            work_id: work.work_id,
            run_id: run.run_id,
            work_revision: claim.accepted_work_revision,
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
        };
        let mut run_actor = actor(&session.0);
        run_actor.run_id = Some(run.run_id.0.to_string());
        let transaction = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("protocol-obligation transaction");
        let source_basis = ExecutionSourceBasis {
            workspace_id: "workspace-protocol".into(),
            source_revision: "revision-protocol".into(),
        };
        append_control_execution_observation_on(
            &transaction,
            &ExecutionObservation {
                schema_version: SCHEMA_VERSION,
                project_id: project.clone(),
                binding: binding.clone(),
                session_id: session.clone(),
                grant_id: "protocol-obligation-grant".into(),
                observation_id: "protocol-source-mutation".into(),
                action_fingerprint: ObjectHash::from_canonical_bytes(b"write protocol source"),
                effect: EffectClass::MutateLocal,
                outcome: ExecutionOutcome::Succeeded,
                source_changed: true,
                obligation_rule_set: builtin_rule_set_hash(),
                source_basis: Some(source_basis.clone()),
                observed_at: Some(at(3)),
                actor: run_actor.clone(),
                recorded_at: at(3),
            },
        )
        .expect("append protocol mutation obligation");
        transaction.commit().expect("commit protocol obligation");
        let obligation = store
            .work_run_obligations(run.run_id)
            .expect("protocol obligation")
            .pop()
            .expect("one protocol obligation");
        evidence(
            &mut store,
            &work,
            &claim,
            &session.0,
            "protocol-completion-evidence",
            4,
        );
        (work, obligation, binding, run_actor, source_basis)
    };
    let service = LocalWorkService::new(
        database.clone(),
        project,
        "runner".into(),
        session,
        Some("obligation-protocol-test".into()),
    );
    let input = WorkCompleteInput {
        capture: Some(WorkCompletionCaptureInput {
            summary: "capture the exact completion evidence cut".into(),
            refs: Vec::new(),
        }),
        evidence: Vec::new(),
        acceptance: Some(vec![WorkAcceptanceInput {
            criterion: None,
            satisfied: true,
            evidence: Vec::new(),
            note: "completion evidence is present".into(),
        }]),
        note: None,
        idempotency_key: "typed-open-obligation-result".into(),
    };
    let first = service
        .work_complete(input.clone(), at(6))
        .expect("open obligation is a typed result");
    let WorkCompleteResult::Refused(refusal) = &first else {
        panic!("open obligation must not complete the work");
    };
    assert_eq!(refusal.code, "open_work_obligations");
    assert_eq!(refusal.work_id, work.work_id);
    assert_eq!(refusal.obligation_page.items.len(), 1);
    assert_eq!(
        refusal.obligation_page.items[0].obligation_id,
        expected_obligation.obligation.obligation_id
    );
    assert_eq!(
        refusal.obligation_page.items[0].definition,
        expected_obligation.definition_hash
    );
    assert_eq!(
        refusal.obligation_page.items[0].requirement.check_kind,
        VerificationKind::Test
    );
    assert_eq!(refusal.obligation_page.omitted_count, 0);
    let recovery = &refusal.recovery;
    assert!(matches!(
        &recovery.cause,
        WorkCompletionRecoveryCause::OpenObligation {
            obligation_id,
            definition,
            required_check: VerificationKind::Test,
        } if *obligation_id == expected_obligation.obligation.obligation_id
            && *definition == expected_obligation.definition_hash
    ));
    assert_eq!(recovery.item.work_id, work.work_id);
    assert_eq!(recovery.item.title, work.title);
    assert!(
        recovery
            .command
            .starts_with(&format!("engram work done {}", work.short_ref))
    );
    assert_eq!(
        refusal.remedy,
        "record the matching host verification, then checkpoint_work acknowledging it, then complete; or request a host/operator waiver"
    );
    let run_id = binding.run_id;
    let head_before_replay = SqliteStore::open(&database)
        .expect("store before refusal replay")
        .work_feed_head(&FeedId::RunExecution(run_id))
        .expect("run feed before refusal replay");
    let replay = service
        .work_complete(input.clone(), at(7))
        .expect("typed refusal is recomputed from current state");
    assert_eq!(
        serde_json::to_value(replay).expect("replay JSON"),
        serde_json::to_value(first).expect("first JSON")
    );
    assert_eq!(
        SqliteStore::open(&database)
            .expect("store after refusal replay")
            .work_feed_head(&FeedId::RunExecution(run_id))
            .expect("run feed after refusal replay"),
        head_before_replay,
        "an unchanged refusal reuses the exact current checkpoint"
    );

    let foreign_checkpoint_head = {
        let mut store = SqliteStore::open(&database).expect("foreign checkpoint store");
        let mut evidence = store
            .work_run_evidence(run_id)
            .expect("current run evidence");
        evidence.sort();
        store
            .checkpoint_work(
                &CheckpointWorkRequest {
                    work_id: work.work_id,
                    run_id,
                    expected_work_revision: work.revision,
                    holder: SessionId("runner".into()),
                    claim_id: binding.claim_id,
                    claim_fence: binding.claim_fence,
                    summary: "holder checkpoint outside completion".into(),
                    evidence: Some(evidence),
                    actor: run_actor.clone(),
                    idempotency_key: "foreign-holder-checkpoint".into(),
                    checkpointed_at: at(8),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("holder writes an independent checkpoint");
        store
            .work_feed_head(&FeedId::RunExecution(run_id))
            .expect("feed head after independent checkpoint")
    };
    assert!(matches!(
        service
            .work_complete(input.clone(), at(9))
            .expect("completion owns a checkpoint after the independent one"),
        WorkCompleteResult::Refused(_)
    ));
    let completion_checkpoint_head = SqliteStore::open(&database)
        .expect("store after completion checkpoint")
        .work_feed_head(&FeedId::RunExecution(run_id))
        .expect("feed head after completion checkpoint");
    assert!(completion_checkpoint_head > foreign_checkpoint_head);
    assert!(matches!(
        service
            .work_complete(input.clone(), at(10))
            .expect("unchanged retry reuses its own checkpoint"),
        WorkCompleteResult::Refused(_)
    ));
    let refused_store = SqliteStore::open(&database).expect("store after stable refusal");
    assert_eq!(
        refused_store
            .work_feed_head(&FeedId::RunExecution(run_id))
            .expect("stable refusal feed head"),
        completion_checkpoint_head,
        "a foreign checkpoint is replaced once, then the completion-owned checkpoint converges"
    );
    let pending_attempts: i64 = refused_store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM work_protocol_attempts
             WHERE operation = 'work_complete' AND result_hash IS NULL",
            [],
            |row| row.get(0),
        )
        .expect("pending completion attempt count");
    assert_eq!(
        pending_attempts, 1,
        "refusals retain one pending target and caller-key binding"
    );
    drop(refused_store);

    {
        let store = SqliteStore::open(&database).expect("verification store");
        let transaction = store
            .connection
            .unchecked_transaction()
            .expect("verification transaction");
        let producer = append_control_execution_observation_on(
            &transaction,
            &ExecutionObservation {
                schema_version: SCHEMA_VERSION,
                project_id: work.project_id.clone(),
                binding: binding.clone(),
                session_id: SessionId("runner".into()),
                grant_id: "protocol-obligation-grant".into(),
                observation_id: "protocol-obligation-verification".into(),
                action_fingerprint: ObjectHash::from_canonical_bytes(
                    b"cargo test protocol obligation",
                ),
                effect: EffectClass::Observe,
                outcome: ExecutionOutcome::Succeeded,
                source_changed: false,
                obligation_rule_set: builtin_rule_set_hash(),
                source_basis: Some(source_basis.clone()),
                observed_at: Some(at(11)),
                actor: run_actor.clone(),
                recorded_at: at(11),
            },
        )
        .expect("append verification producer");
        append_control_verification_evidence_on(
            &transaction,
            &VerificationEvidence {
                schema_version: SCHEMA_VERSION,
                project_id: work.project_id.clone(),
                binding: binding.clone(),
                session_id: SessionId("runner".into()),
                producer_observation: producer,
                source_basis,
                environment: None,
                check_kind: VerificationKind::Test,
                check_fingerprint: ObjectHash::from_canonical_bytes(
                    b"cargo test protocol obligation",
                ),
                result: VerificationResult::Passed,
                completed_at: at(11),
                summary: "protocol obligation verification passed".into(),
                refs: Vec::new(),
                actor: run_actor,
                recorded_at: at(11),
            },
        )
        .expect("append matching host verification");
        transaction.commit().expect("commit host verification");
    }
    assert!(matches!(
        service
            .work_complete(input, at(12))
            .expect("retry after host verification"),
        WorkCompleteResult::Completed(_)
    ));
    let stored = SqliteStore::open(&database).expect("inspect store");
    assert_eq!(
        stored
            .get_work_item(work.work_id)
            .expect("completed work remains readable")
            .lifecycle,
        WorkLifecycle::Completed
    );
    let report = stored.verify_all().expect("typed refusal integrity report");
    assert!(report.is_healthy(), "{report:?}");
}

#[test]
fn basisless_mutation_is_waiver_only_until_a_later_verified_source_state() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("engram.sqlite3");
    let mut store = SqliteStore::open(&database).expect("store");
    let work = store
        .create_work(
            &root_request("project-obligations", "create-obligation-work", 1),
            &DevelopmentNoopRedactor,
        )
        .expect("create local work");
    let claim = claim(&mut store, &work, "runner", "claim-obligation-work", 2, 120);
    let run = load_work_run(&store.connection, claim.run_id).expect("claimed run");
    let binding = ControlWorkBinding {
        root_execution_id: run.root_execution_id,
        work_id: work.work_id,
        run_id: run.run_id,
        work_revision: claim.accepted_work_revision,
        claim_id: claim.claim_id,
        claim_fence: claim.fence,
    };
    let mut run_actor = actor("runner");
    run_actor.run_id = Some(run.run_id.0.to_string());
    let observation = |id: &str,
                       source_changed: bool,
                       basis: Option<ExecutionSourceBasis>,
                       action: &str,
                       at_time: DateTime<Utc>| ExecutionObservation {
        schema_version: SCHEMA_VERSION,
        project_id: work.project_id.clone(),
        binding: binding.clone(),
        session_id: SessionId("runner".into()),
        grant_id: "direct-test-grant".into(),
        observation_id: id.into(),
        action_fingerprint: ObjectHash::from_canonical_bytes(action.as_bytes()),
        effect: if source_changed {
            EffectClass::MutateLocal
        } else {
            EffectClass::Observe
        },
        outcome: ExecutionOutcome::Succeeded,
        source_changed,
        obligation_rule_set: builtin_rule_set_hash(),
        source_basis: basis,
        observed_at: Some(at_time),
        actor: run_actor.clone(),
        recorded_at: at_time,
    };

    let basisless = observation(
        "basisless-mutation",
        true,
        None,
        "write without basis",
        at(3),
    );
    let basisless_hash = {
        let transaction = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("basisless transaction");
        let hash = append_control_execution_observation_on(&transaction, &basisless)
            .expect("append basisless mutation");
        transaction.commit().expect("commit basisless mutation");
        hash
    };
    let opened = store
        .work_run_obligations(run.run_id)
        .expect("basisless open obligation");
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0].state, WorkObligationState::Open);
    assert_eq!(opened[0].obligation.triggering_observation, basisless_hash);

    let first_test = observation(
        "test-after-basisless",
        false,
        Some(ExecutionSourceBasis {
            workspace_id: "workspace-a".into(),
            source_revision: "revision-a".into(),
        }),
        "cargo test",
        at(4),
    );
    {
        let transaction = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("first test transaction");
        let producer = append_control_execution_observation_on(&transaction, &first_test)
            .expect("append first test producer");
        append_control_verification_evidence_on(
            &transaction,
            &VerificationEvidence {
                schema_version: SCHEMA_VERSION,
                project_id: work.project_id.clone(),
                binding: binding.clone(),
                session_id: SessionId("runner".into()),
                producer_observation: producer,
                source_basis: first_test.source_basis.clone().expect("first test basis"),
                environment: None,
                check_kind: VerificationKind::Test,
                check_fingerprint: first_test.action_fingerprint.clone(),
                result: VerificationResult::Passed,
                completed_at: at(4),
                summary: "tests passed after a basisless mutation".into(),
                refs: Vec::new(),
                actor: run_actor.clone(),
                recorded_at: at(4),
            },
        )
        .expect("append first test evidence");
        transaction.commit().expect("commit first test");
    }
    assert_eq!(
        store
            .work_run_obligations(run.run_id)
            .expect("still-open obligation")[0]
            .state,
        WorkObligationState::Open
    );

    let based_mutation = observation(
        "based-mutation",
        true,
        Some(ExecutionSourceBasis {
            workspace_id: "workspace-b".into(),
            source_revision: "revision-b".into(),
        }),
        "write with basis",
        at(5),
    );
    let final_test = observation(
        "test-after-based-mutation",
        false,
        based_mutation.source_basis.clone(),
        "cargo test --workspace",
        at(6),
    );
    {
        let transaction = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("verified mutation transaction");
        append_control_execution_observation_on(&transaction, &based_mutation)
            .expect("append based mutation");
        let producer = append_control_execution_observation_on(&transaction, &final_test)
            .expect("append final test producer");
        append_control_verification_evidence_on(
            &transaction,
            &VerificationEvidence {
                schema_version: SCHEMA_VERSION,
                project_id: work.project_id.clone(),
                binding: binding.clone(),
                session_id: SessionId("runner".into()),
                producer_observation: producer,
                source_basis: final_test.source_basis.clone().expect("final test basis"),
                environment: None,
                check_kind: VerificationKind::Test,
                check_fingerprint: final_test.action_fingerprint.clone(),
                result: VerificationResult::Passed,
                completed_at: at(6),
                summary: "tests passed on the latest full source state".into(),
                refs: Vec::new(),
                actor: run_actor.clone(),
                recorded_at: at(6),
            },
        )
        .expect("append final test evidence");
        transaction.commit().expect("commit verified mutation");
    }
    let satisfied = store
        .work_run_obligations(run.run_id)
        .expect("satisfied obligations");
    assert_eq!(satisfied.len(), 2);
    assert!(
        satisfied
            .iter()
            .all(|record| record.state == WorkObligationState::Satisfied)
    );
    let evaluated_cut = satisfied
        .iter()
        .find_map(|record| match &record.resolution.as_ref()?.resolution {
            WorkObligationResolution::Satisfied { evaluated_cut, .. } => {
                Some(evaluated_cut.clone())
            }
            WorkObligationResolution::Waived { .. } => None,
        })
        .expect("satisfaction evaluated cut");
    assert_eq!(
        store
            .open_work_obligations_at_cut(run.run_id, &evaluated_cut)
            .expect("derive obligations before terminal appends")
            .len(),
        2
    );

    let waiver_target = &satisfied[0];
    assert!(matches!(
        store.waive_work_obligation(
            &WaiveWorkObligationRequest {
                obligation_id: waiver_target.obligation.obligation_id,
                expected_definition: waiver_target.definition_hash.clone(),
                waived_by: "operator".into(),
                reason: "already terminal must not be waived".into(),
                actor: actor("operator"),
                idempotency_key: "waive-terminal-obligation".into(),
                waived_at: at(7),
            },
            &DevelopmentNoopRedactor,
        ),
        Err(StoreError::InvalidWork(message)) if message.contains("already terminal")
    ));
    let waiver_mutation = observation(
        "waiver-only-mutation",
        true,
        None,
        "write requiring operator waiver",
        at(7),
    );
    {
        let transaction = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("waiver mutation transaction");
        append_control_execution_observation_on(&transaction, &waiver_mutation)
            .expect("append waiver mutation");
        transaction.commit().expect("commit waiver mutation");
    }
    let waiver_target = store
        .work_run_obligations(run.run_id)
        .expect("open waiver target")
        .into_iter()
        .find(|record| record.state == WorkObligationState::Open)
        .expect("one open waiver target");
    let waiver_request = WaiveWorkObligationRequest {
        obligation_id: waiver_target.obligation.obligation_id,
        expected_definition: waiver_target.definition_hash.clone(),
        waived_by: "operator".into(),
        reason: "operator accepted the unverified final mutation".into(),
        actor: actor("operator"),
        idempotency_key: "waive-open-obligation".into(),
        waived_at: at(8),
    };
    let waived = store
        .waive_work_obligation(&waiver_request, &DevelopmentNoopRedactor)
        .expect("waive exact open obligation");
    assert!(matches!(
        waived.resolution,
        WorkObligationResolution::Waived { ref reason, .. }
            if reason == "operator accepted the unverified final mutation"
    ));
    let mut replay_request = waiver_request.clone();
    replay_request.waived_at = at(9);
    assert_eq!(
        store
            .waive_work_obligation(&replay_request, &DevelopmentNoopRedactor)
            .expect("replay obligation waiver after an uncertain response"),
        waived
    );
    let terminal = store
        .work_run_obligations(run.run_id)
        .expect("terminal obligations");
    assert_eq!(terminal.len(), 3);
    assert_eq!(
        terminal
            .iter()
            .find(|record| record.obligation.obligation_id == waiver_request.obligation_id)
            .expect("waived projection")
            .state,
        WorkObligationState::Waived
    );
    let terminal_cut =
        current_run_feed_cut_on(&store.connection, run.run_id).expect("terminal run-feed cut");
    assert!(
        store
            .open_work_obligations_at_cut(run.run_id, &terminal_cut)
            .expect("derive terminal obligation state")
            .is_empty()
    );
    let report = store.verify_all().expect("obligation integrity report");
    assert!(report.is_healthy(), "{report:?}");
    let target = terminal
        .iter()
        .find(|record| record.obligation.obligation_id == waiver_request.obligation_id)
        .expect("waived corruption target");
    let obligation_id = target.obligation.obligation_id.0.to_string();
    let definition = target.definition_hash.as_str();
    let resolution = target
        .resolution_hash
        .as_ref()
        .expect("waiver resolution")
        .as_str();
    let forged_uuid = uuid::Uuid::new_v4().to_string();
    let corruptions = [
        format!(
            "UPDATE work_run_obligations SET obligation_id = '{forged_uuid}' WHERE obligation_id = '{obligation_id}'"
        ),
        format!(
            "UPDATE work_run_obligations SET definition_hash = '{resolution}' WHERE obligation_id = '{obligation_id}'"
        ),
        format!(
            "UPDATE work_run_obligations SET project_id = 'forged-project' WHERE obligation_id = '{obligation_id}'"
        ),
        format!(
            "UPDATE work_run_obligations SET root_execution_id = '{forged_uuid}' WHERE obligation_id = '{obligation_id}'"
        ),
        format!(
            "UPDATE work_run_obligations SET root_id = '{forged_uuid}' WHERE obligation_id = '{obligation_id}'"
        ),
        format!(
            "UPDATE work_run_obligations SET work_id = '{forged_uuid}' WHERE obligation_id = '{obligation_id}'"
        ),
        format!(
            "UPDATE work_run_obligations SET run_id = '{forged_uuid}' WHERE obligation_id = '{obligation_id}'"
        ),
        format!(
            "UPDATE work_run_obligations SET work_revision = work_revision + 1 WHERE obligation_id = '{obligation_id}'"
        ),
        format!(
            "UPDATE work_run_obligations SET rule_id = 'forged-rule' WHERE obligation_id = '{obligation_id}'"
        ),
        format!(
            "UPDATE work_run_obligations SET rule_version = rule_version + 1 WHERE obligation_id = '{obligation_id}'"
        ),
        format!(
            "UPDATE work_run_obligations SET triggering_observation_hash = '{definition}' WHERE obligation_id = '{obligation_id}'"
        ),
        format!(
            "UPDATE work_run_obligations SET trigger_position = trigger_position + 1 WHERE obligation_id = '{obligation_id}'"
        ),
        format!(
            "UPDATE work_run_obligations SET check_kind = 'build' WHERE obligation_id = '{obligation_id}'"
        ),
        format!(
            "UPDATE work_run_obligations SET check_fingerprint = '{definition}' WHERE obligation_id = '{obligation_id}'"
        ),
        format!(
            "UPDATE work_run_obligations SET state = 'satisfied' WHERE obligation_id = '{obligation_id}'"
        ),
        format!(
            "UPDATE work_run_obligations SET resolution_hash = '{definition}' WHERE obligation_id = '{obligation_id}'"
        ),
        format!(
            "UPDATE work_run_obligations SET resolution_kind = 'satisfied' WHERE obligation_id = '{obligation_id}'"
        ),
        format!(
            "UPDATE work_run_obligations SET evidence_hash = '{definition}' WHERE obligation_id = '{obligation_id}'"
        ),
        format!(
            "UPDATE work_run_obligations SET opened_at_ms = opened_at_ms + 1 WHERE obligation_id = '{obligation_id}'"
        ),
        format!(
            "UPDATE work_run_obligations SET resolved_at_ms = resolved_at_ms + 1 WHERE obligation_id = '{obligation_id}'"
        ),
    ];
    store
        .connection
        .execute_batch("PRAGMA foreign_keys = OFF")
        .expect("disable foreign keys for corruption fixtures");
    for (index, update) in corruptions.iter().enumerate() {
        store
            .connection
            .execute_batch("SAVEPOINT corrupt_obligation")
            .expect("start obligation corruption savepoint");
        store
            .connection
            .execute(update, [])
            .unwrap_or_else(|error| panic!("apply obligation corruption {index}: {error}"));
        assert!(
            store.work_run_obligations(run.run_id).is_err(),
            "obligation corruption {index} was accepted by lifecycle reads"
        );
        let corrupt_report = store
            .verify_all()
            .unwrap_or_else(|error| panic!("verify obligation corruption {index}: {error}"));
        assert!(
            !corrupt_report.invalid_work_records.is_empty(),
            "obligation corruption {index} was not reported: {corrupt_report:?}"
        );
        store
            .connection
            .execute_batch("ROLLBACK TO corrupt_obligation; RELEASE corrupt_obligation")
            .expect("restore obligation projection");
    }
    store
        .connection
        .execute_batch("PRAGMA foreign_keys = ON")
        .expect("restore foreign key enforcement");
    let final_report = store
        .verify_all()
        .expect("final obligation integrity report");
    assert!(final_report.is_healthy(), "{final_report:?}");
}

#[test]
fn bound_host_obligation_waiver_is_typed_human_attributed_and_replayable() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("engram.sqlite3");
    let mut store = SqliteStore::open(&database).expect("store");
    let work = store
        .create_work(
            &root_request("project-host-waiver", "create-host-waiver-work", 1),
            &DevelopmentNoopRedactor,
        )
        .expect("create local work");
    let claim = claim(
        &mut store,
        &work,
        "runner",
        "claim-host-waiver-work",
        2,
        120,
    );
    let run = load_work_run(&store.connection, claim.run_id).expect("claimed run");
    let binding = ControlWorkBinding {
        root_execution_id: run.root_execution_id,
        work_id: work.work_id,
        run_id: run.run_id,
        work_revision: claim.accepted_work_revision,
        claim_id: claim.claim_id,
        claim_fence: claim.fence,
    };
    let mut host_actor = actor("runner");
    host_actor.run_id = Some(run.run_id.0.to_string());
    let observation = ExecutionObservation {
        schema_version: SCHEMA_VERSION,
        project_id: work.project_id.clone(),
        binding: binding.clone(),
        session_id: SessionId("runner".into()),
        grant_id: "host-waiver-test-turn".into(),
        observation_id: "host-waiver-mutation".into(),
        action_fingerprint: ObjectHash::from_canonical_bytes(b"host waiver mutation"),
        effect: EffectClass::MutateLocal,
        outcome: ExecutionOutcome::Succeeded,
        source_changed: true,
        obligation_rule_set: builtin_rule_set_hash(),
        source_basis: None,
        observed_at: Some(at(3)),
        actor: host_actor.clone(),
        recorded_at: at(3),
    };
    {
        let transaction = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("observation transaction");
        append_control_execution_observation_on(&transaction, &observation)
            .expect("append mutation and obligation");
        transaction.commit().expect("commit mutation");
    }
    let open = store
        .work_run_obligations(run.run_id)
        .expect("open obligation")
        .pop()
        .expect("one obligation");
    let unbound_session = SessionId("unbound-runner".into());
    let unbound_connection = store
        .resume_control_connection(&unbound_session, at(4))
        .expect("resume unbound host connection");
    let unbound_actor = actor("unbound-runner");
    let unbound = store
        .bind_control_session(
            &work.project_id,
            "local-work:unbound-host-waiver",
            "Unbound obligation waiver attempt",
            &unbound_session,
            &unbound_connection,
            &unbound_actor,
            ControlAssurance::TurnGated,
            &[EffectClass::Observe],
            1,
            "bind-unbound-host-waiver",
            at(4),
        )
        .expect("bind unbound host session");
    let unbound_refusal = store
        .waive_bound_work_obligation(
            &work.project_id,
            &unbound_session,
            &unbound_connection,
            &unbound.routing_token,
            open.obligation.obligation_id,
            &open.definition_hash,
            "human-operator",
            "reviewed the exact obligation",
            &unbound_actor,
            "unbound-host-waiver",
            at(5),
            &DevelopmentNoopRedactor,
        )
        .expect("unbound refusal is typed");
    assert!(matches!(
        unbound_refusal,
        WorkObligationWaiverDecision::Refused {
            code: WorkObligationWaiverRefusalCode::WaiverNotAdmitted,
            ..
        }
    ));
    let session_id = SessionId("runner".into());
    let connection_token = store
        .resume_control_connection(&session_id, at(4))
        .expect("resume host connection");
    let bound = store
        .bind_control_session_with_work(
            &work.project_id,
            "local-work:host-waiver",
            "Host-authorized obligation waiver",
            &session_id,
            &connection_token,
            &host_actor,
            Some(&binding),
            ControlAssurance::TurnGated,
            &[EffectClass::Observe, EffectClass::MutateLocal],
            1,
            "bind-host-waiver",
            at(4),
        )
        .expect("bind host session");
    let wrong_definition = store
        .waive_bound_work_obligation(
            &work.project_id,
            &session_id,
            &connection_token,
            &bound.routing_token,
            open.obligation.obligation_id,
            &ObjectHash::from_canonical_bytes(b"wrong definition"),
            "human-operator",
            "reviewed the exact obligation",
            &host_actor,
            "host-waiver-wrong-definition",
            at(5),
            &DevelopmentNoopRedactor,
        )
        .expect("definition mismatch is typed");
    assert!(matches!(
        wrong_definition,
        WorkObligationWaiverDecision::Refused {
            code: WorkObligationWaiverRefusalCode::DefinitionChanged,
            ..
        }
    ));
    let waived = store
        .waive_bound_work_obligation(
            &work.project_id,
            &session_id,
            &connection_token,
            &bound.routing_token,
            open.obligation.obligation_id,
            &open.definition_hash,
            "human-operator",
            "reviewed the exact obligation",
            &host_actor,
            "host-waiver-success",
            at(6),
            &DevelopmentNoopRedactor,
        )
        .expect("waive obligation");
    let replay = store
        .waive_bound_work_obligation(
            &work.project_id,
            &session_id,
            &connection_token,
            &bound.routing_token,
            open.obligation.obligation_id,
            &open.definition_hash,
            "human-operator",
            "reviewed the exact obligation",
            &host_actor,
            "host-waiver-success",
            at(7),
            &DevelopmentNoopRedactor,
        )
        .expect("replay host waiver");
    assert_eq!(replay, waived);
    let WorkObligationWaiverDecision::Waived { receipt } = waived else {
        panic!("expected waived receipt");
    };
    assert_eq!(receipt.waived_by, "human-operator");
    let resolved = store
        .work_run_obligations(run.run_id)
        .expect("resolved obligation")
        .pop()
        .expect("one obligation");
    let event = resolved.resolution.expect("waiver resolution");
    assert_eq!(event.actor.actor_id, "runner");
    assert_eq!(event.actor.session_id, Some(session_id.clone()));
    assert!(matches!(
        event.resolution,
        WorkObligationResolution::Waived { waived_by, .. }
            if waived_by == "human-operator"
    ));
    let already_terminal = store
        .waive_bound_work_obligation(
            &work.project_id,
            &session_id,
            &connection_token,
            &bound.routing_token,
            open.obligation.obligation_id,
            &open.definition_hash,
            "human-operator",
            "reviewed the exact obligation",
            &host_actor,
            "host-waiver-terminal",
            at(8),
            &DevelopmentNoopRedactor,
        )
        .expect("terminal refusal is typed");
    assert!(matches!(
        already_terminal,
        WorkObligationWaiverDecision::Refused {
            code: WorkObligationWaiverRefusalCode::ObligationNotOpen,
            ..
        }
    ));
}
