use super::*;
use crate::domain::{
    DisposeWorkRequest, WorkDisposition, WorkItemKind, WorkPlanPrerequisite, WorkPlanTask,
};
use crate::storage::work::query::{load_prerequisite_projection_ids, load_work_item};
use crate::storage::work::test_support::*;

mod guards;
mod large;
mod single_root;

fn task(key: &str, parent: Option<&str>) -> WorkPlanTask {
    WorkPlanTask {
        key: key.into(),
        parent_key: parent.map(str::to_owned),
        title: format!("Task {key}"),
        outcome: format!("Deliver {key}"),
        acceptance: vec![format!("{key} delivered")],
        requirement: None,
        kind: None,
        priority: None,
        labels: Vec::new(),
        assigned_to: None,
        deferred_until: None,
        external_ref: None,
        notes: vec![format!("Note {key}")],
    }
}

fn request() -> ProposeWorkPlanRequest {
    ProposeWorkPlanRequest {
        project_id: crate::ProjectId("atomic-plan".into()),
        actor: actor("planner"),
        created_at: at(0),
        plan: WorkPlanInput {
            idempotency_key: "plan-one".into(),
            tasks: vec![
                task("leaf", Some("child")),
                task("root", None),
                task("child", Some("root")),
                task("other", None),
            ],
            prerequisites: vec![WorkPlanPrerequisite {
                work_key: "leaf".into(),
                prerequisite: WorkPlanDependency::Local("other".into()),
            }],
        },
    }
}

#[test]
fn atomic_plan_two_levels_preserve_map_and_replay_without_writes() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let mut request = request();
    let receipt = store
        .propose_work_plan(&request, &DevelopmentNoopRedactor)
        .expect("plan");
    assert_eq!(
        receipt
            .tasks
            .iter()
            .map(|item| item.key.as_str())
            .collect::<Vec<_>>(),
        vec!["leaf", "root", "child", "other"]
    );
    let items = receipt
        .tasks
        .iter()
        .map(|mapping| load_work_item(&store.connection, mapping.work_id).expect("item"))
        .collect::<Vec<_>>();
    assert_eq!(items[0].parent_id, Some(items[2].work_id));
    assert_eq!(items[2].parent_id, Some(items[1].work_id));
    assert_eq!(items[0].root_id, items[1].work_id);
    assert_eq!(
        load_prerequisite_projection_ids(&store.connection, items[0].work_id).expect("edges"),
        vec![items[3].work_id]
    );
    assert!(
        items
            .iter()
            .all(|item| item.lifecycle == WorkLifecycle::Open && item.origin == WorkOrigin::Local)
    );
    assert!(store.verify_all().expect("doctor").is_healthy());
    let before = test_database_shape_snapshot(&store.connection).expect("before");
    request.created_at = at(20);
    assert_eq!(
        store
            .propose_work_plan(&request, &DevelopmentNoopRedactor)
            .expect("retry"),
        receipt
    );
    assert_eq!(
        test_database_shape_snapshot(&store.connection).expect("after"),
        before
    );
    request.plan.tasks[0].title.push_str(" changed");
    assert!(matches!(
        store.propose_work_plan(&request, &DevelopmentNoopRedactor),
        Err(StoreError::WorkOperationIdempotencyConflict { .. })
    ));
    assert_eq!(
        test_database_shape_snapshot(&store.connection).expect("conflict"),
        before
    );
}

#[test]
fn atomic_plan_invalid_graphs_and_limits_leave_store_unchanged() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let before = test_database_shape_snapshot(&store.connection).expect("before");
    for case in 0..16 {
        let mut request = request();
        match case {
            0 => request.plan.tasks[0].key = "root".into(),
            1 => request.plan.tasks[0].parent_key = Some("missing".into()),
            2 => request.plan.tasks[1].parent_key = Some("leaf".into()),
            3 => request.plan.tasks[0].parent_key = Some("leaf".into()),
            4 => {
                request.plan.prerequisites[0].prerequisite =
                    WorkPlanDependency::Local("missing".into());
            }
            5 => {
                request.plan.prerequisites[0].prerequisite =
                    WorkPlanDependency::Local("root".into());
            }
            6 => request
                .plan
                .tasks
                .extend((0..MAX_WORK_PLAN_TASKS).map(|n| task(&format!("extra{n}"), None))),
            7 => request.plan.tasks[0].title = "x".repeat(MAX_WORK_PLAN_BYTES),
            8 => {
                request = large::forest_with_edges(MAX_WORK_PLAN_EDGES + 1);
            }
            9 => request.plan.tasks[0].acceptance = vec![" ".into()],
            10 => request
                .plan
                .prerequisites
                .push(request.plan.prerequisites[0].clone()),
            11 => request.plan.prerequisites.push(WorkPlanPrerequisite {
                work_key: "other".into(),
                prerequisite: WorkPlanDependency::Local("root".into()),
            }),
            12 => request.plan.tasks[0].key = "space key".into(),
            13 => request.plan.tasks[0].labels = vec!["label".into(); 65],
            14 => request.plan.tasks[0].notes = vec!["note".into(); 17],
            15 => {
                request.plan.tasks = (0..6)
                    .map(|index| task(&format!("depth{index}"), None))
                    .collect();
                for index in 1..6 {
                    request.plan.tasks[index].parent_key = Some(format!("depth{}", index - 1));
                }
                request.plan.prerequisites.clear();
            }
            _ => unreachable!(),
        }
        let result = store.propose_work_plan(&request, &DevelopmentNoopRedactor);
        if case == 8 {
            assert!(matches!(&result, Err(StoreError::InvalidWork(reason))
                if reason == &format!("plan: at most {MAX_WORK_PLAN_EDGES} prerequisite edges are allowed")));
        }
        assert!(
            matches!(
                result,
                Err(StoreError::InvalidWork(_) | StoreError::WorkDependencyCycle)
            ),
            "case {case}"
        );
        assert_eq!(
            test_database_shape_snapshot(&store.connection).expect("after"),
            before,
            "case {case}"
        );
    }
}

#[test]
fn atomic_plan_protocol_validator_checks_every_mapping_including_grandchildren() {
    use crate::storage::work::feeds::validate_work_protocol_result_binding;
    let mut store = SqliteStore::open_in_memory().expect("store");
    let receipt = store
        .propose_work_plan(&request(), &DevelopmentNoopRedactor)
        .expect("plan");
    let foreign = store
        .create_work(
            &root_request("other-project", "foreign", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("foreign");
    let mut value = serde_json::to_value(&receipt).expect("json");
    value["kind"] = serde_json::json!("plan");
    validate_work_protocol_result_binding(
        &store.connection,
        "atomic-plan",
        "work_propose:plan",
        &value,
    )
    .expect("all valid");
    // Replace each row, including the out-of-order grandchild, with an intact
    // identity from another project. Skipping any row makes its case pass.
    for index in 0..receipt.tasks.len() {
        let mut altered = value.clone();
        altered["tasks"][index]["work_id"] = serde_json::json!(foreign.work_id);
        altered["tasks"][index]["short_ref"] = serde_json::json!(foreign.short_ref);
        altered["tasks"][index]["revision"] = serde_json::json!(foreign.revision);
        assert!(
            matches!(validate_work_protocol_result_binding(&store.connection, "atomic-plan", "work_propose:plan", &altered),
            Err(StoreError::InvalidWorkProjection(reason)) if reason.contains("crosses its project binding")),
            "mapping {index}"
        );
    }
}

#[test]
fn atomic_plan_late_redactor_and_receipt_refusals_roll_back_every_write() {
    struct RejectNote;
    impl Redactor for RejectNote {
        fn inspect(&self, text: &str) -> Result<(), String> {
            if text.contains("work_observation:non_holder") {
                Err("reject generated note".into())
            } else {
                Ok(())
            }
        }
        fn description(&self) -> &'static str {
            "generated-note rejection"
        }
    }
    let mut store = SqliteStore::open_in_memory().expect("store");
    let before = test_database_shape_snapshot(&store.connection).expect("before");
    let result =
        store.propose_work_plan_with_admission(&request(), &DevelopmentNoopRedactor, |receipt| {
            assert_eq!(receipt.tasks.len(), 4);
            Err(StoreError::InvalidWork("test receipt byte limit".into()))
        });
    assert!(
        matches!(result, Err(StoreError::InvalidWork(reason)) if reason == "test receipt byte limit")
    );
    assert_eq!(
        test_database_shape_snapshot(&store.connection).expect("after budget"),
        before
    );
    let rejected = store.propose_work_plan(&request(), &RejectingRedactor);
    assert!(matches!(rejected, Err(StoreError::RedactionRefused(_))));
    assert_eq!(
        test_database_shape_snapshot(&store.connection).expect("after redactor"),
        before
    );
    let rejected = store.propose_work_plan(&request(), &RejectNote);
    assert!(
        matches!(rejected, Err(StoreError::RedactionRefused(reason)) if reason == "reject generated note")
    );
    assert_eq!(
        test_database_shape_snapshot(&store.connection).expect("after late refusal"),
        before
    );
}

#[test]
fn atomic_plan_existing_prerequisite_is_resolved_without_mutating_it() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let existing = store
        .create_work(
            &root_request("atomic-plan", "existing", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("existing");
    let mut request = request();
    request.plan.prerequisites[0].prerequisite =
        WorkPlanDependency::Existing(existing.short_ref.clone());
    let receipt = store
        .propose_work_plan(&request, &DevelopmentNoopRedactor)
        .expect("plan");
    assert_eq!(
        load_work_item(&store.connection, existing.work_id).expect("existing after"),
        existing
    );
    assert_eq!(
        load_prerequisite_projection_ids(&store.connection, receipt.tasks[0].work_id)
            .expect("edge"),
        vec![existing.work_id]
    );
    assert!(store.verify_all().expect("doctor").is_healthy());
}

#[test]
fn atomic_plan_optional_root_refuses_without_writes() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let mut request = request();
    request.plan.tasks[1].requirement = Some(ChildRequirement::Optional);
    assert!(request.plan.tasks[1].parent_key.is_none());
    let before = test_database_shape_snapshot(&store.connection).expect("before");
    let error = store
        .propose_work_plan(&request, &DevelopmentNoopRedactor)
        .expect_err("optional root");
    assert!(matches!(error, StoreError::InvalidWork(reason)
        if reason == "plan: an optional child requirement needs a parent key"));
    assert_eq!(
        test_database_shape_snapshot(&store.connection).expect("after"),
        before
    );
    // The requirement is valid on a child: this is not a blanket optional ban.
    request.plan.tasks[1].requirement = None;
    request.plan.tasks[0].requirement = Some(ChildRequirement::Optional);
    store
        .propose_work_plan(&request, &DevelopmentNoopRedactor)
        .expect("optional child");
    assert!(store.verify_all().expect("doctor").is_healthy());
}

#[test]
fn atomic_plan_terminal_existing_prerequisites_refuse_without_writes() {
    for lifecycle in [
        WorkLifecycle::Completed,
        WorkLifecycle::Cancelled,
        WorkLifecycle::Superseded,
    ] {
        let mut store = SqliteStore::open_in_memory().expect("store");
        let existing = store
            .create_work(
                &root_request("atomic-plan", "existing", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("existing");
        if lifecycle == WorkLifecycle::Completed {
            let held = claim(&mut store, &existing, "planner", "claim", 1, 3600);
            let current = store.get_work_item(existing.work_id).expect("claimed item");
            let proof = evidence(&mut store, &current, &held, "planner", "proof", 2);
            checkpoint(
                &mut store,
                &current,
                &held,
                "planner",
                "checkpoint",
                3,
                std::slice::from_ref(&proof),
            );
            complete(
                &mut store, &current, &held, "planner", &proof, "complete", 4,
            )
            .expect("complete prerequisite");
        } else {
            let replacement_id = if lifecycle == WorkLifecycle::Superseded {
                Some(
                    store
                        .create_work(
                            &root_request("atomic-plan", "replacement", 1),
                            &DevelopmentNoopRedactor,
                        )
                        .expect("replacement")
                        .work_id,
                )
            } else {
                None
            };
            store
                .dispose_work(
                    &DisposeWorkRequest {
                        work_id: existing.work_id,
                        expected_work_revision: existing.revision,
                        disposition: if replacement_id.is_some() {
                            WorkDisposition::Superseded
                        } else {
                            WorkDisposition::Cancelled
                        },
                        replacement_id,
                        reason: "retire prerequisite".into(),
                        actor: actor("planner"),
                        idempotency_key: "dispose".into(),
                        disposed_at: at(4),
                    },
                    &DevelopmentNoopRedactor,
                )
                .expect("dispose prerequisite");
        }
        assert_eq!(
            store
                .get_work_item(existing.work_id)
                .expect("terminal item")
                .lifecycle,
            lifecycle
        );
        assert!(store.verify_all().expect("healthy fixture").is_healthy());
        let mut request = request();
        request.created_at = at(5);
        request.plan.prerequisites[0].prerequisite =
            WorkPlanDependency::Existing(existing.short_ref);
        let before = test_database_shape_snapshot(&store.connection).expect("before");
        let error = store
            .propose_work_plan(&request, &DevelopmentNoopRedactor)
            .expect_err("terminal prerequisite");
        if lifecycle == WorkLifecycle::Completed {
            assert!(
                matches!(error, StoreError::WorkPrerequisiteAlreadySatisfied(id) if id == existing.work_id)
            );
        } else {
            assert!(matches!(error, StoreError::WorkNotOpen(id) if id == existing.work_id));
        }
        assert_eq!(
            test_database_shape_snapshot(&store.connection).expect("after"),
            before,
            "{lifecycle:?}"
        );
    }
}

#[test]
fn atomic_plan_resolved_prerequisite_aliases_refuse_without_writes() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let existing = store
        .create_work(
            &root_request("atomic-plan", "existing", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("existing");
    let mut request = request();
    request.plan.prerequisites = [existing.short_ref.clone(), existing.work_id.0.to_string()]
        .into_iter()
        .map(|reference| WorkPlanPrerequisite {
            work_key: "leaf".into(),
            prerequisite: WorkPlanDependency::Existing(reference),
        })
        .collect();
    // Different spellings pass payload validation; only resolved identity is duplicated.
    validate_work_plan(&request.plan).expect("distinct input spellings");
    let before = test_database_shape_snapshot(&store.connection).expect("before");
    let error = store
        .propose_work_plan(&request, &DevelopmentNoopRedactor)
        .expect_err("same existing prerequisite twice");
    assert!(matches!(error, StoreError::InvalidWork(reason)
        if reason == "plan has duplicate resolved prerequisite edges"));
    assert_eq!(
        test_database_shape_snapshot(&store.connection).expect("after"),
        before
    );
    request.plan.prerequisites.pop();
    store
        .propose_work_plan(&request, &DevelopmentNoopRedactor)
        .expect("one edge");
    assert!(store.verify_all().expect("doctor").is_healthy());
}

#[test]
fn atomic_plan_stores_inherited_and_explicit_child_options() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let mut request = request();
    request.plan.idempotency_key = "plan-child-options".into();
    request.plan.prerequisites.clear();
    request.plan.tasks = vec![
        {
            let mut root = task("root", None);
            root.kind = Some(WorkItemKind::Bug);
            root.priority = Some(3);
            root.labels = vec!["Parent".into(), "shared".into()];
            root.external_ref = Some("ext-root".into());
            root
        },
        {
            let mut inherited = task("inherited", Some("root"));
            inherited.labels = vec!["child-a".into(), "shared".into()];
            inherited
        },
        {
            let mut explicit = task("explicit", Some("root"));
            explicit.kind = Some(WorkItemKind::Feature);
            explicit.priority = Some(0);
            explicit.labels = vec!["child-b".into()];
            explicit.external_ref = Some("ext-child".into());
            explicit.requirement = Some(ChildRequirement::Optional);
            explicit.assigned_to = Some("assignee-a".into());
            explicit.deferred_until = Some(at(99));
            explicit
        },
        {
            let mut grandchild = task("grandchild", Some("inherited"));
            grandchild.labels = vec!["leaf".into()];
            grandchild
        },
        task("from-explicit", Some("explicit")),
    ];
    let receipt = store
        .propose_work_plan(&request, &DevelopmentNoopRedactor)
        .expect("plan");
    let item = |key: &str| {
        let mapping = receipt
            .tasks
            .iter()
            .find(|row| row.key == key)
            .unwrap_or_else(|| panic!("{key}"));
        load_work_item(&store.connection, mapping.work_id).expect("stored")
    };
    let root = item("root");
    let inherited = item("inherited");
    let explicit = item("explicit");
    let grandchild = item("grandchild");
    let from_explicit = item("from-explicit");

    assert_eq!(root.kind, WorkItemKind::Bug);
    assert_eq!(root.priority, 3);
    assert_eq!(root.labels, vec!["Parent", "shared"]);
    assert_eq!(root.external_ref.as_deref(), Some("ext-root"));

    assert_eq!(inherited.parent_id, Some(root.work_id));
    assert_eq!(inherited.kind, WorkItemKind::Task);
    assert_eq!(inherited.priority, 3);
    assert_eq!(inherited.child_requirement, ChildRequirement::Required);
    assert_eq!(inherited.assigned_to, None);
    assert_eq!(inherited.deferred_until, None);
    assert_eq!(inherited.labels, vec!["Parent", "child-a", "shared"]);
    assert_eq!(inherited.external_ref, None);

    assert_eq!(explicit.parent_id, Some(root.work_id));
    assert_eq!(explicit.kind, WorkItemKind::Feature);
    assert_eq!(explicit.priority, 0);
    assert_eq!(explicit.child_requirement, ChildRequirement::Optional);
    assert_eq!(explicit.assigned_to.as_deref(), Some("assignee-a"));
    assert_eq!(explicit.deferred_until, Some(at(99)));
    assert_eq!(explicit.labels, vec!["Parent", "child-b", "shared"]);
    assert_eq!(explicit.external_ref.as_deref(), Some("ext-child"));

    assert_eq!(grandchild.parent_id, Some(inherited.work_id));
    assert_eq!(grandchild.kind, WorkItemKind::Task);
    assert_eq!(grandchild.priority, 3);
    assert_eq!(grandchild.child_requirement, ChildRequirement::Required);
    assert_eq!(grandchild.assigned_to, None);
    assert_eq!(grandchild.deferred_until, None);
    assert_eq!(
        grandchild.labels,
        vec!["Parent", "child-a", "leaf", "shared"]
    );
    assert_eq!(grandchild.external_ref, None);
    assert_eq!(grandchild.root_id, root.work_id);

    assert_eq!(from_explicit.parent_id, Some(explicit.work_id));
    assert_eq!(from_explicit.kind, WorkItemKind::Task);
    assert_eq!(from_explicit.priority, 0);
    assert_eq!(from_explicit.child_requirement, ChildRequirement::Required);
    assert_eq!(from_explicit.assigned_to, None);
    assert_eq!(from_explicit.deferred_until, None);
    assert_eq!(from_explicit.labels, vec!["Parent", "child-b", "shared"]);
    assert_eq!(from_explicit.external_ref, None);
    assert_eq!(from_explicit.root_id, root.work_id);
    assert!(store.verify_all().expect("doctor").is_healthy());
}
