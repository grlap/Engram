use super::super::test_support::*;
use super::super::*;
use tempfile::tempdir;

#[test]
fn process_default_work_session_reuse_expires_before_protocol_mutation() {
    let retained = process_default_session_at(10, at(0));
    validate_process_default_work_session(
        &retained,
        true,
        at(PROCESS_DEFAULT_WORK_SESSION_RETENTION_SECONDS - 1),
    )
    .expect("session remains reusable within the window");
    assert!(matches!(
        validate_process_default_work_session(
            &retained,
            false,
            at(PROCESS_DEFAULT_WORK_SESSION_RETENTION_SECONDS)
        ),
        Err(StoreError::InvalidWork(detail))
            if detail == "process-default work session cannot be reused; run without --session-id to receive a fresh process default"
    ));
    assert!(matches!(
        validate_process_default_work_session(
            &SessionId(format!("local-process-10-{}", uuid::Uuid::new_v4())),
            false,
            at(0)
        ),
        Err(StoreError::InvalidWork(detail))
            if detail == "process-default work session cannot be reused; run without --session-id to receive a fresh process default"
    ));
    assert!(matches!(
        validate_process_default_work_session(&SessionId("ordinary-session".into()), true, at(0)),
        Err(StoreError::InvalidWork(detail))
            if detail == "process-default work session cannot be reused; run without --session-id to receive a fresh process default"
    ));
    assert!(matches!(
        validate_process_default_work_session(
            &process_default_session_at(12, at(1)),
            false,
            at(0)
        ),
        Err(StoreError::InvalidWork(detail))
            if detail == "process-default work session cannot be reused; run without --session-id to receive a fresh process default"
    ));

    let directory = tempdir().expect("temporary directory");
    let database = directory.path().join("engram.sqlite3");
    let retained_session = process_default_session_at(11, at(0));
    let service = LocalWorkService::new_with_attribution(
        database.clone(),
        ProjectId("expired-process-session-project".into()),
        "agent".into(),
        retained_session.clone(),
        None,
        None,
        WorkAttributionDefaults {
            actor: None,
            session: true,
        },
    );
    service
        .work_next(
            1,
            WorkNextQuery {
                sections: vec![WorkNextSection::Ready],
                ..WorkNextQuery::default()
            },
            at(PROCESS_DEFAULT_WORK_SESSION_RETENTION_SECONDS - 1),
        )
        .expect("retained service accepts the session inside the window");
    let before_expiry = SqliteStore::open(&database)
        .expect("inspect retained session")
        .work_session_state(
            &ProjectId("expired-process-session-project".into()),
            &retained_session,
            at(PROCESS_DEFAULT_WORK_SESSION_RETENTION_SECONDS - 1),
        )
        .expect("retained state before expiry");
    assert!(matches!(
        service.work_next(
            1,
            WorkNextQuery::default(),
            at(PROCESS_DEFAULT_WORK_SESSION_RETENTION_SECONDS)
        ),
        Err(StoreError::InvalidWork(detail))
            if detail == "process-default work session cannot be reused; run without --session-id to receive a fresh process default"
    ));
    let refused_store = SqliteStore::open(&database).expect("inspect refused store");
    assert_eq!(
        refused_store
            .work_session_state(
                &ProjectId("expired-process-session-project".into()),
                &retained_session,
                at(PROCESS_DEFAULT_WORK_SESSION_RETENTION_SECONDS),
            )
            .expect("retained state after refusal"),
        before_expiry
    );
    assert!(
        refused_store
            .verify_all()
            .expect("refusal left a healthy store")
            .is_healthy()
    );
}

#[test]
fn graph_save_retains_process_default_attribution_without_registering_ambient_session_state() {
    let directory = tempdir().expect("temporary directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("snapshot-attribution-only-project".into());
    let session = process_default_session_at(17, at(0));
    let service = LocalWorkService::new_with_attribution(
        database.clone(),
        project.clone(),
        "snapshot-operator".into(),
        session.clone(),
        None,
        None,
        WorkAttributionDefaults {
            actor: None,
            session: true,
        },
    );

    service
        .save_work_graph_snapshot(None, WorkGraphSnapshotDestinationKind::Stdout, at(1))
        .expect("save attributed snapshot");
    let connection = rusqlite::Connection::open(&database).expect("inspect snapshot store");
    let registered = connection
        .query_row(
            "SELECT COUNT(*) FROM work_session_state
             WHERE project_id = ?1 AND session_id = ?2",
            rusqlite::params![project.0, session.0],
            |row| row.get::<_, i64>(0),
        )
        .expect("count ambient session rows");
    assert_eq!(registered, 0, "graph save attribution is audit-only");
    drop(connection);

    service
        .work_next(1, WorkNextQuery::default(), at(2))
        .expect("later ambient word registers the same process session");
    let registered = rusqlite::Connection::open(&database)
        .expect("inspect ambient store")
        .query_row(
            "SELECT COUNT(*) FROM work_session_state
             WHERE project_id = ?1 AND session_id = ?2",
            rusqlite::params![project.0, session.0],
            |row| row.get::<_, i64>(0),
        )
        .expect("count registered session rows");
    assert_eq!(registered, 1);
}

#[test]
fn core_operation_keys_separate_protocol_variants_and_suboperations() {
    let service = LocalWorkService::new(
        PathBuf::from("unused.sqlite3"),
        ProjectId("key-project".into()),
        "agent".into(),
        SessionId("key-session".into()),
        None,
    );
    let cancel = service
        .core_operation_key("work_update:cancel", "same-key", "dispose_work")
        .expect("cancel key");
    let supersede = service
        .core_operation_key("work_update:supersede", "same-key", "dispose_work")
        .expect("supersede key");
    let capture = service
        .core_operation_key("work_complete", "same-key", "record_work_evidence")
        .expect("capture key");
    let checkpoint = service
        .core_operation_key("work_complete", "same-key", "checkpoint_work")
        .expect("checkpoint key");
    let complete = service
        .core_operation_key("work_complete", "same-key", "complete_work")
        .expect("completion key");

    assert_ne!(cancel, supersede);
    assert_ne!(capture, checkpoint);
    assert_ne!(checkpoint, complete);
    assert_ne!(capture, complete);
}

#[test]
fn local_work_service_rejects_blank_asserted_identity() {
    let directory = tempdir().expect("temporary directory");
    for (actor_id, session_id) in [("   ", "session"), ("agent", "\t")] {
        let service = LocalWorkService::new(
            directory
                .path()
                .join(format!("{}.sqlite3", session_id.len())),
            ProjectId("blank-identity-project".into()),
            actor_id.into(),
            SessionId(session_id.into()),
            None,
        );
        assert!(matches!(
            service.work_next(1, WorkNextQuery::default(), at(0)),
            Err(StoreError::InvalidWork(detail))
                if detail.contains("non-empty asserted actor and session")
        ));
    }
}

#[test]
fn local_work_service_normalizes_actor_context_without_refusing_words() {
    let directory = tempdir().expect("temporary directory");
    for (index, (actor_context, expected)) in [
        (
            format!("  model=codex\n{}  ", "🙂".repeat(100)),
            format!("model=codex {}", "🙂".repeat(61)),
        ),
        ("\n\t".into(), String::new()),
    ]
    .into_iter()
    .enumerate()
    {
        let service = LocalWorkService::new_with_attribution(
            directory
                .path()
                .join(format!("actor-context-{index}.sqlite3")),
            ProjectId("actor-context-bound-project".into()),
            "agent".into(),
            SessionId("session".into()),
            None,
            Some(actor_context),
            WorkAttributionDefaults::default(),
        );
        service
            .work_next(1, WorkNextQuery::default(), at(0))
            .expect("normalized context must not refuse a word");
        let actor = service.actor("work_next", "test normalized actor context");
        if expected.is_empty() {
            assert!(actor.attribution_context().is_none());
            assert!(!actor.provenance_chain.iter().any(|link| {
                link.reference.as_deref() == Some(ACTOR_CONTEXT_PROVENANCE_REFERENCE)
            }));
        } else {
            assert_eq!(actor.attribution_context(), Some(expected.as_str()));
        }
        assert!(actor.provenance_chain.contains(&ProvenanceLink {
            relation: ProvenanceRelation::DerivedFrom,
            source: "actor_context:normalized".into(),
            reference: Some(ACTOR_CONTEXT_NORMALIZED_REFERENCE.into()),
        }));
        assert!(!format!("{service:?}").contains("model=codex"));
    }
}

#[test]
fn terminal_actor_labels_escape_asserted_identity_and_context() {
    let safe = terminal_safe_actor_label("agent\nname", Some("model=codex\u{202e}"));
    assert!(!safe.chars().any(is_unsafe_rendered_text_char));
    assert!(safe.contains("agent\\nname"));
    assert!(safe.contains("model=codex\\u{202e}"));
}

#[test]
fn shell_attribution_defaults_are_explicit_in_actor_provenance() {
    let directory = tempdir().expect("temporary directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("defaulted-attribution-project".into());
    let process_session = process_default_session_at(11, at(0));
    let service = LocalWorkService::new_with_attribution(
        database.clone(),
        project,
        "os-user".into(),
        process_session.clone(),
        None,
        None,
        WorkAttributionDefaults {
            actor: Some(WorkActorDefaultSource::OsUserEnvironment),
            session: true,
        },
    );
    let work = proposed_root(
        service
            .work_propose(
                root_input("Defaulted attribution", "defaulted-attribution"),
                at(0),
            )
            .expect("persist defaulted attribution"),
    );
    let store = SqliteStore::open(&database).expect("defaulted attribution store");
    let entry = store
        .work_event_tail(work.work_id, 1)
        .expect("defaulted attribution event")
        .pop()
        .expect("created event");
    let event = store
        .get::<WorkEvent>(&entry.object_hash)
        .expect("read defaulted attribution event")
        .expect("canonical defaulted attribution event");
    let actor = event.actor;
    assert_eq!(actor.actor_id, "os-user");
    assert_eq!(actor.session_id, Some(process_session));
    assert!(actor.provenance_chain.contains(&ProvenanceLink {
        relation: ProvenanceRelation::DerivedFrom,
        source: "defaulted:os_user_environment".into(),
        reference: Some("actor_id".into()),
    }));
    assert!(actor.provenance_chain.contains(&ProvenanceLink {
        relation: ProvenanceRelation::DerivedFrom,
        source: "defaulted:process_session".into(),
        reference: Some("session_id".into()),
    }));

    let fallback = LocalWorkService::new_with_attribution(
        PathBuf::from("unused.sqlite3"),
        ProjectId("fallback-attribution-project".into()),
        "local-user-1".into(),
        SessionId("process-session".into()),
        None,
        None,
        WorkAttributionDefaults {
            actor: Some(WorkActorDefaultSource::ProcessFallback),
            session: false,
        },
    )
    .actor("work_next", "test fallback attribution");
    assert!(fallback.provenance_chain.contains(&ProvenanceLink {
        relation: ProvenanceRelation::DerivedFrom,
        source: "defaulted:process_actor".into(),
        reference: Some("actor_id".into()),
    }));

    let injected = LocalWorkService::new(
        PathBuf::from("unused.sqlite3"),
        ProjectId("injected-attribution-project".into()),
        " injected actor ".into(),
        SessionId(" injected session ".into()),
        None,
    )
    .actor("work_next", "test injected attribution");
    assert_eq!(injected.actor_id, " injected actor ");
    assert_eq!(
        injected.session_id,
        Some(SessionId(" injected session ".into()))
    );
    assert_eq!(injected.provenance_chain.len(), 1);

    let contextual = LocalWorkService::new_with_attribution(
        PathBuf::from("unused.sqlite3"),
        ProjectId("contextual-attribution-project".into()),
        "greg/codex".into(),
        SessionId("contextual-session".into()),
        None,
        Some("model=opus-4.1;reasoning=high".into()),
        WorkAttributionDefaults::default(),
    )
    .actor("work_next", "test contextual attribution");
    assert_eq!(
        contextual.attribution_context(),
        Some("model=opus-4.1;reasoning=high")
    );
    assert_eq!(contextual.actor_id, "greg/codex");
    assert!(
        !contextual
            .provenance_chain
            .iter()
            .any(|link| { link.reference.as_deref() == Some(ACTOR_CONTEXT_NORMALIZED_REFERENCE) })
    );
}

#[test]
fn actor_context_does_not_change_work_protocol_identity() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("context-independent-intent".into());
    let service = |context: &str| {
        LocalWorkService::new_with_attribution(
            database.clone(),
            project.clone(),
            "agent".into(),
            SessionId("stable-session".into()),
            Some("protocol-test".into()),
            Some(context.into()),
            WorkAttributionDefaults::default(),
        )
    };
    let first = service("model=first")
        .work_propose(root_input("Context-independent retry", "stable-key"), at(0))
        .expect("first operation");
    let replay = service("model=second")
        .work_propose(root_input("Context-independent retry", "stable-key"), at(1))
        .expect("context change must replay instead of conflicting");
    let WorkProposeResult::Root {
        work: first_work, ..
    } = first
    else {
        panic!("expected root");
    };
    let WorkProposeResult::Root {
        work: replayed_work,
        ..
    } = replay
    else {
        panic!("expected replayed root");
    };
    assert_eq!(replayed_work.work_id, first_work.work_id);
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one end-to-end scenario demonstrates that no lifecycle identifiers are shuttled between protocol calls"
)]
fn ambient_protocol_runs_root_claim_evidence_handoff_and_completion() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("protocol-project".into());
    let a = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("session-a".into()),
        Some("protocol-test".into()),
    );
    let b = LocalWorkService::new(
        database.clone(),
        project,
        "agent".into(),
        SessionId("session-b".into()),
        Some("protocol-test".into()),
    );

    let root = match a
        .work_propose(
            WorkProposeInput::Root {
                notes: Vec::new(),
                title: "Ship ambient work".into(),
                outcome: "The six-operation protocol works end to end".into(),
                acceptance: vec!["handoff completion is sealed".into()],
                work_kind: Some(WorkItemKind::Feature),
                priority: Some(1),
                labels: vec!["protocol".into()],
                assigned_to: None,
                deferred_until: None,
                idempotency_key: "root".into(),
            },
            at(0),
        )
        .expect("root proposal")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    let first = a
        .work_next(20, WorkNextQuery::default(), at(1))
        .expect("first ambient page");
    assert_eq!(first.session.focused_work_id, Some(root.work_id));
    let first_delivered = first.delivered_through.expect("first delivered cursor");
    let first_delivery_token = first.delivery_token.clone().expect("first delivery token");
    assert!(first_delivered > 0);
    assert_eq!(first.session.confirmed_project_cursor, 0);
    assert!(first.session.pending_delivery);
    let concurrent = match b
        .work_propose(
            WorkProposeInput::Root {
                notes: Vec::new(),
                title: "Concurrent project event".into(),
                outcome: "Appending after delivery does not change the staged page".into(),
                acceptance: vec!["event is durable".into()],
                work_kind: None,
                priority: None,
                labels: Vec::new(),
                assigned_to: None,
                deferred_until: None,
                idempotency_key: "concurrent-root".into(),
            },
            at(2),
        )
        .expect("append after another session staged a page")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected concurrent root"),
    };
    // A staged page never blocks a focus change.
    let switched = a
        .work_focus(&concurrent.short_ref, at(3))
        .expect("focus changes while a page is staged");
    assert_eq!(switched.status.work.work_id, concurrent.work_id);
    a.work_focus(&root.short_ref, at(3))
        .expect("focus returns to the root");
    // The focus change discarded the un-delivered page, so the next call
    // recomputes the interval from the confirmed cursor through the
    // concurrent append.
    let second = a
        .work_next(20, WorkNextQuery::default(), at(4))
        .expect("second page after the focus change");
    assert_eq!(second.session.confirmed_project_cursor, 0);
    let second_delivered = second.delivered_through.expect("second delivered cursor");
    assert!(second_delivered > first_delivered);
    assert!(second.session.pending_delivery);
    let second_positions = second
        .changes
        .as_ref()
        .expect("second changes")
        .iter()
        .map(|change| change.entry.position.position)
        .collect::<Vec<_>>();
    assert_eq!(second_positions, (1..=second_delivered).collect::<Vec<_>>());
    assert_ne!(second.delivery_token, Some(first_delivery_token.clone()));
    // A host may still acknowledge explicitly, but only the exact current
    // pair; the discarded page's pair is refused without disclosure.
    let stale = a
        .work_next_with_delivery_token(
            20,
            Some(first_delivered),
            Some(first_delivery_token.as_str()),
            WorkNextQuery::default(),
            at(5),
        )
        .expect_err("a discarded page cannot be acknowledged");
    assert!(!stale.to_string().contains(&first_delivery_token));
    let explicit = a
        .work_next_with_delivery_token(
            20,
            Some(second_delivered),
            second.delivery_token.as_deref(),
            WorkNextQuery::default(),
            at(5),
        )
        .expect("explicit acknowledgement of the current page");
    assert_eq!(explicit.session.confirmed_project_cursor, second_delivered);
    assert!(
        explicit
            .changes
            .as_ref()
            .expect("no new changes")
            .is_empty()
    );
    assert!(
        first
            .focus
            .expect("focus")
            .allowed_next
            .contains(&"work_update:claim".into())
    );

    let claim_input = WorkUpdateInput::Claim {
        ttl_seconds: Some(300),
        recovery_reason: None,
        idempotency_key: "claim-a".into(),
    };
    let claimed = a.work_update(claim_input.clone(), at(4)).expect("claim");
    let control_binding = claimed
        .receipt
        .control_binding
        .as_ref()
        .expect("claim receipt exposes a paste-ready control binding");
    assert_eq!(control_binding.work_id, root.work_id);
    assert_eq!(control_binding.work_revision, claimed.receipt.revision);
    let claimed_focus = a
        .work_focus(&root.short_ref, at(4))
        .expect("claimed focus exposes the same control binding");
    assert_eq!(
        claimed_focus.control_binding.as_ref(),
        Some(control_binding)
    );
    assert_eq!(
        claimed_focus
            .run
            .as_ref()
            .expect("claimed run")
            .root_execution_id,
        control_binding.root_execution_id
    );
    assert_eq!(
        claimed_focus
            .claim
            .as_ref()
            .expect("claimed focus claim")
            .claim_id,
        control_binding.claim_id
    );
    a.work_focus(&concurrent.short_ref, at(7))
        .expect("focus changes freely between calls");
    let claim_replay = a
        .work_update(claim_input, at(40))
        .expect("lost-response claim replay");
    assert_eq!(
        serde_json::to_value(&claim_replay).expect("serialize replay"),
        serde_json::to_value(&claimed).expect("serialize original")
    );
    a.work_focus(&root.short_ref, at(41))
        .expect("restore original work focus");
    let attempt_connection = rusqlite::Connection::open(&database).expect("attempt store");
    let (basis_json, result_hash, result_json) = attempt_connection
        .query_row(
            "SELECT basis_json, result_hash, result_json FROM work_protocol_attempts
                 WHERE project_id = ?1 AND session_id = ?2
                   AND operation = 'work_update:claim' AND idempotency_key = 'claim-a'",
            rusqlite::params!["protocol-project", "session-a"],
            |row| {
                Ok((
                    row.get::<_, Option<Vec<u8>>>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, Vec<u8>>(2)?,
                ))
            },
        )
        .expect("compacted attempt");
    assert!(basis_json.is_none());
    assert!(result_hash.is_some());
    let exact_result: serde_json::Value =
        serde_json::from_slice(&result_json).expect("exact replay JSON");
    assert_eq!(
        exact_result,
        serde_json::to_value(&claimed).expect("serialize exact replay basis")
    );
    drop(attempt_connection);
    let conflict = a.work_update(
        WorkUpdateInput::Claim {
            ttl_seconds: Some(301),
            recovery_reason: None,
            idempotency_key: "claim-a".into(),
        },
        at(41),
    );
    assert!(matches!(
        conflict,
        Err(StoreError::WorkOperationIdempotencyConflict { .. })
    ));
    let evidence = a
        .work_update(
            WorkUpdateInput::Evidence {
                summary: "protocol lifecycle test passed".into(),
                refs: vec!["test:ambient-protocol".into()],
                attach: None,
                idempotency_key: "evidence-a".into(),
            },
            at(42),
        )
        .expect("evidence")
        .receipt
        .result
        .as_str()
        .expect("evidence hash")
        .to_owned();
    a.work_handoff(
        WorkHandoffInput::Offer {
            to: "session-b".into(),
            ttl_seconds: Some(200),
            checkpoint_summary: "handoff after evidence capture".into(),
            idempotency_key: "offer-b".into(),
        },
        at(43),
    )
    .expect("offer");

    let focused = b
        .work_focus(&root.short_ref, at(44))
        .expect("recipient focus");
    assert!(focused.allowed_next.contains(&"work_handoff:accept".into()));
    b.work_handoff(
        WorkHandoffInput::Accept {
            idempotency_key: "accept-b".into(),
        },
        at(45),
    )
    .expect("accept");
    b.work_update(
        WorkUpdateInput::Checkpoint {
            summary: "recipient validated evidence and acceptance".into(),
            evidence: Some(vec![evidence]),
            idempotency_key: "checkpoint-b".into(),
        },
        at(46),
    )
    .expect("recipient checkpoint");
    let seal = b
        .work_complete(
            WorkCompleteInput {
                capture: None,
                evidence: Vec::new(),
                acceptance: Some(vec![WorkAcceptanceInput {
                    criterion: None,
                    satisfied: true,
                    evidence: Vec::new(),
                    note: "verified by the receiving session".into(),
                }]),
                note: None,
                idempotency_key: "complete-b".into(),
            },
            at(47),
        )
        .expect("complete after handoff");
    let WorkCompleteResult::Completed(seal) = seal else {
        panic!("handoff completion must seal work");
    };
    assert_eq!(seal.work_id, root.work_id);
    let stored_seal = SqliteStore::open(&database)
        .expect("store")
        .get::<CompletionSeal>(&seal.seal)
        .expect("read seal")
        .expect("canonical completion seal");
    assert_eq!(stored_seal.expected_contributors.len(), 2);
    assert_eq!(
        stored_seal
            .contributions
            .iter()
            .map(|contribution| &contribution.participant)
            .collect::<std::collections::HashSet<_>>()
            .len(),
        2
    );
    let completed = b
        .work_focus(&root.short_ref, at(48))
        .expect("completed focus");
    assert_eq!(completed.status.work.lifecycle, WorkLifecycle::Completed);

    let tamper = rusqlite::Connection::open(&database).expect("tamper store");
    tamper
            .execute(
                "UPDATE work_protocol_attempts
                 SET result_json = CAST('{\"operation\":\"claim\",\"receipt\":{\"work_id\":\"00000000-0000-0000-0000-000000000000\"},\"obligations\":[],\"allowed_next\":[]}' AS BLOB)
                 WHERE project_id = 'protocol-project' AND session_id = 'session-a'
                   AND operation = 'work_update:claim' AND idempotency_key = 'claim-a'",
                [],
            )
            .expect("tamper compact replay bytes");
    let (projection_bytes, canonical_bytes) = tamper
        .query_row(
            "SELECT attempt.result_json, object.canonical_json
                 FROM work_protocol_attempts attempt
                 JOIN objects object ON object.object_hash = attempt.result_hash
                 WHERE attempt.project_id = 'protocol-project'
                   AND attempt.session_id = 'session-a'
                   AND attempt.operation = 'work_update:claim'
                   AND attempt.idempotency_key = 'claim-a'",
            [],
            |row| Ok((row.get::<_, Vec<u8>>(0)?, row.get::<_, Vec<u8>>(1)?)),
        )
        .expect("tampered projection and canonical result");
    assert_ne!(projection_bytes, canonical_bytes);
    let canonical_replay: serde_json::Value =
        serde_json::from_slice(&canonical_bytes).expect("canonical replay JSON");
    assert_ne!(
        canonical_replay
            .pointer("/receipt/work_id")
            .and_then(serde_json::Value::as_str),
        Some("00000000-0000-0000-0000-000000000000")
    );
    drop(tamper);
    let tampered_replay = a.work_update(
        WorkUpdateInput::Claim {
            ttl_seconds: Some(300),
            recovery_reason: None,
            idempotency_key: "claim-a".into(),
        },
        at(49),
    );
    assert!(
        matches!(tampered_replay, Err(StoreError::InvalidWorkProjection(_))),
        "unexpected tampered replay result: {tampered_replay:?}"
    );
    assert!(
        !SqliteStore::open(&database)
            .expect("doctor store")
            .verify_all()
            .expect("doctor")
            .invalid_work_records
            .is_empty()
    );
    let repair = rusqlite::Connection::open(&database).expect("repair store");
    repair
        .execute(
            "UPDATE work_protocol_attempts SET result_json = ?1
                 WHERE project_id = 'protocol-project' AND session_id = 'session-a'
                   AND operation = 'work_update:claim' AND idempotency_key = 'claim-a'",
            [canonical_bytes],
        )
        .expect("restore exact replay projection");
    drop(repair);
    assert!(
        SqliteStore::open(&database)
            .expect("repaired doctor store")
            .verify_all()
            .expect("repaired doctor")
            .is_healthy()
    );
    assert!(
        completed
            .allowed_next
            .contains(&"work_update:reopen".into())
    );
    b.work_update(
        WorkUpdateInput::Reopen {
            reason: "verify honest non-success disposition".into(),
            idempotency_key: "reopen-for-cancel".into(),
        },
        at(49),
    )
    .expect("reopen before cancellation");
    let cancelled = b
        .work_update(
            WorkUpdateInput::Cancel {
                reason: "the reopened experiment is no longer needed".into(),
                idempotency_key: "cancel-root".into(),
            },
            at(50),
        )
        .expect("cancel without false completion");
    assert_eq!(cancelled.receipt.work_id, root.work_id);
    assert_eq!(
        cancelled.receipt.result.get("lifecycle"),
        Some(&serde_json::json!("cancelled"))
    );

    let replacement = match b
        .work_propose(
            WorkProposeInput::Root {
                notes: Vec::new(),
                title: "Replacement approach".into(),
                outcome: "A better local execution plan is tracked".into(),
                acceptance: vec!["replacement is evaluated".into()],
                work_kind: Some(WorkItemKind::Research),
                priority: Some(1),
                labels: Vec::new(),
                assigned_to: None,
                deferred_until: None,
                idempotency_key: "replacement-root".into(),
            },
            at(51),
        )
        .expect("replacement root")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected replacement root"),
    };
    let obsolete = match b
        .work_propose(
            WorkProposeInput::Root {
                notes: Vec::new(),
                title: "Obsolete approach".into(),
                outcome: "This plan is explicitly superseded".into(),
                acceptance: vec!["obsolete plan is not completed".into()],
                work_kind: Some(WorkItemKind::Research),
                priority: Some(2),
                labels: Vec::new(),
                assigned_to: None,
                deferred_until: None,
                idempotency_key: "obsolete-root".into(),
            },
            at(52),
        )
        .expect("obsolete root")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected obsolete root"),
    };
    let superseded = b
        .work_update(
            WorkUpdateInput::Supersede {
                replacement: replacement.short_ref,
                reason: "replacement captures the revised plan".into(),
                idempotency_key: "supersede-obsolete".into(),
            },
            at(53),
        )
        .expect("supersede without false completion");
    assert_eq!(superseded.receipt.work_id, obsolete.work_id);
    assert_eq!(
        superseded.receipt.result.get("lifecycle"),
        Some(&serde_json::json!("superseded"))
    );
    assert_eq!(
        superseded.receipt.result.get("superseded_by"),
        Some(&serde_json::json!(replacement.work_id))
    );
    let catalog = b
        .work_next(
            10,
            WorkNextQuery {
                search: Some("obsolete".into()),
                lifecycles: vec![WorkLifecycle::Superseded],
                ..WorkNextQuery::default()
            },
            at(54),
        )
        .expect("search superseded work");
    let catalog_items = &catalog.catalog.as_ref().expect("catalog").items;
    assert_eq!(catalog_items.len(), 1);
    assert_eq!(catalog_items[0].work.work_id, obsolete.work_id);
    assert_eq!(
        catalog
            .focus
            .expect("query preserves ambient focus")
            .status
            .work
            .work_id,
        obsolete.work_id
    );
    let cancelled_catalog = b
        .work_next(
            10,
            WorkNextQuery {
                lifecycles: vec![WorkLifecycle::Cancelled],
                ..WorkNextQuery::default()
            },
            at(55),
        )
        .expect("list cancelled work");
    assert!(
        cancelled_catalog
            .catalog
            .as_ref()
            .expect("cancelled catalog")
            .items
            .iter()
            .any(|item| item.work.work_id == root.work_id)
    );
}
