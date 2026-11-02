use super::*;
use crate::storage::work::test_support::{child, delegated};
use crate::{ChildRequirement, DecomposeWorkRequest, DisposeWorkRequest, WorkDisposition};

pub(super) fn assert_receipt_links(
    store: &SqliteStore,
    id: &ObjectId,
    original: &Event,
    foreign: WorkId,
) {
    let healthy = state(store);
    for fault in ["before", "hold", "terminal", "local"] {
        let mut event = original.clone();
        let Transition::ReconciledImportApplied { receipt, .. } = &mut event.transition else {
            panic!("fixture receipt");
        };
        match fault {
            "before" => {
                let mut basis = receipt.before[0].clone();
                basis.work_id = foreign;
                receipt.before = vec![basis];
            }
            "hold" => receipt.holds.push(ObjectId::mint()),
            "terminal" => receipt.terminal_evidence.push(ObjectId::mint()),
            "local" => receipt.tasks[0].local_record_ids.push(ObjectId::mint()),
            _ => unreachable!(),
        }
        store
            .connection
            .execute_batch("SAVEPOINT receipt_fault")
            .unwrap();
        let object = CanonicalObject::identified(id, &event).unwrap();
        store
            .connection
            .execute(
                "UPDATE objects SET canonical_json=?1 WHERE object_id=?2",
                params![object.bytes(), id.as_str()],
            )
            .unwrap();
        let broken = state(store);
        assert!(
            crate::storage::work::task_sync::ledger::validate_exchange_on(&store.connection)
                .is_err(),
            "{fault} link must refuse"
        );
        assert!(
            !store.verify_all().unwrap().is_healthy(),
            "doctor must reject {fault}"
        );
        assert_eq!(state(store), broken, "historical validation is read only");
        store
            .connection
            .execute_batch("ROLLBACK TO receipt_fault; RELEASE receipt_fault")
            .unwrap();
        assert_eq!(state(store), healthy);
    }
}

pub(super) fn assert_shared_record_links(
    store: &SqliteStore,
    version_id: &ObjectId,
    receipt_id: &ObjectId,
    original_receipt: &Event,
    foreign: WorkId,
) {
    let original: SharedImportVersion =
        SqliteStore::get_typed_object_on(&store.connection, version_id, SHARED_VERSION_KIND)
            .unwrap()
            .unwrap();
    let own_record = original.original_restored_record.clone().unwrap();
    let foreign_item = store.get_work_item(foreign).unwrap();
    let foreign_origin: ImportOrigin = SqliteStore::get_typed_object_on(
        &store.connection,
        &super::super::super::origin_reference(&foreign_item.created_by).unwrap(),
        ORIGIN_KIND,
    )
    .unwrap()
    .unwrap();
    let healthy = state(store);
    for fault in [
        "valid",
        "empty-receipt",
        "duplicate",
        "dangling",
        "foreign-task",
        "foreign-project",
        "wrong-kind",
        "bound",
        "receipt-extra",
    ] {
        store
            .connection
            .execute_batch("SAVEPOINT shared_record_fault")
            .unwrap();
        let mut version = original.clone();
        let mut event = original_receipt.clone();
        let Transition::ReconciledImportApplied { receipt, .. } = &mut event.transition else {
            panic!("fixture receipt");
        };
        let local = &mut receipt.tasks[0].local_record_ids;
        version.shared_records = vec![own_record.clone()];
        *local = version.shared_records.clone();
        let expected = match fault {
            "valid" => "",
            "empty-receipt" => {
                local.clear();
                "records differ from receipt"
            }
            "receipt-extra" => {
                version.shared_records.clear();
                "records differ from receipt"
            }
            "duplicate" => {
                version.shared_records.push(own_record.clone());
                *local = version.shared_records.clone();
                "records differ from receipt or are repeated"
            }
            "bound" => {
                version.shared_records = vec![own_record.clone(); super::super::MAX_RECORDS + 1];
                "records exceed supported bound"
            }
            "foreign-project" => {
                let mut record: crate::RestoredRecord = SqliteStore::get_typed_object_on(
                    &store.connection,
                    &own_record,
                    "work_restored_record",
                )
                .unwrap()
                .unwrap();
                record.project_id = ProjectId("another-project".into());
                let object = CanonicalObject::identified(&own_record, &record).unwrap();
                store
                    .connection
                    .execute(
                        "UPDATE objects SET canonical_json=?1 WHERE object_id=?2",
                        params![object.bytes(), own_record.as_str()],
                    )
                    .unwrap();
                "local restored record scope differs"
            }
            _ => {
                version.shared_records = vec![match fault {
                    "dangling" => ObjectId::mint(),
                    "foreign-task" => foreign_origin.record_id.clone(),
                    "wrong-kind" => original.binding.clone(),
                    _ => unreachable!(),
                }];
                *local = version.shared_records.clone();
                if fault == "foreign-task" {
                    "local restored record scope differs"
                } else {
                    "local record kind or existence differs"
                }
            }
        };
        for (id, bytes) in [
            (
                version_id,
                CanonicalObject::identified(version_id, &version).unwrap(),
            ),
            (
                receipt_id,
                CanonicalObject::identified(receipt_id, &event).unwrap(),
            ),
        ] {
            store
                .connection
                .execute(
                    "UPDATE objects SET canonical_json=?1 WHERE object_id=?2",
                    params![bytes.bytes(), id.as_str()],
                )
                .unwrap();
        }
        let before_reads = state(store);
        let current = store.task_sync_imported_baseline_state(&version.project_id, version.task.id);
        let historical = versions::validate_version(&store.connection, version_id, &version);
        let exchange =
            crate::storage::work::task_sync::ledger::validate_exchange_on(&store.connection);
        if fault == "valid" {
            current.unwrap();
            historical.unwrap();
            exchange.unwrap();
            let report = store.verify_all().unwrap();
            assert!(report.is_healthy(), "{report:?}");
        } else {
            for error in [
                current.unwrap_err(),
                historical.unwrap_err(),
                exchange.unwrap_err(),
            ] {
                assert!(error.to_string().contains(expected), "{fault}: {error}");
            }
            let report = store.verify_all().unwrap();
            assert!(!report.is_healthy(), "doctor must reject {fault}");
        }
        assert_eq!(
            state(store),
            before_reads,
            "{fault} validation writes nothing"
        );
        store
            .connection
            .execute_batch("ROLLBACK TO shared_record_fault; RELEASE shared_record_fault")
            .unwrap();
        assert_eq!(state(store), healthy);
    }
}

fn refused<T: std::fmt::Debug>(
    store: &SqliteStore,
    before: &crate::storage::TestDatabaseShapeSnapshot,
    result: Result<T, StoreError>,
) {
    assert_eq!(
        crate::store_error_value(&result.unwrap_err())["error"]["code"],
        "task_sync_reconciliation_held"
    );
    assert_eq!(&state(store), before, "held admission must have no effects");
}

#[test]
fn reconciliation_decomposition_fences_an_existing_held_prerequisite() {
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let parent = store
        .create_work(
            &root_request("task-sync", "unheld", 1),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let prerequisite = store
        .create_work(
            &root_request("task-sync", "held", 1),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let (binding, prepared, intent) = prepared(&mut store, home.path());
    let held = hold(
        &store,
        binding,
        prepared,
        &intent,
        vec![prerequisite.work_id],
    );
    store.place_task_sync_reconciliation_hold(&held).unwrap();
    let before = state(&store);
    let result = store.decompose_work(
        &DecomposeWorkRequest {
            parent_id: parent.work_id,
            expected_parent_revision: parent.revision,
            children: vec![child("new", ChildRequirement::Optional, "new")],
            prerequisites: vec![crate::ChildWorkPrerequisite {
                work_key: "new".into(),
                prerequisite: crate::WorkDependencyRef::Existing(prerequisite.work_id),
            }],
            authority: delegated("task-sync", "planner"),
            actor: actor("planner"),
            idempotency_key: "held-endpoint".into(),
            created_at: at(8),
        },
        &DevelopmentNoopRedactor,
    );
    refused(&store, &before, result);
    assert_eq!(store.get_work_item(parent.work_id).unwrap(), parent);
}

#[test]
fn reconciliation_hold_fences_disposal_waiver_rejection_detach_and_planning() {
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let root = store
        .create_work(
            &root_request("task-sync", "root", 1),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let children = store
        .decompose_work(
            &DecomposeWorkRequest {
                parent_id: root.work_id,
                expected_parent_revision: root.revision,
                children: vec![
                    child("open", ChildRequirement::Required, "open"),
                    child("disposed", ChildRequirement::Required, "disposed"),
                    child("offered", ChildRequirement::Optional, "offered"),
                ],
                prerequisites: Vec::new(),
                authority: delegated("task-sync", "planner"),
                actor: actor("planner"),
                idempotency_key: "children".into(),
                created_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap()
        .children;
    let open = &children[0];
    let owned = claim(&mut store, open, "runner", "old-claim", 3, 10);
    let offer_claim = claim(
        &mut store,
        &children[2],
        "outgoing",
        "offered-claim",
        4,
        600,
    );
    let offer_request = crate::OfferWorkHandoffRequest {
        work_id: children[2].work_id,
        run_id: offer_claim.run_id,
        expected_work_revision: children[2].revision,
        from: offer_claim.holder.clone(),
        to: SessionId("recipient".into()),
        claim_id: offer_claim.claim_id,
        claim_fence: offer_claim.fence,
        ttl_seconds: 600,
        checkpoint_summary: "original offer before the hold".into(),
        actor: actor("outgoing"),
        idempotency_key: "original-offer".into(),
        offered_at: at(5),
    };
    let original_offer = store
        .offer_work_handoff(&offer_request, &DevelopmentNoopRedactor)
        .unwrap();
    store
        .dispose_work(
            &DisposeWorkRequest {
                work_id: children[1].work_id,
                expected_work_revision: children[1].revision,
                disposition: WorkDisposition::Cancelled,
                replacement_id: None,
                reason: "already disposed".into(),
                actor: actor("planner"),
                idempotency_key: "dispose-second".into(),
                disposed_at: at(4),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let (binding, prepared, intent) = prepared(&mut store, home.path());
    let held = hold(&store, binding, prepared, &intent, vec![root.work_id]);
    let hold_id = store.place_task_sync_reconciliation_hold(&held).unwrap();
    let root = store.get_work_item(root.work_id).unwrap();
    let before = state(&store);
    assert_eq!(
        store
            .offer_work_handoff(&offer_request, &DevelopmentNoopRedactor)
            .unwrap(),
        original_offer,
        "original mutation replay stays available while held"
    );
    assert_eq!(state(&store), before);
    let result = store.accept_work_handoff(
        &crate::AcceptWorkHandoffRequest {
            work_id: children[2].work_id,
            offer_id: original_offer.offer_id,
            to: SessionId("recipient".into()),
            actor: actor("recipient"),
            idempotency_key: "held-accept".into(),
            accepted_at: at(8),
        },
        &DevelopmentNoopRedactor,
    );
    refused(&store, &before, result);
    let result = store.decompose_work(
        &DecomposeWorkRequest {
            parent_id: root.work_id,
            expected_parent_revision: root.revision,
            children: vec![child("new", ChildRequirement::Optional, "new")],
            prerequisites: Vec::new(),
            authority: delegated("task-sync", "planner"),
            actor: actor("planner"),
            idempotency_key: "held-decompose".into(),
            created_at: at(8),
        },
        &DevelopmentNoopRedactor,
    );
    refused(&store, &before, result);
    let result = store.record_acceptance_evaluation(
        &crate::domain::RecordAcceptanceEvaluationRequest {
            project_id: root.project_id.clone(),
            work_id: root.work_id,
            expected_work_revision: root.revision,
            evaluated_through: 0,
            mode: crate::AcceptanceEvaluationMode::SameSession,
            execution_identity: None,
            parent_session: None,
            evaluator_model: None,
            source_basis: None,
            verdicts: vec![crate::domain::CriterionVerdictInput {
                criterion: 1,
                verdict: crate::AcceptanceVerdict::Fail,
                basis: crate::AcceptanceBasis::Judgment,
                rationale: "fresh evaluation is fenced".into(),
                evidence: Vec::new(),
            }],
            evaluator: actor("evaluator"),
            attempt_key: Some("held-evaluation".into()),
            recorded_at: at(8),
            supersedes: None,
        },
        &DevelopmentNoopRedactor,
    );
    refused(&store, &before, result);
    let result = store.dispose_work(
        &DisposeWorkRequest {
            work_id: open.work_id,
            expected_work_revision: open.revision,
            disposition: WorkDisposition::Cancelled,
            replacement_id: None,
            reason: "expired other holder is not permission".into(),
            actor: actor("importer"),
            idempotency_key: "held-dispose".into(),
            disposed_at: at(100),
        },
        &DevelopmentNoopRedactor,
    );
    refused(&store, &before, result);
    let result = store.reject_required_child(
        &crate::RejectRequiredChildRequest {
            work_id: open.work_id,
            expected_work_revision: open.revision,
            expected_parent_revision: Some(root.revision),
            reason: "held reject".into(),
            actor: actor("importer"),
            idempotency_key: "held-reject".into(),
            rejected_at: at(100),
        },
        &DevelopmentNoopRedactor,
    );
    refused(&store, &before, result);
    let result = store.waive_required_child(
        &crate::WaiveRequiredChildRequest {
            parent_id: root.work_id,
            child_id: children[1].work_id,
            expected_parent_revision: root.revision,
            reason: "held waiver".into(),
            actor: actor("importer"),
            idempotency_key: "held-waive".into(),
            waived_at: at(100),
        },
        &DevelopmentNoopRedactor,
    );
    refused(&store, &before, result);
    let result = store.detach_work(
        &crate::DetachWorkRequest {
            project_id: root.project_id.clone(),
            work_id: open.work_id,
            expected_work_revision: open.revision,
            reason: "held detach".into(),
            actor: actor("importer"),
            idempotency_key: "held-detach".into(),
            detached_at: at(100),
        },
        &DevelopmentNoopRedactor,
    );
    refused(&store, &before, result);
    let result = store.revise_work(
        &crate::ReviseWorkRequest {
            work_id: open.work_id,
            expected_revision: open.revision,
            patch: crate::WorkRevisionPatch {
                title: Some("held revision".into()),
                ..Default::default()
            },
            authority: crate::WorkPlanningAuthority::Project,
            actor: actor("planner"),
            idempotency_key: "held-revise".into(),
            updated_at: at(100),
        },
        &DevelopmentNoopRedactor,
    );
    refused(&store, &before, result);
    let result = store.offer_work_handoff(
        &crate::OfferWorkHandoffRequest {
            work_id: open.work_id,
            run_id: owned.run_id,
            expected_work_revision: open.revision,
            from: owned.holder.clone(),
            to: SessionId("recipient".into()),
            claim_id: owned.claim_id,
            claim_fence: owned.fence,
            ttl_seconds: 10,
            checkpoint_summary: "held offer".into(),
            actor: actor("runner"),
            idempotency_key: "held-offer".into(),
            offered_at: at(8),
        },
        &DevelopmentNoopRedactor,
    );
    refused(&store, &before, result);
    assert!(
        store
            .connection
            .execute(
                "DELETE FROM work_task_sync_reconciliation_active WHERE work_id=?1",
                [open.work_id.0.to_string()]
            )
            .is_err(),
        "missing active membership cannot permit a fresh claim"
    );
    assert_eq!(state(&store), before);
    assert_eq!(
        super::super::active::lookup(&store.connection, open.work_id).unwrap(),
        Some(hold_id)
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn reconciliation_zero_run_hold_fences_folder_scalar_removal_and_relation_endpoints() {
    use crate::storage::TaskSyncFolderApply;
    use crate::task_sync::progress::ExportProfile;
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let mut tree = candidate();
    let mut endpoint = tree
        .tasks
        .iter()
        .find(|task| task.lifecycle == crate::WorkLifecycle::Open && task.parent_id.is_none())
        .unwrap()
        .clone();
    endpoint.id = WorkId(uuid::Uuid::new_v4());
    endpoint.external_ref = None;
    endpoint.prerequisites.clear();
    let endpoint_id = endpoint.id;
    tree.tasks.push(endpoint);
    tree.tasks.sort_by_key(|task| task.id.0);
    let profile = ExportProfile {
        include_notes: tree.project.notes_included,
        origin_machine: None,
    };
    let basis = store
        .export_task_sync_folder(&tree.project.project_id, &profile)
        .unwrap();
    store
        .apply_task_sync_folder(
            TaskSyncFolderApply {
                project: &tree.project.project_id,
                actor: &actor("importer"),
                tree: &tree,
                export_basis: &basis.export_basis,
                profile: &profile,
                complete: true,
                now: at(3),
            },
            "bootstrap",
        )
        .unwrap();
    let selected = tree
        .tasks
        .iter()
        .find(|task| {
            task.id != endpoint_id
                && task.lifecycle == crate::WorkLifecycle::Open
                && task.parent_id.is_none()
        })
        .unwrap()
        .id;
    let (binding, prepared, intent) = prepared(&mut store, home.path());
    let held = hold(&store, binding, prepared, &intent, vec![selected]);
    assert!(held.affected.iter().all(|basis| basis.runs.is_empty()));
    store.place_task_sync_reconciliation_hold(&held).unwrap();
    let basis = store
        .export_task_sync_folder(&tree.project.project_id, &profile)
        .unwrap();
    let before = state(&store);
    let item = store.get_work_item(selected).unwrap();
    let result = store.dispose_work(
        &DisposeWorkRequest {
            work_id: selected,
            expected_work_revision: item.revision,
            disposition: WorkDisposition::Cancelled,
            replacement_id: None,
            reason: "held zero-run disposal".into(),
            actor: actor("importer"),
            idempotency_key: "zero-run-dispose".into(),
            disposed_at: at(9),
        },
        &DevelopmentNoopRedactor,
    );
    refused(&store, &before, result);
    for fault in ["scalar", "removal", "relation"] {
        let mut resolved = basis.tree.clone();
        match fault {
            "scalar" => {
                resolved
                    .tasks
                    .iter_mut()
                    .find(|task| task.id == selected)
                    .unwrap()
                    .title = "held title".into();
            }
            "removal" => resolved.tasks.clear(),
            "relation" => {
                resolved
                    .tasks
                    .iter_mut()
                    .find(|task| task.id == endpoint_id)
                    .unwrap()
                    .prerequisites
                    .push(selected);
            }
            _ => unreachable!(),
        }
        let error = store
            .apply_task_sync_folder(
                TaskSyncFolderApply {
                    project: &tree.project.project_id,
                    actor: &actor("importer"),
                    tree: &resolved,
                    export_basis: &basis.export_basis,
                    profile: &profile,
                    complete: true,
                    now: at(9),
                },
                fault,
            )
            .unwrap_err();
        assert!(
            error.to_string().contains("task_sync_reconciliation_held")
                || crate::store_error_value(&error)["error"]["code"]
                    == "task_sync_reconciliation_held",
            "{fault}: {error}"
        );
        assert_eq!(
            state(&store),
            before,
            "{fault} must preserve graph, heads, baseline and operation result"
        );
    }
}

#[test]
fn reconciliation_legacy_folder_heads_track_edits_and_removed_membership_without_native_adoption() {
    use crate::storage::TaskSyncFolderApply;
    use crate::task_sync::{progress::ExportProfile, reconciliation::ImportedBaseline};
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let mut tree = candidate();
    let mut task = tree
        .tasks
        .iter()
        .find(|task| task.parent_id.is_none() && task.lifecycle == crate::WorkLifecycle::Open)
        .unwrap()
        .clone();
    task.prerequisites.clear();
    task.deferred_until = None;
    let imported = task.id;
    tree.tasks = vec![task];
    let profile = ExportProfile {
        include_notes: false,
        origin_machine: None,
    };
    let initial = store
        .export_task_sync_folder(&tree.project.project_id, &profile)
        .unwrap();
    let apply =
        |store: &mut SqliteStore, tree: &crate::task_sync::TaskTree, basis: &str, key: &str| {
            store.apply_task_sync_folder(
                TaskSyncFolderApply {
                    project: &tree.project.project_id,
                    actor: &actor("importer"),
                    tree,
                    export_basis: basis,
                    profile: &profile,
                    complete: true,
                    now: at(10),
                },
                key,
            )
        };
    apply(&mut store, &tree, &initial.export_basis, "initial").unwrap();
    let item = store.get_work_item(imported).unwrap();
    let origin = super::super::super::origin_reference(&item.created_by).unwrap();
    let original: Vec<u8> = store
        .connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_id=?1",
            [origin.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    let native = store
        .create_work(
            &root_request("task-sync", "native", 11),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let current = store
        .export_task_sync_folder(&tree.project.project_id, &profile)
        .unwrap();
    let mut edited = current.tree.clone();
    edited
        .tasks
        .iter_mut()
        .find(|task| task.id == imported)
        .unwrap()
        .title = "latest admitted title".into();
    apply(&mut store, &edited, &current.export_basis, "edit").unwrap();
    assert!(
        matches!(store.task_sync_imported_baseline_state(&tree.project.project_id,imported).unwrap(),Some(ImportedBaseline::Present {head,task}) if head!=origin && task.title=="latest admitted title")
    );
    assert_eq!(
        store
            .task_sync_imported_baseline_state(&tree.project.project_id, native.work_id)
            .unwrap(),
        None
    );
    assert_eq!(
        store.get_work_item(native.work_id).unwrap(),
        native,
        "shared membership never replaces native work"
    );
    let current = store
        .export_task_sync_folder(&tree.project.project_id, &profile)
        .unwrap();
    let mut removed = current.tree.clone();
    removed.tasks.retain(|task| task.id != imported);
    apply(&mut store, &removed, &current.export_basis, "remove").unwrap();
    let removed_head = super::super::head_for_imported_task_on(
        &store.connection,
        &tree.project.project_id,
        imported,
    )
    .unwrap()
    .unwrap();
    assert!(
        matches!(store.task_sync_imported_baseline_state(&tree.project.project_id,imported).unwrap(),Some(ImportedBaseline::Removed {head}) if head==removed_head)
    );
    assert_eq!(
        store
            .task_sync_imported_baseline_state(&tree.project.project_id, native.work_id)
            .unwrap(),
        None
    );
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT canonical_json FROM objects WHERE object_id=?1",
                [origin.as_str()],
                |row| row.get::<_, Vec<u8>>(0)
            )
            .unwrap(),
        original,
        "original provenance bytes and identity stay frozen"
    );
    assert!(store.verify_all().unwrap().is_healthy());
    let before = state(&store);
    apply(&mut store, &removed, &current.export_basis, "remove").unwrap();
    assert_eq!(
        state(&store),
        before,
        "exact replay keeps heads and operation results"
    );
}

#[test]
fn reconciliation_retired_tree_reopens_root_then_children_as_unclaimed_generations() {
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let root = store
        .create_work(
            &root_request("task-sync", "root", 1),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let children = store
        .decompose_work(
            &DecomposeWorkRequest {
                parent_id: root.work_id,
                expected_parent_revision: root.revision,
                children: vec![
                    child("required", ChildRequirement::Required, "required"),
                    child("optional", ChildRequirement::Optional, "optional"),
                ],
                prerequisites: Vec::new(),
                authority: delegated("task-sync", "planner"),
                actor: actor("planner"),
                idempotency_key: "children".into(),
                created_at: at(2),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap()
        .children;
    let (binding, prepared, intent) = prepared(&mut store, home.path());
    let held = hold(&store, binding, prepared, &intent, vec![root.work_id]);
    let hold_id = store.place_task_sync_reconciliation_hold(&held).unwrap();
    retire_tree_fixture(&mut store, &hold_id, &held, &intent);
    assert!(store.verify_all().unwrap().is_healthy());
    let mut fresh_root = None;
    for id in std::iter::once(root.work_id).chain(children.iter().map(|child| child.work_id)) {
        let item = store.get_work_item(id).unwrap();
        let request = crate::ReopenWorkRequest {
            work_id: id,
            expected_work_revision: item.revision,
            reason: "fresh execution after import retirement".into(),
            actor: actor("operator"),
            idempotency_key: id.0.to_string(),
            reopened_at: at(10),
        };
        let run = store
            .reopen_work(&request, &DevelopmentNoopRedactor)
            .unwrap();
        assert_eq!(run.generation, 2);
        assert!(run.executor.is_none());
        assert!(
            query::load_work_claim_optional(&store.connection, run.run_id)
                .unwrap()
                .is_none()
        );
        if let Some(root_execution) = fresh_root {
            assert_eq!(run.root_execution_id, root_execution);
        } else {
            fresh_root = Some(run.root_execution_id);
        }
        assert_eq!(
            store
                .reopen_work(&request, &DevelopmentNoopRedactor)
                .unwrap(),
            run,
            "reopen replay keeps the same fresh generation"
        );
    }
    assert!(store.verify_all().unwrap().is_healthy());
}

fn retire_tree_fixture(
    store: &mut SqliteStore,
    hold_id: &ObjectId,
    held: &ReconciliationHold,
    intent: &Intent,
) {
    use crate::storage::work::{WorkEventDraft, feeds, planning, root_state};
    let tree = store.export_task_tree(&held.project_id).unwrap();
    super::super::super::folder::record_complete_baseline_for_test(store, &tree).unwrap();
    let planning_id:String=store.connection.query_row("SELECT object_id FROM work_feed_entries WHERE feed_kind='project' AND feed_id=?1 AND object_kind='task_sync_import_planning' ORDER BY position DESC LIMIT 1",[&held.project_id.0],|row|row.get(0)).unwrap();
    let receipt_id = ObjectId::mint();
    let tx = store.begin_work_mutation().unwrap();
    let mut terminals = Vec::new();
    let mut roots = HashSet::new();
    for basis in &held.affected {
        let mut item = query::load_work_item(&tx, basis.work_id).unwrap();
        let mut run = query::load_work_run(&tx, item.active_run_id.unwrap()).unwrap();
        let mut root = query::load_root_execution(&tx, run.root_execution_id).unwrap();
        if roots.insert(root.root_execution_id) {
            root.state = crate::RootExecutionState::Cancelled;
            root.revision += 1;
            root.updated_at = at(8);
            root_state::persist(&tx, &root).unwrap();
        }
        run.state = crate::WorkRunState::SupersededByImport;
        run.revision += 1;
        run.updated_at = at(8);
        item.active_run_id = None;
        item.revision += 1;
        item.updated_at = at(8);
        planning::persist_work_run(&tx, &run, 0).unwrap();
        planning::persist_work_item(&tx, &item).unwrap();
        terminals.push(
            feeds::append_work_event(
                &tx,
                &WorkEventDraft {
                    schema_version: 1,
                    project_id: item.project_id.clone(),
                    root_id: item.root_id,
                    work_id: item.work_id,
                    run_id: Some(run.run_id),
                    revision: item.revision,
                    work: item,
                    run: Some(run),
                    root_execution: Some(root),
                    claim: None,
                    handoff_offer: None,
                    blocker: None,
                    transition: WorkTransition::SupersededByImport {
                        hold: hold_id.clone(),
                        prepared: held.prepared.clone(),
                        receipt: receipt_id.clone(),
                        reason: "explicit fixture reconciliation".into(),
                    },
                    actor: actor("importer"),
                    created_at: at(8),
                },
            )
            .unwrap()
            .0,
        );
    }
    let receipt = ModelAImportReceipt {
        operation: intent.operation,
        operation_key: intent.key.clone(),
        candidate_fingerprint: intent.candidate.documents_fingerprint.clone(),
        base: ReconciliationBase::ExplicitUnbasedResolution {
            local_basis: intent.local_planning_basis.clone(),
            incoming_basis: intent.candidate.planning_fingerprint.clone(),
        },
        resolved: intent.commit.clone(),
        planning: planning_id.parse().unwrap(),
        before: held.affected.clone(),
        after_basis: super::super::super::baseline::head(&tx, &held.project_id)
            .unwrap()
            .unwrap()
            .inventory_root
            .parse()
            .unwrap(),
        tasks: Vec::new(),
        holds: vec![hold_id.clone()],
        terminal_evidence: terminals,
    };
    let event = Event {
        schema_version: 1,
        binding: held.binding.clone(),
        sequence: 3,
        predecessor: Some(held.prepared.clone()),
        actor: actor("importer"),
        recorded_at: at(8),
        transition: Transition::ReconciledImportApplied {
            prepared: held.prepared.clone(),
            receipt: Box::new(receipt),
        },
    };
    SqliteStore::insert_object(
        &tx,
        EVENT_KIND,
        &CanonicalObject::identified(&receipt_id, &event).unwrap(),
    )
    .unwrap();
    tx.execute(
        "UPDATE work_task_sync_reconciliation_holds SET resolution_id=?1 WHERE hold_id=?2",
        params![receipt_id.as_str(), hold_id.as_str()],
    )
    .unwrap();
    super::super::active::resolve_on(&tx, hold_id).unwrap();
    verify_fixture_write(&tx, &event);
    tx.commit().unwrap();
}

/// A receipt names its own delta, not a reconstructed current tree. Missing
/// and retired entries must refuse even when the retained root still matches.
pub(super) fn assert_receipt_delta(store: &SqliteStore, id: &ObjectId, event: &Event) {
    let Transition::ReconciledImportApplied { prepared, receipt } = &event.transition else {
        panic!("fixture receipt");
    };
    let (binding, prepared) = prepared_on(
        &store.connection,
        &event.binding,
        prepared,
        &ProjectId("task-sync".into()),
    )
    .unwrap();
    let Transition::Prepared { intent } = prepared.transition else {
        panic!("prepared intent");
    };
    let original = super::super::super::folder::retained_planning_on(
        &store.connection,
        &receipt.planning,
        &binding.project,
    )
    .unwrap();
    let healthy = state(store);
    for fault in ["absent", "retired", "root", "notes"] {
        let mut planning = original.clone();
        match fault {
            "absent" => {
                planning.changes.remove(&receipt.tasks[0].task);
            }
            "retired" => {
                planning.changes.insert(receipt.tasks[0].task, None);
            }
            "root" => {
                planning.inventory_root = CanonicalObject::freeze(&uuid::Uuid::new_v4())
                    .unwrap()
                    .key()
                    .as_str()
                    .into();
            }
            "notes" => {
                planning.profile.include_notes = !planning.profile.include_notes;
                planning.profile.origin_machine = planning
                    .profile
                    .include_notes
                    .then(|| "receipt-profile-fault".into());
            }
            _ => unreachable!(),
        }
        store
            .connection
            .execute_batch("SAVEPOINT delta_fault")
            .unwrap();
        let object = CanonicalObject::identified(&receipt.planning, &planning).unwrap();
        store
            .connection
            .execute(
                "UPDATE objects SET canonical_json=?1 WHERE object_id=?2",
                params![object.bytes(), receipt.planning.as_str()],
            )
            .unwrap();
        let broken = state(store);
        let error =
            versions::validate_receipt_on(&store.connection, id, &binding, &intent, receipt)
                .unwrap_err();
        let expected = if matches!(fault, "absent" | "retired") {
            "shared version differs from resolved graph"
        } else {
            "graph or notes profile differs"
        };
        assert!(error.to_string().contains(expected), "{fault}: {error}");
        assert_eq!(state(store), broken, "delta validation is read only");
        store
            .connection
            .execute_batch("ROLLBACK TO delta_fault; RELEASE delta_fault")
            .unwrap();
        assert_eq!(state(store), healthy);
    }
}

#[test]
fn reconciliation_keyed_head_read_does_not_scan_unselected_baselines_or_exchange_history() {
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let tree = candidate();
    let profile = crate::task_sync::progress::ExportProfile {
        include_notes: tree.project.notes_included,
        origin_machine: None,
    };
    let empty = store
        .export_task_sync_folder(&tree.project.project_id, &profile)
        .unwrap();
    store
        .apply_task_sync_folder(
            crate::storage::TaskSyncFolderApply {
                project: &tree.project.project_id,
                actor: &actor("importer"),
                tree: &tree,
                export_basis: &empty.export_basis,
                profile: &profile,
                complete: true,
                now: at(3),
            },
            "enroll-keyed",
        )
        .unwrap();
    let chosen = tree.tasks[0].id;
    let unselected = tree.tasks[1].id;
    let expected = store
        .task_sync_imported_baseline_state(&tree.project.project_id, chosen)
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE work_task_sync_baseline SET document=?1 WHERE project_id=?2 AND work_id=?3",
            params![
                b"invalid document".as_slice(),
                &tree.project.project_id.0,
                unselected.0.to_string()
            ],
        )
        .unwrap();
    let malformed =
        CanonicalObject::mint(&serde_json::json!({"unrelated": "invalid private exchange"}))
            .unwrap();
    SqliteStore::insert_object(&store.connection, EVENT_KIND, &malformed).unwrap();
    let before = state(&store);
    assert_eq!(
        store
            .task_sync_imported_baseline_state(&tree.project.project_id, chosen)
            .unwrap(),
        expected
    );
    assert!(
        store
            .task_sync_imported_baseline_state(&tree.project.project_id, unselected)
            .unwrap_err()
            .to_string()
            .contains("does not decode")
    );
    assert_eq!(
        state(&store),
        before,
        "selected reads never repair or inspect unrelated history"
    );
    assert!(
        !store.verify_all().unwrap().is_healthy(),
        "doctor retains the whole-store audit"
    );
}
