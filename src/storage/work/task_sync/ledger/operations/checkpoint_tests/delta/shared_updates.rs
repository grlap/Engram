use super::*;
use crate::storage::work::task_sync::baseline;
use crate::storage::work::test_support::{DevelopmentNoopRedactor, actor};
use crate::task_sync::activity::{ActivityNotice, ActivityState};

mod disclosure;

fn claimable() -> crate::task_sync::TaskTree {
    let mut tree = candidate();
    for task in &mut tree.tasks {
        if task.parent_id.is_none() && task.lifecycle == crate::WorkLifecycle::Open {
            task.assigned_to = None;
            task.deferred_until = None;
            task.prerequisites.clear();
        }
    }
    tree
}

fn add_activity(task: &mut crate::task_sync::TaskDocument) {
    let id = ObjectId::mint();
    task.activity.entry("remote".into()).or_default().insert(
        id.clone(),
        ActivityNotice {
            activity_id: id.clone(),
            task_id: task.id,
            origin_machine: "remote".into(),
            actor: crate::task_sync::SharedActor::from_actor(&actor("remote-executor")),
            state: ActivityState::Started,
            state_revision: 1,
            event_id: id,
        },
    );
}

fn claim_local(store: &mut SqliteStore, id: crate::WorkId, second: i64) -> crate::WorkClaim {
    let item = store.get_work_item(id).unwrap();
    store
        .claim_work(
            &crate::ClaimWorkRequest {
                work_id: id,
                expected_work_revision: item.revision,
                expected_run_id: item.active_run_id,
                holder: crate::SessionId("local".into()),
                ttl_seconds: 600,
                recovery_reason: None,
                actor: actor("local"),
                idempotency_key: "local-claim".into(),
                claimed_at: at(second),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap()
}

#[test]
fn shared_only_delta_applies_under_claims_before_or_after_cut_without_claiming_their_events() {
    for after_cut in [false, true] {
        let mut f = Fixture::with_setup(
            &claimable(),
            |tree| {
                let mut task = tree
                    .tasks
                    .iter()
                    .find(|task| {
                        task.parent_id.is_none() && task.lifecycle == crate::WorkLifecycle::Open
                    })
                    .unwrap()
                    .clone();
                add_activity(&mut task);
                BTreeMap::from([(task.id, Some(task))])
            },
            |store, id| {
                if !after_cut {
                    claim_local(store, id, 31);
                }
            },
        );
        if after_cut {
            claim_local(&mut f.store, f.selected, 35);
        }
        let before = f.store.get_work_item(f.selected).unwrap();
        let payload = f.input.payload.clone();
        let original = f.apply().unwrap();
        let Transition::DeltaImportApplied { receipt, .. } = &original.event.transition else {
            panic!("shared change was deferred");
        };
        assert!(
            receipt
                .tasks
                .iter()
                .all(|task| task.local_record_ids.is_empty())
        );
        assert_eq!(f.store.get_work_item(f.selected).unwrap(), before);
        let current = f
            .store
            .capture_task_documents(
                &f.input.binding.project,
                &f.input.binding.profile,
                [f.selected],
            )
            .unwrap();
        let crate::task_sync::CapturedTask::Present(current) = &current.documents[&f.selected]
        else {
            panic!("present task expected");
        };
        assert_eq!(
            current.as_ref(),
            payload.documents[&f.selected].as_ref().unwrap()
        );
        let baseline =
            baseline::entries(&f.store.connection, &f.input.binding.project, [f.selected]).unwrap();
        assert_eq!(
            baseline[&f.selected].document,
            payload.documents[&f.selected]
        );
        assert_eq!(f.input.payload, payload);
        assert_eq!(
            f.store
                .completed_task_sync_delta_import(&f.input.binding_id, &f.input.intent)
                .unwrap(),
            Some(original)
        );
        let health = f.store.verify_all().unwrap();
        assert!(health.is_healthy(), "{health:?}");
    }
}

#[test]
fn a_mixed_delta_defers_its_shared_members_with_its_governing_changes() {
    let mut f = Fixture::with_setup(
        &claimable(),
        |tree| {
            let mut task = tree
                .tasks
                .iter()
                .find(|task| {
                    task.parent_id.is_none() && task.lifecycle == crate::WorkLifecycle::Open
                })
                .unwrap()
                .clone();
            task.title.push_str(" resolved");
            add_activity(&mut task);
            BTreeMap::from([(task.id, Some(task))])
        },
        |store, id| {
            claim_local(store, id, 31);
        },
    );
    let before = f
        .store
        .export_task_sync_folder(&f.input.binding.project, &f.input.binding.profile)
        .unwrap();
    let old_head = baseline::head(&f.store.connection, &f.input.binding.project).unwrap();
    let completed = f.apply().unwrap();
    assert!(matches!(
        completed.event.transition,
        Transition::PlanningDeferred { .. }
    ));
    assert_eq!(
        f.store
            .export_task_sync_folder(&f.input.binding.project, &f.input.binding.profile)
            .unwrap(),
        before
    );
    assert_eq!(
        baseline::head(&f.store.connection, &f.input.binding.project).unwrap(),
        old_head
    );
    let health = f.store.verify_all().unwrap();
    assert!(health.is_healthy(), "{health:?}");
}

#[test]
#[allow(
    clippy::too_many_lines,
    reason = "one deferred workflow checks the complete mixed successor and its receipt"
)]
fn a_mixed_successor_applies_shared_members_after_normal_local_completion() {
    use crate::storage::work::test_support::{checkpoint, complete, evidence};
    let mut tree = claimable();
    tree.tasks
        .retain(|task| task.parent_id.is_none() && task.lifecycle == crate::WorkLifecycle::Open);
    tree.tasks.truncate(1);
    tree.tasks[0].acceptance_bindings.clear();
    tree.tasks[0].evaluation_mode = None;
    tree.project.notes_included = true;
    let mut independent = tree.tasks[0].clone();
    independent.id = crate::WorkId(uuid::Uuid::new_v4());
    independent.external_ref = None;
    independent.title = "Independent shared-only task".into();
    tree.tasks.push(independent);
    tree.tasks.sort_by_key(|task| task.id);
    let mut f = Fixture::with_setup(
        &tree,
        |tree| {
            let mut task = tree.tasks[0].clone();
            task.title.push_str(" resolved");
            add_activity(&mut task);
            let mut shared = tree.tasks[1].clone();
            shared.notes.push(crate::task_sync::notes::SharedNote {
                id: ObjectId::mint(),
                kind: crate::task_sync::notes::SharedNoteKind::Generic,
                body: "Independent shared-only note".into(),
                author: crate::task_sync::SharedActor::from_actor(&actor("peer")),
                recorded_at: at(35),
                origin_machine: "remote".into(),
            });
            BTreeMap::from([(task.id, Some(task)), (shared.id, Some(shared))])
        },
        |store, id| {
            claim_local(store, id, 31);
        },
    );
    let original = f.apply().unwrap();
    let Transition::PlanningDeferred { hold, .. } = &original.event.transition else {
        panic!("mixed candidate must defer as a whole");
    };
    let hold = f
        .store
        .task_sync_reconciliation_hold(hold)
        .unwrap()
        .unwrap();
    assert!(
        f.input
            .intent
            .candidate
            .changed
            .iter()
            .all(|id| hold.seeds.contains(id))
    );
    let claim = f.store.claim_fixture(f.selected);
    let item = f.store.get_work_item(f.selected).unwrap();
    let proof = evidence(&mut f.store, &item, &claim, "local", "proof", 41);
    checkpoint(
        &mut f.store,
        &item,
        &claim,
        "local",
        "checkpoint",
        42,
        std::slice::from_ref(&proof),
    );
    complete(&mut f.store, &item, &claim, "local", &proof, "complete", 43).unwrap();
    let completed = f
        .store
        .export_task_sync_folder(&f.input.binding.project, &f.input.binding.profile)
        .unwrap();
    let native_activity = completed
        .tree
        .tasks
        .iter()
        .find(|task| task.id == f.selected)
        .unwrap()
        .activity["fixture-host"]
        .clone();
    assert_eq!(native_activity.len(), 1);
    assert!(
        native_activity.values().all(|notice| {
            notice.state == ActivityState::Finished && notice.state_revision == 2
        })
    );
    let mut request = deferred::successor_request(&f, &original);
    request.intent.candidate.directory = retention::delta::candidate_directory(
        &f.input.binding.repository.git_directory,
        request.intent.operation,
    );
    let mut payload = f.input.payload.clone();
    payload.operation = request.intent.operation;
    payload.output.inventory_root = InventoryRoot::parse(
        &baseline::next_root(
            &f.store.connection,
            &f.input.binding.project,
            &payload.documents,
        )
        .unwrap(),
    )
    .unwrap();
    request.intent.candidate.output = payload.output.clone();
    request.intent.candidate.content_fingerprint =
        CanonicalObject::freeze(&payload).unwrap().key().clone();
    let retained = f
        .store
        .retain_task_sync_planning_successor(
            &f.input.binding_id,
            &request,
            &payload,
            at(70),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let applied = f
        .store
        .apply_task_sync_planning_successor(&f.input.binding, &retained, &DevelopmentNoopRedactor)
        .unwrap();
    let current = f
        .store
        .export_task_sync_folder(&f.input.binding.project, &f.input.binding.profile)
        .unwrap();
    let admitted = baseline::entries(
        &f.store.connection,
        &f.input.binding.project,
        payload.documents.keys().copied(),
    )
    .unwrap();
    for task in payload.documents.values().flatten() {
        assert_eq!(admitted[&task.id].document.as_ref(), Some(task));
        let mut expected = task.clone();
        if task.id == f.selected {
            expected
                .activity
                .insert("fixture-host".into(), native_activity.clone());
        }
        assert_eq!(
            current
                .tree
                .tasks
                .iter()
                .find(|actual| actual.id == task.id),
            Some(&expected)
        );
    }
    let Transition::PlanningSuccessorApplied { receipt, .. } = applied.event.transition else {
        panic!("successor receipt expected");
    };
    assert_eq!(receipt.tasks.len(), 2);
    let health = f.store.verify_all().unwrap();
    assert!(health.is_healthy(), "{health:?}");
}

#[test]
fn strict_returned_completion_keeps_exact_input_while_next_export_closes_native_episode() {
    let mut f = Fixture::with_store_changes(
        &claimable(),
        |store, tree| {
            let work = store
                .create_work(
                    &crate::storage::work::test_support::root_request(
                        &tree.project.project_id.0,
                        "new-native",
                        31,
                    ),
                    &DevelopmentNoopRedactor,
                )
                .unwrap();
            crate::storage::work::test_support::claim(
                store,
                &work,
                "expired",
                "short-claim",
                32,
                1,
            );
            let profile = crate::task_sync::progress::ExportProfile {
                include_notes: false,
                origin_machine: Some("local".into()),
            };
            let mut task = store
                .export_task_sync_folder(&tree.project.project_id, &profile)
                .unwrap()
                .tree
                .tasks
                .into_iter()
                .find(|task| task.id == work.work_id)
                .unwrap();
            task.lifecycle = crate::WorkLifecycle::Completed;
            task.completion = Some(crate::task_sync::SharedCompletion {
                summary: "Finished remotely".into(),
                completed_at: at(34),
                actor: crate::task_sync::SharedActor::from_actor(&actor("remote")),
            });
            BTreeMap::from([(task.id, Some(task))])
        },
        |_, _| {},
        |_, _, _| {},
    );
    let incoming = f.input.payload.documents[&f.selected].clone().unwrap();
    let original_payload = f.input.payload.clone();
    assert!(
        incoming.activity["local"]
            .values()
            .all(|notice| notice.state == ActivityState::Started)
    );
    let applied = f.apply().unwrap();
    let Transition::DeltaImportApplied { receipt, .. } = &applied.event.transition else {
        panic!("completion expected");
    };
    assert_ne!(receipt.tasks[0].local_record_ids, Vec::<ObjectId>::new());
    let baseline =
        baseline::entries(&f.store.connection, &f.input.binding.project, [f.selected]).unwrap();
    assert_eq!(baseline[&f.selected].document.as_ref(), Some(&incoming));
    assert_eq!(f.input.payload, original_payload);
    let outgoing = f
        .store
        .export_task_sync_folder(&f.input.binding.project, &f.input.binding.profile)
        .unwrap();
    let actual = outgoing
        .tree
        .tasks
        .iter()
        .find(|task| task.id == f.selected)
        .unwrap();
    assert!(crate::task_sync::shared::governing_equal(&incoming, actual));
    assert!(
        actual.activity["local"]
            .values()
            .all(|notice| notice.state == ActivityState::Abandoned)
    );
    assert_ne!(actual, &incoming);
    assert_eq!(
        f.store
            .completed_task_sync_delta_import(&f.input.binding_id, &f.input.intent)
            .unwrap(),
        Some(applied)
    );
    let health = f.store.verify_all().unwrap();
    assert!(health.is_healthy(), "{health:?}");
}
