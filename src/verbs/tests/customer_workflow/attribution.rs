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
            let expected =
                reader
                    .service
                    .display_identity()
                    .author("agent", "agent", recording.as_ref());
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

/// Every listed refusal that concerns a work item, built around `work`.
fn refusals_naming_work(work: crate::WorkId) -> Vec<StoreError> {
    let record = crate::ObjectId::from_canonical_bytes(b"an evaluation record");
    let admission: crate::domain::AcceptanceEvaluationAdmissionCause =
        serde_json::from_value(json!({
            "kind": "eligibility",
            "mismatch": "mode_disallowed",
            "requested_mode": "same_session",
            "task_mark": null,
            "admitted_modes": ["independent_session"],
            "remedy": "request_eligible_evaluation",
        }))
        .unwrap();
    vec![
        StoreError::WorkNotFound(work),
        StoreError::WorkRevisionConflict {
            work,
            expected: 2,
            current: 3,
        },
        StoreError::WorkNotOpen(work),
        StoreError::WorkPeerDecompositionRefused { parent: work },
        StoreError::WorkPrerequisiteAlreadySatisfied(work),
        StoreError::WorkDetachRefused {
            work_id: work,
            reason: "the child has a live claim".into(),
            remedy: "release the claim first".into(),
        },
        StoreError::WorkClaimMismatch { work },
        StoreError::WorkClaimLapsed {
            work,
            expired_at: at(5),
        },
        StoreError::WorkReleaseWaiverRequired { work },
        StoreError::WorkCompletionRefused {
            work,
            reason: "a criterion is unmet".into(),
        },
        StoreError::WorkBoundVerificationRefused {
            work,
            reason: "criterion 1 requires test verification".into(),
            cause: Box::new(crate::domain::WorkBoundVerificationCause {
                criterion: 1,
                requirement: crate::domain::VerificationRequirement {
                    check_kind: crate::domain::VerificationKind::Test,
                    check_fingerprint: None,
                },
                mismatch: crate::domain::VerificationEvidenceMismatch::StaleSourceRevision,
                verification: record.clone(),
                satisfied_by: record.clone(),
                producer_observation: record.clone(),
                result: crate::domain::VerificationResult::Passed,
                remedy: crate::domain::BoundVerificationRemedy::RunCurrentCheck,
                stale_source: None,
            }),
        },
        StoreError::WorkCompletionRecoveryRequired {
            work,
            cause: crate::WorkCompletionRecoveryCause::AcceptanceEvaluationStale {
                reason: crate::AcceptanceStaleReason::Mutation,
            },
            context: Box::default(),
        },
        StoreError::AcceptanceCriteriaRequired { work },
        StoreError::AcceptanceEvaluationRefused {
            work,
            reason: "no criterion verdicts".into(),
        },
        StoreError::AcceptanceEvaluationAdmissionRefused {
            work,
            reason: "mode same_session is not allowed".into(),
            cause: Box::new(admission),
        },
        StoreError::AcceptanceEvaluationCarriedFailure {
            work,
            refusal: crate::storage::CarriedFailureRefusal::Unacknowledged,
            failed: Some(record.clone()),
            reason: "the carried failure is not named".into(),
        },
        StoreError::AcceptanceEvaluationBasisMoved {
            work,
            moved: crate::storage::EvaluationBasisMove::CheckRecorded,
            reason: "a check was recorded after the basis".into(),
            observation: None,
        },
        StoreError::OpenWorkObligations {
            work,
            obligations: Vec::new(),
            omitted_count: 0,
        },
    ]
}

// The agent rendering of every listed refusal names its work item by short
// reference, on CLI text and in the shared JSON/MCP envelope, while the raw
// host/core envelope, the code and scoped record ids stay as they were.
#[test]
fn listed_refusals_name_their_work_by_short_reference_on_agent_surfaces() {
    let (_directory, verbs, _, _) = fixture();
    let work = crate::WorkId::new();
    let raw = work.0.to_string();
    let short = crate::verbs::short_ref_for_work_id(work);
    let refusals = refusals_naming_work(work);
    assert_eq!(
        refusals.len(),
        18,
        "the listed variants other than the ambiguous reference"
    );
    for error in refusals {
        let core = crate::mcp::store_error_value(&error);
        let label = format!("{:?}", core["error"]["code"]);
        let verb_error = VerbError::from(error);
        let text = verbs.error_message(&verb_error);
        assert!(!text.contains(&raw), "{label}: {text}");
        let projected = verbs.project_error(&verb_error, core.clone());
        let projected_text = projected.to_string();
        assert!(!projected_text.contains(&raw), "{label}: {projected_text}");
        assert_eq!(projected["error"]["code"], core["error"]["code"], "{label}");
        assert_eq!(projected["error"]["message"], json!(text), "{label}");
        if core["error"]["details"].get("work_id").is_some() {
            // The raw envelope keeps the id; the agent one names the ref.
            assert_eq!(core["error"]["details"]["work_id"], json!(raw), "{label}");
            assert_eq!(
                projected["error"]["details"]["work_ref"],
                json!(short),
                "{label}"
            );
        }
        if core["error"]["message"].as_str().unwrap().contains(&raw) {
            assert!(text.contains(&short), "{label}: {text}");
        }
        // Scoped record ids are kept as the raw envelope carries them.
        for key in ["failed_evaluation", "cause"] {
            if let Some(value) = core["error"]["details"].get(key) {
                assert_eq!(&projected["error"]["details"][key], value, "{label} {key}");
            }
        }
        let guidance = verbs.error_guidance(&verb_error);
        for line in guidance.reminders.iter().chain(&guidance.next) {
            assert!(!line.contains(&raw), "{label}: {line}");
        }
    }
}

// An ambiguous short reference keeps the full-work-id fallback: the agent
// message lists the candidates by full id, never as a debug dump, and the
// offered commands show each candidate by its full id. The caller's
// reference is not rewritten.
#[test]
fn an_ambiguous_reference_keeps_the_full_id_fallback_on_agent_surfaces() {
    let (_directory, verbs, _, _) = fixture();
    let first = crate::WorkId::new();
    let second = crate::WorkId::new();
    let candidate = |work_id: crate::WorkId, title: &str| crate::WorkReferenceCandidate {
        work_id,
        short_ref: "w-collision".into(),
        title: title.into(),
        lifecycle: WorkLifecycle::Open,
    };
    let error = VerbError::at(
        StoreError::WorkReferenceAmbiguous {
            reference: "w-collision".into(),
            candidates: vec![candidate(first, "First"), candidate(second, "Second")],
            more: 1,
        },
        "w-collision",
    );
    let text = verbs.error_message(&error);
    assert_eq!(
        text,
        format!(
            "work reference \"w-collision\" is ambiguous; use a full work id for one of {}, {}; 1 additional candidates omitted",
            first.0, second.0
        )
    );
    assert!(!text.contains("WorkReferenceCandidate"), "{text}");
    let core = crate::mcp::store_error_value(&error.error);
    let projected = verbs.project_error(&error, core.clone());
    assert_eq!(projected["error"]["code"], core["error"]["code"]);
    assert_eq!(projected["error"]["message"], json!(text));
    assert_eq!(projected["error"]["details"], core["error"]["details"]);
    assert_eq!(
        verbs.error_guidance(&error).next,
        vec![
            format!("engram work show {}", first.0),
            format!("engram work show {}", second.0),
        ]
    );
}

// The claim-held refusal keeps its existing agent projection.
#[test]
fn the_claim_held_refusal_keeps_its_agent_projection() {
    let (_directory, verbs, _, _) = fixture();
    let work = crate::WorkId::new();
    let error = VerbError::from(StoreError::WorkClaimHeld {
        work,
        holder: "holder-session".into(),
        expires_at: 1_000,
    });
    let core = crate::mcp::store_error_value(&error.error);
    let projected = verbs.project_error(&error, core);
    let details = &projected["error"]["details"];
    assert!(details.get("work_id").is_none());
    assert!(details.get("holder_session_id").is_none());
    assert_eq!(
        details["work_ref"],
        json!(crate::verbs::short_ref_for_work_id(work))
    );
    assert!(!projected.to_string().contains("holder-session"));
}

// Caller-supplied text is never rewritten, even when it quotes the item's
// own raw id: only the refusal's structural rendering of its item becomes the
// short reference, in the message and in a reminder that repeats it; a
// reminder carrying a criterion or reason keeps it byte for byte.
#[test]
fn caller_text_quoting_the_raw_id_is_never_rewritten_on_agent_surfaces() {
    let (_directory, verbs, _, _) = fixture();
    let work = crate::WorkId::new();
    let quoted = format!("inspect {work:?} first");
    let short = crate::verbs::short_ref_for_work_id(work);
    for error in [
        StoreError::WorkCompletionRecoveryRequired {
            work,
            cause: crate::WorkCompletionRecoveryCause::MissingAcceptance {
                criterion: quoted.clone(),
            },
            context: Box::default(),
        },
        StoreError::WorkCompletionRefused {
            work,
            reason: quoted.clone(),
        },
        StoreError::AcceptanceEvaluationRefused {
            work,
            reason: quoted.clone(),
        },
        StoreError::WorkDetachRefused {
            work_id: work,
            reason: quoted.clone(),
            remedy: "release the claim first".into(),
        },
    ] {
        let raw_message = error.to_string();
        let verb_error = VerbError::from(error);
        let label = raw_message.clone();
        let message = verbs.error_message(&verb_error);
        // The caller's text survives whole in the agent message.
        assert!(message.contains(&quoted), "{label}: {message}");
        if raw_message.starts_with("detach refused") {
            // A message that names no item is not touched at all.
            assert_eq!(message, raw_message);
        } else {
            assert!(message.contains(&short), "{label}: {message}");
            assert_eq!(
                message.matches(&format!("{work:?}")).count(),
                raw_message.matches(&format!("{work:?}")).count() - 1,
                "{label}: only the structural rendering is replaced"
            );
        }
        let projected = verbs.project_error(
            &verb_error,
            crate::mcp::store_error_value(&verb_error.error),
        );
        assert_eq!(projected["error"]["message"], json!(message), "{label}");
        let raw_guidance = verb_error.guidance();
        let guidance = verbs.error_guidance(&verb_error);
        assert_eq!(guidance.next, raw_guidance.next, "{label}");
        for (agent, raw) in guidance.reminders.iter().zip(&raw_guidance.reminders) {
            if *raw == raw_message {
                assert_eq!(agent, &message, "{label}");
            } else {
                assert_eq!(agent, raw, "{label}: a reminder with caller text is kept");
            }
        }
    }
}

// Two listed refusals carry no details object in the core envelope; their
// agent projection keeps it absent and names the item in the message alone.
#[test]
fn refusals_without_details_name_their_work_in_the_message_alone() {
    let (_directory, verbs, _, _) = fixture();
    let work = crate::WorkId::new();
    let short = crate::verbs::short_ref_for_work_id(work);
    for error in [
        StoreError::AcceptanceEvaluationRefused {
            work,
            reason: "no criterion verdicts".into(),
        },
        StoreError::OpenWorkObligations {
            work,
            obligations: Vec::new(),
            omitted_count: 0,
        },
    ] {
        let core = crate::mcp::store_error_value(&error);
        assert_eq!(core["error"]["details"], Value::Null, "{core}");
        let verb_error = VerbError::from(error);
        let projected = verbs.project_error(&verb_error, core);
        assert_eq!(projected["error"]["details"], Value::Null);
        assert!(
            projected["error"]["message"]
                .as_str()
                .unwrap()
                .contains(&short),
            "{projected}"
        );
    }
}

// A record of this session by this actor but made as another kind of actor,
// a host operator say, is not labelled "you" in show's notes or history; it
// is labelled by its actor alone. The session's own records, which the agent
// words make, keep "you".
#[test]
fn attribution_another_actor_kind_on_this_session_is_not_you() {
    let (_directory, reader, path, project) = fixture();
    let work = add(&reader, "Actor kind attribution", None, false, 0);
    reader
        .claim(
            ClaimInput {
                work_ref: work.clone(),
                ttl_seconds: Some(3600),
                recover: None,
            },
            at(1),
        )
        .expect("claim");
    note(&reader, &work, "Agent note", 2);
    {
        let mut store = SqliteStore::open(&path).expect("store");
        let item = store.resolve_work_ref(&project, &work).expect("item");
        let claim = store
            .current_work_claim(item.work_id)
            .expect("claim read")
            .expect("live claim");
        store
            .record_work_note(
                &crate::domain::RecordWorkNoteRequest {
                    // A status note, so the current status reads it too.
                    status: true,
                    work_id: item.work_id,
                    run_id: claim.run_id,
                    expected_work_revision: item.revision,
                    holder: claim.holder.clone(),
                    claim_id: claim.claim_id,
                    claim_fence: claim.fence,
                    summary: "Operator note".into(),
                    refs: Vec::new(),
                    actor: crate::ActorContext {
                        actor_id: "agent".into(),
                        actor_kind: "host_operator".into(),
                        assurance: crate::domain::AssuranceLevel::Asserted,
                        run_id: None,
                        session_id: Some(SessionId("agent".into())),
                        source_tool: None,
                        source_skill: None,
                        provenance_chain: Vec::new(),
                        reason: "record as another kind of actor".into(),
                    },
                    idempotency_key: "operator-note".into(),
                    recorded_at: at(3),
                },
                &crate::memory::DevelopmentNoopRedactor,
            )
            .expect("operator note");
    }
    let actor_label = reader.service.display_identity().actor("agent");
    assert!(actor_label.starts_with("peer-actor-"), "{actor_label}");
    let by_summary = |rows: &Value, summary: &str| -> String {
        rows.as_array()
            .expect("rows")
            .iter()
            .find(|row| {
                row["summary"]
                    .as_str()
                    .is_some_and(|text| text.contains(summary))
            })
            .and_then(|row| row["by"].as_str())
            .unwrap_or_else(|| panic!("no row for {summary}: {rows}"))
            .to_owned()
    };
    let notes = reader.show_with_notes(&work, true, at(4)).expect("notes");
    assert_eq!(by_summary(&notes.value["notes"], "Agent note"), "you");
    let operator = by_summary(&notes.value["notes"], "Operator note");
    assert_ne!(operator, "you");
    assert!(operator.starts_with(&actor_label), "{operator}");
    let history = reader
        .show_records(
            &work,
            &crate::verbs::ShowInput {
                history: true,
                ..crate::verbs::ShowInput::default()
            },
            at(4),
        )
        .expect("history");
    let rows = &history.value["history"]["items"];
    // The history rows of the operator note are the ones recorded at its time.
    let operator_rows: Vec<&Value> = rows
        .as_array()
        .expect("history rows")
        .iter()
        .filter(|row| row["created_at"] == json!(at(3)))
        .collect();
    assert!(
        !operator_rows.is_empty(),
        "no operator history rows: {rows}"
    );
    for row in operator_rows {
        assert_eq!(row["by"], json!(actor_label), "{row}");
    }
    assert!(
        rows.as_array()
            .expect("history rows")
            .iter()
            .any(|row| row["by"] == json!("you")),
        "the session's own history keeps you: {rows}"
    );
    // Plain show: its latest notes and its current status, which the
    // operator's status note sets, name the operator by its actor.
    let shown = reader.show(&work, at(4)).expect("show");
    assert_eq!(
        by_summary(&shown.value["notes"], "Operator note"),
        actor_label,
        "{}",
        shown.value
    );
    assert_eq!(
        shown.value["current_status"]["by"],
        json!(actor_label),
        "{}",
        shown.value
    );
    assert!(
        !shown.text().contains("by you: \"Operator note"),
        "{}",
        shown.text()
    );
    // next: the held row's latest own note is the operator's, which is not
    // this session's own as an agent, so it is not marked as yours.
    let next = reader
        .next(
            &NextInput {
                peek: true,
                ..NextInput::default()
            },
            at(4),
        )
        .expect("next");
    let rendered = next.value.to_string();
    assert!(rendered.contains("Operator note"), "{rendered}");
    assert!(!rendered.contains("\"note_by\":\"you\""), "{rendered}");
    assert!(
        !next.text().contains("[note session you]"),
        "{}",
        next.text()
    );
    // Released, the item is one this session took part in: next's
    // participated row carries the session's latest note, the operator's,
    // and does not mark it as yours.
    reader
        .update(
            UpdateInput {
                work_ref: Some(work.clone()),
                action: UpdateAction::Release {
                    reason: Some("hand back".into()),
                },
            },
            at(5),
        )
        .expect("release");
    let claimless = reader
        .next(
            &NextInput {
                peek: true,
                ..NextInput::default()
            },
            at(6),
        )
        .expect("claimless next");
    let participated = claimless.value["participated"].to_string();
    assert!(
        participated.contains("Operator note"),
        "{}",
        claimless.value
    );
    assert!(
        !participated.contains("\"note_by\":\"you\""),
        "{participated}"
    );
    assert!(
        !claimless.text().contains("[note session you]"),
        "{}",
        claimless.text()
    );
}
