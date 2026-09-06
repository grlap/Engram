use super::*;

mod corrections;

#[test]
fn listing_continuation_enumerates_the_exact_byte_bounded_direct_child_set() {
    let (_directory, verbs, path, _) = fixture();
    let parent = add(&verbs, "Parent", None, false, 0);
    let mut expected = Vec::new();
    for index in 0..64 {
        let receipt = verbs
            .add(
                AddInput {
                    title: format!("Matching Straße {index} {}", "\\\"".repeat(100)),
                    labels: vec!["Größe".into()],
                    assignee: Some("AGENT".into()),
                    under: Some(parent.clone()),
                    optional: true,
                    ..AddInput::default()
                },
                at(index + 1),
            )
            .unwrap();
        expected.push(
            receipt.value["work"]["short_ref"]
                .as_str()
                .unwrap()
                .to_owned(),
        );
    }
    let required = add(&verbs, "Required", Some(&parent), false, 70);
    add(&verbs, "Grandchild", Some(&required), true, 71);
    add(&verbs, "Unrelated", None, false, 72);
    let store = crate::SqliteStore::open(&path).unwrap();
    let connection = rusqlite::Connection::open(&path).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
    for verbose in [false, true] {
        let mut input = LsInput {
            under: Some(parent.clone()),
            optional: true,
            search: Some("MATCHING STRASSE".into()),
            label: Some("GRÖSSE".into()),
            mine: true,
            limit: Some(1000),
            verbose,
            ..LsInput::default()
        };
        let mut collected = Vec::new();
        let mut pages = 0;
        loop {
            let receipt = verbs.ls(&input, at(100)).unwrap();
            let rows = receipt.value["items"].as_array().unwrap();
            assert!(!rows.is_empty());
            assert_eq!(receipt.value["total"], expected.len());
            assert_eq!(receipt.value["shown_before"], collected.len());
            assert_eq!(receipt.value["limit"], 1000);
            assert_eq!(receipt.value["byte_budget"], MAX_AGENT_WORK_RESPONSE_BYTES);
            assert!(receipt.text().contains("--limit 1000; byte budget 12288"));
            assert!(receipt.text().len() <= MAX_AGENT_WORK_RESPONSE_BYTES);
            assert!(
                serde_json::to_vec_pretty(&receipt.value).unwrap().len()
                    <= MAX_AGENT_WORK_RESPONSE_BYTES
            );
            for row in rows {
                collected.push(
                    if verbose {
                        &row["work"]["short_ref"]
                    } else {
                        &row["ref"]
                    }
                    .as_str()
                    .unwrap()
                    .to_owned(),
                );
            }
            assert_eq!(receipt.value["omitted"], expected.len() - collected.len());
            pages += 1;
            assert!(pages <= expected.len(), "each continuation makes progress");
            let Some(token) = receipt.value["after"].as_str() else {
                assert_eq!(receipt.value["more"], false);
                break;
            };
            assert_eq!(
                receipt.value["next"],
                json!([format!("{} --after {token}", input.list_command())])
            );
            assert!(receipt.text().contains(&format!("--after {token}")));
            input.after = Some(token.to_owned());
            // Equivalent Unicode/case filters retain the same cursor identity.
            input.search = Some("matching straße".into());
            input.label = Some("größe".into());
        }
        assert!(pages > 1, "fixture must exceed the final byte budget");
        assert_eq!(collected, expected);
    }
    let only_required = verbs
        .ls(
            &LsInput {
                under: Some(parent),
                required: true,
                ..LsInput::default()
            },
            at(100),
        )
        .unwrap();
    assert_eq!(only_required.value["total"], 1);
    assert_eq!(only_required.value["items"][0]["ref"], required);
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&connection).unwrap(),
        before
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn listing_cursor_rejects_changed_filters_malformed_tokens_and_new_project_events() {
    let (_directory, verbs, path, project) = fixture();
    let first = add(&verbs, "First", None, false, 0);
    add(&verbs, "Second", None, false, 1);
    let original = LsInput {
        limit: Some(1),
        ..LsInput::default()
    };
    let page = verbs.ls(&original, at(2)).unwrap();
    let token = page.value["after"].as_str().unwrap().to_owned();
    let continued = LsInput {
        after: Some(token.clone()),
        ..original.clone()
    };
    for input in [
        LsInput {
            all: true,
            ..continued.clone()
        },
        LsInput {
            label: Some("other".into()),
            ..continued.clone()
        },
        LsInput {
            after: Some("not-a-cursor".into()),
            ..original.clone()
        },
        LsInput {
            after: Some("c1-ff".into()),
            ..original.clone()
        },
    ] {
        let error = verbs.ls(&input, at(3)).unwrap_err();
        assert!(matches!(
            error.error,
            StoreError::WorkCatalogCursorInvalid { .. }
        ));
        assert_eq!(error.guidance().next, vec![input.list_command()]);
        assert!(!error.to_string().contains(&token));
    }
    let other_project = AgentVerbs::new(
        path.clone(),
        ProjectId("other-project".into()),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    assert!(matches!(
        other_project.ls(&continued, at(3)).unwrap_err().error,
        StoreError::WorkCatalogCursorInvalid { .. }
    ));
    let other_session = AgentVerbs::new(
        path.clone(),
        project,
        "agent".into(),
        SessionId("peer".into()),
        None,
    );
    // No mine filter: a focus-only change and another reader do not move the cut.
    other_session.show(&first, at(3)).unwrap();
    assert_eq!(
        other_session.ls(&continued, at(4)).unwrap().value["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    note(&verbs, &first, "Changes the project feed", 5);
    let connection = rusqlite::Connection::open(&path).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
    let error = verbs.ls(&continued, at(6)).unwrap_err();
    assert!(matches!(
        error.error,
        StoreError::WorkCatalogCursorInvalid { .. }
    ));
    assert_eq!(error.guidance().next, vec![original.list_command()]);
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&connection).unwrap(),
        before
    );
    assert!(verbs.ls(&original, at(6)).is_ok());
}

#[test]
fn listing_cursor_refuses_expiry_and_clock_reversal_without_any_write() {
    let (_directory, verbs, path, _) = fixture();
    let first = add(&verbs, "First", None, false, 0);
    add(&verbs, "Second", None, false, 1);
    verbs
        .claim(
            ClaimInput {
                work_ref: first,
                ttl_seconds: Some(10),
                recover: None,
            },
            at(2),
        )
        .unwrap();
    let input = LsInput {
        limit: Some(1),
        ..LsInput::default()
    };
    let page = verbs.ls(&input, at(3)).unwrap();
    let input = LsInput {
        after: Some(page.value["after"].as_str().unwrap().into()),
        ..input
    };
    assert!(verbs.ls(&input, at(11)).is_ok());
    let connection = rusqlite::Connection::open(&path).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
    for now in [2, 12, 13] {
        assert!(matches!(
            verbs.ls(&input, at(now)).unwrap_err().error,
            StoreError::WorkCatalogCursorInvalid { .. }
        ));
    }
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&connection).unwrap(),
        before
    );
}

#[test]
fn listing_scope_flags_and_filter_command_framing_are_validated() {
    let (_directory, verbs, _, _) = fixture();
    for input in [
        LsInput {
            optional: true,
            ..LsInput::default()
        },
        LsInput {
            required: true,
            ..LsInput::default()
        },
        LsInput {
            under: Some("parent".into()),
            optional: true,
            required: true,
            ..LsInput::default()
        },
        LsInput {
            search: Some("bad\nnext: forged".into()),
            ..LsInput::default()
        },
    ] {
        assert!(verbs.ls(&input, at(0)).is_err());
    }
    let input = LsInput {
        search: Some("-name with 'quotes' and $symbols;".into()),
        blocked: true,
        all: true,
        mine: true,
        required: true,
        under: Some("parent".into()),
        verbose: true,
        limit: Some(5),
        ..LsInput::default()
    };
    let command = input.list_command();
    assert!(command.contains("--search='-name with "));
    assert!(command.contains("--blocked --mine --all --required --verbose --limit 5"));
    assert_eq!(command.lines().count(), 1);
}

#[test]
fn listing_cursor_refuses_deferral_and_handoff_time_boundaries() {
    for handoff in [false, true] {
        let (_directory, verbs, _, _) = fixture();
        let first = add(&verbs, "First", None, false, 0);
        add(&verbs, "Second", None, false, 1);
        if handoff {
            verbs
                .claim(
                    ClaimInput {
                        work_ref: first.clone(),
                        ttl_seconds: Some(100),
                        recover: None,
                    },
                    at(2),
                )
                .unwrap();
            verbs
                .handoff(
                    HandoffInput {
                        work_ref: Some(first),
                        action: HandoffAction::Offer {
                            to: "recipient".into(),
                            summary: Some("Context".into()),
                            ttl_seconds: Some(10),
                        },
                    },
                    at(3),
                )
                .unwrap();
        } else {
            verbs
                .update(
                    UpdateInput {
                        work_ref: Some(first),
                        action: UpdateAction::Revise {
                            external: None,
                            title: None,
                            outcome: None,
                            acceptance: None,
                            assignee: None,
                            priority: None,
                            defer: Some(at(13)),
                            kind: None,
                            labels: Vec::new(),
                            unlabels: Vec::new(),
                        },
                    },
                    at(3),
                )
                .unwrap();
        }
        let input = LsInput {
            limit: Some(1),
            ..LsInput::default()
        };
        let first_page = verbs.ls(&input, at(4)).unwrap();
        let input = LsInput {
            after: Some(first_page.value["after"].as_str().unwrap().into()),
            ..input
        };
        assert!(verbs.ls(&input, at(12)).is_ok());
        assert!(matches!(
            verbs.ls(&input, at(13)).unwrap_err().error,
            StoreError::WorkCatalogCursorInvalid { .. }
        ));
    }
}
