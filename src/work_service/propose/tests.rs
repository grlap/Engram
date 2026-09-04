use super::super::test_support::*;
use super::super::*;
use tempfile::tempdir;

#[test]
fn maximum_default_fanout_decomposition_receipt_is_bounded_and_replays_exactly() {
    let directory = tempdir().expect("temp directory");
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
