use chrono::{TimeDelta, TimeZone};

use super::*;
use crate::storage::{
    MAX_EXACT_CONTEXT_OMISSIONS, enum_name, test_database_shape_snapshot, test_support::*,
};
use crate::*;

use crate::{
    DevelopmentNoopRedactor,
    domain::{MemoryStatus, NoteVisibility, ProjectId, ProvenanceLink, ProvenanceRelation},
};

#[test]
fn context_omissions_are_exact_then_losslessly_aggregated() {
    let memories = (0..200)
        .map(|index| MemorySummary {
            memory_id: MemoryId::new(),
            version: ObjectHash::from_canonical_bytes(format!("memory-{index}").as_bytes()),
            status: MemoryStatus::Active,
            kind: crate::domain::MemoryKind::Fact,
            authority: crate::domain::Authority::Soft,
            delivery: Delivery::OnDemand,
            scope: Scope::Project {
                project: ProjectId("project-a".into()),
            },
            title: format!("Memory {index}"),
            body: "Available through search".into(),
            sensitivity: Sensitivity::Internal,
            created_at: Utc.timestamp_millis_opt(1_700_000_000_000).unwrap(),
        })
        .collect();
    let assembly = assemble_context(memories, &[]).expect("bounded context assembly");
    assert_eq!(assembly.omissions.len(), MAX_EXACT_CONTEXT_OMISSIONS);
    assert_eq!(
        assembly.omission_summaries,
        vec![ContextOmissionSummary {
            reason: "on-demand memory is available through search".into(),
            count: 72,
        }]
    );
    assert_eq!(
        assembly.omissions.len()
            + assembly
                .omission_summaries
                .iter()
                .map(|summary| usize::try_from(summary.count).unwrap())
                .sum::<usize>(),
        200
    );
}

#[test]
fn context_assembly_never_hides_old_pinned_memory_behind_search_limits() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let task_id = TaskId::new();
    install_memory_task(&store, task_id, &["agent-a"]);
    let mut pinned_request = note_request(
        task_id,
        "agent-a",
        "Constraint: preserve the oldest pinned rule",
        "oldest-pinned",
        NoteVisibility::Shared,
    );
    pinned_request.created_at = Utc::now() - TimeDelta::days(1);
    let pinned = store
        .capture_note(&pinned_request, &DevelopmentNoopRedactor)
        .expect("capture oldest pinned record");
    for index in 0..1_000 {
        let mut request = note_request(
            task_id,
            "agent-a",
            &format!("Observation: bounded filler record {index}"),
            &format!("filler-{index}"),
            NoteVisibility::Shared,
        );
        request.created_at = Utc::now() + TimeDelta::milliseconds(i64::from(index));
        store
            .capture_note(&request, &DevelopmentNoopRedactor)
            .expect("capture filler memory");
    }
    let packet = store
        .build_context(
            &ProjectId("project-a".into()),
            Some(task_id),
            &SessionId("agent-a".into()),
            "agent-a",
            Utc::now(),
        )
        .expect("context includes all pinned candidates before budgeting");
    assert!(
        packet
            .pinned
            .iter()
            .any(|item| item.version == pinned.version)
    );
}

#[test]
fn context_explanation_requires_the_current_task_and_project_binding() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let project = ProjectId("context-auth-project".into());
    let session = SessionId("context-auth-session".into());
    let project_packet = store
        .build_context(&project, None, &session, "context-auth-session", Utc::now())
        .expect("project-only context");
    let first = store
        .start_task(
            &project,
            "dummy:CONTEXT-A",
            "First context",
            &session,
            actor("context-auth-session"),
            Utc::now(),
        )
        .expect("first task");
    assert!(
        store
            .explain_context(
                &project_packet.header.packet_hash,
                &project,
                &session,
                "context-auth-session",
            )
            .is_ok(),
        "an unrelated task binding must not revoke a project-only packet"
    );
    let packet = store
        .build_context(
            &project,
            Some(first.task.task_id),
            &session,
            "context-auth-session",
            Utc::now(),
        )
        .expect("task context");
    assert_eq!(
        store
            .explain_context(
                &packet.header.packet_hash,
                &project,
                &session,
                "context-auth-session",
            )
            .expect("current packet remains explainable")
            .task_id,
        Some(first.task.task_id)
    );

    for (requested_project, requested_session) in [
        (ProjectId("different-project".into()), session.clone()),
        (project.clone(), SessionId("different-session".into())),
    ] {
        assert!(matches!(
            store.explain_context(
                &packet.header.packet_hash,
                &requested_project,
                &requested_session,
                "context-auth-session",
            ),
            Err(StoreError::PacketAccessDenied(_))
        ));
    }

    store
        .start_task(
            &project,
            "dummy:CONTEXT-B",
            "Replacement context",
            &session,
            actor("context-auth-session"),
            Utc::now(),
        )
        .expect("replace active task binding");
    assert!(matches!(
        store.explain_context(
            &packet.header.packet_hash,
            &project,
            &session,
            "context-auth-session",
        ),
        Err(StoreError::PacketAccessDenied(_))
    ));
    store
        .join_task(
            &project,
            "dummy:CONTEXT-A",
            &session,
            actor("context-auth-session"),
            Utc::now(),
        )
        .expect("restore original task binding");
    assert!(
        store
            .explain_context(
                &packet.header.packet_hash,
                &project,
                &session,
                "context-auth-session",
            )
            .is_ok()
    );
}

#[test]
fn sessions_rendezvous_using_only_the_external_reference() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let project = ProjectId("project-a".into());
    let now = Utc::now();
    let first = store
        .start_task(
            &project,
            "dummy:TASK-7",
            "Dogfood the memory loop",
            &SessionId("eval-a".into()),
            actor("eval-a"),
            now,
        )
        .unwrap();
    let peer = store
        .join_task(
            &project,
            "dummy:TASK-7",
            &SessionId("eval-b".into()),
            actor("eval-b"),
            now + TimeDelta::milliseconds(1),
        )
        .unwrap();
    let replay = store
        .join_task(
            &project,
            "dummy:TASK-7",
            &SessionId("eval-b".into()),
            actor("eval-b"),
            now + TimeDelta::milliseconds(2),
        )
        .unwrap();

    assert_eq!(first.task.task_id, peer.task.task_id);
    assert_eq!(peer.task.participants.len(), 2);
    assert_eq!(peer.cursor, replay.cursor);
    assert!(!replay.joined);
    assert_eq!(
        store
            .task_changes_since(first.task.task_id, ChangeCursor::default(), 20)
            .unwrap()
            .len(),
        2
    );
    assert!(matches!(
        store.join_task(
            &project,
            "dummy:MISSING",
            &SessionId("eval-c".into()),
            actor("eval-c"),
            now,
        ),
        Err(StoreError::TaskReferenceNotFound(_))
    ));
}

#[test]
fn generic_memory_actor_context_validation_and_redaction_are_non_mutating() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let task_id = TaskId::new();
    install_memory_task(&store, task_id, &["context-agent"]);
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
fn contradiction_actor_context_is_redactor_inspected() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let task_id = TaskId::new();
    install_memory_task(&store, task_id, &["context-agent"]);
    let left = store
        .capture_note(
            &note_request(
                task_id,
                "context-agent",
                "Constraint: use the first context rule.",
                "context-left",
                NoteVisibility::Shared,
            ),
            &DevelopmentNoopRedactor,
        )
        .expect("left note");
    let right = store
        .capture_note(
            &note_request(
                task_id,
                "context-agent",
                "Constraint: use the second context rule.",
                "context-right",
                NoteVisibility::Shared,
            ),
            &DevelopmentNoopRedactor,
        )
        .expect("right note");
    let mut attribution = actor("context-agent");
    attribution.provenance_chain.push(ProvenanceLink {
        relation: ProvenanceRelation::DerivedFrom,
        source: "model=reject-me-context".into(),
        reference: Some(crate::domain::ACTOR_CONTEXT_PROVENANCE_REFERENCE.into()),
    });
    assert!(matches!(
        store.record_memory_contradiction(
            &ProjectId("project-a".into()),
            Some(task_id),
            None,
            &SessionId("context-agent".into()),
            "context-agent",
            &left.version,
            &right.version,
            "these context rules conflict",
            "context-contradiction",
            attribution.clone(),
            Utc::now(),
            &SentinelRedactor,
        ),
        Err(StoreError::RedactionRefused(message)) if message == "test sentinel was rejected"
    ));

    let before = test_database_shape_snapshot(&store.connection).expect("prepared shape");
    attribution.provenance_chain[0].source = "x".repeat(crate::domain::MAX_ACTOR_CONTEXT_BYTES + 1);
    assert!(matches!(
        store.record_memory_contradiction(
            &ProjectId("project-a".into()),
            Some(task_id),
            None,
            &SessionId("context-agent".into()),
            "context-agent",
            &left.version,
            &right.version,
            "these context rules conflict",
            "context-contradiction-oversized",
            attribution,
            Utc::now(),
            &DevelopmentNoopRedactor,
        ),
        Err(StoreError::InvalidMemoryProjection(detail))
            if detail.contains("not normalized and bounded")
    ));
    assert_eq!(
        test_database_shape_snapshot(&store.connection).expect("shape after refusals"),
        before,
        "invalid or redacted contradiction attribution must not mutate the store"
    );
}

#[test]
fn note_capture_is_idempotent_searchable_and_explainable() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let task_id = TaskId::new();
    install_memory_task(&store, task_id, &["session-a", "session-b"]);
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
    install_memory_task(&store, task_id, &["session-a", "session-b"]);
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
    install_memory_task(&store, task_id, &["agent-a", "agent-b"]);
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
            .task_changes_since(task_id, ChangeCursor::default(), 20)
            .unwrap()
            .is_empty()
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one scenario must preserve the exact pre/post-restart cursor and hashes"
)]
fn context_delta_show_and_private_scope_survive_restart() {
    let directory = tempfile::tempdir().unwrap();
    let database = directory.path().join("engram.db");
    let project = ProjectId("project-a".into());
    let session_a = SessionId("eval-a".into());
    let session_b = SessionId("eval-b".into());
    let now = Utc::now();
    let (task_id, first_receipt, packet, expected_delta, private_hash) = {
        let mut store = SqliteStore::open(&database).unwrap();
        let task = store
            .start_task(
                &project,
                "dummy:TASK-7",
                "Dogfood",
                &session_a,
                actor("eval-a"),
                now,
            )
            .unwrap();
        let task_id = task.task.task_id;
        store
            .join_task(
                &project,
                "dummy:TASK-7",
                &session_b,
                actor("eval-b"),
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
        let packet = store
            .build_context(
                &project,
                Some(task_id),
                &session_b,
                "eval-b",
                now + TimeDelta::milliseconds(2),
            )
            .unwrap();
        assert_eq!(packet.index.len(), 1);
        assert_eq!(packet.index[0].version, first_receipt.version);

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
            .task_delta(
                &project,
                task_id,
                &session_b,
                "eval-b",
                packet.header.event_cursor,
                20,
            )
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
            packet,
            expected_delta,
            private_receipt.version,
        )
    };

    let reopened = SqliteStore::open(&database).unwrap();
    let after_restart = reopened
        .task_delta(
            &project,
            task_id,
            &session_b,
            "eval-b",
            packet.header.event_cursor,
            20,
        )
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
    assert_eq!(
        reopened
            .explain_context(&packet.header.packet_hash, &project, &session_b, "eval-b",)
            .unwrap()
            .event_cursor,
        packet.header.event_cursor
    );
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
    install_memory_task(&store, task_id, &["agent-a", "agent-b"]);
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
        install_memory_task(&store, task_id, &["agent-a", "agent-b"]);
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
            object.hash(),
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

#[test]
fn rebuild_advances_context_revisions_even_when_a_scope_has_no_surviving_projection() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    store
        .connection
        .execute(
            "INSERT INTO project_context_revisions (project_id, revision)
             VALUES ('removed-project-scope', 7)",
            [],
        )
        .expect("install prior project revision");
    store
        .connection
        .execute(
            "INSERT INTO agent_context_revisions (project_id, agent_id, revision)
             VALUES ('removed-agent-scope', 'agent-a', 11)",
            [],
        )
        .expect("install prior private revision");

    assert_eq!(
        store.rebuild_memory_index().expect("rebuild empty index"),
        0
    );
    let revisions = store
        .connection
        .query_row(
            "SELECT
                 (SELECT revision FROM project_context_revisions
                  WHERE project_id = 'removed-project-scope'),
                 (SELECT revision FROM agent_context_revisions
                  WHERE project_id = 'removed-agent-scope' AND agent_id = 'agent-a')",
            [],
            |row| Ok((row.get::<_, i64>(0)?, row.get::<_, i64>(1)?)),
        )
        .expect("read rebuilt revision fences");
    assert_eq!(revisions, (8, 12));
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one scenario verifies declaration, replay, fail-closed delivery, status, and rebuild"
)]
fn applicable_pinned_contradictions_fail_closed_and_rebuild() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let project = ProjectId("project-a".into());
    let session_a = SessionId("agent-a".into());
    let session_b = SessionId("agent-b".into());
    let now = Utc::now();
    let task = store
        .start_task(
            &project,
            "dummy:CONFLICT-1",
            "Exercise contradiction safety",
            &session_a,
            actor("agent-a"),
            now,
        )
        .unwrap();
    store
        .join_task(
            &project,
            "dummy:CONFLICT-1",
            &session_b,
            actor("agent-b"),
            now,
        )
        .unwrap();
    let first = store
        .capture_note(
            &note_request(
                task.task.task_id,
                "agent-a",
                "Never publish before every participant is ready.",
                "constraint-a",
                NoteVisibility::Shared,
            ),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let second = store
        .capture_note(
            &note_request(
                task.task.task_id,
                "agent-a",
                "Always publish immediately when the implementation passes.",
                "constraint-b",
                NoteVisibility::Shared,
            ),
            &DevelopmentNoopRedactor,
        )
        .unwrap();

    let edge = store
        .record_memory_contradiction(
            &project,
            Some(task.task.task_id),
            None,
            &session_a,
            "agent-a",
            &first.version,
            &second.version,
            "the publication timing rules cannot both be followed",
            "contradiction-a",
            actor("agent-a"),
            now,
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let replay = store
        .record_memory_contradiction(
            &project,
            Some(task.task.task_id),
            None,
            &session_a,
            "agent-a",
            &second.version,
            &first.version,
            "the publication timing rules cannot both be followed",
            "contradiction-a",
            actor("agent-a"),
            now + TimeDelta::seconds(1),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    assert_eq!(replay.contradiction, edge.contradiction);
    assert!(replay.duplicate);

    let assert_fails_closed = |store: &mut SqliteStore| {
        let result = store.build_context(
            &project,
            Some(task.task.task_id),
            &session_b,
            "agent-b",
            now,
        );
        match result {
            Err(StoreError::PinnedContradiction {
                contradiction,
                left,
                right,
            }) => {
                assert_eq!(contradiction, edge.contradiction);
                let actual = [left, right];
                assert!(actual.contains(&first.version));
                assert!(actual.contains(&second.version));
            }
            other => panic!("expected pinned contradiction, got {other:?}"),
        }
    };
    assert_fails_closed(&mut store);
    let visible = store
        .search_memories(
            &project,
            Some(task.task.task_id),
            None,
            &session_b,
            "agent-b",
            None,
            20,
        )
        .unwrap();
    assert!(
        visible
            .iter()
            .all(|memory| memory.status == MemoryStatus::Contested)
    );

    assert_eq!(store.rebuild_memory_index().unwrap(), 2);
    assert_fails_closed(&mut store);
}

#[test]
fn soft_contradictions_are_delivered_and_flagged() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let project = ProjectId("project-a".into());
    let session = SessionId("agent-a".into());
    let now = Utc::now();
    let task = store
        .start_task(
            &project,
            "dummy:CONFLICT-2",
            "Surface soft conflicts",
            &session,
            actor("agent-a"),
            now,
        )
        .unwrap();
    let first = store
        .capture_note(
            &note_request(
                task.task.task_id,
                "agent-a",
                "Fact: the integration uses polling.",
                "soft-a",
                NoteVisibility::Shared,
            ),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let second = store
        .capture_note(
            &note_request(
                task.task.task_id,
                "agent-a",
                "Fact: the integration uses notifications only.",
                "soft-b",
                NoteVisibility::Shared,
            ),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    store
        .record_memory_contradiction(
            &project,
            Some(task.task.task_id),
            None,
            &session,
            "agent-a",
            &first.version,
            &second.version,
            "the transport descriptions disagree",
            "soft-conflict",
            actor("agent-a"),
            now,
            &DevelopmentNoopRedactor,
        )
        .unwrap();

    let packet = store
        .build_context(&project, Some(task.task.task_id), &session, "agent-a", now)
        .unwrap();
    assert_eq!(packet.index.len(), 2);
    assert!(packet.index.iter().all(|item| {
        item.status == MemoryStatus::Contested
            && item.retrieval_reason.contains("unresolved contradiction")
    }));
}
