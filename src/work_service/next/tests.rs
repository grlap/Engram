use super::super::test_support::*;
use super::super::*;
use crate::domain::SCHEMA_VERSION;
use tempfile::tempdir;

#[test]
fn advisory_memory_acknowledgement_swallows_every_failure_class() {
    for error in [
        StoreError::Sqlite(rusqlite::Error::SqliteFailure(
            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_BUSY),
            None,
        )),
        StoreError::InvalidProjectMemory("non-contention refusal".into()),
    ] {
        let mut attempted = false;
        ignore_project_memory_advertisement_acknowledgement(|| {
            attempted = true;
            Err(error)
        });
        assert!(attempted);
    }
}

#[test]
fn interrupted_attempt_cannot_follow_changed_focus() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let focus_project = ProjectId("focus-attempt".into());
    let focus_service = LocalWorkService::new(
        database.clone(),
        focus_project.clone(),
        "agent".into(),
        SessionId("focus-session".into()),
        Some("protocol-test".into()),
    );
    let first = match focus_service
        .work_propose(root_input("First target", "first-root"), at(3))
        .expect("first root")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    let revise = WorkUpdateInput::Revise {
        patch: WorkRevisionPatch {
            title: Some("Must stay on first target".into()),
            ..WorkRevisionPatch::default()
        },
        idempotency_key: "interrupted-revise".into(),
    };
    let mut store = SqliteStore::open(&database).expect("store");
    let basis = focus_service
        .protocol_basis(&store, true, false, None, at(4))
        .expect("bound first focus");
    assert_eq!(
        basis.focused_work.as_ref().map(|work| work.work_id),
        Some(first.work_id)
    );
    let intent = focus_service.protocol_intent(&revise);
    store
        .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
            project_id: &focus_project,
            session_id: &SessionId("focus-session".into()),
            operation: "work_update:revise",
            idempotency_key: "interrupted-revise",
            intent: &intent,
            basis: &basis,
            now: at(4),
        })
        .expect("persist focus-bound attempt");
    drop(store);
    let second = match focus_service
        .work_propose(root_input("Second target", "second-root"), at(5))
        .expect("second root")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    assert!(matches!(
        focus_service.work_update(revise, at(6)),
        Err(StoreError::WorkOperationIdempotencyConflict { .. })
    ));
    let unchanged = SqliteStore::open(&database)
        .expect("store")
        .get_work_item(second.work_id)
        .expect("second target");
    assert_eq!(unchanged.title, "Second target");
    assert_eq!(unchanged.revision, second.revision);
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one scenario shows discard-on-focus-change, implicit delivery, and dense continuation in order"
)]
fn staged_page_never_blocks_focus_and_is_delivered_by_the_next_call() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("implicit-delivery".into());
    let session = SessionId("implicit-delivery-session".into());
    let service = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        session.clone(),
        Some("protocol-test".into()),
    );
    let peer = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("implicit-delivery-peer".into()),
        Some("protocol-test".into()),
    );
    service
        .work_propose(root_input("First root", "implicit-first"), at(0))
        .expect("first root");
    let target = match peer
        .work_propose(root_input("Second root", "implicit-second"), at(1))
        .expect("second root")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    let changes_only = || WorkNextQuery {
        sections: vec![WorkNextSection::Changes],
        ..WorkNextQuery::default()
    };
    let first = service
        .work_next(20, changes_only(), at(2))
        .expect("first page");
    let first_head = first.delivered_through.expect("first boundary");
    assert!(first_head > 0);
    assert_eq!(first.session.confirmed_project_cursor, 0);

    // Focus changes while the page is still staged; the page projected
    // under the old focus is discarded and nothing is confirmed.
    let focused = service
        .work_focus(&target.short_ref, at(3))
        .expect("focus while a page is pending");
    assert_eq!(focused.status.work.work_id, target.work_id);
    let discarded = SqliteStore::open(&database)
        .expect("store")
        .work_session_state(&project, &session, at(3))
        .expect("session state");
    assert_eq!(discarded.tentative_project_cursor, None);
    assert_eq!(discarded.project_cursor, 0);
    assert_eq!(discarded.focused_work_id, Some(target.work_id));

    peer.work_propose(root_input("Third root", "implicit-third"), at(4))
        .expect("append after the first page was staged");
    // The next call recomputes the interval under the new focus from the
    // confirmed cursor, densely through the new head.
    let second = service
        .work_next(20, changes_only(), at(5))
        .expect("second page");
    assert_eq!(second.session.confirmed_project_cursor, 0);
    let second_head = second.delivered_through.expect("second boundary");
    assert!(second_head > first_head);
    let positions = second
        .changes
        .as_ref()
        .expect("second changes")
        .iter()
        .map(|change| change.entry.position.position)
        .collect::<Vec<_>>();
    assert_eq!(positions, (1..=second_head).collect::<Vec<_>>());
    // Sections without changes neither deliver nor stage.
    let focus_only = service
        .work_next(
            20,
            WorkNextQuery {
                sections: vec![WorkNextSection::Focus],
                ..WorkNextQuery::default()
            },
            at(6),
        )
        .expect("focus-only view");
    assert_eq!(focus_only.session.confirmed_project_cursor, 0);
    assert_eq!(focus_only.delivered_through, None);
    // Without a focus change, the next call delivers the previous page
    // implicitly and continues densely from its boundary.
    peer.work_propose(root_input("Fourth root", "implicit-fourth"), at(6))
        .expect("append after the second page was staged");
    let third = service
        .work_next(20, changes_only(), at(7))
        .expect("third page");
    assert_eq!(third.session.confirmed_project_cursor, second_head);
    let third_head = third.delivered_through.expect("third boundary");
    assert!(third_head > second_head);
    let positions = third
        .changes
        .as_ref()
        .expect("third changes")
        .iter()
        .map(|change| change.entry.position.position)
        .collect::<Vec<_>>();
    assert_eq!(
        positions,
        (second_head + 1..=third_head).collect::<Vec<_>>()
    );
    let idle = service
        .work_next(20, changes_only(), at(8))
        .expect("idle page");
    assert_eq!(idle.session.confirmed_project_cursor, third_head);
    assert!(idle.changes.as_ref().expect("no new changes").is_empty());
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the two-thread regression keeps both competing pages and the durable winner visible"
)]
fn concurrent_same_session_delivery_returns_only_the_winning_exact_page() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("concurrent-delivery-cas".into());
    let session = SessionId("shared-session".into());
    let service = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        session.clone(),
        Some("protocol-test".into()),
    );
    let peer = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("seed-peer".into()),
        Some("protocol-test".into()),
    );
    service
        .work_propose(root_input("Shared focus", "shared-focus"), at(0))
        .expect("focused root");
    for index in 0..4 {
        peer.work_propose(
            root_input(
                &format!("Concurrent source {index}"),
                &format!("concurrent-source-{index}"),
            ),
            at(index + 1),
        )
        .expect("seed project event");
    }

    let entered = Arc::new(Barrier::new(3));
    let release = Arc::new(Barrier::new(3));
    let hook = DeliveryStageTestHook {
        entered: entered.clone(),
        release: release.clone(),
    };
    let mut short = service.clone();
    short.delivery_stage_hook = Some(hook.clone());
    let mut long = service.clone();
    long.delivery_stage_hook = Some(hook);
    let short_call = std::thread::spawn(move || {
        short.work_next(
            1,
            WorkNextQuery {
                sections: vec![WorkNextSection::Changes],
                ..WorkNextQuery::default()
            },
            at(10),
        )
    });
    let long_call = std::thread::spawn(move || {
        long.work_next(
            20,
            WorkNextQuery {
                sections: vec![WorkNextSection::Changes],
                ..WorkNextQuery::default()
            },
            at(10),
        )
    });
    entered.wait();
    release.wait();
    let short_response = short_call
        .join()
        .expect("short thread")
        .expect("short page");
    let long_response = long_call.join().expect("long thread").expect("long page");

    assert_eq!(
        short_response.delivered_through,
        long_response.delivered_through
    );
    assert_eq!(short_response.delivery_token, long_response.delivery_token);
    assert_eq!(
        serde_json::to_value(&short_response.changes).expect("short changes"),
        serde_json::to_value(&long_response.changes).expect("long changes")
    );
    let store = SqliteStore::open(&database).expect("durable delivery store");
    let state = store
        .work_session_state(&project, &session, at(11))
        .expect("durable delivery state");
    assert_eq!(
        state.tentative_project_cursor,
        short_response.delivered_through
    );
    assert_eq!(
        state.tentative_delivery_token,
        short_response.delivery_token
    );
    let durable: StagedWorkChangePage = store
        .staged_work_session_delivery_payload(&project, &session)
        .expect("durable payload")
        .expect("pending payload")
        .decode()
        .expect("decode durable payload");
    assert_eq!(
        serde_json::to_value(&durable.changes).expect("durable changes"),
        serde_json::to_value(&short_response.changes).expect("response changes")
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the deterministic focus race keeps the losing projection and durable reprojected page in one scenario"
)]
fn focus_winning_before_delivery_stage_forces_reprojection() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("focus-delivery-cas".into());
    let session = SessionId("focus-race-session".into());
    let service = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        session.clone(),
        Some("protocol-test".into()),
    );
    let peer = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("focus-race-peer".into()),
        Some("protocol-test".into()),
    );
    let original = match service
        .work_propose(root_input("Original focus", "focus-race-original"), at(0))
        .expect("original root")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    let replacement = match peer
        .work_propose(
            root_input("Replacement focus", "focus-race-replacement"),
            at(1),
        )
        .expect("replacement root")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: None,
                recovery_reason: None,
                idempotency_key: "focus-race-original-claim".into(),
            },
            at(1),
        )
        .expect("claim original focus");
    peer.work_update(
        WorkUpdateInput::Claim {
            ttl_seconds: None,
            recovery_reason: None,
            idempotency_key: "focus-race-replacement-claim".into(),
        },
        at(1),
    )
    .expect("claim replacement focus");
    let mut memories = SqliteStore::open(&database).expect("memory store");
    for (work_id, prose, key, second, actor) in [
        (
            original.work_id,
            "original-root memory",
            "focus-race-original-memory",
            2,
            service.actor("memory_note", "seed original focus-sensitive delta"),
        ),
        (
            replacement.work_id,
            "replacement-root memory",
            "focus-race-replacement-memory",
            3,
            peer.actor("memory_note", "seed replacement focus-sensitive delta"),
        ),
    ] {
        memories
            .capture_note(
                &crate::NoteRequest {
                    project_id: project.clone(),
                    task_id: None,
                    work_id: Some(work_id),
                    prose: prose.into(),
                    visibility: crate::NoteVisibility::Shared,
                    kind: None,
                    authority: None,
                    sensitivity: Some(Sensitivity::Internal),
                    title: None,
                    tags: Vec::new(),
                    evidence: Vec::new(),
                    refs: Vec::new(),
                    actor,
                    idempotency_key: key.into(),
                    created_at: at(second),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("capture focus-sensitive memory");
    }
    drop(memories);

    let entered = Arc::new(Barrier::new(2));
    let release = Arc::new(Barrier::new(2));
    let mut delivery = service.clone();
    delivery.delivery_stage_hook = Some(DeliveryStageTestHook {
        entered: entered.clone(),
        release: release.clone(),
    });
    let delivery_call = std::thread::spawn(move || {
        delivery.work_next(
            20,
            WorkNextQuery {
                sections: vec![WorkNextSection::Changes],
                ..WorkNextQuery::default()
            },
            at(3),
        )
    });
    entered.wait();
    service
        .work_focus(&replacement.short_ref, at(3))
        .expect("focus wins before staging");
    release.wait();
    let response = delivery_call
        .join()
        .expect("delivery thread")
        .expect("delivery reprojects after focus CAS loss");
    assert_eq!(response.session.focused_work_id, Some(replacement.work_id));
    let memory_changes = response
        .changes
        .as_ref()
        .expect("changes")
        .iter()
        .filter(|change| change.entry.object_kind == "memory_version")
        .collect::<Vec<_>>();
    assert_eq!(memory_changes.len(), 2);
    assert!(matches!(
        &memory_changes[0].delivery,
        WorkChangeProjection::Omitted(WorkChangeOmission {
            omission: WorkChangeOmissionReason::OutsideFocusedRoot,
            ..
        })
    ));
    assert!(matches!(
        &memory_changes[1].delivery,
        WorkChangeProjection::Visible(summary)
            if summary.work_id == Some(replacement.work_id)
    ));

    let store = SqliteStore::open(&database).expect("durable delivery store");
    let state = store
        .work_session_state(&project, &session, at(4))
        .expect("durable state");
    assert_eq!(state.focused_work_id, Some(replacement.work_id));
    assert_eq!(state.tentative_project_cursor, response.delivered_through);
    assert_eq!(state.tentative_delivery_token, response.delivery_token);
    let durable: StagedWorkChangePage = store
        .staged_work_session_delivery_payload(&project, &session)
        .expect("durable payload")
        .expect("pending payload")
        .decode()
        .expect("decode durable payload");
    assert_eq!(
        serde_json::to_value(&durable.changes).expect("durable changes"),
        serde_json::to_value(&response.changes).expect("response changes")
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the regression proves contradiction capture, delivery acknowledgement, and restart integrity as one scenario"
)]
fn work_scoped_contradiction_drains_through_work_next_and_doctor() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("contradiction-delivery".into());
    let session = SessionId("contradiction-session".into());
    let service = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        session.clone(),
        Some("protocol-test".into()),
    );
    let root = match service
        .work_propose(
            root_input("Contradiction delivery", "contradiction-root"),
            at(0),
        )
        .expect("root proposal")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };
    service
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: None,
                recovery_reason: None,
                idempotency_key: "contradiction-claim".into(),
            },
            at(1),
        )
        .expect("claim contradiction work");
    let mut store = SqliteStore::open(&database).expect("store");
    let task = store
        .start_task(
            &project,
            "dummy:MIXED-CONTRADICTION",
            "Mixed contradiction applicability",
            &session,
            service.actor("task_start", "bind mixed contradiction task"),
            at(1),
        )
        .expect("task binding")
        .task;
    let left = store
        .capture_note(
            &crate::NoteRequest {
                project_id: project.clone(),
                task_id: None,
                work_id: Some(root.work_id),
                prose: "Constraint: use the first mutually exclusive work rule".into(),
                visibility: crate::NoteVisibility::Shared,
                kind: None,
                authority: None,
                sensitivity: None,
                title: None,
                tags: Vec::new(),
                evidence: Vec::new(),
                refs: Vec::new(),
                actor: service.actor("memory_note", "capture first work rule"),
                idempotency_key: "contradiction-left".into(),
                created_at: at(1),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("left note");
    let right = store
        .capture_note(
            &crate::NoteRequest {
                project_id: project.clone(),
                task_id: None,
                work_id: Some(root.work_id),
                prose: "Constraint: use the second mutually exclusive work rule".into(),
                visibility: crate::NoteVisibility::Shared,
                kind: None,
                authority: None,
                sensitivity: None,
                title: None,
                tags: Vec::new(),
                evidence: Vec::new(),
                refs: Vec::new(),
                actor: service.actor("memory_note", "capture second work rule"),
                idempotency_key: "contradiction-right".into(),
                created_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("right note");
    let project_memory = store
        .capture_note(
            &crate::NoteRequest {
                project_id: project.clone(),
                task_id: None,
                work_id: None,
                prose: "Project-wide constraint for mixed contradiction".into(),
                visibility: crate::NoteVisibility::Shared,
                kind: None,
                authority: None,
                sensitivity: None,
                title: None,
                tags: Vec::new(),
                evidence: Vec::new(),
                refs: Vec::new(),
                actor: service.actor("memory_note", "capture project constraint"),
                idempotency_key: "contradiction-project".into(),
                created_at: at(3),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("project note");
    let task_memory = store
        .capture_note(
            &crate::NoteRequest {
                project_id: project.clone(),
                task_id: Some(task.task_id),
                work_id: None,
                prose: "Task constraint for mixed contradiction".into(),
                visibility: crate::NoteVisibility::Shared,
                kind: None,
                authority: None,
                sensitivity: None,
                title: None,
                tags: Vec::new(),
                evidence: Vec::new(),
                refs: Vec::new(),
                actor: service.actor("memory_note", "capture task constraint"),
                idempotency_key: "contradiction-task".into(),
                created_at: at(4),
            },
            &DevelopmentNoopRedactor,
        )
        .expect("task note");
    let contradiction = store
        .record_memory_contradiction(
            &project,
            None,
            Some(root.work_id),
            &session,
            "agent",
            &left.version,
            &right.version,
            "the two work rules cannot both guide execution",
            "contradiction-edge",
            service.actor("memory_contradict", "record explicit work contradiction"),
            at(5),
            &DevelopmentNoopRedactor,
        )
        .expect("contradiction");
    let project_contradiction = store
        .record_memory_contradiction(
            &project,
            None,
            Some(root.work_id),
            &session,
            "agent",
            &left.version,
            &project_memory.version,
            "work and project guidance conflict",
            "contradiction-work-project",
            service.actor("memory_contradict", "record mixed project contradiction"),
            at(6),
            &DevelopmentNoopRedactor,
        )
        .expect("work and project contradiction");
    let task_contradiction = store
        .record_memory_contradiction(
            &project,
            Some(task.task_id),
            Some(root.work_id),
            &session,
            "agent",
            &right.version,
            &task_memory.version,
            "work and task guidance conflict",
            "contradiction-work-task",
            service.actor("memory_contradict", "record mixed task contradiction"),
            at(7),
            &DevelopmentNoopRedactor,
        )
        .expect("work and task contradiction");
    assert!(!contradiction.work_positions.is_empty());
    assert!(!project_contradiction.work_positions.is_empty());
    assert!(!task_contradiction.work_positions.is_empty());
    assert!(store.verify_all().expect("integrity report").is_healthy());
    drop(store);

    let mut page = service
        .work_next(100, WorkNextQuery::default(), at(8))
        .expect("deliver contradiction event");
    let expected = [
        contradiction.contradiction,
        project_contradiction.contradiction,
        task_contradiction.contradiction,
    ];
    let mut visible = std::collections::HashSet::new();
    let mut confirmed = 0;
    for offset in 0..8 {
        for change in page.changes.as_deref().unwrap_or_default() {
            if change.entry.object_kind == "memory_contradiction_event"
                && matches!(change.delivery, WorkChangeProjection::Visible(_))
            {
                visible.insert(change.entry.object_hash.clone());
            }
        }
        let delivered = page.delivered_through.expect("delivered cursor");
        let delivery_token = page.delivery_token.as_deref().expect("delivery token");
        page = service
            .work_next_with_delivery_token(
                100,
                Some(delivered),
                Some(delivery_token),
                WorkNextQuery::default(),
                at(9 + offset),
            )
            .expect("acknowledge contradiction page");
        confirmed = page.session.confirmed_project_cursor;
        if expected.iter().all(|hash| visible.contains(hash)) {
            break;
        }
    }
    assert!(expected.iter().all(|hash| visible.contains(hash)));
    assert!(confirmed > 0);
    assert!(
        SqliteStore::open(&database)
            .expect("reopen store")
            .verify_all()
            .expect("integrity report after delivery")
            .is_healthy()
    );
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "the confidentiality regression covers visible, restricted, and cross-root memory feed pairs"
)]
fn work_next_redacts_restricted_and_out_of_root_memory_without_cursor_gaps() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("feed-boundary-project".into());
    let focused = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("focused-session".into()),
        Some("protocol-test".into()),
    );
    let peer = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("peer-session".into()),
        Some("protocol-test".into()),
    );
    let focused_root = match focused
        .work_propose(root_input("Focused root", "focused-root"), at(0))
        .expect("focused root proposal")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected focused root"),
    };
    let peer_root = match peer
        .work_propose(root_input("Peer root", "peer-root"), at(1))
        .expect("peer root proposal")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected peer root"),
    };
    focused
        .work_update(
            WorkUpdateInput::Claim {
                ttl_seconds: None,
                recovery_reason: None,
                idempotency_key: "focused-memory-claim".into(),
            },
            at(1),
        )
        .expect("claim focused root");
    peer.work_update(
        WorkUpdateInput::Claim {
            ttl_seconds: None,
            recovery_reason: None,
            idempotency_key: "peer-memory-claim".into(),
        },
        at(1),
    )
    .expect("claim peer root");
    let mut store = SqliteStore::open(&database).expect("store");
    let (visible, restricted, outside, outside_second) = {
        let mut capture = |work_id: WorkId,
                           prose: &str,
                           sensitivity: Sensitivity,
                           key: &str,
                           actor: ActorContext,
                           captured_at: DateTime<Utc>| {
            store
                .capture_note(
                    &crate::NoteRequest {
                        project_id: project.clone(),
                        task_id: None,
                        work_id: Some(work_id),
                        prose: prose.into(),
                        visibility: crate::NoteVisibility::Shared,
                        kind: None,
                        authority: None,
                        sensitivity: Some(sensitivity),
                        title: None,
                        tags: Vec::new(),
                        evidence: Vec::new(),
                        refs: Vec::new(),
                        actor,
                        idempotency_key: key.into(),
                        created_at: captured_at,
                    },
                    &DevelopmentNoopRedactor,
                )
                .expect("capture work memory")
        };
        let visible = capture(
            focused_root.work_id,
            "visible focused-root memory",
            Sensitivity::Internal,
            "visible-memory",
            focused.actor("memory_note", "capture focused-root memory"),
            at(2),
        );
        let restricted = capture(
            focused_root.work_id,
            "restricted focused-root secret",
            Sensitivity::Restricted,
            "restricted-memory",
            focused.actor("memory_note", "capture restricted focused-root memory"),
            at(3),
        );
        let outside = capture(
            peer_root.work_id,
            "unrelated root memory",
            Sensitivity::Internal,
            "outside-memory",
            peer.actor("memory_note", "capture peer-root memory"),
            at(4),
        );
        let outside_second = capture(
            peer_root.work_id,
            "second unrelated root memory",
            Sensitivity::Internal,
            "outside-memory-second",
            peer.actor("memory_note", "capture second peer-root memory"),
            at(5),
        );
        (visible, restricted, outside, outside_second)
    };
    let peer_contradiction = store
        .record_memory_contradiction(
            &project,
            None,
            Some(peer_root.work_id),
            &peer.session_id,
            "agent",
            &outside.version,
            &outside_second.version,
            "peer-root contradiction must remain outside focused delivery",
            "peer-root-contradiction",
            peer.actor("memory_contradict", "record peer-root contradiction"),
            at(6),
            &DevelopmentNoopRedactor,
        )
        .expect("peer-root contradiction");
    let restricted_contradiction = MemoryContradictionEvent {
        schema_version: SCHEMA_VERSION,
        project_id: project.clone(),
        task_id: None,
        work_root_id: Some(focused_root.root_id),
        left_version: visible.version.clone(),
        right_version: restricted.version.clone(),
        reason: "restricted contradiction payload".into(),
        actor: focused.actor("memory_contradict", "exercise restricted projection"),
        created_at: at(7),
    };
    assert!(matches!(
        agent_change_object(
            &store,
            &project,
            Some(focused_root.root_id),
            None,
            "memory_contradiction_event",
            serde_json::to_value(restricted_contradiction)
                .expect("serialize restricted contradiction"),
            None,
        )
        .expect("restricted contradiction projection"),
        WorkChangeProjection::Omitted(WorkChangeOmission {
            omission: WorkChangeOmissionReason::RestrictedSensitivity,
            ..
        })
    ));
    drop(store);

    let mut page = focused
        .work_next(100, WorkNextQuery::default(), at(8))
        .expect("bounded project delta");
    let expected_hashes = [
        &visible.version,
        &restricted.version,
        &restricted.assertion,
        &outside.version,
        &outside.assertion,
        &peer_contradiction.contradiction,
    ];
    let mut changes = Vec::new();
    let mut final_confirmed = 0;
    for offset in 0..8 {
        let delivered = page.delivered_through.expect("delivered cursor");
        let page_changes = page.changes.as_ref().expect("changes section");
        assert_eq!(
            i64::try_from(page_changes.len()).expect("change count"),
            delivered - page.session.confirmed_project_cursor
        );
        changes.extend(page_changes.iter().cloned());
        let delivery_token = page.delivery_token.as_deref().expect("delivery token");
        page = focused
            .work_next_with_delivery_token(
                100,
                Some(delivered),
                Some(delivery_token),
                WorkNextQuery::default(),
                at(9 + offset),
            )
            .expect("acknowledge protected delta page");
        final_confirmed = page.session.confirmed_project_cursor;
        if expected_hashes.iter().all(|hash| {
            changes
                .iter()
                .any(|change| &change.entry.object_hash == *hash)
        }) {
            break;
        }
    }
    let projection_for = |hash: &ObjectHash| {
        &changes
            .iter()
            .find(|change| &change.entry.object_hash == hash)
            .expect("feed object")
            .delivery
    };
    assert!(matches!(
        projection_for(&visible.version),
        WorkChangeProjection::Visible(value) if value.change_kind == "memory_version"
    ));
    for hash in [&restricted.version, &restricted.assertion] {
        assert!(matches!(
            projection_for(hash),
            WorkChangeProjection::Omitted(WorkChangeOmission {
                omission: WorkChangeOmissionReason::RestrictedSensitivity,
                ..
            })
        ));
    }
    for hash in [&outside.version, &outside.assertion] {
        assert!(matches!(
            projection_for(hash),
            WorkChangeProjection::Omitted(WorkChangeOmission {
                omission: WorkChangeOmissionReason::OutsideFocusedRoot,
                ..
            })
        ));
    }
    assert!(matches!(
        projection_for(&peer_contradiction.contradiction),
        WorkChangeProjection::Omitted(WorkChangeOmission {
            omission: WorkChangeOmissionReason::OutsideFocusedRoot,
            ..
        })
    ));
    let serialized = serde_json::to_string(&changes).expect("serialize work_next changes");
    assert!(serialized.contains("visible focused-root memory"));
    assert!(!serialized.contains("restricted focused-root secret"));
    assert!(!serialized.contains("unrelated root memory"));
    assert!(!serialized.contains("second unrelated root memory"));
    assert!(!serialized.contains("peer-root contradiction must remain outside focused delivery"));
    assert!(final_confirmed > 0);
    assert!(
        SqliteStore::open(&database)
            .expect("reopen store")
            .verify_all()
            .expect("integrity report")
            .is_healthy()
    );
}

#[test]
fn compact_agent_memory_signal_is_acknowledged_only_after_delivery() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("deferred-memory-advertisement".into());
    let service = LocalWorkService::new(
        database,
        project,
        "agent".into(),
        SessionId("deferred-memory-session".into()),
        Some("memory-advertisement-test".into()),
    );
    service
        .remember_project_memory("retained fact".into(), Some("retained-fact".into()), at(0))
        .expect("remember fixture");
    let query = WorkNextQuery {
        sections: vec![WorkNextSection::Memories],
        ..WorkNextQuery::default()
    };
    let first = service
        .work_next_for_agent(20, query.clone(), at(1))
        .expect("first deferred signal");
    assert!(first.memories.as_ref().is_some_and(|signal| signal.changed));
    assert!(first.memory_advertisement.is_some());
    let repeated = service
        .work_next_for_agent(20, query.clone(), at(2))
        .expect("unacknowledged signal repeats");
    assert!(
        repeated
            .memories
            .as_ref()
            .is_some_and(|signal| signal.changed)
    );
    service.acknowledge_work_next_memories(&first, at(2));
    let stable = service
        .work_next_for_agent(20, query, at(3))
        .expect("acknowledged signal is stable");
    assert!(
        stable
            .memories
            .as_ref()
            .is_some_and(|signal| !signal.changed)
    );
    assert!(stable.memory_advertisement.is_none());
}

#[test]
fn rejected_memory_advisory_cannot_consume_an_unseen_work_change_page() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("memory-advisory-delivery-order".into());
    let session = SessionId("memory-advisory-delivery-session".into());
    let reader = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "reader".into(),
        session.clone(),
        Some("memory-advisory-test".into()),
    );
    let writer = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "writer".into(),
        SessionId("memory-advisory-writer".into()),
        Some("memory-advisory-test".into()),
    );
    let created = match writer
        .work_propose(
            root_input("Peer change", "memory-advisory-peer-change"),
            at(0),
        )
        .expect("create peer change")
    {
        WorkProposeResult::Root { work, .. } => work,
        WorkProposeResult::Decomposition(_) => panic!("expected root"),
    };

    assert!(matches!(
        reader.work_next_for_agent(
            20,
            WorkNextQuery {
                context_generation: Some("invalid\ncontext".into()),
                ..WorkNextQuery::default()
            },
            at(1),
        ),
        Err(StoreError::InvalidProjectMemory(_))
    ));
    let after_refusal = SqliteStore::open(&database)
        .expect("store")
        .work_session_state(&project, &session, at(1))
        .expect("session state after refusal");
    assert_eq!(after_refusal.project_cursor, 0);
    assert_eq!(after_refusal.tentative_project_cursor, None);

    let replayed = reader
        .work_next_for_agent(20, WorkNextQuery::default(), at(2))
        .expect("corrected call delivers unseen page");
    assert!(replayed.changes.as_ref().is_some_and(|changes| {
        changes.iter().any(|change| {
            matches!(
                &change.delivery,
                WorkChangeProjection::Visible(summary)
                    if summary.work_id == Some(created.work_id)
            )
        })
    }));
    assert!(replayed.delivered_through.is_some());
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one scale regression creates, selects, replays, and densely drains the complete bounded protocol scenario"
)]
fn work_next_is_byte_bounded_dense_and_section_selective_at_project_scale() {
    let directory = tempdir().expect("temp directory");
    let database = directory.path().join("engram.sqlite3");
    let project = ProjectId("bounded-work-project".into());
    let writer = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("writer".into()),
        Some("protocol-test".into()),
    );
    let reader = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("reader".into()),
        Some("protocol-test".into()),
    );

    let mut work_ids = Vec::new();
    for item_index in 0..500 {
        let work = match writer
            .work_propose(
                root_input(
                    &format!("Bounded item {item_index:03}"),
                    &format!("bounded-root-{item_index:03}"),
                ),
                at(i64::from(item_index)),
            )
            .expect("create bounded root")
        {
            WorkProposeResult::Root { work, .. } => work,
            WorkProposeResult::Decomposition(_) => panic!("expected root"),
        };
        work_ids.push(work.work_id);
    }
    let mut event_store = SqliteStore::open(&database).expect("event store");
    let last_work_id = *work_ids.last().expect("last scale work item");
    let entry = event_store
        .work_event_tail(last_work_id, 1)
        .expect("base event tail")
        .pop()
        .expect("base event");
    let base = event_store
        .get::<WorkEvent>(&entry.object_hash)
        .expect("load base event")
        .expect("base event object");
    for event_index in 0..9 {
        let mut event = base.clone();
        event.created_at = at(1_000 + i64::from(event_index));
        event.actor.reason = format!("bounded synthetic event {event_index}");
        event_store
            .append_test_work_event(&event)
            .expect("append canonical scale event");
    }

    let initial_head = SqliteStore::open(&database)
        .expect("store")
        .work_feed_head(&FeedId::Project(project.clone()))
        .expect("project feed head");
    assert_eq!(initial_head, 509);

    crate::storage::reset_work_event_decode_count();
    crate::storage::reset_work_item_projection_decode_count();
    let ready_started = std::time::Instant::now();
    reader
        .work_next(
            50,
            WorkNextQuery {
                sections: vec![WorkNextSection::Ready],
                ..WorkNextQuery::default()
            },
            at(1_100),
        )
        .expect("ready-only scale query");
    let ready_elapsed = ready_started.elapsed();
    let ready_event_decodes = crate::storage::work_event_decode_count();
    let ready_item_decodes = crate::storage::work_item_projection_decode_count();
    eprintln!(
        "work_next scale ready: elapsed_us={} event_decodes={} item_decodes={}",
        ready_elapsed.as_micros(),
        ready_event_decodes,
        ready_item_decodes
    );
    assert_eq!(ready_event_decodes, 0);
    assert!(ready_item_decodes <= 50);

    crate::storage::reset_work_event_decode_count();
    crate::storage::reset_work_item_projection_decode_count();
    let catalog_started = std::time::Instant::now();
    reader
        .work_next(
            50,
            WorkNextQuery {
                sections: vec![WorkNextSection::Catalog],
                ..WorkNextQuery::default()
            },
            at(1_101),
        )
        .expect("catalog-only scale query");
    let catalog_elapsed = catalog_started.elapsed();
    let catalog_event_decodes = crate::storage::work_event_decode_count();
    let catalog_item_decodes = crate::storage::work_item_projection_decode_count();
    eprintln!(
        "work_next scale catalog: elapsed_us={} event_decodes={} item_decodes={}",
        catalog_elapsed.as_micros(),
        catalog_event_decodes,
        catalog_item_decodes
    );
    assert_eq!(catalog_event_decodes, 0);
    assert!(catalog_item_decodes <= 51);

    crate::storage::reset_work_event_decode_count();
    crate::storage::reset_work_item_projection_decode_count();
    let selective_catalog_started = std::time::Instant::now();
    reader
        .work_next(
            50,
            WorkNextQuery {
                sections: vec![WorkNextSection::Catalog],
                search: Some("Bounded item 499".into()),
                ..WorkNextQuery::default()
            },
            at(1_102),
        )
        .expect("selective catalog scale query");
    let selective_catalog_elapsed = selective_catalog_started.elapsed();
    let selective_catalog_event_decodes = crate::storage::work_event_decode_count();
    let selective_catalog_item_decodes = crate::storage::work_item_projection_decode_count();
    eprintln!(
        "work_next scale selective_catalog: elapsed_us={} event_decodes={} item_decodes={}",
        selective_catalog_elapsed.as_micros(),
        selective_catalog_event_decodes,
        selective_catalog_item_decodes
    );
    assert_eq!(selective_catalog_event_decodes, 0);
    assert_eq!(selective_catalog_item_decodes, 1);

    crate::storage::reset_work_event_decode_count();
    crate::storage::reset_work_item_projection_decode_count();
    let first = reader
        .work_next(1_000, WorkNextQuery::default(), at(1_103))
        .expect("bounded default work_next");
    let first_decode_count = crate::storage::work_event_decode_count();
    let first_item_decode_count = crate::storage::work_item_projection_decode_count();
    eprintln!(
        "work_next scale default: event_decodes={first_decode_count} item_decodes={first_item_decode_count}"
    );
    assert_eq!(first_decode_count, 0);
    assert!(first_item_decode_count <= 1_000);
    assert!(
        serde_json::to_vec(&first)
            .expect("serialize first page")
            .len()
            <= MAX_AGENT_WORK_RESPONSE_BYTES
    );
    assert!(first.omissions.iter().any(|omission| {
        omission.section == WorkNextSection::Changes
            && omission.reason == WorkSectionOmissionReason::Staged
            && omission.omitted_count > 0
    }));
    let first_cursor = first.delivered_through.expect("first delivery cursor");
    let first_hashes = first
        .changes
        .as_ref()
        .expect("default changes")
        .iter()
        .map(|change| change.entry.object_hash.clone())
        .collect::<Vec<_>>();

    crate::storage::reset_work_event_decode_count();
    let mutation = writer
        .work_update(
            WorkUpdateInput::Revise {
                patch: WorkRevisionPatch {
                    title: Some(format!("Post-history mutation {}", "x".repeat(300))),
                    ..WorkRevisionPatch::default()
                },
                idempotency_key: "post-history-mutation".into(),
            },
            at(1_600),
        )
        .expect("mutation after long history");
    let mutation_decode_count = crate::storage::work_event_decode_count();
    assert!(
        mutation_decode_count < 50,
        "target mutation decoded {mutation_decode_count} canonical work events"
    );
    let mutation_bytes = serde_json::to_vec(&mutation).expect("serialize mutation result");
    assert!(
        mutation_bytes.len() < 2_048,
        "mutation response was {} bytes",
        mutation_bytes.len()
    );
    assert!(
        !String::from_utf8(mutation_bytes)
            .expect("UTF-8 response")
            .contains("history")
    );
    let head = initial_head + 1;

    let catalog_only = reader
        .work_next(
            50,
            WorkNextQuery {
                sections: vec![WorkNextSection::Catalog],
                ..WorkNextQuery::default()
            },
            at(601),
        )
        .expect("catalog-only page");
    assert!(catalog_only.changes.is_none());
    assert!(catalog_only.delivered_through.is_none());
    assert_eq!(catalog_only.session.confirmed_project_cursor, 0);
    assert!(catalog_only.session.pending_delivery);
    assert!(
        serde_json::to_vec(&catalog_only)
            .expect("serialize catalog page")
            .len()
            <= MAX_AGENT_WORK_RESPONSE_BYTES
    );

    // The next changes call delivers the first page implicitly and
    // continues densely from its boundary.
    let following = reader
        .work_next(
            1_000,
            WorkNextQuery {
                sections: vec![WorkNextSection::Changes],
                ..WorkNextQuery::default()
            },
            at(602),
        )
        .expect("following staged changes");
    assert_eq!(following.session.confirmed_project_cursor, first_cursor);
    let following_cursor = following
        .delivered_through
        .expect("following delivery cursor");
    assert!(following_cursor > first_cursor);
    let following_changes = following.changes.as_ref().expect("following changes");
    assert_ne!(
        following_changes
            .iter()
            .map(|change| change.entry.object_hash.clone())
            .collect::<Vec<_>>(),
        first_hashes
    );
    for (offset, change) in following_changes.iter().enumerate() {
        assert_eq!(
            change.entry.position.position,
            first_cursor + 1 + i64::try_from(offset).expect("offset")
        );
    }

    let mut expected_position = following_cursor + 1;
    let mut acknowledge = None;
    let mut acknowledge_token = None;
    loop {
        let page = reader
            .work_next_with_delivery_token(
                1_000,
                acknowledge,
                acknowledge_token.as_deref(),
                WorkNextQuery {
                    sections: vec![WorkNextSection::Changes],
                    ..WorkNextQuery::default()
                },
                at(603 + expected_position),
            )
            .expect("drain bounded changes");
        let delivered = page.delivered_through.expect("delivery cursor");
        let delivery_token = page.delivery_token.clone().expect("delivery token");
        let changes = page.changes.as_ref().expect("changes");
        assert!(
            serde_json::to_vec(&page)
                .expect("serialize delta page")
                .len()
                <= MAX_AGENT_WORK_RESPONSE_BYTES
        );
        for change in changes {
            assert_eq!(change.entry.position.position, expected_position);
            expected_position += 1;
        }
        if delivered == head {
            reader
                .work_next_with_delivery_token(
                    1,
                    Some(delivered),
                    Some(delivery_token.as_str()),
                    WorkNextQuery {
                        sections: vec![WorkNextSection::Catalog],
                        ..WorkNextQuery::default()
                    },
                    at(1_200),
                )
                .expect("acknowledge final page without staging more changes");
            break;
        }
        acknowledge = Some(delivered);
        acknowledge_token = Some(delivery_token);
    }
    assert_eq!(expected_position, head + 1);
}
