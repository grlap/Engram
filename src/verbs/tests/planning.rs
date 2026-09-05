use super::*;

fn revise(acceptance: Option<Vec<String>>, title: Option<&str>) -> UpdateAction {
    UpdateAction::Revise {
        acceptance,
        title: title.map(str::to_owned),
        outcome: None,
        assignee: None,
        priority: None,
        defer: None,
        kind: None,
        labels: Vec::new(),
        unlabels: Vec::new(),
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one native/restored matrix covers all fields, bounded history, no-op revisions and canonical shape"
)]
fn phoenix_revision_fields_derive_from_adjacent_native_and_restored_snapshots() {
    for restored in [false, true] {
        let directory = tempdir().expect("temp");
        let project = ProjectId("revision-snapshots".into());
        let source_path = directory.path().join("source.db");
        let source = AgentVerbs::new(
            source_path.clone(),
            project.clone(),
            "agent".into(),
            SessionId("agent".into()),
            None,
        );
        let added = source
            .add(
                AddInput {
                    title: "Before".into(),
                    ..AddInput::default()
                },
                at(0),
            )
            .expect("add");
        let work_ref = added.value["work"]["short_ref"]
            .as_str()
            .expect("ref")
            .to_owned();
        let mut source_store = SqliteStore::open(&source_path).expect("source store");
        let original = source_store
            .resolve_work_ref(&project, &work_ref)
            .expect("original");
        let path = if restored {
            let saved = source_store
                .save_work_graph_snapshot(
                    &project,
                    &original.created_by,
                    None,
                    crate::WorkGraphSnapshotDestinationKind::Stdout,
                    at(1),
                    &crate::DevelopmentNoopRedactor,
                )
                .expect("save");
            let path = directory.path().join("restored.db");
            SqliteStore::open(&path)
                .expect("destination")
                .load_work_graph_snapshot(
                    &project,
                    &original.created_by,
                    &serde_json::to_vec(&saved.document).expect("snapshot bytes"),
                    false,
                    at(2),
                    &crate::DevelopmentNoopRedactor,
                )
                .expect("load");
            path
        } else {
            source_path
        };
        let verbs = AgentVerbs::new(
            path.clone(),
            project.clone(),
            "agent".into(),
            SessionId("agent".into()),
            None,
        );
        verbs
            .update(
                UpdateInput {
                    work_ref: Some(work_ref.clone()),
                    action: UpdateAction::Revise {
                        title: Some("After".into()),
                        outcome: Some("Changed outcome".into()),
                        acceptance: Some(vec!["Changed criterion".into()]),
                        kind: Some(WorkItemKind::Bug),
                        priority: Some(original.priority + 1),
                        labels: vec!["changed".into()],
                        unlabels: Vec::new(),
                        assignee: Some("reviewer".into()),
                        defer: Some(at(200)),
                    },
                },
                at(3),
            )
            .expect("all planning fields");
        let shown = verbs.show(&work_ref, at(4)).expect("show revision");
        assert!(
            shown.value["history"]["items"]
                .as_array()
                .expect("history")
                .iter()
                .any(|row| row["summary"]
                    .as_str()
                    .is_some_and(|summary| summary.starts_with(
                        "title, outcome, acceptance, kind, priority, labels, assignment, deferral:"
                    ))),
            "{}",
            shown.value["history"]
        );
        // More revisions than fit in show force its oldest row to consult a
        // preceding canonical snapshot outside the displayed history window.
        for index in 0..6 {
            verbs
                .update(
                    UpdateInput {
                        work_ref: Some(work_ref.clone()),
                        action: revise(Some(vec![format!("Criterion {index}")]), None),
                    },
                    at(5 + index),
                )
                .expect("acceptance revision");
        }
        let shown = verbs.show(&work_ref, at(12)).expect("bounded history");
        assert!(
            shown.value["history"]["items"]
                .as_array()
                .expect("history")
                .iter()
                .all(|row| row["kind"] == "revised"
                    && row["summary"]
                        .as_str()
                        .expect("summary")
                        .starts_with("acceptance:"))
        );
        verbs
            .update(
                UpdateInput {
                    work_ref: Some(work_ref.clone()),
                    action: revise(Some(vec!["Criterion 5".into()]), None),
                },
                at(13),
            )
            .expect("no-op revision");
        let shown = verbs.show(&work_ref, at(14)).expect("no-op history");
        assert!(
            shown.value["history"]["items"]
                .as_array()
                .expect("history")
                .iter()
                .any(|row| row["summary"]
                    .as_str()
                    .is_some_and(|summary| summary.starts_with("no planning change:")))
        );
        let store = SqliteStore::open(path).expect("store");
        for entry in store
            .work_event_tail(original.work_id, 100)
            .expect("events")
        {
            let event = store
                .get::<crate::domain::WorkEvent>(&entry.object_hash)
                .expect("canonical event")
                .expect("event");
            assert!(
                serde_json::to_value(event.transition)
                    .expect("transition")
                    .get("fields")
                    .is_none(),
                "no new persisted revision metadata"
            );
        }
        assert!(store.verify_all().expect("integrity").is_healthy());
    }
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one lifecycle scenario binds replacement, omission, refusal, canonical history, and the terminal seal"
)]
fn phoenix_acceptance_replacement_is_presence_aware_audited_and_terminal_safe() {
    let directory = tempdir().expect("temp");
    let database = directory.path().join("work.sqlite3");
    let verbs = AgentVerbs::new(
        database.clone(),
        ProjectId("acceptance-revision".into()),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let added = verbs
        .add(
            AddInput {
                title: "Original title".into(),
                acceptance: vec!["Original criterion".into()],
                ..AddInput::default()
            },
            at(0),
        )
        .expect("add");
    let work_ref = added.value["work"]["short_ref"]
        .as_str()
        .expect("ref")
        .to_owned();
    let update = |action, now| {
        verbs.update(
            UpdateInput {
                work_ref: Some(work_ref.clone()),
                action,
            },
            now,
        )
    };
    let replacement = vec!["First replacement".into(), "Second replacement".into()];
    let revised = update(revise(Some(replacement.clone()), None), at(1))
        .expect("unclaimed revision remains admitted");
    assert!(revised.text().contains("acceptance"));
    let shown = verbs.show(&work_ref, at(2)).expect("show");
    assert_eq!(
        shown.value["status"]["work"]["acceptance"],
        json!(replacement)
    );
    assert_eq!(shown.value["status"]["work"]["title"], "Original title");
    assert!(
        shown.value["history"]["items"]
            .as_array()
            .expect("history")
            .iter()
            .any(|row| row["kind"] == "revised"
                && row["summary"]
                    .as_str()
                    .expect("summary")
                    .starts_with("acceptance:"))
    );
    let store = SqliteStore::open(&database).expect("store");
    let item = store
        .resolve_work_ref(&ProjectId("acceptance-revision".into()), &work_ref)
        .expect("item");
    let history = store.work_event_tail(item.work_id, 20).expect("history");
    let connection = rusqlite::Connection::open(&database).expect("read fixture");
    let bytes: Vec<u8> = connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_hash = ?1",
            [history[0].object_hash.as_str()],
            |row| row.get(0),
        )
        .expect("original event");
    let event: crate::domain::WorkEvent = serde_json::from_slice(&bytes).expect("event");
    assert_eq!(event.work.acceptance, vec!["Original criterion"]);
    for invalid in [vec![], vec![" ".into()], vec!["Good".into(), String::new()]] {
        assert!(update(revise(Some(invalid), None), at(3)).is_err());
        assert_eq!(store.get_work_item(item.work_id).expect("unchanged"), item);
    }
    update(revise(None, Some("Changed title")), at(4)).expect("omitted preserves acceptance");
    assert_eq!(
        store.get_work_item(item.work_id).expect("item").acceptance,
        replacement
    );
    verbs
        .claim(
            ClaimInput {
                work_ref: work_ref.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(5),
        )
        .expect("claim");
    update(revise(Some(vec!["Final acceptance".into()]), None), at(6)).expect("holder may revise");
    let done = verbs
        .done(
            DoneInput {
                work_ref: Some(work_ref.clone()),
                summary: Some("Delivered and verified final acceptance".into()),
                note: None,
            },
            at(7),
        )
        .expect("done");
    assert!(!done.owed, "{}", done.text());
    let sealed_item = store.get_work_item(item.work_id).expect("sealed");
    assert!(
        update(
            revise(Some(vec!["Cannot rewrite seal".into()]), None),
            at(8)
        )
        .is_err()
    );
    assert_eq!(
        store.get_work_item(item.work_id).expect("still sealed"),
        sealed_item
    );
    assert!(store.verify_all().expect("integrity").is_healthy());
}

#[test]
fn phoenix_list_reports_exact_counts_and_fits_the_complete_envelope() {
    let directory = tempdir().expect("temp");
    let verbs = AgentVerbs::new(
        directory.path().join("work.sqlite3"),
        ProjectId("list-counts".into()),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    for index in 0..25 {
        verbs
            .add(
                AddInput {
                    title: format!("Count item {index} {}", "\"\\".repeat(100)),
                    outcome: Some("Long detailed outcome ".repeat(100)),
                    labels: vec!["Größe".into()],
                    ..AddInput::default()
                },
                at(index),
            )
            .expect("add");
    }
    let ordinary = verbs.ls(&LsInput::default(), at(30)).expect("list");
    assert_eq!(ordinary.value["total"], 25);
    assert_eq!(ordinary.value["items"].as_array().expect("rows").len(), 20);
    assert_eq!(ordinary.value["omitted"], 5);
    assert!(ordinary.text().contains("showing 20 of 25"));
    assert!(ordinary.text().contains("--limit"));
    for verbose in [false, true] {
        let listed = verbs
            .ls(
                &LsInput {
                    limit: Some(1_000),
                    label: Some("GRÖSSE".into()),
                    verbose,
                    ..LsInput::default()
                },
                at(31),
            )
            .expect("list");
        let len = listed.value["items"].as_array().expect("items").len();
        assert_eq!(listed.value["total"], 25);
        assert_eq!(listed.value["omitted"], 25 - len);
        assert_eq!(listed.value["more"], len < 25);
        if verbose {
            assert!(len < 25, "the verbose fixture must exercise byte fitting");
            assert_eq!(
                listed.value["hint"],
                "page is byte-bounded; narrow with --search or --label, or show an item"
            );
        }
        assert!(
            serde_json::to_vec_pretty(&listed.value)
                .expect("json")
                .len()
                <= MAX_AGENT_WORK_RESPONSE_BYTES
        );
        assert!(listed.text().len() <= MAX_AGENT_WORK_RESPONSE_BYTES);
    }
    let empty = verbs
        .ls(
            &LsInput {
                search: Some("no matching item".into()),
                ..LsInput::default()
            },
            at(32),
        )
        .expect("empty");
    assert_eq!(empty.value["total"], 0);
    assert_eq!(empty.value["omitted"], 0);
    assert_eq!(empty.value["more"], false);
    let smallest = verbs
        .ls(
            &LsInput {
                limit: Some(0),
                ..LsInput::default()
            },
            at(33),
        )
        .expect("clamped");
    assert_eq!(smallest.value["items"].as_array().expect("items").len(), 1);
    assert_eq!(smallest.value["omitted"], 24);
}

#[test]
fn phoenix_only_list_counts_and_zero_row_guidance_names_the_first_match() {
    use crate::storage::{reset_work_catalog_count_queries, work_catalog_count_queries};
    let directory = tempdir().expect("temp");
    let verbs = AgentVerbs::new(
        directory.path().join("work.sqlite3"),
        ProjectId("list-only-count".into()),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let added = verbs
        .add(
            AddInput {
                title: "Held work".into(),
                ..AddInput::default()
            },
            at(0),
        )
        .expect("add");
    let work_ref = added.value["work"]["short_ref"]
        .as_str()
        .expect("ref")
        .to_owned();
    verbs
        .claim(
            ClaimInput {
                work_ref: work_ref.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(1),
        )
        .expect("claim");
    reset_work_catalog_count_queries();
    verbs
        .next(&NextInput::default(), at(2))
        .expect("next with held work");
    assert_eq!(
        work_catalog_count_queries(),
        0,
        "ambient next and held catalogs never count"
    );
    // Keep the real bounded projection and lower only this boundary-test budget.
    let budget = 700;
    let listed = verbs
        .ls_with_budget(
            &LsInput {
                verbose: true,
                ..LsInput::default()
            },
            at(4),
            budget,
        )
        .expect("list");
    assert_eq!(
        work_catalog_count_queries(),
        1,
        "only the emitted list total is counted"
    );
    assert_eq!(listed.value["items"], json!([]));
    assert_eq!(listed.value["total"], 1);
    assert_eq!(listed.value["omitted"], 1);
    assert!(listed.text().contains(&format!(
        "first match is {work_ref}; its row exceeds the page budget"
    )));
    assert!(
        listed.value["hint"]
            .as_str()
            .expect("structured hint")
            .contains(&format!(
                "first match is {work_ref}; its row exceeds the page budget"
            ))
    );
    assert_eq!(
        listed.value["next"],
        json!([format!("engram work show {work_ref}")])
    );
    assert!(
        serde_json::to_vec_pretty(&listed.value)
            .expect("json")
            .len()
            <= budget
    );
    assert!(listed.text().len() <= budget);
}
