use super::*;

fn plan(parent: &WorkItem, requirement: ChildRequirement) -> DecomposeWorkRequest {
    DecomposeWorkRequest {
        parent_id: parent.work_id,
        expected_parent_revision: parent.revision,
        children: vec![child("proposal", requirement, "Peer proposal")],
        prerequisites: Vec::new(),
        authority: WorkPlanningAuthority::Project,
        actor: actor("peer"),
        idempotency_key: "peer-plan".into(),
        created_at: at(3),
    }
}

fn healthy(store: &SqliteStore) {
    let report = store.verify_all().expect("verify");
    assert!(report.is_healthy(), "{report:?}");
}

#[test]
fn phoenix_peer_optional_creation_preserves_holder_authority_and_replays() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(&root_request("peer", "root", 0), &DevelopmentNoopRedactor)
        .expect("root");
    let held = claim(&mut store, &root, "holder", "claim", 1, 3600);
    let checkpoint_hash = checkpoint(&mut store, &root, &held, "holder", "checkpoint", 2, &[]);
    let parent = load_work_item(&store.connection, root.work_id).expect("parent");
    let before_run = load_work_run(&store.connection, held.run_id).expect("run");
    let before_claim = load_work_claim_optional(&store.connection, held.run_id).expect("claim");
    let before_events =
        canonical_work_events_for_item(&store.connection, root.work_id).expect("events");
    let before_execution =
        active_root_execution(&store.connection, root.root_id).expect("execution");
    let mut request = plan(&parent, ChildRequirement::Optional);
    request.children[0].notes = vec![
        "Initial proposal rationale".into(),
        "Initial proposal rationale".into(),
    ];
    let created = store
        .decompose_work(&request, &DevelopmentNoopRedactor)
        .expect("peer proposal");
    assert_eq!(created.parent, parent);
    assert_eq!(
        load_work_item(&store.connection, root.work_id).expect("parent"),
        parent
    );
    assert_eq!(
        load_work_run(&store.connection, held.run_id).expect("run"),
        before_run
    );
    assert_eq!(
        load_work_claim_optional(&store.connection, held.run_id).expect("claim"),
        before_claim
    );
    assert_eq!(
        canonical_work_events_for_item(&store.connection, root.work_id).expect("events"),
        before_events
    );
    assert_eq!(before_run.last_checkpoint, Some(checkpoint_hash));
    let child = &created.children[0];
    assert_eq!(child.lifecycle, WorkLifecycle::Open);
    assert_eq!(child.child_requirement, ChildRequirement::Optional);
    assert!(
        child
            .created_by
            .provenance_chain
            .iter()
            .any(crate::domain::is_peer_child_proposal_marker)
    );
    let child_run =
        load_work_run(&store.connection, child.active_run_id.expect("run id")).expect("child run");
    assert!(child_run.executor.is_none());
    assert!(child_run.last_checkpoint.is_none());
    let after_execution =
        active_root_execution(&store.connection, root.root_id).expect("execution");
    let mut expected_execution = before_execution;
    expected_execution.run_ids.push(child_run.run_id);
    expected_execution.run_ids.sort_by_key(|id| id.0);
    expected_execution.revision += 1;
    expected_execution.updated_at = request.created_at;
    assert_eq!(after_execution, expected_execution);
    let observations = store
        .work_observation_tail(child.work_id, 10)
        .expect("notes");
    assert_eq!(observations.0, 2);
    assert_eq!(
        observations
            .1
            .iter()
            .map(|(_, note)| note.sequence)
            .collect::<Vec<_>>(),
        vec![1, 2]
    );
    let before_replay = test_database_shape_snapshot(&store.connection).expect("snapshot");
    assert_eq!(
        store
            .decompose_work(&request, &DevelopmentNoopRedactor)
            .expect("lost response replay"),
        created
    );
    assert_eq!(
        test_database_shape_snapshot(&store.connection).expect("snapshot"),
        before_replay
    );
    healthy(&store);
    // The unchanged holder can still mutate using exactly its previous fence.
    checkpoint(
        &mut store,
        &parent,
        &before_claim.expect("held"),
        "holder",
        "after-peer",
        4,
        &[],
    );
    healthy(&store);
}

#[test]
fn phoenix_peer_optional_creation_preserves_pending_handoff() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("peer-offer", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let held = claim(&mut store, &root, "holder", "claim", 1, 3600);
    let offer = store
        .offer_work_handoff(
            &OfferWorkHandoffRequest {
                work_id: root.work_id,
                run_id: held.run_id,
                expected_work_revision: root.revision,
                from: held.holder.clone(),
                to: SessionId("acceptor".into()),
                claim_id: held.claim_id,
                claim_fence: held.fence,
                ttl_seconds: 60,
                checkpoint_summary: "Ready for acceptor".into(),
                actor: actor("holder"),
                idempotency_key: "offer".into(),
                offered_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("offer");
    let snapshot = |store: &SqliteStore| -> Vec<Vec<u8>> {
        ["SELECT item_json FROM work_items WHERE parent_id IS NULL",
         "SELECT run_json FROM work_runs WHERE run_id = (SELECT active_run_id FROM work_items WHERE parent_id IS NULL)",
         "SELECT claim_json FROM work_claims",
         "SELECT offer_json FROM work_handoff_offers"]
            .into_iter().map(|sql| store.connection.query_row(sql, [], |row| row.get(0)).expect("authority bytes")).collect()
    };
    let before = snapshot(&store);
    let created = store
        .decompose_work(
            &plan(&root, ChildRequirement::Optional),
            &DevelopmentNoopRedactor,
        )
        .expect("peer under live offer");
    assert_eq!(snapshot(&store), before);
    let accepted = store
        .accept_work_handoff(
            &AcceptWorkHandoffRequest {
                work_id: root.work_id,
                offer_id: offer.offer_id,
                to: SessionId("acceptor".into()),
                actor: actor("acceptor"),
                idempotency_key: "accept".into(),
                accepted_at: at(4),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("accept unchanged offer");
    assert_eq!(accepted.holder, SessionId("acceptor".into()));
    assert_eq!(
        load_work_item(&store.connection, created.children[0].work_id).expect("visible child"),
        created.children[0]
    );
    healthy(&store);
}

#[test]
fn phoenix_peer_required_mixed_and_prerequisite_plans_refuse_without_writes() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("peer-refusal", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    claim(&mut store, &root, "holder", "claim", 1, 3600);
    let mut cases = vec![
        plan(&root, ChildRequirement::Required),
        plan(&root, ChildRequirement::Optional),
        plan(&root, ChildRequirement::Optional),
    ];
    cases[1]
        .children
        .push(child("required", ChildRequirement::Required, "Required"));
    cases[2].prerequisites.push(ChildWorkPrerequisite {
        work_key: "proposal".into(),
        prerequisite: WorkDependencyRef::Existing(root.work_id),
    });
    let before = test_database_shape_snapshot(&store.connection).expect("snapshot");
    for request in cases {
        assert!(
            matches!(store.decompose_work(&request, &DevelopmentNoopRedactor), Err(StoreError::WorkPeerDecompositionRefused { parent }) if parent == root.work_id)
        );
        assert_eq!(
            test_database_shape_snapshot(&store.connection).expect("snapshot"),
            before
        );
    }
    healthy(&store);
}

#[test]
fn phoenix_initial_note_budget_covers_the_whole_atomic_plan() {
    let limit = crate::domain::MAX_INITIAL_WORK_NOTES;
    let mut store = SqliteStore::open_in_memory().expect("store");
    let mut root_request = root_request("note-budget", "root", 0);
    root_request.notes = vec!["initial".into(); limit + 1];
    let before = test_database_shape_snapshot(&store.connection).expect("before");
    let refused = store
        .create_work(&root_request, &DevelopmentNoopRedactor)
        .expect_err("oversized root");
    assert!(
        matches!(refused, StoreError::InvalidWork(reason) if reason.contains(&(limit + 1).to_string()) && reason.contains(&format!("limit {limit}")))
    );
    assert_eq!(
        test_database_shape_snapshot(&store.connection).expect("after"),
        before
    );
    root_request.notes.pop();
    let root = store
        .create_work(&root_request, &DevelopmentNoopRedactor)
        .expect("root at limit");
    let mut request = plan(&root, ChildRequirement::Optional);
    request.children[0].notes = vec!["first child".into(); limit];
    let mut second = child("second", ChildRequirement::Optional, "Second");
    second.notes = vec!["extra".into()];
    request.children.push(second);
    let before = test_database_shape_snapshot(&store.connection).expect("before children");
    assert!(
        matches!(store.decompose_work(&request, &DevelopmentNoopRedactor), Err(StoreError::InvalidWork(reason)) if reason.contains(&format!("limit {limit}")))
    );
    request.children[0].notes.pop();
    request.children[1].notes[0] = "  ".into();
    assert!(
        matches!(store.decompose_work(&request, &DevelopmentNoopRedactor), Err(StoreError::InvalidWork(reason)) if reason == "child 2: initial note 1 must not be blank")
    );
    assert_eq!(
        test_database_shape_snapshot(&store.connection).expect("after refusals"),
        before
    );
    request.children[1].notes[0] = " final ".into();
    let created = store
        .decompose_work(&request, &DevelopmentNoopRedactor)
        .expect("plan at total limit");
    let total: usize = created
        .children
        .iter()
        .map(|item| {
            store
                .work_observation_tail(item.work_id, limit)
                .expect("notes")
                .0
        })
        .sum();
    assert_eq!(total, limit);
    assert_eq!(
        store
            .work_observation_tail(created.children[1].work_id, limit)
            .expect("second")
            .1[0]
            .1
            .summary,
        "final"
    );
    healthy(&store);
}

#[test]
fn phoenix_initial_notes_creation_is_atomic_and_ordered() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let mut request = root_request("initial-notes", "root", 0);
    request.notes = vec!["first\nline".into(), "second".into(), "second".into()];
    let before = test_database_shape_snapshot(&store.connection).expect("snapshot");
    let mut blank = request.clone();
    blank.notes.push("  ".into());
    assert!(store.create_work(&blank, &DevelopmentNoopRedactor).is_err());
    assert_eq!(
        test_database_shape_snapshot(&store.connection).expect("snapshot"),
        before
    );
    assert!(store.create_work(&request, &RejectingRedactor).is_err());
    assert_eq!(
        test_database_shape_snapshot(&store.connection).expect("snapshot"),
        before
    );
    store.connection.execute_batch("CREATE TEMP TRIGGER refuse_initial_note BEFORE INSERT ON work_observations WHEN NEW.sequence = 2 BEGIN SELECT RAISE(ABORT, 'refuse second note'); END;").expect("trigger");
    assert!(
        store
            .create_work(&request, &DevelopmentNoopRedactor)
            .is_err()
    );
    assert_eq!(
        test_database_shape_snapshot(&store.connection).expect("snapshot"),
        before
    );
    store
        .connection
        .execute_batch("DROP TRIGGER refuse_initial_note")
        .expect("remove test trigger");
    let root = store
        .create_work(&request, &DevelopmentNoopRedactor)
        .expect("atomic creation");
    let notes = store
        .work_observation_tail(root.work_id, 10)
        .expect("notes");
    assert_eq!(notes.0, 3);
    for (index, (_, note)) in notes.1.iter().enumerate() {
        assert_eq!(note.summary, request.notes[index]);
        assert_eq!(
            note.actor,
            crate::domain::non_holder_note_actor(request.actor.clone())
        );
        assert_eq!(note.actor.source_tool, request.actor.source_tool);
        assert_eq!(note.actor.reason, request.actor.reason);
        assert_eq!(note.actor.session_id, request.actor.session_id);
        assert_eq!(note.created_at, request.created_at);
    }
    let after = test_database_shape_snapshot(&store.connection).expect("snapshot");
    assert_eq!(
        store
            .create_work(&request, &DevelopmentNoopRedactor)
            .expect("replay"),
        root
    );
    assert_eq!(
        test_database_shape_snapshot(&store.connection).expect("snapshot"),
        after
    );
    let mut changed = request;
    changed.notes.reverse();
    assert!(matches!(
        store.create_work(&changed, &DevelopmentNoopRedactor),
        Err(StoreError::WorkOperationIdempotencyConflict { .. })
    ));
    healthy(&store);
}

#[test]
fn phoenix_child_initial_note_failure_rolls_back_parent_and_all_siblings() {
    for held in [false, true] {
        let mut store = SqliteStore::open_in_memory().expect("store");
        let root = store
            .create_work(
                &root_request("child-notes", "root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("root");
        if held {
            claim(&mut store, &root, "holder", "claim", 1, 3600);
        }
        let mut request = plan(&root, ChildRequirement::Optional);
        request.children[0].notes = vec!["first child".into()];
        let mut second = child("second", ChildRequirement::Optional, "Second");
        second.notes = vec!["second child".into(), "last note".into()];
        request.children.push(second);
        let before = test_database_shape_snapshot(&store.connection).expect("snapshot");
        store.connection.execute_batch("CREATE TEMP TRIGGER refuse_initial_note BEFORE INSERT ON work_observations WHEN NEW.sequence = 2 BEGIN SELECT RAISE(ABORT, 'refuse last note'); END;").expect("trigger");
        assert!(
            store
                .decompose_work(&request, &DevelopmentNoopRedactor)
                .is_err()
        );
        assert_eq!(
            test_database_shape_snapshot(&store.connection).expect("snapshot"),
            before
        );
        store
            .connection
            .execute_batch("DROP TRIGGER refuse_initial_note")
            .expect("remove test trigger");
        let created = store
            .decompose_work(&request, &DevelopmentNoopRedactor)
            .expect("creation after rollback");
        assert_eq!(
            store
                .work_observation_tail(created.children[0].work_id, 10)
                .expect("first notes")
                .0,
            1
        );
        assert_eq!(
            store
                .work_observation_tail(created.children[1].work_id, 10)
                .expect("second notes")
                .0,
            2
        );
        healthy(&store);
    }
}
