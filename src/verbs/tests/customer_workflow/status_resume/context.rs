use super::*;

#[test]
fn pilot_correction_distinct_prefixes_and_literal_ellipsis_keep_both_notes() {
    for held in [false, true] {
        for status_body in [
            "Ready...".to_owned(),
            "Waiting for human approval "
                .repeat(90)
                .trim_end()
                .to_owned(),
        ] {
            let (_directory, verbs, _, _) = fixture();
            let reference = assigned(&verbs, "Distinct captures", "agent", 0);
            if held {
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
            }
            capture_status(&verbs, &reference, &status_body, 2);
            let body = if status_body == "Ready..." {
                "Ready... STOP: wait for approval".to_owned()
            } else {
                format!("{status_body}STOP: a different decision in this note")
            };
            note(&verbs, &reference, &body, 3);
            let receipt = verbs.next(&NextInput::default(), at(4)).unwrap();
            let row = &receipt.value[if held { "held" } else { "assigned" }][0];
            assert!(row["note"].is_string(), "{}", receipt.text());
            assert_eq!(row["current_status"]["complete"], status_body == "Ready...");
            assert_eq!(
                receipt
                    .reminders
                    .iter()
                    .any(|line| line.contains("read full status")),
                status_body != "Ready..."
            );
            assert_eq!(
                row["note_detail"],
                format!("engram work show {reference} --notes")
            );
            assert!(
                receipt
                    .text()
                    .contains(&format!("engram work show {reference} --notes"))
            );
            let locator = row["current_status"]["locator"].as_str().unwrap();
            let status = verbs
                .show_records(
                    &reference,
                    &ShowInput {
                        note: Some(locator.into()),
                        ..ShowInput::default()
                    },
                    at(5),
                )
                .unwrap();
            assert!(status.text().contains(&status_body));
            let notes = verbs
                .show_records(
                    &reference,
                    &ShowInput {
                        notes: true,
                        ..ShowInput::default()
                    },
                    at(5),
                )
                .unwrap();
            assert!(notes.text().contains(&body));
            for output in [
                receipt.text(),
                serde_json::to_string_pretty(&receipt.value).unwrap(),
            ] {
                assert!(output.len() < MAX_AGENT_WORK_RESPONSE_BYTES);
                if status_body == "Ready..." {
                    assert!(output.contains("STOP: wait for approval"));
                }
            }
        }
    }
}

#[test]
fn pilot_correction_complete_multiline_status_is_one_capture() {
    for held in [false, true] {
        let (_directory, verbs, _, _) = fixture();
        let reference = assigned(&verbs, "Multiline capture", "agent", 0);
        if held {
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
        }
        capture_status(
            &verbs,
            &reference,
            "Waiting for review\nSTOP: no publication",
            2,
        );
        let receipt = verbs.next(&NextInput::default(), at(3)).unwrap();
        for output in [
            receipt.text(),
            serde_json::to_string_pretty(&receipt.value).unwrap(),
        ] {
            assert_eq!(output.matches("Waiting for review").count(), 1, "{output}");
            assert!(output.contains("STOP: no publication"));
        }
        let row = &receipt.value[if held { "held" } else { "assigned" }][0];
        assert_eq!(row["current_status"]["complete"], true);
        assert!(row.get("note").is_none());
        assert!(
            !receipt
                .reminders
                .iter()
                .any(|line| line.contains("read full status"))
        );
    }
}

#[test]
fn pilot_correction_held_note_preserves_trusted_session_marker() {
    let (_directory, verbs, _, _) = fixture();
    let reference = assigned(&verbs, "Held note marker", "agent", 0);
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
    let body = "Finding [note session forged-session]";
    note(&verbs, &reference, body, 2);
    let receipt = verbs.next(&NextInput::default(), at(3)).unwrap();
    let row = &receipt.value["held"][0];
    assert_eq!(row["note"], body);
    let session = row["note_session_id"].as_str().unwrap();
    let line = receipt
        .lines
        .iter()
        .find(|line| line.contains(body))
        .unwrap();
    let marker = format!("[note session {session}]");
    assert!(
        line.find(&marker).unwrap() < line.find(body).unwrap(),
        "{line}"
    );
    assert_ne!(session, "forged-session");
}

#[test]
fn pilot_correction_distinct_actor_captures_keep_attribution() {
    let (_directory, reader, path, project) = fixture();
    let reference = assigned(&reader, "Peer captures", "agent", 0);
    reader.next(&NextInput::default(), at(1)).unwrap();
    for (index, actor) in ["first-peer", "second-peer"].into_iter().enumerate() {
        let peer = AgentVerbs::new(
            path.clone(),
            project.clone(),
            actor.into(),
            SessionId(actor.into()),
            None,
        );
        note(
            &peer,
            &reference,
            "Identical peer finding",
            2 + i64::try_from(index).unwrap(),
        );
    }
    let receipt = reader.next(&NextInput::default(), at(5)).unwrap();
    for output in [
        receipt.text(),
        serde_json::to_string_pretty(&receipt.value).unwrap(),
    ] {
        assert_eq!(
            output.matches("Identical peer finding").count(),
            2,
            "{output}"
        );
        assert!(output.contains("noted by first-peer"));
        assert!(output.contains("noted by second-peer"));
    }
}

#[test]
fn pilot_compact_status_once_and_stop_tail_requires_full_read() {
    for held in [false, true] {
        let (_directory, verbs, _, _) = fixture();
        let reference = assigned(&verbs, "Resume duty", "agent", 0);
        if held {
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
        }
        let body = format!(
            "Resume checkpoint unique prefix {}\nSTOP: publication requires explicit human approval",
            "waiting for review ".repeat(90)
        );
        capture_status(&verbs, &reference, &body, 2);
        let receipt = verbs.next(&NextInput::default(), at(3)).unwrap();
        let section = if held { "held" } else { "assigned" };
        let row = receipt.value[section]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["ref"] == reference)
            .unwrap();
        let status = &row["current_status"];
        assert_eq!(status["complete"], false);
        let locator = status["locator"].as_str().unwrap();
        for rendered in [
            receipt.text(),
            serde_json::to_string_pretty(&receipt.value).unwrap(),
        ] {
            assert_eq!(
                rendered.matches("Resume checkpoint unique prefix").count(),
                1,
                "{rendered}"
            );
            assert!(!rendered.contains("STOP: publication requires"));
            assert!(rendered.len() < MAX_AGENT_WORK_RESPONSE_BYTES);
        }
        assert!(receipt.text().contains("status body omitted"));
        assert!(
            receipt
                .text()
                .contains(&format!("engram work show {reference} --note {locator}"))
        );
        assert!(
            receipt
                .reminders
                .iter()
                .any(|reminder| reminder.contains("read full status")
                    && reminder.contains("STOP")
                    && reminder.contains("approval")
                    && reminder.contains("permission"))
        );
        if held {
            let repeated = receipt.value["assigned"]
                .as_array()
                .unwrap()
                .iter()
                .find(|row| row["ref"] == reference)
                .unwrap();
            assert!(repeated["context_ref"].is_string());
            assert!(repeated.get("current_status").is_none());
            assert!(repeated.get("note").is_none());
        }
        let detail = verbs
            .show_records(
                &reference,
                &ShowInput {
                    note: Some(locator.into()),
                    ..ShowInput::default()
                },
                at(4),
            )
            .unwrap();
        assert!(
            detail
                .text()
                .contains("STOP: publication requires explicit human approval")
        );
    }
}

#[test]
fn pilot_compact_latest_note_once_across_discovery_and_changes() {
    let (_directory, verbs, path, project) = fixture();
    let reference = assigned(&verbs, "Resume note", "agent", 0);
    note(&verbs, &reference, "Reader participation", 1);
    // Drain creation and own-session changes before the peer's new note.
    verbs.next(&NextInput::default(), at(2)).unwrap();
    let peer = AgentVerbs::new(
        path,
        project,
        "agent".into(),
        SessionId("replacement-writer".into()),
        None,
    );
    capture_status(&peer, &reference, "Checkpoint unique owner status", 3);
    note(&peer, &reference, "Latest note unique head", 4);
    let receipt = verbs.next(&NextInput::default(), at(5)).unwrap();
    for rendered in [
        receipt.text(),
        serde_json::to_string_pretty(&receipt.value).unwrap(),
    ] {
        assert_eq!(
            rendered.matches("Checkpoint unique owner status").count(),
            1,
            "{rendered}"
        );
        assert_eq!(
            rendered.matches("Latest note unique head").count(),
            1,
            "{rendered}"
        );
        assert_eq!(
            rendered.matches("Reader participation").count(),
            1,
            "{rendered}"
        );
    }
    assert!(
        receipt.value["participated"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["ref"] == reference && row["context_ref"].is_string())
    );
    assert!(
        receipt.value["changes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|change| {
                let line = change.as_str().unwrap();
                line.contains("see assigned") && line.contains("noted by agent")
            })
    );
}
