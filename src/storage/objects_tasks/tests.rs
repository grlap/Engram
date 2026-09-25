use chrono::{TimeDelta, TimeZone, Utc};

use super::*;
use crate::storage::test_support::*;
use crate::*;

use crate::{
    DevelopmentNoopRedactor,
    domain::{EffectClass, NoteVisibility, ProjectId, TurnIntent},
};

#[test]
fn task_cursor_arithmetic_refuses_overflow() {
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open_in_memory().expect("store");
    let binding = bind_control(&mut store, now);
    let (object_kind, object_id) = store
        .connection
        .query_row(
            "SELECT object_kind, object_id FROM objects ORDER BY object_id LIMIT 1",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .expect("existing canonical object");
    store
        .connection
        .execute(
            "DELETE FROM control_changes WHERE task_id = ?1",
            [binding.status.task_id.0.to_string()],
        )
        .expect("clear task feed fixture");
    store
        .connection
        .execute(
            "INSERT INTO control_changes (task_id, task_cursor, object_kind, object_id)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                binding.status.task_id.0.to_string(),
                i64::MAX,
                object_kind,
                object_id
            ],
        )
        .expect("install maximum cursor");
    assert!(matches!(
        store.append_task_object(
            binding.status.task_id,
            "cursor_overflow_event",
            &Example {
                title: "overflow".into(),
                body: "must refuse".into(),
            },
        ),
        Err(StoreError::InvalidTaskProjection(reason))
            if reason.contains("cursor overflowed")
    ));
}

#[test]
fn append_mints_a_record_per_call_and_round_trips_content() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let value = Example {
        title: "Decision".into(),
        body: "Freeze reports before publishing.".into(),
    };

    let first = store.append("memory_version", &value).unwrap();
    let second = store.append("memory_version", &value).unwrap();
    let loaded: Example = store.get(first.key()).unwrap().unwrap();

    assert_ne!(first.key(), second.key());
    assert_eq!(first.bytes(), second.bytes());
    assert_eq!(loaded, value);
    assert_eq!(
        store.verify_all().unwrap(),
        IntegrityReport {
            snapshot: crate::storage::IntegritySnapshot {
                object_count: 5,
                project_feed_heads: Vec::new(),
            },
            checked_objects: 5,
            invalid_objects: Vec::new(),
            checked_graph_snapshot_audits: 0,
            invalid_graph_snapshot_audits: Vec::new(),
            checked_control_records: 2,
            invalid_control_records: Vec::new(),
            checked_work_records: 1,
            invalid_work_records: Vec::new(),
        }
    );
}

#[test]
fn a_stored_id_keeps_its_kind_and_bytes() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let value = Example {
        title: "Decision".into(),
        body: "Task memory is shared by default.".into(),
    };

    let stored = store.append("memory_version", &value).unwrap();
    SqliteStore::insert_object(&store.connection, "memory_version", &stored)
        .expect("the same record under its own id is a replay");
    assert!(matches!(
        SqliteStore::insert_object(&store.connection, "report", &stored),
        Err(StoreError::ObjectKindMismatch { .. })
    ));
    let other = CanonicalObject::identified(
        stored.key(),
        &Example {
            title: "Decision".into(),
            body: "Different content under a taken id.".into(),
        },
    )
    .unwrap();
    assert!(matches!(
        SqliteStore::insert_object(&store.connection, "memory_version", &other),
        Err(StoreError::ImmutableCollision(_))
    ));
}

#[test]
fn control_changes_are_ordered() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let task_id = TaskId::new();
    let first = Example {
        title: "Decision".into(),
        body: "Task memory is shared by default.".into(),
    };
    let second = Example {
        title: "Evidence".into(),
        body: "A peer confirmed the decision.".into(),
    };

    let (first_object, first_cursor) = store
        .append_task_object(task_id, "memory_version", &first)
        .unwrap();
    let (second_object, second_cursor) = store
        .append_task_object(task_id, "memory_version", &second)
        .unwrap();

    assert!(second_cursor > first_cursor);
    assert_eq!(
        store
            .control_changes_since(task_id, first_cursor, 100)
            .unwrap(),
        vec![TaskChange {
            cursor: second_cursor,
            task_id,
            object_kind: "memory_version".into(),
            object_id: second_object.key().clone(),
        }]
    );
    assert_ne!(first_object.key(), second_object.key());
}

#[test]
fn task_local_cursors_stay_dense_across_interleaved_tasks() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let binding = bind_control(&mut store, now);
    let task_a = binding.status.task_id;
    let task_b = store
        .bind_test_control_scope(
            &ProjectId("project-a".into()),
            "dummy:CONTROL-HOST-2",
            "Interleave another task",
            &SessionId("other-session".into()),
            &actor("other-session"),
            now + TimeDelta::milliseconds(1),
        )
        .expect("second task")
        .status
        .task_id;
    store
        .capture_note(
            &note_request(
                task_b,
                "other-session",
                "Decision: task B advances independently.",
                "interleaved-b",
                NoteVisibility::Shared,
            ),
            &DevelopmentNoopRedactor,
        )
        .expect("task B note");
    store
        .capture_note(
            &note_request(
                task_a,
                "control-session",
                "Decision: task A stays dense.",
                "interleaved-a",
                NoteVisibility::Shared,
            ),
            &DevelopmentNoopRedactor,
        )
        .expect("task A note");

    for task_id in [task_a, task_b] {
        let changes = store
            .control_changes_since(task_id, ChangeCursor(0), 100)
            .expect("task-local changes");
        assert!(!changes.is_empty());
        assert!(changes.iter().enumerate().all(|(offset, change)| {
            change.cursor.0 == i64::try_from(offset).expect("small test offset") + 1
        }));
    }
}

// The task's change index is an audit trail no grant delivers, so a backlog
// of any length leaves an ordinary turn free to go at once.
#[test]
fn a_long_task_backlog_does_not_hold_an_ordinary_turn() {
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open_in_memory().expect("store");
    let binding = bind_control(&mut store, now);
    for index in 0..256 {
        store
            .append_task_object(
                binding.status.task_id,
                "backlog_test_event",
                &Example {
                    title: format!("event-{index}"),
                    body: "bounded".into(),
                },
            )
            .expect("append backlog event");
    }
    let decision = store
        .evaluate_control_turn(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &TurnIntent {
                idempotency_key: "ordinary-after-backlog".into(),
                intent_fingerprint: ObjectId::from_canonical_bytes(b"ordinary-after-backlog"),
                purpose: None,
                requested_effects: vec![EffectClass::Observe],
                resource_intents: Vec::new(),
            },
            now + TimeDelta::milliseconds(1),
        )
        .expect("ordinary decision after a backlog");
    let ControlTurnDecision::Grant { grant } = decision else {
        panic!("a backlog must not hold an ordinary turn, got {decision:?}");
    };
    assert!(grant.delivery.is_none());
    assert!(grant.basis.inline_delivery.is_none());
}

#[test]
fn an_oversized_task_event_is_refused() {
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open_in_memory().expect("store");
    let binding = bind_control(&mut store, now);
    let oversized = store.append_task_object(
        binding.status.task_id,
        "oversized_test_event",
        &Example {
            title: "oversized".into(),
            body: "x".repeat(MAX_TASK_CHANGE_OBJECT_BYTES + 1),
        },
    );
    assert!(matches!(
        oversized,
        Err(StoreError::InvalidTaskProjection(_))
    ));
}
