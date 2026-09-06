use super::*;

fn gates(verbs: &AgentVerbs, work: &str, count: i64, start: i64) {
    for index in 0..count {
        verbs
            .gate(
                GateInput {
                    work_ref: Some(work.into()),
                    name: format!("check {start}-{index}"),
                    failed: (0..16)
                        .map(|failure| format!("failure {failure}: {}", "x".repeat(180)))
                        .collect(),
                    evidence_ref: Some(format!("test:gate-{start}-{index}")),
                },
                at(start + index),
            )
            .unwrap();
    }
}

fn read(verbs: &AgentVerbs, work: &str, gates: bool, after: Option<String>, time: i64) -> Receipt {
    verbs
        .show_records(
            work,
            &ShowInput {
                notes: true,
                gates,
                after,
                ..ShowInput::default()
            },
            at(time),
        )
        .unwrap()
}

fn assert_families(receipt: &Receipt, notes: usize, observations: usize, gates: usize) {
    let rows = receipt.value["notes"].as_array().unwrap();
    let families = &receipt.value["notes_window"]["families"];
    for (family, total) in [
        ("notes", notes),
        ("observations", observations),
        ("gates", gates),
    ] {
        let shown = rows.iter().filter(|row| row["family"] == family).count();
        assert_eq!(
            families[family],
            json!({"total":total,"shown":shown,"omitted":total-shown})
        );
    }
    assert_eq!(receipt.text().matches("gate evidence:").count(), 1);
    assert!(receipt.text().len() < MAX_AGENT_WORK_RESPONSE_BYTES);
    assert!(
        serde_json::to_vec_pretty(&receipt.value).unwrap().len() < MAX_AGENT_WORK_RESPONSE_BYTES
    );
}

#[test]
fn note_windows_keep_verdict_after_nine_gates_and_page_gate_evidence_explicitly() {
    let (_directory, verbs, path, project) = fixture();
    let work = add(&verbs, "Verdict before gates", None, false, 0);
    note(&verbs, &work, "Peer observation", 1);
    verbs
        .claim(
            ClaimInput {
                work_ref: work.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(2),
        )
        .unwrap();
    note(
        &verbs,
        &work,
        "Final verdict: approved; gate-like prose is not a gate",
        3,
    );
    gates(&verbs, &work, 9, 4);
    let first = read(&verbs, &work, false, None, 20);
    assert_eq!(first.value["notes_window"]["total"], 2);
    assert_eq!(first.value["notes_omitted"], 0);
    assert_eq!(
        first.value["notes"][1]["summary"],
        "Final verdict: approved; gate-like prose is not a gate"
    );
    assert_eq!(first.value["notes_window"]["includes_gates"], false);
    assert!(
        first
            .next
            .contains(&format!("engram work show {work} --notes --gates"))
    );
    assert_families(&first, 1, 1, 9);
    let connection = rusqlite::Connection::open(&path).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&connection);
    let mut after = None;
    let mut seen = Vec::new();
    loop {
        let receipt = read(&verbs, &work, true, after, 20);
        assert_families(&receipt, 1, 1, 9);
        let rows = receipt.value["notes"].as_array().unwrap();
        assert_eq!(receipt.value["notes_window"]["newer"], seen.len());
        assert_eq!(receipt.value["notes_omitted"], 11 - rows.len());
        for row in rows {
            let locator = row["locator"].as_str().unwrap().to_owned();
            assert!(!seen.contains(&locator));
            let detail = verbs
                .show_records(
                    &work,
                    &ShowInput {
                        note: Some(locator.clone()),
                        ..ShowInput::default()
                    },
                    at(20),
                )
                .unwrap();
            assert_eq!(detail.value["note"]["family"], row["family"]);
            assert_eq!(detail.value["note"]["locator"], row["locator"]);
            seen.push(locator);
        }
        after = receipt.value["notes_window"]["after"]
            .as_str()
            .map(str::to_owned);
        if let Some(token) = &after {
            assert!(receipt.next.contains(&format!(
                "engram work show {work} --notes --gates --after {token}"
            )));
            let error = verbs
                .show_records(
                    &work,
                    &ShowInput {
                        notes: true,
                        after: Some(token.clone()),
                        ..ShowInput::default()
                    },
                    at(20),
                )
                .unwrap_err();
            assert!(matches!(
                error.error,
                StoreError::WorkShowCursorInvalid { .. }
            ));
            assert_eq!(
                error.guidance().next,
                vec![format!("engram work show '{work}' --notes")]
            );
        } else {
            break;
        }
        assert!(seen.len() < 11);
    }
    assert_eq!(seen.len(), 11);
    assert_eq!(
        before,
        crate::storage::test_database_shape_snapshot(&connection)
    );
    assert!(
        SqliteStore::open(&path)
            .unwrap()
            .verify_all()
            .unwrap()
            .is_healthy()
    );
    assert!(
        SqliteStore::open(&path)
            .unwrap()
            .resolve_work_ref(&project, &work)
            .is_ok()
    );
}

#[test]
fn note_window_gate_families_survive_restore_and_late_restored_gates() {
    let (directory, verbs, path, project) = fixture();
    let work = add(&verbs, "Restored gate families", None, false, 0);
    note(&verbs, &work, "Inherited observation", 1);
    verbs
        .claim(
            ClaimInput {
                work_ref: work.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(2),
        )
        .unwrap();
    note(&verbs, &work, "Inherited verdict", 3);
    gates(&verbs, &work, 2, 4);
    let completed = verbs
        .done(
            DoneInput {
                work_ref: Some(work.clone()),
                summary: Some("Completed source".into()),
                note: None,
            },
            at(7),
        )
        .unwrap();
    assert!(!completed.owed);
    let (restored, store, _) = super::super::review::load(
        directory.path(),
        &super::super::review::snapshot(&path, &project, &work),
    );
    gates(&restored, &work, 2, 102);
    let first = read(&restored, &work, false, None, 110);
    // The completion capture is a note, not a gate or an inherited event.
    assert_families(&first, 2, 1, 4);
    assert_eq!(first.value["notes_window"]["total"], 3);
    assert!(
        first.value["notes"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["family"] != "gates")
    );
    let explicit = read(&restored, &work, true, None, 110);
    assert_families(&explicit, 2, 1, 4);
    assert_eq!(explicit.value["notes_window"]["total"], 7);
    assert!(
        explicit.value["notes"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["family"] == "gates")
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn gate_only_items_have_an_empty_default_window_and_explicit_navigation() {
    let (_directory, verbs, _, _) = fixture();
    let work = add(&verbs, "Only gates", None, false, 0);
    verbs
        .claim(
            ClaimInput {
                work_ref: work.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(1),
        )
        .unwrap();
    gates(&verbs, &work, 9, 2);
    let receipt = read(&verbs, &work, false, None, 20);
    assert_families(&receipt, 0, 0, 9);
    assert_eq!(receipt.value["notes"], json!([]));
    assert_eq!(receipt.value["notes_omitted"], 0);
    assert_eq!(receipt.value["notes_window"]["total"], 0);
    assert!(receipt.value["notes_window"]["after"].is_null());
    assert!(
        receipt
            .next
            .contains(&format!("engram work show {work} --notes --gates"))
    );
    for input in [
        ShowInput {
            gates: true,
            ..ShowInput::default()
        },
        ShowInput {
            gates: true,
            history: true,
            ..ShowInput::default()
        },
        ShowInput {
            gates: true,
            note: Some("12345678".into()),
            ..ShowInput::default()
        },
    ] {
        assert!(matches!(
            verbs.show_records(&work, &input, at(20)).unwrap_err().error,
            StoreError::InvalidWork(_)
        ));
    }
}

#[test]
fn note_window_modes_are_bound_even_when_no_gate_changes_the_membership() {
    let (_directory, verbs, _, _) = fixture();
    let work = add(&verbs, "Mode binding", None, false, 0);
    for index in 0..8 {
        note(
            &verbs,
            &work,
            &format!("Observation {index}: {}", "body ".repeat(500)),
            index + 1,
        );
    }
    for includes_gates in [false, true] {
        let first = read(&verbs, &work, includes_gates, None, 20);
        assert_families(&first, 0, 8, 0);
        let token = first.value["notes_window"]["after"]
            .as_str()
            .unwrap()
            .to_owned();
        let error = verbs
            .show_records(
                &work,
                &ShowInput {
                    notes: true,
                    gates: !includes_gates,
                    after: Some(token.clone()),
                    ..ShowInput::default()
                },
                at(20),
            )
            .unwrap_err();
        assert!(matches!(
            error.error,
            StoreError::WorkShowCursorInvalid { .. }
        ));
        assert_eq!(
            error.guidance().next,
            vec![format!(
                "engram work show '{work}' --notes{}",
                if includes_gates { "" } else { " --gates" }
            )]
        );
        let continued = read(&verbs, &work, includes_gates, Some(token), 20);
        assert_families(&continued, 0, 8, 0);
        assert_eq!(
            continued.value["notes_window"]["newer"],
            first.value["notes_window"]["shown"]
        );
    }
}
