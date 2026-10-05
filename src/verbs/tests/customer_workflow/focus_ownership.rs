//! Selected focus is navigation; a claim is execution ownership. Receipts
//! state the two as separate facts, and concurrent use of one session keeps
//! each call's own result.

use std::sync::Arc;

use super::*;

fn hold(verbs: &AgentVerbs, work: &str, second: i64) {
    verbs
        .claim(
            ClaimInput {
                work_ref: work.into(),
                ttl_seconds: Some(3_600),
                recover: None,
            },
            at(second),
        )
        .expect("claim");
}

fn focused_ref(database: &std::path::Path, project: &ProjectId, session: &str) -> Option<String> {
    let store = SqliteStore::open(database).unwrap();
    store
        .work_session_state(project, &SessionId(session.into()), at(0))
        .unwrap()
        .focused_work_id
        .map(|work_id| store.get_work_item(work_id).unwrap().short_ref)
}

// Two adds run at once through one shared session, as an MCP server runs
// them. Each receipt names its own item and the focus its own call set,
// whatever order they finish in; a following implicit-target note goes to the
// documented target, the item of the focus-setting operation committed last.
#[test]
fn concurrent_adds_keep_their_own_refs_and_the_implicit_target_is_the_last_committed() {
    let directory = crate::test_support::temp_home().expect("temp directory");
    let database = directory.path().join("work.sqlite3");
    let project = ProjectId("focus-ownership".into());
    let session = SessionId("agent".into());
    let service = Arc::new(LocalWorkService::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        session.clone(),
        None,
    ));
    let verbs =
        || AgentVerbs::with_shared_service(Arc::clone(&service), "agent".into(), session.clone());
    // Initialize the store before racing words on it.
    verbs().ls(&LsInput::default(), at(0)).ok();
    add(&verbs(), "Initial", None, false, 0);
    for round in 1..7_i64 {
        let barrier = Arc::new(std::sync::Barrier::new(2));
        let handles = ["Left", "Right"]
            .into_iter()
            .map(|side| {
                let words = verbs();
                let barrier = Arc::clone(&barrier);
                let title = format!("{side} {round}");
                std::thread::spawn(move || {
                    barrier.wait();
                    let receipt = words
                        .add(
                            AddInput {
                                title: title.clone(),
                                acceptance: vec![format!("{title} works")],
                                ..AddInput::default()
                            },
                            at(10 * round),
                        )
                        .expect("add");
                    (title, receipt)
                })
            })
            .collect::<Vec<_>>();
        // Collect in reverse spawn order: delivery order is irrelevant.
        let mut results = handles
            .into_iter()
            .map(|handle| handle.join().expect("add thread"))
            .collect::<Vec<_>>();
        results.reverse();
        for (title, receipt) in &results {
            assert_eq!(receipt.value["work"]["title"], json!(title));
            let created = &receipt.value["work"]["short_ref"];
            // The focus each call set is its own item, even if another call
            // has since moved the session elsewhere.
            assert_eq!(receipt.value["focus_change"]["to"], *created, "{title}");
            assert!(
                receipt
                    .text()
                    .contains(&format!("to {}; ", created.as_str().unwrap())),
                "{title}"
            );
        }
        let last = focused_ref(&database, &project, "agent").expect("a focus");
        assert!(
            results
                .iter()
                .any(|(_, receipt)| receipt.value["work"]["short_ref"] == json!(last))
        );
        let noted = verbs()
            .note(
                &NoteInput {
                    status: false,
                    work_ref: None,
                    text: format!("implicit note {round}"),
                    refs: Vec::new(),
                },
                at(10 * round + 1),
            )
            .expect("implicit note");
        assert_eq!(noted.value["work"]["short_ref"], json!(last));
    }
}

// A cancelled or superseded item stays the selected focus until an explicit
// focus-setting word moves it. Receipts name its terminal lifecycle apart from
// ownership, no successor is selected automatically, and its run supplies no
// live control binding.
#[test]
fn terminal_focus_stays_selected_without_ownership_successor_or_binding() {
    let (_directory, verbs, database, project) = fixture();
    let cancelled = add(&verbs, "Cancelled focus", None, false, 0);
    hold(&verbs, &cancelled, 1);
    verbs
        .update(
            UpdateInput {
                work_ref: Some(cancelled.clone()),
                action: UpdateAction::Cancel {
                    reason: "No longer needed".into(),
                },
            },
            at(2),
        )
        .unwrap();
    assert_terminal_focus(&verbs, &database, &project, &cancelled, "cancelled", 3);
    let superseded = add(&verbs, "Superseded focus", None, false, 4);
    let successor = add(&verbs, "Successor", None, false, 5);
    // Back to the item that is about to be superseded.
    hold(&verbs, &superseded, 6);
    verbs
        .update(
            serde_json::from_value(json!({"work_ref": superseded,
                "action": {"action": "supersede", "replacement": successor,
                           "reason": "Replaced by a narrower item"}}))
            .unwrap(),
            at(7),
        )
        .unwrap();
    assert_terminal_focus(&verbs, &database, &project, &superseded, "superseded", 8);
}

fn assert_terminal_focus(
    verbs: &AgentVerbs,
    database: &std::path::Path,
    project: &ProjectId,
    terminal: &str,
    state: &str,
    second: i64,
) {
    assert_eq!(
        focused_ref(database, project, "agent").as_deref(),
        Some(terminal),
        "no successor is selected automatically"
    );
    let next = verbs.next(&NextInput::default(), at(second)).unwrap();
    assert_eq!(next.value["focus"]["ref"], json!(terminal));
    assert_eq!(next.value["focus"]["state"], json!(state));
    assert!(next.value["focus"].get("holder").is_none());
    assert!(!next.text().contains(&format!("{terminal} held by you")));
    let shown = verbs.show(terminal, at(second)).unwrap();
    assert_eq!(shown.value["session_focus"], json!(terminal));
    assert!(shown.value.get("holder").is_none());
    assert!(
        shown
            .text()
            .contains("session focus: this item (selection only; the holder is stated above)")
    );
    let held = verbs.service.work_held(at(second)).unwrap();
    let store = SqliteStore::open(database).unwrap();
    assert_eq!(
        held.focused_work_id
            .map(|id| store.get_work_item(id).unwrap().short_ref)
            .as_deref(),
        Some(terminal)
    );
    // A terminal run never supplies a live control binding: the item has no
    // held row at all.
    assert!(held.items.iter().all(|row| row.short_ref != terminal));
}

// The session's focus and an item's holder are separate facts on next, show
// and a refusal: a focused, unclaimed item is never reported as held, and a
// held item that is not the focus is still reported as held.
#[test]
fn focus_and_holder_are_stated_as_two_facts() {
    let (_directory, verbs, database, project) = fixture();
    let held = add(&verbs, "Held elsewhere", None, false, 0);
    hold(&verbs, &held, 1);
    let selected = add(&verbs, "Selected only", None, false, 2);
    assert_eq!(
        focused_ref(&database, &project, "agent").as_deref(),
        Some(selected.as_str())
    );
    let next = verbs.next(&NextInput::default(), at(3)).unwrap();
    assert_eq!(next.value["focus"]["ref"], json!(selected));
    assert!(next.value["focus"].get("holder").is_none());
    assert!(
        next.value["held"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["ref"] == json!(held))
    );
    let shown_selected = verbs.show(&selected, at(3)).unwrap();
    assert!(shown_selected.value.get("holder").is_none());
    assert_eq!(shown_selected.value["session_focus"], json!(selected));
    assert!(!shown_selected.text().contains("held by you"));
    let shown_held = verbs.show(&held, at(3)).unwrap();
    assert_eq!(shown_held.value["holder"], "you");
    assert_eq!(shown_held.value["session_focus"], json!(selected));
    assert!(
        shown_held
            .text()
            .contains(&format!("session focus: {selected}, not this item"))
    );
    // A session with no focus says so.
    let other = AgentVerbs::new(
        database.clone(),
        project.clone(),
        "peer".into(),
        SessionId("peer".into()),
        None,
    );
    let unfocused = other.show(&held, at(3)).unwrap();
    assert_eq!(unfocused.value["session_focus"], Value::Null);
    assert!(unfocused.text().contains("session focus: none"));
    // An implicit completion of the focused, unclaimed item is refused, and
    // the refusal never says the session holds it.
    let refused = verbs
        .done(
            DoneInput {
                summary: Some("delivered".into()),
                ..DoneInput::default()
            },
            at(4),
        )
        .expect_err("no claim on the focus");
    let text = refused.to_string();
    assert!(!text.contains(&format!("{selected} held by you")), "{text}");
}

// Reads leave the focus where it is, and a mutation given an explicit ref
// acts on that ref whatever the focus.
#[test]
fn reads_never_move_focus_and_explicit_refs_never_consult_it() {
    let (_directory, verbs, database, project) = fixture();
    let first = add(&verbs, "First", None, false, 0);
    let second = add(&verbs, "Second", None, false, 1);
    verbs
        .remember(
            serde_json::from_value(json!({"text": "A note", "key": "focus-key"})).unwrap(),
            at(2),
        )
        .unwrap();
    let focus = || focused_ref(&database, &project, "agent");
    assert_eq!(focus().as_deref(), Some(second.as_str()));
    verbs.show(&first, at(3)).unwrap();
    verbs.ls(&LsInput::default(), at(3)).unwrap();
    verbs.search("First", None, at(3)).unwrap();
    verbs.next(&NextInput::default(), at(3)).unwrap();
    verbs
        .next(
            &NextInput {
                peek: true,
                ..NextInput::default()
            },
            at(3),
        )
        .unwrap();
    verbs.memories(&MemoriesInput::default(), at(3)).unwrap();
    verbs
        .show_records(
            &first,
            &ShowInput {
                history: true,
                ..ShowInput::default()
            },
            at(3),
        )
        .unwrap();
    assert_eq!(focus().as_deref(), Some(second.as_str()));
    let noted = verbs
        .note(
            &NoteInput {
                status: false,
                work_ref: Some(first.clone()),
                text: "explicit note".into(),
                refs: Vec::new(),
            },
            at(4),
        )
        .unwrap();
    assert_eq!(noted.value["work"]["short_ref"], json!(first));
}
