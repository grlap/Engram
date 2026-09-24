use super::*;

#[test]
fn completion_records_an_untested_source_change_as_a_sealed_waiver() {
    let directory = crate::test_support::temp_home().expect("temporary directory");
    let mut store = SqliteStore::open(directory.path().join("engram.sqlite3")).expect("store");
    let work = store
        .create_work(
            &root_request("project-untested-change", "create-untested-work", 1),
            &DevelopmentNoopRedactor,
        )
        .expect("create local work");
    let claim = claim(&mut store, &work, "runner", "claim-untested-work", 2, 300);
    let mutation = source_mutation(
        &mut store,
        &work,
        &claim,
        "runner",
        "untested",
        3,
        Some("revision-untested"),
    );
    let opened = store
        .work_run_obligations(claim.run_id)
        .expect("open obligation");
    assert_eq!(opened.len(), 1);
    assert_eq!(opened[0].state, WorkObligationState::Open);
    assert_eq!(opened[0].obligation.triggering_observation, mutation);
    let generic = evidence(&mut store, &work, &claim, "runner", "untested-evidence", 4);
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "untested-checkpoint",
        5,
        std::slice::from_ref(&generic),
    );
    let checkpoint_end = feed_head(&store.connection, &FeedId::RunExecution(claim.run_id))
        .expect("run head at the checkpoint");
    let seal = complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &generic,
        "untested-completion",
        6,
    )
    .expect("an untested source change does not refuse completion");

    // Completion resolved the obligation as a waiver in the completing
    // actor's name, naming the change and its source revision.
    let terminal = store
        .work_run_obligations(claim.run_id)
        .expect("terminal obligation");
    assert_eq!(terminal.len(), 1);
    assert_eq!(terminal[0].state, WorkObligationState::Waived);
    let Some(WorkObligationResolution::Waived { waived_by, reason }) = terminal[0]
        .resolution
        .as_ref()
        .map(|event| &event.resolution)
    else {
        panic!("completion must waive the untested change: {terminal:?}");
    };
    assert_eq!(waived_by, "runner");
    assert!(
        reason.contains("write-untested")
            && reason.contains("revision-untested")
            && reason.contains("no matching passing test"),
        "{reason}"
    );
    // The seal binds that waiver like any terminal obligation, and the waiver
    // lies after the checkpoint and inside the sealed cut.
    let resolution = terminal[0].resolution_id.clone().expect("waiver hash");
    assert_eq!(
        seal.obligations,
        vec![CompletionObligationBinding {
            obligation_id: terminal[0].obligation.obligation_id,
            definition: terminal[0].definition_id.clone(),
            resolution: resolution.clone(),
        }]
    );
    let waiver_position =
        run_feed_position_for_object_on(&store.connection, claim.run_id, &resolution)
            .expect("waiver run-feed position");
    assert!(waiver_position.position > checkpoint_end);
    assert!(waiver_position.position <= seal.completion_cut.position);
    validate_completion_seal_obligation_basis_on(&store.connection, &seal)
        .expect("reconstruct the sealed waiver basis");
    let report = store.verify_all().expect("integrity report");
    assert!(report.is_healthy(), "{report:?}");
}

#[test]
fn a_recovery_answer_never_names_the_untested_waivers_it_rolls_back() {
    let directory = crate::test_support::temp_home().expect("temporary directory");
    let mut store = SqliteStore::open(directory.path().join("engram.sqlite3")).expect("store");
    let work = store
        .create_work(
            &root_request("project-untested-recovery", "create-untested-recovery", 1),
            &DevelopmentNoopRedactor,
        )
        .expect("create local work");
    let claim = claim(
        &mut store,
        &work,
        "runner",
        "claim-untested-recovery",
        2,
        300,
    );
    source_mutation(
        &mut store,
        &work,
        &claim,
        "runner",
        "untested-recovery",
        3,
        Some("revision-recovery"),
    );
    store
        .add_expected_root_contributor_fixture(
            work.work_id,
            &SessionId("absent-contributor".into()),
            at(4),
        )
        .expect("seed an unaccounted expected contributor");
    let generic = evidence(&mut store, &work, &claim, "runner", "recovery-evidence", 5);
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "recovery-checkpoint",
        6,
        std::slice::from_ref(&generic),
    );
    let run_feed = FeedId::RunExecution(claim.run_id);
    let head = feed_head(&store.connection, &run_feed).expect("run head before completion");
    let answer = store
        .complete_work_for_protocol(
            &completion_request(&work, &claim, "runner", &generic, "recovery-completion", 7),
            &DevelopmentNoopRedactor,
        )
        .expect("a missing contributor is a typed recovery");
    let CompleteWorkStorageResult::Recovery(snapshot) = answer else {
        panic!("an unaccounted contributor must return a recovery answer");
    };
    assert!(matches!(
        snapshot.recovery.cause,
        WorkCompletionRecoveryCause::MissingContribution { .. }
    ));
    // The answer reports what the store holds once the refused completion is
    // dropped: the stock obligation still open, with no waiver.
    let durable = store
        .work_run_obligations(claim.run_id)
        .expect("durable obligations");
    assert_eq!(durable.len(), 1);
    assert_eq!(durable[0].state, WorkObligationState::Open);
    assert_eq!(snapshot.obligations.len(), 1);
    assert_eq!(
        snapshot.obligations[0].obligation.obligation_id,
        durable[0].obligation.obligation_id
    );
    assert_eq!(snapshot.obligations[0].state, WorkObligationState::Open);
    assert!(snapshot.obligations[0].resolution_id.is_none());
    assert_eq!(
        feed_head(&store.connection, &run_feed).expect("run head after refusal"),
        head
    );
}

#[test]
fn every_untested_change_is_counted_when_the_page_names_only_some() {
    let directory = crate::test_support::temp_home().expect("temporary directory");
    let database = directory.path().join("engram.sqlite3");
    let mut store = SqliteStore::open(&database).expect("store");
    let work = store
        .create_work(
            &root_request("project-many-untested", "create-many-untested", 1),
            &DevelopmentNoopRedactor,
        )
        .expect("create local work");
    let claim = claim(&mut store, &work, "runner", "claim-many-untested", 2, 300);
    let changes = 10;
    for index in 0..changes {
        source_mutation(
            &mut store,
            &work,
            &claim,
            "runner",
            &format!("many-{index}"),
            3,
            Some(&format!("revision-{index}")),
        );
    }
    let generic = evidence(&mut store, &work, &claim, "runner", "many-evidence", 4);
    checkpoint(
        &mut store,
        &work,
        &claim,
        "runner",
        "many-checkpoint",
        5,
        std::slice::from_ref(&generic),
    );
    let seal = complete(
        &mut store,
        &work,
        &claim,
        "runner",
        &generic,
        "many-completion",
        6,
    )
    .expect("untested changes do not refuse completion");
    assert_eq!(seal.obligations.len(), changes);
    drop(store);
    let view = LocalWorkService::new(
        database,
        work.project_id.clone(),
        "runner".into(),
        SessionId("runner".into()),
        None,
    )
    .inspect_work(&work.short_ref, at(7))
    .expect("completed focus view");
    let page = &view.obligation_page;
    let named = page
        .items
        .iter()
        .filter(|item| item.untested_change.is_some())
        .count();
    assert_eq!(page.untested_total, changes);
    assert!(named > 0 && named < changes, "{named} named");
    assert_eq!(named + page.omitted_count, changes);
}

#[test]
fn completion_seals_a_tested_source_change_as_the_exact_terminal_basis() {
    let directory = crate::test_support::temp_home().expect("temporary directory");
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
        action_fingerprint: ObjectId::from_canonical_bytes(b"write src/lib.rs"),
        effect: EffectClass::MutateLocal,
        outcome: ExecutionOutcome::Succeeded,
        source_changed: true,
        obligation_rule_set: active_rule_set_id(&store.connection),
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

    let verification_observation = ExecutionObservation {
        schema_version: SCHEMA_VERSION,
        project_id: work.project_id.clone(),
        binding,
        session_id: SessionId("runner".into()),
        grant_id: "completion-obligation-grant".into(),
        observation_id: "completion-verification".into(),
        action_fingerprint: ObjectId::from_canonical_bytes(b"cargo test --workspace"),
        effect: EffectClass::Observe,
        outcome: ExecutionOutcome::Succeeded,
        source_changed: false,
        obligation_rule_set: active_rule_set_id(&store.connection),
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
        .key()
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
            definition: terminal[0].definition_id.clone(),
            resolution: terminal[0]
                .resolution_id
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
    // A tested change discloses nothing as untested.
    let tested = LocalWorkService::new(
        database.clone(),
        work.project_id.clone(),
        "runner".into(),
        SessionId("runner".into()),
        None,
    )
    .inspect_work(&work.short_ref, at(10))
    .expect("completed focus view");
    assert_eq!(tested.obligation_page.untested_total, 0);
    assert!(
        tested
            .obligation_page
            .items
            .iter()
            .all(|item| item.untested_change.is_none())
    );
    let report = store.verify_all().expect("integrity report");
    assert!(report.is_healthy(), "{report:?}");
    let seal_id = store.stored_seal_id(&seal);
    let mut forged_seal = seal.clone();
    forged_seal.obligations.clear();
    store
        .connection
        .execute_batch("SAVEPOINT corrupt_completion_obligations")
        .expect("start completion-seal corruption fixture");
    store
        .connection
        .execute(
            "UPDATE work_completion_seals SET seal_json = ?2 WHERE seal_id = ?1",
            params![
                seal_id.as_str(),
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
                .key()
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
                action_fingerprint: ObjectId::from_canonical_bytes(
                    format!("write source {index}").as_bytes(),
                ),
                effect: EffectClass::MutateLocal,
                outcome: ExecutionOutcome::Succeeded,
                source_changed: true,
                obligation_rule_set: active_rule_set_id(&transaction),
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
    let directory = crate::test_support::temp_home().expect("temporary directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("project-protocol-obligations".into());
    let session = SessionId("runner".into());
    let (work, expected_obligation, binding, run_actor, source_basis) = {
        let mut store = SqliteStore::open(&database).expect("store");
        // A criterion bound to a host test owes it; the stock source-change
        // rule alone would record the change as untested and complete.
        let mut request = root_request(&project.0, "create-protocol-obligation-work", 1);
        request.acceptance_bindings = vec![crate::domain::AcceptanceBinding {
            criterion: 1,
            requirement: crate::domain::VerificationRequirement {
                check_kind: VerificationKind::Test,
                check_fingerprint: None,
                required_environment: None,
            },
        }];
        let work = store
            .create_work(&request, &DevelopmentNoopRedactor)
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
                action_fingerprint: ObjectId::from_canonical_bytes(b"write protocol source"),
                effect: EffectClass::MutateLocal,
                outcome: ExecutionOutcome::Succeeded,
                source_changed: true,
                obligation_rule_set: active_rule_set_id(&transaction),
                source_basis: Some(source_basis.clone()),
                observed_at: Some(at(3)),
                actor: run_actor.clone(),
                recorded_at: at(3),
            },
        )
        .expect("append protocol mutation obligation");
        transaction.commit().expect("commit protocol obligation");
        let obligations = store
            .work_run_obligations(run.run_id)
            .expect("protocol obligations");
        assert_eq!(obligations.len(), 2);
        let obligation = obligations
            .into_iter()
            .find(|record| {
                crate::control::acceptance_binding_criterion(&record.obligation.rule) == Some(1)
            })
            .expect("the bound criterion's obligation");
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
        source_fingerprint: None,
        links: Vec::new(),
        link_basis: None,
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
    // The page is read from the state before completion's waivers: the bound
    // criterion's obligation that refused, then the stock obligation the
    // dropped completion leaves open.
    assert_eq!(refusal.obligation_page.items.len(), 2);
    assert!(
        refusal
            .obligation_page
            .items
            .iter()
            .all(|item| item.state == WorkObligationState::Open && item.untested_change.is_none())
    );
    assert!(crate::control::is_stock_source_change_obligation(
        &refusal.obligation_page.items[1].rule,
        &refusal.obligation_page.items[1].requirement,
    ));
    assert_eq!(
        refusal.obligation_page.items[0].obligation_id,
        expected_obligation.obligation.obligation_id
    );
    assert_eq!(
        refusal.obligation_page.items[0].definition,
        expected_obligation.definition_id
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
            && *definition == expected_obligation.definition_id
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
    // The refusal rolled back the stock rule's waiver with the rest of the
    // completion, so both obligations are still open.
    assert!(
        SqliteStore::open(&database)
            .expect("store after refusal")
            .work_run_obligations(binding.run_id)
            .expect("obligations after refusal")
            .iter()
            .all(|record| record.state == WorkObligationState::Open)
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
             WHERE operation = 'work_complete' AND result_id IS NULL",
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
                action_fingerprint: ObjectId::from_canonical_bytes(
                    b"cargo test protocol obligation",
                ),
                effect: EffectClass::Observe,
                outcome: ExecutionOutcome::Succeeded,
                source_changed: false,
                obligation_rule_set: active_rule_set_id(&store.connection),
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
                check_fingerprint: ObjectId::from_canonical_bytes(
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
    let directory = crate::test_support::temp_home().expect("temporary directory");
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
    let rule_set = active_rule_set_id(&store.connection);
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
        action_fingerprint: ObjectId::from_canonical_bytes(action.as_bytes()),
        effect: if source_changed {
            EffectClass::MutateLocal
        } else {
            EffectClass::Observe
        },
        outcome: ExecutionOutcome::Succeeded,
        source_changed,
        obligation_rule_set: rule_set.clone(),
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
                expected_definition: waiver_target.definition_id.clone(),
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
        expected_definition: waiver_target.definition_id.clone(),
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
    let definition = target.definition_id.as_str();
    let resolution = target
        .resolution_id
        .as_ref()
        .expect("waiver resolution")
        .as_str();
    let forged_uuid = uuid::Uuid::new_v4().to_string();
    let corruptions = [
        format!(
            "UPDATE work_run_obligations SET obligation_id = '{forged_uuid}' WHERE obligation_id = '{obligation_id}'"
        ),
        format!(
            "UPDATE work_run_obligations SET definition_id = '{resolution}' WHERE obligation_id = '{obligation_id}'"
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
            "UPDATE work_run_obligations SET triggering_observation_id = '{definition}' WHERE obligation_id = '{obligation_id}'"
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
            "UPDATE work_run_obligations SET resolution_id = '{definition}' WHERE obligation_id = '{obligation_id}'"
        ),
        format!(
            "UPDATE work_run_obligations SET resolution_kind = 'satisfied' WHERE obligation_id = '{obligation_id}'"
        ),
        format!(
            "UPDATE work_run_obligations SET evidence_id = '{definition}' WHERE obligation_id = '{obligation_id}'"
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
