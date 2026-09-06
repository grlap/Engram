use super::*;

mod guards;

fn service(database: &std::path::Path) -> LocalWorkService {
    LocalWorkService::new(
        database.to_path_buf(),
        ProjectId("decomposition-replay".into()),
        "creator".into(),
        SessionId("creator".into()),
        None,
    )
}

fn child_input(title: &str) -> WorkProposeInput {
    WorkProposeInput::Decompose {
        children: vec![WorkChildInput {
            external_ref: None,
            notes: vec!["Initial observation".into()],
            key: "child".into(),
            title: title.into(),
            outcome: "Deliver the child".into(),
            acceptance: vec!["Child delivered".into()],
            requirement: None,
            kind: None,
            priority: None,
            labels: Vec::new(),
            assigned_to: None,
            deferred_until: None,
        }],
        prerequisites: Vec::new(),
        idempotency_key: String::new(),
    }
}

fn child_id(result: &WorkProposeResult) -> WorkId {
    let WorkProposeResult::Decomposition(summary) = result else {
        panic!("decomposition required");
    };
    assert_eq!(summary.child_count, 1);
    summary.children[0].work_id
}

fn pending_request(
    service: &LocalWorkService,
    input: &WorkProposeInput,
    now: DateTime<Utc>,
) -> DecomposeWorkRequest {
    let mut store = service.store_at(now).unwrap();
    let basis = service
        .protocol_basis(&store, true, false, None, now)
        .unwrap();
    let intent = service.protocol_intent(input);
    let key = service
        .effective_idempotency_key("", "work_propose:decompose", &basis, &intent, now)
        .unwrap();
    store
        .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
            project_id: &service.project_id,
            session_id: &service.session_id,
            operation: "work_propose:decompose",
            idempotency_key: &key,
            intent: &intent,
            basis: &basis,
            now,
        })
        .unwrap();
    let parent = basis.focused_work.as_ref().unwrap();
    let WorkProposeInput::Decompose { children, .. } = input else {
        panic!("decomposition required");
    };
    DecomposeWorkRequest {
        parent_id: parent.work_id,
        expected_parent_revision: parent.revision,
        children: children
            .iter()
            .map(|child| ChildWorkDraft {
                external_ref: None,
                notes: child.notes.clone(),
                local_key: child.key.clone(),
                child_requirement: ChildRequirement::Required,
                title: child.title.clone(),
                outcome: child.outcome.clone(),
                acceptance: child.acceptance.clone(),
                kind: WorkItemKind::Task,
                priority: parent.priority,
                labels: Vec::new(),
                assigned_to: None,
                deferred_until: None,
            })
            .collect(),
        prerequisites: Vec::new(),
        authority: service.planning_authority(basis.claim.as_ref(), parent, now),
        actor: service.actor("work_propose", "atomically decompose ambient local work"),
        idempotency_key: service
            .core_operation_key("work_propose:decompose", &key, "decompose_work")
            .unwrap(),
        created_at: now,
    }
}

#[test]
fn decomposition_retry_replays_after_own_revision_and_claim_renewal() {
    for held in [false, true] {
        let directory = crate::test_support::temp_home().unwrap();
        let database = directory.path().join("work.db");
        let writer = service(&database);
        let parent = proposed_root(
            writer
                .work_propose(root_input("Parent", "root"), at(0))
                .unwrap(),
        );
        if held {
            writer
                .work_update(
                    WorkUpdateInput::Claim {
                        ttl_seconds: Some(30),
                        recovery_reason: None,
                        idempotency_key: "claim".into(),
                    },
                    at(1),
                )
                .unwrap();
        }
        let input = child_input("Child");
        let first = writer.work_propose(input.clone(), at(2)).unwrap();
        let store = SqliteStore::open(&database).unwrap();
        let after = store.get_work_item(parent.work_id).unwrap();
        assert!(after.revision > parent.revision);
        let events = store.work_event_count(parent.work_id).unwrap();
        let replay = service(&database).work_propose(input, at(3)).unwrap();
        assert_eq!(
            serde_json::to_value(&first).unwrap(),
            serde_json::to_value(replay).unwrap()
        );
        assert_eq!(store.get_work_item(parent.work_id).unwrap(), after);
        assert_eq!(store.work_event_count(parent.work_id).unwrap(), events);
        assert_eq!(
            store.work_observation_tail(child_id(&first), 10).unwrap().0,
            1
        );
        let changed = writer
            .work_propose(child_input("Different intent"), at(4))
            .unwrap();
        assert_ne!(child_id(&first), child_id(&changed));
        assert_eq!(store.work_event_count(parent.work_id).unwrap(), events + 1);
        assert!(store.verify_all().unwrap().is_healthy());
    }
}

#[test]
fn decomposition_retry_refreshes_the_existing_pending_attempt_after_revision_race() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("work.db");
    let writer = service(&database);
    writer
        .work_propose(root_input("Parent", "root"), at(0))
        .unwrap();
    let input = child_input("Losing request");
    let request = pending_request(&writer, &input, at(1));
    let peer = service(&database);
    peer.work_propose(child_input("Concurrent winner"), at(2))
        .unwrap();
    let mut store = SqliteStore::open(&database).unwrap();
    assert!(matches!(
        store.decompose_work(&request, &DevelopmentNoopRedactor),
        Err(StoreError::WorkRevisionConflict { .. })
    ));
    let recovered = writer.work_propose(input.clone(), at(3)).unwrap();
    let connection = rusqlite::Connection::open(&database).unwrap();
    let pending: i64 = connection
        .query_row(
            "SELECT count(*) FROM work_protocol_attempts WHERE result_json IS NULL",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(
        pending, 0,
        "the losing attempt must be refreshed, not abandoned under a new key"
    );
    let replay = service(&database).work_propose(input, at(4)).unwrap();
    assert_eq!(child_id(&recovered), child_id(&replay));
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn decomposition_retry_binds_its_own_restored_parent_bootstrap() {
    for finish_protocol in [false, true] {
        let directory = crate::test_support::temp_home().unwrap();
        let source_path = directory.path().join("source.db");
        let creator = service(&source_path);
        let parent = proposed_root(
            creator
                .work_propose(root_input("Parent", "root"), at(0))
                .unwrap(),
        );
        let mut source = SqliteStore::open(&source_path).unwrap();
        let actor = source.get_work_item(parent.work_id).unwrap().created_by;
        let document = source
            .save_work_graph_snapshot(
                &creator.project_id,
                &actor,
                None,
                crate::WorkGraphSnapshotDestinationKind::Stdout,
                at(1),
                &DevelopmentNoopRedactor,
            )
            .unwrap()
            .document;
        let database = directory.path().join("restored.db");
        let mut store = SqliteStore::open(&database).unwrap();
        store
            .load_work_graph_snapshot(
                &creator.project_id,
                &actor,
                &serde_json::to_vec(&document).unwrap(),
                false,
                at(2),
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        assert!(
            store
                .get_work_item(parent.work_id)
                .unwrap()
                .active_run_id
                .is_none()
        );
        let writer = service(&database);
        writer.work_focus(&parent.short_ref, at(3)).unwrap();
        let input = child_input("Bootstrap child");
        let before = writer
            .protocol_basis(&store, true, false, None, at(4))
            .unwrap();
        let first_child = if finish_protocol {
            child_id(&writer.work_propose(input.clone(), at(4)).unwrap())
        } else {
            let request = pending_request(&writer, &input, at(4));
            store
                .decompose_work(&request, &DevelopmentNoopRedactor)
                .unwrap()
                .children[0]
                .work_id
        };
        let after = store.get_work_item(parent.work_id).unwrap();
        assert!(after.active_run_id.is_some());
        let current = writer
            .protocol_basis(&store, true, false, None, at(5))
            .unwrap();
        let stored = serde_json::to_value(&before).unwrap();
        // A runless basis cannot adopt a run without this attempt's core proof.
        assert!(matches!(
            super::super::replay::guard_decomposition_retry(&stored, &current, None),
            Err(StoreError::WorkDecompositionRetryConflict { .. })
        ));
        let events = store.work_event_count(parent.work_id).unwrap();
        let replay = service(&database).work_propose(input, at(5)).unwrap();
        assert_eq!(child_id(&replay), first_child);
        assert_eq!(store.get_work_item(parent.work_id).unwrap(), after);
        assert_eq!(store.work_event_count(parent.work_id).unwrap(), events);
        assert_eq!(store.work_observation_tail(first_child, 10).unwrap().0, 1);
        assert!(store.verify_all().unwrap().is_healthy());
    }
}
