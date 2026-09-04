use super::super::session::{
    MAX_PROCESS_DEFAULT_SESSION_RECLAIMS_PER_CREATION,
    PROCESS_DEFAULT_SESSION_RECLAMATION_CANDIDATES_SQL,
};
use super::super::test_support::*;
use super::super::*;
use super::*;

#[test]
fn inactive_process_default_sessions_are_reclaimed_atomically_without_live_authority() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("process-default-retention-project".into());
    let cleanup_second = crate::storage::PROCESS_DEFAULT_WORK_SESSION_RETENTION_SECONDS + 100;
    let expired = process_default_session_at(10, at(0));
    let recent = process_default_session_at(11, at(cleanup_second - 10));
    let legacy = SessionId(format!("local-process-12-{}", uuid::Uuid::new_v4()));
    let live = SessionId(format!("local-process-13-{}", uuid::Uuid::new_v4()));
    let handoff = process_default_session_at(14, at(0));
    let bound = process_default_session_at(15, at(0));
    let pending = process_default_session_at(16, at(0));
    let staged = process_default_session_at(17, at(0));
    let stable = SessionId("stable-host-session".into());
    let mut first = SqliteStore::open(&database).expect("first store");
    let binder = SessionId("retention-binder".into());
    first
        .start_task(
            &project,
            "retention-task",
            "Retention task",
            &binder,
            actor(&binder.0),
            at(0),
        )
        .expect("task session bind");
    first
        .join_task(&project, "retention-task", &bound, actor(&bound.0), at(0))
        .expect("explicit process session binding");
    let root = first
        .create_work(
            &root_request(&project.0, "retention-root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root work");
    let live_claim = claim(
        &mut first,
        &root,
        &live.0,
        "live-claim",
        cleanup_second - 10,
        1_000,
    );
    first
        .offer_work_handoff(
            &OfferWorkHandoffRequest {
                work_id: root.work_id,
                run_id: live_claim.run_id,
                expected_work_revision: root.revision,
                from: live.clone(),
                to: handoff.clone(),
                claim_id: live_claim.claim_id,
                claim_fence: live_claim.fence,
                ttl_seconds: 1_000,
                checkpoint_summary: "retain the open handoff".into(),
                actor: actor(&live.0),
                idempotency_key: "retention-handoff".into(),
                offered_at: at(cleanup_second - 9),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("open handoff");
    for (index, (session, seen_at)) in [
        (&expired, at(0)),
        (&recent, at(cleanup_second - 10)),
        (&legacy, at(0)),
        (&live, at(0)),
        (&handoff, at(0)),
        (&bound, at(0)),
        (&pending, at(0)),
        (&staged, at(0)),
        (&stable, at(0)),
    ]
    .into_iter()
    .enumerate()
    {
        first
            .connection
            .execute(
                "INSERT INTO work_session_state (
                     project_id, session_id, focused_work_id, project_cursor, updated_at_ms
                 ) VALUES (?1, ?2, NULL, 0, ?3)",
                params![project.0, session.0, seen_at.timestamp_millis()],
            )
            .expect("session state");
        first
            .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
                project_id: &project,
                session_id: session,
                operation: "work_update:gate",
                idempotency_key: &format!("attempt-{index}"),
                intent: &serde_json::json!({"name": "retention"}),
                basis: &serde_json::json!({"revision": root.revision}),
                now: seen_at,
            })
            .expect("protocol attempt");
        if *session != pending {
            first
                .finish_work_protocol_attempt(
                    &project,
                    session,
                    "work_update:gate",
                    &format!("attempt-{index}"),
                    &serde_json::json!({"receipt": {"work_id": root.work_id}}),
                )
                .expect("completed protocol attempt");
        }
    }
    let staged_payload =
        CanonicalObject::freeze(&serde_json::json!({"staged": true})).expect("staged payload");
    first
        .connection
        .execute(
            "UPDATE work_session_state SET
                 tentative_project_cursor = 0,
                 tentative_delivery_token = 'retained-delivery-token',
                 tentative_delivery_payload_hash = ?3,
                 tentative_delivery_payload = ?4
             WHERE project_id = ?1 AND session_id = ?2",
            params![
                project.0,
                staged.0,
                staged_payload.hash().as_str(),
                staged_payload.bytes()
            ],
        )
        .expect("stage an unconfirmed delivery");
    let bulk_expired = MAX_PROCESS_DEFAULT_SESSION_RECLAIMS_PER_CREATION + 6;
    for index in 0..bulk_expired {
        let session = process_default_session_at(
            1_000 + u32::try_from(index).expect("bounded test pid"),
            at(0),
        );
        first
            .connection
            .execute(
                "INSERT INTO work_session_state (
                     project_id, session_id, focused_work_id, project_cursor, updated_at_ms
                 ) VALUES (?1, ?2, NULL, 0, ?3)",
                params![project.0, session.0, at(0).timestamp_millis()],
            )
            .expect("bulk expired session state");
    }
    let second = SqliteStore::open(&database).expect("second store");

    let first_creator = process_default_session_at(20, at(cleanup_second));
    assert!(
        first
            .initialize_process_default_work_session(&project, &first_creator, at(cleanup_second))
            .expect("session creation triggers bounded reclamation")
    );
    assert_eq!(
        second
            .connection
            .query_row(
                "SELECT COUNT(*) FROM work_session_state WHERE project_id = ?1",
                [project.0.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .expect("remaining session states"),
        i64::try_from(bulk_expired + 10 - MAX_PROCESS_DEFAULT_SESSION_RECLAIMS_PER_CREATION)
            .expect("bounded remaining count")
    );
    assert!(
        !first
            .initialize_process_default_work_session(
                &project,
                &first_creator,
                at(cleanup_second + 1)
            )
            .expect("an existing session never scans reclamation again")
    );
    let second_creator = process_default_session_at(21, at(cleanup_second + 1));
    assert!(
        first
            .initialize_process_default_work_session(
                &project,
                &second_creator,
                at(cleanup_second + 1)
            )
            .expect("second session creation drains the next bounded page")
    );
    assert_eq!(
        second
            .connection
            .query_row(
                "SELECT COUNT(*) FROM work_session_state WHERE project_id = ?1",
                [project.0.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .expect("retained session states"),
        9
    );
    assert_eq!(
        second
            .connection
            .query_row(
                "SELECT COUNT(*) FROM work_protocol_attempts WHERE project_id = ?1",
                [project.0.as_str()],
                |row| row.get::<_, i64>(0),
            )
            .expect("retained protocol attempts"),
        7
    );
    for retained in [
        &recent,
        &live,
        &handoff,
        &bound,
        &pending,
        &staged,
        &stable,
        &first_creator,
        &second_creator,
    ] {
        let count = second
            .connection
            .query_row(
                "SELECT COUNT(*) FROM work_session_state
                 WHERE project_id = ?1 AND session_id = ?2",
                params![project.0, retained.0],
                |row| row.get::<_, i64>(0),
            )
            .expect("retained session state");
        assert_eq!(count, 1, "session {} remains", retained.0);
    }

    let explain =
        format!("EXPLAIN QUERY PLAN {PROCESS_DEFAULT_SESSION_RECLAMATION_CANDIDATES_SQL}");
    let process_default_glob = format!(
        "{}*",
        crate::storage::PROCESS_DEFAULT_WORK_SESSION_NAMESPACE
    );
    let mut statement = first
        .connection
        .prepare(&explain)
        .expect("prepare retention candidate plan");
    let plan = statement
        .query_map(
            params![
                project.0,
                at(1).timestamp_millis(),
                process_default_glob,
                first_creator.0,
                at(cleanup_second).timestamp_millis(),
                i64::try_from(MAX_PROCESS_DEFAULT_SESSION_RECLAIMS_PER_CREATION)
                    .expect("bounded reclamation limit")
            ],
            |row| row.get::<_, String>(3),
        )
        .expect("explain retention candidates")
        .collect::<Result<Vec<_>, _>>()
        .expect("retention candidate plan");
    drop(statement);
    for required_index in [
        "work_session_state_retention",
        "work_claims_holder_live",
        "work_handoff_offer_from_live",
        "work_handoff_offer_to_live",
    ] {
        assert!(
            plan.iter().any(|detail| detail.contains(required_index)),
            "retention plan does not use {required_index}: {plan:?}"
        );
    }
    assert!(
        plan.iter()
            .all(|detail| !detail.contains("USE TEMP B-TREE")),
        "retention candidate discovery sorts through a temporary B-tree: {plan:?}"
    );

    let rollback_candidate = process_default_session_at(30, at(0));
    first
        .connection
        .execute(
            "INSERT INTO work_session_state (
                 project_id, session_id, focused_work_id, project_cursor, updated_at_ms
             ) VALUES (?1, ?2, NULL, 0, ?3)",
            params![project.0, rollback_candidate.0, at(0).timestamp_millis()],
        )
        .expect("rollback candidate");
    let rollback_creator = process_default_session_at(31, at(cleanup_second + 2));
    first
        .connection
        .execute_batch(&format!(
            "CREATE TEMP TRIGGER refuse_retention_creator
             BEFORE INSERT ON work_session_state
             WHEN NEW.session_id = '{}'
             BEGIN
                 SELECT RAISE(ABORT, 'refuse creator after reclamation');
             END;",
            rollback_creator.0
        ))
        .expect("install rollback trigger");
    assert!(
        first
            .initialize_process_default_work_session(
                &project,
                &rollback_creator,
                at(cleanup_second + 2)
            )
            .is_err(),
        "a refused creator must abort the reclamation transaction"
    );
    first
        .connection
        .execute_batch("DROP TRIGGER refuse_retention_creator;")
        .expect("drop rollback trigger");
    for (session, expected) in [(&rollback_candidate, 1_i64), (&rollback_creator, 0_i64)] {
        assert_eq!(
            second
                .connection
                .query_row(
                    "SELECT COUNT(*) FROM work_session_state
                     WHERE project_id = ?1 AND session_id = ?2",
                    params![project.0, session.0],
                    |row| row.get::<_, i64>(0),
                )
                .expect("rollback state count"),
            expected,
            "rollback preserves prior rows and refuses the creator"
        );
    }
    assert!(second.verify_all().expect("integrity report").is_healthy());
}

#[test]
fn prerelease_agent_grant_schema_is_refused_as_a_different_build() {
    let directory = tempfile::tempdir().expect("temporary schema fixture");
    let database = directory.path().join("engram.sqlite3");
    drop(SqliteStore::open(&database).expect("create current store"));
    let connection = Connection::open(&database).expect("open schema fixture");
    connection
        .execute(
            "CREATE TABLE work_authority_grants (obsolete TEXT NOT NULL)",
            [],
        )
        .expect("inject prerelease grant table");
    drop(connection);

    let Err(error) = SqliteStore::open(&database) else {
        panic!("obsolete grant schema must refuse");
    };
    assert!(
        error.to_string().contains("different Engram build"),
        "unexpected refusal: {error}"
    );
}

#[test]
fn latest_gate_observation_uses_indexed_canonical_run_history() {
    let project = "canonical-gate-history";
    let mut store = SqliteStore::open_in_memory().expect("store");
    let work = store
        .create_work(
            &root_request(project, "canonical-gate-root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("gate work");
    let claim = claim(
        &mut store,
        &work,
        "gate-agent",
        "canonical-gate-claim",
        1,
        10_000,
    );
    let mut hashes = Vec::new();
    for index in 0..128_i64 {
        let failed = (index % 2 != 0).then(|| vec!["cargo-test".into()]);
        hashes.push(
            store
                .record_gate_evidence(
                    &RecordGateEvidenceRequest {
                        work_id: work.work_id,
                        run_id: claim.run_id,
                        expected_work_revision: work.revision,
                        holder: claim.holder.clone(),
                        claim_id: claim.claim_id,
                        claim_fence: claim.fence,
                        name: "cargo-test".into(),
                        failed: failed.unwrap_or_default(),
                        evidence_ref: None,
                        actor: actor("gate-agent"),
                        recorded_at: at(index + 2),
                    },
                    &DevelopmentNoopRedactor,
                )
                .expect("alternating gate observation"),
        );
    }
    assert!(hashes.windows(2).all(|pair| pair[0] != pair[1]));
    for (index, hash) in hashes.iter().enumerate() {
        let evidence = store
            .get::<WorkEvidence>(hash)
            .expect("gate evidence read")
            .expect("gate evidence");
        assert_eq!(
            evidence.gate.expect("typed gate").previous.as_ref(),
            index.checked_sub(1).map(|previous| &hashes[previous])
        );
    }
    let latest = latest_gate_evidence_on(&store.connection, claim.run_id, "cargo-test")
        .expect("latest canonical gate observation")
        .expect("gate observation");
    assert_eq!(latest.0, *hashes.last().expect("last gate hash"));

    let explain = format!("EXPLAIN QUERY PLAN {LATEST_GATE_EVIDENCE_SQL}");
    let mut statement = store
        .connection
        .prepare(&explain)
        .expect("prepare gate lookup plan");
    let plan = statement
        .query_map(params![claim.run_id.0.to_string(), "cargo-test"], |row| {
            row.get::<_, String>(3)
        })
        .expect("explain gate lookup")
        .collect::<Result<Vec<_>, _>>()
        .expect("collect gate lookup plan");
    assert!(
        plan.iter().any(|detail| {
            detail.contains("SEARCH object USING INDEX objects_work_evidence_gate_name")
        }),
        "gate lookup does not search its expression index: {plan:?}"
    );
    assert!(
        plan.iter().all(|detail| !detail.contains("SCAN ")),
        "gate lookup contains an unbounded scan: {plan:?}"
    );
    assert!(
        plan.iter()
            .all(|detail| !detail.contains("USE TEMP B-TREE")),
        "gate lookup sorts through a temporary B-tree: {plan:?}"
    );
    assert!(
        store
            .verify_all()
            .expect("gate history integrity")
            .is_healthy()
    );
    let mutable_head_exists = store
        .connection
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM sqlite_master
                 WHERE name = 'work_gate_evidence_heads'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )
        .expect("gate head absence");
    assert!(!mutable_head_exists);
    let expression_index_exists = store
        .connection
        .query_row(
            "SELECT EXISTS(
                 SELECT 1 FROM sqlite_master
                 WHERE name = 'objects_work_evidence_gate_name'
                   AND type = 'index'
             )",
            [],
            |row| row.get::<_, bool>(0),
        )
        .expect("gate expression index");
    assert!(expression_index_exists);
}

#[test]
fn gate_replay_never_reuses_another_actors_attribution() {
    let project = "gate-actor-attribution";
    let holder = "shared-session";
    let mut store = SqliteStore::open_in_memory().expect("gate actor fixture");
    let work = store
        .create_work(
            &root_request(project, "gate-actor-root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("gate actor root");
    let claim = claim(&mut store, &work, holder, "gate-actor-claim", 1, 300);
    let project_id = crate::domain::ProjectId(project.into());
    let session_id = SessionId(holder.into());
    let basis = serde_json::json!({"test_basis": work.work_id});
    let actor_a = actor(holder);
    let failed = Vec::<String>::new();
    let refs = Vec::<String>::new();
    let pending_intent = GateWorkProtocolIntent {
        schema_version: SCHEMA_VERSION,
        project_id: &project_id,
        session_id: &session_id,
        actor: &actor_a,
        work_id: work.work_id,
        run_id: claim.run_id,
        claim_id: claim.claim_id,
        claim_fence: claim.fence,
        name: "cargo-test",
        failed: &failed,
        refs: &refs,
        previous: None,
    };
    let pending_object = CanonicalObject::freeze(&pending_intent).expect("actor A intent");
    let actor_a_key = format!("gate:{}", pending_object.hash().as_str());
    assert!(
        store
            .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
                project_id: &project_id,
                session_id: &session_id,
                operation: "work_update:gate",
                idempotency_key: &actor_a_key,
                intent: &pending_intent,
                basis: &basis,
                now: at(2),
            })
            .expect("reserve actor A attempt")
            .result
            .is_none()
    );

    let mut actor_b = actor(holder);
    actor_b.actor_id = "different-actor".into();
    actor_b.source_tool = Some("different-tool".into());
    let actor_b_request = RecordGateEvidenceRequest {
        work_id: work.work_id,
        run_id: claim.run_id,
        expected_work_revision: work.revision,
        holder: session_id.clone(),
        claim_id: claim.claim_id,
        claim_fence: claim.fence,
        name: "cargo-test".into(),
        failed: Vec::new(),
        evidence_ref: None,
        actor: actor_b.clone(),
        recorded_at: at(2),
    };
    let actor_b_attempt = store
        .record_gate_evidence_protocol(
            &actor_b_request,
            &BeginGateWorkProtocolAttempt {
                project_id: &project_id,
                session_id: &session_id,
                basis: &basis,
                now: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("actor B records its own gate observation");
    assert_ne!(actor_b_attempt.idempotency_key, actor_a_key);
    let different_attribution = store
        .get::<WorkEvidence>(&actor_b_attempt.evidence)
        .expect("actor B evidence read")
        .expect("actor B evidence");
    assert_eq!(different_attribution.actor, actor_b);
    assert_eq!(
        different_attribution
            .gate
            .as_ref()
            .expect("actor B gate")
            .previous,
        None
    );

    let actor_a_hash = store
        .record_gate_evidence(
            &RecordGateEvidenceRequest {
                actor: actor_a.clone(),
                recorded_at: at(3),
                ..actor_b_request
            },
            &DevelopmentNoopRedactor,
        )
        .expect("actor A records a distinct attributed observation");
    assert_ne!(actor_a_hash, actor_b_attempt.evidence);
    let original_attribution = store
        .get::<WorkEvidence>(&actor_a_hash)
        .expect("actor A evidence read")
        .expect("actor A evidence");
    assert_eq!(original_attribution.actor, actor_a);
    assert_eq!(
        original_attribution
            .gate
            .expect("actor A gate")
            .previous
            .as_ref(),
        Some(&actor_b_attempt.evidence)
    );
}

#[test]
fn gate_replay_ignores_optional_actor_context_but_keeps_original_attribution() {
    let project = "gate-context-replay";
    let holder = "gate-context-session";
    let mut store = SqliteStore::open_in_memory().expect("gate context fixture");
    let work = store
        .create_work(
            &root_request(project, "gate context root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("gate context root");
    let claim = claim(&mut store, &work, holder, "gate-context-claim", 1, 300);
    let project_id = ProjectId(project.into());
    let session_id = SessionId(holder.into());
    let basis = serde_json::json!({"test_basis": work.work_id});
    let mut actor_a = actor(holder);
    actor_a.provenance_chain.push(ProvenanceLink {
        relation: ProvenanceRelation::DerivedFrom,
        source: "model=first".into(),
        reference: Some(crate::domain::ACTOR_CONTEXT_PROVENANCE_REFERENCE.into()),
    });
    let request = RecordGateEvidenceRequest {
        work_id: work.work_id,
        run_id: claim.run_id,
        expected_work_revision: work.revision,
        holder: session_id.clone(),
        claim_id: claim.claim_id,
        claim_fence: claim.fence,
        name: "cargo-test".into(),
        failed: Vec::new(),
        evidence_ref: None,
        actor: actor_a.clone(),
        recorded_at: at(2),
    };
    let first = store
        .record_gate_evidence_protocol(
            &request,
            &BeginGateWorkProtocolAttempt {
                project_id: &project_id,
                session_id: &session_id,
                basis: &basis,
                now: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("first gate observation");

    let mut actor_b = actor(holder);
    actor_b.provenance_chain.push(ProvenanceLink {
        relation: ProvenanceRelation::DerivedFrom,
        source: "model=second".into(),
        reference: Some(crate::domain::ACTOR_CONTEXT_PROVENANCE_REFERENCE.into()),
    });
    let retry = RecordGateEvidenceRequest {
        actor: actor_b,
        recorded_at: at(3),
        ..request
    };
    let second = store
        .record_gate_evidence_protocol(
            &retry,
            &BeginGateWorkProtocolAttempt {
                project_id: &project_id,
                session_id: &session_id,
                basis: &basis,
                now: at(3),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("context-only retry");

    assert_eq!(second.idempotency_key, first.idempotency_key);
    assert_eq!(second.evidence, first.evidence);
    assert_eq!(
        store
            .work_run_evidence(claim.run_id)
            .expect("one gate observation"),
        vec![first.evidence.clone()]
    );
    assert_eq!(
        store
            .get::<WorkEvidence>(&first.evidence)
            .expect("gate evidence read")
            .expect("gate evidence")
            .actor,
        actor_a
    );
}

#[test]
fn gate_integrity_requires_a_valid_payload_and_prior_same_name_head() {
    let work_id = WorkId(uuid::Uuid::now_v7());
    let run_id = WorkRunId(uuid::Uuid::now_v7());
    let claim_id = WorkClaimId(uuid::Uuid::now_v7());
    let make = |previous: Option<ObjectHash>, passed: bool| WorkEvidence {
        schema_version: SCHEMA_VERSION,
        work_id,
        run_id,
        claim_id,
        claim_fence: 1,
        summary: GATE_EVIDENCE_SUMMARY.into(),
        refs: Vec::new(),
        gate: Some(GateEvidenceRecord {
            schema_version: SCHEMA_VERSION,
            name: "cargo-test".into(),
            passed,
            failed: Vec::new(),
            previous,
        }),
        actor: actor("gate-integrity"),
        created_at: at(1),
    };

    let first = make(None, true);
    let first_hash = CanonicalObject::freeze(&first)
        .expect("first gate object")
        .hash()
        .clone();
    let mut heads = HashMap::new();
    validate_gate_evidence_chain(&first_hash, &first, &mut heads)
        .expect("first normalized gate starts the chain");

    let mut alternate_summary = first.clone();
    alternate_summary.summary = "human rendering may evolve".into();
    validate_gate_evidence_payload(&alternate_summary)
        .expect("the typed gate field, not a summary literal, is the discriminator");

    let second = make(Some(first_hash.clone()), true);
    let second_hash = CanonicalObject::freeze(&second)
        .expect("second gate object")
        .hash()
        .clone();
    validate_gate_evidence_chain(&second_hash, &second, &mut heads)
        .expect("the exact prior head advances the chain");

    let dangling_target = CanonicalObject::freeze(&"dangling")
        .expect("dangling target")
        .hash()
        .clone();
    let dangling = make(Some(dangling_target), true);
    let dangling_hash = CanonicalObject::freeze(&dangling)
        .expect("dangling gate object")
        .hash()
        .clone();
    assert!(validate_gate_evidence_chain(&dangling_hash, &dangling, &mut heads).is_err());

    let malformed = make(Some(second_hash), false);
    let malformed_hash = CanonicalObject::freeze(&malformed)
        .expect("malformed gate object")
        .hash()
        .clone();
    assert!(validate_gate_evidence_chain(&malformed_hash, &malformed, &mut heads).is_err());
}

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
                expected_run_id: root.active_run_id.expect("active run"),
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
            expected_run_id: root.active_run_id.expect("active run"),
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
            expected_run_id: root.active_run_id.expect("active run"),
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
            expected_run_id: root.active_run_id.expect("active run"),
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
        expected_run_id: active.run_id,
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
                expected_run_id: blocked.active_run_id.expect("active run"),
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
            expected_run_id: blocked_claim.run_id,
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
                expected_run_id: deferred.active_run_id.expect("active run"),
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
            expected_run_id: deferred_claim.run_id,
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
        expected_run_id: initial.run_id,
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
                expected_run_id: active.run_id,
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
                expected_run_id: renewed.run_id,
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
                expected_run_id: root.active_run_id.expect("active run"),
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

#[test]
fn expired_handoff_is_audited_and_does_not_block_a_new_offer() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("project-expiry", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let initial = claim(&mut store, &root, "agent-a", "claim-a", 1, 10);
    let expired_offer = store
        .offer_work_handoff(
            &OfferWorkHandoffRequest {
                work_id: root.work_id,
                run_id: initial.run_id,
                expected_work_revision: root.revision,
                from: initial.holder.clone(),
                to: SessionId("agent-b".into()),
                claim_id: initial.claim_id,
                claim_fence: initial.fence,
                ttl_seconds: 10,
                checkpoint_summary: "handoff that will expire".into(),
                actor: actor("agent-a"),
                idempotency_key: "first-offer".into(),
                offered_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("first offer");
    let renewed = store
        .current_work_claim(root.work_id)
        .expect("renewed claim")
        .expect("current holder remains authoritative");
    let replacement = store
        .offer_work_handoff(
            &OfferWorkHandoffRequest {
                work_id: root.work_id,
                run_id: renewed.run_id,
                expected_work_revision: root.revision,
                from: renewed.holder.clone(),
                to: SessionId("agent-d".into()),
                claim_id: renewed.claim_id,
                claim_fence: renewed.fence,
                ttl_seconds: 20,
                checkpoint_summary: "replacement handoff".into(),
                actor: actor("agent-a"),
                idempotency_key: "replacement-offer".into(),
                offered_at: at(13),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("replacement offer");
    assert_ne!(replacement.offer_id, expired_offer.offer_id);
    let expired_state = store
        .connection
        .query_row(
            "SELECT state FROM work_handoff_offers WHERE offer_id = ?1",
            [expired_offer.offer_id.0.to_string()],
            |row| row.get::<_, String>(0),
        )
        .expect("expired state");
    assert_eq!(expired_state, "expired");
    let expired_events = store
        .work_feed_after(&FeedId::RunExecution(initial.run_id), 0, 100)
        .expect("run feed")
        .into_iter()
        .filter(|entry| entry.object_kind == "work_event")
        .filter_map(|entry| {
            load_typed_work_object::<WorkEvent>(&store.connection, &entry.object_hash, "work_event")
                .ok()
        })
        .filter(|event| matches!(event.transition, WorkTransition::HandoffExpired { .. }))
        .count();
    assert_eq!(expired_events, 1);
}

#[test]
fn outgoing_holder_can_cancel_a_handoff_and_resume_progress() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("project-cancel", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let claim = claim(&mut store, &root, "agent-a", "claim", 1, 100);
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
                ttl_seconds: 30,
                checkpoint_summary: "possible transfer".into(),
                actor: actor("agent-a"),
                idempotency_key: "offer".into(),
                offered_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("offer");
    let cancelled = store
        .cancel_work_handoff(
            &CancelWorkHandoffRequest {
                work_id: root.work_id,
                run_id: claim.run_id,
                expected_work_revision: root.revision,
                holder: claim.holder.clone(),
                offer_id: offer.offer_id,
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                reason: "  destination did not accept  ".into(),
                actor: actor("agent-a"),
                idempotency_key: "cancel".into(),
                cancelled_at: at(3),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("cancel");
    assert_eq!(cancelled.state, WorkHandoffState::Cancelled);
    let cancelled_entry = store
        .work_event_tail(root.work_id, 1)
        .expect("handoff cancellation event")
        .pop()
        .expect("handoff cancellation tail");
    let cancelled_event: WorkEvent = load_typed_work_object(
        &store.connection,
        &cancelled_entry.object_hash,
        "work_event",
    )
    .expect("canonical handoff cancellation event");
    assert!(matches!(
        cancelled_event.transition,
        WorkTransition::HandoffCancelled { reason, .. }
            if reason == "destination did not accept"
    ));
    evidence(&mut store, &root, &claim, "agent-a", "resumed-evidence", 4);
}

#[test]
fn foreign_holder_cannot_commit_an_expired_handoff_sweep() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("project-cancel-auth", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let claim = claim(&mut store, &root, "agent-a", "claim", 1, 100);
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
                checkpoint_summary: "short transfer window".into(),
                actor: actor("agent-a"),
                idempotency_key: "offer".into(),
                offered_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("offer");
    let events_before = store
        .work_event_tail(root.work_id, 100)
        .expect("event tail")
        .len();

    let refused = store.cancel_work_handoff(
        &CancelWorkHandoffRequest {
            work_id: root.work_id,
            run_id: claim.run_id,
            expected_work_revision: root.revision,
            holder: SessionId("intruder".into()),
            offer_id: offer.offer_id,
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
            reason: "expire another holder's offer".into(),
            actor: actor("intruder"),
            idempotency_key: "foreign-cancel".into(),
            cancelled_at: at(5),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(refused, Err(StoreError::InvalidWork(_))));
    let state: String = store
        .connection
        .query_row(
            "SELECT state FROM work_handoff_offers WHERE offer_id = ?1",
            [offer.offer_id.0.to_string()],
            |row| row.get(0),
        )
        .expect("offer state");
    assert_eq!(state, "offered");
    assert_eq!(
        store
            .work_event_tail(root.work_id, 100)
            .expect("event tail")
            .len(),
        events_before
    );
}

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
