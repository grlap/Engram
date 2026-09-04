use super::super::test_support::*;
use super::super::*;

#[test]
fn staged_work_delivery_rejects_future_stale_and_gapped_ranges() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let first = store
        .create_work(
            &root_request("project-delivery", "delivery-root-a", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("first root");
    store
        .create_work(
            &root_request("project-delivery", "delivery-root-b", 1),
            &DevelopmentNoopRedactor,
        )
        .expect("second root");
    let feed = FeedId::Project(first.project_id.clone());
    let head = store.work_feed_head(&feed).expect("project feed head");
    assert!(head > 1);
    let entries = store
        .work_feed_between(&feed, 0, head)
        .expect("dense source entries");
    let payload = CanonicalObject::freeze(&entries).expect("test delivery payload");
    let empty_payload =
        CanonicalObject::freeze(&Vec::<WorkFeedEntry>::new()).expect("empty test delivery payload");

    assert!(matches!(
        store.stage_work_session_delivery(
            &first.project_id,
            &SessionId("future".into()),
            StageWorkSessionDelivery {
                expected_confirmed_through: 0,
                expected_focused_work_id: None,
                expected_bound_task_id: None,
                delivered_through: head + 1,
                delivered_entries: &[],
                delivery_payload: &empty_payload,
                now: at(2),
            },
        ),
        Err(StoreError::InvalidWork(_))
    ));

    let session = SessionId("confirmed".into());
    let staged = store
        .stage_work_session_delivery(
            &first.project_id,
            &session,
            StageWorkSessionDelivery {
                expected_confirmed_through: 0,
                expected_focused_work_id: None,
                expected_bound_task_id: None,
                delivered_through: head,
                delivered_entries: &entries,
                delivery_payload: &payload,
                now: at(2),
            },
        )
        .expect("stage exact head");
    let staged = staged.expect("stage wins exact compare-and-swap");
    let delivery_token = staged
        .tentative_delivery_token
        .clone()
        .expect("staged delivery token");
    let expected_refusal = "work delivery acknowledgement does not match the pending page; replay it with work_next (changes selected, no acknowledgement) and acknowledge the delivered_through and delivery_token you receive";
    for (through, token) in [(head + 1_000, "wrong-token"), (head, "wrong-token")] {
        match store.acknowledge_work_session_delivery(
            &first.project_id,
            &session,
            through,
            Some(token),
            at(3),
        ) {
            Err(StoreError::InvalidWork(message)) => assert_eq!(message, expected_refusal),
            result => panic!("invalid acknowledgement must fail generically: {result:?}"),
        }
    }
    let still_pending = store
        .work_session_state(&first.project_id, &session, at(3))
        .expect("pending delivery survives rejected acknowledgements");
    assert_eq!(still_pending.project_cursor, 0);
    assert_eq!(still_pending.tentative_project_cursor, Some(head));
    assert_eq!(
        still_pending.tentative_delivery_token.as_deref(),
        Some(delivery_token.as_str())
    );
    store
        .acknowledge_work_session_delivery(
            &first.project_id,
            &session,
            head,
            Some(&delivery_token),
            at(3),
        )
        .expect("acknowledge exact head");
    assert!(matches!(
        store.stage_work_session_delivery(
            &first.project_id,
            &session,
            StageWorkSessionDelivery {
                expected_confirmed_through: head,
                expected_focused_work_id: None,
                expected_bound_task_id: None,
                delivered_through: head - 1,
                delivered_entries: &[],
                delivery_payload: &empty_payload,
                now: at(4),
            }
        ),
        Err(StoreError::InvalidWork(_))
    ));

    let (feed_kind, feed_id) = feed_parts(&feed);
    store
        .connection
        .execute(
            "DELETE FROM work_feed_entries
             WHERE feed_kind = ?1 AND feed_id = ?2 AND position = 1",
            params![feed_kind, feed_id],
        )
        .expect("create a corrupt feed gap");
    assert!(matches!(
        store.stage_work_session_delivery(
            &first.project_id,
            &SessionId("gap".into()),
            StageWorkSessionDelivery {
                expected_confirmed_through: 0,
                expected_focused_work_id: None,
                expected_bound_task_id: None,
                delivered_through: head,
                delivered_entries: &entries,
                delivery_payload: &payload,
                now: at(5),
            },
        ),
        Err(StoreError::InvalidWorkProjection(_))
    ));
}

#[test]
fn staged_work_delivery_cas_binds_the_current_task() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let work = store
        .create_work(
            &root_request("project-delivery-task-cas", "delivery-root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let feed = FeedId::Project(work.project_id.clone());
    let head = store.work_feed_head(&feed).expect("project feed head");
    let entries = store
        .work_feed_between(&feed, 0, head)
        .expect("dense source entries");
    let payload = CanonicalObject::freeze(&entries).expect("test delivery payload");
    let session = SessionId("task-bound-delivery".into());
    let task = store
        .start_task(
            &work.project_id,
            "dummy:DELIVERY-TASK-CAS",
            "Delivery task CAS",
            &session,
            actor("task-bound-delivery"),
            at(1),
        )
        .expect("task binding")
        .task;

    assert!(
        store
            .stage_work_session_delivery(
                &work.project_id,
                &session,
                StageWorkSessionDelivery {
                    expected_confirmed_through: 0,
                    expected_focused_work_id: None,
                    expected_bound_task_id: None,
                    delivered_through: head,
                    delivered_entries: &entries,
                    delivery_payload: &payload,
                    now: at(2),
                },
            )
            .expect("basis mismatch is a retry")
            .is_none()
    );
    let staged = store
        .stage_work_session_delivery(
            &work.project_id,
            &session,
            StageWorkSessionDelivery {
                expected_confirmed_through: 0,
                expected_focused_work_id: None,
                expected_bound_task_id: Some(task.task_id),
                delivered_through: head,
                delivered_entries: &entries,
                delivery_payload: &payload,
                now: at(3),
            },
        )
        .expect("current task basis stages")
        .expect("exact staging CAS");
    assert_eq!(staged.tentative_project_cursor, Some(head));
    assert!(staged.tentative_delivery_token.is_some());
}

#[test]
fn focus_change_and_pending_delivery_serialize_across_connections() {
    let directory = tempfile::tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let mut writer = SqliteStore::open(&database).expect("writer");
    let first = writer
        .create_work(
            &root_request("project-focus-delivery", "focus-first", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("first root");
    let second = writer
        .create_work(
            &root_request("project-focus-delivery", "focus-second", 1),
            &DevelopmentNoopRedactor,
        )
        .expect("second root");
    let session = SessionId("focus-delivery-session".into());
    writer
        .focus_work_session(&first.project_id, &session, first.work_id, at(2))
        .expect("initial focus");
    let head = writer
        .work_feed_head(&FeedId::Project(first.project_id.clone()))
        .expect("feed head");

    let mut delivery = SqliteStore::open(&database).expect("delivery connection");
    let feed = FeedId::Project(first.project_id.clone());
    let entries = delivery
        .work_feed_between(&feed, 0, head)
        .expect("dense source entries");
    let payload = CanonicalObject::freeze(&entries).expect("test delivery payload");
    let staged = delivery
        .stage_work_session_delivery(
            &first.project_id,
            &session,
            StageWorkSessionDelivery {
                expected_confirmed_through: 0,
                expected_focused_work_id: Some(first.work_id),
                expected_bound_task_id: None,
                delivered_through: head,
                delivered_entries: &entries,
                delivery_payload: &payload,
                now: at(3),
            },
        )
        .expect("stage from a second connection");
    let staged = staged.expect("delivery wins exact compare-and-swap");
    // Re-focusing the same item keeps the staged page.
    let same = writer
        .focus_work_session(&first.project_id, &session, first.work_id, at(4))
        .expect("same focus");
    assert_eq!(same.tentative_project_cursor, Some(head));
    // A different focus discards the page projected under the old focus
    // without confirming anything.
    let discarded = writer
        .focus_work_session(&first.project_id, &session, second.work_id, at(4))
        .expect("focus changes while a page is still staged");
    assert_eq!(discarded.focused_work_id, Some(second.work_id));
    assert_eq!(discarded.tentative_project_cursor, None);
    assert_eq!(discarded.tentative_delivery_token, None);
    assert_eq!(discarded.project_cursor, 0);
    assert!(
        delivery
            .acknowledge_work_session_delivery(
                &first.project_id,
                &session,
                head,
                staged.tentative_delivery_token.as_deref(),
                at(5),
            )
            .is_err(),
        "a discarded page cannot be acknowledged"
    );

    // The next staging recomputes the interval under the new focus.
    let restaged = delivery
        .stage_work_session_delivery(
            &first.project_id,
            &session,
            StageWorkSessionDelivery {
                expected_confirmed_through: 0,
                expected_focused_work_id: Some(second.work_id),
                expected_bound_task_id: None,
                delivered_through: head,
                delivered_entries: &entries,
                delivery_payload: &payload,
                now: at(6),
            },
        )
        .expect("restage under the new focus")
        .expect("exact staging CAS");
    delivery
        .acknowledge_work_session_delivery(
            &first.project_id,
            &session,
            head,
            restaged.tentative_delivery_token.as_deref(),
            at(7),
        )
        .expect("acknowledge the restaged page");
    let acknowledged = writer
        .work_session_state(&first.project_id, &session, at(8))
        .expect("acknowledged state");
    assert_eq!(acknowledged.focused_work_id, Some(second.work_id));
    assert_eq!(acknowledged.tentative_project_cursor, None);
    assert_eq!(acknowledged.project_cursor, head);
}

#[test]
fn doctor_rejects_tampered_pending_protocol_basis() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let project = crate::domain::ProjectId("project-pending-attempt".into());
    let session = SessionId("pending-session".into());
    store
        .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
            project_id: &project,
            session_id: &session,
            operation: "work_next",
            idempotency_key: "pending-attempt",
            intent: &serde_json::json!({"query":"ready"}),
            basis: &serde_json::json!({"cursor":0}),
            now: at(0),
        })
        .expect("begin pending attempt");
    store
        .connection
        .execute(
            "UPDATE work_protocol_attempts SET basis_json = ?1
             WHERE project_id = ?2 AND session_id = ?3
               AND operation = 'work_next' AND idempotency_key = 'pending-attempt'",
            params![b"{}".as_slice(), project.0, session.0],
        )
        .expect("tamper pending basis");

    let report = store.verify_all().expect("integrity report");
    assert!(report.invalid_work_records.iter().any(|record| {
        record.contains("work_protocol_attempt:project-pending-attempt:pending-session")
    }));
}

#[test]
fn pending_protocol_basis_refresh_accepts_an_identical_two_connection_cas() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("basis-refresh.sqlite3");
    let project = crate::domain::ProjectId("basis-refresh-project".into());
    let session = SessionId("basis-refresh-session".into());
    let source = serde_json::json!({"claim": {"holder": "holder-a", "fence": 1}});
    let target = serde_json::json!({"claim": {"holder": "holder-a", "fence": 2}});
    let mut first = SqliteStore::open(&database).expect("first store");
    first
        .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
            project_id: &project,
            session_id: &session,
            operation: "work_complete",
            idempotency_key: "identical-refresh",
            intent: &serde_json::json!({"complete": true}),
            basis: &source,
            now: at(0),
        })
        .expect("pending attempt");
    let mut second = SqliteStore::open(&database).expect("second store");
    second
        .refresh_pending_work_protocol_attempt_basis(
            &project,
            &session,
            "work_complete",
            "identical-refresh",
            &source,
            &target,
        )
        .expect("first refresh wins");

    first
        .refresh_pending_work_protocol_attempt_basis(
            &project,
            &session,
            "work_complete",
            "identical-refresh",
            &source,
            &target,
        )
        .expect("changed-zero refresh accepts the identical target");
}

#[test]
fn pending_protocol_basis_refresh_conflict_preserves_the_durable_target() {
    let directory = tempfile::tempdir().expect("temporary directory");
    let database = directory.path().join("basis-refresh-conflict.sqlite3");
    let project = crate::domain::ProjectId("basis-refresh-conflict-project".into());
    let session = SessionId("basis-refresh-conflict-session".into());
    let source = serde_json::json!({"claim": {"holder": "holder-a", "fence": 1}});
    let durable_target = serde_json::json!({"claim": {"holder": "holder-b", "fence": 2}});
    let competing_target = serde_json::json!({"claim": {"holder": "holder-c", "fence": 2}});
    let mut first = SqliteStore::open(&database).expect("first store");
    first
        .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
            project_id: &project,
            session_id: &session,
            operation: "work_complete",
            idempotency_key: "conflicting-refresh",
            intent: &serde_json::json!({"complete": true}),
            basis: &source,
            now: at(0),
        })
        .expect("pending attempt");
    let mut second = SqliteStore::open(&database).expect("second store");
    second
        .refresh_pending_work_protocol_attempt_basis(
            &project,
            &session,
            "work_complete",
            "conflicting-refresh",
            &source,
            &durable_target,
        )
        .expect("different holder wins the refresh");

    assert!(matches!(
        first.refresh_pending_work_protocol_attempt_basis(
            &project,
            &session,
            "work_complete",
            "conflicting-refresh",
            &source,
            &competing_target,
        ),
        Err(StoreError::WorkOperationIdempotencyConflict { operation, key })
            if operation == "work_complete" && key == "conflicting-refresh"
    ));
    let stored_basis: Vec<u8> = first
        .connection
        .query_row(
            "SELECT basis_json FROM work_protocol_attempts
             WHERE project_id = ?1 AND session_id = ?2
               AND operation = 'work_complete'
               AND idempotency_key = 'conflicting-refresh'",
            params![project.0, session.0],
            |row| row.get(0),
        )
        .expect("durable target basis");
    assert_eq!(
        stored_basis,
        CanonicalObject::freeze(&durable_target)
            .expect("canonical durable target")
            .bytes()
    );
}
