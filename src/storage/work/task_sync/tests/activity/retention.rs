use super::*;

fn activity_candidate() -> TaskTree {
    let mut tree = candidate();
    let mut root = tree
        .tasks
        .iter()
        .find(|task| task.parent_id.is_none() && task.lifecycle == WorkLifecycle::Open)
        .unwrap()
        .clone();
    root.prerequisites.clear();
    root.blockers.clear();
    root.assigned_to = None;
    root.deferred_until = None;
    tree.tasks = vec![root];
    let task = &mut tree.tasks[0];
    for machine in ["machine-A", "machine-B"] {
        let id = CanonicalObject::mint(&machine).unwrap().key().clone();
        task.activity.entry(machine.into()).or_default().insert(
            id.clone(),
            ActivityNotice {
                activity_id: id.clone(),
                task_id: task.id,
                origin_machine: machine.into(),
                actor: SharedActor::from_actor(&actor("foreign-author")),
                state: ActivityState::Started,
                state_revision: 1,
                event_id: id,
            },
        );
    }
    tree
}

#[test]
fn task_sync_activity_imported_maps_survive_native_alias_and_remain_inert() {
    let original = activity_candidate();
    round_trip(&original);
    let mut store = SqliteStore::open_in_memory().unwrap();
    import(&mut store, &original);
    let exported = export(&store, &original.project.project_id, "machine-C");
    assert_eq!(exported.documents().unwrap(), original.documents().unwrap());
    assert!(
        exported
            .tasks
            .iter()
            .all(|task| !task.activity.contains_key("machine-C"))
    );
    let work = store.get_work_item(original.tasks[0].id).unwrap();
    assert_eq!(work.origin, crate::WorkOrigin::Local);
    let live = store
        .claim_work(
            &crate::domain::ClaimWorkRequest {
                work_id: work.work_id,
                expected_work_revision: work.revision,
                expected_run_id: None,
                holder: crate::SessionId("native-holder".into()),
                ttl_seconds: 100,
                recovery_reason: None,
                actor: actor("native-holder"),
                idempotency_key: "claim-imported".into(),
                claimed_at: at(7),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let started = export(&store, &original.project.project_id, "machine-C");
    let task = started
        .tasks
        .iter()
        .find(|task| task.id == work.work_id)
        .unwrap();
    for machine in ["machine-A", "machine-B"] {
        assert_eq!(task.activity[machine], original.tasks[0].activity[machine]);
    }
    assert_eq!(task.activity["machine-C"].len(), 1);
    let work = store.get_work_item(work.work_id).unwrap();
    release(&mut store, &work, &live, 8);
    let released = export(&store, &original.project.project_id, "machine-C");
    let task = released
        .tasks
        .iter()
        .find(|task| task.id == work.work_id)
        .unwrap();
    assert!(
        task.activity["machine-C"]
            .values()
            .all(|n| n.state == ActivityState::Released)
    );
    for machine in ["machine-A", "machine-B"] {
        assert_eq!(task.activity[machine], original.tasks[0].activity[machine]);
    }
    round_trip(&released);
}

#[test]
fn task_sync_activity_changed_rollback_deletion_and_cross_project_collision_refuse_atomically() {
    let mut original = activity_candidate();
    let retained = original.tasks[0]
        .activity
        .get_mut("machine-A")
        .unwrap()
        .values_mut()
        .next()
        .unwrap();
    retained.state = ActivityState::Released;
    retained.state_revision = 2;
    retained.event_id = CanonicalObject::mint(&"release").unwrap().key().clone();
    let mut store = SqliteStore::open_in_memory().unwrap();
    import(&mut store, &original);
    let before = state(&store);
    let mut candidates = Vec::new();
    let mut changed = original.clone();
    let notice = changed.tasks[0].activity["machine-A"]
        .values()
        .next()
        .unwrap()
        .clone();
    let updated = changed.tasks[0]
        .activity
        .get_mut("machine-A")
        .unwrap()
        .get_mut(&notice.activity_id)
        .unwrap();
    updated.state = ActivityState::Finished;
    updated.state_revision += 1;
    updated.event_id = CanonicalObject::mint(&"finish").unwrap().key().clone();
    candidates.push((changed, true));
    let mut deleted = original.clone();
    deleted.tasks[0].activity.clear();
    candidates.push((deleted, false));
    let mut rollback = original.clone();
    let earlier = rollback.tasks[0]
        .activity
        .get_mut("machine-A")
        .unwrap()
        .get_mut(&notice.activity_id)
        .unwrap();
    earlier.state = ActivityState::Started;
    earlier.state_revision = 1;
    earlier.event_id.clone_from(&earlier.activity_id);
    candidates.push((rollback, true));
    for (tree, known_change) in candidates {
        tree.validate().unwrap();
        for dry_run in [false, true] {
            let error = store
                .import_task_tree(
                    &original.project.project_id,
                    &actor("importer"),
                    &tree,
                    dry_run,
                    at(9),
                )
                .unwrap_err();
            if known_change {
                assert!(error.to_string().contains("known activity differs"));
                assert_eq!(
                    crate::store_error_value(&error)["error"]["code"],
                    "task_sync_divergent_planning"
                );
            }
            assert_eq!(state(&store), before);
        }
    }
    // The same known activity identity moved onto another task of the same
    // project collides instead of diverging.
    let mut moved = original.clone();
    let entries = moved.tasks[0].activity.get_mut("machine-A").unwrap();
    let entry = entries.remove(&notice.activity_id).unwrap();
    if entries.is_empty() {
        moved.tasks[0].activity.remove("machine-A");
    }
    let mut other_task = moved.tasks[0].clone();
    other_task.id = WorkId(uuid::Uuid::new_v4());
    other_task.external_ref = None;
    other_task.activity = std::collections::BTreeMap::default();
    other_task
        .activity
        .entry("machine-A".into())
        .or_default()
        .insert(
            entry.activity_id.clone(),
            ActivityNotice {
                task_id: other_task.id,
                ..entry
            },
        );
    moved.tasks.push(other_task);
    moved.tasks.sort_by_key(|task| task.id.0);
    moved.validate().unwrap();
    let error = store
        .import_task_tree(
            &original.project.project_id,
            &actor("importer"),
            &moved,
            false,
            at(9),
        )
        .unwrap_err();
    assert!(error.to_string().contains("known activity differs"));
    assert_eq!(
        crate::store_error_value(&error)["error"]["code"],
        "task_sync_id_collision"
    );
    assert_eq!(state(&store), before);
    let mut other = original;
    other.project.project_id = ProjectId("different-project".into());
    let error = store
        .import_task_tree(
            &other.project.project_id,
            &actor("importer"),
            &other,
            false,
            at(9),
        )
        .unwrap_err();
    assert!(error.to_string().contains("known activity differs"));
    assert_eq!(
        crate::store_error_value(&error)["error"]["code"],
        "task_sync_id_collision"
    );
    assert_eq!(state(&store), before);
}

#[test]
fn task_sync_activity_absent_legacy_origin_bytes_and_all_rows_stay_unchanged() {
    let tree = candidate();
    let mut store = SqliteStore::open_in_memory().unwrap();
    import(&mut store, &tree);
    // Produce the actual old, field-absent canonical shape, not a new-field fixture.
    let origins = store.connection.prepare("SELECT object_id, canonical_json FROM objects WHERE object_kind = ?1 ORDER BY object_id").unwrap()
        .query_map([ORIGIN_KIND], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))).unwrap()
        .collect::<Result<Vec<_>, _>>().unwrap();
    assert_ne!(origins.len(), 0);
    for (_, bytes) in &origins {
        let value: serde_json::Value = serde_json::from_slice(bytes).unwrap();
        assert!(value["task"].get("activity").is_none());
    }
    let before = state(&store);
    for task in &tree.tasks {
        store.get_work_item(task.id).unwrap();
    }
    assert!(store.verify_all().unwrap().is_healthy());
    assert_eq!(
        export(&store, &tree.project.project_id, "local-machine")
            .documents()
            .unwrap(),
        tree.documents().unwrap()
    );
    assert!(
        !store
            .import_task_tree(
                &tree.project.project_id,
                &actor("importer"),
                &tree,
                false,
                at(9)
            )
            .unwrap()
            .changed
    );
    let after = store.connection.prepare("SELECT object_id, canonical_json FROM objects WHERE object_kind = ?1 ORDER BY object_id").unwrap()
        .query_map([ORIGIN_KIND], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?))).unwrap()
        .collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(after, origins);
    assert_eq!(state(&store), before);
}
