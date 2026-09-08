use super::*;
use crate::storage::{WorkRecordFamily, WorkRecordKind};

fn assert_family_index_does_not_parse_body(
    store: &SqliteStore,
    path: &std::path::Path,
    project: &ProjectId,
    work: &str,
    hash: &crate::ObjectHash,
    family: WorkRecordFamily,
) {
    let id = store.resolve_work_ref(project, work).unwrap().work_id;
    let connection = rusqlite::Connection::open(path).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&connection);
    let original: Vec<u8> = connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_hash = ?1",
            [hash.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    // Invalid JSON is a deterministic probe: parsing this body's gate field
    // during indexing must fail. Selected content must still reject corruption.
    connection
        .execute(
            "UPDATE objects SET canonical_json = ?1 WHERE object_hash = ?2",
            rusqlite::params![b"{".as_slice(), hash.as_str()],
        )
        .unwrap();
    let index = store.work_record_index(project, id, WorkRecordKind::NotesWithGates);
    let content_refused = index.as_ref().is_ok_and(|index| {
        let row = index
            .iter()
            .find(|entry| entry.address.hash == *hash)
            .unwrap();
        store.work_record_content(project, id, row).is_err()
    });
    connection
        .execute(
            "UPDATE objects SET canonical_json = ?1 WHERE object_hash = ?2",
            rusqlite::params![original, hash.as_str()],
        )
        .unwrap();
    let index = index.expect("non-gate-capable and restored bodies are not parsed by the index");
    assert_eq!(
        index
            .iter()
            .find(|entry| entry.address.hash == *hash)
            .unwrap()
            .record_family,
        family
    );
    assert!(
        content_refused,
        "content reads must still verify canonical bytes"
    );
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&connection),
        before
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn note_window_family_index_avoids_observation_and_restored_body_probes() {
    let (directory, verbs, path, project) = fixture();
    let work = add(&verbs, "Projection-only families", None, false, 0);
    note(&verbs, &work, &"Large observation ".repeat(2000), 1);
    let store = SqliteStore::open(&path).unwrap();
    let id = store.resolve_work_ref(&project, &work).unwrap().work_id;
    let index = store
        .work_record_index(&project, id, WorkRecordKind::Notes)
        .unwrap();
    assert_eq!(index.len(), 1);
    assert_family_index_does_not_parse_body(
        &store,
        &path,
        &project,
        &work,
        &index[0].address.hash,
        WorkRecordFamily::Observations,
    );
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
    assert!(
        !verbs
            .done(
                DoneInput {
                    links: Vec::new(),
                    link_basis: None,
                    work_ref: Some(work.clone()),
                    summary: Some("Completed source".into()),
                    note: None
                },
                at(3)
            )
            .unwrap()
            .owed
    );
    let (restored, store, path) = super::super::review::load(
        directory.path(),
        &super::super::review::snapshot(&path, &project, &work),
    );
    note(&restored, &work, "Late restored note", 102);
    gates(&restored, &work, 1, 103);
    let index = store
        .work_record_index(&project, id, WorkRecordKind::NotesWithGates)
        .unwrap();
    let native = index
        .iter()
        .filter(|entry| entry.address.member.is_none())
        .collect::<Vec<_>>();
    assert_eq!(native.len(), 2);
    for (row, family) in native
        .into_iter()
        .zip([WorkRecordFamily::Notes, WorkRecordFamily::Gates])
    {
        assert_family_index_does_not_parse_body(
            &store,
            &path,
            &project,
            &work,
            &row.address.hash,
            family,
        );
    }
}

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
            assert!(row.get("actor_session_id").is_none());
            assert_eq!(row["by"], "you");
            assert!(row["feed_position"].as_i64().unwrap() > 0);
            assert_eq!(detail.value["note"]["feed_position"], row["feed_position"]);
            assert_eq!(
                detail.value["note"]["actor_session_id"],
                row["actor_session_id"]
            );
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
                links: Vec::new(),
                link_basis: None,
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
    for row in explicit.value["notes"].as_array().unwrap() {
        let inherited = row["locator"].as_str().unwrap().contains(':');
        assert_eq!(row.get("feed_position").is_none(), inherited);
        assert!(row.get("actor_session_id").is_none());
        assert!(
            row["by"]
                .as_str()
                .is_some_and(|label| label == "you" || label.starts_with("peer-"))
        );
    }
    assert!(!explicit.text().contains("feed_position"));
    assert!(!explicit.text().contains("actor_session_id"));
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
