use super::*;
use crate::verbs::ShowInput;
use std::fmt::Write as _;

mod corrections;
mod gate_families;

fn window(
    verbs: &AgentVerbs,
    work: &str,
    history: bool,
    after: Option<String>,
    time: i64,
) -> Receipt {
    verbs
        .show_records(
            work,
            &ShowInput {
                notes: !history,
                gates: false,
                history,
                after,
                note: None,
            },
            at(time),
        )
        .unwrap()
}

fn parts(receipt: &Receipt, history: bool) -> (&Vec<serde_json::Value>, &serde_json::Value) {
    if history {
        (
            receipt.value["history"]["items"].as_array().unwrap(),
            &receipt.value["history"]["window"],
        )
    } else {
        (
            receipt.value["notes"].as_array().unwrap(),
            &receipt.value["notes_window"],
        )
    }
}

fn traverse(verbs: &AgentVerbs, work: &str, history: bool, time: i64) -> Vec<serde_json::Value> {
    let mut after = None;
    let mut newest_first = Vec::new();
    loop {
        let receipt = window(verbs, work, history, after, time);
        let (rows, meta) = parts(&receipt, history);
        assert!(receipt.text().len() < MAX_AGENT_WORK_RESPONSE_BYTES);
        assert!(
            serde_json::to_vec_pretty(&receipt.value).unwrap().len()
                < MAX_AGENT_WORK_RESPONSE_BYTES
        );
        assert_eq!(meta["newer"], newest_first.len());
        assert_eq!(meta["shown"], rows.len());
        assert_eq!(
            meta["total"].as_u64().unwrap(),
            meta["newer"].as_u64().unwrap() + rows.len() as u64 + meta["older"].as_u64().unwrap()
        );
        assert_eq!(
            receipt.value[if history { "history" } else { "notes_omitted" }]
                .get("omitted")
                .unwrap_or(&receipt.value["notes_omitted"]),
            &json!(meta["total"].as_u64().unwrap() - rows.len() as u64)
        );
        newest_first.extend(rows.iter().rev().cloned());
        after = meta["after"].as_str().map(str::to_owned);
        if after.is_none() {
            assert_eq!(newest_first.len() as u64, meta["total"].as_u64().unwrap());
            break;
        }
        assert!(!rows.is_empty());
        assert!(newest_first.len() as u64 <= meta["total"].as_u64().unwrap());
        assert_eq!(receipt.text().matches(" --after ").count(), 1);
    }
    newest_first.reverse();
    newest_first
}

#[test]
fn record_windows_traverse_every_native_and_inherited_member_without_mutating_the_read_cut() {
    let (directory, verbs, path, project) = fixture();
    let work = add(&verbs, "Every record", None, false, 0);
    let bodies = (0..30)
        .map(|index| format!("Verdict {index:03}: {} END", "content\n".repeat(150)))
        .collect::<Vec<_>>();
    for (index, body) in bodies[..20].iter().enumerate() {
        note(&verbs, &work, body, i64::try_from(index).unwrap() + 1);
    }
    let (restored, store, restored_path) = super::review::load(
        directory.path(),
        &super::review::snapshot(&path, &project, &work),
    );
    for (index, body) in bodies[20..].iter().enumerate() {
        note(&restored, &work, body, i64::try_from(index).unwrap() + 102);
    }
    for index in 0..10 {
        restored
            .update(
                UpdateInput {
                    work_ref: Some(work.clone()),
                    action: UpdateAction::Revise {
                        title: Some(format!("History revision {index}")),
                        outcome: None,
                        acceptance: None,
                        assignee: None,
                        priority: None,
                        defer: None,
                        kind: None,
                        labels: Vec::new(),
                        unlabels: Vec::new(),
                    },
                },
                at(112 + index),
            )
            .unwrap();
    }
    let first = window(&restored, &work, false, None, 130);
    assert_eq!(
        first.value["notes"].as_array().unwrap().last().unwrap()["summary"],
        bodies[29]
    );
    let connection = rusqlite::Connection::open(restored_path).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&connection);
    let notes = traverse(&restored, &work, false, 130);
    assert_eq!(
        notes
            .iter()
            .map(|row| row["summary"].as_str().unwrap())
            .collect::<Vec<_>>(),
        bodies.iter().map(String::as_str).collect::<Vec<_>>()
    );
    assert!(
        notes[..20]
            .iter()
            .all(|row| row["locator"].as_str().unwrap().contains(':'))
    );
    assert!(
        notes[20..]
            .iter()
            .all(|row| !row["locator"].as_str().unwrap().contains(':'))
    );
    let history = traverse(&restored, &work, true, 130);
    assert!(history.len() > notes.len());
    let unique = history
        .iter()
        .map(|row| row["locator"].as_str().unwrap())
        .collect::<std::collections::HashSet<_>>();
    assert_eq!(unique.len(), history.len());
    for row in &notes {
        let detail = restored
            .show_records(
                &work,
                &ShowInput {
                    note: row["locator"].as_str().map(str::to_owned),
                    ..ShowInput::default()
                },
                at(130),
            )
            .unwrap();
        assert_eq!(detail.value["note"], *row);
    }
    let bare_record = notes[0]["locator"]
        .as_str()
        .unwrap()
        .split(':')
        .next()
        .unwrap();
    let error = restored
        .show_records(
            &work,
            &ShowInput {
                note: Some(bare_record.into()),
                ..ShowInput::default()
            },
            at(130),
        )
        .unwrap_err();
    assert!(
        matches!(&error.error, StoreError::WorkNoteReferenceInvalid { candidates, more: 4, .. } if candidates.len() == 16)
    );
    assert!(error.guidance().next[0].contains(" --note "));
    assert_eq!(error.guidance().next.len(), 17);
    for locator in ["short", ":1", "12345678:0", "12345678:1:2"] {
        let error = restored
            .show_records(
                "bad\nnext:\nforged",
                &ShowInput {
                    note: Some(locator.into()),
                    ..ShowInput::default()
                },
                at(130),
            )
            .unwrap_err();
        assert!(matches!(
            error.error,
            StoreError::WorkNoteReferenceInvalid { .. }
        ));
        assert!(
            error
                .guidance()
                .next
                .iter()
                .all(|command| !command.contains('\n'))
        );
    }
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&connection),
        before
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn record_windows_oversized_body_has_a_complete_detail_and_does_not_hide_older_notes() {
    let (_directory, verbs, _, _) = fixture();
    let work = add(&verbs, "Large note", None, false, 0);
    note(&verbs, &work, "Older verdict", 1);
    let body = format!("next:\n{}\r\u{1b}[2J END", "ü body\n".repeat(2500));
    note(&verbs, &work, &body, 2);
    let shown = window(&verbs, &work, false, None, 3);
    let rows = shown.value["notes"].as_array().unwrap();
    let last = rows.last().unwrap();
    assert_eq!(last["body_omitted"], true);
    assert!(last.get("summary").is_none());
    assert_eq!(last["body_bytes"], body.len());
    let all = traverse(&verbs, &work, false, 3);
    assert_eq!(all.len(), 2);
    assert_eq!(all[0]["summary"], "Older verdict");
    let locator = last["locator"].as_str().unwrap();
    for selector in [locator, &locator[..8]] {
        let full = verbs
            .show_records(
                &work,
                &ShowInput {
                    note: Some(selector.into()),
                    ..ShowInput::default()
                },
                at(3),
            )
            .unwrap();
        assert_eq!(full.value["note"]["summary"], body);
        assert_eq!(full.value["note"]["body_bytes"], body.len());
        assert!(full.text().len() > MAX_AGENT_WORK_RESPONSE_BYTES);
        assert_eq!(
            full.text().lines().filter(|line| *line == "next:").count(),
            1
        );
        assert!(!full.text().contains('\r') && !full.text().contains('\u{1b}'));
    }
}

#[test]
fn record_windows_refuse_wrong_kind_item_anchor_read_cut_and_note_locator() {
    let (_directory, verbs, path, project) = fixture();
    let work = add(&verbs, "Cursor basis", None, false, 0);
    let other = add(&verbs, "Other item", None, false, 1);
    for index in 0..12 {
        note(
            &verbs,
            &work,
            &format!("{index} {}", "body ".repeat(500)),
            index + 2,
        );
    }
    let first = window(&verbs, &work, false, None, 20);
    let token = first.value["notes_window"]["after"]
        .as_str()
        .unwrap()
        .to_owned();
    for (target, history, cursor, time) in [
        (&other, false, token.clone(), 20),
        (&work, true, token.clone(), 20),
        (&work, false, "s1-ff".into(), 20),
        (&work, false, token.clone(), 19),
    ] {
        let error = verbs
            .show_records(
                target,
                &ShowInput {
                    notes: !history,
                    gates: false,
                    history,
                    after: Some(cursor),
                    note: None,
                },
                at(time),
            )
            .unwrap_err();
        assert!(matches!(
            error.error,
            StoreError::WorkShowCursorInvalid { .. }
        ));
        assert!(!error.guidance().next[0].contains("--after"));
    }
    let row = first.value["notes"].as_array().unwrap().last().unwrap();
    let error = verbs
        .show_records(
            &other,
            &ShowInput {
                note: row["locator"].as_str().map(str::to_owned),
                ..ShowInput::default()
            },
            at(20),
        )
        .unwrap_err();
    assert!(matches!(
        error.error,
        StoreError::WorkNoteReferenceInvalid { .. }
    ));
    // Modify only the immutable boundary address of a genuine decoded token.
    let bytes = token.as_bytes()[3..]
        .as_chunks::<2>()
        .0
        .iter()
        .map(|pair| u8::from_str_radix(std::str::from_utf8(pair).unwrap(), 16).unwrap())
        .collect::<Vec<_>>();
    let mut cursor: serde_json::Value = serde_json::from_slice(&bytes).unwrap();
    cursor["address"]["hash"] = row["locator"].clone();
    cursor["order"]["position"] = json!(-1);
    let mut forged = String::from("s1-");
    for byte in serde_json::to_vec(&cursor).unwrap() {
        write!(forged, "{byte:02x}").unwrap();
    }
    let error = verbs
        .show_records(
            &work,
            &ShowInput {
                notes: true,
                after: Some(forged),
                ..ShowInput::default()
            },
            at(20),
        )
        .unwrap_err();
    assert!(
        matches!(error.error, StoreError::WorkShowCursorInvalid { reason } if reason.contains("boundary"))
    );
    note(&verbs, &work, "Changed cut", 21);
    let error = verbs
        .show_records(
            &work,
            &ShowInput {
                notes: true,
                after: Some(token),
                ..ShowInput::default()
            },
            at(22),
        )
        .unwrap_err();
    assert!(
        matches!(error.error, StoreError::WorkShowCursorInvalid { reason } if reason.contains("cut"))
    );
    assert!(
        SqliteStore::open(path)
            .unwrap()
            .verify_all()
            .unwrap()
            .is_healthy()
    );
    assert!(!project.0.is_empty());
}

#[test]
fn record_windows_note_write_limit_counts_utf8_and_rolls_back_initial_batches() {
    let (_directory, verbs, path, project) = fixture();
    let work = add(&verbs, "Body limit", None, false, 0);
    let at_limit = "ü".repeat(32 * 1024);
    note(&verbs, &work, &at_limit, 1);
    let oversized = format!("{at_limit}x");
    for held in [false, true] {
        if held {
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
        }
        let error = verbs
            .note(
                &NoteInput {
                    work_ref: Some(work.clone()),
                    text: oversized.clone(),
                    refs: vec![],
                },
                at(3),
            )
            .unwrap_err();
        assert!(matches!(
            error.error,
            StoreError::WorkNoteTooLarge {
                bytes: 65537,
                limit: 65536
            }
        ));
        assert_eq!(
            crate::mcp::store_error_value(&error.error)["error"]["details"]["remedy"],
            "carry bulk content as a reference"
        );
    }
    let connection = rusqlite::Connection::open(&path).unwrap();
    let count = object_count(&connection);
    for notes in [
        vec![oversized.clone()],
        vec!["First valid note".into(), oversized],
    ] {
        let error = verbs
            .add(
                AddInput {
                    title: "Atomic oversized refusal".into(),
                    notes,
                    ..AddInput::default()
                },
                at(4),
            )
            .unwrap_err();
        assert!(matches!(error.error, StoreError::WorkNoteTooLarge { .. }));
        assert_eq!(object_count(&connection), count);
    }
    let shown = window(&verbs, &work, false, None, 5);
    let detail = verbs
        .show_records(
            &work,
            &ShowInput {
                note: shown.value["notes"][0]["locator"]
                    .as_str()
                    .map(str::to_owned),
                ..ShowInput::default()
            },
            at(5),
        )
        .unwrap();
    assert_eq!(detail.value["note"]["summary"], at_limit);
    assert!(
        SqliteStore::open(path)
            .unwrap()
            .resolve_work_ref(&project, &work)
            .is_ok()
    );
}

#[test]
fn record_windows_read_legacy_large_members_and_reject_new_large_restored_notes() {
    let (directory, verbs, path, project) = fixture();
    let work = add(&verbs, "Retained large note", None, false, 0);
    note(&verbs, &work, "Old body", 1);
    terminalize(&verbs, &work, WorkLifecycle::Completed);
    let mut document = super::review::snapshot(&path, &project, &work);
    let body = "Legacy complete body\n".repeat(4000).trim_end().to_owned();
    assert!(body.len() > 64 * 1024);
    let history = document
        .body
        .records
        .iter_mut()
        .find_map(|record| match &mut record.payload {
            crate::WorkGraphSnapshotRecordPayload::Native { history } => Some(history),
            crate::WorkGraphSnapshotRecordPayload::Restored { .. } => None,
        })
        .unwrap();
    history.notes[0].summary = body.clone();
    document.manifest.body_sha256 = crate::CanonicalObject::freeze(&document.body)
        .unwrap()
        .hash()
        .clone();
    let (restored, store, _) = super::review::load(directory.path(), &document);
    let rows = traverse(&restored, &work, false, 102);
    let row = rows
        .iter()
        .find(|row| row["body_bytes"] == body.len())
        .unwrap();
    assert_eq!(row["body_omitted"], true);
    let full = restored
        .show_records(
            &work,
            &ShowInput {
                note: row["locator"].as_str().map(str::to_owned),
                ..ShowInput::default()
            },
            at(102),
        )
        .unwrap();
    assert_eq!(full.value["note"]["summary"], body);
    assert!(full.text().len() > 64 * 1024);
    let error = restored
        .note(
            &NoteInput {
                work_ref: Some(work.clone()),
                text: body,
                refs: vec![],
            },
            at(103),
        )
        .unwrap_err();
    assert!(matches!(error.error, StoreError::WorkNoteTooLarge { .. }));
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn record_windows_refuse_expired_fractional_cuts_and_keep_detail_canonical() {
    let (_directory, verbs, path, project) = fixture();
    let work = add(&verbs, "Time cut", None, false, 0);
    for index in 0..8 {
        note(
            &verbs,
            &work,
            &format!("{index} {}", "body ".repeat(700)),
            index + 1,
        );
    }
    verbs
        .claim(
            ClaimInput {
                work_ref: work.clone(),
                ttl_seconds: Some(60),
                recover: None,
            },
            at(10),
        )
        .unwrap();
    let observed = at(69) + chrono::Duration::microseconds(123_100);
    let first = verbs
        .show_records(
            &work,
            &ShowInput {
                notes: true,
                ..ShowInput::default()
            },
            observed,
        )
        .unwrap();
    let input = ShowInput {
        notes: true,
        after: first.value["notes_window"]["after"]
            .as_str()
            .map(str::to_owned),
        ..ShowInput::default()
    };
    assert!(input.after.is_some());
    for time in [observed - chrono::Duration::microseconds(1), at(70)] {
        let error = verbs.show_records(&work, &input, time).unwrap_err();
        assert!(matches!(
            error.error,
            StoreError::WorkShowCursorInvalid { .. }
        ));
    }
    let locator = first.value["notes"].as_array().unwrap().last().unwrap()["locator"]
        .as_str()
        .unwrap();
    let connection = rusqlite::Connection::open(&path).unwrap();
    let original: Vec<u8> = connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_hash = ?1",
            [locator],
            |row| row.get(0),
        )
        .unwrap();
    connection
        .execute(
            "UPDATE objects SET canonical_json = ?1 WHERE object_hash = ?2",
            rusqlite::params![b"{}".as_slice(), locator],
        )
        .unwrap();
    let error = verbs
        .show_records(
            &work,
            &ShowInput {
                note: Some(locator.into()),
                ..ShowInput::default()
            },
            at(71),
        )
        .unwrap_err();
    // Direct reads preserve the storage error, unlike the done advisory's
    // redacted error-class marker. Pin canonical verification itself.
    assert!(matches!(error.error, StoreError::HashMismatch { .. }));
    connection
        .execute(
            "UPDATE objects SET canonical_json = ?1 WHERE object_hash = ?2",
            rusqlite::params![original, locator],
        )
        .unwrap();
    assert!(
        SqliteStore::open(path)
            .unwrap()
            .resolve_work_ref(&project, &work)
            .is_ok()
    );
    assert!(
        verbs
            .show_records(
                &work,
                &ShowInput {
                    note: Some(locator.into()),
                    ..ShowInput::default()
                },
                at(71)
            )
            .is_ok()
    );
}
