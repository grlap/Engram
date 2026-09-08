use super::*;

mod followups;

fn supersede(verbs: &AgentVerbs, child: &str, successor: &str, now: i64) {
    verbs
        .update(
            UpdateInput {
                work_ref: Some(child.into()),
                action: UpdateAction::Supersede {
                    replacement: successor.into(),
                    reason: "Requirement delivered by the replacement".into(),
                },
            },
            at(now),
        )
        .unwrap();
}

fn assert_resolution(
    verbs: &AgentVerbs,
    parent: &str,
    child: &str,
    successor: &str,
    resolved: bool,
    now: i64,
) {
    let expected = if resolved {
        "resolved_by_successor"
    } else {
        "owed"
    };
    for notes in [false, true] {
        let receipt = verbs.show_with_notes(child, notes, at(now)).unwrap();
        let resolution = &receipt.value["status"]["work"]["child_resolution"];
        assert_eq!(resolution["ref"], successor);
        assert_eq!(resolution["disposition"], expected);
        assert!(receipt.text().contains(successor));
        assert_eq!(resolution.get("remedy").is_some(), !resolved);
    }
    for verbose in [false, true] {
        let receipt = verbs
            .ls(
                &LsInput {
                    under: Some(parent.into()),
                    required: true,
                    all: true,
                    verbose,
                    ..LsInput::default()
                },
                at(now),
            )
            .unwrap();
        let item = receipt.value["items"]
            .as_array()
            .unwrap()
            .iter()
            .find(|item| {
                if verbose {
                    item["work"]["short_ref"] == child
                } else {
                    item["ref"] == child
                }
            })
            .unwrap();
        let resolution = if verbose {
            &item["work"]["child_resolution"]
        } else {
            &item["child_resolution"]
        };
        assert_eq!(resolution["ref"], successor);
        assert_eq!(resolution["disposition"], expected);
        assert!(receipt.text().contains(if resolved {
            "resolved by successor"
        } else {
            "explicit waiver still required"
        }));
    }
    let parent_show = verbs.show(parent, at(now)).unwrap();
    let children = parent_show.value["children"].as_array().unwrap();
    let child_row = children
        .iter()
        .find(|item| item["short_ref"] == child)
        .unwrap();
    assert_eq!(child_row["child_resolution"]["disposition"], expected);
    let owed = parent_show.value["child_obligations"]["required_owed"]["items"]
        .as_array()
        .unwrap();
    assert_eq!(owed.iter().any(|item| item["ref"] == child), !resolved);
    if !resolved {
        let row = owed.iter().find(|item| item["ref"] == child).unwrap();
        assert_eq!(row["child_resolution"]["ref"], successor);
        assert!(
            row["resolve_first"]
                .as_str()
                .unwrap()
                .contains("explicit waiver still required")
        );
        assert_eq!(
            row["remedy"],
            format!("engram work update {parent} --waive {child} --reason \"…\"")
        );
    }
    assert!(parent_show.text().len() < MAX_AGENT_WORK_RESPONSE_BYTES);
    assert!(
        serde_json::to_vec_pretty(&parent_show.value).unwrap().len()
            < MAX_AGENT_WORK_RESPONSE_BYTES
    );
}

#[test]
fn required_successor_resolution_agrees_in_show_listing_and_completion_without_waiver() {
    let (_directory, verbs, path, project) = fixture();
    let parent = add(&verbs, "Parent", None, false, 0);
    let child = add(&verbs, "Original required child", Some(&parent), false, 1);
    let successor = add(&verbs, "Required successor", Some(&parent), false, 2);
    let remaining = (0..3)
        .map(|index| {
            add(
                &verbs,
                &format!("Remaining {index}"),
                Some(&parent),
                false,
                3 + index,
            )
        })
        .collect::<Vec<_>>();
    finish(&verbs, &successor, 6);
    let store = SqliteStore::open(&path).unwrap();
    let successor_id = store
        .resolve_work_ref(&project, &successor)
        .unwrap()
        .work_id;
    let successor_hash = store
        .latest_work_run(successor_id)
        .unwrap()
        .unwrap()
        .completion_seal
        .unwrap();
    let connection = rusqlite::Connection::open(&path).unwrap();
    let original_bytes: Vec<u8> = connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_hash = ?1",
            [successor_hash.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    let unchanged: crate::CompletionSeal = store.get(&successor_hash).unwrap().unwrap();
    assert!(unchanged.required_child_resolutions.is_empty());
    assert!(
        !serde_json::to_value(&unchanged)
            .unwrap()
            .as_object()
            .unwrap()
            .contains_key("required_child_resolutions")
    );
    assert_eq!(
        crate::CanonicalObject::freeze(&unchanged).unwrap().hash(),
        &successor_hash
    );
    supersede(&verbs, &child, &successor, 8);
    assert_resolution(&verbs, &parent, &child, &successor, true, 9);
    assert_eq!(
        verbs.show(&parent, at(9)).unwrap().value["child_obligations"]["required_owed"]["count"],
        3
    );
    for (index, work) in remaining.iter().enumerate() {
        finish(&verbs, work, 10 + i64::try_from(index).unwrap() * 2);
    }
    finish(&verbs, &parent, 20);
    let parent_id = store.resolve_work_ref(&project, &parent).unwrap().work_id;
    let hash = store
        .latest_work_run(parent_id)
        .unwrap()
        .unwrap()
        .completion_seal
        .unwrap();
    let seal: crate::CompletionSeal = store.get(&hash).unwrap().unwrap();
    assert!(seal.required_child_waivers.is_empty());
    assert_eq!(seal.required_child_seals.len(), 4);
    assert_eq!(seal.required_child_resolutions.len(), 1);
    let crate::RequiredChildResolution::ResolvedBySuccessor {
        work_id,
        work_revision,
        supersession,
        successor: resolved,
        successor_seal,
    } = &seal.required_child_resolutions[0];
    let original = store.resolve_work_ref(&project, &child).unwrap();
    assert_eq!(*work_id, original.work_id);
    assert_eq!(*work_revision, original.revision);
    assert_eq!(*resolved, successor_id);
    assert_eq!(*successor_seal, successor_hash);
    let event: crate::WorkEvent = store.get(supersession).unwrap().unwrap();
    assert_eq!(event.work, original);
    assert_eq!(
        connection
            .query_row::<Vec<u8>, _, _>(
                "SELECT canonical_json FROM objects WHERE object_hash = ?1",
                [successor_hash.as_str()],
                |row| row.get(0)
            )
            .unwrap(),
        original_bytes
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn required_successor_resolution_keeps_nonqualifying_immediate_successors_owed() {
    for shape in ["open", "cancelled", "optional", "other_parent", "chain"] {
        let (_directory, verbs, path, project) = fixture();
        let parent = add(&verbs, "Parent", None, false, 0);
        let child = add(&verbs, "Original", Some(&parent), false, 1);
        let other = add(&verbs, "Other parent", None, false, 2);
        let successor = add(
            &verbs,
            "Successor",
            Some(if shape == "other_parent" {
                &other
            } else {
                &parent
            }),
            shape == "optional",
            3,
        );
        supersede(&verbs, &child, &successor, 4);
        match shape {
            "cancelled" => cancel(&verbs, &successor, 5),
            "chain" => {
                let final_child = add(&verbs, "Final successor", Some(&parent), false, 5);
                supersede(&verbs, &successor, &final_child, 6);
                finish(&verbs, &final_child, 7);
            }
            "optional" | "other_parent" => finish(&verbs, &successor, 5),
            "open" => {}
            _ => unreachable!(),
        }
        assert_resolution(&verbs, &parent, &child, &successor, false, 10);
        verbs
            .claim(
                ClaimInput {
                    work_ref: parent.clone(),
                    ttl_seconds: None,
                    recover: None,
                },
                at(11),
            )
            .unwrap();
        let receipt = verbs
            .done(
                DoneInput {
                    links: Vec::new(),
                    link_basis: None,
                    work_ref: Some(parent.clone()),
                    summary: Some("Parent delivery".into()),
                    note: None,
                },
                at(12),
            )
            .unwrap();
        assert!(receipt.owed, "{shape}");
        let store = SqliteStore::open(&path).unwrap();
        let id = store.resolve_work_ref(&project, &parent).unwrap().work_id;
        assert!(
            store
                .latest_work_run(id)
                .unwrap()
                .unwrap()
                .completion_seal
                .is_none()
        );
        assert!(store.verify_all().unwrap().is_healthy());
    }
}

fn reopen(service: &LocalWorkService, work: &str, now: i64) {
    service.work_focus(work, at(now)).unwrap();
    service
        .work_update(
            crate::work_service::WorkUpdateInput::Reopen {
                reason: "New execution generation".into(),
                idempotency_key: format!("reopen-{work}-{now}"),
            },
            at(now + 1),
        )
        .unwrap();
}

#[test]
fn required_successor_resolution_never_transfers_across_root_generations() {
    let (_directory, verbs, path, project) = fixture();
    let parent = add(&verbs, "Parent", None, false, 0);
    let old = add(&verbs, "Old requirement", Some(&parent), false, 1);
    let successor = add(&verbs, "Successor", Some(&parent), false, 2);
    supersede(&verbs, &old, &successor, 3);
    finish(&verbs, &successor, 4);
    finish(&verbs, &parent, 6);
    let store = SqliteStore::open(&path).unwrap();
    let parent_id = store.resolve_work_ref(&project, &parent).unwrap().work_id;
    let old_hash = store
        .latest_work_run(parent_id)
        .unwrap()
        .unwrap()
        .completion_seal
        .unwrap();
    let old_seal: crate::CompletionSeal = store.get(&old_hash).unwrap().unwrap();
    let service = LocalWorkService::new(
        path.clone(),
        project.clone(),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    reopen(&service, &parent, 8);
    // C's supersession is from the old generation, even if S is completed.
    assert_resolution(&verbs, &parent, &old, &successor, false, 10);
    let fresh = add(&verbs, "New requirement", Some(&parent), false, 11);
    supersede(&verbs, &fresh, &successor, 12);
    // The new supersession must not adopt S's old-generation seal either.
    assert_resolution(&verbs, &parent, &fresh, &successor, false, 13);
    assert!(
        verbs.show(&fresh, at(13)).unwrap().value["status"]["work"]["child_resolution"]["reason"]
            .as_str()
            .unwrap()
            .starts_with("successor belongs to another execution generation")
    );
    reopen(&service, &successor, 14);
    finish(&verbs, &successor, 16);
    assert_resolution(&verbs, &parent, &fresh, &successor, true, 18);
    assert_resolution(&verbs, &parent, &old, &successor, false, 18);
    verbs
        .claim(
            ClaimInput {
                work_ref: parent.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(19),
        )
        .unwrap();
    let refused = verbs
        .done(
            DoneInput {
                links: Vec::new(),
                link_basis: None,
                work_ref: Some(parent),
                summary: Some("Still missing old requirement".into()),
                note: None,
            },
            at(20),
        )
        .unwrap();
    assert!(refused.owed);
    assert!(refused.text().contains(&old));
    assert_eq!(
        store
            .get::<crate::CompletionSeal>(&old_hash)
            .unwrap()
            .unwrap(),
        old_seal
    );
    assert_eq!(
        crate::CanonicalObject::freeze(&old_seal).unwrap().hash(),
        &old_hash
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn required_successor_resolution_does_not_adopt_restored_only_completion() {
    let (directory, verbs, path, project) = fixture();
    let parent = add(&verbs, "Parent", None, false, 0);
    let child = add(&verbs, "Requirement", Some(&parent), false, 1);
    let successor = add(&verbs, "Delivered successor", Some(&parent), false, 2);
    finish(&verbs, &successor, 3);
    let mut source = SqliteStore::open(&path).unwrap();
    let actor = source
        .resolve_work_ref(&project, &parent)
        .unwrap()
        .created_by;
    let document = source
        .save_work_graph_snapshot(
            &project,
            &actor,
            None,
            crate::WorkGraphSnapshotDestinationKind::Stdout,
            at(5),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap()
        .document;
    let restored_path = directory.path().join("restored.db");
    let mut store = SqliteStore::open(&restored_path).unwrap();
    store
        .load_work_graph_snapshot(
            &project,
            &actor,
            &serde_json::to_vec(&document).unwrap(),
            false,
            at(6),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let reader = AgentVerbs::new(
        restored_path,
        project.clone(),
        "agent".into(),
        SessionId("agent".into()),
        None,
    );
    reader
        .claim(
            ClaimInput {
                work_ref: parent.clone(),
                ttl_seconds: None,
                recover: None,
            },
            at(7),
        )
        .unwrap();
    supersede(&reader, &child, &successor, 8);
    assert_resolution(&reader, &parent, &child, &successor, false, 9);
    assert_eq!(
        reader.show(&child, at(9)).unwrap().value["status"]["work"]["child_resolution"]["reason"],
        "successor has no native completion seal; explicit waiver still required"
    );
    let receipt = reader
        .done(
            DoneInput {
                links: Vec::new(),
                link_basis: None,
                work_ref: Some(parent.clone()),
                summary: Some("Parent".into()),
                note: None,
            },
            at(10),
        )
        .unwrap();
    assert!(receipt.owed);
    assert!(receipt.text().contains(&child));
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn required_successor_resolution_preserves_explicit_waiver_accounting() {
    let (_directory, verbs, path, project) = fixture();
    let parent = add(&verbs, "Parent", None, false, 0);
    let child = add(&verbs, "Original", Some(&parent), false, 1);
    let successor = add(&verbs, "Successor", Some(&parent), false, 2);
    supersede(&verbs, &child, &successor, 3);
    verbs
        .update(
            UpdateInput {
                work_ref: Some(parent.clone()),
                action: UpdateAction::WaiveRequiredChild {
                    child: child.clone(),
                    reason: "Deliberately waived before the replacement completed".into(),
                },
            },
            at(4),
        )
        .unwrap();
    for time in [5, 8] {
        if time == 8 {
            finish(&verbs, &successor, 6);
        }
        let shown = verbs.show(&child, at(time)).unwrap();
        assert_eq!(
            shown.value["status"]["work"]["child_resolution"]["disposition"],
            "waived"
        );
        assert_eq!(
            shown.value["status"]["work"]["child_resolution"]["reason"],
            "accounted by an explicit waiver"
        );
        assert!(
            shown.value["status"]["work"]["child_resolution"]
                .get("remedy")
                .is_none()
        );
        assert!(!shown.text().contains("explicit waiver still required"));
    }
    finish(&verbs, &parent, 10);
    let store = SqliteStore::open(&path).unwrap();
    let id = store.resolve_work_ref(&project, &parent).unwrap().work_id;
    let hash = store
        .latest_work_run(id)
        .unwrap()
        .unwrap()
        .completion_seal
        .unwrap();
    let seal: crate::CompletionSeal = store.get(&hash).unwrap().unwrap();
    assert_eq!(seal.required_child_waivers.len(), 1);
    assert!(seal.required_child_resolutions.is_empty());
    assert!(store.verify_all().unwrap().is_healthy());
}
