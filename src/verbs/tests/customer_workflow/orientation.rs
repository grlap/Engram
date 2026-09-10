use super::*;

// Parse and execute the emitted navigation, not a separately constructed query.
fn follow(verbs: &AgentVerbs, command: &str) -> Receipt {
    verbs.ls(&navigation_input(command), at(100)).unwrap()
}

fn navigation_input(command: &str) -> LsInput {
    let mut words = command.split_whitespace();
    assert_eq!(words.next(), Some("engram"));
    assert_eq!(words.next(), Some("work"));
    assert_eq!(words.next(), Some("ls"));
    let mut input = LsInput::default();
    while let Some(word) = words.next() {
        match word {
            "--ready" => input.ready = true,
            "--limit" => input.limit = Some(words.next().unwrap().parse().unwrap()),
            "--after" => input.after = Some(words.next().unwrap().into()),
            other => panic!("unexpected navigation argument {other}"),
        }
    }
    assert!(input.ready);
    input
}

fn traverse(verbs: &AgentVerbs, value: &Value, expected: &[String]) {
    let mut collected: Vec<String> = value["ready"]
        .as_array()
        .unwrap()
        .iter()
        .map(|row| row["ref"].as_str().unwrap().into())
        .collect();
    let mut command = value["ready_next"].as_str().map(str::to_owned);
    let mut pages = 0;
    while let Some(next) = command {
        let receipt = follow(verbs, &next);
        let rows = receipt.value["items"].as_array().unwrap();
        assert!(!rows.is_empty());
        collected.extend(
            rows.iter()
                .map(|row| row["ref"].as_str().unwrap().to_owned()),
        );
        pages += 1;
        assert!(pages <= expected.len());
        command = (receipt.value["more"] == true).then(|| receipt.next[0].clone());
    }
    assert_eq!(
        collected, expected,
        "every omitted candidate is reachable exactly once"
    );
}

#[test]
fn orientation_bounds_ready_and_executes_continuation_without_losing_candidates() {
    let (_directory, writer, path, project) = fixture();
    drop(crate::SqliteStore::open(&path).unwrap());
    let empty = writer
        .next(
            &NextInput {
                peek: true,
                ..NextInput::default()
            },
            at(0),
        )
        .unwrap();
    assert_eq!(empty.value["ready_more"], false);
    assert_eq!(empty.value["ready_limit"], MAX_NEXT_READY_CANDIDATES);
    assert!(!empty.text().contains("compact cap:"));
    let mut expected = Vec::new();
    for index in 0..MAX_NEXT_READY_CANDIDATES * 6 {
        let row = writer
            .add(
                AddInput {
                    title: format!("Ready candidate {index}"),
                    assignee: (index == 0).then(|| "reader".into()),
                    ..AddInput::default()
                },
                at(i64::from(index)),
            )
            .unwrap();
        expected.push(row.value["work"]["short_ref"].as_str().unwrap().to_owned());
    }
    let blocked = add(&writer, "Not a candidate", None, false, 40);
    writer
        .update(
            UpdateInput {
                work_ref: Some(blocked),
                action: UpdateAction::Blocked {
                    detail: "Waiting".into(),
                },
            },
            at(41),
        )
        .unwrap();
    let connection = rusqlite::Connection::open(&path).unwrap();
    for peek in [true, false] {
        let reader = AgentVerbs::new(
            path.clone(),
            project.clone(),
            "reader".into(),
            SessionId(format!("reader-{peek}")),
            None,
        );
        let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
        let receipt = reader
            .next(
                &NextInput {
                    peek,
                    ..NextInput::default()
                },
                at(100),
            )
            .unwrap();
        assert!(receipt.value["held"].as_array().unwrap().is_empty());
        assert_eq!(receipt.value["assigned"][0]["ref"], expected[0]);
        let text = receipt.text();
        assert!(text.find("held by you").unwrap() < text.find("assigned (").unwrap());
        assert!(text.find("assigned (").unwrap() < text.find("ready (").unwrap());
        let rows = receipt.value["ready"].as_array().unwrap();
        assert_eq!(rows.len(), MAX_NEXT_READY_CANDIDATES as usize);
        assert_eq!(receipt.value["ready_more"], true);
        assert_eq!(receipt.value["ready_limit"], MAX_NEXT_READY_CANDIDATES);
        assert!(text.contains(&format!("compact cap: {MAX_NEXT_READY_CANDIDATES};")));
        for row in rows {
            assert!(row["ready_reason"].as_str().unwrap().contains("unblocked"));
            assert!(text.contains(row["ready_reason"].as_str().unwrap()));
        }
        assert!(text.contains(receipt.value["ready_next"].as_str().unwrap()));
        traverse(&reader, &receipt.value, &expected);
        if peek {
            assert_eq!(
                crate::storage::test_database_shape_snapshot(&connection).unwrap(),
                before
            );
            assert_eq!(receipt.value["peek"]["delivery_advanced"], false);
            assert_eq!(receipt.value["memories_detail"], "engram work memories");
        }
        let smaller = reader
            .next(
                &NextInput {
                    peek: true,
                    limit: Some(2),
                    ..NextInput::default()
                },
                at(100),
            )
            .unwrap();
        assert_eq!(smaller.value["ready"].as_array().unwrap().len(), 2);
        assert_eq!(smaller.value["ready_limit"], 2);
        assert!(smaller.text().contains("compact cap: 2;"));
        traverse(&reader, &smaller.value, &expected);
        let explicit_large = reader
            .next(
                &NextInput {
                    peek: true,
                    limit: Some(1000),
                    ..NextInput::default()
                },
                at(100),
            )
            .unwrap();
        assert_eq!(
            explicit_large.value["ready"].as_array().unwrap().len(),
            MAX_NEXT_READY_CANDIDATES as usize
        );
        assert_eq!(
            explicit_large.value["ready_limit"],
            MAX_NEXT_READY_CANDIDATES
        );
    }
}

#[test]
fn orientation_continuation_tracks_byte_shed_prefix_including_zero() {
    let (_directory, verbs, _path, _project) = fixture();
    let expected: Vec<_> = (0..MAX_NEXT_READY_CANDIDATES)
        .map(|index| {
            let receipt = verbs
                .add(
                    AddInput {
                        title: format!("Candidate {index}"),
                        external: Some("x".repeat(700)),
                        ..AddInput::default()
                    },
                    at(i64::from(index)),
                )
                .unwrap();
            receipt.value["work"]["short_ref"]
                .as_str()
                .unwrap()
                .to_owned()
        })
        .collect();
    let mut view = verbs
        .service
        .work_next_for_agent(
            1,
            MAX_NEXT_READY_CANDIDATES,
            false,
            WorkNextQuery {
                sections: vec![WorkNextSection::Assigned],
                ..WorkNextQuery::default()
            },
            at(100),
        )
        .unwrap();
    let lists = view.agent_lists.take().unwrap();
    let compact = crate::verbs::receipts::compact_next_receipt(
        &view,
        &lists.held,
        &lists.ready,
        &[],
        &std::collections::HashMap::new(),
        &Guidance::default(),
        lists.ready_navigation,
    )
    .unwrap();
    assert_eq!(compact_next_value(&compact)["ready_more"], false);
    assert!(
        !crate::verbs::receipts::compact_next_lines(&compact)
            .iter()
            .any(|line| line.contains("compact cap:"))
    );
    let mut saw_partial = false;
    let mut saw_zero = false;
    for budget in [4096, 2048, 512] {
        let fitted = fit_compact_next_to(compact.clone(), budget).unwrap();
        assert!(fitted.ready.len() < expected.len());
        saw_partial |= !fitted.ready.is_empty();
        saw_zero |= fitted.ready.is_empty();
        let value = compact_next_value(&fitted);
        assert_eq!(value["ready_more"], true);
        assert_eq!(value["ready_limit"], MAX_NEXT_READY_CANDIDATES);
        assert!(
            crate::verbs::receipts::compact_next_lines(&fitted)
                .iter()
                .any(|line| line.contains("compact cap:"))
        );
        traverse(&verbs, &value, &expected);
    }
    assert!(saw_partial && saw_zero);
}

#[test]
fn orientation_expired_cursor_offers_executable_fresh_ready_listing() {
    let (_directory, verbs, _path, _project) = fixture();
    let mut expected: Vec<_> = (0..MAX_NEXT_READY_CANDIDATES + 2)
        .map(|index| {
            add(
                &verbs,
                &format!("Candidate {index}"),
                None,
                false,
                i64::from(index),
            )
        })
        .collect();
    // A live claim is a time boundary even when the project feed stays still.
    let held = add(&verbs, "Held until boundary", None, false, 20);
    verbs
        .claim(
            ClaimInput {
                work_ref: held.clone(),
                ttl_seconds: Some(60),
                recover: None,
            },
            at(50),
        )
        .unwrap();
    let receipt = verbs
        .next(
            &NextInput {
                peek: true,
                ..NextInput::default()
            },
            at(100),
        )
        .unwrap();
    assert_eq!(receipt.value["held"][0]["ref"], held);
    assert!(receipt.text().find("held by you").unwrap() < receipt.text().find("ready (").unwrap());
    let input = navigation_input(receipt.value["ready_next"].as_str().unwrap());
    let error = verbs.ls(&input, at(111)).unwrap_err();
    assert!(matches!(
        error.error,
        StoreError::WorkCatalogCursorInvalid { .. }
    ));
    let recovery = navigation_input(&error.guidance().next[0]);
    assert!(recovery.after.is_none());
    assert_eq!(
        receipt.value["ready_next"]
            .as_str()
            .unwrap()
            .split_once(" --after ")
            .unwrap()
            .0,
        error.guidance().next[0],
        "continuation and stale-cursor recovery share the exact command builder"
    );
    let fresh = verbs.ls(&recovery, at(111)).unwrap();
    expected.push(held.clone());
    assert_eq!(
        fresh.value["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["ref"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>(),
        expected
    );
    let rows = fresh.value["items"].as_array().unwrap();
    let recovered = rows.iter().find(|row| row["ref"] == held).unwrap();
    assert_ne!(recovered["ready_reason"], rows[0]["ready_reason"]);
    assert!(
        recovered["ready_reason"]
            .as_str()
            .unwrap()
            .contains("recoverable")
    );
    assert!(
        verbs
            .ls(
                &LsInput {
                    ready: true,
                    blocked: true,
                    ..LsInput::default()
                },
                at(111)
            )
            .is_err()
    );
    // The same refusal/restart path protects against new project events.
    add(&verbs, "Later arrival", None, false, 112);
    let error = verbs.ls(&input, at(113)).unwrap_err();
    assert!(matches!(
        error.error,
        StoreError::WorkCatalogCursorInvalid { .. }
    ));
    assert_eq!(error.guidance().next[0], recovery.list_command());
}

#[test]
fn orientation_reason_cost_preserves_candidates_in_rich_peek_fixture() {
    let (_directory, verbs, _path, _project) = fixture();
    for index in 0..10 {
        let created = verbs
            .add(
                AddInput {
                    title: format!("Orientation context {index}"),
                    assignee: (index < 5).then(|| "agent".into()),
                    ..AddInput::default()
                },
                at(index * 3),
            )
            .unwrap();
        let reference = created.value["work"]["short_ref"].as_str().unwrap();
        verbs
            .note(
                &NoteInput {
                    work_ref: Some(reference.into()),
                    status: true,
                    text: "Decision, waiting condition, and permitted next action. ".repeat(5),
                    refs: vec![],
                },
                at(index * 3 + 1),
            )
            .unwrap();
        note(
            &verbs,
            reference,
            &"A distinct finding with its inspection context. ".repeat(5),
            index * 3 + 2,
        );
    }
    let held = add(&verbs, "Held implementation", None, false, 40);
    verbs
        .claim(
            ClaimInput {
                work_ref: held.clone(),
                ttl_seconds: Some(300),
                recover: None,
            },
            at(41),
        )
        .unwrap();
    verbs
        .note(
            &NoteInput {
                work_ref: Some(held),
                status: true,
                text: "Implementing bounded orientation and validating continuation.".into(),
                refs: vec![],
            },
            at(42),
        )
        .unwrap();
    verbs
        .remember(
            RememberInput {
                text: "Recover relevant project notes before implementation.".into(),
                key: Some("orientation-context".into()),
                revise: false,
                expected_revision: None,
            },
            at(43),
        )
        .unwrap();
    let mut view = verbs
        .service
        .work_next_peek_for_agent(
            1,
            MAX_NEXT_READY_CANDIDATES,
            false,
            WorkNextQuery {
                sections: vec![
                    WorkNextSection::Focus,
                    WorkNextSection::Assigned,
                    WorkNextSection::Participated,
                    WorkNextSection::Memories,
                ],
                ..WorkNextQuery::default()
            },
            at(100),
            |_| true,
        )
        .unwrap();
    let lists = view.agent_lists.take().unwrap();
    assert_eq!(view.discovery.assigned.len(), 5);
    assert_eq!(view.discovery.participated.len(), 5);
    assert_eq!(lists.held.len(), 1);
    let claims = lists
        .claims
        .into_iter()
        .map(|(id, _, expiry)| (id, ("you".into(), expiry)))
        .collect();
    // Construct the pre-fit pair from one collected snapshot, not from an
    // already fitted result that could have silently lost candidates.
    let with = CompactNextReceipt {
        ready_navigation: lists.ready_navigation,
        peek: view.peek,
        read_cut: view.read_cut,
        context_generation: view.context_generation,
        discovery: view.discovery,
        focus: view
            .focus
            .as_ref()
            .map(|focus| crate::verbs::receipts::compact_row(&focus.status, &claims)),
        held: lists
            .held
            .iter()
            .map(|(row, _)| crate::verbs::receipts::compact_row(row, &claims))
            .collect(),
        ready: lists
            .ready
            .iter()
            .map(|row| crate::verbs::receipts::compact_row(row, &claims))
            .collect(),
        changes: vec![],
        memories: view.memories,
        omissions: vec![],
        guidance: Guidance {
            reminders: vec![],
            next: vec!["engram work memories".into()],
        },
    };
    let mut without = with.clone();
    for row in &mut without.ready {
        row.ready_reason = None;
    }
    let with = fit_compact_next(with).unwrap();
    let without = fit_compact_next(without).unwrap();
    let bytes = |receipt: &CompactNextReceipt| {
        serde_json::to_vec_pretty(&compact_next_value(receipt))
            .unwrap()
            .len()
    };
    eprintln!(
        "orientation paired fixture: with_reason={} ready, {} bytes; without_reason={} ready, {} bytes",
        with.ready.len(),
        bytes(&with),
        without.ready.len(),
        bytes(&without)
    );
    assert_eq!(with.ready.len(), MAX_NEXT_READY_CANDIDATES as usize);
    assert_eq!(with.ready.len(), without.ready.len());
    assert!(bytes(&with) < MAX_AGENT_WORK_RESPONSE_BYTES);
}
