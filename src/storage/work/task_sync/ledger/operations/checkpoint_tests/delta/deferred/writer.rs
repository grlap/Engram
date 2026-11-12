use super::*;
use crate::storage::work::test_support::{checkpoint, complete, evidence};
use crate::task_sync::exchange::apply_planning_successor;

mod historical;
mod recovery;
#[cfg(windows)]
mod retired;

fn ended_fixture() -> (Fixture, CompletedExchange, crate::WorkRun) {
    ended_fixture_in(|_, _, _| {})
}

/// [`ended_fixture`] whose binding `repository` selects before activation.
fn ended_fixture_in(
    repository: impl FnOnce(&std::path::Path, &crate::task_sync::TaskTree, &mut Binding),
) -> (Fixture, CompletedExchange, crate::WorkRun) {
    let mut tree = candidate();
    let mut task = tree
        .tasks
        .iter()
        .find(|task| task.lifecycle == crate::WorkLifecycle::Open && task.parent_id.is_none())
        .unwrap()
        .clone();
    task.external_ref = None;
    task.assigned_to = None;
    task.deferred_until = None;
    task.prerequisites.clear();
    task.blockers.clear();
    task.acceptance_bindings.clear();
    task.evaluation_mode = None;
    task.notes.clear();
    task.activity.clear();
    tree.tasks = vec![task];
    let mut f = Fixture::with_repository(
        &tree,
        |tree| {
            let mut task = tree.tasks[0].clone();
            task.title = "original incoming title".into();
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
                        idempotency_key: "successor-writer-holder".into(),
                        claimed_at: at(31),
                    },
                    &DevelopmentNoopRedactor,
                )
                .unwrap();
        },
        repository,
    );
    let original = f.apply().unwrap();
    let claim = f.store.claim_fixture(f.selected);
    let item = f.store.get_work_item(f.selected).unwrap();
    let proof = evidence(
        &mut f.store,
        &item,
        &claim,
        "runner",
        "writer-original-proof",
        41,
    );
    checkpoint(
        &mut f.store,
        &item,
        &claim,
        "runner",
        "writer-original-checkpoint",
        41,
        std::slice::from_ref(&proof),
    );
    let item = f.store.get_work_item(f.selected).unwrap();
    complete(
        &mut f.store,
        &item,
        &claim,
        "runner",
        &proof,
        "writer-normal-end",
        42,
    )
    .unwrap();
    let run =
        crate::storage::work::query::load_work_run(&f.store.connection, claim.run_id).unwrap();
    assert_eq!(run.state, crate::WorkRunState::Completed);
    (f, original, run)
}

fn resolved(
    f: &Fixture,
    original: &CompletedExchange,
) -> (PlanningSuccessorRequest, DeltaCandidatePayload) {
    let mut request = successor_request(f, original);
    let mut payload = f.input.payload.clone();
    payload.operation = request.intent.operation;
    payload
        .documents
        .get_mut(&f.selected)
        .unwrap()
        .as_mut()
        .unwrap()
        .title = "resolved local planning title".into();
    payload.output.inventory_root = InventoryRoot::parse(
        &crate::storage::work::task_sync::baseline::next_root(
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
    (request, payload)
}

#[test]
fn successor_writer_replays_each_key_offline_and_preserves_original_execution() {
    let (mut f, original, old_run) = ended_fixture();
    let (request, payload) = resolved(&f, &original);
    let applied = apply_planning_successor(
        &mut f.store,
        &f.input.binding_id,
        &f.input.binding,
        &request,
        &payload,
        at(70),
        &DevelopmentNoopRedactor,
    )
    .unwrap();
    assert_ne!(applied.prepared, original.prepared);
    assert_ne!(applied.outcome, original.outcome);
    assert!(matches!(
        applied.event.transition,
        Transition::PlanningSuccessorApplied { .. }
    ));
    let item = f.store.get_work_item(f.selected).unwrap();
    assert_eq!(item.title, "resolved local planning title");
    assert_eq!(item.lifecycle, crate::WorkLifecycle::Open);
    let new_run = crate::storage::work::query::load_work_run(
        &f.store.connection,
        item.active_run_id.unwrap(),
    )
    .unwrap();
    assert_ne!(new_run.run_id, old_run.run_id);
    assert_eq!(new_run.state, crate::WorkRunState::Open);
    assert!(
        crate::storage::work::query::load_work_claim_optional(&f.store.connection, new_run.run_id)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        crate::storage::work::query::load_work_run(&f.store.connection, old_run.run_id).unwrap(),
        old_run
    );
    assert!(
        crate::storage::work::task_sync::reconciliation::holds::held_by_on(
            &f.store.connection,
            f.selected
        )
        .unwrap()
        .is_none()
    );
    let source = f
        .store
        .capture_task_sync_membership_component(
            &f.input.binding_id,
            crate::task_sync::TaskSyncMembershipView::Source,
            [f.selected],
        )
        .unwrap();
    assert_eq!(
        source.component.documents[&f.selected].title,
        "original incoming title"
    );
    assert_ne!(
        source.membership.inventory_root,
        payload.output.inventory_root
    );
    assert_eq!(f.apply().unwrap(), original);
    assert_resolved_family(&f, &applied);
    let integrity = f.store.verify_all().unwrap();
    assert!(integrity.is_healthy(), "{integrity:#?}");
    let file = f
        .input
        .binding
        .repository
        .git_directory
        .join("engram-task-sync-planning-successors")
        .join(format!("{}.json", request.intent.operation));
    std::fs::remove_file(file).unwrap();
    let before = state(&f.store);
    assert_eq!(
        apply_planning_successor(
            &mut f.store,
            &f.input.binding_id,
            &f.input.binding,
            &request,
            &payload,
            at(99),
            &DevelopmentNoopRedactor
        )
        .unwrap(),
        applied
    );
    assert_eq!(f.apply().unwrap(), original);
    assert_eq!(state(&f.store), before);
    let mut wrong = request;
    wrong.intent.key.push_str(" changed");
    assert!(
        f.store
            .completed_task_sync_planning_successor(&f.input.binding_id, &wrong)
            .is_err()
    );
}

fn assert_resolved_family(f: &Fixture, applied: &CompletedExchange) {
    use crate::task_sync::ledger::{CoverageMeaning, Direction, ExchangeFamily};
    let status = f
        .store
        .task_sync_family_frontiers(&f.input.binding_id)
        .unwrap();
    let row = status
        .families
        .iter()
        .find(|row| row.family == ExchangeFamily::Planning && row.direction == Direction::Incoming)
        .unwrap();
    assert_eq!(row.resolved_by_successor, 1);
    assert_eq!(row.deferred_requests, 0);
    assert_eq!(row.uncovered_requests, 0);
    let covered = row.covered.as_ref().unwrap();
    assert_eq!(covered.capture.prepared, applied.prepared);
    assert_eq!(covered.outcome, applied.outcome);
    assert_eq!(covered.meaning, CoverageMeaning::PlanningApplied);
}

#[test]
fn successor_writer_sql_failure_preserves_hold_and_reuses_retained_construction() {
    let (mut f, original, old_run) = ended_fixture();
    let (request, payload) = resolved(&f, &original);
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
    let before = state(&f.store);
    let frontiers = f
        .store
        .task_sync_family_frontiers(&f.input.binding_id)
        .unwrap();
    let item = f.store.get_work_item(f.selected).unwrap();
    f.store.connection.execute_batch("CREATE TEMP TRIGGER successor_baseline_failure BEFORE INSERT ON objects
        WHEN NEW.object_kind='task_sync_import_planning' BEGIN SELECT RAISE(ABORT,'injected successor baseline failure'); END;").unwrap();
    let error = f
        .store
        .apply_task_sync_planning_successor(&f.input.binding, &retained, &DevelopmentNoopRedactor)
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("injected successor baseline failure"),
        "{error}"
    );
    assert_eq!(state(&f.store), before);
    assert_eq!(
        f.store
            .task_sync_family_frontiers(&f.input.binding_id)
            .unwrap(),
        frontiers
    );
    assert_eq!(f.store.get_work_item(f.selected).unwrap(), item);
    assert_eq!(
        crate::storage::work::query::load_work_run(&f.store.connection, old_run.run_id).unwrap(),
        old_run
    );
    assert!(
        crate::storage::work::task_sync::reconciliation::holds::held_by_on(
            &f.store.connection,
            f.selected
        )
        .unwrap()
        .is_some()
    );
    f.store
        .connection
        .execute_batch("DROP TRIGGER successor_baseline_failure;")
        .unwrap();
    let replayed = retention::successor::read(&f.input.binding_id, &f.input.binding, &request)
        .unwrap()
        .unwrap();
    assert_eq!(replayed, retained);
    let applied = f
        .store
        .apply_task_sync_planning_successor(&f.input.binding, &replayed, &DevelopmentNoopRedactor)
        .unwrap();
    assert_eq!(applied.prepared, retained.recipe.prepared);
    assert_eq!(applied.outcome, retained.recipe.outcome);
    let integrity = f.store.verify_all().unwrap();
    assert!(integrity.is_healthy(), "{integrity:#?}");
}

/// A newer incoming request for the held task, at the current local baseline
/// and cut, under `operation`. With a `carrier` already fetched for that
/// operation, the request selects it. With none and `fetched`, a placeholder
/// commit selection is retained as a fetch leaves one. Its candidate is never
/// retained here: the store's incoming retention does that.
fn newer_incoming(
    f: &Fixture,
    title: &str,
    operation: uuid::Uuid,
    carrier: Option<crate::task_sync::ledger::RetainedCommit>,
    fetched: bool,
) -> InspectedDelta {
    let binding = &f.input.binding;
    let mut payload = f.input.payload.clone();
    payload.operation = operation;
    payload
        .documents
        .get_mut(&f.selected)
        .unwrap()
        .as_mut()
        .unwrap()
        .title = title.into();
    payload.output.inventory_root = InventoryRoot::parse(
        &crate::storage::work::task_sync::baseline::next_root(
            &f.store.connection,
            &binding.project,
            &payload.documents,
        )
        .unwrap(),
    )
    .unwrap();
    let head =
        crate::storage::work::task_sync::baseline::head(&f.store.connection, &binding.project)
            .unwrap()
            .unwrap();
    let cut = producing_origin::cut_on(&f.store.connection, &binding.project).unwrap();
    let fingerprint = CanonicalObject::freeze(&operation).unwrap().key().clone();
    let mut intent = f.input.intent.clone();
    intent.operation = operation;
    intent.key = operation.to_string();
    intent.local_cut.position.position = cut.position;
    intent.local_cut.anchor = cut.anchor;
    let DeltaBasis::Incoming {
        source_after,
        local_before,
        ..
    } = &mut intent.basis
    else {
        panic!("incoming basis expected")
    };
    let placeholder = carrier.is_none();
    source_after.commit = carrier.unwrap_or_else(|| {
        let oid = fingerprint.as_str()[..source_after.commit.oid.len()].to_owned();
        crate::task_sync::ledger::RetainedCommit {
            oid: oid.clone(),
            tree: oid,
            parents: source_after.commit.parents.clone(),
            retention_ref: format!("refs/engram/task-sync/exchange/{operation}/commit"),
            raw_bytes_fingerprint: fingerprint,
        }
    });
    source_after.membership = payload.output.clone();
    let commit = source_after.commit.clone();
    **local_before = DestinationPlanningBasis {
        baseline: head.record,
        membership: OutputMembership {
            inventory_root: InventoryRoot::parse(&head.inventory_root).unwrap(),
            profile: binding.profile.clone(),
        },
    };
    intent.candidate.operation = operation;
    intent.candidate.directory =
        retention::delta::candidate_directory(&binding.repository.git_directory, operation);
    intent.candidate.output = payload.output.clone();
    intent.candidate.content_fingerprint = CanonicalObject::freeze(&payload).unwrap().key().clone();
    if placeholder && fetched {
        std::fs::write(
            binding
                .repository
                .git_directory
                .join("engram-task-sync-commits")
                .join(format!("{operation}.json")),
            crate::canonical::canonical_bytes(&commit).unwrap(),
        )
        .unwrap();
    }
    InspectedDelta {
        binding_id: f.input.binding_id.clone(),
        binding: binding.clone(),
        intent,
        payload,
    }
}

/// A newer incoming request whose planning scope overlaps an active hold is
/// refused before anything is retained, so the earlier hold's successor still
/// resolves it; once the hold is resolved, the same request retains. The
/// refused request is checked before any fetch of its own: a carrier already
/// fetched for it would still need the separate audited abandonment.
#[test]
fn incoming_retention_refuses_an_active_hold_scope_before_retaining_and_the_hold_resolves() {
    let (mut f, original, _) = ended_fixture();
    let blocked = newer_incoming(
        &f,
        "newer incoming title",
        uuid::Uuid::new_v4(),
        None,
        false,
    );
    let before = state(&f.store);
    let error = f
        .store
        .retain_task_sync_incoming_delta(&blocked, at(60))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("held by reconciliation candidate"),
        "{error}"
    );
    assert_eq!(state(&f.store), before);
    assert!(!blocked.intent.candidate.directory.exists());

    let (request, payload) = resolved(&f, &original);
    let applied = apply_planning_successor(
        &mut f.store,
        &f.input.binding_id,
        &f.input.binding,
        &request,
        &payload,
        at(70),
        &DevelopmentNoopRedactor,
    )
    .unwrap();
    assert!(matches!(
        applied.event.transition,
        Transition::PlanningSuccessorApplied { .. }
    ));
    assert!(
        crate::storage::work::task_sync::reconciliation::holds::held_by_on(
            &f.store.connection,
            f.selected
        )
        .unwrap()
        .is_none()
    );

    let free = newer_incoming(&f, "newer incoming title", uuid::Uuid::new_v4(), None, true);
    let retained = f
        .store
        .retain_task_sync_incoming_delta(&free, at(80))
        .unwrap();
    assert_eq!(retained, free.intent.candidate);
    assert!(free.intent.candidate.directory.exists());
    let integrity = f.store.verify_all().unwrap();
    assert!(integrity.is_healthy(), "{integrity:#?}");
}

/// The supported recovery when a carrier was already fetched for a request
/// refused for its held scope: the refusal retains nothing, the fetched carrier
/// still holds custody from the earlier hold's successor, and its explicit
/// audited abandonment releases it without touching the hold or its task. The
/// successor then resolves the hold and a later request is fetched and retained.
#[cfg(windows)]
#[tokio::test]
async fn a_carrier_fetched_for_a_held_scope_refusal_is_abandoned_by_audit_and_the_hold_resolves() {
    use crate::storage::work::task_sync::reconciliation::holds::held_by_on;
    use crate::task_sync::exchange;
    // The fixture's recorded commits have SHA-256-width ids, and a delta may
    // not mix object formats, so the real carrier comes from a SHA-256 repository.
    let (mut f, original, _) = ended_fixture_in(|home, tree, binding| {
        super::super::super::data_bootstrap::real_repository_in(home, tree, binding, "sha256");
    });
    let (binding, id) = (f.input.binding.clone(), f.input.binding_id.clone());
    let mut abandon = audit_request(&f.store, &binding, &id, CustodySeedSelection::CustodyOnly);
    abandon.reason = CheckpointReason::NamedAudit;
    let head = crate::task_sync::data_worktree::DataWorktree::from_binding(&binding)
        .unwrap()
        .head
        .unwrap();
    let carrier = exchange::retain_commit(
        &binding.repository.main_checkout,
        abandon.operation,
        &head,
        exchange::RunLimits::default(),
        &exchange::Cancellation::default(),
    )
    .await
    .unwrap();
    abandon.seed = CustodySeedSelection::AbandonFetchedCommit {
        binding: id.clone(),
        commit: carrier.clone(),
    };
    let blocked = newer_incoming(
        &f,
        "newer incoming title",
        abandon.operation,
        Some(carrier),
        false,
    );
    let item = f.store.get_work_item(f.selected).unwrap();
    let hold = held_by_on(&f.store.connection, f.selected).unwrap();
    assert!(hold.is_some());
    let before = state(&f.store);
    let error = f
        .store
        .retain_task_sync_incoming_delta(&blocked, at(60))
        .unwrap_err();
    assert!(
        error
            .to_string()
            .contains("held by reconciliation candidate"),
        "{error}"
    );
    assert_eq!(state(&f.store), before);
    assert!(!blocked.intent.candidate.directory.exists());

    let (request, payload) = resolved(&f, &original);
    let error = apply_planning_successor(
        &mut f.store,
        &id,
        &binding,
        &request,
        &payload,
        at(65),
        &DevelopmentNoopRedactor,
    )
    .unwrap_err();
    assert!(
        error.to_string().contains("retained operations are ahead"),
        "{error}"
    );
    assert_eq!(state(&f.store), before);

    f.store
        .audit_task_sync_custody(&abandon, &binding.actor, at(70))
        .unwrap();
    assert_eq!(f.store.get_work_item(f.selected).unwrap(), item);
    assert_eq!(held_by_on(&f.store.connection, f.selected).unwrap(), hold);

    let (request, payload) = resolved(&f, &original);
    let applied = apply_planning_successor(
        &mut f.store,
        &id,
        &binding,
        &request,
        &payload,
        at(75),
        &DevelopmentNoopRedactor,
    )
    .unwrap();
    assert!(matches!(
        applied.event.transition,
        Transition::PlanningSuccessorApplied { .. }
    ));
    assert!(
        held_by_on(&f.store.connection, f.selected)
            .unwrap()
            .is_none()
    );

    let later = newer_incoming(&f, "later incoming title", uuid::Uuid::new_v4(), None, true);
    f.store
        .retain_task_sync_incoming_delta(&later, at(80))
        .unwrap();
    assert!(later.intent.candidate.directory.exists());
    let integrity = f.store.verify_all().unwrap();
    assert!(integrity.is_healthy(), "{integrity:#?}");
}
