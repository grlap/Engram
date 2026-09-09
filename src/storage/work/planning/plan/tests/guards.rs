use super::*;
use crate::storage::work::query::latest_canonical_work_event_for_item;
use crate::storage::work::{WorkEventDraft, feeds};

#[test]
fn atomic_plan_relation_basis_rejects_wrong_owner_and_immediate_mode_without_writes() {
    for wrong_owner in [true, false] {
        let mut store = SqliteStore::open_in_memory().expect("store");
        let mut input = request();
        input.plan.tasks = vec![task("a", None), task("b", None)];
        input.plan.prerequisites.clear();
        let receipt = store
            .propose_work_plan(&input, &DevelopmentNoopRedactor)
            .expect("seed");
        let work_id = receipt.tasks[0].work_id;
        let other_id = receipt.tasks[1].work_id;
        let before = test_database_shape_snapshot(&store.connection).expect("before");
        let tx = store.begin_work_mutation().expect("transaction");
        let item = load_work_item(&tx, work_id).expect("item");
        let mut basis = PlanRelationBasis {
            work_id: if wrong_owner { other_id } else { work_id },
            basis: validated_current_work_relation_basis(&tx, work_id).expect("basis"),
        };
        let error = change_work_prerequisite_with_validation_on(
            &tx,
            &ChangeWorkPrerequisiteRequest {
                work_id,
                prerequisite_id: other_id,
                expected_revision: item.revision,
                authority: WorkPlanningAuthority::Project,
                actor: input.actor.clone(),
                idempotency_key: String::new(),
                changed_at: at(1),
            },
            true,
            if wrong_owner {
                PlanningValidation::AtomicPlan
            } else {
                PlanningValidation::Immediate
            },
            Some(&mut basis),
        )
        .expect_err("invalid relation basis");
        assert!(matches!(error, StoreError::InvalidWorkProjection(reason)
            if reason == "planned relation basis has the wrong owner"));
        // Inspect before dropping the transaction: rollback cannot mask writes.
        assert_eq!(
            test_database_shape_snapshot(&tx).expect("inside refusal"),
            before
        );
        drop(tx);
        assert_eq!(
            test_database_shape_snapshot(&store.connection).expect("after"),
            before
        );
    }
}

#[test]
fn atomic_plan_event_basis_rejects_other_transition_kinds_without_writes() {
    let mut store = SqliteStore::open_in_memory().expect("store");
    let root = store
        .create_work(
            &root_request("atomic-plan", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .expect("root");
    let before = test_database_shape_snapshot(&store.connection).expect("before");
    let tx = store.begin_work_mutation().expect("transaction");
    let event = latest_canonical_work_event_for_item(&tx, root.work_id).expect("created event");
    let mut draft = WorkEventDraft::with_root_state(&event, None);
    draft.transition = crate::domain::WorkTransition::Revised {
        authority: WorkPlanningAuthority::Project,
    };
    draft.created_at = at(1);
    let basis = validated_current_work_relation_basis(&tx, root.work_id).expect("basis");
    let error = feeds::append_planned_prerequisite_event(&tx, &draft, &basis)
        .expect_err("wrong transition");
    assert!(matches!(error, StoreError::InvalidWorkProjection(reason)
        if reason == "planned relation basis requires a prerequisite addition"));
    assert_eq!(
        test_database_shape_snapshot(&tx).expect("inside refusal"),
        before
    );
    drop(tx);
    assert_eq!(
        test_database_shape_snapshot(&store.connection).expect("after"),
        before
    );
}

#[test]
fn atomic_plan_published_limits_allow_one_complete_tree() {
    // These are published numeric limits, not canonical hash fixtures.
    assert_eq!(MAX_WORK_PLAN_TASKS, 256);
    assert_eq!(MAX_WORK_PLAN_EDGES, 1024);
    assert_eq!(MAX_WORK_PLAN_BYTES, 1_048_576);
    assert_eq!(
        MAX_WORK_PLAN_TASKS,
        usize::try_from(MAX_OPEN_WORK_DESCENDANTS).expect("descendant limit") + 1
    );
}
