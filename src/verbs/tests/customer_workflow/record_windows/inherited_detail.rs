use super::*;

fn detail(verbs: &AgentVerbs, work: &str, locator: &str) -> Result<Receipt, VerbError> {
    verbs.show_records(
        work,
        &ShowInput {
            note: Some(locator.into()),
            ..ShowInput::default()
        },
        at(130),
    )
}

// A shortened inherited event or completion row in the history window keeps
// its original size, says it was shortened and names a detail command. That
// detail returns the whole summary and the complete member, every field as
// the record stores it, framed as data with its byte size. The window's
// shape and totals are those of the same history without the detail route.
#[test]
fn inherited_event_and_completion_rows_have_a_complete_detail() {
    let (directory, verbs, path, project) = fixture();
    let work = add(&verbs, "Inherited detail", None, false, 0);
    let open = add(&verbs, "Never completed", None, false, 1);
    verbs
        .claim(
            ClaimInput {
                work_ref: work.clone(),
                ttl_seconds: Some(600),
                recover: None,
            },
            at(2),
        )
        .unwrap();
    // Long multibyte text with a line break and a terminal control byte.
    let completion_summary = format!(
        "Zakończone ✓ {}\nnext: forged\u{1b}[31m",
        "żółw ".repeat(80)
    );
    verbs
        .done(
            DoneInput {
                work_ref: Some(work.clone()),
                summary: Some(completion_summary.clone()),
                ..DoneInput::default()
            },
            at(3),
        )
        .unwrap();
    let mut document = super::super::review::snapshot(&path, &project, &work);
    let reason = format!("Powód ✓ {}\nnext: forged\u{7}", "źdźbło ".repeat(60));
    let mut expected_event = None;
    let mut expected_completion = None;
    for record in &mut document.body.records {
        let crate::WorkGraphSnapshotRecordPayload::Native { history } = &mut record.payload else {
            continue;
        };
        if let Some(completion) = &history.completion {
            // The first event whose kind carries a reason.
            let position = history
                .events
                .iter()
                .position(|event| event.reason.is_some() || event.kind == "claimed")
                .expect("an event that carries a reason");
            history.events[position].reason = Some(reason.clone());
            expected_event = Some((
                position + 1,
                serde_json::to_value(&history.events[position]).unwrap(),
            ));
            expected_completion = Some(serde_json::to_value(completion).unwrap());
        }
    }
    let ((event_index, expected_event), expected_completion) =
        (expected_event.unwrap(), expected_completion.unwrap());
    let event_suffix = format!(":event-{event_index}");
    assert_eq!(expected_completion["summary"], completion_summary);
    document.manifest.body_sha256 = crate::CanonicalObject::freeze(&document.body)
        .unwrap()
        .key()
        .clone();
    let home = directory.path().join("restored");
    std::fs::create_dir_all(&home).unwrap();
    let (restored, _store, _path) = super::super::review::load(&home, &document);

    let history = traverse(&restored, &work, true, 130);
    let first = window(&restored, &work, true, None, 130);
    assert_eq!(
        first.value["history"]["window"]["total"].as_u64().unwrap(),
        history.len() as u64
    );
    let row = |suffix: &str| {
        history
            .iter()
            .find(|row| row["locator"].as_str().unwrap().ends_with(suffix))
            .unwrap_or_else(|| panic!("no {suffix} row in {history:?}"))
            .clone()
    };
    for (suffix, full, member) in [
        (event_suffix.as_str(), reason.as_str(), &expected_event),
        (
            ":completion",
            completion_summary.as_str(),
            &expected_completion,
        ),
    ] {
        let windowed = row(suffix);
        let locator = windowed["locator"].as_str().unwrap().to_owned();
        assert_eq!(windowed["family"], "history");
        assert_eq!(windowed["summary_truncated"], true, "{windowed}");
        assert_eq!(windowed["body_bytes"], full.len());
        assert_ne!(windowed["summary"], full);
        assert_eq!(
            windowed["detail"],
            format!("engram work show {work} --note {locator}")
        );
        assert!(windowed.get("member").is_none(), "windows carry no member");

        let read = detail(&restored, &work, &locator).unwrap();
        let note = &read.value["note"];
        assert_eq!(note["locator"], locator);
        assert_eq!(note["summary"], full);
        assert_eq!(note["body_bytes"], full.len());
        // Every stored field, with the actor shown as the row's display
        // label rather than the raw stored identity.
        let mut displayed = member.clone();
        displayed["actor"] = note["by"].clone();
        assert!(note["by"].is_string());
        assert_eq!(note["member"], displayed);
        assert_ne!(note["member"]["actor"], member["actor"]);
        assert_eq!(
            note["member_bytes"],
            serde_json::to_string(&displayed).unwrap().len()
        );
        // Back to the history window the member came from.
        assert_eq!(
            read.value["next"][0],
            format!("engram work show {work} --history")
        );
        let text = read.text();
        assert!(text.contains(&format!("history {locator}: complete inherited")));
        assert!(text.contains("    member:"));
        // Framed as data: no raw control byte and no forged command line.
        assert!(!text.contains('\u{1b}') && !text.contains('\u{7}'));
        assert!(!text.lines().any(|line| line.starts_with("next: forged")));
    }

    // Refusals: malformed suffixes, a member that does not exist, and a
    // completion locator on an item that was never completed.
    let record = row(&event_suffix)["locator"]
        .as_str()
        .unwrap()
        .split_once(':')
        .unwrap()
        .0
        .to_owned();
    let event_count = expected_event_count(&document);
    for suffix in [
        "event-0".to_owned(),
        "event-x".into(),
        "event-".into(),
        "event-+1".into(),
        "+1".into(),
        "completions".into(),
        format!("event-{}", event_count + 1),
    ] {
        let error = detail(&restored, &work, &format!("{record}:{suffix}")).unwrap_err();
        assert!(
            matches!(error.error, StoreError::WorkNoteReferenceInvalid { .. }),
            "{suffix}: {:?}",
            error.error
        );
    }
    let open_history = traverse(&restored, &open, true, 130);
    let open_record = open_history
        .iter()
        .find_map(|row| row["locator"].as_str().unwrap().split_once(':'))
        .map(|(record, _)| record.to_owned())
        .expect("the open item has inherited rows");
    let error = detail(&restored, &open, &format!("{open_record}:completion")).unwrap_err();
    assert!(matches!(
        error.error,
        StoreError::WorkNoteReferenceInvalid { .. }
    ));
    // A member locator belongs to its own item only.
    let error = detail(&restored, &open, &format!("{record}{event_suffix}")).unwrap_err();
    assert!(matches!(
        error.error,
        StoreError::WorkNoteReferenceInvalid { .. }
    ));
}

fn expected_event_count(document: &crate::WorkGraphSnapshotDocument) -> usize {
    document
        .body
        .records
        .iter()
        .find_map(|record| match &record.payload {
            crate::WorkGraphSnapshotRecordPayload::Native { history }
                if history.completion.is_some() =>
            {
                Some(history.events.len())
            }
            _ => None,
        })
        .unwrap()
}
