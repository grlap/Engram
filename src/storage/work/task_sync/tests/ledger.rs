use super::*;
use crate::task_sync::{
    ledger::{BINDING_KIND, Binding, EVENT_KIND, RepositoryBasis, StoreBasis, TargetBasis},
    progress::{ExportCut, ExportProfile},
};

pub(in crate::storage::work::task_sync) fn binding(directory: &std::path::Path) -> Binding {
    Binding {
        schema_version: 1,
        project: ProjectId("task-sync".into()),
        store: StoreBasis {
            database: directory.join("work.db"),
            origin: None,
            restore_occurrence: None,
            cut: ExportCut {
                position: 0,
                anchor: None,
            },
        },
        repository: RepositoryBasis {
            enrollment: uuid::Uuid::new_v4(),
            main_checkout: directory.join("checkout"),
            git_directory: directory.join("checkout/.git"),
        },
        branch: "refs/heads/main".into(),
        target: TargetBasis {
            remote: "origin".into(),
            reference: "refs/heads/main".into(),
            configuration_fingerprint: CanonicalObject::freeze(&uuid::Uuid::new_v4())
                .unwrap()
                .key()
                .clone(),
        },
        profile: ExportProfile {
            include_notes: false,
            origin_machine: None,
        },
        supersedes: None,
        superseded_tail: None,
        actor: actor("exchange-author"),
        created_at: at(5),
    }
}

#[test]
fn binding_activation_is_atomic_idempotent_and_not_shared_task_text() {
    let directory = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(directory.path().join("work.db")).unwrap();
    let selected = binding(directory.path());
    let before = store
        .export_task_tree(&selected.project)
        .unwrap()
        .documents()
        .unwrap();
    let id = store.activate_task_sync_binding(&selected).unwrap();
    let activated = state(&store);
    assert_eq!(store.activate_task_sync_binding(&selected).unwrap(), id);
    assert_eq!(state(&store), activated);
    assert_eq!(
        store.task_sync_binding(&id).unwrap(),
        Some(selected.clone())
    );
    assert_eq!(
        store
            .export_task_tree(&selected.project)
            .unwrap()
            .documents()
            .unwrap(),
        before
    );
    assert!(store.verify_all().unwrap().is_healthy());
    let feeds: i64 = store
        .connection
        .query_row(
            "SELECT count(*) FROM work_feed_entries WHERE object_kind IN (?1, ?2)",
            params![BINDING_KIND, EVENT_KIND],
            |row| row.get(0),
        )
        .unwrap();
    assert_eq!(feeds, 0);
}

#[test]
fn competing_activation_rolls_back_both_typed_records() {
    let directory = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(directory.path().join("work.db")).unwrap();
    let selected = binding(directory.path());
    store.activate_task_sync_binding(&selected).unwrap();
    let before = state(&store);
    let mut other = selected;
    other.branch = "refs/heads/other".into();
    assert!(
        store
            .activate_task_sync_binding(&other)
            .unwrap_err()
            .to_string()
            .contains("competing active")
    );
    assert_eq!(state(&store), before);
    assert!(store.verify_all().unwrap().is_healthy());
}

#[test]
fn a_second_enrollment_for_the_same_repository_is_a_competing_binding() {
    let directory = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(directory.path().join("work.db")).unwrap();
    let selected = binding(directory.path());
    store.activate_task_sync_binding(&selected).unwrap();
    let before = state(&store);
    let mut other = selected;
    other.repository.enrollment = uuid::Uuid::new_v4();
    assert!(
        store
            .activate_task_sync_binding(&other)
            .unwrap_err()
            .to_string()
            .contains("competing active exchange bindings for one repository")
    );
    assert_eq!(state(&store), before);
}

/// The same repository spelled differently is still the same repository: the
/// competing-binding check compares canonical directories, not raw paths.
#[cfg(windows)]
#[test]
fn a_second_enrollment_for_one_repository_spelled_differently_is_refused() {
    let directory = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(directory.path().join("work.db")).unwrap();
    let selected = binding(directory.path());
    std::fs::create_dir_all(&selected.repository.git_directory).unwrap();
    store.activate_task_sync_binding(&selected).unwrap();
    let before = state(&store);
    let mut other = selected.clone();
    other.repository.enrollment = uuid::Uuid::new_v4();
    other.repository.git_directory = directory.path().join("checkout").join(".GIT");
    assert_ne!(
        other.repository.git_directory,
        selected.repository.git_directory
    );
    assert!(
        store
            .activate_task_sync_binding(&other)
            .unwrap_err()
            .to_string()
            .contains("competing active exchange bindings for one repository")
    );
    assert_eq!(state(&store), before);
}

/// Exchange records are ordinary object rows: a full-store migration carries
/// them with their ids and bytes, and the imported store is healthy. The new
/// file's path differs from the binding's recorded one; that is an observation
/// the next exchange reconciles, never a doctor failure.
#[test]
fn full_store_migration_carries_exchange_records_unchanged() {
    let directory = crate::test_support::temp_home().unwrap();
    let source = directory.path().join("work.db");
    let selected = binding(directory.path());
    let id = {
        let mut store = SqliteStore::open(&source).unwrap();
        store.activate_task_sync_binding(&selected).unwrap()
    };
    let exchange_rows = |path: &std::path::Path| -> Vec<(String, String, Vec<u8>)> {
        let connection = rusqlite::Connection::open(path).unwrap();
        let mut statement = connection
            .prepare(
                "SELECT object_id, object_kind, canonical_json FROM objects
                 WHERE object_kind IN (?1, ?2) ORDER BY object_id",
            )
            .unwrap();
        statement
            .query_map(params![BINDING_KIND, EVENT_KIND], |row| {
                Ok((row.get(0)?, row.get(1)?, row.get(2)?))
            })
            .unwrap()
            .collect::<Result<_, _>>()
            .unwrap()
    };
    let before = exchange_rows(&source);
    assert_eq!(before.len(), 2, "one binding and its activation");

    let file = directory.path().join("export.jsonl");
    crate::storage::migration::export_json(&source, &file).unwrap();
    let target = directory.path().join("migrated.db");
    crate::storage::migration::import_json(&file, &target).unwrap();

    assert_eq!(exchange_rows(&target), before);
    let migrated = SqliteStore::open(&target).unwrap();
    assert_eq!(migrated.task_sync_binding(&id).unwrap(), Some(selected));
    assert!(migrated.verify_all().unwrap().is_healthy());
}

#[test]
fn doctor_refuses_missing_activation_without_rewriting_opaque_bytes() {
    let directory = crate::test_support::temp_home().unwrap();
    let store = SqliteStore::open_in_memory().unwrap();
    let object = CanonicalObject::mint(&binding(directory.path())).unwrap();
    SqliteStore::insert_object(&store.connection, BINDING_KIND, &object).unwrap();
    let before = state(&store);
    let report = store.verify_all().unwrap();
    assert!(!report.is_healthy());
    assert!(format!("{report:?}").contains("exchange activation is missing"));
    assert_eq!(state(&store), before);
}
