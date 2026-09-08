use super::*;

#[test]
fn attribution_same_actor_different_sessions_are_not_you() {
    let (_directory, reader, path, project) = fixture();
    let work = add(&reader, "Shared actor attribution", None, false, 0);
    let peer = AgentVerbs::new(
        path,
        project,
        "agent".into(),
        SessionId("different-session".into()),
        None,
    );
    note(&peer, &work, "Peer observation", 1);
    let receipt = reader.show(&work, at(2)).expect("show");
    let by = receipt.value["notes"][0]["by"].as_str().expect("author");
    assert!(by.starts_with("peer-"), "peer was attributed as {by}");
    assert!(!receipt.text().contains("by you"));
    assert!(!receipt.text().contains("different-session"));
}

#[test]
fn attribution_actor_only_fallback_uses_full_identity_not_the_summary() {
    let (_directory, reader, _, _) = fixture();
    let work = add(&reader, "Actor-only fallback", None, false, 0);
    note(&reader, &work, "Original note", 1);
    let source = reader.service.work_focus_for_agent(&work, at(2)).unwrap();
    let mut labels = Vec::new();
    for suffix in ["first", "second"] {
        let raw = format!("{}-{suffix}", "same-prefix".repeat(30));
        let mut view = source.clone();
        let mut evidence = view.latest_evidence_item.take().unwrap();
        // Exercise the actor-only projection shape, including its independently
        // bounded rich field. Neither clone is written back into canonical data.
        evidence.producer_session_id = None;
        evidence.display_actor_session_id = None;
        evidence.actor_id = Some("same-prefix…".into());
        evidence.display_actor_id = Some(raw.clone());
        view.evidence_items = vec![evidence];
        let notes = crate::verbs::show::show_notes(&view, reader.service.display_identity());
        let value = serde_json::to_value(notes).unwrap();
        assert_eq!(
            value[0]["by"],
            reader.service.display_identity().actor(&raw)
        );
        labels.push(value[0]["by"].clone());
    }
    assert_ne!(labels[0], labels[1]);
}

#[test]
fn attribution_peer_labels_agree_across_reads_and_preserve_canonical_authors() {
    let (_directory, reader, path, project) = fixture();
    let work = add(&reader, "Distinct peer authors", None, false, 0);
    let sessions = [
        SessionId(uuid::Uuid::new_v4().to_string()),
        SessionId(uuid::Uuid::new_v4().to_string()),
    ];
    let mut labels = Vec::new();
    for (index, session) in sessions.iter().enumerate() {
        let writer = AgentVerbs::new(
            path.clone(),
            project.clone(),
            "agent".into(),
            session.clone(),
            None,
        );
        note(
            &writer,
            &work,
            &format!("Peer observation {index}"),
            i64::try_from(index).unwrap() + 1,
        );
    }
    let shown = reader.show(&work, at(5)).unwrap();
    let notes = reader
        .show_records(
            &work,
            &ShowInput {
                notes: true,
                ..Default::default()
            },
            at(5),
        )
        .unwrap();
    for (index, session) in sessions.iter().enumerate() {
        let summary = format!("Peer observation {index}");
        let row = notes.value["notes"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["summary"] == summary)
            .unwrap();
        let label = row["by"].as_str().unwrap().to_owned();
        assert_eq!(label, reader.service.display_identity().session(session));
        assert!(
            shown.value["notes"]
                .as_array()
                .unwrap()
                .iter()
                .any(|row| row["by"] == label)
        );
        let detail = reader
            .show_records(
                &work,
                &ShowInput {
                    note: Some(row["locator"].as_str().unwrap().into()),
                    ..Default::default()
                },
                at(6),
            )
            .unwrap();
        assert_eq!(detail.value["note"]["by"], label);
        let other_project = ProjectId("a different project".into());
        let identity = crate::work_service::identity::DisplayIdentity {
            project: &other_project,
            actor: "agent",
            session: &SessionId("agent".into()),
        };
        assert_ne!(label, identity.session(session));
        for receipt in [&shown, &notes, &detail] {
            assert!(!receipt.text().contains(&session.0));
            assert!(!receipt.value.to_string().contains(&session.0));
        }
        labels.push(label);
    }
    assert_ne!(labels[0], labels[1]);
    assert_eq!(
        notes.value,
        reader
            .show_records(
                &work,
                &ShowInput {
                    notes: true,
                    ..Default::default()
                },
                at(5)
            )
            .unwrap()
            .value
    );
    let canonical = reader
        .service
        .work_record_window(&work, crate::storage::WorkRecordKind::Notes, None, at(5))
        .unwrap()
        .1;
    assert!(
        canonical
            .rows
            .iter()
            .all(|row| row.actor.actor_id == "agent")
    );
    for session in sessions {
        assert!(
            canonical
                .rows
                .iter()
                .any(|row| row.actor.session_id.as_ref() == Some(&session))
        );
    }
}

#[test]
fn attribution_typed_evidence_uses_recorder_not_producer_session() {
    let (_directory, reader, _, _) = fixture();
    let work = add(&reader, "Recording versus producing", None, false, 0);
    note(&reader, &work, "Evidence body", 1);
    let source = reader.service.work_focus_for_agent(&work, at(2)).unwrap();
    let peer = SessionId("recording-peer".into());
    for kind in [
        crate::WorkEvidenceKind::Verification,
        crate::WorkEvidenceKind::Environment,
    ] {
        for (recording, producer) in [
            (Some(peer.clone()), SessionId("agent".into())),
            (Some(SessionId("agent".into())), peer.clone()),
            (None, SessionId("agent".into())),
        ] {
            // Model the distinct typed projection fields without inventing or
            // altering a canonical control capture (which currently aligns them).
            let mut view = source.clone();
            let mut evidence = view.latest_evidence_item.take().unwrap();
            evidence.evidence_kind = kind;
            evidence.display_actor_session_id = recording.clone();
            evidence.producer_session_id = Some(producer.clone());
            let rich = serde_json::to_value(&evidence).unwrap();
            assert_eq!(rich["producer_session_id"], producer.0);
            assert!(rich.get("display_actor_session_id").is_none());
            view.evidence_items = vec![evidence.clone()];
            view.latest_evidence_item = Some(evidence);
            let expected = reader
                .service
                .display_identity()
                .author("agent", recording.as_ref());
            let projected = crate::verbs::show::show_receipt_value(
                &view,
                Holder::Nobody,
                reader.service.display_identity(),
                at(2),
            );
            let value = serde_json::to_value(projected).unwrap();
            assert_eq!(value["notes"][0]["by"], expected);
            let lines = crate::verbs::show::show_lines(
                &view,
                Holder::Nobody,
                reader.service.display_identity(),
                at(2),
            );
            assert!(
                lines
                    .iter()
                    .any(|line| line.contains(&format!(" by {expected}")))
            );
            if recording.as_ref() != Some(&SessionId("agent".into())) {
                assert_ne!(value["notes"][0]["by"], "you");
            }
        }
    }
}

#[test]
fn attribution_staged_replay_keeps_labels_and_exact_payload_bytes() {
    let (_directory, reader, path, project) = fixture();
    let work = add(&reader, "Replay identity", None, false, 0);
    let peer = AgentVerbs::new(
        path.clone(),
        project.clone(),
        "agent".into(),
        SessionId("peer-producer".into()),
        None,
    );
    note(&peer, &work, "Distinct replay observation", 1);
    let query = || WorkNextQuery {
        sections: vec![WorkNextSection::Changes],
        ..Default::default()
    };
    let first = reader.service.work_next(20, query(), at(2)).unwrap();
    let store = SqliteStore::open(&path).unwrap();
    let before = store
        .staged_work_session_delivery_payload(&project, &SessionId("agent".into()))
        .unwrap()
        .unwrap();
    let first_lines = collapsed_changes(
        first.changes.as_deref().unwrap(),
        reader.service.display_identity(),
    )
    .iter()
    .map(|row| row.line.clone())
    .collect::<Vec<_>>();
    assert!(
        first_lines
            .iter()
            .any(|line| line.contains("Distinct replay observation") && line.contains("peer-"))
    );
    let replacement = AgentVerbs::new(
        path,
        project.clone(),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let preview = replacement
        .next(
            &NextInput {
                peek: true,
                ..Default::default()
            },
            at(3),
        )
        .unwrap();
    for line in &first_lines {
        assert!(preview.text().contains(line), "{}", preview.text());
    }
    assert_eq!(
        before,
        store
            .staged_work_session_delivery_payload(&project, &SessionId("agent".into()))
            .unwrap()
            .unwrap()
    );
    let confirmed = store
        .work_session_state(&project, &SessionId("agent".into()), at(2))
        .unwrap()
        .project_cursor;
    let replay = replacement
        .service
        .work_next_with_delivery_token(20, Some(confirmed), None, query(), at(3))
        .unwrap();
    let replay_lines = collapsed_changes(
        replay.changes.as_deref().unwrap(),
        replacement.service.display_identity(),
    )
    .iter()
    .map(|row| row.line.clone())
    .collect::<Vec<_>>();
    assert_eq!(first_lines, replay_lines);
    let after = store
        .staged_work_session_delivery_payload(&project, &SessionId("agent".into()))
        .unwrap()
        .unwrap();
    assert_eq!(before, after);
    assert_eq!(
        serde_json::to_value(first.changes).unwrap(),
        serde_json::to_value(replay.changes).unwrap()
    );
}

#[test]
fn attribution_release_handoff_and_errors_keep_identity_in_audit_only() {
    let (_directory, reader, path, project) = fixture();
    let work = add(&reader, "Identity boundaries", None, false, 0);
    let peer_session = SessionId(uuid::Uuid::new_v4().to_string());
    let peer_actor = "non-uuid-private-principal";
    let peer = AgentVerbs::new(
        path.clone(),
        project,
        peer_actor.into(),
        peer_session.clone(),
        None,
    );
    peer.claim(
        ClaimInput {
            work_ref: work.clone(),
            ttl_seconds: None,
            recover: None,
        },
        at(1),
    )
    .unwrap();
    note(&peer, &work, "Checkpoint before handoff", 2);
    let error = reader
        .claim(
            ClaimInput {
                work_ref: work.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(3),
        )
        .unwrap_err();
    let raw = crate::mcp::store_error_value(&error.error);
    assert_eq!(raw["error"]["details"]["holder_session_id"], peer_session.0);
    let safe = reader.project_error(&error, raw);
    assert_eq!(
        safe["error"]["details"]["holder"],
        reader.service.display_identity().session(&peer_session)
    );
    assert!(!safe.to_string().contains(&peer_session.0));
    assert!(!reader.error_message(&error).contains(&peer_session.0));
    let StoreError::WorkClaimHeld {
        expires_at: expiry, ..
    } = error.error
    else {
        panic!("expected peer claim refusal");
    };
    let expiry_text = crate::verbs::receipts::claim_expiry_text(expiry);
    let holder_text = reader.service.display_identity().session(&peer_session);
    assert_eq!(
        reader.error_guidance(&error).reminders,
        vec![format!("held by {holder_text} until {expiry_text}")]
    );
    assert!(
        reader
            .error_message(&error)
            .ends_with(&format!("until {expiry_text}"))
    );
    assert!(!reader.error_message(&error).contains(&expiry.to_string()));
    for receipt in [
        reader.show(&work, at(3)).unwrap(),
        reader.ls(&LsInput::default(), at(3)).unwrap(),
        reader
            .next(
                &NextInput {
                    peek: true,
                    ..Default::default()
                },
                at(3),
            )
            .unwrap(),
    ] {
        for text in [receipt.text(), receipt.value.to_string()] {
            assert!(!text.contains(&peer_session.0), "{text}");
            assert!(!text.contains(peer_actor), "{text}");
        }
    }
    peer.update(
        UpdateInput {
            work_ref: Some(work.clone()),
            action: UpdateAction::Release { reason: None },
        },
        at(4),
    )
    .unwrap();
    let history = reader
        .show_records(
            &work,
            &ShowInput {
                history: true,
                ..Default::default()
            },
            at(5),
        )
        .unwrap();
    assert!(!history.text().contains(peer_actor));
    assert!(!history.value.to_string().contains(&peer_session.0));
    let raw_history = reader
        .service
        .work_record_window(&work, crate::storage::WorkRecordKind::History, None, at(5))
        .unwrap()
        .1;
    assert!(
        raw_history
            .rows
            .iter()
            .any(|row| row.actor.actor_id == peer_actor
                && row.actor.session_id.as_ref() == Some(&peer_session))
    );

    reader
        .claim(
            ClaimInput {
                work_ref: work.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(6),
        )
        .unwrap();
    note(&reader, &work, "Checkpoint before offering", 7);
    let connection =
        rusqlite::Connection::open_with_flags(path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY)
            .unwrap();
    for label in [
        reader.service.display_identity().session(&peer_session),
        reader.service.display_identity().actor(peer_actor),
    ] {
        let before = crate::storage::test_database_shape_snapshot(&connection);
        let refusal = reader
            .handoff(
                HandoffInput {
                    work_ref: Some(work.clone()),
                    action: HandoffAction::Offer {
                        to: format!(" {label} "),
                        summary: None,
                        ttl_seconds: None,
                    },
                },
                at(8),
            )
            .unwrap_err();
        assert!(
            matches!(&refusal.error, StoreError::InvalidWork(reason) if reason == crate::verbs::attribution::HANDOFF_DISPLAY_TARGET_REFUSAL)
        );
        assert!(refusal.guidance().reminders[0].contains("host or coordinator"));
        assert_eq!(
            crate::storage::test_database_shape_snapshot(&connection),
            before
        );
        assert!(
            reader
                .service
                .inspect_work(&work, at(8))
                .unwrap()
                .handoffs
                .is_empty()
        );
    }
    // A real session supplied by the host remains usable; no display alias is
    // resolved, and the failed requests left no dead offer or changed claim.
    let offered = reader
        .handoff(
            HandoffInput {
                work_ref: Some(work.clone()),
                action: HandoffAction::Offer {
                    to: peer_session.0.clone(),
                    summary: None,
                    ttl_seconds: None,
                },
            },
            at(8),
        )
        .unwrap();
    assert!(!offered.text().contains(&peer_session.0));
    let raw = reader.service.inspect_work(&work, at(9)).unwrap();
    assert_eq!(raw.handoffs[0].to, peer_session);
    peer.handoff(
        HandoffInput {
            work_ref: Some(work.clone()),
            action: HandoffAction::Accept,
        },
        at(9),
    )
    .unwrap();
    assert_eq!(
        reader
            .service
            .inspect_work(&work, at(9))
            .unwrap()
            .claim
            .unwrap()
            .holder,
        peer_session
    );
}
