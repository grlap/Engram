use super::*;
use crate::domain::{WorkPlanDependency, WorkPlanPrerequisite, WorkPlanTask};
use crate::storage::test_database_shape_snapshot;
use crate::work_service::test_support::{at, root_input};
use crate::{ProjectId, SessionId};

fn plan(count: usize) -> WorkPlanInput {
    WorkPlanInput {
        idempotency_key: "atomic-plan".into(),
        tasks: (0..count)
            .map(|index| WorkPlanTask {
                key: format!("k{index:03}{}", "x".repeat(60)),
                parent_key: None,
                title: format!("Task {index}"),
                outcome: format!("Outcome {index}"),
                acceptance: vec!["delivered".into()],
                requirement: None,
                kind: None,
                priority: None,
                labels: Vec::new(),
                assigned_to: None,
                deferred_until: None,
                external_ref: None,
                notes: Vec::new(),
            })
            .collect(),
        prerequisites: Vec::new(),
    }
}

fn service(database: std::path::PathBuf) -> LocalWorkService {
    LocalWorkService::new(
        database,
        ProjectId("plan-service".into()),
        "planner".into(),
        SessionId("planner".into()),
        None,
    )
}

#[test]
fn atomic_plan_explicit_target_refuses_without_changing_ambient_state() {
    let temp = crate::test_support::temp_home().expect("temp");
    let service = service(temp.path().join("plan.db"));
    let WorkProposeResult::Root { work: focus, .. } = service
        .work_propose(root_input("Ambient focus", "focus"), at(0))
        .expect("focus item")
    else {
        panic!("root");
    };
    let WorkProposeResult::Root { work: target, .. } = service
        .work_propose(root_input("Explicit target", "target"), at(1))
        .expect("other item")
    else {
        panic!("root");
    };
    service
        .select_work(&focus.short_ref, at(2))
        .expect("select focus");
    let session = service
        .store()
        .expect("store")
        .work_session_state(&service.project_id, &service.session_id, at(2))
        .expect("session");
    assert_eq!(session.focused_work_id, Some(focus.work_id));
    assert_ne!(focus.work_id, target.work_id);
    let inspection = rusqlite::Connection::open(&service.database).expect("inspection");
    let before = test_database_shape_snapshot(&inspection).expect("before");
    let input = WorkProposeInput::Plan { plan: plan(3) };
    // A valid different target catches accidental binding; the current focus
    // is equally forbidden. Refusal must precede even protocol bookkeeping.
    for reference in [&target.short_ref, &focus.short_ref] {
        let error = service
            .work_propose_on(Some(reference), input.clone(), at(3))
            .expect_err("plans refuse explicit work_ref");
        assert!(matches!(error, StoreError::InvalidWork(reason)
            if reason == "plan parents must use payload-local keys; omit work_ref"));
        assert_eq!(
            test_database_shape_snapshot(&inspection).expect("after"),
            before
        );
        // Keep updated_at in this full equality: omitting it would hide a
        // timestamp-only session change on a refused plan.
        assert_eq!(
            service
                .store()
                .expect("store")
                .work_session_state(&service.project_id, &service.session_id, at(2))
                .expect("session after"),
            session
        );
    }
    let WorkProposeResult::Plan(receipt) =
        service.work_propose(input, at(4)).expect("without target")
    else {
        panic!("plan");
    };
    assert_eq!(receipt.tasks.len(), 3);
    assert_eq!(
        service
            .store()
            .expect("store")
            .work_session_state(&service.project_id, &service.session_id, at(2))
            .expect("session after admission"),
        session
    );
}

#[test]
fn atomic_plan_core_collision_refuses_different_intent_without_graph_writes() {
    for case in 0..3 {
        let temp = crate::test_support::temp_home().expect("temp");
        let service = service(temp.path().join("plan.db"));
        let input = plan(3);
        let mut original = input.clone();
        let mut actor = service.actor("work_propose", "atomically admit an authored local plan");
        match case {
            0 => original.tasks[0].outcome.push_str(" different"),
            1 => actor.actor_id = "different actor".into(),
            2 => actor.source_skill = Some("different skill".into()),
            _ => unreachable!(),
        }
        {
            let mut store = service.store_at(at(0)).expect("store");
            store
                .propose_work_plan(
                    &ProposeWorkPlanRequest {
                        project_id: service.project_id.clone(),
                        plan: original,
                        actor,
                        created_at: at(0),
                    },
                    &DevelopmentNoopRedactor,
                )
                .expect("direct core commit, no protocol receipt");
            // A pending attempt is permitted audit state. Prepare it before the
            // full-row snapshot so the refusal must leave everything unchanged.
            store
                .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
                    project_id: &service.project_id,
                    session_id: &service.session_id,
                    operation: "work_propose:plan",
                    idempotency_key: &input.idempotency_key,
                    intent: &service.protocol_intent(&WorkProposeInput::Plan {
                        plan: input.clone(),
                    }),
                    basis: &(),
                    now: at(0),
                })
                .expect("pending service attempt");
        }
        let connection = rusqlite::Connection::open(&service.database).expect("inspection");
        let before = test_database_shape_snapshot(&connection).expect("before");
        assert!(
            matches!(
                service.work_propose(WorkProposeInput::Plan { plan: input }, at(1)),
                Err(StoreError::WorkOperationIdempotencyConflict { .. })
            ),
            "case {case}"
        );
        assert_eq!(
            test_database_shape_snapshot(&connection).expect("after"),
            before
        );
    }
}

#[test]
fn atomic_plan_maximum_response_and_restart_replay_preserve_every_key() {
    let temp = crate::test_support::temp_home().expect("temp");
    let db = temp.path().join("plan.db");
    let service = service(db.clone());
    service
        .work_propose(root_input("Existing focus", "root"), at(0))
        .expect("focus");
    let before = service
        .store()
        .expect("store")
        .work_session_state(&service.project_id, &service.session_id, at(0))
        .expect("session");
    let mut full_tree = plan(crate::domain::MAX_WORK_PLAN_TASKS);
    let root_key = full_tree.tasks[0].key.clone();
    for task in &mut full_tree.tasks[1..] {
        task.parent_key = Some(root_key.clone());
    }
    let input = WorkProposeInput::Plan { plan: full_tree };
    let receipt = service.work_propose(input.clone(), at(1)).expect("plan");
    let WorkProposeResult::Plan(result) = &receipt else {
        panic!("plan");
    };
    assert_eq!(result.tasks.len(), 256);
    assert!(result.tasks.iter().all(|mapping| mapping.key.len() == 64));
    let bytes = serde_json::to_vec(&receipt).expect("json").len();
    assert!(bytes > crate::work_service::MAX_AGENT_WORK_RESPONSE_BYTES);
    assert!(bytes <= MAX_WORK_PLAN_RESPONSE_BYTES);
    assert!(crate::work_service::ensure_agent_response_budget(&receipt, "agent").is_err());
    eprintln!("atomic plan 256-task receipt: {bytes} compact JSON bytes");
    assert_eq!(
        service
            .store()
            .expect("store")
            .work_session_state(&service.project_id, &service.session_id, at(0))
            .expect("session"),
        before
    );
    let after = test_database_shape_snapshot(
        &rusqlite::Connection::open(&service.database).expect("inspection"),
    )
    .expect("shape");
    let restarted = super::tests::service(db);
    assert_eq!(
        serde_json::to_value(
            restarted
                .work_propose(input.clone(), at(2))
                .expect("replay")
        )
        .expect("json"),
        serde_json::to_value(receipt).expect("json")
    );
    assert_eq!(
        test_database_shape_snapshot(
            &rusqlite::Connection::open(&restarted.database).expect("inspection")
        )
        .expect("after"),
        after
    );
    let mut changed = input;
    let WorkProposeInput::Plan { plan } = &mut changed else {
        panic!("plan");
    };
    plan.tasks[0].outcome.push('!');
    assert!(matches!(
        restarted.work_propose(changed, at(3)),
        Err(StoreError::WorkOperationIdempotencyConflict { .. })
    ));
}

#[test]
fn atomic_plan_response_budget_refusal_rolls_back_graph_but_retains_protocol_attempt() {
    let temp = crate::test_support::temp_home().expect("temp");
    let service = service(temp.path().join("plan.db"));
    let mut input = plan(3);
    input.tasks[1].parent_key = Some(input.tasks[0].key.clone());
    input.tasks[2].parent_key = Some(input.tasks[1].key.clone());
    // Prepare precisely the protocol row allowed to survive a refused plan.
    {
        let mut store = service.store().expect("store");
        store
            .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
                project_id: &service.project_id,
                session_id: &service.session_id,
                operation: "work_propose:plan",
                idempotency_key: &input.idempotency_key,
                intent: &service.protocol_intent(&WorkProposeInput::Plan {
                    plan: input.clone(),
                }),
                basis: &(),
                now: at(0),
            })
            .expect("attempt");
    }
    let before = test_database_shape_snapshot(
        &rusqlite::Connection::open(&service.database).expect("inspection"),
    )
    .expect("before");
    let result = service.work_propose_plan_with_budget(None, &input, at(0), 1);
    assert!(
        matches!(result, Err(StoreError::InvalidWorkProjection(reason)) if reason.contains("1-byte limit"))
    );
    assert_eq!(
        test_database_shape_snapshot(
            &rusqlite::Connection::open(&service.database).expect("inspection")
        )
        .expect("after"),
        before
    );
    let receipt = service
        .work_propose(WorkProposeInput::Plan { plan: input }, at(1))
        .expect("ordinary budget");
    let WorkProposeResult::Plan(receipt) = receipt else {
        panic!("plan");
    };
    assert_eq!(receipt.tasks.len(), 3);
}

#[test]
fn atomic_plan_recovers_map_when_core_committed_before_protocol_receipt() {
    let temp = crate::test_support::temp_home().expect("temp");
    let db = temp.path().join("plan.db");
    let service = service(db.clone());
    let mut input = plan(80);
    input.prerequisites.push(WorkPlanPrerequisite {
        work_key: input.tasks[0].key.clone(),
        prerequisite: WorkPlanDependency::Local(input.tasks[1].key.clone()),
    });
    let receipt = {
        let mut store = service.store().expect("store");
        store
            .begin_work_protocol_attempt(&BeginWorkProtocolAttempt {
                project_id: &service.project_id,
                session_id: &service.session_id,
                operation: "work_propose:plan",
                idempotency_key: &input.idempotency_key,
                intent: &service.protocol_intent(&WorkProposeInput::Plan {
                    plan: input.clone(),
                }),
                basis: &(),
                now: at(0),
            })
            .expect("attempt");
        store
            .propose_work_plan(
                &ProposeWorkPlanRequest {
                    project_id: service.project_id.clone(),
                    plan: input.clone(),
                    actor: service.actor("work_propose", "atomically admit an authored local plan"),
                    created_at: at(0),
                },
                &DevelopmentNoopRedactor,
            )
            .expect("core commit")
    };
    let inspection = rusqlite::Connection::open(&db).expect("inspection");
    assert_eq!(
        inspection
            .query_row(
                "SELECT COUNT(*) FROM work_operation_results WHERE operation = 'propose_work_plan'",
                [],
                |row| row.get::<_, i64>(0)
            )
            .expect("core receipt"),
        1
    );
    assert_eq!(
        inspection
            .query_row(
                "SELECT COUNT(*) FROM work_protocol_attempts
             WHERE operation = 'work_propose:plan' AND result_json IS NULL",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("pending protocol"),
        1,
    );
    let mut restarted = super::tests::service(db);
    restarted.actor_context = Some("replacement host context".into());
    let WorkProposeResult::Plan(recovered) = restarted
        .work_propose(
            WorkProposeInput::Plan {
                plan: input.clone(),
            },
            at(1),
        )
        .expect("recover")
    else {
        panic!("plan");
    };
    assert_eq!(recovered, receipt);
    assert_eq!(
        inspection
            .query_row(
                "SELECT COUNT(*) FROM work_protocol_attempts
             WHERE operation = 'work_propose:plan' AND result_json IS NOT NULL",
                [],
                |row| row.get::<_, i64>(0),
            )
            .expect("finished protocol"),
        1,
    );
    assert_eq!(
        rusqlite::Connection::open(&restarted.database)
            .expect("inspection")
            .query_row("SELECT COUNT(*) FROM work_items", [], |row| row
                .get::<_, i64>(0))
            .expect("count"),
        80
    );
    let before = test_database_shape_snapshot(
        &rusqlite::Connection::open(&restarted.database).expect("inspection"),
    )
    .expect("before");
    let WorkProposeResult::Plan(replayed) = restarted
        .work_propose(WorkProposeInput::Plan { plan: input }, at(2))
        .expect("replay")
    else {
        panic!("plan");
    };
    assert_eq!(replayed, receipt);
    assert_eq!(
        test_database_shape_snapshot(
            &rusqlite::Connection::open(&restarted.database).expect("inspection")
        )
        .expect("after"),
        before
    );
}
