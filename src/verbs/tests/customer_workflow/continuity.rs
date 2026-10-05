use super::*;
use crate::work_service::{SAME_ACTOR_OTHER_SESSION, WorkActorDefaultSource};

fn session(
    database: &std::path::Path,
    project: &ProjectId,
    actor: &str,
    session: &str,
    defaulted: bool,
) -> AgentVerbs {
    AgentVerbs::new_with_attribution(
        database.into(),
        project.clone(),
        actor.into(),
        SessionId(session.into()),
        None,
        None,
        crate::WorkAttributionDefaults {
            actor: defaulted.then_some(WorkActorDefaultSource::OsUserEnvironment),
            session: false,
        },
    )
}

fn participated(receipt: &Receipt) -> Vec<Value> {
    receipt.value["participated"]
        .as_array()
        .cloned()
        .unwrap_or_default()
}

#[test]
fn status_relation_needs_the_same_asserted_actor_on_both_sides() {
    // (writer actor, writer defaulted, reader defaulted, relation shown)
    for (writer_actor, writer_defaulted, reader_defaulted, related) in [
        ("Coordinator", false, false, true),
        ("Coordinator", true, false, false),
        ("Coordinator", false, true, false),
        ("Reviewer", false, false, false),
    ] {
        let (_directory, owner, database, project) = fixture();
        let work = owner
            .add(
                AddInput {
                    title: "Accountable duty".into(),
                    assignee: Some("Coordinator".into()),
                    ..AddInput::default()
                },
                at(0),
            )
            .unwrap()
            .value["work"]["short_ref"]
            .as_str()
            .unwrap()
            .to_owned();
        let writer = session(
            &database,
            &project,
            writer_actor,
            "old-session",
            writer_defaulted,
        );
        writer
            .claim(
                ClaimInput {
                    work_ref: work.clone(),
                    ttl_seconds: Some(600),
                    recover: None,
                },
                at(1),
            )
            .unwrap();
        writer
            .note(
                &NoteInput {
                    status: true,
                    work_ref: Some(work.clone()),
                    text: "Waiting for review".into(),
                    refs: vec![],
                },
                at(2),
            )
            .unwrap();
        let reader = session(
            &database,
            &project,
            "Coordinator",
            "new-session",
            reader_defaulted,
        );
        let shown = reader.show(&work, at(3)).unwrap();
        let case = format!("{writer_actor} {writer_defaulted} {reader_defaulted}");
        assert_eq!(
            shown.value["current_status"]["body_or_first_line"], "Waiting for review",
            "{case}"
        );
        assert_eq!(
            shown.value["current_status"]["by_relation"],
            if related {
                json!(SAME_ACTOR_OTHER_SESSION)
            } else {
                Value::Null
            },
            "{case}"
        );
        assert_eq!(
            shown
                .text()
                .contains("(same asserted actor, another session)"),
            related,
            "{case}"
        );
        let next = reader.next(&NextInput::default(), at(4)).unwrap();
        assert_eq!(
            next.text()
                .contains("(same asserted actor, another session)"),
            related,
            "{case}"
        );
    }
}

#[test]
fn continuity_lists_the_same_asserted_actors_work_from_another_session() {
    let (_directory, owner, database, project) = fixture();
    let earlier = session(&database, &project, "Coordinator", "old-session", false);
    let replacement = session(&database, &project, "Coordinator", "new-session", false);
    let own = add(&owner, "Noted by this session", None, false, 0);
    let elsewhere = add(&owner, "Noted by an earlier session", None, false, 1);
    note(&earlier, &elsewhere, "Earlier finding\nHidden detail", 2);
    note(&replacement, &own, "This session's finding", 3);
    let marker = " [note by same asserted actor, another session]";
    for verbose in [false, true] {
        let receipt = replacement
            .next(
                &NextInput {
                    verbose,
                    ..NextInput::default()
                },
                at(4),
            )
            .unwrap();
        let rows = participated(&receipt);
        // The session's own participation comes first, continuity after it.
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["ref"], own);
        assert!(rows[0].get("continuity").is_none());
        let row = &rows[1];
        assert_eq!(row["ref"], elsewhere);
        assert_eq!(row["note"], "Earlier finding");
        assert_eq!(row["continuity"], SAME_ACTOR_OTHER_SESSION);
        // Never "you" and never the other session's raw spelling.
        assert!(row.get("note_session_id").is_none());
        assert!(row.get("note_by").is_none());
        assert!(!receipt.value.to_string().contains("old-session"));
        let expected = format!(
            "  {elsewhere} \"Noted by an earlier session\" (unclaimed){marker} — Earlier finding"
        );
        assert!(receipt.text().lines().any(|line| line == expected));
        assert!(!receipt.text().contains("Hidden detail"));
    }
}

#[test]
fn continuity_excludes_defaulted_actors_on_either_side() {
    let (_directory, owner, database, project) = fixture();
    let work = add(&owner, "Defaulted attribution", None, false, 0);
    let defaulted_writer = session(&database, &project, "Coordinator", "old-session", true);
    note(&defaulted_writer, &work, "A shell-defaulted note", 1);
    let asserted_reader = session(&database, &project, "Coordinator", "new-session", false);
    let receipt = asserted_reader.next(&NextInput::default(), at(2)).unwrap();
    assert_eq!(participated(&receipt), Vec::<Value>::new());
    assert!(!receipt.text().contains("another session]"));

    let asserted_work = add(&owner, "Asserted attribution", None, false, 3);
    let asserted_writer = session(&database, &project, "Coordinator", "older-session", false);
    note(&asserted_writer, &asserted_work, "An asserted note", 4);
    let defaulted_reader = session(&database, &project, "Coordinator", "third-session", true);
    let receipt = defaulted_reader.next(&NextInput::default(), at(5)).unwrap();
    // A defaulted reader's actor id proves nothing about continuity.
    assert_eq!(participated(&receipt), Vec::<Value>::new());
    // The asserted reader still finds the asserted note, never the defaulted one.
    let receipt = asserted_reader.next(&NextInput::default(), at(6)).unwrap();
    let rows = participated(&receipt);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["ref"], asserted_work);
    assert_eq!(rows[0]["continuity"], SAME_ACTOR_OTHER_SESSION);
}

#[test]
fn continuity_excludes_another_actor_in_another_session() {
    let (_directory, owner, database, project) = fixture();
    let work = add(&owner, "Another actor's work", None, false, 0);
    let other = session(&database, &project, "Reviewer", "old-session", false);
    note(&other, &work, "Another actor's finding", 1);
    let reader = session(&database, &project, "Coordinator", "new-session", false);
    for verbose in [false, true] {
        let receipt = reader
            .next(
                &NextInput {
                    verbose,
                    ..NextInput::default()
                },
                at(2),
            )
            .unwrap();
        assert_eq!(participated(&receipt), Vec::<Value>::new());
        assert!(!receipt.text().contains("another session]"));
    }
}

#[test]
fn continuity_inherits_no_claim_or_authority() {
    let (_directory, owner, database, project) = fixture();
    let earlier = session(&database, &project, "Coordinator", "old-session", false);
    let replacement = session(&database, &project, "Coordinator", "new-session", false);
    let work = add(&owner, "Held by the earlier session", None, false, 0);
    earlier
        .claim(
            ClaimInput {
                work_ref: work.clone(),
                ttl_seconds: Some(600),
                recover: None,
            },
            at(1),
        )
        .unwrap();
    note(&earlier, &work, "Earlier session's checkpoint", 2);
    let receipt = replacement.next(&NextInput::default(), at(3)).unwrap();
    assert_eq!(receipt.value["held"], json!([]));
    let rows = participated(&receipt);
    assert_eq!(rows.len(), 1);
    assert_eq!(rows[0]["continuity"], SAME_ACTOR_OTHER_SESSION);
    assert_eq!(
        rows[0]["holder"],
        replacement
            .service
            .display_identity()
            .session(&SessionId("old-session".into()))
    );
    assert_ne!(rows[0]["holder"], "you");
    // The row is navigation: the replacement cannot claim, revise or finish it.
    assert!(
        replacement
            .claim(
                ClaimInput {
                    work_ref: work.clone(),
                    ttl_seconds: Some(600),
                    recover: None,
                },
                at(4),
            )
            .is_err()
    );
    let revision = serde_json::from_value(
        json!({"work_ref": work, "action": {"action": "revise", "title": "Unauthorized change"}}),
    )
    .unwrap();
    assert!(replacement.update(revision, at(5)).is_err());
    let shown = earlier.next(&NextInput::default(), at(6)).unwrap();
    assert_eq!(shown.value["held"][0]["ref"], work);
}

#[test]
fn continuity_shares_the_participation_budget_after_own_rows() {
    let (_directory, owner, database, project) = fixture();
    let earlier = session(&database, &project, "Coordinator", "old-session", false);
    let replacement = session(&database, &project, "Coordinator", "new-session", false);
    let mut own = Vec::new();
    for index in 0..4 {
        let work = add(&owner, &format!("Own {index}"), None, false, index);
        note(
            &replacement,
            &work,
            &format!("Own note {index}"),
            10 + index,
        );
        own.push(work);
    }
    for index in 0..3 {
        let work = add(&owner, &format!("Earlier {index}"), None, false, 20 + index);
        note(
            &earlier,
            &work,
            &format!("Earlier note {index}"),
            30 + index,
        );
    }
    let receipt = replacement.next(&NextInput::default(), at(40)).unwrap();
    let rows = participated(&receipt);
    assert_eq!(rows.len(), crate::storage::DISCOVERY_ROWS);
    for row in &rows[..4] {
        assert!(own.iter().any(|work| row["ref"] == *work));
        assert!(row.get("continuity").is_none());
    }
    assert_eq!(rows[4]["continuity"], SAME_ACTOR_OTHER_SESSION);
    assert_eq!(receipt.value["participated_omitted"], 2);
}
