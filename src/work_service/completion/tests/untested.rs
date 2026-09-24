//! Completion after an untested source change: the stock rule's waiver lies
//! past the final checkpoint, so replay keys an interrupted attempt from the
//! checkpoint rather than from the seal's cut.

use super::*;

/// Commits the core seal of a linked completion the way `work_complete`
/// does, then stops before the protocol attempt records its result.
fn commit_linked_completion_without_finishing(
    service: &LocalWorkService,
    input: &WorkCompleteInput,
    now: DateTime<Utc>,
) -> CompletionSeal {
    let mut store = service.store().expect("completion store");
    let basis = service
        .protocol_basis(&store, true, false, None, now)
        .expect("completion basis");
    let intent = service.protocol_intent(input);
    let raw_key = service
        .effective_idempotency_key(
            &input.idempotency_key,
            "work_complete",
            &basis,
            &intent,
            now,
        )
        .expect("completion key");
    store
        .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
            project_id: &service.project_id,
            session_id: &service.session_id,
            operation: "work_complete",
            idempotency_key: &raw_key,
            intent: &intent,
            basis: &basis,
            now,
        })
        .expect("pending completion attempt");
    let work = basis.focused_work.clone().expect("focused completion work");
    let claim = service
        .live_protocol_claim(&basis, &work, now)
        .expect("completion claim");
    let actor = service.actor("work_complete", "complete ambient local work");
    let evidence_basis =
        LocalWorkService::completion_evidence_basis(&store, &claim, &input.evidence)
            .expect("completion evidence basis");
    let acceptance = super::super::links::validated_acceptance(
        &store,
        &work,
        &claim,
        input,
        &actor,
        &evidence_basis,
    )
    .expect("linked acceptance");
    let prepared = service
        .prepare_completion_evidence(
            &mut store,
            CompletionEvidencePlan {
                work: &work,
                claim: &claim,
                capture: input.capture.as_ref(),
                evidence: evidence_basis,
                base_key: &raw_key,
                now,
            },
        )
        .expect("completion substeps");
    let scoped_key = service
        .core_operation_key("work_complete", &prepared.attempt_key, "complete_work")
        .expect("completion core key");
    match store
        .complete_work_for_protocol(
            &CompleteWorkRequest {
                work_id: work.work_id,
                run_id: claim.run_id,
                holder: service.session_id.clone(),
                expected_work_revision: work.revision,
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                evidence: prepared.evidence,
                acceptance,
                drain: CompletionDrainAttestation {
                    reconciled_action_outcomes: Vec::new(),
                    released_resource_leases: Vec::new(),
                },
                source_fingerprint: None,
                actor,
                idempotency_key: scoped_key,
                completed_at: now,
            },
            &DevelopmentNoopRedactor,
        )
        .expect("completion core commits")
    {
        CompleteWorkStorageResult::Completed(seal) => *seal,
        CompleteWorkStorageResult::Recovery(_) => {
            panic!("an untested change must not refuse completion")
        }
    }
}

#[test]
fn peers_read_a_completion_waiver_as_the_untested_change() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("untested-peer-delta".into());
    let service = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("untested-delta-session".into()),
        Some("protocol-test".into()),
    );
    let root = proposed_root(
        service
            .work_propose(root_input("Untested delta", "untested-delta-root"), at(0))
            .expect("root"),
    );
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(3_600),
                recovery_reason: None,
                idempotency_key: "untested-delta-claim".into(),
            },
            at(1),
        )
        .expect("claim");
    SqliteStore::open(&database)
        .expect("fixture store")
        .append_source_change_fixture(root.work_id, "untested-delta", at(2), "delta-revision");
    let WorkCompleteResult::Completed(receipt) = service
        .work_complete(
            completion_input("delivered untested", "untested-delta-completion"),
            at(3),
        )
        .expect("complete")
    else {
        panic!("an untested change must not refuse completion");
    };
    assert_eq!(receipt.obligation_page.untested_total, 1);

    let store = service.store().expect("store");
    let waiver = store
        .work_run_obligations(receipt.run_id)
        .expect("obligations")
        .pop()
        .and_then(|record| record.resolution_id)
        .expect("the completion waiver");
    let object: serde_json::Value = store
        .get(&waiver)
        .expect("load waiver")
        .expect("canonical waiver");
    let WorkChangeProjection::Visible(change) = agent_change_object(
        &store,
        &project,
        Some(root.work_id),
        "work_obligation_resolution",
        object,
        None,
    )
    .expect("peer projection") else {
        panic!("the untested change must be visible to peers");
    };
    assert_eq!(change.change_kind, "untested_source_change");
    assert_eq!(
        change.summary,
        "write-untested-delta (source revision delta-revision); waiver attributed to agent"
    );
}

#[test]
fn interrupted_no_capture_completion_with_links_replays_after_an_untested_change() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let service = LocalWorkService::new(
        database.clone(),
        ProjectId("interrupted-untested-links".into()),
        "agent".into(),
        SessionId("interrupted-untested-session".into()),
        Some("protocol-test".into()),
    );
    let root = proposed_root(
        service
            .work_propose(root_input("Replay untested", "untested-links-root"), at(0))
            .expect("root"),
    );
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(3_600),
                recovery_reason: None,
                idempotency_key: "untested-links-claim".into(),
            },
            at(1),
        )
        .expect("claim");
    SqliteStore::open(&database)
        .expect("fixture store")
        .append_source_change_fixture(
            root.work_id,
            "untested-links",
            at(2),
            "untested-links-revision",
        );
    let evidence: ObjectId = serde_json::from_value(
        service
            .work_update(
                WorkUpdateInput::Evidence {
                    summary: "the change is in place".into(),
                    refs: vec!["test:untested-links".into()],
                    attach: None,
                    idempotency_key: "untested-links-evidence".into(),
                },
                at(3),
            )
            .expect("evidence")
            .receipt
            .result,
    )
    .expect("evidence id");
    service
        .work_update(
            WorkUpdateInput::Checkpoint {
                summary: "acknowledge the completion evidence".into(),
                evidence: None,
                idempotency_key: "untested-links-checkpoint".into(),
            },
            at(4),
        )
        .expect("checkpoint");
    let input = WorkCompleteInput {
        links: vec![WorkCriterionLinkInput {
            criterion: 1,
            locator: evidence.as_str().to_owned(),
        }],
        link_basis: Some(root.revision),
        capture: None,
        ..completion_input("no capture", "untested-links-completion")
    };
    let seal = commit_linked_completion_without_finishing(&service, &input, at(5));
    let store = service.store().expect("sealed store");
    let checkpoint: crate::domain::WorkCheckpoint = store
        .get(seal.checkpoint.as_ref().expect("sealed checkpoint"))
        .expect("load checkpoint")
        .expect("canonical checkpoint");
    assert!(
        seal.completion_cut.position
            > crate::storage::checkpoint_run_feed_end(&checkpoint)
                .expect("checkpoint end")
                .position,
        "the untested-change waiver lies past the checkpoint"
    );
    drop(store);

    let replay = service
        .work_complete(input, at(6))
        .expect("the interrupted completion replays");
    let WorkCompleteResult::Completed(receipt) = replay else {
        panic!("an interrupted completion must replay its seal");
    };
    assert_eq!(receipt.run_id, seal.run_id);
    assert_eq!(receipt.obligation_page.untested_total, 1);
    assert!(
        service
            .store()
            .expect("store")
            .verify_all()
            .expect("integrity")
            .is_healthy()
    );
}
