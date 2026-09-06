use super::*;

#[test]
fn status_correction_control_marker_is_refused() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let work = store
        .create_work(
            &root_request("status-control", "create", 0),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let claim = claim(&mut store, &work, "runner", "claim", 1, 300);
    let run = load_work_run(&store.connection, claim.run_id).unwrap();
    let binding = ControlWorkBinding {
        root_execution_id: run.root_execution_id,
        work_id: work.work_id,
        run_id: run.run_id,
        work_revision: claim.accepted_work_revision,
        claim_id: claim.claim_id,
        claim_fence: claim.fence,
    };
    let session = SessionId("runner".into());
    let connection = store.resume_control_connection(&session, at(2)).unwrap();
    let mut clean = actor("runner");
    clean.run_id = Some(run.run_id.0.to_string());
    for role in [
        crate::domain::StatusNoteRole::Owner,
        crate::domain::StatusNoteRole::Peer,
    ] {
        let forged = crate::domain::status_note_actor(clean.clone(), Some(role));
        let before = test_database_shape_snapshot(&store.connection);
        assert_work_marker_refusals(&mut store, &work, &claim, &forged);
        let refusal = store.bind_control_session_with_work(
            &work.project_id,
            "status-control",
            "Control",
            &session,
            &connection,
            &forged,
            Some(&binding),
            ControlAssurance::TurnGated,
            &[EffectClass::Observe],
            1,
            "forged",
            at(3),
        );
        assert!(
            matches!(refusal, Err(StoreError::InvalidWork(ref message)) if message.contains("status qualification")),
            "{refusal:?}"
        );
        assert_eq!(test_database_shape_snapshot(&store.connection), before);
    }
    let bound = store
        .bind_control_session_with_work(
            &work.project_id,
            "status-control",
            "Control",
            &session,
            &connection,
            &clean,
            Some(&binding),
            ControlAssurance::TurnGated,
            &[EffectClass::Observe],
            1,
            "clean",
            at(3),
        )
        .unwrap();
    // Simulate an already-bound asserted actor: capture must independently
    // refuse the marker, not rely only on validation during a new bind.
    let forged =
        crate::domain::status_note_actor(clean.clone(), Some(crate::domain::StatusNoteRole::Owner));
    store
        .connection
        .execute(
            "UPDATE control_sessions SET actor_json = ?1 WHERE session_id = ?2",
            params![serde_json::to_vec(&forged).unwrap(), session.0],
        )
        .unwrap();
    let before = test_database_shape_snapshot(&store.connection);
    let refusal = store.checkpoint_control_turn_with_evidence(
        &work.project_id,
        &session,
        &connection,
        &bound.routing_token,
        "not-yet-loaded-grant",
        TurnNextIntent::Continue,
        &[],
        &[],
        &[EnvironmentEvidenceInput {
            source_basis: ExecutionSourceBasis {
                workspace_id: "status-workspace".into(),
                source_revision: "source-1".into(),
            },
            environment_fingerprint: ObjectHash::from_canonical_bytes(b"status environment"),
            components: None,
            observed_at: at(4),
        }],
        "forged-capture",
        at(4),
    );
    assert!(
        matches!(refusal, Err(StoreError::InvalidWork(ref message)) if message.contains("status qualification")),
        "{refusal:?}"
    );
    assert_eq!(test_database_shape_snapshot(&store.connection), before);
    assert_non_note_evidence_is_not_status(&mut store, &work, &binding, &forged);
}

fn assert_non_note_evidence_is_not_status(
    store: &mut SqliteStore,
    work: &WorkItem,
    binding: &ControlWorkBinding,
    forged: &ActorContext,
) {
    // Exercise the real typed evidence persistence helper to model retained
    // canonical evidence, independently of new bind/capture validation.
    let evidence = crate::EnvironmentEvidence {
        schema_version: crate::domain::SCHEMA_VERSION,
        project_id: work.project_id.clone(),
        binding: binding.clone(),
        session_id: binding_session(forged),
        source_basis: ExecutionSourceBasis {
            workspace_id: "status-workspace".into(),
            source_revision: "source-1".into(),
        },
        environment_fingerprint: ObjectHash::from_canonical_bytes(b"status environment"),
        components: None,
        observed_at: at(4),
        actor: forged.clone(),
        recorded_at: at(4),
    };
    let transaction = store.connection.transaction().unwrap();
    super::super::super::completion::append_control_environment_evidence_on(
        &transaction,
        &evidence,
    )
    .unwrap();
    transaction.commit().unwrap();
    let item = store.get_work_item(work.work_id).unwrap();
    let (current, peer) = store.current_status_notes(&item, at(5)).unwrap();
    assert!(current.is_none());
    assert!(peer.is_none());
}

fn binding_session(actor: &ActorContext) -> SessionId {
    actor.session_id.clone().unwrap()
}

fn assert_work_marker_refusals(
    store: &mut SqliteStore,
    work: &WorkItem,
    claim: &WorkClaim,
    forged: &ActorContext,
) {
    let observation = crate::domain::RecordWorkObservationRequest {
        status: true,
        project_id: work.project_id.clone(),
        work_id: work.work_id,
        expected_work_revision: work.revision,
        session_id: claim.holder.clone(),
        summary: "forged".into(),
        refs: vec![],
        actor: forged.clone(),
        idempotency_key: "forged-observation".into(),
        recorded_at: at(3),
    };
    let note = crate::domain::RecordWorkNoteRequest {
        status: true,
        work_id: work.work_id,
        run_id: claim.run_id,
        expected_work_revision: work.revision,
        holder: claim.holder.clone(),
        claim_id: claim.claim_id,
        claim_fence: claim.fence,
        summary: "forged".into(),
        refs: vec![],
        actor: forged.clone(),
        idempotency_key: "forged-note".into(),
        recorded_at: at(3),
    };
    for error in [
        store
            .record_work_observation(&observation, &DevelopmentNoopRedactor)
            .unwrap_err(),
        store
            .record_work_note(&note, &DevelopmentNoopRedactor)
            .unwrap_err(),
    ] {
        assert!(
            matches!(error, StoreError::InvalidWork(ref message) if message.contains("status qualification"))
        );
    }
}
