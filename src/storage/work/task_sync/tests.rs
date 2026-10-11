use super::super::test_support::{actor, at, claim, root_request};
use super::*;
use crate::{
    ChildRequirement, WorkBlockerKind, WorkItemKind, WorkLifecycle,
    domain::{
        AcceptanceBinding, AcceptanceEvaluationMode, VerificationKind, VerificationRequirement,
    },
};

mod activity;
mod exclusion;
mod native;
mod notes;
mod origin_links;
mod regressions;

fn candidate() -> TaskTree {
    let make = |title: &str| TaskDocument {
        id: WorkId(uuid::Uuid::new_v4()),
        external_ref: None,
        title: title.into(),
        outcome: format!("Deliver {title}"),
        acceptance: vec!["first authored criterion".into(), "second criterion".into()],
        acceptance_bindings: vec![AcceptanceBinding {
            criterion: 2,
            requirement: VerificationRequirement {
                check_kind: VerificationKind::Test,
                check_fingerprint: Some(ObjectId::from_canonical_bytes(title.as_bytes())),
            },
        }],
        evaluation_mode: Some(AcceptanceEvaluationMode::IndependentSession),
        kind: WorkItemKind::Feature,
        priority: 1,
        labels: vec!["alpha".into(), "beta".into()],
        assigned_to: Some("future-worker".into()),
        deferred_until: Some(at(100)),
        lifecycle: WorkLifecycle::Open,
        parent_id: None,
        child_requirement: ChildRequirement::Required,
        prerequisites: Vec::new(),
        blockers: Vec::new(),
        superseded_by: None,
        disposal_reason: None,
        completion: None,
        notes: Vec::new(),
        activity: std::collections::BTreeMap::default(),
    };
    let mut root = make("root");
    root.external_ref = Some("external-item".into());
    let mut child = make("optional child");
    child.parent_id = Some(root.id);
    child.child_requirement = ChildRequirement::Optional;
    child.blockers.push(SharedBlocker {
        blocker_id: uuid::Uuid::new_v4().to_string(),
        kind: WorkBlockerKind::ExternalInput,
        detail: "input still needed".into(),
        created_by: SharedActor::from_actor(&actor("shared-author")),
        created_at: at(1),
    });
    let mut completed = make("reported complete");
    completed.lifecycle = WorkLifecycle::Completed;
    completed.completion = Some(SharedCompletion {
        summary: "reported result".into(),
        completed_at: at(2),
        actor: SharedActor::from_actor(&actor("reported-author")),
    });
    root.prerequisites.push(completed.id);
    let mut cancelled = make("cancelled");
    cancelled.lifecycle = WorkLifecycle::Cancelled;
    cancelled.disposal_reason = Some("no longer needed".into());
    let mut superseded = make("superseded");
    superseded.lifecycle = WorkLifecycle::Superseded;
    superseded.superseded_by = Some(root.id);
    superseded.disposal_reason = Some("replaced by root".into());
    let mut proposed = make("proposal");
    proposed.lifecycle = WorkLifecycle::Proposed;
    let mut tasks = vec![root, child, completed, cancelled, superseded, proposed];
    tasks.sort_by_key(|task| task.id.0);
    TaskTree {
        project: ProjectDocument {
            format: FORMAT.into(),
            project_id: ProjectId("task-sync".into()),
            notes_included: false,
        },
        tasks,
    }
}

fn import(store: &mut SqliteStore, tree: &TaskTree) -> TaskSyncImportResult {
    store
        .import_task_tree(
            &tree.project.project_id,
            &actor("importer"),
            tree,
            false,
            at(5),
        )
        .unwrap()
}

fn state(store: &SqliteStore) -> crate::storage::TestDatabaseShapeSnapshot {
    crate::storage::test_database_shape_snapshot(&store.connection).unwrap()
}

#[test]
fn task_sync_round_trip_is_deterministic_and_repeat_is_a_true_noop() {
    let tree = candidate();
    let mut source = SqliteStore::open_in_memory().unwrap();
    assert!(import(&mut source, &tree).changed);
    let exported = source.export_task_tree(&tree.project.project_id).unwrap();
    let bytes = exported.documents().unwrap();
    assert_eq!(bytes, tree.documents().unwrap());
    assert_eq!(
        bytes,
        source
            .export_task_tree(&tree.project.project_id)
            .unwrap()
            .documents()
            .unwrap()
    );
    assert!(
        bytes
            .values()
            .all(|bytes| bytes.last() == Some(&b'\n') && !bytes.contains(&b'\r'))
    );
    let mut target = SqliteStore::open_in_memory().unwrap();
    let decoded = TaskTree::from_documents(&bytes).unwrap();
    assert!(import(&mut target, &decoded).changed);
    assert_eq!(
        target
            .export_task_tree(&tree.project.project_id)
            .unwrap()
            .documents()
            .unwrap(),
        bytes
    );
    let before = state(&target);
    assert!(
        !target
            .import_task_tree(
                &tree.project.project_id,
                &actor("different-importer"),
                &decoded,
                false,
                at(50)
            )
            .unwrap()
            .changed
    );
    assert_eq!(state(&target), before);
    assert!(target.verify_all().unwrap().is_healthy());
    for task in tree.tasks {
        let item = query::load_work_item(&target.connection, task.id).unwrap();
        assert_eq!(item.acceptance_bindings, task.acceptance_bindings);
        assert!(item.restored && item.active_run_id.is_none());
    }
    for table in ["work_runs", "work_claims", "work_completion_seals"] {
        let count: i64 = target
            .connection
            .query_row(&format!("SELECT count(*) FROM {table}"), [], |row| {
                row.get(0)
            })
            .unwrap();
        assert_eq!(count, 0, "imported planning creates no {table}");
    }
    let proof_count: i64 = target
        .connection
        .query_row(
            "SELECT count(*) FROM objects WHERE object_kind IN
        ('verification_evidence', 'acceptance_evaluation', 'completion_seal')",
            [],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(proof_count, 0);
}

#[test]
fn task_sync_dry_run_divergence_and_removal_preserve_every_row() {
    let tree = candidate();
    let mut target = SqliteStore::open_in_memory().unwrap();
    let before = state(&target);
    assert!(
        target
            .import_task_tree(
                &tree.project.project_id,
                &actor("importer"),
                &tree,
                true,
                at(4)
            )
            .unwrap()
            .changed
    );
    assert_eq!(state(&target), before);
    import(&mut target, &tree);
    let before = state(&target);
    let mut divergent = tree.clone();
    divergent.tasks[0].title.push_str(" changed");
    assert!(
        target
            .import_task_tree(
                &tree.project.project_id,
                &actor("importer"),
                &divergent,
                false,
                at(5)
            )
            .is_err()
    );
    let mut removed = tree.clone();
    removed
        .tasks
        .retain(|task| task.lifecycle != WorkLifecycle::Proposed);
    assert!(
        target
            .import_task_tree(
                &tree.project.project_id,
                &actor("importer"),
                &removed,
                false,
                at(5)
            )
            .is_err()
    );
    assert_eq!(state(&target), before);
}

#[test]
fn task_sync_partial_insert_failure_rolls_back_objects_items_and_relations() {
    let tree = candidate();
    let mut target = SqliteStore::open_in_memory().unwrap();
    target.connection.execute_batch("CREATE TEMP TRIGGER fail_second_task BEFORE INSERT ON work_items
        WHEN (SELECT count(*) FROM work_items) = 1 BEGIN SELECT RAISE(ABORT, 'injected second task failure'); END").unwrap();
    let before = state(&target);
    for dry_run in [true, false] {
        let error = target
            .import_task_tree(
                &tree.project.project_id,
                &actor("importer"),
                &tree,
                dry_run,
                at(5),
            )
            .unwrap_err();
        assert!(error.to_string().contains("injected second task failure"));
        assert_eq!(state(&target), before);
    }
}

#[test]
fn task_sync_native_state_and_cross_project_id_collisions_are_refused() {
    let mut target = SqliteStore::open_in_memory().unwrap();
    let tree = candidate();
    let item = target
        .create_work(
            &root_request("task-sync", "native", 0),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let before = state(&target);
    assert!(
        target
            .import_task_tree(
                &tree.project.project_id,
                &actor("importer"),
                &tree,
                false,
                at(5)
            )
            .is_err()
    );
    assert_eq!(state(&target), before);
    let mut foreign = tree;
    foreign.project.project_id = ProjectId("different-project".into());
    // Use an isolated leaf so the candidate still passes graph validation and
    // reaches the actual store-wide identity collision check.
    foreign
        .tasks
        .retain(|task| task.lifecycle == WorkLifecycle::Proposed);
    foreign.tasks[0].id = item.work_id;
    foreign.validate().unwrap();
    let before = state(&target);
    let error = target
        .import_task_tree(
            &foreign.project.project_id,
            &actor("importer"),
            &foreign,
            false,
            at(5),
        )
        .unwrap_err();
    assert!(error.to_string().contains("already belongs"));
    assert_eq!(state(&target), before);
}

#[test]
fn task_sync_invalid_graph_and_nested_unknown_policy_fields_are_refused() {
    let tree = candidate();
    let mut dangling = tree.clone();
    dangling.tasks[0].prerequisites = vec![WorkId(uuid::Uuid::new_v4())];
    assert!(dangling.documents().is_err());
    let mut cycle = tree.clone();
    cycle.tasks[0].prerequisites = vec![cycle.tasks[1].id];
    cycle.tasks[1].prerequisites = vec![cycle.tasks[0].id];
    assert!(cycle.documents().is_err());
    let mut documents = tree.documents().unwrap();
    let path = format!("tasks/{}.json", tree.tasks[0].id.0);
    let original = documents[&path].clone();
    let mut value: serde_json::Value = serde_json::from_slice(&original).unwrap();
    value["acceptance_bindings"][0]["requirement"]["credit"] = true.into();
    documents.insert(path.clone(), serde_json::to_vec(&value).unwrap());
    assert!(TaskTree::from_documents(&documents).is_err());
    documents.insert(path, original);
    documents.insert(
        "project.json".into(),
        b"{\"format\":\"x\",\"format\":\"y\",\"project_id\":\"task-sync\"}".to_vec(),
    );
    assert!(TaskTree::from_documents(&documents).is_err());
}

#[test]
fn task_sync_projection_repair_and_full_migration_preserve_identity_and_bytes() {
    let directory = crate::test_support::temp_home().unwrap();
    let database = directory.path().join("source.db");
    let tree = candidate();
    let mut store = SqliteStore::open(&database).unwrap();
    import(&mut store, &tree);
    let before_items = tree
        .tasks
        .iter()
        .map(|task| query::load_work_item(&store.connection, task.id).unwrap())
        .collect::<Vec<_>>();
    let before_objects = objects(&store);
    let before_item_bytes = item_bytes(&store);
    for item in &before_items {
        let reference = origin_reference(&item.created_by).unwrap();
        assert_eq!(
            item.created_by
                .provenance_chain
                .iter()
                .filter(|link| link.source == ORIGIN_KIND)
                .count(),
            1
        );
        assert!(
            before_objects
                .iter()
                .any(|(id, kind, _)| id == reference.as_str() && kind == ORIGIN_KIND)
        );
    }
    let bytes = store
        .export_task_tree(&tree.project.project_id)
        .unwrap()
        .documents()
        .unwrap();
    drop(store);
    let before_show = shown_tasks(&database, &tree);
    assert!(
        SqliteStore::repair_rebuildable_projections(&database)
            .unwrap()
            .is_healthy()
    );
    let store = SqliteStore::open(&database).unwrap();
    assert_eq!(item_bytes(&store), before_item_bytes);
    assert_eq!(objects(&store), before_objects);
    assert_eq!(
        store
            .export_task_tree(&tree.project.project_id)
            .unwrap()
            .documents()
            .unwrap(),
        bytes
    );
    assert_eq!(
        tree.tasks
            .iter()
            .map(|task| query::load_work_item(&store.connection, task.id).unwrap())
            .collect::<Vec<_>>(),
        before_items
    );
    drop(store);
    assert_eq!(shown_tasks(&database, &tree), before_show);
    let export_file = directory.path().join("migration.json");
    crate::storage::migration::export_json(&database, &export_file).unwrap();
    let target = directory.path().join("imported.db");
    crate::storage::migration::import_json(&export_file, &target).unwrap();
    let migrated = SqliteStore::open(&target).unwrap();
    assert_eq!(item_bytes(&migrated), before_item_bytes);
    assert!(migrated.verify_all().unwrap().is_healthy());
    assert_eq!(objects(&migrated), before_objects);
    assert_eq!(
        migrated
            .export_task_tree(&tree.project.project_id)
            .unwrap()
            .documents()
            .unwrap(),
        bytes
    );
    drop(migrated);
    assert_eq!(shown_tasks(&target, &tree), before_show);
}

fn shown_tasks(database: &std::path::Path, tree: &TaskTree) -> Vec<(String, serde_json::Value)> {
    let verbs = crate::verbs::AgentVerbs::new(
        database.to_path_buf(),
        tree.project.project_id.clone(),
        "reader".into(),
        crate::SessionId("task-sync-show-reader".into()),
        None,
    );
    tree.tasks
        .iter()
        .map(|task| {
            let receipt = verbs.show(&task.id.0.to_string(), at(6)).unwrap();
            (receipt.text(), receipt.value)
        })
        .collect()
}

fn objects(store: &SqliteStore) -> Vec<(String, String, Vec<u8>)> {
    store
        .connection
        .prepare("SELECT object_id, object_kind, canonical_json FROM objects ORDER BY object_id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?, row.get(2)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

fn item_bytes(store: &SqliteStore) -> Vec<(String, Vec<u8>)> {
    store
        .connection
        .prepare("SELECT work_id, item_json FROM work_items ORDER BY work_id")
        .unwrap()
        .query_map([], |row| Ok((row.get(0)?, row.get(1)?)))
        .unwrap()
        .collect::<Result<_, _>>()
        .unwrap()
}

#[test]
fn task_sync_integrity_refuses_a_malformed_new_origin() {
    let mut store = SqliteStore::open_in_memory().unwrap();
    let tree = candidate();
    import(&mut store, &tree);
    assert!(store.verify_all().unwrap().is_healthy());
    let object = CanonicalObject::mint(&serde_json::json!({"unknown": true})).unwrap();
    SqliteStore::insert_object(&store.connection, ORIGIN_KIND, &object).unwrap();
    let report = store.verify_all().unwrap();
    assert!(!report.is_healthy());
    assert!(format!("{report:?}").contains("task_sync_import_origin"));
}
