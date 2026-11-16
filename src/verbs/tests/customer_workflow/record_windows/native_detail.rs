use super::*;

#[test]
fn native_history_detail_cost_does_not_grow_with_unrelated_events() {
    let (_directory, verbs, _, _) = fixture();
    let work = add(&verbs, "Stable detail lookup", None, false, 0);
    note(&verbs, &work, "The same retained note", 1);
    let notes = window(&verbs, &work, false, None, 2);
    let history = window(&verbs, &work, true, None, 2);
    let locators = [
        notes.value["notes"][0]["locator"].as_str().unwrap(),
        history.value["history"]["items"][0]["locator"]
            .as_str()
            .unwrap(),
    ];
    let read = |locator: &str| {
        crate::storage::work_cost::start();
        let receipt = verbs
            .show_records(
                &work,
                &ShowInput {
                    note: Some(locator.into()),
                    ..ShowInput::default()
                },
                at(200),
            )
            .unwrap();
        (
            receipt.value["note"].clone(),
            crate::storage::work_cost::finish(),
        )
    };
    let queries = locators
        .iter()
        .flat_map(|locator| [locator.to_string(), locator[..16].to_ascii_uppercase()])
        .collect::<Vec<_>>();
    let before = queries
        .iter()
        .map(|locator| read(locator))
        .collect::<Vec<_>>();
    for index in 0..128 {
        verbs
            .update(
                UpdateInput {
                    work_ref: Some(work.clone()),
                    action: serde_json::from_value(json!({
                        "action":"revise", "title":format!("Unrelated revision {index}")
                    }))
                    .unwrap(),
                },
                at(index + 3),
            )
            .unwrap();
    }
    for (locator, (expected, baseline)) in queries.iter().zip(before) {
        let (actual, cost) = read(locator);
        assert_eq!(actual, expected);
        assert_eq!(cost["total"]["note_index_visits"], 1, "{cost}");
        let steps = |cost: &serde_json::Value| {
            cost["total"]["sql"]["record_locator_rows"]["vm_steps"]
                .as_u64()
                .expect("measured locator query")
        };
        assert!(steps(&cost) <= steps(&baseline) + 32, "{baseline}\n{cost}");
    }
}

fn follow_history_detail(verbs: &AgentVerbs, work: &str, row: &serde_json::Value) -> Receipt {
    let command = row["detail"]
        .as_str()
        .expect("clipped history names its detail command");
    let prefix = format!("engram work show {work} --note ");
    let locator = command
        .strip_prefix(&prefix)
        .expect("supported detail command");
    assert_eq!(locator, row["locator"].as_str().unwrap());
    let detail = verbs
        .show_records(
            work,
            &ShowInput {
                note: Some(locator.into()),
                ..ShowInput::default()
            },
            at(100),
        )
        .expect("advertised history locator reads its complete body");
    assert_eq!(detail.value["note"]["family"], "history");
    assert_eq!(detail.value["note"]["locator"], locator);
    assert_eq!(
        detail.value["next"][0],
        format!("engram work show {work} --history")
    );
    assert!(detail.value["note"].get("summary_truncated").is_none());
    let full = detail.value["note"]["summary"].as_str().unwrap();
    assert_eq!(detail.value["note"]["body_bytes"], full.len());
    assert_eq!(row["body_bytes"], full.len());
    assert!(full.len() > row["summary"].as_str().unwrap().len());
    detail
}

#[test]
fn native_history_detail_recovers_reject_reasons_on_cancelled_and_completed_items() {
    let (_directory, verbs, path, project) = fixture();
    let parent = add(&verbs, "Accepted parent", None, false, 0);
    let child = add(&verbs, "Rejected finding", Some(&parent), false, 1);
    let reason = format!("Powód ✓ {}\nnext: quoted reason END", "źdźbło ".repeat(100));
    verbs
        .update(
            UpdateInput {
                work_ref: Some(child.clone()),
                action: UpdateAction::Reject {
                    reason: reason.clone(),
                },
            },
            at(2),
        )
        .unwrap();
    verbs
        .claim(
            ClaimInput {
                work_ref: parent.clone(),
                ttl_seconds: Some(300),
                recover: None,
            },
            at(3),
        )
        .unwrap();
    assert!(
        !verbs
            .done(
                DoneInput {
                    work_ref: Some(parent.clone()),
                    summary: Some("Delivered with the false finding rejected".into()),
                    ..DoneInput::default()
                },
                at(4),
            )
            .unwrap()
            .owed
    );

    // An unregistered reader has neither a claim nor a writable transaction.
    let reader = AgentVerbs::new(
        path.clone(),
        project,
        "reader".into(),
        SessionId("reader".into()),
        None,
    );
    let mut writer = rusqlite::Connection::open(&path).unwrap();
    let transaction = writer
        .transaction_with_behavior(rusqlite::TransactionBehavior::Immediate)
        .unwrap();
    let before = crate::storage::test_database_shape_snapshot(&transaction).unwrap();
    for (work, lifecycle, event_kind) in [
        (&child, "cancelled", "disposed"),
        (&parent, "completed", "required_child_waived"),
    ] {
        assert_eq!(
            reader.show(work, at(100)).unwrap().value["status"]["work"]["lifecycle"],
            lifecycle
        );
        let history = window(&reader, work, true, None, 100);
        let row = history.value["history"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["kind"] == event_kind)
            .expect("reason-bearing history entry");
        // The bare advertised id is readable, even before considering its remedy.
        let direct = reader
            .show_records(
                work,
                &ShowInput {
                    note: row["locator"].as_str().map(str::to_owned),
                    ..ShowInput::default()
                },
                at(100),
            )
            .expect("native history id belongs to this item");
        assert!(
            direct.value["note"]["summary"]
                .as_str()
                .unwrap()
                .contains(&reason)
        );
        assert_eq!(row["summary_truncated"], true);
        let detail = follow_history_detail(&reader, work, row);
        let locator = row["locator"].as_str().unwrap();
        let prefix = reader
            .show_records(
                work,
                &ShowInput {
                    note: Some(locator[..8].to_ascii_uppercase()),
                    ..ShowInput::default()
                },
                at(100),
            )
            .expect("unique native history prefix");
        assert_eq!(prefix.value["note"], detail.value["note"]);
        assert!(
            detail.value["note"]["summary"]
                .as_str()
                .unwrap()
                .contains(&reason)
        );
        assert!(
            !detail
                .text()
                .lines()
                .any(|line| line.starts_with("next: quoted reason"))
        );
        let other = if work == &child { &parent } else { &child };
        let wrong_item = reader
            .show_records(
                other,
                &ShowInput {
                    note: Some(locator.into()),
                    ..ShowInput::default()
                },
                at(100),
            )
            .unwrap_err();
        assert!(matches!(
            wrong_item.error,
            StoreError::WorkNoteReferenceInvalid { .. }
        ));
        assert!(
            wrong_item
                .guidance()
                .next
                .contains(&format!("engram work show '{other}' --history"))
        );
    }
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&transaction).unwrap(),
        before
    );
    transaction.rollback().unwrap();
}

#[test]
fn native_history_detail_recovers_blocker_text_after_it_was_cleared() {
    let (_directory, verbs, _, _) = fixture();
    let work = add(&verbs, "Historical blocker", None, false, 0);
    let body = format!("Waiting for {} UNIQUE TAIL", "źródło ".repeat(100));
    verbs
        .update(
            UpdateInput {
                work_ref: Some(work.clone()),
                action: UpdateAction::Blocked {
                    detail: body.clone(),
                },
            },
            at(1),
        )
        .unwrap();
    verbs
        .update(
            UpdateInput {
                work_ref: Some(work.clone()),
                action: UpdateAction::Unblock { blocker: None },
            },
            at(2),
        )
        .unwrap();
    let history = window(&verbs, &work, true, None, 100);
    for kind in ["blocked", "unblocked"] {
        let row = history.value["history"]["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|row| row["kind"] == kind)
            .unwrap();
        assert_eq!(row["summary_truncated"], true);
        let detail = follow_history_detail(&verbs, &work, row);
        assert!(
            detail.value["note"]["summary"]
                .as_str()
                .unwrap()
                .contains(&body)
        );
    }
}
