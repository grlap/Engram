use chrono::{TimeDelta, TimeZone, Utc};

use super::*;
use crate::storage::{enum_name, test_database_shape_snapshot, test_support::*};
use crate::*;

use crate::{
    DevelopmentNoopRedactor,
    domain::{MemoryStatus, NoteVisibility, ProjectId, ProvenanceLink, ProvenanceRelation},
};

#[test]
fn sessions_rendezvous_using_only_the_external_reference() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let project = ProjectId("project-a".into());
    let now = Utc::now();
    let first = store
        .bind_test_control_scope(
            &project,
            "dummy:TASK-7",
            "Dogfood the memory loop",
            &SessionId("eval-a".into()),
            &actor("eval-a"),
            now,
        )
        .unwrap();
    let peer = store
        .bind_test_control_scope(
            &project,
            "dummy:TASK-7",
            "Peer control scope",
            &SessionId("eval-b".into()),
            &actor("eval-b"),
            now + TimeDelta::milliseconds(1),
        )
        .unwrap();
    let replay = store
        .bind_test_control_scope(
            &project,
            "dummy:TASK-7",
            "Peer control scope",
            &SessionId("eval-b".into()),
            &actor("eval-b"),
            now + TimeDelta::milliseconds(2),
        )
        .unwrap();

    assert_eq!(first.status.task_id, peer.status.task_id);
    assert_eq!(peer.status.task_id, replay.status.task_id);
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT COUNT(*) FROM control_sessions WHERE task_id = ?1",
                [first.status.task_id.0.to_string()],
                |row| row.get::<_, i64>(0),
            )
            .unwrap(),
        2
    );
    assert_eq!(
        store
            .control_changes_since(first.status.task_id, ChangeCursor::default(), 20)
            .unwrap()
            .len(),
        0
    );
    let other = store
        .bind_test_control_scope(
            &project,
            "dummy:MISSING",
            "Peer control scope",
            &SessionId("eval-c".into()),
            &actor("eval-c"),
            now,
        )
        .unwrap();
    assert_ne!(other.status.task_id, first.status.task_id);
}

#[test]
fn generic_memory_actor_context_validation_and_redaction_are_non_mutating() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let task_id = TaskId::new();
    install_memory_task(&mut store, task_id, &["context-agent"]);
    let before = test_database_shape_snapshot(&store.connection).expect("initial shape");
    let mut request = note_request(
        task_id,
        "context-agent",
        "Decision: context admission remains explicit.",
        "actor-context-redaction",
        NoteVisibility::Shared,
    );
    request.actor.provenance_chain.push(ProvenanceLink {
        relation: ProvenanceRelation::DerivedFrom,
        source: "model=reject-me-context".into(),
        reference: Some(crate::domain::ACTOR_CONTEXT_PROVENANCE_REFERENCE.into()),
    });

    assert!(matches!(
        store.capture_note(&request, &SentinelRedactor),
        Err(StoreError::RedactionRefused(message)) if message == "test sentinel was rejected"
    ));

    let mut duplicate_context = request.clone();
    duplicate_context
        .actor
        .provenance_chain
        .push(ProvenanceLink {
            relation: ProvenanceRelation::DerivedFrom,
            source: "model=second-context".into(),
            reference: Some(crate::domain::ACTOR_CONTEXT_PROVENANCE_REFERENCE.into()),
        });
    assert!(matches!(
        store.capture_note(&duplicate_context, &DevelopmentNoopRedactor),
        Err(StoreError::InvalidMemoryProjection(detail)) if detail.contains("at most one value")
    ));

    let normalized_marker = || ProvenanceLink {
        relation: ProvenanceRelation::DerivedFrom,
        source: "actor_context:normalized".into(),
        reference: Some(crate::domain::ACTOR_CONTEXT_NORMALIZED_REFERENCE.into()),
    };
    let mut duplicate_marker = note_request(
        task_id,
        "context-agent",
        "Decision: normalization provenance is unique.",
        "actor-context-duplicate-marker",
        NoteVisibility::Shared,
    );
    duplicate_marker
        .actor
        .provenance_chain
        .extend([normalized_marker(), normalized_marker()]);
    assert!(matches!(
        store.capture_note(&duplicate_marker, &DevelopmentNoopRedactor),
        Err(StoreError::InvalidMemoryProjection(detail)) if detail.contains("must be unique")
    ));

    let mut forged_marker = note_request(
        task_id,
        "context-agent",
        "Decision: normalization provenance is exact.",
        "actor-context-forged-marker",
        NoteVisibility::Shared,
    );
    forged_marker.actor.provenance_chain.push(ProvenanceLink {
        relation: ProvenanceRelation::DerivedFrom,
        source: "actor_context:forged".into(),
        reference: Some(crate::domain::ACTOR_CONTEXT_NORMALIZED_REFERENCE.into()),
    });
    assert!(matches!(
        store.capture_note(&forged_marker, &DevelopmentNoopRedactor),
        Err(StoreError::InvalidMemoryProjection(detail)) if detail.contains("is invalid")
    ));

    let mut unsafe_context = note_request(
        task_id,
        "context-agent",
        "Decision: retained context is terminal safe.",
        "actor-context-unsafe",
        NoteVisibility::Shared,
    );
    unsafe_context.actor.provenance_chain.push(ProvenanceLink {
        relation: ProvenanceRelation::DerivedFrom,
        source: "model=line\nbreak".into(),
        reference: Some(crate::domain::ACTOR_CONTEXT_PROVENANCE_REFERENCE.into()),
    });
    assert!(matches!(
        store.capture_note(&unsafe_context, &DevelopmentNoopRedactor),
        Err(StoreError::InvalidMemoryProjection(detail))
            if detail.contains("not normalized and bounded")
    ));
    assert_eq!(
        test_database_shape_snapshot(&store.connection).expect("shape after refusals"),
        before,
        "invalid or redacted generic-memory attribution must not mutate the store"
    );
}

#[test]
fn note_capture_is_idempotent_searchable_and_explainable() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let task_id = TaskId::new();
    install_memory_task(&mut store, task_id, &["session-a", "session-b"]);
    let request = note_request(
        task_id,
        "session-a",
        "Decision: use canonical task memory as the shared source",
        "note-a",
        NoteVisibility::Shared,
    );

    let first = store
        .capture_note(&request, &DevelopmentNoopRedactor)
        .unwrap();
    let mut retry_request = request.clone();
    retry_request.created_at += TimeDelta::seconds(1);
    let replay = store
        .capture_note(&retry_request, &DevelopmentNoopRedactor)
        .unwrap();
    let mut restricted_request = request.clone();
    restricted_request.prose = "restricted: never return this task memory body".into();
    restricted_request.sensitivity = Some(Sensitivity::Restricted);
    restricted_request.idempotency_key = "note-restricted".into();
    let restricted = store
        .capture_note(&restricted_request, &DevelopmentNoopRedactor)
        .expect("capture restricted task memory");
    let visible = store
        .search_memories(
            &request.project_id,
            Some(task_id),
            None,
            &SessionId("session-b".into()),
            "session-b",
            Some("canonical source"),
            20,
        )
        .unwrap();

    assert_eq!(first.memory_id, replay.memory_id);
    assert!(!first.duplicate);
    assert!(replay.duplicate);
    assert_eq!(first.status, MemoryStatus::Active);
    assert_eq!(first.kind, crate::domain::MemoryKind::Decision);
    assert_eq!(visible.len(), 1);
    assert_eq!(visible[0].version, first.version);
    assert_ne!(visible[0].version, restricted.version);
    assert!(first.cursor.is_some());

    let mut conflict = request.clone();
    conflict.prose = "Decision: reuse the key for something else".into();
    assert!(matches!(
        store.capture_note(&conflict, &DevelopmentNoopRedactor),
        Err(StoreError::NoteIdempotencyConflict(_))
    ));
}

#[test]
fn note_idempotency_keys_are_scoped_to_the_calling_session() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let task_id = TaskId::new();
    install_memory_task(&mut store, task_id, &["session-a", "session-b"]);
    let first = note_request(
        task_id,
        "session-a",
        "Decision: first caller meaning",
        "local-retry-1",
        NoteVisibility::Shared,
    );
    let second = note_request(
        task_id,
        "session-b",
        "Decision: second caller meaning",
        "local-retry-1",
        NoteVisibility::Shared,
    );

    let first = store
        .capture_note(&first, &DevelopmentNoopRedactor)
        .expect("first caller-local key");
    let second = store
        .capture_note(&second, &DevelopmentNoopRedactor)
        .expect("same raw key is independent in another session");

    assert_ne!(first.memory_id, second.memory_id);
    assert_eq!(first.idempotency_key, second.idempotency_key);
}

#[test]
fn private_task_scratch_never_enters_the_peer_feed() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let task_id = TaskId::new();
    install_memory_task(&mut store, task_id, &["agent-a", "agent-b"]);
    let request = note_request(
        task_id,
        "agent-a",
        "Hypothesis: the failure may be environmental.",
        "private-a",
        NoteVisibility::Private,
    );
    let receipt = store
        .capture_note(&request, &DevelopmentNoopRedactor)
        .unwrap();

    assert!(receipt.cursor.is_none());
    assert_eq!(
        store
            .search_memories(
                &request.project_id,
                Some(task_id),
                None,
                &SessionId("agent-a".into()),
                "agent-a",
                None,
                20,
            )
            .unwrap()
            .len(),
        1
    );
    assert!(
        store
            .search_memories(
                &request.project_id,
                Some(task_id),
                None,
                &SessionId("agent-b".into()),
                "agent-b",
                None,
                20,
            )
            .unwrap()
            .is_empty()
    );
    assert!(
        store
            .control_changes_since(task_id, ChangeCursor::default(), 20)
            .unwrap()
            .is_empty()
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one scenario must preserve the exact pre/post-restart cursor and hashes"
)]
fn task_delta_show_and_private_scope_survive_restart() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("engram.db");
    let project = ProjectId("project-a".into());
    let session_a = SessionId("eval-a".into());
    let session_b = SessionId("eval-b".into());
    let now = Utc::now();
    let (task_id, first_receipt, first_cursor, expected_delta, private_hash) = {
        let mut store = SqliteStore::open(&database).unwrap();
        let task = store
            .bind_test_control_scope(
                &project,
                "dummy:TASK-7",
                "Dogfood",
                &session_a,
                &actor("eval-a"),
                now,
            )
            .unwrap();
        let task_id = task.status.task_id;
        store
            .bind_test_control_scope(
                &project,
                "dummy:TASK-7",
                "Peer control scope",
                &session_b,
                &actor("eval-b"),
                now + TimeDelta::milliseconds(1),
            )
            .unwrap();
        let first_request = note_request(
            task_id,
            "eval-a",
            "Decision: freeze one report payload per retry key",
            "first",
            NoteVisibility::Shared,
        );
        let first_receipt = store
            .capture_note(&first_request, &DevelopmentNoopRedactor)
            .unwrap();
        let first_cursor = first_receipt
            .cursor
            .expect("a shared note has a task cursor");

        let second_request = note_request(
            task_id,
            "eval-a",
            "Evidence: retry integration test returns byte-identical content",
            "second",
            NoteVisibility::Shared,
        );
        store
            .capture_note(&second_request, &DevelopmentNoopRedactor)
            .unwrap();
        let expected_delta = store
            .task_delta(&project, task_id, &session_b, "eval-b", first_cursor, 20)
            .unwrap();
        assert_eq!(expected_delta.changes.len(), 1);

        let private_request = note_request(
            task_id,
            "eval-a",
            "scratch: half-formed hypothesis Z",
            "private",
            NoteVisibility::Private,
        );
        let private_receipt = store
            .capture_note(&private_request, &DevelopmentNoopRedactor)
            .unwrap();
        assert!(matches!(
            store.show_memory(
                &private_receipt.version,
                &project,
                Some(task_id),
                None,
                &session_b,
                "eval-b",
            ),
            Err(StoreError::MemoryAccessDenied(_))
        ));
        assert!(
            store
                .search_memories(
                    &project,
                    Some(task_id),
                    None,
                    &session_b,
                    "eval-b",
                    Some("hypothesis Z"),
                    20,
                )
                .unwrap()
                .is_empty()
        );
        (
            task_id,
            first_receipt,
            first_cursor,
            expected_delta,
            private_receipt.version,
        )
    };

    let reopened = SqliteStore::open(&database).unwrap();
    let after_restart = reopened
        .task_delta(&project, task_id, &session_b, "eval-b", first_cursor, 20)
        .unwrap();
    assert_eq!(
        serde_json::to_vec(&after_restart).unwrap(),
        serde_json::to_vec(&expected_delta).unwrap()
    );
    let shown = reopened
        .show_memory(
            &first_receipt.version,
            &project,
            Some(task_id),
            None,
            &session_b,
            "eval-b",
        )
        .unwrap();
    assert_eq!(shown.version.actor.session_id, Some(session_a));
    assert!(!shown.version.classification_reason.is_empty());
    assert!(matches!(
        reopened.show_memory(
            &private_hash,
            &project,
            Some(task_id),
            None,
            &session_b,
            "eval-b",
        ),
        Err(StoreError::MemoryAccessDenied(_))
    ));
}

#[test]
fn memory_projection_rebuilds_from_canonical_objects() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let task_id = TaskId::new();
    install_memory_task(&mut store, task_id, &["agent-a", "agent-b"]);
    let request = note_request(
        task_id,
        "agent-a",
        "Evidence: the integration test passes after restart",
        "evidence-a",
        NoteVisibility::Shared,
    );
    store
        .capture_note(&request, &DevelopmentNoopRedactor)
        .unwrap();

    assert_eq!(store.rebuild_memory_index().unwrap(), 1);
    let rebuilt = store
        .search_memories(
            &request.project_id,
            Some(task_id),
            None,
            &SessionId("agent-b".into()),
            "agent-b",
            Some("integration restart"),
            20,
        )
        .unwrap();
    assert_eq!(rebuilt.len(), 1);
    assert_eq!(rebuilt[0].kind, crate::domain::MemoryKind::Fact);
}

#[test]
fn generic_memory_search_excludes_terminal_head_statuses() {
    for status in [MemoryStatus::Retracted, MemoryStatus::Expired] {
        let mut store = SqliteStore::open_in_memory().expect("store");
        let task_id = TaskId::new();
        install_memory_task(&mut store, task_id, &["agent-a", "agent-b"]);
        let status_name = enum_name(status).expect("status name");
        let request = note_request(
            task_id,
            "agent-a",
            "Fact: terminal visibility must stay out of retrieval",
            &format!("terminal-visibility-{status_name}"),
            NoteVisibility::Shared,
        );
        let receipt = store
            .capture_note(&request, &DevelopmentNoopRedactor)
            .expect("capture active note");
        let version: MemoryVersion = store
            .get_typed_object(&receipt.version, "memory_version")
            .expect("read version")
            .expect("stored version");
        let assertion = MemoryAssertionEvent {
            schema_version: SCHEMA_VERSION,
            memory_id: receipt.memory_id,
            version: receipt.version.clone(),
            status,
            policy_reason: "terminal visibility test".into(),
            actor: actor("agent-a"),
            created_at: Utc::now(),
        };
        let object = CanonicalObject::freeze(&assertion).expect("freeze terminal assertion");
        let transaction = store
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)
            .expect("begin terminal projection");
        SqliteStore::insert_object(&transaction, "memory_assertion_event", &object)
            .expect("insert terminal assertion");
        SqliteStore::apply_memory_projection(
            &transaction,
            &receipt.version,
            object.key(),
            &version,
            &assertion,
            MemoryProjectionMode::Live,
        )
        .expect("apply terminal projection");
        transaction.commit().expect("commit terminal projection");
        assert_eq!(
            store.rebuild_memory_index().expect("rebuild terminal head"),
            2
        );
        assert_eq!(
            store
                .connection
                .query_row(
                    "SELECT status FROM memory_heads WHERE memory_id = ?1",
                    [receipt.memory_id.0.to_string()],
                    |row| row.get::<_, String>(0),
                )
                .expect("rebuilt terminal status"),
            status_name
        );
        assert!(
            store
                .search_memories(
                    &request.project_id,
                    Some(task_id),
                    None,
                    &SessionId("agent-b".into()),
                    "agent-b",
                    Some("terminal visibility"),
                    20,
                )
                .expect("search terminal head")
                .is_empty()
        );
    }
}

fn standalone_note(session: &str, key: &str) -> NoteRequest {
    NoteRequest {
        project_id: ProjectId("project-a".into()),
        task_id: None,
        work_id: None,
        prose: "standalone observation".into(),
        visibility: NoteVisibility::Shared,
        kind: None,
        authority: None,
        sensitivity: None,
        title: None,
        tags: Vec::new(),
        evidence: Vec::new(),
        refs: Vec::new(),
        actor: actor(session),
        idempotency_key: key.into(),
        created_at: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
    }
}

fn refuse_capture_note_before_effects(live: &SessionId) {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let before = test_database_shape_snapshot(&store.connection).expect("before");
    let error = store
        .capture_note(
            &standalone_note(&live.0, "should-not-write"),
            &DevelopmentNoopRedactor,
        )
        .expect_err("oversized note actor");
    assert_oversized_session_refusal(&error, live);
    assert_eq!(
        test_database_shape_snapshot(&store.connection).expect("after"),
        before
    );
}

#[test]
fn capture_note_refuses_an_ascii65_actor_session_before_effects() {
    refuse_capture_note_before_effects(&ascii65_session());
}

#[test]
fn capture_note_refuses_a_utf8_oversized_actor_session_before_effects() {
    refuse_capture_note_before_effects(&utf8_oversized_session());
}

#[test]
fn capture_note_preserves_an_exact_64_byte_actor_session() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let session = exact_64_ascii_session();
    store
        .capture_note(
            &standalone_note(&session, "exact-session-note"),
            &DevelopmentNoopRedactor,
        )
        .expect("admitted note");
    let bytes: Vec<u8> = store
        .connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_kind = 'memory_version'",
            [],
            |row| row.get(0),
        )
        .expect("stored note");
    let version: MemoryVersion = serde_json::from_slice(&bytes).expect("decode note");
    assert_eq!(
        version.actor.session_id.as_ref().map(|id| id.0.as_str()),
        Some(session.as_str())
    );
}

#[test]
fn capture_note_preserves_an_exact_64_byte_utf8_actor_session() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let session = exact_64_utf8_session();
    store
        .capture_note(
            &standalone_note(&session, "exact-utf8-session-note"),
            &DevelopmentNoopRedactor,
        )
        .expect("admitted note");
    let bytes: Vec<u8> = store
        .connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_kind = 'memory_version'",
            [],
            |row| row.get(0),
        )
        .expect("stored note");
    let version: MemoryVersion = serde_json::from_slice(&bytes).expect("decode note");
    assert_eq!(
        version.actor.session_id.as_ref().map(|id| id.0.as_str()),
        Some(session.as_str())
    );
}
