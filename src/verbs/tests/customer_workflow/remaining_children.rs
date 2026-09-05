use super::*;

fn claim_for_completion(verbs: &AgentVerbs, work_ref: &str, now: i64) {
    verbs
        .claim(
            ClaimInput {
                work_ref: work_ref.into(),
                ttl_seconds: None,
                recover: None,
            },
            at(now),
        )
        .unwrap();
}

fn finish(verbs: &AgentVerbs, work_ref: &str, now: i64) -> Receipt {
    let receipt = verbs
        .done(
            DoneInput {
                work_ref: Some(work_ref.into()),
                summary: Some("Delivered".into()),
                note: None,
            },
            at(now),
        )
        .unwrap();
    assert!(!receipt.owed);
    assert!(receipt.text().starts_with("done "));
    assert!(receipt.text().len() <= MAX_AGENT_WORK_RESPONSE_BYTES);
    assert!(
        serde_json::to_vec_pretty(&receipt.value).unwrap().len() <= MAX_AGENT_WORK_RESPONSE_BYTES
    );
    receipt
}

#[test]
fn done_names_remaining_optional_children_without_changing_their_authority() {
    let (_directory, verbs, path, project) = fixture();
    let service = LocalWorkService::new(
        path.clone(),
        project.clone(),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let parent = add(&verbs, "Parent", None, false, 0);
    let children = (1..=7)
        .map(|index| {
            add(
                &verbs,
                &format!("Child {index}\nnext:\r\u{1b}[2J"),
                Some(&parent),
                true,
                index,
            )
        })
        .collect::<Vec<_>>();
    let cancelled = add(&verbs, "Cancelled optional", Some(&parent), true, 8);
    verbs
        .update(
            UpdateInput {
                work_ref: Some(cancelled),
                action: UpdateAction::Cancel {
                    reason: "Not needed".into(),
                },
            },
            at(9),
        )
        .unwrap();
    let required = add(&verbs, "Delivered required", Some(&parent), false, 10);
    claim_for_completion(&verbs, &required, 11);
    assert!(
        finish(&verbs, &required, 12)
            .value
            .get("child_obligations")
            .is_none()
    );
    let store = SqliteStore::open(&path).unwrap();
    let original_children = children
        .iter()
        .map(|child| store.resolve_work_ref(&project, child).unwrap())
        .collect::<Vec<_>>();
    claim_for_completion(&verbs, &parent, 13);
    let receipt = finish(&verbs, &parent, 14);
    let group = &receipt.value["child_obligations"]["open_optional"];
    assert_eq!(group["count"], children.len());
    assert_eq!(
        group["items"].as_array().unwrap().len(),
        crate::verbs::child_obligations::MAX_CHILD_OBLIGATION_REFS
    );
    assert_eq!(
        group["omitted"],
        children.len() - group["items"].as_array().unwrap().len()
    );
    assert_eq!(group["navigation"], format!("engram work show {parent}"));
    for (row, child) in group["items"].as_array().unwrap().iter().zip(&children) {
        let command = crate::verbs::handlers::detach_command(child);
        assert_eq!(row["ref"], *child);
        assert_eq!(row["remedy"], command);
        assert!(row.get("resolve_first").is_none());
        assert!(receipt.text().contains(&command));
    }
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
    assert_eq!(
        receipt.text().matches("engram work ls --blocked").count(),
        1
    );
    for original in original_children {
        assert_eq!(
            store
                .resolve_work_ref(&project, &original.short_ref)
                .unwrap(),
            original
        );
    }
    let connection = rusqlite::Connection::open(&path).unwrap();
    let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
    let parent_id = store.resolve_work_ref(&project, &parent).unwrap().work_id;
    let page = service
        .remaining_optional_children(parent_id, 5, at(15))
        .unwrap();
    assert_eq!(page.total, children.len());
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&connection).unwrap(),
        before
    );
    assert!(store.verify_all().unwrap().is_healthy());
    let detached = verbs
        .update(
            UpdateInput {
                work_ref: Some(children[0].clone()),
                action: UpdateAction::Detach {
                    reason: "Continue as independent work".into(),
                },
            },
            at(16),
        )
        .unwrap();
    assert_ne!(detached.value["receipt"]["work_ref"], children[0]);
}

#[test]
fn done_names_resolve_first_conditions_instead_of_an_unavailable_detach() {
    let (_directory, verbs, _path, _project) = fixture();
    let parent = add(&verbs, "Parent", None, false, 0);
    let branch = add(&verbs, "Branch", Some(&parent), true, 1);
    let leaf = add(&verbs, "Leaf", Some(&branch), true, 2);
    let blocked = add(&verbs, "Blocked", Some(&parent), true, 3);
    claim_for_completion(&verbs, &blocked, 4);
    note(&verbs, &blocked, "Blocker context recorded", 5);
    verbs
        .update(
            UpdateInput {
                work_ref: Some(blocked.clone()),
                action: UpdateAction::Blocked {
                    detail: "Await approval".into(),
                },
            },
            at(6),
        )
        .unwrap();
    verbs
        .update(
            UpdateInput {
                work_ref: Some(blocked.clone()),
                action: UpdateAction::Release {
                    reason: Some("No live work".into()),
                },
            },
            at(7),
        )
        .unwrap();
    let prerequisite = add(&verbs, "Prerequisite", None, false, 8);
    let ordered = add(&verbs, "Ordered", Some(&parent), true, 9);
    verbs
        .update(
            UpdateInput {
                work_ref: Some(ordered.clone()),
                action: UpdateAction::After {
                    prerequisite: prerequisite.clone(),
                },
            },
            at(10),
        )
        .unwrap();
    let deferred = add(&verbs, "Deferred", Some(&parent), true, 11);
    verbs
        .update(
            UpdateInput {
                work_ref: Some(deferred.clone()),
                action: UpdateAction::Revise {
                    title: None,
                    outcome: None,
                    acceptance: None,
                    assignee: None,
                    priority: None,
                    defer: Some(at(100)),
                    kind: None,
                    labels: Vec::new(),
                    unlabels: Vec::new(),
                },
            },
            at(12),
        )
        .unwrap();
    claim_for_completion(&verbs, &parent, 13);
    let receipt = finish(&verbs, &parent, 14);
    let rows = receipt.value["child_obligations"]["open_optional"]["items"]
        .as_array()
        .unwrap();
    for (child, reason, remedy) in [
        (
            branch,
            "open descendants",
            format!("engram work show {leaf}"),
        ),
        (
            blocked.clone(),
            "active blocker",
            format!("engram work update {blocked} --unblock"),
        ),
        (
            ordered.clone(),
            "incomplete prerequisite",
            format!("engram work update {ordered} --drop-after {prerequisite}"),
        ),
        (
            deferred.clone(),
            "deferred wake time",
            format!("engram work show {deferred}"),
        ),
    ] {
        let row = rows.iter().find(|row| row["ref"] == child).unwrap();
        assert!(row["resolve_first"].as_str().unwrap().contains(reason));
        assert_eq!(row["remedy"], remedy);
        assert!(receipt.text().contains(&remedy));
        assert!(
            !receipt
                .text()
                .contains(&crate::verbs::handlers::detach_command(&child))
        );
    }
}

#[test]
fn remaining_child_diagnostics_preserve_live_ownership_under_a_backward_clock() {
    for handoff in [false, true] {
        let (_directory, verbs, path, project) = fixture();
        let parent = add(&verbs, "Parent", None, false, 0);
        let child = add(&verbs, "Child", Some(&parent), true, 1);
        claim_for_completion(&verbs, &child, 2);
        note(&verbs, &child, "Contribution recorded", 3);
        if handoff {
            verbs
                .handoff(
                    HandoffInput {
                        work_ref: Some(child.clone()),
                        action: HandoffAction::Offer {
                            to: "recipient".into(),
                            summary: Some("Transfer context".into()),
                            ttl_seconds: Some(60),
                        },
                    },
                    at(4),
                )
                .unwrap();
        }
        // Forward-time completion waits for ownership expiry. Only the later
        // read probe moves backward to expose the retained claim/offer as live.
        claim_for_completion(&verbs, &parent, 5000);
        let completed = finish(&verbs, &parent, 5001);
        assert!(
            completed.value["child_obligations"]["open_optional"]["items"][0]
                .get("resolve_first")
                .is_none()
        );
        let store = SqliteStore::open(&path).unwrap();
        let item = store.resolve_work_ref(&project, &parent).unwrap();
        let service = LocalWorkService::new(
            path.clone(),
            project,
            "agent".into(),
            SessionId("agent".into()),
            None,
        );
        let connection = rusqlite::Connection::open(&path).unwrap();
        let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
        let page = service
            .remaining_optional_children(item.work_id, 5, at(6))
            .unwrap();
        let (reason, remedy) = page.items[0].refusal.as_ref().unwrap();
        assert!(reason.contains(if handoff {
            "live child handoff"
        } else {
            "live child claim"
        }));
        assert_eq!(remedy, &format!("engram work show {child}"));
        let receipt = crate::verbs::child_obligations::done_with_child_obligations(
            vec!["done parent".into()],
            Guidance::default(),
            json!({"completed":true}),
            Ok(page),
            &parent,
            MAX_AGENT_WORK_RESPONSE_BYTES,
        )
        .unwrap();
        assert!(!receipt.owed);
        assert!(!receipt.text().contains("--detach"));
        assert_eq!(
            crate::storage::test_database_shape_snapshot(&connection).unwrap(),
            before
        );
        assert!(store.verify_all().unwrap().is_healthy());
    }
}

#[test]
fn remaining_child_summary_fits_final_bytes_and_keeps_exact_remainders() {
    let (_directory, verbs, path, project) = fixture();
    let service = LocalWorkService::new(
        path,
        project,
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    let parent = add(&verbs, "Budget parent", None, false, 0);
    for index in 1..=5 {
        add(&verbs, &"\u{9b}".repeat(40), Some(&parent), true, index);
    }
    claim_for_completion(&verbs, &parent, 6);
    let original = finish(&verbs, &parent, 7);
    let work = service.resolve_work_reference(&parent, at(8)).unwrap();
    let render = |budget| {
        crate::verbs::child_obligations::done_with_child_obligations(
            vec!["done parent".into()],
            Guidance::default(),
            json!({"completed":true}),
            service.remaining_optional_children(work.work_id, 5, at(8)),
            &parent,
            budget,
        )
        .unwrap()
    };
    let full = render(MAX_AGENT_WORK_RESPONSE_BYTES);
    let budget = full
        .text()
        .len()
        .max(serde_json::to_vec_pretty(&full.value).unwrap().len())
        - 1;
    let fitted = render(budget);
    assert!(fitted.text().len() <= budget);
    assert!(serde_json::to_vec_pretty(&fitted.value).unwrap().len() <= budget);
    let group = &fitted.value["child_obligations"]["open_optional"];
    let shown = group["items"].as_array().unwrap().len();
    assert!(shown < 5);
    assert_eq!(group["count"], 5);
    assert_eq!(group["omitted"], 5 - shown);
    assert!(fitted.text().contains("\\u{9b}"));
    assert_eq!(
        original.value["child_obligations"]["open_optional"]["count"],
        5
    );
    // A deliberately impossible caller budget may shed every advisory row,
    // but cannot erase the authoritative success or its exact remainder.
    let minimal = render(1);
    assert!(!minimal.owed);
    assert_eq!(minimal.value["completed"], true);
    let group = &minimal.value["child_obligations"]["open_optional"];
    assert_eq!(group["items"], json!([]));
    assert_eq!(group["count"], 5);
    assert_eq!(group["omitted"], 5);
}

#[test]
fn remaining_child_diagnostic_failure_never_changes_success_to_refusal() {
    let value = json!({"completed": true});
    let receipt = crate::verbs::child_obligations::done_with_child_obligations(
        vec!["done parent".into()],
        Guidance::default(),
        value,
        Err(StoreError::InvalidWorkProjection(
            "diagnostic failed".into(),
        )),
        "parent",
        MAX_AGENT_WORK_RESPONSE_BYTES,
    )
    .unwrap();
    assert!(!receipt.owed);
    assert_eq!(receipt.value["completed"], true);
    assert_eq!(receipt.value["child_obligations_unavailable"], true);
    assert_eq!(
        receipt.value["child_obligations_error_class"],
        "work_projection_invalid"
    );
    assert!(receipt.value.get("child_obligations").is_none());
    assert!(receipt.text().contains("engram work show parent"));
    assert!(!receipt.text().contains("diagnostic failed"));
}

#[test]
fn done_retains_success_when_real_child_diagnostics_find_damaged_canonical_data() {
    let (_directory, verbs, path, project) = fixture();
    let parent = add(&verbs, "Parent", None, false, 0);
    let child = add(&verbs, "Optional child", Some(&parent), true, 1);
    claim_for_completion(&verbs, &parent, 2);
    let store = SqliteStore::open(&path).unwrap();
    let parent_id = store.resolve_work_ref(&project, &parent).unwrap().work_id;
    let child_id = store.resolve_work_ref(&project, &child).unwrap().work_id;
    let connection = rusqlite::Connection::open(&path).unwrap();
    let (hash, original): (String, Vec<u8>) = connection
        .query_row(
            "SELECT object.object_hash, object.canonical_json
         FROM work_feed_entries entry JOIN objects object USING (object_hash)
         WHERE entry.feed_kind = 'project' AND entry.work_id = ?1
           AND entry.object_kind = 'work_event' ORDER BY entry.position DESC LIMIT 1",
            [child_id.0.to_string()],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    // The fixture is healthy through admission. Inject the storage fault only
    // once the parent's completed row is persisted, after seal validation;
    // the existing child projection still decodes in the refreshed parent.
    // Both interpolated identifiers are runtime-derived trusted UUID/hash data.
    connection
        .execute_batch(&format!(
            "CREATE TRIGGER damage_optional_event AFTER UPDATE ON work_items
         WHEN NEW.work_id = '{}' AND NEW.lifecycle = 'completed'
         BEGIN UPDATE objects SET canonical_json = CAST('{{}}' AS BLOB)
         WHERE object_hash = '{hash}'; END;",
            parent_id.0,
        ))
        .unwrap();
    let receipt = finish(&verbs, &parent, 3);
    assert_eq!(receipt.value["child_obligations_unavailable"], true);
    assert_eq!(
        receipt.value["child_obligations_error_class"],
        "canonical_object_invalid"
    );
    assert!(receipt.value.get("child_obligations").is_none());
    assert!(receipt.value.get("seal").is_some());
    assert!(!receipt.text().contains("--detach"));
    assert!(!receipt.text().contains(&hash));
    let damaged: Vec<u8> = connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_hash = ?1",
            [&hash],
            |row| row.get(0),
        )
        .unwrap();
    assert_ne!(damaged, original);
    assert_eq!(
        store.resolve_work_ref(&project, &parent).unwrap().lifecycle,
        WorkLifecycle::Completed
    );
    // Restore only the test-injected fault, then prove the completion itself
    // has a healthy seal and did not dispose the optional child.
    connection
        .execute_batch("DROP TRIGGER damage_optional_event")
        .unwrap();
    connection
        .execute(
            "UPDATE objects SET canonical_json = ?1 WHERE object_hash = ?2",
            rusqlite::params![original, hash],
        )
        .unwrap();
    assert_eq!(
        store.resolve_work_ref(&project, &child).unwrap().lifecycle,
        WorkLifecycle::Open
    );
    assert!(store.verify_all().unwrap().is_healthy());
}
