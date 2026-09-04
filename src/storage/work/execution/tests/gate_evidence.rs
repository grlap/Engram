use super::*;

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
fn late_finding_marker_is_reserved_for_completed_work_evidence() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("post-completion-marker", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let claim = claim(&mut store, &root, "holder", "claim", 1, 300);
    let mut marked_actor = actor("holder");
    marked_actor.provenance_chain.push(ProvenanceLink {
        relation: ProvenanceRelation::DerivedFrom,
        source: POST_COMPLETION_EVIDENCE_PROVENANCE_SOURCE.into(),
        reference: Some(POST_COMPLETION_EVIDENCE_PROVENANCE_REFERENCE.into()),
    });

    let marked_evidence = store.record_work_evidence(
        &RecordWorkEvidenceRequest {
            work_id: root.work_id,
            run_id: claim.run_id,
            expected_work_revision: root.revision,
            holder: claim.holder.clone(),
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
            summary: "must not masquerade as a late finding".into(),
            refs: Vec::new(),
            actor: marked_actor.clone(),
            idempotency_key: "marked-open-evidence".into(),
            recorded_at: at(2),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(
        marked_evidence,
        Err(StoreError::InvalidWorkProjection(message))
            if message.contains("reserved for evidence on completed work")
    ));

    let marked_checkpoint = store.checkpoint_work(
        &CheckpointWorkRequest {
            work_id: root.work_id,
            run_id: claim.run_id,
            expected_work_revision: root.revision,
            holder: claim.holder.clone(),
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
            summary: "must not carry the late marker".into(),
            evidence: None,
            actor: marked_actor.clone(),
            idempotency_key: "marked-open-checkpoint".into(),
            checkpointed_at: at(2),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(
        marked_checkpoint,
        Err(StoreError::InvalidWorkProjection(message))
            if message.contains("reserved for evidence on completed work")
    ));
    let marked_handoff = store.offer_work_handoff(
        &OfferWorkHandoffRequest {
            work_id: root.work_id,
            run_id: claim.run_id,
            expected_work_revision: root.revision,
            from: claim.holder.clone(),
            to: SessionId("next-holder".into()),
            claim_id: claim.claim_id,
            claim_fence: claim.fence,
            ttl_seconds: 30,
            checkpoint_summary: "must not carry the late marker".into(),
            actor: marked_actor.clone(),
            idempotency_key: "marked-open-handoff".into(),
            offered_at: at(2),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(
        marked_handoff,
        Err(StoreError::InvalidWorkProjection(message))
            if message.contains("reserved for evidence on completed work")
    ));
    assert!(
        store
            .work_run_evidence(claim.run_id)
            .expect("open run evidence")
            .is_empty()
    );
}

#[test]
fn completed_evidence_phase_validator_rejects_corrupt_frozen_basis_bindings() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("completed-evidence-phase", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let claim = claim(&mut store, &root, "holder", "claim", 1, 300);
    let sealed_evidence = evidence(&mut store, &root, &claim, "holder", "sealed-evidence", 2);
    checkpoint(
        &mut store,
        &root,
        &claim,
        "holder",
        "seal-checkpoint",
        3,
        std::slice::from_ref(&sealed_evidence),
    );
    let seal = complete(
        &mut store,
        &root,
        &claim,
        "holder",
        &sealed_evidence,
        "complete",
        4,
    )
    .expect("complete work");
    let completed = store.get_work_item(root.work_id).expect("completed item");
    let mut late_actor = actor("peer");
    late_actor.provenance_chain.push(ProvenanceLink {
        relation: ProvenanceRelation::DerivedFrom,
        source: POST_COMPLETION_EVIDENCE_PROVENANCE_SOURCE.into(),
        reference: Some(POST_COMPLETION_EVIDENCE_PROVENANCE_REFERENCE.into()),
    });
    let capture = store
        .record_work_note(
            &RecordWorkNoteRequest {
                work_id: root.work_id,
                run_id: claim.run_id,
                expected_work_revision: completed.revision,
                holder: SessionId("peer".into()),
                claim_id: seal.claim_id,
                claim_fence: seal.claim_fence,
                summary: "late finding".into(),
                refs: Vec::new(),
                actor: late_actor,
                idempotency_key: "late-note".into(),
                recorded_at: at(5),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("late note");
    let evidence = store
        .get::<WorkEvidence>(&capture.evidence)
        .expect("late evidence read")
        .expect("late evidence");
    let event = canonical_work_events_for_item(&store.connection, root.work_id)
        .expect("work history")
        .into_iter()
        .last()
        .expect("late evidence event");
    validate_work_evidence_event_phase_on(&store.connection, &capture.evidence, &evidence, &event)
        .expect("valid completed evidence basis");

    let mut wrong_actor = event.clone();
    wrong_actor.actor.actor_id = "forged-peer".into();
    assert!(
        validate_work_evidence_event_phase_on(
            &store.connection,
            &capture.evidence,
            &evidence,
            &wrong_actor,
        )
        .is_err()
    );
    let mut before_completion = evidence;
    before_completion.created_at = seal.completed_at - chrono::Duration::seconds(1);
    assert!(
        validate_work_evidence_event_phase_on(
            &store.connection,
            &capture.evidence,
            &before_completion,
            &event,
        )
        .is_err()
    );
}
