use super::*;
use crate::verbs::ShowInput;
use std::fmt::Write as _;

fn rendered_error(verbs: &AgentVerbs, error: &VerbError) -> String {
    let projected = verbs.project_error(error, crate::store_error_value(&error.error));
    if verbs.argument_names == crate::argument_names::ArgumentNames::Mcp {
        crate::mcp::prose_sweep::assert_prose_names_fields(&projected);
    }
    assert_eq!(projected["error"]["message"], verbs.error_message(error));
    projected.to_string()
}

#[test]
fn navigation_refusals_report_clock_membership_and_count_without_body_disclosure() {
    let (_directory, reader, path, project) = fixture();
    let target = add(&reader, "Target", None, false, 0);
    add(&reader, "Second", None, false, 1);
    for index in 0..12 {
        note(
            &reader,
            &target,
            &format!("PRIVATE BODY {index} {}", "content ".repeat(500)),
            index + 2,
        );
    }
    let mcp = AgentVerbs::new(
        path.clone(),
        project.clone(),
        "agent".into(),
        SessionId("mcp".into()),
        None,
    )
    .with_mcp_argument_names();
    for verbs in [&reader, &mcp] {
        let input = LsInput {
            limit: Some(1),
            ..Default::default()
        };
        let captured = at(30) + chrono::Duration::microseconds(123_100);
        let first = verbs.ls(&input, captured).unwrap();
        let token = first.value["after"].as_str().unwrap();
        let continued = LsInput {
            after: Some(token.into()),
            ..input.clone()
        };
        let reversed = captured - chrono::Duration::microseconds(1);
        let error = verbs.ls(&continued, reversed).unwrap_err();
        let text = rendered_error(verbs, &error);
        assert!(text.contains("check=clock_reversal"));
        assert!(text.contains(&format!("captured_at={captured}")));
        assert!(text.contains(&format!("now={reversed}")));
        assert_eq!(error.guidance().next, vec![input.list_command()]);
        let mut forged = crate::work_service::listing_cursor_json(token).unwrap();
        forged["basis"]["fingerprint"] = "PRIVATE BODY".into();
        let error = verbs
            .ls(
                &LsInput {
                    after: Some(crate::work_service::encode_listing_cursor_json(&forged).unwrap()),
                    ..input
                },
                captured,
            )
            .unwrap_err();
        let text = rendered_error(verbs, &error);
        assert!(text.contains("check=membership"));
        assert!(text.contains("expected_fingerprint=invalid(bytes=12, sha256="));
        assert!(text.contains("observed_fingerprint="));
        assert!(!text.contains("PRIVATE BODY"));
    }
    let input = ShowInput {
        notes: true,
        gates: true,
        ..Default::default()
    };
    let first = reader.show_records(&target, &input, at(40)).unwrap();
    let token = first.value["notes_window"]["after"].as_str().unwrap();
    let total = first.value["notes_window"]["total"].as_u64().unwrap();
    let continued = ShowInput {
        after: Some(token.into()),
        ..input
    };
    let error = reader
        .show_records(&target, &continued, at(39))
        .unwrap_err();
    assert!(rendered_error(&reader, &error).contains("check=clock_reversal"));
    let bytes = token.as_bytes()[3..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect::<Vec<_>>();
    let mut boundary: Value = serde_json::from_slice(&bytes).unwrap();
    boundary["order"]["position"] = (-1).into();
    let mut forged = String::from("s1-");
    for byte in serde_json::to_vec(&boundary).unwrap() {
        write!(forged, "{byte:02x}").unwrap();
    }
    let error = reader
        .show_records(
            &target,
            &ShowInput {
                after: Some(forged),
                ..continued.clone()
            },
            at(40),
        )
        .unwrap_err();
    let text = rendered_error(&reader, &error);
    assert!(text.contains("check=boundary"));
    assert!(text.contains("observed=tuple_not_found"));
    assert!(text.contains("expected_order="));
    assert!(!text.contains("PRIVATE BODY"));
    note(&reader, &target, "PRIVATE BODY appended", 41);
    for verbs in [&reader, &mcp] {
        let error = verbs.show_records(&target, &continued, at(42)).unwrap_err();
        let text = rendered_error(verbs, &error);
        assert!(text.contains("check=selected_count"));
        assert!(text.contains("kind=notes_with_gates"));
        assert!(text.contains(&format!("expected_count={total}")));
        assert!(text.contains(&format!("observed_count={}", total + 1)));
        assert!(!text.contains("PRIVATE BODY"));
        assert!(error.guidance().next[0].contains(&target));
        assert!(error.guidance().next[0].ends_with("--notes --gates"));
    }
}

#[test]
fn navigation_refusals_pages_survive_another_sessions_notes_gates_and_claim_renewals() {
    let (_directory, reader, path, project) = fixture();
    let target = add(&reader, "Window target", None, false, 0);
    reader
        .claim(
            ClaimInput {
                work_ref: target.clone(),
                ttl_seconds: Some(3600),
                recover: None,
            },
            at(1),
        )
        .unwrap();
    for index in 0..12 {
        note(
            &reader,
            &target,
            &format!("Note {index} {}", "content ".repeat(500)),
            index + 2,
        );
        reader
            .gate(
                GateInput {
                    work_ref: Some(target.clone()),
                    name: format!("gate-{index}"),
                    failed: Vec::new(),
                    evidence_ref: None,
                },
                at(index + 2),
            )
            .unwrap();
    }
    for index in 0..5 {
        add(
            &reader,
            &format!("Catalog {index}"),
            None,
            false,
            20 + index,
        );
    }
    let (step_tx, step_rx) = std::sync::mpsc::channel::<i64>();
    let (ack_tx, ack_rx) = std::sync::mpsc::channel();
    let writer = std::thread::spawn(move || {
        let writer = AgentVerbs::new(
            path,
            project,
            "writer".into(),
            SessionId("writer".into()),
            None,
        );
        let unrelated = add(&writer, "Unrelated writer", None, false, 30);
        writer
            .claim(
                ClaimInput {
                    work_ref: unrelated.clone(),
                    ttl_seconds: Some(3600),
                    recover: None,
                },
                at(31),
            )
            .unwrap();
        ack_tx.send(()).unwrap();
        for time in step_rx {
            note(&writer, &unrelated, "Concurrent unrelated note", time);
            writer
                .gate(
                    GateInput {
                        work_ref: Some(unrelated.clone()),
                        name: format!("unrelated-{time}"),
                        failed: Vec::new(),
                        evidence_ref: None,
                    },
                    at(time),
                )
                .unwrap();
            writer
                .claim(
                    ClaimInput {
                        work_ref: unrelated.clone(),
                        ttl_seconds: Some(3600),
                        recover: None,
                    },
                    at(time),
                )
                .unwrap();
            ack_tx.send(()).unwrap();
        }
    });
    ack_rx.recv().unwrap();
    let input = LsInput {
        all: true,
        limit: Some(1000),
        ..Default::default()
    };
    let expected = reader.ls(&input, at(100)).unwrap().value["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["ref"].as_str().unwrap().to_owned())
        .collect::<Vec<_>>();
    let mut input = LsInput {
        all: true,
        limit: Some(1),
        ..Default::default()
    };
    let mut seen = Vec::new();
    let mut time = 101;
    loop {
        let page = reader.ls(&input, at(time)).unwrap();
        seen.extend(
            page.value["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["ref"].as_str().unwrap().to_owned()),
        );
        input.after = page.value["after"].as_str().map(str::to_owned);
        if input.after.is_none() {
            break;
        }
        time += 1;
        step_tx.send(time).unwrap();
        ack_rx.recv().unwrap();
    }
    assert_eq!(seen, expected);
    assert_eq!(
        seen.iter().collect::<std::collections::BTreeSet<_>>().len(),
        seen.len()
    );
    let mut input = ShowInput {
        notes: true,
        gates: true,
        ..Default::default()
    };
    let mut newest_first = Vec::new();
    let mut total = None;
    let mut pages = 0;
    loop {
        let page = reader.show_records(&target, &input, at(time)).unwrap();
        let meta = &page.value["notes_window"];
        let count = meta["total"].as_u64().unwrap();
        assert_eq!(*total.get_or_insert(count), count);
        let rows = page.value["notes"].as_array().unwrap();
        assert_ne!(rows.as_slice(), &[] as &[serde_json::Value]);
        assert_eq!(meta["newer"], newest_first.len());
        newest_first.extend(rows.iter().rev().map(|row| {
            (
                row["locator"].as_str().unwrap().to_owned(),
                row["feed_position"].as_i64().unwrap(),
            )
        }));
        pages += 1;
        input.after = meta["after"].as_str().map(str::to_owned);
        if input.after.is_none() {
            break;
        }
        time += 1;
        step_tx.send(time).unwrap();
        ack_rx.recv().unwrap();
    }
    assert!(pages > 1);
    assert_eq!(newest_first.len() as u64, total.unwrap());
    assert_eq!(
        newest_first
            .iter()
            .collect::<std::collections::BTreeSet<_>>()
            .len(),
        newest_first.len()
    );
    newest_first.reverse();
    assert_eq!(newest_first.len(), 24);
    assert!(newest_first.windows(2).all(|pair| pair[0].1 < pair[1].1));
    drop(step_tx);
    writer.join().unwrap();
}
