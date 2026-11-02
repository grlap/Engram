use super::*;

/// A storage-only atomic writer fixture. Actual host settlement remains a
/// prerequisite of the later production apply owner, never inferred here.
fn append_version(
    store: &mut SqliteStore,
    home: &std::path::Path,
    work: WorkId,
    title: &str,
) -> ObjectId {
    let project = ProjectId("task-sync".into());
    let mut tree = super::super::super::baseline::load(&store.connection, &project)
        .unwrap()
        .unwrap()
        .tree;
    let task = tree.tasks.iter_mut().find(|task| task.id == work).unwrap();
    task.title = title.into();
    let task = task.clone();
    let (binding, prepared, intent) = prepared_tree(store, home, &tree);
    let before = store
        .task_sync_reconciliation_basis(&project, &[work])
        .unwrap();
    let local_before = before
        .iter()
        .find(|basis| basis.work_id == work)
        .unwrap()
        .clone();
    let original =
        super::super::super::origin_reference(&store.get_work_item(work).unwrap().created_by)
            .unwrap();
    let origin: ImportOrigin =
        SqliteStore::get_typed_object_on(&store.connection, &original, ORIGIN_KIND)
            .unwrap()
            .unwrap();
    let receipt_id = ObjectId::mint();
    let version_id = ObjectId::mint();
    let base = ReconciliationBase::ExplicitUnbasedResolution {
        local_basis: intent.local_planning_basis.clone(),
        incoming_basis: intent.candidate.planning_fingerprint.clone(),
    };
    let version = SharedImportVersion {
        schema_version: 1,
        project_id: project.clone(),
        task: task.clone(),
        predecessor: local_before.imported_head.clone(),
        original_import_origin: Some(original),
        original_restored_record: Some(origin.record_id),
        binding: binding.clone(),
        prepared: prepared.clone(),
        receipt: receipt_id.clone(),
        base: base.clone(),
        local_before,
        shared_records: Vec::new(),
        actor: intent.actor.clone(),
        imported_at: at(8),
    };
    let previous = super::super::super::baseline::head(&store.connection, &project)
        .unwrap()
        .unwrap();
    let tx = store.begin_work_mutation().unwrap();
    let planning = super::super::super::baseline::append(
        &tx,
        &project,
        Some(previous.record),
        BTreeMap::from([(work, Some(task))]),
        &previous.export_basis,
        &crate::task_sync::progress::ExportProfile {
            include_notes: false,
            origin_machine: None,
        },
        at(8),
    )
    .unwrap();
    let after_basis = super::super::super::baseline::head(&tx, &project)
        .unwrap()
        .unwrap()
        .inventory_root
        .parse()
        .unwrap();
    let prepared_event: Event = SqliteStore::get_typed_object_on(&tx, &prepared, EVENT_KIND)
        .unwrap()
        .unwrap();
    let event = Event {
        schema_version: 1,
        binding,
        sequence: prepared_event.sequence + 1,
        predecessor: Some(prepared.clone()),
        actor: intent.actor.clone(),
        recorded_at: at(8),
        transition: Transition::ReconciledImportApplied {
            prepared,
            receipt: Box::new(ModelAImportReceipt {
                operation: intent.operation,
                operation_key: intent.key.clone(),
                candidate_fingerprint: intent.candidate.documents_fingerprint.clone(),
                base,
                resolved: intent.commit.clone(),
                planning,
                before,
                after_basis,
                tasks: vec![ReconciledTask {
                    task: work,
                    previous_head: version.predecessor.clone(),
                    shared_version: version_id.clone(),
                    local_record_ids: Vec::new(),
                }],
                holds: Vec::new(),
                terminal_evidence: Vec::new(),
            }),
        },
    };
    let object = CanonicalObject::identified(&version_id, &version).unwrap();
    SqliteStore::insert_object(&tx, SHARED_VERSION_KIND, &object).unwrap();
    crate::storage::work::feeds::append_to_project_feed(
        &tx,
        &project,
        SHARED_VERSION_KIND,
        &object,
    )
    .unwrap();
    SqliteStore::insert_object(
        &tx,
        EVENT_KIND,
        &CanonicalObject::identified(&receipt_id, &event).unwrap(),
    )
    .unwrap();
    tx.execute(
        "UPDATE work_task_sync_import_heads SET head_id=?1 WHERE project_id=?2 AND work_id=?3",
        params![version_id.as_str(), &project.0, work.0.to_string()],
    )
    .unwrap();
    verify_fixture_write(&tx, &event);
    tx.commit().unwrap();
    let report = store.verify_all().unwrap();
    assert!(report.is_healthy(), "{report:?}");
    version_id
}

#[test]
fn current_shared_head_skips_historical_deltas_and_versions_but_doctor_audits_them() {
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let mut tree = candidate();
    let template = tree
        .tasks
        .iter()
        .find(|task| task.parent_id.is_none() && task.lifecycle == crate::WorkLifecycle::Open)
        .unwrap()
        .clone();
    let work = template.id;
    for i in 0..256 {
        let mut task = template.clone();
        task.id = WorkId(uuid::Uuid::new_v4());
        task.external_ref = None;
        task.prerequisites.clear();
        task.title = format!("unrelated member {i}");
        tree.tasks.push(task);
    }
    tree.tasks.sort_by_key(|task| task.id.0);
    let project = tree.project.project_id.clone();
    let profile = crate::task_sync::progress::ExportProfile {
        include_notes: false,
        origin_machine: None,
    };
    let empty = store.export_task_sync_folder(&project, &profile).unwrap();
    store
        .apply_task_sync_folder(
            crate::storage::TaskSyncFolderApply {
                project: &project,
                actor: &actor("importer"),
                tree: &tree,
                export_basis: &empty.export_basis,
                profile: &profile,
                complete: true,
                now: at(3),
            },
            "large-enrollment",
        )
        .unwrap();
    for task in &mut tree.tasks {
        if task.lifecycle == crate::WorkLifecycle::Open {
            task.title.push_str(" updated");
        }
    }
    let before = store.export_task_sync_folder(&project, &profile).unwrap();
    store
        .apply_task_sync_folder(
            crate::storage::TaskSyncFolderApply {
                project: &project,
                actor: &actor("importer"),
                tree: &tree,
                export_basis: &before.export_basis,
                profile: &profile,
                complete: true,
                now: at(4),
            },
            "large-delta",
        )
        .unwrap();
    let large_delta = super::super::super::baseline::head(&store.connection, &project)
        .unwrap()
        .unwrap()
        .record;
    let first = append_version(&mut store, home.path(), work, "version one");
    let second = append_version(&mut store, home.path(), work, "version two");
    let current = append_version(&mut store, home.path(), work, "version three");
    let expected = store
        .task_sync_imported_baseline_state(&project, work)
        .unwrap();
    // Invalid old bytes make any accidental decoding or predecessor traversal
    // observable. The current record's own links remain intact and checked.
    store
        .connection
        .execute_batch("SAVEPOINT historical_fault")
        .unwrap();
    for id in [&large_delta, &first, &second] {
        store
            .connection
            .execute(
                "UPDATE objects SET canonical_json=?1 WHERE object_id=?2",
                params![b"invalid historical record".as_slice(), id.as_str()],
            )
            .unwrap();
    }
    let broken = state(&store);
    assert_eq!(
        store
            .task_sync_imported_baseline_state(&project, work)
            .unwrap(),
        expected
    );
    let version: SharedImportVersion =
        SqliteStore::get_typed_object_on(&store.connection, &current, SHARED_VERSION_KIND)
            .unwrap()
            .unwrap();
    assert!(matches!(
        versions::validate_version(&store.connection, &current, &version),
        Err(StoreError::Json(_))
    ));
    assert!(
        matches!(store.verify_all(), Err(StoreError::Json(_))),
        "doctor must refuse the malformed historical JSON"
    );
    assert_eq!(state(&store), broken, "both read paths are read only");
    store
        .connection
        .execute_batch("ROLLBACK TO historical_fault; RELEASE historical_fault")
        .unwrap();
    // Advancing a no-op delta after a shared version keeps its head, exactly
    // as the doctor's canonical derivation does.
    let previous =
        super::super::super::baseline::entries(&store.connection, &project, [work]).unwrap();
    let baseline = super::super::super::baseline::head(&store.connection, &project)
        .unwrap()
        .unwrap();
    let tx = store.begin_work_mutation().unwrap();
    super::super::super::baseline::append(
        &tx,
        &project,
        Some(baseline.record),
        BTreeMap::from([(work, Some(version.task))]),
        &baseline.export_basis,
        &profile,
        at(9),
    )
    .unwrap();
    super::super::heads::advance_on(&tx, &project, [work], &previous).unwrap();
    tx.commit().unwrap();
    assert_eq!(
        versions::imported_head_on(&store.connection, &project, work).unwrap(),
        Some(current)
    );
    let report = store.verify_all().unwrap();
    assert!(report.is_healthy(), "{report:?}");
}
