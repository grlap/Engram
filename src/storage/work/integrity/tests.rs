use super::super::test_support::*;
use super::super::*;

#[test]
fn scoped_mutation_ignores_unrelated_corruption_but_refuses_its_target() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let healthy = store
        .create_work(
            &root_request("project-scoped-integrity", "healthy", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("healthy root");
    let unrelated = store
        .create_work(
            &root_request("project-scoped-integrity", "unrelated", 1),
            &DevelopmentNoopRedactor,
        )
        .expect("unrelated root");
    store
        .connection
        .execute(
            "UPDATE work_items
             SET item_json = CAST(json_set(item_json, '$.title', 'corrupt unrelated') AS BLOB)
             WHERE work_id = ?1",
            [unrelated.work_id.0.to_string()],
        )
        .expect("corrupt unrelated projection");

    let revised = store
        .revise_work(
            &ReviseWorkRequest {
                work_id: healthy.work_id,
                expected_revision: healthy.revision,
                patch: WorkRevisionPatch {
                    title: Some("healthy revision".into()),
                    ..WorkRevisionPatch::default()
                },
                authority: delegated(&healthy.project_id.0, "planner"),
                actor: actor("planner"),
                idempotency_key: "healthy-revision".into(),
                updated_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("healthy scoped mutation");
    assert!(
        !store
            .verify_all()
            .expect("doctor")
            .invalid_work_records
            .is_empty()
    );
    let project_feed = FeedId::Project(healthy.project_id.clone());
    let head_before = store
        .work_feed_head(&project_feed)
        .expect("project feed head");
    store
        .connection
        .execute(
            "UPDATE work_items
             SET item_json = CAST(json_set(item_json, '$.title', 'corrupt target') AS BLOB)
             WHERE work_id = ?1",
            [healthy.work_id.0.to_string()],
        )
        .expect("corrupt target projection");
    let refused = store.revise_work(
        &ReviseWorkRequest {
            work_id: healthy.work_id,
            expected_revision: revised.revision,
            patch: WorkRevisionPatch {
                title: Some("must not commit".into()),
                ..WorkRevisionPatch::default()
            },
            authority: delegated(&healthy.project_id.0, "planner"),
            actor: actor("planner"),
            idempotency_key: "corrupt-target-revision".into(),
            updated_at: at(3),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(refused, Err(StoreError::InvalidWorkProjection(_))));
    assert_eq!(
        store
            .work_feed_head(&project_feed)
            .expect("project feed head"),
        head_before
    );
}

#[test]
fn stale_self_consistent_projection_cannot_authorize_a_new_event() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let original = store
        .create_work(
            &root_request("project-stale-projection", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let revised = store
        .revise_work(
            &ReviseWorkRequest {
                work_id: original.work_id,
                expected_revision: original.revision,
                patch: WorkRevisionPatch {
                    title: Some("canonical revision two".into()),
                    ..WorkRevisionPatch::default()
                },
                authority: delegated("project-stale-projection", "planner"),
                actor: actor("planner"),
                idempotency_key: "revision-two".into(),
                updated_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("revision two");
    assert_eq!(revised.revision, original.revision + 1);
    let event_count_before = store
        .work_event_tail(original.work_id, 100)
        .expect("event tail")
        .len();

    store
        .connection
        .execute(
            "UPDATE work_items SET revision = ?2, updated_at_ms = ?3, item_json = ?4
             WHERE work_id = ?1",
            params![
                original.work_id.0.to_string(),
                original.revision,
                original.updated_at.timestamp_millis(),
                serde_json::to_vec(&original).expect("original projection")
            ],
        )
        .expect("restore stale but internally consistent projection");

    let refused = store.revise_work(
        &ReviseWorkRequest {
            work_id: original.work_id,
            expected_revision: original.revision,
            patch: WorkRevisionPatch {
                title: Some("must not become revision three".into()),
                ..WorkRevisionPatch::default()
            },
            authority: delegated("project-stale-projection", "planner"),
            actor: actor("planner"),
            idempotency_key: "revision-after-corruption".into(),
            updated_at: at(2),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(refused, Err(StoreError::InvalidWorkProjection(_))));
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT COUNT(*)
                 FROM objects object
                 JOIN work_feed_entries entry ON entry.object_hash = object.object_hash
                 WHERE entry.feed_kind = 'project'
                   AND object.object_kind = 'work_event'
                   AND json_extract(object.canonical_json, '$.work_id') = ?1",
                [original.work_id.0.to_string()],
                |row| row.get::<_, i64>(0),
            )
            .expect("event count after refusal"),
        i64::try_from(event_count_before).expect("event count fits i64"),
    );
}

#[test]
fn relation_fingerprint_refuses_projection_drift_before_append() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("project-relation-fingerprint", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let decomposition = store
        .decompose_work(
            &DecomposeWorkRequest {
                parent_id: root.work_id,
                expected_parent_revision: root.revision,
                children: vec![
                    child("first", ChildRequirement::Required, "First"),
                    child("second", ChildRequirement::Required, "Second"),
                ],
                prerequisites: vec![ChildWorkPrerequisite {
                    work_key: "second".into(),
                    prerequisite: WorkDependencyRef::Proposed("first".into()),
                }],
                authority: delegated(&root.project_id.0, "planner"),
                actor: actor("planner"),
                idempotency_key: "decompose-relation-fingerprint".into(),
                created_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("decompose");
    let first = &decomposition.children[0];
    let second = &decomposition.children[1];
    let feed = FeedId::Project(root.project_id.clone());
    let head_before = store.work_feed_head(&feed).expect("project feed head");
    store
        .connection
        .execute(
            "DELETE FROM work_prerequisites
             WHERE work_id = ?1 AND prerequisite_id = ?2",
            params![second.work_id.0.to_string(), first.work_id.0.to_string()],
        )
        .expect("omit prerequisite projection");

    assert!(matches!(
        store.inspect_work(second.work_id, at(2)),
        Err(StoreError::InvalidWorkProjection(_))
    ));
    let refused = store.revise_work(
        &ReviseWorkRequest {
            work_id: second.work_id,
            expected_revision: second.revision,
            patch: WorkRevisionPatch {
                title: Some("must not promote omitted relation".into()),
                ..WorkRevisionPatch::default()
            },
            authority: delegated(&root.project_id.0, "planner"),
            actor: actor("planner"),
            idempotency_key: "refuse-omitted-relation".into(),
            updated_at: at(2),
        },
        &DevelopmentNoopRedactor,
    );
    assert!(matches!(refused, Err(StoreError::InvalidWorkProjection(_))));
    assert_eq!(
        store.work_feed_head(&feed).expect("feed after refusal"),
        head_before
    );
    assert!(!store.verify_all().expect("relation report").is_healthy());
}

#[test]
fn work_event_trigger_rejects_null_work_binding() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("project-trigger", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    store
        .connection
        .execute(
            "INSERT INTO work_feed_heads (feed_kind, feed_id, position)
             VALUES ('project', 'trigger-probe', 0)",
            [],
        )
        .expect("trigger probe feed");
    let event_hash = store
        .connection
        .query_row(
            "SELECT latest_event_hash FROM work_items WHERE work_id = ?1",
            [root.work_id.0.to_string()],
            |row| row.get::<_, String>(0),
        )
        .expect("root latest event");
    assert!(
        store
            .connection
            .execute(
                "INSERT INTO work_feed_entries (
                     feed_kind, feed_id, position, object_kind, object_hash, work_id
                 ) VALUES ('project', 'trigger-probe', 1, 'work_event', ?1, NULL)",
                [event_hash],
            )
            .is_err(),
        "schema trigger must reject a work event without work_id"
    );
}
