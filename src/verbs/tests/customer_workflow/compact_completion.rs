use super::*;

#[test]
fn compact_peer_completion_keeps_ordinary_checkpoints_and_collapses_owned_captures_on_replay() {
    for capture_at_completion in [false, true] {
        let (_directory, reader, path, project) = fixture();
        let peer = AgentVerbs::new(
            path.clone(),
            project.clone(),
            "agent".into(),
            SessionId("peer".into()),
            None,
        );
        let work = add(&peer, "Peer delivery", None, false, 0);
        peer.claim(
            ClaimInput {
                work_ref: work.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(1),
        )
        .unwrap();
        note(&peer, &work, "Substantive progress", 2);
        // Equal prose does not make this separate checkpoint part of the note.
        peer.service
            .work_update(
                crate::work_service::WorkUpdateInput::Checkpoint {
                    summary: "Substantive progress".into(),
                    evidence: None,
                    idempotency_key: "ordinary".into(),
                },
                at(3),
            )
            .unwrap();
        peer.service
            .work_complete(
                crate::work_service::WorkCompleteInput {
                    capture: capture_at_completion.then(|| {
                        crate::work_service::WorkCompletionCaptureInput {
                            summary: "Completion capture".into(),
                            refs: vec![],
                        }
                    }),
                    evidence: vec![],
                    acceptance: None,
                    note: Some("Delivered".into()),
                    links: vec![],
                    link_basis: None,
                    source_fingerprint: None,
                    landing: None,
                    idempotency_key: "complete".into(),
                },
                at(4),
            )
            .unwrap();
        let query = || WorkNextQuery {
            sections: vec![WorkNextSection::Changes],
            ..Default::default()
        };
        let lines = |changes: &[WorkChange]| {
            collapsed_changes(changes, reader.service.display_identity())
                .into_iter()
                .map(|row| row.line)
                .collect::<Vec<_>>()
        };
        let store = SqliteStore::open(&path).unwrap();
        let mut changes = Vec::new();
        // Consume every bounded page, checking replay separately for each.
        // Combining the retained records also exercises collapse when the
        // checkpoint and its completion are present in the same renderer cut.
        for _ in 0..10 {
            let first = reader.service.work_next(50, query(), at(5)).unwrap();
            if first.changes.as_ref().unwrap().is_empty() {
                break;
            }
            let before = store
                .staged_work_session_delivery_payload(&project, &SessionId("agent".into()))
                .unwrap()
                .unwrap();
            let confirmed = store
                .work_session_state(&project, &SessionId("agent".into()), at(5))
                .unwrap()
                .project_cursor;
            let replay = reader
                .service
                .work_next_with_delivery_token(50, Some(confirmed), None, query(), at(6))
                .unwrap();
            assert_eq!(
                lines(first.changes.as_ref().unwrap()),
                lines(replay.changes.as_ref().unwrap())
            );
            assert_eq!(
                before,
                store
                    .staged_work_session_delivery_payload(&project, &SessionId("agent".into()))
                    .unwrap()
                    .unwrap()
            );
            changes.extend(first.changes.unwrap());
        }
        let first_lines = lines(&changes);
        assert_eq!(
            first_lines
                .iter()
                .filter(|line| line.contains("noted") && line.contains("Substantive progress"))
                .count(),
            1,
            "{first_lines:?}"
        );
        assert_eq!(
            first_lines
                .iter()
                .filter(
                    |line| line.contains("checkpointed") && line.contains("Substantive progress")
                )
                .count(),
            1,
            "{first_lines:?}"
        );
        assert!(
            !first_lines
                .iter()
                .any(|line| line.contains("checkpointed") && line.contains("Completion capture")),
            "{first_lines:?}"
        );
        assert!(
            first_lines.iter().any(|line| line.contains("completed")),
            "{first_lines:?}"
        );
    }
}

#[test]
fn refused_completion_capture_collapses_on_separate_delivery_pages_and_replay() {
    let (_directory, reader, path, project) = fixture();
    let peer = AgentVerbs::new(
        path.clone(),
        project.clone(),
        "peer".into(),
        SessionId("peer".into()),
        None,
    );
    let work = add(&peer, "Refused completion", None, false, 0);
    peer.claim(
        ClaimInput {
            work_ref: work.clone(),
            ttl_seconds: None,
            recover: None,
        },
        at(1),
    )
    .unwrap();
    // Child readiness is enforced after the completion capture is recorded;
    // evaluated-policy readiness would refuse before creating that capture.
    peer.service
        .work_update(
            crate::work_service::WorkUpdateInput::Checkpoint {
                summary: "Ordinary progress".into(),
                evidence: None,
                idempotency_key: "ordinary".into(),
            },
            at(2),
        )
        .unwrap();
    add(&peer, "Unfinished required child", Some(&work), false, 2);
    let store = SqliteStore::open(&path).unwrap();
    let result = peer
        .service
        .work_complete_on(
            Some(&work),
            crate::work_service::WorkCompleteInput {
                capture: Some(crate::work_service::WorkCompletionCaptureInput {
                    summary: "Refused capture".into(),
                    refs: vec![],
                }),
                evidence: vec![],
                acceptance: None,
                note: None,
                links: vec![],
                link_basis: None,
                source_fingerprint: None,
                landing: None,
                idempotency_key: "refused".into(),
            },
            at(3),
        )
        .unwrap();
    assert!(matches!(
        result,
        crate::work_service::WorkCompleteResult::Refused(_)
    ));
    let query = || WorkNextQuery {
        sections: vec![WorkNextSection::Changes],
        ..Default::default()
    };
    let lines = |changes: &[WorkChange]| {
        collapsed_changes(changes, reader.service.display_identity())
            .into_iter()
            .map(|row| row.line)
            .collect::<Vec<_>>()
    };
    let mut rendered = Vec::new();
    let mut checkpoints = 0;
    let mut ordinary_checkpoints = 0;
    let mut note_page = None;
    let mut checkpoint_page = None;
    for page in 0..30 {
        // One entry forces the capture note and its checkpoint onto separate pages.
        let first = reader.service.work_next(1, query(), at(4)).unwrap();
        let changes = first.changes.as_ref().unwrap();
        if changes.is_empty() {
            break;
        }
        let payload = store
            .staged_work_session_delivery_payload(&project, &SessionId("agent".into()))
            .unwrap()
            .unwrap();
        let cursor = store
            .work_session_state(&project, &SessionId("agent".into()), at(4))
            .unwrap()
            .project_cursor;
        let replay = reader
            .service
            .work_next_with_delivery_token(1, Some(cursor), None, query(), at(5))
            .unwrap();
        assert_eq!(lines(changes), lines(replay.changes.as_ref().unwrap()));
        assert_eq!(
            payload,
            store
                .staged_work_session_delivery_payload(&project, &SessionId("agent".into()))
                .unwrap()
                .unwrap()
        );
        for change in changes {
            if change.entry.object_kind == "work_event" {
                let event: crate::domain::WorkEvent =
                    store.get(&change.entry.object_id).unwrap().unwrap();
                if matches!(
                    event.transition,
                    crate::domain::WorkTransition::Checkpointed { .. }
                ) && event.actor.source_tool.as_deref() == Some("work_complete")
                {
                    assert!(change.capture.is_some());
                    assert!(
                        !lines(changes)
                            .iter()
                            .any(|line| line.contains("checkpointed")),
                        "completion-owned checkpoint event must collapse too"
                    );
                }
            }
            if change.entry.object_kind == "work_evidence" {
                note_page = Some(page);
            }
            if change.entry.object_kind == "work_checkpoint" {
                let checkpoint: crate::domain::WorkCheckpoint =
                    store.get(&change.entry.object_id).unwrap().unwrap();
                if checkpoint.actor.source_tool.as_deref() == Some("work_complete") {
                    assert_eq!(checkpoint.summary, "Refused capture");
                    checkpoints += 1;
                    checkpoint_page = Some(page);
                    assert_ne!(
                        change.capture.as_ref().unwrap().hash,
                        change.entry.object_id
                    );
                } else {
                    ordinary_checkpoints += 1;
                    assert_eq!(
                        change.capture.as_ref().unwrap().hash,
                        change.entry.object_id
                    );
                    assert!(
                        lines(changes)
                            .iter()
                            .any(|line| line.contains("checkpointed")),
                        "ordinary checkpoint must remain visible"
                    );
                }
            }
        }
        let page_lines = lines(changes);
        assert!(
            !page_lines.iter().any(|line| (line.contains("checkpointed")
                && line.contains("Refused capture"))
                || line.contains("completed")),
            "{page_lines:?}"
        );
        rendered.extend(page_lines);
    }
    assert_eq!(checkpoints, 1);
    assert!(ordinary_checkpoints > 0);
    assert!(note_page.is_some() && checkpoint_page.is_some());
    assert_ne!(note_page, checkpoint_page);
    assert_eq!(
        rendered
            .iter()
            .filter(|line| line.contains("noted") && line.contains("Refused capture"))
            .count(),
        1,
        "{rendered:?}"
    );
    assert!(store.verify_all().unwrap().is_healthy());
}
