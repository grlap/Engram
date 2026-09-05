use super::*;

#[test]
fn phoenix_handoff_acceptor_receives_peer_proposal() {
    let (_directory, owner, database, project) = fixture();
    let acceptor = AgentVerbs::new(
        database.clone(),
        project.clone(),
        "acceptor".into(),
        SessionId("acceptor".into()),
        None,
    );
    let peer = AgentVerbs::new(
        database.clone(),
        project.clone(),
        "peer".into(),
        SessionId("peer".into()),
        None,
    );
    let parent = add(&owner, "Pending handoff parent", None, false, 0);
    owner
        .claim(
            ClaimInput {
                work_ref: parent.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(1),
        )
        .expect("claim");
    owner
        .handoff(
            HandoffInput {
                work_ref: Some(parent.clone()),
                action: HandoffAction::Offer {
                    to: "acceptor".into(),
                    summary: Some("Transfer ownership".into()),
                    ttl_seconds: Some(60),
                },
            },
            at(2),
        )
        .expect("offer");
    let child = add(&peer, "Proposed during offer", Some(&parent), true, 3);
    acceptor
        .handoff(
            HandoffInput {
                work_ref: Some(parent),
                action: HandoffAction::Accept,
            },
            at(4),
        )
        .expect("accept");
    let store = SqliteStore::open(database).expect("store");
    let head = store
        .work_feed_head(&crate::FeedId::Project(project.clone()))
        .expect("head");
    let mut saw_child = false;
    for _ in 0..=head {
        let page = acceptor.next(&NextInput::default(), at(5)).expect("next");
        saw_child |=
            page.text().contains("peer optional-child proposal") && page.text().contains(&child);
        let state = store
            .work_session_state(&project, &SessionId("acceptor".into()), at(5))
            .expect("session");
        if state.tentative_project_cursor == Some(head) {
            break;
        }
    }
    assert!(
        saw_child,
        "acceptor must receive the peer proposal through next"
    );
}

#[test]
fn phoenix_initial_note_input_refuses_before_opening_the_store() {
    let (_directory, verbs, database, _) = fixture();
    for notes in [
        vec!["ok".into(), "  ".into()],
        vec!["x".into(); crate::domain::MAX_INITIAL_WORK_NOTES + 1],
    ] {
        let refusal = verbs
            .add(
                AddInput {
                    title: "Invalid notes".into(),
                    notes,
                    ..AddInput::default()
                },
                at(0),
            )
            .expect_err("prevalidate");
        assert!(matches!(refusal.error, StoreError::InvalidWork(_)));
        assert!(
            !database.exists(),
            "invalid input must not initialize or mutate the store"
        );
    }
}

#[test]
fn phoenix_add_initial_notes_cover_roots_children_and_peer_proposals() {
    let (_directory, owner, database, project) = fixture();
    let peer = AgentVerbs::new(
        database.clone(),
        project.clone(),
        "agent".into(),
        SessionId("peer".into()),
        None,
    );
    let parent = add(&owner, "Parent", None, false, 0);
    for shape in ["root", "unclaimed-child", "held-child", "peer-child"] {
        if shape == "held-child" {
            owner
                .claim(
                    ClaimInput {
                        work_ref: parent.clone(),
                        ttl_seconds: None,
                        recover: None,
                    },
                    at(3),
                )
                .expect("hold parent");
        }
        let verbs = if shape == "peer-child" { &peer } else { &owner };
        if shape == "peer-child" {
            // Establish delivery before the peer append; the earlier fixture
            // history is larger than one ordinary compact changes page.
            drain_fixture(&owner, &database, &project);
        }
        let notes = vec![
            "First\nobservation".into(),
            "Repeated".into(),
            "Repeated".into(),
        ];
        let receipt = verbs
            .add(
                AddInput {
                    title: shape.into(),
                    under: (shape != "root").then(|| parent.clone()),
                    optional: shape == "peer-child",
                    notes: notes.clone(),
                    ..AddInput::default()
                },
                at(4),
            )
            .expect("add with initial observations");
        assert!(
            receipt
                .text()
                .contains("initial observations (no execution credit)")
        );
        let child_ref = receipt.value["work"]["short_ref"].as_str().expect("ref");
        let shown = verbs
            .show_with_notes(child_ref, true, at(5))
            .expect("full initial notes");
        let actual = shown.value["notes"].as_array().expect("notes");
        assert_eq!(actual.len(), notes.len(), "{shape}");
        for (note, expected) in actual.iter().zip(&notes) {
            assert_eq!(note["summary"], *expected);
            assert_eq!(note["non_holder"], true);
        }
    }
    let refusal = peer
        .add(
            AddInput {
                title: "Required peer".into(),
                under: Some(parent),
                ..AddInput::default()
            },
            at(6),
        )
        .expect_err("required peer refusal");
    let error = &refusal.error;
    assert!(matches!(
        error,
        StoreError::WorkPeerDecompositionRefused { .. }
    ));
    let payload = crate::mcp::store_error_value(error);
    assert!(
        payload["error"]["details"]["remedy"]
            .as_str()
            .expect("remedy")
            .contains("parent holder")
    );
    let page = owner
        .next(&NextInput::default(), at(7))
        .expect("holder next");
    assert!(
        page.text().contains("peer optional-child proposal"),
        "{}",
        page.text()
    );
    let store = SqliteStore::open(&database).expect("store");
    assert!(store.verify_all().expect("integrity").is_healthy());
}

fn drain_fixture(owner: &AgentVerbs, database: &std::path::Path, project: &ProjectId) {
    let store = SqliteStore::open(database).expect("store");
    let head = store
        .work_feed_head(&crate::FeedId::Project(project.clone()))
        .expect("head");
    for _ in 0..=head {
        owner
            .next(&NextInput::default(), at(4))
            .expect("drain fixture page");
        let state = store
            .work_session_state(project, &SessionId("agent".into()), at(4))
            .expect("session");
        if state.tentative_project_cursor == Some(head) {
            return;
        }
    }
    panic!("fixture pages did not advance to the observed feed head");
}
