use super::*;

#[test]
fn completed_root_parent_remains_readable_and_detachable_after_child_detach() {
    let (_directory, verbs, database, project) = fixture();
    let root = add(&verbs, "Root", None, false, 0);
    let parent = add(&verbs, "Optional parent", Some(&root), true, 1);
    let child = add(&verbs, "Required child", Some(&parent), false, 2);
    verbs
        .claim(
            ClaimInput {
                work_ref: root.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(3),
        )
        .expect("claim root");
    let done = verbs
        .done(
            DoneInput {
                work_ref: Some(root.clone()),
                summary: Some("Root delivered".into()),
                ..DoneInput::default()
            },
            at(4),
        )
        .expect("complete root with optional subtree");
    assert!(!done.owed);
    let store = SqliteStore::open(&database).expect("store");
    let root_item = store.resolve_work_ref(&project, &root).expect("root");
    let root_run = store.latest_work_run(root_item.work_id).unwrap().unwrap();
    let connection = rusqlite::Connection::open(&database).expect("connection");
    let retained = || {
        let header: Vec<u8> = connection
            .query_row(
                "SELECT header_json FROM work_root_executions WHERE root_execution_id = ?1",
                [root_run.root_execution_id.0.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        let members = connection.prepare(
            "SELECT member_hash, member_json FROM work_root_members WHERE root_execution_id = ?1 ORDER BY member_hash",
        ).unwrap().query_map([root_run.root_execution_id.0.to_string()], |row| {
            Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))
        }).unwrap().collect::<Result<Vec<_>, _>>().unwrap();
        let seal: Vec<u8> = connection
            .query_row(
                "SELECT seal_json FROM work_completion_seals WHERE work_id = ?1",
                [root_item.work_id.0.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        (header, members, seal)
    };
    let before = retained();
    let shown_child = verbs.show(&child, at(5)).expect("stranded grandchild");
    let ancestor = serde_json::json!({"ref": root, "lifecycle": "completed"});
    assert_eq!(shown_child.value["parent_ref"], parent);
    assert_eq!(shown_child.value["parent_lifecycle"], "open");
    assert_eq!(shown_child.value["blocking_ancestor"], ancestor);
    assert!(
        shown_child
            .text()
            .contains(&format!("execution blocked by ancestor {root} (completed)"))
    );
    assert!(!shown_child.text().contains("parent completed"));
    assert_eq!(
        shown_child.value["next"],
        serde_json::json!([
            format!("engram work update {child} --detach \"Continue as independent work\""),
            format!("engram work note {child} \"…\""),
            format!("engram work show {root}"),
            format!("engram work show {parent}"),
            format!("engram work show {child} --history")
        ])
    );
    let child_item = store.resolve_work_ref(&project, &child).unwrap();
    let child_run = store.latest_work_run(child_item.work_id).unwrap().unwrap();
    let claim_error = verbs
        .claim(
            ClaimInput {
                work_ref: child.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(5),
        )
        .expect_err("terminal root prevents claim");
    let refusal_value = crate::mcp::store_error_value(&claim_error.error);
    assert_eq!(refusal_value["error"]["code"], "work_invalid");
    assert_eq!(
        refusal_value["error"]["details"]["blocking_ancestor"],
        ancestor
    );
    let guidance = claim_error.guidance();
    assert!(guidance.next.contains(&format!("engram work show {child}")));
    assert!(guidance.next.contains(&format!("engram work show {root}")));
    assert!(
        !guidance
            .next
            .iter()
            .any(|command| command.contains("--detach"))
    );
    assert_eq!(
        store.latest_work_run(child_item.work_id).unwrap().unwrap(),
        child_run
    );
    assert_eq!(store.current_work_claim(child_item.work_id).unwrap(), None);
    assert_eq!(retained(), before);
    let refusal = verbs
        .update(
            UpdateInput {
                work_ref: Some(parent.clone()),
                action: UpdateAction::Detach {
                    reason: "Continue independently".into(),
                },
            },
            at(5),
        )
        .expect_err("open descendant still blocks parent detach");
    let payload = crate::mcp::store_error_value(&refusal.error);
    assert_eq!(payload["error"]["code"], "work_detach_refused");
    assert_eq!(
        payload["error"]["details"]["reason"],
        "resolve open descendants before detaching their parent"
    );
    assert_eq!(
        payload["error"]["details"]["remedy"],
        format!("engram work show {child}")
    );
    verbs
        .update(
            UpdateInput {
                work_ref: Some(child.clone()),
                action: UpdateAction::Detach {
                    reason: "Child needs independent execution".into(),
                },
            },
            at(6),
        )
        .expect("detach child");
    let shown_child = verbs.show(&child, at(7)).expect("show detached child");
    assert!(!shown_child.text().contains("--waive"));
    assert!(
        !serde_json::to_string(&shown_child.value)
            .unwrap()
            .contains("--waive")
    );
    for verbose in [false, true] {
        let listed = verbs
            .ls(
                &LsInput {
                    under: Some(parent.clone()),
                    required: true,
                    all: true,
                    verbose,
                    ..LsInput::default()
                },
                at(7),
            )
            .expect("list retained required child");
        assert_eq!(listed.value["total"], 1);
        assert!(!listed.text().contains("--waive"));
        assert!(
            !serde_json::to_string(&listed.value)
                .unwrap()
                .contains("--waive")
        );
    }
    let shown = verbs
        .show(&parent, at(7))
        .expect("show stranded intermediate parent");
    assert!(!shown.text().contains("--waive"));
    assert!(
        !serde_json::to_string(&shown.value)
            .unwrap()
            .contains("--waive")
    );
    let rows = shown.value["child_obligations"]["required_owed"]["items"]
        .as_array()
        .unwrap();
    let row = rows.iter().find(|row| row["ref"] == child).unwrap();
    assert_eq!(
        row["remedy"],
        super::super::super::handlers::detach_command(&parent)
    );
    assert_eq!(
        row["resolve_first"],
        "waiver is unavailable; resolve the parent's execution context"
    );
    let view = verbs
        .service
        .work_focus(&parent, at(7))
        .expect("parent guidance");
    assert!(view.waivable_required_children.is_empty());
    note(
        &verbs,
        &parent,
        "Retained parent can receive observations",
        8,
    );
    verbs
        .service
        .select_work(&parent, at(9))
        .expect("select parent");
    let focused = verbs
        .next(&NextInput::default(), at(9))
        .expect("focused next");
    assert_eq!(focused.value["focus"]["ref"], parent);
    assert!(!focused.text().contains("--waive"));
    let detached = verbs
        .update(
            UpdateInput {
                work_ref: Some(parent.clone()),
                action: UpdateAction::Detach {
                    reason: "Parent needs independent execution".into(),
                },
            },
            at(10),
        )
        .expect("detach intermediate parent");
    let successor = detached.value["receipt"]["work_ref"]
        .as_str()
        .expect("successor");
    assert_ne!(successor, parent);
    let successor_item = store.resolve_work_ref(&project, successor).unwrap();
    assert!(successor_item.parent_id.is_none());
    assert_eq!(successor_item.root_id, successor_item.work_id);
    note(
        &verbs,
        successor,
        "Independent successor accepts observations",
        11,
    );
    assert_eq!(
        store.resolve_work_ref(&project, &parent).unwrap().lifecycle,
        WorkLifecycle::Superseded
    );
    assert_eq!(store.get_work_item(root_item.work_id).unwrap(), root_item);
    assert_eq!(
        store.latest_work_run(root_item.work_id).unwrap().unwrap(),
        root_run
    );
    assert_eq!(retained(), before);
    assert!(store.verify_all().expect("doctor").is_healthy());
}

#[test]
fn parent_detach_guidance_requires_all_open_descendants_to_be_resolved() {
    let (_directory, verbs, _database, _project) = fixture();
    let root = add(&verbs, "Root", None, false, 0);
    let parent = add(&verbs, "Optional parent", Some(&root), true, 1);
    let child = add(&verbs, "Required child", Some(&parent), false, 2);
    let sibling = add(&verbs, "Open sibling", Some(&parent), false, 3);
    verbs
        .claim(
            ClaimInput {
                work_ref: root.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(4),
        )
        .unwrap();
    verbs
        .done(
            DoneInput {
                work_ref: Some(root),
                summary: Some("Root delivered".into()),
                ..DoneInput::default()
            },
            at(5),
        )
        .unwrap();
    verbs
        .update(
            UpdateInput {
                work_ref: Some(child.clone()),
                action: UpdateAction::Detach {
                    reason: "Child continues independently".into(),
                },
            },
            at(6),
        )
        .unwrap();
    let shown = verbs.show(&parent, at(7)).unwrap();
    let detach = super::super::super::handlers::detach_command(&parent);
    assert!(!shown.text().contains(&detach));
    assert!(
        !serde_json::to_string(&shown.value)
            .unwrap()
            .contains("--detach")
    );
    let rows = shown.value["child_obligations"]["required_owed"]["items"]
        .as_array()
        .unwrap();
    let row = rows.iter().find(|row| row["ref"] == child).unwrap();
    assert_eq!(row["remedy"], format!("engram work show {child}"));
    assert_eq!(
        row["resolve_first"],
        "waiver is unavailable; resolve the parent's execution context"
    );
    let refusal = verbs
        .update(
            UpdateInput {
                work_ref: Some(parent),
                action: UpdateAction::Detach {
                    reason: "Parent cannot leave open descendants".into(),
                },
            },
            at(8),
        )
        .expect_err("open sibling still blocks detach");
    let payload = crate::mcp::store_error_value(&refusal.error);
    assert_eq!(payload["error"]["code"], "work_detach_refused");
    assert_eq!(
        payload["error"]["details"]["reason"],
        "resolve open descendants before detaching their parent"
    );
    assert_eq!(
        payload["error"]["details"]["remedy"],
        format!("engram work show {sibling}")
    );
}

#[test]
fn superseded_child_waiver_guidance_remains_available_under_an_open_root() {
    let (_directory, verbs, _database, _project) = fixture();
    let parent = add(&verbs, "Open root", None, false, 0);
    let child = add(&verbs, "Required child", Some(&parent), false, 1);
    let successor = add(&verbs, "Independent successor", None, false, 2);
    verbs
        .update(
            UpdateInput {
                work_ref: Some(child.clone()),
                action: UpdateAction::Supersede {
                    replacement: successor,
                    reason: "Continue in independent work".into(),
                },
            },
            at(3),
        )
        .unwrap();
    let command = format!("engram work update {parent} --waive {child} --reason \"…\"");
    let shown = verbs.show(&child, at(4)).unwrap();
    assert!(shown.text().contains(&command));
    assert_eq!(
        shown.value["status"]["work"]["child_resolution"]["remedy"],
        command
    );
    let shown_parent = verbs.show(&parent, at(4)).unwrap();
    let children = shown_parent.value["children"].as_array().unwrap();
    let child_row = children
        .iter()
        .find(|row| row["short_ref"] == child)
        .unwrap();
    assert_eq!(child_row["child_resolution"]["remedy"], command);
    let owed = shown_parent.value["child_obligations"]["required_owed"]["items"]
        .as_array()
        .unwrap();
    let owed_row = owed.iter().find(|row| row["ref"] == child).unwrap();
    assert_eq!(owed_row["remedy"], command);
    for verbose in [false, true] {
        let listed = verbs
            .ls(
                &LsInput {
                    under: Some(parent.clone()),
                    required: true,
                    all: true,
                    verbose,
                    ..LsInput::default()
                },
                at(4),
            )
            .unwrap();
        assert!(listed.text().contains(&command));
        assert!(
            serde_json::to_string(&listed.value)
                .unwrap()
                .contains("--waive")
        );
    }
}

#[test]
fn child_waiver_rendering_preserves_live_and_terminal_parent_guidance() {
    let (_directory, verbs, _database, _project) = fixture();
    let parent = add(&verbs, "Parent", None, false, 0);
    let child = add(&verbs, "Required child", Some(&parent), false, 1);
    verbs
        .update(
            UpdateInput {
                work_ref: Some(child.clone()),
                action: UpdateAction::Cancel {
                    reason: "Not needed".into(),
                },
            },
            at(2),
        )
        .unwrap();
    let shown = verbs.show(&parent, at(3)).unwrap();
    let row = &shown.value["child_obligations"]["required_owed"]["items"][0];
    assert_eq!(
        row["remedy"],
        format!("engram work update {parent} --waive {child} --reason \"…\"")
    );
    assert_eq!(
        row["resolve_first"],
        "disposed required child still needs an explicit waiver"
    );
    verbs
        .update(
            UpdateInput {
                work_ref: Some(parent.clone()),
                action: UpdateAction::Cancel {
                    reason: "Parent retired".into(),
                },
            },
            at(4),
        )
        .unwrap();
    let shown = verbs.show(&parent, at(5)).unwrap();
    let row = &shown.value["child_obligations"]["required_owed"]["items"][0];
    assert_eq!(
        row["resolve_first"],
        "parent is terminal; inspect retained child context"
    );
    assert_eq!(row["remedy"], format!("engram work show {child}"));
    assert!(!shown.text().contains("--waive"));
}

pub(super) fn stranded_child(verbs: &AgentVerbs) -> (String, String) {
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
                source_fingerprint: None,
                landing: None,
                links: Vec::new(),
                link_basis: None,
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
    let cause = format!("execution blocked by ancestor {parent} (completed)");
    assert!(shown.text().contains(&cause));
    assert_eq!(
        shown.value["blocking_ancestor"],
        serde_json::json!({"ref": parent, "lifecycle": "completed"})
    );
    // The child was added without criteria, so its placeholder status leads.
    assert_eq!(
        shown.value["reminders"],
        serde_json::json!([
            "acceptance is only the title placeholder ('Follow-up is done'); set real criteria by revising acceptance with update",
            cause
        ])
    );
    assert_eq!(
        shown.value["next"],
        serde_json::json!([
            command,
            format!("engram work note {child} \"…\""),
            format!("engram work show {parent}"),
            format!("engram work show {child} --history")
        ])
    );
    // The advisory retains the child's remedy without steering focus away
    // from the completed parent.
    let next = verbs.next(&NextInput::default(), at(5)).expect("next");
    assert_eq!(next.value["focus"]["ref"], parent);
    assert_eq!(next.value["focus"]["state"], "completed");
    assert_eq!(next.value["reminders"], serde_json::json!([]));
    assert!(!next.next.contains(&command));
    assert!(next.text().contains(&command));
    assert_eq!(next.value["stranded_children"][0]["remedy"], command);
    // Explicit selection remains supported and retains the original focused
    // detach guidance; only implicit read-side steering is removed.
    verbs
        .service
        .select_work(&child, at(5))
        .expect("explicit focus");
    let focused = verbs
        .next(&NextInput::default(), at(5))
        .expect("focused next");
    assert!(focused.text().contains(&command));
    assert_eq!(focused.value["reminders"], serde_json::json!([cause]));
    assert_eq!(focused.value["next"][0], command);
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
    assert!(listed.text().contains(&cause));
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
    let shown_successor = verbs
        .show_with_notes(new_ref, true, at(7))
        .expect("successor origin");
    assert_eq!(
        shown_successor.value["detached_from"],
        serde_json::json!({
            "ref": child, "reason": "Follow-up needs its own execution"
        })
    );
    assert!(shown_successor.text().contains(&format!(
        "detached from: {child} — Follow-up needs its own execution"
    )));
    assert!(
        shown_successor
            .next
            .contains(&format!("engram work show {child}"))
    );
    let observed = shown_successor.value["notes"]
        .as_array()
        .expect("new root notes");
    assert!(observed.is_empty(), "{observed:?}");
    // Only the live child catalog and its advisory group change; the parent's
    // own state/history/notes do not.
    parent_before["children"][0]["lifecycle"] = serde_json::json!("superseded");
    parent_before["child_obligations"]["open_optional"] = serde_json::json!({
        "count": 0, "items": [], "omitted": 0,
        "navigation": format!("engram work ls --under {parent} --optional")
    });
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
    let (parent, child) = stranded_child(&verbs);
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
    assert_eq!(
        row.blocked_reason,
        Some(format!(
            "execution blocked by ancestor {parent} (completed)"
        ))
    );
    assert_eq!(
        view.status.blocking_ancestor.as_ref().unwrap().short_ref,
        parent
    );
    assert_eq!(
        row.remedy,
        Some(crate::verbs::handlers::detach_command(&child))
    );
}

#[test]
fn detached_origin_requires_reciprocal_canonical_history() {
    let (_directory, verbs, database, project) = fixture();
    let (_, child) = stranded_child(&verbs);
    let unrelated = add(&verbs, "Unrelated root", None, false, 5);
    let store = SqliteStore::open(&database).expect("store");
    let source = store.resolve_work_ref(&project, &child).expect("source");
    let mut asserted = store.resolve_work_ref(&project, &unrelated).expect("root");
    asserted
        .created_by
        .provenance_chain
        .push(crate::domain::ProvenanceLink {
            relation: crate::domain::ProvenanceRelation::DerivedFrom,
            source: "work_detach".into(),
            reference: Some(source.work_id.0.to_string()),
        });
    assert_eq!(
        store
            .detached_work_origin(&asserted)
            .expect("assertion only"),
        None
    );
    let receipt = verbs
        .update(
            UpdateInput {
                work_ref: Some(child.clone()),
                action: UpdateAction::Detach {
                    reason: "Recorded reason".into(),
                },
            },
            at(6),
        )
        .expect("detach");
    let successor = store
        .resolve_work_ref(
            &project,
            receipt.value["receipt"]["work_ref"].as_str().unwrap(),
        )
        .expect("successor");
    assert_eq!(
        store.detached_work_origin(&successor).expect("origin"),
        Some((child, "Recorded reason".into()))
    );
    let connection = rusqlite::Connection::open(&database).expect("connection");
    let (hash, bytes): (String, Vec<u8>) = connection
        .query_row(
            "SELECT object.object_id, object.canonical_json FROM objects object
         JOIN work_feed_entries entry ON entry.object_id = object.object_id
         WHERE entry.feed_kind = 'project' AND entry.work_id = ?1
           AND json_extract(object.canonical_json, '$.transition.kind') = 'disposed'
         ORDER BY entry.position DESC LIMIT 1",
            [source.work_id.0.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .expect("source event");
    connection
        .execute(
            "UPDATE objects SET canonical_json = ?1 WHERE object_id = ?2",
            rusqlite::params![b"{}".as_slice(), hash],
        )
        .expect("damage source proof");
    // The damaged source event no longer decodes as a work event.
    match store
        .detached_work_origin(&successor)
        .expect_err("must verify source")
    {
        StoreError::Json(_) => {}
        error => panic!("unexpected refusal: {error}"),
    }
    connection
        .execute(
            "UPDATE objects SET canonical_json = ?1 WHERE object_id = ?2",
            rusqlite::params![bytes, hash],
        )
        .expect("restore exact bytes");
    assert!(store.verify_all().expect("doctor").is_healthy());
}

#[test]
fn detached_origin_preserves_a_full_reason_without_copying_source_notes() {
    let (_directory, verbs, _database, _project) = fixture();
    let (_, child) = stranded_child(&verbs);
    let reason = "Long recorded detach reason. ".repeat(80).trim().to_owned();
    let receipt = verbs
        .update(
            UpdateInput {
                work_ref: Some(child.clone()),
                action: UpdateAction::Detach {
                    reason: reason.clone(),
                },
            },
            at(5),
        )
        .expect("detach");
    let successor = receipt.value["receipt"]["work_ref"].as_str().unwrap();
    let shown = verbs
        .show_with_notes(successor, true, at(6))
        .expect("bounded show");
    assert_eq!(shown.value["detached_from"]["ref"], child);
    assert_eq!(shown.value["detached_from"]["reason"], reason);
    assert!(shown.value["detached_from"].get("reason_omitted").is_none());
    assert!(
        shown.value["detached_from"]
            .get("reason_truncated")
            .is_none()
    );
    assert!(shown.text().contains(&reason));
    assert!(shown.next.contains(&format!("engram work show {child}")));
    assert_eq!(shown.value["notes"], serde_json::json!([]));
    assert!(shown.text().len() <= MAX_AGENT_WORK_RESPONSE_BYTES);
    assert!(serde_json::to_vec(&shown.value).unwrap().len() <= MAX_AGENT_WORK_RESPONSE_BYTES);
}

#[test]
fn detached_origin_terminal_reason_is_one_safe_line() {
    let (_directory, verbs, _database, _project) = fixture();
    let (_, child) = stranded_child(&verbs);
    let reason = "Reason\r\u{1b}[2J\nnext:\n  forged command";
    let detached = verbs
        .update(
            UpdateInput {
                work_ref: Some(child),
                action: UpdateAction::Detach {
                    reason: reason.into(),
                },
            },
            at(5),
        )
        .unwrap();
    let successor = detached.value["receipt"]["work_ref"].as_str().unwrap();
    let shown = verbs.show(successor, at(6)).unwrap();
    assert_eq!(shown.value["detached_from"]["reason"], reason);
    let text = shown.text();
    let line = text
        .lines()
        .find(|line| line.starts_with("detached from:"))
        .unwrap();
    assert!(line.contains("Reason\\r\\u{1b}[2J next: forged command"));
    assert!(!line.contains('\r'));
    assert!(!line.contains('\u{1b}'));
    assert_eq!(text.lines().filter(|line| *line == "next:").count(), 1);
    assert!(text.len() <= MAX_AGENT_WORK_RESPONSE_BYTES);
    assert!(serde_json::to_vec(&shown.value).unwrap().len() <= MAX_AGENT_WORK_RESPONSE_BYTES);
}
