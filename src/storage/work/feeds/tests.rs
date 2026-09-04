use super::super::test_support::*;
use super::super::*;

#[test]
fn canonical_work_events_reject_blank_asserted_identity() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    for (actor_id, session_id, key) in [
        ("   ", "session", "blank-actor"),
        ("agent", "\t", "blank-session"),
    ] {
        let mut request = root_request("blank-work-identity", key, 0);
        request.actor.actor_id = actor_id.into();
        request.actor.session_id = Some(SessionId(session_id.into()));
        assert!(matches!(
            store.create_work(&request, &DevelopmentNoopRedactor),
            Err(StoreError::InvalidWork(detail))
                if detail.contains("non-empty asserted actor and session")
        ));
    }
}

#[test]
fn focus_derived_work_contradiction_publishes_one_feed_delta_and_doctor_backstops() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("project-contra", "contra-root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let project = root.project_id.clone();
    let session = SessionId("contra-session".into());
    claim(&mut store, &root, "contra-session", "contra-claim", 1, 300);
    store
        .focus_work_session(&project, &session, root.work_id, at(1))
        .expect("focus the root");
    let note = |prose: &str, key: &str, second: i64| crate::NoteRequest {
        project_id: project.clone(),
        task_id: None,
        work_id: Some(root.work_id),
        prose: prose.into(),
        visibility: crate::NoteVisibility::Shared,
        kind: None,
        authority: None,
        sensitivity: None,
        title: None,
        tags: Vec::new(),
        evidence: Vec::new(),
        refs: Vec::new(),
        actor: actor("contra-session"),
        idempotency_key: key.into(),
        created_at: at(second),
    };
    let left = store
        .capture_note(
            &note("Constraint: use the first rule", "contra-left", 2),
            &DevelopmentNoopRedactor,
        )
        .expect("left note");
    let right = store
        .capture_note(
            &note("Constraint: use the second rule", "contra-right", 3),
            &DevelopmentNoopRedactor,
        )
        .expect("right note");
    let third = store
        .capture_note(
            &note("Constraint: use the third rule", "contra-third", 4),
            &DevelopmentNoopRedactor,
        )
        .expect("third note");

    // The caller omits the work anchor; the validated focus supplies it.
    let derived = store
        .record_memory_contradiction(
            &project,
            None,
            None,
            &session,
            "contra-session",
            &left.version,
            &right.version,
            "the first and second rules cannot both apply",
            "contra-derived",
            actor("contra-session"),
            at(5),
            &DevelopmentNoopRedactor,
        )
        .expect("focus-derived contradiction");
    let explicit = store
        .record_memory_contradiction(
            &project,
            None,
            Some(root.work_id),
            &session,
            "contra-session",
            &left.version,
            &third.version,
            "the first and third rules cannot both apply",
            "contra-explicit",
            actor("contra-session"),
            at(6),
            &DevelopmentNoopRedactor,
        )
        .expect("explicit contradiction");
    let feeds = |positions: &[FeedPosition]| {
        positions
            .iter()
            .map(|position| match &position.feed {
                FeedId::Project(_) => "project",
                FeedId::RootWork(_) => "root_work",
                FeedId::RunExecution(_) => "run_execution",
            })
            .collect::<Vec<_>>()
    };
    assert!(!derived.work_positions.is_empty());
    assert_eq!(
        feeds(&derived.work_positions),
        feeds(&explicit.work_positions)
    );
    assert!(derived.work_positions.iter().any(
        |position| matches!(position.feed, FeedId::RootWork(root_id) if root_id == root.work_id)
    ));
    assert!(store.verify_all().expect("integrity").is_healthy());

    // Doctor names a work-anchored contradiction that lost its feed entry.
    store
        .connection
        .execute(
            "DELETE FROM work_feed_entries WHERE feed_kind = 'root_work' AND object_hash = ?1",
            [derived.contradiction.as_str()],
        )
        .expect("simulate a missing anchored feed entry");
    let report = store.verify_all().expect("integrity after damage");
    assert!(!report.is_healthy());
    assert!(
        report.invalid_work_records.iter().any(|label| {
            label
                == &format!(
                    "memory_contradiction_event:{}:work-feed",
                    derived.contradiction
                )
        }),
        "{:?}",
        report.invalid_work_records
    );
}

#[test]
fn indexed_feed_work_identity_is_fail_closed_and_doctor_visible() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("project-feed-work-id", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let other = store
        .create_work(
            &root_request("project-feed-work-id", "other", 1),
            &DevelopmentNoopRedactor,
        )
        .expect("other root");
    claim(&mut store, &root, "planner", "claim-feed-work-id", 2, 120);
    let head_before = store
        .work_feed_head(&FeedId::Project(root.project_id.clone()))
        .expect("project feed head");
    store
        .connection
        .execute(
            "UPDATE work_feed_entries SET work_id = ?2
             WHERE object_kind = 'work_event'
               AND work_id = ?1
               AND object_hash = (
                   SELECT latest_event_hash FROM work_items WHERE work_id = ?1
               )",
            params![root.work_id.0.to_string(), other.work_id.0.to_string()],
        )
        .expect("corrupt indexed work identity");

    assert!(matches!(
        store.get_work_item(root.work_id),
        Err(StoreError::InvalidWorkProjection(_))
    ));
    let report = store.verify_all().expect("feed identity report");
    assert!(
        report
            .invalid_work_records
            .iter()
            .any(|record| record.contains("work_id_binding"))
    );
    assert!(
        report
            .invalid_work_records
            .iter()
            .any(|record| { record == &format!("work_item:{}:latest_event_hash", root.work_id.0) })
    );
    assert_eq!(
        store
            .work_feed_head(&FeedId::Project(root.project_id))
            .expect("unchanged project feed head"),
        head_before
    );
}
