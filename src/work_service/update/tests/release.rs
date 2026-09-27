use super::*;

#[test]
fn core_committed_release_recovery_keeps_the_recorded_waiver() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("committed-release".into());
    let session = SessionId("committed-release-session".into());
    let service = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        session.clone(),
        Some("protocol-test".into()),
    );
    let root = proposed_root(
        service
            .work_propose(
                root_input("Released before any work", "release-root"),
                at(0),
            )
            .expect("root"),
    );
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: Some(300),
                recovery_reason: None,
                idempotency_key: String::new(),
            },
            at(1),
        )
        .expect("claim");
    let input = WorkUpdateInput::Release {
        reason: "redirected before any work".into(),
        waiver_reason: Some("redirected before any work".into()),
        idempotency_key: "committed-release".into(),
    };

    // Commit the core release under the protocol attempt, then stop before
    // the attempt records its receipt, as an interrupted process would.
    let mut store = SqliteStore::open(&database).expect("store");
    let basis = service
        .protocol_basis(&store, true, false, None, at(2))
        .expect("basis");
    let intent = service.protocol_intent(&input);
    store
        .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
            project_id: &project,
            session_id: &session,
            operation: "work_update:release",
            idempotency_key: "committed-release",
            intent: &intent,
            basis: &basis,
            now: at(2),
        })
        .expect("begin durable attempt");
    let work = basis.focused_work.clone().expect("focus");
    let claim = basis.claim.clone().expect("live claim");
    let committed = store
        .release_work(
            &ReleaseWorkRequest {
                work_id: work.work_id,
                run_id: claim.run_id,
                expected_work_revision: work.revision,
                holder: session.clone(),
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                reason: "redirected before any work".into(),
                waiver_reason: Some("redirected before any work".into()),
                actor: service.actor("work_update", "release ambient local work"),
                idempotency_key: service
                    .core_operation_key("work_update:release", "committed-release", "release_work")
                    .expect("scoped operation key"),
                released_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("commit core release without protocol result");
    assert_eq!(committed.waiver_recorded, Some(true));
    drop(store);

    let recovered = service
        .work_update(input.clone(), at(3))
        .expect("recover core-committed release");
    assert_eq!(recovered.operation, "release");
    assert_eq!(recovered.receipt.work_id, root.work_id);
    assert_eq!(
        recovered.receipt.result["waiver_recorded"],
        serde_json::json!(true)
    );
    let replayed = service
        .work_update(input, at(4))
        .expect("replay exact protocol result");
    assert_eq!(
        serde_json::to_vec(&replayed).expect("serialize replay"),
        serde_json::to_vec(&recovered).expect("serialize recovery")
    );
}
