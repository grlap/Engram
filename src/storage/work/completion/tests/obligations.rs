use super::*;

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
