use super::*;

fn decomposition(parent: &WorkItem, count: usize) -> DecomposeWorkRequest {
    DecomposeWorkRequest {
        parent_id: parent.work_id,
        expected_parent_revision: parent.revision,
        children: (0..count)
            .map(|index| {
                child(
                    &format!("child-{index}"),
                    ChildRequirement::Required,
                    "Child",
                )
            })
            .collect(),
        prerequisites: Vec::new(),
        authority: WorkPlanningAuthority::Project,
        actor: actor("planner"),
        idempotency_key: "decompose".into(),
        created_at: at(1),
    }
}

fn unchanged<T: std::fmt::Debug>(
    transaction: &Transaction<'_>,
    action: impl FnOnce() -> Result<T, StoreError>,
    expected: &str,
) {
    let before = test_database_shape_snapshot(transaction).expect("before helper");
    let changes = transaction.total_changes();
    assert!(matches!(action(), Err(StoreError::InvalidWork(reason)) if reason == expected));
    assert_eq!(transaction.total_changes(), changes, "no helper writes");
    assert_eq!(
        test_database_shape_snapshot(transaction).expect("after helper, before rollback"),
        before,
        "a typed refusal must leave the caller's transaction unchanged"
    );
}

#[test]
fn shared_root_guards_refuse_invalid_inputs_before_writes() {
    for validation in [
        PlanningValidation::Immediate,
        PlanningValidation::AtomicPlan,
    ] {
        let mut store = SqliteStore::open_in_memory().expect("store");
        let transaction = store.begin_work_mutation().expect("transaction");
        for (priority, parent, origin, snapshot, reason) in [
            (
                -1,
                None,
                WorkOrigin::Local,
                None,
                "priority must be an integer from 0 through 4",
            ),
            (
                5,
                None,
                WorkOrigin::Local,
                None,
                "priority must be an integer from 0 through 4",
            ),
            (
                1,
                Some(WorkId::new()),
                WorkOrigin::Local,
                None,
                "direct child creation is not allowed; use decompose_work with the parent revision",
            ),
            (
                1,
                None,
                WorkOrigin::Imported,
                None,
                "imported work requires a source snapshot",
            ),
            (
                1,
                None,
                WorkOrigin::Local,
                Some(ObjectId::mint()),
                "local work cannot carry an imported source snapshot",
            ),
        ] {
            let mut request = root_request("shared-root", "root", 0);
            request.priority = priority;
            request.parent_id = parent;
            request.origin = origin;
            request.source_snapshot_id = snapshot;
            unchanged(
                &transaction,
                || {
                    create_root_with_validation_on(
                        &transaction,
                        &request,
                        &[],
                        &DevelopmentNoopRedactor,
                        validation,
                    )
                },
                reason,
            );
        }
    }
}

#[test]
fn shared_decomposition_guards_refuse_invalid_inputs_before_writes() {
    for validation in [
        PlanningValidation::Immediate,
        PlanningValidation::AtomicPlan,
    ] {
        let mut store = SqliteStore::open_in_memory().expect("store");
        let parent = store
            .create_work(
                &root_request("shared-children", "root", 0),
                &DevelopmentNoopRedactor,
            )
            .expect("root");
        let transaction = store.begin_work_mutation().expect("transaction");
        let limit = validation.child_budget();
        for count in [0, limit + 1] {
            let request = decomposition(&parent, count);
            unchanged(
                &transaction,
                || {
                    decompose_work_with_validation_on(
                        &transaction,
                        &request,
                        &vec![Vec::new(); count],
                        &DevelopmentNoopRedactor,
                        validation,
                    )
                },
                &format!("decomposition must contain from 1 through {limit} children"),
            );
        }
        for (key, priority, reason) in [
            (" \t ", 1, "child local key must not be empty"),
            (" child-0 ", 1, "child local keys must be unique"),
            (
                "child-1",
                -1,
                "child priority must be an integer from 0 through 4",
            ),
            (
                "child-1",
                5,
                "child priority must be an integer from 0 through 4",
            ),
        ] {
            let mut request = decomposition(&parent, 2);
            // Keep the first child valid: catching this after child insertion
            // would already have changed the caller-owned transaction.
            request.children[1].local_key = key.into();
            request.children[1].priority = priority;
            unchanged(
                &transaction,
                || {
                    decompose_work_with_validation_on(
                        &transaction,
                        &request,
                        &[Vec::new(), Vec::new()],
                        &DevelopmentNoopRedactor,
                        validation,
                    )
                },
                reason,
            );
        }
    }
}

#[test]
fn shared_prerequisite_guards_refuse_self_add_and_remove_before_writes() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("shared-self", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let request = ChangeWorkPrerequisiteRequest {
        work_id: root.work_id,
        prerequisite_id: root.work_id,
        expected_revision: root.revision,
        authority: WorkPlanningAuthority::Project,
        actor: actor("planner"),
        idempotency_key: "self".into(),
        changed_at: at(1),
    };
    let transaction = store.begin_work_mutation().expect("transaction");
    let before = test_database_shape_snapshot(&transaction).expect("before");
    let changes = transaction.total_changes();
    for add in [false, true] {
        assert!(matches!(
            change_work_prerequisite_on(&transaction, &request, add),
            Err(StoreError::WorkDependencyCycle)
        ));
        let mut basis = PlanRelationBasis {
            work_id: root.work_id,
            basis: validated_current_work_relation_basis(&transaction, root.work_id)
                .expect("basis"),
        };
        let before_basis = serde_json::to_value(&basis.basis).expect("before basis");
        assert!(matches!(
            change_work_prerequisite_with_validation_on(
                &transaction,
                &request,
                add,
                PlanningValidation::AtomicPlan,
                Some(&mut basis),
            ),
            Err(StoreError::WorkDependencyCycle)
        ));
        assert_eq!(
            serde_json::to_value(&basis.basis).expect("after basis"),
            before_basis
        );
        assert_eq!(transaction.total_changes(), changes);
        assert_eq!(
            test_database_shape_snapshot(&transaction).expect("before rollback"),
            before
        );
    }
}

#[test]
fn shared_decomposition_retains_mode_specific_fanout_and_priority_boundaries() {
    for (validation, count) in [
        (
            PlanningValidation::Immediate,
            MAX_CHILDREN_PER_DECOMPOSITION,
        ),
        (
            PlanningValidation::AtomicPlan,
            MAX_CHILDREN_PER_DECOMPOSITION + 1,
        ),
    ] {
        let mut store = SqliteStore::open_in_memory().expect("store");
        let mut root = root_request("shared-boundary", "root", 0);
        root.priority = 0;
        let transaction = store.begin_work_mutation().expect("transaction");
        let parent = create_root_with_validation_on(
            &transaction,
            &root,
            &[],
            &DevelopmentNoopRedactor,
            validation,
        )
        .expect("priority zero root");
        let mut request = decomposition(&parent, count);
        for (index, draft) in request.children.iter_mut().enumerate() {
            draft.priority = if index % 2 == 0 { 0 } else { 4 };
        }
        let result = decompose_work_with_validation_on(
            &transaction,
            &request,
            &vec![Vec::new(); count],
            &DevelopmentNoopRedactor,
            validation,
        )
        .expect("mode-specific boundary");
        assert_eq!(result.children.len(), count);
        assert_eq!(result.children[0].priority, 0);
        assert_eq!(result.children[1].priority, 4);
        // Atomic helpers defer these whole-graph checks to their caller.
        assert!(combined_graph_is_acyclic(&transaction, "shared-boundary").expect("acyclic"));
        validate_root_descendant_budget(&transaction, "shared-boundary", parent.root_id, 0)
            .expect("root bound");
        transaction.commit().expect("commit");
        assert!(store.verify_all().expect("doctor").is_healthy());
    }
}
