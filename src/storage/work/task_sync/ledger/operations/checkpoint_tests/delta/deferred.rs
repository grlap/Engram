use super::*;
use crate::storage::work::test_support::{DevelopmentNoopRedactor, actor};
use crate::task_sync::ledger::CompletedExchange;

mod foreign_retention;
mod historical_claim;
mod native_successor;
mod restored_successor;
mod writer;

fn claimed_fixture() -> Fixture {
    claimed_fixture_with_setup(&candidate(), |_, _| {})
}

/// The selected root is ready to claim: not deferred, unassigned and without
/// prerequisites. The setup runs with nothing claimed.
fn claimable_fixture_with_setup(
    tree: &crate::task_sync::TaskTree,
    setup: impl FnOnce(&mut SqliteStore, crate::WorkId),
) -> Fixture {
    let mut tree = tree.clone();
    for task in &mut tree.tasks {
        if task.lifecycle == crate::WorkLifecycle::Open && task.parent_id.is_none() {
            task.deferred_until = None;
            task.assigned_to = None;
            task.prerequisites.clear();
        }
    }
    Fixture::with_setup(
        &tree,
        |tree| {
            let mut task = tree
                .tasks
                .iter()
                .find(|task| {
                    task.lifecycle == crate::WorkLifecycle::Open && task.parent_id.is_none()
                })
                .unwrap()
                .clone();
            task.title = "deferred incoming planning".into();
            BTreeMap::from([(task.id, Some(task))])
        },
        setup,
    )
}

fn claimed_fixture_with_setup(
    tree: &crate::task_sync::TaskTree,
    setup: impl FnOnce(&mut SqliteStore, crate::WorkId),
) -> Fixture {
    claimable_fixture_with_setup(tree, |store, selected| {
        let item = store.get_work_item(selected).unwrap();
        store
            .claim_work(
                &crate::ClaimWorkRequest {
                    work_id: selected,
                    expected_work_revision: item.revision,
                    expected_run_id: item.active_run_id,
                    holder: crate::SessionId("runner".into()),
                    ttl_seconds: 600,
                    recovery_reason: Some("recover retained planning".into()),
                    actor: actor("runner"),
                    idempotency_key: "deferred-holder".into(),
                    claimed_at: at(31),
                },
                &DevelopmentNoopRedactor,
            )
            .unwrap();
        setup(store, selected);
    })
}

/// A live claim taken after the request's local cut, while its candidate is
/// retained, is exactly the local owner deferral waits for: applying defers
/// under that claim instead of refusing for ever on the claim's own event. A
/// change after the cut with no live owner still refuses, with nothing written.
#[test]
fn a_claim_after_the_retained_cut_defers_and_unowned_drift_still_refuses() {
    let mut f = claimable_fixture_with_setup(&candidate(), |_, _| {});
    let item = f.store.get_work_item(f.selected).unwrap();
    f.store
        .revise_work(
            &crate::ReviseWorkRequest {
                work_id: f.selected,
                expected_revision: item.revision,
                patch: crate::WorkRevisionPatch {
                    title: Some("Local edit after the retained cut".into()),
                    ..Default::default()
                },
                authority: crate::WorkPlanningAuthority::Project,
                actor: actor("runner"),
                idempotency_key: "unowned-drift".into(),
                updated_at: at(34),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let before = state(&f.store);
    let error = f.apply().unwrap_err();
    assert!(
        error
            .to_string()
            .contains("native readset changed since its local cut"),
        "{error}"
    );
    assert_eq!(state(&f.store), before);
    let item = f.store.get_work_item(f.selected).unwrap();
    f.store
        .claim_work(
            &crate::ClaimWorkRequest {
                work_id: f.selected,
                expected_work_revision: item.revision,
                expected_run_id: item.active_run_id,
                holder: crate::SessionId("runner".into()),
                ttl_seconds: 600,
                recovery_reason: Some("claim after the retained cut".into()),
                actor: actor("runner"),
                idempotency_key: "claim-after-cut".into(),
                claimed_at: at(35),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let deferred = f.apply().unwrap();
    let Transition::PlanningDeferred {
        blocking_claims, ..
    } = &deferred.event.transition
    else {
        panic!(
            "a live claim after the cut defers: {:?}",
            deferred.event.transition
        )
    };
    assert_eq!(blocking_claims.len(), 1);
    assert_eq!(
        f.store.get_work_item(f.selected).unwrap().title,
        "Local edit after the retained cut"
    );
    assert_eq!(f.apply().unwrap(), deferred);
    assert!(f.store.verify_all().unwrap().is_healthy());
}

fn successor_request(f: &Fixture, original: &CompletedExchange) -> PlanningSuccessorRequest {
    let Transition::PlanningDeferred { hold, .. } = &original.event.transition else {
        panic!("deferred planning expected")
    };
    let mut intent = f.input.intent.clone();
    intent.operation = uuid::Uuid::new_v4();
    intent.candidate.operation = intent.operation;
    intent.key = "new successor inspection".into();
    let cut = producing_origin::cut_on(&f.store.connection, &f.input.binding.project).unwrap();
    intent.local_cut.position.position = cut.position;
    intent.local_cut.anchor = cut.anchor;
    PlanningSuccessorRequest {
        deferred: original.outcome.clone(),
        hold: hold.clone(),
        original_prepared: original.prepared.clone(),
        original_candidate: f.input.intent.candidate.content_fingerprint.clone(),
        intent,
    }
}

fn check_successor_admission(
    f: &mut Fixture,
    recipe: &PlanningSuccessorRecipe,
) -> Result<(), StoreError> {
    let tx = f.store.begin_work_mutation()?;
    let check = crate::storage::work::task_sync::ledger::delta::successor::admit_on(
        &tx,
        &f.input.binding_id,
        recipe,
    )?;
    assert_eq!(check.hold_id(), &recipe.request.hold);
    assert_eq!(check.recipe(), recipe);
    assert!(std::ptr::eq(check.transaction(), &raw const *tx));
    Ok(())
}

#[test]
fn successor_admission_preserves_normally_completed_execution() {
    use crate::storage::work::test_support::{checkpoint, complete, evidence};
    let mut tree = candidate();
    let mut task = tree
        .tasks
        .iter()
        .find(|task| task.lifecycle == crate::WorkLifecycle::Open && task.parent_id.is_none())
        .unwrap()
        .clone();
    task.deferred_until = None;
    task.assigned_to = None;
    task.prerequisites.clear();
    task.acceptance_bindings.clear();
    task.evaluation_mode = None;
    tree.tasks = vec![task];
    let mut f = Fixture::with_setup(
        &tree,
        |tree| {
            let mut task = tree.tasks[0].clone();
            task.title = "planning after normal local completion".into();
            BTreeMap::from([(task.id, Some(task))])
        },
        |store, selected| {
            let item = store.get_work_item(selected).unwrap();
            store
                .claim_work(
                    &crate::ClaimWorkRequest {
                        work_id: selected,
                        expected_work_revision: item.revision,
                        expected_run_id: item.active_run_id,
                        holder: crate::SessionId("runner".into()),
                        ttl_seconds: 600,
                        recovery_reason: None,
                        actor: actor("runner"),
                        idempotency_key: "successor-normal-holder".into(),
                        claimed_at: at(31),
                    },
                    &DevelopmentNoopRedactor,
                )
                .unwrap();
        },
    );
    let original = f.apply().unwrap();
    let claim = f.store.claim_fixture(f.selected);
    let item = f.store.get_work_item(f.selected).unwrap();
    let proof = evidence(
        &mut f.store,
        &item,
        &claim,
        "runner",
        "normal-end-proof",
        41,
    );
    checkpoint(
        &mut f.store,
        &item,
        &claim,
        "runner",
        "normal-end-checkpoint",
        41,
        std::slice::from_ref(&proof),
    );
    let item = f.store.get_work_item(f.selected).unwrap();
    let seal = complete(
        &mut f.store,
        &item,
        &claim,
        "runner",
        &proof,
        "normal-end",
        42,
    )
    .unwrap();
    let ended = f.store.get_work_item(f.selected).unwrap();
    assert_eq!(ended.lifecycle, crate::WorkLifecycle::Completed);
    assert_eq!(ended.active_run_id, None);
    let historical_run =
        crate::storage::work::query::load_work_run(&f.store.connection, claim.run_id).unwrap();
    assert_eq!(historical_run.state, crate::WorkRunState::Completed);
    let request = successor_request(&f, &original);
    let basis = f
        .store
        .inspect_task_sync_planning_successor(&f.input.binding_id, &request)
        .unwrap();
    let recipe = PlanningSuccessorRecipe::new(request, basis, &[]).unwrap();
    let before = state(&f.store);
    check_successor_admission(&mut f, &recipe).unwrap();
    assert_eq!(state(&f.store), before);
    assert_eq!(f.store.get_work_item(f.selected).unwrap(), ended);
    assert_eq!(
        complete(
            &mut f.store,
            &item,
            &claim,
            "runner",
            &proof,
            "normal-end",
            42
        )
        .unwrap(),
        seal
    );
    assert_eq!(f.apply().unwrap(), original);
    let mut retirement = recipe.clone();
    retirement.retirements.push(PlannedRunRetirement {
        run: claim.run_id,
        event: ObjectId::mint(),
    });
    let error = check_successor_admission(&mut f, &retirement).unwrap_err();
    assert!(
        error.to_string().contains("cannot retire local execution"),
        "{error}"
    );
    let mut changed = recipe.clone();
    changed.before[0].revision += 1;
    let error = check_successor_admission(&mut f, &changed).unwrap_err();
    assert!(
        error.to_string().contains("local readset changed"),
        "{error}"
    );
    assert_eq!(state(&f.store), before);
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn successor_inspection_preserves_live_holder_and_original_deferred_receipt() {
    let mut f = claimed_fixture();
    let original = f.apply().unwrap();
    let request = successor_request(&f, &original);
    let before = state(&f.store);
    let item = f.store.get_work_item(f.selected).unwrap();
    let basis = f
        .store
        .inspect_task_sync_planning_successor(&f.input.binding_id, &request)
        .unwrap();
    assert!(basis.iter().any(|basis| basis.work_id == f.selected));
    let recipe =
        crate::task_sync::delta::PlanningSuccessorRecipe::new(request.clone(), basis, &[]).unwrap();
    let retained = crate::canonical::canonical_bytes(&recipe).unwrap();
    let recovered: crate::task_sync::delta::PlanningSuccessorRecipe =
        serde_json::from_slice(&retained).unwrap();
    recovered.validate().unwrap();
    assert_eq!(recovered, recipe);
    let error = check_successor_admission(&mut f, &recipe).unwrap_err();
    assert!(error.to_string().contains("active local claim"), "{error}");
    for mutation in ["original identity", "planned identity", "foreign run"] {
        let mut changed = recovered.clone();
        match mutation {
            "original identity" => changed.outcome.clone_from(&request.deferred),
            "planned identity" => changed.baseline.clone_from(&changed.planning),
            "foreign run" => {
                changed
                    .retirements
                    .push(crate::task_sync::delta::PlannedRunRetirement {
                        run: crate::WorkRunId(uuid::Uuid::new_v4()),
                        event: ObjectId::mint(),
                    });
            }
            _ => unreachable!(),
        }
        assert!(changed.validate().is_err(), "{mutation}");
    }
    assert_eq!(state(&f.store), before);
    assert_eq!(f.store.get_work_item(f.selected).unwrap(), item);
    assert_eq!(f.apply().unwrap(), original);
    for mutation in ["old key", "old operation", "source", "hold", "cut"] {
        let mut changed = request.clone();
        match mutation {
            "old key" => changed.intent.key.clone_from(&f.input.intent.key),
            "old operation" => {
                changed.intent.operation = f.input.intent.operation;
                changed.intent.candidate.operation = f.input.intent.operation;
            }
            "source" => {
                let crate::task_sync::delta::DeltaBasis::Incoming { source_after, .. } =
                    &mut changed.intent.basis
                else {
                    unreachable!()
                };
                source_after.commit.parents.clear();
                source_after
                    .commit
                    .tree
                    .clone_from(&source_after.commit.oid);
            }
            "hold" => changed.hold = ObjectId::mint(),
            "cut" => changed.intent.local_cut.position.position += 1,
            _ => unreachable!(),
        }
        assert!(
            f.store
                .inspect_task_sync_planning_successor(&f.input.binding_id, &changed)
                .is_err(),
            "{mutation}"
        );
        assert_eq!(state(&f.store), before);
    }
}

#[test]
fn deferred_planning_keeps_hold_and_original_receipt_after_holder_release() {
    let mut f = claimed_fixture();
    let before_item = f.store.get_work_item(f.selected).unwrap();
    let baseline = crate::storage::work::task_sync::baseline::head(
        &f.store.connection,
        &f.input.binding.project,
    )
    .unwrap()
    .unwrap()
    .record;
    let applied_head: Option<String> = f.store.connection.query_row(
        "SELECT last_incoming_completed_id FROM work_task_sync_exchange_heads WHERE binding_id=?1",
        [f.input.binding_id.as_str()], |row| row.get(0),
    ).unwrap();
    let original = f.apply().unwrap();
    let Transition::PlanningDeferred {
        hold,
        blocking_claims,
        ..
    } = &original.event.transition
    else {
        panic!("deferred planning expected")
    };
    assert_eq!(blocking_claims.len(), 1);
    assert_eq!(f.store.get_work_item(f.selected).unwrap(), before_item);
    assert_eq!(
        crate::storage::work::task_sync::baseline::head(
            &f.store.connection,
            &f.input.binding.project,
        )
        .unwrap()
        .unwrap()
        .record,
        baseline
    );
    let head: (Option<String>, Option<String>) = f.store.connection.query_row(
        "SELECT pending_prepared_id,last_incoming_completed_id FROM work_task_sync_exchange_heads WHERE binding_id=?1",
        [f.input.binding_id.as_str()], |row| Ok((row.get(0)?,row.get(1)?)),
    ).unwrap();
    assert_eq!(head, (None, applied_head));
    let reconciliation = f
        .store
        .task_sync_reconciliation_hold(hold)
        .unwrap()
        .unwrap();
    assert_eq!(reconciliation.prepared, original.prepared);
    assert!(
        crate::storage::work::task_sync::reconciliation::refuse_held_on(
            &f.store.connection,
            f.selected,
        )
        .is_err()
    );
    let renewed = f
        .store
        .claim_work(
            &crate::ClaimWorkRequest {
                work_id: f.selected,
                expected_work_revision: before_item.revision,
                expected_run_id: before_item.active_run_id,
                holder: crate::SessionId("runner".into()),
                ttl_seconds: 600,
                recovery_reason: None,
                actor: actor("runner"),
                idempotency_key: "deferred-holder-renew".into(),
                claimed_at: at(41),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    f.store
        .release_work(
            &crate::ReleaseWorkRequest {
                work_id: f.selected,
                run_id: renewed.run_id,
                expected_work_revision: before_item.revision,
                claim_id: renewed.claim_id,
                claim_fence: renewed.fence,
                holder: renewed.holder.clone(),
                reason: "original holder settled".into(),
                waiver_reason: Some("no unfinished contribution".into()),
                actor: actor("runner"),
                idempotency_key: "deferred-holder-release".into(),
                released_at: at(42),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    // A new local basis and an ended claim do not reinterpret the old key.
    assert_eq!(f.apply().unwrap(), original);
    let request = successor_request(&f, &original);
    let basis = f
        .store
        .inspect_task_sync_planning_successor(&f.input.binding_id, &request)
        .unwrap();
    let recipe = PlanningSuccessorRecipe::new(request, basis, &[]).unwrap();
    let before = state(&f.store);
    let error = check_successor_admission(&mut f, &recipe).unwrap_err();
    assert!(
        error.to_string().contains("unfinished local run"),
        "{error}"
    );
    assert_eq!(state(&f.store), before);
    assert!(
        crate::storage::work::task_sync::reconciliation::refuse_held_on(
            &f.store.connection,
            f.selected,
        )
        .is_err()
    );
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn deferred_outcome_sql_failure_rolls_back_hold_and_preparation_together() {
    let mut f = claimed_fixture();
    f.store
        .connection
        .execute_batch(
            "CREATE TEMP TRIGGER refuse_deferred BEFORE INSERT ON objects
         WHEN NEW.object_kind='task_sync_exchange_event'
           AND json_extract(NEW.canonical_json,'$.transition.transition')='planning_deferred'
         BEGIN SELECT RAISE(ABORT,'injected deferred failure'); END;",
        )
        .unwrap();
    let before = state(&f.store);
    let error = f.apply().unwrap_err();
    assert!(
        error.to_string().contains("injected deferred failure"),
        "{error}"
    );
    assert_eq!(state(&f.store), before);
    f.store
        .connection
        .execute_batch("DROP TRIGGER refuse_deferred")
        .unwrap();
    let original = f.apply().unwrap();
    assert!(matches!(
        original.event.transition,
        Transition::PlanningDeferred { .. }
    ));
    assert_eq!(f.apply().unwrap(), original);
    assert!(f.store.verify_all().unwrap().is_healthy());
}
