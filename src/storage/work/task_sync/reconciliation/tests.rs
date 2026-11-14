use super::*;
use crate::storage::work::{
    task_sync::tests::{candidate, ledger::binding, state},
    test_support::{DevelopmentNoopRedactor, actor, at, checkpoint, claim, root_request},
};
use crate::task_sync::{
    ledger::{Intent, RetainedCommit},
    reconciliation::{ModelAImportReceipt, ReconciledTask, ReconciliationBase},
};
use crate::{ClaimWorkRequest, ReleaseWorkRequest, SessionId};

mod corrections;
mod current_heads;
mod selected;
mod snapshots;

#[test]
fn reconciliation_scope_lookups_use_the_shared_index() {
    let home = crate::test_support::temp_home().unwrap();
    let store = SqliteStore::open(home.path().join("work.db")).unwrap();
    // Discover the index from its live definition, rather than pinning a query
    // plan or a separate hand-maintained identity for the schema object.
    let index: String = store.connection.query_row(
        "SELECT name FROM sqlite_schema WHERE type='index' AND tbl_name='objects' AND instr(sql,?1)>0 AND instr(sql,?2)>0",
        params![HOLD_KIND, EVENT_KIND],
        |row| row.get(0),
    ).unwrap();
    let sql = format!("EXPLAIN QUERY PLAN {}", super::lookup::SCOPED_RECORDS_SQL);
    for (kind, scope) in [(HOLD_KIND, "project"), (EVENT_KIND, "binding")] {
        let mut statement = store.connection.prepare(&sql).unwrap();
        let plan = statement
            .query_map(
                params![kind, scope, super::lookup::record_limit().unwrap()],
                |row| row.get::<_, String>(3),
            )
            .unwrap()
            .collect::<Result<Vec<_>, _>>()
            .unwrap();
        assert!(
            plan.iter()
                .any(|line| line.contains("SEARCH") && line.contains(&index)),
            "scoped lookup must use its expression index: {plan:?}"
        );
        assert!(
            plan.iter().all(|line| !line.contains("SCAN ")),
            "scoped lookup must not scan all objects: {plan:?}"
        );
    }
}

/// Storage-only setup of a retained incoming intent. This does not stand in for
/// the core's admitted owner registry, host fencing or settlement challenge.
fn prepared(store: &mut SqliteStore, home: &std::path::Path) -> (ObjectId, ObjectId, Intent) {
    let tree = store
        .export_task_tree(&ProjectId("task-sync".into()))
        .unwrap();
    prepared_tree(store, home, &tree)
}

fn prepared_tree(
    store: &mut SqliteStore,
    home: &std::path::Path,
    tree: &crate::task_sync::TaskTree,
) -> (ObjectId, ObjectId, Intent) {
    let mut binding = binding(home);
    binding.profile.include_notes = tree.project.notes_included;
    binding.profile.origin_machine = binding.profile.include_notes.then(|| "fixture-host".into());
    let existing: Option<String> = store
        .connection
        .query_row(
            "SELECT object_id FROM objects WHERE object_kind=?1 LIMIT 1",
            [BINDING_KIND],
            |row| row.get(0),
        )
        .optional()
        .unwrap();
    let binding_id = if let Some(id) = existing {
        let id: ObjectId = id.parse().unwrap();
        binding = SqliteStore::get_typed_object_on(&store.connection, &id, BINDING_KIND)
            .unwrap()
            .unwrap();
        id
    } else {
        store.activate_task_sync_binding(&binding).unwrap()
    };
    let events = store.task_sync_exchange_events(&binding_id).unwrap();
    let (predecessor, previous) = events.last().unwrap();
    let predecessor = predecessor.clone();
    let sequence = previous.sequence + 1;
    let recorded_at = at(6).max(previous.recorded_at);
    let operation = uuid::Uuid::new_v4();
    let fingerprint: ObjectId =
        crate::task_sync::progress::documents_fingerprint(&tree.documents().unwrap())
            .parse()
            .unwrap();
    let local: ObjectId = crate::task_sync::progress::documents_fingerprint(
        &store
            .export_task_tree(&binding.project)
            .unwrap()
            .documents()
            .unwrap(),
    )
    .parse()
    .unwrap();
    let producer = CanonicalObject::freeze(&"fixture producer")
        .unwrap()
        .key()
        .clone();
    std::fs::create_dir_all(&binding.repository.git_directory).unwrap();
    let candidate = crate::task_sync::retention::retain_prepared(
        &binding.repository.git_directory,
        operation,
        &crate::task_sync::progress::PreparedCandidate {
            documents: tree.documents().unwrap(),
            tree: tree.clone(),
            provenance: crate::task_sync::progress::PreparedProvenance {
                sequence: 1,
                operation,
                predecessor: None,
                store: binding.store.database.clone(),
                destination: home.join("prepared-output"),
                project: binding.project.clone(),
                profile: binding.profile.clone(),
                producer: producer.clone(),
                cut: binding.store.cut.clone(),
                omissions: crate::task_sync::notes::NoteOmissions::default(),
                activity_omissions: crate::task_sync::notes::NoteOmissions::default(),
            },
        },
    )
    .unwrap();
    let intent = Intent {
        operation,
        key: operation.to_string(),
        direction: Direction::Incoming,
        actor: actor("importer"),
        producer,
        expected_head: None,
        common_commit: None,
        candidate,
        commit: RetainedCommit {
            oid: fingerprint.as_str().into(),
            tree: fingerprint.as_str().into(),
            parents: Vec::new(),
            retention_ref: format!("refs/engram/task-sync/exchange/{operation}/commit"),
            raw_bytes_fingerprint: fingerprint.clone(),
        },
        local_planning_basis: local,
    };
    assert_eq!(
        crate::task_sync::retention::read_for_intent(&binding, &intent).unwrap(),
        tree.documents().unwrap()
    );
    let event = Event {
        schema_version: 1,
        binding: binding_id.clone(),
        sequence,
        predecessor: Some(predecessor),
        actor: intent.actor.clone(),
        recorded_at,
        transition: Transition::Prepared {
            intent: Box::new(intent.clone()),
        },
    };
    let object = CanonicalObject::mint(&event).unwrap();
    SqliteStore::insert_object(&store.connection, EVENT_KIND, &object).unwrap();
    super::super::ledger::view::rebuild(&store.connection).unwrap();
    (binding_id, object.key().clone(), intent)
}

fn hold(
    store: &SqliteStore,
    binding: ObjectId,
    prepared: ObjectId,
    intent: &Intent,
    mut seeds: Vec<WorkId>,
) -> ReconciliationHold {
    seeds.sort_by_key(|id| id.0);
    let project = ProjectId("task-sync".into());
    ReconciliationHold {
        schema_version: 1,
        project_id: project.clone(),
        binding,
        prepared,
        seeds: seeds.clone(),
        candidate_fingerprint: intent.candidate.documents_fingerprint.clone(),
        candidate_revision: 1,
        hold_revision: 1,
        scope_revision: 1,
        owner_set_revision: 1,
        association: ObjectId::mint(),
        affected: store
            .task_sync_reconciliation_basis(&project, &seeds)
            .unwrap(),
        non_run_owner_refs: Vec::new(),
        previous: None,
        actor: intent.actor.clone(),
        created_at: at(7),
    }
}

fn claim_request(item: &crate::WorkItem, key: &str, second: i64) -> ClaimWorkRequest {
    ClaimWorkRequest {
        work_id: item.work_id,
        expected_work_revision: item.revision,
        expected_run_id: item.active_run_id,
        holder: SessionId("runner".into()),
        ttl_seconds: 600,
        recovery_reason: Some("recover existing work".into()),
        actor: actor("runner"),
        idempotency_key: key.into(),
        claimed_at: at(second),
    }
}

fn assert_delivery(database: &std::path::Path, kind: &str) {
    use crate::work_service::{
        LocalWorkService, WorkChangeProjection, WorkNextQuery, WorkNextSection,
    };
    for session in ["importer", "peer"] {
        let service = LocalWorkService::new(
            database.into(),
            ProjectId("task-sync".into()),
            session.into(),
            SessionId(session.into()),
            None,
        );
        let query = WorkNextQuery {
            sections: vec![WorkNextSection::Changes],
            ..WorkNextQuery::default()
        };
        let peek = service
            .work_next_peek_for_agent(100, 100, true, query.clone(), at(20), |_| true)
            .unwrap();
        let next = service.work_next(100, query, at(21)).unwrap();
        if session == "peer" {
            for changes in [peek.changes, next.changes] {
                assert!(changes.unwrap().iter().any(|change| matches!(&change.delivery,WorkChangeProjection::Visible(summary) if summary.change_kind == kind)));
            }
        }
    }
}

fn verify_fixture_write(connection: &Connection, event: &Event) {
    super::super::ledger::view::rebuild(connection).unwrap();
    let binding: Binding =
        SqliteStore::get_typed_object_on(connection, &event.binding, BINDING_KIND)
            .unwrap()
            .unwrap();
    let Transition::ReconciledImportApplied { prepared, receipt } = &event.transition else {
        panic!("reconciled fixture outcome");
    };
    let (_, prepared) =
        prepared_on(connection, &event.binding, prepared, &binding.project).unwrap();
    let Transition::Prepared { intent } = prepared.transition else {
        panic!("prepared fixture");
    };
    let documents = crate::task_sync::retention::read_for_intent(&binding, &intent).unwrap();
    let candidate = crate::task_sync::TaskTree::from_stored_documents(&documents).unwrap();
    assert_eq!(
        intent.candidate.planning_fingerprint.as_str(),
        crate::task_sync::progress::documents_fingerprint(&candidate.documents().unwrap())
    );
    versions::verify_receipt_content_on(connection, &binding, &intent, receipt, &candidate)
        .unwrap();
}

#[test]
fn reconciliation_one_hold_many_slots_survives_restart_and_preserves_settlement() {
    let home = crate::test_support::temp_home().unwrap();
    let path = home.path().join("work.db");
    let mut store = SqliteStore::open(&path).unwrap();
    let first = store
        .create_work(
            &root_request("task-sync", "first", 1),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let second = store
        .create_work(
            &root_request("task-sync", "second", 2),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let owned = claim(&mut store, &first, "runner", "initial-claim", 3, 600);
    let (binding, prepared, intent) = prepared(&mut store, home.path());
    let held = hold(
        &store,
        binding,
        prepared,
        &intent,
        vec![first.work_id, second.work_id],
    );
    let id = store.place_task_sync_reconciliation_hold(&held).unwrap();
    let snapshot = state(&store);
    assert_eq!(
        store.place_task_sync_reconciliation_hold(&held).unwrap(),
        id
    );
    assert_eq!(state(&store), snapshot);
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT count(*) FROM work_task_sync_reconciliation_holds WHERE hold_id=?1",
                [id.as_str()],
                |row| row.get::<_, i64>(0)
            )
            .unwrap(),
        2
    );
    let error = store
        .claim_work(
            &claim_request(&second, "new-claim", 8),
            &DevelopmentNoopRedactor,
        )
        .unwrap_err();
    assert_eq!(
        crate::store_error_value(&error)["error"]["code"],
        "task_sync_reconciliation_held"
    );
    assert_eq!(state(&store), snapshot);
    // A current holder can still settle and release its OWN claim. No importer
    // revokes or moves it, and an unclaimed slot never becomes host proof.
    let renewed = store
        .claim_work(&claim_request(&first, "renew", 8), &DevelopmentNoopRedactor)
        .unwrap();
    assert_eq!(renewed.claim_id, owned.claim_id);
    checkpoint(&mut store, &first, &renewed, "runner", "settle", 9, &[]);
    store
        .release_work(
            &ReleaseWorkRequest {
                work_id: first.work_id,
                run_id: renewed.run_id,
                expected_work_revision: first.revision,
                holder: renewed.holder.clone(),
                claim_id: renewed.claim_id,
                claim_fence: renewed.fence,
                reason: "original holder settled".into(),
                waiver_reason: Some("no unfinished contribution".into()),
                actor: actor("runner"),
                idempotency_key: "release".into(),
                released_at: at(10),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    assert!(store.verify_all().unwrap().is_healthy());
    drop(store);
    let mut store = SqliteStore::open(&path).unwrap();
    assert_eq!(
        store.task_sync_reconciliation_hold(&id).unwrap(),
        Some(held)
    );
    assert!(
        store
            .claim_work(
                &claim_request(&first, "after-restart", 11),
                &DevelopmentNoopRedactor
            )
            .unwrap_err()
            .to_string()
            .contains("held by reconciliation")
    );
    drop(store);
    let exported = home.path().join("held-store.jsonl");
    let migrated = home.path().join("migrated.db");
    crate::storage::migration::export_json(&path, &exported).unwrap();
    crate::storage::migration::import_json(&exported, &migrated).unwrap();
    assert!(
        SqliteStore::repair_rebuildable_projections(&migrated)
            .unwrap()
            .is_healthy()
    );
    let mut store = SqliteStore::open(&migrated).unwrap();
    assert_eq!(
        store
            .task_sync_reconciliation_hold(&id)
            .unwrap()
            .unwrap()
            .candidate_fingerprint,
        intent.candidate.documents_fingerprint
    );
    let current = store.get_work_item(first.work_id).unwrap();
    assert_eq!(
        crate::store_error_value(
            &store
                .claim_work(
                    &claim_request(&current, "after-migration", 12),
                    &DevelopmentNoopRedactor
                )
                .unwrap_err()
        )["error"]["code"],
        "task_sync_reconciliation_held"
    );
    drop(store);
    assert_delivery(&migrated, HOLD_KIND);
}

#[test]
fn reconciliation_moved_or_cross_candidate_hold_has_no_effects() {
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let work = store
        .create_work(
            &root_request("task-sync", "native", 1),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let other_work = store
        .create_work(
            &root_request("task-sync", "other-native", 2),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let (binding, prepared, intent) = prepared(&mut store, home.path());
    let mut original = hold(&store, binding, prepared, &intent, vec![work.work_id]);
    let before = state(&store);
    let mut wrong = original.clone();
    wrong.candidate_fingerprint = CanonicalObject::freeze(&"other candidate")
        .unwrap()
        .key()
        .clone();
    assert!(
        store
            .place_task_sync_reconciliation_hold(&wrong)
            .unwrap_err()
            .to_string()
            .contains("prepared candidate")
    );
    assert_eq!(state(&store), before);
    let other_head: String = store
        .connection
        .query_row(
            "SELECT latest_event_id FROM work_items WHERE work_id=?1",
            [other_work.work_id.0.to_string()],
            |row| row.get(0),
        )
        .unwrap();
    let mut malformed = original.clone();
    malformed.affected[0].head = Some(other_head.parse().unwrap());
    assert!(
        store
            .place_task_sync_reconciliation_hold(&malformed)
            .unwrap_err()
            .to_string()
            .contains("historical work head scope differs")
    );
    assert_eq!(state(&store), before);
    store
        .revise_work(
            &crate::ReviseWorkRequest {
                work_id: work.work_id,
                expected_revision: work.revision,
                patch: crate::WorkRevisionPatch {
                    title: Some("native planning changed after capture".into()),
                    ..Default::default()
                },
                authority: crate::WorkPlanningAuthority::Project,
                actor: actor("planner"),
                idempotency_key: "move-local-basis".into(),
                updated_at: at(8),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let moved = state(&store);
    let error = store
        .place_task_sync_reconciliation_hold(&original)
        .unwrap_err();
    assert_eq!(
        crate::store_error_value(&error)["error"]["code"],
        "task_sync_divergent_planning"
    );
    assert_eq!(state(&store), moved);
    original.affected = store
        .task_sync_reconciliation_basis(&original.project_id, &original.seeds)
        .unwrap();
    original.created_at = at(9);
    let id = store
        .place_task_sync_reconciliation_hold(&original)
        .unwrap();
    let tx = store.begin_work_mutation().unwrap();
    let other = ObjectId::mint();
    let expected = holds::HoldExpectation {
        project: &original.project_id,
        hold: &id,
        prepared: &other,
        scope: &id,
        association: &original.association,
        candidate_revision: 1,
        hold_revision: 1,
        scope_revision: 1,
        owner_set_revision: 1,
    };
    assert!(matches!(
        holds::load_reconciliation_hold_on(&tx, &expected).unwrap(),
        holds::ReconciliationHoldCheck::Pending(holds::ReconciliationGap::BasisChanged)
    ));
    tx.rollback().unwrap();
}

#[test]
fn reconciliation_zero_run_hold_still_fences_restored_first_claim() {
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let tree = candidate();
    store
        .import_task_tree(
            &tree.project.project_id,
            &actor("importer"),
            &tree,
            false,
            at(3),
        )
        .unwrap();
    let selected = tree
        .tasks
        .iter()
        .find(|task| task.parent_id.is_none() && task.lifecycle == crate::WorkLifecycle::Open)
        .unwrap()
        .id;
    let (binding, prepared, intent) = prepared(&mut store, home.path());
    let held = hold(&store, binding, prepared, &intent, vec![selected]);
    assert!(held.affected.iter().all(|basis| basis.runs.is_empty()));
    let id = store.place_task_sync_reconciliation_hold(&held).unwrap();
    assert_eq!(
        store.task_sync_reconciliation_hold(&id).unwrap(),
        Some(held)
    );
    let work = store.get_work_item(selected).unwrap();
    let before = state(&store);
    assert_eq!(
        crate::store_error_value(
            &store
                .claim_work(
                    &claim_request(&work, "first-claim", 8),
                    &DevelopmentNoopRedactor
                )
                .unwrap_err()
        )["error"]["code"],
        "task_sync_reconciliation_held"
    );
    assert_eq!(state(&store), before);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn reconciliation_current_baseline_preserves_original_origin_and_native_overlay() {
    for unchanged in [false, true] {
        current_baseline_case(unchanged);
    }
}

fn current_baseline_case(unchanged: bool) {
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let tree = candidate();
    store
        .import_task_tree(
            &tree.project.project_id,
            &actor("importer"),
            &tree,
            false,
            at(3),
        )
        .unwrap();
    let work = tree
        .tasks
        .iter()
        .find(|task| task.parent_id.is_none() && task.lifecycle == crate::WorkLifecycle::Open)
        .unwrap()
        .id;
    let item = store.get_work_item(work).unwrap();
    let origin_id = super::super::origin_reference(&item.created_by).unwrap();
    let origin: ImportOrigin =
        SqliteStore::get_typed_object_on(&store.connection, &origin_id, ORIGIN_KIND)
            .unwrap()
            .unwrap();
    assert_eq!(
        versions::imported_head_on(&store.connection, &tree.project.project_id, work).unwrap(),
        Some(origin_id.clone())
    );
    let intact = state(&store);
    store
        .connection
        .execute(
            "DELETE FROM work_task_sync_import_heads WHERE work_id=?1",
            [work.0.to_string()],
        )
        .unwrap();
    let missing = state(&store);
    let profile = crate::task_sync::progress::ExportProfile {
        include_notes: tree.project.notes_included,
        origin_machine: None,
    };
    let exported = store
        .export_task_sync_folder(&tree.project.project_id, &profile)
        .unwrap();
    let refusal = store
        .apply_task_sync_folder(
            crate::storage::TaskSyncFolderApply {
                project: &tree.project.project_id,
                actor: &actor("importer"),
                tree: &exported.tree,
                export_basis: &exported.export_basis,
                profile: &profile,
                complete: true,
                now: at(5),
            },
            "missing-head-folder-apply",
        )
        .unwrap_err();
    assert!(
        refusal
            .to_string()
            .contains("origin-created task has no durable imported head"),
        "{refusal}"
    );
    assert_eq!(
        state(&store),
        missing,
        "folder apply cannot repair a missing durable head"
    );
    assert!(
        store
            .task_sync_imported_baseline_state(&tree.project.project_id, work)
            .unwrap_err()
            .to_string()
            .contains("no durable imported head")
    );
    assert!(!store.verify_all().unwrap().is_healthy());
    assert_eq!(
        state(&store),
        missing,
        "missing durable state is refused, never reseeded on read"
    );
    assert!(
        super::head_for_imported_task_on(&store.connection, &tree.project.project_id, work)
            .unwrap_err()
            .to_string()
            .contains("origin-created task has no durable imported head")
    );
    store
        .connection
        .execute(
            "INSERT INTO work_task_sync_import_heads(project_id,work_id,head_id) VALUES (?1,?2,?3)",
            params![
                &tree.project.project_id.0,
                work.0.to_string(),
                origin_id.as_str()
            ],
        )
        .unwrap();
    assert_eq!(state(&store), intact);
    let original_bytes: Vec<u8> = store
        .connection
        .query_row(
            "SELECT canonical_json FROM objects WHERE object_id=?1",
            [origin_id.as_str()],
            |row| row.get(0),
        )
        .unwrap();
    let exported = store
        .export_task_sync_folder(&tree.project.project_id, &profile)
        .unwrap();
    store
        .apply_task_sync_folder(
            crate::storage::TaskSyncFolderApply {
                project: &tree.project.project_id,
                actor: &actor("importer"),
                tree: &exported.tree,
                export_basis: &exported.export_basis,
                profile: &profile,
                complete: true,
                now: at(5),
            },
            "enroll-existing-origins",
        )
        .unwrap();
    let exported = store
        .export_task_sync_folder(&tree.project.project_id, &profile)
        .unwrap();
    let mut intermediate = exported.tree.clone();
    intermediate
        .tasks
        .iter_mut()
        .find(|task| task.id == work)
        .unwrap()
        .title = "intermediate admitted folder baseline".into();
    store
        .apply_task_sync_folder(
            crate::storage::TaskSyncFolderApply {
                project: &tree.project.project_id,
                actor: &actor("importer"),
                tree: &intermediate,
                export_basis: &exported.export_basis,
                profile: &profile,
                complete: true,
                now: at(5),
            },
            "advance-through-folder",
        )
        .unwrap();
    let baseline_head =
        versions::imported_head_on(&store.connection, &tree.project.project_id, work)
            .unwrap()
            .unwrap();
    assert_ne!(
        baseline_head, origin_id,
        "the changed task now has a retained folder head"
    );
    let item = store.get_work_item(work).unwrap();
    let mut resolved_tree = tree.clone();
    resolved_tree
        .tasks
        .iter_mut()
        .find(|task| task.id == work)
        .unwrap()
        .title = if unchanged {
        "intermediate admitted folder baseline"
    } else {
        "new imported baseline"
    }
    .into();
    let (binding, prepared, intent) = prepared_tree(&mut store, home.path(), &resolved_tree);
    let before = store
        .task_sync_reconciliation_basis(&tree.project.project_id, &[work])
        .unwrap();
    let local_before = before
        .iter()
        .find(|basis| basis.work_id == work)
        .unwrap()
        .clone();
    let receipt_id = ObjectId::mint();
    let version_id = ObjectId::mint();
    let task = resolved_tree
        .tasks
        .iter()
        .find(|task| task.id == work)
        .unwrap()
        .clone();
    let base = ReconciliationBase::ExplicitUnbasedResolution {
        local_basis: intent.local_planning_basis.clone(),
        incoming_basis: intent.candidate.planning_fingerprint.clone(),
    };
    let version = SharedImportVersion {
        schema_version: 1,
        project_id: tree.project.project_id.clone(),
        task,
        predecessor: Some(baseline_head.clone()),
        original_import_origin: Some(origin_id.clone()),
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
    let baseline = super::super::baseline::head(&store.connection, &tree.project.project_id)
        .unwrap()
        .unwrap();
    let tx = store.begin_work_mutation().unwrap();
    // Model A explicitly names every versioned task, including a no-op document.
    let planning = super::super::baseline::append(
        &tx,
        &tree.project.project_id,
        Some(baseline.record),
        BTreeMap::from([(work, Some(version.task.clone()))]),
        baseline.export_basis.as_deref(),
        &profile,
        at(8),
    )
    .unwrap();
    tx.commit().unwrap();
    let next = super::super::baseline::head(&store.connection, &tree.project.project_id)
        .unwrap()
        .unwrap();
    assert_eq!(next.inventory_root == baseline.inventory_root, unchanged);

    let receipt = ModelAImportReceipt {
        operation: intent.operation,
        operation_key: intent.key.clone(),
        candidate_fingerprint: intent.candidate.documents_fingerprint.clone(),
        base,
        resolved: intent.commit.clone(),
        planning,
        before,
        after_basis: super::super::baseline::head(&store.connection, &tree.project.project_id)
            .unwrap()
            .unwrap()
            .inventory_root
            .parse()
            .unwrap(),
        tasks: vec![ReconciledTask {
            task: work,
            previous_head: Some(baseline_head),
            shared_version: version_id.clone(),
            local_record_ids: Vec::new(),
        }],
        holds: Vec::new(),
        terminal_evidence: Vec::new(),
    };
    let event = Event {
        schema_version: 1,
        binding,
        sequence: 3,
        predecessor: Some(prepared.clone()),
        actor: intent.actor,
        recorded_at: at(8),
        transition: Transition::ReconciledImportApplied {
            prepared,
            receipt: Box::new(receipt),
        },
    };
    let tx = store.begin_work_mutation().unwrap();
    let object = CanonicalObject::identified(&version_id, &version).unwrap();
    SqliteStore::insert_object(&tx, SHARED_VERSION_KIND, &object).unwrap();
    crate::storage::work::feeds::append_to_project_feed(
        &tx,
        &tree.project.project_id,
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
        "UPDATE work_task_sync_import_heads SET head_id=?1 WHERE work_id=?2",
        params![version_id.as_str(), work.0.to_string()],
    )
    .unwrap();
    verify_fixture_write(&tx, &event);
    tx.commit().unwrap();
    assert_eq!(
        store
            .task_sync_imported_baseline(&tree.project.project_id, work)
            .unwrap(),
        Some(version.task)
    );
    assert_eq!(
        store.get_work_item(work).unwrap(),
        item,
        "a baseline read never discards local planning"
    );
    assert_eq!(
        store
            .connection
            .query_row(
                "SELECT canonical_json FROM objects WHERE object_id=?1",
                [origin_id.as_str()],
                |row| row.get::<_, Vec<u8>>(0)
            )
            .unwrap(),
        original_bytes
    );
    for task in tree.tasks.iter().filter(|task| task.id != work) {
        let sibling = store.get_work_item(task.id).unwrap();
        assert_eq!(
            versions::imported_head_on(&store.connection, &tree.project.project_id, task.id)
                .unwrap(),
            Some(super::super::origin_reference(&sibling.created_by).unwrap()),
            "a complete baseline must not supersede an unchanged sibling's imported head"
        );
    }
    let report = store.verify_all().unwrap();
    assert!(report.is_healthy(), "{report:?}");
    let other = store
        .get_work_item(tree.tasks.iter().find(|task| task.id != work).unwrap().id)
        .unwrap();
    corrections::assert_receipt_links(&store, &receipt_id, &event, other.work_id);
    corrections::assert_shared_record_links(
        &store,
        &version_id,
        &receipt_id,
        &event,
        other.work_id,
    );
    corrections::assert_receipt_delta(&store, &receipt_id, &event);
    let wrong_origin = super::super::origin_reference(&other.created_by).unwrap();
    let intact = state(&store);
    store
        .connection
        .execute_batch("SAVEPOINT wrong_head")
        .unwrap();
    store
        .connection
        .execute(
            "UPDATE work_task_sync_import_heads SET head_id=?1 WHERE work_id=?2",
            params![wrong_origin.as_str(), work.0.to_string()],
        )
        .unwrap();
    let malformed = state(&store);
    assert!(
        store
            .task_sync_imported_baseline(&tree.project.project_id, work)
            .unwrap_err()
            .to_string()
            .contains("scope differs")
    );
    assert_eq!(
        state(&store),
        malformed,
        "a read refuses without repairing durable heads"
    );
    assert!(!store.verify_all().unwrap().is_healthy());
    store
        .connection
        .execute_batch("ROLLBACK TO wrong_head; RELEASE wrong_head")
        .unwrap();
    assert_eq!(state(&store), intact);
    drop(store);
    assert_delivery(&home.path().join("work.db"), SHARED_VERSION_KIND);
}

#[test]
fn reconciliation_retired_generation_has_no_seal_or_credit_and_reopens_unclaimed() {
    use crate::storage::work::test_support::{complete, evidence};
    use crate::storage::work::{WorkEventDraft, feeds, planning, query, root_state};
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let work = store
        .create_work(
            &root_request("task-sync", "native", 1),
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let first_run = work.active_run_id.unwrap();
    let first_claim = claim(&mut store, &work, "runner", "first-claim", 2, 600);
    let retained_evidence = evidence(
        &mut store,
        &work,
        &first_claim,
        "runner",
        "first-evidence",
        3,
    );
    checkpoint(
        &mut store,
        &work,
        &first_claim,
        "runner",
        "first-checkpoint",
        3,
        std::slice::from_ref(&retained_evidence),
    );
    complete(
        &mut store,
        &work,
        &first_claim,
        "runner",
        &retained_evidence,
        "first-completion",
        4,
    )
    .unwrap();
    let completed = store.get_work_item(work.work_id).unwrap();
    store
        .reopen_work(
            &crate::ReopenWorkRequest {
                work_id: work.work_id,
                expected_work_revision: completed.revision,
                reason: "another execution generation".into(),
                actor: actor("operator"),
                idempotency_key: "initial-reopen".into(),
                reopened_at: at(5),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    let preserved_run = query::load_work_run(&store.connection, first_run).unwrap();
    let preserved_seal = preserved_run.completion_seal.clone().unwrap();
    let retained_bytes = |connection: &Connection, id: &ObjectId| {
        connection
            .query_row(
                "SELECT canonical_json FROM objects WHERE object_id=?1",
                [id.as_str()],
                |row| row.get::<_, Vec<u8>>(0),
            )
            .unwrap()
    };
    let seal_bytes = retained_bytes(&store.connection, &preserved_seal);
    let evidence_bytes = retained_bytes(&store.connection, &retained_evidence);
    let work = store.get_work_item(work.work_id).unwrap();
    let original_run = work.active_run_id.unwrap();
    let (binding, prepared, intent) = prepared(&mut store, home.path());
    let held = hold(
        &store,
        binding.clone(),
        prepared.clone(),
        &intent,
        vec![work.work_id],
    );
    let hold_id = store.place_task_sync_reconciliation_hold(&held).unwrap();
    let tree = store.export_task_tree(&held.project_id).unwrap();
    super::super::folder::record_complete_baseline_for_test(&mut store, &tree).unwrap();
    let planning_id: String = store.connection.query_row("SELECT object_id FROM work_feed_entries WHERE object_kind='task_sync_import_planning' ORDER BY position DESC LIMIT 1",[],|row|row.get(0)).unwrap();
    let receipt_id = ObjectId::mint();
    // A canonical historical post-apply fixture. Production successors wait
    // for both the local claim and run to end; releasing a claim is insufficient.
    let tx = store.begin_work_mutation().unwrap();
    let mut item = query::load_work_item(&tx, work.work_id).unwrap();
    let mut run = query::load_work_run(&tx, original_run).unwrap();
    let mut root = query::load_root_execution(&tx, run.root_execution_id).unwrap();
    run.state = crate::WorkRunState::SupersededByImport;
    run.revision += 1;
    run.updated_at = at(8);
    root.state = crate::RootExecutionState::Cancelled;
    root.revision += 1;
    root.updated_at = at(8);
    item.active_run_id = None;
    item.revision += 1;
    item.updated_at = at(8);
    planning::persist_work_run(&tx, &run, 0).unwrap();
    planning::persist_work_item(&tx, &item).unwrap();
    root_state::persist(&tx, &root).unwrap();
    let retirement = feeds::append_work_event(
        &tx,
        &WorkEventDraft {
            schema_version: 1,
            project_id: item.project_id.clone(),
            root_id: item.root_id,
            work_id: item.work_id,
            run_id: Some(run.run_id),
            revision: item.revision,
            work: item.clone(),
            run: Some(run.clone()),
            root_execution: Some(root),
            claim: None,
            handoff_offer: None,
            blocker: None,
            transition: WorkTransition::SupersededByImport {
                hold: hold_id.clone(),
                prepared: prepared.clone(),
                receipt: receipt_id.clone(),
                reason: "retained explicit reconciliation".into(),
            },
            actor: actor("importer"),
            created_at: at(8),
        },
    )
    .unwrap();
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
        before: held.affected,
        after_basis: super::super::baseline::head(&tx, &tree.project.project_id)
            .unwrap()
            .unwrap()
            .inventory_root
            .parse()
            .unwrap(),
        tasks: Vec::new(),
        holds: vec![hold_id.clone()],
        terminal_evidence: vec![retirement.0],
    };
    let event = Event {
        schema_version: 1,
        binding,
        sequence: 3,
        predecessor: Some(prepared.clone()),
        actor: actor("importer"),
        recorded_at: at(8),
        transition: Transition::ReconciledImportApplied {
            prepared,
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
    super::active::resolve_on(&tx, &hold_id).unwrap();
    verify_fixture_write(&tx, &event);
    tx.commit().unwrap();
    assert!(run.is_finished());
    assert!(run.completion_seal.is_none());
    assert_eq!(
        store
            .connection
            .query_row("SELECT count(*) FROM work_completion_seals", [], |row| row
                .get::<_, i64>(
                0
            ))
            .unwrap(),
        1
    );
    assert_eq!(store.connection.query_row("SELECT count(*) FROM objects WHERE object_kind IN ('verification_evidence','environment_evidence')",[],|row|row.get::<_,i64>(0)).unwrap(),0);
    assert!(store.verify_all().unwrap().is_healthy());
    let retired_view = store.inspect_work(work.work_id, at(8)).unwrap();
    snapshots::roundtrip(&mut store, &work.project_id, "superseded_by_import");
    assert_eq!(retired_view.availability, crate::WorkAvailability::Waiting);
    assert!(
        retired_view
            .reason_codes
            .contains(&crate::WorkReadinessReason::ImportRetired)
    );
    let waiting = store
        .query_work_catalog(
            &work.project_id,
            at(8),
            &crate::WorkCatalogQuery {
                availabilities: vec![crate::WorkAvailability::Waiting],
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        waiting
            .items
            .iter()
            .any(|item| item.work.work_id == work.work_id)
    );
    let ready = store
        .query_work_catalog(
            &work.project_id,
            at(8),
            &crate::WorkCatalogQuery {
                availabilities: vec![crate::WorkAvailability::Ready],
                ..Default::default()
            },
        )
        .unwrap();
    assert!(
        ready
            .items
            .iter()
            .all(|item| item.work.work_id != work.work_id)
    );
    let service = crate::work_service::LocalWorkService::new(
        home.path().join("work.db"),
        work.project_id.clone(),
        "operator".into(),
        SessionId("operator".into()),
        None,
    );
    service.work_focus(&work.short_ref, at(8)).unwrap();
    let reopened = store
        .reopen_work(
            &crate::ReopenWorkRequest {
                work_id: work.work_id,
                expected_work_revision: item.revision,
                reason: "start fresh local execution".into(),
                actor: actor("operator"),
                idempotency_key: "reopen-imported".into(),
                reopened_at: at(9),
            },
            &DevelopmentNoopRedactor,
        )
        .unwrap();
    assert_eq!(reopened.generation, run.generation + 1);
    assert_eq!(reopened.state, crate::WorkRunState::Open);
    assert!(reopened.executor.is_none() && reopened.completion_seal.is_none());
    assert!(
        query::load_work_claim_optional(&store.connection, reopened.run_id)
            .unwrap()
            .is_none()
    );
    assert_eq!(
        query::load_work_run(&store.connection, original_run).unwrap(),
        run
    );
    assert_eq!(
        query::load_work_run(&store.connection, first_run).unwrap(),
        preserved_run
    );
    assert_eq!(
        retained_bytes(&store.connection, &preserved_seal),
        seal_bytes
    );
    assert_eq!(
        retained_bytes(&store.connection, &retained_evidence),
        evidence_bytes
    );
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn reconciliation_old_durable_shape_is_refused_without_writes() {
    let home = crate::test_support::temp_home().unwrap();
    let path = home.path().join("old.db");
    drop(SqliteStore::open(&path).unwrap());
    let connection = Connection::open(&path).unwrap();
    connection.execute_batch("DROP TABLE work_task_sync_reconciliation_holds; DROP TABLE work_task_sync_import_heads; DROP INDEX work_run_active; CREATE UNIQUE INDEX work_run_active ON work_runs(work_id) WHERE state != 'completed' AND state != 'cancelled';").unwrap();
    let before = crate::storage::test_database_shape_snapshot(&connection).unwrap();
    assert!(matches!(
        SqliteStore::open(&path),
        Err(StoreError::DifferentBuildSchema)
    ));
    assert_eq!(
        crate::storage::test_database_shape_snapshot(&connection).unwrap(),
        before
    );
}
