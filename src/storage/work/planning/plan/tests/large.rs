use super::*;
use crate::storage::work::integrity::take_graph_scan_metrics;
use crate::storage::work::planning::take_descendant_scan_count;

#[test]
fn atomic_plan_admission_decodes_stay_bounded_for_sparse_and_dense_plans() {
    for edges in [0, 252, MAX_WORK_PLAN_EDGES] {
        let mut request = maximum_forest();
        if edges == 252 {
            request.plan.prerequisites = (1..128)
                .chain(130..255)
                .map(|from| WorkPlanPrerequisite {
                    work_key: format!("task{from}"),
                    prerequisite: WorkPlanDependency::Local(format!("task{}", from + 1)),
                })
                .collect();
        } else {
            request.plan.prerequisites.truncate(edges);
        }
        let mut store = SqliteStore::open_in_memory().expect("store");
        assert_eq!(request.plan.prerequisites.len(), edges);
        crate::canonical::reset_canonical_decode_count();
        let start = std::time::Instant::now();
        store
            .propose_work_plan(&request, &DevelopmentNoopRedactor)
            .expect("admit");
        let elapsed = start.elapsed();
        assert!(
            crate::canonical::canonical_decode_count() < 20_000,
            "one admission must not re-decode every edge prefix"
        );
        eprintln!(
            "profile: background0, tasks256, edges{edges}, elapsed{elapsed:?}, decodes{}",
            crate::canonical::canonical_decode_count()
        );
    }
}

#[test]
fn atomic_plan_final_relation_audit_refuses_corrupt_edge_proofs_and_projection() {
    for corruption in [
        "UPDATE work_prerequisites SET event_hash = (SELECT latest_event_hash FROM work_items WHERE work_id = NEW.prerequisite_id) WHERE work_id = NEW.work_id AND prerequisite_id = NEW.prerequisite_id;",
        "DELETE FROM work_prerequisites WHERE work_id = NEW.work_id AND prerequisite_id = NEW.prerequisite_id;",
    ] {
        let mut store = SqliteStore::open_in_memory().expect("store");
        store.connection.execute_batch(&format!("CREATE TEMP TRIGGER damage_plan_edge AFTER INSERT ON work_prerequisites BEGIN {corruption} END;")).expect("inject corruption");
        let before = test_database_shape_snapshot(&store.connection).expect("before");
        let mut input = large_request(80);
        input.plan.prerequisites.push(WorkPlanPrerequisite {
            work_key: "task0".into(),
            prerequisite: WorkPlanDependency::Local("task1".into()),
        });
        assert!(matches!(
            store.propose_work_plan(&input, &DevelopmentNoopRedactor),
            Err(StoreError::InvalidWorkProjection(_))
        ));
        assert_eq!(
            test_database_shape_snapshot(&store.connection).expect("after"),
            before
        );
    }
}

#[test]
fn atomic_plan_relation_fingerprints_match_immediate_transitions() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let receipt = store
        .propose_work_plan(&large_request(3), &DevelopmentNoopRedactor)
        .expect("seed");
    let transaction = store.begin_work_mutation().expect("transaction");
    let work_id = receipt.tasks[0].work_id;
    let mut fingerprints = Vec::new();
    for planned in [false, true] {
        transaction
            .execute_batch("SAVEPOINT compare_edges")
            .expect("savepoint");
        let mut basis = PlanRelationBasis {
            work_id,
            basis: validated_current_work_relation_basis(&transaction, work_id).expect("basis"),
        };
        let mut actual = Vec::new();
        for target in &receipt.tasks[1..] {
            let item = load_work_item(&transaction, work_id).expect("item");
            change_work_prerequisite_with_validation_on(
                &transaction,
                &ChangeWorkPrerequisiteRequest {
                    work_id,
                    prerequisite_id: target.work_id,
                    expected_revision: item.revision,
                    authority: WorkPlanningAuthority::Project,
                    actor: actor("planner"),
                    idempotency_key: String::new(),
                    changed_at: at(1),
                },
                true,
                if planned {
                    PlanningValidation::AtomicPlan
                } else {
                    PlanningValidation::Immediate
                },
                planned.then_some(&mut basis),
            )
            .expect("edge");
            let event = crate::storage::work::query::latest_canonical_work_event_for_item(
                &transaction,
                work_id,
            )
            .expect("event");
            actual.push(event.relation_fingerprint);
        }
        require_work_item_relation_integrity(&transaction, work_id).expect("final audit");
        fingerprints.push(actual);
        transaction
            .execute_batch("ROLLBACK TO compare_edges; RELEASE compare_edges")
            .expect("same IDs and initial state");
    }
    assert_eq!(fingerprints[0], fingerprints[1]);
}

#[test]
fn atomic_plan_existing_prerequisite_retains_canonical_relation_validation() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let mut seed = large_request(3);
    seed.plan.prerequisites.push(WorkPlanPrerequisite {
        work_key: "task0".into(),
        prerequisite: WorkPlanDependency::Local("task1".into()),
    });
    let existing = store
        .propose_work_plan(&seed, &DevelopmentNoopRedactor)
        .expect("existing graph");
    let mut input = large_request(80);
    input.plan.idempotency_key = "healthy-existing".into();
    input.plan.prerequisites.push(WorkPlanPrerequisite {
        work_key: "task0".into(),
        prerequisite: WorkPlanDependency::Existing(existing.tasks[0].short_ref.clone()),
    });
    store
        .propose_work_plan(&input, &DevelopmentNoopRedactor)
        .expect("healthy external prerequisite");
    store.connection.execute("UPDATE work_prerequisites SET event_hash = (SELECT latest_event_hash FROM work_items WHERE work_id = ?1) WHERE work_id = ?2", rusqlite::params![existing.tasks[2].work_id.0.to_string(), existing.tasks[0].work_id.0.to_string()]).expect("corrupt existing edge proof");
    let before = test_database_shape_snapshot(&store.connection).expect("before");
    input.plan.idempotency_key = "corrupt-existing".into();
    assert!(matches!(
        store.propose_work_plan(&input, &DevelopmentNoopRedactor),
        Err(StoreError::InvalidWorkProjection(_))
    ));
    assert_eq!(
        test_database_shape_snapshot(&store.connection).expect("after"),
        before
    );
}

fn large_request(count: usize) -> ProposeWorkPlanRequest {
    let mut input = request();
    input.plan.tasks = (0..count)
        .map(|index| {
            let mut task = task(&format!("task{index}"), None);
            task.notes.clear();
            task
        })
        .collect();
    input.plan.prerequisites.clear();
    input
}

fn maximum_forest() -> ProposeWorkPlanRequest {
    forest_with_edges(MAX_WORK_PLAN_EDGES)
}

pub(super) fn forest_with_edges(edges: usize) -> ProposeWorkPlanRequest {
    let mut input = large_request(MAX_WORK_PLAN_TASKS);
    // Preserve the former 128-descendant boundary as an interior two-tree
    // fixture. It now covers root-count/background scans, not the current cap.
    for index in 1..=128 {
        input.plan.tasks[index].parent_key = Some("task0".into());
    }
    for index in 130..MAX_WORK_PLAN_TASKS {
        input.plan.tasks[index].parent_key = Some("task129".into());
    }
    input.plan.prerequisites = (1..=128)
        .flat_map(|from| {
            (from + 1..=128).map(move |to| WorkPlanPrerequisite {
                work_key: format!("task{from}"),
                prerequisite: WorkPlanDependency::Local(format!("task{to}")),
            })
        })
        .take(edges)
        .collect();
    assert_eq!(input.plan.prerequisites.len(), edges);
    input
}

#[test]
fn atomic_plan_large_forest_scans_the_project_once_and_preserves_all_edges() {
    for background in [256, 768] {
        let mut store = SqliteStore::open_in_memory().expect("store");
        for batch in 0..background / MAX_WORK_PLAN_TASKS {
            let mut seed = large_request(MAX_WORK_PLAN_TASKS);
            seed.plan.idempotency_key = format!("background-{batch}");
            store
                .propose_work_plan(&seed, &DevelopmentNoopRedactor)
                .expect("seed");
        }
        let request = maximum_forest();
        take_graph_scan_metrics();
        take_descendant_scan_count();
        let started = std::time::Instant::now();
        let receipt = store
            .propose_work_plan(&request, &DevelopmentNoopRedactor)
            .expect("large forest");
        let elapsed = started.elapsed();
        let measured = take_graph_scan_metrics();
        assert_eq!(take_descendant_scan_count(), 2);
        assert_eq!(
            measured,
            (1, background + MAX_WORK_PLAN_TASKS + MAX_WORK_PLAN_EDGES)
        );
        assert_eq!(receipt.tasks.len(), MAX_WORK_PLAN_TASKS);
        for (input, mapping) in request.plan.tasks.iter().zip(&receipt.tasks) {
            assert_eq!(input.key, mapping.key);
            let item = load_work_item(&store.connection, mapping.work_id).expect("item");
            assert_eq!(item.revision, mapping.revision);
            let expected_parent = input.parent_key.as_ref().map(|key| {
                receipt
                    .tasks
                    .iter()
                    .find(|row| row.key == *key)
                    .expect("parent")
                    .work_id
            });
            assert_eq!(item.parent_id, expected_parent);
            let mut expected_edges = request
                .plan
                .prerequisites
                .iter()
                .filter(|edge| edge.work_key == input.key)
                .map(|edge| {
                    let WorkPlanDependency::Local(key) = &edge.prerequisite else {
                        panic!("local");
                    };
                    receipt
                        .tasks
                        .iter()
                        .find(|row| row.key == *key)
                        .expect("target")
                        .work_id
                })
                .collect::<Vec<_>>();
            expected_edges.sort_by_key(|id| id.0);
            assert_eq!(
                load_prerequisite_projection_ids(&store.connection, item.work_id).expect("edges"),
                expected_edges
            );
        }
        eprintln!(
            "atomic plan: background={background}, tasks={}, edges={}, scans={}, rows={}, admission_ms={:.1}",
            MAX_WORK_PLAN_TASKS,
            MAX_WORK_PLAN_EDGES,
            measured.0,
            measured.1,
            elapsed.as_secs_f64() * 1000.0
        );
        assert!(store.verify_all().expect("doctor").is_healthy());
    }
}

#[test]
fn atomic_plan_nested_forest_checks_each_root_size_once() {
    let mut input = large_request(128);
    // Two four-level trees, each with several decomposed parents.
    for index in 1..64 {
        input.plan.tasks[index].parent_key = Some(format!("task{}", (index - 1) / 4));
    }
    for index in 65..128 {
        input.plan.tasks[index].parent_key = Some(format!("task{}", 64 + (index - 65) / 4));
    }
    let mut store = SqliteStore::open_in_memory().expect("store");
    take_descendant_scan_count();
    take_graph_scan_metrics();
    let result = store
        .propose_work_plan(&input, &DevelopmentNoopRedactor)
        .expect("nested plan");
    assert_eq!(result.tasks.len(), 128);
    assert_eq!(take_descendant_scan_count(), 2);
    assert_eq!(take_graph_scan_metrics(), (1, 128));
    assert!(store.verify_all().expect("doctor").is_healthy());
}

#[test]
fn atomic_plan_typed_byte_limit_admits_exact_boundary_and_refuses_one_more() {
    let mut input = large_request(1);
    let size = serde_json::to_vec(&input.plan).expect("size").len();
    input.plan.tasks[0]
        .outcome
        .push_str(&"x".repeat(MAX_WORK_PLAN_BYTES - size));
    assert_eq!(
        serde_json::to_vec(&input.plan).expect("size").len(),
        MAX_WORK_PLAN_BYTES
    );
    validate_work_plan(&input.plan).expect("exact limit");
    input.plan.tasks[0].outcome.push('x');
    assert!(
        matches!(validate_work_plan(&input.plan), Err(StoreError::InvalidWork(reason)) if reason.contains("serialized plan exceeds"))
    );
}

#[test]
fn atomic_plan_large_refusals_preserve_the_entire_store() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let before = test_database_shape_snapshot(&store.connection).expect("before");
    for case in 0..4 {
        let mut input = maximum_forest();
        match case {
            0 => input.plan.tasks.push(task("too-many", None)),
            1 => input = forest_with_edges(MAX_WORK_PLAN_EDGES + 1),
            2 => input.plan.tasks[0].outcome = "x".repeat(MAX_WORK_PLAN_BYTES),
            3 => {
                input.plan.prerequisites[0] = WorkPlanPrerequisite {
                    work_key: "task128".into(),
                    prerequisite: WorkPlanDependency::Local("task1".into()), // cycle through other edges
                }
            }
            _ => unreachable!(),
        }
        take_graph_scan_metrics();
        let error = store
            .propose_work_plan(&input, &DevelopmentNoopRedactor)
            .expect_err("invalid large plan");
        match case {
            0 => assert!(matches!(&error, StoreError::InvalidWork(reason)
                if reason == &format!("plan: expected 1 through {MAX_WORK_PLAN_TASKS} tasks"))),
            1 => assert!(matches!(&error, StoreError::InvalidWork(reason)
                if reason == &format!("plan: at most {MAX_WORK_PLAN_EDGES} prerequisite edges are allowed"))),
            2 => assert!(matches!(&error, StoreError::InvalidWork(reason)
                if reason == &format!("plan: serialized plan exceeds {MAX_WORK_PLAN_BYTES} bytes"))),
            3 => assert!(matches!(error, StoreError::WorkDependencyCycle)),
            _ => unreachable!(),
        }
        assert_eq!(
            take_graph_scan_metrics(),
            (0, 0),
            "refuse before admission: case {case}"
        );
        assert_eq!(
            test_database_shape_snapshot(&store.connection).expect("after"),
            before,
            "case {case}"
        );
    }
    let error = store
        .propose_work_plan_with_admission(&maximum_forest(), &DevelopmentNoopRedactor, |receipt| {
            assert_eq!(receipt.tasks.len(), MAX_WORK_PLAN_TASKS);
            Err(StoreError::InvalidWork(
                "reject complete large receipt".into(),
            ))
        })
        .expect_err("late refusal");
    assert!(
        matches!(error, StoreError::InvalidWork(reason) if reason == "reject complete large receipt")
    );
    assert_eq!(
        test_database_shape_snapshot(&store.connection).expect("after receipt refusal"),
        before
    );
}

#[test]
fn atomic_plan_descendant_refusal_names_a_later_payload_root() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let roots = store
        .propose_work_plan(
            &large_request(MAX_WORK_PLAN_TASKS),
            &DevelopmentNoopRedactor,
        )
        .expect("roots");
    let extra = store
        .create_work(
            &root_request("atomic-plan", "extra", 1),
            &DevelopmentNoopRedactor,
        )
        .expect("extra root");
    let before = test_database_shape_snapshot(&store.connection).expect("before");
    // The valid 256-task payload cannot exceed 255 descendants anymore. Exercise
    // the defensive root-count guard on an injected projection, not by claiming
    // a now-valid 129-descendant plan must refuse. Roll back each injected shape.
    for index in [0, 129] {
        let root = &roots.tasks[index];
        let tx = store.begin_work_mutation().expect("transaction");
        tx.execute("UPDATE work_items SET parent_id = ?1, root_id = ?1 WHERE work_id != ?1 AND work_id != ?2", rusqlite::params![root.work_id.0.to_string(), extra.work_id.0.to_string()]).expect("boundary projection");
        validate_plan_root_budget(&tx, root).expect("255 descendants");
        tx.execute(
            "UPDATE work_items SET parent_id = ?1, root_id = ?1 WHERE work_id = ?2",
            rusqlite::params![root.work_id.0.to_string(), extra.work_id.0.to_string()],
        )
        .expect("one beyond boundary");
        let error = validate_plan_root_budget(&tx, root).expect_err("oversized root");
        assert!(matches!(error, StoreError::InvalidWork(reason)
            if reason == format!("plan: root '{}' has 256 open descendants; at most 255 are allowed (256 tasks including the root)", root.key)));
        drop(tx);
        assert_eq!(
            test_database_shape_snapshot(&store.connection).expect("after"),
            before
        );
    }
}

#[test]
fn atomic_plan_final_union_check_rejects_an_existing_unrelated_cycle() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let old = store
        .propose_work_plan(&large_request(2), &DevelopmentNoopRedactor)
        .expect("seed");
    // Deliberately damage an unrelated projection. The final whole-project scan
    // must run even for a new forest whose payload-local topology is valid.
    store
        .connection
        .execute(
            "UPDATE work_items SET superseded_by = work_id WHERE work_id = ?1",
            [old.tasks[0].work_id.0.to_string()],
        )
        .expect("cycle");
    let before = test_database_shape_snapshot(&store.connection).expect("before");
    let mut input = large_request(80);
    input.plan.idempotency_key = "new-forest".into();
    take_graph_scan_metrics();
    assert!(matches!(
        store.propose_work_plan(&input, &DevelopmentNoopRedactor),
        Err(StoreError::WorkDependencyCycle)
    ));
    assert_eq!(take_graph_scan_metrics(), (1, 82));
    assert_eq!(
        test_database_shape_snapshot(&store.connection).expect("after"),
        before
    );
}

#[test]
fn atomic_plan_payload_cycle_check_handles_long_paths_and_parallel_edges() {
    let mut adjacency = (0..256)
        .map(|index| {
            if index == 255 {
                vec![]
            } else {
                vec![index + 1, index + 1]
            }
        })
        .collect::<Vec<_>>();
    assert!(plan_graph_is_acyclic(&adjacency));
    adjacency[255].push(0);
    assert!(!plan_graph_is_acyclic(&adjacency));
}
