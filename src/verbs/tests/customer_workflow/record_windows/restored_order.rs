use super::*;

// Inherited history is listed in a deterministic presentation order, never
// as a chronology: each generation's notes, then its events, then its
// completion, each in the order the record stores them. The carried timestamps are only data, so
// times far outside the snapshot cut and the load time, running backward or
// repeating, change neither the order nor which member a locator names.
#[test]
fn restored_history_order_ignores_carried_timestamps() {
    let (directory, verbs, path, project) = fixture();
    let work = add(&verbs, "Carried order", None, false, 0);
    let written = ["first", "second", "third", "fourth"];
    // Completing the item stores its summary as one more note.
    let summaries = ["first", "second", "third", "fourth", "delivered"];
    for (index, summary) in written.iter().enumerate() {
        note(&verbs, &work, summary, i64::try_from(index).unwrap() + 1);
    }
    for index in 0..2 {
        verbs
            .update(
                UpdateInput {
                    work_ref: Some(work.clone()),
                    action: UpdateAction::Revise {
                        bindings: None,
                        clear_external: false,
                        external: None,
                        title: Some(format!("Carried order {index}")),
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
                at(10 + index),
            )
            .unwrap();
    }
    verbs
        .claim(
            ClaimInput {
                work_ref: work.clone(),
                ttl_seconds: Some(600),
                recover: None,
            },
            at(20),
        )
        .unwrap();
    verbs
        .done(
            DoneInput {
                work_ref: Some(work.clone()),
                summary: Some("delivered".into()),
                ..DoneInput::default()
            },
            at(21),
        )
        .unwrap();
    let document = super::super::review::snapshot(&path, &project, &work);
    let mut skewed = document.clone();
    let record = skewed
        .body
        .records
        .iter_mut()
        .find_map(|record| match &mut record.payload {
            crate::WorkGraphSnapshotRecordPayload::Native { history } => Some(history),
            crate::WorkGraphSnapshotRecordPayload::Restored { .. } => None,
        })
        .expect("native history");
    assert_eq!(record.notes.len(), summaries.len());
    assert!(
        record.events.len() > 1,
        "the item has creation and note events"
    );
    // Far in the future, far in the past, a repeat, and backward again.
    let times = [
        at(4_000_000_000),
        at(-4_000_000_000),
        at(-4_000_000_000),
        at(-5),
    ];
    for (note, time) in record.notes.iter_mut().zip(times) {
        note.recorded_at = time;
    }
    let event_count = record.events.len();
    for (index, event) in record.events.iter_mut().enumerate() {
        // Events run backward: the first is the latest.
        event.occurred_at = at(3_000_000_000 - i64::try_from(index).unwrap());
    }
    // The completion claims to come before everything else.
    record
        .completion
        .as_mut()
        .expect("a carried completion")
        .completed_at = at(-9_000_000_000);
    skewed.manifest.body_sha256 = crate::CanonicalObject::freeze(&skewed.body)
        .unwrap()
        .key()
        .clone();

    let read = |document: &crate::WorkGraphSnapshotDocument, name: &str| {
        let home = directory.path().join(name);
        std::fs::create_dir_all(&home).unwrap();
        let (restored, _store, _path) = super::super::review::load(&home, document);
        // Each load mints its own record id; the member after the colon
        // names which stored member a locator reads.
        let member = |row: &serde_json::Value| {
            let locator = row["locator"].as_str().unwrap();
            let (_, member) = locator.split_once(':').expect("an inherited locator");
            member.to_owned()
        };
        let notes = traverse(&restored, &work, false, 130)
            .into_iter()
            .map(|row| (row["summary"].clone(), member(&row)))
            .collect::<Vec<_>>();
        let history = traverse(&restored, &work, true, 130)
            .into_iter()
            .map(|row| (row["kind"].clone(), row["summary"].clone(), member(&row)))
            .collect::<Vec<_>>();
        let focus = restored
            .service
            .work_focus_for_agent(&work, at(130))
            .unwrap()
            .restored_history;
        let tail = focus
            .items
            .iter()
            .map(|entry| (entry.kind.clone(), entry.generation_index))
            .collect::<Vec<_>>();
        (notes, history, focus.total, tail)
    };
    let (notes, history, total, tail) = read(&document, "ordered");
    let (skewed_notes, skewed_history, skewed_total, skewed_tail) = read(&skewed, "skewed");

    // Notes keep their stored order and their locators.
    assert_eq!(
        notes
            .iter()
            .map(|(summary, _)| summary.clone())
            .collect::<Vec<_>>(),
        summaries.map(serde_json::Value::from)
    );
    assert_eq!(skewed_notes, notes);
    // History lists the notes first, then the events, then the completion,
    // whatever the times.
    assert_eq!(history.len(), summaries.len() + event_count + 1);
    let (last_kind, _, last_member) = history.last().unwrap();
    assert_eq!(last_kind, "completed");
    assert_eq!(last_member, "completion");
    assert!(
        history[summaries.len()..history.len() - 1]
            .iter()
            .zip(1..)
            .all(|((_, _, member), index)| *member == format!("event-{index}")),
        "{history:?}"
    );
    assert!(
        history[..summaries.len()]
            .iter()
            .zip(summaries)
            .all(|((_, summary, _), expected)| summary == expected),
        "{history:?}"
    );
    assert_eq!(skewed_history, history);
    // The focus view's restored history agrees: same total, same tail.
    assert_eq!(total, summaries.len() + event_count + 1);
    assert_eq!(skewed_total, total);
    assert_eq!(skewed_tail, tail);
    let history_tail = history[history.len() - tail.len()..]
        .iter()
        .map(|(kind, _, _)| kind.clone())
        .collect::<Vec<_>>();
    // The focus view names a peer's note "non_holder_note"; the window
    // gives its evidence kind and flags it non_holder instead.
    assert_eq!(
        tail.iter()
            .map(|(kind, _)| {
                serde_json::Value::from(if kind == "non_holder_note" {
                    "generic"
                } else {
                    kind.as_str()
                })
            })
            .collect::<Vec<_>>(),
        history_tail
    );
    // The completion stays last in the focus view too.
    assert_eq!(tail.last().unwrap().0, "completed");
}
