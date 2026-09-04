use super::super::session::{
    MAX_PROCESS_DEFAULT_SESSION_RECLAIMS_PER_CREATION,
    PROCESS_DEFAULT_SESSION_RECLAMATION_CANDIDATES_SQL,
};
use super::super::test_support::*;
use super::super::*;
use super::*;

mod claims;
mod gate_evidence;
mod handoffs;
mod sessions;

#[test]
fn work_bound_control_checkpoint_records_execution_observation_once() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("engram.sqlite3");
    let mut store = SqliteStore::open(&database).expect("store");
    let frozen_rule_set = store
        .control_diagnostics()
        .expect("initial control policy")
        .obligation_rule_set;
    let work = store
        .create_work(
            &root_request("project-control-work", "create-control-work", 1),
            &DevelopmentNoopRedactor,
        )
        .expect("create local work");
    let claim = claim(&mut store, &work, "runner", "claim-control-work", 2, 120);
    let run = load_work_run(&store.connection, claim.run_id).expect("load claimed run");
    let work_binding = ControlWorkBinding {
        root_execution_id: run.root_execution_id,
        work_id: work.work_id,
        run_id: run.run_id,
        work_revision: claim.accepted_work_revision,
        claim_id: claim.claim_id,
        claim_fence: claim.fence,
    };
    let session_id = SessionId("runner".into());
    let connection_token = store
        .resume_control_connection(&session_id, at(3))
        .expect("resume host control connection");
    let mut host_actor = actor("runner");
    host_actor.run_id = Some(run.run_id.0.to_string());
    let control_binding = store
        .bind_control_session_with_work(
            &work.project_id,
            "local-work:control-observation",
            "Record one bound execution observation",
            &session_id,
            &connection_token,
            &host_actor,
            Some(&work_binding),
            ControlAssurance::TurnGated,
            &[EffectClass::Observe, EffectClass::MutateLocal],
            1,
            "bind-control-work",
            at(3),
        )
        .expect("bind control session to live claim");
    assert_eq!(
        control_binding.status.work_binding.as_ref(),
        Some(&work_binding)
    );
    let peer_session = SessionId("peer-runner".into());
    let peer_connection = store
        .resume_control_connection(&peer_session, at(3))
        .expect("resume peer host control connection");
    let mut peer_actor = actor("peer-runner");
    peer_actor.run_id = Some(run.run_id.0.to_string());
    assert!(matches!(
        store.bind_control_session_with_work(
            &work.project_id,
            "local-work:peer-observation",
            "Peer must not inherit another claim",
            &peer_session,
            &peer_connection,
            &peer_actor,
            Some(&work_binding),
            ControlAssurance::TurnGated,
            &[EffectClass::Observe],
            1,
            "bind-peer-control-work",
            at(3),
        ),
        Err(StoreError::WorkClaimMismatch { .. })
    ));

    let synchronize = store
        .evaluate_control_turn(
            &work.project_id,
            &session_id,
            &connection_token,
            &control_binding.routing_token,
            &TurnIntent {
                idempotency_key: "synchronize-control-work".into(),
                intent_fingerprint: ObjectHash::from_canonical_bytes(b"sync bound work"),
                purpose: TurnPurpose::Ordinary,
                requested_effects: vec![EffectClass::Observe],
                resource_intents: Vec::new(),
            },
            at(4),
        )
        .expect("evaluate synchronization turn");
    let ControlTurnDecision::Grant { grant: synchronize } = synchronize else {
        panic!("bound work synchronization must grant: {synchronize:?}");
    };
    assert_eq!(synchronize.basis.work_binding.as_ref(), Some(&work_binding));
    let sync_tokens = synchronize
        .delivery
        .as_ref()
        .map(|delivery| vec![delivery.page.delivery_token.clone()])
        .unwrap_or_default();
    assert!(matches!(
        store
            .begin_control_turn(
                &work.project_id,
                &session_id,
                &connection_token,
                &control_binding.routing_token,
                &synchronize.grant_id,
                &sync_tokens,
                "begin-control-work-sync",
                at(5),
            )
            .expect("begin bound work synchronization"),
        ControlTurnBeginDecision::Begin { .. }
    ));
    assert!(matches!(
        store
            .checkpoint_control_turn(
                &work.project_id,
                &session_id,
                &connection_token,
                &control_binding.routing_token,
                &synchronize.grant_id,
                TurnNextIntent::Continue,
                "checkpoint-control-work-sync",
                at(6),
            )
            .expect("checkpoint bound work synchronization"),
        ControlTurnCheckpointDecision::Checkpointed { .. }
    ));

    let subject = crate::domain::ResourceSubject::Path {
        project_id: work.project_id.clone(),
        segments: vec!["src".into()],
        coverage: crate::domain::ResourceCoverage::Tree,
    };
    let lease = store
        .acquire_work_lease(
            &work.project_id,
            &session_id,
            &connection_token,
            &control_binding.routing_token,
            crate::domain::LeaseKind::Execution,
            crate::domain::LeaseMode::Exclusive,
            &subject,
            60,
            "lease-control-work",
            at(7),
        )
        .expect("acquire bound execution lease");
    assert!(matches!(
        lease,
        crate::domain::WorkLeaseDecision::Granted { .. }
    ));
    let decision = store
        .evaluate_control_turn(
            &work.project_id,
            &session_id,
            &connection_token,
            &control_binding.routing_token,
            &TurnIntent {
                idempotency_key: "evaluate-control-work".into(),
                intent_fingerprint: ObjectHash::from_canonical_bytes(b"mutate bound work"),
                purpose: TurnPurpose::Ordinary,
                requested_effects: vec![EffectClass::MutateLocal],
                resource_intents: vec![subject],
            },
            at(8),
        )
        .expect("evaluate bound work mutation");
    let ControlTurnDecision::Grant { grant } = decision else {
        panic!("live bound work mutation must grant: {decision:?}");
    };
    assert_eq!(grant.basis.work_binding.as_ref(), Some(&work_binding));
    let delivery_tokens = grant
        .delivery
        .as_ref()
        .map(|delivery| vec![delivery.page.delivery_token.clone()])
        .unwrap_or_default();
    assert!(matches!(
        store
            .begin_control_turn(
                &work.project_id,
                &session_id,
                &connection_token,
                &control_binding.routing_token,
                &grant.grant_id,
                &delivery_tokens,
                "begin-control-work",
                at(9),
            )
            .expect("begin bound work turn"),
        ControlTurnBeginDecision::Begin { .. }
    ));
    let empty_rule_set = crate::domain::ObligationRuleSet {
        schema_version: crate::domain::OBLIGATION_RULE_SET_SCHEMA_VERSION,
        rules: Vec::new(),
    };
    let changed_rules = store
        .set_obligation_rule_set(
            &empty_rule_set,
            &actor("obligation-rule-admin"),
            "disable future built-in obligation triggers in this test",
            "work-obligation-rule-disable",
            None,
            at(9),
            &DevelopmentNoopRedactor,
        )
        .expect("activate empty future rule set after turn begin");
    assert_ne!(changed_rules.obligation_rule_set, frozen_rule_set);
    let out_of_scope = ExecutionObservationInput {
        observation_id: "outside-grant-scope".into(),
        action_fingerprint: ObjectHash::from_canonical_bytes(b"observe after mutation grant"),
        effect: EffectClass::Observe,
        outcome: ExecutionOutcome::Succeeded,
        source_changed: false,
        source_basis: None,
        observed_at: None,
    };
    assert!(matches!(
        store.checkpoint_control_turn_with_observations(
            &work.project_id,
            &session_id,
            &connection_token,
            &control_binding.routing_token,
            &grant.grant_id,
            TurnNextIntent::Continue,
            &[out_of_scope],
            "checkpoint-control-work-scope",
            at(10),
        ),
        Err(StoreError::ControlObservationScopeMismatch { observation_id })
            if observation_id == "outside-grant-scope"
    ));
    let observations = vec![
        ExecutionObservationInput {
            observation_id: "source-mutation-1".into(),
            action_fingerprint: ObjectHash::from_canonical_bytes(b"write src/lib.rs"),
            effect: EffectClass::MutateLocal,
            outcome: ExecutionOutcome::Succeeded,
            source_changed: true,
            source_basis: Some(ExecutionSourceBasis {
                workspace_id: "workspace-a".into(),
                source_revision: "content-revision-1".into(),
            }),
            observed_at: Some(at(9)),
        },
        ExecutionObservationInput {
            observation_id: "verification-command-1".into(),
            action_fingerprint: ObjectHash::from_canonical_bytes(b"cargo test --workspace"),
            effect: EffectClass::MutateLocal,
            outcome: ExecutionOutcome::Succeeded,
            source_changed: false,
            source_basis: Some(ExecutionSourceBasis {
                workspace_id: "workspace-b".into(),
                source_revision: "content-revision-1".into(),
            }),
            observed_at: Some(at(9)),
        },
    ];
    let verification_inputs = vec![VerificationEvidenceInput {
        producer_observation: ExecutionObservationReference::ObservationId {
            observation_id: "verification-command-1".into(),
        },
        check_kind: VerificationKind::Test,
        environment: Some(EnvironmentEvidenceReference::Index { index: 0 }),
        summary: Some("host observed the workspace test suite".into()),
        refs: vec!["command:cargo-test-workspace".into()],
    }];
    let environment_components = EnvironmentComponents {
        toolchain: "rustc-1.89.0".into(),
        sandbox: Some("windows-host-sandbox-v1".into()),
        workspace_id: "workspace-b".into(),
        capability_map_revision: 1,
    };
    let environment_fingerprint = CanonicalObject::freeze(&environment_components)
        .expect("freeze environment components")
        .hash()
        .clone();
    let environment_inputs = vec![EnvironmentEvidenceInput {
        source_basis: ExecutionSourceBasis {
            workspace_id: "workspace-b".into(),
            source_revision: "content-revision-1".into(),
        },
        environment_fingerprint,
        components: Some(environment_components.clone()),
        observed_at: at(9),
    }];
    let missing_producer = VerificationEvidenceInput {
        producer_observation: ExecutionObservationReference::ObjectHash {
            object_hash: ObjectHash::from_canonical_bytes(b"missing producer"),
        },
        check_kind: VerificationKind::Test,
        environment: None,
        summary: None,
        refs: Vec::new(),
    };
    assert!(matches!(
        store.checkpoint_control_turn_with_evidence(
            &work.project_id,
            &session_id,
            &connection_token,
            &control_binding.routing_token,
            &grant.grant_id,
            TurnNextIntent::Continue,
            &[],
            &[],
            &[EnvironmentEvidenceInput {
                environment_fingerprint: ObjectHash::from_canonical_bytes(
                    b"wrong environment fingerprint"
                ),
                ..environment_inputs[0].clone()
            }],
            "checkpoint-mismatched-environment-fingerprint",
            at(10),
        ),
        Err(StoreError::EnvironmentFingerprintMismatch)
    ));
    let mismatched_components = EnvironmentComponents {
        capability_map_revision: 2,
        ..environment_components.clone()
    };
    assert!(matches!(
        store.checkpoint_control_turn_with_evidence(
            &work.project_id,
            &session_id,
            &connection_token,
            &control_binding.routing_token,
            &grant.grant_id,
            TurnNextIntent::Continue,
            &[],
            &[],
            &[EnvironmentEvidenceInput {
                environment_fingerprint: CanonicalObject::freeze(&mismatched_components)
                    .expect("freeze mismatched environment components")
                    .hash()
                    .clone(),
                components: Some(mismatched_components),
                ..environment_inputs[0].clone()
            }],
            "checkpoint-mismatched-capability-map",
            at(10),
        ),
        Err(StoreError::EnvironmentBasisMismatch(_))
    ));
    assert!(matches!(
        store.checkpoint_control_turn_with_evidence(
            &work.project_id,
            &session_id,
            &connection_token,
            &control_binding.routing_token,
            &grant.grant_id,
            TurnNextIntent::Continue,
            &observations,
            &verification_inputs,
            &[],
            "checkpoint-missing-same-request-environment",
            at(10),
        ),
        Err(StoreError::EnvironmentEvidenceNotFound(index)) if index == "0"
    ));
    assert!(matches!(
        store.checkpoint_control_turn_with_evidence(
            &work.project_id,
            &session_id,
            &connection_token,
            &control_binding.routing_token,
            &grant.grant_id,
            TurnNextIntent::Continue,
            &[],
            &[missing_producer],
            &[],
            "checkpoint-missing-verification-producer",
            at(10),
        ),
        Err(StoreError::VerificationProducerObservationNotFound(_))
    ));
    let checkpointed = store
        .checkpoint_control_turn_with_evidence(
            &work.project_id,
            &session_id,
            &connection_token,
            &control_binding.routing_token,
            &grant.grant_id,
            TurnNextIntent::Continue,
            &observations,
            &verification_inputs,
            &environment_inputs,
            "checkpoint-control-work",
            at(10),
        )
        .expect("checkpoint bound work turn");
    let ControlTurnCheckpointDecision::Checkpointed { receipt } = &checkpointed else {
        panic!("bound work turn must checkpoint");
    };
    assert_eq!(receipt.execution_observations.len(), 2);
    assert_eq!(receipt.verification_evidence.len(), 1);
    assert_eq!(receipt.environment_evidence.len(), 1);
    let observation_hash = &receipt.execution_observations[0];
    let observation = load_typed_work_object::<ExecutionObservation>(
        &store.connection,
        observation_hash,
        "execution_observation",
    )
    .expect("load canonical execution observation");
    assert_eq!(observation.binding, work_binding);
    assert_eq!(observation.session_id, session_id);
    assert!(observation.source_changed);
    assert_eq!(observation.obligation_rule_set, frozen_rule_set);
    assert_eq!(observation.source_basis, observations[0].source_basis);
    assert_eq!(observation.observed_at, observations[0].observed_at);
    assert_eq!(observation.recorded_at, at(10));
    let feed_count = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM work_feed_entries WHERE object_hash = ?1",
            [observation_hash.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .expect("count observation feed entries");
    assert_eq!(feed_count, 3);

    let verification_hash = &receipt.verification_evidence[0];
    let verification = store
        .load_verification_evidence(verification_hash)
        .expect("load typed verification evidence");
    let producer_hash = &receipt.execution_observations[1];
    let producer = load_control_execution_observation_on(&store.connection, producer_hash)
        .expect("load verification producer")
        .expect("verification producer exists");
    assert_eq!(verification.producer_observation, *producer_hash);
    assert_eq!(verification.source_basis.workspace_id, "workspace-b");
    assert_eq!(
        verification.source_basis.source_revision,
        "content-revision-1"
    );
    assert_eq!(verification.check_fingerprint, producer.action_fingerprint);
    assert_eq!(
        verification.environment.as_ref(),
        Some(&receipt.environment_evidence[0])
    );
    assert_eq!(
        verification.result,
        crate::domain::VerificationResult::Passed
    );
    assert_eq!(
        store
            .work_evidence_kind(run.run_id, verification_hash)
            .expect("verification projection kind"),
        WorkEvidenceKind::Verification
    );
    let not_yet_recorded_environment =
        ObjectHash::from_canonical_bytes(b"required environment not yet produced");
    let focus_candidates = store
        .work_run_evidence_projection(
            run.run_id,
            std::slice::from_ref(&not_yet_recorded_environment),
            8,
        )
        .expect("an unmet required environment is not projection corruption");
    assert!(
        focus_candidates
            .iter()
            .all(|candidate| candidate.hash != not_yet_recorded_environment)
    );
    assert!(matches!(
        store.work_run_evidence_projection(run.run_id, std::slice::from_ref(verification_hash), 8,),
        Err(StoreError::InvalidWorkProjection(_))
    ));
    store
        .connection
        .execute(
            "UPDATE work_run_evidence SET evidence_kind = 'generic'
             WHERE evidence_hash = ?1",
            [verification_hash.as_str()],
        )
        .expect("corrupt selection-driving evidence kind");
    assert!(matches!(
        store.work_run_evidence_projection(run.run_id, &[], 8),
        Err(StoreError::InvalidWorkProjection(_))
    ));
    store
        .connection
        .execute(
            "UPDATE work_run_evidence SET evidence_kind = 'verification'
             WHERE evidence_hash = ?1",
            [verification_hash.as_str()],
        )
        .expect("restore evidence selection projection");
    let environment_hash = &receipt.environment_evidence[0];
    let environment = store
        .load_environment_evidence(environment_hash)
        .expect("load typed environment evidence");
    assert_eq!(environment.source_basis.workspace_id, "workspace-b");
    assert_eq!(
        environment.environment_fingerprint,
        CanonicalObject::freeze(&environment_components)
            .expect("re-freeze environment components")
            .hash()
            .clone()
    );
    assert_eq!(
        environment.components.as_ref(),
        Some(&environment_components)
    );
    let opaque_environment = EnvironmentEvidence {
        components: None,
        ..environment.clone()
    };
    let opaque_object =
        CanonicalObject::freeze(&opaque_environment).expect("freeze opaque environment evidence");
    assert!(
        !std::str::from_utf8(opaque_object.bytes())
            .expect("environment evidence is UTF-8 JSON")
            .contains("components"),
        "the optional component field changed opaque canonical bytes"
    );
    assert_eq!(
        opaque_object
            .decode::<EnvironmentEvidence>()
            .expect("decode opaque environment evidence"),
        opaque_environment
    );
    assert_eq!(
        super::super::super::resolve_verification_environment_on(
            &store.connection,
            Some(&EnvironmentEvidenceReference::ObjectHash {
                object_hash: environment_hash.clone(),
            }),
            &[],
            &work.project_id,
            &work_binding,
            &verification.source_basis,
        )
        .expect("resolve existing environment by hash"),
        Some(environment_hash.clone())
    );
    let mut wrong_environment_binding = work_binding.clone();
    wrong_environment_binding.run_id = WorkRunId(uuid::Uuid::now_v7());
    assert!(matches!(
        super::super::super::resolve_verification_environment_on(
            &store.connection,
            Some(&EnvironmentEvidenceReference::ObjectHash {
                object_hash: environment_hash.clone(),
            }),
            &[],
            &work.project_id,
            &wrong_environment_binding,
            &verification.source_basis,
        ),
        Err(StoreError::EnvironmentBasisMismatch(_))
    ));
    let wrong_source_basis = ExecutionSourceBasis {
        source_revision: "content-revision-2".into(),
        ..verification.source_basis.clone()
    };
    assert!(matches!(
        super::super::super::resolve_verification_environment_on(
            &store.connection,
            Some(&EnvironmentEvidenceReference::ObjectHash {
                object_hash: environment_hash.clone(),
            }),
            &[],
            &work.project_id,
            &work_binding,
            &wrong_source_basis,
        ),
        Err(StoreError::EnvironmentBasisMismatch(_))
    ));
    assert!(matches!(
        super::super::super::resolve_verification_environment_on(
            &store.connection,
            Some(&EnvironmentEvidenceReference::ObjectHash {
                object_hash: ObjectHash::from_canonical_bytes(b"missing environment"),
            }),
            &[],
            &work.project_id,
            &work_binding,
            &verification.source_basis,
        ),
        Err(StoreError::EnvironmentEvidenceNotFound(_))
    ));
    assert_eq!(
        store
            .work_evidence_kind(run.run_id, environment_hash)
            .expect("environment projection kind"),
        WorkEvidenceKind::Environment
    );
    for evidence_hash in [verification_hash, environment_hash] {
        let typed_feed_count = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM work_feed_entries WHERE object_hash = ?1",
                [evidence_hash.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .expect("count typed evidence feed entries");
        assert_eq!(typed_feed_count, 3);
    }
    let run_positions = |hash: &ObjectHash| {
        store
            .connection
            .query_row(
                "SELECT position FROM work_feed_entries
                 WHERE feed_kind = 'run_execution' AND feed_id = ?1 AND object_hash = ?2",
                params![run.run_id.0.to_string(), hash.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .expect("typed run-feed position")
    };
    let verification_match = VerificationEvidenceMatchInput {
        candidate_kind: WorkEvidenceKind::Verification,
        evidence: Some(&verification),
        producer: Some(&producer),
        latest_mutation: &observation,
        evidence_position: run_positions(verification_hash),
        latest_mutation_position: run_positions(observation_hash),
        requirement: &crate::domain::VerificationRequirement {
            check_kind: crate::domain::VerificationKind::Test,
            check_fingerprint: Some(producer.action_fingerprint.clone()),
            required_environment: None,
        },
    };
    assert_eq!(match_verification_evidence(&verification_match), Ok(()));
    let exact_environment_requirement = crate::domain::VerificationRequirement {
        check_kind: crate::domain::VerificationKind::Test,
        check_fingerprint: Some(producer.action_fingerprint.clone()),
        required_environment: Some(environment_hash.clone()),
    };
    assert_eq!(
        match_verification_evidence(&VerificationEvidenceMatchInput {
            requirement: &exact_environment_requirement,
            ..verification_match
        }),
        Ok(())
    );
    let wrong_environment_requirement = crate::domain::VerificationRequirement {
        required_environment: Some(ObjectHash::from_canonical_bytes(b"other environment")),
        ..exact_environment_requirement
    };
    assert_eq!(
        match_verification_evidence(&VerificationEvidenceMatchInput {
            requirement: &wrong_environment_requirement,
            ..verification_match
        }),
        Err(VerificationEvidenceMismatch::EnvironmentMismatch)
    );
    let obligations = store
        .work_run_obligations(run.run_id)
        .expect("load immutable run obligations");
    assert_eq!(obligations.len(), 1);
    let obligation = &obligations[0];
    assert_eq!(obligation.state, WorkObligationState::Satisfied);
    assert_eq!(obligation.obligation.rule_set, frozen_rule_set);
    assert_eq!(
        obligation.obligation.triggering_observation,
        *observation_hash
    );
    assert_eq!(
        obligation.obligation.requirement.check_kind,
        VerificationKind::Test
    );
    assert_eq!(obligation.obligation.requirement.check_fingerprint, None);
    assert!(matches!(
        obligation
            .resolution
            .as_ref()
            .map(|event| &event.resolution),
        Some(WorkObligationResolution::Satisfied { evidence, .. })
            if evidence == verification_hash
    ));
    for hash in [
        &obligation.definition_hash,
        obligation
            .resolution_hash
            .as_ref()
            .expect("satisfied resolution hash"),
    ] {
        let feed_count = store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM work_feed_entries WHERE object_hash = ?1",
                [hash.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .expect("count obligation feed entries");
        assert_eq!(feed_count, 3);
    }
    let mut later_mutation = observation.clone();
    later_mutation
        .source_basis
        .as_mut()
        .expect("mutation source basis")
        .source_revision = "content-revision-2".into();
    let stale_match = VerificationEvidenceMatchInput {
        latest_mutation: &later_mutation,
        latest_mutation_position: run_positions(verification_hash) + 1,
        evidence_position: run_positions(verification_hash),
        ..verification_match
    };
    assert_eq!(
        match_verification_evidence(&stale_match),
        Err(VerificationEvidenceMismatch::StaleSourceRevision)
    );

    let objects_before_attach = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM objects WHERE object_hash = ?1",
            [verification_hash.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .expect("count verification objects before attach");
    let feeds_before_attach = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM work_feed_entries WHERE object_hash = ?1",
            [verification_hash.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .expect("count verification feeds before attach");
    let work_protocol = crate::LocalWorkService::new(
        database.clone(),
        work.project_id.clone(),
        "runner".into(),
        session_id.clone(),
        Some("typed-evidence-attach-test".into()),
    );
    work_protocol
        .work_focus(&work.short_ref, at(10))
        .expect("focus claimed work before attach");
    let attached = work_protocol
        .work_update(
            crate::WorkUpdateInput::Evidence {
                summary: String::new(),
                refs: Vec::new(),
                attach: Some(crate::WorkEvidenceAttachInput {
                    evidence: verification_hash.to_string(),
                }),
                idempotency_key: "attach-verification-evidence".into(),
            },
            at(10),
        )
        .expect("attach host-minted verification evidence");
    assert_eq!(attached.receipt.result["attached"], true);
    assert_eq!(
        attached.receipt.result["evidence"],
        verification_hash.as_str()
    );
    assert_eq!(attached.receipt.result["evidence_kind"], "verification");
    let focus = work_protocol
        .work_focus(&work.short_ref, at(10))
        .expect("focus after obligation resolution");
    assert_eq!(focus.obligation_page.items.len(), 1);
    assert_eq!(
        focus.obligation_page.items[0].state,
        WorkObligationState::Satisfied
    );
    let objects_after_attach = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM objects WHERE object_hash = ?1",
            [verification_hash.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .expect("count verification objects after attach");
    let feeds_after_attach = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM work_feed_entries WHERE object_hash = ?1",
            [verification_hash.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .expect("count verification feeds after attach");
    assert_eq!(objects_after_attach, objects_before_attach);
    assert_eq!(feeds_after_attach, feeds_before_attach);

    let replay = store
        .checkpoint_control_turn_with_evidence(
            &work.project_id,
            &session_id,
            &connection_token,
            &control_binding.routing_token,
            &grant.grant_id,
            TurnNextIntent::Continue,
            &observations,
            &verification_inputs,
            &environment_inputs,
            "checkpoint-control-work",
            at(11),
        )
        .expect("replay checkpoint exactly");
    assert_eq!(replay, checkpointed);
    let replay_feed_count = store
        .connection
        .query_row(
            "SELECT COUNT(*) FROM work_feed_entries WHERE object_hash = ?1",
            [observation_hash.as_str()],
            |row| row.get::<_, i64>(0),
        )
        .expect("count replayed observation feed entries");
    assert_eq!(replay_feed_count, 3);

    let mut changed_observations = observations.clone();
    changed_observations[0].source_changed = false;
    assert!(matches!(
        store.checkpoint_control_turn_with_observations(
            &work.project_id,
            &session_id,
            &connection_token,
            &control_binding.routing_token,
            &grant.grant_id,
            TurnNextIntent::Continue,
            &changed_observations,
            "checkpoint-control-work",
            at(12),
        ),
        Err(StoreError::ControlOperationIdempotencyConflict { operation, key })
            if operation == "turn_checkpoint" && key == "checkpoint-control-work"
    ));

    let mut future_observation = observation.clone();
    future_observation.observation_id = "source-mutation-under-empty-rules".into();
    future_observation.action_fingerprint =
        ObjectHash::from_canonical_bytes(b"write after rule-set activation");
    future_observation.obligation_rule_set = changed_rules.obligation_rule_set.clone();
    future_observation
        .source_basis
        .as_mut()
        .expect("future mutation source basis")
        .source_revision = "content-revision-2".into();
    future_observation.observed_at = Some(at(12));
    future_observation.recorded_at = at(12);
    let transaction = store
        .connection
        .transaction_with_behavior(TransactionBehavior::Immediate)
        .expect("future observation transaction");
    append_control_execution_observation_on(&transaction, &future_observation)
        .expect("append observation under the newly active empty rule set");
    transaction.commit().expect("commit future observation");
    let retained = store
        .work_run_obligations(run.run_id)
        .expect("reload obligations after rule-set activation");
    assert_eq!(retained.len(), 1);
    assert_eq!(retained[0].definition_hash, obligation.definition_hash);
    store
        .connection
        .execute_batch("SAVEPOINT corrupt_obligation_rule_set")
        .expect("start obligation rule-set corruption fixture");
    store
        .connection
        .execute(
            "UPDATE work_run_obligations SET rule_set_hash = ?1
             WHERE definition_hash = ?2",
            params![
                changed_rules.obligation_rule_set.as_str(),
                obligation.definition_hash.as_str()
            ],
        )
        .expect("corrupt obligation rule-set projection");
    assert!(store.work_run_obligations(run.run_id).is_err());
    let corrupt_rules = store
        .verify_all()
        .expect("report obligation rule-set corruption");
    assert!(
        corrupt_rules
            .invalid_work_records
            .iter()
            .any(|record| record.contains("work_obligation")),
        "rule-set corruption was not reported: {corrupt_rules:?}"
    );
    store
        .connection
        .execute_batch(
            "ROLLBACK TO corrupt_obligation_rule_set; RELEASE corrupt_obligation_rule_set",
        )
        .expect("restore obligation rule-set projection");

    let stale = store
        .evaluate_control_turn(
            &work.project_id,
            &session_id,
            &connection_token,
            &control_binding.routing_token,
            &TurnIntent {
                idempotency_key: "evaluate-expired-control-work".into(),
                intent_fingerprint: ObjectHash::from_canonical_bytes(b"expired bound work"),
                purpose: TurnPurpose::Ordinary,
                requested_effects: vec![EffectClass::Observe],
                resource_intents: Vec::new(),
            },
            at(123),
        )
        .expect("expired bound work is a typed refusal");
    assert!(matches!(
        stale,
        ControlTurnDecision::Refuse { directive }
            if directive.code == ControlRefusalCode::StaleFence
    ));
    let stale_connection = store
        .resume_control_connection(&session_id, at(124))
        .expect("resume stale binding connection");
    assert!(matches!(
        store.bind_control_session_with_work(
            &work.project_id,
            "local-work:stale-observation",
            "Stale binding must require a reread",
            &session_id,
            &stale_connection,
            &host_actor,
            Some(&work_binding),
            ControlAssurance::TurnGated,
            &[EffectClass::Observe],
            1,
            "bind-stale-control-work",
            at(124),
        ),
        Err(StoreError::ControlWorkBindingStale { .. })
    ));
    let unbound_evidence = ObjectHash::from_canonical_bytes(b"unbound run evidence");
    assert!(ensure_run_evidence(&store.connection, run.run_id, &[]).is_ok());
    let mixed_evidence = [(*verification_hash).clone(), unbound_evidence.clone()];
    assert!(matches!(
        ensure_run_evidence(
            &store.connection,
            run.run_id,
            &mixed_evidence,
        ),
        Err(StoreError::InvalidWork(detail))
            if detail.contains(unbound_evidence.as_str())
    ));
    let projection_corruptions = [
        (
            verification_hash,
            "verification-kind",
            "UPDATE work_run_evidence SET evidence_kind = 'environment'
             WHERE evidence_hash = ?1",
            None,
        ),
        (
            verification_hash,
            "verification-workspace",
            "UPDATE work_run_evidence SET workspace_id = 'forged-workspace'
             WHERE evidence_hash = ?1",
            None,
        ),
        (
            verification_hash,
            "verification-revision",
            "UPDATE work_run_evidence SET source_revision = 'forged-revision'
             WHERE evidence_hash = ?1",
            None,
        ),
        (
            verification_hash,
            "verification-session",
            "UPDATE work_run_evidence SET producer_session_id = 'forged-session'
             WHERE evidence_hash = ?1",
            None,
        ),
        (
            verification_hash,
            "verification-producer",
            "UPDATE work_run_evidence SET producer_observation_hash = ?2
             WHERE evidence_hash = ?1",
            Some(observation_hash.as_str()),
        ),
        (
            verification_hash,
            "verification-check",
            "UPDATE work_run_evidence SET check_fingerprint = ?2
             WHERE evidence_hash = ?1",
            Some(environment_hash.as_str()),
        ),
        (
            verification_hash,
            "verification-result",
            "UPDATE work_run_evidence SET verification_result = 'failed'
             WHERE evidence_hash = ?1",
            None,
        ),
        (
            verification_hash,
            "verification-time",
            "UPDATE work_run_evidence SET observed_at_ms = observed_at_ms + 1
             WHERE evidence_hash = ?1",
            None,
        ),
        (
            verification_hash,
            "verification-environment-fingerprint",
            "UPDATE work_run_evidence SET environment_fingerprint = ?2
             WHERE evidence_hash = ?1",
            Some(environment_hash.as_str()),
        ),
        (
            verification_hash,
            "verification-environment-link",
            "UPDATE work_run_evidence SET environment_evidence_hash = ?2
             WHERE evidence_hash = ?1",
            Some(producer_hash.as_str()),
        ),
        (
            verification_hash,
            "verification-components",
            "UPDATE work_run_evidence SET components_json = X'7B7D'
             WHERE evidence_hash = ?1",
            None,
        ),
        (
            environment_hash,
            "environment-kind",
            "UPDATE work_run_evidence SET evidence_kind = 'verification'
             WHERE evidence_hash = ?1",
            None,
        ),
        (
            environment_hash,
            "environment-workspace",
            "UPDATE work_run_evidence SET workspace_id = 'forged-workspace'
             WHERE evidence_hash = ?1",
            None,
        ),
        (
            environment_hash,
            "environment-revision",
            "UPDATE work_run_evidence SET source_revision = 'forged-revision'
             WHERE evidence_hash = ?1",
            None,
        ),
        (
            environment_hash,
            "environment-session",
            "UPDATE work_run_evidence SET producer_session_id = 'forged-session'
             WHERE evidence_hash = ?1",
            None,
        ),
        (
            environment_hash,
            "environment-producer",
            "UPDATE work_run_evidence SET producer_observation_hash = ?2
             WHERE evidence_hash = ?1",
            Some(producer_hash.as_str()),
        ),
        (
            environment_hash,
            "environment-check",
            "UPDATE work_run_evidence SET check_fingerprint = ?2
             WHERE evidence_hash = ?1",
            Some(verification_hash.as_str()),
        ),
        (
            environment_hash,
            "environment-result",
            "UPDATE work_run_evidence SET verification_result = 'passed'
             WHERE evidence_hash = ?1",
            None,
        ),
        (
            environment_hash,
            "environment-time",
            "UPDATE work_run_evidence SET observed_at_ms = observed_at_ms + 1
             WHERE evidence_hash = ?1",
            None,
        ),
        (
            environment_hash,
            "environment-fingerprint",
            "UPDATE work_run_evidence SET environment_fingerprint = ?2
             WHERE evidence_hash = ?1",
            Some(verification_hash.as_str()),
        ),
        (
            environment_hash,
            "environment-link",
            "UPDATE work_run_evidence SET environment_evidence_hash = ?2
             WHERE evidence_hash = ?1",
            Some(verification_hash.as_str()),
        ),
        (
            environment_hash,
            "environment-components",
            "UPDATE work_run_evidence SET components_json = X'7B7D'
             WHERE evidence_hash = ?1",
            None,
        ),
    ];
    for (evidence_hash, label, sql, second_value) in projection_corruptions {
        store
            .connection
            .execute_batch("SAVEPOINT corrupt_typed_evidence")
            .expect("start typed-evidence corruption savepoint");
        match second_value {
            Some(value) => store
                .connection
                .execute(sql, params![evidence_hash.as_str(), value]),
            None => store.connection.execute(sql, [evidence_hash.as_str()]),
        }
        .unwrap_or_else(|error| panic!("corrupt {label}: {error}"));
        assert!(
            work_evidence_kind_on(&store.connection, run.run_id, evidence_hash).is_err(),
            "{label} remained readable through the lifecycle path"
        );
        assert!(
            ensure_run_evidence(
                &store.connection,
                run.run_id,
                std::slice::from_ref(evidence_hash),
            )
            .is_ok(),
            "{label} lost its run-membership projection"
        );
        let corrupt_report = store
            .verify_all()
            .unwrap_or_else(|error| panic!("verify {label}: {error}"));
        assert!(
            corrupt_report
                .invalid_work_records
                .iter()
                .any(|record| { record == &format!("work_evidence:{evidence_hash}:run_binding") }),
            "{label} was not reported: {corrupt_report:?}"
        );
        store
            .connection
            .execute_batch("ROLLBACK TO corrupt_typed_evidence; RELEASE corrupt_typed_evidence")
            .unwrap_or_else(|error| panic!("restore {label}: {error}"));
    }
    let final_report = store.verify_all().expect("integrity report");
    assert!(final_report.is_healthy(), "{final_report:?}");
}
