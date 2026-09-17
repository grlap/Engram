use super::*;

fn assert_agent_surfaces_fit(receipt: &Receipt) {
    let emitted = emitted_receipt_bytes(receipt);
    let terminal = format!("{}\n", receipt.text()).len();
    assert!(
        emitted < MAX_AGENT_WORK_RESPONSE_BYTES,
        "emitted={emitted} terminal={terminal} compact={}",
        compact_json_len(&receipt.value)
    );
}

#[test]
fn show_window_measures_compact_json_and_gains_rows_against_pretty() {
    let (_directory, verbs, _, _) = fixture();
    let work = add(&verbs, "Window bytes", None, false, 0);
    let bodies: Vec<String> = (0..24)
        .map(|index| format!("Note {index}: {} END {index}", "body ".repeat(400)))
        .collect();
    for (index, body) in bodies.iter().enumerate() {
        note(
            &verbs,
            &work,
            body,
            2 + i64::try_from(index).expect("index"),
        );
    }
    let page = verbs
        .show_with_notes(&work, true, at(100))
        .expect("notes window");
    let notes = page.value["notes"].as_array().expect("notes");
    assert!(!notes.is_empty());
    assert_eq!(page.value["notes_omitted"], bodies.len() - notes.len());
    assert_eq!(
        notes.last().unwrap()["summary"],
        *bodies.last().unwrap(),
        "newest retained note stays on the page"
    );
    assert!(format!("{}\n", page.text()).len() < MAX_AGENT_WORK_RESPONSE_BYTES);
    let compact = serde_json::to_vec(&page.value).expect("compact").len();
    assert!(compact < MAX_AGENT_WORK_RESPONSE_BYTES);
    let pretty = serde_json::to_vec_pretty(&page.value)
        .expect("pretty")
        .len();
    assert!(compact < pretty, "compact JSON is the delivered measure");
    assert!(
        pretty >= MAX_AGENT_WORK_RESPONSE_BYTES,
        "this multi-row page exceeds the pretty measure; shown={} pretty={pretty}",
        notes.len()
    );
    assert_eq!(
        page.text().matches("Note ").count(),
        notes.len(),
        "text and JSON retain the same window rows"
    );
}

#[test]
fn show_window_measures_hostile_controls_without_dropping_the_page() {
    let (_directory, verbs, _, _) = fixture();
    let work = add(&verbs, "Hostile window", None, false, 0);
    let hostile = format!(
        "marker\u{0001}esc\u{009b}bell\u{0007} {}",
        "payload ".repeat(300)
    );
    note(&verbs, &work, &hostile, 1);
    note(&verbs, &work, &format!("plain {}", "x".repeat(200)), 2);
    let page = verbs
        .show_with_notes(&work, true, at(10))
        .expect("hostile notes");
    let notes = page.value["notes"].as_array().expect("notes");
    assert_eq!(notes.len(), 2);
    assert_eq!(page.value["notes_omitted"], 0);
    assert!(notes.iter().any(|note| {
        note["summary"]
            .as_str()
            .unwrap()
            .contains("marker\u{0001}esc")
    }));
    assert!(page.text().contains("marker"));
    assert!(!page.text().contains('\u{9b}'));
    assert_agent_surfaces_fit(&page);
}

#[test]
fn verbose_next_pages_twenty_kib_of_changes_exactly() {
    let (_directory, reader, path, project) = fixture();
    let peer = AgentVerbs::new(
        path.clone(),
        project,
        "peer".into(),
        SessionId("peer".into()),
        None,
    );
    let work = add(&peer, "Shared change root", None, false, 0);
    let mut raw_bytes = 0usize;
    for index in 0..40 {
        let body = format!("Peer note {index} {}", "x".repeat(500));
        raw_bytes += body.len();
        note(&peer, &work, &body, index + 1);
    }
    assert!(
        raw_bytes >= 20 * 1024,
        "fixture must exceed 20 KiB of change material: {raw_bytes}"
    );

    let inspect = rusqlite::Connection::open(&path).unwrap();
    let before_peek = crate::storage::test_database_shape_snapshot(&inspect).unwrap();
    let peeked = reader
        .next(
            &NextInput {
                verbose: true,
                peek: true,
                limit: Some(20),
                ..NextInput::default()
            },
            at(200),
        )
        .expect("peek");
    assert_agent_surfaces_fit(&peeked);
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&inspect).unwrap(),
        before_peek,
        "peek must not advance delivery"
    );
    let peek_positions = change_positions(&peeked.value);

    let mut delivered = Vec::new();
    let mut change_bytes = 0usize;
    let mut now = 201;
    let mut first = true;
    loop {
        let page = reader
            .next(
                &NextInput {
                    verbose: true,
                    limit: Some(20),
                    ..NextInput::default()
                },
                at(now),
            )
            .expect("verbose page");
        assert_agent_surfaces_fit(&page);
        let positions = change_positions(&page.value);
        if first {
            if !peek_positions.is_empty() {
                assert_eq!(positions, peek_positions);
            }
            first = false;
        }
        if let Some(changes) = page.value.get("changes") {
            change_bytes += compact_json_len(changes);
        }
        if positions.is_empty() && change_omitted(&page.value) == 0 {
            break;
        }
        delivered.extend(positions);
        now += 1;
        assert!(now < 250, "dense traversal should finish");
    }
    assert!(
        change_bytes >= 20 * 1024,
        "delivered change pages must cover at least 20 KiB: {change_bytes}"
    );
    assert_eq!(delivered.len(), 41);
    assert!(
        delivered.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "raw feed positions must stay dense and unique: {delivered:?}"
    );
}

fn change_positions(value: &Value) -> Vec<i64> {
    value
        .get("changes")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .filter_map(|change| change["entry"]["position"]["position"].as_i64())
        .collect()
}

fn change_omitted(value: &Value) -> usize {
    value
        .get("omissions")
        .and_then(Value::as_array)
        .into_iter()
        .flatten()
        .find(|entry| entry["section"] == "changes")
        .and_then(|entry| entry["omitted_count"].as_u64())
        .and_then(|count| usize::try_from(count).ok())
        .unwrap_or(0)
}

#[test]
fn process_default_session_is_fitted_before_mutation_emission() {
    let (directory, verbs, path, project) = fixture();
    let session = SessionId("local-process-v1-fitted".into());
    let fitted = AgentVerbs::with_shared_service(
        verbs.service.clone(),
        "agent".into(),
        SessionId("agent".into()),
    )
    .with_fitted_effective_session(session.clone());
    let added = fitted
        .add(
            AddInput {
                title: "Fitted session".into(),
                ..AddInput::default()
            },
            at(1),
        )
        .expect("add");
    assert_eq!(added.value["effective_session_id"], session.0);
    assert!(format!("{}\n", added.text()).len() < MAX_AGENT_WORK_RESPONSE_BYTES);
    assert!(
        serde_json::to_vec(&added.value).expect("add compact").len()
            < MAX_AGENT_WORK_RESPONSE_BYTES
    );
    let explicit = AgentVerbs::new(
        path,
        project,
        "agent".into(),
        SessionId("explicit-session".into()),
        None,
    );
    let explicit_add = explicit
        .add(
            AddInput {
                title: "Explicit session".into(),
                ..AddInput::default()
            },
            at(2),
        )
        .expect("explicit add");
    assert!(explicit_add.value.get("effective_session_id").is_none());
    let next = fitted
        .next(&NextInput::default(), at(3))
        .expect("read receipt");
    assert!(next.value.get("effective_session_id").is_none());
    let owed_parent = add(&fitted, "Owed parent", None, false, 4);
    add(&fitted, "Required child", Some(&owed_parent), false, 5);
    fitted
        .claim(
            ClaimInput {
                work_ref: owed_parent.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(6),
        )
        .expect("claim owed parent");
    note(&fitted, &owed_parent, "cannot seal yet", 7);
    let owed = fitted
        .done(
            DoneInput {
                source_fingerprint: None,
                links: Vec::new(),
                link_basis: None,
                work_ref: Some(owed_parent),
                summary: Some("still owed".into()),
                note: None,
            },
            at(8),
        )
        .expect("owed done");
    assert!(owed.owed, "{}", owed.text());
    assert!(owed.value.get("effective_session_id").is_none());
    drop((fitted, explicit, verbs));
    drop(directory);
}

fn late_append_session(receipt: &Receipt, session: &SessionId) -> Receipt {
    Receipt::assemble(
        receipt.lines.clone(),
        crate::verbs::Guidance {
            reminders: receipt.reminders.clone(),
            next: receipt.next.clone(),
        },
        receipt.value.clone(),
        receipt.owed,
    )
    .with_effective_session_id(session)
}

fn mutation_fitter_base() -> Receipt {
    Receipt::assemble(
        vec!["done parent".into()],
        crate::verbs::Guidance::default(),
        json!({"completed": true}),
        false,
    )
}

#[test]
fn process_default_session_is_reserved_inside_mutation_fitting() {
    let (_directory, verbs, path, project) = fixture();
    let session = SessionId(crate::new_process_default_work_session_id());
    let parent = add(&verbs, "Budget parent", None, false, 0);
    for index in 1..=5 {
        add(
            &verbs,
            &format!("{index} {}", "child ".repeat(20)),
            Some(&parent),
            true,
            index,
        );
    }
    verbs
        .claim(
            ClaimInput {
                work_ref: parent.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(6),
        )
        .expect("claim");
    note(&verbs, &parent, "holder progress", 7);
    verbs
        .done(
            DoneInput {
                source_fingerprint: None,
                links: Vec::new(),
                link_basis: None,
                work_ref: Some(parent.clone()),
                summary: Some("Delivered".into()),
                note: None,
            },
            at(8),
        )
        .expect("done");
    let service = crate::work_service::LocalWorkService::new(
        path,
        project,
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let work = service.resolve_work_reference(&parent, at(9)).unwrap();
    let children = service.remaining_optional_children(work.work_id, 5, at(9));
    let render = |base: Receipt, budget| {
        crate::verbs::child_obligations::done_with_child_obligations(
            base.lines,
            crate::verbs::Guidance {
                reminders: base.reminders,
                next: base.next,
            },
            base.value,
            &children,
            &parent,
            budget,
        )
        .expect("fit")
    };
    let full = render(mutation_fitter_base(), MAX_AGENT_WORK_RESPONSE_BYTES);
    let full_text = format!("{}\n", full.text()).len();
    let full_compact = serde_json::to_vec(&full.value).expect("full").len();
    let late = late_append_session(&full, &session);
    let late_text = format!("{}\n", late.text()).len();
    let late_compact = serde_json::to_vec(&late.value).expect("late").len();
    let session_cost = late_compact.saturating_sub(full_compact);
    assert!(
        session_cost > 0,
        "real process-default id must add compact JSON bytes compact={full_compact} late={late_compact}"
    );
    let budget = if full_text < full_compact + 1 {
        full_compact + 1
    } else {
        full_text.max(full_compact) + 1
    };
    assert!(
        full_text < budget,
        "full emitted text sits under the derived budget text={full_text} budget={budget}"
    );
    assert!(full_compact < budget);
    assert!(
        late_text >= budget || late_compact >= budget,
        "late append of the real default id exceeds the derived budget text={late_text} compact={late_compact} budget={budget} session_cost={session_cost}"
    );
    assert!(full.value.get("effective_session_id").is_none());
    let full_shown = full.value["child_obligations"]["open_optional"]["items"]
        .as_array()
        .expect("full rows")
        .len();
    assert_eq!(full_shown, 5, "unreserved candidate keeps every child row");
    let fitted = render(
        mutation_fitter_base().with_effective_session_id(&session),
        budget,
    );
    assert_eq!(fitted.value["effective_session_id"], session.0);
    assert!(emitted_receipt_bytes(&fitted) < budget);
    assert!(serde_json::to_vec(&fitted.value).expect("fitted").len() < budget);
    let fitted_shown = fitted.value["child_obligations"]["open_optional"]["items"]
        .as_array()
        .expect("fitted rows")
        .len();
    assert!(
        fitted_shown < full_shown,
        "reserved default id must shed rows full={full_shown} fitted={fitted_shown} session_cost={session_cost}"
    );
}

fn session_effect_snapshot(
    path: &std::path::Path,
    project: &ProjectId,
    session: &SessionId,
    now: chrono::DateTime<Utc>,
) -> (Option<String>, i64, Option<i64>, i64, i64, i64) {
    let store = crate::SqliteStore::open(path).expect("inspect");
    let state = store
        .work_session_state(project, session, now)
        .expect("session state");
    drop(store);
    let connection = rusqlite::Connection::open(path).expect("counts");
    let attempts: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM work_protocol_attempts WHERE session_id = ?1",
            [session.0.as_str()],
            |row| row.get(0),
        )
        .expect("attempts");
    let offers: i64 = connection
        .query_row("SELECT COUNT(*) FROM work_handoff_offers", [], |row| {
            row.get(0)
        })
        .expect("offers");
    let session_rows: i64 = connection
        .query_row(
            "SELECT COUNT(*) FROM work_session_state WHERE session_id = ?1",
            [session.0.as_str()],
            |row| row.get(0),
        )
        .expect("session rows");
    (
        state.focused_work_id.map(|id| id.0.to_string()),
        state.project_cursor,
        state.tentative_project_cursor,
        attempts,
        offers,
        session_rows,
    )
}

fn assert_session_admission_refusal(error: &VerbError, giant: &str) {
    assert!(
        matches!(
            &error.error,
            StoreError::InvalidWork(reason) if reason == crate::SessionIdAdmissionError::TooLong.as_str()
        ),
        "{error}"
    );
    let message = error.to_string();
    assert!(
        message.contains(crate::SessionIdAdmissionError::TooLong.as_str()),
        "{message}"
    );
    assert!(!message.contains(giant));
}

#[test]
fn oversized_session_is_refused_before_store_effects() {
    let (_directory, peer, path, project) = fixture();
    add(&peer, "Shared change root", None, false, 0);
    let giant = "s".repeat(65);
    let reader = AgentVerbs::new(
        path.clone(),
        project.clone(),
        "reader".into(),
        SessionId(giant.clone()),
        None,
    );
    let before = session_effect_snapshot(&path, &project, &SessionId(giant.clone()), at(199));
    assert_eq!(before.5, 0, "giant session must have no row yet");
    let error = reader
        .next(
            &NextInput {
                verbose: true,
                peek: true,
                limit: Some(20),
                ..NextInput::default()
            },
            at(200),
        )
        .expect_err("oversized caller");
    assert_session_admission_refusal(&error, &giant);
    assert!(
        format!("{:?}", reader.service).contains("store_initialized: false"),
        "peek must not open a store for a refused session"
    );
    let after = session_effect_snapshot(&path, &project, &SessionId(giant.clone()), at(201));
    assert_eq!(after, before);
    assert_eq!(after.5, 0);
}

#[test]
fn admitted_max_session_id_is_preserved_on_verbose_next() {
    let (_directory, seed, path, project) = fixture();
    add(&seed, "Seed store", None, false, 0);
    let session = "s".repeat(crate::MAX_SESSION_ID_BYTES);
    let reader = AgentVerbs::new(
        path,
        project,
        "reader".into(),
        SessionId(session.clone()),
        None,
    );
    let page = reader
        .next(
            &NextInput {
                verbose: true,
                peek: true,
                ..NextInput::default()
            },
            at(1),
        )
        .expect("admitted session");
    assert_eq!(page.value["session"]["session_id"], session);
    assert_agent_surfaces_fit(&page);
}

#[test]
fn oversized_handoff_recipient_is_refused_before_offer_effects() {
    let (_directory, verbs, path, project) = fixture();
    let first = add(&verbs, "First claimed", None, false, 0);
    let second = add(&verbs, "Second selected", None, false, 1);
    verbs
        .claim(
            ClaimInput {
                work_ref: first.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(2),
        )
        .expect("claim first");
    verbs
        .claim(
            ClaimInput {
                work_ref: second.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(3),
        )
        .expect("select second");
    let session = SessionId("agent".into());
    let before = session_effect_snapshot(&path, &project, &session, at(4));
    assert!(before.0.is_some(), "second work must be focused");
    let giant = "t".repeat(65);
    let error = verbs
        .handoff(
            HandoffInput {
                work_ref: Some(first),
                action: HandoffAction::Offer {
                    to: giant.clone(),
                    summary: Some("Transfer context".into()),
                    ttl_seconds: Some(60),
                },
            },
            at(5),
        )
        .expect_err("oversized recipient");
    assert_session_admission_refusal(&error, &giant);
    let after = session_effect_snapshot(&path, &project, &session, at(6));
    assert_eq!(
        after, before,
        "focus, cursors, attempts, and offers stay put"
    );
    let shown = verbs.show(&second, at(7)).expect("second still readable");
    assert!(shown.text().contains("Second selected"));
}

#[test]
fn oversized_handoff_recipient_does_not_move_focus() {
    let (_directory, verbs, _, _) = fixture();
    let focused = add(&verbs, "Already focused", None, false, 0);
    let other = add(&verbs, "Other target", None, false, 1);
    verbs
        .claim(
            ClaimInput {
                work_ref: focused.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(2),
        )
        .expect("claim focused");
    let before = verbs
        .next(
            &NextInput {
                verbose: true,
                peek: true,
                ..NextInput::default()
            },
            at(3),
        )
        .expect("before");
    let focused_id = before.value["session"]["focused_work_id"]
        .as_str()
        .expect("focus")
        .to_owned();
    let giant = "t".repeat(65);
    let error = verbs
        .handoff(
            HandoffInput {
                work_ref: Some(other),
                action: HandoffAction::Offer {
                    to: giant.clone(),
                    summary: Some("should not bind focus".into()),
                    ttl_seconds: Some(60),
                },
            },
            at(4),
        )
        .expect_err("oversized recipient");
    assert_session_admission_refusal(&error, &giant);
    let after = verbs
        .next(
            &NextInput {
                verbose: true,
                peek: true,
                ..NextInput::default()
            },
            at(5),
        )
        .expect("after");
    assert_eq!(after.value["session"]["focused_work_id"], focused_id);
    assert_eq!(after.value["focus"]["status"]["work"]["short_ref"], focused);
}

#[test]
fn verbose_next_fits_a_large_ready_catalog_without_truncating_changes() {
    const READY_CANDIDATES: usize = 40;
    let (_directory, reader, path, project) = fixture();
    let peer = AgentVerbs::new(path, project, "peer".into(), SessionId("peer".into()), None);
    for index in 0..READY_CANDIDATES {
        peer.add(
            AddInput {
                title: format!("Ready {index} {}", "quote\" ".repeat(80)),
                ..AddInput::default()
            },
            at(i64::try_from(index).expect("index")),
        )
        .expect("ready root");
    }
    let page = reader
        .next(
            &NextInput {
                verbose: true,
                limit: Some(100),
                ..NextInput::default()
            },
            at(50),
        )
        .expect("verbose");
    assert_agent_surfaces_fit(&page);
    let shown = page.value["ready"].as_array().map_or(0, Vec::len);
    let ready_budget = byte_budget_omissions(&page.value, "omissions", "ready");
    assert_eq!(
        ready_budget.len(),
        1,
        "one merged Ready/ByteBudget omission: {ready_budget:?}"
    );
    let omitted = omitted_count(&ready_budget[0]);
    assert!(
        omitted > 0,
        "ready shedding must actually omit rows; shown={shown}"
    );
    assert_eq!(shown + omitted, READY_CANDIDATES);
    assert!(
        page.text()
            .contains(&format!("  ({omitted} more ready items not shown)")),
        "{}",
        page.text()
    );
    let positions = change_positions(&page.value);
    assert!(!positions.is_empty());
    assert!(
        positions.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "staged change page stays dense: {positions:?}"
    );
}

fn section_omissions(value: &Value, field: &str, section: &str) -> Vec<Value> {
    value[field]
        .as_array()
        .into_iter()
        .flatten()
        .filter(|entry| entry["section"] == section)
        .cloned()
        .collect()
}

fn byte_budget_omissions(value: &Value, field: &str, section: &str) -> Vec<Value> {
    section_omissions(value, field, section)
        .into_iter()
        .filter(|entry| entry["reason"] == "byte_budget")
        .collect()
}

fn omitted_count(entry: &Value) -> usize {
    usize::try_from(entry["omitted_count"].as_u64().expect("omitted_count"))
        .expect("omitted fits usize")
}

fn focus_section_omissions(value: &Value, field: &str) -> Vec<Value> {
    section_omissions(value, field, "focus")
}

#[test]
fn verbose_next_retains_focus_for_an_oversized_defaulted_title() {
    let (_directory, reader, _, _) = fixture();
    let title = format!("Defaulted title {}", "T".repeat(400));
    assert!(title.len() > 192);
    let focused = reader
        .add(
            AddInput {
                title: title.clone(),
                ..AddInput::default()
            },
            at(0),
        )
        .expect("oversized defaulted title")
        .value["work"]["short_ref"]
        .as_str()
        .expect("ref")
        .to_owned();
    reader
        .claim(
            ClaimInput {
                work_ref: focused.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(1),
        )
        .expect("claim");
    let page = reader
        .next(
            &NextInput {
                verbose: true,
                limit: Some(20),
                ..NextInput::default()
            },
            at(2),
        )
        .expect("verbose next");
    assert_agent_surfaces_fit(&page);
    let focus_line = page
        .text()
        .lines()
        .find(|line| line.starts_with("focus:"))
        .expect("focus line")
        .to_owned();
    assert_ne!(focus_line, "focus: none");
    assert_ne!(focus_line, "focus: omitted (byte budget)");
    assert!(focus_line.contains(&focused), "{focus_line}");
    assert!(!page.value["focus"].is_null());
    let focus_outcome = page.value["focus"]["outcome"]
        .as_str()
        .expect("focus.outcome");
    assert!(focus_outcome.len() <= 192, "{}", focus_outcome.len());
    // Independent of product compact_text: ASCII ellipsis is three dots after
    // a 189-byte prefix of the defaulted stored outcome (the title).
    let expected = format!("{}...", &title[..189]);
    assert_eq!(expected.len(), 192);
    assert_eq!(focus_outcome, expected);
}

#[test]
fn verbose_next_omits_an_oversized_focused_title_without_lying_none() {
    // Measured retained-focus emission for this fixture at the protocol
    // ceiling was 10790 bytes. Inject a named overlay budget below that
    // measurement. This does not prove production-default 12288 whole-focus
    // omission after Summary outcome compact.
    const VERBOSE_FOCUS_OMISSION_BUDGET: usize = 10_000;
    let (_directory, reader, path, project) = fixture();
    let title = format!("Focused title {}", "T".repeat(400));
    let focused = reader
        .add(
            AddInput {
                title: title.clone(),
                // Summary outcome is compact_text (192). That no longer forces
                // whole-focus removal under the 12 KiB overlay. Remaining
                // untrimmable summary fields plus the staged change page still
                // omit focus when the overlay ceiling is tighter; production
                // next keeps the protocol budget.
                outcome: Some(format!("Bounded outcome {}", "O".repeat(200))),
                acceptance: (0..6)
                    .map(|index| format!("Criterion {index} {}", "C".repeat(200)))
                    .collect(),
                labels: (0..8)
                    .map(|index| format!("label-{index}-{}", "L".repeat(200)))
                    .collect(),
                assignee: Some(format!("assignee {}", "A".repeat(200))),
                ..AddInput::default()
            },
            at(0),
        )
        .expect("oversized focused title")
        .value["work"]["short_ref"]
        .as_str()
        .expect("ref")
        .to_owned();
    reader
        .claim(
            ClaimInput {
                work_ref: focused.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(1),
        )
        .expect("claim focused title");
    for index in 0..8 {
        reader
            .add(
                AddInput {
                    title: format!("Trim child {index} {}", "K".repeat(200)),
                    under: Some(focused.clone()),
                    ..AddInput::default()
                },
                at(2 + i64::from(index)),
            )
            .expect("trim-step child");
    }
    for index in 0..8 {
        note(
            &reader,
            &focused,
            &format!("Focus history {index} {}", "H".repeat(400)),
            12 + i64::from(index),
        );
    }
    let peer = AgentVerbs::new(path, project, "peer".into(), SessionId("peer".into()), None);
    let shared = add(&peer, "Shared change root", None, false, 30);
    for index in 0..40 {
        note(
            &peer,
            &shared,
            &format!("Peer note {index} {}", "x".repeat(500)),
            31 + i64::from(index),
        );
    }

    let peeked = reader
        .next(
            &NextInput {
                verbose: true,
                peek: true,
                limit: Some(20),
                ..NextInput::default()
            },
            at(80),
        )
        .expect("peek verbose");
    assert_agent_surfaces_fit(&peeked);
    let peek_text = peeked.text();
    let peek_focus = peek_text
        .lines()
        .find(|line| line.starts_with("focus:"))
        .expect("peek focus line");
    assert_ne!(peek_focus, "focus: none");
    if peek_focus == "focus: omitted (byte budget)" {
        let peek_whole = focus_section_omissions(&peeked.value, "preview_omissions");
        assert_eq!(peek_whole.len(), 1, "{peek_whole:?}");
        assert_eq!(peek_whole[0]["omitted_count"], 1);
        assert!(peeked.value["focus"].is_null());
    } else {
        assert!(peek_focus.contains(&focused));
        assert!(!peeked.value["focus"].is_null());
    }

    let page = reader
        .next_with_verbose_budget(
            &NextInput {
                verbose: true,
                limit: Some(20),
                ..NextInput::default()
            },
            at(81),
            VERBOSE_FOCUS_OMISSION_BUDGET,
        )
        .expect("non-peek verbose");
    assert_agent_surfaces_fit(&page);
    assert!(
        emitted_receipt_bytes(&page) < VERBOSE_FOCUS_OMISSION_BUDGET,
        "injected overlay budget is the independent oracle, not the 12288 protocol ceiling"
    );
    let page_text = page.text();
    let focus_line = page_text
        .lines()
        .find(|line| line.starts_with("focus:"))
        .expect("focus line");
    assert_eq!(focus_line, "focus: omitted (byte budget)");
    assert!(page.value["focus"].is_null());
    let whole_focus = focus_section_omissions(&page.value, "agent_omissions");
    assert_eq!(whole_focus.len(), 1, "{whole_focus:?}");
    assert_eq!(whole_focus[0]["omitted_count"], 1);
    assert_eq!(whole_focus[0]["reason"], "byte_budget");
    let trim_focus = focus_section_omissions(&page.value, "omissions");
    assert!(
        !trim_focus.is_empty(),
        "trim-step Focus counts belong in omissions, distinct from the one whole-focus omission"
    );
    assert!(
        trim_focus
            .iter()
            .all(|entry| entry["reason"] == "byte_budget"),
        "trim-step Focus counts stay in omissions: {trim_focus:?}"
    );
    let positions = change_positions(&page.value);
    assert!(!positions.is_empty());
    assert!(
        positions.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "staged change page stays dense: {positions:?}"
    );
    assert!(page.value["delivery_token"].as_str().is_some());
}

#[test]
fn verbose_next_sheds_held_rows_without_popping_staged_changes() {
    const HELD_CANDIDATES: usize = 20;
    let (_directory, reader, path, project) = fixture();
    for index in 0..HELD_CANDIDATES {
        let work = reader
            .add(
                AddInput {
                    title: format!("Held {index} {}", "quote\" ".repeat(80)),
                    ..AddInput::default()
                },
                at(i64::try_from(index).expect("index")),
            )
            .expect("held root")
            .value["work"]["short_ref"]
            .as_str()
            .expect("ref")
            .to_owned();
        reader
            .claim(
                ClaimInput {
                    work_ref: work,
                    ttl_seconds: None,
                    recover: None,
                },
                at(100 + i64::try_from(index).expect("index")),
            )
            .expect("claim held");
    }
    let peer = AgentVerbs::new(path, project, "peer".into(), SessionId("peer".into()), None);
    let shared = add(&peer, "Shared change root", None, false, 200);
    let mut raw_bytes = 0usize;
    for index in 0..40 {
        let body = format!("Peer note {index} {}", "x".repeat(500));
        raw_bytes += body.len();
        note(&peer, &shared, &body, 201 + i64::from(index));
    }
    assert!(
        raw_bytes >= 20 * 1024,
        "fixture must exceed 20 KiB of change material: {raw_bytes}"
    );

    let page = reader
        .next(
            &NextInput {
                verbose: true,
                limit: Some(100),
                ..NextInput::default()
            },
            at(300),
        )
        .expect("non-peek verbose held");
    assert_agent_surfaces_fit(&page);
    let shown = page.value["held"].as_array().map_or(0, Vec::len);
    let held_budget = byte_budget_omissions(&page.value, "agent_omissions", "held");
    assert_eq!(
        held_budget.len(),
        1,
        "one merged held/ByteBudget omission: {held_budget:?}"
    );
    let omitted = omitted_count(&held_budget[0]);
    assert!(
        omitted > 0,
        "held shedding must actually omit rows; shown={shown}"
    );
    assert_eq!(shown + omitted, HELD_CANDIDATES);
    let page_text = page.text();
    assert!(
        page_text.contains(&format!("held by you ({shown}):")),
        "{page_text}"
    );
    assert!(
        page_text.contains(&format!("  ({omitted} held omitted to fit this receipt)")),
        "{page_text}"
    );
    let positions = change_positions(&page.value);
    assert!(!positions.is_empty());
    assert!(
        positions.windows(2).all(|pair| pair[1] == pair[0] + 1),
        "staged change page stays dense: {positions:?}"
    );
    assert!(page.value["delivery_token"].as_str().is_some());
}

#[test]
fn listing_cursor_is_smaller_than_half_the_legacy_hex_token() {
    let (_directory, verbs, _, _) = fixture();
    let parent = add(&verbs, "Cursor parent", None, false, 0);
    verbs
        .add(
            AddInput {
                title: "Ready child".into(),
                under: Some(parent.clone()),
                priority: Some(2),
                ..AddInput::default()
            },
            at(1),
        )
        .expect("child");
    verbs
        .add(
            AddInput {
                title: "Ready sibling".into(),
                under: Some(parent.clone()),
                priority: Some(3),
                ..AddInput::default()
            },
            at(2),
        )
        .expect("sibling");
    let page = verbs
        .ls(
            &LsInput {
                ready: true,
                under: Some(parent),
                limit: Some(1),
                ..LsInput::default()
            },
            at(10),
        )
        .expect("listing");
    let token = page.value["after"].as_str().expect("after");
    assert!(token.starts_with("c1-"));
    let value = crate::work_service::listing_cursor_json(token).expect("cursor JSON");
    let mut legacy_filters = serde_json::Map::new();
    if let Some(filters) = value.get("filters").and_then(Value::as_object) {
        legacy_filters.extend(filters.clone());
    }
    for (key, empty) in [
        ("search", Value::Null),
        ("lifecycles", json!([])),
        ("availabilities", json!([])),
        ("blocked_only", json!(false)),
        ("assigned_to", Value::Null),
        ("held_by", Value::Null),
        ("label", Value::Null),
        ("parent_id", Value::Null),
        ("child_requirement", Value::Null),
        ("after", Value::Null),
        ("limit", json!(0)),
    ] {
        legacy_filters.entry(key.to_owned()).or_insert(empty);
    }
    let mut legacy = value.clone();
    legacy["filters"] = Value::Object(legacy_filters);
    let old = encode_legacy_hex_listing_token(&legacy);
    assert!(
        token.len() * 2 <= old.len(),
        "new {} vs old {}",
        token.len(),
        old.len()
    );
    let error = verbs
        .ls(
            &LsInput {
                ready: true,
                after: Some(old),
                limit: Some(1),
                ..LsInput::default()
            },
            at(11),
        )
        .unwrap_err();
    assert!(matches!(
        error.error,
        StoreError::WorkCatalogCursorInvalid { .. }
    ));
}

fn encode_legacy_hex_listing_token(value: &Value) -> String {
    let mut token = String::from("c1-");
    for byte in serde_json::to_vec(value).unwrap() {
        use std::fmt::Write as _;
        write!(token, "{byte:02x}").unwrap();
    }
    token
}
