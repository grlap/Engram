use super::*;
use crate::storage::work::query::inspect_work_on;
use crate::storage::work::test_support::*;
use crate::storage::work::*;
use crate::{
    COMPLETION_OBLIGATION_SCHEMA_VERSION, VerificationEvidence, WorkCompletionRecoveryCause,
};

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
                expected_run_id: Some(child.active_run_id.expect("child run")),
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
