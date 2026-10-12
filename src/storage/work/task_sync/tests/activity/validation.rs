use super::*;

fn one_notice(task: WorkId) -> ActivityNotice {
    let id = CanonicalObject::mint(&"start").unwrap().key().clone();
    ActivityNotice {
        activity_id: id.clone(),
        task_id: task,
        origin_machine: "host-A".into(),
        actor: SharedActor::from_actor(&actor("greg/codex")),
        state: ActivityState::Started,
        state_revision: 1,
        event_id: id,
    }
}

#[test]
fn task_sync_activity_validates_typed_ids_keys_task_alias_and_disclosure() {
    let mut tree = candidate();
    let notice = one_notice(tree.tasks[0].id);
    tree.tasks[0]
        .activity
        .entry("host-A".into())
        .or_default()
        .insert(notice.activity_id.clone(), notice.clone());
    tree.validate().unwrap();
    for case in [
        "origin",
        "key",
        "task",
        "revision",
        "actor",
        "actor-final-dot",
        "actor-three-segments",
        "actor-final-prefix",
        "kind-path",
        "restricted",
        "empty-map",
    ] {
        let mut bad = tree.clone();
        let task = &mut bad.tasks[0];
        let notice = task
            .activity
            .get_mut("host-A")
            .unwrap()
            .values_mut()
            .next()
            .unwrap();
        match case {
            "origin" => notice.origin_machine = "host-B".into(),
            "key" => notice.activity_id = CanonicalObject::mint(&"other").unwrap().key().clone(),
            "task" => notice.task_id = WorkId(uuid::Uuid::new_v4()),
            "revision" => notice.state_revision = 0,
            "actor" => notice.actor.actor_id = "C:\\private\\actor".into(),
            "actor-final-dot" => notice.actor.actor_id = "greg/.ssh".into(),
            "actor-three-segments" => notice.actor.actor_id = "x/etc/shadow".into(),
            "actor-final-prefix" => notice.actor.actor_id = "x/Users".into(),
            "kind-path" => notice.actor.actor_kind = "team/var/lib/engram/key.pem".into(),
            "restricted" => task.labels.push("restricted".into()),
            "empty-map" => task.activity.get_mut("host-A").unwrap().clear(),
            _ => unreachable!(),
        }
        assert!(bad.validate().is_err(), "accepted {case}");
    }
    let mut bad = tree.clone();
    let other_id = bad.tasks[1].id;
    bad.tasks[1].activity = bad.tasks[0].activity.clone();
    for notice in bad.tasks[1]
        .activity
        .values_mut()
        .flat_map(|map| map.values_mut())
    {
        notice.task_id = other_id;
    }
    assert!(
        bad.validate()
            .unwrap_err()
            .to_string()
            .contains("duplicate origin-qualified activity")
    );
    // The supported historical 64-hex identity is data, not prose or a path.
    let historical_id = ObjectId::from_canonical_bytes(b"historical activity identity");
    let mut historical = notice;
    historical.activity_id = historical_id.clone();
    historical.event_id = ObjectId::from_canonical_bytes(b"historical finish identity");
    historical.state = ActivityState::Finished;
    historical.state_revision = 2;
    tree.tasks[0].activity = ActivityMap::from([(
        "host-A".into(),
        std::collections::BTreeMap::from([(historical_id, historical)]),
    )]);
    round_trip(&tree);
}

#[test]
fn task_sync_activity_candidate_state_invariants_refuse_before_import_writes() {
    for activity_state in [
        ActivityState::Started,
        ActivityState::Released,
        ActivityState::Abandoned,
        ActivityState::Finished,
    ] {
        for revision in [1, 2, 7] {
            for same_event in [false, true] {
                let mut tree = candidate();
                let mut notice = one_notice(tree.tasks[0].id);
                notice.state = activity_state;
                notice.state_revision = revision;
                if !same_event {
                    notice.event_id = CanonicalObject::mint(&"close").unwrap().key().clone();
                }
                tree.tasks[0].activity = ActivityMap::from([(
                    "host-A".into(),
                    std::collections::BTreeMap::from([(notice.activity_id.clone(), notice)]),
                )]);
                // Stored reads retain the recorded shape without applying new admission policy.
                tree.validate_stored().unwrap();
                let valid = if activity_state == ActivityState::Started {
                    revision == 1 && same_event
                } else {
                    revision >= 2 && !same_event
                };
                if valid {
                    round_trip(&tree);
                } else {
                    assert!(tree.validate().is_err());
                    let mut store = SqliteStore::open_in_memory().unwrap();
                    let before = state(&store);
                    for dry_run in [false, true] {
                        let error = store
                            .import_task_tree(
                                &tree.project.project_id,
                                &actor("importer"),
                                &tree,
                                dry_run,
                                at(9),
                            )
                            .unwrap_err();
                        assert!(error.to_string().contains("event identity disagree"));
                        assert_eq!(state(&store), before);
                    }
                }
            }
        }
    }
}

#[test]
fn task_sync_activity_unknown_fields_and_duplicate_json_members_are_refused() {
    let mut tree = candidate();
    let notice = one_notice(tree.tasks[0].id);
    tree.tasks[0]
        .activity
        .entry("host-A".into())
        .or_default()
        .insert(notice.activity_id.clone(), notice);
    let documents = tree.documents().unwrap();
    let path = format!("tasks/{}.json", tree.tasks[0].id.0);
    let value: serde_json::Value = serde_json::from_slice(&documents[&path]).unwrap();
    for field in [
        "claim_id",
        "session_id",
        "run_id",
        "fence",
        "expires_at",
        "grant",
    ] {
        let mut invalid = value.clone();
        invalid["activity"]["host-A"]
            .as_object_mut()
            .unwrap()
            .values_mut()
            .next()
            .unwrap()[field] = serde_json::json!("private");
        let mut bad = documents.clone();
        bad.insert(path.clone(), serde_json::to_vec(&invalid).unwrap());
        assert!(TaskTree::from_documents(&bad).is_err(), "accepted {field}");
    }
    let text = String::from_utf8(documents[&path].clone()).unwrap();
    let mut duplicate = documents;
    duplicate.insert(
        path,
        text.replace(
            "\"state_revision\": 1",
            "\"state_revision\": 1, \"state_revision\": 2",
        )
        .into_bytes(),
    );
    assert!(TaskTree::from_documents(&duplicate).is_err());
}

#[test]
fn task_sync_activity_exports_visible_omissions_without_redacting_typed_ids() {
    for (label, principal) in [
        (Some("restricted"), "ordinary-author"),
        (None, "C:\\private\\actor"),
        (None, "greg/.ssh"),
        (None, "x/etc/shadow"),
        (None, "x/users"),
        (None, "greg/codex"),
        (None, "org.example/codex"),
    ] {
        let mut store = SqliteStore::open_in_memory().unwrap();
        let mut request = root_request("exclusions", "create", 0);
        if let Some(label) = label {
            request.labels.push(label.into());
        }
        let work = store
            .create_work(&request, &crate::DevelopmentNoopRedactor)
            .unwrap();
        claim(&mut store, &work, principal, "claim", 1, 100);
        let before = state(&store);
        let result = store
            .export_task_tree_with_options(
                &work.project_id,
                &TaskSyncExportOptions {
                    include_notes: false,
                    origin_machine: Some("machine-A".into()),
                },
            )
            .unwrap();
        let admitted = ["greg/codex", "org.example/codex"].contains(&principal);
        if label.is_some() || !admitted {
            assert_eq!(notices(&result.tree).len(), 0, "{principal}");
            assert_eq!(result.activity_omissions.counts.values().sum::<usize>(), 1);
            let shared = result
                .tree
                .documents()
                .unwrap()
                .into_values()
                .flatten()
                .collect::<Vec<_>>();
            let shared = String::from_utf8(shared).unwrap();
            // Compare the JSON form, so an escaped principal cannot hide.
            let encoded = serde_json::to_string(principal).unwrap();
            assert!(!shared.contains(&encoded), "{principal}");
        } else {
            assert_eq!(notices(&result.tree).len(), 1);
            assert!(result.activity_omissions.counts.is_empty());
        }
        assert_eq!(state(&store), before);
    }
    for alias in ["", "C:\\private", "machine with spaces"] {
        assert!(
            TaskSyncExportOptions {
                include_notes: false,
                origin_machine: Some(alias.into())
            }
            .validate()
            .is_err()
        );
    }
    assert!(
        TaskSyncExportOptions {
            include_notes: true,
            origin_machine: None
        }
        .validate()
        .is_err()
    );
}
