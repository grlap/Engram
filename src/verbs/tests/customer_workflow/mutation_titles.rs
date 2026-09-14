use super::*;

const SUMMARY_BYTES: usize = 192;

fn mutation_title(receipt: &Receipt) -> &str {
    receipt.value["work"]["title"]
        .as_str()
        .expect("mutation work.title")
}

fn canonical_item(path: &std::path::Path, project: &ProjectId, work: &str) -> crate::WorkItem {
    SqliteStore::open(path)
        .expect("store")
        .resolve_work_ref(project, work)
        .expect("canonical work")
}

fn assert_canonical_fields(
    path: &std::path::Path,
    project: &ProjectId,
    work: &str,
    title: &str,
    outcome: &str,
    acceptance: &[String],
) {
    let item = canonical_item(path, project, work);
    assert_eq!(item.title, title);
    assert_eq!(item.outcome, outcome);
    assert_eq!(item.acceptance, acceptance);
}

fn assert_durable_bounded_add(receipt: &Receipt, verbs: &AgentVerbs, now: i64) {
    let work = receipt.value["work"]["short_ref"]
        .as_str()
        .expect("ref")
        .to_owned();
    let inspect = verbs
        .service
        .inspect_work(&work, at(now))
        .expect("summary inspect");
    assert_eq!(mutation_title(receipt), inspect.status.work.title);
    assert!(inspect.status.work.title.len() <= SUMMARY_BYTES);
    assert!(inspect.outcome.len() <= SUMMARY_BYTES);
    assert!(emitted_receipt_bytes(receipt) < MAX_AGENT_WORK_RESPONSE_BYTES);
    assert_eq!(receipt.text().matches("full detail:").count(), 1);
}

fn store_root(verbs: &AgentVerbs, title: &str, now: i64) -> String {
    match verbs
        .service
        .work_propose(
            WorkProposeInput::Root {
                external_ref: None,
                notes: Vec::new(),
                title: title.into(),
                outcome: format!("{title} outcome"),
                acceptance: vec![format!("{title} accepted")],
                work_kind: None,
                priority: None,
                labels: Vec::new(),
                assigned_to: None,
                deferred_until: None,
                idempotency_key: String::new(),
            },
            at(now),
        )
        .expect("durable root")
    {
        WorkProposeResult::Root { work, .. } => work.short_ref,
        other => panic!("expected root, got {other:?}"),
    }
}

fn defaulted_acceptance(title: &str) -> Vec<String> {
    vec![format!("{title} is done")]
}

fn full_command(work: &str) -> String {
    format!("engram work show '{work}' --full")
}

fn show_full(verbs: &AgentVerbs, work: &str, now: DateTime<Utc>) -> Receipt {
    verbs
        .show_records(
            work,
            &crate::verbs::ShowInput {
                full: true,
                ..crate::verbs::ShowInput::default()
            },
            now,
        )
        .unwrap_or_else(|error| panic!("show --full must succeed: {error}"))
}

fn assert_full_contract(
    verbs: &AgentVerbs,
    work: &str,
    title: &str,
    outcome: &str,
    acceptance: &[String],
    now: DateTime<Utc>,
) {
    let receipt = show_full(verbs, work, now);
    assert_eq!(receipt.value["work"]["short_ref"], work);
    assert_eq!(receipt.value["work"]["title"], title);
    assert_eq!(receipt.value["work"]["outcome"], outcome);
    assert_eq!(receipt.value["work"]["acceptance"], json!(acceptance));
    assert!(receipt.value["work"]["revision"].as_i64().is_some());
    assert!(receipt.text().contains("complete contract"));
}

fn assert_ordinary_show_and_full_contract(
    verbs: &AgentVerbs,
    work: &str,
    title: &str,
    outcome: &str,
    acceptance: &[String],
    now: i64,
    must_omit_outcome: bool,
) {
    let shown = verbs
        .show(work, at(now))
        .unwrap_or_else(|error| panic!("ordinary show must not refuse: {error}"));
    assert!(emitted_receipt_bytes(&shown) < MAX_AGENT_WORK_RESPONSE_BYTES);
    let shown_title = shown.value["status"]["work"]["title"]
        .as_str()
        .expect("show title");
    if title.len() <= SUMMARY_BYTES {
        assert_eq!(shown_title, title);
        assert!(shown.value["status"]["work"]["title_truncated"].is_null());
        assert!(shown.value["status"]["work"]["title_bytes"].is_null());
    } else {
        assert!(shown_title.len() <= SUMMARY_BYTES);
        assert_ne!(shown_title, title);
        assert!(shown_title.ends_with("..."));
        assert_eq!(shown.value["status"]["work"]["title_truncated"], true);
        assert_eq!(
            shown.value["status"]["work"]["title_bytes"].as_u64(),
            Some(u64::try_from(title.len()).expect("title bytes"))
        );
    }
    if must_omit_outcome {
        assert!(shown.value["status"]["work"]["outcome"].is_null());
        assert_eq!(
            shown.value["status"]["work"]["outcome_omitted"].as_u64(),
            Some(u64::try_from(outcome.len()).expect("outcome bytes"))
        );
        assert!(
            shown.text().contains("UTF-8 bytes omitted"),
            "{}",
            shown.text()
        );
    } else if let Some(visible) = shown.value["status"]["work"]["outcome"].as_str() {
        assert_eq!(visible, outcome);
        assert!(shown.value["status"]["work"]["outcome_omitted"].is_null());
    } else {
        assert_eq!(
            shown.value["status"]["work"]["outcome_omitted"].as_u64(),
            Some(u64::try_from(outcome.len()).expect("outcome bytes"))
        );
    }
    let needs_full = title.len() > SUMMARY_BYTES
        || shown.value["status"]["work"]["outcome"].is_null()
        || shown.value["status"]["work"]["acceptance_omitted"]
            .as_u64()
            .is_some_and(|count| count > 0);
    if needs_full {
        assert!(
            shown
                .next
                .iter()
                .any(|command| command == &full_command(work)),
            "{}",
            shown.next.join("\n")
        );
    }
    let notes = verbs
        .show_with_notes(work, true, at(now + 1))
        .unwrap_or_else(|error| panic!("notes window must share ordinary show recovery: {error}"));
    assert!(emitted_receipt_bytes(&notes) < MAX_AGENT_WORK_RESPONSE_BYTES);
    assert_full_contract(verbs, work, title, outcome, acceptance, at(now + 2));
}

#[test]
fn add_does_not_commit_then_refuse_defaulted_outcome_across_measured_bands() {
    let (_directory, verbs, path, project) = fixture();
    // Installed preflight: 6000/8000 already succeeded; 9000/10000 failed
    // work_propose after commit; 11000/12000 failed work_focus after commit.
    for (index, size) in [6_000, 8_000, 9_000, 10_000, 11_000, 12_000, 16_384]
        .into_iter()
        .enumerate()
    {
        let now = i64::try_from(index).expect("index");
        let title = format!("ASCII title {size} {}", "A".repeat(size));
        let added = verbs
            .add(
                AddInput {
                    title: title.clone(),
                    ..AddInput::default()
                },
                at(now),
            )
            .unwrap_or_else(|error| {
                panic!("defaulted add of {size}-byte title must succeed after store: {error}")
            });
        let work = added.value["work"]["short_ref"]
            .as_str()
            .expect("ref")
            .to_owned();
        assert_canonical_fields(
            &path,
            &project,
            &work,
            &title,
            &title,
            &defaulted_acceptance(&title),
        );
        assert_durable_bounded_add(&added, &verbs, now + 20);
        assert!(
            added
                .reminders
                .iter()
                .any(|line| line == "acceptance defaulted to the title being done; set --accept")
        );
        assert_ordinary_show_and_full_contract(
            &verbs,
            &work,
            &title,
            &title,
            &defaulted_acceptance(&title),
            now + 40,
            size == 16_384,
        );
    }
}

#[test]
fn add_with_explicit_acceptance_bounds_utf8_and_escape_heavy_titles() {
    let (_directory, verbs, path, project) = fixture();
    for (index, title) in [
        format!("UTF-8 title {}", "é".repeat(8_192)),
        format!(
            "Escape title quote\" \u{1b}[31m\u{9b} {}",
            "x".repeat(12_000)
        ),
    ]
    .into_iter()
    .enumerate()
    {
        let now = i64::try_from(index).expect("index");
        let added = verbs
            .add(
                AddInput {
                    title: title.clone(),
                    acceptance: vec!["Delivered".into()],
                    ..AddInput::default()
                },
                at(now),
            )
            .unwrap_or_else(|error| panic!("explicit add must succeed: {error}"));
        let work = added.value["work"]["short_ref"]
            .as_str()
            .expect("ref")
            .to_owned();
        assert_canonical_fields(
            &path,
            &project,
            &work,
            &title,
            &title,
            &["Delivered".into()],
        );
        assert_durable_bounded_add(&added, &verbs, now + 10);
        assert!(
            !added
                .reminders
                .iter()
                .any(|line| line.starts_with("acceptance defaulted"))
        );
        assert_ordinary_show_and_full_contract(
            &verbs,
            &work,
            &title,
            &title,
            &["Delivered".into()],
            now + 20,
            false,
        );
    }
}

#[test]
fn canonical_fields_survive_summary_truncation_boundaries() {
    let (_directory, verbs, path, project) = fixture();
    let utf8_over_bound = "é".repeat(97);
    assert_eq!(utf8_over_bound.len(), 194);
    for (index, title) in ["A".repeat(192), "A".repeat(193), utf8_over_bound]
        .into_iter()
        .enumerate()
    {
        let now = i64::try_from(index).expect("index");
        let added = verbs
            .add(
                AddInput {
                    title: title.clone(),
                    ..AddInput::default()
                },
                at(now),
            )
            .expect("boundary add");
        let work = added.value["work"]["short_ref"]
            .as_str()
            .expect("ref")
            .to_owned();
        assert_canonical_fields(
            &path,
            &project,
            &work,
            &title,
            &title,
            &defaulted_acceptance(&title),
        );
        assert_durable_bounded_add(&added, &verbs, now + 10);
        if title.len() <= SUMMARY_BYTES {
            assert_eq!(mutation_title(&added), title);
        } else {
            assert_ne!(mutation_title(&added), title);
            assert!(mutation_title(&added).ends_with("..."));
        }
    }
}

#[test]
fn ordinary_short_title_still_appears_in_full_on_the_mutation_envelope() {
    let (_directory, verbs, path, project) = fixture();
    let title = "Ordinary bounded title";
    let added = verbs
        .add(
            AddInput {
                title: title.into(),
                acceptance: vec!["Delivered".into()],
                ..AddInput::default()
            },
            at(0),
        )
        .expect("ordinary add");
    assert_eq!(mutation_title(&added), title);
    assert_durable_bounded_add(&added, &verbs, 1);
    let work = added.value["work"]["short_ref"]
        .as_str()
        .expect("ref")
        .to_owned();
    assert_canonical_fields(&path, &project, &work, title, title, &["Delivered".into()]);
    let shown = verbs.show(&work, at(2)).expect("show");
    assert_eq!(shown.value["status"]["work"]["title"], title);
    assert_eq!(shown.value["status"]["work"]["outcome"], title);
    assert!(shown.value["status"]["work"]["title_truncated"].is_null());
    assert!(shown.value["status"]["work"]["outcome_omitted"].is_null());
    assert!(emitted_receipt_bytes(&shown) < MAX_AGENT_WORK_RESPONSE_BYTES);
    assert_full_contract(&verbs, &work, title, title, &["Delivered".into()], at(3));
}

#[test]
fn defaulted_oversized_child_add_under_a_bounded_parent_records_initial_notes() {
    let (_directory, verbs, path, project) = fixture();
    let parent_title = "Bounded parent";
    let parent = verbs
        .add(
            AddInput {
                title: parent_title.into(),
                acceptance: vec!["Parent delivered".into()],
                ..AddInput::default()
            },
            at(0),
        )
        .expect("parent");
    let parent_ref = parent.value["work"]["short_ref"]
        .as_str()
        .expect("parent ref")
        .to_owned();
    let title = format!("Child title {}", "C".repeat(16_384));
    let added = verbs
        .add(
            AddInput {
                title: title.clone(),
                under: Some(parent_ref.clone()),
                notes: vec!["initial child observation".into()],
                ..AddInput::default()
            },
            at(1),
        )
        .unwrap_or_else(|error| panic!("defaulted oversized child add must succeed: {error}"));
    let work = added.value["work"]["short_ref"]
        .as_str()
        .expect("child ref")
        .to_owned();
    assert_ne!(work, parent_ref);
    assert_canonical_fields(
        &path,
        &project,
        &work,
        &title,
        &title,
        &defaulted_acceptance(&title),
    );
    assert_canonical_fields(
        &path,
        &project,
        &parent_ref,
        parent_title,
        parent_title,
        &["Parent delivered".into()],
    );
    assert_durable_bounded_add(&added, &verbs, 1);
    assert!(
        added
            .reminders
            .iter()
            .any(|line| line == "acceptance defaulted to the title being done; set --accept")
    );
    assert!(
        added.reminders.iter().any(|line| {
            line == "initial observations (no execution credit) recorded at creation"
        })
    );
    assert_ordinary_show_and_full_contract(
        &verbs,
        &work,
        &title,
        &title,
        &defaulted_acceptance(&title),
        10,
        true,
    );
}

#[test]
fn claim_note_gate_and_done_bound_an_already_stored_oversized_title() {
    let (_directory, verbs, path, project) = fixture();
    let title = format!("Stored title {}", "B".repeat(16_384));
    let work = store_root(&verbs, &title, 0);
    let claimed = verbs
        .claim(
            ClaimInput {
                work_ref: work.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(1),
        )
        .expect("claim");
    assert_durable_bounded_add(&claimed, &verbs, 1);
    let noted = verbs
        .note(
            &NoteInput {
                status: false,
                work_ref: Some(work.clone()),
                text: "holder note".into(),
                refs: Vec::new(),
            },
            at(2),
        )
        .expect("note");
    assert_durable_bounded_add(&noted, &verbs, 2);
    let gated = verbs
        .gate(
            GateInput {
                work_ref: Some(work.clone()),
                name: "fmt".into(),
                failed: Vec::new(),
                evidence_ref: None,
            },
            at(3),
        )
        .expect("gate");
    assert_durable_bounded_add(&gated, &verbs, 3);
    let done = verbs
        .done(
            DoneInput {
                work_ref: Some(work.clone()),
                summary: Some("closed".into()),
                ..DoneInput::default()
            },
            at(4),
        )
        .expect("done");
    assert!(!done.owed, "{}", done.text());
    assert_durable_bounded_add(&done, &verbs, 4);
    assert_canonical_fields(
        &path,
        &project,
        &work,
        &title,
        &format!("{title} outcome"),
        &[format!("{title} accepted")],
    );
    assert_ordinary_show_and_full_contract(
        &verbs,
        &work,
        &title,
        &format!("{title} outcome"),
        &[format!("{title} accepted")],
        10,
        true,
    );
}

#[test]
fn show_keeps_full_stored_outcome_when_the_show_receipt_fits() {
    let (_directory, verbs, path, project) = fixture();
    let title = format!("Showable title {}", "D".repeat(1_000));
    let added = verbs
        .add(
            AddInput {
                title: title.clone(),
                acceptance: vec!["Delivered".into()],
                ..AddInput::default()
            },
            at(0),
        )
        .expect("showable add");
    assert_durable_bounded_add(&added, &verbs, 0);
    let work = added.value["work"]["short_ref"]
        .as_str()
        .expect("ref")
        .to_owned();
    assert_canonical_fields(
        &path,
        &project,
        &work,
        &title,
        &title,
        &["Delivered".into()],
    );
    let shown = verbs.show(&work, at(1)).expect("show");
    let shown_title = shown.value["status"]["work"]["title"]
        .as_str()
        .expect("show title");
    assert!(shown_title.len() <= SUMMARY_BYTES);
    assert_ne!(shown_title, title);
    assert_eq!(shown.value["status"]["work"]["title_truncated"], true);
    assert_eq!(
        shown.value["status"]["work"]["title_bytes"].as_u64(),
        Some(u64::try_from(title.len()).expect("title bytes"))
    );
    assert_eq!(shown.value["status"]["work"]["outcome"], title);
    assert!(shown.value["status"]["work"]["outcome_omitted"].is_null());
    assert!(
        shown
            .next
            .iter()
            .any(|command| command == &full_command(&work)),
        "{}",
        shown.next.join("\n")
    );
    assert!(emitted_receipt_bytes(&shown) < MAX_AGENT_WORK_RESPONSE_BYTES);
    assert_full_contract(&verbs, &work, &title, &title, &["Delivered".into()], at(2));
}

#[test]
fn show_full_rejects_mixed_window_flags_before_store_effects() {
    let (_directory, verbs, path, project) = fixture();
    let work = super::add(&verbs, "Conflict target", None, false, 0);
    let store = SqliteStore::open(&path).unwrap();
    let focused_before = store
        .work_session_state(&project, &SessionId("agent".into()), at(1))
        .unwrap()
        .focused_work_id;
    let before =
        crate::storage::test_database_shape_snapshot(&rusqlite::Connection::open(&path).unwrap())
            .unwrap();
    for input in [
        crate::verbs::ShowInput {
            full: true,
            notes: true,
            ..crate::verbs::ShowInput::default()
        },
        crate::verbs::ShowInput {
            full: true,
            history: true,
            ..crate::verbs::ShowInput::default()
        },
        crate::verbs::ShowInput {
            full: true,
            note: Some("abcd1234".into()),
            ..crate::verbs::ShowInput::default()
        },
        crate::verbs::ShowInput {
            full: true,
            after: Some("s1-token".into()),
            ..crate::verbs::ShowInput::default()
        },
        crate::verbs::ShowInput {
            full: true,
            notes: true,
            gates: true,
            ..crate::verbs::ShowInput::default()
        },
    ] {
        let error = verbs
            .show_records(&work, &input, at(1))
            .expect_err("conflict");
        assert!(
            matches!(
                error.error,
                StoreError::InvalidWork(ref reason)
                    if reason.contains("--full")
            ),
            "{error}"
        );
    }
    let after =
        crate::storage::test_database_shape_snapshot(&rusqlite::Connection::open(&path).unwrap())
            .unwrap();
    assert_eq!(before, after);
    assert_eq!(
        SqliteStore::open(&path)
            .unwrap()
            .work_session_state(&project, &SessionId("agent".into()), at(1))
            .unwrap()
            .focused_work_id,
        focused_before
    );
}

fn first_show_windows(
    verbs: &AgentVerbs,
    work: &str,
    now: DateTime<Utc>,
) -> Vec<(&'static str, Receipt)> {
    [
        ("ordinary", verbs.show(work, now).expect("ordinary show")),
        (
            "notes",
            verbs
                .show_with_notes(work, true, now)
                .expect("notes window"),
        ),
        (
            "notes+gates",
            verbs
                .show_records(
                    work,
                    &crate::verbs::ShowInput {
                        notes: true,
                        gates: true,
                        ..crate::verbs::ShowInput::default()
                    },
                    now,
                )
                .expect("notes+gates window"),
        ),
        (
            "history",
            verbs
                .show_records(
                    work,
                    &crate::verbs::ShowInput {
                        history: true,
                        ..crate::verbs::ShowInput::default()
                    },
                    now,
                )
                .expect("history window"),
        ),
    ]
    .into()
}

fn assert_header_keeps_complete_summary_title(receipt: &Receipt, title: &str, label: &str) {
    let header = receipt.lines.first().expect("show header");
    assert!(
        !header.contains('\u{2026}'),
        "{label} header used the 96-byte ellipsis: {header}"
    );
    let json_title = receipt.value["status"]["work"]["title"]
        .as_str()
        .expect("show title");
    assert_eq!(json_title, title, "{label}");
    assert!(
        receipt.value["status"]["work"]["title_truncated"].is_null(),
        "{label}"
    );
    assert!(
        receipt.value["status"]["work"]["title_bytes"].is_null(),
        "{label}"
    );
    let quoted = format!("\"{}\"", crate::work_service::terminal_error_line(title));
    assert!(
        header.contains(&quoted),
        "{label} header {header} missing bounded title {quoted}"
    );
}

#[test]
fn ordinary_show_header_keeps_complete_summary_titles() {
    let (_directory, verbs, path, project) = fixture();
    let cases = [
        "A".repeat(97),
        "A".repeat(120),
        "A".repeat(192),
        "é".repeat(60),
        format!("say \"hi\" \u{1b}[0m {}", "B".repeat(80)),
    ];
    for (index, title) in cases.into_iter().enumerate() {
        assert!(title.len() <= SUMMARY_BYTES, "{}", title.len());
        let now = i64::try_from(index).expect("index");
        let added = verbs
            .add(
                AddInput {
                    title: title.clone(),
                    outcome: Some("Short outcome".into()),
                    acceptance: vec!["Delivered".into()],
                    notes: vec!["window seed".into()],
                    ..AddInput::default()
                },
                at(now),
            )
            .unwrap_or_else(|error| panic!("add summary title: {error}"));
        let work = added.value["work"]["short_ref"]
            .as_str()
            .expect("ref")
            .to_owned();
        assert_canonical_fields(
            &path,
            &project,
            &work,
            &title,
            "Short outcome",
            &["Delivered".into()],
        );
        for (label, shown) in first_show_windows(&verbs, &work, at(now + 10)) {
            assert_header_keeps_complete_summary_title(&shown, &title, label);
            assert!(
                shown.next.iter().all(|command| !command.contains("--full")),
                "{label} advertised --full for a complete 192-byte title:\n{}",
                shown.next.join("\n")
            );
            assert!(emitted_receipt_bytes(&shown) < MAX_AGENT_WORK_RESPONSE_BYTES);
        }
    }
}

#[test]
fn oversized_child_keeps_parent_and_full_when_lifecycle_fills_the_text_footer() {
    let (_directory, verbs, _, _) = fixture();
    let parent = super::add(&verbs, "Parent", None, false, 0);
    let title = format!("Oversized child {}", "C".repeat(400));
    let child = verbs
        .add(
            AddInput {
                title,
                outcome: Some("Short outcome".into()),
                acceptance: vec!["Delivered".into()],
                under: Some(parent.clone()),
                ..AddInput::default()
            },
            at(1),
        )
        .unwrap()
        .value["work"]["short_ref"]
        .as_str()
        .unwrap()
        .to_owned();
    let prerequisite = super::add(&verbs, "Prerequisite", None, false, 2);
    for (work_ref, action, now) in [
        (
            child.clone(),
            UpdateAction::After {
                prerequisite: prerequisite.clone(),
            },
            3,
        ),
        (
            prerequisite.clone(),
            UpdateAction::Cancel {
                reason: "Prerequisite unavailable".into(),
            },
            4,
        ),
        (
            child.clone(),
            UpdateAction::Blocked {
                detail: "Independent blocker".into(),
            },
            5,
        ),
    ] {
        verbs
            .update(
                UpdateInput {
                    work_ref: Some(work_ref),
                    action,
                },
                at(now),
            )
            .unwrap();
    }
    let recovery = format!("engram work update {child} --drop-after {prerequisite}");
    let parent_command = format!("engram work show {parent}");
    let shown = verbs.show(&child, at(20)).unwrap();
    assert_eq!(
        shown.next[..3],
        [
            recovery.clone(),
            format!("engram work note {child} \"…\""),
            format!("engram work update {child} --unblock"),
        ],
        "fixture must exercise all three lifecycle suggestions"
    );
    assert!(
        shown.next[..MAX_TEXT_NEXT_COMMANDS].contains(&parent_command),
        "parent must stay in the text footer:\n{}",
        shown.next.join("\n")
    );
    assert!(shown.text().contains(&format!("  {parent_command}\n")));
    assert!(
        shown
            .next
            .iter()
            .any(|command| command == &full_command(&child)),
        "{}",
        shown.next.join("\n")
    );
    assert!(
        shown
            .lines
            .iter()
            .any(|line| line.contains(&full_command(&child))),
        "body must keep --full when the text footer is full:\n{}",
        shown.lines.join("\n")
    );
    assert_eq!(shown.value["status"]["work"]["title_truncated"], true);
    assert!(shown.value["status"]["work"]["outcome_omitted"].is_null());
    assert!(emitted_receipt_bytes(&shown) < MAX_AGENT_WORK_RESPONSE_BYTES);
}

#[test]
fn acceptance_omission_advertises_full_in_the_body() {
    let (_directory, verbs, _, _) = fixture();
    let criteria = (0..40)
        .map(|index| format!("Criterion {index}: {}", "x".repeat(500)))
        .collect::<Vec<_>>();
    let work = verbs
        .add(
            AddInput {
                title: "Short title".into(),
                outcome: Some("Short outcome".into()),
                acceptance: criteria,
                ..AddInput::default()
            },
            at(0),
        )
        .unwrap()
        .value["work"]["short_ref"]
        .as_str()
        .unwrap()
        .to_owned();
    let shown = verbs.show(&work, at(1)).unwrap();
    assert_eq!(shown.value["status"]["work"]["title"], "Short title");
    assert!(shown.value["status"]["work"]["title_truncated"].is_null());
    assert_eq!(shown.value["status"]["work"]["outcome"], "Short outcome");
    assert!(shown.value["status"]["work"]["outcome_omitted"].is_null());
    assert!(
        shown.value["status"]["work"]["acceptance_omitted"]
            .as_u64()
            .is_some_and(|count| count > 0),
        "{}",
        shown.text()
    );
    let command = full_command(&work);
    assert!(
        shown.next.iter().any(|next| next == &command),
        "{}",
        shown.next.join("\n")
    );
    assert!(
        shown.lines.iter().any(|line| {
            line.contains("hidden criteria continue from position") && line.contains(&command)
        }),
        "acceptance omission must carry --full in the body:\n{}",
        shown.lines.join("\n")
    );
    assert!(emitted_receipt_bytes(&shown) < MAX_AGENT_WORK_RESPONSE_BYTES);
}
