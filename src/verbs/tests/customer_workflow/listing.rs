use super::*;

mod basis;
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
            assert!(!rows.is_empty(), "{rows:?}");
            assert_eq!(receipt.value["total"], expected.len());
            assert_eq!(receipt.value["shown_before"], collected.len());
            assert_eq!(receipt.value["limit"], 1000);
            assert_eq!(receipt.value["byte_budget"], MAX_AGENT_WORK_RESPONSE_BYTES);
            assert!(receipt.text().contains("--limit 1000; byte budget 12288"));
            assert!(emitted_receipt_bytes(&receipt) < MAX_AGENT_WORK_RESPONSE_BYTES);
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
fn listing_cursor_rejects_changed_filters_malformed_tokens_and_membership_changes() {
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
    // A note moves the project feed but not this listing's membership or
    // order: the continuation still reads the remaining page.
    note(&verbs, &first, "Changes the project feed", 5);
    assert_eq!(
        verbs.ls(&continued, at(6)).unwrap().value["items"]
            .as_array()
            .unwrap()
            .len(),
        1
    );
    // A new item enters the listing: the continuation is refused.
    add(&verbs, "Third", None, false, 7);
    let connection = rusqlite::Connection::open(&path).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
    let error = verbs.ls(&continued, at(8)).unwrap_err();
    assert!(matches!(
        error.error,
        StoreError::WorkCatalogCursorInvalid { .. }
    ));
    assert_eq!(error.guidance().next, vec![original.list_command()]);
    // The reason is stated once: in the refusal, not again as a reminder.
    assert!(
        error.guidance().reminders.is_empty(),
        "{:?}",
        error.guidance()
    );
    assert_eq!(
        error.to_string().matches("catalog changed").count(),
        1,
        "{error}"
    );
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&connection).unwrap(),
        before
    );
    assert!(verbs.ls(&original, at(8)).is_ok());
}

#[test]
fn listing_cursor_refuses_a_time_transition_that_moves_membership_and_clock_reversal() {
    let (_directory, verbs, path, _) = fixture();
    let first = add(&verbs, "First", None, false, 0);
    add(&verbs, "Second", None, false, 1);
    add(&verbs, "Third", None, false, 2);
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
    // The claim's expiry at 12 changes no member of the unfiltered listing:
    // its continuation outlives it, and only a reversed clock refuses.
    let all = LsInput {
        limit: Some(1),
        ..LsInput::default()
    };
    let page = verbs.ls(&all, at(3)).unwrap();
    let all = LsInput {
        after: Some(page.value["after"].as_str().unwrap().into()),
        ..all
    };
    assert!(verbs.ls(&all, at(13)).is_ok());
    // In the ready listing the expiry makes the claimed item ready, so it
    // enters the listing and the continuation is refused.
    let ready = LsInput {
        ready: true,
        limit: Some(1),
        ..LsInput::default()
    };
    let page = verbs.ls(&ready, at(3)).unwrap();
    let ready = LsInput {
        after: Some(page.value["after"].as_str().unwrap().into()),
        ..ready
    };
    assert!(verbs.ls(&ready, at(11)).is_ok());
    let connection = rusqlite::Connection::open(&path).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
    for (input, now) in [(&all, 2), (&ready, 2), (&ready, 12), (&ready, 13)] {
        assert!(matches!(
            verbs.ls(input, at(now)).unwrap_err().error,
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
fn listing_cursor_refuses_a_deferral_ending_and_outlives_a_handoff_expiry() {
    for handoff in [false, true] {
        let (_directory, verbs, _, _) = fixture();
        let first = add(&verbs, "First", None, false, 0);
        add(&verbs, "Second", None, false, 1);
        add(&verbs, "Third", None, false, 2);
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
                            bindings: None,
                            clear_external: false,
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
        // The ready listing: a deferral ending at 13 makes the first item
        // ready, so it enters and the continuation is refused; a handoff
        // offer expiring at 13 leaves the claim live, so membership holds.
        let input = LsInput {
            ready: true,
            limit: Some(1),
            ..LsInput::default()
        };
        let first_page = verbs.ls(&input, at(4)).unwrap();
        let input = LsInput {
            after: Some(first_page.value["after"].as_str().unwrap().into()),
            ..input
        };
        assert!(verbs.ls(&input, at(12)).is_ok());
        let late = verbs.ls(&input, at(13));
        if handoff {
            assert!(late.is_ok(), "{:?}", late.err().map(|error| error.error));
        } else {
            assert!(matches!(
                late.unwrap_err().error,
                StoreError::WorkCatalogCursorInvalid { .. }
            ));
        }
    }
}

#[test]
fn listing_verbose_ready_text_keeps_full_why_and_holder_overlay() {
    let (_directory, verbs, _, _) = fixture();
    let held = add_ready(&verbs, "Held ready sibling", 3, 0);
    verbs
        .claim(
            ClaimInput {
                work_ref: held,
                ttl_seconds: Some(600),
                recover: None,
            },
            at(1),
        )
        .unwrap();
    add_ready(&verbs, "Unclaimed ready", 3, 2);
    let verbose_ready = verbs
        .ls(
            &LsInput {
                ready: true,
                verbose: true,
                ..LsInput::default()
            },
            at(3),
        )
        .unwrap();
    assert!(
        verbose_ready.text().contains(crate::PLAIN_READY_REASON),
        "verbose ls --ready text keeps the full why"
    );
    let compact_ready = verbs
        .ls(
            &LsInput {
                ready: true,
                ..LsInput::default()
            },
            at(3),
        )
        .unwrap();
    assert!(
        !compact_ready.text().contains(crate::PLAIN_READY_REASON),
        "compact ls --ready still omits the constant plain-ready sentence"
    );
    let verbose_all = verbs
        .ls(
            &LsInput {
                verbose: true,
                ..LsInput::default()
            },
            at(3),
        )
        .unwrap();
    assert!(
        verbose_all.text().contains("held by"),
        "verbose listing text keeps the claims overlay"
    );
    assert!(verbose_all.text().contains(crate::PLAIN_READY_REASON));
}

#[test]
fn listing_ready_cursor_rejects_old_work_id_only_tokens() {
    let (_directory, verbs, _path, _project) = fixture();
    for index in 0..3 {
        add_ready(&verbs, &format!("Ready {index}"), 3, index);
    }
    let page = verbs
        .ls(
            &LsInput {
                ready: true,
                limit: Some(1),
                ..LsInput::default()
            },
            at(10),
        )
        .unwrap();
    let token = page.value["after"].as_str().unwrap();
    let mut value = listing_token_value(token);
    value.as_object_mut().unwrap().remove("after_priority");
    let old = encode_listing_token(&value);
    let error = verbs
        .ls(
            &LsInput {
                ready: true,
                after: Some(old),
                limit: Some(1),
                ..LsInput::default()
            },
            at(10),
        )
        .unwrap_err();
    assert!(matches!(
        error.error,
        StoreError::WorkCatalogCursorInvalid { .. }
    ));
    assert!(error.guidance().next[0].contains("ls --ready"));
    assert!(!error.guidance().next[0].contains("--after"));
}

#[test]
fn listing_ready_cursor_rejects_wrong_after_priority_for_same_work_id() {
    let (_directory, verbs, _path, _project) = fixture();
    for index in 0..3 {
        add_ready(&verbs, &format!("Ready {index}"), 3, index);
    }
    let page = verbs
        .ls(
            &LsInput {
                ready: true,
                limit: Some(1),
                ..LsInput::default()
            },
            at(10),
        )
        .unwrap();
    let token = page.value["after"].as_str().unwrap();
    let mut value = listing_token_value(token);
    let live_priority = value["after_priority"].as_i64().unwrap();
    value["after_priority"] = Value::from(live_priority + 1);
    let tampered = encode_listing_token(&value);
    let error = verbs
        .ls(
            &LsInput {
                ready: true,
                after: Some(tampered),
                limit: Some(1),
                ..LsInput::default()
            },
            at(10),
        )
        .unwrap_err();
    assert!(matches!(
        error.error,
        StoreError::WorkCatalogCursorInvalid { .. }
    ));
    assert!(error.guidance().next[0].contains("ls --ready"));
    assert!(!error.guidance().next[0].contains("--after"));
    let two = verbs
        .ls(
            &LsInput {
                ready: true,
                limit: Some(2),
                ..LsInput::default()
            },
            at(10),
        )
        .unwrap();
    let live = verbs
        .ls(
            &LsInput {
                ready: true,
                after: Some(token.to_owned()),
                limit: Some(1),
                ..LsInput::default()
            },
            at(10),
        )
        .unwrap();
    assert_eq!(live.value["items"][0]["ref"], two.value["items"][1]["ref"]);
}

fn listing_token_value(token: &str) -> Value {
    crate::work_service::listing_cursor_json(token).expect("listing cursor JSON")
}

fn encode_listing_token(value: &Value) -> String {
    crate::work_service::encode_listing_cursor_json(value).expect("listing cursor token")
}

#[test]
fn listing_ordinary_cursor_omits_after_priority_and_rejects_an_unexpected_one() {
    let (_directory, verbs, _path, _project) = fixture();
    for index in 0..3 {
        verbs
            .add(
                AddInput {
                    title: format!("Row {index}"),
                    ..AddInput::default()
                },
                at(index),
            )
            .unwrap();
    }
    let page = verbs
        .ls(
            &LsInput {
                limit: Some(1),
                ..LsInput::default()
            },
            at(10),
        )
        .unwrap();
    let token = page.value["after"].as_str().unwrap();
    let value = listing_token_value(token);
    assert!(value.get("after_priority").is_none());
    let second = verbs
        .ls(
            &LsInput {
                after: Some(token.to_owned()),
                limit: Some(1),
                ..LsInput::default()
            },
            at(10),
        )
        .unwrap();
    let two = verbs
        .ls(
            &LsInput {
                limit: Some(2),
                ..LsInput::default()
            },
            at(10),
        )
        .unwrap();
    assert_eq!(
        second.value["items"][0]["ref"],
        two.value["items"][1]["ref"]
    );
    let mut unexpected = value;
    unexpected["after_priority"] = Value::from(3);
    let error = verbs
        .ls(
            &LsInput {
                after: Some(encode_listing_token(&unexpected)),
                limit: Some(1),
                ..LsInput::default()
            },
            at(10),
        )
        .unwrap_err();
    assert!(matches!(
        error.error,
        StoreError::WorkCatalogCursorInvalid { .. }
    ));
    assert!(!error.guidance().next[0].contains("--after"));
}

#[test]
fn listing_ready_byte_bound_last_visible_priority_differs_from_next() {
    let (_directory, verbs, path, project) = fixture();
    let mut created = Vec::new();
    for index in 0..3 {
        let row = verbs
            .add(
                AddInput {
                    title: format!("Older low {index} {}", "x".repeat(1600)),
                    priority: Some(3),
                    external: Some("x".repeat(1000)),
                    ..AddInput::default()
                },
                at(index),
            )
            .unwrap();
        created.push((
            3,
            row.value["work"]["short_ref"].as_str().unwrap().to_owned(),
        ));
    }
    created.push((0, add_ready(&verbs, "Later high", 0, 10)));
    let expected = ready_rank_tuples(&path, &project, &created);
    let expected_refs: Vec<String> = expected.iter().map(|row| row.2.clone()).collect();
    let input = LsInput {
        ready: true,
        verbose: true,
        limit: Some(20),
        ..LsInput::default()
    };
    let mut bound = None;
    for budget in (1600..=11000).rev().step_by(200) {
        let page = match verbs.ls_with_budget(&input, at(20), budget) {
            Ok(page) => page,
            Err(error) => {
                assert!(
                    matches!(
                        &error.error,
                        StoreError::WorkCatalogCursorInvalid { reason }
                            if reason.contains("continuation metadata")
                    ),
                    "unexpected listing error at budget {budget}: {error:?}"
                );
                continue;
            }
        };
        let rows = page.value["items"].as_array().unwrap();
        if rows.is_empty() {
            continue;
        }
        let Some(next) = expected.get(rows.len()) else {
            continue;
        };
        let last_priority = rows
            .last()
            .and_then(|row| row.pointer("/work/priority"))
            .and_then(Value::as_i64)
            .expect("verbose ready row has work.priority");
        if page.value["more"] == true
            && page
                .value
                .get("hint")
                .and_then(Value::as_str)
                .is_some_and(|hint| hint.contains("byte-bounded"))
            && last_priority != i64::from(next.0)
        {
            bound = Some(page);
            break;
        }
    }
    let page = bound.expect("a byte-bounded mixed-priority cut");
    assert!(
        page.value["hint"]
            .as_str()
            .unwrap()
            .contains("byte-bounded")
    );
    let row_ref = |row: &Value| {
        row.get("ref")
            .or_else(|| row.pointer("/work/short_ref"))
            .and_then(Value::as_str)
            .unwrap()
            .to_owned()
    };
    let mut collected: Vec<String> = page.value["items"]
        .as_array()
        .unwrap()
        .iter()
        .map(row_ref)
        .collect();
    let mut after = page.value["after"].as_str().map(str::to_owned);
    while let Some(token) = after {
        let next = verbs
            .ls(
                &LsInput {
                    ready: true,
                    verbose: true,
                    after: Some(token),
                    limit: Some(20),
                    ..LsInput::default()
                },
                at(21),
            )
            .unwrap();
        collected.extend(next.value["items"].as_array().unwrap().iter().map(row_ref));
        after = next.value["after"].as_str().map(str::to_owned);
    }
    assert_eq!(collected, expected_refs);
}

/// Revises one item's labels or priority, unclaimed.
fn revise_listing_keys(
    verbs: &AgentVerbs,
    work: &str,
    labels: &[&str],
    unlabels: &[&str],
    priority: Option<i32>,
    now: i64,
) {
    verbs
        .update(
            UpdateInput {
                work_ref: Some(work.into()),
                action: UpdateAction::Revise {
                    bindings: None,
                    clear_external: false,
                    external: None,
                    title: None,
                    outcome: None,
                    acceptance: None,
                    assignee: None,
                    priority,
                    defer: None,
                    kind: None,
                    labels: labels.iter().map(|label| (*label).to_owned()).collect(),
                    unlabels: unlabels.iter().map(|label| (*label).to_owned()).collect(),
                },
            },
            at(now),
        )
        .expect("revise listing keys");
}

// A continuation is refused exactly when the listing's membership or order
// changes. A note on an item outside the filtered set leaves the remaining
// page as it was; one item leaving while another enters, with the count
// unchanged, refuses; and under ready order a priority moving across the
// page boundary refuses.
#[test]
fn listing_cursor_follows_membership_and_order_not_unrelated_writes() {
    let (_directory, verbs, _, _) = fixture();
    let labelled = ["A", "B", "C"]
        .iter()
        .enumerate()
        .map(|(index, title)| {
            let work = add(&verbs, title, None, false, i64::try_from(index).unwrap());
            revise_listing_keys(&verbs, &work, &["scope"], &[], None, 10);
            work
        })
        .collect::<Vec<_>>();
    let outside = add(&verbs, "Outside", None, false, 11);
    let first = LsInput {
        label: Some("scope".into()),
        limit: Some(1),
        ..LsInput::default()
    };
    let page = verbs.ls(&first, at(12)).unwrap();
    let continued = LsInput {
        after: Some(page.value["after"].as_str().unwrap().into()),
        ..first.clone()
    };
    note(&verbs, &outside, "An unrelated write", 13);
    let next = verbs
        .ls(&continued, at(14))
        .expect("an unrelated note keeps the page");
    assert_eq!(next.value["total"], 3);
    // One labelled item leaves and the outside item enters: still three.
    revise_listing_keys(&verbs, &labelled[2], &[], &["scope"], None, 15);
    revise_listing_keys(&verbs, &outside, &["scope"], &[], None, 16);
    assert_eq!(verbs.ls(&first, at(17)).unwrap().value["total"], 3);
    assert!(matches!(
        verbs.ls(&continued, at(17)).unwrap_err().error,
        StoreError::WorkCatalogCursorInvalid { .. }
    ));

    // Ready order is (priority, id): moving a later item ahead of the
    // boundary changes the order the remaining page continues.
    let (_directory, verbs, _, _) = fixture();
    add_ready(&verbs, "Urgent", 1, 0);
    let later = add_ready(&verbs, "Later", 3, 1);
    add_ready(&verbs, "Last", 3, 2);
    let ready = LsInput {
        ready: true,
        limit: Some(1),
        ..LsInput::default()
    };
    let page = verbs.ls(&ready, at(3)).unwrap();
    let continued = LsInput {
        after: Some(page.value["after"].as_str().unwrap().into()),
        ..ready
    };
    assert!(verbs.ls(&continued, at(4)).is_ok());
    revise_listing_keys(&verbs, &later, &[], &[], Some(0), 5);
    assert!(matches!(
        verbs.ls(&continued, at(6)).unwrap_err().error,
        StoreError::WorkCatalogCursorInvalid { .. }
    ));
}

// The ready filter selects open work only, so the all filter adds nothing to
// it: all with ready lists exactly the ready set, in readiness order, with no
// completed or cancelled item. The blocked filter selects an active blocker
// or an unmet prerequisite, so all with blocked adds an ended item that still
// carries an active blocker, and only that.
#[test]
fn all_adds_nothing_to_ready_and_only_blocked_ended_items_to_blocked() {
    let (_directory, verbs, _, _) = fixture();
    let low = add_ready(&verbs, "Low priority ready", 3, 0);
    let high = add_ready(&verbs, "High priority ready", 1, 1);
    let blocked = add(&verbs, "Blocked open", None, false, 2);
    let blocked_then_cancelled = add(&verbs, "Blocked then cancelled", None, false, 3);
    let completed = add(&verbs, "Completed", None, false, 4);
    for work in [&blocked, &blocked_then_cancelled] {
        verbs
            .update(
                UpdateInput {
                    work_ref: Some(work.clone()),
                    action: UpdateAction::Blocked {
                        detail: "waiting".into(),
                    },
                },
                at(5),
            )
            .unwrap();
    }
    super::terminalize(&verbs, &blocked_then_cancelled, WorkLifecycle::Cancelled);
    super::terminalize(&verbs, &completed, WorkLifecycle::Completed);
    let refs = |input: LsInput| {
        verbs.ls(&input, at(10)).unwrap().value["items"]
            .as_array()
            .unwrap()
            .iter()
            .map(|row| row["ref"].as_str().unwrap().to_owned())
            .collect::<Vec<_>>()
    };
    let ready = refs(LsInput {
        ready: true,
        ..LsInput::default()
    });
    assert_eq!(ready, vec![high.clone(), low.clone()], "readiness order");
    assert_eq!(
        refs(LsInput {
            ready: true,
            all: true,
            ..LsInput::default()
        }),
        ready
    );
    assert_eq!(
        refs(LsInput {
            blocked: true,
            ..LsInput::default()
        }),
        vec![blocked.clone()]
    );
    let mut all_blocked = refs(LsInput {
        blocked: true,
        all: true,
        ..LsInput::default()
    });
    all_blocked.sort();
    let mut expected = vec![blocked.clone(), blocked_then_cancelled.clone()];
    expected.sort();
    assert_eq!(
        all_blocked, expected,
        "only ended items with an active blocker join"
    );
    // The all filter alone includes every ended item.
    let everything = refs(LsInput {
        all: true,
        ..LsInput::default()
    });
    for terminal in [&blocked_then_cancelled, &completed] {
        assert!(everything.contains(terminal), "{terminal}: {everything:?}");
    }
}
