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
               AND object_id = (
                   SELECT latest_event_id FROM work_items WHERE work_id = ?1
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
            .any(|record| { record == &format!("work_item:{}:latest_event_id", root.work_id.0) })
    );
    assert_eq!(
        store
            .work_feed_head(&FeedId::Project(root.project_id))
            .expect("unchanged project feed head"),
        head_before
    );
}
