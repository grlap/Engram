use super::*;

fn stranded_child(verbs: &AgentVerbs) -> (String, String) {
    let parent = add(verbs, "Parent", None, false, 0);
    let child = add(verbs, "Follow-up", Some(&parent), true, 1);
    note(verbs, &child, "Original finding stays with the source", 2);
    verbs
        .claim(
            ClaimInput {
                work_ref: parent.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(3),
        )
        .expect("claim parent");
    verbs
        .done(
            DoneInput {
                work_ref: Some(parent.clone()),
                summary: Some("Parent delivered".into()),
                note: None,
            },
            at(4),
        )
        .expect("done parent");
    (parent, child)
}

#[test]
fn detach_guidance_and_one_command_successor_are_consistent() {
    let (_directory, verbs, database, project) = fixture();
    let (parent, child) = stranded_child(&verbs);
    let mut parent_before = verbs.show(&parent, at(5)).expect("parent").value;
    let shown = verbs.show(&child, at(5)).expect("show child");
    let command = format!("engram work update {child} --detach \"Continue as independent work\"");
    assert!(shown.text().contains("parent completed"));
    assert_eq!(shown.value["next"][0], command);
    let next = verbs.next(&NextInput::default(), at(5)).expect("next");
    assert!(next.text().contains(&command));
    assert_eq!(next.value["next"][0], command);
    let listed = verbs
        .ls(
            &LsInput {
                blocked: true,
                ..LsInput::default()
            },
            at(5),
        )
        .expect("blocked");
    assert_eq!(listed.value["total"], 1);
    assert!(listed.text().contains("parent completed"));
    assert!(listed.text().contains(&command));
    let receipt = verbs
        .update(
            UpdateInput {
                work_ref: Some(child.clone()),
                action: UpdateAction::Detach {
                    reason: "Follow-up needs its own execution".into(),
                },
            },
            at(6),
        )
        .expect("detach");
    let new_ref = receipt.value["receipt"]["work_ref"]
        .as_str()
        .expect("root ref");
    assert_ne!(new_ref, child);
    assert!(
        receipt
            .text()
            .contains(&format!("as independent root {new_ref}"))
    );
    assert_eq!(
        receipt.value["next"][0],
        format!("engram work claim {new_ref}")
    );
    // Only the live child catalog changes; the parent's own state/history/notes do not.
    parent_before["children"][0]["lifecycle"] = serde_json::json!("superseded");
    assert_eq!(
        parent_before,
        verbs.show(&parent, at(7)).expect("parent").value
    );
    assert!(
        verbs
            .show(&child, at(7))
            .expect("source")
            .text()
            .contains(new_ref)
    );
    let old_notes = verbs
        .show_with_notes(&child, true, at(7))
        .expect("old notes");
    assert!(
        old_notes
            .text()
            .contains("Original finding stays with the source")
    );
    assert_eq!(
        verbs
            .ls(
                &LsInput {
                    blocked: true,
                    ..LsInput::default()
                },
                at(7)
            )
            .expect("blocked after")
            .value["total"],
        0
    );
    verbs
        .claim(
            ClaimInput {
                work_ref: new_ref.into(),
                ttl_seconds: None,
                recover: None,
            },
            at(8),
        )
        .expect("claim successor");
    let store = SqliteStore::open(database).expect("store");
    let successor = store
        .resolve_work_ref(&project, new_ref)
        .expect("successor");
    assert!(successor.parent_id.is_none());
    assert!(store.verify_all().expect("doctor").is_healthy());
}

#[test]
fn detach_blocked_reason_is_independent_of_readiness_prose() {
    let (_directory, verbs, database, project) = fixture();
    let (_, child) = stranded_child(&verbs);
    let service = LocalWorkService::new(
        database,
        project,
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let mut view = service.work_focus(&child, at(5)).expect("child");
    assert_eq!(view.status.blocking_parent, Some(WorkLifecycle::Completed));
    view.status.why.clear();
    let row = crate::verbs::receipts::compact_row(&view.status, &std::collections::HashMap::new());
    assert_eq!(row.blocked_reason.as_deref(), Some("parent completed"));
    assert_eq!(
        row.remedy,
        Some(crate::verbs::handlers::detach_command(&child))
    );
}
