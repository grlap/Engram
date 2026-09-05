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
        UNION ALL SELECT 'root', root_execution_id, hex(execution_json) FROM work_root_executions
        UNION ALL SELECT 'claim', run_id, hex(claim_json) FROM work_claims
        UNION ALL SELECT 'evidence', evidence_hash, run_id FROM work_run_evidence
        UNION ALL SELECT 'run_feed', feed_id, CAST(position AS TEXT) FROM work_feed_heads WHERE feed_kind = 'run_execution'
        UNION ALL SELECT object_kind, object_hash, '' FROM objects
            WHERE object_kind IN ('work_event', 'work_checkpoint', 'work_evidence', 'completion_seal')
        ORDER BY 1, 2, 3").unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?))).unwrap()
        .collect::<Result<Vec<_>, _>>().unwrap()
}

#[test]
fn phoenix_non_holder_notes_preserve_execution_and_replay_on_every_open_shape() {
    for shape in ["unclaimed", "peer-held", "blocked", "expired"] {
        let directory = tempdir().unwrap();
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
    let directory = tempdir().unwrap();
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
    let hash: ObjectHash = serde_json::from_value(result.evidence.result).unwrap();
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
    hash: ObjectHash,
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
    let directory = tempdir().unwrap();
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
        BEFORE UPDATE OF result_hash ON work_protocol_attempts
        WHEN NEW.operation = 'work_update:note' AND NEW.result_hash IS NOT NULL
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
    let directory = tempdir().unwrap();
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
    let page: StagedWorkChangePage = payload.decode().unwrap();
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
        &page,
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
            &wrong_session
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
    assert!(
        !reviewer_words
            .next(&NextInput::default(), at(7))
            .unwrap()
            .text()
            .contains("same actor peer finding")
    );
}

#[test]
fn phoenix_non_holder_note_latest_uses_dense_order_not_asserted_time() {
    let directory = tempdir().unwrap();
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
    let directory = tempdir().unwrap();
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
fn phoenix_gate_without_focus_names_explicit_target_and_never_guesses_completed_work() {
    use crate::verbs::{AgentVerbs, GateInput};
    let directory = tempdir().unwrap();
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
    let error = words.gate(gate(None), at(3)).unwrap_err();
    let guidance = error.guidance();
    assert_eq!(
        guidance.reminders,
        vec![crate::verbs::GATE_WORK_REF_REQUIRED]
    );
    assert!(guidance.next.is_empty());
    let structured = crate::mcp::store_error_value(&error.error);
    assert_eq!(
        structured["error"]["details"]["remedy"],
        crate::verbs::GATE_WORK_REF_REQUIRED
    );
    words.gate(gate(Some(root.short_ref)), at(4)).unwrap();
    words.gate(gate(None), at(5)).unwrap();
}
