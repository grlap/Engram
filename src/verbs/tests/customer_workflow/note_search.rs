use super::*;

fn search(verbs: &AgentVerbs, query: &str, all: bool) -> Receipt {
    verbs
        .ls(
            &LsInput {
                search: Some(query.into()),
                all,
                ..LsInput::default()
            },
            at(200),
        )
        .unwrap()
}

fn detail(verbs: &AgentVerbs, row: &serde_json::Value, work: &str) -> Receipt {
    verbs
        .show_records(
            work,
            &ShowInput {
                note: Some(row["note_match"]["locator"].as_str().unwrap().into()),
                ..ShowInput::default()
            },
            at(200),
        )
        .unwrap()
}

fn assert_match_lines_beside_rows(receipt: &Receipt) {
    let text = receipt.text();
    let lines = text.lines().collect::<Vec<_>>();
    let footer = lines
        .iter()
        .position(|line| line.starts_with("page:"))
        .unwrap();
    let mut annotated = 0;
    for row in receipt.value["items"].as_array().unwrap() {
        let Some(matched) = row.get("note_match") else {
            continue;
        };
        let work = row["ref"]
            .as_str()
            .or_else(|| row["work"]["short_ref"].as_str())
            .unwrap();
        let family = match matched["family"].as_str().unwrap() {
            "notes" => "note",
            "observations" => "observation",
            "gates" => "gate",
            unexpected => panic!("unexpected searchable family: {unexpected}"),
        };
        let annotation = format!(
            "  {work}: matching {family} {}",
            matched["locator"].as_str().unwrap()
        );
        let position = lines.iter().position(|line| *line == annotation).unwrap();
        assert!(position < footer);
        assert!(lines[position - 1].starts_with(&format!("  {work} [")));
        annotated += 1;
    }
    assert_eq!(
        lines
            .iter()
            .filter(|line| line.contains(": matching "))
            .count(),
        annotated
    );
}

#[test]
fn note_search_matches_complete_literal_unicode_text_refs_and_gate_fields_read_only() {
    let (_directory, verbs, path, _project) = fixture();
    let work = add(&verbs, "Unrelated title", None, false, 0);
    let citation = ".worktrees/NAME/target/tmp/file";
    let body = format!("{} {citation} Straße Café \"%_\"", "prefix ".repeat(1000));
    note(&verbs, &work, &body, 1); // Non-holder observation.
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
    note(&verbs, &work, "holder-only-token", 3);
    verbs
        .gate(
            GateInput {
                work_ref: Some(work.clone()),
                name: "typed-gate-name".into(),
                failed: vec!["typed-failure-token".into()],
                evidence_ref: Some("gate-ref-token".into()),
            },
            at(4),
        )
        .unwrap();
    let connection = rusqlite::Connection::open(&path).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
    for (query, family) in [
        (citation, "observations"),
        ("STRASSE", "observations"),
        ("CAFE\u{301}", "observations"),
        ("\"%_\"", "observations"),
        ("%", "observations"),
        ("_", "observations"),
        ("ße", "observations"),
        ("test:full-note", "observations"),
        ("holder-only-token", "notes"),
        ("typed-gate-name", "gates"),
        ("typed-failure-token", "gates"),
        ("gate-ref-token", "gates"),
    ] {
        let receipt = search(&verbs, query, false);
        assert_eq!(receipt.value["total"], 1, "{query}");
        let row = &receipt.value["items"][0];
        assert_eq!(row["ref"], work);
        assert_eq!(row["note_match"]["family"], family);
        assert_match_lines_beside_rows(&receipt);
        let verbose = verbs
            .ls(
                &LsInput {
                    search: Some(query.into()),
                    verbose: true,
                    ..LsInput::default()
                },
                at(200),
            )
            .unwrap();
        assert_eq!(verbose.value["items"][0]["note_match"], row["note_match"]);
        assert_match_lines_beside_rows(&verbose);
        let full = detail(&verbs, row, &work);
        assert_eq!(full.value["note"]["locator"], row["note_match"]["locator"]);
        assert!(emitted_receipt_bytes(&receipt) < MAX_AGENT_WORK_RESPONSE_BYTES);
    }
    assert_eq!(search(&verbs, "not-present", false).value["total"], 0);
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&connection).unwrap(),
        before
    );
    assert!(
        SqliteStore::open(&path)
            .unwrap()
            .verify_all()
            .unwrap()
            .is_healthy()
    );
    // Ordinary search-free reads do not acquire a diagnostic overlay.
    let plain = verbs.ls(&LsInput::default(), at(200)).unwrap();
    assert!(plain.value["items"][0].get("note_match").is_none());
    assert_match_lines_beside_rows(&plain);
}

#[test]
fn core_next_note_only_matches_resolve_through_listing_and_show_without_changing_focus_or_delivery()
{
    let (_directory, verbs, path, project) = fixture();
    let note_work = add(&verbs, "Unrelated note subject", None, false, 0);
    note(&verbs, &note_work, "core-only-note-token", 1);
    let gate_work = add(&verbs, "Unrelated gate subject", None, false, 2);
    verbs
        .claim(
            ClaimInput {
                work_ref: gate_work.clone(),
                ttl_seconds: Some(600),
                recover: None,
            },
            at(3),
        )
        .unwrap();
    verbs
        .gate(
            GateInput {
                work_ref: Some(gate_work.clone()),
                name: "core-only-gate-token".into(),
                failed: Vec::new(),
                evidence_ref: None,
            },
            at(4),
        )
        .unwrap();
    let foreign = AgentVerbs::new(
        path,
        ProjectId(format!("{}-foreign", project.0)),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let other = add(&foreign, "Foreign item", None, false, 5);
    note(
        &foreign,
        &other,
        "core-only-note-token core-only-gate-token",
        6,
    );
    let before = verbs
        .service
        .work_next(
            20,
            WorkNextQuery {
                sections: vec![WorkNextSection::Focus],
                ..WorkNextQuery::default()
            },
            at(200),
        )
        .unwrap();
    for (query, expected) in [
        ("core-only-note-token", &note_work),
        ("core-only-gate-token", &gate_work),
    ] {
        let view = verbs
            .service
            .work_next(
                20,
                WorkNextQuery {
                    sections: vec![WorkNextSection::Catalog],
                    search: Some(query.into()),
                    ..WorkNextQuery::default()
                },
                at(200),
            )
            .unwrap();
        let rows = &view.catalog.as_ref().unwrap().items;
        assert_eq!(rows.len(), 1);
        assert_eq!(&rows[0].work.short_ref, expected);
        assert!(view.changes.is_none());
        assert!(view.delivered_through.is_none());
        assert!(serde_json::to_vec(&view).unwrap().len() <= MAX_AGENT_WORK_RESPONSE_BYTES);
        let receipt = search(&verbs, query, true);
        assert_eq!(receipt.value["total"], 1);
        assert_eq!(
            receipt.value["items"][0]["ref"].as_str(),
            Some(expected.as_str())
        );
        let full = detail(&verbs, &receipt.value["items"][0], expected);
        assert_eq!(
            full.value["note"]["locator"],
            receipt.value["items"][0]["note_match"]["locator"]
        );
    }
    let after = verbs
        .service
        .work_next(
            20,
            WorkNextQuery {
                sections: vec![WorkNextSection::Focus],
                ..WorkNextQuery::default()
            },
            at(200),
        )
        .unwrap();
    assert_eq!(
        after.session.focused_work_id,
        before.session.focused_work_id
    );
    assert_eq!(
        after.session.confirmed_project_cursor,
        before.session.confirmed_project_cursor
    );
    assert_eq!(
        after.session.pending_delivery,
        before.session.pending_delivery
    );
}

#[test]
fn note_search_inherited_and_restored_late_evidence_follow_lifecycle_and_project_scope() {
    let (directory, verbs, path, project) = fixture();
    let work = add(&verbs, "Completed source", None, false, 0);
    note(&verbs, &work, "inherited-observation-token", 1);
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
    note(&verbs, &work, "inherited-holder-token", 3);
    verbs
        .gate(
            GateInput {
                work_ref: Some(work.clone()),
                name: "inherited-gate-token".into(),
                failed: Vec::new(),
                evidence_ref: Some("test:inherited".into()),
            },
            at(4),
        )
        .unwrap();
    verbs
        .done(
            DoneInput {
                work_ref: Some(work.clone()),
                summary: Some("Completed source".into()),
                ..DoneInput::default()
            },
            at(5),
        )
        .unwrap();
    let mut document = super::review::snapshot(&path, &project, &work);
    for record in &mut document.body.records {
        if let crate::WorkGraphSnapshotRecordPayload::Native { history } = &mut record.payload {
            history
                .events
                .iter_mut()
                .find(|event| event.kind == "claimed")
                .unwrap()
                .reason = Some("history-only-token".into());
        }
    }
    document.manifest.body_sha256 = crate::CanonicalObject::freeze(&document.body)
        .unwrap()
        .key()
        .clone();
    let (restored, _store, restored_path) = super::review::load(directory.path(), &document);
    note(&restored, &work, "restored-late-note-token", 102);
    restored
        .gate(
            GateInput {
                work_ref: Some(work.clone()),
                name: "restored-late-gate-token".into(),
                failed: vec!["late-failure-token".into()],
                evidence_ref: Some("test:late".into()),
            },
            at(103),
        )
        .unwrap();
    for query in [
        "inherited-observation-token",
        "inherited-holder-token",
        "inherited-gate-token",
        "restored-late-note-token",
        "restored-late-gate-token",
        "late-failure-token",
    ] {
        assert_eq!(search(&restored, query, false).value["total"], 0);
        let receipt = search(&restored, query, true);
        assert_eq!(receipt.value["total"], 1, "{query}");
        assert_match_lines_beside_rows(&receipt);
        let full = detail(&restored, &receipt.value["items"][0], &work);
        assert_eq!(
            full.value["note"]["locator"],
            receipt.value["items"][0]["note_match"]["locator"]
        );
    }
    assert_eq!(
        search(&restored, "history-only-token", true).value["total"],
        0
    );
    let foreign = AgentVerbs::new(
        restored_path,
        ProjectId("other-project".into()),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let other = add(&foreign, "Foreign", None, false, 104);
    note(&foreign, &other, "foreign-note-token", 105);
    assert_eq!(
        search(&restored, "foreign-note-token", true).value["total"],
        0
    );
    assert_eq!(
        search(&foreign, "foreign-note-token", false).value["total"],
        1
    );
}

#[test]
fn note_search_deduplicates_and_traverses_bounded_pages_with_membership_continuations() {
    let (_directory, verbs, _path, _project) = fixture();
    let mut expected = Vec::new();
    for index in 0..12 {
        let work = add(
            &verbs,
            &format!(
                "{} item {index} {}",
                if index % 2 == 0 {
                    "needle"
                } else {
                    "unrelated"
                },
                "long ".repeat(30)
            ),
            None,
            false,
            index,
        );
        note(&verbs, &work, "needle in note", 20 + index);
        note(&verbs, &work, "needle in another note", 40 + index);
        expected.push(work);
    }
    let mut input = LsInput {
        search: Some("needle".into()),
        limit: Some(3),
        ..LsInput::default()
    };
    let first = verbs.ls_with_budget(&input, at(200), 2200).unwrap();
    let token = first.value["after"].as_str().unwrap().to_owned();
    note(&verbs, &expected[0], "needle extra match", 61);
    let mut found = Vec::new();
    loop {
        let receipt = verbs.ls_with_budget(&input, at(200), 2200).unwrap();
        assert_eq!(receipt.value["total"], 12);
        assert!(emitted_receipt_bytes(&receipt) <= 2200);
        assert_match_lines_beside_rows(&receipt);
        let rows = receipt.value["items"].as_array().unwrap();
        assert_ne!(rows.len(), 0);
        found.extend(
            rows.iter()
                .map(|row| row["ref"].as_str().unwrap().to_owned()),
        );
        let Some(after) = receipt.value["after"].as_str() else {
            break;
        };
        input.after = Some(after.into());
    }
    found.sort();
    expected.sort();
    assert_eq!(found, expected);
    // The old token is still valid after an additional match within a member.
    input.after = Some(token);
    verbs.ls(&input, at(200)).unwrap();
    let verbose = verbs
        .ls(
            &LsInput {
                verbose: true,
                search: Some("needle".into()),
                ..LsInput::default()
            },
            at(200),
        )
        .unwrap();
    assert!(
        verbose.value["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["note_match"]["locator"].is_string())
    );
    assert_match_lines_beside_rows(&verbose);
    let new = add(&verbs, "Unmatched item", None, false, 70);
    note(&verbs, &new, "needle newly matching item", 71);
    assert!(verbs.ls(&input, at(200)).is_err());
}
