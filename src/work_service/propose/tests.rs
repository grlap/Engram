use super::super::test_support::*;
use super::super::*;

mod replay;

#[test]
fn phoenix_initial_notes_recover_after_creation_commits_before_protocol_result() {
    let directory = crate::test_support::temp_home().expect("temp");
    let database = directory.path().join("work.db");
    let project = ProjectId("creation-replay".into());
    let service = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "creator".into(),
        SessionId("creator".into()),
        None,
    );
    let mut input = root_input("Atomic creation", "initial-notes-retry");
    let WorkProposeInput::Root { notes, .. } = &mut input else {
        unreachable!()
    };
    *notes = vec!["First".into(), "Second".into()];
    let committed = {
        let mut store = service.store_at(at(0)).expect("store");
        let basis = service
            .protocol_basis(&store, false, false, None, at(0))
            .expect("basis");
        let intent = service.protocol_intent(&input);
        store
            .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
                project_id: &service.project_id,
                session_id: &service.session_id,
                operation: "work_propose:root",
                idempotency_key: "initial-notes-retry",
                intent: &intent,
                basis: &basis,
                now: at(0),
            })
            .expect("pending protocol attempt");
        store
            .create_work(
                &CreateWorkRequest {
                    evaluation_mode: None,
                    external_ref: None,
                    notes: vec!["First".into(), "Second".into()],
                    project_id: project.clone(),
                    parent_id: None,
                    child_requirement: ChildRequirement::Required,
                    title: "Atomic creation".into(),
                    outcome: "Atomic creation outcome".into(),
                    acceptance: vec!["Atomic creation accepted".into()],
                    kind: WorkItemKind::Task,
                    priority: 1,
                    labels: Vec::new(),
                    assigned_to: None,
                    deferred_until: None,
                    origin: WorkOrigin::Local,
                    source_snapshot_id: None,
                    actor: service.actor("work_propose", "create local root work"),
                    idempotency_key: service
                        .core_operation_key(
                            "work_propose:root",
                            "initial-notes-retry",
                            "create_work",
                        )
                        .expect("core key"),
                    created_at: at(0),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("commit creation without response")
    };
    let restarted = LocalWorkService::new(
        database.clone(),
        project,
        "creator".into(),
        SessionId("creator".into()),
        None,
    );
    let recovered = restarted
        .work_propose(input.clone(), at(1))
        .expect("recover creation");
    let work = proposed_root(recovered.clone());
    assert_eq!(work.work_id, committed.work_id);
    assert_eq!(
        serde_json::to_value(restarted.work_propose(input, at(2)).expect("replay")).expect("json"),
        serde_json::to_value(recovered).expect("json")
    );
    let store = SqliteStore::open(&database).expect("store");
    let notes = store
        .work_observation_tail(work.work_id, 10)
        .expect("notes");
    assert_eq!(notes.0, 2);
    assert_eq!(
        notes
            .1
            .iter()
            .map(|(_, note)| note.summary.as_str())
            .collect::<Vec<_>>(),
        vec!["First", "Second"]
    );
    assert!(store.verify_all().expect("integrity").is_healthy());
}

#[test]
fn maximum_default_fanout_decomposition_receipt_is_bounded_and_replays_exactly() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("maximum-fanout".into());
    let service = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("fanout-session".into()),
        Some("protocol-test".into()),
    );
    service
        .work_propose(root_input("Maximum fanout", "fanout-root"), at(0))
        .expect("root proposal");
    let input = WorkProposeInput::Decompose {
        children: (0..16)
            .map(|index| WorkChildInput {
                evaluation_mode: None,
                external_ref: None,
                notes: Vec::new(),
                key: format!("child-{index:02}"),
                title: format!("Child {index:02} {}", "x".repeat(256)),
                outcome: format!("Outcome {index:02} {}", "y".repeat(256)),
                acceptance: vec![format!("Acceptance {index:02} {}", "z".repeat(256))],
                requirement: Some(ChildRequirement::Required),
                kind: Some(WorkItemKind::Task),
                priority: Some(1),
                labels: vec![format!("label-{index:02}-{}", "q".repeat(128))],
                assigned_to: None,
                deferred_until: None,
            })
            .collect(),
        prerequisites: Vec::new(),
        idempotency_key: "fanout-decompose".into(),
    };
    let first = service
        .work_propose(input.clone(), at(1))
        .expect("maximum decomposition");
    let WorkProposeResult::Decomposition(summary) = &first else {
        panic!("expected decomposition");
    };
    assert_eq!(summary.child_count, 16);
    assert_eq!(summary.children.len(), 16);
    assert!(summary.details_omitted);
    assert_eq!(
        summary
            .children
            .iter()
            .map(|child| child.work_id)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        16
    );
    assert!(
        serde_json::to_vec(&first)
            .expect("serialize first receipt")
            .len()
            <= MAX_AGENT_WORK_RESPONSE_BYTES
    );

    let restarted = LocalWorkService::new(
        database.clone(),
        project,
        "agent".into(),
        SessionId("fanout-session".into()),
        Some("protocol-test".into()),
    );
    let replay = restarted
        .work_propose(input, at(2))
        .expect("exact replay after restart");
    assert_eq!(
        serde_json::to_value(&replay).expect("replay JSON"),
        serde_json::to_value(&first).expect("first JSON")
    );
    let connection = rusqlite::Connection::open(database).expect("inspect replay store");
    let stored: Vec<u8> = connection
        .query_row(
            "SELECT result_json FROM work_protocol_attempts
                 WHERE project_id = 'maximum-fanout'
                   AND session_id = 'fanout-session'
                   AND operation = 'work_propose:decompose'
                   AND idempotency_key = 'fanout-decompose'",
            [],
            |row| row.get(0),
        )
        .expect("durable bounded result");
    assert_eq!(
        serde_json::from_slice::<serde_json::Value>(&stored).expect("stored result JSON"),
        serde_json::to_value(first).expect("first result JSON")
    );
}

fn count_rows(database: &std::path::Path, sql: &str, param: &str) -> i64 {
    rusqlite::Connection::open(database)
        .expect("inspect")
        .query_row(sql, [param], |row| row.get(0))
        .expect("count")
}

fn count_roots(database: &std::path::Path) -> i64 {
    rusqlite::Connection::open(database)
        .expect("inspect")
        .query_row(
            "SELECT COUNT(*) FROM work_items WHERE parent_id IS NULL",
            [],
            |row| row.get(0),
        )
        .expect("root count")
}

#[test]
fn oversized_default_outcome_does_not_refuse_after_create() {
    let directory = crate::test_support::temp_home().expect("temp");
    let database = directory.path().join("work.db");
    let service = LocalWorkService::new(
        database.clone(),
        ProjectId("oversized-outcome".into()),
        "creator".into(),
        SessionId("creator".into()),
        None,
    );
    // Measured installed-build bands: 9000/10000 overflowed work_propose after
    // commit; 11000/12000 overflowed work_focus. 6000/8000 already succeeded.
    let mut replay = None;
    for (index, size) in [6_000, 8_000, 9_000, 10_000, 11_000, 12_000, 16_384]
        .into_iter()
        .enumerate()
    {
        let title = format!("T{size} {}", "A".repeat(size));
        let input = WorkProposeInput::Root {
            evaluation_mode: None,
            external_ref: None,
            notes: Vec::new(),
            title: title.clone(),
            outcome: title.clone(),
            acceptance: vec!["Delivered".into()],
            work_kind: None,
            priority: None,
            labels: Vec::new(),
            assigned_to: None,
            deferred_until: None,
            idempotency_key: format!("oversized-{size}"),
        };
        let result = service
            .work_propose(input.clone(), at(i64::try_from(index).expect("index")))
            .unwrap_or_else(|error| {
                panic!("work_propose must succeed for defaulted {size}-byte outcome: {error}")
            });
        let bytes = serde_json::to_vec(&result).expect("whole WorkProposeResult");
        assert!(
            bytes.len() <= MAX_AGENT_WORK_RESPONSE_BYTES,
            "independent whole-result measure {size}: {} bytes",
            bytes.len()
        );
        let WorkProposeResult::Root { work, focus } = &result else {
            panic!("expected root");
        };
        assert_eq!(work.title, compact_text(&title));
        assert!(focus.outcome.len() <= MAX_SUMMARY_BYTES);
        assert_eq!(focus.outcome, compact_text(&title));
        if size == 16_384 {
            replay = Some((input, result));
        }
    }
    let (input, first) = replay.expect("16KiB case");
    let WorkProposeResult::Root { work, .. } = &first else {
        panic!("expected root");
    };
    let work_id = work.work_id.0.to_string();
    assert_eq!(count_roots(&database), 7);
    assert_eq!(
        count_rows(
            &database,
            "SELECT COUNT(*) FROM work_items WHERE work_id = ?1",
            &work_id,
        ),
        1
    );
    assert_eq!(
        count_rows(
            &database,
            "SELECT COUNT(*) FROM work_protocol_attempts WHERE idempotency_key = ?1",
            "oversized-16384",
        ),
        1
    );
    let second = service
        .work_propose(input, at(100))
        .expect("identical explicit-key replay");
    assert_eq!(
        serde_json::to_value(&second).expect("replay JSON"),
        serde_json::to_value(&first).expect("first JSON")
    );
    assert_eq!(
        count_rows(
            &database,
            "SELECT COUNT(*) FROM work_items WHERE work_id = ?1",
            &work_id,
        ),
        1
    );
    assert_eq!(
        count_rows(
            &database,
            "SELECT COUNT(*) FROM work_protocol_attempts WHERE idempotency_key = ?1",
            "oversized-16384",
        ),
        1
    );
    assert_eq!(count_roots(&database), 7);
}
