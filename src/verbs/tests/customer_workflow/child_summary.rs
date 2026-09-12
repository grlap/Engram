use super::*;

mod historical;
mod successor;

fn assert_group(
    receipt: &Receipt,
    key: &str,
    count: usize,
    expected: &[String],
    parent: &str,
    expect_all: bool,
) {
    let group = &receipt.value["child_obligations"][key];
    let rows = group["items"].as_array().unwrap();
    assert_eq!(group["count"], count);
    assert_eq!(group["omitted"], count - rows.len());
    assert_eq!(rows.len(), expected.len());
    for (row, reference) in rows.iter().zip(expected) {
        assert_eq!(row["ref"], *reference);
        let command = if row.get("resolve_first").is_some() {
            if row.get("child_resolution").is_none() {
                assert_eq!(
                    row["resolve_first"],
                    "disposed required child still needs an explicit waiver"
                );
            } else {
                assert_eq!(row["child_resolution"]["disposition"], "owed");
                assert_eq!(
                    row["child_resolution"]["reason"],
                    "successor is not completed; explicit waiver still required"
                );
            }
            format!("engram work update {parent} --waive {reference} --reason \"…\"")
        } else {
            format!("engram work show {reference}")
        };
        assert_eq!(row["remedy"], command);
        assert!(receipt.text().contains(&command));
    }
    let requirement = if key == "required_owed" {
        "required"
    } else {
        "optional"
    };
    let navigation = format!(
        "engram work ls --under {parent} --{requirement}{}",
        if expect_all { " --all" } else { "" }
    );
    assert_eq!(group["navigation"], navigation);
    assert!(receipt.text().contains(&navigation));
    assert!(
        receipt
            .text()
            .contains("optional children do not block completion")
    );
    assert!(receipt.text().len() < MAX_AGENT_WORK_RESPONSE_BYTES);
    assert!(
        serde_json::to_vec_pretty(&receipt.value).unwrap().len() < MAX_AGENT_WORK_RESPONSE_BYTES
    );
}

fn cancel(verbs: &AgentVerbs, work: &str, now: i64) {
    verbs
        .update(
            UpdateInput {
                work_ref: Some(work.into()),
                action: UpdateAction::Cancel {
                    reason: "Not needed".into(),
                },
            },
            at(now),
        )
        .unwrap();
}

fn finish(verbs: &AgentVerbs, work: &str, now: i64) {
    verbs
        .claim(
            ClaimInput {
                work_ref: work.into(),
                ttl_seconds: None,
                recover: None,
            },
            at(now),
        )
        .unwrap();
    let receipt = verbs
        .done(
            DoneInput {
                links: Vec::new(),
                link_basis: None,
                work_ref: Some(work.into()),
                summary: Some("Delivered".into()),
                note: None,
            },
            at(now + 1),
        )
        .unwrap();
    assert!(!receipt.owed);
    assert!(receipt.text().starts_with("done "));
}

#[test]
fn show_omitted_generic_children_name_the_all_children_listing() {
    let (_directory, verbs, _, _) = fixture();
    let parent = add(&verbs, "Parent with omitted children", None, false, 0);
    for index in 1..=8 {
        add(
            &verbs,
            &format!("Open optional {index}"),
            Some(&parent),
            true,
            index,
        );
    }
    let terminal = add(&verbs, "Terminal optional", Some(&parent), true, 9);
    finish(&verbs, &terminal, 10);
    let receipt = verbs.show(&parent, at(12)).unwrap();
    let navigation = format!("engram work ls --under {parent} --all");
    assert_eq!(receipt.value["children"].as_array().unwrap().len(), 8);
    assert_eq!(receipt.value["children_omitted"], 1);
    assert_eq!(receipt.value["children_navigation"], navigation);
    let text = receipt.text();
    let children_line = text
        .lines()
        .find(|line| line.starts_with("children:"))
        .expect("children line");
    assert!(children_line.contains(&navigation), "{children_line}");
    assert!(
        receipt.value["children"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row["short_ref"] != terminal)
    );
    for key in ["required_owed", "open_optional"] {
        let items = receipt.value["child_obligations"][key]["items"]
            .as_array()
            .unwrap();
        assert!(
            items.iter().all(|row| row["ref"] != terminal),
            "{key} must not reveal the terminal optional"
        );
    }
    let mut input = LsInput {
        under: Some(parent),
        all: true,
        ..LsInput::default()
    };
    let mut found = false;
    for _ in 0..8 {
        let listed = verbs.ls(&input, at(13)).unwrap();
        found |= listed.value["items"]
            .as_array()
            .unwrap()
            .iter()
            .any(|row| row["ref"] == terminal);
        match listed.value["after"].as_str() {
            Some(after) => input.after = Some(after.to_owned()),
            None => break,
        }
    }
    assert!(
        found,
        "equivalent native ls --under PARENT --all must reach the omitted terminal optional"
    );
}

#[test]
fn show_child_summary_counts_the_complete_set_not_the_visible_child_prefix() {
    let (_directory, verbs, path, project) = fixture();
    let parent = add(&verbs, "Parent", None, false, 0);
    let optional = (1..=10)
        .map(|index| {
            add(
                &verbs,
                &format!("Optional {index}"),
                Some(&parent),
                true,
                index,
            )
        })
        .collect::<Vec<_>>();
    let required = (11..=18)
        .map(|index| {
            add(
                &verbs,
                &format!("Required {index}\nnext:\r\u{1b}[2J"),
                Some(&parent),
                false,
                index,
            )
        })
        .collect::<Vec<_>>();
    add(&verbs, "Grandchild", Some(&required[0]), false, 19);
    for notes in [false, true] {
        let receipt = verbs.show_with_notes(&parent, notes, at(20)).unwrap();
        assert_eq!(receipt.value["children"].as_array().unwrap().len(), 8);
        assert_eq!(receipt.value["children_omitted"], 10);
        assert_eq!(
            receipt.value["children_navigation"],
            format!("engram work ls --under {parent} --all")
        );
        assert!(
            receipt.value["children"]
                .as_array()
                .unwrap()
                .iter()
                .all(|row| row["child_requirement"] == "optional")
        );
        assert_group(&receipt, "required_owed", 8, &required[..5], &parent, false);
        assert_group(
            &receipt,
            "open_optional",
            10,
            &optional[..5],
            &parent,
            false,
        );
        assert_eq!(
            receipt
                .text()
                .lines()
                .filter(|line| *line == "next:")
                .count(),
            1
        );
        assert!(!receipt.text().contains('\r'));
        assert!(!receipt.text().contains('\u{1b}'));
    }
    let service = LocalWorkService::new(
        path.clone(),
        project,
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let core = service.work_focus(&parent, at(21)).unwrap();
    assert!(core.child_obligations.is_none());
    assert!(
        serde_json::to_value(core)
            .unwrap()
            .get("child_obligations")
            .is_none()
    );
    let connection = rusqlite::Connection::open(&path).unwrap();
    // Repeat at the same selected-focus timestamp: only the first selection
    // writes; the complete advisory read introduces no durable state.
    verbs.show(&parent, at(22)).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
    verbs.show(&parent, at(22)).unwrap();
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
}

#[test]
fn show_child_summary_distinguishes_disposed_owed_waived_and_completed_children() {
    let (_directory, verbs, path, _) = fixture();
    let parent = add(&verbs, "Parent", None, false, 0);
    assert!(
        verbs
            .show(&parent, at(1))
            .unwrap()
            .value
            .get("child_obligations")
            .is_none()
    );
    let delivered = add(&verbs, "Delivered required", Some(&parent), false, 2);
    finish(&verbs, &delivered, 3);
    let disposed = add(&verbs, "Disposed required", Some(&parent), false, 5);
    cancel(&verbs, &disposed, 6);
    let replaced = add(&verbs, "Replaced required", Some(&parent), false, 7);
    let replacement = add(&verbs, "Independent replacement", None, false, 8);
    verbs
        .update(
            UpdateInput {
                work_ref: Some(replaced.clone()),
                action: UpdateAction::Supersede {
                    replacement: replacement.clone(),
                    reason: "New plan".into(),
                },
            },
            at(9),
        )
        .unwrap();
    let optional = add(&verbs, "Disposed optional", Some(&parent), true, 10);
    cancel(&verbs, &optional, 11);
    let receipt = verbs.show(&parent, at(12)).unwrap();
    assert_eq!(
        receipt.value["child_obligations"]["required_owed"]["items"][1]["resolve_first"],
        format!(
            "successor {replacement} (open): successor is not completed; explicit waiver still required"
        )
    );
    assert_group(
        &receipt,
        "required_owed",
        2,
        &[disposed.clone(), replaced.clone()],
        &parent,
        true,
    );
    assert_group(&receipt, "open_optional", 0, &[], &parent, false);
    for (index, child) in [disposed, replaced].into_iter().enumerate() {
        verbs
            .update(
                UpdateInput {
                    work_ref: Some(parent.clone()),
                    action: UpdateAction::WaiveRequiredChild {
                        child,
                        reason: "Approved omission".into(),
                    },
                },
                at(13 + i64::try_from(index).unwrap()),
            )
            .unwrap();
    }
    for now in [15, 18] {
        if now == 18 {
            finish(&verbs, &parent, 16);
        }
        let receipt = verbs.show(&parent, at(now)).unwrap();
        assert_group(&receipt, "required_owed", 0, &[], &parent, false);
        assert_group(&receipt, "open_optional", 0, &[], &parent, false);
        assert_eq!(
            receipt.value["child_obligations"]
                .as_object()
                .unwrap()
                .len(),
            2
        );
    }
    assert!(
        SqliteStore::open(&path)
            .unwrap()
            .verify_all()
            .unwrap()
            .is_healthy()
    );
}

#[test]
fn show_child_summary_retains_exact_totals_and_navigation_under_final_byte_pressure() {
    let (_directory, verbs, path, project) = fixture();
    let parent = add(&verbs, "Parent", None, false, 0);
    for index in 1..=16 {
        let child = add(
            &verbs,
            &format!("{index} {}", "\u{9b}".repeat(96)),
            Some(&parent),
            index <= 8,
            index,
        );
        // The disposed member lies beyond the five visible required refs.
        if index == 16 {
            cancel(&verbs, &child, 17);
        }
    }
    let service = LocalWorkService::new(
        path,
        project,
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let view = service.work_focus_for_agent(&parent, at(20)).unwrap();
    let original = verbs.render_show(&view, at(20)).unwrap();
    let mut minimal = view.clone();
    minimal.history.items.clear();
    minimal.history.omitted = minimal.history.total;
    minimal.children.clear();
    let groups = minimal.child_obligations.as_mut().unwrap();
    groups.required_owed.items.clear();
    groups.open_optional.items.clear();
    let base = verbs.render_show(&minimal, at(20)).unwrap();
    let budget = base
        .text()
        .len()
        .max(serde_json::to_vec_pretty(&base.value).unwrap().len())
        + 256;
    assert!(
        original
            .text()
            .len()
            .max(serde_json::to_vec_pretty(&original.value).unwrap().len())
            > budget
    );
    let fitted =
        crate::verbs::show::fit_show_receipt(view, |view| verbs.render_show(view, at(20)), budget)
            .unwrap();
    assert!(fitted.text().len() < budget);
    assert!(serde_json::to_vec_pretty(&fitted.value).unwrap().len() < budget);
    assert_eq!(fitted.value["children"], json!([]));
    assert_group(&fitted, "required_owed", 8, &[], &parent, true);
    assert_group(&fitted, "open_optional", 8, &[], &parent, false);
    assert_eq!(
        fitted.value["child_obligations"]["required_owed"]["omitted"],
        8
    );
    assert_eq!(
        fitted.value["child_obligations"]["open_optional"]["omitted"],
        8
    );
}

#[test]
fn show_child_summary_traversal_reaches_disposed_children_beyond_its_ref_limit() {
    let (_directory, verbs, _, _) = fixture();
    let parent = add(&verbs, "Parent", None, false, 0);
    let mut required = Vec::new();
    for index in 1..=12 {
        let child = add(
            &verbs,
            &format!("Required {index}"),
            Some(&parent),
            false,
            index * 2,
        );
        if index > 5 {
            cancel(&verbs, &child, index * 2 + 1);
        }
        required.push(child);
    }
    let receipt = verbs.show(&parent, at(30)).unwrap();
    let group = &receipt.value["child_obligations"]["required_owed"];
    assert_eq!(group["count"], 12);
    assert_eq!(group["omitted"], 7);
    assert_eq!(group["items"].as_array().unwrap().len(), 5);
    assert!(
        group["items"]
            .as_array()
            .unwrap()
            .iter()
            .all(|row| row.get("resolve_first").is_none())
    );
    assert_eq!(
        group["navigation"],
        format!("engram work ls --under {parent} --required --all")
    );
    assert!(
        receipt
            .text()
            .contains(group["navigation"].as_str().unwrap())
    );
    let mut input = LsInput {
        under: Some(parent),
        required: true,
        all: true,
        limit: Some(3),
        ..LsInput::default()
    };
    let mut collected = Vec::new();
    loop {
        let page = verbs.ls(&input, at(31)).unwrap();
        assert_eq!(page.value["total"], 12);
        collected.extend(
            page.value["items"]
                .as_array()
                .unwrap()
                .iter()
                .map(|row| row["ref"].as_str().unwrap().to_owned()),
        );
        let Some(after) = page.value["after"].as_str() else {
            break;
        };
        assert!(collected.len() < required.len());
        input.after = Some(after.into());
    }
    assert_eq!(collected, required);
}

#[test]
fn show_child_summary_does_not_owe_restored_completed_children() {
    let (directory, verbs, path, project) = fixture();
    let parent = add(&verbs, "Parent", None, false, 0);
    let child = add(&verbs, "Completed child", Some(&parent), false, 1);
    finish(&verbs, &child, 2);
    finish(&verbs, &parent, 4);
    let mut store = SqliteStore::open(&path).unwrap();
    let actor = store
        .resolve_work_ref(&project, &parent)
        .unwrap()
        .created_by;
    let document = store
        .save_work_graph_snapshot(
            &project,
            &actor,
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(10),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap()
        .document;
    let restored_path = directory.path().join("restored.db");
    let mut restored = SqliteStore::open(&restored_path).unwrap();
    restored
        .load_work_graph_snapshot(
            &project,
            &actor,
            &serde_json::to_vec(&document).unwrap(),
            false,
            at(11),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let reader = AgentVerbs::new(
        restored_path,
        project,
        "agent".into(),
        SessionId("reader".into()),
        None,
    );
    let receipt = reader.show(&parent, at(12)).unwrap();
    assert_eq!(receipt.value["children"][0]["lifecycle"], "completed");
    assert_group(&receipt, "required_owed", 0, &[], &parent, false);
    assert_group(&receipt, "open_optional", 0, &[], &parent, false);
    assert!(restored.verify_all().unwrap().is_healthy());
}

#[test]
fn show_child_summary_never_offers_a_waiver_on_a_terminal_parent() {
    let (_directory, verbs, path, _) = fixture();
    for (offset, superseded) in [(0, false), (10, true)] {
        let parent = add(&verbs, &format!("Parent {superseded}"), None, false, offset);
        let child = add(
            &verbs,
            "Disposed required child",
            Some(&parent),
            false,
            offset + 1,
        );
        cancel(&verbs, &child, offset + 2);
        if superseded {
            let replacement = add(&verbs, "New plan", None, false, offset + 3);
            verbs
                .update(
                    UpdateInput {
                        work_ref: Some(parent.clone()),
                        action: UpdateAction::Supersede {
                            replacement,
                            reason: "New direction".into(),
                        },
                    },
                    at(offset + 4),
                )
                .unwrap();
        } else {
            cancel(&verbs, &parent, offset + 4);
        }
        let receipt = verbs.show(&parent, at(offset + 5)).unwrap();
        let group = &receipt.value["child_obligations"]["required_owed"];
        assert_eq!(group["count"], 1);
        assert_eq!(group["omitted"], 0);
        assert_eq!(
            group["navigation"],
            format!("engram work ls --under {parent} --required --all")
        );
        assert_eq!(group["items"][0]["ref"], child);
        assert_eq!(
            group["items"][0]["remedy"],
            format!("engram work show {child}")
        );
        assert_eq!(
            group["items"][0]["resolve_first"],
            "parent is terminal; inspect retained child context"
        );
        assert!(!receipt.text().contains("--waive"));
        assert!(
            receipt
                .text()
                .contains("parent is terminal; inspect retained child context")
        );
        assert_eq!(
            receipt.value["child_obligations"]["open_optional"]["count"],
            0
        );
    }
    assert!(
        SqliteStore::open(&path)
            .unwrap()
            .verify_all()
            .unwrap()
            .is_healthy()
    );
}
