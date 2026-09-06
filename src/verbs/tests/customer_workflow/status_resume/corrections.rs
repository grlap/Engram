use super::*;

#[test]
fn status_correction_verbose_guidance_counts_toward_budget() {
    let (_directory, owner, path, project) = fixture();
    let peer = AgentVerbs::new(
        path.clone(),
        project.clone(),
        "peer".into(),
        SessionId("peer".into()),
        None,
    );
    let reader = AgentVerbs::new(
        path,
        project,
        "agent".into(),
        SessionId("replacement".into()),
        None,
    );
    let references = (0..3)
        .map(|index| assigned(&owner, &format!("Duty {index}"), "agent", index))
        .collect::<Vec<_>>();
    let capture = |body: &str| {
        for reference in &references {
            capture_status(&owner, reference, body, 4);
            capture_status(&peer, reference, body, 5);
            note(&owner, reference, "Ordinary progress", 6);
        }
        // Consume exact delivery separately; only advisory fitting is tested.
        for _ in 0..32 {
            let page = reader
                .service
                .work_next(
                    1,
                    crate::work_service::WorkNextQuery {
                        sections: vec![crate::work_service::WorkNextSection::Changes],
                        ..crate::work_service::WorkNextQuery::default()
                    },
                    at(7),
                )
                .unwrap();
            if page.changes.as_ref().is_none_or(Vec::is_empty) {
                return;
            }
        }
        panic!("fixture must drain its finite delivery stream");
    };
    let input = NextInput {
        verbose: true,
        context_generation: Some(String::new()),
        ..NextInput::default()
    };
    let pretty_len = |value: &serde_json::Value| serde_json::to_vec_pretty(value).unwrap().len();
    let without_guidance = |mut value: serde_json::Value| {
        let object = value.as_object_mut().unwrap();
        object.remove("reminders");
        object.remove("next");
        value
    };
    capture(&"\u{1}".repeat(129));
    let template = reader.next(&input, at(8)).unwrap();
    // Choose source length from the actual wire shape, leaving room for a
    // bounded context-generation suffix to place it exactly at the boundary.
    let length = (129..=768)
        .find(|length| {
            let mut value = without_guidance(template.value.clone());
            for row in value["assigned"].as_array_mut().unwrap() {
                for key in ["current_status", "status_observation"] {
                    row[key]["body_or_first_line"] = json!("\u{1}".repeat(*length));
                }
            }
            (MAX_AGENT_WORK_RESPONSE_BYTES - 200..MAX_AGENT_WORK_RESPONSE_BYTES - 150)
                .contains(&pretty_len(&value))
        })
        .expect("a reducible status candidate near the protocol boundary");
    let body = "\u{1}".repeat(length);
    capture(&body);
    let candidate = reader.next(&input, at(8)).unwrap();
    assert!(candidate.value["focus"].is_null());
    assert_eq!(candidate.value["assigned"].as_array().unwrap().len(), 3);
    for row in candidate.value["assigned"].as_array().unwrap() {
        for key in ["current_status", "status_observation"] {
            assert_eq!(row[key]["body_or_first_line"], body);
            assert_eq!(row[key]["complete"], true);
        }
    }
    let unassembled = without_guidance(candidate.value.clone());
    let padding = MAX_AGENT_WORK_RESPONSE_BYTES - pretty_len(&unassembled) - 1;
    assert!(padding <= 256);
    let generation = "x".repeat(padding);
    let mut expected = candidate.value.clone();
    expected["context_generation"] = json!(generation);
    assert!(pretty_len(&without_guidance(expected.clone())) < MAX_AGENT_WORK_RESPONSE_BYTES);
    assert!(pretty_len(&expected) > MAX_AGENT_WORK_RESPONSE_BYTES);
    let resumed = reader
        .next(
            &NextInput {
                context_generation: Some(generation),
                ..input
            },
            at(8),
        )
        .unwrap();
    assert!(
        pretty_len(&resumed.value) < MAX_AGENT_WORK_RESPONSE_BYTES,
        "assembled receipt is {} bytes; guidance must count before accepting the fit",
        pretty_len(&resumed.value)
    );
    assert!(resumed.text().len() < MAX_AGENT_WORK_RESPONSE_BYTES);
    assert_eq!(resumed.value["assigned"].as_array().unwrap().len(), 3);
    assert_eq!(resumed.next, candidate.next);
    assert_eq!(resumed.reminders, candidate.reminders);
    assert!(
        resumed.value["assigned"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| {
                row["status_observation"]["complete"] == false
                    && row["status_observation"]["locator"].is_string()
            })
    );
    assert!(
        resumed
            .text()
            .contains("status body omitted; read engram work show")
    );
}

#[test]
fn status_correction_late_status_is_advisory_outside_the_seal() {
    let (_directory, verbs, path, project) = fixture();
    let reference = assigned(&verbs, "Finish coordination", "agent", 0);
    verbs
        .claim(
            ClaimInput {
                work_ref: reference.clone(),
                ttl_seconds: Some(300),
                recover: None,
            },
            at(1),
        )
        .unwrap();
    let done = verbs
        .done(
            DoneInput {
                work_ref: Some(reference.clone()),
                summary: Some("Delivered".into()),
                note: None,
            },
            at(2),
        )
        .unwrap();
    assert_eq!(done.value["work"]["lifecycle"], "completed");
    let store = SqliteStore::open(&path).unwrap();
    let item = store.resolve_work_ref(&project, &reference).unwrap();
    let connection = rusqlite::Connection::open(path).unwrap();
    let seal = || {
        connection
            .query_row(
                "SELECT seal_hash, seal_json FROM work_completion_seals WHERE work_id = ?1",
                [item.work_id.0.to_string()],
                |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)),
            )
            .unwrap()
    };
    let before = seal();
    capture_status(
        &verbs,
        &reference,
        "Late advisory: awaiting downstream publication",
        3,
    );
    let shown = verbs.show(&reference, at(4)).unwrap();
    assert_eq!(
        shown.value["current_status"]["body_or_first_line"],
        "Late advisory: awaiting downstream publication"
    );
    assert_eq!(seal(), before);
    assert!(store.live_work_claims(&project, at(4)).unwrap().is_empty());
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn status_correction_restored_history_decoded_once() {
    let (directory, verbs, path, project) = fixture();
    let reference = assigned(&verbs, "Inherited", "agent", 0);
    capture_status(&verbs, &reference, "Inherited commitment", 1);
    let external = "planner:\u{1b}[2J\u{202e}ref\r\nnext:";
    verbs
        .update(
            serde_json::from_value(
                json!({"work_ref": reference, "action": {"action":"revise", "external":external}}),
            )
            .unwrap(),
            at(2),
        )
        .unwrap();
    let mut source = SqliteStore::open(path).unwrap();
    let item = source.resolve_work_ref(&project, &reference).unwrap();
    let saved = source
        .save_work_graph_snapshot(
            &project,
            &item.created_by,
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(3),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let restored_path = directory.path().join("status-restored.db");
    let mut destination = SqliteStore::open(&restored_path).unwrap();
    destination
        .load_work_graph_snapshot(
            &project,
            &item.created_by,
            &serde_json::to_vec(&saved.document).unwrap(),
            false,
            at(4),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let restored = destination.resolve_work_ref(&project, &reference).unwrap();
    crate::canonical::reset_canonical_decode_count();
    let (current, peer) = destination.current_status_notes(&restored, at(5)).unwrap();
    assert_eq!(
        crate::canonical::canonical_decode_count(),
        1,
        "one verified history load shared by owner and peer"
    );
    assert_eq!(current.unwrap().note.summary, "Inherited commitment");
    assert!(peer.is_none());
    let reader = AgentVerbs::new(
        restored_path,
        project,
        "agent".into(),
        SessionId("restored".into()),
        None,
    );
    let shown = reader.show(&reference, at(5)).unwrap();
    assert_eq!(shown.value["external_ref"], external);
    assert!(!shown.text().contains('\u{1b}'));
    assert!(!shown.text().contains('\u{202e}'));
    assert!(!shown.text().contains('\r'));
    assert_eq!(
        shown
            .text()
            .split('\n')
            .filter(|line| *line == "next:")
            .count(),
        1
    );
    assert!(destination.verify_all().unwrap().is_healthy());
    let mut changed = saved.document.clone();
    for record in &mut changed.body.records {
        if let crate::WorkGraphSnapshotRecordPayload::Native { history } = &mut record.payload {
            let mut not_note = history.notes[0].clone();
            for kind in [
                crate::WorkEvidenceKind::Verification,
                crate::WorkEvidenceKind::Environment,
            ] {
                not_note.evidence_kind = kind;
                not_note.summary = "Not a captured status note".into();
                history.notes.push(not_note.clone());
            }
        }
    }
    changed.manifest.body_sha256 = crate::CanonicalObject::freeze(&changed.body)
        .unwrap()
        .hash()
        .clone();
    let mut other = SqliteStore::open_in_memory().unwrap();
    other
        .load_work_graph_snapshot(
            &changed.body.summary.project_id,
            &item.created_by,
            &serde_json::to_vec(&changed).unwrap(),
            false,
            at(6),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let item = other.get_work_item(item.work_id).unwrap();
    let (current, peer) = other.current_status_notes(&item, at(7)).unwrap();
    assert_eq!(current.unwrap().note.summary, "Inherited commitment");
    assert!(peer.is_none());
}

#[test]
fn status_correction_escape_heavy_show_and_next_fit() {
    let (_directory, owner, path, project) = fixture();
    let peer = AgentVerbs::new(path, project, "peer".into(), SessionId("peer".into()), None);
    let body = "\u{1}".repeat(768);
    let mut references = Vec::new();
    for index in 0..3 {
        let receipt = owner
            .add(
                AddInput {
                    title: format!("Duty {index}"),
                    outcome: Some("x".repeat(4096)),
                    assignee: Some("agent".into()),
                    ..AddInput::default()
                },
                at(index * 3),
            )
            .unwrap();
        let reference = receipt.value["work"]["short_ref"]
            .as_str()
            .unwrap()
            .to_owned();
        capture_status(&owner, &reference, &body, index * 3 + 1);
        capture_status(&peer, &reference, &body, index * 3 + 2);
        references.push(reference);
    }
    for reference in &references {
        for options in [
            ShowInput::default(),
            ShowInput {
                notes: true,
                ..ShowInput::default()
            },
            ShowInput {
                history: true,
                ..ShowInput::default()
            },
        ] {
            let shown = owner.show_records(reference, &options, at(10)).unwrap();
            assert!(shown.text().len() < MAX_AGENT_WORK_RESPONSE_BYTES);
            assert!(
                serde_json::to_vec_pretty(&shown.value).unwrap().len()
                    < MAX_AGENT_WORK_RESPONSE_BYTES
            );
            assert_eq!(shown.value["status"]["work"]["outcome"], "x".repeat(4096));
            assert!(
                ["current_status", "status_observation"]
                    .iter()
                    .any(|key| shown.value[*key]["complete"] == false)
            );
            for key in ["current_status", "status_observation"] {
                let locator = shown.value[key]["locator"].as_str().unwrap();
                let detail = owner
                    .show_records(
                        reference,
                        &ShowInput {
                            note: Some(locator.into()),
                            ..ShowInput::default()
                        },
                        at(11),
                    )
                    .unwrap();
                assert_eq!(detail.value["note"]["summary"], body);
            }
            assert!(
                shown
                    .text()
                    .contains("status body omitted; read engram work show")
            );
        }
    }
    // Ordinary note previews are separate from the immutable current status.
    for reference in &references {
        note(&owner, reference, "Ordinary progress", 12);
    }
    let held = add(&owner, "Held duty", None, false, 13);
    owner
        .claim(
            ClaimInput {
                work_ref: held.clone(),
                ttl_seconds: Some(300),
                recover: None,
            },
            at(14),
        )
        .unwrap();
    capture_status(&owner, &held, &body, 15);
    capture_status(&peer, &held, &body, 16);
    note(&owner, &held, "Ordinary held progress", 17);
    // Consume the exact change stream separately. Verbose next deliberately
    // retains that page/cursor; this fixture isolates advisory status fitting.
    let mut drained = false;
    for _ in 0..32 {
        let page = owner
            .service
            .work_next(
                1,
                crate::work_service::WorkNextQuery {
                    sections: vec![crate::work_service::WorkNextSection::Changes],
                    ..crate::work_service::WorkNextQuery::default()
                },
                at(18),
            )
            .unwrap();
        if page.changes.as_ref().is_none_or(Vec::is_empty) {
            drained = true;
            break;
        }
    }
    assert!(
        drained,
        "fixture consumes the bounded exact delivery stream"
    );
    for verbose in [false, true] {
        let resumed = owner
            .next(
                &NextInput {
                    verbose,
                    ..NextInput::default()
                },
                at(18),
            )
            .unwrap();
        assert!(resumed.text().len() < MAX_AGENT_WORK_RESPONSE_BYTES);
        assert!(
            serde_json::to_vec_pretty(&resumed.value).unwrap().len()
                < MAX_AGENT_WORK_RESPONSE_BYTES,
            "verbose={verbose} bytes={}",
            serde_json::to_vec_pretty(&resumed.value).unwrap().len(),
        );
        assert_eq!(resumed.value["assigned"].as_array().unwrap().len(), 3);
        assert_eq!(resumed.value["held"].as_array().unwrap().len(), 1);
        assert!(
            resumed
                .text()
                .contains("status body omitted; read engram work show")
        );
    }
}

#[test]
fn status_correction_exact_identity_and_non_holder_disclosure() {
    let (_directory, owner, path, project) = fixture();
    let reference = assigned(&owner, "Coordination", "agent", 0);
    let peer = AgentVerbs::new(
        path,
        project,
        "Agent".into(),
        SessionId("peer".into()),
        None,
    );
    capture_status(&owner, &reference, "Owner commitment", 1);
    capture_status(&peer, &reference, "Distinct principal observation", 2);
    let shown = peer
        .show_records(
            &reference,
            &ShowInput {
                notes: true,
                ..ShowInput::default()
            },
            at(3),
        )
        .unwrap();
    assert_eq!(
        shown.value["current_status"]["body_or_first_line"],
        "Owner commitment"
    );
    assert_eq!(
        shown.value["status_observation"]["body_or_first_line"],
        "Distinct principal observation"
    );
    for row in shown.value["notes"].as_array().unwrap() {
        assert_eq!(row["non_holder"], true);
        let line = shown
            .text()
            .split('\n')
            .find(|line| line.contains(row["locator"].as_str().unwrap()))
            .unwrap()
            .to_owned();
        if row["status_owner"] == true {
            assert!(line.contains("(non-holder)"));
        } else {
            assert!(line.contains("peer status observation, no commitment"));
        }
    }
    let discovery = peer.next(&NextInput::default(), at(4)).unwrap();
    assert_eq!(discovery.value["assigned"][0]["ref"], reference);
    assert_eq!(
        discovery.value["assigned"][0]["current_status"]["body_or_first_line"],
        "Owner commitment"
    );
}
