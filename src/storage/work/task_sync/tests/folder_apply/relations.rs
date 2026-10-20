use super::*;

fn blocker(detail: &str) -> SharedBlocker {
    SharedBlocker {
        blocker_id: uuid::Uuid::new_v4().to_string(),
        kind: WorkBlockerKind::Manual,
        detail: detail.into(),
        created_by: SharedActor::from_actor(&actor("reported-author")),
        created_at: at(1),
    }
}

fn roots(tasks: Vec<TaskDocument>) -> TaskTree {
    let mut tree = fixture().tree;
    tree.tasks = tasks;
    tree.tasks.sort_by_key(|task| task.id.0);
    tree
}

#[test]
fn folder_relations_compose_scalars_and_keep_exact_event_and_blocker_bindings() {
    let mut owner = document("owner", None);
    let old = document("old prerequisite", None);
    let new = document("new prerequisite", None);
    owner.prerequisites.push(old.id);
    let removed = blocker("original blocker");
    owner.blockers.push(removed.clone());
    let original = roots(vec![owner.clone(), old, new.clone()]);
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &original, "enrol", 5).unwrap();
    let origins = durable(&store);
    let added = blocker("replacement blocker");
    let resolved = edited(&original, owner.id, |task| {
        task.title = "edited owner".into();
        task.prerequisites = vec![new.id];
        task.blockers = vec![added.clone()];
    });
    exchange(&mut store, &resolved, "relations", 6).unwrap();
    let history = events(&store, owner.id);
    assert_eq!(history.len(), 5);
    assert!(matches!(
        history[0].transition,
        WorkTransition::PrerequisiteRemoved { .. }
    ));
    assert!(matches!(
        history[1].transition,
        WorkTransition::Unblocked { .. }
    ));
    assert!(matches!(
        history[2].transition,
        WorkTransition::Revised { .. }
    ));
    assert!(matches!(
        history[3].transition,
        WorkTransition::PrerequisiteAdded { .. }
    ));
    assert!(matches!(
        history[4].transition,
        WorkTransition::Blocked { .. }
    ));
    for pair in history.windows(2) {
        assert_eq!(pair[1].revision, pair[0].revision + 1);
    }
    for event in &history {
        assert!(event.run_id.is_none() && event.run.is_none());
        assert!(event.root_execution.is_none() && event.claim.is_none());
        assert!(event.handoff_offer.is_none());
        assert_eq!(event.actor.actor_id, "folder-importer");
    }
    let records =
        query::canonical_work_event_records_for_item(&store.connection, owner.id).unwrap();
    let clear: (String, String) = store
        .connection
        .query_row(
            "SELECT state, cleared_event_id FROM work_blockers WHERE blocker_id = ?1",
            [&removed.blocker_id],
            |row| Ok((row.get(0)?, row.get(1)?)),
        )
        .unwrap();
    assert_eq!(clear, ("cleared".into(), records[1].0.as_str().to_owned()));
    let creation: String = store
        .connection
        .query_row(
            "SELECT created_event_id FROM work_blockers WHERE blocker_id = ?1",
            [&added.blocker_id],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(creation, records[4].0.as_str());
    assert_eq!(
        history[4].blocker.as_ref().unwrap().created_at,
        added.created_at
    );
    assert_eq!(exported(&store), resolved);
    assert_eq!(durable(&store), origins);
    assert!(store.verify_all().unwrap().is_healthy());
    assert!(
        !exchange(&mut store, &resolved, "unchanged", 7)
            .unwrap()
            .changed
    );
    let scalar = edited(&resolved, owner.id, |task| task.priority = 3);
    exchange(&mut store, &scalar, "later-scalar", 8).unwrap();
    let cleared = edited(&scalar, owner.id, |task| task.blockers.clear());
    exchange(&mut store, &cleared, "later-relations", 9).unwrap();
    assert_eq!(exported(&store), cleared);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn folder_relations_remove_edges_before_reversing_them_or_inserting_new_tasks() {
    let mut a = document("a", None);
    let b = document("b", None);
    a.prerequisites.push(b.id);
    let initial = roots(vec![a.clone(), b.clone()]);
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &initial, "enrol", 5).unwrap();
    let reversed = edited(
        &edited(&initial, a.id, |task| task.prerequisites.clear()),
        b.id,
        |task| task.prerequisites = vec![a.id],
    );
    exchange(&mut store, &reversed, "reverse", 6).unwrap();
    assert_eq!(exported(&store), reversed);
    assert!(store.verify_all().unwrap().is_healthy());
    let mut added = document("added", None);
    added.prerequisites.push(a.id);
    let mut next = edited(&reversed, b.id, |task| task.prerequisites.clear());
    next.tasks.push(added.clone());
    next.tasks.sort_by_key(|task| task.id.0);
    exchange(&mut store, &next, "add-with-removal", 7).unwrap();
    assert_eq!(events(&store, added.id).len(), 0);
    assert_eq!(exported(&store), next);
}

#[test]
fn folder_relations_preserve_blocker_identity_and_refuse_rewrite_or_resurrection() {
    let mut owner = document("owner", None);
    let original_blocker = blocker("immutable detail");
    owner.blockers.push(original_blocker.clone());
    let initial = roots(vec![owner.clone()]);
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &initial, "enrol", 5).unwrap();
    let rewritten = edited(&initial, owner.id, |task| {
        task.blockers[0].detail = "rewritten".into();
    });
    let before = state(&store);
    assert!(
        conflict(&exchange(&mut store, &rewritten, "rewrite", 6).unwrap_err())
            .contains("immutable_blocker")
    );
    assert_eq!(state(&store), before);
    let cleared = edited(&initial, owner.id, |task| task.blockers.clear());
    exchange(&mut store, &cleared, "clear", 7).unwrap();
    let before = state(&store);
    assert!(
        conflict(&exchange(&mut store, &initial, "resurrect", 8).unwrap_err())
            .contains("id_collision")
    );
    assert_eq!(state(&store), before);
}

#[test]
fn folder_relations_distinguish_held_endpoint_from_changed_owner_and_completed_edge() {
    let owner = document("owner", None);
    let endpoint = document("held endpoint", None);
    let initial = roots(vec![owner.clone(), endpoint.clone()]);
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &initial, "enrol", 5).unwrap();
    claim_restored(&mut store, endpoint.id, "endpoint-worker", 6);
    let held = query::load_work_item(&store.connection, endpoint.id).unwrap();
    let current = exported(&store);
    let resolved = edited(&current, owner.id, |task| {
        task.prerequisites.push(endpoint.id);
    });
    exchange(&mut store, &resolved, "held-endpoint", 7).unwrap();
    assert_eq!(
        query::load_work_item(&store.connection, endpoint.id).unwrap(),
        held
    );
    let changed_owner = edited(&resolved, endpoint.id, |task| {
        task.blockers.push(blocker("blocked"));
    });
    let before = state(&store);
    assert!(
        conflict(&exchange(&mut store, &changed_owner, "held-owner", 8).unwrap_err())
            .contains("active_run")
    );
    assert_eq!(state(&store), before);

    let (mut store, fixture) = enrolled();
    let completed_edge = edited(&fixture.tree, fixture.first, |task| {
        task.prerequisites.push(fixture.completed);
    });
    let before = state(&store);
    assert!(
        conflict(&exchange(&mut store, &completed_edge, "completed-edge", 6).unwrap_err())
            .contains("already_satisfied")
    );
    assert_eq!(state(&store), before);
}

#[test]
fn folder_relations_refuse_affected_root_but_preserve_unrelated_scalar_changes() {
    let (mut store, fixture) = enrolled();
    claim_restored(&mut store, fixture.first, "worker", 6);
    let current = exported(&store);
    let changed = edited(&current, fixture.second, |task| {
        task.blockers.push(blocker("readiness"));
    });
    let before = state(&store);
    assert!(
        conflict(&exchange(&mut store, &changed, "bound-root", 7).unwrap_err())
            .contains("ancestor_binding")
    );
    assert_eq!(state(&store), before);
    let scalar = edited(&current, fixture.second, |task| {
        task.title = "scalar only".into();
    });
    exchange(&mut store, &scalar, "scalar-only", 8).unwrap();
}

#[test]
fn folder_relations_dry_run_late_failure_and_replay_after_local_work_are_atomic() {
    let (mut store, fixture) = enrolled();
    let resolved = edited(&fixture.tree, fixture.first, |task| {
        task.blockers.push(blocker("incoming"));
    });
    let selected_basis = basis(&store);
    let importer = actor("folder-importer");
    let project = project();
    let profile = profile();
    let request = TaskSyncFolderApply {
        project: &project,
        actor: &importer,
        tree: &resolved,
        export_basis: &selected_basis,
        profile: &profile,
        complete: true,
        now: at(6),
    };
    let before = state(&store);
    assert!(
        store
            .dry_run_task_sync_folder(request, "relations")
            .unwrap()
            .dry_run
    );
    assert_eq!(state(&store), before);
    store.connection.execute_batch("CREATE TEMP TRIGGER refuse_relation_result BEFORE INSERT ON work_operation_results WHEN NEW.operation = 'apply_task_sync_folder' BEGIN SELECT RAISE(ABORT, 'late failure'); END;").unwrap();
    assert!(store.apply_task_sync_folder(request, "relations").is_err());
    assert_eq!(state(&store), before);
    store
        .connection
        .execute_batch("DROP TRIGGER refuse_relation_result")
        .unwrap();
    let first = store.apply_task_sync_folder(request, "relations").unwrap();
    let later = edited(&resolved, fixture.second, |task| task.priority = 4);
    exchange(&mut store, &later, "later", 7).unwrap();
    let after = state(&store);
    assert_eq!(
        store.apply_task_sync_folder(request, "relations").unwrap(),
        first
    );
    assert_eq!(state(&store), after);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn folder_relations_allow_native_open_endpoint_but_never_a_native_owner() {
    let (mut store, fixture) = enrolled();
    let native = store
        .create_work(
            &root_request(PROJECT, "native", 6),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let current = exported(&store);
    let resolved = edited(&current, fixture.first, |task| {
        task.prerequisites.push(native.work_id);
    });
    exchange(&mut store, &resolved, "native-endpoint", 7).unwrap();
    assert_eq!(
        query::load_work_item(&store.connection, native.work_id).unwrap(),
        native
    );
    let changed = edited(&resolved, native.work_id, |task| {
        task.blockers.push(blocker("not an owner"));
    });
    let before = state(&store);
    assert!(
        conflict(&exchange(&mut store, &changed, "native-owner", 8).unwrap_err())
            .contains("unsupported_origin")
    );
    assert_eq!(state(&store), before);
}

#[test]
fn folder_relations_retained_completed_edge_is_not_readded_and_can_be_removed() {
    let mut fixture = fixture();
    fixture.tree = edited(&fixture.tree, fixture.first, |task| {
        task.prerequisites = vec![fixture.completed];
    });
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &fixture.tree, "enrol", 5).unwrap();
    let resolved = edited(&fixture.tree, fixture.first, |task| {
        task.blockers.push(blocker("blocked"));
    });
    exchange(&mut store, &resolved, "keep-completed", 6).unwrap();
    assert_eq!(events(&store, fixture.first).len(), 1);
    let removed = edited(&resolved, fixture.first, |task| task.prerequisites.clear());
    exchange(&mut store, &removed, "remove-completed", 7).unwrap();
    assert_eq!(exported(&store), removed);
    assert_eq!(events(&store, fixture.completed).len(), 0);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn folder_relations_bound_replacement_removes_before_adding_and_overflow_is_atomic() {
    let limit = crate::storage::work::MAX_WORK_PREREQUISITES_PER_ITEM;
    let endpoints = (0..=limit)
        .map(|index| document(&format!("endpoint {index}"), None))
        .collect::<Vec<_>>();
    let mut owner = document("bounded owner", None);
    owner.prerequisites = endpoints[..limit].iter().map(|task| task.id).collect();
    owner.prerequisites.sort_by_key(|id| id.0);
    let mut tasks = endpoints.clone();
    tasks.push(owner.clone());
    let initial = roots(tasks);
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &initial, "enrol", 5).unwrap();
    let overflow = edited(&initial, owner.id, |task| {
        task.prerequisites.push(endpoints[limit].id);
        task.prerequisites.sort_by_key(|id| id.0);
    });
    let before = state(&store);
    assert!(
        conflict(&exchange(&mut store, &overflow, "overflow", 6).unwrap_err())
            .contains("prerequisite_bound")
    );
    assert_eq!(state(&store), before);
    let replacement = edited(&overflow, owner.id, |task| {
        task.prerequisites.retain(|id| *id != endpoints[0].id);
    });
    exchange(&mut store, &replacement, "replace-at-bound", 7).unwrap();
    assert_eq!(exported(&store), replacement);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn folder_relations_preserve_all_durable_rows_through_reopen_and_projection_repair() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("folder-relations.db");
    let fixture = fixture();
    let resolved = edited(&fixture.tree, fixture.first, |task| {
        task.blockers.push(blocker("original attribution"));
    });
    let read_durable = |store: &SqliteStore| {
        state(store)
            .rows
            .into_iter()
            .filter(|(name, _)| {
                !super::super::super::super::is_rebuilt_projection_table(name)
                    && !name.starts_with("work_catalog_fts")
            })
            .collect::<Vec<_>>()
    };
    let before = {
        let mut store = SqliteStore::open(&database).unwrap();
        exchange(&mut store, &fixture.tree, "enrol", 5).unwrap();
        exchange(&mut store, &resolved, "relations", 6).unwrap();
        assert!(store.verify_all().unwrap().is_healthy());
        read_durable(&store)
    };
    assert!(
        SqliteStore::repair_rebuildable_projections(&database)
            .unwrap()
            .is_healthy()
    );
    let mut store = SqliteStore::open(&database).unwrap();
    assert_eq!(exported(&store), resolved);
    assert_eq!(read_durable(&store), before);
    let next = edited(&resolved, fixture.first, |task| {
        task.title = "after reopen".into();
    });
    exchange(&mut store, &next, "after-reopen", 7).unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn folder_relations_keep_original_origin_union_when_current_edges_are_removed() {
    let p = document("parent", None);
    let mut q = document("original dependent", None);
    q.prerequisites.push(p.id);
    let initial = roots(vec![p.clone(), q.clone()]);
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &initial, "enrol", 5).unwrap();
    let mut child = document("added child", Some(p.id));
    child.prerequisites.push(q.id);
    let mut resolved = edited(&initial, q.id, |task| task.prerequisites.clear());
    resolved.tasks.push(child.clone());
    resolved.tasks.sort_by_key(|task| task.id.0);
    let before = state(&store);
    let message = conflict(&exchange(&mut store, &resolved, "origin-cycle", 6).unwrap_err());
    assert!(
        message.contains(&format!("{}: origin_graph", child.id.0)),
        "{message}"
    );
    assert_eq!(state(&store), before);
}

#[test]
fn folder_relations_preserve_unchanged_local_blocker_attribution() {
    let (mut store, fixture) = enrolled();
    let item = query::load_work_item(&store.connection, fixture.first).unwrap();
    let recorded = store
        .add_work_blocker(
            &crate::domain::AddWorkBlockerRequest {
                work_id: fixture.first,
                expected_work_revision: item.revision,
                kind: WorkBlockerKind::Manual,
                detail: "local original".into(),
                authority: WorkPlanningAuthority::Project,
                actor: actor("local-author"),
                idempotency_key: "local-blocker".into(),
                blocked_at: at(6),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let current = exported(&store);
    let resolved = edited(&current, fixture.first, |task| {
        task.prerequisites.push(fixture.second);
    });
    exchange(&mut store, &resolved, "keep-original-blocker", 7).unwrap();
    let held = query::load_active_blocker_projections(&store.connection, fixture.first).unwrap();
    assert_eq!(held, vec![recorded]);
    assert_eq!(exported(&store), resolved);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn folder_relations_identity_refusal_does_not_overwrite_unsupported_field_precedence() {
    let mut owner = document("owner", None);
    owner.blockers.push(blocker("original"));
    let initial = roots(vec![owner.clone()]);
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &initial, "enrol", 5).unwrap();
    let resolved = edited(&initial, owner.id, |task| {
        task.blockers[0].detail = "rewrite".into();
        task.labels.push("unsupported".into());
    });
    let before = state(&store);
    let message = conflict(&exchange(&mut store, &resolved, "precedence", 6).unwrap_err());
    assert!(message.contains("unsupported_fields"), "{message}");
    assert!(!message.contains("immutable_blocker"), "{message}");
    assert_eq!(message.matches(&owner.id.0.to_string()).count(), 1);
    assert_eq!(state(&store), before);
}

#[test]
fn folder_relations_later_scalar_relation_and_disposal_survive_reopen() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("relations-disposal.sqlite");
    let owner = document("owner", None);
    let endpoint = document("endpoint", None);
    let initial = roots(vec![owner.clone(), endpoint.clone()]);
    let mut store = SqliteStore::open(&database).unwrap();
    exchange(&mut store, &initial, "enrol", 5).unwrap();
    let origins = durable(&store);
    let related = edited(&initial, owner.id, |task| {
        task.prerequisites.push(endpoint.id);
        task.blockers.push(blocker("reported obstacle"));
    });
    exchange(&mut store, &related, "relations", 6).unwrap();
    drop(store);
    let mut store = SqliteStore::open(&database).unwrap();
    let scalar = edited(&related, owner.id, |task| task.title = "later title".into());
    exchange(&mut store, &scalar, "scalar", 7).unwrap();
    let cleared = edited(&scalar, owner.id, |task| {
        task.prerequisites.clear();
        task.blockers.clear();
    });
    exchange(&mut store, &cleared, "clear-relations", 8).unwrap();
    drop(store);
    let mut store = SqliteStore::open(&database).unwrap();
    let removed = without(&cleared, &[owner.id]);
    exchange(&mut store, &removed, "dispose", 9).unwrap();
    assert_eq!(exported(&store), removed);
    assert_eq!(durable(&store), origins);
    let history = events(&store, owner.id);
    assert_eq!(history.len(), 6);
    assert!(matches!(
        history.last().unwrap().transition,
        WorkTransition::Disposed { .. }
    ));
    assert!(history.iter().all(|event| event.run_id.is_none()
        && event.run.is_none()
        && event.root_execution.is_none()
        && event.claim.is_none()
        && event.handoff_offer.is_none()));
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn folder_relations_explicit_edge_removal_allows_disposal_in_the_same_folder() {
    let mut survivor = document("survivor", None);
    let removed = document("removed", None);
    survivor.prerequisites.push(removed.id);
    let initial = roots(vec![survivor.clone(), removed.clone()]);
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &initial, "enrol", 5).unwrap();
    let selected = without(
        &edited(&initial, survivor.id, |task| {
            task.prerequisites.clear();
            task.blockers.push(blocker("new obstacle"));
        }),
        &[removed.id],
    );
    exchange(&mut store, &selected, "remove-edge-and-owner", 6).unwrap();
    assert_eq!(exported(&store), selected);
    assert!(matches!(events(&store, survivor.id)[0].transition,
        WorkTransition::PrerequisiteRemoved { prerequisite_id, .. } if prerequisite_id == removed.id));
    assert!(matches!(
        events(&store, removed.id)[0].transition,
        WorkTransition::Disposed { .. }
    ));
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn folder_relations_refuse_a_cycle_through_a_retired_required_child() {
    let parent = document("parent", None);
    let endpoint = document("endpoint", None);
    let mut retired = document("retired required child", Some(parent.id));
    retired.prerequisites.push(endpoint.id);
    let initial = roots(vec![parent.clone(), endpoint.clone(), retired.clone()]);
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &initial, "enrol", 5).unwrap();
    let current = without(&initial, &[retired.id]);
    exchange(&mut store, &current, "retire-child", 6).unwrap();
    // The exported graph and immutable origin union are both valid. The
    // durable graph retains parent -> retired -> endpoint, so this edge
    // would close its cycle even though the retired document is omitted.
    let resolved = edited(&current, endpoint.id, |task| {
        task.prerequisites.push(parent.id);
    });
    assert!(resolved.validate_stored().is_ok());
    let before = state(&store);
    let message = conflict(&exchange(&mut store, &resolved, "hidden-cycle", 7).unwrap_err());
    assert!(
        message.contains(&format!("{}: durable_graph", endpoint.id.0)),
        "{message}"
    );
    assert_eq!(state(&store), before);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn folder_relations_return_leg_keeps_shared_identity_without_local_runtime() {
    let owner = document("owner", None);
    let endpoint = document("endpoint", None);
    let initial = roots(vec![owner.clone(), endpoint.clone()]);
    let mut first = SqliteStore::open_in_memory().unwrap();
    let mut second = SqliteStore::open_in_memory().unwrap();
    exchange(&mut first, &initial, "enrol-first", 5).unwrap();
    exchange(&mut second, &initial, "enrol-second", 5).unwrap();
    let obstacle = blocker("shared obstacle");
    let outbound = edited(&initial, owner.id, |task| {
        task.prerequisites.push(endpoint.id);
        task.blockers.push(obstacle.clone());
    });
    exchange(&mut first, &outbound, "outbound-first", 6).unwrap();
    exchange(&mut second, &exported(&first), "outbound-second", 7).unwrap();
    let returned = edited(&outbound, owner.id, |task| {
        task.title = "resolved on second host".into();
        task.prerequisites.clear();
    });
    exchange(&mut second, &returned, "return-second", 8).unwrap();
    let original_blocker =
        query::load_active_blocker_projections(&first.connection, owner.id).unwrap();
    exchange(&mut first, &exported(&second), "return-first", 9).unwrap();
    assert_eq!(exported(&first), returned);
    assert_eq!(exported(&second), returned);
    assert_eq!(
        query::load_active_blocker_projections(&first.connection, owner.id).unwrap(),
        original_blocker
    );
    for store in [&first, &second] {
        let history = events(store, owner.id);
        assert_eq!(history.len(), 4);
        assert!(history.iter().all(|event| event.run_id.is_none()
            && event.run.is_none()
            && event.root_execution.is_none()
            && event.claim.is_none()
            && event.handoff_offer.is_none()));
        assert!(store.verify_all().unwrap().is_healthy());
    }
}

#[test]
fn folder_relations_refuse_same_folder_addition_below_a_changed_held_ancestor() {
    for immediate in [true, false] {
        let (mut store, fixture) = enrolled();
        let parent = if immediate {
            fixture.parent
        } else {
            fixture.second
        };
        // For the indirect case the changed owner is the grandparent.
        let ancestor = fixture.parent;
        let child = document("new descendant", Some(parent));
        let mut resolved = edited(&fixture.tree, ancestor, |task| {
            task.blockers.push(blocker("same-folder relation change"));
        });
        resolved.tasks.push(child.clone());
        resolved.tasks.sort_by_key(|task| task.id.0);
        let before = state(&store);
        let message =
            conflict(&exchange(&mut store, &resolved, "changed-ancestor", 6).unwrap_err());
        assert!(
            message.contains(&format!(
                "{}: ancestor_binding (ancestor {} has a relation change in this folder)",
                child.id.0, ancestor.0
            )),
            "{message}"
        );
        assert!(!message.contains(&format!("{}:", ancestor.0)), "{message}");
        assert_eq!(state(&store), before);
    }
}

#[test]
fn folder_relations_refuse_new_edges_to_cancelled_prerequisites() {
    let owner = document("owner", None);
    let mut endpoint = document("cancelled endpoint", None);
    endpoint.lifecycle = WorkLifecycle::Cancelled;
    endpoint.disposal_reason = Some("reported cancellation".into());
    let initial = roots(vec![owner.clone(), endpoint.clone()]);
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &initial, "enrol", 5).unwrap();
    let resolved = edited(&initial, owner.id, |task| {
        task.prerequisites.push(endpoint.id);
    });
    let before = state(&store);
    let message = conflict(&exchange(&mut store, &resolved, "cancelled-endpoint", 6).unwrap_err());
    assert!(
        message.contains(&format!(
            "{}: prerequisite_lifecycle (prerequisite {} is Cancelled)",
            owner.id.0, endpoint.id.0
        )),
        "{message}"
    );
    assert_eq!(state(&store), before);
}

fn defend_dependency_edge_after_shape_validation(ancestor: bool) {
    use crate::task_sync::folder_plan::{TaskDelta, plan};

    let (mut store, fixture) = enrolled();
    let target = if ancestor {
        fixture.parent
    } else {
        fixture.first
    };
    let safe = edited(&fixture.tree, fixture.first, |task| {
        task.prerequisites.push(fixture.completed);
    });
    let membership = fixture
        .tree
        .tasks
        .iter()
        .map(|task| task.id)
        .collect::<std::collections::HashSet<_>>();
    let mut planned = plan(&fixture.tree, &safe, &membership).unwrap();
    let resolved = edited(&fixture.tree, fixture.first, |task| {
        task.prerequisites.push(target);
    });
    let before = state(&store);
    // The public surface rejects this malformed graph before admission.
    let error = exchange(&mut store, &resolved, "invalid-edge", 6).unwrap_err();
    assert_eq!(refusal_code(&error), "task_sync_invalid_graph");
    assert_eq!(state(&store), before);

    // Exercise the transaction-internal defense independently of that
    // earlier shape check. The classified delta matches the invalid edge.
    for task in &mut planned.tasks {
        if task.id == fixture.first {
            let TaskDelta::Changed(change) = &mut task.delta else {
                panic!("expected a classified relation change")
            };
            change.relations.prerequisite_additions = vec![target];
        }
    }
    let transaction = store.begin_work_mutation().unwrap();
    let error = crate::storage::work::task_sync::folder_admission::apply_plan(
        &transaction,
        &planned,
        &fixture.tree,
        &resolved,
        &actor("folder-importer"),
        at(6),
    )
    .unwrap_err();
    drop(transaction);
    let message = conflict(&error);
    assert!(
        message.contains(&format!(
            "{}: dependency_cycle (prerequisite {} is the owner or its ancestor)",
            fixture.first.0, target.0
        )),
        "{message}"
    );
    assert_eq!(state(&store), before);
}

#[test]
fn folder_relations_defend_self_edges_after_shape_validation() {
    defend_dependency_edge_after_shape_validation(false);
}

#[test]
fn folder_relations_defend_ancestor_edges_after_shape_validation() {
    defend_dependency_edge_after_shape_validation(true);
}

#[test]
fn folder_relations_validate_cycles_through_retained_local_nonmembers() {
    let p = document("p", None);
    let q = document("q", None);
    let initial = roots(vec![p.clone(), q.clone()]);
    let mut store = SqliteStore::open_in_memory().unwrap();
    exchange(&mut store, &initial, "enrol", 5).unwrap();
    let parent = query::load_work_item(&store.connection, p.id).unwrap();
    let decomposition = store
        .decompose_work(
            &crate::domain::DecomposeWorkRequest {
                parent_id: p.id,
                expected_parent_revision: parent.revision,
                children: vec![crate::storage::work::test_support::child(
                    "local required child",
                    ChildRequirement::Required,
                    "local-child",
                )],
                prerequisites: Vec::new(),
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner"),
                idempotency_key: "decompose".into(),
                created_at: at(6),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let local =
        query::load_work_item(&store.connection, decomposition.children[0].work_id).unwrap();
    store
        .add_work_prerequisite(
            &crate::domain::ChangeWorkPrerequisiteRequest {
                work_id: local.work_id,
                prerequisite_id: q.id,
                expected_revision: local.revision,
                authority: WorkPlanningAuthority::Project,
                actor: actor("planner"),
                idempotency_key: "local-needs-q".into(),
                changed_at: at(7),
            },
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let resolved = edited(&initial, q.id, |task| task.prerequisites.push(p.id));
    resolved.validate_folder().unwrap();
    // R omits local H and is valid; C plus R contains P -> H -> Q -> P.
    let before = state(&store);
    let message = conflict(&exchange(&mut store, &resolved, "retained-cycle", 8).unwrap_err());
    assert!(
        message.contains(&format!("{}: retained_graph (", q.id.0)),
        "{message}"
    );
    assert!(!message.contains("durable_graph"), "{message}");
    assert_eq!(state(&store), before);
}

#[test]
fn folder_relations_apply_beside_a_retained_local_nonmember_without_enrolling_it() {
    let (mut store, fixture) = enrolled();
    let local = store
        .create_work(
            &root_request(PROJECT, "host-local", 6),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let local_events = events(&store, local.work_id);
    let resolved = edited(&fixture.tree, fixture.first, |task| {
        task.blockers.push(blocker("incoming obstacle"));
    });
    exchange(&mut store, &resolved, "relations-beside-local", 7).unwrap();
    let mut expected = resolved.clone();
    let held = exported(&store);
    expected.tasks.push(
        held.tasks
            .iter()
            .find(|task| task.id == local.work_id)
            .unwrap()
            .clone(),
    );
    expected.tasks.sort_by_key(|task| task.id.0);
    assert_eq!(held, expected);
    assert_eq!(
        query::load_work_item(&store.connection, local.work_id).unwrap(),
        local
    );
    assert_eq!(events(&store, local.work_id), local_events);
    // A later omission is still a retained nonmember, not a deletion.
    assert!(
        !exchange(&mut store, &resolved, "still-omitted", 8)
            .unwrap()
            .changed
    );
    assert_eq!(exported(&store), expected);
    assert!(store.verify_all().unwrap().is_healthy());
}
