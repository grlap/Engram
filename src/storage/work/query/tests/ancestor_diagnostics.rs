use super::*;
use crate::storage::work::completion::{AncestorExecutionState, first_blocking_ancestor};

#[test]
fn terminal_ancestor_diagnostics_match_canonical_admission_without_claim_writes() {
    for intermediate in [false, true] {
        for independent_barrier in [false, true] {
            let mut store = SqliteStore::open_in_memory().unwrap();
            let root = store
                .create_work(
                    &root_request("ancestors", "root", 0),
                    &DevelopmentNoopRedactor,
                )
                .unwrap();
            let parent = store
                .decompose_work(
                    &DecomposeWorkRequest {
                        parent_id: root.work_id,
                        expected_parent_revision: root.revision,
                        children: vec![super::child(
                            "parent",
                            ChildRequirement::Optional,
                            "Parent",
                        )],
                        prerequisites: Vec::new(),
                        authority: WorkPlanningAuthority::Project,
                        actor: actor("planner"),
                        idempotency_key: "parent".into(),
                        created_at: at(1),
                    },
                    &DevelopmentNoopRedactor,
                )
                .unwrap()
                .children
                .remove(0);
            let child = store
                .decompose_work(
                    &DecomposeWorkRequest {
                        parent_id: parent.work_id,
                        expected_parent_revision: parent.revision,
                        children: vec![super::child("child", ChildRequirement::Optional, "Child")],
                        prerequisites: Vec::new(),
                        authority: WorkPlanningAuthority::Project,
                        actor: actor("planner"),
                        idempotency_key: "child".into(),
                        created_at: at(2),
                    },
                    &DevelopmentNoopRedactor,
                )
                .unwrap()
                .children
                .remove(0);
            assert!(
                super::super::super::completion::ancestors_admit_execution(
                    &store.connection,
                    &child
                )
                .unwrap()
            );
            assert_eq!(
                store
                    .inspect_work(child.work_id, at(3))
                    .unwrap()
                    .blocking_ancestor,
                None
            );
            if independent_barrier {
                let prerequisite = store
                    .create_work(
                        &root_request("ancestors", "prerequisite", 3),
                        &DevelopmentNoopRedactor,
                    )
                    .unwrap();
                store
                    .add_work_prerequisite(
                        &crate::domain::ChangeWorkPrerequisiteRequest {
                            work_id: child.work_id,
                            prerequisite_id: prerequisite.work_id,
                            expected_revision: child.revision,
                            authority: WorkPlanningAuthority::Project,
                            actor: actor("planner"),
                            idempotency_key: "prerequisite-link".into(),
                            changed_at: at(4),
                        },
                        &DevelopmentNoopRedactor,
                    )
                    .unwrap();
                let revised_child = store.get_work_item(child.work_id).unwrap();
                store
                    .add_work_blocker(
                        &crate::domain::AddWorkBlockerRequest {
                            work_id: child.work_id,
                            expected_work_revision: revised_child.revision,
                            kind: crate::domain::WorkBlockerKind::Manual,
                            detail: "independent barrier".into(),
                            authority: WorkPlanningAuthority::Project,
                            actor: actor("planner"),
                            idempotency_key: "blocker".into(),
                            blocked_at: at(5),
                        },
                        &DevelopmentNoopRedactor,
                    )
                    .unwrap();
            }
            let terminal = store
                .get_work_item(if intermediate {
                    parent.work_id
                } else {
                    root.work_id
                })
                .unwrap();
            let held = claim(&mut store, &terminal, "holder", "terminal-claim", 6, 3600);
            let terminal = store.get_work_item(terminal.work_id).unwrap();
            let proof = evidence(&mut store, &terminal, &held, "holder", "proof", 7);
            checkpoint(
                &mut store,
                &terminal,
                &held,
                "holder",
                "completion-checkpoint",
                7,
                std::slice::from_ref(&proof),
            );
            complete(
                &mut store, &terminal, &held, "holder", &proof, "complete", 8,
            )
            .unwrap();
            let child = store.get_work_item(child.work_id).unwrap();
            let projected = store.inspect_work(child.work_id, at(9)).unwrap();
            let canonical =
                inspect_work_canonical_on(&store.connection, child.work_id, at(9)).unwrap();
            assert_eq!(projected, canonical);
            assert_eq!(projected.availability, WorkAvailability::Blocked);
            let ancestor = projected.blocking_ancestor.as_ref().unwrap();
            assert_eq!(ancestor.work_id, terminal.work_id);
            assert_eq!(ancestor.short_ref, terminal.short_ref);
            assert_eq!(ancestor.lifecycle, WorkLifecycle::Completed);
            assert!(
                !super::super::super::completion::ancestors_admit_execution(
                    &store.connection,
                    &child
                )
                .unwrap()
            );
            assert_eq!(projected.blockers.len(), usize::from(independent_barrier));
            assert_eq!(projected.blocked_by.len(), usize::from(independent_barrier));
            assert_eq!(
                projected
                    .reason_codes
                    .contains(&WorkReadinessReason::DetachAvailable),
                !independent_barrier
            );
            let before = test_database_shape_snapshot(&store.connection).unwrap();
            let refusal = store
                .claim_work(
                    &ClaimWorkRequest {
                        work_id: child.work_id,
                        expected_work_revision: child.revision,
                        expected_run_id: child.active_run_id,
                        holder: SessionId("new-holder".into()),
                        ttl_seconds: 60,
                        recovery_reason: None,
                        actor: actor("new-holder"),
                        idempotency_key: "refused-claim".into(),
                        claimed_at: at(9),
                    },
                    &DevelopmentNoopRedactor,
                )
                .unwrap_err();
            assert!(
                matches!(refusal, StoreError::WorkAncestorNotOpen { work, ancestor }
                if work == child.work_id && ancestor.work_id == terminal.work_id)
            );
            assert_eq!(
                test_database_shape_snapshot(&store.connection).unwrap(),
                before
            );
            assert!(store.verify_all().unwrap().is_healthy());
        }
    }
}

#[test]
fn ancestor_walk_selects_nearest_non_open_and_keeps_corruption_guards() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let root = store
        .create_work(&root_request("walk", "root", 0), &DevelopmentNoopRedactor)
        .unwrap();
    let mut item = root.clone();
    item.work_id = WorkId::new();
    let parent = WorkId::new();
    item.parent_id = Some(parent);
    for lifecycle in [
        WorkLifecycle::Proposed,
        WorkLifecycle::Completed,
        WorkLifecycle::Cancelled,
        WorkLifecycle::Superseded,
    ] {
        let found = first_blocking_ancestor(&item, |id| {
            Ok(AncestorExecutionState {
                project_id: root.project_id.clone(),
                root_id: root.work_id,
                parent_id: Some(root.work_id),
                ancestor: crate::domain::WorkBlockingAncestor {
                    work_id: id,
                    short_ref: "parent".into(),
                    lifecycle,
                },
            })
        })
        .unwrap()
        .unwrap();
        assert_eq!((found.work_id, found.lifecycle), (parent, lifecycle));
    }
    for cross_boundary in [false, true] {
        let refusal = first_blocking_ancestor(&item, |id| {
            Ok(AncestorExecutionState {
                project_id: if cross_boundary {
                    ProjectId("other".into())
                } else {
                    root.project_id.clone()
                },
                root_id: root.work_id,
                parent_id: Some(parent),
                ancestor: crate::domain::WorkBlockingAncestor {
                    work_id: id,
                    short_ref: "parent".into(),
                    lifecycle: WorkLifecycle::Open,
                },
            })
        });
        assert!(matches!(refusal, Err(StoreError::InvalidWorkProjection(_))));
    }
}

#[test]
fn all_open_generation_barrier_does_not_invent_a_terminal_ancestor() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let root = store
        .create_work(
            &root_request("generation", "root", 0),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    // Exercise the projected stale-generation branch with open ancestry.
    // The canonical mutation path still validates the root head independently.
    store
        .connection
        .execute(
            "UPDATE work_root_executions SET state = 'completed' WHERE root_id = ?1",
            [root.work_id.0.to_string()],
        )
        .unwrap();
    let view =
        derive_projected_work_availability(&store.connection, root, Vec::new(), Vec::new(), at(1))
            .unwrap();
    assert_eq!(view.availability, WorkAvailability::Blocked);
    assert_eq!(view.blocking_ancestor, None);
    assert_eq!(view.blocking_parent, None);
    assert!(
        !view
            .reason_codes
            .contains(&WorkReadinessReason::DetachAvailable)
    );
    assert_eq!(
        view.why,
        ["the ancestor or root-execution generation does not admit execution"]
    );
}
