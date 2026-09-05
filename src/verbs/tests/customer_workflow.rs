use super::*;

mod review;

fn fixture() -> (tempfile::TempDir, AgentVerbs, PathBuf, ProjectId) {
    let directory = tempdir().expect("temp");
    let path = directory.path().join("work.db");
    let project = ProjectId("customer-workflow".into());
    let verbs = AgentVerbs::new(
        path.clone(),
        project.clone(),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    (directory, verbs, path, project)
}

fn add(verbs: &AgentVerbs, title: &str, under: Option<&str>, optional: bool, now: i64) -> String {
    verbs
        .add(
            AddInput {
                title: title.into(),
                under: under.map(str::to_owned),
                optional,
                ..AddInput::default()
            },
            at(now),
        )
        .expect("add")
        .value["work"]["short_ref"]
        .as_str()
        .expect("ref")
        .to_owned()
}

fn note(verbs: &AgentVerbs, work: &str, text: &str, now: i64) {
    verbs
        .note(
            &NoteInput {
                work_ref: Some(work.into()),
                text: text.into(),
                refs: vec!["test:full-note".into()],
            },
            at(now),
        )
        .expect("note");
}

#[test]
fn phoenix_add_reminds_only_for_defaulted_acceptance_on_roots_and_children() {
    let (_directory, verbs, _, _) = fixture();
    let parent = add(&verbs, "Parent", None, false, 0);
    for under in [None, Some(parent)] {
        for explicit in [false, true] {
            let title = format!("Reminder {explicit} {}", under.is_some());
            let receipt = verbs
                .add(
                    AddInput {
                        title: title.clone(),
                        under: under.clone(),
                        acceptance: if explicit {
                            vec![format!("{title} is done")]
                        } else {
                            Vec::new()
                        },
                        ..AddInput::default()
                    },
                    at(1),
                )
                .expect("add");
            let expected = format!("acceptance defaulted to {title} is done; set --accept");
            assert_eq!(receipt.reminders.contains(&expected), !explicit);
            assert_eq!(receipt.text().contains(&expected), !explicit);
            assert_eq!(
                receipt.value["reminders"]
                    .as_array()
                    .expect("reminders")
                    .contains(&json!(expected)),
                !explicit
            );
        }
    }
    assert!(
        verbs
            .add(
                AddInput {
                    title: "Blank acceptance".into(),
                    acceptance: vec![" ".into()],
                    ..AddInput::default()
                },
                at(2)
            )
            .is_err()
    );
}

#[test]
fn phoenix_full_notes_keep_entire_bodies_refs_and_recorded_order_without_changing_default_show() {
    let (_directory, verbs, _, _) = fixture();
    let work = add(&verbs, "Full notes", None, false, 0);
    let bodies = (0..4)
        .map(|index| {
            format!(
                "Note {index}\n{}\nFinal line {index}",
                "Full ünicode content. ".repeat(12)
            )
        })
        .collect::<Vec<_>>();
    for (index, body) in bodies.iter().enumerate() {
        note(
            &verbs,
            &work,
            body,
            100 - i64::try_from(index).expect("index"),
        );
    }
    let before = verbs.show(&work, at(200)).expect("default show");
    let unchanged = verbs
        .show_with_notes(&work, false, at(200))
        .expect("flag false");
    assert_eq!(before.value, unchanged.value);
    assert_eq!(before.text(), unchanged.text());
    let full = verbs
        .show_with_notes(&work, true, at(200))
        .expect("full notes");
    let notes = full.value["notes"].as_array().expect("notes");
    assert_eq!(notes.len(), bodies.len());
    assert_eq!(full.value["notes_omitted"], 0);
    for (entry, body) in notes.iter().zip(&bodies) {
        assert_eq!(entry["summary"], *body);
        assert_eq!(entry["refs"], json!(["test:full-note"]));
        assert_eq!(entry["non_holder"], true);
        for line in body.lines() {
            assert!(full.text().contains(line));
        }
    }
    assert!(full.text().find("Note 0").expect("first") < full.text().find("Note 3").expect("last"));
    assert!(full.text().len() <= MAX_AGENT_WORK_RESPONSE_BYTES);
    assert!(
        serde_json::to_vec_pretty(&full.value).expect("JSON").len()
            <= MAX_AGENT_WORK_RESPONSE_BYTES
    );
    assert_eq!(
        verbs.show(&work, at(200)).expect("default after").value,
        before.value
    );
}

#[test]
fn phoenix_full_notes_fit_whole_prefix_and_report_exact_remainder_including_zero() {
    let (_directory, verbs, _, _) = fixture();
    let work = add(&verbs, "Budget notes", None, false, 0);
    let empty = verbs.show_with_notes(&work, true, at(1)).expect("empty");
    assert_eq!(empty.value["notes"], json!([]));
    assert_eq!(empty.value["notes_omitted"], 0);
    let bodies = (0..25)
        .map(|index| format!("Note {index}: {} END {index}", "\\\"ü\n".repeat(120)))
        .collect::<Vec<_>>();
    for (index, body) in bodies.iter().enumerate() {
        note(
            &verbs,
            &work,
            body,
            2 + i64::try_from(index).expect("index"),
        );
    }
    let full = verbs
        .show_with_notes(&work, true, at(100))
        .expect("budgeted");
    let notes = full.value["notes"].as_array().expect("notes");
    assert!(!notes.is_empty() && notes.len() < bodies.len());
    assert_eq!(full.value["notes_omitted"], bodies.len() - notes.len());
    for (entry, body) in notes.iter().zip(&bodies) {
        assert_eq!(entry["summary"], *body);
    }
    assert!(
        full.text()
            .contains(&format!("{} notes omitted", bodies.len() - notes.len()))
    );
    assert!(full.text().len() <= MAX_AGENT_WORK_RESPONSE_BYTES);
    assert!(
        serde_json::to_vec_pretty(&full.value).expect("JSON").len()
            <= MAX_AGENT_WORK_RESPONSE_BYTES
    );
}

fn terminalize(verbs: &AgentVerbs, parent: &str, lifecycle: WorkLifecycle) {
    if lifecycle == WorkLifecycle::Completed {
        verbs
            .claim(
                ClaimInput {
                    work_ref: parent.into(),
                    ttl_seconds: None,
                    recover: None,
                },
                at(2),
            )
            .expect("claim");
        let done = verbs
            .done(
                DoneInput {
                    work_ref: Some(parent.into()),
                    summary: Some("Delivered parent".into()),
                    note: Some("Terminal parent is done".into()),
                },
                at(3),
            )
            .expect("done");
        assert!(!done.owed, "{}", done.text());
    } else {
        let action = match lifecycle {
            WorkLifecycle::Cancelled => UpdateAction::Cancel {
                reason: "Not needed".into(),
            },
            WorkLifecycle::Superseded => UpdateAction::Supersede {
                replacement: add(verbs, "Replacement", None, false, 2),
                reason: "Replaced".into(),
            },
            _ => unreachable!("terminal matrix"),
        };
        verbs
            .update(
                UpdateInput {
                    work_ref: Some(parent.into()),
                    action,
                },
                at(3),
            )
            .expect("dispose");
    }
}

fn object_count(connection: &rusqlite::Connection) -> i64 {
    connection
        .query_row("SELECT COUNT(*) FROM objects", [], |row| row.get(0))
        .expect("canonical count")
}

#[test]
fn phoenix_add_under_terminal_parent_returns_typed_root_followup_remedy_without_graph_writes() {
    for lifecycle in [
        WorkLifecycle::Completed,
        WorkLifecycle::Cancelled,
        WorkLifecycle::Superseded,
    ] {
        let (_directory, verbs, path, project) = fixture();
        let parent = add(&verbs, "Terminal parent", None, false, 0);
        let existing_child = (lifecycle == WorkLifecycle::Completed)
            .then(|| add(&verbs, "Existing optional child", Some(&parent), true, 1));
        terminalize(&verbs, &parent, lifecycle);
        let mut store = SqliteStore::open(&path).expect("store");
        let parent_before = store.resolve_work_ref(&project, &parent).expect("parent");
        let child_before = existing_child
            .as_ref()
            .map(|child| store.resolve_work_ref(&project, child).expect("child"));
        let connection = rusqlite::Connection::open(&path).expect("inspect");
        let objects = object_count(&connection);
        for optional in [false, true] {
            let request = crate::DecomposeWorkRequest {
                parent_id: parent_before.work_id,
                expected_parent_revision: parent_before.revision,
                children: vec![crate::ChildWorkDraft {
                    local_key: "refused".into(),
                    child_requirement: if optional {
                        ChildRequirement::Optional
                    } else {
                        ChildRequirement::Required
                    },
                    title: "Refused child".into(),
                    outcome: "Refused outcome".into(),
                    acceptance: vec!["Refused criterion".into()],
                    kind: WorkItemKind::Task,
                    priority: 1,
                    labels: Vec::new(),
                    assigned_to: None,
                    deferred_until: None,
                }],
                prerequisites: Vec::new(),
                authority: crate::WorkPlanningAuthority::Project,
                actor: parent_before.created_by.clone(),
                idempotency_key: format!("refused-{optional}"),
                created_at: at(4),
            };
            assert!(
                matches!(store.decompose_work(&request, &crate::DevelopmentNoopRedactor), Err(StoreError::WorkParentNotOpen { parent, lifecycle: state }) if parent == parent_before.work_id && state == lifecycle)
            );
            let error = verbs
                .add(
                    AddInput {
                        title: "Refused child".into(),
                        under: Some(parent.clone()),
                        optional,
                        ..AddInput::default()
                    },
                    at(4),
                )
                .expect_err("terminal parent");
            assert!(
                matches!(&error.error, StoreError::WorkParentNotOpen { parent: id, lifecycle: state } if *id == parent_before.work_id && *state == lifecycle)
            );
            let json = crate::mcp::store_error_value(&error.error);
            assert_eq!(json["error"]["code"], "work_parent_not_open");
            assert_eq!(
                json["error"]["details"]["remedy"],
                "file an independent root follow-up or add under an open ancestor"
            );
            assert!(
                error
                    .guidance()
                    .reminders
                    .iter()
                    .any(|text| text.contains("independent root follow-up"))
            );
        }
        assert_eq!(object_count(&connection), objects);
        assert_eq!(
            store
                .resolve_work_ref(&project, &parent)
                .expect("parent after"),
            parent_before
        );
        if let Some(child) = child_before {
            assert_eq!(
                store
                    .resolve_work_ref(&project, &child.short_ref)
                    .expect("unchanged child"),
                child
            );
            note(
                &verbs,
                &child.short_ref,
                "Existing fenced child remains observable",
                5,
            );
        }
    }
}

#[test]
fn phoenix_full_notes_include_inherited_late_restored_and_reopened_native_generations() {
    let (directory, verbs, path, project) = fixture();
    let work = add(&verbs, "Generations", None, false, 0);
    note(&verbs, &work, "First inherited note", 1);
    verbs
        .claim(
            ClaimInput {
                work_ref: work.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(2),
        )
        .expect("claim");
    assert!(
        !verbs
            .done(
                DoneInput {
                    work_ref: Some(work.clone()),
                    summary: Some("First completion".into()),
                    note: None
                },
                at(3)
            )
            .expect("complete")
            .owed
    );
    let mut source = SqliteStore::open(&path).expect("source");
    let actor = source
        .resolve_work_ref(&project, &work)
        .expect("work")
        .created_by;
    let saved = source
        .save_work_graph_snapshot(
            &project,
            &actor,
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(4),
            &crate::DevelopmentNoopRedactor,
        )
        .expect("save");
    let restored_path = directory.path().join("restored.db");
    let mut destination = SqliteStore::open(&restored_path).expect("destination");
    destination
        .load_work_graph_snapshot(
            &project,
            &actor,
            &serde_json::to_vec(&saved.document).expect("bytes"),
            false,
            at(5),
            &crate::DevelopmentNoopRedactor,
        )
        .expect("load");
    let current = destination
        .resolve_work_ref(&project, &work)
        .expect("restored work");
    let restored = AgentVerbs::new(
        restored_path,
        project,
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    note(&restored, &work, "Late restored note", 6);
    destination
        .reopen_work(
            &crate::ReopenWorkRequest {
                work_id: current.work_id,
                expected_work_revision: current.revision,
                reason: "More execution".into(),
                actor,
                idempotency_key: "reopen-notes".into(),
                reopened_at: at(7),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .expect("reopen");
    restored
        .claim(
            ClaimInput {
                work_ref: work.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(8),
        )
        .expect("claim again");
    note(&restored, &work, "New native note", 9);
    let full = restored
        .show_with_notes(&work, true, at(10))
        .expect("all generations");
    let notes = full.value["notes"].as_array().expect("notes");
    assert_eq!(notes.len(), 4, "{}", full.value);
    assert_eq!(full.value["notes_omitted"], 0);
    assert_eq!(notes[0]["summary"], "First inherited note");
    assert_eq!(notes[1]["summary"], "First completion");
    assert_eq!(notes[2]["summary"], "Late restored note");
    assert_eq!(notes[3]["summary"], "New native note");
    assert!(destination.verify_all().expect("integrity").is_healthy());
}

#[test]
fn phoenix_full_notes_omit_an_oversized_first_note_without_skipping_to_later_notes() {
    let (_directory, verbs, _, _) = fixture();
    let work = add(&verbs, "Oversized first note", None, false, 0);
    note(&verbs, &work, &"\\\"".repeat(5_000), 1);
    note(&verbs, &work, "Later small note", 2);
    let full = verbs
        .show_with_notes(&work, true, at(3))
        .expect("whole-note prefix");
    assert_eq!(full.value["notes"], json!([]));
    assert_eq!(full.value["notes_omitted"], 2);
    assert!(full.text().contains("2 notes omitted"));
    assert!(
        serde_json::to_vec_pretty(&full.value).expect("JSON").len()
            <= MAX_AGENT_WORK_RESPONSE_BYTES
    );
}

#[test]
fn phoenix_full_notes_keep_all_gate_failure_labels_and_references() {
    let (_directory, verbs, _, _) = fixture();
    let work = add(&verbs, "Full gate notes", None, false, 0);
    verbs
        .claim(
            ClaimInput {
                work_ref: work.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(1),
        )
        .expect("claim");
    let failures = vec![
        "first check".into(),
        "second check".into(),
        "third check".into(),
    ];
    verbs
        .gate(
            GateInput {
                work_ref: Some(work.clone()),
                name: "Full gate".into(),
                failed: failures.clone(),
                evidence_ref: Some("test:gate-ref".into()),
            },
            at(2),
        )
        .expect("gate");
    let full = verbs.show_with_notes(&work, true, at(3)).expect("notes");
    assert_eq!(full.value["notes_omitted"], 0);
    assert_eq!(full.value["notes"].as_array().expect("notes").len(), 1);
    for failure in failures {
        assert!(
            full.value["notes"][0]["summary"]
                .as_str()
                .expect("summary")
                .contains(&failure)
        );
        assert!(full.text().contains(&failure));
    }
    assert_eq!(full.value["notes"][0]["refs"], json!(["test:gate-ref"]));
}
