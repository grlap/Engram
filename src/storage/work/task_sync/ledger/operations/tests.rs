use super::*;
use crate::storage::work::{
    task_sync::tests::{candidate, ledger::binding, state},
    test_support::{actor, at},
};
use crate::task_sync::{
    ledger::{BINDING_KIND, Intent, RetainedCommit},
    notes::NoteOmissions,
    progress::{PreparedCandidate, PreparedProvenance},
};

// These are storage transaction fixtures. Real bounded Git/tree inspection has
// separate tests; the private capability here substitutes only that observation.
pub(super) fn input(
    store: &SqliteStore,
    binding_id: &ObjectId,
    binding: &Binding,
    tree: crate::task_sync::TaskTree,
) -> InspectedExchange {
    std::fs::create_dir_all(&binding.repository.git_directory).unwrap();
    let operation = uuid::Uuid::new_v4();
    let producer = CanonicalObject::freeze(&"fixture producer")
        .unwrap()
        .key()
        .clone();
    let prepared = PreparedCandidate {
        documents: tree.documents().unwrap(),
        tree: tree.clone(),
        provenance: PreparedProvenance {
            sequence: 1,
            operation: uuid::Uuid::new_v4(),
            predecessor: None,
            store: binding.store.database.clone(),
            destination: binding.repository.main_checkout.join("prepared"),
            project: binding.project.clone(),
            profile: binding.profile.clone(),
            producer: producer.clone(),
            cut: binding.store.cut.clone(),
            omissions: NoteOmissions::default(),
            activity_omissions: NoteOmissions::default(),
        },
    };
    let candidate =
        retention::retain_prepared(&binding.repository.git_directory, operation, &prepared)
            .unwrap();
    let commit = RetainedCommit {
        oid: CanonicalObject::freeze(&prepared.documents)
            .unwrap()
            .key()
            .as_str()
            .into(),
        tree: CanonicalObject::freeze(&tree)
            .unwrap()
            .key()
            .as_str()
            .into(),
        parents: Vec::new(),
        retention_ref: format!("refs/engram/task-sync/exchange/{operation}/commit"),
        raw_bytes_fingerprint: CanonicalObject::freeze(&operation).unwrap().key().clone(),
    };
    let selections = binding
        .repository
        .git_directory
        .join("engram-task-sync-commits");
    std::fs::create_dir_all(&selections).unwrap();
    std::fs::write(
        selections.join(format!("{operation}.json")),
        crate::canonical::canonical_bytes(&commit).unwrap(),
    )
    .unwrap();
    let before = store.export_task_tree(&binding.project).unwrap();
    let before = crate::task_sync::progress::documents_fingerprint(&before.documents().unwrap())
        .parse()
        .unwrap();
    InspectedExchange {
        binding_id: binding_id.clone(),
        binding: binding.clone(),
        tree,
        intent: Intent {
            operation,
            key: operation.to_string(),
            direction: Direction::Incoming,
            actor: actor("importer"),
            producer,
            expected_head: None,
            common_commit: None,
            candidate,
            commit,
            local_planning_basis: before,
        },
    }
}

#[test]
fn typed_import_receipt_is_atomic_dry_run_and_post_commit_retry_is_a_noop() {
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let binding = binding(home.path());
    let binding_id = store.activate_task_sync_binding(&binding).unwrap();
    let input = input(&store, &binding_id, &binding, candidate());
    let prepared = store.prepare_task_sync_exchange(&input, at(6)).unwrap();
    let before = state(&store);
    let files_before = retention::inventory(&binding.repository.git_directory).unwrap();
    let dry = store.apply_task_sync_exchange(&input, true, at(7)).unwrap();
    assert_eq!(dry.tasks.len(), input.tree.tasks.len());
    assert!(
        dry.tasks
            .iter()
            .all(|task| task.disposition == ImportDisposition::Created)
    );
    assert_eq!(state(&store), before);
    assert_eq!(
        retention::inventory(&binding.repository.git_directory).unwrap(),
        files_before
    );
    {
        let transaction = store.begin_work_mutation().unwrap();
        let mut ledger = Ledger::load(&transaction).unwrap();
        let (id, receipt) = apply_on(&transaction, &mut ledger, &input, false, at(8)).unwrap();
        let event: Event = SqliteStore::get_typed_object_on(&transaction, &id, EVENT_KIND)
            .unwrap()
            .unwrap();
        assert!(
            matches!(event.transition, Transition::ImportApplied { prepared: selected, .. } if selected == prepared)
        );
        assert_eq!(receipt.tasks.len(), input.tree.tasks.len());
        // Inject failure after BOTH the graph/provenance and actual typed receipt
        // have been staged. Dropping the transaction rolls all of them back.
    }
    assert_eq!(state(&store), before);
    assert!(store.verify_all().unwrap().is_healthy());
    let receipt = store
        .apply_task_sync_exchange(&input, false, at(9))
        .unwrap();
    let applied = state(&store);
    // The commit succeeded but its response was lost. Retry returns the original
    // receipt and record identities, with no duplicated history or progress.
    assert_eq!(
        store
            .apply_task_sync_exchange(&input, false, at(10))
            .unwrap(),
        receipt
    );
    assert_eq!(
        store.prepare_task_sync_exchange(&input, at(11)).unwrap(),
        prepared
    );
    assert_eq!(state(&store), applied);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn request_content_actor_and_unresolved_operation_refuse_without_writes() {
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let binding = binding(home.path());
    let binding_id = store.activate_task_sync_binding(&binding).unwrap();
    let selected = input(&store, &binding_id, &binding, candidate());
    store.prepare_task_sync_exchange(&selected, at(6)).unwrap();
    let before = state(&store);
    let mut changed = selected.clone();
    changed.intent.actor = actor("different-originating-session");
    assert!(
        store
            .prepare_task_sync_exchange(&changed, at(7))
            .unwrap_err()
            .to_string()
            .contains("different content or attribution")
    );
    assert_eq!(state(&store), before);
    let mut changed = selected.clone();
    changed.intent.key.push_str("another-key");
    assert!(store.prepare_task_sync_exchange(&changed, at(7)).is_err());
    assert_eq!(state(&store), before);
    let next = input(&store, &binding_id, &binding, candidate());
    assert!(
        store
            .prepare_task_sync_exchange(&next, at(7))
            .unwrap_err()
            .to_string()
            .contains("unresolved operation")
    );
    assert_eq!(state(&store), before);
    // A surviving operation not found in the DB is not a disposable orphan.
    // The old feed cut still matches (both are zero), which proves nothing.
    assert!(
        store
            .prepare_task_sync_exchange(&selected, at(8))
            .unwrap_err()
            .to_string()
            .contains("ahead of this ledger")
    );
    assert_eq!(state(&store), before);
}

#[test]
fn exact_repeat_records_a_new_commit_receipt_without_duplicating_import_history() {
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let binding = binding(home.path());
    let tree = candidate();
    store
        .import_task_tree(
            &binding.project,
            &actor("earlier-importer"),
            &tree,
            false,
            at(5),
        )
        .unwrap();
    let binding_id = store.activate_task_sync_binding(&binding).unwrap();
    let selected = input(&store, &binding_id, &binding, tree);
    let original = store.connection.prepare("SELECT object_id, canonical_json FROM objects WHERE object_kind != ?1 AND object_kind != ?2 ORDER BY object_id").unwrap()
        .query_map(rusqlite::params![BINDING_KIND, EVENT_KIND], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)))
        .unwrap().collect::<Result<Vec<_>, _>>().unwrap();
    store.prepare_task_sync_exchange(&selected, at(6)).unwrap();
    let receipt = store
        .apply_task_sync_exchange(&selected, false, at(7))
        .unwrap();
    assert!(
        receipt
            .tasks
            .iter()
            .all(|task| task.disposition == ImportDisposition::ExactRepeat)
    );
    let after = store.connection.prepare("SELECT object_id, canonical_json FROM objects WHERE object_kind != ?1 AND object_kind != ?2 ORDER BY object_id").unwrap()
        .query_map(rusqlite::params![BINDING_KIND, EVENT_KIND], |row| Ok((row.get::<_, String>(0)?, row.get::<_, Vec<u8>>(1)?)))
        .unwrap().collect::<Result<Vec<_>, _>>().unwrap();
    assert_eq!(after, original);
    assert!(store.verify_all().unwrap().is_healthy());
    // Corrupt the current projection as an old/outside mutator could do. The
    // remembered request key cannot bypass fresh graph/provenance validation.
    store.connection.execute("UPDATE work_items SET item_json = CAST(json_set(item_json, '$.title', 'outside local edit') AS BLOB) WHERE work_id = ?1", [selected.tree.tasks[0].id.0.to_string()]).unwrap();
    let changed = state(&store);
    assert!(
        store
            .apply_task_sync_exchange(&selected, false, at(8))
            .is_err()
    );
    assert_eq!(state(&store), changed);
}

fn canonical_rows(store: &SqliteStore) -> Vec<(String, String, Vec<u8>, String)> {
    store.connection.prepare("SELECT object_id, object_kind, canonical_json, created_at FROM objects ORDER BY object_id").unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))).unwrap()
        .collect::<Result<Vec<_>, _>>().unwrap()
}

#[test]
fn same_operation_prepared_snapshot_replays_original_ids_bytes_and_times() {
    let home = crate::test_support::temp_home().unwrap();
    let path = home.path().join("work.db");
    let backup = home.path().join("prepared-copy.db");
    let mut store = SqliteStore::open(&path).unwrap();
    let binding = binding(home.path());
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let selected = input(&store, &id, &binding, candidate());
    store.prepare_task_sync_exchange(&selected, at(6)).unwrap();
    store.backup_to(&backup).unwrap(); // coherent, owned fixture copy, never live project data
    let first = store
        .apply_task_sync_exchange(&selected, false, at(10))
        .unwrap();
    let rows = canonical_rows(&store);
    drop(store);
    assert!(!home.path().join("work.db-wal").exists());
    assert!(!home.path().join("work.db-shm").exists());
    std::fs::copy(&backup, &path).unwrap(); // all consumers of this isolated fixture are closed
    let mut restored = SqliteStore::open(&path).unwrap();
    let replay = restored
        .apply_task_sync_exchange(&selected, false, at(1000))
        .unwrap();
    assert_eq!(replay, first);
    assert_eq!(
        canonical_rows(&restored),
        rows,
        "new call-time now changed identified transaction effects"
    );
    assert!(restored.verify_all().unwrap().is_healthy());
    // This replay does NOT certify continuity. An observable restore occurrence
    // changes the binding and still holds for explicit reconciliation.
    let mut known_restore = selected;
    known_restore.binding.store.restore_occurrence = Some(uuid::Uuid::new_v4());
    let before = state(&restored);
    assert!(
        restored
            .apply_task_sync_exchange(&known_restore, false, at(1001))
            .is_err()
    );
    assert_eq!(state(&restored), before);
}

#[test]
fn plan_without_prepared_commit_is_preserved_for_explicit_reconciliation() {
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let binding = binding(home.path());
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let selected = input(&store, &id, &binding, candidate());
    let before = state(&store);
    let retained = {
        let transaction = store.begin_work_mutation().unwrap();
        let ledger = Ledger::load(&transaction).unwrap();
        let proposed = plan::create(&transaction, &ledger, &selected, at(6)).unwrap();
        retention::publish_import_plan(&binding.repository.git_directory, &proposed).unwrap()
        // Inject crash after complete external plan, before the Prepared commit.
    };
    assert!(
        store
            .prepare_task_sync_exchange(&selected, at(7))
            .unwrap_err()
            .to_string()
            .contains("ahead of this ledger")
    );
    assert_eq!(state(&store), before);
    assert_eq!(
        retention::read_import_plan(&binding.repository.git_directory, selected.intent.operation)
            .unwrap(),
        Some(retained)
    );
}

#[test]
fn missing_partial_changed_plan_and_record_collision_never_mint_replacements() {
    for scenario in ["missing", "partial", "encoder", "record collision"] {
        let home = crate::test_support::temp_home().unwrap();
        let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
        let binding = binding(home.path());
        let id = store.activate_task_sync_binding(&binding).unwrap();
        let selected = input(&store, &id, &binding, candidate());
        store.prepare_task_sync_exchange(&selected, at(6)).unwrap();
        let mut plan = retention::read_import_plan(
            &binding.repository.git_directory,
            selected.intent.operation,
        )
        .unwrap()
        .unwrap();
        let directory = binding
            .repository
            .git_directory
            .join("engram-task-sync-import-plans")
            .join(selected.intent.operation.to_string());
        match scenario {
            "missing" => std::fs::remove_file(directory.join("plan.json")).unwrap(),
            "partial" => std::fs::write(directory.join("plan.json"), b"{").unwrap(),
            "encoder" => {
                plan.encoder = CanonicalObject::freeze(&"another encoder")
                    .unwrap()
                    .key()
                    .clone();
                std::fs::write(
                    directory.join("plan.json"),
                    crate::canonical::canonical_bytes(&plan).unwrap(),
                )
                .unwrap();
            }
            "record collision" => {
                let object = CanonicalObject::identified(
                    &plan.tasks[0].origin_id,
                    &serde_json::json!({"different": "body"}),
                )
                .unwrap();
                SqliteStore::insert_object(&store.connection, ORIGIN_KIND, &object).unwrap();
            }
            _ => unreachable!(),
        }
        let before = state(&store);
        assert!(
            store
                .apply_task_sync_exchange(&selected, false, at(1000))
                .is_err(),
            "{scenario}"
        );
        assert_eq!(state(&store), before, "{scenario} wrote partial effects");
    }
}

#[test]
fn concurrent_plan_publication_selects_one_immutable_winner() {
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let binding = binding(home.path());
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let selected = input(&store, &id, &binding, candidate());
    let (left, right) = {
        let transaction = store.begin_work_mutation().unwrap();
        let ledger = Ledger::load(&transaction).unwrap();
        (
            plan::create(&transaction, &ledger, &selected, at(6)).unwrap(),
            plan::create(&transaction, &ledger, &selected, at(7)).unwrap(),
        )
    };
    let barrier = std::sync::Barrier::new(2);
    let results = std::thread::scope(|scope| {
        let left = scope.spawn(|| {
            barrier.wait();
            retention::publish_import_plan(&binding.repository.git_directory, &left)
        });
        let right = scope.spawn(|| {
            barrier.wait();
            retention::publish_import_plan(&binding.repository.git_directory, &right)
        });
        [left.join().unwrap(), right.join().unwrap()]
    });
    assert!(results.iter().any(Result::is_ok));
    let winner =
        retention::read_import_plan(&binding.repository.git_directory, selected.intent.operation)
            .unwrap()
            .unwrap();
    assert!(winner == left || winner == right);
    for result in results.into_iter().flatten() {
        assert_eq!(result, winner);
    }
    assert_eq!(
        retention::publish_import_plan(&binding.repository.git_directory, &left).unwrap(),
        winner
    );
    assert_eq!(
        retention::publish_import_plan(&binding.repository.git_directory, &right).unwrap(),
        winner
    );
}

#[test]
fn an_exchange_import_refuses_a_native_target_and_records_nothing() {
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let binding = binding(home.path());
    let binding_id = store.activate_task_sync_binding(&binding).unwrap();
    store
        .create_work(
            &crate::storage::work::test_support::root_request(&binding.project.0, "native", 0),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let input = input(&store, &binding_id, &binding, candidate());
    let before = state(&store);
    assert!(
        store
            .prepare_task_sync_exchange(&input, at(6))
            .unwrap_err()
            .to_string()
            .contains("import plan refuses native execution state")
    );
    assert_eq!(state(&store), before);
}

#[test]
fn an_exchange_import_refuses_divergent_existing_planning_and_records_nothing() {
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let binding = binding(home.path());
    let binding_id = store.activate_task_sync_binding(&binding).unwrap();
    let mut other = candidate();
    other.tasks[0].title = "a different title on this host".into();
    store
        .import_task_tree(&binding.project, &actor("importer"), &other, false, at(5))
        .unwrap();
    let input = input(&store, &binding_id, &binding, candidate());
    let before = state(&store);
    assert!(
        store
            .prepare_task_sync_exchange(&input, at(6))
            .unwrap_err()
            .to_string()
            .contains("import plan refuses divergent existing planning")
    );
    assert_eq!(state(&store), before);
}

/// A store holding an applied import receipt survives a full-store migration
/// row for row, and the migrated store's doctor, which checks receipts
/// structurally, finds it healthy.
#[test]
fn full_store_migration_carries_an_applied_import_receipt() {
    let home = crate::test_support::temp_home().unwrap();
    let source = home.path().join("work.db");
    let binding = binding(home.path());
    {
        let mut store = SqliteStore::open(&source).unwrap();
        let binding_id = store.activate_task_sync_binding(&binding).unwrap();
        let input = input(&store, &binding_id, &binding, candidate());
        store.prepare_task_sync_exchange(&input, at(6)).unwrap();
        store
            .apply_task_sync_exchange(&input, false, at(7))
            .unwrap();
        assert!(store.verify_all().unwrap().is_healthy());
    }
    let objects = |path: &std::path::Path| -> Vec<(String, String, Vec<u8>)> {
        let connection = rusqlite::Connection::open(path).unwrap();
        let mut statement = connection
            .prepare(
                "SELECT object_id, object_kind, canonical_json FROM objects ORDER BY object_id",
            )
            .unwrap();
        statement
            .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let before = objects(&source);
    let file = home.path().join("export.jsonl");
    crate::storage::migration::export_json(&source, &file).unwrap();
    let target = home.path().join("migrated.db");
    crate::storage::migration::import_json(&file, &target).unwrap();
    assert_eq!(objects(&target), before);
    assert!(
        SqliteStore::open(&target)
            .unwrap()
            .verify_all()
            .unwrap()
            .is_healthy()
    );
}
