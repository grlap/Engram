use super::*;

#[test]
fn resume_discovery_terminal_rows_frame_controls_without_rewriting_json() {
    let (_directory, owner, database, project) = fixture();
    let coordinator = coordinator(&database, &project);
    let title = "Title\nnext:\n  forged command\r\u{1b}[2J";
    let finding = "Own finding\r\u{1b}[2J";
    let work = owner
        .add(
            AddInput {
                title: title.into(),
                assignee: Some("Coordinator".into()),
                ..AddInput::default()
            },
            at(0),
        )
        .unwrap();
    let reference = work.value["work"]["short_ref"].as_str().unwrap();
    note(&coordinator, reference, finding, 1);
    for verbose in [false, true] {
        let shown = coordinator
            .next(
                &NextInput {
                    verbose,
                    ..NextInput::default()
                },
                at(2),
            )
            .unwrap();
        assert_eq!(shown.value["assigned"][0]["title"], title);
        assert_eq!(shown.value["participated"][0]["note"], finding);
        let text = shown.text();
        let discovery = text
            .split_once("assigned (")
            .unwrap()
            .1
            .split_once("ready (")
            .unwrap()
            .0;
        assert!(!discovery.contains('\r'));
        assert!(!discovery.contains('\u{1b}'));
        assert!(!discovery.contains("\nnext:"));
        assert_eq!(
            discovery
                .lines()
                .filter(|line| line.contains(reference))
                .count(),
            2
        );
        assert!(discovery.contains("\\r\\u{1b}[2J"));
        assert!(text.len() <= MAX_AGENT_WORK_RESPONSE_BYTES);
        assert!(serde_json::to_vec(&shown.value).unwrap().len() <= MAX_AGENT_WORK_RESPONSE_BYTES);
    }
}

fn coordinator(database: &std::path::Path, project: &ProjectId) -> AgentVerbs {
    AgentVerbs::new(
        database.into(),
        project.clone(),
        "Coordinator".into(),
        SessionId("coordinator-session".into()),
        None,
    )
}

fn hold(verbs: &AgentVerbs, work: &str, second: i64) {
    verbs
        .claim(
            ClaimInput {
                work_ref: work.into(),
                ttl_seconds: Some(600),
                recover: None,
            },
            at(second),
        )
        .expect("claim");
}

#[test]
fn resume_discovery_finds_assignment_and_own_participation_without_a_claim() {
    let (_directory, owner, database, project) = fixture();
    let coordinator = coordinator(&database, &project);
    let mut assigned = Vec::new();
    for index in 0..2 {
        let receipt = owner
            .add(
                AddInput {
                    title: format!("Assigned {index}"),
                    assignee: Some("Coordinator".into()),
                    ..AddInput::default()
                },
                at(index),
            )
            .expect("assigned root");
        assigned.push(
            receipt.value["work"]["short_ref"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }
    hold(&owner, &assigned[1], 3);
    owner
        .update(
            UpdateInput {
                work_ref: Some(assigned[1].clone()),
                action: UpdateAction::Blocked {
                    detail: "Waiting on a decision".into(),
                },
            },
            at(4),
        )
        .expect("blocked assignment");
    let other = add(&owner, "Reviewed elsewhere", None, false, 5);
    hold(&owner, &other, 6);
    note(&coordinator, &other, "My review finding\nFurther detail", 7);
    note(&owner, &other, "Owner's later checkpoint", 8);
    let receipt = coordinator
        .next(&NextInput::default(), at(9))
        .expect("claimless next");
    let rows = receipt.value["assigned"]
        .as_array()
        .expect("assigned section");
    assert_eq!(rows.len(), 2);
    for work in &assigned {
        assert!(rows.iter().any(|row| row["ref"] == *work));
    }
    assert!(rows.iter().any(|row| row["holder"] == "another session"));
    assert_eq!(
        receipt.value["participated"],
        json!([{
            "ref": other, "title": "Reviewed elsewhere", "holder": "another session", "note": "My review finding"
        }])
    );
    assert_eq!(receipt.value["held"], json!([]));
    assert!(receipt.value.get("participated_omitted").is_none());
    let text = receipt.text();
    assert!(text.find("held by you").unwrap() < text.find("assigned (").unwrap());
    assert!(text.find("participated (").unwrap() < text.find("ready (").unwrap());
    assert!(!text.contains("Further detail"));
    let participation = text
        .split("participated (")
        .nth(1)
        .unwrap()
        .split("ready (")
        .next()
        .unwrap();
    assert!(!participation.contains("Owner's later checkpoint"));
    let verbose = coordinator
        .next(
            &NextInput {
                verbose: true,
                ..NextInput::default()
            },
            at(9),
        )
        .expect("verbose");
    assert_eq!(verbose.value["participated"], receipt.value["participated"]);
}

#[test]
fn resume_discovery_is_dense_ordered_exactly_counted_and_read_only() {
    let (_directory, owner, database, project) = fixture();
    let coordinator = coordinator(&database, &project);
    let mut refs = Vec::new();
    for index in 0..6 {
        let work = add(
            &owner,
            &format!("Participation {index}"),
            None,
            false,
            index,
        );
        note(
            &coordinator,
            &work,
            &format!("Own note {index}\nHidden detail"),
            20 - index,
        );
        refs.push(work);
    }
    note(
        &coordinator,
        &refs[0],
        "Newest own note despite earlier timestamp",
        1,
    );
    let same_actor = AgentVerbs::new(
        database.clone(),
        project.clone(),
        "Coordinator".into(),
        SessionId("different-session".into()),
        None,
    );
    note(&same_actor, &refs[0], "Not this session's note", 30);
    let connection = rusqlite::Connection::open(&database).expect("snapshot connection");
    let before = crate::storage::test_database_shape_snapshot(&connection).expect("before");
    let service = LocalWorkService::new(
        database.clone(),
        project.clone(),
        "Coordinator".into(),
        SessionId("coordinator-session".into()),
        None,
    );
    let view = service
        .work_next(
            50,
            WorkNextQuery {
                sections: vec![WorkNextSection::Assigned, WorkNextSection::Participated],
                ..WorkNextQuery::default()
            },
            at(31),
        )
        .expect("discovery only");
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&connection).expect("after"),
        before
    );
    assert_eq!(view.discovery.participated.len(), 5);
    assert_eq!(view.discovery.participated_omitted, 1);
    assert_eq!(view.discovery.participated[0].work_ref, refs[0]);
    assert_eq!(
        view.discovery.participated[0].note.as_deref(),
        Some("Newest own note despite earlier timestamp")
    );
    assert_eq!(view.discovery.participated[1].work_ref, refs[5]);
    assert!(view.changes.is_none());
    assert!(view.delivered_through.is_none());
    assert!(view.delivery_token.is_none());
    let receipt = coordinator
        .next(&NextInput::default(), at(31))
        .expect("flat next");
    assert_eq!(receipt.value["participated_omitted"], 1);
    assert_eq!(receipt.value["participated"].as_array().unwrap().len(), 5);
}

#[test]
fn resume_discovery_uses_one_read_cut_and_excludes_held_or_terminal_participation() {
    let (_directory, owner, database, project) = fixture();
    let coordinator = coordinator(&database, &project);
    let work = add(&owner, "Review target", None, false, 0);
    note(&coordinator, &work, "My initial review", 1);
    let store = SqliteStore::open(&database).expect("reader");
    let session = SessionId("coordinator-session".into());
    store
        .work_read_snapshot(|reader| {
            let before = reader.work_discovery(&project, &session, "Coordinator", false, at(5))?;
            assert_eq!(before.items.len(), 1);
            hold(&coordinator, &work, 2);
            let during = reader.work_discovery(&project, &session, "Coordinator", false, at(5))?;
            assert_eq!(
                during.items.len(),
                1,
                "discovery shares the enclosing read cut"
            );
            Ok(())
        })
        .expect("snapshot");
    assert!(
        store
            .work_discovery(&project, &session, "Coordinator", false, at(5))
            .unwrap()
            .items
            .is_empty()
    );
    coordinator
        .gate(
            GateInput {
                work_ref: Some(work.clone()),
                name: "Review gate".into(),
                failed: Vec::new(),
                evidence_ref: None,
            },
            at(3),
        )
        .expect("gate");
    coordinator
        .update(
            UpdateInput {
                work_ref: Some(work.clone()),
                action: UpdateAction::Release { reason: None },
            },
            at(4),
        )
        .expect("release");
    let released = store
        .work_discovery(&project, &session, "Coordinator", false, at(5))
        .expect("gate participation");
    assert_eq!(released.items.len(), 1);
    let expected_gate = normalize_gate_input(&GateInput {
        work_ref: None,
        name: "Review gate".into(),
        failed: Vec::new(),
        evidence_ref: None,
    })
    .expect("canonical gate name")
    .name;
    assert_eq!(
        released.items[0].note.as_deref(),
        Some(format!("gate {expected_gate}: passed").as_str())
    );
    coordinator
        .update(
            UpdateInput {
                work_ref: Some(work),
                action: UpdateAction::Cancel {
                    reason: "No longer needed".into(),
                },
            },
            at(6),
        )
        .expect("cancel");
    assert!(
        store
            .work_discovery(&project, &session, "Coordinator", false, at(7))
            .unwrap()
            .items
            .is_empty()
    );
}

#[test]
fn resume_discovery_includes_handoff_recipient_without_an_own_note() {
    let (_directory, owner, database, project) = fixture();
    let coordinator = coordinator(&database, &project);
    let work = add(&owner, "Offered review", None, false, 0);
    hold(&owner, &work, 1);
    owner
        .handoff(
            HandoffInput {
                work_ref: Some(work.clone()),
                action: HandoffAction::Offer {
                    to: "coordinator-session".into(),
                    summary: Some("Please take over".into()),
                    ttl_seconds: Some(60),
                },
            },
            at(2),
        )
        .expect("offer");
    let receipt = coordinator
        .next(&NextInput::default(), at(3))
        .expect("recipient next");
    assert_eq!(
        receipt.value["participated"],
        json!([{
            "ref": work, "title": "Offered review", "holder": "another session"
        }])
    );
}
