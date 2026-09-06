use super::*;

fn assert_parent(receipt: &Receipt, parent: &str, requirement: &str) {
    assert_eq!(receipt.value["parent_ref"], parent);
    assert_eq!(receipt.value["parent_title"], "Parent");
    assert_eq!(receipt.value["parent_lifecycle"], "open");
    assert_eq!(
        receipt.value["status"]["work"]["child_requirement"],
        requirement
    );
    assert!(
        receipt
            .text()
            .lines()
            .any(|line| line == format!("parent: {parent} \"Parent\" (open), {requirement}"))
    );
    let command = format!("engram work show {parent}");
    assert!(receipt.next.contains(&command));
    assert!(receipt.text().contains(&format!("  {command}\n")));
}

#[test]
fn show_parent_context_names_required_optional_and_root_relationships() {
    let (_directory, verbs, _, _) = fixture();
    let parent = add(&verbs, "Parent", None, false, 0);
    for optional in [false, true] {
        let child = add(
            &verbs,
            &format!("Child {optional}"),
            Some(&parent),
            optional,
            1,
        );
        assert_parent(
            &verbs.show(&child, at(2)).unwrap(),
            &parent,
            if optional { "optional" } else { "required" },
        );
    }
    let root = verbs.show(&parent, at(3)).unwrap();
    assert!(root.text().lines().any(|line| line == "parent: root"));
    assert!(root.value.get("parent_ref").is_none());
    assert!(
        root.value["status"]["work"]
            .get("child_requirement")
            .is_none()
    );
    assert!(root.value.get("parent_lifecycle").is_none());
}

#[test]
fn show_parent_context_survives_acceptance_and_note_trimming() {
    let (_directory, verbs, path, project) = fixture();
    let parent = add(&verbs, "Parent", None, false, 0);
    let child = verbs
        .add(
            AddInput {
                title: "Child".into(),
                under: Some(parent.clone()),
                acceptance: (0..6)
                    .map(|i| format!("Criterion {i}: {}", "x".repeat(500)))
                    .collect(),
                notes: vec!["Initial note".into()],
                ..AddInput::default()
            },
            at(1),
        )
        .unwrap()
        .value["work"]["short_ref"]
        .as_str()
        .unwrap()
        .to_owned();
    let service = LocalWorkService::new(
        path,
        project,
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let source = service.work_focus_for_agent(&child, at(2)).unwrap();
    let mut minimal = source.clone();
    minimal.status.work.acceptance.clear();
    minimal.history.items.clear();
    minimal.history.omitted = minimal.history.total;
    minimal.evidence_items.clear();
    minimal.latest_evidence_item = None;
    let small = verbs.render_show(&minimal, at(2)).unwrap();
    let budget = small
        .text()
        .len()
        .max(serde_json::to_vec_pretty(&small.value).unwrap().len())
        + 600;
    let fitted = crate::verbs::show::fit_show_receipt(
        source.clone(),
        |view| verbs.render_show(view, at(2)),
        budget,
    )
    .unwrap();
    assert_parent(&fitted, &parent, "required");
    assert!(
        fitted.value["status"]["work"]["acceptance_omitted"]
            .as_u64()
            .unwrap()
            > 0
    );
    assert!(fitted.value["notes"].as_array().unwrap().is_empty());
    assert_eq!(fitted.value["notes_omitted"], 1);
    assert!(fitted.text().len() < budget);
    assert!(serde_json::to_vec_pretty(&fitted.value).unwrap().len() < budget);
    assert!(
        serde_json::to_value(&source)
            .unwrap()
            .get("parent")
            .is_none(),
        "private carrier must not change core wire"
    );
}

#[test]
fn show_parent_context_uses_direct_parent_state_and_child_requirement() {
    let (_directory, verbs, _, _) = fixture();
    let root = add(&verbs, "Grandparent", None, false, 0);
    let parent = add(&verbs, "Parent", Some(&root), true, 1);
    let child = add(&verbs, "Required grandchild", Some(&parent), false, 2);
    assert_parent(&verbs.show(&child, at(3)).unwrap(), &parent, "required");
    verbs
        .update(
            UpdateInput {
                work_ref: Some(child.clone()),
                action: UpdateAction::Cancel {
                    reason: "Child stopped".into(),
                },
            },
            at(4),
        )
        .unwrap();
    verbs
        .update(
            UpdateInput {
                work_ref: Some(parent.clone()),
                action: UpdateAction::Cancel {
                    reason: "Parent stopped".into(),
                },
            },
            at(4),
        )
        .unwrap();
    let shown = verbs.show(&child, at(5)).unwrap();
    assert_eq!(shown.value["parent_ref"], parent);
    assert_eq!(shown.value["parent_lifecycle"], "cancelled");
    assert_eq!(
        shown.value["status"]["work"]["child_requirement"],
        "required"
    );
    assert!(shown.text().contains(&format!(
        "parent: {parent} \"Parent\" (cancelled), required"
    )));
}

#[test]
fn show_parent_context_frames_parent_title_without_rewriting_json() {
    let (_directory, verbs, _, _) = fixture();
    let title = "Parent\u{1b}[2J\u{202e}\r\nnext:";
    let parent = add(&verbs, title, None, false, 0);
    let child = add(&verbs, "Child", Some(&parent), false, 1);
    let shown = verbs.show(&child, at(2)).unwrap();
    assert_eq!(shown.value["parent_title"], title);
    let text = shown.text();
    let line = text
        .split('\n')
        .find(|line| line.starts_with("parent:"))
        .unwrap();
    assert!(
        !line
            .chars()
            .any(crate::domain::is_unsafe_rendered_text_char)
    );
    assert!(line.contains("\\u{1b}"));
    assert_eq!(text.split('\n').filter(|line| *line == "next:").count(), 1);
}

#[test]
fn show_parent_context_keeps_detach_first_and_parent_navigation_visible() {
    let (_directory, verbs, _, _) = fixture();
    let (parent, child) = super::detach::stranded_child(&verbs);
    let shown = verbs.show(&child, at(5)).unwrap();
    let detach = format!("engram work update {child} --detach \"Continue as independent work\"");
    let parent_command = format!("engram work show {parent}");
    assert_eq!(shown.next[0], detach);
    assert!(shown.next.contains(&parent_command));
    assert!(shown.text().contains(&format!("  {parent_command}\n")));
    assert_eq!(shown.value["parent_lifecycle"], "completed");
}

#[test]
fn show_parent_correction_paginated_first_page_keeps_parent_and_recovery_visible() {
    let (_directory, verbs, _, _) = fixture();
    let parent = add(&verbs, "Parent", None, false, 0);
    let child = add(&verbs, "Child", Some(&parent), false, 1);
    let prerequisite = add(&verbs, "Prerequisite", None, false, 2);
    for (work_ref, action, now) in [
        (
            child.clone(),
            UpdateAction::After {
                prerequisite: prerequisite.clone(),
            },
            3,
        ),
        (
            prerequisite.clone(),
            UpdateAction::Cancel {
                reason: "Prerequisite unavailable".into(),
            },
            4,
        ),
        (
            child.clone(),
            UpdateAction::Blocked {
                detail: "Independent blocker".into(),
            },
            5,
        ),
    ] {
        verbs
            .update(
                UpdateInput {
                    work_ref: Some(work_ref),
                    action,
                },
                at(now),
            )
            .unwrap();
    }
    for index in 0..4 {
        note(
            &verbs,
            &child,
            &format!("Note {index}: {}", "body ".repeat(800)),
            6 + index,
        );
    }
    let recovery = format!("engram work update {child} --drop-after {prerequisite}");
    let plain = verbs.show(&child, at(20)).unwrap();
    assert_eq!(
        plain.next[..3],
        [
            recovery.clone(),
            format!("engram work note {child} \"…\""),
            format!("engram work update {child} --unblock"),
        ],
        "fixture must exercise all three lifecycle suggestions"
    );
    let first = verbs.show_with_notes(&child, true, at(20)).unwrap();
    let after = first.value["notes_window"]["after"]
        .as_str()
        .expect("older notes continuation");
    assert!(first.value["notes_window"]["older"].as_u64().unwrap() > 0);
    assert_eq!(
        first.next[0],
        format!("engram work show {child} --notes --after {after}")
    );
    assert_eq!(first.next[1], recovery);
    assert!(first.text().contains(&format!("  {}\n", first.next[0])));
    assert!(first.text().contains(&format!("  {recovery}\n")));
    assert_parent(&first, &parent, "required");
    assert!(first.text().len() < MAX_AGENT_WORK_RESPONSE_BYTES);
    assert!(serde_json::to_vec_pretty(&first.value).unwrap().len() < MAX_AGENT_WORK_RESPONSE_BYTES);

    let continued = verbs
        .show_records(
            &child,
            &ShowInput {
                notes: true,
                after: Some(after.to_owned()),
                ..ShowInput::default()
            },
            at(21),
        )
        .unwrap();
    for field in ["parent_ref", "parent_title", "parent_lifecycle", "status"] {
        assert!(continued.value.get(field).is_none());
    }
    assert!(
        continued.value["full_detail"]
            .as_str()
            .unwrap()
            .contains(&child)
    );
    assert!(
        !continued
            .text()
            .lines()
            .any(|line| line.starts_with("parent:"))
    );
    assert!(
        !continued
            .next
            .contains(&format!("engram work show {parent}"))
    );
}

#[test]
fn show_parent_correction_roundtripped_child_refuses_missing_parent() {
    let (_directory, verbs, path, project) = fixture();
    let parent = add(&verbs, "Parent", None, false, 0);
    let child = add(&verbs, "Child", Some(&parent), false, 1);
    let service = LocalWorkService::new(
        path,
        project,
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let full = service.work_focus_for_agent(&child, at(2)).unwrap();
    assert!(verbs.render_show(&full, at(2)).is_ok());
    let roundtripped: WorkFocusView =
        serde_json::from_value(serde_json::to_value(&full).unwrap()).unwrap();
    assert!(roundtripped.status.work.parent_id.is_some());
    assert!(roundtripped.parent.is_none());
    assert!(matches!(
        verbs.render_show(&roundtripped, at(2)),
        Err(VerbError {
            error: StoreError::InvalidWorkProjection(_),
            ..
        })
    ));
    let root = service.work_focus_for_agent(&parent, at(3)).unwrap();
    let root: WorkFocusView = serde_json::from_value(serde_json::to_value(&root).unwrap()).unwrap();
    assert!(
        verbs
            .render_show(&root, at(3))
            .unwrap()
            .text()
            .contains("parent: root")
    );
}
