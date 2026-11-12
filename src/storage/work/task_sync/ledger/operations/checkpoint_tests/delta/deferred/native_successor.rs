//! Native transitions use a genuinely deferred strict delta, never a forged capability.

mod catalog_limits;

use super::*;
use crate::storage::work::{
    completion::reopen_successor_on,
    planning::successor::{
        add_blocker_successor_on, apply_successor_effects_on, change_prerequisite_successor_on,
        clear_blocker_successor_on, plan_successor_revision_obligations_on, revise_successor_on,
        select_successor_effects_on,
    },
    query::{load_work_claim_optional, load_work_item, load_work_run},
    test_support::{checkpoint, complete, evidence},
};
use crate::{
    AddWorkBlockerRequest, ChangeWorkPrerequisiteRequest, ClearWorkBlockerRequest,
    ReopenWorkRequest, ReviseWorkRequest, WorkBlocker, WorkBlockerKind, WorkLifecycle,
    WorkPlanningAuthority, WorkRevisionPatch, WorkRunState,
};

fn native_fixture() -> Fixture {
    native_fixture_with_tree(false)
}

fn native_fixture_with_tree(required_child: bool) -> Fixture {
    native_fixture_with_options(required_child, false)
}

fn native_fixture_with_options(required_child: bool, source_deletion: bool) -> Fixture {
    let mut tree = candidate();
    let mut root = tree
        .tasks
        .iter()
        .find(|task| task.lifecycle == WorkLifecycle::Open && task.parent_id.is_none())
        .unwrap()
        .clone();
    root.external_ref = None;
    root.assigned_to = None;
    root.deferred_until = None;
    root.prerequisites.clear();
    root.blockers.clear();
    root.acceptance_bindings.clear();
    root.evaluation_mode = None;
    root.notes.clear();
    root.activity.clear();
    let mut peer = root.clone();
    peer.id = crate::WorkId::new();
    peer.title = "Independent prerequisite".into();
    tree.tasks = vec![root, peer];
    tree.tasks.sort_by_key(|task| task.id);
    if required_child {
        tree.tasks[1].parent_id = Some(tree.tasks[0].id);
        tree.tasks[1].child_requirement = crate::ChildRequirement::Required;
    }
    let child = required_child.then_some(tree.tasks[1].id);
    Fixture::with_setup(
        &tree,
        |tree| {
            tree.tasks
                .iter()
                .cloned()
                .map(|mut task| {
                    if source_deletion && task.id == tree.tasks[1].id {
                        return (task.id, None);
                    }
                    task.title.push_str(" incoming");
                    (task.id, Some(task))
                })
                .collect()
        },
        |store, selected| {
            for work in std::iter::once(selected).chain(child) {
                let item = store.get_work_item(work).unwrap();
                store
                    .claim_work(
                        &crate::ClaimWorkRequest {
                            work_id: work,
                            expected_work_revision: item.revision,
                            expected_run_id: item.active_run_id,
                            holder: crate::SessionId("runner".into()),
                            ttl_seconds: 600,
                            recovery_reason: Some("start local execution".into()),
                            actor: actor("runner"),
                            idempotency_key: format!("native-holder-{work:?}"),
                            claimed_at: at(31),
                        },
                        &DevelopmentNoopRedactor,
                    )
                    .unwrap();
            }
        },
    )
}

#[test]
fn native_successor_reopens_required_tree_ancestor_first_without_reusing_seals() {
    let mut f = native_fixture_with_tree(true);
    let original = f.apply().unwrap();
    let child = *f
        .input
        .intent
        .candidate
        .changed
        .iter()
        .find(|&&work| work != f.selected)
        .unwrap();
    let item = f.store.get_work_item(child).unwrap();
    let claim = load_work_claim_optional(&f.store.connection, item.active_run_id.unwrap())
        .unwrap()
        .unwrap();
    let proof = evidence(&mut f.store, &item, &claim, "runner", "child-proof", 41);
    checkpoint(
        &mut f.store,
        &item,
        &claim,
        "runner",
        "child-checkpoint",
        41,
        std::slice::from_ref(&proof),
    );
    complete(
        &mut f.store,
        &item,
        &claim,
        "runner",
        &proof,
        "child-complete",
        42,
    )
    .unwrap();
    let old_child = load_work_run(&f.store.connection, claim.run_id).unwrap();
    let root = f.store.get_work_item(f.selected).unwrap();
    let claim = load_work_claim_optional(&f.store.connection, root.active_run_id.unwrap())
        .unwrap()
        .unwrap();
    let proof = evidence(&mut f.store, &root, &claim, "runner", "root-proof", 43);
    checkpoint(
        &mut f.store,
        &root,
        &claim,
        "runner",
        "root-checkpoint",
        43,
        std::slice::from_ref(&proof),
    );
    let old_root_seal = complete(
        &mut f.store,
        &root,
        &claim,
        "runner",
        &proof,
        "root-complete",
        44,
    )
    .unwrap();
    assert!(
        old_root_seal
            .required_child_seals
            .contains(old_child.completion_seal.as_ref().unwrap())
    );
    let old_root = crate::storage::work::query::load_root_execution(
        &f.store.connection,
        old_child.root_execution_id,
    )
    .unwrap();
    let before = state(&f.store);
    let mut recipe = recipe(&f, &original);
    recipe.plan_reopen(child, false).unwrap();
    let child_request = ReopenWorkRequest {
        work_id: child,
        expected_work_revision: f.store.get_work_item(child).unwrap().revision,
        reason: "new child generation".into(),
        actor: actor("runner"),
        idempotency_key: "reopen-child".into(),
        reopened_at: at(47),
    };
    {
        let tx = f.store.begin_work_mutation().unwrap();
        let checked = crate::storage::work::task_sync::ledger::delta::successor::admit_on(
            &tx,
            &f.input.binding_id,
            &recipe,
        )
        .unwrap();
        assert!(
            reopen_successor_on(&checked, &child_request, &recipe.task_events[&child][0]).is_err()
        );
        let root_request = ReopenWorkRequest {
            work_id: f.selected,
            expected_work_revision: load_work_item(&tx, f.selected).unwrap().revision,
            reason: "new root generation".into(),
            actor: actor("runner"),
            idempotency_key: "reopen-root".into(),
            reopened_at: at(46),
        };
        let root_run =
            reopen_successor_on(&checked, &root_request, &recipe.task_events[&f.selected][0])
                .unwrap();
        let child_run =
            reopen_successor_on(&checked, &child_request, &recipe.task_events[&child][0]).unwrap();
        assert_eq!(child_run.root_execution_id, root_run.root_execution_id);
        assert_eq!(child_run.run_id, recipe.new_runs[&child]);
        let new_root =
            crate::storage::work::query::load_root_execution(&tx, root_run.root_execution_id)
                .unwrap();
        assert!(new_root.run_ids.contains(&root_run.run_id));
        assert!(new_root.run_ids.contains(&child_run.run_id));
        assert_eq!(new_root.required_child_seals, [] as [ObjectId; 0]);
        assert_eq!(
            new_root.required_child_waivers,
            [] as [crate::RequiredChildWaiver; 0]
        );
        assert_eq!(
            crate::storage::work::query::load_retained_root_execution(
                &tx,
                old_root.root_execution_id
            )
            .unwrap(),
            old_root
        );
        assert_eq!(load_work_run(&tx, old_child.run_id).unwrap(), old_child);
        assert!(child_run.executor.is_none());
        assert!(child_run.completion_seal.is_none());
    }
    assert_eq!(state(&f.store), before);
    assert!(f.store.verify_all().unwrap().is_healthy());
}

fn recipe(
    f: &Fixture,
    original: &crate::task_sync::ledger::CompletedExchange,
) -> PlanningSuccessorRecipe {
    let Transition::PlanningDeferred { hold, .. } = &original.event.transition else {
        panic!("a real deferred outcome is required")
    };
    let mut intent = f.input.intent.clone();
    intent.operation = uuid::Uuid::new_v4();
    intent.candidate.operation = intent.operation;
    intent.candidate.directory = retention::delta::candidate_directory(
        &f.input.binding.repository.git_directory,
        intent.operation,
    );
    intent.key = intent.operation.to_string();
    let cut = producing_origin::cut_on(&f.store.connection, &f.input.binding.project).unwrap();
    intent.local_cut.position.position = cut.position;
    intent.local_cut.anchor = cut.anchor;
    let request = PlanningSuccessorRequest {
        deferred: original.outcome.clone(),
        hold: hold.clone(),
        original_prepared: original.prepared.clone(),
        original_candidate: f.input.intent.candidate.content_fingerprint.clone(),
        intent,
    };
    let before = f
        .store
        .inspect_task_sync_planning_successor(&f.input.binding_id, &request)
        .unwrap();
    let mut recipe = PlanningSuccessorRecipe::new(request, before, &[]).unwrap();
    recipe
        .task_events
        .insert(f.selected, (0..6).map(|_| ObjectId::mint()).collect());
    recipe.plan_reopen(f.selected, true).unwrap();
    recipe
}

fn finish_holder(f: &mut Fixture) -> (crate::WorkRun, crate::CompletionSeal, ObjectId) {
    let item = f.store.get_work_item(f.selected).unwrap();
    let claim = load_work_claim_optional(&f.store.connection, item.active_run_id.unwrap())
        .unwrap()
        .unwrap();
    let evidence = evidence(&mut f.store, &item, &claim, "runner", "native-evidence", 41);
    checkpoint(
        &mut f.store,
        &item,
        &claim,
        "runner",
        "native-checkpoint",
        41,
        std::slice::from_ref(&evidence),
    );
    let seal = complete(
        &mut f.store,
        &item,
        &claim,
        "runner",
        &evidence,
        "native-complete",
        42,
    )
    .unwrap();
    let run = load_work_run(&f.store.connection, claim.run_id).unwrap();
    assert_eq!(run.state, WorkRunState::Completed);
    (run, seal, evidence)
}

fn reopen_request(f: &Fixture) -> ReopenWorkRequest {
    ReopenWorkRequest {
        work_id: f.selected,
        expected_work_revision: f.store.get_work_item(f.selected).unwrap().revision,
        reason: "apply retained planning after normal completion".into(),
        actor: actor("runner"),
        idempotency_key: "native-reopen".into(),
        reopened_at: at(44),
    }
}

#[test]
fn native_successor_reopens_and_plans_without_replacing_completion_evidence() {
    let mut f = native_fixture();
    let original = f.apply().unwrap();
    let (old_run, seal, evidence) = finish_holder(&mut f);
    let request = reopen_request(&f);
    let before_item = f.store.get_work_item(f.selected).unwrap();
    let before = state(&f.store);
    let error = f
        .store
        .reopen_work(&request, &DevelopmentNoopRedactor)
        .unwrap_err();
    assert!(
        matches!(
            &error,
            StoreError::TaskSyncRefused {
                refusal: crate::task_sync::TaskSyncRefusal::ReconciliationHeld,
                ..
            }
        ),
        "{error}"
    );
    assert_eq!(state(&f.store), before);
    let mut recipe = recipe(&f, &original);
    let ids = recipe.task_events[&f.selected].clone();
    let blocker = WorkBlocker {
        blocker_id: uuid::Uuid::new_v4().to_string(),
        work_id: f.selected,
        kind: WorkBlockerKind::ExternalInput,
        detail: "shared input is pending".into(),
        created_by: actor("foreign-planner"),
        created_at: at(12),
    };
    let blocker_record = ObjectId::mint();
    recipe.blocker_records.insert(
        f.selected,
        BTreeMap::from([(blocker.blocker_id.clone(), blocker_record.clone())]),
    );
    recipe.validate().unwrap();
    let peer = *recipe
        .task_events
        .keys()
        .find(|&&work| work != f.selected)
        .unwrap();
    {
        let tx = f.store.begin_work_mutation().unwrap();
        let checked = crate::storage::work::task_sync::ledger::delta::successor::admit_on(
            &tx,
            &f.input.binding_id,
            &recipe,
        )
        .unwrap();
        let run = reopen_successor_on(&checked, &request, &ids[0]).unwrap();
        assert_eq!(run.run_id, recipe.new_runs[&f.selected]);
        assert_eq!(
            run.root_execution_id,
            recipe.new_root_executions[&f.selected]
        );
        assert_eq!(run.generation, old_run.generation + 1);
        assert_eq!(run.state, WorkRunState::Open);
        assert!(run.executor.is_none());
        assert!(load_work_claim_optional(&tx, run.run_id).unwrap().is_none());
        let item = load_work_item(&tx, f.selected).unwrap();
        let item = revise_successor_on(
            &checked,
            &ReviseWorkRequest {
                work_id: f.selected,
                expected_revision: item.revision,
                patch: WorkRevisionPatch {
                    title: Some("Admitted native title".into()),
                    ..Default::default()
                },
                authority: WorkPlanningAuthority::Project,
                actor: actor("runner"),
                idempotency_key: "native-revise".into(),
                updated_at: at(45),
            },
            &ids[1],
        )
        .unwrap();
        assert_eq!(item.title, "Admitted native title");
        assert_eq!(item.created_at, before_item.created_at);
        assert_eq!(item.created_by, before_item.created_by);
        assert_eq!(item.origin, before_item.origin);
        let mut relation = ChangeWorkPrerequisiteRequest {
            work_id: f.selected,
            prerequisite_id: peer,
            expected_revision: item.revision,
            authority: WorkPlanningAuthority::Project,
            actor: actor("runner"),
            idempotency_key: "native-add-edge".into(),
            changed_at: at(46),
        };
        let item = change_prerequisite_successor_on(&checked, &relation, true, &ids[2]).unwrap();
        let edge_count: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM work_prerequisites WHERE work_id=?1 AND prerequisite_id=?2",
                [f.selected.0.to_string(), peer.0.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(edge_count, 1);
        relation.expected_revision = item.revision;
        relation.changed_at = at(47);
        let item = change_prerequisite_successor_on(&checked, &relation, false, &ids[3]).unwrap();
        let edge_count: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM work_prerequisites WHERE work_id=?1 AND prerequisite_id=?2",
                [f.selected.0.to_string(), peer.0.to_string()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(edge_count, 0);
        let stored = add_blocker_successor_on(
            &checked,
            &AddWorkBlockerRequest {
                work_id: f.selected,
                expected_work_revision: item.revision,
                kind: blocker.kind,
                detail: blocker.detail.clone(),
                authority: WorkPlanningAuthority::Project,
                actor: actor("runner"),
                idempotency_key: "native-block".into(),
                blocked_at: at(48),
            },
            &blocker,
            &blocker_record,
            &ids[4],
        )
        .unwrap();
        assert_eq!(stored, blocker);
        let item = load_work_item(&tx, f.selected).unwrap();
        clear_blocker_successor_on(
            &checked,
            &ClearWorkBlockerRequest {
                work_id: f.selected,
                expected_work_revision: item.revision,
                blocker_id: blocker.blocker_id.clone(),
                authority: WorkPlanningAuthority::Project,
                actor: actor("runner"),
                idempotency_key: "native-unblock".into(),
                cleared_at: at(49),
            },
            &ids[5],
        )
        .unwrap();
        for id in &ids {
            let kind: String = tx
                .query_row(
                    "SELECT object_kind FROM objects WHERE object_id=?1",
                    [id.as_str()],
                    |row| row.get(0),
                )
                .unwrap();
            assert_eq!(kind, "work_event");
        }
        assert_eq!(load_work_run(&tx, old_run.run_id).unwrap(), old_run);
        assert!(seal.evidence.contains(&evidence));
        let preserved: i64 = tx
            .query_row(
                "SELECT COUNT(*) FROM objects WHERE object_id=?1",
                [evidence.as_str()],
                |row| row.get(0),
            )
            .unwrap();
        assert_eq!(preserved, 1);
        // The combined writer owns hold resolution and the receipt. Dropping
        // this transaction proves the native cores never commit independently.
    }
    assert_eq!(state(&f.store), before);
    assert_eq!(f.store.get_work_item(f.selected).unwrap(), before_item);
    assert_eq!(
        load_work_run(&f.store.connection, old_run.run_id).unwrap(),
        old_run
    );
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn native_successor_waits_for_both_claim_and_run_to_end() {
    let mut f = native_fixture();
    let original = f.apply().unwrap();
    let active = recipe(&f, &original);
    let before = state(&f.store);
    {
        let tx = f.store.begin_work_mutation().unwrap();
        let error = crate::storage::work::task_sync::ledger::delta::successor::admit_on(
            &tx,
            &f.input.binding_id,
            &active,
        )
        .err()
        .unwrap();
        assert!(error.to_string().contains("active local claim"), "{error}");
    }
    assert_eq!(state(&f.store), before);
    let item = f.store.get_work_item(f.selected).unwrap();
    let claim = load_work_claim_optional(&f.store.connection, item.active_run_id.unwrap())
        .unwrap()
        .unwrap();
    f.store
        .release_work(
            &crate::ReleaseWorkRequest {
                work_id: f.selected,
                run_id: claim.run_id,
                expected_work_revision: item.revision,
                claim_id: claim.claim_id,
                claim_fence: claim.fence,
                holder: claim.holder,
                reason: "claim release is not run completion".into(),
                waiver_reason: Some("no contribution".into()),
                actor: actor("runner"),
                idempotency_key: "native-release".into(),
                released_at: at(41),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let unfinished = recipe(&f, &original);
    let before = state(&f.store);
    {
        let tx = f.store.begin_work_mutation().unwrap();
        let error = crate::storage::work::task_sync::ledger::delta::successor::admit_on(
            &tx,
            &f.input.binding_id,
            &unfinished,
        )
        .err()
        .unwrap();
        assert!(
            error.to_string().contains("unfinished local run"),
            "{error}"
        );
    }
    assert_eq!(state(&f.store), before);
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn native_successor_refuses_stale_basis_unplanned_event_and_sql_failure_atomically() {
    let mut f = native_fixture();
    let original = f.apply().unwrap();
    finish_holder(&mut f);
    let good = recipe(&f, &original);
    let request = reopen_request(&f);
    let before = state(&f.store);
    let mut stale = good.clone();
    stale
        .before
        .iter_mut()
        .find(|basis| basis.work_id == f.selected)
        .unwrap()
        .revision += 1;
    {
        let tx = f.store.begin_work_mutation().unwrap();
        assert!(
            crate::storage::work::task_sync::ledger::delta::successor::admit_on(
                &tx,
                &f.input.binding_id,
                &stale
            )
            .is_err()
        );
        let checked = crate::storage::work::task_sync::ledger::delta::successor::admit_on(
            &tx,
            &f.input.binding_id,
            &good,
        )
        .unwrap();
        assert!(reopen_successor_on(&checked, &request, &ObjectId::mint()).is_err());
    }
    assert_eq!(state(&f.store), before);
    f.store.connection.execute_batch("CREATE TEMP TRIGGER refuse_native_reopen BEFORE INSERT ON objects WHEN NEW.object_kind='work_event' AND json_extract(NEW.canonical_json,'$.transition.kind')='reopened' BEGIN SELECT RAISE(ABORT,'injected native failure'); END;").unwrap();
    {
        let tx = f.store.begin_work_mutation().unwrap();
        let checked = crate::storage::work::task_sync::ledger::delta::successor::admit_on(
            &tx,
            &f.input.binding_id,
            &good,
        )
        .unwrap();
        let error =
            reopen_successor_on(&checked, &request, &good.task_events[&f.selected][0]).unwrap_err();
        assert!(
            error.to_string().contains("injected native failure"),
            "{error}"
        );
    }
    assert_eq!(state(&f.store), before);
    f.store
        .connection
        .execute_batch("DROP TRIGGER refuse_native_reopen")
        .unwrap();
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn native_successor_binding_construction_is_read_only_and_consumes_exact_ids() {
    let mut f = native_fixture();
    let original = f.apply().unwrap();
    let (old_run, _, _) = finish_holder(&mut f);
    let mut retained = recipe(&f, &original);
    let request = reopen_request(&f);
    let patch = WorkRevisionPatch {
        acceptance_bindings: Some(vec![crate::domain::AcceptanceBinding {
            criterion: 1,
            requirement: crate::domain::VerificationRequirement {
                check_kind: crate::VerificationKind::Test,
                check_fingerprint: None,
            },
        }]),
        ..Default::default()
    };
    let before = state(&f.store);
    let mut projected = f.store.get_work_item(f.selected).unwrap();
    projected.lifecycle = WorkLifecycle::Open;
    projected.active_run_id = Some(retained.new_runs[&f.selected]);
    projected.revision += 1;
    let (definitions, resolutions) =
        plan_successor_revision_obligations_on(&f.store.connection, &projected, &patch).unwrap();
    assert_eq!(definitions.len(), 1);
    assert_eq!(
        resolutions,
        [] as [crate::task_sync::delta::PlannedSuccessorResolution; 0]
    );
    assert_eq!(state(&f.store), before);
    retained
        .obligation_definitions
        .insert(f.selected, definitions.clone());
    for fault in ["none", "missing", "extra", "foreign waiver"] {
        let mut recipe = retained.clone();
        match fault {
            "none" => {}
            "missing" => {
                recipe.obligation_definitions.clear();
            }
            "extra" => recipe
                .obligation_definitions
                .get_mut(&f.selected)
                .unwrap()
                .push(crate::task_sync::delta::PlannedSuccessorObligation {
                    obligation_id: crate::WorkObligationId::new(),
                    record: ObjectId::mint(),
                }),
            "foreign waiver" => {
                recipe.obligation_resolutions.insert(
                    f.selected,
                    vec![crate::task_sync::delta::PlannedSuccessorResolution {
                        obligation_id: crate::WorkObligationId::new(),
                        record: ObjectId::mint(),
                    }],
                );
            }
            _ => unreachable!(),
        }
        recipe.validate().unwrap();
        {
            let tx = f.store.begin_work_mutation().unwrap();
            let checked = crate::storage::work::task_sync::ledger::delta::successor::admit_on(
                &tx,
                &f.input.binding_id,
                &recipe,
            )
            .unwrap();
            let run = reopen_successor_on(&checked, &request, &recipe.task_events[&f.selected][0])
                .unwrap();
            let item = load_work_item(&tx, f.selected).unwrap();
            let result = revise_successor_on(
                &checked,
                &ReviseWorkRequest {
                    work_id: f.selected,
                    expected_revision: item.revision,
                    patch: patch.clone(),
                    authority: WorkPlanningAuthority::Project,
                    actor: actor("runner"),
                    idempotency_key: "retained-bound-revision".into(),
                    updated_at: at(45),
                },
                &recipe.task_events[&f.selected][1],
            );
            if fault == "none" {
                let item = result.unwrap();
                let stored: (String, String, i64) = tx.query_row(
                    "SELECT obligation_id, definition_id, work_revision FROM work_run_obligations WHERE run_id=?1",
                    [run.run_id.0.to_string()], |row| Ok((row.get(0)?,row.get(1)?,row.get(2)?)),
                ).unwrap();
                assert_eq!(
                    stored,
                    (
                        definitions[0].obligation_id.0.to_string(),
                        definitions[0].record.as_str().into(),
                        item.revision
                    )
                );
                assert_eq!(load_work_run(&tx, old_run.run_id).unwrap(), old_run);
            } else {
                assert!(result.is_err(), "{fault}");
            }
        }
        assert_eq!(state(&f.store), before, "{fault}");
    }
    assert!(f.store.verify_all().unwrap().is_healthy());
}

fn mapping_fixture(
    source_deletion: bool,
) -> (
    Fixture,
    PlanningSuccessorRecipe,
    DeltaCandidatePayload,
    crate::WorkRun,
) {
    let mut f = native_fixture_with_options(false, source_deletion);
    let original = f.apply().unwrap();
    let (old_run, _, _) = finish_holder(&mut f);
    let mut retained = recipe(&f, &original);
    let mut payload = f.input.payload.clone();
    payload.operation = retained.request.intent.operation;
    let captured = f
        .store
        .capture_task_documents(
            &payload.project_id,
            &payload.output.profile,
            payload.documents.keys().copied(),
        )
        .unwrap();
    for (&work, document) in &mut payload.documents {
        let crate::task_sync::CapturedTask::Present(current) = &captured.documents[&work] else {
            panic!("mapping fixture requires a present local task")
        };
        let mut current = current.as_ref().clone();
        if work == f.selected {
            current.lifecycle = WorkLifecycle::Open;
            current.completion = None;
            current.title = "Resolved successor title".into();
        }
        *document = Some(current);
    }
    payload.output.inventory_root = payload
        .documents
        .values()
        .flatten()
        .try_fold(
            crate::task_sync::inventory::InventoryRoot::ZERO,
            crate::task_sync::inventory::InventoryRoot::including,
        )
        .unwrap();
    retained.request.intent.candidate.output = payload.output.clone();
    retained.request.intent.candidate.content_fingerprint =
        CanonicalObject::freeze(&payload).unwrap().key().clone();
    // This test gives construction to the selector instead of the direct-core
    // helper's preallocated reopen and six events.
    retained.new_runs.clear();
    retained.new_root_executions.clear();
    for events in retained.task_events.values_mut() {
        *events = vec![ObjectId::mint()];
    }
    retained
        .bind_chain(original.outcome.clone(), original.event.sequence, at(44))
        .unwrap();
    (f, retained, payload, old_run)
}

#[test]
fn native_successor_mapping_selects_without_writes_and_applies_exact_operations() {
    let (mut f, mut retained, payload, old_run) = mapping_fixture(false);
    let before = state(&f.store);
    select_successor_effects_on(
        &f.store.connection,
        &f.input.binding.project,
        &payload,
        &mut retained,
    )
    .unwrap();
    assert_eq!(state(&f.store), before);
    assert_eq!(retained.task_events[&f.selected].len(), 2);
    assert!(
        retained
            .task_events
            .iter()
            .filter(|(work, _)| **work != f.selected)
            .all(|(_, events)| events.is_empty())
    );
    for extra in [false, true] {
        let mut selected = retained.clone();
        if extra {
            selected
                .task_events
                .get_mut(&f.selected)
                .unwrap()
                .push(ObjectId::mint());
        }
        {
            let tx = f.store.begin_work_mutation().unwrap();
            let checked = crate::storage::work::task_sync::ledger::delta::successor::admit_on(
                &tx,
                &f.input.binding_id,
                &selected,
            )
            .unwrap();
            let result = apply_successor_effects_on(&checked, &payload);
            if extra {
                assert!(result.is_err());
                assert_eq!(
                    crate::storage::test_database_shape_snapshot(&tx).unwrap(),
                    before
                );
            } else {
                result.unwrap();
                let item = load_work_item(&tx, f.selected).unwrap();
                assert_eq!(item.title, "Resolved successor title");
                assert_eq!(item.lifecycle, WorkLifecycle::Open);
                assert_eq!(item.active_run_id, Some(selected.new_runs[&f.selected]));
                assert_eq!(load_work_run(&tx, old_run.run_id).unwrap(), old_run);
            }
        }
        assert_eq!(state(&f.store), before);
    }
    let mut removed = payload.clone();
    removed.documents.insert(f.selected, None);
    let mut refused_recipe = retained.clone();
    assert!(
        select_successor_effects_on(
            &f.store.connection,
            &f.input.binding.project,
            &removed,
            &mut refused_recipe
        )
        .is_err()
    );
    assert_eq!(state(&f.store), before);
    assert!(f.store.verify_all().unwrap().is_healthy());
}

#[test]
fn native_successor_mapping_keeps_a_source_only_deletion_out_of_native_effects() {
    let (mut f, mut retained, mut payload, old_run) = mapping_fixture(true);
    let peer = *payload
        .documents
        .keys()
        .find(|&&work| work != f.selected)
        .unwrap();
    let peer_before = f.store.get_work_item(peer).unwrap();
    // The genuine original source deleted this member. The agent's resolved
    // local candidate retains it, so its source payload is not a local write.
    assert!(f.input.payload.documents[&peer].is_none());
    payload.documents.insert(peer, None);
    retained.request.intent.candidate.changed.remove(&peer);
    retained.task_events.remove(&peer);
    retained.request.intent.candidate.content_fingerprint =
        CanonicalObject::freeze(&payload).unwrap().key().clone();
    let before = state(&f.store);
    select_successor_effects_on(
        &f.store.connection,
        &f.input.binding.project,
        &payload,
        &mut retained,
    )
    .unwrap();
    assert_eq!(state(&f.store), before);
    assert_eq!(
        retained.task_events.keys().copied().collect::<Vec<_>>(),
        vec![f.selected]
    );
    assert!(!retained.new_runs.contains_key(&peer));
    {
        let tx = f.store.begin_work_mutation().unwrap();
        let checked = crate::storage::work::task_sync::ledger::delta::successor::admit_on(
            &tx,
            &f.input.binding_id,
            &retained,
        )
        .unwrap();
        apply_successor_effects_on(&checked, &payload).unwrap();
        assert_eq!(load_work_item(&tx, peer).unwrap(), peer_before);
        assert_eq!(
            load_work_item(&tx, f.selected).unwrap().title,
            "Resolved successor title"
        );
        assert_eq!(load_work_run(&tx, old_run.run_id).unwrap(), old_run);
    }
    assert_eq!(state(&f.store), before);
    assert_eq!(f.store.get_work_item(peer).unwrap(), peer_before);
    assert!(f.store.verify_all().unwrap().is_healthy());
}
