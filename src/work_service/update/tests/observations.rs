use super::*;
use crate::domain::{NON_HOLDER_NOTE_SOURCE, WorkObservation};

mod closure;

fn service(database: &std::path::Path, session: &str) -> LocalWorkService {
    LocalWorkService::new(
        database.to_owned(),
        ProjectId("observation-test".into()),
        "shared-actor".into(),
        SessionId(session.into()),
        None,
    )
}

fn claim(service: &LocalWorkService, work_ref: &str, second: i64) {
    service
        .work_update_on(
            Some(work_ref),
            WorkUpdateInput::Claim {
                ttl_seconds: Some(120),
                recovery_reason: None,
                idempotency_key: String::new(),
            },
            at(second),
        )
        .expect("claim work");
}

fn execution_inventory(database: &std::path::Path) -> Vec<(String, String, String)> {
    let connection = rusqlite::Connection::open(database).unwrap();
    connection.prepare("SELECT 'item', work_id, hex(item_json) FROM work_items
        UNION ALL SELECT 'run', run_id, hex(run_json) FROM work_runs
        UNION ALL SELECT 'root', root_execution_id, hex(header_json) || head_id FROM work_root_executions
        UNION ALL SELECT 'root_member', root_execution_id || member_hash, hex(member_json) FROM work_root_members
        UNION ALL SELECT 'claim', run_id, hex(claim_json) FROM work_claims
        UNION ALL SELECT 'evidence', evidence_id, run_id FROM work_run_evidence
        UNION ALL SELECT 'run_feed', feed_id, CAST(position AS TEXT) FROM work_feed_heads WHERE feed_kind = 'run_execution'
        UNION ALL SELECT object_kind, object_id, '' FROM objects
            WHERE object_kind IN ('work_event', 'work_root_delta', 'work_checkpoint', 'work_evidence', 'completion_seal')
        ORDER BY 1, 2, 3").unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap()
        .collect::<Result<Vec<_>, _>>().unwrap()
}

#[test]
fn phoenix_non_holder_notes_preserve_execution_and_replay_on_every_open_shape() {
    for shape in ["unclaimed", "peer-held", "blocked", "expired"] {
        let directory = crate::test_support::temp_home().unwrap();
        let database = directory.path().join("work.db");
        let owner = service(&database, "holder");
        let reviewer = service(&database, "reviewer");
        let root = proposed_root(owner.work_propose(root_input(shape, shape), at(0)).unwrap());
        if shape != "unclaimed" {
            claim(&owner, &root.short_ref, 1);
        }
        if shape == "blocked" {
            owner
                .work_update(
                    WorkUpdateInput::Block {
                        blocker_kind: WorkBlockerKind::Manual,
                        detail: "awaiting an external answer".into(),
                        idempotency_key: "block".into(),
                    },
                    at(2),
                )
                .unwrap();
        }
        let now = if shape == "expired" { 4_000 } else { 3 };
        let before = execution_inventory(&database);
        let first = reviewer
            .work_note_on(
                Some(&root.short_ref),
                "review finding",
                &["review:detail".into()],
                at(now),
            )
            .unwrap();
        let repeated = reviewer
            .work_note_on(
                Some(&root.short_ref),
                "review finding",
                &["review:detail".into()],
                at(now + 1),
            )
            .unwrap();
        assert!(first.non_holder);
        assert_eq!(first.receipt.result, first.evidence.result);
        assert_eq!(
            serde_json::to_value(&first).unwrap(),
            serde_json::to_value(&repeated).unwrap()
        );
        assert_eq!(execution_inventory(&database), before, "{shape}");
        let store = SqliteStore::open(&database).unwrap();
        let (count, observations) = store.work_observation_tail(root.work_id, 8).unwrap();
        assert_eq!(count, 1);
        assert_eq!(observations.len(), 1);
        assert_eq!(
            observations[0].1.actor.session_id,
            Some(SessionId("reviewer".into()))
        );
        assert!(
            observations[0]
                .1
                .actor
                .provenance_chain
                .iter()
                .any(|link| link.source == NON_HOLDER_NOTE_SOURCE)
        );
        let focus = reviewer.work_focus(&root.short_ref, at(now + 2)).unwrap();
        assert!(
            focus
                .allowed_next
                .iter()
                .any(|action| action == "work_update:note")
        );
        assert!(
            focus
                .evidence_items
                .iter()
                .any(|note| note.non_holder && note.summary == "review finding")
        );
        let invalid = store.verify_all().unwrap().invalid_work_records;
        assert!(invalid.is_empty(), "{shape}: {invalid:?}");
    }
}

#[test]
fn phoenix_note_under_completed_parent_survives_snapshot_and_rebuild() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("source.db");
    let owner = service(&database, "holder");
    let reviewer = service(&database, "reviewer");
    let root = proposed_root(
        owner
            .work_propose(root_input("Parent", "parent"), at(0))
            .unwrap(),
    );
    let WorkProposeResult::Decomposition(children) = owner
        .work_propose(
            WorkProposeInput::Decompose {
                children: vec![WorkChildInput {
                    acceptance_bindings: Vec::new(),
                    evaluation_mode: None,
                    external_ref: None,
                    notes: Vec::new(),
                    key: "optional".into(),
                    title: "Review later".into(),
                    outcome: "reviewed".into(),
                    acceptance: vec!["review recorded".into()],
                    requirement: Some(ChildRequirement::Optional),
                    kind: None,
                    priority: None,
                    labels: Vec::new(),
                    assigned_to: None,
                    deferred_until: None,
                }],
                prerequisites: Vec::new(),
                idempotency_key: "child".into(),
            },
            at(1),
        )
        .unwrap()
    else {
        panic!("decomposition");
    };
    let child = &children.children[0];
    claim(&owner, &root.short_ref, 2);
    assert!(matches!(
        owner
            .work_complete(completion_input("parent delivered", "done"), at(3))
            .unwrap(),
        WorkCompleteResult::Completed(_)
    ));
    assert_eq!(
        reviewer
            .work_focus(&child.short_ref, at(4))
            .unwrap()
            .status
            .availability,
        WorkAvailability::Blocked
    );
    let before = execution_inventory(&database);
    let result = reviewer
        .work_note_on(
            Some(&child.short_ref),
            "parent is closed; observation only",
            &[],
            at(4),
        )
        .unwrap();
    assert_eq!(execution_inventory(&database), before);
    let hash: ObjectId = serde_json::from_value(result.evidence.result).unwrap();
    let store = SqliteStore::open(&database).unwrap();
    let recorded: WorkObservation = store.get(&hash).unwrap().unwrap();
    assert_eq!(recorded.work_id, child.work_id);
    let snapshot = reviewer
        .save_work_graph_snapshot(None, WorkGraphSnapshotDestinationKind::Stdout, at(5))
        .unwrap();
    let destination_path = directory.path().join("destination.db");
    let restored = service(&destination_path, "restore-reader");
    restored
        .load_work_graph_snapshot(
            &serde_json::to_vec(&snapshot.document).unwrap(),
            false,
            at(6),
        )
        .unwrap();
    let focus = restored.work_focus(&child.short_ref, at(7)).unwrap();
    assert!(focus.restored_history.items.iter().any(|note| {
        note.summary.contains("observation only")
            && note
                .actor
                .provenance_chain
                .iter()
                .any(|link| link.source == NON_HOLDER_NOTE_SOURCE)
    }));
    let restored_before = execution_inventory(&destination_path);
    restored
        .work_note_on(Some(&child.short_ref), "review after restore", &[], at(8))
        .unwrap();
    assert_eq!(execution_inventory(&destination_path), restored_before);
    restored
        .save_work_graph_snapshot(None, WorkGraphSnapshotDestinationKind::Stdout, at(9))
        .unwrap();

    drop(store);
    assert_observation_repair(&database, hash, recorded);
}

fn assert_observation_repair(
    database: &std::path::Path,
    hash: ObjectId,
    recorded: WorkObservation,
) {
    let store = SqliteStore::open(database).unwrap();
    let connection = rusqlite::Connection::open(database).unwrap();
    connection
        .execute("DELETE FROM work_observations", [])
        .unwrap();
    let invalid = store.verify_all().unwrap().invalid_work_records;
    assert!(
        invalid.contains(&format!("work_observation:{hash}")),
        "{invalid:?}"
    );
    drop(connection);
    drop(store);
    assert!(
        SqliteStore::repair_rebuildable_projections(database)
            .unwrap()
            .is_healthy()
    );
    let repaired = SqliteStore::open(database).unwrap();
    let (_, observations) = repaired.work_observation_tail(recorded.work_id, 8).unwrap();
    assert_eq!(observations, vec![(hash, recorded)]);
    let invalid = repaired.verify_all().unwrap().invalid_work_records;
    assert!(invalid.is_empty(), "{invalid:?}");
}

#[test]
fn phoenix_non_holder_note_recovers_core_commit_without_appending_again() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("work.db");
    let owner = service(&database, "holder");
    let reviewer = service(&database, "reviewer");
    let root = proposed_root(
        owner
            .work_propose(root_input("Replay", "replay"), at(0))
            .unwrap(),
    );
    // Interrupt only receipt persistence, after the core append committed.
    // Unlike clearing a finished receipt, this retains the real pending basis.
    reviewer.work_focus(&root.short_ref, at(1)).unwrap();
    let store = SqliteStore::open(&database).unwrap();
    let connection = rusqlite::Connection::open(&database).unwrap();
    connection
        .execute_batch(
            "CREATE TRIGGER interrupt_note_receipt
        BEFORE UPDATE OF result_id ON work_protocol_attempts
        WHEN NEW.operation = 'work_update:note' AND NEW.result_id IS NOT NULL
        BEGIN SELECT RAISE(ABORT, 'test receipt interruption'); END;",
        )
        .unwrap();
    assert!(
        reviewer
            .work_note_on(Some(&root.short_ref), "durable finding", &[], at(1))
            .is_err()
    );
    let (_, first) = store.work_observation_tail(root.work_id, 8).unwrap();
    assert_eq!(first.len(), 1);
    connection
        .execute_batch("DROP TRIGGER interrupt_note_receipt;")
        .unwrap();
    let restarted = service(&database, "reviewer");
    let recovered = restarted
        .work_note_on(Some(&root.short_ref), "durable finding", &[], at(2))
        .unwrap();
    assert!(
        recovered.non_holder,
        "recovery preserves the original authority path"
    );
    assert_eq!(
        serde_json::to_value(&first[0].0).unwrap(),
        recovered.evidence.result
    );
    assert_eq!(
        SqliteStore::open(&database)
            .unwrap()
            .work_observation_tail(root.work_id, 8)
            .unwrap()
            .0,
        1
    );
}

#[test]
fn phoenix_same_actor_peer_note_is_delivered_once_and_session_bound_in_staging() {
    use crate::verbs::{AgentVerbs, NextInput};
    use std::sync::Arc;
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("work.db");
    let owner = Arc::new(service(&database, "holder"));
    let reviewer = service(&database, "reviewer-private-session");
    let root = proposed_root(
        owner
            .work_propose(root_input("Peer notes", "root"), at(0))
            .unwrap(),
    );
    claim(&owner, &root.short_ref, 1);
    let words = AgentVerbs::with_shared_service(
        owner.clone(),
        "shared-actor".into(),
        SessionId("holder".into()),
    );
    words.next(&NextInput::default(), at(2)).unwrap();
    words.next(&NextInput::default(), at(3)).unwrap();
    reviewer
        .work_note_on(Some(&root.short_ref), "same actor peer finding", &[], at(4))
        .unwrap();
    let receipt = words.next(&NextInput::default(), at(5)).unwrap();
    assert_eq!(
        receipt
            .text()
            .lines()
            .filter(|line| line.contains("same actor peer finding"))
            .count(),
        1
    );
    assert!(
        !serde_json::to_string(&receipt.value)
            .unwrap()
            .contains("reviewer-private-session")
    );
    let store = SqliteStore::open(&database).unwrap();
    let session = store
        .work_session_state(&owner.project_id, &owner.session_id, at(5))
        .unwrap();
    let payload = store
        .staged_work_session_delivery_payload(&owner.project_id, &owner.session_id)
        .unwrap()
        .unwrap();
    let mut page: StagedWorkChangePage = serde_json::from_slice(&payload).unwrap();
    assert_eq!(page.changes.len(), 1);
    assert!(!page.changes[0].from_current_session);
    assert!(
        serde_json::to_value(&page).unwrap()["changes"][0]
            .get("from_current_session")
            .is_none()
    );
    let feed = FeedId::Project(owner.project_id.clone());
    let through = session.tentative_project_cursor.unwrap();
    verify_staged_work_change_page(
        &store,
        &owner.session_id,
        &feed,
        session.project_cursor,
        through,
        &mut page,
    )
    .unwrap();
    let mut wrong_session = page.clone();
    wrong_session.changes[0].from_current_session = true;
    assert!(
        verify_staged_work_change_page(
            &store,
            &owner.session_id,
            &feed,
            session.project_cursor,
            through,
            &mut wrong_session,
        )
        .is_err()
    );
    let next = words.next(&NextInput::default(), at(6)).unwrap();
    assert!(!next.text().contains("same actor peer finding"));
    let reviewer_words = AgentVerbs::with_shared_service(
        Arc::new(reviewer),
        "shared-actor".into(),
        SessionId("reviewer-private-session".into()),
    );
    let own_view = reviewer_words.next(&NextInput::default(), at(7)).unwrap();
    // Own findings remain excluded from change delivery; recent participation
    // is a separate persistent discovery section, not a delivered peer delta.
    assert!(
        !serde_json::to_string(&own_view.value["changes"])
            .unwrap()
            .contains("same actor peer finding")
    );
    assert_eq!(
        own_view.value["participated"][0]["note"],
        "same actor peer finding"
    );
    let text = own_view.text();
    let changes = text.split_once("changes by others").unwrap().1;
    assert!(!changes.contains("same actor peer finding"));
}

#[test]
fn phoenix_non_holder_note_latest_uses_dense_order_not_asserted_time() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("work.db");
    let owner = service(&database, "holder");
    let reviewer = service(&database, "reviewer");
    let root = proposed_root(
        owner
            .work_propose(root_input("Ordering", "root"), at(0))
            .unwrap(),
    );
    claim(&owner, &root.short_ref, 1);
    owner
        .work_note_on(Some(&root.short_ref), "holder future clock", &[], at(100))
        .unwrap();
    reviewer
        .work_note_on(Some(&root.short_ref), "peer earlier clock", &[], at(2))
        .unwrap();
    let focused = owner.work_focus(&root.short_ref, at(3)).unwrap();
    assert_eq!(
        focused.latest_evidence_item.unwrap().summary,
        "peer earlier clock"
    );
    owner
        .work_note_on(Some(&root.short_ref), "holder latest append", &[], at(4))
        .unwrap();
    let focused = owner.work_focus(&root.short_ref, at(5)).unwrap();
    let latest = focused.latest_evidence_item.unwrap();
    assert_eq!(latest.summary, "holder latest append");
    assert!(!latest.non_holder);
    assert_eq!(
        SqliteStore::open(&database)
            .unwrap()
            .work_observation_tail(root.work_id, 8)
            .unwrap()
            .0,
        1
    );
}

#[test]
fn phoenix_non_holder_append_checks_project_lifecycle_holder_and_provenance_atomically() {
    use crate::domain::RecordWorkObservationRequest;
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("work.db");
    let owner = service(&database, "holder");
    let reviewer = service(&database, "reviewer");
    let root = proposed_root(
        owner
            .work_propose(root_input("Admission", "root"), at(0))
            .unwrap(),
    );
    let mut store = SqliteStore::open(&database).unwrap();
    let request = || RecordWorkObservationRequest {
        status: false,
        project_id: owner.project_id.clone(),
        work_id: root.work_id,
        expected_work_revision: root.revision,
        session_id: reviewer.session_id.clone(),
        summary: "finding".into(),
        refs: Vec::new(),
        actor: reviewer.non_holder_note_actor(),
        idempotency_key: "direct-observation".into(),
        recorded_at: at(2),
    };
    for defect in ["project", "revision", "session", "marker"] {
        let mut invalid = request();
        match defect {
            "project" => invalid.project_id = ProjectId("another-project".into()),
            "revision" => invalid.expected_work_revision += 1,
            "session" => invalid.session_id = SessionId("another-session".into()),
            "marker" => invalid
                .actor
                .provenance_chain
                .retain(|link| link.source != NON_HOLDER_NOTE_SOURCE),
            _ => unreachable!(),
        }
        let count = store.verify_all().unwrap().checked_objects;
        assert!(
            store
                .record_work_observation(&invalid, &DevelopmentNoopRedactor)
                .is_err(),
            "{defect}"
        );
        assert_eq!(
            store.verify_all().unwrap().checked_objects,
            count,
            "{defect}"
        );
    }
    claim(&owner, &root.short_ref, 1);
    let mut holder = request();
    holder.expected_work_revision = store.get_work_item(root.work_id).unwrap().revision;
    holder.session_id = owner.session_id.clone();
    holder.actor = owner.non_holder_note_actor();
    assert!(matches!(
        store.record_work_observation(&holder, &DevelopmentNoopRedactor),
        Err(StoreError::WorkClaimMismatch { .. })
    ));
    owner
        .work_complete(completion_input("delivered", "done"), at(3))
        .unwrap();
    let mut completed = request();
    completed.expected_work_revision = store.get_work_item(root.work_id).unwrap().revision;
    completed.recorded_at = at(4);
    let before = store.verify_all().unwrap().checked_objects;
    assert!(matches!(
        store.record_work_observation(&completed, &DevelopmentNoopRedactor),
        Err(StoreError::InvalidWork(_))
    ));
    assert_eq!(store.verify_all().unwrap().checked_objects, before);
    assert_eq!(store.work_observation_tail(root.work_id, 8).unwrap().0, 0);
}

#[test]
fn non_holder_observation_replay_preserves_defaulted_capture_attribution() {
    use crate::domain::{ProvenanceLink, ProvenanceRelation, RecordWorkObservationRequest};
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("work.db");
    let owner = service(&database, "holder");
    let reviewer = service(&database, "reviewer");
    let root = proposed_root(
        owner
            .work_propose(root_input("Retry", "root"), at(0))
            .unwrap(),
    );
    let mut request = RecordWorkObservationRequest {
        status: false,
        project_id: owner.project_id.clone(),
        work_id: root.work_id,
        expected_work_revision: root.revision,
        session_id: reviewer.session_id.clone(),
        summary: "one observation".into(),
        refs: Vec::new(),
        actor: reviewer.non_holder_note_actor(),
        idempotency_key: "observation-retry".into(),
        recorded_at: at(2),
    };
    request.actor.provenance_chain.push(ProvenanceLink {
        relation: ProvenanceRelation::DerivedFrom,
        source: "defaulted:process_session".into(),
        reference: Some("session_id".into()),
    });
    let original_actor = request.actor.clone();
    let mut store = SqliteStore::open(&database).unwrap();
    let connection = rusqlite::Connection::open(&database).unwrap();
    let capture = store
        .record_work_observation(&request, &DevelopmentNoopRedactor)
        .unwrap();
    let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
    request.actor = request.actor.retry_stable();
    assert_eq!(
        serde_json::to_value(
            store
                .record_work_observation(&request, &DevelopmentNoopRedactor)
                .unwrap()
        )
        .unwrap(),
        serde_json::to_value(capture).unwrap()
    );
    assert_eq!(
        before,
        crate::storage::test_database_shape_snapshot(&connection).unwrap()
    );
    let (total, rows) = store.work_observation_tail(root.work_id, 8).unwrap();
    assert_eq!(total, 1);
    assert_eq!(rows[0].1.actor, original_actor);
    request.actor.reason = "a different intent".into();
    assert!(matches!(
        store.record_work_observation(&request, &DevelopmentNoopRedactor),
        Err(StoreError::WorkOperationIdempotencyConflict { .. })
    ));
    assert_eq!(
        before,
        crate::storage::test_database_shape_snapshot(&connection).unwrap()
    );
}

#[test]
fn phoenix_gate_without_focus_names_explicit_target_and_never_guesses_completed_work() {
    use crate::verbs::{AgentVerbs, GateInput};
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("work.db");
    let owner = service(&database, "holder");
    let root = proposed_root(
        owner
            .work_propose(root_input("Late gate", "root"), at(0))
            .unwrap(),
    );
    claim(&owner, &root.short_ref, 1);
    owner
        .work_complete(completion_input("delivered", "done"), at(2))
        .unwrap();
    let words = AgentVerbs::new(
        database,
        owner.project_id.clone(),
        "shared-actor".into(),
        SessionId("new-session".into()),
        None,
    );
    let gate = |work_ref| GateInput {
        work_ref,
        name: "check".into(),
        failed: Vec::new(),
        evidence_ref: None,
    };
    words.show(&root.short_ref, at(3)).unwrap();
    let error = words.gate(gate(None), at(3)).unwrap_err();
    assert!(matches!(&error.error, StoreError::InvalidWork(reason)
        if reason == "no item is selected for this gate; use gate NAME --work-ref REF"));
    let guidance = error.guidance();
    assert_eq!(
        guidance.reminders,
        vec![crate::verbs::GATE_WORK_REF_REQUIRED]
    );
    assert_eq!(guidance.next, vec!["engram work next"]);
    let structured = crate::mcp::store_error_value(&error.error);
    assert_eq!(
        structured["error"]["details"]["remedy"],
        crate::verbs::GATE_WORK_REF_REQUIRED
    );
    words.gate(gate(Some(root.short_ref)), at(4)).unwrap();
    // A late gate on work this session does not hold leaves its focus where
    // it was, so a bare gate still names no item rather than guessing the
    // completed one.
    let again = words.gate(gate(None), at(5)).unwrap_err();
    assert!(matches!(&again.error, StoreError::InvalidWork(reason)
        if reason == "no item is selected for this gate; use gate NAME --work-ref REF"));
}

/// The claim `held` reports as focused, with the binding a host would bind.
fn focused_binding(
    session: &LocalWorkService,
    now: chrono::DateTime<chrono::Utc>,
) -> crate::ControlWorkBinding {
    session
        .work_held(now)
        .expect("held")
        .items
        .into_iter()
        .find(|row| row.focused)
        .and_then(|row| row.control_binding)
        .expect("the focused claim is bindable")
}

/// One host turn, as the host runs it: bind the holder's control session to
/// `binding`, then grant, begin and report a turn that changed the source.
fn host_turn_changing_source(
    database: &std::path::Path,
    binding: &crate::ControlWorkBinding,
    key: &str,
    second: i64,
) {
    use crate::domain::{
        EffectClass, ExecutionObservationInput, ExecutionOutcome, ExecutionSourceBasis,
        ResourceCoverage, ResourceSubject, TurnIntent, TurnNextIntent, TurnPurpose,
    };
    let mut host = crate::storage::SqliteStore::open(database).expect("host store");
    let project = ProjectId("observation-test".into());
    let holder = SessionId("holder".into());
    let connection = host
        .resume_control_connection(&holder, at(second))
        .expect("host connection");
    let actor = ActorContext {
        actor_id: "host".into(),
        actor_kind: "host".into(),
        assurance: AssuranceLevel::Asserted,
        run_id: Some(binding.run_id.0.to_string()),
        session_id: Some(holder.clone()),
        source_tool: Some("host-control:bind".into()),
        source_skill: None,
        provenance_chain: Vec::new(),
        reason: "bind the focused claim".into(),
    };
    let bound = host
        .bind_control_session_with_work(
            &project,
            &format!("local-work:{key}"),
            "Focused claim",
            &holder,
            &connection,
            &actor,
            Some(binding),
            crate::ControlAssurance::TurnGated,
            &[EffectClass::Observe, EffectClass::MutateLocal],
            1,
            &format!("bind-{key}"),
            at(second),
        )
        .expect("bind");
    let decision = host
        .evaluate_control_turn(
            &project,
            &holder,
            &connection,
            &bound.routing_token,
            &TurnIntent {
                idempotency_key: format!("evaluate-{key}"),
                intent_fingerprint: crate::ObjectId::from_canonical_bytes(key.as_bytes()),
                purpose: Some(TurnPurpose::Ordinary),
                requested_effects: vec![EffectClass::MutateLocal],
                resource_intents: vec![ResourceSubject::Path {
                    project_id: project.clone(),
                    segments: vec!["src".into()],
                    coverage: ResourceCoverage::Tree,
                }],
            },
            at(second + 1),
        )
        .expect("evaluate");
    let crate::ControlTurnDecision::Grant { grant } = decision else {
        panic!("the turn must grant: {decision:?}");
    };
    assert!(matches!(
        host.begin_control_turn(
            &project,
            &holder,
            &connection,
            &bound.routing_token,
            &grant.grant_id,
            &[],
            &format!("begin-{key}"),
            at(second + 2),
        )
        .expect("begin"),
        crate::ControlTurnBeginDecision::Begin { .. }
    ));
    host.checkpoint_control_turn_with_observations(
        &project,
        &holder,
        &connection,
        &bound.routing_token,
        &grant.grant_id,
        TurnNextIntent::Continue,
        &[ExecutionObservationInput {
            observation_id: format!("write-{key}"),
            action_fingerprint: crate::ObjectId::from_canonical_bytes(
                format!("write {key}").as_bytes(),
            ),
            effect: EffectClass::MutateLocal,
            outcome: ExecutionOutcome::Succeeded,
            source_changed: true,
            source_basis: Some(ExecutionSourceBasis {
                workspace_id: "workspace".into(),
                source_revision: format!("after-{key}"),
            }),
            observed_at: Some(at(second + 3)),
        }],
        &format!("checkpoint-{key}"),
        at(second + 3),
    )
    .expect("checkpoint");
}

/// The agent words for the holder session, as CLI and MCP drive them.
fn note_word(database: &std::path::Path) -> crate::verbs::AgentVerbs {
    crate::verbs::AgentVerbs::new(
        database.to_owned(),
        ProjectId("observation-test".into()),
        "shared-actor".into(),
        SessionId("holder".into()),
        None,
    )
}

/// How many stored records of `kind` name `work` as their work item.
fn records_on(database: &std::path::Path, kind: &str, work: &WorkItemSummary) -> i64 {
    let path = if kind == "execution_observation" {
        "$.binding.work_id"
    } else {
        "$.work_id"
    };
    rusqlite::Connection::open(database)
        .unwrap()
        .query_row(
            "SELECT count(*) FROM objects WHERE object_kind = ?1
             AND json_extract(CAST(canonical_json AS TEXT), ?2) = ?3",
            rusqlite::params![kind, path, work.work_id.0.to_string()],
            |row| row.get(0),
        )
        .unwrap()
}

/// A note on an item this session does not hold is an observation, not a
/// switch of work: it leaves the session's focus, and so the claim `held`
/// reports as focused, where it was. A host binds the next turn from that
/// focused claim, so the turn's source change lands on the work the session
/// was doing, not on the item it only commented on.
#[test]
fn a_non_holder_note_leaves_focus_and_the_next_turn_on_the_focused_claim() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("work.db");
    let session = service(&database, "holder");
    let first = proposed_root(
        session
            .work_propose(root_input("first", "first"), at(0))
            .unwrap(),
    );
    let second = proposed_root(
        session
            .work_propose(root_input("second", "second"), at(0))
            .unwrap(),
    );
    let other = proposed_root(
        session
            .work_propose(root_input("other", "other"), at(0))
            .unwrap(),
    );
    claim(&session, &first.short_ref, 1);
    claim(&session, &second.short_ref, 2);
    assert_eq!(focused_binding(&session, at(3)).work_id, second.work_id);

    let observed = session
        .work_note_on(
            Some(&other.short_ref),
            "a comment on work I do not hold",
            &[],
            at(4),
        )
        .expect("non-holder note");
    assert!(observed.non_holder);
    let binding = focused_binding(&session, at(5));
    assert_eq!(
        binding.work_id, second.work_id,
        "a non-holder note must not move focus"
    );
    // The note word is the path CLI and MCP take. It resolves the named item
    // too, and must not focus it ahead of the holder check.
    let words = note_word(&database);
    let worded = words
        .note(
            &crate::verbs::NoteInput {
                status: false,
                work_ref: Some(other.short_ref.clone()),
                text: "another comment, through the note word".into(),
                refs: Vec::new(),
            },
            at(5),
        )
        .expect("non-holder note word");
    assert!(
        worded.text().contains("(observation, no run credit)"),
        "{}",
        worded.text()
    );
    let binding = focused_binding(&session, at(5));
    assert_eq!(
        binding.work_id, second.work_id,
        "the note word must not move focus either"
    );
    // A gate or an evaluation naming work the session does not hold is
    // refused here, and neither moves focus on the way.
    let gate: crate::verbs::GateInput = serde_json::from_value(serde_json::json!({
        "work_ref": other.short_ref,
        "name": "a peer's check",
    }))
    .unwrap();
    assert!(words.gate(gate, at(5)).is_err());
    let evaluation: crate::verbs::EvaluateInput = serde_json::from_value(serde_json::json!({
        "work_ref": other.short_ref,
        "mode": "independent_session",
        "acceptance_basis": 1,
        "evidence_basis": 1,
        "verdicts": [],
    }))
    .unwrap();
    assert!(words.evaluate(evaluation, at(5)).is_err());
    assert_eq!(
        focused_binding(&session, at(5)).work_id,
        second.work_id,
        "a gate or evaluation on work the session does not hold must not move focus"
    );

    host_turn_changing_source(&database, &binding, "edit", 6);
    assert_eq!(records_on(&database, "execution_observation", &second), 1);
    assert_eq!(records_on(&database, "execution_observation", &other), 0);
    assert_eq!(records_on(&database, "execution_observation", &first), 0);
}

/// Engram cannot tell which claim a file belongs to: a host reports each
/// turn's source change against the claim bound when the turn started, and
/// it binds the focused claim. A holder's targeted note moves focus, so the
/// next turn's change, and the "tests have not run" obligation it opens,
/// land on the noted claim even when the edit was for another one. Switch
/// claims at a turn boundary to keep a change on the work it belongs to.
#[test]
fn a_holder_note_moves_focus_so_the_next_source_change_lands_on_that_claim() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("work.db");
    let session = service(&database, "holder");
    let first = proposed_root(
        session
            .work_propose(root_input("first", "first"), at(0))
            .unwrap(),
    );
    let second = proposed_root(
        session
            .work_propose(root_input("second", "second"), at(0))
            .unwrap(),
    );
    claim(&session, &first.short_ref, 1);
    claim(&session, &second.short_ref, 2);
    assert_eq!(focused_binding(&session, at(3)).work_id, second.work_id);

    // Through the note word, the path CLI and MCP take.
    let words = note_word(&database);
    let noted = words
        .note(
            &crate::verbs::NoteInput {
                status: false,
                work_ref: Some(first.short_ref.clone()),
                text: "a finding on the first claim".into(),
                refs: Vec::new(),
            },
            at(4),
        )
        .expect("holder note");
    assert!(!noted.text().contains("observation"), "{}", noted.text());
    let binding = focused_binding(&session, at(5));
    assert_eq!(binding.work_id, first.work_id, "a holder note moves focus");

    host_turn_changing_source(&database, &binding, "edit", 6);
    assert_eq!(records_on(&database, "execution_observation", &first), 1);
    assert_eq!(records_on(&database, "work_obligation", &first), 1);
    assert_eq!(records_on(&database, "execution_observation", &second), 0);
    assert_eq!(records_on(&database, "work_obligation", &second), 0);
}
