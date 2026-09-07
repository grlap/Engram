use super::*;

mod omissions;

fn rich_focus(
    participating: i64,
) -> (
    crate::test_support::TempHome,
    AgentVerbs,
    LocalWorkService,
    String,
) {
    let (directory, verbs, path, project) = fixture();
    let service = LocalWorkService::new(
        path,
        project,
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let root = add(&verbs, "Coordinator root", None, false, 0);
    for index in 0..12 {
        let child = verbs
            .add(
                AddInput {
                    title: format!("Child {index}: {}", "t".repeat(180)),
                    under: Some(root.clone()),
                    optional: true,
                    acceptance: (0..6)
                        .map(|criterion| format!("Criterion {criterion}: {}", "a".repeat(180)))
                        .collect(),
                    assignee: (index < 8).then(|| "agent".into()),
                    ..AddInput::default()
                },
                at(index * 2 + 1),
            )
            .expect("child with rich planning metadata");
        if index < participating {
            note(
                &verbs,
                child.value["work"]["short_ref"].as_str().unwrap(),
                &coordination_note(index),
                index * 2 + 2,
            );
        }
    }
    (directory, verbs, service, root)
}

fn coordination_note(index: i64) -> String {
    format!(
        "Requested design {index}: preserve the original execution authority and expose the follow-up as independent work with source context.\nWaiting condition: the coordinator must confirm the replacement is installed.\nNext action: inspect the review result and continue the focused task."
    )
}

#[test]
fn rich_focus_fixture_bindings_close_both_services_before_directory() {
    let path;
    {
        let (directory, verbs, service, root) = rich_focus(0);
        verbs.show(&root, at(100)).unwrap();
        service.work_focus_for_agent(&root, at(101)).unwrap();
        path = directory.path().to_owned();
    }
    assert!(!path.exists());
}

#[test]
fn next_does_not_shed_discovery_for_hidden_core_metadata() {
    let (_directory, verbs, _service, root) = rich_focus(8);
    verbs
        .service
        .select_work(&root, at(100))
        .expect("select root");
    verbs.show(&root, at(100)).expect("read root");
    let receipt = verbs.next(&NextInput::default(), at(101)).expect("next");
    assert_eq!(receipt.value["focus"]["ref"], root);
    assert!(receipt.text().len() < MAX_AGENT_WORK_RESPONSE_BYTES);
    assert!(
        serde_json::to_vec_pretty(&receipt.value).unwrap().len() < MAX_AGENT_WORK_RESPONSE_BYTES
    );
    for section in ["assigned", "participated"] {
        assert_eq!(
            receipt.value[section].as_array().map(Vec::len),
            Some(5),
            "{section}"
        );
        assert_eq!(receipt.value[format!("{section}_omitted")], 3);
        for row in receipt.value[section].as_array().unwrap() {
            assert!(
                receipt
                    .text()
                    .contains(&format!("  {} \"", row["ref"].as_str().unwrap()))
            );
        }
    }
    assert!(
        !receipt.value["omissions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["reason"] == "byte_budget"),
        "{}",
        receipt.value["omissions"]
    );
}

#[test]
fn compact_guidance_count_caps_are_not_byte_omissions() {
    let (_directory, _verbs, service, root) = rich_focus(0);
    service.select_work(&root, at(100)).unwrap();
    let view = service
        .work_next_for_agent(20, 20, false, WorkNextQuery::default(), at(101))
        .unwrap();
    let guidance = Guidance {
        reminders: (0..MAX_COMPACT_REMINDER_ITEMS + 2)
            .map(|index| format!("Reminder {index}"))
            .collect(),
        next: (0..MAX_TEXT_NEXT_COMMANDS + 2)
            .map(|index| format!("engram work memories note-{index}"))
            .collect(),
    };
    let compact = crate::verbs::receipts::compact_next_receipt(
        &view,
        &[],
        &[],
        &[],
        &std::collections::HashMap::new(),
        &guidance,
    )
    .unwrap();
    for section in ["reminders", "next"] {
        assert_eq!(
            compact_omitted_for_reason(&compact, section, WorkSectionOmissionReason::CountLimit),
            2
        );
        assert_eq!(
            compact_omitted_for_reason(&compact, section, WorkSectionOmissionReason::ByteBudget),
            0
        );
    }
}

#[test]
fn show_sheds_only_under_final_representation_pressure() {
    let (_directory, verbs, service, root) = rich_focus(8);
    let view = service.work_focus_for_agent(&root, at(100)).unwrap();
    let original = verbs.render_show(&view, at(100)).unwrap();
    let render = |view: &WorkFocusView| verbs.render_show(view, at(100));
    let original_size = original
        .text()
        .len()
        .max(serde_json::to_vec_pretty(&original.value).unwrap().len());
    // Strict fitting at the exact full-output size must shed something; one
    // extra byte must retain every projected row and the exact guidance.
    let retained =
        crate::verbs::show::fit_show_receipt(view.clone(), render, original_size + 1).unwrap();
    assert_eq!(retained.value, original.value);
    assert_eq!(retained.text(), original.text());
    let fitted = crate::verbs::show::fit_show_receipt(view, render, original_size).unwrap();
    assert!(fitted.text().len() < original_size);
    assert!(serde_json::to_vec_pretty(&fitted.value).unwrap().len() < original_size);
    assert_eq!(fitted.value["children"], original.value["children"]);
    assert_eq!(fitted.value["next"], original.value["next"]);
    let removed = original.value["history"]["items"].as_array().unwrap().len()
        - fitted.value["history"]["items"].as_array().unwrap().len();
    assert!(removed > 0);
    assert_eq!(
        fitted.value["history"]["omitted"].as_u64().unwrap(),
        original.value["history"]["omitted"].as_u64().unwrap() + removed as u64
    );
    assert_eq!(
        fitted.value["omissions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["reason"] == "byte_budget")
            .unwrap()["omitted_count"],
        removed
    );
}

#[test]
fn show_note_budget_counts_only_visible_rows() {
    let (_directory, verbs, path, project) = fixture();
    let service = LocalWorkService::new(
        path,
        project,
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let root = add(&verbs, "Note page", None, false, 0);
    for index in 1..=8 {
        note(&verbs, &root, &coordination_note(index), index);
    }
    let page = service
        .work_focus_for_agent(&root, at(9))
        .unwrap()
        .evidence_items;
    assert_eq!(page.len(), 8);
    note(&verbs, &root, &coordination_note(9), 10);
    let mut view = service.work_focus_for_agent(&root, at(11)).unwrap();
    // Exercise the supported full page plus independently selected latest
    // note shape. The final page has already replaced its last old row.
    view.evidence_items = page;
    assert!(
        !view
            .evidence_items
            .iter()
            .any(|entry| entry.evidence == view.latest_evidence_item.as_ref().unwrap().evidence)
    );
    view.history.items.clear();
    view.history.omitted = view.history.total;
    let original = verbs.render_show(&view, at(11)).unwrap();
    assert_eq!(original.value["notes"].as_array().unwrap().len(), 8);
    assert_eq!(original.value["notes_omitted"], 1);
    let budget = original
        .text()
        .len()
        .max(serde_json::to_vec_pretty(&original.value).unwrap().len());
    let fitted =
        crate::verbs::show::fit_show_receipt(view, |view| verbs.render_show(view, at(11)), budget)
            .unwrap();
    assert!(fitted.text().len() < budget);
    assert!(serde_json::to_vec_pretty(&fitted.value).unwrap().len() < budget);
    let rows = fitted.value["notes"].as_array().unwrap();
    assert_eq!(rows.len(), 7);
    assert_eq!(
        rows.last(),
        original.value["notes"].as_array().unwrap().last()
    );
    assert_eq!(fitted.value["notes_omitted"], 2);
    assert_eq!(
        fitted.value["omissions"]
            .as_array()
            .unwrap()
            .iter()
            .find(|entry| entry["reason"] == "byte_budget")
            .unwrap()["omitted_count"],
        1
    );
}

#[test]
fn core_focus_and_verbose_next_keep_the_rich_response_budget() {
    let (_directory, _verbs, service, root) = rich_focus(8);
    let core = service.work_focus(&root, at(100)).unwrap();
    assert!(serde_json::to_vec(&core).unwrap().len() <= MAX_AGENT_WORK_RESPONSE_BYTES);
    assert!(
        core.children.len() < 8,
        "rich metadata really requires core shedding"
    );
    let core_next = service
        .work_next(20, WorkNextQuery::default(), at(101))
        .unwrap();
    assert!(serde_json::to_vec(&core_next).unwrap().len() <= MAX_AGENT_WORK_RESPONSE_BYTES);
    let verbose = service
        .work_next_for_agent(20, 20, true, WorkNextQuery::default(), at(102))
        .unwrap();
    assert!(serde_json::to_vec(&verbose).unwrap().len() <= MAX_AGENT_WORK_RESPONSE_BYTES);
}

#[test]
fn show_does_not_shed_relations_for_hidden_core_metadata() {
    let (_directory, verbs, _service, root) = rich_focus(8);
    let receipt = verbs.show(&root, at(100)).expect("show");
    assert!(receipt.text().len() < MAX_AGENT_WORK_RESPONSE_BYTES);
    assert!(
        serde_json::to_vec_pretty(&receipt.value).unwrap().len() < MAX_AGENT_WORK_RESPONSE_BYTES
    );
    assert_eq!(receipt.value["children"].as_array().unwrap().len(), 8);
    assert_eq!(receipt.value["children_omitted"], 4);
    assert!(
        !receipt.value["omissions"]
            .as_array()
            .unwrap()
            .iter()
            .any(|entry| entry["reason"] == "byte_budget")
    );
}

#[test]
fn a_fourth_coordination_note_does_not_erase_resume_discovery() {
    let (_directory, verbs, service, root) = rich_focus(3);
    for expected in [3, 4] {
        let now = 100 + i64::try_from(expected - 3).unwrap() * 10;
        let view = service.work_focus_for_agent(&root, at(now)).unwrap();
        if expected == 4 {
            let fourth = view
                .children
                .iter()
                .find(|item| item.title.starts_with("Child 3:"))
                .unwrap();
            note(&verbs, &fourth.short_ref, &coordination_note(3), now + 1);
        }
        service.select_work(&root, at(now + 2)).unwrap();
        let shown = verbs.show(&root, at(now + 2)).unwrap();
        assert_eq!(shown.value["children"].as_array().unwrap().len(), 8);
        assert_eq!(shown.value["children_omitted"], 4);
        assert!(shown.text().len() < MAX_AGENT_WORK_RESPONSE_BYTES);
        assert!(
            serde_json::to_vec_pretty(&shown.value).unwrap().len() < MAX_AGENT_WORK_RESPONSE_BYTES
        );
        let next = verbs.next(&NextInput::default(), at(now + 3)).unwrap();
        assert_eq!(next.value["focus"]["ref"], root);
        assert_eq!(
            next.value["participated"].as_array().map(Vec::len),
            Some(expected)
        );
        assert!(next.value.get("participated_omitted").is_none());
        assert!(
            next.text()
                .contains(&format!("participated ({expected} shown):"))
        );
        for row in next.value["participated"].as_array().unwrap() {
            let primary = if let Some(target) = row["context_ref"].as_str() {
                let reference = row["ref"].as_str().unwrap();
                assert_eq!(target, format!("assigned {reference}"));
                assert!(row.get("note").is_none());
                next.value["assigned"]
                    .as_array()
                    .unwrap()
                    .iter()
                    .find(|assigned| assigned["ref"] == reference)
                    .unwrap()
            } else {
                row
            };
            let first_line = primary["note"].as_str().unwrap();
            let child = view
                .children
                .iter()
                .find(|child| row["ref"] == child.short_ref)
                .unwrap();
            let index: i64 = child
                .title
                .strip_prefix("Child ")
                .unwrap()
                .split_once(':')
                .unwrap()
                .0
                .parse()
                .unwrap();
            let expected_note = coordination_note(index);
            assert_eq!(first_line, expected_note.lines().next().unwrap());
            assert!(first_line.starts_with("Requested design"));
            assert!(!first_line.contains('\n'));
            assert_eq!(next.text().matches(first_line).count(), 1);
            assert_eq!(
                next.value
                    .to_string()
                    .matches(&serde_json::to_string(first_line).unwrap())
                    .count(),
                1
            );
        }
        assert!(next.text().len() < MAX_AGENT_WORK_RESPONSE_BYTES);
        assert!(
            serde_json::to_vec_pretty(&next.value).unwrap().len() < MAX_AGENT_WORK_RESPONSE_BYTES
        );
    }
}
