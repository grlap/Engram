use super::*;
use crate::storage::work::{
    task_sync::tests::{candidate, ledger::binding, state},
    test_support::{actor, at, root_request},
};
use crate::task_sync::ledger::BINDING_KIND;

fn private_rows(store: &SqliteStore) -> Vec<(String, String, Vec<u8>, String)> {
    store.connection.prepare(
        "SELECT object_id, object_kind, canonical_json, created_at FROM objects WHERE object_kind IN (?1, ?2) ORDER BY object_id"
    ).unwrap().query_map(rusqlite::params![BINDING_KIND, EVENT_KIND], |row| {
        Ok((row.get(0)?, row.get(1)?, row.get(2)?, row.get(3)?))
    }).unwrap().collect::<Result<Vec<_>, _>>().unwrap()
}

#[test]
fn ordinary_mutators_repair_and_coherent_copy_preserve_private_records_verbatim() {
    let home = crate::test_support::temp_home().unwrap();
    let database = home.path().join("work.db");
    let mut store = SqliteStore::open(&database).unwrap();
    let binding = binding(home.path());
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let tree = candidate();
    let selected = super::tests::input(&store, &id, &binding, tree.clone());
    store.prepare_task_sync_exchange(&selected, at(6)).unwrap();
    store
        .apply_task_sync_exchange(&selected, false, at(7))
        .unwrap();
    let original = private_rows(&store);
    assert_eq!(original.len(), 4);
    assert_eq!(
        store
            .export_task_tree(&binding.project)
            .unwrap()
            .documents()
            .unwrap(),
        tree.documents().unwrap()
    );
    // The ordinary task importer never consumes the private binding/events.
    assert!(
        !store
            .import_task_tree(
                &binding.project,
                &actor("ordinary-importer"),
                &tree,
                false,
                at(8)
            )
            .unwrap()
            .changed
    );
    assert_eq!(private_rows(&store), original);
    // A current ordinary writer has no global exchange fence. Its independent
    // local edit is legal, but invalidates the old exchange's planning basis.
    store
        .create_work(
            &root_request(&binding.project.0, "independent local task", 9),
            &crate::DevelopmentNoopRedactor,
        )
        .unwrap();
    let edited = state(&store);
    assert!(
        store
            .apply_task_sync_exchange(&selected, false, at(10))
            .unwrap_err()
            .to_string()
            .contains("local planning changed")
    );
    assert_eq!(state(&store), edited);
    assert_eq!(private_rows(&store), original);
    assert!(store.verify_all().unwrap().is_healthy());
    let copy = home.path().join("coherent-copy.db");
    store.backup_to(&copy).unwrap();
    drop(store);
    assert!(
        SqliteStore::repair_rebuildable_projections(&database)
            .unwrap()
            .is_healthy()
    );
    for path in [&database, &copy] {
        let store = SqliteStore::open(path).unwrap();
        assert_eq!(private_rows(&store), original);
        assert!(store.verify_all().unwrap().is_healthy());
        assert_eq!(store.task_sync_exchange_events(&id).unwrap().len(), 3);
    }
}

#[test]
fn doctor_audits_private_event_semantics_and_preserves_invalid_bytes() {
    let home = crate::test_support::temp_home().unwrap();
    let mut store = SqliteStore::open(home.path().join("work.db")).unwrap();
    let binding = binding(home.path());
    let id = store.activate_task_sync_binding(&binding).unwrap();
    let mut events = store.task_sync_exchange_events(&id).unwrap();
    let (_, mut fork) = events.pop().unwrap();
    fork.sequence = 2;
    fork.predecessor = Some(ObjectId::mint());
    let object = CanonicalObject::mint(&fork).unwrap();
    SqliteStore::insert_object(&store.connection, EVENT_KIND, &object).unwrap();
    let before = state(&store);
    let report = store.verify_all().unwrap();
    assert!(!report.is_healthy());
    assert!(format!("{report:?}").contains("exchange predecessor chain is broken"));
    assert_eq!(state(&store), before);
}
