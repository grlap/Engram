use chrono::{TimeDelta, TimeZone};

use super::*;
use crate::storage::test_support::*;
use crate::*;

use crate::{
    DevelopmentNoopRedactor,
    domain::{EffectClass, NoteVisibility, ProjectId, SessionPhase, TurnIntent, TurnPurpose},
};

#[test]
fn task_cursor_arithmetic_refuses_overflow() {
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open_in_memory().expect("store");
    let binding = bind_control(&mut store, now);
    {
        let transaction = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("cursor snapshot");
        assert!(matches!(
            SqliteStore::task_delta_range_on(
                &transaction,
                binding.status.task_id,
                ChangeCursor(i64::MIN),
                ChangeCursor(i64::MAX),
            ),
            Err(StoreError::InvalidTaskProjection(reason))
                if reason.contains("overflowed")
        ));
        transaction.rollback().expect("rollback cursor snapshot");
    }

    let (object_kind, object_hash) = store
        .connection
        .query_row(
            "SELECT object_kind, object_hash FROM objects ORDER BY object_hash LIMIT 1",
            [],
            |row| Ok((row.get::<_, String>(0)?, row.get::<_, String>(1)?)),
        )
        .expect("existing canonical object");
    store
        .connection
        .execute(
            "DELETE FROM task_changes WHERE task_id = ?1",
            [binding.status.task_id.0.to_string()],
        )
        .expect("clear task feed fixture");
    store
        .connection
        .execute(
            "INSERT INTO task_changes (task_id, task_cursor, object_kind, object_hash)
             VALUES (?1, ?2, ?3, ?4)",
            params![
                binding.status.task_id.0.to_string(),
                i64::MAX,
                object_kind,
                object_hash
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
fn append_is_idempotent_and_round_trips_verified_content() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let value = Example {
        title: "Decision".into(),
        body: "Freeze reports before publishing.".into(),
    };

    let first = store.append("memory_version", &value).unwrap();
    let second = store.append("memory_version", &value).unwrap();
    let loaded: Example = store.get(first.hash()).unwrap().unwrap();

    assert_eq!(first, second);
    assert_eq!(loaded, value);
    assert_eq!(
        store.verify_all().unwrap(),
        IntegrityReport {
            checked_objects: 4,
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
fn object_kind_is_bound_to_the_content_address() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let value = Example {
        title: "Decision".into(),
        body: "Task memory is shared by default.".into(),
    };

    store.append("memory_version", &value).unwrap();
    let mismatch = store.append("report", &value);

    assert!(matches!(
        mismatch,
        Err(StoreError::ObjectKindMismatch { .. })
    ));
}

#[test]
fn task_changes_are_ordered_and_idempotent() {
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
    let (_, replay_cursor) = store
        .append_task_object(task_id, "memory_version", &first)
        .unwrap();
    let (second_object, second_cursor) = store
        .append_task_object(task_id, "memory_version", &second)
        .unwrap();

    assert_eq!(first_cursor, replay_cursor);
    assert!(second_cursor > first_cursor);
    assert_eq!(
        store
            .task_changes_since(task_id, first_cursor, 100)
            .unwrap(),
        vec![TaskChange {
            cursor: second_cursor,
            task_id,
            object_kind: "memory_version".into(),
            object_hash: second_object.hash().clone(),
        }]
    );
    assert_ne!(first_object.hash(), second_object.hash());
}

#[test]
fn task_local_cursors_keep_exact_host_delivery_dense_across_interleaved_tasks() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let binding = bind_control(&mut store, now);
    let task_a = binding.status.task_id;
    let task_b = store
        .start_task(
            &ProjectId("project-a".into()),
            "dummy:CONTROL-HOST-2",
            "Interleave another task",
            &SessionId("other-session".into()),
            actor("other-session"),
            now + TimeDelta::milliseconds(1),
        )
        .expect("second task")
        .task
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
                "Decision: task A delivery remains exact.",
                "interleaved-a",
                NoteVisibility::Shared,
            ),
            &DevelopmentNoopRedactor,
        )
        .expect("task A note");

    for task_id in [task_a, task_b] {
        let changes = store
            .task_changes_since(task_id, ChangeCursor(0), 100)
            .expect("task-local changes");
        assert!(changes.iter().enumerate().all(|(offset, change)| {
            change.cursor.0 == i64::try_from(offset).expect("small test offset") + 1
        }));
    }

    let decision = store
        .evaluate_control_turn(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &TurnIntent {
                idempotency_key: "interleaved-turn-a".into(),
                intent_fingerprint: ObjectHash::from_canonical_bytes(b"interleaved-turn-a"),
                purpose: TurnPurpose::Ordinary,
                requested_effects: vec![EffectClass::Observe],
                resource_intents: Vec::new(),
            },
            now + TimeDelta::seconds(1),
        )
        .expect("evaluate exact task A delivery");
    let ControlTurnDecision::Grant { grant } = decision else {
        panic!("interleaved task delivery must grant");
    };
    let delivery = grant.delivery.as_ref().expect("initial exact delta");
    assert!(
        delivery
            .delta
            .changes
            .iter()
            .enumerate()
            .all(|(offset, change)| {
                change.cursor.0 == i64::try_from(offset).expect("small test offset") + 1
            })
    );
    assert!(crate::control::delivery_matches_grant(&grant));
}

#[test]
fn host_delivery_refuses_a_gap_in_the_task_local_feed() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let binding = bind_control(&mut store, now);
    store
        .capture_note(
            &note_request(
                binding.status.task_id,
                "control-session",
                "Decision: create a second task-local event.",
                "gap-note-a",
                NoteVisibility::Shared,
            ),
            &DevelopmentNoopRedactor,
        )
        .expect("task note");
    let head = store
        .connection
        .query_row(
            "SELECT COALESCE(MAX(task_cursor), 0)
             FROM task_changes WHERE task_id = ?1",
            [binding.status.task_id.0.to_string()],
            |row| row.get::<_, i64>(0).map(ChangeCursor),
        )
        .expect("task head");
    assert!(head.0 > 1);
    store
        .connection
        .execute(
            "DELETE FROM task_changes WHERE task_id = ?1 AND task_cursor = 1",
            [binding.status.task_id.0.to_string()],
        )
        .expect("create corrupt task-feed gap");
    assert!(matches!(
        store.evaluate_control_turn(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &TurnIntent {
                idempotency_key: "gapped-turn".into(),
                intent_fingerprint: ObjectHash::from_canonical_bytes(b"gapped-turn"),
                purpose: TurnPurpose::Ordinary,
                requested_effects: vec![EffectClass::Observe],
                resource_intents: Vec::new(),
            },
            now + TimeDelta::seconds(1),
        ),
        Err(StoreError::InvalidTaskProjection(_))
    ));
}

#[test]
fn another_agents_private_capture_does_not_invalidate_or_enter_a_grant() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let binding = bind_control(&mut store, now);
    let peer_session = SessionId("private-peer".into());
    store
        .join_task(
            &ProjectId("project-a".into()),
            "dummy:CONTROL-HOST-1",
            &peer_session,
            actor("private-peer"),
            now,
        )
        .expect("join private peer");
    let decision = store
        .evaluate_control_turn(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &TurnIntent {
                idempotency_key: "owner-scoped-private-grant".into(),
                intent_fingerprint: ObjectHash::from_canonical_bytes(b"owner-scoped-private-grant"),
                purpose: TurnPurpose::Ordinary,
                requested_effects: vec![EffectClass::Observe],
                resource_intents: Vec::new(),
            },
            now + TimeDelta::seconds(1),
        )
        .expect("evaluate owner context");
    let ControlTurnDecision::Grant { grant } = decision else {
        panic!("owner context must grant");
    };
    let peer_private = store
        .capture_note(
            &note_request(
                binding.status.task_id,
                "private-peer",
                "Constraint: only the peer may see this private rule.",
                "peer-private-after-grant",
                NoteVisibility::Private,
            ),
            &DevelopmentNoopRedactor,
        )
        .expect("capture peer-private memory");
    assert_eq!(peer_private.cursor, None);
    let token = grant
        .delivery
        .as_ref()
        .expect("context delivery")
        .page
        .delivery_token
        .clone();
    assert!(matches!(
        store
            .begin_control_turn(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &grant.grant_id,
                &[token],
                "begin-after-peer-private",
                now + TimeDelta::seconds(2),
            )
            .expect("peer-private state cannot invalidate owner context"),
        ControlTurnBeginDecision::Begin { .. }
    ));
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one recovery scenario proves every page advances and converges to an ordinary turn"
)]
fn recovery_turns_drain_a_bounded_backlog_before_ordinary_work_resumes() {
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open_in_memory().expect("store");
    let binding = bind_control(&mut store, now);
    for index in 0..(MAX_CONTROL_DELIVERY_EVENTS * 2) {
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
    assert!(matches!(
        store
            .evaluate_control_turn(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &TurnIntent {
                    idempotency_key: "ordinary-before-recovery".into(),
                    intent_fingerprint: ObjectHash::from_canonical_bytes(
                        b"ordinary-before-recovery",
                    ),
                    purpose: TurnPurpose::Ordinary,
                    requested_effects: vec![EffectClass::Observe],
                    resource_intents: Vec::new(),
                },
                now + TimeDelta::milliseconds(1),
            )
            .expect("ordinary backlog decision"),
        ControlTurnDecision::Refuse { directive }
            if directive.code == crate::domain::ControlRefusalCode::RecoveryRequired
    ));

    let mut saw_partial = false;
    let mut pages = 0_i64;
    loop {
        pages += 1;
        assert!(pages <= 5, "bounded recovery must converge");
        let decision = store
            .evaluate_control_turn(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &TurnIntent {
                    idempotency_key: format!("recovery-page-{pages}"),
                    intent_fingerprint: ObjectHash::from_canonical_bytes(
                        format!("recovery-page-{pages}").as_bytes(),
                    ),
                    purpose: TurnPurpose::Recovery,
                    requested_effects: vec![EffectClass::Observe],
                    resource_intents: Vec::new(),
                },
                now + TimeDelta::seconds(pages),
            )
            .expect("recovery page decision");
        let ControlTurnDecision::Grant { grant } = decision else {
            panic!("recovery page must grant, got {decision:?}");
        };
        let delivery = grant.delivery.as_ref().expect("recovery delivery");
        assert!(
            delivery.delta.changes.len()
                <= usize::try_from(MAX_CONTROL_DELIVERY_EVENTS).expect("positive event budget")
        );
        if delivery.page.has_more {
            saw_partial = true;
            assert!(delivery.context.is_none());
        } else {
            assert!(delivery.context.is_some());
        }
        let begun = store
            .begin_control_turn(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &grant.grant_id,
                std::slice::from_ref(&delivery.page.delivery_token),
                &format!("begin-recovery-page-{pages}"),
                now + TimeDelta::seconds(pages) + TimeDelta::milliseconds(1),
            )
            .expect("begin recovery page");
        assert!(matches!(begun, ControlTurnBeginDecision::Begin { .. }));
        let checkpoint = store
            .checkpoint_control_turn(
                &ProjectId("project-a".into()),
                &binding.status.session_id,
                &binding.connection_token,
                &binding.routing_token,
                &grant.grant_id,
                TurnNextIntent::Continue,
                &format!("checkpoint-recovery-page-{pages}"),
                now + TimeDelta::seconds(pages) + TimeDelta::milliseconds(2),
            )
            .expect("checkpoint recovery page");
        let ControlTurnCheckpointDecision::Checkpointed { receipt } = checkpoint else {
            panic!("recovery page must checkpoint");
        };
        if !delivery.page.has_more {
            assert_eq!(receipt.phase, SessionPhase::Ready);
            break;
        }
        assert_eq!(receipt.phase, SessionPhase::SyncRequired);
    }
    assert!(saw_partial);

    let ordinary = store
        .evaluate_control_turn(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &TurnIntent {
                idempotency_key: "ordinary-after-recovery".into(),
                intent_fingerprint: ObjectHash::from_canonical_bytes(b"ordinary-after-recovery"),
                purpose: TurnPurpose::Ordinary,
                requested_effects: vec![EffectClass::Observe],
                resource_intents: Vec::new(),
            },
            now + TimeDelta::seconds(10),
        )
        .expect("ordinary turn after recovery");
    let ControlTurnDecision::Grant { grant } = ordinary else {
        panic!("ordinary turn must be granted after recovery");
    };
    let delivery = grant
        .delivery
        .expect("ordinary turn carries a context-only delivery basis");
    assert_eq!(delivery.page.from_cursor, delivery.page.to_cursor);
    assert_eq!(delivery.page.to_cursor, delivery.page.head_cursor);
    assert!(!delivery.page.has_more);
    assert!(delivery.delta.changes.is_empty());
    assert!(delivery.context.is_some());

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

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the restart scenario keeps frozen delivery, cursor, fencing, and checkpoint assertions together"
)]
fn begun_partial_recovery_is_exactly_redeliverable_after_host_restart() {
    let directory = tempfile::tempdir().expect("tempdir");
    let database = directory.path().join("engram.db");
    let now = Utc.timestamp_millis_opt(1_700_000_000_000).unwrap();
    let mut store = SqliteStore::open(&database).expect("store");
    let binding = bind_control(&mut store, now);
    for index in 0..(MAX_CONTROL_DELIVERY_EVENTS * 2) {
        store
            .append_task_object(
                binding.status.task_id,
                "restart_backlog_event",
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
                idempotency_key: "restart-recovery-page".into(),
                intent_fingerprint: ObjectHash::from_canonical_bytes(b"restart-recovery-page"),
                purpose: TurnPurpose::Recovery,
                requested_effects: vec![EffectClass::Observe],
                resource_intents: Vec::new(),
            },
            now + TimeDelta::seconds(1),
        )
        .expect("recovery decision");
    let ControlTurnDecision::Grant { grant } = decision else {
        panic!("partial recovery must grant");
    };
    let delivery = grant.delivery.as_ref().expect("delivery");
    assert!(delivery.page.has_more);
    store
        .begin_control_turn(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &grant.grant_id,
            std::slice::from_ref(&delivery.page.delivery_token),
            "begin-restart-recovery-page",
            now + TimeDelta::seconds(2),
        )
        .expect("begin recovery");
    store
        .append_task_object(
            binding.status.task_id,
            "post_begin_event",
            &Example {
                title: "later".into(),
                body: "must remain pending".into(),
            },
        )
        .expect("append event after begin");
    drop(store);

    let mut reopened = SqliteStore::open(&database).expect("reopen store");
    let connection_token = reopened
        .resume_control_connection(&binding.status.session_id, now + TimeDelta::seconds(3))
        .expect("resume host connection");
    let status = reopened
        .control_status(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &connection_token,
            &binding.routing_token,
            now + TimeDelta::seconds(3),
        )
        .expect("status after restart");
    assert_eq!(status.phase, SessionPhase::TurnOpen);
    assert_eq!(status.confirmed_cursor, binding.status.confirmed_cursor);
    assert_eq!(status.tentative_cursor, Some(grant.basis.delivery_cursor));
    assert_eq!(status.recoverable_grant.as_deref(), Some(grant.as_ref()));
    assert!(matches!(
        reopened.checkpoint_control_turn(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &binding.connection_token,
            &binding.routing_token,
            &grant.grant_id,
            TurnNextIntent::Continue,
            "checkpoint-from-superseded-host",
            now + TimeDelta::seconds(4),
        ),
        Err(StoreError::ControlConnectionSuperseded(_))
    ));

    let checkpoint = reopened
        .checkpoint_control_turn(
            &ProjectId("project-a".into()),
            &binding.status.session_id,
            &connection_token,
            &binding.routing_token,
            &grant.grant_id,
            TurnNextIntent::Continue,
            "checkpoint-redelivered-recovery-page",
            now + TimeDelta::seconds(4),
        )
        .expect("checkpoint exact redelivery");
    let ControlTurnCheckpointDecision::Checkpointed { receipt } = checkpoint else {
        panic!("redelivered page must checkpoint");
    };
    assert_eq!(receipt.confirmed_cursor, grant.basis.delivery_cursor);
    assert_eq!(receipt.phase, SessionPhase::SyncRequired);
}

#[test]
fn live_task_claims_are_atomic_across_connections() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("engram.db");
    let mut first_store = SqliteStore::open(&database).unwrap();
    let mut peer_store = SqliteStore::open(&database).unwrap();
    let task_id = TaskId::new();
    let now = Utc::now();
    let first = first_store
        .claim_task(
            task_id,
            &SessionId("session-a".into()),
            "claim-a",
            now,
            300,
            actor("session-a"),
        )
        .unwrap();
    let replay = first_store
        .claim_task(
            task_id,
            &SessionId("session-a".into()),
            "claim-a",
            now + TimeDelta::seconds(2),
            300,
            actor("session-a"),
        )
        .unwrap();
    let conflict = peer_store.claim_task(
        task_id,
        &SessionId("session-b".into()),
        "claim-b",
        now,
        300,
        actor("session-b"),
    );

    assert_eq!(first, replay);
    assert!(matches!(conflict, Err(StoreError::TaskClaimHeld { .. })));
    assert!(matches!(
        first_store.claim_task(
            task_id,
            &SessionId("session-a".into()),
            "claim-a",
            now,
            360,
            actor("session-a"),
        ),
        Err(StoreError::ClaimIdempotencyConflict(_))
    ));

    let after_expiry = first.expires_at + TimeDelta::milliseconds(1);
    let peer = peer_store
        .claim_task(
            task_id,
            &SessionId("session-b".into()),
            "claim-b-after-expiry",
            after_expiry,
            300,
            actor("session-b"),
        )
        .unwrap();

    assert_eq!(peer.revision, first.revision + 1);
    assert_eq!(
        peer_store
            .task_changes_since(task_id, ChangeCursor::default(), 100)
            .unwrap()
            .len(),
        2
    );
}
